//! ZEB-309 Phase 4a-main: multi-engine integration tests for Tier 3
//! sortition + STAR ratification + drafting. Builds on the ZEB-307
//! two-engine bidirectional-mpsc bridge pattern from
//! community_dfrost_transport_integration.rs.
//!
//! ## Layout
//!
//! - `fixture_identity` — deterministic Ed25519 keys + OwnerAddrs that bind
//!   through the address_hash verify gate.
//! - `StaticIdentityResolver` — minimal IdentityResolver for tests.
//! - `setup_two_voting_engine_bridge` — creates two VotingLogEngine instances
//!   wired with bidirectional mpsc bridges (same shape as dfrost transport test).
//! - `wait_for_log` — polling helper that avoids tokio::time::sleep flakiness.
//! - Event builders: `build_tier3_poll_create_event`, `build_decline_event`,
//!   `build_draft_candidate_event`, `build_draft_approval_event`,
//!   `build_ratification_ballot_event`, `build_sortition_failed_event`.
//! - Smoke tests: bridge starts/shuts down cleanly; Tier 1 ballot exchange
//!   works through the bridge (sanity check before Tier 3 behavior tests in
//!   Tasks 13-16).
//!
//! ## Why no DKG setup here
//!
//! A full DfrostLog DKG flow is heavy and already tested in ZEB-301/303/307.
//! Task 12's scope is fixture infrastructure. DfrostLog integration (beacon
//! path) is deferred to Task 13 which sets up a pre-computed kd=vb state.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use harmony_app::community_membership::ChannelId;
use harmony_app::community_state_sync::IdentityResolver;
use harmony_app::community_voting_approval::Tier1PollConfig;
use harmony_app::community_voting_core::VotingIdentityResolver;
use harmony_app::community_voting_core::{
    build_signed_poll_create_tier1, derive_poll_id, CandidateEventHash, Eligibility, MemberAttrs,
    MembershipSnapshot, PollEventKindCode, RatificationBallotPayload, SignedVotingEvent, Tier,
    Tier3PollConfigPayload,
};
use harmony_app::community_voting_log::{
    ApplyError, MembershipSnapshotResolver, SnapshotResolverError, VotingLog,
};
use harmony_app::community_voting_log_engine::{VotingLogEngine, VotingLogEngineParams};
use harmony_app::community_voting_sortition::fisher_yates_select;
use harmony_app::community_voting_star::tally_star;
use harmony_app::community_voting_tier3::{
    drafting_advancers, ratification_candidates_ordering, synthesize_status_quo, verify_sd,
    verify_sf, Stage, VerifyError,
};
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use tauri::Listener;
use tokio::sync::{mpsc, Mutex};

// ─── Identity-resolver helper ──────────────────────────────────────────────────

/// Minimal `IdentityResolver` backed by a `HashMap`. Mirrors `StaticResolver`
/// in `community_dfrost_transport_integration.rs`. Tests that exercise the
/// VotingLogEngine inbound path need a resolver if the verify gate is ever
/// wired; for now it is present for completeness.
#[allow(dead_code)]
struct StaticIdentityResolver(HashMap<OwnerAddr, [u8; 64]>);

#[async_trait::async_trait]
impl IdentityResolver for StaticIdentityResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        self.0.get(addr).copied()
    }
}

// ─── VotingIdentityResolver + MembershipSnapshotResolver for bridge tests ────────

/// Mutable bridge-test resolvers: identity map + membership snapshot both
/// guarded by a `std::sync::RwLock` so they can be populated AFTER the
/// engines are started (the resolver Arc is shared with both engines at
/// setup time; tests add identities incrementally as test actors are created).
pub struct BridgeTestResolvers {
    identity: std::sync::RwLock<HashMap<OwnerAddr, [u8; 64]>>,
    snapshot: std::sync::RwLock<MembershipSnapshot>,
}

impl BridgeTestResolvers {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            identity: std::sync::RwLock::new(HashMap::new()),
            snapshot: std::sync::RwLock::new(MembershipSnapshot {
                members: HashMap::new(),
            }),
        })
    }

    /// Register an identity and add it to the membership snapshot.
    pub fn add_identity(&self, id: &TestIdentity) {
        self.identity.write().unwrap().insert(id.owner, id.pub_64);
        self.snapshot.write().unwrap().members.insert(
            id.owner,
            MemberAttrs {
                power: 1,
                vouching_depth: 1,
            },
        );
    }
}

#[async_trait::async_trait]
impl VotingIdentityResolver for BridgeTestResolvers {
    async fn resolve(&self, owner: &OwnerAddr) -> Option<[u8; 64]> {
        self.identity.read().unwrap().get(owner).copied()
    }
}

#[async_trait::async_trait]
impl MembershipSnapshotResolver for BridgeTestResolvers {
    async fn snapshot_at(
        &self,
        _community_id: harmony_app::owner_state_types::SpaceId,
        _hlc: &harmony_app::owner_state_types::Hlc,
    ) -> Result<MembershipSnapshot, SnapshotResolverError> {
        Ok(self.snapshot.read().unwrap().clone())
    }
}

// ─── Test identity ─────────────────────────────────────────────────────────────

/// A convenience bundle returned by `fixture_identity`.
pub struct TestIdentity {
    pub owner: OwnerAddr,
    pub signing_key: ed25519_dalek::SigningKey,
    #[allow(dead_code)]
    pub verifying_key: ed25519_dalek::VerifyingKey,
    /// Raw 64-byte identity composite (X25519 || Ed25519). Needed to
    /// populate `StaticIdentityResolver` so the address_hash binding passes
    /// through the dfrost engine's `verify_signed_committee_event` gate.
    pub pub_64: [u8; 64],
}

/// Build a `TestIdentity` from a single-byte seed. The returned `owner`'s
/// `address_hash` field is derived from the public key bytes — the same
/// binding enforced by `verify_signed_committee_event` in the dfrost engine.
///
/// Mirrors `fixture_identity` in `community_dfrost_transport_integration.rs`.
pub fn fixture_identity(seed: u8) -> TestIdentity {
    let priv_id = harmony_identity::PrivateIdentity::from_seed(&[seed; 32]);
    let owner = OwnerAddr(priv_id.identity.address_hash);
    let pub_64 = priv_id.identity.to_public_bytes();
    let private_bytes = priv_id.to_private_bytes();
    let mut ed_secret = [0u8; 32];
    ed_secret.copy_from_slice(&private_bytes[32..64]);
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_secret);
    let verifying_key = signing_key.verifying_key();
    TestIdentity {
        owner,
        signing_key,
        verifying_key,
        pub_64,
    }
}

// ─── HLC helpers ──────────────────────────────────────────────────────────────

fn hlc_at(wall_ms: u64, device_id: &str) -> Hlc {
    Hlc {
        wall_ms,
        logical: 0,
        device_id: device_id.into(),
    }
}

// ─── Two-engine bridge ─────────────────────────────────────────────────────────

/// Handles for both sides of a two-VotingLogEngine bridge. Drop to shut
/// everything down cleanly.
pub struct TwoVotingEngines {
    pub engine_a: Arc<VotingLogEngine<tauri::test::MockRuntime>>,
    pub engine_b: Arc<VotingLogEngine<tauri::test::MockRuntime>>,
    pub log_a: Arc<Mutex<VotingLog>>,
    pub log_b: Arc<Mutex<VotingLog>>,
    /// Shared mutable resolvers for both engines' inbound paths.
    /// Tests call `resolvers.add_identity(&id)` for each actor that
    /// will publish events crossing the bridge.
    pub resolvers: Arc<BridgeTestResolvers>,
    /// ZEB-850 Task 5: each engine's D-FROST log, wired via
    /// `install_beacon_oracle_for` so the tier-3 peer-ingest `verify_ss`
    /// gate can consult a `DfrostBeaconOracle` and ADMIT a test-injected
    /// kd=ss. Seed a VRF beacon into both with `seed_ss_beacon` after
    /// PollCreate and before publishing the kd=ss.
    pub dfrost_log_a: Arc<Mutex<harmony_app::community_dfrost_log::DfrostLog>>,
    pub dfrost_log_b: Arc<Mutex<harmony_app::community_dfrost_log::DfrostLog>>,
}

/// ZEB-850 Task 5: wire a lightweight beacon oracle into `engine` so the
/// tier-3 peer-ingest `verify_ss` gate can consult a `DfrostBeaconOracle`
/// (`beacon_oracle_holder()` returns `Some`) and ADMIT a legitimately
/// test-injected kd=ss rather than fail-closed on `BeaconNotYetAvailable`.
///
/// Registers a `DfrostLogEngine` (with a no-op `BeaconRequester`) for
/// `community_id` in a fresh registry, installs the handle, and returns the
/// engine's `DfrostLog` so `seed_ss_beacon` can insert VRF outputs straight
/// into `beacon_index`. Nothing here drives the beacon apply path: the no-op
/// requester never issues a real request and no `VrfBeacon` event is ever
/// applied, so the engine's `on_dfrost_beacon` self-mint path never fires
/// (which would change what the tests exercise).
async fn install_beacon_oracle_for(
    engine: &Arc<VotingLogEngine<tauri::test::MockRuntime>>,
    community_id: SpaceId,
) -> Arc<Mutex<harmony_app::community_dfrost_log::DfrostLog>> {
    use harmony_app::community_dfrost_log::DfrostLog;
    use harmony_app::community_dfrost_log_engine::{DfrostLogEngineParams, DfrostLogRegistry};
    use harmony_app::community_voting_log_engine::BeaconRequester;

    let dfrost_log = Arc::new(Mutex::new(DfrostLog::new()));
    let registry = Arc::new(DfrostLogRegistry::<tauri::test::MockRuntime>::new());

    // A DfrostLogEngine needs an AppHandle + publish/subscribe channels. None of
    // it is exercised — tests seed `beacon_index` directly, so no VrfBeacon event
    // is ever applied through this engine's receive loop. The cloned AppHandle
    // survives the mock_app drop (same pattern as the in-crate engine tests).
    let app = tauri::test::mock_app();
    let app_handle = app.handle().clone();
    let (pub_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_sub_tx, sub_rx) = mpsc::channel::<Vec<u8>>(8);

    DfrostLogRegistry::register(
        &registry,
        DfrostLogEngineParams {
            community_id,
            dfrost_log: Arc::clone(&dfrost_log),
            publisher_tx: pub_tx,
            subscriber_rx: sub_rx,
            app_handle: Some(app_handle),
            self_addr: OwnerAddr([0u8; 16]),
            self_x25519_priv: [0u8; 32],
            identity_resolver: Arc::new(StaticIdentityResolver(HashMap::new())),
            registry_weak: None,
            driver: None,
            membership_resolver: None,
            orchestrator_config: Default::default(),
            persist: None,
        },
    )
    .await;

    // No-op BeaconRequester: present only so `beacon_oracle_holder()` returns
    // `Some`; never issues a real beacon request.
    let requester: BeaconRequester =
        Arc::new(|_cid, _seed, _epoch| Box::pin(async { Ok(String::new()) }));
    VotingLogEngine::install_dfrost_handle(engine, registry, requester).await;

    dfrost_log
}

impl TwoVotingEngines {
    /// ZEB-850 Task 5: seed a VRF beacon into BOTH engines' D-FROST logs so the
    /// tier-3 peer-ingest `verify_ss` gate ADMITS a test-injected kd=ss.
    ///
    /// `verify_ss` looks up `beacon_index[derive_vrf_seed(derive_beacon_seed(
    /// poll_create_event_hash, community_epoch), community_epoch)]` and recomputes
    /// the sortition from the stored `vrf_output`, so `vrf_output` MUST equal the
    /// value the test used to build the injected sortition. The
    /// `poll_create_event_hash` + `community_epoch` are read straight from the
    /// already-applied poll state (identical on both logs), so the caller only
    /// supplies the `poll_id` and the `vrf_output`.
    ///
    /// Call AFTER PollCreate is applied (so the poll's meta exists) and BEFORE
    /// publishing the kd=ss. Seeds both logs because tests bridge in either
    /// direction.
    pub async fn seed_ss_beacon(
        &self,
        poll_id: harmony_app::community_voting_core::PollId,
        vrf_output: [u8; 32],
    ) {
        let (poll_create_event_hash, community_epoch) = {
            let log = self.log_a.lock().await;
            let meta = &log
                .polls
                .get(&poll_id)
                .and_then(|ps| ps.tier_state.as_tier3())
                .expect("seed_ss_beacon: tier3 poll must be applied to log_a first")
                .meta;
            (meta.poll_create_event_hash, meta.community_epoch)
        };

        let seed = harmony_app::community_voting_sortition::derive_beacon_seed(
            &poll_create_event_hash,
            community_epoch,
        );
        let message_hash =
            harmony_app::community_dfrost_types::derive_vrf_seed(&seed, community_epoch);

        for dfrost_log in [&self.dfrost_log_a, &self.dfrost_log_b] {
            let mut log = dfrost_log.lock().await;
            log.beacon_index.insert(message_hash, vrf_output);
            // `find_vrf_beacon_output_by_seed` ignores committee_state, but keep
            // it consistent with a real post-DKG log (matches the in-crate seed
            // pattern in community_dfrost_log_engine.rs).
            log.committee_state.active = true;
            log.committee_state.current_epoch = community_epoch;
        }
    }
}

/// Wire two `VotingLogEngine` instances with bidirectional mpsc channels,
/// mirroring the two-engine bridge in `community_dfrost_transport_integration.rs`.
///
/// Spawns two forwarder tasks that relay published bytes from engine A's
/// outbound channel into engine B's inbound channel and vice versa. When
/// either engine is dropped its publisher sender closes, the forwarder
/// receives `None`, and exits cleanly.
///
/// `resolvers`: optional identity + membership resolvers for both engines'
/// inbound paths. `None` means inbound events are rejected (resolvers not
/// installed) — safe for tests that don't test bridge convergence on
/// inbound events.
pub async fn setup_two_voting_engine_bridge(community_id: SpaceId) -> TwoVotingEngines {
    // Channel pairs: each engine gets its own pub_tx/sub_rx.
    // The forwarder tasks relay: a_pub_rx → b_sub_tx and b_pub_rx → a_sub_tx.
    let (a_pub_tx, mut a_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_pub_tx, mut b_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (a_sub_tx, a_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_sub_tx, b_sub_rx) = mpsc::channel::<Vec<u8>>(64);

    // Forwarder: A → B
    let b_sub_tx_clone = b_sub_tx.clone();
    tokio::spawn(async move {
        while let Some(packet) = a_pub_rx.recv().await {
            if b_sub_tx_clone.send(packet).await.is_err() {
                break;
            }
        }
    });

    // Forwarder: B → A
    let a_sub_tx_clone = a_sub_tx.clone();
    tokio::spawn(async move {
        while let Some(packet) = b_pub_rx.recv().await {
            if a_sub_tx_clone.send(packet).await.is_err() {
                break;
            }
        }
    });

    let log_a = Arc::new(Mutex::new(VotingLog::new()));
    let log_b = Arc::new(Mutex::new(VotingLog::new()));

    // ZEB-310 Task 9: provide engine_a with a per-device HLC tracker
    // + device_id so the engine-auto orchestration hook
    // (`maybe_trigger_engine_auto_orchestration`) can reserve HLCs on
    // the local lane when triggered. engine_b is the peer — no
    // orchestration on its side — so we wire one for it too (cheap;
    // empty tracker behaves identically to None for non-orchestrating
    // engines).
    let a_hlc_tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
        "engine-a".into(),
    )));
    let b_hlc_tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
        "engine-a".into(),
    )));

    // ZEB-298+ZEB-312 PR 1: shared mutable resolver for bridge tests.
    // Tests must call `engines.resolvers.add_identity(&id)` for each
    // actor that publishes events crossing the bridge, before those events
    // are sent.
    let resolvers = BridgeTestResolvers::new();
    let id_resolver_a: Arc<dyn VotingIdentityResolver> =
        Arc::clone(&resolvers) as Arc<dyn VotingIdentityResolver>;
    let mem_resolver_a: Arc<dyn MembershipSnapshotResolver> =
        Arc::clone(&resolvers) as Arc<dyn MembershipSnapshotResolver>;
    let id_resolver_b: Arc<dyn VotingIdentityResolver> =
        Arc::clone(&resolvers) as Arc<dyn VotingIdentityResolver>;
    let mem_resolver_b: Arc<dyn MembershipSnapshotResolver> =
        Arc::clone(&resolvers) as Arc<dyn MembershipSnapshotResolver>;

    let engine_a = VotingLogEngine::start(VotingLogEngineParams {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        community_id,
        voting_log: Arc::clone(&log_a),
        publisher_tx: a_pub_tx,
        subscriber_rx: a_sub_rx,
        hlc_tracker: Some(a_hlc_tracker),
        device_id: Some("engine-a".into()),
        app_handle: None,
        identity_resolver: Some(id_resolver_a),
        membership_resolver: Some(mem_resolver_a),
    })
    .await;

    let engine_b = VotingLogEngine::start(VotingLogEngineParams {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        community_id,
        voting_log: Arc::clone(&log_b),
        publisher_tx: b_pub_tx,
        subscriber_rx: b_sub_rx,
        hlc_tracker: Some(b_hlc_tracker),
        device_id: Some("engine-b".into()),
        app_handle: None,
        identity_resolver: Some(id_resolver_b),
        membership_resolver: Some(mem_resolver_b),
    })
    .await;

    // ZEB-850 Task 5: wire a beacon oracle into each engine so the tier-3
    // peer-ingest verify_ss gate can admit a test-injected kd=ss.
    let dfrost_log_a = install_beacon_oracle_for(&engine_a, community_id).await;
    let dfrost_log_b = install_beacon_oracle_for(&engine_b, community_id).await;

    TwoVotingEngines {
        engine_a,
        engine_b,
        log_a,
        log_b,
        resolvers,
        dfrost_log_a,
        dfrost_log_b,
    }
}

/// ZEB-310 Task 9 fixture: `setup_two_voting_engine_bridge` + installs
/// `proposer`'s signing key on `engine_a` so the engine-auto orchestration
/// hook can mint follow-up events (kd=sf, and Tasks 10/11 kd=cl + kd=rs)
/// on the proposer's behalf.
///
/// engine_b is left in read-only peer mode (no signing key installed) so
/// the test exercises a realistic distributed shape: only the engine
/// that holds the proposer's key originates the follow-up event; the
/// peer receives it via the bridge.
pub async fn setup_two_voting_engine_bridge_with_signing(
    community_id: SpaceId,
    proposer: &TestIdentity,
) -> TwoVotingEngines {
    let engines = setup_two_voting_engine_bridge(community_id).await;
    // Register the proposer so their events can be verified on the inbound path.
    engines.resolvers.add_identity(proposer);
    let signing_key = Arc::new(proposer.signing_key.clone());
    engines
        .engine_a
        .install_local_signing_key(signing_key, proposer.owner)
        .await;
    engines
}

/// ZEB-310 Task 12 fixture: like `setup_two_voting_engine_bridge_with_signing`
/// but also installs an `app_handle` on `engine_a` so the engine's post-apply
/// hook emits the four Tier 3 lifecycle Tauri events
/// (`voting-tier3-sortition-complete`, `voting-tier3-drafting-open`,
/// `voting-tier3-ratification-open`, `voting-tier3-finalized`) to a
/// `tauri::test::MockRuntime` event bus that tests can `listen` to.
///
/// `engine_b` keeps `app_handle: None` so emit code only runs on the
/// authoritative side — mirrors the realistic UX shape (only the user's
/// device fires UI notifications, not every peer).
pub async fn setup_two_voting_engine_bridge_with_signing_and_app(
    community_id: SpaceId,
    proposer: &TestIdentity,
    app_handle: tauri::AppHandle<tauri::test::MockRuntime>,
) -> TwoVotingEngines {
    let (a_pub_tx, mut a_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_pub_tx, mut b_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (a_sub_tx, a_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_sub_tx, b_sub_rx) = mpsc::channel::<Vec<u8>>(64);

    let b_sub_tx_clone = b_sub_tx.clone();
    tokio::spawn(async move {
        while let Some(packet) = a_pub_rx.recv().await {
            if b_sub_tx_clone.send(packet).await.is_err() {
                break;
            }
        }
    });
    let a_sub_tx_clone = a_sub_tx.clone();
    tokio::spawn(async move {
        while let Some(packet) = b_pub_rx.recv().await {
            if a_sub_tx_clone.send(packet).await.is_err() {
                break;
            }
        }
    });

    let log_a = Arc::new(Mutex::new(VotingLog::new()));
    let log_b = Arc::new(Mutex::new(VotingLog::new()));

    let a_hlc_tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
        "engine-a".into(),
    )));
    let b_hlc_tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
        "engine-a".into(),
    )));

    // ZEB-298+ZEB-312 PR 1: shared mutable resolver for bridge tests.
    let resolvers = BridgeTestResolvers::new();
    resolvers.add_identity(proposer);
    let id_resolver_a: Arc<dyn VotingIdentityResolver> =
        Arc::clone(&resolvers) as Arc<dyn VotingIdentityResolver>;
    let mem_resolver_a: Arc<dyn MembershipSnapshotResolver> =
        Arc::clone(&resolvers) as Arc<dyn MembershipSnapshotResolver>;
    let id_resolver_b: Arc<dyn VotingIdentityResolver> =
        Arc::clone(&resolvers) as Arc<dyn VotingIdentityResolver>;
    let mem_resolver_b: Arc<dyn MembershipSnapshotResolver> =
        Arc::clone(&resolvers) as Arc<dyn MembershipSnapshotResolver>;

    let engine_a = VotingLogEngine::start(VotingLogEngineParams {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        community_id,
        voting_log: Arc::clone(&log_a),
        publisher_tx: a_pub_tx,
        subscriber_rx: a_sub_rx,
        hlc_tracker: Some(a_hlc_tracker),
        device_id: Some("engine-a".into()),
        app_handle: Some(app_handle),
        identity_resolver: Some(id_resolver_a),
        membership_resolver: Some(mem_resolver_a),
    })
    .await;

    let engine_b = VotingLogEngine::start(VotingLogEngineParams {
        adopt_floor: harmony_app::hlc_adopt_floor::HlcAdoptFloor::new(),
        community_id,
        voting_log: Arc::clone(&log_b),
        publisher_tx: b_pub_tx,
        subscriber_rx: b_sub_rx,
        hlc_tracker: Some(b_hlc_tracker),
        device_id: Some("engine-b".into()),
        app_handle: None,
        identity_resolver: Some(id_resolver_b),
        membership_resolver: Some(mem_resolver_b),
    })
    .await;

    let signing_key = Arc::new(proposer.signing_key.clone());
    engine_a
        .install_local_signing_key(signing_key, proposer.owner)
        .await;

    // ZEB-850 Task 5: wire a beacon oracle into each engine so the tier-3
    // peer-ingest verify_ss gate can admit a test-injected kd=ss.
    let dfrost_log_a = install_beacon_oracle_for(&engine_a, community_id).await;
    let dfrost_log_b = install_beacon_oracle_for(&engine_b, community_id).await;

    TwoVotingEngines {
        engine_a,
        engine_b,
        log_a,
        log_b,
        resolvers,
        dfrost_log_a,
        dfrost_log_b,
    }
}

// ─── Polling helper ────────────────────────────────────────────────────────────

/// Convenience wrapper: poll log_a + log_b until `predicate(log)` is true
/// on the target log. Times out after 5 000 ms.
pub async fn wait_for_log<F>(
    label: &str,
    log: &Arc<Mutex<VotingLog>>,
    predicate: F,
) -> Result<(), String>
where
    F: Fn(&VotingLog) -> bool,
{
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        {
            let guard = log.lock().await;
            if predicate(&guard) {
                return Ok(());
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("wait_for_log({label}) timed out after 5s"));
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ─── Event builders ────────────────────────────────────────────────────────────

/// Default valid Tier 3 poll config (sortition_size 20 satisfies the [20,300]
/// validation range; windows satisfy the ≥60s floor).
pub fn default_tier3_config() -> Tier3PollConfigPayload {
    Tier3PollConfigPayload {
        proposal_text: "Should the community adopt proposal X?".into(),
        sortition_size: 20,
        deliberation_window_seconds: 3600,
        drafting_window_seconds: 3600,
        ratification_window_seconds: 3600,
        privacy_mode: "pu".into(),
        incentive_mode: "a".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
        predecessor: None,
        ce: None,
    }
}

/// Build a signed Tier 3 PollCreate (kd=cr, tier=Sortition) event.
///
/// Thin `&TestIdentity` adapter over the core
/// `build_signed_poll_create_tier3` — see `community_voting_core.rs` for
/// the canonical `(&SigningKey, OwnerAddr)` shape used by the IPC layer.
pub fn build_tier3_poll_create_event(
    proposer: &TestIdentity,
    config: &Tier3PollConfigPayload,
    hlc: Hlc,
) -> SignedVotingEvent {
    harmony_app::community_voting_core::build_signed_poll_create_tier3(
        &proposer.signing_key,
        proposer.owner,
        config,
        hlc,
    )
    .expect("build_signed_poll_create_tier3")
}

/// Build a signed kd=md MiniPublicDecline event.
pub fn build_decline_event(
    actor: &TestIdentity,
    poll_id: harmony_app::community_voting_core::PollId,
    hlc: Hlc,
) -> SignedVotingEvent {
    harmony_app::community_voting_core::build_signed_mini_public_decline(
        &actor.signing_key,
        actor.owner,
        poll_id,
        None,
        hlc,
    )
    .expect("build_signed_mini_public_decline")
}

/// Build a signed kd=dc DraftCandidate event.
pub fn build_draft_candidate_event(
    actor: &TestIdentity,
    poll_id: harmony_app::community_voting_core::PollId,
    text: &str,
    hlc: Hlc,
) -> SignedVotingEvent {
    harmony_app::community_voting_core::build_signed_draft_candidate(
        &actor.signing_key,
        actor.owner,
        poll_id,
        text.into(),
        hlc,
    )
    .expect("build_signed_draft_candidate")
}

/// Build a signed kd=da DraftApproval event.
pub fn build_draft_approval_event(
    actor: &TestIdentity,
    poll_id: harmony_app::community_voting_core::PollId,
    candidate_event_hash: CandidateEventHash,
    hlc: Hlc,
) -> SignedVotingEvent {
    harmony_app::community_voting_core::build_signed_draft_approval(
        &actor.signing_key,
        actor.owner,
        poll_id,
        candidate_event_hash,
        hlc,
    )
    .expect("build_signed_draft_approval")
}

/// Build a signed kd=rb RatificationBallot event.
/// `scores` is one byte per ratification candidate (0..=5).
pub fn build_ratification_ballot_event(
    actor: &TestIdentity,
    poll_id: harmony_app::community_voting_core::PollId,
    scores: Vec<u8>,
    hlc: Hlc,
) -> SignedVotingEvent {
    harmony_app::community_voting_core::build_signed_ratification_ballot(
        &actor.signing_key,
        actor.owner,
        poll_id,
        scores,
        hlc,
    )
    .expect("build_signed_ratification_ballot")
}

/// Build a signed kd=sf SortitionFailed event.
pub fn build_sortition_failed_event(
    proposer: &TestIdentity,
    poll_id: harmony_app::community_voting_core::PollId,
    hlc: Hlc,
) -> SignedVotingEvent {
    harmony_app::community_voting_core::build_signed_sortition_failed(
        &proposer.signing_key,
        proposer.owner,
        poll_id,
        hlc,
    )
    .expect("build_signed_sortition_failed")
}

/// Build a signed kd=ss SortitionSelection event (engine-generated shape:
/// zero-sig actor is `OwnerAddr([0; 16])`). Used in tests that inject a
/// pre-computed sortition result without a real DKG/beacon.
///
/// ZEB-298+ZEB-312 PR 1: now signs the event with `actor`'s key so it
/// passes `verify_voting_event` on the receiving engine. The actor must be
/// in the bridge resolvers' snapshot + identity map.
pub fn build_sortition_selection_event(
    actor: &TestIdentity,
    poll_id: harmony_app::community_voting_core::PollId,
    primary: Vec<OwnerAddr>,
    backup: Vec<OwnerAddr>,
    hlc: Hlc,
) -> SignedVotingEvent {
    harmony_app::community_voting_core::build_signed_sortition_selection(
        &actor.signing_key,
        actor.owner,
        poll_id,
        primary,
        backup,
        hlc,
    )
    .expect("build_signed_sortition_selection")
}

/// Build a signed kd=cl PollClose event (Tier 3).
pub fn build_poll_close_event(
    actor: &TestIdentity,
    poll_id: harmony_app::community_voting_core::PollId,
    hlc: Hlc,
) -> SignedVotingEvent {
    harmony_app::community_voting_core::build_signed_poll_close_tier3(
        &actor.signing_key,
        actor.owner,
        poll_id,
        hlc,
    )
    .expect("build_signed_poll_close_tier3")
}

/// Build a signed kd=rs PollResult event (Tier 3).
pub fn build_poll_result_event(
    actor: &TestIdentity,
    poll_id: harmony_app::community_voting_core::PollId,
    result: harmony_app::community_voting_star::StarResult,
    hlc: Hlc,
) -> SignedVotingEvent {
    harmony_app::community_voting_core::build_signed_poll_result_tier3(
        &actor.signing_key,
        actor.owner,
        poll_id,
        result,
        hlc,
    )
    .expect("build_signed_poll_result_tier3")
}

// ─── SMOKE TESTS ───────────────────────────────────────────────────────────────

/// Verify that `setup_two_voting_engine_bridge` constructs + tears down
/// without panics. This is a fixture meta-test — if this fails, all
/// subsequent Task 13-16 tests are invalid.
#[tokio::test]
async fn smoke_two_engine_bridge_tier3_starts_and_shuts_down() {
    let community_id = SpaceId([0xaa; 16]);
    let engines = setup_two_voting_engine_bridge(community_id).await;
    // Drop explicitly so forwarder tasks see channel closure and exit.
    drop(engines);
    // No assertion — just no panic.
}

/// Sanity check: an existing Tier 1 PollCreate published via engine_a
/// crosses the bridge and is applied by engine_b. Confirms the bridge is
/// functional before Tier 3 tests build on it.
///
/// ZEB-298+ZEB-312 PR 1: the production gate is removed; the inbound path
/// now verifies signatures. This test uses `BridgeTestResolvers` with alice
/// registered so the inbound PollCreate passes verification on engine_b.
#[tokio::test]
async fn smoke_tier3_bridge_tier1_event_crosses_to_peer() {
    let community_id = SpaceId([0xbb; 16]);
    let engines = setup_two_voting_engine_bridge(community_id).await;

    let alice = fixture_identity(0xA1);
    // Register alice so her PollCreate passes inbound verification on engine_b.
    engines.resolvers.add_identity(&alice);

    let tier1_cfg = Tier1PollConfig {
        options: vec!["Yes".into(), "No".into()],
        window_seconds: 3600,
        quorum: None,
        threshold_percent: None,
        multi_winner: None,
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        channel_id: ChannelId([0x11; 16]),
    };
    let hlc = hlc_at(1_000, "alice-dev");
    let event = build_signed_poll_create_tier1(&alice.signing_key, alice.owner, &tier1_cfg, hlc)
        .expect("build_signed_poll_create_tier1");

    engines
        .engine_a
        .publish_event(event, None)
        .await
        .expect("engine_a publish Tier1 PollCreate");

    // Engine A should have applied it locally.
    {
        let log = engines.log_a.lock().await;
        assert_eq!(
            log.polls.len(),
            1,
            "engine_a must have exactly one poll after publish"
        );
    }

    // Engine B must receive + apply via the bridge.
    wait_for_log("engine_b sees tier1 poll", &engines.log_b, |log| {
        log.polls.len() == 1
    })
    .await
    .expect("engine_b must apply the Tier1 PollCreate from engine_a");

    drop(engines);
}

/// Multi-engine 4-stage E2E happy path for Tier 3.
///
/// Design spec §12 AC #7: two voting engines must converge bit-identically on
/// sortition_result, drafting advancers, ratification candidate ordering, and
/// StarResult after driving the full PollCreate → kd=ss → Drafting → Ratification
/// → Close → Result lifecycle.
///
/// ## Beacon injection
///
/// Rather than running a real DKG ceremony, we inject a kd=ss SortitionSelection
/// event directly (zero-sig engine-generated shape) after computing the sortition
/// deterministically with a fixed VRF output `[0xAB; 32]`. This simulates exactly
/// what `VotingLogEngine::on_dfrost_beacon` would publish after receiving a matching
/// VRF beacon from DfrostLog.
///
/// ## HLC advancement
///
/// Windows are 60s (minimum valid). Events are assigned explicit HLC wall_ms values
/// that skip past each window boundary so we never wait real clock time.
///
/// ## sortition_size
///
/// Minimum valid per `validate_tier3_poll_config` is 20. We create 50 fixture
/// identities as the electorate (20 primary + 20 backup = 40 slots, 50 total).
#[tokio::test]
async fn tier3_full_lifecycle_4_stage_convergence() {
    const SORTITION_SIZE: u16 = 20;
    const N_IDENTITIES: usize = 50;
    const COMMUNITY_ID: SpaceId = SpaceId([0xE2; 16]);

    // ── Step 1: build electorate ──────────────────────────────────────────────

    let identities: Vec<TestIdentity> = (0..N_IDENTITIES as u8).map(fixture_identity).collect();

    // All identity owner addresses — used as the electorate snapshot at PollCreate.
    let electorate: Vec<OwnerAddr> = identities.iter().map(|id| id.owner).collect();

    // ── Step 2: set up two-engine bridge ──────────────────────────────────────

    let engines = setup_two_voting_engine_bridge(COMMUNITY_ID).await;
    // Register all identities so any of them can have their events verified
    // on the inbound path (ZEB-298+ZEB-312 PR 1: verify-then-apply).
    for id in &identities {
        engines.resolvers.add_identity(id);
    }

    // ── Step 3: PollCreate (proposer = identities[0]) ─────────────────────────

    let t0: u64 = 1_000_000; // base wall_ms
    let proposer = &identities[0];

    let config = Tier3PollConfigPayload {
        proposal_text: "E2E lifecycle test proposal".into(),
        sortition_size: SORTITION_SIZE,
        deliberation_window_seconds: 60,
        drafting_window_seconds: 60,
        ratification_window_seconds: 60,
        privacy_mode: "pu".into(),
        incentive_mode: "d".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
        predecessor: None,
        ce: None,
    };

    let create_event = build_tier3_poll_create_event(proposer, &config, hlc_at(t0, "proposer-dev"));

    // Compute poll_id and poll_create_event_hash from the create event.
    let create_signing_bytes = create_event
        .signing_bytes()
        .expect("create event signing_bytes");
    let poll_id = derive_poll_id(&COMMUNITY_ID, &create_signing_bytes);
    let _poll_create_event_hash: [u8; 32] = {
        use sha2::{Digest, Sha256};
        Sha256::digest(&create_signing_bytes).into()
    };

    // Apply PollCreate via engine_a with the full electorate snapshot.
    let snapshot = MembershipSnapshot {
        members: electorate
            .iter()
            .map(|addr| {
                (
                    *addr,
                    MemberAttrs {
                        power: 1,
                        vouching_depth: 0,
                    },
                )
            })
            .collect(),
    };
    {
        let mut log = engines.log_a.lock().await;
        log.apply_with_snapshot(create_event.clone(), &COMMUNITY_ID, Some(snapshot))
            .expect("engine_a: PollCreate apply");
    }
    // Also feed to engine_b via the bridge by publishing via engine_a.
    // But since we applied directly to log_a, we need to broadcast manually.
    // Use the engine_a's publish_event path but that would double-apply to log_a.
    // Instead: encode and send directly to engine_b's inbound via the bridge.
    //
    // Actually: re-apply directly to log_b with the same snapshot.
    let snapshot_b = MembershipSnapshot {
        members: electorate
            .iter()
            .map(|addr| {
                (
                    *addr,
                    MemberAttrs {
                        power: 1,
                        vouching_depth: 0,
                    },
                )
            })
            .collect(),
    };
    {
        let mut log = engines.log_b.lock().await;
        log.apply_with_snapshot(create_event, &COMMUNITY_ID, Some(snapshot_b))
            .expect("engine_b: PollCreate apply");
    }

    // Verify both logs have the poll in Sortition stage.
    {
        let log_a = engines.log_a.lock().await;
        let t3 = log_a
            .polls
            .get(&poll_id)
            .expect("poll in log_a")
            .tier_state
            .as_tier3()
            .expect("tier3 state in log_a");
        assert_eq!(
            t3.stage,
            Stage::Sortition,
            "log_a: should start in Sortition"
        );
        assert!(t3.sortition_result.is_none(), "log_a: no sortition yet");
        assert_eq!(
            t3.eligible_electorate_snapshot.len(),
            N_IDENTITIES,
            "log_a: full electorate snapshotted"
        );
    }
    {
        let log_b = engines.log_b.lock().await;
        let t3 = log_b
            .polls
            .get(&poll_id)
            .expect("poll in log_b")
            .tier_state
            .as_tier3()
            .expect("tier3 state in log_b");
        assert_eq!(
            t3.stage,
            Stage::Sortition,
            "log_b: should start in Sortition"
        );
    }

    // ── Step 4: inject kd=ss via engine_a (beacon simulation) ────────────────
    //
    // Fixed VRF output. Mirrors exactly what VotingLogEngine::on_dfrost_beacon
    // would publish after receiving a matching VRfBeaconPayload with
    // vrf_output = [0xAB; 32].
    let vrf_output: [u8; 32] = [0xAB; 32];
    let sortition_result = fisher_yates_select(
        &vrf_output,
        &electorate,
        SORTITION_SIZE as usize,
        SORTITION_SIZE as usize,
    );

    // ZEB-850 Task 5: seed the beacon so the peer's verify_ss admits this kd=ss.
    engines.seed_ss_beacon(poll_id, vrf_output).await;

    let ss_event = build_sortition_selection_event(
        proposer,
        poll_id,
        sortition_result.primary.clone(),
        sortition_result.backup.clone(),
        hlc_at(t0 + 1, "engine"),
    );

    // Publish via engine_a — applies to log_a and broadcasts to engine_b via bridge.
    engines
        .engine_a
        .publish_event(ss_event, None)
        .await
        .expect("engine_a: publish kd=ss");

    // Wait for engine_b to apply the kd=ss (via bridge forwarder).
    wait_for_log("engine_b: sortition_result set", &engines.log_b, |log| {
        log.polls
            .get(&poll_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .map(|t3| t3.sortition_result.is_some())
            .unwrap_or(false)
    })
    .await
    .expect("engine_b: kd=ss must arrive via bridge within 5s");

    // Assert: both engines have identical sortition_result.
    let primary_a = {
        let log = engines.log_a.lock().await;
        let t3 = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        t3.sortition_result
            .clone()
            .expect("log_a: sortition_result Some")
    };
    let primary_b = {
        let log = engines.log_b.lock().await;
        let t3 = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        t3.sortition_result
            .clone()
            .expect("log_b: sortition_result Some")
    };
    assert_eq!(
        primary_a, primary_b,
        "CONVERGENCE: both engines must have identical sortition_result"
    );
    assert_eq!(primary_a.primary.len(), SORTITION_SIZE as usize);
    assert_eq!(primary_a.backup.len(), SORTITION_SIZE as usize);

    // ── Step 5: Drafting stage — mini-public publishes kd=dc + kd=da ─────────
    //
    // Advance HLC past deliberation_window (60s = 60_000ms).
    // Drafting opens at t0 + dw_ms.
    let t_drafting = t0 + 60_001; // past deliberation window

    // The actual mini-public is the primary set (no declines in this test).
    // Find which TestIdentity objects correspond to primary OwnerAddrs.
    let primary_set: std::collections::HashSet<OwnerAddr> =
        primary_a.primary.iter().copied().collect();
    let mini_public_ids: Vec<&TestIdentity> = identities
        .iter()
        .filter(|id| primary_set.contains(&id.owner))
        .collect();
    assert_eq!(
        mini_public_ids.len(),
        SORTITION_SIZE as usize,
        "must find all primary members in identity list"
    );

    // Build 3 draft candidates from the first 3 mini-public members.
    let dc_texts = ["Option A", "Option B", "Option C"];
    let mut dc_events: Vec<SignedVotingEvent> = Vec::new();
    for (i, text) in dc_texts.iter().enumerate() {
        let actor = mini_public_ids[i];
        let dc_ev = build_draft_candidate_event(
            actor,
            poll_id,
            text,
            hlc_at(t_drafting + i as u64 * 10, &format!("member-{i}-dev")),
        );
        engines
            .engine_a
            .publish_event(dc_ev.clone(), None)
            .await
            .expect("engine_a: publish kd=dc");
        dc_events.push(dc_ev);
    }

    // Compute candidate_event_hash for each kd=dc event.
    let candidate_hashes: Vec<CandidateEventHash> = dc_events
        .iter()
        .map(|ev| {
            use sha2::{Digest, Sha256};
            Sha256::digest(ev.signing_bytes().expect("dc signing bytes")).into()
        })
        .collect();

    // All mini-public members approve each candidate (threshold = ceil(20/2) = 10).
    // We need at least 10 approvals per candidate to advance to ratification.
    // The proposer already has self-approval (1 approval each). Add 9 more per candidate.
    // Use mini_public_ids[1..10] for candidate 0, mini_public_ids[0]+[2..10] for candidate 1, etc.
    //
    // Simple approach: all 20 mini-public members approve all 3 candidates.
    for (c_idx, &candidate_hash) in candidate_hashes.iter().enumerate() {
        for (m_idx, actor) in mini_public_ids.iter().enumerate() {
            // Skip if this actor is the dc proposer (they already have self-approval,
            // but adding again is idempotent — HashSet.insert is safe).
            let da_ev = build_draft_approval_event(
                actor,
                poll_id,
                candidate_hash,
                hlc_at(
                    t_drafting + 100 + (c_idx * 100 + m_idx) as u64,
                    &format!("member-{m_idx}-dev"),
                ),
            );
            engines
                .engine_a
                .publish_event(da_ev, None)
                .await
                .expect("engine_a: publish kd=da");
        }
    }

    // Wait for engine_b to receive all drafting events.
    let expected_events_after_drafting = 1 // kd=cr
        + 1 // kd=ss
        + dc_texts.len() // kd=dc ×3
        + mini_public_ids.len() * dc_texts.len(); // kd=da ×(20*3=60)
    wait_for_log(
        "engine_b: all drafting events applied",
        &engines.log_b,
        |log| log.events.len() >= expected_events_after_drafting,
    )
    .await
    .expect("engine_b: must receive all drafting events within 5s");

    // ── Step 6: Ratification stage — full electorate casts kd=rb ──────────────
    //
    // Advance past drafting_window: t0 + dw + fw = t0 + 120_001ms.
    let t_ratification = t0 + 120_001;

    // Compute the ratification candidate ordering (same as both engines will see).
    let sq = synthesize_status_quo(&poll_id);
    let sq_hash = sq.event_hash;
    let mini_public_size = SORTITION_SIZE as usize; // 20 (no declines)
    let advancers = {
        // We need the candidates from log_a to compute advancers.
        let log = engines.log_a.lock().await;
        let t3 = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        // Build a candidates slice that includes status_quo (synthesized, not in state yet).
        let mut all_candidates = t3.candidates.clone();
        all_candidates.push(sq.clone());
        drafting_advancers(&all_candidates, mini_public_size, sq_hash)
            .expect("status_quo is in all_candidates")
    };
    assert_eq!(
        advancers.len(),
        4, // 3 real + 1 status_quo (all 3 pass threshold of 10 since all 20 approved them)
        "all 3 candidates + status_quo should advance"
    );
    let ordered_candidates = ratification_candidates_ordering(&advancers, sq_hash);
    let n_candidates = ordered_candidates.len();

    // All 50 electorate members cast ballots with simple scores.
    // Scores are indexed by position in ordered_candidates. Give candidate 0 the
    // highest score so it wins the STAR tally.
    let mut ballots: Vec<RatificationBallotPayload> = Vec::new();
    for (e_idx, actor) in identities.iter().enumerate() {
        // Score pattern: [5, 3, 2, 1] with slight variation for voter index.
        // Ensures a clear winner at position 0 without all ties.
        let scores: Vec<u8> = (0..n_candidates as u8)
            .map(|i| if i == 0 { 5 } else { 4u8.saturating_sub(i) })
            .collect();
        let rb_ev = build_ratification_ballot_event(
            actor,
            poll_id,
            scores.clone(),
            hlc_at(
                t_ratification + e_idx as u64 * 2,
                &format!("voter-{e_idx}-dev"),
            ),
        );
        ballots.push(RatificationBallotPayload {
            poll_id,
            scores: Some(scores),
            ciphertexts_scores: None,
            ciphertexts_indicators: None,
            proof: None,
        });
        engines
            .engine_a
            .publish_event(rb_ev, None)
            .await
            .expect("engine_a: publish kd=rb");
    }

    // Wait for engine_b to see all ballots.
    let expected_events_after_ratification = expected_events_after_drafting + N_IDENTITIES;
    wait_for_log(
        "engine_b: all ratification ballots applied",
        &engines.log_b,
        |log| log.events.len() >= expected_events_after_ratification,
    )
    .await
    .expect("engine_b: must receive all kd=rb events within 5s");

    // ── Step 7: Close + Result ────────────────────────────────────────────────
    //
    // Advance past ratification_window: t0 + dw + fw + rw = t0 + 180_001ms.
    let t_close = t0 + 180_001;

    let close_ev = build_poll_close_event(proposer, poll_id, hlc_at(t_close, "proposer-dev"));
    engines
        .engine_a
        .publish_event(close_ev, None)
        .await
        .expect("engine_a: publish kd=cl");

    // Compute the STAR result deterministically (same computation as SR1 verify).
    let star_result = tally_star(&ordered_candidates, &ballots);

    let result_ev = build_poll_result_event(
        proposer,
        poll_id,
        star_result.clone(),
        hlc_at(t_close + 1, "proposer-dev"),
    );
    engines
        .engine_a
        .publish_event(result_ev, None)
        .await
        .expect("engine_a: publish kd=rs");

    // Wait for engine_b to see Close + Result.
    let expected_final_events = expected_events_after_ratification + 2;
    wait_for_log(
        "engine_b: PollClose + PollResult applied",
        &engines.log_b,
        |log| log.events.len() >= expected_final_events,
    )
    .await
    .expect("engine_b: must receive kd=cl + kd=rs within 5s");

    // ── Step 8: Assert full convergence ──────────────────────────────────────

    // ZEB-469: no fixed settle delay — engine_a applied kd=cl/kd=rs synchronously
    // inside publish_event (apply-before-broadcast), and engine_b is gated by the
    // wait_for_log above, so both logs are settled here.
    let (t3_a, t3_b) = {
        let log_a = engines.log_a.lock().await;
        let log_b = engines.log_b.lock().await;
        let t3_a = log_a.polls[&poll_id]
            .tier_state
            .as_tier3()
            .expect("log_a tier3")
            .clone();
        let t3_b = log_b.polls[&poll_id]
            .tier_state
            .as_tier3()
            .expect("log_b tier3")
            .clone();
        (t3_a, t3_b)
    };

    // Both engines must be in Finalized stage.
    assert_eq!(
        t3_a.stage,
        Stage::Finalized,
        "engine_a: stage must be Finalized"
    );
    assert_eq!(
        t3_b.stage,
        Stage::Finalized,
        "engine_b: stage must be Finalized"
    );

    // Convergence: sortition_result identical.
    assert_eq!(
        t3_a.sortition_result, t3_b.sortition_result,
        "CONVERGENCE: sortition_result must be identical"
    );

    // Convergence: candidates set identical (count + hashes).
    assert_eq!(
        t3_a.candidates.len(),
        t3_b.candidates.len(),
        "CONVERGENCE: candidate count must be identical"
    );
    // Compare candidate event_hashes (order is insertion-order, same on both since events arrive in order).
    let hashes_a: Vec<_> = t3_a.candidates.iter().map(|c| c.event_hash).collect();
    let hashes_b: Vec<_> = t3_b.candidates.iter().map(|c| c.event_hash).collect();
    assert_eq!(
        hashes_a, hashes_b,
        "CONVERGENCE: candidate hashes must be identical"
    );

    // Convergence: ratification ballot count.
    assert_eq!(
        t3_a.ratification_ballots.len(),
        N_IDENTITIES,
        "engine_a: all {N_IDENTITIES} ballots applied"
    );
    assert_eq!(
        t3_b.ratification_ballots.len(),
        N_IDENTITIES,
        "engine_b: all {N_IDENTITIES} ballots applied"
    );

    // Convergence: StarResult identical.
    assert_eq!(
        t3_a.result, t3_b.result,
        "CONVERGENCE: StarResult must be identical on both engines"
    );

    // StarResult must match what we computed locally.
    let result_a = t3_a.result.as_ref().expect("engine_a: result Some");
    assert_eq!(
        result_a, &star_result,
        "engine_a: StarResult must match local tally_star computation"
    );

    // The winner should be the first ordered candidate (highest score = 5 × 50 voters).
    assert_eq!(
        result_a.winner.event_hash, ordered_candidates[0].event_hash,
        "STAR winner must be the highest-scored candidate"
    );

    // Log event count convergence.
    {
        let log_a = engines.log_a.lock().await;
        let log_b = engines.log_b.lock().await;
        assert_eq!(
            log_a.events.len(),
            log_b.events.len(),
            "CONVERGENCE: both logs must have identical event count"
        );
    }

    drop(engines);
}
/// Multi-engine decline + backup promotion test.
///
/// Design spec §3 (decline flow) + §6 (materialize on kd=md auto-promotes backup)
/// + §10 (failure modes / rejection path).
///
/// ## Scenario
///
/// 1. Two engines + 50 fixture identities + sortition_size=20.
/// 2. kd=cr + kd=ss injected (same pattern as Task 13).
/// 3. 3 primary members publish kd=md decline events.
/// 4. Both engines converge: declines.len()==3, current_mini_public = primary[remaining] + backup[0..3].
/// 5. A promoted backup member publishes kd=dc → SD1 verify accepts.
/// 6. A non-mini-public, non-backup identity tries kd=dc → SD1 verify rejects NotInMiniPublic.
/// 7. A kd=md from a non-primary member → SD1 verify rejects NotInMiniPublic.
#[tokio::test]
async fn tier3_decline_triggers_backup_promotion_across_engines() {
    const SORTITION_SIZE: u16 = 20;
    const N_IDENTITIES: usize = 50;
    const COMMUNITY_ID: SpaceId = SpaceId([0xD3; 16]);

    // ── Step 1: electorate ────────────────────────────────────────────────────

    let identities: Vec<TestIdentity> = (0..N_IDENTITIES as u8).map(fixture_identity).collect();
    let electorate: Vec<OwnerAddr> = identities.iter().map(|id| id.owner).collect();

    // ── Step 2: two-engine bridge ─────────────────────────────────────────────

    let engines = setup_two_voting_engine_bridge(COMMUNITY_ID).await;
    for id in &identities {
        engines.resolvers.add_identity(id);
    }

    // ── Step 3: PollCreate ─────────────────────────────────────────────────────

    let t0: u64 = 2_000_000;
    let proposer = &identities[0];

    let config = Tier3PollConfigPayload {
        proposal_text: "Decline + backup promotion test proposal".into(),
        sortition_size: SORTITION_SIZE,
        deliberation_window_seconds: 60,
        drafting_window_seconds: 60,
        ratification_window_seconds: 60,
        privacy_mode: "pu".into(),
        incentive_mode: "d".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
        predecessor: None,
        ce: None,
    };

    let create_event = build_tier3_poll_create_event(proposer, &config, hlc_at(t0, "proposer-dev"));

    let create_signing_bytes = create_event
        .signing_bytes()
        .expect("create event signing_bytes");
    let poll_id = derive_poll_id(&COMMUNITY_ID, &create_signing_bytes);

    let snapshot = MembershipSnapshot {
        members: electorate
            .iter()
            .map(|addr| {
                (
                    *addr,
                    MemberAttrs {
                        power: 1,
                        vouching_depth: 0,
                    },
                )
            })
            .collect(),
    };

    // Apply PollCreate to both logs directly (same pattern as Task 13 happy path).
    {
        let mut log = engines.log_a.lock().await;
        log.apply_with_snapshot(create_event.clone(), &COMMUNITY_ID, Some(snapshot.clone()))
            .expect("engine_a: PollCreate apply");
    }
    {
        let mut log = engines.log_b.lock().await;
        log.apply_with_snapshot(create_event, &COMMUNITY_ID, Some(snapshot))
            .expect("engine_b: PollCreate apply");
    }

    // ── Step 4: inject kd=ss (deterministic VRF output [0xCD; 32]) ───────────

    let vrf_output: [u8; 32] = [0xCD; 32];
    let sortition_result = fisher_yates_select(
        &vrf_output,
        &electorate,
        SORTITION_SIZE as usize,
        SORTITION_SIZE as usize,
    );

    // ZEB-850 Task 5: seed the beacon so the peer's verify_ss admits this kd=ss.
    engines.seed_ss_beacon(poll_id, vrf_output).await;

    let ss_event = build_sortition_selection_event(
        proposer,
        poll_id,
        sortition_result.primary.clone(),
        sortition_result.backup.clone(),
        hlc_at(t0 + 1, "engine"),
    );

    engines
        .engine_a
        .publish_event(ss_event, None)
        .await
        .expect("engine_a: publish kd=ss");

    // Wait for engine_b to see the kd=ss via bridge.
    wait_for_log("engine_b: sortition_result set", &engines.log_b, |log| {
        log.polls
            .get(&poll_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .map(|t3| t3.sortition_result.is_some())
            .unwrap_or(false)
    })
    .await
    .expect("engine_b: kd=ss must arrive via bridge within 5s");

    // Confirm primary + backup sizes.
    {
        let log = engines.log_a.lock().await;
        let t3 = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        let sr = t3.sortition_result.as_ref().unwrap();
        assert_eq!(sr.primary.len(), SORTITION_SIZE as usize);
        assert_eq!(sr.backup.len(), SORTITION_SIZE as usize);
    }

    // ── Step 5: 3 primary members decline ────────────────────────────────────
    //
    // We need actual primary members (from fisher_yates_select output).
    // Resolve their TestIdentity objects.
    let primary_owners: Vec<OwnerAddr> = sortition_result.primary.clone();
    let primary_ids: Vec<&TestIdentity> = primary_owners
        .iter()
        .map(|owner| {
            identities
                .iter()
                .find(|id| id.owner == *owner)
                .expect("primary member must be in identities list")
        })
        .collect();

    // Have primary_ids[0..3] decline (HLC within deliberation window).
    let t_decline_base = t0 + 100;
    for (i, decliner) in primary_ids[0..3].iter().enumerate() {
        let decline_ev = build_decline_event(
            decliner,
            poll_id,
            hlc_at(t_decline_base + i as u64 * 10, &format!("primary-{i}-dev")),
        );
        engines
            .engine_a
            .publish_event(decline_ev, None)
            .await
            .expect("engine_a: publish kd=md decline");
    }

    // Wait for engine_b to receive all 3 decline events.
    wait_for_log("engine_b: 3 declines applied", &engines.log_b, |log| {
        log.polls
            .get(&poll_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .map(|t3| t3.declines.len() >= 3)
            .unwrap_or(false)
    })
    .await
    .expect("engine_b: must receive 3 decline events within 5s");

    // ── Step 6: assert convergence on BOTH engines ────────────────────────────
    // ZEB-469: no settle sleep — log_a is synchronous (publish applies before
    // broadcast); engine_b's 3 declines are gated by the wait_for_log above.

    let t_after_declines = hlc_at(t_decline_base + 200, "observer");

    let (t3_a, t3_b) = {
        let log_a = engines.log_a.lock().await;
        let log_b = engines.log_b.lock().await;
        let t3_a = log_a.polls[&poll_id]
            .tier_state
            .as_tier3()
            .expect("engine_a tier3")
            .clone();
        let t3_b = log_b.polls[&poll_id]
            .tier_state
            .as_tier3()
            .expect("engine_b tier3")
            .clone();
        (t3_a, t3_b)
    };

    // Both engines: declines.len() == 3.
    assert_eq!(t3_a.declines.len(), 3, "engine_a: must have 3 declines");
    assert_eq!(t3_b.declines.len(), 3, "engine_b: must have 3 declines");

    // Both engines: current_mini_public at t_after_declines has same size (still 20).
    let mp_a = t3_a.current_mini_public(&t_after_declines);
    let mp_b = t3_b.current_mini_public(&t_after_declines);
    assert_eq!(
        mp_a.len(),
        SORTITION_SIZE as usize,
        "engine_a: mini-public still has 20 members (3 removed + 3 promoted)"
    );
    assert_eq!(
        mp_b.len(),
        SORTITION_SIZE as usize,
        "engine_b: mini-public still has 20 members (3 removed + 3 promoted)"
    );
    assert_eq!(
        mp_a, mp_b,
        "CONVERGENCE: current_mini_public must be identical on both engines"
    );

    // The 3 decliners must NOT be in the current mini-public.
    for decliner_id in &primary_ids[0..3] {
        assert!(
            !mp_a.contains(&decliner_id.owner),
            "decliner {:?} must not be in current_mini_public",
            decliner_id.owner
        );
    }

    // The first 3 backup members MUST now be in the current mini-public.
    let backup_owners: Vec<OwnerAddr> = sortition_result.backup.clone();
    for (i, backup_owner) in backup_owners[0..3].iter().enumerate() {
        assert!(
            mp_a.contains(backup_owner),
            "backup[{i}] ({backup_owner:?}) must be promoted into current_mini_public"
        );
    }

    // The remaining primary members (non-decliners) must still be in the set.
    for remaining_id in &primary_ids[3..] {
        assert!(
            mp_a.contains(&remaining_id.owner),
            "non-declining primary {:?} must remain in current_mini_public",
            remaining_id.owner
        );
    }

    // ── Step 7: promoted backup publishes kd=dc → SD1 verify ACCEPTS ─────────

    // Find the TestIdentity for backup[0].
    let promoted_backup_id = identities
        .iter()
        .find(|id| id.owner == backup_owners[0])
        .expect("backup[0] must be in identities list");

    let t_drafting = t0 + 60_001; // past deliberation window

    let dc_event_promoted = build_draft_candidate_event(
        promoted_backup_id,
        poll_id,
        "Draft from promoted backup member",
        hlc_at(t_drafting, "promoted-backup-dev"),
    );

    // SD1 verify on engine_a's state — promoted backup IS in current_mini_public.
    {
        let log = engines.log_a.lock().await;
        let poll_state = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        let verify_result = verify_sd(&dc_event_promoted, poll_state);
        assert_eq!(
            verify_result,
            Ok(()),
            "SD1 verify must ACCEPT kd=dc from promoted backup member"
        );
    }

    // ── Step 8: non-mini-public identity tries kd=dc → SD1 verify REJECTS ────
    //
    // We need an identity that is genuinely outside both the primary and backup
    // pools. With 50 identities and sortition_size=20, fisher_yates_select picks
    // 40 total (20 primary + 20 backup), leaving 10 outside. We scan to find one.
    let outsider_id = identities
        .iter()
        .find(|id| !primary_owners.contains(&id.owner) && !backup_owners.contains(&id.owner))
        .expect("at least one identity is outside both primary and backup pools");

    // Confirm outsider is not in current_mini_public.
    assert!(
        !mp_a.contains(&outsider_id.owner),
        "outsider must not be in current_mini_public"
    );

    let dc_event_outsider = build_draft_candidate_event(
        outsider_id,
        poll_id,
        "Draft from outsider — should be rejected",
        hlc_at(t_drafting + 10, "outsider-dev"),
    );

    {
        let log = engines.log_a.lock().await;
        let poll_state = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        let verify_result = verify_sd(&dc_event_outsider, poll_state);
        assert_eq!(
            verify_result,
            Err(VerifyError::NotInMiniPublic(outsider_id.owner)),
            "SD1 verify must REJECT kd=dc from non-mini-public outsider"
        );
    }

    // ── Step 9: kd=md from non-primary member → SD1 verify REJECTS ──────────
    //
    // The non-primary outsider tries to publish a decline event.
    // SD1 verify checks current_mini_public at event.hlc, and the outsider
    // was never in primary, so they are not in the mini-public set.
    let decline_event_outsider = build_decline_event(
        outsider_id,
        poll_id,
        hlc_at(t_decline_base + 300, "outsider-dev"),
    );

    {
        let log = engines.log_a.lock().await;
        let poll_state = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        let verify_result = verify_sd(&decline_event_outsider, poll_state);
        assert_eq!(
            verify_result,
            Err(VerifyError::NotInMiniPublic(outsider_id.owner)),
            "SD1 verify must REJECT kd=md from non-primary member"
        );
    }

    drop(engines);
}

/// Multi-engine mass-decline + SortitionFailed + retry chain test.
///
/// Design spec §3 (Failed = terminal state) + §6 (kd=sf apply rules + retry_of) +
/// §10 (failure modes + retry chain).
///
/// ## Scenario
///
/// 1. Two engines + 50 fixture identities + sortition_size=20 + proposer = identities[49].
/// 2. kd=cr + kd=ss injected (proposer is identities[49], outside primary/backup pools).
/// 3. ALL 20 primary + ALL 20 backup members (40 total) publish kd=md decline events.
/// 4. Both engines converge: declines.len()==40, decline_count_at==40.
/// 5. SF1 verify accepts: decline_count(40) >= total_pool_size(40).
/// 6. Proposer publishes kd=sf; both engines transition to Stage::Failed.
/// 7. Subsequent events for the failed poll are rejected (PollInFailedState).
/// 8. Proposer publishes new kd=cr with retry_of=Some(prev_poll_id) → accepted.
/// 9. Negative: retry_of=Some(nonexistent) → RetryOfPollNotFound.
/// 10. Negative: retry_of=Some(retry_poll_id) (non-Failed) → RetryOfPollNotFailed.
#[tokio::test]
async fn tier3_mass_decline_sortition_failed_with_retry_chain() {
    const SORTITION_SIZE: u16 = 20;
    const N_IDENTITIES: usize = 50;
    const COMMUNITY_ID: SpaceId = SpaceId([0xEF; 16]);

    // ── Step 1: electorate ────────────────────────────────────────────────────
    //
    // Proposer = identities[49], deliberately kept OUT of the sortition. Since
    // ZEB-850 Task 3's verify_ss recomputes fisher_yates_select over the poll's
    // `eligible_electorate_snapshot` and rejects a mismatch, the poll's electorate
    // must equal the pool the injected kd=ss is drawn from. So the proposer is
    // excluded from BOTH the sortition pool and the PollCreate snapshot; it still
    // mints kd=sf as the proposer (verify_sf checks actor == proposer, not
    // electorate membership).

    let identities: Vec<TestIdentity> = (0..N_IDENTITIES as u8).map(fixture_identity).collect();

    // Proposer is identities[49] — separate from the sortition pool.
    let proposer = &identities[49];

    // Sortition pool == PollCreate electorate: identities[0..49] (49 members),
    // so the proposer is never selected into the mini-public.
    let sortition_pool: Vec<OwnerAddr> = identities[..49].iter().map(|id| id.owner).collect();

    // ── Step 2: two-engine bridge ─────────────────────────────────────────────

    let engines = setup_two_voting_engine_bridge(COMMUNITY_ID).await;
    for id in &identities {
        engines.resolvers.add_identity(id);
    }

    // ── Step 3: PollCreate (proposer = identities[49]) ────────────────────────

    let t0: u64 = 3_000_000;

    let config = Tier3PollConfigPayload {
        proposal_text: "Mass-decline failure + retry chain test".into(),
        sortition_size: SORTITION_SIZE,
        deliberation_window_seconds: 3600,
        drafting_window_seconds: 3600,
        ratification_window_seconds: 3600,
        privacy_mode: "pu".into(),
        incentive_mode: "a".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
        predecessor: None,
        ce: None,
    };

    let create_event = build_tier3_poll_create_event(proposer, &config, hlc_at(t0, "proposer-dev"));

    let create_signing_bytes = create_event
        .signing_bytes()
        .expect("create event signing_bytes");
    let poll_id = derive_poll_id(&COMMUNITY_ID, &create_signing_bytes);

    // ZEB-850 Task 5 (Issue A): the poll's electorate must equal the set the
    // injected kd=ss sortition is drawn from — verify_ss recomputes
    // fisher_yates_select over `eligible_electorate_snapshot` and rejects a
    // mismatch. Exclude the proposer (never a sortition candidate) from the
    // electorate; it still mints kd=sf as the proposer (verify_sf checks
    // actor == proposer, not electorate membership).
    let snapshot = MembershipSnapshot {
        members: sortition_pool
            .iter()
            .map(|addr| {
                (
                    *addr,
                    MemberAttrs {
                        power: 1,
                        vouching_depth: 0,
                    },
                )
            })
            .collect(),
    };

    // Apply PollCreate directly to both logs with the sortition-pool snapshot.
    {
        let mut log = engines.log_a.lock().await;
        log.apply_with_snapshot(create_event.clone(), &COMMUNITY_ID, Some(snapshot.clone()))
            .expect("engine_a: PollCreate apply");
    }
    {
        let mut log = engines.log_b.lock().await;
        log.apply_with_snapshot(create_event, &COMMUNITY_ID, Some(snapshot))
            .expect("engine_b: PollCreate apply");
    }

    // Verify both logs: poll in Sortition stage, proposer = identities[49].
    {
        let log = engines.log_a.lock().await;
        let t3 = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        assert_eq!(t3.stage, Stage::Sortition, "engine_a: starts in Sortition");
        assert_eq!(
            t3.meta.proposer, proposer.owner,
            "engine_a: proposer = identities[49]"
        );
        assert_eq!(
            t3.eligible_electorate_snapshot.len(),
            sortition_pool.len(),
            "engine_a: sortition-pool electorate snapshotted (proposer excluded)"
        );
    }

    // ── Step 4: inject kd=ss (VRF seed [0xEF; 32], sortition from pool[0..49]) ─

    let vrf_output: [u8; 32] = [0xEF; 32];
    let sortition_result = fisher_yates_select(
        &vrf_output,
        &sortition_pool,
        SORTITION_SIZE as usize,
        SORTITION_SIZE as usize,
    );

    // ZEB-850 Task 5: seed the beacon so the peer's verify_ss admits this kd=ss.
    engines.seed_ss_beacon(poll_id, vrf_output).await;

    // Sanity: proposer must NOT be in primary or backup (by construction).
    assert!(
        !sortition_result.primary.contains(&proposer.owner),
        "proposer must not be in primary pool"
    );
    assert!(
        !sortition_result.backup.contains(&proposer.owner),
        "proposer must not be in backup pool"
    );

    let ss_event = build_sortition_selection_event(
        proposer,
        poll_id,
        sortition_result.primary.clone(),
        sortition_result.backup.clone(),
        hlc_at(t0 + 1, "engine"),
    );

    engines
        .engine_a
        .publish_event(ss_event, None)
        .await
        .expect("engine_a: publish kd=ss");

    // Wait for engine_b to receive kd=ss via bridge.
    wait_for_log("engine_b: sortition_result set", &engines.log_b, |log| {
        log.polls
            .get(&poll_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .map(|t3| t3.sortition_result.is_some())
            .unwrap_or(false)
    })
    .await
    .expect("engine_b: kd=ss must arrive via bridge within 5s");

    // Confirm 20 primary + 20 backup.
    {
        let log = engines.log_a.lock().await;
        let t3 = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        let sr = t3.sortition_result.as_ref().unwrap();
        assert_eq!(sr.primary.len(), SORTITION_SIZE as usize);
        assert_eq!(sr.backup.len(), SORTITION_SIZE as usize);
    }

    // ── Step 5: ALL 40 primary + backup members publish kd=md declines ────────

    let primary_owners: Vec<OwnerAddr> = sortition_result.primary.clone();
    let backup_owners: Vec<OwnerAddr> = sortition_result.backup.clone();

    // Resolve TestIdentity for each primary member.
    let primary_ids: Vec<&TestIdentity> = primary_owners
        .iter()
        .map(|owner| {
            identities
                .iter()
                .find(|id| id.owner == *owner)
                .expect("primary member must be in identities list")
        })
        .collect();

    // Resolve TestIdentity for each backup member.
    let backup_ids: Vec<&TestIdentity> = backup_owners
        .iter()
        .map(|owner| {
            identities
                .iter()
                .find(|id| id.owner == *owner)
                .expect("backup member must be in identities list")
        })
        .collect();

    let t_decline_base = t0 + 100;

    // All 20 primary members decline.
    for (i, decliner) in primary_ids.iter().enumerate() {
        let decline_ev = build_decline_event(
            decliner,
            poll_id,
            hlc_at(t_decline_base + i as u64 * 10, &format!("primary-{i}-dev")),
        );
        engines
            .engine_a
            .publish_event(decline_ev, None)
            .await
            .expect("engine_a: publish primary kd=md decline");
    }

    // All 20 backup members decline (HLC continues ascending, per-actor).
    let backup_decline_base = t_decline_base + (SORTITION_SIZE as u64) * 10 + 10;
    for (i, decliner) in backup_ids.iter().enumerate() {
        let decline_ev = build_decline_event(
            decliner,
            poll_id,
            hlc_at(
                backup_decline_base + i as u64 * 10,
                &format!("backup-{i}-dev"),
            ),
        );
        engines
            .engine_a
            .publish_event(decline_ev, None)
            .await
            .expect("engine_a: publish backup kd=md decline");
    }

    // Wait for engine_b to receive all 40 decline events via bridge.
    wait_for_log("engine_b: 40 declines applied", &engines.log_b, |log| {
        log.polls
            .get(&poll_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .map(|t3| t3.declines.len() >= 40)
            .unwrap_or(false)
    })
    .await
    .expect("engine_b: must receive all 40 decline events within 5s");

    // ── Step 6: assert convergence on both engines: declines.len()==40 ────────
    // ZEB-469: no settle sleep — log_a is synchronous; engine_b's 40 declines are
    // gated by the wait_for_log above.

    let t_after_all_declines = hlc_at(
        backup_decline_base + (SORTITION_SIZE as u64) * 10 + 1,
        "observer",
    );

    let (t3_a, t3_b) = {
        let log_a = engines.log_a.lock().await;
        let log_b = engines.log_b.lock().await;
        let t3_a = log_a.polls[&poll_id]
            .tier_state
            .as_tier3()
            .expect("engine_a tier3")
            .clone();
        let t3_b = log_b.polls[&poll_id]
            .tier_state
            .as_tier3()
            .expect("engine_b tier3")
            .clone();
        (t3_a, t3_b)
    };

    // Both engines: 40 total declines (20 primary + 20 backup).
    assert_eq!(t3_a.declines.len(), 40, "engine_a: must have 40 declines");
    assert_eq!(t3_b.declines.len(), 40, "engine_b: must have 40 declines");

    // decline_count_at returns 40 for both engines.
    let count_a = t3_a.decline_count_at(&t_after_all_declines);
    let count_b = t3_b.decline_count_at(&t_after_all_declines);
    assert_eq!(count_a, 40, "engine_a: decline_count_at must be 40");
    assert_eq!(count_b, 40, "engine_b: decline_count_at must be 40");

    // ── Step 7: SF1 verify accepts (decline_count ≥ backup_pool_size) ─────────

    let t_sf = backup_decline_base + (SORTITION_SIZE as u64) * 10 + 100;
    let sf_event = build_sortition_failed_event(proposer, poll_id, hlc_at(t_sf, "proposer-dev"));

    // verify_sf must accept: proposer is correct, decline_count(40) >= total_pool_size(40).
    {
        let log = engines.log_a.lock().await;
        let poll_state = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        let verify_result = verify_sf(&sf_event, poll_state);
        assert_eq!(
            verify_result,
            Ok(()),
            "SF1 verify must ACCEPT: decline_count=40 >= total_pool_size(primary=20+backup=20)=40"
        );
    }

    // ── Step 8: proposer publishes kd=sf; both engines transition to Failed ───

    engines
        .engine_a
        .publish_event(sf_event, None)
        .await
        .expect("engine_a: publish kd=sf SortitionFailed");

    // Wait for engine_b to apply kd=sf → Stage::Failed.
    wait_for_log("engine_b: stage == Failed", &engines.log_b, |log| {
        log.polls
            .get(&poll_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .map(|t3| t3.stage == Stage::Failed)
            .unwrap_or(false)
    })
    .await
    .expect("engine_b: stage must transition to Failed within 5s");

    // ZEB-469: no settle sleep — engine_a applied kd=sf synchronously in
    // publish_event; engine_b's Failed transition is gated by the wait_for_log above.
    // Assert both engines are in Failed stage.
    {
        let log_a = engines.log_a.lock().await;
        let log_b = engines.log_b.lock().await;
        let t3_a = log_a.polls[&poll_id].tier_state.as_tier3().unwrap();
        let t3_b = log_b.polls[&poll_id].tier_state.as_tier3().unwrap();
        assert_eq!(
            t3_a.stage,
            Stage::Failed,
            "engine_a: stage must be Failed after kd=sf"
        );
        assert_eq!(
            t3_b.stage,
            Stage::Failed,
            "engine_b: stage must be Failed after kd=sf"
        );
    }

    // ── Step 9: subsequent events on the failed poll are rejected ─────────────
    //
    // Try publishing another kd=md from a primary member. Tier3PollState::apply_event
    // returns PollInFailedState; VotingLog maps that to ApplyError::IllegalTransition.
    let extra_decline_ev =
        build_decline_event(primary_ids[0], poll_id, hlc_at(t_sf + 10, "primary-0-dev"));

    // Apply directly to log_a to check the rejection (without going through engine
    // since publish_event would propagate the error up as a string).
    {
        let mut log = engines.log_a.lock().await;
        let err = log
            .apply_with_snapshot(extra_decline_ev, &COMMUNITY_ID, None)
            .expect_err("extra kd=md after Failed must be rejected");
        assert_eq!(
            err,
            ApplyError::IllegalTransition,
            "extra kd=md after Failed must return IllegalTransition (PollInFailedState mapped)"
        );
    }

    // ── Step 10: retry chain — new kd=cr with retry_of=Some(prev_poll_id) ─────

    let t_retry = t_sf + 1000;

    let retry_config = Tier3PollConfigPayload {
        proposal_text: "Retry of mass-decline poll".into(),
        sortition_size: SORTITION_SIZE,
        deliberation_window_seconds: 3600,
        drafting_window_seconds: 3600,
        ratification_window_seconds: 3600,
        privacy_mode: "pu".into(),
        incentive_mode: "a".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: Some(poll_id),
        predecessor: None,
        ce: None,
    };

    let retry_create_event =
        build_tier3_poll_create_event(proposer, &retry_config, hlc_at(t_retry, "proposer-dev"));

    let retry_signing_bytes = retry_create_event
        .signing_bytes()
        .expect("retry create event signing_bytes");
    let retry_poll_id = derive_poll_id(&COMMUNITY_ID, &retry_signing_bytes);

    let retry_snapshot = MembershipSnapshot {
        members: sortition_pool
            .iter()
            .map(|addr| {
                (
                    *addr,
                    MemberAttrs {
                        power: 1,
                        vouching_depth: 0,
                    },
                )
            })
            .collect(),
    };

    // Apply retry kd=cr to both logs: must be accepted (predecessor is in Failed).
    {
        let mut log = engines.log_a.lock().await;
        log.apply_with_snapshot(
            retry_create_event.clone(),
            &COMMUNITY_ID,
            Some(retry_snapshot.clone()),
        )
        .expect("engine_a: retry kd=cr must be accepted (predecessor is Failed)");
    }
    {
        let mut log = engines.log_b.lock().await;
        log.apply_with_snapshot(retry_create_event, &COMMUNITY_ID, Some(retry_snapshot))
            .expect("engine_b: retry kd=cr must be accepted (predecessor is Failed)");
    }

    // Both engines must have the new poll in their state.
    {
        let log_a = engines.log_a.lock().await;
        let log_b = engines.log_b.lock().await;
        assert!(
            log_a.polls.contains_key(&retry_poll_id),
            "engine_a: retry poll must be present"
        );
        assert!(
            log_b.polls.contains_key(&retry_poll_id),
            "engine_b: retry poll must be present"
        );
        // New poll starts in Sortition stage.
        let retry_t3_a = log_a.polls[&retry_poll_id].tier_state.as_tier3().unwrap();
        let retry_t3_b = log_b.polls[&retry_poll_id].tier_state.as_tier3().unwrap();
        assert_eq!(
            retry_t3_a.stage,
            Stage::Sortition,
            "engine_a: retry poll must start in Sortition"
        );
        assert_eq!(
            retry_t3_b.stage,
            Stage::Sortition,
            "engine_b: retry poll must start in Sortition"
        );
        // New poll has a fresh poll_create_event_hash (different beacon seed).
        assert_ne!(
            retry_t3_a.meta.poll_create_event_hash, t3_a.meta.poll_create_event_hash,
            "retry poll must have a different poll_create_event_hash (fresh beacon seed)"
        );
    }

    // ── Step 11: negative — retry_of pointing to nonexistent poll → rejected ──

    let phantom_poll_id = harmony_app::community_voting_core::PollId([0xFF; 32]);
    let bad_retry_config_nonexistent = Tier3PollConfigPayload {
        proposal_text: "Bad retry — nonexistent predecessor".into(),
        sortition_size: SORTITION_SIZE,
        deliberation_window_seconds: 3600,
        drafting_window_seconds: 3600,
        ratification_window_seconds: 3600,
        privacy_mode: "pu".into(),
        incentive_mode: "a".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: Some(phantom_poll_id),
        predecessor: None,
        ce: None,
    };

    let bad_nonexistent_event = build_tier3_poll_create_event(
        proposer,
        &bad_retry_config_nonexistent,
        hlc_at(t_retry + 10, "proposer-dev"),
    );

    {
        let mut log = engines.log_a.lock().await;
        let err = log
            .apply_with_snapshot(bad_nonexistent_event, &COMMUNITY_ID, None)
            .expect_err("retry_of nonexistent poll must be rejected");
        assert_eq!(
            err,
            ApplyError::RetryOfPollNotFound,
            "retry_of nonexistent must return RetryOfPollNotFound"
        );
    }

    // ── Step 12: negative — retry_of pointing to non-Failed poll → rejected ───
    //
    // The retry poll (retry_poll_id) is in Stage::Sortition, not Failed.
    // A kd=cr with retry_of=Some(retry_poll_id) must be rejected.

    let bad_retry_config_nonfailed = Tier3PollConfigPayload {
        proposal_text: "Bad retry — predecessor is not Failed".into(),
        sortition_size: SORTITION_SIZE,
        deliberation_window_seconds: 3600,
        drafting_window_seconds: 3600,
        ratification_window_seconds: 3600,
        privacy_mode: "pu".into(),
        incentive_mode: "a".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: Some(retry_poll_id),
        predecessor: None,
        ce: None,
    };

    let bad_nonfailed_event = build_tier3_poll_create_event(
        proposer,
        &bad_retry_config_nonfailed,
        hlc_at(t_retry + 20, "proposer-dev"),
    );

    {
        let mut log = engines.log_a.lock().await;
        let err = log
            .apply_with_snapshot(bad_nonfailed_event, &COMMUNITY_ID, None)
            .expect_err("retry_of non-Failed poll must be rejected");
        assert_eq!(
            err,
            ApplyError::RetryOfPollNotFailed,
            "retry_of non-Failed must return RetryOfPollNotFailed"
        );
    }

    drop(engines);
}

#[tokio::test]
async fn smoke_tier3_event_builders_encode_without_panic() {
    let alice = fixture_identity(0xA2);
    let bob = fixture_identity(0xB2);

    let config = default_tier3_config();
    let poll_id = harmony_app::community_voting_core::PollId([0x42; 32]);

    // PollCreate
    let create_ev = build_tier3_poll_create_event(&alice, &config, hlc_at(1_000, "alice-dev"));
    assert_eq!(create_ev.tier, Tier::Sortition);
    assert_eq!(create_ev.kind, PollEventKindCode::PollCreate);
    assert_eq!(create_ev.actor, alice.owner);

    // MiniPublicDecline
    let decline_ev = build_decline_event(&bob, poll_id, hlc_at(2_000, "bob-dev"));
    assert_eq!(decline_ev.kind, PollEventKindCode::MiniPublicDecline);

    // DraftCandidate
    let draft_ev = build_draft_candidate_event(
        &bob,
        poll_id,
        "Proposal text here",
        hlc_at(3_000, "bob-dev"),
    );
    assert_eq!(draft_ev.kind, PollEventKindCode::DraftCandidate);

    // Derive candidate_event_hash from draft event signing bytes (SHA-256).
    let draft_signing_bytes = draft_ev.signing_bytes().expect("draft signing bytes");
    let candidate_event_hash: [u8; 32] = {
        use sha2::{Digest, Sha256};
        Sha256::digest(&draft_signing_bytes).into()
    };

    // DraftApproval
    let approval_ev = build_draft_approval_event(
        &alice,
        poll_id,
        candidate_event_hash,
        hlc_at(4_000, "alice-dev"),
    );
    assert_eq!(approval_ev.kind, PollEventKindCode::DraftApproval);

    // RatificationBallot
    let ballot_ev =
        build_ratification_ballot_event(&alice, poll_id, vec![5, 3, 1], hlc_at(5_000, "alice-dev"));
    assert_eq!(ballot_ev.kind, PollEventKindCode::RatificationBallot);

    // SortitionFailed
    let failed_ev = build_sortition_failed_event(&alice, poll_id, hlc_at(6_000, "alice-dev"));
    assert_eq!(failed_ev.kind, PollEventKindCode::SortitionFailed);

    // SortitionSelection (signed by alice as the proposer/orchestrator actor)
    let primary: Vec<OwnerAddr> = (0..20u8).map(|i| OwnerAddr([i; 16])).collect();
    let backup: Vec<OwnerAddr> = (20..40u8).map(|i| OwnerAddr([i; 16])).collect();
    let ss_ev =
        build_sortition_selection_event(&alice, poll_id, primary, backup, hlc_at(7_000, "engine"));
    assert_eq!(ss_ev.kind, PollEventKindCode::SortitionSelection);
    assert_eq!(ss_ev.actor, alice.owner);

    // Verify all events encode without panic (CBOR round-trip).
    for ev in &[
        &create_ev,
        &decline_ev,
        &draft_ev,
        &approval_ev,
        &ballot_ev,
        &failed_ev,
        &ss_ev,
    ] {
        let mut buf = Vec::new();
        ciborium::into_writer(ev, &mut buf).expect("CBOR encode must not panic");
        let decoded: SignedVotingEvent =
            ciborium::from_reader(&buf[..]).expect("CBOR decode must round-trip");
        assert_eq!(decoded.kind, ev.kind, "round-trip kind must match");
        assert_eq!(decoded.tier, ev.tier, "round-trip tier must match");
    }
}

/// Cross-engine kd=ss convergence: idempotent apply under Phase 4a-main zero-sig constraint.
///
/// Design spec §3 (HLC LWW for racing publishes) + §5 (SS1: bit-identical content) +
/// §10 (cross-engine convergence). Acceptance criterion #7.
///
/// ## Phase 4a-main zero-sig constraint
///
/// Both engines publish kd=ss with `actor = OwnerAddr([0; 16])` (zero-sig, engine-generated
/// shape — Task 19 will wire real signing). Because the zero actor is shared, we cannot
/// demonstrate strict per-actor HLC LWW (that requires distinct actors). Instead, we
/// demonstrate the convergence property that matters for acceptance: two engines that each
/// independently compute the same Fisher-Yates result (same VRF + same electorate) and
/// publish it at different HLCs both end with bit-identical `sortition_result` on both
/// sides after cross-engine bridge propagation.
///
/// ## Race scenario
///
/// The "race" is: engine_a publishes kd=ss at HLC 5000; engine_b independently publishes
/// kd=ss at HLC 5001. Both events are identical in content (same primary + backup arrays)
/// but differ in HLC. The materialize layer accepts both (5001 > 5000 is monotonic);
/// the second overwrite is a no-op because content is identical. Both engines converge.
///
/// ## What this proves
///
/// Even without strict HLC LWW gate (deferred to Task 19 real signing), the invariant
/// "all nodes converge on the same sortition_result for a given VRF output + electorate"
/// is proven by this test. This is the load-bearing convergence property per spec §5 SS1.
///
/// A second sub-test (`tier3_cross_engine_kd_ss_hlc_tied_resolves_by_device_id_lex`)
/// tests the tie-breaking path: same wall_ms+logical, different device_id. The lex-smaller
/// device_id event arrives first in the log; both engines see both events and converge.
#[tokio::test]
async fn tier3_kd_ss_idempotent_apply_cross_engine_convergence() {
    const SORTITION_SIZE: u16 = 20;
    const N_IDENTITIES: usize = 50;
    const COMMUNITY_ID: SpaceId = SpaceId([0xC4; 16]);

    // ── Step 1: build electorate ──────────────────────────────────────────────

    let identities: Vec<TestIdentity> = (0..N_IDENTITIES as u8).map(fixture_identity).collect();
    let electorate: Vec<OwnerAddr> = identities.iter().map(|id| id.owner).collect();

    // ── Step 2: two-engine bridge ─────────────────────────────────────────────

    let engines = setup_two_voting_engine_bridge(COMMUNITY_ID).await;
    for id in &identities {
        engines.resolvers.add_identity(id);
    }

    // ── Step 3: PollCreate (proposer = identities[0]) ─────────────────────────

    let t0: u64 = 4_000_000;
    let proposer = &identities[0];

    let config = Tier3PollConfigPayload {
        proposal_text: "Cross-engine kd=ss convergence test".into(),
        sortition_size: SORTITION_SIZE,
        deliberation_window_seconds: 60,
        drafting_window_seconds: 60,
        ratification_window_seconds: 60,
        privacy_mode: "pu".into(),
        incentive_mode: "a".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
        predecessor: None,
        ce: None,
    };

    let create_event = build_tier3_poll_create_event(proposer, &config, hlc_at(t0, "proposer-dev"));

    let create_signing_bytes = create_event
        .signing_bytes()
        .expect("create event signing_bytes");
    let poll_id = derive_poll_id(&COMMUNITY_ID, &create_signing_bytes);

    // Apply PollCreate directly to both logs with the full snapshot.
    let snapshot = MembershipSnapshot {
        members: electorate
            .iter()
            .map(|addr| {
                (
                    *addr,
                    MemberAttrs {
                        power: 1,
                        vouching_depth: 0,
                    },
                )
            })
            .collect(),
    };
    {
        let mut log = engines.log_a.lock().await;
        log.apply_with_snapshot(create_event.clone(), &COMMUNITY_ID, Some(snapshot.clone()))
            .expect("engine_a: PollCreate apply");
    }
    {
        let mut log = engines.log_b.lock().await;
        log.apply_with_snapshot(create_event, &COMMUNITY_ID, Some(snapshot))
            .expect("engine_b: PollCreate apply");
    }

    // Confirm both logs start in Sortition with no sortition_result.
    {
        let log = engines.log_a.lock().await;
        let t3 = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        assert_eq!(t3.stage, Stage::Sortition, "engine_a: starts in Sortition");
        assert!(t3.sortition_result.is_none(), "engine_a: no sortition yet");
    }
    {
        let log = engines.log_b.lock().await;
        let t3 = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        assert_eq!(t3.stage, Stage::Sortition, "engine_b: starts in Sortition");
        assert!(t3.sortition_result.is_none(), "engine_b: no sortition yet");
    }

    // ── Step 4: both engines independently compute the same sortition result ───
    //
    // Fixed VRF output [0xC4; 32]. Both engines use the same VRF output + electorate
    // → fisher_yates_select produces bit-identical primary + backup arrays.
    let vrf_output: [u8; 32] = [0xC4; 32];
    let sortition_result = fisher_yates_select(
        &vrf_output,
        &electorate,
        SORTITION_SIZE as usize,
        SORTITION_SIZE as usize,
    );

    // ZEB-850 Task 5: seed the beacon so the peer's verify_ss admits this kd=ss.
    engines.seed_ss_beacon(poll_id, vrf_output).await;

    // ── Step 5: race — engine_a publishes kd=ss at HLC 5000, engine_b at HLC 5001 ──
    //
    // Both events carry the same primary + backup arrays (bit-identical content).
    // HLCs differ: 5000 vs 5001, device_id: "engine-a" vs "engine-b".
    //
    // Phase 4a-main zero-sig constraint: both use actor=OwnerAddr([0;16]).
    // The materialize layer's HLC monotonicity check is per-poll (not per-actor);
    // 5001 > 5000 is monotonic, so both events are accepted in arrival order.
    // The second overwrite is a no-op (same content). Both engines converge.
    // HLC wall_ms must exceed t0 (4_000_000) for HLC monotonicity to pass after PollCreate.
    // Race: engine_a at t0+1, engine_b at t0+2 (lex order: a < b for same wall_ms+1 would
    // also work, but distinct wall_ms is cleaner — no ambiguity).
    let ss_event_a = build_sortition_selection_event(
        proposer,
        poll_id,
        sortition_result.primary.clone(),
        sortition_result.backup.clone(),
        hlc_at(t0 + 1, "engine-a"),
    );

    let ss_event_b = build_sortition_selection_event(
        proposer,
        poll_id,
        sortition_result.primary.clone(),
        sortition_result.backup.clone(),
        hlc_at(t0 + 2, "engine-b"),
    );

    // engine_a publishes its kd=ss (HLC t0+1) — propagates to engine_b via bridge.
    engines
        .engine_a
        .publish_event(ss_event_a, None)
        .await
        .expect("engine_a: publish kd=ss at HLC t0+1");

    // Wait for engine_b to receive engine_a's kd=ss.
    wait_for_log("engine_b: sees engine_a kd=ss", &engines.log_b, |log| {
        log.polls
            .get(&poll_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .map(|t3| t3.sortition_result.is_some())
            .unwrap_or(false)
    })
    .await
    .expect("engine_b: engine_a's kd=ss must arrive via bridge within 5s");

    // engine_b publishes its own kd=ss (HLC t0+2) — propagates to engine_a via bridge.
    // Content is identical; t0+2 > t0+1 is monotonic → materialize accepts it.
    engines
        .engine_b
        .publish_event(ss_event_b, None)
        .await
        .expect("engine_b: publish kd=ss at HLC t0+2");

    // Wait for engine_a to receive engine_b's kd=ss (a second apply of identical content).
    // We need event count on both sides to reach ≥ 2 (kd=cr + kd=ss from A + kd=ss from B).
    // The bridge relays both directions. After engine_b publishes, engine_a gets it back.
    // Note: engine_a already applied its own kd=ss; the bridge delivers engine_b's kd=ss
    // to engine_a. Both logs should have 3 events total (cr + ss_a + ss_b).
    wait_for_log(
        "engine_a: receives engine_b's kd=ss (3 total events)",
        &engines.log_a,
        |log| log.events.len() >= 3,
    )
    .await
    .expect("engine_a: must receive engine_b's kd=ss within 5s");

    wait_for_log(
        "engine_b: has both kd=ss events (3 total events)",
        &engines.log_b,
        |log| log.events.len() >= 3,
    )
    .await
    .expect("engine_b: must have 3 events (cr + ss_a + ss_b) within 5s");

    // ── Step 6: assert cross-engine convergence — sortition_result identical ───
    // ZEB-469: no settle sleep — both engines are gated by the events.len() >= 3
    // wait_for_log barriers above (which deterministically wait for the bridged
    // duplicate kd=ss); materialization is atomic under the apply lock.

    let sr_a = {
        let log = engines.log_a.lock().await;
        let t3 = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        t3.sortition_result
            .clone()
            .expect("engine_a: sortition_result must be Some")
    };

    let sr_b = {
        let log = engines.log_b.lock().await;
        let t3 = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        t3.sortition_result
            .clone()
            .expect("engine_b: sortition_result must be Some")
    };

    // CONVERGENCE: both engines must have identical sortition_result.
    assert_eq!(
        sr_a, sr_b,
        "CONVERGENCE: both engines must have identical sortition_result after cross-engine kd=ss race"
    );

    // Content must match the locally computed Fisher-Yates result.
    assert_eq!(
        sr_a.primary, sortition_result.primary,
        "engine_a: sortition_result.primary must be bit-identical to fisher_yates_select output"
    );
    assert_eq!(
        sr_a.backup, sortition_result.backup,
        "engine_a: sortition_result.backup must be bit-identical to fisher_yates_select output"
    );
    assert_eq!(sr_a.primary.len(), SORTITION_SIZE as usize);
    assert_eq!(sr_a.backup.len(), SORTITION_SIZE as usize);

    // Both engines have sortition_result set → current_stage_at a future HLC is Deliberation.
    // Note: `stage` field on Tier3PollState is NOT eagerly updated on kd=ss apply; the
    // materialize layer uses current_stage_at for HLC-watermark-driven transitions.
    // We probe current_stage_at(t0 + 10ms) which is within the deliberation window.
    {
        let log_a = engines.log_a.lock().await;
        let log_b = engines.log_b.lock().await;
        let t3_a = log_a.polls[&poll_id].tier_state.as_tier3().unwrap();
        let t3_b = log_b.polls[&poll_id].tier_state.as_tier3().unwrap();
        let probe_hlc = hlc_at(t0 + 10, "probe");
        assert_eq!(
            t3_a.current_stage_at(&probe_hlc),
            Stage::Deliberation,
            "engine_a: current_stage_at must be Deliberation after kd=ss (sortition_result set)"
        );
        assert_eq!(
            t3_b.current_stage_at(&probe_hlc),
            Stage::Deliberation,
            "engine_b: current_stage_at must be Deliberation after kd=ss (sortition_result set)"
        );
        assert_eq!(
            t3_a.current_stage_at(&probe_hlc),
            t3_b.current_stage_at(&probe_hlc),
            "CONVERGENCE: both engines in same logical stage"
        );
    }

    drop(engines);
}

/// Cross-engine kd=ss HLC tie-breaking by device_id lex order.
///
/// When two kd=ss events have equal wall_ms + logical but different device_ids,
/// the lex-smaller device_id sorts first in `(wall_ms, logical, device_id)` order.
/// The materialize HLC monotonicity gate uses this lex order. The event with the
/// lex-smaller device_id arrives at wall_ms=5000,"engine-a"; the event with device_id
/// "engine-z" arrives at wall_ms=5000,"engine-z" which is lex-larger → monotonic (a < z).
/// Both events are accepted; second overwrite is no-op (same content). Both engines converge.
///
/// This confirms the tie-breaking rule from design spec §3 (HLC lex order) works correctly
/// across engines even when wall_ms + logical are identical.
#[tokio::test]
async fn tier3_cross_engine_kd_ss_hlc_tied_resolves_by_device_id_lex() {
    const SORTITION_SIZE: u16 = 20;
    const N_IDENTITIES: usize = 50;
    const COMMUNITY_ID: SpaceId = SpaceId([0xC5; 16]);

    // ── Step 1: electorate + two-engine bridge ────────────────────────────────

    let identities: Vec<TestIdentity> = (0..N_IDENTITIES as u8).map(fixture_identity).collect();
    let electorate: Vec<OwnerAddr> = identities.iter().map(|id| id.owner).collect();
    let engines = setup_two_voting_engine_bridge(COMMUNITY_ID).await;
    for id in &identities {
        engines.resolvers.add_identity(id);
    }

    // ── Step 2: PollCreate ─────────────────────────────────────────────────────

    let t0: u64 = 5_000_000;
    let proposer = &identities[0];

    let config = Tier3PollConfigPayload {
        proposal_text: "HLC tie-breaking by device_id lex".into(),
        sortition_size: SORTITION_SIZE,
        deliberation_window_seconds: 60,
        drafting_window_seconds: 60,
        ratification_window_seconds: 60,
        privacy_mode: "pu".into(),
        incentive_mode: "a".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
        predecessor: None,
        ce: None,
    };

    let create_event = build_tier3_poll_create_event(proposer, &config, hlc_at(t0, "proposer-dev"));
    let create_signing_bytes = create_event
        .signing_bytes()
        .expect("create event signing_bytes");
    let poll_id = derive_poll_id(&COMMUNITY_ID, &create_signing_bytes);

    let snapshot = MembershipSnapshot {
        members: electorate
            .iter()
            .map(|addr| {
                (
                    *addr,
                    MemberAttrs {
                        power: 1,
                        vouching_depth: 0,
                    },
                )
            })
            .collect(),
    };
    {
        let mut log = engines.log_a.lock().await;
        log.apply_with_snapshot(create_event.clone(), &COMMUNITY_ID, Some(snapshot.clone()))
            .expect("engine_a: PollCreate apply");
    }
    {
        let mut log = engines.log_b.lock().await;
        log.apply_with_snapshot(create_event, &COMMUNITY_ID, Some(snapshot))
            .expect("engine_b: PollCreate apply");
    }

    // ── Step 3: compute sortition (same VRF + electorate → identical content) ──

    let vrf_output: [u8; 32] = [0xC5; 32];
    let sortition_result = fisher_yates_select(
        &vrf_output,
        &electorate,
        SORTITION_SIZE as usize,
        SORTITION_SIZE as usize,
    );

    // ZEB-850 Task 5: seed the beacon so the peer's verify_ss admits this kd=ss.
    engines.seed_ss_beacon(poll_id, vrf_output).await;

    // ── Step 4: tied HLC — same wall_ms + logical, different device_id ─────────
    //
    // "engine-a" < "engine-z" lexicographically. The materialize HLC monotonicity
    // gate uses (wall_ms, logical, device_id) lex order. "engine-a" event sorts first;
    // "engine-z" event (lex-larger) arrives second — monotonically valid ("a" < "z").
    // Both accepted; second overwrite is no-op (same content). Both engines converge.
    //
    // Both events use wall_ms = t0+1 (above PollCreate HLC for monotonicity).
    // As long as ss_event_a is applied before ss_event_z on each engine, the order is valid.
    // Both events share wall_ms = t0+1 (above PollCreate's t0 for HLC monotonicity)
    // but differ in device_id. "engine-a" < "engine-z" lexicographically.
    let ss_event_a = build_sortition_selection_event(
        proposer,
        poll_id,
        sortition_result.primary.clone(),
        sortition_result.backup.clone(),
        // device_id "engine-a" → lex-smaller → arrives first.
        Hlc {
            wall_ms: t0 + 1,
            logical: 0,
            device_id: "engine-a".into(),
        },
    );

    let ss_event_z = build_sortition_selection_event(
        proposer,
        poll_id,
        sortition_result.primary.clone(),
        sortition_result.backup.clone(),
        // device_id "engine-z" → lex-larger → arrives second (monotonic: "engine-a" < "engine-z").
        Hlc {
            wall_ms: t0 + 1,
            logical: 0,
            device_id: "engine-z".into(),
        },
    );

    // Publish lex-smaller event first via engine_a → bridge → engine_b.
    engines
        .engine_a
        .publish_event(ss_event_a, None)
        .await
        .expect("engine_a: publish kd=ss (device_id=engine-a)");

    // Wait for engine_b to receive it.
    wait_for_log("engine_b: sees engine-a kd=ss", &engines.log_b, |log| {
        log.polls
            .get(&poll_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .map(|t3| t3.sortition_result.is_some())
            .unwrap_or(false)
    })
    .await
    .expect("engine_b: kd=ss from engine-a must arrive within 5s");

    // Publish lex-larger event via engine_b → bridge → engine_a.
    // "engine-z" > "engine-a" → lex order is monotonically increasing → accepted.
    engines
        .engine_b
        .publish_event(ss_event_z, None)
        .await
        .expect("engine_b: publish kd=ss (device_id=engine-z)");

    // Wait for both engines to have 3 events (cr + ss_a + ss_z).
    wait_for_log(
        "engine_a: receives engine_b's kd=ss (3 events)",
        &engines.log_a,
        |log| log.events.len() >= 3,
    )
    .await
    .expect("engine_a: must receive engine_b's kd=ss within 5s");

    wait_for_log("engine_b: has all 3 events", &engines.log_b, |log| {
        log.events.len() >= 3
    })
    .await
    .expect("engine_b: must have 3 events within 5s");

    // ── Step 5: assert convergence ────────────────────────────────────────────
    // ZEB-469: no settle sleep — both engines are gated by the events.len() >= 3
    // wait_for_log barriers above; materialization is atomic under the apply lock.

    let sr_a = {
        let log = engines.log_a.lock().await;
        let t3 = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        t3.sortition_result
            .clone()
            .expect("engine_a: sortition_result Some")
    };

    let sr_b = {
        let log = engines.log_b.lock().await;
        let t3 = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        t3.sortition_result
            .clone()
            .expect("engine_b: sortition_result Some")
    };

    assert_eq!(
        sr_a, sr_b,
        "CONVERGENCE: both engines must have identical sortition_result after HLC-tied kd=ss race"
    );
    assert_eq!(
        sr_a.primary, sortition_result.primary,
        "sortition_result.primary must be bit-identical to fisher_yates_select output"
    );
    assert_eq!(
        sr_a.backup, sortition_result.backup,
        "sortition_result.backup must be bit-identical to fisher_yates_select output"
    );

    // Confirm both engines compute the same logical stage at a probe HLC.
    // `stage` field is not eagerly updated; use current_stage_at for watermark-driven stages.
    {
        let log_a = engines.log_a.lock().await;
        let log_b = engines.log_b.lock().await;
        let t3_a = log_a.polls[&poll_id].tier_state.as_tier3().unwrap();
        let t3_b = log_b.polls[&poll_id].tier_state.as_tier3().unwrap();
        let probe_hlc = hlc_at(t0 + 10, "probe");
        assert_eq!(
            t3_a.current_stage_at(&probe_hlc),
            Stage::Deliberation,
            "engine_a: current_stage_at must be Deliberation (sortition_result set)"
        );
        assert_eq!(
            t3_b.current_stage_at(&probe_hlc),
            Stage::Deliberation,
            "engine_b: current_stage_at must be Deliberation (sortition_result set)"
        );
        assert_eq!(
            t3_a.current_stage_at(&probe_hlc),
            t3_b.current_stage_at(&probe_hlc),
            "CONVERGENCE: both engines in same logical stage"
        );
    }

    drop(engines);
}

// ─── ZEB-310 Task 9: engine-auto orchestration multi-engine test ─────────────

/// Engine-auto orchestration: with the proposer's signing key installed on
/// `engine_a`, once decline_count reaches `primary_size + backup_size` the
/// engine MUST mint + publish a signed kd=sf without an IPC call, and the
/// follow-up event MUST cross the bridge so `engine_b` also transitions
/// to `Stage::Failed`.
///
/// Mirrors the structure of `tier3_mass_decline_sortition_failed_with_retry_chain`
/// but proves the orchestration hook (vs the manual `publish_event(sf_event)`
/// call there).
#[tokio::test]
async fn engine_auto_sf_on_mass_decline_from_proposer() {
    const SORTITION_SIZE: u16 = 20;
    const N_IDENTITIES: usize = 50;
    const COMMUNITY_ID: SpaceId = SpaceId([0xC1; 16]);

    let identities: Vec<TestIdentity> = (0..N_IDENTITIES as u8).map(fixture_identity).collect();
    let proposer = &identities[49];
    let sortition_pool: Vec<OwnerAddr> = identities[..49].iter().map(|id| id.owner).collect();

    // engine_a holds the proposer's signing key → orchestration hook active.
    // engine_b stays read-only (no key installed).
    let engines = setup_two_voting_engine_bridge_with_signing(COMMUNITY_ID, proposer).await;
    for id in &identities {
        engines.resolvers.add_identity(id);
    }

    let t0: u64 = 5_000_000;

    let config = Tier3PollConfigPayload {
        proposal_text: "Engine-auto kd=sf test".into(),
        sortition_size: SORTITION_SIZE,
        deliberation_window_seconds: 3600,
        drafting_window_seconds: 3600,
        ratification_window_seconds: 3600,
        privacy_mode: "pu".into(),
        incentive_mode: "a".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
        predecessor: None,
        ce: None,
    };

    let create_event = build_tier3_poll_create_event(proposer, &config, hlc_at(t0, "proposer-dev"));
    let create_signing_bytes = create_event
        .signing_bytes()
        .expect("create event signing_bytes");
    let poll_id = derive_poll_id(&COMMUNITY_ID, &create_signing_bytes);

    // ZEB-850 Task 5 (Issue A): the poll's electorate must equal the set the
    // injected kd=ss sortition is drawn from — verify_ss recomputes
    // fisher_yates_select over `eligible_electorate_snapshot` and rejects a
    // mismatch. The proposer is deliberately excluded from sortition, so exclude
    // it from the electorate too (it still mints kd=sf as the proposer;
    // verify_sf checks actor == proposer, not electorate membership).
    let snapshot = MembershipSnapshot {
        members: sortition_pool
            .iter()
            .map(|addr| {
                (
                    *addr,
                    MemberAttrs {
                        power: 1,
                        vouching_depth: 0,
                    },
                )
            })
            .collect(),
    };

    // Apply PollCreate directly to both logs with the sortition-pool electorate.
    // (We skip publish_event for the create to avoid wiring D-FROST in this
    // test; the existing kd=ss / kd=md publish path below exercises the
    // orchestration hook.)
    {
        let mut log = engines.log_a.lock().await;
        log.apply_with_snapshot(create_event.clone(), &COMMUNITY_ID, Some(snapshot.clone()))
            .expect("engine_a: PollCreate apply");
    }
    {
        let mut log = engines.log_b.lock().await;
        log.apply_with_snapshot(create_event, &COMMUNITY_ID, Some(snapshot))
            .expect("engine_b: PollCreate apply");
    }

    // Inject kd=ss: deterministic VRF on sortition_pool (excludes proposer).
    let vrf_output: [u8; 32] = [0xC1; 32];
    let sortition_result = fisher_yates_select(
        &vrf_output,
        &sortition_pool,
        SORTITION_SIZE as usize,
        SORTITION_SIZE as usize,
    );

    // ZEB-850 Task 5: seed the beacon so the peer's verify_ss admits this kd=ss.
    engines.seed_ss_beacon(poll_id, vrf_output).await;
    assert!(
        !sortition_result.primary.contains(&proposer.owner),
        "proposer must not be selected"
    );
    assert!(
        !sortition_result.backup.contains(&proposer.owner),
        "proposer must not be selected"
    );

    let ss_event = build_sortition_selection_event(
        proposer,
        poll_id,
        sortition_result.primary.clone(),
        sortition_result.backup.clone(),
        hlc_at(t0 + 1, "engine"),
    );
    engines
        .engine_a
        .publish_event(ss_event, None)
        .await
        .expect("engine_a: publish kd=ss");

    // Wait for engine_b to apply kd=ss.
    wait_for_log("engine_b: sortition_result set", &engines.log_b, |log| {
        log.polls
            .get(&poll_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .map(|t3| t3.sortition_result.is_some())
            .unwrap_or(false)
    })
    .await
    .expect("engine_b: kd=ss must arrive within 5s");

    // Publish 40 declines (20 primary + 20 backup) via engine_a. Each apply
    // calls the orchestration hook, but kd=sf should only fire once the
    // 40th decline lands (covering the full pool). Each pre-40 hook call
    // returns early because `decline_count < capacity`.
    let primary_ids: Vec<&TestIdentity> = sortition_result
        .primary
        .iter()
        .map(|owner| identities.iter().find(|id| id.owner == *owner).unwrap())
        .collect();
    let backup_ids: Vec<&TestIdentity> = sortition_result
        .backup
        .iter()
        .map(|owner| identities.iter().find(|id| id.owner == *owner).unwrap())
        .collect();

    let t_decline_base = t0 + 100;
    for (i, decliner) in primary_ids.iter().enumerate() {
        let decline_ev = build_decline_event(
            decliner,
            poll_id,
            hlc_at(t_decline_base + i as u64 * 10, &format!("primary-{i}-dev")),
        );
        engines
            .engine_a
            .publish_event(decline_ev, None)
            .await
            .expect("publish primary decline");
    }
    let backup_decline_base = t_decline_base + (SORTITION_SIZE as u64) * 10 + 10;
    for (i, decliner) in backup_ids.iter().enumerate() {
        let decline_ev = build_decline_event(
            decliner,
            poll_id,
            hlc_at(
                backup_decline_base + i as u64 * 10,
                &format!("backup-{i}-dev"),
            ),
        );
        engines
            .engine_a
            .publish_event(decline_ev, None)
            .await
            .expect("publish backup decline");
    }

    // Engine-auto orchestration should now have minted kd=sf on engine_a's
    // apply hook (40th decline). Wait for engine_b to receive + apply it
    // via the bridge.
    wait_for_log(
        "engine_b: stage == Failed (engine-auto kd=sf propagated)",
        &engines.log_b,
        |log| {
            log.polls
                .get(&poll_id)
                .and_then(|ps| ps.tier_state.as_tier3())
                .map(|t3| t3.stage == Stage::Failed)
                .unwrap_or(false)
        },
    )
    .await
    .expect("engine-auto kd=sf must propagate to engine_b within 5s");

    // Confirm engine_a also transitioned (the hook recurses through
    // publish_event which applies locally before broadcasting).
    {
        let log = engines.log_a.lock().await;
        let t3 = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        assert_eq!(
            t3.stage,
            Stage::Failed,
            "engine_a: must be Failed after engine-auto kd=sf"
        );
        // The kd=sf was signed by the proposer's installed key (not a
        // zero-actor synthetic). Confirm via the SF1 verify path —
        // peers will accept this event because the actor matches the
        // poll proposer.
        assert!(
            t3.declines.len() >= 40,
            "engine_a: expected at least 40 declines on the poll"
        );
    }

    drop(engines);
}

/// ZEB-310 Task 10: engine-auto orchestration of kd=cl PollClose.
///
/// Verifies the post-apply hook on `VotingLogEngine` mints + publishes a
/// signed kd=cl when:
///
///   (1) the poll is in `Stage::Ratification` per `current_stage_at(now_hlc)`
///       — i.e. sortition_result is Some AND now ≥ create + dw + fw,
///   (2) `close_event_hash.is_none()` (no prior kd=cl applied), AND
///   (3) the ratification window has expired (now ≥ create + dw + fw + rw).
///
/// Setup: build a Tier 3 poll with the minimum-valid 60-second windows.
/// Inject kd=ss with `hlc.wall_ms = t0 + 500_000` (320s past the ratification
/// deadline) so the hook's HLC-driven "now" view sees the deadline as
/// expired. engine_a.publish_event(kd=ss) applies kd=ss locally, fires the
/// post-apply hook, which mints + broadcasts kd=cl.
///
/// The plan's "≥1 ballot" guideline is satisfied implicitly: with 0 ballots
/// + 1 candidate (status_quo, always synthesized), `tally_star` still
/// produces a deterministic result on the kd=rs leg (Task 11). For kd=cl
/// alone, no ballots are required — only the window-expiry guards above.
#[tokio::test]
async fn engine_auto_cl_when_ratification_window_expires() {
    const COMMUNITY_ID: SpaceId = SpaceId([0xC2; 16]);
    const SORTITION_SIZE: u16 = 20;
    const N_IDENTITIES: usize = 50;

    let identities: Vec<TestIdentity> = (0..N_IDENTITIES as u8).map(fixture_identity).collect();
    let proposer = &identities[49];
    let sortition_pool: Vec<OwnerAddr> = identities[..49].iter().map(|id| id.owner).collect();

    // engine_a holds the proposer's signing key → orchestration hook active.
    let engines = setup_two_voting_engine_bridge_with_signing(COMMUNITY_ID, proposer).await;
    for id in &identities {
        engines.resolvers.add_identity(id);
    }

    // Synthetic t0 — the kd=cl trigger is HLC-driven (uses
    // `t3.last_hlc.wall_ms`, NOT wall-clock — see hook comments in
    // community_voting_log_engine.rs). The kd=ss event below is minted
    // with `hlc.wall_ms = t0 + 500_000`, well past the ratification
    // deadline (`t0 + 180_000`).
    let t0: u64 = 6_000_000;

    let config = Tier3PollConfigPayload {
        proposal_text: "Engine-auto kd=cl test".into(),
        sortition_size: SORTITION_SIZE,
        // Minimum-valid windows per validate_tier3_poll_config (60s floor).
        // total_window_ms = 180_000 — well under the t0 offset above.
        deliberation_window_seconds: 60,
        drafting_window_seconds: 60,
        ratification_window_seconds: 60,
        privacy_mode: "pu".into(),
        incentive_mode: "a".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
        predecessor: None,
        ce: None,
    };

    let create_event = build_tier3_poll_create_event(proposer, &config, hlc_at(t0, "proposer-dev"));
    let create_signing_bytes = create_event
        .signing_bytes()
        .expect("create event signing_bytes");
    let poll_id = derive_poll_id(&COMMUNITY_ID, &create_signing_bytes);

    let snapshot = MembershipSnapshot {
        members: sortition_pool
            .iter()
            .map(|addr| {
                (
                    *addr,
                    MemberAttrs {
                        power: 1,
                        vouching_depth: 0,
                    },
                )
            })
            .collect(),
    };

    // Apply PollCreate directly to both logs (skip publish_event to avoid
    // D-FROST wiring requirement; the kd=ss publish below exercises the hook).
    {
        let mut log = engines.log_a.lock().await;
        log.apply_with_snapshot(create_event.clone(), &COMMUNITY_ID, Some(snapshot.clone()))
            .expect("engine_a: PollCreate apply");
    }
    {
        let mut log = engines.log_b.lock().await;
        log.apply_with_snapshot(create_event, &COMMUNITY_ID, Some(snapshot))
            .expect("engine_b: PollCreate apply");
    }

    // Inject kd=ss via engine_a.publish_event. This applies kd=ss locally
    // (setting sortition_result), then fires the engine-auto hook. The hook
    // is HLC-driven (uses `t3.last_hlc.wall_ms` as the "now" estimate, NOT
    // real wall-clock — see community_voting_log_engine.rs comments). So
    // the kd=ss HLC must already be past the ratification deadline
    // (t0 + total_window = t0 + 180_000 ms) for current_stage_at to return
    // Ratification AND for the deadline check to fire. We set
    // `ss_hlc.wall_ms = t0 + 500_000` so the deadline is 320_000 ms past
    // — well clear of any ordering subtleties.
    let vrf_output: [u8; 32] = [0xC2; 32];
    let sortition_result = fisher_yates_select(
        &vrf_output,
        &sortition_pool,
        SORTITION_SIZE as usize,
        SORTITION_SIZE as usize,
    );

    // ZEB-850 Task 5: seed the beacon so the peer's verify_ss admits this kd=ss.
    engines.seed_ss_beacon(poll_id, vrf_output).await;
    let ss_hlc_wall = t0 + 500_000;
    let ss_event = build_sortition_selection_event(
        proposer,
        poll_id,
        sortition_result.primary.clone(),
        sortition_result.backup.clone(),
        hlc_at(ss_hlc_wall, "engine"),
    );
    engines
        .engine_a
        .publish_event(ss_event, None)
        .await
        .expect("engine_a: publish kd=ss");

    // Wait for engine_b to see close_event_hash become Some, which means
    // the engine-auto kd=cl crossed the bridge.
    wait_for_log(
        "engine_b: close_event_hash set (engine-auto kd=cl propagated)",
        &engines.log_b,
        |log| {
            log.polls
                .get(&poll_id)
                .and_then(|ps| ps.tier_state.as_tier3())
                .map(|t3| t3.close_event_hash.is_some())
                .unwrap_or(false)
        },
    )
    .await
    .expect("engine-auto kd=cl must propagate to engine_b within 5s");

    // engine_a applied kd=cl locally as part of the recursive publish_event.
    {
        let log = engines.log_a.lock().await;
        let t3 = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        assert!(
            t3.close_event_hash.is_some(),
            "engine_a: close_event_hash must be set after engine-auto kd=cl"
        );
    }

    // ZEB-850 peer verify_ss admission (CodeAnt Major): engine_b must have
    // ADMITTED the bridged kd=ss (sortition_result set), not merely applied the
    // ungated kd=cl. With the snapshot built from `sortition_pool`, verify_ss
    // recomputes the same selection and admits it; with the prior 50-member
    // `full_electorate` snapshot it rejected as SortitionMismatch and this
    // assertion fails, proving the peer sortition-admission cascade runs.
    {
        let log = engines.log_b.lock().await;
        let t3 = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        assert!(
            t3.sortition_result.is_some(),
            "engine_b: must admit peer kd=ss via verify_ss (sortition_result set)"
        );
    }

    drop(engines);
}

/// ZEB-310 Task 11: engine-auto orchestration of kd=rs PollResult.
///
/// Verifies that after the kd=cl trigger fires (per Task 10), the same
/// post-apply hook re-fires from inside the recursive `publish_event`
/// call and mints + publishes a signed kd=rs with a deterministic STAR
/// tally. Both engines must converge on:
///
///   1. `Stage::Finalized` (kd=rs applied),
///   2. bit-identical `Tier3PollResultPayload.winner.event_hash`.
///
/// Setup is the same as `engine_auto_cl_when_ratification_window_expires`
/// (Task 10) — minimum-valid 60-second windows, synthetic t0, kd=ss
/// minted with hlc.wall_ms = t0 + 500_000 (320s past the ratification
/// deadline). With 0 ratification ballots and 1 candidate (status_quo,
/// always synthesized by `synthesize_status_quo`), `tally_star` returns
/// a deterministic result: status_quo wins the (degenerate) STAR runoff.
#[tokio::test]
async fn engine_auto_rs_after_cl_with_bit_identical_tally() {
    use harmony_app::community_voting_tier3::synthesize_status_quo;

    const COMMUNITY_ID: SpaceId = SpaceId([0xC3; 16]);
    const SORTITION_SIZE: u16 = 20;
    const N_IDENTITIES: usize = 50;

    let identities: Vec<TestIdentity> = (0..N_IDENTITIES as u8).map(fixture_identity).collect();
    let proposer = &identities[49];
    let sortition_pool: Vec<OwnerAddr> = identities[..49].iter().map(|id| id.owner).collect();

    let engines = setup_two_voting_engine_bridge_with_signing(COMMUNITY_ID, proposer).await;
    for id in &identities {
        engines.resolvers.add_identity(id);
    }

    let t0: u64 = 6_000_000;

    let config = Tier3PollConfigPayload {
        proposal_text: "Engine-auto kd=rs test".into(),
        sortition_size: SORTITION_SIZE,
        deliberation_window_seconds: 60,
        drafting_window_seconds: 60,
        ratification_window_seconds: 60,
        privacy_mode: "pu".into(),
        incentive_mode: "a".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
        predecessor: None,
        ce: None,
    };

    let create_event = build_tier3_poll_create_event(proposer, &config, hlc_at(t0, "proposer-dev"));
    let create_signing_bytes = create_event
        .signing_bytes()
        .expect("create event signing_bytes");
    let poll_id = derive_poll_id(&COMMUNITY_ID, &create_signing_bytes);

    let snapshot = MembershipSnapshot {
        members: sortition_pool
            .iter()
            .map(|addr| {
                (
                    *addr,
                    MemberAttrs {
                        power: 1,
                        vouching_depth: 0,
                    },
                )
            })
            .collect(),
    };

    {
        let mut log = engines.log_a.lock().await;
        log.apply_with_snapshot(create_event.clone(), &COMMUNITY_ID, Some(snapshot.clone()))
            .expect("engine_a: PollCreate apply");
    }
    {
        let mut log = engines.log_b.lock().await;
        log.apply_with_snapshot(create_event, &COMMUNITY_ID, Some(snapshot))
            .expect("engine_b: PollCreate apply");
    }

    let vrf_output: [u8; 32] = [0xC3; 32];
    let sortition_result = fisher_yates_select(
        &vrf_output,
        &sortition_pool,
        SORTITION_SIZE as usize,
        SORTITION_SIZE as usize,
    );

    // ZEB-850 Task 5: seed the beacon so the peer's verify_ss admits this kd=ss.
    engines.seed_ss_beacon(poll_id, vrf_output).await;
    let ss_hlc_wall = t0 + 500_000;
    let ss_event = build_sortition_selection_event(
        proposer,
        poll_id,
        sortition_result.primary.clone(),
        sortition_result.backup.clone(),
        hlc_at(ss_hlc_wall, "engine"),
    );
    engines
        .engine_a
        .publish_event(ss_event, None)
        .await
        .expect("engine_a: publish kd=ss");

    // Wait for engine_b to see Stage::Finalized — the recursive hook on
    // engine_a fired kd=cl → applied → re-fired hook → fired kd=rs →
    // applied → Stage = Finalized. Both events broadcast across the bridge
    // in apply order.
    wait_for_log(
        "engine_b: stage == Finalized (engine-auto kd=rs propagated)",
        &engines.log_b,
        |log| {
            log.polls
                .get(&poll_id)
                .and_then(|ps| ps.tier_state.as_tier3())
                .map(|t3| t3.stage == Stage::Finalized)
                .unwrap_or(false)
        },
    )
    .await
    .expect("engine-auto kd=rs must propagate to engine_b within 5s");

    // Bit-identical convergence: same StarResult on both engines.
    let (t3_a, t3_b) = {
        let log_a = engines.log_a.lock().await;
        let log_b = engines.log_b.lock().await;
        (
            log_a.polls[&poll_id]
                .tier_state
                .as_tier3()
                .expect("log_a tier3")
                .clone(),
            log_b.polls[&poll_id]
                .tier_state
                .as_tier3()
                .expect("log_b tier3")
                .clone(),
        )
    };
    assert_eq!(t3_a.stage, Stage::Finalized);
    assert_eq!(t3_b.stage, Stage::Finalized);
    // ZEB-850 peer verify_ss admission (CodeAnt Major): engine_b must ADMIT the
    // bridged kd=ss (sortition_result set) rather than only replay the ungated
    // kd=cl/kd=rs. Aligning the snapshot with `sortition_pool` makes verify_ss
    // recompute the same selection; the prior 50-member snapshot rejected it.
    assert!(
        t3_a.sortition_result.is_some(),
        "engine_a: sortition_result must be Some"
    );
    assert!(
        t3_b.sortition_result.is_some(),
        "engine_b: must admit peer kd=ss via verify_ss (sortition_result set)"
    );
    assert!(
        t3_a.close_event_hash.is_some(),
        "engine_a: close_event_hash must be Some"
    );
    assert!(
        t3_b.close_event_hash.is_some(),
        "engine_b: close_event_hash must be Some"
    );
    let result_a = t3_a.result.as_ref().expect("engine_a result");
    let result_b = t3_b.result.as_ref().expect("engine_b result");
    assert_eq!(
        result_a, result_b,
        "CONVERGENCE: both engines must have bit-identical StarResult"
    );
    // With 0 ballots + 1 candidate (status_quo), the STAR runoff has a
    // single finalist — status_quo — which trivially wins.
    let sq_hash = synthesize_status_quo(&poll_id).event_hash;
    assert_eq!(
        result_a.winner.event_hash, sq_hash,
        "winner must be status_quo in the degenerate 0-ballot path"
    );

    drop(engines);
}

/// ZEB-310 Task 12: verify the engine's post-apply hook emits the Tier 3
/// lifecycle Tauri events at the expected lifecycle points.
///
/// Scenario: drive a Tier 3 poll through the kd=ss → engine-auto kd=cl →
/// engine-auto kd=rs cascade (same shape as `engine_auto_rs_after_cl…`
/// above) and assert:
///
///   - `voting-tier3-sortition-complete` fires exactly once (kd=ss apply)
///   - `voting-tier3-finalized` fires exactly once (kd=rs apply, cascaded
///      from inside the recursive `publish_event` from the engine-auto hook)
///
/// `voting-tier3-drafting-open` and `voting-tier3-ratification-open` are NOT
/// asserted in this scenario — the hook uses `current_hlc_estimate()`
/// (real wall-clock) for the previous/new stage diff, and the synthetic
/// `t0 = 6_000_000ms` poll create HLC is decades behind real wall-clock,
/// so `current_stage_at(now)` collapses straight to Ratification (skipping
/// the Deliberation and Drafting transitions). The IPC-emitted version of
/// these two events (Tasks 7 / 8 of the broader plan) covers the real
/// transition path explicitly. Here we cover the always-fires-on-kind
/// events (sortition-complete + finalized).
#[tokio::test]
async fn engine_auto_tier3_tauri_events_fire_at_expected_lifecycle_points() {
    use std::sync::Mutex as StdMutex;

    const COMMUNITY_ID: SpaceId = SpaceId([0xC4; 16]);
    const SORTITION_SIZE: u16 = 20;
    const N_IDENTITIES: usize = 50;

    let identities: Vec<TestIdentity> = (0..N_IDENTITIES as u8).map(fixture_identity).collect();
    let proposer = &identities[49];
    let sortition_pool: Vec<OwnerAddr> = identities[..49].iter().map(|id| id.owner).collect();

    // Mock Tauri app + handle, plus listeners for the two events we assert on.
    // Listeners must be registered BEFORE the events fire — the engine's
    // emit happens inside `publish_event`, and Tauri's bus delivers
    // asynchronously to listeners attached at emit time only if the
    // listener was registered prior.
    let app = tauri::test::mock_app();
    let app_handle = app.handle().clone();

    let sortition_complete_payloads: Arc<StdMutex<Vec<String>>> =
        Arc::new(StdMutex::new(Vec::new()));
    let finalized_payloads: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));

    {
        let sink = Arc::clone(&sortition_complete_payloads);
        app_handle.listen("voting-tier3-sortition-complete", move |evt| {
            sink.lock()
                .expect("sortition_complete sink")
                .push(evt.payload().to_string());
        });
    }
    {
        let sink = Arc::clone(&finalized_payloads);
        app_handle.listen("voting-tier3-finalized", move |evt| {
            sink.lock()
                .expect("finalized sink")
                .push(evt.payload().to_string());
        });
    }

    let engines = setup_two_voting_engine_bridge_with_signing_and_app(
        COMMUNITY_ID,
        proposer,
        app_handle.clone(),
    )
    .await;
    for id in &identities {
        engines.resolvers.add_identity(id);
    }

    let t0: u64 = 6_000_000;
    let config = Tier3PollConfigPayload {
        proposal_text: "Engine-auto Tauri events test".into(),
        sortition_size: SORTITION_SIZE,
        deliberation_window_seconds: 60,
        drafting_window_seconds: 60,
        ratification_window_seconds: 60,
        privacy_mode: "pu".into(),
        incentive_mode: "a".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
        predecessor: None,
        ce: None,
    };

    let create_event = build_tier3_poll_create_event(proposer, &config, hlc_at(t0, "proposer-dev"));
    let create_signing_bytes = create_event
        .signing_bytes()
        .expect("create event signing_bytes");
    let poll_id = derive_poll_id(&COMMUNITY_ID, &create_signing_bytes);

    let snapshot = MembershipSnapshot {
        members: sortition_pool
            .iter()
            .map(|addr| {
                (
                    *addr,
                    MemberAttrs {
                        power: 1,
                        vouching_depth: 0,
                    },
                )
            })
            .collect(),
    };

    // Apply PollCreate directly (no Tauri events emitted by the engine for
    // this event — `voting-tier3-poll-created` is emitted by the IPC, not
    // the engine).
    {
        let mut log = engines.log_a.lock().await;
        log.apply_with_snapshot(create_event.clone(), &COMMUNITY_ID, Some(snapshot.clone()))
            .expect("engine_a: PollCreate apply");
    }
    {
        let mut log = engines.log_b.lock().await;
        log.apply_with_snapshot(create_event, &COMMUNITY_ID, Some(snapshot))
            .expect("engine_b: PollCreate apply");
    }

    // Publish kd=ss via engine_a. This:
    //   1. emits `voting-tier3-sortition-complete` (kd=ss kind match), then
    //   2. fires engine-auto orchestration → kd=cl recursive publish → kd=rs
    //      recursive publish → poll Finalized → `voting-tier3-finalized`
    //      emitted from the kd=rs inner publish_event's post-apply hook.
    let vrf_output: [u8; 32] = [0xC4; 32];
    let sortition_result = fisher_yates_select(
        &vrf_output,
        &sortition_pool,
        SORTITION_SIZE as usize,
        SORTITION_SIZE as usize,
    );

    // ZEB-850 Task 5: seed the beacon so the peer's verify_ss admits this kd=ss.
    engines.seed_ss_beacon(poll_id, vrf_output).await;
    let ss_hlc_wall = t0 + 500_000;
    let ss_event = build_sortition_selection_event(
        proposer,
        poll_id,
        sortition_result.primary.clone(),
        sortition_result.backup.clone(),
        hlc_at(ss_hlc_wall, "engine"),
    );
    engines
        .engine_a
        .publish_event(ss_event, None)
        .await
        .expect("engine_a: publish kd=ss");

    // Wait for engine_a to reach Stage::Finalized so we know the full
    // cascade has applied + both events have been emitted onto the Tauri
    // bus. The bus delivery is async; we then poll the listener sinks.
    wait_for_log(
        "engine_a: stage == Finalized (kd=ss → kd=cl → kd=rs cascade)",
        &engines.log_a,
        |log| {
            log.polls
                .get(&poll_id)
                .and_then(|ps| ps.tier_state.as_tier3())
                .map(|t3| t3.stage == Stage::Finalized)
                .unwrap_or(false)
        },
    )
    .await
    .expect("engine_a: full Finalized cascade must complete within 5s");

    // Poll until BOTH events have been delivered (Tauri bus has its own
    // dispatch latency). Bounded at 2s.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let sc_count = sortition_complete_payloads.lock().expect("sc lock").len();
        let fin_count = finalized_payloads.lock().expect("fin lock").len();
        if sc_count >= 1 && fin_count >= 1 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "Tauri events did not deliver in time: sortition-complete={sc_count}, \
                 finalized={fin_count} (each must be >= 1)"
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Exact-once assertions: each event fired exactly once on engine_a.
    let sc_payloads = sortition_complete_payloads
        .lock()
        .expect("sc lock final")
        .clone();
    let fin_payloads = finalized_payloads.lock().expect("fin lock final").clone();
    assert_eq!(
        sc_payloads.len(),
        1,
        "voting-tier3-sortition-complete must fire exactly once; got {} payloads: {:?}",
        sc_payloads.len(),
        sc_payloads
    );
    assert_eq!(
        fin_payloads.len(),
        1,
        "voting-tier3-finalized must fire exactly once; got {} payloads: {:?}",
        fin_payloads.len(),
        fin_payloads
    );

    // Spot-check payload shape: poll_id field should contain our hex.
    let poll_id_hex = hex::encode(poll_id.0);
    assert!(
        sc_payloads[0].contains(&poll_id_hex),
        "sortition-complete payload missing pollId hex: {}",
        sc_payloads[0]
    );
    assert!(
        fin_payloads[0].contains(&poll_id_hex),
        "finalized payload missing pollId hex: {}",
        fin_payloads[0]
    );

    drop(engines);
}
