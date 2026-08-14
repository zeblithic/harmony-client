# ZEB-930 O1 — Topology churn + reconvergence sensitivity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Measure neighbor-set churn under membership change exactly (from the real `community_topology`) for R4 vs the R3 ring vs pre-R4 full-mesh, model reconvergence time as a sensitivity band, and write up the claim-B verdict + O2 recommendation.

**Architecture:** Two layers. Layer 1 (exact) drives the real `community_neighbors`/`ring_order`/`neighbors_on_ring` through deterministic join/leave events and computes per-event churn — this is 100% our own code, zero fidelity gap. Layer 2 (modeled) translates churn → time via `T ≈ max(D·L, ⌈maxAdds/C⌉·d)`. Delivered as an in-tree example (report generator), an inline regression-guard test, and a findings doc.

**Tech Stack:** Rust (workspace `harmony-app`), `cargo nextest` for tests, `cargo run --example` for the report. No new dependencies. No new production public API.

## Global Constraints

- **Run all cargo from `src-tauri/`** (CLAUDE.md: `.cargo/config.toml` is cwd-discovered).
- **`cargo clippy --all-targets ... -D warnings` lints the example** — the example MUST be clippy-clean (inline format args for bare idents; alias complex types).
- **MSRV gate builds `--all-targets`** — the example must compile under the declared MSRV (plain Rust, no bleeding-edge syntax).
- **Deterministic only** — no wall-clock, no RNG; the whole sweep must replay bit-for-bit.
- **In-tree, no production API growth** — churn/harness helpers live in the example and the test mod, not in the production surface of `community_topology`. Modest duplication of the ~25-line churn arithmetic between the example and the guard test is deliberate: the guard stays independent of the report tool.
- **No key-distance implementation** — O2's fix is analyzed in prose only.
- **Lib facts (verified):** crate `harmony_app`; `pub mod community_topology` (lib.rs:160); public API `ring_order(&BTreeSet<[u8;32]>, &[u8]) -> Vec<[u8;32]>`, `neighbors_on_ring(&[[u8;32]], &[u8;32]) -> BTreeSet<[u8;32]>`, `community_neighbors(&BTreeSet<[u8;32]>, &[u8;32], &[u8]) -> BTreeSet<[u8;32]>`, `FULL_MESH_THRESHOLD: usize = 32`. Existing test-mod helpers: `synth_devices(n) -> BTreeSet<[u8;32]>`, `synth_key(tag) -> [u8;32]`.
- **Cost-model constant:** `C = max_concurrent_dials = 4` (`reconnect_supervisor.rs:198`).
- **Commit trailers** on every commit:
  ```
  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D
  ```

---

### Task 1: Churn helper + regression guard (inline in `community_topology.rs`)

Builds the churn arithmetic (new, testable code) and lands two regression-guard tests that lock the finding: the ring is O(1) per join; the R4 rank-based circulant is a re-dial storm at N=200. TDD applies to the churn helper — a stub returns 0 and the assertions fail until the helper is correct.

**Files:**
- Modify: `src-tauri/src/community_topology.rs` — append to the existing `#[cfg(test)] mod tests` (helpers + 2 tests).

**Interfaces:**
- Consumes: `ring_order`, `neighbors_on_ring`, `synth_devices`, `synth_key` (all in scope via `use super::*`), `BTreeSet` (module import).
- Produces (test-mod-local): `type Adj`, `adj_r4`, `adj_ring`, `edges`, `edge_churn`, `nodes_affected` — reused by both tests. (The example re-derives its own copies; not shared across the crate boundary.)

- [ ] **Step 1: Create the working branch**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && git checkout main && git pull --ff-only origin main && git checkout -b zeblith/zeb-930-o1-topology-churn
```

- [ ] **Step 2: Add the churn helpers + failing tests**

Append inside `mod tests` in `src-tauri/src/community_topology.rs` (after the existing tests). Add `use std::collections::BTreeMap;` to the test module's imports first (it currently imports `HashMap, HashSet, VecDeque`).

```rust
    // ---- ZEB-930 O1: membership-change churn (regression guard) ----

    type Adj = BTreeMap<[u8; 32], BTreeSet<[u8; 32]>>;

    /// R4 production adjacency: every node's bounded neighbor set.
    fn adj_r4(devices: &BTreeSet<[u8; 32]>, salt: &[u8]) -> Adj {
        let ring = ring_order(devices, salt);
        ring.iter().map(|d| (*d, neighbors_on_ring(&ring, d))).collect()
    }

    /// R3 ring baseline adjacency: neighbors are the two ring-adjacent devices.
    fn adj_ring(devices: &BTreeSet<[u8; 32]>, salt: &[u8]) -> Adj {
        let ring = ring_order(devices, salt);
        let n = ring.len();
        ring.iter()
            .enumerate()
            .map(|(r, d)| {
                let mut s = BTreeSet::new();
                if n >= 2 {
                    s.insert(ring[(r + 1) % n]);
                    s.insert(ring[(r + n - 1) % n]);
                }
                s.remove(d);
                (*d, s)
            })
            .collect()
    }

    /// Undirected edge set (each edge once, canonical (min, max)).
    fn edges(adj: &Adj) -> BTreeSet<([u8; 32], [u8; 32])> {
        let mut e = BTreeSet::new();
        for (u, nbrs) in adj {
            for v in nbrs {
                e.insert(if u < v { (*u, *v) } else { (*v, *u) });
            }
        }
        e
    }

    /// Community-wide edge churn: edges added + torn down (symmetric difference).
    fn edge_churn(before: &Adj, after: &Adj) -> usize {
        edges(after).symmetric_difference(&edges(before)).count()
    }

    /// Existing nodes (present before AND after) whose neighbor set changed.
    fn nodes_affected(before: &Adj, after: &Adj) -> usize {
        before
            .keys()
            .filter(|k| after.contains_key(*k))
            .filter(|k| before.get(*k) != after.get(*k))
            .count()
    }

    #[test]
    fn zeb930_ring_baseline_join_churn_is_o1_at_scale() {
        // A join into the degree-2 ring splits exactly one edge: O(1) churn,
        // independent of N. Guards the claim-B baseline.
        let salt = b"zeb930";
        for &n in &[50usize, 200] {
            let before = synth_devices(n);
            let mut after = before.clone();
            after.insert(synth_key(1_000_000));
            let churn = edge_churn(&adj_ring(&before, salt), &adj_ring(&after, salt));
            assert!(churn <= 8, "ring join churn at N={n} was {churn}, expected O(1) (<=8)");
        }
    }

    #[test]
    fn zeb930_r4_join_is_a_redial_storm_at_n200() {
        // ZEB-930 O1 headline: the rank-based circulant turns one join into a
        // community-wide re-dial storm — roughly half the nodes change neighbors
        // and >100 edges churn — because the hash-ranked insert shifts every rank
        // past the insertion point. Locks the storm as a KNOWN property: a future
        // key-distance fix (O2) must visibly move these floors.
        let salt = b"zeb930";
        let n = 200usize;
        let before = synth_devices(n);
        let mut after = before.clone();
        after.insert(synth_key(1_000_000));
        let b = adj_r4(&before, salt);
        let a = adj_r4(&after, salt);
        let churn = edge_churn(&b, &a);
        let affected = nodes_affected(&b, &a);
        assert!(churn >= 100, "R4 join edge-churn at N={n} was {churn}, expected a storm (>=100)");
        assert!(affected >= 80, "R4 join nodes-affected at N={n} was {affected}/{n}, expected >=80");
    }
```

- [ ] **Step 3: Run the two tests — confirm they pass (characterization) and print the real numbers**

```bash
cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(zeb930)'
```
Expected: both PASS. If `zeb930_r4_join_is_a_redial_storm_at_n200` fails because the measured value is *below* a floor, that is a real signal — print the actual `churn`/`affected` (temporarily add `eprintln!`) and confirm they are in the ~200-edge / ~120-node range the throwaway predicted; only lower a floor if the true value is genuinely lower, and note why.

- [ ] **Step 4: Gate — clippy + fmt on the changed file**

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo fmt --all -- --check
```
Expected: clean. Fix any lint (e.g. `uninlined_format_args`) inline.

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && git add src-tauri/src/community_topology.rs && git commit -F - <<'EOF'
ZEB-930 O1: churn regression guard — ring O(1), R4 storm at N=200

Adds a test-mod churn helper (edge-set symmetric difference + nodes-affected)
and two guards: the R3 ring pays O(1) edge churn per join at N=50/200, while
the rank-based R4 circulant churns >=100 edges and re-dials >=80/200 nodes on
a single join. Locks the storm as a known property so an O2 key-distance fix
visibly moves these floors.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D
EOF
```

---

### Task 2: Example report generator (`examples/topology_churn.rs`)

The full harness: three topology adjacencies, churn metrics, BFS diameter, the Layer-2 cost band, and table output. A self-check assertion (R4 degree == 14 at N=200, matching Part-1 T1) makes running it validate the harness.

**Files:**
- Create: `src-tauri/examples/topology_churn.rs`

**Interfaces:**
- Consumes: `harmony_app::community_topology::{ring_order, neighbors_on_ring, FULL_MESH_THRESHOLD}`.
- Produces: a stdout report (`cargo run --example topology_churn`). No importable API.

- [ ] **Step 1: Write the example**

Create `src-tauri/examples/topology_churn.rs`:

```rust
//! ZEB-930 O1 — topology churn + reconvergence-sensitivity report.
//!
//! Neighbor-set churn under membership change for the R4 circulant vs the R3
//! ring vs pre-R4 full-mesh, plus a diameter + dial-concurrency cost model that
//! bounds reconvergence time. Deterministic (no wall-clock, no RNG). Run:
//!
//!     cd src-tauri && cargo run --release --example topology_churn
//!
//! Regenerate the findings-doc tables from this output.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use harmony_app::community_topology::{neighbors_on_ring, ring_order, FULL_MESH_THRESHOLD};

type Key = [u8; 32];
type Adj = BTreeMap<Key, BTreeSet<Key>>;

const SALT: &[u8] = b"zeb930-o1";

/// Distinct device key `i`; `ring_order` hashes it, so ring positions stay uniform.
fn dev(i: u64) -> Key {
    let mut k = [0u8; 32];
    k[..8].copy_from_slice(&i.to_be_bytes());
    k
}

fn base(n: usize) -> BTreeSet<Key> {
    (0..n as u64).map(dev).collect()
}

fn adj_r4(devices: &BTreeSet<Key>) -> Adj {
    let ring = ring_order(devices, SALT);
    ring.iter().map(|d| (*d, neighbors_on_ring(&ring, d))).collect()
}

fn adj_ring(devices: &BTreeSet<Key>) -> Adj {
    let ring = ring_order(devices, SALT);
    let n = ring.len();
    ring.iter()
        .enumerate()
        .map(|(r, d)| {
            let mut s = BTreeSet::new();
            if n >= 2 {
                s.insert(ring[(r + 1) % n]);
                s.insert(ring[(r + n - 1) % n]);
            }
            s.remove(d);
            (*d, s)
        })
        .collect()
}

fn adj_full(devices: &BTreeSet<Key>) -> Adj {
    devices
        .iter()
        .map(|d| (*d, devices.iter().copied().filter(|x| x != d).collect()))
        .collect()
}

fn edges(adj: &Adj) -> BTreeSet<(Key, Key)> {
    let mut e = BTreeSet::new();
    for (u, nbrs) in adj {
        for v in nbrs {
            e.insert(if u < v { (*u, *v) } else { (*v, *u) });
        }
    }
    e
}

struct Churn {
    edges: f64,
    affected: f64,
    max_adds: f64,
}

fn churn(before: &Adj, after: &Adj) -> Churn {
    let ec = edges(after).symmetric_difference(&edges(before)).count();
    let mut affected = 0usize;
    let mut max_adds = 0usize;
    for (k, b) in before {
        if let Some(a) = after.get(k) {
            if a != b {
                affected += 1;
            }
            max_adds = max_adds.max(a.difference(b).count());
        }
    }
    Churn { edges: ec as f64, affected: affected as f64, max_adds: max_adds as f64 }
}

/// Mean per-join churn over a deterministic batch of `joins` distinct new devices.
fn avg_join(n: usize, mk: fn(&BTreeSet<Key>) -> Adj, joins: u64) -> Churn {
    let before = base(n);
    let b = mk(&before);
    let (mut se, mut sa, mut sm) = (0f64, 0f64, 0f64);
    for j in 0..joins {
        let mut after = before.clone();
        after.insert(dev(1_000_000 + j));
        let c = churn(&b, &mk(&after));
        se += c.edges;
        sa += c.affected;
        sm += c.max_adds;
    }
    let k = joins as f64;
    Churn { edges: se / k, affected: sa / k, max_adds: sm / k }
}

/// Unweighted graph diameter (max eccentricity) via BFS from every node.
fn diameter(adj: &Adj) -> usize {
    let mut best = 0usize;
    for src in adj.keys() {
        let mut dist: BTreeMap<Key, usize> = BTreeMap::new();
        dist.insert(*src, 0);
        let mut q = VecDeque::from([*src]);
        while let Some(u) = q.pop_front() {
            let du = dist[&u];
            best = best.max(du);
            for v in &adj[&u] {
                if !dist.contains_key(v) {
                    dist.insert(*v, du + 1);
                    q.push_back(*v);
                }
            }
        }
    }
    best
}

fn main() {
    // Self-check: R4 degree at N=200 is 14 (Part-1 T1). Validates the harness.
    let ring200 = ring_order(&base(200), SALT);
    assert_eq!(
        neighbors_on_ring(&ring200, &ring200[0]).len(),
        14,
        "R4 degree at N=200 must be 14 (Part-1 T1)"
    );

    let sizes = [32usize, 50, 64, 100, 128, 200];
    let joins = 64u64;

    println!("# ZEB-930 O1 — churn per join (mean over {joins} joiners)");
    println!("# FULL_MESH_THRESHOLD = {FULL_MESH_THRESHOLD}");
    println!("# columns: edges = community edge churn; aff = existing nodes re-dialing; maxA = worst node's new dials\n");
    println!(
        "{:>5} | {:>8} {:>6} {:>6} | {:>8} {:>6} {:>6} | {:>8} {:>6} {:>6}",
        "N", "R4 edg", "aff", "maxA", "ring edg", "aff", "maxA", "mesh edg", "aff", "maxA"
    );
    for &n in &sizes {
        let r = avg_join(n, adj_r4, joins);
        let g = avg_join(n, adj_ring, joins);
        let f = avg_join(n, adj_full, joins);
        println!(
            "{:>5} | {:>8.0} {:>6.0} {:>6.0} | {:>8.0} {:>6.0} {:>6.0} | {:>8.0} {:>6.0} {:>6.0}",
            n, r.edges, r.affected, r.max_adds, g.edges, g.affected, g.max_adds, f.edges, f.affected, f.max_adds
        );
    }

    println!("\n# Layer 2 — reconvergence-time band  T = max(D*L, ceil(maxAdds/C)*d),  C=4 dials");
    let hop_ms = [10.0f64, 40.0, 80.0];
    let link_ms = [300.0f64, 800.0, 1500.0];
    for &n in &[50usize, 200] {
        let d_r4 = diameter(&adj_r4(&base(n)));
        let d_rg = diameter(&adj_ring(&base(n)));
        let a_r4 = avg_join(n, adj_r4, joins).max_adds;
        let a_rg = avg_join(n, adj_ring, joins).max_adds;
        println!(
            "\nN={n}: diameter R4={d_r4} ring={d_rg}; maxAdds R4~{a_r4:.0} ring~{a_rg:.0}"
        );
        for &l in &hop_ms {
            for &d in &link_ms {
                let t = |dia: usize, adds: f64| {
                    let flood = dia as f64 * l;
                    let storm = (adds / 4.0).ceil() * d;
                    flood.max(storm)
                };
                println!(
                    "  L={l:>4.0}ms d={d:>6.0}ms   R4~{:>8.0}ms   ring~{:>8.0}ms",
                    t(d_r4, a_r4),
                    t(d_rg, a_rg)
                );
            }
        }
    }
}
```

- [ ] **Step 2: Run it — confirm the self-check passes and the numbers are sane**

```bash
cd src-tauri && cargo run --locked --release --example topology_churn
```
Expected: the `assert_eq!(… 14 …)` passes (R4 degree 14 at N=200); R4 edge churn climbs with N (≈200-300 at N=200), ring stays ≈3-4 flat, full-mesh edges ≈ N with maxA=1; below N=32 R4 == full-mesh (threshold). Capture the full stdout for Task 3.

- [ ] **Step 3: Gate — clippy on the example (it is linted under `--all-targets`)**

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo fmt --all -- --check
```
Expected: clean. Fix any `uninlined_format_args`/`type_complexity`/`needless_*` inline.

- [ ] **Step 4: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && git add src-tauri/examples/topology_churn.rs && git commit -F - <<'EOF'
ZEB-930 O1: topology_churn example — churn tables + reconvergence band

cargo run --example topology_churn prints per-join churn for R4 vs R3 ring vs
pre-R4 full-mesh across N=32..200, plus a diameter + dial-concurrency cost band
(T = max(D*L, ceil(maxAdds/4)*d)). Self-checks R4 degree=14 at N=200. In-tree
and CI-compiled so it cannot rot; run manually to regenerate the findings.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D
EOF
```

---

### Task 3: Findings doc + ticket update

Write up the results from Task 2's run into the honest-findings format, state the claim-B verdict as a band + direction, and give the O2 recommendation (key-distance, analytical).

**Files:**
- Create: `docs/research/2026-08-13-zeb930-o1-topology-churn-reconvergence.md`

- [ ] **Step 1: Write the findings doc**

Create `docs/research/2026-08-13-zeb930-o1-topology-churn-reconvergence.md` with this structure. Populate the two data blocks by pasting the actual `cargo run --example topology_churn` output from Task 2 Step 2 (do not hand-transcribe — paste the tool output verbatim into fenced blocks).

```markdown
# ZEB-930 O1 — Topology churn + reconvergence (findings)

**Ticket:** ZEB-930 (O1). Spec: `docs/superpowers/specs/2026-08-13-zeb930-o1-topology-churn-reconvergence-design.md`. Follows ZEB-929 Part 1 (PR #675).
**Harness:** `src-tauri/examples/topology_churn.rs` (`cargo run --example topology_churn`), deterministic. Guard: `community_topology.rs` tests `zeb930_*`.

## TL;DR (three findings, decreasing certainty)

1. **Membership-change churn — EXACT, our code.** A single join re-dials [N] of 200 nodes under R4 (rank-based circulant) vs ~2 under the R3 ring — because the hash-ranked insert shifts every rank past the insertion point and remaps each node's offset targets. R4 buys its bounded *steady-state* degree with a *membership-change* re-dial storm the ring — and the pre-R4 full-mesh — never pay.
2. **Reconvergence time — MODELED band.** Under `T = max(D·L, ⌈maxAdds/C⌉·d)`, R4's short diameter (D≈[..]) beats the ring's (D≈100 at N=200) on the flood-inform term, but its re-dial storm (maxAdds≈[..], C=4) dominates for realistic link bring-up d. [State the crossover.]
3. **Claim B verdict.** [Confirmed-direction / bounded / inconclusive], with the residual (true L, d for our stack) named as the fleet's job.

## Method

[Topologies R4/ring/full-mesh; deterministic keys via dev(i), ring_order hashes to uniform positions; join batch of 64; sizes 32..200; churn = edge-set symmetric difference + nodes-affected + per-node max adds; diameter by BFS; Layer-2 constants L, d swept, C=4 from reconnect_supervisor.rs:198.]

## Data — churn per join

​```
[paste the churn table from the example run]
​```

## Data — reconvergence band

​```
[paste the Layer-2 band from the example run]
​```

## Analysis

[Why R4 churns O(N·log N)-ish while ring is O(1) and full-mesh is O(N) edges but O(1) per node with no tear-downs; the R4==full-mesh crossover below N=32; which Layer-2 term dominates at N=50 vs 200 and over which (L,d).]

## O2 recommendation (analytical — no new production code)

The storm is a property of the *rank-based* construction: ranks shift under insertion. A **key-distance** construction — offsets taken in hash-space, so a node's ~2^k-distance neighbors are chosen by key rather than by rank — localizes a join's effect to the O(log N) nodes near it in hash-space, dropping churn toward the ring's regime while preserving the small-world diameter. Recommend O2/ZEB-914 prototype and measure this variant through the same harness. O1 does not implement it.

## Threats to validity

1. Layer 2 is a model; absolute time needs the fleet — reported as a band, not a point.
2. Churn is a topological upper bound (assumes all affected members online + re-dialing).
3. Re-dial assumption matches the ZEB-928 wiring (dropped neighbors park Dormant, new neighbors dial).
4. Churn magnitude is permutation-independent (rank-shift mechanics); the throwaway pre-check and the real blake3 harness agree in magnitude.

## Go / no-go for R4

[Keep R4 as shipped for now / revisit under O2. Tie to the churn + band: R4's degree bound and O(N log N) flood improvement over full-mesh stand (Part 1); the membership-change storm is the new cost, and the key-distance fix is the recommended O2 direction if the storm proves operationally material.]
```

- [ ] **Step 2: Verify the doc's numbers match the harness output**

Re-run `cargo run --release --example topology_churn` and diff its output against the pasted blocks — they must be byte-identical (deterministic harness). Fill every `[..]` bracket with the real number; no bracket placeholders may remain.

- [ ] **Step 3: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && git add docs/research/2026-08-13-zeb930-o1-topology-churn-reconvergence.md && git commit -F - <<'EOF'
ZEB-930 O1: findings — R4 membership-change re-dial storm vs ring O(1)

Churn measured exactly on the real community_topology: one join re-dials ~half
the community under the rank-based R4 circulant vs ~2 under the ring. Layer-2
band shows the re-dial storm dominates reconvergence for realistic link setup.
Claim-B verdict + key-distance O2 recommendation (analytical).

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D
EOF
```

---

### Task 4: Final gate + PR

- [ ] **Step 1: Full workspace gate (CI parity)**

```bash
cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Expected: fmt clean, clippy clean, all tests pass (including the two `zeb930_*` guards). Confirm `git status` clean (working-tree gate — the gate must run the committed tree).

- [ ] **Step 2: Finish the branch**

Announce and use **superpowers:finishing-a-development-branch** to push and open the PR (base `main`, branch `zeblith/zeb-930-o1-topology-churn`). PR body: summarize the three findings, link the spec + findings doc, note "no production code changes — measurement + guard + doc; R4 topology unchanged." Then fire exactly one `@coderabbitai review`, and converge per the autonomous PR loop (CodeAnt auto-reviews; Greptile excludes the author; do not re-trigger any bot).

---

## Self-Review

**1. Spec coverage:**
- Layer 1 churn (R4/ring/full-mesh, sizes, metrics, determinism) → Task 2 example + Task 1 guard. ✓
- Layer 2 cost model (max(flood, storm), C=4, sweep, band) → Task 2 `main` band. ✓
- Deliverables: example + regression test + findings doc → Tasks 2, 1, 3. ✓
- O2 key-distance analytical → Task 3 doc section. ✓
- Threats to validity → Task 3 doc section. ✓
- Success criteria (example prints tables; guard pins invariants; doc states verdict + O2) → Tasks 1-3. ✓

**2. Placeholder scan:** The doc `[..]` brackets in Task 3 are populated from the Task 2 run (Step 2 forbids leaving any) — this is data-generation, not a plan placeholder. No `TBD`/`TODO`/"handle edge cases" in code steps. ✓

**3. Type consistency:** `Adj = BTreeMap<[u8;32], BTreeSet<[u8;32]>>` in both Task 1 (test mod) and Task 2 (example, aliased `Key`); `adj_r4`/`adj_ring`/`edges` signatures match across tasks; `avg_join` returns `Churn { edges, affected, max_adds }` used consistently in the band; `mk: fn(&BTreeSet<Key>) -> Adj` matches `adj_r4`/`adj_ring`/`adj_full`. ✓
```
