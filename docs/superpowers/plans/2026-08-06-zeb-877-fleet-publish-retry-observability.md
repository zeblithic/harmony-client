# ZEB-877 — Fleet publish-retry observability — Implementation Plan

> Executed **inline** (executing-plans) — the 7 files' types interlock (tightly coupled), so no subagent fan-out. Each task ends with a scoped gate; a full local gate runs at the end.

**Goal:** Surface each fleet `FleetSyncEngine<T>`'s publish-retry `RetryBackoff` state + ZEB-705 fetch counters through `NetworkHealthSnapshot.fleet_sync`, mirroring the community seam (ZEB-805 + ZEB-762). Design: `docs/superpowers/specs/2026-08-06-zeb-877-fleet-publish-retry-observability-design.md`.

## Global Constraints

- Additive `#[serde(default)]` wire fields; `NetworkHealthSnapshot::empty()` stays valid. Recording gated on `DirtySignal::Restore`. Retry policy untouched (read `RetryBackoff` only). Wire + TS-type only (no panel widget). One shared `PublishErrorClass` + one shared `PublishRetryHealth`.
- Gate from `src-tauri/`: `cargo fmt --all -- --check`; `cargo clippy --locked --lib --bins --no-deps -- -D warnings`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Frontend (root): `npx tsc --noEmit`; `npx vitest run`.

## Precedent anchors (mirror these exactly)

- `CommunitySyncStats` + `record_publish_failure`/`record_publish_success` + `PublishErrorClass`: `community_state_sync.rs:3720-3832`.
- `community_sync_raw`: `community_state_sync.rs:5869-5905`.
- `CommunitySyncHealth` / `CommunityPublishRetryHealth` / `CommunitySyncRaw` / `CommunitySyncSource`: `network_health.rs:126-222`; snapshot field `:114-115`.
- `community_sync_row`: `network_health.rs:1903-1934`.
- `NetworkHealthService.community_sync` field `:2321`; `set_community_sync_source` `:2472`; snapshot assembly `:2832-2840`.
- Fleet `settle_publish` `fleet_sync.rs:678-699` (called `:811`, `:832`); `SyncError` `:115-128`; Ctx fetch fields `:641-644`; increments `:865,1257,1264,1268,1276,1290`; handle cfg(test) fields `:281-288` + accessors `:423-444`.

---

## Task 1 — Hoist `PublishErrorClass`; rename `CommunityPublishRetryHealth → PublishRetryHealth`

**Files:** `network_health.rs`, `community_state_sync.rs`.

1. `network_health.rs`: add near the community DTOs —
   ```rust
   /// Shared coarse class of a publish-path failure (community + fleet).
   /// Stable u8 (0 = none, discriminants start at 1) + stable snake_case label.
   #[derive(Clone, Copy, PartialEq, Eq, Debug)]
   pub enum PublishErrorClass { TransportClosed = 1, ContentStore = 2, Crypto = 3, Encode = 4, Other = 5 }
   impl PublishErrorClass {
       pub fn from_u8(code: u8) -> Option<Self> { /* 1..=5 → variant, else None */ }
       pub fn label(self) -> &'static str { /* transport_closed|content_store|crypto|encode|other */ }
   }
   ```
2. `network_health.rs`: rename `CommunityPublishRetryHealth` → `PublishRetryHealth` (fields, derives, `#[serde(rename_all="camelCase")]` unchanged); update `CommunitySyncHealth.publish_retry: PublishRetryHealth` and the `community_sync_row` construction literal (`:1923`).
3. `community_state_sync.rs`: delete the local `enum PublishErrorClass` + impl (`:3790-3832`); add `fn classify_community_publish_error(err: &CommunitySyncError) -> crate::network_health::PublishErrorClass` (old `of` body). Update `record_publish_failure` (`:3769`) → `classify_community_publish_error(err) as u8`; update `community_sync_raw` (`:5897-5900`) → `crate::network_health::PublishErrorClass::from_u8(..).map(crate::network_health::PublishErrorClass::label)`; update the in-module tests (`:8117-8176`) to the moved enum + `classify_community_publish_error`.
4. **Gate:** `cargo clippy --locked --lib --bins --no-deps -- -D warnings` + `cargo nextest run -E 'test(community_sync) + test(publish_error) + test(publish_retry)'`.

## Task 2 — `FleetSyncStats` + classifier + engine plumbing + recording + fetch-counter migration

**Files:** `fleet_sync.rs`.

1. Add `pub(crate) struct FleetSyncStats` (`#[derive(Debug, Default)]`): `fetch_retries_scheduled/run/dropped/inflight_peak: AtomicU64`; `publish_retry_owed: AtomicBool`; `publish_retry_consecutive_failures/backoff_ms/last_failure_ms: AtomicU64`; `publish_retry_last_error_code: AtomicU8`. `impl`: `record_publish_failure(&self, backoff_ms: u64, err: &SyncError, now_wall_ms: u64)` + `record_publish_success(&self)` — byte-for-byte mirror of `community_state_sync.rs:3760-3781`, with `classify_fleet_publish_error(err) as u8`.
2. `fn classify_fleet_publish_error(err: &SyncError) -> crate::network_health::PublishErrorClass`: `TransportClosed→TransportClosed`, `ContentStore(_)→ContentStore`, `Crypto(_)→Crypto`, `CborEncode(_)→Encode`, `_→Other`.
3. Handle `FleetSyncEngine`: delete the 4 `#[cfg(test)]` fetch fields (`:281-288`); add `sync_stats: Arc<FleetSyncStats>` (ungated) + `pub fn sync_stats(&self) -> Arc<FleetSyncStats> { Arc::clone(&self.sync_stats) }`. Ctx: replace the 4 `Arc<AtomicU64>` fetch fields (`:641-644`) with `sync_stats: Arc<FleetSyncStats>`.
4. `new()`: `let sync_stats = Arc::new(FleetSyncStats::default());` — clone into Ctx (`:388-391`) and handle (`:410-417`, drop the `#[cfg(test)]`).
5. Increment sites → `ctx.sync_stats.fetch_*`: `:865` run; `:1257`/`:1290` dropped; `:1264` scheduled; `:1268` inflight_peak (`fetch_max`); `:1276` clone → clone the single `Arc<FleetSyncStats>` for the spawned task. (Read `:1245-1295` at edit time.)
6. `settle_publish` recording: in the `DirtySignal::Restore` arm after `retry.on_failure(now_ms)` — `if let Err(e) = pub_result { ctx.sync_stats.record_publish_failure(retry.delay_ms(), e, crate::network_health::now_ms()); }`; after `retry.clear(now_ms)` in the `is_ok()` arm — `ctx.sync_stats.record_publish_success();`.
7. Delete cfg(test) accessors (`:423-444`); migrate the ~11 in-module test reads (`:3135-3459`) `engine.fetch_retries_run()` → `engine.sync_stats().fetch_retries_run.load(Ordering::Relaxed)`.
8. Add unit tests: `fleet_publish_retry_stats_record_escalation_and_clear` (owed/failures/backoff/last-error escalate; success clears active, retains historical); `fleet_publish_error_class_maps_variants` (each `SyncError` variant → expected label via classifier + shared `from_u8` round-trip).
9. **Gate:** `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` + `cargo nextest run -E 'package(harmony-app) and test(fleet)'` (or the fleet_sync module).

## Task 3 — `FleetDoc`/`FleetSyncRaw`/`FleetSyncHealth`/`FleetSyncSource`/`fleet_sync_row` + snapshot; `FleetSyncRegistry`

**Files:** `network_health.rs`, `fleet_sync.rs`.

1. `network_health.rs`:
   - `pub enum FleetDoc { OwnerState, OwnerTrust, Notes, DmInbox, CommunityDeviceIntro, RelayHold, RelayOptIn, DmOuthold, FleetNet, OwnerQuorum, FleetKeys }` — `#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]`; `pub fn label(self) -> &'static str` (camelCase: `"ownerState"…"fleetKeys"`).
   - `pub struct FleetSyncRaw` (`#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]`): 4 fetch u64 + `publish_retry_owed: bool` + 3 publish u64 + `publish_retry_last_error: Option<&'static str>`.
   - `pub struct FleetSyncHealth` (`Serialize, Deserialize, PartialEq`, camelCase): `doc: String`; `fetch_retries_scheduled/run/dropped/inflight_peak: u64`; `#[serde(default)] publish_retry: PublishRetryHealth`.
   - `#[async_trait::async_trait] pub trait FleetSyncSource: Send + Sync { async fn per_fleet(&self) -> Vec<(FleetDoc, FleetSyncRaw)>; }`
   - `pub fn fleet_sync_row(doc: FleetDoc, raw: FleetSyncRaw) -> FleetSyncHealth` (no `now`/staleness): map fetch counters straight; `publish_retry: PublishRetryHealth { owed, consecutive_failures, backoff_ms, last_failure_ms: (raw.publish_retry_last_failure_ms != 0).then_some(..), last_error: raw.publish_retry_last_error.map(Cow::Borrowed) }`.
   - `NetworkHealthService`: `fleet_sync: Option<Arc<dyn FleetSyncSource>>` (init `None` in `new`); `pub(crate) fn set_fleet_sync_source(&mut self, src)`; snapshot() assembly mirroring `:2832-2840` with `per_fleet()`/`fleet_sync_row`. `NetworkHealthSnapshot`: `#[serde(default)] pub fleet_sync: Vec<FleetSyncHealth>`.
2. `fleet_sync.rs`:
   ```rust
   pub struct FleetSyncRegistry { engines: tokio::sync::Mutex<BTreeMap<network_health::FleetDoc, Arc<FleetSyncStats>>> }
   impl FleetSyncRegistry {
       pub fn new() -> Self { Self { engines: tokio::sync::Mutex::new(BTreeMap::new()) } }
       pub async fn register(&self, doc: network_health::FleetDoc, stats: Arc<FleetSyncStats>) { self.engines.lock().await.insert(doc, stats); }
       pub async fn fleet_sync_raw(&self) -> Vec<(network_health::FleetDoc, network_health::FleetSyncRaw)> { /* lock, iter, atomic loads; last_error via network_health::PublishErrorClass::from_u8(..).map(..label) */ }
   }
   #[async_trait::async_trait] impl network_health::FleetSyncSource for FleetSyncRegistry {
       async fn per_fleet(&self) -> Vec<(network_health::FleetDoc, network_health::FleetSyncRaw)> { self.fleet_sync_raw().await }
   }
   ```
3. Serde tests (`network_health.rs`): extend the camelCase test for `fleetSync` keys (+ `publishRetry` nested); add a pre-field absence-default deserialize of a snapshot without `fleetSync`; keep a `PublishRetryHealth` camelCase assertion (rename guard).
4. **Gate:** `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` + `cargo nextest run -E 'test(fleet_sync) + test(network_health) + test(snapshot)'`.

## Task 4 — Boot wiring

**Files:** `owner_state_sync.rs`, `lib.rs`.

1. `owner_state_sync.rs`: `pub fn sync_stats(&self) -> Arc<crate::fleet_sync::FleetSyncStats>` on `SyncEngine`, delegating to the inner `FleetSyncEngine` (`:142` region).
2. `lib.rs`: add `fleet_sync_registry: Arc<FleetSyncRegistry>` to `NodeState`; create `let fleet_sync_registry = Arc::new(FleetSyncRegistry::new());` in `start_node` before the engines; after each of the 11 constructions add `fleet_sync_registry.register(FleetDoc::X, engine.sync_stats()).await;` (labels per the spec §4 table); wire `nh.set_fleet_sync_source(Arc::clone(&fleet_sync_registry) as Arc<dyn network_health::FleetSyncSource>);` next to the community wiring (`:~13544`).
3. **Gate:** `cargo check --locked --all-targets --features test-fixtures`.

## Task 5 — TS types + integration test + full gate

**Files:** `src/lib/types/network-health.ts`, `fleet_sync.rs` (test), maybe `src/lib/network-health-adapter.ts`.

1. `network-health.ts`: rename `CommunityPublishRetryHealth → PublishRetryHealth` (referenced by `CommunitySyncHealth.publishRetry`); add `FleetSyncHealth` interface (`doc`, 4 `fetchRetries*`, `publishRetry?: PublishRetryHealth`); add `fleetSync?: FleetSyncHealth[]` to `NetworkHealthSnapshot`. Verify `network-health-adapter.ts` (diagnostic passthrough — expect no change, as `community_sync` needed none).
2. Integration test (mirror `publish_retry_backoff_surfaces_in_community_sync_row_zeb762`): construct a fleet engine with a failing publish transport, `notify_dirty()`, drive ≥2 failed flushes, register its stats, read back via `registry.fleet_sync_raw()` → `network_health::fleet_sync_row(..)`, assert `publish_retry.owed`, `consecutive_failures >= 2`, `backoff_ms >= RETRY_BASE_MS`, `last_error.is_some()`. (Read the fleet test harness at `fleet_sync.rs:1621/1713/2175` for the failing-transport construction.)
3. Fixtures: `grep -rn "fleet_sync\|network_health" src-tauri/tests` for any snapshot byte-pin (expect none — DTO asserted per-key).
4. **Full gate:** fmt · clippy `--lib --bins` · clippy `--all-targets --features test-fixtures` · nextest `--workspace --all-targets --features test-fixtures` · tsc · vitest.

## Self-review

- **Spec coverage:** item 1 (publish-retry observability) = Tasks 2-4; item 2 (fetch-counter resolution by promotion + cfg(test) delete) = Task 2; DRY hoist = Task 1; wire+TS = Tasks 3,5; gate/tests = Tasks 2,3,5. ✓
- **Type consistency:** `PublishErrorClass` (network_health, shared), `PublishRetryHealth` (network_health, shared), `FleetSyncStats` (fleet_sync), `FleetDoc`/`FleetSyncRaw`/`FleetSyncHealth`/`FleetSyncSource` (network_health), `FleetSyncRegistry` (fleet_sync). `sync_stats()` on both `FleetSyncEngine` and `owner_state_sync::SyncEngine`. ✓
- **Placeholder scan:** the only prose-level items are JIT reads (increment sites `:1245-1295`, fleet test harness `:1621+`) — concrete anchors, not open-ended. ✓
