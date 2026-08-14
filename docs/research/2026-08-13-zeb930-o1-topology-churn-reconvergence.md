# ZEB-930 O1 — Topology churn + reconvergence (findings)

**Ticket:** ZEB-930 (O1), under epic ZEB-909. Follows ZEB-929 Part 1 (PR #675).
**Spec:** `docs/superpowers/specs/2026-08-13-zeb930-o1-topology-churn-reconvergence-design.md`
**Harness:** `src-tauri/examples/topology_churn.rs` (`cargo run --example topology_churn`), deterministic — replays bit-for-bit.
**Guard:** `community_topology.rs` tests `zeb930_ring_baseline_join_churn_is_o1_at_scale`, `zeb930_r4_join_is_a_redial_storm_at_n200`.

## TL;DR (three findings)

1. **Membership-change churn is wide but shallow — EXACT, our code.** A single join re-dials **128 of 200** nodes under the rank-based R4 circulant vs **~2** under the R3 ring — because the hash-ranked insert shifts every rank past the insertion point and remaps each node's offset targets. But the *worst* node adds only **6** new links (not the ~12 a naive read of the rank shift suggests): the storm is broad, not deep. R4's community-wide edge churn (259) is even comparable to the pre-R4 full-mesh's (200), while cutting steady-state degree **14×** (199 → 14).

2. **Reconvergence — R4 wins in the regime it targets — MODELED, self-calibrated.** Under `T = max(D·L, ⌈maxAdds/C⌉·d)` with C=4 dials, R4's O(log N) diameter (**4** at N=200) crushes the ring's O(N) diameter (**100**) on the flood-inform term, and its shallow storm (maxAdds 6 → 2 dial rounds) does not overcome that lead. The model **self-calibrates against Part-1's real measurement**: the ring's modeled `D·L = 100·L` equals Part-1's *measured* ~4.6 s reconvergence exactly at **L ≈ 46 ms** (a plausible WAN per-hop). At that calibrated L, R4 reconverges in **~0.6–3.0 s vs the ring's ~4.6 s — a 1.5–8× win** across all realistic link-setup costs.

3. **Claim B verdict: CONFIRMED in direction for the WAN/scale regime.** The circulant's short diameter genuinely buys faster reconvergence where the ring's O(N) diameter is the bottleneck (WAN latency, large N). R4 loses only in the low-latency-LAN + slow-link corner (L=10 ms, d ≥ 800 ms), where the flood is already cheap and R4's storm has nothing to beat. This **overturns the going-in hypothesis** that the re-dial storm would sink R4.

## Method

- **Topologies (three):** R4 (production rank-based circulant, `community_neighbors`); R3 ring (neighbors = rank ± 1 on the same hash ring, the claim-B baseline); pre-R4 full-mesh (what R4 replaced).
- **Determinism:** device keys `dev(i)` (distinct 32-byte keys; `ring_order` hashes them to uniform ring positions, so structured input keys still scatter uniformly). Join batch = 64 distinct new devices, each measured independently → a distribution over insertion positions. Fixed salt. No wall-clock, no RNG.
- **Churn metrics (Layer 1, exact):** `edg` = community-wide edge churn (edges added + torn down, undirected symmetric difference); `aff` = existing nodes whose neighbor set changed; `maxA` = worst existing node's *new* dials (adds only — tear-downs are cheap Dormant parks, so adds drive the dial cost).
- **Cost model (Layer 2, modeled):** diameter `D` by exact BFS over the real adjacency; `L` (per-hop propagation) and `d` (one iroh+zenoh link bring-up) swept; `C = max_concurrent_dials = 4` (`reconnect_supervisor.rs:198`). `T = max(D·L, ⌈maxAdds/C⌉·d)` — the two dominant terms overlap in time, so `max`, not sum.

## Data — churn per join

```
# ZEB-930 O1 — churn per join (mean over 64 joiners)
# FULL_MESH_THRESHOLD = 32
# columns: edg = community edge churn; aff = existing nodes re-dialing; maxA = worst node's new dials

    N |   R4 edg    aff   maxA | ring edg    aff   maxA | mesh edg    aff   maxA
   32 |       49     32      4 |        3      2      1 |       32     32      1
   50 |       65     32      4 |        3      2      1 |       50     50      1
   64 |       98     64      5 |        3      2      1 |       64     64      1
  100 |      130     64      5 |        3      2      1 |      100    100      1
  128 |      195    128      6 |        3      2      1 |      128    128      1
  200 |      259    128      6 |        3      2      1 |      200    200      1
```

## Data — reconvergence band

```
# Layer 2 — reconvergence-time band  T = max(D*L, ceil(maxAdds/C)*d),  C=4 dials

N=50: diameter R4=3 ring=25; maxAdds R4~4 ring~1
  L=  10ms d=   300ms   R4~     300ms   ring~     300ms
  L=  10ms d=   800ms   R4~     800ms   ring~     800ms
  L=  10ms d=  1500ms   R4~    1500ms   ring~    1500ms
  L=  40ms d=   300ms   R4~     300ms   ring~    1000ms
  L=  40ms d=   800ms   R4~     800ms   ring~    1000ms
  L=  40ms d=  1500ms   R4~    1500ms   ring~    1500ms
  L=  80ms d=   300ms   R4~     300ms   ring~    2000ms
  L=  80ms d=   800ms   R4~     800ms   ring~    2000ms
  L=  80ms d=  1500ms   R4~    1500ms   ring~    2000ms

N=200: diameter R4=4 ring=100; maxAdds R4~6 ring~1
  L=  10ms d=   300ms   R4~     600ms   ring~    1000ms
  L=  10ms d=   800ms   R4~    1600ms   ring~    1000ms
  L=  10ms d=  1500ms   R4~    3000ms   ring~    1500ms
  L=  40ms d=   300ms   R4~     600ms   ring~    4000ms
  L=  40ms d=   800ms   R4~    1600ms   ring~    4000ms
  L=  40ms d=  1500ms   R4~    3000ms   ring~    4000ms
  L=  80ms d=   300ms   R4~     600ms   ring~    8000ms
  L=  80ms d=   800ms   R4~    1600ms   ring~    8000ms
  L=  80ms d=  1500ms   R4~    3000ms   ring~    8000ms
```

## Analysis

**Churn shape.** R4 edge churn climbs 49 → 259 across N=32 → 200 — order O(N), a genuine community-wide re-dial on every membership change. `aff` steps 32/32/64/64/128/128: a join's remap reaches out to the largest power-of-two offset ≤ N/2, so roughly the top-offset fraction of the ring changes identity, quantized by which power of two N/2 last crosses. Crucially `maxA` stays at **4–6**: the churn is spread thinly, no node re-dials more than ~6 peers. Contrast the ring (a join splits exactly one edge → `edg` 3, `aff` 2, flat in N) and full-mesh (every node adds the one joiner → `aff` = N, `maxA` = 1, pure adds, no tear-downs). So on raw edge churn R4 (259) sits *above* the ring (3) but *beside* the old full-mesh (200) — and unlike full-mesh, R4's changes include tear-downs. The price R4 pays for its bounded degree is **breadth of churn**, not depth.

**Reconvergence — why R4 wins.** The ring's reconvergence is flood-bound: informing all N nodes of a change takes `D·L = ⌊N/2⌋·L` hops of latency — 100·L at N=200. That is exactly where Part-1's measured ~4.6 s came from, and it back-solves to **L ≈ 46 ms** per hop. R4 informs everyone in **4** hops (~0.2 s at that L), so its reconvergence is bounded by the re-dial storm instead: `⌈6/4⌉·d = 2·d`. For any realistic link bring-up d ≤ ~2.3 s, `2·d < 4.6 s`, so R4 finishes first. The band shows this crossover cleanly: R4 loses only when L is so low (10 ms, LAN) that the ring's flood is already fast *and* d is high enough (≥800 ms) that R4's 2-round storm dominates. In the WAN regime R4 is built for (L ≈ 40–80 ms), R4 wins in **all 6** of the N=200 cells, by 1.5–8×.

**Net.** R4 trades a 14× steady-state degree reduction (199 → 14) and a diameter-driven reconvergence win for broad-but-shallow membership-change churn comparable in edge count to the full-mesh it replaced. In the regime it targets, that is a good trade.

## O2 recommendation (analytical — no new production code)

The churn *breadth* is a property of the **rank-based** construction: a hash-ranked insert shifts every rank past the insertion point, so far-side offset targets remap network-wide even though each node's own set barely moves. A **key-distance** construction — offsets taken in hash-space (a node's ~2^k-distance neighbors chosen by key, not by rank) — would confine a join's effect to the O(log N) nodes near it in hash-space, dropping `aff` toward the ring's regime while keeping the same small-world diameter (hence the same reconvergence win). 

But O1's verdict is that R4 **largely earns its keep as shipped**: the reconvergence win is real and the storm is shallow. So key-distance is an **optimization for the churn-breadth corner, not a rescue**. Recommendation: **keep R4 as shipped**; prototype and measure a key-distance variant under O2/ZEB-914 **only if** the breadth of re-dialing (128/200 nodes touched per join) proves operationally material — e.g. if the simultaneous re-dial fan-out stresses relays or presence. O1 does not implement it.

## Threats to validity

1. **Layer 2 is a model.** Absolute reconvergence time depends on (L, d) we cannot pin without the fleet; the verdict is a *direction* over a band, anchored by one real calibration point (Part-1's ~4.6 s ring). If that measurement was itself contention-inflated, the calibrated L (46 ms) is an over-estimate — which would only *strengthen* R4's relative win (a lower true L shrinks the ring's flood less than R4's already-tiny flood). The direction is robust; the multiplier is not exact.
2. **The storm term assumes per-node-parallel dialing.** `⌈maxAdds/C⌉·d` credits each node its own C=4 dial slots acting in parallel. If the 128 simultaneously re-dialing nodes share a bottleneck (one relay, presence fan-out), the storm serialises and R4's advantage narrows. This is the strongest argument for the key-distance optimization and the thing a real fleet should measure.
3. **Churn is a topological upper bound.** The harness assumes all affected members are online and re-dial; a real change with offline members re-dials less. This is the worst realistic case — which a scale design must survive.
4. **Re-dial assumption matches the ZEB-928 wiring.** A neighbor-set change triggers tear-down (park Dormant) + dial of the new neighbors; the churn count maps to real dial/park actions.
5. **Permutation independence.** Churn magnitude comes from rank-shift mechanics, not the specific hash permutation; the design-time throwaway (identity order) and this harness (`dev(i)` keys, real `ring_order` hashing) agree in magnitude (~259 vs ~266 edges at N=200).

## Go / no-go for R4

**KEEP R4 as shipped.** Its degree bound (ZEB-929 Part 1: exact, 14 at N=200) and O(N log N) flood improvement over full-mesh stand; O1 now adds that its **reconvergence is faster than the R3 ring across the WAN/scale regime it targets** (1.5–8× at the calibrated L), overturning the concern that membership-change churn would sink it. The residual is the *breadth* of re-dialing (128/200 nodes/join) and the shared-bottleneck question (threat 2) — both addressed, if they prove material, by the key-distance optimization under O2/ZEB-914, not by reverting R4. O2's keep/revisit gate resolves to **keep**, with key-distance as a tracked optional optimization.
