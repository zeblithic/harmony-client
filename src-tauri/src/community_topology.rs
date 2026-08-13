//! Bounded-degree community topology (ZEB-914, R4).
//!
//! Pure, deterministic neighbor selection for one community's device ring.
//! See `docs/superpowers/specs/2026-08-13-zeb914-bounded-degree-topology-design.md`.
//!
//! The ring places every active device at `H(community_salt ‖ device_key)`;
//! neighbors are the devices at ring-ranks `self ± offset` where the offsets are
//! the powers of two up to `N/2`. The `±1` offset (protected lattice) makes the
//! graph connected by construction; the doubling powers give **O(log N) diameter
//! at O(log N) degree** — Chord's law. The ratio is fixed at 2, so the offset
//! count (and hence the degree) grows as `⌊log₂(N/2)⌋ + 1`. All nodes derive the
//! same offset set from `N`, so every edge is symmetric — a hard degree bound
//! with no pruning.
//!
//! Degree is therefore ~2·log₂N: 14 at 200 devices, 16 at 400, 22 at 4000 — small
//! and bounded, and the honest price of genuine logarithmic diameter. (A fixed
//! *count* of geometric offsets would keep the degree constant but make the ratio
//! grow with N, giving polynomial O(N^…) diameter instead — see the R4 review.)
//!
//! Cost: selecting one node's neighbors is dominated by the O(N log N) ring sort
//! ([`ring_order`]), done once per membership change. A caller computing many
//! nodes' sets (the wiring's harness, a simulation) should call [`ring_order`]
//! once and [`neighbors_on_ring`] per node — not [`community_neighbors`] N times.
//!
//! UNWIRED: this module has no live call sites. R4's hot-path wiring (kick-inflow
//! filter, device-key→node_id resolver bridge, router-mode gate) is a follow-up
//! ticket, ZEB-928, under the ZEB-909 epic.

use std::collections::BTreeSet;

/// Below this many active devices, a community stays full mesh (cheap at small N
/// per the R3 sounding; the bounded ring is only worth its diameter cost above it).
pub const FULL_MESH_THRESHOLD: usize = 32;

/// Ring coordinate for a device in a given community. Uniform over `u64`.
fn ring_position(community_salt: &[u8], device: &[u8; 32]) -> u64 {
    let mut buf = Vec::with_capacity(community_salt.len() + 32);
    buf.extend_from_slice(community_salt);
    buf.extend_from_slice(device);
    let h = harmony_crypto::hash::blake3_hash(&buf);
    u64::from_be_bytes(h[..8].try_into().expect("blake3 output is 32 bytes"))
}

/// Canonical cyclic order of `devices` by ring position; ties broken by key
/// bytes so every node computes the identical ring.
///
/// Public so a caller computing many nodes' neighbor sets can sort once and pass
/// the result to [`neighbors_on_ring`] repeatedly — O(N log N) once, not per node.
pub fn ring_order(devices: &BTreeSet<[u8; 32]>, community_salt: &[u8]) -> Vec<[u8; 32]> {
    let mut ordered: Vec<[u8; 32]> = devices.iter().copied().collect();
    ordered.sort_by_key(|d| (ring_position(community_salt, d), *d));
    ordered
}

/// Power-of-two offset set `{1, 2, 4, …, 2^k}` with `2^k ≤ n/2`. The ratio is
/// fixed at 2, so the count grows as `⌊log₂(n/2)⌋ + 1` — this is what makes the
/// diameter O(log n) (a fixed *count* with a growing ratio would give polynomial
/// diameter). Offset 1 is the protected lattice; offsets above `n/2` are
/// redundant on an `n`-cycle. Empty for `n < 2`.
fn ring_offsets(n: usize) -> BTreeSet<usize> {
    let max_off = n / 2;
    let mut offs = BTreeSet::new();
    if max_off == 0 {
        return offs;
    }
    let mut o = 1usize;
    loop {
        offs.insert(o);
        match o.checked_mul(2) {
            Some(next) if next <= max_off => o = next,
            _ => break,
        }
    }
    offs
}

/// Bounded neighbor set for `self_device` on a pre-sorted `ring` (from
/// [`ring_order`]). Selects the devices at ring-ranks `self ± offset` for the
/// power-of-two offsets; O(log n) work beyond locating `self`. Does **not**
/// re-sort — pass the same `ring` when computing many nodes' sets.
///
/// Returns empty if `self_device` is not on `ring`. Below [`FULL_MESH_THRESHOLD`]
/// devices, returns all-but-self (full mesh).
pub fn neighbors_on_ring(ring: &[[u8; 32]], self_device: &[u8; 32]) -> BTreeSet<[u8; 32]> {
    let n = ring.len();
    let self_rank = match ring.iter().position(|d| d == self_device) {
        Some(r) => r,
        None => return BTreeSet::new(),
    };
    if n < FULL_MESH_THRESHOLD {
        return ring.iter().copied().filter(|d| d != self_device).collect();
    }
    let mut neighbors = BTreeSet::new();
    for o in ring_offsets(n) {
        // +o and -o (mod n). All nodes share this offset set, so the edge is
        // symmetric: the node at rank+o computes this node at its own rank-o.
        let fwd = ring[(self_rank + o) % n];
        let bwd = ring[(self_rank + n - o) % n];
        if fwd != *self_device {
            neighbors.insert(fwd);
        }
        if bwd != *self_device {
            neighbors.insert(bwd);
        }
    }
    neighbors
}

/// Deterministic bounded-degree neighbor selection for one community's device ring.
///
/// - `devices`: all active (Joined, post-revocation) enrolled device keys,
///   INCLUDING `self_device`.
/// - `self_device`: this node's enrolled device key; must be in `devices`.
/// - `community_salt`: community id bytes — decorrelates ring positions per community.
///
/// Returns the subset of `devices` this node keeps persistent links to (never
/// includes `self_device`); degree ~2·log₂N above the threshold. Sorts the ring
/// once ([`ring_order`]) then delegates to [`neighbors_on_ring`]; to compute many
/// nodes' sets, sort once yourself and call [`neighbors_on_ring`] per node.
///
/// Deterministic and symmetric: for any `a`, `b` in `devices`,
/// `b ∈ community_neighbors(a, …)` ⟺ `a ∈ community_neighbors(b, …)`.
pub fn community_neighbors(
    devices: &BTreeSet<[u8; 32]>,
    self_device: &[u8; 32],
    community_salt: &[u8],
) -> BTreeSet<[u8; 32]> {
    if !devices.contains(self_device) {
        return BTreeSet::new();
    }
    let ring = ring_order(devices, community_salt);
    neighbors_on_ring(&ring, self_device)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet, VecDeque};

    /// Deterministic distinct synthetic device keys for property tests.
    fn synth_devices(n: usize) -> BTreeSet<[u8; 32]> {
        (0..n)
            .map(|i| harmony_crypto::hash::blake3_hash(&(i as u64).to_be_bytes()))
            .collect()
    }

    fn synth_key(tag: u64) -> [u8; 32] {
        harmony_crypto::hash::blake3_hash(&tag.to_be_bytes())
    }

    // ---- ring order ----

    #[test]
    fn ring_order_is_deterministic_and_a_permutation() {
        let devices = synth_devices(50);
        let a = ring_order(&devices, b"community-A");
        let b = ring_order(&devices, b"community-A");
        assert_eq!(a, b);
        assert_eq!(a.len(), devices.len());
        assert_eq!(a.iter().copied().collect::<BTreeSet<_>>(), devices);
    }

    #[test]
    fn ring_order_decorrelates_across_communities() {
        let devices = synth_devices(200);
        assert_ne!(
            ring_order(&devices, b"community-A"),
            ring_order(&devices, b"community-B")
        );
    }

    // ---- API + threshold + guards ----

    #[test]
    fn full_mesh_below_threshold() {
        let devices = synth_devices(20);
        for a in &devices {
            let nb = community_neighbors(&devices, a, b"c");
            assert_eq!(nb.len(), devices.len() - 1);
            assert!(!nb.contains(a));
            assert!(nb.is_subset(&devices));
        }
    }

    #[test]
    fn non_member_self_gets_no_neighbors() {
        let devices = synth_devices(50);
        let stranger = synth_key(999_999);
        assert!(!devices.contains(&stranger));
        assert!(community_neighbors(&devices, &stranger, b"c").is_empty());
    }

    // ---- offsets: powers of two ----

    #[test]
    fn offsets_are_exactly_powers_of_two_up_to_half_ring() {
        for &n in &[32usize, 64, 200, 400, 1000] {
            let offs = ring_offsets(n);
            let expected: BTreeSet<usize> = (0..64)
                .map(|k| 1usize << k)
                .take_while(|&o| o <= n / 2)
                .collect();
            assert_eq!(offs, expected, "n={n}");
            assert!(offs.contains(&1), "protected ±1 lattice present");
            // The largest offset spans a meaningful fraction of the ring, so
            // greedy routing reaches the antipode in O(log n) hops.
            assert!(*offs.iter().max().unwrap() > n / 4, "n={n}");
        }
    }

    #[test]
    fn degree_grows_logarithmically_not_polynomially() {
        // The whole point of the powers-of-two fix: 8× the devices adds ~3
        // doublings (~+6 neighbors), NOT ~8× the degree.
        let node = synth_key(0); // present in both rosters (indices start at 0)
        let d64 = community_neighbors(&synth_devices(64), &node, b"c").len();
        let d512 = community_neighbors(&synth_devices(512), &node, b"c").len();
        assert!(
            d64 >= 2 && d512 > d64,
            "degree grows with N: {d64} -> {d512}"
        );
        assert!(
            d512 <= d64 + 8,
            "degree grows only logarithmically: {d64} -> {d512}"
        );
    }

    // ---- circulant assembly ----

    #[test]
    fn symmetry_holds_above_threshold() {
        let devices = synth_devices(200);
        let ring = ring_order(&devices, b"community-A");
        for a in &devices {
            for b in neighbors_on_ring(&ring, a) {
                assert!(
                    neighbors_on_ring(&ring, &b).contains(a),
                    "edge not mirrored"
                );
            }
        }
    }

    #[test]
    fn degree_bounded_above_threshold() {
        let devices = synth_devices(200);
        let bound = 2 * ring_offsets(devices.len()).len(); // 2 per offset magnitude
        for a in &devices {
            let nb = community_neighbors(&devices, a, b"c");
            assert!(nb.len() <= bound, "{} > {bound}", nb.len());
            assert!(!nb.contains(a));
            assert!(nb.is_subset(&devices));
        }
    }

    #[test]
    fn deterministic_above_threshold() {
        let devices = synth_devices(200);
        let node = *devices.iter().next().unwrap();
        assert_eq!(
            community_neighbors(&devices, &node, b"c"),
            community_neighbors(&devices, &node, b"c")
        );
    }

    #[test]
    fn batched_path_matches_convenience() {
        // ring_order once + neighbors_on_ring per node == community_neighbors N times.
        let devices = synth_devices(200);
        let ring = ring_order(&devices, b"c");
        for a in &devices {
            assert_eq!(
                neighbors_on_ring(&ring, a),
                community_neighbors(&devices, a, b"c")
            );
        }
    }

    // ---- graph invariants ----

    fn adjacency(devices: &BTreeSet<[u8; 32]>, salt: &[u8]) -> (Vec<[u8; 32]>, Vec<Vec<usize>>) {
        // Sort the ring ONCE, then select each node's neighbors from it (O(N log N)
        // + N·O(log N), not O(N² log N)).
        let ring = ring_order(devices, salt);
        let idx: HashMap<[u8; 32], usize> = ring.iter().enumerate().map(|(i, k)| (*k, i)).collect();
        let adj = ring
            .iter()
            .map(|a| neighbors_on_ring(&ring, a).iter().map(|b| idx[b]).collect())
            .collect();
        (ring, adj)
    }

    fn eccentricity(adj: &[Vec<usize>], src: usize) -> usize {
        let mut dist = vec![usize::MAX; adj.len()];
        let mut q = VecDeque::new();
        dist[src] = 0;
        q.push_back(src);
        let mut mx = 0;
        while let Some(u) = q.pop_front() {
            for &v in &adj[u] {
                if dist[v] == usize::MAX {
                    dist[v] = dist[u] + 1;
                    mx = mx.max(dist[v]);
                    q.push_back(v);
                }
            }
        }
        mx
    }

    #[test]
    fn graph_is_connected() {
        let devices = synth_devices(200);
        let (nodes, adj) = adjacency(&devices, b"c");
        let mut seen = HashSet::new();
        let mut stack = vec![0usize];
        seen.insert(0usize);
        while let Some(u) = stack.pop() {
            for &v in &adj[u] {
                if seen.insert(v) {
                    stack.push(v);
                }
            }
        }
        assert_eq!(seen.len(), nodes.len());
    }

    #[test]
    fn diameter_is_logarithmic() {
        // Diameter must stay O(log n) as the ring grows — the property Greptile's
        // P1 showed the old fixed-offset construction violated.
        for &n in &[64usize, 128, 256, 512] {
            let devices = synth_devices(n);
            let (nodes, adj) = adjacency(&devices, b"c");
            let diam = (0..nodes.len())
                .map(|s| eccentricity(&adj, s))
                .max()
                .unwrap();
            let log_bound = 2 * (usize::BITS - n.leading_zeros()) as usize; // ~2·⌈log₂ n⌉
            assert!(
                diam <= log_bound,
                "N={n}: diameter {diam} exceeds log-bound {log_bound}"
            );
        }
    }

    #[test]
    fn single_join_bounded_per_node_delta() {
        let base = synth_devices(200);
        let mut grown = base.clone();
        grown.insert(synth_key(987_654));
        let node = *base.iter().next().unwrap();
        let before = community_neighbors(&base, &node, b"c");
        let after = community_neighbors(&grown, &node, b"c");
        // The per-node neighbor-set *delta* is O(degree): both sets are
        // degree-bounded, so a membership change cannot cascade a node's own links
        // network-wide. (The recompute *work* is the O(N log N) ring sort;
        // network-wide rank-churn is higher by design — the spec's tradeoff.)
        let deg = 2 * ring_offsets(base.len()).len();
        let delta = before.symmetric_difference(&after).count();
        assert!(
            delta <= 2 * deg,
            "delta {delta} exceeds 2·degree {}",
            2 * deg
        );
    }

    #[test]
    fn per_community_decorrelation() {
        let devices = synth_devices(200);
        let node = *devices.iter().next().unwrap();
        let a = community_neighbors(&devices, &node, b"community-A");
        let b = community_neighbors(&devices, &node, b"community-B");
        assert!(a.intersection(&b).count() < a.len());
    }
}
