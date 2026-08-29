//! ZEB-310 Phase 4a-main Task 15: IPC-driven Tier 3 lifecycle integration
//! tests. Verifies that the 6 Tier 3 IPC handlers + engine-auto orchestration
//! produce the expected Tier 3 lifecycle behavior across two engines.
//!
//! ## Path choice
//!
//! Tests 1-4 use **Path C** (engine-layer invocation), and Test 5 uses
//! **Path A** (full `tauri::test::get_ipc_response` invocation with
//! `tauri::test::mock_app`).
//!
//! ### Rationale
//!
//! - Path A (full Tauri IPC) requires the IPC handlers to find a fully
//!   wired `NodeState` (community_registry + crdt_state + dm_outbox with
//!   real signing key + channel_log_registry). Wiring all of that for
//!   Ok-path lifecycle tests would dwarf the test itself and reproduce
//!   the same convergence the engine-layer tests already cover. See
//!   `community_dfrost_ipc_integration.rs` for the same scoping decision
//!   on the dfrost side.
//! - Path C exercises the same code path as the IPCs:
//!   `build_signed_*_tier3` builders → `VotingLog::apply_with_snapshot`
//!   (which the IPCs call directly). The only thing skipped is the
//!   hex-encode/decode boundary, which Test 5 covers explicitly via
//!   Path A AND which the JS vitest tests in Task 14 also cover at the
//!   adapter boundary.
//! - For Test 5 (error extraction), Path A is the right boundary —
//!   `dm_ipc_roundtrip.rs` establishes the pattern.
//!
//! ## Fixture duplication
//!
//! Each `tests/*.rs` integration test file is its own crate, so we
//! cannot directly `use` fixture functions from
//! `community_voting_tier3_integration.rs`. We duplicate the minimal
//! fixture surface here. The canonical source remains the existing
//! file.

#![cfg(feature = "test-fixtures")]

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use harmony_app::community_state_sync::IdentityResolver;
use harmony_app::community_voting_core::{
    build_signed_draft_approval, build_signed_draft_candidate, build_signed_mini_public_decline,
    build_signed_poll_close_tier3, build_signed_poll_create_tier3,
    build_signed_ratification_ballot, build_signed_ratification_ballot_payload,
    build_signed_sortition_selection, build_signed_tally_share, derive_poll_id, BallotNIZKProof,
    CandidateEventHash, Eligibility, MemberAttrs, MembershipSnapshot, PollId,
    RatificationBallotPayload, SignedVotingEvent, TallyShareEntry, TallySharePayload,
    Tier3PollConfigPayload, VotingIdentityResolver,
};
use harmony_app::community_voting_log::{
    MembershipSnapshotResolver, SnapshotResolverError, VotingLog,
};
use harmony_app::community_voting_log_engine::{VotingLogEngine, VotingLogEngineParams};
use harmony_app::community_voting_sortition::fisher_yates_select;
use harmony_app::community_voting_tier3::{
    aggregate_se_ballots, CommitteeOracle, CommitteePublicState, DraftCandidateState, Stage,
};
use harmony_app::community_voting_tier3_crypto::{
    compress_point, decompress_point, partial_decrypt_share,
};
use harmony_app::community_voting_tier3_nizk::{
    dleq_prove, prove_ballot_bundle_with_outputs_with_score_nonces,
};
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

use curve25519_dalek::{
    constants::RISTRETTO_BASEPOINT_POINT, ristretto::RistrettoPoint, scalar::Scalar,
};
use frost_ristretto255::rand_core::OsRng;
use harmony_app::{add_dm_ipc_handlers, mock_context_with_full_acl, NodeState, LOCAL_IPC_URL};
use std::collections::BTreeMap;
use std::collections::HashMap;
use tauri::ipc::{CallbackFn, InvokeBody};
use tauri::test::{get_ipc_response, mock_builder, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::WebviewWindowBuilder;
use tokio::sync::{mpsc, Mutex};

// ─── Identity resolver helper ─────────────────────────────────────────────────

#[allow(dead_code)]
struct StaticIdentityResolver(HashMap<OwnerAddr, [u8; 64]>);

#[async_trait::async_trait]
impl IdentityResolver for StaticIdentityResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        self.0.get(addr).copied()
    }
}

// ─── Test identity ────────────────────────────────────────────────────────────

/// Minimal duplicate of `TestIdentity` from
/// `community_voting_tier3_integration.rs`. See that file for the canonical
/// shape; this duplication is necessary because integration tests are
/// separate Cargo crates.
pub struct TestIdentity {
    pub owner: OwnerAddr,
    pub signing_key: ed25519_dalek::SigningKey,
    #[allow(dead_code)]
    pub verifying_key: ed25519_dalek::VerifyingKey,
    #[allow(dead_code)]
    pub pub_64: [u8; 64],
}

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

// ─── HLC helper ───────────────────────────────────────────────────────────────

fn hlc_at(wall_ms: u64, device_id: &str) -> Hlc {
    Hlc {
        wall_ms,
        logical: 0,
        device_id: device_id.into(),
    }
}

// ─── BridgeTestResolvers ──────────────────────────────────────────────────────

/// Mutable resolver for bridge tests. See `community_voting_tier3_integration.rs`
/// for the canonical implementation; this is a local duplicate required because
/// each integration test file is its own Cargo crate.
pub struct BridgeTestResolvers {
    identity: std::sync::RwLock<HashMap<OwnerAddr, [u8; 64]>>,
    snapshot: std::sync::RwLock<MembershipSnapshot>,
}

impl BridgeTestResolvers {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            identity: std::sync::RwLock::new(HashMap::new()),
            snapshot: std::sync::RwLock::new(MembershipSnapshot {
                members: HashMap::new(),
            }),
        })
    }

    fn add_identity(&self, id: &TestIdentity) {
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
        _community_id: SpaceId,
        _hlc: &Hlc,
    ) -> Result<MembershipSnapshot, SnapshotResolverError> {
        Ok(self.snapshot.read().unwrap().clone())
    }
}

// ─── Two-engine bridge ────────────────────────────────────────────────────────

pub struct TwoVotingEngines {
    pub engine_a: Arc<VotingLogEngine<tauri::test::MockRuntime>>,
    pub engine_b: Arc<VotingLogEngine<tauri::test::MockRuntime>>,
    pub log_a: Arc<Mutex<VotingLog>>,
    pub log_b: Arc<Mutex<VotingLog>>,
    pub resolvers: Arc<BridgeTestResolvers>,
    /// ZEB-731: the shared per-device HLC trackers handed to each engine.
    /// Exposed so tests can assert that engine-auto kd=rs mints do NOT bump
    /// the device lane (the poll-derived-lane fix keeps a future-walled poll
    /// watermark out of the device's global outbound lane).
    pub a_hlc_tracker: Arc<Mutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>>,
    pub b_hlc_tracker: Arc<Mutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>>,
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
/// Returns the engine's `DfrostLog` so `seed_ss_beacon` can insert VRF outputs
/// straight into `beacon_index`. The no-op requester never issues a real
/// request and no `VrfBeacon` event is ever applied, so the engine's
/// `on_dfrost_beacon` self-mint path never fires.
async fn install_beacon_oracle_for(
    engine: &Arc<VotingLogEngine<tauri::test::MockRuntime>>,
    community_id: SpaceId,
) -> Arc<Mutex<harmony_app::community_dfrost_log::DfrostLog>> {
    use harmony_app::community_dfrost_log::DfrostLog;
    use harmony_app::community_dfrost_log_engine::{DfrostLogEngineParams, DfrostLogRegistry};
    use harmony_app::community_voting_log_engine::BeaconRequester;

    let dfrost_log = Arc::new(Mutex::new(DfrostLog::new()));
    let registry = Arc::new(DfrostLogRegistry::<tauri::test::MockRuntime>::new());

    // A DfrostLogEngine needs an AppHandle + publish/subscribe channels; none of
    // it is exercised because tests seed `beacon_index` directly.
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
    /// `poll_create_event_hash` + `community_epoch` are read from the
    /// already-applied poll state (identical on both logs).
    ///
    /// Call AFTER PollCreate is applied and BEFORE publishing the kd=ss.
    pub async fn seed_ss_beacon(&self, poll_id: PollId, vrf_output: [u8; 32]) {
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
            log.committee_state.active = true;
            log.committee_state.current_epoch = community_epoch;
        }
    }
}

async fn setup_two_voting_engine_bridge(community_id: SpaceId) -> TwoVotingEngines {
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
        "engine-b".into(),
    )));

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
        hlc_tracker: Some(Arc::clone(&a_hlc_tracker)),
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
        hlc_tracker: Some(Arc::clone(&b_hlc_tracker)),
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
        a_hlc_tracker,
        b_hlc_tracker,
        dfrost_log_a,
        dfrost_log_b,
    }
}

async fn setup_two_voting_engine_bridge_with_signing(
    community_id: SpaceId,
    proposer: &TestIdentity,
) -> TwoVotingEngines {
    let engines = setup_two_voting_engine_bridge(community_id).await;
    engines.resolvers.add_identity(proposer);
    let signing_key = Arc::new(proposer.signing_key.clone());
    engines
        .engine_a
        .install_local_signing_key(signing_key, proposer.owner)
        .await;
    engines
}

/// Variant where BOTH engines have the proposer's signing key installed —
/// both can independently auto-orchestrate kd=cl + kd=rs. Used by Test 3
/// (race-tolerant convergence).
async fn setup_two_voting_engine_bridge_with_both_signing(
    community_id: SpaceId,
    proposer: &TestIdentity,
) -> TwoVotingEngines {
    let engines = setup_two_voting_engine_bridge(community_id).await;
    engines.resolvers.add_identity(proposer);
    let signing_key = Arc::new(proposer.signing_key.clone());
    engines
        .engine_a
        .install_local_signing_key(Arc::clone(&signing_key), proposer.owner)
        .await;
    engines
        .engine_b
        .install_local_signing_key(signing_key, proposer.owner)
        .await;
    engines
}

// ─── Polling helper ───────────────────────────────────────────────────────────

async fn wait_for_log<F>(
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

// ─── Event builders (thin wrappers over the IPC's underlying builders) ───────
//
// These call the SAME `build_signed_*` functions that the IPCs call. The
// only thing skipped is the hex-decode boundary (covered by Test 5 and by
// Task 14 vitest tests).

fn build_tier3_poll_create_event(
    proposer: &TestIdentity,
    config: &Tier3PollConfigPayload,
    hlc: Hlc,
) -> SignedVotingEvent {
    build_signed_poll_create_tier3(&proposer.signing_key, proposer.owner, config, hlc)
        .expect("build_signed_poll_create_tier3")
}

fn build_decline_event(actor: &TestIdentity, poll_id: PollId, hlc: Hlc) -> SignedVotingEvent {
    build_signed_mini_public_decline(&actor.signing_key, actor.owner, poll_id, None, hlc)
        .expect("build_signed_mini_public_decline")
}

fn build_draft_candidate_event(
    actor: &TestIdentity,
    poll_id: PollId,
    text: &str,
    hlc: Hlc,
) -> SignedVotingEvent {
    build_signed_draft_candidate(&actor.signing_key, actor.owner, poll_id, text.into(), hlc)
        .expect("build_signed_draft_candidate")
}

fn build_draft_approval_event(
    actor: &TestIdentity,
    poll_id: PollId,
    candidate_event_hash: CandidateEventHash,
    hlc: Hlc,
) -> SignedVotingEvent {
    build_signed_draft_approval(
        &actor.signing_key,
        actor.owner,
        poll_id,
        candidate_event_hash,
        hlc,
    )
    .expect("build_signed_draft_approval")
}

fn build_ratification_ballot_event(
    actor: &TestIdentity,
    poll_id: PollId,
    scores: Vec<u8>,
    hlc: Hlc,
) -> SignedVotingEvent {
    build_signed_ratification_ballot(&actor.signing_key, actor.owner, poll_id, scores, hlc)
        .expect("build_signed_ratification_ballot")
}

/// Build a signed kd=ss SortitionSelection event. ZEB-298+ZEB-312 PR 1:
/// uses real signing so the event passes `verify_voting_event` on the
/// receiving engine's inbound path.
fn build_sortition_selection_event(
    actor: &TestIdentity,
    poll_id: PollId,
    primary: Vec<OwnerAddr>,
    backup: Vec<OwnerAddr>,
    hlc: Hlc,
) -> SignedVotingEvent {
    build_signed_sortition_selection(
        &actor.signing_key,
        actor.owner,
        poll_id,
        primary,
        backup,
        hlc,
    )
    .expect("build_signed_sortition_selection")
}

// ─── Path A helpers (Test 5) ──────────────────────────────────────────────────

fn build_test_app() -> tauri::App<tauri::test::MockRuntime> {
    add_dm_ipc_handlers(mock_builder())
        .manage(StdMutex::new(NodeState::default()))
        .build(mock_context_with_full_acl(&[
            "voting_decline_sortition",
            "voting_create_tier3_proposal",
            "voting_propose_draft_candidate",
            "voting_approve_draft_candidate",
        ]))
        .expect("failed to build mock app")
}

fn make_invoke_request(cmd: &str, body: serde_json::Value) -> InvokeRequest {
    InvokeRequest {
        cmd: cmd.into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: LOCAL_IPC_URL.parse().expect("url must parse"),
        body: InvokeBody::Json(body),
        headers: Default::default(),
        invoke_key: INVOKE_KEY.to_string(),
    }
}

fn err_value_as_string(v: &serde_json::Value) -> String {
    v.as_str()
        .map(String::from)
        .unwrap_or_else(|| v.to_string())
}

// ─── TEST 1: Full lifecycle ───────────────────────────────────────────────────

/// IPC-equivalent full lifecycle (Path C). Drives a Tier 3 poll through
/// PollCreate → kd=ss → declines → drafts → approvals → ratification ballots
/// → engine-auto kd=cl → engine-auto kd=rs → both engines converge on
/// Stage::Finalized with bit-identical winner.
#[tokio::test]
async fn ipc_tier3_full_lifecycle_two_engines() {
    use harmony_app::community_voting_tier3::synthesize_status_quo;

    const SORTITION_SIZE: u16 = 20;
    const N_IDENTITIES: usize = 50;
    const COMMUNITY_ID: SpaceId = SpaceId([0xF1; 16]);

    let identities: Vec<TestIdentity> = (0..N_IDENTITIES as u8).map(fixture_identity).collect();
    let proposer = &identities[49];
    // ZEB-850 Task 5 (Issue A): the poll electorate must equal the injected-sortition
    // pool (verify_ss recomputes over eligible_electorate_snapshot). The proposer is
    // never a sortition candidate and never casts a ratification ballot here (voters
    // are identities[10..13]), so exclude it from the electorate.
    let sortition_pool: Vec<OwnerAddr> = identities[..49].iter().map(|id| id.owner).collect();

    // engine_a holds proposer's signing key (orchestration enabled).
    let engines = setup_two_voting_engine_bridge_with_signing(COMMUNITY_ID, proposer).await;
    for id in &identities {
        engines.resolvers.add_identity(id);
    }

    // Use HLCs well past total_window so engine-auto kd=cl + kd=rs fire.
    let t0: u64 = 6_000_000;

    let config = Tier3PollConfigPayload {
        proposal_text: "Full lifecycle IPC test".into(),
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

    // Apply PollCreate to both logs (skip publish_event to avoid D-FROST wiring).
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

    // Inject kd=ss with HLC well past total_window so engine-auto cl/rs
    // will fire after we've added ballots.
    let vrf_output: [u8; 32] = [0xF1; 32];
    let sortition_result = fisher_yates_select(
        &vrf_output,
        &sortition_pool,
        SORTITION_SIZE as usize,
        SORTITION_SIZE as usize,
    );

    // ZEB-850 Task 5: seed the beacon so the peer's verify_ss admits this kd=ss.
    engines.seed_ss_beacon(poll_id, vrf_output).await;
    // Important: kd=ss HLC must be EARLIER than the ballots' HLCs so the
    // ballots land while stage == Ratification. Use t0 + 1.
    let ss_hlc_wall = t0 + 1;
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

    wait_for_log("engine_b: sortition_result set", &engines.log_b, |log| {
        log.polls
            .get(&poll_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .map(|t3| t3.sortition_result.is_some())
            .unwrap_or(false)
    })
    .await
    .expect("engine_b: kd=ss must arrive within 5s");

    // Resolve primary TestIdentities.
    let primary_ids: Vec<&TestIdentity> = sortition_result
        .primary
        .iter()
        .map(|owner| identities.iter().find(|id| id.owner == *owner).unwrap())
        .collect();

    // Step: 1 representative decline (mini_public[0]). Not all decline →
    // poll proceeds to drafting (decline_count << capacity).
    let decline_ev =
        build_decline_event(primary_ids[0], poll_id, hlc_at(t0 + 100, "primary-0-dev"));
    engines
        .engine_a
        .publish_event(decline_ev, None)
        .await
        .expect("engine_a: publish kd=md");

    wait_for_log("engine_b: 1 decline applied", &engines.log_b, |log| {
        log.polls
            .get(&poll_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .map(|t3| !t3.declines.is_empty())
            .unwrap_or(false)
    })
    .await
    .expect("engine_b: must apply decline within 5s");

    // Step: drafting — primary_ids[1] and primary_ids[2] propose drafts.
    // Use HLC past deliberation window (60_000 ms).
    let t_drafting = t0 + 60_001;
    let dc_ev_1 = build_draft_candidate_event(
        primary_ids[1],
        poll_id,
        "Draft A from member 1",
        hlc_at(t_drafting, "primary-1-dev"),
    );
    let dc_ev_2 = build_draft_candidate_event(
        primary_ids[2],
        poll_id,
        "Draft B from member 2",
        hlc_at(t_drafting + 10, "primary-2-dev"),
    );
    engines
        .engine_a
        .publish_event(dc_ev_1.clone(), None)
        .await
        .expect("engine_a: publish kd=dc 1");
    engines
        .engine_a
        .publish_event(dc_ev_2.clone(), None)
        .await
        .expect("engine_a: publish kd=dc 2");

    // Compute candidate_event_hashes (same shape the IPC returns).
    let candidate_hash_1: CandidateEventHash = {
        use sha2::{Digest, Sha256};
        Sha256::digest(dc_ev_1.signing_bytes().expect("dc1 signing bytes")).into()
    };
    let candidate_hash_2: CandidateEventHash = {
        use sha2::{Digest, Sha256};
        Sha256::digest(dc_ev_2.signing_bytes().expect("dc2 signing bytes")).into()
    };

    // Step: drafting approvals — threshold is ceil(20/2)=10. The proposer
    // gets implicit self-approval at kd=dc apply, so each candidate needs
    // 9 more kd=da approvals from other mini-public members.
    //
    // ZEB-850 Task 5 (Issue C): primary_ids[0] DECLINED above, so it is no
    // longer in the mini-public — its kd=da is (correctly) rejected by
    // verify_sd at peer ingest. Draw approvers from primary_ids[1..11] so all 9
    // are still-seated members and both engines converge on 10 approvals.
    let mut t_approval = t_drafting + 100;
    for actor in primary_ids[1..11].iter() {
        // Skip self-approval (proposer already has one implicitly).
        if actor.owner != primary_ids[1].owner {
            let da_ev = build_draft_approval_event(
                actor,
                poll_id,
                candidate_hash_1,
                hlc_at(t_approval, "approver-dev"),
            );
            engines
                .engine_a
                .publish_event(da_ev, None)
                .await
                .expect("engine_a: publish kd=da for candidate 1");
            t_approval += 5;
        }
    }
    for actor in primary_ids[1..11].iter() {
        if actor.owner != primary_ids[2].owner {
            let da_ev = build_draft_approval_event(
                actor,
                poll_id,
                candidate_hash_2,
                hlc_at(t_approval, "approver-dev"),
            );
            engines
                .engine_a
                .publish_event(da_ev, None)
                .await
                .expect("engine_a: publish kd=da for candidate 2");
            t_approval += 5;
        }
    }

    // Step: ratification ballots. HLC past drafting window (t0 + 120_001).
    // 3 voters cast ballots with deterministic scores. Status quo is
    // always present; the 2 drafted candidates + status_quo = 3 candidates.
    let t_ratification = t0 + 120_001;

    // First let drafting events settle so we know ordered_candidates.
    wait_for_log("engine_a: candidates count >= 2", &engines.log_a, |log| {
        log.polls
            .get(&poll_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .map(|t3| t3.candidates.len() >= 2)
            .unwrap_or(false)
    })
    .await
    .expect("engine_a: 2 candidates registered");

    // Compute candidate count directly from t3.candidates + 1 for status_quo.
    // (`tier3_ratification_candidate_count` returns None unless status_quo is
    // already in `t3.candidates`, which only happens via engine-side
    // synthesize+push during orchestration. In production, the IPC's
    // pre-flight is also short-circuited until then — so we don't gain
    // coverage by going through that helper for this lifecycle test.)
    let n_ratification_candidates = {
        let log = engines.log_a.lock().await;
        let t3 = log.polls[&poll_id]
            .tier_state
            .as_tier3()
            .expect("log_a tier3");
        // Count non-status_quo candidates that hit threshold + status_quo.
        let sq = harmony_app::community_voting_tier3::synthesize_status_quo(&poll_id);
        let primary_size = t3.meta.config.sortition_size as usize;
        let threshold = primary_size.div_ceil(2);
        let passers = t3
            .candidates
            .iter()
            .filter(|c| c.event_hash != sq.event_hash && c.approvals.len() >= threshold)
            .count();
        passers + 1 // + status_quo
    };
    assert!(
        n_ratification_candidates >= 2,
        "ratification must have ≥ 2 candidates (got {n_ratification_candidates})"
    );

    // 3 voters cast deterministic ballots. All score candidate 0 highest.
    // Reuse identities[10..13] (full electorate is eligible to ratify).
    let voters = &identities[10..13];
    let mut t_ballot = t_ratification;
    for voter in voters.iter() {
        let scores: Vec<u8> = (0..n_ratification_candidates as u8)
            .map(|i| if i == 0 { 5 } else { 4u8.saturating_sub(i) })
            .collect();
        let rb_ev =
            build_ratification_ballot_event(voter, poll_id, scores, hlc_at(t_ballot, "voter-dev"));
        engines
            .engine_a
            .publish_event(rb_ev, None)
            .await
            .expect("engine_a: publish kd=rb");
        t_ballot += 10;
    }

    // Now publish a "ticker" event at HLC past total_window to drive
    // engine-auto kd=cl + kd=rs cascade. We re-use the kd=ss broadcast
    // pattern: any apply with HLC past the ratification deadline triggers
    // the orchestration hook on engine_a (which holds the signing key).
    //
    // The trick: publish a (no-op-from-state-perspective) kd=rb from
    // another voter with HLC well past total_window (t0 + 180_001). The
    // apply succeeds (ballot recorded), then the orchestration hook sees
    // ratification deadline expired AND close_event_hash.is_none() → fires
    // kd=cl → re-fires hook → fires kd=rs → Stage::Finalized.
    let trigger_voter = &identities[13];
    let trigger_scores: Vec<u8> = vec![5; n_ratification_candidates];
    let trigger_rb = build_ratification_ballot_event(
        trigger_voter,
        poll_id,
        trigger_scores,
        hlc_at(t0 + 200_000, "voter-trigger-dev"),
    );
    engines
        .engine_a
        .publish_event(trigger_rb, None)
        .await
        .expect("engine_a: publish trigger kd=rb");

    // Wait for full convergence on both engines.
    wait_for_log("engine_a: Stage::Finalized", &engines.log_a, |log| {
        log.polls
            .get(&poll_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .map(|t3| t3.stage == Stage::Finalized)
            .unwrap_or(false)
    })
    .await
    .expect("engine_a: must reach Stage::Finalized");

    wait_for_log("engine_b: Stage::Finalized", &engines.log_b, |log| {
        log.polls
            .get(&poll_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .map(|t3| t3.stage == Stage::Finalized)
            .unwrap_or(false)
    })
    .await
    .expect("engine_b: must reach Stage::Finalized");

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
    assert_eq!(t3_a.stage, Stage::Finalized, "engine_a: must be Finalized");
    assert_eq!(t3_b.stage, Stage::Finalized, "engine_b: must be Finalized");
    let result_a = t3_a.result.as_ref().expect("engine_a result");
    let result_b = t3_b.result.as_ref().expect("engine_b result");
    assert_eq!(
        result_a, result_b,
        "CONVERGENCE: both engines must have bit-identical StarResult"
    );
    // Winner must be one of: candidate 1, candidate 2, or status_quo.
    let sq_hash = synthesize_status_quo(&poll_id).event_hash;
    let winner_hash = result_a.winner.event_hash;
    assert!(
        winner_hash == candidate_hash_1
            || winner_hash == candidate_hash_2
            || winner_hash == sq_hash,
        "winner must be one of the 3 candidates"
    );

    drop(engines);
}

// ─── TEST 2: Engine-auto kd=sf on mass decline ────────────────────────────────

/// IPC-equivalent mass decline path (Path C). Builds on the engine-layer
/// `engine_auto_sf_on_mass_decline_from_proposer` test but distinguishes
/// itself as the IPC-driven equivalent by using the build_signed_*
/// builders the IPC handlers themselves call. Drives all 40 mini-public
/// members (20 primary + 20 backup) to decline → engine-auto kd=sf fires
/// → both engines converge on Stage::Failed.
#[tokio::test]
async fn ipc_tier3_engine_auto_kd_sf_on_mass_decline() {
    const SORTITION_SIZE: u16 = 20;
    const N_IDENTITIES: usize = 50;
    const COMMUNITY_ID: SpaceId = SpaceId([0xF2; 16]);

    let identities: Vec<TestIdentity> = (0..N_IDENTITIES as u8).map(fixture_identity).collect();
    let proposer = &identities[49];
    // ZEB-850 Task 5 (Issue A): the poll electorate must equal the injected-sortition
    // pool (verify_ss recomputes over eligible_electorate_snapshot). Proposer excluded
    // from both; it still mints kd=sf as the proposer.
    let sortition_pool: Vec<OwnerAddr> = identities[..49].iter().map(|id| id.owner).collect();

    let engines = setup_two_voting_engine_bridge_with_signing(COMMUNITY_ID, proposer).await;

    let t0: u64 = 5_000_000;
    let config = Tier3PollConfigPayload {
        proposal_text: "IPC-equivalent kd=sf test".into(),
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
    };

    let create_event = build_tier3_poll_create_event(proposer, &config, hlc_at(t0, "proposer-dev"));
    let poll_id = derive_poll_id(
        &COMMUNITY_ID,
        &create_event.signing_bytes().expect("signing_bytes"),
    );

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

    let vrf_output: [u8; 32] = [0xF2; 32];
    let sortition_result = fisher_yates_select(
        &vrf_output,
        &sortition_pool,
        SORTITION_SIZE as usize,
        SORTITION_SIZE as usize,
    );

    // ZEB-850 Task 5: seed the beacon so the peer's verify_ss admits this kd=ss.
    engines.seed_ss_beacon(poll_id, vrf_output).await;
    // Register all identities so the receiving engine can verify their signatures.
    for id in &identities {
        engines.resolvers.add_identity(id);
    }

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

    wait_for_log("engine_b: sortition_result set", &engines.log_b, |log| {
        log.polls
            .get(&poll_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .map(|t3| t3.sortition_result.is_some())
            .unwrap_or(false)
    })
    .await
    .expect("engine_b: kd=ss must arrive within 5s");

    // Publish 40 declines via the IPC-equivalent build_signed_mini_public_decline.
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
            hlc_at(t_decline_base + i as u64 * 10, "primary-dev"),
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
            hlc_at(backup_decline_base + i as u64 * 10, "backup-dev"),
        );
        engines
            .engine_a
            .publish_event(decline_ev, None)
            .await
            .expect("publish backup decline");
    }

    // Wait for engine-auto kd=sf to propagate to engine_b.
    wait_for_log(
        "engine_b: Stage::Failed (engine-auto kd=sf propagated)",
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

    {
        let log = engines.log_a.lock().await;
        let t3 = log.polls[&poll_id].tier_state.as_tier3().unwrap();
        assert_eq!(
            t3.stage,
            Stage::Failed,
            "engine_a: must be Failed after engine-auto kd=sf"
        );
        assert!(
            t3.declines.len() >= 40,
            "engine_a: expected ≥ 40 declines, got {}",
            t3.declines.len()
        );
    }

    drop(engines);
}

// ─── TEST 3: Race-tolerant kd=cl + kd=rs ──────────────────────────────────────

/// IPC-equivalent race-tolerant orchestration (Path C). BOTH engines have
/// the proposer's signing key installed; both independently emit kd=cl +
/// kd=rs from their own post-apply hook — engine_a via `publish_event`,
/// engine_b via the ZEB-316 re-enabled INBOUND dispatch hook. Because the
/// mint HLCs are now derived deterministically from the triggering kd=ss HLC
/// (no wall-clock), the two independent mints are byte-identical, so the
/// `close_event_hash_a == close_event_hash_b` equality below proves
/// convergence of INDEPENDENT mints (not merely one engine adopting the
/// other's broadcast).
///
/// Runs the full scenario, asserts complete cross-engine convergence
/// (identical stage, byte-identical close_event_hash, and identical
/// StarResult), and returns the live engines plus the finalized
/// `close_event_hash` so callers can additionally assert determinism ACROSS
/// runs (see the repeat test below) or inspect post-finalize engine state
/// (e.g. the shared HLC trackers — ZEB-731).
///
/// `ss_hlc_wall` is the wall (ms) of the injected kd=ss trigger. It must be
/// past the ratification deadline (`t0 + total_window`); a far-future value
/// also exercises the ZEB-731 future-walled-watermark path.
async fn run_race_tolerant_inner(ss_hlc_wall: u64) -> (TwoVotingEngines, Option<[u8; 32]>) {
    const SORTITION_SIZE: u16 = 20;
    const N_IDENTITIES: usize = 50;
    const COMMUNITY_ID: SpaceId = SpaceId([0xF3; 16]);

    let identities: Vec<TestIdentity> = (0..N_IDENTITIES as u8).map(fixture_identity).collect();
    let proposer = &identities[49];
    let sortition_pool: Vec<OwnerAddr> = identities[..49].iter().map(|id| id.owner).collect();

    // Both engines have the signing key installed → both can auto-orchestrate.
    let engines = setup_two_voting_engine_bridge_with_both_signing(COMMUNITY_ID, proposer).await;

    let t0: u64 = 6_000_000;
    let config = Tier3PollConfigPayload {
        proposal_text: "Race-tolerant cl/rs test".into(),
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
    };

    let create_event = build_tier3_poll_create_event(proposer, &config, hlc_at(t0, "proposer-dev"));
    let poll_id = derive_poll_id(
        &COMMUNITY_ID,
        &create_event.signing_bytes().expect("signing_bytes"),
    );

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

    // Inject kd=ss with HLC well past the ratification deadline
    // (t0 + total_window = t0 + 180_000). The caller-supplied `ss_hlc_wall`
    // (default t0 + 500_000 leaves 320_000ms of safety margin; a far-future
    // value exercises the ZEB-731 path). BOTH engines apply kd=ss and BOTH
    // fire engine-auto kd=cl + kd=rs from their own apply hook. The
    // HLC-driven "now" view is the kd=ss HLC, so both see ratification expired.
    let vrf_output: [u8; 32] = [0xF3; 32];
    let sortition_result = fisher_yates_select(
        &vrf_output,
        &sortition_pool,
        SORTITION_SIZE as usize,
        SORTITION_SIZE as usize,
    );

    // ZEB-850 Task 5: seed the beacon so the peer's verify_ss admits this kd=ss.
    engines.seed_ss_beacon(poll_id, vrf_output).await;
    // Register all identities so the receiving engine can verify their signatures.
    for id in &identities {
        engines.resolvers.add_identity(id);
    }

    let ss_event = build_sortition_selection_event(
        proposer,
        poll_id,
        sortition_result.primary.clone(),
        sortition_result.backup.clone(),
        hlc_at(ss_hlc_wall, "engine"),
    );

    // Publish via engine_a; the bridge forwards to engine_b. BOTH engines
    // therefore apply kd=ss, and BOTH fire their orchestration hook.
    engines
        .engine_a
        .publish_event(ss_event, None)
        .await
        .expect("engine_a: publish kd=ss");

    // Wait for both engines to reach Stage::Finalized. Each engine
    // independently emits kd=cl + kd=rs; the log-side ordering (and
    // first-by-HLC verify gate in apply_with_snapshot) ensures only one
    // wins.
    wait_for_log("engine_a: Stage::Finalized", &engines.log_a, |log| {
        log.polls
            .get(&poll_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .map(|t3| t3.stage == Stage::Finalized)
            .unwrap_or(false)
    })
    .await
    .expect("engine_a: must reach Stage::Finalized");

    wait_for_log("engine_b: Stage::Finalized", &engines.log_b, |log| {
        log.polls
            .get(&poll_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .map(|t3| t3.stage == Stage::Finalized)
            .unwrap_or(false)
    })
    .await
    .expect("engine_b: must reach Stage::Finalized");

    // Bit-identical convergence: kd=rs `scores_summary` (i.e. the
    // StarResult payload) must be byte-for-byte identical across engines.
    // This proves the race-tolerant pattern: even if engine_a and engine_b
    // each minted their own kd=cl + kd=rs candidate, the first-by-HLC
    // winner has bit-identical `result` because tally_star is deterministic.
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
    // ZEB-850 peer verify_ss admission (CodeAnt/CodeRabbit): both engines must
    // have set `sortition_result` from the bridged kd=ss. Before the snapshot
    // was aligned with `sortition_pool`, engine_b's peer verify_ss rejected the
    // selection (SortitionMismatch) and only reached Finalized via the bridged
    // kd=cl/kd=rs fallback with `sortition_result == None` — this assertion
    // fails in that degenerate case, proving the peer kd=ss accept path runs.
    assert!(
        t3_a.sortition_result.is_some(),
        "engine_a must have applied kd=ss (sortition_result set)"
    );
    assert!(
        t3_b.sortition_result.is_some(),
        "engine_b must ADMIT the peer kd=ss via verify_ss (sortition_result set), \
         not merely finalize through bridged kd=cl/kd=rs"
    );
    // CRITICAL: close_event_hash must match (only one kd=cl winner).
    assert_eq!(
        t3_a.close_event_hash, t3_b.close_event_hash,
        "race-tolerant: both engines must converge on the SAME kd=cl winner"
    );
    let result_a = t3_a.result.as_ref().expect("engine_a result");
    let result_b = t3_b.result.as_ref().expect("engine_b result");
    assert_eq!(
        result_a, result_b,
        "CONVERGENCE: bit-identical StarResult across both engines"
    );

    // The winning kd=cl close_event_hash — deterministic across runs.
    // Return the live engines so callers can inspect post-finalize state
    // (ZEB-731 tracker inspection); default callers just drop them.
    (engines, t3_a.close_event_hash)
}

/// Default kd=ss trigger wall for the standard race-tolerant tests: past the
/// ratification deadline but in the PAST relative to real `SystemTime::now()`
/// (t0 = 6_000_000, + 500_000 margin).
const RACE_TOLERANT_DEFAULT_SS_WALL: u64 = 6_500_000;

/// Thin wrapper preserving the pre-ZEB-731 signature for the convergence +
/// determinism-repeat callers: runs with the default trigger wall, drops the
/// engines, and returns only the finalized `close_event_hash`.
async fn run_race_tolerant_and_return_close_hash() -> Option<[u8; 32]> {
    let (engines, close_hash) = run_race_tolerant_inner(RACE_TOLERANT_DEFAULT_SS_WALL).await;
    drop(engines);
    close_hash
}

/// Thin wrapper preserving the original test name; drives the scenario once
/// and asserts a winning kd=cl close_event_hash was recorded. All the
/// cross-engine convergence assertions live in the helper above.
#[tokio::test]
async fn ipc_tier3_engine_auto_kd_cl_kd_rs_race_tolerant() {
    let close_hash = run_race_tolerant_and_return_close_hash().await;
    assert!(
        close_hash.is_some(),
        "a finalized race-tolerant poll must record the winning kd=cl close_event_hash"
    );
}

/// ZEB-316 determinism proof. Run the two-engine race-tolerant scenario 100×;
/// a wall-clock-derived mint HLC would make the kd=cl event (and hence its
/// `close_event_hash`) vary run-to-run. Structural determinism — the mint HLC
/// is derived purely from the triggering kd=ss HLC — implies a single distinct
/// `close_event_hash` across every run. Lightweight: reuses the same fixtures,
/// a fresh pair of engines per iteration, and the scenario itself finalizes in
/// tens of milliseconds.
#[tokio::test]
async fn ipc_tier3_engine_auto_kd_cl_kd_rs_deterministic_repeat() {
    let mut hashes = std::collections::HashSet::new();
    for _ in 0..100 {
        hashes.insert(run_race_tolerant_and_return_close_hash().await);
    }
    assert_eq!(
        hashes.len(),
        1,
        "close_event_hash must be identical across 100 independent runs; got {} distinct",
        hashes.len()
    );
}

/// ZEB-731 regression guard. When the poll's receive watermark is FUTURE-walled
/// (clock skew or a future-dated trigger), the engine-auto kd=rs mint must NOT
/// bump the shared per-device HLC tracker to that future wall — otherwise the
/// future wall leaks into the device's *global* outbound lane and can
/// transiently wedge every other poll/channel/DM mint on that device forward.
///
/// The poll-derived-lane fix mints kd=rs via
/// `engine_auto_hlc_from_base(&watermark, pid, "rs")` — strictly above the
/// watermark by `logical+1`, WITHOUT reading the wall clock or touching the
/// device tracker — so the future wall stays poll-scoped. RED against the
/// shipped wall-clock floor (`reserve_next_local_hlc_above` reserved on the
/// device lane, leaking the future wall); GREEN after the fix.
///
/// Also proves LIVENESS: the poll still reaches Stage::Finalized under a
/// future-walled watermark (the helper's Finalized waits would time out
/// otherwise).
///
/// ZEB-846 interaction: the kd=ss trigger is published via `engine_a` and
/// BRIDGES to `engine_b` through `process_inbound`, which (post-ZEB-846) now
/// rejects any inbound event whose `hlc.wall_ms` is beyond
/// `receiver_now + clock_trust::MAX_FORWARD_SKEW_MS` (5 min) at admission.
/// The old ~year-33658 (`1e15`) wall is therefore unreachable through
/// ingestion any more — engine_b would never apply kd=ss and this guard would
/// hang waiting for `Stage::Finalized`. The bounded worst case a receiver can
/// actually observe is `now + MAX_FORWARD_SKEW_MS`, so the trigger wall here
/// is real-now + 2min: comfortably inside the 5-min bound (ACCEPTED at
/// engine_b) yet far enough above the honest device lane (~real-now) to still
/// detect a ZEB-731 leak. This keeps proving both (a) liveness — the poll
/// still finalizes under the bounded worst-case future watermark — and (b)
/// the ZEB-731 guarantee — the kd=rs mint does not leak that wall into either
/// engine's shared device lane.
#[tokio::test]
async fn ipc_tier3_engine_auto_kd_rs_future_walled_no_device_lane_leak() {
    // Real-now + 2min: within clock_trust::MAX_FORWARD_SKEW_MS (5min) so the
    // bridged kd=ss is ACCEPTED at engine_b (post-ZEB-846), yet far enough
    // above the honest device lane (~real-now) to detect a ZEB-731 leak.
    let future_wall: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX_EPOCH")
        .as_millis() as u64
        + 120_000;

    let (engines, close_hash) = run_race_tolerant_inner(future_wall).await;

    // Liveness: the poll finalized despite the future-walled watermark.
    assert!(
        close_hash.is_some(),
        "poll must finalize (record a kd=cl close_event_hash) even when the \
         trigger — and thus the receive watermark — is future-walled"
    );

    // Core ZEB-731 guard: engine_a auto-minted kd=cl + kd=rs off a future-walled
    // watermark. kd=cl/kd=sf never touch the device tracker; the fixed kd=rs
    // mints on a poll-derived lane and must not either. So neither engine's own
    // device lane may have been advanced to the future wall. (Under the shipped
    // wall-clock floor engine_a's lane read >= future_wall — the device-wide
    // leak; engine_a is the kd=ss originator so it deterministically mints its
    // own kd=rs before adopting a peer's.)
    //
    // Post-fix both trackers stay empty for a pu-mode poll (poll-lane kd=rs
    // touches no device lane; kd=ts is se-mode only), so `< future_wall` holds
    // via the `unwrap_or(0)` absent-lane default.
    let lane_wall = |tr: &std::collections::BTreeMap<String, Hlc>, dev: &str| {
        tr.get(dev).map(|h| h.wall_ms).unwrap_or(0)
    };
    let (engine_a_lane_wall, engine_b_lane_wall) = {
        let ta = engines.a_hlc_tracker.lock().await;
        let tb = engines.b_hlc_tracker.lock().await;
        (
            lane_wall(ta.accepted(), "engine-a"),
            lane_wall(tb.accepted(), "engine-b"),
        )
    };
    assert!(
        engine_a_lane_wall < future_wall,
        "ZEB-731: future-walled kd=rs mint leaked into engine_a's shared device \
         lane — engine-a lane wall {engine_a_lane_wall} >= future {future_wall}"
    );
    assert!(
        engine_b_lane_wall < future_wall,
        "ZEB-731: future-walled kd=rs mint leaked into engine_b's shared device \
         lane — engine-b lane wall {engine_b_lane_wall} >= future {future_wall}"
    );

    drop(engines);
}

// ─── TEST 4: retry_of via IPC ─────────────────────────────────────────────────

/// IPC-equivalent retry_of chain. Build poll A, drive to Stage::Failed via
/// mass decline. Build poll B with retry_of = Some(poll_A_id) via the
/// IPC's underlying builder (`build_signed_poll_create_tier3` with
/// `Tier3PollConfigPayload.retry_of`). Apply on both engines. Assert:
///
///   - poll B's tier_state has retry_of = Some(poll_A_id) on BOTH engines,
///   - poll A is still Stage::Failed.
#[tokio::test]
async fn ipc_tier3_retry_of_via_ipc() {
    const SORTITION_SIZE: u16 = 20;
    const N_IDENTITIES: usize = 50;
    const COMMUNITY_ID: SpaceId = SpaceId([0xF4; 16]);

    let identities: Vec<TestIdentity> = (0..N_IDENTITIES as u8).map(fixture_identity).collect();
    let proposer = &identities[49];
    // ZEB-850 Task 5 (Issue A): the poll electorate must equal the injected-sortition
    // pool (verify_ss recomputes over eligible_electorate_snapshot). Proposer excluded
    // from both; it still mints kd=sf as the proposer.
    let sortition_pool: Vec<OwnerAddr> = identities[..49].iter().map(|id| id.owner).collect();

    let engines = setup_two_voting_engine_bridge_with_signing(COMMUNITY_ID, proposer).await;
    // Register all identities so the receiving engine can verify their signatures.
    for id in &identities {
        engines.resolvers.add_identity(id);
    }

    // ─── Poll A: drive to Stage::Failed via mass decline ─────────────────────
    let t0_a: u64 = 7_000_000;
    let config_a = Tier3PollConfigPayload {
        proposal_text: "Failed parent poll for retry_of test".into(),
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
    };

    let create_a = build_tier3_poll_create_event(proposer, &config_a, hlc_at(t0_a, "proposer-dev"));
    let poll_a_id = derive_poll_id(
        &COMMUNITY_ID,
        &create_a.signing_bytes().expect("create_a signing_bytes"),
    );

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
        log.apply_with_snapshot(create_a.clone(), &COMMUNITY_ID, Some(snapshot.clone()))
            .expect("engine_a: poll A apply");
    }
    {
        let mut log = engines.log_b.lock().await;
        log.apply_with_snapshot(create_a, &COMMUNITY_ID, Some(snapshot.clone()))
            .expect("engine_b: poll A apply");
    }

    let vrf_output_a: [u8; 32] = [0xF4; 32];
    let sortition_a = fisher_yates_select(
        &vrf_output_a,
        &sortition_pool,
        SORTITION_SIZE as usize,
        SORTITION_SIZE as usize,
    );

    // ZEB-850 Task 5: seed the beacon so the peer's verify_ss admits this kd=ss.
    engines.seed_ss_beacon(poll_a_id, vrf_output_a).await;
    let ss_a = build_sortition_selection_event(
        proposer,
        poll_a_id,
        sortition_a.primary.clone(),
        sortition_a.backup.clone(),
        hlc_at(t0_a + 1, "engine"),
    );
    engines
        .engine_a
        .publish_event(ss_a, None)
        .await
        .expect("engine_a: publish kd=ss for poll A");

    wait_for_log(
        "engine_b: poll A sortition_result set",
        &engines.log_b,
        |log| {
            log.polls
                .get(&poll_a_id)
                .and_then(|ps| ps.tier_state.as_tier3())
                .map(|t3| t3.sortition_result.is_some())
                .unwrap_or(false)
        },
    )
    .await
    .expect("engine_b: poll A kd=ss must arrive within 5s");

    let primary_ids: Vec<&TestIdentity> = sortition_a
        .primary
        .iter()
        .map(|owner| identities.iter().find(|id| id.owner == *owner).unwrap())
        .collect();
    let backup_ids: Vec<&TestIdentity> = sortition_a
        .backup
        .iter()
        .map(|owner| identities.iter().find(|id| id.owner == *owner).unwrap())
        .collect();

    let t_decline_base = t0_a + 100;
    for (i, decliner) in primary_ids.iter().enumerate() {
        let decline_ev = build_decline_event(
            decliner,
            poll_a_id,
            hlc_at(t_decline_base + i as u64 * 10, "primary-dev"),
        );
        engines
            .engine_a
            .publish_event(decline_ev, None)
            .await
            .expect("engine_a: publish primary decline (poll A)");
    }
    let backup_decline_base = t_decline_base + (SORTITION_SIZE as u64) * 10 + 10;
    for (i, decliner) in backup_ids.iter().enumerate() {
        let decline_ev = build_decline_event(
            decliner,
            poll_a_id,
            hlc_at(backup_decline_base + i as u64 * 10, "backup-dev"),
        );
        engines
            .engine_a
            .publish_event(decline_ev, None)
            .await
            .expect("engine_a: publish backup decline (poll A)");
    }

    wait_for_log("engine_b: poll A Stage::Failed", &engines.log_b, |log| {
        log.polls
            .get(&poll_a_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .map(|t3| t3.stage == Stage::Failed)
            .unwrap_or(false)
    })
    .await
    .expect("engine_b: poll A must reach Stage::Failed within 5s");

    // ─── Poll B: retry_of = Some(poll_A_id) ──────────────────────────────────
    let t0_b: u64 = 8_000_000;
    let config_b = Tier3PollConfigPayload {
        proposal_text: "Retry of failed poll A".into(),
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
        retry_of: Some(poll_a_id),
    };

    let create_b = build_tier3_poll_create_event(proposer, &config_b, hlc_at(t0_b, "proposer-dev"));
    let poll_b_id = derive_poll_id(
        &COMMUNITY_ID,
        &create_b.signing_bytes().expect("create_b signing_bytes"),
    );

    // Apply poll B to both engines via direct apply (skips D-FROST wiring).
    {
        let mut log = engines.log_a.lock().await;
        log.apply_with_snapshot(create_b.clone(), &COMMUNITY_ID, Some(snapshot.clone()))
            .expect("engine_a: poll B apply");
    }
    {
        let mut log = engines.log_b.lock().await;
        log.apply_with_snapshot(create_b, &COMMUNITY_ID, Some(snapshot))
            .expect("engine_b: poll B apply");
    }

    // Assert: poll B's tier_state config.retry_of = Some(poll_A_id) on both engines.
    {
        let log_a = engines.log_a.lock().await;
        let t3_b_on_a = log_a.polls[&poll_b_id]
            .tier_state
            .as_tier3()
            .expect("log_a poll B tier3");
        assert_eq!(
            t3_b_on_a.meta.config.retry_of,
            Some(poll_a_id),
            "engine_a: poll B must carry retry_of = poll A's id"
        );

        let log_b = engines.log_b.lock().await;
        let t3_b_on_b = log_b.polls[&poll_b_id]
            .tier_state
            .as_tier3()
            .expect("log_b poll B tier3");
        assert_eq!(
            t3_b_on_b.meta.config.retry_of,
            Some(poll_a_id),
            "engine_b: poll B must carry retry_of = poll A's id"
        );

        // Assert: poll A is still Stage::Failed on both engines.
        let t3_a_on_a = log_a.polls[&poll_a_id]
            .tier_state
            .as_tier3()
            .expect("log_a poll A tier3");
        assert_eq!(t3_a_on_a.stage, Stage::Failed, "engine_a: poll A unchanged");
        let t3_a_on_b = log_b.polls[&poll_a_id]
            .tier_state
            .as_tier3()
            .expect("log_b poll A tier3");
        assert_eq!(t3_a_on_b.stage, Stage::Failed, "engine_b: poll A unchanged");
    }

    drop(engines);
}

// ─── TEST 5: Error extraction (Path A — full IPC) ─────────────────────────────

/// Path A: drive the Tier 3 IPCs through `tauri::test::get_ipc_response`
/// with invalid arguments. Verifies that error returns are `Result<_, String>`
/// per project convention AND that the error messages contain the expected
/// substrings (per `voting_*` IPCs' hex-decode + length-validation paths).
///
/// Pattern source: `tests/dm_ipc_roundtrip.rs`.
#[test]
fn ipc_tier3_error_extraction_string_and_error() {
    let app = build_test_app();
    let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .expect("webview build");

    // Case 1: voting_decline_sortition with invalid hex.
    {
        let response = get_ipc_response(
            &webview,
            make_invoke_request(
                "voting_decline_sortition",
                serde_json::json!({
                    "pollId": "not-valid-hex-XX",
                    "reason": null,
                }),
            ),
        );
        let err = response.expect_err("invalid hex must return Err");
        let err_str = err_value_as_string(&err);
        assert!(
            err_str.contains("invalid poll_id hex") || err_str.contains("poll_id must be 32 bytes"),
            "voting_decline_sortition invalid-hex must surface hex error; got: {err_str}"
        );
    }

    // Case 2: voting_create_tier3_proposal with non-16-byte community_id hex
    // (valid hex chars, wrong length).
    {
        let response = get_ipc_response(
            &webview,
            make_invoke_request(
                "voting_create_tier3_proposal",
                serde_json::json!({
                    "communityId": "aabb",
                    "channelId": "00".repeat(16),
                    "proposalText": "Some proposal",
                    "sortitionSize": 20,
                    "deliberationWindowSeconds": 3600,
                    "draftingWindowSeconds": 3600,
                    "ratificationWindowSeconds": 3600,
                    "incentiveMode": "a",
                    "minPower": 0,
                    "minVouchingDepth": null,
                    "retryOf": null,
                }),
            ),
        );
        let err = response.expect_err("short community_id hex must return Err");
        let err_str = err_value_as_string(&err);
        assert!(
            err_str.contains("community_id must be 16 bytes"),
            "voting_create_tier3_proposal short-community_id must surface length error; got: {err_str}"
        );
    }

    // Case 3: voting_propose_draft_candidate with empty candidate_text
    // (proves error returns are `Result<_, String>` — easy to extract).
    {
        let response = get_ipc_response(
            &webview,
            make_invoke_request(
                "voting_propose_draft_candidate",
                serde_json::json!({
                    "pollId": "00".repeat(32),
                    "candidateText": "",
                }),
            ),
        );
        let err = response.expect_err("empty candidate_text must return Err");
        let err_str = err_value_as_string(&err);
        assert!(
            err_str.contains("candidate_text length 0 out of range"),
            "voting_propose_draft_candidate empty-text must surface range error; got: {err_str}"
        );
    }

    // Case 4: voting_approve_draft_candidate with invalid candidate_event_hash hex.
    {
        let response = get_ipc_response(
            &webview,
            make_invoke_request(
                "voting_approve_draft_candidate",
                serde_json::json!({
                    "pollId": "00".repeat(32),
                    "candidateEventHash": "not-valid-hex",
                }),
            ),
        );
        let err = response.expect_err("invalid candidate_event_hash hex must return Err");
        let err_str = err_value_as_string(&err);
        assert!(
            err_str.contains("invalid candidate_event_hash hex")
                || err_str.contains("candidate_event_hash must be 32 bytes"),
            "voting_approve_draft_candidate invalid-hash must surface hex error; got: {err_str}"
        );
    }
}

// ─── TEST 6: se-mode two-engine result-convergence (ZEB-316 payoff) ───────────
//
// Proves the RE-ENABLED inbound orchestration hook drives secret-tally
// finalization on a peer replica in the REALISTIC production arrangement where
// committee kd=ts (tally-share) events land AFTER the close carrying per-replica
// wall-clock HLCs whose `wall > close.wall`. The design is deliberately
// ASYMMETRIC so the test is *load-bearing* for the inbound re-enable — a
// symmetric both-signing setup would let engine_b lean on engine_a's broadcast
// kd=rs and pass even if the hook were still disabled:
//
//   * engine_a holds NO signing key — a pure relay that publishes the
//     committee's kd=ts shares onto the bridge. Its own post-apply cascade
//     short-circuits at the `local_signing` gate, so it never mints kd=rs.
//   * engine_b holds the proposer signing key. It receives the ≥t kd=ts shares
//     via the INBOUND dispatch path and — only because ZEB-316 re-enabled
//     `maybe_trigger_engine_auto_orchestration` there — its cascade tail runs
//     `try_finalize_secret_tally`, minting kd=rs on a WALL-CLOCK HLC
//     (`reserve_next_local_hlc`). se-mode does NOT anchor kd=rs on the close
//     event's HLC: the kd=ts above already sit at a wall > close, so a
//     close-anchored kd=rs would be non-monotonic and rejected by the
//     apply-time gate (ZEB-316 C1).
//   * engine_a then finalizes by applying engine_b's broadcast kd=rs.
//
// If the inbound hook were still disabled engine_b would store both shares but
// never finalize → no kd=rs → engine_a never finalizes → the waits below time
// out. A green run confirms the re-enabled cascade (which subsumes the removed
// standalone se-mode block) finalizes secret polls through inbound dispatch.
//
// RED→GREEN evidence (ZEB-316 C1 fix): with kd=ts.wall > close.wall (below),
// this test FAILS against the pre-fix close-anchored se-mode kd=rs — engine_b
// mints kd=rs at close.wall, the apply-time monotonic gate rejects it (an
// applied kd=ts sits higher), engine_b never finalizes, the wait times out. It
// PASSES once se-mode kd=rs reverts to a wall-clock HLC. The kd=rs *result*
// (StarResult) still converges bit-identically across replicas via Lagrange
// invariance + the apply-time LWW gate — which is what the assertions check.

/// A `threshold`-of-`n` mock threshold-ElGamal committee over Ristretto255.
/// `members` is sorted ascending by `OwnerAddr` so FROST identifier `i+1`
/// matches the BTreeMap iteration order `recover_secret_tally` derives.
struct SeCommittee {
    joint_y: RistrettoPoint,
    members: Vec<OwnerAddr>,
    shares: BTreeMap<u16, Scalar>,
    verifying_shares: BTreeMap<u16, RistrettoPoint>,
    threshold: u16,
}

/// Build a `threshold`-of-`members.len()` committee from a random
/// degree-(threshold-1) polynomial. `members` MUST be sorted ascending.
fn build_se_committee(members: &[OwnerAddr], threshold: u16) -> SeCommittee {
    assert!(
        members.windows(2).all(|w| w[0] < w[1]),
        "committee members must be sorted ascending and distinct"
    );
    let n = members.len() as u16;
    assert!((1..=n).contains(&threshold));
    let coeffs: Vec<Scalar> = (0..threshold).map(|_| Scalar::random(&mut OsRng)).collect();
    let joint_y = RISTRETTO_BASEPOINT_POINT * coeffs[0];
    let mut shares = BTreeMap::new();
    let mut verifying_shares = BTreeMap::new();
    for i in 1u16..=n {
        let id = Scalar::from(i as u64);
        let mut acc = Scalar::ZERO;
        let mut id_pow = Scalar::ONE;
        for c in &coeffs {
            acc += c * id_pow;
            id_pow *= id;
        }
        shares.insert(i, acc);
        verifying_shares.insert(i, RISTRETTO_BASEPOINT_POINT * acc);
    }
    SeCommittee {
        joint_y,
        members: members.to_vec(),
        shares,
        verifying_shares,
        threshold,
    }
}

#[derive(Debug)]
struct SeMockOracle {
    joint_verifying_key: [u8; 32],
    verifying_shares: BTreeMap<OwnerAddr, [u8; 32]>,
    threshold: u16,
    latest: u64,
}

impl CommitteeOracle for SeMockOracle {
    fn committee_at_epoch(&self, epoch: u64) -> Option<CommitteePublicState> {
        Some(CommitteePublicState {
            epoch,
            joint_verifying_key: self.joint_verifying_key,
            verifying_shares: self.verifying_shares.clone(),
            threshold: self.threshold,
        })
    }
    fn latest_epoch(&self) -> Option<u64> {
        Some(self.latest)
    }
}

fn se_oracle_for(committee: &SeCommittee, latest: u64) -> Arc<SeMockOracle> {
    let mut verifying_shares = BTreeMap::new();
    for (i, addr) in committee.members.iter().enumerate() {
        verifying_shares.insert(
            *addr,
            compress_point(&committee.verifying_shares[&((i + 1) as u16)]),
        );
    }
    Arc::new(SeMockOracle {
        joint_verifying_key: compress_point(&committee.joint_y),
        verifying_shares,
        threshold: committee.threshold,
        latest,
    })
}

/// Real se-mode encrypted ballot (NIZK-proven) bound to `poll_id`.
fn build_se_ballot(
    committee: &SeCommittee,
    poll_id: PollId,
    scores: &[u64],
) -> RatificationBallotPayload {
    let r_scores: Vec<Scalar> = (0..scores.len())
        .map(|_| Scalar::random(&mut OsRng))
        .collect();
    let (bundle, cs, ci) =
        prove_ballot_bundle_with_outputs_with_score_nonces(&committee.joint_y, scores, &r_scores);
    RatificationBallotPayload {
        poll_id,
        scores: None,
        ciphertexts_scores: Some(cs),
        ciphertexts_indicators: Some(ci),
        proof: Some(BallotNIZKProof {
            range_proofs: bundle.range_proofs,
            consistency_proofs: bundle.consistency_proofs,
        }),
    }
}

/// Real kd=ts payload: per-aggregate partial decryption shares + DLEQ proofs
/// for committee member `member_idx`, computed against the homomorphic
/// aggregate of `ballots` (n score-sum + C(n,2) indicator aggregates).
fn build_se_ts_payload(
    committee: &SeCommittee,
    ballots: &[RatificationBallotPayload],
    member_idx: usize,
    poll_id: PollId,
    n: usize,
    epoch: u64,
) -> TallySharePayload {
    let aggregates = aggregate_se_ballots(ballots, n).expect("aggregate_se_ballots");
    let frost_id = (member_idx + 1) as u16;
    let x_i = committee.shares[&frost_id];
    let y_i = committee.verifying_shares[&frost_id];
    let g = RISTRETTO_BASEPOINT_POINT;
    let entries: Vec<TallyShareEntry> = aggregates
        .iter()
        .map(|agg| {
            let c1_agg = decompress_point(&agg.c1).expect("c1 decompress");
            let share_pt = partial_decrypt_share(&c1_agg, &x_i);
            let proof = dleq_prove(&g, &y_i, &c1_agg, &share_pt, &x_i);
            TallyShareEntry {
                share: compress_point(&share_pt),
                dleq_proof: proof.to_bytes(),
            }
        })
        .collect();
    TallySharePayload {
        poll_id,
        committee_epoch: epoch,
        entries,
    }
}

#[tokio::test]
async fn ipc_tier3_engine_auto_se_mode_two_engine_finalize() {
    const COMMUNITY_ID: SpaceId = SpaceId([0xE5; 16]);
    // se-mode config dw=fw=rw=60s (PollCreate validation requires ≥60) →
    // ratification window [120_000, 180_000] ms with create at wall=0.
    const RATIFICATION_OPEN_MS: u64 = 120_000;
    const RATIFICATION_END_MS: u64 = 180_000;
    // Committee kd=ts land AFTER the close carrying per-replica wall-clock HLCs
    // — the normal production arrangement (kd=ts.wall > close.wall). This is the
    // case the pre-C1-fix close-anchored se-mode kd=rs could NOT finalize
    // (the deterministic kd=rs at close.wall is non-monotonic once a kd=ts at a
    // higher wall has been applied). staggered per member to mirror per-replica
    // wall-clock arrival.
    const TS_WALL_BASE_MS: u64 = RATIFICATION_END_MS + 5_000; // 185_000 > close
    const N_TOTAL: usize = 3; // 2 explicit candidates + status_quo
    const N: usize = N_TOTAL;

    // engine_a = relay (no signing); engine_b = signer (finalizer).
    let engines = setup_two_voting_engine_bridge(COMMUNITY_ID).await;

    // 3-member committee from real identities, sorted ascending so FROST id
    // order matches the oracle's BTreeMap iteration order.
    let mut committee_ids: Vec<TestIdentity> = (100u8..103).map(fixture_identity).collect();
    committee_ids.sort_by_key(|id| id.owner);
    let member_addrs: Vec<OwnerAddr> = committee_ids.iter().map(|id| id.owner).collect();
    let committee = build_se_committee(&member_addrs, 2);

    // The proposer signs the seeded kd=cl and — on engine_b — the auto-minted
    // kd=rs. Distinct from the committee; kd=rs is a public result event.
    let proposer = fixture_identity(200);

    // Register every actor whose signed events cross the bridge: kd=ts from
    // committee members, kd=rs from the proposer. (Ballots / kd=ss / kd=cl are
    // injected directly into both logs and never verified.)
    for id in &committee_ids {
        engines.resolvers.add_identity(id);
    }
    engines.resolvers.add_identity(&proposer);

    // engine_b alone holds the proposer's signing key.
    engines
        .engine_b
        .install_local_signing_key(Arc::new(proposer.signing_key.clone()), proposer.owner)
        .await;

    let config = Tier3PollConfigPayload {
        proposal_text: "se-mode two-engine determinism".into(),
        sortition_size: 20,
        deliberation_window_seconds: 60,
        drafting_window_seconds: 60,
        ratification_window_seconds: 60,
        privacy_mode: "se".into(),
        incentive_mode: "d".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
    };
    // Create at wall=0 so ratification_end = 180_000.
    let create_event = build_tier3_poll_create_event(&proposer, &config, hlc_at(0, "proposer"));
    let poll_id = derive_poll_id(
        &COMMUNITY_ID,
        &create_event.signing_bytes().expect("signing_bytes"),
    );

    // Electorate = the committee.
    let snapshot = MembershipSnapshot {
        members: member_addrs
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

    // Pre-build the shared setup events ONCE so both logs reach byte-identical
    // pre-finalize state: kd=ss (leave Sortition), real se ballots, kd=cl
    // (seeds close_event_hash).
    let ss_event = build_sortition_selection_event(
        &proposer,
        poll_id,
        member_addrs.clone(),
        vec![],
        hlc_at(10, "ss"),
    );
    // c0 wins clearly; scores [c0, c1, status_quo] per voter.
    let ballot_scores: [[u64; N]; 3] = [[5, 1, 0], [5, 2, 0], [5, 3, 0]];
    let ballots: Vec<RatificationBallotPayload> = ballot_scores
        .iter()
        .map(|s| build_se_ballot(&committee, poll_id, s))
        .collect();
    let ballot_events: Vec<SignedVotingEvent> = ballots
        .iter()
        .enumerate()
        .map(|(i, p)| {
            build_signed_ratification_ballot_payload(
                &committee_ids[i].signing_key,
                committee_ids[i].owner,
                p.clone(),
                hlc_at(RATIFICATION_OPEN_MS + i as u64, "rb"),
            )
            .expect("build_signed_ratification_ballot_payload")
        })
        .collect();
    let close_hlc = hlc_at(RATIFICATION_END_MS, "close");
    let cl_event = build_signed_poll_close_tier3(
        &proposer.signing_key,
        proposer.owner,
        poll_id,
        close_hlc.clone(),
    )
    .expect("build_signed_poll_close_tier3");

    // Synthetic candidate approvals: ceil(sortition_size/2) = 10 approvers each
    // so both candidates advance through drafting → n = 3.
    let approvals: std::collections::HashSet<OwnerAddr> =
        (0..10u8).map(|i| OwnerAddr([0xA0 | i; 16])).collect();

    // Seed BOTH logs identically.
    for log in [&engines.log_a, &engines.log_b] {
        let mut g = log.lock().await;
        g.apply_with_snapshot(create_event.clone(), &COMMUNITY_ID, Some(snapshot.clone()))
            .expect("PollCreate apply");
        let t3 = g
            .polls
            .get_mut(&poll_id)
            .and_then(|ps| ps.tier_state.as_tier3_mut())
            .expect("tier3 state");
        t3.install_committee_oracle(se_oracle_for(&committee, 0));
        for i in 0..(N_TOTAL - 1) {
            t3.candidates.push(DraftCandidateState {
                event_hash: [0xC0 | (i as u8); 32],
                text: format!("candidate {i}"),
                proposer: Some(member_addrs[0]),
                approvals: approvals.clone(),
            });
        }
        t3.apply_event(&ss_event).expect("kd=ss apply");
        for rb in &ballot_events {
            t3.apply_event(rb).expect("kd=rb apply");
        }
        t3.apply_event(&cl_event).expect("kd=cl apply");
    }

    // Publish the 2 kd=ts shares via engine_a (the relay). engine_a self-applies
    // them (crossing the threshold in its own log, but never minting — no
    // signing key); the bridge forwards both to engine_b's inbound path.
    //
    // HLC ORDERING — the REALISTIC production case (ZEB-316 C1). Both kd=ts land
    // at walls STRICTLY GREATER than the close (`TS_WALL_BASE_MS > close.wall`),
    // staggered per member to mirror the per-replica wall-clock HLCs committee
    // members actually stamp (kd=ts is minted well after the ratification close).
    // The apply-time monotonic gate (`Tier3PollState::apply_event` /
    // `last_received_hlc`) rejects any event whose (wall, logical, device_id) is
    // strictly below the last applied — so after these kd=ts apply, the poll's
    // watermark sits above `close.wall`. A close-anchored se-mode kd=rs (the
    // pre-C1-fix behavior) would therefore be non-monotonic and REJECTED, and
    // engine_b would never finalize (the waits below would time out) — that is
    // this test's RED. The fix mints se-mode kd=rs on a wall-clock HLC
    // (`reserve_next_local_hlc`, real "now" ≫ these synthetic walls), which is
    // monotonically applicable; both replicas then converge on the same terminal
    // StarResult via Lagrange invariance + the apply-time LWW gate — GREEN.
    for member_idx in [0usize, 1] {
        let ts_payload = build_se_ts_payload(&committee, &ballots, member_idx, poll_id, N, 0);
        let ts_ev = build_signed_tally_share(
            &committee_ids[member_idx].signing_key,
            committee_ids[member_idx].owner,
            ts_payload,
            hlc_at(
                TS_WALL_BASE_MS + member_idx as u64 * 1_000,
                &format!("ts{member_idx}"),
            ),
        )
        .expect("build_signed_tally_share");
        engines
            .engine_a
            .publish_event(ts_ev, None)
            .await
            .expect("engine_a publish kd=ts");
    }

    // engine_b finalizes via the re-enabled inbound cascade; engine_a then
    // finalizes by applying engine_b's broadcast kd=rs.
    wait_for_log("engine_b: se Finalized", &engines.log_b, |log| {
        log.polls
            .get(&poll_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .map(|t3| t3.stage == Stage::Finalized)
            .unwrap_or(false)
    })
    .await
    .expect("engine_b must finalize the se poll via the inbound cascade");
    wait_for_log("engine_a: se Finalized", &engines.log_a, |log| {
        log.polls
            .get(&poll_id)
            .and_then(|ps| ps.tier_state.as_tier3())
            .map(|t3| t3.stage == Stage::Finalized)
            .unwrap_or(false)
    })
    .await
    .expect("engine_a must finalize via engine_b's broadcast kd=rs");

    let (t3_a, t3_b) = {
        let la = engines.log_a.lock().await;
        let lb = engines.log_b.lock().await;
        (
            la.polls[&poll_id]
                .tier_state
                .as_tier3()
                .expect("log_a tier3")
                .clone(),
            lb.polls[&poll_id]
                .tier_state
                .as_tier3()
                .expect("log_b tier3")
                .clone(),
        )
    };
    assert_eq!(t3_a.stage, Stage::Finalized);
    assert_eq!(t3_b.stage, Stage::Finalized);
    let result_a = t3_a.result.as_ref().expect("engine_a result");
    let result_b = t3_b.result.as_ref().expect("engine_b result");
    // Result-convergence is the load-bearing invariant for se-mode: the kd=rs
    // *result* (StarResult) is bit-identical across both engines via Lagrange
    // invariance in `recover_secret_tally` + the apply-time LWW gate. The kd=rs
    // *HLC* is intentionally NOT deterministic in se-mode (wall-clock mint —
    // ZEB-316 C1), so we deliberately do NOT assert byte-identity of the kd=rs
    // event / its HLC across engines; only the recovered result must converge.
    assert_eq!(
        result_a, result_b,
        "se-mode: bit-identical recovered StarResult across both engines"
    );

    drop(engines);
}
