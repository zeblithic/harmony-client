//! Bounded-degree community topology (ZEB-914, R4).
//!
//! Pure, deterministic neighbor selection for one community's device ring.
//! See `docs/superpowers/specs/2026-08-13-zeb914-bounded-degree-topology-design.md`.
//!
//! The ring places every active device at `H(community_salt ‖ device_key)`;
//! neighbors are the devices at ring-ranks `self ± offset` for a geometric
//! offset set that always includes ±1 (the protected lattice, which makes the
//! graph connected by construction) plus larger fingers (which give O(log N)
//! diameter). All nodes derive the same offset set from `(N, degree)`, so every
//! edge is symmetric — a hard degree bound with no pruning or capacity heuristics.
//!
//! UNWIRED: this module has no live call sites. R4's hot-path wiring (kick-inflow
//! filter, device-key→node_id resolver bridge, router-mode gate) is a follow-up
//! ticket under ZEB-909.

use std::collections::BTreeSet;

/// Target neighbor count above the full-mesh threshold.
pub const TOPOLOGY_DEFAULT_DEGREE: usize = 10;
/// Below this many active devices, a community stays full mesh.
pub const FULL_MESH_THRESHOLD: usize = 32;

/// Ring coordinate for a device in a given community. Uniform over `u64`.
fn ring_position(community_salt: &[u8], device: &[u8; 32]) -> u64 {
    let mut buf = Vec::with_capacity(community_salt.len() + 32);
    buf.extend_from_slice(community_salt);
    buf.extend_from_slice(device);
    let h = harmony_crypto::hash::blake3_hash(&buf);
    u64::from_be_bytes(h[..8].try_into().expect("blake3 output is 32 bytes"))
}

/// Canonical cyclic order of devices by ring position; ties broken by key bytes
/// so every node computes the identical ring.
fn ring_order(devices: &BTreeSet<[u8; 32]>, community_salt: &[u8]) -> Vec<[u8; 32]> {
    let mut ordered: Vec<[u8; 32]> = devices.iter().copied().collect();
    ordered.sort_by_key(|d| (ring_position(community_salt, d), *d));
    ordered
}

/// Geometric offset set for the circulant ring: at most `⌊degree/2⌋` offsets,
/// always including 1 (the protected lattice), spanning `[1, n/2]` so greedy
/// routing composes them to reach any rank in O(log n) hops. Offsets > n/2 are
/// redundant on an n-cycle, so the range is capped at n/2.
fn ring_offsets(n: usize, degree_budget: usize) -> BTreeSet<usize> {
    let max_off = (n / 2).max(1);
    let count = (degree_budget / 2).max(1);
    let mut offs = BTreeSet::new();
    offs.insert(1);
    if count > 1 && max_off > 1 {
        for i in 1..count {
            let frac = i as f64 / (count - 1) as f64;
            let off = (max_off as f64).powf(frac).round() as usize;
            offs.insert(off.clamp(1, max_off));
        }
    }
    offs
}

/// Deterministic bounded-degree neighbor selection for one community's device ring.
///
/// - `devices`: all active (Joined, post-revocation) enrolled device keys,
///   INCLUDING `self_device`.
/// - `self_device`: this node's enrolled device key; must be in `devices`.
/// - `community_salt`: community id bytes — decorrelates ring positions per community.
/// - `degree_budget`: target max neighbors above the full-mesh threshold.
///
/// Returns the subset of `devices` this node keeps persistent links to (never
/// includes `self_device`). Below [`FULL_MESH_THRESHOLD`] devices, returns
/// all-but-self. Deterministic and symmetric: for any `a`, `b` in `devices`,
/// `b ∈ community_neighbors(a, …)` ⟺ `a ∈ community_neighbors(b, …)`.
pub fn community_neighbors(
    devices: &BTreeSet<[u8; 32]>,
    self_device: &[u8; 32],
    community_salt: &[u8],
    degree_budget: usize,
) -> BTreeSet<[u8; 32]> {
    if !devices.contains(self_device) {
        return BTreeSet::new();
    }
    let n = devices.len();
    if n < FULL_MESH_THRESHOLD {
        return devices
            .iter()
            .copied()
            .filter(|d| d != self_device)
            .collect();
    }

    let ordered = ring_order(devices, community_salt);
    let self_rank = ordered
        .iter()
        .position(|d| d == self_device)
        .expect("self_device is in devices");
    let offsets = ring_offsets(n, degree_budget);

    let mut neighbors = BTreeSet::new();
    for o in offsets {
        // +o and -o (mod n). All nodes share this offset set, so the edge is
        // symmetric: the node at rank+o computes this node at its own rank-o.
        let fwd = ordered[(self_rank + o) % n];
        let bwd = ordered[(self_rank + n - o) % n];
        if fwd != *self_device {
            neighbors.insert(fwd);
        }
        if bwd != *self_device {
            neighbors.insert(bwd);
        }
    }
    neighbors
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
            let nb = community_neighbors(&devices, a, b"c", TOPOLOGY_DEFAULT_DEGREE);
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
        assert!(community_neighbors(&devices, &stranger, b"c", 10).is_empty());
    }

    // ---- offsets ----

    #[test]
    fn offsets_include_lattice_and_respect_budget() {
        for &n in &[32usize, 50, 100, 200, 400] {
            let offs = ring_offsets(n, 10);
            assert!(offs.contains(&1));
            assert!(offs.len() <= 10 / 2);
            assert!(offs.iter().all(|&o| o >= 1 && o <= n / 2));
        }
    }

    #[test]
    fn offsets_span_toward_half_ring() {
        let offs = ring_offsets(400, 10);
        assert!(*offs.iter().max().unwrap() >= 400 / 4);
    }

    // ---- circulant assembly ----

    #[test]
    fn symmetry_holds_above_threshold() {
        let devices = synth_devices(200);
        let salt = b"community-A";
        for a in &devices {
            for b in community_neighbors(&devices, a, salt, 10) {
                assert!(
                    community_neighbors(&devices, &b, salt, 10).contains(a),
                    "edge not mirrored"
                );
            }
        }
    }

    #[test]
    fn degree_bounded_above_threshold() {
        let devices = synth_devices(200);
        for a in &devices {
            let nb = community_neighbors(&devices, a, b"c", 10);
            assert!(nb.len() <= 10);
            assert!(!nb.contains(a));
            assert!(nb.is_subset(&devices));
        }
    }

    #[test]
    fn deterministic_above_threshold() {
        let devices = synth_devices(200);
        let node = *devices.iter().next().unwrap();
        assert_eq!(
            community_neighbors(&devices, &node, b"c", 10),
            community_neighbors(&devices, &node, b"c", 10)
        );
    }

    // ---- graph invariants ----

    fn adjacency(
        devices: &BTreeSet<[u8; 32]>,
        salt: &[u8],
        deg: usize,
    ) -> (Vec<[u8; 32]>, Vec<Vec<usize>>) {
        let nodes: Vec<[u8; 32]> = devices.iter().copied().collect();
        let idx: HashMap<[u8; 32], usize> =
            nodes.iter().enumerate().map(|(i, k)| (*k, i)).collect();
        let adj = nodes
            .iter()
            .map(|a| {
                community_neighbors(devices, a, salt, deg)
                    .iter()
                    .map(|b| idx[b])
                    .collect()
            })
            .collect();
        (nodes, adj)
    }

    #[test]
    fn graph_is_connected() {
        let devices = synth_devices(200);
        let (nodes, adj) = adjacency(&devices, b"c", 10);
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
        let devices = synth_devices(200);
        let (nodes, adj) = adjacency(&devices, b"c", 10);
        let ecc = |src: usize| -> usize {
            let mut dist = vec![usize::MAX; nodes.len()];
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
        };
        for src in [0usize, 50, 100, 199] {
            let d = ecc(src);
            assert!(d <= 20, "diameter {d} exceeds log-bound (N=200, deg=10)");
        }
    }

    #[test]
    fn single_join_bounded_per_node_delta() {
        let base = synth_devices(200);
        let mut grown = base.clone();
        grown.insert(synth_key(987_654));
        let node = *base.iter().next().unwrap();
        let before = community_neighbors(&base, &node, b"c", 10);
        let after = community_neighbors(&grown, &node, b"c", 10);
        // Per-node recompute is O(degree), not O(N): both sets are degree-bounded,
        // so their symmetric difference cannot cascade network-wide. (Network-wide
        // rank-churn is higher by design — the spec's acknowledged tradeoff.)
        let delta = before.symmetric_difference(&after).count();
        assert!(delta <= 2 * TOPOLOGY_DEFAULT_DEGREE);
    }

    #[test]
    fn per_community_decorrelation() {
        let devices = synth_devices(200);
        let node = *devices.iter().next().unwrap();
        let a = community_neighbors(&devices, &node, b"community-A", 10);
        let b = community_neighbors(&devices, &node, b"community-B", 10);
        assert!(a.intersection(&b).count() < a.len());
    }
}
