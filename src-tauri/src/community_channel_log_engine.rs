//! ZEB-270 Phase 3: ChannelLog Zenoh transport engine.
//!
//! Wraps the in-process Phase 2 (ZEB-269) `ChannelLog` primitives with
//! Zenoh broadcast + queryable backfill plus the per-(community,
//! channel) lifecycle binding to channel-config materialize.
//!
//! See `docs/specs/2026-05-09-zeb-270-channel-log-zenoh-transport-design.md`.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use serde::Serialize;
use thiserror::Error;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::JoinHandle;

use crate::community_channel_log::{
    decrypt_channel_packet, derive_channel_key, encrypt_channel_packet, sign_channel_event,
    verify_channel_event, ChannelEventError, ChannelIdentityResolver, ChannelKey, ChannelLog,
    ChannelLogConfig, ChannelLogPersistError, ChannelLogReplayTracker, ChannelPostPayload,
    CommunityStateAtHlc, MessageId, SignedChannelEvent,
};
use crate::community_membership::{ChannelId, MaterializedMembership};
use crate::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};

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

    #[error("body not valid UTF-8: {0}")]
    BodyNotUtf8(String),
}

// ── DTOs (used here for Tauri event emission; surfaced to IPC layer in Task 5) ──

/// Hybrid Logical Clock — wire/IPC shape.
///
/// Mirrors `owner_state_types::Hlc` but uses serde camelCase for the
/// Tauri/IPC surface. Phase 2's `Hlc` keeps single-letter wire keys
/// (`w` / `l` / `d`) for canonical-CBOR field-length parity, which is
/// the wrong shape for a TypeScript consumer.
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
///
/// **HLC tracker shape.** `hlc_tracker` is the same per-device
/// `BTreeMap<String, Hlc>` shape `dm_outbox::reserve_next_hlc_for_device`
/// expects. Plumbed through the engine so channel posts share the HLC
/// monotonicity lane with DMs and community membership events from
/// the same device. Plan/spec called this `Arc<Mutex<CommunityRootHlcTracker>>`,
/// but `CommunityRootHlcTracker` keys by `(OwnerAddr, String)` for
/// cross-device replay-rejection — a different concern from the local
/// HLC reservation tracker. See ZEB-267 for the rationale; existing
/// callers (`send_dm`, `create_community`, `redeem_invite`, channel-
/// config IPCs) all use this same per-device tracker.
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
    pub hlc_tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
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

/// Hard cap on a single channel post body. Spec §13 mentions "e.g.
/// 64 KiB"; this is the active value Phase 4 will surface in the IPC
/// error mapping.
const MAX_BODY_BYTES: usize = 64 * 1024;

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
    hlc_tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
    app: tauri::AppHandle<R>,
    config: ChannelLogEngineConfig,

    publisher_tx: mpsc::Sender<Vec<u8>>,
    query_request_tx: mpsc::Sender<BackfillQueryRequest>,

    receive_handle: Mutex<Option<JoinHandle<()>>>,
    flush_handle: Mutex<Option<JoinHandle<()>>>,

    flush_dirty: Arc<Notify>,
    closing: Arc<AtomicBool>,
    /// Wakeup signal for background loops to re-check `closing`.
    /// Both the receive and flush loops `.notified()` on this in
    /// their outer `select!` instead of polling the closing flag on
    /// a 1s timer. `shutdown` calls `notify_waiters` (NOT
    /// `notify_one`) after flipping `closing` so both loops wake.
    /// `closing` remains the source of truth; `Notify` only ensures
    /// prompt wakeup so the next `closing.load()` happens within ms.
    closing_notify: Arc<Notify>,
}

impl<R: tauri::Runtime> ChannelLogEngine<R> {
    /// Construct + spawn receive/flush loops. Takes ownership of the
    /// subscriber receiver from `params`; the registry passes one end
    /// of an `mpsc::channel` and keeps the sender in the adapter.
    pub async fn new(
        params: ChannelLogEngineParams<R>,
    ) -> Result<Arc<Self>, ChannelLogEngineError> {
        let log = ChannelLog::new(
            params.community_id,
            params.channel_id,
            params.root_dir,
            params.config.log_config.clone(),
        );

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
            closing_notify: Arc::new(Notify::new()),
        });

        // Spawn receive loop, taking ownership of subscriber_rx.
        let receive_handle = {
            let me = Arc::clone(&engine);
            let mut rx = params.subscriber_rx;
            let closing = Arc::clone(&engine.closing);
            let closing_notify = Arc::clone(&engine.closing_notify);
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        maybe = rx.recv() => {
                            let Some(packet) = maybe else { break; };
                            me.process_inbound_packet(packet).await;
                        }
                        _ = closing_notify.notified() => {
                            if closing.load(Ordering::SeqCst) { break; }
                        }
                    }
                }
            })
        };
        *engine.receive_handle.lock().await = Some(receive_handle);

        let flush_handle = engine.spawn_flush_loop();
        *engine.flush_handle.lock().await = Some(flush_handle);

        Ok(engine)
    }

    /// Signal closing + drop all internal task handles.
    ///
    /// Mirrors `community_state_sync::CommunitySyncEngine::shutdown` and
    /// `owner_state_sync::SyncEngine::shutdown` — we DO NOT `handle.await`
    /// the JoinHandles. Awaiting from a different runtime than the spawn-
    /// runtime risks deadlocking under future tokio releases. The closing
    /// flag + notify already signals the receive + flush loops to exit;
    /// the synchronous-rendezvous flush guarantee comes from the explicit
    /// `flush_now()` call below — by the time `shutdown` returns, the
    /// in-memory tail has been written to disk.
    pub async fn shutdown(&self) -> Result<(), ChannelLogEngineError> {
        self.closing.store(true, Ordering::SeqCst);
        // Wake BOTH background loops (receive + flush) so they re-check
        // `closing` and exit promptly. `notify_waiters` (vs `notify_one`)
        // delivers to every current waiter; we have two.
        self.closing_notify.notify_waiters();
        self.flush_dirty.notify_one();

        // Force-flush the tail synchronously BEFORE dropping the flush
        // task handle. The flush loop may still be inside its debounce
        // window — drop alone wouldn't force it to write. Calling
        // flush_now serializes with the loop via the same `log` mutex,
        // so whichever side gets the lock first writes the same bytes.
        self.flush_now().await?;

        let _ = self.receive_handle.lock().await.take();
        let _ = self.flush_handle.lock().await.take();
        Ok(())
    }

    pub fn community_id(&self) -> SpaceId {
        self.community_id
    }

    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    /// Read events in HLC order from segments + tail back to `since`.
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
        // on-disk; sorted ascending by `range.0`). For correct HLC-
        // order iteration we walk segments first, then tail.
        let mut out: Vec<SignedChannelEvent> = Vec::new();

        for seg in &log.manifest.segments {
            if let Some(since_hlc) = &since {
                // Phase 2's SegmentDescriptor.range = (first_hlc, last_hlc).
                // Skip segments entirely older-than-or-equal-to `since` —
                // they have no events strictly newer than `since` to
                // contribute. (No is_strictly_older_than method on Hlc;
                // express via !is_strictly_newer_than on the last-event
                // bound.)
                if !seg.range.1.is_strictly_newer_than(since_hlc) {
                    continue;
                }
            }
            let events = log
                .read_segment(seg)
                .map_err(ChannelLogEngineError::Persist)?;
            for ev in events {
                if let Some(since_hlc) = &since {
                    let SignedChannelEvent::Post { at, .. } = &ev;
                    if !at.is_strictly_newer_than(since_hlc) {
                        continue;
                    }
                }
                out.push(ev);
                if out.len() >= effective_limit {
                    return Ok(out);
                }
            }
        }

        // Then walk the in-memory tail.
        for ev in &log.tail {
            if let Some(since_hlc) = &since {
                let SignedChannelEvent::Post { at, .. } = ev;
                if !at.is_strictly_newer_than(since_hlc) {
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

    /// IPC entry: mint a Post event, sign it with self, encrypt with
    /// ChannelKey, send the packet to publisher_tx, locally append to
    /// the log, emit `channel-message-received` Tauri event.
    ///
    /// Does NOT wait for the broadcast to round-trip via Zenoh — the
    /// local log + emit are synchronous (per spec §6.5).
    pub async fn publish(
        self: &Arc<Self>,
        body: Vec<u8>,
        reply_to: Option<MessageId>,
    ) -> Result<MessageId, ChannelLogEngineError> {
        if body.len() > MAX_BODY_BYTES {
            return Err(ChannelLogEngineError::BodyTooLarge {
                len: body.len(),
                max: MAX_BODY_BYTES,
            });
        }

        // Phase 2 stores body as String inside SignedChannelEvent::Post
        // (the canonical-CBOR signature covers a `&str`). The IPC layer
        // passes raw Vec<u8> per spec §9.1 — convert via UTF-8 here so
        // non-UTF-8 callers get a clean error instead of silent garbage.
        // v3 chat bodies are UTF-8 by contract.
        let body_str = String::from_utf8(body)
            .map_err(|e| ChannelLogEngineError::BodyNotUtf8(e.to_string()))?;

        // ZEB-267: reserve next HLC under the per-device tracker lock.
        let wall_now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let hlc = crate::dm_outbox::reserve_next_hlc_for_device(
            &self.hlc_tracker,
            &self.self_device_id,
            wall_now_ms,
        )
        .await;

        // Generate a fresh 16-byte MessageId.
        let msg_id = {
            use rand::RngCore;
            let mut buf = [0u8; 16];
            rand::thread_rng().fill_bytes(&mut buf);
            MessageId(buf)
        };

        let payload = ChannelPostPayload {
            id: msg_id,
            community_id: self.community_id,
            channel_id: self.channel_id,
            author: self.self_owner,
            at: hlc,
            content_kind: 0,
            body: &body_str,
            reply_to,
        };
        let event = sign_channel_event(&payload, &self.signing_key)
            .map_err(ChannelLogEngineError::ChannelEvent)?;

        // Encrypt for broadcast.
        let packet = encrypt_channel_packet(&self.channel_key, &event)
            .map_err(ChannelLogEngineError::ChannelEvent)?;

        // Order matters here. We must bump the replay tracker BEFORE
        // putting the packet on publisher_tx — otherwise a Zenoh
        // self-loopback delivery via subscriber_rx can race
        // process_inbound_packet through verify_channel_event (which
        // calls would_accept against a not-yet-bumped tracker, passes,
        // then advances + appends + emits) before publish gets to its
        // own append, producing a duplicate tail entry persisted to
        // disk. With this ordering, any loopback packet finds the
        // tracker already at the new HLC and is dropped as Replay at
        // the receive-loop's debug level. Aligns with
        // feedback_metadata_before_irreversible_write (verify state
        // before broadcast) and feedback_two_ipc_toctou (bind through
        // server-side cached state, not via re-verification on the
        // loopback path).
        {
            let mut tracker = self.replay_tracker.lock().await;
            // Self-mint path: HLC reservation guarantees this event is
            // strictly newer than any prior on this lane (the
            // `hlc_tracker` lock above is the serialization point), so
            // would_accept is a known-true precondition and we call
            // record directly. record is unconditional insert.
            tracker.record(&event);
        }
        {
            let mut log = self.log.lock().await;
            log.append(event.clone())
                .map_err(ChannelLogEngineError::Persist)?;
        }

        // Send to adapter for Zenoh broadcast. Drop on full channel
        // (degraded mode) — local append already succeeded so the user
        // sees their own message; remote peers will catch up via
        // backfill on next reconnect.
        if let Err(e) = self.publisher_tx.try_send(packet) {
            tracing::warn!(
                community_id = ?self.community_id,
                channel_id = ?self.channel_id,
                err = ?e,
                "publisher_tx full or closed; broadcast skipped"
            );
        }

        // Notify flush loop.
        self.flush_dirty.notify_one();

        // Emit Tauri event for self-loopback (UI sees own message
        // without round-tripping through Zenoh).
        self.emit_message_received(&event);

        Ok(msg_id)
    }

    /// Force a synchronous flush, bypassing debounce. Called by
    /// `shutdown` and (Task 5) by the registry on stop.
    pub async fn flush_now(&self) -> Result<(), ChannelLogEngineError> {
        let log = self.log.lock().await;
        log.flush_tail().map_err(ChannelLogEngineError::Persist)?;
        Ok(())
    }

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

    // ── Internal helpers ────────────────────────────────────────────────

    fn emit_message_received(&self, event: &SignedChannelEvent) {
        use tauri::Emitter;
        let dto = self.message_dto_for_event(event);
        let payload = ChannelMessageReceivedPayload {
            community_id: hex::encode(self.community_id.0),
            channel_id: hex::encode(self.channel_id.0),
            message: dto,
        };
        if let Err(e) = self.app.emit("channel-message-received", &payload) {
            tracing::warn!(
                community_id = ?self.community_id,
                channel_id = ?self.channel_id,
                err = ?e,
                "failed to emit channel-message-received"
            );
        }
    }

    fn message_dto_for_event(&self, event: &SignedChannelEvent) -> ChannelMessageDto {
        // Phase 2's SignedChannelEvent::Post stores body as `String`
        // directly — encrypt_channel_packet wraps the canonical-CBOR
        // event (including the String body) under ChannelKey. By the
        // time we have an event in hand (post-decrypt or pre-encrypt),
        // the plaintext body is just `body.as_bytes().to_vec()`.
        let SignedChannelEvent::Post {
            id,
            author,
            at,
            body,
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
            body: body.as_bytes().to_vec(),
            reply_to: reply_to.map(|m| hex::encode(m.0)),
        }
    }

    fn emit_degraded(&self, reason: &str) {
        use tauri::Emitter;
        let payload = serde_json::json!({
            "communityId": hex::encode(self.community_id.0),
            "channelId": hex::encode(self.channel_id.0),
            "reason": reason,
        });
        if let Err(e) = self.app.emit("channel-log-degraded", &payload) {
            tracing::warn!(err = ?e, "failed to emit channel-log-degraded");
        }
    }

    async fn process_inbound_packet(self: &Arc<Self>, packet: Vec<u8>) {
        // 1. Decrypt.
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

        // 2. Verify chain. verify_channel_event takes &mut tracker and
        // ADVANCES it on success — no separate record() call needed.
        let verify = {
            let mut tracker = self.replay_tracker.lock().await;
            verify_channel_event(
                &event,
                &self.community_id,
                &self.channel_id,
                self.state_at_hlc.as_ref(),
                self.resolver.as_ref(),
                &mut tracker,
            )
            .await
        };
        if let Err(e) = verify {
            match e {
                ChannelEventError::Replay { .. } => {
                    tracing::debug!(
                        community_id = ?self.community_id,
                        channel_id = ?self.channel_id,
                        err = ?e,
                        "drop replay"
                    );
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

        // 3. Append.
        let appended = {
            let mut log = self.log.lock().await;
            match log.append(event.clone()) {
                Ok(_seal_ready) => true,
                Err(e) => {
                    tracing::error!(
                        community_id = ?self.community_id,
                        channel_id = ?self.channel_id,
                        err = ?e,
                        "channel-log persist failed; degraded"
                    );
                    self.emit_degraded(&format!("persist: {e}"));
                    false
                }
            }
        };
        if !appended {
            return;
        }

        // 4. Emit + notify flush.
        self.emit_message_received(&event);
        self.flush_dirty.notify_one();
    }

    fn spawn_flush_loop(self: &Arc<Self>) -> JoinHandle<()> {
        let me = Arc::clone(self);
        let debounce = Duration::from_millis(self.config.flush_debounce_ms);
        let max_dirty = Duration::from_millis(self.config.max_dirty_ms);

        tokio::spawn(async move {
            let closing = Arc::clone(&me.closing);
            let closing_notify = Arc::clone(&me.closing_notify);
            loop {
                // Wait for first dirty notification (or shutdown wakeup).
                tokio::select! {
                    biased;
                    _ = me.flush_dirty.notified() => {}
                    _ = closing_notify.notified() => {
                        if closing.load(Ordering::SeqCst) { break; }
                        continue;
                    }
                }

                // Sliding-debounce window with hard-cap.
                let first_dirty = std::time::Instant::now();
                let hard_deadline = first_dirty + max_dirty;
                let mut soft_deadline = first_dirty + debounce;

                loop {
                    let target = soft_deadline.min(hard_deadline);
                    let now = std::time::Instant::now();
                    if now >= target {
                        break;
                    }
                    tokio::select! {
                        biased;
                        _ = me.flush_dirty.notified() => {
                            // Reset soft deadline on each dirty pulse.
                            // hard_deadline preserved (don't reset on continuous dirty).
                            soft_deadline = std::time::Instant::now() + debounce;
                        }
                        _ = tokio::time::sleep_until(target.into()) => {
                            break;
                        }
                    }
                }

                // Flush + check seal threshold.
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

                // Seal-on-threshold (best-effort; threshold check uses
                // Phase 2's append return value as ground truth in the
                // hot path, this is the catch-up for events that were
                // appended directly via log_for_test in tests).
                let should_seal = {
                    let log = me.log.lock().await;
                    log.tail.len() >= log.config().seal_threshold_events
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
}

// ── Test-only accessors ────────────────────────────────────────────────────

#[cfg(test)]
impl<R: tauri::Runtime> ChannelLogEngine<R> {
    pub(crate) fn log_for_test(&self) -> &Arc<Mutex<ChannelLog>> {
        &self.log
    }

    pub(crate) fn notify_dirty_for_test(&self) {
        self.flush_dirty.notify_one();
    }

    pub(crate) fn app_handle_for_test(&self) -> &tauri::AppHandle<R> {
        &self.app
    }
}

impl<R: tauri::Runtime> ChannelLogEngine<R> {
    /// Borrow the per-channel encryption key. Used by the registry's
    /// `read_for_query` callback to encrypt backfill replies in the
    /// same wire shape as live broadcast packets (spec §17.1).
    pub(crate) fn channel_key_ref(&self) -> &ChannelKey {
        self.channel_key.as_ref()
    }
}

// ── Registry ───────────────────────────────────────────────────────────────

/// Shared deps for every engine spawned by a single registry. Cloned
/// (Arc-bumped) into the per-engine `ChannelLogEngineParams` at spawn
/// time. Lifetime is the registry — `start_node` constructs it once
/// per identity and stores the `Arc<ChannelLogRegistry>` on
/// `NodeState` until `stop_inner` clears it.
///
/// **Note on Zenoh session.** ZEB-270 Phase 3 Task 4.5 replaced the
/// prior `session: Arc<zenoh::Session>` field with the
/// `adapter_request_tx` mpsc bridge below. The session lives
/// exclusively inside `event_loop::run`'s scope (it's opened during
/// node-runtime bootstrap and dropped on shutdown); the registry's
/// `spawn` enqueues a `ChannelLogAdapterRequest` over the bridge, and
/// the event loop's `select!` arm wires the request to a per-channel
/// adapter against the live session. This decouples the registry from
/// the session lifetime — which was load-bearing because the registry
/// is constructed in `start_node`'s scope, where the session isn't
/// reachable.
pub struct ChannelLogRegistryConfig<R: tauri::Runtime> {
    /// Adapter-request bridge to `event_loop::run`. Each
    /// `ChannelLogRegistry::spawn` call enqueues one
    /// `ChannelLogAdapterRequest`; the event loop drains the matching
    /// receiver and calls `spawn_channel_log_zenoh_adapter` against
    /// the live `Arc<zenoh::Session>`.
    ///
    /// **Unbounded by design.** Boot-time `reconcile_from_state` runs
    /// BEFORE the event-loop thread spawns (the reconcile must
    /// populate the bridge so event_loop can wire each request to the
    /// session as soon as it opens). A bounded channel would deadlock
    /// at boot if a user has more channels than the bound — `.send`
    /// would await forever because no consumer exists yet. Memory
    /// pressure is a non-issue: each request is a few Arcs + a
    /// closure; for a user with 1000 channels, the queued requests
    /// are O(KB).
    pub adapter_request_tx: mpsc::UnboundedSender<crate::event_loop::ChannelLogAdapterRequest>,
    /// Tauri AppHandle — propagated into each engine for
    /// `channel-message-received` / `channel-log-degraded` /
    /// `channel-backfill-progress` event emission.
    pub app: tauri::AppHandle<R>,
    /// Filesystem root under which per-(community, channel) directories
    /// live (`identity_dir/communities/{cid_hex}/channels/{ch_id_hex}/`).
    /// Mirrors `CommunityRegistryConfig.identity_dir` — same convention.
    pub identity_dir: PathBuf,
    /// Local member's owner address. Stamped on every locally-minted
    /// `ChannelPostPayload.author`; bound at registry construction so
    /// every spawned engine shares the same self-identity.
    pub self_owner: OwnerAddr,
    /// Local stable device id. Used as the publisher key in HLC
    /// reservation (`dm_outbox::reserve_next_hlc_for_device`) and as
    /// the device_id field of every minted Hlc.
    pub self_device_id: String,
    /// Local Ed25519 signing key. Same Arc the community sync engine
    /// and the DM outbox already share — sourced from `PrivateIdentity`
    /// at `start_node` time. `Arc` so per-engine spawns are cheap (no
    /// secret-byte copy).
    pub signing_key: Arc<SigningKey>,
    /// Per-engine tunables. Cloned into each engine; tests override
    /// `log_config.seal_threshold_events` to exercise seal/reload
    /// paths in reasonable time.
    pub engine_config: ChannelLogEngineConfig,
}

/// Per-CommunitySyncEngine registry of running per-channel engines.
/// Mirrors `community_state_sync::CommunitySyncRegistry` in shape:
///   - idempotent `spawn` (returns existing Arc on duplicate)
///   - clean `stop` (flushes engine, drops the entry — no in-memory
///     tombstones; on-disk segments persist per spec §17.4)
///   - `reconcile_from_state` is the boot-time / restart pass
///   - `shutdown_all` is the stop-node hook
///
/// Lifetime: one registry per identity, constructed at `start_node`
/// time alongside `CommunitySyncRegistry`, stored as
/// `Option<Arc<...>>` on `NodeState`, cleared during `stop_inner`.
///
/// **Adapter lifetime.** ZEB-270 Phase 3 Task 4.5 moved Zenoh adapter
/// ownership out of the registry: the registry's `spawn` enqueues a
/// `ChannelLogAdapterRequest` over the bridge mpsc, and `event_loop::run`
/// owns the adapter `JoinHandle`. The registry only retains the per-
/// channel `closing` flag — flipping it to `true` causes the adapter's
/// per-task select arms to exit on their next 1s closing-poll. The
/// adapter's `JoinHandle` is dropped by event_loop on shutdown.
///
/// Lock-discipline: each map is held under its own
/// `tokio::sync::Mutex`. The `spawn` flow takes the engines lock,
/// performs the idempotency check, releases, does the engine
/// construction + adapter request build off-lock, then re-takes the
/// engines lock to insert. Concurrent spawns for the same `(cid, chid)`
/// resolve to a single engine via the post-construction re-check (the
/// second caller's engine is immediately stopped + dropped, and the
/// loser's adapter request is never enqueued). The `closings` map is
/// touched only after the engine map insert succeeds.
#[allow(clippy::type_complexity)]
pub struct ChannelLogRegistry<R: tauri::Runtime> {
    engines: Mutex<HashMap<(SpaceId, ChannelId), Arc<ChannelLogEngine<R>>>>,
    closings: Mutex<HashMap<(SpaceId, ChannelId), Arc<AtomicBool>>>,
    config: ChannelLogRegistryConfig<R>,
}

impl<R: tauri::Runtime> ChannelLogRegistry<R> {
    /// Production constructor. Wires the registry to the
    /// adapter-request bridge defined in `ChannelLogRegistryConfig`.
    /// Each `spawn` call enqueues a `ChannelLogAdapterRequest` over
    /// the bridge; `event_loop::run` drains the matching receiver and
    /// spawns the per-channel Zenoh adapter against the live session.
    pub fn new(config: ChannelLogRegistryConfig<R>) -> Arc<Self> {
        Arc::new(Self {
            engines: Mutex::new(HashMap::new()),
            closings: Mutex::new(HashMap::new()),
            config,
        })
    }

    /// Spawn engine + Zenoh adapter for one `(community_id, channel_id)`.
    /// Idempotent — returns the existing Arc if already present (no new
    /// engine spawned, no new adapter, no fs `create_dir_all`).
    ///
    /// `state_at_hlc`, `resolver`, `hlc_tracker` are passed through to
    /// `ChannelLogEngineParams` unchanged. The caller (production:
    /// the delta-consumer hook + `reconcile_from_state`; tests: the
    /// fixture) is responsible for sourcing them from the matching
    /// `CommunitySyncEngine`.
    ///
    /// On error the engine + adapter are not registered (the partial
    /// `engines` insert is the commit point); the caller may retry.
    pub async fn spawn(
        self: &Arc<Self>,
        community_id: SpaceId,
        channel_id: ChannelId,
        channel_key: ChannelKey,
        state_at_hlc: Arc<dyn CommunityStateAtHlc + Send + Sync>,
        resolver: Arc<dyn ChannelIdentityResolver + Send + Sync>,
        hlc_tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
    ) -> Result<Arc<ChannelLogEngine<R>>, ChannelLogEngineError> {
        let key = (community_id, channel_id);

        // Cheap pre-check under the engines lock — returns Arc-cloned
        // existing engine on the duplicate path so we skip dir-creation,
        // engine construction, adapter spawn, and the second insert.
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
            ChannelLogEngineError::Persist(ChannelLogPersistError::Io(e.to_string()))
        })?;

        let (publisher_tx, publisher_rx) = mpsc::channel::<Vec<u8>>(64);
        let (subscriber_tx, subscriber_rx) = mpsc::channel::<Vec<u8>>(64);
        let (query_request_tx, query_request_rx) = mpsc::channel::<BackfillQueryRequest>(8);

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

        // `read_for_query` closure passed to the adapter's queryable
        // task: maps a backfill query (since, limit) to a vec of
        // encrypted packets — wire-identical to live broadcast (spec
        // §17.1). Captures `Arc<engine>`. No cycle: the adapter holds
        // the closure by `Arc`, and the closure holds `Arc<engine>`,
        // but the adapter task itself is owned by the registry's
        // `adapter_handles` map and dropped on `stop`. The engine has
        // no back-reference to the adapter or its closure.
        let engine_for_query = Arc::clone(&engine);
        let read_for_query =
            Arc::new(
                move |since: Option<Hlc>,
                      limit: usize|
                      -> std::pin::Pin<
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
                            .filter_map(|ev| encrypt_channel_packet(me.channel_key_ref(), ev).ok())
                            .collect()
                    })
                },
            );

        let closing = Arc::new(AtomicBool::new(false));

        // Re-check under the engines lock — a concurrent spawn may
        // have inserted for the same key while we were doing the
        // heavy lifting (dir-create + engine construction). Whoever
        // inserts first wins; the loser's engine is shut down + dropped
        // here BEFORE the adapter-request is enqueued, so the adapter
        // bridge never sees the loser's halves. The registry maps never
        // observe the loser's engine, so external callers see the
        // consistent winner.
        {
            let mut engines = self.engines.lock().await;
            if let Some(existing) = engines.get(&key) {
                let existing = Arc::clone(existing);
                drop(engines);
                // Best-effort cleanup of the loser. shutdown errors
                // are logged but not surfaced — the winner's engine is
                // what the caller gets back, and partial-cleanup of
                // the loser is the recoverable case (worst outcome: a
                // leaked tail.cbor write on the loser's path, harmless
                // because the winner writes to the same file).
                if let Err(e) = engine.shutdown().await {
                    tracing::warn!(
                        community_id = ?community_id,
                        channel_id = ?channel_id,
                        error = ?e,
                        "channel-log spawn race: loser shutdown failed",
                    );
                }
                // closing flag wasn't published anywhere yet; just
                // drop it. publisher_rx / subscriber_tx /
                // query_request_rx are dropped at scope end — never
                // wired to a Zenoh adapter, so no observable effect.
                return Ok(existing);
            }
            engines.insert(key, Arc::clone(&engine));
        }
        {
            let mut closings = self.closings.lock().await;
            closings.insert(key, Arc::clone(&closing));
        }

        // Send the adapter request over the bridge. The event loop
        // drains the matching receiver and spawns the per-channel
        // Zenoh adapter against the live session. Send failure means
        // the bridge is closed (event_loop already exited) — log + drop
        // the local-only engine references; the caller still gets back
        // a valid local engine (publish/list_messages still work off
        // the per-channel disk segments), it just can't reach the wire
        // until next start_node. shutdown_all on stop_inner will clean
        // it up.
        //
        // Unbounded send is non-blocking (no .await) — the bridge
        // can't apply back-pressure. See `adapter_request_tx` doc on
        // ChannelLogRegistryConfig for why.
        if let Err(e) =
            self.config
                .adapter_request_tx
                .send(crate::event_loop::ChannelLogAdapterRequest {
                    community_id_hex,
                    channel_id_hex,
                    publisher_rx,
                    subscriber_tx,
                    query_request_rx,
                    read_for_query,
                    closing: Arc::clone(&closing),
                })
        {
            tracing::warn!(
                community_id = ?community_id,
                channel_id = ?channel_id,
                error = %e,
                "channel-log adapter bridge send failed; engine spawned without wire \
                 — local publish/list_messages still work, wire transport unavailable \
                 until next start_node"
            );
        }

        Ok(engine)
    }

    /// Stop engine and discard the entry. Idempotent — second call
    /// (and stop-of-unknown) returns `Ok(())`. Per spec §17.4 the
    /// in-memory entry is dropped; on-disk segments persist so a
    /// subsequent re-spawn (e.g., admin re-creates the channel) reads
    /// historical messages back.
    ///
    /// `engine.shutdown()` flushes the tail synchronously before the
    /// background loops are released — by the time this returns, the
    /// in-memory tail is durably written. The adapter task continues
    /// until its closing-flag poll fires (≤1s); we drop the
    /// `JoinHandle` immediately rather than awaiting it so `stop` is
    /// fast and predictable. The closing flag stays alive in the
    /// dropped `Arc` long enough for the adapter to observe it.
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
            // Stop-of-unknown is a no-op (mirrors
            // CommunitySyncRegistry::stop_engine semantics).
            return Ok(());
        };

        engine.shutdown().await?;

        if let Some(closing) = self.closings.lock().await.remove(&key) {
            closing.store(true, Ordering::SeqCst);
        }
        // ZEB-270 Phase 3 Task 4.5: the adapter `JoinHandle` is owned
        // by `event_loop::run` (the bridge architecture moved it out
        // of the registry). Flipping `closing` above causes the
        // adapter's per-task select arms to exit on their next 1s
        // closing-poll; the JoinHandle is dropped by event_loop on
        // shutdown.

        Ok(())
    }

    /// Snapshot of the engine for `(community_id, channel_id)`. Used
    /// by IPC handlers (Task 5) that need to call `engine.publish` /
    /// `engine.list_messages` / `engine.request_backfill`. Returns
    /// `None` if no engine is currently registered.
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

    /// Drain every spawned engine. Surfaces the LAST error encountered
    /// after attempting to stop all (mirrors
    /// `CommunitySyncRegistry::shutdown_all` — bailing on first error
    /// would leak engines after it). Called from `stop_inner`.
    pub async fn shutdown_all(&self) -> Result<(), ChannelLogEngineError> {
        let keys: Vec<(SpaceId, ChannelId)> = {
            let engines = self.engines.lock().await;
            engines.keys().copied().collect()
        };
        let mut last_err: Option<ChannelLogEngineError> = None;
        for (cid, chid) in keys {
            if let Err(e) = self.stop(&cid, &chid).await {
                tracing::warn!(
                    community_id = ?cid,
                    channel_id = ?chid,
                    error = ?e,
                    "channel-log engine stop failed during shutdown_all",
                );
                last_err = Some(e);
            }
        }
        match last_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Walk a community's materialized channels and `spawn` an engine
    /// for each non-tombstoned entry. Idempotent — re-running on the
    /// same state is a no-op (each `spawn` returns the existing Arc).
    ///
    /// Called from `start_node` after each `CommunitySyncRegistry::spawn_engine`
    /// returns, plus on subsequent reload paths. Mirrors spec §7.4 — the
    /// registry state must always reflect the materialized channels map
    /// (this is the boot-time source of truth; the delta-consumer
    /// callback handles incremental Created/Deleted between boots).
    ///
    /// Takes `&MaterializedMembership` rather than `&CommunityState` —
    /// the spec/plan called this `CommunityState`, but `CommunityState`
    /// stores the raw event log (`events: BTreeMap<EventId, ...>`),
    /// not the materialized `channels` map. Callers materialize first
    /// via `CommunityState::materialized(admin_addr)` and pass the
    /// resulting view in. Documented as a Task 4 deviation in the
    /// commit message.
    pub async fn reconcile_from_state(
        self: &Arc<Self>,
        community_id: SpaceId,
        materialized: &MaterializedMembership,
        membership_key: &MembershipKey,
        state_at_hlc: Arc<dyn CommunityStateAtHlc + Send + Sync>,
        resolver: Arc<dyn ChannelIdentityResolver + Send + Sync>,
        hlc_tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
    ) -> Result<(), ChannelLogEngineError> {
        for (channel_id, info) in &materialized.channels {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_channel_log::derive_channel_key;
    use crate::community_membership::ChannelInfo;
    use crate::owner_state_types::MembershipKey;
    use ed25519_dalek::SigningKey;
    use harmony_identity::PrivateIdentity;
    use std::collections::HashMap;
    use tempfile::TempDir;

    /// State stub: returns a known channel with low write-power and the
    /// fixture identity Joined at the engine's owner. Sufficient for
    /// receive-loop tests; verify-chain edge cases live in Phase 2.
    struct AlwaysJoinedState {
        channel_id: ChannelId,
        owner: OwnerAddr,
    }

    #[async_trait::async_trait]
    impl CommunityStateAtHlc for AlwaysJoinedState {
        async fn channel_at(&self, channel_id: &ChannelId, _at: &Hlc) -> Option<ChannelInfo> {
            if channel_id != &self.channel_id {
                return None;
            }
            Some(ChannelInfo {
                name: "test".to_string(),
                write_power: 0,
                created_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "fixture".to_string(),
                },
                deleted_at: None,
            })
        }

        async fn author_power_at(&self, author: &OwnerAddr, _at: &Hlc) -> Option<u8> {
            if author == &self.owner {
                Some(100)
            } else {
                None
            }
        }
    }

    /// Resolver stub: maps OwnerAddr → 64-byte identity composite.
    struct FixedIdentityResolver {
        map: HashMap<OwnerAddr, [u8; 64]>,
    }

    #[async_trait::async_trait]
    impl ChannelIdentityResolver for FixedIdentityResolver {
        async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
            self.map.get(addr).copied()
        }
    }

    /// Build a deterministic (signing_key, owner_addr, identity_pub_64)
    /// triple. Mirrors Phase 2's `fixture_identity` helper at
    /// `community_channel_log.rs:1253`.
    fn fixture_identity(seed: u8) -> (SigningKey, OwnerAddr, [u8; 64]) {
        let priv_id = PrivateIdentity::from_seed(&[seed; 32]);
        let owner = OwnerAddr(priv_id.identity.address_hash);
        let pub_64 = priv_id.identity.to_public_bytes();
        let private_bytes = priv_id.to_private_bytes();
        let mut ed_secret = [0u8; 32];
        ed_secret.copy_from_slice(&private_bytes[32..64]);
        let signing = SigningKey::from_bytes(&ed_secret);
        (signing, owner, pub_64)
    }

    struct EngineFixture {
        engine: Arc<ChannelLogEngine<tauri::test::MockRuntime>>,
        publisher_rx: mpsc::Receiver<Vec<u8>>,
        subscriber_tx: mpsc::Sender<Vec<u8>>,
        query_request_rx: mpsc::Receiver<BackfillQueryRequest>,
        self_owner: OwnerAddr,
        signing_key: Arc<SigningKey>,
        channel_key: Arc<ChannelKey>,
        community_id: SpaceId,
        channel_id: ChannelId,
        tmp: TempDir,
    }

    async fn build_engine_fixture(
        seal_threshold: usize,
        flush_debounce_ms: u64,
        max_dirty_ms: u64,
    ) -> EngineFixture {
        let tmp = TempDir::new().expect("tempdir");

        let (signing_key_raw, self_owner, identity_pub_64) = fixture_identity(0x42);
        let signing_key = Arc::new(signing_key_raw);

        let community_id = SpaceId([0xc1; 16]);
        let channel_id = ChannelId([0x77; 16]);
        let membership_key = MembershipKey::new([0x55; 32]);

        let channel_key = Arc::new(derive_channel_key(
            &membership_key,
            &community_id,
            &channel_id,
        ));

        let mut resolver_map = HashMap::new();
        resolver_map.insert(self_owner, identity_pub_64);
        let resolver = Arc::new(FixedIdentityResolver { map: resolver_map });

        let state = Arc::new(AlwaysJoinedState {
            channel_id,
            owner: self_owner,
        });

        let hlc_tracker: Arc<Mutex<BTreeMap<String, Hlc>>> = Arc::new(Mutex::new(BTreeMap::new()));

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
            tmp,
        }
    }

    /// Build a signed Post event with the fixture identity (so the
    /// event's signature verifies and address_hash binds correctly).
    fn make_signed_event(
        community_id: SpaceId,
        channel_id: ChannelId,
        author: OwnerAddr,
        at: Hlc,
        body: &str,
        signing_key: &SigningKey,
    ) -> SignedChannelEvent {
        use rand::RngCore;
        let mut id_bytes = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut id_bytes);
        let payload = ChannelPostPayload {
            id: MessageId(id_bytes),
            community_id,
            channel_id,
            author,
            at,
            content_kind: 0,
            body,
            reply_to: None,
        };
        sign_channel_event(&payload, signing_key).expect("sign")
    }

    /// Poll until `predicate` returns Some, or timeout.
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

    fn extract_id(ev: &SignedChannelEvent) -> MessageId {
        let SignedChannelEvent::Post { id, .. } = ev;
        *id
    }

    // ── Sub-task 2A: list_messages ────────────────────────────────────

    #[tokio::test]
    async fn list_messages_empty_log_returns_empty() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        let msgs = fix.engine.list_messages(None, 100).await.expect("list");
        assert!(msgs.is_empty());
    }

    #[tokio::test]
    async fn list_messages_returns_hlc_ordered() {
        let fix = build_engine_fixture(8, 250, 1000).await;

        let mut events = Vec::new();
        for (i, body) in ["first", "second", "third"].iter().enumerate() {
            let hlc = Hlc {
                wall_ms: 1_000 + i as u64,
                logical: 0,
                device_id: "test-device".to_string(),
            };
            let ev = make_signed_event(
                fix.community_id,
                fix.channel_id,
                fix.self_owner,
                hlc,
                body,
                &fix.signing_key,
            );
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
            assert_eq!(extract_id(got), extract_id(want));
        }
    }

    #[tokio::test]
    async fn list_messages_walks_tail_then_segments() {
        // Spec §14.1: with seal_threshold=4 and 10 events appended,
        // the engine ends up with 2 sealed segments + 2 events in the
        // tail. list_messages must walk segments first then tail and
        // return all 10 in HLC order. Closes a coverage gap — the
        // existing list_messages tests never populate manifest.segments,
        // so the segment-walk branch (lines ~358-385) was structurally
        // executed against an empty list only.
        let fix = build_engine_fixture(4, 250, 1000).await;

        let mut events = Vec::new();
        for i in 0..10u64 {
            let hlc = Hlc {
                wall_ms: 100 + i,
                logical: 0,
                device_id: "test-device".to_string(),
            };
            let ev = make_signed_event(
                fix.community_id,
                fix.channel_id,
                fix.self_owner,
                hlc,
                &format!("msg-{i}"),
                &fix.signing_key,
            );
            events.push(ev);
        }

        // Append + seal directly under the log mutex. Manual seal-
        // every-4 (matching seal_threshold) gives a deterministic
        // 2-segment + 2-tail layout without racing the flush loop.
        {
            let mut log = fix.engine.log_for_test().lock().await;
            for (i, ev) in events.iter().enumerate() {
                log.append(ev.clone()).expect("append");
                if (i + 1) % 4 == 0 {
                    log.seal_and_persist().expect("seal");
                }
            }
            assert_eq!(
                log.manifest.segments.len(),
                2,
                "expected 2 sealed segments after 8 of 10 appends",
            );
            assert_eq!(log.tail.len(), 2, "expected 2 events left in tail");
        }

        let listed = fix.engine.list_messages(None, 100).await.expect("list");
        assert_eq!(listed.len(), 10, "all 10 events must be returned");

        // HLC-ascending order across the segment+tail boundary.
        for (i, ev) in listed.iter().enumerate() {
            let SignedChannelEvent::Post { at, .. } = ev;
            assert_eq!(
                at.wall_ms,
                100 + i as u64,
                "event {i} out of HLC order (got wall_ms={})",
                at.wall_ms,
            );
        }

        // First/last bookend checks per spec §14.1.
        let SignedChannelEvent::Post { at: first_at, .. } = &listed[0];
        let SignedChannelEvent::Post { at: last_at, .. } = &listed[9];
        assert_eq!(first_at.wall_ms, 100);
        assert_eq!(last_at.wall_ms, 109);
        assert_eq!(extract_id(&listed[0]), extract_id(&events[0]));
        assert_eq!(extract_id(&listed[9]), extract_id(&events[9]));
    }

    #[tokio::test]
    async fn list_messages_respects_limit() {
        let fix = build_engine_fixture(8, 250, 1000).await;

        for i in 0..5 {
            let hlc = Hlc {
                wall_ms: 2_000 + i,
                logical: 0,
                device_id: "test-device".to_string(),
            };
            let ev = make_signed_event(
                fix.community_id,
                fix.channel_id,
                fix.self_owner,
                hlc,
                "x",
                &fix.signing_key,
            );
            let mut log = fix.engine.log_for_test().lock().await;
            log.append(ev).expect("append");
        }

        let listed = fix.engine.list_messages(None, 2).await.expect("list");
        assert_eq!(listed.len(), 2, "limit must cap result count");
    }

    // ── Sub-task 2B: publish ──────────────────────────────────────────

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
        assert_eq!(extract_id(&listed[0]), msg_id);

        // The packet decrypts back to the same event.
        let decrypted = decrypt_channel_packet(&fix.channel_key, &packet).expect("decrypt");
        assert_eq!(extract_id(&decrypted), msg_id);
    }

    #[tokio::test]
    async fn publish_emits_channel_message_received_event() {
        // Spec §14.1 requires "log mutation + Tauri event" for the
        // publish + receive paths. The other publish test only checks
        // log mutation; this one closes the gap by installing a real
        // Tauri listener on the mock runtime and asserting the
        // channel-message-received payload shape.
        use std::sync::Mutex as StdMutex;
        use tauri::Listener;

        let fix = build_engine_fixture(8, 250, 1000).await;

        let captured: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let captured_for_listener = Arc::clone(&captured);
        fix.engine
            .app_handle_for_test()
            .listen("channel-message-received", move |event| {
                captured_for_listener
                    .lock()
                    .expect("captured lock")
                    .push(event.payload().to_string());
            });

        let body = b"emit-test-body".to_vec();
        let msg_id = Arc::clone(&fix.engine)
            .publish(body.clone(), None)
            .await
            .expect("publish");

        let captured_payload = wait_for(
            || {
                let captured = Arc::clone(&captured);
                async move {
                    let v = captured.lock().expect("captured lock");
                    v.first().cloned()
                }
            },
            Duration::from_secs(1),
        )
        .await
        .expect("listener must receive channel-message-received within 1s");

        let payload: serde_json::Value =
            serde_json::from_str(&captured_payload).expect("payload is JSON");

        assert_eq!(
            payload["communityId"].as_str(),
            Some(hex::encode(fix.community_id.0).as_str()),
            "communityId in payload",
        );
        assert_eq!(
            payload["channelId"].as_str(),
            Some(hex::encode(fix.channel_id.0).as_str()),
            "channelId in payload",
        );
        assert_eq!(
            payload["message"]["messageId"].as_str(),
            Some(hex::encode(msg_id.0).as_str()),
            "message.messageId matches publish() return",
        );

        // Body is serialized by serde as a JSON array of byte values.
        let body_arr = payload["message"]["body"]
            .as_array()
            .expect("message.body must be a JSON array");
        let body_bytes: Vec<u8> = body_arr
            .iter()
            .map(|v| v.as_u64().expect("byte fits u64") as u8)
            .collect();
        assert_eq!(body_bytes, body, "message.body matches input bytes");
    }

    #[tokio::test]
    async fn publish_rejects_oversized_body() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        let body = vec![0u8; MAX_BODY_BYTES + 1];
        let err = Arc::clone(&fix.engine)
            .publish(body, None)
            .await
            .expect_err("oversized body must reject");
        assert!(matches!(err, ChannelLogEngineError::BodyTooLarge { .. }));
    }

    // ── Sub-task 2C: receive loop ─────────────────────────────────────

    #[tokio::test]
    async fn receive_well_formed_packet_appends_and_emits() {
        let fix = build_engine_fixture(8, 250, 1000).await;

        let hlc = Hlc {
            wall_ms: 5_000,
            logical: 0,
            device_id: "remote-device".to_string(),
        };
        let event = make_signed_event(
            fix.community_id,
            fix.channel_id,
            fix.self_owner,
            hlc,
            "from-remote",
            &fix.signing_key,
        );
        let packet = encrypt_channel_packet(&fix.channel_key, &event).expect("encrypt");

        fix.subscriber_tx.send(packet).await.expect("send");

        let listed = wait_for(
            || async {
                let v = fix.engine.list_messages(None, 100).await.unwrap();
                if v.len() == 1 {
                    Some(v)
                } else {
                    None
                }
            },
            Duration::from_secs(2),
        )
        .await
        .expect("event appeared");

        assert_eq!(extract_id(&listed[0]), extract_id(&event));
    }

    #[tokio::test]
    async fn receive_garbage_packet_drops_silently() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        fix.subscriber_tx
            .send(b"not a real packet".to_vec())
            .await
            .expect("send");

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
        let event = make_signed_event(
            fix.community_id,
            fix.channel_id,
            fix.self_owner,
            hlc,
            "once",
            &fix.signing_key,
        );
        let packet = encrypt_channel_packet(&fix.channel_key, &event).expect("encrypt");

        fix.subscriber_tx
            .send(packet.clone())
            .await
            .expect("send 1");
        fix.subscriber_tx.send(packet).await.expect("send 2");

        // Wait for first to land.
        let _ = wait_for(
            || async {
                let v = fix.engine.list_messages(None, 100).await.unwrap();
                if v.len() == 1 {
                    Some(v)
                } else {
                    None
                }
            },
            Duration::from_secs(2),
        )
        .await
        .expect("first event must land");

        // Give second packet (replay) time to be processed and rejected.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let listed = fix.engine.list_messages(None, 100).await.expect("list");
        assert_eq!(listed.len(), 1, "replay must be dropped");
    }

    // ── Sub-task 2D: flush loop ───────────────────────────────────────

    #[tokio::test]
    async fn flush_debounce_coalesces_burst() {
        // 50 ms debounce + 500 ms cap so the test runs quickly. Large
        // seal threshold ensures we don't seal away the tail.
        let fix = build_engine_fixture(1024, 50, 500).await;

        for i in 0..5 {
            let hlc = Hlc {
                wall_ms: 7_000 + i,
                logical: 0,
                device_id: "burst".to_string(),
            };
            let ev = make_signed_event(
                fix.community_id,
                fix.channel_id,
                fix.self_owner,
                hlc,
                &format!("burst-{i}"),
                &fix.signing_key,
            );
            {
                let mut log = fix.engine.log_for_test().lock().await;
                log.append(ev).expect("append");
            }
            fix.engine.notify_dirty_for_test();
        }

        // Wait past the debounce window for the flush to occur.
        let tail_path = fix.tmp.path().join("tail.cbor");
        let appeared = wait_for(
            || {
                let p = tail_path.clone();
                async move {
                    if p.exists() {
                        Some(())
                    } else {
                        None
                    }
                }
            },
            Duration::from_millis(500),
        )
        .await;
        assert!(
            appeared.is_some(),
            "tail.cbor should be written after debounce"
        );

        let bytes = std::fs::read(&tail_path).expect("read tail");
        assert!(bytes.len() > 1, "tail.cbor non-empty");
    }

    #[tokio::test]
    async fn flush_max_dirty_forces_under_continuous_load() {
        let fix = build_engine_fixture(1024, 100, 250).await;

        let start = std::time::Instant::now();
        let mut i = 0u64;
        while start.elapsed() < Duration::from_millis(600) {
            let hlc = Hlc {
                wall_ms: 8_000 + i,
                logical: 0,
                device_id: "continuous".to_string(),
            };
            let ev = make_signed_event(
                fix.community_id,
                fix.channel_id,
                fix.self_owner,
                hlc,
                "x",
                &fix.signing_key,
            );
            {
                let mut log = fix.engine.log_for_test().lock().await;
                log.append(ev).expect("append");
            }
            fix.engine.notify_dirty_for_test();
            tokio::time::sleep(Duration::from_millis(50)).await;
            i += 1;
        }

        // Wait an extra tick for the most recent flush to finish.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let tail_path = fix.tmp.path().join("tail.cbor");
        assert!(
            tail_path.exists(),
            "tail.cbor must be written under continuous load via max_dirty cap"
        );
    }

    #[tokio::test]
    async fn flush_now_writes_synchronously() {
        // Long debounce so flush_now beats the loop.
        let fix = build_engine_fixture(1024, 5_000, 10_000).await;

        let hlc = Hlc {
            wall_ms: 9_000,
            logical: 0,
            device_id: "sync".to_string(),
        };
        let ev = make_signed_event(
            fix.community_id,
            fix.channel_id,
            fix.self_owner,
            hlc,
            "sync-flushed",
            &fix.signing_key,
        );
        {
            let mut log = fix.engine.log_for_test().lock().await;
            log.append(ev).expect("append");
        }

        fix.engine.flush_now().await.expect("flush_now");

        let tail_path = fix.tmp.path().join("tail.cbor");
        assert!(tail_path.exists());
        assert!(std::fs::metadata(&tail_path).expect("meta").len() > 1);
    }

    // Smoke: skeleton tests still work end-to-end.
    #[tokio::test]
    async fn engine_construct_shutdown_round_trip() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        fix.engine.shutdown().await.expect("shutdown");
    }

    /// Shutdown must complete promptly via the closing_notify wakeup
    /// path rather than the previous 1s closing-flag-poll. Tightens
    /// the API contract that Ok(()) means the background loops have
    /// actually exited (or are about to within a few ms) and removes
    /// a known source of TempDir-cleanup test flakiness.
    ///
    /// 200ms gives generous slack vs the previous 0–1000ms range.
    /// The flush loop's inner sliding-debounce can still hold up
    /// shutdown if it's mid-debounce, but with no dirty events here
    /// that path never enters.
    #[tokio::test]
    async fn shutdown_completes_promptly() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        let start = std::time::Instant::now();
        fix.engine.shutdown().await.expect("shutdown");
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "shutdown took {elapsed:?}, expected < 200ms"
        );
    }

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

    // ── Sub-task 4A: registry ─────────────────────────────────────────

    /// Per-registry-test fixture. Holds the registry plus the
    /// dependencies callers need to thread into `spawn` /
    /// `reconcile_from_state`. The TempDir keeps the per-channel root
    /// dir alive across the test's awaits. The `_adapter_drainer`
    /// JoinHandle keeps the test-side adapter loop alive — drained
    /// adapter requests have their channels reconnected to a real
    /// Zenoh session so the post-spawn handshake completes; the
    /// drainer exits cleanly when `adapter_request_tx` drops on
    /// fixture teardown.
    struct RegistryFixture {
        registry: Arc<ChannelLogRegistry<tauri::test::MockRuntime>>,
        state: Arc<AlwaysJoinedState>,
        resolver: Arc<FixedIdentityResolver>,
        hlc_tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
        membership_key: MembershipKey,
        self_owner: OwnerAddr,
        // Held to keep the temp dir alive for the duration of the test.
        _tmp: TempDir,
        // Held to keep the adapter-bridge drainer alive. Drops at
        // fixture teardown; the drainer's recv() then returns None and
        // the task exits.
        _adapter_drainer: tokio::task::JoinHandle<()>,
    }

    /// Build a registry against an in-memory Zenoh session and stub
    /// state/resolver/tracker. Mirrors `build_engine_fixture` shape so
    /// the registry tests have the same per-channel deps the engine
    /// tests already rely on.
    ///
    /// Test-side adapter bridge: the production wiring (Phase 3 Task
    /// 4.5) routes adapter requests through an mpsc to `event_loop::run`
    /// which owns the live Zenoh session. The test fixture instead
    /// runs a small drainer task that reads each request and binds it
    /// to a real in-process Zenoh session via
    /// `spawn_channel_log_zenoh_adapter` — same call shape as the
    /// production event_loop arm. The four registry tests don't drive
    /// messages through the wire path (the assertion targets are the
    /// registry's engine map, not the wire), so the drainer is purely
    /// to satisfy the bridge's consumer side.
    ///
    /// Marked `#[allow(...)]` for the rare-flavor flag — Zenoh's
    /// runtime requires `multi_thread`. The default `current_thread`
    /// flavor panics on `zenoh::open`.
    async fn build_registry_fixture() -> RegistryFixture {
        let tmp = TempDir::new().expect("tempdir");

        let (signing_key_raw, self_owner, identity_pub_64) = fixture_identity(0x42);
        let signing_key = Arc::new(signing_key_raw);

        // The fixture's AlwaysJoinedState answers for one specific
        // channel_id; all registry tests use the same id so the stub
        // works for every spawned engine. We use a sentinel id here;
        // tests that need different ids would need a wider stub.
        let stub_channel_id = ChannelId([0xff; 16]);
        let mut resolver_map = HashMap::new();
        resolver_map.insert(self_owner, identity_pub_64);
        let resolver = Arc::new(FixedIdentityResolver { map: resolver_map });

        let state = Arc::new(AlwaysJoinedState {
            channel_id: stub_channel_id,
            owner: self_owner,
        });

        let hlc_tracker: Arc<Mutex<BTreeMap<String, Hlc>>> = Arc::new(Mutex::new(BTreeMap::new()));

        let app = tauri::test::mock_app().handle().clone();

        // In-memory Zenoh session for the test-side adapter drainer.
        let cfg = zenoh::Config::default();
        let session = Arc::new(zenoh::open(cfg).await.expect("zenoh open"));

        // Adapter-request bridge for the test fixture. Unbounded to
        // mirror the production shape (see
        // `ChannelLogRegistryConfig.adapter_request_tx` doc for why
        // production is unbounded).
        let (adapter_request_tx, mut adapter_request_rx) =
            mpsc::unbounded_channel::<crate::event_loop::ChannelLogAdapterRequest>();

        // Drainer task: stand-in for `event_loop::run`'s select arm
        // (production code path lives in event_loop.rs; the test just
        // needs a consumer that satisfies the bridge handshake). Each
        // received request gets bound to the in-memory session via
        // the same `spawn_channel_log_zenoh_adapter` the production
        // path uses. The fixture's `_adapter_drainer` field keeps the
        // task alive; on fixture drop, `adapter_request_tx` drops, the
        // recv() returns None, and the task exits cleanly.
        let drainer_session = Arc::clone(&session);
        let _adapter_drainer = tokio::spawn(async move {
            while let Some(req) = adapter_request_rx.recv().await {
                let _handle = crate::event_loop::spawn_channel_log_zenoh_adapter(
                    Arc::clone(&drainer_session),
                    req.community_id_hex,
                    req.channel_id_hex,
                    req.publisher_rx,
                    req.subscriber_tx,
                    req.query_request_rx,
                    req.read_for_query,
                    req.closing,
                );
                // JoinHandle dropped — adapter task is fire-and-forget;
                // closing flag (held by registry) signals shutdown.
            }
        });

        let config = ChannelLogRegistryConfig {
            adapter_request_tx,
            app,
            identity_dir: tmp.path().to_path_buf(),
            self_owner,
            self_device_id: "registry-test-device".to_string(),
            signing_key,
            engine_config: ChannelLogEngineConfig {
                log_config: ChannelLogConfig {
                    seal_threshold_events: 8,
                },
                ..Default::default()
            },
        };
        let registry = ChannelLogRegistry::new(config);

        RegistryFixture {
            registry,
            state,
            resolver,
            hlc_tracker,
            membership_key: MembershipKey::new([0x55; 32]),
            self_owner,
            _tmp: tmp,
            _adapter_drainer,
        }
    }

    /// Helper — spawn a single channel under the fixture's deps. Returns
    /// the engine Arc so callers can chain assertions.
    async fn spawn_under_fixture(
        fix: &RegistryFixture,
        community_id: SpaceId,
        channel_id: ChannelId,
    ) -> Arc<ChannelLogEngine<tauri::test::MockRuntime>> {
        let key = derive_channel_key(&fix.membership_key, &community_id, &channel_id);
        Arc::clone(&fix.registry)
            .spawn(
                community_id,
                channel_id,
                key,
                Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                Arc::clone(&fix.resolver) as Arc<dyn ChannelIdentityResolver + Send + Sync>,
                Arc::clone(&fix.hlc_tracker),
            )
            .await
            .expect("spawn")
    }

    /// Suppress unused-field lint for `self_owner` — the struct is shared
    /// across tests and not every test reads every field.
    #[allow(dead_code)]
    fn _registry_fixture_field_use(fix: &RegistryFixture) -> OwnerAddr {
        fix.self_owner
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn registry_spawn_idempotent() {
        let fix = build_registry_fixture().await;
        let community_id = SpaceId([0xc1; 16]);
        let channel_id = ChannelId([0xff; 16]);

        let e1 = spawn_under_fixture(&fix, community_id, channel_id).await;
        let e2 = spawn_under_fixture(&fix, community_id, channel_id).await;

        assert!(
            Arc::ptr_eq(&e1, &e2),
            "spawn must return the existing engine on duplicate (cid, chid)",
        );

        fix.registry
            .stop(&community_id, &channel_id)
            .await
            .expect("stop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn registry_stop_discards_entry() {
        let fix = build_registry_fixture().await;
        let cid = SpaceId([0xc1; 16]);
        let chid = ChannelId([0xff; 16]);

        let _ = spawn_under_fixture(&fix, cid, chid).await;
        assert!(
            fix.registry.engine(&cid, &chid).await.is_some(),
            "engine must be present after spawn",
        );

        fix.registry.stop(&cid, &chid).await.expect("stop");

        assert!(
            fix.registry.engine(&cid, &chid).await.is_none(),
            "engine must be discarded after stop (spec §17.4)",
        );

        // Stop-of-unknown is a no-op.
        fix.registry
            .stop(&cid, &chid)
            .await
            .expect("idempotent stop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn registry_reconcile_skips_deleted_channels() {
        let fix = build_registry_fixture().await;
        let cid = SpaceId([0xc1; 16]);
        let live_chid = ChannelId([0xff; 16]);
        let dead_chid = ChannelId([0x02; 16]);

        // Build a MaterializedMembership with one live + one tombstoned
        // channel. (`MaterializedMembership` derives `Default`; the
        // members + power_levels maps are empty — irrelevant for this
        // assertion which only inspects `channels`.)
        let mut materialized = MaterializedMembership::default();
        materialized.channels.insert(
            live_chid,
            crate::community_membership::ChannelInfo {
                name: "live".to_string(),
                write_power: 0,
                created_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "x".to_string(),
                },
                deleted_at: None,
            },
        );
        materialized.channels.insert(
            dead_chid,
            crate::community_membership::ChannelInfo {
                name: "dead".to_string(),
                write_power: 0,
                created_at: Hlc {
                    wall_ms: 2,
                    logical: 0,
                    device_id: "x".to_string(),
                },
                deleted_at: Some(Hlc {
                    wall_ms: 3,
                    logical: 0,
                    device_id: "x".to_string(),
                }),
            },
        );

        Arc::clone(&fix.registry)
            .reconcile_from_state(
                cid,
                &materialized,
                &fix.membership_key,
                Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                Arc::clone(&fix.resolver) as Arc<dyn ChannelIdentityResolver + Send + Sync>,
                Arc::clone(&fix.hlc_tracker),
            )
            .await
            .expect("reconcile");

        assert!(
            fix.registry.engine(&cid, &live_chid).await.is_some(),
            "live channel must spawn",
        );
        assert!(
            fix.registry.engine(&cid, &dead_chid).await.is_none(),
            "tombstoned channel must NOT spawn",
        );

        fix.registry
            .stop(&cid, &live_chid)
            .await
            .expect("cleanup stop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn registry_reconcile_idempotent() {
        let fix = build_registry_fixture().await;
        let cid = SpaceId([0xc1; 16]);
        let chid = ChannelId([0xff; 16]);

        let mut materialized = MaterializedMembership::default();
        materialized.channels.insert(
            chid,
            crate::community_membership::ChannelInfo {
                name: "live".to_string(),
                write_power: 0,
                created_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "x".to_string(),
                },
                deleted_at: None,
            },
        );

        Arc::clone(&fix.registry)
            .reconcile_from_state(
                cid,
                &materialized,
                &fix.membership_key,
                Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                Arc::clone(&fix.resolver) as Arc<dyn ChannelIdentityResolver + Send + Sync>,
                Arc::clone(&fix.hlc_tracker),
            )
            .await
            .expect("reconcile 1");
        let e1 = fix.registry.engine(&cid, &chid).await.expect("engine 1");

        Arc::clone(&fix.registry)
            .reconcile_from_state(
                cid,
                &materialized,
                &fix.membership_key,
                Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                Arc::clone(&fix.resolver) as Arc<dyn ChannelIdentityResolver + Send + Sync>,
                Arc::clone(&fix.hlc_tracker),
            )
            .await
            .expect("reconcile 2");
        let e2 = fix.registry.engine(&cid, &chid).await.expect("engine 2");

        assert!(
            Arc::ptr_eq(&e1, &e2),
            "second reconcile must return the same engine Arc (idempotent)",
        );

        fix.registry.stop(&cid, &chid).await.expect("cleanup stop");
    }
}
