# ZEB-929 Part 1 — R4 bounded-degree scale validation (findings)

**Date:** 2026-08-13. **Host:** Koya (macOS, Apple Silicon). **Ticket:** ZEB-929 (R4 wiring follow-up), epic ZEB-909.
**Spec:** `docs/superpowers/specs/2026-08-13-zeb929-r4-scale-validation-design.md`.
**Validates:** the R4 bounded-degree dial filter shipped in PR #674 (ZEB-928), wiring the ZEB-914
topology engine (`community_topology::community_neighbors`) into the live dial path.

## TL;DR

Three results, of decreasing certainty:

1. **Degree bound — CONFIRMED, exact and reproducible.** The realized per-node degree is exactly the
   engine's antipode-aware law (9, 10, 11, 12, 13, 14 at N = 32, 50, 64, 100, 128, 200), both in the
   raw-zenoh probe graph and in the live `controller → oracle → supervisor` pipeline (in-tree tests
   T1/T2). Claim C holds in practice, not just in unit math.
2. **Flood — R4 costs materially MORE than a plain ring (real, not an artifact).** ~9–13× the ring's
   per-join flood, reproducible across two runs. It is inherent: an R4 joiner wires up ~2·log₂N links
   vs the ring's 2, so join cost is degree-proportional and total flood is O(N·log N) vs the ring's
   O(N). **Bounded *degree* does not bound *flood*** — zenoh's linkstate is global/edge-bound, so the
   log-N shortcut chords flood like a scaled-down mesh.
3. **Reconvergence (the headline claim B) — INCONCLUSIVE; the probe cannot settle it.** R4 is fast at
   N ≤ 64 (0–1 ms) but degrades noisily at N ≥ 100 (812 ms and 1934 ms on two runs; timeout at N=200).
   The single-process probe conflates R4's ~7× edge count with CPU contention, penalizing it for edges
   while giving no credit for its shorter diameter — the very thing a real distributed fleet rewards.
   The predicted "sub-second vs the ring's ~4.6 s at N=200" is neither confirmed nor fairly refuted
   here. This is precisely the "probe ≠ real stack" threat the spec flagged.

**Disposition:** R4 remains a large, real improvement over full mesh (O(N log N) ≪ the O(N²) that
collapsed at N≈50 in the R3 sounding), and its degree bound is validated. But it does **not** match the
ring's flood efficiency, and its reconvergence advantage over a simple ring is **unproven on one host**.
Two open items follow (see §5). R4 stays as shipped; no code changes to the topology.

---

## 1. Method

The vehicle is the ZEB-912 R3 raw-zenoh **scale probe** (`~/work/zeb912-scale-probe/`, out-of-tree,
per R3 precedent), extended with a fourth topology, `R4`, that builds the engine's exact circulant
graph. All other machinery — production-mirroring router config (`mode:"router"`, scouting/gossip off,
timestamping pinned false), the admin-space flood readout (`@/{zid}/router?_stats=true`),
`boot_convergence_ms`, `reconverge_ms`, `churn_once`, CPU/RSS — is reused unchanged, so R4 rows are
directly comparable to the committed R3 `mesh`/`ring`/`line` tables. Metrics are measured
**data-quiescent** (zero app puts in the window), so every transport byte is routing/linkstate overhead.

The R4 graph is built by **index-as-rank**: probe node `i` sits at ring-rank `i` and connects to ranks
`(i ± offset) mod n` for the power-of-two offsets `{1,2,4,…,≤n/2}`. This is faithful because the engine's
graph is a **vertex-transitive circulant** — its salted-hash sort only relabels vertices, leaving every
measured property (degree, diameter, flood) invariant. The degree column (below) matches the engine's
`community_neighbors` cardinality exactly, confirming graph fidelity. Full source delta in the appendix.

Two independent runs were captured: run 1 at N ∈ {32, 50, 100, 200}, run 2 at a finer, mostly-unsaturated
grid N ∈ {50, 64, 100, 128}. Raw output: `~/work/zeb912-scale-probe/sweep-r4.md` and `sweep-r4-v2.md`.

## 2. Data

**Run 1 — N ∈ {32, 50, 100, 200}.** `join_reconv_ms` = time for a far node (rank n/2) to reach a fresh
joiner after the join; `join_KB` = total flood across all nodes for that one join; `idle_cores` = whole-
process CPU over a 2 s quiescent window; `n/a` = the 30 s (reconv) / 60 s (boot) budget elapsed.

| topology | N | degree | boot_ms | reconv_ms | join_KB | hop_ms | idle_cores | rss_mb |
|---|---|---|---|---|---|---|---|---|
| ring | 32 | 2 | 203 | 1 | 20.0 | 0.189 | 0.00 | 29 |
| ring | 50 | 2 | 202 | 2 | 31.0 | 0.198 | 0.00 | 63 |
| ring | 100 | 2 | 203 | 3 | 61.7 | 0.404 | 0.00 | 177 |
| ring | 200 | 2 | 17922 | 6084 | 129.3 | 0.171 | 0.45 | 691 |
| **r4** | 32 | **9** | 203 | 0 | **143.8** | 0.166 | 0.02 | 737 |
| **r4** | 50 | **10** | 203 | 0 | **274.6** | 0.141 | 0.03 | 816 |
| **r4** | 100 | **12** | 2696 | **1934** | **898.7** | 0.218 | 0.31 | 1036 |
| **r4** | 200 | **14** | n/a | **n/a** | **43728.9** | n/a | **3.15** | 1536 |

**Run 2 — N ∈ {50, 64, 100, 128}** (finer grid, mostly below the single-process saturation ceiling).

| topology | N | degree | boot_ms | reconv_ms | join_KB | hop_ms | idle_cores | rss_mb |
|---|---|---|---|---|---|---|---|---|
| ring | 50 | 2 | 202 | 2 | 31.0 | 0.358 | 0.00 | 46 |
| ring | 64 | 2 | 203 | 2 | 40.3 | 0.226 | 0.01 | 95 |
| ring | 100 | 2 | 367 | 3 | 62.3 | 0.206 | 0.04 | 214 |
| ring | 128 | 2 | 2099 | 1419 | 80.8 | 0.349 | 0.08 | 387 |
| **r4** | 50 | **10** | 202 | 1 | **277.2** | 0.355 | 0.02 | 493 |
| **r4** | 64 | **11** | 202 | 0 | **492.1** | 0.238 | 0.03 | 610 |
| **r4** | 100 | **12** | 3439 | **812** | **811.7** | 0.177 | 0.21 | 837 |
| **r4** | 128 | **13** | n/a | **4308** | **1506.5** | 566.052 | 0.03 | 1169 |

R3 committed baseline for reference: ring N=200 → reconv **4650 ms**, flood **130 KB**; mesh collapses
at N≈50 (a single join floods ~205 MB and delivery times out). This run's ring N=200 reconv (6084 ms) is
consistent with the R3 4650 ms (same vehicle, host variance).

## 3. Analysis

### 3.1 Degree bound — confirmed (claim C ✅)

The `degree` column is the exact cardinality of `community_neighbors` at each N, and it matches the
antipode-aware law to the unit: `2·(⌊log₂(N/2)⌋+1)`, minus one when `N/2` is a power of two (the largest
offset `o=N/2` has `(i+o)≡(i−o) mod N`, collapsing forward/backward). Hence 9 (N=32), 10 (N=50), 11
(N=64), 12 (N=100), 13 (N=128), 14 (N=200). The in-tree tests corroborate: T1 pins 14 at N=200 from
`compute_admitted`; T2 shows the live dial-dispatch pipeline connecting exactly the 12 engine-selected
neighbors out of 99 peers at N=100, parking the rest. The bound is real, in the graph and in the wiring.

### 3.2 Flood — R4 ≫ ring, and it is real (claim: a genuine cost, not the predicted win)

R4's per-join flood is ~9–13× the ring's at matched N, reproducibly (N=100: 812–899 KB vs 62 KB; N=50:
275–277 KB vs 31 KB). The mechanism is structural, not an artifact:

- **Join cost is degree-proportional.** A joiner establishes one transport per neighbor and floods the
  new adjacencies' linkstate. R4's joiner has ~2·log₂N neighbors; the ring's has 2. So each R4 join
  inherently costs O(log N)× more than a ring join, and the steady-state edge set is O(N·log N) vs O(N).
- **zenoh linkstate is global / edge-bound.** Every router carries the full topology; a change floods
  network-wide. The flood therefore scales with total *edges*, not per-node *degree*. R4's log-N shortcut
  chords multiply the edge count, so R4 floods like a scaled-down mesh — cf. R3's mesh N=25 (300 edges) =
  843 KB, next to R4 N=100 (600 edges) = 812–899 KB, the same ~edges→bytes regime.

The load-bearing correction: **bounding per-node degree does not bound the linkstate flood.** R4 was
designed to bound degree; it does. But the flood — the thing that actually collapsed full mesh in R3 — is
edge-bound, and R4 has O(N log N) edges. R4 is far better than mesh's O(N²), but strictly worse than the
ring's O(N).

### 3.3 Reconvergence — inconclusive; the probe is biased against R4 (claim B: unproven)

The design thesis was that R4's O(log N) diameter buys sub-second membership reconvergence, beating the
ring's ~4.6 s at N=200 (the ring pays for its ~N/2 diameter). The data does not support that here — but
it also cannot fairly refute it, because the probe is a biased instrument for exactly this question:

- **The confound.** All N nodes run in *one process*; CPU and scheduler contention scale with total
  edges. R4 has ~7× the ring's edges, so the probe charges R4 for its edge count while a real
  distributed system would not — each node there is a separate machine with only ~14 links.
- **No credit for diameter.** R4's payoff is a short diameter (~log₂N hops), which shows up as fast
  convergence only when per-node work runs in parallel across machines. A single process serializes that
  work, erasing the benefit the topology exists to provide.
- **The symptoms fit contention, not a clean property.** R4 reconv is fine at N≤64 (0–1 ms, edges still
  few), then degrades *noisily* at N≥100 (812 ms vs 1934 ms on two runs — 2.4× variance), and at N=200
  hits outright saturation (3.15 cores, delivery timeout, a 43.7 MB retransmission-storm flood). Noise
  and a CPU ceiling are contention fingerprints, not a reproducible latency law.

So claim B is **unmeasured**, not disproven. The probe was the right tool for flood scaling (it caught
mesh's O(N²) in R3 and R4's O(N log N) here), but it is the wrong tool for a diameter-vs-edges reconvergence
trade — that needs the real distributed fleet the spec named as the gold standard and deferred as
infeasible on one host.

## 4. Threats to validity

1. **Probe ≠ real stack (central).** V1 runs raw zenoh with production-mirroring config, not harmony-app.
   It proves the *graph's* transport properties (flood) faithfully; it does **not** fairly measure a
   topology whose value is per-node-parallel diameter (reconvergence). §3.3.
2. **Single host, single process.** CPU/fd contention scales with edges; R4 is disproportionately charged.
   The N=200 R4 row is dominated by this and should be read as a probe ceiling, not a router-mode limit
   (cf. R3's identical caveat for mesh).
3. **Index-as-rank.** Valid only because the graph is a vertex-transitive circulant — corroborated by the
   exact degree match.
4. **Static churn model.** The join event adds the joiner's edges but does not re-sort the existing ring
   (as R3 did); reconv is diameter-driven, so this is second-order.
5. **Absolute vs ratio.** Absolute latencies are host-relative; the robust results are the *degree* (exact)
   and the *flood ratio* R4/ring (reproducible), both measured on the same host in the same run.

## 5. Go / no-go and open items

**Go:** R4's degree bound and its improvement over full mesh are validated. The shipped wiring (PR #674)
is correct and bounded; no topology code changes are warranted by these results. T1/T2 lock the bound as
regression guards.

**Two open items** (documented, not resolved here):

- **O1 — Reconvergence claim (B) needs the real fleet.** The sub-second-reconvergence premise of ZEB-914
  is neither confirmed nor refuted on one host. Settling it requires N=50/200 across separate processes or
  machines, where per-node parallelism lets diameter matter. This is the harness-validation the ZEB-929
  scope already names; it should be treated as *required to trust claim B*, not merely nice-to-have.
- **O2 — Flood cost vs a cheaper topology.** Because zenoh linkstate is edge-bound, the circulant's
  O(N log N) edges flood ~10× a ring. Whether the log-N chords earn that cost depends entirely on O1's
  outcome. If the diameter benefit does not materialize in the real fleet, a cheaper topology (plain ring,
  or ring + a small fixed number of long chords ⇒ far fewer edges) may be the better degree/flood/diameter
  trade for zenoh. A design revisit of ZEB-914 is warranted **iff** O1 shows the reconvergence win is
  small.

Boot over-dial (ZEB-929 Part 3) was not quantified here; it is gated on the real-fleet harness (O1) and
should be measured there.

## Appendix — R4 topology probe source (delta atop the R3 probe)

Applied to `~/work/zeb912-scale-probe/src/main.rs` (the R3 probe, whose full source is the appendix of
`docs/research/2026-08-13-zeb912-r3-scale-sounding.md`). Reconstructable on demand; not committed.

```rust
/// Mirror of community_topology::ring_offsets — powers of two {1,2,4,…,≤ n/2}.
fn ring_offsets(n: usize) -> Vec<usize> {
    let max_off = n / 2;
    if max_off == 0 { return vec![]; }
    let (mut offs, mut o) = (vec![], 1usize);
    loop {
        offs.push(o);
        match o.checked_mul(2) { Some(x) if x <= max_off => o = x, _ => break }
    }
    offs
}

/// R4 neighbors of ring-rank `i` on a size-n ring (index-as-rank; below FULL_MESH_THRESHOLD=32,
/// full mesh). The BTreeSet collapses the antipodal offset when n/2 is a power of two, matching
/// the engine's degree exactly.
fn r4_neighbors(i: usize, n: usize) -> Vec<usize> {
    if n < 32 { return (0..n).filter(|&j| j != i).collect(); }
    let mut s = std::collections::BTreeSet::new();
    for o in ring_offsets(n) { s.insert((i + o) % n); s.insert((i + n - o) % n); }
    s.remove(&i);
    s.into_iter().collect()
}

// enum Topo { Mesh, Ring, Line, R4 }  +  name(): Topo::R4 => "r4"

// connects_for(): dial only lower-index ring neighbors (each undirected edge forms once).
Topo::R4 => r4_neighbors(i, n).into_iter().filter(|&j| j < i).map(|j| base + j as u16).collect(),

// churn_once(): joiner at rank n on the grown size-(n+1) ring; all its neighbors are existing lower ranks.
Topo::R4 => r4_neighbors(n, n + 1).into_iter().map(|j| base + j as u16).collect(),

// main(): degree column
let degree = match &topo {
    Topo::Mesh => n.saturating_sub(1),
    Topo::Ring => 2,
    Topo::Line => 1,
    Topo::R4 => r4_neighbors(n / 2, n).len(),
};
// sweep: let topos = [Topo::Ring, Topo::R4];  let sizes = [32, 50, 100, 200]; (run 2: [50, 64, 100, 128])
```
