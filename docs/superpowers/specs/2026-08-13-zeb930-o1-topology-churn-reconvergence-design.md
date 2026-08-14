# ZEB-930 O1 — Topology churn + reconvergence sensitivity (design)

**Ticket:** ZEB-930 (O1), under epic ZEB-909. Follows ZEB-929 Part 1 (PR #675).
**Status:** design approved 2026-08-13.

## Purpose

Settle R4 **claim B** — *does the bounded-degree circulant's short diameter buy
faster membership-change reconvergence than the R3 ring?* — on an instrument that
is honest about what it can and cannot prove.

ZEB-929 Part 1 measured zenoh's linkstate flood on a raw-zenoh probe but left
claim B inconclusive for two reasons:

1. **Contention confound.** All N nodes ran in one process, so R4's ~7× edge
   count showed up as CPU contention and swamped any diameter benefit — the
   single-process probe cannot credit R4's short O(log N) diameter.
2. **Append-only under-count (CodeRabbit CR-B).** The probe's churn wired only
   the *joiner's* own edges; it never re-ranked the ring, so it never saw the
   membership-change re-dial cost at all.

The decisive reconvergence cost is not zenoh's flood (Part 1 already characterized
that). It is **how much of the mesh must re-dial when membership changes** — and
that quantity is 100% our own deterministic code (`community_topology`), with
**zero fidelity gap**. Re-simulating zenoh's flood would re-measure the thing we
already have while modeling the thing we can compute exactly. So O1 inverts the
emphasis: measure churn exactly, model only the propagation.

### Motivating pre-check (throwaway; the harness reproduces this faithfully)

A combinatorial reproduction of the rank-shift mechanics (no crypto, insertion
positions enumerated) already shows the shape:

| N | R4 rank-based edge-churn/join | nodes re-dialing | max per-node churn | Ring edge-churn/join |
|---|------------------------------:|-----------------:|-------------------:|---------------------:|
| 50  | ~70  | 32 / 50  (64%) | 8  | ~4 |
| 200 | ~266 | 128 / 200 (64%) | 12 | ~4 |

At N=200 a single join forces ~64% of the community to re-dial and some nodes
replace their **entire** 12-link neighbor set, because the hash-ranked insert
shifts every rank past the insertion point and remaps each node's offset targets
(`neighbors_on_ring` selects `ring[(self_rank ± offset) % n]`,
`community_topology.rs:97`). The R3 ring pays a flat ~4. If this holds under the
real blake3 ring it is plausibly *more* decisive for O2 than the flood result.

## Claim decomposition (carried from Part 1)

- **A — delivery over the bounded mesh.** Proven in Part 1 (R3 multi-hop) + the
  live pipeline test. Not re-litigated here.
- **B — reconvergence after membership change.** The subject of O1.
- **C — degree bound.** CONFIRMED exact in Part 1 (9/10/11/12/13/14 at
  N=32/50/64/100/128/200). O1 does not re-measure it.
- **D — wiring enforcement.** Proven in Part 1 (T2 live pipeline). Not here.

Claim B splits into two measurable sub-quantities:

- **B1 — re-dial volume (exact, our code):** on a membership change, how many
  nodes must change connections, and by how much. Pure `community_topology`.
- **B2 — reconvergence time (modeled, honest):** given B1's churn, how long until
  the mesh restabilizes. Depends on transport constants we cannot pin without the
  fleet; O1 gives a parameterized sensitivity band, not a point number.

## Architecture — two layers

### Layer 1 — Churn harness (exact, zero fidelity gap)

Drive the real `community_neighbors` / `ring_order` / `neighbors_on_ring` through
a deterministic sequence of membership events and compute per-event churn from the
before/after adjacencies.

**Topologies compared (three, for honest contrast):**

1. **R4** — production rank-based circulant (`community_neighbors`, salt fixed).
2. **R3 ring** — the claim-B baseline: neighbors = rank ± 1 on the same hash ring.
3. **Full-mesh** — what R4 replaced (the old dial-everyone policy). Included so the
   verdict answers "did R4 make membership-change churn better or worse than what
   we already had", not just "R4 vs an idealized ring". Full-mesh join = every node
   adds exactly one link to the joiner (per-node churn +1, no tear-downs).

**Events:** `join` (insert a new device) and `leave`/`revoke` (remove an existing
device — identical topology operation). Both measured; revocation is a leave.

**Sizes:** N ∈ {32, 50, 64, 100, 128, 200} for the curve; N=50 and N=200 are the
headline sizes from the ticket. (32 and 64 include the antipode sizes for continuity
with Part 1's degree table.)

**Determinism (no RNG surprises):**
- Base device set: `blake3(salt‖ i.to_be_bytes())` for i in 0..N (real keys, same
  helper as `community_topology`'s existing `synth_devices`).
- Joins: a fixed batch of M=64 distinct new keys (tags N..N+M); each measured
  independently → a deterministic distribution over insertion positions.
- Leaves: exhaustive — remove each of the N existing devices once.
- Fixed community salt. No wall-clock, no `Math.random`; the whole sweep replays
  bit-for-bit.

**Metrics, per event, per topology:**
- **edge churn** = links added + links torn down across the whole community
  (adds and tear-downs reported *separately* — Layer 2 uses adds for the dial
  storm; tear-downs are cheap parks).
- **nodes affected** = count of existing nodes whose neighbor set changed.
- **per-node churn distribution** = mean / p95 / max of (adds+drops) over nodes.
- **per-node adds distribution** = mean / p95 / max of adds only (the dial-storm
  driver).

Aggregate across the join batch and the exhaustive leaves → mean and max per size.

### Layer 2 — Reconvergence cost model (modeled, honest)

Translate churn → time with a transparent, named-parameter model:

```
T_reconv(topology, N) ≈ max( flood_inform , redial_storm )
  flood_inform  = D(N) · L                       # inform every node of the change
  redial_storm  = ⌈ max_node_adds / C ⌉ · d      # worst node re-establishes links
```

where:

| Parameter | Meaning | Value / source |
|---|---|---|
| `D(N)` | graph diameter | ring ≈ ⌊N/2⌋; R4 ≈ small-world O(log N), computed exactly by BFS in the harness |
| `L` | per-hop propagation | swept 10–80 ms (LAN↔WAN RTT/2) |
| `C` | per-node concurrent dials | **4** (`max_concurrent_dials`, `reconnect_supervisor.rs:198`) |
| `d` | one iroh+zenoh link bring-up | swept 0.3–1.5 s (hole-punch + session) |
| `max_node_adds` | worst node's new dials | measured exactly in Layer 1 |

`D(N)` is computed exactly (BFS over the real adjacency), not assumed — the harness
already holds every node's neighbor set, so diameter is a free by-product.

The model is a **sensitivity band**: sweep `L` and `d` over their plausible ranges
and report, per size, the T_reconv range for R4 vs ring, plus the **crossover** —
the (L, d) region where R4's short diameter beats the ring despite its re-dial
storm, and where it loses. Claim B's verdict is stated as a *direction* over that
band, with the residual (the true (L, d) for our stack) named as the fleet's job.

Rationale for the `max()`: the two costs overlap in time, not add — the flood
informs while nodes that already know begin re-dialing. `max` of the two dominant
terms is the honest first-order estimate; we do not claim more precision than the
model supports.

## Deliverables & file layout

In-tree (unlike Part 1's out-of-tree scratch probe — this is pure Rust over our own
module, so it is a reproducible, rot-proof, CI-compiled artifact):

- **`src-tauri/examples/topology_churn.rs`** — report generator.
  `cargo run --example topology_churn` prints the churn tables (all three
  topologies, all sizes) and the Layer-2 band. Compiled by CI (`--all-targets`),
  so it cannot rot; run manually to regenerate the doc's numbers.
- **Regression-guard test** (inline `#[cfg(test)]` in `community_topology.rs`)
  pinning the invariants that make the finding durable:
  - ring join-churn ≤ 8 edges at N=200 (stays O(1));
  - R4 join-churn at N=200 ≥ a characterized floor (e.g. ≥ 100 edge-ops, ≥ 40%
    of nodes affected) — locks the storm as a *known* property so a future
    key-distance fix visibly moves it.
  The exact floors are set from the harness's measured values at plan time.
- **`docs/research/2026-08-13-zeb930-o1-topology-churn-reconvergence.md`** —
  findings: churn tables, the Layer-2 sensitivity band, the claim-B verdict, and
  the **O2 conclusion** (see below).

## O2 conclusion (analytical, no new production code)

O1 feeds O2's keep/revisit decision and reasons about the fix analytically only
(per design decision): the rank-based construction's churn comes from *ranks
shifting under insertion*. A **key-distance** construction — offsets taken in
hash-space (a node's ~2^k-distance neighbors by key, not by rank) — localizes a
join's effect to the O(log N) nodes near it in hash-space, so churn should drop
toward the ring's regime while preserving the small-world diameter. O1 states this
as the recommended O2 direction and hands empirical validation to O2/ZEB-914; it
does **not** implement or wire a key-distance variant.

## Threats to validity

1. **Layer 2 is a model.** Absolute T_reconv depends on (L, d) we cannot pin
   without the fleet; O1 reports a band and a crossover, never a single "sub-second"
   claim. Explicitly labeled.
2. **Churn is a topological upper bound.** The harness assumes all affected members
   are online and re-dial; a real change with offline members re-dials less. Stated
   as an upper bound (the worst realistic case, which is what a scale design must
   survive).
3. **Re-dial assumption.** Layer 2 assumes a neighbor-set change actually triggers
   tear-down + dial. This matches the ZEB-928 wiring (the dial set is filtered to
   `community_neighbors`; dropped neighbors park Dormant, new neighbors dial). The
   spec notes this dependency; the harness measures topology, the doc states the
   link to the live behavior.
4. **Permutation independence.** Churn magnitude depends on rank-shift mechanics,
   not the specific hash permutation; the pre-check used identity order and the
   harness uses real blake3 keys — both must land in the same magnitude, which the
   harness confirms.

## Success criteria

- The example prints churn tables for R4 / ring / full-mesh at all sizes, with
  edge churn, nodes affected, and per-node adds/(adds+drops) distributions.
- The regression-guard test pins the ring-O(1) and R4-storm invariants and passes
  under the full gate.
- The findings doc states the claim-B verdict as a direction over the Layer-2 band
  and gives the O2 keep/revisit recommendation with the key-distance rationale.

## Out of scope (YAGNI)

Real fleet; zenoh flood re-simulation; ZEB-930 Parts 2–3 (beacon/pkarr seams, boot
backfill); implementing or wiring a key-distance topology; any production change.
O1 measures and concludes; the fix, if warranted, is O2/ZEB-914.
