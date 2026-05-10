//! ZEB-270 Phase 3: ChannelLog Zenoh transport engine.
//!
//! Wraps the in-process Phase 2 (ZEB-269) `ChannelLog` primitives with
//! Zenoh broadcast + queryable backfill plus the per-(community,
//! channel) lifecycle binding to channel-config materialize.
//!
//! See `docs/specs/2026-05-09-zeb-270-channel-log-zenoh-transport-design.md`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ed25519_dalek::SigningKey;
use thiserror::Error;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::JoinHandle;

use crate::community_channel_log::{
    ChannelEventError, ChannelIdentityResolver, ChannelKey, ChannelLog, ChannelLogConfig,
    ChannelLogPersistError, ChannelLogReplayTracker, CommunityStateAtHlc,
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

#[allow(dead_code)]
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
        );

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
