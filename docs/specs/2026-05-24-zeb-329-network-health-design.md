# ZEB-329 — Network Health: cross-WAN validation surface

**Status:** Design approved 2026-05-24

**Parent:** [ZEB-327](https://linear.app/zeblith/issue/ZEB-327) Sub-project B

**Related:**
- [ZEB-321](https://linear.app/zeblith/issue/ZEB-321) Phase 1 (b082e66) — ReachabilityResolver, ReachabilityPublisher, connectivity IPCs, DiagnosticsPanel (read-only dev mode)
- [ZEB-321](https://linear.app/zeblith/issue/ZEB-321) Phase 2 (cb5cca5, 3c4c21d, c5c4da9) — pkarr-based discovery + invite handshake
- [ZEB-172](https://linear.app/zeblith/issue/ZEB-172) Track D — in-app diagnostics goal subsumed by this work
- Sub-project A ([ZEB-328](https://linear.app/zeblith/issue/ZEB-328), merged 9844170) — distribution pipeline + auto-updater

**Subsumes:** [ZEB-172](https://linear.app/zeblith/issue/ZEB-172) Track D's in-app diagnostics deliverable

---

## 1. Goal

When a tester reports "I can't reach anyone" or "messages aren't delivering across WAN," give them an in-app surface that answers the question without a developer-side debug session for every issue. The surface must:

1. Communicate reachability in plain language (with raw technical detail one hover/expand away for the technically curious)
2. Surface per-peer state (mode, RTT, last-seen) scoped to the tester's actual social graph
3. Offer a one-click self-test that distinguishes "my side is broken" from "this peer is unreachable"
4. Produce a redacted-by-default diagnostic export the tester reviews before sharing
5. Anchor a two-host validation playbook that operators can follow to prove the connectivity stack works across real NATs

The existing `DiagnosticsPanel.svelte` (Phase 1, dev-mode-only) is the raw-data layer — `NodeId` hex, relay URL, fallback events. This spec adds the **synthesized** layer on top, for testers, not developers.

## 2. Non-goals

- Continuous push of RTT/jitter telemetry — episodic snapshot is sufficient
- Direct GitHub Issue submission — copy/save lets the tester paste anywhere
- Central echo service or any operator-hosted infrastructure — violates polycentric
- Telemetry, crash reporting, or any backend call-home — violates self-sovereign
- Mobile push / liveness / rebinding — that is [ZEB-321](https://linear.app/zeblith/issue/ZEB-321) Phase 3
- Relay governance / federated DERPs — that is [ZEB-321](https://linear.app/zeblith/issue/ZEB-321) Phase 5+

## 3. Architecture

```text
┌───────────────────────────────────────────────────────────────────────┐
│  Frontend (Svelte 5)                                                  │
│  ┌─────────────────────────────────┐    ┌───────────────────────────┐ │
│  │ NetworkHealthView.svelte        │    │ DiagnosticExportModal     │ │
│  │ (new route: /network)           │    │ .svelte                   │ │
│  │                                 │    │                           │ │
│  │   - your-network summary card   │    │   - rendered markdown     │ │
│  │   - peer list (community-scoped)│    │   - redaction toggle      │ │
│  │   - "Run self-test" button      │    │   - copy / save buttons   │ │
│  │   - "Submit diagnostics" button │────│                           │ │
│  └─────────────────────────────────┘    └───────────────────────────┘ │
│                  │                                  │                  │
│  ┌───────────────┴──────────────────────────────────┴──────────────┐  │
│  │ network-health-adapter.ts                                       │  │
│  │  - snapshot() / runSelfTest() / exportPayload()                 │  │
│  │  - onNetworkHealthChanged(cb)  ← Tauri event subscriber         │  │
│  │  - plain-language NAT translation (pure TS, testable)           │  │
│  │  - redaction helper (pure TS, testable)                         │  │
│  └─────────────────────────────────────────────────────────────────┘  │
└──────────────────────────────────┬────────────────────────────────────┘
                                   │ Tauri IPC + events
┌──────────────────────────────────┴────────────────────────────────────┐
│  Backend (Rust, harmony-app crate)                                    │
│  ┌─────────────────────────────────────────────────────────────────┐  │
│  │ network_health.rs (NEW — ~600 LOC + tests)                      │  │
│  │                                                                 │  │
│  │  NetworkHealthService                                           │  │
│  │   ├─ snapshot() -> NetworkHealthSnapshot                        │  │
│  │   ├─ run_self_test() -> SelfTestReport                          │  │
│  │   └─ emit_change_event(app_handle) (called by event_loop hook)  │  │
│  │                                                                 │  │
│  │  Pure functions (testable in isolation):                        │  │
│  │   - classify_nat(conn_info) -> NatClass                         │  │
│  │   - derive_reachability_status(...)                             │  │
│  │   - filter_peers_by_shared_membership(...)                      │  │
│  │   - format_export_markdown(snapshot, self_test, include_full)   │  │
│  └─────────────┬───────────────────────────────────────────────────┘  │
│                │ reads from (no writes — synthesis only)              │
│                ▼                                                      │
│  ┌─────────────────────────────────────────────────────────────────┐  │
│  │ Existing sources (no change to public APIs)                     │  │
│  │  - iroh::Endpoint   (connection_info, home_relay, direct_addrs) │  │
│  │  - ReachabilityResolver  (peer reachability records)            │  │
│  │  - pkarr_*_publisher  (publication freshness, fallback log)     │  │
│  │  - community_membership   (my membership set → peer scope)      │  │
│  │  - identity              (for self-test pkarr round-trip)       │  │
│  └─────────────────────────────────────────────────────────────────┘  │
└───────────────────────────────────────────────────────────────────────┘
```

### Three Tauri IPCs

All snake_case Rust ↔ camelCase JS, all read-only (no preview/commit pairs, no TOCTOU):

| Rust name | Returns | Purpose |
|---|---|---|
| `network_health_snapshot` | `NetworkHealthSnapshot` | Read current synthesis |
| `network_health_run_self_test` | `SelfTestReport` | Run + cache self-test |
| `network_health_export_payload` | `String` (markdown) | Build export with redaction |

### One Tauri event

`network-health-changed` — emitted from backend at most **once every 2s** (rate-limit, not classical debounce) when the resolver update hook fires for any reason (add, remove, refresh). The snapshot refetch is cheap; the frontend listener stays naive and re-fetches on every event. Connection-mode flips (direct↔relay) and stale crossover are surfaced on the next snapshot refetch rather than driving their own events — keeps the event surface single-source.

### Key principle

`network_health.rs` is **synthesis only** — reads from sources, never writes to them. The Phase 1/2 connectivity IPCs stay intact; this layer is additive. Pure functions (classification, scope filter, export formatter) decompose for direct unit testing.

## 4. Components & data types

### 4.1 Backend — `src-tauri/src/network_health.rs` (NEW)

```rust
pub struct NetworkHealthSnapshot {
    pub schema_version: u32,           // 1; bump on breaking export-format changes
    pub captured_at_ms: u64,           // wall-clock at snapshot time
    pub app_version: String,           // env!("CARGO_PKG_VERSION")
    pub platform: String,              // "darwin/aarch64" etc.
    pub my_network: Option<MyNetworkSummary>,  // None if iroh not yet bound
    pub peers: Vec<PeerHealth>,        // community-scoped; sorted by last_seen desc
    pub pkarr_status: PkarrHealthSummary,
}

pub struct MyNetworkSummary {
    pub iroh_node_id: String,          // hex; full string — frontend redacts
    pub reachability: ReachabilityStatus, // Reachable | Degraded | Unreachable
    pub nat_classification: NatClass,
    pub home_relay_url: Option<String>,
    pub relay_rtt_ms: Option<u32>,     // None if relay unreachable
    pub direct_addresses: Vec<String>, // IP:port strings
}

pub struct PeerHealth {
    pub owner_addr: String,            // full Ed25519 hex
    pub display_name: Option<String>,  // from profile cache if available
    pub shared_communities: Vec<String>, // community IDs we share with this peer
    pub connection_mode: ConnectionMode, // Direct | Relay | NoConnection
    pub rtt_ms: Option<u32>,           // current measured RTT, None if no conn
    pub last_seen_ms: Option<u64>,     // last successful exchange
    pub reachability_record_age_ms: Option<u64>, // freshness of the CRDT record
}

pub struct PkarrHealthSummary {
    pub identity_published: bool,
    pub identity_last_publish_ms: Option<u64>,
    pub community_publish_count: u32,
    pub recent_fallback_events: Vec<PkarrFallbackHit>, // last 5, ordered newest-first
}

pub struct PkarrFallbackHit {
    pub peer_addr_short: String,       // first 8 chars + ellipsis
    pub community_id_short: String,    // first 8 chars + ellipsis
    pub hit: bool,                     // true = found via fallback, false = miss
    pub captured_at_ms: u64,
}

pub enum ReachabilityStatus { Reachable, Degraded, Unreachable }
pub enum NatClass {
    FullCone,
    RestrictedCone,
    PortRestricted,
    Symmetric,
    Unknown,
}
pub enum ConnectionMode { Direct, Relay, NoConnection }

pub struct SelfTestReport {
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    pub steps: Vec<SelfTestStep>,
    pub peer_results: Vec<PeerPingResult>,
}

pub struct SelfTestStep {
    pub name: String,   // "endpoint", "relay", "pkarr_publish", "pkarr_resolve"
    pub outcome: StepOutcome,
}

pub struct PeerPingResult {
    pub owner_addr: String,
    pub outcome: StepOutcome,
    pub mode: Option<ConnectionMode>,  // populated on Pass
}

pub enum StepOutcome {
    Pass { duration_ms: u32 },
    Fail { reason: String },
    Skipped { reason: String },
}
```

#### Pure functions (testable without iroh / network)

- `classify_nat(connection_info: &iroh::ConnectionInfo) -> NatClass` — wraps iroh's own probe output into our enum
- `derive_reachability_status(my: &MyNetworkSummary, peers: &[PeerHealth]) -> ReachabilityStatus`
- `filter_peers_by_shared_membership(resolver_records, my_memberships) -> Vec<PeerHealth>`
- `format_export_markdown(snapshot, self_test, include_full_ids) -> String`

### 4.2 Frontend

- `src/lib/types/network-health.ts` — TS mirrors of the Rust structs (camelCase via Tauri convention)
- `src/lib/network-health-adapter.ts` — IPC wrappers + event subscriber + pure helpers (`explainNatClass`, `redactAddr`)
- `src/lib/components/NetworkHealthView.svelte` — dedicated `/network` route. Owns snapshot fetch, subscribes to `network-health-changed`, renders summary + peer list. Triggers self-test on click; renders results inline.
- `src/lib/components/DiagnosticExportModal.svelte` — opens on "Submit diagnostics". Calls `exportPayload(false)` initially; toggle re-calls with `true`. Buttons: Copy (writes to `navigator.clipboard`), Save (uses `@tauri-apps/plugin-dialog` save dialog), Cancel.

### 4.3 Nav integration

Add a "Network" sidebar item routing to `/network`. Exact placement determined at implementation time (matches existing sidebar conventions).

### 4.4 Invariants

- **`schema_version` on the snapshot is load-bearing.** If we ever change the export format, the version bumps so old exported reports remain interpretable. Defaults to `1`; first format change → `2`.
- **No new wire-format CRDT events.** This entire feature reads existing data; no events get published. No fixture-pinning tests, no community-membership coupling beyond reading my own.
- **Identity portability invariant unaffected.** Doesn't touch keychain entries or bundle ID.
- **Synthesis-only contract.** `network_health.rs` never mutates the sources it reads from. Tested by code review; not enforced by the type system.

## 5. Data flow

### 5.1 Page load (cold)

```text
User navigates to /network
    ↓
NetworkHealthView.svelte onMount
    ↓
adapter.snapshot()  ── IPC ──▶  network_health_snapshot
                                       │
                                       ├─ read iroh::Endpoint.connection_info()
                                       ├─ read ReachabilityResolver.list()
                                       ├─ read my membership set (AppState)
                                       ├─ filter_peers_by_shared_membership(...)
                                       ├─ read PkarrPublicationStatus
                                       └─ assemble NetworkHealthSnapshot
                                                │
    ◀────── snapshot ──────────────────────────┘
    ↓
Render summary card + peer list
    ↓
adapter.onNetworkHealthChanged(refresh)  // subscribe before first paint
```

Typical wall-clock: <50 ms (all in-process reads, no network).

### 5.2 Event-driven refresh

`event_loop.rs` already calls `reachability_resolver.update(...)` on incoming `kd=rch` CRDT events. We add ONE hook in the existing handler:

```rust
  reachability_resolver.update(actor, payload, hlc);
+ network_health_change_debouncer.notify(NotifyReason::ReachabilityChanged);
```

The rate-limiter (in `network_health.rs`) emits at most one Tauri event every 2s:

```text
rate-limiter task → app_handle.emit("network-health-changed", ()) → listener → snapshot() refetch
```

**Why rate-limit on the backend, not the frontend:** during membership churn (e.g., joining a community, 30 reachability records arriving in one burst) we want one refetch, not 30. The frontend listener stays naive — fires `snapshot()` on every event.

**Single trigger source:** the existing `reachability_resolver.update(...)` call site is the only place that calls `notify()`. We do not try to classify which resolver changes are "significant"; the snapshot is cheap, so a refetch is always OK. Connection-mode flips and stale-crossover are surfaced on the next refetch (driven by the resolver event or by a user reopening the panel) rather than as their own event sources.

### 5.3 Self-test

```text
User clicks "Run self-test"
    ↓
View calls adapter.runSelfTest()  ── IPC ──▶  network_health_run_self_test
                                                      │
                                                      ├─ Step: endpoint up?
                                                      ├─ Step: home relay reachable?
                                                      ├─ Step: pkarr publish
                                                      ├─ Step: pkarr resolve self
                                                      │    (verify payload matches what we
                                                      │     just published)
                                                      └─ For each scoped peer (parallel, 5s timeout):
                                                           open Iroh bi-stream with
                                                           HARMONY_PING_V1 ALPN, write 1 byte,
                                                           read 1 byte echo, record RTT
                                                                │
    ◀────── SelfTestReport ──────────────────────────────────────┘
```

#### New ALPN: `harmony/ping/v1`

Tiny accept handler in `iroh_endpoint.rs` echoes one byte and closes. Used only by self-test. Documented as "self-test only — produces no app-level state."

#### Self-test bounding

- Peers list already community-scoped (capped at the same set the panel shows)
- For very large communities, cap parallel pings at **32** (semaphore) — a 10k-member community must not open 10k QUIC streams at once
- Ping timeout 5s per peer
- Concurrent self-test invocations: second invocation ignored until first completes (frontend disables the button + shows spinner). Backend doesn't need a lock — the disabled button is the lock.

### 5.4 Diagnostic export

```text
User clicks "Submit diagnostics"
    ↓
DiagnosticExportModal opens
    ↓
adapter.exportPayload(includeFullIds=false)  ── IPC ──▶  network_health_export_payload
                                                                  │
                                                                  ├─ take_snapshot()
                                                                  ├─ get_cached_last_self_test()
                                                                  │    (None if never run)
                                                                  └─ format_export_markdown(
                                                                       snapshot, self_test,
                                                                       include_full_ids)
                                                                          │
    ◀────── markdown string ────────────────────────────────────────────────┘
    ↓
Render in <pre> with monospace
    ↓
User toggles "Include full identifiers"
    ↓
adapter.exportPayload(true)  → re-render
    ↓
User clicks Copy or Save
    ↓
Copy: navigator.clipboard.writeText(markdown)
Save: invoke @tauri-apps/plugin-dialog save() → write file
```

#### Redaction is server-side

The `include_full_ids=false` branch in `format_export_markdown` is the only place that emits identifier prefixes. Frontend doesn't get the full IDs unless the toggle is on. **A screenshot of the modal in redacted mode genuinely contains no full identifiers** — no "data is in the DOM but hidden by CSS" footgun.

#### Last self-test cache

Stored in `AppState` as `Arc<RwLock<Option<SelfTestReport>>>`, populated by `network_health_run_self_test`, read by export. Lives for the session. Not persisted. If user exports without running a self-test, the export simply omits that section (no boilerplate "self-test: not run").

### 5.5 State coupling summary

```rust
NetworkHealthService holds:
  - Arc<IrohEndpoint>          (from AppState, already there)
  - ReachabilityResolver       (from AppState, already there)
  - Arc<RwLock<Option<SelfTestReport>>>  (NEW — cached for export)
  - rate-limiter task handle   (NEW — spawned at boot, lives for app lifetime)

AppState gets one new field:
  pub network_health: Option<Arc<NetworkHealthService>>,

Boot wiring (in lib.rs setup hook):
  let nh = NetworkHealthService::new(
      iroh_endpoint.clone(),
      reachability_resolver.clone(),
      pkarr_publisher.clone(),
      app_handle.clone(),
  );
  guard.network_health = Some(Arc::new(nh));
```

## 6. Error handling

The user is a tester, often debugging the very system they're using. Error UX has to be *informative*, not just "something went wrong."

### 6.1 Backend errors

**`network_health_snapshot` never fails.** If iroh isn't ready, the endpoint hasn't bound, or the resolver is empty, the snapshot fields just go `None` / empty. An empty-but-well-formed snapshot always renders.

```rust
pub async fn network_health_snapshot(
    state: tauri::State<'_, AppStateGuard>,
) -> Result<NetworkHealthSnapshot, String> {
    let guard = state.lock().await;
    let snapshot = match guard.network_health.as_ref() {
        Some(svc) => svc.snapshot().await,
        None => NetworkHealthSnapshot::empty(),
    };
    Ok(snapshot)
}
```

**`network_health_run_self_test` returns `Result<SelfTestReport, String>`** — fails only on truly exceptional cases (AppState lock poisoned, panic in spawn). Step failures (relay down, pkarr timeout, peer unreachable) are *outcomes inside the report*, not IPC errors.

**`network_health_export_payload`** is effectively infallible (depends on snapshot, which never fails). Returns `Result<String, String>` to match Tauri convention; the error branch is dead code for future-proofing.

### 6.2 Self-test step error semantics

Every step has three outcomes:

| Outcome | Render | Meaning |
|---|---|---|
| `Pass { duration_ms }` | ✓ green | Worked; duration shown |
| `Fail { reason }` | ✗ red, reason on hover | Tried, didn't work; reason is human-readable |
| `Skipped { reason }` | ⊘ grey, reason on hover | Couldn't try (precondition missing) |

**Why `Skipped` exists:** if iroh endpoint isn't up, all four endpoint-dependent steps must be `Skipped`, not `Fail` — otherwise testers see "4 things failed!" when really one root cause cascaded. Backend orders steps and short-circuits to `Skipped` for downstream steps when an upstream prerequisite failed.

**Reason strings are bounded** — fixed set of human-readable strings:

- `"endpoint not bound"`
- `"relay timeout after 5s"`
- `"pkarr publish failed: <bounded category>"`
- `"pkarr resolved unexpected payload"`
- `"peer reachability record stale (Nd)"`
- `"timeout"`
- `"no reachability record"`

Not raw Rust error chains. The export carries these strings verbatim; tester pastes them, we know exactly what they mean.

### 6.3 Frontend errors

**Network panel itself never shows a top-level "error" banner.** Snapshot is always well-formed; partial data renders as partial data. If the IPC genuinely throws (Tauri layer failure, app shutting down), we render a single line:

```text
⚠ Diagnostics unavailable — try restarting Harmony.  [Retry]
```

**Self-test errors** display per-step inline. If `runSelfTest()` IPC itself rejects (rare), render: `Self-test couldn't start: <error message>  [Retry]` using `e instanceof Error ? e.message : String(e)` per the Tauri error extraction rule.

**Export modal errors:**
- Snapshot fetch fails → modal shows "Couldn't gather diagnostics. Try again." with retry.
- Clipboard write fails (browser permissions etc.) → toast: "Couldn't copy. Use Save instead."
- Save dialog cancelled by user → no error (silent dismiss).
- Save dialog write fails → toast with the OS error message.

### 6.4 Edge cases the implementation must handle

1. **Tester opens panel before iroh has bound.** Snapshot returns empty `my_network` (None). View renders: "Network is starting up… (this can take 10–30 seconds on first launch)" with auto-retry every 2s for 30s, then a manual retry button.
2. **Tester runs self-test while offline.** Endpoint up but no network. Steps cascade: endpoint ✓, relay ✗ ("timeout"), pkarr_publish ⊘ ("skipped: relay unreachable"), pkarr_resolve ⊘, all peer pings ⊘. Report renders honestly: 1/4 local checks passed, 0 peers reached.
3. **Tester runs self-test twice in rapid succession.** Second invocation is ignored until first completes (frontend disables the button + shows spinner). Backend doesn't need a lock — the disabled button is the lock.
4. **Tester exports with no self-test yet.** Export markdown omits the self-test section entirely; doesn't render "self-test: not run" boilerplate that would look like a failure.
5. **Pkarr publisher not initialized yet.** `pkarr_status.identity_published = false`, `last_publish_ms = None`. Snapshot renders pkarr summary as "Discovery service still starting…" — same gentle treatment as the iroh-not-ready path.
6. **Peer in shared community but no reachability record yet.** Appears in peer list with `connection_mode: NoConnection`, RTT `None`, last_seen `None`. Rendered as: dim peer name — "no reachability info yet". Honest about what we don't know.
7. **Reachability record present but >7 days stale.** Rendered with warning icon: `⚠ alice@… — record from 8d ago, may not be reachable`. Self-test would `Skip` pinging.

### 6.5 What we explicitly do NOT do

- No retries inside the snapshot IPC — synchronous read of in-memory state; retrying won't help. Frontend's manual retry suffices.
- No telemetry on errors — no call-home. Per polycentric/self-sovereign rules.
- No automatic export submission on errors — even a "report this" prompt would be telemetry-like. Kept entirely user-initiated.

## 7. UX presentation

### 7.1 Top-level reachability indicator

Plain-language + raw on hover (default mode):

```text
✓ Direct connections work          …
  Most peers reach you without a
  relay. Best speed.

Relay: use1.derp.iroh.network
  RTT: 24ms
```

Hover on the `…`: `NAT classification: full-cone (open NAT — peers can connect directly).`

### 7.2 Per-peer rows

Compact one-line format:

```text
✓ alice@…    direct   18ms   3s ago
⚠ bob@…      relay    97ms   2m ago
✗ carol@…    timeout                 (no reachability record)
⚠ dave@…     timeout                 (record stale, last-seen 4d)
```

Click on a row → inline expand showing `iroh_node_id`, shared communities, `reachability_record_age_ms`, raw addresses.

### 7.3 Self-test results pane

Replaces the "Run self-test" button while running + after completion:

```text
Running self-test…
  ✓ Iroh endpoint listening              (12ms)
  ✓ Home relay reachable                 (24ms)
  ✓ Published own identity to DHT        (380ms)
  ✓ Resolved own identity from DHT       (210ms)
  ──────────────────────────────────────────────
  Peers (2 reachable / 12 known)
  ✓ alice@…   direct  18ms
  ✓ bob@…     relay   97ms
  ✗ carol@…   timeout (no reachability record)
  ⚠ dave@…    timeout (record stale, last-seen 4d)

All local checks passed. 2 of 12 known peers reached.

[Run again]
```

### 7.4 Diagnostic export modal

```text
┌─ Diagnostic export ──────────────────────────────┐
│ Review what you're about to share:               │
│ ──────────────────────────────────────────────── │
│ ## Harmony v0.1.0-alpha.3 (darwin/aarch64)        │
│ ## Network: ✓ reachable                          │
│ NAT: full-cone   Relay: use1.derp.iroh.network    │
│ RTT to relay: 24ms                                │
│                                                    │
│ ## Self-test (2026-05-24T17:32Z)                  │
│ ✓ endpoint  ✓ relay  ✓ pkarr round-trip          │
│ Reached 2 of 12 known peers                       │
│                                                    │
│ ## Peers                                          │
│ a3f9e1c2… direct 18ms (3s ago)                    │
│ b7d2884a… relay  97ms (2m ago)                    │
│ ──────────────────────────────────────────────── │
│ [ ] Include full identifiers (default off)        │
│ [Copy]  [Save as .txt]  [Cancel]                  │
└──────────────────────────────────────────────────┘
```

## 8. Testing

### 8.1 Backend unit tests (`#[cfg(test)] mod tests` in `network_health.rs`)

Pure-function tests (no iroh, no network — the bulk):

- `classify_nat`: each variant of iroh's NAT probe output → expected `NatClass`. ~6 cases.
- `derive_reachability_status`: 3 inputs producing each of `Reachable / Degraded / Unreachable`.
- `filter_peers_by_shared_membership`:
  - empty membership set → empty peer list (even if resolver has records)
  - peer in 0 of my communities → excluded
  - peer in 1 of my communities → included, `shared_communities.len() == 1`
  - peer in N of my communities → `shared_communities.len() == N`, deduped
  - sort order: `last_seen_ms` desc, `None` values last
- `format_export_markdown`:
  - `include_full_ids=false` → no full Ed25519 hex anywhere in output (regex assertion)
  - `include_full_ids=true` → full identifiers present
  - missing self-test → section omitted, no "not run" boilerplate
  - empty peer list → "no peers" line, not a header with empty body
  - schema version present in output

Stateful tests (use a fake `IrohEndpoint` trait — small trait extraction so we can swap):

- `snapshot()` with iroh-not-ready → returns `NetworkHealthSnapshot::empty()`, no panic
- `snapshot()` with iroh ready + empty resolver → `my_network: Some`, `peers: []`
- `snapshot()` with iroh ready + 3 peers in shared communities → all three appear, sorted correctly
- Rate-limiter: 30 `notify()` calls in <2s → exactly 1 emit. 30 `notify()` calls spaced 1s apart over 30s → 15 emits (one per 2s window). No emit fires if zero `notify()` calls arrive in a 2s window.

Self-test behavior tests:

- All steps pass path → `SelfTestReport` with 4 `Pass`, peer pings populated
- Relay down → relay step `Fail`, downstream pkarr steps `Skipped`, peer pings still attempted (use direct addresses)
- Endpoint not bound → all steps `Skipped`, peer pings `Skipped`
- Pkarr resolve returns mismatched payload → `Fail { reason: "pkarr resolved unexpected payload" }`
- Peer ping timeout → `PeerPingResult { outcome: Fail, mode: None }`, doesn't block other peers
- Concurrent peer pings: 100 peers, semaphore cap 32 → max-in-flight observed ≤ 32

### 8.2 Backend integration test (`src-tauri/tests/network_health_two_endpoint.rs`)

One test, real iroh, two `IrohEndpoint` instances in the same process:

1. Endpoint A registers the `harmony/ping/v1` accept handler
2. Endpoint B issues a self-test ping to A's NodeId
3. Assert: `SelfTestReport.peer_results[0].outcome == Pass`, `mode == Direct` (loopback), duration < 1s

Exercises the actual ALPN handler + bi-stream + RTT measurement end-to-end. Mirrors the Phase 1 two-endpoint pattern in `pkarr_iroh_redeem_full_integration.rs`.

### 8.3 Frontend unit tests (`src/lib/__tests__/network-health-adapter.test.ts`)

- `explainNatClass`: each `NatClass` value → returns non-empty `headline` and `detail`. 6 cases.
- `redactAddr`:
  - `full=true` → returns full hex unchanged
  - `full=false` → returns first 8 chars + `…`
  - empty/short addr → returns `(unknown)`, doesn't crash

### 8.4 Frontend component tests (`src/lib/components/__tests__/`)

`NetworkHealthView.test.ts`:
- Renders "starting up…" when `snapshot.my_network` is `None`
- Renders summary card when `snapshot.my_network` is populated
- Renders empty-peer state when `peers: []`
- Renders peer list sorted by `last_seen`
- Self-test button disabled while in-flight
- Self-test results render `Pass`/`Fail`/`Skipped` with correct icons + reason on hover
- `onNetworkHealthChanged` event triggers `snapshot()` refetch (mock IPC)

`DiagnosticExportModal.test.ts`:
- Renders redacted markdown by default (assert no full Ed25519 hex in DOM)
- Toggle "Include full identifiers" → re-renders with full IDs
- Copy button calls `navigator.clipboard.writeText(...)`
- Save button calls the Tauri dialog save IPC (mocked)
- Cancel button closes without side effects

All component tests use Svelte 5 runes + vitest (matches `DiagnosticsPanel.test.ts` pattern).

### 8.5 Manual validation — two-host playbook (`docs/cross-wan-validation.md` NEW)

~250 lines of markdown. Structure:

```markdown
# Cross-WAN validation playbook

## What you need
- Two machines on different networks (home + coffee shop, two friends, etc.)
- Both running Harmony v0.1.0-alpha-N
- One out-of-band channel (Signal, SMS) to exchange a harmony:// invite URL

## Step 1: Baseline (single-machine sanity)
On EACH machine independently:
1. Launch Harmony
2. Open Network panel
3. Wait until "Reachable" green dot appears (typically <30s)
4. Click "Run self-test". Expect: endpoint ✓ relay ✓ pkarr ✓ pkarr-resolve ✓
5. Screenshot the panel

If a machine fails Step 1, the playbook can't proceed — file a tester-feedback
report with the export from Step 1's Network panel.

## Step 2: First contact
On machine A:
1. Create a community ("test-cross-wan-YYYYMMDD")
2. Generate invite URL, paste into your out-of-band channel

On machine B:
1. Click the harmony:// URL from machine A
2. Confirm the join dialog
3. After "Joined" toast, return to Network panel
4. Expect: peer A appears in the list within 60s

## Step 3: Exchange
1. On machine A: send a DM "hello from A"
2. On machine B: confirm receipt
3. Reverse: B → A
4. Network panel on both machines should now show the other peer with:
   - last_seen within seconds
   - either "direct" or "relay" mode (note which)
   - RTT measured

## Step 4: Export
Both machines: Submit diagnostics → Save as .txt → attach both reports to
tester-feedback issue along with: "successful Step 3 cross-WAN exchange" or
the step number where you got stuck.

## Troubleshooting cheatsheet
[table: symptom → likely cause → next step]
```

### 8.6 Deliberate exclusions

- **No live DHT integration tests.** Mainline DHT is slow + flaky; not worth the CI minutes. Pkarr publish/resolve in self-test uses the local `pkarr_publisher`'s internal hook, not a real DHT round-trip in tests.
- **No NAT-type integration tests.** Iroh's NAT probe needs real network egress; we trust iroh's own tests and only test our classification wrapper.
- **No UI snapshot tests.** Snapshot diffs rot fast for evolving UIs; behavior tests (rendered text content, event firing) catch real regressions without the noise.

### 8.7 Wire-format pinning

**None needed.** This entire feature reads existing data; no CRDT events published, no on-disk persistence, no IPC payload pinning. `schema_version` on the snapshot covers export-format stability.

## 9. Rollout

Single PR, branch `zeb-329-network-health` off latest `origin/main`. Estimated ~600 LOC backend + tests, ~400 LOC frontend across view + modal + adapter + helpers. No migrations. No new dependencies. Documentation deliverable (`cross-wan-validation.md`) lands in the same PR.

PR title: `ZEB-329: Network Health panel + self-test + cross-WAN validation playbook (ZEB-327 Sub-B)`. PR body uses markdown-linked refs for ZEB-327 and ZEB-329 (no bare-close trigger on the parent, since ZEB-327 has Sub-C and Sub-D still to come).

## 10. Open questions

None. All decisions locked during the 2026-05-24 brainstorm.

## 11. Cross-references for downstream work

- **Sub-C (onboarding UX)** will reference the Network panel in the first-run walkthrough ("after creating identity, here's how to see your connection state")
- **Sub-D (Zeblithic + invite distribution)** will reference this panel in tester-facing docs ("if you can't reach anyone, open Network and run self-test")
- **[ZEB-321](https://linear.app/zeblith/issue/ZEB-321) Phase 3** (liveness/rebinding) builds on top — the panel will need new fields when rebinding state becomes a thing, but the `schema_version` mechanism is in place to handle that without breaking older exports
