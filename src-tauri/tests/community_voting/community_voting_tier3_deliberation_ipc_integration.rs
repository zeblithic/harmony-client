//! ZEB-294: IPC integration tests for deliberation-stage projection fields in
//! `Tier3PollExport` and for `voting_list_bridging_statements_impl`.
//!
//! Harness pattern: mirrors `community_voting_tier3_get_ipc_integration.rs`.
//! Tests use Path C (build events via `build_signed_*` builders, apply via
//! `VotingLog::apply_with_snapshot`, then call `_impl` shims directly).
//!
//! The `voting_submit_deliberation_statement` and `voting_cast_deliberation_vote`
//! IPCs require a fully-wired NodeState (HLC tracker, signing key, engine, etc.)
//! and do not have `_impl`/`_raw` shims. We therefore test the projection fields
//! by building events with `build_signed_deliberation_{statement,vote}` and
//! applying them to `VotingLog`, then asserting the export via
//! `voting_get_tier3_poll_impl`.

#![cfg(feature = "test-fixtures")]

use std::sync::{Arc, Mutex as StdMutex};

use harmony_app::community_voting_core::{
    build_signed_deliberation_statement, build_signed_deliberation_vote,
    build_signed_poll_create_tier3, build_signed_sortition_selection, derive_poll_id, Eligibility,
    MemberAttrs, MembershipSnapshot, PollId, Tier3PollConfigPayload,
};
use harmony_app::community_voting_log::VotingLog;
use harmony_app::community_voting_tier3::event_hash_of;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use harmony_app::{
    voting_get_tier3_poll_impl, voting_list_bridging_statements_impl, BridgingScoreExport,
    NodeState, Tier3StageTag,
};
use tokio::sync::Mutex;

// ─── Identity helper ──────────────────────────────────────────────────────────

struct TestIdentity {
    pub owner: OwnerAddr,
    pub signing_key: ed25519_dalek::SigningKey,
}

fn fixture_identity(seed: u8) -> TestIdentity {
    let priv_id = harmony_identity::PrivateIdentity::from_seed(&[seed; 32]);
    let owner = OwnerAddr(priv_id.identity.address_hash);
    let private_bytes = priv_id.to_private_bytes();
    let mut ed_secret = [0u8; 32];
    ed_secret.copy_from_slice(&private_bytes[32..64]);
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_secret);
    TestIdentity { owner, signing_key }
}

fn hlc_at(wall_ms: u64) -> Hlc {
    Hlc {
        wall_ms,
        logical: 0,
        device_id: "test-dev".into(),
    }
}

// ─── Shared config ────────────────────────────────────────────────────────────

/// Minimum-valid Tier 3 config with a 3600s deliberation window.
/// Poll created at `t0=1_000_000ms`; deliberation ends at `t0 + 3_600_000ms`.
/// Statement/vote events must have HLC `wall_ms` in `[1_000_000, 4_600_000)`.
fn delib_config() -> Tier3PollConfigPayload {
    Tier3PollConfigPayload {
        proposal_text: "Deliberation test amendment".into(),
        sortition_size: 20,
        deliberation_window_seconds: 3600,
        drafting_window_seconds: 3600,
        ratification_window_seconds: 3600,
        privacy_mode: "pu".into(),
        incentive_mode: "d".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
        predecessor: None,
    }
}

fn all_member_snapshot(owners: &[OwnerAddr]) -> MembershipSnapshot {
    let members = owners
        .iter()
        .map(|o| {
            (
                *o,
                MemberAttrs {
                    power: 1,
                    vouching_depth: 0,
                },
            )
        })
        .collect();
    MembershipSnapshot { members }
}

// ─── Harness ─────────────────────────────────────────────────────────────────

/// Carries a NodeState Arc (for IPC _impl calls) and a direct log Arc (for
/// applying additional events without re-acquiring through NodeState's async
/// map). This avoids holding a StdMutex guard across async await points.
struct DeliberationHarness {
    pub state: Arc<StdMutex<NodeState>>,
    pub log: Arc<Mutex<VotingLog>>,
    pub poll_id_hex: String,
    #[allow(dead_code)]
    pub community_id: SpaceId,
}

impl DeliberationHarness {
    /// Build a NodeState with a single Tier 3 poll in the Deliberation stage:
    /// - kd=cr at `wall_ms=1_000_000`
    /// - kd=ss at `wall_ms=1_000_001`; primary = `primary_owners`, backup = []
    /// - `self_owner` is set on NodeState for `my_*` field projection
    async fn build(
        community_id: SpaceId,
        proposer: &TestIdentity,
        primary_owners: Vec<OwnerAddr>,
        self_owner: Option<OwnerAddr>,
    ) -> Self {
        let config = delib_config();
        let hlc_create = hlc_at(1_000_000);
        let event_create = build_signed_poll_create_tier3(
            &proposer.signing_key,
            proposer.owner,
            &config,
            hlc_create,
        )
        .expect("build kd=cr");
        let signing_bytes = event_create.signing_bytes().expect("signing_bytes");
        let poll_id = derive_poll_id(&community_id, &signing_bytes);

        // Snapshot includes proposer + all primary members.
        let all_owners: Vec<OwnerAddr> = std::iter::once(proposer.owner)
            .chain(primary_owners.iter().copied())
            .collect();
        let snapshot = all_member_snapshot(&all_owners);

        let mut log = VotingLog::new();
        log.apply_with_snapshot(event_create, &community_id, Some(snapshot))
            .expect("apply kd=cr");

        // kd=ss — mini-public established; effective stage = Deliberation.
        let ss_event = build_signed_sortition_selection(
            &proposer.signing_key,
            proposer.owner,
            poll_id,
            primary_owners,
            vec![], // no backup
            hlc_at(1_000_001),
        )
        .expect("build kd=ss");
        log.apply_with_snapshot(ss_event, &community_id, None)
            .expect("apply kd=ss");

        let log_arc = Arc::new(Mutex::new(log));
        let mut state = NodeState::default();
        if let Some(owner) = self_owner {
            state.set_test_self_owner(owner);
        }
        {
            let mut map = state.voting_logs.lock().await;
            map.insert(community_id, Arc::clone(&log_arc));
        }
        let state_arc = Arc::new(StdMutex::new(state));
        DeliberationHarness {
            state: state_arc,
            log: log_arc,
            poll_id_hex: hex::encode(poll_id.0),
            community_id,
        }
    }

    fn poll_id(&self) -> PollId {
        let bytes: [u8; 32] = hex::decode(&self.poll_id_hex)
            .unwrap()
            .try_into()
            .expect("32 bytes");
        PollId(bytes)
    }

    async fn get_export(&self) -> harmony_app::Tier3PollExport {
        voting_get_tier3_poll_impl(&self.state, self.poll_id_hex.clone())
            .await
            .expect("get_tier3_poll ok")
    }

    async fn list_bridging(&self, top_n: u16) -> Result<Vec<BridgingScoreExport>, String> {
        voting_list_bridging_statements_impl(&self.state, self.poll_id_hex.clone(), top_n).await
    }
}

// ─── Test 1: submit_statement_happy_path_emits_statement_in_export ────────────

/// Fixture: poll in Deliberation stage with `primary[0]` as self.
/// Apply a kd=ds event from `primary[0]`.
/// Assert: export has 1 statement, my_deliberation_statement_count == 1.
#[tokio::test]
async fn submit_statement_happy_path_emits_statement_in_export() {
    let community_id = SpaceId([0xE0; 16]);
    let proposer = fixture_identity(10);
    // 20 primary members; self = primary[0].
    let primary: Vec<TestIdentity> = (20u8..40).map(fixture_identity).collect();
    let primary_owners: Vec<OwnerAddr> = primary.iter().map(|id| id.owner).collect();
    let self_id = &primary[0];

    let h =
        DeliberationHarness::build(community_id, &proposer, primary_owners, Some(self_id.owner))
            .await;

    // Apply a kd=ds from self (primary[0]) within the deliberation window.
    let ds_event = build_signed_deliberation_statement(
        &self_id.signing_key,
        self_id.owner,
        h.poll_id(),
        "My test statement".into(),
        hlc_at(2_000_000), // within dw: [1_000_000, 4_600_000)
    )
    .expect("build kd=ds");

    h.log
        .lock()
        .await
        .apply_with_snapshot(ds_event, &community_id, None)
        .expect("apply kd=ds");

    let export = h.get_export().await;

    assert_eq!(
        export.deliberation_statements.len(),
        1,
        "export must expose the accepted statement"
    );
    assert_eq!(
        export.my_deliberation_statement_count, 1,
        "my_deliberation_statement_count must be 1 after accepting one statement"
    );
    let stmt = &export.deliberation_statements[0];
    assert_eq!(stmt.text, "My test statement");
    assert_eq!(stmt.agree_count, 0);
    assert_eq!(stmt.disagree_count, 0);
    assert_eq!(stmt.pass_count, 0);
}

// ─── Test 2: submit_statement_rejects_observer_at_apply ──────────────────────

/// An actor NOT in the mini-public who submits a kd=ds event is silently dropped
/// at apply time. The export should show 0 statements.
#[tokio::test]
async fn submit_statement_rejects_observer_at_apply() {
    let community_id = SpaceId([0xE1; 16]);
    let proposer = fixture_identity(11);
    let primary: Vec<TestIdentity> = (20u8..40).map(fixture_identity).collect();
    let primary_owners: Vec<OwnerAddr> = primary.iter().map(|id| id.owner).collect();
    // observer: NOT in the primary pool.
    let observer = fixture_identity(99);

    let h = DeliberationHarness::build(
        community_id,
        &proposer,
        primary_owners,
        Some(observer.owner),
    )
    .await;

    // Build kd=ds from observer (not in mini-public).
    let ds_event = build_signed_deliberation_statement(
        &observer.signing_key,
        observer.owner,
        h.poll_id(),
        "Observer statement (should be dropped)".into(),
        hlc_at(2_000_000),
    )
    .expect("build kd=ds");

    // apply returns Ok (silent drop per spec §2.3).
    h.log
        .lock()
        .await
        .apply_with_snapshot(ds_event, &community_id, None)
        .expect("apply returns Ok even for dropped events");

    let export = h.get_export().await;

    assert!(
        export.deliberation_statements.is_empty(),
        "observer's statement must be silently dropped at apply time; \
         got {} statements",
        export.deliberation_statements.len()
    );
    assert_eq!(
        export.my_deliberation_statement_count, 0,
        "observer's count must remain 0"
    );
}

// ─── Test 3: submit_statement_rejects_6th_from_same_author ───────────────────

/// After 5 accepted statements from the same actor, a 6th is silently dropped
/// (spam cap). The export must show exactly 5 statements.
#[tokio::test]
async fn submit_statement_rejects_6th_from_same_author() {
    let community_id = SpaceId([0xE2; 16]);
    let proposer = fixture_identity(12);
    let primary: Vec<TestIdentity> = (20u8..40).map(fixture_identity).collect();
    let primary_owners: Vec<OwnerAddr> = primary.iter().map(|id| id.owner).collect();
    let author = &primary[0];

    let h = DeliberationHarness::build(community_id, &proposer, primary_owners, Some(author.owner))
        .await;

    let poll_id = h.poll_id();

    // Apply 6 kd=ds events; only 5 should be accepted.
    let mut log = h.log.lock().await;
    for i in 0u64..6 {
        let ds_event = build_signed_deliberation_statement(
            &author.signing_key,
            author.owner,
            poll_id,
            format!("Statement #{}", i + 1),
            hlc_at(2_000_000 + i),
        )
        .expect("build kd=ds");
        log.apply_with_snapshot(ds_event, &community_id, None)
            .expect("apply returns Ok");
    }
    drop(log);

    let export = h.get_export().await;

    assert_eq!(
        export.deliberation_statements.len(),
        5,
        "exactly 5 statements must be accepted; 6th is spam-capped"
    );
    assert_eq!(
        export.my_deliberation_statement_count, 5,
        "my_deliberation_statement_count must be capped at 5"
    );
}

// ─── Test 4: cast_vote_revote_lww_updates_export ──────────────────────────────

/// Voter casts agree, then revotes disagree (LWW). The export must show:
/// - my_deliberation_votes has one entry with vote == "disagree"
/// - deliberation_statements[0] has agree_count=0, disagree_count=1
#[tokio::test]
async fn cast_vote_revote_lww_updates_export() {
    let community_id = SpaceId([0xE3; 16]);
    let proposer = fixture_identity(13);
    let primary: Vec<TestIdentity> = (20u8..40).map(fixture_identity).collect();
    let primary_owners: Vec<OwnerAddr> = primary.iter().map(|id| id.owner).collect();
    // author submits statement; voter (different primary member) votes on it.
    let author = &primary[0];
    let voter = &primary[1];

    let h = DeliberationHarness::build(
        community_id,
        &proposer,
        primary_owners,
        Some(voter.owner), // self = voter
    )
    .await;

    let poll_id = h.poll_id();

    // Apply a statement from author, then two votes from voter.
    let ds_event = build_signed_deliberation_statement(
        &author.signing_key,
        author.owner,
        poll_id,
        "Statement to vote on".into(),
        hlc_at(2_000_000),
    )
    .expect("build kd=ds");
    let stmt_hash: [u8; 32] = event_hash_of(&ds_event);

    let mut log = h.log.lock().await;
    log.apply_with_snapshot(ds_event, &community_id, None)
        .expect("apply kd=ds");

    // Voter casts "agree" first.
    let dv_agree = build_signed_deliberation_vote(
        &voter.signing_key,
        voter.owner,
        poll_id,
        stmt_hash,
        harmony_app::community_voting_core::BridgingVoteCode::Agree,
        hlc_at(2_000_001),
    )
    .expect("build kd=dv agree");
    log.apply_with_snapshot(dv_agree, &community_id, None)
        .expect("apply kd=dv agree");

    // Voter revotes "disagree" (LWW — higher HLC wins).
    let dv_disagree = build_signed_deliberation_vote(
        &voter.signing_key,
        voter.owner,
        poll_id,
        stmt_hash,
        harmony_app::community_voting_core::BridgingVoteCode::Disagree,
        hlc_at(2_000_002),
    )
    .expect("build kd=dv disagree");
    log.apply_with_snapshot(dv_disagree, &community_id, None)
        .expect("apply kd=dv disagree");
    drop(log);

    let export = h.get_export().await;

    // My votes: one entry with vote == "disagree".
    assert_eq!(
        export.my_deliberation_votes.len(),
        1,
        "LWW must collapse to exactly one vote entry per (voter, stmt)"
    );
    assert_eq!(
        export.my_deliberation_votes[0].vote, "disagree",
        "revote must supersede the original agree"
    );
    assert_eq!(
        export.my_deliberation_votes[0].statement_event_hash,
        hex::encode(stmt_hash)
    );

    // Aggregate counts on the statement.
    assert_eq!(
        export.deliberation_statements.len(),
        1,
        "one statement must be visible"
    );
    let stmt = &export.deliberation_statements[0];
    assert_eq!(stmt.agree_count, 0, "agree_count must be 0 after revote");
    assert_eq!(
        stmt.disagree_count, 1,
        "disagree_count must be 1 after revote"
    );
    assert_eq!(stmt.pass_count, 0);
}

// ─── Test 5: cast_vote_rejects_non_existent_statement_target ─────────────────

/// A kd=dv event targeting a non-existent statement hash is silently dropped.
/// The export must show no votes.
#[tokio::test]
async fn cast_vote_rejects_non_existent_statement_target() {
    let community_id = SpaceId([0xE4; 16]);
    let proposer = fixture_identity(14);
    let primary: Vec<TestIdentity> = (20u8..40).map(fixture_identity).collect();
    let primary_owners: Vec<OwnerAddr> = primary.iter().map(|id| id.owner).collect();
    let voter = &primary[0];

    let h = DeliberationHarness::build(community_id, &proposer, primary_owners, Some(voter.owner))
        .await;

    let poll_id = h.poll_id();
    let fake_stmt_hash: [u8; 32] = [0xDE; 32]; // does not exist

    let dv_event = build_signed_deliberation_vote(
        &voter.signing_key,
        voter.owner,
        poll_id,
        fake_stmt_hash,
        harmony_app::community_voting_core::BridgingVoteCode::Agree,
        hlc_at(2_000_001),
    )
    .expect("build kd=dv");

    // Silent drop: apply returns Ok.
    h.log
        .lock()
        .await
        .apply_with_snapshot(dv_event, &community_id, None)
        .expect("apply returns Ok even for non-existent target");

    let export = h.get_export().await;

    assert!(
        export.my_deliberation_votes.is_empty(),
        "vote targeting non-existent statement must be silently dropped"
    );
    assert!(
        export.deliberation_statements.is_empty(),
        "no statements were added"
    );
}

// ─── Test 6: list_bridging_returns_sorted_desc_by_score ──────────────────────

/// Two statements with votes. `voting_list_bridging_statements_impl` must return
/// results sorted by `bridging_score_q64` DESC.
///
/// Setup: 20 primary members. Author A and B each submit one statement.
/// Votes on B from all 18 non-author primary members (unanimous agree = high
/// bridging score). Votes on A from only 9 members (half agree, half disagree =
/// lower bridging score due to polarization).
#[tokio::test]
async fn list_bridging_returns_sorted_desc_by_score() {
    let community_id = SpaceId([0xE5; 16]);
    let proposer = fixture_identity(15);

    // 20 primary members (satisfies sortition_size=20 minimum).
    let primary: Vec<TestIdentity> = (20u8..40).map(fixture_identity).collect();
    let primary_owners: Vec<OwnerAddr> = primary.iter().map(|id| id.owner).collect();

    let h = DeliberationHarness::build(community_id, &proposer, primary_owners, None).await;
    let poll_id = h.poll_id();

    let mut log = h.log.lock().await;

    // Statement A from primary[0].
    let ds_a = build_signed_deliberation_statement(
        &primary[0].signing_key,
        primary[0].owner,
        poll_id,
        "Statement A (polarized)".into(),
        hlc_at(2_000_000),
    )
    .expect("build kd=ds A");
    let hash_a: [u8; 32] = event_hash_of(&ds_a);
    log.apply_with_snapshot(ds_a, &community_id, None)
        .expect("apply kd=ds A");

    // Statement B from primary[1].
    let ds_b = build_signed_deliberation_statement(
        &primary[1].signing_key,
        primary[1].owner,
        poll_id,
        "Statement B (broadly agreeable)".into(),
        hlc_at(2_000_001),
    )
    .expect("build kd=ds B");
    let hash_b: [u8; 32] = event_hash_of(&ds_b);
    log.apply_with_snapshot(ds_b, &community_id, None)
        .expect("apply kd=ds B");

    // Votes on A: 9 agree + 9 disagree (polarized).
    let voters = &primary[2..]; // 18 voters
    let half = voters.len() / 2;
    for (i, voter) in voters[..half].iter().enumerate() {
        let dv = build_signed_deliberation_vote(
            &voter.signing_key,
            voter.owner,
            poll_id,
            hash_a,
            harmony_app::community_voting_core::BridgingVoteCode::Agree,
            hlc_at(2_001_000 + i as u64),
        )
        .expect("build dv agree A");
        log.apply_with_snapshot(dv, &community_id, None)
            .expect("apply dv agree A");
    }
    for (i, voter) in voters[half..].iter().enumerate() {
        let dv = build_signed_deliberation_vote(
            &voter.signing_key,
            voter.owner,
            poll_id,
            hash_a,
            harmony_app::community_voting_core::BridgingVoteCode::Disagree,
            hlc_at(2_002_000 + i as u64),
        )
        .expect("build dv disagree A");
        log.apply_with_snapshot(dv, &community_id, None)
            .expect("apply dv disagree A");
    }

    // Votes on B: all 18 voters agree (broadly agreeable).
    for (i, voter) in voters.iter().enumerate() {
        let dv = build_signed_deliberation_vote(
            &voter.signing_key,
            voter.owner,
            poll_id,
            hash_b,
            harmony_app::community_voting_core::BridgingVoteCode::Agree,
            hlc_at(2_003_000 + i as u64),
        )
        .expect("build dv agree B");
        log.apply_with_snapshot(dv, &community_id, None)
            .expect("apply dv agree B");
    }
    drop(log);

    let results = h.list_bridging(10).await.expect("list bridging ok");

    assert_eq!(results.len(), 2, "both statements must appear");

    // Results must be sorted DESC by bridging_score_q64.
    let score_0 = results[0].bridging_score_q64.parse::<u64>().expect("u64");
    let score_1 = results[1].bridging_score_q64.parse::<u64>().expect("u64");
    assert!(
        score_0 >= score_1,
        "results must be sorted DESC by bridging_score_q64: [{score_0} >= {score_1}]"
    );
}

// ─── Test 7: list_bridging_rejects_sortition_stage ───────────────────────────

/// Calling `voting_list_bridging_statements_impl` for a poll in Sortition stage
/// (kd=ss not yet applied) must return an error mentioning the stage rejection.
#[tokio::test]
async fn list_bridging_rejects_sortition_stage() {
    let community_id = SpaceId([0xE6; 16]);
    let proposer = fixture_identity(16);

    let config = delib_config();
    let hlc_create = hlc_at(1_000_000);
    let event_create =
        build_signed_poll_create_tier3(&proposer.signing_key, proposer.owner, &config, hlc_create)
            .expect("build kd=cr");
    let signing_bytes = event_create.signing_bytes().expect("signing_bytes");
    let poll_id = derive_poll_id(&community_id, &signing_bytes);
    let poll_id_hex = hex::encode(poll_id.0);

    let snapshot = all_member_snapshot(&[proposer.owner]);
    let mut log = VotingLog::new();
    log.apply_with_snapshot(event_create, &community_id, Some(snapshot))
        .expect("apply kd=cr");

    // No kd=ss applied → effective stage = Sortition.
    let log_arc = Arc::new(Mutex::new(log));
    let state = NodeState::default();
    {
        let mut map = state.voting_logs.lock().await;
        map.insert(community_id, Arc::clone(&log_arc));
    }
    let state_arc = Arc::new(StdMutex::new(state));

    // Sanity: export confirms Sortition stage.
    let export = voting_get_tier3_poll_impl(&state_arc, poll_id_hex.clone())
        .await
        .expect("get ok");
    assert_eq!(
        export.stage,
        Tier3StageTag::Sortition,
        "stage must be Sortition before kd=ss"
    );

    // list_bridging must error for Sortition stage.
    let err = voting_list_bridging_statements_impl(&state_arc, poll_id_hex, 10)
        .await
        .unwrap_err();
    assert!(
        err.contains("stage") || err.contains("Sortition") || err.contains("not available"),
        "error must mention stage rejection; got: {err}"
    );
}
