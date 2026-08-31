//! ZEB-295 Phase 6 Task 10: integration tests for the se-mode (ballot-secret)
//! apply path + recovery of the aggregate STAR result.
//!
//! Scope per Task 10 caveat:
//!
//! - Task 8 left `maybe_emit_tally_share` as a stub (no engine-side kd=ts
//!   emission); the production end-to-end path is wired through the lower-
//!   level `recover_secret_tally` + `compute_star_from_sums` helpers. This
//!   test suite drives those helpers directly rather than full IPC.
//! - The kd=rb + kd=ts apply-time rules (B5 / T1 / T2 / timing / mode) are
//!   exercised at the integration level by constructing real `SignedVotingEvent`s
//!   and calling `Tier3PollState::apply_event`. This mirrors the unit-test
//!   coverage in `community_voting_tier3.rs::ts_apply_helpers` but at the
//!   public-API level — wire format + apply layer are pinned together.
//!
//! Helpers are duplicated from the `ts_apply_helpers` private mod in
//! `community_voting_tier3.rs` since `#[cfg(test)] mod tests` isn't reachable
//! from integration tests. The duplication is intentional — the spec acceptance
//! criteria require independent re-verification at the integration boundary.
//!
//! The kd=cr/kd=ss path is bypassed in favor of `Tier3PollState::new_from_create`
//! with direct kd=ss application — full IPC for poll creation is exercised
//! in sibling tests (`community_voting_tier3_ipc_integration.rs`) and is out
//! of scope for the se-mode-specific properties under test here.

#![cfg(feature = "test-fixtures")]

use curve25519_dalek::{
    constants::RISTRETTO_BASEPOINT_POINT, ristretto::RistrettoPoint, scalar::Scalar,
};
use frost_ristretto255::rand_core::OsRng;
use std::collections::BTreeMap;

use harmony_app::community_voting_core::{
    BallotNIZKProof, Eligibility, PollEventKindCode, PollId, RatificationBallotPayload,
    SignedVotingEvent, SortitionSelectionPayload, TallyShareEntry, TallySharePayload, Tier,
    Tier3PollConfigPayload,
};
use harmony_app::community_voting_star::CandidateRef;
use harmony_app::community_voting_tier3::{
    recover_secret_tally, synthesize_status_quo, CommitteeOracle, CommitteePublicState,
    DraftCandidateState, Tier3PollMeta, Tier3PollState,
};
use harmony_app::community_voting_tier3_crypto::{
    compress_point, decompress_point, partial_decrypt_share,
};
use harmony_app::community_voting_tier3_nizk::{
    dleq_prove, prove_ballot_bundle_with_outputs_with_score_nonces,
};
use harmony_app::owner_state_types::{Hlc, OwnerAddr};

// ─── Shared fixture helpers (duplicated from ts_apply_helpers) ───────────────

/// 2-of-3 mock committee built from a random degree-1 polynomial.
/// `f(0) = secret` (joint x); `f(i+1) = x_i` (per-member share).
pub struct MockCommittee {
    pub joint_y: RistrettoPoint,
    pub member_addrs: Vec<OwnerAddr>,
    pub shares: BTreeMap<u16, Scalar>,
    pub verifying_shares: BTreeMap<u16, RistrettoPoint>,
}

fn fake_committee_2_of_3() -> MockCommittee {
    let coeffs: Vec<Scalar> = (0..2).map(|_| Scalar::random(&mut OsRng)).collect();
    let secret = coeffs[0];
    let joint_y = RISTRETTO_BASEPOINT_POINT * secret;
    let mut shares = BTreeMap::new();
    let mut verifying_shares = BTreeMap::new();
    for i in 1u16..=3 {
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
    // Deterministic per-member OwnerAddrs (0x10, 0x11, 0x12) — sort order
    // matches FROST identifier order 1, 2, 3 (since 0x10 < 0x11 < 0x12).
    let member_addrs = vec![
        OwnerAddr([0x10; 16]),
        OwnerAddr([0x11; 16]),
        OwnerAddr([0x12; 16]),
    ];
    MockCommittee {
        joint_y,
        member_addrs,
        shares,
        verifying_shares,
    }
}

#[derive(Debug)]
struct MockCommitteeOracle {
    joint_verifying_key: [u8; 32],
    verifying_shares: BTreeMap<OwnerAddr, [u8; 32]>,
    threshold: u16,
    latest: u64,
}

impl CommitteeOracle for MockCommitteeOracle {
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

fn oracle_for(committee: &MockCommittee, latest: u64) -> std::sync::Arc<MockCommitteeOracle> {
    let mut verifying_shares = BTreeMap::new();
    for (i, addr) in committee.member_addrs.iter().enumerate() {
        let id = (i + 1) as u16;
        verifying_shares.insert(*addr, compress_point(&committee.verifying_shares[&id]));
    }
    std::sync::Arc::new(MockCommitteeOracle {
        joint_verifying_key: compress_point(&committee.joint_y),
        verifying_shares,
        threshold: 2,
        latest,
    })
}

fn hlc(wall_ms: u64) -> Hlc {
    Hlc {
        wall_ms,
        logical: 0,
        device_id: "test".into(),
    }
}

fn poll_id() -> PollId {
    PollId([0x01; 32])
}

fn se_config() -> Tier3PollConfigPayload {
    Tier3PollConfigPayload {
        proposal_text: "Tier 3c ballot-secret integration test poll".into(),
        sortition_size: 20,
        deliberation_window_seconds: 10,
        drafting_window_seconds: 10,
        ratification_window_seconds: 10,
        privacy_mode: "se".into(),
        incentive_mode: "d".into(),
        eligibility: Eligibility {
            min_power: 1,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
        predecessor: None,
        ce: None,
    }
}

fn pu_config() -> Tier3PollConfigPayload {
    let mut c = se_config();
    c.privacy_mode = "pu".into();
    c
}

fn meta_with(config: Tier3PollConfigPayload, create_wall_ms: u64) -> Tier3PollMeta {
    Tier3PollMeta {
        poll_id: poll_id(),
        proposer: OwnerAddr([0xff; 16]),
        poll_create_hlc: hlc(create_wall_ms),
        config,
        poll_create_event_hash: [0xaa; 32],
        community_epoch: 1,
    }
}

/// Default windows: dw=10/fw=10/rw=10 (s). So:
/// - PollCreate at wall_ms=0 → deliberation closes at 10_000
/// - Drafting closes at 20_000 → ratification opens at 20_000
/// - Ratification closes at 30_000
const RATIFICATION_OPEN_MS: u64 = 20_000;
const RATIFICATION_END_MS: u64 = 30_000;

fn build_event_with_payload<T: serde::Serialize>(
    kind: PollEventKindCode,
    wall_ms: u64,
    actor: OwnerAddr,
    payload: &T,
) -> SignedVotingEvent {
    let mut buf = Vec::new();
    ciborium::ser::into_writer(payload, &mut buf).expect("encode");
    SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind,
        hlc: hlc(wall_ms),
        actor,
        payload: buf,
        sig: vec![0u8; 64],
    }
}

fn build_ss_event(wall_ms: u64, primary: Vec<OwnerAddr>) -> SignedVotingEvent {
    let payload = SortitionSelectionPayload {
        poll_id: poll_id(),
        primary,
        backup: vec![],
    };
    build_event_with_payload(
        PollEventKindCode::SortitionSelection,
        wall_ms,
        OwnerAddr([0xfe; 16]),
        &payload,
    )
}

/// Construct a se-mode poll in (effective) Ratification stage with a real
/// 2-of-3 committee oracle installed. `n_total` is the spec's `n` (score-
/// aggregate count); explicit candidates = `n_total - 1` (status_quo
/// accounts for the missing one). After this, applying further events at
/// `wall_ms >= RATIFICATION_OPEN_MS` lands in the Ratification stage.
/// Build an approval set of size ≥ `min_approvals`. The integration-test
/// config uses sortition_size=20 → drafting threshold ceil(20/2)=10, more
/// than the 3-member committee can supply, so we synthesize fake approver
/// addresses (drafting_advancers only checks set cardinality, not identity).
fn synthetic_approvals(min_approvals: usize) -> std::collections::HashSet<OwnerAddr> {
    (0..min_approvals as u8)
        .map(|i| OwnerAddr([0xA0 | i; 16]))
        .collect()
}

fn arrange_se_poll_in_ratification(n_total: usize) -> (Tier3PollState, MockCommittee) {
    assert!(n_total >= 1, "n must be ≥ 1 (status_quo counts)");
    let committee = fake_committee_2_of_3();
    let electorate: Vec<OwnerAddr> = committee.member_addrs.to_vec();
    let cfg = se_config();
    let threshold = (cfg.sortition_size as usize).div_ceil(2);
    let mut state = Tier3PollState::new_from_create(meta_with(cfg, 0), electorate);
    state.install_committee_oracle(oracle_for(&committee, 0));
    // Synthetic candidates so n_total = ratification ordering length post-fix.
    // Apply-time n derivation (Qodo PR #155 #2 fix) consults
    // `ratification_candidates_ordering(drafting_advancers(...))`, which
    // filters non-status-quo candidates that lack ceil(sortition_size/2)
    // approvals. Give every synthetic candidate a full threshold-sized
    // approval set so all advance.
    let approvals = synthetic_approvals(threshold);
    for i in 0..(n_total - 1) {
        state.candidates.push(DraftCandidateState {
            event_hash: [0xC0 | (i as u8); 32],
            text: format!("candidate {i}"),
            proposer: Some(committee.member_addrs[0]),
            approvals: approvals.clone(),
        });
    }
    // Apply a kd=ss at wall_ms=10 so the stage-projection has a
    // sortition_result (required to leave Sortition).
    let ss = build_ss_event(10, committee.member_addrs.clone());
    state.apply_event(&ss).expect("ss apply");
    (state, committee)
}

fn arrange_pu_poll_in_ratification(n_total: usize) -> Tier3PollState {
    assert!(n_total >= 1);
    let electorate: Vec<OwnerAddr> = (0..3u8).map(|i| OwnerAddr([0x10 + i; 16])).collect();
    let cfg = pu_config();
    let threshold = (cfg.sortition_size as usize).div_ceil(2);
    let mut state = Tier3PollState::new_from_create(meta_with(cfg, 0), electorate.clone());
    // Same canonical-n fix as the se helper above.
    let approvals = synthetic_approvals(threshold);
    for i in 0..(n_total - 1) {
        state.candidates.push(DraftCandidateState {
            event_hash: [0xC0 | (i as u8); 32],
            text: format!("candidate {i}"),
            proposer: Some(electorate[0]),
            approvals: approvals.clone(),
        });
    }
    let ss = build_ss_event(10, electorate.clone());
    state.apply_event(&ss).expect("ss apply pu");
    state
}

/// Build a real se-mode RatificationBallotPayload via the NIZK prover.
/// `scores.len()` must equal n_total.
fn build_real_se_ballot(committee: &MockCommittee, scores: &[u64]) -> RatificationBallotPayload {
    let r_scores: Vec<Scalar> = (0..scores.len())
        .map(|_| Scalar::random(&mut OsRng))
        .collect();
    let (bundle, cs, ci) =
        prove_ballot_bundle_with_outputs_with_score_nonces(&committee.joint_y, scores, &r_scores);
    RatificationBallotPayload {
        poll_id: poll_id(),
        scores: None,
        ciphertexts_scores: Some(cs),
        ciphertexts_indicators: Some(ci),
        proof: Some(BallotNIZKProof {
            range_proofs: bundle.range_proofs,
            consistency_proofs: bundle.consistency_proofs,
        }),
    }
}

/// Compute a real kd=ts payload for committee member at index `member_idx`
/// (0-based). Aggregates `state.ratification_ballots`, produces per-entry
/// DLEQ proofs against that member's secret share x_i.
fn build_real_ts_payload(
    state: &Tier3PollState,
    committee: &MockCommittee,
    member_idx: usize,
    committee_epoch: u64,
) -> TallySharePayload {
    let n = state.candidates.len() + 1;
    let aggregates =
        harmony_app::community_voting_tier3::aggregate_se_ballots(&state.ratification_ballots, n)
            .expect("aggregates");
    let frost_id = (member_idx + 1) as u16;
    let x_i = committee.shares[&frost_id];
    let y_i = committee.verifying_shares[&frost_id];
    let g = RISTRETTO_BASEPOINT_POINT;
    let entries: Vec<TallyShareEntry> = aggregates
        .iter()
        .map(|agg| {
            let c1_agg = decompress_point(&agg.c1).expect("c1");
            let share_pt = partial_decrypt_share(&c1_agg, &x_i);
            let proof = dleq_prove(&g, &y_i, &c1_agg, &share_pt, &x_i);
            TallyShareEntry {
                share: compress_point(&share_pt),
                dleq_proof: proof.to_bytes(),
            }
        })
        .collect();
    TallySharePayload {
        poll_id: poll_id(),
        committee_epoch,
        entries,
    }
}

fn rb_event_at(
    wall_ms: u64,
    actor: OwnerAddr,
    payload: &RatificationBallotPayload,
) -> SignedVotingEvent {
    build_event_with_payload(
        PollEventKindCode::RatificationBallot,
        wall_ms,
        actor,
        payload,
    )
}

fn ts_event_at(wall_ms: u64, actor: OwnerAddr, payload: &TallySharePayload) -> SignedVotingEvent {
    build_event_with_payload(PollEventKindCode::TallyShare, wall_ms, actor, payload)
}

/// Build the canonical ratification-ordering CandidateRefs for a se-mode
/// state arranged via `arrange_se_poll_in_ratification`. Returns
/// [candidate_0, candidate_1, …, status_quo] in spec ordering.
fn ordered_candidates_for(state: &Tier3PollState) -> Vec<CandidateRef> {
    state
        .candidates
        .iter()
        .map(|c| CandidateRef {
            event_hash: c.event_hash,
            approval_count: 0,
        })
        .chain(std::iter::once(CandidateRef {
            event_hash: synthesize_status_quo(&state.meta.poll_id).event_hash,
            approval_count: 0,
        }))
        .collect()
}

// ─── Test 1: happy path — se-mode poll finalizes with recovered tally ───────

/// Build a se-mode poll, cast 3 ballots, publish 2 kd=ts shares (threshold
/// = 2-of-3), recover the tally, assert the StarResult matches what the
/// pu-mode path would have produced for the same plaintext scores.
///
/// Ballots: voter0=[5,1,0], voter1=[5,2,0], voter2=[5,3,0].
/// score_sums = [15, 6, 0]; finalists = c0+c1; c0 wins decisively.
#[tokio::test]
async fn happy_path_se_poll_finalizes_with_recovered_tally() {
    let (mut state, committee) = arrange_se_poll_in_ratification(3);
    let ballots = [[5u64, 1, 0], [5, 2, 0], [5, 3, 0]];
    for (i, scores) in ballots.iter().enumerate() {
        let payload = build_real_se_ballot(&committee, scores);
        let ev = rb_event_at(
            RATIFICATION_OPEN_MS + i as u64,
            committee.member_addrs[i],
            &payload,
        );
        state.apply_event(&ev).expect("rb apply");
    }
    assert_eq!(
        state.ratification_ballots.len(),
        3,
        "all 3 NIZK-valid ballots must be accepted"
    );

    // Two valid shares (committee threshold = 2-of-3).
    for member_idx in [0, 1] {
        let ts_payload = build_real_ts_payload(&state, &committee, member_idx, 0);
        let ev = ts_event_at(
            RATIFICATION_END_MS + member_idx as u64,
            committee.member_addrs[member_idx],
            &ts_payload,
        );
        state.apply_event(&ev).expect("ts apply");
    }
    assert_eq!(state.secret_tally.tally_shares.len(), 2);

    let ordered = ordered_candidates_for(&state);
    let result = recover_secret_tally(&state, &ordered).expect("recover");
    // Score sums: c0=15, c1=6, c2(status_quo)=0.
    assert_eq!(result.total_scores, vec![15, 6, 0]);
    assert_eq!(result.finalists.len(), 2, "c0 + c1 must advance to runoff");
    assert_eq!(
        result.winner.event_hash, ordered[0].event_hash,
        "c0 (score 15) wins decisively"
    );
}

// ─── Test 2: B5 — pu-mode payload sent to se-mode poll is silent-dropped ────

/// Already covered by the unit test `kd_rb_b5_pu_mode_payload_in_se_mode_poll_silent_drops`
/// in `community_voting_tier3.rs`. Re-verified here at the integration boundary
/// to keep the wire-format + apply-time-invariant pair pinned together (B5 is
/// the spec §5 verify rule whose absence would silently mix pu and se traffic).
#[tokio::test]
async fn b5_pu_payload_rejected_on_se_poll() {
    let (mut state, _committee) = arrange_se_poll_in_ratification(3);
    let pu_payload = RatificationBallotPayload {
        poll_id: poll_id(),
        scores: Some(vec![5, 3, 1]),
        ciphertexts_scores: None,
        ciphertexts_indicators: None,
        proof: None,
    };
    let prev_last_hlc = state.last_hlc.clone();
    let ev = rb_event_at(
        RATIFICATION_OPEN_MS,
        state.eligible_electorate_snapshot[0],
        &pu_payload,
    );
    state.apply_event(&ev).expect("apply ok (silent drop)");
    assert_eq!(
        state.ratification_ballots.len(),
        0,
        "pu-mode payload must be silently dropped in se-mode poll"
    );
    assert_eq!(
        state.last_hlc, prev_last_hlc,
        "ZEB-320: dropped event must not advance projection watermark last_hlc"
    );
}

// ─── Test 3: NIZK-invalid ballot is silent-dropped ──────────────────────────

#[tokio::test]
async fn nizk_invalid_ballot_rejected() {
    let (mut state, committee) = arrange_se_poll_in_ratification(3);
    let mut payload = build_real_se_ballot(&committee, &[5, 3, 1]);
    // Bit-flip one byte of the range_proof blob.
    payload.proof.as_mut().unwrap().range_proofs[0] ^= 0x01;
    let prev_last_hlc = state.last_hlc.clone();
    let ev = rb_event_at(
        RATIFICATION_OPEN_MS,
        state.eligible_electorate_snapshot[0],
        &payload,
    );
    state.apply_event(&ev).expect("apply ok (silent drop)");
    assert_eq!(
        state.ratification_ballots.len(),
        0,
        "tampered NIZK must be silently rejected"
    );
    assert_eq!(state.last_hlc, prev_last_hlc);
}

// ─── Test 4: kd=ts too early (before ratification_end) is silent-dropped ────

#[tokio::test]
async fn ts_too_early_rejected() {
    let (mut state, committee) = arrange_se_poll_in_ratification(3);
    let ballot = build_real_se_ballot(&committee, &[5, 3, 1]);
    let rb_ev = rb_event_at(RATIFICATION_OPEN_MS, committee.member_addrs[0], &ballot);
    state.apply_event(&rb_ev).expect("rb apply");

    let payload = build_real_ts_payload(&state, &committee, 0, 0);
    // wall_ms < RATIFICATION_END_MS (= 30_000)
    let prev_last_hlc = state.last_hlc.clone();
    let ev = ts_event_at(RATIFICATION_END_MS - 1, committee.member_addrs[0], &payload);
    state.apply_event(&ev).expect("apply ok (silent drop)");
    assert!(
        state.secret_tally.tally_shares.is_empty(),
        "kd=ts published before ratification_end must be silently dropped"
    );
    assert_eq!(state.last_hlc, prev_last_hlc);
}

// ─── Test 5: kd=ts from non-committee actor is silent-dropped (T1) ──────────

#[tokio::test]
async fn ts_non_committee_actor_rejected() {
    let (mut state, committee) = arrange_se_poll_in_ratification(3);
    // Submit a valid ballot first so aggregates exist + the kd=ts shape check
    // reaches the T1 actor-membership rule (not bypassed by no-aggregates path).
    let ballot = build_real_se_ballot(&committee, &[5, 3, 1]);
    let rb_ev = rb_event_at(RATIFICATION_OPEN_MS, committee.member_addrs[0], &ballot);
    state.apply_event(&rb_ev).expect("rb apply");

    // Actor 0xFF is not in the committee.
    let bogus_actor = OwnerAddr([0xFF; 16]);
    let n = state.candidates.len() + 1;
    let payload = TallySharePayload {
        poll_id: poll_id(),
        committee_epoch: 0,
        entries: vec![
            TallyShareEntry {
                share: [0; 32],
                dleq_proof: [0; 64],
            };
            n + n * (n - 1) / 2
        ],
    };
    let prev_last_hlc = state.last_hlc.clone();
    let ev = ts_event_at(RATIFICATION_END_MS, bogus_actor, &payload);
    state.apply_event(&ev).expect("apply ok (silent drop)");
    assert!(
        state.secret_tally.tally_shares.is_empty(),
        "kd=ts from non-committee actor must be silently rejected (T1)"
    );
    assert_eq!(state.last_hlc, prev_last_hlc);
}

// ─── Test 6: kd=ts with tampered DLEQ proof is silent-dropped (T2) ──────────

#[tokio::test]
async fn ts_invalid_dleq_rejected() {
    let (mut state, committee) = arrange_se_poll_in_ratification(3);
    let ballot = build_real_se_ballot(&committee, &[5, 3, 1]);
    let rb_ev = rb_event_at(RATIFICATION_OPEN_MS, committee.member_addrs[0], &ballot);
    state.apply_event(&rb_ev).expect("rb apply");

    let mut payload = build_real_ts_payload(&state, &committee, 0, 0);
    payload.entries[0].dleq_proof[0] ^= 0x01;
    let prev_last_hlc = state.last_hlc.clone();
    let ev = ts_event_at(RATIFICATION_END_MS, committee.member_addrs[0], &payload);
    state.apply_event(&ev).expect("apply ok (silent drop)");
    assert!(
        state.secret_tally.tally_shares.is_empty(),
        "kd=ts with tampered DLEQ proof must be silently rejected (T2)"
    );
    assert_eq!(state.last_hlc, prev_last_hlc);
}

// ─── Test 7: kd=ts on a pu-mode poll is silent-dropped ──────────────────────

#[tokio::test]
async fn ts_in_pu_mode_poll_rejected() {
    let mut state = arrange_pu_poll_in_ratification(3);
    let payload = TallySharePayload {
        poll_id: poll_id(),
        committee_epoch: 0,
        entries: vec![],
    };
    let prev_last_hlc = state.last_hlc.clone();
    let ev = ts_event_at(
        RATIFICATION_END_MS,
        state.eligible_electorate_snapshot[0],
        &payload,
    );
    state.apply_event(&ev).expect("apply ok (silent drop)");
    assert!(
        state.secret_tally.tally_shares.is_empty(),
        "kd=ts on a pu-mode poll must be silently dropped"
    );
    assert_eq!(state.last_hlc, prev_last_hlc);
}

// ─── Test 8: threshold-not-reached → recover_secret_tally returns None ──────

/// Spec §3.4: Lagrange-combine + BSGS only proceeds when the latest epoch
/// has ≥ threshold valid shares. With t-1 = 1 share (committee threshold
/// is 2), no recovery should be possible. This is the "no kd=rs emitted"
/// acceptance criterion at the recovery boundary.
#[tokio::test]
async fn threshold_not_reached_no_recovery() {
    let (mut state, committee) = arrange_se_poll_in_ratification(3);
    let ballot = build_real_se_ballot(&committee, &[5, 3, 1]);
    let rb_ev = rb_event_at(RATIFICATION_OPEN_MS, committee.member_addrs[0], &ballot);
    state.apply_event(&rb_ev).expect("rb apply");

    // Only ONE share (threshold = 2).
    let payload = build_real_ts_payload(&state, &committee, 0, 0);
    let ev = ts_event_at(RATIFICATION_END_MS, committee.member_addrs[0], &payload);
    state.apply_event(&ev).expect("ts apply");
    assert_eq!(
        state.secret_tally.tally_shares.len(),
        1,
        "one valid share accepted, but threshold not reached"
    );

    let ordered = ordered_candidates_for(&state);
    let result = recover_secret_tally(&state, &ordered);
    assert!(
        result.is_none(),
        "recover_secret_tally must return None when shares < threshold"
    );
}
