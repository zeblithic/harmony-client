# ZEB-373 — Dynamic mid-session iroh→Zenoh dial — design

**Status:** approved 2026-06-04 (Jake)
**Parent:** ZEB-321 Phase 2 (cross-WAN connectivity). Defer from ZEB-368.
**Branch:** `zeb-373-dynamic-midsession-iroh-dial` (off `origin/main` `3a3e8b4f`, the ZEB-368 merge).
**Predecessor spec:** `docs/specs/2026-06-02-zeb-321-phase2-zenoh-over-iroh-ingestion-design.md`.

## 1. Problem

ZEB-368 made iroh a first-class Zenoh unicast transport: **inbound** ingestion (vendored
`zenoh-link` fork + forwarder into Zenoh's accept queue) and **outbound** dialing via
**static** `connect/endpoints` seeding — every iroh peer the `ReachabilityResolver` knows
at `zenoh::open`, re-seeded on each node start. Because a Zenoh transport is
**bidirectional once formed**, only one side needs to dial, so static seeding yields a
well-connected graph that heals on every reconnect.

The one uncovered case: a peer discovered **strictly mid-session** — neither side
restarts, and the new peer never dials us. Its `iroh/<hex>` is known to the resolver but
no dial is ever issued until the next node start.

zenoh 1.9.0 has no clean public API for post-`open` dial (verified, ZEB-368 spike):
`session.open_link` does not exist; runtime `insert_json5("connect/endpoints", …)` is
rejected (only `plugins/` keys writable post-start, and nothing watches the key); the live
`TransportManager` is reachable only via `Runtime::manager()`, which is `pub(crate)`.

## 2. Gate (why this ships now)

ZEB-373 was deferred behind telemetry showing static-only is insufficient. We have not yet
run the ZEB-330 two-machine validation, so no such telemetry exists. **Decision (Jake,
2026-06-04): build now AND fold in the dial telemetry**, so the work becomes its own
evidence — ZEB-330 will then show live whether mid-session dial ever fires and heals a gap.
The gate is honored by making telemetry a deliverable, not a precondition.

## 3. Approach — A: zenoh `internal` feature (chosen)

Enable zenoh's `internal` cargo feature (additive; unlocks `zenoh::internal::runtime::*`
and `zenoh::session::init` — both verified present in 1.9.0). Replace the terminal
`zenoh::open(config)` with the runtime path and retain the `Runtime` handle:

```rust
let mut runtime = zenoh::internal::runtime::RuntimeBuilder::new(config).build().await?;
runtime.start().await?;                              // Runtime::start(&mut self) — orchestrator.rs:125
let session = zenoh::session::init(runtime.clone().into()).await?;  // init(GenericRuntime); Runtime: Into<GenericRuntime>
```

On a resolver update introducing a not-yet-dialed peer, dial through the un-filtered path:

```rust
// orchestrator.rs:1052 — pub async fn connect_peer(&self, zid: &ZenohIdProto, locators: &[Locator]) -> bool
let ok = runtime.connect_peer(&fresh_placeholder_zid, &[Locator::from_str("iroh/<hex>")?]).await;
```

The placeholder `ZenohIdProto` is **fresh per dial** — zenoh uses it only for pre-dial
dedup; the real peer zid is negotiated on the wire. iroh locators stay **out** of
`connect/endpoints` (that config path filters unknown schemes); `connect_peer` is the
un-filtered route.

### Rejected — B: vendor zenoh core for a `transport_manager()` accessor

A second `[patch]` vendoring the whole zenoh core crate to expose
`session.transport_manager().open_transport_unicast(EndPoint)` gives cleaner zid-less
semantics but ~doubles the fork-maintenance tax (large multi-file crate vs. our single-file
`zenoh-link`). Not worth it for a placeholder-zid cosmetic cost.

## 4. Components

### 4.1 Session-creation swap — `event_loop.rs` (~line 651)

Keep the **entire** existing config build untouched — static `connect/endpoints` seed
(ZEB-368), `listen/endpoints` merge, iroh factory registration. Replace **only** the
terminal `zenoh::open(config)` with the three-step runtime path, still wrapped in
`cancellable!`. Retain `runtime` (a clone) for the dial driver.

This is the single highest-risk change (it reroutes the session-creation path ZEB-368 just
shipped). It is implemented first and gated on a **parity check**: a node with a plain
config + LAN + static iroh seed must open exactly as today (same listeners, same factory
invocation, same inbound ingestion) before the dial driver is added.

### 4.2 Resolver notify seam — `reachability_resolver.rs`

Add an optional `tokio::sync::mpsc::UnboundedSender<DialHint>` to `ReachabilityResolver`,
set via a setter at boot. Inside `update()`, after the existing HLC merge, if the row makes
an iroh node-id **newly active** (it was not active before this update), emit
`DialHint { node_id: [u8; 32], owner: OwnerAddr }`. Behind `Option`, so every existing
caller and test is unaffected (no hint channel → no-op).

`DialHint` carries the node-id (dedup key + locator source) and owner (telemetry/logging
only).

### 4.3 Dial driver — new module `iroh_dial_driver.rs`

`DynamicDialDriver`, spawned after session init, owns: the `Runtime` clone, a
`HashSet<[u8; 32]>` dialed-set, the self node-id, and a `DialTelemetry` handle. Loop:
receive `DialHint` → `skipped_self` if it is our node-id → `skipped_duplicate` if already
in the dialed-set → otherwise insert, build the `iroh/<hex>` `Locator`, and spawn a bounded
dial task.

Dial task: `connect_peer(fresh_zid, &[loc])` with bounded retry — **3 attempts, backoff
1s → 2s → 4s**. On success → `succeeded`. On exhaustion → `failed` **and remove the node-id
from the dialed-set**, so a later announce for the same peer re-arms a fresh dial. Never
panics; never affects the session.

### 4.4 Telemetry — fold into `network_health.rs`

Mirror the existing `PkarrFallbackHit` ring-buffer pattern. A shared `DialTelemetry`
(`Arc`) holds four counters — `attempts`, `succeeded`, `failed`, `skipped_duplicate` — plus
a bounded ring of recent `DynamicDialHit { node_id_short, owner_short, outcome, at }`.
Surface as a new field on `NetworkHealthSnapshot`, already served by the
`network_health_snapshot` IPC, so the Network Health panel shows dial activity live during
ZEB-330. Plus structured `tracing` (info on success/terminal-failure, debug on attempt/skip).

The `DialTelemetry` `Arc` lives in shared app state so both the driver (writer, on the
runtime thread) and `network_health_snapshot` (reader, in lib.rs command context) reach the
same instance — created at boot, cloned into the driver, read in the snapshot assembly.

`skipped_self` is logged at debug only (not a user-facing metric).

## 5. Data flow

```
peer B's ReachabilityAnnounce arrives over an EXISTING link (LAN, or another iroh peer)
  → CRDT materialize → resolver.update(B.node_id)            [first time active]
    → DialHint{B} on the channel
      → DynamicDialDriver: dedup-miss → connect_peer("iroh/<B-hex>")
        → zenoh invokes our factory manager's new_link() → iroh transport forms
          → bidirectional Zenoh-over-iroh sync A↔B           [gap closed]
```

## 6. Error handling & lifecycle

- **Runtime `build`/`start`/`session::init` failure** → same `ready_tx.send(Err(..))` +
  `zenoh-status: error` emit as today's `zenoh::open` failure path. No new failure surface
  reaches the caller.
- **`connect_peer` false/error** → retry-then-re-arm (4.3). Logged; session unaffected.
- **Channel closed/full** → log and continue; an unbounded sender does not block `update()`.
- **Teardown** → node-stop drops the `DialHint` sender (alongside the existing iroh-ctx
  clear) → driver task ends; `Runtime` drops with the session. Clean across restarts —
  matches the ZEB-368 ctx-swap lifecycle.

## 7. Safety argument

A transport-layer dial to a wrong, stale, or non-member peer is **inert**: community events
are authenticated against materialized membership (ZEB-339), so a mis-targeted or
over-eager dial wastes one connection attempt but can never leak or corrupt state. This is
what lets the dedup/backoff policy stay simple — no membership pre-check is needed in the
dial path.

## 8. Testing

- **Unit:** dedup (skip-self, skip-duplicate); backoff state machine (3 attempts, re-arm on
  exhaustion); resolver emits exactly one `DialHint` per newly-active node-id and **none**
  for an HLC-stale or no-op re-update.
- **Integration (two-engine, loopback iroh) — acceptance test:** engine A opens with an
  **empty** resolver (no static seed); engine B comes up; B's `ReachabilityAnnounce` is
  injected into A's community state **mid-session**; assert A dials B and a community
  state-root round-trips A↔B over Zenoh-over-iroh. Directly proves the uncovered gap is
  closed. Belongs to the known iroh-loopback flake class → gated with the ZEB-347
  warm-up-bind pattern and a generous internal timeout.
- **Telemetry:** snapshot counters increment as expected across attempt/success/dup/fail.
- **No wire-format change** → no CBOR fixture required (no serialized type changes).

## 9. Scope boundary

In scope: dial-once per node-id per session; failed dials retried-then-re-armable;
dial telemetry. **Out of scope (ZEB-321 Phase 3):** re-dial when an established transport
drops; liveness/rebinding; reconnection-after-offline. No transport-event hook is added.

## 10. Files

- `src-tauri/Cargo.toml` — add `features = ["internal"]` to the `zenoh` dep.
- `src-tauri/src/event_loop.rs` — session-creation swap; create the `DialHint` channel;
  install the sender on the resolver; spawn the dial driver with the receiver + a
  `DialTelemetry` clone.
- `src-tauri/src/reachability_resolver.rs` — optional `DialHint` sender + setter + emit on
  newly-active node-id; `DialHint` type.
- `src-tauri/src/iroh_dial_driver.rs` — **new**: `DynamicDialDriver`, dial task, dedup,
  backoff.
- `src-tauri/src/network_health.rs` — `DialTelemetry`, `DynamicDialHit`, snapshot field.
- `src-tauri/src/lib.rs` — hold the `DialTelemetry` `Arc` in shared app state; read it in
  the `network_health_snapshot` assembly.
- `src-tauri/tests/` — the two-engine mid-session-dial acceptance test.
