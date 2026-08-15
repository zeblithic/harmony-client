# R6a study — Adaptive dial ordering from per-peer performance estimators

**Date:** 2026-08-15 · **Ticket:** ZEB-915 (epic ZEB-909) · **Sources:** `harmony-client` main @ `a6e52c82`; Freenet review `docs/research/2026-08-11-freenet-architecture-review.md` §1.2/§5.
**Method:** three parallel read-only code-exploration passes over `harmony-client` (reconnect supervisor + dial scheduling; iroh path-selection/observation seam; bounded-selection audit), verified to file:line, against the documented Freenet estimator design.

---

## Verdict in one paragraph

Freenet's observed-performance routing layer **does not transplant** — not because Harmony couldn't afford it, but because the decision it optimizes does not exist here. Freenet's three isotonic estimators + kNN blend predict *which of many candidate next-hops minimizes expected time to an unknown content location* on a ring it routes through greedily over ~443 peers it does not fully know. Harmony never routes toward a location: it reconnects to **specific, known roster peers**, so "which peer is closest to the target" has no referent, and the estimator's entire scoring function (expected total time to a ring location, ×3 failure penalty) has nothing to score. What *does* survive the translation is a far humbler idea — *don't spend a scarce dial permit on a peer that keeps failing* — and that needs a per-peer failure counter, not a regression ensemble. Recommendation: **don't build the estimator; skip relay-racing (architecturally infeasible on iroh); a tiny in-memory dial-ordering tweak is defensible but evidence-gated and low priority.** The genuinely actionable output of this study is the #4222 audit, which surfaced **two real reachability-blind selection windows** (rendezvous slot assignment; R4 ring admission) that warrant their own island-aware-selection follow-ups.

**One-line answer to the ticket's ask:** *worth a smaller version at most, deferred; the estimator itself is not worth building; the audit is the part worth acting on now.*

---

## 1 · What Freenet built (the thing we're evaluating)

From the review §1.2, verified there to file:line in `freenet-core`:

- Under 50 recorded events: pure ring-distance with an untried-first exploration bias ("break death spirals where the closest peer always times out").
- Beyond 50: an **adaptive ensemble** — three isotonic-regression estimators (latency / failure-probability / throughput as monotone functions of *distance-to-target*; 500-event rolling window; per-peer EWMA corrections, α=0.1) blended with an external kNN crate (`renegade-ml`, 3-stage funnel, auto-selected k), weight ramping to a 0.5 cap. Final score = **expected total time with a 3× failure penalty**. Always on; **cold-starts on every restart** (no persistence).
- Embedded production lesson (their #4222): the routing candidate window was raised 5→25 after telemetry showed **63% of failing GETs never visited any subscriber** — the window had been hiding reachable holders.

The load-bearing detail is *distance-to-target*. Every estimator is a function of ring distance, because Freenet is choosing a next hop that carries a request **toward a content key it is trying to reach**. Performance history is a correction to the distance heuristic.

## 2 · Why the shape doesn't transplant

Harmony has no greedy-routing decision to correct. Three structural differences:

1. **The roster is known and exact.** Topology selection is informed, not statistical (the review says this explicitly for R4). Harmony dials *this specific device of this specific member*; there is no "nearest-to-key candidate set" to rank.
2. **The target is the peer, not a location reached through the peer.** Freenet scores next-hops by expected time *to the destination*; Harmony's "destination" of a reconnect *is* the peer being dialed. Expected-time-to-target collapses to expected-time-to-connect-to-X — a single number per peer, not a comparison across candidates competing to carry the same request.
3. **Path selection isn't ours.** Freenet chooses transport paths itself; Harmony delegates to iroh (§4 below). Half of what Freenet's estimator would steer, we don't control.

So the ensemble's inputs (distance), its structure (rank competing next-hops), and one of its two outputs (path choice) are all absent or externalized. Only the residual — *rank the peers we're about to dial by how likely each is to succeed, so scarce permits aren't wasted* — has a Harmony analogue. That residual is a sort key, not a model.

## 3 · Harmony baseline — the reconnect supervisor

`src/reconnect_supervisor.rs` (single pure-logic module, ~2.8k lines), wired by `src/event_loop.rs`.

**The seam exists.** Dispatch iterates the per-peer state map in **arbitrary `HashMap` order** (`reconnect_supervisor.rs:654`, over `states: Mutex<HashMap<[u8;32], PeerSlot>>` at `:246`). Peers are "due" when `Retrying { next_at <= now }`. There is **no prioritization among due peers** — the only ordering anywhere is a pairwise NodeId tie-breaker (`dial_role`, `:488`). The dial budget is a **`Semaphore(4)`** (`max_concurrent_dials = 4`, `:198`; acquired at `:692`); on exhaustion the pass `break`s. So under contention (>4 simultaneously-due peers), *which* peers get the scarce permits is decided purely by hash-seed iteration order. That contention point, plus the Dormant-parole `sort_by_key((last_fresh_trigger, peer))` at `:937` (longest-dormant-first, batch=2/900s — deterministic but not performance-informed), are the two clean insertion points an ordering key would target.

**But the inputs don't exist.** `PeerSlot` (`:211`) carries `state`, `last_fresh_trigger` (last *interest* kick, not last success), `dial_in_flight`, `epoch`, `ever_connected` (a single bool), `pending_reconnected_marker`. No success/failure counts, no RTT, no last-success timestamp; `Retrying.attempt` **resets on every fresh kick**, so it is a ladder-rung index, not even a lifetime failure tally. The only dial-outcome record anywhere is `DialTelemetry` (`network_health.rs:899`) — a **global** aggregate plus a 32-event global ring (`DIAL_RING_CAP = 32`) of 4-byte-truncated ids for the Network Health UI. It is not per-peer, not indexed, and evicts after 32 events. An estimator's entire input signal — per-peer connect-success rate, RTT-over-time, consecutive-failure counters — **would be built from scratch.**

**No persistence.** `states`/`dirty` are in-memory `Mutex<HashMap>`, rebuilt empty each boot (`event_loop.rs:1671`); on restart the map is re-seeded from live signals (`seed_boot_peers_into_supervisor`, `:1747`). All ladder/dormancy/attempt history is discarded. Any layer built here **cold-starts every restart**, exactly Freenet's named property.

## 4 · Harmony baseline — relay-vs-direct is not ours to race

The ticket's application #2 (per-peer history pre-selects the winning path) is **architecturally infeasible as framed**:

- **No control knob.** The only dial API Harmony ever calls is `endpoint.connect(EndpointAddr, alpn)` (e.g. `open_join_dial.rs:164`, and ~15 other sites). There is no `prefer_relay` / `relay_only` / `ConnectOptions` / per-connection transport preference anywhere in the tree. The only levers are an **endpoint-global** `RelayMode` (`iroh_endpoint.rs:157`, one set for all peers, `MAX_RELAYS = 8`) and per-dial `EndpointAddr` coordinate hints (`with_relay_url`/`with_direct_addresses`) that *supply* candidates without choosing among them. Selection between supplied direct addrs and relay is 100% internal to iroh's magicsock. The most we could do is *withhold* a candidate — amputation, not racing.
- **Observation is already rich** (this corrects the review's "observed read-only" as understated). `peer_liveness.rs:475` (`run_conn_path_watcher`) reads live per-connection `paths()` / `is_selected()` / `is_relay()` / `rtt()` / `path_events()`, refreshed every 30s, and folds mode+RTT into `PeerHealthRecord.connection_mode` + `rtt_ms` (`network_health.rs:2883`). The estimator's *input* is a solved problem — there just is nothing to steer with it.
- **No memory.** Those values are in-memory current-state only (`network_health.rs:1037` — "this map is in-memory only"); `reconnect_supervisor` has no path field at all. Nothing records "peer X last succeeded via direct/relay."

Net: relay-racing needs an iroh API that doesn't exist. Passive path/RTT observation could at most feed dial *ordering* or the coarse relay-URL-shipping decision — which folds into §3's seam, not a separate mechanism.

## 5 · The #4222 audit — the actionable part

Auditing every bounded peer-selection window for the Freenet failure mode (*a too-small window silently hides reachable peers*), classified **WINDOW** (bounds *which* peers are ever considered — real hazard) vs **THROTTLE** (bounds rate; every peer eventually processed — safe):

| Site | Constant | Class | #4222 risk |
|---|---|---|---|
| Rendezvous slot assignment — `community_rendezvous.rs:40` (sel. `:139`) | `RENDEZVOUS_SLOT_COUNT = 8` | **WINDOW** | **Real.** Slot = rank in an **address-sorted** advertiser list; island/reachability-blind. If the 8 lowest-address fresh advertisers land in one island after a split, every other island's advertisers rank ≥8, claim no slot, publish no beacon → "all advertisers in one island, no bridge." |
| R4 ring-neighbor admission — `community_topology.rs:37` (enf. `reconnect_supervisor.rs:670`, gate `event_loop.rs:1701`) | `FULL_MESH_THRESHOLD = 32`, degree ~2·log₂N | **WINDOW (router-mode only)** | **Real but scoped.** Ring neighbors are pure blake3-identity hash distance — reachability-blind. A reachable non-neighbor that is the *only* bridge to an island is parked Dormant and never dialed. Router-mode only (peer-mode admits all); softened by fail-open-on-unknown (`admission_oracle.rs:71`) + whole-set anti-islanding fallback (`:127`). |
| Relay read cap — `community_relay_announce.rs:24` (sel. `community_relay_resolver.rs:95`) | `COMMUNITY_RELAY_ADVERTISERS_MAX = 4` | WINDOW (bounded) | Lower risk — recency-ranked service fan-out, and discovery breadth (8) is decoupled from and wider than this service cap. |
| Outbound dial concurrency — `reconnect_supervisor.rs:198` | `max_concurrent_dials = 4` | THROTTLE | Deferred peers re-picked next pass. |
| Dormant parole batch — `reconnect_supervisor.rs:202` | `parole_batch = 2` | THROTTLE | Longest-dormant-first rotation; all eventually paroled. |
| Inbound handshake gate — `inflight_handshake_gate.rs:29,35` | `PER_SOURCE = 8`, `GLOBAL = 1024` | THROTTLE | Permit released post-handshake; shed source re-dials. |
| Content GET — `event_loop.rs:2030` | `Locality::Any`, `ROOT_FETCH_SPILL_MAX` | THROTTLE/safe | No bounded holder set; Zenoh fans to all queryables, first valid reply wins. |
| Observed-holder map — `observed_holders.rs:28` | `MAX_HOLDERS_PER_CID = 32` | watch | Window-*shaped* but not on the fetch path today (observability only). Becomes a live hazard if any future fetch selects holders *from* this capped map. |

**ZEB-910 (R1) closed the read side, not the selection side.** `resolve_rendezvous_all_slots` (`community_rendezvous.rs:410`) now scans *every* slot across both epoch windows and returns *every* distinct verified beacon ("repair passes need every reachable island's beacon, not the first one"). But *who occupies* the bounded slots is still an address sort with zero island/region/subnet awareness. Discovery **breadth** improved; advertiser **diversity** did not. The two WINDOW sites above select on identity/recency, never on reachability — the same blind spot R1 was created to attack, one layer down.

## 6 · Verdicts per application

- **Full observed-performance estimator (isotonic + kNN + throughput/EWMA): NOT WORTH IT.** Wrong scale (≤ low-hundreds known roster vs ~443 unknown greedy-routed peers) *and* wrong problem shape (§2: no distance-to-target decision to correct). The mechanism has no referent here.
- **Relay-vs-direct racing: NOT WORTH IT / infeasible.** iroh owns path selection; no control knob exists (§4). Revisit only if iroh ever exposes a per-connection path-preference API.
- **A tiny dial-ordering tweak: WORTH A SMALLER VERSION, but deferred and evidence-gated.** The seam is real (§3), but it only bites under dial-budget contention (>4 simultaneously-due peers) — rare at current scale, where jittered exponential backoff already spreads due-times. If ever built, the right shape is: lightweight per-peer counters (consecutive-failure count + last-success recency) used as a **secondary sort key** at the two seams (the semaphore-contention dispatch point and the parole `sort_by_key`) so chronic failures don't crowd out fresh/likely peers — explicitly **not** a regression model. Build it only when telemetry shows actual contention-induced starvation; until then the arbitrary order is harmless because the budget is rarely saturated.
- **The #4222 audit: ACT ON IT.** Two reachability-blind WINDOW sites (rendezvous slot selection; R4 ring admission) are the study's real find and warrant island-aware-selection follow-ups (§7).

## 7 · The persistence question (explicitly asked)

Freenet's layer cold-starts every restart and accepts it; §3 shows Harmony's supervisor is likewise in-memory only. **If a smaller dial-ordering version is ever built, keep it in-memory too — do not add persistence.** Reconnect-quality history is most valuable *fresh* (right after a wake / network change), and a cold start merely means the first few post-boot ticks fall back to backoff-only ordering — benign, and boot already re-dials everyone. Persistence would add a serialization surface and a staleness question (a peer's connectivity a day ago is weak evidence now) for a payoff that only exists under an over-budget boot storm we don't observe. Persist only if evidence later shows restart-churn starvation — unlikely.

## 8 · Recommended follow-ups (for triage, not filed by this study)

1. **Island/diversity-aware rendezvous slot selection** (R1-adjacent, ZEB-910 follow-up). Replace the pure address-sort slot assignment (`community_rendezvous.rs:139`) with a selection that reserves slots across a diversity axis (subnet/region/reachability-cohort) so one island cannot monopolize all 8 advertiser slots. Highest-value item here — it directly attacks the split-bridge failure R1 targets.
2. **Reachability-aware override on R4 ring admission** (router-mode; gated with ZEB-931 router-mode enablement). Let a proven-reachable non-neighbor that is the sole bridge to an otherwise-unreachable subset escape the Dormant park (`reconnect_supervisor.rs:670`). Low urgency — router mode is opt-in and off by default.
3. **Watch note on `observed_holders` (`observed_holders.rs:28`)** — leave as-is, but any future content-fetch path that selects holders *from* this 32-cap map must not treat the cap as a candidate window.
4. **(Deferred) tiny dial-ordering key** — per §6, only when contention-starvation is observed. Not worth a ticket until then.

## 9 · Coverage & sources

Three read-only passes over `harmony-client` main: (a) `reconnect_supervisor.rs` + `event_loop.rs` scheduling; (b) the iroh dial/observe seam across `iroh_endpoint.rs`, `peer_liveness.rs`, `network_health.rs`, `connectivity_settings.rs`, and ~15 `connect()` call sites; (c) bounded-selection audit across `community_rendezvous.rs`, `community_relay_*`, `community_topology.rs`, `admission_oracle.rs`, `inflight_handshake_gate.rs`, `observed_holders.rs`, `event_loop.rs` content GET. Freenet estimator design taken from the 2026-08-11 review §1.2/§5 (verified there to `freenet-core` file:line). Not re-verified: `freenet-core`'s estimator internals (trusted from the prior review); Harmony frontend has no role in dial ordering (not examined).
