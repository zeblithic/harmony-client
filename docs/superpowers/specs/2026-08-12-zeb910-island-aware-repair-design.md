# ZEB-910: Island-aware community repair — design

**Ticket:** [ZEB-910](https://linear.app/zeblith/issue/ZEB-910) (R1 of the Freenet review, ZEB-909 epic)
**Verified against:** main @ `2ed1baf1` (2026-08-12)
**Prior art:** ZEB-824 (gateway-dial driver), ZEB-827 (vouch-verified beacons), ZEB-918 (live epoch keys), ZEB-620/621/622/634 (reconnect supervisor), ZEB-804/805 (traffic-proof lesson)

## 1 · Problem (all three premises re-verified on current main)

A community that splits into two internally-connected islands never self-heals:

1. **Driver level** — the starvation predicate is `members.iter().any(|m| connected_owners.contains(m))` (`community_gateway_dial_driver.rs:435`): one live member session reads as Healthy. In a two-island split every node has ≥1 intra-island link, so both sides report Healthy and beacon repair never fires. Worse (verified this cycle): `connected_owners` derives from supervisor `PeerState::Connected`, which is **registry occupancy** — set on the zenoh-link registry swap (`zenoh_iroh_transport.rs:375`) or dial success, cleared only when `conn.closed()` fires the drop-watcher. A blackholed-but-unclosed QUIC connection reads Connected indefinitely, so the driver can report Healthy for a fully dark community. This is the ZEB-804/805 "session exists ⇒ healthy" lie at community granularity.
2. **Pair level** — a reconnect-supervisor slot goes `Dormant` after `dormant_after` (900 s) of no fresh interest (`reconnect_supervisor.rs:834-835`) and is never scheduled again for process life; pkarr stale-refresh (`maybe_refresh_stale`) fires only while dispatching a `Retrying`-and-due peer (`reconnect_supervisor.rs:597`, sole call site), so a Dormant peer's address is never re-resolved either. Every revival is edge-triggered (record add/change kicks, drop-watchers, roster-edge sweeps) or app restart.
3. **Aggravator** — rendezvous has `RENDEZVOUS_SLOT_COUNT = COMMUNITY_RELAY_ADVERTISERS_MAX = 4` slots (`community_rendezvous.rs:31`, `community_relay_announce.rs:24`); if all advertisers sit in one island, the other island can't find a bridge even when its driver fires. Additionally (verified): the escalating-batch resolve returns **one** winning beacon, so a repair pass can keep re-finding the same intra-island advertiser and never bridge.

## 2 · Design overview

Three coordinated parts, all client-side (`harmony_pkarr` and core stay untouched):

- **Part 1 — coverage-based health + repair** (gateway-dial driver): replace the any-session predicate with a three-state coverage measure (`Healthy` / `Degraded` / `Starved`) whose *numerator is proven traffic* and whose *denominator is fresh-record membership*, and upgrade the repair action to bridge splits: all-slots beacon resolve seeding every verified hit, member-record force refresh, and targeted revival of Dormant slots.
- **Part 2 — Dormant parole** (reconnect supervisor): a low-frequency loop-internal tick that re-arms a small batch of record-backed Dormant peers with a paired stale-refresh, making pair-level recovery steady-state instead of edge-triggered.
- **Part 3 — rendezvous slot raise** (4 → 8, decoupled from the relay-read cap): a cheap probabilistic hardening against one island holding every beacon slot. Deliberately modest — see §6 for why rank-diversity selection is unworkable.

Mixed-version fleets degrade gracefully (§8). No wire, CRDT, or IPC schema changes; telemetry gains two additive outcome arms and counters.

## 3 · Part 1 — coverage-based split detection

### 3.1 The measure

Computed per community per pass in `run_one_pass`, replacing the single `any` predicate:

- **M** = `members_of(community)` — Joined members excluding self (unchanged).
- **E** (eligible / denominator) = members of M with ≥1 dial-view row (`list_dialable_peers`) whose `effective_announced_at_ms` is within `GATEWAY_COVERAGE_RECORD_FRESH_MS` = `STALE_RECORD_REFRESH_MS` (24 h). `list_dialable_peers` applies no freshness filter today; the driver filters.
- **P** (proven / numerator) = members of M with ≥1 device whose **proven-traffic stamp** is within `GATEWAY_PROVEN_TRAFFIC_MS` = `STALENESS_QUIET_MS` (5 min). The stamp is the ZEB-804 fusion, exposed to the driver through a new ctx seam (§3.2): `max(peer-liveness rx app-frame stamp, PeerTrafficRegistry::last_any_served_ms)` per node-id.
- **U** (unproven) = E \ P.

Verdicts:

| Condition | Verdict | Behavior |
|---|---|---|
| M empty | `SoloCommunity` | unchanged |
| P empty (and M nonempty) | **Starved** | today's semantics, upgraded repair (§3.3) |
| P nonempty, U nonempty | **Degraded** (new) | same ladder + repair, new telemetry arms |
| P nonempty, U empty | `Healthy` | ladder cleared, unchanged |

Notes:

- The numerator deliberately does NOT use `connected_owners`. Zenoh keeps per-link keepalives flowing as QUIC stream data, so any live link shows rx app frames continuously; >5 min of rx silence on a "connected" link is genuinely anomalous (blackhole, wedge, split). The liveness stamp is coarse (30 s watcher tick) — it lags, never leads, so a 5-min window has ample margin. Per ZEB-805, coverage counts merge-shaped *evidence*, not session existence.
- Members with no dial-view rows at all (M \ E ∪ record-holders that aged out) do NOT hold the community in Degraded — a member offline long enough for their record to decay (or be evicted) is expected-unreachable, not split evidence. They still get a discovery attempt during repair passes (§3.3, action 3).
- **Why the denominator self-maintains during a real split:** a split member keeps publishing to the pkarr relay (internet path, unaffected by the mesh split), so the *relay's* copy stays fresh. Our copy would age past 24 h — except the repair's forced refresh (§3.3, action 2) keeps re-pulling it while the community is non-Healthy. The relay thus becomes the arbiter: still-publishing member ⇒ our copy stays fresh ⇒ stays in E ⇒ detection persists; stopped-publishing member ⇒ relay copy ages too ⇒ ages out of E ⇒ community returns to Healthy. If pkarr itself is down there is no repair path anyway and detection degrades to today's behavior — acceptable and observable via `resolveError`.
- OS sleep/resume of the local device makes every stamp stale simultaneously → transient mass-Degraded on resume. Bounded: repairs are ladder-paced and refresh is cooldown-bounded; stamps recover within ~1 min of links re-establishing.

### 3.2 New ctx seam: proven-traffic lookup

The driver gets a `TrafficEvidenceFn` (the `JoinedCommunitiesFn` closure pattern):

```rust
pub type TrafficEvidenceFn = Arc<dyn Fn(&[u8; 32]) -> Option<u64> + Send + Sync>;
```

returning the fused proven-traffic wall-ms for a node-id, `None` when no evidence exists. Production wiring composes the peer-liveness registry stamp and `PeerTrafficRegistry::last_any_served_ms` with `max` (the same fusion `network_health.rs:2919-2930` performs for the snapshot). Tests inject a `HashMap`-backed closure. `None`/absent ⇒ unproven — a peer with no evidence must never count as proven.

### 3.3 The repair action (shared by Starved and Degraded)

One ladder (`LadderState`, unchanged pacing: 30 s base → 600 s cap, first sighting fires immediately) entered on any non-Healthy verdict. A due pass runs four actions:

1. **All-slots beacon resolve, seed every hit.** New client-side `resolve_rendezvous_all_slots` in `community_rendezvous.rs`: probe every slot (0..`RENDEZVOUS_SLOT_COUNT`) across the time-epoch tolerance window via the existing `IdentifiedSlotResolver`, collect ALL distinct verified hits (dedup by `iroh_node_id`), plus the same `resolve_errors` / `membership_rejects` counts. The `BeaconResolver` trait's method becomes multi-hit (`BeaconsOutcome { hits: Vec<IdentifiedBeacon>, resolve_errors, membership_rejects }`); the driver seeds + kicks each hit after the existing CR-2 fresh-enrollment re-validation, and classification maps `hits.is_empty()` through the existing `NotFound`/`ResolveError`/`RejectedNonMember` logic. The ZEB-918 epoch-key candidate ladder is preserved: candidates in order, first candidate yielding ≥1 hit wins, current-key outcome recorded when none does. Rationale: the single-hit resolve stops at the first beacon, which on a split is coin-flip likely to be an already-reachable member; seeding all hits maximizes bridge probability at a bounded cost (≤ `RENDEZVOUS_SLOT_COUNT` × 2 probes per pass, ladder-paced).
2. **Member-record force refresh for U.** For each unproven member's dial-view rows: `refresh_if_older_than(owner, node, now, GATEWAY_DIAL_RETRY_CAP_MS)` — a new `pub(crate)` variant of `maybe_refresh_stale` with the staleness threshold as a parameter (the existing method delegates with `STALE_RECORD_REFRESH_MS`). The 15-min per-owner cooldown, fleet-sibling skip, no-fallback bail, and 4-permit semaphore all stay — only the staleness bar drops, so "force" is still rate-bounded. This is the member-keyed pkarr path (identity-case records, per-owner keys, no slot contention) — the robust cross-island discovery channel, and what keeps E fresh (§3.1).
3. **Record-less member discovery.** For members with zero dial-view rows: `resolve_async_with_source(owner)` — the existing cache-miss path; it hits pkarr only when the cache is truly empty, which is exactly this case. Runs only while the community is non-Healthy, so a forever-offline member costs nothing in steady state.
4. **Targeted Dormant revival.** For each device of an unproven member whose supervisor slot is `Dormant`: `kick(node, ReconnectTrigger::RecordChanged)` (revives at base rung via `apply_trigger`). Deliberately **only** Dormant slots — kicking Retrying peers would reset live backoff ladders every repair pass and multiply dial pressure ~4×; Retrying peers are already trying. Absent slots need no kick: the refresh/discovery record-writes auto-kick `NewPeer`/`RecordChanged` through the resolver's supervisor hooks.

### 3.4 Telemetry

Two new `GatewayBootstrapOutcome` arms (additive wire strings, diagnostics/e2e-only — no frontend consumers exist):

- `DegradedWaiting` (`"degradedWaiting"`) — partial coverage, ladder not due. Row-only, like `StarvedWaiting`.
- `DegradedSeeded` (`"degradedSeeded"`) — a due Degraded pass that seeded ≥1 beacon; counter `degraded_seeded`. A due Degraded pass that seeds nothing records the resolve-shaped arm (`NoBeacon`/`ResolveError`/`RejectedNonMember`) exactly as a starved pass does — those semantics are about the resolve infrastructure and stay uniform.

`BeaconSeeded` remains the starved-pass seeded arm, so starved-vs-degraded seeding is distinguishable per community. The summary DTO gains the new counter (serde camelCase; e2e asserts use the DTO key).

## 4 · Part 2 — Dormant parole (reconnect supervisor)

Loop-internal periodic tick (the loop already owns scheduling and holds the resolver — producers stay outside, per the module's design note):

- **Config:** `parole_interval: Duration` (default **15 min** — ≥ `PKARR_REFRESH_COOLDOWN` so paired refreshes aren't cooldown-blocked; low-frequency per R1) and `parole_batch: usize` (default **2**). Tests use ms-scale values + the seeded jitter harness.
- **Scheduling:** a `next_parole: Instant` deadline folded into the loop's `select!` sleep (paused-clock `Instant`, NOT wall `now_ms()` — the harness pins scheduling to tokio's paused clock).
- **On fire:** among `Dormant` slots that are **record-backed** (`resolve_by_node_id` returns a row), pick up to `parole_batch` oldest by `since_ms`. For each: `resolver.maybe_refresh_stale(owner, node, now_ms())` (the standard 24 h/15 min gates — parole is background hygiene, not urgent repair), then re-arm `Retrying { attempt: 0 }` with the jittered base delay, refresh `last_fresh_trigger`, bump the slot epoch.
- **Self-bounding:** a parolee that keeps failing re-enters Dormant after `dormant_after` (900 s) — each parole grants one bounded retry window (~10 attempts). Batch × interval caps steady-state parole load at ~8 peers/hour regardless of Dormant population.
- **Record-less Dormant slots are skipped:** dialing is record-gated, so a parole without a record is pure state churn (soft-fail ladder, zero dials). Their healing paths are record arrival (auto-kick) and Part 1's repair actions 2-3. ZEB-634's eviction semantics are preserved — departed members' slots were *removed*, not parked, so parole cannot resurrect them.
- **Telemetry:** `DialTelemetry` gains a `paroled` counter; `tracing::debug!` per parole with peer + dormant-age.

Part 1's targeted revival (§3.3.4) and parole overlap by design — defense in depth. Part 1 is evidence-directed and community-scoped; parole is global hygiene that also covers peers outside any Degraded community's view (fleet-sibling entries are skipped by refresh but still get re-armed; multi-device edges where the owner reads proven via another device).

## 5 · Part 3 — rendezvous slot raise (4 → 8, decoupled)

`RENDEZVOUS_SLOT_COUNT` becomes its own constant (**8**), no longer aliasing `COMMUNITY_RELAY_ADVERTISERS_MAX` (stays **4**):

- The two caps serve different consumers: slots bound *discovery* (who publishes a bridgeable beacon), the relay cap bounds *service reads* (`relays_for_community` → pull drivers). Raising the read cap would double relay-pull fan-out for volunteer-rich communities — not this ticket's goal. The `assert_eq!(RENDEZVOUS_SLOT_COUNT, COMMUNITY_RELAY_ADVERTISERS_MAX)` pin test is replaced by docs + a test pinning 8.
- Core's `core_slot_for_advertiser(advertisers, me, cap)` takes the cap as a parameter — no core change. Rank 4-7 advertisers (self-nominated, addr-sorted — `harmony-pkarr` rev `88861ae` `rendezvous.rs:254-268`) now publish beacons instead of getting nothing.
- Compatibility: slot index is just a key-derivation input. Old resolvers probe 0-3 (their curve's max width) and still find those slots; new resolvers widen to 8 and see old publishers' 0-3. No wire change (§8).

## 6 · Rejected alternatives

- **Rank-diversity advertiser selection** (ticket's "and/or"): the rank must be computed identically by every member (single-writer-per-slot invariant), but any diversity input (liveness, relay URL, NAT class) rides gossip that *diverges exactly when the community splits*. Worse, rank is positional among the *visible* fresh advertiser set: post-split each island ranks only its own advertisers, so both islands contend for the same low slots regardless of the cap — per-slot LWW at the relay then alternates islands at the ad-refresh cadence, which the all-slots resolve (§3.3.1) already exploits. Slot-count raising helps pre-split occupancy diversity; post-split bridging is carried by Parts 1-2, especially the member-keyed refresh (no slots involved at all). Documented here so the next reader doesn't re-derive it.
- **Coverage numerator from `connected_owners`** — rejected per §3.1; it cannot see blackholes (ZEB-805).
- **A separate slow "steady-state beacon re-resolve tick"** (R1's fourth rider): subsumed. Degraded-state repair *is* the steady-state re-resolve, paced by the ladder cap (10 min) exactly while coverage is incomplete, and free when coverage is full — a standing tick would burn resolves when they provably add nothing.
- **`kick_sweep` as the parole mechanism**: `do_sweep` re-arms every non-connected peer at base — resetting live Retrying backoff ladders and stampeding dials; parole must be batch-bounded and Dormant-only.
- **True force refresh (no cooldown/semaphore)**: repair passes repeat on a ladder; an ungated refresh would hammer pkarr for members that are simply asleep. The threshold-parameterized variant keeps every abuse guard and only lowers the staleness bar.

## 7 · Testing

- **Driver unit** (existing `StubCtx`/`StubBeacons` harness + injected traffic-evidence map): verdict classification (healthy / degraded / starved / solo transitions, incl. connected-but-traffic-dark ⇒ Degraded, stale-record member excluded from E, record-less member doesn't hold Degraded); repair actions on a due Degraded pass (all hits seeded post-CR-2, only-Dormant kicks, refresh + cache-miss discovery invoked via stub seams); ladder shared across Starved↔Degraded transitions without reset; telemetry arm mapping incl. current-key-outcome preservation from ZEB-918's candidate ladder.
- **Rendezvous unit**: `resolve_rendezvous_all_slots` collects multiple verified hits across slots, dedups by node-id, counts errors/rejects, tolerance-window behavior; slot-count-8 pins (rank 4-7 now publish; publisher slot derivation).
- **Supervisor unit** (paused-clock harness): parole revives oldest record-backed Dormant peers at base rung and re-dials; batch bound respected; record-less Dormant skipped; parolee re-dormants after `dormant_after`; `maybe_refresh_stale` invoked on parole (CountingFallback); no parole interference with Retrying/Connected slots; `paroled` counter.
- **Resolver unit**: `refresh_if_older_than` threshold semantics (fresh-under-threshold no-op; cooldown still enforced; fleet-sibling still skipped; `maybe_refresh_stale` delegates unchanged — pin with existing CountingFallback pattern).
- **E2E** (`tests/pkarr_net/`, #657 harness): two publishers under one epoch key in different slots → all-slots resolve returns both hits (the bridge-both-islands shape the single-hit resolve cannot produce).

## 8 · Rollout / compatibility

- No wire, CRDT, or IPC changes. New telemetry arms/counters are additive on a diagnostics-only DTO.
- Mixed fleets: old resolvers see a subset of slots (0-3) — today's behavior; old publishers fill only 0-3 — new resolvers still find them. Old nodes lack coverage detection/parole but interoperate fully; healed sessions benefit both sides (a bridge dialed from a new node restores the mesh for old nodes too).
- Steady-state cost when Healthy: zero added (coverage math is in-memory set algebra per pass). Cost while Degraded: ≤16 pkarr GETs + cooldown-bounded refreshes per 10-min ladder cap per community, plus ~8 parole peers/hour globally.

## 9 · Out of scope

- Demoting/probing stale-`Connected` supervisor slots (the blackhole itself): the coverage numerator routes around it for detection, and QUIC idle timeout eventually drops the conn. A liveness-driven `Dropped` for dark conns is ZEB-804 follow-up territory.
- Advertiser liveness/eviction (the "griefable rank" finding, Q4): self-nomination with no service proof predates this ticket; folding service-proof into advertiser ranking is a separate design.
- Per-peer CRDT-merge attribution (the Q2 gap): `last_advance_ms` is per-engine; a per-(peer, community) merge clock would be a stronger proven signal — future hardening, not needed for v1.
- R6c deterministic partition simulation (ZEB-917) — the natural next-level test harness for this feature.
