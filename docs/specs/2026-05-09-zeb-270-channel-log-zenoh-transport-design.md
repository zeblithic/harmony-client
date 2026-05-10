# ZEB-270 Phase 3 — ChannelLog Zenoh transport + IPCs

**Status:** Design approved 2026-05-09. Branch `zeb-270-channel-log-zenoh-transport` cuts from `origin/main` `3ac6671` (post-Phase-2 merge).

**Linear:** [ZEB-270](https://linear.app/zeblith/issue/ZEB-270/harmony-client-zeb-248-phase-3-channellog-zenoh-transport-ipcs).

## 1. Context

Third of four phases implementing [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) (Sub-C v2: channels-within-communities). The parent design lives at `docs/specs/2026-05-09-zeb-248-channels-within-communities-design.md` (commit `5145484`); this document refines its §9 (sync), §10 (registry), §11 (IPC surface) to a build-ready shape and locks plan-time questions §15.

**Phase status entering Phase 3:**
- **Phase 1 ([ZEB-266](https://linear.app/zeblith/issue/ZEB-266))** — channel-config CRDT (`MembershipEventKind::ChannelCreate/Modify/Delete`, materialize, verify gates, 4 channel-management IPCs, default `#general` auto-create). Merged via PR #93.
- **Phase 2 ([ZEB-269](https://linear.app/zeblith/issue/ZEB-269))** — in-process ChannelLog primitives (`SignedChannelEvent`, `ChannelKey` HKDF, `ChannelLog` manifest+tail+segments, `ChannelLogReplayTracker`, `verify_channel_event`). Merged via PR #95.
- **Cross-cutting refactor ([ZEB-267](https://linear.app/zeblith/issue/ZEB-267))** — atomic HLC reservation. Merged via PR #94.

Phase 3 ships the Zenoh transport that wraps the Phase 2 primitives, plus the lifecycle binding to channel-config materialize, plus the three message-surface IPCs and two Tauri events that Phase 4 (frontend) will consume.

## 2. Scope

1. New module `src-tauri/src/community_channel_log_engine.rs` containing:
   - `ChannelLogEngine` — owns one `ChannelLog` per `(community_id, channel_id)`, wraps Zenoh sub + queryable + publisher tasks, drives debounced disk flush.
   - `ChannelLogRegistry` — per-running-CommunitySyncEngine; manages spawn/stop of channel engines; reconciles from materialized `CommunityState.channels` on boot.
   - `ChannelLogEngineConfig` — wraps Phase 2 `ChannelLogConfig` plus Phase 3 tunables (debounce, max-dirty cap, default backfill limit).
   - `ChannelLogEngineError` enum.
2. New function in `src-tauri/src/event_loop.rs`: `spawn_channel_log_zenoh_adapter` — bespoke per-(community, channel) Zenoh adapter, mirroring `spawn_community_state_zenoh_adapter`.
3. Three new IPCs in `src-tauri/src/lib.rs`:
   - `post_channel_message`
   - `list_channel_messages`
   - `request_channel_backfill`
4. Two new Tauri events:
   - `channel-message-received`
   - `channel-backfill-progress`
5. Lifecycle hook in the existing `run_community_delta_consumer` task: a new third callback that calls `channel_log_registry.spawn` on `ChannelConfigChangeAction::Created`, `.stop` on `Deleted`, and is a no-op on `Modified`.
6. Boot-time reconciliation: `ChannelLogRegistry::reconcile_from_state` walks each running community's materialized channels map and spawns engines for live (non-deleted) channels.
7. New integration test `src-tauri/tests/community_channel_messages_integration.rs` covering live broadcast, offline-then-backfill, and replay rejection.
8. Wire-format pin extension in `src-tauri/tests/wire_format_channel_log_fixtures.rs` for the per-event backfill reply packet.

## 3. Out of scope

- **Frontend** — `CommunityView.svelte`, channel sub-sidebar, `ChannelMessageService`, scroll-trigger backfill, dialogs. Phase 4.
- **Voice/video channel transport** — separate Sub-D.
- **Channel categories / nested folders** — outside ZEB-248.
- **Channel-level permissions distinct from `write_power`** — already in Phase 2 verify chain. Outside ZEB-248.
- **Per-channel rate-limiting / DoS guards beyond `limit` parameter** — YAGNI for v3 (see §17.2).
- **Backfill auto-retry / exponential backoff** — surfaced as failure to caller; UI re-fires (see §17.2).
- **Promoting `ChannelLog` to a shared module / wrapping `ChannelKey` in a newtype with rotation API** — Phase 2 shapes are sufficient.

## 4. Architecture overview

The data plane mirrors the well-established split in `community_state_sync.rs` + `event_loop.rs`:

```
                 ┌─────────────────────────────────────────────┐
   IPC layer ───▶│ ChannelLogEngine                            │
                 │   - log: Arc<Mutex<ChannelLog>>            │
                 │   - replay_tracker                          │
                 │   - publish() / list_messages() /           │
                 │     request_backfill() / flush_now()        │
                 │   - debounced flush loop (250 ms / 1 s cap)│
                 └─────────────┬───────────────────────────────┘
                               │ mpsc<Vec<u8>> (publisher_tx, subscriber_rx, query_request_tx)
                               ▼
                 ┌─────────────────────────────────────────────┐
   event_loop ──▶│ spawn_channel_log_zenoh_adapter             │
                 │   - publisher task (drain → session.put)    │
                 │   - subscriber task (declare → forward)     │
                 │   - queryable task (declare → reply per ev) │
                 │   - query-request task (drain → session.get)│
                 └─────────────────────────────────────────────┘
                               │
                               ▼ Zenoh
                 harmony/channels/{cid}/{ch_id}/events     (broadcast)
                 harmony/channels/{cid}/{ch_id}/since/**   (queryable)
```

The engine is pure logic + three mpsc channel pairs (publisher, subscriber, query-request) plus a callback the queryable handler uses to read the log. The adapter is the only thing that touches the `zenoh::Session`. This split mirrors `CommunitySyncEngine` ↔ `spawn_community_state_zenoh_adapter` and is non-negotiable: it makes the engine unit-testable without Zenoh and lets the adapter handle session-level concerns (closing flag, biased select, degraded reporting) uniformly across engine types.

The registry is a thin async-Mutex-guarded `HashMap<(SpaceId, ChannelId), Arc<ChannelLogEngine>>` plus the boot-reconcile path. It mirrors `CommunitySyncRegistry`.

## 5. Module split

| File | Role | Why this boundary |
|---|---|---|
| `community_channel_log.rs` (Phase 2, unchanged) | Pure log primitives + crypto + verify chain. | Phase 2 stays I/O-free and tokio-free. |
| `community_channel_log_engine.rs` (Phase 3, new) | Engine + registry + config + error type + tokio tasks. | Phase 3 owns lifecycle + I/O; mirrors `community_state_sync.rs`. |
| `event_loop.rs` (extended) | `spawn_channel_log_zenoh_adapter` next to `spawn_community_state_zenoh_adapter`. | The Zenoh session belongs to event_loop; per-engine adapters are spawned by it. |
| `lib.rs` (extended) | 3 IPCs + 2 Tauri event payloads + delta-consumer callback wire-up + module registration. | IPC layer is centralized in lib.rs by repo convention. |

## 6. ChannelLogEngine

### 6.1 Type

```rust
pub struct ChannelLogEngine {
    community_id: SpaceId,
    channel_id: ChannelId,
    channel_key: Arc<ChannelKey>,
    log: Arc<Mutex<ChannelLog>>,
    replay_tracker: Arc<Mutex<ChannelLogReplayTracker>>,
    state_at_hlc: Arc<dyn CommunityStateAtHlc>,
    resolver: Arc<dyn ChannelIdentityResolver>,
    self_owner: OwnerAddr,
    self_device_id: String,
    signing_key: Arc<SigningKey>,
    hlc_tracker: Arc<Mutex<CommunityRootHlcTracker>>,    // shared with sync engine; per-device monotone HLC
    config: ChannelLogEngineConfig,

    // I/O channels owned by the spawned adapter on the event_loop side.
    publisher_tx: mpsc::Sender<Vec<u8>>,
    query_request_tx: mpsc::Sender<BackfillQueryRequest>,

    // Internal task handles.
    receive_handle: tokio::task::JoinHandle<()>,
    flush_handle: tokio::task::JoinHandle<()>,

    // Coordination.
    flush_dirty: Arc<Notify>,
    closing: Arc<AtomicBool>,
    app: AppHandle,                                       // for Tauri event emission
}
```

### 6.2 Methods

```rust
/// Per-instance bundle passed to ChannelLogEngine::new. Bundles per-engine deps
/// + the I/O channel endpoints + the tunables config.
pub struct ChannelLogEngineParams {
    pub community_id: SpaceId,
    pub channel_id: ChannelId,
    pub channel_key: Arc<ChannelKey>,
    pub root_dir: PathBuf,                                // for ChannelLog::reload / new
    pub state_at_hlc: Arc<dyn CommunityStateAtHlc>,
    pub resolver: Arc<dyn ChannelIdentityResolver>,
    pub self_owner: OwnerAddr,
    pub self_device_id: String,
    pub signing_key: Arc<SigningKey>,
    pub hlc_tracker: Arc<Mutex<CommunityRootHlcTracker>>,
    pub app: AppHandle,                                   // for Tauri event emission
    pub config: ChannelLogEngineConfig,                   // tunables (see §6.3)

    // I/O channel endpoints. Other ends owned by the adapter spawned by the registry.
    pub publisher_tx: mpsc::Sender<Vec<u8>>,
    pub subscriber_rx: mpsc::Receiver<Vec<u8>>,
    pub query_request_tx: mpsc::Sender<BackfillQueryRequest>,
}

impl ChannelLogEngine {
    /// Construct + spawn receive loop + flush loop. Caller (registry) provides the three mpsc
    /// channel pairs (publisher, subscriber, query-request) — adapter is spawned separately
    /// by event_loop and wired to the other ends.
    pub async fn new(params: ChannelLogEngineParams) -> Result<Arc<Self>, ChannelLogEngineError>;

    /// IPC entry: mint Post event, encrypt, broadcast, locally append.
    pub async fn publish(
        self: &Arc<Self>,
        body: Vec<u8>,
        reply_to: Option<MessageId>,
    ) -> Result<MessageId, ChannelLogEngineError>;

    /// Synchronous read from log; returns events in HLC order.
    /// `since=None` means "from the earliest available in tail+segments".
    /// Walks tail first (most recent), then segments backward, until limit reached.
    pub async fn list_messages(
        &self,
        since: Option<Hlc>,
        limit: usize,
    ) -> Result<Vec<SignedChannelEvent>, ChannelLogEngineError>;

    /// Fire a Zenoh queryable request via the adapter.
    /// Replies stream back through the same subscriber path; fire-and-forget for the IPC caller.
    pub async fn request_backfill(
        self: &Arc<Self>,
        since: Option<Hlc>,
    ) -> Result<(), ChannelLogEngineError>;

    /// Force tail-to-disk flush, bypassing the debounce window.
    pub async fn flush_now(&self) -> Result<(), ChannelLogEngineError>;

    /// Shutdown: flush + signal closing + join all internal tasks.
    pub async fn shutdown(&self) -> Result<(), ChannelLogEngineError>;
}
```

### 6.3 Config

```rust
#[derive(Clone, Debug)]
pub struct ChannelLogEngineConfig {
    pub log_config: ChannelLogConfig,                     // Phase 2; tests override seal_threshold_events
    pub flush_debounce_ms: u64,                           // default 250 (matches DEFAULT_DEBOUNCE_MS)
    pub max_dirty_ms: u64,                                // default 1000
    pub backfill_default_limit: usize,                    // default 256
    pub backfill_progress_event_interval: usize,          // default 16 (emit progress every 16 events)
}

impl Default for ChannelLogEngineConfig {
    fn default() -> Self { /* values above */ }
}
```

### 6.4 Internal state machines

**Flush loop:**
```
loop {
    select! {
        _ = flush_dirty.notified() => {
            // Sliding debounce: wait flush_debounce_ms or until max_dirty_ms since first dirty
            let mut deadline = Instant::now() + flush_debounce_ms;
            let hard_deadline = first_dirty_instant + max_dirty_ms;
            // Until either deadline fires
            loop {
                select! {
                    _ = sleep_until(min(deadline, hard_deadline)) => break,
                    _ = flush_dirty.notified() => {
                        deadline = Instant::now() + flush_debounce_ms;
                        // hard_deadline preserved (don't reset on continuous dirty)
                    }
                }
            }
            log.lock().flush_tail()?;       // bubble Persist errors via degraded event
            // Seal-on-threshold check: if tail.len() >= seal_threshold_events, also seal_and_persist().
        }
        _ = sleep(1s) => {
            if closing.load() { break; }
        }
    }
}
```

**Receive loop:**
```
loop {
    select! {
        Some(packet_bytes) = subscriber_rx.recv() => {
            // 1. decrypt_channel_packet → SignedChannelEvent (skip-on-fail)
            // 2. verify_channel_event chain (skip-on-fail; log warn on replay-tracker reject)
            // 3. log.append(event) — bumps replay tracker
            // 4. if Inserted, emit "channel-message-received" Tauri event
            // 5. notify flush_dirty
        }
        _ = subscriber_rx.closed() => break,
        _ = sleep(1s) => { if closing.load() { break; } }
    }
}
```

### 6.5 Self-loopback policy

`publish` does NOT send the event back through the subscriber path. After encrypting + sending to publisher_tx, it directly calls `log.append(event)` + emits the Tauri event itself. This avoids the round-trip through Zenoh-loopback (which Zenoh's pub-sub does support) but more importantly avoids depending on it for correctness — the local engine already has the event, no reason to wait for the network.

The replay tracker therefore sees self-events at append time, which means a peer's Zenoh-broadcast copy of our own event (if Zenoh loops it back) is correctly rejected as a replay. Good defensive layering.

## 7. ChannelLogRegistry

### 7.1 Type

```rust
pub struct ChannelLogRegistry {
    engines: tokio::sync::Mutex<HashMap<(SpaceId, ChannelId), Arc<ChannelLogEngine>>>,
    config: ChannelLogRegistryConfig,
    closing: Arc<AtomicBool>,
}

pub struct ChannelLogRegistryConfig {
    pub session: Arc<zenoh::Session>,
    pub app: AppHandle,
    pub identity_dir: PathBuf,
    pub self_owner: OwnerAddr,
    pub self_device_id: String,
    pub signing_key: Arc<SigningKey>,
    pub engine_config: ChannelLogEngineConfig,
}
```

### 7.2 Methods

```rust
impl ChannelLogRegistry {
    pub fn new(cfg: ChannelLogRegistryConfig) -> Arc<Self>;

    /// Spawn engine + its Zenoh adapter. Idempotent: returns the existing Arc if already present.
    pub async fn spawn(
        self: &Arc<Self>,
        community_id: SpaceId,
        channel_id: ChannelId,
        channel_key: ChannelKey,
        state_at_hlc: Arc<dyn CommunityStateAtHlc>,
        resolver: Arc<dyn ChannelIdentityResolver>,
        hlc_tracker: Arc<Mutex<CommunityRootHlcTracker>>,
    ) -> Result<Arc<ChannelLogEngine>, ChannelLogEngineError>;

    /// Stop engine: flush_now + shutdown + drop from HashMap. Idempotent (no-op if not present).
    pub async fn stop(
        &self,
        community_id: &SpaceId,
        channel_id: &ChannelId,
    ) -> Result<(), ChannelLogEngineError>;

    pub async fn engine(
        &self,
        community_id: &SpaceId,
        channel_id: &ChannelId,
    ) -> Option<Arc<ChannelLogEngine>>;

    /// Walk community_state.channels: spawn for live channels, ignore tombstoned.
    /// Idempotent — re-running on the same state is a no-op.
    pub async fn reconcile_from_state(
        self: &Arc<Self>,
        community_id: SpaceId,
        community_state: &CommunityState,
        membership_key: &MembershipKey,
        state_at_hlc: Arc<dyn CommunityStateAtHlc>,
        resolver: Arc<dyn ChannelIdentityResolver>,
        hlc_tracker: Arc<Mutex<CommunityRootHlcTracker>>,
    ) -> Result<(), ChannelLogEngineError>;

    pub async fn shutdown_all(&self) -> Result<(), ChannelLogEngineError>;
}
```

### 7.3 Lifecycle binding

The registry hooks into the existing `run_community_delta_consumer` task at `lib.rs:1230-1300`. That task already takes two callbacks (membership-changed, channel-config-updated). Phase 3 adds a third callback that fires the registry call on `ChannelConfigChangeAction`:

```rust
move |payload: &ChannelConfigChangedPayload, community_state: &CommunityState| async {
    let cid = SpaceId::from_hex(&payload.community_id)?;
    let chid = ChannelId::from_hex(&payload.channel_id)?;
    match payload.action {
        ChannelConfigChangeAction::Created => {
            let key = derive_channel_key(membership_key, cid, chid);
            registry.spawn(cid, chid, key, /* deps */).await?;
        }
        ChannelConfigChangeAction::Modified => { /* no-op for registry */ }
        ChannelConfigChangeAction::Deleted => {
            registry.stop(&cid, &chid).await?;
        }
    }
}
```

The membership_key needed for `derive_channel_key` is reachable via `CommunitySyncRegistry::engine_arc(community_id)` → engine state → membership_key. The delta consumer already has access to this via the registry handle.

### 7.4 Boot-time reconciliation

`ChannelLogRegistry::reconcile_from_state` is called once per community-engine startup (in `start_node`'s community-loading loop, after each `CommunitySyncRegistry.spawn_engine` returns). It walks `community_state.channels`:
- For each `(channel_id, ChannelInfo { deleted_at: None, .. })`, call `spawn` (idempotent — returns existing if already present).
- Tombstoned channels (`deleted_at.is_some()`) are skipped.

This is the single source of truth for which engines should be running: registry state must always reflect the materialized channels map. The delta-consumer callback handles incremental changes; reconcile handles the boot/restart case.

## 8. Zenoh adapter (event_loop)

`spawn_channel_log_zenoh_adapter` in `event_loop.rs`. Mirrors `spawn_community_state_zenoh_adapter` in shape. Spawned by `ChannelLogRegistry::spawn` (registry passes the four mpsc endpoints).

```rust
pub fn spawn_channel_log_zenoh_adapter(
    session: Arc<zenoh::Session>,
    community_id_hex: String,
    channel_id_hex: String,
    publisher_rx: mpsc::Receiver<Vec<u8>>,
    subscriber_tx: mpsc::Sender<Vec<u8>>,
    query_request_rx: mpsc::Receiver<BackfillQueryRequest>,
    closing: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()>
```

Spawns four tokio tasks under the returned outer JoinHandle:

1. **Publisher** — drains `publisher_rx` → `session.put(events_topic, bytes)`. Biased select on `publisher_rx.recv()` vs 1s closing-poll. Identical structure to `spawn_community_state_zenoh_adapter::pub_handle`.
2. **Subscriber** — `session.declare_subscriber(events_topic)` → forwards each sample's payload to `subscriber_tx`. Biased select on `sub.recv_async()`, `subscriber_tx.closed()`, 1s closing-poll. Identical structure to `spawn_community_state_zenoh_adapter::sub_handle`.
3. **Queryable** — `session.declare_queryable(query_topic_prefix)` where prefix is `harmony/channels/{cid}/{ch_id}/since/**`. For each query: parse key suffix `since/{hlc_hex}/{limit}` → ask the engine via a callback (or via shared registry-lookup; see §8.1) for events → for each event, encrypt with fresh nonce + reply via `query.reply(reply_key, packet_bytes)`.
4. **Query-request driver** — drains `query_request_rx` → for each `BackfillQueryRequest { since, limit }`, builds the query key + fires `session.get(...).await`. Each reply payload feeds into `subscriber_tx` (same path as live broadcast — symmetric verify chain). Periodically (every `backfill_progress_event_interval` events) emits `channel-backfill-progress` directly via the AppHandle.

```rust
/// Cross-task message: engine asks adapter to fire a Zenoh query on its behalf.
#[derive(Debug, Clone)]
pub struct BackfillQueryRequest {
    pub since: Option<Hlc>,                               // None = from earliest
    pub limit: usize,                                     // 0 = use server default
}
```

### 8.1 Queryable handler — engine access

The queryable handler needs read access to the engine's log. Two options were considered:
- (a) Pass an `Arc<ChannelLogEngine>` into the adapter — circular dependency risk (engine owns adapter handle, adapter owns engine handle).
- (b) Pass a callback `Arc<dyn Fn(...) -> Future<Output = Vec<SignedChannelEvent>>>` constructed at spawn time, capturing only the `Arc<Mutex<ChannelLog>>` and `Arc<ChannelKey>` (no engine cycle).

**Choice: (b).** Adapter holds only the inner data structures it needs to serve queries, not the engine itself. This breaks the cycle and keeps the adapter's lifetime independent of the engine wrapper.

### 8.2 Topic shapes (locked)

```
Live broadcast (sub + put):
    harmony/channels/{cid_hex}/{ch_id_hex}/events

Queryable (declare_queryable + get):
    harmony/channels/{cid_hex}/{ch_id_hex}/since/{hlc_hex}/{limit}

Where:
    cid_hex   = SpaceId hex (32 chars; 16 bytes)
    ch_id_hex = ChannelId hex (32 chars; 16 bytes)
    hlc_hex   = canonical HLC hex encoding (wall_ms LE u64 || logical LE u32 || device_id_bytes)
    limit     = u32 decimal, 0 means "use server default" (256)
```

Receiver of a queryable packet runs the same `decrypt_channel_packet` + `verify_channel_event` chain as live — backfill packets are wire-identical to live broadcasts (per Q1 decision §17.1).

## 9. IPC surface

All commands return camelCase DTOs via serde `rename_all = "camelCase"`, mirroring `RedeemInviteResultDto` and Phase 1's channel-config IPCs.

```rust
#[tauri::command]
async fn post_channel_message(
    app: AppHandle,
    community_id: String,                                 // hex SpaceId
    channel_id: String,                                   // hex ChannelId
    body: Vec<u8>,                                        // raw bytes; UI serializes display format
    reply_to: Option<String>,                             // hex MessageId
) -> Result<String, String>;                              // returns hex message_id

#[tauri::command]
async fn list_channel_messages(
    app: AppHandle,
    community_id: String,
    channel_id: String,
    since: Option<HlcDto>,                                // None = earliest available locally
    limit: u32,                                           // 0 = default 256; hard cap 1000
) -> Result<Vec<ChannelMessageDto>, String>;

#[tauri::command]
async fn request_channel_backfill(
    app: AppHandle,
    community_id: String,
    channel_id: String,
    since: Option<HlcDto>,
) -> Result<(), String>;                                  // fire-and-forget; results via channel-message-received
```

### 9.1 DTOs

```rust
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelMessageDto {
    pub message_id: String,                               // hex
    pub community_id: String,                             // hex
    pub channel_id: String,                               // hex
    pub author: String,                                   // hex OwnerAddr
    pub at: HlcDto,                                       // wall_ms + logical + device_id
    pub body: Vec<u8>,                                    // decrypted plaintext
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,                         // hex MessageId
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HlcDto {
    pub wall_ms: u64,
    pub logical: u32,
    pub device_id: String,
}
```

`HlcDto` already exists in `lib.rs` for other surfaces (e.g., `MemberInfoDto.joined_at`); reuse if present, otherwise extract.

### 9.2 IPC error mapping

Each IPC's `Err` arm calls `e.to_string()` on `ChannelLogEngineError`. The variant taxonomy:

```rust
#[derive(thiserror::Error, Debug)]
pub enum ChannelLogEngineError {
    #[error("community not found: {0}")]
    CommunityNotFound(SpaceId),
    #[error("channel not found in community: {0}")]
    ChannelNotFound(ChannelId),
    #[error("channel engine not running for {community_id}/{channel_id}")]
    EngineNotRunning { community_id: SpaceId, channel_id: ChannelId },
    #[error("publish failed: {0}")]
    PublishFailed(String),
    #[error("channel event invalid: {0}")]
    ChannelEvent(#[from] ChannelEventError),
    #[error("persist error: {0}")]
    Persist(#[from] ChannelLogPersistError),
    #[error("backfill request failed: {0}")]
    BackfillFailed(String),
    #[error("body too large: {len} bytes (max {max})")]
    BodyTooLarge { len: usize, max: usize },             // hard cap, e.g. 64 KiB; Phase 4 will surface this
    #[error("limit too large: {0} (max {max})")]
    LimitTooLarge { limit: u32, max: u32 },
}
```

## 10. Tauri events

```rust
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelMessageReceivedPayload {
    pub community_id: String,                             // hex
    pub channel_id: String,                               // hex
    pub message: ChannelMessageDto,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelBackfillProgressPayload {
    pub community_id: String,
    pub channel_id: String,
    pub fetched: u32,                                     // events received this backfill
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_estimate: Option<u32>,                      // None for v3 — Zenoh queryable doesn't pre-announce
}
```

Emission sites:
- `channel-message-received`: by `ChannelLogEngine::publish` (self-event) and by the receive loop (peer event), both via `app.emit(...)`.
- `channel-backfill-progress`: by the query-request driver task in the adapter, every `backfill_progress_event_interval` events (default 16) and once at the end (`fetched == total_for_request`, `total_estimate` still `None`).

## 11. Persistence layout

Phase 3 adds nothing to disk beyond what Phase 2 already created. Layout (per Phase 2):

```
<identity_dir>/communities/{cid_hex}/channels/{ch_id_hex}/
    manifest.cbor                                         (schema-versioned CBOR, V1 byte prefix)
    tail.cbor                                             (schema-versioned CBOR, V1 byte prefix)
    segments/
        00000000.cbor                                     (sealed segments, V1 byte prefix)
        00000001.cbor
        ...
```

Phase 3's flush loop drives the existing `ChannelLog::flush_tail` (writes tail.cbor atomically). When the tail crosses `seal_threshold_events`, the same loop calls `seal_and_persist` (rotates current tail into a sealed segment, updates manifest, atomically renames). All atomic-rename and path-validation behavior stays in the Phase 2 implementation.

## 12. Boot-time reconciliation

In `start_node` (lib.rs), after each `CommunitySyncRegistry::spawn_engine` returns successfully:

```rust
let community_state = engine.state().lock().await.clone();
let membership_key = engine.membership_key();              // exposed via Phase 1/2 plumbing
channel_log_registry.reconcile_from_state(
    community_id,
    &community_state,
    membership_key,
    /* deps */,
).await?;
```

This spawns a `ChannelLogEngine` for every live channel in the community's materialized state. Re-running is a no-op (per registry's idempotency contract).

## 13. Error handling

| Failure | Treatment | Why |
|---|---|---|
| `decrypt_channel_packet` returns Err on inbound packet | `tracing::warn!` + drop packet | Hostile peer can broadcast garbage; not user-visible |
| `verify_channel_event` returns `ChannelEventError::Replay` | `tracing::debug!` + drop | Common case (peer reconnects, re-broadcasts cached events); normal |
| `verify_channel_event` returns any other variant | `tracing::warn!` + drop | Could be hostile (bad signature, mismatched IDs) or genuine bug; surface to logs but don't UI |
| `ChannelLog::append` returns `Persist(e)` (disk full / permission) | Bubble to caller; emit `channel-log-degraded` Tauri event with `community_id`, `channel_id`, `reason` | Disk-full needs UI surface |
| `publisher_tx.send` returns Err (adapter task gone) | `tracing::error!` + emit degraded event; subsequent `publish` calls fail with `EngineNotRunning` | Engine is broken for this run; restart of community engine recovers |
| `subscriber_rx` closed | Receive loop exits cleanly; engine's incoming-events stop arriving but `publish` + `list_messages` still work (degraded mode) | Mirrors `community_state_sync` degraded reporting |
| Queryable handler errors during reply | `tracing::warn!` + skip that reply | Don't let one bad reply tear down the queryable |
| `request_backfill` Zenoh `session.get` times out (10 s default) | Emit `channel-backfill-progress { fetched: N }` final tick + return Ok (not Err) | Request was best-effort; UI may re-fire if 0 events received |

A `channel-log-degraded` event payload (mirroring `community-state-sync-degraded`):
```rust
#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ChannelLogDegradedPayload {
    pub community_id: String,
    pub channel_id: String,
    pub reason: String,                                   // human-readable
}
```

## 14. Testing strategy

### 14.1 Unit tests (in `community_channel_log_engine.rs::tests`)

Each test uses `ChannelLogEngineConfig { log_config: ChannelLogConfig { seal_threshold_events: 4 }, .. Default::default() }` for fast seal/reload exercise and a tighter debounce (`flush_debounce_ms: 10`) where timing is asserted.

| Test | Assertion |
|---|---|
| `engine_construct_shutdown_round_trip` | `new(...)` → `shutdown(...)` returns Ok; all internal tasks join cleanly |
| `publish_appends_locally_and_emits_event` | After `publish`, log contains the event; `app.emit` was called with `channel-message-received` carrying the right payload |
| `publish_writes_to_publisher_tx` | After `publish`, `publisher_rx` receives the encrypted packet bytes |
| `receive_garbage_packet_drops_silently` | Inject random bytes into `subscriber_tx`; assert no `app.emit`, no log mutation, debug log only |
| `receive_well_formed_packet_appends_and_emits` | Inject a properly-encrypted-and-signed event; assert log mutation + Tauri event |
| `receive_replay_drops_silently` | Inject same well-formed packet twice; assert one append + one emit |
| `list_messages_returns_hlc_ordered` | Append events out of order; `list_messages(None, 100)` returns them in HLC order |
| `list_messages_walks_tail_then_segments` | After 10 events with seal_threshold=4 (→ 2 segments + 2 in tail), `list_messages` returns all 10 |
| `flush_debounce_coalesces_burst` | 5 rapid appends → exactly 1 disk write after `flush_debounce_ms`; assert via spy on `flush_tail` |
| `flush_max_dirty_forces_under_continuous_load` | Append every `flush_debounce_ms / 2` indefinitely; assert `flush_tail` fires by `max_dirty_ms` |
| `flush_now_bypasses_debounce` | After `publish`, immediate `flush_now` writes synchronously |
| `seal_threshold_triggers_seal` | Append `seal_threshold_events + 1` events; assert manifest has a new segment, tail is shorter |
| `registry_spawn_idempotent` | Two `spawn` calls for same `(cid, chid)` return same `Arc` |
| `registry_stop_discards_entry` | `spawn` then `stop`; subsequent `engine(cid, chid)` returns `None` |
| `registry_reconcile_skips_deleted_channels` | `CommunityState` with one live + one tombstoned channel → only live spawns |
| `registry_reconcile_idempotent` | Run twice with same state; second call is no-op (no double-spawn) |

### 14.2 Integration test (`tests/community_channel_messages_integration.rs`)

Single test `two_engines_live_then_offline_backfill_with_replay_rejection`:

1. Set up two `CommunitySyncEngine` + `ChannelLogRegistry` pairs, A and B, joined via shared in-memory Zenoh router (per existing pattern in `community_sync_integration.rs::build_fixture`).
2. A creates community + default `#general` channel; B redeems invite, joins, materializes, registry on B spawns its engine for `#general`.
3. **Phase live**: A.engine(#general).publish 100 messages (small bodies, e.g., `format!("msg {}", i).into_bytes()`). Assert B receives 100 `channel-message-received` events in order via a tokio::sync::mpsc subscribed to `app.listen("channel-message-received")`.
4. **Phase offline**: drop B's adapter publisher_rx (or call `registry.stop` then re-spawn without subscriber); A publishes 50 more messages. Assert B's emit-counter stays at 100.
5. **Phase backfill**: re-spawn B's engine; B.request_backfill(since=None). Assert B receives the missing 50 events via `channel-message-received`, plus the original 100 are NOT re-emitted (deduped against existing log entries by `message_id`). Assert at least one `channel-backfill-progress` event fires during backfill.
6. **Replay attack**: capture one packet from A's publisher_tx during phase 1 (snapshot). Manually inject it into B's subscriber_tx after backfill completes. Assert no additional `channel-message-received` event (replay tracker rejects).
7. Assert final state: B's log contains all 150 events in HLC order; `list_messages(None, 200)` returns 150 events.

Uses `seal_threshold_events: 8` so the 100 + 50 = 150 events produce ≥ 18 sealed segments — exercises seal/reload paths.

### 14.3 Wire-format pin extension

In `tests/wire_format_channel_log_fixtures.rs`, add `backfill_reply_packet_wire_bytes_pinned` test:
- Build a `SignedChannelEvent::Post` with deterministic seeds (mirror existing test fixture)
- Call `encrypt_channel_packet(channel_key, &event, fixed_nonce)` — note: this requires the test to bypass `encrypt_channel_packet`'s random nonce and call a deterministic helper or supply the nonce
- Assert the packet bytes match a literal hex pin

This drift-guards the backfill reply format. If Phase 4 or later changes packet shape silently, this test catches it.

## 15. Acceptance criteria (mirror ZEB-270 ticket)

1. `ChannelLogEngine` exposes `publish`, `list_messages`, `request_backfill`, `flush_now`, `shutdown` with shapes per §6.2.
2. `ChannelLogRegistry` exposes `spawn`, `stop`, `engine`, `reconcile_from_state`, `shutdown_all` with shapes per §7.2; idempotent on reload (verified by unit test).
3. Lifecycle binding: `ChannelConfigChangeAction::Created` → registry spawn; `Deleted` → registry stop; `Modified` → no-op (verified by integration test).
4. Three IPCs registered: `post_channel_message`, `list_channel_messages`, `request_channel_backfill`. Round-trippable via `tauri::invoke`. DTOs use camelCase serde.
5. Two Tauri events emitted with payloads per §10. Integration test asserts both fire on the right boundary.
6. Two-engine integration test passes per §14.2.
7. `cargo fmt --all -- --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test --workspace --no-fail-fast` all green.
8. PR cuts from `origin/main` `3ac6671`. Single PR for Phase 3.

## 16. Cross-repo

None — entirely in `harmony-client`.

## 17. Plan-time decisions (locked)

These resolve the five questions left open by the parent spec §15.

### 17.1 Backfill response shape: per-event packets

Each Zenoh queryable reply carries exactly one `ChannelKey`-encrypted `SignedChannelEvent`. Same wire format as a live broadcast packet — receiver runs the same `decrypt_channel_packet` + `verify_channel_event` chain on backfill replies as on live ones.

**Why:** Symmetry with live broadcast > Zenoh per-reply efficiency. Receive code is one path, not two; replay tracker behavior is uniform; AAD binding is per-event everywhere. 100 events backfilled = 100 Zenoh replies — well under any saturation threshold for v3 scale.

### 17.2 Backfill rate-limit / batching: minimal posture

- **Server-side:** no concurrency cap on the queryable handler. Trust the `limit` parameter (default 256, hard cap 1000) to bound each reply volume. Add `tracing::info!` metrics on query rate per channel for future visibility.
- **Requester-side:** Zenoh's built-in query timeout (10 s default) is the failure mode. No auto-retry — IPC `request_channel_backfill` returns Ok regardless and the UI re-fires if needed.

**Why:** v3 scale (friends-and-family, low channel count, low concurrent peer count) doesn't need bigger guards. YAGNI. Add concurrency caps in v4 if metrics show saturation.

### 17.3 Tail flush cadence: 250 ms debounce, 1 s max-dirty cap

- `flush_debounce_ms: u64 = 250` — matches `community_state_sync::DEFAULT_DEBOUNCE_MS = 250`.
- `max_dirty_ms: u64 = 1000` — hard cap on continuous append; force flush after 1 s regardless of debounce activity.
- `flush_now()` exposed for shutdown.

**Why:** Match project precedent. Consistent debounce semantics across the two engine layers means one mental model for "from append to durable." Bounded data loss at ~1 s of messages on crash.

### 17.4 Registry storage of stopped engines: discard cleanly

`registry.stop(channel_id)` calls `engine.shutdown()` (which calls `flush_now`), then drops the `Arc` from the HashMap. No tombstone descriptor retained.

**Why:** The materialized `CommunityState.channels` map is the single source of truth for which channels exist. `reconcile_from_state` filters `deleted_at.is_some()` and never spawns those — so the registry doesn't need its own tombstone tracking. On-disk segments persist (for the breadcrumb-render use case in parent spec §12.1).

### 17.5 Sealed-segment threshold for tests: separate `ChannelLogEngineConfig` wrapping `ChannelLogConfig`

```rust
pub struct ChannelLogEngineConfig {
    pub log_config: ChannelLogConfig,                     // Phase 2 (seal_threshold_events lives here)
    pub flush_debounce_ms: u64,
    pub max_dirty_ms: u64,
    pub backfill_default_limit: usize,
    pub backfill_progress_event_interval: usize,
}
```

Tests that need a small seal threshold construct `ChannelLogEngineConfig { log_config: ChannelLogConfig { seal_threshold_events: 8 }, .. Default::default() }`.

**Why:** Layered config keeps Phase 2 primitives unaware of Phase 3 timing tunables. Mirrors `CommunitySyncEngineConfig` wrapping pattern. Tests of Phase 2 stay unchanged.

## 18. References

- **Parent spec:** `docs/specs/2026-05-09-zeb-248-channels-within-communities-design.md` (commit `5145484`) — §9, §10, §11, §15
- **Parent ticket:** [ZEB-248](https://linear.app/zeblith/issue/ZEB-248)
- **Sibling Phase 1:** [ZEB-266](https://linear.app/zeblith/issue/ZEB-266), merged via PR #93 (2026-05-09)
- **Sibling Phase 2:** [ZEB-269](https://linear.app/zeblith/issue/ZEB-269), merged via PR #95 (2026-05-09)
- **Sibling cross-cutting refactor:** [ZEB-267](https://linear.app/zeblith/issue/ZEB-267), merged via PR #94 (2026-05-09)
- **Predecessor:** [ZEB-217](https://linear.app/zeblith/issue/ZEB-217) (Sub-C v1)
- **Codebase patterns:**
  - `src-tauri/src/community_state_sync.rs` — `CommunitySyncEngine` + `CommunitySyncRegistry` shape this mirrors
  - `src-tauri/src/event_loop.rs:2263` — `spawn_community_state_zenoh_adapter` shape this mirrors
  - `src-tauri/src/lib.rs:1230-1300` — `run_community_delta_consumer` extension point
  - `src-tauri/src/community_channel_log.rs` — Phase 2 primitives this wraps
  - `src-tauri/src/dm_outbox.rs` — debounce-flush pattern reference
