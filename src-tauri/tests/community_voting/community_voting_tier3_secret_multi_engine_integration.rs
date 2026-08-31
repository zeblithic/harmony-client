//! ZEB-295 Phase 6 Task 10: multi-engine determinism for ballot-secret tally
//! recovery. **Load-bearing per ZEB-295 acceptance #7.**
//!
//! ## What this suite proves
//!
//! 1. Two independent `Tier3PollState` projections, fed byte-identical
//!    HLC-monotonic event sequences (the discipline `Tier3PollState::apply_event`
//!    enforces on any caller), recover **bit-identical** `StarResult`
//!    (`two_engines_recover_bit_identical_tally_from_same_log`). This proves
//!    `apply_event` + `recover_secret_tally` is a pure function of
//!    (initial-state, sequence) — no hidden global ordering, RNG, or
//!    nondeterministic permutation inside the apply path leaks into the
//!    recovered tally.
//!
//! 2. **Lagrange invariance**: combining different size-`threshold` subsets of
//!    valid shares yields the same plaintext (`lagrange_invariance_subset_a_eq_subset_b`).
//!    Exercised at the `combine_shares` + `bsgs` level without engines — this is
//!    the canonical Shamir/threshold-ElGamal property, and is the load-bearing
//!    guarantee that lets replicas that received different (but each ≥ threshold)
//!    share subsets converge on the same plaintext.
//!
//! 3. **Plaintext equivalence**: a se-mode poll and a pu-mode poll fed the same
//!    per-voter scores recover identical `StarResult.total_scores +
//!    winner.event_hash` (`plaintext_equivalence_se_mode_matches_pu_mode`).
//!    Sentinel against silent crypto-pipeline soundness regressions.
//!
//! 4. Multi-epoch CHURP rotation (`churp_rotation_mid_test_recovers_correctly`)
//!    is **deferred to v2** per spec §5.3 — the Phase 4 dfrost log stores only
//!    the latest committee state, so a same-poll-spanning rotation can't be
//!    threaded through the apply path yet. The Lagrange-invariance test (#2)
//!    independently verifies the math the multi-epoch path would rely on, and
//!    the wire-format pin (`fixture_ts_pre_post_rotation_different_epoch_values`
//!    in `wire_format_voting_tier3_secret_fixtures.rs`) pins the cross-epoch
//!    discriminator on the wire. See report below for full rationale.
//!
//! Helpers are duplicated from the sibling
//! `community_voting_tier3_secret_ipc_integration.rs` since both files need
//! the same fixture-shape build helpers and integration tests can't share
//! mods across binaries.

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
use harmony_app::community_voting_star::{tally_star, CandidateRef};
use harmony_app::community_voting_tier3::{
    recover_secret_tally, synthesize_status_quo, CommitteeOracle, CommitteePublicState,
    DraftCandidateState, Tier3PollMeta, Tier3PollState,
};
use harmony_app::community_voting_tier3_crypto::{
    bsgs, combine_shares, compress_point, decompress_point, encrypt, partial_decrypt_share,
};
use harmony_app::community_voting_tier3_nizk::{
    dleq_prove, prove_ballot_bundle_with_outputs_with_score_nonces,
};
use harmony_app::owner_state_types::{Hlc, OwnerAddr};

// ─── Shared fixture helpers ──────────────────────────────────────────────────

struct MockCommittee {
    pub joint_y: RistrettoPoint,
    pub member_addrs: Vec<OwnerAddr>,
    pub shares: BTreeMap<u16, Scalar>,
    pub verifying_shares: BTreeMap<u16, RistrettoPoint>,
}

/// `t`-of-`n` mock committee built from a random degree-(t-1) polynomial.
/// `f(0)=secret`; `f(i+1)=x_i`. Member addrs are 0x10..0x10+n-1 so sort order
/// matches FROST identifier order (1, 2, …, n).
fn fake_committee(threshold: u16, total: u16) -> MockCommittee {
    assert!(threshold >= 1 && threshold <= total);
    let coeffs: Vec<Scalar> = (0..threshold).map(|_| Scalar::random(&mut OsRng)).collect();
    let secret = coeffs[0];
    let joint_y = RISTRETTO_BASEPOINT_POINT * secret;
    let mut shares = BTreeMap::new();
    let mut verifying_shares = BTreeMap::new();
    for i in 1u16..=total {
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
    let member_addrs = (1u16..=total)
        .map(|i| OwnerAddr([0x10u8 + (i - 1) as u8; 16]))
        .collect();
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

fn oracle_for(
    committee: &MockCommittee,
    threshold: u16,
    latest: u64,
) -> std::sync::Arc<MockCommitteeOracle> {
    let mut verifying_shares = BTreeMap::new();
    for (i, addr) in committee.member_addrs.iter().enumerate() {
        let id = (i + 1) as u16;
        verifying_shares.insert(*addr, compress_point(&committee.verifying_shares[&id]));
    }
    std::sync::Arc::new(MockCommitteeOracle {
        joint_verifying_key: compress_point(&committee.joint_y),
        verifying_shares,
        threshold,
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

fn config_with_privacy(privacy_mode: &str) -> Tier3PollConfigPayload {
    Tier3PollConfigPayload {
        proposal_text: "Multi-engine determinism poll".into(),
        sortition_size: 20,
        deliberation_window_seconds: 10,
        drafting_window_seconds: 10,
        ratification_window_seconds: 10,
        privacy_mode: privacy_mode.into(),
        incentive_mode: "d".into(),
        eligibility: Eligibility {
            min_power: 1,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
        predecessor: None,
    }
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

/// Build a se-mode Tier3PollState with `committee.member_addrs` as the
/// electorate, oracle installed at `latest_epoch=0`, and `n_total` candidates
/// (n_total - 1 synthetic + 1 implicit status_quo) in Ratification stage.
fn build_se_poll_state(n_total: usize, committee: &MockCommittee) -> Tier3PollState {
    let electorate: Vec<OwnerAddr> = committee.member_addrs.to_vec();
    let cfg = config_with_privacy("se");
    let threshold = (cfg.sortition_size as usize).div_ceil(2);
    let mut state = Tier3PollState::new_from_create(meta_with(cfg, 0), electorate);
    state.install_committee_oracle(oracle_for(committee, 2, 0));
    // Apply-time n derivation (Qodo PR #155 #2 fix) uses
    // `ratification_candidates_ordering(drafting_advancers(...))` which
    // filters non-status-quo candidates lacking ceil(sortition_size/2)
    // approvals. Give every synthetic candidate enough fake approvers to
    // pass; addresses don't need to be in the electorate — drafting
    // advances on set cardinality alone.
    let approvals: std::collections::HashSet<OwnerAddr> = (0..threshold as u8)
        .map(|i| OwnerAddr([0xA0 | i; 16]))
        .collect();
    for i in 0..(n_total - 1) {
        state.candidates.push(DraftCandidateState {
            event_hash: [0xC0 | (i as u8); 32],
            text: format!("candidate {i}"),
            proposer: Some(committee.member_addrs[0]),
            approvals: approvals.clone(),
        });
    }
    let ss = build_ss_event(10, committee.member_addrs.clone());
    state.apply_event(&ss).expect("ss apply");
    state
}

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

// ─── Test 1: two engines, same byte-identical HLC-monotonic event log ───────

/// Build two independent `Tier3PollState` projections, feed both the same
/// byte-identical HLC-monotonic event sequence, and assert that both recover
/// **bit-identical** `StarResult`. Same-process variant (matches the pattern
/// in `community_voting_tier3_deliberation_multi_engine_integration.rs`).
///
/// Why feed identical sequences (not permuted ones)? `Tier3PollState::apply_event`
/// enforces strict HLC monotonicity (Qodo PR #154 finding): a truly-permuted
/// order with HLC-staggered events would be rejected at the apply boundary,
/// not folded in. The HLC-monotonic dispatch discipline IS the convergence
/// mechanism — every replica sorts incoming events into HLC order before
/// apply, so by the time `apply_event` sees them they are already byte-identical
/// across replicas. This test exercises the apply-path-is-pure-function
/// invariant downstream of that discipline: given the same byte-identical
/// input sequence, `apply_event` + `recover_secret_tally` is deterministic.
/// A nondeterministic permutation inside `apply_event` itself (e.g. a
/// HashMap iteration leaking into `ratification_ballots` ordering or into
/// `recover_secret_tally`'s aggregate construction) would manifest as a
/// diff in score_sums or runoff_votes between engine A and engine B. The
/// bitwise-identical assertion is the canary.
//
// CodeAnt + CodeRabbit PR #155: the previous docstring claimed
// "deliberately-different arrival orders" but the implementation feeds
// byte-identical sequences. Docstring updated to match what the test
// actually proves; semantics unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_engines_recover_bit_identical_tally_from_same_log() {
    let committee = fake_committee(2, 3);
    // Ballots: c0 wins clearly. Scores per voter [c0, c1, status_quo].
    let scores = [[5u64, 1, 0], [5, 2, 0], [5, 3, 0]];

    // Pre-build the wire events once (same bytes-identical sequence).
    let mut events: Vec<SignedVotingEvent> = Vec::new();
    // ── kd=rb events (strictly HLC-monotonic) ───────────────────────
    // Build the ballots ONCE so engines A & B replay byte-identical events.
    let rb_payloads: Vec<RatificationBallotPayload> = scores
        .iter()
        .map(|s| build_real_se_ballot(&committee, s))
        .collect();
    for (i, p) in rb_payloads.iter().enumerate() {
        events.push(rb_event_at(
            RATIFICATION_OPEN_MS + i as u64,
            committee.member_addrs[i],
            p,
        ));
    }
    // ── kd=ts events: need to build against a state that already has the
    //    ballots aggregated. Build a throwaway projection just for share
    //    derivation. ─────────────────────────────────────────────────
    let mut probe = build_se_poll_state(3, &committee);
    for ev in events.iter().take(3) {
        probe.apply_event(ev).expect("probe rb apply");
    }
    for member_idx in [0, 1] {
        let ts = build_real_ts_payload(&probe, &committee, member_idx, 0);
        events.push(ts_event_at(
            RATIFICATION_END_MS + member_idx as u64,
            committee.member_addrs[member_idx],
            &ts,
        ));
    }

    // ── Engine A: feed events in original (HLC-ascending) order ─────
    let mut state_a = build_se_poll_state(3, &committee);
    for ev in &events {
        state_a.apply_event(ev).expect("A apply");
    }
    let ordered_a = ordered_candidates_for(&state_a);
    let result_a = recover_secret_tally(&state_a, &ordered_a).expect("A recover");

    // ── Engine B: feed events in same HLC-ascending order (strict
    //    monotonic dispatch forces convergence; nondeterministic
    //    permutation inside ratification_ballots would manifest as a
    //    diff in score_sums or runoff_votes). ─────────────────────────
    let mut state_b = build_se_poll_state(3, &committee);
    for ev in &events {
        state_b.apply_event(ev).expect("B apply");
    }
    let ordered_b = ordered_candidates_for(&state_b);
    let result_b = recover_secret_tally(&state_b, &ordered_b).expect("B recover");

    // Bitwise-identical assertion: every field of StarResult.
    assert_eq!(
        result_a.total_scores, result_b.total_scores,
        "score-sum divergence across engines"
    );
    assert_eq!(
        result_a.runoff_votes, result_b.runoff_votes,
        "runoff-vote divergence across engines"
    );
    assert_eq!(
        result_a.winner.event_hash, result_b.winner.event_hash,
        "winner divergence across engines (load-bearing)"
    );
    assert_eq!(
        result_a.winner.approval_count, result_b.winner.approval_count,
        "winner.approval_count divergence"
    );
    assert_eq!(
        result_a.finalists.len(),
        result_b.finalists.len(),
        "finalist-count divergence"
    );
    for (i, (fa, fb)) in result_a
        .finalists
        .iter()
        .zip(result_b.finalists.iter())
        .enumerate()
    {
        assert_eq!(fa.event_hash, fb.event_hash, "finalists[{i}].event_hash");
        assert_eq!(
            fa.approval_count, fb.approval_count,
            "finalists[{i}].approval_count"
        );
    }

    // Substantive: scenario must actually exercise non-trivial tallying.
    assert_eq!(
        result_a.total_scores,
        vec![15, 6, 0],
        "scenario sanity: c0=15, c1=6, c2=0"
    );
    assert_eq!(
        result_a.winner.event_hash, ordered_a[0].event_hash,
        "scenario sanity: c0 wins"
    );
}

// ─── Test 2: Lagrange invariance — different subsets, same plaintext ─────────

/// Encrypt a single value `m` to a 2-of-3 committee key. Construct two
/// share subsets — `{member 1, member 2}` vs `{member 2, member 3}` — and
/// verify both Lagrange-combine + BSGS to the same `m`. This is the canonical
/// Shamir-secret-sharing property at the threshold-ElGamal layer; if it
/// fails, replicas that received different (but each ≥ threshold) share
/// subsets would NOT converge.
///
/// At the engine level: this guarantees that even if two replicas saw share
/// subsets `S_A` and `S_B` with `|S_A ∩ S_B| < threshold` (i.e. disjoint
/// shares aside from coincidence), they recover the same plaintext as long
/// as `|S_A| ≥ threshold && |S_B| ≥ threshold`.
#[tokio::test]
async fn lagrange_invariance_subset_a_eq_subset_b() {
    let committee = fake_committee(2, 3);
    // Encrypt some plaintext `m` to the committee's joint key.
    let m: u64 = 42;
    let r = Scalar::random(&mut OsRng);
    let (c1, c2) = encrypt(Scalar::from(m), committee.joint_y, r);

    // Build per-member partial decryption shares.
    let mut partials: BTreeMap<u16, RistrettoPoint> = BTreeMap::new();
    for id in 1u16..=3 {
        let share_pt = partial_decrypt_share(&c1, &committee.shares[&id]);
        partials.insert(id, share_pt);
    }

    // Subset A: members {1, 2}.
    let subset_a: BTreeMap<u16, RistrettoPoint> = partials
        .iter()
        .filter(|(id, _)| **id <= 2)
        .map(|(k, v)| (*k, *v))
        .collect();
    // Subset B: members {2, 3}.
    let subset_b: BTreeMap<u16, RistrettoPoint> = partials
        .iter()
        .filter(|(id, _)| **id >= 2)
        .map(|(k, v)| (*k, *v))
        .collect();

    // |A∩B| = {member 2} — only one share in common; combining requires
    // BOTH subsets to interpolate the polynomial through different sets of
    // points but recover f(0) = secret.
    assert_eq!(subset_a.len(), 2);
    assert_eq!(subset_b.len(), 2);
    let common: std::collections::BTreeSet<u16> = subset_a
        .keys()
        .filter(|k| subset_b.contains_key(k))
        .copied()
        .collect();
    assert_eq!(
        common.len(),
        1,
        "subsets must share exactly one share for a meaningful test"
    );

    // Combine + decode each subset.
    let d_a = combine_shares(&c1, &subset_a).expect("combine A");
    let d_b = combine_shares(&c1, &subset_b).expect("combine B");
    assert_eq!(
        d_a, d_b,
        "Lagrange invariance: different subsets must yield the SAME aggregate \
         decryption share (= c1 · secret)"
    );

    // BSGS-decode both to the original plaintext.
    let m_a = bsgs(&(c2 - d_a), 100).expect("BSGS A");
    let m_b = bsgs(&(c2 - d_b), 100).expect("BSGS B");
    assert_eq!(m_a, m, "subset A recovers m");
    assert_eq!(m_b, m, "subset B recovers m");
}

// ─── Test 3: CHURP rotation mid-poll — DEFERRED to v2 per spec §5.3 ─────────

/// CHURP rotation mid-poll requires the Phase 4 dfrost log to expose
/// per-epoch committee state (joint_verifying_key + per-member verifying
/// shares) via the `CommitteeOracle::committee_at_epoch` query — which it
/// currently doesn't (only the latest committee state is queryable). The
/// test's hypothetical scenario (pre-rotation ballots cast → rotation event
/// published → post-rotation ballots cast → both committees publish shares
/// → recover_secret_tally falls through epochs) would also require the
/// apply path to track which ballots were encrypted under which committee
/// public key — a piece of book-keeping not yet in the data model. The
/// wire-format pin
/// (`wire_format_voting_tier3_secret_fixtures::fixture_ts_pre_post_rotation_different_epoch_values`)
/// independently asserts that the on-wire CHURP-rotation discriminator
/// (`ce` field) round-trips distinctly across epochs.
///
/// Once the dfrost log exposes multi-epoch committee history and the apply
/// path tags ballots with their encrypting epoch, this test should construct:
///
///   1. committee_A (epoch=0) + committee_B (epoch=1)
///   2. apply N kd=rb events encrypted under Y_A at wall_ms < rotation_ms
///   3. apply a synthetic "rotation" event at rotation_ms
///   4. apply M kd=rb events encrypted under Y_B at wall_ms > rotation_ms
///   5. publish ≥ threshold_A shares for epoch=0 + ≥ threshold_B for epoch=1
///   6. assert `recover_secret_tally` returns the correct aggregate over
///      ALL N+M ballots via the §5.3 multi-epoch fall-through path
#[tokio::test]
#[ignore = "ZEB-295 Phase 6: multi-epoch CHURP rotation deferred to v2 per spec §5.3 \
            — dfrost log stores only latest committee state, and the apply path \
            does not yet tag ballots by encrypting epoch. Lagrange invariance \
            (Test 2) independently verifies the underlying math; wire-format \
            pin verifies the cross-epoch discriminator."]
async fn churp_rotation_mid_test_recovers_correctly() {
    // Intentionally empty: see #[ignore] reason above.
    panic!("ignored test must not be executed");
}

// ─── Test 4: plaintext-equivalence se-mode ≡ pu-mode ─────────────────────────

/// Run the same set of per-voter scores through both pu-mode (raw scores in
/// the ballot payload, `tally_star` directly) and se-mode (encrypted ballots
/// + threshold-decryption + `compute_star_from_sums`). The two paths MUST
/// produce identical `StarResult` (modulo `runoff_votes` tied-pair attribution
/// per §4.7.2 — this scenario avoids per-ballot ties to keep the assertion
/// crisp). Sentinel against silent crypto-pipeline soundness regressions.
///
/// Score matrix [c0, c1, status_quo] across 3 voters, all of which give c0
/// the maximum and c1 a strictly lower nonzero score → no per-ballot ties
/// between the finalists.
#[tokio::test]
async fn plaintext_equivalence_se_mode_matches_pu_mode() {
    let committee = fake_committee(2, 3);
    let scores = [[5u64, 3, 0], [4, 1, 0], [5, 2, 0]];

    // ── se-mode path ────────────────────────────────────────────────
    let mut state = build_se_poll_state(3, &committee);
    for (i, s) in scores.iter().enumerate() {
        let payload = build_real_se_ballot(&committee, s);
        let ev = rb_event_at(
            RATIFICATION_OPEN_MS + i as u64,
            committee.member_addrs[i],
            &payload,
        );
        state.apply_event(&ev).expect("rb apply");
    }
    for member_idx in [0, 1] {
        let ts = build_real_ts_payload(&state, &committee, member_idx, 0);
        let ev = ts_event_at(
            RATIFICATION_END_MS + member_idx as u64,
            committee.member_addrs[member_idx],
            &ts,
        );
        state.apply_event(&ev).expect("ts apply");
    }
    let ordered = ordered_candidates_for(&state);
    let se_result = recover_secret_tally(&state, &ordered).expect("se recover");

    // ── pu-mode reference path: direct tally_star ───────────────────
    let pu_ballots: Vec<RatificationBallotPayload> = scores
        .iter()
        .map(|s| RatificationBallotPayload {
            poll_id: poll_id(),
            scores: Some(s.iter().map(|v| *v as u8).collect()),
            ciphertexts_scores: None,
            ciphertexts_indicators: None,
            proof: None,
        })
        .collect();
    let pu_result = tally_star(&ordered, &pu_ballots);

    // Score-round outputs MUST match — these are the most-stable invariants
    // (per-candidate u32 score totals, finalist set, winner).
    assert_eq!(
        se_result.total_scores, pu_result.total_scores,
        "score-sum equivalence between se-mode and pu-mode"
    );
    assert_eq!(
        se_result.finalists.len(),
        pu_result.finalists.len(),
        "finalist-count equivalence"
    );
    let se_finalist_hashes: Vec<[u8; 32]> =
        se_result.finalists.iter().map(|c| c.event_hash).collect();
    let pu_finalist_hashes: Vec<[u8; 32]> =
        pu_result.finalists.iter().map(|c| c.event_hash).collect();
    assert_eq!(
        se_finalist_hashes, pu_finalist_hashes,
        "finalist-set equivalence"
    );
    assert_eq!(
        se_result.winner.event_hash, pu_result.winner.event_hash,
        "winner equivalence — the canonical plaintext-equivalence sentinel: \
         if this fires, the threshold-ElGamal + BSGS pipeline introduces a \
         silent soundness regression. Spec §7.5 acceptance criterion."
    );
}
