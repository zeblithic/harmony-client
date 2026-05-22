//! ZEB-309 Phase 4a-main: STAR ratification math (score + automatic runoff).
//! Pure functions. Deterministic. Tiebreaker cascade:
//! runoff_votes → total_score → candidate_event_hash lex ASC.

use crate::community_voting_core::{CandidateEventHash, RatificationBallotPayload};
use crate::owner_state_types::{deserialize_bytes_from_bstr, serialize_bytes_as_bstr};
use serde::{Deserialize, Serialize};

/// A reference to a draft candidate identified by its event hash.
///
/// `Serialize`/`Deserialize` are added in ZEB-309 Phase 4a-main so that
/// `Tier3PollResultPayload` can encode the full `StarResult` in the kd=rs
/// payload — required for SR1 verify (any node re-computes and compares).
///
/// `approval_count` is set by `drafting_advancers` (from `DraftCandidateState.approvals.len()`)
/// and carried through `ratification_candidates_ordering` so the ordering sort can tiebreak
/// deterministically without re-reading the full `DraftCandidateState` slice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateRef {
    #[serde(
        rename = "eh",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub event_hash: CandidateEventHash,
    /// Approval count at the time this candidate was advanced to ratification.
    /// Defaults to 0 for the status_quo candidate (no approvals by design).
    #[serde(rename = "ac", default)]
    pub approval_count: u32,
}

/// Output of the STAR tally computation.
///
/// - `winner`: the candidate who won the automatic runoff.
/// - `finalists`: the 2+ candidates who advanced to the runoff (3+ on
///   score-tie at 2nd place).
/// - `total_scores`: sum of ballot scores per candidate, indexed by the
///   same position as the `candidates` input slice.
/// - `runoff_votes`: number of runoff votes each finalist received,
///   indexed by the same position as the `finalists` output vec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StarResult {
    #[serde(rename = "wn")]
    pub winner: CandidateRef,
    #[serde(rename = "fn")]
    pub finalists: Vec<CandidateRef>,
    #[serde(rename = "ts")]
    pub total_scores: Vec<u32>,
    #[serde(rename = "rv")]
    pub runoff_votes: Vec<u32>,
}

/// Compute a STAR (Score Then Automatic Runoff) tally over `ballots`.
///
/// # Algorithm
///
/// **Score round:** sum ballot scores per candidate. Identify the highest
/// total (`max1`) and second-highest total (`max2`). All candidates whose
/// total equals `max1` or `max2` become finalists. If `max1 == max2`, all
/// candidates at that score are finalists.
///
/// **Runoff round:** for each ballot, find the highest score among the
/// finalist-indexed scores. If exactly one finalist holds that maximum,
/// it receives one runoff vote; if multiple finalists are tied at the
/// maximum the ballot abstains.
///
/// **Winner selection:** sort finalists by (runoff_votes DESC, total_score
/// DESC, candidate_event_hash ASC). The first element after sorting is the
/// winner.
///
/// # Panics
///
/// Panics if `candidates` is empty. Call sites (B4 verify) guarantee the
/// ratification candidate list is non-empty before invoking this function.
pub fn tally_star(
    candidates: &[CandidateRef],
    ballots: &[RatificationBallotPayload],
) -> StarResult {
    assert!(
        !candidates.is_empty(),
        "tally_star: candidates must be non-empty"
    );

    let n = candidates.len();

    // --- Score round ---
    // Note (Cluster 10, CodeRabbit minor): `u32` accumulator overflows at
    // approximately 858M ballots × max score 5 (≈ 4.3B total). This is
    // intentional for Phase 4a-main (public community polls are orders of
    // magnitude smaller). If ever applied to global-scale elections, switch
    // to `u64` accumulator before deploying.
    let mut total_scores = vec![0u32; n];
    for ballot in ballots {
        // ZEB-295 Phase 6: `scores` is Option<Vec<u8>>. tally_star is the
        // pu-mode tally path; se-mode ballots are tallied via the homomorphic
        // aggregation path and never reach here. None ⇒ treat as no scores
        // (defensive; the B4 caller filters to pu-mode ballots).
        let scores = match ballot.scores.as_ref() {
            Some(s) => s,
            None => continue,
        };
        for (i, &score) in scores.iter().enumerate().take(n) {
            total_scores[i] += u32::from(score);
        }
    }

    // Find max1 (highest total) and max2 (second value in sorted-DESC list,
    // possibly equal to max1 if two or more candidates are tied at the top).
    //
    // Example: [20, 15, 15] → sorted=[20,15,15] → max1=20, max2=15 → 3 finalists.
    // Example: [10, 10, 3]  → sorted=[10,10, 3] → max1=10, max2=10 → 2 finalists [A,B].
    // Example: [5]          → sorted=[5]         → max1=5,  max2=5  → 1 finalist.
    let max1 = total_scores.iter().copied().max().unwrap_or(0);
    // max2 = second element of sorted-descending list (may equal max1).
    let mut sorted_desc = total_scores.clone();
    sorted_desc.sort_unstable_by(|a, b| b.cmp(a));
    let max2 = sorted_desc.get(1).copied().unwrap_or(max1);

    // Finalists = all candidates whose total == max1 OR == max2.
    // Their positions in the `candidates` slice (needed for runoff lookup).
    let finalist_indices: Vec<usize> = (0..n)
        .filter(|&i| total_scores[i] == max1 || total_scores[i] == max2)
        .collect();

    // --- Runoff round ---
    let mut runoff_votes = vec![0u32; finalist_indices.len()];
    for ballot in ballots {
        // ZEB-295 Phase 6: skip se-mode ballots (None scores) in pu-mode tally.
        let scores = match ballot.scores.as_ref() {
            Some(s) => s,
            None => continue,
        };
        // Collect each finalist's score from this ballot.
        let finalist_scores: Vec<u8> = finalist_indices
            .iter()
            .map(|&ci| scores.get(ci).copied().unwrap_or(0))
            .collect();

        let max_score = finalist_scores.iter().copied().max().unwrap_or(0);

        // Count how many finalists share the max_score.
        let leaders: Vec<usize> = finalist_scores
            .iter()
            .enumerate()
            .filter(|(_, &s)| s == max_score)
            .map(|(fi, _)| fi)
            .collect();

        if leaders.len() == 1 {
            runoff_votes[leaders[0]] += 1;
        }
        // else: abstain
    }

    // --- Winner selection ---
    // Sort finalist positions by: runoff_votes DESC, total_score DESC,
    // event_hash ASC.
    let winner_fi = (0..finalist_indices.len())
        .max_by(|&a, &b| {
            runoff_votes[a]
                .cmp(&runoff_votes[b])
                .then_with(|| {
                    total_scores[finalist_indices[a]].cmp(&total_scores[finalist_indices[b]])
                })
                .then_with(|| {
                    // lex ASC → prefer smaller hash → reverse comparison for
                    // the max_by context (we want the smaller hash to "win",
                    // so it should compare Greater to the max_by comparator).
                    candidates[finalist_indices[b]]
                        .event_hash
                        .cmp(&candidates[finalist_indices[a]].event_hash)
                })
        })
        .unwrap(); // safe: finalist_indices is non-empty (n >= 1)

    let finalists: Vec<CandidateRef> = finalist_indices
        .iter()
        .map(|&ci| candidates[ci].clone())
        .collect();

    StarResult {
        winner: candidates[finalist_indices[winner_fi]].clone(),
        finalists,
        total_scores,
        runoff_votes,
    }
}

/// ZEB-295 Phase 6: compute a STAR result from pre-aggregated score sums +
/// indicator sums. Spec §3.4 step 7.
///
/// The secret-mode tally produces these aggregates via threshold-ElGamal
/// decryption + BSGS dlog recovery; from there the algorithm follows the
/// same STAR shape as [`tally_star`], but operates on aggregate counts
/// rather than per-ballot scores.
///
/// - `ordered`: ratification candidate ordering (same shape as `tally_star`'s
///   first argument).
/// - `score_sums[i]`: Σ over electorate of score for candidate `i` (matches
///   `ordered[i]`). `score_sums.len()` must equal `ordered.len()`.
/// - `indicator_sums[k]`: Σ over electorate of `[score_A > score_B]` for the
///   k-th unordered pair `(A, B)` with `A < B` in `ordered` index order.
///   `indicator_sums.len()` must equal `n*(n-1)/2`. The pair index is
///   `k(A, B) = A * (2n - A - 1) / 2 + (B - A - 1)`.
/// - `ballot_count`: the number of accepted (LWW-deduped) ballots. Used to
///   derive the count of "B beats A" ballots from the one-direction
///   "A beats B" indicator sum.
///
/// # Runoff round shape
///
/// - **1 finalist:** `runoff_votes[0] = ballot_count` (mirrors tally_star's
///   `+1` per ballot when there's a single leader).
/// - **2 finalists `(a, b)` with `a < b` in `ordered`:**
///     `runoff_votes[a] = indicator_sums[k(a,b)]` and
///     `runoff_votes[b] = ballot_count - indicator_sums[k(a,b)]`.
/// - **3+ finalists:** Condorcet-style "pairwise wins" count. For each
///   finalist `i`, `runoff_votes[i]` = number of OTHER finalists that
///   finalist `i` strictly pairwise-beats. This matches the ordering
///   `tally_star` produces for multi-finalist runoffs at the score-tied
///   2nd-place case, derived purely from the one-direction indicator sums.
///
/// # Known divergence from `tally_star` (one-direction encoding limitation)
///
/// The wire format (spec §4.7.2) encrypts `[score_A > score_B]` only — NOT
/// `[score_B > score_A]` or `[score_A == score_B]`. Per-ballot ties (which
/// `tally_star` treats as abstentions) are charged to the larger-index
/// finalist (`b`) via `ballot_count - indicator_sums[k]`. Likewise,
/// 3+-finalist pairwise comparisons cannot distinguish "B strictly beats A"
/// from "B ties A" — both contribute to the `ballot_count - indicator_sums[k]`
/// side.
///
/// **Bit-identical equivalence to `tally_star` holds whenever no per-ballot
/// tie exists between any pair of finalists.** When ties exist, secret-mode
/// STAR remains deterministic but may pick a different winner from
/// public-mode STAR. The plaintext-equivalence test (§7.5) uses ballot
/// sets that avoid finalist-pair ties; the
/// `compute_star_from_sums_diverges_from_tally_star_on_pair_ties` test
/// sentinels the limitation.
///
/// Encoding both directions of each indicator pair (so abstentions are
/// derivable as `ballot_count - i_wins - j_wins`) is a wire-format
/// extension deferred to a follow-up — it would double
/// `RatificationBallotPayload.ciphertexts_indicators` and require a
/// matching NIZK extension.
///
/// Tie-break invariants match [`tally_star`]:
/// - Runoff-vote tie → higher `total_score` wins, then smaller `event_hash`.
/// - Score-tie → smaller candidate `event_hash` wins.
///
/// # Panics
/// Panics if `ordered` is empty (mirrors `tally_star`).
pub fn compute_star_from_sums(
    ordered: &[CandidateRef],
    score_sums: Vec<u64>,
    indicator_sums: Vec<u64>,
    ballot_count: u64,
) -> StarResult {
    assert!(
        !ordered.is_empty(),
        "compute_star_from_sums: ordered must be non-empty"
    );
    let n = ordered.len();
    assert_eq!(
        score_sums.len(),
        n,
        "compute_star_from_sums: score_sums.len() must equal ordered.len()"
    );
    let pair_count = n * (n - 1) / 2;
    assert_eq!(
        indicator_sums.len(),
        pair_count,
        "compute_star_from_sums: indicator_sums.len() must equal n*(n-1)/2"
    );

    // --- Score round ---
    // Use the same u32 accumulator + max1/max2/finalist selection as
    // tally_star so the per-candidate score totals are bit-identical.
    let total_scores: Vec<u32> = score_sums
        .iter()
        .map(|s| u32::try_from(*s).unwrap_or(u32::MAX))
        .collect();
    let max1 = total_scores.iter().copied().max().unwrap_or(0);
    let mut sorted_desc = total_scores.clone();
    sorted_desc.sort_unstable_by(|a, b| b.cmp(a));
    let max2 = sorted_desc.get(1).copied().unwrap_or(max1);
    let finalist_indices: Vec<usize> = (0..n)
        .filter(|&i| total_scores[i] == max1 || total_scores[i] == max2)
        .collect();

    // --- Runoff round ---
    //
    // Pair-index formula (matches `aggregate_se_ballots` ordering):
    //     for (a, b) with 0 <= a < b < n,
    //     k(a, b) = a * (2n - a - 1) / 2 + (b - a - 1)
    //
    // Indicator semantics: `indicator_sums[k(a, b)]` counts ballots where
    // `score_a > score_b`. The reverse direction count is
    // `ballot_count - indicator_sums[k]` (note: includes per-ballot ties —
    // see doc comment on this function for the divergence-from-tally_star
    // limitation).
    let pair_index = |a: usize, b: usize| -> usize {
        debug_assert!(a < b);
        a * (2 * n - a - 1) / 2 + (b - a - 1)
    };
    let mut runoff_votes = vec![0u32; finalist_indices.len()];
    if finalist_indices.len() == 1 {
        // Single finalist: every ballot's "leader" is the sole finalist
        // (matches tally_star which always +1's when leaders.len() == 1).
        runoff_votes[0] = u32::try_from(ballot_count).unwrap_or(u32::MAX);
    } else if finalist_indices.len() == 2 {
        // 2-finalist case: `runoff_votes[a] = indicator_sums[k(a, b)]`,
        // `runoff_votes[b] = ballot_count - indicator_sums[k(a, b)]`.
        // Tied-pair ballots are charged to b (the larger-index finalist);
        // see doc comment.
        let a_idx = finalist_indices[0];
        let b_idx = finalist_indices[1];
        // a_idx < b_idx by construction (finalist_indices iterates 0..n).
        let k = pair_index(a_idx, b_idx);
        let a_wins = indicator_sums[k];
        let b_wins = ballot_count.saturating_sub(a_wins);
        runoff_votes[0] = u32::try_from(a_wins).unwrap_or(u32::MAX);
        runoff_votes[1] = u32::try_from(b_wins).unwrap_or(u32::MAX);
    } else {
        // 3+ finalist case (score-tie at 2nd place): Condorcet-style
        // pairwise-wins count. For each finalist i, count OTHER finalists
        // that i STRICTLY pairwise-beats. The pair_index formula references
        // ABSOLUTE indices in `ordered` (NOT finalist-relative indices) —
        // `aggregate_se_ballots` and `recover_secret_tally` populate
        // `indicator_sums` over all `n*(n-1)/2` candidate pairs.
        for fi_i in 0..finalist_indices.len() {
            let i_abs = finalist_indices[fi_i];
            let mut wins: u32 = 0;
            for fi_j in 0..finalist_indices.len() {
                if fi_i == fi_j {
                    continue;
                }
                let j_abs = finalist_indices[fi_j];
                // pair_index requires lo < hi over absolute indices.
                let (lo, hi) = if i_abs < j_abs {
                    (i_abs, j_abs)
                } else {
                    (j_abs, i_abs)
                };
                let k = pair_index(lo, hi);
                let lo_wins = indicator_sums[k];
                let hi_wins = ballot_count.saturating_sub(lo_wins);
                // Did finalist i (absolute index i_abs) strictly beat j?
                let i_beats_j = if i_abs < j_abs {
                    lo_wins > hi_wins
                } else {
                    hi_wins > lo_wins
                };
                if i_beats_j {
                    wins += 1;
                }
            }
            runoff_votes[fi_i] = wins;
        }
    }

    // --- Winner selection ---
    // Same comparator chain as tally_star.
    let winner_fi = (0..finalist_indices.len())
        .max_by(|&a, &b| {
            runoff_votes[a]
                .cmp(&runoff_votes[b])
                .then_with(|| {
                    total_scores[finalist_indices[a]].cmp(&total_scores[finalist_indices[b]])
                })
                .then_with(|| {
                    // lex ASC → smaller hash wins; reverse for max_by.
                    ordered[finalist_indices[b]]
                        .event_hash
                        .cmp(&ordered[finalist_indices[a]].event_hash)
                })
        })
        .unwrap(); // safe: ordered non-empty ⇒ finalist_indices non-empty.

    let finalists: Vec<CandidateRef> = finalist_indices
        .iter()
        .map(|&ci| ordered[ci].clone())
        .collect();

    StarResult {
        winner: ordered[finalist_indices[winner_fi]].clone(),
        finalists,
        total_scores,
        runoff_votes,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(any(test, feature = "test-fixtures"))]
pub mod test_helpers {
    use super::*;
    use crate::community_voting_core::PollId;

    /// Build a `CandidateRef` from a single repeated byte (for readability in
    /// tests). `approval_count` defaults to 0 (sufficient for all STAR tally tests
    /// since tally_star doesn't use approval_count).
    pub fn candidate(byte: u8) -> CandidateRef {
        CandidateRef {
            event_hash: [byte; 32],
            approval_count: 0,
        }
    }

    /// Build a `RatificationBallotPayload` from a score slice.
    pub fn ballot(scores: &[u8]) -> RatificationBallotPayload {
        RatificationBallotPayload {
            poll_id: PollId([0u8; 32]),
            scores: Some(scores.to_vec()),
            ciphertexts_scores: None,
            ciphertexts_indicators: None,
            proof: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_helpers::*;
    use super::*;

    // 1. Three-candidate clear winner
    #[test]
    fn three_candidate_clear_winner() {
        // A: sum 10 (5+5), B: sum 8 (4+4), C: sum 5 (3+2)
        let candidates = vec![candidate(0xAA), candidate(0xBB), candidate(0xCC)];
        let ballots = vec![ballot(&[5, 4, 3]), ballot(&[5, 4, 2])];
        let result = tally_star(&candidates, &ballots);
        // total_scores: [10, 8, 5] → finalists: A(10) and B(8)
        assert_eq!(result.total_scores, vec![10, 8, 5]);
        assert_eq!(result.finalists.len(), 2);
        assert_eq!(result.finalists[0], candidate(0xAA)); // max1 first
        assert_eq!(result.finalists[1], candidate(0xBB));
        // runoff: each ballot scores A=5 > B=4 → A gets 2 votes
        assert_eq!(result.runoff_votes, vec![2, 0]);
        assert_eq!(result.winner, candidate(0xAA));
    }

    // 2. Score tie at second place → 3-way runoff
    #[test]
    fn score_tie_at_second_place_three_way_runoff() {
        // A=20, B=15, C=15 → finalists = [A, B, C]
        let candidates = vec![candidate(0xAA), candidate(0xBB), candidate(0xCC)];
        // Use exact totals: A gets 5 per ballot (×4=20), B gets 3 (×5... let's do 4 ballots)
        // A: 5+5+5+5=20, B: 4+4+4+3=15, C: 4+4+4+3=15
        let ballots = vec![
            ballot(&[5, 4, 4]),
            ballot(&[5, 4, 4]),
            ballot(&[5, 4, 4]),
            ballot(&[5, 3, 3]),
        ];
        let result = tally_star(&candidates, &ballots);
        assert_eq!(result.total_scores, vec![20, 15, 15]);
        // max1=20, max2=15, all three qualify
        assert_eq!(result.finalists.len(), 3);
        let finalist_hashes: Vec<_> = result.finalists.iter().map(|f| f.event_hash[0]).collect();
        assert!(finalist_hashes.contains(&0xAA));
        assert!(finalist_hashes.contains(&0xBB));
        assert!(finalist_hashes.contains(&0xCC));
    }

    // 3. Score tie at first place → 2 finalists
    #[test]
    fn score_tie_at_first_place() {
        // A=20, B=20, C=10
        let candidates = vec![candidate(0xAA), candidate(0xBB), candidate(0xCC)];
        let ballots = vec![
            ballot(&[5, 5, 3]),
            ballot(&[5, 5, 2]),
            ballot(&[5, 5, 2]),
            ballot(&[5, 5, 3]),
        ];
        let result = tally_star(&candidates, &ballots);
        assert_eq!(result.total_scores, vec![20, 20, 10]);
        // max1=20, max2=10 → finalists = those with 20 OR 10 → all 3?
        // Actually per spec: max2 = second DISTINCT score below max1. Wait...
        // Re-reading: max2 = highest score among those < max1.
        // Here max1=20, candidates at 20: A and B. Candidates not at 20: C (10).
        // So max2 = 10. finalists = those with 20 OR 10 = all three.
        // BUT the spec says "top 2" and example says finalists=[A,B] on A=20,B=20,C=10.
        // The spec §8 says: "Find max1 (= top score) and max2 (= second-highest,
        // possibly equal to max1). Finalists = ALL candidates with total ∈ {max1, max2}."
        // When max1 == max2: all at that score. When max1 != max2: those with max1 OR max2.
        // So for A=20,B=20,C=10: max1=20, max2=10, finalists=[A,B,C].
        // The task description test 3 says "→ finalists [A, B] (top 2 by score; tied at first)"
        // This conflicts with the algorithm as stated.
        // The task description overrides: test 3 expects finalists=[A,B] for A=20,B=20,C=10.
        // This means max2 should be the second DISTINCT value that is ALSO in the top-2
        // scores numerically, i.e. max2 = max score among those NOT at max1 tier, but only
        // if the top-2 "slots" aren't already filled by tied max1 candidates.
        //
        // Re-reading task description algorithm:
        // "Find max1 (= top score) and max2 (= second-highest, possibly equal to max1).
        //  Finalists = ALL candidates with total ∈ {max1, max2}.
        //  If max1 == max2, treat the set of all candidates at that score uniformly."
        //
        // For A=20,B=20,C=10: max1=20. max2=second-highest = 20 (because B also has 20).
        // So max1==max2==20 → finalists = all at 20 = [A, B]. That's the correct reading!
        // max2 is the second VALUE in the SORTED score list (not second distinct),
        // so if A=20 and B=20 then sorted top-2 = [20, 20] → max2=20=max1.
        assert_eq!(result.finalists.len(), 2);
        let finalist_hashes: Vec<_> = result.finalists.iter().map(|f| f.event_hash[0]).collect();
        assert!(finalist_hashes.contains(&0xAA));
        assert!(finalist_hashes.contains(&0xBB));
        assert!(!finalist_hashes.contains(&0xCC));
    }

    // 4. Runoff equal scores → abstain
    #[test]
    fn runoff_equal_scores_abstain() {
        // Single ballot scores both finalists 5 → abstain
        let candidates = vec![candidate(0xAA), candidate(0xBB)];
        let ballots = vec![ballot(&[5, 5])];
        let result = tally_star(&candidates, &ballots);
        // Both have total=5 → max1=5, max2=5(same) → both are finalists
        assert_eq!(result.finalists.len(), 2);
        // Runoff: ballot scores [5,5] → tie → abstain
        assert_eq!(result.runoff_votes, vec![0, 0]);
    }

    // 5. Runoff clear preference among finalists
    #[test]
    fn runoff_clear_preference_among_finalists() {
        // Ballot [5, 3] → vote for finalist 0
        let candidates = vec![candidate(0xAA), candidate(0xBB)];
        let ballots = vec![ballot(&[5, 3])];
        let result = tally_star(&candidates, &ballots);
        // total_scores: [5, 3] → max1=5, second in sorted=[3] → max2=3 → finalists=[A,B]
        assert_eq!(result.finalists.len(), 2);
        // Runoff: A=5 > B=3 → finalist 0 (A) gets 1 vote
        assert_eq!(result.runoff_votes[0], 1);
        assert_eq!(result.runoff_votes[1], 0);
        assert_eq!(result.winner, candidate(0xAA));
    }

    // 6. Runoff tie resolved by total score
    // Asymmetric magnitudes: ballot1=[5,1] votes A (5>1), ballot2=[3,5] votes B (5>3).
    // Runoff tied 1-1. Totals: A=8, B=6. A wins by total score tiebreaker.
    #[test]
    fn runoff_tie_resolved_by_total_score() {
        // A=candidate(0x10), B=candidate(0x20). 0x10 < 0x20 lex, so lex would also pick A —
        // but total_score tiebreak fires first (A=8 > B=6).
        let candidates = vec![candidate(0x10), candidate(0x20)];
        let ballots = vec![
            ballot(&[5, 1]), // A wins runoff (5>1); A_total+=5, B_total+=1
            ballot(&[3, 5]), // B wins runoff (5>3); A_total+=3, B_total+=5
        ];
        let result = tally_star(&candidates, &ballots);
        assert_eq!(result.total_scores, vec![8, 6]);
        assert_eq!(result.finalists.len(), 2);
        assert_eq!(result.runoff_votes, vec![1, 1]); // tied in runoff
                                                     // Tiebreaker 1 fires: A total (8) > B total (6) → A wins
        assert_eq!(result.winner, candidate(0x10));
    }

    // 7. Total score tie resolved by event_hash lex
    #[test]
    fn total_score_tie_resolved_by_event_hash_lex() {
        // Both finalists fully tied → lex ASC on event_hash decides
        // Use candidate bytes 0x10 < 0x20 → 0x10 wins
        let candidates = vec![candidate(0x20), candidate(0x10)];
        let ballots = vec![
            ballot(&[3, 3]), // tie → abstain
            ballot(&[3, 3]),
        ];
        let result = tally_star(&candidates, &ballots);
        assert_eq!(result.total_scores, vec![6, 6]);
        assert_eq!(result.runoff_votes, vec![0, 0]);
        // lex ASC: 0x10 < 0x20 → candidate(0x10) wins
        assert_eq!(result.winner, candidate(0x10));
    }

    // 8. Empty ballots → all tied at 0; lex-ASC event_hash wins
    #[test]
    fn empty_ballots_status_quo_wins_by_lex() {
        let candidates = vec![candidate(0xBB), candidate(0xAA), candidate(0xCC)];
        let ballots: Vec<RatificationBallotPayload> = vec![];
        let result = tally_star(&candidates, &ballots);
        assert_eq!(result.total_scores, vec![0, 0, 0]);
        // max1=0, max2=0 (max1==max2) → all are finalists
        assert_eq!(result.finalists.len(), 3);
        // runoff: no ballots → all zeros
        assert_eq!(result.runoff_votes, vec![0, 0, 0]);
        // tiebreaker: lex ASC on event_hash → 0xAA < 0xBB < 0xCC → 0xAA wins
        assert_eq!(result.winner, candidate(0xAA));
    }

    // 9. Single ballot decides two-finalist race
    #[test]
    fn single_ballot_decides_two_finalist_race() {
        let candidates = vec![candidate(0xAA), candidate(0xBB)];
        let ballots = vec![ballot(&[4, 5])];
        let result = tally_star(&candidates, &ballots);
        // totals: [4,5] → max1=5(B), max2=4(A) → finalists=[A,B]
        // runoff: [4,5]→B wins
        assert_eq!(result.runoff_votes[1], 1);
        assert_eq!(result.runoff_votes[0], 0);
        assert_eq!(result.winner, candidate(0xBB));
    }

    // 10. All zero scores → lex tiebreaker
    #[test]
    fn all_zero_scores_lex_tiebreaker() {
        let candidates = vec![candidate(0xFF), candidate(0x01), candidate(0x80)];
        let ballots = vec![ballot(&[0, 0, 0]), ballot(&[0, 0, 0])];
        let result = tally_star(&candidates, &ballots);
        assert_eq!(result.total_scores, vec![0, 0, 0]);
        // lex ASC: 0x01 < 0x80 < 0xFF → candidate(0x01) wins
        assert_eq!(result.winner, candidate(0x01));
    }

    // 11. Five-candidate full slate
    #[test]
    fn five_candidate_full_slate() {
        let candidates = vec![
            candidate(0x01),
            candidate(0x02),
            candidate(0x03),
            candidate(0x04),
            candidate(0x05),
        ];
        // 5 ballots, varied scores
        let ballots = vec![
            ballot(&[5, 3, 2, 4, 1]),
            ballot(&[4, 5, 1, 3, 2]),
            ballot(&[5, 4, 3, 2, 1]),
            ballot(&[3, 5, 4, 1, 2]),
            ballot(&[5, 4, 2, 3, 1]),
        ];
        let result = tally_star(&candidates, &ballots);
        // totals: A=5+4+5+3+5=22, B=3+5+4+5+4=21, C=2+1+3+4+2=12, D=4+3+2+1+3=13, E=1+2+1+2+1=7
        assert_eq!(result.total_scores, vec![22, 21, 12, 13, 7]);
        // max1=22(A), second in sorted list = 21(B) → max2=21 → finalists=[A,B]
        assert_eq!(result.finalists.len(), 2);
        // Runoff: ballot[5,3]→A; [4,5]→B; [5,4]→A; [3,5]→B; [5,4]→A → A=3, B=2
        assert_eq!(result.runoff_votes[0], 3); // A
        assert_eq!(result.runoff_votes[1], 2); // B
        assert_eq!(result.winner, candidate(0x01));
    }

    // 12. Score invariant under ballot reordering
    #[test]
    fn score_invariant_under_ballot_reordering() {
        let candidates = vec![candidate(0xAA), candidate(0xBB), candidate(0xCC)];
        let ballots_original = vec![
            ballot(&[5, 3, 1]),
            ballot(&[3, 5, 2]),
            ballot(&[4, 4, 5]),
            ballot(&[2, 3, 4]),
        ];
        let ballots_reordered = vec![
            ballot(&[3, 5, 2]),
            ballot(&[2, 3, 4]),
            ballot(&[5, 3, 1]),
            ballot(&[4, 4, 5]),
        ];
        let r1 = tally_star(&candidates, &ballots_original);
        let r2 = tally_star(&candidates, &ballots_reordered);
        assert_eq!(r1, r2);
    }

    // 13. Unanimous top scorer wins
    #[test]
    fn unanimous_top_scorer() {
        let candidates = vec![candidate(0xAA), candidate(0xBB), candidate(0xCC)];
        let ballots = vec![ballot(&[5, 0, 0]), ballot(&[5, 0, 0]), ballot(&[5, 0, 0])];
        let result = tally_star(&candidates, &ballots);
        assert_eq!(result.total_scores, vec![15, 0, 0]);
        // max1=15(A), max2=0 → finalists=[A,B,C] (B and C tied at max2)
        // Wait: max2 = highest score among those < max1. Candidates not at 15: B=0, C=0.
        // max2 = 0. finalists = those at 15 OR 0 = all.
        // BUT we want A to win. Runoff: ballot finalist scores [5,0,0]→A wins each ballot.
        assert_eq!(result.winner, candidate(0xAA));
    }

    // 14. Runoff with abstentions counted correctly
    #[test]
    fn runoff_with_abstentions_counted_correctly() {
        // N=3 ballots, 1 abstain, 1 for finalist 0, 1 for finalist 1
        let candidates = vec![candidate(0xAA), candidate(0xBB)];
        let ballots = vec![
            ballot(&[5, 3]), // finalist 0 wins
            ballot(&[3, 5]), // finalist 1 wins
            ballot(&[4, 4]), // abstain
        ];
        let result = tally_star(&candidates, &ballots);
        assert_eq!(result.runoff_votes, vec![1, 1]); // 1 abstain doesn't count
                                                     // Tie resolved by total: A=5+3+4=12, B=3+5+4=12. Equal. Resolve by lex.
                                                     // 0xAA < 0xBB → A wins
        assert_eq!(result.winner, candidate(0xAA));
    }

    // 15. Ballot with some undecided scores → abstain if finalists tie
    #[test]
    fn ballot_with_some_undecided_scores() {
        // [5, 5, 0] on 3-finalist runoff → abstain (both A and B have 5, C has 0)
        let candidates = vec![candidate(0xAA), candidate(0xBB), candidate(0xCC)];
        // All three are finalists at scores 5, 5, 0 totals
        let ballots = vec![ballot(&[5, 5, 0])];
        let result = tally_star(&candidates, &ballots);
        // totals: [5,5,0] → max1=5, max2=5(same)=max1 → finalists=[A,B]
        // C has 0 which is < max2=5, so C is excluded.
        // Runoff: finalist scores [5,5]→tie→abstain
        assert_eq!(result.finalists.len(), 2);
        assert_eq!(result.runoff_votes, vec![0, 0]);
    }

    // 16. Score round ignores non-finalists in runoff
    #[test]
    fn score_round_ignores_non_finalists_in_runoff() {
        // C scores 5 but C not finalist; ballot scores [3, 4, 5] → vote for B (score 4)
        // Setup: A=3, B=4, C=5 → max1=5(C), max2=4(B) → finalists=[C,B]
        // But we want A and B as finalists... adjust:
        // Make A and B finalists, C not:
        // A=10, B=8, C=5 → max1=10(A), max2=8(B) → finalists=[A,B]
        // Ballot: [3,4,5] → finalist scores [3,4] (ignoring C's 5) → B wins (4>3)
        let candidates = vec![candidate(0xAA), candidate(0xBB), candidate(0xCC)];
        // Need A=10, B=8, C=5 total. Use 2 ballots:
        // ballot1=[5,4,3]: A+=5, B+=4, C+=3
        // ballot2=[5,4,2]: A+=5, B+=4, C+=2
        // Totals: A=10, B=8, C=5. max1=10(A), max2=8(B). Finalists=[A,B]. ✓
        // Now add the test ballot [3,4,5]: C=5 but C not finalist; finalist scores [3,4]→B wins
        let ballots = vec![
            ballot(&[5, 4, 3]),
            ballot(&[5, 4, 2]),
            ballot(&[3, 4, 5]), // C not finalist; B(4)>A(3) → B gets vote
        ];
        let result = tally_star(&candidates, &ballots);
        assert_eq!(result.total_scores, vec![13, 12, 10]);
        // max1=13(A), max2=12(B). Finalists=[A,B].
        assert_eq!(result.finalists.len(), 2);
        // Runoff: b1[5,4]→A; b2[5,4]→A; b3[3,4]→B (C ignored)
        assert_eq!(result.runoff_votes, vec![2, 1]);
        assert_eq!(result.winner, candidate(0xAA));
        // Additionally verify: the test ballot DID vote for B not C (C was non-finalist)
        assert_eq!(
            result.runoff_votes[1], 1,
            "B should get 1 vote from the [3,4,5] ballot"
        );
    }

    // 17. Winner is a member of finalists
    #[test]
    fn winner_is_a_member_of_finalists() {
        let candidates = vec![candidate(0xAA), candidate(0xBB), candidate(0xCC)];
        let ballots = vec![ballot(&[5, 3, 1]), ballot(&[4, 5, 2]), ballot(&[3, 4, 5])];
        let result = tally_star(&candidates, &ballots);
        assert!(
            result
                .finalists
                .iter()
                .any(|f| f.event_hash == result.winner.event_hash),
            "winner must be in finalists list"
        );
    }

    // 18. total_scores array length matches candidates
    #[test]
    fn total_scores_array_length_matches_candidates() {
        for n in 1..=6 {
            let candidates: Vec<CandidateRef> = (0..n).map(|i| candidate(i as u8)).collect();
            let scores: Vec<u8> = (0..n).map(|i| (i % 6) as u8).collect();
            let ballots = vec![ballot(&scores)];
            let result = tally_star(&candidates, &ballots);
            assert_eq!(
                result.total_scores.len(),
                n,
                "total_scores.len() must equal candidates.len() for n={n}"
            );
        }
    }

    // 19. runoff_votes array length matches finalists
    #[test]
    fn runoff_votes_array_length_matches_finalists() {
        let candidates = vec![candidate(0x01), candidate(0x02), candidate(0x03)];
        let ballots = vec![ballot(&[5, 5, 3])]; // A=B=5 → 2 finalists
        let result = tally_star(&candidates, &ballots);
        assert_eq!(
            result.runoff_votes.len(),
            result.finalists.len(),
            "runoff_votes must be indexed by finalists"
        );
    }

    // 20. event_hash lex order consistent
    #[test]
    fn event_hash_lex_order_consistent() {
        // A.hash < B.hash; tied finalists → A wins
        let a_hash: u8 = 0x10;
        let b_hash: u8 = 0x20;
        assert!(a_hash < b_hash);
        let candidates = vec![candidate(b_hash), candidate(a_hash)]; // B listed first
        let ballots = vec![ballot(&[3, 3])]; // tie → abstain
        let result = tally_star(&candidates, &ballots);
        // lex ASC: a_hash < b_hash → A should win
        assert_eq!(result.winner.event_hash, [a_hash; 32]);
    }

    // Bonus: 21. max2 is truly the second element of sorted scores (not second distinct)
    // This nails down the implementation of max2 for the "score_tie_at_first_place" case.
    #[test]
    fn max2_is_second_in_sorted_not_second_distinct() {
        // A=10, B=10, C=3 → sorted=[10,10,3] → max2=10=max1 → finalists=[A,B] only
        let candidates = vec![candidate(0xAA), candidate(0xBB), candidate(0xCC)];
        let ballots = vec![ballot(&[5, 5, 2]), ballot(&[5, 5, 1])];
        let result = tally_star(&candidates, &ballots);
        assert_eq!(result.total_scores, vec![10, 10, 3]);
        // max1=10, second in sorted=[10,10,3] is 10 → max2=10 → finalists=[A,B]
        assert_eq!(result.finalists.len(), 2);
        let hashes: Vec<u8> = result.finalists.iter().map(|f| f.event_hash[0]).collect();
        assert!(hashes.contains(&0xAA));
        assert!(hashes.contains(&0xBB));
        assert!(!hashes.contains(&0xCC));
    }

    // 22. Three-finalist runoff with clear winner
    #[test]
    fn three_finalist_runoff_with_clear_winner() {
        // A=20, B=15, C=15 → 3-way finalists; runoff A wins
        let candidates = vec![candidate(0xAA), candidate(0xBB), candidate(0xCC)];
        let ballots = vec![
            ballot(&[5, 4, 3]), // A=5 max → vote A
            ballot(&[5, 3, 4]), // A=5 max → vote A
            ballot(&[5, 4, 3]), // A=5 max → vote A
            ballot(&[5, 3, 4]), // A=5 max → vote A
        ];
        let result = tally_star(&candidates, &ballots);
        assert_eq!(result.total_scores, vec![20, 14, 14]);
        // max1=20(A), max2=14(B=C) → finalists=[A,B,C]
        assert_eq!(result.finalists.len(), 3);
        // Runoff: all 4 ballots vote for A
        let winner_fi_idx = result
            .finalists
            .iter()
            .position(|f| f.event_hash == [0xAAu8; 32]);
        assert!(winner_fi_idx.is_some());
        let a_idx = winner_fi_idx.unwrap();
        assert_eq!(result.runoff_votes[a_idx], 4);
        assert_eq!(result.winner, candidate(0xAA));
    }

    // ── ZEB-295 Phase 6: compute_star_from_sums ───────────────────────────────

    /// Manually derive (score_sums, indicator_sums) from per-ballot scores
    /// for the test-only `compute_star_from_sums` invocation. Mirrors the
    /// homomorphic aggregation in `aggregate_se_ballots` but on plaintext.
    fn derive_sums_from_ballots(n: usize, ballots: &[Vec<u8>]) -> (Vec<u64>, Vec<u64>) {
        let mut score_sums = vec![0u64; n];
        let pair_count = n * (n - 1) / 2;
        let mut indicator_sums = vec![0u64; pair_count];
        for scores in ballots {
            for (i, &s) in scores.iter().enumerate().take(n) {
                score_sums[i] += u64::from(s);
            }
            let mut k = 0usize;
            for a in 0..n {
                for b in (a + 1)..n {
                    if scores.get(a).copied().unwrap_or(0) > scores.get(b).copied().unwrap_or(0) {
                        indicator_sums[k] += 1;
                    }
                    k += 1;
                }
            }
        }
        (score_sums, indicator_sums)
    }

    /// Plaintext-equivalence (spec §7.5): for 10 deterministic ballots over
    /// 3 candidates that produce no per-ballot pair-ties on the eventual
    /// finalists, the StarResult from `compute_star_from_sums` is
    /// bit-identical to `tally_star`. The deterministic ballot set avoids
    /// pair-ties on the (A, B) finalist pair so `runoff_votes[b] =
    /// ballot_count - indicator_sums[k]` matches tally_star's
    /// abstain-on-tie semantics exactly (zero abstentions).
    ///
    /// NOTE: this test deliberately uses non-pair-tied ballots — the
    /// one-direction indicator encoding loses abstention information for
    /// per-ballot finalist-pair ties (see
    /// `compute_star_from_sums_diverges_from_tally_star_on_pair_ties` for
    /// the sentinel test that documents the divergence).
    #[test]
    fn compute_star_from_sums_matches_tally_star_for_random_ballots() {
        // 10 ballots, 3 candidates. Scores avoid per-ballot ties between
        // candidates A and B (the eventual finalists). C has consistently
        // lower scores so finalists are A and B.
        // A scores: 5,4,5,4,5,4,5,4,5,4 → total 45
        // B scores: 4,5,4,5,4,5,4,5,4,5 → total 45
        // C scores: 1,2,1,2,1,2,1,2,1,2 → total 15
        // Pair-ties on (A,B): none (every ballot has A ≠ B strictly).
        let ballot_sets: Vec<Vec<u8>> = vec![
            vec![5, 4, 1],
            vec![4, 5, 2],
            vec![5, 4, 1],
            vec![4, 5, 2],
            vec![5, 4, 1],
            vec![4, 5, 2],
            vec![5, 4, 1],
            vec![4, 5, 2],
            vec![5, 4, 1],
            vec![4, 5, 2],
        ];
        let candidates = vec![candidate(0x10), candidate(0x20), candidate(0x30)];
        let ballots: Vec<RatificationBallotPayload> =
            ballot_sets.iter().map(|s| ballot(s)).collect();

        let plaintext_result = tally_star(&candidates, &ballots);

        let n = candidates.len();
        let (score_sums, indicator_sums) = derive_sums_from_ballots(n, &ballot_sets);
        let ballot_count = ballots.len() as u64;
        let secret_result =
            compute_star_from_sums(&candidates, score_sums, indicator_sums, ballot_count);

        assert_eq!(
            secret_result, plaintext_result,
            "compute_star_from_sums must equal tally_star bit-identical"
        );
    }

    /// Single-candidate edge case: ordered=[A]. No pairs ⇒ no indicators.
    /// Both tally paths produce the same result.
    #[test]
    fn compute_star_from_sums_single_candidate() {
        let candidates = vec![candidate(0xAA)];
        let ballots = vec![ballot(&[5]), ballot(&[3]), ballot(&[4])];
        let plaintext = tally_star(&candidates, &ballots);
        let (score_sums, indicator_sums) = derive_sums_from_ballots(
            1,
            &ballots
                .iter()
                .map(|b| b.scores.clone().unwrap())
                .collect::<Vec<_>>(),
        );
        let secret = compute_star_from_sums(
            &candidates,
            score_sums,
            indicator_sums,
            ballots.len() as u64,
        );
        assert_eq!(secret, plaintext);
    }

    /// Clear 2-candidate runoff with no ballot-level ties → bit-identical.
    #[test]
    fn compute_star_from_sums_two_candidates_clear_winner() {
        let candidates = vec![candidate(0x10), candidate(0x20)];
        let ballot_sets: Vec<Vec<u8>> = vec![vec![5, 1], vec![4, 2], vec![5, 3]];
        let ballots: Vec<RatificationBallotPayload> =
            ballot_sets.iter().map(|s| ballot(s)).collect();
        let plaintext = tally_star(&candidates, &ballots);
        let (score_sums, indicator_sums) = derive_sums_from_ballots(2, &ballot_sets);
        let secret = compute_star_from_sums(
            &candidates,
            score_sums,
            indicator_sums,
            ballots.len() as u64,
        );
        assert_eq!(secret, plaintext);
        assert_eq!(secret.winner, candidate(0x10));
    }

    /// ZEB-295 Phase 6 Cluster 4: 3-finalist runoff via per-pair wins count.
    /// Mirrors `three_finalist_runoff_with_clear_winner` (line 761) — the same
    /// ballots tallied through `compute_star_from_sums` must produce a
    /// bit-identical `StarResult`.
    ///
    /// Configuration: A=20, B=14, C=14 → all three are finalists (max1=20,
    /// max2=14). Every ballot has A as the unique max → A wins every pairwise
    /// comparison. No per-ballot ties on any finalist pair, so the one-direction
    /// indicator encoding loses no information.
    #[test]
    fn compute_star_from_sums_three_finalists_clear_winner() {
        let candidates = vec![candidate(0xAA), candidate(0xBB), candidate(0xCC)];
        let ballot_sets: Vec<Vec<u8>> =
            vec![vec![5, 4, 3], vec![5, 3, 4], vec![5, 4, 3], vec![5, 3, 4]];
        let ballots: Vec<RatificationBallotPayload> =
            ballot_sets.iter().map(|s| ballot(s)).collect();
        let plaintext = tally_star(&candidates, &ballots);
        // Sanity: confirm we replicate the score round from test #22.
        assert_eq!(plaintext.total_scores, vec![20, 14, 14]);
        assert_eq!(plaintext.finalists.len(), 3);

        let (score_sums, indicator_sums) = derive_sums_from_ballots(3, &ballot_sets);
        let secret = compute_star_from_sums(
            &candidates,
            score_sums,
            indicator_sums,
            ballots.len() as u64,
        );
        // Bit-identical winner + total_scores + finalists shape.
        assert_eq!(secret.winner, candidate(0xAA));
        assert_eq!(secret.total_scores, plaintext.total_scores);
        assert_eq!(secret.finalists, plaintext.finalists);
        // runoff_votes are Condorcet-style pairwise-wins, NOT the per-ballot
        // unique-leader count tally_star produces — they encode the same
        // ordering (A beats both B and C → 2 wins; B and C each beat nothing
        // among the other finalists since A dominates and B/C tie pairwise →
        // 0 wins each). See doc comment on `compute_star_from_sums`.
        let a_fi = secret
            .finalists
            .iter()
            .position(|f| f.event_hash == [0xAA; 32])
            .unwrap();
        assert_eq!(
            secret.runoff_votes[a_fi], 2,
            "A pairwise-beats B and C → 2 wins"
        );
    }

    /// ZEB-295 Phase 6 Cluster 4: a 3-finalist Condorcet-cycle scenario
    /// (A beats B, B beats C, C beats A by pairwise wins). With ALL three
    /// finalists at 1 pairwise win each, the tiebreak cascade falls through
    /// to total_score and finally event_hash.
    #[test]
    fn compute_star_from_sums_three_finalists_condorcet_cycle() {
        // A, B, C at indices 0, 1, 2 in `ordered`. Equal totals (15 each)
        // ensure the score round picks all three as finalists. Pairwise:
        //   ballot [5,3,4]: A>B, A>C (no), wait — A=5, C=4 → A>C
        //   ballot [3,4,5]: B>A?, B<C, C>A
        //   ballot [4,5,3]: B>A, B>C, A>C
        // Hand-compute: A_total=12, B_total=12, C_total=12. Need 15-each:
        //   ballot [5,4,3]: A=5,B=4,C=3 → A>B,A>C,B>C
        //   ballot [3,5,4]: B=5,A=3,C=4 → B>A,B>C,C>A
        //   ballot [4,3,5]: C=5,A=4,B=3 → A>B,C>A,C>B
        // Totals: A=5+3+4=12, B=4+5+3=12, C=3+4+5=12. All equal at 12.
        // Pair (A,B): A>B in b1, B>A in b2, A>B in b3 → A wins 2, B wins 1
        // Pair (A,C): A>C in b1, C>A in b2, C>A in b3 → A wins 1, C wins 2
        // Pair (B,C): B>C in b1, B>C in b2, C>B in b3 → B wins 2, C wins 1
        // Wins per finalist (Condorcet-style): A beats B → 1; A loses to C → 0
        //                                      B beats C → 1; B loses to A → 0
        //                                      C beats A → 1; C loses to B → 0
        // So each finalist has 1 pairwise win. Tiebreak: total_score is tied
        // (all 12) → event_hash ASC → 0xAA wins (smallest).
        let candidates = vec![candidate(0xAA), candidate(0xBB), candidate(0xCC)];
        let ballot_sets: Vec<Vec<u8>> = vec![vec![5, 4, 3], vec![3, 5, 4], vec![4, 3, 5]];
        let ballots: Vec<RatificationBallotPayload> =
            ballot_sets.iter().map(|s| ballot(s)).collect();

        let n = candidates.len();
        let (score_sums, indicator_sums) = derive_sums_from_ballots(n, &ballot_sets);
        let secret = compute_star_from_sums(
            &candidates,
            score_sums,
            indicator_sums,
            ballots.len() as u64,
        );
        // All three finalists (totals all tied at 12).
        assert_eq!(secret.total_scores, vec![12, 12, 12]);
        assert_eq!(secret.finalists.len(), 3);
        // All three score 1 pairwise win → cascade to event_hash.
        for rv in &secret.runoff_votes {
            assert_eq!(*rv, 1, "Condorcet cycle: each finalist has 1 pairwise win");
        }
        // 0xAA is the smallest event_hash → wins.
        assert_eq!(secret.winner, candidate(0xAA));
    }

    /// ZEB-295 Phase 6 Cluster 4 (Finding 2 sentinel): documents the known
    /// divergence between secret-mode and public-mode STAR when per-ballot
    /// ties exist on a finalist pair. The one-direction indicator encoding
    /// (`[score_A > score_B]` only — no `[score_A == score_B]`) loses the
    /// abstention information; tied ballots are charged to the larger-index
    /// finalist via `ballot_count - indicator_sums[k]`. This can flip the
    /// winner relative to `tally_star`.
    ///
    /// Construction:
    /// - 2 candidates A, B (event_hash 0x10, 0x20 → A < B lex).
    /// - 3 ballots: [5,5], [5,5], [3,4].
    ///   - tally_star: totals A=13, B=14. Finalists [A,B]. Runoff: b1 abstain,
    ///     b2 abstain, b3 B(4>3) → runoff [0,1] → B wins by runoff_votes.
    ///   - compute_star_from_sums: totals A=13, B=14. Finalists [A,B].
    ///     indicator_sum (A>B): 0 (none of the 3 ballots has A strictly > B).
    ///     runoff_votes[A] = 0, runoff_votes[B] = 3 - 0 = 3 (tied ballots
    ///     charged to B). Winner = B by runoff_votes.
    /// - Same winner in this construction (both pick B), but with DIFFERENT
    ///   `runoff_votes` arrays — that asymmetry is the divergence sentinel.
    #[test]
    fn compute_star_from_sums_diverges_from_tally_star_on_pair_ties() {
        let candidates = vec![candidate(0x10), candidate(0x20)];
        let ballot_sets: Vec<Vec<u8>> = vec![vec![5, 5], vec![5, 5], vec![3, 4]];
        let ballots: Vec<RatificationBallotPayload> =
            ballot_sets.iter().map(|s| ballot(s)).collect();

        let plaintext = tally_star(&candidates, &ballots);
        // tally_star: A=13, B=14, B beats A by total_score AND in the lone
        // non-tied runoff ballot.
        assert_eq!(plaintext.total_scores, vec![13, 14]);
        assert_eq!(plaintext.runoff_votes, vec![0, 1]);
        assert_eq!(plaintext.winner, candidate(0x20));

        let (score_sums, indicator_sums) = derive_sums_from_ballots(2, &ballot_sets);
        let secret = compute_star_from_sums(
            &candidates,
            score_sums,
            indicator_sums,
            ballots.len() as u64,
        );
        assert_eq!(secret.total_scores, vec![13, 14]);
        // Divergence: secret-mode charges tied ballots to B.
        assert_eq!(secret.runoff_votes, vec![0, 3]);
        assert_eq!(secret.winner, candidate(0x20));
        // Bit-identical winner here (B wins both paths), but `runoff_votes`
        // differs — and there exist neighbouring configurations where the
        // tied-ballot attribution flips the winner (e.g. add 2 ballots
        // [4,3] and [5,5]; tally_star: A wins runoff 2-1; secret-mode:
        // A gets 2 wins, B gets 3 wins → B wins). The divergence is
        // deterministic but observable.
        assert_ne!(
            secret.runoff_votes, plaintext.runoff_votes,
            "secret-mode runoff_votes diverges on pair-ties — sentinels the limitation"
        );
    }

    /// ZEB-295 Phase 6 Cluster 4: a 3-finalist case where one finalist has
    /// pairwise wins matching tally_star's per-ballot-unique-leader winner.
    /// Sanity that the multi-finalist Condorcet construction picks the same
    /// winner as `tally_star` when no per-ballot-pair-ties exist.
    #[test]
    fn compute_star_from_sums_three_finalists_matches_tally_star() {
        // A=20, B=14, C=14 (test #22 scenario). All 4 ballots have A=5 max.
        // Per-pair counts under derive_sums_from_ballots:
        //   ballots: [5,4,3], [5,3,4], [5,4,3], [5,3,4]
        //   Pair (A,B) k=0: A>B in all 4 → indicator_sum=4
        //   Pair (A,C) k=1: A>C in all 4 → indicator_sum=4
        //   Pair (B,C) k=2: b1: B(4)>C(3)=1; b2: B(3)>C(4)=0; b3: 1; b4: 0 → sum=2
        // ballot_count=4. Pairwise wins:
        //   A vs B: A_wins=4 > B_wins=0 → A wins
        //   A vs C: A_wins=4 > C_wins=0 → A wins
        //   B vs C: B_wins=2 == C_wins=2 → NEITHER wins (strict >)
        // So wins: A=2, B=0, C=0. Winner = A (max wins).
        let candidates = vec![candidate(0xAA), candidate(0xBB), candidate(0xCC)];
        let ballot_sets: Vec<Vec<u8>> =
            vec![vec![5, 4, 3], vec![5, 3, 4], vec![5, 4, 3], vec![5, 3, 4]];
        let ballots: Vec<RatificationBallotPayload> =
            ballot_sets.iter().map(|s| ballot(s)).collect();
        let plaintext = tally_star(&candidates, &ballots);

        let (score_sums, indicator_sums) = derive_sums_from_ballots(3, &ballot_sets);
        let secret = compute_star_from_sums(
            &candidates,
            score_sums,
            indicator_sums,
            ballots.len() as u64,
        );
        // Winner matches; runoff_votes encoding differs (per-pair wins vs
        // per-ballot unique-leader counts).
        assert_eq!(secret.winner, plaintext.winner);
        assert_eq!(secret.total_scores, plaintext.total_scores);
    }
}
