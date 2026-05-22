//! ZEB-309 Phase 4a-main: Tier 3 sortition — VRF-seeded Fisher-Yates selection.
//!
//! See spec `docs/specs/2026-05-20-zeb-309-phase4a-main-design.md` §7.
//!
//! This module provides **pure, deterministic** functions only. No I/O,
//! no Tauri state, no async. The caller is responsible for obtaining the
//! VRF output and invoking these helpers.

use crate::owner_state_types::OwnerAddr;
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;
use sha2::{Digest, Sha256};

// ── Public types ──────────────────────────────────────────────────────────────

/// Output of a Tier 3 sortition draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortitionResult {
    /// Primary mini-public members, drawn first from the shuffled electorate.
    pub primary: Vec<OwnerAddr>,
    /// Backup pool members, drawn immediately after primary.
    pub backup: Vec<OwnerAddr>,
}

// ── Public functions ──────────────────────────────────────────────────────────

/// Derive the beacon seed used to parameterise the VRF input.
///
/// `beacon = SHA-256( poll_create_hash || u64::to_le_bytes(community_epoch) )`
///
/// Deterministic: identical inputs always produce identical output.
/// `community_epoch` is `u64` to match `DfrostLogEngine::current_epoch()`.
pub fn derive_beacon_seed(poll_create_hash: &[u8; 32], community_epoch: u64) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(poll_create_hash);
    hasher.update(community_epoch.to_le_bytes());
    hasher.finalize().into()
}

/// Return a deterministic, canonical ordering of the electorate.
///
/// Members are sorted by their `OwnerAddr` bytes in ascending lexicographic
/// order. This eliminates input-order ambiguity so that `fisher_yates_select`
/// is invariant under arbitrary permutations of the supplied slice.
pub fn canonical_electorate_order(electorate: &[OwnerAddr]) -> Vec<OwnerAddr> {
    let mut sorted = electorate.to_vec();
    sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    sorted
}

/// Select `primary_size` primary members and `backup_size` backup members from
/// the electorate using a Fisher-Yates shuffle seeded by `vrf_output`.
///
/// Steps:
/// 1. Canonicalise the electorate (sort by `OwnerAddr` lex ASC).
/// 2. Seed a `ChaCha20Rng` from the first 32 bytes of `vrf_output`.
/// 3. Perform a partial Fisher-Yates shuffle (draw `primary_size + backup_size`
///    elements from the front of the shuffled array).
/// 4. Return the first `primary_size` elements as `primary` and the next
///    `backup_size` as `backup`.
///
/// # Panics
///
/// Panics with `"electorate too small"` if
/// `electorate.len() < primary_size + backup_size`.
pub fn fisher_yates_select(
    vrf_output: &[u8; 32],
    electorate: &[OwnerAddr],
    primary_size: usize,
    backup_size: usize,
) -> SortitionResult {
    let total_needed = primary_size + backup_size;
    assert!(
        electorate.len() >= total_needed,
        "electorate too small: need {total_needed} but have {}",
        electorate.len(),
    );

    // Step 1: canonical order.
    let mut pool = canonical_electorate_order(electorate);

    // Step 2: seed RNG.
    let mut rng = ChaCha20Rng::from_seed(*vrf_output);

    // Step 3: partial Fisher-Yates — only draw as many elements as we need.
    let n = pool.len();
    for i in 0..total_needed {
        // Pick a random index in [i, n).
        let j = i + rand::Rng::gen_range(&mut rng, 0..(n - i));
        pool.swap(i, j);
    }

    // Step 4: slice out results.
    let primary = pool[..primary_size].to_vec();
    let backup = pool[primary_size..total_needed].to_vec();
    SortitionResult { primary, backup }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::OwnerAddr;

    fn make_electorate(n: usize) -> Vec<OwnerAddr> {
        (0..n).map(|i| OwnerAddr([i as u8; 16])).collect()
    }

    #[test]
    fn derive_beacon_seed_deterministic() {
        let h = [0x42u8; 32];
        let s1 = derive_beacon_seed(&h, 7);
        let s2 = derive_beacon_seed(&h, 7);
        assert_eq!(s1, s2);
    }

    #[test]
    fn derive_beacon_seed_changes_with_epoch() {
        let h = [0x42u8; 32];
        let s1 = derive_beacon_seed(&h, 7);
        let s2 = derive_beacon_seed(&h, 8);
        assert_ne!(s1, s2);
    }

    #[test]
    fn derive_beacon_seed_changes_with_hash() {
        let h1 = [0x42u8; 32];
        let mut h2 = [0x42u8; 32];
        h2[0] = 0x43;
        let s1 = derive_beacon_seed(&h1, 7);
        let s2 = derive_beacon_seed(&h2, 7);
        assert_ne!(s1, s2);
    }

    #[test]
    fn canonical_order_idempotent() {
        let e = make_electorate(10);
        let c1 = canonical_electorate_order(&e);
        let c2 = canonical_electorate_order(&c1);
        assert_eq!(c1, c2);
    }

    #[test]
    fn canonical_order_invariant_under_input_permutation() {
        let e1 = make_electorate(10);
        let mut e2 = e1.clone();
        e2.reverse();
        assert_eq!(
            canonical_electorate_order(&e1),
            canonical_electorate_order(&e2)
        );
    }

    #[test]
    fn fisher_yates_deterministic() {
        let e = make_electorate(20);
        let vrf = [0x55u8; 32];
        let r1 = fisher_yates_select(&vrf, &e, 5, 5);
        let r2 = fisher_yates_select(&vrf, &e, 5, 5);
        assert_eq!(r1, r2);
    }

    #[test]
    fn fisher_yates_different_seeds_yield_different_results() {
        let e = make_electorate(20);
        let r1 = fisher_yates_select(&[0x01u8; 32], &e, 5, 5);
        let r2 = fisher_yates_select(&[0x02u8; 32], &e, 5, 5);
        assert_ne!(r1, r2);
    }

    #[test]
    fn fisher_yates_primary_and_backup_sizes_correct() {
        let e = make_electorate(30);
        let r = fisher_yates_select(&[0u8; 32], &e, 10, 5);
        assert_eq!(r.primary.len(), 10);
        assert_eq!(r.backup.len(), 5);
    }

    #[test]
    fn fisher_yates_primary_and_backup_disjoint() {
        let e = make_electorate(30);
        let r = fisher_yates_select(&[0u8; 32], &e, 10, 5);
        for p in &r.primary {
            assert!(!r.backup.contains(p), "primary {p:?} also in backup");
        }
    }

    #[test]
    fn fisher_yates_all_selections_in_electorate() {
        let e = make_electorate(20);
        let r = fisher_yates_select(&[0u8; 32], &e, 5, 5);
        for p in r.primary.iter().chain(r.backup.iter()) {
            assert!(e.contains(p));
        }
    }

    #[test]
    #[should_panic(expected = "electorate too small")]
    fn fisher_yates_panics_on_small_electorate() {
        let e = make_electorate(5);
        fisher_yates_select(&[0u8; 32], &e, 5, 5);
    }

    #[test]
    fn fisher_yates_canonicalizes_input_order() {
        let e1 = make_electorate(20);
        let mut e2 = e1.clone();
        e2.reverse();
        let r1 = fisher_yates_select(&[0u8; 32], &e1, 5, 5);
        let r2 = fisher_yates_select(&[0u8; 32], &e2, 5, 5);
        assert_eq!(r1, r2, "result must not depend on input order");
    }
}

// ── Phase 5 (Tier 3b) bridging-statement detection ─────────────────────────

pub mod bridging {
    //! Diversity-of-Supporters (DoS) bridging-statement detection per
    //! ZEB-294 spec §4. Pure integer arithmetic (Q32/Q64 fixed-point);
    //! no f64 in the sort path. Deterministic across engines.

    use crate::community_voting_core::BridgingVoteCode;
    use crate::community_voting_tier3::{DeliberationStatement, DeliberationVoteEntry};
    use crate::owner_state_types::OwnerAddr;
    use std::collections::{BTreeMap, HashSet};

    /// Per-statement bridging output. Sort by (`bridging_score_q64` DESC,
    /// `statement_event_hash` ASC) for deterministic ordering across engines.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct BridgingScore {
        pub statement_event_hash: [u8; 32],
        pub statement_text: String,
        pub author: OwnerAddr,
        pub agree_count: u16,
        pub disagree_count: u16,
        pub pass_count: u16,
        /// Q32 fixed-point dissimilarity fraction in `[0, 2^32]`.
        pub diversity_q32: u64,
        /// `agree_count` × `diversity_q32`. Sort key.
        pub bridging_score_q64: u64,
    }

    /// Compute bridging scores for all statements. Deterministic given
    /// the same `(statements, votes, mini_public)` triple.
    ///
    /// Complexity: O(M² × S) precompute + O(S × |supporters|²) per stmt.
    /// Typical M=100 S=150 → ~3M ops; negligible.
    pub fn compute_bridging_scores(
        statements: &BTreeMap<[u8; 32], DeliberationStatement>,
        votes: &BTreeMap<(OwnerAddr, [u8; 32]), DeliberationVoteEntry>,
        mini_public: &HashSet<OwnerAddr>,
    ) -> Vec<BridgingScore> {
        // Sorted member list — BTreeSet would also work, but we need indexed
        // access for pairwise iteration.
        let mut members: Vec<OwnerAddr> = mini_public.iter().copied().collect();
        members.sort();

        // Sorted statement keys (BTreeMap iter is already by-key).
        let stmt_keys: Vec<[u8; 32]> = statements.keys().copied().collect();

        let n_members = members.len();
        let n_stmts = stmt_keys.len();

        // Per-member vote vector V_m[stmt_idx]: +1 agree / -1 disagree / 0 pass-or-no-vote.
        let mut vote_vec: Vec<Vec<i8>> = vec![vec![0i8; n_stmts]; n_members];
        for (mi, m) in members.iter().enumerate() {
            for (si, s) in stmt_keys.iter().enumerate() {
                if let Some(entry) = votes.get(&(*m, *s)) {
                    vote_vec[mi][si] = match entry.vote {
                        BridgingVoteCode::Agree => 1,
                        BridgingVoteCode::Disagree => -1,
                        BridgingVoteCode::Pass => 0,
                    };
                }
            }
        }

        // Precompute upper-triangular pairwise dissimilarity (Q32 fixed-point).
        // For n_members < 2 there are no pairs and we leave this empty.
        let pair_dissim: Vec<u64> = if n_members < 2 {
            Vec::new()
        } else {
            let mut out = Vec::with_capacity((n_members * (n_members - 1)) / 2);
            for i in 0..n_members {
                for j in (i + 1)..n_members {
                    let mut joint_support: u64 = 0;
                    let mut disagree_count: u64 = 0;
                    for (&a, &b) in vote_vec[i].iter().zip(vote_vec[j].iter()) {
                        if a != 0 && b != 0 {
                            joint_support += 1;
                            if a != b {
                                disagree_count += 1;
                            }
                        }
                    }
                    let d = if joint_support == 0 {
                        0u64
                    } else {
                        (disagree_count << 32) / joint_support
                    };
                    out.push(d);
                }
            }
            out
        };

        // Triangular index helper: for i<j, position in flat vec.
        let pair_index = |i: usize, j: usize| -> usize {
            debug_assert!(i < j);
            i * (2 * n_members - i - 1) / 2 + (j - i - 1)
        };

        // Per-statement bridging score.
        let mut scores: Vec<BridgingScore> = Vec::with_capacity(n_stmts);
        for s_hash in stmt_keys.iter() {
            let stmt = &statements[s_hash];

            let mut agree_indices: Vec<usize> = Vec::new();
            let mut agree_count: u16 = 0;
            let mut disagree_count: u16 = 0;
            let mut pass_count: u16 = 0;
            for (mi, m) in members.iter().enumerate() {
                if let Some(entry) = votes.get(&(*m, *s_hash)) {
                    match entry.vote {
                        BridgingVoteCode::Agree => {
                            agree_indices.push(mi);
                            agree_count += 1;
                        }
                        BridgingVoteCode::Disagree => disagree_count += 1,
                        BridgingVoteCode::Pass => pass_count += 1,
                    }
                }
            }

            let diversity_q32 = if agree_indices.len() < 2 {
                0u64
            } else {
                let mut sum_d: u64 = 0;
                let mut pair_count: u64 = 0;
                for ai in 0..agree_indices.len() {
                    for aj in (ai + 1)..agree_indices.len() {
                        let (i, j) = (agree_indices[ai], agree_indices[aj]);
                        sum_d += pair_dissim[pair_index(i, j)];
                        pair_count += 1;
                    }
                }
                if pair_count == 0 {
                    0
                } else {
                    sum_d / pair_count
                }
            };

            let bridging_score_q64 = (agree_count as u64) * diversity_q32;

            scores.push(BridgingScore {
                statement_event_hash: *s_hash,
                statement_text: stmt.text.clone(),
                author: stmt.author,
                agree_count,
                disagree_count,
                pass_count,
                diversity_q32,
                bridging_score_q64,
            });
        }

        // Sort by (bridging_score_q64 DESC, statement_event_hash ASC).
        scores.sort_by(|a, b| {
            b.bridging_score_q64
                .cmp(&a.bridging_score_q64)
                .then(a.statement_event_hash.cmp(&b.statement_event_hash))
        });

        scores
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::community_voting_core::BridgingVoteCode;
        use crate::community_voting_tier3::{DeliberationStatement, DeliberationVoteEntry};
        use crate::owner_state_types::{Hlc, OwnerAddr};
        use std::collections::{BTreeMap, HashSet};

        fn hash(b: u8) -> [u8; 32] {
            [b; 32]
        }
        fn addr(b: u8) -> OwnerAddr {
            OwnerAddr([b; 16])
        }
        fn hlc(ms: u64) -> Hlc {
            Hlc {
                wall_ms: ms,
                logical: 0,
                device_id: "test".to_string(),
            }
        }

        fn stmt(author: OwnerAddr, h: [u8; 32], text: &str) -> DeliberationStatement {
            DeliberationStatement {
                event_hash: h,
                author,
                text: text.to_string(),
                created_at_hlc: hlc(1000),
            }
        }

        fn vote(v: OwnerAddr, sh: [u8; 32], code: BridgingVoteCode) -> DeliberationVoteEntry {
            DeliberationVoteEntry {
                voter: v,
                statement_event_hash: sh,
                vote: code,
                last_update_hlc: hlc(2000),
                last_update_event_hash: [0; 32],
            }
        }

        #[test]
        fn empty_inputs_return_empty() {
            let res = compute_bridging_scores(&BTreeMap::new(), &BTreeMap::new(), &HashSet::new());
            assert!(res.is_empty());
        }

        #[test]
        fn single_statement_zero_supporters_scores_zero() {
            let mut statements = BTreeMap::new();
            statements.insert(hash(1), stmt(addr(1), hash(1), "S1"));
            let votes = BTreeMap::new();
            let mp: HashSet<_> = (1..=3).map(addr).collect();
            let res = compute_bridging_scores(&statements, &votes, &mp);
            assert_eq!(res.len(), 1);
            assert_eq!(res[0].agree_count, 0);
            assert_eq!(res[0].bridging_score_q64, 0);
        }

        #[test]
        fn single_supporter_yields_zero_diversity() {
            let mut statements = BTreeMap::new();
            statements.insert(hash(1), stmt(addr(1), hash(1), "S1"));
            let mut votes = BTreeMap::new();
            votes.insert(
                (addr(2), hash(1)),
                vote(addr(2), hash(1), BridgingVoteCode::Agree),
            );
            let mp: HashSet<_> = (1..=3).map(addr).collect();
            let res = compute_bridging_scores(&statements, &votes, &mp);
            assert_eq!(res[0].agree_count, 1);
            assert_eq!(res[0].diversity_q32, 0);
            assert_eq!(res[0].bridging_score_q64, 0);
        }

        #[test]
        fn cross_cluster_supporters_outscore_lockstep() {
            // Scenario A — lockstep: 5 supporters all agree on s1 and s2.
            // Pairwise disagreement is 0 → diversity_q32 = 0 → bridging = 0.
            let s1 = hash(1);
            let s2 = hash(2);
            let mut statements_a = BTreeMap::new();
            statements_a.insert(s1, stmt(addr(0xAA), s1, "S1"));
            statements_a.insert(s2, stmt(addr(0xAA), s2, "S2"));
            let mut votes_a = BTreeMap::new();
            for v in 1..=5u8 {
                votes_a.insert((addr(v), s1), vote(addr(v), s1, BridgingVoteCode::Agree));
                votes_a.insert((addr(v), s2), vote(addr(v), s2, BridgingVoteCode::Agree));
            }
            let mp_a: HashSet<_> = (1..=5u8).map(addr).collect();
            let r_a = compute_bridging_scores(&statements_a, &votes_a, &mp_a);
            let s1_a = r_a
                .iter()
                .find(|b| b.statement_event_hash == s1)
                .unwrap()
                .bridging_score_q64;

            // Scenario B — cross-cluster: same 5 supporters agree on s1 and s2
            // BUT alternate on s3 (odd voters Agree, even Disagree).
            let s3 = hash(3);
            let mut statements_b = BTreeMap::new();
            statements_b.insert(s1, stmt(addr(0xAA), s1, "S1"));
            statements_b.insert(s2, stmt(addr(0xAA), s2, "S2"));
            statements_b.insert(s3, stmt(addr(0xAA), s3, "S3"));
            let mut votes_b = BTreeMap::new();
            for v in 1..=5u8 {
                votes_b.insert((addr(v), s1), vote(addr(v), s1, BridgingVoteCode::Agree));
                votes_b.insert((addr(v), s2), vote(addr(v), s2, BridgingVoteCode::Agree));
                let s3_vote = if v % 2 == 1 {
                    BridgingVoteCode::Agree
                } else {
                    BridgingVoteCode::Disagree
                };
                votes_b.insert((addr(v), s3), vote(addr(v), s3, s3_vote));
            }
            let mp_b = mp_a.clone();
            let r_b = compute_bridging_scores(&statements_b, &votes_b, &mp_b);
            let s1_b = r_b
                .iter()
                .find(|b| b.statement_event_hash == s1)
                .unwrap()
                .bridging_score_q64;

            assert_eq!(
                s1_a, 0,
                "lockstep supporters have zero diversity → zero bridging"
            );
            assert!(
                s1_b > s1_a,
                "cross-cluster supporters score higher: {s1_b} > {s1_a}"
            );
        }

        #[test]
        fn determinism_same_logical_inputs_same_output() {
            let s1 = hash(1);
            let s2 = hash(2);
            let mut statements = BTreeMap::new();
            statements.insert(s2, stmt(addr(0xBB), s2, "S2"));
            statements.insert(s1, stmt(addr(0xAA), s1, "S1"));
            let mut votes_a = BTreeMap::new();
            votes_a.insert((addr(1), s1), vote(addr(1), s1, BridgingVoteCode::Agree));
            votes_a.insert((addr(2), s1), vote(addr(2), s1, BridgingVoteCode::Agree));
            votes_a.insert((addr(3), s1), vote(addr(3), s1, BridgingVoteCode::Disagree));
            votes_a.insert((addr(1), s2), vote(addr(1), s2, BridgingVoteCode::Disagree));
            votes_a.insert((addr(2), s2), vote(addr(2), s2, BridgingVoteCode::Agree));
            let mut votes_b = BTreeMap::new();
            for (k, v) in votes_a.iter() {
                votes_b.insert(*k, v.clone());
            }
            let mp: HashSet<_> = (1..=3u8).map(addr).collect();
            let r_a = compute_bridging_scores(&statements, &votes_a, &mp);
            let r_b = compute_bridging_scores(&statements, &votes_b, &mp);
            assert_eq!(r_a, r_b);
        }

        #[test]
        fn sort_tie_break_by_statement_hash_ascending() {
            // Two statements with identical bridging_score_q64 (both zero):
            // smaller hash sorts first.
            let s_lo = hash(1);
            let s_hi = hash(2);
            let mut statements = BTreeMap::new();
            statements.insert(s_hi, stmt(addr(0xAA), s_hi, "Hi-hash"));
            statements.insert(s_lo, stmt(addr(0xAA), s_lo, "Lo-hash"));
            let res = compute_bridging_scores(&statements, &BTreeMap::new(), &HashSet::new());
            assert_eq!(res[0].statement_event_hash, s_lo);
            assert_eq!(res[1].statement_event_hash, s_hi);
        }

        #[test]
        fn pair_index_arithmetic_is_correct() {
            // n=4 → 6 pairs in upper-triangular indexing.
            let n: usize = 4;
            let pair_index =
                |i: usize, j: usize| -> usize { i * (2 * n - i - 1) / 2 + (j - i - 1) };
            assert_eq!(pair_index(0, 1), 0);
            assert_eq!(pair_index(0, 2), 1);
            assert_eq!(pair_index(0, 3), 2);
            assert_eq!(pair_index(1, 2), 3);
            assert_eq!(pair_index(1, 3), 4);
            assert_eq!(pair_index(2, 3), 5);
        }
    }
}
