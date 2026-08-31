//! ZEB-294 Task 8 — multi-engine bridging-determinism acceptance test.
//!
//! ## What this test proves
//!
//! Spec §3 + §5 + §4.7: two voting projections fed the same Tier 3
//! deliberation event stream produce **bitwise-identical**
//! `compute_bridging_scores` output. This is the load-bearing
//! determinism property — if it fails, the algorithm is not a pure
//! function of `(statements, votes, mini_public)` and CRDT convergence
//! on bridging output is broken.
//!
//! ## Variant chosen: same-process two-`VotingLog` projection
//!
//! Considered alternatives:
//!
//! 1. **Two real `VotingLogEngine` instances over real Zenoh**. The
//!    end-to-end Zenoh transport for voting events is already covered
//!    by `community_voting_zenoh_integration.rs` (peer-to-peer Tier 1
//!    PollCreate flow). Reproducing that wiring for Tier 3 deliberation
//!    would require D-FROST registry setup (PollCreate path), identity
//!    resolvers, membership resolvers, and warm-up sentinel packets —
//!    over 500 LOC of fixture for a property that is independent of
//!    transport.
//!
//! 2. **Two engines bridged via mpsc** (the
//!    `community_dfrost_transport_integration.rs` pattern). Mirrors the
//!    sibling DKG-convergence test, but the engine boundary doesn't
//!    add coverage that the same-process variant lacks: `process_inbound`
//!    delegates to the same `VotingLog::apply_with_snapshot` /
//!    `Tier3PollState::apply_event` path that the same-process test
//!    drives directly.
//!
//! 3. **(Chosen) Same-process two `VotingLog` instances**. Builds the
//!    deliberation event stream once, applies it to two independent
//!    `VotingLog` projections in the same HLC-monotonic order (Tier 3
//!    `apply_event` enforces strict monotonicity, so "different arrival
//!    orders" is moot — both engines MUST converge through the same
//!    sequence), then calls `compute_bridging_scores` on each and
//!    compares every integer field bitwise.
//!
//! The same-process variant directly verifies spec §4.7's determinism
//! guarantee (pure integer Q32/Q64 arithmetic; no `f64` in the sort
//! path) and is robust against CI environment variation.
//!
//! ## Bitwise-identical assertion
//!
//! The test compares only integer fields — `bridging_score_q64` (u64),
//! `diversity_q32` (u64), `agree_count` / `disagree_count` /
//! `pass_count` (u16), `statement_event_hash` ([u8; 32]), `author`
//! (OwnerAddr). No `as f64` anywhere. A divergence in any of these
//! fields would catch Q32/Q64 arithmetic drift between projections —
//! the failure mode that would break CRDT convergence on bridging
//! output.

#![cfg(feature = "test-fixtures")]

use harmony_app::community_voting_core::{
    build_signed_deliberation_statement, build_signed_deliberation_vote,
    build_signed_poll_create_tier3, build_signed_sortition_selection, derive_poll_id,
    BridgingVoteCode, Eligibility, MemberAttrs, MembershipSnapshot, PollId, Tier3PollConfigPayload,
};
use harmony_app::community_voting_log::VotingLog;
use harmony_app::community_voting_sortition::bridging::{compute_bridging_scores, BridgingScore};
use harmony_app::community_voting_tier3::event_hash_of;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

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
/// Poll created at `t0 = 1_000_000ms`; deliberation ends at `t0 + 3_600_000ms`.
/// Statement/vote events use HLC `wall_ms` in `[1_000_000, 4_600_000)`.
fn delib_config() -> Tier3PollConfigPayload {
    Tier3PollConfigPayload {
        proposal_text: "Multi-engine convergence test amendment".into(),
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
        ce: None,
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

// ─── Convergence helper ──────────────────────────────────────────────────────

/// Apply the given event sequence to a fresh `VotingLog` and return the
/// `Vec<BridgingScore>` computed by `compute_bridging_scores`. The
/// PollCreate event must be at index 0 and is applied with the supplied
/// membership snapshot; subsequent events are applied with `None`.
fn build_log_and_compute(
    community_id: SpaceId,
    snapshot: MembershipSnapshot,
    events: &[harmony_app::community_voting_core::SignedVotingEvent],
    poll_id: PollId,
) -> Vec<BridgingScore> {
    let mut log = VotingLog::new();
    // PollCreate must be the first event in the supplied sequence so the
    // poll state exists before subsequent events reference it.
    log.apply_with_snapshot(events[0].clone(), &community_id, Some(snapshot))
        .expect("apply kd=cr");
    for ev in events.iter().skip(1) {
        log.apply_with_snapshot(ev.clone(), &community_id, None)
            .expect("apply non-create event");
    }

    let poll_state = log
        .polls
        .get(&poll_id)
        .expect("poll present after apply sequence");
    let t3 = poll_state.tier_state.as_tier3().expect("poll is Tier 3");
    let eval_hlc = t3
        .last_hlc
        .clone()
        .expect("last_hlc present after at least one apply");

    let (stmts, votes, mini_public) = t3.bridging_inputs(&eval_hlc);
    compute_bridging_scores(stmts, votes, &mini_public)
}

// ─── The test ─────────────────────────────────────────────────────────────────

/// Two independent `VotingLog` projections, fed the same Tier 3
/// deliberation event stream, must produce bitwise-identical
/// `compute_bridging_scores` output. Verifies spec §4.7 determinism
/// guarantees + acceptance criteria §3 + §5.
///
/// Event sequence (HLC-monotonic):
///   1. kd=cr (PollCreate)            — proposer at t=1_000_000
///   2. kd=ss (SortitionSelection)    — proposer at t=1_000_001; mini-public established
///   3. kd=ds (DeliberationStatement) — author A at t=2_000_000
///   4. kd=ds (DeliberationStatement) — author B at t=2_000_001
///   5. kd=dv (DeliberationVote)      — voter X on stmt A: Agree    at t=2_010_000
///   6. kd=dv (DeliberationVote)      — voter X on stmt B: Agree    at t=2_010_001
///   7. kd=dv (DeliberationVote)      — voter Y on stmt A: Disagree at t=2_010_002
///   8. kd=dv (DeliberationVote)      — voter Y on stmt B: Agree    at t=2_010_003
///
/// Voter X agrees with both statements; voter Y agrees with B but
/// disagrees with A. This gives stmt B 2 supporters with non-trivial
/// pairwise dissimilarity (because the same voters voted differently
/// on stmt A), exercising the diversity-of-supporters arithmetic
/// (Q32 dissimilarity → Q64 bridging score) end-to-end.
///
/// The convergence assertion compares only integer fields. No `as f64`
/// anywhere in the assertion path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_engines_converge_on_identical_bridging_output() {
    let community_id = SpaceId([0xC0; 16]);

    // 20 primary members (satisfies sortition_size = 20). Proposer is
    // outside the mini-public; primary[0..2] author statements;
    // primary[2..4] cast votes.
    let proposer = fixture_identity(0xA0);
    let primary: Vec<TestIdentity> = (0xB0u8..0xC4).map(fixture_identity).collect();
    let primary_owners: Vec<OwnerAddr> = primary.iter().map(|id| id.owner).collect();
    let author_a = &primary[0];
    let author_b = &primary[1];
    let voter_x = &primary[2];
    let voter_y = &primary[3];

    // Build the event sequence ONCE — both projections apply the same
    // bytes-identical events.
    let snapshot_owners: Vec<OwnerAddr> = std::iter::once(proposer.owner)
        .chain(primary_owners.iter().copied())
        .collect();
    let snapshot = all_member_snapshot(&snapshot_owners);

    let config = delib_config();
    let hlc_create = hlc_at(1_000_000);
    let event_cr =
        build_signed_poll_create_tier3(&proposer.signing_key, proposer.owner, &config, hlc_create)
            .expect("build kd=cr");
    let signing_bytes = event_cr.signing_bytes().expect("signing_bytes");
    let poll_id = derive_poll_id(&community_id, &signing_bytes);

    let event_ss = build_signed_sortition_selection(
        &proposer.signing_key,
        proposer.owner,
        poll_id,
        primary_owners.clone(),
        vec![], // no backup
        hlc_at(1_000_001),
    )
    .expect("build kd=ss");

    let event_ds_a = build_signed_deliberation_statement(
        &author_a.signing_key,
        author_a.owner,
        poll_id,
        "Statement A — proposal extension".into(),
        hlc_at(2_000_000),
    )
    .expect("build kd=ds A");
    let hash_a = event_hash_of(&event_ds_a);

    let event_ds_b = build_signed_deliberation_statement(
        &author_b.signing_key,
        author_b.owner,
        poll_id,
        "Statement B — clarification".into(),
        hlc_at(2_000_001),
    )
    .expect("build kd=ds B");
    let hash_b = event_hash_of(&event_ds_b);

    // Voter X: Agree on both A and B.
    let event_dv_xa = build_signed_deliberation_vote(
        &voter_x.signing_key,
        voter_x.owner,
        poll_id,
        hash_a,
        BridgingVoteCode::Agree,
        hlc_at(2_010_000),
    )
    .expect("build kd=dv X→A");
    let event_dv_xb = build_signed_deliberation_vote(
        &voter_x.signing_key,
        voter_x.owner,
        poll_id,
        hash_b,
        BridgingVoteCode::Agree,
        hlc_at(2_010_001),
    )
    .expect("build kd=dv X→B");

    // Voter Y: Disagree on A, Agree on B.
    let event_dv_ya = build_signed_deliberation_vote(
        &voter_y.signing_key,
        voter_y.owner,
        poll_id,
        hash_a,
        BridgingVoteCode::Disagree,
        hlc_at(2_010_002),
    )
    .expect("build kd=dv Y→A");
    let event_dv_yb = build_signed_deliberation_vote(
        &voter_y.signing_key,
        voter_y.owner,
        poll_id,
        hash_b,
        BridgingVoteCode::Agree,
        hlc_at(2_010_003),
    )
    .expect("build kd=dv Y→B");

    let events = vec![
        event_cr,
        event_ss,
        event_ds_a,
        event_ds_b,
        event_dv_xa,
        event_dv_xb,
        event_dv_ya,
        event_dv_yb,
    ];

    // ── Project A: build log + compute ────────────────────────────────────
    let scores_a = build_log_and_compute(community_id, snapshot.clone(), &events, poll_id);

    // ── Project B: independent log + compute (same event sequence) ────────
    let scores_b = build_log_and_compute(community_id, snapshot, &events, poll_id);

    // ── Bitwise-identical assertion ───────────────────────────────────────
    //
    // Sanity: both projections must produce the same NUMBER of scores
    // (one per accepted statement).
    assert_eq!(
        scores_a.len(),
        scores_b.len(),
        "score-vec length divergence: A has {}, B has {}",
        scores_a.len(),
        scores_b.len()
    );
    assert_eq!(
        scores_a.len(),
        2,
        "both statements (A, B) must produce one score each; got {}",
        scores_a.len()
    );

    for (i, (sa, sb)) in scores_a.iter().zip(scores_b.iter()).enumerate() {
        assert_eq!(
            sa.statement_event_hash, sb.statement_event_hash,
            "score[{i}].statement_event_hash divergence"
        );
        assert_eq!(
            sa.author, sb.author,
            "score[{i}].author divergence (hash {:?})",
            sa.statement_event_hash
        );
        assert_eq!(
            sa.agree_count, sb.agree_count,
            "score[{i}].agree_count divergence"
        );
        assert_eq!(
            sa.disagree_count, sb.disagree_count,
            "score[{i}].disagree_count divergence"
        );
        assert_eq!(
            sa.pass_count, sb.pass_count,
            "score[{i}].pass_count divergence"
        );
        assert_eq!(
            sa.diversity_q32, sb.diversity_q32,
            "score[{i}].diversity_q32 divergence: spec §4.7 broken — \
             Q32 dissimilarity arithmetic is not deterministic across projections"
        );
        assert_eq!(
            sa.bridging_score_q64, sb.bridging_score_q64,
            "score[{i}].bridging_score_q64 divergence: spec §4.7 broken — \
             Q64 bridging-score arithmetic is not deterministic across projections"
        );
        // `statement_text` is a String but is derived from the same
        // event payload bytes that produced `statement_event_hash`;
        // include it in the assertion for defense-in-depth against
        // accidental projection-side mutation.
        assert_eq!(
            sa.statement_text, sb.statement_text,
            "score[{i}].statement_text divergence"
        );
    }

    // ── Substantive: the scenario must actually exercise non-trivial
    // ── bridging math (otherwise the assertion is vacuously true for
    // ── two empty/zero-only outputs).
    //
    // Statement B has 2 agree-supporters (X and Y) who disagree on
    // statement A. Their pairwise dissimilarity on A is 1/1 = 1.0
    // (joint_support=1 vote pair on A, disagreement=1), so
    // diversity_q32 = 2^32, agree_count = 2, bridging_score_q64 =
    // 2 × 2^32. Statement A has 1 agree (X) + 1 disagree (Y); 1
    // supporter means diversity_q32 = 0 and bridging_score_q64 = 0.
    //
    // The deterministic sort places B first (DESC by score).
    // Assert the publicly contracted ordering — (bridging_score_q64 DESC,
    // statement_event_hash ASC) — directly on both projections. Without this,
    // a regression that flips both projections to the same wrong order would
    // still pass the per-hash content checks below.
    for scores in [&scores_a, &scores_b] {
        for pair in scores.windows(2) {
            let lhs = &pair[0];
            let rhs = &pair[1];
            assert!(
                lhs.bridging_score_q64 > rhs.bridging_score_q64
                    || (lhs.bridging_score_q64 == rhs.bridging_score_q64
                        && lhs.statement_event_hash <= rhs.statement_event_hash),
                "bridging output must be sorted (score DESC, hash ASC); \
                 violated at lhs=(score={}, hash={:?}) rhs=(score={}, hash={:?})",
                lhs.bridging_score_q64,
                lhs.statement_event_hash,
                rhs.bridging_score_q64,
                rhs.statement_event_hash,
            );
        }
    }
    // Specifically: B has a non-zero score, A has zero, so B must come first.
    assert_eq!(
        scores_a[0].statement_event_hash, hash_b,
        "statement B (higher score) must sort first"
    );
    assert_eq!(
        scores_a[1].statement_event_hash, hash_a,
        "statement A (zero score) must sort after B"
    );
    let stmt_b_score = scores_a
        .iter()
        .find(|s| s.statement_event_hash == hash_b)
        .expect("statement B in scores");
    assert_eq!(stmt_b_score.agree_count, 2, "stmt B: 2 supporters (X, Y)");
    assert_eq!(stmt_b_score.disagree_count, 0, "stmt B: 0 disagrees");
    assert!(
        stmt_b_score.diversity_q32 > 0,
        "stmt B must have non-zero diversity (X and Y disagree on A)"
    );
    assert!(
        stmt_b_score.bridging_score_q64 > 0,
        "stmt B must have non-zero bridging score (load-bearing — otherwise \
         the convergence assertion is vacuous)"
    );

    let stmt_a_score = scores_a
        .iter()
        .find(|s| s.statement_event_hash == hash_a)
        .expect("statement A in scores");
    assert_eq!(stmt_a_score.agree_count, 1, "stmt A: 1 supporter (X)");
    assert_eq!(stmt_a_score.disagree_count, 1, "stmt A: 1 disagree (Y)");
    assert_eq!(
        stmt_a_score.bridging_score_q64, 0,
        "stmt A: 1 supporter → diversity_q32 = 0 → bridging_score_q64 = 0"
    );
}
