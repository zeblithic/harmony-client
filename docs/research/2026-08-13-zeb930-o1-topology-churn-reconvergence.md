# ZEB-930 O1 — Topology churn + reconvergence (findings)

**Ticket:** ZEB-930 (O1), under epic ZEB-909. Follows ZEB-929 Part 1 (PR #675).
**Spec:** `docs/superpowers/specs/2026-08-13-zeb930-o1-topology-churn-reconvergence-design.md`
**Harness:** `src-tauri/examples/topology_churn.rs` (`cd src-tauri && cargo run --locked --example topology_churn`), deterministic — replays bit-for-bit.
**Guard:** `community_topology.rs` tests `zeb930_ring_baseline_join_churn_is_o1_at_scale`, `zeb930_r4_join_is_a_redial_storm_at_n200`.

## TL;DR (three findings)

1. **Membership-change churn is wide but shallow — EXACT, our code.** A single join re-dials **128 of 200** nodes under the rank-based R4 circulant vs **~2** under the R3 ring — because the hash-ranked insert shifts every rank past the insertion point and remaps each node's offset targets. Yet the *existing* worst node adds only ~6 links; the largest dial burden is the **joiner** itself, which must establish its full degree-**14** set (inherent to degree 14, not to churn). R4's community-wide edge churn (259) is comparable to the pre-R4 full-mesh's (200), while cutting steady-state degree **14×** (199 → 14).

2. **Reconvergence — R4 wins in the WAN/scale regime, loses on the LAN — MODELED, calibrated.** Under `T = max(D·L, ⌈maxAdds/C⌉·d)` with C=4 dials, R4's O(log N) diameter (**4** at N=200) crushes the ring's O(N) diameter (**100**) on the flood-inform term, while its dial storm is degree-bounded (`⌈14/4⌉ = 4` rounds). So R4 ≈ `max(4·L, 4·d)` and the ring ≈ `max(100·L, d)`. At genuine WAN latency (L ≥ 40 ms) with moderate link setup, R4 reconverges **faster**; on a low-latency LAN (L ≈ 10 ms), the ring's flood is already cheap and R4's 4-round dial storm loses. Calibrating against Part-1's *measured* ~4.6 s ring reconvergence (⟹ **L ≈ 46 ms**): R4 reconverges in **~1.2–6.0 s vs the ring's ~4.6 s**, winning whenever link setup `d < ~1.15 s`, and its margin widens with latency and N.

3. **Claim B: CONFIRMED in direction for the WAN/scale regime — regime-dependent, not universal.** The circulant's short diameter genuinely buys faster reconvergence where the ring's O(N) diameter is the bottleneck (WAN latency, large N). It loses on low-latency LANs and when link setup is slow at only-moderate latency. The exact crossover needs the real (L, d) of our stack. This still overturns the going-in hypothesis that the re-dial storm would sink R4 outright.

## Method

- **Topologies (three):** R4 (production rank-based circulant, `community_neighbors`); R3 ring (neighbors = rank ± 1 on the same hash ring, the claim-B baseline); pre-R4 full-mesh (what R4 replaced).
- **Determinism:** device keys `dev(i)` (distinct 32-byte keys; `ring_order` hashes them to uniform ring positions, so structured input keys still scatter uniformly). Join batch = 64 distinct new devices, each measured independently → a distribution over insertion positions. Fixed salt. No wall-clock, no RNG.
- **Churn metrics (Layer 1, exact):** `edg` = community-wide edge churn (edges added + torn down, undirected symmetric difference); `aff` = existing nodes whose neighbor set changed; `maxA` = worst node's *new* dials (adds only — tear-downs are cheap Dormant parks), **including the joiner**, which establishes its whole neighbor set and is usually the worst-case dialer.
- **Cost model (Layer 2, modeled):** diameter `D` by exact BFS over the real adjacency; `L` (per-hop propagation) and `d` (one iroh+zenoh link bring-up) swept; `C = max_concurrent_dials = 4` (`reconnect_supervisor.rs:198`). `T = max(D·L, ⌈maxAdds/C⌉·d)` — the flood-inform and dial-storm terms overlap in time, so `max`, not sum.

## Data — churn per join

```text
# ZEB-930 O1 — churn per join (mean over 64 joiners)
# FULL_MESH_THRESHOLD = 32
# columns: edg = community edge churn; aff = existing nodes re-dialing; maxA = worst node's new dials

    N |   R4 edg    aff   maxA | ring edg    aff   maxA | mesh edg    aff   maxA
   32 |       49     32     10 |        3      2      2 |       32     32     32
   50 |       65     32     10 |        3      2      2 |       50     50     50
   64 |       98     64     12 |        3      2      2 |       64     64     64
  100 |      130     64     12 |        3      2      2 |      100    100    100
  128 |      195    128     14 |        3      2      2 |      128    128    128
  200 |      259    128     14 |        3      2      2 |      200    200    200
```

## Data — reconvergence band

```text
# Layer 2 — reconvergence-time band  T = max(D*L, ceil(maxAdds/C)*d),  C=4 dials

N=50: diameter R4=3 ring=25; maxAdds R4~10 ring~2
  L=  10ms d=   300ms   R4~     900ms   ring~     300ms
  L=  10ms d=   800ms   R4~    2400ms   ring~     800ms
  L=  10ms d=  1500ms   R4~    4500ms   ring~    1500ms
  L=  40ms d=   300ms   R4~     900ms   ring~    1000ms
  L=  40ms d=   800ms   R4~    2400ms   ring~    1000ms
  L=  40ms d=  1500ms   R4~    4500ms   ring~    1500ms
  L=  80ms d=   300ms   R4~     900ms   ring~    2000ms
  L=  80ms d=   800ms   R4~    2400ms   ring~    2000ms
  L=  80ms d=  1500ms   R4~    4500ms   ring~    2000ms

N=200: diameter R4=4 ring=100; maxAdds R4~14 ring~2
  L=  10ms d=   300ms   R4~    1200ms   ring~    1000ms
  L=  10ms d=   800ms   R4~    3200ms   ring~    1000ms
  L=  10ms d=  1500ms   R4~    6000ms   ring~    1500ms
  L=  40ms d=   300ms   R4~    1200ms   ring~    4000ms
  L=  40ms d=   800ms   R4~    3200ms   ring~    4000ms
  L=  40ms d=  1500ms   R4~    6000ms   ring~    4000ms
  L=  80ms d=   300ms   R4~    1200ms   ring~    8000ms
  L=  80ms d=   800ms   R4~    3200ms   ring~    8000ms
  L=  80ms d=  1500ms   R4~    6000ms   ring~    8000ms
```

## Analysis

**Churn shape.** R4 edge churn climbs 49 → 259 across N=32 → 200 — order O(N), a genuine community-wide re-dial on every membership change. `aff` steps 32/32/64/64/128/128: a join's remap reaches out to the largest power-of-two offset ≤ N/2, so roughly that fraction of the ring changes identity, quantized by which power of two N/2 last crosses. The *existing* nodes each re-dial only a little (worst ~6), but the **joiner** must establish its whole degree-14 set — so `maxA` tracks the degree (10/12/14). Contrast the ring (a join splits one edge → `edg` 3, `aff` 2, and the joiner adds just its 2 ring neighbors) and full-mesh (every node adds the joiner; the joiner adds all N). So on raw edge churn R4 (259) sits *above* the ring (3) but *beside* the old full-mesh (200) — and unlike full-mesh, R4's changes include tear-downs. R4's price for bounded degree is **breadth of churn** among existing nodes plus the **degree-bounded** dial cost of each joiner.

**Reconvergence — why the regime matters.** The ring's reconvergence is flood-bound: informing all N nodes takes `D·L = ⌊N/2⌋·L` — 100·L at N=200, which back-solves to **L ≈ 46 ms** from Part-1's measured ~4.6 s. R4 informs everyone in **4** hops (~0.2 s at that L), so it is bounded instead by its dial storm: the joiner establishing 14 links at concurrency 4 = `⌈14/4⌉ = 4` rounds → `4·d`. The two models are `R4 ≈ max(4·L, 4·d)` and `ring ≈ max(100·L, d)`. That produces a clean regime split:

- **WAN / scale (L ≥ 40 ms):** the ring bleeds on its 100-hop flood (4–8 s), while R4 is bounded by `4·d`. R4 wins for `d < 100·L / 4 = 25·L` (≈ 1 s at L=40 ms, ≈ 2 s at L=80 ms). At the calibrated L ≈ 46 ms, R4 wins for `d < 1.15 s` — reconverging in ~1.2 s (d=300 ms) to ~6.0 s (d=1500 ms) against the ring's 4.6 s.
- **LAN (L ≈ 10 ms):** the ring's flood is already ~1 s, and R4's `4·d` storm (1.2–6.0 s) loses across the board. But at that scale reconvergence is fast for both — not the regime R4 is engineered for.

**Net.** R4 trades a 14× steady-state degree reduction (199 → 14) for broad-but-shallow existing-node churn plus a degree-bounded joiner dial cost, and buys a diameter-driven reconvergence win **in the high-latency / large-N regime it targets**. On low-latency LANs it is slower than the ring, but there reconvergence is cheap regardless.

## O2 recommendation (analytical — no new production code)

The churn *breadth* is a property of the **rank-based** construction: a hash-ranked insert shifts every rank past the insertion point, so far-side offset targets remap network-wide even though each existing node's own set barely moves. A **key-distance** construction — offsets taken in hash-space (a node's ~2^k-distance neighbors chosen by key, not by rank) — would confine a join's effect to the O(log N) nodes near it in hash-space, dropping `aff` toward the ring's regime while keeping the same small-world diameter (hence the same WAN reconvergence win). It would **not** reduce the joiner's own degree-bounded dial cost, which is inherent to degree 14.

O1's verdict is that R4 **earns its keep in the regime it targets**. So key-distance is an **optimization for the churn-breadth corner, not a rescue**. Recommendation: **keep R4 as shipped**; prototype and measure a key-distance variant under O2/ZEB-914 **only if** the breadth of re-dialing (128/200 nodes touched per join) proves operationally material — e.g. if the simultaneous re-dial fan-out stresses relays or presence. O1 does not implement it.

## Threats to validity

1. **Layer 2 is a model, and the calibration cuts against R4, not for it.** The verdict is a *direction* over a band, anchored by one real point (Part-1's ~4.6 s ring ⟹ L ≈ 46 ms). If that measurement was itself contention-inflated, the true `L` is **lower** — which makes the ring's `100·L` flood *cheaper* while R4 stays bounded by `4·d`, **narrowing** R4's advantage (and, in the LAN corner, reversing it: at N=200, L=10 ms, d=800 ms the model already shows R4 3200 ms vs ring 1000 ms). So the direction holds only for genuinely high per-hop latency; it must be qualified until `L` is independently measured on the real fleet.
2. **The storm term assumes per-node-parallel dialing.** `⌈maxAdds/C⌉·d` credits each node its own C=4 dial slots acting in parallel. If the 128 simultaneously re-dialing nodes share a bottleneck (one relay, presence fan-out), the storm serialises and R4's advantage narrows. This is the strongest argument for the key-distance optimization and a thing a real fleet should measure.
3. **Churn is a topological upper bound.** The harness assumes all affected members are online and re-dial; a real change with offline members re-dials less. This is the worst realistic case — which a scale design must survive.
4. **Re-dial assumption matches the ZEB-928 wiring.** A neighbor-set change triggers tear-down (park Dormant) + dial of the new neighbors; the churn count maps to real dial/park actions.
5. **Permutation independence.** Churn magnitude comes from rank-shift mechanics, not the specific hash permutation; the design-time throwaway (identity order) and this harness (`dev(i)` keys, real `ring_order` hashing) agree in magnitude (~259 vs ~266 edges at N=200).

## Go / no-go for R4

**KEEP R4 as shipped.** Its degree bound (ZEB-929 Part 1: exact, 14 at N=200) and O(N log N) flood improvement over full-mesh stand; O1 adds that its **reconvergence is faster than the R3 ring in the WAN / large-N regime it targets** (calibrated ~1.2–6.0 s vs ~4.6 s for link setup under ~1.15 s, widening with latency and N). R4 is slower than the ring only on low-latency LANs — where reconvergence is already sub-second-class for both — and when link setup is slow at only-moderate latency. The residual is the *breadth* of re-dialing (128/200 nodes/join) and the shared-bottleneck question (threat 2) — both addressed, if they prove material, by the key-distance optimization under O2/ZEB-914, not by reverting R4. O2's keep/revisit gate resolves to **keep**, with key-distance as a tracked optional optimization and the (L, d) crossover flagged as needing a real-fleet measurement.
