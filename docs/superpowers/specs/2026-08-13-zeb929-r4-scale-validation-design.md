# ZEB-929 Part 1 — R4 bounded-degree scale validation (design)

**Ticket:** ZEB-929 (R4 wiring follow-up), under the ZEB-909 Freenet-inspired epic.
**Depends on:** ZEB-914 (topology engine, `community_topology.rs`) + ZEB-928 (live wiring, PR #674).
**Date:** 2026-08-13. **Author:** J Eng (via Claude).

**Goal:** Empirically prove that the R4 bounded-degree dial filter shipped in PR #674 actually
bounds per-node degree at scale (N=50…200) and that the resulting sparse graph still delivers
messages with **sub-second** membership-change reconvergence — the deferred item 6 of ZEB-928.

**Architecture:** Two measurement vehicles, each matched to the claim it can honestly prove.
A raw-zenoh **scale probe** (extended from the ZEB-912 R3 sounding) measures the *graph's*
routing properties at N=200; a pair of **in-process Rust tests** proves the *live dial pipeline*
produces exactly that graph at N up to 200. No real `harmony-app` fleet (200 full processes on
one host is infeasible — the reason R3 used raw zenoh).

**Tech stack:** raw `zenoh 1.9.0` (probe, out-of-tree scratch crate); `cargo nextest` + the
existing `admission_oracle` / `reconnect_supervisor` / `community_topology` modules (in-tree tests).

## Global Constraints

- **Router-mode gate:** the filter only engages when `HARMONY_ZENOH_MODE=router`. Peer mode is
  byte-for-byte unchanged and out of scope.
- **`FULL_MESH_THRESHOLD = 32`** (`community_topology.rs:33`, a hardcoded const). Below 32 active
  devices, `neighbors_on_ring` returns all-but-self (full mesh) and nothing is denied. Any
  bounded-degree measurement must use **≥ 32 devices**. The const is NOT overridden by this work.
- **Degree law:** bounded degree is **~2·log₂N** (`⌊log₂(N/2)⌋+1` offsets × 2 directions),
  ≈ 14 at N=200 — NOT a fixed 6–10 (ZEB-914 Greptile P1). Genuine O(log N) diameter requires the
  offset *count* to grow with N. **Antipode exception:** when `N/2` is itself a power of two, the
  largest offset `o = N/2` has `(i+o) ≡ (i−o) (mod N)` — forward and backward coincide, so the
  `BTreeSet` collapses them and degree is one *less* than the closed form (e.g. N=64 → 11, N=32 → 9;
  N=200 is unaffected since 100 is not a power of two). All degree assertions therefore compare
  against `community_neighbors(...).len()` directly, never a naively-applied formula.
- **Probe stays out-of-tree** (scratch, per R3 precedent): reconstructed under
  `~/work/zeb912-scale-probe/`, never a workspace member; its source + results are captured in the
  findings doc appendix. No repo/CI weight for the probe.
- **Two-hash distinction** (carried from ZEB-928): an enrolled device Ed25519 verify key
  (`[u8;32]`, on the ring) is NOT the iroh `node_id` (`[u8;32]`, the dial target) and NOT the
  `OwnerAddr` (`[u8;16]`). The oracle's reverse index binds them; tests must respect the split.

---

## Background: what ZEB-928 shipped, and the gap this closes

PR #674 wired `community_topology::community_neighbors` into the live dial path:

- **`AdmissionOracle`** (`admission_oracle.rs`): `enabled = (zenoh_session_mode() == "router")`;
  `admit(node_id)` returns true in peer mode, else admits iff any owner's current bound device key
  is in the `admitted` set (unknown node_id → fail-open true).
- **`run_admission_controller`** (`event_loop.rs:993`): a 2 s poll loop that, on a
  `materialized_version` delta, recomputes `compute_admitted(communities, self_vk)` (union of
  `community_neighbors` over shared communities), calls `oracle.publish_admitted(...)`, and
  `supervisor.kick_sweep()`.
- **Single enforcement point** (`reconnect_supervisor.rs:663`): the dial-dispatch pass parks a
  denied peer as `PeerState::Dormant` and `continue`s (no dial), leaving it in `states` so a later
  admit re-arms it. (See `reference_dial_admission_enforce_at_dispatch`.)

ZEB-928's tests prove each component in isolation (`compute_admitted` correctness; a small
deny→park→admit→dial supervisor loop). **The gap:** nothing yet proves (a) the *emergent graph* at
scale has bounded degree and still delivers, (b) the sparse graph's reconvergence beats the R3 ring
baseline, or (c) the three components *composed* produce exactly the engine's set at N=200. That is
this ticket.

## Claim decomposition

| # | Claim | Property of… | Vehicle |
|---|---|---|---|
| **A** | Delivery survives the bounded mesh (multi-hop routes around holes) | the **graph** | V1 probe |
| **B** | Reconvergence is sub-second (vs R3 ring ~4.6 s @ N=200) | the **graph** | V1 probe |
| **C** | Realized degree ≈ 2·log₂N (≈14 @ N=200) | the **graph** | V1 probe + V2 test |
| **D** | The **live pipeline** produces that graph ("in practice, not just unit tests") | the **wiring** | V2 tests |

The thesis (why B is the payoff): R3's ~4.6 s reconvergence at N=200 was the **plain ring**
(degree 2, diameter ≈ N/2 ≈ 100 hops). The R4 graph is a **circulant small-world** with diameter
~log₂N ≈ 8 hops. Reconvergence is roughly diameter-proportional, so R4 should land near
**~350 ms** — sub-second, a ~13× improvement — at *lower* flood than the ring.

---

## Vehicle 1 — Probe on the engine's real R4 graph (scale metrics, N=50/100/200)

Extend the R3 scale probe (`~/work/zeb912-scale-probe/`, reconstructed from
`docs/research/2026-08-13-zeb912-r3-scale-sounding.md` appendix) with a fourth topology, `R4`,
that builds the **exact** `community_neighbors` circulant graph. Everything else — the
production-mirroring router config, the admin-space flood readout (`@/{zid}/router?_stats=true`),
`boot_convergence_ms`, `reconverge_ms`, `churn_once`, CPU/RSS — is reused unchanged, so the R4 row
is directly comparable to the committed R3 `mesh`/`ring`/`line` tables.

### The R4 topology builder (correctness hinge)

Replicate the engine's offset math faithfully (from `community_topology.rs:60-107`):

```rust
// Power-of-two offsets {1,2,4,…,≤ n/2}; empty for n < 2. Mirrors ring_offsets().
fn ring_offsets(n: usize) -> Vec<usize> {
    let max_off = n / 2;
    if max_off == 0 { return vec![]; }
    let mut offs = vec![]; let mut o = 1usize;
    loop { offs.push(o); match o.checked_mul(2) { Some(x) if x <= max_off => o = x, _ => break } }
    offs
}

// node i's R4 neighbors on a size-n ring, using index-as-rank (valid: the graph is a
// vertex-transitive circulant; the engine's salted-hash sort only relabels vertices).
// Below FULL_MESH_THRESHOLD, full mesh — matches neighbors_on_ring().
fn r4_neighbors(i: usize, n: usize) -> Vec<usize> {
    if n < 32 { return (0..n).filter(|&j| j != i).collect(); } // FULL_MESH_THRESHOLD
    let mut s = std::collections::BTreeSet::new();
    for o in ring_offsets(n) {
        s.insert((i + o) % n);
        s.insert((i + n - o) % n);
    }
    s.remove(&i);
    s.into_iter().collect()
}
```

`connects_for(R4, i, n)` returns `r4_neighbors(i, n)` filtered to `j < i` (dial lower indices only
→ each undirected edge dialed exactly once from its higher-index endpoint; symmetric relation ⇒
full coverage; spawn-order-safe since lower ports are already listening). The `churn_once` joiner
(index n) dials `r4_neighbors(n, n+1)` — its neighbors on the *grown* ring, all `< n`, giving the
joiner degree ~2·log₂(n+1) (the R4 join cost, vs mesh's N and ring's 2).

**Known modeling simplification (carried from R3, for apples-to-apples):** on churn the probe adds
the joiner's edges but does not re-sort the existing ring (a real join bumps `materialized_version`
and shifts some existing edges). The reconvergence-to-joiner metric is diameter-driven, so the
joiner's own links + linkstate propagation dominate; the existing-node re-sort is a second-order
effect the R3 ring/line rows also omitted. Documented as a threat to validity, not silently.

### Metrics captured (per N ∈ {32, 50, 100, 200})

Reuse the R3 table columns: `boot_ms`, `join_reconv_ms`, `join_bytes` / `join_KB`, `hop_ms`,
`idle_cores`, `rss_mb`, **plus a new `degree` column** = `r4_neighbors(mid, n).len()`, the measured
cardinality (antipode-aware — see Global Constraints; ≈14 at N=200). N=32 is added as the
threshold-crossing datapoint (first N where bounded mode engages; degree 9 there).

### Expected results & the comparison that matters

| Metric | R3 mesh | R3 ring (baseline) | R4 (expected) |
|---|---|---|---|
| degree @ N=200 | 199 | 2 | **~14** |
| flood/join | super-linear (collapses ~N=50) | linear (~0.6 KB/node) | linear, ~log N/node |
| **reconv @ N=200** | n/a (collapsed) | **~4650 ms** | **~sub-second (~350 ms)** |
| hop latency | n/a | diameter-proportional | ~log N hops |
| idle cores @ N=50 | ~2.87 | ~0 | ~0 |

The headline deliverable is the **reconv @ N=200: ring ~4.6 s → R4 sub-second** row — the empirical
proof that bounded degree buys back the diameter latency the R3 ring paid. If R4 reconv is NOT
sub-second at N=200, that is a finding (the offset structure or per-hop cost needs revisiting),
reported honestly — the validation can fail.

---

## Vehicle 2 — Scaled wiring proof (in-process, N up to 200 synthetic)

Two deterministic in-tree tests proving claim **D** — the live pipeline produces exactly the
engine's set at scale — at the true `FULL_MESH_THRESHOLD = 32`, no override, no real processes.

### V2a — `compute_admitted` at scale (pure)

In `admission_oracle.rs` tests: build one synthetic community of **200** distinct device keys, pick
a `self_vk` in it, assert `compute_admitted(&[(devices, salt)], self_vk)` equals
`community_neighbors(devices, self_vk, salt)` and that its cardinality is `2·(⌊log₂(200/2)⌋+1) = 14`.
Trivial (compute_admitted is the controller's pure core) but pins the degree bound at the ticket's
headline N.

### V2b — oracle → supervisor enforcement at scale

Extend the existing supervisor loop test pattern (`r4_denied_peer_parked_until_admitted_then_dialed`)
to a synthetic community of **N = 100 devices** (self + 99 dialable peers) with a recording stub
dialer (N=100 chosen for a clean degree of 12 — `50` is not a power of two, so no antipode collapse —
at genuine "hundreds" scale):

1. Generate 100 distinct enrolled device keys (one is `self_vk`); for the 99 peers, mint a distinct
   iroh `node_id` + owner each and bind via `oracle.bind(owner_i, node_id_i, device_key_i)`.
2. Compute the admitted union: `oracle.publish_admitted(compute_admitted(&[(all_100_keys, salt)], self_vk))`.
3. Arm all 99 peer node_ids in the supervisor; run the dispatch loop to quiescence.
4. Snapshot via `states_snapshot()` / `count_peer_states()` and assert (structurally, against the
   engine — never a hardcoded degree):
   - the set of `Connected` (dialed) node_ids == exactly the node_ids whose device key ∈
     `community_neighbors(devices, self_vk, salt)` (which is 12 here);
   - every other node_id is `Dormant` (parked, never dialed);
   - the recording dialer's dial count == `community_neighbors(...).len()` (no over-dial).
5. Then `publish_admitted` a shrunk set (simulate a membership delta) + `kick_sweep()`, and assert a
   previously-Dormant admitted peer becomes `Connected` and a previously-Connected revoked peer is
   NOT re-dialed (deny recovery + revocation, at scale).

This is the "bound proven in practice" test: the real `admit()` decision, driven through the real
dispatch enforcement point, over 64 peers, yields exactly the engine's bounded set. The controller's
polling glue (interval, version-delta skip, retain-still-joined) is orthogonal to the bound and
already covered by ZEB-928's smaller tests; V2b does not re-test it.

---

## Deliverables

1. **Findings doc:** `docs/research/2026-08-13-zeb929-r4-scale-validation.md` — the R4 probe table
   alongside the R3 ring/mesh baseline, the reconv comparison, degree confirmation, threats to
   validity, and the full R4-topology probe source in an appendix (reconstructable, per R3).
2. **V2 tests** (in-tree, via a reviewed PR on `zeblith/zeb-929-r4-harness-validation`): V2a in
   `admission_oracle.rs`, V2b in `reconnect_supervisor.rs`.
3. **Ticket outcome:** a go/no-go on whether the bound + sub-second reconvergence hold in practice,
   feeding the decision on ZEB-929 Parts 2–3 (beacon/pkarr seams, boot backfill) — including whether
   the boot over-dial is material enough to fix.

## Non-goals (explicit)

- No real `harmony-app` N-node fleet (infeasible on one host; pkarr-relay throttles joins).
- No change to `FULL_MESH_THRESHOLD`, the router-mode default, or any production code path.
- ZEB-929 Parts 2 (beacon/pkarr bind seams) & 3 (boot backfill) — gated on these results.
- Cross-community global degree cap; presence-driven healing (other tickets).

## Threats to validity (stated, not hidden)

1. **Probe ≠ real stack:** V1 runs raw zenoh with production-mirroring config, not harmony-app; it
   proves the *graph's* properties. V2 proves harmony *produces* the graph. The seam between "zenoh
   delivers over graph G" (V1) and "harmony builds graph G" (V2) is argued, not directly observed in
   one process. The optional real-stack s16 (deferred) would close it; the ticket accepts the seam.
2. **Index-as-rank:** valid only because the graph is a vertex-transitive circulant — asserted by
   the degree/symmetry checks, and true by construction of `ring_offsets`.
3. **Static churn model:** the join event omits existing-ring re-sort (as R3 did); reconv is
   diameter-driven so this is second-order. Documented in the findings.
4. **Single host, macOS:** absolute latencies are host-relative; the *ratio* R4/ring is the robust
   result, both measured on the same host in the same run.
