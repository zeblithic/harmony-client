//! ZEB-824: member session-bootstrap driver ("gateway dial").
//!
//! Post-ZEB-815, a rebuilt member's address book starts empty, so the dial
//! supervisor has zero candidates and the node deadlocks sessionless
//! (no sessions → empty addrbook → no candidates → no sessions). This driver
//! is the session-independent escape hatch: for each joined community with no
//! live member session ("starved"), resolve the community's rendezvous beacon
//! from pkarr (the record open-join dials — knowledge-free, keyed only by the
//! epoch key), verify the beacon is a Joined member, seed it into the
//! [`ReachabilityResolver`], and kick the reconnect supervisor. Everything
//! downstream (record-gated dial, session, addrbook subscribe + snapshot,
//! state sync) is existing machinery.
//!
//! A feeder, not a dialer. Self-contained task — no inline awaits reach back
//! into start_node (see `crate::community_relay_pull_driver` for the shape).
//! Spec: docs/superpowers/specs/2026-07-27-zeb-824-member-gateway-dial-design.md

use crate::community_relay_pull_driver::JoinedCommunitiesFn;
use crate::community_rendezvous::{rendezvous_config_from_env, IdentifiedBeacon};
use crate::network_health::{GatewayBootstrapOutcome, GatewayBootstrapTelemetry};
use crate::owner_state_types::{DeviceIdentityHash, EpochKey, OwnerAddr, SpaceId};
use crate::reachability_resolver::ReachabilityResolver;
use crate::reconnect_supervisor::{PeerStateWire, ReconnectTrigger};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

/// Predicate-only tick cadence. Cheap (in-memory reads); pkarr IO happens only
/// for starved communities whose ladder is due.
pub const GATEWAY_DIAL_TICK_MS: u64 = 30_000;
/// Per-community resolve ladder while starved: base, doubling, cap (the
/// channel_backfill 30s→600s shape).
pub const GATEWAY_DIAL_RETRY_BASE_MS: u64 = 30_000;
pub const GATEWAY_DIAL_RETRY_CAP_MS: u64 = 600_000;

/// Community facts the driver needs from the sync engines. A trait seam (like
/// `RelayIngestCtx`) so unit tests stub it without a registry.
#[async_trait::async_trait]
pub trait GatewayDialCtx: Send + Sync {
    /// Joined members of `community` EXCLUDING self, from the locally
    /// materialized membership (persisted CRDT — survives rebuilds).
    async fn members_of(&self, community: &SpaceId) -> Vec<OwnerAddr>;
    /// The engine's spawn-time `membership_key()`. MUST match the rendezvous
    /// publisher's key choice (`lib.rs:11298`) — never the live epoch key
    /// (spec §5.3). `None` = engine not registered (transient); skip.
    async fn epoch_key_of(&self, community: &SpaceId) -> Option<EpochKey>;
}

/// The pkarr resolve seam. Prod = [`ProdBeaconResolver`]; tests stub it.
#[async_trait::async_trait]
pub trait BeaconResolver: Send + Sync {
    async fn resolve_beacon(&self, epoch_key: &EpochKey, now_ms: u64) -> Option<IdentifiedBeacon>;
}

/// Production [`BeaconResolver`]: Task 1's identified resolve with the
/// open-join env-knob config.
pub struct ProdBeaconResolver {
    pub pkarr: Arc<harmony_pkarr::PkarrResolver>,
    pub self_endpoint_id: [u8; 32],
}

#[async_trait::async_trait]
impl BeaconResolver for ProdBeaconResolver {
    async fn resolve_beacon(&self, epoch_key: &EpochKey, now_ms: u64) -> Option<IdentifiedBeacon> {
        crate::community_rendezvous::resolve_rendezvous_identified(
            &self.pkarr,
            epoch_key,
            self.self_endpoint_id,
            now_ms,
            &rendezvous_config_from_env(),
        )
        .await
        .payload
    }
}

/// Per-community resolve backoff while starved. Present only for communities
/// currently on the ladder; removed on heal, so a re-starved community starts
/// again at the base delay.
struct LadderState {
    next_attempt_ms: u64,
    delay_ms: u64,
}

type NowFn = Arc<dyn Fn() -> u64 + Send + Sync>;

pub struct CommunityGatewayDialDriver {
    ctx: Arc<dyn GatewayDialCtx>,
    beacons: Arc<dyn BeaconResolver>,
    reachability: Arc<ReachabilityResolver>,
    joined_communities: JoinedCommunitiesFn,
    self_owner: OwnerAddr,
    wake: Arc<Notify>,
    interval: Duration,
    telemetry: Option<Arc<GatewayBootstrapTelemetry>>,
    ladders: Mutex<HashMap<SpaceId, LadderState>>,
    now_fn: NowFn,
}

impl CommunityGatewayDialDriver {
    pub fn new(
        ctx: Arc<dyn GatewayDialCtx>,
        beacons: Arc<dyn BeaconResolver>,
        reachability: Arc<ReachabilityResolver>,
        joined_communities: JoinedCommunitiesFn,
        self_owner: OwnerAddr,
    ) -> Self {
        Self {
            ctx,
            beacons,
            reachability,
            joined_communities,
            self_owner,
            wake: Arc::new(Notify::new()),
            interval: Duration::from_millis(GATEWAY_DIAL_TICK_MS),
            telemetry: None,
            ladders: Mutex::new(HashMap::new()),
            now_fn: Arc::new(now_ms),
        }
    }

    /// Install the gateway-bootstrap telemetry sink. Builder-style so boot
    /// wiring attaches it without growing `new`'s arity (the
    /// [`crate::community_relay_pull_driver::CommunityRelayPullDriver::with_telemetry`]
    /// shape).
    pub fn with_telemetry(mut self, telemetry: Arc<GatewayBootstrapTelemetry>) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    /// Test-only clock injection so the per-community ladder can be driven
    /// through simulated time without sleeping.
    #[cfg(test)]
    pub fn with_now_fn(mut self, f: NowFn) -> Self {
        self.now_fn = f;
        self
    }

    /// Test-only ladder reads. The backoff schedule is the driver's only
    /// hidden state, and asserting on it through resolve-call counts alone
    /// cannot tell "capped at 600s" apart from "capped at 300s" without
    /// stepping simulated time through both.
    #[cfg(test)]
    fn ladder_delay_ms(&self, community: &SpaceId) -> Option<u64> {
        self.ladders
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(community)
            .map(|l| l.delay_ms)
    }

    #[cfg(test)]
    fn ladder_next_attempt_ms(&self, community: &SpaceId) -> Option<u64> {
        self.ladders
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(community)
            .map(|l| l.next_attempt_ms)
    }

    /// Clone the wake handle so external callers (a membership/session change
    /// hook, or a test) can trigger an immediate pass without holding the whole
    /// `Arc<Self>`.
    pub fn wake_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.wake)
    }

    /// One pass over every joined community. Spec §5.
    pub async fn run_one_pass(&self) {
        if let Some(t) = self.telemetry.as_ref() {
            // ZEB-803: unconditionally, BEFORE the joined-set read, so an
            // alive-but-idle loop stays distinguishable from a dead task.
            t.record_pass_start();
        }
        let now_ms = (self.now_fn)();
        // Connected node-ids, once per pass. No supervisor (pre-install race,
        // spec §6) ⇒ empty set: starved verdicts still stand, kicks skipped.
        let connected: HashSet<[u8; 32]> = self
            .reachability
            .supervisor()
            .map(|s| {
                s.states_snapshot()
                    .into_iter()
                    .filter(|(_, st)| matches!(st, PeerStateWire::Connected { .. }))
                    .map(|(id, _)| id)
                    .collect()
            })
            .unwrap_or_default();
        // Owners holding at least ONE connected device, once per pass. Folding
        // the dial view down to owner→single-node first would misjudge a
        // multi-device member: `list_dialable_peers` yields one row per
        // (owner, node_id), so a same-owner row for an idle device can displace
        // the connected device's row and read as starvation.
        let connected_owners: HashSet<OwnerAddr> = self
            .reachability
            .list_dialable_peers()
            .into_iter()
            .filter(|(_, entry)| connected.contains(&entry.payload.iroh_node_id))
            .map(|(owner, _)| owner)
            .collect();

        for community in (self.joined_communities)() {
            let members = self.ctx.members_of(&community).await;
            if members.is_empty() {
                // Solo community: nothing to dial, never starved (spec §4). It
                // still gets a row — absence from `perCommunity` must keep
                // meaning "never evaluated", never "fine".
                self.record(&community, GatewayBootstrapOutcome::SoloCommunity);
                continue;
            }
            if members.iter().any(|m| connected_owners.contains(m)) {
                let healed = self
                    .ladders
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&community)
                    .is_some();
                if healed {
                    tracing::info!(community = ?community, "ZEB-824: community healed — member session live, bootstrap ladder reset");
                }
                self.record(&community, GatewayBootstrapOutcome::Healthy);
                continue;
            }
            // Starved. The epoch key is an in-memory registry read, so it is
            // resolved BEFORE the ladder gate: an unregistered engine never
            // attempted anything, and arming the ladder for it would delay the
            // first REAL attempt by a full rung on every boot race.
            let Some(epoch_key) = self.ctx.epoch_key_of(&community).await else {
                tracing::debug!(community = ?community, "ZEB-824: no engine registered; skipping this pass");
                self.record(&community, GatewayBootstrapOutcome::EngineUnregistered);
                continue;
            };
            // Due per the per-community ladder?
            let due = {
                let mut ladders = self.ladders.lock().unwrap_or_else(|p| p.into_inner());
                match ladders.get_mut(&community) {
                    None => {
                        // First starved sighting: attempt NOW, arm the base delay.
                        tracing::info!(community = ?community, "ZEB-824: community starved — no live member session; resolving rendezvous beacon");
                        ladders.insert(
                            community,
                            LadderState {
                                next_attempt_ms: now_ms.saturating_add(GATEWAY_DIAL_RETRY_BASE_MS),
                                delay_ms: GATEWAY_DIAL_RETRY_BASE_MS,
                            },
                        );
                        true
                    }
                    Some(l) if now_ms >= l.next_attempt_ms => {
                        l.delay_ms = l.delay_ms.saturating_mul(2).min(GATEWAY_DIAL_RETRY_CAP_MS);
                        l.next_attempt_ms = now_ms.saturating_add(l.delay_ms);
                        true
                    }
                    Some(_) => false,
                }
            };
            if !due {
                self.record(&community, GatewayBootstrapOutcome::StarvedWaiting);
                continue;
            }
            let Some(hit) = self.beacons.resolve_beacon(&epoch_key, now_ms).await else {
                self.record(&community, GatewayBootstrapOutcome::NoBeacon);
                continue;
            };
            let Ok(identity) =
                harmony_identity::Identity::from_public_bytes(&hit.beacon_identity_pub)
            else {
                // Defensive only on the production path: `PkarrResolver::resolve`
                // already parsed this same Ed25519 half to verify the record's
                // inner signature, so a record that reaches here unparseable
                // cannot have arrived through `ProdBeaconResolver`. Firing means
                // a wire-format or publisher bug (or a non-prod `BeaconResolver`)
                // — never an attacker — and it would otherwise present as an
                // ordinary `rejectedNonMember` tick with no context at all.
                tracing::warn!(
                    community = ?community,
                    node_id = %hex::encode(&hit.payload.iroh_node_id[..8]),
                    "ZEB-824: beacon identity failed to decode — record passed inner-sig verification but its identity bytes are unparseable"
                );
                self.record(&community, GatewayBootstrapOutcome::RejectedNonMember);
                continue;
            };
            let beacon_owner = OwnerAddr(identity.address_hash);
            // Secondary self-guard (primary is the resolve-layer endpoint-id
            // filter): a same-owner sibling record is the fleet seed path's
            // job, not ours (spec §5.b). Checked before the membership gate so
            // a self-record is never reported as a stranger.
            if beacon_owner == self.self_owner {
                self.record(&community, GatewayBootstrapOutcome::RejectedNonMember);
                tracing::info!(
                    community = ?community,
                    beacon_owner = %hex::encode(&beacon_owner.0[..4]),
                    node_id = %hex::encode(&hit.payload.iroh_node_id[..8]),
                    "ZEB-824: beacon rejected — our own owner address; a sibling device is the fleet seed path's job"
                );
                continue;
            }
            // The security gate: the record proves its writer holds the epoch
            // key AND the claimed identity's signing key, but only membership
            // stops a LEAKED epoch key from steering our dials at an attacker
            // endpoint. When this fires in the fleet the first two questions are
            // "which identity" and "which endpoint" — so both are on the line.
            if !members.contains(&beacon_owner) {
                self.record(&community, GatewayBootstrapOutcome::RejectedNonMember);
                tracing::info!(
                    community = ?community,
                    beacon_owner = %hex::encode(&beacon_owner.0[..4]),
                    node_id = %hex::encode(&hit.payload.iroh_node_id[..8]),
                    "ZEB-824: beacon rejected — identity is not a Joined member of this community"
                );
                continue;
            }
            let node_id = hit.payload.iroh_node_id;
            self.reachability
                .seed_from_pkarr(beacon_owner, DeviceIdentityHash([0u8; 16]), hit.payload)
                .await;
            if let Some(sup) = self.reachability.supervisor() {
                // Explicit kick: idempotent with the seed's auto-kick (the
                // dirty set coalesces; NewPeer either way).
                sup.kick(node_id, ReconnectTrigger::NewPeer);
            }
            self.record(&community, GatewayBootstrapOutcome::BeaconSeeded);
            tracing::info!(community = ?community, "ZEB-824: rendezvous beacon seeded — reconnect supervisor kicked");
        }
    }

    fn record(&self, community: &SpaceId, outcome: GatewayBootstrapOutcome) {
        if let Some(t) = self.telemetry.as_ref() {
            t.record_outcome(&community.0, outcome);
        }
    }

    /// Spawn the driver loop: one immediate startup pass, then a pass on every
    /// `wake` notification or `interval` tick. Returns the `JoinHandle` the
    /// caller may `abort()` on shutdown. Self-contained — no inline awaits reach
    /// back into start_node.
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run_one_pass().await;
            let mut ticker = tokio::time::interval(self.interval);
            // After a slow pass (a starved community's resolve can take
            // seconds), Tokio's default `Burst` behavior would fire every
            // missed tick back-to-back. `Skip` collapses the backlog to a
            // single tick on the next period boundary.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // `interval` fires the first tick immediately; we just ran the
            // startup pass, so consume it.
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = self.wake.notified() => {
                        self.run_one_pass().await;
                    }
                    _ = ticker.tick() => {
                        self.run_one_pass().await;
                    }
                }
            }
        })
    }
}

/// Wall-clock now in epoch-ms — the ladder's clock and the resolve layer's
/// freshness clock.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::Hlc;
    use crate::reachability_record::ReachabilityAnnouncePayload;
    use crate::reconnect_supervisor::SupervisorHandle;
    use std::sync::atomic::{AtomicU64, Ordering};

    // ------------------------------------------------------------------
    // Stubs
    // ------------------------------------------------------------------

    struct StubCtx {
        members: Mutex<HashMap<SpaceId, Vec<OwnerAddr>>>,
        keys: Mutex<HashMap<SpaceId, EpochKey>>,
    }

    impl StubCtx {
        fn new(
            members: HashMap<SpaceId, Vec<OwnerAddr>>,
            keys: HashMap<SpaceId, EpochKey>,
        ) -> Self {
            Self {
                members: Mutex::new(members),
                keys: Mutex::new(keys),
            }
        }
    }

    #[async_trait::async_trait]
    impl GatewayDialCtx for StubCtx {
        async fn members_of(&self, c: &SpaceId) -> Vec<OwnerAddr> {
            self.members
                .lock()
                .unwrap()
                .get(c)
                .cloned()
                .unwrap_or_default()
        }
        async fn epoch_key_of(&self, c: &SpaceId) -> Option<EpochKey> {
            self.keys.lock().unwrap().get(c).cloned()
        }
    }

    struct StubBeacons {
        hit: Option<IdentifiedBeacon>,
        calls: AtomicU64,
    }

    impl StubBeacons {
        fn new(hit: Option<IdentifiedBeacon>) -> Self {
            Self {
                hit,
                calls: AtomicU64::new(0),
            }
        }
        fn calls(&self) -> u64 {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl BeaconResolver for StubBeacons {
        async fn resolve_beacon(&self, _k: &EpochKey, _now: u64) -> Option<IdentifiedBeacon> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.hit.clone()
        }
    }

    // ------------------------------------------------------------------
    // Fixtures
    // ------------------------------------------------------------------

    /// A deterministic identity: the public half plus its 64-byte
    /// `to_public_bytes()` form (the shape the beacon record carries). The
    /// derived `OwnerAddr` is `identity.address_hash`, exactly the derivation
    /// the driver's membership gate performs.
    fn test_identity(seed: u8) -> (harmony_identity::Identity, [u8; 64]) {
        let private = harmony_identity::PrivateIdentity::from_seed(&[seed; 32]);
        let identity = private.identity.clone();
        let public_bytes = identity.to_public_bytes();
        (identity, public_bytes)
    }

    /// `(identity_pub_bytes, owner_addr)` for `seed`.
    fn test_member(seed: u8) -> ([u8; 64], OwnerAddr) {
        let (identity, public_bytes) = test_identity(seed);
        (public_bytes, OwnerAddr(identity.address_hash))
    }

    fn test_payload(node_id: [u8; 32]) -> ReachabilityAnnouncePayload {
        ReachabilityAnnouncePayload {
            iroh_node_id: node_id,
            home_relay_url: "https://derp.example/".into(),
            direct_addresses: vec![],
            announced_at_ms: 1_700_000_000_000,
            identity_signature: [0; 64],
            butler_set: Vec::new(),
            bs_at: 0,
        }
    }

    fn hlc(wall_ms: u64) -> Hlc {
        Hlc {
            wall_ms,
            logical: 0,
            device_id: "test".into(),
        }
    }

    fn beacon(identity_pub: [u8; 64], node_id: [u8; 32]) -> IdentifiedBeacon {
        IdentifiedBeacon {
            payload: test_payload(node_id),
            beacon_identity_pub: identity_pub,
        }
    }

    /// Everything a pass-level test needs: the driver plus the handles its
    /// assertions read.
    struct Harness {
        driver: CommunityGatewayDialDriver,
        beacons: Arc<StubBeacons>,
        resolver: Arc<ReachabilityResolver>,
        ctx: Arc<StubCtx>,
        telemetry: Arc<GatewayBootstrapTelemetry>,
    }

    /// Build a driver over one community with `members`, an epoch key (unless
    /// `with_key` is false), and a stub resolve returning `hit`. `supervisor`
    /// installs a [`SupervisorHandle`] on the reachability resolver.
    fn harness(
        community: SpaceId,
        members: Vec<OwnerAddr>,
        hit: Option<IdentifiedBeacon>,
        with_key: bool,
        supervisor: Option<SupervisorHandle>,
    ) -> Harness {
        let resolver = Arc::new(ReachabilityResolver::new());
        if let Some(h) = supervisor {
            resolver.set_supervisor(h);
        }
        let keys = if with_key {
            HashMap::from([(community, EpochKey::new([0x33; 32]))])
        } else {
            HashMap::new()
        };
        let ctx = Arc::new(StubCtx::new(HashMap::from([(community, members)]), keys));
        let beacons = Arc::new(StubBeacons::new(hit));
        let telemetry = Arc::new(GatewayBootstrapTelemetry::new());
        let driver = CommunityGatewayDialDriver::new(
            Arc::clone(&ctx) as Arc<dyn GatewayDialCtx>,
            Arc::clone(&beacons) as Arc<dyn BeaconResolver>,
            Arc::clone(&resolver),
            Arc::new(move || vec![community]),
            // Self ≠ any test member identity: every fixture owner is a real
            // address hash, never the all-0xFF placeholder.
            OwnerAddr([0xFF; 16]),
        )
        .with_telemetry(Arc::clone(&telemetry));
        Harness {
            driver,
            beacons,
            resolver,
            ctx,
            telemetry,
        }
    }

    /// The single per-community outcome row the telemetry recorded, if any.
    fn outcome_of(t: &GatewayBootstrapTelemetry, community: &SpaceId) -> Option<String> {
        t.summary()
            .per_community
            .into_iter()
            .find(|row| row.community_short == hex::encode(&community.0[..4]))
            .map(|row| row.outcome)
    }

    // ------------------------------------------------------------------
    // 1. Empty resolver, one starved community with a valid member beacon:
    //    pass seeds + kicks.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn starved_community_with_member_beacon_seeds_and_kicks() {
        let community = SpaceId([0x11; 16]);
        let (member_pub, member_owner) = test_member(1);
        let beacon_node_id = [0x22; 32];
        let handle = SupervisorHandle::new();
        let h = harness(
            community,
            vec![member_owner],
            Some(beacon(member_pub, beacon_node_id)),
            true,
            Some(handle.clone()),
        );

        h.driver.run_one_pass().await;

        assert_eq!(h.beacons.calls(), 1, "a starved community must resolve");
        let seeded = h.resolver.resolve_by_node_id(&beacon_node_id);
        assert!(
            seeded.is_some(),
            "beacon record must be seeded into the resolver"
        );
        assert_eq!(
            seeded.unwrap().0,
            member_owner,
            "the seed must be keyed by the identity-derived owner, not the epoch key holder"
        );
        // seed_from_pkarr's update auto-kick may fire first and the explicit kick
        // coalesces — either way exactly NewPeer is pending.
        assert_eq!(
            handle.pending_trigger(beacon_node_id),
            Some(ReconnectTrigger::NewPeer)
        );
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("beaconSeeded")
        );
        assert_eq!(h.telemetry.summary().beacons_seeded, 1);
    }

    // ------------------------------------------------------------------
    // 1b. The EXPLICIT kick, isolated: when the seed changes nothing in the
    //     resolver, the resolver's own auto-kick stays silent and the pass
    //     must still raise the peer.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn explicit_kick_fires_when_the_seed_raises_no_auto_kick() {
        // Review MAJOR-1: test 1 starts from an EMPTY resolver, so its kick
        // assertion is satisfied by `update_with_source`'s first-learn
        // auto-kick alone — deleting the driver's explicit kick leaves it
        // green. This is the case the explicit kick exists for: a node whose
        // supervisor slot was evicted (membership churn) while its resolver
        // row survived. Nothing else would ever re-raise that peer.
        let community = SpaceId([0x1C; 16]);
        let (member_pub, member_owner) = test_member(14);
        let beacon_node_id = [0xCC; 32];
        let handle = SupervisorHandle::new();
        let h = harness(
            community,
            vec![member_owner],
            Some(beacon(member_pub, beacon_node_id)),
            true,
            Some(handle.clone()),
        );

        // Pre-seed the SAME record the beacon will carry, then clear the
        // supervisor state that pre-seed raised.
        h.resolver.update(
            member_owner,
            test_payload(beacon_node_id),
            hlc(1_700_000_000_000),
        );
        handle.evict_peer(beacon_node_id);
        assert!(
            handle.pending_trigger(beacon_node_id).is_none(),
            "control: the dirty set starts clear"
        );

        // Control for the whole test: replaying the driver's seed BY ITSELF
        // raises nothing, because the effective dial addressing is unchanged
        // (`was_present` && `before_view == after_view`). Without this the
        // assertion below could not distinguish the two kick sources.
        h.resolver
            .seed_from_pkarr(
                member_owner,
                DeviceIdentityHash([0u8; 16]),
                test_payload(beacon_node_id),
            )
            .await;
        assert!(
            handle.pending_trigger(beacon_node_id).is_none(),
            "control: an addressing-identical seed must not auto-kick"
        );

        h.driver.run_one_pass().await;

        // Only the driver's explicit kick can have produced this.
        assert_eq!(
            handle.pending_trigger(beacon_node_id),
            Some(ReconnectTrigger::NewPeer),
            "the explicit kick is the sole path that re-raises an evicted peer \
             whose resolver row survived"
        );
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("beaconSeeded")
        );
    }

    // ------------------------------------------------------------------
    // 2. A Connected member ⇒ healthy ⇒ no resolve call.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn connected_member_means_healthy_no_resolve() {
        let community = SpaceId([0x12; 16]);
        let (member_pub, member_owner) = test_member(2);
        let member_node_id = [0x44; 32];
        let handle = SupervisorHandle::new();
        let h = harness(
            community,
            vec![member_owner],
            Some(beacon(member_pub, [0x22; 32])),
            true,
            Some(handle.clone()),
        );
        // The member is already in the address book AND has a live session.
        h.resolver.update(
            member_owner,
            test_payload(member_node_id),
            hlc(1_700_000_000_000),
        );
        handle.mark_connected(member_node_id);

        h.driver.run_one_pass().await;

        assert_eq!(
            h.beacons.calls(),
            0,
            "a live member session must produce zero pkarr IO"
        );
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("healthy")
        );
    }

    // ------------------------------------------------------------------
    // 2b. A member with TWO devices, only one Connected, reads healthy.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn multi_device_member_reads_healthy_from_its_connected_device() {
        // Review MAJOR-2: this is the falsifier for the deliberate deviation
        // from the brief's `owner → node_id` HashMap. `list_dialable_peers`
        // emits one row per (owner, DEVICE) out of a BTreeMap keyed by
        // (owner, node_id), so collecting it into a per-owner map keeps
        // whichever row sorts LAST — for a two-device member that is the idle
        // device roughly half the time, and the community then reads as
        // starved and burns a pkarr resolve it does not need. Both node-id
        // orderings are exercised so the test bites regardless of iteration
        // order (and would keep biting if the map ever stopped being sorted).
        for (connected_node, idle_node) in [([0x11; 32], [0xEE; 32]), ([0xEE; 32], [0x11; 32])] {
            let community = SpaceId([0x1D; 16]);
            let (member_pub, member_owner) = test_member(15);
            let handle = SupervisorHandle::new();
            let h = harness(
                community,
                vec![member_owner],
                Some(beacon(member_pub, [0x22; 32])),
                true,
                Some(handle.clone()),
            );
            // Two genuinely distinct devices for ONE owner: different node ids
            // and different home relays.
            let mut idle_payload = test_payload(idle_node);
            idle_payload.home_relay_url = "https://idle.example/".into();
            h.resolver
                .update(member_owner, idle_payload, hlc(1_700_000_000_000));
            h.resolver.update(
                member_owner,
                test_payload(connected_node),
                hlc(1_700_000_000_000),
            );
            handle.mark_connected(connected_node);

            h.driver.run_one_pass().await;

            assert_eq!(
                h.beacons.calls(),
                0,
                "one connected device is a live session for the member \
                 (connected={connected_node:?} idle={idle_node:?})"
            );
            assert_eq!(
                outcome_of(&h.telemetry, &community).as_deref(),
                Some("healthy")
            );
        }
    }

    // ------------------------------------------------------------------
    // 3. A Connected NON-member does not mask starvation.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn connected_non_member_does_not_mask_starvation() {
        let community = SpaceId([0x13; 16]);
        let (member_pub, member_owner) = test_member(3);
        let (_other_pub, other_owner) = test_member(4);
        let other_node_id = [0x55; 32];
        let handle = SupervisorHandle::new();
        let h = harness(
            community,
            vec![member_owner],
            Some(beacon(member_pub, [0x22; 32])),
            true,
            Some(handle.clone()),
        );
        // A live session to somebody who is NOT a member of this community —
        // zenoh does not route this community's queries through them.
        h.resolver.update(
            other_owner,
            test_payload(other_node_id),
            hlc(1_700_000_000_000),
        );
        handle.mark_connected(other_node_id);

        h.driver.run_one_pass().await;

        assert_eq!(
            h.beacons.calls(),
            1,
            "a non-member session must not read as community health"
        );
        // Review INFO-11: without this the test would also pass for a
        // predicate that ignored connectivity entirely. The verdict row shows
        // the pass ran to completion rather than merely reaching the resolver.
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("beaconSeeded")
        );
    }

    // ------------------------------------------------------------------
    // 4. Solo community (members_of == []) is never starved.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn solo_community_never_resolves() {
        let community = SpaceId([0x14; 16]);
        let (member_pub, _member_owner) = test_member(5);
        let h = harness(
            community,
            Vec::new(),
            Some(beacon(member_pub, [0x22; 32])),
            true,
            Some(SupervisorHandle::new()),
        );

        h.driver.run_one_pass().await;

        assert_eq!(h.beacons.calls(), 0, "nobody to dial ⇒ no pkarr traffic");
        // Task 2 ruling: the not-actionable skip still gets a row, so absence
        // in `perCommunity` keeps meaning "never evaluated".
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("soloCommunity")
        );
    }

    // ------------------------------------------------------------------
    // 5. Membership gate: beacon identity NOT in members_of ⇒ no seed, no
    //    kick, outcome RejectedNonMember.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn non_member_beacon_is_rejected_not_seeded() {
        let community = SpaceId([0x15; 16]);
        let (_member_pub, member_owner) = test_member(6);
        // The beacon record is signed by an identity that is NOT a member: a
        // leaked epoch key steering our dials at an attacker endpoint.
        let (stranger_pub, _stranger_owner) = test_member(7);
        let beacon_node_id = [0x66; 32];
        let handle = SupervisorHandle::new();
        let h = harness(
            community,
            vec![member_owner],
            Some(beacon(stranger_pub, beacon_node_id)),
            true,
            Some(handle.clone()),
        );

        h.driver.run_one_pass().await;

        assert_eq!(h.beacons.calls(), 1);
        assert!(
            h.resolver.resolve_by_node_id(&beacon_node_id).is_none(),
            "a non-member beacon must never enter the dial view"
        );
        assert!(handle.pending_trigger(beacon_node_id).is_none());
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("rejectedNonMember")
        );
        assert_eq!(h.telemetry.summary().rejected_non_member, 1);
    }

    // ------------------------------------------------------------------
    // 6. Ladder: repeated no-beacon passes back off 30s → 60s → … → 600s cap;
    //    a healed community resets to base.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn ladder_backs_off_and_resets_on_heal() {
        let community = SpaceId([0x16; 16]);
        let (_member_pub, member_owner) = test_member(8);
        let member_node_id = [0x77; 32];
        let handle = SupervisorHandle::new();
        // No beacon ever resolves — the community stays starved.
        let h = harness(
            community,
            vec![member_owner],
            None,
            true,
            Some(handle.clone()),
        );
        let t0 = 1_700_000_000_000u64;
        let clock = Arc::new(AtomicU64::new(t0));
        let clock_for_fn = Arc::clone(&clock);
        let driver = h
            .driver
            .with_now_fn(Arc::new(move || clock_for_fn.load(Ordering::SeqCst)));

        // First starved sighting attempts immediately and arms the base delay.
        driver.run_one_pass().await;
        assert_eq!(h.beacons.calls(), 1);
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("noBeacon")
        );

        // Not yet due: no IO, and the pass still leaves a verdict row.
        clock.store(t0 + 1_000, Ordering::SeqCst);
        driver.run_one_pass().await;
        assert_eq!(h.beacons.calls(), 1, "an undue pass must not resolve");
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("starvedWaiting")
        );

        // Due at base: attempt #2, delay doubles to 60s.
        clock.store(t0 + GATEWAY_DIAL_RETRY_BASE_MS, Ordering::SeqCst);
        driver.run_one_pass().await;
        assert_eq!(h.beacons.calls(), 2);
        assert_eq!(driver.ladder_delay_ms(&community), Some(60_000));

        // Walk the ladder to the cap by always waking exactly when due.
        for _ in 0..8 {
            let next = driver.ladder_next_attempt_ms(&community).expect("armed");
            clock.store(next, Ordering::SeqCst);
            driver.run_one_pass().await;
        }
        assert_eq!(
            driver.ladder_delay_ms(&community),
            Some(GATEWAY_DIAL_RETRY_CAP_MS),
            "the ladder must saturate at the 600s cap, not run away"
        );

        // Heal: the member's record lands and its session comes up.
        h.resolver
            .update(member_owner, test_payload(member_node_id), hlc(t0));
        handle.mark_connected(member_node_id);
        let calls_before_heal = h.beacons.calls();
        driver.run_one_pass().await;
        assert_eq!(h.beacons.calls(), calls_before_heal, "healthy ⇒ no IO");
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("healthy")
        );
        assert!(
            driver.ladder_delay_ms(&community).is_none(),
            "healing must drop the ladder entry"
        );

        // Re-starve: the session drops. The very next pass must attempt
        // immediately — a 600s cap carried across the heal would leave a
        // recovered-then-lost node dark for ten minutes.
        handle.evict_peer(member_node_id);
        driver.run_one_pass().await;
        assert_eq!(
            h.beacons.calls(),
            calls_before_heal + 1,
            "a re-starved community must resolve at once (ladder reset to base)"
        );
        assert_eq!(
            driver.ladder_delay_ms(&community),
            Some(GATEWAY_DIAL_RETRY_BASE_MS),
            "the reset must land on the base rung, not the pre-heal cap"
        );
    }

    // ------------------------------------------------------------------
    // 7. No supervisor installed: seed still lands, no panic.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn missing_supervisor_still_seeds() {
        let community = SpaceId([0x17; 16]);
        let (member_pub, member_owner) = test_member(9);
        let beacon_node_id = [0x88; 32];
        // Pre-install race (spec §6): the driver's first pass can beat the
        // event loop's `set_supervisor`.
        let h = harness(
            community,
            vec![member_owner],
            Some(beacon(member_pub, beacon_node_id)),
            true,
            None,
        );

        h.driver.run_one_pass().await;

        assert!(
            h.resolver.resolve_by_node_id(&beacon_node_id).is_some(),
            "the seed must land even with no supervisor to kick"
        );
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("beaconSeeded")
        );
    }

    // ------------------------------------------------------------------
    // 8. Pass counter increments even with zero joined communities.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn pass_counter_advances_on_idle_pass() {
        // ZEB-803 shape: a driver whose task died and a driver with nothing to
        // do are indistinguishable in every outcome counter; they must differ
        // here.
        let resolver = Arc::new(ReachabilityResolver::new());
        let ctx = Arc::new(StubCtx::new(HashMap::new(), HashMap::new()));
        let beacons = Arc::new(StubBeacons::new(None));
        let telemetry = Arc::new(GatewayBootstrapTelemetry::new());
        let driver = CommunityGatewayDialDriver::new(
            ctx as Arc<dyn GatewayDialCtx>,
            Arc::clone(&beacons) as Arc<dyn BeaconResolver>,
            resolver,
            Arc::new(Vec::new),
            OwnerAddr([0xFF; 16]),
        )
        .with_telemetry(Arc::clone(&telemetry));

        driver.run_one_pass().await;

        let s = telemetry.summary();
        assert_eq!(s.passes_run, 1, "the loop must prove liveness with no work");
        assert!(s.last_pass_ms.is_some());
        assert!(s.per_community.is_empty());
        assert_eq!(beacons.calls(), 0);
    }

    // ------------------------------------------------------------------
    // 9. An unregistered engine is not a failed resolve attempt: it records
    //    EngineUnregistered and must NOT burn the ladder.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn engine_unregistered_records_outcome_without_burning_the_ladder() {
        let community = SpaceId([0x19; 16]);
        let (member_pub, member_owner) = test_member(10);
        let beacon_node_id = [0x99; 32];
        let h = harness(
            community,
            vec![member_owner],
            Some(beacon(member_pub, beacon_node_id)),
            // No epoch key: the registry has not registered the engine yet.
            false,
            Some(SupervisorHandle::new()),
        );
        let t0 = 1_700_000_000_000u64;
        let clock = Arc::new(AtomicU64::new(t0));
        let clock_for_fn = Arc::clone(&clock);
        let driver = h
            .driver
            .with_now_fn(Arc::new(move || clock_for_fn.load(Ordering::SeqCst)));

        driver.run_one_pass().await;

        assert_eq!(h.beacons.calls(), 0, "no key ⇒ nothing to resolve under");
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("engineUnregistered")
        );
        assert_eq!(h.telemetry.summary().engine_unregistered, 1);
        assert!(
            driver.ladder_delay_ms(&community).is_none(),
            "a registration gap must not arm the backoff ladder"
        );

        // The engine registers. The very next pass — same instant — must
        // attempt: had the previous pass armed the ladder, this would be
        // 30 seconds of avoidable dark time on every boot race.
        h.ctx
            .keys
            .lock()
            .unwrap()
            .insert(community, EpochKey::new([0x33; 32]));
        driver.run_one_pass().await;
        assert_eq!(
            h.beacons.calls(),
            1,
            "the first pass after registration must resolve immediately"
        );
    }

    // ------------------------------------------------------------------
    // 10. Secondary self-guard: our OWN identity in a beacon record is never
    //     seeded, even when the membership view lists us.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn own_identity_beacon_is_rejected_by_the_self_guard() {
        let community = SpaceId([0x1A; 16]);
        let (self_pub, self_owner) = test_member(11);
        let (_member_pub, member_owner) = test_member(12);
        let beacon_node_id = [0xAA; 32];
        let handle = SupervisorHandle::new();
        let resolver = Arc::new(ReachabilityResolver::new());
        resolver.set_supervisor(handle.clone());
        // `members_of` here INCLUDES us, so the membership clause admits the
        // record and only the self-guard can reject it. (Production's
        // `members_of` excludes self; this stub deliberately does not, so the
        // guard is tested in isolation rather than shadowed.)
        let ctx = Arc::new(StubCtx::new(
            HashMap::from([(community, vec![self_owner, member_owner])]),
            HashMap::from([(community, EpochKey::new([0x33; 32]))]),
        ));
        let beacons = Arc::new(StubBeacons::new(Some(beacon(self_pub, beacon_node_id))));
        let telemetry = Arc::new(GatewayBootstrapTelemetry::new());
        let driver = CommunityGatewayDialDriver::new(
            ctx as Arc<dyn GatewayDialCtx>,
            Arc::clone(&beacons) as Arc<dyn BeaconResolver>,
            Arc::clone(&resolver),
            Arc::new(move || vec![community]),
            self_owner,
        )
        .with_telemetry(Arc::clone(&telemetry));

        driver.run_one_pass().await;

        assert!(
            resolver.resolve_by_node_id(&beacon_node_id).is_none(),
            "a same-owner sibling record is the fleet seed path's job, not ours"
        );
        assert!(handle.pending_trigger(beacon_node_id).is_none());
        assert_eq!(
            outcome_of(&telemetry, &community).as_deref(),
            Some("rejectedNonMember")
        );
    }

    // ------------------------------------------------------------------
    // 11. The spawned loop runs a startup pass and wakes on notify.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn spawn_runs_a_startup_pass_then_wakes_on_notify() {
        let community = SpaceId([0x1B; 16]);
        let (member_pub, member_owner) = test_member(13);
        let h = harness(
            community,
            vec![member_owner],
            Some(beacon(member_pub, [0xBB; 32])),
            true,
            Some(SupervisorHandle::new()),
        );
        let driver = Arc::new(h.driver);
        let wake = driver.wake_handle();
        let handle = Arc::clone(&driver).spawn();

        for _ in 0..2_000 {
            if h.telemetry.summary().passes_run >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(h.telemetry.summary().passes_run >= 1, "startup pass");

        wake.notify_one();
        for _ in 0..2_000 {
            if h.telemetry.summary().passes_run >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            h.telemetry.summary().passes_run >= 2,
            "a wake must trigger another pass"
        );

        handle.abort();
    }
}
