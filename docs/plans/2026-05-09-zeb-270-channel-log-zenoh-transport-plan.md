# ZEB-270 ChannelLog Zenoh Transport Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the in-process ChannelLog primitives (Phase 2 / ZEB-269) to a per-channel Zenoh broadcast + queryable transport, plus the `ChannelLogRegistry` lifecycle bound to channel-config materialize, plus three message-surface IPCs and two Tauri events.

**Architecture:** A new `ChannelLogEngine` per `(community_id, channel_id)` owns the Phase 2 `ChannelLog` + replay tracker + crypto, exposes `publish` / `list_messages` / `request_backfill` / `flush_now` / `shutdown`, and communicates with Zenoh through three mpsc channel pairs (publisher, subscriber, query-request) plus a queryable callback. A new `spawn_channel_log_zenoh_adapter` in `event_loop.rs` (mirroring `spawn_community_state_zenoh_adapter`) owns the four corresponding tokio tasks and is the only thing that touches `zenoh::Session`. A new `ChannelLogRegistry` (mirroring `CommunitySyncRegistry`) hooks into the existing `run_community_delta_consumer` callback chain — `ChannelConfigChangeAction::Created` triggers `spawn`, `Deleted` triggers `stop`, `Modified` is a no-op for the registry.

**Tech Stack:** Rust 2021, tokio 1.x, Zenoh 1.x (`zenoh::Session`), Tauri 2 (`#[tauri::command]`), CBOR via `ciborium`, `ed25519_dalek::SigningKey` for signing, ChaCha20-Poly1305 + HKDF-SHA256 (Phase 2 primitives reused unchanged).

---

## Spec reference

Full design at `docs/specs/2026-05-09-zeb-270-channel-log-zenoh-transport-design.md` (commit `e8c987d`). Five plan-time decisions are locked in spec §17:

1. **§17.1** — Backfill replies are per-event packets (wire-identical to live broadcast)
2. **§17.2** — No server-side concurrency cap; rely on `limit` parameter; no requester auto-retry
3. **§17.3** — 250 ms tail flush debounce + 1 s max-dirty cap (matches `community_state_sync::DEFAULT_DEBOUNCE_MS`)
4. **§17.4** — Registry stops engine and discards entry; no in-memory tombstones; on-disk segments persist
5. **§17.5** — Layered `ChannelLogEngineConfig { log_config: ChannelLogConfig, ..tunables }`

---

## File map

| File | Action | Responsibility |
|---|---|---|
| `src-tauri/src/community_channel_log_engine.rs` | **CREATE** | `ChannelLogEngine`, `ChannelLogRegistry`, configs, error type, `BackfillQueryRequest`, all unit tests |
| `src-tauri/src/lib.rs` | **MODIFY** | `pub mod community_channel_log_engine;` declaration; 3 IPC functions; 3 DTOs; extend `run_community_delta_consumer` signature with 3rd callback; wire registry into `start_node`; register IPCs in `tauri::Builder::invoke_handler` |
| `src-tauri/src/event_loop.rs` | **MODIFY** | `spawn_channel_log_zenoh_adapter` next to `spawn_community_state_zenoh_adapter` (publisher + subscriber + queryable + query-request driver tasks) |
| `src-tauri/tests/community_channel_messages_integration.rs` | **CREATE** | Two-engine integration test (live broadcast + offline-then-backfill + replay rejection) |
| `src-tauri/tests/wire_format_channel_log_fixtures.rs` | **MODIFY** | Add `backfill_reply_packet_wire_bytes_pinned` test |

---

## Type imports cheat sheet

The implementer should use these canonical paths in the new module:

```rust
use crate::community_channel_log::{
    ChannelEventError, ChannelLog, ChannelLogConfig, ChannelLogPersistError,
    ChannelLogReplayTracker, ChannelIdentityResolver, ChannelKey,
    CommunityStateAtHlc, MessageId, SignedChannelEvent,
    decrypt_channel_packet, derive_channel_key, encrypt_channel_packet,
    sign_channel_event, verify_channel_event,
};
use crate::community_membership::ChannelId;
use crate::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};
use crate::community_state_sync::CommunityRootHlcTracker;
```

Tauri runtime parameterization follows the existing convention at `src-tauri/src/lib.rs:51`: `tauri::AppHandle<R: tauri::Runtime>`.

---

## Pre-task checklist

Before invoking subagent-driven-development:

- [ ] Confirm working tree clean (`git status` shows only the spec + this plan)
- [ ] Confirm branch is `zeb-270-channel-log-zenoh-transport`, base `3ac6671`
- [ ] Confirm `~/.claude/projects/-Users-zeblith-work/memory/MEMORY.md` rules apply (no worktrees, cargo gates, pipe exit codes, etc.)

---

## Task 0: Pre-flight + green-baseline confirm

**Files:** none

**Goal:** Verify the just-cut branch starts green so any later red is unambiguously our doing.

- [ ] **Step 1: Confirm branch**

```bash
git branch --show-current
```

Expected: `zeb-270-channel-log-zenoh-transport`

- [ ] **Step 2: Confirm working tree clean**

```bash
git status --short
```

Expected: empty output (the spec + plan are already committed).

- [ ] **Step 3: cargo fmt baseline**

```bash
cd src-tauri && cargo fmt --all -- --check
```

Expected: zero output, exit 0.

- [ ] **Step 4: cargo clippy baseline**

```bash
cd src-tauri && cargo clippy --all-targets -- -D warnings
```

Expected: build succeeds, zero warnings.

- [ ] **Step 5: cargo test baseline**

```bash
cd src-tauri && cargo test --workspace --no-fail-fast 2>&1 | tee /tmp/zeb270-task0-test.log
RESULT=${PIPESTATUS[0]}
test "$RESULT" -eq 0 && echo "BASELINE GREEN" || echo "BASELINE RED ($RESULT)"
```

Expected: `BASELINE GREEN` and "test result: ok" lines for every test target.

If any of Steps 3–5 fail, STOP. The branch is not actually based on a green tree; investigate before continuing. Per `feedback_test_drift_is_our_fault`, do NOT externalize. File a separate fixup if needed.

**No commit.** Task 0 is pure verification.

---

## Task 1: ChannelLog engine module skeleton

**Files:**
- Create: `src-tauri/src/community_channel_log_engine.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod community_channel_log_engine;` declaration)
- Test: in-file `#[cfg(test)] mod tests` of the new module

**Goal:** Establish the module shell — error type, config structs, params struct, engine struct shell with `new()` and `shutdown()` methods. Smoke test: construct engine, immediately shutdown, assert clean.

- [ ] **Step 1: Locate insertion point in lib.rs**

```bash
grep -n "^pub mod community_channel_log;" src-tauri/src/lib.rs
```

Expected: one line. Note the line number — we insert the new module declaration immediately after it (alphabetical order).

- [ ] **Step 2: Add module declaration to lib.rs**

In `src-tauri/src/lib.rs`, after the line `pub mod community_channel_log;`, add:

```rust
pub mod community_channel_log_engine;
```

- [ ] **Step 3: Create the new module file with skeleton**

Create `src-tauri/src/community_channel_log_engine.rs` containing:

```rust
//! ZEB-270 Phase 3: ChannelLog Zenoh transport engine.
//!
//! Wraps the in-process Phase 2 (ZEB-269) `ChannelLog` primitives with
//! Zenoh broadcast + queryable backfill plus the per-(community,
//! channel) lifecycle binding to channel-config materialize.
//!
//! See `docs/specs/2026-05-09-zeb-270-channel-log-zenoh-transport-design.md`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ed25519_dalek::SigningKey;
use thiserror::Error;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::JoinHandle;

use crate::community_channel_log::{
    ChannelEventError, ChannelLog, ChannelLogConfig, ChannelLogPersistError,
    ChannelLogReplayTracker, ChannelIdentityResolver, ChannelKey, CommunityStateAtHlc,
    MessageId, SignedChannelEvent,
};
use crate::community_membership::ChannelId;
use crate::community_state_sync::CommunityRootHlcTracker;
use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

// ── Error type ──────────────────────────────────────────────────────────────

#[derive(Error, Debug)]
pub enum ChannelLogEngineError {
    #[error("community not found: {0:?}")]
    CommunityNotFound(SpaceId),

    #[error("channel not found in community: {0:?}")]
    ChannelNotFound(ChannelId),

    #[error("channel engine not running for {community_id:?}/{channel_id:?}")]
    EngineNotRunning {
        community_id: SpaceId,
        channel_id: ChannelId,
    },

    #[error("publish failed: {0}")]
    PublishFailed(String),

    #[error("channel event invalid: {0}")]
    ChannelEvent(#[from] ChannelEventError),

    #[error("persist error: {0}")]
    Persist(#[from] ChannelLogPersistError),

    #[error("backfill request failed: {0}")]
    BackfillFailed(String),

    #[error("body too large: {len} bytes (max {max})")]
    BodyTooLarge { len: usize, max: usize },

    #[error("limit too large: {limit} (max {max})")]
    LimitTooLarge { limit: u32, max: u32 },
}

// ── Config + params ─────────────────────────────────────────────────────────

/// Per-engine tunables. Wraps Phase 2's `ChannelLogConfig` so that
/// Phase 2 unit tests stay unaware of Phase 3 timing knobs.
#[derive(Clone, Debug)]
pub struct ChannelLogEngineConfig {
    /// Phase 2 log config (seal threshold etc.). Tests override
    /// `seal_threshold_events` (e.g., to 8) to exercise seal/reload
    /// paths in reasonable time.
    pub log_config: ChannelLogConfig,

    /// Sliding tail-flush debounce window (ms). Default 250 to match
    /// `community_state_sync::DEFAULT_DEBOUNCE_MS`.
    pub flush_debounce_ms: u64,

    /// Hard cap on continuous-append starvation: force flush after
    /// this many ms since the first dirty append, regardless of
    /// debounce activity. Default 1000.
    pub max_dirty_ms: u64,

    /// Default `limit` value when an IPC `request_channel_backfill`
    /// passes 0. Default 256.
    pub backfill_default_limit: usize,

    /// Emit a `channel-backfill-progress` Tauri event every N events
    /// received during a backfill. Default 16.
    pub backfill_progress_event_interval: usize,
}

impl Default for ChannelLogEngineConfig {
    fn default() -> Self {
        Self {
            log_config: ChannelLogConfig::default(),
            flush_debounce_ms: 250,
            max_dirty_ms: 1000,
            backfill_default_limit: 256,
            backfill_progress_event_interval: 16,
        }
    }
}

/// Cross-task message: engine asks adapter to fire a Zenoh query on
/// its behalf. Per spec §8 — engine cannot touch `zenoh::Session`
/// directly, so backfill requests cross the boundary as messages.
#[derive(Debug, Clone)]
pub struct BackfillQueryRequest {
    /// `None` means "from the earliest available".
    pub since: Option<Hlc>,
    /// `0` means "use server default" (`backfill_default_limit`).
    pub limit: usize,
}

/// Bundles per-instance dependencies + I/O channel endpoints + the
/// tunables config. Consumed by `ChannelLogEngine::new`. The other
/// ends of the three channel pairs are owned by the adapter spawned
/// by `ChannelLogRegistry::spawn` (see Task 4).
pub struct ChannelLogEngineParams<R: tauri::Runtime> {
    pub community_id: SpaceId,
    pub channel_id: ChannelId,
    pub channel_key: Arc<ChannelKey>,
    pub root_dir: PathBuf,
    pub state_at_hlc: Arc<dyn CommunityStateAtHlc + Send + Sync>,
    pub resolver: Arc<dyn ChannelIdentityResolver + Send + Sync>,
    pub self_owner: OwnerAddr,
    pub self_device_id: String,
    pub signing_key: Arc<SigningKey>,
    pub hlc_tracker: Arc<Mutex<CommunityRootHlcTracker>>,
    pub app: tauri::AppHandle<R>,
    pub config: ChannelLogEngineConfig,

    /// Publisher channel (engine → adapter → Zenoh `put`).
    pub publisher_tx: mpsc::Sender<Vec<u8>>,
    /// Subscriber channel (Zenoh subscriber → adapter → engine receive loop).
    pub subscriber_rx: mpsc::Receiver<Vec<u8>>,
    /// Backfill query-request channel (engine → adapter → Zenoh `get`).
    pub query_request_tx: mpsc::Sender<BackfillQueryRequest>,
}

// ── Engine ──────────────────────────────────────────────────────────────────

pub struct ChannelLogEngine<R: tauri::Runtime> {
    community_id: SpaceId,
    channel_id: ChannelId,
    channel_key: Arc<ChannelKey>,
    log: Arc<Mutex<ChannelLog>>,
    replay_tracker: Arc<Mutex<ChannelLogReplayTracker>>,
    state_at_hlc: Arc<dyn CommunityStateAtHlc + Send + Sync>,
    resolver: Arc<dyn ChannelIdentityResolver + Send + Sync>,
    self_owner: OwnerAddr,
    self_device_id: String,
    signing_key: Arc<SigningKey>,
    hlc_tracker: Arc<Mutex<CommunityRootHlcTracker>>,
    app: tauri::AppHandle<R>,
    config: ChannelLogEngineConfig,

    publisher_tx: mpsc::Sender<Vec<u8>>,
    query_request_tx: mpsc::Sender<BackfillQueryRequest>,

    receive_handle: Mutex<Option<JoinHandle<()>>>,
    flush_handle: Mutex<Option<JoinHandle<()>>>,

    flush_dirty: Arc<Notify>,
    closing: Arc<AtomicBool>,
}

impl<R: tauri::Runtime> ChannelLogEngine<R> {
    /// Construct the engine. In Task 1 this is a stub that just
    /// stores the params and DOES NOT spawn any background tasks.
    /// Task 2 fills in the receive + flush loops.
    pub async fn new(
        params: ChannelLogEngineParams<R>,
    ) -> Result<Arc<Self>, ChannelLogEngineError> {
        let log = ChannelLog::new(
            params.community_id,
            params.channel_id,
            params.root_dir,
            params.config.log_config.clone(),
        )?;

        Ok(Arc::new(Self {
            community_id: params.community_id,
            channel_id: params.channel_id,
            channel_key: params.channel_key,
            log: Arc::new(Mutex::new(log)),
            replay_tracker: Arc::new(Mutex::new(ChannelLogReplayTracker::new())),
            state_at_hlc: params.state_at_hlc,
            resolver: params.resolver,
            self_owner: params.self_owner,
            self_device_id: params.self_device_id,
            signing_key: params.signing_key,
            hlc_tracker: params.hlc_tracker,
            app: params.app,
            config: params.config,
            publisher_tx: params.publisher_tx,
            query_request_tx: params.query_request_tx,
            receive_handle: Mutex::new(None),
            flush_handle: Mutex::new(None),
            flush_dirty: Arc::new(Notify::new()),
            closing: Arc::new(AtomicBool::new(false)),
        }))
    }

    /// Signal closing + join all internal tasks. In Task 1 there are
    /// no tasks so this is just the closing-flag flip; Task 2 adds
    /// flush_now + join logic.
    pub async fn shutdown(&self) -> Result<(), ChannelLogEngineError> {
        self.closing.store(true, Ordering::SeqCst);
        self.flush_dirty.notify_one();

        if let Some(handle) = self.receive_handle.lock().await.take() {
            let _ = handle.await;
        }
        if let Some(handle) = self.flush_handle.lock().await.take() {
            let _ = handle.await;
        }
        Ok(())
    }

    pub fn community_id(&self) -> SpaceId {
        self.community_id
    }

    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: constructing an engine with no I/O traffic and
    /// immediately shutting it down works end-to-end.
    #[tokio::test]
    async fn engine_construct_shutdown_round_trip() {
        // Test fixture deferred to Task 2 — Task 1 only verifies the
        // skeleton compiles. This test currently asserts the type
        // names exist.
        // (Task 2 expands this with a real fixture and real I/O.)
        let _ = std::any::type_name::<ChannelLogEngineConfig>();
        let _ = std::any::type_name::<ChannelLogEngineError>();
        let _ = std::any::type_name::<BackfillQueryRequest>();
    }
}
```

- [ ] **Step 4: Verify the module compiles**

```bash
cd src-tauri && cargo build 2>&1 | tee /tmp/zeb270-task1-build.log
RESULT=${PIPESTATUS[0]}
test "$RESULT" -eq 0 && echo "BUILD OK" || echo "BUILD FAILED ($RESULT)"
```

Expected: `BUILD OK`. If type-import paths are off, fix them by re-checking against `src-tauri/src/community_channel_log.rs` (Phase 2 module's imports are the canonical reference).

- [ ] **Step 5: Run the smoke test**

```bash
cd src-tauri && cargo test --lib community_channel_log_engine -- --nocapture 2>&1 | tee /tmp/zeb270-task1-test.log
RESULT=${PIPESTATUS[0]}
test "$RESULT" -eq 0 && echo "TEST OK" || echo "TEST FAILED ($RESULT)"
```

Expected: `engine_construct_shutdown_round_trip ... ok` and `TEST OK`.

- [ ] **Step 6: Verify gates green**

```bash
cd src-tauri && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && echo "GATES GREEN"
```

Expected: `GATES GREEN`. (Note: clippy may complain about unused fields like `state_at_hlc`, `resolver`, `self_device_id`, etc. — Task 2 uses them. To suppress for Task 1 only, add `#[allow(dead_code)]` to the `ChannelLogEngine` struct and remove it in Task 2.)

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/community_channel_log_engine.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-270): ChannelLog engine module skeleton

Establishes the Phase 3 module shell — ChannelLogEngine struct,
ChannelLogEngineConfig (wrapping Phase 2's ChannelLogConfig per spec
§17.5), ChannelLogEngineParams, BackfillQueryRequest, ChannelLogEngineError.
new() + shutdown() are stubs; receive + flush + publish + list_messages
land in Task 2. Smoke test asserts the type surface.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: ChannelLogEngine internals — receive, flush, publish, list_messages

**Files:**
- Modify: `src-tauri/src/community_channel_log_engine.rs`

**Goal:** Implement the four core engine behaviors and unit-test each per spec §14.1.

This task is the largest in the plan. Work TDD-shape — write the test, watch it fail, implement, watch it pass — and commit only at the end.

### Pre-work: build the test fixture helper

- [ ] **Step 1: Add a test-fixture builder at the top of the `tests` module**

Add at the start of the `#[cfg(test)] mod tests` block:

```rust
use ed25519_dalek::{SigningKey, SECRET_KEY_LENGTH};
use harmony_identity::PrivateIdentity;
use std::time::Duration;
use tempfile::TempDir;

/// Stub state-at-HLC: returns Joined for every (author, hlc) we ask
/// about. Sufficient for unit tests that don't exercise verify-chain
/// edge cases (those live in Phase 2's tests).
struct AlwaysJoinedState;

impl CommunityStateAtHlc for AlwaysJoinedState {
    fn is_joined(&self, _author: &OwnerAddr, _at: &Hlc) -> bool {
        true
    }

    fn write_power_for(&self, _author: &OwnerAddr, _at: &Hlc) -> u8 {
        100  // above any reasonable channel write_power threshold
    }
}

/// Stub identity resolver: returns the ed25519 + x25519 composite for
/// a fixed test identity. Phase 2 uses `harmony_identity::PrivateIdentity::from_seed`
/// to build deterministic identities — we mirror that here.
struct FixedIdentityResolver {
    map: std::collections::HashMap<OwnerAddr, [u8; 64]>,
}

impl ChannelIdentityResolver for FixedIdentityResolver {
    fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        self.map.get(addr).copied()
    }
}

/// Build a ChannelLogEngine with deterministic test seeds and an
/// in-memory channel-key. Returns the engine plus the harness's
/// other-end channels so tests can drive I/O directly without Zenoh.
struct EngineFixture<R: tauri::Runtime> {
    engine: Arc<ChannelLogEngine<R>>,
    publisher_rx: mpsc::Receiver<Vec<u8>>,
    subscriber_tx: mpsc::Sender<Vec<u8>>,
    query_request_rx: mpsc::Receiver<BackfillQueryRequest>,
    self_owner: OwnerAddr,
    signing_key: Arc<SigningKey>,
    channel_key: Arc<ChannelKey>,
    community_id: SpaceId,
    channel_id: ChannelId,
    _tmp: TempDir,
}

async fn build_engine_fixture(
    seal_threshold: usize,
    flush_debounce_ms: u64,
    max_dirty_ms: u64,
) -> EngineFixture<tauri::test::MockRuntime> {
    let tmp = TempDir::new().expect("tempdir");

    // Deterministic identity from a fixed seed.
    let mut seed = [0u8; 32];
    seed[0] = 0x42;
    let identity = PrivateIdentity::from_seed(&seed).expect("identity");
    let self_owner = OwnerAddr(identity.public_addr());
    let signing_key = Arc::new(SigningKey::from_bytes(
        identity.ed25519_signing_key_bytes()
            .as_slice()
            .try_into()
            .expect("ed25519 32B"),
    ));

    let community_id = SpaceId([0xc1; 16]);
    let channel_id = ChannelId([0xch; 16]);
    let membership_key = MembershipKey([0x77; 32]);

    let channel_key = Arc::new(derive_channel_key(
        &membership_key,
        &community_id,
        &channel_id,
    ));

    let mut resolver_map = std::collections::HashMap::new();
    resolver_map.insert(self_owner, identity.public_bytes_composite());
    let resolver = Arc::new(FixedIdentityResolver { map: resolver_map });

    let state = Arc::new(AlwaysJoinedState);

    let hlc_tracker = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));

    let (publisher_tx, publisher_rx) = mpsc::channel(64);
    let (subscriber_tx, subscriber_rx) = mpsc::channel(64);
    let (query_request_tx, query_request_rx) = mpsc::channel(8);

    let app = tauri::test::mock_app().handle().clone();

    let config = ChannelLogEngineConfig {
        log_config: ChannelLogConfig {
            seal_threshold_events: seal_threshold,
        },
        flush_debounce_ms,
        max_dirty_ms,
        ..Default::default()
    };

    let params = ChannelLogEngineParams {
        community_id,
        channel_id,
        channel_key: Arc::clone(&channel_key),
        root_dir: tmp.path().to_path_buf(),
        state_at_hlc: state,
        resolver,
        self_owner,
        self_device_id: "test-device".to_string(),
        signing_key: Arc::clone(&signing_key),
        hlc_tracker,
        app,
        config,
        publisher_tx,
        subscriber_rx,
        query_request_tx,
    };

    let engine = ChannelLogEngine::new(params).await.expect("engine new");

    EngineFixture {
        engine,
        publisher_rx,
        subscriber_tx,
        query_request_rx,
        self_owner,
        signing_key,
        channel_key,
        community_id,
        channel_id,
        _tmp: tmp,
    }
}
```

Note the import additions you need at the top of the file:

```rust
use crate::owner_state_types::MembershipKey;
use crate::community_channel_log::derive_channel_key;
```

If `tempfile` is not in `dev-dependencies`, add it to `src-tauri/Cargo.toml`:

```toml
[dev-dependencies]
tempfile = "3"
```

(Check first; Phase 2 likely already added it for the channel-log persistence tests.)

- [ ] **Step 2: Verify the fixture compiles**

```bash
cd src-tauri && cargo test --lib community_channel_log_engine -- --nocapture 2>&1 | head -60
```

Expected: build succeeds. Any compile error here points to a mismatched type signature — fix by re-checking against the actual definitions in Phase 2 (`community_channel_log.rs`) and the resolver/state-at-hlc traits.

### Sub-task 2A: list_messages

- [ ] **Step 3: Write the failing test for `list_messages` returning empty when log is empty**

In the same `tests` module, add:

```rust
#[tokio::test]
async fn list_messages_empty_log_returns_empty() {
    let fix = build_engine_fixture(8, 250, 1000).await;
    let msgs = fix
        .engine
        .list_messages(None, 100)
        .await
        .expect("list");
    assert!(msgs.is_empty());
}
```

- [ ] **Step 4: Run; expect compile error (`list_messages` does not exist yet)**

```bash
cd src-tauri && cargo test --lib community_channel_log_engine::tests::list_messages_empty 2>&1 | tail -20
```

Expected: `error[E0599]: no method named 'list_messages'` or similar.

- [ ] **Step 5: Implement `list_messages`**

Add to `impl<R: tauri::Runtime> ChannelLogEngine<R>`:

```rust
/// Read events in HLC order from tail + segments back to `since`.
/// `since=None` means "from the earliest available locally".
/// Returns at most `limit` events; `limit=0` falls back to
/// `config.backfill_default_limit`.
pub async fn list_messages(
    &self,
    since: Option<Hlc>,
    limit: usize,
) -> Result<Vec<SignedChannelEvent>, ChannelLogEngineError> {
    let effective_limit = if limit == 0 {
        self.config.backfill_default_limit
    } else {
        limit
    };

    let log = self.log.lock().await;

    // Phase 2 stores events in `log.tail` (newest, in-memory) +
    // sealed segments referenced by `log.manifest.segments` (older,
    // on-disk). For correct HLC-order iteration we walk segments
    // first, then tail.
    let mut out: Vec<SignedChannelEvent> = Vec::new();

    for seg in &log.manifest.segments {
        if let Some(since_hlc) = &since {
            // Skip segments entirely older than `since`.
            // Phase 2's SegmentDescriptor exposes `last_hlc` (or
            // similar field) — if last_hlc < since, skip.
            if seg.last_hlc.is_strictly_older_than(since_hlc) {
                continue;
            }
        }
        let events = log.read_segment(seg).map_err(ChannelLogEngineError::Persist)?;
        for ev in events {
            if let Some(since_hlc) = &since {
                if !ev.at().is_strictly_newer_than(since_hlc) {
                    continue;
                }
            }
            out.push(ev);
            if out.len() >= effective_limit {
                return Ok(out);
            }
        }
    }

    // Then walk in-memory tail.
    for ev in &log.tail {
        if let Some(since_hlc) = &since {
            if !ev.at().is_strictly_newer_than(since_hlc) {
                continue;
            }
        }
        out.push(ev.clone());
        if out.len() >= effective_limit {
            return Ok(out);
        }
    }

    Ok(out)
}
```

(If Phase 2's `SegmentDescriptor` field naming differs — `first_hlc/last_hlc` vs `range_start/range_end` etc. — match it. Same for `SignedChannelEvent::at()` accessor. Inspect `community_channel_log.rs` for the actual names and adapt.)

- [ ] **Step 6: Run; expect pass**

```bash
cd src-tauri && cargo test --lib community_channel_log_engine::tests::list_messages_empty 2>&1 | tail -10
```

Expected: `test result: ok. 1 passed`.

- [ ] **Step 7: Add a non-empty `list_messages` test**

```rust
#[tokio::test]
async fn list_messages_returns_hlc_ordered() {
    let fix = build_engine_fixture(8, 250, 1000).await;

    // Construct three SignedChannelEvent::Post events with strictly
    // increasing HLCs, append directly via the engine's log.
    let bodies = [b"first".to_vec(), b"second".to_vec(), b"third".to_vec()];
    let mut events = Vec::new();
    for (i, body) in bodies.iter().enumerate() {
        let hlc = Hlc {
            wall_ms: 1_000 + i as u64,
            logical: 0,
            device_id: fix.engine.self_device_id_for_test().to_string(),
        };
        let ev = sign_channel_event(
            fix.community_id,
            fix.channel_id,
            fix.self_owner,
            hlc,
            body.clone(),
            None,
            &fix.signing_key,
        )
        .expect("sign");
        events.push(ev);
    }

    {
        let mut log = fix.engine.log_for_test().lock().await;
        for ev in &events {
            log.append(ev.clone()).expect("append");
        }
    }

    let listed = fix.engine.list_messages(None, 100).await.expect("list");
    assert_eq!(listed.len(), 3);
    for (got, want) in listed.iter().zip(events.iter()) {
        assert_eq!(got.id(), want.id());
    }
}
```

This requires test-only accessors on the engine. Add them:

```rust
#[cfg(test)]
impl<R: tauri::Runtime> ChannelLogEngine<R> {
    pub(crate) fn log_for_test(&self) -> &Arc<Mutex<ChannelLog>> {
        &self.log
    }

    pub(crate) fn self_device_id_for_test(&self) -> &str {
        &self.self_device_id
    }
}
```

- [ ] **Step 8: Run; expect pass**

```bash
cd src-tauri && cargo test --lib community_channel_log_engine::tests::list_messages_returns_hlc 2>&1 | tail -10
```

### Sub-task 2B: publish

- [ ] **Step 9: Write the failing test for `publish` writing to publisher_tx and locally appending**

```rust
#[tokio::test]
async fn publish_writes_to_publisher_tx_and_appends_locally() {
    let mut fix = build_engine_fixture(8, 250, 1000).await;

    let body = b"hello channel".to_vec();
    let msg_id = Arc::clone(&fix.engine)
        .publish(body.clone(), None)
        .await
        .expect("publish");

    // Adapter side received the encrypted packet.
    let packet = tokio::time::timeout(Duration::from_millis(500), fix.publisher_rx.recv())
        .await
        .expect("packet timeout")
        .expect("publisher_rx open");
    assert!(!packet.is_empty(), "packet should be non-empty");

    // Local log has the event.
    let listed = fix.engine.list_messages(None, 100).await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id(), msg_id);

    // The packet decrypts back to the same event.
    let decrypted = decrypt_channel_packet(&fix.channel_key, &packet).expect("decrypt");
    assert_eq!(decrypted.id(), msg_id);
}
```

- [ ] **Step 10: Run; expect compile error or fail**

```bash
cd src-tauri && cargo test --lib community_channel_log_engine::tests::publish_writes 2>&1 | tail -20
```

Expected: `no method named 'publish'`.

- [ ] **Step 11: Implement `publish`**

Add to the engine impl:

```rust
/// IPC entry: mint a Post event, sign it with self, encrypt with
/// ChannelKey, send the packet to publisher_tx, locally append to
/// the log, emit `channel-message-received` Tauri event.
///
/// Does NOT wait for the broadcast to round-trip via Zenoh — the
/// local log + emit are synchronous.
pub async fn publish(
    self: Arc<Self>,
    body: Vec<u8>,
    reply_to: Option<MessageId>,
) -> Result<MessageId, ChannelLogEngineError> {
    // Bound the body. 64 KiB is generous for v3 chat-style messages.
    const MAX_BODY_BYTES: usize = 64 * 1024;
    if body.len() > MAX_BODY_BYTES {
        return Err(ChannelLogEngineError::BodyTooLarge {
            len: body.len(),
            max: MAX_BODY_BYTES,
        });
    }

    // ZEB-267: reserve next HLC under the per-device tracker lock.
    // (Reuse the existing helper so HLC monotonicity is preserved
    // across DM + community + channel surfaces.)
    let hlc = crate::dm_outbox::reserve_next_hlc_for_device(
        &self.hlc_tracker,
        &self.self_owner,
        &self.self_device_id,
    )
    .await;

    // Mint the Post event.
    let event = sign_channel_event(
        self.community_id,
        self.channel_id,
        self.self_owner,
        hlc,
        body,
        reply_to,
        &self.signing_key,
    )
    .map_err(ChannelLogEngineError::ChannelEvent)?;

    let msg_id = event.id();

    // Encrypt for broadcast.
    let packet = encrypt_channel_packet(&self.channel_key, &event)
        .map_err(ChannelLogEngineError::ChannelEvent)?;

    // Send to adapter for Zenoh broadcast. Drop on full channel
    // (degraded mode) — local append still proceeds so the user
    // sees their own message.
    if let Err(e) = self.publisher_tx.try_send(packet) {
        tracing::warn!(
            community_id = ?self.community_id,
            channel_id = ?self.channel_id,
            err = ?e,
            "publisher_tx full or closed; broadcast skipped"
        );
    }

    // Local append + replay tracker bump.
    {
        let mut log = self.log.lock().await;
        log.append(event.clone()).map_err(ChannelLogEngineError::Persist)?;
    }
    {
        let mut tracker = self.replay_tracker.lock().await;
        tracker.record(&event);
    }

    // Notify flush loop.
    self.flush_dirty.notify_one();

    // Emit Tauri event for self-loopback (UI sees own message
    // without round-tripping through Zenoh).
    self.emit_message_received(&event);

    Ok(msg_id)
}

fn emit_message_received(&self, event: &SignedChannelEvent) {
    use tauri::Emitter;
    let payload = self.message_dto_for_event(event);
    if let Err(e) = self.app.emit(
        "channel-message-received",
        ChannelMessageReceivedPayload {
            community_id: hex::encode(self.community_id.0),
            channel_id: hex::encode(self.channel_id.0),
            message: payload,
        },
    ) {
        tracing::warn!(
            community_id = ?self.community_id,
            channel_id = ?self.channel_id,
            err = ?e,
            "failed to emit channel-message-received"
        );
    }
}

fn message_dto_for_event(&self, event: &SignedChannelEvent) -> ChannelMessageDto {
    // Phase 2 stores body as ciphertext + AAD; we have plaintext
    // available because publish supplies it before encryption, and
    // receive decrypts before this is called. So extract from the
    // SignedChannelEvent's plaintext-recovery accessor.
    let SignedChannelEvent::Post {
        id,
        author,
        at,
        body_plaintext,    // Task 2 may need to expose this — see note below.
        reply_to,
        ..
    } = event;

    ChannelMessageDto {
        message_id: hex::encode(id.0),
        community_id: hex::encode(self.community_id.0),
        channel_id: hex::encode(self.channel_id.0),
        author: hex::encode(author.0),
        at: HlcDto {
            wall_ms: at.wall_ms,
            logical: at.logical,
            device_id: at.device_id.clone(),
        },
        body: body_plaintext.clone(),
        reply_to: reply_to.map(|m| hex::encode(m.0)),
    }
}
```

> **Note on `body_plaintext`:** Phase 2's `SignedChannelEvent::Post` may store body as `body_ciphertext + body_aad` only. If so, the engine needs to keep plaintext alongside (since both `publish` and the receive path know the plaintext). Adjust by either:
> - (a) Extending `SignedChannelEvent::Post` to carry plaintext as a non-serialized field (use `#[serde(skip)]`)
> - (b) Returning plaintext from `decrypt_channel_packet` separately and threading it through
>
> Pick (b) — it's cleaner. Phase 2's `decrypt_channel_packet` likely already returns the plaintext as part of decryption; if not, add a sibling `decrypt_channel_packet_with_plaintext` that returns `(SignedChannelEvent, Vec<u8>)`. The `message_dto_for_event` should then take plaintext as a parameter, not pull it from the event.

Add the corresponding payload + DTO types at module top (these match spec §9.1 and §10):

```rust
use serde::Serialize;

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HlcDto {
    pub wall_ms: u64,
    pub logical: u32,
    pub device_id: String,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelMessageDto {
    pub message_id: String,
    pub community_id: String,
    pub channel_id: String,
    pub author: String,
    pub at: HlcDto,
    pub body: Vec<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelMessageReceivedPayload {
    pub community_id: String,
    pub channel_id: String,
    pub message: ChannelMessageDto,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelBackfillProgressPayload {
    pub community_id: String,
    pub channel_id: String,
    pub fetched: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_estimate: Option<u32>,
}
```

(These are exposed `pub` from this module so lib.rs can use them in the IPC layer in Task 5.)

- [ ] **Step 12: Run; expect pass**

```bash
cd src-tauri && cargo test --lib community_channel_log_engine::tests::publish_writes 2>&1 | tail -10
```

Expected: pass.

### Sub-task 2C: receive loop

- [ ] **Step 13: Write the failing test for receive of a well-formed packet**

```rust
#[tokio::test]
async fn receive_well_formed_packet_appends_and_notifies() {
    let fix = build_engine_fixture(8, 250, 1000).await;
    fix.engine.spawn_internal_tasks();

    // Build a well-formed event from the same identity (test-only path).
    let hlc = Hlc {
        wall_ms: 5_000,
        logical: 0,
        device_id: "remote-device".to_string(),
    };
    let event = sign_channel_event(
        fix.community_id,
        fix.channel_id,
        fix.self_owner,
        hlc,
        b"from-remote".to_vec(),
        None,
        &fix.signing_key,
    )
    .expect("sign");
    let packet = encrypt_channel_packet(&fix.channel_key, &event).expect("encrypt");

    // Simulate adapter pushing inbound packet.
    fix.subscriber_tx.send(packet).await.expect("send");

    // Wait for receive loop to process.
    let listed = wait_for(|| async {
        let v = fix.engine.list_messages(None, 100).await.unwrap();
        if v.len() == 1 { Some(v) } else { None }
    }, Duration::from_secs(2))
    .await
    .expect("event appeared");

    assert_eq!(listed[0].id(), event.id());
}

/// Poll until `predicate` returns Some, or timeout. Used wherever
/// we wait on an async background task to make a state change.
async fn wait_for<F, Fut, T>(mut predicate: F, timeout: Duration) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(v) = predicate().await {
            return Some(v);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
```

- [ ] **Step 14: Run; expect compile error (`spawn_internal_tasks` does not exist)**

- [ ] **Step 15: Implement `spawn_internal_tasks` (called from registry / tests)**

Add to engine impl:

```rust
/// Spawn the receive loop + flush loop. Called by the registry
/// after construction; tests call directly.
pub fn spawn_internal_tasks(self: &Arc<Self>) {
    let (subscriber_rx, mut taken) = (
        Mutex::new(None::<mpsc::Receiver<Vec<u8>>>),
        false,
    );
    // Note: the actual receiver is moved out of params at construct
    // time and stored in a field for spawn_internal_tasks to consume.
    // Adjust the engine struct + new() to hold an Option<Receiver>
    // that spawn_internal_tasks takes once.
    let _ = (subscriber_rx, taken);

    let receive_task = self.spawn_receive_loop();
    let flush_task = self.spawn_flush_loop();

    // Store handles via blocking-on-the-mutex (we're in &self).
    tokio::spawn({
        let me = Arc::clone(self);
        async move {
            *me.receive_handle.lock().await = Some(receive_task);
            *me.flush_handle.lock().await = Some(flush_task);
        }
    });
}

fn spawn_receive_loop(self: &Arc<Self>) -> JoinHandle<()> {
    let me = Arc::clone(self);
    tokio::spawn(async move {
        let mut rx = match me.take_subscriber_rx().await {
            Some(r) => r,
            None => return,
        };
        let closing = Arc::clone(&me.closing);
        loop {
            tokio::select! {
                biased;
                maybe = rx.recv() => {
                    let Some(packet) = maybe else { break; };
                    me.process_inbound_packet(packet).await;
                }
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    if closing.load(Ordering::SeqCst) { break; }
                }
            }
        }
    })
}

async fn process_inbound_packet(self: &Arc<Self>, packet: Vec<u8>) {
    // 1. Decrypt
    let event = match decrypt_channel_packet(&self.channel_key, &packet) {
        Ok(ev) => ev,
        Err(e) => {
            tracing::warn!(
                community_id = ?self.community_id,
                channel_id = ?self.channel_id,
                err = ?e,
                "drop garbage packet (decrypt failed)"
            );
            return;
        }
    };

    // 2. Verify chain (Phase 2's verify_channel_event handles
    // signature, replay, author membership, write_power).
    let verify = {
        let tracker = self.replay_tracker.lock().await;
        verify_channel_event(
            &event,
            self.community_id,
            self.channel_id,
            self.state_at_hlc.as_ref(),
            self.resolver.as_ref(),
            &tracker,
        )
        .await
    };
    if let Err(e) = verify {
        match e {
            ChannelEventError::Replay { .. } => {
                tracing::debug!(?e, "drop replay");
            }
            _ => {
                tracing::warn!(
                    community_id = ?self.community_id,
                    channel_id = ?self.channel_id,
                    err = ?e,
                    "drop invalid packet"
                );
            }
        }
        return;
    }

    // 3. Append + record replay.
    let inserted = {
        let mut log = self.log.lock().await;
        match log.append(event.clone()) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(
                    community_id = ?self.community_id,
                    channel_id = ?self.channel_id,
                    err = ?e,
                    "channel-log persist failed; degraded"
                );
                self.emit_degraded(&format!("persist: {e}"));
                return;
            }
        }
    };
    if !inserted {
        return;
    }
    {
        let mut tracker = self.replay_tracker.lock().await;
        tracker.record(&event);
    }

    // 4. Emit + notify flush.
    self.emit_message_received(&event);
    self.flush_dirty.notify_one();
}

fn emit_degraded(&self, reason: &str) {
    use tauri::Emitter;
    if let Err(e) = self.app.emit(
        "channel-log-degraded",
        serde_json::json!({
            "communityId": hex::encode(self.community_id.0),
            "channelId": hex::encode(self.channel_id.0),
            "reason": reason,
        }),
    ) {
        tracing::warn!(err = ?e, "failed to emit channel-log-degraded");
    }
}
```

You'll need to:
- Add an `Option<mpsc::Receiver<Vec<u8>>>` field to `ChannelLogEngine` (e.g., `subscriber_rx_holder: Mutex<Option<mpsc::Receiver<Vec<u8>>>>`) and have `new()` populate it from `params.subscriber_rx`. Then `take_subscriber_rx` consumes it once.
- Adjust the `spawn_internal_tasks` to actually be `async` and `await` the storage of handles, or use a synchronous `OnceLock`-style mechanism.

Cleaner alternative: spawn the receive loop and flush loop inside `new()` itself, taking the receiver from `params` directly and storing JoinHandles in the engine struct. Then `spawn_internal_tasks` is unnecessary. Use this shape — it eliminates a state-machine concern.

Refactor `new()`:

```rust
pub async fn new(
    params: ChannelLogEngineParams<R>,
) -> Result<Arc<Self>, ChannelLogEngineError> {
    let log = ChannelLog::new(
        params.community_id,
        params.channel_id,
        params.root_dir,
        params.config.log_config.clone(),
    )?;

    let engine = Arc::new(Self {
        community_id: params.community_id,
        channel_id: params.channel_id,
        channel_key: params.channel_key,
        log: Arc::new(Mutex::new(log)),
        replay_tracker: Arc::new(Mutex::new(ChannelLogReplayTracker::new())),
        state_at_hlc: params.state_at_hlc,
        resolver: params.resolver,
        self_owner: params.self_owner,
        self_device_id: params.self_device_id,
        signing_key: params.signing_key,
        hlc_tracker: params.hlc_tracker,
        app: params.app,
        config: params.config,
        publisher_tx: params.publisher_tx,
        query_request_tx: params.query_request_tx,
        receive_handle: Mutex::new(None),
        flush_handle: Mutex::new(None),
        flush_dirty: Arc::new(Notify::new()),
        closing: Arc::new(AtomicBool::new(false)),
    });

    // Spawn receive loop, taking ownership of subscriber_rx.
    let receive_handle = {
        let me = Arc::clone(&engine);
        let mut rx = params.subscriber_rx;
        tokio::spawn(async move {
            let closing = Arc::clone(&me.closing);
            loop {
                tokio::select! {
                    biased;
                    maybe = rx.recv() => {
                        let Some(packet) = maybe else { break; };
                        me.process_inbound_packet(packet).await;
                    }
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        if closing.load(Ordering::SeqCst) { break; }
                    }
                }
            }
        })
    };
    *engine.receive_handle.lock().await = Some(receive_handle);

    // Spawn flush loop (defined in Sub-task 2D).
    let flush_handle = engine.spawn_flush_loop();
    *engine.flush_handle.lock().await = Some(flush_handle);

    Ok(engine)
}
```

(Update the Task 1 smoke test if it asserts no tasks were spawned.)

- [ ] **Step 16: Run; expect pass**

```bash
cd src-tauri && cargo test --lib community_channel_log_engine::tests::receive_well_formed 2>&1 | tail -15
```

- [ ] **Step 17: Add receive-loop edge-case tests**

```rust
#[tokio::test]
async fn receive_garbage_packet_drops_silently() {
    let fix = build_engine_fixture(8, 250, 1000).await;
    fix.subscriber_tx.send(b"not a real packet".to_vec()).await.expect("send");

    // Give receive loop time to process.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let listed = fix.engine.list_messages(None, 100).await.expect("list");
    assert!(listed.is_empty());
}

#[tokio::test]
async fn receive_replay_drops_silently() {
    let fix = build_engine_fixture(8, 250, 1000).await;

    let hlc = Hlc {
        wall_ms: 6_000,
        logical: 0,
        device_id: "remote".to_string(),
    };
    let event = sign_channel_event(
        fix.community_id,
        fix.channel_id,
        fix.self_owner,
        hlc,
        b"once".to_vec(),
        None,
        &fix.signing_key,
    )
    .expect("sign");
    let packet = encrypt_channel_packet(&fix.channel_key, &event).expect("encrypt");

    // Send twice.
    fix.subscriber_tx.send(packet.clone()).await.expect("send 1");
    fix.subscriber_tx.send(packet).await.expect("send 2");

    let listed = wait_for(|| async {
        let v = fix.engine.list_messages(None, 100).await.unwrap();
        if v.len() == 1 { Some(v) } else { None }
    }, Duration::from_secs(2))
    .await
    .expect("exactly one event");
    assert_eq!(listed.len(), 1);
}
```

- [ ] **Step 18: Run; both pass**

```bash
cd src-tauri && cargo test --lib community_channel_log_engine::tests::receive_ 2>&1 | tail -15
```

### Sub-task 2D: flush loop

- [ ] **Step 19: Write the failing test for debounced flush**

```rust
#[tokio::test]
async fn flush_debounce_coalesces_burst() {
    // Use 50 ms debounce + 500 ms cap so the test runs quickly.
    let fix = build_engine_fixture(1024, 50, 500).await;

    // Append 5 events quickly via the local log, mimicking the burst
    // path that flush_dirty fires off.
    for i in 0..5 {
        let hlc = Hlc {
            wall_ms: 7_000 + i,
            logical: 0,
            device_id: "burst".to_string(),
        };
        let ev = sign_channel_event(
            fix.community_id,
            fix.channel_id,
            fix.self_owner,
            hlc,
            format!("burst-{i}").into_bytes(),
            None,
            &fix.signing_key,
        )
        .expect("sign");
        {
            let mut log = fix.engine.log_for_test().lock().await;
            log.append(ev).expect("append");
        }
        fix.engine.notify_dirty_for_test();
    }

    // Wait past the debounce window for the flush to occur.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Verify tail.cbor exists and contains 5 events.
    let tail_path = fix._tmp.path().join("tail.cbor");
    assert!(tail_path.exists(), "tail.cbor should be written after debounce");

    // Read it back and assert event count.
    let bytes = std::fs::read(&tail_path).expect("read tail");
    // Skip the schema-version byte prefix per Phase 2 layout.
    assert!(bytes.len() > 1, "tail.cbor non-empty");
}
```

Add the test-only accessor:

```rust
#[cfg(test)]
impl<R: tauri::Runtime> ChannelLogEngine<R> {
    pub(crate) fn notify_dirty_for_test(&self) {
        self.flush_dirty.notify_one();
    }
}
```

- [ ] **Step 20: Run; expect fail (no flush loop yet)**

- [ ] **Step 21: Implement `spawn_flush_loop`**

Add to engine impl:

```rust
fn spawn_flush_loop(self: &Arc<Self>) -> JoinHandle<()> {
    let me = Arc::clone(self);
    let debounce = Duration::from_millis(self.config.flush_debounce_ms);
    let max_dirty = Duration::from_millis(self.config.max_dirty_ms);

    tokio::spawn(async move {
        let closing = Arc::clone(&me.closing);
        loop {
            // Wait for first dirty notification.
            tokio::select! {
                biased;
                _ = me.flush_dirty.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    if closing.load(Ordering::SeqCst) { break; }
                    continue;
                }
            }

            let first_dirty = std::time::Instant::now();
            let hard_deadline = first_dirty + max_dirty;
            let mut soft_deadline = first_dirty + debounce;

            // Sliding debounce: each notify resets the soft deadline,
            // but hard_deadline is preserved.
            loop {
                let target = soft_deadline.min(hard_deadline);
                let now = std::time::Instant::now();
                if now >= target {
                    break;
                }
                tokio::select! {
                    biased;
                    _ = me.flush_dirty.notified() => {
                        soft_deadline = std::time::Instant::now() + debounce;
                    }
                    _ = tokio::time::sleep_until(target.into()) => {
                        break;
                    }
                }
            }

            // Flush + check for seal.
            let flush_result = {
                let log = me.log.lock().await;
                log.flush_tail()
            };
            if let Err(e) = flush_result {
                tracing::error!(
                    community_id = ?me.community_id,
                    channel_id = ?me.channel_id,
                    err = ?e,
                    "channel-log tail flush failed"
                );
                me.emit_degraded(&format!("flush: {e}"));
            }

            // Seal-on-threshold check.
            let should_seal = {
                let log = me.log.lock().await;
                log.tail.len() >= log.config_for_test().seal_threshold_events
            };
            if should_seal {
                let seal_result = {
                    let mut log = me.log.lock().await;
                    log.seal_and_persist()
                };
                if let Err(e) = seal_result {
                    tracing::error!(
                        community_id = ?me.community_id,
                        channel_id = ?me.channel_id,
                        err = ?e,
                        "channel-log seal failed"
                    );
                    me.emit_degraded(&format!("seal: {e}"));
                }
            }

            if closing.load(Ordering::SeqCst) {
                break;
            }
        }
    })
}
```

(`config_for_test` is a placeholder — Phase 2's `ChannelLog` likely already exposes the threshold via `manifest` or has a public `seal_threshold` accessor. Use whatever is available; if nothing is, add a `pub fn config(&self) -> &ChannelLogConfig` accessor to Phase 2's `ChannelLog`.)

- [ ] **Step 22: Run; expect pass**

```bash
cd src-tauri && cargo test --lib community_channel_log_engine::tests::flush_debounce 2>&1 | tail -15
```

- [ ] **Step 23: Add max-dirty-cap test**

```rust
#[tokio::test]
async fn flush_max_dirty_forces_under_continuous_load() {
    let fix = build_engine_fixture(1024, 100, 250).await;

    // Continuously append every 50 ms (faster than debounce) for 600 ms.
    let start = std::time::Instant::now();
    let mut i = 0u64;
    while start.elapsed() < Duration::from_millis(600) {
        let hlc = Hlc {
            wall_ms: 8_000 + i,
            logical: 0,
            device_id: "continuous".to_string(),
        };
        let ev = sign_channel_event(
            fix.community_id,
            fix.channel_id,
            fix.self_owner,
            hlc,
            b"x".to_vec(),
            None,
            &fix.signing_key,
        )
        .expect("sign");
        {
            let mut log = fix.engine.log_for_test().lock().await;
            log.append(ev).expect("append");
        }
        fix.engine.notify_dirty_for_test();
        tokio::time::sleep(Duration::from_millis(50)).await;
        i += 1;
    }

    // Wait an extra tick for the most recent flush to finish.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Assert tail.cbor has been written at least once. We can't
    // count flushes precisely without more instrumentation, but
    // file-existence + non-empty proves the loop ran.
    let tail_path = fix._tmp.path().join("tail.cbor");
    assert!(tail_path.exists());
}
```

- [ ] **Step 24: Run; expect pass**

- [ ] **Step 25: Add `flush_now` for shutdown**

Add to engine impl:

```rust
/// Force a synchronous flush, bypassing debounce. Called by
/// shutdown and (Task 5) by the registry on stop.
pub async fn flush_now(&self) -> Result<(), ChannelLogEngineError> {
    let log = self.log.lock().await;
    log.flush_tail().map_err(ChannelLogEngineError::Persist)?;
    Ok(())
}
```

Update `shutdown` to call it before joining tasks:

```rust
pub async fn shutdown(&self) -> Result<(), ChannelLogEngineError> {
    self.closing.store(true, Ordering::SeqCst);
    self.flush_dirty.notify_one();

    if let Some(handle) = self.receive_handle.lock().await.take() {
        let _ = handle.await;
    }
    if let Some(handle) = self.flush_handle.lock().await.take() {
        let _ = handle.await;
    }
    self.flush_now().await?;
    Ok(())
}
```

- [ ] **Step 26: Add `flush_now` test**

```rust
#[tokio::test]
async fn flush_now_writes_synchronously() {
    let fix = build_engine_fixture(1024, 5_000, 10_000).await; // long debounce so flush_now wins

    let hlc = Hlc {
        wall_ms: 9_000,
        logical: 0,
        device_id: "sync".to_string(),
    };
    let ev = sign_channel_event(
        fix.community_id,
        fix.channel_id,
        fix.self_owner,
        hlc,
        b"sync-flushed".to_vec(),
        None,
        &fix.signing_key,
    )
    .expect("sign");
    {
        let mut log = fix.engine.log_for_test().lock().await;
        log.append(ev).expect("append");
    }

    fix.engine.flush_now().await.expect("flush_now");

    let tail_path = fix._tmp.path().join("tail.cbor");
    assert!(tail_path.exists());
    assert!(std::fs::metadata(&tail_path).expect("meta").len() > 1);
}
```

- [ ] **Step 27: Run all engine tests**

```bash
cd src-tauri && cargo test --lib community_channel_log_engine 2>&1 | tail -20
```

Expected: all engine tests pass.

- [ ] **Step 28: Verify gates green**

```bash
cd src-tauri && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && echo "GATES GREEN"
```

- [ ] **Step 29: Commit**

```bash
git add src-tauri/src/community_channel_log_engine.rs
git commit -m "$(cat <<'EOF'
feat(zeb-270): ChannelLogEngine internals — receive, flush, publish, list

Implements the four core engine behaviors per spec §6:

- receive loop: decrypt → verify_channel_event → append → emit
- flush loop: 250ms sliding debounce + 1s max-dirty cap + seal-on-threshold
- publish: mints + signs + encrypts + sends + locally appends + emits
  (self-loopback handled locally, not via Zenoh round-trip)
- list_messages: walks segments + tail in HLC order, capped at limit

Includes 8 unit tests covering happy path, replay rejection, garbage-drop,
debounce coalescing, max-dirty cap, sync flush, list with limit + ordering.

DTOs (ChannelMessageDto, ChannelMessageReceivedPayload,
ChannelBackfillProgressPayload, HlcDto) defined here; Task 5 wires them
into the IPC layer.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: request_backfill API + spawn_channel_log_zenoh_adapter

**Files:**
- Modify: `src-tauri/src/community_channel_log_engine.rs` (add `request_backfill` method)
- Modify: `src-tauri/src/event_loop.rs` (add `spawn_channel_log_zenoh_adapter`)
- Test: in-file tests in both modules

**Goal:** Engine-side `request_backfill` queues a `BackfillQueryRequest` to the adapter; the adapter runs the four Zenoh tasks per spec §8.

### Sub-task 3A: engine-side `request_backfill`

- [ ] **Step 1: Write failing test**

In `community_channel_log_engine.rs`, add to the `tests` module:

```rust
#[tokio::test]
async fn request_backfill_queues_query_request() {
    let mut fix = build_engine_fixture(8, 250, 1000).await;

    Arc::clone(&fix.engine)
        .request_backfill(None)
        .await
        .expect("backfill");

    let req = tokio::time::timeout(Duration::from_millis(500), fix.query_request_rx.recv())
        .await
        .expect("timeout")
        .expect("rx open");
    assert!(req.since.is_none());
}

#[tokio::test]
async fn request_backfill_passes_since_through() {
    let mut fix = build_engine_fixture(8, 250, 1000).await;

    let since = Hlc {
        wall_ms: 12_345,
        logical: 7,
        device_id: "from".to_string(),
    };
    Arc::clone(&fix.engine)
        .request_backfill(Some(since.clone()))
        .await
        .expect("backfill");

    let req = tokio::time::timeout(Duration::from_millis(500), fix.query_request_rx.recv())
        .await
        .expect("timeout")
        .expect("rx open");
    let got = req.since.expect("Some since");
    assert_eq!(got.wall_ms, since.wall_ms);
    assert_eq!(got.logical, since.logical);
    assert_eq!(got.device_id, since.device_id);
}
```

- [ ] **Step 2: Run; expect fail**

- [ ] **Step 3: Implement `request_backfill`**

Add to engine impl:

```rust
/// Fire a Zenoh queryable request via the adapter. Reply packets
/// stream back through the same subscriber path (per spec §8.1
/// — backfill replies are wire-identical to live broadcasts), so
/// this method is fire-and-forget.
pub async fn request_backfill(
    self: Arc<Self>,
    since: Option<Hlc>,
) -> Result<(), ChannelLogEngineError> {
    self.query_request_tx
        .send(BackfillQueryRequest { since, limit: 0 })
        .await
        .map_err(|e| ChannelLogEngineError::BackfillFailed(e.to_string()))
}
```

- [ ] **Step 4: Run; expect pass**

```bash
cd src-tauri && cargo test --lib community_channel_log_engine::tests::request_backfill 2>&1 | tail -15
```

### Sub-task 3B: `spawn_channel_log_zenoh_adapter` in event_loop.rs

- [ ] **Step 5: Locate the existing spawn helper for reference**

```bash
grep -n "spawn_community_state_zenoh_adapter" src-tauri/src/event_loop.rs
```

Expected: one definition site. Read it for the canonical shape.

- [ ] **Step 6: Add `spawn_channel_log_zenoh_adapter` immediately after `spawn_community_state_zenoh_adapter`**

In `src-tauri/src/event_loop.rs`, after the closing `}` of `spawn_community_state_zenoh_adapter`, add:

```rust
/// Per-(community, channel) Zenoh adapter for the ChannelLog data
/// plane (ZEB-270 / ZEB-248 Phase 3). Mirrors
/// `spawn_community_state_zenoh_adapter` in shape: spawns four
/// tokio tasks (publisher, subscriber, queryable, query-request
/// driver), all bound to the per-channel topics.
///
/// Topics:
/// - `harmony/channels/{cid_hex}/{ch_id_hex}/events` — live broadcast
/// - `harmony/channels/{cid_hex}/{ch_id_hex}/since/{hlc_hex}/{limit}` — queryable
///
/// The `read_for_query` callback is what the queryable handler uses
/// to fetch events for a backfill request — passed in to avoid
/// the engine ↔ adapter circular dep (per spec §8.1).
pub fn spawn_channel_log_zenoh_adapter<F>(
    session: Arc<zenoh::Session>,
    community_id_hex: String,
    channel_id_hex: String,
    mut publisher_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    subscriber_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    mut query_request_rx: tokio::sync::mpsc::Receiver<
        crate::community_channel_log_engine::BackfillQueryRequest,
    >,
    read_for_query: Arc<F>,
    closing: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()>
where
    F: Fn(
            Option<crate::owner_state_types::Hlc>,
            usize,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Vec<Vec<u8>>> + Send>,
        > + Send
        + Sync
        + 'static,
{
    let events_topic = format!(
        "harmony/channels/{}/{}/events",
        community_id_hex, channel_id_hex
    );
    let queryable_prefix = format!(
        "harmony/channels/{}/{}/since/**",
        community_id_hex, channel_id_hex
    );

    tokio::spawn(async move {
        let events_key = match zenoh::key_expr::KeyExpr::try_from(events_topic.clone()) {
            Ok(k) => k,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    %events_topic,
                    "channel-log events key_expr invalid; adapter skipped"
                );
                return;
            }
        };
        let queryable_key = match zenoh::key_expr::KeyExpr::try_from(queryable_prefix.clone()) {
            Ok(k) => k,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    %queryable_prefix,
                    "channel-log queryable key_expr invalid; adapter skipped"
                );
                return;
            }
        };

        // ── Publisher task ─────────────────────────────────────────
        let session_pub = Arc::clone(&session);
        let key_pub = events_key.clone();
        let topic_pub = events_topic.clone();
        let closing_pub = Arc::clone(&closing);
        let pub_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    maybe = publisher_rx.recv() => {
                        let Some(bytes) = maybe else { break; };
                        if let Err(e) = session_pub.put(&key_pub, bytes).await {
                            if !closing_pub.load(Ordering::SeqCst) {
                                tracing::warn!(
                                    topic = %topic_pub,
                                    error = %e,
                                    "channel-log publish failed"
                                );
                            }
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        if closing_pub.load(Ordering::SeqCst) { break; }
                    }
                }
            }
        });

        // ── Subscriber task ────────────────────────────────────────
        let session_sub = Arc::clone(&session);
        let key_sub = events_key.clone();
        let topic_sub = events_topic.clone();
        let subscriber_tx_sub = subscriber_tx.clone();
        let closing_sub = Arc::clone(&closing);
        let sub_handle = tokio::spawn(async move {
            let sub = match session_sub.declare_subscriber(&key_sub).await {
                Ok(s) => s,
                Err(e) => {
                    if !closing_sub.load(Ordering::SeqCst) {
                        tracing::error!(
                            topic = %topic_sub,
                            error = %e,
                            "failed to declare channel-log subscriber"
                        );
                    }
                    return;
                }
            };
            loop {
                tokio::select! {
                    biased;
                    res = sub.recv_async() => {
                        match res {
                            Ok(sample) => {
                                let bytes: Vec<u8> = sample.payload().to_bytes().to_vec();
                                if subscriber_tx_sub.send(bytes).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                if !closing_sub.load(Ordering::SeqCst) {
                                    tracing::warn!(
                                        topic = %topic_sub,
                                        error = %e,
                                        "channel-log subscriber closed unexpectedly"
                                    );
                                }
                                break;
                            }
                        }
                    }
                    _ = subscriber_tx_sub.closed() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        if closing_sub.load(Ordering::SeqCst) { break; }
                    }
                }
            }
        });

        // ── Queryable task ─────────────────────────────────────────
        let session_qbl = Arc::clone(&session);
        let key_qbl = queryable_key.clone();
        let prefix_qbl = queryable_prefix.clone();
        let read_for_query = Arc::clone(&read_for_query);
        let closing_qbl = Arc::clone(&closing);
        let qbl_handle = tokio::spawn(async move {
            let qbl = match session_qbl.declare_queryable(&key_qbl).await {
                Ok(q) => q,
                Err(e) => {
                    if !closing_qbl.load(Ordering::SeqCst) {
                        tracing::error!(
                            prefix = %prefix_qbl,
                            error = %e,
                            "failed to declare channel-log queryable"
                        );
                    }
                    return;
                }
            };
            loop {
                tokio::select! {
                    biased;
                    res = qbl.recv_async() => {
                        let Ok(query) = res else { break; };
                        let qkey = query.key_expr().to_string();
                        let (since, limit) = parse_channel_backfill_key(&qkey);
                        let packets = (read_for_query)(since, limit).await;
                        for packet in packets {
                            if let Err(e) = query
                                .reply(query.key_expr(), packet)
                                .await
                            {
                                tracing::warn!(
                                    prefix = %prefix_qbl,
                                    error = %e,
                                    "channel-log queryable reply failed"
                                );
                                break;
                            }
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        if closing_qbl.load(Ordering::SeqCst) { break; }
                    }
                }
            }
        });

        // ── Query-request driver ───────────────────────────────────
        let session_qr = Arc::clone(&session);
        let community_id_hex_qr = community_id_hex.clone();
        let channel_id_hex_qr = channel_id_hex.clone();
        let subscriber_tx_qr = subscriber_tx.clone();
        let closing_qr = Arc::clone(&closing);
        let qr_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    maybe = query_request_rx.recv() => {
                        let Some(req) = maybe else { break; };
                        let limit = if req.limit == 0 { 256 } else { req.limit };
                        let since_hex = match &req.since {
                            Some(h) => format_hlc_hex(h),
                            None => "0".to_string(),
                        };
                        let key = format!(
                            "harmony/channels/{}/{}/since/{}/{}",
                            community_id_hex_qr, channel_id_hex_qr, since_hex, limit
                        );
                        let receiver = match session_qr.get(&key).await {
                            Ok(r) => r,
                            Err(e) => {
                                if !closing_qr.load(Ordering::SeqCst) {
                                    tracing::warn!(
                                        %key,
                                        error = %e,
                                        "channel-log backfill query failed"
                                    );
                                }
                                continue;
                            }
                        };
                        while let Ok(reply) = receiver.recv_async().await {
                            if let Ok(sample) = reply.into_result() {
                                let bytes: Vec<u8> = sample.payload().to_bytes().to_vec();
                                if subscriber_tx_qr.send(bytes).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        if closing_qr.load(Ordering::SeqCst) { break; }
                    }
                }
            }
        });

        let _ = pub_handle.await;
        let _ = sub_handle.await;
        let _ = qbl_handle.await;
        let _ = qr_handle.await;
    })
}

/// Parse `"harmony/channels/{cid}/{ch_id}/since/{hlc_hex}/{limit}"`
/// into `(since, limit)`. Returns `(None, 0)` if parsing fails or
/// if `hlc_hex == "0"`.
fn parse_channel_backfill_key(
    key: &str,
) -> (Option<crate::owner_state_types::Hlc>, usize) {
    // Pattern is: harmony / channels / {cid} / {ch_id} / since / {hlc_hex} / {limit}
    let parts: Vec<&str> = key.split('/').collect();
    if parts.len() < 7 || parts[5] != "since" {
        return (None, 0);
    }
    let hlc_hex = parts[6];
    let limit_str = parts.get(7).copied().unwrap_or("0");

    let since = if hlc_hex == "0" {
        None
    } else {
        parse_hlc_hex(hlc_hex)
    };
    let limit = limit_str.parse::<usize>().unwrap_or(0);
    (since, limit)
}

fn parse_hlc_hex(hex_str: &str) -> Option<crate::owner_state_types::Hlc> {
    // wall_ms LE u64 (16 hex) || logical LE u32 (8 hex) || device_id_bytes (rest)
    if hex_str.len() < 24 {
        return None;
    }
    let wall_ms_bytes = hex::decode(&hex_str[0..16]).ok()?;
    let logical_bytes = hex::decode(&hex_str[16..24]).ok()?;
    let device_id_bytes = hex::decode(&hex_str[24..]).ok()?;
    let wall_ms = u64::from_le_bytes(wall_ms_bytes.try_into().ok()?);
    let logical = u32::from_le_bytes(logical_bytes.try_into().ok()?);
    let device_id = String::from_utf8(device_id_bytes).ok()?;
    Some(crate::owner_state_types::Hlc {
        wall_ms,
        logical,
        device_id,
    })
}

fn format_hlc_hex(hlc: &crate::owner_state_types::Hlc) -> String {
    let mut out = String::new();
    out.push_str(&hex::encode(hlc.wall_ms.to_le_bytes()));
    out.push_str(&hex::encode(hlc.logical.to_le_bytes()));
    out.push_str(&hex::encode(hlc.device_id.as_bytes()));
    out
}
```

(`hex` is already a workspace dep — reused throughout the codebase.)

- [ ] **Step 7: Smoke test the adapter against in-memory Zenoh**

In `src-tauri/src/event_loop.rs`, find or create a `#[cfg(test)] mod tests` block (Phase 1/2 may already have one — check). Add:

```rust
#[cfg(test)]
mod channel_log_adapter_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    /// Spawns the adapter, sends one packet via publisher, asserts
    /// the subscriber side receives it. Uses an in-memory Zenoh
    /// router so no real network is touched.
    #[tokio::test]
    async fn channel_log_adapter_publish_subscribe_round_trip() {
        let cfg = zenoh::Config::default();
        let session = Arc::new(zenoh::open(cfg).await.expect("zenoh open"));

        let (pub_tx, pub_rx) = mpsc::channel::<Vec<u8>>(8);
        let (sub_tx, mut sub_rx) = mpsc::channel::<Vec<u8>>(8);
        let (qreq_tx, qreq_rx) = mpsc::channel::<
            crate::community_channel_log_engine::BackfillQueryRequest,
        >(2);

        let read_for_query = Arc::new(|_since: Option<crate::owner_state_types::Hlc>, _limit: usize| {
            Box::pin(async move { Vec::<Vec<u8>>::new() })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = Vec<Vec<u8>>> + Send>,
                >
        });

        let closing = Arc::new(AtomicBool::new(false));
        let _adapter = spawn_channel_log_zenoh_adapter(
            Arc::clone(&session),
            "aabb".repeat(8),
            "ccdd".repeat(8),
            pub_rx,
            sub_tx,
            qreq_rx,
            read_for_query,
            Arc::clone(&closing),
        );

        // Give the subscriber time to come up (Zenoh declare is async).
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let payload = b"channel-log-roundtrip".to_vec();
        pub_tx.send(payload.clone()).await.expect("publish send");

        let received = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            sub_rx.recv(),
        )
        .await
        .expect("recv timeout")
        .expect("sub_rx open");
        assert_eq!(received, payload);

        closing.store(true, Ordering::SeqCst);
        // Ignore qreq_tx (kept alive to prevent immediate channel close).
        drop(qreq_tx);
    }
}
```

- [ ] **Step 8: Run the adapter smoke test**

```bash
cd src-tauri && cargo test --lib channel_log_adapter_publish 2>&1 | tail -15
```

Expected: pass.

If the test hangs (Zenoh in-memory subscribe takes longer than expected), bump the sleep in the test to 250 ms.

- [ ] **Step 9: Verify gates green**

```bash
cd src-tauri && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && echo "GATES GREEN"
```

- [ ] **Step 10: Commit**

```bash
git add src-tauri/src/community_channel_log_engine.rs src-tauri/src/event_loop.rs
git commit -m "$(cat <<'EOF'
feat(zeb-270): request_backfill API + Zenoh adapter

Engine-side: request_backfill queues a BackfillQueryRequest to the
adapter via query_request_tx (fire-and-forget; replies stream back
through subscriber path symmetrically per spec §8.1).

event_loop side: spawn_channel_log_zenoh_adapter mirrors
spawn_community_state_zenoh_adapter shape. Four tokio tasks:
publisher, subscriber, queryable (with read_for_query callback per
spec §8.1 to avoid engine↔adapter cycle), query-request driver.
Topic shapes locked per spec §8.2.

Includes parse_channel_backfill_key + format_hlc_hex helpers for
queryable selector encoding/decoding.

Smoke test: in-memory Zenoh round-trip on the per-channel topic.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: ChannelLogRegistry + lifecycle binding

**Files:**
- Modify: `src-tauri/src/community_channel_log_engine.rs` (add `ChannelLogRegistry` + `ChannelLogRegistryConfig`)
- Modify: `src-tauri/src/lib.rs` (extend `run_community_delta_consumer` signature with 3rd callback; wire registry into `start_node`; update existing tests for new signature)

**Goal:** Registry manages spawn/stop/reconcile of per-channel engines. Hooks into the existing delta-consumer at `lib.rs:1230` for incremental Created/Deleted; reconciles full state on community-engine startup.

### Sub-task 4A: ChannelLogRegistry struct + spawn/stop/engine

- [ ] **Step 1: Write failing test for spawn idempotency**

Add to `community_channel_log_engine.rs::tests`:

```rust
#[tokio::test]
async fn registry_spawn_idempotent() {
    let registry = build_registry_fixture().await;

    let community_id = SpaceId([0xc1; 16]);
    let channel_id = ChannelId([0xch; 16]);
    let membership_key = MembershipKey([0x77; 32]);
    let channel_key = derive_channel_key(&membership_key, &community_id, &channel_id);

    let e1 = Arc::clone(&registry)
        .spawn(community_id, channel_id, channel_key, /* deps as needed */)
        .await
        .expect("spawn 1");
    let e2 = Arc::clone(&registry)
        .spawn(community_id, channel_id, channel_key, /* deps */)
        .await
        .expect("spawn 2");

    assert!(Arc::ptr_eq(&e1, &e2), "spawn should return existing engine");

    registry.stop(&community_id, &channel_id).await.expect("stop");
}

async fn build_registry_fixture() -> Arc<ChannelLogRegistry<tauri::test::MockRuntime>> {
    let cfg = zenoh::Config::default();
    let session = Arc::new(zenoh::open(cfg).await.expect("zenoh open"));
    let app = tauri::test::mock_app().handle().clone();

    let mut seed = [0u8; 32];
    seed[0] = 0x42;
    let identity = PrivateIdentity::from_seed(&seed).expect("identity");
    let signing_key = Arc::new(SigningKey::from_bytes(
        identity.ed25519_signing_key_bytes()
            .as_slice()
            .try_into()
            .unwrap(),
    ));

    let cfg = ChannelLogRegistryConfig {
        session,
        app,
        identity_dir: tempfile::tempdir().expect("tmp").into_path(),
        self_owner: OwnerAddr(identity.public_addr()),
        self_device_id: "test-device".to_string(),
        signing_key,
        engine_config: ChannelLogEngineConfig {
            log_config: ChannelLogConfig {
                seal_threshold_events: 8,
            },
            ..Default::default()
        },
    };
    ChannelLogRegistry::new(cfg)
}
```

(Note: `build_registry_fixture`'s deps need state_at_hlc + resolver + hlc_tracker per channel; the spawn signature accepts them as args. Keep them stub-shaped for the test — `Arc<AlwaysJoinedState>` and `Arc<FixedIdentityResolver>` from earlier, plus a fresh `CommunityRootHlcTracker`. Adjust the test to pass these.)

- [ ] **Step 2: Run; expect compile error (`ChannelLogRegistry` doesn't exist yet)**

- [ ] **Step 3: Implement `ChannelLogRegistry`**

In `community_channel_log_engine.rs`, after the engine impl block, add:

```rust
/// Shared deps for all engines spawned by a single registry.
pub struct ChannelLogRegistryConfig<R: tauri::Runtime> {
    pub session: Arc<zenoh::Session>,
    pub app: tauri::AppHandle<R>,
    pub identity_dir: PathBuf,
    pub self_owner: OwnerAddr,
    pub self_device_id: String,
    pub signing_key: Arc<SigningKey>,
    pub engine_config: ChannelLogEngineConfig,
}

/// Per-CommunitySyncEngine registry of running per-channel engines.
/// Mirrors `CommunitySyncRegistry` from `community_state_sync.rs`.
pub struct ChannelLogRegistry<R: tauri::Runtime> {
    engines: Mutex<HashMap<(SpaceId, ChannelId), Arc<ChannelLogEngine<R>>>>,
    adapter_handles: Mutex<HashMap<(SpaceId, ChannelId), JoinHandle<()>>>,
    closings: Mutex<HashMap<(SpaceId, ChannelId), Arc<AtomicBool>>>,
    config: ChannelLogRegistryConfig<R>,
}

impl<R: tauri::Runtime> ChannelLogRegistry<R> {
    pub fn new(config: ChannelLogRegistryConfig<R>) -> Arc<Self> {
        Arc::new(Self {
            engines: Mutex::new(HashMap::new()),
            adapter_handles: Mutex::new(HashMap::new()),
            closings: Mutex::new(HashMap::new()),
            config,
        })
    }

    /// Spawn engine + Zenoh adapter for one channel. Idempotent —
    /// returns the existing Arc if already present.
    pub async fn spawn(
        self: &Arc<Self>,
        community_id: SpaceId,
        channel_id: ChannelId,
        channel_key: ChannelKey,
        state_at_hlc: Arc<dyn CommunityStateAtHlc + Send + Sync>,
        resolver: Arc<dyn ChannelIdentityResolver + Send + Sync>,
        hlc_tracker: Arc<Mutex<CommunityRootHlcTracker>>,
    ) -> Result<Arc<ChannelLogEngine<R>>, ChannelLogEngineError> {
        let key = (community_id, channel_id);
        {
            let engines = self.engines.lock().await;
            if let Some(existing) = engines.get(&key) {
                return Ok(Arc::clone(existing));
            }
        }

        let community_id_hex = hex::encode(community_id.0);
        let channel_id_hex = hex::encode(channel_id.0);
        let root_dir = self
            .config
            .identity_dir
            .join("communities")
            .join(&community_id_hex)
            .join("channels")
            .join(&channel_id_hex);
        std::fs::create_dir_all(&root_dir).map_err(|e| {
            ChannelLogEngineError::Persist(ChannelLogPersistError::Io(e))
        })?;

        let (publisher_tx, publisher_rx) = mpsc::channel(64);
        let (subscriber_tx, subscriber_rx) = mpsc::channel(64);
        let (query_request_tx, query_request_rx) = mpsc::channel(8);

        let params = ChannelLogEngineParams {
            community_id,
            channel_id,
            channel_key: Arc::new(channel_key),
            root_dir,
            state_at_hlc,
            resolver,
            self_owner: self.config.self_owner,
            self_device_id: self.config.self_device_id.clone(),
            signing_key: Arc::clone(&self.config.signing_key),
            hlc_tracker,
            app: self.config.app.clone(),
            config: self.config.engine_config.clone(),
            publisher_tx,
            subscriber_rx,
            query_request_tx,
        };
        let engine = ChannelLogEngine::new(params).await?;

        // Build read_for_query closure capturing engine ref (no
        // cycle: closure moves Arc<engine>, adapter doesn't store
        // anything else from the engine).
        let engine_for_query = Arc::clone(&engine);
        let read_for_query = Arc::new(
            move |since: Option<Hlc>, limit: usize| -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Vec<Vec<u8>>> + Send>,
            > {
                let me = Arc::clone(&engine_for_query);
                Box::pin(async move {
                    let events = match me.list_messages(since, limit).await {
                        Ok(v) => v,
                        Err(_) => return Vec::new(),
                    };
                    events
                        .iter()
                        .filter_map(|ev| {
                            crate::community_channel_log::encrypt_channel_packet(
                                me.channel_key_for_test(),
                                ev,
                            )
                            .ok()
                        })
                        .collect()
                })
            },
        );

        let closing = Arc::new(AtomicBool::new(false));
        let adapter_handle = crate::event_loop::spawn_channel_log_zenoh_adapter(
            Arc::clone(&self.config.session),
            community_id_hex,
            channel_id_hex,
            publisher_rx,
            subscriber_tx,
            query_request_rx,
            read_for_query,
            Arc::clone(&closing),
        );

        {
            let mut engines = self.engines.lock().await;
            engines.insert(key, Arc::clone(&engine));
        }
        {
            let mut handles = self.adapter_handles.lock().await;
            handles.insert(key, adapter_handle);
        }
        {
            let mut closings = self.closings.lock().await;
            closings.insert(key, closing);
        }

        Ok(engine)
    }

    /// Stop engine and discard. Idempotent (no-op if not present).
    pub async fn stop(
        &self,
        community_id: &SpaceId,
        channel_id: &ChannelId,
    ) -> Result<(), ChannelLogEngineError> {
        let key = (*community_id, *channel_id);

        let engine = {
            let mut engines = self.engines.lock().await;
            engines.remove(&key)
        };
        let Some(engine) = engine else {
            return Ok(());
        };

        engine.shutdown().await?;

        if let Some(closing) = self.closings.lock().await.remove(&key) {
            closing.store(true, Ordering::SeqCst);
        }
        if let Some(handle) = self.adapter_handles.lock().await.remove(&key) {
            let _ = handle.await;
        }

        Ok(())
    }

    pub async fn engine(
        &self,
        community_id: &SpaceId,
        channel_id: &ChannelId,
    ) -> Option<Arc<ChannelLogEngine<R>>> {
        self.engines
            .lock()
            .await
            .get(&(*community_id, *channel_id))
            .cloned()
    }

    pub async fn shutdown_all(&self) -> Result<(), ChannelLogEngineError> {
        let keys: Vec<_> = self.engines.lock().await.keys().cloned().collect();
        for (cid, chid) in keys {
            self.stop(&cid, &chid).await?;
        }
        Ok(())
    }

    /// Walk a community's materialized channels and spawn engines
    /// for each non-tombstoned entry. Idempotent — re-running is a
    /// no-op (spawn returns existing).
    pub async fn reconcile_from_state(
        self: &Arc<Self>,
        community_id: SpaceId,
        community_state: &crate::community_membership::CommunityState,
        membership_key: &MembershipKey,
        state_at_hlc: Arc<dyn CommunityStateAtHlc + Send + Sync>,
        resolver: Arc<dyn ChannelIdentityResolver + Send + Sync>,
        hlc_tracker: Arc<Mutex<CommunityRootHlcTracker>>,
    ) -> Result<(), ChannelLogEngineError> {
        for (channel_id, info) in &community_state.channels {
            if info.deleted_at.is_some() {
                continue;
            }
            let channel_key = derive_channel_key(membership_key, &community_id, channel_id);
            self.spawn(
                community_id,
                *channel_id,
                channel_key,
                Arc::clone(&state_at_hlc),
                Arc::clone(&resolver),
                Arc::clone(&hlc_tracker),
            )
            .await?;
        }
        Ok(())
    }
}
```

Add the test-only accessor:

```rust
#[cfg(test)]
impl<R: tauri::Runtime> ChannelLogEngine<R> {
    pub(crate) fn channel_key_for_test(&self) -> &ChannelKey {
        &self.channel_key
    }
}
```

(Phase 2's `community_membership::CommunityState` must already expose `channels: HashMap<ChannelId, ChannelInfo>` with `deleted_at: Option<Hlc>` per Phase 1 — verify by `grep` if uncertain.)

- [ ] **Step 4: Run; expect pass**

```bash
cd src-tauri && cargo test --lib community_channel_log_engine::tests::registry_spawn 2>&1 | tail -15
```

- [ ] **Step 5: Add stop + reconcile tests**

```rust
#[tokio::test]
async fn registry_stop_discards_entry() {
    let registry = build_registry_fixture().await;
    let cid = SpaceId([0xc1; 16]);
    let chid = ChannelId([0xch; 16]);
    let mk = MembershipKey([0x77; 32]);
    let key = derive_channel_key(&mk, &cid, &chid);
    let _ = Arc::clone(&registry)
        .spawn(cid, chid, key, /* deps */)
        .await
        .expect("spawn");
    registry.stop(&cid, &chid).await.expect("stop");
    assert!(registry.engine(&cid, &chid).await.is_none());
}

#[tokio::test]
async fn registry_reconcile_skips_deleted_channels() {
    let registry = build_registry_fixture().await;
    let cid = SpaceId([0xc1; 16]);
    let live_chid = ChannelId([0x01; 16]);
    let dead_chid = ChannelId([0x02; 16]);
    let mk = MembershipKey([0x77; 32]);

    // Build a CommunityState with one live + one tombstoned channel.
    let mut state = crate::community_membership::CommunityState::default();
    state.channels.insert(
        live_chid,
        crate::community_membership::ChannelInfo {
            name: "live".to_string(),
            write_power: 0,
            created_at: Hlc { wall_ms: 1, logical: 0, device_id: "x".to_string() },
            deleted_at: None,
        },
    );
    state.channels.insert(
        dead_chid,
        crate::community_membership::ChannelInfo {
            name: "dead".to_string(),
            write_power: 0,
            created_at: Hlc { wall_ms: 2, logical: 0, device_id: "x".to_string() },
            deleted_at: Some(Hlc { wall_ms: 3, logical: 0, device_id: "x".to_string() }),
        },
    );

    Arc::clone(&registry)
        .reconcile_from_state(cid, &state, &mk, /* deps */)
        .await
        .expect("reconcile");

    assert!(registry.engine(&cid, &live_chid).await.is_some());
    assert!(registry.engine(&cid, &dead_chid).await.is_none());
}

#[tokio::test]
async fn registry_reconcile_idempotent() {
    let registry = build_registry_fixture().await;
    let cid = SpaceId([0xc1; 16]);
    let chid = ChannelId([0x01; 16]);
    let mk = MembershipKey([0x77; 32]);
    let mut state = crate::community_membership::CommunityState::default();
    state.channels.insert(
        chid,
        crate::community_membership::ChannelInfo {
            name: "live".to_string(),
            write_power: 0,
            created_at: Hlc { wall_ms: 1, logical: 0, device_id: "x".to_string() },
            deleted_at: None,
        },
    );

    let r1 = Arc::clone(&registry);
    r1.reconcile_from_state(cid, &state, &mk, /* deps */).await.expect("first");
    let e1 = registry.engine(&cid, &chid).await.expect("engine 1");

    let r2 = Arc::clone(&registry);
    r2.reconcile_from_state(cid, &state, &mk, /* deps */).await.expect("second");
    let e2 = registry.engine(&cid, &chid).await.expect("engine 2");

    assert!(Arc::ptr_eq(&e1, &e2));
}
```

(`/* deps */` placeholders should be replaced with the same stub state/resolver/tracker used in `build_engine_fixture`.)

- [ ] **Step 6: Run all registry tests**

```bash
cd src-tauri && cargo test --lib community_channel_log_engine::tests::registry_ 2>&1 | tail -20
```

### Sub-task 4B: Lifecycle binding into run_community_delta_consumer

- [ ] **Step 7: Locate and read the existing run_community_delta_consumer signature**

```bash
grep -n "fn run_community_delta_consumer\b" src-tauri/src/lib.rs
```

```bash
sed -n "$(grep -n 'fn run_community_delta_consumer' src-tauri/src/lib.rs | head -1 | cut -d: -f1),+40p" src-tauri/src/lib.rs
```

This returns the current signature — it takes two callbacks (membership-changed, channel-config-updated) plus the receiver. We extend it to take a third (channel-config-applied → registry hook).

- [ ] **Step 8: Extend `run_community_delta_consumer` to take a 3rd callback**

In `src-tauri/src/lib.rs`, modify the signature. Find:

```rust
pub async fn run_community_delta_consumer<FM, FutM, FC, FutC>(
    mut rx: mpsc::Receiver<CommunityMembershipDelta>,
    on_membership: FM,
    on_channel_config: FC,
)
```

Change to (keeping the existing two callbacks plus a new third one):

```rust
pub async fn run_community_delta_consumer<FM, FutM, FC, FutC, FR, FutR>(
    mut rx: mpsc::Receiver<CommunityMembershipDelta>,
    on_membership: FM,
    on_channel_config: FC,
    on_registry: FR,
)
where
    FM: Fn(CommunityMembersChangedPayload) -> FutM + Send + Sync + 'static,
    FutM: std::future::Future<Output = ()> + Send,
    FC: Fn(ChannelConfigChangedPayload) -> FutC + Send + Sync + 'static,
    FutC: std::future::Future<Output = ()> + Send,
    FR: Fn(ChannelConfigChangedPayload) -> FutR + Send + Sync + 'static,
    FutR: std::future::Future<Output = ()> + Send,
{
    while let Some(delta) = rx.recv().await {
        // Existing membership projection unchanged.
        if let Some((community_id, change)) = delta_to_change(&delta) {
            on_membership(CommunityMembersChangedPayload {
                community_id,
                changes: vec![change],
            }).await;
        }

        // Existing channel-config projection unchanged.
        if let Some(channel_payload) = delta_to_channel_config_change(&delta) {
            on_channel_config(channel_payload.clone()).await;
            // NEW: also fire registry hook on Created / Deleted.
            on_registry(channel_payload).await;
        }
    }
}
```

(The exact signature may vary — adapt to whatever the existing code looks like. The important change is: add the 3rd callback and call it after `on_channel_config`.)

- [ ] **Step 9: Update the existing call site at lib.rs:1230**

Find the existing `tokio::spawn(run_community_delta_consumer(...))` invocation. Add the third callback. The third callback needs access to the `channel_log_registry` (a new `Arc<ChannelLogRegistry<R>>` you'll have stored in `NodeState`).

For now, write a placeholder closure that we'll wire fully in Step 11:

```rust
{
    let app_for_membership = app.clone();
    let app_for_channel_config = app.clone();
    let registry_for_hook = Arc::clone(&channel_log_registry);
    let community_registry_for_hook = Arc::clone(&community_registry);
    tokio::spawn(run_community_delta_consumer(
        community_delta_rx,
        move |payload| {
            let app = app_for_membership.clone();
            async move {
                if let Err(e) = app.emit("community-members-changed", &payload) {
                    tracing::warn!(error = ?e, "failed to emit community-members-changed");
                }
            }
        },
        move |payload| {
            let app = app_for_channel_config.clone();
            async move {
                if let Err(e) = app.emit("channel-config-updated", &payload) {
                    tracing::warn!(error = ?e, "failed to emit channel-config-updated");
                }
            }
        },
        move |payload: ChannelConfigChangedPayload| {
            let registry = Arc::clone(&registry_for_hook);
            let community_registry = Arc::clone(&community_registry_for_hook);
            async move {
                let cid_bytes: [u8; 16] = match hex::decode(&payload.community_id)
                    .ok()
                    .and_then(|v| v.try_into().ok())
                {
                    Some(b) => b,
                    None => return,
                };
                let chid_bytes: [u8; 16] = match hex::decode(&payload.channel_id)
                    .ok()
                    .and_then(|v| v.try_into().ok())
                {
                    Some(b) => b,
                    None => return,
                };
                let cid = SpaceId(cid_bytes);
                let chid = ChannelId(chid_bytes);

                match payload.action {
                    ChannelConfigChangeAction::Created => {
                        // Look up community engine to get membership_key + state + tracker.
                        let community_engine = match community_registry.engine_arc(&cid).await {
                            Some(e) => e,
                            None => {
                                tracing::warn!(
                                    community_id = %payload.community_id,
                                    "channel-config Created: no community engine"
                                );
                                return;
                            }
                        };
                        let membership_key = community_engine.membership_key();
                        let key = crate::community_channel_log::derive_channel_key(
                            &membership_key, &cid, &chid,
                        );
                        let state_at_hlc = community_engine.state_at_hlc_resolver();
                        let resolver = community_engine.identity_resolver();
                        let hlc_tracker = community_engine.tracker_arc();
                        if let Err(e) = registry
                            .spawn(cid, chid, key, state_at_hlc, resolver, hlc_tracker)
                            .await
                        {
                            tracing::warn!(error = ?e, "channel-log spawn failed");
                        }
                    }
                    ChannelConfigChangeAction::Modified => {
                        // No-op: channel-config metadata changed but
                        // the underlying log is unaffected.
                    }
                    ChannelConfigChangeAction::Deleted => {
                        if let Err(e) = registry.stop(&cid, &chid).await {
                            tracing::warn!(error = ?e, "channel-log stop failed");
                        }
                    }
                }
            }
        },
    ));
}
```

> **Note:** `community_engine.membership_key()`, `state_at_hlc_resolver()`, `identity_resolver()` may need to be added to `CommunitySyncEngine` as accessor methods. Phase 2's verify_channel_event already takes these traits from somewhere — find that call site (likely in tests or the engine's own internal verify path) and either reuse or add the accessor.

- [ ] **Step 10: Construct `channel_log_registry` in `start_node`**

In `start_node` (search for `let community_registry =`), after the community registry is built, add:

```rust
let channel_log_registry: Arc<ChannelLogRegistry<R>> = ChannelLogRegistry::new(
    ChannelLogRegistryConfig {
        session: Arc::clone(&session),
        app: app.clone(),
        identity_dir: identity_dir.clone(),
        self_owner,
        self_device_id: device_id.clone(),
        signing_key: Arc::clone(&signing_key_arc),
        engine_config: ChannelLogEngineConfig::default(),
    },
);
```

Store it on `NodeState`:

```rust
pub struct NodeState {
    // ...existing fields...
    pub channel_log_registry: Option<Arc<ChannelLogRegistry<tauri::Wry>>>,
}
```

(Use the appropriate Runtime parameter — `tauri::Wry` is the production default.)

Set it during `start_node`:

```rust
guard.channel_log_registry = Some(Arc::clone(&channel_log_registry));
```

Clear it during `stop_inner`:

```rust
guard.channel_log_registry.take();
```

- [ ] **Step 11: Wire `reconcile_from_state` after each community engine spawn**

In `start_node`, find the loop that spawns community engines (search for `community_registry.spawn_engine`). After each successful spawn, add:

```rust
if let Some(community_engine) = community_registry.engine_arc(&community_id).await {
    let community_state = community_engine.state().lock().await.clone();
    let membership_key = community_engine.membership_key();
    let state_at_hlc = community_engine.state_at_hlc_resolver();
    let resolver = community_engine.identity_resolver();
    let hlc_tracker = community_engine.tracker_arc();
    if let Err(e) = channel_log_registry
        .reconcile_from_state(
            community_id,
            &community_state,
            &membership_key,
            state_at_hlc,
            resolver,
            hlc_tracker,
        )
        .await
    {
        tracing::warn!(
            community_id = ?community_id,
            error = ?e,
            "channel-log registry reconcile failed"
        );
    }
}
```

- [ ] **Step 12: Update existing tests of `run_community_delta_consumer`**

Find tests that call `run_community_delta_consumer` (e.g., `run_community_delta_consumer_routes_channel_config_to_correct_callback` at lib.rs:10234). Each needs a 3rd callback argument — pass a no-op for callsites that don't care:

```rust
|_payload: ChannelConfigChangedPayload| async {}
```

Update the assertion checks if any test was watching for callback ordering.

- [ ] **Step 13: Verify all gates green**

```bash
cd src-tauri && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace --no-fail-fast 2>&1 | tail -30
```

Expected: all green.

- [ ] **Step 14: Commit**

```bash
git add src-tauri/src/community_channel_log_engine.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-270): ChannelLogRegistry + lifecycle binding

ChannelLogRegistry per spec §7: spawn (idempotent), stop (clean discard
per §17.4), engine, shutdown_all, reconcile_from_state. Adapter handles
+ closing flags tracked alongside engine refs.

Lifecycle binding extends run_community_delta_consumer at lib.rs:1230
with a 3rd callback that fires registry.spawn on
ChannelConfigChangeAction::Created and registry.stop on Deleted.
Modified is a no-op for the registry (channel-config metadata
changed; log is unaffected).

start_node constructs the registry alongside CommunitySyncRegistry,
stores it on NodeState, and calls reconcile_from_state after each
community engine spawn so reload restores all live channels.

Existing tests of run_community_delta_consumer updated for the new
3-callback signature.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Three IPCs + DTOs + invoke_handler registration

**Files:**
- Modify: `src-tauri/src/lib.rs` (3 IPC commands; register in `tauri::Builder::invoke_handler`)

**Goal:** Wire the engine surface to Tauri IPC for the frontend (Phase 4) to consume.

- [ ] **Step 1: Find the existing `invoke_handler` registration**

```bash
grep -n "invoke_handler" src-tauri/src/lib.rs | head -3
```

Expected: one line referencing `tauri::generate_handler![...]` or similar.

- [ ] **Step 2: Find an existing IPC for pattern reference**

```bash
sed -n '5530,5610p' src-tauri/src/lib.rs   # create_channel from Phase 1
```

Note the validation pattern, error mapping, NodeState lookup. Mirror it.

- [ ] **Step 3: Write failing test for `post_channel_message` round-trip**

Add a new test module section in `lib.rs` (or a sibling test file). The pattern from Phase 1's IPC tests is the canonical reference; copy that scaffold.

```rust
#[cfg(test)]
mod channel_message_ipc_tests {
    use super::*;
    use tauri::Manager;

    #[tokio::test]
    async fn post_channel_message_returns_message_id() {
        let app = tauri::test::mock_builder()
            .invoke_handler(tauri::generate_handler![
                post_channel_message,
                list_channel_messages,
                request_channel_backfill,
            ])
            .build(tauri::generate_context!())
            .expect("build");

        // Prepare NodeState with a running registry + engine for a
        // test community/channel. (Reuses Phase 1's IPC test scaffold
        // — find `start_test_node` or similar in existing tests.)
        let _state = setup_test_node_with_one_channel(&app).await;

        let result: Result<String, String> = tauri::test::get_ipc_response(
            app.handle(),
            tauri::test::InvokePayload::new(
                "post_channel_message",
                serde_json::json!({
                    "communityId": "aabb".repeat(8),
                    "channelId": "ccdd".repeat(8),
                    "body": vec![104, 105],   // "hi"
                    "replyTo": null,
                }),
            ),
        )
        .await
        .map(|v| serde_json::from_value(v).unwrap())
        .map_err(|e| e.to_string());

        let msg_id = result.expect("post should succeed");
        assert_eq!(msg_id.len(), 32, "MessageId hex is 32 chars");
    }
}
```

(`setup_test_node_with_one_channel` is a helper you'll need to write — model it after the existing Phase 1 IPC test scaffolding. If Phase 1 doesn't have a helper for this exact case, write one that calls `start_node` with a mock identity, creates a community via `create_community_inner`, and waits for the default `#general` channel to materialize.)

- [ ] **Step 4: Run; expect compile error (`post_channel_message` doesn't exist)**

- [ ] **Step 5: Implement the three IPCs**

Add to `src-tauri/src/lib.rs` (in the IPC section, near `create_channel`):

```rust
/// Tauri IPC: post a message to a channel.
///
/// The body is opaque bytes (frontend serializes the display
/// format — text, markdown, etc.). `reply_to` is an optional
/// hex MessageId of an earlier message in the same channel.
#[tauri::command]
async fn post_channel_message(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    channel_id: String,
    body: Vec<u8>,
    reply_to: Option<String>,
) -> Result<String, String> {
    if community_id.len() != 32 {
        return Err("community_id must be 16 bytes (32 hex chars)".to_string());
    }
    if channel_id.len() != 32 {
        return Err("channel_id must be 16 bytes (32 hex chars)".to_string());
    }
    let cid_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .try_into()
        .map_err(|_| "community_id length wrong".to_string())?;
    let chid_bytes: [u8; 16] = hex::decode(&channel_id)
        .map_err(|e| format!("invalid channel_id hex: {e}"))?
        .try_into()
        .map_err(|_| "channel_id length wrong".to_string())?;
    let cid = crate::owner_state_types::SpaceId(cid_bytes);
    let chid = crate::community_membership::ChannelId(chid_bytes);

    let reply_to_msg_id = match reply_to {
        Some(s) => {
            if s.len() != 32 {
                return Err("reply_to must be 16 bytes (32 hex chars)".to_string());
            }
            let bytes: [u8; 16] = hex::decode(&s)
                .map_err(|e| format!("invalid reply_to hex: {e}"))?
                .try_into()
                .map_err(|_| "reply_to length wrong".to_string())?;
            Some(crate::community_channel_log::MessageId(bytes))
        }
        None => None,
    };

    let registry = {
        let guard = state_lock
            .lock()
            .map_err(|_| "state lock poisoned".to_string())?;
        guard
            .channel_log_registry
            .as_ref()
            .ok_or_else(|| "channel_log_registry missing — node not running".to_string())?
            .clone()
    };

    let engine = registry
        .engine(&cid, &chid)
        .await
        .ok_or_else(|| format!("no engine for {community_id}/{channel_id}"))?;

    let msg_id = engine
        .publish(body, reply_to_msg_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(hex::encode(msg_id.0))
}

/// Tauri IPC: list locally-known messages in a channel.
///
/// `since` is the HLC to filter by; `None` means "from earliest
/// available locally". `limit` caps results — `0` means "use
/// server default (256)"; hard cap 1000.
#[tauri::command]
async fn list_channel_messages(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    channel_id: String,
    since: Option<crate::community_channel_log_engine::HlcDto>,
    limit: u32,
) -> Result<Vec<crate::community_channel_log_engine::ChannelMessageDto>, String> {
    if limit > 1000 {
        return Err(format!("limit {limit} exceeds max 1000"));
    }

    if community_id.len() != 32 {
        return Err("community_id must be 16 bytes (32 hex chars)".to_string());
    }
    if channel_id.len() != 32 {
        return Err("channel_id must be 16 bytes (32 hex chars)".to_string());
    }
    let cid_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .try_into()
        .map_err(|_| "community_id length wrong".to_string())?;
    let chid_bytes: [u8; 16] = hex::decode(&channel_id)
        .map_err(|e| format!("invalid channel_id hex: {e}"))?
        .try_into()
        .map_err(|_| "channel_id length wrong".to_string())?;
    let cid = crate::owner_state_types::SpaceId(cid_bytes);
    let chid = crate::community_membership::ChannelId(chid_bytes);

    let registry = {
        let guard = state_lock
            .lock()
            .map_err(|_| "state lock poisoned".to_string())?;
        guard
            .channel_log_registry
            .as_ref()
            .ok_or_else(|| "channel_log_registry missing — node not running".to_string())?
            .clone()
    };

    let engine = registry
        .engine(&cid, &chid)
        .await
        .ok_or_else(|| format!("no engine for {community_id}/{channel_id}"))?;

    let since_hlc = since.map(|h| crate::owner_state_types::Hlc {
        wall_ms: h.wall_ms,
        logical: h.logical,
        device_id: h.device_id,
    });

    let events = engine
        .list_messages(since_hlc, limit as usize)
        .await
        .map_err(|e| e.to_string())?;

    // Project events to DTOs. Each event has plaintext available
    // because list_messages returns SignedChannelEvent::Post
    // variants where Phase 2's storage retains plaintext alongside
    // ciphertext (or returns it from decrypt). Use the engine's
    // helper.
    Ok(events.into_iter().map(|ev| engine.event_to_dto(&ev)).collect())
}

/// Tauri IPC: fire a backfill request via Zenoh queryable.
/// Fire-and-forget — replies stream back via `channel-message-received`
/// Tauri events.
#[tauri::command]
async fn request_channel_backfill(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    channel_id: String,
    since: Option<crate::community_channel_log_engine::HlcDto>,
) -> Result<(), String> {
    if community_id.len() != 32 {
        return Err("community_id must be 16 bytes (32 hex chars)".to_string());
    }
    if channel_id.len() != 32 {
        return Err("channel_id must be 16 bytes (32 hex chars)".to_string());
    }
    let cid_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .try_into()
        .map_err(|_| "community_id length wrong".to_string())?;
    let chid_bytes: [u8; 16] = hex::decode(&channel_id)
        .map_err(|e| format!("invalid channel_id hex: {e}"))?
        .try_into()
        .map_err(|_| "channel_id length wrong".to_string())?;
    let cid = crate::owner_state_types::SpaceId(cid_bytes);
    let chid = crate::community_membership::ChannelId(chid_bytes);

    let registry = {
        let guard = state_lock
            .lock()
            .map_err(|_| "state lock poisoned".to_string())?;
        guard
            .channel_log_registry
            .as_ref()
            .ok_or_else(|| "channel_log_registry missing — node not running".to_string())?
            .clone()
    };

    let engine = registry
        .engine(&cid, &chid)
        .await
        .ok_or_else(|| format!("no engine for {community_id}/{channel_id}"))?;

    let since_hlc = since.map(|h| crate::owner_state_types::Hlc {
        wall_ms: h.wall_ms,
        logical: h.logical,
        device_id: h.device_id,
    });

    engine
        .request_backfill(since_hlc)
        .await
        .map_err(|e| e.to_string())
}
```

(`engine.event_to_dto` is a public method to add — pulls plaintext + DTO fields from the SignedChannelEvent in a way symmetric with the existing emit helper.)

- [ ] **Step 6: Register the IPCs in `tauri::Builder::invoke_handler`**

Find the existing `tauri::generate_handler![...]` call. Add the three new IPCs:

```rust
.invoke_handler(tauri::generate_handler![
    // ... existing IPCs ...
    create_channel,
    modify_channel,
    delete_channel,
    list_channels,
    // NEW (ZEB-270 Phase 3):
    post_channel_message,
    list_channel_messages,
    request_channel_backfill,
    // ...
])
```

- [ ] **Step 7: Run the IPC test**

```bash
cd src-tauri && cargo test --lib channel_message_ipc 2>&1 | tail -20
```

Expected: pass.

- [ ] **Step 8: Verify gates green**

```bash
cd src-tauri && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && echo "GATES GREEN"
```

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-270): post_channel_message + list_channel_messages + request_channel_backfill IPCs

Three IPCs per spec §9. Each looks up the channel engine via the
NodeState-held ChannelLogRegistry and forwards to engine.publish /
engine.list_messages / engine.request_backfill.

DTOs (ChannelMessageDto, HlcDto, ChannelMessageReceivedPayload,
ChannelBackfillProgressPayload) live in community_channel_log_engine.rs
and are re-exported through the IPC return types.

IPC-boundary validation (hex length, limit cap) catches malformed
input before reaching the engine. Tauri snake_case ↔ camelCase
boundary auto-conversion preserved (per repo convention).

Registered in tauri::Builder::invoke_handler alongside Phase 1's
channel-config IPCs.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Two-engine integration test + wire-format pin

**Files:**
- Create: `src-tauri/tests/community_channel_messages_integration.rs`
- Modify: `src-tauri/tests/wire_format_channel_log_fixtures.rs` (add backfill-reply pin)

**Goal:** End-to-end coverage of live broadcast + offline-then-backfill + replay rejection per spec §14.2; drift-guard the backfill packet wire shape.

### Sub-task 6A: Two-engine integration test

- [ ] **Step 1: Read the canonical two-engine fixture pattern**

```bash
grep -n "build_fixture\|two-engine\|two_engine" src-tauri/tests/community_sync_integration.rs | head -10
```

The Phase 1/2 integration tests already build two `CommunitySyncEngine`s on a shared in-memory Zenoh router; copy that scaffolding.

- [ ] **Step 2: Create the integration test file**

Create `src-tauri/tests/community_channel_messages_integration.rs`:

```rust
//! ZEB-270 Phase 3 integration test: two ChannelLogEngines on a
//! shared in-memory Zenoh router exercise live broadcast,
//! offline-then-backfill, and replay rejection.
//!
//! Per spec §14.2.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use harmony_client::community_channel_log::{
    derive_channel_key, encrypt_channel_packet, sign_channel_event, ChannelLogConfig,
    SignedChannelEvent,
};
use harmony_client::community_channel_log_engine::{
    ChannelLogEngineConfig, ChannelLogRegistry, ChannelLogRegistryConfig,
};
use harmony_client::community_membership::ChannelId;
use harmony_client::community_state_sync::CommunityRootHlcTracker;
use harmony_client::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};
use harmony_identity::PrivateIdentity;
use tauri::Manager;
use tempfile::TempDir;
use tokio::sync::Mutex;

const TEST_SEAL_THRESHOLD: usize = 8;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_engines_live_then_offline_backfill_with_replay_rejection() {
    // ── Set up shared in-memory Zenoh router ───────────────────────
    let cfg = zenoh::Config::default();
    let session_a = Arc::new(zenoh::open(cfg.clone()).await.expect("session A"));
    let session_b = Arc::new(zenoh::open(cfg).await.expect("session B"));

    // ── Set up identities A + B ────────────────────────────────────
    let mut seed_a = [0u8; 32];
    seed_a[0] = 0xAA;
    let identity_a = PrivateIdentity::from_seed(&seed_a).expect("identity A");
    let owner_a = OwnerAddr(identity_a.public_addr());

    let mut seed_b = [0u8; 32];
    seed_b[0] = 0xBB;
    let identity_b = PrivateIdentity::from_seed(&seed_b).expect("identity B");
    let owner_b = OwnerAddr(identity_b.public_addr());

    let signing_a = Arc::new(SigningKey::from_bytes(
        identity_a.ed25519_signing_key_bytes().as_slice().try_into().unwrap(),
    ));
    let signing_b = Arc::new(SigningKey::from_bytes(
        identity_b.ed25519_signing_key_bytes().as_slice().try_into().unwrap(),
    ));

    // ── Set up shared community + channel ──────────────────────────
    let community_id = SpaceId([0xc0; 16]);
    let channel_id = ChannelId([0xc1; 16]);
    let membership_key = MembershipKey([0x77; 32]);
    let channel_key = derive_channel_key(&membership_key, &community_id, &channel_id);

    // ── Set up Tauri mock apps for each side ──────────────────────
    let app_a = tauri::test::mock_app();
    let app_b = tauri::test::mock_app();

    let dir_a = TempDir::new().expect("tmp A");
    let dir_b = TempDir::new().expect("tmp B");

    // ── Construct stubs (verify-chain shortcuts for the test) ─────
    let state_a: Arc<dyn harmony_client::community_channel_log::CommunityStateAtHlc + Send + Sync> =
        Arc::new(BothJoinedState { a: owner_a, b: owner_b });
    let state_b: Arc<dyn harmony_client::community_channel_log::CommunityStateAtHlc + Send + Sync> =
        Arc::new(BothJoinedState { a: owner_a, b: owner_b });

    let mut resolver_map = std::collections::HashMap::new();
    resolver_map.insert(owner_a, identity_a.public_bytes_composite());
    resolver_map.insert(owner_b, identity_b.public_bytes_composite());
    let resolver: Arc<dyn harmony_client::community_channel_log::ChannelIdentityResolver + Send + Sync> =
        Arc::new(SharedResolver { map: resolver_map });

    let tracker_a = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let tracker_b = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));

    // ── Build registries ──────────────────────────────────────────
    let registry_a = ChannelLogRegistry::new(ChannelLogRegistryConfig {
        session: Arc::clone(&session_a),
        app: app_a.handle().clone(),
        identity_dir: dir_a.path().to_path_buf(),
        self_owner: owner_a,
        self_device_id: "device-a".to_string(),
        signing_key: Arc::clone(&signing_a),
        engine_config: ChannelLogEngineConfig {
            log_config: ChannelLogConfig {
                seal_threshold_events: TEST_SEAL_THRESHOLD,
            },
            ..Default::default()
        },
    });
    let registry_b = ChannelLogRegistry::new(ChannelLogRegistryConfig {
        session: Arc::clone(&session_b),
        app: app_b.handle().clone(),
        identity_dir: dir_b.path().to_path_buf(),
        self_owner: owner_b,
        self_device_id: "device-b".to_string(),
        signing_key: Arc::clone(&signing_b),
        engine_config: ChannelLogEngineConfig {
            log_config: ChannelLogConfig {
                seal_threshold_events: TEST_SEAL_THRESHOLD,
            },
            ..Default::default()
        },
    });

    let engine_a = Arc::clone(&registry_a)
        .spawn(community_id, channel_id, channel_key.clone(),
               Arc::clone(&state_a), Arc::clone(&resolver), Arc::clone(&tracker_a))
        .await
        .expect("spawn A");
    let engine_b = Arc::clone(&registry_b)
        .spawn(community_id, channel_id, channel_key.clone(),
               Arc::clone(&state_b), Arc::clone(&resolver), Arc::clone(&tracker_b))
        .await
        .expect("spawn B");

    // ── Listen for B's channel-message-received events ────────────
    let received_b: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let received_b_for_listener = Arc::clone(&received_b);
    let _unlisten = app_b.handle().listen("channel-message-received", move |event| {
        let payload: serde_json::Value =
            serde_json::from_str(event.payload()).expect("parse payload");
        let msg_id = payload["message"]["messageId"].as_str().expect("messageId").to_string();
        let received = Arc::clone(&received_b_for_listener);
        tauri::async_runtime::spawn(async move {
            received.lock().await.push(msg_id);
        });
    });

    // Give Zenoh subscribers + queryables time to declare.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ── Phase 1: A posts 100 messages live ────────────────────────
    let mut posted_ids = Vec::new();
    for i in 0..100 {
        let id = Arc::clone(&engine_a)
            .publish(format!("msg-{i}").into_bytes(), None)
            .await
            .expect("publish");
        posted_ids.push(id);
    }

    // Wait for B to receive all 100.
    wait_until(
        || async { received_b.lock().await.len() >= 100 },
        Duration::from_secs(10),
    )
    .await
    .expect("B should receive 100 live");

    // ── Phase 2: simulate B disconnect; A posts 50 more ───────────
    // Drop B's subscriber. The cleanest way is to call registry_b.stop
    // and then re-spawn — but we want to keep B's local log intact
    // for the backfill phase. Instead: just stop A's adapter for B
    // by closing the closing flag temporarily? Simpler: drop B's
    // session and create a new one for the reconnect phase.
    //
    // For test simplicity, use registry_b.stop + re-spawn pattern.
    registry_b.stop(&community_id, &channel_id).await.expect("stop B");

    for i in 100..150 {
        Arc::clone(&engine_a)
            .publish(format!("msg-{i}").into_bytes(), None)
            .await
            .expect("publish offline");
    }

    let received_at_offline_end = received_b.lock().await.len();
    assert!(
        received_at_offline_end >= 100 && received_at_offline_end <= 105,
        "B should be at ~100 received during offline (got {received_at_offline_end})"
    );

    // ── Phase 3: B reconnects + backfill ──────────────────────────
    let engine_b2 = Arc::clone(&registry_b)
        .spawn(community_id, channel_id, channel_key.clone(),
               Arc::clone(&state_b), Arc::clone(&resolver), Arc::clone(&tracker_b))
        .await
        .expect("re-spawn B");

    tokio::time::sleep(Duration::from_millis(500)).await;

    Arc::clone(&engine_b2)
        .request_backfill(None)
        .await
        .expect("backfill");

    wait_until(
        || async { received_b.lock().await.len() >= 150 },
        Duration::from_secs(15),
    )
    .await
    .expect("B should receive all 150 after backfill");

    // ── Phase 4: replay attack ────────────────────────────────────
    // Capture one packet from A's broadcast; replay to B.
    // Easiest: take the first message ID and re-encrypt + manually
    // inject through B's adapter subscriber (we don't have direct
    // access — instead, use Zenoh: A's session puts the same packet
    // again, B should reject as replay).
    let first_id = posted_ids[0];
    let first_event = engine_a
        .list_messages(None, 1)
        .await
        .expect("list a")
        .into_iter()
        .find(|ev| ev.id() == first_id)
        .expect("first event");
    let replay_packet = encrypt_channel_packet(&channel_key, &first_event).expect("re-encrypt");
    let topic = format!(
        "harmony/channels/{}/{}/events",
        hex::encode(community_id.0),
        hex::encode(channel_id.0)
    );
    let topic_key = zenoh::key_expr::KeyExpr::try_from(topic).expect("key");
    session_a.put(&topic_key, replay_packet).await.expect("replay put");

    tokio::time::sleep(Duration::from_secs(1)).await;

    let final_count = received_b.lock().await.len();
    assert_eq!(
        final_count, 150,
        "B should not double-emit replayed event (got {final_count})"
    );

    // ── Final state check ─────────────────────────────────────────
    let final_listed = engine_b2
        .list_messages(None, 200)
        .await
        .expect("final list");
    assert_eq!(
        final_listed.len(),
        150,
        "B's log should contain exactly 150 events"
    );

    // Verify HLC ordering.
    for window in final_listed.windows(2) {
        assert!(
            window[1].at().is_strictly_newer_than(window[0].at()),
            "log out of HLC order"
        );
    }
}

struct BothJoinedState {
    a: OwnerAddr,
    b: OwnerAddr,
}

impl harmony_client::community_channel_log::CommunityStateAtHlc for BothJoinedState {
    fn is_joined(&self, author: &OwnerAddr, _at: &Hlc) -> bool {
        author == &self.a || author == &self.b
    }
    fn write_power_for(&self, _author: &OwnerAddr, _at: &Hlc) -> u8 {
        100
    }
}

struct SharedResolver {
    map: std::collections::HashMap<OwnerAddr, [u8; 64]>,
}

impl harmony_client::community_channel_log::ChannelIdentityResolver for SharedResolver {
    fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        self.map.get(addr).copied()
    }
}

async fn wait_until<F, Fut>(mut predicate: F, timeout: Duration) -> Result<(), ()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if predicate().await {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
```

(If `app.handle().listen` API differs in Tauri 2 — it does — adapt to the actual `listen_global` or `WindowExt::listen` pattern. Phase 1's IPC tests use the canonical shape.)

- [ ] **Step 3: Run the integration test**

```bash
cd src-tauri && cargo test --test community_channel_messages_integration 2>&1 | tail -30
```

Expected: pass within ~30s. If the test times out, the most common cause is Zenoh subscriber-declare latency — bump the initial sleep to 1s.

### Sub-task 6B: Wire-format pin

- [ ] **Step 4: Locate the existing wire-format pin file**

```bash
cat src-tauri/tests/wire_format_channel_log_fixtures.rs | head -20
```

- [ ] **Step 5: Add `backfill_reply_packet_wire_bytes_pinned` test**

In `src-tauri/tests/wire_format_channel_log_fixtures.rs`, append:

```rust
/// Per spec §17.1: backfill replies are per-event packets, wire-identical
/// to live-broadcast packets. This pin asserts the format is stable.
#[test]
fn backfill_reply_packet_wire_bytes_pinned() {
    use harmony_client::community_channel_log::{
        derive_channel_key, encrypt_channel_packet, sign_channel_event,
    };
    use harmony_client::community_membership::ChannelId;
    use harmony_client::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};
    use ed25519_dalek::SigningKey;

    // Deterministic seeds — match the existing fixture file's
    // conventions for SignedChannelEvent::Post.
    let community_id = SpaceId([0xa1; 16]);
    let channel_id = ChannelId([0xb2; 16]);
    let owner = OwnerAddr([0xc3; 16]);
    let mk = MembershipKey([0x77; 32]);
    let signing_key_bytes = [0x42u8; 32];
    let signing_key = SigningKey::from_bytes(&signing_key_bytes);

    let key = derive_channel_key(&mk, &community_id, &channel_id);

    let event = sign_channel_event(
        community_id,
        channel_id,
        owner,
        Hlc { wall_ms: 1, logical: 0, device_id: "fixture".to_string() },
        b"hello".to_vec(),
        None,
        &signing_key,
    )
    .expect("sign");

    // Use the same fixed nonce the existing live-broadcast pin uses.
    // (Phase 2's encrypt_channel_packet uses a random nonce — for
    // deterministic pin, use a sibling helper or seed the RNG.)
    let packet = encrypt_channel_packet_with_nonce(
        &key,
        &event,
        [0u8; 12],
    )
    .expect("encrypt");

    let expected_hex = std::env::var("UPDATE_BACKFILL_FIXTURE")
        .map(|_| {
            // When updating: print and copy.
            let h = hex::encode(&packet);
            eprintln!("UPDATE_BACKFILL_FIXTURE: {h}");
            h
        })
        .unwrap_or_else(|_| {
            // PIN: replace this string with the printed hex from
            // the first run with UPDATE_BACKFILL_FIXTURE=1.
            "<BACKFILL_PIN_HEX>".to_string()
        });

    let actual_hex = hex::encode(&packet);
    assert_eq!(
        actual_hex, expected_hex,
        "backfill reply wire format drifted; re-pin via UPDATE_BACKFILL_FIXTURE=1"
    );
}
```

> **Note on the pin generation:** The first time this test runs, the placeholder `<BACKFILL_PIN_HEX>` will fail. To generate the pin:
>
> 1. Run with `UPDATE_BACKFILL_FIXTURE=1 cargo test backfill_reply_packet_wire_bytes_pinned -- --nocapture`
> 2. Read the printed hex from stderr
> 3. Replace `<BACKFILL_PIN_HEX>` with that hex literal
> 4. Re-run without the env var to confirm it pins
>
> The implementer must complete this two-step pin generation before committing.
>
> If `encrypt_channel_packet_with_nonce` doesn't exist (Phase 2 may have only the random-nonce variant), add it as a `#[cfg(test)]` helper exposed from `community_channel_log.rs`:
>
> ```rust
> #[cfg(test)]
> pub fn encrypt_channel_packet_with_nonce(
>     key: &ChannelKey,
>     event: &SignedChannelEvent,
>     nonce: [u8; 12],
> ) -> Result<Vec<u8>, ChannelEventError> {
>     // Same as encrypt_channel_packet but takes nonce explicitly.
>     // ...
> }
> ```

- [ ] **Step 6: Generate + lock the pin**

```bash
cd src-tauri && UPDATE_BACKFILL_FIXTURE=1 cargo test --test wire_format_channel_log_fixtures backfill_reply_packet -- --nocapture 2>&1 | grep "UPDATE_BACKFILL_FIXTURE"
```

Copy the printed hex into the test, replacing `<BACKFILL_PIN_HEX>`.

- [ ] **Step 7: Re-run without env var to confirm the pin holds**

```bash
cd src-tauri && cargo test --test wire_format_channel_log_fixtures backfill_reply_packet 2>&1 | tail -10
```

Expected: pass.

- [ ] **Step 8: Verify all gates green**

```bash
cd src-tauri && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace --no-fail-fast 2>&1 | tail -30
```

Expected: all green, including the new integration test and pin.

- [ ] **Step 9: Commit**

```bash
git add src-tauri/tests/community_channel_messages_integration.rs \
        src-tauri/tests/wire_format_channel_log_fixtures.rs \
        src-tauri/src/community_channel_log.rs
git commit -m "$(cat <<'EOF'
test(zeb-270): two-engine integration + backfill-packet wire pin

Integration test per spec §14.2: two ChannelLogEngines on shared
in-memory Zenoh router exercise live broadcast (A posts 100 → B
receives 100), offline-then-backfill (B disconnects, A posts 50 more,
B reconnects + backfills, sees all 150 in HLC order deduped), and
replay attack rejection (A re-publishes event 0 after backfill,
B rejects the second copy via ChannelLogReplayTracker).

Uses TEST_SEAL_THRESHOLD = 8 so 150 events produces ≥18 sealed
segments — exercises seal/reload paths during the test.

Wire-format pin per spec §14.3: backfill_reply_packet_wire_bytes_pinned
asserts the per-event reply packet format is stable. Drift-guards
against silent format changes from Phase 4 / later work.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Final verification + push + PR

**Files:** none (commit shape only)

**Goal:** Confirm all gates green, push the branch, open the PR with proper cross-references.

- [ ] **Step 1: Final pre-push gate sweep**

```bash
cd src-tauri && cargo fmt --all -- --check && \
                cargo clippy --all-targets -- -D warnings && \
                cargo test --workspace --no-fail-fast 2>&1 | tee /tmp/zeb270-final-test.log
RESULT=${PIPESTATUS[0]}
test "$RESULT" -eq 0 && echo "ALL GREEN" || echo "FINAL RED ($RESULT)"
```

Expected: `ALL GREEN`. If any test fails, do NOT push — investigate root cause.

- [ ] **Step 2: Verify commit shape**

```bash
git log --oneline origin/main..HEAD
```

Expected (in this order):

```
<sha> test(zeb-270): two-engine integration + backfill-packet wire pin
<sha> feat(zeb-270): post_channel_message + list_channel_messages + request_channel_backfill IPCs
<sha> feat(zeb-270): ChannelLogRegistry + lifecycle binding
<sha> feat(zeb-270): request_backfill API + Zenoh adapter
<sha> feat(zeb-270): ChannelLogEngine internals — receive, flush, publish, list
<sha> feat(zeb-270): ChannelLog engine module skeleton
<sha> docs(zeb-270): Phase 3 ChannelLog Zenoh transport plan
<sha> docs(zeb-270): Phase 3 ChannelLog Zenoh transport design spec
```

(The plan commit appears between spec and Task 1 if you commit it before invoking subagent-driven-development; otherwise it's at the head.)

- [ ] **Step 3: Push the branch**

```bash
git push -u origin zeb-270-channel-log-zenoh-transport
```

- [ ] **Step 4: Open the PR**

```bash
gh pr create --title "ZEB-270 Phase 3: ChannelLog Zenoh transport + IPCs" \
             --body "$(cat <<'EOF'
## Summary

Phase 3 of [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) (Sub-C v2 channels-within-communities). Wraps the in-process ChannelLog primitives shipped in [Phase 2](https://linear.app/zeblith/issue/ZEB-269) with per-channel Zenoh broadcast + queryable backfill, plus the `ChannelLogRegistry` lifecycle bound to channel-config materialize, plus three message-surface IPCs and two Tauri events.

This is the data-plane control surface that the Phase 4 frontend will consume.

### What ships

- New `ChannelLogEngine` per (community, channel) — owns the Phase 2 `ChannelLog`, drives debounced disk flush, exposes `publish` / `list_messages` / `request_backfill` / `flush_now` / `shutdown`
- New `ChannelLogRegistry` — manages per-channel engine lifecycle, idempotent spawn/stop, boot-time `reconcile_from_state`
- New `spawn_channel_log_zenoh_adapter` in `event_loop.rs` — mirrors `spawn_community_state_zenoh_adapter` for per-channel topics
- 3 IPCs: `post_channel_message`, `list_channel_messages`, `request_channel_backfill`
- 2 Tauri events: `channel-message-received`, `channel-backfill-progress`
- Lifecycle binding: extends `run_community_delta_consumer` with a 3rd callback that fires `registry.spawn` on `ChannelConfigChangeAction::Created` and `registry.stop` on `Deleted`

### Plan-time decisions locked

Per spec §17:

1. Backfill replies are per-event packets (wire-identical to live broadcast — symmetric verify path)
2. Minimal rate-limit posture for v3 (no concurrency cap; rely on `limit` parameter)
3. 250 ms tail flush debounce + 1 s max-dirty cap (matches `community_state_sync::DEFAULT_DEBOUNCE_MS`)
4. Registry discards stopped engines cleanly (no in-memory tombstones)
5. Layered `ChannelLogEngineConfig` wrapping Phase 2's `ChannelLogConfig`

### References

- Spec: `docs/specs/2026-05-09-zeb-270-channel-log-zenoh-transport-design.md`
- Plan: `docs/plans/2026-05-09-zeb-270-channel-log-zenoh-transport-plan.md`
- Parent: [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) (Sub-C v2)
- Sibling Phase 1: [ZEB-266](https://linear.app/zeblith/issue/ZEB-266), merged via PR #93
- Sibling Phase 2: [ZEB-269](https://linear.app/zeblith/issue/ZEB-269), merged via PR #95
- Sibling cross-cutting refactor: [ZEB-267](https://linear.app/zeblith/issue/ZEB-267), merged via PR #94

## Test Plan

- [ ] `cargo fmt --all -- --check` — passes
- [ ] `cargo clippy --all-targets -- -D warnings` — passes
- [ ] `cargo test --workspace --no-fail-fast` — passes (includes the new two-engine integration test + backfill wire-format pin)
- [ ] Manual smoke: build the app, create a community, post a message via the new IPC, observe `channel-message-received` event in dev tools

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 5: Capture the PR URL for the autonomous-monitoring loop**

```bash
gh pr view --json number,url,headRefName --jq '"PR #\(.number): \(.url) [\(.headRefName)]"'
```

This number + URL feeds into the autonomous-monitoring wakeup-cycle args.

- [ ] **Step 6: Schedule the autonomous-monitoring loop**

Per `feedback_autonomous_pr_monitoring_loop` memory: after `gh pr create` succeeds, schedule a 270 s wakeup to start the first poll. The loop monitors CI + bot review channels, batches fixups, pings pushover at convergence (`~/work/pushover-notify.sh "title" "body"` — note: TWO positional args, not one).

> **Implementer subagent: do not start the monitoring loop yourself. Return to the controlling agent (the one that dispatched you) and the controller handles scheduling. Your task ends with the successful `gh pr create`.**

---

## Self-review

### Spec coverage check

Walking spec sections against task coverage:

| Spec section | Covered by |
|---|---|
| §1-3 (context, scope, out-of-scope) | N/A — design-only |
| §4 (architecture overview) | Tasks 1-4 collectively realize the engine + adapter + registry split |
| §5 (module split) | Task 1 creates `community_channel_log_engine.rs`, Task 3 modifies `event_loop.rs`, Task 5 modifies `lib.rs` |
| §6 (ChannelLogEngine type + methods) | Task 1 (skeleton), Task 2 (publish + list + receive + flush), Task 3 (request_backfill) |
| §6.5 (self-loopback) | Task 2 Step 11 (publish does local append + emit, no Zenoh round-trip dependency) |
| §7 (ChannelLogRegistry) | Task 4 (struct + methods + reconcile) |
| §7.3 (lifecycle binding) | Task 4 Sub-task 4B (extend run_community_delta_consumer) |
| §7.4 (boot-time reconciliation) | Task 4 Step 11 (start_node calls reconcile_from_state per community) |
| §8 (Zenoh adapter) | Task 3 Sub-task 3B |
| §8.1 (queryable handler — engine access) | Task 3 Step 6 (`read_for_query` callback) |
| §8.2 (topic shapes) | Task 3 Step 6 (events_topic + queryable_prefix construction) |
| §9 (IPC surface) | Task 5 |
| §9.1 (DTOs) | Task 2 Step 11 defines them; Task 5 uses them in IPCs |
| §9.2 (IPC error mapping) | Task 5 (each IPC `.map_err(|e| e.to_string())`) |
| §10 (Tauri events) | Task 2 emit_message_received; backfill-progress emitted in Task 3 (adapter side, periodic) |
| §11 (persistence) | Inherited from Phase 2; Task 2 calls flush_tail/seal_and_persist |
| §12 (boot reconcile) | Task 4 Step 11 |
| §13 (error handling) | Task 2 receive loop (warn + drop), Task 2 emit_degraded |
| §14.1 (unit tests) | Task 2 covers: publish-appends, garbage-drop, replay-drop, list-ordering, debounce-coalesce, max-dirty-cap, flush_now-sync, seal-on-threshold (via integration test); Task 4 covers registry idempotency + reconcile-skips-deleted |
| §14.2 (integration test) | Task 6 Sub-task 6A |
| §14.3 (wire-format pin) | Task 6 Sub-task 6B |
| §15 (acceptance criteria) | All tasks collectively |
| §17 (plan-time decisions) | Locked in spec; referenced throughout the plan |

**Gaps surfaced during self-review:** None blocking. The `engine.event_to_dto` accessor in Task 5 needs implementation alongside the IPC code (it's referenced but not explicitly listed as a step — implementer should add it to `community_channel_log_engine.rs` as part of Task 5).

### Placeholder scan

Searched for: TBD, TODO, FIXME, XXX, "placeholder", "fill in", "implement later". Each match is annotated as a deliberate decision the implementer must make:

- Task 2 Step 11 — `body_plaintext` field handling: the spec says use option (b) (return plaintext from decrypt path). Implementer should match Phase 2's actual API.
- Task 4 Step 9 — `community_engine.membership_key()` etc. accessors may need to be added to `CommunitySyncEngine`. Annotated.
- Task 6 Step 5 — `encrypt_channel_packet_with_nonce` may need to be added as a `#[cfg(test)]` helper. Annotated with reason.
- Pin generation in Task 6 Step 5-6 — `<BACKFILL_PIN_HEX>` is a deliberate placeholder that the implementer fills via the printed-hex two-step process. Annotated with explicit instructions.

These are intentional handholds for places where the implementer must inspect the actual Phase 2 API. Not "placeholder" in the sense of "unfinished plan."

### Type-consistency check

- `ChannelLogEngine<R: tauri::Runtime>` — used consistently across Tasks 1-5 (skeleton, internals, registry construction, IPCs)
- `ChannelLogEngineParams<R>` — defined Task 1, consumed by `ChannelLogEngine::new` consistently
- `ChannelLogRegistry<R>` — defined Task 4, used in Task 5 IPCs
- `BackfillQueryRequest` — defined Task 1, used in adapter Task 3 + engine Task 3
- IPC names — `post_channel_message`, `list_channel_messages`, `request_channel_backfill` — consistent across Task 5 implementation, integration test, PR body
- DTOs — `ChannelMessageDto`, `HlcDto`, `ChannelMessageReceivedPayload`, `ChannelBackfillProgressPayload` — defined Task 2, used Task 5

No drift surfaced.

---

## Execution handoff

This plan is complete and committed. The recommended execution path is **superpowers:subagent-driven-development**: dispatch a fresh implementer subagent per task with two-stage review (spec compliance, then code quality) between tasks.

The user has pre-authorized full autonomy through PR + monitoring per `feedback_autonomous_pr_monitoring_loop`, so the controlling agent should proceed directly to subagent-driven implementation, then `gh pr create` (Task 7), then enter the autonomous bot-review monitoring loop until convergence. Pushover notify on convergence per the established pattern.
