//! ZEB-824: member session-bootstrap driver ("gateway dial").
//!
//! Post-ZEB-815, a rebuilt member's address book starts empty, so the dial
//! supervisor has zero candidates and the node deadlocks sessionless
//! (no sessions → empty addrbook → no candidates → no sessions). This driver
//! is the session-independent escape hatch: for each joined community with no
//! live member session ("starved"), resolve the community's rendezvous beacon
//! from pkarr (the record open-join dials — knowledge-free, keyed only by the
//! epoch key), seed it into the [`ReachabilityResolver`], and kick the reconnect
//! supervisor. Everything downstream (record-gated dial, session, addrbook
//! subscribe + snapshot, state sync) is existing machinery.
//!
//! Trust is **epoch-envelope only** — open-join parity for this record family:
//! the record proves epoch-key possession plus the claimed identity's signing
//! key, and that is all we require. A membership check is deliberately absent
//! because the beacon identity and community member keys are the repo's two
//! non-convergent 16-byte hash notions; comparing them rejected every beacon.
//! See spec §5c and the trust-model comment in `run_one_pass`. ZEB-827 carries
//! the principled member binding.
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
use std::collections::{BTreeMap, HashMap, HashSet};
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
    /// ZEB-918: ordered membership-epoch key candidates for beacon
    /// resolution — the LIVE current key first, then (when one exists) the
    /// immediately-previous epoch's archived key; never more than one epoch
    /// back. MUST stay coherent with the rendezvous publisher's key choice
    /// (the slot-refresh arm in `lib.rs`, which publishes under the live key
    /// and degrades to the spawn-time key when the live read is unavailable —
    /// this ctx degrades identically, so pub/resolver match in both modes).
    /// `None` = engine not registered (transient); skip — load-bearing:
    /// it is the ONLY signal separating `engineUnregistered` from
    /// `soloCommunity`. `Some(v)` is non-empty.
    async fn epoch_key_candidates_of(&self, community: &SpaceId) -> Option<Vec<EpochKey>>;
    /// ZEB-827: union of Joined (non-self) members' effective enrolled device
    /// verify keys — the set a beacon's membership-vouch key must belong to.
    async fn enrolled_device_keys_of(&self, community: &SpaceId) -> HashSet<[u8; 32]>;
}

/// ZEB-910: outcome of one all-slots beacon resolve — every verified hit plus
/// the probe-error / vouch-reject counts the empty-case classification needs.
/// `hits` is deduped by `iroh_node_id` (the prod resolver does this); a
/// nonempty `hits` wins outright — an errored probe on some OTHER slot does
/// not demote found beacons.
#[derive(Debug, Clone, Default)]
pub struct BeaconsOutcome {
    pub hits: Vec<IdentifiedBeacon>,
    pub resolve_errors: usize,
    pub membership_rejects: usize,
}

/// Pure classification of a hit-less resolve (review r1 finding 2, ZEB-827):
/// a beacon that was present but failed the membership vouch is more
/// informative than a probe error, so `RejectedNonMember` wins; a miss with
/// any errored probe carries no information and must not masquerade as
/// proof-shaped absence (`ResolveError` vs `NoBeacon`).
fn classify_empty(resolve_errors: usize, membership_rejects: usize) -> GatewayBootstrapOutcome {
    if membership_rejects > 0 {
        GatewayBootstrapOutcome::RejectedNonMember
    } else if resolve_errors > 0 {
        GatewayBootstrapOutcome::ResolveError
    } else {
        GatewayBootstrapOutcome::NoBeacon
    }
}

/// The pkarr resolve seam. Prod = [`ProdBeaconResolver`]; tests stub it.
#[async_trait::async_trait]
pub trait BeaconResolver: Send + Sync {
    /// ZEB-910: resolve EVERY slot for this epoch key and return every
    /// verified hit — on a community split the first-found beacon is
    /// coin-flip likely to be an already-reachable member, so repair must
    /// see the full slot space to bridge.
    async fn resolve_beacons(
        &self,
        epoch_key: &EpochKey,
        community_id: SpaceId,
        enrolled_keys: Arc<HashSet<[u8; 32]>>,
        now_ms: u64,
    ) -> BeaconsOutcome;
}

/// Production [`BeaconResolver`]: the ZEB-910 all-slots identified resolve
/// with the open-join env-knob config.
pub struct ProdBeaconResolver {
    pub pkarr: Arc<harmony_pkarr::PkarrResolver>,
    pub self_endpoint_id: [u8; 32],
}

#[async_trait::async_trait]
impl BeaconResolver for ProdBeaconResolver {
    async fn resolve_beacons(
        &self,
        epoch_key: &EpochKey,
        community_id: SpaceId,
        enrolled_keys: Arc<HashSet<[u8; 32]>>,
        now_ms: u64,
    ) -> BeaconsOutcome {
        let res = crate::community_rendezvous::resolve_rendezvous_all_slots(
            &self.pkarr,
            epoch_key,
            self.self_endpoint_id,
            community_id,
            enrolled_keys,
            now_ms,
            &rendezvous_config_from_env(),
        )
        .await;
        BeaconsOutcome {
            hits: res.hits,
            resolve_errors: res.resolve_errors,
            membership_rejects: res.membership_rejects,
        }
    }
}

/// ZEB-827: the union of Joined members' enrolled device verify keys — the set
/// each beacon's membership vouch is checked against. Extracted as a free
/// function so the exact filtering is unit-testable without a live registry
/// (the coverage gap that let the sibling-device bug ship).
///
/// The local owner IS included — unlike [`ProdGatewayDialCtx::members_of`],
/// which drops self because a self-inclusive member set makes a solo community
/// read as "has members, none connected" and resolve forever. That rationale is
/// about the *starved* predicate, not this *verification* set. An owner may hold
/// several enrolled devices (`MemberState::enrolled_device_keys` is a set for
/// exactly this "eventual state"); a sibling device — same owner, a different
/// enrolled key — publishes legitimate rendezvous beacons whose vouch must
/// verify. The resolver already drops THIS device's own beacon at the transport
/// layer (`self_endpoint_id` in `resolve_slot`), so excluding the owner here
/// only rejects a sibling's valid vouch as `NotEnrolled` and can strand an
/// empty-address-book node whose sole reachable beacon is its own other device.
/// `enrolled_device_keys` is already post-revocation in materialized state, so a
/// revoked sibling key is absent and still rejected.
pub(crate) fn enrolled_keys_from_members(
    members: &BTreeMap<OwnerAddr, crate::community_membership::MemberState>,
) -> HashSet<[u8; 32]> {
    members
        .values()
        .filter(|m| m.status == crate::community_membership::MemberStatus::Joined)
        .flat_map(|m| m.enrolled_device_keys.iter().copied())
        .collect()
}

/// Production [`GatewayDialCtx`] over the community sync registry.
pub struct ProdGatewayDialCtx {
    pub registry: Arc<crate::community_state_sync::CommunitySyncRegistry>,
    pub self_owner: OwnerAddr,
    /// ZEB-918: live owner-state handle for epoch-key candidate reads.
    /// `None` (test/legacy wiring) degrades every read to the engine's
    /// spawn-time key — matching the publisher's degraded mode.
    pub crdt_state: Option<Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>>,
}

#[async_trait::async_trait]
impl GatewayDialCtx for ProdGatewayDialCtx {
    async fn members_of(&self, community: &SpaceId) -> Vec<OwnerAddr> {
        let Some(engine) = self.registry.engine_arc(community).await else {
            return Vec::new();
        };
        let state_arc = engine.state();
        let st = state_arc.lock().await;
        let mat = st.materialized(engine.admin_addr());
        mat.members
            .iter()
            .filter(|(addr, m)| {
                // Self MUST be excluded: a self-inclusive member set makes a
                // solo community read as "has members, none connected" and
                // resolve pkarr on the ladder forever.
                m.status == crate::community_membership::MemberStatus::Joined
                    && **addr != self.self_owner
            })
            .map(|(addr, _)| *addr)
            .collect()
    }

    async fn epoch_key_candidates_of(&self, community: &SpaceId) -> Option<Vec<EpochKey>> {
        // ZEB-918: live current key first, previous epoch's archived key
        // second — coherent with the publisher, which now publishes under
        // the live key (and degrades to the spawn-time key exactly when
        // this read does, so a mismatched slot keypair can no longer arise
        // from process lifetime alone; pre-ZEB-918 both sides pinned the
        // spawn-time key and rotation broke discovery until restart).
        //
        // Engine presence stays the None-gate: "no engine registered" must
        // keep reaching the ladder as EngineUnregistered, distinct from
        // "no members".
        let engine = self.registry.engine_arc(community).await?;
        let fallback = engine.membership_key();
        Some(
            crate::community_state_sync::epoch_key_candidates(
                *community,
                self.crdt_state.as_ref(),
                &fallback,
            )
            .await,
        )
    }

    async fn enrolled_device_keys_of(&self, community: &SpaceId) -> HashSet<[u8; 32]> {
        // ZEB-827: the union of Joined (non-self) members' enrolled device
        // verify keys. In materialized state `enrolled_device_keys` is already
        // post-revocation (materialize replay removes a revoked key), so a
        // plain membership check against this set needs no subtraction.
        let Some(engine) = self.registry.engine_arc(community).await else {
            return HashSet::new();
        };
        let state_arc = engine.state();
        let st = state_arc.lock().await;
        let mat = st.materialized(engine.admin_addr());
        enrolled_keys_from_members(&mat.members)
    }
}

/// Per-community resolve backoff while starved. INVARIANT: an entry exists
/// ONLY while its community's current episode is starved — removed on heal, on
/// solo, on engine-unregistered, and on un-join (the pass-top prune) — so the
/// next starved episode always starts again at the base delay instead of
/// inheriting a stale rung (up to 600s of avoidable dark time).
struct LadderState {
    next_attempt_ms: u64,
    delay_ms: u64,
}

type NowFn = Arc<dyn Fn() -> u64 + Send + Sync>;

/// ZEB-910: proven-traffic lookup — the fused per-node stamp
/// (`max(peer-liveness rx app-frame stamp, PeerTrafficRegistry
/// last_any_served_ms)`, the same fusion `network_health` snapshots). `None`
/// = no evidence exists, which MUST read as unproven — a peer with no
/// traffic evidence is exactly what the coverage measure exists to catch.
pub type TrafficEvidenceFn = Arc<dyn Fn(&[u8; 32]) -> Option<u64> + Send + Sync>;

/// ZEB-910: traffic-evidence age within which a member counts as PROVEN (the
/// coverage numerator). Shares `network_health`'s "fresh" staleness bar:
/// zenoh keeps per-link keepalives flowing as QUIC stream data, so any live
/// link shows rx app frames continuously — >5 min of rx silence on a
/// supposedly-connected link is genuinely anomalous (blackhole, wedge,
/// split). Deliberately NOT session existence (ZEB-805: count evidence, not
/// sessions — a blackholed-but-unclosed conn reads Connected forever).
pub const GATEWAY_PROVEN_TRAFFIC_MS: u64 = crate::network_health::STALENESS_QUIET_MS;

/// ZEB-910: record-freshness bar for the coverage DENOMINATOR — a member
/// whose freshest dial-view row is older than this is expected-unreachable
/// (they stopped announcing), not split evidence. Shares the pkarr
/// stale-refresh window. During a real split the repair's forced refresh
/// keeps re-pulling the member's still-fresh relay record, so they stay in
/// the denominator for as long as they keep publishing — the relay is the
/// arbiter of "should be reachable".
pub const GATEWAY_COVERAGE_RECORD_FRESH_MS: u64 =
    crate::reachability_resolver::STALE_RECORD_REFRESH_MS;

/// One dial-view row, reduced to what the coverage measure needs.
struct PeerRow {
    node_id: [u8; 32],
    effective_announced_at_ms: u64,
}

/// ZEB-910: per-community coverage verdict (spec §3.1).
#[derive(Debug)]
enum CoverageVerdict {
    /// Every fresh-record member is proven by recent traffic.
    Healthy,
    /// Some members proven, but ≥1 fresh-record member is not — the split
    /// signature (or a member briefly asleep). Repair runs on the ladder.
    Degraded { unproven: Vec<OwnerAddr> },
    /// NO member is proven by recent traffic — the original ZEB-824 starved
    /// shape, now measured by evidence instead of session existence.
    Starved,
}

/// Pure coverage classification over one community's members (spec §3.1).
///
/// - proven (numerator): any dial-view row's node shows traffic within
///   [`GATEWAY_PROVEN_TRAFFIC_MS`]. Row freshness is irrelevant for
///   attribution — a member proven via a stale row still counts.
/// - eligible (denominator): any row fresh under
///   [`GATEWAY_COVERAGE_RECORD_FRESH_MS`]. Members with NO rows are neither
///   proven nor eligible: they can't hold the community Degraded (a
///   long-offline member is expected-unreachable), but repair passes still
///   attempt discovery for them (run_one_pass action 3).
///
/// DELIBERATE (PR #659 review, Qodo): `P == ∅` reads Starved even when the
/// eligible set is ALSO empty (every row stale). That state is
/// indistinguishable from the ZEB-824 cold boot — a node offline past the
/// freshness window whose only path back in IS the beacon repair — so
/// returning Healthy there would strand exactly the case the driver exists
/// for. The cost for a genuinely dead community is the pre-existing ZEB-824
/// posture: one ladder-capped (10 min) repair pass, cooldown-bounded, for
/// process life.
fn coverage_verdict(
    members: &[OwnerAddr],
    rows_by_owner: &HashMap<OwnerAddr, Vec<PeerRow>>,
    traffic: &TrafficEvidenceFn,
    now_ms: u64,
) -> CoverageVerdict {
    let mut any_proven = false;
    let mut unproven: Vec<OwnerAddr> = Vec::new();
    for m in members {
        let Some(rows) = rows_by_owner.get(m) else {
            continue;
        };
        let proven = rows.iter().any(|r| {
            traffic(&r.node_id)
                .is_some_and(|t| now_ms.saturating_sub(t) <= GATEWAY_PROVEN_TRAFFIC_MS)
        });
        if proven {
            any_proven = true;
            continue;
        }
        let eligible = rows.iter().any(|r| {
            now_ms.saturating_sub(r.effective_announced_at_ms) <= GATEWAY_COVERAGE_RECORD_FRESH_MS
        });
        if eligible {
            unproven.push(*m);
        }
    }
    if !any_proven {
        return CoverageVerdict::Starved;
    }
    if !unproven.is_empty() {
        return CoverageVerdict::Degraded { unproven };
    }
    CoverageVerdict::Healthy
}

pub struct CommunityGatewayDialDriver {
    ctx: Arc<dyn GatewayDialCtx>,
    beacons: Arc<dyn BeaconResolver>,
    reachability: Arc<ReachabilityResolver>,
    joined_communities: JoinedCommunitiesFn,
    /// ZEB-910: proven-traffic lookup for the coverage numerator.
    traffic_evidence: TrafficEvidenceFn,
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
        traffic_evidence: TrafficEvidenceFn,
    ) -> Self {
        Self {
            ctx,
            beacons,
            reachability,
            joined_communities,
            traffic_evidence,
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
        // ZEB-910: the dial view, once per pass — owner → rows. Feeds the
        // coverage denominator (row freshness) and traffic attribution (row
        // node-ids). One row per (owner, device), so a multi-device member is
        // judged by their best device (the ZEB-702 lesson: folding to
        // owner→single-node first lets an idle device's row displace the live
        // one). The numerator deliberately does NOT read supervisor
        // `Connected` state: that is registry occupancy, cleared only by
        // `conn.closed()` — a blackholed conn reads Connected forever
        // (ZEB-804/805), which is exactly the lie coverage exists to catch.
        let mut rows_by_owner: HashMap<OwnerAddr, Vec<PeerRow>> = HashMap::new();
        for (owner, entry) in self.reachability.list_dialable_peers() {
            rows_by_owner.entry(owner).or_default().push(PeerRow {
                node_id: entry.payload.iroh_node_id,
                effective_announced_at_ms: entry.effective_announced_at_ms,
            });
        }

        let joined = (self.joined_communities)();
        {
            // Prune un-joined communities' ladder entries before iterating:
            // a community that left the joined set is never examined by the
            // loop again, so nothing else could ever clean its entry up, and
            // a leave/rejoin cycle would otherwise resume a stale rung. This
            // also bounds the map by the joined-set size.
            let mut ladders = self.ladders.lock().unwrap_or_else(|p| p.into_inner());
            ladders.retain(|c, _| joined.contains(c));
        }
        for community in joined {
            // The epoch key comes first because it is the ONLY signal that
            // separates "no engine registered" from "no members": production's
            // `members_of` returns an empty Vec for a missing engine, so a
            // members-first order reports every registration gap as
            // `soloCommunity` and makes `engineUnregistered` unreachable
            // outside tests. It is a cheap in-memory registry lookup, and
            // taking it before the ladder also keeps a registration gap from
            // arming the backoff — an unregistered engine never attempted
            // anything, and arming for it would delay the first REAL attempt
            // by a full rung on every boot race. Registration gaps also CLEAR
            // any armed backoff, so the first real attempt after
            // re-registration is never delayed by a stale rung.
            let Some(epoch_keys) = self.ctx.epoch_key_candidates_of(&community).await else {
                tracing::debug!(community = ?community, "ZEB-824: no engine registered; skipping this pass");
                let cleared = self
                    .ladders
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&community)
                    .is_some();
                if cleared {
                    tracing::debug!(community = ?community, "ZEB-824: engine unregistered — bootstrap ladder cleared");
                }
                self.record(&community, GatewayBootstrapOutcome::EngineUnregistered);
                continue;
            };
            let members = self.ctx.members_of(&community).await;
            if members.is_empty() {
                // Solo community: nothing to dial, never starved (spec §4). It
                // still gets a row — absence from `perCommunity` must keep
                // meaning "never evaluated", never "fine". Going solo ends the
                // starved episode, so the ladder entry goes too (invariant on
                // [`LadderState`]).
                let cleared = self
                    .ladders
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&community)
                    .is_some();
                if cleared {
                    tracing::debug!(community = ?community, "ZEB-824: community went solo — bootstrap ladder cleared");
                }
                self.record(&community, GatewayBootstrapOutcome::SoloCommunity);
                continue;
            }
            // ZEB-910: three-state coverage verdict replaces the old
            // any-member-connected predicate (which read Healthy on both
            // sides of a two-island split — every node has an intra-island
            // session).
            let unproven = match coverage_verdict(
                &members,
                &rows_by_owner,
                &self.traffic_evidence,
                now_ms,
            ) {
                CoverageVerdict::Healthy => {
                    let healed = self
                        .ladders
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&community)
                        .is_some();
                    if healed {
                        tracing::info!(community = ?community, "ZEB-910: community healed — full member coverage, bootstrap ladder reset");
                    }
                    self.record(&community, GatewayBootstrapOutcome::Healthy);
                    continue;
                }
                CoverageVerdict::Degraded { unproven } => Some(unproven),
                CoverageVerdict::Starved => None,
            };
            let degraded = unproven.is_some();
            // Non-Healthy. Due per the per-community ladder? (One ladder for
            // both flavors: a Starved↔Degraded transition mid-episode keeps
            // its backoff — the episode is "coverage incomplete", not the
            // specific flavor.)
            let due = {
                let mut ladders = self.ladders.lock().unwrap_or_else(|p| p.into_inner());
                match ladders.get_mut(&community) {
                    None => {
                        // First non-Healthy sighting: attempt NOW, arm the base delay.
                        if degraded {
                            tracing::info!(community = ?community, "ZEB-910: community coverage degraded — unproven fresh-record members; running split repair");
                        } else {
                            tracing::info!(community = ?community, "ZEB-824: community starved — no proven member; resolving rendezvous beacon");
                        }
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
                self.record(
                    &community,
                    if degraded {
                        GatewayBootstrapOutcome::DegradedWaiting
                    } else {
                        GatewayBootstrapOutcome::StarvedWaiting
                    },
                );
                continue;
            }
            // ZEB-910 repair actions 2-4 (spec §3.3), BEFORE the beacon
            // resolve so the record refreshes overlap the resolve's network
            // time. Targets: the unproven set when Degraded; every member
            // with rows when Starved.
            let refresh_targets: Vec<OwnerAddr> = match &unproven {
                Some(u) => u.clone(),
                None => members
                    .iter()
                    .filter(|m| rows_by_owner.contains_key(*m))
                    .copied()
                    .collect(),
            };
            // (2) Member-record refresh — the member-keyed pkarr path, the
            // robust cross-island discovery channel (per-owner keys, no slot
            // contention) and what keeps a still-publishing split member in
            // the coverage denominator. Sync + fire-and-forget internally;
            // `refresh_if_older_than` keeps every abuse guard (per-owner
            // cooldown, fleet-sibling skip, refresh semaphore) — only the
            // staleness bar drops to the ladder cap.
            for owner in &refresh_targets {
                for row in rows_by_owner.get(owner).into_iter().flatten() {
                    self.reachability.refresh_if_older_than(
                        *owner,
                        row.node_id,
                        now_ms,
                        GATEWAY_DIAL_RETRY_CAP_MS,
                    );
                }
            }
            // (3) Record-less member discovery, bounded inside the resolver
            // (PR #659 review): per-owner cooldown + the SAME refresh
            // semaphore the stale-refresh path uses, so a mostly-record-less
            // community cannot fan out unbounded pkarr fetches. Sync call;
            // the network fetch runs in the resolver's spawned task — the
            // pass never blocks on pkarr. Fires only while the community is
            // non-Healthy (this pass is ladder-paced).
            for owner in members.iter().filter(|m| !rows_by_owner.contains_key(*m)) {
                self.reachability.discover_record_less(*owner);
            }
            // (4) Revive ONLY Dormant slots of the target set. Retrying peers
            // keep their ladders (re-arming live backoff every repair pass
            // would multiply dial pressure for nothing — they are already
            // trying); absent slots need no kick, because a record write
            // auto-kicks NewPeer/RecordChanged through the resolver's
            // supervisor hooks.
            if let Some(sup) = self.reachability.supervisor() {
                let dormant: HashSet<[u8; 32]> = sup
                    .states_snapshot()
                    .into_iter()
                    .filter(|(_, st)| matches!(st, PeerStateWire::Dormant { .. }))
                    .map(|(id, _)| id)
                    .collect();
                for owner in &refresh_targets {
                    for row in rows_by_owner.get(owner).into_iter().flatten() {
                        if dormant.contains(&row.node_id) {
                            sup.kick(row.node_id, ReconnectTrigger::RecordChanged);
                        }
                    }
                }
            }
            // Ladder semantics are identical for both miss flavors: the rung
            // was already consumed before the resolve, and an errored attempt
            // paces exactly like a missed one.
            // ZEB-827: fetch this community's enrolled device-key set so the
            // resolve layer can verify each beacon's membership vouch. A beacon
            // lacking a valid vouch reads as an empty slot (the batch driver
            // widens); if a beacon WAS present but rejected, the resolve returns
            // `RejectedNonMember`.
            let enrolled_keys = Arc::new(self.ctx.enrolled_device_keys_of(&community).await);
            // ZEB-918 + PR #659 review (CodeAnt/Greptile convergent finding):
            // scan EVERY candidate key (live current first, previous epoch
            // second) and MERGE the hits, deduped by node id with the
            // earlier-candidate beacon winning. A rotation-straddling split
            // publishes the un-rotated island's beacons ONLY under the
            // previous key, so stopping at the first candidate with hits
            // could keep seeding the already-reachable island forever and
            // never bridge. Cost: one extra all-slots scan per DUE repair
            // pass while a rotation archive exists — repair passes are
            // ladder-paced (30s→10min) and only run while non-Healthy, so
            // this is bounded, not steady-state. Telemetry: when EVERY
            // candidate is empty, the CURRENT-key attempt's outcome is
            // recorded — it is the canonical health signal, and the
            // previous-key attempt exists only to heal rotation skew, so it
            // must not mask it (a previous-key ResolveError never upgrades a
            // current-key NotFound; a current-key RejectedNonMember condemns
            // one publisher's bad vouch, not the previous-epoch audience).
            let mut outcome_when_empty = GatewayBootstrapOutcome::NoBeacon;
            let mut hits: Vec<IdentifiedBeacon> = Vec::new();
            let mut seen_nodes: HashSet<[u8; 32]> = HashSet::new();
            for (i, candidate) in epoch_keys.iter().enumerate() {
                let attempt = self
                    .beacons
                    .resolve_beacons(candidate, community, Arc::clone(&enrolled_keys), now_ms)
                    .await;
                if i == 0 {
                    outcome_when_empty =
                        classify_empty(attempt.resolve_errors, attempt.membership_rejects);
                }
                for hit in attempt.hits {
                    if seen_nodes.insert(hit.payload.iroh_node_id) {
                        hits.push(hit);
                    }
                }
            }
            if hits.is_empty() {
                self.record(&community, outcome_when_empty);
                continue;
            }
            // CR-2 (TOCTOU): each vouch was verified against the enrolled
            // snapshot taken BEFORE the async resolve. Re-validate every hit's
            // device key against ONE fresh snapshot now — if it was revoked
            // while we resolved, the stale accept must not reach seed/kick.
            // This narrows the check→use window to microseconds; revocation
            // propagation itself remains eventually-consistent, and
            // post-session data is gated by per-event enrollment checks
            // regardless.
            let fresh_enrolled = self.ctx.enrolled_device_keys_of(&community).await;
            // ZEB-910: seed EVERY hit that survives re-validation — under a
            // split, any one of them may be the only bridge to the other
            // island, and seeding an already-reachable member is an idempotent
            // no-op at the resolver.
            let mut seeded = 0usize;
            for hit in hits {
                if !fresh_enrolled.contains(&hit.membership_device_vk) {
                    tracing::debug!(
                        community = ?community,
                        "ZEB-827: beacon device key revoked during resolve — not seeding"
                    );
                    continue;
                }
                let Ok(identity) =
                    harmony_identity::Identity::from_public_bytes(&hit.beacon_identity_pub)
                else {
                    // Unreachable on the prod path: `PkarrResolver::resolve`
                    // already parsed this same Ed25519 half to verify the inner
                    // signature, so a record reaching here unparseable means a
                    // wire/publisher bug (or a non-prod `BeaconResolver`),
                    // never an attacker.
                    tracing::warn!(
                        community = ?community,
                        node_id = %hex::encode(&hit.payload.iroh_node_id[..8]),
                        "ZEB-827: beacon identity failed to decode — record passed inner-sig verification but its identity bytes are unparseable"
                    );
                    continue;
                };
                // ZEB-827: the beacon carried a membership vouch verified in
                // `resolve_slot` against this community's enrolled device keys,
                // so it belongs to a Joined member. Seed as before — under the
                // composite device-address owner with the `[0u8;16]`
                // device-hash placeholder (invite-path parity). The seed-owner
                // "split" that ZEB-704's `freshest_across_owners` view already
                // tolerates is a low-value, ReachabilityResolver-internal
                // follow-up (ZEB-824 spec §9), out of scope here too.
                let beacon_owner = OwnerAddr(identity.address_hash);
                let node_id = hit.payload.iroh_node_id;
                self.reachability
                    .seed_from_pkarr(beacon_owner, DeviceIdentityHash([0u8; 16]), hit.payload)
                    .await;
                if let Some(sup) = self.reachability.supervisor() {
                    // Explicit kick: idempotent with the seed's auto-kick (the
                    // dirty set coalesces; NewPeer either way).
                    sup.kick(node_id, ReconnectTrigger::NewPeer);
                }
                seeded += 1;
                tracing::info!(community = ?community, node_id = %hex::encode(&node_id[..8]), "ZEB-827: vouch-verified rendezvous beacon seeded — reconnect supervisor kicked");
            }
            if seeded == 0 {
                // Every hit fell to CR-2 revalidation (or the unreachable
                // identity-decode arm) — the present-but-not-a-member shape.
                self.record(&community, GatewayBootstrapOutcome::RejectedNonMember);
                continue;
            }
            self.record(
                &community,
                if degraded {
                    GatewayBootstrapOutcome::DegradedSeeded
                } else {
                    GatewayBootstrapOutcome::BeaconSeeded
                },
            );
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
        /// Ordered epoch-key candidates per community (ZEB-918): entry
        /// absent = engine unregistered.
        keys: Mutex<HashMap<SpaceId, Vec<EpochKey>>>,
    }

    impl StubCtx {
        fn new(
            members: HashMap<SpaceId, Vec<OwnerAddr>>,
            keys: HashMap<SpaceId, Vec<EpochKey>>,
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
        async fn epoch_key_candidates_of(&self, c: &SpaceId) -> Option<Vec<EpochKey>> {
            self.keys.lock().unwrap().get(c).cloned()
        }
        async fn enrolled_device_keys_of(&self, _c: &SpaceId) -> HashSet<[u8; 32]> {
            // Resolution is driven by `StubBeacons` (which ignores the pre-resolve
            // enrolled set), but the driver's post-resolve re-validation (CR-2)
            // checks the beacon's device key against THIS set, so report the
            // fixture key the `beacon()` fixture vouches under.
            std::iter::once(FIXTURE_DEVICE_VK).collect()
        }
    }

    /// ZEB-910: outcome DSL for the stubs — the old single-hit
    /// `BeaconResolution` arms expressed as `BeaconsOutcome` values, so the
    /// pre-existing tests read the same.
    fn found(hit: IdentifiedBeacon) -> BeaconsOutcome {
        BeaconsOutcome {
            hits: vec![hit],
            ..Default::default()
        }
    }
    fn found_all(hits: Vec<IdentifiedBeacon>) -> BeaconsOutcome {
        BeaconsOutcome {
            hits,
            ..Default::default()
        }
    }
    fn not_found() -> BeaconsOutcome {
        BeaconsOutcome::default()
    }
    fn resolve_error() -> BeaconsOutcome {
        BeaconsOutcome {
            resolve_errors: 1,
            ..Default::default()
        }
    }
    fn rejected_non_member() -> BeaconsOutcome {
        BeaconsOutcome {
            membership_rejects: 1,
            ..Default::default()
        }
    }

    struct StubBeacons {
        resolution: Mutex<BeaconsOutcome>,
        /// ZEB-918: per-epoch-key overrides (by key bytes). A key with no
        /// entry falls back to the global `resolution`, so pre-ZEB-918
        /// single-key tests are unaffected.
        by_key: Mutex<HashMap<[u8; 32], BeaconsOutcome>>,
        calls: AtomicU64,
        /// ZEB-918: epoch keys in probe order, for candidate-order pins.
        key_log: Mutex<Vec<[u8; 32]>>,
    }

    impl StubBeacons {
        fn new(hit: Option<IdentifiedBeacon>) -> Self {
            let resolution = match hit {
                Some(h) => found(h),
                None => not_found(),
            };
            Self {
                resolution: Mutex::new(resolution),
                by_key: Mutex::new(HashMap::new()),
                calls: AtomicU64::new(0),
                key_log: Mutex::new(Vec::new()),
            }
        }
        /// Swap the stubbed resolution mid-test (e.g. to `resolve_error()`).
        fn set_resolution(&self, r: BeaconsOutcome) {
            *self.resolution.lock().unwrap() = r;
        }
        /// ZEB-918: stub a per-key resolution (keyed by epoch-key bytes).
        fn set_resolution_for_key(&self, key: [u8; 32], r: BeaconsOutcome) {
            self.by_key.lock().unwrap().insert(key, r);
        }
        fn calls(&self) -> u64 {
            self.calls.load(Ordering::SeqCst)
        }
        /// ZEB-918: epoch keys probed so far, in order.
        fn probed_keys(&self) -> Vec<[u8; 32]> {
            self.key_log.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl BeaconResolver for StubBeacons {
        async fn resolve_beacons(
            &self,
            k: &EpochKey,
            _community: SpaceId,
            _enrolled: Arc<HashSet<[u8; 32]>>,
            _now: u64,
        ) -> BeaconsOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.key_log.lock().unwrap().push(*k.as_bytes());
            if let Some(r) = self.by_key.lock().unwrap().get(k.as_bytes()) {
                return r.clone();
            }
            self.resolution.lock().unwrap().clone()
        }
    }

    // ------------------------------------------------------------------
    // Fixtures
    // ------------------------------------------------------------------

    /// A deterministic identity: the public half plus its 64-byte
    /// `to_public_bytes()` form (the shape the beacon record carries). The
    /// derived `OwnerAddr` is `identity.address_hash`, exactly the derivation
    /// the driver performs to pick the seed key.
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

    /// Device key the `beacon()` fixture vouches under; `StubCtx` reports it as
    /// the community's sole enrolled key so the driver's post-resolve
    /// re-validation (CR-2) passes for seed-path tests.
    const FIXTURE_DEVICE_VK: [u8; 32] = [0x77; 32];

    fn beacon(identity_pub: [u8; 64], node_id: [u8; 32]) -> IdentifiedBeacon {
        beacon_with_device_vk(identity_pub, node_id, FIXTURE_DEVICE_VK)
    }

    fn beacon_with_device_vk(
        identity_pub: [u8; 64],
        node_id: [u8; 32],
        device_vk: [u8; 32],
    ) -> IdentifiedBeacon {
        IdentifiedBeacon {
            payload: test_payload(node_id),
            beacon_identity_pub: identity_pub,
            membership_device_vk: device_vk,
        }
    }

    // ------------------------------------------------------------------
    // ZEB-827 (Greptile P1): the vouch-verification key set must retain the
    // LOCAL OWNER's enrolled device keys. An owner can hold several enrolled
    // devices (`MemberState::enrolled_device_keys` is a set for exactly this);
    // a sibling device — same owner, a different key — publishes valid
    // rendezvous beacons, and its vouch must verify. Excluding the owner here
    // classifies a sibling's valid vouch as `NotEnrolled` and can strand an
    // empty-address-book node whose only reachable beacon is its own other
    // device. Self-loop prevention already lives at the transport layer
    // (`self_endpoint_id` in `resolve_slot`), never in this key set.
    // ------------------------------------------------------------------
    #[test]
    fn enrolled_keys_retain_local_owner_sibling_devices() {
        let self_owner = OwnerAddr([0x01; 16]);
        let peer = OwnerAddr([0x02; 16]);
        let left = OwnerAddr([0x03; 16]);

        let member = |status, keys: &[[u8; 32]]| crate::community_membership::MemberState {
            status,
            joined_at: hlc(1),
            left_at: None,
            enrolled_device_keys: keys.iter().copied().collect(),
            revoked_device_keys: std::collections::BTreeSet::new(),
        };

        // Local owner: THIS device (0xA1) + an enrolled SIBLING device (0xB2).
        // A joined peer contributes 0xC3. A Left member's 0xD4 must never count.
        let mut members = BTreeMap::new();
        members.insert(
            self_owner,
            member(
                crate::community_membership::MemberStatus::Joined,
                &[[0xA1; 32], [0xB2; 32]],
            ),
        );
        members.insert(
            peer,
            member(
                crate::community_membership::MemberStatus::Joined,
                &[[0xC3; 32]],
            ),
        );
        members.insert(
            left,
            member(
                crate::community_membership::MemberStatus::Left,
                &[[0xD4; 32]],
            ),
        );

        let keys = enrolled_keys_from_members(&members);

        assert!(
            keys.contains(&[0xA1; 32]),
            "local owner's own device key must be retained"
        );
        assert!(
            keys.contains(&[0xB2; 32]),
            "local owner's SIBLING device key must be retained (Greptile P1)"
        );
        assert!(
            keys.contains(&[0xC3; 32]),
            "a joined peer's device key must be retained"
        );
        assert!(
            !keys.contains(&[0xD4; 32]),
            "a non-Joined member's device key must be excluded"
        );
        assert_eq!(keys.len(), 3, "exactly the three Joined-member keys");
    }

    /// Everything a pass-level test needs: the driver plus the handles its
    /// assertions read.
    /// ZEB-910: shared node-id → proven-traffic-stamp map backing the test
    /// driver's [`TrafficEvidenceFn`].
    type TrafficMap = Arc<Mutex<HashMap<[u8; 32], u64>>>;

    fn traffic_fn(map: &TrafficMap) -> TrafficEvidenceFn {
        let map = Arc::clone(map);
        Arc::new(move |node: &[u8; 32]| map.lock().unwrap().get(node).copied())
    }

    struct Harness {
        driver: CommunityGatewayDialDriver,
        beacons: Arc<StubBeacons>,
        resolver: Arc<ReachabilityResolver>,
        ctx: Arc<StubCtx>,
        telemetry: Arc<GatewayBootstrapTelemetry>,
        /// ZEB-910: stamp a node here to make its owner PROVEN at `now`.
        traffic: TrafficMap,
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
            HashMap::from([(community, vec![EpochKey::new([0x33; 32])])])
        } else {
            HashMap::new()
        };
        let ctx = Arc::new(StubCtx::new(HashMap::from([(community, members)]), keys));
        let beacons = Arc::new(StubBeacons::new(hit));
        let telemetry = Arc::new(GatewayBootstrapTelemetry::new());
        let traffic: TrafficMap = Arc::new(Mutex::new(HashMap::new()));
        let driver = CommunityGatewayDialDriver::new(
            Arc::clone(&ctx) as Arc<dyn GatewayDialCtx>,
            Arc::clone(&beacons) as Arc<dyn BeaconResolver>,
            Arc::clone(&resolver),
            Arc::new(move || vec![community]),
            traffic_fn(&traffic),
        )
        .with_telemetry(Arc::clone(&telemetry));
        Harness {
            driver,
            beacons,
            resolver,
            ctx,
            telemetry,
            traffic,
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
        // ZEB-918: the healthy single-candidate path costs exactly one probe —
        // the previous-epoch rung must add no cost when there is no rotation.
        assert_eq!(h.beacons.calls(), 1);
    }

    // ------------------------------------------------------------------
    // ZEB-910: multi-hit seeding — under a split, any one hit may be the only
    // bridge to the other island, so a due pass seeds EVERY verified hit.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn due_pass_seeds_every_verified_hit_and_kicks_each() {
        let community = SpaceId([0x12; 16]);
        let (member_a_pub, member_a) = test_member(2);
        let (member_b_pub, member_b) = test_member(3);
        let node_a = [0x2D; 32];
        let node_b = [0x2E; 32];
        let handle = SupervisorHandle::new();
        let h = harness(
            community,
            vec![member_a, member_b],
            None,
            true,
            Some(handle.clone()),
        );
        h.beacons.set_resolution(found_all(vec![
            beacon(member_a_pub, node_a),
            beacon(member_b_pub, node_b),
        ]));

        h.driver.run_one_pass().await;

        assert!(
            h.resolver.resolve_by_node_id(&node_a).is_some(),
            "hit A seeded"
        );
        assert!(
            h.resolver.resolve_by_node_id(&node_b).is_some(),
            "hit B seeded"
        );
        assert_eq!(
            handle.pending_trigger(node_a),
            Some(ReconnectTrigger::NewPeer)
        );
        assert_eq!(
            handle.pending_trigger(node_b),
            Some(ReconnectTrigger::NewPeer)
        );
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("beaconSeeded")
        );
        assert_eq!(
            h.telemetry.summary().beacons_seeded,
            1,
            "one seeded PASS is one count, not one per hit"
        );
    }

    /// ZEB-910 + CR-2: the fresh-enrollment revalidation filters PER HIT — a
    /// revoked co-resolved beacon must neither seed nor take down its valid
    /// sibling, and one surviving hit still makes the pass a seeded pass.
    #[tokio::test]
    async fn cr2_revalidation_filters_per_hit_not_per_pass() {
        let community = SpaceId([0x13; 16]);
        let (member_a_pub, member_a) = test_member(4);
        let (member_b_pub, member_b) = test_member(5);
        let node_a = [0x3D; 32];
        let node_b = [0x3E; 32];
        let handle = SupervisorHandle::new();
        let h = harness(
            community,
            vec![member_a, member_b],
            None,
            true,
            Some(handle.clone()),
        );
        h.beacons.set_resolution(found_all(vec![
            beacon(member_a_pub, node_a),
            // Vouched under a key StubCtx does NOT report as enrolled — the
            // revoked-during-resolve shape.
            beacon_with_device_vk(member_b_pub, node_b, [0x66; 32]),
        ]));

        h.driver.run_one_pass().await;

        assert!(
            h.resolver.resolve_by_node_id(&node_a).is_some(),
            "the valid hit must seed"
        );
        assert!(
            h.resolver.resolve_by_node_id(&node_b).is_none(),
            "the revoked hit must not seed"
        );
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("beaconSeeded"),
            "≥1 surviving hit makes the pass a seeded pass"
        );
    }

    // ------------------------------------------------------------------
    // ZEB-910: pure coverage-verdict classification (spec §3.1).
    // ------------------------------------------------------------------

    fn row_at(node: [u8; 32], announced: u64) -> PeerRow {
        PeerRow {
            node_id: node,
            effective_announced_at_ms: announced,
        }
    }

    fn static_traffic(entries: &[([u8; 32], u64)]) -> TrafficEvidenceFn {
        let map: HashMap<[u8; 32], u64> = entries.iter().copied().collect();
        Arc::new(move |n: &[u8; 32]| map.get(n).copied())
    }

    const V_NOW: u64 = 1_700_000_000_000;

    #[test]
    fn verdict_all_proven_is_healthy() {
        let (a, b) = (OwnerAddr([1; 16]), OwnerAddr([2; 16]));
        let rows = HashMap::from([
            (a, vec![row_at([0xA1; 32], V_NOW - 1_000)]),
            (b, vec![row_at([0xB1; 32], V_NOW - 1_000)]),
        ]);
        let traffic = static_traffic(&[([0xA1; 32], V_NOW - 500), ([0xB1; 32], V_NOW - 500)]);
        assert!(matches!(
            coverage_verdict(&[a, b], &rows, &traffic, V_NOW),
            CoverageVerdict::Healthy
        ));
    }

    #[test]
    fn verdict_fresh_record_unproven_member_is_degraded() {
        let (a, b) = (OwnerAddr([1; 16]), OwnerAddr([2; 16]));
        let rows = HashMap::from([
            (a, vec![row_at([0xA1; 32], V_NOW - 1_000)]),
            (b, vec![row_at([0xB1; 32], V_NOW - 1_000)]),
        ]);
        // B's traffic stamp is just past the proven window.
        let traffic = static_traffic(&[
            ([0xA1; 32], V_NOW - 500),
            ([0xB1; 32], V_NOW - GATEWAY_PROVEN_TRAFFIC_MS - 1),
        ]);
        match coverage_verdict(&[a, b], &rows, &traffic, V_NOW) {
            CoverageVerdict::Degraded { unproven } => assert_eq!(unproven, vec![b]),
            other => panic!("expected Degraded, got {other:?}"),
        }
    }

    #[test]
    fn verdict_stale_record_member_is_not_split_evidence() {
        // B stopped announcing >24h ago: expected-unreachable, not a split.
        let (a, b) = (OwnerAddr([1; 16]), OwnerAddr([2; 16]));
        let rows = HashMap::from([
            (a, vec![row_at([0xA1; 32], V_NOW - 1_000)]),
            (
                b,
                vec![row_at(
                    [0xB1; 32],
                    V_NOW - GATEWAY_COVERAGE_RECORD_FRESH_MS - 1,
                )],
            ),
        ]);
        let traffic = static_traffic(&[([0xA1; 32], V_NOW - 500)]);
        assert!(matches!(
            coverage_verdict(&[a, b], &rows, &traffic, V_NOW),
            CoverageVerdict::Healthy
        ));
    }

    #[test]
    fn verdict_record_less_member_is_not_split_evidence() {
        let (a, b) = (OwnerAddr([1; 16]), OwnerAddr([2; 16]));
        let rows = HashMap::from([(a, vec![row_at([0xA1; 32], V_NOW - 1_000)])]);
        let traffic = static_traffic(&[([0xA1; 32], V_NOW - 500)]);
        assert!(matches!(
            coverage_verdict(&[a, b], &rows, &traffic, V_NOW),
            CoverageVerdict::Healthy
        ));
    }

    #[test]
    fn verdict_zero_proven_is_starved() {
        // Fresh records all around, but nobody has traffic evidence — the
        // original starved shape, and the connected-but-dark community too.
        let (a, b) = (OwnerAddr([1; 16]), OwnerAddr([2; 16]));
        let rows = HashMap::from([
            (a, vec![row_at([0xA1; 32], V_NOW - 1_000)]),
            (b, vec![row_at([0xB1; 32], V_NOW - 1_000)]),
        ]);
        let traffic = static_traffic(&[]);
        assert!(matches!(
            coverage_verdict(&[a, b], &rows, &traffic, V_NOW),
            CoverageVerdict::Starved
        ));
    }

    /// PR #659 review (Qodo) — DELIBERATE-semantics pin: all rows stale + no
    /// traffic is Starved, NOT Healthy. This state is indistinguishable from
    /// the ZEB-824 cold boot (offline past the freshness window; beacon
    /// repair is the only path back in); reading it as Healthy would strand
    /// exactly the case the driver exists for.
    #[test]
    fn verdict_all_rows_stale_no_traffic_is_starved_cold_boot_pin() {
        let (a, b) = (OwnerAddr([1; 16]), OwnerAddr([2; 16]));
        let stale = V_NOW - GATEWAY_COVERAGE_RECORD_FRESH_MS - 1;
        let rows = HashMap::from([
            (a, vec![row_at([0xA1; 32], stale)]),
            (b, vec![row_at([0xB1; 32], stale)]),
        ]);
        let traffic = static_traffic(&[]);
        assert!(matches!(
            coverage_verdict(&[a, b], &rows, &traffic, V_NOW),
            CoverageVerdict::Starved
        ));
    }

    #[test]
    fn verdict_traffic_on_stale_row_still_proves() {
        // Attribution rides ANY row; only the denominator needs freshness. A
        // member whose record aged out but whose device demonstrably talks to
        // us is proven, and must keep the community out of Starved.
        let a = OwnerAddr([1; 16]);
        let rows = HashMap::from([(
            a,
            vec![row_at(
                [0xA1; 32],
                V_NOW - GATEWAY_COVERAGE_RECORD_FRESH_MS - 1,
            )],
        )]);
        let traffic = static_traffic(&[([0xA1; 32], V_NOW - 500)]);
        assert!(matches!(
            coverage_verdict(&[a], &rows, &traffic, V_NOW),
            CoverageVerdict::Healthy
        ));
    }

    // ------------------------------------------------------------------
    // ZEB-910: pass-level Degraded behavior + repair actions.
    // ------------------------------------------------------------------

    fn payload_at(node_id: [u8; 32], announced: u64) -> ReachabilityAnnouncePayload {
        let mut p = test_payload(node_id);
        p.announced_at_ms = announced;
        p
    }

    /// Logs every fallback resolve's owner — observes both the ZEB-910 forced
    /// refresh (action 2) and the cache-miss discovery (action 3).
    struct OwnerLogFallback {
        log: Arc<Mutex<Vec<OwnerAddr>>>,
    }

    #[async_trait::async_trait]
    impl crate::reachability_resolver::ReachabilityFallback<OwnerAddr> for OwnerLogFallback {
        async fn resolve(&self, addr: &OwnerAddr) -> Vec<ReachabilityAnnouncePayload> {
            self.log.lock().unwrap().push(*addr);
            Vec::new()
        }
    }

    /// Drive the scheduler until `pred` holds (bounded; the caller's assertion
    /// reports the failure) — the spawned refresh/discovery tasks need polls.
    async fn wait_until(pred: impl Fn() -> bool) {
        for _ in 0..200 {
            if pred() {
                return;
            }
            tokio::task::yield_now().await;
        }
    }

    /// A proven member A + an unproven fresh-record member B, clock-driven.
    /// Returns (harness, clock, b_owner, b_node).
    fn degraded_harness(
        community: SpaceId,
        supervisor: Option<SupervisorHandle>,
    ) -> (Harness, Arc<AtomicU64>, OwnerAddr, [u8; 32]) {
        let (_a_pub, a_owner) = test_member(31);
        let (_b_pub, b_owner) = test_member(32);
        let (a_node, b_node) = ([0xA1; 32], [0xB1; 32]);
        let h = harness(community, vec![a_owner, b_owner], None, true, supervisor);
        let t0 = V_NOW;
        h.resolver
            .update(a_owner, payload_at(a_node, t0 - 1_000), hlc(t0 - 1_000));
        h.resolver
            .update(b_owner, payload_at(b_node, t0 - 1_000), hlc(t0 - 1_000));
        h.traffic.lock().unwrap().insert(a_node, t0 - 500);
        let clock = Arc::new(AtomicU64::new(t0));
        (h, clock, b_owner, b_node)
    }

    #[tokio::test]
    async fn degraded_not_due_records_degraded_waiting_without_resolve() {
        let community = SpaceId([0x31; 16]);
        let (h, clock, _b_owner, _b_node) = degraded_harness(community, None);
        let clock_for_fn = Arc::clone(&clock);
        let driver = h
            .driver
            .with_now_fn(Arc::new(move || clock_for_fn.load(Ordering::SeqCst)));

        // First sighting: due immediately (repair runs; stub has no beacon).
        driver.run_one_pass().await;
        assert_eq!(h.beacons.calls(), 1);
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("noBeacon"),
            "resolve-shaped arms stay uniform on a due degraded pass"
        );

        // Second pass, 1s later: not due — waiting, zero IO, degraded flavor.
        clock.store(V_NOW + 1_000, Ordering::SeqCst);
        driver.run_one_pass().await;
        assert_eq!(h.beacons.calls(), 1, "an undue pass must not resolve");
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("degradedWaiting")
        );
    }

    #[tokio::test]
    async fn degraded_due_pass_with_hit_records_degraded_seeded() {
        let community = SpaceId([0x32; 16]);
        let (h, clock, _b_owner, _b_node) = degraded_harness(community, None);
        let clock_for_fn = Arc::clone(&clock);
        let driver = h
            .driver
            .with_now_fn(Arc::new(move || clock_for_fn.load(Ordering::SeqCst)));
        let (_bridge_identity, bridge_pub) = test_identity(33);
        h.beacons
            .set_resolution(found(beacon(bridge_pub, [0xC1; 32])));

        driver.run_one_pass().await;

        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("degradedSeeded"),
            "a degraded seeded pass is distinguishable from starved bootstrap"
        );
        let s = h.telemetry.summary();
        assert_eq!(s.degraded_seeded, 1);
        assert_eq!(
            s.beacons_seeded, 0,
            "the starved counter must not absorb it"
        );
    }

    /// ZEB-805 pin: a Connected-but-traffic-dark member community must NOT
    /// read healthy — Connected is registry occupancy, and a blackholed conn
    /// keeps it forever.
    #[tokio::test]
    async fn connected_but_dark_member_still_repairs() {
        let community = SpaceId([0x33; 16]);
        let (_m_pub, m_owner) = test_member(34);
        let m_node = [0xD1; 32];
        let handle = SupervisorHandle::new();
        let h = harness(community, vec![m_owner], None, true, Some(handle.clone()));
        let now = now_ms();
        h.resolver
            .update(m_owner, payload_at(m_node, now - 1_000), hlc(now - 1_000));
        handle.mark_connected(m_node);
        // NO traffic stamp: the session exists, evidence does not.

        h.driver.run_one_pass().await;

        assert_eq!(
            h.beacons.calls(),
            1,
            "a connected-but-dark community must repair, not read healthy"
        );
    }

    #[tokio::test]
    async fn degraded_due_pass_refreshes_unproven_rows_only() {
        let community = SpaceId([0x34; 16]);
        let (_a_pub, a_owner) = test_member(35);
        let (_b_pub, b_owner) = test_member(36);
        let (a_node, b_node) = ([0xA2; 32], [0xB2; 32]);
        let h = harness(community, vec![a_owner, b_owner], None, true, None);
        let log = Arc::new(Mutex::new(Vec::new()));
        h.resolver.set_fallback_source(Arc::new(OwnerLogFallback {
            log: Arc::clone(&log),
        }));
        let now = now_ms();
        // A: proven, fresh row. B: unproven; row 20 min old — fresh for the
        // denominator (24h), stale for the repair refresh (10-min ladder cap).
        h.resolver
            .update(a_owner, payload_at(a_node, now - 1_000), hlc(now - 1_000));
        h.resolver.update(
            b_owner,
            payload_at(b_node, now - 20 * 60 * 1_000),
            hlc(now - 20 * 60 * 1_000),
        );
        h.traffic.lock().unwrap().insert(a_node, now - 500);

        h.driver.run_one_pass().await;

        wait_until(|| !log.lock().unwrap().is_empty()).await;
        let owners = log.lock().unwrap().clone();
        assert!(
            owners.contains(&b_owner),
            "the unproven member's record must be force-refreshed"
        );
        assert!(
            !owners.contains(&a_owner),
            "the proven member must not be refreshed by repair"
        );
    }

    #[tokio::test]
    async fn record_less_member_gets_cache_miss_discovery() {
        let community = SpaceId([0x35; 16]);
        let (_a_pub, a_owner) = test_member(37);
        let (_b_pub, b_owner) = test_member(38);
        let (_c_pub, c_owner) = test_member(39);
        let (a_node, b_node) = ([0xA3; 32], [0xB3; 32]);
        let h = harness(community, vec![a_owner, b_owner, c_owner], None, true, None);
        let log = Arc::new(Mutex::new(Vec::new()));
        h.resolver.set_fallback_source(Arc::new(OwnerLogFallback {
            log: Arc::clone(&log),
        }));
        let now = now_ms();
        // A proven; B unproven with a FRESH row (1s old — under the repair
        // refresh bar, so any fallback call for B would be a bug); C has no
        // rows at all → cache-miss discovery.
        h.resolver
            .update(a_owner, payload_at(a_node, now - 1_000), hlc(now - 1_000));
        h.resolver
            .update(b_owner, payload_at(b_node, now - 1_000), hlc(now - 1_000));
        h.traffic.lock().unwrap().insert(a_node, now - 500);

        h.driver.run_one_pass().await;

        wait_until(|| log.lock().unwrap().contains(&c_owner)).await;
        let owners = log.lock().unwrap().clone();
        assert!(
            owners.contains(&c_owner),
            "a record-less member must get a cache-miss pkarr discovery"
        );
        assert!(
            !owners.contains(&b_owner),
            "a fresh-rowed member must be neither refreshed nor re-discovered"
        );
    }

    #[tokio::test]
    async fn dormant_device_of_unproven_member_kicked_retrying_untouched() {
        use crate::reconnect_supervisor::PeerState;
        let community = SpaceId([0x36; 16]);
        let (_a_pub, a_owner) = test_member(40);
        let (_b_pub, b_owner) = test_member(41);
        let (a_node, b_dormant, b_retrying) = ([0xA4; 32], [0xB4; 32], [0xB5; 32]);
        let handle = SupervisorHandle::new();
        // Supervisor is installed AFTER the row seeding below: the resolver's
        // first-learn auto-kick would otherwise queue NewPeer for B's devices
        // (equal merge strength — first-in wins) and mask whether the REPAIR
        // kicked. With late install, any pending trigger is the repair's.
        let h = harness(community, vec![a_owner, b_owner], None, true, None);
        let now = now_ms();
        h.resolver
            .update(a_owner, payload_at(a_node, now - 1_000), hlc(now - 1_000));
        // B's two devices: one Dormant (revive), one Retrying (leave alone).
        let mut d1 = payload_at(b_dormant, now - 1_000);
        d1.home_relay_url = "https://d1.example/".into();
        let mut d2 = payload_at(b_retrying, now - 1_000);
        d2.home_relay_url = "https://d2.example/".into();
        h.resolver.update(b_owner, d1, hlc(now - 1_000));
        h.resolver.update(b_owner, d2, hlc(now - 1_000));
        h.traffic.lock().unwrap().insert(a_node, now - 500);
        h.resolver.set_supervisor(handle.clone());
        handle.set_peer_state_for_test(
            b_dormant,
            PeerState::Dormant {
                since_ms: now - 5_000,
            },
        );
        handle.set_peer_state_for_test(
            b_retrying,
            PeerState::Retrying {
                attempt: 3,
                next_at: tokio::time::Instant::now() + Duration::from_secs(60),
            },
        );

        h.driver.run_one_pass().await;

        assert_eq!(
            handle.pending_trigger(b_dormant),
            Some(ReconnectTrigger::RecordChanged),
            "the Dormant device must be revived"
        );
        assert_eq!(
            handle.pending_trigger(b_retrying),
            None,
            "a Retrying device keeps its ladder — no re-arm kick"
        );
        assert_eq!(
            handle.pending_trigger(a_node),
            None,
            "the proven member's devices are not targets"
        );
    }

    // ------------------------------------------------------------------
    // ZEB-918: ordered epoch-key candidate iteration — the rotation-skew
    // healing rung. Rotation propagates THROUGH connectivity, so a rotated
    // resolver must keep finding not-yet-rotated members' beacons (published
    // under the previous epoch key) or the community partitions into
    // rotated/un-rotated islands exactly when the un-rotated island needs a
    // connection to receive the rotation event.
    // ------------------------------------------------------------------

    const K_NEW: [u8; 32] = [0x44; 32];
    const K_OLD: [u8; 32] = [0x33; 32];

    /// Harness with two epoch-key candidates `[K_NEW, K_OLD]` (a resolver
    /// that has ingested a rotation) and per-key stub resolutions.
    fn rotation_harness(
        community: SpaceId,
        members: Vec<OwnerAddr>,
        new_key: BeaconsOutcome,
        old_key: BeaconsOutcome,
    ) -> Harness {
        let h = harness(community, members, None, true, None);
        h.ctx
            .keys
            .lock()
            .unwrap()
            .insert(community, vec![EpochKey::new(K_NEW), EpochKey::new(K_OLD)]);
        h.beacons.set_resolution_for_key(K_NEW, new_key);
        h.beacons.set_resolution_for_key(K_OLD, old_key);
        h
    }

    /// (a) Rotation skew, resolver ahead: nothing under the new key yet, a
    /// not-yet-rotated member's beacon under the previous key → seeded, with
    /// the current key probed FIRST.
    #[tokio::test]
    async fn rotation_skew_previous_epoch_candidate_heals() {
        let community = SpaceId([0x91; 16]);
        let (member_pub, member_owner) = test_member(21);
        let beacon_node_id = [0x2A; 32];
        let h = rotation_harness(
            community,
            vec![member_owner],
            not_found(),
            found(beacon(member_pub, beacon_node_id)),
        );

        h.driver.run_one_pass().await;

        assert_eq!(h.beacons.calls(), 2, "current key missed, previous tried");
        assert_eq!(
            h.beacons.probed_keys(),
            vec![K_NEW, K_OLD],
            "the LIVE current key must be probed before the previous epoch's"
        );
        assert!(
            h.resolver.resolve_by_node_id(&beacon_node_id).is_some(),
            "previous-epoch beacon must seed the resolver"
        );
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("beaconSeeded")
        );
    }

    /// (c) Nothing under either candidate: the CURRENT-key attempt's outcome
    /// is what telemetry records — the previous-epoch rung heals skew but
    /// must not mask current-key health signals.
    #[tokio::test]
    async fn no_beacon_under_any_candidate_records_current_key_outcome() {
        let community = SpaceId([0x92; 16]);
        let (_member_pub, member_owner) = test_member(22);
        let h = rotation_harness(
            community,
            vec![member_owner],
            not_found(),
            // Previous-key infrastructure trouble must not upgrade/replace
            // the current-key NotFound in telemetry.
            resolve_error(),
        );

        h.driver.run_one_pass().await;

        assert_eq!(h.beacons.calls(), 2);
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("noBeacon"),
            "current-key outcome (NotFound) is the canonical health signal"
        );
    }

    /// (d) RejectedNonMember under the current key, valid beacon under the
    /// previous: healing wins. The reject condemns one publisher's bad vouch,
    /// not the whole previous-epoch audience.
    #[tokio::test]
    async fn rejected_current_key_still_heals_via_previous_epoch() {
        let community = SpaceId([0x93; 16]);
        let (member_pub, member_owner) = test_member(23);
        let beacon_node_id = [0x2B; 32];
        let h = rotation_harness(
            community,
            vec![member_owner],
            rejected_non_member(),
            found(beacon(member_pub, beacon_node_id)),
        );

        h.driver.run_one_pass().await;

        assert!(
            h.resolver.resolve_by_node_id(&beacon_node_id).is_some(),
            "a valid previous-epoch beacon must seed despite a current-key reject"
        );
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("beaconSeeded")
        );
    }

    /// PR #659 review (CodeAnt/Greptile convergent): a current-key hit must
    /// NOT stop the scan — a rotation-straddling split publishes the
    /// un-rotated island's beacons only under the previous key, so both
    /// candidates are scanned and their hits merged (dedup by node id,
    /// current-epoch beacon winning).
    #[tokio::test]
    async fn current_key_hit_still_scans_previous_epoch_and_merges_bridges() {
        let community = SpaceId([0x94; 16]);
        let (member_pub, member_owner) = test_member(24);
        let (old_member_pub, old_member_owner) = test_member(25);
        let beacon_node_id = [0x2C; 32];
        let old_island_node_id = [0x2D; 32];
        let h = rotation_harness(
            community,
            vec![member_owner, old_member_owner],
            found(beacon(member_pub, beacon_node_id)),
            found(beacon(old_member_pub, old_island_node_id)),
        );

        h.driver.run_one_pass().await;

        assert_eq!(
            h.beacons.calls(),
            2,
            "both candidates must be scanned even when the current key hits"
        );
        assert!(
            h.resolver.resolve_by_node_id(&old_island_node_id).is_some(),
            "the previous-epoch island's beacon must seed too — it may be the only bridge"
        );
        assert_eq!(
            h.beacons.probed_keys(),
            vec![K_NEW, K_OLD],
            "the LIVE current key is still probed first"
        );
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("beaconSeeded")
        );
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
    // 2. A traffic-PROVEN member ⇒ healthy ⇒ no resolve call. (ZEB-910: the
    //    numerator is proven traffic, not session existence.)
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn proven_member_means_healthy_no_resolve() {
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
        // The member is already in the address book, has a live session, and
        // traffic demonstrably flowed just now.
        h.resolver.update(
            member_owner,
            test_payload(member_node_id),
            hlc(1_700_000_000_000),
        );
        handle.mark_connected(member_node_id);
        h.traffic.lock().unwrap().insert(member_node_id, now_ms());

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
            h.traffic.lock().unwrap().insert(connected_node, now_ms());

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
        // A live, traffic-proven session to somebody who is NOT a member of
        // this community — zenoh does not route this community's queries
        // through them.
        h.resolver.update(
            other_owner,
            test_payload(other_node_id),
            hlc(1_700_000_000_000),
        );
        handle.mark_connected(other_node_id);
        h.traffic.lock().unwrap().insert(other_node_id, now_ms());

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
    // 5. Epoch-envelope trust (Jake's decision, spec §5c): a beacon whose
    //    identity is UNKNOWN to the membership view is still seeded. This is
    //    the decided behavior, not an oversight — see 5b for what still
    //    rejects.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn beacon_with_identity_unknown_to_membership_is_seeded() {
        // Before the ZEB-824 fix round this asserted the opposite. The gate it
        // pinned compared the beacon's COMPOSITE device-address hash against
        // master-flavored member keys, so it rejected every real beacon too —
        // the feature was a no-op in production. Trust is now what the epoch
        // envelope proves, matching open-join for this record family.
        let community = SpaceId([0x15; 16]);
        let (_member_pub, member_owner) = test_member(6);
        // A member set that does NOT contain the beacon's identity — in
        // production this is the normal case, because the two sides are
        // different hash notions entirely.
        let (stranger_pub, stranger_owner) = test_member(7);
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
        let seeded = h
            .resolver
            .resolve_by_node_id(&beacon_node_id)
            .expect("an epoch-envelope-verified beacon must be seeded");
        assert_eq!(
            seeded.0, stranger_owner,
            "the seed is keyed by the owner derived from the record's identity"
        );
        assert_eq!(
            handle.pending_trigger(beacon_node_id),
            Some(ReconnectTrigger::NewPeer)
        );
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("beaconSeeded")
        );
        assert_eq!(h.telemetry.summary().rejected_non_member, 0);
    }

    // ------------------------------------------------------------------
    // 5b. The one remaining source of RejectedNonMember: the record's identity
    //     bytes do not decode. Defensive — a record reaching here through
    //     `ProdBeaconResolver` already had this Ed25519 half parsed for the
    //     inner-sig check, so firing means a wire-format or publisher bug.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn undecodable_beacon_identity_is_rejected_not_seeded() {
        let community = SpaceId([0x1F; 16]);
        let (_member_pub, member_owner) = test_member(17);
        let beacon_node_id = [0xEB; 32];
        // An Ed25519 half whose y-coordinate is not on the curve, so
        // decompression fails. Found by searching candidates against the
        // PRODUCTION predicate rather than guessed: the obvious `[0xFF; 32]`
        // does NOT work, because dalek reduces a non-canonical y and then
        // decodes it happily. The assert below is the guard that caught that.
        let mut bad_pub = [0u8; 64];
        bad_pub[32] = 0x02;
        assert!(
            harmony_identity::Identity::from_public_bytes(&bad_pub).is_err(),
            "fixture precondition: these identity bytes must be undecodable"
        );
        let handle = SupervisorHandle::new();
        let h = harness(
            community,
            vec![member_owner],
            Some(beacon(bad_pub, beacon_node_id)),
            true,
            Some(handle.clone()),
        );

        h.driver.run_one_pass().await;

        assert_eq!(h.beacons.calls(), 1);
        assert!(
            h.resolver.resolve_by_node_id(&beacon_node_id).is_none(),
            "an undecodable identity must never enter the dial view"
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

        // Heal: the member's record lands, its session comes up, and traffic
        // flows (ZEB-910: healing requires proven traffic, not a session).
        h.resolver
            .update(member_owner, test_payload(member_node_id), hlc(t0));
        handle.mark_connected(member_node_id);
        h.traffic
            .lock()
            .unwrap()
            .insert(member_node_id, clock.load(Ordering::SeqCst));
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

        // Re-starve: the session drops and traffic ceases (the stamp ages past
        // the proven window). The very next pass must attempt immediately — a
        // 600s cap carried across the heal would leave a recovered-then-lost
        // node dark for ten minutes.
        handle.evict_peer(member_node_id);
        clock.store(
            clock.load(Ordering::SeqCst) + GATEWAY_PROVEN_TRAFFIC_MS + 1,
            Ordering::SeqCst,
        );
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
            Arc::new(|_: &[u8; 32]| None),
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
            // Non-empty deliberately: keeps this a pure epoch-key test even if
            // the key-first ordering is ever revisited. (Under the shipped
            // order the members check cannot produce the verdict for any member
            // set — that is what test 9b pins.)
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
            .insert(community, vec![EpochKey::new([0x33; 32])]);
        driver.run_one_pass().await;
        assert_eq!(
            h.beacons.calls(),
            1,
            "the first pass after registration must resolve immediately"
        );
    }

    // ------------------------------------------------------------------
    // 9b. A MISSING engine reads as unregistered, not solo.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn missing_engine_records_unregistered_not_solo() {
        // Task 4 review Minor 2: `ProdGatewayDialCtx::members_of` returns an
        // EMPTY Vec when the registry has no engine — the same shape a genuinely
        // solo community produces. A members-first pass therefore records every
        // registration gap as `soloCommunity` and `engineUnregistered` never
        // fires in production at all. This ctx reproduces the prod shape exactly
        // (`epoch_key_candidates_of` → None AND `members_of` → []), so it can
        // only pass if the epoch-key probe runs first.
        let community = SpaceId([0x1E; 16]);
        let (member_pub, _member_owner) = test_member(16);
        let h = harness(
            community,
            Vec::new(),
            Some(beacon(member_pub, [0xDD; 32])),
            false,
            Some(SupervisorHandle::new()),
        );

        h.driver.run_one_pass().await;

        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("engineUnregistered"),
            "an unregistered engine must not be reported as a solo community"
        );
        assert_eq!(h.telemetry.summary().engine_unregistered, 1);
        assert_eq!(h.beacons.calls(), 0, "no key ⇒ nothing to resolve under");
    }

    // ------------------------------------------------------------------
    // 10. Ladder invariant (review r1, finding 1): an entry exists only while
    //     the community's CURRENT episode is starved. Solo, un-join, and
    //     engine-unregistered all end the episode and must drop the entry, so
    //     the next starved episode re-arms at base instead of inheriting a
    //     stale rung.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn starved_then_solo_clears_ladder_and_restarve_rearms_at_base() {
        let community = SpaceId([0x20; 16]);
        let (_member_pub, member_owner) = test_member(18);
        // No beacon ever resolves — the community stays starved while it has
        // members.
        let h = harness(
            community,
            vec![member_owner],
            None,
            true,
            Some(SupervisorHandle::new()),
        );
        let t0 = 1_700_000_000_000u64;
        let clock = Arc::new(AtomicU64::new(t0));
        let clock_for_fn = Arc::clone(&clock);
        let driver = h
            .driver
            .with_now_fn(Arc::new(move || clock_for_fn.load(Ordering::SeqCst)));

        // Starved pass 1: resolve fires, ladder armed at base.
        driver.run_one_pass().await;
        assert_eq!(h.beacons.calls(), 1);
        assert_eq!(
            driver.ladder_delay_ms(&community),
            Some(GATEWAY_DIAL_RETRY_BASE_MS)
        );

        // Due again: the rung advances PAST base, so a carried-over entry is
        // distinguishable from a fresh one below.
        clock.store(t0 + GATEWAY_DIAL_RETRY_BASE_MS, Ordering::SeqCst);
        driver.run_one_pass().await;
        assert_eq!(h.beacons.calls(), 2);
        assert_eq!(driver.ladder_delay_ms(&community), Some(60_000));

        // The last other member leaves: solo ends the starved episode and the
        // ladder entry must go with it.
        h.ctx.members.lock().unwrap().insert(community, Vec::new());
        driver.run_one_pass().await;
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("soloCommunity")
        );
        assert!(
            driver.ladder_delay_ms(&community).is_none(),
            "going solo must drop the ladder entry"
        );
        assert_eq!(h.beacons.calls(), 2, "a solo community must not resolve");

        // Members return; WITHOUT advancing time the fresh starved episode
        // must attempt immediately and re-arm at exactly the base rung — a
        // stale 60s rung carried across the solo window would leave the node
        // dark for no reason.
        h.ctx
            .members
            .lock()
            .unwrap()
            .insert(community, vec![member_owner]);
        driver.run_one_pass().await;
        assert_eq!(
            h.beacons.calls(),
            3,
            "a re-starved community must resolve at once after the solo window"
        );
        assert_eq!(
            driver.ladder_delay_ms(&community),
            Some(GATEWAY_DIAL_RETRY_BASE_MS),
            "the fresh episode must re-arm at base, not resume the stale rung"
        );
    }

    #[tokio::test]
    async fn unjoin_prunes_ladder_and_rejoin_rearms_at_base() {
        let community = SpaceId([0x21; 16]);
        let (_member_pub, member_owner) = test_member(19);
        let resolver = Arc::new(ReachabilityResolver::new());
        let ctx = Arc::new(StubCtx::new(
            HashMap::from([(community, vec![member_owner])]),
            HashMap::from([(community, vec![EpochKey::new([0x33; 32])])]),
        ));
        let beacons = Arc::new(StubBeacons::new(None));
        let telemetry = Arc::new(GatewayBootstrapTelemetry::new());
        // Mutable joined set: the harness closure returns a fixed Vec, but this
        // test IS the leave/rejoin cycle.
        let joined = Arc::new(Mutex::new(vec![community]));
        let joined_for_fn = Arc::clone(&joined);
        let t0 = 1_700_000_000_000u64;
        let clock = Arc::new(AtomicU64::new(t0));
        let clock_for_fn = Arc::clone(&clock);
        let driver = CommunityGatewayDialDriver::new(
            Arc::clone(&ctx) as Arc<dyn GatewayDialCtx>,
            Arc::clone(&beacons) as Arc<dyn BeaconResolver>,
            Arc::clone(&resolver),
            Arc::new(move || joined_for_fn.lock().unwrap().clone()),
            Arc::new(|_: &[u8; 32]| None),
        )
        .with_telemetry(Arc::clone(&telemetry))
        .with_now_fn(Arc::new(move || clock_for_fn.load(Ordering::SeqCst)));

        // Starve and walk the rung past base.
        driver.run_one_pass().await;
        clock.store(t0 + GATEWAY_DIAL_RETRY_BASE_MS, Ordering::SeqCst);
        driver.run_one_pass().await;
        assert_eq!(beacons.calls(), 2);
        assert_eq!(driver.ladder_delay_ms(&community), Some(60_000));

        // Un-join: the loop never reaches the community again, so only the
        // pass-top prune can drop the entry.
        joined.lock().unwrap().clear();
        driver.run_one_pass().await;
        assert!(
            driver.ladder_delay_ms(&community).is_none(),
            "un-join must prune the ladder entry"
        );
        assert_eq!(
            beacons.calls(),
            2,
            "an un-joined community must not resolve"
        );

        // Re-join without advancing time: a fresh episode, immediate attempt,
        // armed at base.
        joined.lock().unwrap().push(community);
        driver.run_one_pass().await;
        assert_eq!(
            beacons.calls(),
            3,
            "a re-joined starved community must resolve at once"
        );
        assert_eq!(
            driver.ladder_delay_ms(&community),
            Some(GATEWAY_DIAL_RETRY_BASE_MS),
            "the rejoin episode must arm at base, not resume the pre-leave rung"
        );
    }

    #[tokio::test]
    async fn engine_unregistered_clears_armed_ladder() {
        let community = SpaceId([0x24; 16]);
        let (_member_pub, member_owner) = test_member(20);
        let h = harness(
            community,
            vec![member_owner],
            None,
            true,
            Some(SupervisorHandle::new()),
        );
        let t0 = 1_700_000_000_000u64;
        let clock = Arc::new(AtomicU64::new(t0));
        let clock_for_fn = Arc::clone(&clock);
        let driver = h
            .driver
            .with_now_fn(Arc::new(move || clock_for_fn.load(Ordering::SeqCst)));

        // Starve and walk the rung past base.
        driver.run_one_pass().await;
        clock.store(t0 + GATEWAY_DIAL_RETRY_BASE_MS, Ordering::SeqCst);
        driver.run_one_pass().await;
        assert_eq!(h.beacons.calls(), 2);
        assert_eq!(driver.ladder_delay_ms(&community), Some(60_000));

        // The engine deregisters (restart race): the pass records the fault
        // AND clears the armed backoff.
        h.ctx.keys.lock().unwrap().remove(&community);
        driver.run_one_pass().await;
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("engineUnregistered")
        );
        assert!(
            driver.ladder_delay_ms(&community).is_none(),
            "an engine-unregistered pass must clear the armed ladder"
        );
        assert_eq!(h.beacons.calls(), 2, "no key ⇒ nothing to resolve under");

        // The engine re-registers, community still starved: immediate attempt
        // at base — a stale rung here is dark time on every boot race.
        h.ctx
            .keys
            .lock()
            .unwrap()
            .insert(community, vec![EpochKey::new([0x33; 32])]);
        driver.run_one_pass().await;
        assert_eq!(
            h.beacons.calls(),
            3,
            "the first pass after re-registration must resolve immediately"
        );
        assert_eq!(
            driver.ladder_delay_ms(&community),
            Some(GATEWAY_DIAL_RETRY_BASE_MS),
            "the post-re-registration episode must arm at base"
        );
    }

    // ------------------------------------------------------------------
    // 10b. Resolve-error de-conflation (review r1, finding 2): a resolve that
    //      errored at the pkarr layer must surface as `resolveError`, never as
    //      `noBeacon` — debug logs are off in prod, so the counters are the
    //      only signal separating "nobody published" from "pkarr is failing".
    // ------------------------------------------------------------------
    #[test]
    fn classify_empty_precedence() {
        // A miss with zero errors and zero rejects is proof-shaped absence.
        assert_eq!(classify_empty(0, 0), GatewayBootstrapOutcome::NoBeacon);
        // A miss with any errored probe carries no information.
        assert_eq!(classify_empty(1, 0), GatewayBootstrapOutcome::ResolveError);
        // ZEB-827: a present-but-rejected beacon is more informative than a
        // probe error — it wins the precedence.
        assert_eq!(
            classify_empty(0, 1),
            GatewayBootstrapOutcome::RejectedNonMember
        );
        assert_eq!(
            classify_empty(2, 1),
            GatewayBootstrapOutcome::RejectedNonMember
        );
        // ("Hits win outright" now lives in the driver loop: a nonempty
        // `hits` never reaches classify_empty — pinned by the seed-path tests.)
    }

    #[tokio::test]
    async fn resolve_error_records_resolve_error_not_no_beacon() {
        let community = SpaceId([0x25; 16]);
        let (_member_pub, member_owner) = test_member(22);
        let h = harness(
            community,
            vec![member_owner],
            None,
            true,
            Some(SupervisorHandle::new()),
        );
        h.beacons.set_resolution(resolve_error());

        h.driver.run_one_pass().await;

        assert_eq!(h.beacons.calls(), 1, "a starved community must resolve");
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("resolveError")
        );
        let s = h.telemetry.summary();
        assert_eq!(s.resolve_error, 1, "the errored resolve must be counted");
        assert_eq!(
            s.no_beacon, 0,
            "an errored resolve must NOT masquerade as noBeacon"
        );
    }

    // ------------------------------------------------------------------
    // 10c. ZEB-827 strict: a beacon present but lacking a valid membership
    //      vouch resolves as `RejectedNonMember` — recorded, never seeded.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn rejected_beacon_records_rejected_non_member_and_does_not_seed() {
        let community = SpaceId([0x27; 16]);
        let (_member_pub, member_owner) = test_member(24);
        let h = harness(
            community,
            vec![member_owner],
            None,
            true,
            Some(SupervisorHandle::new()),
        );
        h.beacons.set_resolution(rejected_non_member());

        h.driver.run_one_pass().await;

        assert_eq!(h.beacons.calls(), 1, "a starved community must resolve");
        assert_eq!(
            outcome_of(&h.telemetry, &community).as_deref(),
            Some("rejectedNonMember")
        );
        let s = h.telemetry.summary();
        assert_eq!(
            s.rejected_non_member, 1,
            "the rejected beacon must be counted"
        );
        assert_eq!(s.beacons_seeded, 0, "a rejected beacon must NOT be seeded");
    }

    // ------------------------------------------------------------------
    // 10d. CR-2 TOCTOU: a beacon accepted at resolve-time whose device key is
    //      no longer enrolled at seed-time (revoked during the async resolve)
    //      must be re-rejected, never seeded.
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn beacon_revoked_during_resolve_is_not_seeded() {
        let community = SpaceId([0x2A; 16]);
        let (member_pub, member_owner) = test_member(31);
        // The Found beacon vouches under a device key that is NOT the fixture key
        // `StubCtx` reports as enrolled — modelling a revoke that lands between
        // the resolve-time snapshot and the seed-time re-check.
        let h = harness(
            community,
            vec![member_owner],
            Some(beacon_with_device_vk(member_pub, [0x44; 32], [0xAA; 32])),
            true,
            Some(SupervisorHandle::new()),
        );

        h.driver.run_one_pass().await;

        assert!(
            h.resolver.resolve_by_node_id(&[0x44; 32]).is_none(),
            "a beacon revoked during resolve must NOT be seeded"
        );
        let s = h.telemetry.summary();
        assert_eq!(
            s.rejected_non_member, 1,
            "the stale accept must be re-rejected"
        );
        assert_eq!(
            s.beacons_seeded, 0,
            "a revoked-mid-resolve beacon must NOT seed"
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
