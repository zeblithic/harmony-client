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
    build_signed_poll_create_tier3, build_signed_ratification_ballot,
    build_signed_sortition_selection, derive_poll_id, CandidateEventHash, Eligibility, MemberAttrs,
    MembershipSnapshot, PollId, SignedVotingEvent, Tier3PollConfigPayload, VotingIdentityResolver,
};
use harmony_app::community_voting_log::{
    MembershipSnapshotResolver, SnapshotResolverError, VotingLog,
};
use harmony_app::community_voting_log_engine::{VotingLogEngine, VotingLogEngineParams};
use harmony_app::community_voting_sortition::fisher_yates_select;
use harmony_app::community_voting_tier3::Stage;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use harmony_app::{add_dm_ipc_handlers, mock_context_with_full_acl, NodeState, LOCAL_IPC_URL};
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

    let a_hlc_tracker = Arc::new(Mutex::new(std::collections::BTreeMap::new()));
    let b_hlc_tracker = Arc::new(Mutex::new(std::collections::BTreeMap::new()));

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

    TwoVotingEngines {
        engine_a,
        engine_b,
        log_a,
        log_b,
        resolvers,
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
    let full_electorate: Vec<OwnerAddr> = identities.iter().map(|id| id.owner).collect();
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
        members: full_electorate
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
    let mut t_approval = t_drafting + 100;
    for actor in primary_ids.iter().take(10) {
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
    for actor in primary_ids.iter().take(10) {
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
    let full_electorate: Vec<OwnerAddr> = identities.iter().map(|id| id.owner).collect();
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
        members: full_electorate
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
/// kd=rs when their post-apply hook sees the ratification window expired.
/// First-by-HLC wins (per design spec §10 idempotency rule), and kd=rs
/// `result` payload is bit-identical because StarResult is deterministic
/// over the same ballot set.
#[tokio::test]
async fn ipc_tier3_engine_auto_kd_cl_kd_rs_race_tolerant() {
    const SORTITION_SIZE: u16 = 20;
    const N_IDENTITIES: usize = 50;
    const COMMUNITY_ID: SpaceId = SpaceId([0xF3; 16]);

    let identities: Vec<TestIdentity> = (0..N_IDENTITIES as u8).map(fixture_identity).collect();
    let proposer = &identities[49];
    let full_electorate: Vec<OwnerAddr> = identities.iter().map(|id| id.owner).collect();
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
        members: full_electorate
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
    // (t0 + total_window = t0 + 180_000). t0 + 500_000 leaves 320_000ms of
    // safety margin. BOTH engines apply kd=ss and BOTH fire engine-auto
    // kd=cl + kd=rs from their own apply hook. The HLC-driven "now" view
    // is the kd=ss HLC, so both engines see ratification as expired.
    let vrf_output: [u8; 32] = [0xF3; 32];
    let sortition_result = fisher_yates_select(
        &vrf_output,
        &sortition_pool,
        SORTITION_SIZE as usize,
        SORTITION_SIZE as usize,
    );
    // Register all identities so the receiving engine can verify their signatures.
    for id in &identities {
        engines.resolvers.add_identity(id);
    }

    let ss_hlc_wall = t0 + 500_000;
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

    // ZEB-316: the engine-auto kd=cl HLC is deterministic — derived purely
    // from the triggering kd=ss HLC, not wall-clock. Both replicas converge
    // on the same close_hlc.
    let expected_close_hlc = Hlc {
        wall_ms: ss_hlc_wall,
        logical: 1, // hlc_at(..) uses logical 0 → derived logical 1
        device_id: format!("engine-auto-cl-{}", hex::encode(&poll_id.0[..4])),
    };
    assert_eq!(
        t3_a.close_hlc.as_ref(),
        Some(&expected_close_hlc),
        "kd=cl HLC must be the deterministic derivation of the kd=ss trigger HLC"
    );
    assert_eq!(
        t3_a.close_hlc, t3_b.close_hlc,
        "both replicas converge on the same close_hlc"
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
    let full_electorate: Vec<OwnerAddr> = identities.iter().map(|id| id.owner).collect();
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
        members: full_electorate
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
