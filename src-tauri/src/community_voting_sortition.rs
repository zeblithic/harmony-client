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
/// `beacon = SHA-256( poll_create_hash || u32::to_le_bytes(community_epoch) )`
///
/// Deterministic: identical inputs always produce identical output.
pub fn derive_beacon_seed(poll_create_hash: &[u8; 32], community_epoch: u32) -> [u8; 32] {
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
