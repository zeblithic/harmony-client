# ZEB-912 step-3: router-mode scale sounding — design

**Date:** 2026-08-13 · **Ticket:** ZEB-912 (R3, step 3 — the last piece before close) ·
**Branch:** `zeblith/zeb-912-step3-scale-sounding` · **Depends on:** the R3 spike
(`docs/research/2026-08-12-zeb912-r3-zenoh-multihop-spike.md`) and step-2
(PRs #671 router-mode knob, #672/ZEB-927 join-layer snapshot), both merged.

## Goal

Decide, from measured data, **whether to flip zenoh router mode on by default** for
community sessions, or gate it per-community, or hold it behind R4. The spike proved
router mode is the *only* mode that multi-hops in zenoh 1.9.0 and that it works
end-to-end at N=3–4; step-2 shipped it behind `HARMONY_ZENOH_MODE=router` and proved
delivery survives a severed pair. What remains (spike §4.3) is the **cost at scale**:
does router mode's linkstate flood + spanning-tree recompute stay cheap as a full-mesh
community grows toward its realistic ceiling?

## Scope and the N=200 ceiling

The sounding sweeps community size **N ∈ {10, 25, 50, 100, 200}**. The ceiling is a
**product boundary, not a technical guess**: a Harmony community is meant to feel
private, close, and familiar; past ~200 members that intimacy is gone and the use case
belongs to Discord-class tools. So the decision the sounding must answer is sharp:

- **Does full-mesh router mode comfortably serve Harmony's *largest intended* community
  (~200)?** If yes, router-by-default is safe and R4 (ZEB-914, bounded-degree ring)
  becomes a someday-only-if-the-boundary-moves item. If no, we have found a real gap
  *inside* the product's own target range, and the measured breaking-point N hands R4 a
  concrete target maximum degree.

The sweep is extended toward 200 until a metric crosses "unacceptable" (defined in
§Decision) **or** the probe host runs out of headroom — the latter logged as an explicit
limit and reported as a finding, never a silent cap.

**Non-goals.** This is a measurement sounding, not a feature. It does **not**:
- build the flip mechanism (default value / per-community config surface) — that is a
  follow-up gated on the decision;
- build or design R4 (ZEB-914);
- change any production code path. The only durable code artifact is the findings doc.

## Vehicle

**Primary: a raw-zenoh scratch probe.** Extends the spike's probe (same file, same
approach): raw `zenoh = "=1.9.0"` sessions over loopback TCP, production-mirroring config
(`scouting/multicast` off, `scouting/gossip` off, `timestamping.enabled` pinned false —
the router-mode default is true, see spike §3.3), all sessions `mode: "router"`. Built
with `features = ["stats"]` for routing-overhead counters. It is **not** committed as a
workspace crate (its future value is uncertain); instead its complete, runnable source
and exact run instructions live in the findings-doc appendix, exactly as the spike did —
reconstructable on demand, promotable to a crate later if a need emerges.

**Anchor: one real-stack datapoint.** Loopback raw-zenoh must be shown to predict the
real harmony-over-iroh stack, or the extrapolation is untrustworthy. The existing s14
router-mode harness (`e2e-harness/tests/e2e_two_node.rs`) stands up 3 real
`harmony-app` nodes in router mode; we extract (or add a minimal timestamp to) **one
3-node delivery-latency datapoint** and compare it to the probe at N=3.

This anchor validates **latency transfer only**. It does **not** anchor the flood /
recompute cost — and it does not need to. Those costs are a property of zenoh's routing
layer (the linkstate `Network` graph and spanning-tree OAM,
`zenoh-1.9.0/src/net/routing/hat/router/`), which travels as ordinary zenoh messages
*independent of the underlying link transport*. Loopback TCP and iroh carry the identical
OAM; only per-hop RTT differs. This is a code-verified claim (stated as such in the
findings), not a measured one. The absolute latencies over iroh will be higher by roughly
the real-RTT offset the anchor measures; the *scaling shape* is what transfers.

## Topologies (each N)

- **Full mesh** — O(N²) edges. Harmony's emergent steady state: the dial set is
  record-driven and ≥6 healing mechanisms rebuild density (spike §3.1), so a real
  community trends to full mesh. **This is the case the flip decision actually faces** and
  the sounding's primary subject.
- **Ring** — O(N) edges, each node degree 2. The R4 bounded-degree target shape; its
  numbers quantify what R4 would buy versus full mesh at each N.
- **Line** — O(N) edges, diameter N−1. Worst-case tree depth; stresses max-hop latency and
  reconvergence-after-intermediate-drop.

## Churn model

- **Boot burst** — all N sessions open at once; measure **cold convergence**: time until
  the full mesh/tree is built and a designated far pair delivers.
- **Steady churn** — once converged, one session closes and a fresh one opens (same
  neighbor set) every ~2 s, sustained for a fixed window; measure per-event flood,
  per-event reconvergence, and CPU under sustained churn.

## Metrics (per N × topology)

1. **Routing-OAM flood** — bytes and messages of routing overhead per churn event.
   Measured **data-quiescent** (zero application puts during the window), so *every*
   transport byte moved across a churn event is routing/linkstate/Declare traffic. Read
   via zenoh's `stats` counters (`get_stats() → TransportStats`, fed by
   `inc_network_message`/`inc_transport_message`/`inc_bytes` — confirmed present in
   `zenoh-transport-1.9.0`). Snapshot mesh-wide stats at steady state, trigger one churn
   event, snapshot again after reconvergence; the delta is that event's routing cost.
   Report total-per-event and per-session-per-event versus N. **Fallback** if the
   session→transport-stats accessor proves awkward: byte-count the loopback links the
   probe owns. The plan front-loads a task proving the readout at N=3 before the sweep.
2. **Reconvergence time** — after a churn event, time until delivery to the affected node
   is restored, via a tight put-poll loop. Two flavors: **(i) new-joiner-reachable** (all
   topologies) — new session open → it can pub/sub a designated far node; **(ii)
   survivor-repath** (ring/line) — an intermediate on an active path drops → the re-routed
   path delivers.
3. **Steady-state overhead** — whole-process CPU% and RSS at idle steady state per N, and
   under sustained churn. All sessions share one process and (where zenoh allows) one
   tokio runtime; aggregate CPU/RSS is the scaling signal, per-session = aggregate/N. If
   one process cannot host N sessions before N=200 (fd/thread limits), that limit is a
   reportable finding and the probe falls back to multi-process for the affected N.
4. **Multi-hop latency by hop count** — end-to-end pub/sub latency across the topology
   diameter at each N (extends the spike's put/`recv` timing). Full mesh diameter=1
   (baseline); line/ring expose max-hop latency versus N.

## Decision framework

Numbers are finalized from data; the *shape* and the N=200 anchor are fixed now.

- **Flip router-mode on by default** — full-mesh router stays cheap all the way to
  N=200: per-event flood bounded (target: does not grow super-linearly into the tens of
  MB/event range), reconvergence < ~2 s, steady CPU a small fraction of a core, RSS
  modest. Then R4 is deprioritized (needed only if the 200 product boundary later moves).
- **Per-community opt-in** — cheap at small N but a metric degrades past some N\* that is
  still inside (10, 200). Router mode ships opt-in; small communities flip, large ones
  wait for R4. The follow-up builds the per-community config surface.
- **R4 is a prerequisite** — a metric is unacceptable well below 200 (i.e., full mesh
  cannot serve a mid-size intended community). Router-by-default is unsafe; the measured
  breaking-point N becomes R4/ZEB-914's target maximum degree, and the ring-topology
  numbers from this same sounding show the payoff.

"Unacceptable" is defined per-metric before the runs from the flip-by-default targets
above; the findings state which threshold (if any) was crossed and at what N.

## Deliverables and close-out

- **Findings doc** `docs/research/2026-08-13-zeb912-r3-scale-sounding.md` in the spike's
  style: methodology, raw per-run tables, the scaling curve(s), the anchor comparison, the
  transport-agnostic argument for flood, and the decision with its rationale. Full probe
  source + run instructions in the appendix.
- **Update ZEB-912** with the decision and **close it** (step 3 was the last piece).
- **If the decision is "opt-in" or "R4 prerequisite":** record the target degree / N\* on
  **ZEB-914** so R4 inherits a measured target; note any config-surface follow-up as a new
  ticket (not built here).

## Risks and caveats

- **Loopback vs iroh fidelity** — addressed by the anchor (latency) + the code-verified
  transport-agnostic argument (flood/recompute). Stated honestly in the findings.
- **`stats` readout accessor** — the one real implementation unknown; de-risked by a
  front-loaded N=3 readout task and the loopback-byte fallback.
- **Host headroom at N=200** — 200 zenoh sessions in one process may hit fd/thread limits
  before 200; handled by multi-process fallback and reported as a finding either way.
- **Single-host contention** — CPU/latency numbers are from one machine (Koya); the
  *scaling shape* is the signal, absolute numbers are host-relative. Noted in findings.
