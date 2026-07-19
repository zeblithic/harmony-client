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
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::JoinHandle;

use crate::community_channel_log::{
    decrypt_channel_packet, derive_channel_key, encrypt_channel_packet, open_watermark_vector,
    read_segment_at, seal_watermark_vector, sign_channel_event, verify_channel_event,
    ChannelAttachment, ChannelEventError, ChannelKey, ChannelLog, ChannelLogConfig,
    ChannelLogPersistError, ChannelLogReplayTracker, ChannelPostPayload, CommunityStateAtHlc,
    MessageId, SegmentDescriptor, SignedChannelEvent, WatermarkVector, MAX_ATTACHMENTS,
    MAX_ATTACHMENT_FIELD_BYTES, MAX_MENTIONS, MAX_WATERMARK_VECTOR_BYTES,
    MAX_WATERMARK_VECTOR_ENTRIES,
};
use crate::community_membership::{ChannelId, MaterializedMembership};
use crate::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};

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

    #[error("too many mentions: {count} (max {max})")]
    TooManyMentions { count: usize, max: usize },

    #[error("too many attachments: {count} (max {max})")]
    TooManyAttachments { count: usize, max: usize },

    #[error("attachment name/mime too long (max {max})")]
    AttachmentFieldTooLong { max: usize },

    #[error("limit too large: {limit} (max {max})")]
    LimitTooLarge { limit: u32, max: u32 },

    #[error("body not valid UTF-8: {0}")]
    BodyNotUtf8(String),

    #[error("invariant violation: {0}")]
    InvariantViolation(String),

    /// ZEB-288 (CodeAnt): a local `publish` raced engine `shutdown()` and
    /// found `closing` set under the `log` lock. Appending would strand an
    /// unflushed event past shutdown's flush; unlike an inbound packet a
    /// local publish has no backfill recovery, so the caller is told the
    /// post did not land rather than silently losing it.
    #[error("channel engine is shutting down; publish not persisted")]
    EngineShuttingDown,

    /// ZEB-536: emoji string exceeded the per-reaction byte cap.
    #[error("reaction emoji too large: {len} bytes (max {max})")]
    ReactionEmojiTooLarge { len: usize, max: usize },
}

// ── Transaction primitives (ZEB-271) ─────────────────────────────────────────

/// Outcome of `ChannelLogRegistry::spawn`. Per ZEB-271 spec §3.3, a
/// spawn during an open community transaction is deferred until commit;
/// a spawn outside a transaction follows the existing fast-path and
/// returns the live engine.
pub enum SpawnOutcome {
    /// The engine was constructed and inserted into the registry.
    Spawned(Arc<ChannelLogEngine>),
    /// A community transaction is open for this `community_id`; the
    /// spawn was queued and will fire on `commit()`.
    DeferredForCommit,
}

/// One queued spawn within a `PendingTransaction`. Captures every
/// argument the registry's spawn body needs so commit can replay it.
struct DeferredSpawn {
    channel_id: ChannelId,
    channel_key: ChannelKey,
    state_at_hlc: Arc<dyn CommunityStateAtHlc + Send + Sync>,
    hlc_tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
}

/// One open community transaction. `tx_id` tags every guard so a stale
/// guard's deferred abort cannot clobber a fresh transaction's queue
/// (spec §5.4).
struct PendingTransaction {
    tx_id: u64,
    queue: Vec<DeferredSpawn>,
}

// ── DTOs (used here for Tauri event emission; surfaced to IPC layer in Task 5) ──

/// Hybrid Logical Clock — wire/IPC shape.
///
/// Mirrors `owner_state_types::Hlc` but uses serde camelCase for the
/// Tauri/IPC surface. Phase 2's `Hlc` keeps single-letter wire keys
/// (`w` / `l` / `d`) for canonical-CBOR field-length parity, which is
/// the wrong shape for a TypeScript consumer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
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
    /// ZEB-291 Phase 1.5 chat dispatch — `Some("poll")` iff the body
    /// matches the poll-message convention (`0x00` magic + 64 ASCII hex
    /// chars). `None` (= text) for all other bodies. Serialized as `kind`
    /// over IPC; omitted when None so existing text-only consumers
    /// don't see a `kind: null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
    /// ZEB-291 Phase 1.5 — 64-char hex `PollId`, present iff `kind ==
    /// Some("poll")`. Extracted from `body[1..65]` (the ASCII-hex tail
    /// of the poll-body convention).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_id: Option<String>,
    /// ZEB-536: materialized reactions on this message (empty when none).
    #[serde(default)]
    pub reactions: Vec<crate::community_channel_log::ReactionDto>,
    /// ZEB-534: owner-ids (lowercase hex) this message addresses. Omitted
    /// when the post carries no mentions so existing consumers never see
    /// `mentions: null`. Recipients derive "mentions me" as
    /// `self_owner_hex ∈ mentions`. `ChannelMessageReceivedPayload` carries
    /// the full DTO, so this rides the live event automatically.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mentions: Option<Vec<String>>,
    /// ZEB-535: CAS artifacts this message references; omitted when none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<ChannelAttachmentDto>>,
}

/// ZEB-535: IPC-facing attachment (hex cid + metadata). `encrypted` is
/// derived from the CID flag so the frontend can label members-only vs
/// public without re-parsing the CID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelAttachmentDto {
    pub cid: String,
    pub mime: String,
    pub name: String,
    pub size: u64,
    pub encrypted: bool,
}

/// Magic-byte prefix for ZEB-291 Phase 1.5 poll-message bodies. Chosen
/// as `0x00` (NUL) because it is valid UTF-8 (the engine enforces UTF-8
/// on channel bodies — see `ChannelLogEngine::publish`) and is
/// vanishingly unlikely to occur at offset 0 in legitimate UTF-8 chat
/// text (which always starts with a printable codepoint).
pub const POLL_BODY_MAGIC: u8 = 0x00;

/// Poll-message body length: 1 magic byte + 64 ASCII hex chars (the
/// hex encoding of a 32-byte `PollId`). Total 65 bytes.
pub const POLL_BODY_LEN: usize = 1 + 64;

/// Which signed-event kinds may supply the authorizing descriptor for a CID.
///
/// A CID binds the *bytes*, but a descriptor's `size`/`mime` are self-declared
/// in the signed event (not derived from the bytes). When two events reference
/// the same CID — e.g. an old `Post` attachment and a later `React`
/// `emoji_attachment` — `find_attachment` returns the oldest match, so a `Post`
/// can shadow a `React` emoji descriptor (and vice-versa). The emoji-preview
/// path must therefore resolve the `React` descriptor *specifically*, so a
/// `Post` with the same CID but a different/over-cap declared size can't cause a
/// valid custom emoji to be mis-sized or rejected (CodeRabbit PR #320).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentScope {
    /// Any attachment-bearing event authorizes (Post attachments + React emoji).
    /// Used for generic artifact re-serve, where any signed reference suffices.
    Any,
    /// Only a `React`'s custom-emoji `emoji_attachment` authorizes. Used by the
    /// emoji-preview path so a Post descriptor can't shadow the emoji's.
    ReactionEmoji,
}

/// ZEB-539 / ZEB-541: if `event` references a `ChannelAttachment` whose CID
/// equals `cid` (within `scope`), return a clone of that descriptor. Used by
/// `find_attachment` to scan the log for a re-serve authorization record.
///
/// Two authorizing event shapes:
/// - `Post` — any of its `attachments` (ZEB-539). Skipped under
///   `AttachmentScope::ReactionEmoji`.
/// - `React` — its optional custom-emoji `emoji_attachment` (ZEB-541). A signed
///   React referencing an emoji CID makes that CID serve-authorizable to anyone
///   who can read the channel, exactly like a Post attachment (same power gate).
fn attachment_with_cid(
    event: &SignedChannelEvent,
    cid: &[u8; 32],
    scope: AttachmentScope,
) -> Option<ChannelAttachment> {
    match event {
        SignedChannelEvent::Post { attachments, .. } => {
            if scope == AttachmentScope::ReactionEmoji {
                return None;
            }
            attachments
                .as_ref()?
                .iter()
                .find(|att| &att.cid == cid)
                .cloned()
        }
        SignedChannelEvent::React {
            emoji_attachment, ..
        } => emoji_attachment
            .as_ref()
            .filter(|att| &att.cid == cid)
            .cloned(),
    }
}

/// Inspect a channel-message body for the ZEB-291 Phase 1.5 poll
/// convention. Returns `(Some("poll"), Some(hex))` when the body is
/// exactly `0x00` + 64 ASCII hex chars; `(None, None)` otherwise.
///
/// We use hex-encoded ASCII (not raw bytes) because the engine
/// enforces UTF-8 on bodies — random poll_id bytes would frequently
/// fail UTF-8 validation. ASCII hex is always valid UTF-8 and only
/// adds 32 bytes per poll message.
pub fn detect_poll_kind(body: &[u8]) -> (Option<&'static str>, Option<String>) {
    if body.len() != POLL_BODY_LEN || body[0] != POLL_BODY_MAGIC {
        return (None, None);
    }
    // All of body[1..] must be lowercase ASCII hex. We accept only
    // lowercase to match `hex::encode` output exactly, so a malformed
    // body that happens to be 65 bytes with a NUL prefix doesn't
    // accidentally trigger poll dispatch.
    for &b in &body[1..] {
        let is_hex = b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
        if !is_hex {
            return (None, None);
        }
    }
    // Safe: we just validated the tail is ASCII hex.
    let hex = std::str::from_utf8(&body[1..]).expect("ASCII hex is UTF-8");
    (Some("poll"), Some(hex.to_string()))
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

/// ZEB-536: emitted as `channel-reaction-received` when a React event
/// lands (local or inbound).
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelReactionReceivedPayload {
    pub community_id: String,
    pub channel_id: String,
    pub message_id: String,
    pub reactor: String,
    pub emoji: String,
    pub add: bool,
    pub at: HlcDto,
    /// ZEB-541: hex CID of the custom emoji this React references, when the
    /// reaction is a CAS-backed image emoji. `None` for unicode reactions.
    /// Lets a peer render the custom chip immediately from the live event,
    /// instead of staying blank until a `list_channel_messages`
    /// re-materialization. Additive optional field — omitted on the wire for
    /// unicode reactions, so it doesn't perturb the existing event shape.
    /// (`default` is omitted: this payload is `Serialize`-only — it is only
    /// emitted, never deserialized — so a deserialize default would be dead.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emoji_cid: Option<String>,
    /// ZEB-541: advisory plaintext byte size of the custom emoji (from the
    /// signed descriptor). `None` for unicode reactions. Pairs with
    /// `emoji_cid`; the authoritative size is re-derived at serve time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emoji_size: Option<u64>,
    /// True/false for a custom (CAS-backed) emoji: whether its CID is
    /// encrypted. `None` for unicode reactions. Mirrors `ReactionDto.encrypted`
    /// so a LIVE custom chip carries the same flag a channel reseed would,
    /// letting the UI hide the "name this emoji" affordance on encrypted chips
    /// (naming is public-only) without waiting for a reseed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypted: Option<bool>,
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

    /// ZEB-418 P3a: first-retry delay (ms) after an unanswered
    /// backfill request. Default 30_000 — test-injectable; spec D24
    /// base. Threaded from `spawn_inner_now` into each engine's
    /// `BackfillLatch` (the cap stays the
    /// `channel_backfill::BACKFILL_RETRY_CAP_MS` constant).
    pub backfill_retry_base_ms: u64,
}

impl Default for ChannelLogEngineConfig {
    fn default() -> Self {
        Self {
            log_config: ChannelLogConfig::default(),
            flush_debounce_ms: 250,
            max_dirty_ms: 1000,
            backfill_default_limit: 256,
            backfill_progress_event_interval: 16,
            backfill_retry_base_ms: crate::channel_backfill::BACKFILL_RETRY_BASE_MS,
        }
    }
}

/// ZEB-418 P3a: what the qr-driver reports after one backfill query's
/// reply stream closes. `replies` counts raw packets received
/// (pre-verification — the latch only needs full-page detection; the
/// post-page watermark is re-read from the log, which only holds
/// VERIFIED events). `limit` is the effective (clamped) page limit the
/// GET was issued with.
#[derive(Debug, Clone, Copy)]
pub struct BackfillPageReport {
    pub replies: usize,
    pub limit: usize,
}

/// Cross-task message: engine asks adapter to fire a Zenoh query on
/// its behalf. Per spec §8 — engine cannot touch `zenoh::Session`
/// directly, so backfill requests cross the boundary as messages.
#[derive(Debug)]
pub struct BackfillQueryRequest {
    /// `None` means "from the earliest available".
    pub since: Option<Hlc>,
    /// `0` means "use server default" (`backfill_default_limit`).
    pub limit: usize,
    /// ZEB-418 P3a: `Some` → the qr-driver sends exactly one
    /// `BackfillPageReport` after the query's reply stream closes
    /// naturally (a shutdown-interrupted drain drops the sender
    /// instead, so the receiver observes a closed channel). `None` →
    /// existing fire-and-forget behaviour (IPC path unchanged).
    pub outcome_tx: Option<tokio::sync::oneshot::Sender<BackfillPageReport>>,
    /// ZEB-585: an AEAD-sealed per-author [`WatermarkVector`] for a normal
    /// catch-up (`since == Some`). `None` for a full reconcile
    /// (`since == None`) or when sealing/cap degraded to the key-expr
    /// scalar path. The requester-side GET driver forwards these opaque
    /// bytes as the GET payload; the responder opens them with the channel
    /// key (it has no key on the requester side).
    pub watermark_sealed: Option<Vec<u8>>,
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
pub struct ChannelLogEngineParams {
    pub community_id: SpaceId,
    pub channel_id: ChannelId,
    pub channel_key: Arc<ChannelKey>,
    pub root_dir: PathBuf,
    pub state_at_hlc: Arc<dyn CommunityStateAtHlc + Send + Sync>,
    pub self_owner: OwnerAddr,
    pub self_device_id: String,
    pub signing_key: Arc<SigningKey>,
    pub hlc_tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
    pub sink: Arc<dyn crate::node_event_sink::NodeEventSink>,
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

pub struct ChannelLogEngine {
    community_id: SpaceId,
    channel_id: ChannelId,
    channel_key: Arc<ChannelKey>,
    log: Arc<Mutex<ChannelLog>>,
    replay_tracker: Arc<Mutex<ChannelLogReplayTracker>>,
    /// ZEB-688: monotonic count of inbound packets dropped as replays (any of
    /// `process_inbound_packet`'s three drop sites: 2a fast-path, the
    /// defensive verify-path arm, 2c atomic-recheck). Relaxed ordering — a
    /// test barrier to wait on, not a synchronization edge; read via the
    /// test-gated [`Self::replay_drop_count`].
    replay_drops: std::sync::atomic::AtomicU64,
    state_at_hlc: Arc<dyn CommunityStateAtHlc + Send + Sync>,
    self_owner: OwnerAddr,
    self_device_id: String,
    signing_key: Arc<SigningKey>,
    hlc_tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
    sink: Arc<dyn crate::node_event_sink::NodeEventSink>,
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

impl ChannelLogEngine {
    /// Construct + spawn receive/flush loops. Takes ownership of the
    /// subscriber receiver from `params`; the registry passes one end
    /// of an `mpsc::channel` and keeps the sender in the adapter.
    pub async fn new(params: ChannelLogEngineParams) -> Result<Arc<Self>, ChannelLogEngineError> {
        // Per spec §14.2 acceptance criterion + §17.4 (segments persist
        // across stop/respawn): on construction, attempt to reload any
        // on-disk log state. ChannelLog::reload returns a fresh empty
        // log if no manifest exists (cold start) — same shape as
        // ChannelLog::new in that case. The boolean second-tuple value
        // (segment_count + tail.len) is unused here; we just want the
        // tail in memory so the replay tracker rebuild below sees it.
        let (log, _total_count) = ChannelLog::reload(
            params.community_id,
            params.channel_id,
            params.root_dir,
            params.config.log_config.clone(),
        )
        .map_err(ChannelLogEngineError::Persist)?;

        // Per spec §14.2 step 5 (deduped against existing log entries
        // by message_id): on respawn the replay tracker must reflect
        // every author/device HLC already on disk. Otherwise backfill
        // replies for previously-stored events get re-appended +
        // re-emitted (the tracker's would_accept gate is what dedupes
        // backfill; it can't dedupe what it never saw). We walk every
        // sealed segment AND the tail to rebuild last_seen.
        //
        // Iteration order matters: `ChannelLogReplayTracker::record`
        // overwrites unconditionally (blind insert), so the LAST call
        // for any (author, device) lane wins. Sealed segments hold the
        // older history; the tail holds the most recent events. We
        // walk segments first then tail so the tail's `record` calls
        // win — this matches the natural ChannelLog::reload order
        // (segments first, then tail). Reversing this order would
        // regress the high-water mark whenever segments contain newer-
        // than-tail events (possible in the post-tail-flush window
        // before the next seal lands), allowing already-persisted
        // events to be re-accepted on respawn.
        //
        // Why call `record` directly (vs `check_and_advance`): on-disk
        // events were validated when first appended; re-running the
        // replay check during a rebuild would falsely flag the SECOND
        // occurrence of any (author, device) lane as a replay. We just
        // want the high-water mark recorded.
        let mut tracker = ChannelLogReplayTracker::new();
        for seg in &log.manifest.segments {
            let events = log
                .read_segment(seg)
                .map_err(ChannelLogEngineError::Persist)?;
            for ev in &events {
                tracker.record(ev);
            }
        }
        for ev in &log.tail {
            tracker.record(ev);
        }

        let engine = Arc::new(Self {
            community_id: params.community_id,
            channel_id: params.channel_id,
            channel_key: params.channel_key,
            log: Arc::new(Mutex::new(log)),
            replay_tracker: Arc::new(Mutex::new(tracker)),
            replay_drops: std::sync::atomic::AtomicU64::new(0),
            state_at_hlc: params.state_at_hlc,
            self_owner: params.self_owner,
            self_device_id: params.self_device_id,
            signing_key: params.signing_key,
            hlc_tracker: params.hlc_tracker,
            sink: params.sink,
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
                    // ZEB-288: cheap early-out — skip the decrypt/verify
                    // work once shutdown has been signalled. This is an
                    // OPTIMIZATION, not the durability guarantee.
                    // `notify_waiters()` stores no permit, so a shutdown
                    // that lands while this loop is inside
                    // `process_inbound_packet` is not observed here until
                    // the next iteration, and the biased recv arm can
                    // still win one more time. The AUTHORITATIVE guard
                    // that no event is appended after shutdown's flush
                    // lives under the `log` lock in
                    // `process_inbound_packet` step 3 — see the note
                    // there. (CodeAnt: the top-of-loop check alone leaves
                    // a post-flush-append window open.)
                    if closing.load(Ordering::SeqCst) {
                        break;
                    }
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
        self.collect_events(since, limit, |_| true).await
    }

    /// ZEB-536: Post-only variant of `list_messages`. Backs the pre-fork
    /// snapshot (message-only for v1) — pages by POSTS RETURNED so a long
    /// reaction run cannot exhaust the pull budget before later posts
    /// (CodeRabbit PR #314). React (and any future non-Post) events are
    /// skipped and do NOT count toward `limit`.
    pub async fn list_post_events(
        &self,
        since: Option<Hlc>,
        limit: usize,
    ) -> Result<Vec<SignedChannelEvent>, ChannelLogEngineError> {
        self.collect_events(since, limit, |ev| {
            matches!(ev, SignedChannelEvent::Post { .. })
        })
        .await
    }

    /// Shared backing for `list_messages` / `list_post_events`: walk sealed
    /// segments (oldest-first) then the in-memory tail in HLC order, keeping
    /// only events for which `keep` returns true and counting ONLY kept
    /// events toward `limit`. Paging by retained events means a filtered-out
    /// run (e.g. a long reaction streak) cannot exhaust the budget before
    /// later kept events (CodeRabbit PR #314).
    async fn collect_events(
        &self,
        since: Option<Hlc>,
        limit: usize,
        keep: impl Fn(&SignedChannelEvent) -> bool + Send + 'static,
    ) -> Result<Vec<SignedChannelEvent>, ChannelLogEngineError> {
        let effective_limit = if limit == 0 {
            self.config.backfill_default_limit
        } else {
            limit
        };

        // ZEB-591: snapshot the segment descriptors + in-memory tail + root
        // path UNDER the lock, then drop the lock and read the sealed segments
        // off the async executor via `spawn_blocking` (they use synchronous
        // `std::fs::read`). Mirrors `find_attachment` so a large catch-up no
        // longer stalls concurrent log ops (e.g. live `append`) for the
        // duration of the disk reads. The tail is bounded by
        // `seal_threshold_events`, so the under-lock clone is cheap.
        let (segments, tail, root): (Vec<SegmentDescriptor>, Vec<SignedChannelEvent>, _) = {
            let log = self.log.lock().await;
            (
                log.manifest.segments.clone(),
                log.tail.clone(),
                log.root().to_path_buf(),
            )
        };

        // Phase 2 stores events in the tail (newest, in-memory) + sealed
        // segments (older, on-disk; sorted ascending by `range.0`). For correct
        // HLC-order iteration we walk segments first, then tail. Counting ONLY
        // kept events toward `limit` means a filtered-out run (e.g. a long
        // reaction streak) can't exhaust the budget before later kept events
        // (CodeRabbit PR #314).
        tokio::task::spawn_blocking(move || {
            let mut out: Vec<SignedChannelEvent> = Vec::new();
            for seg in &segments {
                if let Some(since_hlc) = &since {
                    // SegmentDescriptor.range = (first_hlc, last_hlc). Skip
                    // segments entirely older-than-or-equal-to `since` — they
                    // have no events strictly newer than `since` to contribute.
                    // (No is_strictly_older_than on Hlc; express via
                    // !is_strictly_newer_than on the last-event bound.)
                    if !seg.range.1.is_strictly_newer_than(since_hlc) {
                        continue;
                    }
                }
                let events = read_segment_at(&root, seg)?;
                for ev in events {
                    if let Some(since_hlc) = &since {
                        if !ev.at().is_strictly_newer_than(since_hlc) {
                            continue;
                        }
                    }
                    if !keep(&ev) {
                        continue;
                    }
                    out.push(ev);
                    if out.len() >= effective_limit {
                        return Ok(out);
                    }
                }
            }

            // Then the in-memory tail (already snapshotted; no I/O).
            for ev in tail {
                if let Some(since_hlc) = &since {
                    if !ev.at().is_strictly_newer_than(since_hlc) {
                        continue;
                    }
                }
                if !keep(&ev) {
                    continue;
                }
                out.push(ev);
                if out.len() >= effective_limit {
                    return Ok(out);
                }
            }

            Ok(out)
        })
        .await
        .map_err(|e| {
            ChannelLogEngineError::Persist(ChannelLogPersistError::Io(format!(
                "collect_events segment-read task panicked: {e}"
            )))
        })?
        .map_err(ChannelLogEngineError::Persist)
    }

    /// ZEB-585: per-author-lane serve predicate for the watermark-vector
    /// catch-up. Serve an event when the requester has no entry for its
    /// `(author, device)` lane (never seen it → send all) or the event
    /// exceeds the requester's per-lane max. Keying by the full lane (not
    /// `device_id` alone) prevents one author's watermark from suppressing
    /// another author who shares the same `device_id`. Within a lane the
    /// HLC order reduces to `(wall_ms, logical)`.
    fn vector_serves(vector: &WatermarkVector, ev: &SignedChannelEvent) -> bool {
        let at = ev.at();
        let lane = (*ev.author(), at.device_id.clone());
        match vector.get(&lane) {
            None => true,
            Some(&(w, l)) => (at.wall_ms, at.logical) > (w, l),
        }
    }

    /// ZEB-585: vector backing for `list_messages_vector` /
    /// `list_post_events_vector`. Same segment-then-tail HLC walk as
    /// `collect_events`, but filters per authoring-device against the
    /// requester's watermark vector. Unlike the scalar path there is NO
    /// global-range segment skip — a never-seen device's events may sit in
    /// any segment, so every segment is scanned. Wire cost stays O(diff)
    /// (only matching events are returned); the O(history) disk read is the
    /// accepted Part-A cost (ZEB-585 §A.6; Part B's segment fingerprints
    /// bound it).
    async fn collect_events_vector(
        &self,
        vector: &WatermarkVector,
        limit: usize,
        keep: impl Fn(&SignedChannelEvent) -> bool + Send + 'static,
    ) -> Result<Vec<SignedChannelEvent>, ChannelLogEngineError> {
        let effective_limit = if limit == 0 {
            self.config.backfill_default_limit
        } else {
            limit
        };

        // ZEB-591: same off-lock snapshot+read as `collect_events`. The vector
        // path has NO `since` segment skip (a never-seen lane may sit in any
        // segment), so it reads every segment — which is exactly where holding
        // the lock across the disk I/O hurts most. `vector` is cloned into the
        // blocking task.
        let (segments, tail, root): (Vec<SegmentDescriptor>, Vec<SignedChannelEvent>, _) = {
            let log = self.log.lock().await;
            (
                log.manifest.segments.clone(),
                log.tail.clone(),
                log.root().to_path_buf(),
            )
        };
        let vector = vector.clone();

        tokio::task::spawn_blocking(move || {
            let mut out: Vec<SignedChannelEvent> = Vec::new();
            for seg in &segments {
                let events = read_segment_at(&root, seg)?;
                for ev in events {
                    if !Self::vector_serves(&vector, &ev) || !keep(&ev) {
                        continue;
                    }
                    out.push(ev);
                    if out.len() >= effective_limit {
                        return Ok(out);
                    }
                }
            }

            for ev in tail {
                if !Self::vector_serves(&vector, &ev) || !keep(&ev) {
                    continue;
                }
                out.push(ev);
                if out.len() >= effective_limit {
                    return Ok(out);
                }
            }

            Ok(out)
        })
        .await
        .map_err(|e| {
            ChannelLogEngineError::Persist(ChannelLogPersistError::Io(format!(
                "collect_events_vector segment-read task panicked: {e}"
            )))
        })?
        .map_err(ChannelLogEngineError::Persist)
    }

    /// ZEB-585: watermark-vector counterpart of `list_messages`.
    pub async fn list_messages_vector(
        &self,
        vector: &WatermarkVector,
        limit: usize,
    ) -> Result<Vec<SignedChannelEvent>, ChannelLogEngineError> {
        self.collect_events_vector(vector, limit, |_| true).await
    }

    /// ZEB-585: watermark-vector counterpart of `list_post_events`.
    pub async fn list_post_events_vector(
        &self,
        vector: &WatermarkVector,
        limit: usize,
    ) -> Result<Vec<SignedChannelEvent>, ChannelLogEngineError> {
        self.collect_events_vector(vector, limit, |ev| {
            matches!(ev, SignedChannelEvent::Post { .. })
        })
        .await
    }

    /// ZEB-539: returns the first verified `ChannelAttachment` in this
    /// channel's log whose CID matches `cid`, or `None`.
    ///
    /// Scans ALL stored events — every persisted segment plus the
    /// in-memory tail — NOT a bounded recent window, so an attachment
    /// shared long ago is still authorizable for re-serve. The returned
    /// record is the signed source of truth (use its `size` as
    /// authoritative). Encapsulated as an accessor so a future in-memory
    /// CID→attachment index is a drop-in replacement for this linear scan.
    ///
    /// Matches are unique by content (a CID names exactly one byte
    /// stream), so returning the first hit in oldest-first order is
    /// well-defined regardless of which event referenced it.
    pub async fn find_attachment(
        &self,
        cid: &[u8; 32],
        scope: AttachmentScope,
    ) -> Result<Option<crate::community_channel_log::ChannelAttachment>, ChannelLogEngineError>
    {
        // Qodo (High): do NOT hold the async mutex across disk I/O, and do NOT
        // run synchronous `std::fs::read` on a Tokio worker. Snapshot the
        // segment descriptors + the in-memory tail + the root path UNDER the
        // lock, drop the lock, then read the segments off the executor via
        // `spawn_blocking`. Behavior is identical: scan persisted segments
        // (oldest first) then the tail, returning the first matching
        // `ChannelAttachment` (within `scope`), else `None`.
        let (segments, tail, root): (Vec<SegmentDescriptor>, Vec<SignedChannelEvent>, _) = {
            let log = self.log.lock().await;
            (
                log.manifest.segments.clone(),
                log.tail.clone(),
                log.root().to_path_buf(),
            )
        };

        // Read + scan the persisted segments off the async executor — the
        // reads use blocking `std::fs::read`. Early-exit on the first match.
        let cid = *cid;
        let seg_hit = tokio::task::spawn_blocking(move || {
            for seg in &segments {
                let events = read_segment_at(&root, seg)?;
                for ev in &events {
                    if let Some(att) = attachment_with_cid(ev, &cid, scope) {
                        return Ok::<_, ChannelLogPersistError>(Some(att));
                    }
                }
            }
            Ok(None)
        })
        .await
        .map_err(|e| {
            ChannelLogEngineError::Persist(ChannelLogPersistError::Io(format!(
                "find_attachment segment-read task panicked: {e}"
            )))
        })?
        .map_err(ChannelLogEngineError::Persist)?;
        if seg_hit.is_some() {
            return Ok(seg_hit);
        }

        // Then the in-memory tail (already snapshotted; no I/O).
        for ev in &tail {
            if let Some(att) = attachment_with_cid(ev, &cid, scope) {
                return Ok(Some(att));
            }
        }
        Ok(None)
    }

    /// ZEB-418 P3a: max HLC currently in the verified log (segments +
    /// tail). The backfill driver's watermark source — only verified
    /// events land in the log, so a hostile holder serving garbage
    /// replies can't advance the driver's `since` cursor.
    pub async fn log_max_hlc(&self) -> Option<Hlc> {
        self.log.lock().await.max_hlc()
    }

    /// ZEB-585: snapshot the per-device catch-up watermark vector from the
    /// verified log. Counterpart of `log_max_hlc` for the vector path.
    pub async fn log_watermark_vector(&self) -> WatermarkVector {
        self.log.lock().await.watermark_vector()
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
        mentions: Option<Vec<OwnerAddr>>,
        attachments: Option<Vec<ChannelAttachment>>,
    ) -> Result<MessageId, ChannelLogEngineError> {
        if body.len() > MAX_BODY_BYTES {
            return Err(ChannelLogEngineError::BodyTooLarge {
                len: body.len(),
                max: MAX_BODY_BYTES,
            });
        }
        if let Some(m) = &mentions {
            if m.len() > MAX_MENTIONS {
                return Err(ChannelLogEngineError::TooManyMentions {
                    count: m.len(),
                    max: MAX_MENTIONS,
                });
            }
        }
        // ZEB-535: bound the attachment fan-out at mint, mirroring the
        // mentions cap. Inbound verification enforces the same cap.
        if let Some(a) = &attachments {
            if a.len() > MAX_ATTACHMENTS {
                return Err(ChannelLogEngineError::TooManyAttachments {
                    count: a.len(),
                    max: MAX_ATTACHMENTS,
                });
            }
            // ZEB-535: bound each attachment's name/mime length at mint to
            // match `verify_channel_event`'s field-length cap. Without this a
            // local publisher could mint a post that every remote peer drops at
            // verification (`AttachmentFieldTooLong`) — a cross-node
            // inconsistency. Mirrors the MAX_MENTIONS precedent.
            for att in a {
                if att.name.len() > MAX_ATTACHMENT_FIELD_BYTES
                    || att.mime.len() > MAX_ATTACHMENT_FIELD_BYTES
                {
                    return Err(ChannelLogEngineError::AttachmentFieldTooLong {
                        max: MAX_ATTACHMENT_FIELD_BYTES,
                    });
                }
            }
        }
        // ZEB-534: normalize Some(empty) -> None so an empty mentions list
        // never emits the `mn` key. Without this, `Some(vec![])` would
        // serialize `mn: []` and change the signed bytes — defeating the
        // "mention-less posts are byte-identical to pre-feature" invariant
        // for callers that default to an empty list.
        let mentions = mentions.filter(|m| !m.is_empty());
        // ZEB-535: same Some(empty) -> None normalization for attachments so
        // an empty list never emits the `pa` key (byte-identical to a post
        // with no attachments).
        let attachments = attachments.filter(|a| !a.is_empty());

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
            mentions,
            attachments,
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
            // ZEB-288 (CodeAnt): the same durability guard as the inbound
            // path (`process_inbound_packet` step 3), under the same `log`
            // lock so it is atomic w.r.t. shutdown's `flush_now()`.
            // `shutdown()` orders `closing = true` strictly before
            // `flush_now()`, so once we hold the lock: closing == false ⟹
            // the flush hasn't started and will pick up this append;
            // closing == true ⟹ shutdown is past the store and may have
            // already flushed, so appending now would strand an unflushed
            // event past the "tail is on disk on return" contract. Unlike
            // an inbound packet (re-fetched via backfill), a locally minted
            // event has NO recovery path, so we surface an error instead of
            // silently dropping — the caller must learn the post did not
            // land. (The replay-tracker `record` above is discarded on
            // shutdown, since the tracker is rebuilt from the on-disk log
            // at boot.)
            if self.closing.load(Ordering::SeqCst) {
                return Err(ChannelLogEngineError::EngineShuttingDown);
            }
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
        self.send_backfill_request(since, None).await
    }

    /// ZEB-418 P3a: like [`Self::request_backfill`], but threads a
    /// oneshot through the request so the qr-driver can report when
    /// the query's reply stream closed and how many raw packets
    /// arrived (the BackfillLatch's full-page detection input). If the
    /// query is aborted before a clean stream close (adapter shutdown,
    /// `session.get` failure), the sender is dropped and `outcome_tx`'s
    /// receiver resolves to `RecvError` instead of a report.
    pub async fn request_backfill_with_outcome(
        self: Arc<Self>,
        since: Option<Hlc>,
        outcome_tx: tokio::sync::oneshot::Sender<BackfillPageReport>,
    ) -> Result<(), ChannelLogEngineError> {
        self.send_backfill_request(since, Some(outcome_tx)).await
    }

    /// Shared tail of the two `request_backfill*` entry points.
    /// `limit: 0` = "qr-driver applies the per-engine
    /// `backfill_default_limit` (clamped to the adapter-side hard
    /// max)".
    async fn send_backfill_request(
        &self,
        since: Option<Hlc>,
        outcome_tx: Option<tokio::sync::oneshot::Sender<BackfillPageReport>>,
    ) -> Result<(), ChannelLogEngineError> {
        // ZEB-585: attach a per-author watermark vector for a normal
        // catch-up (since=Some). since=None is a full reconcile (periodic
        // floor / fresh join) — no vector, serve everything, exactly as
        // today. Sealed engine-side because the requester-side GET driver
        // holds no channel key. Over the byte cap or a seal error → no
        // vector (degrade to the key-expr scalar `since` + periodic floor).
        let watermark_sealed = if since.is_some() {
            let vector = self.log_watermark_vector().await;
            // Bound the lane count BEFORE materializing the CBOR + AEAD —
            // the byte cap only guards the responder's open path.
            if vector.len() > MAX_WATERMARK_VECTOR_ENTRIES {
                None
            } else {
                match seal_watermark_vector(self.channel_key_ref(), &vector) {
                    Ok(bytes) if bytes.len() <= MAX_WATERMARK_VECTOR_BYTES => Some(bytes),
                    _ => None,
                }
            }
        } else {
            None
        };
        self.query_request_tx
            .send(BackfillQueryRequest {
                since,
                limit: 0,
                outcome_tx,
                watermark_sealed,
            })
            .await
            .map_err(|e| ChannelLogEngineError::BackfillFailed(e.to_string()))
    }

    // ── Internal helpers ────────────────────────────────────────────────

    fn emit_message_received(&self, event: &SignedChannelEvent) {
        let dto = Self::message_dto_for_event(self.community_id, self.channel_id, event);
        let payload = ChannelMessageReceivedPayload {
            community_id: hex::encode(self.community_id.0),
            channel_id: hex::encode(self.channel_id.0),
            message: dto,
        };
        crate::node_event_sink::emit_ser(&*self.sink, "channel-message-received", &payload);
    }

    /// ZEB-536: react/un-react to a prior message. Mirrors `publish`:
    /// reserve HLC → sign → encrypt → record (loopback dedup) → append
    /// (updates the reaction index under the log lock) → broadcast →
    /// emit. `add=false` un-reacts.
    pub async fn react(
        self: &Arc<Self>,
        target: crate::community_channel_log::MessageId,
        emoji: String,
        add: bool,
        emoji_attachment: Option<crate::community_channel_log::ChannelAttachment>,
    ) -> Result<(), ChannelLogEngineError> {
        if emoji.len() > crate::community_channel_log::MAX_REACTION_EMOJI_BYTES {
            return Err(ChannelLogEngineError::ReactionEmojiTooLarge {
                len: emoji.len(),
                max: crate::community_channel_log::MAX_REACTION_EMOJI_BYTES,
            });
        }
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
        let payload = crate::community_channel_log::ChannelReactPayload {
            target,
            community_id: self.community_id,
            channel_id: self.channel_id,
            author: self.self_owner,
            at: hlc,
            // ZEB-541: custom-emoji CAS descriptor (None for unicode). Carried
            // into the signed set so the reaction→emoji binding is tamper-proof.
            emoji_attachment,
            emoji,
            add,
        };
        let event = crate::community_channel_log::sign_channel_react(&payload, &self.signing_key)
            .map_err(ChannelLogEngineError::ChannelEvent)?;
        let packet = encrypt_channel_packet(&self.channel_key, &event)
            .map_err(ChannelLogEngineError::ChannelEvent)?;
        {
            let mut tracker = self.replay_tracker.lock().await;
            tracker.record(&event);
        }
        {
            let mut log = self.log.lock().await;
            if self.closing.load(Ordering::SeqCst) {
                return Err(ChannelLogEngineError::EngineShuttingDown);
            }
            log.append(event.clone())
                .map_err(ChannelLogEngineError::Persist)?;
        }
        if let Err(e) = self.publisher_tx.try_send(packet) {
            tracing::warn!(
                community_id = ?self.community_id,
                channel_id = ?self.channel_id,
                err = ?e,
                "publisher_tx full or closed; reaction broadcast skipped"
            );
        }
        self.flush_dirty.notify_one();
        self.emit_reaction_received(&event);
        Ok(())
    }

    fn emit_reaction_received(&self, event: &SignedChannelEvent) {
        let SignedChannelEvent::React {
            target,
            author,
            at,
            emoji,
            emoji_attachment,
            add,
            ..
        } = event
        else {
            return;
        };
        // ZEB-541: surface the custom-emoji CID/size on the live event so a peer
        // can render the chip immediately. All three are `None` for unicode
        // reactions. `encrypted` is derived the same way `reactions_for` does
        // (`ContentId::from_bytes(att.cid).flags().encrypted`) so a LIVE custom
        // chip carries the flag a reseed would, gating the "name this" UI.
        let (emoji_cid, emoji_size, encrypted) = match emoji_attachment {
            Some(att) => (
                Some(hex::encode(att.cid)),
                Some(att.size),
                Some(
                    harmony_content::cid::ContentId::from_bytes(att.cid)
                        .flags()
                        .encrypted,
                ),
            ),
            None => (None, None, None),
        };
        let payload = ChannelReactionReceivedPayload {
            community_id: hex::encode(self.community_id.0),
            channel_id: hex::encode(self.channel_id.0),
            message_id: hex::encode(target.0),
            reactor: hex::encode(author.0),
            emoji: emoji.clone(),
            add: *add,
            at: HlcDto {
                wall_ms: at.wall_ms,
                logical: at.logical,
                device_id: at.device_id.clone(),
            },
            emoji_cid,
            emoji_size,
            encrypted,
        };
        crate::node_event_sink::emit_ser(&*self.sink, "channel-reaction-received", &payload);
    }

    /// ZEB-536 IPC read path: messages (Post only) with reactions folded in.
    ///
    /// Pages by POSTS returned, not raw events scanned: a `React` event never
    /// consumes the page budget. This guarantees forward progress for a client
    /// paging by `since` even across a long run of reactions between two posts.
    /// Counting reactions toward `limit` (the prior behavior) could return an
    /// empty page whose `since` cursor can't advance — the client never
    /// receives the skipped reaction HLCs, so it re-requests the same page
    /// forever (Qodo/CodeAnt finding on PR #314). Walks segments (oldest-first)
    /// then the in-memory tail under one log lock, mirroring `list_messages`,
    /// and attaches the materialized reaction view per post. A pathological
    /// all-reactions tail scans to the end of the log (bounded by log size,
    /// like `find_attachment`).
    pub async fn list_message_dtos(
        &self,
        since: Option<Hlc>,
        limit: usize,
    ) -> Result<Vec<ChannelMessageDto>, ChannelLogEngineError> {
        self.list_message_dtos_ordered(since, limit, false).await
    }

    /// ZEB-602: newest-first counterpart of `list_message_dtos`. Same
    /// strictly-newer-than `since` floor and POST-paging semantics, but the
    /// walk starts from the newest end — in-memory tail (reversed), then
    /// sealed segments newest→oldest — so `limit` bounds from the newest
    /// end and a "latest N" query usually never touches disk. Returns DTOs
    /// newest-first: the reply equals the unbounded oldest-first listing,
    /// reversed, truncated to `limit`.
    pub async fn list_message_dtos_desc(
        &self,
        since: Option<Hlc>,
        limit: usize,
    ) -> Result<Vec<ChannelMessageDto>, ChannelLogEngineError> {
        self.list_message_dtos_ordered(since, limit, true).await
    }

    async fn list_message_dtos_ordered(
        &self,
        since: Option<Hlc>,
        limit: usize,
        newest_first: bool,
    ) -> Result<Vec<ChannelMessageDto>, ChannelLogEngineError> {
        let effective_limit = if limit == 0 {
            self.config.backfill_default_limit
        } else {
            limit
        };

        let log = self.log.lock().await;
        let mut out: Vec<ChannelMessageDto> = Vec::new();

        // Shared per-event step: `since` floor + Post filter + DTO
        // projection with the materialized reaction view folded in.
        // Returns true iff the event was retained (counts toward the
        // POST page budget).
        let push_if_post = |ev: &SignedChannelEvent, out: &mut Vec<ChannelMessageDto>| -> bool {
            if let Some(since_hlc) = &since {
                if !ev.at().is_strictly_newer_than(since_hlc) {
                    return false;
                }
            }
            if !matches!(ev, SignedChannelEvent::Post { .. }) {
                return false;
            }
            let mut dto = Self::message_dto_for_event(self.community_id, self.channel_id, ev);
            dto.reactions = log.reactions_for(ev.id(), &self.self_owner);
            out.push(dto);
            true
        };

        if newest_first {
            // Newest end first: tail reversed, then segments newest→oldest
            // with events reversed within each. The `since` filter stays
            // `continue`-style (no early break), mirroring the oldest-first
            // walk's conservatism about intra-log ordering — the result is
            // defined as the reversed unbounded asc listing, truncated.
            for ev in log.tail.iter().rev() {
                if push_if_post(ev, &mut out) && out.len() >= effective_limit {
                    return Ok(out);
                }
            }
            for seg in log.manifest.segments.iter().rev() {
                if let Some(since_hlc) = &since {
                    if !seg.range.1.is_strictly_newer_than(since_hlc) {
                        continue;
                    }
                }
                let events = log
                    .read_segment(seg)
                    .map_err(ChannelLogEngineError::Persist)?;
                for ev in events.iter().rev() {
                    if push_if_post(ev, &mut out) && out.len() >= effective_limit {
                        return Ok(out);
                    }
                }
            }
        } else {
            for seg in &log.manifest.segments {
                if let Some(since_hlc) = &since {
                    if !seg.range.1.is_strictly_newer_than(since_hlc) {
                        continue;
                    }
                }
                let events = log
                    .read_segment(seg)
                    .map_err(ChannelLogEngineError::Persist)?;
                for ev in &events {
                    if push_if_post(ev, &mut out) && out.len() >= effective_limit {
                        return Ok(out);
                    }
                }
            }
            for ev in &log.tail {
                if push_if_post(ev, &mut out) && out.len() >= effective_limit {
                    return Ok(out);
                }
            }
        }

        Ok(out)
    }

    /// Public accessor: project a `SignedChannelEvent` to the IPC
    /// `ChannelMessageDto` shape using the engine's `(community_id,
    /// channel_id)` context. Returns `None` for non-message events
    /// (e.g. `React`). The IPC layer (`list_channel_messages`)
    /// uses this to project the engine's `list_messages` output.
    /// Wraps the existing private `message_dto_for_event` so the emit
    /// helper and the IPC projection stay symmetric — change one,
    /// change both.
    pub fn event_to_dto(&self, event: &SignedChannelEvent) -> Option<ChannelMessageDto> {
        match event {
            SignedChannelEvent::Post { .. } => Some(Self::message_dto_for_event(
                self.community_id,
                self.channel_id,
                event,
            )),
            _ => None,
        }
    }

    /// Engine-free projection for contexts holding bare persisted events
    /// with no live engine — the pre-fork snapshot IPC (`get_pre_fork_snapshot`,
    /// ZEB-538), where the original community's engine may not exist locally.
    /// Uses the event's own embedded `(community_id, channel_id)` (which the
    /// snapshot's carried history is keyed by), where `event_to_dto` stamps
    /// the engine's context. Both delegate to `message_dto_for_event`: there
    /// is exactly one `ChannelMessageDto` projection.
    pub fn event_to_dto_embedded(event: &SignedChannelEvent) -> Option<ChannelMessageDto> {
        match event {
            SignedChannelEvent::Post {
                community_id,
                channel_id,
                ..
            } => Some(Self::message_dto_for_event(
                *community_id,
                *channel_id,
                event,
            )),
            _ => None,
        }
    }

    fn message_dto_for_event(
        community_id: SpaceId,
        channel_id: ChannelId,
        event: &SignedChannelEvent,
    ) -> ChannelMessageDto {
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
            mentions,
            attachments,
            reply_to,
            ..
        } = event
        else {
            unreachable!("message_dto_for_event called on non-Post event; callers filter to Post");
        };

        let body_bytes = body.as_bytes().to_vec();
        let (kind, poll_id) = detect_poll_kind(&body_bytes);
        ChannelMessageDto {
            message_id: hex::encode(id.0),
            community_id: hex::encode(community_id.0),
            channel_id: hex::encode(channel_id.0),
            author: hex::encode(author.0),
            at: HlcDto {
                wall_ms: at.wall_ms,
                logical: at.logical,
                device_id: at.device_id.clone(),
            },
            body: body_bytes,
            reply_to: reply_to.map(|m| hex::encode(m.0)),
            // Omit an empty mentions list to honor the DTO contract
            // (no-mention posts have no `mentions` field). `publish()`
            // normalizes `Some([])` -> `None` at mint time, but this
            // projection also runs on arbitrary inbound/persisted events,
            // where a remote peer could sign `mn: []` (empty passes the
            // cap check). Filter here so the DTO is consistent regardless.
            mentions: mentions
                .as_ref()
                .filter(|v| !v.is_empty())
                .map(|v| v.iter().map(|a| hex::encode(a.0)).collect()),
            // ZEB-535: project the signed attachment list to DTOs. `encrypted`
            // is derived from the CID header flag so the frontend can label
            // members-only vs public without re-parsing the CID. An empty list
            // (a remote peer could sign `pa: []`) projects to None for a
            // consistent DTO, mirroring the mentions normalization above.
            attachments: attachments.as_ref().filter(|v| !v.is_empty()).map(|v| {
                v.iter()
                    .map(|a| ChannelAttachmentDto {
                        cid: hex::encode(a.cid),
                        mime: a.mime.clone(),
                        name: a.name.clone(),
                        size: a.size,
                        encrypted: harmony_content::cid::ContentId::from_bytes(a.cid)
                            .flags()
                            .encrypted,
                    })
                    .collect()
            }),
            kind,
            poll_id,
            reactions: Vec::new(),
        }
    }

    fn emit_degraded(&self, reason: &str) {
        let payload = serde_json::json!({
            "communityId": hex::encode(self.community_id.0),
            "channelId": hex::encode(self.channel_id.0),
            "reason": reason,
        });
        self.sink.emit("channel-log-degraded", payload);
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

        // 2. Verify chain. The replay_tracker Mutex must NOT be held
        // across the async I/O inside verify_channel_event (identity
        // resolver + state snapshot — both can take tens to hundreds
        // of ms during aggressive backfill). Holding it would stall
        // concurrent IPC publish() calls that share the same tracker.
        //
        // Three-step pattern:
        //   2a. Read-only fast-path replay check under the lock — bail
        //       early if we already saw this event. Avoids burning
        //       async I/O on a known duplicate.
        //   2b. Async verification OUTSIDE the lock with a throwaway
        //       tracker. The throwaway-tracker advance is wasted work
        //       (writes to an empty BTreeMap, dropped); the real
        //       replay decision is made under the lock in step 2c.
        //   2c. Atomic check_and_advance under the lock for the real
        //       commit. If a concurrent receive of the same event won
        //       the race between 2a and 2c, check_and_advance returns
        //       Err(Replay) and we drop silently.
        //
        // TOCTOU window between 2a and 2c is intentionally re-checked
        // at 2c — 2c is the authoritative gate. ChannelLogReplayTracker
        // is a `BTreeMap::new()` under the hood, so the throwaway
        // construction is cheap.

        // 2a. Fast-path replay check.
        {
            let tracker = self.replay_tracker.lock().await;
            if let Err(e) = tracker.would_accept(&event) {
                tracing::debug!(
                    community_id = ?self.community_id,
                    channel_id = ?self.channel_id,
                    err = ?e,
                    "drop replay (fast-path)"
                );
                self.replay_drops
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
            // Drop tracker lock here so async verify doesn't hold it.
        }

        // 2b. Async verification OUTSIDE the lock. Pass a throwaway
        // tracker so verify_channel_event's signature is satisfied;
        // the throwaway's `record` side-effect on Ok is discarded —
        // the real tracker advance happens in step 2c.
        let mut throwaway_tracker = ChannelLogReplayTracker::new();
        let verify_result = verify_channel_event(
            &event,
            &self.community_id,
            &self.channel_id,
            self.state_at_hlc.as_ref(),
            &mut throwaway_tracker,
        )
        .await;
        if let Err(e) = verify_result {
            match &e {
                ChannelEventError::Replay { .. } => {
                    // Throwaway tracker was empty so this should never
                    // trigger from the throwaway. Defensive log.
                    tracing::debug!(
                        community_id = ?self.community_id,
                        channel_id = ?self.channel_id,
                        err = ?e,
                        "drop replay (verify path)"
                    );
                    self.replay_drops
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

        // 2c. Atomic re-check + commit under the lock. If a concurrent
        // receive of the same event won the race between 2a and 2c,
        // check_and_advance returns Err(Replay) and we drop silently.
        {
            let mut tracker = self.replay_tracker.lock().await;
            if let Err(e) = tracker.check_and_advance(&event) {
                tracing::debug!(
                    community_id = ?self.community_id,
                    channel_id = ?self.channel_id,
                    err = ?e,
                    "drop replay (atomic-recheck)"
                );
                self.replay_drops
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        }

        // 3. Append. The `closing` check MUST sit under the `log` lock —
        // the same lock `flush_now()` takes — so it is atomic w.r.t.
        // shutdown's synchronous flush (ZEB-288, CodeAnt Critical).
        // `shutdown()` orders `closing = true` strictly BEFORE
        // `flush_now()`, so once we hold the lock:
        //   * closing == false ⟹ the store-then-flush sequence has not
        //     started, so the flush that follows will pick up this
        //     append (it runs after we release the lock);
        //   * closing == true  ⟹ shutdown is past the store and may have
        //     already flushed, so we must NOT append — doing so would
        //     strand an unflushed event and violate shutdown's "the
        //     in-memory tail is on disk by the time it returns" contract.
        // A dropped inbound packet is re-fetched via backfill on the next
        // engine start; the design already tolerates that (and the
        // replay-tracker advance at step 2c is discarded on shutdown,
        // since the tracker is rebuilt from the on-disk log at boot).
        let appended = {
            let mut log = self.log.lock().await;
            if self.closing.load(Ordering::SeqCst) {
                return;
            }
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
        match &event {
            SignedChannelEvent::React { .. } => self.emit_reaction_received(&event),
            _ => self.emit_message_received(&event),
        }
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
impl ChannelLogEngine {
    pub(crate) fn log_for_test(&self) -> &Arc<Mutex<ChannelLog>> {
        &self.log
    }

    pub(crate) fn notify_dirty_for_test(&self) {
        self.flush_dirty.notify_one();
    }
}

impl ChannelLogEngine {
    /// Borrow the per-channel encryption key. Used by the registry's
    /// `read_for_query` callback to encrypt backfill replies in the
    /// same wire shape as live broadcast packets (spec §17.1).
    pub(crate) fn channel_key_ref(&self) -> &ChannelKey {
        self.channel_key.as_ref()
    }

    /// ZEB-592: respond to one RBSR reconciliation round. Opens the sealed
    /// request (cap-before-alloc + AEAD inside `open_rbsr_message`; returns
    /// `None` on cap/decrypt failure so the caller falls back to the legacy
    /// path), computes the reply against the local log, resolves any `Have`
    /// keys to their events for inline transfer (under the same log lock), and
    /// seals the reply. Returns `(sealed_reply, have_events)`.
    // ZEB-593: called by the `rbsr/**` Zenoh queryable (via the registry's
    // `RbsrAdapterHooks::respond` closure).
    pub(crate) async fn rbsr_respond(
        &self,
        sealed_request: &[u8],
    ) -> Option<(Vec<u8>, Vec<SignedChannelEvent>)> {
        let key = self.channel_key_ref();
        let request = crate::community_channel_log::open_rbsr_message(key, sealed_request).ok()?;
        let log = self.log.lock().await;
        let reply = crate::channel_rbsr::respond(&request, &*log);
        let have_keys: Vec<crate::channel_rbsr::ReconcileKey> = reply
            .ranges
            .iter()
            .flat_map(|r| match &r.mode {
                crate::channel_rbsr::RbsrMode::Have(ks) => ks.clone(),
                _ => Vec::new(),
            })
            .collect();
        let have_events = match log.events_for_keys(&have_keys) {
            Ok(events) if events.len() == have_keys.len() => events,
            // A segment read failed, or a Have key couldn't be resolved to its
            // body → don't advertise keys we can't back with events. Fail so the
            // requester falls back (vector path / retry) instead of treating the
            // range as resolved and silently losing those events.
            _ => {
                drop(log);
                return None;
            }
        };
        drop(log);
        let sealed = crate::community_channel_log::seal_rbsr_message(key, &reply).ok()?;
        Some((sealed, have_events))
    }

    /// ZEB-593 (requester half): build + seal this round-0 RBSR request — one
    /// `Fingerprint` over the whole canonical universe — under the channel key.
    /// Sealing a small fixed-shape message is effectively infallible; on the
    /// off chance it fails, an empty payload makes the responder's `open` fail
    /// and the driver falls back to the vector path (no panic on the hot path).
    pub(crate) async fn rbsr_build_initial(&self) -> Vec<u8> {
        let req = {
            let log = self.log.lock().await;
            crate::channel_rbsr::initial_request(&*log)
        };
        crate::community_channel_log::seal_rbsr_message(self.channel_key_ref(), &req)
            .unwrap_or_default()
    }

    /// ZEB-688: monotonic count of inbound packets dropped by the replay
    /// tracker (all three `process_inbound_packet` drop sites). Test-only
    /// observability: from outside the engine, a correctly-dropped replay and
    /// a not-yet-arrived packet are indistinguishable, so replay-rejection
    /// tests previously proved the drop with a fixed sleep — a negative
    /// assertion whose only failure mode is a spurious pass. Waiting for this
    /// counter to increment makes the assertion deterministic.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn replay_drop_count(&self) -> u64 {
        self.replay_drops.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// ZEB-593 (requester half): ingest one round's reply frames and advance the
    /// reconcile. The GET runs under `ConsolidationMode::None`, so a round can
    /// draw frames from **more than one** remote holder; the frames are NOT
    /// positional. Each frame is classified by trying to open it as a sealed
    /// `RbsrMessage` (the reply, our AAD) — anything else is an encrypted
    /// channel-event packet (a responder's inline `Have`, the channel-packet
    /// AAD). All `Have` packets route through the **same**
    /// [`Self::process_inbound_packet`] verify/replay/append/flush path as live
    /// gossip (RBSR is a different *delivery* path, not a different *trust* path;
    /// dedup makes overlapping sends from multiple holders harmless). One reply
    /// message drives the next round's narrowing. Returns the count of `Have`
    /// packets actually ingested this round.
    pub(crate) async fn rbsr_ingest_and_next(
        self: &Arc<Self>,
        frames: Vec<Vec<u8>>,
    ) -> crate::event_loop::RbsrStep {
        use crate::event_loop::RbsrStep;
        let key = self.channel_key_ref();
        let mut reply: Option<crate::channel_rbsr::RbsrMessage> = None;
        let mut saw_extra_reply = false;
        let mut have_packets: Vec<Vec<u8>> = Vec::new();
        for frame in frames {
            match crate::community_channel_log::open_rbsr_message(key, &frame) {
                Ok(msg) if reply.is_none() => reply = Some(msg),
                // A second sealed reply means a second remote holder answered.
                Ok(_) => saw_extra_reply = true,
                // Not an RBSR message → an inline `Have` channel packet.
                Err(_) => have_packets.push(frame),
            }
        }
        // No responder reply this round → fall back.
        let Some(reply) = reply else {
            return RbsrStep::Failed;
        };
        // Multiple holders answered with different logs: a later holder may hold
        // ranges this (possibly all-`Skip`) first reply doesn't, so converging
        // on one reply could end catch-up with events still missing. Fall back
        // to the dedup-tolerant `since/**` vector path, which handles multiple
        // holders safely. (Common reconnect = a single holder → RBSR proceeds.)
        if saw_extra_reply {
            return RbsrStep::Failed;
        };
        let ingested = have_packets.len();
        for packet in have_packets {
            self.process_inbound_packet(packet).await;
        }
        // Recompute our own fingerprints over the (now-updated) log and narrow.
        let next = {
            let log = self.log.lock().await;
            let (_missing, next) = crate::channel_rbsr::process_reply(&reply, &*log);
            next
        };
        match next {
            None => RbsrStep::Converged { ingested },
            Some(msg) => match crate::community_channel_log::seal_rbsr_message(key, &msg) {
                Ok(sealed) => RbsrStep::Continue {
                    ingested,
                    next: sealed,
                },
                Err(_) => RbsrStep::Failed,
            },
        }
    }

    /// ZEB-350: clone the `Arc<ChannelKey>` so the voice relay can hold the key
    /// for the lifetime of a join without borrowing the engine.
    pub(crate) fn channel_key_arc(&self) -> std::sync::Arc<ChannelKey> {
        std::sync::Arc::clone(&self.channel_key)
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
pub struct ChannelLogRegistryConfig {
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
    /// ZEB-445: mode-agnostic event sink — propagated into each engine
    /// for `channel-message-received` / `channel-log-degraded` /
    /// `channel-backfill-progress` event emission.
    pub sink: Arc<dyn crate::node_event_sink::NodeEventSink>,
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
    /// ZEB-434 Task 7: transport-epoch watch receiver (bumped by the
    /// event loop's 5s peer refresh whenever a never-before-seen zenoh
    /// zid appears). Cloned into every spawned channel-log backfill
    /// driver so a satisfied driver parks on Idle and re-arms — with a
    /// fresh verified-log watermark — when a peer arrives/recovers
    /// (closes P3a spec §9's deferred transport-recovery hook). `None`
    /// (tests, callers without a transport watch) preserves the legacy
    /// return-on-Idle driver behavior.
    pub transport_epoch_rx: Option<tokio::sync::watch::Receiver<u64>>,
    /// ZEB-599 Direction 1: presence-driven full-reconcile watch. Bumped by the
    /// per-community presence subscriber whenever a new device enters a roster
    /// (a new potential holder became reachable cross-WAN). Cloned into every
    /// spawned channel-log backfill driver so a satisfied driver re-arms with a
    /// FULL reconcile (`since = None`) within the cooldown — the fast,
    /// relay-mediated analogue of the ~1h anti-entropy floor, closing the gap
    /// where a NAT'd cross-WAN peer's below-watermark backlog only healed
    /// hourly. `None` (tests / callers with no presence signal) preserves the
    /// prior behavior.
    pub presence_resync_rx: Option<tokio::sync::watch::Receiver<u64>>,
}

/// One registered channel: the running engine and the adapter's
/// closing flag, held together so `spawn` and `stop` operate
/// atomically on both. Storing them in a single map (rather than two
/// parallel maps) eliminates a spawn-stop race that previously could
/// orphan an adapter task — see `ChannelLogRegistry` doc.
struct EngineEntry {
    engine: Arc<ChannelLogEngine>,
    /// Closing flag for the per-channel adapter task that
    /// `event_loop::run` spawned in response to the spawn-time
    /// `ChannelLogAdapterRequest`. Flipping it to `true` causes the
    /// adapter's per-task select arms to exit on their next 1s
    /// closing-poll. Independent from the engine's internal closing
    /// flag — see `event_loop::ChannelLogAdapterRequest.closing` doc.
    closing: Arc<AtomicBool>,
    /// ZEB-418 P3a: shutdown signal for the per-channel backfill
    /// driver spawned alongside the engine. Lives next to `closing`
    /// and is flipped to `true` wherever `closing` flips (registry
    /// `stop`, which `shutdown_all` funnels through). Dropping the
    /// entry also closes the watch channel, which the driver treats
    /// as shutdown — so error paths that skip the explicit send still
    /// end the driver.
    backfill_shutdown_tx: tokio::sync::watch::Sender<bool>,
}

// ── CommunityTransactionGuard ─────────────────────────────────────────────────

/// RAII handle to an open community transaction. Drop without
/// explicit `commit().await` or `abort()` triggers the
/// `Handle::try_current()` safety-net abort with a `tracing::warn!`
/// (spec §5.2). If no runtime is present (e.g., panic-during-drop after
/// runtime teardown, or a future sync caller of `begin_transaction`),
/// Drop falls back to a synchronous `abort_transaction_internal` call
/// directly. Both paths converge on the same map cleanup.
///
/// `tx_id` tags the guard so a stale guard's deferred abort is a no-op
/// after a fresh `begin_transaction(same community_id)` has overwritten
/// the slot (spec §5.4).
pub struct CommunityTransactionGuard {
    registry: Arc<ChannelLogRegistry>,
    community_id: SpaceId,
    tx_id: u64,
    completed: std::sync::atomic::AtomicBool,
}

impl CommunityTransactionGuard {
    /// Drain the queue and fire the deferred spawns sequentially.
    /// On failure of any spawn, the remaining items are STILL attempted —
    /// only the first error is returned; subsequent errors are logged at
    /// `warn` (so a single bad channel doesn't strand the rest). Sets
    /// `completed = true` so `Drop` skips the safety net.
    pub async fn commit(self) -> Result<(), ChannelLogEngineError> {
        let drained = {
            let mut map = self
                .registry
                .pending_transactions
                .lock()
                .expect("pending_transactions poisoned");
            // tx_id-tag check: only drain if the slot still belongs to
            // this guard (spec §5.4).
            match map.get(&self.community_id) {
                Some(pt) if pt.tx_id == self.tx_id => {
                    let pt = map.remove(&self.community_id).expect("just-checked");
                    pt.queue
                }
                Some(pt) => {
                    tracing::warn!(
                        community_id = ?self.community_id,
                        guard_tx_id = self.tx_id,
                        slot_tx_id = pt.tx_id,
                        "stale CommunityTransactionGuard.commit — slot \
                         was overwritten; no-op"
                    );
                    self.completed
                        .store(true, std::sync::atomic::Ordering::Release);
                    return Ok(());
                }
                None => {
                    // Already aborted (or never queued anything). Treat
                    // as success.
                    self.completed
                        .store(true, std::sync::atomic::Ordering::Release);
                    return Ok(());
                }
            }
        };

        // Lock dropped. Replay each deferred spawn. We invoke a helper
        // that performs the inner-spawn body (everything except the
        // pending_transactions check). On first error, log and continue
        // with remaining items so a single failure doesn't strand the
        // rest, but surface the first error from commit.
        let mut first_err: Option<ChannelLogEngineError> = None;
        for ds in drained {
            match self.registry.spawn_inner_now(self.community_id, ds).await {
                Ok(_) => {}
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    } else {
                        tracing::warn!(
                            community_id = ?self.community_id,
                            error = ?e,
                            "additional deferred-spawn failure during commit drain \
                             (ignored, first error already captured)"
                        );
                    }
                }
            }
        }

        self.completed
            .store(true, std::sync::atomic::Ordering::Release);
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Abort the transaction. Discards the queue. Sets `completed` so
    /// `Drop` skips the safety net.
    ///
    /// Sync (not `async`): the body has no `.await` points; callers do
    /// not need to `.await` it. The `self`-by-value receiver still
    /// guarantees the `Drop` safety net is bypassed (Greptile P2 round 2).
    pub fn abort(self) {
        self.registry
            .abort_transaction_internal(self.community_id, self.tx_id);
        self.completed
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Test-only accessor for the internal `tx_id`. Used by the
    /// reentrancy unit test.
    #[cfg(test)]
    pub(crate) fn tx_id_for_test(&self) -> u64 {
        self.tx_id
    }
}

impl Drop for CommunityTransactionGuard {
    fn drop(&mut self) {
        if !self.completed.load(std::sync::atomic::Ordering::Acquire) {
            tracing::warn!(
                community_id = ?self.community_id,
                tx_id = self.tx_id,
                "CommunityTransactionGuard dropped without commit/abort — \
                 running safety net"
            );
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    let registry = Arc::clone(&self.registry);
                    let community_id = self.community_id;
                    let tx_id = self.tx_id;
                    handle.spawn(async move {
                        registry.abort_transaction_internal(community_id, tx_id);
                    });
                }
                Err(_) => {
                    // No tokio runtime present (e.g., panic-during-drop after
                    // runtime teardown, or a future sync caller of
                    // begin_transaction). Call the sync abort directly so we
                    // don't panic via tokio::spawn. Same map-cleanup outcome.
                    self.registry
                        .abort_transaction_internal(self.community_id, self.tx_id);
                }
            }
        }
    }
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
/// channel `closing` flag (inside `EngineEntry`) — flipping it to
/// `true` causes the adapter's per-task select arms to exit on their
/// next 1s closing-poll. The adapter's `JoinHandle` is dropped by
/// event_loop on shutdown.
///
/// Lock-discipline: a single `tokio::sync::Mutex<HashMap<key,
/// EngineEntry>>` holds both the engine and its closing flag together.
/// The `spawn` flow takes the engines lock, performs the idempotency
/// check, releases, does the engine construction + adapter request
/// build off-lock, then re-takes the engines lock to insert the
/// `EngineEntry` atomically. Concurrent spawns for the same
/// `(cid, chid)` resolve to a single engine via the post-construction
/// re-check (the second caller's engine is immediately stopped +
/// dropped, and the loser's adapter request is never enqueued). A
/// concurrent `spawn` + `stop` for the same key cannot orphan the
/// adapter: `stop` removes the entire `EngineEntry` (engine + closing)
/// in one map op, so the closing flag goes with the engine.
pub struct ChannelLogRegistry {
    engines: Mutex<HashMap<(SpaceId, ChannelId), EngineEntry>>,
    config: ChannelLogRegistryConfig,
    // ZEB-271: per-community deferred-spawn queue gated by an explicit
    // CommunityTransactionGuard. See spec §3.1 for the rationale.
    // std::sync::Mutex (not tokio) — critical sections never span an
    // .await, and matching the codebase's NodeState convention.
    pending_transactions: std::sync::Mutex<HashMap<SpaceId, PendingTransaction>>,
    next_tx_id: std::sync::atomic::AtomicU64,
}

impl ChannelLogRegistry {
    /// Production constructor. Wires the registry to the
    /// adapter-request bridge defined in `ChannelLogRegistryConfig`.
    /// Each `spawn` call enqueues a `ChannelLogAdapterRequest` over
    /// the bridge; `event_loop::run` drains the matching receiver and
    /// spawns the per-channel Zenoh adapter against the live session.
    pub fn new(config: ChannelLogRegistryConfig) -> Arc<Self> {
        Arc::new(Self {
            engines: Mutex::new(HashMap::new()),
            config,
            pending_transactions: std::sync::Mutex::new(HashMap::new()),
            next_tx_id: std::sync::atomic::AtomicU64::new(1),
        })
    }

    /// Open a community transaction. Subsequent `spawn` calls for this
    /// `community_id` are queued in the transaction's deferred-spawn
    /// list; they fire on `commit().await` and are dropped on
    /// `abort()` or guard drop. See spec §3.2.
    ///
    /// If a transaction for `community_id` is already open,
    /// `begin_transaction` overwrites the slot with a `tracing::warn!`
    /// (spec §5.5); the prior guard's commit/abort becomes a no-op due
    /// to tx_id mismatch.
    pub fn begin_transaction(self: &Arc<Self>, community_id: SpaceId) -> CommunityTransactionGuard {
        let tx_id = self
            .next_tx_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        {
            let mut map = self
                .pending_transactions
                .lock()
                .expect("pending_transactions poisoned");
            if let Some(prev) = map.insert(
                community_id,
                PendingTransaction {
                    tx_id,
                    queue: Vec::new(),
                },
            ) {
                tracing::warn!(
                    community_id = ?community_id,
                    prev_tx_id = prev.tx_id,
                    new_tx_id = tx_id,
                    queued = prev.queue.len(),
                    "begin_transaction overwrote an existing pending transaction \
                     (reentrant — see spec §5.5)"
                );
            }
        }
        CommunityTransactionGuard {
            registry: Arc::clone(self),
            community_id,
            tx_id,
            completed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Internal: remove the pending-transaction slot iff `tx_id`
    /// matches. Called from `Drop` (via `tokio::spawn`) and from
    /// `abort()`. tx_id mismatch is a no-op (stale-guard race).
    fn abort_transaction_internal(&self, community_id: SpaceId, tx_id: u64) {
        let mut map = self
            .pending_transactions
            .lock()
            .expect("pending_transactions poisoned");
        match map.get(&community_id) {
            Some(pt) if pt.tx_id == tx_id => {
                map.remove(&community_id);
            }
            Some(pt) => {
                tracing::warn!(
                    community_id = ?community_id,
                    guard_tx_id = tx_id,
                    slot_tx_id = pt.tx_id,
                    "abort_transaction_internal: stale guard, no-op"
                );
            }
            None => {
                // Already gone — fine.
            }
        }
    }

    /// Test-only — `true` if a `PendingTransaction` exists for
    /// `community_id`.
    #[cfg(test)]
    pub(crate) fn has_pending_transaction_for_test(&self, community_id: &SpaceId) -> bool {
        let map = self
            .pending_transactions
            .lock()
            .expect("pending_transactions poisoned");
        map.contains_key(community_id)
    }

    /// Test-only — total number of engine entries in the registry map.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub async fn engines_count_for_test(&self) -> usize {
        let engines = self.engines.lock().await;
        engines.len()
    }

    /// Spawn a per-channel engine + adapter for `(community_id, channel_id)`.
    ///
    /// **ZEB-271 transaction-aware:** if `community_id` has an open
    /// transaction (see `begin_transaction`), the spawn is queued and
    /// fires on `commit().await`. Returns `DeferredForCommit` in that
    /// case. The live-callers path (the delta consumer in
    /// `lib.rs::run_community_delta_consumer`, which runs inside the
    /// `create_community_inner` / `redeem_invite_inner` transaction
    /// scope) treats `DeferredForCommit` as success and lets `commit()`
    /// drain the queue. `reconcile_from_state`, by contrast, runs at
    /// `start_node` init outside any transaction and treats
    /// `DeferredForCommit` as a hard `InvariantViolation` error
    /// (transactions must not span boots). See spec §3.3 + §10.
    ///
    /// On error the engine + adapter are not registered (the partial
    /// `engines` insert is the commit point); the caller may retry.
    pub async fn spawn(
        self: &Arc<Self>,
        community_id: SpaceId,
        channel_id: ChannelId,
        channel_key: ChannelKey,
        state_at_hlc: Arc<dyn CommunityStateAtHlc + Send + Sync>,
        hlc_tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
    ) -> Result<SpawnOutcome, ChannelLogEngineError> {
        // ZEB-271: queue iff an open transaction targets this community.
        // Sync lock — critical section is just a HashMap mutation.
        {
            let mut map = self
                .pending_transactions
                .lock()
                .expect("pending_transactions poisoned");
            if let Some(pt) = map.get_mut(&community_id) {
                pt.queue.push(DeferredSpawn {
                    channel_id,
                    channel_key,
                    state_at_hlc,
                    hlc_tracker,
                });
                return Ok(SpawnOutcome::DeferredForCommit);
            }
        }

        // No open transaction — fast-path. Do the work and return the
        // engine.
        let ds = DeferredSpawn {
            channel_id,
            channel_key,
            state_at_hlc,
            hlc_tracker,
        };
        let engine = self.spawn_inner_now(community_id, ds).await?;
        Ok(SpawnOutcome::Spawned(engine))
    }

    /// Inner spawn body — the pre-ZEB-271 `spawn` content relocated to
    /// a helper. Called from the fast-path of the outer `spawn` AND
    /// from `CommunityTransactionGuard::commit` to drain the deferred
    /// queue. Idempotent — returns the existing Arc if already present.
    async fn spawn_inner_now(
        self: &Arc<Self>,
        community_id: SpaceId,
        ds: DeferredSpawn,
    ) -> Result<Arc<ChannelLogEngine>, ChannelLogEngineError> {
        let key = (community_id, ds.channel_id);

        // Cheap pre-check under the engines lock — returns Arc-cloned
        // existing engine on the duplicate path so we skip dir-creation,
        // engine construction, adapter spawn, and the second insert.
        {
            let engines = self.engines.lock().await;
            if let Some(existing) = engines.get(&key) {
                return Ok(Arc::clone(&existing.engine));
            }
        }

        let community_id_hex = hex::encode(community_id.0);
        let channel_id_hex = hex::encode(ds.channel_id.0);
        let root_dir = self
            .config
            .identity_dir
            .join("communities")
            .join(&community_id_hex)
            .join("channels")
            .join(&channel_id_hex);
        // Qodo (PR #267): async create_dir_all so spawn's only blocking fs
        // call doesn't park a tokio worker — matters now that
        // create_channel_impl awaits spawn eagerly on the IPC path (ZEB-467),
        // not just the background delta consumer.
        tokio::fs::create_dir_all(&root_dir).await.map_err(|e| {
            ChannelLogEngineError::Persist(ChannelLogPersistError::Io(e.to_string()))
        })?;

        // ZEB-599: read the persisted last-full-reconcile timestamp before
        // `root_dir` moves into the engine params below. The backfill
        // driver arms its FIRST periodic-resync floor at an absolute
        // deadline derived from this (restart-aware) instead of
        // `spawn + interval`, and re-persists on each fire — so a node that
        // restarts more often than hourly still gets its full reconcile.
        let backfill_state_root = root_dir.clone();
        let backfill_last_full_reconcile_ms =
            crate::community_channel_log::ChannelBackfillState::load_async(&backfill_state_root)
                .await
                .map(|s| s.last_full_reconcile_ms);

        let (publisher_tx, publisher_rx) = mpsc::channel::<Vec<u8>>(64);
        let (subscriber_tx, subscriber_rx) = mpsc::channel::<Vec<u8>>(64);
        let (query_request_tx, query_request_rx) = mpsc::channel::<BackfillQueryRequest>(8);

        let params = ChannelLogEngineParams {
            community_id,
            channel_id: ds.channel_id,
            channel_key: Arc::new(ds.channel_key),
            root_dir,
            state_at_hlc: ds.state_at_hlc,
            self_owner: self.config.self_owner,
            self_device_id: self.config.self_device_id.clone(),
            signing_key: Arc::clone(&self.config.signing_key),
            hlc_tracker: ds.hlc_tracker,
            sink: self.config.sink.clone(),
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
        // but the adapter task this closure feeds is owned by
        // `event_loop::run`'s select-arm draining of
        // `ChannelLogAdapterRequest`. The adapter exits when its
        // closing flag (held in this registry's `engines` map per
        // `EngineEntry.closing`) flips on `stop()`. The engine has no
        // back-reference to the adapter or its closure.
        let engine_for_query = Arc::clone(&engine);
        let read_for_query =
            Arc::new(
                move |since: Option<Hlc>,
                      limit: usize,
                      watermark_sealed: Option<Vec<u8>>|
                      -> std::pin::Pin<
                    Box<dyn std::future::Future<Output = Vec<Vec<u8>>> + Send>,
                > {
                    let me = Arc::clone(&engine_for_query);
                    Box::pin(async move {
                        // ZEB-585: a sealed watermark vector selects the
                        // per-device diff path; absent (or undecryptable)
                        // falls back to the scalar `since` path.
                        let events = match watermark_sealed {
                            Some(bytes) => {
                                match open_watermark_vector(me.channel_key_ref(), &bytes) {
                                    Ok(vector) => me.list_messages_vector(&vector, limit).await,
                                    Err(_) => me.list_messages(since, limit).await,
                                }
                            }
                            None => me.list_messages(since, limit).await,
                        };
                        let events = match events {
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

        // ZEB-593: engine-side RBSR closures, bundled for the adapter. Each
        // captures its own `Arc<engine>` clone (same no-cycle reasoning as
        // `read_for_query`). The engine holds the channel key + reconcile
        // source; the adapter owns the Zenoh session and only shuttles sealed
        // bytes between these closures and the wire.
        let engine_rbsr_respond = Arc::clone(&engine);
        let engine_rbsr_initial = Arc::clone(&engine);
        let engine_rbsr_ingest = Arc::clone(&engine);
        let rbsr_hooks = Some(crate::event_loop::RbsrAdapterHooks {
            respond: Arc::new(
                move |sealed: Vec<u8>| -> crate::event_loop::RbsrRespondFut {
                    let me = Arc::clone(&engine_rbsr_respond);
                    Box::pin(async move {
                        let (sealed_reply, have_events) = me.rbsr_respond(&sealed).await?;
                        // Encrypt each Have event into a wire packet; bail to
                        // `None` (→ requester falls back) rather than advertise
                        // a Have we can't back with a packet.
                        let mut packets = Vec::with_capacity(have_events.len());
                        for ev in &have_events {
                            match encrypt_channel_packet(me.channel_key_ref(), ev) {
                                Ok(p) => packets.push(p),
                                Err(_) => return None,
                            }
                        }
                        Some((sealed_reply, packets))
                    })
                },
            ),
            initial: Arc::new(move || -> crate::event_loop::RbsrInitialFut {
                let me = Arc::clone(&engine_rbsr_initial);
                Box::pin(async move { me.rbsr_build_initial().await })
            }),
            ingest: Arc::new(
                move |frames: Vec<Vec<u8>>| -> crate::event_loop::RbsrIngestFut {
                    let me = Arc::clone(&engine_rbsr_ingest);
                    Box::pin(async move { me.rbsr_ingest_and_next(frames).await })
                },
            ),
        });

        let closing = Arc::new(AtomicBool::new(false));
        // ZEB-418 P3a: lifecycle signal for the backfill driver
        // spawned below. The sender lives in the `EngineEntry` next to
        // `closing`; `stop()` flips both together.
        let (backfill_shutdown_tx, backfill_shutdown_rx) = tokio::sync::watch::channel(false);

        // Re-check under the engines lock — a concurrent spawn may
        // have inserted for the same key while we were doing the
        // heavy lifting (dir-create + engine construction). Whoever
        // inserts first wins; the loser's engine is shut down + dropped
        // here BEFORE the adapter-request is enqueued, so the adapter
        // bridge never sees the loser's halves. The registry map never
        // observes the loser's engine, so external callers see the
        // consistent winner. The `EngineEntry` insert (engine +
        // closing flag together) is atomic under this single lock —
        // a concurrent `stop` for the same key cannot remove the
        // engine without also removing the closing flag, so the
        // adapter task spawned below always remains reachable from
        // `stop()`.
        {
            let mut engines = self.engines.lock().await;
            if let Some(existing) = engines.get(&key) {
                let existing = Arc::clone(&existing.engine);
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
                        channel_id = ?ds.channel_id,
                        error = ?e,
                        "channel-log spawn race: loser shutdown failed",
                    );
                }
                // closing flag wasn't published anywhere yet; just
                // drop it (same for the backfill watch — the driver
                // is only spawned after a winning insert, below).
                // publisher_rx / subscriber_tx / query_request_rx are
                // dropped at scope end — never wired to a Zenoh
                // adapter, so no observable effect.
                return Ok(existing);
            }
            engines.insert(
                key,
                EngineEntry {
                    engine: Arc::clone(&engine),
                    closing: Arc::clone(&closing),
                    backfill_shutdown_tx,
                },
            );
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
        // Per spec §10: emit `channel-backfill-progress` from the
        // adapter's qr task. The adapter is emission-agnostic (sees no
        // sink type), so the registry constructs a closure that
        // captures the NodeEventSink and emits a
        // `ChannelBackfillProgressPayload`.
        let sink_progress = self.config.sink.clone();
        let community_id_hex_progress = hex::encode(community_id.0);
        let channel_id_hex_progress = hex::encode(ds.channel_id.0);
        let emit_backfill_progress: Arc<dyn Fn(u32, Option<u32>) + Send + Sync + 'static> =
            Arc::new(move |fetched: u32, total_estimate: Option<u32>| {
                let payload = ChannelBackfillProgressPayload {
                    community_id: community_id_hex_progress.clone(),
                    channel_id: channel_id_hex_progress.clone(),
                    fetched,
                    total_estimate,
                };
                crate::node_event_sink::emit_ser(
                    &*sink_progress,
                    "channel-backfill-progress",
                    &payload,
                );
            });
        let backfill_progress_interval = self.config.engine_config.backfill_progress_event_interval;
        let backfill_default_limit = self.config.engine_config.backfill_default_limit;

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
                    emit_backfill_progress,
                    backfill_progress_interval,
                    backfill_default_limit,
                    closing: Arc::clone(&closing),
                    rbsr_hooks,
                })
        {
            tracing::warn!(
                community_id = ?community_id,
                channel_id = ?ds.channel_id,
                error = %e,
                "channel-log adapter bridge send failed; engine spawned without wire \
                 — local publish/list_messages still work, wire transport unavailable \
                 until next start_node"
            );
        }

        // ZEB-418 P3a: every freshly inserted engine entry starts a
        // backfill driver. Running it unconditionally at engine start
        // unifies the spec's join + reconnect triggers: a fresh
        // joiner's empty log yields watermark None (request full
        // history); a reconnecting device's reloaded log yields
        // Some(watermark) (catch-up). The idempotent already-exists
        // paths above return earlier, so exactly one driver runs per
        // live entry.
        let latch = crate::channel_backfill::BackfillLatch::new_with_backoff(
            engine.log_max_hlc().await,
            self.config.engine_config.backfill_retry_base_ms,
            crate::channel_backfill::BACKFILL_RETRY_CAP_MS,
        );
        let request_engine = Arc::clone(&engine);
        let watermark_engine = Arc::clone(&engine);
        // ZEB-599: restart-aware periodic floor. `resync_interval_ms` is the
        // same per-driver jittered ~1h as before; `first_deadline` places
        // the FIRST fire relative to the persisted last-full-reconcile so a
        // frequently-restarting node still crosses the floor. The persist
        // callback offloads the tiny atomic sidecar write to `spawn_blocking`
        // so the driver never parks a worker on fsync; a failed write only
        // forfeits restart-awareness for one cycle (logged, non-fatal).
        let resync_interval_ms = crate::channel_backfill::periodic_resync_interval_ms();
        let now_for_deadline = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let resync_first_deadline_ms = crate::channel_backfill::first_resync_deadline(
            backfill_last_full_reconcile_ms,
            resync_interval_ms,
            now_for_deadline,
        );
        // ZEB-599 D3: short hex ids for the driver's `harmony_channel` debug
        // span — full ids are already logged at INFO by the spawn/adapter paths.
        let community_short = hex::encode(&community_id.0[..4]);
        let channel_short = hex::encode(&ds.channel_id.0[..4]);
        tracing::debug!(
            target: "harmony_channel",
            community = %community_short,
            channel = %channel_short,
            last_full_reconcile_ms = ?backfill_last_full_reconcile_ms,
            first_floor_deadline_ms = resync_first_deadline_ms,
            "backfill floor deadline computed (restart-aware)"
        );
        let persist_root = backfill_state_root;
        let on_full_reconcile: Arc<dyn Fn(u64) + Send + Sync> = Arc::new(move |ts: u64| {
            let root = persist_root.clone();
            tokio::task::spawn_blocking(move || {
                match crate::community_channel_log::ChannelBackfillState::save(&root, ts) {
                    Err(e) => tracing::warn!(
                        error = %e,
                        "ZEB-599: failed to persist channel backfill_state sidecar"
                    ),
                    Ok(()) => tracing::debug!(
                        target: "harmony_channel",
                        ts_ms = ts,
                        "persisted backfill_state sidecar"
                    ),
                }
            });
        });
        // ZEB-599 D3: the span attaches community/channel to every
        // `harmony_channel` debug event the driver emits — the driver's
        // signature stays identity-free (test call sites unchanged).
        use tracing::Instrument as _;
        let driver_span = tracing::debug_span!(
            target: "harmony_channel",
            "backfill",
            community = %community_short,
            channel = %channel_short,
        );
        tokio::spawn(
            crate::channel_backfill::run_backfill_driver(
                latch,
                move |since: Option<Hlc>| {
                    let me = Arc::clone(&request_engine);
                    async move {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        // Send failure = query bridge closed: the
                        // engine/adapter is gone for good (no recovery
                        // hook exists) — stop the driver instead of
                        // burning eternal futile retries.
                        if me.request_backfill_with_outcome(since, tx).await.is_err() {
                            return crate::channel_backfill::PageFetch::EngineGone;
                        }
                        match rx.await {
                            Ok(report) => crate::channel_backfill::PageFetch::Completed(
                                report.replies,
                                report.limit,
                            ),
                            // Sender dropped before a clean reply-stream
                            // close: query aborted (adapter shutdown /
                            // GET failure) — could be a transient
                            // teardown race during shutdown, so treat as
                            // no-reply → backoff; the shutdown watch ends
                            // the driver promptly anyway.
                            Err(_) => crate::channel_backfill::PageFetch::NoReply,
                        }
                    }
                },
                move || {
                    let me = Arc::clone(&watermark_engine);
                    // Post-page watermark is re-read from the LOG (only
                    // verified events land there), never taken from raw
                    // reply packets — see `run_backfill_driver` doc.
                    async move { me.log_max_hlc().await }
                },
                backfill_shutdown_rx,
                // ZEB-434 Task 7: park-on-Idle + re-arm on transport-epoch
                // bumps (None preserves the legacy return-on-Idle path).
                self.config.transport_epoch_rx.clone(),
                // ZEB-599 Direction 1: presence-driven fast full-reconcile re-arm —
                // bumped when a new roster device (potential holder) appears, so a
                // relay-mediated cross-WAN peer's below-watermark backlog heals in
                // seconds instead of waiting the ~1h floor below.
                self.config.presence_resync_rx.clone(),
                // ZEB-425: anti-entropy floor — re-arm ~hourly (jittered per
                // driver to avoid a startup thundering herd) even with no epoch
                // bump (router-only holders / late queryables / same-zid
                // reconnects the never-seen-zid signal misses).
                Some(resync_interval_ms),
                || {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64
                },
                // ZEB-599: make the floor restart-aware + persist each fire.
                Some(crate::channel_backfill::ResyncPersist {
                    first_deadline_ms: resync_first_deadline_ms,
                    on_full_reconcile,
                }),
            )
            .instrument(driver_span),
        );

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

        // ZEB-271 round 3: drop any queued deferred spawn for this
        // channel from the open transaction (if one is open). Without
        // this, a Created→Deleted sequence within the same critical
        // section would leave the deferred spawn in the queue, and
        // `commit()` would resurrect a channel that the materialized
        // state has already deleted. This can happen during
        // `redeem_invite_inner` when the engine sync surfaces both a
        // ChannelCreate and a ChannelDelete for the same channel
        // before the post-apply_space commit. Cleared first so a
        // concurrent `spawn` racing the engine remove cannot see a
        // stale queue entry. (CodeRabbit Major round 3 outside-diff.)
        {
            let mut pending = self
                .pending_transactions
                .lock()
                .expect("pending_transactions poisoned");
            if let Some(pt) = pending.get_mut(community_id) {
                pt.queue.retain(|ds| ds.channel_id != *channel_id);
            }
        }

        // Atomic remove: engine and closing flag come out together,
        // so we cannot race a concurrent `spawn` into a state where
        // the engine is gone but the closing flag remains (or vice
        // versa). Either the entry exists (we get both halves) or
        // it doesn't (no-op).
        let entry = {
            let mut engines = self.engines.lock().await;
            engines.remove(&key)
        };
        let Some(EngineEntry {
            engine,
            closing,
            backfill_shutdown_tx,
        }) = entry
        else {
            // Stop-of-unknown is a no-op (mirrors
            // CommunitySyncRegistry::stop_engine semantics).
            return Ok(());
        };

        engine.shutdown().await?;

        closing.store(true, Ordering::SeqCst);
        // ZEB-418 P3a: stop the backfill driver alongside the adapter
        // — paired with every `closing` flip (this is the registry's
        // only one; `shutdown_all` funnels through here). Send fails
        // only if the driver already exited; either way the sender
        // drops with this frame, which the driver also treats as
        // shutdown — so the early-`?` return above ends it too.
        let _ = backfill_shutdown_tx.send(true);
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
    ) -> Option<Arc<ChannelLogEngine>> {
        self.engines
            .lock()
            .await
            .get(&(*community_id, *channel_id))
            .map(|entry| Arc::clone(&entry.engine))
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
        membership_key: &EpochKey,
        state_at_hlc: Arc<dyn CommunityStateAtHlc + Send + Sync>,
        hlc_tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
    ) -> Result<(), ChannelLogEngineError> {
        // Continue on error, accumulate the LAST error, return it after
        // attempting all channels. Mirrors `shutdown_all` — bailing on
        // first error would leave every channel later in the
        // (non-deterministic) HashMap iteration order without an engine
        // for the entire session, with the affected set varying per run.
        let mut last_err: Option<ChannelLogEngineError> = None;
        for (channel_id, info) in &materialized.channels {
            if info.deleted_at.is_some() {
                continue;
            }
            let channel_key = derive_channel_key(membership_key, &community_id, channel_id);
            match self
                .spawn(
                    community_id,
                    *channel_id,
                    channel_key,
                    Arc::clone(&state_at_hlc),
                    Arc::clone(&hlc_tracker),
                )
                .await
            {
                Ok(SpawnOutcome::Spawned(_)) => {}
                Ok(SpawnOutcome::DeferredForCommit) => {
                    // reconcile_from_state runs at start_node init, outside
                    // any transaction. DeferredForCommit here means a
                    // pending transaction was left open across boots —
                    // an invariant violation, since the only owners of
                    // begin_transaction are create_community_inner /
                    // redeem_invite_inner, both of which scope the guard
                    // to the IPC handler's call frame. Hard-fail so the
                    // bug surfaces immediately rather than masquerading
                    // as a missing-engine NotRunning error later
                    // (CodeRabbit Minor round 2).
                    let msg = format!(
                        "reconcile_from_state observed pending transaction for \
                         community {community_id:?} (channel {channel_id:?}); \
                         transactions must not span boots"
                    );
                    tracing::error!(
                        community_id = ?community_id,
                        channel_id = ?channel_id,
                        "{msg}"
                    );
                    return Err(ChannelLogEngineError::InvariantViolation(msg));
                }
                Err(e) => {
                    tracing::warn!(
                        community_id = ?community_id,
                        channel_id = ?channel_id,
                        error = ?e,
                        "channel-log reconcile: spawn failed; continuing with remaining channels"
                    );
                    last_err = Some(e);
                }
            }
        }
        match last_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_channel_log::derive_channel_key;
    use crate::community_membership::ChannelInfo;
    use crate::owner_state_types::EpochKey;
    use ed25519_dalek::SigningKey;
    use harmony_identity::PrivateIdentity;
    use tempfile::TempDir;

    /// State stub: returns a known channel with low write-power and the
    /// fixture identity Joined at the engine's owner. Sufficient for
    /// receive-loop tests; verify-chain edge cases live in Phase 2.
    struct AlwaysJoinedState {
        channel_id: ChannelId,
        owner: OwnerAddr,
        /// ZEB-399: the owner's enrolled device verifying key (ed25519),
        /// surfaced so verify_channel_event can authenticate the post.
        enrolled_key: [u8; 32],
    }

    #[async_trait::async_trait]
    impl CommunityStateAtHlc for AlwaysJoinedState {
        async fn snapshot_at(
            &self,
            channel_id: &ChannelId,
            author: &OwnerAddr,
            _at: &Hlc,
        ) -> crate::community_channel_log::CommunityStateSnapshot {
            let channel = if channel_id == &self.channel_id {
                Some(ChannelInfo {
                    name: "test".to_string(),
                    write_power: 0,
                    kind: crate::community_membership::ChannelKind::Text,
                    created_at: Hlc {
                        wall_ms: 0,
                        logical: 0,
                        device_id: "fixture".to_string(),
                    },
                    deleted_at: None,
                })
            } else {
                None
            };
            let author_power = if author == &self.owner {
                Some(100)
            } else {
                None
            };
            let author_enrolled_keys = if author == &self.owner {
                vec![self.enrolled_key]
            } else {
                vec![]
            };
            crate::community_channel_log::CommunityStateSnapshot {
                channel,
                author_power,
                author_enrolled_keys,
            }
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
        engine: Arc<ChannelLogEngine>,
        publisher_rx: mpsc::Receiver<Vec<u8>>,
        subscriber_tx: mpsc::Sender<Vec<u8>>,
        query_request_rx: mpsc::Receiver<BackfillQueryRequest>,
        self_owner: OwnerAddr,
        signing_key: Arc<SigningKey>,
        channel_key: Arc<ChannelKey>,
        community_id: SpaceId,
        channel_id: ChannelId,
        tmp: TempDir,
        /// ZEB-445: recording handle onto the engine's event sink so
        /// tests can assert on emitted frames.
        sink: Arc<crate::node_event_sink::RecordingSink>,
    }

    /// ZEB-445: build a (recording handle, dyn sink) pair. NodeEventSink
    /// is impl'd on `Arc<RecordingSink>`, so the dyn coercion wraps the
    /// Arc once more.
    fn recording_sink_pair() -> (
        Arc<crate::node_event_sink::RecordingSink>,
        Arc<dyn crate::node_event_sink::NodeEventSink>,
    ) {
        let rec = crate::node_event_sink::RecordingSink::new();
        let sink: Arc<dyn crate::node_event_sink::NodeEventSink> = Arc::new(Arc::clone(&rec));
        (rec, sink)
    }

    async fn build_engine_fixture(
        seal_threshold: usize,
        flush_debounce_ms: u64,
        max_dirty_ms: u64,
    ) -> EngineFixture {
        let tmp = TempDir::new().expect("tempdir");

        let (signing_key_raw, self_owner, _identity_pub_64) = fixture_identity(0x42);
        let signing_key = Arc::new(signing_key_raw);

        let community_id = SpaceId([0xc1; 16]);
        let channel_id = ChannelId([0x77; 16]);
        let membership_key = EpochKey::new([0x55; 32]);

        let channel_key = Arc::new(derive_channel_key(
            &membership_key,
            &community_id,
            &channel_id,
        ));

        let state = Arc::new(AlwaysJoinedState {
            channel_id,
            owner: self_owner,
            enrolled_key: signing_key.verifying_key().to_bytes(),
        });

        let hlc_tracker: Arc<Mutex<BTreeMap<String, Hlc>>> = Arc::new(Mutex::new(BTreeMap::new()));

        let (publisher_tx, publisher_rx) = mpsc::channel(64);
        let (subscriber_tx, subscriber_rx) = mpsc::channel(64);
        let (query_request_tx, query_request_rx) = mpsc::channel(8);

        let (rec_sink, sink) = recording_sink_pair();

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
            self_owner,
            self_device_id: "test-device".to_string(),
            signing_key: Arc::clone(&signing_key),
            hlc_tracker,
            sink,
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
            sink: rec_sink,
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
            mentions: None,
            attachments: None,
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
        *ev.id()
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
    async fn rbsr_respond_self_reconcile_all_skip_and_rejects_oversize() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        let mut events = Vec::new();
        for i in 0..5u64 {
            let hlc = Hlc {
                wall_ms: 1_000 + i,
                logical: 0,
                device_id: "test-device".to_string(),
            };
            events.push(make_signed_event(
                fix.community_id,
                fix.channel_id,
                fix.self_owner,
                hlc,
                "x",
                &fix.signing_key,
            ));
        }
        {
            let mut log = fix.engine.log_for_test().lock().await;
            for ev in &events {
                log.append(ev.clone()).expect("append");
            }
        }

        // A request built over the engine's own log must reconcile to all-Skip
        // with nothing to transfer.
        let sealed_req = fix.engine.rbsr_build_initial().await;
        let (sealed_reply, have) = fix
            .engine
            .rbsr_respond(&sealed_req)
            .await
            .expect("responds");
        let reply = crate::community_channel_log::open_rbsr_message(
            fix.engine.channel_key_ref(),
            &sealed_reply,
        )
        .expect("open reply");
        assert!(
            reply
                .ranges
                .iter()
                .all(|r| matches!(r.mode, crate::channel_rbsr::RbsrMode::Skip)),
            "self-reconcile must converge to all-Skip: {:?}",
            reply.ranges
        );
        assert!(have.is_empty(), "no events transferred on self-reconcile");

        // Oversize payload → None (cap-before-alloc; caller falls back).
        let oversize = vec![0u8; crate::community_channel_log::MAX_RBSR_MESSAGE_BYTES + 1];
        assert!(fix.engine.rbsr_respond(&oversize).await.is_none());
    }

    #[tokio::test]
    async fn rbsr_ingest_and_next_recovers_missing_events_via_inbound_path() {
        // Two fixtures share the same deterministic channel key (fixed
        // epoch/community/channel), so a responder's sealed reply + encrypted
        // Have packets open and verify on the requester.
        let responder = build_engine_fixture(8, 250, 1000).await;
        let requester = build_engine_fixture(8, 250, 1000).await;

        let mut events = Vec::new();
        for i in 0..3u64 {
            let hlc = Hlc {
                wall_ms: 1_000 + i,
                logical: 0,
                device_id: "test-device".to_string(),
            };
            events.push(make_signed_event(
                responder.community_id,
                responder.channel_id,
                responder.self_owner,
                hlc,
                "x",
                &responder.signing_key,
            ));
        }
        {
            let mut log = responder.engine.log_for_test().lock().await;
            for ev in &events {
                log.append(ev.clone()).expect("append");
            }
        }

        // Requester drives round 0: build initial → responder replies.
        let req = requester.engine.rbsr_build_initial().await;
        let (sealed_reply, have_events) = responder
            .engine
            .rbsr_respond(&req)
            .await
            .expect("responder replies");
        assert_eq!(
            have_events.len(),
            3,
            "small leaf ships all events wholesale"
        );

        // Assemble the wire frames the adapter would deliver: the sealed reply
        // followed by one encrypted channel packet per Have event.
        let mut frames = vec![sealed_reply];
        for ev in &have_events {
            frames.push(
                crate::community_channel_log::encrypt_channel_packet(
                    requester.engine.channel_key_ref(),
                    ev,
                )
                .expect("encrypt have packet"),
            );
        }

        let step = requester.engine.rbsr_ingest_and_next(frames).await;
        assert!(
            !matches!(step, crate::event_loop::RbsrStep::Failed),
            "ingest of a well-formed reply must not fail",
        );

        // The previously-missing events are now in the requester's log, having
        // flowed through the same inbound verify + append path as live gossip.
        let msgs = requester
            .engine
            .list_messages(None, 100)
            .await
            .expect("list requester messages");
        assert_eq!(
            msgs.len(),
            3,
            "requester recovered every responder event via process_inbound_packet",
        );
    }

    #[tokio::test]
    async fn rbsr_ingest_falls_back_on_multiple_responder_replies() {
        // A ConsolidationMode::None GET can draw frames from MORE than one remote
        // holder: multiple sealed reply messages interleaved with Have packets.
        // Converging on one reply could miss events a different holder still
        // holds, so the engine falls back (Failed → vector path) the moment it
        // sees a second sealed reply — and ingests nothing this round.
        let responder = build_engine_fixture(8, 250, 1000).await;
        let requester = build_engine_fixture(8, 250, 1000).await;
        let mut events = Vec::new();
        for i in 0..3u64 {
            let hlc = Hlc {
                wall_ms: 1_000 + i,
                logical: 0,
                device_id: "test-device".to_string(),
            };
            events.push(make_signed_event(
                responder.community_id,
                responder.channel_id,
                responder.self_owner,
                hlc,
                "x",
                &responder.signing_key,
            ));
        }
        {
            let mut log = responder.engine.log_for_test().lock().await;
            for ev in &events {
                log.append(ev.clone()).expect("append");
            }
        }
        let req = requester.engine.rbsr_build_initial().await;
        let (sealed_reply, have_events) = responder
            .engine
            .rbsr_respond(&req)
            .await
            .expect("responder replies");
        // Build frames with TWO sealed reply messages (a 2nd holder) plus the
        // Have packets, in a non-frame-0 order to confirm classification (not
        // position) drives parsing.
        let mut frames = vec![sealed_reply.clone()];
        for ev in &have_events {
            frames.push(
                crate::community_channel_log::encrypt_channel_packet(
                    requester.engine.channel_key_ref(),
                    ev,
                )
                .expect("encrypt"),
            );
        }
        frames.push(sealed_reply); // a second responder's reply frame
        let step = requester.engine.rbsr_ingest_and_next(frames).await;
        assert!(
            matches!(step, crate::event_loop::RbsrStep::Failed),
            "a second sealed reply (second holder) must fall back, not converge",
        );
        // Nothing is ingested on the fall-back path — the vector path re-fetches.
        let msgs = requester
            .engine
            .list_messages(None, 100)
            .await
            .expect("list");
        assert_eq!(msgs.len(), 0, "no events ingested when falling back");
    }

    #[tokio::test]
    async fn rbsr_multi_round_bisection_recovers_large_set_in_process() {
        // Repro for the deep-bisection path (count ≫ LEAF_THRESHOLD): drive the
        // full requester↔responder round loop in-process (no Zenoh) and assert
        // the empty requester recovers EVERY responder event. This is the path
        // the 150-event reconnect integration test exercises over the wire.
        const N: u64 = 150;
        let responder = build_engine_fixture(8, 250, 1000).await;
        let requester = build_engine_fixture(8, 250, 1000).await;
        {
            let mut log = responder.engine.log_for_test().lock().await;
            for i in 0..N {
                let hlc = Hlc {
                    wall_ms: 1_000 + i,
                    logical: 0,
                    device_id: "test-device".to_string(),
                };
                log.append(make_signed_event(
                    responder.community_id,
                    responder.channel_id,
                    responder.self_owner,
                    hlc,
                    "x",
                    &responder.signing_key,
                ))
                .expect("append");
            }
        }

        // Drive rounds: responder is stateless per request; requester ingests
        // each reply (Have packets → process_inbound_packet) and narrows.
        let mut sealed = requester.engine.rbsr_build_initial().await;
        let mut rounds = 0u32;
        loop {
            rounds += 1;
            assert!(
                rounds <= crate::channel_rbsr::MAX_RBSR_ROUNDS,
                "exceeded round cap"
            );
            let (sealed_reply, have_events) = responder
                .engine
                .rbsr_respond(&sealed)
                .await
                .expect("responder replies");
            let mut frames = vec![sealed_reply];
            for ev in &have_events {
                frames.push(
                    crate::community_channel_log::encrypt_channel_packet(
                        requester.engine.channel_key_ref(),
                        ev,
                    )
                    .expect("encrypt"),
                );
            }
            match requester.engine.rbsr_ingest_and_next(frames).await {
                crate::event_loop::RbsrStep::Converged { .. } => break,
                crate::event_loop::RbsrStep::Continue { next, .. } => sealed = next,
                crate::event_loop::RbsrStep::Failed => panic!("ingest failed mid-reconcile"),
            }
        }

        let msgs = requester
            .engine
            .list_messages(None, 1000)
            .await
            .expect("list");
        assert_eq!(
            msgs.len() as u64,
            N,
            "multi-round bisection must recover every event (got {})",
            msgs.len()
        );
    }

    #[tokio::test]
    async fn collect_events_vector_serves_unseen_device_and_per_device_tail() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        let mk = |wall: u64, dev: &str, body: &str| {
            make_signed_event(
                fix.community_id,
                fix.channel_id,
                fix.self_owner,
                Hlc {
                    wall_ms: wall,
                    logical: 0,
                    device_id: dev.to_string(),
                },
                body,
                &fix.signing_key,
            )
        };
        // dev-a posts (100) then (200); dev-b posts (50) — a sub-max wall_ms
        // from a device the requester has never seen.
        let events = vec![
            mk(100, "dev-a", "a1"),
            mk(200, "dev-a", "a2"),
            mk(50, "dev-b", "b1"),
        ];
        {
            let mut log = fix.engine.log_for_test().lock().await;
            for ev in &events {
                log.append(ev.clone()).expect("append");
            }
        }

        // log_watermark_vector reflects the per-lane maxes (one author here).
        let wv = fix.engine.log_watermark_vector().await;
        assert_eq!(
            wv.get(&(fix.self_owner, "dev-a".to_string())),
            Some(&(200, 0))
        );
        assert_eq!(
            wv.get(&(fix.self_owner, "dev-b".to_string())),
            Some(&(50, 0))
        );

        // Requester has the (self, dev-a) lane up to (150,0); has NEVER seen
        // the (self, dev-b) lane.
        let mut v: WatermarkVector = std::collections::BTreeMap::new();
        v.insert((fix.self_owner, "dev-a".to_string()), (150, 0));
        let bodies: Vec<String> = fix
            .engine
            .list_messages_vector(&v, 1000)
            .await
            .expect("list_vector")
            .into_iter()
            .filter_map(|ev| match ev {
                SignedChannelEvent::Post { body, .. } => Some(body),
                _ => None,
            })
            .collect();
        assert!(
            bodies.contains(&"a2".to_string()),
            "dev-a tail beyond (150,0) served"
        );
        assert!(
            bodies.contains(&"b1".to_string()),
            "never-seen dev-b served even though its HLC (50) sorts below the requester's global max"
        );
        assert!(
            !bodies.contains(&"a1".to_string()),
            "dev-a (100,0) <= (150,0) filtered out"
        );
    }

    /// ZEB-591: characterization test for the scalar `collect_events` off-lock
    /// segment read. Pins oldest-first ordering, retained-event paging, and the
    /// `since` segment-skip across the 2-sealed-segments + tail layout — the
    /// exact path that now snapshots under the lock and reads via
    /// `spawn_blocking`. Behavior must be byte-identical to the under-lock walk.
    #[tokio::test]
    async fn collect_events_offlock_order_paging_and_since_across_segments() {
        // seal_threshold=4, 10 events => seg0 [msg-0..3], seg1 [msg-4..7],
        // tail [msg-8, msg-9].
        let fix = build_engine_fixture(4, 250, 1000).await;
        {
            let mut log = fix.engine.log_for_test().lock().await;
            for i in 0..10u64 {
                let ev = make_signed_event(
                    fix.community_id,
                    fix.channel_id,
                    fix.self_owner,
                    Hlc {
                        wall_ms: 100 + i,
                        logical: 0,
                        device_id: "test-device".to_string(),
                    },
                    &format!("msg-{i}"),
                    &fix.signing_key,
                );
                log.append(ev).expect("append");
                if (i + 1) % 4 == 0 {
                    log.seal_and_persist().expect("seal");
                }
            }
            assert_eq!(log.manifest.segments.len(), 2, "expected 2 sealed segments");
            assert_eq!(log.tail.len(), 2, "expected 2 tail events");
        }

        let bodies = |evs: Vec<SignedChannelEvent>| -> Vec<String> {
            evs.into_iter()
                .filter_map(|ev| match ev {
                    SignedChannelEvent::Post { body, .. } => Some(body),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        // Full oldest-first walk across both segments + tail.
        let all = bodies(fix.engine.list_messages(None, 1000).await.expect("list"));
        assert_eq!(
            all,
            (0..10).map(|i| format!("msg-{i}")).collect::<Vec<_>>(),
            "oldest-first ordering preserved across segments + tail"
        );

        // Limit counts retained events and cuts INSIDE the second segment
        // (msg-0..3 from seg0, msg-4,5 from seg1).
        let capped = bodies(fix.engine.list_messages(None, 6).await.expect("list"));
        assert_eq!(
            capped,
            (0..6).map(|i| format!("msg-{i}")).collect::<Vec<_>>(),
            "limit early-exit cuts mid-segment"
        );

        // `since` = msg-3's HLC (wall 103): seg0.range.1 == (103,0) is NOT
        // strictly-newer-than `since`, so seg0 is skipped wholesale; seg1 + tail
        // contribute msg-4..msg-9.
        let since = Hlc {
            wall_ms: 103,
            logical: 0,
            device_id: "test-device".to_string(),
        };
        let after = bodies(
            fix.engine
                .list_messages(Some(since), 1000)
                .await
                .expect("list"),
        );
        assert_eq!(
            after,
            (4..10).map(|i| format!("msg-{i}")).collect::<Vec<_>>(),
            "since skips the fully-older first segment and filters within"
        );
    }

    /// ZEB-591: characterization test for the vector `collect_events_vector`
    /// off-lock segment read. The vector path scans ALL segments (no `since`
    /// skip) — proves a never-seen `(author, device)` lane event sitting in the
    /// OLDEST sealed segment is still served after the read moves off-lock.
    #[tokio::test]
    async fn collect_events_vector_offlock_serves_unseen_lane_from_sealed_segment() {
        let fix = build_engine_fixture(4, 250, 1000).await;
        let mk = |wall: u64, dev: &str, body: &str| {
            make_signed_event(
                fix.community_id,
                fix.channel_id,
                fix.self_owner,
                Hlc {
                    wall_ms: wall,
                    logical: 0,
                    device_id: dev.to_string(),
                },
                body,
                &fix.signing_key,
            )
        };
        {
            let mut log = fix.engine.log_for_test().lock().await;
            // seg0 (sealed): a never-seen dev-b event buried among seen dev-a.
            for ev in [
                mk(100, "dev-a", "a1"),
                mk(50, "dev-b", "b-unseen"),
                mk(150, "dev-a", "a2"),
                mk(160, "dev-a", "a3"),
            ] {
                log.append(ev).expect("append");
            }
            log.seal_and_persist().expect("seal");
            // tail: another seen dev-a + a never-seen dev-b.
            for ev in [mk(170, "dev-a", "a4"), mk(60, "dev-b", "b-unseen2")] {
                log.append(ev).expect("append");
            }
            assert_eq!(log.manifest.segments.len(), 1, "expected 1 sealed segment");
            assert_eq!(log.tail.len(), 2, "expected 2 tail events");
        }

        // Requester knows the (self, dev-a) lane up to (200,0); has never seen
        // the (self, dev-b) lane.
        let mut v: WatermarkVector = std::collections::BTreeMap::new();
        v.insert((fix.self_owner, "dev-a".to_string()), (200, 0));
        let bodies: Vec<String> = fix
            .engine
            .list_messages_vector(&v, 1000)
            .await
            .expect("list_vector")
            .into_iter()
            .filter_map(|ev| match ev {
                SignedChannelEvent::Post { body, .. } => Some(body),
                _ => None,
            })
            .collect();
        assert!(
            bodies.contains(&"b-unseen".to_string()),
            "never-seen lane event served from the OLDEST sealed segment (off-lock scan reads all segments)"
        );
        assert!(
            bodies.contains(&"b-unseen2".to_string()),
            "never-seen lane event served from the tail"
        );
        assert!(
            !bodies.iter().any(|b| b.starts_with('a')),
            "all seen dev-a events (<= (200,0)) filtered out, segment and tail alike"
        );
    }

    #[tokio::test]
    async fn vector_does_not_collapse_two_authors_sharing_a_device_id() {
        // CodeRabbit ZEB-585: keying the watermark by device alone would let
        // author A's high lane watermark suppress author B's events on the
        // SAME device_id. The lane key is (author, device), so B's unseen
        // lane is still served. (ChannelLog::append checks only channel
        // binding, so a second author can be injected directly.)
        let fix = build_engine_fixture(8, 250, 1000).await;
        let author_a = fix.self_owner;
        let author_b = OwnerAddr([0xBB; 16]);
        let ev_a = make_signed_event(
            fix.community_id,
            fix.channel_id,
            author_a,
            Hlc {
                wall_ms: 300,
                logical: 0,
                device_id: "shared-dev".to_string(),
            },
            "a-on-shared",
            &fix.signing_key,
        );
        let ev_b = make_signed_event(
            fix.community_id,
            fix.channel_id,
            author_b,
            Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "shared-dev".to_string(),
            },
            "b-on-shared",
            &fix.signing_key,
        );
        {
            let mut log = fix.engine.log_for_test().lock().await;
            log.append(ev_a).expect("append a");
            log.append(ev_b).expect("append b");
        }
        // Requester has the (A, shared-dev) lane up to (300,0); has NEVER
        // seen the (B, shared-dev) lane.
        let mut v: WatermarkVector = std::collections::BTreeMap::new();
        v.insert((author_a, "shared-dev".to_string()), (300, 0));
        let bodies: Vec<String> = fix
            .engine
            .list_messages_vector(&v, 1000)
            .await
            .expect("list_vector")
            .into_iter()
            .filter_map(|ev| match ev {
                SignedChannelEvent::Post { body, .. } => Some(body),
                _ => None,
            })
            .collect();
        assert!(
            bodies.contains(&"b-on-shared".to_string()),
            "author B's event on the shared device (HLC 100, below A's 300) \
             must NOT be suppressed by A's lane watermark"
        );
        assert!(
            !bodies.contains(&"a-on-shared".to_string()),
            "author A's (300,0) <= requester's (300,0) → filtered"
        );
    }

    /// ZEB-270 Phase 3 Task 5: pub `event_to_dto` accessor.
    ///
    /// The IPC layer (`list_channel_messages`) projects engine output
    /// to `ChannelMessageDto` via this accessor. Verifies the
    /// projection lifts every relevant `SignedChannelEvent::Post`
    /// field, and that hex encoding + body byte projection match
    /// the spec §9.1 shape.
    /// ZEB-291 Tasks 21-23: `detect_poll_kind` recognizes the Phase
    /// 1.5 poll-body convention (`0x00` magic + 64-char ASCII hex)
    /// and rejects everything else. Locked here because the
    /// JS-side dispatch in `ChannelMessageFeed.svelte` consumes the
    /// `(kind, poll_id)` tuple this returns.
    #[test]
    fn detect_poll_kind_recognizes_valid_poll_body() {
        let poll_id = [0xab; 32];
        let hex = hex::encode(poll_id);
        let mut body = Vec::with_capacity(POLL_BODY_LEN);
        body.push(POLL_BODY_MAGIC);
        body.extend_from_slice(hex.as_bytes());
        let (kind, pid) = detect_poll_kind(&body);
        assert_eq!(kind, Some("poll"));
        assert_eq!(pid.as_deref(), Some(hex.as_str()));
    }

    #[test]
    fn detect_poll_kind_rejects_text_bodies() {
        // Plain UTF-8 chat text.
        assert_eq!(detect_poll_kind(b"hello world"), (None, None));
        // 65-byte body that is NOT prefixed with 0x00.
        let not_magic = vec![b'!'; 65];
        assert_eq!(detect_poll_kind(&not_magic), (None, None));
        // 0x00 prefix but wrong length.
        let too_short = [0u8; 32];
        assert_eq!(detect_poll_kind(&too_short), (None, None));
        // 0x00 prefix + correct length but non-hex tail.
        let mut bad_tail = vec![POLL_BODY_MAGIC];
        bad_tail.extend(std::iter::repeat_n(b'z', 64));
        assert_eq!(detect_poll_kind(&bad_tail), (None, None));
        // 0x00 prefix + uppercase hex (we accept lowercase only to
        // match hex::encode output exactly).
        let mut upper = vec![POLL_BODY_MAGIC];
        upper.extend(std::iter::repeat_n(b'A', 64));
        assert_eq!(detect_poll_kind(&upper), (None, None));
    }

    #[tokio::test]
    async fn event_to_dto_projects_post_fields() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        let hlc = Hlc {
            wall_ms: 4_242,
            logical: 7,
            device_id: "device-x".to_string(),
        };
        let ev = make_signed_event(
            fix.community_id,
            fix.channel_id,
            fix.self_owner,
            hlc,
            "hello",
            &fix.signing_key,
        );
        let dto = fix.engine.event_to_dto(&ev).expect("Post projects to Some");

        assert_eq!(dto.community_id, hex::encode(fix.community_id.0));
        assert_eq!(dto.channel_id, hex::encode(fix.channel_id.0));
        assert_eq!(dto.author, hex::encode(fix.self_owner.0));
        assert_eq!(dto.message_id, hex::encode(extract_id(&ev).0));
        assert_eq!(dto.at.wall_ms, 4_242);
        assert_eq!(dto.at.logical, 7);
        assert_eq!(dto.at.device_id, "device-x");
        assert_eq!(dto.body, b"hello".to_vec());
        assert!(dto.reply_to.is_none());
    }

    #[tokio::test]
    async fn event_to_dto_projects_mentions_as_hex() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        let m0 = OwnerAddr([0xb2; 16]);
        let m1 = OwnerAddr([0xc3; 16]);
        let id = {
            use rand::RngCore;
            let mut b = [0u8; 16];
            rand::thread_rng().fill_bytes(&mut b);
            MessageId(b)
        };
        let payload = ChannelPostPayload {
            id,
            community_id: fix.community_id,
            channel_id: fix.channel_id,
            author: fix.self_owner,
            at: Hlc {
                wall_ms: 5_000,
                logical: 0,
                device_id: "device-x".to_string(),
            },
            content_kind: 0,
            body: "hi @bob",
            reply_to: None,
            mentions: Some(vec![m0, m1]),
            attachments: None,
        };
        let ev = sign_channel_event(&payload, &fix.signing_key).expect("sign");
        let dto = fix
            .engine
            .event_to_dto(&ev)
            .expect("Post projects to a DTO");
        assert_eq!(
            dto.mentions,
            Some(vec![hex::encode(m0.0), hex::encode(m1.0)])
        );

        // Mention-less event omits the field.
        let ev_none = make_signed_event(
            fix.community_id,
            fix.channel_id,
            fix.self_owner,
            Hlc {
                wall_ms: 5_001,
                logical: 0,
                device_id: "device-x".to_string(),
            },
            "no mentions",
            &fix.signing_key,
        );
        assert!(fix
            .engine
            .event_to_dto(&ev_none)
            .expect("Post projects to a DTO")
            .mentions
            .is_none());
    }

    #[tokio::test]
    async fn event_to_dto_omits_empty_mentions() {
        // A signed event carrying `Some(vec![])` (reachable inbound: a
        // remote peer can sign `mn: []`, which passes the cap check) must
        // still project to a DTO with no `mentions` field, matching the
        // no-mention contract. `sign_channel_event` does not normalize, so
        // this builds the empty-vec event directly.
        let fix = build_engine_fixture(8, 250, 1000).await;
        let id = {
            use rand::RngCore;
            let mut b = [0u8; 16];
            rand::thread_rng().fill_bytes(&mut b);
            MessageId(b)
        };
        let payload = ChannelPostPayload {
            id,
            community_id: fix.community_id,
            channel_id: fix.channel_id,
            author: fix.self_owner,
            at: Hlc {
                wall_ms: 5_002,
                logical: 0,
                device_id: "device-x".to_string(),
            },
            content_kind: 0,
            body: "empty mentions",
            reply_to: None,
            mentions: Some(vec![]),
            attachments: None,
        };
        let ev = sign_channel_event(&payload, &fix.signing_key).expect("sign");
        assert!(fix
            .engine
            .event_to_dto(&ev)
            .expect("Post projects to a DTO")
            .mentions
            .is_none());
    }

    #[tokio::test]
    async fn event_to_dto_embedded_uses_event_ids_and_omits_empty_mentions() {
        // Engine-free projection (ZEB-538): `get_pre_fork_snapshot` holds
        // bare persisted events with no live engine, so the projection takes
        // (community, channel) from the event's own embedded fields — and
        // must normalize `Some(vec![])` mentions to None exactly like the
        // engine path (the hand-rolled literal it replaced did not, so a
        // snapshot DTO could surface `mentions: []` where the canonical
        // path surfaces None).
        let fix = build_engine_fixture(8, 250, 1000).await;
        let other_community = SpaceId([0x5a; 16]);
        let other_channel = ChannelId([0x6b; 16]);
        let payload = ChannelPostPayload {
            id: MessageId([0x11; 16]),
            community_id: other_community,
            channel_id: other_channel,
            author: fix.self_owner,
            at: Hlc {
                wall_ms: 7_000,
                logical: 0,
                device_id: "device-x".to_string(),
            },
            content_kind: 0,
            body: "carried history",
            reply_to: None,
            mentions: Some(vec![]),
            attachments: None,
        };
        let ev = sign_channel_event(&payload, &fix.signing_key).expect("sign");
        let dto = ChannelLogEngine::event_to_dto_embedded(&ev).expect("Post projects to Some");
        // IDs come from the event, not any engine context.
        assert_eq!(dto.community_id, hex::encode(other_community.0));
        assert_eq!(dto.channel_id, hex::encode(other_channel.0));
        assert!(
            dto.mentions.is_none(),
            "Some(vec![]) must normalize to None on the snapshot path"
        );
    }

    #[tokio::test]
    async fn event_to_dto_projects_attachments() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        // Build an attachment with an ENCRYPTED-flagged CID so `encrypted` is true.
        let enc_cid = harmony_content::cid::ContentId::for_book(
            b"ct",
            harmony_content::cid::ContentFlags {
                encrypted: true,
                ..Default::default()
            },
        )
        .expect("cid")
        .to_bytes();
        let att = crate::community_channel_log::ChannelAttachment {
            cid: enc_cid,
            mime: "text/plain".into(),
            name: "log.txt".into(),
            size: 9,
        };
        let id = {
            use rand::RngCore;
            let mut b = [0u8; 16];
            rand::thread_rng().fill_bytes(&mut b);
            MessageId(b)
        };
        let payload = ChannelPostPayload {
            id,
            community_id: fix.community_id,
            channel_id: fix.channel_id,
            author: fix.self_owner,
            at: Hlc {
                wall_ms: 5_000,
                logical: 0,
                device_id: "device-x".to_string(),
            },
            content_kind: 0,
            body: "see log",
            reply_to: None,
            mentions: None,
            attachments: Some(vec![att.clone()]),
        };
        let ev = sign_channel_event(&payload, &fix.signing_key).expect("sign");
        let dto = fix
            .engine
            .event_to_dto(&ev)
            .expect("Post projects to a DTO");
        let got = dto.attachments.expect("attachments present");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].cid, hex::encode(enc_cid));
        assert_eq!(got[0].name, "log.txt");
        assert_eq!(got[0].mime, "text/plain");
        assert_eq!(got[0].size, 9);
        assert!(
            got[0].encrypted,
            "encrypted-flagged cid projects encrypted=true"
        );
    }

    #[tokio::test]
    async fn event_to_dto_projects_unencrypted_attachment() {
        // Companion to event_to_dto_projects_attachments: a CID built with
        // default (non-encrypted) flags must project encrypted=false. The
        // positive-only test would still pass if the projection inverted the
        // flag, so this negative case is what actually pins the derivation.
        let fix = build_engine_fixture(8, 250, 1000).await;
        let pub_cid = harmony_content::cid::ContentId::for_book(
            b"ct",
            harmony_content::cid::ContentFlags::default(),
        )
        .expect("cid")
        .to_bytes();
        let att = crate::community_channel_log::ChannelAttachment {
            cid: pub_cid,
            mime: "text/plain".into(),
            name: "public.txt".into(),
            size: 9,
        };
        let id = {
            use rand::RngCore;
            let mut b = [0u8; 16];
            rand::thread_rng().fill_bytes(&mut b);
            MessageId(b)
        };
        let payload = ChannelPostPayload {
            id,
            community_id: fix.community_id,
            channel_id: fix.channel_id,
            author: fix.self_owner,
            at: Hlc {
                wall_ms: 5_000,
                logical: 0,
                device_id: "device-x".to_string(),
            },
            content_kind: 0,
            body: "see log",
            reply_to: None,
            mentions: None,
            attachments: Some(vec![att]),
        };
        let ev = sign_channel_event(&payload, &fix.signing_key).expect("sign");
        let dto = fix
            .engine
            .event_to_dto(&ev)
            .expect("Post projects to a DTO");
        let got = dto.attachments.expect("attachments present");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].cid, hex::encode(pub_cid));
        assert_eq!(got[0].name, "public.txt");
        assert_eq!(got[0].mime, "text/plain");
        assert_eq!(got[0].size, 9);
        assert!(
            !got[0].encrypted,
            "default-flag cid projects encrypted=false"
        );
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
            let wall_ms = ev.at().wall_ms;
            assert_eq!(
                wall_ms,
                100 + i as u64,
                "event {i} out of HLC order (got wall_ms={wall_ms})",
            );
        }

        // First/last bookend checks per spec §14.1.
        assert_eq!(listed[0].at().wall_ms, 100);
        assert_eq!(listed[9].at().wall_ms, 109);
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

    // ── ZEB-539: find_attachment ──────────────────────────────────────

    /// Build a signed Post carrying `attachments` (fixture identity, so the
    /// signature verifies). Mirrors `make_signed_event` but populates `pa`.
    fn make_signed_event_with_attachments(
        community_id: SpaceId,
        channel_id: ChannelId,
        author: OwnerAddr,
        at: Hlc,
        body: &str,
        attachments: Option<Vec<ChannelAttachment>>,
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
            mentions: None,
            attachments,
        };
        sign_channel_event(&payload, signing_key).expect("sign")
    }

    #[tokio::test]
    async fn find_attachment_scans_segments_and_tail() {
        // seal_threshold=4 with 10 events => 2 sealed segments + 2 tail
        // events (matching list_messages_walks_tail_then_segments), so this
        // exercises BOTH the persisted-segment scan and the in-memory tail.
        let fix = build_engine_fixture(4, 250, 1000).await;

        // Attachment in the very first event (lands in the OLDEST sealed
        // segment) — proves the scan is unbounded, not a recent window.
        let seg_att = ChannelAttachment {
            cid: [0xb2; 32],
            mime: "text/plain".to_string(),
            name: "old-log.txt".to_string(),
            size: 4242,
        };
        // Attachment in the last event (stays in the tail).
        let tail_att = ChannelAttachment {
            cid: [0xc3; 32],
            mime: "application/json".to_string(),
            name: "recent.json".to_string(),
            size: 99,
        };

        {
            let mut log = fix.engine.log_for_test().lock().await;
            for i in 0..10u64 {
                let hlc = Hlc {
                    wall_ms: 100 + i,
                    logical: 0,
                    device_id: "test-device".to_string(),
                };
                let attachments = match i {
                    0 => Some(vec![seg_att.clone()]),
                    9 => Some(vec![tail_att.clone()]),
                    // One event with no attachments to prove the scan skips
                    // `None` posts cleanly.
                    _ => None,
                };
                let ev = make_signed_event_with_attachments(
                    fix.community_id,
                    fix.channel_id,
                    fix.self_owner,
                    hlc,
                    &format!("msg-{i}"),
                    attachments,
                    &fix.signing_key,
                );
                log.append(ev).expect("append");
                if (i + 1) % 4 == 0 {
                    log.seal_and_persist().expect("seal");
                }
            }
            // Sanity: the layout actually splits across segments + tail.
            assert_eq!(log.manifest.segments.len(), 2, "expected 2 sealed segments");
            assert_eq!(log.tail.len(), 2, "expected 2 tail events");
        }

        // Present CID in the oldest persisted segment.
        let got = fix
            .engine
            .find_attachment(&[0xb2; 32], AttachmentScope::Any)
            .await
            .expect("find ok")
            .expect("segment attachment present");
        assert_eq!(got, seg_att, "returns the signed segment record");

        // Present CID in the in-memory tail.
        let got = fix
            .engine
            .find_attachment(&[0xc3; 32], AttachmentScope::Any)
            .await
            .expect("find ok")
            .expect("tail attachment present");
        assert_eq!(got, tail_att, "returns the signed tail record");

        // Absent CID → None.
        let none = fix
            .engine
            .find_attachment(&[0xff; 32], AttachmentScope::Any)
            .await
            .expect("find ok");
        assert!(none.is_none(), "absent cid must return None");
    }

    /// Build a signed React carrying a custom-emoji descriptor (fixture
    /// identity, so the signature verifies). Mirrors `engine.react`'s payload.
    fn make_signed_react_with_emoji(
        community_id: SpaceId,
        channel_id: ChannelId,
        author: OwnerAddr,
        at: Hlc,
        target: MessageId,
        emoji_attachment: Option<ChannelAttachment>,
        signing_key: &SigningKey,
    ) -> SignedChannelEvent {
        let payload = crate::community_channel_log::ChannelReactPayload {
            target,
            community_id,
            channel_id,
            author,
            at,
            emoji_attachment,
            emoji: String::new(),
            add: true,
        };
        crate::community_channel_log::sign_channel_react(&payload, signing_key).expect("sign react")
    }

    /// CodeRabbit PR #320: an OLDER `Post` attachment sharing the emoji CID must
    /// NOT shadow the `React` emoji descriptor for the preview path. A CID binds
    /// the bytes, but `size`/`mime` are self-declared per event — so the
    /// emoji-preview path (`AttachmentScope::ReactionEmoji`) must resolve the
    /// React descriptor specifically, while the generic scan
    /// (`AttachmentScope::Any`) still returns the oldest match (the Post).
    #[tokio::test]
    async fn find_attachment_react_emoji_scope_skips_shadowing_post() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        let shared_cid = [0x5a; 32];

        // OLDER Post referencing the shared CID with a large, over-emoji-cap
        // self-declared size and a non-image mime — the shadow that must lose.
        let post_att = ChannelAttachment {
            cid: shared_cid,
            mime: "text/plain".to_string(),
            name: "decoy.txt".to_string(),
            size: crate::MAX_CUSTOM_EMOJI_BYTES + 50_000,
        };
        // NEWER React whose emoji_attachment references the SAME CID with the
        // true (small, in-cap) descriptor.
        let emoji_att = ChannelAttachment {
            cid: shared_cid,
            mime: "image/png".to_string(),
            name: String::new(),
            size: 1234,
        };

        {
            let mut log = fix.engine.log_for_test().lock().await;
            let post = make_signed_event_with_attachments(
                fix.community_id,
                fix.channel_id,
                fix.self_owner,
                Hlc {
                    wall_ms: 100,
                    logical: 0,
                    device_id: "test-device".to_string(),
                },
                "decoy post",
                Some(vec![post_att.clone()]),
                &fix.signing_key,
            );
            log.append(post).expect("append post");
            let react = make_signed_react_with_emoji(
                fix.community_id,
                fix.channel_id,
                fix.self_owner,
                Hlc {
                    wall_ms: 200,
                    logical: 0,
                    device_id: "test-device".to_string(),
                },
                MessageId([7u8; 16]),
                Some(emoji_att.clone()),
                &fix.signing_key,
            );
            log.append(react).expect("append react");
        }

        // Generic scan returns the oldest match — the Post (decoy).
        let any = fix
            .engine
            .find_attachment(&shared_cid, AttachmentScope::Any)
            .await
            .expect("find ok")
            .expect("some match under Any");
        assert_eq!(any, post_att, "Any scope returns the oldest (Post) match");

        // Emoji-preview scope skips the Post and resolves the React descriptor,
        // so the valid emoji's true (in-cap) size is used — not the decoy's.
        let emoji = fix
            .engine
            .find_attachment(&shared_cid, AttachmentScope::ReactionEmoji)
            .await
            .expect("find ok")
            .expect("React emoji descriptor must be found, not shadowed by Post");
        assert_eq!(
            emoji, emoji_att,
            "ReactionEmoji scope must return the React descriptor, not the Post"
        );
    }

    // ── Sub-task 2B: publish ──────────────────────────────────────────

    #[tokio::test]
    async fn publish_writes_to_publisher_tx_and_appends_locally() {
        let mut fix = build_engine_fixture(8, 250, 1000).await;

        let body = b"hello channel".to_vec();
        let msg_id = Arc::clone(&fix.engine)
            .publish(body.clone(), None, None, None)
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
        // Spec §14.1 requires "log mutation + event emission" for the
        // publish + receive paths. The other publish test only checks
        // log mutation; this one closes the gap by asserting the
        // channel-message-received payload shape on the recording sink
        // (ZEB-445: emission goes through NodeEventSink, not Tauri).
        let fix = build_engine_fixture(8, 250, 1000).await;

        let body = b"emit-test-body".to_vec();
        let msg_id = Arc::clone(&fix.engine)
            .publish(body.clone(), None, None, None)
            .await
            .expect("publish");

        let payload = wait_for(
            || {
                let sink = Arc::clone(&fix.sink);
                async move {
                    sink.frames()
                        .iter()
                        .find(|(name, _)| name == "channel-message-received")
                        .map(|(_, payload)| payload.clone())
                }
            },
            Duration::from_secs(1),
        )
        .await
        .expect("sink must record channel-message-received within 1s");

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
            .publish(body, None, None, None)
            .await
            .expect_err("oversized body must reject");
        assert!(matches!(err, ChannelLogEngineError::BodyTooLarge { .. }));
    }

    #[tokio::test]
    async fn publish_rejects_too_many_mentions() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        let too_many: Vec<OwnerAddr> = (0..=MAX_MENTIONS)
            .map(|i| OwnerAddr([i as u8; 16]))
            .collect();
        assert_eq!(too_many.len(), MAX_MENTIONS + 1);
        let err = Arc::clone(&fix.engine)
            .publish(b"hi".to_vec(), None, Some(too_many), None)
            .await
            .expect_err("over-cap mentions must reject");
        assert!(
            matches!(err, ChannelLogEngineError::TooManyMentions { count, max }
                if count == MAX_MENTIONS + 1 && max == MAX_MENTIONS),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn publish_rejects_too_many_attachments() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        let too_many: Vec<crate::community_channel_log::ChannelAttachment> = (0
            ..=crate::community_channel_log::MAX_ATTACHMENTS)
            .map(|i| crate::community_channel_log::ChannelAttachment {
                cid: [i as u8; 32],
                mime: "x".into(),
                name: "n".into(),
                size: 1,
            })
            .collect();
        let err = Arc::clone(&fix.engine)
            .publish(b"hi".to_vec(), None, None, Some(too_many))
            .await
            .expect_err("over-cap must error");
        assert!(
            matches!(err, ChannelLogEngineError::TooManyAttachments { .. }),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn publish_rejects_overlong_attachment_field() {
        // ZEB-535: an attachment with a name/mime longer than
        // MAX_ATTACHMENT_FIELD_BYTES must be rejected at publish() time, so a
        // locally-minted post can't be dropped by remote peers at
        // verify_channel_event (which enforces the same cap).
        let fix = build_engine_fixture(8, 250, 1000).await;
        let overlong = vec![crate::community_channel_log::ChannelAttachment {
            cid: [1u8; 32],
            mime: "text/plain".into(),
            name: "x".repeat(MAX_ATTACHMENT_FIELD_BYTES + 1),
            size: 1,
        }];
        let err = Arc::clone(&fix.engine)
            .publish(b"hi".to_vec(), None, None, Some(overlong))
            .await
            .expect_err("over-long attachment field must error");
        assert!(
            matches!(err, ChannelLogEngineError::AttachmentFieldTooLong { max }
                if max == MAX_ATTACHMENT_FIELD_BYTES),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn publish_normalizes_empty_mentions_to_none() {
        // ZEB-534: Some(vec![]) must serialize WITHOUT the mn key (omitted),
        // so an empty mentions list is byte-identical to a mention-less post.
        let fix = build_engine_fixture(8, 250, 1000).await;
        Arc::clone(&fix.engine)
            .publish(b"hi".to_vec(), None, Some(vec![]), None)
            .await
            .expect("publish with empty mentions");
        let msgs = fix.engine.list_messages(None, 100).await.expect("list");
        assert_eq!(msgs.len(), 1);
        let dto = fix
            .engine
            .event_to_dto(&msgs[0])
            .expect("Post projects to a DTO");
        assert!(
            dto.mentions.is_none(),
            "empty mentions must normalize to None (mn key omitted)"
        );
    }

    #[tokio::test]
    async fn publish_normalizes_empty_attachments_to_none() {
        // Mirrors publish_normalizes_empty_mentions_to_none: Some(vec![])
        // attachments must serialize WITHOUT the pa key (omitted), so an
        // empty attachment list is byte-identical to an attachment-less post.
        // Load-bearing for signature stability (sig is over canonical CBOR).
        let fix = build_engine_fixture(8, 250, 1000).await;
        Arc::clone(&fix.engine)
            .publish(b"hi".to_vec(), None, None, Some(vec![]))
            .await
            .expect("publish with empty attachments");
        let msgs = fix.engine.list_messages(None, 100).await.expect("list");
        assert_eq!(msgs.len(), 1);
        let dto = fix
            .engine
            .event_to_dto(&msgs[0])
            .expect("Post projects to a DTO");
        assert!(
            dto.attachments.is_none(),
            "empty attachments must normalize to None (pa key omitted)"
        );
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

        // ZEB-688: wait for the replay drop itself instead of sleeping — the
        // counter makes the negative assertion below non-vacuous (we KNOW the
        // second packet reached the drop path before asserting nothing landed).
        wait_for(
            || async { (fix.engine.replay_drop_count() >= 1).then_some(()) },
            Duration::from_secs(2),
        )
        .await
        .expect("the replayed packet must be observed dropping");

        let listed = fix.engine.list_messages(None, 100).await.expect("list");
        assert_eq!(listed.len(), 1, "replay must be dropped");
    }

    #[tokio::test]
    async fn closing_engine_drops_inbound_without_appending() {
        // ZEB-288 durability guard (CodeAnt Critical): once `shutdown()`
        // has set `closing` (and may already have run its synchronous
        // `flush_now()`), an inbound packet that passes decrypt+verify
        // must NOT be appended — appending after the flush would strand
        // an unflushed event past shutdown's "tail is on disk on return"
        // contract. We drive `process_inbound_packet` directly with
        // `closing` already set so the step-3 under-lock guard is the
        // thing under test (deterministic — no shutdown timing race).
        let fix = build_engine_fixture(8, 250, 1000).await;
        let hlc = Hlc {
            wall_ms: 7_500,
            logical: 0,
            device_id: "remote".to_string(),
        };
        let event = make_signed_event(
            fix.community_id,
            fix.channel_id,
            fix.self_owner,
            hlc,
            "arrives-during-shutdown",
            &fix.signing_key,
        );
        let packet = encrypt_channel_packet(&fix.channel_key, &event).expect("encrypt");

        fix.engine.closing.store(true, Ordering::SeqCst);
        fix.engine.process_inbound_packet(packet).await;

        let listed = fix.engine.list_messages(None, 100).await.expect("list");
        assert!(
            listed.is_empty(),
            "a closing engine must not append inbound packets (got {})",
            listed.len()
        );
    }

    #[tokio::test]
    async fn closing_engine_publish_errors_without_appending() {
        // ZEB-288 (CodeAnt): the LOCAL publish path must also refuse to
        // append once shutdown has begun. Unlike the inbound path, a
        // locally minted event has no backfill recovery, so the guard
        // surfaces `EngineShuttingDown` rather than silently dropping —
        // the caller must learn the post did not land.
        let fix = build_engine_fixture(8, 250, 1000).await;
        fix.engine.closing.store(true, Ordering::SeqCst);

        let err = Arc::clone(&fix.engine)
            .publish(b"arrives-during-shutdown".to_vec(), None, None, None)
            .await
            .expect_err("publish on a closing engine must error");
        assert!(
            matches!(err, ChannelLogEngineError::EngineShuttingDown),
            "expected EngineShuttingDown, got {err:?}"
        );

        let listed = fix.engine.list_messages(None, 100).await.expect("list");
        assert!(
            listed.is_empty(),
            "a closing engine must not append a local publish (got {})",
            listed.len()
        );
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

    /// Regression: replay tracker rebuild on respawn must walk sealed
    /// segments BEFORE the tail so the tail's (more-recent) HLC wins
    /// when the same (author, device) lane is touched in both.
    /// `ChannelLogReplayTracker::record` overwrites unconditionally;
    /// reversing the order regresses the high-water mark and lets a
    /// re-broadcast of an already-persisted event slip through verify.
    ///
    /// Strategy:
    /// 1. Build engine with a tight seal threshold; append events with
    ///    strictly-monotone HLCs from a single (author, device) lane.
    /// 2. Force a seal partway through so some events are sealed and
    ///    some are still in tail. The tail's last HLC strictly exceeds
    ///    every sealed event's HLC.
    /// 3. Drop the engine; respawn it (same dir, same identity).
    /// 4. Take a packet that was originally appended (now in a sealed
    ///    segment OR in the persisted tail) and inject it into the new
    ///    engine's subscriber path. The verify chain must reject it as
    ///    `ChannelEventError::Replay` — i.e. the rebuilt tracker's
    ///    high-water mark must reflect the LATEST event from that lane,
    ///    not whichever happened to be visited last in the rebuild.
    #[tokio::test]
    async fn replay_tracker_survives_respawn_without_high_water_regression() {
        // Build first engine; small seal threshold so we get sealed segments.
        let fix = build_engine_fixture(3, 5_000, 10_000).await;
        let dir = fix.tmp.path().to_path_buf();
        let community_id = fix.community_id;
        let channel_id = fix.channel_id;
        let self_owner = fix.self_owner;
        let signing_key = Arc::clone(&fix.signing_key);
        let channel_key = Arc::clone(&fix.channel_key);

        // Append 7 events with strictly-monotone HLCs on a single
        // (author=self_owner, device="test-device") lane. Force a seal
        // after the 4th — leaves 4 events sealed (segment 0 — 3 events,
        // sealed because seal_threshold=3) and 3 events in tail.
        // The tail's last HLC (wall_ms=106) strictly exceeds every
        // sealed event's HLC (max wall_ms=103). The wrong rebuild
        // order (tail-then-segments) would leave the tracker pointing
        // at wall_ms=103 and accept wall_ms=104..106 as fresh.
        let mut events = Vec::new();
        for i in 0..7u64 {
            let hlc = Hlc {
                wall_ms: 100 + i,
                logical: 0,
                device_id: "test-device".to_string(),
            };
            let ev = make_signed_event(
                community_id,
                channel_id,
                self_owner,
                hlc,
                &format!("msg-{i}"),
                &signing_key,
            );
            events.push(ev);
        }
        {
            let mut log = fix.engine.log_for_test().lock().await;
            for (i, ev) in events.iter().enumerate() {
                log.append(ev.clone()).expect("append");
                if i == 3 {
                    log.seal_and_persist().expect("seal");
                }
            }
            assert!(
                !log.manifest.segments.is_empty(),
                "test setup expected ≥ 1 sealed segment after the partway seal",
            );
            assert!(
                !log.tail.is_empty(),
                "test setup expected ≥ 1 event left in tail post-seal",
            );
        }
        // Force tail.cbor to disk so the respawn sees it.
        fix.engine.flush_now().await.expect("flush_now");
        // Drop the original engine. shutdown joins the loops cleanly so
        // we can respawn against the same root_dir without racing.
        fix.engine.shutdown().await.expect("shutdown");

        // Re-build dependencies for the respawned engine. Same identity,
        // same dir, same channel_key — different mpsc endpoints since
        // the originals were owned by the dropped fixture.
        let state = Arc::new(AlwaysJoinedState {
            channel_id,
            owner: self_owner,
            enrolled_key: signing_key.verifying_key().to_bytes(),
        });
        let hlc_tracker: Arc<Mutex<BTreeMap<String, Hlc>>> = Arc::new(Mutex::new(BTreeMap::new()));
        let (publisher_tx, _publisher_rx) = mpsc::channel(64);
        let (subscriber_tx, subscriber_rx) = mpsc::channel(64);
        let (query_request_tx, _query_request_rx) = mpsc::channel(8);
        let (_rec_sink, sink) = recording_sink_pair();
        let config = ChannelLogEngineConfig {
            log_config: ChannelLogConfig {
                seal_threshold_events: 3,
            },
            flush_debounce_ms: 5_000,
            max_dirty_ms: 10_000,
            ..Default::default()
        };
        let params = ChannelLogEngineParams {
            community_id,
            channel_id,
            channel_key: Arc::clone(&channel_key),
            root_dir: dir,
            state_at_hlc: state,
            self_owner,
            self_device_id: "test-device".to_string(),
            signing_key: Arc::clone(&signing_key),
            hlc_tracker,
            sink,
            config,
            publisher_tx,
            subscriber_rx,
            query_request_tx,
        };
        let engine2 = ChannelLogEngine::new(params).await.expect("respawn");

        // Pick the LAST event (newest HLC, in the tail). Re-encrypt
        // and inject it via the subscriber path. The rebuilt tracker
        // must already reflect this HLC, so verify rejects with Replay.
        // This is the precise test the wrong order fails: with
        // tail-then-segments, the tail's record() runs first and is
        // overwritten by the older sealed events' record() calls,
        // dropping last_seen back to wall_ms=103. The injected
        // wall_ms=106 packet would then verify cleanly and append.
        let last_event = events.last().expect("events non-empty").clone();
        let packet = encrypt_channel_packet(&channel_key, &last_event).expect("encrypt");

        let log_count_before = engine2
            .list_messages(None, 100)
            .await
            .expect("list before")
            .len();
        subscriber_tx
            .send(packet)
            .await
            .expect("send injected packet");

        // Give the receive loop a brief window to process. If the
        // tracker correctly carries the wall_ms=106 high-water, the
        // packet is dropped (no log mutation). Otherwise the broken
        // path appends — which we'd see as a count increase.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let log_count_after = engine2
            .list_messages(None, 100)
            .await
            .expect("list after")
            .len();
        assert_eq!(
            log_count_before, log_count_after,
            "replay tracker rebuild regressed: re-broadcast of already-persisted \
             event was accepted (count: {} -> {})",
            log_count_before, log_count_after,
        );

        engine2.shutdown().await.expect("respawn shutdown");
    }

    // Smoke: skeleton tests still work end-to-end.
    #[tokio::test]
    async fn engine_construct_shutdown_round_trip() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        fix.engine.shutdown().await.expect("shutdown");
    }

    /// Shutdown and loop teardown must complete without advancing
    /// tokio's virtual clock. The receive + flush loops wake via
    /// `closing_notify`; a regression to `tokio::time::sleep`-based
    /// polling would auto-advance the paused clock when those
    /// sleeps fire. We also assert the receive loop actually
    /// exited (its `subscriber_rx` was dropped), catching parked-
    /// forever regressions the virtual-time check alone wouldn't
    /// see. Flush-loop termination has no test-side observable;
    /// the virtual-time check covers its sleep-regression class.
    ///
    /// Logical-time-based, so wall-clock jitter on shared CI
    /// runners is irrelevant (ZEB-282).
    #[tokio::test(start_paused = true)]
    async fn shutdown_completes_promptly() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        // Let the freshly-spawned receive + flush loops register on
        // `closing_notify`. `Notify::notify_waiters` only wakes
        // already-registered waiters (no permit stored for future
        // waiters); without this pre-yield, shutdown's notify call
        // would find no waiters and the loops would park. In
        // production, rt-multi-thread + non-zero wall-clock between
        // engine construction and shutdown makes this register-
        // before-notify ordering implicit.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        let v_start = tokio::time::Instant::now();
        fix.engine.shutdown().await.expect("shutdown");
        // Let the receive + flush loops actually exit. yield_now()
        // exhausts ready tasks; once everything is parked, paused-
        // time auto-advance fires for the next pending sleep wake
        // — only relevant under regression. 50 yields is comfortably
        // more than needed for two loops to break out of their
        // selects via closing_notify.
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        let v_elapsed = v_start.elapsed();
        assert!(
            v_elapsed < std::time::Duration::from_millis(10),
            "shutdown + loop teardown advanced virtual time by \
             {v_elapsed:?} — implies tokio::time::sleep-based wakeup \
             regression in the receive/flush loops"
        );
        // Observable receive-loop termination: the loop owns
        // `subscriber_rx`; once it breaks out of its select, the
        // rx is dropped and the fixture-side sender reports
        // closed. Catches a "loop parks forever" regression that
        // virtual-time alone wouldn't see.
        assert!(
            fix.subscriber_tx.is_closed(),
            "subscriber_tx should be closed after shutdown — implies \
             receive loop did not exit"
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
        assert!(
            req.outcome_tx.is_none(),
            "plain request_backfill must stay fire-and-forget (IPC path)"
        );
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
        assert!(
            req.outcome_tx.is_none(),
            "plain request_backfill must stay fire-and-forget (IPC path)"
        );
    }

    /// ZEB-418 P3a Task 3: `request_backfill_with_outcome` mirrors
    /// `request_backfill` but threads a oneshot reporting sender
    /// through the `BackfillQueryRequest` so the qr-driver can tell
    /// the caller when the reply stream closed (BackfillLatch needs
    /// full-page detection).
    #[tokio::test]
    async fn request_backfill_with_outcome_carries_oneshot() {
        let mut fix = build_engine_fixture(8, 250, 1000).await;

        let (tx, _rx) = tokio::sync::oneshot::channel::<BackfillPageReport>();
        Arc::clone(&fix.engine)
            .request_backfill_with_outcome(None, tx)
            .await
            .expect("backfill");

        let req = tokio::time::timeout(Duration::from_millis(500), fix.query_request_rx.recv())
            .await
            .expect("timeout")
            .expect("rx open");
        assert!(req.since.is_none());
        assert_eq!(req.limit, 0, "limit 0 = qr-driver applies engine default");
        assert!(
            req.outcome_tx.is_some(),
            "with_outcome variant must carry the oneshot through"
        );
    }

    #[tokio::test]
    async fn since_some_seals_vector_since_none_does_not() {
        let mut fix = build_engine_fixture(8, 250, 1000).await;
        // One event so the watermark vector is non-empty.
        {
            let mut log = fix.engine.log_for_test().lock().await;
            log.append(make_signed_event(
                fix.community_id,
                fix.channel_id,
                fix.self_owner,
                Hlc {
                    wall_ms: 500,
                    logical: 0,
                    device_id: "dev-x".to_string(),
                },
                "x1",
                &fix.signing_key,
            ))
            .expect("append");
        }
        let expected = fix.engine.log_watermark_vector().await;
        assert!(!expected.is_empty());

        // since=Some → a sealed vector is attached and opens to the engine's vector.
        let (tx, _rx) = tokio::sync::oneshot::channel::<BackfillPageReport>();
        Arc::clone(&fix.engine)
            .request_backfill_with_outcome(
                Some(Hlc {
                    wall_ms: 500,
                    logical: 0,
                    device_id: "dev-x".to_string(),
                }),
                tx,
            )
            .await
            .expect("backfill some");
        let req = tokio::time::timeout(Duration::from_millis(500), fix.query_request_rx.recv())
            .await
            .expect("timeout")
            .expect("rx open");
        let sealed = req
            .watermark_sealed
            .expect("since=Some must seal a watermark vector");
        let opened = crate::community_channel_log::open_watermark_vector(
            fix.engine.channel_key_ref(),
            &sealed,
        )
        .expect("open");
        assert_eq!(
            opened, expected,
            "sealed vector opens to the engine's current vector"
        );

        // since=None → no vector (full reconcile).
        let (tx2, _rx2) = tokio::sync::oneshot::channel::<BackfillPageReport>();
        Arc::clone(&fix.engine)
            .request_backfill_with_outcome(None, tx2)
            .await
            .expect("backfill none");
        let req2 = tokio::time::timeout(Duration::from_millis(500), fix.query_request_rx.recv())
            .await
            .expect("timeout")
            .expect("rx open");
        assert!(
            req2.watermark_sealed.is_none(),
            "since=None must NOT seal a vector (full reconcile)"
        );
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
        registry: Arc<ChannelLogRegistry>,
        state: Arc<AlwaysJoinedState>,
        hlc_tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
        membership_key: EpochKey,
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

        let (signing_key_raw, self_owner, _identity_pub_64) = fixture_identity(0x42);
        let signing_key = Arc::new(signing_key_raw);

        // The fixture's AlwaysJoinedState answers for one specific
        // channel_id; all registry tests use the same id so the stub
        // works for every spawned engine. We use a sentinel id here;
        // tests that need different ids would need a wider stub.
        let stub_channel_id = ChannelId([0xff; 16]);
        let state = Arc::new(AlwaysJoinedState {
            channel_id: stub_channel_id,
            owner: self_owner,
            enrolled_key: signing_key.verifying_key().to_bytes(),
        });

        let hlc_tracker: Arc<Mutex<BTreeMap<String, Hlc>>> = Arc::new(Mutex::new(BTreeMap::new()));

        let (_rec_sink, sink) = recording_sink_pair();

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
                    req.emit_backfill_progress,
                    req.backfill_progress_interval,
                    req.backfill_default_limit,
                    req.closing,
                    req.rbsr_hooks,
                );
                // JoinHandle dropped — adapter task is fire-and-forget;
                // closing flag (held by registry) signals shutdown.
            }
        });

        let config = ChannelLogRegistryConfig {
            adapter_request_tx,
            sink,
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
            transport_epoch_rx: None,
            // ZEB-599 Direction 1: no presence watch in this test harness.
            presence_resync_rx: None,
        };
        let registry = ChannelLogRegistry::new(config);

        RegistryFixture {
            registry,
            state,
            hlc_tracker,
            membership_key: EpochKey::new([0x55; 32]),
            self_owner,
            _tmp: tmp,
            _adapter_drainer,
        }
    }

    /// Helper — spawn a single channel under the fixture's deps. Returns
    /// the engine Arc so callers can chain assertions.
    ///
    /// Panics if called during an open transaction (i.e., if spawn
    /// returns `DeferredForCommit`) — tests exercising transactions
    /// should call `spawn()` directly and match the outcome themselves.
    async fn spawn_under_fixture(
        fix: &RegistryFixture,
        community_id: SpaceId,
        channel_id: ChannelId,
    ) -> Arc<ChannelLogEngine> {
        let key = derive_channel_key(&fix.membership_key, &community_id, &channel_id);
        match Arc::clone(&fix.registry)
            .spawn(
                community_id,
                channel_id,
                key,
                Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                Arc::clone(&fix.hlc_tracker),
            )
            .await
            .expect("spawn")
        {
            SpawnOutcome::Spawned(engine) => engine,
            SpawnOutcome::DeferredForCommit => {
                panic!(
                    "spawn_under_fixture used during a transaction; tests \
                     that exercise transactions should call spawn() directly"
                )
            }
        }
    }

    /// Suppress unused-field lint for `self_owner` — the struct is shared
    /// across tests and not every test reads every field.
    #[allow(dead_code)]
    fn _registry_fixture_field_use(fix: &RegistryFixture) -> OwnerAddr {
        fix.self_owner
    }

    // ── ZEB-418 P3a Task 4: registry-spawn backfill driver ───────────

    /// Fixture variant for backfill-driver tests: same registry shape
    /// as `build_registry_fixture`, but the test holds the adapter-
    /// bridge receiver itself (no Zenoh session, no drainer) so it can
    /// intercept each spawned channel's `query_request_rx` and observe
    /// the backfill driver's wire-side requests directly. No Zenoh
    /// means the default `current_thread` test flavor works here.
    struct BackfillRegistryFixture {
        registry: Arc<ChannelLogRegistry>,
        state: Arc<AlwaysJoinedState>,
        hlc_tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
        membership_key: EpochKey,
        adapter_request_rx: mpsc::UnboundedReceiver<crate::event_loop::ChannelLogAdapterRequest>,
        _tmp: TempDir,
    }

    async fn build_backfill_registry_fixture() -> BackfillRegistryFixture {
        let tmp = TempDir::new().expect("tempdir");

        let (signing_key_raw, self_owner, _identity_pub_64) = fixture_identity(0x42);
        let signing_key = Arc::new(signing_key_raw);

        let stub_channel_id = ChannelId([0xff; 16]);
        let state = Arc::new(AlwaysJoinedState {
            channel_id: stub_channel_id,
            owner: self_owner,
            enrolled_key: signing_key.verifying_key().to_bytes(),
        });

        let hlc_tracker: Arc<Mutex<BTreeMap<String, Hlc>>> = Arc::new(Mutex::new(BTreeMap::new()));

        let (_rec_sink, sink) = recording_sink_pair();

        let (adapter_request_tx, adapter_request_rx) =
            mpsc::unbounded_channel::<crate::event_loop::ChannelLogAdapterRequest>();

        let config = ChannelLogRegistryConfig {
            adapter_request_tx,
            sink,
            identity_dir: tmp.path().to_path_buf(),
            self_owner,
            self_device_id: "backfill-test-device".to_string(),
            signing_key,
            engine_config: ChannelLogEngineConfig {
                log_config: ChannelLogConfig {
                    seal_threshold_events: 8,
                },
                ..Default::default()
            },
            transport_epoch_rx: None,
            // ZEB-599 Direction 1: no presence watch in this test harness.
            presence_resync_rx: None,
        };
        let registry = ChannelLogRegistry::new(config);

        BackfillRegistryFixture {
            registry,
            state,
            hlc_tracker,
            membership_key: EpochKey::new([0x55; 32]),
            adapter_request_rx,
            _tmp: tmp,
        }
    }

    /// `spawn_under_fixture` twin for the backfill fixture.
    async fn spawn_under_backfill_fixture(
        fix: &BackfillRegistryFixture,
        community_id: SpaceId,
        channel_id: ChannelId,
    ) -> Arc<ChannelLogEngine> {
        let key = derive_channel_key(&fix.membership_key, &community_id, &channel_id);
        match Arc::clone(&fix.registry)
            .spawn(
                community_id,
                channel_id,
                key,
                Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                Arc::clone(&fix.hlc_tracker),
            )
            .await
            .expect("spawn")
        {
            SpawnOutcome::Spawned(engine) => engine,
            SpawnOutcome::DeferredForCommit => {
                panic!("no transaction open in backfill fixture tests")
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn registry_spawn_fires_backfill_request_fresh_log_since_none() {
        let mut fix = build_backfill_registry_fixture().await;
        let cid = SpaceId([0xc1; 16]);
        let chid = ChannelId([0xff; 16]);

        // Spawn alone must start the backfill driver — no manual
        // request_backfill call anywhere in this test.
        let _engine = spawn_under_backfill_fixture(&fix, cid, chid).await;

        let adapter_req =
            tokio::time::timeout(Duration::from_secs(30), fix.adapter_request_rx.recv())
                .await
                .expect("adapter request within timeout")
                .expect("adapter bridge open");
        let mut query_rx = adapter_req.query_request_rx;
        let query = tokio::time::timeout(Duration::from_secs(30), query_rx.recv())
            .await
            .expect("backfill query within timeout")
            .expect("query channel open");
        assert_eq!(
            query.since, None,
            "fresh joiner's empty log → watermark None → full history"
        );
        assert!(
            query.outcome_tx.is_some(),
            "driver requests must thread an outcome oneshot"
        );

        fix.registry.stop(&cid, &chid).await.expect("stop");
    }

    #[tokio::test(start_paused = true)]
    async fn registry_respawn_backfill_request_since_log_watermark() {
        let mut fix = build_backfill_registry_fixture().await;
        let cid = SpaceId([0xc1; 16]);
        let chid = ChannelId([0xff; 16]);

        let engine = spawn_under_backfill_fixture(&fix, cid, chid).await;
        // Drain spawn #1's adapter request (fresh-log driver).
        let _req1 = tokio::time::timeout(Duration::from_secs(30), fix.adapter_request_rx.recv())
            .await
            .expect("adapter request within timeout")
            .expect("adapter bridge open");

        // Write one post so the log has a watermark, then stop (which
        // flushes the tail durably) and respawn — the reconnect path.
        engine
            .publish(b"hello".to_vec(), None, None, None)
            .await
            .expect("publish");
        let expected = engine.log_max_hlc().await;
        assert!(expected.is_some(), "publish must set a log watermark");
        fix.registry.stop(&cid, &chid).await.expect("stop");

        let _engine2 = spawn_under_backfill_fixture(&fix, cid, chid).await;
        let adapter_req2 =
            tokio::time::timeout(Duration::from_secs(30), fix.adapter_request_rx.recv())
                .await
                .expect("adapter request within timeout")
                .expect("adapter bridge open");
        let mut query_rx = adapter_req2.query_request_rx;
        let query = tokio::time::timeout(Duration::from_secs(30), query_rx.recv())
            .await
            .expect("backfill query within timeout")
            .expect("query channel open");
        assert_eq!(
            query.since, expected,
            "respawned engine must catch up from the persisted log watermark"
        );

        fix.registry.stop(&cid, &chid).await.expect("stop");
    }

    /// ZEB-418 P3a Task 5 (spec D23 trigger 3): a channel CREATED
    /// MID-SESSION — e.g. a remote member adds #random while this
    /// device is subscribed — must get a local engine AND a backfill.
    ///
    /// Production funnel (verified — no new production code needed):
    ///   1. the remote `ChannelCreate` materializes via
    ///      `handle_incoming_publish`, which emits one
    ///      `CommunityMembershipDelta` per Inserted event
    ///      (community_state_sync.rs Phase C-pre);
    ///   2. `run_community_delta_consumer` projects it through
    ///      `delta_to_channel_config_change` → action `Created`;
    ///   3. the consumer's registry hook (lib.rs, ZEB-270 Phase 3
    ///      Task 4.5) hex-decodes the payload ids, derives the
    ///      channel key from the community engine's membership key
    ///      and calls `ChannelLogRegistry::spawn`;
    ///   4. BOTH spawn routes (fast path + transaction-deferred
    ///      commit drain) funnel through `spawn_inner_now`, which
    ///      starts the Task-4 backfill driver unconditionally.
    ///
    /// This test replays steps 2–4 exactly (same projection fn, same
    /// id-decode + key-derivation recipe as the production hook)
    /// against a registry that already has a boot-time channel
    /// running, and asserts the NEW channel's adapter sees a
    /// `BackfillQueryRequest { since: None, outcome_tx: Some }`.
    #[tokio::test(start_paused = true)]
    async fn mid_session_new_channel_created_spawn_fires_backfill_since_none() {
        let mut fix = build_backfill_registry_fixture().await;
        let cid = SpaceId([0xc1; 16]);
        let boot_chid = ChannelId([0xff; 16]);

        // Mid-session precondition: a boot-time channel (#general
        // analog) is already live. Drain its adapter request so the
        // next recv() observes the NEW channel only.
        let _boot = spawn_under_backfill_fixture(&fix, cid, boot_chid).await;
        let boot_req = tokio::time::timeout(Duration::from_secs(30), fix.adapter_request_rx.recv())
            .await
            .expect("boot adapter request within timeout")
            .expect("adapter bridge open");
        assert_eq!(boot_req.channel_id_hex, hex::encode(boot_chid.0));

        // Step 1 analog: a remote member creates #random — the CRDT
        // apply path emits this delta on Inserted.
        let new_chid = ChannelId([0xee; 16]);
        let create_event = crate::community_membership::SignedMembershipEvent {
            signer_certs: Vec::new(),
            id: [0x0e; 16],
            community_id: cid,
            kind: crate::community_membership::MembershipEventKind::ChannelCreate {
                channel_id: new_chid,
                name: "random".into(),
                write_power: 0,
                kind: crate::community_membership::ChannelKind::Text,
            },
            // Remote member, NOT self — mid-session discovery must not
            // depend on the local device having authored the event.
            actor: OwnerAddr([0x99; 16]),
            at: Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "remote-device".into(),
            },
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        };

        // Step 2: the delta consumer's projection.
        let payload = crate::delta_to_channel_config_change(
            &crate::community_state_sync::CommunityMembershipDelta {
                community_id: cid,
                event: create_event,
            },
        )
        .expect("ChannelCreate must project to a channel-config change");
        assert_eq!(payload.action, crate::ChannelConfigChangeAction::Created);

        // Step 3: the registry hook's recipe — decode ids from the
        // payload, derive the channel key, spawn.
        let cid_bytes: [u8; 16] = hex::decode(&payload.community_id)
            .expect("community_id hex")
            .try_into()
            .expect("16 bytes");
        let chid_bytes: [u8; 16] = hex::decode(&payload.channel_id)
            .expect("channel_id hex")
            .try_into()
            .expect("16 bytes");
        let hook_cid = SpaceId(cid_bytes);
        let hook_chid = ChannelId(chid_bytes);
        let key = derive_channel_key(&fix.membership_key, &hook_cid, &hook_chid);
        let outcome = Arc::clone(&fix.registry)
            .spawn(
                hook_cid,
                hook_chid,
                key,
                Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                Arc::clone(&fix.hlc_tracker),
            )
            .await
            .expect("mid-session spawn");
        assert!(
            matches!(outcome, SpawnOutcome::Spawned(_)),
            "no transaction open mid-session → fast path must return Spawned"
        );

        // Step 4: the NEW channel's adapter must see the Task-4
        // backfill driver's first request — fresh log → full history.
        let adapter_req =
            tokio::time::timeout(Duration::from_secs(30), fix.adapter_request_rx.recv())
                .await
                .expect("new-channel adapter request within timeout")
                .expect("adapter bridge open");
        assert_eq!(
            adapter_req.channel_id_hex,
            hex::encode(new_chid.0),
            "adapter request must target the mid-session-created channel"
        );
        let mut query_rx = adapter_req.query_request_rx;
        let query = tokio::time::timeout(Duration::from_secs(30), query_rx.recv())
            .await
            .expect("backfill query within timeout")
            .expect("query channel open");
        assert_eq!(
            query.since, None,
            "brand-new channel has an empty log → watermark None → full history"
        );
        assert!(
            query.outcome_tx.is_some(),
            "driver requests must thread an outcome oneshot"
        );

        fix.registry
            .stop(&cid, &boot_chid)
            .await
            .expect("stop boot channel");
        fix.registry
            .stop(&hook_cid, &hook_chid)
            .await
            .expect("stop new channel");
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
                kind: crate::community_membership::ChannelKind::Text,
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
                kind: crate::community_membership::ChannelKind::Text,
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
                kind: crate::community_membership::ChannelKind::Text,
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

    /// Concurrency regression: a `spawn` racing a `stop` for the same
    /// `(cid, chid)` must not orphan the adapter task (i.e., must not
    /// leave the closing flag unreachable from `stop()`). Pre-fix, the
    /// registry stored the engine and closing flag in two separate
    /// `Mutex<HashMap>`s; a stop interleaved between the two inserts
    /// would remove the engine, fail to find a closing flag, and the
    /// subsequent insert of the orphan closing flag would never be
    /// flipped — leaking the adapter for the registry's lifetime.
    /// With the atomic `EngineEntry` insert, the registry's terminal
    /// state after each iteration is consistent: either the entry
    /// exists (next stop drains it) or it doesn't.
    ///
    /// We don't assert on the adapter task itself (the bridge drainer
    /// runs in the fixture, the JoinHandle is dropped fire-and-forget);
    /// we assert on the registry's observable state, which is the
    /// invariant the bug violated. A `stop` after the race must always
    /// converge on `engine() == None`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn registry_spawn_stop_race_does_not_orphan() {
        let fix = build_registry_fixture().await;
        let cid = SpaceId([0xc1; 16]);
        let chid = ChannelId([0xff; 16]);
        let key = derive_channel_key(&fix.membership_key, &cid, &chid);

        for _ in 0..50 {
            let r1 = Arc::clone(&fix.registry);
            let r2 = Arc::clone(&fix.registry);
            let state = Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>;
            let tracker = Arc::clone(&fix.hlc_tracker);
            let key_for_spawn = key.clone();

            let handle1 = tokio::spawn(async move {
                let _ = r1.spawn(cid, chid, key_for_spawn, state, tracker).await;
            });
            let handle2 = tokio::spawn(async move {
                let _ = r2.stop(&cid, &chid).await;
            });
            let _ = tokio::join!(handle1, handle2);

            // Cleanup: stop again to drain whichever ordering won. Pre-fix,
            // an orphaned closing flag would be leaked here (the engine
            // map would be empty, but the closings map would contain a
            // flag no caller can reach — and the in-flight adapter task
            // would never observe its closing signal).
            fix.registry
                .stop(&cid, &chid)
                .await
                .expect("post-race stop");

            // Final state per iteration: registry must be clean.
            assert!(
                fix.registry.engine(&cid, &chid).await.is_none(),
                "engine must be drained after race + cleanup stop",
            );
        }
    }

    /// Resilience regression: a `reconcile_from_state` whose first
    /// channel-spawn fails must NOT abort iteration over the remaining
    /// channels. Pre-fix, the loop propagated the first error via `?`,
    /// leaving every channel later in the (non-deterministic) HashMap
    /// iteration order without an engine for the entire session — silent,
    /// intermittent, hard to reproduce.
    ///
    /// Setup: pre-create a regular file at one channel's directory path
    /// so `spawn`'s `create_dir_all` returns `ErrorKind::NotADirectory`
    /// (or platform equivalent) and the spawn errors. Reconcile must
    /// then still spawn the OTHER channel and return the captured error.
    ///
    /// The fixture's stub `AlwaysJoinedState` only answers for one
    /// specific channel id, but reconcile's failure path is in `spawn`
    /// (dir-create) which runs BEFORE state is consulted, and the
    /// successful path uses the stub's recognized channel — so this
    /// test exercises both arms cleanly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn registry_reconcile_continues_past_spawn_failure() {
        let fix = build_registry_fixture().await;
        let cid = SpaceId([0xc1; 16]);
        // Stub recognizes [0xff; 16] — this is the channel that should
        // succeed and have an engine after reconcile.
        let good_chid = ChannelId([0xff; 16]);
        // The bad channel uses a distinct id; we sabotage its dir path.
        let bad_chid = ChannelId([0xee; 16]);

        // Sabotage: pre-create a regular file at the bad channel's
        // directory path so create_dir_all in spawn() will fail.
        let identity_dir = fix._tmp.path();
        let bad_channel_dir = identity_dir
            .join("communities")
            .join(hex::encode(cid.0))
            .join("channels")
            .join(hex::encode(bad_chid.0));
        std::fs::create_dir_all(bad_channel_dir.parent().expect("parent")).expect("parent dirs");
        std::fs::write(&bad_channel_dir, b"sabotage").expect("write sabotage file");

        let mut materialized = MaterializedMembership::default();
        let info = |name: &str| crate::community_membership::ChannelInfo {
            name: name.to_string(),
            write_power: 0,
            kind: crate::community_membership::ChannelKind::Text,
            created_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "x".to_string(),
            },
            deleted_at: None,
        };
        materialized.channels.insert(good_chid, info("good"));
        materialized.channels.insert(bad_chid, info("bad"));

        let result = Arc::clone(&fix.registry)
            .reconcile_from_state(
                cid,
                &materialized,
                &fix.membership_key,
                Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                Arc::clone(&fix.hlc_tracker),
            )
            .await;

        // Reconcile must surface the bad-channel error...
        assert!(
            result.is_err(),
            "reconcile_from_state must surface the spawn error from the sabotaged channel",
        );

        // ...but the GOOD channel must still have an engine spawned.
        // This is the regression target: pre-fix, HashMap iteration
        // order would non-deterministically determine whether the good
        // channel got an engine, depending on whether it iterated
        // before or after the bad one.
        assert!(
            fix.registry.engine(&cid, &good_chid).await.is_some(),
            "good channel must spawn even when the bad channel's spawn fails",
        );
        assert!(
            fix.registry.engine(&cid, &bad_chid).await.is_none(),
            "bad channel must NOT have an engine (its spawn failed)",
        );

        fix.registry
            .stop(&cid, &good_chid)
            .await
            .expect("cleanup stop");
    }

    // ZEB-271: transaction-protocol tests. These verify that
    // begin_transaction → spawn → commit fires the deferred spawn,
    // begin_transaction → spawn → abort drops it, and corner cases
    // (drop safety net, stale tx_id, reentrancy, ordering, partial
    // failure) all converge on the documented behavior. See spec §7.1.

    /// Bounded condition-wait: polls until the registry has no pending
    /// transaction for `community_id`, or `timeout` elapses.
    /// Returns `true` if the condition was met before the deadline.
    async fn wait_until_no_pending_tx(
        registry: &ChannelLogRegistry,
        community_id: &SpaceId,
        timeout: std::time::Duration,
    ) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if !registry.has_pending_transaction_for_test(community_id) {
                return true;
            }
            tokio::task::yield_now().await;
        }
        false
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tx_begin_commit_drains_queued_spawn() {
        let fix = build_registry_fixture().await;
        let community_id = SpaceId([0xc1; 16]);
        let channel_id = ChannelId([0xa1; 16]);

        let tx = Arc::clone(&fix.registry).begin_transaction(community_id);

        let key = derive_channel_key(&fix.membership_key, &community_id, &channel_id);
        let outcome = Arc::clone(&fix.registry)
            .spawn(
                community_id,
                channel_id,
                key,
                Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                Arc::clone(&fix.hlc_tracker),
            )
            .await
            .expect("spawn");

        assert!(
            matches!(outcome, SpawnOutcome::DeferredForCommit),
            "spawn during open transaction must return DeferredForCommit"
        );

        // Pre-commit: engine must NOT be in the registry yet.
        assert!(
            fix.registry
                .engine(&community_id, &channel_id)
                .await
                .is_none(),
            "deferred spawn should not be visible in engines map before commit"
        );

        tx.commit().await.expect("commit");

        // Post-commit: engine must be in the registry.
        assert!(
            fix.registry
                .engine(&community_id, &channel_id)
                .await
                .is_some(),
            "deferred spawn must be visible after commit drains the queue"
        );

        fix.registry
            .stop(&community_id, &channel_id)
            .await
            .expect("stop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tx_begin_abort_drops_queued_spawn() {
        let fix = build_registry_fixture().await;
        let community_id = SpaceId([0xc2; 16]);
        let channel_id = ChannelId([0xa2; 16]);

        let tx = Arc::clone(&fix.registry).begin_transaction(community_id);

        let key = derive_channel_key(&fix.membership_key, &community_id, &channel_id);
        Arc::clone(&fix.registry)
            .spawn(
                community_id,
                channel_id,
                key,
                Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                Arc::clone(&fix.hlc_tracker),
            )
            .await
            .expect("spawn");

        tx.abort();

        assert!(
            fix.registry
                .engine(&community_id, &channel_id)
                .await
                .is_none(),
            "aborted transaction must not spawn the queued engine"
        );

        // No on-disk dir for the channel either (the registry's spawn
        // body never ran, so no fs::create_dir_all happened).
        let channel_dir = fix
            ._tmp
            .path()
            .join("communities")
            .join(hex::encode(community_id.0))
            .join("channels")
            .join(hex::encode(channel_id.0));
        assert!(
            !channel_dir.exists(),
            "aborted transaction must not create the channel-log on-disk dir"
        );
    }

    /// CodeRabbit Major round 3: stop() must clear queued deferred
    /// spawns for the same channel, otherwise commit() resurrects a
    /// channel that the materialized state has already deleted.
    ///
    /// Scenario: open transaction; spawn channel A → DeferredForCommit
    /// (queued); stop channel A (no live engine yet); commit drains the
    /// queue. Without the fix, commit would spawn A. With the fix, the
    /// queue is empty after stop and commit is a no-op.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tx_stop_cancels_queued_deferred_spawn() {
        let fix = build_registry_fixture().await;
        let community_id = SpaceId([0xc4; 16]);
        let channel_id = ChannelId([0xa4; 16]);

        let tx = Arc::clone(&fix.registry).begin_transaction(community_id);

        let key = derive_channel_key(&fix.membership_key, &community_id, &channel_id);
        let outcome = Arc::clone(&fix.registry)
            .spawn(
                community_id,
                channel_id,
                key,
                Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                Arc::clone(&fix.hlc_tracker),
            )
            .await
            .expect("spawn");
        assert!(
            matches!(outcome, SpawnOutcome::DeferredForCommit),
            "spawn inside open tx must return DeferredForCommit"
        );

        // Simulate the consumer observing a Delete for the same channel
        // before commit. stop() must drop the queued deferred spawn.
        fix.registry
            .stop(&community_id, &channel_id)
            .await
            .expect("stop");

        // commit() now drains an empty queue (because stop scrubbed it)
        // and must NOT resurrect the channel.
        tx.commit().await.expect("commit");

        assert!(
            fix.registry
                .engine(&community_id, &channel_id)
                .await
                .is_none(),
            "stop() before commit() must cancel the queued deferred spawn — \
             commit must NOT resurrect a channel that has been deleted"
        );

        // No on-disk dir either — the spawn body never ran.
        let channel_dir = fix
            ._tmp
            .path()
            .join("communities")
            .join(hex::encode(community_id.0))
            .join("channels")
            .join(hex::encode(channel_id.0));
        assert!(
            !channel_dir.exists(),
            "stop-before-commit must not create the channel-log on-disk dir"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tx_dropped_guard_safety_net_aborts() {
        let fix = build_registry_fixture().await;
        let community_id = SpaceId([0xc3; 16]);
        let channel_id = ChannelId([0xa3; 16]);

        {
            let _tx = Arc::clone(&fix.registry).begin_transaction(community_id);
            let key = derive_channel_key(&fix.membership_key, &community_id, &channel_id);
            Arc::clone(&fix.registry)
                .spawn(
                    community_id,
                    channel_id,
                    key,
                    Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                    Arc::clone(&fix.hlc_tracker),
                )
                .await
                .expect("spawn");
            // _tx drops here without explicit commit/abort.
        }

        // Drop spawned a tokio task to call abort_transaction_internal.
        // Poll until the pending-tx map entry is gone (max 500ms) to
        // avoid flakes without a fixed sleep.
        //
        // Greptile round 2 P2: `wait_until_no_pending_tx` uses
        // `yield_now()` which only yields to the Tokio scheduler. Under
        // `worker_threads = 1` or extreme load this could in principle
        // re-enter before the spawned cleanup task runs. We accept that
        // theoretical risk because (a) this test pins
        // `worker_threads = 2`, so the cleanup task always has a free
        // worker, and (b) the alternative — capturing a `JoinHandle`
        // from inside `Drop` — would force the guard to expose the
        // cleanup task externally just for tests. The no-runtime sibling
        // test (`tx_dropped_guard_no_runtime_falls_back_to_sync_abort`)
        // covers the race-free synchronous abort path.
        assert!(
            wait_until_no_pending_tx(
                &fix.registry,
                &community_id,
                std::time::Duration::from_millis(500)
            )
            .await,
            "dropped transaction guard must trigger safety-net abort"
        );

        assert!(
            fix.registry
                .engine(&community_id, &channel_id)
                .await
                .is_none(),
            "dropped transaction guard must trigger safety-net abort"
        );
    }

    /// Drop safety-net — no-runtime fallback (CodeAnt Major fix).
    ///
    /// Verify that `CommunityTransactionGuard::drop` takes the sync
    /// `abort_transaction_internal` path (not `tokio::spawn`) when there
    /// is no active Tokio runtime in the drop thread. A bare
    /// `std::thread::spawn` closure has no runtime association, so
    /// `Handle::try_current()` returns `Err` and the guard's drop code
    /// must fall back to the synchronous abort path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tx_dropped_guard_no_runtime_falls_back_to_sync_abort() {
        let fix = build_registry_fixture().await;
        let community_id = SpaceId([0xcd; 16]);
        let channel_id = ChannelId([0xad; 16]);

        // Open a transaction and queue a deferred spawn so there is an
        // entry in pending_transactions to be cleaned up by the drop.
        let tx = Arc::clone(&fix.registry).begin_transaction(community_id);
        let key = derive_channel_key(&fix.membership_key, &community_id, &channel_id);
        Arc::clone(&fix.registry)
            .spawn(
                community_id,
                channel_id,
                key,
                Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                Arc::clone(&fix.hlc_tracker),
            )
            .await
            .expect("spawn");

        // Move the guard into a bare OS thread. std::thread::spawn
        // closures have no Tokio runtime association, so
        // Handle::try_current() returns Err and the drop code takes the
        // synchronous abort path (not tokio::spawn, which would panic).
        let registry_clone = Arc::clone(&fix.registry);
        std::thread::spawn(move || {
            // Verify no runtime is reachable from this thread.
            assert!(
                tokio::runtime::Handle::try_current().is_err(),
                "bare std::thread must not have a runtime handle"
            );
            // Drop the guard — exercises the sync fallback.
            drop(tx);
            // The sync path cleans up immediately (no yield needed).
            assert!(
                !registry_clone.has_pending_transaction_for_test(&community_id),
                "sync abort path must clean up pending_transactions immediately"
            );
        })
        .join()
        .expect("thread join");

        // Double-check from the async context.
        assert!(
            !fix.registry.has_pending_transaction_for_test(&community_id),
            "no pending transaction entry after no-runtime sync abort"
        );
        // The queue was discarded — no engine was spawned.
        assert!(
            fix.registry
                .engine(&community_id, &channel_id)
                .await
                .is_none(),
            "no-runtime sync abort must discard the deferred spawn"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tx_spawn_outside_transaction_immediate() {
        let fix = build_registry_fixture().await;
        let community_id = SpaceId([0xc4; 16]);
        let channel_id = ChannelId([0xa4; 16]);

        let key = derive_channel_key(&fix.membership_key, &community_id, &channel_id);
        let outcome = Arc::clone(&fix.registry)
            .spawn(
                community_id,
                channel_id,
                key,
                Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                Arc::clone(&fix.hlc_tracker),
            )
            .await
            .expect("spawn");

        match outcome {
            SpawnOutcome::Spawned(engine) => {
                assert_eq!(engine.community_id(), community_id);
                assert_eq!(engine.channel_id(), channel_id);
            }
            SpawnOutcome::DeferredForCommit => {
                panic!("spawn outside a transaction must return Spawned, not Deferred");
            }
        }

        fix.registry
            .stop(&community_id, &channel_id)
            .await
            .expect("stop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tx_stale_guard_commit_no_ops() {
        let fix = build_registry_fixture().await;
        let community_id = SpaceId([0xc5; 16]);
        let channel_a = ChannelId([0xa5; 16]);
        let channel_b = ChannelId([0xb5; 16]);

        // Open tx_A.
        let tx_a = Arc::clone(&fix.registry).begin_transaction(community_id);

        // Spawn a channel under tx_A's queue.
        let key_a = derive_channel_key(&fix.membership_key, &community_id, &channel_a);
        Arc::clone(&fix.registry)
            .spawn(
                community_id,
                channel_a,
                key_a,
                Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                Arc::clone(&fix.hlc_tracker),
            )
            .await
            .expect("spawn a");

        // Open tx_B for the same community_id (overwrites tx_A's entry).
        let tx_b = Arc::clone(&fix.registry).begin_transaction(community_id);

        // Spawn a different channel under tx_B's queue.
        let key_b = derive_channel_key(&fix.membership_key, &community_id, &channel_b);
        Arc::clone(&fix.registry)
            .spawn(
                community_id,
                channel_b,
                key_b,
                Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                Arc::clone(&fix.hlc_tracker),
            )
            .await
            .expect("spawn b");

        // Commit tx_A — should be a no-op (tx_id mismatch).
        tx_a.commit().await.expect("stale commit no-ops");

        // Channel a was queued in tx_A's overwritten entry — it should
        // NOT be in the registry (tx_A's queue was dropped on overwrite).
        assert!(
            fix.registry
                .engine(&community_id, &channel_a)
                .await
                .is_none(),
            "stale tx_A.commit must not resurrect tx_A's overwritten queue"
        );

        // tx_B's queue is intact — channel b can still be committed.
        tx_b.commit().await.expect("commit b");

        assert!(
            fix.registry
                .engine(&community_id, &channel_b)
                .await
                .is_some(),
            "tx_B.commit must drain tx_B's queue"
        );

        fix.registry
            .stop(&community_id, &channel_b)
            .await
            .expect("stop b");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tx_reentrant_begin_transaction_overwrites() {
        let fix = build_registry_fixture().await;
        let community_id = SpaceId([0xc6; 16]);

        let tx_a = Arc::clone(&fix.registry).begin_transaction(community_id);
        let tx_id_a = tx_a.tx_id_for_test();

        let tx_b = Arc::clone(&fix.registry).begin_transaction(community_id);
        let tx_id_b = tx_b.tx_id_for_test();

        assert_ne!(
            tx_id_a, tx_id_b,
            "reentrant begin_transaction must mint a fresh tx_id"
        );

        // Drop both without explicit cleanup (safety net handles either).
        drop(tx_a);
        drop(tx_b);

        // Poll until the pending-tx map entry is gone (max 500ms).
        assert!(
            wait_until_no_pending_tx(
                &fix.registry,
                &community_id,
                std::time::Duration::from_millis(500)
            )
            .await,
            "after both drops, no pending transaction entry remains"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tx_multiple_deferred_spawns_drain_in_order() {
        let fix = build_registry_fixture().await;
        let community_id = SpaceId([0xc7; 16]);
        let channels: [ChannelId; 3] = [
            ChannelId([0x01; 16]),
            ChannelId([0x02; 16]),
            ChannelId([0x03; 16]),
        ];

        let tx = Arc::clone(&fix.registry).begin_transaction(community_id);

        for ch in channels.iter() {
            let key = derive_channel_key(&fix.membership_key, &community_id, ch);
            Arc::clone(&fix.registry)
                .spawn(
                    community_id,
                    *ch,
                    key,
                    Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                    Arc::clone(&fix.hlc_tracker),
                )
                .await
                .expect("spawn");
        }

        tx.commit().await.expect("commit");

        for ch in channels.iter() {
            assert!(
                fix.registry.engine(&community_id, ch).await.is_some(),
                "channel {:?} must be present after commit",
                ch
            );
            fix.registry.stop(&community_id, ch).await.expect("stop");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tx_commit_partial_failure_continues() {
        // Failure injection: pre-create the second channel's dir as a
        // FILE (not a directory) so the inner spawn's
        // `std::fs::create_dir_all` fails on it. The first and third
        // channels' dirs are untouched, so their spawns succeed.
        let fix = build_registry_fixture().await;
        let community_id = SpaceId([0xc8; 16]);
        let channels: [ChannelId; 3] = [
            ChannelId([0x11; 16]),
            ChannelId([0x22; 16]),
            ChannelId([0x33; 16]),
        ];

        // Sabotage channel 2's path — create a file at the path that
        // create_dir_all would want to be a directory.
        let bad_dir = fix
            ._tmp
            .path()
            .join("communities")
            .join(hex::encode(community_id.0))
            .join("channels");
        std::fs::create_dir_all(&bad_dir).unwrap();
        let bad_path = bad_dir.join(hex::encode(channels[1].0));
        std::fs::write(&bad_path, b"sabotage").unwrap();

        let tx = Arc::clone(&fix.registry).begin_transaction(community_id);
        for ch in channels.iter() {
            let key = derive_channel_key(&fix.membership_key, &community_id, ch);
            Arc::clone(&fix.registry)
                .spawn(
                    community_id,
                    *ch,
                    key,
                    Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                    Arc::clone(&fix.hlc_tracker),
                )
                .await
                .expect("spawn (queueing always succeeds during a tx)");
        }

        let result = tx.commit().await;
        assert!(
            result.is_err(),
            "commit must surface the first error from the partial-failure drain"
        );

        // First channel spawned successfully; third should also have
        // been attempted and succeeded.
        assert!(
            fix.registry
                .engine(&community_id, &channels[0])
                .await
                .is_some(),
            "first channel must spawn before the second's failure"
        );
        assert!(
            fix.registry
                .engine(&community_id, &channels[2])
                .await
                .is_some(),
            "third channel must still be attempted after the second's failure"
        );
        assert!(
            fix.registry
                .engine(&community_id, &channels[1])
                .await
                .is_none(),
            "the second (sabotaged) channel must not be present"
        );

        fix.registry.stop(&community_id, &channels[0]).await.ok();
        fix.registry.stop(&community_id, &channels[2]).await.ok();
    }

    // ── ZEB-536 Task 4: react() + list_message_dtos ───────────────────

    /// TDD RED → GREEN: react updates the reaction index and the index
    /// is visible via `list_message_dtos`. Un-react converges to empty.
    #[tokio::test]
    async fn react_updates_index_and_lists_in_dto() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        let msg_id = Arc::clone(&fix.engine)
            .publish(b"hi".to_vec(), None, None, None)
            .await
            .expect("post");
        Arc::clone(&fix.engine)
            .react(msg_id, "👍".to_string(), true, None)
            .await
            .expect("react");
        let dtos = fix
            .engine
            .list_message_dtos(None, 100)
            .await
            .expect("list dtos");
        let m = dtos
            .iter()
            .find(|d| d.message_id == hex::encode(msg_id.0))
            .unwrap();
        assert_eq!(
            m.reactions.iter().find(|r| r.emoji == "👍").unwrap().count,
            1
        );
        assert!(m.reactions.iter().find(|r| r.emoji == "👍").unwrap().mine);

        // un-react converges to empty
        Arc::clone(&fix.engine)
            .react(msg_id, "👍".to_string(), false, None)
            .await
            .expect("unreact");
        let dtos2 = fix
            .engine
            .list_message_dtos(None, 100)
            .await
            .expect("list dtos");
        let m2 = dtos2
            .iter()
            .find(|d| d.message_id == hex::encode(msg_id.0))
            .unwrap();
        assert!(m2.reactions.iter().all(|r| r.emoji != "👍"));
    }

    /// ZEB-541: the live `channel-reaction-received` event carries the custom
    /// emoji CID + size + encrypted flag so a peer can render the chip
    /// immediately (instead of staying blank until a `list_channel_messages`
    /// re-materialization) AND gate the "name this emoji" affordance correctly
    /// on the live chip (naming is public-only). A custom-emoji React emits
    /// `emojiCid`/`emojiSize`/`encrypted`; a unicode React emits none of them
    /// (the optional fields are skipped on serialize). `encrypted` is derived
    /// the same way `reactions_for` does, so a LIVE chip matches a reseed.
    #[tokio::test]
    async fn reaction_received_event_carries_custom_emoji_cid() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        let msg_id = Arc::clone(&fix.engine)
            .publish(b"hi".to_vec(), None, None, None)
            .await
            .expect("post");

        // ENCRYPTED custom-emoji React (`[0xB2; 32]` → encrypted bit set) → the
        // emitted event surfaces emojiCid/emojiSize and `encrypted: true`.
        let emoji_cid = [0xB2u8; 32];
        assert!(
            harmony_content::cid::ContentId::from_bytes(emoji_cid)
                .flags()
                .encrypted,
            "fixture CID must have the encrypted flag set"
        );
        let att = crate::community_channel_log::ChannelAttachment {
            cid: emoji_cid,
            mime: "image/png".to_string(),
            name: String::new(),
            size: 1024,
        };
        Arc::clone(&fix.engine)
            .react(msg_id, String::new(), true, Some(att))
            .await
            .expect("custom-emoji react");

        let frames = fix.sink.frames();
        let (_, custom) = frames
            .iter()
            .rev()
            .find(|(name, _)| name == "channel-reaction-received")
            .expect("a channel-reaction-received frame must be emitted");
        assert_eq!(
            custom.get("emojiCid").and_then(|v| v.as_str()),
            Some(hex::encode(emoji_cid).as_str()),
            "custom React must carry the hex emoji CID"
        );
        assert_eq!(
            custom.get("emojiSize").and_then(|v| v.as_u64()),
            Some(1024),
            "custom React must carry the emoji size"
        );
        assert_eq!(
            custom.get("encrypted").and_then(|v| v.as_bool()),
            Some(true),
            "encrypted custom React must carry encrypted: true"
        );
        assert_eq!(
            custom.get("emoji").and_then(|v| v.as_str()),
            Some(""),
            "custom React uses an empty unicode emoji field"
        );

        // PUBLIC custom-emoji React (`[0x42; 32]` → encrypted bit clear) →
        // `encrypted: false`, so the UI shows the "name this" affordance live.
        let public_cid = [0x42u8; 32];
        assert!(
            !harmony_content::cid::ContentId::from_bytes(public_cid)
                .flags()
                .encrypted,
            "fixture CID must have the encrypted flag clear"
        );
        let pub_att = crate::community_channel_log::ChannelAttachment {
            cid: public_cid,
            mime: "image/png".to_string(),
            name: String::new(),
            size: 2048,
        };
        Arc::clone(&fix.engine)
            .react(msg_id, String::new(), true, Some(pub_att))
            .await
            .expect("public custom-emoji react");
        let frames = fix.sink.frames();
        let (_, public) = frames
            .iter()
            .rev()
            .find(|(name, v)| {
                name == "channel-reaction-received"
                    && v.get("emojiCid").and_then(|c| c.as_str())
                        == Some(hex::encode(public_cid).as_str())
            })
            .expect("the public custom reaction frame must be emitted");
        assert_eq!(
            public.get("encrypted").and_then(|v| v.as_bool()),
            Some(false),
            "public custom React must carry encrypted: false"
        );

        // Unicode React → none of the optional fields are present.
        Arc::clone(&fix.engine)
            .react(msg_id, "👍".to_string(), true, None)
            .await
            .expect("unicode react");
        let frames = fix.sink.frames();
        let (_, unicode) = frames
            .iter()
            .rev()
            .find(|(name, v)| {
                name == "channel-reaction-received"
                    && v.get("emoji").and_then(|e| e.as_str()) == Some("👍")
            })
            .expect("the unicode reaction frame must be emitted");
        assert!(
            unicode.get("emojiCid").is_none(),
            "unicode React must omit emojiCid"
        );
        assert!(
            unicode.get("emojiSize").is_none(),
            "unicode React must omit emojiSize"
        );
        assert!(
            unicode.get("encrypted").is_none(),
            "unicode React must omit encrypted"
        );
    }

    /// ZEB-536 regression (Qodo/CodeAnt on PR #314): `list_message_dtos` must
    /// page by POSTS returned, not raw events scanned. A run of reactions
    /// longer than `limit` between two posts must NOT drop the later post (and
    /// strand a `since`-paging client on reaction HLCs it never receives).
    #[tokio::test]
    async fn list_message_dtos_pages_by_posts_not_reactions() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        let p = Arc::clone(&fix.engine)
            .publish(b"P".to_vec(), None, None, None)
            .await
            .expect("post P");
        // More reactions than the page limit, all landing between P and Q.
        for i in 0..5u8 {
            Arc::clone(&fix.engine)
                .react(p, format!("e{i}"), true, None)
                .await
                .expect("react");
        }
        Arc::clone(&fix.engine)
            .publish(b"Q".to_vec(), None, None, None)
            .await
            .expect("post Q");

        // limit=2 posts: both P and Q come back despite the 5 intervening
        // reactions that would fill an event-scanned page (old code: [P] only).
        let page = fix.engine.list_message_dtos(None, 2).await.expect("list");
        let bodies: Vec<Vec<u8>> = page.iter().map(|d| d.body.clone()).collect();
        assert_eq!(bodies, vec![b"P".to_vec(), b"Q".to_vec()]);

        // Progress check: page-by-1 from P's HLC advances past the reaction run
        // to Q rather than returning an empty, cursor-stranding page.
        let events = fix.engine.list_messages(None, 100).await.expect("evs");
        let p_hlc = events
            .iter()
            .find(|e| matches!(e, SignedChannelEvent::Post { .. }) && e.id() == &p)
            .map(|e| e.at().clone())
            .expect("P in log");
        let page2 = fix
            .engine
            .list_message_dtos(Some(p_hlc), 1)
            .await
            .expect("p2");
        assert_eq!(page2.len(), 1, "paging past reactions reaches Q (no stall)");
        assert_eq!(page2[0].body, b"Q".to_vec());
    }

    // ── ZEB-602: list_message_dtos_desc (newest-first) ────────────────

    /// ZEB-602: `desc` returns the newest N posts, newest-first — `limit`
    /// bounds from the NEWEST end, not the oldest (the whole point of the
    /// ticket: "the latest 2" must not mean "the oldest 2").
    #[tokio::test]
    async fn list_message_dtos_desc_returns_newest_n_newest_first() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        for b in [b"A".to_vec(), b"B".to_vec(), b"C".to_vec(), b"D".to_vec()] {
            Arc::clone(&fix.engine)
                .publish(b, None, None, None)
                .await
                .expect("post");
        }
        let page = fix
            .engine
            .list_message_dtos_desc(None, 2)
            .await
            .expect("list");
        let bodies: Vec<Vec<u8>> = page.iter().map(|d| d.body.clone()).collect();
        assert_eq!(bodies, vec![b"D".to_vec(), b"C".to_vec()]);
    }

    /// ZEB-602: the strictly-newer-than `since` floor applies in desc too.
    #[tokio::test]
    async fn list_message_dtos_desc_applies_since_floor() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        for b in [b"A".to_vec(), b"B".to_vec(), b"C".to_vec()] {
            Arc::clone(&fix.engine)
                .publish(b, None, None, None)
                .await
                .expect("post");
        }
        let events = fix.engine.list_messages(None, 100).await.expect("evs");
        let b_hlc = events
            .iter()
            .find(|e| matches!(e, SignedChannelEvent::Post { body, .. } if body == "B"))
            .map(|e| e.at().clone())
            .expect("B in log");
        let page = fix
            .engine
            .list_message_dtos_desc(Some(b_hlc), 100)
            .await
            .expect("list");
        let bodies: Vec<Vec<u8>> = page.iter().map(|d| d.body.clone()).collect();
        assert_eq!(bodies, vec![b"C".to_vec()], "B itself excluded (strict)");
    }

    /// ZEB-602: the desc walk crosses the tail→segment and segment→segment
    /// boundaries newest-first, cuts mid-segment on `limit`, and honors the
    /// `since` segment skip. seal=4, 10 events ⇒ seg0 [msg-0..3],
    /// seg1 [msg-4..7], tail [msg-8, msg-9] (same layout as the asc
    /// characterization test `collect_events_offlock_order_paging_and_since_across_segments`).
    #[tokio::test]
    async fn list_message_dtos_desc_spans_seal_boundaries() {
        let fix = build_engine_fixture(4, 250, 1000).await;
        {
            let mut log = fix.engine.log_for_test().lock().await;
            for i in 0..10u64 {
                let ev = make_signed_event(
                    fix.community_id,
                    fix.channel_id,
                    fix.self_owner,
                    Hlc {
                        wall_ms: 100 + i,
                        logical: 0,
                        device_id: "test-device".to_string(),
                    },
                    &format!("msg-{i}"),
                    &fix.signing_key,
                );
                log.append(ev).expect("append");
                if (i + 1) % 4 == 0 {
                    log.seal_and_persist().expect("seal");
                }
            }
            assert_eq!(log.manifest.segments.len(), 2, "expected 2 sealed segments");
            assert_eq!(log.tail.len(), 2, "expected 2 tail events");
        }

        let bodies = |dtos: Vec<ChannelMessageDto>| -> Vec<String> {
            dtos.into_iter()
                .map(|d| String::from_utf8(d.body).expect("utf8"))
                .collect()
        };

        // Unbounded desc == full asc listing reversed.
        let all = bodies(
            fix.engine
                .list_message_dtos_desc(None, 1000)
                .await
                .expect("list"),
        );
        assert_eq!(
            all,
            (0..10)
                .rev()
                .map(|i| format!("msg-{i}"))
                .collect::<Vec<_>>(),
            "newest-first ordering preserved across tail + both segments"
        );

        // limit=5 takes the tail (msg-9, msg-8) then cuts INSIDE seg1.
        let capped = bodies(
            fix.engine
                .list_message_dtos_desc(None, 5)
                .await
                .expect("list"),
        );
        assert_eq!(
            capped,
            (5..10)
                .rev()
                .map(|i| format!("msg-{i}"))
                .collect::<Vec<_>>(),
            "limit bounds from the newest end and cuts mid-segment"
        );

        // since = msg-3's HLC: seg0.range.1 is not strictly newer, so seg0
        // is skipped wholesale; tail + seg1 contribute msg-9..msg-4.
        let since = Hlc {
            wall_ms: 103,
            logical: 0,
            device_id: "test-device".to_string(),
        };
        let after = bodies(
            fix.engine
                .list_message_dtos_desc(Some(since), 1000)
                .await
                .expect("list"),
        );
        assert_eq!(
            after,
            (4..10)
                .rev()
                .map(|i| format!("msg-{i}"))
                .collect::<Vec<_>>(),
            "since skips the fully-older first segment and filters within"
        );
    }

    /// ZEB-602: desc pages by POSTS (a reaction run never consumes the
    /// budget) and folds the materialized reaction view in, same as asc.
    #[tokio::test]
    async fn list_message_dtos_desc_skips_reactions_and_folds_view() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        let p = Arc::clone(&fix.engine)
            .publish(b"P".to_vec(), None, None, None)
            .await
            .expect("post P");
        for i in 0..5u8 {
            Arc::clone(&fix.engine)
                .react(p, format!("e{i}"), true, None)
                .await
                .expect("react");
        }
        Arc::clone(&fix.engine)
            .publish(b"Q".to_vec(), None, None, None)
            .await
            .expect("post Q");

        let page = fix
            .engine
            .list_message_dtos_desc(None, 2)
            .await
            .expect("list");
        let bodies: Vec<Vec<u8>> = page.iter().map(|d| d.body.clone()).collect();
        assert_eq!(bodies, vec![b"Q".to_vec(), b"P".to_vec()]);
        assert_eq!(
            page[1].reactions.len(),
            5,
            "P carries its full reaction view in desc"
        );
        assert!(page[0].reactions.is_empty(), "Q has no reactions");
    }

    /// ZEB-602: `limit=0` falls back to the engine default in desc too, and
    /// an empty log returns empty rather than erroring.
    #[tokio::test]
    async fn list_message_dtos_desc_limit_zero_default_and_empty_log() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        let empty = fix
            .engine
            .list_message_dtos_desc(None, 100)
            .await
            .expect("list");
        assert!(empty.is_empty());

        for b in [b"A".to_vec(), b"B".to_vec()] {
            Arc::clone(&fix.engine)
                .publish(b, None, None, None)
                .await
                .expect("post");
        }
        let page = fix
            .engine
            .list_message_dtos_desc(None, 0)
            .await
            .expect("list");
        let bodies: Vec<Vec<u8>> = page.iter().map(|d| d.body.clone()).collect();
        assert_eq!(bodies, vec![b"B".to_vec(), b"A".to_vec()]);
    }

    /// ZEB-536 (CodeRabbit PR #314): the Post-only accessor that backs the
    /// pre-fork snapshot must page by POSTS, not raw events — a long reaction
    /// run between two posts must not consume the budget and strand the later
    /// post. P, then 5 reactions, then Q; a 2-POST budget returns [P, Q].
    #[tokio::test]
    async fn list_post_events_pages_by_posts_not_reactions() {
        let fix = build_engine_fixture(8, 250, 1000).await;
        let p = Arc::clone(&fix.engine)
            .publish(b"P".to_vec(), None, None, None)
            .await
            .expect("post P");
        for i in 0..5u8 {
            Arc::clone(&fix.engine)
                .react(p, format!("e{i}"), true, None)
                .await
                .expect("react");
        }
        let q = Arc::clone(&fix.engine)
            .publish(b"Q".to_vec(), None, None, None)
            .await
            .expect("post Q");

        let posts = fix
            .engine
            .list_post_events(None, 2)
            .await
            .expect("list post events");
        assert!(
            posts
                .iter()
                .all(|e| matches!(e, SignedChannelEvent::Post { .. })),
            "Post-only accessor must not return React events"
        );
        let ids: Vec<&MessageId> = posts.iter().map(|e| e.id()).collect();
        assert_eq!(
            ids,
            vec![&p, &q],
            "both posts returned despite the 5 intervening reactions"
        );
    }

    /// ZEB-536 two-node convergence: Engine A posts; Engine A react()s and
    /// broadcasts the packet; feeding the packet into Engine B's
    /// process_inbound_packet produces channel-reaction-received on B and
    /// B's list_message_dtos shows the same reaction.  Un-react also
    /// converges on both.
    ///
    /// Two-node approach used: both engines share the same
    /// (community_id, channel_id, channel_key, identity) because
    /// AlwaysJoinedState validates a single owner; the signed React packet
    /// from A is captured from A's publisher_rx and fed directly into B's
    /// process_inbound_packet. This avoids the Zenoh transport layer and
    /// tests the convergence logic in isolation — the same approach used
    /// by receive_well_formed_packet_appends_and_emits.
    #[tokio::test]
    async fn two_node_react_converges() {
        // Build two independent engines on the same (community, channel, key,
        // identity) — A posts + reacts, B receives the React packet.
        let mut fix_a = build_engine_fixture(8, 250, 1000).await;
        // Engine B: same community/channel/key/identity but separate dirs,
        // channels, and sinks.
        let tmp_b = tempfile::TempDir::new().expect("tempdir b");
        let state_b = Arc::new(AlwaysJoinedState {
            channel_id: fix_a.channel_id,
            owner: fix_a.self_owner,
            enrolled_key: fix_a.signing_key.verifying_key().to_bytes(),
        });
        let hlc_tracker_b: Arc<Mutex<BTreeMap<String, Hlc>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let (publisher_tx_b, _publisher_rx_b) = mpsc::channel(64);
        let (subscriber_tx_b, subscriber_rx_b) = mpsc::channel(64);
        let (query_tx_b, _query_rx_b) = mpsc::channel(8);
        let (rec_b, sink_b) = recording_sink_pair();
        let params_b = ChannelLogEngineParams {
            community_id: fix_a.community_id,
            channel_id: fix_a.channel_id,
            channel_key: Arc::clone(&fix_a.channel_key),
            root_dir: tmp_b.path().to_path_buf(),
            state_at_hlc: state_b,
            self_owner: fix_a.self_owner,
            self_device_id: "device-b".to_string(),
            signing_key: Arc::clone(&fix_a.signing_key),
            hlc_tracker: hlc_tracker_b,
            sink: sink_b,
            config: ChannelLogEngineConfig {
                log_config: ChannelLogConfig {
                    seal_threshold_events: 8,
                },
                flush_debounce_ms: 250,
                max_dirty_ms: 1000,
                ..Default::default()
            },
            publisher_tx: publisher_tx_b,
            subscriber_rx: subscriber_rx_b,
            query_request_tx: query_tx_b,
        };
        // B needs to see the Post event too — first feed A's Post packet to B.
        let engine_b = ChannelLogEngine::new(params_b).await.expect("engine b");

        // A: publish a message.
        let msg_id = Arc::clone(&fix_a.engine)
            .publish(b"hello from A".to_vec(), None, None, None)
            .await
            .expect("A publish");

        // Capture the Post packet from A's publisher and feed it into B.
        let post_packet = fix_a.publisher_rx.try_recv().expect("post packet from A");
        subscriber_tx_b
            .send(post_packet)
            .await
            .expect("feed Post to B");

        // Wait for B to ingest the Post.
        wait_for(
            || async {
                let v = engine_b.list_messages(None, 100).await.unwrap();
                if !v.is_empty() {
                    Some(())
                } else {
                    None
                }
            },
            Duration::from_secs(2),
        )
        .await
        .expect("B must see the Post");

        // A: react to the message.
        Arc::clone(&fix_a.engine)
            .react(msg_id, "👍".to_string(), true, None)
            .await
            .expect("A react");

        // Assert A sees the reaction in list_message_dtos.
        let dtos_a = fix_a
            .engine
            .list_message_dtos(None, 100)
            .await
            .expect("A list dtos");
        let m_a = dtos_a
            .iter()
            .find(|d| d.message_id == hex::encode(msg_id.0))
            .unwrap();
        assert_eq!(
            m_a.reactions
                .iter()
                .find(|r| r.emoji == "👍")
                .unwrap()
                .count,
            1,
            "A must see count=1 after react"
        );
        assert!(
            m_a.reactions.iter().find(|r| r.emoji == "👍").unwrap().mine,
            "A must see mine=true"
        );

        // Capture the React packet from A's publisher and feed it into B.
        let react_packet = fix_a.publisher_rx.try_recv().expect("react packet from A");
        subscriber_tx_b
            .send(react_packet)
            .await
            .expect("feed React to B");

        // Wait for B to emit channel-reaction-received.
        let reaction_emitted = wait_for(
            || async {
                let frames = rec_b.frames();
                if frames
                    .iter()
                    .any(|(name, _)| name == "channel-reaction-received")
                {
                    Some(())
                } else {
                    None
                }
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(
            reaction_emitted.is_some(),
            "B must emit channel-reaction-received"
        );

        // Assert B's list_message_dtos shows the reaction.
        let dtos_b = engine_b
            .list_message_dtos(None, 100)
            .await
            .expect("B list dtos");
        let m_b = dtos_b
            .iter()
            .find(|d| d.message_id == hex::encode(msg_id.0))
            .unwrap();
        assert_eq!(
            m_b.reactions
                .iter()
                .find(|r| r.emoji == "👍")
                .unwrap()
                .count,
            1,
            "B must see count=1 after receiving React"
        );
        // mine=true on B because B has the same self_owner as A (same identity).
        assert!(
            m_b.reactions.iter().find(|r| r.emoji == "👍").unwrap().mine,
            "B must see mine=true (same identity)"
        );

        // Un-react: A sends add=false; both nodes converge to no 👍.
        Arc::clone(&fix_a.engine)
            .react(msg_id, "👍".to_string(), false, None)
            .await
            .expect("A unreact");
        let unreact_packet = fix_a
            .publisher_rx
            .try_recv()
            .expect("unreact packet from A");
        subscriber_tx_b
            .send(unreact_packet)
            .await
            .expect("feed unreact to B");

        // Wait for B to process the un-react — poll its DTO state rather than
        // sleeping a fixed interval, which can flake under load while B
        // decrypts/verifies/appends and rebuilds its reaction index
        // (CodeRabbit PR #314).
        wait_for(
            || async {
                let dtos = engine_b.list_message_dtos(None, 100).await.ok()?;
                let m = dtos
                    .iter()
                    .find(|d| d.message_id == hex::encode(msg_id.0))?;
                if m.reactions.iter().all(|r| r.emoji != "👍") {
                    Some(())
                } else {
                    None
                }
            },
            Duration::from_secs(2),
        )
        .await
        .expect("B must process the un-react");

        // A converges to no 👍.
        let dtos_a2 = fix_a
            .engine
            .list_message_dtos(None, 100)
            .await
            .expect("A list dtos 2");
        let m_a2 = dtos_a2
            .iter()
            .find(|d| d.message_id == hex::encode(msg_id.0))
            .unwrap();
        assert!(
            m_a2.reactions.iter().all(|r| r.emoji != "👍"),
            "A must see no 👍 after unreact"
        );

        // B converges to no 👍.
        let dtos_b2 = engine_b
            .list_message_dtos(None, 100)
            .await
            .expect("B list dtos 2");
        let m_b2 = dtos_b2
            .iter()
            .find(|d| d.message_id == hex::encode(msg_id.0))
            .unwrap();
        assert!(
            m_b2.reactions.iter().all(|r| r.emoji != "👍"),
            "B must see no 👍 after receiving unreact"
        );
    }

    /// ZEB-541 two-engine custom-emoji react: engine A ingests a small emoji
    /// blob through the production CAS ingest pipeline (encrypted, serveable),
    /// reacts to a message carrying that blob's `emoji_attachment`, and
    /// broadcasts the React packet. Engine B (a second, independent engine on
    /// the same community/channel/key/identity, separate dirs+sinks) receives
    /// the React via `process_inbound_packet` and then proves the full
    /// cross-engine custom-emoji path:
    ///
    /// 1. **Cross-engine authorize.** B's OWN signed channel log now holds A's
    ///    React, so `B.find_attachment(emoji_cid)` returns A's signed
    ///    `emoji_attachment` descriptor (the ZEB-541 React-scan extension —
    ///    this is "B serving A's emoji CID": authorization is decided from the
    ///    React B received, not from anything A told B out-of-band).
    /// 2. **Cross-engine materialization.** B's `list_message_dtos` surfaces a
    ///    `ReactionDto` with `emoji_cid = Some(hex)`, `emoji_size = Some`,
    ///    `count == 1`, and an empty unicode `emoji` field.
    /// 3. **Cross-engine fetch + decrypt.** Using the AUTHORITATIVE size from
    ///    B's authorized descriptor, B fetches the ciphertext from the shared
    ///    CAS by the authorized CID (the same `cid_hex` A ingested under), then
    ///    decrypts with the shared community epoch key and size-verifies —
    ///    mirroring the `authorize_and_fetch_artifact` → `decrypt_and_verify_artifact`
    ///    contract `preview_reaction_emoji_impl` runs. The recovered plaintext
    ///    must byte-equal what A ingested.
    ///
    /// Harness: mirrors `two_node_react_converges` (two independent engines,
    /// packets forwarded A→B via `subscriber_tx`/`process_inbound_packet`)
    /// extended with the `cas_serve_two_node_integration` ingest-drain shape
    /// (a shared `cid_hex -> ciphertext` map fed by the production
    /// `streaming_ingest_with_options` pipeline). Both engines derive the same
    /// channel key from `build_engine_fixture`'s membership key
    /// (`EpochKey::new([0x55; 32])`), modeling B holding the community epoch
    /// key it would use to decrypt A's served emoji book.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_engine_custom_emoji_react_cross_engine_authorize_and_fetch() {
        use crate::community_state_sync::decrypt_blob;

        // The community epoch key both engines' channel keys are derived from
        // (see `build_engine_fixture`). The emoji blob is encrypted under this
        // same key, so B — which holds it — can decrypt A's served book.
        let epoch_key = EpochKey::new([0x55; 32]);

        // ── Engine A ──────────────────────────────────────────────────────
        let mut fix_a = build_engine_fixture(8, 250, 1000).await;

        // ── Engine B: same community/channel/key/identity, separate dirs ───
        let tmp_b = tempfile::TempDir::new().expect("tempdir b");
        let state_b = Arc::new(AlwaysJoinedState {
            channel_id: fix_a.channel_id,
            owner: fix_a.self_owner,
            enrolled_key: fix_a.signing_key.verifying_key().to_bytes(),
        });
        let hlc_tracker_b: Arc<Mutex<BTreeMap<String, Hlc>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let (publisher_tx_b, _publisher_rx_b) = mpsc::channel(64);
        let (subscriber_tx_b, subscriber_rx_b) = mpsc::channel(64);
        let (query_tx_b, _query_rx_b) = mpsc::channel(8);
        let (_rec_b, sink_b) = recording_sink_pair();
        let params_b = ChannelLogEngineParams {
            community_id: fix_a.community_id,
            channel_id: fix_a.channel_id,
            channel_key: Arc::clone(&fix_a.channel_key),
            root_dir: tmp_b.path().to_path_buf(),
            state_at_hlc: state_b,
            self_owner: fix_a.self_owner,
            self_device_id: "device-b".to_string(),
            signing_key: Arc::clone(&fix_a.signing_key),
            hlc_tracker: hlc_tracker_b,
            sink: sink_b,
            config: ChannelLogEngineConfig {
                log_config: ChannelLogConfig {
                    seal_threshold_events: 8,
                },
                flush_debounce_ms: 250,
                max_dirty_ms: 1000,
                ..Default::default()
            },
            publisher_tx: publisher_tx_b,
            subscriber_rx: subscriber_rx_b,
            query_request_tx: query_tx_b,
        };
        let engine_b = ChannelLogEngine::new(params_b).await.expect("engine b");

        // ── Shared in-memory CAS: cid_hex -> stored bytes ─────────────────
        // Mirrors `cas_serve_two_node_integration`'s ingest drain: the
        // production `streaming_ingest_with_options` pipeline emits one
        // `IngestRequest` per CID; the drain stores each under its cid_hex.
        // The SAME map backs B's later fetch — the cross-engine CAS A serves.
        let cas: Arc<std::sync::Mutex<HashMap<String, Vec<u8>>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (ingest_tx, mut ingest_rx) = mpsc::channel::<crate::event_loop::IngestRequest>(64);
        let cas_i = Arc::clone(&cas);
        let ingest_drainer = tokio::spawn(async move {
            while let Some(req) = ingest_rx.recv().await {
                cas_i.lock().unwrap().insert(req.cid_hex, req.data);
                let _ = req.reply.send(Ok(()));
            }
        });

        // ── A: ingest a small emoji blob (encrypted, serveable) ───────────
        // A tiny payload chunks to a single Book, so the root CID's bytes are
        // the whole ciphertext (no reassembly needed for the fetch below).
        let emoji_plaintext: Vec<u8> = (0u8..200).collect();
        let ciphertext = crate::community_state_sync::encrypt_blob(&epoch_key, &emoji_plaintext)
            .expect("encrypt emoji blob");
        let reader = tokio::io::BufReader::new(std::io::Cursor::new(ciphertext.clone()));
        let (root_cid, _n) = crate::streaming_ingest_with_options(
            reader,
            &ingest_tx,
            harmony_content::chunker::ChunkerConfig::DEFAULT,
            None,
            crate::IngestOptions {
                flags: harmony_content::cid::ContentFlags {
                    encrypted: true,
                    ..Default::default()
                },
                serveable: true,
            },
        )
        .await
        .expect("ingest emoji blob");
        assert!(
            root_cid.flags().encrypted,
            "emoji root CID must carry the encrypted flag"
        );
        let emoji_cid_bytes: [u8; 32] = root_cid.to_bytes();
        let emoji_cid_hex = hex::encode(emoji_cid_bytes);
        let emoji_size = emoji_plaintext.len() as u64;

        // ── A: post a message; forward the Post packet to B ───────────────
        let msg_id = Arc::clone(&fix_a.engine)
            .publish(b"react to me from A".to_vec(), None, None, None)
            .await
            .expect("A publish");
        let post_packet = fix_a.publisher_rx.try_recv().expect("post packet from A");
        subscriber_tx_b
            .send(post_packet)
            .await
            .expect("feed Post to B");
        wait_for(
            || async {
                let v = engine_b.list_messages(None, 100).await.unwrap();
                if v.is_empty() {
                    None
                } else {
                    Some(())
                }
            },
            Duration::from_secs(2),
        )
        .await
        .expect("B must see the Post");

        // ── A: react with the ingested emoji descriptor (binds the CID into
        //     a signed React → serve-authorizable). Forward the React to B ──
        let emoji_att = ChannelAttachment {
            cid: emoji_cid_bytes,
            mime: "image/png".to_string(),
            name: String::new(),
            size: emoji_size,
        };
        Arc::clone(&fix_a.engine)
            .react(msg_id, String::new(), true, Some(emoji_att.clone()))
            .await
            .expect("A custom-emoji react");
        let react_packet = fix_a.publisher_rx.try_recv().expect("react packet from A");
        subscriber_tx_b
            .send(react_packet)
            .await
            .expect("feed React to B");

        // ── Cross-engine authorize: B's find_attachment must yield A's
        //     signed emoji descriptor (decided from the React B received) ──
        let authorized = wait_for(
            || async {
                engine_b
                    .find_attachment(&emoji_cid_bytes, AttachmentScope::ReactionEmoji)
                    .await
                    .ok()
                    .flatten()
            },
            Duration::from_secs(2),
        )
        .await
        .expect("B must authorize A's emoji CID via the React it received");
        assert_eq!(
            authorized, emoji_att,
            "B's authorized descriptor must match A's signed emoji_attachment"
        );

        // ── Cross-engine materialization: B's ReactionDto carries the CID ──
        let dtos_b = engine_b
            .list_message_dtos(None, 100)
            .await
            .expect("B list dtos");
        let m_b = dtos_b
            .iter()
            .find(|d| d.message_id == hex::encode(msg_id.0))
            .expect("B must hold the posted message");
        assert_eq!(
            m_b.reactions.len(),
            1,
            "exactly one custom-emoji reaction chip on B"
        );
        let r = &m_b.reactions[0];
        assert_eq!(r.count, 1, "B must see count=1 for the custom emoji");
        assert_eq!(
            r.emoji, "",
            "a custom emoji uses an empty unicode emoji field"
        );
        assert_eq!(
            r.emoji_cid.as_deref(),
            Some(emoji_cid_hex.as_str()),
            "B's materialized DTO must surface A's custom emoji CID"
        );
        assert_eq!(
            r.emoji_size,
            Some(emoji_size),
            "B's materialized DTO must surface the emoji size"
        );

        // ── Cross-engine fetch + decrypt (the serve path B would run) ─────
        // Authoritative size comes from B's authorized descriptor, NOT a
        // client value. B fetches the ciphertext from the shared CAS by the
        // authorized CID, decrypts with the shared epoch key, and size-checks
        // — exactly the decrypt_and_verify_artifact contract. The recovered
        // plaintext must byte-equal what A ingested.
        let authoritative_size = authorized.size;
        let stored_ciphertext = cas
            .lock()
            .unwrap()
            .get(&emoji_cid_hex)
            .cloned()
            .expect("the authorized emoji CID must be fetchable from A's CAS");
        let recovered =
            decrypt_blob(&epoch_key, &stored_ciphertext).expect("B decrypts emoji blob");
        assert_eq!(
            recovered.len() as u64,
            authoritative_size,
            "decrypted length must match the signed authoritative size"
        );
        assert_eq!(
            recovered, emoji_plaintext,
            "B must recover the exact emoji bytes A ingested (cross-engine round-trip)"
        );

        // Drop A's ingest sender so the drain task exits cleanly.
        drop(ingest_tx);
        ingest_drainer.abort();
    }
}
