# ZEB-877 — Fleet publish-retry observability (Full parity + DRY) — Design

**Goal:** Make each fleet `FleetSyncEngine<T>`'s publish-side `RetryBackoff` state (and its ZEB-705 fetch-retry counters) observable to an operator through `NetworkHealthSnapshot`, mirroring the community-side seam ZEB-805 + ZEB-762 established.

**Architecture:** A non-generic `FleetSyncStats` atomics struct is shared between each engine's task-side `Ctx` and its handle. A lightweight collector `FleetSyncRegistry` holds `(FleetDoc → Arc<FleetSyncStats>)` for the 11 live engines and implements a new `network_health::FleetSyncSource`. `network_health` reads it into `fleet_sync: Vec<FleetSyncHealth>` on the snapshot. Shared observability types (`PublishErrorClass`, `PublishRetryHealth`) are hoisted so community and fleet use one type each.

**Tech Stack:** Rust (`AtomicBool`/`AtomicU64`/`AtomicU8`/`Relaxed`, `tokio::sync::Mutex`, `async_trait`, serde camelCase DTOs), TypeScript type mirror.

## Global Constraints

- **Additive, forward-compatible wire.** New snapshot field `fleet_sync` and the new DTO's nested `publish_retry` field are `#[serde(default)]`; `NetworkHealthSnapshot::empty()` stays valid (spec §6.1: snapshot never throws). A pre-field cached snapshot deserializes to an empty vec / defaults.
- **Observability only.** No change to retry policy — the ZEB-761 schedule (30 s base → 600 s cap) and precedence (fresh mutation supersedes pending retry) are settled and untouched. `RetryBackoff` accessors are read, never modified.
- **No duplication.** One `PublishErrorClass`, one `PublishRetryHealth` (Rust + TS) shared by both subsystems. Reuse the ZEB-762 Raw→DTO tier-mapping split.
- **Recording gated on `DirtySignal::Restore`.** A failed publish that owes nothing (`Spent`) is not a replication stall and records nothing — identical gating to the community side.
- **Wire + TS-type only.** No new panel widget. `fleet_sync` reaches operators via the full-snapshot diagnostic export, exactly like `community_sync` / `community_relay` / `dm_fence` / `gateway_bootstrap`. A live-panel rendering of the whole surface is a separable UI pass, deliberately out of scope.
- **Gate parity.** Local gate from `src-tauri/`: `cargo fmt --all -- --check`; `cargo clippy --locked --lib --bins --no-deps -- -D warnings`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Frontend (repo root): `npx tsc --noEmit`; `npx vitest run`.

---

## 1. The problem, and the insight that dissolves it

The community precedent works because **one** engine type sits in a registry that *owns* them (`BTreeMap<SpaceId, Arc<CommunitySyncEngine>>`, `community_state_sync.rs:5376`), populated by the registry's own factory spawn. Fleet has **11 heterogeneous** `FleetSyncEngine<T>` (10 distinct `T`) constructed inline in `start_node` with heavy per-engine config, held as individual typed `Option<Arc<…>>` fields on `NodeState`. There is no owning map, and converting the 11 bespoke construction sites to a factory would be a large, risky refactor.

**Insight:** the publish-retry and fetch-retry state is entirely **`T`-independent** — it is about the transport and the publish outcome, not the CRDT document. So `FleetSyncStats` is *non-generic*, every `FleetSyncEngine<T>` owns an `Arc<FleetSyncStats>` regardless of `T`, and that `Arc` is already type-erased. The registry therefore never touches the generic engines — only their stats handles. This lets a single registry enumerate 10 engine types uniformly **without** a factory and **without** a trait object over the engines.

Consequence: the fleet registry is a **collector** (each construction site pushes its `(label, stats)` pair), not a factory (as community is). This is the minimal seam.

## 2. Components

### 2.1 `FleetSyncStats` — `fleet_sync.rs` (new, non-generic)

```
#[derive(Debug, Default)]
pub(crate) struct FleetSyncStats {
    // ZEB-705 fetch-retry counters (migrated off Ctx's 4 separate Arc<AtomicU64>)
    fetch_retries_scheduled: AtomicU64,
    fetch_retries_run: AtomicU64,
    fetch_retries_dropped: AtomicU64,
    fetch_retry_inflight_peak: AtomicU64,
    // ZEB-762 publish-retry state (mirror of CommunitySyncStats)
    publish_retry_owed: AtomicBool,
    publish_retry_consecutive_failures: AtomicU64,
    publish_retry_backoff_ms: AtomicU64,
    publish_retry_last_failure_ms: AtomicU64,
    publish_retry_last_error_code: AtomicU8,
}
```

Methods mirror `CommunitySyncStats` exactly:
- `record_publish_failure(&self, backoff_ms: u64, err: &<FleetPublishError>, now_wall_ms: u64)` — sets `owed=true`, `fetch_add` failures, stores backoff, `last_failure_ms`, and the `PublishErrorClass` code (classifier over the fleet publish error type — exact variant map determined against the error enum during planning).
- `record_publish_success(&self)` — clears `owed=false`, failures=0, backoff=0; **retains** `last_failure_ms` / `last_error_code` as historical evidence.

Engine ownership: `FleetSyncEngine<T>` gains `sync_stats: Arc<FleetSyncStats>` (the real handle, **not** `#[cfg(test)]`) and `pub fn sync_stats(&self) -> Arc<FleetSyncStats>`. The `Ctx<S>` gains `sync_stats: Arc<FleetSyncStats>` (clone of the same Arc), constructed once in `FleetSyncEngine::new`.

### 2.2 `FleetDoc` — `network_health.rs` (new enum, the registry key + wire label)

```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FleetDoc {
    OwnerState, OwnerTrust, Notes, DmInbox, CommunityDeviceIntro,
    RelayHold, RelayOptIn, DmOuthold, FleetNet, OwnerQuorum, FleetKeys,
}
impl FleetDoc { pub fn label(self) -> &'static str { /* stable camelCase: "ownerState", … */ } }
```

`Ord` makes it a `BTreeMap` key (stable enumeration order). `label()` is the DTO's `doc` field. `OwnerState` vs `OwnerTrust` distinguishes the two `OwnerState`-typed datasets (#1 owner-state, #9 trust CRDT).

### 2.3 `FleetSyncRegistry` — `fleet_sync.rs` (new, collector)

```
pub struct FleetSyncRegistry { engines: tokio::sync::Mutex<BTreeMap<FleetDoc, Arc<FleetSyncStats>>> }
impl FleetSyncRegistry {
    pub fn new() -> Self
    pub async fn register(&self, doc: FleetDoc, stats: Arc<FleetSyncStats>)
    pub async fn fleet_sync_raw(&self) -> Vec<(FleetDoc, network_health::FleetSyncRaw)>  // atomic loads per engine
}
#[async_trait] impl network_health::FleetSyncSource for FleetSyncRegistry {
    async fn per_fleet(&self) -> Vec<(FleetDoc, network_health::FleetSyncRaw)> { self.fleet_sync_raw().await }
}
```

Registration is boot-only (11 sites, never removed). Mirrors community's `tokio::sync::Mutex` for pattern-consistency even though the map is effectively frozen after boot.

### 2.4 `network_health.rs` — the wire tier

- `FleetSyncSource` trait: `#[async_trait] pub trait FleetSyncSource: Send + Sync { async fn per_fleet(&self) -> Vec<(FleetDoc, FleetSyncRaw)>; }`.
- `FleetSyncRaw` (`Copy` struct): the 4 fetch counters (u64) + publish fields (`publish_retry_owed: bool`, `…_consecutive_failures/_backoff_ms/_last_failure_ms: u64`, `publish_retry_last_error: Option<&'static str>`). The `last_error` label is resolved from the `u8` code inside `fleet_sync_raw()` via `PublishErrorClass::from_u8(..).map(label)`, exactly as `community_sync_raw()` does.
- `FleetSyncHealth` (DTO, `#[serde(rename_all = "camelCase")]`): `doc: String`, `fetch_retries_scheduled/run/dropped/inflight_peak: u64`, `#[serde(default)] publish_retry: PublishRetryHealth`.
- `fleet_sync_row(doc: FleetDoc, raw: FleetSyncRaw, now: u64) -> FleetSyncHealth` — 0-sentinel → `None` for `last_failure_ms`; builds nested `PublishRetryHealth`. (No staleness derivation — fleet has no inbound/advance timestamps in this scope; publish-retry + fetch counters only.)
- `NetworkHealthService`: `fleet_sync: Option<Arc<dyn FleetSyncSource>>` + `set_fleet_sync_source(..)`; `snapshot()` assembles `fleet_sync: Vec<FleetSyncHealth>` (empty when unset). New snapshot field `#[serde(default)] pub fleet_sync: Vec<FleetSyncHealth>`.

### 2.5 Shared-type hoist (the DRY half)

- **`PublishErrorClass`** moves from `community_state_sync.rs` into `network_health.rs`: the enum (`TransportClosed=1 … Other=5`), `from_u8`, `label`, and `code`/`as_u8`. Each subsystem keeps its own classifier free-fn over its own error type (`classify_community_publish_error`, `classify_fleet_publish_error`) returning the shared enum — so `network_health` never depends on either error type. `community_state_sync.rs` is repointed to the moved enum (minimal edit).
- **`PublishRetryHealth`**: rename `CommunityPublishRetryHealth` → `PublishRetryHealth` (Rust + TS). `CommunitySyncHealth.publish_retry` and `FleetSyncHealth.publish_retry` both reference it. Field names are unchanged, so the wire is byte-identical for the community path (rename is source-only).

## 3. Data flow

```
fleet_sync::settle_publish  (DirtySignal::Restore → on_failure; ok → clear)
  └─ ctx.sync_stats.record_publish_failure(retry.delay_ms(), &err, now_ms())   // gated on Restore
  └─ ctx.sync_stats.record_publish_success()                                    // on ok
        │  (Arc<FleetSyncStats> shared with the handle)
        ▼
FleetSyncEngine::sync_stats()  ──registered at boot──▶  FleetSyncRegistry (label → stats)
        ▼
FleetSyncSource::per_fleet()  →  fleet_sync_raw()  (atomic loads)  →  Vec<(FleetDoc, FleetSyncRaw)>
        ▼
NetworkHealthService::snapshot()  →  fleet_sync_row(doc, raw, now)  →  FleetSyncHealth
        ▼
NetworkHealthSnapshot.fleet_sync  ──(existing network_health_snapshot IPC)──▶  TS FleetSyncHealth[]
```

Recording call site is `fleet_sync.rs settle_publish` (`:678-699`): after `retry.on_failure(now_ms)` (`:692`) record failure with `retry.delay_ms()` and the publish error; after `retry.clear(now_ms)` (`:697`) record success. `now_wall_ms` from `network_health::now_ms()`.

## 4. The 11 registration sites (`start_node`, `lib.rs`)

| `FleetDoc` | Construction | `T` |
|---|---|---|
| `OwnerState` | `lib.rs:5731` → `owner_state_sync::SyncEngine::new` (`owner_state_sync.rs:142`) — via a new `SyncEngine::sync_stats()` delegating accessor | `OwnerState` |
| `Notes` | `lib.rs:5954` | `NotesDoc` |
| `DmInbox` | `lib.rs:6052` | `DmInboxDoc` |
| `CommunityDeviceIntro` | `lib.rs:6217` | `CommunityDeviceIntroDoc` |
| `RelayHold` | `lib.rs:6333` | `RelayHoldDoc` |
| `RelayOptIn` | `lib.rs:6385` | `RelayOptInDoc` |
| `DmOuthold` | `lib.rs:6477` | `DmOutholdDoc` |
| `FleetNet` | `lib.rs:6589` | `FleetNetDoc` |
| `OwnerTrust` | `lib.rs:6836` | `OwnerState` (trust CRDT) |
| `OwnerQuorum` | `lib.rs:6916` | `QuorumReqDoc` |
| `FleetKeys` | `lib.rs:7067` | `FleetKeyEpochDoc` |

`NodeState` gains `fleet_sync_registry: Arc<FleetSyncRegistry>`, created before the engines. Each site adds one `fleet_sync_registry.register(FleetDoc::X, engine.sync_stats()).await`. Boot wiring adds `nh.set_fleet_sync_source(Arc::clone(&fleet_sync_registry) as Arc<dyn FleetSyncSource>)` next to the community wiring (`lib.rs:~13544`).

The `owner_state_sync::SyncEngine` wrapper (#1) owns its `FleetSyncEngine<OwnerState>` privately; it exposes `pub fn sync_stats(&self) -> Arc<FleetSyncStats>` delegating to the inner engine (one accessor added in `owner_state_sync.rs`).

## 5. Fetch-counter migration (resolving item 2 by promotion)

Delete the 4 `#[cfg(test)]` handle-side fields (`fleet_sync.rs:281-288`) and 4 accessors (`:423-444`). Replace the 4 separate `Ctx` `Arc<AtomicU64>` fetch fields (`:641-644`, created `:352-355`, assigned `:388-391`) with reads/writes on `ctx.sync_stats`. Increment sites (`:865` run, `:1257`/`:1290` dropped, `:1264` scheduled, `:1268` inflight_peak, and the `:1276` clone for the spawned task → clone the single `Arc<FleetSyncStats>`) update to `ctx.sync_stats.fetch_*`. The ~11 in-module test reads (`:3135…:3459`) migrate from `engine.fetch_retries_run()` to `engine.sync_stats().fetch_retries_run.load(Relaxed)`. Behavior is unchanged; the counters are now real telemetry via `FleetSyncHealth`.

## 6. Wire compatibility & fixtures

Additive `#[serde(default)]` field on the snapshot + additive nested `#[serde(default)] publish_retry`. Pinned network-health fixtures pin community **CRDT network bytes**, not the network-health IPC DTO (verified for ZEB-762); the snapshot serde tests assert per-key, not by exact-object equality. Re-verify no fixture byte-pins the snapshot shape before landing. The `PublishRetryHealth` rename is source-only (field names unchanged) → community wire byte-identical.

## 7. Testing

- **Unit** — `FleetSyncStats::record_publish_failure`/`record_publish_success`: escalation surfaces; success clears active state while retaining historical `last_failure`/`last_error`.
- **Classifier** — `classify_fleet_publish_error` variant→class over the fleet publish error type; shared `PublishErrorClass` `u8` round-trip (incl. `other` fall-through and `0`=none sentinel; may already be covered on the community side — add the fleet-error classifier case).
- **End-to-end** — drive a fleet engine into a wedged-publisher / sustained-backoff state (dirty armed so `settle` → `Restore`) and assert the escalating backoff surfaces through the **real assembly path** (`settle_publish` → stats → registry `fleet_sync_raw` → `fleet_sync_row` DTO), mirroring `publish_retry_backoff_surfaces_in_community_sync_row_zeb762`.
- **Serde** — extend the camelCase test for `fleetSync` keys + a pre-field **absence-default** deserialize (snapshot never throws); keep a `PublishRetryHealth` camelCase regression assertion for the community path (rename guard).
- **Migration** — the ~11 existing fleet fetch-counter tests continue to pass through the new `sync_stats()` path.

## 8. File structure / touch list

- `src-tauri/src/fleet_sync.rs` — `FleetSyncStats` (+ methods), `classify_fleet_publish_error`, `settle_publish` recording, Ctx/handle refactor (fetch counters → stats; delete `cfg(test)` clones), `sync_stats()` accessor, `FleetSyncRegistry` + `FleetSyncSource` impl, test migration + new tests.
- `src-tauri/src/network_health.rs` — `FleetDoc`, `FleetSyncRaw`, `FleetSyncHealth`, `PublishRetryHealth` (rename from `CommunityPublishRetryHealth`), hoisted `PublishErrorClass`, `FleetSyncSource` trait, `fleet_sync_row`, snapshot field + assembly + `set_fleet_sync_source`, serde tests.
- `src-tauri/src/community_state_sync.rs` — repoint `PublishErrorClass` to the moved enum; `CommunitySyncHealth.publish_retry` uses the renamed `PublishRetryHealth`; `classify_community_publish_error` free-fn (extracted from the old enum's `of`).
- `src-tauri/src/owner_state_sync.rs` — `SyncEngine::sync_stats()` delegating accessor.
- `src-tauri/src/lib.rs` — `NodeState.fleet_sync_registry`, create-before-engines, 11 `register(..)` calls, `set_fleet_sync_source(..)` at boot.
- `src/lib/types/network-health.ts` — `FleetSyncHealth` interface, `fleetSync?: FleetSyncHealth[]` on the snapshot, `PublishRetryHealth` rename (referenced by both `CommunitySyncHealth` and `FleetSyncHealth`).
- `src/lib/network-health-adapter.ts` — only if it enumerates snapshot fields for the diagnostic export; verify during planning (community_sync required no adapter change).

## Non-goals

Retry policy (ZEB-761 schedule + precedence). A live panel widget for the fleet/community sync surface (separable UI pass). Fleet engine factory-ization (the collector avoids it).
