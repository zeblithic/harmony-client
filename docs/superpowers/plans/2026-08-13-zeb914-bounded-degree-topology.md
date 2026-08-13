# ZEB-914 R4 — Bounded-degree topology engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a pure, deterministic, property-tested `community_neighbors` engine that computes a bounded-degree neighbor set for one community's device ring — with **zero live call sites**.

**Architecture:** A single client-side module `src-tauri/src/community_topology.rs`. Devices are sorted into a canonical per-community ring by `H(salt ‖ device_key)`; neighbors are the devices at ring-ranks `self ± offset` for a geometric offset set (always including ±1, the protected lattice). Symmetric by construction → hard degree bound. No I/O, no async, no app state.

**Tech Stack:** Rust; `harmony_crypto::hash::blake3_hash`; `std::collections::BTreeSet`; inline `#[cfg(test)]` tests.

**Design doc:** `docs/superpowers/specs/2026-08-13-zeb914-bounded-degree-topology-design.md`.

> **Post-review addendum (PR #673).** The shipped implementation evolved from the
> TDD snippets below in two review-driven ways; where they differ, the code + the
> updated design doc are authoritative:
> 1. **Batched API** (CodeAnt/CodeRabbit): `ring_order` (sort once) and
>    `neighbors_on_ring` (select from a pre-sorted ring) are public; `community_neighbors`
>    wraps them so callers computing many nodes' sets don't re-sort per call.
> 2. **Powers-of-two offsets, no `degree_budget`** (Greptile P1): the offset set is
>    `{1,2,4,…,≤N/2}` (fixed ratio 2) for genuine O(log N) diameter; degree is
>    therefore `~2·log₂N`, a function of N, so the `degree_budget` parameter and
>    `TOPOLOGY_DEFAULT_DEGREE` const in the snippets below were removed. A fixed
>    offset *count* (as originally drafted) gives polynomial O(N^¼) diameter.

## Global Constraints

- **Unwired.** The module must have **no call sites** in this plan. It is registered in `lib.rs` so it compiles and tests, but nothing calls `community_neighbors`. Wiring is a follow-up ticket.
- **Pure.** No `async`, no I/O, no globals, no app types in the signature — takes `&BTreeSet<[u8;32]>` device keys and a `&[u8]` community salt.
- **Determinism.** Output depends only on the arguments; identical inputs → identical output; independent of iteration/insertion order.
- **Symmetry is load-bearing.** For any `a`, `b` in `devices`: `b ∈ community_neighbors(a,…)` ⟺ `a ∈ community_neighbors(b,…)`. All nodes use the same offset set (a function of `n` and `degree_budget` only), which is what guarantees this.
- **Constants:** `TOPOLOGY_DEFAULT_DEGREE = 10`, `FULL_MESH_THRESHOLD = 32`.
- **Branch:** all code commits on `zeblith/zeb-914-bounded-degree-topology` (this plan doc itself lands on main, doc-only). 
- **Gate:** `cargo clippy --all-targets` (inline test lints) + run this module's tests. Prefer `scripts/test-select` / a `community_topology` test filter to avoid the full ~97-binary relink.

---

### Task 1: Module scaffold, registration, and the canonical ring order

**Files:**
- Create: `src-tauri/src/community_topology.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod community_topology;` in the `community_*` block, alphabetical order)
- Test: inline `#[cfg(test)]` in `community_topology.rs`

**Interfaces:**
- Produces: `fn ring_position(community_salt: &[u8], device: &[u8;32]) -> u64` and `fn ring_order(devices: &BTreeSet<[u8;32]>, community_salt: &[u8]) -> Vec<[u8;32]>` (module-private); test helper `synth_devices(n) -> BTreeSet<[u8;32]>`.

- [ ] **Step 1: Register the module.** In `src-tauri/src/lib.rs`, add in alphabetical position among the `community_*` modules:

```rust
pub mod community_topology;
```

- [ ] **Step 2: Write the failing test** in `src-tauri/src/community_topology.rs`:

```rust
//! Bounded-degree community topology (ZEB-914, R4).
//!
//! Pure, deterministic neighbor selection for one community's device ring.
//! See docs/superpowers/specs/2026-08-13-zeb914-bounded-degree-topology-design.md.
//!
//! UNWIRED: this module has no live call sites. R4's hot-path wiring (kick-inflow
//! filter, device-key→node_id resolver bridge, router-mode gate) is a follow-up.

use std::collections::BTreeSet;

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic distinct synthetic device keys for property tests.
    fn synth_devices(n: usize) -> BTreeSet<[u8; 32]> {
        (0..n)
            .map(|i| harmony_crypto::hash::blake3_hash(&(i as u64).to_be_bytes()))
            .collect()
    }

    #[test]
    fn ring_order_is_deterministic_and_a_permutation() {
        let devices = synth_devices(50);
        let a = ring_order(&devices, b"community-A");
        let b = ring_order(&devices, b"community-A");
        assert_eq!(a, b, "ring order must be deterministic");
        assert_eq!(a.len(), devices.len(), "ring order must be a permutation");
        assert_eq!(a.iter().copied().collect::<BTreeSet<_>>(), devices);
    }

    #[test]
    fn ring_order_decorrelates_across_communities() {
        let devices = synth_devices(200);
        let a = ring_order(&devices, b"community-A");
        let b = ring_order(&devices, b"community-B");
        assert_ne!(a, b, "different salts must yield different ring orders");
    }
}
```

- [ ] **Step 3: Run to verify it fails.** `cargo test --lib community_topology` → FAIL (`ring_order` not found).

- [ ] **Step 4: Implement** above the test module:

```rust
/// Ring coordinate for a device in a given community. Uniform over u64.
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
```

- [ ] **Step 5: Run to verify it passes.** `cargo test --lib community_topology` → PASS.

- [ ] **Step 6: Commit.**

```bash
git add src-tauri/src/community_topology.rs src-tauri/src/lib.rs
git commit -m "ZEB-914: R4 topology engine — canonical per-community ring order"
```

---

### Task 2: Public API — self-exclusion, non-member, and full-mesh below threshold

**Files:** Modify `src-tauri/src/community_topology.rs`.

**Interfaces:**
- Produces: `pub fn community_neighbors(devices: &BTreeSet<[u8;32]>, self_device: &[u8;32], community_salt: &[u8], degree_budget: usize) -> BTreeSet<[u8;32]>` and the two `pub const`s. This task implements only the below-threshold and guard paths; Task 4 fills the above-threshold branch.

- [ ] **Step 1: Write the failing tests** (append to the `tests` module):

```rust
    #[test]
    fn full_mesh_below_threshold() {
        let devices = synth_devices(20); // < FULL_MESH_THRESHOLD
        for a in &devices {
            let nb = community_neighbors(&devices, a, b"c", TOPOLOGY_DEFAULT_DEGREE);
            assert_eq!(nb.len(), devices.len() - 1, "below threshold is full mesh");
            assert!(!nb.contains(a), "never a neighbor of self");
            assert!(nb.is_subset(&devices), "neighbors ⊆ devices");
        }
    }

    #[test]
    fn non_member_self_gets_no_neighbors() {
        let devices = synth_devices(50);
        let stranger = harmony_crypto::hash::blake3_hash(&999_999u64.to_be_bytes());
        assert!(!devices.contains(&stranger));
        assert!(community_neighbors(&devices, &stranger, b"c", 10).is_empty());
    }
```

- [ ] **Step 2: Run to verify it fails.** FAIL (`community_neighbors`, consts not found).

- [ ] **Step 3: Implement** (constants near the top; function below `ring_order`):

```rust
/// Target neighbor count above the full-mesh threshold.
pub const TOPOLOGY_DEFAULT_DEGREE: usize = 10;
/// Below this many active devices, a community stays full mesh.
pub const FULL_MESH_THRESHOLD: usize = 32;

/// Deterministic bounded-degree neighbor selection for one community's device ring.
///
/// - `devices`: all active (Joined, post-revocation) enrolled device keys, INCLUDING `self_device`.
/// - `self_device`: this node's enrolled device key; must be in `devices`.
/// - `community_salt`: community id bytes — decorrelates ring positions per community.
/// - `degree_budget`: target max neighbors above the full-mesh threshold.
///
/// Returns the subset of `devices` this node keeps persistent links to (never
/// includes `self_device`). Below `FULL_MESH_THRESHOLD` devices, returns all-but-self.
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
        return devices.iter().copied().filter(|d| d != self_device).collect();
    }
    // Above-threshold circulant path: filled in Task 4.
    let _ = community_salt;
    let _ = degree_budget;
    BTreeSet::new()
}
```

- [ ] **Step 4: Run to verify it passes.** PASS (both new tests use only the below-threshold / guard paths).

- [ ] **Step 5: Commit.**

```bash
git commit -am "ZEB-914: R4 engine — community_neighbors API, full-mesh threshold + guards"
```

---

### Task 3: Geometric offset set

**Files:** Modify `src-tauri/src/community_topology.rs`.

**Interfaces:**
- Produces: `fn ring_offsets(n: usize, degree_budget: usize) -> BTreeSet<usize>` (module-private). Consumed by Task 4.

- [ ] **Step 1: Write the failing tests:**

```rust
    #[test]
    fn offsets_include_lattice_and_respect_budget() {
        for &n in &[32usize, 50, 100, 200, 400] {
            let offs = ring_offsets(n, 10);
            assert!(offs.contains(&1), "protected ±1 lattice always present");
            assert!(offs.len() <= 10 / 2, "at most ⌊degree/2⌋ offsets");
            assert!(offs.iter().all(|&o| o >= 1 && o <= n / 2), "offsets in [1, n/2]");
        }
    }

    #[test]
    fn offsets_span_toward_half_ring() {
        // A finger reaching near n/2 is needed for O(log n) diameter.
        let offs = ring_offsets(400, 10);
        let max = *offs.iter().max().unwrap();
        assert!(max >= 400 / 4, "largest offset must reach a meaningful fraction of the ring");
    }
```

- [ ] **Step 2: Run to verify it fails.** FAIL (`ring_offsets` not found).

- [ ] **Step 3: Implement:**

```rust
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
```

- [ ] **Step 4: Run to verify it passes.** PASS.

- [ ] **Step 5: Commit.**

```bash
git commit -am "ZEB-914: R4 engine — geometric offset set (degree↔diameter knob)"
```

---

### Task 4: Circulant assembly + the symmetry and degree-bound invariants

**Files:** Modify `src-tauri/src/community_topology.rs` (replace the Task-2 placeholder above-threshold branch).

**Interfaces:**
- Consumes: `ring_order`, `ring_offsets`. Completes `community_neighbors`.

- [ ] **Step 1: Write the failing tests:**

```rust
    #[test]
    fn symmetry_holds_above_threshold() {
        let devices = synth_devices(200);
        let salt = b"community-A";
        for a in &devices {
            for b in community_neighbors(&devices, a, salt, 10) {
                assert!(
                    community_neighbors(&devices, &b, salt, 10).contains(a),
                    "edge not mirrored: {a:?} -> {b:?}"
                );
            }
        }
    }

    #[test]
    fn degree_bounded_above_threshold() {
        let devices = synth_devices(200);
        for a in &devices {
            let nb = community_neighbors(&devices, a, b"c", 10);
            assert!(nb.len() <= 10, "degree {} exceeds budget", nb.len());
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
```

- [ ] **Step 2: Run to verify it fails.** FAIL (above-threshold returns empty → symmetry/degree tests fail).

- [ ] **Step 3: Implement** — replace the Task-2 placeholder branch with:

```rust
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
```

(Remove the now-unused `let _ = community_salt; let _ = degree_budget;` lines.)

- [ ] **Step 4: Run to verify it passes.** PASS (symmetry, degree bound, determinism).

- [ ] **Step 5: Commit.**

```bash
git commit -am "ZEB-914: R4 engine — symmetric circulant neighbor assembly"
```

---

### Task 5: Graph invariants (connectivity, diameter, churn, decorrelation) + full gate

**Files:** Modify `src-tauri/src/community_topology.rs` (tests only).

- [ ] **Step 1: Write the failing/holding tests:**

```rust
    // Build adjacency once for graph-level assertions.
    fn adjacency(devices: &BTreeSet<[u8; 32]>, salt: &[u8], deg: usize)
        -> (Vec<[u8; 32]>, Vec<Vec<usize>>)
    {
        let nodes: Vec<[u8; 32]> = devices.iter().copied().collect();
        let idx: std::collections::HashMap<[u8; 32], usize> =
            nodes.iter().enumerate().map(|(i, k)| (*k, i)).collect();
        let adj = nodes
            .iter()
            .map(|a| community_neighbors(devices, a, salt, deg).iter().map(|b| idx[b]).collect())
            .collect();
        (nodes, adj)
    }

    #[test]
    fn graph_is_connected() {
        let devices = synth_devices(200);
        let (nodes, adj) = adjacency(&devices, b"c", 10);
        let mut seen = std::collections::HashSet::new();
        let mut stack = vec![0usize];
        seen.insert(0usize);
        while let Some(u) = stack.pop() {
            for &v in &adj[u] {
                if seen.insert(v) {
                    stack.push(v);
                }
            }
        }
        assert_eq!(seen.len(), nodes.len(), "graph must be connected");
    }

    #[test]
    fn diameter_is_logarithmic() {
        let devices = synth_devices(200);
        let (nodes, adj) = adjacency(&devices, b"c", 10);
        // Eccentricity from a sample of sources; assert all ≤ a generous log-bound.
        let ecc = |src: usize| -> usize {
            let mut dist = vec![usize::MAX; nodes.len()];
            let mut q = std::collections::VecDeque::new();
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
        for src in [0, 50, 100, 199] {
            let d = ecc(src);
            assert!(d <= 20, "diameter {d} from {src} exceeds log-bound (N=200, deg=10)");
        }
    }

    #[test]
    fn single_join_bounded_per_node_delta() {
        let base = synth_devices(200);
        let newcomer = harmony_crypto::hash::blake3_hash(&987_654u64.to_be_bytes());
        let mut grown = base.clone();
        grown.insert(newcomer);
        let node = *base.iter().next().unwrap();
        let before = community_neighbors(&base, &node, b"c", 10);
        let after = community_neighbors(&grown, &node, b"c", 10);
        let delta = before.symmetric_difference(&after).count();
        // A single roster change perturbs one node by at most ~its degree.
        assert!(delta <= 10, "per-node churn delta {delta} too large");
    }

    #[test]
    fn per_community_decorrelation() {
        let devices = synth_devices(200);
        let node = *devices.iter().next().unwrap();
        let a = community_neighbors(&devices, &node, b"community-A", 10);
        let b = community_neighbors(&devices, &node, b"community-B", 10);
        assert!(a.intersection(&b).count() < a.len(), "neighbor sets identical across communities");
    }
```

- [ ] **Step 2: Run to verify.** All PASS (the engine is complete after Task 4; these assert its emergent graph properties).

- [ ] **Step 3: Full gate.** Run clippy over test code and the module tests:

```bash
cargo clippy --all-targets -- -D warnings   # or the crate-scoped equivalent used in CI
cargo test --lib community_topology
```

Expected: clean clippy, all `community_topology` tests green. Confirm the working tree is clean afterward.

- [ ] **Step 4: Commit.**

```bash
git commit -am "ZEB-914: R4 engine — graph invariant suite (connectivity, diameter, churn, decorrelation)"
```

---

## Self-Review

- **Spec coverage:** ring-over-devices (input is the device-key set), identity-derived per-community position (Task 1 `ring_position`), identity-fixed (pure function of the roster set — no presence input), symmetric circulant with protected lattice (Task 4, offset 1 always present), degree↔diameter knob (Task 3 `ring_offsets`), full-mesh threshold (Task 2), all 9 spec properties covered by tests (self-exclusion, membership, symmetry, degree bound, connectivity, diameter, determinism, decorrelation, churn-delta). Unwired (Global Constraints; no call sites added). ✓
- **Placeholder scan:** every code step has real code; test bounds are concrete. ✓
- **Type consistency:** `community_neighbors` signature identical across Tasks 2 and 4; `ring_offsets` returns `BTreeSet<usize>` (Task 3) and is iterated in Task 4; `[u8;32]` throughout. ✓
- **Deferred correctly:** the resolver bridge, kick-inflow filter, router-mode gate, cross-community union, and harness validation are the follow-up ticket, not this plan. ✓
