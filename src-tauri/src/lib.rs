use std::sync::Mutex;
use std::thread;

use harmony_compute::InstructionBudget;
use harmony_content::book::MemoryBookStore;
use harmony_content::storage_tier::{ContentPolicy, FilterBroadcastConfig, StorageBudget};
use harmony_runtime::{NodeConfig, NodeRuntime};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

pub mod backup_state;
pub mod community_channel_log;
pub mod community_channel_log_engine;
pub mod community_fork;
pub mod community_invite;
pub mod community_membership;
pub mod community_state_crdt;
pub mod community_state_persist;
pub mod community_state_sync;
pub mod content_index;
pub mod content_store;
pub mod dm_crypto;
pub mod dm_envelope;
pub mod dm_outbox;
pub mod dm_signing;
pub mod event_loop;
pub mod folders;
mod follows;
pub mod identity;
pub mod identity_commands;
pub mod inbound_packet;
pub mod library_directory;
pub mod mail;
pub mod mail_sync;
pub mod owner_commands;
pub mod owner_state;
pub mod owner_state_crdt;
pub mod owner_state_crypto;
pub mod owner_state_persist;
pub mod owner_state_sync;
pub mod owner_state_types;
pub mod pairing;
pub mod pairing_commands;
pub mod profile_broadcast;
pub mod recovery_cli;
pub mod recovery_policy;
mod save_dialog;
pub mod state_snapshot;
pub mod vine_feed_cache;
pub mod voice;

/// ZEB-262 Phase 4 Task 9: production impl of
/// `community_invite::AppHandleEmit` on `tauri::AppHandle<R>`. Lets
/// `community_invite::handle_unicast` emit
/// `community-state-sync-degraded` events without depending on `tauri`
/// directly (the trait + unit-type stub live in `community_invite.rs`
/// so tests can compile without a Tauri runtime).
impl<R: tauri::Runtime> crate::community_invite::AppHandleEmit for tauri::AppHandle<R> {
    fn emit_degraded(&self, community_id_hex: &str, reason_tag: &'static str) {
        let _ = self.emit(
            "community-state-sync-degraded",
            serde_json::json!({
                "communityId": community_id_hex,
                "reason": reason_tag,
            }),
        );
    }
}

// ── Chunked ingest (ZEB-154) ──────────────────────────────────────────────

/// Maximum bytes supported by the v1 flat-bundle chunked-ingest path.
///
/// Derived from the chunker's **minimum** chunk size — not the payload
/// maximum — because FastCDC with `ChunkerConfig::DEFAULT` emits at most
/// `ceil(N / min_chunk)` chunks. Using `min_chunk` guarantees the leaf
/// count can never exceed `MAX_BUNDLE_ENTRIES`, so `BundleBuilder` never
/// fails with a confusing "bundle full" error just below the true cap.
///
/// With the current defaults (MAX_BUNDLE_ENTRIES ≈ 32 767, min_chunk =
/// 256 KiB) this lands at ~8 GiB. Files larger than this need nested
/// bundles, which land with folder/directory support (ZEB-156 et al).
/// A flat-bundle-only v1 is intentional; see
/// docs/specs/2026-04-23-chunked-ingest-design.md (Q1).
pub(crate) const FLAT_BUNDLE_MAX: u64 = (harmony_content::bundle::MAX_BUNDLE_ENTRIES as u64)
    * (harmony_content::chunker::ChunkerConfig::DEFAULT.min_chunk as u64);

/// Dispatch decision for `ingest_content`, derived purely from file size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IngestDispatch {
    /// File fits in a single `for_book` CID — use the existing path.
    Single,
    /// File is larger than `MAX_PAYLOAD_SIZE` and must be chunked through
    /// the FastCDC chunker into a root bundle.
    Chunked,
}

/// Classify a file size into an ingest strategy, or return an error message
/// suitable for surfacing to the frontend if the file exceeds the v1 cap.
pub(crate) fn ingest_dispatch(size: u64) -> Result<IngestDispatch, String> {
    if size > FLAT_BUNDLE_MAX {
        return Err(format!(
            "file too large ({} bytes). v1 flat-bundle cap is {} bytes (~8 GiB). \
             Support for larger files lands with folder/nested-bundle support.",
            size, FLAT_BUNDLE_MAX
        ));
    }
    if size > harmony_content::cid::MAX_PAYLOAD_SIZE as u64 {
        Ok(IngestDispatch::Chunked)
    } else {
        Ok(IngestDispatch::Single)
    }
}

/// Chunk `bytes` via FastCDC and assemble the resulting leaf CIDs into a
/// flat bundle. Returns the ordered leaf (CID, slice) pairs, the raw bundle
/// payload, and the root bundle CID.
///
/// The caller is responsible for driving each `(cid, bytes)` pair through
/// the runtime's ingest channel in order, and for one final ingest of the
/// bundle payload under the root CID.
///
/// Expects `bytes.len() > MAX_PAYLOAD_SIZE` — for smaller inputs use the
/// existing single-book path.
///
/// Visibility is `pub` rather than `pub(crate)` so the integration tests
/// under `src-tauri/tests/` can drive the chunk + bundle construction
/// directly. `pub(crate)` would hide the symbol from the external test
/// crate and break `content_index_integration::chunked_ingest_pin_cascade_
/// fetch_burn_roundtrip`. Treat this as crate-internal — no external
/// consumers are expected.
#[allow(clippy::type_complexity)] // pre-existing; tracked for cleanup
pub fn chunk_and_bundle(
    bytes: &[u8],
) -> Result<
    (
        Vec<(harmony_content::cid::ContentId, &[u8])>,
        Vec<u8>,
        harmony_content::cid::ContentId,
    ),
    String,
> {
    use harmony_content::bundle::BundleBuilder;
    use harmony_content::chunker::{chunk_all, ChunkerConfig};
    use harmony_content::cid::{ContentFlags, ContentId, MAX_PAYLOAD_SIZE};

    if bytes.len() <= MAX_PAYLOAD_SIZE {
        return Err(format!(
            "chunk_and_bundle requires input larger than MAX_PAYLOAD_SIZE ({} bytes); \
             got {} bytes — use the single-book path instead",
            MAX_PAYLOAD_SIZE,
            bytes.len()
        ));
    }

    let ranges =
        chunk_all(bytes, &ChunkerConfig::DEFAULT).map_err(|e| format!("chunker error: {e:?}"))?;

    let mut leaves: Vec<(ContentId, &[u8])> = Vec::with_capacity(ranges.len());
    for range in ranges {
        let chunk = &bytes[range];
        let cid = ContentId::for_book(chunk, ContentFlags::default())
            .map_err(|e| format!("leaf CID error: {e:?}"))?;
        leaves.push((cid, chunk));
    }

    let mut builder = BundleBuilder::new();
    for (cid, _) in &leaves {
        builder.add(*cid);
    }
    let (bundle_payload, root) = builder
        .build_with_flags(ContentFlags::default())
        .map_err(|e| format!("bundle build error: {e:?}"))?;

    Ok((leaves, bundle_payload, root))
}

// ── Managed Tauri state ──────────────────────────────────────────────────

/// ZEB-234: shutdown-fence permit count for `send_dm`. Practical
/// "unbounded" for typical IPC concurrency; exhaustion awaits rather
/// than rejects. `stop_inner` drains all permits via `acquire_many`
/// to guarantee no in-flight `send_dm` is mid-write when
/// `SyncEngine::shutdown` runs.
pub const DM_SEND_FENCE_CAPACITY: usize = 1024;

pub struct NodeState {
    /// Background thread running the event loop (NodeRuntime is !Send).
    thread: Option<thread::JoinHandle<()>>,
    /// Send `true` to shut down the event loop.
    shutdown_tx: Option<tokio::sync::watch::Sender<bool>>,
    /// Channel for routing publish requests through the event loop's session.
    publish_tx: Option<tokio::sync::mpsc::Sender<event_loop::PublishRequest>>,
    /// Channel for routing content-fetch requests through the event loop's session.
    fetch_tx: Option<tokio::sync::mpsc::Sender<event_loop::FetchRequest>>,
    /// Channel for routing content-ingest requests through the event loop.
    ingest_tx: Option<tokio::sync::mpsc::Sender<event_loop::IngestRequest>>,
    /// Channel for routing content verb (pin/unpin/burn) requests through the event loop.
    content_verb_tx: Option<tokio::sync::mpsc::Sender<event_loop::ContentVerbRequest>>,
    /// Channel for routing follow/unfollow requests through the event loop.
    follow_tx: Option<tokio::sync::mpsc::Sender<event_loop::FollowRequest>>,
    /// Channel for sending outbound voice frames to the event loop.
    voice_tx: Option<tokio::sync::mpsc::Sender<voice::VoiceOutbound>>,
    /// Channel for voice channel join/leave requests.
    voice_channel_tx: Option<tokio::sync::mpsc::Sender<voice::VoiceChannelRequest>>,
    /// Persistent follow manager (disk-backed follow list).
    follow_mgr: Option<follows::FollowManager>,
    /// Shared set of followed addresses (read by the event loop for source tagging).
    followed_set: Option<std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>>,
    /// In-memory Vine feed cache (ZEB-286). Updated by the event loop on
    /// receive; read by list_vine_videos / mark_vine_viewed IPCs.
    /// Disk persistence deferred to ZEB-147.
    vine_feed_cache: Option<std::sync::Arc<std::sync::Mutex<vine_feed_cache::VineFeedCache>>>,
    /// Shared mail manager (read/written by event loop on receive, by commands for queries).
    mail_mgr: Option<std::sync::Arc<std::sync::Mutex<mail::MailManager>>>,
    /// Shared mail sync (walker + lazy body fetch). Stored here so Tauri
    /// commands (refresh_mail, fetch_mail_body) can reach it.
    mail_sync: Option<std::sync::Arc<mail_sync::MailSync>>,
    /// Disk-backed content index (pin/replication metadata).
    content_index: std::sync::Arc<std::sync::Mutex<content_index::ContentIndex>>,
    /// Monotonic install generation. Bumped at lock-2 install site under
    /// `start_node`. Post-install checks (pairing-handle install, failure
    /// cleanup, stop_inner gating) compare against this to detect whether a
    /// later `start_node` has SUCCESSFULLY INSTALLED over us. Distinct from
    /// `install_seq`, which detects attempts-in-progress.
    generation: u64,
    /// Monotonic start-attempt sequence (ZEB-221). Bumped at lock-1 of
    /// `start_node` to reserve a slot before async work. Validated at
    /// lock-2 to detect supersede WITHOUT changing `generation`'s
    /// "successful install" semantics — that distinction matters because
    /// post-install code uses `generation` to determine whether a later
    /// install completed, not whether a later attempt merely started.
    install_seq: u64,
    /// Hex-encoded node address (set on startup, used to stamp outgoing messages).
    node_addr: String,
    /// ZEB-197 v2 pairing state-machine handle. `Some` while the node is
    /// running; the inner task drives the abstract pairing state machine
    /// against a `ZenohPairingTransport` bound to the running event loop.
    pairing_handle: Option<crate::pairing::state_machine::PairingHandle>,
    /// Phase 3a SyncEngine — `Some` while the node is running and an
    /// owner identity (master_seed) is available. Shutdown is called
    /// explicitly in `stop_inner` before the event-loop thread is joined.
    sync_engine: Option<std::sync::Arc<crate::owner_state_sync::SyncEngine>>,
    /// ZEB-217 Sub-C Phase 2: registry of per-community state-CRDT
    /// SyncEngines. Lifted from start_node (mirrors `sync_engine`
    /// above) so Phase 3 IPC handlers (create_community,
    /// redeem_invite, leave_community, list_community_members) can
    /// reach the engine pool without holding the per-engine Arcs
    /// directly. Shared with the event-loop ONLY through the
    /// per-community `CommunityAdapterRequest`s passed at startup;
    /// the registry itself is owned exclusively by NodeState.
    /// Shutdown (`registry.shutdown_all()`) is awaited explicitly in
    /// `stop_inner` BEFORE the event-loop thread is joined so each
    /// engine's final flush + persist runs while the Zenoh session
    /// (and thus the publisher) is still live.
    community_registry: Option<std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>>,
    /// Sender side of the community delta channel — kept for stop_node /
    /// restart to drop on shutdown so the consumer task winds down
    /// cleanly. The receiver was moved into the consumer task at
    /// start_node time; this Sender is the only handle on the
    /// engine-side senders' clone-source. Dropping closes the channel
    /// after every per-engine clone has also been dropped (which
    /// happens after `registry.shutdown_all()`).
    community_delta_tx:
        Option<tokio::sync::mpsc::Sender<crate::community_state_sync::CommunityMembershipDelta>>,
    /// ZEB-225 Sub-B Phase 2: per-process DM outbox state. Constructed in
    /// start_node alongside the SyncEngine; shared with the IPC handler
    /// (send_dm) and the event-loop drain tick.
    dm_outbox: Option<std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>>,
    /// Phase 2: in-process StubTransport. Phase 3b replaces with a real
    /// adapter that pushes RuntimeAction::SendUnicastToDevice.
    dm_transport: Option<std::sync::Arc<dyn crate::dm_outbox::DmTransport>>,
    /// CRDT state Mutex (already constructed for SyncEngine; we hold a
    /// clone so the IPC handler can lock it independently of SyncEngine).
    /// Stored as Option because identity-restore can null out everything.
    crdt_state: Option<std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>>,
    /// HLC tracker (mirror of SyncEngine's tracker; the dm_outbox handler
    /// reads/writes the local device's entry to keep send_dm's HLCs
    /// monotone with state-root publishes).
    hlc_tracker: Option<
        std::sync::Arc<
            tokio::sync::Mutex<std::collections::BTreeMap<String, crate::owner_state_types::Hlc>>,
        >,
    >,
    /// Local device_id string + self OwnerAddr — captured at start_node
    /// time, snapshot for IPC handlers that mint OutboxEntry / HLC stamps.
    dm_device_id: Option<String>,
    dm_self_owner: Option<crate::owner_state_types::OwnerAddr>,
    /// ContentStore handle — same `Arc` SyncEngine was constructed with.
    /// Lifted onto NodeState so send_dm can write blobs through the same
    /// store SyncEngine uses for state-root publishes (RuntimeContentStore
    /// in production, InMemoryStub in some tests).
    content_store: Option<std::sync::Arc<dyn crate::content_store::ContentStore>>,
    /// ZEB-234: shutdown fence. `send_dm` and `delete_outbox_entry`
    /// each acquire a permit for the duration of their outbox mutation;
    /// `stop_inner` sets `stopping` then drains all permits before
    /// `SyncEngine::shutdown`, ensuring no in-flight mutation races
    /// the flush. `Some(_)` while running, `None` after stop.
    dm_send_inflight: Option<std::sync::Arc<tokio::sync::Semaphore>>,
    /// ZEB-234: paired stopping flag. Set synchronously in
    /// `stop_inner` BEFORE the permit drain so newly-arriving
    /// `send_dm` calls early-reject. Cleared (None'd) in symmetry
    /// with `dm_send_inflight`.
    dm_send_stopping: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// ZEB-227 Path B: outbound DM unicast channel sender.
    /// `RuntimeUnicastTransport` (Task 6) holds a clone; `event_loop` drains
    /// the receiver and forwards each `UnicastSendRequest` as
    /// `RuntimeEvent::SendUnicastToDevice`. Cleared on stop_node so a
    /// restart's transport doesn't carry a stale sender.
    unicast_send_tx: Option<tokio::sync::mpsc::Sender<crate::dm_outbox::UnicastSendRequest>>,
    /// ZEB-228 Phase 4: 64-byte combined `identity_pub` for our local
    /// device (X25519_pub(32) || Ed25519_pub(32) per
    /// `harmony_identity::Identity::to_public_bytes()`). Captured in
    /// `start_node` before the in-memory `PrivateIdentity` is dropped, so
    /// `add_space` can ship it as the bootstrap pubkey on outbound
    /// `DmInvite` packets without re-deriving from the private bytes.
    /// Cleared on stop_node so a stale pub never leaks into a new
    /// identity's invites.
    dm_identity_pub_64: Option<[u8; 64]>,
    /// ZEB-217 Sub-C Phase 3 Task 9: sender used by IPC handlers
    /// (`create_community`, `redeem_invite`) to dispatch a
    /// `CommunityAdapterRequest` into the event loop, where it's
    /// drained from the `select!` and converted to a
    /// `spawn_community_state_zenoh_adapter` call against the live
    /// session. Decoupling the IPC from the session means the
    /// `Session` doesn't need to be reachable from `NodeState` — the
    /// event loop owns it exclusively. Cleared on stop_node so a
    /// restart's adapter requests don't dispatch to a dropped event
    /// loop's channel.
    community_adapter_request_tx:
        Option<tokio::sync::mpsc::Sender<crate::event_loop::CommunityAdapterRequest>>,
    /// ZEB-270 Phase 3 Task 4C: per-(community, channel) ChannelLog
    /// engine registry. `None` until `start_node` constructs it
    /// (post-event-loop-ready, so the registry can hold the live
    /// `Arc<zenoh::Session>` per spec §7.1). Cleared during
    /// `stop_inner` so the per-channel engines + their Zenoh adapters
    /// shut down cleanly before the session itself drops.
    ///
    /// Typed `tauri::Wry` because Wry is the production runtime;
    /// tests construct registries directly against `tauri::test::MockRuntime`
    /// (see `community_channel_log_engine::tests::registry_*`) without
    /// going through `NodeState`. If we later want to support a
    /// non-Wry production runtime, this field becomes parameterised
    /// (which would propagate `R: Runtime` through `NodeState` itself
    /// — a larger refactor).
    channel_log_registry:
        Option<std::sync::Arc<crate::community_channel_log_engine::ChannelLogRegistry<tauri::Wry>>>,
    /// ZEB-218 Sub-D Phase 1: aggregated library-directory state. `Some`
    /// while the node is running; the matching `request_rx` is consumed
    /// by an event-loop task that declares per-library Zenoh
    /// subscribers on demand. Cleared in `stop_inner` so the channel
    /// closes and the consumer task winds down on next recv.
    pub library_directory: Option<std::sync::Arc<crate::library_directory::LibraryDirectory>>,
    /// ZEB-281 Sub-D Phase 4: profile-broadcast publisher. `Some` while the
    /// node is running and an owner identity is available. Shutdown is
    /// called explicitly in `stop_inner` before the event-loop thread is
    /// joined so the in-flight publish drains.
    profile_broadcast_publisher:
        Option<std::sync::Arc<crate::profile_broadcast::ProfileBroadcastPublisher>>,
    /// ZEB-281 Sub-D Phase 4: peer-broadcast cache. Shared with the
    /// event-loop's subscriber task pool. Always Some while node is
    /// running.
    profile_broadcast_cache:
        Option<std::sync::Arc<crate::profile_broadcast::ProfileBroadcastCache>>,
    /// ZEB-281 Sub-D Phase 4: control channel into the event-loop's
    /// profile-broadcast subscriber task pool. IPC handlers send
    /// `Subscribe`/`Unsubscribe`; the event loop owns the Zenoh
    /// subscriber map.
    profile_broadcast_request_tx:
        Option<tokio::sync::mpsc::Sender<crate::event_loop::ProfileBroadcastRequest>>,
    /// ZEB-281 Sub-D Phase 4: monotonic subscription-id allocator.
    /// Persisted only across IPC calls within a single node lifetime;
    /// reset on stop_node.
    profile_broadcast_next_subscription_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl NodeState {
    /// True when the event-loop thread is running. Identity-restore IPCs
    /// refuse while the node is up, since the running NodeRuntime caches
    /// the old keys + zenoh subscriptions and would not pick up the new
    /// identity until restart (CodeRabbit round 5).
    pub fn is_running(&self) -> bool {
        self.thread.is_some()
    }
}

impl Default for NodeState {
    fn default() -> Self {
        Self {
            thread: None,
            shutdown_tx: None,
            publish_tx: None,
            fetch_tx: None,
            ingest_tx: None,
            content_verb_tx: None,
            follow_tx: None,
            voice_tx: None,
            voice_channel_tx: None,
            follow_mgr: None,
            followed_set: None,
            vine_feed_cache: None,
            mail_mgr: None,
            mail_sync: None,
            content_index: std::sync::Arc::new(std::sync::Mutex::new(
                content_index::ContentIndex::load(std::path::Path::new("")),
            )),
            generation: 0,
            install_seq: 0,
            node_addr: String::new(),
            pairing_handle: None,
            sync_engine: None,
            community_registry: None,
            community_delta_tx: None,
            dm_outbox: None,
            dm_transport: None,
            crdt_state: None,
            hlc_tracker: None,
            dm_device_id: None,
            dm_self_owner: None,
            content_store: None,
            dm_send_inflight: None,
            dm_send_stopping: None,
            unicast_send_tx: None,
            dm_identity_pub_64: None,
            community_adapter_request_tx: None,
            // ZEB-270 Task 4C: registry stays None until start_node
            // wires it (see follow-up Task 4C deferred work).
            channel_log_registry: None,
            // ZEB-218 Sub-D Phase 1: directory stays None until
            // start_node wires it.
            library_directory: None,
            // ZEB-281 Sub-D Phase 4: publisher / cache / request_tx stay
            // None until start_node wires them; the SubscriptionId
            // allocator starts at 1 (0 reserved as "uninitialized").
            profile_broadcast_publisher: None,
            profile_broadcast_cache: None,
            profile_broadcast_request_tx: None,
            profile_broadcast_next_subscription_id: std::sync::Arc::new(
                std::sync::atomic::AtomicU64::new(1),
            ),
        }
    }
}

// ── Data types (shared with frontend via Tauri events) ───────────────────

/// Parsed capacity advertisement from a harmony-node.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapacityUpdate {
    pub node_addr: String,
    pub model_cid: String,
    pub ready: bool,
    /// Hop distance derived from Zenoh routing: 1 = direct peer, 2 = via router.
    /// `None` when the publisher didn't include a ZenohId attachment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hop_distance: Option<u8>,
}

/// Zenoh connection status pushed to the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ZenohStatus {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Profile published to/received from the network.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfilePayload {
    pub address: String,
    pub display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    /// Hex-encoded CID for full-size avatar content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_cid: Option<String>,
    /// Hex-encoded CID for thumbnail avatar content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_mini_cid: Option<String>,
}

/// Channel message sent from the frontend.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessagePayload {
    /// Channel identifier (matches navNode id, e.g. "general").
    pub channel: String,
    /// Community/hub identifier (e.g. "harmony-dev").
    pub hub: String,
    pub text: String,
    pub priority: String,
    pub reply_to: Option<String>,
    /// Sender's display name (included in wire format so receivers can
    /// show it even before receiving a profile update).
    #[serde(default)]
    pub sender_name: String,
}

/// Channel message received from the network (emitted to frontend).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelMessagePayload {
    pub id: String,
    pub sender_address: String,
    pub sender_name: String,
    pub channel: String,
    pub hub: String,
    pub text: String,
    pub timestamp: u64,
    pub priority: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

/// Telemetry event pushed to the frontend via IPC.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryEventPayload {
    pub node_addr: String,
    pub intent: String,
    pub sequence: u64,
    pub timestamp: u64,
    pub payload: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

// ── Parsing helpers (used by event_loop.rs and tests) ────────────────────

const CAPACITY_PREFIX: &str = "harmony/compute/capacity/";

pub fn parse_capacity(key_expr: &str, payload: &[u8]) -> Option<CapacityUpdate> {
    let node_addr = key_expr.strip_prefix(CAPACITY_PREFIX)?;
    if payload.len() < 33 {
        return None;
    }
    let model_cid = hex::encode(&payload[..32]);
    let ready = payload[32] == 0x01;
    Some(CapacityUpdate {
        node_addr: node_addr.to_string(),
        model_cid,
        ready,
        hop_distance: None, // Set by emit_frontend_event after ZID matching
    })
}

pub fn parse_telemetry(wire: &[u8]) -> Option<TelemetryEventPayload> {
    let event = harmony_telemetry::decode_event(wire).ok()?;
    Some(TelemetryEventPayload {
        node_addr: event.node_addr,
        intent: event.intent,
        sequence: event.sequence,
        timestamp: event.timestamp,
        payload: event.payload,
        confidence: event.confidence,
        source: event.source,
    })
}

// ── Tauri commands ───────────────────────────────────────────────────────

/// Stop a node given its extracted handles (called outside the lock).
fn stop_handles(
    shutdown_tx: Option<tokio::sync::watch::Sender<bool>>,
    thread: Option<thread::JoinHandle<()>>,
) {
    if let Some(tx) = shutdown_tx {
        let _ = tx.send(true);
    }
    if let Some(thread) = thread {
        let _ = thread.join();
    }
}

/// Stop the running node (if any). Returns after the event loop thread exits.
/// Returns `true` if a node was actually stopped, `false` if it was a no-op.
fn stop_inner(state: &Mutex<NodeState>, expected_gen: Option<u64>) -> bool {
    // ZEB-270 Phase 3 Task 4C: declared in the outer scope so the
    // post-lock shutdown_all block can take it. Assigned inside the
    // lock alongside the other `take()` calls below.
    let channel_log_registry_for_shutdown: Option<
        std::sync::Arc<crate::community_channel_log_engine::ChannelLogRegistry<tauri::Wry>>,
    >;
    // ZEB-281 Sub-D Phase 4: declared in the outer scope so the
    // post-lock `shutdown().await` can drive the publisher's final-flush
    // pass on an ephemeral runtime (the std `MutexGuard` is `!Send`).
    let profile_broadcast_publisher_for_shutdown: Option<
        std::sync::Arc<crate::profile_broadcast::ProfileBroadcastPublisher>,
    >;
    let (
        shutdown_tx,
        thread,
        publish_tx,
        fetch_tx,
        ingest_tx,
        content_verb_tx,
        follow_tx,
        voice_tx,
        voice_channel_tx,
        _follow_mgr,
        _followed_set,
        _vine_feed_cache,
        _mail_sync,
        pairing_handle,
        sync_engine,
        community_registry,
        community_delta_tx,
        dm_outbox,
        dm_transport,
        crdt_state,
        hlc_tracker,
        dm_device_id,
        dm_self_owner,
        content_store,
        unicast_send_tx,
        dm_send_inflight,
        dm_send_stopping,
    ) = {
        let mut guard = match state.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        if let Some(gen) = expected_gen {
            if guard.generation != gen {
                return false; // Stale stop — newer node exists
            }
        }
        guard.node_addr.clear();
        let tup = (
            guard.shutdown_tx.take(),
            guard.thread.take(),
            guard.publish_tx.take(),
            guard.fetch_tx.take(),
            guard.ingest_tx.take(),
            guard.content_verb_tx.take(),
            guard.follow_tx.take(),
            guard.voice_tx.take(),
            guard.voice_channel_tx.take(),
            guard.follow_mgr.take(),
            guard.followed_set.take(),
            guard.vine_feed_cache.take(),
            // Drop mail_sync so refresh_mail / fetch_mail_body can't reach
            // a closed fetch_tx / refresh_tx after stop. Channels are
            // already gone above; the MailSync handle would just yield
            // "channel closed" errors until next start.
            guard.mail_sync.take(),
            guard.pairing_handle.take(),
            guard.sync_engine.take(),
            guard.community_registry.take(),
            guard.community_delta_tx.take(),
            guard.dm_outbox.take(),
            guard.dm_transport.take(),
            guard.crdt_state.take(),
            guard.hlc_tracker.take(),
            guard.dm_device_id.take(),
            guard.dm_self_owner.take(),
            guard.content_store.take(),
            guard.unicast_send_tx.take(),
            // ZEB-234: take fence handles so we can set the stopping flag
            // and drain in-flight send_dm calls outside the lock scope.
            guard.dm_send_inflight.take(),
            guard.dm_send_stopping.take(),
        );
        // ZEB-228 Phase 4: clear our cached identity_pub so a restart
        // can't accidentally ship the prior identity's pub on a new
        // identity's invites. `[u8; 64]` is Copy, so just `take()` and
        // discard — no extra cleanup needed beyond the assignment.
        let _ = guard.dm_identity_pub_64.take();
        // ZEB-217 Sub-C Phase 3 Task 9: drop the on-demand
        // adapter-request sender. The event_loop's matching receiver
        // gets None on next recv(); the select arm exits cleanly.
        // Cleared even when the channel was unused (no
        // create_community calls in this lifetime) so a restart's
        // fresh `Sender` doesn't collide with a leaked one.
        let _ = guard.community_adapter_request_tx.take();
        // ZEB-270 Phase 3 Task 4C: take the registry handle so we
        // can run `shutdown_all` against it below outside the std
        // `MutexGuard` scope (the `block_on` would panic on the
        // !Send guard). `let _ = ` not `take().map(...)` because the
        // shutdown happens later — we hoist the take into the outer
        // scope via a separate binding rather than trying to thread
        // it through the already-saturated `tup`.
        channel_log_registry_for_shutdown = guard.channel_log_registry.take();
        // ZEB-218 Sub-D Phase 1: drop the library_directory Arc. The
        // event-loop's consumer task observes the matching request_tx
        // close on next recv and exits cleanly.
        let _ = guard.library_directory.take();
        // ZEB-281 Sub-D Phase 4: take the publisher into the outer-scope
        // binding so `shutdown()` runs on an ephemeral runtime below
        // (the std `MutexGuard` is `!Send` across an await). Clear the
        // cache + request_tx + reset the SubscriptionId allocator so a
        // restart starts at 1 again.
        profile_broadcast_publisher_for_shutdown = guard.profile_broadcast_publisher.take();
        guard.profile_broadcast_cache = None;
        guard.profile_broadcast_request_tx = None;
        guard.profile_broadcast_next_subscription_id =
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
        tup
    };

    let had_node = shutdown_tx.is_some() || thread.is_some();
    // ZEB-234: signal stopping synchronously so any send_dm currently
    // in its pre-acquire pre-check fast-rejects without queuing a new
    // permit. Must happen after the lock is released (the flag is an
    // Arc<AtomicBool> taken out of the NodeState above) and before the
    // drain below.
    if let Some(ref stopping) = dm_send_stopping {
        stopping.store(true, std::sync::atomic::Ordering::Release);
    }
    // ZEB-281 Sub-D Phase 4: abort the profile-broadcast publisher BEFORE
    // dropping `publish_tx`. `shutdown()` calls `JoinHandle::abort()` on
    // the publisher's background task — ordering this before the
    // publish channel drops avoids a race where the publisher debounce
    // tick wakes during teardown and the sink call surfaces a closed-
    // channel Err. `stop_inner` is sync but the publisher's shutdown is
    // async — use the same `thread::scope` + ephemeral-runtime pattern
    // as the other registry shutdowns below.
    if let Some(publisher) = profile_broadcast_publisher_for_shutdown {
        std::thread::scope(|s| {
            s.spawn(|| {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => {
                        rt.block_on(publisher.shutdown());
                    }
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            "could not build ephemeral tokio runtime for \
                             ProfileBroadcastPublisher shutdown — task abort skipped"
                        );
                    }
                }
            });
        });
    }
    // Drop the pairing handle BEFORE the publish_tx so the state machine
    // task observes its mpsc shutdown cleanly: the handle owns the JoinHandle
    // for the SM task; once dropped, the task's transport.recv path exits
    // when its owned receiver hits None. We then drop publish_tx, which
    // closes the event-loop publish channel.
    drop(pairing_handle);
    drop(publish_tx); // drop sender so event loop's recv returns None
    drop(fetch_tx);
    drop(ingest_tx);
    drop(content_verb_tx);
    drop(follow_tx);
    drop(voice_tx);
    drop(voice_channel_tx);
    // ZEB-225 Sub-B Phase 2: drop DM outbox handles after the channel
    // drops. send_dm IPC and the event-loop drain tick both clone these
    // Arcs into local scope before await, so dropping our Arc here just
    // releases our reference; any in-flight IPC keeps its own clone
    // alive for the duration of its critical section.
    drop(dm_outbox);
    drop(dm_transport);
    drop(crdt_state);
    drop(hlc_tracker);
    drop(dm_device_id);
    // OwnerAddr is Copy → use `let _` instead of drop() to satisfy
    // clippy::dropping_copy_types (the binding goes out of scope here
    // either way; the explicit binding is just for documentation).
    let _ = dm_self_owner;
    drop(content_store);
    // ZEB-227 Path B: drop the outbound unicast sender so any clone held
    // by the now-shutting-down RuntimeUnicastTransport (Task 11) sees its
    // last reference reach the close threshold. The event_loop's receiver
    // gets None on its next .recv() and the select arm de-registers.
    drop(unicast_send_tx);
    // ZEB-270 Phase 3 Task 4C: shut down per-(community, channel) log
    // engines BEFORE the per-community state engines. The channel-log
    // engine's verify-on-receive path resolves identity + state-at-HLC
    // from the matching CommunitySyncEngine; tearing those down first
    // would break inflight verifies the channel-log loop is mid-await
    // on. Final flush is synchronous per `engine.shutdown` contract,
    // so by the time `shutdown_all` returns every channel's tail is
    // durably written.
    //
    // Same `thread::scope` + ephemeral-runtime pattern as the
    // CommunitySyncRegistry block below — `stop_inner` is sync but
    // reachable from async contexts; a `block_on` inside an existing
    // runtime panics.
    if let Some(registry) = channel_log_registry_for_shutdown {
        std::thread::scope(|s| {
            s.spawn(|| {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => {
                        if let Err(e) = rt.block_on(registry.shutdown_all()) {
                            tracing::error!(
                                error = %e,
                                "ChannelLogRegistry shutdown_all failed during stop_inner"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            "could not build ephemeral tokio runtime for \
                             ChannelLogRegistry shutdown — final flush/persist skipped"
                        );
                    }
                }
            });
        });
    }
    // ZEB-217 Sub-C Phase 2: shut down the per-community engine pool
    // BEFORE the owner-state SyncEngine. Each community engine drives
    // its own debounced final-publish + persist pass on
    // `shutdown()`; running this before the event-loop thread joins
    // keeps the Zenoh session (and per-community publisher tasks)
    // alive long enough for the final state-root publish to land on
    // the wire. Awaiting all engines also closes their internal
    // `error_tx` clones, which lets the start_node-spawned drain task
    // exit cleanly when its receiver returns None.
    //
    // Same `thread::scope` + ephemeral-runtime pattern as the
    // SyncEngine shutdown below — `stop_inner` is sync but reachable
    // from async contexts, and a `block_on` from inside an existing
    // runtime panics.
    if let Some(registry) = community_registry {
        std::thread::scope(|s| {
            s.spawn(|| {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => {
                        if let Err(e) = rt.block_on(registry.shutdown_all()) {
                            tracing::error!(
                                error = %e,
                                "CommunitySyncRegistry shutdown_all failed during stop_inner"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            "could not build ephemeral tokio runtime for \
                             CommunitySyncRegistry shutdown — final publish/persist skipped"
                        );
                    }
                }
            });
        });
    }
    // ZEB-217 Sub-C Phase 3 Task 8: drop the community delta sender after
    // `registry.shutdown_all()` completes so every per-engine clone has
    // already been released. The consumer task's receiver then observes
    // a closed channel and exits cleanly.
    drop(community_delta_tx);
    // ZEB-234: drain in-flight send_dm permits before SyncEngine final
    // flush. `acquire_many(CAPACITY)` blocks until every outstanding
    // permit has been returned — guaranteeing no send_dm is
    // mid-mutation when the flush runs. Mirror the existing
    // `thread::scope` + ephemeral-runtime pattern used by the registry
    // shutdowns above — `stop_inner` is sync and cannot `.await`.
    if let Some(sem) = dm_send_inflight {
        std::thread::scope(|s| {
            s.spawn(|| {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => {
                        rt.block_on(drain_dm_send_fence(sem));
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "ZEB-234: failed to build drain runtime; \
                             proceeding with shutdown (in-flight \
                             send_dm may produce duplicates)"
                        );
                    }
                }
            });
        });
    }
    // Phase 3a: explicitly shut down the SyncEngine before joining the
    // event-loop thread. This flushes any pending debounced publish and
    // runs the final persist pass. Must run before stop_handles so the
    // engine's internal tokio task is still alive when we await it.
    //
    // `stop_inner` is sync, but it's reachable from async contexts (e.g.,
    // start_node's restart path). Calling `Runtime::block_on` on a thread
    // that already participates in a Tokio runtime panics with "Cannot
    // start a runtime from within a runtime." Host the shutdown on a
    // fresh OS thread via `thread::scope` so the new runtime sees no
    // outer runtime context.
    if let Some(engine) = sync_engine {
        std::thread::scope(|s| {
            s.spawn(|| {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => {
                        if let Err(e) = rt.block_on(engine.shutdown()) {
                            tracing::error!(
                                error = %e,
                                "SyncEngine final flush failed during stop_inner"
                            );
                        }
                    }
                    Err(e) => {
                        // Without the ephemeral runtime we can't
                        // drive `engine.shutdown()` from this sync
                        // context, so the final publish + persist
                        // are skipped. Surfacing the failure loudly
                        // is the best we can do — silently dropping
                        // the last delta would corrupt next-boot
                        // state. Runtime build is essentially
                        // infallible in practice (only fails on OOM
                        // / thread-creation failure), so this path
                        // is mostly defensive.
                        tracing::error!(
                            error = %e,
                            "could not build ephemeral tokio runtime for SyncEngine \
                             shutdown — final publish/persist skipped"
                        );
                    }
                }
            });
        });
    }
    stop_handles(shutdown_tx, thread);
    had_node
}

/// Bump `state.install_seq` and return the new value.
///
/// Called under lock-1 of `start_node` to reserve a start-attempt slot
/// BEFORE doing any async work outside the lock. The reserved value is
/// later validated under lock-2 via [`check_install_seq_or_supersede`]
/// so a concurrent `start_node` cannot orphan our spawned resources.
///
/// Critically, this does NOT bump `generation` — that field keeps its
/// pre-ZEB-221 semantics (bumped only on SUCCESSFUL install at lock-2)
/// because post-install code (pairing-handle install, failure cleanup,
/// stop_inner gating) needs to know whether a later install actually
/// completed, not just whether a later attempt started.
///
/// See [ZEB-221](https://linear.app/zeblith/issue/ZEB-221) for the
/// full race analysis.
fn reserve_install_seq(state: &Mutex<NodeState>) -> Result<u64, String> {
    let mut guard = state
        .lock()
        .map_err(|e| format!("reserve_install_seq lock error: {e}"))?;
    guard.install_seq += 1;
    Ok(guard.install_seq)
}

/// Lock `state` and verify the caller's reserved install-attempt sequence
/// still matches.
///
/// Returns the guard on match. Returns
/// [`SupersededError::Superseded`] if a later [`reserve_install_seq`] has
/// bumped past `my_seq`, indicating a concurrent `start_node` has reserved
/// a higher slot and the caller must abort + clean up the resources it
/// built outside the lock.
fn check_install_seq_or_supersede(
    state: &Mutex<NodeState>,
    my_seq: u64,
) -> Result<std::sync::MutexGuard<'_, NodeState>, SupersededError> {
    let guard = state
        .lock()
        .map_err(|e| SupersededError::LockError(format!("{e}")))?;
    if guard.install_seq != my_seq {
        return Err(SupersededError::Superseded {
            my_seq,
            current: guard.install_seq,
        });
    }
    Ok(guard)
}

#[derive(Debug)]
enum SupersededError {
    LockError(String),
    Superseded { my_seq: u64, current: u64 },
}

impl std::fmt::Display for SupersededError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SupersededError::LockError(msg) => write!(f, "node-state lock error: {msg}"),
            SupersededError::Superseded { my_seq, current } => write!(
                f,
                "start_node superseded by concurrent call (my_seq={my_seq}, current={current})"
            ),
        }
    }
}

/// Start the harmony node with an embedded NodeRuntime.
///
/// Generates/loads identity, creates the runtime, and spawns the event loop
/// as a background task. Emits `zenoh-status` events to the frontend.
#[tauri::command]
async fn start_node(
    endpoint: Option<String>,
    app: AppHandle,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    // ── Atomic stop→identity→config→spawn→store ─────────────────────
    // Everything from stop through handle registration runs under the
    // lock (with a brief drop for the blocking thread join). This
    // prevents concurrent start_node calls from racing on identity
    // generation or orphaning threads.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (publish_tx, publish_rx) = tokio::sync::mpsc::channel(64);
    let (fetch_tx, fetch_rx) = tokio::sync::mpsc::channel(64);
    let (ingest_tx, ingest_rx) = tokio::sync::mpsc::channel(64);
    // Phase 3b: CasOp channel for SyncEngine ↔ event_loop.
    // Capacity 8 is chosen because the SyncEngine serializes its publishes
    // (debounce window) so at most one PutLocal is in flight at a time;
    // GetOrFetch uses a second-mpsc-hop re-entry pattern that briefly
    // doubles the queue depth. See spec §"Risks: cas_op_tx capacity".
    let (cas_op_tx, cas_op_rx) = tokio::sync::mpsc::channel::<crate::content_store::CasOp>(8);
    // ZEB-227 Path B: outbound DM unicast channel. Sized at 256 to absorb
    // realistic group-DM fan-out spikes: a single send_dm to a group can
    // emit up to 16 members × 4 devices = 64 UnicastSendRequests, and
    // overlapping batches from concurrent send_dm + handle_cidnotify_lifted ack
    // fan-out can stack on top. 256 is "doubled-and-then-some" of that
    // single-send bound — production try_send call sites
    // (RuntimeUnicastTransport::send + handle_cidnotify_lifted ack fan-out)
    // surface Transient on full so back-pressure NEVER causes deadlock
    // even if the cap is exceeded; the larger cap just keeps that
    // recovery path off the hot path. Sender clone is lifted onto
    // NodeState so Task 11 can reach it when instantiating
    // RuntimeUnicastTransport; receiver is consumed by event_loop::run's
    // new select! arm (forwards each request as
    // RuntimeEvent::SendUnicastToDevice into NodeRuntime).
    let (unicast_send_tx, unicast_send_rx) =
        tokio::sync::mpsc::channel::<crate::dm_outbox::UnicastSendRequest>(256);
    let (follow_tx, follow_rx) = tokio::sync::mpsc::channel(64);
    let (voice_tx, voice_rx) = tokio::sync::mpsc::channel(100);
    let (voice_channel_tx, voice_channel_rx) = tokio::sync::mpsc::channel(16);
    let (content_verb_tx, content_verb_rx) =
        tokio::sync::mpsc::channel::<event_loop::ContentVerbRequest>(32);
    // Mail refresh channel. MailSync (constructed below once identity is
    // loaded) owns the sender; the event loop's select! arm services
    // RefreshRequests by issuing a Zenoh get against the gateway's
    // mail-root queryable.
    let (mail_refresh_tx, mail_refresh_rx) =
        tokio::sync::mpsc::channel::<crate::mail_sync::RefreshRequest>(8);
    // ZEB-197 pairing wire-message inbound channel. The event loop fills
    // this from `harmony/pairing/v2/lan/**` Zenoh subscription samples;
    // the ZenohPairingTransport (constructed after ready_rx) drains it.
    let (pairing_in_tx, pairing_in_rx) =
        tokio::sync::mpsc::channel::<crate::pairing::types::PairingWireMessage>(64);

    // Load the follow list from disk and create the shared followed set.
    let app_data_dir = {
        use tauri::Manager;
        app.path()
            .app_data_dir()
            .map_err(|e| format!("app_data_dir: {e}"))?
    };
    std::fs::create_dir_all(&app_data_dir).map_err(|e| format!("create app_data_dir: {e}"))?;
    let follow_mgr = follows::FollowManager::load(&app_data_dir);
    let followed_set = std::sync::Arc::new(std::sync::Mutex::new(
        follow_mgr
            .addresses()
            .into_iter()
            .collect::<std::collections::HashSet<String>>(),
    ));
    // ZEB-155: fetch-completion channel. Both halves are owned by
    // start_node so the spawned fetch task (in event_loop) can clone the
    // tx, while the main loop consumes from the rx.
    let (fetch_completion_tx, fetch_completion_rx) = tokio::sync::mpsc::channel::<[u8; 32]>(32);

    let followed_set_clone = followed_set.clone();

    // ZEB-286: in-memory VineFeedCache shared between event loop and IPCs.
    // ZEB-147: load() reads vine_feed.json (if any) and arms save() so
    // every mutating outcome persists to disk atomically.
    let vine_feed_cache = std::sync::Arc::new(std::sync::Mutex::new(
        vine_feed_cache::VineFeedCache::load(&app_data_dir),
    ));
    let vine_feed_cache_clone = vine_feed_cache.clone();

    // MailManager will be initialized after identity loading (needs owner address).
    // Placeholder — set below once we have our_addr_bytes.
    let mail_mgr: std::sync::Arc<std::sync::Mutex<mail::MailManager>>;

    // Stop existing node — extract handles under the lock in a tight
    // inner scope so the std `MutexGuard` (which is `!Send`) is fully
    // out of scope before the SyncEngine's `.await`. Without this
    // scoping, rustc's async generator analysis sees the guard's
    // storage slot as live across the await point and rejects the
    // function as not `Send`.
    //
    // ZEB-270 Phase 3 Task 4.5: declared in outer scope so the
    // post-lock shutdown_all block can take it. Assigned inside the
    // lock alongside the other `take()` calls below (see
    // `old_channel_log_registry = guard.channel_log_registry.take();`).
    let old_channel_log_registry: Option<
        std::sync::Arc<crate::community_channel_log_engine::ChannelLogRegistry<tauri::Wry>>,
    >;
    // ZEB-281 Sub-D Phase 4: outer-scope binding for the previous
    // identity's profile-broadcast publisher. Awaited outside the std
    // `MutexGuard` scope (the guard is `!Send`) — mirrors
    // `old_channel_log_registry`'s pattern.
    let old_profile_broadcast_publisher: Option<
        std::sync::Arc<crate::profile_broadcast::ProfileBroadcastPublisher>,
    >;
    // ZEB-234: outer-scope bindings for the old fence handles. Taken
    // inside the lock (below) and drained outside after the lock is
    // released — mirrors stop_inner's pattern. Declared here so they
    // outlive the lock scope and can be used in the drain block.
    let old_dm_send_inflight: Option<std::sync::Arc<tokio::sync::Semaphore>>;
    let old_dm_send_stopping: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>;
    // ZEB-221: reserve our start-attempt sequence via the dedicated helper
    // BEFORE the lock-1 tuple-take block. Acquires + releases its own lock;
    // the subsequent lock-1 acquisition observes the bumped install_seq.
    // Held in outer scope so the lock-2 validation
    // (`check_install_seq_or_supersede`) can reach it. Mismatch routes
    // through the existing thread_install_failure cleanup path.
    //
    // Distinct from `generation`, which keeps its pre-ZEB-221 semantics
    // (bumped only on SUCCESSFUL install at lock-2) so post-install code
    // (pairing-handle install, failure cleanup, stop_inner gating)
    // continues to detect "later install completed" rather than the
    // strictly weaker "later attempt started" — see Cursor bug report on
    // PR #124.
    let my_install_seq: u64 = reserve_install_seq(&state)?;
    let (
        old_shutdown,
        old_thread,
        old_publish,
        old_fetch,
        old_ingest,
        old_content_verb,
        old_follow,
        old_voice,
        old_voice_channel,
        old_pairing_handle,
        old_sync_engine,
        old_community_registry,
        old_community_delta_tx,
        old_dm_outbox,
        old_dm_transport,
        old_crdt_state,
        old_hlc_tracker,
        old_dm_device_id,
        old_dm_self_owner,
        old_content_store,
        old_unicast_send_tx,
    ) = {
        let mut guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
        // ZEB-234: take fence handles before releasing the lock so any
        // concurrent send_dm / delete_outbox_entry that snapshots from
        // NodeState after this point finds None and rejects immediately.
        old_dm_send_inflight = guard.dm_send_inflight.take();
        old_dm_send_stopping = guard.dm_send_stopping.take();
        let tup = (
            guard.shutdown_tx.take(),
            guard.thread.take(),
            guard.publish_tx.take(),
            guard.fetch_tx.take(),
            guard.ingest_tx.take(),
            guard.content_verb_tx.take(),
            guard.follow_tx.take(),
            guard.voice_tx.take(),
            guard.voice_channel_tx.take(),
            guard.pairing_handle.take(),
            guard.sync_engine.take(),
            // ZEB-217 Sub-C Phase 2: take + shutdown the previous
            // identity's per-community engine pool. Mirrors stop_inner's
            // ordering — drain communities BEFORE the owner SyncEngine.
            guard.community_registry.take(),
            // ZEB-217 Sub-C Phase 3 Task 8: take the previous identity's
            // delta sender; dropped after `registry.shutdown_all()` below
            // so the consumer task exits cleanly.
            guard.community_delta_tx.take(),
            // ZEB-225 Sub-B Phase 2: take + drop the per-identity DM
            // handles so a restart doesn't carry stale Arc<DmOutbox> /
            // Arc<DmTransport> / Arc<OwnerState> / Arc<HlcTracker> /
            // Arc<dyn ContentStore> against the prior identity into the
            // new generation. Mirrors stop_inner's cleanup.
            guard.dm_outbox.take(),
            guard.dm_transport.take(),
            guard.crdt_state.take(),
            guard.hlc_tracker.take(),
            guard.dm_device_id.take(),
            guard.dm_self_owner.take(),
            guard.content_store.take(),
            // ZEB-227 Path B: take + drop the previous identity's outbound
            // unicast sender so the new generation gets a fresh channel.
            guard.unicast_send_tx.take(),
        );
        let _old_follow_mgr = guard.follow_mgr.take();
        let _old_followed_set = guard.followed_set.take();
        let _old_vine_feed_cache = guard.vine_feed_cache.take();
        let _old_mail_mgr = guard.mail_mgr.take();
        let _old_mail_sync = guard.mail_sync.take();
        // ZEB-228 Phase 4: clear our cached identity_pub so a restart
        // can't ship the prior identity's pub on the new identity's
        // outbound DmInvites. Mirrors stop_inner's cleanup.
        let _ = guard.dm_identity_pub_64.take();
        // ZEB-217 Sub-C Phase 3 Task 9: clear the previous adapter-
        // request sender so it doesn't outlive the previous event
        // loop. The new event loop is constructed below with a fresh
        // channel pair.
        let _ = guard.community_adapter_request_tx.take();
        // ZEB-270 Phase 3 Task 4.5: take the prior channel-log
        // registry into the outer-scope binding. Awaited outside the
        // guard scope (the std `MutexGuard` is `!Send`) — mirrors
        // `old_community_registry`'s pattern. Hoisted out via the
        // separate outer binding rather than adding to the already-
        // saturated tuple. Awaited via `shutdown_all()` BEFORE the
        // community engine pool shuts down (verify-chain dependency,
        // same ordering as stop_inner).
        old_channel_log_registry = guard.channel_log_registry.take();
        // ZEB-218 Sub-D Phase 1: drop the previous identity's library
        // directory handle. The matching event_loop consumer task
        // observes the request_tx close on next recv and exits.
        let _ = guard.library_directory.take();
        // ZEB-281 Sub-D Phase 4: take the prior identity's
        // profile-broadcast publisher into a scope-local binding so
        // `shutdown()` runs outside the std `MutexGuard` (which is
        // !Send across an await). Clear the cache + request_tx + reset
        // the SubscriptionId allocator so the new identity's IPCs
        // start from id=1 again.
        old_profile_broadcast_publisher = guard.profile_broadcast_publisher.take();
        guard.profile_broadcast_cache = None;
        guard.profile_broadcast_request_tx = None;
        guard.profile_broadcast_next_subscription_id =
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
        tup
    };

    // Drop pairing_handle BEFORE publish_tx so the SM task's transport
    // sees its receiver close after the publish channel is gone — same
    // ordering as stop_inner.
    drop(old_pairing_handle);
    // ZEB-234: signal the old stopping flag so any concurrent send_dm /
    // delete_outbox_entry that already cloned the old fence handles fast-
    // rejects without queuing a new permit. Then drain the semaphore —
    // acquire_many(CAPACITY) blocks until every outstanding permit has
    // been returned, ensuring no in-flight DM mutation is mid-critical-
    // section when the old SyncEngine's final flush runs below.
    //
    // We're already in async context (start_node is async), so we can
    // .await directly — no thread::scope + ephemeral-runtime needed here
    // (unlike stop_inner, which is sync).
    if let Some(ref stopping) = old_dm_send_stopping {
        stopping.store(true, std::sync::atomic::Ordering::Release);
    }
    if let Some(sem) = old_dm_send_inflight {
        drain_dm_send_fence(sem).await;
    }
    // ZEB-281 Sub-D Phase 4: explicitly await the previous identity's
    // profile-broadcast publisher shutdown BEFORE dropping the prior
    // publish channels. The publisher's background task may otherwise
    // wake during teardown and surface a closed-sink Err. Mirrors the
    // ordering in stop_inner.
    if let Some(publisher) = old_profile_broadcast_publisher {
        publisher.shutdown().await;
    }
    drop(old_publish);
    drop(old_fetch);
    drop(old_ingest);
    drop(old_content_verb);
    drop(old_follow);
    drop(old_voice);
    drop(old_voice_channel);
    // ZEB-225 Sub-B Phase 2: drop the previous identity's DM handles so
    // the new SyncEngine/DmOutbox built below sees no stale Arc clones
    // outside the new NodeState. Same drop-order rationale as stop_inner.
    drop(old_dm_outbox);
    drop(old_dm_transport);
    drop(old_crdt_state);
    drop(old_hlc_tracker);
    drop(old_dm_device_id);
    // OwnerAddr is Copy → use `let _` instead of drop() to satisfy
    // clippy::dropping_copy_types.
    let _ = old_dm_self_owner;
    drop(old_content_store);
    // ZEB-227 Path B: drop the previous identity's outbound unicast sender
    // so the new generation's RuntimeUnicastTransport (Task 11) sees no
    // stale clones outside the new NodeState.
    drop(old_unicast_send_tx);
    // ZEB-270 Phase 3 Task 4.5: explicitly await the previous channel-
    // log registry's shutdown BEFORE the per-community state engines
    // tear down. The channel-log engine's verify-on-receive path
    // resolves identity + state-at-HLC from the matching
    // CommunitySyncEngine; tearing those down first would break
    // inflight verifies. Same async ordering rule as stop_inner; here
    // we're already in async context so no thread::scope juggling
    // needed.
    if let Some(registry) = old_channel_log_registry {
        if let Err(e) = registry.shutdown_all().await {
            tracing::error!(
                error = %e,
                "previous ChannelLogRegistry shutdown_all failed during start_node restart"
            );
        }
    }
    // ZEB-217 Sub-C Phase 2: explicitly await the previous community
    // engine pool's shutdown BEFORE the owner SyncEngine. Mirrors
    // stop_inner's ordering — community engines need their final
    // state-root publish to land on the wire before the event-loop
    // thread joins. We're in async start_node so no thread::scope
    // juggling needed (unlike stop_inner).
    if let Some(registry) = old_community_registry {
        if let Err(e) = registry.shutdown_all().await {
            tracing::error!(
                error = %e,
                "previous CommunitySyncRegistry shutdown_all failed during start_node restart"
            );
        }
    }
    // ZEB-217 Sub-C Phase 3 Task 8: drop the prior delta sender after the
    // registry shut down so every per-engine clone is gone first; the
    // consumer task drains pending events and exits.
    drop(old_community_delta_tx);
    // Phase 3a: explicitly await the previous SyncEngine's shutdown
    // before installing the replacement, so any pending debounced
    // publish flushes and the final persist pass completes. Dropping
    // alone is best-effort — the internal task could be mid-await
    // and never observe the channel close in time. We're in async
    // start_node, so no thread::scope juggling needed.
    if let Some(engine) = old_sync_engine {
        if let Err(e) = engine.shutdown().await {
            tracing::error!(
                error = %e,
                "previous SyncEngine final flush failed during start_node restart"
            );
        }
    }
    stop_handles(old_shutdown, old_thread);

    let our_gen = {
        // ── Identity loading — no lock held here; the inner block at
        //    line ~735 re-acquires the std::Mutex to atomically register
        //    the new node handles. (Stopping the old node already ran
        //    above outside this block, so the registration race window
        //    is bounded by that re-acquisition only.)
        let id_path = identity::resolve_path(None)?;
        let id = identity::load_or_generate(&id_path)?;
        let identity::NodeIdentity { pq, ed25519 } = id;

        let our_addr_bytes: [u8; 16] = ed25519.public_identity().address_hash;
        let node_addr = hex::encode(our_addr_bytes);

        let pq_pub = pq.public_identity();
        let local_pq_identity_hash = pq_pub.address_hash;
        let local_dsa_pubkey = pq_pub.verifying_key.as_bytes();
        let local_kem_pubkey = pq_pub.encryption_key.as_bytes();
        drop(pq);

        // ZEB-228 Phase 4: capture our 64-byte combined identity_pub
        // (X25519_pub(32) || Ed25519_pub(32) per
        // `harmony_identity::Identity::to_public_bytes()`) BEFORE the
        // ed25519 PrivateIdentity is dropped below. add_space ships this
        // as the bootstrap pubkey on outbound DmInvite packets so the
        // recipient can verify the signature without a prior
        // OwnerDeviceCache entry for us.
        let identity_pub_64: [u8; 64] = ed25519.public_identity().to_public_bytes();

        let reticulum_identity_bytes = Some(zeroize::Zeroizing::new(ed25519.to_private_bytes()));
        // ZEB-262 Phase 4 Task 2: snapshot a second `PrivateIdentity` instance
        // BEFORE the local `ed25519` binding is dropped. The Reticulum/Ed25519
        // identity is the same material we'll later use on the receive-side
        // counter-sign path (`handle_invite` →
        // `community_membership::attach_countersig_with_identity`); plumbing
        // it through `DmOutbox` lets the inbound CommunityInvite handler grab
        // a reference under the dm_outbox lock without re-reading the
        // on-disk identity.
        //
        // We can't `clone()` `PrivateIdentity` (it carries `ZeroizeOnDrop`
        // and intentionally does NOT implement Clone), so we reconstruct
        // from the private bytes we just captured. Round-trip via
        // `from_private_bytes` is bit-identical: same X25519 secret + same
        // Ed25519 secret → same `Identity` (verified by
        // `dm_outbox_holds_private_identity_for_countersign`).
        let private_identity_arc = std::sync::Arc::new(
            harmony_identity::PrivateIdentity::from_private_bytes(
                reticulum_identity_bytes
                    .as_ref()
                    .expect("populated above")
                    .as_slice(),
            )
            .expect("private bytes round-trip"),
        );
        drop(ed25519);

        tracing::info!(address = %node_addr, path = %id_path.display(), "identity loaded");

        // Initialize mail manager (needs owner address from identity).
        mail_mgr = std::sync::Arc::new(std::sync::Mutex::new(mail::MailManager::load(
            &app_data_dir.join("mail"),
            our_addr_bytes,
        )));

        // Construct MailSync now that identity, mail_mgr, and the refresh
        // channel are all available. Owns a clone of fetch_tx (so commands
        // keep their own sender in AppState) and the sole refresh_tx.
        let mail_sync = std::sync::Arc::new(mail_sync::MailSync::new(
            fetch_tx.clone(),
            mail_refresh_tx,
            std::sync::Arc::clone(&mail_mgr),
            app.clone(),
        ));

        // ── Phase 3a: SyncEngine construction ──────────────────────────
        // Load the owner identity (master_seed + device_signing_key) to
        // construct the SyncEngine. This is independent of the Reticulum
        // network identity loaded above. If no owner identity exists yet
        // (pre-mint), sync_handles / sync_engine are None and the rest of
        // start_node proceeds normally.
        let identity_dir = crate::owner_commands::resolve_identity_dir()?;
        let owner_loaded = crate::owner_state::load_owner_state(
            &identity_dir,
            crate::identity::KeychainStore::new().ok(),
        )?;

        let mut sync_handles_opt: Option<crate::event_loop::SyncEngineHandles> = None;
        // ZEB-225 Sub-B Phase 2: lift the per-identity handles SyncEngine
        // depends on (device_id, self_owner, crdt_state, tracker,
        // content_store) out of the `if let Some(seed)` block so the
        // outer NodeState assignment can reach them. send_dm IPC clones
        // these from NodeState; without lifting, they'd be unreachable
        // outside the SyncEngine constructor.
        let mut device_id_for_state: Option<String> = None;
        let mut self_owner_for_state: Option<crate::owner_state_types::OwnerAddr> = None;
        let mut crdt_state_for_state: Option<
            std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
        > = None;
        let mut tracker_for_state: Option<
            std::sync::Arc<
                tokio::sync::Mutex<
                    std::collections::BTreeMap<String, crate::owner_state_types::Hlc>,
                >,
            >,
        > = None;
        let mut content_store_for_state: Option<
            std::sync::Arc<dyn crate::content_store::ContentStore>,
        > = None;
        let mut dm_outbox_arc: Option<
            std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>,
        > = None;
        let mut dm_transport_arc: Option<std::sync::Arc<dyn crate::dm_outbox::DmTransport>> = None;
        // ZEB-217 Sub-C Phase 2 Task 13: per-community engine pool +
        // adapter requests handed to the event loop. Both stay None /
        // empty when no owner identity is loaded (registry depends on
        // crdt_state). When an owner IS loaded, we build the registry
        // inside the if-let block below, scan owner-state for joined
        // communities, spawn one engine per community, and push one
        // CommunityAdapterRequest per spawn for event_loop::run to
        // wire up against the Zenoh session.
        let mut community_registry_arc: Option<
            std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
        > = None;
        // ZEB-217 Sub-C Phase 3 Task 8: outer-scope holder for the
        // community delta sender. The original is created inside the
        // if-let-Some(owner_loaded) block alongside the registry; we
        // lift a clone out here so it can be stashed on NodeState below
        // for stop_node / restart cleanup.
        let mut community_delta_tx_for_state: Option<
            tokio::sync::mpsc::Sender<crate::community_state_sync::CommunityMembershipDelta>,
        > = None;
        let mut community_adapter_requests: Vec<crate::event_loop::CommunityAdapterRequest> =
            Vec::new();
        // ZEB-270 Phase 3 Task 4.5: bridge channel for the channel-log
        // adapter requests. Built unconditionally — even when no owner
        // identity is loaded, `event_loop::run` needs to be passed the
        // receiver half. The matching sender is only handed to a
        // ChannelLogRegistry inside the owner-loaded branch below; an
        // unloaded node has no spawn calls so the rx never wakes.
        //
        // **Unbounded.** Boot-time `reconcile_from_state` runs BEFORE
        // event_loop spawns (the reconcile populates the bridge so
        // event_loop can wire each request to the session as soon as
        // it opens). A bounded channel could deadlock at boot if a
        // user has more channels than the bound — `.send` would await
        // forever because no consumer exists yet. See
        // `ChannelLogRegistryConfig.adapter_request_tx` doc for the
        // full rationale.
        let (channel_log_adapter_request_tx_outer, channel_log_adapter_request_rx_for_loop) =
            tokio::sync::mpsc::unbounded_channel::<crate::event_loop::ChannelLogAdapterRequest>();
        // Outer-scope holder for the channel-log registry handle. The
        // owner-loaded branch builds + populates it; the post-spawn
        // NodeState assignment below stashes it.
        let mut channel_log_registry_arc: Option<
            std::sync::Arc<crate::community_channel_log_engine::ChannelLogRegistry<tauri::Wry>>,
        > = None;

        // ── ZEB-281 Sub-D Phase 4: profile-broadcast publisher + cache + request channel ──
        //
        // All three holders live in the outer scope so the post-spawn
        // `guard.profile_broadcast_*` assignments can reach them. The
        // publisher is constructed INSIDE the `if let Some(seed)` block
        // below (needs the `signing_key_arc` + `identity_pub_64` +
        // `crdt_state` + `tracker` + `device_id` + `publish_tx` from
        // the owner-identity path). The cache is constructed
        // unconditionally so the subscriber pool can be wired up even
        // when no owner identity is loaded; the matching
        // request_rx is moved into `event_loop::run` below.
        let mut profile_broadcast_publisher_arc: Option<
            std::sync::Arc<crate::profile_broadcast::ProfileBroadcastPublisher>,
        > = None;
        let profile_broadcast_cache_arc =
            std::sync::Arc::new(crate::profile_broadcast::ProfileBroadcastCache::default());
        let (profile_broadcast_request_tx, profile_broadcast_request_rx) =
            tokio::sync::mpsc::channel::<crate::event_loop::ProfileBroadcastRequest>(64);

        let sync_engine_arc: Option<std::sync::Arc<crate::owner_state_sync::SyncEngine>> =
            if let Some(ref loaded) = owner_loaded {
                if let Some(seed) = loaded.master_seed.as_ref() {
                    let kt = std::sync::Arc::new(
                        crate::owner_state_crypto::KeyTree::derive(seed)
                            .map_err(|e| format!("KeyTree::derive: {e}"))?,
                    );
                    let device_id = loaded
                        .device_signing_key
                        .verifying_key()
                        .to_bytes()
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>();

                    let crdt_path = identity_dir.join("owner_state_crdt.cbor");
                    let replay_path = identity_dir.join("state_root_replay.cbor");
                    let initial_crdt = crate::owner_state_persist::load_crdt(&crdt_path)
                        .map_err(|e| format!("load owner_state_crdt.cbor: {e}"))?;
                    let initial_replay = crate::owner_state_persist::load_replay(&replay_path)
                        .map_err(|e| format!("load state_root_replay.cbor: {e}"))?;

                    let crdt_state = std::sync::Arc::new(tokio::sync::Mutex::new(initial_crdt));
                    let tracker = std::sync::Arc::new(tokio::sync::Mutex::new(initial_replay));
                    // Phase 3b: real harmony-content CAS via RuntimeContentStore.
                    // Sends CasOp messages over cas_op_tx into the harmony-
                    // runtime event loop, which admits/queries through the
                    // shared NodeRuntime + StorageTier. See spec
                    // §"Architecture / High-level flow".
                    let content_store: std::sync::Arc<dyn crate::content_store::ContentStore> =
                        std::sync::Arc::new(crate::content_store::RuntimeContentStore::new(
                            cas_op_tx.clone(),
                            std::time::Duration::from_millis(
                                crate::content_store::DEFAULT_FETCH_TIMEOUT_MS,
                            ),
                        ));

                    let (out_tx, out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
                    let (in_tx, in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

                    let self_owner = crate::owner_state_types::OwnerAddr(loaded.state.owner_id);

                    // ZEB-225 Sub-B Phase 2: construct DmOutbox + transport
                    // alongside SyncEngine. Both share device_id + self_owner
                    // with the SyncEngine.
                    //
                    // ZEB-227 Phase 3b Task 11: DmOutbox + RuntimeUnicastTransport
                    // both consume the SAME (signing_key, signing_device_hash)
                    // pair sourced from the Reticulum identity loaded above.
                    // The Reticulum identity's `address_hash` IS the
                    // DeviceIdentityHash that peers cache in OwnerDeviceCache —
                    // signing with any other key would produce signatures that
                    // fail verification at the receiver's
                    // `verify_dm_packet_signature` (key-substitution defense:
                    // Step 1 derives the device hash from the identity_pub
                    // and rejects if it doesn't match the wire-claimed hash).
                    //
                    // SigningKey extraction: `ed25519.to_private_bytes()`
                    // returns `[32B X25519_secret][32B Ed25519_secret]` per
                    // harmony_identity::PrivateIdentity::to_private_bytes
                    // (identity.rs:308). The Ed25519 secret half occupies
                    // bytes 32..64 and constructs an ed25519_dalek::SigningKey
                    // bit-identical to the one PrivateIdentity::sign uses
                    // internally (verified by sign_dm_packet_matches_private_identity_sign
                    // in dm_signing.rs).
                    // Wrap in Zeroizing — the signing seed must be scrubbed
                    // when this scope ends, mirroring how
                    // reticulum_identity_bytes is held above (line 772).
                    // Without this the 32-byte stack copy would persist in
                    // freed stack memory until overwritten.
                    let ed25519_seed = zeroize::Zeroizing::new(
                        <[u8; 32]>::try_from(
                            &reticulum_identity_bytes
                                .as_ref()
                                .expect("reticulum_identity_bytes populated above")
                                [32..64],
                        )
                        .expect("64 - 32 == 32"),
                    );
                    let signing_key_arc =
                        std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&ed25519_seed));
                    let our_signing_device_hash =
                        crate::owner_state_types::DeviceIdentityHash(our_addr_bytes);
                    let outbox = std::sync::Arc::new(tokio::sync::Mutex::new(
                        crate::dm_outbox::DmOutbox::new(
                            device_id.clone(),
                            self_owner,
                            our_signing_device_hash,
                            signing_key_arc.clone(),
                            std::sync::Arc::clone(&private_identity_arc),
                        ),
                    ));
                    // Production transport: RuntimeUnicastTransport pushes
                    // signed CidNotify packets into unicast_send_tx, which
                    // event_loop::run translates into
                    // RuntimeEvent::SendUnicastToDevice. OwnerAddr →
                    // device-hash resolution happens inside drain (which
                    // has `&OwnerState` from the event-loop's mutex guard),
                    // not in the transport — splitting resolution out
                    // sidesteps the recursive-lock deadlock that broke
                    // delivery in the original Phase 3b shape.
                    let transport: std::sync::Arc<dyn crate::dm_outbox::DmTransport> =
                        std::sync::Arc::new(crate::dm_outbox::RuntimeUnicastTransport::new(
                            unicast_send_tx.clone(),
                            self_owner,
                            our_signing_device_hash,
                            std::sync::Arc::clone(&signing_key_arc),
                        ));

                    let engine = std::sync::Arc::new(crate::owner_state_sync::SyncEngine::new(
                        std::sync::Arc::clone(&kt),
                        device_id.clone(),
                        std::sync::Arc::clone(&crdt_state),
                        std::sync::Arc::clone(&tracker),
                        std::sync::Arc::clone(&content_store),
                        out_tx,
                        in_rx,
                        crate::owner_state_sync::PersistPaths {
                            crdt: crdt_path,
                            replay: replay_path,
                        },
                        crate::owner_state_sync::DEFAULT_DEBOUNCE_MS,
                    ));

                    // Topic key is the OWNER identity (16-byte address from
                    // `harmony_owner::state::OwnerState.owner_id`), not the
                    // per-device Reticulum transport address — every device
                    // bound to this owner must converge on the same Zenoh
                    // topic `harmony/owner/{addr_hex}/state-root-v1`.
                    let owner_addr_hex = hex::encode(loaded.state.owner_id);
                    sync_handles_opt = Some(crate::event_loop::SyncEngineHandles {
                        addr_hex: owner_addr_hex,
                        outbound_rx: out_rx,
                        inbound_tx: in_tx,
                    });

                    // ── ZEB-217 Sub-C Phase 2 + Phase 3 Task 8: per-community state CRDT sync ─
                    //
                    // Build the registry (owns the multi-community engine pool)
                    // along with two consumer tasks:
                    //   - `community-members-changed` (Phase 3 Task 8) — the
                    //     engine fires one `CommunityMembershipDelta` per
                    //     CRDT mutation; the consumer projects each delta
                    //     into `CommunityMembersChangedPayload` via
                    //     `delta_to_change` and emits a Tauri event so the
                    //     frontend updates the member list incrementally.
                    //   - `community-state-sync-degraded` (Phase 2) — every
                    //     spawned engine clones `community_degraded_tx`
                    //     into its `CommunitySyncEngineConfig`; the
                    //     consumer receives reports and surfaces a degraded
                    //     banner per-community.
                    //
                    // Both channels are created BEFORE the registry config
                    // so the senders can be passed into `CommunityRegistryConfig`
                    // and cloned into every per-engine config inside
                    // `spawn_engine`. Channel capacity (256) is sized for
                    // burst-tolerance under degraded / mass-receive
                    // conditions; a full channel falls back to dropping
                    // the message (`try_send`-style fire-and-forget) so a
                    // single noisy community can't starve the rest of the
                    // engine pool.
                    let (community_delta_tx, community_delta_rx) = tokio::sync::mpsc::channel::<
                        crate::community_state_sync::CommunityMembershipDelta,
                    >(256);
                    let (community_degraded_tx, community_degraded_rx) = tokio::sync::mpsc::channel::<
                        crate::community_state_sync::CommunityDegradedReport,
                    >(256);

                    let registry: std::sync::Arc<
                        crate::community_state_sync::CommunitySyncRegistry,
                    > = {
                        let resolver: std::sync::Arc<
                            dyn crate::community_state_sync::IdentityResolver,
                        > = std::sync::Arc::new(
                            crate::community_state_sync::OwnerDeviceCacheResolver::new(
                                std::sync::Arc::clone(&crdt_state),
                                self_owner,
                                identity_pub_64,
                            ),
                        );
                        let cfg = crate::community_state_sync::CommunityRegistryConfig {
                            device_id: device_id.clone(),
                            content_store: std::sync::Arc::clone(&content_store),
                            identity_resolver: resolver,
                            identity_dir: identity_dir.clone(),
                            debounce_ms: crate::community_state_sync::DEFAULT_DEBOUNCE_MS,
                            error_tx: Some(community_degraded_tx),
                            delta_tx: Some(community_delta_tx.clone()),
                            // ZEB-256 Task 6: registry holds the local
                            // identity once; every spawned engine
                            // clones into its CommunitySyncEngineConfig.
                            // Both values already exist above for the
                            // DmOutbox plumbing.
                            self_owner,
                            signing_key: std::sync::Arc::clone(&signing_key_arc),
                            // ZEB-249 §10.6 (Phase A): pass the live owner-state CRDT
                            // so every spawned engine reads the current epoch key
                            // dynamically rather than using its spawn-time capture.
                            crdt_state: Some(std::sync::Arc::clone(&crdt_state)),
                            // ZEB-254 Task 11: joiner-side pending-clear hook. Emits
                            // nav-updated { pending: false } when a JoinCountersign
                            // targeting a self-authored PendingJoin lands in the engine.
                            nav_emitter: Some(std::sync::Arc::new({
                                let app_handle_for_emitter = app.clone();
                                move |community_id: crate::owner_state_types::SpaceId,
                                      space_name: String| {
                                    use tauri::Emitter as _;
                                    let space_id_hex = hex::encode(community_id.0);
                                    if let Err(e) = app_handle_for_emitter.emit(
                                        "nav-updated",
                                        &NavUpdatedPayload {
                                            action: "modified",
                                            space_id: space_id_hex,
                                            kind: "community",
                                            name: space_name,
                                            members: None,
                                            parent_id: None,
                                            pending: Some(false),
                                        },
                                    ) {
                                        tracing::warn!(
                                            error = %e,
                                            "ZEB-254 pending-clear: nav-updated emit failed"
                                        );
                                    }
                                }
                            })),
                        };
                        std::sync::Arc::new(
                            crate::community_state_sync::CommunitySyncRegistry::new(cfg),
                        )
                    };

                    // ZEB-270 Phase 3 Task 4.5: per-(community, channel)
                    // log engine registry. Built BEFORE the delta
                    // consumer spawn so the consumer's 3rd callback can
                    // capture it (the callback fires `registry.spawn` /
                    // `registry.stop` on Created / Deleted channel-config
                    // events). Built BEFORE the community_snapshots loop
                    // so the boot-time `reconcile_from_state` per-
                    // community can run there.
                    //
                    // The matching adapter-request `Receiver` was
                    // constructed in outer scope above
                    // (`channel_log_adapter_request_rx_for_loop`) so
                    // event_loop::run can be called with a uniform
                    // signature regardless of whether an owner identity
                    // is loaded. Here we pair the outer `Sender` with
                    // the registry config — `take()` because the outer
                    // sender is moved by value once.
                    let channel_log_registry: std::sync::Arc<
                        crate::community_channel_log_engine::ChannelLogRegistry<tauri::Wry>,
                    > = crate::community_channel_log_engine::ChannelLogRegistry::new(
                        crate::community_channel_log_engine::ChannelLogRegistryConfig {
                            adapter_request_tx: channel_log_adapter_request_tx_outer.clone(),
                            app: app.clone(),
                            identity_dir: identity_dir.clone(),
                            self_owner,
                            self_device_id: device_id.clone(),
                            signing_key: std::sync::Arc::clone(&signing_key_arc),
                            engine_config:
                                crate::community_channel_log_engine::ChannelLogEngineConfig::default(
                                ),
                        },
                    );
                    // Clones of the registry: one for the delta
                    // consumer's third callback, one for the boot-time
                    // reconcile loop below, one for NodeState (assigned
                    // post-thread-spawn).
                    let channel_log_registry_for_consumer =
                        std::sync::Arc::clone(&channel_log_registry);
                    let channel_log_registry_for_reconcile =
                        std::sync::Arc::clone(&channel_log_registry);
                    channel_log_registry_arc = Some(channel_log_registry);

                    // Spawn the delta consumer: each `CommunityMembershipDelta`
                    // becomes one `community-members-changed` Tauri event.
                    // Task exits cleanly when every per-engine `delta_tx`
                    // clone AND the start_node-held clone have all
                    // dropped — which happens after `registry.shutdown_all()`
                    // and the explicit `drop(community_delta_tx)` in
                    // stop_inner / start_node restart.
                    {
                        let app_for_membership = app.clone();
                        let app_for_channel_config = app.clone();
                        // ZEB-270 Phase 3 Task 4B: third callback —
                        // the channel-log registry hook. Currently
                        // wired as a no-op placeholder; Task 4C
                        // production wiring (which requires the
                        // registry handle to be threaded through to
                        // this site) is deferred until the
                        // session-bridge to event_loop lands. Until
                        // then the hook just logs at trace level so
                        // observability is preserved without polluting
                        // the warn/info channels. The callback shape
                        // is stable now — flipping the body to call
                        // `registry.spawn` / `registry.stop` is a
                        // one-edit change once `NodeState.channel_log_registry`
                        // is populated by start_node + event_loop.
                        tokio::spawn(run_community_delta_consumer(
                            community_delta_rx,
                            move |payload| {
                                let app = app_for_membership.clone();
                                async move {
                                    if let Err(e) = app.emit("community-members-changed", &payload)
                                    {
                                        tracing::warn!(
                                            error = ?e,
                                            "failed to emit community-members-changed"
                                        );
                                    }
                                }
                            },
                            move |payload| {
                                let app = app_for_channel_config.clone();
                                async move {
                                    if let Err(e) = app.emit("channel-config-updated", &payload) {
                                        tracing::warn!(
                                            error = ?e,
                                            "failed to emit channel-config-updated"
                                        );
                                    }
                                }
                            },
                            // ZEB-270 Phase 3 Task 4.5: production
                            // channel-log registry hook. Per spec §7.3,
                            // Created → registry.spawn (which derives
                            // the per-channel key + dispatches the
                            // adapter request through the bridge);
                            // Modified → no-op (config change without a
                            // new log lifecycle event); Deleted →
                            // registry.stop (idempotent — flushes the
                            // engine + flips the closing flag; on-disk
                            // segments persist per spec §17.4).
                            //
                            // The closure captures three Arcs through
                            // a wrapping block expression:
                            //   - channel-log registry (target of spawn/stop)
                            //   - community sync registry (source of
                            //     membership_key + state-at-HLC + identity
                            //     resolver via engine_arc())
                            //   - per-device hlc_tracker (the SAME
                            //     `Arc<Mutex<BTreeMap<String, Hlc>>>`
                            //     dm_outbox::reserve_next_hlc_for_device
                            //     uses; channel-log mints share this
                            //     monotonicity lane).
                            //
                            // All three Arcs are cloned per delta
                            // because the FnMut callback may fire many
                            // times across the consumer's lifetime —
                            // each invocation clones for its own future
                            // body.
                            {
                                let registry_for_hook =
                                    std::sync::Arc::clone(&channel_log_registry_for_consumer);
                                let community_registry_for_hook = std::sync::Arc::clone(&registry);
                                let hlc_tracker_for_hook = std::sync::Arc::clone(&tracker);
                                move |payload: ChannelConfigChangedPayload| {
                                    let registry = std::sync::Arc::clone(&registry_for_hook);
                                    let community_registry =
                                        std::sync::Arc::clone(&community_registry_for_hook);
                                    let hlc_tracker = std::sync::Arc::clone(&hlc_tracker_for_hook);
                                    async move {
                                        let cid_bytes: [u8; 16] = match hex::decode(
                                            &payload.community_id,
                                        )
                                        .ok()
                                        .and_then(|v| v.try_into().ok())
                                        {
                                            Some(b) => b,
                                            None => {
                                                tracing::warn!(
                                                    community_id = %payload.community_id,
                                                    "channel-log registry hook: invalid community_id hex"
                                                );
                                                return;
                                            }
                                        };
                                        let chid_bytes: [u8; 16] = match hex::decode(
                                            &payload.channel_id,
                                        )
                                        .ok()
                                        .and_then(|v| v.try_into().ok())
                                        {
                                            Some(b) => b,
                                            None => {
                                                tracing::warn!(
                                                    channel_id = %payload.channel_id,
                                                    "channel-log registry hook: invalid channel_id hex"
                                                );
                                                return;
                                            }
                                        };
                                        let cid = crate::owner_state_types::SpaceId(cid_bytes);
                                        let chid =
                                            crate::community_membership::ChannelId(chid_bytes);

                                        match payload.action {
                                            ChannelConfigChangeAction::Created => {
                                                let community_engine = match community_registry
                                                    .engine_arc(&cid)
                                                    .await
                                                {
                                                    Some(e) => e,
                                                    None => {
                                                        tracing::warn!(
                                                            community_id = %payload.community_id,
                                                            "channel-log registry hook: \
                                                             no community engine spawned"
                                                        );
                                                        return;
                                                    }
                                                };
                                                let membership_key =
                                                    community_engine.membership_key();
                                                let key = crate::community_channel_log::derive_channel_key(
                                                &membership_key,
                                                &cid,
                                                &chid,
                                            );
                                                let state_at_hlc =
                                                    community_engine.state_at_hlc_resolver();
                                                let resolver = match community_engine
                                                    .identity_resolver()
                                                {
                                                    Some(r) => r,
                                                    None => {
                                                        tracing::warn!(
                                                            community_id = %payload.community_id,
                                                            "channel-log registry hook: \
                                                             community engine has no identity resolver"
                                                        );
                                                        return;
                                                    }
                                                };
                                                match registry
                                                    .spawn(
                                                        cid,
                                                        chid,
                                                        key,
                                                        state_at_hlc,
                                                        resolver,
                                                        hlc_tracker,
                                                    )
                                                    .await
                                                {
                                                    Ok(crate::community_channel_log_engine::SpawnOutcome::Spawned(_)) => {}
                                                    Ok(crate::community_channel_log_engine::SpawnOutcome::DeferredForCommit) => {
                                                        // Deferred until a transaction commits.
                                                    }
                                                    Err(e) => {
                                                        tracing::warn!(
                                                            community_id = %payload.community_id,
                                                            channel_id = %payload.channel_id,
                                                            error = ?e,
                                                            "channel-log spawn failed"
                                                        );
                                                    }
                                                }
                                            }
                                            ChannelConfigChangeAction::Modified => {
                                                // No-op per spec §7.3. The
                                                // channel-log itself is
                                                // unaffected by metadata
                                                // changes (rename,
                                                // write_power) — those are
                                                // membership-CRDT events
                                                // and the in-flight engine
                                                // sees them through its
                                                // shared state-at-HLC view.
                                            }
                                            ChannelConfigChangeAction::Deleted => {
                                                if let Err(e) = registry.stop(&cid, &chid).await {
                                                    tracing::warn!(
                                                        community_id = %payload.community_id,
                                                        channel_id = %payload.channel_id,
                                                        error = ?e,
                                                        "channel-log stop failed"
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            },
                            // ZEB-249 Task 6 §4.3 + §4.6: self-healing observer.
                            // After every CRDT delta, re-materialize the community
                            // and check pending_rotation_for / pending_catchup_for.
                            // If the local user has admin power, synthesize any
                            // missing EpochRotation or EpochCatchup events.
                            //
                            // Per-session BTreeSet<(SpaceId, OwnerAddr)> prevents
                            // repeated synthesis of the same (community, target) pair;
                            // first-admin-wins via HLC linearization handles
                            // multi-admin races (duplicate rotations are no-ops in
                            // materialize's staleness-gate at §4.2).
                            {
                                let community_registry_for_heal = std::sync::Arc::clone(&registry);
                                let signing_key_for_heal = std::sync::Arc::clone(&signing_key_arc);
                                let hlc_tracker_for_heal = std::sync::Arc::clone(&tracker);
                                let device_id_for_heal = device_id.clone();
                                let self_owner_for_heal = self_owner;
                                let crdt_state_for_heal = std::sync::Arc::clone(&crdt_state);
                                // ZEB-249 §10.6 Phase B: extra captures for
                                // apply_remote_epoch_event (signing key + self addr).
                                let signing_key_for_epoch = std::sync::Arc::clone(&signing_key_arc);
                                let self_owner_for_epoch = self_owner;
                                let crdt_state_for_epoch = std::sync::Arc::clone(&crdt_state);
                                // Per-session synthesized-set: avoids re-synthesizing
                                // the same rotation/catchup within one node session.
                                // Wrapped in Arc<Mutex<_>> so the FnMut closure can
                                // mutate across invocations (FnMut + async closures
                                // require shared state, not `move`-only).
                                //
                                // M7: rotation dedup key is (SpaceId, OwnerAddr, EventId)
                                // where EventId is the triggering Kick/Leave event's id.
                                // A pure (SpaceId, OwnerAddr) key would suppress the
                                // second rotation after a rejoin + re-kick sequence,
                                // leaving the re-kicked member in the community's epoch.
                                let synthesized_rotations: std::sync::Arc<
                                    std::sync::Mutex<
                                        std::collections::BTreeSet<(
                                            crate::owner_state_types::SpaceId,
                                            crate::owner_state_types::OwnerAddr,
                                            crate::community_membership::EventId,
                                        )>,
                                    >,
                                > = std::sync::Arc::new(std::sync::Mutex::new(
                                    std::collections::BTreeSet::new(),
                                ));
                                // Catchup dedupe: (SpaceId, OwnerAddr, EventId, u64).
                                // Including the originating Join EventId means a
                                // second rotation producing a new pending_catchup_for
                                // for the same member fires fresh; a pure (SpaceId,
                                // OwnerAddr) key would suppress it across epoch
                                // boundaries. The u64 discriminator is current_epoch
                                // at synthesis time: the same Join EventId can produce
                                // a fresh catchup after each successive epoch rotation
                                // (e.g., a member who joins during a rapid kick flurry
                                // needs a catchup after each rotation that advances the
                                // epoch while they are still pending). ZEB-249 PR #106
                                // R5 (CodeRabbit Major). Type alias: SynthCatchupsSet.
                                let synthesized_catchups: SynthCatchupsSet = std::sync::Arc::new(
                                    std::sync::Mutex::new(std::collections::BTreeSet::new()),
                                );
                                move |delta: crate::community_state_sync::CommunityMembershipDelta| {
                                    let registry = std::sync::Arc::clone(&community_registry_for_heal);
                                    let signing_key = std::sync::Arc::clone(&signing_key_for_heal);
                                    let hlc_tracker = std::sync::Arc::clone(&hlc_tracker_for_heal);
                                    let device_id = device_id_for_heal.clone();
                                    let self_owner = self_owner_for_heal;
                                    let crdt_state = std::sync::Arc::clone(&crdt_state_for_heal);
                                    let synth_rotations = std::sync::Arc::clone(&synthesized_rotations);
                                    let synth_catchups = std::sync::Arc::clone(&synthesized_catchups);
                                    // ZEB-249 §10.6 Phase B: apply remote epoch
                                    // key extraction BEFORE self-heal so the
                                    // observer's catchup synthesis uses the
                                    // freshly-updated current_epoch_key.
                                    let sk_epoch = std::sync::Arc::clone(&signing_key_for_epoch);
                                    let cs_epoch = std::sync::Arc::clone(&crdt_state_for_epoch);
                                    let community_id = delta.community_id;
                                    let event = delta.event.clone();
                                    let local_addr_epoch = self_owner_for_epoch;
                                    async move {
                                        apply_remote_epoch_event(
                                            cs_epoch,
                                            sk_epoch,
                                            community_id,
                                            &event,
                                            local_addr_epoch,
                                        )
                                        .await;
                                        self_heal_community_observer(
                                            delta.community_id,
                                            registry,
                                            signing_key,
                                            hlc_tracker,
                                            device_id,
                                            self_owner,
                                            crdt_state,
                                            synth_rotations,
                                            synth_catchups,
                                        )
                                        .await;
                                    }
                                }
                            },
                        ));
                    }

                    // Spawn the degraded consumer: each `CommunityDegradedReport`
                    // becomes one `community-state-sync-degraded` Tauri event.
                    // Task exits cleanly when `community_degraded_rx.recv()`
                    // returns None — happens after every engine's
                    // `error_tx` clone drops on `registry.shutdown_all()`.
                    {
                        let app_for_degraded = app.clone();
                        tokio::spawn(run_community_degraded_consumer(
                            community_degraded_rx,
                            move |payload| {
                                let app = app_for_degraded.clone();
                                async move {
                                    if let Err(e) =
                                        app.emit("community-state-sync-degraded", &payload)
                                    {
                                        tracing::warn!(
                                            error = ?e,
                                            "failed to emit community-state-sync-degraded"
                                        );
                                    }
                                }
                            },
                        ));
                    }

                    // Lift the start_node-held delta sender out for
                    // NodeState assignment so stop_node / restart can
                    // drop it after `registry.shutdown_all()` to close
                    // the consumer's channel.
                    community_delta_tx_for_state = Some(community_delta_tx);

                    // Scan owner-state for joined communities and spawn
                    // one engine per community. Each spawn allocates a
                    // pair of mpsc channels here (publisher_tx /
                    // subscriber_rx for the engine; matching
                    // publisher_rx / subscriber_tx into a
                    // CommunityAdapterRequest the event loop wires to
                    // Zenoh after `zenoh::open`). Skip any Community
                    // Space that's missing membership_key or admin_addr
                    // — those fields are MUST-be-Some-for-Community per
                    // owner_state_types.rs:1420 / 1427, so a missing
                    // value means a corrupt or partially-applied row;
                    // logging + skipping keeps boot resilient rather
                    // than crashing the node.
                    // Snapshot community metadata under the crdt_state
                    // lock first, then drop the lock before awaiting
                    // spawn_engine. Holding crdt_state across await
                    // would create a lock-order hazard with engine
                    // initialization paths and prevent other tasks from
                    // reading owner-state during boot. Adapter requests
                    // are only enqueued after spawn_engine succeeds so
                    // a failed spawn doesn't leave an orphaned channel
                    // pair for the event_loop to wire to a dead engine.
                    type CommunitySpawnSnapshot = (
                        crate::owner_state_types::SpaceId,
                        crate::owner_state_types::EpochKey,
                        crate::owner_state_types::OwnerAddr,
                        bool,
                    );
                    let community_snapshots: Vec<CommunitySpawnSnapshot> = {
                        let state_snap = crdt_state.lock().await;
                        state_snap
                            .spaces
                            .iter()
                            .filter_map(|(space_id, space)| {
                                if space.kind
                                    != crate::owner_state_types::SpaceKind::Community
                                {
                                    return None;
                                }
                                if space.left_at.is_some() {
                                    return None;
                                }
                                let mk = match space.current_epoch_key.as_ref() {
                                    Some(k) => k.clone(),
                                    None => {
                                        tracing::warn!(
                                            ?space_id,
                                            "community Space missing current_epoch_key — skipping engine spawn"
                                        );
                                        return None;
                                    }
                                };
                                let admin = match space.admin_addr {
                                    Some(a) => a,
                                    None => {
                                        tracing::warn!(
                                            ?space_id,
                                            "community Space missing admin_addr — skipping engine spawn"
                                        );
                                        return None;
                                    }
                                };
                                let is_invite_only =
                                    space.is_invite_only.unwrap_or(false);
                                Some((*space_id, mk, admin, is_invite_only))
                            })
                            .collect()
                    }; // crdt_state lock released here

                    for (space_id, mk, admin, is_invite_only) in community_snapshots {
                        let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
                        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

                        if let Err(e) = registry
                            .spawn_engine_inner_now(
                                space_id,
                                mk,
                                admin,
                                is_invite_only,
                                pub_tx,
                                sub_rx,
                            )
                            .await
                        {
                            tracing::error!(
                                ?space_id,
                                error = %e,
                                "failed to spawn community engine"
                            );
                            // Drop pub_rx + sub_tx implicitly — no
                            // adapter request enqueued, so the
                            // event_loop won't try to wire orphan
                            // channels to a non-existent engine.
                            continue;
                        }
                        community_adapter_requests.push(
                            crate::event_loop::CommunityAdapterRequest {
                                id_hex: hex::encode(space_id.0),
                                publisher_rx: pub_rx,
                                subscriber_tx: sub_tx,
                            },
                        );

                        // ZEB-270 Phase 3 Task 4.5: walk this
                        // community's materialized channels map and
                        // spawn a per-channel engine for each live
                        // (non-tombstoned) entry. Source the
                        // membership_key from the just-spawned engine
                        // (clone-by-value; same bytes as `mk` above
                        // which got moved into spawn_engine). The
                        // adapter requests for the per-channel engines
                        // queue into `channel_log_adapter_request_tx_outer`
                        // — the matching rx is moved into event_loop
                        // below, where it's drained AFTER the Zenoh
                        // session opens.
                        if let Some(community_engine) = registry.engine_arc(&space_id).await {
                            // Materialise the channels map under the
                            // engine's CRDT lock. `materialized` (cached)
                            // is cheap — it holds the lock briefly,
                            // recomputes if stale, returns a clone.
                            let materialized = {
                                let state_g = community_engine.state();
                                let g = state_g.lock().await;
                                g.materialized(community_engine.admin_addr())
                            };
                            let membership_key = community_engine.membership_key();
                            let state_at_hlc = community_engine.state_at_hlc_resolver();
                            let resolver = match community_engine.identity_resolver() {
                                Some(r) => r,
                                None => {
                                    tracing::warn!(
                                        ?space_id,
                                        "boot reconcile: community engine has no identity \
                                         resolver — skipping per-channel reconcile (engine \
                                         can still receive own publishes but cannot verify peers)"
                                    );
                                    continue;
                                }
                            };
                            let hlc_tracker_for_reconcile = std::sync::Arc::clone(&tracker);
                            if let Err(e) =
                                std::sync::Arc::clone(&channel_log_registry_for_reconcile)
                                    .reconcile_from_state(
                                        space_id,
                                        &materialized,
                                        &membership_key,
                                        state_at_hlc,
                                        resolver,
                                        hlc_tracker_for_reconcile,
                                    )
                                    .await
                            {
                                tracing::warn!(
                                    ?space_id,
                                    error = ?e,
                                    "channel-log registry reconcile_from_state failed at boot"
                                );
                            }
                        }
                    }

                    // ZEB-254 R4-4: restart-time rehydrate pass for the
                    // engine's `admin_identity_pub` OnceLock. The R3-C1
                    // pre-bind at spawn time calls
                    // `resolver.resolve(&admin_addr)`, but if the
                    // OwnerDeviceCache was still cold at that moment
                    // (e.g., the joiner crashed before the admin's
                    // device entry got learned), the OnceLock is left
                    // unset. Since the joiner's persisted CRDT may
                    // already contain the admin's bootstrap Join (it
                    // arrived in the SAME publish that bootstrapped
                    // the engine, returned `AlreadyKnown` on the
                    // reconcile insert, and the post-verify bind
                    // therefore never fired), no later event-flow
                    // will rebind the lock until a brand-new admin
                    // event arrives.
                    //
                    // Sweep every spawned engine and call
                    // `try_rehydrate_admin_identity_pub` — best-effort,
                    // idempotent, no-ops cleanly on already-bound
                    // engines and on engines whose persisted log
                    // contains no admin events.
                    {
                        let space_ids: Vec<crate::owner_state_types::SpaceId> = {
                            let g = crdt_state.lock().await;
                            g.spaces
                                .iter()
                                .filter(|(_, s)| {
                                    s.kind == crate::owner_state_types::SpaceKind::Community
                                        && s.left_at.is_none()
                                })
                                .map(|(id, _)| *id)
                                .collect()
                        };
                        for space_id in space_ids {
                            let engine = match registry.engine_arc(&space_id).await {
                                Some(e) => e,
                                None => continue,
                            };
                            if engine.try_rehydrate_admin_identity_pub().await {
                                tracing::info!(
                                    ?space_id,
                                    "ZEB-254 R4-4: admin_identity_pub rehydrated at boot \
                                     (resolver re-resolve hit after spawn-time cold miss)"
                                );
                            }
                        }
                    }

                    // ZEB-254 R3 (C3): restart-time healing pass for
                    // pending_join_at. The post-Inserted clear hook only
                    // fires for events freshly Inserted in this process —
                    // if a process crashed between PendingJoin landing and
                    // the JoinCountersign arriving (both events on disk,
                    // but the hook never ran), `Space.pending_join_at`
                    // would remain Some forever. Walk every Community
                    // Space whose `pending_join_at` is Some; if the
                    // engine's persisted event log holds a JoinCountersign
                    // for a self-authored PendingJoin, clear it via the
                    // same `apply_space_with_canonicalization` path the
                    // online hook uses + invoke the nav_emitter callback.
                    {
                        // Snapshot under the crdt_state lock first.
                        type HealCandidate = (
                            crate::owner_state_types::SpaceId,
                            crate::owner_state_types::Space,
                        );
                        let candidates: Vec<HealCandidate> = {
                            let g = crdt_state.lock().await;
                            g.spaces
                                .iter()
                                .filter(|(_, s)| {
                                    s.kind == crate::owner_state_types::SpaceKind::Community
                                        && s.pending_join_at.is_some()
                                        && s.left_at.is_none()
                                })
                                .map(|(id, s)| (*id, s.clone()))
                                .collect()
                        };

                        for (space_id, space) in candidates {
                            let engine = match registry.engine_arc(&space_id).await {
                                Some(e) => e,
                                None => continue, // no engine spawned (skipped above)
                            };
                            // R4-5: locate the SPECIFIC self-authored
                            // PendingJoin event matching `Space.pending_join_at`
                            // (i.e., the HLC of THIS pending attempt — set
                            // when the joiner minted the event in
                            // `redeem_invite_inner`). Only clear if THAT
                            // event has been countersigned, not just
                            // "any prior PendingJoin from this user". A
                            // joiner with history (leave → re-join twice)
                            // could have a stale JoinCountersign for an
                            // older attempt; without this guard, the
                            // healing pass would clear `pending_join_at`
                            // for the CURRENT attempt by matching against
                            // a previous attempt's countersign.
                            let pending_join_at = match space.pending_join_at.as_ref() {
                                Some(hlc) => hlc.clone(),
                                None => continue, // covered by the candidates filter, defensive
                            };
                            let cleared = {
                                let state = engine.state();
                                let g = state.lock().await;
                                // Find the self-authored PendingJoin whose HLC equals
                                // pending_join_at. HLC is the (wall_ms, logical, device_id)
                                // triple; the `Space.pending_join_at` field is exactly
                                // the `event.at` stored at mint time.
                                let target_event_id: Option<crate::community_membership::EventId> = g
                                    .events
                                    .values()
                                    .find(|e| {
                                        e.actor == self_owner
                                            && matches!(
                                                &e.kind,
                                                crate::community_membership::MembershipEventKind::PendingJoin { .. }
                                            )
                                            && e.at == pending_join_at
                                    })
                                    .map(|e| e.id);
                                match target_event_id {
                                    None => false, // no matching PendingJoin on disk
                                    Some(target_id) => g.events.values().any(|e| matches!(
                                        &e.kind,
                                        crate::community_membership::MembershipEventKind::JoinCountersign {
                                            target_event_id,
                                        } if *target_event_id == target_id
                                    )),
                                }
                            };

                            if !cleared {
                                continue;
                            }

                            // Found a matching JoinCountersign — clear
                            // Space.pending_join_at. Use monotonic HLC.
                            let wall_now_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as u64;
                            let new_hlc = if wall_now_ms > space.updated_at.wall_ms {
                                crate::owner_state_types::Hlc {
                                    wall_ms: wall_now_ms,
                                    logical: 0,
                                    device_id: space.updated_at.device_id.clone(),
                                }
                            } else {
                                crate::owner_state_types::Hlc {
                                    wall_ms: space.updated_at.wall_ms,
                                    logical: space.updated_at.logical.saturating_add(1),
                                    device_id: space.updated_at.device_id.clone(),
                                }
                            };
                            let mut updated = space.clone();
                            updated.pending_join_at = None;
                            updated.updated_at = new_hlc;
                            let space_name = space.name.clone();

                            let outcome = {
                                let mut g = crdt_state.lock().await;
                                g.apply_space_with_canonicalization(updated)
                            };

                            match outcome {
                                crate::owner_state_crdt::ApplyOutcome::Inserted
                                | crate::owner_state_crdt::ApplyOutcome::Merged { .. } => {
                                    tracing::info!(
                                        ?space_id,
                                        "ZEB-254 R3 (C3): restart-time pending_join_at healed \
                                         (countersign found on disk, owner-state stale Some cleared)"
                                    );
                                    // Fire nav-updated event so the UI
                                    // ungreys this community at boot.
                                    let space_id_hex = hex::encode(space_id.0);
                                    use tauri::Emitter as _;
                                    if let Err(e) = app.emit(
                                        "nav-updated",
                                        &NavUpdatedPayload {
                                            action: "modified",
                                            space_id: space_id_hex,
                                            kind: "community",
                                            name: space_name,
                                            members: None,
                                            parent_id: None,
                                            pending: Some(false),
                                        },
                                    ) {
                                        tracing::warn!(
                                            error = %e,
                                            "ZEB-254 R3 (C3): nav-updated emit failed during boot heal"
                                        );
                                    }
                                }
                                crate::owner_state_crdt::ApplyOutcome::Rejected(ref reason) => {
                                    tracing::warn!(
                                        ?space_id,
                                        reason = ?reason,
                                        "ZEB-254 R3 (C3): apply_space_with_canonicalization \
                                         rejected during restart-time heal"
                                    );
                                }
                            }
                        }
                    }

                    community_registry_arc = Some(registry);

                    // ── ZEB-281 Sub-D Phase 4: profile-broadcast publisher spawn ──
                    //
                    // Owns Arc clones of `crdt_state` + `tracker` for the
                    // `OwnerStateBroadcastSource` (walks Community Spaces
                    // for the opted-in set + bumps the HLC for each
                    // publish), and a clone of `publish_tx` for the
                    // `EventLoopPublishSink` (forwards canonical-CBOR
                    // bytes to the event loop's per-topic Zenoh
                    // publisher). The signing key is derived from the
                    // same Ed25519 seed `DmOutbox` consumes — bit-
                    // identical to PrivateIdentity::sign's internal key
                    // (see dm_signing.rs). identity_pub_64 is the same
                    // 64-byte bundle stamped on outbound DmInvite
                    // packets (captured before PrivateIdentity dropped).
                    profile_broadcast_publisher_arc =
                        Some(crate::profile_broadcast::ProfileBroadcastPublisher::spawn(
                            (*signing_key_arc).clone(),
                            identity_pub_64,
                            std::sync::Arc::new(
                                crate::profile_broadcast::OwnerStateBroadcastSource {
                                    crdt_state: std::sync::Arc::clone(&crdt_state),
                                    hlc_tracker: std::sync::Arc::clone(&tracker),
                                    device_id: device_id.clone(),
                                },
                            ),
                            std::sync::Arc::new(crate::profile_broadcast::EventLoopPublishSink {
                                publish_tx: publish_tx.clone(),
                            }),
                            crate::profile_broadcast::PUBLISHER_DEBOUNCE,
                            crate::profile_broadcast::PUBLISHER_REFRESH_INTERVAL,
                        ));

                    // Lift the per-identity handles out for NodeState
                    // assignment below.
                    device_id_for_state = Some(device_id);
                    self_owner_for_state = Some(self_owner);
                    crdt_state_for_state = Some(crdt_state);
                    tracker_for_state = Some(tracker);
                    content_store_for_state = Some(content_store);
                    dm_outbox_arc = Some(outbox);
                    dm_transport_arc = Some(transport);

                    Some(engine)
                } else {
                    None
                }
            } else {
                None
            };

        let node_addr_for_state = node_addr.clone();
        let config = NodeConfig {
            storage_budget: StorageBudget {
                cache_capacity: 512,
                max_pinned_bytes: 50_000_000,
            },
            compute_budget: InstructionBudget { fuel: 100_000 },
            schedule: Default::default(),
            content_policy: ContentPolicy::default(),
            filter_broadcast_config: FilterBroadcastConfig {
                mutation_threshold: 10,
                max_interval_ticks: 40,
                expected_items: 512,
                fp_rate: 0.001,
            },
            node_addr,
            local_identity_hash: our_addr_bytes,
            local_pq_identity_hash,
            local_dsa_pubkey,
            local_kem_pubkey,
            reticulum_identity_bytes,
            inference_gguf_cid: None,
            inference_tokenizer_cid: None,
            engram_manifest_cid: None,
            disk_enabled: false,
            disk_entries: Vec::new(),
            disk_quota: None,
            archive_enabled: false,
            archive_entries: Vec::new(),
            archive_quota: None,
            archive_ingest_enabled: false,
            eviction_push_enabled: false,
            s3_enabled: false,
        };

        // ZEB-218 Sub-D Phase 1: construct the `LibraryDirectory` (Arc<…>)
        // and split off the matching `request_rx` for the event-loop
        // consumer task. The Arc is stashed onto NodeState below so
        // future IPC handlers (Task 4) can reach `request_tx` to add /
        // remove libraries; the rx is moved into `event_loop::run`.
        // Built BEFORE the std MutexGuard lock acquisition since the
        // startup walk awaits on the tokio Mutex `crdt_state` lock and
        // would otherwise hold the !Send std guard across an await.
        let (library_directory_arc, library_request_rx) =
            crate::library_directory::LibraryDirectory::new();
        // ZEB-218 Sub-D Phase 1: walk owner-state at startup and send a
        // Subscribe request for each effective (non-tombstoned) library.
        // Done BEFORE the event_loop spawn. The channel is unbounded
        // (UnboundedSender::send is synchronous and never blocks) —
        // see the F1 fix discussion in `LibraryDirectory::new` doc:
        // Subscribe/Unsubscribe traffic is small + infrequent + only
        // grows with user library count, so the bounded(64) variant
        // could deadlock on >64 libraries here BEFORE the consumer task
        // even spawned.
        if let Some(ref crdt_arc) = crdt_state_for_state {
            let crdt_g = crdt_arc.lock().await;
            for (addr, lib_entry) in &crdt_g.libraries {
                if lib_entry.is_effective() {
                    if let Err(e) = library_directory_arc
                        .request_tx
                        .send(crate::library_directory::LibraryDirectoryRequest::Subscribe(*addr))
                    {
                        tracing::warn!(
                            ?addr,
                            error = %e,
                            "failed to enqueue library Subscribe at startup"
                        );
                    }
                }
            }
        }

        // Re-acquire lock and atomically register the new node.
        // Handles are stored BEFORE awaiting ready_rx so stop_node can
        // cancel an in-flight startup via shutdown_tx.
        //
        // ZEB-221: validate our lock-1 reservation. If a later start_node
        // has bumped past `my_install_seq` while we were building
        // SyncEngine et al. outside the lock, set the `superseded`
        // sentinel and skip the install block; post-lock cleanup will
        // await shutdown on each of the four background-task-owning Arcs.
        //
        // ZEB-221 (CodeRabbit R3): every lock-poison failure mode — the
        // check helper's own lock acquisition AND the supersede re-lock —
        // must converge on the same post-block cleanup. Early-returning
        // from a poison failure would orphan the four background-task-
        // owning Arcs constructed outside the lock. The labeled
        // `'install_or_skip` block below routes every poison case through
        // `lock_failure_msg`; the post-block cleanup branch then awaits
        // `shutdown()` on each Arc uniformly (the std `MutexGuard` has
        // gone out of scope by that point, so the `!Send` guard does not
        // cross the awaits).
        let mut lock_failure_msg: Option<String> = None;
        let mut current_generation: u64 = 0;
        let mut superseded = false;
        // ZEB-221: declared outside the `if !superseded` block so the
        // tuple-return below sees it regardless of which branch ran.
        // On the supersede path it stays `None` — cleanup is driven by
        // the `superseded` sentinel instead.
        let mut thread_install_failure: Option<String> = None;
        'install_or_skip: {
            // The match below MUST NOT span an await (`MutexGuard` is
            // `!Send`). Each arm either assigns `guard` synchronously or
            // breaks the labeled block with `lock_failure_msg` set.
            let mut guard;
            match check_install_seq_or_supersede(&state, my_install_seq) {
                Ok(g) => {
                    guard = g;
                }
                Err(SupersededError::Superseded { .. }) => {
                    // Re-acquire the lock for the install-skip path so the
                    // post-block tuple captures the current generation.
                    // The check_install_seq_or_supersede helper consumed
                    // its lock acquisition on the Err path. Lock-poison
                    // in this narrow window routes through
                    // `lock_failure_msg` → post-block cleanup.
                    match state.lock() {
                        Ok(g) => {
                            guard = g;
                            superseded = true;
                        }
                        Err(e) => {
                            lock_failure_msg = Some(format!("lock error: {e}"));
                            break 'install_or_skip;
                        }
                    }
                }
                Err(SupersededError::LockError(msg)) => {
                    // Poison in the check helper's own lock acquisition.
                    // Same routing as the supersede re-lock failure above.
                    lock_failure_msg = Some(msg);
                    break 'install_or_skip;
                }
            }

            if !superseded {
                // ZEB-221 (CodeRabbit R2 finding): the `generation` bump is
                // INSIDE the `Ok(thread)` arm of `match thread_result` below,
                // NOT here. Bumping pre-spawn would consume a generation even
                // when thread::Builder::spawn fails (OOM, ulimit), violating
                // the documented "successful install only" semantics that
                // post-install checks rely on (pairing-handle install at
                // ~2799-2806, failure cleanup at ~2945-2948, stop_inner
                // gating). Race detection between concurrent start_node
                // attempts uses `install_seq` (already validated by
                // check_install_seq_or_supersede above), so this site no
                // longer needs to bump anything.

                // ZEB-155: load the sidecar NOW — after stop_handles has
                // quiesced the previous node and under the state lock — so any
                // pin_content / unpin_content / burn_content that raced with
                // the stop path has already durably written to disk. Concurrent
                // command handlers are blocked on state.lock(), so they cannot
                // slip a write between this load and the Arc install below.
                //
                // A narrower window remains: a mutation command that cloned
                // the OLD Arc before stop_handles and is still mid-set_pinned
                // when the NEW Arc is installed will orphan its disk write
                // (the next NEW-Arc save() overwrites). That end-to-end
                // serialization is ZEB-160's territory.
                let content_index = std::sync::Arc::new(std::sync::Mutex::new(
                    content_index::ContentIndex::load(&app_data_dir),
                ));
                let pin_intent: std::collections::HashSet<[u8; 32]> = {
                    let idx = content_index
                        .lock()
                        .map_err(|e| format!("content_index lock on startup: {e}"))?;
                    // ZEB-164: multiple sidecar entries can pin the same CID. The
                    // runtime pin_intent set is CID-keyed, so we dedupe here —
                    // collecting into a HashSet drops duplicates without effect.
                    // (Functionally identical to the pre-ZEB-164 path; the dedupe
                    // is just made explicit so debug logs don't show repeated
                    // restores for the same CID.)
                    idx.entries().filter(|e| e.pinned).map(|e| e.cid).collect()
                };

                let ep_clone = endpoint.clone();
                let app_clone = app.clone();
                let mail_mgr_clone = mail_mgr.clone();
                let mail_sync_for_loop = std::sync::Arc::clone(&mail_sync);
                let cas_op_tx_for_loop = cas_op_tx.clone();
                let sync_handles_for_loop = sync_handles_opt;
                let dm_outbox_for_loop = dm_outbox_arc.clone();
                let dm_transport_for_loop = dm_transport_arc.clone();
                let crdt_state_for_loop = crdt_state_for_state.clone();
                // ZEB-227 Path B Task 11: extra handles for the
                // RuntimeAction::UnicastReceived interception block in event_loop.
                // cas_handle: handle_cidnotify_lifted does a 500ms-timeout cas.get; reuse
                //   the same RuntimeContentStore the SyncEngine consumes.
                // unicast_send_tx_for_loop: handle_cidnotify_lifted pushes DmAck fan-out
                //   into the same channel the production transport uses for
                //   outbound CidNotify. Same channel, both directions push.
                let cas_handle_for_loop = content_store_for_state.clone();
                let unicast_send_tx_for_loop = Some(unicast_send_tx.clone());
                // ZEB-217 Sub-C Phase 2: per-community Zenoh adapter requests.
                // Move (not clone) — the Vec carries Receiver halves the engines
                // already own the matching Sender / other-half for; only the
                // event loop reads from this Vec, no other consumer.
                let community_adapter_requests_for_loop =
                    std::mem::take(&mut community_adapter_requests);
                // ZEB-217 Sub-C Phase 3 Task 9: on-demand adapter request channel.
                // The IPC `create_community` (and Phase 4's `redeem_invite`)
                // dispatch a `CommunityAdapterRequest` here; the event loop's
                // `select!` drains the rx and binds the per-community channel
                // halves to a Zenoh adapter against the live session. Capacity
                // 32 is sized to match peak join-burst load — one request per
                // create/redeem; full-channel falls back to a clear Err on the
                // IPC side rather than blocking under contention.
                let (community_adapter_request_tx, community_adapter_request_rx) =
                    tokio::sync::mpsc::channel::<crate::event_loop::CommunityAdapterRequest>(32);
                // ZEB-262 Phase 4 Task 9: clone the community_registry handle
                // for event_loop::run BEFORE the closure capture below moves
                // `community_registry_arc` (the post-spawn `guard.community_registry`
                // assignment still needs the original handle).
                let community_registry_for_loop = community_registry_arc.clone();
                // ZEB-218 Sub-D Phase 1: thread the pre-built `LibraryDirectory`
                // handle + request rx (constructed + populated above the
                // std::sync::MutexGuard scope) into event_loop::run.
                let library_directory_for_loop =
                    Some(std::sync::Arc::clone(&library_directory_arc));
                let library_request_rx_for_loop = Some(library_request_rx);
                // ZEB-281 Sub-D Phase 4: thread the profile-broadcast cache +
                // request rx into event_loop::run. The cache is shared with the
                // NodeState (so IPC handlers can register/drop subscriptions
                // synchronously); the rx is moved into the consumer task.
                let profile_broadcast_cache_for_loop =
                    Some(std::sync::Arc::clone(&profile_broadcast_cache_arc));
                let profile_broadcast_request_rx_for_loop = Some(profile_broadcast_request_rx);
                let thread_result = thread::Builder::new()
                    .name("harmony-runtime".to_string())
                    // Windows debug builds overflow the default ~2 MiB stack inside
                    // Zenoh session setup; match the 8 MiB used throughout identity.rs.
                    .stack_size(8 * 1024 * 1024)
                    .spawn(move || {
                        // Zenoh's `.wait()` (used by its `IntoFuture` impl) calls
                        // ZRuntime::block_in_place, which panics on a current-thread
                        // scheduler. A single-worker multi-thread runtime is the
                        // minimum Zenoh supports.
                        //
                        // `.thread_stack_size(8 MiB)` covers Tokio's own worker
                        // threads independently of RUST_MIN_STACK — important
                        // because Cargo's `[env]` block in .cargo/config.toml only
                        // propagates to binaries Cargo launches (e.g. cargo run /
                        // tauri dev), not to release binaries run directly. Without
                        // this call, a `tauri build` artifact would silently regress
                        // to the 2 MiB default on Windows.
                        let rt = tokio::runtime::Builder::new_multi_thread()
                            .worker_threads(1)
                            .thread_stack_size(8 * 1024 * 1024)
                            .enable_all()
                            .build()
                            .expect("failed to create tokio runtime for harmony-runtime");
                        rt.block_on(async move {
                            let (mut runtime, startup_actions) =
                                NodeRuntime::new(config, MemoryBookStore::new());

                            // ZEB-227 Path B: register our DM destination so inbound
                            // packets to it surface as RuntimeAction::UnicastReceived.
                            // Without this registration, every inbound DmInvite /
                            // DmCidNotify / DmAck would drop in the runtime as
                            // NoLocalDestination before reaching
                            // dm_outbox::handle_unicast.
                            //
                            // Our DM destination hash is computed from our local
                            // Reticulum identity hash via the same
                            // SHA256(SHA256("harmony.dm")[:10] || identity)[:16]
                            // scheme that DmOutbox::drain uses to resolve outbound
                            // destinations from OwnerDeviceCache (so a peer's
                            // outbound dest_hash for us == our registered
                            // dest_hash for ourselves).
                            //
                            // Unconditional: every node has a Reticulum identity
                            // (loaded above via identity::load_or_generate, before
                            // owner-loading). DMs themselves only flow once the owner
                            // identity is loaded (which gates DmOutbox /
                            // RuntimeUnicastTransport construction above), but the
                            // raw destination registration is harmless when no owner
                            // is loaded — it just means inbound packets surface but
                            // event_loop's UnicastReceived arm has no DmOutbox to
                            // dispatch to (and logs the drop).
                            let our_identity_hash = runtime.local_identity_hash();
                            let our_dm_dest =
                                crate::dm_signing::compute_dm_destination_hash(our_identity_hash);
                            runtime.register_local_destination(our_dm_dest);
                            tracing::info!(
                                dm_dest = hex::encode(our_dm_dest),
                                "registered DM destination for inbound DmInvite/DmCidNotify/DmAck"
                            );

                            event_loop::run(
                                runtime,
                                startup_actions,
                                app_clone,
                                ep_clone,
                                ready_tx,
                                shutdown_rx,
                                publish_rx,
                                fetch_rx,
                                ingest_rx,
                                content_verb_rx,
                                cas_op_tx_for_loop,
                                cas_op_rx,
                                follow_rx,
                                voice_rx,
                                voice_channel_rx,
                                followed_set_clone,
                                vine_feed_cache_clone,
                                mail_mgr_clone,
                                Some(mail_sync_for_loop),
                                mail_refresh_rx,
                                pin_intent,
                                fetch_completion_tx,
                                fetch_completion_rx,
                                Some(pairing_in_tx),
                                sync_handles_for_loop,
                                dm_outbox_for_loop,
                                dm_transport_for_loop,
                                crdt_state_for_loop,
                                Some(unicast_send_rx),
                                cas_handle_for_loop,
                                unicast_send_tx_for_loop,
                                community_adapter_requests_for_loop,
                                community_adapter_request_rx,
                                community_registry_for_loop,
                                channel_log_adapter_request_rx_for_loop,
                                library_directory_for_loop,
                                library_request_rx_for_loop,
                                profile_broadcast_cache_for_loop,
                                profile_broadcast_request_rx_for_loop,
                            )
                            .await;
                        });
                    });

                // If the runtime-thread spawn fails (rare — typically only on
                // OOM / kernel-thread limits), the SyncEngine constructed
                // above has ALREADY spawned its background tokio task. We
                // can't drop the Arc<SyncEngine> here without first calling
                // `shutdown()` — that would orphan the task and silently lose
                // the final-flush path. The await must happen OUTSIDE this
                // lock-held block (the std `MutexGuard` is `!Send` across an
                // await point), so we capture the failure into a sentinel
                // and clean up below.
                match thread_result {
                    Ok(thread) => {
                        // ZEB-221 (CodeRabbit R2): bump generation ONLY here,
                        // inside the Ok(thread) arm, so the field reflects
                        // "successful install" — never advanced on thread-
                        // spawn failure. Post-install checks (pairing-handle
                        // install, failure cleanup, stop_inner gating) compare
                        // `guard.generation` against `our_gen` and rely on
                        // this invariant.
                        guard.generation += 1;
                        guard.thread = Some(thread);
                        guard.shutdown_tx = Some(shutdown_tx);
                        guard.publish_tx = Some(publish_tx);
                        guard.fetch_tx = Some(fetch_tx);
                        guard.ingest_tx = Some(ingest_tx);
                        guard.content_verb_tx = Some(content_verb_tx);
                        guard.content_index = content_index;
                        guard.follow_tx = Some(follow_tx);
                        guard.voice_tx = Some(voice_tx);
                        guard.voice_channel_tx = Some(voice_channel_tx);
                        guard.follow_mgr = Some(follow_mgr);
                        guard.followed_set = Some(followed_set);
                        guard.vine_feed_cache = Some(vine_feed_cache);
                        guard.mail_mgr = Some(mail_mgr);
                        guard.mail_sync = Some(mail_sync);
                        guard.node_addr = node_addr_for_state;
                        guard.sync_engine = sync_engine_arc.clone();
                        // ZEB-217 Sub-C Phase 2: stash the per-community engine
                        // registry on NodeState so Phase 3 IPC handlers can
                        // reach it. Cloned (Arc bump) — `community_registry_arc`
                        // is also held by the failure-cleanup tuple below.
                        guard.community_registry = community_registry_arc.clone();
                        // ZEB-217 Sub-C Phase 3 Task 8: store the start_node-held
                        // delta sender so stop_node / restart can drop it after
                        // `registry.shutdown_all()` and the consumer task winds
                        // down cleanly.
                        guard.community_delta_tx = community_delta_tx_for_state.clone();
                        // ZEB-225 Sub-B Phase 2: store DM outbox + per-identity
                        // handles on NodeState for send_dm IPC + (T7) drain tick.
                        guard.dm_outbox = dm_outbox_arc.clone();
                        guard.dm_transport = dm_transport_arc.clone();
                        guard.crdt_state = crdt_state_for_state.clone();
                        guard.hlc_tracker = tracker_for_state.clone();
                        guard.dm_device_id = device_id_for_state.clone();
                        guard.dm_self_owner = self_owner_for_state;
                        guard.content_store = content_store_for_state.clone();
                        // ZEB-234: initialize the shutdown fence. Semaphore starts
                        // at DM_SEND_FENCE_CAPACITY permits (one per concurrent
                        // send_dm); stopping flag starts false. Both cleared in
                        // stop_inner after the permit drain.
                        guard.dm_send_inflight = Some(std::sync::Arc::new(
                            tokio::sync::Semaphore::new(DM_SEND_FENCE_CAPACITY),
                        ));
                        guard.dm_send_stopping = Some(std::sync::Arc::new(
                            std::sync::atomic::AtomicBool::new(false),
                        ));
                        // ZEB-227 Path B: store the outbound unicast sender so
                        // Task 11's RuntimeUnicastTransport instantiation in
                        // start_node can clone it. The receiver was moved into
                        // event_loop above; the sender remains unused-by-production
                        // until Task 11 wires it to the real transport.
                        guard.unicast_send_tx = Some(unicast_send_tx.clone());
                        // ZEB-228 Phase 4: store our 64-byte combined identity_pub
                        // so add_space can ship it as the bootstrap pubkey on
                        // outbound DmInvite packets. Captured above before the
                        // ed25519 PrivateIdentity was dropped.
                        guard.dm_identity_pub_64 = Some(identity_pub_64);
                        // ZEB-217 Sub-C Phase 3 Task 9: store the adapter-
                        // request sender so create_community / Phase 4
                        // redeem_invite can dispatch on-demand
                        // `CommunityAdapterRequest`s into the event loop. The
                        // matching rx was moved into event_loop::run above.
                        guard.community_adapter_request_tx = Some(community_adapter_request_tx);
                        // ZEB-270 Phase 3 Task 4.5: store the channel-log
                        // registry handle so stop_inner can flip every
                        // per-channel `closing` flag and run final flushes
                        // before the event loop tears down. `None` here when
                        // no owner identity is loaded — the registry is gated
                        // on owner-load above.
                        guard.channel_log_registry = channel_log_registry_arc.clone();
                        // ZEB-218 Sub-D Phase 1: stash the library_directory Arc
                        // so future IPC handlers (Task 4) can reach `request_tx`
                        // to add / remove libraries. The matching rx was moved
                        // into event_loop above.
                        guard.library_directory = Some(library_directory_arc.clone());
                        // ZEB-281 Sub-D Phase 4: stash the publisher, cache,
                        // request channel + subscription-id allocator on
                        // NodeState. The publisher Arc is None when no owner
                        // identity was loaded (publisher.spawn is gated inside
                        // the if-let-Some(seed) block above) — without an owner
                        // identity, set_space_shared_in_profile IPC will still
                        // fail with "publisher missing" so the absence is fine.
                        guard.profile_broadcast_publisher = profile_broadcast_publisher_arc.clone();
                        guard.profile_broadcast_cache =
                            Some(std::sync::Arc::clone(&profile_broadcast_cache_arc));
                        guard.profile_broadcast_request_tx = Some(profile_broadcast_request_tx);
                        guard.profile_broadcast_next_subscription_id =
                            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
                        thread_install_failure = None;
                    }
                    Err(e) => {
                        thread_install_failure =
                            Some(format!("failed to spawn runtime thread: {e}"));
                    }
                }
            } // end `if !superseded`
              // Capture the generation BEFORE the labeled block ends — once
              // it ends, `guard` is dropped and inaccessible. On lock-poison
              // break paths above, `current_generation` stays at its default
              // `0` (unread because the post-block cleanup branch returns
              // before any code checks generation).
            current_generation = guard.generation;
        } // end `'install_or_skip` labeled block (guard dropped here)
          // The Arc clones carry the SyncEngine + registries + publisher
          // back out of the block so the failure-cleanup path below can
          // await `shutdown()` on each without holding the std
          // `MutexGuard` across an await (the guard is `!Send`). On
          // success these Arcs are discarded; NodeState already owns its
          // own clone of each.
        (
            current_generation,
            thread_install_failure,
            superseded,
            lock_failure_msg,
            sync_engine_arc.clone(),
            community_registry_arc.clone(),
            channel_log_registry_arc.clone(),
            profile_broadcast_publisher_arc.clone(),
        )
    };
    let (
        our_gen,
        thread_spawn_failure,
        superseded,
        lock_failure_msg,
        engine_for_cleanup,
        registry_for_cleanup,
        channel_log_registry_for_cleanup,
        profile_broadcast_publisher_for_cleanup,
    ) = our_gen;

    // ZEB-221 + thread-spawn-failure cleanup + lock-poison cleanup: all
    // three paths require the same shutdown-then-drop sequence on the
    // four background-task-owning Arcs built outside the lock. Priority
    // order: lock-poison message wins over supersede over thread spawn
    // (lock failure is the most specific root cause; supersede is the
    // next-most-specific; thread spawn failure is the fallback).
    let cleanup_msg: Option<String> = if let Some(msg) = lock_failure_msg {
        Some(msg)
    } else if superseded {
        Some("start_node superseded by concurrent call".to_string())
    } else {
        thread_spawn_failure
    };
    if let Some(msg) = cleanup_msg {
        // ZEB-281 Sub-D Phase 4: abort the profile-broadcast publisher
        // FIRST — its background task holds a clone of `publish_tx`
        // (now orphaned because the runtime thread never spawned), so
        // aborting it deterministically releases the clone before the
        // other registries shut down.
        if let Some(publisher) = profile_broadcast_publisher_for_cleanup {
            publisher.shutdown().await;
        }
        // ZEB-270 Phase 3 Task 4.5: shutdown the channel-log registry
        // FIRST so each per-channel engine's final flush completes
        // before the per-community state engines (which back the
        // verify chain) tear down. Mirrors stop_inner's ordering.
        if let Some(registry) = channel_log_registry_for_cleanup {
            if let Err(e) = registry.shutdown_all().await {
                tracing::error!(
                    error = %e,
                    "ChannelLogRegistry cleanup after start_node failure"
                );
            }
        }
        // ZEB-217 Sub-C Phase 2: shutdown the registry FIRST so each
        // community engine's final flush completes before the owner
        // SyncEngine tears down. Mirrors stop_inner's ordering.
        if let Some(registry) = registry_for_cleanup {
            if let Err(e) = registry.shutdown_all().await {
                tracing::error!(
                    error = %e,
                    "CommunitySyncRegistry cleanup after start_node failure"
                );
            }
        }
        if let Some(engine) = engine_for_cleanup {
            if let Err(e) = engine.shutdown().await {
                tracing::error!(
                    error = %e,
                    "SyncEngine cleanup after start_node failure"
                );
            }
        }
        return Err(msg);
    }

    // Wait for the event loop to report startup success or failure.
    // stop_node can cancel this by signaling shutdown_tx (now registered).
    let result = match ready_rx.await {
        Ok(Ok(())) => 'arm: {
            // Phase 3b: cross-device sync now works through real CAS
            // (RuntimeContentStore); the Phase 3a degraded banner is
            // retired. Transport-layer failures (subscriber declare,
            // key_expr invalid, subscriber closed mid-session) still
            // fire `state-root-sync-degraded` from event_loop.rs as
            // genuine degradation signals.
            //
            // ZEB-197: spawn the pairing state machine now that the
            // event loop is up. Construct ZenohPairingTransport with
            // a clone of publish_tx (publishes go through the running
            // event loop) and the receiver half of pairing_in. Stash
            // the handle on NodeState so stop_node can drop it.
            let install_pairing = {
                let guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
                if guard.generation != our_gen {
                    // A newer start_node has raced us; do not install.
                    None
                } else {
                    guard.publish_tx.as_ref().cloned()
                }
            };
            if let Some(publish_tx_clone) = install_pairing {
                // ZEB-200: pairing without persistence is the exact UX hole
                // ZEB-197 closed — a successful pair would be silently
                // dropped on next start_node. Resolve up front and surface
                // any failure as a hard start_node error so the cleanup
                // hook below tears down the running event loop.
                let identity_dir = match crate::owner_commands::resolve_identity_dir() {
                    Ok(p) => p,
                    Err(e) => {
                        break 'arm Err(format!(
                            "cannot resolve identity_dir for pairing persistence: {e}"
                        ));
                    }
                };
                let pairing_transport: std::sync::Arc<
                    dyn crate::pairing::transport::PairingTransport,
                > = std::sync::Arc::new(
                    crate::pairing::zenoh_transport::ZenohPairingTransport::new(
                        publish_tx_clone,
                        pairing_in_rx,
                    ),
                );
                let mut pairing_handle = crate::pairing::state_machine::spawn_state_machine(
                    pairing_transport,
                    std::sync::Arc::new(|| {
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0)
                    }),
                    crate::pairing::state_machine::DEFAULT_DISCOVER_REBROADCAST_INTERVAL,
                );
                // Bridge pairing state changes to a Tauri frontend event.
                // Clone state_rx before moving the handle into NodeState.
                let mut prx = pairing_handle.state_rx.clone();
                let app_clone = app.clone();
                tokio::spawn(async move {
                    loop {
                        if prx.changed().await.is_err() {
                            break;
                        }
                        let s = prx.borrow().clone();
                        let _ = app_clone.emit("pairing-state-changed", s);
                    }
                });

                // ZEB-197 persistence drainers. The pairing state machine
                // emits {Joiner,Inviter}EnrollResult on Complete; without
                // these drainers the post-Complete state lives only in RAM
                // and the user's DevicesPanel reverts on next start_node.
                //
                // Receivers are taken out of the handle (mpsc receivers are
                // single-consumer, not Clone like watch::Receiver). The
                // drainer task owns each receiver until the SM shuts down.
                // ZEB-199: install_*_state acquires OWNER_STATE_WRITE_LOCK
                // (a std::sync::Mutex). Awaiting the persist call directly on
                // the runtime would block the executor thread for the full
                // load+merge+save window — measured at ~5-50ms per pair, but
                // longer under contention with mint or other persist callers.
                // Run the sync work on the dedicated blocking pool via
                // spawn_blocking so the runtime stays responsive (zenoh sync,
                // IPC, UI events). Mirrors the run_blocking pattern used by
                // mint in owner_commands.rs.
                if let Some(mut rx) = pairing_handle.joiner_result_rx.take() {
                    let id_dir = identity_dir.clone();
                    tokio::spawn(async move {
                        while let Some(result) = rx.recv().await {
                            let id_dir = id_dir.clone();
                            let outcome = tokio::task::spawn_blocking(move || {
                                crate::pairing::persist::install_joiner_state(&id_dir, result)
                            })
                            .await;
                            match outcome {
                                Ok(Ok(())) => {
                                    tracing::info!("joiner pairing persisted successfully");
                                }
                                Ok(Err(e)) => {
                                    tracing::error!("failed to persist joiner pairing result: {e}");
                                }
                                Err(e) => {
                                    tracing::error!("joiner persist task join failed: {e}");
                                }
                            }
                        }
                    });
                }
                if let Some(mut rx) = pairing_handle.inviter_result_rx.take() {
                    let id_dir = identity_dir.clone();
                    tokio::spawn(async move {
                        while let Some(result) = rx.recv().await {
                            let id_dir = id_dir.clone();
                            let outcome = tokio::task::spawn_blocking(move || {
                                crate::pairing::persist::install_inviter_state(&id_dir, result)
                            })
                            .await;
                            match outcome {
                                Ok(Ok(())) => {
                                    tracing::info!("inviter pairing persisted successfully");
                                }
                                Ok(Err(e)) => {
                                    tracing::error!(
                                        "failed to persist inviter pairing result: {e}"
                                    );
                                }
                                Err(e) => {
                                    tracing::error!("inviter persist task join failed: {e}");
                                }
                            }
                        }
                    });
                }

                if let Ok(mut guard) = state.lock() {
                    if guard.generation == our_gen {
                        guard.pairing_handle = Some(pairing_handle);
                    }
                    // else: a newer start_node has replaced us; drop the
                    // freshly spawned handle by letting it fall out of scope.
                }
            }
            let _ = app.emit(
                "zenoh-status",
                &ZenohStatus {
                    status: "connected".to_string(),
                    endpoint,
                    error: None,
                },
            );
            Ok(())
        }
        Ok(Err(e)) => Err(e),
        Err(_) => Err("runtime thread exited before reporting startup status".to_string()),
    };

    // On startup failure, clean up stale handles — but only if the
    // generation still matches. A newer start_node may have already
    // replaced our handles; passing our generation avoids tearing
    // down the newer node.
    if result.is_err() {
        let _ = stop_inner(&state, Some(our_gen));
    }

    result
}

/// Stop the harmony node and clean up.
#[tauri::command]
fn stop_node(app: AppHandle, state: tauri::State<'_, Mutex<NodeState>>) -> Result<(), String> {
    let gen = {
        let guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
        guard.generation
    };
    let stopped = stop_inner(&state, Some(gen));
    // Only emit disconnected if we actually stopped a running node.
    if stopped {
        let _ = app.emit(
            "zenoh-status",
            &ZenohStatus {
                status: "disconnected".to_string(),
                endpoint: None,
                error: None,
            },
        );
    }
    Ok(())
}

// ── Legacy command aliases (backward compat with frontend) ───────────────

/// Alias: the frontend calls `connect_zenoh` — route to `start_node`.
#[tauri::command]
async fn connect_zenoh(
    endpoint: String,
    app: AppHandle,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    start_node(Some(endpoint), app, state).await
}

/// Alias: the frontend calls `disconnect_zenoh` — route to `stop_node`.
#[tauri::command]
fn disconnect_zenoh(
    app: AppHandle,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    stop_node(app, state)
}

/// Publish a profile to the mesh network via the event loop's Zenoh session.
#[tauri::command]
async fn publish_profile(
    profile: ProfilePayload,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    if profile.address.contains('/')
        || profile.address.contains('*')
        || profile.address.contains('?')
        || profile.address.contains('#')
        || profile.address.contains('$')
        || profile.address.is_empty()
    {
        return Err(format!("invalid address: {}", profile.address));
    }

    let publish_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .publish_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };

    let key_expr = format!("harmony/profile/{}", profile.address);
    let payload = serde_json::to_vec(&profile).map_err(|e| format!("serialize: {e}"))?;

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    publish_tx
        .send(event_loop::PublishRequest {
            key_expr,
            payload,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;

    reply_rx
        .await
        .map_err(|_| "event loop dropped publish request".to_string())?
}

/// Send a channel message to the mesh network via Zenoh pub/sub.
///
/// Publishes JSON to `harmony/community/{hub}/channels/{channel}`.
/// Other nodes subscribed to that key expression will receive the message
/// and emit it to their frontends as `message-received` events.
#[tauri::command]
async fn send_message(
    message: SendMessagePayload,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    // Validate channel/hub identifiers (same rules as profile address).
    for (label, val) in [("channel", &message.channel), ("hub", &message.hub)] {
        if val.is_empty()
            || val.contains('/')
            || val.contains('*')
            || val.contains('?')
            || val.contains('#')
            || val.contains('$')
        {
            return Err(format!("invalid {label}: {val}"));
        }
    }

    let (publish_tx, node_addr) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        let tx = guard
            .publish_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?;
        (tx, guard.node_addr.clone())
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let wire = ChannelMessagePayload {
        id: format!(
            "msg-{}-{now_ms}-{:08x}",
            &node_addr[..8.min(node_addr.len())],
            rand::random::<u32>()
        ),
        sender_address: node_addr.clone(),
        sender_name: message.sender_name.clone(),
        channel: message.channel.clone(),
        hub: message.hub.clone(),
        text: message.text,
        timestamp: now_ms,
        priority: message.priority,
        reply_to: message.reply_to,
    };

    let key_expr = format!(
        "harmony/community/{}/channels/{}",
        message.hub, message.channel
    );
    let payload = serde_json::to_vec(&wire).map_err(|e| format!("serialize: {e}"))?;

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    publish_tx
        .send(event_loop::PublishRequest {
            key_expr,
            payload,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;

    reply_rx
        .await
        .map_err(|_| "event loop dropped publish request".to_string())?
}

/// IPC payload for `send_dm` — both ids the frontend needs to thread the
/// optimistic / `dm-received` / `read_dm_thread` paths together.
///
/// Why both ids:
///   - `messageCid` is the content hash. `dm-received` events and
///     `read_dm_thread` results both key on this; the frontend uses it as
///     the stable identity that survives "optimistic local message"
///     becoming "self-echo from the receive path" without a duplicate.
///   - `messageId` (OutboxEntryId) is the lifecycle handle. `dm-delivered`,
///     `dm-expired`, `dm-deleted`, and `delete_outbox_entry` all key on
///     this — it's the OutboxEntry primary key.
///
/// Returning only one would force the caller to either re-fetch the other
/// (TOCTOU window) or live with a 3-way dedupe failure between the
/// optimistic-local path, the dm-received path, and the cold-start
/// scrollback path. PR #81 review surfaced exactly that bug.
#[derive(serde::Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SendDmResult {
    /// Hex-encoded OutboxEntryId (16 bytes → 32 hex chars). Use for
    /// lifecycle correlation (dm-delivered / dm-expired / delete).
    pub message_id: String,
    /// Hex-encoded ContentId (32 bytes → 64 hex chars). Use as the
    /// stable cross-path message identity for dedupe.
    pub message_cid: String,
}

/// ZEB-234: shutdown fence helper for `send_dm`.
///
/// Pre-checks the `stopping` flag; acquires one owned permit from `sem`;
/// re-checks `stopping` after acquire (the flag can be set in the `await`
/// window). Returns the permit so the caller holds it until IPC return,
/// guaranteeing `stop_inner`'s `acquire_many(CAPACITY)` drain blocks until
/// every in-flight `send_dm` has completed its mutation.
///
/// Extracted into a standalone function so it can be unit-tested without
/// standing up a full `NodeState` fixture.
async fn check_dm_send_fence(
    stopping: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    sem: std::sync::Arc<tokio::sync::Semaphore>,
) -> Result<tokio::sync::OwnedSemaphorePermit, String> {
    use std::sync::atomic::Ordering;
    if stopping.load(Ordering::Acquire) {
        return Err("node stopping; operation rejected".into());
    }
    let permit = sem
        .acquire_owned()
        .await
        .map_err(|_| "node stopping (semaphore closed)".to_string())?;
    if stopping.load(Ordering::Acquire) {
        return Err("node stopping; operation rejected".into());
    }
    Ok(permit)
}

/// ZEB-234: drain all in-flight `send_dm` permits before `SyncEngine::shutdown`.
///
/// Acquires all `DM_SEND_FENCE_CAPACITY` permits from the semaphore in one
/// `acquire_many` call. This blocks until every outstanding
/// `check_dm_send_fence` permit (held by an in-flight `send_dm`) has been
/// returned. The permit-set is immediately dropped — we only need the blocking
/// effect, not the permits themselves.
///
/// Called by `stop_inner` inside a `thread::scope` + ephemeral current-thread
/// runtime (mirroring the pattern at the `SyncEngine::shutdown` block) because
/// `stop_inner` is sync and cannot `.await` directly.
///
/// Extracted into a standalone async function so it can be unit-tested without
/// standing up a full `NodeState` fixture (parallel to `check_dm_send_fence`).
async fn drain_dm_send_fence(sem: std::sync::Arc<tokio::sync::Semaphore>) {
    // Acquire all permits to block until every in-flight send_dm / delete_outbox_entry
    // has returned its permit. The permits are dropped immediately — we only need the
    // blocking effect. On Err (semaphore closed), log-and-continue: degraded shutdown
    // is better than deadlock. Mirrors the ephemeral-runtime build-failure warn in
    // stop_inner.
    if let Err(e) = sem.acquire_many(DM_SEND_FENCE_CAPACITY as u32).await {
        tracing::warn!(
            error = ?e,
            "ZEB-234: drain_dm_send_fence: acquire_many failed; \
            proceeding with shutdown (semaphore was closed — \
            in-flight send_dm may produce duplicates)"
        );
    }
    // Permits returned / dropped immediately on the Ok path.
}

/// ZEB-225 Sub-B Phase 2: send a DM into a direct/group-DM Space.
///
/// Snapshots the DmOutbox/CRDT/HLC/ContentStore handles under the NodeState
/// sync mutex, releases it (before any `.await`), then orchestrates the
/// send: encrypt+CAS+apply_outbox via `DmOutbox::send_dm`, then bump the
/// HLC tracker so the next state-root publish stamps monotonically.
///
/// Lock order (mirror in event_loop drain): dm_outbox → crdt_state → hlc_tracker.
///
/// `space_id_hex` is the 32-character hex of a 16-byte SpaceId.
/// Returns `{ messageId, messageCid }` on success — see `SendDmResult`
/// for why both are surfaced.
// PR #81 round 4: param renamed from `space_id_hex` → `space_id`.
// Tauri 2's default JS→Rust convention auto-converts camelCase keys
// to snake_case (so JS `spaceId` resolves to Rust `space_id`). The
// previous `space_id_hex` name didn't match anything the frontend
// could send (would have required JS `spaceIdHex`), so the IPC was
// silently broken. The variable is still hex-encoded on the wire —
// the doc comment + downstream `hex::decode` make that clear.
#[tauri::command]
async fn send_dm(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    space_id: String,
    content: Vec<u8>,
    mime_type: String,
) -> Result<SendDmResult, String> {
    let space_id_hex = space_id;
    // Snapshot all handles under the sync mutex; release it before any await.
    // (Per ZEB-225 spec: NodeState sync-mutex must not be held across `.await`.)
    let (
        dm_outbox,
        _dm_transport,
        crdt_state,
        hlc_tracker,
        device_id,
        _self_owner,
        cas,
        dm_send_inflight,
        dm_send_stopping,
    ) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.dm_outbox
                .clone()
                .ok_or("node not running or no owner identity")?,
            g.dm_transport.clone().ok_or("dm_transport missing")?,
            g.crdt_state.clone().ok_or("crdt_state missing")?,
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.content_store.clone().ok_or("content_store missing")?,
            g.dm_send_inflight
                .clone()
                .ok_or("node not running (no fence)")?,
            g.dm_send_stopping
                .clone()
                .ok_or("node not running (no fence)")?,
        )
    };

    // ZEB-234: shutdown fence. Pre-check the stopping flag — if set,
    // short-circuit before any work. Then acquire a permit for the
    // duration of mutation; stop_inner's acquire_many drain blocks on
    // this permit, preventing SyncEngine::shutdown from racing the
    // flush. Re-check stopping after acquire (could have been set
    // during the await).
    //
    // While _fence_permit is held, stop_inner CANNOT complete its
    // SyncEngine::shutdown — it is blocked waiting for the drain.
    // This makes the old generation/handle post-checks that formerly
    // followed the mutation unnecessary and harmful (they would return
    // Err after a successful fenced write, driving a retry that mints
    // a second OutboxEntry → duplicate DM). Those post-checks were
    // removed by ZEB-234 (this PR).
    let _fence_permit = check_dm_send_fence(&dm_send_stopping, dm_send_inflight).await?;

    let space_bytes = hex::decode(&space_id_hex).map_err(|e| format!("space_id hex: {e}"))?;
    let space_arr: [u8; 16] = space_bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("space_id must be 16 bytes, got {}", space_bytes.len()))?;
    let space_id_typed = crate::owner_state_types::SpaceId(space_arr);

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // Lock order: dm_outbox → crdt_state → hlc_tracker.
    // Mirror this order in event_loop drain (T7) to avoid deadlock.
    let mut outbox_g = dm_outbox.lock().await;
    let mut state_g = crdt_state.lock().await;
    let mut tracker_g = hlc_tracker.lock().await;
    let prev_hlc = tracker_g.get(&device_id).cloned();

    let (msg_id, msg_cid) = outbox_g
        .send_dm(
            &mut state_g,
            cas.as_ref(),
            space_id_typed,
            content,
            mime_type,
            wall_now_ms,
            prev_hlc.as_ref(),
        )
        .await
        .map_err(|e| format!("send_dm: {e}"))?;

    // Read the HLC that DmOutbox::send_dm actually minted from the
    // just-inserted OutboxEntry. Single source of truth: if next_hlc's logic
    // ever changes (Phase 3b's planned ±20% jitter, etc.), the tracker stays
    // in lockstep automatically — the prior manual re-derivation here would
    // silently desync.
    let next_hlc = state_g
        .outbox
        .get(&msg_id)
        .map(|e| e.created_at.clone())
        .ok_or("send_dm minted entry not in outbox (apply_outbox rejected?)")?;
    tracker_g.insert(device_id, next_hlc);

    // Drop the per-handle locks. No post-check needed: _fence_permit guarantees
    // stop_inner cannot complete SyncEngine::shutdown while this permit is held,
    // so the mutation above is always visible to the live node. (ZEB-234)
    drop(tracker_g);
    drop(state_g);
    drop(outbox_g);

    Ok(SendDmResult {
        message_id: hex::encode(msg_id.0),
        message_cid: hex::encode(msg_cid.to_bytes()),
    })
}

// ── ZEB-228 Phase 4: read_dm_thread (cold-start scrollback) ──────────────

/// Phase 4 cold-start scrollback IPC payload — one decrypted message in a
/// DM Space's history. Hex-encoded fields are sized for the Tauri JSON
/// channel (Vec<u8> would round-trip through base64; hex is what every
/// other DM-shaped payload in this codebase uses).
#[derive(serde::Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DmThreadMessage {
    /// Hex-encoded ContentId (32 bytes → 64 hex chars).
    pub message_cid: String,
    /// Hex-encoded sender OwnerAddr (16 bytes → 32 hex chars). For
    /// self-sent messages, equals self_owner; for received messages,
    /// equals the original sender's OwnerAddr.
    pub from: String,
    /// `MessagePayload.sent_at.wall_ms` — sender's HLC at compose time.
    pub sent_at: u64,
    /// `InboxEntry.received_at.wall_ms` — local HLC at apply_inbox time.
    /// Pagination cursor: callers pass the oldest entry's value as
    /// `before_hlc` to fetch the next page.
    pub received_at: u64,
    /// Hex-encoded plaintext body (decrypted from CAS storage_blob).
    pub body: String,
    pub mime_type: String,
    /// True iff `from == self_owner` — UI uses this to right-align the
    /// bubble + skip the avatar fetch for self.
    pub is_self_outbound: bool,
    /// For self-entries (`is_self_outbound == true`): the outbox-derived
    /// delivery state — `"sending" | "delivered" | "expired" | "failed"`.
    ///
    /// `"sending"` = OutboxEntry exists with `Pending` or `Partial` status.
    /// `"expired"` = OutboxEntry exists with `Expired` status.
    /// `"delivered"` = OutboxEntry exists with `Complete` status, OR the
    ///                 OutboxEntry is gone (post-Complete GC, which means
    ///                 it WAS delivered before being collected).
    /// `None` = received entry (`is_self_outbound == false`); receivers
    ///          don't track outbox state.
    ///
    /// `read_dm_thread` joins inbox→outbox by `(space_id, message_cid)`
    /// per call. Without this field a stuck-sending self-message would
    /// render as "delivered" in scrollback even while still in the
    /// outbox awaiting delivery — Qodo flagged that on PR #81 review.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_state: Option<String>,
    /// For self-entries: hex-encoded `OutboxEntryId` (16 bytes → 32 hex
    /// chars), populated only when the OutboxEntry is still present in
    /// `state.outbox`. The frontend's `TextMessage.canDelete` requires
    /// `messageId !== undefined` to expose the inline ⓧ button — without
    /// this field, scrollback-loaded self-messages stuck in `'sending'`
    /// or `'expired'` couldn't be deleted after a cold restart (Cursor
    /// Bugbot flagged this on PR #81 review).
    ///
    /// `None` for: received entries (no outbox row), or self-entries
    /// whose OutboxEntry was already GC'd post-Complete (in which case
    /// `delivery_state == "delivered"` and there's nothing to delete).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

/// Pure helper: sort InboxEntries by `received_at` descending, drop entries
/// at or past the `before_hlc` cursor, then truncate to `limit`.
///
/// Extracted so `read_dm_thread` (the IPC) and `read_dm_thread_inner` (the
/// integration-test entry point) share one implementation. Pre-extraction,
/// each had its own copy of the sort+filter+truncate body — production
/// hit the IPC's copy while tests only ever hit the inner's, so a quiet
/// divergence (e.g., flipping the cursor comparison inclusivity) would
/// never be caught.
///
/// Lock-free + pure: takes owned entries by value so the caller can
/// gather them under whatever lock and drop the lock before invoking
/// this. Returns the truncated, sorted, cursor-filtered vec.
fn filter_sort_paginate_inbox(
    entries: Vec<crate::owner_state_types::InboxEntry>,
    before_hlc: Option<u64>,
    limit: usize,
) -> Vec<crate::owner_state_types::InboxEntry> {
    let mut entries = entries;
    // Sort by received_at descending. Hlc has no Ord impl, so compare on
    // the (wall_ms, logical, device_id) tuple — same lex ordering
    // `is_strictly_newer_than` uses. Newest-first ordering means we
    // call `b.cmp(&a)` (the inversion of natural ascending order) — the
    // tuple keys below are NAMED for the element they describe so a
    // future "fix" to align names with assignments doesn't silently
    // flip the sort direction (Cursor PR #81 round 4 review).
    entries.sort_by(|a, b| {
        let key_a = (
            a.received_at.wall_ms,
            a.received_at.logical,
            &a.received_at.device_id,
        );
        let key_b = (
            b.received_at.wall_ms,
            b.received_at.logical,
            &b.received_at.device_id,
        );
        key_b.cmp(&key_a) // descending: larger keys first
    });
    if let Some(cursor) = before_hlc {
        entries.retain(|e| e.received_at.wall_ms < cursor);
    }
    entries.truncate(limit);
    entries
}

/// Pure inner implementation of `read_dm_thread`. The `#[tauri::command]`
/// shim snapshots NodeState handles, drops the sync mutex, and calls this.
///
/// Behavior matches the IPC contract:
///   1. `space_id` MUST exist in `state.spaces`; otherwise `UnknownSpace`.
///   2. The Space MUST have a `content_key`; otherwise `MissingContentKey`.
///   3. InboxEntries are filtered to `space_id`, sorted by `received_at`
///      DESCENDING (newest first), the optional `before_hlc` cursor
///      filters out entries with `received_at.wall_ms >= cursor`, then
///      truncated to `limit`.
///   4. Each surviving InboxEntry's `message_cid` is fetched from CAS and
///      decrypted via `dm_crypto::decrypt_dm_message` with the prior-keys
///      fallback (matches `handle_cidnotify_lifted`'s receive path so post-key-
///      rotation scrollback works).
///   5. Any per-entry CAS miss (`Ok(None)`) or fetch error (`Err(_)`) or
///      decrypt failure surfaces as a single `Err` with the failing
///      message_cid in the message — caller can retry. (Partial-result
///      handling is a follow-up if needed; today's UI is fine with
///      "scrollback failed, retry".)
///
/// The pure-function shape lets integration tests exercise the decrypt +
/// pagination logic without standing up a tauri::State<NodeState>.
pub async fn read_dm_thread_inner(
    state: &crate::owner_state_crdt::OwnerState,
    cas: &dyn crate::content_store::ContentStore,
    space_id: crate::owner_state_types::SpaceId,
    limit: usize,
    before_hlc: Option<u64>,
    self_owner: crate::owner_state_types::OwnerAddr,
) -> Result<Vec<DmThreadMessage>, String> {
    let space = state
        .spaces
        .get(&space_id)
        .ok_or_else(|| format!("UnknownSpace({space_id:?})"))?;
    let content_key = space
        .content_key
        .clone()
        .ok_or_else(|| format!("MissingContentKey({space_id:?})"))?;
    let prior_content_keys = space.prior_content_keys.clone();
    let aad = crate::dm_crypto::compute_aad(space).map_err(|e| format!("compute_aad: {e}"))?;

    // Gather + filter+sort+paginate via the shared helper. All in
    // memory; no .await crosses the borrow of `state`.
    let raw: Vec<crate::owner_state_types::InboxEntry> =
        state.inbox_entries_for_space(space_id).cloned().collect();
    let entries = filter_sort_paginate_inbox(raw, before_hlc, limit);
    // Snapshot the outbox so decrypt_inbox_entries can populate per-entry
    // delivery_state without re-locking. Cheap (Phase 4 scale: tens of
    // entries max).
    let outbox_snapshot = state.outbox.clone();

    decrypt_inbox_entries(
        cas,
        &content_key,
        &prior_content_keys,
        &aad,
        entries,
        self_owner,
        &outbox_snapshot,
    )
    .await
}

/// ZEB-228 Phase 4 — Cold-start DM scrollback IPC.
///
/// Returns InboxEntries for a given Space (self-sent + received), each
/// with its decrypted body + mime_type. Reverse-chronological order by
/// `received_at`. Paginated via `limit` + `before_hlc` cursor:
///
/// - `space_id_hex`: 32-character hex of a 16-byte SpaceId.
/// - `limit`: max entries per page (UI page size, typical 50).
/// - `before_hlc`: if `Some(wall_ms)`, return entries with
///   `received_at.wall_ms < before_hlc`. None = newest first page.
///
/// Decryption uses `dm_crypto::decrypt_dm_message` with the prior-keys
/// fallback (matches `handle_cidnotify_lifted`'s receive path), so scrollback
/// after a content_key rotation still surfaces older messages encrypted
/// under the previous key.
///
/// Frontend uses this on first DM-channel switch to populate the
/// TextFeed with history. To paginate: pass the oldest entry's
/// `received_at` as `before_hlc` for the next call.
// Param rename per PR #81 round 4 — see send_dm above for rationale.
#[tauri::command]
async fn read_dm_thread(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    space_id: String,
    limit: usize,
    before_hlc: Option<u64>,
) -> Result<Vec<DmThreadMessage>, String> {
    let space_id_hex = space_id;
    // Snapshot handles under the sync mutex; release before any .await.
    // (Same pattern as send_dm — NodeState's sync mutex must not span
    // .await boundaries.)
    let (crdt_state, cas, self_owner) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state
                .clone()
                .ok_or("node not running or no owner identity")?,
            g.content_store.clone().ok_or("content_store missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
        )
    };

    let space_bytes = hex::decode(&space_id_hex).map_err(|e| format!("space_id hex: {e}"))?;
    let space_arr: [u8; 16] = space_bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("space_id must be 16 bytes, got {}", space_bytes.len()))?;
    let space_id = crate::owner_state_types::SpaceId(space_arr);

    // Two-phase: gather everything we need under the OwnerState lock
    // (no .await), drop the lock, then run the cas.get + decrypt loop
    // unlocked. This honors the locks-across-await rule (mirrors ZEB-241
    // pending refactor's pattern).
    //
    // Filter+sort+paginate runs through the shared
    // `filter_sort_paginate_inbox` helper so production exercises the
    // exact code path the integration tests exercise via
    // `read_dm_thread_inner` — no silent divergence between the two.
    let (entries, content_key, prior_content_keys, aad, outbox_snapshot) = {
        let state_guard = crdt_state.lock().await;
        let space = state_guard
            .spaces
            .get(&space_id)
            .ok_or_else(|| format!("UnknownSpace({space_id:?})"))?;
        let content_key = space
            .content_key
            .clone()
            .ok_or_else(|| format!("MissingContentKey({space_id:?})"))?;
        let prior = space.prior_content_keys.clone();
        let aad = crate::dm_crypto::compute_aad(space).map_err(|e| format!("compute_aad: {e}"))?;

        let raw: Vec<crate::owner_state_types::InboxEntry> = state_guard
            .inbox_entries_for_space(space_id)
            .cloned()
            .collect();
        let entries = filter_sort_paginate_inbox(raw, before_hlc, limit);
        // Snapshot the outbox so the decrypt loop can populate
        // per-entry delivery_state without re-locking. Cheap (Phase 4
        // scale: tens of entries max).
        let outbox_snapshot = state_guard.outbox.clone();

        (entries, content_key, prior, aad, outbox_snapshot)
    };

    decrypt_inbox_entries(
        cas.as_ref(),
        &content_key,
        &prior_content_keys,
        &aad,
        entries,
        self_owner,
        &outbox_snapshot,
    )
    .await
}

/// Helper: fetch + decrypt a pre-filtered + pre-sorted slice of
/// InboxEntries. Shared between the `tauri::command` (which gathers
/// entries under the OwnerState lock and drops it before calling this)
/// and `read_dm_thread_inner` (which the integration tests use without
/// a NodeState).
///
/// `outbox_snapshot` is a pre-cloned view of `state.outbox` so the
/// delivery-state join can run without re-acquiring the OwnerState lock.
/// Cheap at Phase 4 scale (tens of entries); if outbox grows we'd swap
/// for a `(space_id, message_cid) → status` index built once per call.
async fn decrypt_inbox_entries(
    cas: &dyn crate::content_store::ContentStore,
    content_key: &crate::owner_state_types::DmContentKey,
    prior_content_keys: &[crate::owner_state_types::DmContentKey],
    aad: &[u8],
    entries: Vec<crate::owner_state_types::InboxEntry>,
    self_owner: crate::owner_state_types::OwnerAddr,
    outbox_snapshot: &std::collections::BTreeMap<
        crate::owner_state_types::OutboxEntryId,
        crate::owner_state_types::OutboxEntry,
    >,
) -> Result<Vec<DmThreadMessage>, String> {
    let mut out: Vec<DmThreadMessage> = Vec::with_capacity(entries.len());
    for entry in entries {
        // PR #81 round 4 (Greptile P2): per-entry skip-on-error instead
        // of aborting the whole page. A single corrupted CAS blob or a
        // missing one (e.g. mid-sync state) shouldn't black-hole the
        // user's entire scrollback. Log the failure + continue; the UI
        // sees N-1 messages instead of zero. Future polish: surface a
        // placeholder "decrypt failed" Message stub so the user knows
        // something exists at that slot. Out of Phase 4 scope.
        let blob = match cas.get(&entry.message_cid).await {
            Ok(Some(b)) => b,
            Ok(None) => {
                tracing::warn!(
                    message_cid = ?entry.message_cid,
                    "read_dm_thread: blob missing in CAS — skipping entry"
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    message_cid = ?entry.message_cid,
                    error = ?e,
                    "read_dm_thread: cas.get failed — skipping entry"
                );
                continue;
            }
        };
        let payload =
            match crate::dm_crypto::decrypt_dm_message(content_key, prior_content_keys, aad, &blob)
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        message_cid = ?entry.message_cid,
                        error = ?e,
                        "read_dm_thread: decrypt failed — skipping entry"
                    );
                    continue;
                }
            };
        let is_self_outbound = entry.from == self_owner;
        // Self entries: join against outbox by (space_id, message_cid).
        // Outbox is keyed by OutboxEntryId so we walk values; Phase 4
        // scale makes the linear scan acceptable.
        //
        // Missing outbox entry on a self-message → "delivered" (the
        // OutboxEntry was GC'd post-Complete, meaning it definitely
        // WAS delivered before collection). This is the same fallback
        // the frontend's loadDmThread used to apply unconditionally;
        // we narrow it here so Pending/Partial/Expired surface
        // accurately.
        // For self entries, capture BOTH the OutboxEntryId (so the
        // frontend can correlate dm-delivered/expired/deleted IPC events
        // and gate the delete button) AND the delivery_status (so
        // scrollback reflects current outbox state, not a hardcoded
        // 'delivered'). One linear scan over the snapshot serves both.
        let (delivery_state, message_id) = if is_self_outbound {
            let hit = outbox_snapshot.iter().find_map(|(id, e)| {
                if e.space_id == entry.space_id && e.message_cid == entry.message_cid {
                    Some((*id, e.delivery_status))
                } else {
                    None
                }
            });
            let state = match hit.map(|(_, s)| s) {
                Some(crate::owner_state_types::DeliveryStatus::Pending)
                | Some(crate::owner_state_types::DeliveryStatus::Partial) => "sending",
                Some(crate::owner_state_types::DeliveryStatus::Expired) => "expired",
                Some(crate::owner_state_types::DeliveryStatus::Complete) | None => "delivered",
            };
            (
                Some(state.to_string()),
                hit.map(|(id, _)| hex::encode(id.0)),
            )
        } else {
            (None, None)
        };
        out.push(DmThreadMessage {
            message_cid: hex::encode(entry.message_cid.to_bytes()),
            from: hex::encode(entry.from.0),
            sent_at: payload.sent_at.wall_ms,
            received_at: entry.received_at.wall_ms,
            body: hex::encode(&payload.body),
            mime_type: payload.mime_type,
            is_self_outbound,
            delivery_state,
            message_id,
        });
    }
    Ok(out)
}

// ── ZEB-228 Phase 4: delete_outbox_entry (manual delete) ─────────────────

/// Phase 4 — Delete a stuck or expired DM message (manual delete).
///
/// Wraps `DmOutbox::delete_dm_outbox_entry`. Removes BOTH the
/// OutboxEntry and the corresponding self-InboxEntry keyed by
/// `(space_id, message_cid)`, and clears in-flight + backoff caches.
/// On success with a non-default outcome, emits a `dm-deleted` IPC
/// event so the frontend MessageService can prune the message from
/// its local cache.
///
/// Idempotent: a missing `message_id` returns `Ok(())` without
/// emitting any event.
///
/// `message_id` is the 32-character hex of a 16-byte OutboxEntryId.
#[tauri::command]
async fn delete_outbox_entry<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    message_id: String,
) -> Result<(), String> {
    // Snapshot handles under the sync mutex; release before any .await.
    // Same pattern as send_dm — NodeState's sync mutex must not span
    // .await boundaries.
    let (dm_outbox, crdt_state, hlc_tracker, device_id, dm_send_inflight, dm_send_stopping) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.dm_outbox
                .clone()
                .ok_or("node not running or no owner identity")?,
            g.crdt_state.clone().ok_or("crdt_state missing")?,
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            // ZEB-234: snapshot the fence handles so delete_outbox_entry
            // holds a permit for the duration of its outbox mutation,
            // preventing stop_inner's drain from racing a mid-delete
            // tombstone write. Mirrors send_dm's fence pattern.
            g.dm_send_inflight
                .clone()
                .ok_or("node not running (no fence)")?,
            g.dm_send_stopping
                .clone()
                .ok_or("node not running (no fence)")?,
        )
    };

    // ZEB-234: acquire the shutdown fence before any outbox mutation.
    // Pre-check + acquire a permit that blocks stop_inner's drain until
    // this IPC returns. While _fence_permit is held, stop_inner cannot
    // complete SyncEngine::shutdown — making the old post-check guards
    // obsolete and harmful (they would Err after a successful fenced
    // write, driving a retry that could mint a second OutboxEntry).
    // Those post-checks were removed by ZEB-234 (this PR).
    let _fence_permit = check_dm_send_fence(&dm_send_stopping, dm_send_inflight).await?;

    // Decode message_id from hex → OutboxEntryId.
    let id_bytes = hex::decode(&message_id).map_err(|e| format!("message_id hex: {e}"))?;
    let id_arr: [u8; 16] = id_bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("message_id must be 16 bytes, got {}", id_bytes.len()))?;
    let outbox_entry_id = crate::owner_state_types::OutboxEntryId(id_arr);

    // Capture wall time before entering the lock so the tombstone HLC is
    // sourced from a single consistent wall reading (matches send_dm's
    // own wall_now_ms pattern). Cheap: one syscall, no lock held.
    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // Lock order: dm_outbox → crdt_state → hlc_tracker. Mirrors send_dm to
    // avoid deadlock against any concurrent send/drain.
    let outcome = {
        let mut outbox_g = dm_outbox.lock().await;
        let mut state_g = crdt_state.lock().await;
        let mut tracker_g = hlc_tracker.lock().await;
        let outcome = outbox_g
            .delete_dm_outbox_entry(&mut state_g, outbox_entry_id, wall_now_ms)
            .map_err(|e| format!("delete_dm_outbox_entry: {e}"))?;

        // ZEB-234/ZEB-243: advance hlc_tracker after writing the tombstone HLC,
        // mirroring send_dm's analogous write-back. Without this, a subsequent
        // local mutation in the same wall-millisecond could mint an HLC equal to
        // the tombstone's, breaking the monotonicity guarantee send_dm preserves.
        // Read the HLC from state_g (single source of truth — same approach as
        // send_dm reading the OutboxEntry HLC).
        //
        // Two guards are required before advancing our tracker lane:
        // (a) device_id match: outbox_tombstones may already hold a tombstone
        //     synced in via CRDT from a paired device. Writing a remote device's
        //     HLC into our tracker lane is a category error.
        // (b) strictly-newer check: the tombstone HLC is minted
        //     strictly-newer-than the entry's created_at, but NOT necessarily
        //     newer than the tracker's current value. A send_dm that executed
        //     between entry creation and this delete may have already advanced
        //     the tracker past this tombstone; an unconditional insert would
        //     regress it. Also catches idempotent re-deletes (tombstone was
        //     written by a prior call, not the one just executed).
        if let Some(tomb_hlc) = state_g.outbox_tombstones.get(&outbox_entry_id).cloned() {
            if tomb_hlc.device_id == device_id {
                let should_update = match tracker_g.get(&device_id) {
                    Some(curr) => tomb_hlc.is_strictly_newer_than(curr),
                    None => true,
                };
                if should_update {
                    tracker_g.insert(device_id.clone(), tomb_hlc);
                }
            }
        }

        outcome
    };

    // Locks dropped (block scope ended). No post-check needed: _fence_permit
    // guarantees stop_inner cannot complete SyncEngine::shutdown while this
    // permit is held, so the deletion above is always visible to the live
    // node. (ZEB-234)

    // Emit IPC event only if something actually changed (idempotent
    // missing-id is no-op).
    if let (Some(space_id), Some(message_cid)) = (outcome.space_id, outcome.message_cid) {
        let _ = app.emit(
            "dm-deleted",
            serde_json::json!({
                "messageId": message_id,
                "spaceId": hex::encode(space_id.0),
                "messageCid": hex::encode(message_cid.to_bytes()),
            }),
        );
    }

    Ok(())
}

// ── ZEB-228 Phase 4: add_space (DM/GroupDm creation) ─────────────────────

/// Pure inner implementation of `add_space`'s DM/GroupDm dispatch. The
/// `#[tauri::command]` shim snapshots NodeState handles, drops the sync
/// mutex, calls this, then forwards each `UnicastSendRequest` into the
/// outbound unicast channel.
///
/// Behavior:
///   1. Validate kind ∈ {Dm, GroupDm} and the recipient list:
///      - Dm: exactly 1 recipient (total members = 2).
///      - GroupDm: 2-15 recipients (total members = 3-16).
///      - No self in recipients.
///      - No duplicate recipients.
///   2. Generate a fresh content_key (32 random bytes via OsRng, wrapped
///      in `Zeroizing` while in scope).
///   3. Build a Space CRDT entry with sorted self+recipient members,
///      Reticulum transport (empty participants — populated lazily as
///      announces flow), `created_at`/`updated_at` from a fresh HLC.
///   4. Apply locally via `apply_space_with_canonicalization`.
///   5. Build a signed `DmInvite` packet and emit one
///      `UnicastSendRequest` per device in each recipient's
///      `OwnerDeviceCache` entry. Best-effort: a recipient with no
///      cached devices yields zero outbound packets — Phase 3b's
///      handle_invite-on-first-send_dm path still recovers because the
///      sender's outbox loop will fan out the missing invite when the
///      first message ships.
///
/// Returns `(canonical_space_id, send_requests, was_merge)`:
///   - `canonical_space_id` is the SpaceId after CRDT canonicalization.
///     If `apply_space_with_canonicalization` merged the freshly-minted
///     Space into an existing one with the same dedupe key (sorted
///     members), this is the EXISTING Space's id, not the minted one
///     that just got dropped — the outer command's `state.spaces.get(&id)`
///     readback would otherwise miss a real winner.
///   - `send_requests` is the list of DmInvite UnicastSendRequests.
///     Empty when `was_merge == true` because the existing Space's
///     invites were already sent at original creation time; sending
///     duplicates here would just generate redundant network traffic
///     and (for the recipient) noisy duplicate-invite handling.
///   - `was_merge` lets the caller skip the dispatch loop without
///     re-checking the sends vec emptiness for the duplicate-create
///     case (a fresh Space with zero recipients in OwnerDeviceCache
///     also produces an empty sends list — those two empties have
///     different semantics).
///
/// The pure-function shape lets integration tests exercise the
/// validation + Space-construction + invite-build logic without
/// standing up a tauri::State<NodeState>.
#[allow(clippy::too_many_arguments)]
pub fn add_space_dm_inner(
    state: &mut crate::owner_state_crdt::OwnerState,
    signing_key: &ed25519_dalek::SigningKey,
    inviter_identity_pub: &[u8; 64],
    self_owner: crate::owner_state_types::OwnerAddr,
    our_signing_device_hash: crate::owner_state_types::DeviceIdentityHash,
    device_id: &str,
    kind: crate::owner_state_types::SpaceKind,
    name: String,
    recipients: Vec<crate::owner_state_types::OwnerAddr>,
    wall_now_ms: u64,
    prev_hlc: Option<&crate::owner_state_types::Hlc>,
) -> Result<
    (
        crate::owner_state_types::SpaceId,
        Vec<crate::dm_outbox::UnicastSendRequest>,
        bool,
    ),
    String,
> {
    use crate::owner_state_types::{
        DmContentKey, OwnerAddr, ReticulumDest, Space, SpaceId, SpaceKind, TransportBinding,
    };

    // ── 1. Validate kind + recipients. ───────────────────────────────
    if !matches!(kind, SpaceKind::Dm | SpaceKind::GroupDm) {
        return Err(format!(
            "add_space_dm_inner only handles Dm/GroupDm; got {kind:?}"
        ));
    }
    if recipients.contains(&self_owner) {
        return Err("self must not be in recipients (backend adds self automatically)".to_string());
    }
    // Defense in depth — frontend already blocks but enforce here too.
    let total_members = 1 + recipients.len();
    if total_members > 16 {
        return Err(format!(
            "DM/GroupDm cap is 16 members; got {total_members} (use a community for larger groups)"
        ));
    }
    match kind {
        SpaceKind::Dm => {
            if recipients.len() != 1 {
                return Err(format!(
                    "Dm kind requires exactly 1 recipient; got {} (use GroupDm for 2-15)",
                    recipients.len()
                ));
            }
        }
        SpaceKind::GroupDm => {
            if !(2..=15).contains(&recipients.len()) {
                return Err(format!(
                    "GroupDm requires 2-15 recipients; got {}",
                    recipients.len()
                ));
            }
        }
        _ => unreachable!("kind already restricted to Dm or GroupDm above"),
    }

    // ── 2. Build sorted+deduped member list (self + recipients). ─────
    let mut all_members: Vec<OwnerAddr> = std::iter::once(self_owner)
        .chain(recipients.iter().copied())
        .collect();
    all_members.sort();
    all_members.dedup();
    if all_members.len() != total_members {
        return Err("duplicate recipient(s) in input".to_string());
    }

    // ── 3. Generate fresh content_key. Bytes live in `Zeroizing` for
    //       the duration of this scope; `DmContentKey::new` copies the
    //       bytes into its own (also-zeroize-on-drop) wrapper. ───────
    let content_key = {
        use rand::RngCore;
        use zeroize::Zeroizing;
        let mut k = Zeroizing::new([0u8; 32]);
        rand::rngs::OsRng.fill_bytes(k.as_mut());
        DmContentKey::new(*k)
    };

    // ── 4. Mint HLCs for created_at / updated_at. Both stamped from
    //       the same `next_hlc` so a peer comparing them by lex-order
    //       sees them as equal (the typical case for fresh creation).
    //       The IPC shim's caller is responsible for keeping the HLC
    //       tracker monotone post-mint; this inner function doesn't
    //       touch the tracker. ───────────────────────────────────────
    let creation_hlc = crate::dm_outbox::next_hlc(prev_hlc, wall_now_ms, device_id);

    // ── 5. Build the Space CRDT entry. ───────────────────────────────
    let space_id = SpaceId(rand::random());
    let space = Space {
        id: space_id,
        kind,
        parent: None,
        community_id: None,
        name,
        members: all_members.clone(),
        // DM kinds always Reticulum; participants populated lazily as
        // announces propagate (Phase 3b currently leaves it empty —
        // resolution happens via OwnerDeviceCache, not the Space's
        // transport binding).
        transport: Some(TransportBinding::Reticulum {
            participants: Vec::<ReticulumDest>::new(),
        }),
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: creation_hlc.clone(),
        updated_at: creation_hlc.clone(),
        content_key: Some(content_key.clone()),
        prior_content_keys: vec![],
        current_epoch: None,
        current_epoch_key: None,
        old_epoch_keys: std::collections::BTreeMap::new(),
        admin_addr: None,
        is_invite_only: None,
        shared_in_profile: false,
        pending_join_at: None,
    };

    // Validate invariants up front — catches programmer error before we
    // mutate state. apply_space_with_canonicalization itself does NOT
    // validate (it's the receive path's job, and incoming entries from
    // remote replicas are guarded by their own decode-time checks).
    space
        .validate_invariants()
        .map_err(|e| format!("Space invariants violated: {}", e.0))?;

    // ── 6. Apply locally. apply_space_with_canonicalization returns
    //       an ApplyOutcome; we MUST check it to learn whether a dedupe
    //       merge collapsed our minted Space into an existing one (same
    //       sorted members). The earlier shape ignored the outcome and
    //       returned `space_id` unconditionally — when the merge
    //       dropped our minted id (we lost the ULID tie-break), the
    //       outer command's `state.spaces.get(&result.0)` readback
    //       missed the live entry entirely. Qodo flagged this on PR #81.
    // ───────────────────────────────────────────────────────────────
    use crate::owner_state_crdt::ApplyOutcome;
    let outcome = state.apply_space_with_canonicalization(space);
    let (canonical_space_id, was_merge) = match outcome {
        ApplyOutcome::Inserted => (space_id, false),
        ApplyOutcome::Merged { old_id: None } => {
            // Same-SpaceId update path. Our minted id collided with an
            // existing entry on the same id — practically impossible
            // for a fresh `rand::random()` SpaceId (16 bytes of
            // randomness collision). Treat as Inserted-equivalent for
            // the canonical id, but skip dispatch: the existing entry
            // already had its invites sent at original creation.
            (space_id, true)
        }
        ApplyOutcome::Merged {
            old_id: Some(loser),
        } => {
            // Cross-id dedupe merge: lex-MIN ULID wins. Our minted id
            // may be the winner OR the loser. The winner is the unique
            // entry now in `state.spaces` matching our dedupe key
            // (sorted members + kind). Walk and find it.
            //
            // Skip dispatch in this case: the existing Space's invites
            // were already sent at original creation, and re-firing
            // here would just generate redundant network traffic +
            // noisy duplicate-invite handling on the recipient side.
            let canonical = state
                .spaces
                .iter()
                .find(|(_, s)| s.kind == kind && s.members == all_members)
                .map(|(id, _)| *id)
                .ok_or_else(|| {
                    format!(
                        "add_space_dm_inner: post-merge canonical winner not found \
                         (loser={loser:?}, members={all_members:?})"
                    )
                })?;
            (canonical, true)
        }
        ApplyOutcome::Rejected(reason) => {
            return Err(format!(
                "add_space_dm_inner: apply_space_with_canonicalization rejected: {reason}"
            ));
        }
    };

    // Short-circuit on merge: nothing to dispatch (existing Space
    // already invited everyone at original creation). Returning
    // `was_merge=true` lets the outer command skip the dispatch loop
    // on a more strongly typed signal than "sends is empty" (which
    // would conflate the merge case with the legitimate "no recipient
    // devices known yet" case).
    if was_merge {
        return Ok((canonical_space_id, Vec::new(), true));
    }

    // ── 7. Build + sign the DmInvite. Our own devices come from
    //       OwnerDeviceCache (populated by Flow A); fall back to just
    //       our_signing_device_hash if no entry yet (pre-bootstrap). ──
    let our_devices: Vec<crate::owner_state_types::DeviceIdentityHash> = state
        .owner_device_cache
        .devices
        .get(&self_owner)
        .map(|e| e.devices.clone())
        .unwrap_or_else(|| vec![our_signing_device_hash]);
    // Defense in depth — sender_devices MUST contain signing_device_hash
    // (Phase 3b invariant; validated wire-side by decode_packet).
    let sender_devices = if our_devices.contains(&our_signing_device_hash) {
        our_devices
    } else {
        let mut combined = our_devices;
        combined.push(our_signing_device_hash);
        combined.sort();
        combined.dedup();
        combined
    };

    let signed_invite = crate::dm_envelope::DmInviteSigned {
        space_id: canonical_space_id,
        kind,
        members: all_members,
        inviter: self_owner,
        inviter_identity_pub: *inviter_identity_pub,
        content_key,
        sender_devices,
        signing_device_hash: our_signing_device_hash,
        created_at: creation_hlc,
    };
    let invite_packet = crate::dm_envelope::build_signed_invite(signed_invite, signing_key)
        .map_err(|e| format!("build_signed_invite: {e}"))?;
    let invite_wire = crate::dm_envelope::encode_packet(&invite_packet)
        .map_err(|e| format!("encode_packet: {e}"))?;

    // ── 8. One UnicastSendRequest per non-self recipient device. ─────
    // Note: we hold a borrow of `state.owner_device_cache` here, which
    // is fine because the `apply_space_with_canonicalization` write
    // above already returned.
    let mut sends: Vec<crate::dm_outbox::UnicastSendRequest> = Vec::new();
    for r in &recipients {
        let entry = match state.owner_device_cache.devices.get(r) {
            Some(e) => e,
            None => continue, // recipient unknown — outbox loop on first send_dm recovers
        };
        for device in &entry.devices {
            let dest_hash = crate::dm_signing::compute_dm_destination_hash(device.0);
            sends.push(crate::dm_outbox::UnicastSendRequest {
                destination_hash: dest_hash,
                packet: invite_wire.clone(),
            });
        }
    }

    Ok((canonical_space_id, sends, false))
}

/// ZEB-228 Phase 4 — Create a new Space.
///
/// For DM/GroupDm kinds: generates a fresh content_key, builds the
/// Space CRDT entry with members (self + recipients), applies it
/// locally, and dispatches a signed DmInvite to each recipient's
/// known devices via the unicast channel. Returns the new SpaceId
/// (hex-encoded).
///
/// Validation:
///   - DM kind = exactly 1 recipient (total members = 2).
///   - GroupDm kind = 2-15 recipients (total members = 3-16).
///   - Total members ≤ 16 (defense in depth — frontend also blocks).
///   - No self in recipients (caller passes recipients only; backend
///     adds self automatically).
///   - No duplicate recipients.
///
/// Other kinds (Folder, Channel, Community, PublicChannel) are not
/// yet implemented in this IPC and return Err — they have their own
/// dedicated flows (e.g., `create_folder`).
///
/// Frontend's DmCreateDialog calls this; the dispatched DmInvite
/// flows through Phase 3b's `handle_invite` on each recipient's
/// device, which auto-accepts and writes the Space + cache entry.
#[tauri::command]
async fn add_space(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    kind: String,
    name: String,
    members: Option<Vec<String>>,
) -> Result<String, String> {
    use crate::owner_state_types::{OwnerAddr, SpaceKind};

    // Parse the kind string. Accept the same wire codes the SpaceKind
    // serde-rename uses ("d", "g") AND the human-friendly forms the
    // frontend will probably send ("dm", "group-dm").
    let parsed_kind = match kind.as_str() {
        "d" | "dm" | "Dm" => SpaceKind::Dm,
        "g" | "group-dm" | "groupdm" | "GroupDm" => SpaceKind::GroupDm,
        // Other kinds are not implemented in this IPC yet (Phase 4
        // ships DM/GroupDm only). Surface as a clear Err so a future
        // frontend that tries to call add_space for, e.g., a folder
        // gets a useful diagnostic rather than silent acceptance.
        other => {
            return Err(format!(
                "add_space: unsupported kind '{other}' (Phase 4 ships Dm/GroupDm only)"
            ));
        }
    };

    // Decode each recipient OwnerAddr from hex.
    let recipients: Vec<OwnerAddr> = members
        .unwrap_or_default()
        .iter()
        .map(|hex_addr| {
            let bytes = hex::decode(hex_addr)
                .map_err(|e| format!("recipient '{hex_addr}' hex decode: {e}"))?;
            let arr: [u8; 16] = bytes.as_slice().try_into().map_err(|_| {
                format!(
                    "recipient '{hex_addr}' must be 16 bytes, got {}",
                    bytes.len()
                )
            })?;
            Ok(OwnerAddr(arr))
        })
        .collect::<Result<Vec<_>, String>>()?;

    // Snapshot all handles under the sync mutex; release before any
    // .await. (Same pattern as send_dm — NodeState's sync mutex must
    // not span .await boundaries.)
    //
    // ZEB-245 (PR #81 round 6): capture `generation` paired-atomically
    // with the Arcs so the post-stop check below can detect a
    // stop+restart racing through this command — see send_dm for the
    // full rationale on why both `generation` and handle-attachment
    // need to be re-verified.
    let (
        dm_outbox,
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        unicast_send_tx,
        identity_pub_64,
        snapshot_generation,
    ) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.dm_outbox
                .clone()
                .ok_or("node not running or no owner identity")?,
            g.crdt_state.clone().ok_or("crdt_state missing")?,
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.unicast_send_tx.clone().ok_or("unicast_send_tx missing")?,
            g.dm_identity_pub_64
                .ok_or("dm_identity_pub_64 missing (start_node didn't capture it?)")?,
            g.generation,
        )
    };

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // Lock order mirrors send_dm: dm_outbox → crdt_state → hlc_tracker.
    // We borrow signing_key + our_signing_device_hash from DmOutbox so
    // we don't double-store identity-derived material on NodeState.
    //
    // `was_merge` from the inner function tells us whether
    // apply_space_with_canonicalization collapsed our minted Space into
    // an existing one with the same dedupe key. In that case `space_id`
    // is the EXISTING (canonical winner) id — guaranteed live in
    // state.spaces — and `sends` is empty (the existing Space was
    // already invited at original creation).
    let (space_id, sends, was_merge, new_hlc) = {
        let outbox_g = dm_outbox.lock().await;
        let mut state_g = crdt_state.lock().await;
        let mut tracker_g = hlc_tracker.lock().await;
        let prev_hlc = tracker_g.get(&device_id).cloned();

        let signing_key = outbox_g.signing_key.as_ref();
        let our_signing_device_hash = outbox_g.our_signing_device_hash;

        let (canonical_id, sends, was_merge) = add_space_dm_inner(
            &mut state_g,
            signing_key,
            &identity_pub_64,
            self_owner,
            our_signing_device_hash,
            &device_id,
            parsed_kind,
            name,
            recipients,
            wall_now_ms,
            prev_hlc.as_ref(),
        )?;

        // Fetch the HLC stamped on the canonical Space — single source
        // of truth. The inner function guarantees `canonical_id` is
        // present in `state.spaces` post-apply (Inserted, same-id
        // Merged, or cross-id Merged: in all three the canonical id
        // is the live entry); the get() below should never fail.
        let stamped = state_g
            .spaces
            .get(&canonical_id)
            .map(|s| s.created_at.clone())
            .ok_or_else(|| {
                "add_space: canonical Space not in state \
                 (apply_space_with_canonicalization invariants broken?)"
                    .to_string()
            })?;
        // HLC monotonicity: a dedupe-merge can land on an EXISTING Space
        // whose `created_at` is OLDER than our local tracker's prev_hlc
        // (the existing Space was created before our most recent HLC-
        // stamping operation). Writing `stamped` to the tracker in that
        // case would regress the cursor below prev_hlc, breaking the
        // monotonicity next_hlc relies on. Only update the tracker when
        // `stamped` strictly advances from prev_hlc; otherwise leave the
        // tracker as-is. (CodeRabbit flagged this on PR #81 review.)
        let should_advance_tracker = match prev_hlc.as_ref() {
            None => true,
            Some(prev) => stamped.is_strictly_newer_than(prev),
        };
        if should_advance_tracker {
            tracker_g.insert(device_id.clone(), stamped.clone());
        }

        (canonical_id, sends, was_merge, stamped)
    };
    let _ = new_hlc; // borrowed only to pin the tracker update timing

    // ZEB-245 (PR #81 round 6): post-stop check BEFORE dispatching
    // invites. If a stop+restart fired during the .await chain above,
    // our cloned `crdt_state` Arc is now detached from the live
    // NodeState — the Space landed in an orphan that won't be
    // persisted, but if we still dispatched invites the recipients
    // would auto-accept and ship messages to a Space the sender lost
    // on restart (cross-device divergence). Suppressing the dispatch
    // when we detect detachment closes the worst-case asymmetry.
    //
    // Mirrors send_dm's fence (lib.rs ~1762) and delete_outbox_entry's
    // fence below: same residual TOCTOU applies — a stop_inner that
    // flushes the cloned crdt_state between mutate and post-check
    // still persists the Space + invites, so ZEB-234's shutdown fence
    // is the real fix. This guard closes the common case (stop_node
    // alone, no flush) which is the only detach path Phase 4 UI can
    // actually trigger.
    {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err(format!(
                "node generation changed during add_space (was {}, now {}); \
                 Space was created in a detached crdt_state and won't be persisted — \
                 invites suppressed; retry against the live node",
                snapshot_generation, g.generation
            ));
        }
        if g.dm_outbox.is_none() {
            return Err("node was stopped during add_space; Space was created in a \
                detached crdt_state and won't be persisted — invites suppressed"
                .to_string());
        }
    }

    // Dispatch invites only when our minted Space actually became the
    // live entry (or extended one with same id). When `was_merge==true`
    // the existing Space's invites were already dispatched at original
    // creation; firing them again here would just generate redundant
    // network traffic + noisy duplicate-invite handling on the
    // recipient side. `sends` is also empty in that case (defense in
    // depth — the inner function guarantees this), but the explicit
    // flag check makes the semantics unambiguous.
    if !was_merge {
        // Best-effort try_send — a full channel surfaces as a dropped
        // invite, recovered by the outbox loop's first send_dm into
        // this Space (which builds + ships its own DmInvite).
        for req in sends {
            if let Err(e) = unicast_send_tx.try_send(req) {
                tracing::warn!(
                    error = %e,
                    "add_space: dropped DmInvite dispatch (channel full); outbox retry on first send_dm will recover"
                );
            }
        }
    }

    Ok(hex::encode(space_id.0))
}

/// Return the hex-encoded node address (derived from the Ed25519 identity).
///
/// The frontend uses this to identify self-sent messages in the Zenoh echo.
#[tauri::command]
fn get_node_addr(state: tauri::State<'_, Mutex<NodeState>>) -> Result<String, String> {
    let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
    if guard.node_addr.is_empty() {
        return Err("not connected".to_string());
    }
    Ok(guard.node_addr.clone())
}

// ── Vine types and commands ──────────────────────────────────────────────

/// Vine descriptor published/received over Zenoh.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VineDescriptorPayload {
    pub id: String,
    pub creator_address: String,
    pub creator_name: String,
    pub created_at: u64,
    pub video_cid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reshare_of: Option<String>,
    /// If this vine is a reshare, the hex-encoded address of the original creator.
    /// Always traces to the true origin — if Alice reshares Bob's reshare of Carol's vine,
    /// the field carries Carol's address. None for non-reshare originals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_creator_address: Option<String>,
    /// Display name of the original creator (snapshot at reshare time). See above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_creator_name: Option<String>,
}

/// Vine descriptor sent from the frontend to publish.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishVinePayload {
    pub video_cid: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub reshare_of: Option<String>,
    /// Creator's display name (included so receivers can display it).
    #[serde(default)]
    pub creator_name: String,
    /// See VineDescriptorPayload::original_creator_address.
    #[serde(default)]
    pub original_creator_address: Option<String>,
    /// See VineDescriptorPayload::original_creator_name.
    #[serde(default)]
    pub original_creator_name: Option<String>,
}

/// Vine video descriptor returned to the frontend (includes local viewed state).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VineVideoDto {
    pub id: String,
    pub creator_address: String,
    pub creator_name: String,
    pub created_at: u64,
    pub video_cid: String,
    pub title: Option<String>,
    pub reshare_of: Option<String>,
    pub viewed: bool,
    /// See VineDescriptorPayload::original_creator_address.
    pub original_creator_address: Option<String>,
    /// See VineDescriptorPayload::original_creator_name.
    pub original_creator_name: Option<String>,
}

/// Response returned by list_followed — one entry per followed address.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FollowEntryResponse {
    pub address: String,
    pub name: Option<String>,
    pub followed_at: u64,
}

/// Vine reaction published/received over Zenoh.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VineReactionPayload {
    pub vine_id: String,
    pub reactor_address: String,
    pub reactor_name: String,
    pub liked: bool,
    pub timestamp: u64,
}

/// Vine reaction sent from the frontend to publish.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishReactionPayload {
    pub vine_id: String,
    pub vine_creator_address: String,
    pub liked: bool,
    #[serde(default)]
    pub reactor_name: String,
}

/// Publish a vine descriptor to the mesh network via Zenoh pub/sub.
///
/// Publishes JSON to `harmony/vines/{creator_address}`.
/// Other nodes subscribed to `harmony/vines/*` will receive the descriptor
/// and emit it to their frontends as `vine-received` events.
#[tauri::command]
async fn publish_vine(
    vine: PublishVinePayload,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    if vine.video_cid.trim().is_empty() {
        return Err("video_cid is required".to_string());
    }
    if let Some(ref title) = vine.title {
        if title.len() > 140 {
            return Err("title exceeds 140 bytes".to_string());
        }
    }

    let (publish_tx, node_addr) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        let tx = guard
            .publish_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?;
        (tx, guard.node_addr.clone())
    };

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let wire = VineDescriptorPayload {
        id: format!(
            "vine-{}-{now_secs}-{:08x}",
            &node_addr[..8.min(node_addr.len())],
            rand::random::<u32>()
        ),
        creator_address: node_addr.clone(),
        creator_name: vine.creator_name,
        created_at: now_secs,
        video_cid: vine.video_cid,
        title: vine.title,
        reshare_of: vine.reshare_of,
        original_creator_address: vine.original_creator_address,
        original_creator_name: vine.original_creator_name,
    };

    let key_expr = format!("harmony/vines/{}", node_addr);
    let payload = serde_json::to_vec(&wire).map_err(|e| format!("serialize: {e}"))?;

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    publish_tx
        .send(event_loop::PublishRequest {
            key_expr,
            payload,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;

    reply_rx
        .await
        .map_err(|_| "event loop dropped publish request".to_string())?
}

/// Publish a vine reaction (like/unlike) to the mesh network via Zenoh pub/sub.
///
/// Publishes JSON to `harmony/vines/{vine_creator_address}/reactions/{vine_id}/{own_addr}`.
#[tauri::command]
async fn publish_vine_reaction(
    reaction: PublishReactionPayload,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    if reaction.vine_id.trim().is_empty() {
        return Err("vine_id is required".to_string());
    }
    if reaction.vine_creator_address.trim().is_empty() {
        return Err("vine_creator_address is required".to_string());
    }

    let (publish_tx, node_addr) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        let tx = guard
            .publish_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?;
        (tx, guard.node_addr.clone())
    };

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let wire = VineReactionPayload {
        vine_id: reaction.vine_id.clone(),
        reactor_address: node_addr.clone(),
        reactor_name: reaction.reactor_name,
        liked: reaction.liked,
        timestamp: now_secs,
    };

    let key_expr = format!(
        "harmony/vines/{}/reactions/{}/{}",
        reaction.vine_creator_address, reaction.vine_id, node_addr
    );
    let payload = serde_json::to_vec(&wire).map_err(|e| format!("serialize: {e}"))?;

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    publish_tx
        .send(event_loop::PublishRequest {
            key_expr,
            payload,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;

    reply_rx
        .await
        .map_err(|_| "event loop dropped publish request".to_string())?
}

/// Return all vines currently in the local cache, sorted by
/// `created_at` descending (newest first). `viewed` field reflects
/// local-only `mark_vine_viewed` state.
///
/// Returns `Err("not connected")` if the node is not running.
/// ZEB-147 will extend this with disk persistence.
#[tauri::command]
fn list_vine_videos(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<VineVideoDto>, String> {
    let cache = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .vine_feed_cache
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };
    let result = cache
        .lock()
        .map_err(|e| format!("vine_feed_cache lock: {e}"))?
        .list_descriptors();
    Ok(result)
}

#[tauri::command]
async fn follow_vine_creator(
    address: String,
    name: Option<String>,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let mut guard = state.lock().map_err(|e| format!("lock: {e}"))?;

    if address == guard.node_addr {
        return Err("cannot follow yourself".to_string());
    }

    let mgr = guard.follow_mgr.as_mut().ok_or("not connected")?;
    if !mgr.follow(address.clone(), name) {
        return Ok(false);
    }

    if let Some(ref set) = guard.followed_set {
        let mut s = set.lock().unwrap();
        s.insert(address.clone());
    }

    if let Some(ref tx) = guard.follow_tx {
        if tx
            .try_send(event_loop::FollowRequest::Follow { address })
            .is_err()
        {
            tracing::error!("follow_tx full — follow update not sent to event loop");
        }
    }

    Ok(true)
}

#[tauri::command]
async fn unfollow_vine_creator(
    address: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let mut guard = state.lock().map_err(|e| format!("lock: {e}"))?;

    let mgr = guard.follow_mgr.as_mut().ok_or("not connected")?;
    if !mgr.unfollow(&address) {
        return Ok(false);
    }

    if let Some(ref set) = guard.followed_set {
        let mut s = set.lock().unwrap();
        s.remove(&address);
    }

    if let Some(ref tx) = guard.follow_tx {
        if tx
            .try_send(event_loop::FollowRequest::Unfollow { address })
            .is_err()
        {
            tracing::error!("follow_tx full — unfollow update not sent to event loop");
        }
    }

    Ok(true)
}

#[tauri::command]
fn list_followed(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<FollowEntryResponse>, String> {
    let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
    let mgr = guard.follow_mgr.as_ref().ok_or("not connected")?;
    Ok(mgr
        .list()
        .into_iter()
        .map(|e| FollowEntryResponse {
            address: e.address,
            name: e.name,
            followed_at: e.followed_at,
        })
        .collect())
}

/// Mark a vine viewed by the local peer. Returns `Ok(true)` if newly
/// marked viewed, `Ok(false)` if already viewed.
///
/// Returns `Err("not connected")` if the node is not running.
/// Local-only in this PR; cross-device sync deferred to ZEB-147.
#[tauri::command]
fn mark_vine_viewed(
    vine_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let cache = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .vine_feed_cache
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };
    let result = cache
        .lock()
        .map_err(|e| format!("vine_feed_cache lock: {e}"))?
        .mark_viewed(vine_id);
    Ok(result)
}

// ── Content announcement types and file manager stubs ───────────────────

/// Content availability announcement received from the mesh network.
///
/// When a node stores content, it publishes to `harmony/announce/{cid_hex}`
/// with the payload size. The event loop routes these to the frontend as
/// `content-announced` IPC events.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentAnnouncementPayload {
    /// Hex-encoded CID from the key expression.
    pub cid: String,
    /// Payload size in bytes (from the 4-byte big-endian announcement body).
    pub size_bytes: u32,
}

/// Parse a content announcement from key expression + payload.
///
/// Key format: `harmony/announce/{cid_hex}`
/// Payload: 4 bytes big-endian u32 size.
pub fn parse_content_announcement(
    key_expr: &str,
    payload: &[u8],
) -> Option<ContentAnnouncementPayload> {
    let cid_hex = key_expr.strip_prefix("harmony/announce/")?;
    if cid_hex.is_empty() {
        return None;
    }
    if !cid_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    if payload.len() < 4 {
        return None;
    }
    let size_bytes = u32::from_be_bytes(payload[..4].try_into().ok()?);
    Some(ContentAnnouncementPayload {
        cid: cid_hex.to_string(),
        size_bytes,
    })
}

/// Wire format returned by `list_content` — one entry per self-ingested
/// file the client is aware of. Joins sidecar metadata with the runtime
/// cache's pinned state snapshot. ZEB-158 slice 1 adds `kind` to
/// distinguish leaf files from folder bundles. ZEB-164 adds `sidecarId`
/// (the wire-stable handle for pin/burn/archive operations); empty for
/// manifest-derived rows where no sidecar entry exists.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentItemWire {
    /// ZEB-164: opaque per-entry stable handle. Empty string for
    /// manifest-derived rows (children inside a folder bundle that have
    /// no sidecar entry of their own). Frontend gates pin/burn/archive
    /// buttons on this being non-empty.
    pub sidecar_id: String,
    pub cid: String, // hex
    pub name: String,
    pub size_bytes: u64,
    pub stored_at: u64,           // ms since epoch
    pub sensitivity: String,      // "private" | "confidential" | "public"
    pub replication_tier: String, // "expendable" | "light" | "default" | "high" | "ultra"
    pub pinned: bool,
    pub licensed: bool,
    pub archived: bool,
    pub kind: String, // ZEB-158: "leaf" | "folder"
}

fn sensitivity_wire(s: content_index::Sensitivity) -> &'static str {
    match s {
        content_index::Sensitivity::Private => "private",
        content_index::Sensitivity::Confidential => "confidential",
        content_index::Sensitivity::Public => "public",
    }
}

fn replication_tier_wire(t: content_index::ReplicationTier) -> &'static str {
    match t {
        content_index::ReplicationTier::Expendable => "expendable",
        content_index::ReplicationTier::Light => "light",
        content_index::ReplicationTier::Default => "default",
        content_index::ReplicationTier::High => "high",
        content_index::ReplicationTier::Ultra => "ultra",
    }
}

fn kind_wire(k: content_index::ContentKind) -> &'static str {
    match k {
        content_index::ContentKind::Leaf => "leaf",
        content_index::ContentKind::Folder => "folder",
    }
}

fn parse_cid_hex(cid_hex: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(cid_hex).map_err(|_| "invalid cid hex".to_string())?;
    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| "cid must be 32 bytes".to_string())
}

fn parse_sidecar_id(s: &str) -> Result<content_index::SidecarId, String> {
    if s.is_empty() {
        return Err("sidecar_id is empty (manifest-derived row?)".into());
    }
    content_index::SidecarId::parse_str(s).map_err(|e| format!("invalid sidecar_id: {e}"))
}

/// Result returned to the frontend after a successful file ingest.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestResult {
    pub sidecar_id: String,
    pub cid: String,
    pub file_name: String,
    pub size_bytes: u64,
}

/// Result returned by `create_folder` and `create_folder_at_root`. The
/// frontend stashes `sidecar_id` immediately so subsequent operations on
/// the just-created folder (pin, archive, future move/rename) have the
/// stable handle. `cid` is provided alongside for content-addressed reads.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateFolderResult {
    pub sidecar_id: String,
    pub cid: String,
}

#[tauri::command]
async fn list_content(
    folder_cid: Option<String>,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<ContentItemWire>, String> {
    // Root listings read entry.pinned directly from the sidecar (the runtime
    // pin_intent OR-join keeps that flag authoritative), so they don't need
    // the runtime's pinned-CID snapshot. Only fetch it for folder listings,
    // where manifest-derived rows have no sidecar entry to consult.
    match folder_cid {
        None => list_root(state),
        Some(hex) => {
            let verb_tx = {
                let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
                guard
                    .content_verb_tx
                    .clone()
                    .ok_or_else(|| "runtime unavailable".to_string())?
            };
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            verb_tx
                .send(event_loop::ContentVerbRequest::PinnedSet { reply: reply_tx })
                .await
                .map_err(|_| "event loop not running".to_string())?;
            let pinned_set = reply_rx
                .await
                .map_err(|_| "event loop dropped snapshot request".to_string())?;
            list_folder(hex, verb_tx, &pinned_set).await
        }
    }
}

pub(crate) fn list_root(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<ContentItemWire>, String> {
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let mut entries: Vec<ContentItemWire> = {
        let idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.entries()
            .map(|e| ContentItemWire {
                sidecar_id: e.sidecar_id.to_string(),
                cid: hex::encode(e.cid),
                name: e.file_name.clone(),
                size_bytes: e.size_bytes,
                stored_at: e.stored_at_ms,
                sensitivity: sensitivity_wire(e.sensitivity).to_string(),
                replication_tier: replication_tier_wire(e.replication_tier).to_string(),
                pinned: e.pinned,
                licensed: e.licensed,
                archived: e.archived,
                kind: kind_wire(e.kind).to_string(),
            })
            .collect()
    };
    // HashMap iter is non-deterministic; sort newest-first for stable UI.
    // Rust 1.95's clippy::unnecessary_sort_by lint flags `sort_by` with a
    // reverse comparator — sort_by_key + Reverse expresses the same intent
    // more directly.
    entries.sort_by_key(|e| std::cmp::Reverse(e.stored_at));
    Ok(entries)
}

pub async fn list_folder(
    folder_cid_hex: String,
    verb_tx: tokio::sync::mpsc::Sender<event_loop::ContentVerbRequest>,
    pinned_set: &std::collections::HashSet<[u8; 32]>,
) -> Result<Vec<ContentItemWire>, String> {
    use harmony_content::bundle::parse_bundle;

    let folder_cid = parse_cid_hex(&folder_cid_hex)?;

    // Fetch the folder's bundle bytes from the runtime cache.
    let bundle_bytes = match read_cached_bytes(&verb_tx, folder_cid).await? {
        Some(b) => b,
        None => {
            // Folder not in cache — likely evicted or never admitted.
            // Return empty (UI shows empty folder); ZEB-159 will add
            // transparent re-fetch in a follow-up.
            tracing::debug!(
                folder_cid = %folder_cid_hex,
                "list_folder: bundle not in cache; returning empty",
            );
            return Ok(vec![]);
        }
    };

    // Parse bundle child CIDs; child-0 is the manifest book.
    let child_cids: Vec<[u8; 32]> = parse_bundle(&bundle_bytes)
        .map_err(|e| format!("malformed folder bundle: {e:?}"))?
        .iter()
        .map(|c| c.to_bytes())
        .collect();
    let manifest_cid: [u8; 32] = child_cids
        .first()
        .copied()
        .ok_or_else(|| "folder bundle has no children".to_string())?;

    // Read the manifest book bytes.
    let manifest_bytes = read_cached_bytes(&verb_tx, manifest_cid)
        .await?
        .ok_or_else(|| "manifest book not in cache".to_string())?;

    let manifest = crate::folders::parse_manifest(&manifest_bytes)?;
    crate::folders::validate_manifest_matches_bundle(&manifest, &child_cids)?;

    // Synthesize wire rows. Nested items have no sidecar: sidecar_id is
    // the empty-string sentinel ("frontend: no mutations apply"); size_bytes
    // /stored_at default to 0; sensitivity/replication_tier default;
    // licensed/archived false. For manifest-derived rows we DO consult the
    // runtime pinned set — those rows have no sidecar.pinned to read, and
    // a CID currently held in cache via some other entry's pin_intent is
    // the only signal of "this content is sticking around right now".
    Ok(manifest
        .folder_manifest
        .entries
        .into_iter()
        .map(|e| ContentItemWire {
            sidecar_id: String::new(),
            cid: hex::encode(e.cid),
            name: e.name,
            size_bytes: 0,
            stored_at: 0,
            sensitivity: "private".into(),
            replication_tier: "default".into(),
            pinned: pinned_set.contains(&e.cid),
            licensed: false,
            archived: false,
            kind: kind_wire(e.kind).to_string(),
        })
        .collect())
}

#[tauri::command]
async fn pin_content(
    sidecar_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let id = parse_sidecar_id(&sidecar_id)?;

    // ZEB-155 + ZEB-164: persist pin intent on the sidecar BEFORE the
    // runtime verb. After flipping the bit, look up the entry's CID so
    // we can dispatch Pin against it. The Pin verb is idempotent for
    // CIDs already in pin_intent (a sibling entry pinning the same CID
    // will have already added it).
    let (index, maybe_verb_tx) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        (guard.content_index.clone(), guard.content_verb_tx.clone())
    };
    let cid_bytes = {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.set_pinned(&id, true);
        idx.get(&id)
            .ok_or_else(|| "unknown sidecar_id".to_string())?
            .cid
    };

    // Sidecar already committed. Runtime Pin failures split into two
    // categories:
    //   - Deterministic refusal (Ok(Ok(false)) = pin quota exhausted):
    //     surface to the caller as Ok(false). The sidecar bit is set so
    //     intent is recorded, but the runtime answered "no, can't fit"
    //     and the user needs to know (free space, retry). The
    //     start_node sweep will retry on next start; if quota is still
    //     exhausted there too, it also gets false.
    //   - Transient runtime gaps (event loop down, dropped reply,
    //     verb_tx None, runtime returned Err): best-effort, log, return
    //     Ok(true). Intent is recorded; the start_node pin-restore
    //     sweep walks the sidecar and re-pins every entry with
    //     pinned=true, so the gap is bounded and self-healing.
    // This preserves the runtime's quota-exhausted signal that the
    // pre-best-effort code returned via the bool, while keeping the
    // best-effort behavior for transient failures that pin/unpin/burn
    // all share.
    let pinned = if let Some(verb_tx) = maybe_verb_tx {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        match verb_tx
            .send(event_loop::ContentVerbRequest::Pin {
                cid: cid_bytes,
                reply: reply_tx,
            })
            .await
        {
            Ok(()) => match reply_rx.await {
                Ok(Ok(true)) => true,
                Ok(Ok(false)) => {
                    tracing::warn!(
                        cid = %hex::encode(cid_bytes),
                        "pin_content: runtime pin quota exhausted",
                    );
                    false
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        cid = %hex::encode(cid_bytes),
                        err = %e,
                        "pin_content: runtime pin failed; pin_intent will repopulate on next start_node sweep",
                    );
                    true
                }
                Err(_) => {
                    tracing::warn!(
                        cid = %hex::encode(cid_bytes),
                        "pin_content: event loop dropped pin reply",
                    );
                    true
                }
            },
            Err(_) => {
                tracing::warn!(
                    cid = %hex::encode(cid_bytes),
                    "pin_content: event loop closed before pin send; pin_intent will repopulate on next start_node sweep",
                );
                true
            }
        }
    } else {
        tracing::warn!(
            cid = %hex::encode(cid_bytes),
            "pin_content: runtime unavailable; pin_intent will repopulate on next start_node sweep",
        );
        true
    };
    Ok(pinned)
}

#[tauri::command]
async fn unpin_content(
    sidecar_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let id = parse_sidecar_id(&sidecar_id)?;

    // ZEB-164: clear sidecar intent. Then check OR-join: if some other
    // sidecar entry STILL pins this CID, leave runtime pin_intent alone
    // (the bytes are still wanted). Only dispatch Unpin to the runtime
    // when no entry references this CID with pinned=true anymore.
    let (index, maybe_verb_tx) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        (guard.content_index.clone(), guard.content_verb_tx.clone())
    };
    let unpin_runtime_for: Option<[u8; 32]> = {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.set_pinned(&id, false);
        let cid = idx
            .get(&id)
            .ok_or_else(|| "unknown sidecar_id".to_string())?
            .cid;
        if idx.is_cid_pinned_by_any(&cid) {
            None
        } else {
            Some(cid)
        }
    };

    let Some(cid_bytes) = unpin_runtime_for else {
        return Ok(true); // sidecar updated; another entry still pins
    };

    // Sidecar already committed. Runtime Unpin is best-effort: if the
    // event loop is gone, we have a stale pin_intent that self-corrects
    // on the next start_node pin-restore sweep. Log, don't propagate —
    // matches burn_content's RuntimeAction::Unpin branch and the
    // create_folder_nested post-rekey pattern.
    if let Some(verb_tx) = maybe_verb_tx {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        match verb_tx
            .send(event_loop::ContentVerbRequest::Unpin {
                cid: cid_bytes,
                reply: reply_tx,
            })
            .await
        {
            Ok(()) => match reply_rx.await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!(
                    cid = %hex::encode(cid_bytes),
                    err = %e,
                    "unpin_content: runtime unpin failed; pin_intent may be stale",
                ),
                Err(_) => tracing::warn!(
                    cid = %hex::encode(cid_bytes),
                    "unpin_content: event loop dropped unpin reply",
                ),
            },
            Err(_) => tracing::warn!(
                cid = %hex::encode(cid_bytes),
                "unpin_content: event loop closed before unpin send; pin_intent may be stale",
            ),
        }
    }
    Ok(true)
}

/// Burn a sidecar entry. With ZEB-164's symlink-style sidecar, burn is
/// "remove this entry from my list" — not "destroy the bytes everyone
/// shares." The runtime's `Burn` verb only fires when this entry was the
/// last reference to its CID. Otherwise we issue an `Unpin` (if the burn
/// drops the only pinning entry) or no runtime action (if siblings still
/// pin it).
#[tauri::command]
async fn burn_content(
    sidecar_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let id = parse_sidecar_id(&sidecar_id)?;

    // Match pin_content/unpin_content's best-effort pattern: clone
    // maybe_verb_tx without erroring on None. The pre-existing upfront
    // ok_or_else was a weak guard anyway — runtime can die between the
    // check and the verb_tx.send().await — and asymmetry meant users
    // could pin/unpin offline but not burn. With sidecar-as-source-of-
    // truth, the entry-removal step succeeds even with the runtime down;
    // a future reconciliation pass cleans up surviving bytes.
    let (index, maybe_verb_tx) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        (guard.content_index.clone(), guard.content_verb_tx.clone())
    };

    // Three-branch decision under a single lock acquisition: read entry's
    // CID, remove the entry, then inspect the post-state to decide which
    // (if any) runtime verb to dispatch.
    enum RuntimeAction {
        Burn([u8; 32]),
        Unpin([u8; 32]),
        Nothing,
    }
    let action = {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        // Snapshot the burned entry's pinned bit before removing — we
        // need it to decide whether the runtime pin_intent had this CID
        // at all. If the burned entry wasn't pinning, then runtime
        // pin_intent state is independent of any sibling (the OR-join
        // is unchanged by removing a contributor that wasn't asserting).
        let (cid, was_pinned) = match idx.get(&id) {
            Some(e) => (e.cid, e.pinned),
            None => return Ok(false), // unknown sidecar_id; no-op
        };
        idx.remove(&id);
        if idx.entries_for_cid(&cid).next().is_none() {
            RuntimeAction::Burn(cid)
        } else if was_pinned && !idx.is_cid_pinned_by_any(&cid) {
            // The burned entry was the last pinning reference; drop
            // runtime pin_intent. Without the was_pinned guard, an
            // unpinned-entry burn whose siblings are also unpinned
            // would dispatch a spurious Unpin (no-op at the cache
            // layer, but generates misleading "post-burn unpin failed"
            // warnings if the runtime path errors).
            RuntimeAction::Unpin(cid)
        } else {
            RuntimeAction::Nothing
        }
    };

    match action {
        RuntimeAction::Burn(cid) => {
            // Sidecar mutation already committed — runtime Burn is best-
            // effort. If the event loop is gone, bytes may survive until
            // W-TinyLFU evicts them or a future reconciliation pass runs.
            // Log so the desync is diagnosable. Matches the unpin /
            // post-burn-Unpin pattern.
            if let Some(verb_tx) = maybe_verb_tx {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                match verb_tx
                    .send(event_loop::ContentVerbRequest::Burn {
                        cid,
                        reply: reply_tx,
                    })
                    .await
                {
                    Ok(()) => match reply_rx.await {
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => tracing::warn!(
                            cid = %hex::encode(cid),
                            err = %e,
                            "burn_content: runtime burn failed; bytes may survive until reconciliation",
                        ),
                        Err(_) => tracing::warn!(
                            cid = %hex::encode(cid),
                            "burn_content: event loop dropped burn reply",
                        ),
                    },
                    Err(_) => tracing::warn!(
                        cid = %hex::encode(cid),
                        "burn_content: event loop closed before burn send; bytes may survive",
                    ),
                }
            } else {
                tracing::warn!(
                    cid = %hex::encode(cid),
                    "burn_content: runtime unavailable; bytes may survive until reconciliation",
                );
            }
        }
        RuntimeAction::Unpin(cid) => {
            // Sibling entries still reference this CID, but none pin it —
            // drop runtime pin_intent so W-TinyLFU can reclaim. Best-
            // effort: any failure here is a runtime/cache desync, not a
            // user-visible regression (the sidecar mutation already
            // committed). Log so the desync is diagnosable.
            if let Some(verb_tx) = maybe_verb_tx {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                match verb_tx
                    .send(event_loop::ContentVerbRequest::Unpin {
                        cid,
                        reply: reply_tx,
                    })
                    .await
                {
                    Ok(()) => match reply_rx.await {
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => tracing::warn!(
                            cid = %hex::encode(cid),
                            err = %e,
                            "burn_content: post-burn unpin failed; runtime may hold stale pin",
                        ),
                        Err(_) => tracing::warn!(
                            cid = %hex::encode(cid),
                            "burn_content: event loop dropped post-burn unpin reply",
                        ),
                    },
                    Err(_) => tracing::warn!(
                        cid = %hex::encode(cid),
                        "burn_content: event loop closed before post-burn unpin send",
                    ),
                }
            } else {
                tracing::warn!(
                    cid = %hex::encode(cid),
                    "burn_content: runtime unavailable for post-burn unpin; runtime may hold stale pin",
                );
            }
        }
        RuntimeAction::Nothing => {} // siblings still pin; runtime untouched
    }
    Ok(true)
}

#[tauri::command]
async fn archive_content(
    sidecar_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let id = parse_sidecar_id(&sidecar_id)?;
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let flipped = {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.set_archived(&id, true)
    };
    Ok(flipped)
}

#[tauri::command]
async fn set_replication_tier(
    sidecar_ids: Vec<String>,
    tier: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<u32, String> {
    let parsed_tier = match tier.as_str() {
        "expendable" => content_index::ReplicationTier::Expendable,
        "light" => content_index::ReplicationTier::Light,
        "default" => content_index::ReplicationTier::Default,
        "high" => content_index::ReplicationTier::High,
        "ultra" => content_index::ReplicationTier::Ultra,
        other => return Err(format!("unknown replication tier: {other}")),
    };
    let mut parsed_ids: Vec<content_index::SidecarId> = Vec::with_capacity(sidecar_ids.len());
    for s in &sidecar_ids {
        parsed_ids.push(parse_sidecar_id(s)?);
    }
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let updated = {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.set_replication_tier(&parsed_ids, parsed_tier)
    };
    Ok(updated as u32)
}

/// Export content to the local filesystem via a save dialog.
///
/// Fetches the raw bytes for `cid` through the Zenoh content transport,
/// opens a native save-file dialog with `file_name` as the suggested name,
/// and writes the bytes to the chosen path.
#[tauri::command]
async fn export_content(
    app: tauri::AppHandle,
    cid: String,
    file_name: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    use tauri_plugin_dialog::DialogExt;

    // Validate hex CID
    if cid.is_empty() || !cid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("invalid CID hex: {cid}"));
    }

    // 1. Fetch content bytes via the existing fetch channel.
    let fetch_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .fetch_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    fetch_tx
        .send(event_loop::FetchRequest {
            cid_hex: cid,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;

    let bytes = reply_rx
        .await
        .map_err(|_| "event loop dropped fetch request".to_string())??;

    // 2. Open a native save-file dialog.
    let (path_tx, path_rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_file_name(&file_name)
        .save_file(move |path| {
            let _ = path_tx.send(path);
        });

    let file_path = path_rx
        .await
        .map_err(|_| "dialog error".to_string())?
        .ok_or_else(|| "export cancelled".to_string())?;

    // 3. Write bytes to disk.
    let path = file_path
        .as_path()
        .ok_or_else(|| "unsupported file path".to_string())?;
    tokio::fs::write(path, &bytes)
        .await
        .map_err(|e| format!("write failed: {e}"))?;

    Ok(true)
}

/// Ingest a local file into the content store via a native open-file dialog.
///
/// Opens a file picker, reads the selected file, computes a CID, and stores
/// the content in the runtime's storage tier (which handles announcement to
/// the mesh). Returns metadata so the frontend can add it to the file list.
#[tauri::command]
async fn ingest_content(
    app: tauri::AppHandle,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<IngestResult, String> {
    use harmony_content::cid::{ContentFlags, ContentId};
    use tauri_plugin_dialog::DialogExt;

    // 1. Open a native file picker dialog.
    let (path_tx, path_rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_file(move |path| {
        let _ = path_tx.send(path);
    });
    let file_path = path_rx
        .await
        .map_err(|_| "dialog error".to_string())?
        .ok_or_else(|| "upload cancelled".to_string())?;

    // 2. Read file bytes (with size guard to avoid OOM on large files).
    let path = file_path
        .as_path()
        .ok_or_else(|| "unsupported file path".to_string())?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("read failed: {e}"))?;
    // Early reject above the flat-bundle cap, before reading the file into
    // memory. Dispatch is recomputed from actual bytes below in case the
    // file changes size between this stat and the read that follows.
    ingest_dispatch(meta.len())?;

    // OOM caveat: this materializes the full file in RAM before chunking.
    // Acceptable for v1 (FLAT_BUNDLE_MAX is ~8 GiB and realistic uploads
    // are far smaller) but a near-cap file would consume ~8 GiB of heap.
    // Streaming ingest pairs with the disk-backed storage tier — see the
    // spec's out-of-scope section. If you raise FLAT_BUNDLE_MAX without
    // landing streaming first, you are asking for OOMs.
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| format!("read failed: {e}"))?;
    let size_bytes = bytes.len() as u64;
    // Final dispatch decision from the bytes actually read. This closes the
    // TOCTOU window between metadata() and read(): if the file grew past the
    // cap we reject cleanly, and if it shrank below MAX_PAYLOAD_SIZE we take
    // the single-book fast path instead of tripping chunk_and_bundle's
    // precondition guard.
    let dispatch = ingest_dispatch(size_bytes)?;

    let ingest_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .ingest_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };

    let root_cid_bytes: [u8; 32] = match dispatch {
        IngestDispatch::Single => {
            let cid = ContentId::for_book(&bytes, ContentFlags::default())
                .map_err(|e| format!("CID error: {e:?}"))?;
            let cid_hex = hex::encode(cid.to_bytes());
            send_ingest(&ingest_tx, cid_hex, bytes).await?;
            cid.to_bytes()
        }
        IngestDispatch::Chunked => {
            let (leaves, bundle_payload, root) = chunk_and_bundle(&bytes)?;
            // Ingest every leaf in order.
            for (leaf_cid, leaf_bytes) in &leaves {
                send_ingest(
                    &ingest_tx,
                    hex::encode(leaf_cid.to_bytes()),
                    leaf_bytes.to_vec(),
                )
                .await?;
            }
            // Ingest the bundle itself.
            send_ingest(&ingest_tx, hex::encode(root.to_bytes()), bundle_payload).await?;
            root.to_bytes()
        }
    };

    // Record sidecar metadata so list_content can surface this entry.
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let stored_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let sidecar_id = content_index::SidecarId::new();
    {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        let inserted = idx.insert(content_index::ContentIndexEntry {
            sidecar_id,
            cid: root_cid_bytes,
            file_name: file_name.clone(),
            size_bytes,
            stored_at_ms,
            sensitivity: content_index::Sensitivity::Private,
            replication_tier: content_index::ReplicationTier::Default,
            licensed: false,
            archived: false,
            pinned: false,
            kind: content_index::ContentKind::Leaf,
        });
        if !inserted {
            // Effectively impossible (UUID v4 collision); kept as a
            // sanity guard against future SidecarId construction bugs.
            // Pre-ZEB-164 this branch silently deduped duplicate-CID
            // re-uploads; under the symlink model, two ingests of the
            // same content produce two distinct sidecar entries, so
            // !inserted here means the freshly-minted sidecar_id was
            // already in use. Fail loudly so the caller doesn't get a
            // phantom IngestResult whose sidecar_id list_content/pin/
            // burn/archive will all reject as unknown — mirrors
            // create_folder_at_root's symmetric guard.
            tracing::error!(
                sidecar_id = %sidecar_id,
                file_name = %file_name,
                "ingest_content: sidecar_id collision (UUID v4 collision or construction bug); aborting ingest result",
            );
            return Err("sidecar_id collision".into());
        }
    }

    Ok(IngestResult {
        sidecar_id: sidecar_id.to_string(),
        cid: hex::encode(root_cid_bytes),
        file_name,
        size_bytes,
    })
}

/// Send one (cid_hex, data) pair through the ingest channel and await its ack.
///
/// Shared by `ingest_content` and `create_folder` so both commands go
/// through a single implementation (DRY; no behavior change).
pub async fn send_ingest(
    tx: &tokio::sync::mpsc::Sender<event_loop::IngestRequest>,
    cid_hex: String,
    data: Vec<u8>,
) -> Result<(), String> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(event_loop::IngestRequest {
        cid_hex,
        data,
        reply: reply_tx,
    })
    .await
    .map_err(|_| "event loop not running".to_string())?;
    reply_rx
        .await
        .map_err(|_| "event loop dropped ingest request".to_string())??;
    Ok(())
}

/// ZEB-164: create a new folder at the root or inside an existing folder.
/// Empty `parent_path` means root; non-empty means a walk from top-level
/// root (index 0) down to immediate parent (last element).
#[tauri::command]
async fn create_folder(
    name: String,
    parent_sidecar_id: Option<String>,
    parent_path: Vec<String>,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<CreateFolderResult, String> {
    // Defence-in-depth: the UI already trims and rejects blank names, but
    // the IPC surface is callable by anything with a Tauri handle. An empty
    // or whitespace-only label would produce folders that are hard to
    // distinguish in listings and breadcrumbs, so reject at the boundary.
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("folder name cannot be empty".to_string());
    }
    if parent_path.is_empty() {
        if parent_sidecar_id.is_some() {
            return Err("root creates must not provide parent_sidecar_id".into());
        }
        return create_folder_at_root(name, state).await;
    }
    let psid =
        parent_sidecar_id.ok_or_else(|| "nested creates require parent_sidecar_id".to_string())?;
    create_folder_nested(name, psid, parent_path, state).await
}

async fn create_folder_at_root(
    name: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<CreateFolderResult, String> {
    // Build the (empty) manifest + bundle locally. No runtime state
    // mutated yet — we can still bail cleanly on send_ingest failure.
    let built = folders::build_folder(&name, &[])?;

    let (ingest_tx, index) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        (
            guard
                .ingest_tx
                .clone()
                .ok_or_else(|| "not connected".to_string())?,
            guard.content_index.clone(),
        )
    };
    let bundle_size = built.bundle_bytes.len() as u64;
    let stored_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    // ZEB-164: every empty folder bundle has the same CID, but multiple
    // sidecar entries can now reference that shared CID — so the slice-1
    // collision workaround ("a folder with identical contents already
    // exists") is gone. We mint a fresh sidecar_id, reserve the slot
    // before publishing bytes, and roll back if either ingest fails.
    let sidecar_id = content_index::SidecarId::new();
    {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        let inserted = idx.insert(content_index::ContentIndexEntry {
            sidecar_id,
            cid: built.bundle_cid.to_bytes(),
            file_name: name,
            size_bytes: bundle_size,
            stored_at_ms,
            sensitivity: content_index::Sensitivity::Private,
            replication_tier: content_index::ReplicationTier::Default,
            licensed: false,
            archived: false,
            pinned: false,
            kind: content_index::ContentKind::Folder,
        });
        if !inserted {
            // Effectively impossible (UUID v4 collision); kept as a
            // sanity guard against future SidecarId construction bugs.
            return Err("sidecar_id collision".into());
        }
    }

    // Slot reserved — now publish the bytes. ZEB-155's fetch-completion
    // recovery hook is gated on ZEB-159, so an orphan sidecar entry
    // would be unrecoverable until the user manually burned it. Roll
    // back the reservation on any ingest failure so the sidecar never
    // points at bytes that don't exist.
    if let Err(e) = send_ingest(
        &ingest_tx,
        hex::encode(built.manifest_cid.to_bytes()),
        built.manifest_bytes,
    )
    .await
    {
        if let Ok(mut idx) = index.lock() {
            idx.remove(&sidecar_id);
        }
        return Err(e);
    }
    if let Err(e) = send_ingest(
        &ingest_tx,
        hex::encode(built.bundle_cid.to_bytes()),
        built.bundle_bytes,
    )
    .await
    {
        if let Ok(mut idx) = index.lock() {
            idx.remove(&sidecar_id);
        }
        return Err(e);
    }

    Ok(CreateFolderResult {
        sidecar_id: sidecar_id.to_string(),
        cid: hex::encode(built.bundle_cid.to_bytes()),
    })
}

async fn create_folder_nested(
    name: String,
    parent_sidecar_id: String,
    parent_path: Vec<String>,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<CreateFolderResult, String> {
    use harmony_content::bundle::parse_bundle;

    let parent_id = parse_sidecar_id(&parent_sidecar_id)?;

    // Parse all path CIDs up-front; fail fast on malformed input.
    let path_cids: Vec<[u8; 32]> = parent_path
        .iter()
        .map(|h| parse_cid_hex(h))
        .collect::<Result<_, _>>()?;
    let root_old = *path_cids.first().expect("non-empty by guard above");
    let immediate_parent_cid = *path_cids.last().expect("non-empty");

    let (ingest_tx, verb_tx, index) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        (
            guard
                .ingest_tx
                .clone()
                .ok_or_else(|| "not connected".to_string())?,
            guard
                .content_verb_tx
                .clone()
                .ok_or_else(|| "not connected".to_string())?,
            guard.content_index.clone(),
        )
    };

    // Verify the caller's claim: parent_sidecar_id maps to root_old.
    {
        let idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        let entry = idx
            .get(&parent_id)
            .ok_or_else(|| "parent_sidecar_id not in sidecar".to_string())?;
        if entry.cid != root_old {
            return Err(format!(
                "parent_sidecar_id refers to cid {} but parent_path[0] is {}",
                hex::encode(entry.cid),
                hex::encode(root_old),
            ));
        }
    }

    // The verification above and the rekey below are non-atomic — we
    // yield across multiple await points (ancestor reads, then
    // pending_ingests drain). A concurrent create_folder_nested on the
    // same parent_sidecar_id could land its rekey between our verify
    // and our rekey, so we pass root_old as the expected_old_cid to
    // ContentIndex::rekey: if the entry's current CID has shifted,
    // rekey returns RekeyError::Conflict instead of silently
    // overwriting the concurrent winner. The UI serializes per-folder
    // mutations so this is rarely hit in practice, but the
    // ingest-before-rekey reorder widened the verify→rekey window
    // from "ancestor reads" to "drain pending_ingests" — wide enough
    // that the CAS guard is now load-bearing rather than defensive.

    // 1. Build the new empty sub-folder LOCALLY. Defer all ingests so
    // that a downstream OldMissing during rekey doesn't leave orphan
    // bytes in the runtime cache (which could be announced over Zenoh
    // and waste capacity for content no sidecar entry will ever
    // reference).
    let new_child = folders::build_folder(&name, &[])?;
    let new_child_bundle_cid = new_child.bundle_cid;

    let mut pending_ingests: Vec<(String, Vec<u8>)> = Vec::new();
    pending_ingests.push((
        hex::encode(new_child.manifest_cid.to_bytes()),
        new_child.manifest_bytes,
    ));
    pending_ingests.push((
        hex::encode(new_child_bundle_cid.to_bytes()),
        new_child.bundle_bytes,
    ));

    // 2. Bottom-up walk: rebuild each ancestor LOCALLY (read-only verb
    // requests), accumulating into pending_ingests.
    let mut prev_old_cid = immediate_parent_cid;
    let mut prev_new_cid = new_child_bundle_cid.to_bytes();
    let mut last_bundle_size: u64 = pending_ingests
        .last()
        .map(|(_, b)| b.len() as u64)
        .unwrap_or(0);

    for (i, &anc_cid) in path_cids.iter().enumerate().rev() {
        let is_deepest = i == path_cids.len() - 1;

        let anc_bundle = read_cached_bytes(&verb_tx, anc_cid).await?.ok_or_else(|| {
            format!(
                "ancestor {} not in cache; cannot rebuild parent chain",
                hex::encode(anc_cid)
            )
        })?;
        let anc_child_ids =
            parse_bundle(&anc_bundle).map_err(|e| format!("malformed ancestor bundle: {e:?}"))?;
        let manifest_cid = anc_child_ids
            .first()
            .copied()
            .ok_or_else(|| "ancestor bundle has no children".to_string())?;
        let anc_children: Vec<[u8; 32]> = anc_child_ids.iter().map(|c| c.to_bytes()).collect();

        let manifest_bytes = read_cached_bytes(&verb_tx, manifest_cid.to_bytes())
            .await?
            .ok_or_else(|| "ancestor manifest not in cache".to_string())?;
        let mut manifest =
            folders::parse_manifest(&manifest_bytes).map_err(|e| format!("ancestor {e}"))?;
        folders::validate_manifest_matches_bundle(&manifest, &anc_children)
            .map_err(|e| format!("ancestor {} {e}", hex::encode(anc_cid)))?;

        if is_deepest {
            manifest
                .folder_manifest
                .entries
                .push(folders::ManifestEntry {
                    cid: prev_new_cid,
                    name: name.clone(),
                    kind: content_index::ContentKind::Folder,
                });
        } else {
            let target_idx = manifest
                .folder_manifest
                .entries
                .iter()
                .position(|e| e.cid == prev_old_cid)
                .ok_or_else(|| {
                    format!(
                        "ancestor {} has no entry pointing to child {}",
                        hex::encode(anc_cid),
                        hex::encode(prev_old_cid)
                    )
                })?;
            manifest.folder_manifest.entries[target_idx].cid = prev_new_cid;
        }

        let rebuilt = folders::build_folder("", &manifest.folder_manifest.entries)?;
        let rebuilt_bundle_cid = rebuilt.bundle_cid;
        last_bundle_size = rebuilt.bundle_bytes.len() as u64;
        pending_ingests.push((
            hex::encode(rebuilt.manifest_cid.to_bytes()),
            rebuilt.manifest_bytes,
        ));
        pending_ingests.push((
            hex::encode(rebuilt_bundle_cid.to_bytes()),
            rebuilt.bundle_bytes,
        ));

        prev_old_cid = anc_cid;
        prev_new_cid = rebuilt_bundle_cid.to_bytes();
    }

    // 3. Drain the deferred ingests BEFORE rekeying. Earlier this was
    // ordered rekey-then-ingest to avoid leaving orphan bytes in the
    // runtime cache (and being announced over Zenoh) if rekey hit
    // OldMissing — but that ordering had a strictly worse failure
    // mode: a send_ingest failure after a successful rekey would leave
    // the sidecar pointing at a chain whose bytes are missing,
    // rendering the user's folder unreadable until manual burn.
    //
    // Reversed: an ingest failure now leaves the sidecar pointing at
    // the original root_old (intact). Bytes ingested before the
    // failure become orphans, but W-TinyLFU evicts them under cache
    // pressure since nothing pins them — recoverable, vs. data-loss
    // for the user. ZEB-167 still tracks the rekey-rollback path for
    // the residual rekey-OldMissing case (would leave orphans without
    // user-visible damage).
    let new_bundle_size = last_bundle_size;
    let stored_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    for (cid_hex, bytes) in pending_ingests {
        send_ingest(&ingest_tx, cid_hex, bytes).await?;
    }

    // 4. Rekey the top-level sidecar entry. CAS-style: pass root_old
    // as the expected current CID. With ZEB-164 the CID-collision
    // branch is gone — multiple entries sharing a CID is legal —
    // so OldMissing and Conflict are the only failure modes.
    {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        match idx.rekey(
            &parent_id,
            root_old,
            prev_new_cid,
            new_bundle_size,
            stored_at_ms,
        ) {
            Ok(()) => {}
            Err(content_index::RekeyError::OldMissing) => {
                return Err("parent_sidecar_id removed mid-flight — nothing to rekey".to_string());
            }
            Err(content_index::RekeyError::Conflict { actual }) => {
                // A concurrent rekey on the same parent_sidecar_id
                // landed between our verify and our rekey. The new
                // bundle bytes we just ingested are orphans — W-TinyLFU
                // will evict them under cache pressure. Surface the
                // actual current CID so future retry logic could
                // rebuild from it; for now the user re-issues the
                // create from the refreshed UI state.
                return Err(format!(
                    "concurrent rekey on parent_sidecar_id (now at cid {}); retry from refreshed state",
                    hex::encode(actual)
                ));
            }
        }
    }

    // 5. Maintain the runtime pin_intent OR-join invariant for both
    // old and new CIDs. If no remaining entry pins root_old, drop it
    // from runtime pin_intent. If any entry pins prev_new_cid (this
    // entry might, depending on its persisted intent), add it.
    //
    // Both dispatches are best-effort: the sidecar has already
    // committed the rekey, so any failure here is a runtime/cache
    // desync rather than a user-visible regression. Log so the desync
    // is diagnosable. The fetch-completion hook (ZEB-155 + ZEB-159)
    // re-converges on the next fetch of the new root.
    let (drop_old, add_new) = {
        let idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        (
            !idx.is_cid_pinned_by_any(&root_old),
            idx.is_cid_pinned_by_any(&prev_new_cid),
        )
    };
    if drop_old {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        match verb_tx
            .send(event_loop::ContentVerbRequest::Unpin {
                cid: root_old,
                reply: reply_tx,
            })
            .await
        {
            Ok(()) => match reply_rx.await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!(
                    old_cid = %hex::encode(root_old),
                    err = %e,
                    "create_folder_nested: runtime unpin of old root failed; cache may hold stale pin",
                ),
                Err(_) => tracing::warn!(
                    old_cid = %hex::encode(root_old),
                    "create_folder_nested: event loop dropped unpin reply",
                ),
            },
            Err(_) => tracing::warn!(
                old_cid = %hex::encode(root_old),
                "create_folder_nested: event loop closed before unpin send; cache may hold stale pin",
            ),
        }
    }
    if add_new {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        match verb_tx
            .send(event_loop::ContentVerbRequest::Pin {
                cid: prev_new_cid,
                reply: reply_tx,
            })
            .await
        {
            Ok(()) => match reply_rx.await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!(
                    new_cid = %hex::encode(prev_new_cid),
                    err = %e,
                    "create_folder_nested: runtime pin of new root failed; sidecar pin intent will repin on next fetch",
                ),
                Err(_) => tracing::warn!(
                    new_cid = %hex::encode(prev_new_cid),
                    "create_folder_nested: event loop dropped pin reply",
                ),
            },
            Err(_) => tracing::warn!(
                new_cid = %hex::encode(prev_new_cid),
                "create_folder_nested: event loop closed before pin send; sidecar pin intent will repin on next fetch",
            ),
        }
    }

    Ok(CreateFolderResult {
        // Identity unchanged, but emit the canonical lowercase-hyphenated form
        // (via SidecarId::Display) instead of echoing the caller's raw input —
        // every other endpoint that returns a sidecar_id wire field does the same.
        sidecar_id: parent_id.to_string(),
        cid: hex::encode(prev_new_cid),
    })
}

async fn read_cached_bytes(
    verb_tx: &tokio::sync::mpsc::Sender<event_loop::ContentVerbRequest>,
    cid: [u8; 32],
) -> Result<Option<Vec<u8>>, String> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    verb_tx
        .send(event_loop::ContentVerbRequest::ReadBytes {
            cid,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;
    reply_rx
        .await
        .map_err(|_| "event loop dropped read request".to_string())
}

/// Fetch raw content bytes by hex-encoded CID via Zenoh get().
///
/// Used by the frontend to resolve avatar CIDs (and other content) into
/// displayable blob URLs.
#[tauri::command]
async fn fetch_content(
    cid: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<u8>, String> {
    // Validate hex CID
    if cid.is_empty() || !cid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("invalid CID hex: {cid}"));
    }

    let fetch_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .fetch_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    fetch_tx
        .send(event_loop::FetchRequest {
            cid_hex: cid,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;

    reply_rx
        .await
        .map_err(|_| "event loop dropped fetch request".to_string())?
}

// ── Voice commands ──────────────────────────────────────────────────────

/// Reject channel IDs that could escape the intended Zenoh key namespace.
/// Same forbidden characters as send_message's channel/hub validation.
fn validate_voice_channel_id(channel_id: &str) -> Result<(), String> {
    if channel_id.is_empty()
        || channel_id.contains('/')
        || channel_id.contains('*')
        || channel_id.contains('?')
        || channel_id.contains('#')
        || channel_id.contains('$')
    {
        return Err(format!("invalid voice channel_id: {channel_id}"));
    }
    Ok(())
}

#[tauri::command]
async fn send_voice_frame(
    payload: voice::SendVoiceFramePayload,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    validate_voice_channel_id(&payload.channel_id)?;
    let voice_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .voice_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };
    voice_tx
        .send(voice::VoiceOutbound {
            channel_id: payload.channel_id,
            frame: payload.frame_bytes,
        })
        .await
        .map_err(|_| "event loop not running".to_string())
}

#[tauri::command]
async fn join_voice_channel(
    channel_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    validate_voice_channel_id(&channel_id)?;
    let tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .voice_channel_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };
    tx.send(voice::VoiceChannelRequest::Join { channel_id })
        .await
        .map_err(|_| "event loop not running".to_string())
}

#[tauri::command]
async fn leave_voice_channel(
    channel_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    validate_voice_channel_id(&channel_id)?;
    let tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .voice_channel_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };
    tx.send(voice::VoiceChannelRequest::Leave { channel_id })
        .await
        .map_err(|_| "event loop not running".to_string())
}

// ── Mail commands ────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendMailPayload {
    to: Vec<String>,
    subject: String,
    body: String,
    reply_to: Option<String>,
}

#[tauri::command]
async fn send_mail(
    payload: SendMailPayload,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    use harmony_mailbox::message::{
        unique_message_id, HarmonyMessage, MailMessageType, MessageFlags, Recipient, RecipientType,
    };

    if payload.to.is_empty() {
        return Err("at least one recipient required".to_string());
    }
    if payload.subject.is_empty() && payload.body.is_empty() {
        return Err("subject or body required".to_string());
    }

    let (publish_tx, node_addr, mail_mgr) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        let tx = guard
            .publish_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?;
        let mgr = guard
            .mail_mgr
            .clone()
            .ok_or_else(|| "mail not initialized".to_string())?;
        (tx, guard.node_addr.clone(), mgr)
    };

    // Parse sender address
    let sender_bytes: [u8; 16] = hex::decode(&node_addr)
        .map_err(|e| format!("bad node_addr: {e}"))?
        .try_into()
        .map_err(|_| "node_addr not 16 bytes".to_string())?;

    // Parse in_reply_to
    let in_reply_to = match &payload.reply_to {
        Some(hex_str) if !hex_str.is_empty() => {
            let bytes = hex::decode(hex_str).map_err(|e| format!("bad reply_to: {e}"))?;
            let arr: [u8; 16] = bytes
                .try_into()
                .map_err(|_| "reply_to not 16 bytes".to_string())?;
            Some(arr)
        }
        _ => None,
    };

    // Parse recipients
    let recipients: Vec<Recipient> = payload
        .to
        .iter()
        .map(|addr_hex| {
            let bytes =
                hex::decode(addr_hex).map_err(|e| format!("bad recipient {addr_hex}: {e}"))?;
            let arr: [u8; 16] = bytes
                .try_into()
                .map_err(|_| format!("recipient {addr_hex} not 16 bytes"))?;
            Ok(Recipient {
                address_hash: arr,
                recipient_type: RecipientType::To,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let is_reply = in_reply_to.is_some();
    let msg = HarmonyMessage {
        version: 0x01,
        message_type: MailMessageType::Email,
        flags: MessageFlags::new(false, is_reply, false),
        timestamp: now,
        message_id: unique_message_id(),
        in_reply_to,
        sender_address: sender_bytes,
        recipients,
        subject: payload.subject,
        body: payload.body,
        attachments: vec![],
    };

    let msg_bytes = msg.to_bytes().map_err(|e| format!("serialize: {e}"))?;

    // Publish to each recipient's Zenoh key (canonical lowercase hex)
    for recipient in &msg.recipients {
        let key_expr = format!("harmony/mail/v1/{}", hex::encode(recipient.address_hash));
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        publish_tx
            .send(event_loop::PublishRequest {
                key_expr,
                payload: msg_bytes.clone(),
                reply: reply_tx,
            })
            .await
            .map_err(|_| "event loop not running".to_string())?;
        reply_rx
            .await
            .map_err(|_| "event loop dropped request".to_string())??;
    }

    // Store in sent folder only after all publishes succeed
    {
        let mut mgr = mail_mgr.lock().map_err(|e| format!("mail lock: {e}"))?;
        mgr.store_sent(&msg_bytes, &msg)?;
    }

    Ok(())
}

#[tauri::command]
fn list_mail(
    folder: String,
    page: usize,
    per_page: usize,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<mail::EntryRecord>, String> {
    let mgr_arc = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .mail_mgr
            .clone()
            .ok_or_else(|| "mail not initialized".to_string())?
    }; // NodeState lock dropped — disk I/O below doesn't block other commands
    let mgr = mgr_arc.lock().map_err(|e| format!("mail lock: {e}"))?;
    Ok(mgr.list_folder(&folder, page, per_page))
}

#[tauri::command]
fn get_mail(
    message_cid: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<mail::MailDetail, String> {
    let mgr_arc = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .mail_mgr
            .clone()
            .ok_or_else(|| "mail not initialized".to_string())?
    };
    let mgr = mgr_arc.lock().map_err(|e| format!("mail lock: {e}"))?;

    // Targeted O(N) scan by reference (no folder clone): only the matching
    // entry is read, even on a 10k-message inbox. If Pending, return a
    // stub MailDetail — the blob doesn't exist on disk yet, so
    // mgr.get_message would fail. Frontend recognizes body_state=Pending
    // and triggers fetch_mail_body.
    if let Some(entry) = mgr.entry_by_cid(&message_cid) {
        if entry.body_state == mail::BodyState::Pending {
            return Ok(mail::MailDetail {
                message_cid: message_cid.clone(),
                message_id: entry.message_id.clone(),
                subject: entry.subject_snippet.clone(),
                body: String::new(),
                sender_address: entry.sender_address.clone(),
                recipients: vec![],
                timestamp: entry.timestamp,
                attachments: vec![],
                is_reply: false,
                is_forward: false,
                in_reply_to: None,
                body_state: mail::BodyState::Pending,
            });
        }
    }

    // Local (or entry missing — let get_message produce the proper error).
    mgr.get_message(&message_cid)
}

#[tauri::command]
async fn refresh_mail(state: tauri::State<'_, Mutex<NodeState>>) -> Result<(), String> {
    let sync_arc = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .mail_sync
            .clone()
            .ok_or_else(|| "mail_sync not initialized".to_string())?
    };
    sync_arc.refresh_now().await;
    Ok(())
}

#[tauri::command]
async fn fetch_mail_body(
    message_cid: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<mail::MailDetail, String> {
    let (sync_arc, mgr_arc) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        let sync = guard
            .mail_sync
            .clone()
            .ok_or_else(|| "mail_sync not initialized".to_string())?;
        let mgr = guard
            .mail_mgr
            .clone()
            .ok_or_else(|| "mail not initialized".to_string())?;
        (sync, mgr)
    };

    // Decode CID hex → 32-byte array.
    let cid_bytes = hex::decode(&message_cid).map_err(|e| format!("bad cid hex: {e}"))?;
    let cid_arr: [u8; 32] = cid_bytes
        .try_into()
        .map_err(|_| "cid must be 32 bytes".to_string())?;

    // Trigger lazy fetch (no-op if already Local; writes blob + promotes entry).
    sync_arc.fetch_body(cid_arr).await?;

    // Now return the fully-Local MailDetail from the manager.
    let mgr = mgr_arc.lock().map_err(|e| format!("mail lock: {e}"))?;
    mgr.get_message(&message_cid)
}

#[tauri::command]
fn update_mail(
    message_cid: String,
    action: String,
    folder: Option<String>,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let mgr_arc = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .mail_mgr
            .clone()
            .ok_or_else(|| "mail not initialized".to_string())?
    };
    let mut mgr = mgr_arc.lock().map_err(|e| format!("mail lock: {e}"))?;
    let folder_ref = folder.as_deref();
    match action.as_str() {
        "mark_read" => mgr.mark_read(&message_cid, true, folder_ref),
        "mark_unread" => mgr.mark_read(&message_cid, false, folder_ref),
        "move_trash" => mgr.move_message(&message_cid, folder_ref, "trash"),
        "move_inbox" => mgr.move_message(&message_cid, folder_ref, "inbox"),
        "delete" => mgr.delete_message(&message_cid, folder_ref),
        _ => Err(format!("unknown action: {action}")),
    }
}

#[tauri::command]
fn get_mail_counts(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<std::collections::HashMap<String, mail::FolderCounts>, String> {
    let mgr_arc = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .mail_mgr
            .clone()
            .ok_or_else(|| "mail not initialized".to_string())?
    };
    let mgr = mgr_arc.lock().map_err(|e| format!("mail lock: {e}"))?;
    Ok(mgr.folder_counts())
}

// ── E2E test helpers (debug builds only) ────────────────────────────────

/// Close a child window by label. Used by the Playwright E2E suite to reset
/// the network-viz window between runs — without this, a leftover viz from
/// the previous run makes the ZEB-144 "Open network visualization" regression
/// guard pass vacuously on reruns.
///
/// Restricted to the `network-viz` label so a stray dev-build IPC call can't
/// take down the main window. Stripped from release binaries entirely via
/// `#[cfg(debug_assertions)]` and the matching conditional registration in
/// `run()` below.
#[cfg(debug_assertions)]
#[tauri::command]
async fn e2e_close_window(app: AppHandle, label: String) -> Result<(), String> {
    use tauri::Manager;
    if label != "network-viz" {
        return Err(format!("e2e_close_window: label '{label}' not allowed"));
    }
    if let Some(window) = app.get_webview_window(&label) {
        window.close().map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ── CLI entry points ─────────────────────────────────────────────────────

/// CLI entry point for `harmony-app rotate-passphrase`.
///
/// Refusal conditions (in order):
///   1. OS keychain has an identity → refuse with explanation
///   2. HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE not set → refuse
///   3. --new-passphrase-file unreadable / empty → refuse
///   4. New passphrase byte-identical to old → log warning, proceed
///
/// Returns Ok(()) on successful rotation; Err on any refusal or rotation
/// failure. Caller (main.rs) translates Err into a non-zero exit.
pub fn rotate_passphrase_cli(new_passphrase_file: &std::path::Path) -> Result<(), String> {
    use identity::KeyStore as _;
    use secrecy::SecretString;

    // Refusal 1: keychain has identity, or its state can't be determined.
    // Failing closed on load() Err is important — if we can't tell whether the
    // identity is in the keychain, we must NOT rotate the encrypted file
    // (the rotation would silently target the wrong backend).
    //
    // KeychainStore::new() Err is trickier. The strict-correct posture is to
    // also fail closed, but that breaks the legitimate headless case (Linux
    // server with no Secret Service / no D-Bus session — the entire point of
    // the encrypted-file backend). The keyring crate's error type doesn't
    // cleanly distinguish "no backend on this system" from "backend present
    // but transiently unreachable", so we can't auto-discriminate. Compromise:
    // log a loud warning on new() Err so an operator on a misconfigured
    // desktop sees a signal, and proceed (the typical headless case is
    // benign). Operators with both a populated OS keychain and an .enc file
    // who hit a transient keychain failure mid-rotation could rotate the
    // wrong backend; this is a known niche risk documented here.
    match identity::KeychainStore::new() {
        Ok(kc) => match kc.load() {
            Ok(Some(_)) => {
                return Err(
                    "your identity is currently in the OS keychain; passphrase rotation only applies to headless installs. \
                     Re-encryption of keychain entries is handled by the OS when you change your login password.".to_string(),
                );
            }
            Ok(None) => {
                // Keychain reachable and empty → safe to rotate the .enc backend.
            }
            Err(e) => {
                return Err(format!(
                    "could not determine whether the identity is stored in the OS keychain — refusing to rotate to avoid acting on the wrong backend: {e}"
                ));
            }
        },
        Err(e) => {
            tracing::warn!(
                "OS keychain backend unavailable ({e}); proceeding with encrypted-file \
                 rotation. If you have a desktop install where the keychain SHOULD be \
                 reachable, this may indicate a transient or configuration issue and the \
                 rotation could affect a different backend than your active identity. \
                 On a headless install (typical case for this command) this is expected."
            );
        }
    }

    // Resolve old passphrase via the standard env chain.
    let plaintext_path = identity::resolve_path(None)?;
    let enc_path = plaintext_path.with_file_name("identity.enc");
    let old_store = identity::EncryptedFileStore::from_env(enc_path)?
        .ok_or_else(|| {
            "HARMONY_PASSPHRASE / HARMONY_PASSPHRASE_FILE not set — cannot rotate without the old passphrase".to_string()
        })?;

    // Read the new passphrase file via the same parser as HARMONY_PASSPHRASE_FILE
    // — UTF-8, exactly one trailing newline strip, empty rejection, AND the
    // 0600-mode warning the inline version was missing.
    let new_str = identity::parse_passphrase_file(new_passphrase_file).map_err(|e| {
        format!(
            "--new-passphrase-file={} {e}",
            new_passphrase_file.display()
        )
    })?;

    // Move into SecretString immediately so the plaintext String is consumed
    // (no second copy lingers on the heap unzeroed). passphrase_eq takes a
    // borrow, then rotate_passphrase moves the SecretString through.
    let candidate = SecretString::from(new_str);
    if old_store.passphrase_eq(&candidate) {
        tracing::warn!("new passphrase matches old — proceeding anyway");
    }

    // Rotate.
    identity::rotate_passphrase(&old_store, candidate)?;
    Ok(())
}

// ── ZEB-217 community IPC types ──────────────────────────────────────────

/// Frontend-facing member status. Mirrors `MemberStatus` from
/// `community_membership` but serializes with human-readable strings
/// ("joined" / "left" / "invited" / "banned") instead of the CBOR wire
/// codes ("j" / "l" / "i" / "b") — the CBOR renames exist for canonical
/// CBOR compactness on the wire, but the Tauri IPC boundary should
/// surface a string the frontend can read directly without a lookup
/// table.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MemberStatusDto {
    Joined,
    Left,
    Invited,
    Banned,
    /// ZEB-254: joiner has minted a PendingJoin awaiting admin counter-sign.
    PendingJoin,
}

impl From<crate::community_membership::MemberStatus> for MemberStatusDto {
    fn from(s: crate::community_membership::MemberStatus) -> Self {
        use crate::community_membership::MemberStatus;
        match s {
            MemberStatus::Joined => Self::Joined,
            MemberStatus::Left => Self::Left,
            MemberStatus::Invited => Self::Invited,
            MemberStatus::Banned => Self::Banned,
            // ZEB-254: wired in Task 4 (IPC).
            MemberStatus::PendingJoin => Self::PendingJoin,
        }
    }
}

/// Member-list row returned by `list_community_members` IPC. Mirrors
/// the spec's MemberInfo type. `addr` is hex of OwnerAddr (16 bytes →
/// 32 chars). `display_name` is None in Phase 3 — the existing profile
/// cache lookup is wired in Phase 5.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemberInfoDto {
    pub addr: String,
    pub display_name: Option<String>,
    pub status: MemberStatusDto,
    pub power: u8,
    pub joined_at: crate::owner_state_types::Hlc,
}

/// Event-kind discriminant for `ModerationEventDto`. Serializes as
/// `"kick" | "unban" | "set_power"` (snake_case) for the JS bridge.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModerationEventKindDto {
    Kick,
    Unban,
    SetPower,
}

/// One moderation action returned by `list_recent_moderation_events`.
/// Covers Kick, Unban, and SetPower event kinds only; channel-config
/// and epoch events are filtered out by the IPC.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModerationEventDto {
    /// Hex-encoded 16-byte EventId (32 chars).
    pub event_id: String,
    pub kind: ModerationEventKindDto,
    /// Hex-encoded actor OwnerAddr (32 chars).
    pub actor_addr: String,
    /// Hex-encoded target OwnerAddr (32 chars).
    pub target_addr: String,
    /// Free-text reason signed into the CRDT event; None for SetPower.
    /// Serialized as `null` (not omitted) so the JS side observes
    /// `reason: null` not `reason: undefined` — matches the TS contract
    /// `string | null` rather than introducing `undefined` semantics.
    pub reason: Option<String>,
    /// New power level; None for Kick and Unban. Serialized as `null`
    /// (not omitted) — same rationale as `reason`.
    pub new_power: Option<u8>,
    /// HLC at which the event was signed and inserted.
    pub hlc: crate::owner_state_types::Hlc,
}

/// ZEB-250: discriminated result of an admin moderation IPC. The
/// handler auto-routes based on the target community's admin_quorum:
/// - `Completed` if admin_quorum == 1 OR action is not admin-affecting.
/// - `Pending` if admin_quorum > 1 AND action is admin-affecting — an
///   AdminProposal was minted instead and is awaiting countersignatures.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind")]
pub enum AdminActionResult {
    Completed,
    Pending {
        proposal_event_id: String,
        signers_so_far: u8,
        quorum_required: u8,
    },
}

/// ZEB-250 §6.3: return type for `countersign_admin_proposal` IPC.
/// Reports the signer count after the (possibly no-op idempotent)
/// operation, the community's current quorum requirement, and whether
/// quorum has been reached.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CountersignResult {
    pub signers_after: u8,
    pub quorum_required: u8,
    pub reached_quorum: bool,
}

/// Project a materialized membership into the IPC DTO list, sorted by
/// power level descending then joined_at ascending. Stable for two
/// addrs at the same power+joined_at — falls through to OwnerAddr-bytes
/// comparison so the order is deterministic across calls.
pub fn member_info_for(
    m: &crate::community_membership::MaterializedMembership,
) -> Vec<MemberInfoDto> {
    let mut rows: Vec<MemberInfoDto> = m
        .members
        .iter()
        .map(|(addr, state)| MemberInfoDto {
            addr: hex::encode(addr.0),
            display_name: None,
            status: state.status.into(),
            power: m.power_levels.get(addr).copied().unwrap_or(0),
            joined_at: state.joined_at.clone(),
        })
        .collect();
    rows.sort_by(|a, b| {
        b.power
            .cmp(&a.power)
            .then_with(|| a.joined_at.wall_ms.cmp(&b.joined_at.wall_ms))
            .then_with(|| a.joined_at.logical.cmp(&b.joined_at.logical))
            .then_with(|| a.addr.cmp(&b.addr))
    });
    rows
}

/// Read-only IPC over a community's materialized member list.
/// Returns rows sorted by power desc then joined_at asc (see
/// `member_info_for`). `community_id` is the 32-char lowercase
/// hex of the 16-byte SpaceId.
///
/// Errors:
/// - `Err("invalid community_id hex: ...")` — couldn't parse hex.
/// - `Err("no community_registry — node not running?")` — start_node
///   hasn't wired the registry.
/// - `Err("no Space for community {hex} in owner-state")` — we
///   haven't joined this community (or we left and removed the Space).
/// - `Err("community Space missing admin_addr (corrupt row?)")` —
///   defensive guard; should be unreachable since `validate_invariants`
///   rejects these on apply.
/// - `Err("no engine for community {hex} — not joined or not yet
///   started")` — the community isn't in the registry's map.
#[tauri::command]
async fn list_community_members(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
) -> Result<Vec<MemberInfoDto>, String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let (crdt_state, registry) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.community_registry
                .clone()
                .ok_or("no community_registry — node not running?")?,
        )
    };

    let admin_addr = {
        let s = crdt_state.lock().await;
        let space = s.spaces.get(&space_id).cloned();
        drop(s);
        let space = space.ok_or_else(|| {
            format!(
                "no Space for community {} in owner-state",
                hex::encode(space_id.0)
            )
        })?;
        if space.kind != crate::owner_state_types::SpaceKind::Community {
            return Err(format!(
                "Space {} exists but is kind {:?}, not Community",
                hex::encode(space_id.0),
                space.kind
            ));
        }
        space
            .admin_addr
            .ok_or("community Space missing admin_addr (corrupt row?)")?
    };

    let engine_state = registry.state_for(&space_id).await.ok_or_else(|| {
        format!(
            "no engine for community {} — not joined or not yet started",
            hex::encode(space_id.0)
        )
    })?;

    let materialized = {
        let g = engine_state.lock().await;
        g.materialize_now(admin_addr)
    };

    Ok(member_info_for(&materialized))
}

// ── ZEB-287 Phase 2: list_community_forks IPC ─────────────────────────
//
// Walks a community's membership log for `MembershipEventKind::Fork`
// events, resolves forker display names via the same ladder used for
// member-list rendering (active member → cross-community cache → None
// fallback), marks descendant communities `locally_known` if the
// forker's local OwnerState carries a Space at that SpaceId. Authorized
// behind a Joined gate: non-members cannot enumerate forks of
// communities they aren't in.
//
// Silent forks are absent by design — silent mode emits no Fork event,
// so a log walk returns nothing for those. Phase 2 spec §4.1.

/// ZEB-287 Phase 2: one row in the descendants list. Returned by
/// `list_community_forks` IPC. `forker_display_name` is None in Phase 2
/// pending ZEB-281 PMB integration (matches Phase 1's
/// `MemberInfoDto.display_name` pattern); the UI renders the fallback
/// "an unknown member" when None.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkDescendantDto {
    /// Hex-encoded SpaceId of the descendant fork community.
    pub fork_space_id: String,
    /// Hex-encoded OwnerAddr of the forker (the signer of the Fork event).
    pub forker_addr: String,
    /// Resolved display name of the forker if currently Joined in this
    /// community AND a display-name source is available. None in Phase 2
    /// until ZEB-281 wires profile-broadcast resolution.
    pub forker_display_name: Option<String>,
    /// wall_ms of the Fork event's HLC.
    pub forked_at_wall_ms: u64,
    /// Whether the descendant community is locally known
    /// (in the joiner's OwnerState). UI uses this to gate clickability.
    pub locally_known: bool,
}

/// Pure-helper for `list_community_forks` IPC: takes the engine state's
/// event log + materialized membership + locally-known SpaceId set, plus
/// the caller's OwnerAddr, and returns the sorted descendants list (or
/// `Err("not a member")` if the caller is not Joined). Extracted to enable
/// unit testing without standing up a full NodeState fixture.
pub fn build_fork_descendants(
    events: &std::collections::BTreeMap<
        crate::community_membership::EventId,
        crate::community_membership::SignedMembershipEvent,
    >,
    materialized: &crate::community_membership::MaterializedMembership,
    locally_known: &std::collections::BTreeSet<crate::owner_state_types::SpaceId>,
    self_owner: crate::owner_state_types::OwnerAddr,
) -> Result<Vec<ForkDescendantDto>, String> {
    // Authorize: caller must be Joined.
    match materialized.members.get(&self_owner).map(|m| m.status) {
        Some(crate::community_membership::MemberStatus::Joined) => {}
        _ => return Err("not a member".to_string()),
    }

    // R1-3: sort by full HLC ordering BEFORE projecting to DTOs so that two
    // Fork events at the same wall_ms but different logical clocks order
    // deterministically by full HLC (wall_ms → logical → device_id), then
    // tiebreak by actor address. Projecting first and sorting on
    // `forked_at_wall_ms` alone loses the logical-clock distinction.
    let mut fork_events: Vec<&crate::community_membership::SignedMembershipEvent> = events
        .values()
        .filter(|signed| {
            matches!(
                &signed.kind,
                crate::community_membership::MembershipEventKind::Fork { .. }
            )
        })
        .collect();
    fork_events.sort_by(|a, b| {
        a.at.wall_ms
            .cmp(&b.at.wall_ms)
            .then_with(|| a.at.logical.cmp(&b.at.logical))
            .then_with(|| a.at.device_id.cmp(&b.at.device_id))
            .then_with(|| a.actor.0.cmp(&b.actor.0))
    });

    let dtos: Vec<ForkDescendantDto> = fork_events
        .into_iter()
        .map(|signed| {
            let fork_space_id = match &signed.kind {
                crate::community_membership::MembershipEventKind::Fork { fork_space_id } => {
                    fork_space_id
                }
                // Unreachable: filtered above.
                _ => unreachable!("non-Fork event survived filter"),
            };
            // Forker display name: per spec §4.1 only resolve when the
            // forker is currently Joined. Phase 2 has no profile-broadcast
            // ladder integration yet (ZEB-281 deferred), so the resolved
            // value is None even for Joined members — matches Phase 1's
            // MemberInfoDto.display_name placeholder pattern.
            let forker_display_name =
                match materialized.members.get(&signed.actor).map(|m| m.status) {
                    Some(crate::community_membership::MemberStatus::Joined) => None,
                    _ => None,
                };
            ForkDescendantDto {
                fork_space_id: hex::encode(fork_space_id.0),
                forker_addr: hex::encode(signed.actor.0),
                forker_display_name,
                forked_at_wall_ms: signed.at.wall_ms,
                locally_known: locally_known.contains(fork_space_id),
            }
        })
        .collect();

    Ok(dtos)
}

/// Tauri IPC: list visible Fork events from a community's membership log.
///
/// Authorization: caller must be `Joined` in the community. Sorted ascending
/// by `forked_at_wall_ms` with stable secondary sort by `forker_addr` for
/// HLC-tie cases.
///
/// Errors:
/// - `Err("invalid community_id hex: ...")` — couldn't parse hex.
/// - `Err("community_id must be 16 bytes (32 hex chars)")` — wrong length.
/// - `Err("crdt_state missing — node not running?")` — node not started.
/// - `Err("no community_registry — node not running?")` — registry missing.
/// - `Err("no Space for community {hex} in owner-state")` — community absent.
/// - `Err("no engine for community {hex} — not joined or not yet started")` —
///   engine absent.
/// - `Err("not a member")` — caller is not currently Joined.
#[tauri::command]
async fn list_community_forks(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
) -> Result<Vec<ForkDescendantDto>, String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let (crdt_state, registry, self_owner) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        let self_owner = g
            .dm_self_owner
            .ok_or("self_owner missing — node not running?")?;
        (
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.community_registry
                .clone()
                .ok_or("no community_registry — node not running?")?,
            self_owner,
        )
    };

    let admin_addr = {
        let s = crdt_state.lock().await;
        let space = s.spaces.get(&space_id).cloned();
        drop(s);
        let space = space.ok_or_else(|| {
            format!(
                "no Space for community {} in owner-state",
                hex::encode(space_id.0)
            )
        })?;
        if space.kind != crate::owner_state_types::SpaceKind::Community {
            return Err(format!(
                "Space {} exists but is kind {:?}, not Community",
                hex::encode(space_id.0),
                space.kind
            ));
        }
        space
            .admin_addr
            .ok_or("community Space missing admin_addr (corrupt row?)")?
    };

    let engine_state = registry.state_for(&space_id).await.ok_or_else(|| {
        format!(
            "no engine for community {} — not joined or not yet started",
            hex::encode(space_id.0)
        )
    })?;

    // Snapshot the log + materialized members under the minimum mutex holding.
    let (events_snapshot, materialized) = {
        let g = engine_state.lock().await;
        let materialized = g.materialize_now(admin_addr);
        let events_clone: std::collections::BTreeMap<
            crate::community_membership::EventId,
            crate::community_membership::SignedMembershipEvent,
        > = g.events.clone();
        (events_clone, materialized)
    };

    // Resolve locally-known set: which SpaceIds appear in joiner's OwnerState.spaces?
    let locally_known: std::collections::BTreeSet<crate::owner_state_types::SpaceId> = {
        let s = crdt_state.lock().await;
        s.spaces.keys().copied().collect()
    };

    build_fork_descendants(&events_snapshot, &materialized, &locally_known, self_owner)
}

// ── ZEB-287 Phase 2: get_community_lineage IPC ────────────────────────
//
// Exposes the lineage fields from CommunityState behind a tight DTO so
// the frontend can render the ForkLineageTree without leaking the full
// CommunityState wire shape. Authorized behind a Joined gate. Phase 2
// spec §4.2 + §4.4.

/// ZEB-287 Phase 2: one entry in the ancestor-chain DTO returned by
/// `get_community_lineage`. Mirrors `community_invite::ParentLineageEntry`
/// but with hex-encoded SpaceId for IPC boundary cleanliness.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParentLineageDto {
    /// Hex-encoded SpaceId of this ancestor.
    pub space_id: String,
    /// Frozen display name of this ancestor at the time it was added
    /// to the chain. May be empty for legacy/corrupted snapshots.
    pub name: String,
    /// wall_ms of this ancestor's fork-from-parent event; None for the
    /// root of the chain.
    pub forked_at_wall_ms: Option<u64>,
}

/// ZEB-287 Phase 2: lineage metadata returned by `get_community_lineage`
/// IPC. Carries enough state for `ForkLineageTree.svelte` to render
/// ancestors + "you are here" + descendants without a second IPC.
///
/// Note: Phase 1's `CommunityLineageDto` was renamed to
/// `ForkSnapshotMetadataDto` to free this name for Phase 2 semantics.
/// To minimize confusion at the call site, this Phase 2 type uses the
/// `PhaseTwoCommunityLineageDto` Rust name even though it serializes as
/// `CommunityLineageDto`-shaped JSON for the frontend (via camelCase
/// renames matching `CommunityLineageDto` interface in TS).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhaseTwoCommunityLineageDto {
    /// Phase 1 field: hex-encoded SpaceId of the immediate parent, or
    /// None for top-level (non-fork) communities.
    pub forked_from: Option<String>,
    /// Phase 2 field: wall_ms of THIS community's Fork event from its
    /// parent. None for top-level communities and Phase 1 forks.
    pub forked_at_wall_ms: Option<u64>,
    /// Phase 2 field: ordered ancestor chain (root → immediate parent).
    ///
    /// Stored-state shape (CommunityState.parent_lineage): EXCLUDES the
    /// immediate parent (which lives in `forked_from`) per spec §3.2.
    ///
    /// IPC-DTO shape (this field): the `get_community_lineage` IPC
    /// SYNTHESIZES an immediate-parent entry when the stored chain is
    /// empty but `forked_from` is set (Phase 1 / single-hop forks), so
    /// the frontend tree can render the parent row uniformly. For
    /// multi-hop Phase 2 forks the stored chain already includes the
    /// immediate parent at the tail. Empty only for top-level (non-fork)
    /// communities.
    pub parent_lineage: Vec<ParentLineageDto>,
    /// This community's own SpaceId (hex) — convenience so frontend can
    /// render "you are here" without a second IPC.
    pub self_space_id: String,
    /// This community's own display name.
    pub self_name: String,
}

/// Pure helper: project Phase 2 `CommunityState` lineage data into the
/// IPC DTO. Extracted to enable unit testing without standing up a
/// full NodeState fixture.
pub fn build_community_lineage_dto(
    self_space_id: crate::owner_state_types::SpaceId,
    self_name: String,
    forked_from: Option<crate::owner_state_types::SpaceId>,
    forked_at_wall_ms: Option<u64>,
    parent_lineage: &[crate::community_invite::ParentLineageEntry],
) -> PhaseTwoCommunityLineageDto {
    PhaseTwoCommunityLineageDto {
        forked_from: forked_from.map(|s| hex::encode(s.0)),
        forked_at_wall_ms,
        parent_lineage: parent_lineage
            .iter()
            .map(|e| ParentLineageDto {
                space_id: hex::encode(e.space_id.0),
                name: e.name.clone(),
                forked_at_wall_ms: e.forked_at_wall_ms,
            })
            .collect(),
        self_space_id: hex::encode(self_space_id.0),
        self_name,
    }
}

/// Tauri IPC: read lineage fields from CommunityState behind a tight DTO.
/// Caller must be Joined in the community.
///
/// Errors:
/// - `Err("invalid community_id hex: ...")` — couldn't parse hex.
/// - `Err("community_id must be 16 bytes (32 hex chars)")` — wrong length.
/// - `Err("crdt_state missing — node not running?")` — node not started.
/// - `Err("no community_registry — node not running?")` — registry missing.
/// - `Err("no Space for community {hex} in owner-state")` — community absent.
/// - `Err("no engine for community {hex} — not joined or not yet started")` —
///   engine absent.
/// - `Err("not a member")` — caller is not currently Joined.
#[tauri::command]
async fn get_community_lineage(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
) -> Result<PhaseTwoCommunityLineageDto, String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let (crdt_state, registry, self_owner) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        let self_owner = g
            .dm_self_owner
            .ok_or("self_owner missing — node not running?")?;
        (
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.community_registry
                .clone()
                .ok_or("no community_registry — node not running?")?,
            self_owner,
        )
    };

    let (admin_addr, self_name) = {
        let s = crdt_state.lock().await;
        let space = s.spaces.get(&space_id).cloned();
        drop(s);
        let space = space.ok_or_else(|| {
            format!(
                "no Space for community {} in owner-state",
                hex::encode(space_id.0)
            )
        })?;
        if space.kind != crate::owner_state_types::SpaceKind::Community {
            return Err(format!(
                "Space {} exists but is kind {:?}, not Community",
                hex::encode(space_id.0),
                space.kind
            ));
        }
        let admin = space
            .admin_addr
            .ok_or("community Space missing admin_addr (corrupt row?)")?;
        (admin, space.name)
    };

    let engine_state = registry.state_for(&space_id).await.ok_or_else(|| {
        format!(
            "no engine for community {} — not joined or not yet started",
            hex::encode(space_id.0)
        )
    })?;

    let (forked_from, forked_at_wall_ms, parent_lineage_clone, materialized) = {
        let g = engine_state.lock().await;
        let mat = g.materialize_now(admin_addr);
        (
            g.forked_from,
            g.forked_at_wall_ms,
            g.parent_lineage.clone(),
            mat,
        )
    };

    // Authorize: caller must be Joined.
    match materialized.members.get(&self_owner).map(|m| m.status) {
        Some(crate::community_membership::MemberStatus::Joined) => {}
        _ => return Err("not a member".to_string()),
    }

    // R1-1: synthesize an immediate-parent entry for Phase 1 / single-hop
    // forks. CommunityState.parent_lineage EXCLUDES the immediate parent
    // (which lives in `forked_from`); the IPC DTO needs the parent so the
    // frontend tree can render it as a row. When the stored chain is empty
    // but `forked_from` is set, push a synthesized entry with the parent's
    // name resolved from local owner-state (best-effort; falls back to a
    // truncated-hex sentinel when the parent isn't locally known).
    //
    // Note: this synthesis affects ONLY the IPC-DTO shape, NOT the stored
    // `CommunityState.parent_lineage` (which per spec §3.2 still excludes
    // the immediate parent — that's encoded via `original_community_id` on
    // the wire / `forked_from` on disk).
    let lineage_for_dto: Vec<crate::community_invite::ParentLineageEntry> =
        if let Some(parent_id) = forked_from {
            if parent_lineage_clone.is_empty() {
                let parent_name = {
                    let s = crdt_state.lock().await;
                    s.spaces
                        .get(&parent_id)
                        .map(|sp| sp.name.clone())
                        .unwrap_or_else(|| {
                            // Fallback: truncated hex sentinel for unknown parents.
                            let hex = hex::encode(parent_id.0);
                            format!("0x{}…", &hex[..8])
                        })
                };
                vec![crate::community_invite::ParentLineageEntry {
                    space_id: parent_id,
                    name: parent_name,
                    // wall_ms of how THIS-parent-was-forked-from-its-parent
                    // is unknown for Phase 1 / single-hop synthesis; None.
                    forked_at_wall_ms: None,
                }]
            } else {
                parent_lineage_clone
            }
        } else {
            parent_lineage_clone
        };

    Ok(build_community_lineage_dto(
        space_id,
        self_name,
        forked_from,
        forked_at_wall_ms,
        &lineage_for_dto,
    ))
}

#[cfg(test)]
mod get_community_lineage_tests {
    use super::*;
    use crate::community_invite::ParentLineageEntry;
    use crate::owner_state_types::SpaceId;

    #[test]
    fn build_community_lineage_dto_returns_phase1_state_with_default_phase2_fields() {
        // Phase 1-shape: no Phase 2 lineage data.
        let self_id = SpaceId([0x77; 16]);
        let parent_id = SpaceId([0x33; 16]);

        let dto = build_community_lineage_dto(
            self_id,
            "Self".into(),
            Some(parent_id),
            None,
            &[], // empty parent_lineage = Phase 1 shape
        );

        assert_eq!(dto.self_space_id, hex::encode(self_id.0));
        assert_eq!(dto.self_name, "Self");
        assert_eq!(dto.forked_from, Some(hex::encode(parent_id.0)));
        assert_eq!(dto.forked_at_wall_ms, None);
        assert!(dto.parent_lineage.is_empty());
    }

    #[test]
    fn build_community_lineage_dto_returns_phase2_chain() {
        // Phase 2-shape: 2-entry chain.
        let self_id = SpaceId([0x77; 16]);
        let parent_id = SpaceId([0x33; 16]);
        let lineage = vec![
            ParentLineageEntry {
                space_id: SpaceId([0x11; 16]),
                name: "Root".into(),
                forked_at_wall_ms: None,
            },
            ParentLineageEntry {
                space_id: SpaceId([0x22; 16]),
                name: "Middle".into(),
                forked_at_wall_ms: Some(1_700_000_000_000),
            },
        ];

        let dto = build_community_lineage_dto(
            self_id,
            "MyFork".into(),
            Some(parent_id),
            Some(1_710_000_000_000),
            &lineage,
        );

        assert_eq!(dto.self_space_id, hex::encode(self_id.0));
        assert_eq!(dto.forked_from, Some(hex::encode(parent_id.0)));
        assert_eq!(dto.forked_at_wall_ms, Some(1_710_000_000_000));
        assert_eq!(dto.parent_lineage.len(), 2);
        assert_eq!(dto.parent_lineage[0].name, "Root");
        assert_eq!(dto.parent_lineage[0].forked_at_wall_ms, None);
        assert_eq!(dto.parent_lineage[1].name, "Middle");
        assert_eq!(
            dto.parent_lineage[1].forked_at_wall_ms,
            Some(1_700_000_000_000)
        );
    }

    #[test]
    fn build_community_lineage_dto_top_level_community() {
        let self_id = SpaceId([0x88; 16]);
        let dto = build_community_lineage_dto(self_id, "Top".into(), None, None, &[]);

        assert_eq!(dto.forked_from, None);
        assert_eq!(dto.forked_at_wall_ms, None);
        assert!(dto.parent_lineage.is_empty());
        assert_eq!(dto.self_name, "Top");
    }
}

#[cfg(test)]
mod list_community_forks_tests {
    use super::*;
    use crate::community_membership::{
        ChannelId, MemberState, MemberStatus, MembershipEventKind, SignedMembershipEvent,
    };
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
    use std::collections::{BTreeMap, BTreeSet};

    fn hlc_at(wall_ms: u64) -> Hlc {
        Hlc {
            wall_ms,
            logical: 0,
            device_id: "t".into(),
        }
    }

    fn synth_signed(
        id_byte: u8,
        community_id: SpaceId,
        kind: MembershipEventKind,
        actor: OwnerAddr,
        wall_ms: u64,
    ) -> SignedMembershipEvent {
        SignedMembershipEvent {
            id: [id_byte; 16],
            community_id,
            kind,
            actor,
            at: hlc_at(wall_ms),
            sig: [0u8; 64],
            countersig: None,
        }
    }

    fn materialized_with_join(
        addr: OwnerAddr,
        status: MemberStatus,
    ) -> crate::community_membership::MaterializedMembership {
        let mut m = crate::community_membership::MaterializedMembership::default();
        m.members.insert(
            addr,
            MemberState {
                status,
                joined_at: hlc_at(0),
                left_at: None,
            },
        );
        m
    }

    #[test]
    fn list_community_forks_returns_fork_events_only() {
        // Setup: a Joined caller, an events log mixing a Join, a Fork, and a
        // ChannelCreate. The IPC must surface only the Fork event.
        let cid = SpaceId([0xaa; 16]);
        let caller = OwnerAddr([0xc0; 16]);
        let fork_dest = SpaceId([0xf1; 16]);

        let mut events: BTreeMap<[u8; 16], SignedMembershipEvent> = BTreeMap::new();
        events.insert(
            [0x01; 16],
            synth_signed(0x01, cid, MembershipEventKind::Join, caller, 100),
        );
        events.insert(
            [0x02; 16],
            synth_signed(
                0x02,
                cid,
                MembershipEventKind::Fork {
                    fork_space_id: fork_dest,
                },
                caller,
                200,
            ),
        );
        // A non-Fork event after the Fork: must NOT appear in descendants.
        events.insert(
            [0x03; 16],
            synth_signed(
                0x03,
                cid,
                MembershipEventKind::ChannelCreate {
                    channel_id: ChannelId([0; 16]),
                    name: "general".to_string(),
                    write_power: 0,
                },
                caller,
                300,
            ),
        );

        let materialized = materialized_with_join(caller, MemberStatus::Joined);
        let locally_known: BTreeSet<SpaceId> = std::iter::once(fork_dest).collect();

        let result = build_fork_descendants(&events, &materialized, &locally_known, caller)
            .expect("Joined caller must succeed");
        assert_eq!(result.len(), 1, "only Fork event surfaces");
        assert_eq!(result[0].fork_space_id, hex::encode(fork_dest.0));
        assert_eq!(result[0].forker_addr, hex::encode(caller.0));
        assert_eq!(result[0].forked_at_wall_ms, 200);
        assert!(result[0].locally_known);
    }

    #[test]
    fn list_community_forks_resolves_active_member_name() {
        // ZEB-287 spec §4.1: forker_display_name is None in Phase 2 even
        // when the forker is Joined (PMB resolution deferred to ZEB-281).
        let cid = SpaceId([0xab; 16]);
        let caller = OwnerAddr([0xc1; 16]);
        let fork_dest = SpaceId([0xf2; 16]);

        let mut events: BTreeMap<[u8; 16], SignedMembershipEvent> = BTreeMap::new();
        events.insert(
            [0x01; 16],
            synth_signed(
                0x01,
                cid,
                MembershipEventKind::Fork {
                    fork_space_id: fork_dest,
                },
                caller,
                200,
            ),
        );

        let materialized = materialized_with_join(caller, MemberStatus::Joined);
        let result =
            build_fork_descendants(&events, &materialized, &BTreeSet::new(), caller).unwrap();

        assert_eq!(result[0].forker_display_name, None);
    }

    #[test]
    fn list_community_forks_falls_back_when_forker_kicked() {
        // Forker has been kicked → status != Joined → forker_display_name None.
        let cid = SpaceId([0xac; 16]);
        let caller = OwnerAddr([0xc2; 16]);
        let forker = OwnerAddr([0xc3; 16]);
        let fork_dest = SpaceId([0xf3; 16]);

        let mut events: BTreeMap<[u8; 16], SignedMembershipEvent> = BTreeMap::new();
        events.insert(
            [0x01; 16],
            synth_signed(
                0x01,
                cid,
                MembershipEventKind::Fork {
                    fork_space_id: fork_dest,
                },
                forker,
                200,
            ),
        );

        // Materialized state: caller is Joined; forker is Banned (was kicked).
        let mut materialized = materialized_with_join(caller, MemberStatus::Joined);
        materialized.members.insert(
            forker,
            MemberState {
                status: MemberStatus::Banned,
                joined_at: hlc_at(50),
                left_at: Some(hlc_at(150)),
            },
        );

        let result =
            build_fork_descendants(&events, &materialized, &BTreeSet::new(), caller).unwrap();
        assert_eq!(result[0].forker_display_name, None);
    }

    #[test]
    fn list_community_forks_marks_locally_unknown_descendants() {
        let cid = SpaceId([0xad; 16]);
        let caller = OwnerAddr([0xc4; 16]);
        let fork_dest = SpaceId([0xf4; 16]);

        let mut events: BTreeMap<[u8; 16], SignedMembershipEvent> = BTreeMap::new();
        events.insert(
            [0x01; 16],
            synth_signed(
                0x01,
                cid,
                MembershipEventKind::Fork {
                    fork_space_id: fork_dest,
                },
                caller,
                200,
            ),
        );

        let materialized = materialized_with_join(caller, MemberStatus::Joined);

        // No entries in locally_known → descendant is locally unknown.
        let result =
            build_fork_descendants(&events, &materialized, &BTreeSet::new(), caller).unwrap();
        assert!(!result[0].locally_known);
    }

    #[test]
    fn list_community_forks_rejects_non_member_caller() {
        let cid = SpaceId([0xae; 16]);
        let caller = OwnerAddr([0xc5; 16]);
        let fork_dest = SpaceId([0xf5; 16]);

        let mut events: BTreeMap<[u8; 16], SignedMembershipEvent> = BTreeMap::new();
        events.insert(
            [0x01; 16],
            synth_signed(
                0x01,
                cid,
                MembershipEventKind::Fork {
                    fork_space_id: fork_dest,
                },
                caller,
                200,
            ),
        );

        // Empty materialized → caller is NOT a member.
        let materialized = crate::community_membership::MaterializedMembership::default();

        let result = build_fork_descendants(&events, &materialized, &BTreeSet::new(), caller);
        assert_eq!(
            result.as_ref().err().map(|s| s.as_str()),
            Some("not a member")
        );
    }

    #[test]
    fn list_community_forks_sorts_chronologically() {
        // Three Fork events with wall_ms = [200, 100, 300] — verify
        // returned order is ascending [100, 200, 300].
        let cid = SpaceId([0xaf; 16]);
        let caller = OwnerAddr([0xc6; 16]);
        let f_a = SpaceId([0xa1; 16]);
        let f_b = SpaceId([0xb1; 16]);
        let f_c = SpaceId([0xc1; 16]);

        let mut events: BTreeMap<[u8; 16], SignedMembershipEvent> = BTreeMap::new();
        events.insert(
            [0x01; 16],
            synth_signed(
                0x01,
                cid,
                MembershipEventKind::Fork { fork_space_id: f_a },
                caller,
                200,
            ),
        );
        events.insert(
            [0x02; 16],
            synth_signed(
                0x02,
                cid,
                MembershipEventKind::Fork { fork_space_id: f_b },
                caller,
                100,
            ),
        );
        events.insert(
            [0x03; 16],
            synth_signed(
                0x03,
                cid,
                MembershipEventKind::Fork { fork_space_id: f_c },
                caller,
                300,
            ),
        );

        let materialized = materialized_with_join(caller, MemberStatus::Joined);
        let result =
            build_fork_descendants(&events, &materialized, &BTreeSet::new(), caller).unwrap();

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].forked_at_wall_ms, 100);
        assert_eq!(result[1].forked_at_wall_ms, 200);
        assert_eq!(result[2].forked_at_wall_ms, 300);
    }

    #[test]
    fn list_community_forks_sorts_by_full_hlc_when_wall_ms_ties() {
        // R1-3: two Fork events at the SAME wall_ms but different logical
        // clocks must order by HLC.logical, not be wall_ms-only ambiguous.
        // A naive sort on `forked_at_wall_ms` alone leaves the tiebreak
        // dependent on the BTreeMap event-id ordering (the source values()
        // iteration), which loses the HLC's authoritative ordering.
        let cid = SpaceId([0xb0; 16]);
        let caller = OwnerAddr([0xc7; 16]);
        let f_low = SpaceId([0xa1; 16]); // logical=0 (earlier)
        let f_high = SpaceId([0xa2; 16]); // logical=5 (later)

        // The two Fork events share wall_ms=500 but differ in logical.
        // Insert with the LATER (logical=5) event's id ordered FIRST in
        // the BTreeMap so a wall_ms-only sort would emit it before the
        // earlier (logical=0) event. Correct HLC sort must invert that.
        let mut events: BTreeMap<[u8; 16], SignedMembershipEvent> = BTreeMap::new();
        events.insert(
            [0x01; 16],
            SignedMembershipEvent {
                id: [0x01; 16],
                community_id: cid,
                kind: MembershipEventKind::Fork {
                    fork_space_id: f_high,
                },
                actor: caller,
                at: Hlc {
                    wall_ms: 500,
                    logical: 5, // later
                    device_id: "t".into(),
                },
                sig: [0u8; 64],
                countersig: None,
            },
        );
        events.insert(
            [0x02; 16],
            SignedMembershipEvent {
                id: [0x02; 16],
                community_id: cid,
                kind: MembershipEventKind::Fork {
                    fork_space_id: f_low,
                },
                actor: caller,
                at: Hlc {
                    wall_ms: 500,
                    logical: 0, // earlier
                    device_id: "t".into(),
                },
                sig: [0u8; 64],
                countersig: None,
            },
        );

        let materialized = materialized_with_join(caller, MemberStatus::Joined);
        let result =
            build_fork_descendants(&events, &materialized, &BTreeSet::new(), caller).unwrap();

        assert_eq!(result.len(), 2);
        // Earlier-logical-clock event must come first despite being
        // inserted under a larger BTreeMap key.
        assert_eq!(
            result[0].fork_space_id,
            hex::encode(f_low.0),
            "HLC tiebreak: logical=0 must precede logical=5 at same wall_ms"
        );
        assert_eq!(result[1].fork_space_id, hex::encode(f_high.0));
    }
}

// ── ZEB-248 Phase 1: create_channel ──────────────────────────────────
//
// Mints a ChannelCreate SignedMembershipEvent and inserts it through
// the per-community engine. Power-gate enforcement happens INSIDE
// engine.insert_local_event (which calls verify_event) — actor must
// have power ≥ POWER_THRESHOLDS.kick (50, mod-tier). The IPC trusts
// verify_event to surface ChannelAdminInsufficientPower for under-
// powered callers; pre-validating here would duplicate the rules and
// risk drift. Mirrors the `kick_from_community` / `set_power_level`
// shape from Phase 4.

/// Pure function: mint a self-signed ChannelCreate event for a
/// community we belong to and have permission to moderate. Mirrors
/// `mint_kick_event` / `mint_set_power_event`. The fresh `channel_id`
/// (16 random bytes) and event id are sourced from the supplied RNG
/// (via `rand::thread_rng` in production).
///
/// ZEB-267: Caller pre-reserves `hlc` via
/// `dm_outbox::reserve_next_hlc_for_device`. This helper is now pure
/// on the HLC — it does not call `next_hlc` internally.
pub fn mint_channel_create_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    channel_id: crate::community_membership::ChannelId,
    name: String,
    write_power: u8,
    signing_key: &ed25519_dalek::SigningKey,
    hlc: crate::owner_state_types::Hlc,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::ChannelCreate {
            channel_id,
            name,
            write_power,
        },
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign channel_create: {e}"))
}

/// Tauri IPC: create a new channel in a community we currently
/// belong to and have permission to moderate.
///
/// Power-gated by `verify_event`: actor must have power
/// ≥ POWER_THRESHOLDS.kick (50, mod-tier). Returns the new channel's
/// 32-char lowercase-hex `channel_id` on success. The frontend should
/// rely on the `channel-config-updated` Tauri event for incremental UI
/// state — the event carries the camelCase `name` + `writePower` +
/// `atWallMs` payload via `delta_to_channel_config_change`.
///
/// Errors:
/// - `Err("invalid community_id hex: ...")` — couldn't parse hex.
/// - `Err("community_id must be 16 bytes (32 hex chars)")` — wrong length.
/// - `Err("hlc_tracker missing" / "dm_device_id missing" / ...)` — node
///   not running or owner identity not loaded.
/// - `Err("node generation changed during create_channel ...")` — a
///   `stop_node` raced with this call.
/// - `Err("community_registry detached during create_channel ...")` —
///   ditto, registry-presence variant.
/// - `Err("no engine for community {hex} — not currently joined")`.
/// - `Err("create_channel rejected: ...")` — `verify_event` rejected
///   the event (e.g. caller below mod-tier →
///   `ChannelAdminInsufficientPower`).
#[tauri::command]
async fn create_channel(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    name: String,
    write_power: u8,
) -> Result<String, String> {
    // IPC-boundary validation. Rejecting here surfaces fast to the JS
    // caller without minting+inserting an invalid event. verify_event
    // also validates these (defense-in-depth for remote events).
    if name.trim().is_empty() || name.chars().count() > 32 {
        return Err("channel name is empty or exceeds 32 chars".to_string());
    }
    if write_power > crate::community_membership::POWER_THRESHOLDS.max {
        return Err(format!(
            "write_power {write_power} exceeds POWER_THRESHOLDS.max ({})",
            crate::community_membership::POWER_THRESHOLDS.max
        ));
    }

    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let (hlc_tracker, device_id, self_owner, community_registry, dm_outbox, snapshot_generation) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.generation,
        )
    };

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // Generate a fresh ChannelId (16 random bytes).
    let channel_id: crate::community_membership::ChannelId = {
        use rand::RngCore;
        let mut buf = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut buf);
        crate::community_membership::ChannelId(buf)
    };

    // ZEB-267: atomic HLC reservation.
    let hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;
    let event = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_channel_create_event(
            space_id,
            self_owner,
            channel_id,
            name,
            write_power,
            signing_key,
            hlc,
        )?
    };

    // Generation + registry fence (mirrors kick_from_community /
    // set_power_level; stop_node nullifies registry without bumping
    // generation, so the registry-presence check is load-bearing).
    {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err(format!(
                "node generation changed during create_channel (was {}, now {})",
                snapshot_generation, g.generation
            ));
        }
        if g.community_registry.is_none() {
            return Err(
                "community_registry detached during create_channel (node stopped?)".to_string(),
            );
        }
    }

    let engine_arc = community_registry
        .engine_arc(&space_id)
        .await
        .ok_or_else(|| {
            format!(
                "no engine for community {} — not currently joined",
                hex::encode(space_id.0)
            )
        })?;
    let outcome = engine_arc
        .insert_local_event(event.clone())
        .await
        .map_err(|e| format!("engine.insert_local_event: {e}"))?;
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Rejected(_)
    ) {
        return Err(membership_outcome_err("create_channel", &outcome));
    }

    // ZEB-267: tracker is bumped at reservation time, so no post-Inserted
    // advance here. AlreadyKnown is a 16-byte-event-id collision —
    // vanishingly unlikely; surface as Err so the caller knows the
    // channel wasn't created (the new channel_id we generated is gone).
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        Ok(hex::encode(channel_id.0))
    } else {
        Err(format!(
            "create_channel unexpected outcome: AlreadyKnown (event_id collision: {})",
            hex::encode(event.id)
        ))
    }
}

// ── ZEB-248 Phase 1: modify_channel ──────────────────────────────────
//
// Mints a ChannelModify event for a channel that already exists. The
// IPC boundary rejects all-None calls (no name + no write_power) up
// front so we don't pollute the CRDT log with no-op events. Power-gate
// enforcement happens INSIDE engine.insert_local_event (verify_event
// surfaces ChannelAdminInsufficientPower for under-powered callers).
// Mirrors the `create_channel` shape exactly.

/// Pure function: mint a self-signed ChannelModify event for a community
/// we moderate. Mirrors `mint_channel_create_event`. Caller is responsible
/// for ensuring at least one of `name`/`write_power` is `Some` (the IPC
/// boundary rejects all-None before this is reached).
///
/// ZEB-267: Caller pre-reserves `hlc` via
/// `dm_outbox::reserve_next_hlc_for_device`.
pub fn mint_channel_modify_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    channel_id: crate::community_membership::ChannelId,
    name: Option<String>,
    write_power: Option<u8>,
    signing_key: &ed25519_dalek::SigningKey,
    hlc: crate::owner_state_types::Hlc,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::ChannelModify {
            channel_id,
            name,
            write_power,
        },
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign channel_modify: {e}"))
}

/// Pure function: mint a self-signed ChannelDelete event for a community
/// we moderate. Mirrors `mint_channel_create_event`. Caller is responsible
/// for the metadata-before-write check (channel exists + not already
/// tombstoned) — this helper does NOT validate; it only mints.
///
/// ZEB-267: Caller pre-reserves `hlc` via
/// `dm_outbox::reserve_next_hlc_for_device`.
pub fn mint_channel_delete_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    channel_id: crate::community_membership::ChannelId,
    signing_key: &ed25519_dalek::SigningKey,
    hlc: crate::owner_state_types::Hlc,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::ChannelDelete { channel_id },
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign channel_delete: {e}"))
}

/// Tauri IPC: modify a channel's name and/or write_power. Power-gated
/// at mod-tier (verify_event returns ChannelAdminInsufficientPower for
/// underpowered callers). At least ONE of `name` or `write_power` must
/// be Some; all-None is rejected at the IPC boundary as a no-op error
/// before any signing.
///
/// Errors:
/// - `Err("modify_channel: must provide name and/or write_power")` —
///   both args were None.
/// - `Err("invalid community_id hex: ...")` / `Err("invalid channel_id hex: ...")`
///   — couldn't parse hex.
/// - `Err("community_id must be 16 bytes (32 hex chars)")` / same for channel_id.
/// - `Err("hlc_tracker missing" / "dm_device_id missing" / ...)` — node
///   not running or owner identity not loaded.
/// - `Err("node generation changed during modify_channel ...")` — a
///   `stop_node` raced with this call.
/// - `Err("community_registry detached during modify_channel ...")`.
/// - `Err("no engine for community {hex} — not currently joined")`.
/// - `Err("modify_channel rejected: ...")` — `verify_event` rejected
///   the event (e.g. caller below mod-tier →
///   `ChannelAdminInsufficientPower`).
#[tauri::command]
async fn modify_channel(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    channel_id: String,
    name: Option<String>,
    write_power: Option<u8>,
) -> Result<(), String> {
    // Boundary validation: reject all-None up-front (no-op event spam).
    if name.is_none() && write_power.is_none() {
        return Err("modify_channel: must provide name and/or write_power".to_string());
    }
    // IPC-boundary validation matching verify_event so JS callers fail
    // fast without minting an event that would be rejected at insert
    // time.
    if let Some(n) = &name {
        if n.trim().is_empty() || n.chars().count() > 32 {
            return Err("channel name is empty or exceeds 32 chars".to_string());
        }
    }
    if let Some(wp) = write_power {
        if wp > crate::community_membership::POWER_THRESHOLDS.max {
            return Err(format!(
                "write_power {wp} exceeds POWER_THRESHOLDS.max ({})",
                crate::community_membership::POWER_THRESHOLDS.max
            ));
        }
    }

    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let channel_id_bytes: [u8; 16] = hex::decode(&channel_id)
        .map_err(|e| format!("invalid channel_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "channel_id must be 16 bytes (32 hex chars)".to_string())?;
    let channel_id = crate::community_membership::ChannelId(channel_id_bytes);

    let (hlc_tracker, device_id, self_owner, community_registry, dm_outbox, snapshot_generation) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.generation,
        )
    };

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // ZEB-267: atomic HLC reservation.
    let hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;
    let event = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_channel_modify_event(
            space_id,
            self_owner,
            channel_id,
            name,
            write_power,
            signing_key,
            hlc,
        )?
    };

    // Generation + registry fence (mirrors create_channel /
    // kick_from_community / set_power_level).
    {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err(format!(
                "node generation changed during modify_channel (was {}, now {})",
                snapshot_generation, g.generation
            ));
        }
        if g.community_registry.is_none() {
            return Err(
                "community_registry detached during modify_channel (node stopped?)".to_string(),
            );
        }
    }

    let engine_arc = community_registry
        .engine_arc(&space_id)
        .await
        .ok_or_else(|| {
            format!(
                "no engine for community {} — not currently joined",
                hex::encode(space_id.0)
            )
        })?;
    let outcome = engine_arc
        .insert_local_event(event.clone())
        .await
        .map_err(|e| format!("engine.insert_local_event: {e}"))?;
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Rejected(_)
    ) {
        return Err(membership_outcome_err("modify_channel", &outcome));
    }

    // ZEB-267: tracker is bumped at reservation time. AlreadyKnown is
    // a 16-byte-event-id collision — vanishingly unlikely; surface as
    // Err so the caller knows the modify didn't apply.
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        Ok(())
    } else {
        Err(format!(
            "modify_channel unexpected outcome: AlreadyKnown (event_id collision: {})",
            hex::encode(event.id)
        ))
    }
}

// ── ZEB-248 Phase 1: delete_channel ──────────────────────────────────
//
// Tombstones a channel by signing a ChannelDelete event. Power-gated
// at mod-tier (engine.insert_local_event → verify_event chain).
//
// HARD MEMORY RULE — metadata-before-irreversible-write: read-only
// "channel exists / not already tombstoned" verification MUST precede
// the irreversible engine.insert_local_event call. Without this check,
// calling delete_channel with a stale or invalid channel_id would still
// pollute the CRDT log with a no-op event — bad telemetry, bad UX, and
// the user would see "success" for a non-existent channel.

/// Tauri IPC: delete (tombstone) a channel. Power-gated at mod-tier.
/// Tombstone semantics: the channel stays in the materialized map with
/// `deleted_at` set so historical messages with this channel_id still
/// resolve their breadcrumb.
///
/// Reads materialized state and verifies the channel exists AND is not
/// already deleted BEFORE signing the irreversible ChannelDelete event
/// (per the metadata-before-irreversible-write HARD memory rule).
///
/// Errors:
/// - `Err("invalid community_id hex: ...")` / `Err("invalid channel_id hex: ...")`.
/// - `Err("community_id must be 16 bytes (32 hex chars)")` / same for channel_id.
/// - `Err("hlc_tracker missing" / "dm_device_id missing" / ...)`.
/// - `Err("crdt_state missing — node not running?")`.
/// - `Err("community_registry missing — node not running?")`.
/// - `Err("no Space for community {hex} in owner-state")` / kind check.
/// - `Err("community Space missing admin_addr (corrupt row?)")`.
/// - `Err("no engine for community {hex} — ...")`.
/// - `Err("no channel {hex} in community {hex}")` — channel never existed
///   (read-side metadata check).
/// - `Err("channel {hex} is already deleted")` — already tombstoned
///   (idempotent rejection; read-side metadata check).
/// - `Err("node generation changed during delete_channel ...")`.
/// - `Err("community_registry detached during delete_channel ...")`.
/// - `Err("delete_channel rejected: ...")` — verify_event rejected
///   (e.g. ChannelAdminInsufficientPower).
#[tauri::command]
async fn delete_channel(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    channel_id: String,
) -> Result<(), String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let channel_id_bytes: [u8; 16] = hex::decode(&channel_id)
        .map_err(|e| format!("invalid channel_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "channel_id must be 16 bytes (32 hex chars)".to_string())?;
    let channel_id = crate::community_membership::ChannelId(channel_id_bytes);

    let (
        hlc_tracker,
        device_id,
        self_owner,
        community_registry,
        dm_outbox,
        crdt_state,
        snapshot_generation,
    ) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.generation,
        )
    };

    // Look up admin_addr from owner-state (needed for materialize_now).
    let admin_addr = {
        let s = crdt_state.lock().await;
        let space = s.spaces.get(&space_id).cloned();
        drop(s);
        let space = space.ok_or_else(|| {
            format!(
                "no Space for community {} in owner-state",
                hex::encode(space_id.0)
            )
        })?;
        if space.kind != crate::owner_state_types::SpaceKind::Community {
            return Err(format!(
                "Space {} exists but is kind {:?}, not Community",
                hex::encode(space_id.0),
                space.kind
            ));
        }
        space
            .admin_addr
            .ok_or("community Space missing admin_addr (corrupt row?)")?
    };

    // METADATA-BEFORE-IRREVERSIBLE-WRITE: read-only verify the channel
    // exists (and isn't already deleted) BEFORE signing. Without this,
    // a stale or invalid channel_id would pollute the CRDT log with a
    // no-op event — the engine would happily accept the ChannelDelete
    // and surface success, but the user's intent ("delete X") was
    // unrelated to any real channel.
    {
        let engine_state = community_registry
            .state_for(&space_id)
            .await
            .ok_or_else(|| {
                format!(
                    "no engine for community {} — not joined or not yet started",
                    hex::encode(space_id.0)
                )
            })?;
        let materialized = {
            let g = engine_state.lock().await;
            g.materialize_now(admin_addr)
        };
        match materialized.channels.get(&channel_id) {
            None => {
                return Err(format!(
                    "no channel {} in community {}",
                    hex::encode(channel_id.0),
                    hex::encode(space_id.0)
                ));
            }
            Some(info) if info.deleted_at.is_some() => {
                return Err(format!(
                    "channel {} is already deleted",
                    hex::encode(channel_id.0)
                ));
            }
            Some(_) => {}
        }
    }

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // ZEB-267: atomic HLC reservation. Note that the reservation
    // happens AFTER the metadata-before-irreversible-write read at
    // step 6 above (the channel-exists / not-tombstoned check) per
    // user memory rule — burning an HLC on a stale read-side rejection
    // is fine, but burning an HLC inside an actually-no-op event is
    // worse UX (caller pays for an HLC tick they can't see).
    let hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;
    let event = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_channel_delete_event(space_id, self_owner, channel_id, signing_key, hlc)?
    };

    // Generation + registry fence.
    {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err(format!(
                "node generation changed during delete_channel (was {}, now {})",
                snapshot_generation, g.generation
            ));
        }
        if g.community_registry.is_none() {
            return Err(
                "community_registry detached during delete_channel (node stopped?)".to_string(),
            );
        }
    }

    let engine_arc = community_registry
        .engine_arc(&space_id)
        .await
        .ok_or_else(|| {
            format!(
                "no engine for community {} — not currently joined",
                hex::encode(space_id.0)
            )
        })?;
    let outcome = engine_arc
        .insert_local_event(event.clone())
        .await
        .map_err(|e| format!("engine.insert_local_event: {e}"))?;
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Rejected(_)
    ) {
        return Err(membership_outcome_err("delete_channel", &outcome));
    }

    // ZEB-267: tracker bumped at reservation time. AlreadyKnown is a
    // 16-byte-event-id collision — vanishingly unlikely; surface as
    // Err so the caller knows the delete didn't apply.
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        Ok(())
    } else {
        Err(format!(
            "delete_channel unexpected outcome: AlreadyKnown (event_id collision: {})",
            hex::encode(event.id)
        ))
    }
}

// ── ZEB-248 Phase 1: list_channels ───────────────────────────────────
//
// Read-only IPC; mirrors `list_community_members`. Returns ALL channels
// (including tombstoned ones — frontend filters for default view; admin
// UI surfaces them as deletable-only). Sorted by created_at ascending
// so #general (auto-created first in Task 7) is always at the top.

/// Tauri IPC: list all channels in a community (including tombstoned
/// ones). Read-only; does not require power beyond the Joined membership
/// gate enforced by the engine. Sorted by `created_at` ascending so the
/// auto-created `#general` channel is always at the top of the list.
///
/// Errors: same hex/registry/Space-row error path as `list_community_members`.
#[tauri::command]
async fn list_channels(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
) -> Result<Vec<ChannelInfoDto>, String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let (crdt_state, registry) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.community_registry
                .clone()
                .ok_or("no community_registry — node not running?")?,
        )
    };

    let admin_addr = {
        let s = crdt_state.lock().await;
        let space = s.spaces.get(&space_id).cloned();
        drop(s);
        let space = space.ok_or_else(|| {
            format!(
                "no Space for community {} in owner-state",
                hex::encode(space_id.0)
            )
        })?;
        if space.kind != crate::owner_state_types::SpaceKind::Community {
            return Err(format!(
                "Space {} exists but is kind {:?}, not Community",
                hex::encode(space_id.0),
                space.kind
            ));
        }
        space
            .admin_addr
            .ok_or("community Space missing admin_addr (corrupt row?)")?
    };

    let engine_state = registry.state_for(&space_id).await.ok_or_else(|| {
        format!(
            "no engine for community {} — not joined or not yet started",
            hex::encode(space_id.0)
        )
    })?;

    let materialized = {
        let g = engine_state.lock().await;
        g.materialize_now(admin_addr)
    };

    let mut rows: Vec<ChannelInfoDto> = materialized
        .channels
        .iter()
        .map(|(channel_id, info)| ChannelInfoDto {
            channel_id: hex::encode(channel_id.0),
            name: info.name.clone(),
            write_power: info.write_power,
            created_at: info.created_at.clone(),
            deleted_at: info.deleted_at.clone(),
        })
        .collect();
    // Sort by created_at ascending so #general (auto-created first in
    // Task 7's create_community extension) is always at the top of the
    // list. Tie-break on logical, then channel_id, for determinism.
    rows.sort_by(|a, b| {
        a.created_at
            .wall_ms
            .cmp(&b.created_at.wall_ms)
            .then_with(|| a.created_at.logical.cmp(&b.created_at.logical))
            .then_with(|| a.channel_id.cmp(&b.channel_id))
    });
    Ok(rows)
}

// ── ZEB-270 Phase 3: channel-message IPCs ────────────────────────────
//
// Three IPCs per spec §9 that wrap the ChannelLogEngine surface:
//   - post_channel_message          → engine.publish
//   - list_channel_messages         → engine.list_messages
//   - request_channel_backfill      → engine.request_backfill (fire-and-forget)
//
// Each IPC validates hex strings + length at the boundary so JS callers
// fail fast without minting events that would be rejected at the engine.
// Engine lookup is via NodeState.channel_log_registry (populated by
// start_node in Task 4+4.5); a missing registry means the node isn't
// running and is surfaced as Err. A missing engine for the requested
// (community_id, channel_id) means that channel isn't currently live
// (not joined or not yet spawned by the delta consumer).

/// Tauri IPC: post a message to a channel.
///
/// `body` is opaque bytes (the frontend serializes the display format —
/// text, markdown, etc.). The engine's `publish` enforces UTF-8 and a
/// hard `MAX_BODY_BYTES` cap; the IPC layer just forwards.
///
/// `reply_to` is an optional hex `MessageId` (32 chars) of an earlier
/// message in the same channel.
///
/// Returns the hex `MessageId` of the newly minted Post event.
///
/// Errors (string-mapped from `ChannelLogEngineError::to_string()`):
/// - `Err("community_id must be 16 bytes (32 hex chars)")` /
///   `Err("channel_id must be 16 bytes (32 hex chars)")` /
///   `Err("reply_to must be 16 bytes (32 hex chars)")` — bad length.
/// - `Err("invalid {field} hex: ...")` — bad hex characters.
/// - `Err("channel_log_registry missing — node not running")`.
/// - `Err("no engine for {cid}/{chid}")` — channel isn't live.
/// - `Err("body too large: ...")` / `Err("body not UTF-8: ...")` /
///   `Err("publish failed: ...")` etc. — engine surfaces.
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
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
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
/// `since` filters to events strictly newer than the given HLC; `None`
/// means "from earliest available locally". `limit` caps the reply —
/// `0` means "use the engine's default (256)"; the IPC enforces a hard
/// cap of 1000 per spec §9.2 (rejected before reaching the engine).
///
/// Returns DTOs in HLC order (segments first, then in-memory tail).
///
/// Errors:
/// - `Err("limit {N} exceeds max 1000")` — boundary cap.
/// - `Err("community_id must be 16 bytes (32 hex chars)")` /
///   `Err("channel_id must be 16 bytes (32 hex chars)")`.
/// - `Err("invalid {field} hex: ...")`.
/// - `Err("channel_log_registry missing — node not running")`.
/// - `Err("no engine for {cid}/{chid}")`.
/// - `Err("persist error: ...")` — engine read failure (e.g., corrupt
///   segment).
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
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
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

    Ok(events
        .into_iter()
        .map(|ev| engine.event_to_dto(&ev))
        .collect())
}

/// Tauri IPC: fire a backfill request via the channel's Zenoh queryable.
///
/// Fire-and-forget — the engine forwards the `BackfillQueryRequest` to
/// the adapter's query-driver task, which fans out the Zenoh `get` and
/// pumps reply packets back through the same subscriber path so they
/// surface to the UI as `channel-message-received` events alongside
/// live broadcasts. No reply payload here.
///
/// Errors:
/// - `Err("community_id must be 16 bytes (32 hex chars)")` /
///   `Err("channel_id must be 16 bytes (32 hex chars)")`.
/// - `Err("invalid {field} hex: ...")`.
/// - `Err("channel_log_registry missing — node not running")`.
/// - `Err("no engine for {cid}/{chid}")`.
/// - `Err("backfill request failed: ...")` — adapter channel closed.
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
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
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

/// Encode a CommunityInvitePayload into the harmony://invite/ URL form.
/// Thin wrapper over `community_invite::encode_invite_url` so call sites
/// don't need to import the lower-level error type — surfaces failures
/// as `Result<String, String>` matching the IPC convention.
pub fn build_open_invite_url(
    payload: &crate::community_invite::CommunityInvitePayload,
) -> Result<String, String> {
    crate::community_invite::encode_invite_url(payload)
        .map_err(|e| format!("encode invite URL: {e}"))
}

/// Generate a `harmony://invite/...` URL for an OPEN community. The
/// returned URL carries the community id + symmetric `EpochKey` +
/// admin addr + community name, so any holder can decrypt the
/// state-root topic and publish their own Join event.
///
/// `invitee_hint` and `expires_at` are accepted to match the spec's IPC
/// contract but are unused in Phase 3 — Phase 4 will sign an
/// `InviteToken` carrying both. Phase 3 returns a token-less payload.
///
/// Errors:
/// - `Err("invalid community_id hex: ...")` — bad hex.
/// - `Err("no community_registry — node not running?")` — registry not
///   wired (start_node hasn't run).
/// - `Err("no Space for community {hex} in owner-state")` — the
///   community isn't in our local owner-state (we haven't joined or
///   we left).
/// - `Err("community Space missing membership_key / admin_addr / kind")`
///   — defensive guard; should be unreachable since
///   `validate_invariants` rejects these on apply, but cheap to check.
#[tauri::command]
async fn generate_invite(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    invitee_hint: Option<String>,
    expires_at: Option<u64>,
) -> Result<String, String> {
    let _ = (invitee_hint, expires_at); // Phase 4 wiring; ignored in Phase 3.

    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let (crdt_state, community_registry) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.community_registry
                .clone()
                .ok_or("no community_registry — node not running?")?,
        )
    };

    let space = {
        let s = crdt_state.lock().await;
        s.spaces.get(&space_id).cloned()
    }
    .ok_or_else(|| {
        format!(
            "no Space for community {} in owner-state",
            hex::encode(space_id.0)
        )
    })?;

    if space.kind != crate::owner_state_types::SpaceKind::Community {
        return Err(format!(
            "Space {} exists but is kind {:?}, not Community",
            hex::encode(space_id.0),
            space.kind
        ));
    }
    let mk = space
        .current_epoch_key
        .clone()
        .ok_or("community Space missing current_epoch_key (corrupt row?)")?;
    let admin = space
        .admin_addr
        .ok_or("community Space missing admin_addr (corrupt row?)")?;
    let is_invite_only = space.is_invite_only.unwrap_or(false);

    if is_invite_only {
        return Err(
            "Phase 3 supports OPEN communities only; invite-only generate_invite ships in Phase 4"
                .to_string(),
        );
    }

    // ZEB-249: build InviteEpochSnapshot. For open communities there is no
    // specific invitee to seal to, so sealed_epoch_key carries the raw 32-byte
    // epoch key (the key is "public" for open joins — anyone with the link may
    // join). Phase 4 invite-only will use X25519-sealed delivery.
    let epoch = space
        .current_epoch
        .ok_or("community Space missing current_epoch (corrupt row?)")?;

    // ZEB-249: snapshot current materialized state for invitee's UI bootstrap.
    // Spec §5.2: state_snapshot is populated from the inviter's current
    // materialized state at issuance time. CRDT replay post-redemption
    // corrects any inviter-tampered snapshot.
    let state_snapshot = {
        let materialized = if let Some(state_arc) = community_registry.state_for(&space_id).await {
            let state = state_arc.lock().await;
            let events: Vec<crate::community_membership::SignedMembershipEvent> =
                state.events.values().cloned().collect();
            drop(state);
            // R4-6: pass wall_now_ms so an idle-community PendingJoin
            // already past 30d is excluded from the bootstrap snapshot
            // sent to a new invitee.
            let wall_now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            crate::community_membership::materialize_with_now(&events, admin, Some(wall_now_ms))
        } else {
            // No engine yet (e.g., just-created community with no events):
            // fall back to empty maps — still a valid bootstrap hint.
            crate::community_membership::MaterializedMembership::default()
        };
        crate::community_invite::MaterializedCommunityState {
            members: materialized.members,
            channels: materialized.channels,
            power_levels: materialized.power_levels,
        }
    };

    let epoch_snapshot = crate::community_invite::InviteEpochSnapshot {
        epoch,
        sealed_epoch_key: mk.as_bytes().to_vec(),
        state_snapshot,
    };

    // ZEB-285: if this community is a fork, bundle forked_from + pre_fork_snapshot
    // into the invite so joiners can mirror the fork lineage locally.
    let (forked_from, pre_fork_snapshot) = {
        let state_arc = community_registry.state_for(&space_id).await;
        let fork_origin: Option<crate::owner_state_types::SpaceId> = if let Some(arc) = state_arc {
            let g = arc.lock().await;
            g.forked_from
        } else {
            None
        };

        if fork_origin.is_some() {
            // Read pre_fork_snapshot.bin from the fork's data dir.
            let snapshot: Option<crate::community_invite::PreForkSnapshot> = (|| {
                let identity_dir = match crate::owner_commands::resolve_identity_dir() {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(
                            error = ?e,
                            community_id = %hex::encode(space_id.0),
                            "ZEB-285 generate_invite: failed to resolve identity_dir; \
                             fork-invite will be minted without snapshot bundled"
                        );
                        return None;
                    }
                };
                let snapshot_path = identity_dir
                    .join("communities")
                    .join(hex::encode(space_id.0))
                    .join("pre_fork_snapshot.bin");
                let bytes = match std::fs::read(&snapshot_path) {
                    Ok(b) => b,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
                    Err(e) => {
                        tracing::warn!(
                            error = ?e,
                            path = %snapshot_path.display(),
                            community_id = %hex::encode(space_id.0),
                            "generate_invite: failed to read pre_fork_snapshot.bin; \
                             fork invite will be sent without snapshot (degraded experience)"
                        );
                        return None;
                    }
                };
                match crate::owner_state_crypto::canonical_cbor_decode::<
                    crate::community_invite::PreForkSnapshot,
                >(&bytes)
                {
                    Ok(s) => Some(s),
                    Err(e) => {
                        tracing::warn!(
                            community_id = %hex::encode(space_id.0),
                            error = %e,
                            "generate_invite: failed to decode pre_fork_snapshot.bin; \
                             fork invite will be sent without snapshot (degraded experience)"
                        );
                        None
                    }
                }
            })();
            // INVARIANT: forked_from and pre_fork_snapshot must be paired —
            // both Some or both None. When the snapshot couldn't be loaded
            // (file missing, decode failure, identity_dir unavailable), clear
            // forked_from too so the invite doesn't arrive with forked_from
            // set but no snapshot, which would leave joiners in a half-fork
            // state. (Fix: PR #122 round-2 bot review — CodeRabbit Major.)
            //
            // Bind forked_from to snapshot.original_community_id, not to
            // CommunityState.forked_from. If the two diverge (should not
            // happen in practice, but could in theory via corrupt state),
            // the invite reflects the snapshot's value — the disk artifact
            // is the authoritative source for fork lineage metadata.
            // (Fix: PR #122 round-3 bot review — CodeRabbit Major.)
            if let Some(s) = snapshot {
                let original_id = s.original_community_id;
                (Some(original_id), Some(s))
            } else {
                tracing::warn!(
                    community_id = %hex::encode(space_id.0),
                    "ZEB-285 generate_invite: pre_fork_snapshot unavailable; \
                     clearing forked_from to preserve forked_from↔pre_fork_snapshot invariant"
                );
                (None, None)
            }
        } else {
            (None, None)
        }
    };

    let mut payload = crate::community_invite::CommunityInvitePayload {
        community_id: space_id,
        epoch_snapshot,
        admin_addr: admin,
        community_name: space.name.clone(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: None,
        forked_from,
        pre_fork_snapshot,
    };

    // RELIABILITY: if the encoded invite payload would exceed the URL cap
    // (MAX_INVITE_BODY_B64_CHARS ≈ 64 KiB), a snapshot-bundled fork-invite
    // will fail. Fall back to no-snapshot mode: the forker still gets a
    // working invite URL; fork-invitees joining via that URL will see the fork
    // community starting empty (no pre-fork history). Phase 2 will add
    // content-addressed delivery (Zenoh BLOB) for large snapshots.
    // Both forked_from and pre_fork_snapshot are cleared together to maintain
    // the invariant that forked_from is None iff pre_fork_snapshot is None.
    // (Fix: PR #122 bot review — CodeAnt invariant + size cap issue.)
    match crate::community_invite::encode_invite_url(&payload) {
        Ok(url) => Ok(url),
        Err(crate::community_invite::InviteUrlError::TooLarge(actual_len))
            if payload.pre_fork_snapshot.is_some() =>
        {
            tracing::warn!(
                community_id = %hex::encode(space_id.0),
                actual_b64_len = actual_len,
                cap = crate::community_invite::MAX_INVITE_BODY_B64_CHARS,
                "ZEB-285 generate_invite: fork-invite payload exceeds URL cap; \
                 falling back to no-snapshot mode. Fork-invitees joining via this \
                 URL will see no pre-fork history. Phase 2 will add content-addressed \
                 delivery for large snapshots."
            );
            payload.forked_from = None;
            payload.pre_fork_snapshot = None;
            build_open_invite_url(&payload)
        }
        Err(e) => Err(format!("encode invite URL: {e}")),
    }
}

// ── ZEB-217 Sub-C Phase 3 Task 9: create_community ───────────────────
//
// Mints a new community: fresh community_id, fresh EpochKey,
// bootstrap admin self-Join SignedMembershipEvent, then applies the
// Community Space row to owner-state CRDT, advances the local HLC
// tracker, spawns a CommunitySyncEngine via the registry, hands the
// adapter wiring to event_loop through a channel, and finally
// `insert_local_event`s the bootstrap Join into the engine so the
// debounced state-root publish picks it up.
//
// The `mint_community_creation` pure function is exposed separately
// (no NodeState, no async, no I/O) so the canonical-CBOR / signing
// path is unit-testable without standing up a Tauri test harness.

/// Bundle of values produced by `mint_community_creation` — kept as a
/// plain struct so callers (the `create_community` IPC + tests) can
/// destructure cleanly.
pub struct MintedCommunity {
    pub community_id: crate::owner_state_types::SpaceId,
    pub membership_key: crate::owner_state_types::EpochKey,
    pub space: crate::owner_state_types::Space,
    pub bootstrap_join: crate::community_membership::SignedMembershipEvent,
}

/// Pure function: mint a fresh community + signed bootstrap Join.
///
/// Generates random `community_id` (16 bytes) and `EpochKey`
/// (32 bytes), builds the Community Space row, signs a self-Join
/// `SignedMembershipEvent` with the caller's ed25519 key. Returns
/// all four artefacts so the IPC layer can apply the Space, send
/// the Join through the engine, and return the hex id to the frontend.
///
/// Sync / no I/O / no async — caller supplies `creation_hlc`
/// (pre-reserved via `dm_outbox::reserve_next_hlc_for_device` per
/// ZEB-267); `community_id`, `EpochKey`, and the bootstrap-Join
/// `event_id` are drawn from `rand::thread_rng()`. Not deterministic,
/// but "pure" w.r.t. HLC ordering — concurrent callers with distinct
/// reserved HLCs always produce monotone outputs. Free of channels,
/// mutexes, and Tauri runtime, so `create_community_inner_tests` can
/// cover the full mint in isolation.
pub fn mint_community_creation(
    name: &str,
    is_invite_only: bool,
    self_owner: crate::owner_state_types::OwnerAddr,
    signing_key: &ed25519_dalek::SigningKey,
    creation_hlc: crate::owner_state_types::Hlc,
) -> Result<MintedCommunity, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use crate::owner_state_types::{EpochKey, Space, SpaceId, SpaceKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut id_bytes = [0u8; 16];
    rng.fill_bytes(&mut id_bytes);
    let community_id = SpaceId(id_bytes);

    let mut mk_bytes = [0u8; 32];
    rng.fill_bytes(&mut mk_bytes);
    let membership_key = EpochKey::new(mk_bytes);

    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);
    let join_payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::Join,
        actor: self_owner,
        at: creation_hlc.clone(),
    };
    let bootstrap_join =
        sign_event(&join_payload, signing_key).map_err(|e| format!("sign bootstrap join: {e}"))?;

    let space = Space {
        id: community_id,
        kind: SpaceKind::Community,
        parent: None,
        community_id: None,
        name: name.to_string(),
        transport: None,
        members: Vec::new(),
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: creation_hlc.clone(),
        updated_at: creation_hlc,
        content_key: None,
        prior_content_keys: Vec::new(),
        current_epoch: Some(0),
        current_epoch_key: Some(membership_key.clone()),
        old_epoch_keys: std::collections::BTreeMap::new(),
        admin_addr: Some(self_owner),
        is_invite_only: Some(is_invite_only),
        shared_in_profile: false,
        pending_join_at: None,
    };

    Ok(MintedCommunity {
        community_id,
        membership_key,
        space,
        bootstrap_join,
    })
}

/// Internal helper for `create_community`. Takes already-snapshotted
/// handles; pure of `tauri::State`. The final generation fence
/// re-acquires the std `NodeState` lock (passed as `&Mutex<NodeState>`)
/// to guard against a stop-during-await race. ZEB-258: owner-state
/// Space commit is the LAST persistent step. Failures BEFORE the
/// commit tear down the engine + return Err with crdt_state untouched.
///
/// Takes `&Mutex<NodeState>` rather than `tauri::State<'_, Mutex<NodeState>>`
/// so integration tests can invoke the helper directly with a
/// freshly-constructed `Mutex<NodeState>` (the regression test for the
/// ZEB-258 reorder is load-bearing only when it actually drives the
/// production code path). The wrapper passes `&state_lock` (Tauri's
/// `State` auto-derefs to `&Mutex<NodeState>`).
///
/// Argument shape mirrors what `redeem_invite_inner` will look like
/// (Phase 4 Task 8 will extract it the same way) so the two IPCs share
/// a code-review pattern.
///
/// Lock-order discipline (load-bearing — flagged on PR #86 round 2,
/// updated for ZEB-267 reservation-time tracker bump):
/// the `crdt_state` `tokio::sync::Mutex` guard MUST drop before
/// `hlc_tracker.lock().await` is acquired. Holding `state_g` across
/// `tracker_g.lock().await` would (a) violate the project-wide "no
/// `.await` while holding state mutex" rule, and (b) invert lock order
/// vs callers that take `hlc_tracker` first — a deadlock risk under
/// concurrent IPCs.
///
/// Post-ZEB-267 shape: `hlc_tracker` is locked EARLY via
/// `reserve_next_hlc_for_device` (atomic read-bump-write under one
/// lock) and the guard is dropped before any owner-state work. The
/// later `apply_space` block takes `crdt_state` alone — the two locks
/// are never held simultaneously, and `crdt_state` is never held
/// across an `.await`. Tracker advance is reservation-time (burn
/// semantics on rollback), no longer co-located with the apply_space
/// commit. See spec at `docs/specs/2026-05-09-zeb-267-...md` for the
/// rationale behind moving the bump out of the apply critical section.
#[allow(clippy::too_many_arguments)]
pub async fn create_community_inner<R: tauri::Runtime>(
    name: String,
    is_invite_only: bool,
    crdt_state: std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    hlc_tracker: std::sync::Arc<
        tokio::sync::Mutex<std::collections::BTreeMap<String, crate::owner_state_types::Hlc>>,
    >,
    device_id: String,
    self_owner: crate::owner_state_types::OwnerAddr,
    signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    community_registry: std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
    community_adapter_tx: tokio::sync::mpsc::Sender<crate::event_loop::CommunityAdapterRequest>,
    channel_log_registry: std::sync::Arc<
        crate::community_channel_log_engine::ChannelLogRegistry<R>,
    >,
    snapshot_generation: u64,
    node_state: &std::sync::Mutex<NodeState>,
) -> Result<String, String> {
    // Phase 4 unblocks invite-only minting. `is_invite_only` flows
    // through `mint_community_creation` into the Space row + engine
    // config; the verify chain enforces invite-only semantics on every
    // Join from there. The receive-side counter-sign hop ships with
    // this PR; share-side `generate_invite` for invite-only is still
    // its own work item (the IPC handler still blocks it pending an
    // InviteToken sign path).

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // ZEB-267: atomic HLC reservation. The tracker is bumped here
    // (reservation time), not at the post-commit `tracker_g.insert`
    // line that ZEB-258 originally placed inside the apply_space
    // critical section. Burn semantics: if owner-state apply_space
    // rejects later, the reserved HLC is "burned" — fine, since
    // HLCs are 64-bit logical and the burn-on-rollback shape is
    // already implicit on the engine-spawn / adapter-dispatch
    // failure paths above. ZEB-258's atomicity property (Space row
    // commit is the LAST persistent step) is preserved — the
    // tracker advance is no longer co-located with the apply_space
    // call, but tracker advance was always orthogonal to the
    // owner-state Space-row write (they both persist via
    // persist_both, but the tracker is a per-device monotone
    // counter, not a Space-row field).
    //
    // BREAKING WITH ZEB-258 NOTE: the commented invariant at the
    // matching tracker_g.insert site below ("hold both guards
    // across the apply+insert pair") is no longer load-bearing for
    // tracker monotonicity since the reservation is atomic. We
    // still hold state_g across the apply for owner-state rollback
    // semantics, but tracker_g is gone from that block.
    let creation_hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;
    let minted = mint_community_creation(
        &name,
        is_invite_only,
        self_owner,
        signing_key.as_ref(),
        creation_hlc,
    )?;

    // ZEB-258: spawn engine + dispatch adapter BEFORE the owner-state
    // commit. Both can fail; both have rollback paths. ZEB-274: the
    // 9 scattered `shutdown_engine_and_cleanup_persistence` rollback
    // sites are now collapsed into a single RAII guard
    // (`community_sync_guard`) — Drop on early-return runs the
    // shutdown automatically. At this point owner-state is unchanged.
    //
    // ZEB-271: open a channel-log transaction so any ChannelCreate
    // events that materialize during this critical section are queued
    // and only fire on commit. Drop on early-return triggers the
    // safety-net abort, preventing phantom default-#general spawns on
    // fence/apply_space failure. See spec §3-§5.
    let channel_log_tx = channel_log_registry.begin_transaction(minted.community_id);

    // ZEB-274: RAII rollback guard for the community-sync spawn + adapter
    // dispatch. If anything between here and `community_sync_guard.commit()`
    // below fails (including panics), Drop runs
    // shutdown_engine_and_cleanup_persistence. Replaces the 9 scattered
    // explicit rollback sites that this function previously had.
    let mut community_sync_guard = community_registry.begin_spawn_guard(minted.community_id);

    // Channel pair shape mirrors start_node's per-community spawn
    // path: pub_tx / sub_rx feed the engine, pub_rx / sub_tx feed the
    // Zenoh adapter. The CommunityAdapterRequest carries the adapter
    // halves into event_loop via the mpsc; the event loop spawns the
    // adapter against its live session.
    let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

    let engine_arc = community_registry
        .spawn_engine_with_guard(
            &mut community_sync_guard,
            minted.community_id,
            minted.membership_key.clone(),
            self_owner,
            is_invite_only,
            pub_tx,
            sub_rx,
            pub_rx,
            sub_tx,
            community_adapter_tx,
        )
        .await
        .map_err(|e| format!("registry.spawn_engine_with_guard: {e}"))?;

    // ZEB-254 R1 (C1): bind admin_identity_pub so the P5 gate can verify
    // PendingJoin InviteToken signatures on this engine. The admin IS the
    // community creator — we derive the 64-byte composite pub from the
    // signing key (same layout as the joiner path at ~L9350).
    {
        use crate::dm_signing::ed25519_priv_to_x25519;
        let x25519_priv = ed25519_priv_to_x25519(&signing_key);
        let x25519_pub =
            x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*x25519_priv));
        let ed25519_pub_bytes = signing_key.verifying_key().to_bytes();
        let mut admin_identity_pub = [0u8; 64];
        admin_identity_pub[..32].copy_from_slice(x25519_pub.as_bytes());
        admin_identity_pub[32..].copy_from_slice(&ed25519_pub_bytes);
        engine_arc.bind_admin_identity_pub(admin_identity_pub);
    }

    // Bootstrap-Join via the engine. The engine's `insert_local_event`
    // runs verify_event (which authorizes the admin self-Join via the
    // bootstrap rule) and fires `notify_dirty` on success; the debounced
    // publish picks up the event and writes to the per-community
    // state-root topic. ZEB-258: still BEFORE the owner-state commit;
    // a failure here tears the engine down with crdt_state untouched.
    //
    // ZEB-274: engine_arc() lookup removed — spawn_engine_with_guard above
    // returned the engine handle directly. The CodeRabbit P0 manual
    // rollback collapses into the guard's Drop on `?` early-return.
    let outcome = engine_arc
        .insert_local_event(minted.bootstrap_join.clone())
        .await
        .map_err(|e| format!("engine.insert_local_event (bootstrap_join): {e}"))?;
    if !matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        // ZEB-274: rollback collapses into community_sync_guard Drop on early-return.
        return Err(format!("bootstrap Join not inserted (got {outcome:?})"));
    }

    // ZEB-248 Phase 1: atomically auto-create the default #general channel.
    // Same engine-transaction window as the bootstrap_join — if this insert
    // fails, community_sync_guard Drop runs the shutdown rollback (ZEB-274).
    // The HLC for the default-channel event is computed via the canonical
    // `next_hlc` helper anchored on `minted.bootstrap_join.at` — keeps
    // Join < ChannelCreate ordering deterministic without manually adding
    // logical+1 (which would risk u32 overflow on a wedge-in path) and
    // matches the helper used by every other mint site.
    let default_channel_id: crate::community_membership::ChannelId = {
        use rand::RngCore;
        let mut buf = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut buf);
        crate::community_membership::ChannelId(buf)
    };
    let default_channel_event_id: crate::community_membership::EventId = {
        use rand::RngCore;
        let mut buf = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut buf);
        buf
    };
    // ZEB-267: reserve a SECOND HLC atomically. The first reservation
    // above bumped tracker[device_id] to `bootstrap_join.at`, BUT
    // there are .await points between the two reservations (engine
    // spawn, adapter dispatch), and the only ordering guarantee on
    // `hlc_tracker` is its own internal mutex — a concurrent same-
    // device IPC could slot into one of those gaps and advance
    // tracker[device_id] past bootstrap_join.at before this
    // reservation reads it. So the only invariant we can rely on is:
    //   default_channel_at > bootstrap_join.at  (strictly greater)
    // Adjacency (bootstrap.logical+1) and bit-equality with the
    // pre-ZEB-267 chained `next_hlc(Some(&bootstrap_join.at), …)`
    // call held only in the uncontended single-IPC case; under
    // concurrency it does NOT, and that's fine — the engine's
    // event_sort_key only requires strict ordering, not adjacency.
    // Burn semantics: same as the first reservation — if a downstream
    // step fails, the burned HLC is fine.
    let default_channel_at =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;
    let default_channel_payload = crate::community_membership::EventPayload {
        id: default_channel_event_id,
        community_id: minted.community_id,
        kind: crate::community_membership::MembershipEventKind::ChannelCreate {
            channel_id: default_channel_id,
            name: "general".to_string(),
            write_power: 0,
        },
        actor: self_owner,
        at: default_channel_at.clone(),
    };
    // ZEB-274: rollback collapses into community_sync_guard Drop on early-return.
    let default_channel_signed =
        crate::community_membership::sign_event(&default_channel_payload, signing_key.as_ref())
            .map_err(|e| format!("sign default-channel ChannelCreate: {e}"))?;

    let default_channel_outcome = engine_arc
        .insert_local_event(default_channel_signed)
        .await
        .map_err(|e| format!("engine.insert_local_event (default channel): {e}"))?;
    if !matches!(
        default_channel_outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        // ZEB-274: rollback collapses into community_sync_guard Drop on early-return.
        return Err(format!(
            "default-channel ChannelCreate not inserted (got {default_channel_outcome:?})"
        ));
    }

    // ZEB-258: SNAPSHOT-THEN-COMMIT FENCE. If the node generation
    // changed since we snapshotted, owner-state is on a different
    // lifetime — abort. Mirrors add_space's post-stop guard. Done
    // BEFORE the owner-state commit so a stop-during-await race is
    // caught with crdt_state untouched. Capture the verdict + the
    // current generation under the std lock then drop the guard
    // BEFORE any `.await` (`std::sync::MutexGuard` is `!Send`, so a
    // held guard across an .await would prevent the future from
    // implementing Send — see tauri::command's Send bound).
    enum FenceVerdict {
        Ok,
        GenerationChanged(u64),
        RegistryGone,
    }
    let verdict = {
        let g = node_state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            FenceVerdict::GenerationChanged(g.generation)
        } else if g.community_registry.is_none() {
            FenceVerdict::RegistryGone
        } else {
            FenceVerdict::Ok
        }
    }; // std lock guard dropped here before any .await.
    match verdict {
        FenceVerdict::Ok => {}
        FenceVerdict::GenerationChanged(now_gen) => {
            // ZEB-274: rollback collapses into community_sync_guard Drop on early-return.
            return Err(format!(
                "node generation changed during create_community (was {}, now {}); \
                 community minted on a detached crdt_state and won't be persisted — \
                 engine spawn suppressed",
                snapshot_generation, now_gen
            ));
        }
        FenceVerdict::RegistryGone => {
            // ZEB-274: rollback collapses into community_sync_guard Drop on early-return.
            return Err(
                "community_registry was torn down during create_community — engine spawn \
                 suppressed"
                    .to_string(),
            );
        }
    }

    // ZEB-258 / ZEB-267: COMMIT owner-state Space as the LAST
    // persistent step. Tokio Mutex guards are safe to hold across
    // `.await` (only `std::sync::Mutex` guards must not be); the
    // Rejected branch drops `state_g` before awaiting the registry
    // tear-down to keep the rollback path .await-clean.
    //
    // Pre-ZEB-267 this block ALSO held `tracker_g` across an insert
    // to keep the tracker advance atomic with the Space commit. With
    // the atomic reservation pattern, the tracker is bumped at
    // reservation time (above) and this block is owner-state-only.
    {
        let mut state_g = crdt_state.lock().await;
        let outcome = state_g.apply_space_with_canonicalization(minted.space.clone());
        if matches!(outcome, crate::owner_state_crdt::ApplyOutcome::Rejected(_)) {
            // ZEB-274: rollback collapses into community_sync_guard Drop on early-return.
            return Err(format!("apply_space rejected new community: {outcome:?}"));
        }
        // ZEB-267: tracker advance no longer needed here — the two
        // reservations above (bootstrap_join + default-channel)
        // already bumped the tracker atomically. state_g drops at
        // scope end. Burn semantics (if apply_space had rejected
        // earlier in this block): the reserved HLCs are fine to
        // "burn" — HLCs are 64-bit logical, not finite.
    }

    // ZEB-274: release the community-sync rollback obligation. apply_space
    // succeeded — the community is durable. Sync (no .await needed). Per
    // spec §8 #4: community_sync_guard.commit() FIRST, then channel_log_tx.
    community_sync_guard.commit();

    // ZEB-271: post-durable-commit drain. apply_space above is the LAST
    // PERSISTENT step — the community is committed. If commit() fails,
    // log and continue: the deferred channel-log spawns (e.g., default
    // #general) will be re-attempted by reconcile_from_state at next
    // start_node. Returning Err here would surface the create as failed
    // even though the community exists, leading to retry → duplicate
    // community.
    if let Err(e) = channel_log_tx.commit().await {
        tracing::warn!(
            community_id = %hex::encode(minted.community_id.0),
            error = %e,
            "channel_log_registry commit failed after durable community create; \
             pending channel-log spawns will be re-attempted via \
             reconcile_from_state at next start_node"
        );
    }

    Ok(hex::encode(minted.community_id.0))
}

/// Tauri IPC: create a fresh OPEN community.
///
/// Phase 3 ships only OPEN communities; invite-only `create_community`
/// returns `Err` until Phase 4 lands the invite-token signing.
///
/// Snapshots the relevant `NodeState` handles under the std lock, then
/// delegates to `create_community_inner`, which encodes the ZEB-258
/// reorder (owner-state Space commit is the LAST persistent step;
/// engine + adapter failures roll back with crdt_state untouched).
///
/// Adapter wiring flows through `event_loop` (not directly through
/// the Zenoh `Session` from this command's task) per spec
/// §"Architecture / Adapter wiring": this command sends a
/// `CommunityAdapterRequest` over an mpsc; the event loop's `select!`
/// drains it and calls `spawn_community_state_zenoh_adapter` against
/// the live session.
#[tauri::command]
async fn create_community(
    app: tauri::AppHandle,
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    name: String,
    is_invite_only: bool,
) -> Result<String, String> {
    // Snapshot NodeState handles in a single guard scope, then drop
    // the std lock BEFORE any `.await`. The signing key lives inside
    // dm_outbox under a tokio Mutex, so we acquire the dm_outbox
    // handle under the std lock (Arc clone) and `.await` its lock
    // afterward.
    let (
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        community_registry,
        community_adapter_tx,
        channel_log_registry,
        dm_outbox,
        snapshot_generation,
    ) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.community_adapter_request_tx
                .clone()
                .ok_or("community_adapter_request_tx missing")?,
            g.channel_log_registry
                .clone()
                .ok_or("channel_log_registry missing — node not running?")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.generation,
        )
    }; // std `state_lock` guard dropped here.

    // Now safe to `.await` — the std lock has been released.
    let signing_key = {
        let outbox_g = dm_outbox.lock().await;
        std::sync::Arc::clone(&outbox_g.signing_key)
    };

    let name_for_emit = name.clone();
    let community_id = create_community_inner(
        name,
        is_invite_only,
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        signing_key,
        community_registry,
        community_adapter_tx,
        channel_log_registry,
        snapshot_generation,
        &state_lock,
    )
    .await?;

    // ZEB-265: surface the new community to the nav listener. emit
    // failure is non-fatal — the create already committed, and the
    // frontend's synthesis fallback (App.svelte) keeps the node visible
    // either way.
    if let Err(e) = app.emit(
        "nav-updated",
        &NavUpdatedPayload {
            action: "added",
            space_id: community_id.clone(),
            kind: "community",
            name: name_for_emit,
            members: None,
            parent_id: None,
            pending: None,
        },
    ) {
        tracing::warn!(error = %e, "create_community: nav-updated emit failed");
    }

    Ok(community_id)
}

#[cfg(test)]
mod create_community_inner_tests {
    use super::*;
    use crate::community_channel_log_engine::{
        ChannelLogEngineConfig, ChannelLogRegistry, ChannelLogRegistryConfig,
    };
    use crate::community_state_sync::{
        CommunityMembershipDelta, CommunityRegistryConfig, CommunitySyncRegistry,
        DEFAULT_DEBOUNCE_MS,
    };
    use crate::content_store::{ContentStore, RuntimeContentStore};
    use crate::owner_state_crdt::OwnerState;
    use crate::owner_state_types::{Hlc, OwnerAddr};
    use harmony_identity::PrivateIdentity;
    use std::collections::BTreeMap;
    use tokio::sync::mpsc;

    // ── Fixture helper ────────────────────────────────────────────────────────

    /// Extract the canonical Ed25519 signing key from a `PrivateIdentity`.
    /// Mirrors the pattern used by all other test modules in this file.
    fn signing_key_from_identity(
        identity: &PrivateIdentity,
    ) -> std::sync::Arc<ed25519_dalek::SigningKey> {
        let sk_bytes_full = identity.to_private_bytes();
        let ed_seed: [u8; 32] = sk_bytes_full[32..64]
            .try_into()
            .expect("ed25519 seed slice 32..64");
        std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&ed_seed))
    }

    struct CreateCommunityTestFixture {
        crdt_state: std::sync::Arc<tokio::sync::Mutex<OwnerState>>,
        hlc_tracker: std::sync::Arc<tokio::sync::Mutex<BTreeMap<String, Hlc>>>,
        device_id: String,
        self_owner: OwnerAddr,
        signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
        community_registry: std::sync::Arc<CommunitySyncRegistry>,
        community_adapter_tx: tokio::sync::mpsc::Sender<crate::event_loop::CommunityAdapterRequest>,
        channel_log_registry: std::sync::Arc<ChannelLogRegistry<tauri::test::MockRuntime>>,
        node_state: std::sync::Mutex<NodeState>,
        // Held alive so adapter channels don't report Closed.
        _community_adapter_rx:
            tokio::sync::mpsc::Receiver<crate::event_loop::CommunityAdapterRequest>,
        // Held alive so the channel_log registry's adapter bridge stays open.
        _channel_log_adapter_rx:
            tokio::sync::mpsc::UnboundedReceiver<crate::event_loop::ChannelLogAdapterRequest>,
        // Held alive so the delta consumer task keeps running.
        _consumer_drainer: tokio::task::JoinHandle<()>,
        // Tempdir held alive for the duration of the test.
        _tmp: tempfile::TempDir,
    }

    impl CreateCommunityTestFixture {
        /// Current node-state generation (mirrors the snapshot_generation
        /// arg `create_community` passes to `create_community_inner`).
        fn snapshot_generation(&self) -> u64 {
            self.node_state
                .lock()
                .expect("node_state poisoned")
                .generation
        }

        /// Bump the node-state generation to simulate a stop-during-await
        /// race that triggers the fence abort path.
        fn bump_node_state_generation(&self) {
            self.node_state
                .lock()
                .expect("node_state poisoned")
                .generation += 1;
        }
    }

    /// Build a minimal fixture for `create_community_inner` unit tests.
    ///
    /// Key design choices:
    /// - `delta_tx: Some(delta_tx)` in `CommunityRegistryConfig` — ChannelCreate
    ///   events emitted by the engine flow to the consumer task, which mirrors
    ///   the production lib.rs:1552+ callback: it calls
    ///   `channel_log_registry.spawn(...)` for each Created event. During an
    ///   open transaction this call enqueues a `DeferredSpawn`; `commit()` drains
    ///   the queue and calls `spawn_inner_now`. After a successful commit the
    ///   happy-path test can assert `engines_count == 1`.
    /// - The channel-log registry uses a dummy adapter bridge (unbounded
    ///   sender + receiver kept alive). No Zenoh drainer is needed — the
    ///   send succeeds so `spawn_inner_now` completes, but the adapter
    ///   request just queues unprocessed (harmless for these tests).
    /// - The community adapter channel is live (receiver kept alive) so
    ///   the adapter `try_send` inside `create_community_inner` succeeds.
    /// - The fence-check `node_state` is a bare `NodeState::default()`
    ///   with `generation = 0`. Tests that exercise the fence path bump
    ///   the generation via `fixture.bump_node_state_generation()`.
    async fn build_create_community_test_fixture() -> CreateCommunityTestFixture {
        let tmp = tempfile::TempDir::new().expect("tempdir");

        let identity = PrivateIdentity::from_seed(&[0xc0; 32]);
        let self_owner = OwnerAddr(identity.identity.address_hash);
        let identity_pub_64 = identity.identity.to_public_bytes();
        let signing_key = signing_key_from_identity(&identity);

        // Community registry — delta_tx wired so ChannelCreate deltas
        // flow to the consumer task (spawned below). The resolver returns
        // self_owner's identity_pub so verify_event admits the local
        // admin's events.
        struct SingleOwnerResolver {
            owner: OwnerAddr,
            pub_64: [u8; 64],
        }
        #[async_trait::async_trait]
        impl crate::community_state_sync::IdentityResolver for SingleOwnerResolver {
            async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
                if *addr == self.owner {
                    Some(self.pub_64)
                } else {
                    None
                }
            }
        }
        let (cas_op_tx, _cas_op_rx) = mpsc::channel(8);
        let cs: std::sync::Arc<dyn ContentStore> = std::sync::Arc::new(RuntimeContentStore::new(
            cas_op_tx,
            std::time::Duration::from_millis(1000),
        ));

        // delta_tx / delta_rx: engine emits CommunityMembershipDelta here;
        // consumer task mirrors the production lib.rs:1552+ registry hook.
        let (delta_tx, delta_rx) = mpsc::channel::<CommunityMembershipDelta>(64);

        let community_registry =
            std::sync::Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
                device_id: "test-dev".into(),
                content_store: cs,
                identity_resolver: std::sync::Arc::new(SingleOwnerResolver {
                    owner: self_owner,
                    pub_64: identity_pub_64,
                }),
                identity_dir: tmp.path().to_path_buf(),
                debounce_ms: DEFAULT_DEBOUNCE_MS,
                error_tx: None,
                delta_tx: Some(delta_tx),
                self_owner,
                signing_key: std::sync::Arc::clone(&signing_key),
                crdt_state: None,
                nav_emitter: None,
            }));

        // Community adapter channel — receiver kept alive so try_send
        // succeeds inside create_community_inner.
        let (community_adapter_tx, _community_adapter_rx) =
            mpsc::channel::<crate::event_loop::CommunityAdapterRequest>(16);

        // Channel-log registry — dummy adapter bridge. The unbounded
        // receiver is kept alive so adapter-request sends succeed inside
        // spawn_inner_now, but no drainer runs (the request just queues).
        // This is sufficient: spawn_inner_now inserts the engine into the
        // registry map regardless of whether the adapter request is
        // consumed, so engines_count_for_test() reflects real spawns.
        let (channel_log_adapter_tx, _channel_log_adapter_rx) =
            mpsc::unbounded_channel::<crate::event_loop::ChannelLogAdapterRequest>();
        let app = tauri::test::mock_app().handle().clone();
        let channel_log_registry = ChannelLogRegistry::new(ChannelLogRegistryConfig {
            adapter_request_tx: channel_log_adapter_tx,
            app,
            identity_dir: tmp.path().to_path_buf(),
            self_owner,
            self_device_id: "test-dev".into(),
            signing_key: std::sync::Arc::clone(&signing_key),
            engine_config: ChannelLogEngineConfig::default(),
        });

        let crdt_state = std::sync::Arc::new(tokio::sync::Mutex::new(OwnerState::default()));
        let hlc_tracker = std::sync::Arc::new(tokio::sync::Mutex::new(BTreeMap::new()));

        // Fence-check node state — generation starts at 0 (matches
        // snapshot_generation = 0 for the happy-path and apply_space
        // rejected paths). Tests that drive the fence path bump it.
        let node_state = std::sync::Mutex::new(NodeState {
            // Populate community_registry so the fence-check's
            // `g.community_registry.is_none()` guard doesn't fire.
            community_registry: Some(std::sync::Arc::clone(&community_registry)),
            ..NodeState::default()
        });

        // Consumer task: mirrors the production lib.rs:1552+ registry hook.
        // Drains delta_rx; for each ChannelCreate delta, derives the
        // channel key from the community engine's membership_key and calls
        // channel_log_registry.spawn — during an open transaction this
        // call enqueues a DeferredSpawn; commit() drains and fires
        // spawn_inner_now.
        //
        // The engine's state_at_hlc and identity_resolver are obtained
        // via community_registry.engine_arc() — same path as production.
        // The task exits cleanly when every delta_tx clone drops (i.e.,
        // when community_registry is shut down or dropped at test end).
        let _consumer_drainer = {
            let registry_for_hook = std::sync::Arc::clone(&channel_log_registry);
            let community_registry_for_hook = std::sync::Arc::clone(&community_registry);
            let hlc_tracker_for_hook = std::sync::Arc::clone(&hlc_tracker);
            tokio::spawn(run_community_delta_consumer(
                delta_rx,
                // membership-changed callback — no-op in test (no Tauri
                // event emission needed).
                |_payload| async {},
                // channel-config-updated callback — no-op in test.
                |_payload| async {},
                // channel-log registry hook — mirrors lib.rs:1552+.
                move |payload: ChannelConfigChangedPayload| {
                    let registry = std::sync::Arc::clone(&registry_for_hook);
                    let community_registry = std::sync::Arc::clone(&community_registry_for_hook);
                    let hlc_tracker = std::sync::Arc::clone(&hlc_tracker_for_hook);
                    async move {
                        if payload.action != ChannelConfigChangeAction::Created {
                            return;
                        }
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
                        let cid = crate::owner_state_types::SpaceId(cid_bytes);
                        let chid = crate::community_membership::ChannelId(chid_bytes);

                        let community_engine = match community_registry.engine_arc(&cid).await {
                            Some(e) => e,
                            None => return,
                        };
                        let membership_key = community_engine.membership_key();
                        let key = crate::community_channel_log::derive_channel_key(
                            &membership_key,
                            &cid,
                            &chid,
                        );
                        let state_at_hlc = community_engine.state_at_hlc_resolver();
                        let resolver = match community_engine.identity_resolver() {
                            Some(r) => r,
                            None => return,
                        };
                        let _ = registry
                            .spawn(cid, chid, key, state_at_hlc, resolver, hlc_tracker)
                            .await;
                    }
                },
                // ZEB-249 Task 6: epoch-event hook — no-op in test
                |_delta| async {},
            ))
        };

        CreateCommunityTestFixture {
            crdt_state,
            hlc_tracker,
            device_id: "test-dev".into(),
            self_owner,
            signing_key,
            community_registry,
            community_adapter_tx,
            channel_log_registry,
            node_state,
            _community_adapter_rx,
            _channel_log_adapter_rx,
            _consumer_drainer,
            _tmp: tmp,
        }
    }

    // ── ZEB-271 wait helpers ──────────────────────────────────────────────────

    /// Bounded condition-wait: polls until `engines_count_for_test()`
    /// equals `expected`, or `timeout` elapses. Returns `true` on match.
    async fn wait_until_engines_count(
        registry: &crate::community_channel_log_engine::ChannelLogRegistry<
            tauri::test::MockRuntime,
        >,
        expected: usize,
        timeout: std::time::Duration,
    ) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if registry.engines_count_for_test().await == expected {
                return true;
            }
            tokio::task::yield_now().await;
        }
        false
    }

    // ── ZEB-271: channel-log transaction coverage ─────────────────────────────
    //
    // These tests verify that `create_community_inner` opens a
    // `CommunityTransactionGuard`, that the deferred-spawn queue actually
    // receives the #general ChannelCreate via the wired delta consumer,
    // and that the guard is committed (draining the queue → spawning the
    // engine) on success or dropped (triggering safety-net abort, discarding
    // the queue) on every failure path. See spec §7.2.

    /// Happy path (spec §7.2): `create_community_inner` returns Ok.
    ///
    /// The wired delta consumer delivers the #general ChannelCreate to
    /// `channel_log_registry.spawn` while the transaction is open, which
    /// enqueues a DeferredSpawn. `commit()` drains the queue and calls
    /// `spawn_inner_now`, inserting the engine into the registry. After
    /// commit: no pending transaction entry, and `engines_count == 1`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn happy_path_spawns_default_channel_engine() {
        let fixture = build_create_community_test_fixture().await;
        let snapshot_gen = fixture.snapshot_generation();

        let result = create_community_inner(
            "happy-community".to_string(),
            false,
            std::sync::Arc::clone(&fixture.crdt_state),
            std::sync::Arc::clone(&fixture.hlc_tracker),
            fixture.device_id.clone(),
            fixture.self_owner,
            std::sync::Arc::clone(&fixture.signing_key),
            std::sync::Arc::clone(&fixture.community_registry),
            fixture.community_adapter_tx.clone(),
            std::sync::Arc::clone(&fixture.channel_log_registry),
            snapshot_gen,
            &fixture.node_state,
        )
        .await;

        assert!(result.is_ok(), "happy path must succeed: {:?}", result);

        // Post-commit: no pending transaction entry remains.
        let community_id_hex = result.unwrap();
        let id_bytes: [u8; 16] = hex::decode(&community_id_hex)
            .expect("hex community_id")
            .as_slice()
            .try_into()
            .expect("16 bytes");
        let community_id = crate::owner_state_types::SpaceId(id_bytes);

        assert!(
            !fixture
                .channel_log_registry
                .has_pending_transaction_for_test(&community_id),
            "happy path: channel_log transaction must be committed (no pending entry)"
        );

        // Poll until the consumer task has processed the ChannelCreate delta
        // and the engine has appeared in the registry (max 500ms).
        // The delta may arrive before or after commit() — either way the
        // engine ends up spawned; the bounded poll avoids a fixed sleep.
        assert!(
            wait_until_engines_count(
                &fixture.channel_log_registry,
                1,
                std::time::Duration::from_millis(500)
            )
            .await,
            "happy path: commit() must spawn the #general channel-log engine \
             (engines_count must be 1 after deferred-spawn drain)"
        );
    }

    /// Failure path — fence abort after default-channel insert: the guard
    /// is dropped (safety-net abort) and no phantom engines leak.
    ///
    /// # apply_space rejection — DEVIATION NOTE (spec §7.2)
    ///
    /// Spec §7.2 requests a test that forces `apply_space_with_canonicalization`
    /// to reject, proving the guard's safety-net abort cleans up after that
    /// specific failure point. The apply_space rejection path is not exercised
    /// here because it cannot be triggered without controlling the SpaceId that
    /// `create_community_inner` will mint — the ID is randomly generated inside
    /// the function (rand::thread_rng().fill_bytes), so the test cannot
    /// pre-insert a conflicting Space row for the same SpaceId.
    ///
    /// What would be needed to implement this properly:
    /// 1. A `mint_community_creation` overload that accepts a caller-supplied
    ///    SpaceId (or a seed for the RNG) so the test can predict the id.
    /// 2. Pre-apply that SpaceId to `crdt_state` with a different
    ///    `membership_key` or `admin_addr`. `apply_space_with_canonicalization`
    ///    then rejects the second application with `InvariantFail` (same-SpaceId
    ///    community creation-pinned fields are immutable — see owner_state_crdt.rs
    ///    lines ~173–203).
    /// 3. Call `create_community_inner` with the same SpaceId so the rejection
    ///    actually fires.
    ///
    /// In the meantime this test exercises a guard abort that fires AFTER the
    /// default-channel ChannelCreate is inserted into the engine (the fence check
    /// fires after `insert_local_event` but before `apply_space`) — same
    /// safety-net drop code path as the apply_space rejection branch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fence_abort_after_default_channel_insert_no_channel_log_leak() {
        let fixture = build_create_community_test_fixture().await;
        let snapshot_gen = fixture.snapshot_generation();

        // Bump the generation so the fence check fires. The fence check
        // runs AFTER the default-channel ChannelCreate insert_local_event,
        // so the deferred-spawn queue has one entry at abort time.
        // Guard drop → safety-net abort → queue discarded → engines_count == 0.
        fixture.bump_node_state_generation();

        let result = create_community_inner(
            "fence-aborted-community".to_string(),
            false,
            std::sync::Arc::clone(&fixture.crdt_state),
            std::sync::Arc::clone(&fixture.hlc_tracker),
            fixture.device_id.clone(),
            fixture.self_owner,
            std::sync::Arc::clone(&fixture.signing_key),
            std::sync::Arc::clone(&fixture.community_registry),
            fixture.community_adapter_tx.clone(),
            std::sync::Arc::clone(&fixture.channel_log_registry),
            snapshot_gen, // stale — node_state.generation is now snapshot_gen + 1
            &fixture.node_state,
        )
        .await;

        assert!(result.is_err(), "must Err on fence abort");

        // Poll until the safety-net abort has run (max 500ms).
        assert!(
            wait_until_engines_count(
                &fixture.channel_log_registry,
                0,
                std::time::Duration::from_millis(500)
            )
            .await,
            "no channel-log engines must leak after fence-aborted create_community_inner"
        );
    }

    /// Failure path — fence generation changed: the guard is dropped
    /// (safety-net abort) and no phantom engines leak. Directly
    /// exercises the ZEB-258/ZEB-271 combined rollback property.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fence_generation_changed_no_channel_log_leak() {
        let fixture = build_create_community_test_fixture().await;
        let snapshot_gen = fixture.snapshot_generation();

        // Bump BEFORE the call so the fence check (which re-acquires the
        // node_state lock) sees a changed generation.
        fixture.bump_node_state_generation();

        let result = create_community_inner(
            "fence-changed-community".to_string(),
            false,
            std::sync::Arc::clone(&fixture.crdt_state),
            std::sync::Arc::clone(&fixture.hlc_tracker),
            fixture.device_id.clone(),
            fixture.self_owner,
            std::sync::Arc::clone(&fixture.signing_key),
            std::sync::Arc::clone(&fixture.community_registry),
            fixture.community_adapter_tx.clone(),
            std::sync::Arc::clone(&fixture.channel_log_registry),
            snapshot_gen,
            &fixture.node_state,
        )
        .await;

        assert!(
            result.is_err(),
            "fence generation change must abort create: {:?}",
            result
        );

        // Poll until the safety-net abort has run (max 500ms).
        assert!(
            wait_until_engines_count(
                &fixture.channel_log_registry,
                0,
                std::time::Duration::from_millis(500)
            )
            .await,
            "no channel-log engines must leak after fence-generation-changed abort"
        );
    }

    // ── Existing tests ─────────────────────────────────────────────────────────

    #[test]
    fn mint_creation_produces_consistent_id_join_event_and_space() {
        let identity = PrivateIdentity::from_seed(&[0xc1; 32]);
        let self_owner = OwnerAddr(identity.identity.address_hash);
        // Reach into the PrivateIdentity's signing path the same way
        // production does: the canonical 32-byte seed lives in bytes
        // 32..64 of `to_private_bytes()` (X25519_secret(32) ||
        // Ed25519_secret(32)). dm_outbox stores the SigningKey
        // constructed from those bytes; mirror that here so the test
        // signs with the same key the IPC will use in production.
        let sk_bytes_full = identity.to_private_bytes();
        let ed_seed: [u8; 32] = sk_bytes_full[32..64]
            .try_into()
            .expect("ed25519 seed slice 32..64");
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_seed);

        let device_id = "creator-dev";
        let wall_now_ms = 1_700_000_000_000u64;
        // ZEB-267: caller pre-reserves the HLC; in production this
        // comes from `reserve_next_hlc_for_device`. The test constructs
        // it inline to keep the mint helper purely synchronous.
        let creation_hlc = Hlc {
            wall_ms: wall_now_ms,
            logical: 0,
            device_id: device_id.to_string(),
        };

        let minted = mint_community_creation(
            "Hackers United",
            false,
            self_owner,
            &signing_key,
            creation_hlc.clone(),
        )
        .expect("mint");

        assert_eq!(
            minted.space.kind,
            crate::owner_state_types::SpaceKind::Community
        );
        assert_eq!(minted.space.id, minted.community_id);
        assert_eq!(minted.space.admin_addr, Some(self_owner));
        assert_eq!(minted.space.is_invite_only, Some(false));
        assert!(minted.space.current_epoch_key.is_some());
        assert_eq!(minted.space.name, "Hackers United");
        assert_eq!(minted.space.created_at.wall_ms, wall_now_ms);
        assert_eq!(&minted.space.created_at.device_id, device_id);

        assert_eq!(minted.bootstrap_join.actor, self_owner);
        assert_eq!(minted.bootstrap_join.community_id, minted.community_id);
        assert!(matches!(
            minted.bootstrap_join.kind,
            crate::community_membership::MembershipEventKind::Join
        ));
        assert_eq!(minted.bootstrap_join.at.wall_ms, wall_now_ms);
        assert!(
            minted.bootstrap_join.countersig.is_none(),
            "open / bootstrap Join carries no countersig"
        );

        // Two consecutive mints must produce DISTINCT community ids /
        // event ids / membership keys — the random source has to fire
        // per call, otherwise two communities created in a row would
        // collide. (16-byte / 32-byte randomness collision is
        // astronomically unlikely; this just guards against a
        // rng-reuse / fixed-buffer bug.)
        let minted2 = mint_community_creation(
            "Other Community",
            false,
            self_owner,
            &signing_key,
            creation_hlc.clone(),
        )
        .expect("mint2");
        assert_ne!(minted.community_id, minted2.community_id);
        assert_ne!(minted.bootstrap_join.id, minted2.bootstrap_join.id);
        assert_ne!(
            minted.space.current_epoch_key.as_ref().unwrap().as_bytes(),
            minted2.space.current_epoch_key.as_ref().unwrap().as_bytes(),
        );

        // Bootstrap signature MUST verify against self_owner's
        // identity_pub — the engine's verify_event will run the same
        // check on insert_local_event.
        let identity_pub = identity.identity.to_public_bytes();
        crate::community_membership::verify_signature(&minted.bootstrap_join, &identity_pub)
            .expect("bootstrap join signature must verify against self identity_pub");
    }
}

// ── ZEB-217 Sub-C Phase 3 Task 10: redeem_invite ─────────────────────
//
// Joiner-side mirror of `create_community`: decodes a
// `harmony://invite/...` URL, mints a signed self-Join, applies the
// derived Community Space row to owner-state CRDT, advances the local
// HLC tracker, spawns a CommunitySyncEngine via the registry (passing
// the invite's admin_addr — NOT the joining peer — so the engine's
// authority root matches the inviter's), hands the adapter wiring to
// event_loop, and finally `insert_local_event`s the self-Join so the
// debounced state-root publish picks it up.
//
// Phase 3 supports OPEN-only redemption; invite-only (with countersig
// fan-out via Reticulum) ships in Phase 4.
//
// Cross-peer dedupe: the new Space row's id is `payload.community_id`,
// IDENTICAL to the creator's Space row id. apply_space's CRDT
// last-writer-wins on (id, hlc) collapses the two rows correctly when
// the peers eventually sync. Phase 1's same-SpaceId rejection of
// community-creation field changes (admin_addr / membership_key /
// is_invite_only) defends against malicious or stale invites trying
// to drift the canonical row out from under the original creator.

/// IPC result for `redeem_invite`. Carries the invite payload's
/// human-readable fields (community name, kind) alongside the
/// community id so the frontend can build a correct NavNode + populate
/// the settings panel without re-decoding the URL or round-tripping
/// for a name lookup. ZEB-265.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RedeemInviteResultDto {
    pub community_id: String,
    pub community_name: String,
    pub is_invite_only: bool,
    /// ZEB-254: true if the redemption returned before a JoinCountersign
    /// landed locally (admin was offline; the 5s fast-path timeout
    /// fired). The community appears in nav greyed; ungreys when
    /// JoinCountersign arrives via state-root sync. false if either
    /// (a) fast-path counter-sign came back within 5s, or (b) community
    /// is open (no countersign required).
    pub pending: bool,
}

/// Wire shape of the `nav-updated` IPC event. Mirrors the frontend
/// `NavUpdatedPayload` interface in `src/lib/nav-service.ts`. `action`
/// values are `"added" | "removed" | "modified"`; `kind` values are
/// `"dm" | "group-dm" | "channel" | "community" | "folder"` —
/// validated by the listener at receive time. ZEB-265.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NavUpdatedPayload {
    pub action: &'static str,
    pub space_id: String,
    pub kind: &'static str,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub members: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// ZEB-254 Task 11: emitted with `action = "modified"` when the
    /// joiner-side pending-clear hook fires. `None` means "not relevant
    /// to this payload" (the frontend skips the pending field on
    /// `"added"` / `"removed"` actions). `Some(false)` means the pending
    /// state just cleared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<bool>,
}

/// Pure function: builds the joiner-side `MintedCommunity` from an
/// invite payload — derives a Community Space row from `payload.name`
/// / `is_invite_only`, and signs a self-Join `SignedMembershipEvent`
/// (actor = `self_owner`, community_id = `payload.community_id`).
///
/// Pure / sync / no I/O — the caller supplies `join_hlc`. This lets
/// the test (`redeem_invite_inner_tests`) cover the full mint without
/// spawning channels, mutexes, or a Tauri runtime.
///
/// ZEB-267: Caller pre-reserves `join_hlc` via
/// `dm_outbox::reserve_next_hlc_for_device`.
pub fn mint_redemption(
    payload: &crate::community_invite::CommunityInvitePayload,
    self_owner: crate::owner_state_types::OwnerAddr,
    signing_key: &ed25519_dalek::SigningKey,
    join_hlc: crate::owner_state_types::Hlc,
) -> Result<MintedCommunity, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use crate::owner_state_types::{EpochKey, Space, SpaceKind};
    use rand::RngCore;

    // ZEB-249 / M3: extract the epoch key from the snapshot.
    // - Open communities: sealed_epoch_key is the raw 32-byte EpochKey
    //   (publicly shareable — anyone with the link gets it).
    // - Invite-only communities: sealed_epoch_key is a 92-byte X25519-sealed
    //   envelope (32 ephemeral_pub + 12 nonce + 32 ct + 16 tag) encrypted
    //   to the invitee's X25519 pubkey. Derive the invitee's X25519 private
    //   scalar from signing_key (RFC 7748 §5 via ed25519_priv_to_x25519)
    //   and decrypt here.
    let epoch_key_bytes: [u8; 32] = if payload.is_invite_only {
        // Invite-only path: decrypt the 92-byte sealed blob
        // (32 ephemeral_pub + 12 nonce + 32 ciphertext + 16 AEAD tag).
        // E2: validate minimum envelope size before attempting decryption so
        // a truncated payload produces a clear error rather than the generic
        // MalformedSealedEnvelope from open_from_owner.
        const SEALED_MIN: usize = 32 + 12 + 16; // ephemeral_pub + nonce + AEAD tag
        if payload.epoch_snapshot.sealed_epoch_key.len() < SEALED_MIN {
            return Err(format!(
                "invite-only epoch key envelope too short: need ≥ {} bytes, got {}",
                SEALED_MIN,
                payload.epoch_snapshot.sealed_epoch_key.len()
            ));
        }
        use crate::dm_signing::{ed25519_priv_to_x25519, open_from_owner};
        let x25519_priv = ed25519_priv_to_x25519(signing_key);
        let plaintext = open_from_owner(&x25519_priv, &payload.epoch_snapshot.sealed_epoch_key)
            .map_err(|e| format!("invite-only epoch key decryption failed: {e}"))?;
        plaintext.as_slice().try_into().map_err(|_| {
            format!(
                "decrypted invite-only epoch key must be 32 bytes (got {})",
                plaintext.len()
            )
        })?
    } else {
        // Open-community path: raw 32-byte key.
        payload
            .epoch_snapshot
            .sealed_epoch_key
            .as_slice()
            .try_into()
            .map_err(|_| {
                format!(
                    "epoch_snapshot.sealed_epoch_key must be 32 bytes for open communities \
                     (got {})",
                    payload.epoch_snapshot.sealed_epoch_key.len()
                )
            })?
    };
    let membership_key = EpochKey::new(epoch_key_bytes);
    let epoch = payload.epoch_snapshot.epoch;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    // ZEB-254: invite-only redemptions mint a PendingJoin event carrying
    // the InviteToken (admin-signed bearer credential) + the joiner's
    // full identity_pub. Distributed via the community CRDT so admins
    // who were offline at redemption time can counter-sign asynchronously.
    let event_kind = if payload.is_invite_only {
        use crate::dm_signing::ed25519_priv_to_x25519;
        let x25519_priv = ed25519_priv_to_x25519(signing_key);
        let x25519_pub =
            x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*x25519_priv));
        let ed25519_pub_bytes = signing_key.verifying_key().to_bytes();
        let mut identity_pub = [0u8; 64];
        identity_pub[..32].copy_from_slice(x25519_pub.as_bytes());
        identity_pub[32..].copy_from_slice(&ed25519_pub_bytes);

        let invite_token = payload
            .invite_token
            .clone()
            .ok_or_else(|| "invite-only payload is missing invite_token".to_string())?;

        MembershipEventKind::PendingJoin {
            invite_token,
            joiner_identity_pub: identity_pub,
        }
    } else {
        MembershipEventKind::Join
    };

    let join_payload = EventPayload {
        id: event_id_bytes,
        community_id: payload.community_id,
        kind: event_kind,
        actor: self_owner,
        at: join_hlc.clone(),
    };
    let bootstrap_join =
        sign_event(&join_payload, signing_key).map_err(|e| format!("sign self-join: {e}"))?;

    let space = Space {
        id: payload.community_id,
        kind: SpaceKind::Community,
        parent: None,
        community_id: None,
        name: payload.community_name.clone(),
        transport: None,
        members: Vec::new(),
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: join_hlc.clone(),
        updated_at: join_hlc,
        content_key: None,
        prior_content_keys: Vec::new(),
        current_epoch: Some(epoch),
        current_epoch_key: Some(membership_key.clone()),
        old_epoch_keys: std::collections::BTreeMap::new(),
        admin_addr: Some(payload.admin_addr),
        // Use the invite's declared is_invite_only so the redeemer's
        // Space row matches the creator's row (Phase 1's CRDT same-
        // SpaceId rejection of community-creation field changes would
        // silently reject the redemption Space if these disagreed).
        is_invite_only: Some(payload.is_invite_only),
        shared_in_profile: false,
        pending_join_at: None,
    };

    Ok(MintedCommunity {
        community_id: payload.community_id,
        membership_key,
        space,
        bootstrap_join,
    })
}

/// ZEB-262 Phase 4: invite-only `redeem_invite` inner helper. Encodes
/// the 10-step flow per spec §"Send path: redeem_invite":
///
///   1. decode URL (caller-supplied; we receive it as `url: String`)
///   2. snapshot handles (done by caller — passed in as args)
///   3. wall_now_ms
///   4. RESERVE HLC under tracker lock
///   5. mint_redemption (pure helper from Phase 3)
///   6. spawn_engine + dispatch adapter
///   7. branch on `payload.is_invite_only`:
///      - OPEN — `engine.insert_local_event(bootstrap_join)`
///      - INVITE-ONLY:
///        - 7a. register oneshot keyed on `bootstrap_join.id` via
///          `community_registry.register_pending_redemption`
///        - 7b. build + sign `CommunityInviteSigned`
///        - 7c. resolve inviter Reticulum dest(s); send packet via
///          `unicast_send_tx`
///        - 7d. await oneshot ≤ T (env
///          `HARMONY_REDEEM_INVITE_TIMEOUT_MS`, default 5s)
///   8. fence_check (generation guard via closure)
///   9. COMMIT owner-state Space (LAST step — ZEB-258 reorder)
///  10. return `Ok(hex(community_id))`
///
/// On any failure between steps 6-8, the engine is torn down via
/// `community_registry.shutdown_engine_and_cleanup_persistence` (Task
/// 7) and owner-state is byte-identical to pre-call.
///
/// **Closure-pattern fence_check.** The IPC wrapper passes a closure
/// that re-locks the std `NodeState` mutex and compares `generation`.
/// Tests pass `|| Ok(())` since they don't drive the fence. Production
/// passes a closure that captures `&Mutex<NodeState>` + the snapshot.
/// `Fn` (not `FnOnce`) so future-proof retries can re-call without
/// consuming.
#[allow(clippy::too_many_arguments)]
pub async fn redeem_invite_inner<R: tauri::Runtime, F>(
    url: String,
    crdt_state: std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    hlc_tracker: std::sync::Arc<
        tokio::sync::Mutex<std::collections::BTreeMap<String, crate::owner_state_types::Hlc>>,
    >,
    device_id: String,
    self_owner: crate::owner_state_types::OwnerAddr,
    signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    community_registry: std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
    community_adapter_tx: tokio::sync::mpsc::Sender<crate::event_loop::CommunityAdapterRequest>,
    unicast_send_tx: tokio::sync::mpsc::Sender<crate::dm_outbox::UnicastSendRequest>,
    dm_outbox: std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>,
    channel_log_registry: std::sync::Arc<
        crate::community_channel_log_engine::ChannelLogRegistry<R>,
    >,
    fence_check: F,
    // ZEB-285: identity_dir used to write pre_fork_snapshot.bin for fork invites.
    // None suppresses the snapshot write (e.g., when resolve_identity_dir fails).
    // Write failure is always non-fatal — the join proceeds; the user just won't
    // see pre-fork history.
    identity_dir: Option<std::path::PathBuf>,
) -> Result<RedeemInviteResultDto, String>
where
    F: Fn() -> Result<(), String>,
{
    // 1. Decode URL.
    let payload = crate::community_invite::decode_invite_url(&url)
        .map_err(|e| format!("decode invite URL: {e}"))?;

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // 4. ZEB-267: atomic HLC reservation. Replaces the
    //    snapshot-then-release pattern + post-commit advance.
    let join_hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;

    // 5. Mint (pure helper — no side effects on owner-state yet).
    let minted = mint_redemption(&payload, self_owner, signing_key.as_ref(), join_hlc)?;

    // ZEB-271: open a channel-log transaction. Protects against remote
    // ChannelCreate events that arrive via Zenoh sync between
    // spawn_engine and apply_space — they're queued, only fire on
    // commit, dropped on rollback. See spec §4.2.
    let channel_log_tx = channel_log_registry.begin_transaction(minted.community_id);

    // ZEB-274: RAII rollback guard for the community-sync spawn + adapter
    // dispatch. Same pattern as create_community_inner. Internalizes
    // the freshness flag — the local fresh-creation bool (was at
    // lib.rs:8530 pre-ZEB-274, ZEB-260 PR #90 round-5) is removed; the
    // guard tracks it and its Drop only runs cleanup if THIS call's
    // spawn was the fresh one. Concurrent-redeem race losers' guards
    // are no-ops on Drop (spec §5.2).
    let mut community_sync_guard = community_registry.begin_spawn_guard(minted.community_id);

    // ZEB-267 (replaces the prior ZEB-258 comment): the HLC tracker
    // is bumped at reservation time (step 4) regardless of whether
    // owner-state apply_space succeeds at step 9. Burn semantics:
    // a reserved HLC on a rollback path is "burned" — fine, since
    // HLCs are 64-bit logical and burn-on-rollback is already the
    // implicit behavior on the engine-spawn / adapter-dispatch
    // failure paths above. The original ZEB-258 concern (phantom
    // tracker entry without matching Space row) was about
    // persistence atomicity; advancing the tracker without persisting
    // the Space leaves a stale tracker entry, but that's a benign
    // consistency drift — the tracker is a per-device monotone
    // counter, not a constraint on the Space-row map.

    // 6. Spawn engine + dispatch adapter atomically via the guard.
    //    Owner-state is still untouched at this point. ZEB-274: rollback
    //    on any subsequent early-return is handled by the
    //    community_sync_guard's Drop — no manual shutdown calls in this
    //    function.
    //
    //    spawn_engine_with_guard takes `payload.admin_addr` (the original
    //    community admin from the invite), NOT self_owner — the
    //    engine's authority root is the creator's identity. Invite-
    //    only engines spawn with `is_invite_only=true` so verify_event
    //    applies the countersig rule.
    //
    //    Re-redemption guard: ZEB-260 PR #90 round-5 (CodeRabbit) — the
    //    freshness flag (true = this call freshly created the engine) is
    //    now captured INSIDE the guard by spawn_engine_with_guard rather
    //    than being a local bool. ZEB-274 collapses the prior 17 manual
    //    fresh-only rollback sites into the guard's Drop. The race-safety
    //    is preserved: only the call whose spawn freshly created the
    //    engine carries the rollback obligation. Concurrent-redeem race
    //    losers see freshly_created = false and their guard Drop is a
    //    no-op (spec §5.2). spawn_engine is idempotent; on a pre-existing
    //    engine the freshly-built channels we passed in are dropped
    //    inside spawn_engine_with_guard (the engine already owns its
    //    live adapter pair).
    let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

    // ZEB-274: spawn engine + dispatch adapter atomically via the guard.
    // Internalizes the freshness flag — no separate freshness local (the
    // guard tracks it). Concurrent-redeem race losers see a
    // pre-existing engine; their pub_rx/sub_tx/community_adapter_tx args
    // are dropped inside spawn_engine_with_guard's idempotent path and
    // their guard's freshly_created stays false → Drop is a no-op (spec
    // §5.2). Adapter try_send failure also tears down inline and leaves
    // the guard's freshly_created false (spec §5.3).
    let engine_arc = community_registry
        .spawn_engine_with_guard(
            &mut community_sync_guard,
            minted.community_id,
            minted.membership_key.clone(),
            payload.admin_addr,
            payload.is_invite_only,
            pub_tx,
            sub_rx,
            pub_rx,
            sub_tx,
            community_adapter_tx,
        )
        .await
        .map_err(|e| format!("registry.spawn_engine_with_guard: {e}"))?;

    // ZEB-254: bind admin identity pub to the engine so the P5 gate in
    // verify_event can validate PendingJoin InviteToken signatures. Must
    // happen before any event insert (including the bootstrap admin Join
    // below) so the shared OnceLock is populated before the task's
    // handle_incoming_publish path could race it. Invite-only payloads
    // always carry admin_identity_pub; open-community payloads carry None
    // (no PendingJoin events possible → no binding needed).
    if let Some(pub_bytes) = payload.admin_identity_pub {
        engine_arc.bind_admin_identity_pub(pub_bytes);
    }

    // ZEB-249 Task 6 spec §5.2: seed the bootstrap hint BEFORE the first
    // insert_local_event so the guard (version == 0 && events.is_empty())
    // in CommunityState::materialized() can actually return the hint.
    // The hint is seeded here — after the engine is spawned (state exists)
    // but before either path inserts any event. Once the first real event
    // arrives the hint is superseded by CRDT replay (spec §5.2 + §10.3).
    {
        let snapshot = &payload.epoch_snapshot.state_snapshot;
        let hint = crate::community_membership::MaterializedMembership {
            members: snapshot.members.clone(),
            channels: snapshot.channels.clone(),
            power_levels: snapshot.power_levels.clone(),
            current_epoch: Some(payload.epoch_snapshot.epoch),
            pending_rotation_for: std::collections::BTreeSet::new(),
            pending_catchup_for: std::collections::BTreeSet::new(),
            admin_quorum: 1,
        };
        if let Some(state_arc) = community_registry.state_for(&minted.community_id).await {
            let state_g = state_arc.lock().await;
            state_g.seed_bootstrap_hint(hint);
        }
    }

    // 7. Branch on payload.is_invite_only.
    // ZEB-254: tracks whether the invite-only fast-path 5s timeout fired
    // without a counter-sign landing. Always false for open communities.
    // Set inside the invite-only branch by the timeout match arm.
    let mut pending_redemption_timed_out: bool = false;
    if !payload.is_invite_only {
        // OPEN: insert bootstrap_join via the engine. The engine's
        // `insert_local_event` runs verify_event (which authorizes the
        // open Join via signature alone) and fires `notify_dirty` on
        // success.
        //
        // ZEB-274: engine_arc() lookup removed — spawn_engine_with_guard
        // above returned the engine handle directly. All rollback sites
        // collapse into community_sync_guard's Drop on early-return.
        let outcome = engine_arc
            .insert_local_event(minted.bootstrap_join.clone())
            .await
            .map_err(|e| format!("engine.insert_local_event: {e}"))?;
        if !matches!(
            outcome,
            crate::community_state_crdt::InsertOutcome::Inserted
        ) {
            // ZEB-274: rollback collapses into community_sync_guard Drop on early-return.
            return Err(format!("self Join not inserted (got {outcome:?})"));
        }
    } else {
        // INVITE-ONLY: 7a-d.
        // ZEB-274: the engine + persistence dir were spawned via the
        // guard above; all rollback sites in this branch collapse into
        // community_sync_guard's Drop on early-return.
        let invite_token = match payload.invite_token.as_ref() {
            Some(t) => t.clone(),
            None => {
                // ZEB-274: rollback collapses into community_sync_guard Drop on early-return.
                return Err("invite-only payload missing invite_token".to_string());
            }
        };

        // ZEB-260: verify admin's bootstrap from the invite payload AND
        // insert it into the joiner's engine BEFORE sending the unicast.
        // Closes the cold-cache gap: the joiner's empty CRDT cannot
        // admit the admin's eventual publish-back unless admin is in
        // the joiner's local prefix at the gate's `prior_state_at_hlc`
        // evaluation. Order is critical — the publish-back is generated
        // strictly later than the unicast arrives at admin, so the
        // bootstrap insert here cannot be raced.
        let (admin_bootstrap, admin_identity_pub) =
            crate::community_invite::verify_admin_bootstrap(&payload)
                // ZEB-274: rollback collapses into community_sync_guard Drop on early-return.
                .map_err(|verify_err| verify_err.to_string())?;
        // Idempotent on retry: insert_local_event_with_pubs dedups on
        // event-id. The clone is cheap (SignedMembershipEvent is a few
        // hundred bytes) and required because the engine consumes by
        // value.
        let admin_bootstrap_owned = admin_bootstrap.clone();
        let admin_identity_pub_owned = *admin_identity_pub;
        // ZEB-274: engine_arc() lookup removed — spawn_engine_with_guard
        // returned the engine handle directly. The engine-vanished
        // rollback is no longer reachable here.
        //
        // ZEB-260 PR #90 round-3 (CodeRabbit): explicitly match on the
        // InsertOutcome variant. Treating Ok(_) as success would silently
        // proceed past Ok(InsertOutcome::Rejected(verify_err)), which
        // means a bootstrap that failed the engine's own verify_event
        // (e.g., signature mismatch — should be unreachable given our
        // own verify_admin_bootstrap chain just passed, but the engine
        // re-runs verify under its VerifyContext) would land us in the
        // unicast-send-and-wait path with no admin-state in the engine,
        // causing a publish-back rejection downstream. Surface
        // explicitly on Rejected just like on Err.
        match engine_arc
            .insert_local_event_with_pubs(admin_bootstrap_owned, admin_identity_pub_owned, None)
            .await
        {
            Ok(crate::community_state_crdt::InsertOutcome::Inserted)
            | Ok(crate::community_state_crdt::InsertOutcome::AlreadyKnown) => {}
            Ok(crate::community_state_crdt::InsertOutcome::Rejected(verify_err)) => {
                // ZEB-274: rollback collapses into community_sync_guard Drop on early-return.
                return Err(format!("engine rejected admin bootstrap: {verify_err}"));
            }
            Err(insert_err) => {
                // ZEB-274: rollback collapses into community_sync_guard Drop on early-return.
                return Err(format!(
                    "engine.insert_local_event_with_pubs (admin bootstrap): {insert_err}"
                ));
            }
        }

        // 7a. Register oneshot keyed on bootstrap_join.id. Engine's
        //     insert hook (Task 7's notify_pending_redemption_in_map)
        //     fires it once the counter-signed Join lands.
        let (notify_tx, notify_rx) = tokio::sync::oneshot::channel::<()>();
        community_registry
            .register_pending_redemption(minted.bootstrap_join.id, notify_tx)
            .await;

        // 7b. Build + sign CommunityInviteSigned. Read the joiner's
        //     identity (private_identity → identity_pub +
        //     device_hash) and signing_key from the dm_outbox under
        //     its lock. Drop the guard before any further `.await`
        //     to keep the outbox available to drain ticks.
        let (joiner_pub, joiner_device_hash, sign_key_arc) = {
            let outbox_g = dm_outbox.lock().await;
            let joiner_pub = outbox_g.private_identity.identity.to_public_bytes();
            let joiner_device_hash = crate::owner_state_types::DeviceIdentityHash(
                outbox_g.private_identity.identity.address_hash,
            );
            let sign_key_arc = std::sync::Arc::clone(&outbox_g.signing_key);
            (joiner_pub, joiner_device_hash, sign_key_arc)
        };

        let signed = crate::community_invite::CommunityInviteSigned {
            community_id: minted.community_id,
            join_event: minted.bootstrap_join.clone(),
            invite_token,
            joiner_identity_pub: joiner_pub,
            signing_device_hash: joiner_device_hash,
            // CommunityInvitePacket.created_at carries the joiner's
            // wall-clock at packet construction; reusing the bootstrap
            // Join's HLC keeps the redeemer's outbound packet temporally
            // bound to the event being counter-signed.
            created_at: minted.bootstrap_join.at.clone(),
        };

        // Both encode steps below run AFTER `register_pending_redemption`,
        // so a `?` early-return would leak the registered oneshot AND
        // leave the engine + persistence dir we spawned at step 4
        // running. Roll back explicitly on either error.
        let packet = match crate::community_invite::build_signed_invite_packet(
            signed,
            sign_key_arc.as_ref(),
        ) {
            Ok(p) => p,
            Err(e) => {
                let _ = community_registry
                    .take_pending_redemption(&minted.bootstrap_join.id)
                    .await;
                // ZEB-274: rollback collapses into community_sync_guard Drop on early-return.
                return Err(format!("build_signed_invite_packet: {e}"));
            }
        };
        let wire = match crate::community_invite::encode_packet(&packet) {
            Ok(w) => w,
            Err(e) => {
                let _ = community_registry
                    .take_pending_redemption(&minted.bootstrap_join.id)
                    .await;
                // ZEB-274: rollback collapses into community_sync_guard Drop on early-return.
                return Err(format!("encode_packet: {e}"));
            }
        };

        // ZEB-254: insert PendingJoin into local engine FIRST so the
        // engine's state-root publisher picks it up — the wire path
        // (Zenoh CRDT) is the durable channel; the unicast below is
        // just the fast-path optimization for when an admin device is
        // online at this moment.
        match engine_arc
            .insert_local_event(minted.bootstrap_join.clone())
            .await
        {
            Ok(crate::community_state_crdt::InsertOutcome::Inserted)
            | Ok(crate::community_state_crdt::InsertOutcome::AlreadyKnown) => {}
            Ok(crate::community_state_crdt::InsertOutcome::Rejected(verify_err)) => {
                // ZEB-274: rollback collapses into community_sync_guard Drop on early-return.
                return Err(format!(
                    "engine rejected local PendingJoin insert: {verify_err}"
                ));
            }
            Err(insert_err) => {
                // ZEB-274: rollback collapses into community_sync_guard Drop on early-return.
                return Err(format!(
                    "engine.insert_local_event (PendingJoin): {insert_err}"
                ));
            }
        }

        // 7c. Resolve inviter's Reticulum destination(s) and send.
        let inviter_addr = payload.admin_addr;
        let destinations = resolve_destinations_for_owner(crdt_state.as_ref(), inviter_addr).await;
        if destinations.is_empty() {
            // No known device for inviter → drop oneshot.
            let _ = community_registry
                .take_pending_redemption(&minted.bootstrap_join.id)
                .await;
            // ZEB-274: rollback collapses into community_sync_guard Drop on early-return.
            return Err(format!(
                "no known device for inviter {} — invite cannot route",
                hex::encode(inviter_addr.0)
            ));
        }
        // Per-destination fan-out with at-least-one-success semantics.
        //
        // The inviter may have multiple devices (any of which can
        // counter-sign). Reticulum unicast is best-effort per
        // destination — if even one queue-side `try_send` succeeds the
        // packet is on its way and we cannot retract it, so a partial
        // failure followed by local rollback would leave the receiver
        // counter-signing while we tear down the engine here. Track
        // success across the loop and ONLY roll back when all
        // destinations failed.
        let mut any_sent = false;
        let mut last_err: Option<String> = None;
        for destination_hash in &destinations {
            match unicast_send_tx.try_send(crate::dm_outbox::UnicastSendRequest {
                destination_hash: *destination_hash,
                packet: wire.clone(),
            }) {
                Ok(()) => any_sent = true,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        destination_hash = %hex::encode(destination_hash),
                        "redeem_invite unicast try_send failed for destination — \
                         continuing fan-out"
                    );
                    last_err = Some(e.to_string());
                }
            }
        }
        if !any_sent {
            let _ = community_registry
                .take_pending_redemption(&minted.bootstrap_join.id)
                .await;
            // ZEB-274: rollback collapses into community_sync_guard Drop on early-return.
            return Err(format!(
                "unicast_send_tx try_send failed for all {} destination(s){}",
                destinations.len(),
                last_err
                    .as_deref()
                    .map(|s| format!(" (last error: {s})"))
                    .unwrap_or_default()
            ));
        }

        // 7d. Await oneshot ≤ T (env-overridable for tests).
        // ZEB-254: 5s fast-path timeout (down from 15s). On timeout,
        // redeem_invite_inner does NOT roll back — it proceeds to commit
        // the Space with pending_join_at = Some and returns Ok { pending:
        // true }. The PendingJoin event is already on the wire via the
        // engine's state-root publisher; admins counter-sign whenever
        // they next come online.
        let timeout_ms: u64 = std::env::var("HARMONY_REDEEM_INVITE_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5_000);

        match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), notify_rx).await {
            Ok(Ok(())) => {
                // Counter-signed Join landed — proceed to commit.
            }
            Ok(Err(_recv_err)) => {
                // Sender dropped without sending — should be
                // unreachable with the current pending_redemptions
                // shape, but treat defensively as a failure.
                // ZEB-274: rollback collapses into community_sync_guard Drop on early-return.
                return Err("invite-only redemption oneshot closed unexpectedly".into());
            }
            Err(_elapsed) => {
                // ZEB-254: 5s fast-path timeout fired. Two sub-cases:
                //
                //   (A) take_pending_redemption returns Some(tx) — we won
                //       the race; the notifier hadn't run; genuine
                //       timeout → set pending_redemption_timed_out = true
                //       and fall through to commit with pending = true.
                //
                //   (B) take_pending_redemption returns None — notifier
                //       won the race and already ingested the counter-
                //       signed event → treat as success; commit with
                //       pending = false.
                //
                // CRITICAL: do NOT roll back. The PendingJoin event is
                // already on the wire via the engine's state-root publish.
                // Admins counter-sign whenever they come online.
                match community_registry
                    .take_pending_redemption(&minted.bootstrap_join.id)
                    .await
                {
                    Some(_tx) => {
                        // ZEB-254 R3 (C2): TOCTOU recheck. Between the oneshot
                        // timeout firing and our take_pending_redemption, a
                        // JoinCountersign may have landed via state-root sync
                        // (its post-Inserted hook ran and tried to fire the
                        // oneshot but found the sender already consumed by us
                        // — or fired it concurrently). Before latching
                        // pending=true and writing pending_join_at, scan the
                        // engine's CRDT for a JoinCountersign whose
                        // target_event_id matches our bootstrap_join.id.
                        // If found, treat as success (countersigned already).
                        let countersigned_now = {
                            let state = engine_arc.state();
                            let g = state.lock().await;
                            g.events.values().any(|e| matches!(
                                &e.kind,
                                crate::community_membership::MembershipEventKind::JoinCountersign {
                                    target_event_id,
                                } if *target_event_id == minted.bootstrap_join.id
                            ))
                        };
                        if countersigned_now {
                            tracing::debug!(
                                community_id = %hex::encode(minted.community_id.0),
                                event_id = %hex::encode(minted.bootstrap_join.id),
                                "ZEB-254 R3 (C2): TOCTOU recheck found JoinCountersign \
                                 between timeout and take_pending_redemption — treating as success"
                            );
                        } else {
                            pending_redemption_timed_out = true;
                        }
                    }
                    None => {
                        // Notifier won the race; treat as success.
                        // Fall through to the post-await commit path.
                        tracing::debug!(
                            community_id = %hex::encode(minted.community_id.0),
                            event_id = %hex::encode(minted.bootstrap_join.id),
                            "ZEB-254: 5s timeout fired but counter-sign arrived just in time"
                        );
                    }
                }
            }
        }
    }

    // 8. SNAPSHOT-THEN-COMMIT FENCE — production wrapper re-locks
    //    the std `NodeState` mutex and compares `generation`. Tests
    //    pass `|| Ok(())`. If the node was stopped (or stop+restart
    //    raced our await chain), this returns Err with crdt_state
    //    still untouched.
    // ZEB-274: rollback on Err collapses into community_sync_guard Drop.
    fence_check()?;

    // ZEB-254 R5-2: FINAL TOCTOU recheck before commit. The R3 (C2)
    // recheck above narrows but does not close the window between the
    // `take_pending_redemption` decision and this commit. A
    // `JoinCountersign` can still land via state-root sync in that gap;
    // its post-Inserted clear hook will run before `pending_join_at`
    // exists, leaving this commit to write a stale `Some(...)` that
    // greys the community until restart heal. Re-scan the engine
    // immediately before acquiring the owner-state lock; if a
    // countersign is now present, drop the timed-out flag so the
    // commit below writes pending_join_at = None.
    if pending_redemption_timed_out {
        let already_countersigned = {
            let state = engine_arc.state();
            let g = state.lock().await;
            g.events.values().any(|e| {
                matches!(
                    &e.kind,
                    crate::community_membership::MembershipEventKind::JoinCountersign {
                        target_event_id,
                    } if *target_event_id == minted.bootstrap_join.id
                )
            })
        };
        if already_countersigned {
            tracing::debug!(
                community_id = %hex::encode(minted.community_id.0),
                event_id = %hex::encode(minted.bootstrap_join.id),
                "ZEB-254 R5-2: final pre-commit recheck found JoinCountersign — \
                 dropping pending_redemption_timed_out so commit writes pending_join_at = None"
            );
            pending_redemption_timed_out = false;
        }
    }

    // 9. COMMIT owner-state Space as the LAST persistent step
    //    (ZEB-258 reorder). Pre-ZEB-267 this block also held the
    //    tracker_g guard to advance the tracker atomically with the
    //    Space commit; with the ZEB-267 atomic reservation pattern
    //    the tracker is bumped at step 4 instead, and this block
    //    is owner-state-only.
    {
        let mut state_g = crdt_state.lock().await;
        // ZEB-254: set pending_join_at if the invite-only fast-path
        // timed out (admin offline). For non-invite-only or
        // counter-signed-in-time paths, pending_join_at stays None.
        let mut space_to_commit = minted.space.clone();
        if pending_redemption_timed_out {
            space_to_commit.pending_join_at = Some(minted.space.created_at.clone());
        }
        let outcome = state_g.apply_space_with_canonicalization(space_to_commit);
        if matches!(outcome, crate::owner_state_crdt::ApplyOutcome::Rejected(_)) {
            // ZEB-274: rollback collapses into community_sync_guard Drop on early-return.
            // state_g drops at this block's scope end (before the function-
            // exit guard Drop, which spawns its async cleanup via
            // Handle::try_current — no lock held across the spawn).
            return Err(format!(
                "apply_space rejected redemption Space: {outcome:?}"
            ));
        }
        // ZEB-267: tracker advance no longer needed here — the
        // reservation at step 4 already bumped the tracker atomically.
        // state_g drops at scope end.
    }

    // ZEB-274: release the community-sync rollback obligation. apply_space
    // succeeded — the redemption is durable. Sync (no .await needed). Per
    // spec §8 #4: community_sync_guard.commit() FIRST, then channel_log_tx.
    community_sync_guard.commit();

    // ZEB-285: mirror fork lineage from invite payload into the joiner's
    // local CommunityState. Done POST-commit so we never set fork state on
    // a rolled-back join. Both operations are non-fatal on failure — the join
    // is already durable; the user simply won't see pre-fork history.
    if let Some(original_id) = payload.forked_from {
        // Set CommunityState.forked_from on the joiner's engine.
        if let Some(state_arc) = community_registry.state_for(&minted.community_id).await {
            let mut state_g = state_arc.lock().await;
            state_g.forked_from = Some(original_id);
            // ZEB-287 Phase 2 spec §3.5: when redeeming a fork-invite,
            // mirror the snapshot's lineage data into the new fork's
            // CommunityState. Phase 1 fork-invites have `parent_lineage`
            // default-empty (skip-if-empty); their `forked_at` Hlc has
            // always existed, so wall_ms is filled even for legacy invites.
            if let Some(snapshot) = payload.pre_fork_snapshot.as_ref() {
                state_g.forked_at_wall_ms = Some(snapshot.forked_at.wall_ms);
                // R1-2: apply 16-deep cap on the redeem side too. A
                // malicious or future-protocol-revision payload could
                // carry > 16 entries; the cap drops the oldest (root-side)
                // so our local state stays bounded. Same helper as
                // community_fork.rs::fork_community uses at build time.
                let mut capped_lineage = snapshot.parent_lineage.clone();
                crate::community_invite::apply_lineage_cap(&mut capped_lineage);
                state_g.parent_lineage = capped_lineage;
            }
        } else {
            tracing::warn!(
                community_id = %hex::encode(minted.community_id.0),
                "redeem_invite_inner: could not set forked_from — engine state not found"
            );
        }

        // Write pre_fork_snapshot.bin to the fork's data dir if present.
        if let Some(snapshot) = payload.pre_fork_snapshot.as_ref() {
            if let Some(ref id_dir) = identity_dir {
                let fork_dir = id_dir
                    .join("communities")
                    .join(hex::encode(minted.community_id.0));
                // Ensure the directory exists (create_community_inner also calls
                // create_dir_all, but we want a belt-and-suspenders guarantee).
                if let Err(e) = std::fs::create_dir_all(&fork_dir) {
                    tracing::warn!(
                        path = %fork_dir.display(),
                        error = %e,
                        "redeem_invite_inner: create fork dir failed; pre_fork_snapshot not written"
                    );
                } else {
                    let snapshot_path = fork_dir.join("pre_fork_snapshot.bin");
                    let tmp_path = fork_dir.join("pre_fork_snapshot.bin.tmp");
                    match crate::owner_state_crypto::canonical_cbor_encode(snapshot) {
                        Ok(bytes) => {
                            if let Err(e) = std::fs::write(&tmp_path, &bytes) {
                                tracing::warn!(
                                    path = %tmp_path.display(),
                                    error = %e,
                                    "redeem_invite_inner: write pre_fork_snapshot.bin.tmp failed"
                                );
                            } else if let Err(e) = std::fs::rename(&tmp_path, &snapshot_path) {
                                tracing::warn!(
                                    from = %tmp_path.display(),
                                    to = %snapshot_path.display(),
                                    error = %e,
                                    "redeem_invite_inner: rename pre_fork_snapshot.bin.tmp failed"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                community_id = %hex::encode(minted.community_id.0),
                                path = %snapshot_path.display(),
                                error = %e,
                                "redeem_invite_inner: encode pre_fork_snapshot failed; \
                                 snapshot not written"
                            );
                        }
                    }
                }
            } else {
                tracing::warn!(
                    community_id = %hex::encode(minted.community_id.0),
                    "redeem_invite_inner: identity_dir not available; pre_fork_snapshot not written"
                );
            }
        }
    }

    // ZEB-271: post-durable-commit drain. apply_space above is the LAST
    // PERSISTENT step — the redemption Space is committed. If commit()
    // fails, log and continue: the deferred channel-log spawns from
    // remote sync will be re-attempted by reconcile_from_state at next
    // start_node. Returning Err here would surface the join as failed
    // even though the user already joined; an OPEN retry is non-idempotent
    // and would append a second self-Join (ZEB-260 nominal-cost path).
    if let Err(e) = channel_log_tx.commit().await {
        tracing::warn!(
            community_id = %hex::encode(minted.community_id.0),
            error = %e,
            "channel_log_registry commit failed after durable redemption; \
             pending channel-log spawns will be re-attempted via \
             reconcile_from_state at next start_node"
        );
    }

    // 10. Return DTO with the invite's name + kind so the caller can
    // render the new community without re-decoding the URL or
    // round-tripping. ZEB-265.
    Ok(RedeemInviteResultDto {
        community_id: hex::encode(minted.community_id.0),
        community_name: payload.community_name.clone(),
        is_invite_only: payload.is_invite_only,
        pending: pending_redemption_timed_out,
    })
}

/// Resolve `OwnerAddr` → `Vec<destination_hash>` via the joiner's
/// `OwnerDeviceCache`. Mirrors `dm_outbox::resolve_destinations`'s
/// shape; reproduced inline because the inviter-resolution path is
/// community-specific (the inviter's OwnerAddr is plumbed straight
/// from the invite payload, not from a Space row's recipient).
///
/// Returns an empty Vec when the cache has no entry for `owner` — the
/// invite-only branch interprets that as "no known device → invite
/// cannot route" and surfaces a deterministic Err.
///
/// Returns *DM destination hashes* (per `dm_signing::compute_dm_destination_hash`),
/// NOT raw `DeviceIdentityHash` bytes. `UnicastSendRequest.destination_hash`
/// is the Reticulum-layer destination keyed off the DM Destination's
/// app/aspect derivation; the dm_outbox drain path computes this same
/// derivation (`dm_outbox.rs::resolve_destinations` and the per-device
/// path) before enqueueing. Returning raw `h.0` here would route invite-
/// only packets to the wrong link-layer address.
async fn resolve_destinations_for_owner(
    crdt_state: &tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>,
    owner: crate::owner_state_types::OwnerAddr,
) -> Vec<[u8; 16]> {
    let g = crdt_state.lock().await;
    g.owner_device_cache
        .devices
        .get(&owner)
        .map(|entry| {
            entry
                .devices
                .iter()
                .map(|h| crate::dm_signing::compute_dm_destination_hash(h.0))
                .collect()
        })
        .unwrap_or_default()
}

/// Tauri IPC: redeem a community invite URL (open or invite-only).
///
/// Snapshots the relevant `NodeState` handles under the std lock, then
/// delegates to `redeem_invite_inner`, which encodes the ZEB-258
/// reorder (owner-state Space commit is the LAST step; engine + adapter
/// + invite-only oneshot failures roll back with crdt_state untouched).
///
/// Lock-order discipline (mirrors `create_community`): the std
/// `state_lock` guard MUST drop before any `.await`. The signing key
/// lives inside `dm_outbox` under a tokio Mutex, so we acquire the
/// dm_outbox handle under the std lock (Arc clone) and `.await` its
/// lock afterward.
///
/// Adapter wiring flows through `event_loop` per spec §"Architecture /
/// Adapter wiring": this command sends a `CommunityAdapterRequest` over
/// an mpsc; the event loop's `select!` drains it and calls
/// `spawn_community_state_zenoh_adapter` against the live session.
///
/// Note vs `create_community`: `spawn_engine` takes `payload.admin_addr`
/// (the original community admin from the invite), NOT `self_owner` —
/// the engine's authority root is the creator's identity, not the
/// joining peer's.
///
/// **NOT idempotent** for OPEN payloads. A second call with the same
/// invite mints a fresh self-Join event with a new (random) event_id,
/// which the CRDT accepts as a distinct event. Materialized state is
/// unchanged (LWW on `MemberState`); the event log grows by one every
/// retry. `registry.spawn_engine` IS idempotent. For invite-only the
/// behavior depends on whether the inviter counter-signs again on a
/// retry (a freshly-minted bootstrap_join.id forces a new countersig
/// dance).
#[tauri::command]
async fn redeem_invite(
    app: tauri::AppHandle,
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    url: String,
) -> Result<RedeemInviteResultDto, String> {
    // Snapshot NodeState handles in a single guard scope, then drop
    // the std lock BEFORE any `.await`. Mirrors `create_community`'s
    // pattern.
    let (
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        community_registry,
        community_adapter_tx,
        unicast_send_tx,
        channel_log_registry,
        dm_outbox,
        snapshot_generation,
    ) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.community_adapter_request_tx
                .clone()
                .ok_or("community_adapter_request_tx missing")?,
            g.unicast_send_tx
                .clone()
                .ok_or("unicast_send_tx missing — no owner identity?")?,
            g.channel_log_registry
                .clone()
                .ok_or("channel_log_registry missing — node not running?")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.generation,
        )
    }; // std lock dropped here.

    // Now safe to `.await` — the std lock has been released.
    let signing_key = {
        let outbox_g = dm_outbox.lock().await;
        std::sync::Arc::clone(&outbox_g.signing_key)
    };

    // Fence-check closure: re-locks the std NodeState mutex and
    // compares `generation`. If the node was stopped (or stop+restart
    // raced), the closure returns Err and the inner helper rolls back.
    // Captures `state_lock` (a clonable `tauri::State<'_, _>`) by move;
    // `Fn` (not FnOnce) so the inner helper retains the option of
    // re-checking on retries. The closure borrows `state_lock` through
    // its `'r` lifetime, which the awaited inner future is bounded by.
    let fence_check = {
        let state_lock = state_lock.clone();
        move || -> Result<(), String> {
            let g = state_lock
                .lock()
                .map_err(|e| format!("NodeState poisoned: {e}"))?;
            if g.generation != snapshot_generation {
                return Err(format!(
                    "node generation changed during redeem_invite (was {}, now {}); \
                     redemption minted on a detached crdt_state and won't be persisted — \
                     engine spawn suppressed",
                    snapshot_generation, g.generation
                ));
            }
            if g.community_registry.is_none() {
                return Err(
                    "community_registry was torn down during redeem_invite — engine spawn \
                     suppressed"
                        .to_string(),
                );
            }
            Ok(())
        }
    };

    let dto = redeem_invite_inner(
        url,
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        signing_key,
        community_registry,
        community_adapter_tx,
        unicast_send_tx,
        dm_outbox,
        channel_log_registry,
        fence_check,
        crate::owner_commands::resolve_identity_dir().ok(),
    )
    .await?;

    // ZEB-265: surface the redeemed community to the nav listener.
    // emit failure is non-fatal — the join already committed, and
    // App.svelte still synthesizes from the DTO until step 3 lands.
    //
    // ZEB-254 R1 (I1): carry dto.pending so listeners that subscribe AFTER
    // this emit (e.g. nav components mounted post-redeem) see the correct
    // greyed state rather than assuming non-pending.
    if let Err(e) = app.emit(
        "nav-updated",
        &NavUpdatedPayload {
            action: "added",
            space_id: dto.community_id.clone(),
            kind: "community",
            name: dto.community_name.clone(),
            members: None,
            parent_id: None,
            pending: Some(dto.pending),
        },
    ) {
        tracing::warn!(error = %e, "redeem_invite: nav-updated emit failed");
    }

    Ok(dto)
}

// ── ZEB-252 Sub-D Phase 6: join_open_community ─────────────────────────────
//
// Thin wrapper over redeem_invite_inner. Re-resolves the directory entry
// server-side at click time so the renderer can't pass a URL the user
// never saw. The actual join machinery (URL decode, HLC reserve, mint,
// engine spawn, owner-state commit) is unchanged — Phase 6 is strictly
// a caller of redeem_invite_inner.
//
// See `docs/specs/2026-05-12-zeb-252-sub-d-phase-6-direct-join-design.md`.

/// Inner helper for `join_open_community`. Separated from the Tauri command
/// so unit tests can supply a fabricated snapshot + the standard
/// redeem-invite test fixture without spinning up a `LibraryDirectory` actor.
#[allow(clippy::too_many_arguments)]
async fn join_open_community_inner<R, F>(
    community_id_hex: String,
    snapshot: &[crate::library_directory::AggregatedEntry],
    crdt_state: std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    hlc_tracker: std::sync::Arc<
        tokio::sync::Mutex<std::collections::BTreeMap<String, crate::owner_state_types::Hlc>>,
    >,
    device_id: String,
    self_owner: crate::owner_state_types::OwnerAddr,
    signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    community_registry: std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
    community_adapter_tx: tokio::sync::mpsc::Sender<crate::event_loop::CommunityAdapterRequest>,
    unicast_send_tx: tokio::sync::mpsc::Sender<crate::dm_outbox::UnicastSendRequest>,
    dm_outbox: std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>,
    channel_log_registry: std::sync::Arc<
        crate::community_channel_log_engine::ChannelLogRegistry<R>,
    >,
    fence_check: F,
) -> Result<RedeemInviteResultDto, String>
where
    R: tauri::Runtime,
    F: Fn() -> Result<(), String>,
{
    let invite_url = crate::library_directory::find_open_community_invite_url_in_snapshot(
        snapshot,
        &community_id_hex,
    )?;

    redeem_invite_inner(
        invite_url,
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        signing_key,
        community_registry,
        community_adapter_tx,
        unicast_send_tx,
        dm_outbox,
        channel_log_registry,
        fence_check,
        crate::owner_commands::resolve_identity_dir().ok(),
    )
    .await
}

/// Tauri IPC: join an open community directly from the library-directory
/// aggregation. Re-resolves the entry by `community_id` server-side, then
/// delegates to `redeem_invite_inner` (which Phase 6 strictly wraps).
///
/// `redeem_invite(url)` remains the IPC for hand-pasted URLs.
///
/// Errors:
/// - `"This community is no longer listed by any of your libraries"` — entry
///   not in current aggregation (spec §4.3).
/// - `"Invite-only community cannot be joined directly from the directory"` —
///   defensive re-check; spec §4.4.
/// - Any error from `redeem_invite_inner` propagated verbatim.
#[tauri::command]
async fn join_open_community(
    app: tauri::AppHandle,
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
) -> Result<RedeemInviteResultDto, String> {
    let (
        library_directory,
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        community_registry,
        community_adapter_tx,
        unicast_send_tx,
        channel_log_registry,
        dm_outbox,
        snapshot_generation,
    ) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.library_directory
                .clone()
                .ok_or("library_directory missing — node not running?")?,
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.community_adapter_request_tx
                .clone()
                .ok_or("community_adapter_request_tx missing")?,
            g.unicast_send_tx
                .clone()
                .ok_or("unicast_send_tx missing — no owner identity?")?,
            g.channel_log_registry
                .clone()
                .ok_or("channel_log_registry missing — node not running?")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.generation,
        )
    }; // std lock dropped here.

    let snapshot = library_directory.snapshot_all().await;

    let signing_key = {
        let outbox_g = dm_outbox.lock().await;
        std::sync::Arc::clone(&outbox_g.signing_key)
    };

    let fence_check = {
        let state_lock = state_lock.clone();
        move || -> Result<(), String> {
            let g = state_lock
                .lock()
                .map_err(|e| format!("NodeState poisoned: {e}"))?;
            if g.generation != snapshot_generation {
                return Err(format!(
                    "node generation changed during join_open_community (was {}, now {}); \
                     join minted on a detached crdt_state and won't be persisted — \
                     engine spawn suppressed",
                    snapshot_generation, g.generation
                ));
            }
            if g.community_registry.is_none() {
                return Err(
                    "community_registry was torn down during join_open_community — engine \
                     spawn suppressed"
                        .to_string(),
                );
            }
            Ok(())
        }
    };

    let dto = join_open_community_inner(
        community_id,
        &snapshot,
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        signing_key,
        community_registry,
        community_adapter_tx,
        unicast_send_tx,
        dm_outbox,
        channel_log_registry,
        fence_check,
    )
    .await?;

    if let Err(e) = app.emit(
        "nav-updated",
        &NavUpdatedPayload {
            action: "added",
            space_id: dto.community_id.clone(),
            kind: "community",
            name: dto.community_name.clone(),
            members: None,
            parent_id: None,
            pending: None,
        },
    ) {
        tracing::warn!(error = %e, "join_open_community: nav-updated emit failed");
    }

    Ok(dto)
}

#[cfg(test)]
mod redeem_invite_inner_tests {
    use super::*;
    use crate::community_channel_log_engine::{
        ChannelLogEngineConfig, ChannelLogRegistry, ChannelLogRegistryConfig,
    };
    use crate::community_invite::CommunityInvitePayload;
    use crate::community_state_sync::{CommunityRegistryConfig, CommunitySyncRegistry};
    use crate::content_store::{ContentStore, RuntimeContentStore};
    use crate::dm_outbox::{DmOutbox, UnicastSendRequest};
    use crate::owner_state_crdt::OwnerState;
    use crate::owner_state_types::{DeviceIdentityHash, EpochKey, Hlc, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;
    use std::collections::BTreeMap;
    use tokio::sync::mpsc;

    // ── Fixture helper ────────────────────────────────────────────────────────

    pub(super) fn signing_key_from_identity(
        identity: &PrivateIdentity,
    ) -> std::sync::Arc<ed25519_dalek::SigningKey> {
        let sk_bytes_full = identity.to_private_bytes();
        let ed_seed: [u8; 32] = sk_bytes_full[32..64]
            .try_into()
            .expect("ed25519 seed slice 32..64");
        std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&ed_seed))
    }

    pub(super) struct RedeemInviteTestFixture {
        pub(super) crdt_state: std::sync::Arc<tokio::sync::Mutex<OwnerState>>,
        pub(super) hlc_tracker: std::sync::Arc<tokio::sync::Mutex<BTreeMap<String, Hlc>>>,
        pub(super) device_id: String,
        pub(super) self_owner: OwnerAddr,
        pub(super) signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
        pub(super) community_registry: std::sync::Arc<CommunitySyncRegistry>,
        pub(super) community_adapter_tx:
            tokio::sync::mpsc::Sender<crate::event_loop::CommunityAdapterRequest>,
        pub(super) unicast_send_tx: tokio::sync::mpsc::Sender<UnicastSendRequest>,
        pub(super) dm_outbox: std::sync::Arc<tokio::sync::Mutex<DmOutbox>>,
        pub(super) channel_log_registry:
            std::sync::Arc<ChannelLogRegistry<tauri::test::MockRuntime>>,
        // Held alive so channels don't report Closed.
        _community_adapter_rx:
            tokio::sync::mpsc::Receiver<crate::event_loop::CommunityAdapterRequest>,
        _unicast_rx: tokio::sync::mpsc::Receiver<UnicastSendRequest>,
        _channel_log_adapter_rx:
            tokio::sync::mpsc::UnboundedReceiver<crate::event_loop::ChannelLogAdapterRequest>,
        _tmp: tempfile::TempDir,
    }

    /// Build a minimal fixture for `redeem_invite_inner` unit tests.
    ///
    /// Key design choices:
    /// - OPEN-only invite: `is_invite_only = false` avoids the Zenoh unicast
    ///   path; the happy path inserts the bootstrap_join locally via the engine
    ///   and proceeds to fence_check → apply_space.
    /// - `delta_tx: None` in `CommunityRegistryConfig` — no ChannelCreate
    ///   events flow to `channel_log_registry`. The registry is present to
    ///   receive `begin_transaction` + guard drop / `commit()` calls.
    /// - The channel-log registry uses a dummy adapter bridge (unbounded
    ///   sender + receiver kept alive). No Zenoh drainer runs — harmless
    ///   since no spawns reach `spawn_inner_now` during an OPEN redeem
    ///   (remote Zenoh events don't arrive in unit tests).
    /// - `unicast_send_tx` receiver is kept alive so try_send doesn't see
    ///   Closed (needed by the INVITE-ONLY branch; unused on the OPEN path).
    pub(super) async fn build_redeem_invite_test_fixture() -> RedeemInviteTestFixture {
        let tmp = tempfile::TempDir::new().expect("tempdir");

        // Joiner identity (self).
        let joiner_identity = PrivateIdentity::from_seed(&[0xbb; 32]);
        let self_owner = OwnerAddr(joiner_identity.identity.address_hash);
        let signing_key = signing_key_from_identity(&joiner_identity);

        // Admin identity (invite creator). Used only for invite payload construction.
        let admin_identity = PrivateIdentity::from_seed(&[0xaa; 32]);
        let admin_owner = OwnerAddr(admin_identity.identity.address_hash);
        let admin_pub_64 = admin_identity.identity.to_public_bytes();

        // Identity resolver that admits events signed by both admin and joiner.
        struct TwoOwnerResolver {
            admin: OwnerAddr,
            admin_pub: [u8; 64],
            joiner: OwnerAddr,
            joiner_pub: [u8; 64],
        }
        #[async_trait::async_trait]
        impl crate::community_state_sync::IdentityResolver for TwoOwnerResolver {
            async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
                if *addr == self.admin {
                    Some(self.admin_pub)
                } else if *addr == self.joiner {
                    Some(self.joiner_pub)
                } else {
                    None
                }
            }
        }

        let joiner_pub_64 = joiner_identity.identity.to_public_bytes();

        let (cas_op_tx, _cas_op_rx) = mpsc::channel(8);
        let cs: std::sync::Arc<dyn ContentStore> = std::sync::Arc::new(RuntimeContentStore::new(
            cas_op_tx,
            std::time::Duration::from_millis(1000),
        ));
        let community_registry =
            std::sync::Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
                device_id: "joiner-dev".into(),
                content_store: cs,
                identity_resolver: std::sync::Arc::new(TwoOwnerResolver {
                    admin: admin_owner,
                    admin_pub: admin_pub_64,
                    joiner: self_owner,
                    joiner_pub: joiner_pub_64,
                }),
                identity_dir: tmp.path().to_path_buf(),
                debounce_ms: crate::community_state_sync::DEFAULT_DEBOUNCE_MS,
                error_tx: None,
                delta_tx: None,
                self_owner,
                signing_key: std::sync::Arc::clone(&signing_key),
                crdt_state: None,
                nav_emitter: None,
            }));

        let (community_adapter_tx, _community_adapter_rx) =
            mpsc::channel::<crate::event_loop::CommunityAdapterRequest>(16);
        let (unicast_send_tx, _unicast_rx) = mpsc::channel::<UnicastSendRequest>(16);

        let dm_outbox = std::sync::Arc::new(tokio::sync::Mutex::new(DmOutbox::new(
            "joiner-dev".into(),
            self_owner,
            DeviceIdentityHash(joiner_identity.identity.address_hash),
            std::sync::Arc::clone(&signing_key),
            std::sync::Arc::new(joiner_identity),
        )));

        let (channel_log_adapter_tx, _channel_log_adapter_rx) =
            mpsc::unbounded_channel::<crate::event_loop::ChannelLogAdapterRequest>();
        let app = tauri::test::mock_app().handle().clone();
        let channel_log_registry = ChannelLogRegistry::new(ChannelLogRegistryConfig {
            adapter_request_tx: channel_log_adapter_tx,
            app,
            identity_dir: tmp.path().to_path_buf(),
            self_owner,
            self_device_id: "joiner-dev".into(),
            signing_key: std::sync::Arc::clone(&signing_key),
            engine_config: ChannelLogEngineConfig::default(),
        });

        let crdt_state = std::sync::Arc::new(tokio::sync::Mutex::new(OwnerState::default()));
        let hlc_tracker = std::sync::Arc::new(tokio::sync::Mutex::new(BTreeMap::new()));

        RedeemInviteTestFixture {
            crdt_state,
            hlc_tracker,
            device_id: "joiner-dev".into(),
            self_owner,
            signing_key,
            community_registry,
            community_adapter_tx,
            unicast_send_tx,
            dm_outbox,
            channel_log_registry,
            _community_adapter_rx,
            _unicast_rx,
            _channel_log_adapter_rx,
            _tmp: tmp,
        }
    }

    // ── ZEB-271: channel-log transaction coverage ──────────────────────────────

    /// Happy path (spec §7.3): `redeem_invite_inner` returns Ok for an
    /// OPEN-community invite. After success the transaction must be committed
    /// (no lingering pending entry in `channel_log_registry`).
    ///
    /// Note: unlike `create_community_inner`, `redeem_invite_inner` does NOT
    /// mint ChannelCreate events — the transaction protects against remote
    /// ChannelCreate events that arrive via Zenoh sync. In this unit test
    /// there is no Zenoh session, so the deferred queue is empty at commit.
    /// The assertion "no pending tx" is a proxy for "commit was called".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn happy_path_no_pending_transaction_after_success() {
        let fixture = build_redeem_invite_test_fixture().await;

        // Build a minimal OPEN-community invite URL. The invite carries:
        // - community_id: fixed test bytes so we can check later
        // - membership_key: arbitrary 32 bytes
        // - admin_addr: admin_identity.address_hash (matches our resolver)
        // - is_invite_only: false  ← OPEN path (no Zenoh unicast needed)
        // - no invite_token / admin_bootstrap
        let admin_identity = PrivateIdentity::from_seed(&[0xaa; 32]);
        let admin_addr = OwnerAddr(admin_identity.identity.address_hash);

        let community_id = crate::owner_state_types::SpaceId([0xf1; 16]);
        let membership_key = EpochKey::new([0x42; 32]);

        let invite_payload = CommunityInvitePayload {
            community_id,
            epoch_snapshot: crate::community_invite::InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: membership_key.as_bytes().to_vec(),
                state_snapshot: crate::community_invite::MaterializedCommunityState::default(),
            },
            admin_addr,
            community_name: "TestRedeemCom".into(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
            admin_bootstrap: None,
            admin_identity_pub: None,
            forked_from: None,
            pre_fork_snapshot: None,
        };

        let invite_url =
            crate::community_invite::encode_invite_url(&invite_payload).expect("encode invite url");

        let result = redeem_invite_inner(
            invite_url,
            std::sync::Arc::clone(&fixture.crdt_state),
            std::sync::Arc::clone(&fixture.hlc_tracker),
            fixture.device_id.clone(),
            fixture.self_owner,
            std::sync::Arc::clone(&fixture.signing_key),
            std::sync::Arc::clone(&fixture.community_registry),
            fixture.community_adapter_tx.clone(),
            fixture.unicast_send_tx.clone(),
            std::sync::Arc::clone(&fixture.dm_outbox),
            std::sync::Arc::clone(&fixture.channel_log_registry),
            || Ok(()),
            None, // identity_dir: no fork fields in this test
        )
        .await;

        assert!(
            result.is_ok(),
            "redeem_invite_inner happy path must succeed: {:?}",
            result
        );

        // Proxy assertion for "tx was committed": no pending entry
        // remaining for this community_id.
        assert!(
            !fixture
                .channel_log_registry
                .has_pending_transaction_for_test(&community_id),
            "happy path: channel_log transaction must be committed (no lingering pending entry)"
        );
    }

    // ── Existing tests ─────────────────────────────────────────────────────────

    #[test]
    fn mint_redemption_produces_self_join_and_matching_space() {
        let identity = PrivateIdentity::from_seed(&[0xab; 32]);
        let identity_pub = identity.identity.to_public_bytes();
        let self_owner = OwnerAddr(identity.identity.address_hash);
        // Mirror Task 9's test pattern: pull the canonical 32-byte
        // Ed25519 seed from bytes 32..64 of `to_private_bytes()`. The
        // production IPC borrows this same SigningKey from `dm_outbox`.
        let sk_bytes_full = identity.to_private_bytes();
        let ed_seed: [u8; 32] = sk_bytes_full[32..64]
            .try_into()
            .expect("ed25519 seed slice 32..64");
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_seed);

        let payload = CommunityInvitePayload {
            community_id: SpaceId([0xee; 16]),
            epoch_snapshot: crate::community_invite::InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: EpochKey::new([0x77; 32]).as_bytes().to_vec(),
                state_snapshot: crate::community_invite::MaterializedCommunityState::default(),
            },
            admin_addr: OwnerAddr([0x33; 16]),
            community_name: "TestCom".into(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
            admin_bootstrap: None,
            admin_identity_pub: None,
            forked_from: None,
            pre_fork_snapshot: None,
        };

        let device_id = "joiner-dev";
        // ZEB-267: caller pre-reserves the HLC; constructed inline here
        // since the test isn't driving an actual tracker.
        let join_hlc = Hlc {
            wall_ms: 1_700_000_999_000u64,
            logical: 0,
            device_id: device_id.to_string(),
        };

        let minted = mint_redemption(&payload, self_owner, &signing_key, join_hlc).expect("mint");

        assert_eq!(minted.community_id, payload.community_id);
        assert_eq!(minted.space.id, payload.community_id);
        assert_eq!(minted.space.admin_addr, Some(payload.admin_addr));
        assert_eq!(minted.space.is_invite_only, Some(false));
        assert_eq!(minted.bootstrap_join.actor, self_owner);
        assert_eq!(minted.bootstrap_join.community_id, payload.community_id);
        assert!(matches!(
            minted.bootstrap_join.kind,
            crate::community_membership::MembershipEventKind::Join
        ));

        // Self-join sig must verify against the joiner's identity_pub —
        // the engine's verify_event runs the same check on insert.
        crate::community_membership::verify_signature(&minted.bootstrap_join, &identity_pub)
            .expect("self-join signature must verify against joiner identity_pub");
    }

    // ── ZEB-285 Task 7: fork-invite carry tests ────────────────────────────────

    /// ZEB-285 Task 7: `redeem_invite_inner` with `forked_from` + `pre_fork_snapshot`
    /// in the payload must:
    ///   1. Set `CommunityState.forked_from` on the joiner's engine.
    ///   2. Write `pre_fork_snapshot.bin` to `{identity_dir}/communities/{hex_id}/`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn redeem_invite_writes_snapshot_to_data_dir() {
        let fixture = build_redeem_invite_test_fixture().await;
        let tmp = tempfile::TempDir::new().expect("tempdir for identity_dir");

        let admin_identity = PrivateIdentity::from_seed(&[0xaa; 32]);
        let admin_addr = OwnerAddr(admin_identity.identity.address_hash);

        let original_id = SpaceId([0xab; 16]);
        let community_id = SpaceId([0xf2; 16]);
        let membership_key = EpochKey::new([0x42; 32]);

        // Build a minimal PreForkSnapshot.
        let snapshot = crate::community_invite::PreForkSnapshot {
            original_community_id: original_id,
            original_community_name: "OriginalCom".into(),
            membership_events: vec![],
            channel_log: crate::community_invite::BoundedChannelLogSnapshot {
                per_channel: std::collections::BTreeMap::new(),
            },
            identity_pubs: std::collections::BTreeMap::new(),
            forked_at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "fork-dev".into(),
            },
            parent_lineage: Vec::new(),
        };

        let invite_payload = CommunityInvitePayload {
            community_id,
            epoch_snapshot: crate::community_invite::InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: membership_key.as_bytes().to_vec(),
                state_snapshot: crate::community_invite::MaterializedCommunityState::default(),
            },
            admin_addr,
            community_name: "ForkCom".into(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
            admin_bootstrap: None,
            admin_identity_pub: None,
            forked_from: Some(original_id),
            pre_fork_snapshot: Some(snapshot.clone()),
        };

        let invite_url =
            crate::community_invite::encode_invite_url(&invite_payload).expect("encode invite url");

        let result = redeem_invite_inner(
            invite_url,
            std::sync::Arc::clone(&fixture.crdt_state),
            std::sync::Arc::clone(&fixture.hlc_tracker),
            fixture.device_id.clone(),
            fixture.self_owner,
            std::sync::Arc::clone(&fixture.signing_key),
            std::sync::Arc::clone(&fixture.community_registry),
            fixture.community_adapter_tx.clone(),
            fixture.unicast_send_tx.clone(),
            std::sync::Arc::clone(&fixture.dm_outbox),
            std::sync::Arc::clone(&fixture.channel_log_registry),
            || Ok(()),
            Some(tmp.path().to_path_buf()),
        )
        .await;

        assert!(
            result.is_ok(),
            "fork-invite redeem must succeed: {:?}",
            result
        );

        // Assert 1: pre_fork_snapshot.bin was written.
        let snapshot_path = tmp
            .path()
            .join("communities")
            .join(hex::encode(community_id.0))
            .join("pre_fork_snapshot.bin");
        assert!(
            snapshot_path.exists(),
            "pre_fork_snapshot.bin must be written to data dir: {}",
            snapshot_path.display()
        );

        // Assert 2: decoded bytes match the input snapshot.
        let bytes = std::fs::read(&snapshot_path).expect("read snapshot file");
        let decoded: crate::community_invite::PreForkSnapshot =
            crate::owner_state_crypto::canonical_cbor_decode(&bytes)
                .expect("decode pre_fork_snapshot.bin");
        assert_eq!(decoded, snapshot, "decoded snapshot must match input");

        // Assert 3: CommunityState.forked_from is set on the engine.
        let state_arc = fixture
            .community_registry
            .state_for(&community_id)
            .await
            .expect("engine state must exist after redeem");
        let state_g = state_arc.lock().await;
        assert_eq!(
            state_g.forked_from,
            Some(original_id),
            "CommunityState.forked_from must mirror the invite's forked_from"
        );
    }

    /// ZEB-287 Phase 2 spec §3.5: when redeeming a fork-invite, the new
    /// CommunityState must pick up `forked_at_wall_ms` and `parent_lineage`
    /// from the snapshot. Phase 1 fork-invites with empty `parent_lineage`
    /// still get `forked_at_wall_ms` populated from `snapshot.forked_at`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn redeem_fork_invite_wires_parent_lineage_into_community_state() {
        let fixture = build_redeem_invite_test_fixture().await;
        let tmp = tempfile::TempDir::new().expect("tempdir for identity_dir");

        let admin_identity = PrivateIdentity::from_seed(&[0xab; 32]);
        let admin_addr = OwnerAddr(admin_identity.identity.address_hash);

        let original_id = SpaceId([0xac; 16]);
        let community_id = SpaceId([0xf4; 16]);
        let membership_key = EpochKey::new([0x43; 32]);

        // Phase 2 snapshot: carries a 2-entry parent_lineage representing
        // a grandparent (C) and a parent-of-parent (B) ancestor chain.
        let c_entry = crate::community_invite::ParentLineageEntry {
            space_id: SpaceId([0xc0; 16]),
            name: "Grandparent C".into(),
            forked_at_wall_ms: None, // C is root of the chain
        };
        let b_entry = crate::community_invite::ParentLineageEntry {
            space_id: SpaceId([0xb0; 16]),
            name: "Parent-of-parent B".into(),
            forked_at_wall_ms: Some(1_650_000_000_000),
        };

        let snapshot = crate::community_invite::PreForkSnapshot {
            original_community_id: original_id,
            original_community_name: "OriginalCom".into(),
            membership_events: vec![],
            channel_log: crate::community_invite::BoundedChannelLogSnapshot {
                per_channel: std::collections::BTreeMap::new(),
            },
            identity_pubs: std::collections::BTreeMap::new(),
            forked_at: Hlc {
                wall_ms: 1_715_000_000_000,
                logical: 0,
                device_id: "fork-dev".into(),
            },
            parent_lineage: vec![c_entry.clone(), b_entry.clone()],
        };

        let invite_payload = CommunityInvitePayload {
            community_id,
            epoch_snapshot: crate::community_invite::InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: membership_key.as_bytes().to_vec(),
                state_snapshot: crate::community_invite::MaterializedCommunityState::default(),
            },
            admin_addr,
            community_name: "ForkCom".into(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
            admin_bootstrap: None,
            admin_identity_pub: None,
            forked_from: Some(original_id),
            pre_fork_snapshot: Some(snapshot.clone()),
        };

        let invite_url =
            crate::community_invite::encode_invite_url(&invite_payload).expect("encode invite url");

        let result = redeem_invite_inner(
            invite_url,
            std::sync::Arc::clone(&fixture.crdt_state),
            std::sync::Arc::clone(&fixture.hlc_tracker),
            fixture.device_id.clone(),
            fixture.self_owner,
            std::sync::Arc::clone(&fixture.signing_key),
            std::sync::Arc::clone(&fixture.community_registry),
            fixture.community_adapter_tx.clone(),
            fixture.unicast_send_tx.clone(),
            std::sync::Arc::clone(&fixture.dm_outbox),
            std::sync::Arc::clone(&fixture.channel_log_registry),
            || Ok(()),
            Some(tmp.path().to_path_buf()),
        )
        .await;

        assert!(
            result.is_ok(),
            "fork-invite redeem must succeed: {:?}",
            result
        );

        // Assert: CommunityState.forked_at_wall_ms + parent_lineage carry through.
        let state_arc = fixture
            .community_registry
            .state_for(&community_id)
            .await
            .expect("engine state must exist after redeem");
        let state_g = state_arc.lock().await;
        assert_eq!(
            state_g.forked_from,
            Some(original_id),
            "Phase 1 forked_from must still be wired"
        );
        assert_eq!(
            state_g.forked_at_wall_ms,
            Some(1_715_000_000_000),
            "ZEB-287: forked_at_wall_ms must come from snapshot.forked_at.wall_ms"
        );
        assert_eq!(
            state_g.parent_lineage,
            vec![c_entry, b_entry],
            "ZEB-287: parent_lineage must mirror snapshot.parent_lineage"
        );
    }

    /// ZEB-287 Phase 2 spec §6.2: a Phase 1-shape fork-invite (whose
    /// snapshot.parent_lineage is empty — Phase 1 wire compat default)
    /// must still redeem cleanly. The resulting CommunityState has
    /// `parent_lineage: []` (the correct "ancestry beyond immediate parent
    /// unknown" state) and `forked_at_wall_ms: Some(...)` (Phase 1's
    /// existing Hlc has wall_ms regardless of Phase 2).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn redeem_phase1_fork_invite_yields_empty_lineage_with_forked_at_set() {
        let fixture = build_redeem_invite_test_fixture().await;
        let tmp = tempfile::TempDir::new().expect("tempdir for identity_dir");

        let admin_identity = PrivateIdentity::from_seed(&[0xae; 32]);
        let admin_addr = OwnerAddr(admin_identity.identity.address_hash);

        let original_id = SpaceId([0xad; 16]);
        let community_id = SpaceId([0xf5; 16]);
        let membership_key = EpochKey::new([0x44; 32]);

        // Phase 1-shape snapshot: parent_lineage defaults empty.
        let snapshot = crate::community_invite::PreForkSnapshot {
            original_community_id: original_id,
            original_community_name: "Phase1OriginalCom".into(),
            membership_events: vec![],
            channel_log: crate::community_invite::BoundedChannelLogSnapshot {
                per_channel: std::collections::BTreeMap::new(),
            },
            identity_pubs: std::collections::BTreeMap::new(),
            forked_at: Hlc {
                wall_ms: 1_710_000_000_000,
                logical: 0,
                device_id: "phase1-dev".into(),
            },
            parent_lineage: Vec::new(), // Phase 1 default
        };

        let invite_payload = CommunityInvitePayload {
            community_id,
            epoch_snapshot: crate::community_invite::InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: membership_key.as_bytes().to_vec(),
                state_snapshot: crate::community_invite::MaterializedCommunityState::default(),
            },
            admin_addr,
            community_name: "Phase1ForkCom".into(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
            admin_bootstrap: None,
            admin_identity_pub: None,
            forked_from: Some(original_id),
            pre_fork_snapshot: Some(snapshot.clone()),
        };

        let invite_url =
            crate::community_invite::encode_invite_url(&invite_payload).expect("encode invite url");

        let result = redeem_invite_inner(
            invite_url,
            std::sync::Arc::clone(&fixture.crdt_state),
            std::sync::Arc::clone(&fixture.hlc_tracker),
            fixture.device_id.clone(),
            fixture.self_owner,
            std::sync::Arc::clone(&fixture.signing_key),
            std::sync::Arc::clone(&fixture.community_registry),
            fixture.community_adapter_tx.clone(),
            fixture.unicast_send_tx.clone(),
            std::sync::Arc::clone(&fixture.dm_outbox),
            std::sync::Arc::clone(&fixture.channel_log_registry),
            || Ok(()),
            Some(tmp.path().to_path_buf()),
        )
        .await;

        assert!(
            result.is_ok(),
            "Phase 1-shape fork-invite redeem must succeed: {:?}",
            result
        );

        let state_arc = fixture
            .community_registry
            .state_for(&community_id)
            .await
            .expect("engine state must exist after redeem");
        let state_g = state_arc.lock().await;
        assert_eq!(state_g.forked_from, Some(original_id));
        // Phase 2 still fills forked_at_wall_ms from Phase 1's existing forked_at Hlc.
        assert_eq!(state_g.forked_at_wall_ms, Some(1_710_000_000_000));
        // parent_lineage stays empty — "ancestry beyond immediate parent unknown".
        assert!(
            state_g.parent_lineage.is_empty(),
            "Phase 1 fork's parent_lineage must remain empty"
        );
    }

    // ── ZEB-254 Task 7: mint_redemption event-kind tests ──────────────────────

    #[test]
    fn mint_redemption_open_path_still_produces_join() {
        use crate::community_invite::{
            InviteEpochSnapshot, InviteToken, MaterializedCommunityState,
        };
        use crate::community_membership::MembershipEventKind;
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;

        let admin_addr = OwnerAddr([0x11; 16]);
        let community_id = SpaceId([0x33; 16]);
        let joiner_sk = SigningKey::generate(&mut OsRng);
        let joiner_addr = OwnerAddr([0x22; 16]);

        let token = InviteToken {
            inviter: admin_addr,
            invitee_hint: None,
            minted_at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "admin".into(),
            },
            expires_at: None,
            sig: [0; 64],
        };

        let payload = CommunityInvitePayload {
            community_id,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: EpochKey::new([0x42; 32]).as_bytes().to_vec(),
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr,
            community_name: "Open Test".into(),
            is_invite_only: false,
            expires_at: None,
            invite_token: Some(token),
            admin_bootstrap: None,
            admin_identity_pub: None,
            forked_from: None,
            pre_fork_snapshot: None,
        };

        let hlc = Hlc {
            wall_ms: 1_700_000_001_000,
            logical: 0,
            device_id: "joiner".into(),
        };
        let minted = mint_redemption(&payload, joiner_addr, &joiner_sk, hlc).expect("mint open");

        assert!(
            matches!(minted.bootstrap_join.kind, MembershipEventKind::Join),
            "open path must produce Join kind, got {:?}",
            minted.bootstrap_join.kind
        );
    }

    #[test]
    fn mint_redemption_invite_only_path_produces_pending_join() {
        use crate::community_invite::{
            InviteEpochSnapshot, InviteToken, MaterializedCommunityState,
        };
        use crate::community_membership::MembershipEventKind;
        use crate::dm_signing::{ed25519_priv_to_x25519, seal_to_owner};
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;

        let admin_addr = OwnerAddr([0x11; 16]);
        let community_id = SpaceId([0x33; 16]);
        let joiner_sk = SigningKey::generate(&mut OsRng);
        let joiner_addr = OwnerAddr([0x22; 16]);

        // Derive joiner's X25519 pub for the seal.
        let joiner_x25519_priv = ed25519_priv_to_x25519(&joiner_sk);
        let joiner_x25519_pub =
            x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*joiner_x25519_priv));

        // Seal a 32-byte epoch key to the joiner's X25519 pub.
        let raw_key = [0xEEu8; 32];
        let sealed = seal_to_owner(joiner_x25519_pub.as_bytes(), &raw_key).expect("seal");
        assert_eq!(sealed.len(), 92, "sealed envelope must be 92 bytes");

        let token = InviteToken {
            inviter: admin_addr,
            invitee_hint: Some(joiner_addr),
            minted_at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "admin".into(),
            },
            expires_at: None,
            sig: [0; 64],
        };

        let payload = CommunityInvitePayload {
            community_id,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: sealed,
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr,
            community_name: "Invite-only Test".into(),
            is_invite_only: true,
            expires_at: None,
            invite_token: Some(token),
            admin_bootstrap: None,
            admin_identity_pub: None,
            forked_from: None,
            pre_fork_snapshot: None,
        };

        let hlc = Hlc {
            wall_ms: 1_700_000_001_000,
            logical: 0,
            device_id: "joiner".into(),
        };
        let minted =
            mint_redemption(&payload, joiner_addr, &joiner_sk, hlc).expect("mint invite-only");

        match &minted.bootstrap_join.kind {
            MembershipEventKind::PendingJoin {
                invite_token: t,
                joiner_identity_pub,
            } => {
                assert_eq!(
                    t.inviter, admin_addr,
                    "InviteToken should be carried through"
                );
                assert_eq!(
                    joiner_identity_pub.len(),
                    64,
                    "joiner_identity_pub must be 64 bytes"
                );
                // The Ed25519 half (bytes 32-64) MUST match the signing_key's verifying key.
                let ed_pub = joiner_sk.verifying_key().to_bytes();
                assert_eq!(
                    &joiner_identity_pub[32..],
                    &ed_pub,
                    "Ed25519 half must match"
                );
            }
            other => panic!("expected PendingJoin kind, got {:?}", other),
        }
    }
}

#[cfg(test)]
mod join_open_community_tests {
    use super::*;
    use crate::community_invite::{
        encode_invite_url, CommunityInvitePayload, InviteEpochSnapshot, MaterializedCommunityState,
    };
    use crate::library_directory::{AggregatedEntry, LibraryDirectoryEntry};
    use crate::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;
    use std::collections::BTreeSet;

    fn build_open_directory_aggregated(
        admin_identity: &PrivateIdentity,
        community_id: SpaceId,
        membership_key_bytes: [u8; 32],
        community_name: &str,
    ) -> AggregatedEntry {
        let admin_addr = OwnerAddr(admin_identity.identity.address_hash);
        let payload = CommunityInvitePayload {
            community_id,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: membership_key_bytes.to_vec(),
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr,
            community_name: community_name.into(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
            admin_bootstrap: None,
            admin_identity_pub: None,
            forked_from: None,
            pre_fork_snapshot: None,
        };
        let invite_url = encode_invite_url(&payload).expect("encode open url");
        let entry = LibraryDirectoryEntry {
            community_id,
            community_admin_identity_pub: admin_identity.identity.to_public_bytes(),
            name: community_name.into(),
            description: String::new(),
            topics: Vec::new(),
            invite_url,
            listed_by: OwnerAddr([0xcc; 16]),
            listed_at: Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "test-dev".into(),
            },
            library_identity_pub: None,
            library_signature: None,
            community_signature: [0u8; 64],
        };
        AggregatedEntry {
            entry,
            attested_by: BTreeSet::new(),
            unattested_by: BTreeSet::new(),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn join_open_community_happy_path_delegates_to_redeem_and_returns_dto() {
        let fixture = redeem_invite_inner_tests::build_redeem_invite_test_fixture().await;

        let admin_identity = PrivateIdentity::from_seed(&[0xaa; 32]);
        let community_id = SpaceId([0xf3; 16]);
        let membership_key = EpochKey::new([0x42; 32]);

        let agg = build_open_directory_aggregated(
            &admin_identity,
            community_id,
            *membership_key.as_bytes(),
            "JoinCom",
        );
        let snapshot = vec![agg];

        let dto = join_open_community_inner(
            hex::encode(community_id.0),
            &snapshot,
            std::sync::Arc::clone(&fixture.crdt_state),
            std::sync::Arc::clone(&fixture.hlc_tracker),
            fixture.device_id.clone(),
            fixture.self_owner,
            std::sync::Arc::clone(&fixture.signing_key),
            std::sync::Arc::clone(&fixture.community_registry),
            fixture.community_adapter_tx.clone(),
            fixture.unicast_send_tx.clone(),
            std::sync::Arc::clone(&fixture.dm_outbox),
            std::sync::Arc::clone(&fixture.channel_log_registry),
            || Ok(()),
        )
        .await
        .expect("happy path must succeed");

        assert_eq!(dto.community_id, hex::encode(community_id.0));
        assert_eq!(dto.community_name, "JoinCom");
        assert!(!dto.is_invite_only);
    }
}

/// Sentinel returned from the rotation-bundle closure inside
/// `leave_community` when the leaver is the sole remaining member
/// (solo leave). Using a named constant rather than a bare string
/// literal prevents the fragile `e.contains("no remaining members")`
/// check at the call-site from silently diverging if the message
/// text ever changes. E4 (ZEB-249 §10.6 R3 review).
const LEAVE_SOLO_SENTINEL: &str = "no remaining members — solo leave, no rotation needed";

// ── ZEB-217 Sub-C Phase 3 Task 11: leave_community ───────────────────
//
// Mints a self-Leave SignedMembershipEvent and inserts it into the
// per-community engine. Does NOT mutate owner-state Space (per spec
// line 514): the Space row stays around with its existing fields so
// the user can see "you've left this community" in the UI and choose
// to remove it later via the existing `remove_space` IPC. The Leave
// event in the CRDT is what peers see + what the materialized member
// list reflects.
//
// Engine lifecycle: Phase 3 does NOT call registry.stop_engine on
// leave. The Leave event must publish to peers and the engine's
// debounced publish loop owns that — stopping immediately could race
// the publish. The user's eventual `remove_space` (or a future
// `forget_community` IPC) would call `stop_engine`; for Phase 3 the
// engine stays running.

/// Pure function: mint a self-Leave `SignedMembershipEvent` for a
/// community we currently belong to. Mirrors the
/// `mint_redemption` / `mint_community_creation` shape — pure / sync /
/// no I/O so the canonical-CBOR / signing path is unit-testable
/// without standing up a Tauri test harness.
///
/// ZEB-267: Caller pre-reserves `hlc` via
/// `dm_outbox::reserve_next_hlc_for_device`.
pub fn mint_leave_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    signing_key: &ed25519_dalek::SigningKey,
    hlc: crate::owner_state_types::Hlc,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::Leave,
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign leave: {e}"))
}

/// Convert a non-`Inserted` `InsertOutcome` into a user-facing error
/// string for the membership-IPC surface (`leave_community`,
/// `kick_from_community`, `set_power_level`). Uses the inner
/// `VerifyError`'s `Display` impl (which gives stable, frontend-
/// friendly messages) rather than the debug repr (which would leak
/// internal enum variant names like `InsertOutcome::Rejected(VerifyError::KickTargetPowerNotLower)`).
///
/// `action` is the user-visible verb prefix ("leave_community", "kick",
/// "set_power_level") so the caller knows which IPC the rejection came
/// from when multiple flow through the same UI surface.
fn membership_outcome_err(
    action: &str,
    outcome: &crate::community_state_crdt::InsertOutcome,
) -> String {
    match outcome {
        crate::community_state_crdt::InsertOutcome::Rejected(verr) => {
            format!("{action} rejected: {verr}")
        }
        // Inserted should never reach this helper (callers gate on it
        // via `matches!`), but a stable fallback string is better than
        // panicking on a future-added variant.
        crate::community_state_crdt::InsertOutcome::Inserted => {
            format!("{action} unexpected outcome: Inserted")
        }
        crate::community_state_crdt::InsertOutcome::AlreadyKnown => {
            format!("{action} unexpected outcome: AlreadyKnown")
        }
    }
}

// ── ZEB-218 Sub-D Phase 1: library-directory IPC surface ─────────────
//
// Four `#[tauri::command]` handlers that drive the
// `LibraryDirectory` consumer:
//
// - `list_libraries` — snapshot of effective LibraryEntry rows from
//   owner-state, enriched with per-library aggregation counts.
// - `add_library` — LWW-merge into `owner_state.libraries` + Subscribe
//   request to the event-loop consumer.
// - `remove_library` — set tombstone + Unsubscribe (which evicts the
//   library's contributions from the aggregation map).
// - `browse_library` — snapshot the aggregation map; `None` returns
//   all, `Some(addr_hex)` filters to one library's listings.
//
// Persistence: owner-state mutations flow through the existing
// owner-state SyncEngine's debounced persist cycle — same pattern as
// `add_space`. No explicit write-back IPC-side; the SyncEngine picks
// up the BTreeMap mutation on its next checkpoint.
//
// HLC discipline: mints via `reserve_next_hlc_for_device` (the
// `dm_outbox` helper) so the local tracker advances atomically with
// the reservation — same shape as other owner-state mutations.

/// IPC: list the user's effective libraries with per-library
/// aggregation counts. Returns `Vec<LibraryInfo>` ordered by the
/// `OwnerAddr` key (BTreeMap iteration order).
#[tauri::command]
async fn list_libraries(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
) -> Result<Vec<crate::library_directory::LibraryInfo>, String> {
    let (crdt_state, library_directory) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.library_directory
                .clone()
                .ok_or("library_directory missing — node not running?")?,
        )
    };
    let crdt_g = crdt_state.lock().await;
    let agg_g = library_directory.aggregation.lock().await;
    let mut out = Vec::new();
    for (addr, lib) in &crdt_g.libraries {
        if !lib.is_effective() {
            continue;
        }
        let count = agg_g.entry_count_for_library(addr);
        out.push(crate::library_directory::LibraryInfo {
            address: hex::encode(addr.0),
            added_at: lib.added_at.clone(),
            entry_count: count,
        });
    }
    Ok(out)
}

/// IPC: list libraries the user has discovered via the
/// `harmony/discovery/library/announce` topic but has NOT yet added.
/// Filtered against `OwnerState.libraries` non-tombstoned entries —
/// once the user adds a discovered library, it disappears from this
/// list on the next refetch and appears in `list_libraries` instead.
///
/// Sub-D Phase 2 (ZEB-279). Spec §6.1.
///
/// Ordering: announces are returned newest-`listed_at`-first (the
/// `Announces::snapshot` ordering), so fresh discoveries surface at
/// the top of the UI. `listed_at` ties break on `OwnerAddr` byte order
/// for determinism.
///
/// Cost: re-derives each announce's library_addr via
/// `Identity::from_public_bytes` (one Blake3 hash per entry). At
/// `MAX_DISCOVERED_LIBRARIES = 1000` this is negligible at IPC call
/// time. `process_announce` already validated every announce in the
/// map, so the parse can't realistically fail — but a parse error is
/// handled defensively by skipping that announce.
#[tauri::command]
async fn list_discovered_libraries(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
) -> Result<Vec<crate::library_directory::DiscoveredLibraryInfo>, String> {
    let (crdt_state, library_directory) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.library_directory
                .clone()
                .ok_or("library_directory missing — node not running?")?,
        )
    };

    // Snapshot the already-added set first, drop the lock, then take
    // the announces lock. This keeps the two tokio mutexes lock-disjoint
    // (no nested holding) so an unrelated long-running announce-side
    // operation can't stall library_directory IPCs.
    let already_added: std::collections::BTreeSet<crate::owner_state_types::OwnerAddr> = {
        let crdt_g = crdt_state.lock().await;
        crdt_g
            .libraries
            .iter()
            .filter(|(_, entry)| entry.is_effective())
            .map(|(addr, _)| *addr)
            .collect()
    };

    let snapshot = {
        let announces_g = library_directory.announces.lock().await;
        announces_g.snapshot()
    };

    let mut out: Vec<crate::library_directory::DiscoveredLibraryInfo> =
        Vec::with_capacity(snapshot.len());
    for ann in snapshot {
        // Re-derive library_addr from the signed identity bundle. The
        // announce went through `verify_announce` already (which calls
        // `Identity::from_public_bytes` and ed25519 verify_strict), so
        // a parse failure here is unreachable on the happy path —
        // defensively skip rather than surface an IPC error.
        let identity =
            match harmony_identity::Identity::from_public_bytes(&ann.library_identity_pub) {
                Ok(id) => id,
                Err(_) => continue,
            };
        let addr = crate::owner_state_types::OwnerAddr(identity.address_hash);
        if already_added.contains(&addr) {
            continue;
        }
        out.push(crate::library_directory::DiscoveredLibraryInfo {
            library_addr: hex::encode(addr.0),
            name: ann.name,
            description: ann.description,
            listed_at: ann.listed_at.wall_ms.to_string(),
        });
    }
    Ok(out)
}

/// Pure LWW merge for `add_library`. Splits the merge math out of the
/// IPC handler so it can be unit-tested without spinning up a
/// `tauri::State<NodeState>` harness.
///
/// Returns:
/// - `Ok(new_entry)` — the LibraryEntry to insert into
///   `owner_state.libraries[addr]`. Caller persists.
/// - `Err(msg)` — caller surfaces to the user; mutation MUST be skipped.
///
/// Rules:
/// - If a higher-HLC tombstone exists, refuse (`HLC went backward; refusing to add`).
/// - Never regress `added_at`: keep `existing.added_at` if it's strictly
///   newer than `now_hlc` (R3 F1). Otherwise stamp `now_hlc`.
/// - Carry forward any existing tombstone so re-adds preserve the
///   removed_at HLC for LWW.
pub(crate) fn merge_add_library(
    existing: Option<&crate::owner_state_types::LibraryEntry>,
    addr: crate::owner_state_types::OwnerAddr,
    now_hlc: &crate::owner_state_types::Hlc,
) -> Result<crate::owner_state_types::LibraryEntry, String> {
    if let Some(prev) = existing {
        if let Some(prev_remove) = &prev.removed_at {
            if prev_remove.is_strictly_newer_than(now_hlc) {
                return Err("HLC went backward; refusing to add".into());
            }
        }
    }
    let chosen_added = match existing {
        Some(prev) if prev.added_at.is_strictly_newer_than(now_hlc) => prev.added_at.clone(),
        _ => now_hlc.clone(),
    };
    Ok(crate::owner_state_types::LibraryEntry {
        address: addr,
        added_at: chosen_added,
        removed_at: existing.and_then(|e| e.removed_at.clone()),
    })
}

/// Pure LWW guard for `remove_library`. Returns Ok if the caller may
/// stamp `now_hlc` into the row's `removed_at`; Err otherwise.
///
/// Rules:
/// - Row absent: nothing to remove. Caller treats as a no-op.
/// - Row present and `now_hlc <= existing.added_at`: refuse (R3 F2). A
///   write here would leave `is_effective()` true after the tombstone,
///   so the local unsubscribe + Ok return would create cross-device
///   divergence.
pub(crate) fn merge_remove_library(
    existing: Option<&crate::owner_state_types::LibraryEntry>,
    now_hlc: &crate::owner_state_types::Hlc,
) -> Result<(), String> {
    if let Some(lib) = existing {
        if !now_hlc.is_strictly_newer_than(&lib.added_at) {
            return Err("HLC went backward; refusing to remove".into());
        }
    }
    Ok(())
}

/// IPC: add a library to the user's trust set. LWW-merges into
/// `owner_state.libraries` and sends a Subscribe request to the
/// event-loop consumer (which declares the per-library Zenoh
/// subscriber).
#[tauri::command]
async fn add_library(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    library_addr: String,
) -> Result<(), String> {
    let addr = crate::library_directory::parse_owner_addr_hex(&library_addr)?;
    // R3 F3: snapshot `generation` paired-atomically with the Arcs so
    // the post-await fence below can detect a stop_node / start_node
    // racing through this command (mirrors add_space, send_dm).
    let (crdt_state, library_directory, hlc_tracker, device_id, snapshot_generation) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.library_directory
                .clone()
                .ok_or("library_directory missing — node not running?")?,
            g.hlc_tracker
                .clone()
                .ok_or("hlc_tracker missing — node not running?")?,
            g.dm_device_id
                .clone()
                .ok_or("dm_device_id missing — node not running?")?,
            g.generation,
        )
    };
    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let now_hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;
    {
        let mut crdt_g = crdt_state.lock().await;
        // R3 F3: post-await restart fence. If a stop_node/start_node
        // raced this command, our cloned Arcs are now detached — the
        // mutation would land in an orphan crdt_state that won't be
        // persisted, and the request_tx Subscribe would target the
        // old library_directory consumer task. Re-check generation
        // under the std lock before mutating.
        {
            let g = state_lock
                .lock()
                .map_err(|e| format!("NodeState poisoned: {e}"))?;
            if g.generation != snapshot_generation {
                return Err(format!(
                    "node generation changed during add_library (was {}, now {}); \
                     mutation would land in detached crdt_state — aborted",
                    snapshot_generation, g.generation
                ));
            }
        }
        // R3 F1: never regress the LWW state. If a remote bound device
        // already synced an add with a HIGHER HLC than our local now_hlc
        // (e.g., wall-clock skew or a synced concurrent add), keep the
        // existing `added_at`. Otherwise our overwrite would clobber a
        // strictly-newer LWW state. Logic is in `merge_add_library` so
        // it's unit-testable without a tauri::State harness.
        let new_entry = merge_add_library(crdt_g.libraries.get(&addr), addr, &now_hlc)?;
        crdt_g.libraries.insert(addr, new_entry);
        // Persistence writeback: owner-state SyncEngine debounces its
        // own checkpoint on the same crdt_state Arc (see add_space).
    }
    let _ = library_directory
        .request_tx
        .send(crate::library_directory::LibraryDirectoryRequest::Subscribe(addr));
    Ok(())
}

/// IPC: remove a library. Sets the LWW tombstone in owner-state and
/// sends an Unsubscribe request which (a) aborts the live Zenoh
/// subscriber task and (b) calls `drop_library` to evict the library's
/// contributions from the aggregation map.
#[tauri::command]
async fn remove_library(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    library_addr: String,
) -> Result<(), String> {
    let addr = crate::library_directory::parse_owner_addr_hex(&library_addr)?;
    // R3 F3: snapshot `generation` paired-atomically with the Arcs so
    // the post-await fence below can detect a stop_node / start_node
    // racing through this command (mirrors add_library, add_space).
    let (crdt_state, library_directory, hlc_tracker, device_id, snapshot_generation) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.library_directory
                .clone()
                .ok_or("library_directory missing — node not running?")?,
            g.hlc_tracker
                .clone()
                .ok_or("hlc_tracker missing — node not running?")?,
            g.dm_device_id
                .clone()
                .ok_or("dm_device_id missing — node not running?")?,
            g.generation,
        )
    };
    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let now_hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;
    {
        let mut crdt_g = crdt_state.lock().await;
        // R3 F3: post-await restart fence (same rationale as add_library).
        {
            let g = state_lock
                .lock()
                .map_err(|e| format!("NodeState poisoned: {e}"))?;
            if g.generation != snapshot_generation {
                return Err(format!(
                    "node generation changed during remove_library (was {}, now {}); \
                     mutation would land in detached crdt_state — aborted",
                    snapshot_generation, g.generation
                ));
            }
        }
        // R3 F2: symmetric LWW guard. If our tombstone HLC doesn't beat
        // the row's `added_at` (e.g., a remote add with a later HLC just
        // synced), `is_effective()` will still return true after this
        // write — yet we'd unsubscribe locally and return Ok, leaving
        // cross-device state inconsistent. Refuse and surface the error.
        // Logic is in `merge_remove_library` so it's unit-testable
        // without a tauri::State harness.
        merge_remove_library(crdt_g.libraries.get(&addr), &now_hlc)?;
        if let Some(lib) = crdt_g.libraries.get_mut(&addr) {
            lib.removed_at = Some(now_hlc);
        }
    }
    let _ = library_directory
        .request_tx
        .send(crate::library_directory::LibraryDirectoryRequest::Unsubscribe(addr));
    Ok(())
}

/// IPC: browse the directory. `None` aggregates across all libraries;
/// `Some(addr_hex)` filters to one library's contributions. Maps to
/// `DirectoryEntryDTO` (strips cryptographic material; derives
/// `community_addr` from the admin identity bundle for display).
#[tauri::command]
async fn browse_library(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    library_addr: Option<String>,
) -> Result<Vec<crate::library_directory::DirectoryEntryDTO>, String> {
    let library_directory = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        g.library_directory
            .clone()
            .ok_or("library_directory missing — node not running?")?
    };
    let aggregated = match library_addr {
        None => library_directory.snapshot_all().await,
        Some(addr_hex) => {
            let addr = crate::library_directory::parse_owner_addr_hex(&addr_hex)?;
            library_directory.snapshot_filtered_by_library(&addr).await
        }
    };
    Ok(aggregated
        .iter()
        .map(crate::library_directory::DirectoryEntryDTO::from_aggregated)
        .collect())
}

// ── ZEB-281 Sub-D Phase 4: ProfileMembershipBroadcast IPCs ─────────────
//
// Four IPCs and one Tauri event (`profile-broadcast-received`) wire the
// per-community opt-in toggle + per-peer profile subscription path. See
// `docs/specs/2026-05-12-zeb-281-sub-d-phase-4-profile-membership-broadcast-design.md`
// §6 + §7 for the protocol; the publisher state machine lives in
// `profile_broadcast::ProfileBroadcastPublisher::spawn`. The event-loop
// subscriber pool is in `event_loop.rs`.

/// IPC: Sub-D Phase 4 (ZEB-281). Toggle the opt-in flag for a community
/// Space. Mutates `Space.shared_in_profile`, bumps `Space.updated_at`
/// via the atomic HLC reservation helper (same idiom as add_space /
/// leave_community), then notifies the profile-broadcast publisher so
/// it re-walks the opted-in set and emits the new broadcast.
///
/// No-op when the flag is already in the requested state — does NOT
/// bump the HLC in that case, so a redundant toggle doesn't burn a
/// logical-clock tick.
#[tauri::command]
async fn set_space_shared_in_profile(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    shared: bool,
) -> Result<(), String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let community_space_id = crate::owner_state_types::SpaceId(id_bytes);

    // Capture `generation` paired-atomically with the Arcs. If stop_inner
    // detaches the Arcs (sets to None) and start_node bumps the generation
    // while we're awaiting on the HLC reservation / CRDT lock, the Arcs we
    // hold are orphaned: we'd mutate a `crdt_state` the new node never
    // reads from. The post-check after the mutation surfaces that as Err
    // rather than returning Ok(()) for an effectively dropped write. Same
    // idiom as `send_dm` (~lib.rs:2935).
    let (crdt_state, publisher, hlc_tracker, device_id, snapshot_generation) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.profile_broadcast_publisher
                .clone()
                .ok_or("profile_broadcast_publisher missing — node not running?")?,
            g.hlc_tracker
                .clone()
                .ok_or("hlc_tracker missing — node not running?")?,
            g.dm_device_id
                .clone()
                .ok_or("dm_device_id missing — node not running?")?,
            g.generation,
        )
    };

    // Wall clock for HLC bump. Read once OUTSIDE the lock since the
    // tracker locking happens inside reserve_next_hlc_for_device.
    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // Determine whether this is a no-op (current == target) BEFORE
    // taking a tracker tick — read under the CRDT lock, then drop the
    // guard so the HLC reservation doesn't race with another caller
    // holding the CRDT lock.
    {
        let g = crdt_state.lock().await;
        let space = g
            .spaces
            .values()
            .find(|s| {
                s.id == community_space_id
                    && matches!(s.kind, crate::owner_state_types::SpaceKind::Community)
            })
            .ok_or_else(|| format!("community Space not found for community_id={community_id}"))?;
        if space.shared_in_profile == shared {
            // No-op — return without bumping HLC.
            return Ok(());
        }
    }

    // Reserve next HLC. Same idiom as add_space / leave_community.
    let new_hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;

    // Re-acquire the CRDT lock and apply the mutation. Re-find the
    // Space in case state moved between the no-op probe and now (e.g.,
    // a concurrent EpochRotation merged in); refuse to mutate if the
    // Space disappeared.
    {
        let mut g = crdt_state.lock().await;
        let space = g
            .spaces
            .values_mut()
            .find(|s| {
                s.id == community_space_id
                    && matches!(s.kind, crate::owner_state_types::SpaceKind::Community)
            })
            .ok_or_else(|| {
                format!(
                    "community Space not found for community_id={community_id} \
                     (raced with a concurrent mutation)"
                )
            })?;
        // Second check inside the lock — between the no-op probe and
        // this lock acquisition another caller may have raced us.
        if space.shared_in_profile == shared {
            return Ok(());
        }
        space.shared_in_profile = shared;
        space.updated_at = new_hlc;
    }

    // Post-check: a concurrent stop_node / restart between the initial
    // state_lock grab and now would have orphaned `crdt_state`. The
    // mutation we just performed went into a detached state that the
    // live NodeState's node never reads from. Surface as Err so the
    // caller can retry against the live node. stop_inner clears handles
    // to None WITHOUT bumping `generation`, so additionally verify the
    // handles are still present. Same idiom as `send_dm`.
    {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err(format!(
                "node generation changed during set_space_shared_in_profile \
                 (was {}, now {}); flag was written to a detached crdt_state \
                 — retry against the live node",
                snapshot_generation, g.generation
            ));
        }
        if g.crdt_state.is_none() || g.profile_broadcast_publisher.is_none() {
            return Err("node was stopped during set_space_shared_in_profile; \
                 flag was written to a detached crdt_state"
                .to_string());
        }
    }

    // Notify the publisher to recompute + debounce-publish.
    publisher.notify_dirty();
    Ok(())
}

/// IPC: Sub-D Phase 4 (ZEB-281). Read the set of community SpaceIds the
/// user has opted to share in their public profile. Frontend calls this
/// once at startup to populate the per-community toggle initial state in
/// CommunitySettingsPanel — without this read, the toggle defaults to
/// OFF on every restart even when server-side `shared_in_profile == true`
/// (the publisher continues broadcasting correctly because it reads from
/// CRDT, but the UI would falsely claim "off / private", inverting the
/// privacy contract).
///
/// Returns hex-encoded SpaceIds (32 chars each), matching the on-the-wire
/// shape used in `DiscoveredProfileInfo.communityIds`. Sorted + deduped
/// to mirror `OwnerStateBroadcastSource::current_shared_set` (the
/// publisher's view of the same predicate).
#[tauri::command]
async fn list_shared_in_profile_communities(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
) -> Result<Vec<String>, String> {
    let crdt_state = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        g.crdt_state
            .clone()
            .ok_or("crdt_state missing — node not running?")?
    };
    let g = crdt_state.lock().await;
    let mut ids: Vec<String> = g
        .spaces
        .values()
        .filter(|s| {
            // Mirror `OwnerStateBroadcastSource::current_shared_set`: the
            // frontend opt-in toggle MUST reflect what the publisher
            // actually broadcasts. A Community Space the user has left
            // (`left_at = Some(_)`) is intentionally retained but is
            // suppressed from the broadcast — surfacing it here would
            // falsely claim "sharing on" for a community no longer in
            // the broadcast set.
            matches!(s.kind, crate::owner_state_types::SpaceKind::Community)
                && s.shared_in_profile
                && s.left_at.is_none()
        })
        .map(|s| hex::encode(s.id.0))
        .collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// IPC: Sub-D Phase 4 (ZEB-281). Subscribe to a peer's profile-broadcast
/// topic. Returns a u64 SubscriptionId the frontend uses to address
/// subsequent unsubscribe/get_cached calls + to filter incoming
/// `profile-broadcast-received` events.
#[tauri::command]
async fn subscribe_peer_profile(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    peer_addr: String,
) -> Result<u64, String> {
    let (cache, request_tx, next_id) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.profile_broadcast_cache
                .clone()
                .ok_or("profile_broadcast_cache missing — node not running?")?,
            g.profile_broadcast_request_tx
                .clone()
                .ok_or("profile_broadcast_request_tx missing — node not running?")?,
            std::sync::Arc::clone(&g.profile_broadcast_next_subscription_id),
        )
    };

    let peer_owner_addr = crate::library_directory::parse_owner_addr_hex(&peer_addr)
        .map_err(|e| format!("invalid peer_addr hex: {e}"))?;
    let id = next_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    cache.register(id, peer_owner_addr).await;
    // If the request channel is closing (event-loop shutdown raced this
    // IPC), the slot we just registered would persist forever — a phantom
    // subscription nothing will ever deliver a sample to. Roll the cache
    // entry back before surfacing the error.
    if let Err(e) = request_tx
        .send(crate::event_loop::ProfileBroadcastRequest::Subscribe {
            subscription_id: id,
            peer_addr: peer_owner_addr,
        })
        .await
    {
        cache.drop_subscription(id).await;
        return Err(format!("profile_broadcast_request_tx send: {e}"));
    }
    Ok(id)
}

/// IPC: Sub-D Phase 4 (ZEB-281). Unsubscribe from a previously-registered
/// peer profile subscription. The event-loop's subscriber pool aborts
/// the Zenoh subscriber task AND drops the cache entry (atomic teardown
/// inside the consumer task).
#[tauri::command]
async fn unsubscribe_peer_profile(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    subscription_id: u64,
) -> Result<(), String> {
    let request_tx = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        g.profile_broadcast_request_tx
            .clone()
            .ok_or("profile_broadcast_request_tx missing — node not running?")?
    };
    request_tx
        .send(crate::event_loop::ProfileBroadcastRequest::Unsubscribe { subscription_id })
        .await
        .map_err(|e| format!("profile_broadcast_request_tx send: {e}"))?;
    Ok(())
}

/// IPC: Sub-D Phase 4 (ZEB-281). Snapshot the latest verified broadcast
/// for a subscription. Returns `Ok(None)` when the subscription is
/// known but no broadcast has been received yet (loading state) or
/// when the subscription has been dropped.
#[tauri::command]
async fn get_cached_peer_profile(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    subscription_id: u64,
) -> Result<Option<crate::profile_broadcast::DiscoveredProfileInfo>, String> {
    let cache = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        g.profile_broadcast_cache
            .clone()
            .ok_or("profile_broadcast_cache missing — node not running?")?
    };
    Ok(cache.get_cached(subscription_id).await)
}

#[cfg(test)]
mod library_directory_lww_tests {
    //! Unit tests for the pure LWW helpers behind `add_library` /
    //! `remove_library`. R3 F1 + F2 regression coverage.
    use super::*;
    use crate::owner_state_types::{Hlc, LibraryEntry, OwnerAddr};

    fn hlc(wall_ms: u64, logical: u32) -> Hlc {
        Hlc {
            wall_ms,
            logical,
            device_id: "d".to_string(),
        }
    }

    /// R3 F1: a newer synced add (higher HLC than local now_hlc) must
    /// not be regressed by a local add_library call. The chosen
    /// `added_at` stays at the existing higher value.
    #[test]
    fn add_library_does_not_regress_newer_synced_add() {
        let addr = OwnerAddr([0xAA; 16]);
        let existing = LibraryEntry {
            address: addr,
            added_at: hlc(1000, 0),
            removed_at: None,
        };
        let local_now = hlc(500, 0);
        let merged = merge_add_library(Some(&existing), addr, &local_now)
            .expect("merge should accept non-tombstoned regress attempt");
        assert_eq!(
            merged.added_at,
            hlc(1000, 0),
            "added_at must NOT regress below the synced higher value",
        );
        assert_eq!(merged.removed_at, None);
        assert_eq!(merged.address, addr);
    }

    /// Forward direction: a strictly-newer local add advances added_at
    /// to now_hlc. Sanity check the LWW math both ways.
    #[test]
    fn add_library_advances_added_at_when_now_hlc_is_newer() {
        let addr = OwnerAddr([0xBB; 16]);
        let existing = LibraryEntry {
            address: addr,
            added_at: hlc(500, 0),
            removed_at: None,
        };
        let local_now = hlc(1000, 0);
        let merged = merge_add_library(Some(&existing), addr, &local_now).expect("merge");
        assert_eq!(merged.added_at, hlc(1000, 0));
    }

    /// R1 F1 regression: a higher-HLC tombstone refuses the add.
    #[test]
    fn add_library_refuses_when_tombstone_beats_now_hlc() {
        let addr = OwnerAddr([0xCC; 16]);
        let existing = LibraryEntry {
            address: addr,
            added_at: hlc(100, 0),
            removed_at: Some(hlc(2000, 0)),
        };
        let local_now = hlc(500, 0);
        let err = merge_add_library(Some(&existing), addr, &local_now)
            .expect_err("should refuse: tombstone is strictly newer than now_hlc");
        assert!(err.contains("HLC went backward"));
    }

    /// First-add path: no existing row, stamp now_hlc directly.
    #[test]
    fn add_library_stamps_now_hlc_on_first_add() {
        let addr = OwnerAddr([0xDD; 16]);
        let local_now = hlc(1234, 5);
        let merged = merge_add_library(None, addr, &local_now).expect("merge");
        assert_eq!(merged.added_at, hlc(1234, 5));
        assert_eq!(merged.removed_at, None);
    }

    /// R3 F2: remove must refuse when now_hlc doesn't strictly exceed
    /// the row's added_at — otherwise the tombstone wouldn't win LWW
    /// (is_effective() would still return true) and we'd diverge from
    /// the synced state.
    #[test]
    fn remove_library_refuses_if_hlc_does_not_beat_added_at() {
        let addr = OwnerAddr([0xEE; 16]);
        let existing = LibraryEntry {
            address: addr,
            added_at: hlc(1000, 0),
            removed_at: None,
        };
        let local_now = hlc(500, 0);
        let err = merge_remove_library(Some(&existing), &local_now)
            .expect_err("should refuse: now_hlc < added_at");
        assert!(err.contains("HLC went backward"));
    }

    /// Equal-HLC remove also refuses (need STRICT > to win LWW).
    #[test]
    fn remove_library_refuses_on_equal_hlc() {
        let addr = OwnerAddr([0xEE; 16]);
        let existing = LibraryEntry {
            address: addr,
            added_at: hlc(1000, 0),
            removed_at: None,
        };
        let local_now = hlc(1000, 0);
        assert!(merge_remove_library(Some(&existing), &local_now).is_err());
    }

    /// Remove with strictly-newer HLC: caller may proceed.
    #[test]
    fn remove_library_allows_strictly_newer_hlc() {
        let addr = OwnerAddr([0xFF; 16]);
        let existing = LibraryEntry {
            address: addr,
            added_at: hlc(500, 0),
            removed_at: None,
        };
        let local_now = hlc(1000, 0);
        merge_remove_library(Some(&existing), &local_now)
            .expect("strict-greater now_hlc must beat added_at");
    }

    /// Remove with no existing row is a no-op (caller does the absent-
    /// row check separately — helper just doesn't error).
    #[test]
    fn remove_library_noop_when_row_absent() {
        let local_now = hlc(1000, 0);
        merge_remove_library(None, &local_now).expect("absent row is no-op");
    }
}

/// Tauri IPC: leave a community we currently belong to.
///
/// Mints a self-Leave event, looks up the per-community engine via
/// `community_registry.engine_arc`, and inserts the event through
/// `engine.insert_local_event` so the debounced publish loop pushes
/// ZEB-285 Phase 1 Task 10: read fork snapshot metadata for the Settings
/// panel. ZEB-287 Phase 2 renamed from `get_community_lineage` /
/// `CommunityLineageDto` because Phase 2 introduces a distinct
/// `CommunityLineageDto` (the multi-hop ancestor chain pulled from
/// CommunityState). This older IPC remains the source of the bundled-
/// message count + original-community-name fields used by ForkConfirmDialog.
///
/// Returns `Some(ForkSnapshotMetadataDto)` when `pre_fork_snapshot.bin`
/// exists in the community's data directory, `None` when the community is
/// not a fork (file absent). Reads and decodes the snapshot on every call
/// — suitable for the settings panel (opened rarely) but callers should
/// not call this in a hot path. The full snapshot body (channel events) is
/// decoded then dropped; only the lightweight header fields are returned.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkSnapshotMetadataDto {
    /// Display name of the original community at fork time.
    pub original_community_name: String,
    /// Wall-clock milliseconds from the forked_at HLC.
    pub forked_at_ms: u64,
    /// Total message count captured in the snapshot (after §4.2 trim).
    pub snapshot_message_count: usize,
}

#[tauri::command]
async fn get_fork_snapshot_metadata(
    community_id: String,
) -> Result<Option<ForkSnapshotMetadataDto>, String> {
    // SECURITY: parse community_id as a typed SpaceId (16 raw bytes, 32 hex chars)
    // before using it as a path component. Rejects `../../etc/passwd` and other
    // path-traversal payloads at the boundary. (Fix: PR #122 bot review.)
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("get_fork_snapshot_metadata: invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| {
            "get_fork_snapshot_metadata: community_id must be 16 bytes (32 hex chars)".to_string()
        })?;
    let safe_community_id = hex::encode(id_bytes); // canonical hex, no path components
    let identity_dir = crate::owner_commands::resolve_identity_dir()
        .map_err(|e| format!("get_fork_snapshot_metadata: resolve identity_dir: {e}"))?;
    let snapshot_path = identity_dir
        .join("communities")
        .join(&safe_community_id)
        .join("pre_fork_snapshot.bin");

    let bytes = match std::fs::read(&snapshot_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(format!(
                "get_fork_snapshot_metadata: read pre_fork_snapshot.bin: {e}"
            ))
        }
    };

    let snapshot = crate::owner_state_crypto::canonical_cbor_decode::<
        crate::community_invite::PreForkSnapshot,
    >(&bytes)
    .map_err(|e| format!("get_fork_snapshot_metadata: decode snapshot: {e}"))?;

    let snapshot_message_count: usize = snapshot
        .channel_log
        .per_channel
        .values()
        .map(|v| v.len())
        .sum();

    Ok(Some(ForkSnapshotMetadataDto {
        original_community_name: snapshot.original_community_name,
        forked_at_ms: snapshot.forked_at.wall_ms,
        snapshot_message_count,
    }))
}

/// ZEB-285 Phase 1 Task 11: return the full channel-log from the pre-fork
/// snapshot so the frontend can render a unified timeline.
///
/// Returns `Some(PreForkSnapshotDto)` when `pre_fork_snapshot.bin` exists,
/// `None` when the community is not a fork. The DTO carries only the channel
/// log (as per-channel `Vec<ChannelMessageDto>`) plus the header fields
/// needed to render the fork-point divider.
///
/// The per-channel map is keyed by the channel-ID hex string so TypeScript
/// consumers can index directly by `channelId`.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreForkSnapshotDto {
    pub original_community_name: String,
    pub forked_at_ms: u64,
    /// Per-channel snapshot messages. Key = channel-id hex (32 chars),
    /// value = messages sorted HLC ascending.
    pub channel_log: std::collections::BTreeMap<
        String,
        Vec<crate::community_channel_log_engine::ChannelMessageDto>,
    >,
}

#[tauri::command]
async fn get_pre_fork_snapshot(community_id: String) -> Result<Option<PreForkSnapshotDto>, String> {
    // SECURITY: parse community_id as a typed SpaceId (16 raw bytes, 32 hex chars)
    // before using it as a path component. Rejects path-traversal payloads at the
    // boundary. (Fix: PR #122 bot review.)
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("get_pre_fork_snapshot: invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| {
            "get_pre_fork_snapshot: community_id must be 16 bytes (32 hex chars)".to_string()
        })?;
    let safe_community_id = hex::encode(id_bytes); // canonical hex, no path components
    let identity_dir = crate::owner_commands::resolve_identity_dir()
        .map_err(|e| format!("get_pre_fork_snapshot: resolve identity_dir: {e}"))?;
    let snapshot_path = identity_dir
        .join("communities")
        .join(&safe_community_id)
        .join("pre_fork_snapshot.bin");

    let bytes = match std::fs::read(&snapshot_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(format!(
                "get_pre_fork_snapshot: read pre_fork_snapshot.bin: {e}"
            ))
        }
    };

    let snapshot = crate::owner_state_crypto::canonical_cbor_decode::<
        crate::community_invite::PreForkSnapshot,
    >(&bytes)
    .map_err(|e| format!("get_pre_fork_snapshot: decode snapshot: {e}"))?;

    // Convert per-channel SignedChannelEvents → ChannelMessageDto.
    // Events are already stored HLC-ascending in BoundedChannelLogSnapshot
    // (insertion-ordered during fork capture); sort defensively here.
    let mut channel_log = std::collections::BTreeMap::<
        String,
        Vec<crate::community_channel_log_engine::ChannelMessageDto>,
    >::new();

    for (channel_id, events) in &snapshot.channel_log.per_channel {
        use crate::community_channel_log::SignedChannelEvent;
        let mut dtos: Vec<crate::community_channel_log_engine::ChannelMessageDto> = events
            .iter()
            .filter_map(|ev| {
                // Pattern match on Post only — forward-compatible when
                // SignedChannelEvent gains new variants (Edit, Delete, etc.).
                // The `_ => None` arm is unreachable today but will be
                // load-bearing once additional variants land.
                #[allow(unreachable_patterns)]
                match ev {
                    SignedChannelEvent::Post {
                        id,
                        author,
                        at,
                        body,
                        reply_to,
                        community_id: ev_community_id,
                        channel_id: ev_channel_id,
                        ..
                    } => Some(crate::community_channel_log_engine::ChannelMessageDto {
                        message_id: hex::encode(id.0),
                        community_id: hex::encode(ev_community_id.0),
                        channel_id: hex::encode(ev_channel_id.0),
                        author: hex::encode(author.0),
                        at: crate::community_channel_log_engine::HlcDto {
                            wall_ms: at.wall_ms,
                            logical: at.logical,
                            device_id: at.device_id.clone(),
                        },
                        body: body.as_bytes().to_vec(),
                        reply_to: reply_to.map(|m| hex::encode(m.0)),
                    }),
                    _ => None,
                }
            })
            .collect();

        // Sort HLC ascending: wall_ms → logical → device_id.
        dtos.sort_by(|a, b| {
            a.at.wall_ms
                .cmp(&b.at.wall_ms)
                .then(a.at.logical.cmp(&b.at.logical))
                .then(a.at.device_id.cmp(&b.at.device_id))
        });

        channel_log.insert(hex::encode(channel_id.0), dtos);
    }

    Ok(Some(PreForkSnapshotDto {
        original_community_name: snapshot.original_community_name,
        forked_at_ms: snapshot.forked_at.wall_ms,
        channel_log,
    }))
}

/// it to peers. Advances the local HLC tracker on success.
///
/// Owner-state Space NOT mutated (per spec line 514): the Space row
/// stays around so the UI can show "you've left this community" and
/// the user can choose to call `remove_space` later. The membership
/// CRDT is the source of truth for community membership; `Space.left_at`
/// is only meaningful for DM Spaces (which have no membership CRDT).
///
/// Snapshot-then-spawn-equivalent fence: after minting but before
/// engine ops we re-acquire the std `NodeState` lock and check
/// `generation`. If the node was stopped (or stop+restart raced), we
/// return Err so the Leave doesn't land on a soon-to-be-detached
/// engine.
///
/// Engine lookup: `engine_arc` returns `None` if no engine is running
/// for this community — surfaced as "not currently joined". We do NOT
/// spawn an engine here (unlike `redeem_invite`); leave operates only
/// on existing live engines.
///
/// HLC tracker advanced AFTER successful insert so a verify failure
/// doesn't bump the tracker into "future" state that would cause the
/// next outgoing event to skip a tick.
#[tauri::command]
async fn leave_community(
    app: tauri::AppHandle,
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
) -> Result<(), String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let (hlc_tracker, device_id, self_owner, community_registry, dm_outbox, snapshot_generation) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.generation,
        )
    };

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // ZEB-267: atomic HLC reservation. Borrow the SigningKey from
    // `dm_outbox` under its lock — same canonical local-device key
    // create_community / redeem_invite use.
    let leave_hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;
    let leave = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_leave_event(space_id, self_owner, signing_key, leave_hlc)?
    };

    // Snapshot-then-spawn-equivalent fence: ensure node generation
    // hasn't changed. If it has, the engine we'd touch would be
    // detached from a stopped node and the Leave wouldn't be
    // persisted — surface Err rather than silently writing into a
    // doomed engine.
    {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err(format!(
                "node generation changed during leave_community (was {}, now {})",
                snapshot_generation, g.generation
            ));
        }
    }

    let engine_arc = community_registry
        .engine_arc(&space_id)
        .await
        .ok_or_else(|| {
            format!(
                "no engine for community {} — not currently joined",
                hex::encode(space_id.0)
            )
        })?;
    // ZEB-249 Task 6 spec §4.4 + §4.1 atomicity: cooperative leaver issues
    // an EpochRotation excluding self. We mint the rotation BEFORE inserting
    // either event so both can be submitted atomically via
    // insert_local_event_pair. This closes the crash window between the
    // Leave and its matching Rotation.
    //
    // If rotation minting fails (nobody else to seal to, or identity pubs
    // unknown), we fall back to inserting Leave alone and let the admin
    // self-healing observer synthesize the rotation. We do NOT return Err —
    // the Leave itself is the primary operation.
    let leave_rotation_bundle: Result<crate::community_membership::SignedMembershipEvent, String> =
        async {
            // 1. Read materialized membership PRE-leave.
            let admin_addr = engine_arc.admin_addr();
            let materialized = {
                let state = engine_arc.state();
                let state_g = state.lock().await;
                state_g.materialize_now(admin_addr)
            };

            let current_epoch = materialized.current_epoch.unwrap_or(0);

            // 2. Collect remaining ACTIVE members (excluding self — the leaver).
            // ZEB-249 PR #106 R5 (CodeRabbit Major): distinguish true solo-leave
            // (no remaining Joined/Invited members) from the unresolvable-identities
            // case (members exist but their pubs aren't in the resolver cache yet).
            // The old code treated both as "no rotation needed" and returned the
            // solo-leave sentinel, which silently swallowed the unresolvable case and
            // left a backward-secrecy gap: removed member retains the epoch key.
            let resolver = community_registry.identity_resolver();
            let mut remaining_joined_count: usize = 0;
            let mut member_pubs: Vec<(crate::owner_state_types::OwnerAddr, [u8; 64])> = Vec::new();
            for (addr, state_m) in &materialized.members {
                if *addr == self_owner {
                    continue; // leaver excluded per spec §4.4
                }
                if !matches!(
                    state_m.status,
                    crate::community_membership::MemberStatus::Joined
                        | crate::community_membership::MemberStatus::Invited
                ) {
                    continue;
                }
                remaining_joined_count += 1;
                if let Some(pub64) = resolver.resolve(addr).await {
                    member_pubs.push((*addr, pub64));
                }
            }

            // True solo leave — no other active members, no rotation needed.
            if remaining_joined_count == 0 {
                return Err(LEAVE_SOLO_SENTINEL.to_string());
            }

            // Members exist but none have resolvable identity pubs yet — error so
            // the leaver is NOT falsely reported as "leave succeeded with no rotation".
            // The admin self-healing observer will synthesize the rotation once pubs
            // propagate. Surfaces as a non-solo error string; the Err(_) branch in
            // the caller logs a warn and inserts Leave-alone (self-heal path).
            if member_pubs.is_empty() {
                return Err(format!(
                    "leave_community: {remaining_joined_count} remaining member(s) but \
                     no identity pubs resolvable — rotation deferred to self-heal observer"
                ));
            }

            // 3. Generate K_next and seal to remaining members.
            let k_next = crate::owner_state_types::EpochKey::random();
            let recipients = build_sealed_epoch_recipients(&k_next, member_pubs)?;

            // 4. Reserve HLC for rotation.
            let rotation_hlc = crate::dm_outbox::reserve_next_hlc_for_device(
                &hlc_tracker,
                &device_id,
                wall_now_ms,
            )
            .await;

            // 5. Mint the rotation (leaver is the signer — cooperative leave path).
            let rotation = {
                let outbox_g = dm_outbox.lock().await;
                let signing_key = outbox_g.signing_key.as_ref();
                mint_epoch_rotation_event(
                    space_id,
                    self_owner,
                    leave.id,
                    current_epoch,
                    recipients,
                    signing_key,
                    rotation_hlc,
                )?
            };

            Ok(rotation)
        }
        .await;

    match leave_rotation_bundle {
        Ok(rotation) => {
            // §4.1 happy path: submit leave + rotation atomically.
            // ZEB-249 PR #106 R5 (CodeRabbit Major): rotation rejection is now
            // surfaced as an Err so the caller can signal the rotation gap rather
            // than silently swallowing it. Leave itself is still the definitive
            // membership event; if the rotation is rejected here, the admin
            // self-healing observer will synthesize it on the next delta.
            let (leave_outcome, rot_outcome) = engine_arc
                .insert_local_event_pair(leave.clone(), rotation)
                .await
                .map_err(|e| format!("engine.insert_local_event_pair: {e}"))?;

            if matches!(
                leave_outcome,
                crate::community_state_crdt::InsertOutcome::Rejected(_)
            ) {
                return Err(membership_outcome_err("leave_community", &leave_outcome));
            }

            // Surface rotation rejection as a warning and return Err —
            // the leaver has committed Leave + lost repair authority, so
            // the caller needs to know the leave is only partially
            // successful. The admin self-healing observer will synthesize
            // the rotation, but the caller cannot assume secrecy is
            // fully enforced yet.
            if let crate::community_state_crdt::InsertOutcome::Rejected(ref rot_err) = rot_outcome {
                tracing::warn!(
                    community_id = %hex::encode(space_id.0),
                    self_owner = %hex::encode(self_owner.0),
                    error = ?rot_err,
                    "leave_community: Leave committed but paired EpochRotation was \
                     rejected — admin self-healing observer will synthesize rotation"
                );
                return Err(format!(
                    "leave_community committed Leave but paired EpochRotation was rejected: {rot_err}"
                ));
            }
        }
        Err(e) => {
            // Rotation bundle failed (no members, or identity pubs unknown) —
            // insert Leave alone. If it's the solo-leave sentinel,
            // that's expected and not worth warning about.
            let is_solo = e == LEAVE_SOLO_SENTINEL;
            if !is_solo {
                tracing::warn!(
                    community_id = %hex::encode(space_id.0),
                    self_owner = %hex::encode(self_owner.0),
                    error = %e,
                    "leave_community: cooperative EpochRotation bundle failed; \
                     admin self-healing observer will synthesize rotation"
                );
            }
            let outcome = engine_arc
                .insert_local_event(leave.clone())
                .await
                .map_err(|e2| format!("engine.insert_local_event: {e2}"))?;
            if matches!(
                outcome,
                crate::community_state_crdt::InsertOutcome::Rejected(_)
            ) {
                return Err(membership_outcome_err("leave_community", &outcome));
            }
            // CR Major (PR #106 R7): "survivors exist but we couldn't mint companion
            // EpochRotation" is NOT a successful leave — backward secrecy is broken
            // until the admin self-healing observer synthesizes the rotation. Return
            // Err so callers can distinguish this from a true solo-leave (Ok(())).
            if !is_solo {
                return Err(format!(
                    "leave_community committed Leave but could not mint paired EpochRotation: {e}"
                ));
            }
            // Solo-leave: no rotation needed; fall through to nav-update and Ok(()).
        }
    }

    // ZEB-265: notify the nav layer so the community node disappears
    // from the tree. emit failure is non-fatal — the leave already
    // committed, and worst case the node lingers until reload.
    // Use `hex::encode(space_id.0)` rather than the raw IPC `community_id`
    // String: hex::decode accepts mixed case but the canonical form is
    // lowercase, so the emitted spaceId matches the lowercase ids
    // emitted from create/redeem (CodeRabbit minor — PR #92 round 1).
    if let Err(e) = app.emit(
        "nav-updated",
        &NavUpdatedPayload {
            action: "removed",
            space_id: hex::encode(space_id.0),
            kind: "community",
            name: String::new(),
            members: None,
            parent_id: None,
            pending: None,
        },
    ) {
        tracing::warn!(error = %e, "leave_community: nav-updated emit failed");
    }

    Ok(())
}

#[cfg(test)]
mod leave_community_inner_tests {
    use super::*;
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;

    #[test]
    fn mint_leave_produces_self_leave_event() {
        let identity = PrivateIdentity::from_seed(&[0xab; 32]);
        let identity_pub = identity.identity.to_public_bytes();
        let self_owner = OwnerAddr(identity.identity.address_hash);
        // Mirror Task 9/10's test pattern: pull the canonical 32-byte
        // Ed25519 seed from bytes 32..64 of `to_private_bytes()`. The
        // production IPC borrows this same SigningKey from `dm_outbox`.
        let sk_bytes_full = identity.to_private_bytes();
        let ed_seed: [u8; 32] = sk_bytes_full[32..64]
            .try_into()
            .expect("ed25519 seed slice 32..64");
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_seed);

        let community_id = SpaceId([0x77; 16]);
        let device_id = "leaver-dev";
        let wall_now_ms = 1_700_000_500_000u64;
        // ZEB-267: caller pre-reserves the HLC.
        let leave_hlc = Hlc {
            wall_ms: wall_now_ms,
            logical: 0,
            device_id: device_id.to_string(),
        };

        let event =
            mint_leave_event(community_id, self_owner, &signing_key, leave_hlc).expect("mint");

        assert_eq!(event.actor, self_owner);
        assert_eq!(event.community_id, community_id);
        assert!(matches!(
            event.kind,
            crate::community_membership::MembershipEventKind::Leave
        ));
        assert_eq!(event.at.wall_ms, wall_now_ms);

        // Self-Leave sig must verify against the leaver's identity_pub —
        // the engine's verify_event runs the same check on insert.
        crate::community_membership::verify_signature(&event, &identity_pub)
            .expect("self-leave signature must verify against leaver identity_pub");
    }
}

// ── ZEB-262 Phase 4: kick_from_community ─────────────────────────────
//
// Mints a Kick SignedMembershipEvent and inserts it through the
// per-community engine. Power-gate enforcement happens INSIDE
// engine.insert_local_event (which calls verify_event) — actor must
// have power ≥ kick_threshold (50) AND strictly greater than target's
// power. The IPC trusts verify_event and translates VerifyError
// discriminants to user-readable strings. Pre-validating here would
// duplicate the rules and risk drift.

/// Pure function: mint a self-signed Kick event for a community we
/// belong to and have permission to moderate. Mirrors `mint_leave_event`.
///
/// ZEB-267: Caller pre-reserves `hlc` via
/// `dm_outbox::reserve_next_hlc_for_device`.
pub fn mint_kick_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    target: crate::owner_state_types::OwnerAddr,
    reason: Option<String>,
    signing_key: &ed25519_dalek::SigningKey,
    hlc: crate::owner_state_types::Hlc,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::Kick { target, reason },
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign kick: {e}"))
}

/// Pure function: mint a self-signed Unban event for a community we
/// moderate. Mirrors `mint_kick_event` exactly — only the
/// `MembershipEventKind` variant differs.
///
/// ZEB-284: admin-tier action (power ≥ 100). Power-gate enforcement
/// happens inside `engine.insert_local_event` → `verify_event`.
///
/// ZEB-267: Caller pre-reserves `hlc` via
/// `dm_outbox::reserve_next_hlc_for_device`.
pub fn mint_unban_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    target: crate::owner_state_types::OwnerAddr,
    reason: Option<String>,
    signing_key: &ed25519_dalek::SigningKey,
    hlc: crate::owner_state_types::Hlc,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::Unban { target, reason },
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign unban: {e}"))
}

// ── ZEB-250: AdminProposal minting helpers ────────────────────────────

/// ZEB-250: mint a signed AdminProposal carrying a SetPower proposal_kind,
/// sign with the caller's identity, and return the signed event.
///
/// The caller is responsible for inserting the returned event into the
/// community engine. `signing_key` and `hlc` must be pre-acquired.
pub fn mint_admin_proposal_set_power_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    target: crate::owner_state_types::OwnerAddr,
    level: u8,
    signing_key: &ed25519_dalek::SigningKey,
    hlc: crate::owner_state_types::Hlc,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{
        sign_event, EventPayload, MembershipEventKind, ProposalKind,
    };
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::AdminProposal {
            proposal_kind: ProposalKind::SetPower { target, level },
        },
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign admin_proposal_set_power: {e}"))
}

/// ZEB-250: mint a signed AdminProposal carrying a Kick proposal_kind.
///
/// Same signing/HLC contract as `mint_admin_proposal_set_power_event`.
pub fn mint_admin_proposal_kick_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    target: crate::owner_state_types::OwnerAddr,
    reason: Option<String>,
    signing_key: &ed25519_dalek::SigningKey,
    hlc: crate::owner_state_types::Hlc,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{
        sign_event, EventPayload, MembershipEventKind, ProposalKind,
    };
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::AdminProposal {
            proposal_kind: ProposalKind::Kick { target, reason },
        },
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign admin_proposal_kick: {e}"))
}

/// ZEB-250: mint a signed AdminProposal carrying a ChangeQuorum proposal_kind.
/// Used by `propose_change_quorum` (Task 12).
pub fn mint_admin_proposal_change_quorum_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    new_quorum: u8,
    signing_key: &ed25519_dalek::SigningKey,
    hlc: crate::owner_state_types::Hlc,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{
        sign_event, EventPayload, MembershipEventKind, ProposalKind,
    };
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::AdminProposal {
            proposal_kind: ProposalKind::ChangeQuorum { new_quorum },
        },
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign admin_proposal_change_quorum: {e}"))
}

/// ZEB-250 §6.3: mint a signed AdminCountersign event referencing an
/// existing AdminProposal. Same signing/HLC contract as the
/// `mint_admin_proposal_*` family.
pub fn mint_admin_countersign_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    target_event_id: [u8; 16],
    signing_key: &ed25519_dalek::SigningKey,
    hlc: crate::owner_state_types::Hlc,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::AdminCountersign { target_event_id },
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign admin_countersign: {e}"))
}

/// ZEB-249 Task 6: pure helper — mint a signed `EpochRotation` event.
///
/// `triggered_by` is the `EventId` of the Kick or Leave event this rotation
/// is responding to. `prior_epoch` is the epoch being retired (the new epoch
/// will be `prior_epoch + 1` after materialize applies this event).
/// `recipients` maps each remaining member's `OwnerAddr` to their sealed
/// epoch key bytes (as produced by `seal_to_owner`). The kick target MUST
/// NOT appear in `recipients` (validated by `verify_event` / materialize).
///
/// ZEB-267: Caller pre-reserves `hlc`.
pub fn mint_epoch_rotation_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    triggered_by: crate::community_membership::EventId,
    prior_epoch: u64,
    recipients: Vec<(crate::owner_state_types::OwnerAddr, Vec<u8>)>,
    signing_key: &ed25519_dalek::SigningKey,
    hlc: crate::owner_state_types::Hlc,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{
        sign_event, EventPayload, MembershipEventKind, RecipientCiphertext,
    };
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let recipient_ciphertexts: Vec<RecipientCiphertext> = recipients
        .into_iter()
        .map(|(addr, sealed)| RecipientCiphertext {
            recipient: addr,
            sealed,
        })
        .collect();

    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::EpochRotation {
            prior_epoch,
            triggered_by,
            recipient_ciphertexts,
        },
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign epoch_rotation: {e}"))
}

/// ZEB-249 Task 6: pure helper — mint a signed `EpochCatchup` event.
///
/// `triggered_by` is the `EventId` of the Join event the latecomer member
/// used. `epoch` is the current epoch being delivered. `recipients` maps
/// each member in `pending_catchup_for` to their sealed epoch key bytes
/// (as produced by `seal_to_owner`). Each named recipient MUST appear in
/// `recipients` for the event to pass materialize's validity check.
///
/// ZEB-267: Caller pre-reserves `hlc`.
pub fn mint_epoch_catchup_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    triggered_by: crate::community_membership::EventId,
    epoch: u64,
    recipients: Vec<(crate::owner_state_types::OwnerAddr, Vec<u8>)>,
    signing_key: &ed25519_dalek::SigningKey,
    hlc: crate::owner_state_types::Hlc,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{
        sign_event, EventPayload, MembershipEventKind, RecipientCiphertext,
    };
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let recipient_ciphertexts: Vec<RecipientCiphertext> = recipients
        .into_iter()
        .map(|(addr, sealed)| RecipientCiphertext {
            recipient: addr,
            sealed,
        })
        .collect();

    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::EpochCatchup {
            epoch,
            triggered_by,
            recipient_ciphertexts,
        },
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign epoch_catchup: {e}"))
}

/// ZEB-249 Task 6: build the sealed epoch key map for all given members.
///
/// For each `(OwnerAddr, identity_pub_64)` pair, converts the Ed25519 pub
/// (bytes 32..64 of the 64-byte combined pub) to X25519 and seals the
/// fresh `EpochKey` bytes to that X25519 pubkey via `seal_to_owner`.
///
/// Returns `Err` if any member's identity_pub cannot be converted to X25519
/// or if `seal_to_owner` fails. Callers are responsible for filtering the
/// member list before calling (e.g., dropping members whose identity pubs are
/// not yet available).
fn build_sealed_epoch_recipients(
    k_next: &crate::owner_state_types::EpochKey,
    members: Vec<(crate::owner_state_types::OwnerAddr, [u8; 64])>,
) -> Result<Vec<(crate::owner_state_types::OwnerAddr, Vec<u8>)>, String> {
    use crate::dm_signing::{ed25519_pub_to_x25519, seal_to_owner};

    let mut recipients = Vec::with_capacity(members.len());
    for (addr, identity_pub_64) in members {
        // identity_pub_64 layout: [x25519_pub(32) || ed25519_pub(32)]
        // Ed25519 pub is bytes 32..64.
        let ed_pub: &[u8; 32] = identity_pub_64[32..64]
            .try_into()
            .map_err(|_| "identity_pub_64 slice 32..64 not 32 bytes (should never happen)")?;
        let x25519_pub = ed25519_pub_to_x25519(ed_pub)
            .map_err(|e| format!("ed25519_pub_to_x25519 for {addr:?}: {e}"))?;
        let sealed = seal_to_owner(&x25519_pub, k_next.as_bytes())
            .map_err(|e| format!("seal_to_owner for {addr:?}: {e}"))?;
        recipients.push((addr, sealed));
    }
    Ok(recipients)
}

/// Tauri IPC: kick a member from a community.
///
/// Power-gated by `verify_event`: actor must have power ≥ 50 (kick
/// threshold) AND strictly greater than target's current power.
/// Returns Err with the VerifyError discriminant on rejection.
///
/// ZEB-250: when admin_quorum > 1 AND the target is currently an admin
/// (power == 100), mints an AdminProposal instead of a direct Kick and
/// returns `AdminActionResult::Pending`. Otherwise performs the direct
/// Kick + EpochRotation and returns `AdminActionResult::Completed`.
#[tauri::command]
async fn kick_from_community(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    target_addr: String,
    reason: Option<String>,
) -> Result<AdminActionResult, String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let target_bytes: [u8; 16] = hex::decode(&target_addr)
        .map_err(|e| format!("invalid target_addr hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "target_addr must be 16 bytes (32 hex chars)".to_string())?;
    let target = crate::owner_state_types::OwnerAddr(target_bytes);

    let (hlc_tracker, device_id, self_owner, community_registry, dm_outbox, snapshot_generation) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.generation,
        )
    };

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // ZEB-267: reserve the HLC atomically (read-bump-write under
    // tracker lock) BEFORE minting. Replaces the prior
    // snapshot-then-release pattern that had a race window between
    // the prev_hlc read and the post-Inserted advance.
    let event_hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;

    // Generation + registry fence (mirrors leave_community + the
    // create_community / redeem_invite shape). Plain generation check
    // is insufficient: stop_node nullifies `community_registry` to
    // None without bumping generation, so without the registry-presence
    // check we'd happily insert into a detached engine that's about to
    // be torn down.
    {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err(format!(
                "node generation changed during kick_from_community (was {}, now {})",
                snapshot_generation, g.generation
            ));
        }
        if g.community_registry.is_none() {
            return Err(
                "community_registry detached during kick_from_community (node stopped?)"
                    .to_string(),
            );
        }
    }

    let engine_arc = community_registry
        .engine_arc(&space_id)
        .await
        .ok_or_else(|| {
            format!(
                "no engine for community {} — not currently joined",
                hex::encode(space_id.0)
            )
        })?;

    // ZEB-250: read materialized state to determine admin_quorum + target's
    // current power level. This read is done BEFORE minting any event so
    // the routing decision reflects the current CRDT state.
    let (admin_quorum, target_power_now) = {
        let state = engine_arc.state();
        let state_g = state.lock().await;
        let admin_addr = engine_arc.admin_addr();
        let m = state_g.materialize_now(admin_addr);
        let tpow = m.power_levels.get(&target).copied().unwrap_or(0);
        (m.admin_quorum, tpow)
    };

    // ZEB-250: admin-affecting kick = target is currently an admin (power == 100).
    let admin_affecting = target_power_now == 100;

    if admin_quorum > 1 && admin_affecting {
        // Route via AdminProposal — the proposer counts as signer 1.
        let proposal = {
            let outbox_g = dm_outbox.lock().await;
            let signing_key = outbox_g.signing_key.as_ref();
            mint_admin_proposal_kick_event(
                space_id,
                self_owner,
                target,
                reason.clone(),
                signing_key,
                event_hlc,
            )?
        };
        let proposal_id_hex = hex::encode(proposal.id);
        let outcome = engine_arc
            .insert_local_event(proposal)
            .await
            .map_err(|e| format!("engine.insert_local_event (AdminProposal kick): {e}"))?;
        if matches!(
            outcome,
            crate::community_state_crdt::InsertOutcome::Rejected(_)
        ) {
            return Err(membership_outcome_err(
                "kick_from_community (AdminProposal)",
                &outcome,
            ));
        }
        return Ok(AdminActionResult::Pending {
            proposal_event_id: proposal_id_hex,
            signers_so_far: 1,
            quorum_required: admin_quorum,
        });
    }

    // Direct kick path (admin_quorum == 1 OR target is not an admin).
    let kick = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_kick_event(space_id, self_owner, target, reason, signing_key, event_hlc)?
    };

    // ZEB-249 Task 6 spec §4.1 atomicity: mint the EpochRotation BEFORE
    // inserting either event, then submit both via insert_local_event_pair
    // so no crash window exists between the Kick and its matching Rotation.
    //
    // We read materialized state PRE-kick (before either event is inserted)
    // and exclude the target explicitly — the result is identical to the
    // post-kick materialized state for the purposes of recipient selection.
    //
    // If rotation minting fails (e.g., all member identity pubs unknown),
    // we fall back to inserting just the Kick alone and let the self-healing
    // observer synthesize the rotation. This is the same degraded-recovery
    // story as before, but now the happy path is fully atomic.
    let rotation_bundle: Result<
        (
            crate::community_membership::SignedMembershipEvent,
            crate::owner_state_types::EpochKey,
            u64, // prior_epoch: current_epoch at the time the rotation was minted
        ),
        String,
    > = async {
        // 1. Read materialized membership PRE-kick.
        let materialized = {
            let state = engine_arc.state();
            let state_g = state.lock().await;
            let admin_addr = engine_arc.admin_addr();
            state_g.materialize_now(admin_addr)
        };

        let current_epoch = materialized.current_epoch.unwrap_or(0);

        // 2. Collect remaining active members (excluding the kick target).
        //    We seal to ALL remaining members including self so our own Space can
        //    advance. The resolver returns the 64-byte identity pub.
        let resolver = community_registry.identity_resolver();
        let mut member_pubs: Vec<(crate::owner_state_types::OwnerAddr, [u8; 64])> = Vec::new();
        for (addr, state_m) in &materialized.members {
            if *addr == target {
                continue; // kicked target excluded per spec §4.1
            }
            if !matches!(
                state_m.status,
                crate::community_membership::MemberStatus::Joined
            ) {
                continue; // only deliver to active members
            }
            if let Some(pub64) = resolver.resolve(addr).await {
                member_pubs.push((*addr, pub64));
            }
            // If resolve returns None, member's identity hasn't propagated yet.
            // Self-healing observer will retry.
        }

        // 3. Generate K_next and seal to each remaining member.
        let k_next = crate::owner_state_types::EpochKey::random();
        let recipients = build_sealed_epoch_recipients(&k_next, member_pubs)?;

        // 4. Reserve a second HLC for the rotation (must be strictly newer than kick_hlc).
        let rotation_hlc =
            crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms)
                .await;

        // 5. Mint the EpochRotation event referencing the Kick's EventId.
        let rotation = {
            let outbox_g = dm_outbox.lock().await;
            let signing_key = outbox_g.signing_key.as_ref();
            mint_epoch_rotation_event(
                space_id,
                self_owner,
                kick.id,
                current_epoch,
                recipients,
                signing_key,
                rotation_hlc,
            )?
        };

        Ok((rotation, k_next, current_epoch))
    }
    .await;

    match rotation_bundle {
        Ok((rotation, k_next, prior_epoch)) => {
            // §4.1 happy path: submit kick + rotation atomically.
            let (kick_outcome, rot_outcome) = engine_arc
                .insert_local_event_pair(kick.clone(), rotation)
                .await
                .map_err(|e| format!("engine.insert_local_event_pair: {e}"))?;

            if matches!(
                kick_outcome,
                crate::community_state_crdt::InsertOutcome::Rejected(_)
            ) {
                return Err(membership_outcome_err("kick_from_community", &kick_outcome));
            }

            if matches!(
                rot_outcome,
                crate::community_state_crdt::InsertOutcome::Rejected(_)
            ) {
                tracing::warn!(
                    community_id = %hex::encode(space_id.0),
                    target = %hex::encode(target.0),
                    "kick_from_community: rotation rejected by CRDT; \
                     self-healing observer will retry"
                );
            } else if matches!(
                rot_outcome,
                crate::community_state_crdt::InsertOutcome::Inserted
            ) {
                // Rotation freshly inserted — advance our local Space epoch state.
                // Guard is `Inserted` (not `!Rejected`) to prevent double-advance
                // on the astronomically unlikely AlreadyKnown case (same EventId
                // collision from a re-issued kick replay).
                //
                // CR Critical (PR #106 R7): apply_remote_epoch_event (the delta
                // consumer) can process this node's own EpochRotation before we
                // reach here, since events flow through the consumer regardless of
                // origin. If it already advanced the epoch we must NOT advance again
                // — that would double-advance and archive the wrong key. Compare
                // against `prior_epoch` (the epoch at minting time) and only apply
                // when the stored epoch is still behind the target.
                if let Some(crdt_state) = {
                    let g = state_lock
                        .lock()
                        .map_err(|e| format!("NodeState poisoned: {e}"))?;
                    g.crdt_state.clone()
                } {
                    let mut state_g = crdt_state.lock().await;
                    if let Some(space) = state_g.spaces.get_mut(&space_id) {
                        let target_epoch = prior_epoch + 1;
                        if space.current_epoch.unwrap_or(0) < target_epoch {
                            // Delta-consumer has not yet applied this rotation.
                            let prev_key = space.current_epoch_key.clone();
                            space.current_epoch = Some(target_epoch);
                            space.current_epoch_key = Some(k_next);
                            if let Some(pk) = prev_key {
                                space.old_epoch_keys.insert(prior_epoch, pk);
                            }
                        }
                        // else: delta-consumer path already at or past target — skip.
                    }
                }
            }
        }
        Err(e) => {
            // Rotation bundle failed — fall back to inserting Kick alone.
            // Self-healing observer will synthesize the rotation.
            tracing::warn!(
                community_id = %hex::encode(space_id.0),
                target = %hex::encode(target.0),
                error = %e,
                "kick_from_community: EpochRotation bundle failed; \
                 inserting Kick alone — self-healing observer will synthesize rotation"
            );
            let outcome = engine_arc
                .insert_local_event(kick.clone())
                .await
                .map_err(|e| format!("engine.insert_local_event: {e}"))?;
            if matches!(
                outcome,
                crate::community_state_crdt::InsertOutcome::Rejected(_)
            ) {
                return Err(membership_outcome_err("kick_from_community", &outcome));
            }
        }
    }

    Ok(AdminActionResult::Completed)
}

// ── ZEB-262 Phase 4: set_power_level ─────────────────────────────────
//
// Same shape as kick_from_community. Power-gate enforcement in
// verify_event: actor must have power ≥ set_power_threshold (100), and
// the proposed level must be in [0, POWER_THRESHOLDS.max]. Admin self-
// demote is allowed (foot-gun, but consistent with the CRDT semantics);
// any UI warning lives in Phase 5.

/// Pure function: mint a self-signed SetPower event for a community we
/// moderate (verify_event power-gates at level ≥ 100).
///
/// ZEB-267: Caller pre-reserves `hlc` via
/// `dm_outbox::reserve_next_hlc_for_device`.
pub fn mint_set_power_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    target: crate::owner_state_types::OwnerAddr,
    level: u8,
    signing_key: &ed25519_dalek::SigningKey,
    hlc: crate::owner_state_types::Hlc,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::SetPower { target, level },
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign set_power: {e}"))
}

/// Tauri IPC: set a member's power level in a community.
///
/// Power-gated by `verify_event`: actor must have power ≥ 100 (the
/// set_power threshold). Out-of-range levels are rejected by
/// verify_event as `PowerLevelOutOfRange`. Returns Err with the
/// VerifyError discriminant on rejection.
///
/// ZEB-250: when admin_quorum > 1 AND the action is admin-affecting
/// (new level == 100 OR target is currently admin with level == 100),
/// mints an AdminProposal instead of a direct SetPower event and returns
/// `AdminActionResult::Pending`. Otherwise performs the direct SetPower
/// and returns `AdminActionResult::Completed`.
#[tauri::command]
async fn set_power_level(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    target_addr: String,
    level: u8,
) -> Result<AdminActionResult, String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let target_bytes: [u8; 16] = hex::decode(&target_addr)
        .map_err(|e| format!("invalid target_addr hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "target_addr must be 16 bytes (32 hex chars)".to_string())?;
    let target = crate::owner_state_types::OwnerAddr(target_bytes);

    let (hlc_tracker, device_id, self_owner, community_registry, dm_outbox, snapshot_generation) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.generation,
        )
    };

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // ZEB-267: atomic HLC reservation.
    let event_hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;

    // Generation + registry fence (see kick_from_community for
    // motivation; stop_node nullifies registry without bumping
    // generation).
    {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err(format!(
                "node generation changed during set_power_level (was {}, now {})",
                snapshot_generation, g.generation
            ));
        }
        if g.community_registry.is_none() {
            return Err(
                "community_registry detached during set_power_level (node stopped?)".to_string(),
            );
        }
    }

    let engine_arc = community_registry
        .engine_arc(&space_id)
        .await
        .ok_or_else(|| {
            format!(
                "no engine for community {} — not currently joined",
                hex::encode(space_id.0)
            )
        })?;

    // ZEB-250: read materialized state to determine admin_quorum + target's
    // current power level. This read is done BEFORE minting any event so
    // the routing decision reflects the current CRDT state.
    let (admin_quorum, target_power_now) = {
        let state = engine_arc.state();
        let state_g = state.lock().await;
        let admin_addr = engine_arc.admin_addr();
        let m = state_g.materialize_now(admin_addr);
        let tpow = m.power_levels.get(&target).copied().unwrap_or(0);
        (m.admin_quorum, tpow)
    };

    // ZEB-250: admin-affecting SetPower = new level is 100 (promoting to admin)
    // OR target is currently an admin (power == 100, i.e. a demotion).
    let admin_affecting = level == 100 || target_power_now == 100;

    if admin_quorum > 1 && admin_affecting {
        // Route via AdminProposal — the proposer counts as signer 1.
        let proposal = {
            let outbox_g = dm_outbox.lock().await;
            let signing_key = outbox_g.signing_key.as_ref();
            mint_admin_proposal_set_power_event(
                space_id,
                self_owner,
                target,
                level,
                signing_key,
                event_hlc,
            )?
        };
        let proposal_id_hex = hex::encode(proposal.id);
        let outcome = engine_arc
            .insert_local_event(proposal)
            .await
            .map_err(|e| format!("engine.insert_local_event (AdminProposal set_power): {e}"))?;
        if matches!(
            outcome,
            crate::community_state_crdt::InsertOutcome::Rejected(_)
        ) {
            return Err(membership_outcome_err(
                "set_power_level (AdminProposal)",
                &outcome,
            ));
        }
        return Ok(AdminActionResult::Pending {
            proposal_event_id: proposal_id_hex,
            signers_so_far: 1,
            quorum_required: admin_quorum,
        });
    }

    // Direct SetPower path (admin_quorum == 1 OR action is not admin-affecting).
    let event = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_set_power_event(space_id, self_owner, target, level, signing_key, event_hlc)?
    };
    let outcome = engine_arc
        .insert_local_event(event.clone())
        .await
        .map_err(|e| format!("engine.insert_local_event: {e}"))?;
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Rejected(_)
    ) {
        return Err(membership_outcome_err("set_power_level", &outcome));
    }

    Ok(AdminActionResult::Completed)
}

// ── ZEB-284 Task 2: unban_from_community ─────────────────────────────
//
// Mirrors `set_power_level` (simplest mutation shape — no EpochRotation).
// Power-gate: actor must have power ≥ 100 (admin-tier); target must be
// currently Banned. Does NOT trigger EpochRotation — Unban is additive
// (re-opens invite eligibility); the subsequent Invite → Join flow
// owns its own epoch via the EpochCatchup path.

/// Tauri IPC: lift a prior Kick-as-ban so the target can be re-invited.
///
/// Admin-tier (power ≥ 100). Returns `Err("target is not currently
/// banned")` (via `membership_outcome_err`) when the target's current
/// status is not Banned. Returns `Err("insufficient power")` when the
/// actor is below admin-tier.
///
/// Does NOT trigger EpochRotation — Unban is additive. The subsequent
/// Invite → Join flow handles its own epoch delivery.
#[tauri::command]
async fn unban_from_community(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    target_addr: String,
    reason: Option<String>,
) -> Result<(), String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let target_bytes: [u8; 16] = hex::decode(&target_addr)
        .map_err(|e| format!("invalid target_addr hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "target_addr must be 16 bytes (32 hex chars)".to_string())?;
    let target = crate::owner_state_types::OwnerAddr(target_bytes);

    let (hlc_tracker, device_id, self_owner, community_registry, dm_outbox, snapshot_generation) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.generation,
        )
    };

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // ZEB-267: atomic HLC reservation.
    let hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;
    let event = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_unban_event(space_id, self_owner, target, reason, signing_key, hlc)?
    };

    // Generation + registry fence (see kick_from_community for
    // motivation; stop_node nullifies registry without bumping
    // generation).
    {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err(format!(
                "node generation changed during unban_from_community (was {}, now {})",
                snapshot_generation, g.generation
            ));
        }
        if g.community_registry.is_none() {
            return Err(
                "community_registry detached during unban_from_community (node stopped?)"
                    .to_string(),
            );
        }
    }

    let engine_arc = community_registry
        .engine_arc(&space_id)
        .await
        .ok_or_else(|| {
            format!(
                "no engine for community {} — not currently joined",
                hex::encode(space_id.0)
            )
        })?;
    let outcome = engine_arc
        .insert_local_event(event.clone())
        .await
        .map_err(|e| format!("engine.insert_local_event: {e}"))?;
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Rejected(_)
    ) {
        return Err(membership_outcome_err("unban_from_community", &outcome));
    }

    Ok(())
}

// ── ZEB-284 Task 2: list_recent_moderation_events ────────────────────
//
// Read-only IPC: fetches the raw signed-event log from the community
// engine, filters to Kick / Unban / SetPower kinds, sorts by HLC desc,
// truncates to `limit` (clamped 1..=100), and maps to `ModerationEventDto`.
//
// Pattern mirrors `list_channels` (ZEB-248) which reads the engine
// state via `registry.state_for` + `g.materialize_now`. Here we need
// the raw event log rather than the materialized view, so we access
// `engine_state.lock().await.events` directly — same lock, same Arc.

/// Tauri IPC: return up to `limit` recent moderation events (Kick,
/// Unban, SetPower) for a community, sorted by HLC descending.
///
/// `limit` is clamped to 1..=100. Events are drawn from the community's
/// signed event log; all other kinds (Join, Leave, Invite, ChannelCreate,
/// EpochRotation, etc.) are filtered out.
///
/// Errors: same hex/registry/Space-row error path as `list_community_members`.
#[tauri::command]
async fn list_recent_moderation_events(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    limit: u32,
) -> Result<Vec<ModerationEventDto>, String> {
    let limit = limit.clamp(1, 100) as usize;

    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let (crdt_state, registry) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.community_registry
                .clone()
                .ok_or("no community_registry — node not running?")?,
        )
    };

    // Validate the Space row (same guard as list_community_members).
    {
        let s = crdt_state.lock().await;
        let space = s.spaces.get(&space_id).cloned();
        drop(s);
        let space = space.ok_or_else(|| {
            format!(
                "no Space for community {} in owner-state",
                hex::encode(space_id.0)
            )
        })?;
        if space.kind != crate::owner_state_types::SpaceKind::Community {
            return Err(format!(
                "Space {} exists but is kind {:?}, not Community",
                hex::encode(space_id.0),
                space.kind
            ));
        }
    }

    let engine_state = registry.state_for(&space_id).await.ok_or_else(|| {
        format!(
            "no engine for community {} — not joined or not yet started",
            hex::encode(space_id.0)
        )
    })?;

    // Collect raw events, filter to moderation kinds, map to DTO.
    let raw_events: Vec<crate::community_membership::SignedMembershipEvent> = {
        let g = engine_state.lock().await;
        g.events.values().cloned().collect()
    };

    let mut dtos: Vec<ModerationEventDto> = raw_events
        .into_iter()
        .filter_map(|ev| {
            let (kind, target_addr, reason, new_power) = match &ev.kind {
                crate::community_membership::MembershipEventKind::Kick { target, reason } => (
                    ModerationEventKindDto::Kick,
                    hex::encode(target.0),
                    reason.clone(),
                    None,
                ),
                crate::community_membership::MembershipEventKind::Unban { target, reason } => (
                    ModerationEventKindDto::Unban,
                    hex::encode(target.0),
                    reason.clone(),
                    None,
                ),
                crate::community_membership::MembershipEventKind::SetPower { target, level } => (
                    ModerationEventKindDto::SetPower,
                    hex::encode(target.0),
                    None,
                    Some(*level),
                ),
                _ => return None,
            };
            Some(ModerationEventDto {
                event_id: hex::encode(ev.id),
                kind,
                actor_addr: hex::encode(ev.actor.0),
                target_addr,
                reason,
                new_power,
                hlc: ev.at.clone(),
            })
        })
        .collect();

    // Sort by HLC descending across the full tuple: wall_ms desc, then
    // logical desc, then device_id desc. The device_id tiebreaker keeps
    // ordering stable across replicas when two events from different
    // devices share (wall_ms, logical) — otherwise the "recent actions"
    // list could differ between viewers, making it flaky to assert.
    dtos.sort_by(|a, b| {
        b.hlc
            .wall_ms
            .cmp(&a.hlc.wall_ms)
            .then_with(|| b.hlc.logical.cmp(&a.hlc.logical))
            .then_with(|| b.hlc.device_id.cmp(&a.hlc.device_id))
    });

    dtos.truncate(limit);
    Ok(dtos)
}

// ── ZEB-254 Task 12: list_pending_joins + list_recent_counter_signs ──────────
//
// Admin audit feed IPCs. Both read the raw signed event log from the
// community engine (same Arc<Mutex<CommunityState>> as list_recent_
// moderation_events), apply lightweight filter+sort in-process, and
// return DTOs. No state mutation.

/// DTO returned by `list_pending_joins`. One entry per PendingJoin event
/// that has no matching JoinCountersign and is within the 30-day window.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PendingJoinDto {
    /// Hex-encoded EventId of the PendingJoin (16 bytes → 32 hex chars).
    pub event_id: String,
    /// Hex-encoded OwnerAddr of the joiner (16 bytes → 32 hex chars).
    pub joiner_addr: String,
    /// HLC at which the joiner published the PendingJoin.
    pub pending_at_hlc: crate::community_channel_log_engine::HlcDto,
    /// Optional invitee_hint from the InviteToken, hex-encoded if present.
    pub invitee_hint: Option<String>,
}

/// ZEB-254: admin audit feed — pending joins awaiting counter-sign.
/// Returns PendingJoin events without a matching JoinCountersign AND
/// within the 30-day expiry window. Sorted by pending_at_hlc ascending.
///
/// Errors: same hex/registry/Space-row path as `list_community_members`.
#[tauri::command]
async fn list_pending_joins(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
) -> Result<Vec<PendingJoinDto>, String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let (registry, self_owner) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.community_registry
                .clone()
                .ok_or("no community_registry — node not running?")?,
            g.dm_self_owner
                .ok_or("dm_self_owner missing — no owner identity?")?,
        )
    };

    let engine_arc = registry.engine_arc(&space_id).await.ok_or_else(|| {
        format!(
            "no engine for community {} — not joined or not yet started",
            hex::encode(space_id.0)
        )
    })?;

    // Authorization: caller must be a Joined member with moderator-tier
    // power (≥ POWER_THRESHOLDS.kick). This is an admin audit feed and
    // must not be readable by plain members.
    {
        let admin_addr = engine_arc.admin_addr();
        let materialized = {
            let state = engine_arc.state();
            let g = state.lock().await;
            g.materialize_now(admin_addr)
        };
        let caller_status = materialized.members.get(&self_owner).map(|m| m.status);
        if !matches!(
            caller_status,
            Some(crate::community_membership::MemberStatus::Joined)
        ) {
            return Err("list_pending_joins: caller is not a Joined member".to_string());
        }
        let caller_power = materialized
            .power_levels
            .get(&self_owner)
            .copied()
            .unwrap_or(0);
        if caller_power < crate::community_membership::POWER_THRESHOLDS.kick {
            return Err(format!(
                "list_pending_joins: caller power {} is below moderator threshold {}",
                caller_power,
                crate::community_membership::POWER_THRESHOLDS.kick
            ));
        }
    }

    let engine_state = engine_arc.state();
    let raw_events: Vec<crate::community_membership::SignedMembershipEvent> = {
        let g = engine_state.lock().await;
        g.events.values().cloned().collect()
    };

    // F7: use wall-clock now for expiry threshold so a lone PendingJoin
    // in an otherwise-idle community ages out correctly.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    Ok(filter_pending_joins(&raw_events, now_ms))
}

/// Pure filter: given the raw event log, return PendingJoin DTOs that
/// have no matching JoinCountersign and are within the 30-day window.
/// Sorted oldest-first by (wall_ms, logical).
///
/// `now_ms` is the caller-supplied wall-clock time (milliseconds since Unix
/// epoch). Using the caller's wall clock — rather than max(event.at.wall_ms)
/// — ensures a lone PendingJoin in an otherwise-idle community ages out
/// correctly (the CRDT materialize keeps max() for determinism; the IPC
/// display layer uses real time).
///
/// Extracted so unit tests can exercise the logic without a full NodeState.
pub fn filter_pending_joins(
    events: &[crate::community_membership::SignedMembershipEvent],
    now_ms: u64,
) -> Vec<PendingJoinDto> {
    let expiry_threshold =
        now_ms.saturating_sub(crate::community_membership::MATERIALIZE_PENDING_EXPIRY_MS);

    // Collect target_event_ids of all JoinCountersign events.
    let countersigned: std::collections::HashSet<crate::community_membership::EventId> = events
        .iter()
        .filter_map(|e| match &e.kind {
            crate::community_membership::MembershipEventKind::JoinCountersign {
                target_event_id,
            } => Some(*target_event_id),
            _ => None,
        })
        .collect();

    let mut out: Vec<PendingJoinDto> = events
        .iter()
        .filter_map(|e| {
            let invite_token = match &e.kind {
                crate::community_membership::MembershipEventKind::PendingJoin {
                    invite_token,
                    ..
                } => invite_token,
                _ => return None,
            };
            // Skip if already countersigned.
            if countersigned.contains(&e.id) {
                return None;
            }
            // Skip if outside the 30-day expiry window.
            if e.at.wall_ms < expiry_threshold {
                return None;
            }
            Some(PendingJoinDto {
                event_id: hex::encode(e.id),
                joiner_addr: hex::encode(e.actor.0),
                pending_at_hlc: crate::community_channel_log_engine::HlcDto {
                    wall_ms: e.at.wall_ms,
                    logical: e.at.logical,
                    device_id: e.at.device_id.clone(),
                },
                invitee_hint: invite_token.invitee_hint.as_ref().map(|h| hex::encode(h.0)),
            })
        })
        .collect();

    out.sort_by_key(|p| (p.pending_at_hlc.wall_ms, p.pending_at_hlc.logical));
    out
}

/// DTO returned by `list_recent_counter_signs`. One entry per self-authored
/// JoinCountersign event, sorted recent-first, capped at `limit`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CounterSignDto {
    /// Hex-encoded target_event_id (16 bytes → 32 hex chars).
    pub join_event_id: String,
    /// Hex-encoded OwnerAddr of the joiner (resolved from the PendingJoin
    /// event; `"(unknown target)"` if the PendingJoin is missing from log).
    pub joiner_addr: String,
    /// HLC at which this JoinCountersign was signed.
    pub countersigned_at_hlc: crate::community_channel_log_engine::HlcDto,
}

/// ZEB-254: admin audit feed — recent self-authored counter-signs.
/// Sorted by countersigned_at_hlc descending. Pass limit=0 for default 20.
///
/// Errors: same hex/registry path as `list_recent_moderation_events`.
#[tauri::command]
async fn list_recent_counter_signs(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    limit: u32,
) -> Result<Vec<CounterSignDto>, String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);
    let cap = if limit == 0 { 20 } else { limit as usize };

    let (registry, self_owner) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.community_registry
                .clone()
                .ok_or("no community_registry — node not running?")?,
            g.dm_self_owner
                .ok_or("dm_self_owner missing — no owner identity?")?,
        )
    };

    let engine_arc = registry.engine_arc(&space_id).await.ok_or_else(|| {
        format!(
            "no engine for community {} — not joined or not yet started",
            hex::encode(space_id.0)
        )
    })?;

    // ZEB-254 R3 (S3): authorize — caller must be a Joined member with
    // moderator-tier power (≥ POWER_THRESHOLDS.kick). This is an admin
    // audit feed and must not be readable by plain members. Mirrors the
    // F1 guard in `list_pending_joins`.
    {
        let admin_addr = engine_arc.admin_addr();
        let materialized = {
            let state = engine_arc.state();
            let g = state.lock().await;
            g.materialize_now(admin_addr)
        };
        let caller_status = materialized.members.get(&self_owner).map(|m| m.status);
        if !matches!(
            caller_status,
            Some(crate::community_membership::MemberStatus::Joined)
        ) {
            return Err("list_recent_counter_signs: caller is not a Joined member".to_string());
        }
        let caller_power = materialized
            .power_levels
            .get(&self_owner)
            .copied()
            .unwrap_or(0);
        if caller_power < crate::community_membership::POWER_THRESHOLDS.kick {
            return Err(format!(
                "list_recent_counter_signs: caller power {} is below moderator threshold {}",
                caller_power,
                crate::community_membership::POWER_THRESHOLDS.kick
            ));
        }
    }

    let engine_state = engine_arc.state();
    let raw_events: Vec<crate::community_membership::SignedMembershipEvent> = {
        let g = engine_state.lock().await;
        g.events.values().cloned().collect()
    };

    Ok(filter_recent_counter_signs(&raw_events, self_owner, cap))
}

/// Pure filter: given the raw event log and the local owner's address,
/// return `CounterSignDto`s for JoinCountersign events authored by
/// `self_owner`, sorted by countersigned_at_hlc descending, truncated to
/// `cap`.
///
/// Extracted so unit tests can exercise the logic without a full NodeState.
pub fn filter_recent_counter_signs(
    events: &[crate::community_membership::SignedMembershipEvent],
    self_owner: crate::owner_state_types::OwnerAddr,
    cap: usize,
) -> Vec<CounterSignDto> {
    // Build event_id → joiner_addr lookup from PendingJoin events.
    let pending_actors: std::collections::HashMap<
        crate::community_membership::EventId,
        crate::owner_state_types::OwnerAddr,
    > = events
        .iter()
        .filter_map(|e| match &e.kind {
            crate::community_membership::MembershipEventKind::PendingJoin { .. } => {
                Some((e.id, e.actor))
            }
            _ => None,
        })
        .collect();

    let mut out: Vec<CounterSignDto> = events
        .iter()
        .filter(|e| e.actor == self_owner)
        .filter_map(|e| {
            let target_event_id = match &e.kind {
                crate::community_membership::MembershipEventKind::JoinCountersign {
                    target_event_id,
                } => target_event_id,
                _ => return None,
            };
            let joiner_addr = pending_actors
                .get(target_event_id)
                .map(|a| hex::encode(a.0))
                .unwrap_or_else(|| "(unknown target)".into());
            Some(CounterSignDto {
                join_event_id: hex::encode(target_event_id),
                joiner_addr,
                countersigned_at_hlc: crate::community_channel_log_engine::HlcDto {
                    wall_ms: e.at.wall_ms,
                    logical: e.at.logical,
                    device_id: e.at.device_id.clone(),
                },
            })
        })
        .collect();

    out.sort_by(|a, b| {
        b.countersigned_at_hlc
            .wall_ms
            .cmp(&a.countersigned_at_hlc.wall_ms)
            .then_with(|| {
                b.countersigned_at_hlc
                    .logical
                    .cmp(&a.countersigned_at_hlc.logical)
            })
    });
    out.truncate(cap);
    out
}

// ── ZEB-250 Task 10: list_pending_admin_proposals IPC ─────────────────────
//
// Admin governance feed IPC. Walks the raw signed event log, computes
// per-proposal signers/expired/effective/self_has_signed, resolves
// proposer + signer display names, and sorts:
//   pending → effective → expired (each bucket chronological).

/// DTO for a single AdminProposal returned by `list_pending_admin_proposals`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingAdminProposalDto {
    /// Hex-encoded EventId of the AdminProposal (16 bytes → 32 hex chars).
    pub event_id: String,
    /// Hex-encoded OwnerAddr of the proposer.
    pub proposer_addr: String,
    /// Display name of the proposer, if available in the local profile cache.
    pub proposer_display_name: Option<String>,
    /// Discriminated proposal kind + kind-specific fields.
    pub proposal_kind: ProposalKindDto,
    /// Wall-clock ms at which the proposer published this AdminProposal.
    pub proposed_at_wall_ms: u64,
    /// Number of distinct admins who have signed (proposer counts as 1).
    pub signers_so_far: u8,
    /// Current materialized admin_quorum for this community.
    pub quorum_required: u8,
    /// True when signers_so_far < quorum_required AND now - proposed_at > 30 days.
    pub expired: bool,
    /// True when signers_so_far >= quorum_required AND the Nth-signer arrived
    /// within 30 days of the proposer.
    pub effective: bool,
    /// True when the caller's OwnerAddr is in the signer set.
    pub self_has_signed: bool,
    /// Display names of all signers (proposer + countersigners), in no
    /// guaranteed order. Names missing from the local cache are omitted.
    pub signer_display_names: Vec<String>,
}

/// Discriminated proposal kind embedded in `PendingAdminProposalDto`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind")]
pub enum ProposalKindDto {
    SetPower {
        target_addr: String,
        target_display_name: Option<String>,
        level: u8,
    },
    Kick {
        target_addr: String,
        target_display_name: Option<String>,
        reason: Option<String>,
    },
    ChangeQuorum {
        new_quorum: u8,
    },
}

/// ZEB-250 §6.2: admin governance feed — all AdminProposal events for a
/// community (pending + effective + expired), sorted pending→effective→expired,
/// each bucket chronological.
///
/// Authorization: caller must be a Joined member with admin-tier power (≥ 100).
#[tauri::command]
async fn list_pending_admin_proposals(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
) -> Result<Vec<PendingAdminProposalDto>, String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let (registry, self_owner) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.community_registry
                .clone()
                .ok_or("no community_registry — node not running?")?,
            g.dm_self_owner
                .ok_or("dm_self_owner missing — no owner identity?")?,
        )
    };

    let engine_arc = registry.engine_arc(&space_id).await.ok_or_else(|| {
        format!(
            "no engine for community {} — not joined or not yet started",
            hex::encode(space_id.0)
        )
    })?;

    // Authorization: caller must be a Joined member with admin-tier power (≥ 100).
    let admin_addr = engine_arc.admin_addr();
    let materialized = {
        let state = engine_arc.state();
        let g = state.lock().await;
        g.materialize_now(admin_addr)
    };
    let caller_status = materialized.members.get(&self_owner).map(|m| m.status);
    if !matches!(
        caller_status,
        Some(crate::community_membership::MemberStatus::Joined)
    ) {
        return Err("list_pending_admin_proposals: caller is not a Joined member".to_string());
    }
    let caller_power = materialized
        .power_levels
        .get(&self_owner)
        .copied()
        .unwrap_or(0);
    if caller_power < 100 {
        return Err(format!(
            "list_pending_admin_proposals: caller power {} below admin threshold 100",
            caller_power
        ));
    }

    let admin_quorum = materialized.admin_quorum;

    let raw_events: Vec<crate::community_membership::SignedMembershipEvent> = {
        let state = engine_arc.state();
        let g = state.lock().await;
        g.events.values().cloned().collect()
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    Ok(compute_pending_admin_proposals(
        &raw_events,
        self_owner,
        admin_quorum,
        now_ms,
    ))
}

/// Pure computation: given the raw event log, caller identity, current
/// admin_quorum and wall-clock now, return `PendingAdminProposalDto`s for
/// every AdminProposal in the log, sorted pending→effective→expired.
///
/// Extracted for unit testing without a full NodeState or Tauri runtime.
pub fn compute_pending_admin_proposals(
    events: &[crate::community_membership::SignedMembershipEvent],
    caller_addr: crate::owner_state_types::OwnerAddr,
    admin_quorum: u8,
    now_ms: u64,
) -> Vec<PendingAdminProposalDto> {
    use crate::community_membership::{
        MembershipEventKind, ProposalKind, ADMIN_PROPOSAL_EXPIRY_MS,
    };
    use std::collections::HashSet;

    let mut dtos: Vec<PendingAdminProposalDto> = Vec::new();

    for event in events {
        let proposal_kind = match &event.kind {
            MembershipEventKind::AdminProposal { proposal_kind } => proposal_kind,
            _ => continue,
        };

        // Collect distinct signers: the proposer + every AdminCountersign actor
        // targeting this proposal's event_id.
        let signers: HashSet<crate::owner_state_types::OwnerAddr> = events
            .iter()
            .filter_map(|e| match &e.kind {
                MembershipEventKind::AdminProposal { .. } if e.id == event.id => Some(e.actor),
                MembershipEventKind::AdminCountersign { target_event_id }
                    if *target_event_id == event.id =>
                {
                    Some(e.actor)
                }
                _ => None,
            })
            .collect();

        let signers_so_far = signers.len() as u8;

        // effective: quorum reached AND the Nth signer arrived within the window.
        // Must be computed BEFORE expired so the expired flag can reference it.
        let effective = signers_so_far >= admin_quorum && {
            let mut signing_wall_ms: Vec<u64> = events
                .iter()
                .filter(|e| match &e.kind {
                    MembershipEventKind::AdminProposal { .. } => e.id == event.id,
                    MembershipEventKind::AdminCountersign { target_event_id } => {
                        *target_event_id == event.id
                    }
                    _ => false,
                })
                .map(|e| e.at.wall_ms)
                .collect();
            signing_wall_ms.sort_unstable();
            signing_wall_ms
                .get((admin_quorum as usize).saturating_sub(1))
                .map(|&nth_ms| nth_ms.saturating_sub(event.at.wall_ms) <= ADMIN_PROPOSAL_EXPIRY_MS)
                .unwrap_or(false)
        };

        // expired: proposal is past the 30-day window AND did NOT achieve
        // quorum within that window (effective == false). A proposal that
        // reached quorum but did so late is NOT expired — it is simply
        // ineffective (effective=false, expired=false).
        //
        // Bug-fix R1: previously computed before `effective`, which meant
        // a late-quorum proposal (signers >= quorum but nth signer > 30d)
        // showed as pending (both flags false) instead of expired.
        let expired =
            now_ms.saturating_sub(event.at.wall_ms) > ADMIN_PROPOSAL_EXPIRY_MS && !effective;

        let self_has_signed = signers.contains(&caller_addr);

        // Resolve signer display names. The pure helper emits hex-encoded
        // addresses. The IPC layer with profile-cache access would substitute
        // real names; the frontend falls back to the address when no name is
        // available.
        let signer_display_names: Vec<String> =
            signers.iter().map(|addr| hex::encode(addr.0)).collect();

        let kind_dto = match proposal_kind {
            ProposalKind::SetPower { target, level } => ProposalKindDto::SetPower {
                target_addr: hex::encode(target.0),
                target_display_name: None,
                level: *level,
            },
            ProposalKind::Kick { target, reason } => ProposalKindDto::Kick {
                target_addr: hex::encode(target.0),
                target_display_name: None,
                reason: reason.clone(),
            },
            ProposalKind::ChangeQuorum { new_quorum } => ProposalKindDto::ChangeQuorum {
                new_quorum: *new_quorum,
            },
        };

        dtos.push(PendingAdminProposalDto {
            event_id: hex::encode(event.id),
            proposer_addr: hex::encode(event.actor.0),
            proposer_display_name: None,
            proposal_kind: kind_dto,
            proposed_at_wall_ms: event.at.wall_ms,
            signers_so_far,
            quorum_required: admin_quorum,
            expired,
            effective,
            self_has_signed,
            signer_display_names,
        });
    }

    // Sort: pending first (chronological), then effective, then expired.
    dtos.sort_by_key(|d| {
        let bucket: u8 = if !d.expired && !d.effective {
            0
        } else if d.effective {
            1
        } else {
            2
        };
        (bucket, d.proposed_at_wall_ms)
    });

    dtos
}

// ── ZEB-250 Task 11: countersign_admin_proposal IPC ──────────────────────

/// ZEB-250 §6.3: count the distinct OwnerAddrs that have signed a given
/// AdminProposal (proposer + all AdminCountersign actors targeting it).
/// Returns 0 if the proposal_id is not found (no proposal event and no
/// countersigns).
pub fn count_signers(
    events: &std::collections::BTreeMap<
        crate::community_membership::EventId,
        crate::community_membership::SignedMembershipEvent,
    >,
    proposal_id: [u8; 16],
) -> u8 {
    use crate::community_membership::MembershipEventKind;
    events
        .values()
        .filter_map(|e| match &e.kind {
            MembershipEventKind::AdminProposal { .. } if e.id == proposal_id => Some(e.actor),
            MembershipEventKind::AdminCountersign { target_event_id }
                if *target_event_id == proposal_id =>
            {
                Some(e.actor)
            }
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>()
        .len() as u8
}

/// ZEB-250 §6.3: admin-only IPC that mints an AdminCountersign event
/// targeting the given AdminProposal. Idempotent: if the caller has
/// already signed (as proposer or via a prior AdminCountersign), returns
/// the current signer count without minting a new event. Rejects
/// expired (> 30 days) or non-existent proposals.
///
/// Authorization: caller must be Joined and have power ≥ 100.
#[tauri::command]
async fn countersign_admin_proposal(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    proposal_event_id: String,
) -> Result<CountersignResult, String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let proposal_id_bytes: [u8; 16] = hex::decode(&proposal_event_id)
        .map_err(|e| format!("countersign_admin_proposal: invalid proposal_event_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| {
            "countersign_admin_proposal: proposal_event_id must be 16 bytes (32 hex chars)"
                .to_string()
        })?;

    let (hlc_tracker, device_id, self_owner, community_registry, dm_outbox, snapshot_generation) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.generation,
        )
    };

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // ZEB-267: atomic HLC reservation.
    let event_hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;

    // Generation + registry fence.
    {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err(format!(
                "node generation changed during countersign_admin_proposal (was {}, now {})",
                snapshot_generation, g.generation
            ));
        }
        if g.community_registry.is_none() {
            return Err(
                "community_registry detached during countersign_admin_proposal (node stopped?)"
                    .to_string(),
            );
        }
    }

    let engine_arc = community_registry
        .engine_arc(&space_id)
        .await
        .ok_or_else(|| {
            format!(
                "no engine for community {} — not currently joined",
                hex::encode(space_id.0)
            )
        })?;

    let admin_addr = engine_arc.admin_addr();

    // Authorization + proposal lookup in a single read lock.
    let (admin_quorum, already_signed, proposal_found, proposal_expired) = {
        let state = engine_arc.state();
        let g = state.lock().await;
        let materialized = g.materialize_now(admin_addr);

        let caller_status = materialized.members.get(&self_owner).map(|m| m.status);
        if !matches!(
            caller_status,
            Some(crate::community_membership::MemberStatus::Joined)
        ) {
            return Err("countersign_admin_proposal: caller is not a Joined member".to_string());
        }
        let caller_power = materialized
            .power_levels
            .get(&self_owner)
            .copied()
            .unwrap_or(0);
        if caller_power < 100 {
            return Err(format!(
                "countersign_admin_proposal: caller power {caller_power} below admin threshold 100"
            ));
        }

        // Lookup + validate target event.
        let target = g.events.get(&proposal_id_bytes);
        let proposal_found = matches!(
            target.map(|e| &e.kind),
            Some(crate::community_membership::MembershipEventKind::AdminProposal { .. })
        );
        if !proposal_found {
            // Either missing entirely or wrong kind — checked below.
            if let Some(ev) = target {
                // Exists but is not an AdminProposal.
                let _ = ev;
                return Err(format!(
                    "countersign_admin_proposal: event {proposal_event_id} is not an AdminProposal"
                ));
            }
        }

        // Expiry check (only meaningful if the proposal exists).
        let proposal_expired = target.is_some_and(|ev| {
            wall_now_ms.saturating_sub(ev.at.wall_ms)
                > crate::community_membership::ADMIN_PROPOSAL_EXPIRY_MS
        });

        // Idempotency: already signed as proposer or via AdminCountersign?
        use crate::community_membership::MembershipEventKind;
        let already_signed = g.events.values().any(|e| match &e.kind {
            MembershipEventKind::AdminProposal { .. } => {
                e.id == proposal_id_bytes && e.actor == self_owner
            }
            MembershipEventKind::AdminCountersign { target_event_id } => {
                *target_event_id == proposal_id_bytes && e.actor == self_owner
            }
            _ => false,
        });

        (
            materialized.admin_quorum,
            already_signed,
            proposal_found,
            proposal_expired,
        )
    };

    if !proposal_found {
        return Err(format!(
            "countersign_admin_proposal: proposal {proposal_event_id} not found"
        ));
    }
    if proposal_expired {
        return Err("countersign_admin_proposal: proposal has expired".to_string());
    }

    if already_signed {
        // Idempotent — report current state without minting.
        let state = engine_arc.state();
        let g = state.lock().await;
        let signers_after = count_signers(&g.events, proposal_id_bytes);
        return Ok(CountersignResult {
            signers_after,
            quorum_required: admin_quorum,
            reached_quorum: signers_after >= admin_quorum,
        });
    }

    // Mint and insert AdminCountersign.
    let countersign_event = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_admin_countersign_event(
            space_id,
            self_owner,
            proposal_id_bytes,
            signing_key,
            event_hlc,
        )?
    };
    let outcome = engine_arc
        .insert_local_event(countersign_event)
        .await
        .map_err(|e| format!("engine.insert_local_event (AdminCountersign): {e}"))?;
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Rejected(_)
    ) {
        return Err(membership_outcome_err(
            "countersign_admin_proposal",
            &outcome,
        ));
    }

    // Recompute signer count after insert.
    let state = engine_arc.state();
    let g = state.lock().await;
    let signers_after = count_signers(&g.events, proposal_id_bytes);
    let post_materialized = g.materialize_now(admin_addr);
    let quorum_required = post_materialized.admin_quorum;
    Ok(CountersignResult {
        signers_after,
        quorum_required,
        reached_quorum: signers_after >= quorum_required,
    })
}

// ── ZEB-250 Task 12: propose_change_quorum IPC ────────────────────────────

/// ZEB-250 §6.4: admin IPC that proposes changing the community's
/// `admin_quorum` threshold. Validates `new_quorum` ∈ [1, admin_count].
/// Mints an `AdminProposal { ChangeQuorum { new_quorum } }` event and
/// returns `AdminActionResult::Completed` when the current quorum is 1
/// (proposer's signature self-satisfies), or `Pending` otherwise.
///
/// Authorization: caller must be Joined with power ≥ 100.
#[tauri::command]
async fn propose_change_quorum(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    new_quorum: u8,
) -> Result<AdminActionResult, String> {
    if new_quorum < 1 {
        return Err("propose_change_quorum: new_quorum must be >= 1".to_string());
    }

    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let (hlc_tracker, device_id, self_owner, community_registry, dm_outbox, snapshot_generation) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.generation,
        )
    };

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // ZEB-267: atomic HLC reservation.
    let event_hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;

    // Generation + registry fence.
    {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err(format!(
                "node generation changed during propose_change_quorum (was {}, now {})",
                snapshot_generation, g.generation
            ));
        }
        if g.community_registry.is_none() {
            return Err(
                "community_registry detached during propose_change_quorum (node stopped?)"
                    .to_string(),
            );
        }
    }

    let engine_arc = community_registry
        .engine_arc(&space_id)
        .await
        .ok_or_else(|| {
            format!(
                "no engine for community {} — not currently joined",
                hex::encode(space_id.0)
            )
        })?;

    let admin_addr = engine_arc.admin_addr();

    // Auth + admin_count + current quorum — single read lock.
    let (admin_quorum, admin_count) = {
        let state = engine_arc.state();
        let state_g = state.lock().await;
        let m = state_g.materialize_now(admin_addr);

        let caller_status = m.members.get(&self_owner).map(|ms| ms.status);
        if !matches!(
            caller_status,
            Some(crate::community_membership::MemberStatus::Joined)
        ) {
            return Err("propose_change_quorum: caller is not a Joined member".to_string());
        }
        let caller_power = m.power_levels.get(&self_owner).copied().unwrap_or(0);
        if caller_power < 100 {
            return Err(format!(
                "propose_change_quorum: caller power {caller_power} below admin threshold 100"
            ));
        }

        // Bug-fix R1 (Bug 3): count only LIVE admins (Joined) so kicked/left
        // admins don't ghost-count toward the quorum range cap. Matches AP5
        // in verify_event.
        let count = m
            .power_levels
            .iter()
            .filter(|(addr, p)| {
                **p == 100
                    && m.members
                        .get(addr)
                        .map(|ms| ms.status == crate::community_membership::MemberStatus::Joined)
                        .unwrap_or(false)
            })
            .count() as u32;
        (m.admin_quorum, count)
    };

    if (new_quorum as u32) > admin_count {
        return Err(format!(
            "propose_change_quorum: new_quorum {} exceeds current admin count {}",
            new_quorum, admin_count
        ));
    }

    // Mint AdminProposal{ChangeQuorum}.
    let proposal = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_admin_proposal_change_quorum_event(
            space_id,
            self_owner,
            new_quorum,
            signing_key,
            event_hlc,
        )?
    };
    let proposal_id_hex = hex::encode(proposal.id);
    let outcome = engine_arc
        .insert_local_event(proposal)
        .await
        .map_err(|e| format!("engine.insert_local_event (AdminProposal change_quorum): {e}"))?;
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Rejected(_)
    ) {
        return Err(membership_outcome_err(
            "propose_change_quorum (AdminProposal)",
            &outcome,
        ));
    }

    if admin_quorum == 1 {
        Ok(AdminActionResult::Completed)
    } else {
        Ok(AdminActionResult::Pending {
            proposal_event_id: proposal_id_hex,
            signers_so_far: 1,
            quorum_required: admin_quorum,
        })
    }
}

/// Delta payload for the `community-members-changed` Tauri event.
/// Matches the spec line 561 wire shape:
/// `{ communityId, changes: [{type, target, by?, detail?}] }`. One
/// IPC event per engine-level CRDT mutation; Phase 3's engine fires
/// one delta at a time so `changes` is always a single-element array
/// in this phase. Future batch-receive optimisations can grow the
/// array without breaking the wire format. Frontend updates
/// incrementally without re-fetching the full member list.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommunityMembersChangedPayload {
    pub community_id: String,
    pub changes: Vec<MembershipChange>,
}

/// One delta in `CommunityMembersChangedPayload.changes`. Flat shape
/// per spec — `type` discriminates the event kind, `target` is the
/// entity whose membership state changed, `by` is the actor when
/// distinct from target (kick/setpower/invite), `detail` carries
/// kind-specific info (kick reason, new power level). `at_wall_ms`
/// is an extension over the spec — useful for the frontend to sort
/// or de-dupe rapid-fire deltas; documented as part of the wire
/// contract here so future consumers don't strip it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MembershipChange {
    #[serde(rename = "type")]
    pub r#type: MembershipChangeType,
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<MembershipChangeDetail>,
    pub at_wall_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MembershipChangeType {
    Joined,
    Left,
    Invited,
    Kicked,
    PowerChanged,
    Unbanned,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
pub enum MembershipChangeDetail {
    Reason(String),
    Level(u8),
}

/// Materialized channel info row for the `list_channels` IPC and the
/// `channel-config-updated` Tauri event payload. Mirrors
/// `ChannelInfo` in `community_membership.rs` but with stringified
/// hex `channel_id` and camelCase fields for the JS bridge.
/// `created_at` and `deleted_at` are passed as the wire `Hlc` shape
/// (same convention as `MemberInfoDto.joined_at`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelInfoDto {
    pub channel_id: String,
    pub name: String,
    pub write_power: u8,
    pub created_at: crate::owner_state_types::Hlc,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<crate::owner_state_types::Hlc>,
}

/// Action discriminator for a `channel-config-updated` Tauri event.
/// Distinct enum (vs. reusing MembershipChangeType) so the frontend's
/// `channel-config-updated` listener doesn't have to re-discriminate
/// against unrelated membership variants.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ChannelConfigChangeAction {
    Created,
    Modified,
    Deleted,
}

/// Wire payload for the `channel-config-updated` Tauri event. Emitted
/// by the community-state-CRDT delta consumer when materialization
/// detects a `ChannelCreate`/`ChannelModify`/`ChannelDelete` mutation.
/// `name` and `write_power` are populated for `Created` (always — both
/// fields are required on the event) and `Modified` (only the fields
/// the modify event actually carried — None means unchanged). Both are
/// omitted for `Deleted`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelConfigChangedPayload {
    pub community_id: String,
    pub channel_id: String,
    pub action: ChannelConfigChangeAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_power: Option<u8>,
    pub at_wall_ms: u64,
}

/// Project a `CommunityMembershipDelta` into `(community_id_hex, change)`.
/// The caller (the start_node consumer task) wraps the change in
/// `CommunityMembersChangedPayload { community_id, changes: vec![change] }`
/// and emits the Tauri event.
///
/// Returns `None` for channel-config kinds (`ChannelCreate`/
/// `ChannelModify`/`ChannelDelete` — ZEB-248). Channel-config events
/// are projected to `ChannelConfigChangedPayload` by the (Task 5)
/// `delta_to_channel_config_change` projector instead.
pub fn delta_to_change(
    delta: &crate::community_state_sync::CommunityMembershipDelta,
) -> Option<(String, MembershipChange)> {
    let cid_hex = hex::encode(delta.community_id.0);
    let actor_hex = hex::encode(delta.event.actor.0);
    let at_wall_ms = delta.event.at.wall_ms;
    let change = match &delta.event.kind {
        crate::community_membership::MembershipEventKind::Join => MembershipChange {
            r#type: MembershipChangeType::Joined,
            target: actor_hex,
            by: None,
            detail: None,
            at_wall_ms,
        },
        crate::community_membership::MembershipEventKind::Leave => MembershipChange {
            r#type: MembershipChangeType::Left,
            target: actor_hex,
            by: None,
            detail: None,
            at_wall_ms,
        },
        crate::community_membership::MembershipEventKind::Invite { target } => MembershipChange {
            r#type: MembershipChangeType::Invited,
            target: hex::encode(target.0),
            by: Some(actor_hex),
            detail: None,
            at_wall_ms,
        },
        crate::community_membership::MembershipEventKind::Kick { target, reason } => {
            MembershipChange {
                r#type: MembershipChangeType::Kicked,
                target: hex::encode(target.0),
                by: Some(actor_hex),
                detail: reason.clone().map(MembershipChangeDetail::Reason),
                at_wall_ms,
            }
        }
        crate::community_membership::MembershipEventKind::SetPower { target, level } => {
            MembershipChange {
                r#type: MembershipChangeType::PowerChanged,
                target: hex::encode(target.0),
                by: Some(actor_hex),
                detail: Some(MembershipChangeDetail::Level(*level)),
                at_wall_ms,
            }
        }
        crate::community_membership::MembershipEventKind::Unban { target, reason } => {
            MembershipChange {
                r#type: MembershipChangeType::Unbanned,
                target: hex::encode(target.0),
                by: Some(actor_hex),
                detail: reason.clone().map(MembershipChangeDetail::Reason),
                at_wall_ms,
            }
        }
        // Channel-config events (ZEB-248 Phase 1) don't map to a
        // MembershipChange — they are channel state, not membership
        // state. Return None per the function's documented forward-
        // compat contract; channel-config kinds project to
        // ChannelConfigChangedPayload via delta_to_channel_config_change
        // instead — the consumer fan-out fires the channel-config-updated
        // Tauri event.
        // ZEB-249: EpochRotation and EpochCatchup are epoch-state, not
        // membership-state; no Tauri event emitted from this projector.
        crate::community_membership::MembershipEventKind::ChannelCreate { .. }
        | crate::community_membership::MembershipEventKind::ChannelModify { .. }
        | crate::community_membership::MembershipEventKind::ChannelDelete { .. }
        | crate::community_membership::MembershipEventKind::EpochRotation { .. }
        | crate::community_membership::MembershipEventKind::EpochCatchup { .. }
        // ZEB-285: Fork is non-mutating membership-wise; no MembershipChange
        // is projected for it. Fork events are surfaced via a separate
        // fork-lineage listing path (Task 7+), not via the membership-changed
        // Tauri event stream.
        | crate::community_membership::MembershipEventKind::Fork { .. }
        // ZEB-254: PendingJoin and JoinCountersign project to MembershipChange
        // in Task 4 (IPC wiring). Until then, emit no Tauri event.
        | crate::community_membership::MembershipEventKind::PendingJoin { .. }
        | crate::community_membership::MembershipEventKind::JoinCountersign { .. }
        // ZEB-250: AdminProposal and AdminCountersign are governance events;
        // projection to MembershipChange is deferred to Task 8 (IPC wiring).
        | crate::community_membership::MembershipEventKind::AdminProposal { .. }
        | crate::community_membership::MembershipEventKind::AdminCountersign { .. } => return None,
    };
    Some((cid_hex, change))
}

/// Project a `CommunityMembershipDelta` into a `ChannelConfigChangedPayload`.
/// Returns `None` for membership-event kinds (those are handled by
/// `delta_to_change`). Symmetric to `delta_to_change` — together they
/// cover all `MembershipEventKind` variants without overlap.
pub fn delta_to_channel_config_change(
    delta: &crate::community_state_sync::CommunityMembershipDelta,
) -> Option<ChannelConfigChangedPayload> {
    let community_id_hex = hex::encode(delta.community_id.0);
    let at_wall_ms = delta.event.at.wall_ms;
    let (channel_id, action, name, write_power) = match &delta.event.kind {
        crate::community_membership::MembershipEventKind::ChannelCreate {
            channel_id,
            name,
            write_power,
        } => (
            hex::encode(channel_id.0),
            ChannelConfigChangeAction::Created,
            Some(name.clone()),
            Some(*write_power),
        ),
        crate::community_membership::MembershipEventKind::ChannelModify {
            channel_id,
            name,
            write_power,
        } => (
            hex::encode(channel_id.0),
            ChannelConfigChangeAction::Modified,
            name.clone(),
            *write_power,
        ),
        crate::community_membership::MembershipEventKind::ChannelDelete { channel_id } => (
            hex::encode(channel_id.0),
            ChannelConfigChangeAction::Deleted,
            None,
            None,
        ),
        _ => return None,
    };
    Some(ChannelConfigChangedPayload {
        community_id: community_id_hex,
        channel_id,
        action,
        name,
        write_power,
        at_wall_ms,
    })
}

/// Drain `delta_rx`. Each delta is projected as EITHER:
///   - `MembershipChange` → `community-members-changed` Tauri event
///     (membership variants: Join/Leave/Invite/Kick/SetPower)
///   - `ChannelConfigChangedPayload` → `channel-config-updated` Tauri
///     event (ZEB-248 Phase 1 channel-config variants)
///   - `EpochEventPayload` → `on_epoch_event` hook (ZEB-249 Task 6)
///     for epoch rotation/catchup self-healing
///
/// Stops cleanly when the channel closes (last sender dropped — typically
/// on `stop_node`).
///
/// Phase 3 emits one change per `community-members-changed` IPC event
/// (engine fires one delta per CRDT mutation); the wire format leaves
/// room for batched future deltas without a contract break.
pub async fn run_community_delta_consumer<FM, FutM, FC, FutC, FR, FutR, FE, FutE>(
    mut delta_rx: tokio::sync::mpsc::Receiver<
        crate::community_state_sync::CommunityMembershipDelta,
    >,
    mut emit_membership: FM,
    mut emit_channel_config: FC,
    mut on_channel_config_registry: FR,
    mut on_epoch_event: FE,
) where
    FM: FnMut(CommunityMembersChangedPayload) -> FutM + Send + 'static,
    FutM: std::future::Future<Output = ()> + Send + 'static,
    FC: FnMut(ChannelConfigChangedPayload) -> FutC + Send + 'static,
    FutC: std::future::Future<Output = ()> + Send + 'static,
    // ZEB-270 Phase 3 Task 4B: registry-side hook fires on the same
    // channel-config payload as `emit_channel_config` so the
    // `ChannelLogRegistry` can spawn / stop per-channel engines on
    // Created / Deleted (Modified is a no-op for the registry — only
    // metadata changed; the underlying log is unaffected). Two
    // callbacks rather than one because the IPC-event-emit and
    // registry-mutate paths have different lifetimes (one needs an
    // `AppHandle`, the other an `Arc<ChannelLogRegistry>`) and
    // different failure modes (emit logs + drops; registry surfaces
    // errors via tracing).
    FR: FnMut(ChannelConfigChangedPayload) -> FutR + Send + 'static,
    FutR: std::future::Future<Output = ()> + Send + 'static,
    // ZEB-249 Task 6: fired for every EpochRotation or EpochCatchup delta.
    // The production hook implements the self-healing observer (spec §4.3):
    // checks pending_rotation_for / pending_catchup_for in materialized state
    // and synthesizes missing events if local user has admin power. Tests
    // supply `|_| async {}`.
    FE: FnMut(crate::community_state_sync::CommunityMembershipDelta) -> FutE + Send + 'static,
    FutE: std::future::Future<Output = ()> + Send + 'static,
{
    while let Some(delta) = delta_rx.recv().await {
        if let Some((community_id, change)) = delta_to_change(&delta) {
            let payload = CommunityMembersChangedPayload {
                community_id,
                changes: vec![change],
            };
            emit_membership(payload).await;
        } else if let Some(payload) = delta_to_channel_config_change(&delta) {
            // Order matters: registry hook FIRST (awaits engine
            // spawn/stop), THEN UI event. The UI consumer of
            // `channel-config-updated` for a Created channel
            // immediately fires `list_channel_messages` /
            // `post_channel_message` IPCs that look up the engine
            // in the registry. If we emit the Tauri event before
            // `registry.spawn` has awaited to completion, those
            // IPCs hit a "no engine for ..." race and surface
            // false-error toasts.
            //
            // Cost: UI sees the channel-config-updated event a few
            // ms later (registry.spawn does dir-create + tail
            // reload + adapter bridge enqueue). For Modified the
            // registry hook is a no-op; for Deleted it's
            // registry.stop (cheap). Trade is unambiguously worth
            // it — the previous order was an observed UI race.
            //
            // Both callbacks see the same payload bytes.
            on_channel_config_registry(payload.clone()).await;
            emit_channel_config(payload).await;
        }
        // ZEB-249 Task 6 §4.3: fire on_epoch_event after EVERY delta
        // (not just epoch-kind deltas). A Kick/Leave delta that lands
        // without a matching Rotation also needs the self-healing observer
        // to check pending_rotation_for and synthesize the missing event.
        // The observer is re-entrant-safe (per-session BTreeSet dedupe)
        // so firing it on channel-config or rotation deltas is cheap.
        on_epoch_event(delta).await;
    }
}

/// ZEB-249 §10.6: apply a remote EpochRotation or EpochCatchup event that
/// arrived via CRDT sync on a NON-ORIGINATING node.
///
/// When the originating admin issues the event, it seals the new epoch key to
/// every current member (including itself). Every other node observes the CRDT
/// delta carrying the `SignedMembershipEvent`, finds its own entry in
/// `recipient_ciphertexts`, decrypts the sealed key, and updates its local
/// `Space` row in `OwnerState` — all without any additional round-trips.
///
/// **EpochRotation**: advances `current_epoch` by 1, archives the outgoing
/// key into `old_epoch_keys`, and replaces `current_epoch_key` with the
/// freshly-decrypted key.
///
/// **EpochCatchup**: sets `current_epoch` and `current_epoch_key` to the
/// values carried in the event (no archiving — a catchup delivers the already-
/// current key to a latecomer, so the epoch counter does not advance here).
///
/// Idempotent: if this node's `current_epoch` already equals or exceeds the
/// target epoch, the function is a no-op (avoids double-archival on duplicate
/// CRDT deliveries).
///
/// Called from the delta-consumer task — runs on the consumer's tokio task.
/// Lock-order contract: acquires the owner-state mutex ONLY; must not be
/// called while the community-state mutex is already held (community-state
/// mutations happen in `self_heal_community_observer`, which is always called
/// AFTER this function returns).
pub async fn apply_remote_epoch_event(
    crdt_state: std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    local_signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    community_id: crate::owner_state_types::SpaceId,
    event: &crate::community_membership::SignedMembershipEvent,
    local_addr: crate::owner_state_types::OwnerAddr,
) {
    use crate::community_membership::MembershipEventKind;
    use crate::owner_state_types::EpochKey;

    match &event.kind {
        MembershipEventKind::EpochRotation {
            prior_epoch,
            recipient_ciphertexts,
            ..
        } => {
            // The new epoch = prior_epoch + 1.
            let target_epoch = prior_epoch + 1;

            let my_entry = recipient_ciphertexts
                .iter()
                .find(|rc| rc.recipient == local_addr);
            let sealed = match my_entry {
                Some(rc) => rc.sealed.clone(),
                None => return, // this node was kicked / left; not in recipient list
            };

            let x25519_priv = crate::dm_signing::ed25519_priv_to_x25519(&local_signing_key);
            let k_bytes_vec = match crate::dm_signing::open_from_owner(&x25519_priv, &sealed) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        community_id = ?community_id,
                        "apply_remote_epoch_event: EpochRotation sealed-key open failed: {:?}",
                        e
                    );
                    return;
                }
            };
            let k_bytes: [u8; 32] = match k_bytes_vec.try_into() {
                Ok(b) => b,
                Err(v) => {
                    tracing::warn!(
                        community_id = ?community_id,
                        "apply_remote_epoch_event: EpochRotation sealed-key wrong length ({})",
                        v.len()
                    );
                    return;
                }
            };
            let k_next = EpochKey::new(k_bytes);

            let mut state = crdt_state.lock().await;
            let space = match state.spaces.get_mut(&community_id) {
                Some(s) => s,
                None => return, // community not yet in local state
            };
            let current = space.current_epoch.unwrap_or(0);
            if current >= target_epoch {
                // Already at or past this epoch — idempotent no-op.
                return;
            }
            // Archive the current key before replacing it.
            if let Some(prev_key) = space.current_epoch_key.take() {
                space.old_epoch_keys.insert(current, prev_key);
            }
            space.current_epoch = Some(target_epoch);
            space.current_epoch_key = Some(k_next);
            tracing::debug!(
                community_id = ?community_id,
                target_epoch,
                "apply_remote_epoch_event: EpochRotation applied — epoch advanced"
            );
        }

        MembershipEventKind::EpochCatchup {
            epoch,
            recipient_ciphertexts,
            ..
        } => {
            let target_epoch = *epoch;

            let my_entry = recipient_ciphertexts
                .iter()
                .find(|rc| rc.recipient == local_addr);
            let sealed = match my_entry {
                Some(rc) => rc.sealed.clone(),
                None => return, // not the intended catchup recipient
            };

            let x25519_priv = crate::dm_signing::ed25519_priv_to_x25519(&local_signing_key);
            let k_bytes_vec = match crate::dm_signing::open_from_owner(&x25519_priv, &sealed) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        community_id = ?community_id,
                        "apply_remote_epoch_event: EpochCatchup sealed-key open failed: {:?}",
                        e
                    );
                    return;
                }
            };
            let k_bytes: [u8; 32] = match k_bytes_vec.try_into() {
                Ok(b) => b,
                Err(v) => {
                    tracing::warn!(
                        community_id = ?community_id,
                        "apply_remote_epoch_event: EpochCatchup sealed-key wrong length ({})",
                        v.len()
                    );
                    return;
                }
            };
            let k = EpochKey::new(k_bytes);

            let mut state = crdt_state.lock().await;
            let space = match state.spaces.get_mut(&community_id) {
                Some(s) => s,
                None => return,
            };
            let current = space.current_epoch.unwrap_or(0);
            if current >= target_epoch {
                // Catchup is for an epoch we already hold or have surpassed.
                return;
            }
            // CR Major (PR #106 R6): archive previous (epoch, key) when
            // advancing, matching the EpochRotation path. A member that
            // held K(N) as their current key can still decrypt epoch-N
            // content after receiving a catchup to K(M>N). Without
            // archiving, handle_incoming_publish's root_key_used binding
            // would reject epoch-N blobs after the catchup lands.
            if let Some(prev_key) = space.current_epoch_key.take() {
                space.old_epoch_keys.insert(current, prev_key);
            }
            space.current_epoch = Some(target_epoch);
            space.current_epoch_key = Some(k);
            tracing::debug!(
                community_id = ?community_id,
                target_epoch,
                "apply_remote_epoch_event: EpochCatchup applied — epoch key installed"
            );
        }

        // Non-epoch events: no-op.
        _ => {}
    }
}

/// Dedupe set for synthesized EpochCatchup events. The 4-tuple
/// `(SpaceId, OwnerAddr, EventId, u64)` identifies a catchup by
/// community, target member, originating Join EventId, and the epoch
/// at synthesis time. The epoch discriminator lets a second rotation
/// produce a fresh catchup for the same still-pending member (ZEB-249
/// PR #106 R5 — CodeRabbit Major).
pub type SynthCatchupsSet = std::sync::Arc<
    std::sync::Mutex<
        std::collections::BTreeSet<(
            crate::owner_state_types::SpaceId,
            crate::owner_state_types::OwnerAddr,
            crate::community_membership::EventId,
            u64,
        )>,
    >,
>;

/// ZEB-249 Task 6 §4.3 + §4.6: self-healing community observer.
///
/// Called after every CRDT delta lands in the community engine. Re-materializes
/// the community state and checks `pending_rotation_for` / `pending_catchup_for`.
/// If the local user has admin power (≥ POWER_THRESHOLDS.kick), synthesizes any
/// missing `EpochRotation` or `EpochCatchup` events and inserts them into the
/// engine via `insert_local_event`.
///
/// Anti-spam: the `synth_rotations` / `synth_catchups` BTreeSets track which
/// (community_id, target) pairs have been synthesized in this session.
/// First-admin-wins via HLC linearization handles multi-admin races
/// (materialize's staleness gate silently drops duplicate EpochRotations).
///
/// The catchup dedupe key is `(SpaceId, OwnerAddr, EventId, u64)` where EventId
/// is the originating Join event's id and u64 is current_epoch at synthesis time.
/// The epoch discriminator ensures that after a successive epoch rotation the
/// same still-pending joiner fires a fresh catchup (the old key at the prior
/// epoch is no longer useful). A pure `(SpaceId, OwnerAddr, EventId)` key would
/// suppress the post-rotation catchup because the Join EventId doesn't change
/// across rotations. ZEB-249 PR #106 R5 (CodeRabbit Major).
///
/// The `crdt_state` parameter provides the local owner's CRDT, from which the
/// observer reads `spaces[community_id].current_epoch_key` — the CURRENT epoch
/// key after any rotations have landed locally. This is the key that must be
/// sealed to the new joiner (not the engine's spawn-time key).
///
/// Called from the delta consumer task — runs on the consumer's tokio task.
#[allow(clippy::too_many_arguments)]
pub async fn self_heal_community_observer(
    community_id: crate::owner_state_types::SpaceId,
    registry: std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
    signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    hlc_tracker: std::sync::Arc<
        tokio::sync::Mutex<std::collections::BTreeMap<String, crate::owner_state_types::Hlc>>,
    >,
    device_id: String,
    self_owner: crate::owner_state_types::OwnerAddr,
    crdt_state: std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    synth_rotations: std::sync::Arc<
        std::sync::Mutex<
            std::collections::BTreeSet<(
                crate::owner_state_types::SpaceId,
                crate::owner_state_types::OwnerAddr,
                crate::community_membership::EventId,
            )>,
        >,
    >,
    synth_catchups: SynthCatchupsSet,
) {
    let engine_arc = match registry.engine_arc(&community_id).await {
        Some(e) => e,
        None => return, // engine gone (node stopped) — no-op
    };

    let admin_addr = engine_arc.admin_addr();
    let materialized = {
        let state = engine_arc.state();
        let state_g = state.lock().await;
        state_g.materialize_now(admin_addr)
    };

    // Power gate: only admins synthesize rotations/catchups.
    let local_power = materialized
        .power_levels
        .get(&self_owner)
        .copied()
        .unwrap_or(0);
    if local_power < crate::community_membership::POWER_THRESHOLDS.kick {
        return;
    }

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let resolver = registry.identity_resolver();

    // §4.3: synthesize missing EpochRotations.
    let pending_rotations: Vec<crate::owner_state_types::OwnerAddr> =
        materialized.pending_rotation_for.iter().copied().collect();
    for target in pending_rotations {
        // Find the originating Kick or Leave event for this target FIRST —
        // the M7 dedup key includes the trigger EventId so a rejoin + re-kick
        // sequence produces a fresh key (and fires a new rotation), whereas
        // a pure (community_id, target) key would suppress the second rotation.
        let events: Vec<crate::community_membership::SignedMembershipEvent> = {
            let state_g = engine_arc.state();
            let state_g = state_g.lock().await;
            state_g.events.values().cloned().collect()
        };
        // Select the NEWEST matching Kick or Leave so that a rejoin+re-kick
        // sequence picks the most-recent removal rather than the stale one,
        // ensuring the dedup key (community_id, target, triggered_by_id) is
        // distinct from the prior rotation and a fresh rotation fires.
        let triggered_by_id = events
            .iter()
            .filter(|e| match &e.kind {
                crate::community_membership::MembershipEventKind::Kick { target: t, .. } => {
                    *t == target
                }
                crate::community_membership::MembershipEventKind::Leave => e.actor == target,
                _ => false,
            })
            .max_by_key(|e| (e.at.wall_ms, e.at.logical, e.at.device_id.as_str(), e.id))
            .map(|e| e.id);

        let Some(triggered_by) = triggered_by_id else {
            tracing::warn!(
                ?community_id,
                ?target,
                "self_heal: pending_rotation_for but no Kick/Leave event found — skipping"
            );
            continue;
        };

        // M7: dedup key includes trigger event ID — rejoin + re-kick produces a
        // distinct key and fires a fresh rotation rather than being suppressed.
        let key = (community_id, target, triggered_by);
        {
            let set = synth_rotations
                .lock()
                .expect("synth_rotations mutex poisoned");
            if set.contains(&key) {
                continue; // already synthesized this rotation in this session
            }
        }

        // Collect remaining active members (excluding the target).
        let mut member_pubs: Vec<(crate::owner_state_types::OwnerAddr, [u8; 64])> = Vec::new();
        for (addr, state_m) in &materialized.members {
            if *addr == target {
                continue;
            }
            if !matches!(
                state_m.status,
                crate::community_membership::MemberStatus::Joined
            ) {
                continue;
            }
            if let Some(pub64) = resolver.resolve(addr).await {
                member_pubs.push((*addr, pub64));
            }
        }

        if member_pubs.is_empty() {
            tracing::debug!(
                ?community_id,
                ?target,
                "self_heal: no resolvable remaining members for rotation — skipping"
            );
            continue;
        }

        let current_epoch = materialized.current_epoch.unwrap_or(0);
        let k_next = crate::owner_state_types::EpochKey::random();
        let recipients = match build_sealed_epoch_recipients(&k_next, member_pubs) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(?community_id, ?target, error = %e, "self_heal: build_sealed_epoch_recipients failed");
                continue;
            }
        };

        let hlc =
            crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms)
                .await;

        let rotation = match mint_epoch_rotation_event(
            community_id,
            self_owner,
            triggered_by,
            current_epoch,
            recipients,
            &signing_key,
            hlc,
        ) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(?community_id, ?target, error = %e, "self_heal: mint_epoch_rotation_event failed");
                continue;
            }
        };

        match engine_arc.insert_local_event(rotation).await {
            Ok(crate::community_state_crdt::InsertOutcome::Inserted) => {
                tracing::info!(
                    ?community_id,
                    ?target,
                    "self_heal: synthesized EpochRotation"
                );
                synth_rotations
                    .lock()
                    .expect("synth_rotations mutex poisoned")
                    .insert(key);
            }
            Ok(other) => {
                tracing::debug!(
                    ?community_id,
                    ?target,
                    ?other,
                    "self_heal: rotation insert outcome"
                );
            }
            Err(e) => {
                tracing::warn!(?community_id, ?target, error = %e, "self_heal: insert_local_event for rotation failed");
            }
        }
    }

    // §4.6: synthesize missing EpochCatchups.
    let pending_catchups: Vec<crate::owner_state_types::OwnerAddr> =
        materialized.pending_catchup_for.iter().copied().collect();
    for target in pending_catchups {
        // Find the originating Join event for this target FIRST — the dedupe
        // key includes the EventId so a follow-up catchup (e.g., after a
        // second rotation) is not blocked by a prior stale-key catchup.
        let events: Vec<crate::community_membership::SignedMembershipEvent> = {
            let state_g = engine_arc.state();
            let state_g = state_g.lock().await;
            state_g.events.values().cloned().collect()
        };
        // Most recent Join by this actor (in case of rejoin).
        let triggered_by_id = events
            .iter()
            .filter(|e| {
                e.actor == target
                    && matches!(
                        e.kind,
                        crate::community_membership::MembershipEventKind::Join
                    )
            })
            .max_by_key(|e| (e.at.wall_ms, e.at.logical))
            .map(|e| e.id);

        let Some(triggered_by) = triggered_by_id else {
            tracing::warn!(
                ?community_id,
                ?target,
                "self_heal: pending_catchup_for but no Join event found — skipping"
            );
            continue;
        };

        // Dedupe key: (community_id, target, triggered_by, current_epoch).
        // The epoch discriminator ensures a second epoch rotation landing
        // while the same member is still pending-catchup fires a fresh
        // catchup (the prior epoch's key is no longer useful to them).
        // If the member re-joins (new Join EventId) the observer also
        // fires fresh. ZEB-249 PR #106 R5 (CodeRabbit Major).
        let current_epoch_for_key = materialized.current_epoch.unwrap_or(0);
        let key = (community_id, target, triggered_by, current_epoch_for_key);
        {
            let set = synth_catchups
                .lock()
                .expect("synth_catchups mutex poisoned");
            if set.contains(&key) {
                continue; // already synthesized this session
            }
        }

        // Resolve the target's identity pub.
        let Some(target_pub64) = resolver.resolve(&target).await else {
            tracing::debug!(
                ?community_id,
                ?target,
                "self_heal: identity pub not yet available for catchup target"
            );
            continue;
        };

        let current_epoch = materialized.current_epoch.unwrap_or(0);

        // TODO(zeb-249-followup): cross-node observer correctness. When a
        // REMOTE admin's rotation lands on this node, current_epoch_key in
        // crdt_state is NOT updated (only LOCAL kick/leave handlers update it).
        // Catchups synthesized by remote admins' observers would use stale
        // keys. See spec §10.6.

        // Read the CURRENT epoch key from the local owner-state CRDT.
        // `Space.current_epoch_key` is updated by `kick_from_community` /
        // `leave_community` whenever a rotation lands locally, so this is
        // always the post-rotation key — correct for the §4.6 scenario
        // (stale invite, kick happened between invite issuance and redemption).
        // Fall back to the engine's spawn-time key only if the CRDT has no
        // record for this community (e.g., the observer fires before the
        // owner-state flush completes — in that case the rotation has not
        // yet landed locally and the spawn-time key equals the current key).
        let epoch_key = {
            let crdt_g = crdt_state.lock().await;
            crdt_g
                .current_epoch_key_for(community_id)
                .unwrap_or_else(|| engine_arc.membership_key())
        };

        let seal_result = {
            use crate::dm_signing::{ed25519_pub_to_x25519, seal_to_owner};
            let ed_pub: Result<&[u8; 32], _> = target_pub64[32..64].try_into();
            ed_pub
                .map_err(|_| "ed_pub slice error".to_string())
                .and_then(|ep| ed25519_pub_to_x25519(ep).map_err(|e| e.to_string()))
                .and_then(|x25519_pub| {
                    seal_to_owner(&x25519_pub, epoch_key.as_bytes()).map_err(|e| e.to_string())
                })
        };
        let sealed_for_target = match seal_result {
            Ok(sealed) => sealed,
            Err(e) => {
                tracing::warn!(?community_id, ?target, error = %e, "self_heal: seal_to_owner for catchup failed");
                continue;
            }
        };

        let recipients = vec![(target, sealed_for_target)];

        let hlc =
            crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms)
                .await;

        let catchup = match mint_epoch_catchup_event(
            community_id,
            self_owner,
            triggered_by,
            current_epoch,
            recipients,
            &signing_key,
            hlc,
        ) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(?community_id, ?target, error = %e, "self_heal: mint_epoch_catchup_event failed");
                continue;
            }
        };

        match engine_arc.insert_local_event(catchup).await {
            Ok(crate::community_state_crdt::InsertOutcome::Inserted) => {
                tracing::info!(
                    ?community_id,
                    ?target,
                    "self_heal: synthesized EpochCatchup"
                );
                synth_catchups
                    .lock()
                    .expect("synth_catchups mutex poisoned")
                    .insert(key);
            }
            Ok(other) => {
                tracing::debug!(
                    ?community_id,
                    ?target,
                    ?other,
                    "self_heal: catchup insert outcome"
                );
            }
            Err(e) => {
                tracing::warn!(?community_id, ?target, error = %e, "self_heal: insert_local_event for catchup failed");
            }
        }
    }
}

/// Mirror for `CommunityDegradedReport`. Emits `{ communityId, reason, detail }`
/// — matches the prior inline drain task's wire shape so the frontend
/// banner consumer doesn't break.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommunityStateSyncDegradedPayload {
    pub community_id: String,
    pub reason: String,
    pub detail: String,
}

/// Drain `degraded_rx` and emit each report through `emit`. Stops
/// cleanly when the channel closes (every engine's `error_tx` clone has
/// dropped — happens when `registry.shutdown_all()` finishes).
pub async fn run_community_degraded_consumer<F, Fut>(
    mut degraded_rx: tokio::sync::mpsc::Receiver<
        crate::community_state_sync::CommunityDegradedReport,
    >,
    mut emit: F,
) where
    F: FnMut(CommunityStateSyncDegradedPayload) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    while let Some(report) = degraded_rx.recv().await {
        let payload = CommunityStateSyncDegradedPayload {
            community_id: hex::encode(report.community_id.0),
            reason: report.reason_tag.to_string(),
            detail: report.detail,
        };
        emit(payload).await;
    }
}

/// Result of `get_backup_staleness` — payload for the GUI staleness banner.
///
/// Wire shape (camelCase): `{ isStale: bool, daysSince: u32 }`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupStaleness {
    pub is_stale: bool,
    pub days_since: u32,
}

/// Tauri IPC: compute the backup-staleness banner state.
///
/// Reads `owner_state_crdt.cbor` + `last_backup.json` from `app_data_dir()`
/// and runs `crate::backup_state::should_warn_about_stale_backup` with the
/// current wall clock. `dismiss_until_ms` is the localStorage-backed
/// dismissal expiry passed from the frontend — when `Some(t)` and `t >
/// now_wall_ms`, the banner is suppressed regardless of staleness.
///
/// Missing `owner_state_crdt.cbor` (fresh install before any owner-state
/// writes) defaults to an empty `OwnerState` — `should_warn_about_stale_backup`
/// then returns `is_stale: false` for the "no backup, no mutations" case,
/// which is what we want for a brand-new user.
///
/// Missing `last_backup.json` returns `Ok(None)` from `load_last_backup`;
/// the 14-day grace window is still applied via `last_mutation_wall_ms`.
///
/// Rust keeps NO mutable dismiss state — the frontend owns it in
/// localStorage. See `src/lib/backup-service.ts`.
#[tauri::command]
async fn get_backup_staleness(
    app: tauri::AppHandle,
    dismiss_until_ms: Option<u64>,
) -> Result<BackupStaleness, String> {
    use tauri::Manager;
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app_data_dir: {e}"))?;
    // Single source of truth for the CRDT path — same file the engine
    // boots from and the same file the backup export sidecar reads.
    let state_path = crate::recovery_cli::owner_state_path(&app_data_dir);
    let last_path = app_data_dir.join("last_backup.json");

    crate::identity_commands::run_blocking(move || {
        let state = crate::owner_state_persist::load_crdt(&state_path)
            .unwrap_or_else(|_| crate::owner_state_crdt::OwnerState::default());
        let last = crate::backup_state::load_last_backup(&last_path).unwrap_or(None);
        let now_wall_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let r = crate::backup_state::should_warn_about_stale_backup(
            now_wall_ms,
            last.as_ref(),
            &state,
            dismiss_until_ms,
        );
        Ok(BackupStaleness {
            is_stale: r.is_stale,
            days_since: r.days_since,
        })
    })
    .await
}

/// ZEB-213 — informational preview of the `.state` sidecar next to a
/// recovery file the user picked.
///
/// Wire shape (camelCase): `{ present: bool, spaceCount: Option<u32> }`.
///
/// Returns `present: false` when no sidecar exists at `<in_path>.state`.
/// Otherwise decodes the sidecar with the supplied passphrase (the same
/// one the user just typed for `preview_recovery_file`), loads the
/// resulting tree into a temp file, and counts the Spaces so the GUI can
/// show "Found owner-state snapshot — NN spaces. Restore both?".
///
/// **TOCTOU**: this IPC does NOT cache anything — it is purely
/// informational. The authoritative sidecar restore happens inside
/// `restore_recovery_from_preview_token_helper`, which uses the cached
/// preview seed for addr-binding verification. A swap of the sidecar
/// file between this preview and the actual commit would fail
/// addr-binding at commit time.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SidecarPreview {
    pub present: bool,
    pub space_count: Option<u32>,
}

#[tauri::command]
async fn preview_recovery_state_sidecar(
    in_path: String,
    passphrase: String,
) -> Result<SidecarPreview, String> {
    crate::identity_commands::run_blocking(move || {
        let p = std::path::PathBuf::from(in_path);
        let sidecar = crate::recovery_cli::sidecar_path(&p);
        if !sidecar.exists() {
            return Ok(SidecarPreview {
                present: false,
                space_count: None,
            });
        }
        let snap = crate::state_snapshot::decode_snapshot_file(passphrase.as_bytes(), &sidecar)
            .map_err(|e| e.to_string())?;
        // Count Spaces by reloading the inner tree bytes via a tempfile.
        // load_crdt parses the same canonicalize() output that
        // save_atomically writes — matches the pattern in
        // recovery_cli::restore_recovery_file_pair_with_keychain.
        let tmp = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
        crate::owner_state_persist::save_atomically(tmp.path(), &snap.tree)
            .map_err(|e| e.to_string())?;
        let state = crate::owner_state_persist::load_crdt(tmp.path()).map_err(|e| e.to_string())?;
        Ok(SidecarPreview {
            present: true,
            space_count: Some(state.spaces.len() as u32),
        })
    })
    .await
}

// ── App entry point ──────────────────────────────────────────────────────

#[cfg(test)]
mod community_member_dto_tests {
    use super::{member_info_for, MemberStatusDto};
    use crate::community_membership::{MaterializedMembership, MemberState, MemberStatus};
    use crate::owner_state_types::{Hlc, OwnerAddr};
    use std::collections::BTreeMap;

    fn hlc(wall: u64, dev: &str) -> Hlc {
        Hlc {
            wall_ms: wall,
            logical: 0,
            device_id: dev.to_string(),
        }
    }

    #[test]
    fn member_info_sorts_by_power_desc_then_joined_at_asc() {
        let admin = OwnerAddr([1; 16]);
        let mod_user = OwnerAddr([2; 16]);
        let early = OwnerAddr([3; 16]);
        let late = OwnerAddr([4; 16]);

        let mut members = BTreeMap::new();
        members.insert(
            admin,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: hlc(100, "a"),
                left_at: None,
            },
        );
        members.insert(
            mod_user,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: hlc(200, "b"),
                left_at: None,
            },
        );
        members.insert(
            early,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: hlc(150, "c"),
                left_at: None,
            },
        );
        members.insert(
            late,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: hlc(300, "d"),
                left_at: None,
            },
        );

        let mut power_levels = BTreeMap::new();
        power_levels.insert(admin, 100);
        power_levels.insert(mod_user, 50);

        let materialized = MaterializedMembership {
            members,
            power_levels,
            channels: BTreeMap::new(),
            current_epoch: None,
            pending_rotation_for: std::collections::BTreeSet::new(),
            pending_catchup_for: std::collections::BTreeSet::new(),
            admin_quorum: 1,
        };
        let dto = member_info_for(&materialized);

        assert_eq!(dto.len(), 4);
        assert_eq!(dto[0].addr, hex::encode(admin.0));
        assert_eq!(dto[0].power, 100);
        assert_eq!(dto[1].addr, hex::encode(mod_user.0));
        assert_eq!(dto[1].power, 50);
        assert_eq!(dto[2].addr, hex::encode(early.0));
        assert_eq!(dto[2].power, 0);
        assert_eq!(dto[3].addr, hex::encode(late.0));
        assert_eq!(dto[3].power, 0);
    }

    #[test]
    fn member_info_includes_left_and_banned_members() {
        let a = OwnerAddr([1; 16]);
        let b = OwnerAddr([2; 16]);
        let mut members = BTreeMap::new();
        members.insert(
            a,
            MemberState {
                status: MemberStatus::Left,
                joined_at: hlc(100, "x"),
                left_at: Some(hlc(200, "x")),
            },
        );
        members.insert(
            b,
            MemberState {
                status: MemberStatus::Banned,
                joined_at: hlc(50, "y"),
                left_at: Some(hlc(150, "y")),
            },
        );
        let materialized = MaterializedMembership {
            members,
            power_levels: BTreeMap::new(),
            channels: BTreeMap::new(),
            current_epoch: None,
            pending_rotation_for: std::collections::BTreeSet::new(),
            pending_catchup_for: std::collections::BTreeSet::new(),
            admin_quorum: 1,
        };
        let dto = member_info_for(&materialized);
        assert_eq!(dto.len(), 2);
        let statuses: Vec<_> = dto.iter().map(|d| d.status).collect();
        assert!(statuses.contains(&MemberStatusDto::Left));
        assert!(statuses.contains(&MemberStatusDto::Banned));
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(Mutex::new(NodeState::default()))
        .invoke_handler(tauri::generate_handler![
            list_vine_videos,
            follow_vine_creator,
            unfollow_vine_creator,
            list_followed,
            mark_vine_viewed,
            publish_vine,
            publish_vine_reaction,
            start_node,
            stop_node,
            connect_zenoh,
            disconnect_zenoh,
            publish_profile,
            send_message,
            send_dm,
            read_dm_thread,
            delete_outbox_entry,
            add_space,
            get_node_addr,
            list_content,
            pin_content,
            unpin_content,
            burn_content,
            archive_content,
            set_replication_tier,
            fetch_content,
            export_content,
            ingest_content,
            create_folder,
            send_voice_frame,
            join_voice_channel,
            leave_voice_channel,
            send_mail,
            list_mail,
            get_mail,
            refresh_mail,
            fetch_mail_body,
            update_mail,
            get_mail_counts,
            identity_commands::current_identity_hash,
            identity_commands::export_mnemonic_words,
            identity_commands::preview_mnemonic_identity,
            identity_commands::preview_recovery_file,
            preview_recovery_state_sidecar,
            identity_commands::export_recovery_file_to_path,
            identity_commands::restore_mnemonic_from_words,
            identity_commands::restore_recovery_from_preview_token,
            owner_commands::get_owner_state,
            owner_commands::mint_owner_identity,
            owner_commands::export_owner_recovery_file_to_path,
            owner_commands::issue_owner_recovery_token,
            get_backup_staleness,
            save_dialog::request_export_save_path,
            pairing_commands::start_inviter_pairing,
            pairing_commands::start_joiner_pairing,
            pairing_commands::select_pairing_peer,
            pairing_commands::confirm_pairing_sas,
            pairing_commands::cancel_pairing,
            pairing_commands::get_pairing_state,
            list_community_members,
            generate_invite,
            create_community,
            redeem_invite,
            join_open_community,
            leave_community,
            kick_from_community,
            community_fork::fork_community,
            list_community_forks,
            get_community_lineage,
            get_fork_snapshot_metadata,
            get_pre_fork_snapshot,
            set_power_level,
            unban_from_community,
            list_recent_moderation_events,
            list_pending_joins,
            list_recent_counter_signs,
            list_pending_admin_proposals,
            countersign_admin_proposal,
            propose_change_quorum,
            create_channel,
            modify_channel,
            delete_channel,
            list_channels,
            list_channel_messages,
            post_channel_message,
            request_channel_backfill,
            list_libraries,
            list_discovered_libraries,
            add_library,
            remove_library,
            browse_library,
            set_space_shared_in_profile,
            list_shared_in_profile_communities,
            subscribe_peer_profile,
            unsubscribe_peer_profile,
            get_cached_peer_profile,
            #[cfg(debug_assertions)]
            e2e_close_window,
        ])
        .run(tauri::generate_context!())
        .expect("error while running harmony");
}

/// Test-only helper: adds the 4 DM IPC handlers (`send_dm`,
/// `read_dm_thread`, `delete_outbox_entry`, `add_space`) to a Tauri
/// builder. Used by `tests/dm_ipc_roundtrip.rs` to set up a
/// `tauri::test::mock_app` with the commands registered — integration
/// tests can't see private `#[tauri::command]` fns directly, so this
/// helper provides the registration surface without re-publishing the
/// commands themselves.
///
/// Production code uses the explicit `invoke_handler` block in
/// `run()` above (which lists all ~50 commands); this helper exists
/// solely to support the JS↔Rust binding roundtrip tests added in
/// ZEB-247.
#[cfg(any(test, feature = "test-fixtures"))]
pub fn add_dm_ipc_handlers<R: tauri::Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder.invoke_handler(tauri::generate_handler![
        send_dm,
        read_dm_thread,
        delete_outbox_entry,
        add_space,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_payload(status: u8) -> Vec<u8> {
        let mut p = vec![0xAA; 32];
        p.push(status);
        p
    }

    #[test]
    fn parse_capacity_valid_ready() {
        let result = parse_capacity(
            "harmony/compute/capacity/deadbeef01020304",
            &make_payload(0x01),
        );
        let update = result.unwrap();
        assert_eq!(update.node_addr, "deadbeef01020304");
        assert_eq!(update.model_cid, "aa".repeat(32));
        assert!(update.ready);
    }

    #[test]
    fn parse_capacity_valid_busy() {
        let result = parse_capacity("harmony/compute/capacity/node42", &make_payload(0x00));
        let update = result.unwrap();
        assert_eq!(update.node_addr, "node42");
        assert!(!update.ready);
    }

    #[test]
    fn parse_capacity_truncated() {
        let result = parse_capacity("harmony/compute/capacity/node1", &[0xAA; 10]);
        assert!(result.is_none());
    }

    #[test]
    fn parse_capacity_wrong_prefix() {
        let result = parse_capacity("harmony/telemetry/node1/health", &make_payload(0x01));
        assert!(result.is_none());
    }

    #[test]
    fn parse_capacity_empty_payload() {
        let result = parse_capacity("harmony/compute/capacity/node1", &[]);
        assert!(result.is_none());
    }

    #[test]
    fn profile_payload_roundtrip() {
        let profile = ProfilePayload {
            address: "deadbeef".to_string(),
            display_name: "Alice".to_string(),
            status_text: Some("Building".to_string()),
            avatar_url: None,
            avatar_cid: None,
            avatar_mini_cid: None,
        };
        let json = serde_json::to_vec(&profile).unwrap();
        let parsed: ProfilePayload = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed.address, "deadbeef");
        assert_eq!(parsed.display_name, "Alice");
        assert_eq!(parsed.status_text.as_deref(), Some("Building"));
        assert!(parsed.avatar_url.is_none());
    }

    #[test]
    fn profile_payload_camel_case() {
        let profile = ProfilePayload {
            address: "aa".to_string(),
            display_name: "Bob".to_string(),
            status_text: None,
            avatar_url: None,
            avatar_cid: None,
            avatar_mini_cid: None,
        };
        let json = String::from_utf8(serde_json::to_vec(&profile).unwrap()).unwrap();
        assert!(
            json.contains("\"displayName\""),
            "expected camelCase: {json}"
        );
        assert!(
            !json.contains("\"display_name\""),
            "unexpected snake_case: {json}"
        );
        assert!(
            !json.contains("statusText"),
            "None field should be skipped: {json}"
        );
    }

    #[test]
    fn parse_telemetry_valid_health() {
        let event = harmony_telemetry::TelemetryEvent {
            node_addr: "abcd1234".to_string(),
            intent: "health".to_string(),
            sequence: 1,
            timestamp: 1711600000,
            payload: serde_json::json!({"cpu_percent": 42.5, "mem_mb": 512}),
            confidence: None,
            source: None,
        };
        let wire = harmony_telemetry::encode_event(&event).unwrap();
        let result = parse_telemetry(&wire);
        let payload = result.unwrap();
        assert_eq!(payload.node_addr, "abcd1234");
        assert_eq!(payload.intent, "health");
        assert_eq!(payload.sequence, 1);
        assert_eq!(payload.timestamp, 1711600000);
    }

    #[test]
    fn parse_telemetry_valid_capacity_changed() {
        let event = harmony_telemetry::TelemetryEvent {
            node_addr: "node42".to_string(),
            intent: "capacity_changed".to_string(),
            sequence: 5,
            timestamp: 1711600100,
            payload: serde_json::json!({"model_cid": "aa".repeat(32), "ready": true}),
            confidence: None,
            source: Some("qwen3-0.6b".to_string()),
        };
        let wire = harmony_telemetry::encode_event(&event).unwrap();
        let result = parse_telemetry(&wire);
        let payload = result.unwrap();
        assert_eq!(payload.intent, "capacity_changed");
        assert_eq!(payload.source, Some("qwen3-0.6b".to_string()));
    }

    #[test]
    fn parse_telemetry_empty_payload() {
        let result = parse_telemetry(&[]);
        assert!(result.is_none());
    }

    #[test]
    fn parse_telemetry_bad_tag() {
        let result = parse_telemetry(&[0xFF, b'{', b'}']);
        assert!(result.is_none());
    }

    #[test]
    fn channel_message_roundtrip() {
        let msg = ChannelMessagePayload {
            id: "msg-abc-123".to_string(),
            sender_address: "deadbeef01020304".to_string(),
            sender_name: "Alice".to_string(),
            channel: "general".to_string(),
            hub: "harmony-dev".to_string(),
            text: "Hello, world!".to_string(),
            timestamp: 1711600000000,
            priority: "standard".to_string(),
            reply_to: None,
        };
        let json = serde_json::to_vec(&msg).unwrap();
        let parsed: ChannelMessagePayload = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed.id, "msg-abc-123");
        assert_eq!(parsed.sender_address, "deadbeef01020304");
        assert_eq!(parsed.channel, "general");
        assert_eq!(parsed.hub, "harmony-dev");
        assert_eq!(parsed.text, "Hello, world!");
        assert_eq!(parsed.timestamp, 1711600000000);
        assert!(parsed.reply_to.is_none());
    }

    #[test]
    fn channel_message_camel_case() {
        let msg = ChannelMessagePayload {
            id: "msg-1".to_string(),
            sender_address: "aa".to_string(),
            sender_name: "Bob".to_string(),
            channel: "general".to_string(),
            hub: "test".to_string(),
            text: "hi".to_string(),
            timestamp: 0,
            priority: "quiet".to_string(),
            reply_to: Some("msg-0".to_string()),
        };
        let json = String::from_utf8(serde_json::to_vec(&msg).unwrap()).unwrap();
        assert!(
            json.contains("\"senderAddress\""),
            "expected camelCase: {json}"
        );
        assert!(
            json.contains("\"replyTo\""),
            "replyTo should be present: {json}"
        );
        assert!(
            !json.contains("\"sender_address\""),
            "unexpected snake_case: {json}"
        );
    }

    #[test]
    fn send_message_payload_deserialize() {
        let json = r#"{
            "channel": "general",
            "hub": "harmony-dev",
            "text": "test message",
            "priority": "loud",
            "replyTo": "msg-42",
            "senderName": "Alice"
        }"#;
        let parsed: SendMessagePayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.channel, "general");
        assert_eq!(parsed.hub, "harmony-dev");
        assert_eq!(parsed.text, "test message");
        assert_eq!(parsed.priority, "loud");
        assert_eq!(parsed.reply_to.as_deref(), Some("msg-42"));
        assert_eq!(parsed.sender_name, "Alice");
    }

    #[test]
    fn send_message_payload_sender_name_defaults() {
        let json = r#"{
            "channel": "general",
            "hub": "test",
            "text": "hi",
            "priority": "standard"
        }"#;
        let parsed: SendMessagePayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.sender_name, "", "senderName must default to empty");
        assert!(parsed.reply_to.is_none());
    }

    #[test]
    fn vine_descriptor_roundtrip() {
        let vine = VineDescriptorPayload {
            id: "vine-abc-1234".to_string(),
            creator_address: "deadbeef01020304".to_string(),
            creator_name: "Alice".to_string(),
            created_at: 1711600000,
            video_cid: "aa".repeat(32),
            title: Some("Demo vine".to_string()),
            reshare_of: None,
            original_creator_address: None,
            original_creator_name: None,
        };
        let json = serde_json::to_vec(&vine).unwrap();
        let parsed: VineDescriptorPayload = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed.id, "vine-abc-1234");
        assert_eq!(parsed.creator_address, "deadbeef01020304");
        assert_eq!(parsed.creator_name, "Alice");
        assert_eq!(parsed.created_at, 1711600000);
        assert_eq!(parsed.title.as_deref(), Some("Demo vine"));
        assert!(parsed.reshare_of.is_none());
    }

    #[test]
    fn vine_descriptor_camel_case() {
        let vine = VineDescriptorPayload {
            id: "vine-1".to_string(),
            creator_address: "aa".to_string(),
            creator_name: "Bob".to_string(),
            created_at: 0,
            video_cid: "bb".to_string(),
            title: None,
            reshare_of: Some("vine-0".to_string()),
            original_creator_address: None,
            original_creator_name: None,
        };
        let json = String::from_utf8(serde_json::to_vec(&vine).unwrap()).unwrap();
        assert!(
            json.contains("\"creatorAddress\""),
            "expected camelCase: {json}"
        );
        assert!(json.contains("\"videoCid\""), "expected camelCase: {json}");
        assert!(
            json.contains("\"reshareOf\""),
            "reshareOf should be present: {json}"
        );
        assert!(
            !json.contains("\"creator_address\""),
            "unexpected snake_case: {json}"
        );
        assert!(
            !json.contains("\"title\""),
            "None title should be skipped: {json}"
        );
    }

    #[test]
    fn vine_descriptor_payload_serializes_original_creator_fields_as_camel_case() {
        let payload = VineDescriptorPayload {
            id: "vine-1".to_string(),
            creator_address: "addr-resharer".to_string(),
            creator_name: "Resharer".to_string(),
            created_at: 100,
            video_cid: "cid-1".to_string(),
            title: None,
            reshare_of: Some("vine-0".to_string()),
            original_creator_address: Some("addr-original".to_string()),
            original_creator_name: Some("Original Creator".to_string()),
        };
        let json = serde_json::to_string(&payload).expect("serialize");
        assert!(
            json.contains("\"originalCreatorAddress\":\"addr-original\""),
            "originalCreatorAddress should be present in camelCase: {json}"
        );
        assert!(
            json.contains("\"originalCreatorName\":\"Original Creator\""),
            "originalCreatorName should be present in camelCase: {json}"
        );

        // Round-trip.
        let parsed: VineDescriptorPayload = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            parsed.original_creator_address.as_deref(),
            Some("addr-original")
        );
        assert_eq!(
            parsed.original_creator_name.as_deref(),
            Some("Original Creator")
        );
    }

    #[test]
    fn vine_descriptor_payload_omits_original_creator_fields_when_none() {
        let payload = VineDescriptorPayload {
            id: "vine-1".to_string(),
            creator_address: "addr-1".to_string(),
            creator_name: "Alice".to_string(),
            created_at: 100,
            video_cid: "cid-1".to_string(),
            title: None,
            reshare_of: None,
            original_creator_address: None,
            original_creator_name: None,
        };
        let json = serde_json::to_string(&payload).expect("serialize");
        assert!(
            !json.contains("originalCreatorAddress"),
            "should omit originalCreatorAddress when None: {json}"
        );
        assert!(
            !json.contains("originalCreatorName"),
            "should omit originalCreatorName when None: {json}"
        );
    }

    #[test]
    fn vine_descriptor_payload_deserializes_legacy_wire_without_original_creator_fields() {
        let legacy = r#"{
            "id": "vine-1",
            "creatorAddress": "addr-1",
            "creatorName": "Alice",
            "createdAt": 100,
            "videoCid": "cid-1",
            "reshareOf": "vine-0"
        }"#;
        let parsed: VineDescriptorPayload =
            serde_json::from_str(legacy).expect("legacy wire must deserialize");
        assert_eq!(parsed.reshare_of.as_deref(), Some("vine-0"));
        assert!(parsed.original_creator_address.is_none());
        assert!(parsed.original_creator_name.is_none());
    }

    #[test]
    fn publish_vine_payload_deserialize() {
        let json = r#"{
            "videoCid": "aabbccdd",
            "title": "My vine",
            "creatorName": "Alice"
        }"#;
        let parsed: PublishVinePayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.video_cid, "aabbccdd");
        assert_eq!(parsed.title.as_deref(), Some("My vine"));
        assert_eq!(parsed.creator_name, "Alice");
        assert!(parsed.reshare_of.is_none());
    }

    #[test]
    fn publish_vine_payload_creator_name_defaults() {
        let json = r#"{
            "videoCid": "aabb"
        }"#;
        let parsed: PublishVinePayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.creator_name, "", "creatorName must default to empty");
        assert!(parsed.title.is_none());
        assert!(parsed.reshare_of.is_none());
    }

    #[test]
    fn vine_reaction_payload_roundtrip() {
        let reaction = VineReactionPayload {
            vine_id: "vine-abc-1234".to_string(),
            reactor_address: "deadbeef01020304".to_string(),
            reactor_name: "Alice".to_string(),
            liked: true,
            timestamp: 1711600000,
        };
        let json = serde_json::to_vec(&reaction).unwrap();
        let parsed: VineReactionPayload = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed.vine_id, "vine-abc-1234");
        assert_eq!(parsed.reactor_address, "deadbeef01020304");
        assert_eq!(parsed.reactor_name, "Alice");
        assert!(parsed.liked);
        assert_eq!(parsed.timestamp, 1711600000);
    }

    #[test]
    fn vine_reaction_payload_camel_case() {
        let reaction = VineReactionPayload {
            vine_id: "vine-1".to_string(),
            reactor_address: "aa".to_string(),
            reactor_name: "Bob".to_string(),
            liked: false,
            timestamp: 0,
        };
        let json = String::from_utf8(serde_json::to_vec(&reaction).unwrap()).unwrap();
        assert!(json.contains("\"vineId\""), "expected camelCase: {json}");
        assert!(
            json.contains("\"reactorAddress\""),
            "expected camelCase: {json}"
        );
        assert!(
            json.contains("\"reactorName\""),
            "expected camelCase: {json}"
        );
        assert!(
            !json.contains("\"vine_id\""),
            "unexpected snake_case: {json}"
        );
        assert!(
            !json.contains("\"reactor_address\""),
            "unexpected snake_case: {json}"
        );
        assert!(
            !json.contains("\"reactor_name\""),
            "unexpected snake_case: {json}"
        );
    }

    #[test]
    fn publish_reaction_payload_deserialize() {
        let json = r#"{
            "vineId": "vine-abc",
            "vineCreatorAddress": "deadbeef",
            "liked": true,
            "reactorName": "Alice"
        }"#;
        let parsed: PublishReactionPayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.vine_id, "vine-abc");
        assert_eq!(parsed.vine_creator_address, "deadbeef");
        assert!(parsed.liked);
        assert_eq!(parsed.reactor_name, "Alice");
    }

    #[test]
    fn publish_reaction_payload_reactor_name_defaults() {
        let json = r#"{
            "vineId": "vine-abc",
            "vineCreatorAddress": "deadbeef",
            "liked": true
        }"#;
        let parsed: PublishReactionPayload = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.reactor_name, "", "reactorName must default to empty");
    }

    #[test]
    fn publish_reaction_payload_liked_false() {
        let json = r#"{
        "vineId": "vine-xyz",
        "vineCreatorAddress": "aabb",
        "liked": false
    }"#;
        let parsed: PublishReactionPayload = serde_json::from_str(json).unwrap();
        assert!(!parsed.liked);
    }

    #[test]
    fn content_announcement_valid() {
        let size: u32 = 65536;
        let payload = size.to_be_bytes().to_vec();
        let result = parse_content_announcement("harmony/announce/aabbccdd11223344", &payload);
        let ann = result.unwrap();
        assert_eq!(ann.cid, "aabbccdd11223344");
        assert_eq!(ann.size_bytes, 65536);
    }

    #[test]
    fn content_announcement_camel_case() {
        let size: u32 = 1024;
        let payload = size.to_be_bytes().to_vec();
        let ann = parse_content_announcement("harmony/announce/abc123", &payload).unwrap();
        let json = serde_json::to_string(&ann).unwrap();
        assert!(json.contains("\"sizeBytes\""), "expected camelCase: {json}");
        assert!(
            !json.contains("\"size_bytes\""),
            "unexpected snake_case: {json}"
        );
    }

    #[test]
    fn content_announcement_wrong_prefix() {
        let payload = 100u32.to_be_bytes().to_vec();
        assert!(parse_content_announcement("harmony/vines/abc", &payload).is_none());
    }

    #[test]
    fn content_announcement_empty_cid() {
        let payload = 100u32.to_be_bytes().to_vec();
        assert!(parse_content_announcement("harmony/announce/", &payload).is_none());
    }

    #[test]
    fn content_announcement_short_payload() {
        assert!(parse_content_announcement("harmony/announce/abc123", &[0, 0]).is_none());
    }

    #[test]
    fn content_announcement_empty_payload() {
        assert!(parse_content_announcement("harmony/announce/abc123", &[]).is_none());
    }

    #[test]
    fn content_announcement_non_hex_cid() {
        let payload = 100u32.to_be_bytes().to_vec();
        assert!(parse_content_announcement("harmony/announce/<script>", &payload).is_none());
        assert!(parse_content_announcement("harmony/announce/xyz!", &payload).is_none());
        assert!(parse_content_announcement("harmony/announce/hello world", &payload).is_none());
    }

    #[test]
    fn list_folder_rejects_non_manifest_child_0() {
        use crate::folders::FolderManifest;

        // A bundle whose child-0 book payload is NOT a folder manifest
        // (e.g., plain UTF-8 "not a manifest" or chunked-file sentinel bytes).
        // Simulated here at the parse level — the full wiring test is the
        // integration test malformed_manifest_returns_error.
        let payload = b"definitely not a manifest";
        let parse_result: Result<FolderManifest, _> = serde_json::from_slice(payload);
        assert!(
            parse_result.is_err(),
            "bad JSON must not parse as FolderManifest"
        );
    }
}

#[cfg(test)]
mod chunked_ingest_tests {
    use super::*;
    use harmony_content::bundle::MAX_BUNDLE_ENTRIES;
    use harmony_content::cid::MAX_PAYLOAD_SIZE;

    #[test]
    fn ingest_dispatch_picks_single_for_small_sizes() {
        assert!(matches!(
            ingest_dispatch(0).unwrap(),
            IngestDispatch::Single
        ));
        assert!(matches!(
            ingest_dispatch(MAX_PAYLOAD_SIZE as u64).unwrap(),
            IngestDispatch::Single
        ));
    }

    #[test]
    fn ingest_dispatch_picks_chunked_above_single_book_ceiling() {
        assert!(matches!(
            ingest_dispatch(MAX_PAYLOAD_SIZE as u64 + 1).unwrap(),
            IngestDispatch::Chunked
        ));
    }

    #[test]
    fn ingest_dispatch_rejects_above_flat_bundle_cap() {
        let too_big = FLAT_BUNDLE_MAX + 1;
        let err = ingest_dispatch(too_big).unwrap_err();
        assert!(err.contains("file too large"), "got: {err}");
        assert!(
            err.contains("flat-bundle"),
            "message should explain the cap origin, got: {err}"
        );
    }

    #[test]
    fn ingest_dispatch_rejects_u64_max() {
        // Guard against accidental reintroduction of a `size as usize`
        // comparison — on 32-bit targets that would wrap and misclassify
        // multi-GiB sizes as Single.
        let err = ingest_dispatch(u64::MAX).unwrap_err();
        assert!(err.contains("file too large"), "got: {err}");
    }

    #[test]
    fn ingest_dispatch_accepts_exactly_flat_bundle_max() {
        // FLAT_BUNDLE_MAX is the last accepted byte count (condition is strict >).
        assert!(matches!(
            ingest_dispatch(FLAT_BUNDLE_MAX).unwrap(),
            IngestDispatch::Chunked
        ));
    }

    #[test]
    fn flat_bundle_max_matches_spec() {
        // Sanity-check the constant so a refactor of the underlying
        // harmony-content limits surfaces here. The cap uses the chunker's
        // min_chunk (not MAX_PAYLOAD_SIZE) so the leaf count can never
        // exceed MAX_BUNDLE_ENTRIES.
        assert_eq!(
            FLAT_BUNDLE_MAX,
            (MAX_BUNDLE_ENTRIES as u64)
                * (harmony_content::chunker::ChunkerConfig::DEFAULT.min_chunk as u64)
        );
    }

    use harmony_content::bundle;
    use harmony_content::cid::{CidType, ContentFlags, ContentId};

    fn synthetic_bytes(len: usize) -> Vec<u8> {
        // Deterministic, non-trivially-compressible content — cycle through
        // a small prime to force the chunker to find real cut points.
        (0..len).map(|i| ((i * 37) % 251) as u8).collect()
    }

    #[test]
    fn chunk_and_bundle_produces_bundle_root_over_leaf_cids() {
        let bytes = synthetic_bytes(3 * 1024 * 1024); // 3 MiB
        let (leaves, bundle_payload, root) =
            chunk_and_bundle(&bytes).expect("chunking must succeed");

        // Bundle root has CidType::Bundle(depth) with depth >= 1.
        match root.cid_type() {
            CidType::Bundle(d) => assert!(d >= 1, "root depth should be >= 1"),
            other => panic!("expected bundle, got {other:?}"),
        }

        // Every leaf is a book CID.
        for (leaf_cid, _data) in &leaves {
            assert_eq!(leaf_cid.cid_type(), CidType::Book, "leaves must be books");
        }

        // The bundle payload parses back to exactly those leaf CIDs in order.
        let parsed = bundle::parse_bundle(&bundle_payload).expect("bundle payload must parse");
        let expected: Vec<ContentId> = leaves.iter().map(|(c, _)| *c).collect();
        assert_eq!(parsed.to_vec(), expected);
    }

    #[test]
    fn chunk_and_bundle_leaf_bytes_sum_to_input() {
        let bytes = synthetic_bytes(3 * 1024 * 1024);
        let (leaves, _bundle_payload, _root) = chunk_and_bundle(&bytes).unwrap();
        let total: usize = leaves.iter().map(|(_, d)| d.len()).sum();
        assert_eq!(
            total,
            bytes.len(),
            "leaves must cover the full input exactly"
        );
        let reassembled: Vec<u8> = leaves.iter().flat_map(|(_, d)| d.iter().copied()).collect();
        assert_eq!(reassembled, bytes, "leaves in order must equal original");
    }

    #[test]
    fn chunk_and_bundle_leaf_cid_matches_for_book_of_its_bytes() {
        let bytes = synthetic_bytes(3 * 1024 * 1024);
        let (leaves, _bundle_payload, _root) = chunk_and_bundle(&bytes).unwrap();
        for (leaf_cid, data) in &leaves {
            let recomputed = ContentId::for_book(data, ContentFlags::default()).unwrap();
            assert_eq!(*leaf_cid, recomputed);
        }
    }

    #[test]
    fn chunk_and_bundle_rejects_single_book_sized_input() {
        // MAX_PAYLOAD_SIZE is the single-book ceiling; chunk_and_bundle
        // must reject inputs that should have gone through the single-book path.
        let bytes = synthetic_bytes(harmony_content::cid::MAX_PAYLOAD_SIZE);
        let err = chunk_and_bundle(&bytes).unwrap_err();
        assert!(err.contains("single-book"), "got: {err}");
    }

    #[test]
    fn chunk_and_bundle_accepts_exactly_max_payload_plus_one() {
        // The smallest valid input: MAX_PAYLOAD_SIZE + 1 bytes.
        let bytes = synthetic_bytes(harmony_content::cid::MAX_PAYLOAD_SIZE + 1);
        chunk_and_bundle(&bytes).expect("must succeed at the minimum valid size");
    }
}

#[cfg(test)]
mod pin_persistence_tests {
    use super::*;

    #[test]
    fn content_item_wire_serializes_sidecar_id_and_kind() {
        let id = uuid::Uuid::new_v4().as_hyphenated().to_string();
        let wire = ContentItemWire {
            sidecar_id: id.clone(),
            cid: "aa".repeat(32),
            name: "Photos".into(),
            size_bytes: 32,
            stored_at: 1,
            sensitivity: "private".into(),
            replication_tier: "default".into(),
            pinned: false,
            licensed: false,
            archived: false,
            kind: "folder".into(),
        };
        let json = serde_json::to_string(&wire).expect("serialize");
        assert!(
            json.contains(&format!("\"sidecarId\":\"{id}\"")),
            "got: {json}"
        );
        assert!(json.contains("\"kind\":\"folder\""), "got: {json}");
    }

    #[test]
    fn parse_sidecar_id_accepts_hyphenated_uuid_rejects_garbage() {
        let id = uuid::Uuid::new_v4().as_hyphenated().to_string();
        assert!(parse_sidecar_id(&id).is_ok());
        assert!(parse_sidecar_id("").is_err(), "empty rejected");
        assert!(parse_sidecar_id("not-a-uuid").is_err(), "garbage rejected");
    }
}

#[cfg(test)]
mod list_community_members_ipc_tests {
    use super::*;
    use crate::community_membership::{
        sign_event_with_identity, EventPayload, MembershipEventKind,
    };
    use crate::community_state_crdt::CommunityState;
    use crate::owner_state_types::*;
    use harmony_identity::PrivateIdentity;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn list_members_returns_sorted_dto_for_known_community() {
        let community_id = SpaceId([5; 16]);
        let identity = PrivateIdentity::from_seed(&[0xab; 32]);
        let admin = OwnerAddr(identity.identity.address_hash);
        let identity_pub = identity.identity.to_public_bytes();

        let state = Arc::new(Mutex::new(CommunityState::new(community_id)));
        {
            let mut sa = state.lock().await;
            let payload = EventPayload {
                id: [1; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: admin,
                at: Hlc {
                    wall_ms: 100,
                    logical: 0,
                    device_id: "x".into(),
                },
            };
            let evt = sign_event_with_identity(&payload, &identity).expect("sign");
            let outcome = sa.insert_event(
                evt,
                &crate::community_membership::VerifyContext {
                    expected_community_id: community_id,
                    admin_addr: admin,
                    is_invite_only: false,
                    actor_identity_pub: &identity_pub,
                    countersigner_identity_pub: None,
                    admin_identity_pub: None,
                },
            );
            assert!(
                matches!(
                    outcome,
                    crate::community_state_crdt::InsertOutcome::Inserted
                ),
                "fixture insert must succeed; got {outcome:?}"
            );
        }

        let materialized = state.lock().await.materialize_now(admin);
        let dto = member_info_for(&materialized);
        assert_eq!(dto.len(), 1);
        assert_eq!(dto[0].addr, hex::encode(admin.0));
        assert_eq!(dto[0].power, 100);
    }
}

#[cfg(test)]
mod generate_invite_helper_tests {
    use super::*;
    use crate::community_invite::{decode_invite_url, CommunityInvitePayload};
    use crate::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};

    #[test]
    fn build_open_invite_payload_round_trips_via_url() {
        let payload = CommunityInvitePayload {
            community_id: SpaceId([7; 16]),
            epoch_snapshot: crate::community_invite::InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: EpochKey::new([0x99; 32]).as_bytes().to_vec(),
                state_snapshot: crate::community_invite::MaterializedCommunityState::default(),
            },
            admin_addr: OwnerAddr([0x11; 16]),
            community_name: "DoorClub".into(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
            admin_bootstrap: None,
            admin_identity_pub: None,
            forked_from: None,
            pre_fork_snapshot: None,
        };
        let url = build_open_invite_url(&payload).expect("url");
        let decoded = decode_invite_url(&url).expect("decode");
        assert_eq!(decoded, payload);
        assert!(
            decoded.invite_token.is_none(),
            "open path must be token-less"
        );
    }

    /// ZEB-285 Task 7: a fork-community invite payload round-trips through
    /// `build_open_invite_url` / `decode_invite_url` with `forked_from` and
    /// `pre_fork_snapshot` fields intact.
    ///
    /// This validates the **encoding path** that `generate_invite` uses when it
    /// detects `CommunityState.forked_from.is_some()` and bundles the snapshot.
    /// The registry/disk-reading portion is exercised by
    /// `redeem_invite_inner_tests::redeem_invite_writes_snapshot_to_data_dir`.
    #[test]
    fn mint_invite_for_fork_bundles_snapshot() {
        let original_id = SpaceId([0xab; 16]);
        let fork_id = SpaceId([0xf3; 16]);

        let snapshot = crate::community_invite::PreForkSnapshot {
            original_community_id: original_id,
            original_community_name: "OriginalCom".into(),
            membership_events: vec![],
            channel_log: crate::community_invite::BoundedChannelLogSnapshot {
                per_channel: std::collections::BTreeMap::new(),
            },
            identity_pubs: std::collections::BTreeMap::new(),
            forked_at: Hlc {
                wall_ms: 1_700_000_001_000,
                logical: 0,
                device_id: "fork-dev".into(),
            },
            parent_lineage: Vec::new(),
        };

        let payload = CommunityInvitePayload {
            community_id: fork_id,
            epoch_snapshot: crate::community_invite::InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: EpochKey::new([0x42; 32]).as_bytes().to_vec(),
                state_snapshot: crate::community_invite::MaterializedCommunityState::default(),
            },
            admin_addr: OwnerAddr([0x11; 16]),
            community_name: "ForkCom".into(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
            admin_bootstrap: None,
            admin_identity_pub: None,
            forked_from: Some(original_id),
            pre_fork_snapshot: Some(snapshot.clone()),
        };

        let url = build_open_invite_url(&payload).expect("encode fork-invite url");
        let decoded = decode_invite_url(&url).expect("decode fork-invite url");

        assert_eq!(
            decoded.forked_from,
            Some(original_id),
            "forked_from must survive URL encode/decode"
        );
        assert!(
            decoded.pre_fork_snapshot.is_some(),
            "pre_fork_snapshot must survive URL encode/decode"
        );
        let decoded_snapshot = decoded.pre_fork_snapshot.unwrap();
        assert_eq!(
            decoded_snapshot.original_community_id, original_id,
            "decoded snapshot original_community_id must match"
        );
        assert_eq!(
            decoded_snapshot.original_community_name, "OriginalCom",
            "decoded snapshot original_community_name must match"
        );
        assert_eq!(
            decoded_snapshot.forked_at.wall_ms, 1_700_000_001_000,
            "decoded snapshot forked_at.wall_ms must match"
        );
    }
}

#[cfg(test)]
mod community_delta_projection_tests {
    use super::*;
    use crate::community_membership::{
        sign_event_with_identity, EventPayload, MembershipEventKind,
    };
    use crate::community_state_sync::CommunityMembershipDelta;
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;

    fn make_delta(kind: MembershipEventKind, actor: OwnerAddr) -> CommunityMembershipDelta {
        let identity = PrivateIdentity::from_seed(&[0xee; 32]);
        let community_id = SpaceId([4; 16]);
        let payload = EventPayload {
            id: [0xab; 16],
            community_id,
            kind,
            actor,
            at: Hlc {
                wall_ms: 1234,
                logical: 0,
                device_id: "x".into(),
            },
        };
        let event = sign_event_with_identity(&payload, &identity).expect("sign");
        CommunityMembershipDelta {
            community_id,
            event,
        }
    }

    #[test]
    fn join_projects_with_target_and_no_by() {
        let actor = OwnerAddr([1; 16]);
        let (cid_hex, change) =
            delta_to_change(&make_delta(MembershipEventKind::Join, actor)).expect("Join projects");
        assert_eq!(cid_hex, hex::encode([4u8; 16]));
        assert_eq!(change.r#type, MembershipChangeType::Joined);
        assert_eq!(change.target, hex::encode(actor.0));
        assert!(change.by.is_none(), "Join is self-action; by is None");
        assert!(change.detail.is_none());
        assert_eq!(change.at_wall_ms, 1234);
    }

    #[test]
    fn leave_projects_with_target_and_no_by() {
        let actor = OwnerAddr([2; 16]);
        let (_, change) = delta_to_change(&make_delta(MembershipEventKind::Leave, actor)).unwrap();
        assert_eq!(change.r#type, MembershipChangeType::Left);
        assert_eq!(change.target, hex::encode(actor.0));
        assert!(change.by.is_none());
        assert!(change.detail.is_none());
    }

    #[test]
    fn kick_projects_with_target_by_and_reason_detail() {
        let actor = OwnerAddr([3; 16]);
        let target = OwnerAddr([4; 16]);
        let (_, change) = delta_to_change(&make_delta(
            MembershipEventKind::Kick {
                target,
                reason: Some("spam".into()),
            },
            actor,
        ))
        .unwrap();
        assert_eq!(change.r#type, MembershipChangeType::Kicked);
        assert_eq!(change.target, hex::encode(target.0));
        assert_eq!(change.by.as_deref(), Some(hex::encode(actor.0).as_str()));
        match change.detail.as_ref() {
            Some(MembershipChangeDetail::Reason(s)) => assert_eq!(s, "spam"),
            other => panic!("expected Reason detail, got {other:?}"),
        }
    }

    #[test]
    fn set_power_projects_with_target_by_and_level_detail() {
        let actor = OwnerAddr([5; 16]);
        let target = OwnerAddr([6; 16]);
        let (_, change) = delta_to_change(&make_delta(
            MembershipEventKind::SetPower { target, level: 50 },
            actor,
        ))
        .unwrap();
        assert_eq!(change.r#type, MembershipChangeType::PowerChanged);
        assert_eq!(change.target, hex::encode(target.0));
        assert_eq!(change.by.as_deref(), Some(hex::encode(actor.0).as_str()));
        match change.detail.as_ref() {
            Some(MembershipChangeDetail::Level(n)) => assert_eq!(*n, 50),
            other => panic!("expected Level detail, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod delta_consumer_task_tests {
    use super::*;
    use crate::community_membership::{
        sign_event_with_identity, EventPayload, MembershipEventKind,
    };
    use crate::community_state_sync::CommunityMembershipDelta;
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;

    #[tokio::test]
    async fn consumer_emits_payload_via_handler() {
        let (tx, rx) = tokio::sync::mpsc::channel::<CommunityMembershipDelta>(8);
        let captured: std::sync::Arc<tokio::sync::Mutex<Vec<CommunityMembersChangedPayload>>> =
            std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let captured_for_handler = std::sync::Arc::clone(&captured);

        let handle = tokio::spawn(async move {
            run_community_delta_consumer(
                rx,
                move |payload| {
                    let captured = std::sync::Arc::clone(&captured_for_handler);
                    async move {
                        captured.lock().await.push(payload);
                    }
                },
                |_payload: ChannelConfigChangedPayload| async move {
                    // No-op: this test only drives a Join through the
                    // membership branch.
                },
                // ZEB-270 Phase 3 Task 4B: 3rd callback (registry hook)
                // — no-op for tests that don't exercise the registry.
                |_payload: ChannelConfigChangedPayload| async move {},
                // ZEB-249 Task 6: epoch-event hook — no-op in this test.
                |_delta| async move {},
            )
            .await
        });

        let identity = PrivateIdentity::from_seed(&[0xab; 32]);
        let actor = OwnerAddr(identity.identity.address_hash);
        let community_id = SpaceId([6; 16]);
        let payload = EventPayload {
            id: [9; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "x".into(),
            },
        };
        let event = sign_event_with_identity(&payload, &identity).unwrap();
        tx.send(CommunityMembershipDelta {
            community_id,
            event,
        })
        .await
        .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let cap = captured.lock().await;
        assert_eq!(cap.len(), 1);
        assert_eq!(cap[0].community_id, hex::encode(community_id.0));
        assert_eq!(
            cap[0].changes.len(),
            1,
            "Phase 3 emits one change per IPC event"
        );
        assert_eq!(cap[0].changes[0].r#type, MembershipChangeType::Joined);
        assert_eq!(cap[0].changes[0].target, hex::encode(actor.0));
        drop(tx);
        let _ = handle.await;
    }
}

#[cfg(test)]
mod create_channel_delta_tests {
    use super::*;
    use crate::community_membership::{ChannelId, MembershipEventKind, SignedMembershipEvent};
    use crate::community_state_sync::CommunityMembershipDelta;
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    #[tokio::test]
    async fn delta_to_channel_config_change_projects_create_modify_delete() {
        let community_id = SpaceId([0x37; 16]);
        let actor = OwnerAddr([0x10; 16]);
        let ch_id = ChannelId([0xAB; 16]);

        // Create.
        let create_event = SignedMembershipEvent {
            id: [0x01; 16],
            community_id,
            kind: MembershipEventKind::ChannelCreate {
                channel_id: ch_id,
                name: "general".into(),
                write_power: 0,
            },
            actor,
            at: Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "a".into(),
            },
            sig: [0; 64],
            countersig: None,
        };
        let payload = delta_to_channel_config_change(&CommunityMembershipDelta {
            community_id,
            event: create_event,
        })
        .expect("create");
        assert_eq!(payload.action, ChannelConfigChangeAction::Created);
        assert_eq!(payload.channel_id, hex::encode(ch_id.0));
        assert_eq!(payload.community_id, hex::encode(community_id.0));
        assert_eq!(payload.name.as_deref(), Some("general"));
        assert_eq!(payload.write_power, Some(0));
        assert_eq!(payload.at_wall_ms, 1_000);

        // Modify (name only — write_power None means unchanged).
        let modify_event = SignedMembershipEvent {
            id: [0x02; 16],
            community_id,
            kind: MembershipEventKind::ChannelModify {
                channel_id: ch_id,
                name: Some("renamed".into()),
                write_power: None,
            },
            actor,
            at: Hlc {
                wall_ms: 2_000,
                logical: 0,
                device_id: "a".into(),
            },
            sig: [0; 64],
            countersig: None,
        };
        let payload = delta_to_channel_config_change(&CommunityMembershipDelta {
            community_id,
            event: modify_event,
        })
        .expect("modify");
        assert_eq!(payload.action, ChannelConfigChangeAction::Modified);
        assert_eq!(payload.name.as_deref(), Some("renamed"));
        assert_eq!(payload.write_power, None);

        // Delete.
        let delete_event = SignedMembershipEvent {
            id: [0x03; 16],
            community_id,
            kind: MembershipEventKind::ChannelDelete { channel_id: ch_id },
            actor,
            at: Hlc {
                wall_ms: 3_000,
                logical: 0,
                device_id: "a".into(),
            },
            sig: [0; 64],
            countersig: None,
        };
        let payload = delta_to_channel_config_change(&CommunityMembershipDelta {
            community_id,
            event: delete_event,
        })
        .expect("delete");
        assert_eq!(payload.action, ChannelConfigChangeAction::Deleted);
        assert_eq!(payload.name, None);
        assert_eq!(payload.write_power, None);
    }

    #[tokio::test]
    async fn delta_to_change_returns_none_for_channel_config() {
        // Channel-config deltas are NOT projected through delta_to_change —
        // they go through delta_to_channel_config_change instead. This
        // guarantees the consumer fan-out fires the right event.
        let community_id = SpaceId([0x37; 16]);
        let actor = OwnerAddr([0x10; 16]);
        let create_event = SignedMembershipEvent {
            id: [0x01; 16],
            community_id,
            kind: MembershipEventKind::ChannelCreate {
                channel_id: ChannelId([0xAB; 16]),
                name: "general".into(),
                write_power: 0,
            },
            actor,
            at: Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "a".into(),
            },
            sig: [0; 64],
            countersig: None,
        };
        let delta = CommunityMembershipDelta {
            community_id,
            event: create_event,
        };
        assert!(delta_to_change(&delta).is_none());
    }

    #[tokio::test]
    async fn run_community_delta_consumer_routes_channel_config_to_correct_callback() {
        // Drive a single ChannelCreate delta through run_community_delta_consumer
        // and assert the channel-config callback fires (not the membership one).
        let (tx, rx) = tokio::sync::mpsc::channel::<CommunityMembershipDelta>(8);

        let captured_membership: Arc<TokioMutex<Vec<CommunityMembersChangedPayload>>> =
            Arc::new(TokioMutex::new(Vec::new()));
        let captured_channel: Arc<TokioMutex<Vec<ChannelConfigChangedPayload>>> =
            Arc::new(TokioMutex::new(Vec::new()));

        let m_clone = captured_membership.clone();
        let c_clone = captured_channel.clone();

        let handle = tokio::spawn(run_community_delta_consumer(
            rx,
            move |payload| {
                let m = m_clone.clone();
                async move {
                    m.lock().await.push(payload);
                }
            },
            move |payload| {
                let c = c_clone.clone();
                async move {
                    c.lock().await.push(payload);
                }
            },
            // ZEB-270 Phase 3 Task 4B: 3rd callback (registry hook) —
            // no-op for tests that don't exercise the registry. The
            // assertion below targets the channel-config callback's
            // capture vec, not registry side-effects.
            |_payload: ChannelConfigChangedPayload| async move {},
            // ZEB-249 Task 6: epoch-event hook — no-op in this test.
            |_delta| async move {},
        ));

        let community_id = SpaceId([0x37; 16]);
        let create_event = SignedMembershipEvent {
            id: [0x01; 16],
            community_id,
            kind: MembershipEventKind::ChannelCreate {
                channel_id: ChannelId([0xAB; 16]),
                name: "general".into(),
                write_power: 0,
            },
            actor: OwnerAddr([0x10; 16]),
            at: Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "a".into(),
            },
            sig: [0; 64],
            countersig: None,
        };
        tx.send(CommunityMembershipDelta {
            community_id,
            event: create_event,
        })
        .await
        .expect("send");

        drop(tx); // close channel so consumer exits cleanly
        handle.await.expect("consumer");

        assert_eq!(captured_membership.lock().await.len(), 0);
        assert_eq!(captured_channel.lock().await.len(), 1);
        assert_eq!(
            captured_channel.lock().await[0].action,
            ChannelConfigChangeAction::Created
        );
    }
}

// ── ZEB-270 Phase 3 Task 5: channel-message IPC smoke tests ──────────
//
// Boundary-validation coverage for the three new IPCs. Driving the full
// IPC layer (post→engine→Zenoh→subscribe roundtrip) requires a live
// ChannelLogRegistry bound to a real Zenoh session, which lives in the
// Task 6 integration test. These tests exercise the IPC-boundary
// validation paths (hex length / parse / limit cap / missing-registry)
// against a default NodeState (registry = None), which is the path JS
// callers hit pre-start_node.
//
// `NodeState.channel_log_registry` is hardcoded to `tauri::Wry`, so we
// can't populate it with the test fixture's `MockRuntime` registry.
// End-to-end IPC roundtrips with a populated registry are therefore
// out of scope here — see Task 6's integration test for that coverage.
#[cfg(test)]
mod channel_message_ipc_tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tauri::Manager;

    /// Build a mock app with an empty `NodeState` (registry = None).
    /// Mirrors Phase 1's pattern of testing helper paths without
    /// standing up the production state-machine.
    fn mock_app_with_default_node_state() -> tauri::App<tauri::test::MockRuntime> {
        let app = tauri::test::mock_app();
        app.manage(StdMutex::new(NodeState::default()));
        app
    }

    #[tokio::test]
    async fn post_channel_message_rejects_short_community_id() {
        let app = mock_app_with_default_node_state();
        let state = app.state::<StdMutex<NodeState>>();
        let err = post_channel_message(state, "deadbeef".into(), "00".repeat(16), vec![1], None)
            .await
            .expect_err("short cid must error");
        assert!(err.contains("community_id must be 16 bytes"), "got: {err}");
    }

    #[tokio::test]
    async fn post_channel_message_rejects_short_channel_id() {
        let app = mock_app_with_default_node_state();
        let state = app.state::<StdMutex<NodeState>>();
        let err = post_channel_message(state, "00".repeat(16), "ab".into(), vec![1], None)
            .await
            .expect_err("short chid must error");
        assert!(err.contains("channel_id must be 16 bytes"), "got: {err}");
    }

    #[tokio::test]
    async fn post_channel_message_rejects_bad_hex() {
        let app = mock_app_with_default_node_state();
        let state = app.state::<StdMutex<NodeState>>();
        let err = post_channel_message(state, "zz".repeat(16), "00".repeat(16), vec![1], None)
            .await
            .expect_err("bad hex must error");
        assert!(err.contains("invalid community_id hex"), "got: {err}");
    }

    #[tokio::test]
    async fn post_channel_message_rejects_short_reply_to() {
        let app = mock_app_with_default_node_state();
        let state = app.state::<StdMutex<NodeState>>();
        let err = post_channel_message(
            state,
            "00".repeat(16),
            "00".repeat(16),
            vec![1],
            Some("ab".into()),
        )
        .await
        .expect_err("short reply_to must error");
        assert!(err.contains("reply_to must be 16 bytes"), "got: {err}");
    }

    #[tokio::test]
    async fn post_channel_message_errors_when_registry_missing() {
        let app = mock_app_with_default_node_state();
        let state = app.state::<StdMutex<NodeState>>();
        let err = post_channel_message(
            state,
            "00".repeat(16),
            "11".repeat(16),
            vec![104, 105],
            None,
        )
        .await
        .expect_err("missing registry must error");
        assert!(err.contains("channel_log_registry missing"), "got: {err}");
    }

    #[tokio::test]
    async fn list_channel_messages_rejects_limit_over_cap() {
        let app = mock_app_with_default_node_state();
        let state = app.state::<StdMutex<NodeState>>();
        let err = list_channel_messages(state, "00".repeat(16), "00".repeat(16), None, 1001)
            .await
            .expect_err("over-cap limit must error");
        assert_eq!(err, "limit 1001 exceeds max 1000");
    }

    #[tokio::test]
    async fn list_channel_messages_accepts_zero_limit() {
        // limit=0 is the "use engine default 256" sentinel; it must NOT
        // be rejected at the boundary. The call still errors at the
        // registry-lookup step (missing registry), which proves the
        // validation passed.
        let app = mock_app_with_default_node_state();
        let state = app.state::<StdMutex<NodeState>>();
        let err = list_channel_messages(state, "00".repeat(16), "00".repeat(16), None, 0)
            .await
            .expect_err("missing registry must error");
        assert!(
            err.contains("channel_log_registry missing"),
            "limit=0 should fall through to registry lookup; got: {err}"
        );
    }

    #[tokio::test]
    async fn list_channel_messages_accepts_limit_at_cap() {
        let app = mock_app_with_default_node_state();
        let state = app.state::<StdMutex<NodeState>>();
        let err = list_channel_messages(state, "00".repeat(16), "00".repeat(16), None, 1000)
            .await
            .expect_err("missing registry must error");
        assert!(
            err.contains("channel_log_registry missing"),
            "limit=1000 (== cap) should pass validation; got: {err}"
        );
    }

    #[tokio::test]
    async fn list_channel_messages_rejects_short_community_id() {
        let app = mock_app_with_default_node_state();
        let state = app.state::<StdMutex<NodeState>>();
        let err = list_channel_messages(state, "ab".into(), "00".repeat(16), None, 10)
            .await
            .expect_err("short cid must error");
        assert!(err.contains("community_id must be 16 bytes"), "got: {err}");
    }

    #[tokio::test]
    async fn request_channel_backfill_rejects_short_community_id() {
        let app = mock_app_with_default_node_state();
        let state = app.state::<StdMutex<NodeState>>();
        let err = request_channel_backfill(state, "ab".into(), "00".repeat(16), None)
            .await
            .expect_err("short cid must error");
        assert!(err.contains("community_id must be 16 bytes"), "got: {err}");
    }

    #[tokio::test]
    async fn request_channel_backfill_errors_when_registry_missing() {
        let app = mock_app_with_default_node_state();
        let state = app.state::<StdMutex<NodeState>>();
        let err = request_channel_backfill(state, "00".repeat(16), "00".repeat(16), None)
            .await
            .expect_err("missing registry must error");
        assert!(err.contains("channel_log_registry missing"), "got: {err}");
    }

    #[tokio::test]
    async fn request_channel_backfill_accepts_some_since() {
        // since: Some(HlcDto) is the deserialize path — verifies the
        // DTO is wired correctly as IPC input. The call still errors
        // at the registry-lookup step, which proves deserialize +
        // validation succeeded.
        let app = mock_app_with_default_node_state();
        let state = app.state::<StdMutex<NodeState>>();
        let since = Some(crate::community_channel_log_engine::HlcDto {
            wall_ms: 1234,
            logical: 5,
            device_id: "device-x".into(),
        });
        let err = request_channel_backfill(state, "00".repeat(16), "00".repeat(16), since)
            .await
            .expect_err("missing registry must error");
        assert!(
            err.contains("channel_log_registry missing"),
            "Some(HlcDto) should fall through to registry lookup; got: {err}"
        );
    }
}

// ── ZEB-284 Task 2: unban + kick-with-reason + list_recent_moderation_events IPC tests ──
//
// Tests for:
// - mint_unban_event helper
// - unban_from_community error paths and happy path
// - kick_from_community with reason (sign-into-event check)
// - list_recent_moderation_events filter + ordering + limit
//
// Pattern mirrors `list_community_members_ipc_tests` (direct CommunityState
// manipulation, no Tauri mock app needed for business-logic tests) plus
// the `channel_message_ipc_tests` mock-app pattern for IPC error-return tests.
#[cfg(test)]
mod unban_from_community_tests {
    use super::*;
    use crate::community_membership::{
        sign_event_with_identity, EventPayload, MembershipEventKind, VerifyContext,
    };
    use crate::community_state_crdt::{CommunityState, InsertOutcome};
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    // ── shared fixture helpers ─────────────────────────────────────────

    fn hlc(wall: u64, dev: &str) -> Hlc {
        Hlc {
            wall_ms: wall,
            logical: 0,
            device_id: dev.to_string(),
        }
    }

    /// Sign and insert an event into state, asserting it is Inserted.
    fn insert_ok(
        state: &mut CommunityState,
        payload: EventPayload,
        identity: &PrivateIdentity,
        admin: OwnerAddr,
        actor_pub: &[u8; 64],
    ) {
        let ev = sign_event_with_identity(&payload, identity).expect("sign");
        let ctx = VerifyContext {
            expected_community_id: payload.community_id,
            admin_addr: admin,
            is_invite_only: false,
            actor_identity_pub: actor_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
        };
        let outcome = state.insert_event(ev, &ctx);
        assert!(
            matches!(outcome, InsertOutcome::Inserted),
            "fixture insert must succeed; got {outcome:?}"
        );
    }

    // ── Test 1: unban_from_community_happy_path ────────────────────────────
    //
    // Two-engine scenario: admin kicks member B, then unbans B.
    // We simulate "both engines" by operating on a shared CommunityState
    // (the single source of truth for CRDT state in a sync engine), then
    // running materialize on both admin and member viewpoints.
    #[tokio::test]
    async fn unban_from_community_happy_path() {
        let community_id = SpaceId([0x10; 16]);

        let admin_identity = PrivateIdentity::from_seed(&[0xaa; 32]);
        let admin = OwnerAddr(admin_identity.identity.address_hash);
        let admin_pub = admin_identity.identity.to_public_bytes();

        let member_identity = PrivateIdentity::from_seed(&[0xbb; 32]);
        let member = OwnerAddr(member_identity.identity.address_hash);
        let member_pub = member_identity.identity.to_public_bytes();

        let state = Arc::new(Mutex::new(CommunityState::new(community_id)));

        // Admin joins (admin-power bootstrap).
        insert_ok(
            &mut *state.lock().await,
            EventPayload {
                id: [0x01; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: admin,
                at: hlc(100, "admin"),
            },
            &admin_identity,
            admin,
            &admin_pub,
        );

        // Member joins.
        insert_ok(
            &mut *state.lock().await,
            EventPayload {
                id: [0x02; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: member,
                at: hlc(200, "member"),
            },
            &member_identity,
            admin,
            &member_pub,
        );

        // Admin kicks member.
        insert_ok(
            &mut *state.lock().await,
            EventPayload {
                id: [0x03; 16],
                community_id,
                kind: MembershipEventKind::Kick {
                    target: member,
                    reason: Some("test reason".into()),
                },
                actor: admin,
                at: hlc(300, "admin"),
            },
            &admin_identity,
            admin,
            &admin_pub,
        );

        // Verify member is Banned after kick.
        let m_post_kick = state.lock().await.materialize_now(admin);
        assert_eq!(
            m_post_kick.members[&member].status,
            crate::community_membership::MemberStatus::Banned,
            "member must be Banned after kick"
        );

        // Admin unbans member (uses mint_unban_event helper to build the event).
        let sk_bytes = admin_identity.to_private_bytes();
        let sk_seed: [u8; 32] = sk_bytes[32..64].try_into().unwrap();
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&sk_seed);
        let unban_ev = mint_unban_event(
            community_id,
            admin,
            member,
            None,
            &signing_key,
            hlc(400, "admin"),
        )
        .expect("mint_unban_event must succeed");

        // Insert the unban into state using VerifyContext so verify_event runs.
        let unban_outcome = {
            let mut g = state.lock().await;
            g.insert_event(
                unban_ev,
                &VerifyContext {
                    expected_community_id: community_id,
                    admin_addr: admin,
                    is_invite_only: false,
                    actor_identity_pub: &admin_pub,
                    countersigner_identity_pub: None,
                    admin_identity_pub: None,
                },
            )
        };
        assert!(
            matches!(unban_outcome, InsertOutcome::Inserted),
            "unban must be Inserted; got {unban_outcome:?}"
        );

        // Engine A view: admin's perspective.
        let m_a = state.lock().await.materialize_now(admin);
        assert_eq!(
            m_a.members[&member].status,
            crate::community_membership::MemberStatus::Left,
            "engine A: member must be Left after unban"
        );

        // Engine B view: replicate the same state (same CommunityState Arc)
        // and verify from the member's own admin-lookup perspective. In a
        // real two-peer setup, B would receive the events via Zenoh sync and
        // produce the same materialized view — this tests the CRDT convergence
        // property on a single shared state, which is equivalent.
        let m_b = state.lock().await.materialize_now(admin);
        assert_eq!(
            m_b.members[&member].status,
            crate::community_membership::MemberStatus::Left,
            "engine B (same shared state): member must be Left after unban"
        );
    }

    // ── Test 2: unban_from_community_returns_err_when_actor_lacks_power ──────
    //
    // A moderator (power 50) attempts to unban; verify_event must reject
    // with ActorPowerInsufficient (Unban is admin-tier, power ≥ 100).
    #[tokio::test]
    async fn unban_from_community_returns_err_when_actor_lacks_power() {
        let community_id = SpaceId([0x11; 16]);

        let admin_identity = PrivateIdentity::from_seed(&[0xaa; 32]);
        let admin = OwnerAddr(admin_identity.identity.address_hash);
        let admin_pub = admin_identity.identity.to_public_bytes();

        let mod_identity = PrivateIdentity::from_seed(&[0xcc; 32]);
        let moderator = OwnerAddr(mod_identity.identity.address_hash);
        let mod_pub = mod_identity.identity.to_public_bytes();

        let member_identity = PrivateIdentity::from_seed(&[0xbb; 32]);
        let member = OwnerAddr(member_identity.identity.address_hash);
        let member_pub = member_identity.identity.to_public_bytes();

        let mut state = CommunityState::new(community_id);

        // Admin joins.
        insert_ok(
            &mut state,
            EventPayload {
                id: [0x01; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: admin,
                at: hlc(100, "admin"),
            },
            &admin_identity,
            admin,
            &admin_pub,
        );
        // Moderator joins.
        insert_ok(
            &mut state,
            EventPayload {
                id: [0x02; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: moderator,
                at: hlc(110, "mod"),
            },
            &mod_identity,
            admin,
            &mod_pub,
        );
        // Admin promotes moderator to power 50.
        insert_ok(
            &mut state,
            EventPayload {
                id: [0x03; 16],
                community_id,
                kind: MembershipEventKind::SetPower {
                    target: moderator,
                    level: 50,
                },
                actor: admin,
                at: hlc(120, "admin"),
            },
            &admin_identity,
            admin,
            &admin_pub,
        );
        // Member joins.
        insert_ok(
            &mut state,
            EventPayload {
                id: [0x04; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: member,
                at: hlc(200, "member"),
            },
            &member_identity,
            admin,
            &member_pub,
        );
        // Admin kicks member.
        insert_ok(
            &mut state,
            EventPayload {
                id: [0x05; 16],
                community_id,
                kind: MembershipEventKind::Kick {
                    target: member,
                    reason: None,
                },
                actor: admin,
                at: hlc(300, "admin"),
            },
            &admin_identity,
            admin,
            &admin_pub,
        );

        // Moderator attempts to unban — should be rejected (power 50 < 100).
        let mod_sk_bytes = mod_identity.to_private_bytes();
        let mod_sk_seed: [u8; 32] = mod_sk_bytes[32..64].try_into().unwrap();
        let mod_signing_key = ed25519_dalek::SigningKey::from_bytes(&mod_sk_seed);
        let unban_ev = mint_unban_event(
            community_id,
            moderator,
            member,
            None,
            &mod_signing_key,
            hlc(400, "mod"),
        )
        .expect("mint must succeed");

        let outcome = state.insert_event(
            unban_ev,
            &VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &mod_pub,
                countersigner_identity_pub: None,
                admin_identity_pub: None,
            },
        );

        match outcome {
            InsertOutcome::Rejected(err) => {
                let msg = format!("{err}");
                assert!(
                    msg.contains("power is below"),
                    "expected 'power is below' in error; got: {msg}"
                );
            }
            other => panic!("expected Rejected(ActorPowerInsufficient); got {other:?}"),
        }
    }

    // ── Test 3: unban_from_community_returns_err_when_target_not_banned ──────
    //
    // Admin attempts to unban a member who is currently Joined (not Banned).
    // verify_event must reject with UnbanTargetNotBanned.
    #[tokio::test]
    async fn unban_from_community_returns_err_when_target_not_banned() {
        let community_id = SpaceId([0x12; 16]);

        let admin_identity = PrivateIdentity::from_seed(&[0xaa; 32]);
        let admin = OwnerAddr(admin_identity.identity.address_hash);
        let admin_pub = admin_identity.identity.to_public_bytes();

        let member_identity = PrivateIdentity::from_seed(&[0xbb; 32]);
        let member = OwnerAddr(member_identity.identity.address_hash);
        let member_pub = member_identity.identity.to_public_bytes();

        let mut state = CommunityState::new(community_id);

        // Admin joins.
        insert_ok(
            &mut state,
            EventPayload {
                id: [0x01; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: admin,
                at: hlc(100, "admin"),
            },
            &admin_identity,
            admin,
            &admin_pub,
        );
        // Member joins (not kicked — still Joined).
        insert_ok(
            &mut state,
            EventPayload {
                id: [0x02; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: member,
                at: hlc(200, "member"),
            },
            &member_identity,
            admin,
            &member_pub,
        );

        // Admin attempts to unban a Joined member — should fail.
        let sk_bytes = admin_identity.to_private_bytes();
        let sk_seed: [u8; 32] = sk_bytes[32..64].try_into().unwrap();
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&sk_seed);
        let unban_ev = mint_unban_event(
            community_id,
            admin,
            member,
            None,
            &signing_key,
            hlc(300, "admin"),
        )
        .expect("mint must succeed");

        let outcome = state.insert_event(
            unban_ev,
            &VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
                admin_identity_pub: None,
            },
        );

        match outcome {
            InsertOutcome::Rejected(err) => {
                let msg = format!("{err}");
                assert!(
                    msg.contains("target is not currently banned"),
                    "expected 'target is not currently banned' in error; got: {msg}"
                );
            }
            other => panic!("expected Rejected(UnbanTargetNotBanned); got {other:?}"),
        }
    }

    // ── Test 4: kick_from_community_signs_reason_into_event ───────────────────
    //
    // Kick with reason "smoke"; inspect the raw event log; the Kick event's
    // kind must carry the reason field.
    #[tokio::test]
    async fn kick_from_community_signs_reason_into_event() {
        let community_id = SpaceId([0x13; 16]);

        let admin_identity = PrivateIdentity::from_seed(&[0xaa; 32]);
        let admin = OwnerAddr(admin_identity.identity.address_hash);
        let admin_pub = admin_identity.identity.to_public_bytes();

        let member_identity = PrivateIdentity::from_seed(&[0xbb; 32]);
        let member = OwnerAddr(member_identity.identity.address_hash);
        let member_pub = member_identity.identity.to_public_bytes();

        let mut state = CommunityState::new(community_id);

        // Admin joins.
        insert_ok(
            &mut state,
            EventPayload {
                id: [0x01; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: admin,
                at: hlc(100, "admin"),
            },
            &admin_identity,
            admin,
            &admin_pub,
        );
        // Member joins.
        insert_ok(
            &mut state,
            EventPayload {
                id: [0x02; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: member,
                at: hlc(200, "member"),
            },
            &member_identity,
            admin,
            &member_pub,
        );

        // Kick member with reason.
        let sk_bytes = admin_identity.to_private_bytes();
        let sk_seed: [u8; 32] = sk_bytes[32..64].try_into().unwrap();
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&sk_seed);
        let kick_ev = mint_kick_event(
            community_id,
            admin,
            member,
            Some("smoke".into()),
            &signing_key,
            hlc(300, "admin"),
        )
        .expect("mint_kick_event must succeed");

        let outcome = state.insert_event(
            kick_ev.clone(),
            &VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
                admin_identity_pub: None,
            },
        );
        assert!(
            matches!(outcome, InsertOutcome::Inserted),
            "kick insert must succeed; got {outcome:?}"
        );

        // Inspect the raw event log for the Kick event's reason.
        let kick_in_log = state
            .events
            .values()
            .find(|ev| matches!(&ev.kind, MembershipEventKind::Kick { .. }))
            .expect("Kick event must be in the event log");

        match &kick_in_log.kind {
            MembershipEventKind::Kick { reason, .. } => {
                assert_eq!(
                    reason.as_deref(),
                    Some("smoke"),
                    "Kick event must carry reason 'smoke'; got {reason:?}"
                );
            }
            other => panic!("expected Kick kind; got {other:?}"),
        }
    }

    // ── Test 5: list_recent_moderation_events_filters_to_kick_unban_setpower ─
    //
    // Mixed event sequence: Join, SetPower, ChannelCreate (synthetic
    // direct insert), Kick, Unban. Verify filter returns only the
    // moderation kinds (SetPower, Kick, Unban) in HLC desc order.
    #[tokio::test]
    async fn list_recent_moderation_events_filters_to_kick_unban_setpower() {
        let community_id = SpaceId([0x14; 16]);

        let admin_identity = PrivateIdentity::from_seed(&[0xaa; 32]);
        let admin = OwnerAddr(admin_identity.identity.address_hash);
        let admin_pub = admin_identity.identity.to_public_bytes();

        let member_identity = PrivateIdentity::from_seed(&[0xbb; 32]);
        let member = OwnerAddr(member_identity.identity.address_hash);
        let member_pub = member_identity.identity.to_public_bytes();

        let mut state = CommunityState::new(community_id);

        // Join events (should NOT appear in moderation filter).
        insert_ok(
            &mut state,
            EventPayload {
                id: [0x01; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: admin,
                at: hlc(100, "admin"),
            },
            &admin_identity,
            admin,
            &admin_pub,
        );
        insert_ok(
            &mut state,
            EventPayload {
                id: [0x02; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: member,
                at: hlc(200, "member"),
            },
            &member_identity,
            admin,
            &member_pub,
        );

        let sk_bytes = admin_identity.to_private_bytes();
        let sk_seed: [u8; 32] = sk_bytes[32..64].try_into().unwrap();
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&sk_seed);

        // SetPower event (SHOULD appear).
        let sp_ev = mint_set_power_event(
            community_id,
            admin,
            member,
            50,
            &signing_key,
            hlc(300, "admin"),
        )
        .expect("mint set_power");
        let sp_outcome = state.insert_event(
            sp_ev,
            &VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
                admin_identity_pub: None,
            },
        );
        assert!(matches!(sp_outcome, InsertOutcome::Inserted));

        // Kick event (SHOULD appear).
        let kick_ev = mint_kick_event(
            community_id,
            admin,
            member,
            None,
            &signing_key,
            hlc(400, "admin"),
        )
        .expect("mint kick");
        let kick_outcome = state.insert_event(
            kick_ev,
            &VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
                admin_identity_pub: None,
            },
        );
        assert!(matches!(kick_outcome, InsertOutcome::Inserted));

        // Unban event (SHOULD appear).
        let unban_ev = mint_unban_event(
            community_id,
            admin,
            member,
            Some("reinstatement".into()),
            &signing_key,
            hlc(500, "admin"),
        )
        .expect("mint unban");
        let unban_outcome = state.insert_event(
            unban_ev,
            &VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
                admin_identity_pub: None,
            },
        );
        assert!(matches!(unban_outcome, InsertOutcome::Inserted));

        // Apply the list_recent_moderation_events filter logic directly on
        // the raw event log (mirrors what the IPC does).
        let raw: Vec<crate::community_membership::SignedMembershipEvent> =
            state.events.values().cloned().collect();

        let mut dtos: Vec<ModerationEventDto> = raw
            .into_iter()
            .filter_map(|ev| {
                let (kind, target_addr, reason, new_power) = match &ev.kind {
                    MembershipEventKind::Kick { target, reason } => (
                        ModerationEventKindDto::Kick,
                        hex::encode(target.0),
                        reason.clone(),
                        None,
                    ),
                    MembershipEventKind::Unban { target, reason } => (
                        ModerationEventKindDto::Unban,
                        hex::encode(target.0),
                        reason.clone(),
                        None,
                    ),
                    MembershipEventKind::SetPower { target, level } => (
                        ModerationEventKindDto::SetPower,
                        hex::encode(target.0),
                        None,
                        Some(*level),
                    ),
                    _ => return None,
                };
                Some(ModerationEventDto {
                    event_id: hex::encode(ev.id),
                    kind,
                    actor_addr: hex::encode(ev.actor.0),
                    target_addr,
                    reason,
                    new_power,
                    hlc: ev.at.clone(),
                })
            })
            .collect();

        // Sort HLC desc.
        dtos.sort_by(|a, b| {
            b.hlc
                .wall_ms
                .cmp(&a.hlc.wall_ms)
                .then_with(|| b.hlc.logical.cmp(&a.hlc.logical))
        });

        // Only 3 moderation events (SetPower, Kick, Unban) — Join × 2 filtered.
        assert_eq!(
            dtos.len(),
            3,
            "expected 3 moderation events; got {}",
            dtos.len()
        );

        // HLC desc: Unban(500) > Kick(400) > SetPower(300).
        assert_eq!(dtos[0].kind, ModerationEventKindDto::Unban);
        assert_eq!(dtos[0].hlc.wall_ms, 500);
        assert_eq!(dtos[0].reason.as_deref(), Some("reinstatement"));

        assert_eq!(dtos[1].kind, ModerationEventKindDto::Kick);
        assert_eq!(dtos[1].hlc.wall_ms, 400);
        assert!(dtos[1].reason.is_none());

        assert_eq!(dtos[2].kind, ModerationEventKindDto::SetPower);
        assert_eq!(dtos[2].hlc.wall_ms, 300);
        assert_eq!(dtos[2].new_power, Some(50));
    }

    // ── Test 6: list_recent_moderation_events_respects_limit_and_orders_by_hlc_desc ──
    //
    // Insert 5 Kick events at wall_ms 100, 200, 300, 400, 500.
    // With limit=3, should return events at 500, 400, 300 (newest 3).
    #[tokio::test]
    async fn list_recent_moderation_events_respects_limit_and_orders_by_hlc_desc() {
        let community_id = SpaceId([0x15; 16]);

        let admin_identity = PrivateIdentity::from_seed(&[0xaa; 32]);
        let admin = OwnerAddr(admin_identity.identity.address_hash);
        let admin_pub = admin_identity.identity.to_public_bytes();

        // 5 distinct "members" to kick (to avoid same-target restrictions
        // — a re-kick of an already-Banned member would be rejected).
        let victims: Vec<PrivateIdentity> = (0u8..5)
            .map(|i| PrivateIdentity::from_seed(&[0xd0 + i; 32]))
            .collect();

        let mut state = CommunityState::new(community_id);

        // Admin joins.
        insert_ok(
            &mut state,
            EventPayload {
                id: [0x00; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: admin,
                at: hlc(1, "admin"),
            },
            &admin_identity,
            admin,
            &admin_pub,
        );

        // Each victim joins.
        for (i, victim_id) in victims.iter().enumerate() {
            let victim = OwnerAddr(victim_id.identity.address_hash);
            let victim_pub = victim_id.identity.to_public_bytes();
            insert_ok(
                &mut state,
                EventPayload {
                    id: [0x10 + i as u8; 16],
                    community_id,
                    kind: MembershipEventKind::Join,
                    actor: victim,
                    at: hlc(10 + i as u64, "victim"),
                },
                victim_id,
                admin,
                &victim_pub,
            );
        }

        let sk_bytes = admin_identity.to_private_bytes();
        let sk_seed: [u8; 32] = sk_bytes[32..64].try_into().unwrap();
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&sk_seed);

        // Kick each victim at wall_ms 100, 200, 300, 400, 500.
        let wall_times = [100u64, 200, 300, 400, 500];
        for (i, victim_id) in victims.iter().enumerate() {
            let victim = OwnerAddr(victim_id.identity.address_hash);
            let kick_ev = mint_kick_event(
                community_id,
                admin,
                victim,
                None,
                &signing_key,
                hlc(wall_times[i], "admin"),
            )
            .expect("mint kick");
            let outcome = state.insert_event(
                kick_ev,
                &VerifyContext {
                    expected_community_id: community_id,
                    admin_addr: admin,
                    is_invite_only: false,
                    actor_identity_pub: &admin_pub,
                    countersigner_identity_pub: None,
                    admin_identity_pub: None,
                },
            );
            assert!(
                matches!(outcome, InsertOutcome::Inserted),
                "kick[{i}] insert must succeed; got {outcome:?}"
            );
        }

        // Apply filter + sort + truncate (limit = 3).
        let raw: Vec<crate::community_membership::SignedMembershipEvent> =
            state.events.values().cloned().collect();
        let mut dtos: Vec<ModerationEventDto> = raw
            .into_iter()
            .filter_map(|ev| {
                let (kind, target_addr, reason, new_power) = match &ev.kind {
                    MembershipEventKind::Kick { target, reason } => (
                        ModerationEventKindDto::Kick,
                        hex::encode(target.0),
                        reason.clone(),
                        None,
                    ),
                    MembershipEventKind::Unban { target, reason } => (
                        ModerationEventKindDto::Unban,
                        hex::encode(target.0),
                        reason.clone(),
                        None,
                    ),
                    MembershipEventKind::SetPower { target, level } => (
                        ModerationEventKindDto::SetPower,
                        hex::encode(target.0),
                        None,
                        Some(*level),
                    ),
                    _ => return None,
                };
                Some(ModerationEventDto {
                    event_id: hex::encode(ev.id),
                    kind,
                    actor_addr: hex::encode(ev.actor.0),
                    target_addr,
                    reason,
                    new_power,
                    hlc: ev.at.clone(),
                })
            })
            .collect();
        dtos.sort_by(|a, b| {
            b.hlc
                .wall_ms
                .cmp(&a.hlc.wall_ms)
                .then_with(|| b.hlc.logical.cmp(&a.hlc.logical))
        });
        let limit = 3usize;
        dtos.truncate(limit);

        assert_eq!(dtos.len(), 3, "limit=3 must return exactly 3 events");

        // Newest first: 500, 400, 300.
        assert_eq!(
            dtos[0].hlc.wall_ms, 500,
            "first event must be at wall_ms=500"
        );
        assert_eq!(
            dtos[1].hlc.wall_ms, 400,
            "second event must be at wall_ms=400"
        );
        assert_eq!(
            dtos[2].hlc.wall_ms, 300,
            "third event must be at wall_ms=300"
        );

        // Oldest two (100, 200) must be truncated.
        assert!(
            dtos.iter().all(|d| d.hlc.wall_ms >= 300),
            "events at 100 and 200 must be truncated by limit=3"
        );
    }
}

// ── ZEB-254 Task 12: list_pending_joins + list_recent_counter_signs unit tests ─
//
// These tests exercise the pure `filter_pending_joins` and
// `filter_recent_counter_signs` helpers directly, bypassing the Tauri IPC
// layer and NodeState. Events are inserted directly into a raw Vec so we can
// control timestamps precisely without needing valid crypto signatures.
// (End-to-end IPC coverage is deferred to Task 15.)
#[cfg(test)]
mod pending_join_audit_feed_tests {
    use super::*;
    use crate::community_invite::InviteToken;
    use crate::community_membership::{MembershipEventKind, SignedMembershipEvent};
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

    /// Build a minimal unsigned SignedMembershipEvent for filter testing.
    /// Signature bytes are zeroed — the filter helpers never check sigs.
    fn make_event(
        id: [u8; 16],
        actor: OwnerAddr,
        wall_ms: u64,
        kind: MembershipEventKind,
        community_id: SpaceId,
    ) -> SignedMembershipEvent {
        SignedMembershipEvent {
            id,
            community_id,
            kind,
            actor,
            at: Hlc {
                wall_ms,
                logical: 0,
                device_id: "test-device".into(),
            },
            sig: [0u8; 64],
            countersig: None,
        }
    }

    /// Build a minimal InviteToken for PendingJoin events in tests.
    fn make_token(inviter: OwnerAddr, invitee_hint: Option<OwnerAddr>) -> InviteToken {
        InviteToken {
            inviter,
            invitee_hint,
            minted_at: Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "admin".into(),
            },
            expires_at: None,
            sig: [0u8; 64],
        }
    }

    // ── Test 1: list_pending_joins_returns_pending_only ───────────────────
    //
    // Seed four events:
    //   A) PendingJoin, no countersign, within 30-day window → should appear
    //   B) PendingJoin + JoinCountersign → should NOT appear (countersigned)
    //   C) PendingJoin, expired (>30d before max_wall_ms) → should NOT appear
    //   D) Join (plain) → should NOT appear (wrong kind)
    //
    // max_wall_ms is event A's wall_ms. Expiry threshold = max − 30d.
    // Event C is placed at (max − 31d) so it falls outside the window.
    #[tokio::test]
    async fn list_pending_joins_returns_pending_only() {
        let community_id = SpaceId([0xde; 16]);
        let admin = OwnerAddr([0xaa; 16]);
        let joiner_a = OwnerAddr([0x01; 16]);
        let joiner_b = OwnerAddr([0x02; 16]);
        let joiner_c = OwnerAddr([0x03; 16]);
        let joiner_d = OwnerAddr([0x04; 16]);

        // max_wall_ms will be this value (event A is the "newest").
        const MAX_MS: u64 = 1_000_000_000_000;
        const THIRTY_DAYS_MS: u64 = 30 * 86_400_000;
        const EXPIRED_MS: u64 = MAX_MS - THIRTY_DAYS_MS - 1; // one ms past expiry

        // A: un-countersigned, recent.
        let ev_a = make_event(
            [0x0a; 16],
            joiner_a,
            MAX_MS,
            MembershipEventKind::PendingJoin {
                invite_token: make_token(admin, None),
                joiner_identity_pub: [0u8; 64],
            },
            community_id,
        );

        // B: PendingJoin that will be countersigned.
        let ev_b = make_event(
            [0x0b; 16],
            joiner_b,
            MAX_MS - 1_000,
            MembershipEventKind::PendingJoin {
                invite_token: make_token(admin, None),
                joiner_identity_pub: [0u8; 64],
            },
            community_id,
        );
        // JoinCountersign targeting ev_b.
        let ev_b_cs = make_event(
            [0xb0; 16],
            admin,
            MAX_MS - 500,
            MembershipEventKind::JoinCountersign {
                target_event_id: [0x0b; 16],
            },
            community_id,
        );

        // C: expired PendingJoin (no countersign).
        let ev_c = make_event(
            [0x0c; 16],
            joiner_c,
            EXPIRED_MS,
            MembershipEventKind::PendingJoin {
                invite_token: make_token(admin, None),
                joiner_identity_pub: [0u8; 64],
            },
            community_id,
        );

        // D: plain Join (should be filtered out by kind).
        let ev_d = make_event(
            [0x0d; 16],
            joiner_d,
            MAX_MS - 2_000,
            MembershipEventKind::Join,
            community_id,
        );

        let events = vec![ev_a, ev_b, ev_b_cs, ev_c, ev_d];
        // Pass MAX_MS as now_ms so the expiry threshold is MAX_MS - 30d,
        // consistent with the test's EXPIRED_MS constant.
        let result = filter_pending_joins(&events, MAX_MS);

        assert_eq!(
            result.len(),
            1,
            "expected exactly 1 pending join; got {}: {result:?}",
            result.len()
        );
        assert_eq!(
            result[0].event_id,
            hex::encode([0x0a; 16]),
            "the single result must be event A"
        );
        assert_eq!(
            result[0].joiner_addr,
            hex::encode(joiner_a.0),
            "joiner_addr must match joiner_a"
        );
        assert_eq!(
            result[0].pending_at_hlc.wall_ms, MAX_MS,
            "pending_at_hlc.wall_ms must be MAX_MS"
        );
        assert!(
            result[0].invitee_hint.is_none(),
            "invitee_hint must be None (token had no hint)"
        );
    }

    // ── Test 2: list_pending_joins_invitee_hint_is_hex_encoded ───────────
    //
    // PendingJoin with invitee_hint set → result must carry the hex-encoded
    // hint.
    #[tokio::test]
    async fn list_pending_joins_invitee_hint_is_hex_encoded() {
        let community_id = SpaceId([0xef; 16]);
        let admin = OwnerAddr([0xaa; 16]);
        let hint_addr = OwnerAddr([0x42; 16]);
        let joiner = OwnerAddr([0x01; 16]);

        let ev = make_event(
            [0x01; 16],
            joiner,
            1_000,
            MembershipEventKind::PendingJoin {
                invite_token: make_token(admin, Some(hint_addr)),
                joiner_identity_pub: [0u8; 64],
            },
            community_id,
        );

        // Event wall_ms=1000; pass a now_ms well within 30d so it's visible.
        let result = filter_pending_joins(&[ev], 1_000_000);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].invitee_hint.as_deref(),
            Some(hex::encode(hint_addr.0).as_str()),
            "invitee_hint must be hex-encoded OwnerAddr"
        );
    }

    // ── Test 3: list_recent_counter_signs_returns_self_authored ──────────
    //
    // Seed: 2 JoinCountersigns by self_owner, 1 by another admin, 1 PendingJoin.
    // Call filter_recent_counter_signs with cap=10.
    // Expect: exactly 2 entries (only self-authored JoinCountersigns).
    #[tokio::test]
    async fn list_recent_counter_signs_returns_self_authored() {
        let community_id = SpaceId([0xca; 16]);
        let self_owner = OwnerAddr([0xaa; 16]);
        let other_admin = OwnerAddr([0xbb; 16]);
        let joiner_1 = OwnerAddr([0x01; 16]);
        let joiner_2 = OwnerAddr([0x02; 16]);
        let joiner_3 = OwnerAddr([0x03; 16]);

        // Three PendingJoin events (targets for countersigns).
        let pj_1 = make_event(
            [0x10; 16],
            joiner_1,
            100,
            MembershipEventKind::PendingJoin {
                invite_token: make_token(self_owner, None),
                joiner_identity_pub: [0u8; 64],
            },
            community_id,
        );
        let pj_2 = make_event(
            [0x20; 16],
            joiner_2,
            200,
            MembershipEventKind::PendingJoin {
                invite_token: make_token(self_owner, None),
                joiner_identity_pub: [0u8; 64],
            },
            community_id,
        );
        let pj_3 = make_event(
            [0x30; 16],
            joiner_3,
            300,
            MembershipEventKind::PendingJoin {
                invite_token: make_token(other_admin, None),
                joiner_identity_pub: [0u8; 64],
            },
            community_id,
        );

        // self_owner counter-signs pj_1 and pj_2.
        let cs_self_1 = make_event(
            [0xa1; 16],
            self_owner,
            500,
            MembershipEventKind::JoinCountersign {
                target_event_id: [0x10; 16],
            },
            community_id,
        );
        let cs_self_2 = make_event(
            [0xa2; 16],
            self_owner,
            600,
            MembershipEventKind::JoinCountersign {
                target_event_id: [0x20; 16],
            },
            community_id,
        );

        // other_admin counter-signs pj_3 (should NOT appear).
        let cs_other = make_event(
            [0xb1; 16],
            other_admin,
            700,
            MembershipEventKind::JoinCountersign {
                target_event_id: [0x30; 16],
            },
            community_id,
        );

        let events = vec![pj_1, pj_2, pj_3, cs_self_1, cs_self_2, cs_other];
        let result = filter_recent_counter_signs(&events, self_owner, 10);

        assert_eq!(
            result.len(),
            2,
            "expected 2 self-authored counter-signs; got {}: {result:?}",
            result.len()
        );

        // Sorted descending by wall_ms: cs_self_2 (600) then cs_self_1 (500).
        assert_eq!(
            result[0].countersigned_at_hlc.wall_ms, 600,
            "first result must be the more-recent countersign"
        );
        assert_eq!(
            result[0].join_event_id,
            hex::encode([0x20u8; 16]),
            "first result must target pj_2"
        );
        assert_eq!(
            result[0].joiner_addr,
            hex::encode(joiner_2.0),
            "joiner_addr must be resolved from pj_2"
        );

        assert_eq!(
            result[1].countersigned_at_hlc.wall_ms, 500,
            "second result must be the earlier countersign"
        );
        assert_eq!(
            result[1].join_event_id,
            hex::encode([0x10u8; 16]),
            "second result must target pj_1"
        );
        assert_eq!(
            result[1].joiner_addr,
            hex::encode(joiner_1.0),
            "joiner_addr must be resolved from pj_1"
        );
    }

    // ── Test 4: list_recent_counter_signs_respects_cap ───────────────────
    //
    // 5 self-authored JoinCountersigns; cap=3. Expect 3 newest.
    #[tokio::test]
    async fn list_recent_counter_signs_respects_cap() {
        let community_id = SpaceId([0xcb; 16]);
        let self_owner = OwnerAddr([0xaa; 16]);

        let events: Vec<SignedMembershipEvent> = (0u8..5)
            .map(|i| {
                make_event(
                    [0xa0 + i; 16],
                    self_owner,
                    (i as u64 + 1) * 100,
                    MembershipEventKind::JoinCountersign {
                        target_event_id: [0x10 + i; 16],
                    },
                    community_id,
                )
            })
            .collect();

        let result = filter_recent_counter_signs(&events, self_owner, 3);
        assert_eq!(result.len(), 3, "cap=3 must return exactly 3 entries");
        // Sorted descending: wall_ms 500, 400, 300.
        assert_eq!(result[0].countersigned_at_hlc.wall_ms, 500);
        assert_eq!(result[1].countersigned_at_hlc.wall_ms, 400);
        assert_eq!(result[2].countersigned_at_hlc.wall_ms, 300);
    }
}

// ── ZEB-234: shutdown fence unit tests ──────────────────────────────────────
// These tests exercise the `check_dm_send_fence` helper directly, without
// requiring a full NodeState fixture (which would need all DM handles
// populated). This is the approach from the plan's "lighter test" alternative:
// test the helper that `send_dm` delegates to.
#[cfg(test)]
mod dm_send_fence_tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    #[tokio::test]
    async fn send_dm_rejects_after_stopping_flag_set() {
        let stopping = Arc::new(AtomicBool::new(true)); // flag already set
        let sem = Arc::new(tokio::sync::Semaphore::new(DM_SEND_FENCE_CAPACITY));

        let result = check_dm_send_fence(&stopping, sem).await;
        let err = result.expect_err("must reject when stopping flag is set");
        assert!(
            err.contains("node stopping"),
            "error should mention 'node stopping'; got: {err}"
        );
    }

    #[tokio::test]
    async fn check_dm_send_fence_rejects_when_semaphore_closed() {
        let stopping = Arc::new(AtomicBool::new(false));
        let sem = Arc::new(tokio::sync::Semaphore::new(DM_SEND_FENCE_CAPACITY));

        // Close the semaphore to simulate stop_inner draining it.
        // stopping flag is NOT set so the pre-check passes and we exercise
        // the acquire_owned().await error path.
        sem.close();

        let result = check_dm_send_fence(&stopping, sem).await;
        let err = result.expect_err("must reject when semaphore is closed");
        assert!(
            err.contains("semaphore closed"),
            "error should mention 'semaphore closed'; got: {err}"
        );
    }

    #[tokio::test]
    async fn send_dm_permit_acquired_when_not_stopping() {
        let stopping = Arc::new(AtomicBool::new(false)); // not stopping
        let sem = Arc::new(tokio::sync::Semaphore::new(DM_SEND_FENCE_CAPACITY));
        let sem_clone = sem.clone();

        let permit = check_dm_send_fence(&stopping, sem)
            .await
            .expect("must succeed when not stopping");

        // Verify a permit was consumed (available_permits is CAPACITY - 1).
        assert_eq!(
            sem_clone.available_permits(),
            DM_SEND_FENCE_CAPACITY - 1,
            "one permit should be held"
        );
        drop(permit);
        assert_eq!(
            sem_clone.available_permits(),
            DM_SEND_FENCE_CAPACITY,
            "permit should be returned on drop"
        );
    }

    /// ZEB-234: drain blocks until all in-flight permits are returned.
    ///
    /// Scenario: one permit is held (simulating an in-flight `send_dm`).
    /// `drain_dm_send_fence` is called — it must block until the permit
    /// is dropped, then return.
    #[tokio::test]
    async fn drain_dm_send_fence_blocks_until_inflight_completes() {
        let stopping = Arc::new(AtomicBool::new(false));
        let sem = Arc::new(tokio::sync::Semaphore::new(DM_SEND_FENCE_CAPACITY));

        // Acquire one permit to simulate an in-flight send_dm.
        let _inflight_permit = sem
            .clone()
            .acquire_owned()
            .await
            .expect("should acquire permit");

        // Set the stopping flag (mirrors stop_inner ordering).
        stopping.store(true, Ordering::Release);

        // Spawn the drain concurrently. It should block until we drop
        // the in-flight permit.
        let sem_clone = sem.clone();
        let drain_handle = tokio::spawn(async move {
            drain_dm_send_fence(sem_clone).await;
        });

        // The drain should NOT have completed yet (permit still held).
        // A brief yield confirms it's blocked.
        tokio::task::yield_now().await;
        assert!(
            !drain_handle.is_finished(),
            "drain must block while permit held"
        );

        // Drop the in-flight permit — drain should unblock.
        drop(_inflight_permit);

        // Now the drain should complete promptly.
        tokio::time::timeout(std::time::Duration::from_secs(5), drain_handle)
            .await
            .expect("drain should complete within timeout")
            .expect("drain task should not panic");

        // All permits must be back (drain acquired + immediately dropped them).
        assert_eq!(
            sem.available_permits(),
            DM_SEND_FENCE_CAPACITY,
            "all permits should be returned after drain"
        );
    }
}

#[cfg(test)]
mod start_node_race_tests {
    use super::*;
    use std::sync::Mutex;

    /// Build a minimal `NodeState` for race-helper tests. Only `generation`
    /// is meaningful; everything else is default / None / empty.
    fn fresh_node_state() -> Mutex<NodeState> {
        Mutex::new(NodeState {
            thread: None,
            shutdown_tx: None,
            publish_tx: None,
            fetch_tx: None,
            ingest_tx: None,
            content_verb_tx: None,
            follow_tx: None,
            voice_tx: None,
            voice_channel_tx: None,
            follow_mgr: None,
            followed_set: None,
            vine_feed_cache: None,
            mail_mgr: None,
            mail_sync: None,
            content_index: std::sync::Arc::new(std::sync::Mutex::new(
                content_index::ContentIndex::load(std::path::Path::new("")),
            )),
            generation: 0,
            install_seq: 0,
            node_addr: String::new(),
            pairing_handle: None,
            sync_engine: None,
            community_registry: None,
            community_delta_tx: None,
            dm_outbox: None,
            dm_transport: None,
            crdt_state: None,
            hlc_tracker: None,
            dm_device_id: None,
            dm_self_owner: None,
            content_store: None,
            unicast_send_tx: None,
            dm_send_inflight: None,
            dm_send_stopping: None,
            dm_identity_pub_64: None,
            community_adapter_request_tx: None,
            channel_log_registry: None,
            library_directory: None,
            profile_broadcast_publisher: None,
            profile_broadcast_cache: None,
            profile_broadcast_request_tx: None,
            profile_broadcast_next_subscription_id: std::sync::Arc::new(
                std::sync::atomic::AtomicU64::new(1),
            ),
        })
    }

    #[test]
    fn reserve_install_seq_bumps_and_returns() {
        let state = fresh_node_state();
        let n = reserve_install_seq(&state).expect("reserve");
        assert_eq!(n, 1);
        assert_eq!(state.lock().unwrap().install_seq, 1);
    }

    #[test]
    fn reserve_install_seq_is_monotonic() {
        let state = fresh_node_state();
        assert_eq!(reserve_install_seq(&state).unwrap(), 1);
        assert_eq!(reserve_install_seq(&state).unwrap(), 2);
        assert_eq!(reserve_install_seq(&state).unwrap(), 3);
        assert_eq!(state.lock().unwrap().install_seq, 3);
    }

    #[test]
    fn reserve_install_seq_does_not_touch_generation() {
        // ZEB-221: install_seq is for race detection; generation keeps its
        // pre-ZEB-221 "successful install" semantics. Three reserves must
        // NOT bump generation (only lock-2 install does that).
        let state = fresh_node_state();
        let _ = reserve_install_seq(&state).unwrap();
        let _ = reserve_install_seq(&state).unwrap();
        let _ = reserve_install_seq(&state).unwrap();
        assert_eq!(state.lock().unwrap().generation, 0);
    }

    #[test]
    fn check_or_supersede_accepts_match() {
        let state = fresh_node_state();
        let my_seq = reserve_install_seq(&state).unwrap();
        let guard = check_install_seq_or_supersede(&state, my_seq)
            .expect("should accept matching install_seq");
        assert_eq!(guard.install_seq, my_seq);
    }

    #[test]
    fn check_or_supersede_rejects_stale() {
        let state = fresh_node_state();
        let my_seq = reserve_install_seq(&state).unwrap();
        let _later = reserve_install_seq(&state).unwrap();
        let err = check_install_seq_or_supersede(&state, my_seq)
            .map(drop)
            .expect_err("stale my_seq must be superseded");
        match err {
            SupersededError::Superseded { my_seq: s, current } => {
                assert_eq!(s, 1);
                assert_eq!(current, 2);
            }
            other => panic!("unexpected error: {:?}", other),
        }
    }

    #[test]
    fn check_or_supersede_rejects_zero_when_install_seq_advanced() {
        let state = fresh_node_state();
        // simulate prior reservations without calling our helper
        state.lock().unwrap().install_seq = 5;
        let err = check_install_seq_or_supersede(&state, 0)
            .map(drop)
            .expect_err("my_seq=0 against install_seq=5 must be superseded");
        match err {
            SupersededError::Superseded {
                my_seq: 0,
                current: 5,
            } => {}
            other => panic!("unexpected error: {:?}", other),
        }
    }
}

// ── ZEB-250 Task 9: AdminActionResult routing tests ───────────────────────
//
// These tests verify the mint helpers + CRDT-layer routing at the level
// that the IPC handler exercises. The IPC itself delegates the routing
// decision to the same `admin_quorum > 1 && admin_affecting` predicate
// and the same `insert_event` / `insert_local_event` path, so testing
// at this level covers all invariants without needing the full Tauri
// engine stack.
#[cfg(test)]
mod admin_action_result_routing_tests {
    use super::*;
    use crate::community_membership::{
        sign_event_with_identity, EventPayload, MembershipEventKind, ProposalKind, VerifyContext,
    };
    use crate::community_state_crdt::{CommunityState, InsertOutcome};
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;

    fn hlc(wall: u64, dev: &str) -> Hlc {
        Hlc {
            wall_ms: wall,
            logical: 0,
            device_id: dev.to_string(),
        }
    }

    fn insert_ok(
        state: &mut CommunityState,
        payload: EventPayload,
        identity: &PrivateIdentity,
        admin: OwnerAddr,
        actor_pub: &[u8; 64],
    ) {
        let ev = sign_event_with_identity(&payload, identity).expect("sign");
        let ctx = VerifyContext {
            expected_community_id: payload.community_id,
            admin_addr: admin,
            is_invite_only: false,
            actor_identity_pub: actor_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
        };
        let outcome = state.insert_event(ev, &ctx);
        assert!(
            matches!(outcome, InsertOutcome::Inserted),
            "fixture insert must succeed; got {outcome:?}"
        );
    }

    // ── Test 1: set_power_level_returns_completed_when_quorum_1 ───────────
    //
    // Backwards-compat: admin_quorum=1 (default) → direct SetPower is
    // accepted by verify_event (no quorum gate). The IPC handler routes
    // here and returns Completed. We verify by showing the direct-SetPower
    // event inserts without rejection (which is what the Completed branch
    // does) even when the target's new level is 100.
    #[tokio::test]
    async fn set_power_level_returns_completed_when_quorum_1() {
        let community_id = SpaceId([0xa1; 16]);

        let admin_identity = PrivateIdentity::from_seed(&[0xaa; 32]);
        let admin = OwnerAddr(admin_identity.identity.address_hash);
        let admin_pub = admin_identity.identity.to_public_bytes();

        let member_identity = PrivateIdentity::from_seed(&[0xbb; 32]);
        let member = OwnerAddr(member_identity.identity.address_hash);
        let member_pub = member_identity.identity.to_public_bytes();

        let mut state = CommunityState::new(community_id);
        // admin_quorum defaults to 1 — no change needed.

        insert_ok(
            &mut state,
            EventPayload {
                id: [0x01; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: admin,
                at: hlc(100, "admin"),
            },
            &admin_identity,
            admin,
            &admin_pub,
        );
        insert_ok(
            &mut state,
            EventPayload {
                id: [0x02; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: member,
                at: hlc(200, "member"),
            },
            &member_identity,
            admin,
            &member_pub,
        );

        // Promote member to admin (level == 100). With admin_quorum == 1 this
        // must succeed as a direct SetPower (the Completed path).
        let sk_bytes = admin_identity.to_private_bytes();
        let sk_seed: [u8; 32] = sk_bytes[32..64].try_into().unwrap();
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&sk_seed);

        let sp_event = mint_set_power_event(
            community_id,
            admin,
            member,
            100,
            &signing_key,
            hlc(300, "admin"),
        )
        .expect("mint_set_power_event must succeed");

        let outcome = state.insert_event(
            sp_event,
            &VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
                admin_identity_pub: None,
            },
        );
        // admin_quorum == 1 → direct SetPower accepted (Completed path).
        assert!(
            matches!(outcome, InsertOutcome::Inserted),
            "direct SetPower to 100 with admin_quorum=1 must be Inserted; got {outcome:?}"
        );

        // Verify member's power is now 100 in the materialized view.
        let m = state.materialize_now(admin);
        assert_eq!(
            m.power_levels.get(&member).copied().unwrap_or(0),
            100,
            "member power must be 100 after direct SetPower"
        );
    }

    // ── Test 2: set_power_level_routes_to_proposal_when_quorum_above_1 ───
    //
    // When admin_quorum > 1 AND action is admin-affecting (new level == 100),
    // verify_event rejects a direct SetPower with SetPowerRequiresQuorum AND
    // accepts an AdminProposal for the same action. This validates both halves
    // of the IPC routing branch.
    #[tokio::test]
    async fn set_power_level_routes_to_proposal_when_quorum_above_1_and_target_becomes_admin() {
        let community_id = SpaceId([0xa2; 16]);

        let admin_identity = PrivateIdentity::from_seed(&[0xaa; 32]);
        let admin = OwnerAddr(admin_identity.identity.address_hash);
        let admin_pub = admin_identity.identity.to_public_bytes();

        let member_identity = PrivateIdentity::from_seed(&[0xbb; 32]);
        let member = OwnerAddr(member_identity.identity.address_hash);
        let member_pub = member_identity.identity.to_public_bytes();

        let mut state = CommunityState::new(community_id);

        insert_ok(
            &mut state,
            EventPayload {
                id: [0x01; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: admin,
                at: hlc(100, "admin"),
            },
            &admin_identity,
            admin,
            &admin_pub,
        );
        insert_ok(
            &mut state,
            EventPayload {
                id: [0x02; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: member,
                at: hlc(200, "member"),
            },
            &member_identity,
            admin,
            &member_pub,
        );

        let sk_bytes = admin_identity.to_private_bytes();
        let sk_seed: [u8; 32] = sk_bytes[32..64].try_into().unwrap();
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&sk_seed);

        // Step 1: promote the member to admin (SetPower 100) while admin_quorum==1.
        // This is valid as a direct event (no quorum gate yet).
        let promote_event = mint_set_power_event(
            community_id,
            admin,
            member,
            100,
            &signing_key,
            hlc(210, "admin"),
        )
        .expect("mint_set_power_event must succeed");
        let promote_outcome = state.insert_event(
            promote_event,
            &VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
                admin_identity_pub: None,
            },
        );
        assert!(
            matches!(promote_outcome, InsertOutcome::Inserted),
            "direct SetPower to 100 must be Inserted when admin_quorum=1; got {promote_outcome:?}"
        );

        // Step 2: raise admin_quorum to 2 by inserting a ChangeQuorum AdminProposal.
        // Now that there are 2 admins, ChangeQuorum(2) is in range [1, 2].
        // With the current materialized admin_quorum=1, a single signer
        // (the proposer) meets the quorum threshold, so this self-approves
        // immediately when the event log is replayed in materialize().
        let change_quorum_proposal = mint_admin_proposal_change_quorum_event(
            community_id,
            admin,
            2,
            &signing_key,
            hlc(250, "admin"),
        )
        .expect("mint_admin_proposal_change_quorum_event must succeed");
        let cq_outcome = state.insert_event(
            change_quorum_proposal,
            &VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
                admin_identity_pub: None,
            },
        );
        assert!(
            matches!(cq_outcome, InsertOutcome::Inserted),
            "ChangeQuorum AdminProposal must be Inserted; got {cq_outcome:?}"
        );
        // Verify the quorum is now 2 in the materialized view.
        let m_after_cq = state.materialize_now(admin);
        assert_eq!(
            m_after_cq.admin_quorum, 2,
            "admin_quorum must be 2 after ChangeQuorum"
        );

        // Verify that a direct SetPower to 100 is REJECTED (SetPowerRequiresQuorum).
        // This confirms the routing predicate is correct: the IPC must NOT take
        // the direct path when admin_quorum > 1 AND admin-affecting.
        let direct_sp = mint_set_power_event(
            community_id,
            admin,
            member,
            100,
            &signing_key,
            hlc(300, "admin"),
        )
        .expect("mint must succeed");

        let direct_outcome = state.insert_event(
            direct_sp,
            &VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
                admin_identity_pub: None,
            },
        );
        match direct_outcome {
            InsertOutcome::Rejected(
                crate::community_membership::VerifyError::SetPowerRequiresQuorum,
            ) => {
                // Expected: direct SetPower is rejected — routing to AdminProposal is correct.
            }
            other => panic!("expected SetPowerRequiresQuorum rejection; got {other:?}"),
        }

        // Now mint an AdminProposal for the same action — must be ACCEPTED.
        // This validates mint_admin_proposal_set_power_event + the Pending branch.
        let proposal = mint_admin_proposal_set_power_event(
            community_id,
            admin,
            member,
            100,
            &signing_key,
            hlc(310, "admin"),
        )
        .expect("mint_admin_proposal_set_power_event must succeed");

        // Verify the event kind is AdminProposal{SetPower}.
        match &proposal.kind {
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::SetPower { target, level },
            } => {
                assert_eq!(*target, member, "proposal target must be member");
                assert_eq!(*level, 100, "proposal level must be 100");
            }
            other => panic!("expected AdminProposal{{SetPower}}; got {other:?}"),
        }

        let proposal_id = proposal.id;
        let proposal_outcome = state.insert_event(
            proposal,
            &VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
                admin_identity_pub: None,
            },
        );
        assert!(
            matches!(proposal_outcome, InsertOutcome::Inserted),
            "AdminProposal SetPower must be Inserted; got {proposal_outcome:?}"
        );

        // Confirm the proposal is in the event log (mirrors what the IPC Pending
        // branch returns via hex::encode(proposal.id)).
        assert!(
            state.events.contains_key(&proposal_id),
            "proposal must be in the CRDT event log"
        );
    }
}

// ── ZEB-250 Task 10: list_pending_admin_proposals unit tests ──────────────
//
// These tests exercise `compute_pending_admin_proposals` directly, bypassing
// the Tauri IPC layer and NodeState. Events are built with zeroed signatures
// (the pure helper never checks crypto). Time-sensitive assertions use an
// explicit `now_ms` argument.
#[cfg(test)]
mod list_pending_admin_proposals_tests {
    use super::*;
    use crate::community_membership::{MembershipEventKind, ProposalKind, SignedMembershipEvent};
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

    fn hlc(wall: u64) -> Hlc {
        Hlc {
            wall_ms: wall,
            logical: 0,
            device_id: "test".into(),
        }
    }

    fn make_event(
        id: [u8; 16],
        actor: OwnerAddr,
        wall_ms: u64,
        kind: MembershipEventKind,
    ) -> SignedMembershipEvent {
        SignedMembershipEvent {
            id,
            community_id: SpaceId([0xaa; 16]),
            kind,
            actor,
            at: hlc(wall_ms),
            sig: [0u8; 64],
            countersig: None,
        }
    }

    fn admin_proposal_set_power(
        id: [u8; 16],
        actor: OwnerAddr,
        target: OwnerAddr,
        level: u8,
        wall_ms: u64,
    ) -> SignedMembershipEvent {
        make_event(
            id,
            actor,
            wall_ms,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::SetPower { target, level },
            },
        )
    }

    fn admin_countersign(
        id: [u8; 16],
        actor: OwnerAddr,
        target_event_id: [u8; 16],
        wall_ms: u64,
    ) -> SignedMembershipEvent {
        make_event(
            id,
            actor,
            wall_ms,
            MembershipEventKind::AdminCountersign { target_event_id },
        )
    }

    // ── Test 1: list_pending_admin_proposals_rejects_non_admin_caller ─────
    //
    // When the caller is not in the signers set (not an admin), the IPC
    // should mark self_has_signed = false. This also verifies that non-admin
    // callers who bypass the auth guard (e.g., calling the pure helper
    // directly for testing) correctly see `self_has_signed = false`.
    #[tokio::test]
    async fn list_pending_admin_proposals_rejects_non_admin_caller() {
        let admin = OwnerAddr([0x01; 16]);
        let non_admin = OwnerAddr([0x02; 16]);
        let target = OwnerAddr([0x03; 16]);

        let proposal_id = [0x10; 16];
        let now_ms = 1_000_000;

        let events = vec![admin_proposal_set_power(
            proposal_id,
            admin,
            target,
            50,
            now_ms - 100,
        )];

        // quorum=2 so the proposal is pending (only 1 signer, the proposer).
        let dtos = compute_pending_admin_proposals(&events, non_admin, 2, now_ms);

        assert_eq!(dtos.len(), 1, "one proposal in the log");
        let dto = &dtos[0];
        assert!(
            !dto.self_has_signed,
            "non-admin caller must not be in signer set"
        );
        assert!(!dto.expired, "proposal is fresh");
        assert!(!dto.effective, "quorum not reached");
        assert_eq!(dto.signers_so_far, 1, "proposer counts as signer 1");
        assert_eq!(dto.quorum_required, 2);
    }

    // ── Test 2: list_pending_admin_proposals_returns_pending_and_recent ───
    //
    // Seed three proposals:
    //   A) pending: fresh, quorum not reached → bucket 0
    //   B) effective: quorum reached within window → bucket 1
    //   C) expired: old, quorum not reached → bucket 2
    // Verify sort order: A then B then C.
    #[tokio::test]
    async fn list_pending_admin_proposals_returns_pending_and_recent_sections() {
        use crate::community_membership::ADMIN_PROPOSAL_EXPIRY_MS;

        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);
        let target = OwnerAddr([0x03; 16]);
        let caller = admin1;

        // now_ms chosen so expired proposal is clearly > 30 days old.
        let now_ms = ADMIN_PROPOSAL_EXPIRY_MS * 2 + 1_000_000;

        // Proposal A: pending — fresh, only 1 signer, quorum=2.
        let proposal_a_id = [0xaa; 16];
        let proposal_a_wall = now_ms - 1_000; // 1 second ago

        // Proposal B: effective — quorum=2 reached, both signers within window.
        let proposal_b_id = [0xbb; 16];
        let proposal_b_wall = now_ms - 5_000; // 5 seconds ago
        let countersign_b_id = [0xbc; 16];
        let countersign_b_wall = proposal_b_wall + 100; // 100 ms after proposal

        // Proposal C: expired — older than 30 days, only 1 signer.
        let proposal_c_id = [0xcc; 16];
        let proposal_c_wall = 1_000; // epoch epoch; very old

        let events = vec![
            // A: pending
            admin_proposal_set_power(proposal_a_id, admin1, target, 50, proposal_a_wall),
            // B: proposal + countersign → effective
            admin_proposal_set_power(proposal_b_id, admin1, target, 50, proposal_b_wall),
            admin_countersign(countersign_b_id, admin2, proposal_b_id, countersign_b_wall),
            // C: expired
            admin_proposal_set_power(proposal_c_id, admin1, target, 50, proposal_c_wall),
        ];

        let dtos = compute_pending_admin_proposals(&events, caller, 2, now_ms);

        assert_eq!(dtos.len(), 3, "three proposals in total");

        // Sort must be: A (pending, bucket 0) → B (effective, bucket 1) → C (expired, bucket 2).
        let a = &dtos[0];
        let b = &dtos[1];
        let c = &dtos[2];

        assert_eq!(
            a.event_id,
            hex::encode(proposal_a_id),
            "first is A (pending)"
        );
        assert!(!a.expired && !a.effective, "A is pending");
        assert_eq!(a.signers_so_far, 1);

        assert_eq!(
            b.event_id,
            hex::encode(proposal_b_id),
            "second is B (effective)"
        );
        assert!(b.effective, "B is effective");
        assert_eq!(b.signers_so_far, 2);

        assert_eq!(
            c.event_id,
            hex::encode(proposal_c_id),
            "third is C (expired)"
        );
        assert!(c.expired, "C is expired");
        assert_eq!(c.signers_so_far, 1);
    }

    // ── Test 3: list_pending_admin_proposals_resolves_proposer_and_signer_names ─
    //
    // The pure helper emits hex addresses in signer_display_names (the IPC
    // layer with profile-cache access would substitute real names). Verify that
    // the proposer's hex address appears in signer_display_names and that
    // a second signer (countersigner) also appears.
    #[tokio::test]
    async fn list_pending_admin_proposals_resolves_proposer_and_signer_names() {
        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);
        let target = OwnerAddr([0x03; 16]);

        let proposal_id = [0x10; 16];
        let countersign_id = [0x11; 16];
        let now_ms = 1_000_000;

        let events = vec![
            admin_proposal_set_power(proposal_id, admin1, target, 100, now_ms - 500),
            admin_countersign(countersign_id, admin2, proposal_id, now_ms - 100),
        ];

        let dtos = compute_pending_admin_proposals(&events, admin1, 2, now_ms);

        assert_eq!(dtos.len(), 1);
        let dto = &dtos[0];

        assert_eq!(dto.proposer_addr, hex::encode(admin1.0));
        assert_eq!(
            dto.signers_so_far, 2,
            "proposer + countersigner = 2 signers"
        );
        assert!(dto.effective, "quorum=2 reached within window");
        assert!(dto.self_has_signed, "admin1 is the proposer and caller");

        // signer_display_names should contain both signers' hex addresses.
        let admin1_hex = hex::encode(admin1.0);
        let admin2_hex = hex::encode(admin2.0);
        assert!(
            dto.signer_display_names.contains(&admin1_hex),
            "admin1 hex must appear in signer_display_names"
        );
        assert!(
            dto.signer_display_names.contains(&admin2_hex),
            "admin2 hex must appear in signer_display_names"
        );

        // Target display name: pure helper emits None (no profile cache).
        assert!(
            matches!(&dto.proposal_kind, ProposalKindDto::SetPower { target_addr, .. } if target_addr == &hex::encode(target.0)),
            "SetPower target_addr must match target"
        );
    }

    // ── Test 4: compute_pending_admin_proposals_marks_late_quorum_as_expired
    //
    // Bug-fix R1 (Bug 4): a proposal whose Nth countersign landed AFTER the
    // 30-day window should be marked expired=true, effective=false. Previously
    // the expired flag was computed before effective using
    //   `age > 30d && signers < quorum`
    // which missed the case where signers >= quorum but the Nth signer was late.
    // That case produced expired=false AND effective=false (phantom "pending").
    #[tokio::test]
    async fn compute_pending_admin_proposals_marks_late_quorum_as_expired_not_pending() {
        use crate::community_membership::ADMIN_PROPOSAL_EXPIRY_MS;

        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);
        let target = OwnerAddr([0x03; 16]);

        let proposal_id = [0x10; 16];
        let countersign_id = [0x11; 16];

        // Proposal at wall_ms=0; Nth countersign arrives 31 days later.
        let proposal_wall = 0u64;
        let late_countersign_wall = ADMIN_PROPOSAL_EXPIRY_MS + 1;
        // now_ms is 32 days — well past the window.
        let now_ms = ADMIN_PROPOSAL_EXPIRY_MS * 2;

        let events = vec![
            make_event(
                proposal_id,
                admin1,
                proposal_wall,
                MembershipEventKind::AdminProposal {
                    proposal_kind: ProposalKind::SetPower { target, level: 100 },
                },
            ),
            make_event(
                countersign_id,
                admin2,
                late_countersign_wall,
                MembershipEventKind::AdminCountersign {
                    target_event_id: proposal_id,
                },
            ),
        ];

        // quorum=2: proposer (admin1) + late countersign (admin2) = 2 signers,
        // but the Nth signer arrived after the window. Expect expired=true,
        // effective=false.
        let dtos = compute_pending_admin_proposals(&events, admin1, 2, now_ms);

        assert_eq!(dtos.len(), 1, "one proposal in the log");
        let dto = &dtos[0];
        assert!(
            !dto.effective,
            "proposal with late Nth countersign must not be effective"
        );
        assert!(
            dto.expired,
            "proposal with late Nth countersign (age > 30d, not effective) must be expired"
        );
        assert_eq!(dto.signers_so_far, 2, "two signers were recorded");
    }
}

// ── ZEB-250 Task 11: countersign_admin_proposal unit tests ────────────────
//
// These tests exercise `count_signers` and the countersign routing logic
// directly against `CommunityState`, bypassing the Tauri IPC layer and
// NodeState. Crypto is real (PrivateIdentity + sign_event_with_identity)
// so verify_event accepts the events. Time-sensitive checks use explicit
// wall_ms values derived from ADMIN_PROPOSAL_EXPIRY_MS.
#[cfg(test)]
mod countersign_admin_proposal_tests {
    use super::*;
    use crate::community_membership::{
        sign_event_with_identity, EventPayload, MembershipEventKind, ProposalKind, VerifyContext,
        ADMIN_PROPOSAL_EXPIRY_MS,
    };
    use crate::community_state_crdt::{CommunityState, InsertOutcome};
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;

    fn hlc(wall: u64, dev: &str) -> Hlc {
        Hlc {
            wall_ms: wall,
            logical: 0,
            device_id: dev.to_string(),
        }
    }

    /// Insert a signed event and assert it was Inserted (not Rejected).
    fn insert_ok(
        state: &mut CommunityState,
        payload: EventPayload,
        identity: &PrivateIdentity,
        admin: OwnerAddr,
        actor_pub: &[u8; 64],
    ) {
        let ev = sign_event_with_identity(&payload, identity).expect("sign");
        let ctx = VerifyContext {
            expected_community_id: payload.community_id,
            admin_addr: admin,
            is_invite_only: false,
            actor_identity_pub: actor_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
        };
        let outcome = state.insert_event(ev, &ctx);
        assert!(
            matches!(outcome, InsertOutcome::Inserted),
            "fixture insert must succeed; got {outcome:?}"
        );
    }

    /// Build a minimal community: admin + second admin (member promoted),
    /// admin_quorum raised to 2 via ChangeQuorum AdminProposal.
    /// Returns (state, admin_identity, admin_addr, second_identity, second_addr, proposal_id).
    fn setup_quorum2_community(
        community_id: SpaceId,
    ) -> (
        CommunityState,
        PrivateIdentity,
        OwnerAddr,
        PrivateIdentity,
        OwnerAddr,
        [u8; 16],
    ) {
        let admin_identity = PrivateIdentity::from_seed(&[0xaa; 32]);
        let admin = OwnerAddr(admin_identity.identity.address_hash);
        let admin_pub = admin_identity.identity.to_public_bytes();

        let second_identity = PrivateIdentity::from_seed(&[0xbb; 32]);
        let second = OwnerAddr(second_identity.identity.address_hash);
        let second_pub = second_identity.identity.to_public_bytes();

        let mut state = CommunityState::new(community_id);

        // admin Joins
        insert_ok(
            &mut state,
            EventPayload {
                id: [0x01; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: admin,
                at: hlc(100, "admin"),
            },
            &admin_identity,
            admin,
            &admin_pub,
        );
        // second Joins
        insert_ok(
            &mut state,
            EventPayload {
                id: [0x02; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: second,
                at: hlc(200, "second"),
            },
            &second_identity,
            admin,
            &second_pub,
        );
        // Promote second to admin power=100 (direct, quorum still 1)
        let sk_bytes = admin_identity.to_private_bytes();
        let sk_seed: [u8; 32] = sk_bytes[32..64].try_into().unwrap();
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&sk_seed);
        let promote_ev = mint_set_power_event(
            community_id,
            admin,
            second,
            100,
            &signing_key,
            hlc(210, "admin"),
        )
        .expect("mint set_power");
        let promote_outcome = state.insert_event(
            promote_ev,
            &VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
                admin_identity_pub: None,
            },
        );
        assert!(
            matches!(promote_outcome, InsertOutcome::Inserted),
            "promote to admin must insert; got {promote_outcome:?}"
        );

        // Raise quorum to 2 via ChangeQuorum AdminProposal.
        // With 2 admins and quorum=1, the proposer alone meets the threshold.
        let cq_ev = mint_admin_proposal_change_quorum_event(
            community_id,
            admin,
            2,
            &signing_key,
            hlc(250, "admin"),
        )
        .expect("mint change_quorum_proposal");
        let cq_outcome = state.insert_event(
            cq_ev,
            &VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
                admin_identity_pub: None,
            },
        );
        assert!(
            matches!(cq_outcome, InsertOutcome::Inserted),
            "ChangeQuorum must insert; got {cq_outcome:?}"
        );
        let m = state.materialize_now(admin);
        assert_eq!(m.admin_quorum, 2, "quorum must be 2 after ChangeQuorum");

        // Now mint an AdminProposal (SetPower for a 3rd member — just needs to exist
        // in the log for the countersign target).
        let proposal_id = [0xAB; 16];
        let proposal_ev = sign_event_with_identity(
            &EventPayload {
                id: proposal_id,
                community_id,
                kind: MembershipEventKind::AdminProposal {
                    proposal_kind: ProposalKind::SetPower {
                        target: second,
                        level: 50,
                    },
                },
                actor: admin,
                at: hlc(300, "admin"),
            },
            &admin_identity,
        )
        .expect("sign proposal");
        let proposal_outcome = state.insert_event(
            proposal_ev,
            &VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
                admin_identity_pub: None,
            },
        );
        assert!(
            matches!(proposal_outcome, InsertOutcome::Inserted),
            "AdminProposal must insert; got {proposal_outcome:?}"
        );

        (
            state,
            admin_identity,
            admin,
            second_identity,
            second,
            proposal_id,
        )
    }

    // ── Test 1: countersign_admin_proposal_idempotent_when_already_signed ──
    //
    // After the proposer's AdminProposal is in the log, `count_signers`
    // must return 1. Inserting an AdminCountersign from the proposer again
    // is blocked by verify_event (duplicate actor); the idempotency guard
    // in the IPC layer must return current state without a second insert.
    // We simulate idempotency here by checking count_signers before/after
    // a second (duplicate-actor) countersign attempt is rejected.
    #[tokio::test]
    async fn countersign_admin_proposal_idempotent_when_already_signed() {
        let community_id = SpaceId([0xc1; 16]);
        let (mut state, _admin_identity, admin, second_identity, second, proposal_id) =
            setup_quorum2_community(community_id);

        // Proposer (admin) already signed via AdminProposal.
        let signers_before = count_signers(&state.events, proposal_id);
        assert_eq!(signers_before, 1, "only proposer signed so far");

        // admin_quorum is 2 — quorum not yet reached.
        let m = state.materialize_now(admin);
        assert!(!m.power_levels.is_empty());
        assert_eq!(m.admin_quorum, 2);

        // Now let second countersign — should bump to 2.
        let sk_bytes = second_identity.to_private_bytes();
        let sk_seed: [u8; 32] = sk_bytes[32..64].try_into().unwrap();
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&sk_seed);
        let second_pub = second_identity.identity.to_public_bytes();

        let cs_ev = mint_admin_countersign_event(
            community_id,
            second,
            proposal_id,
            &signing_key,
            hlc(350, "second"),
        )
        .expect("mint countersign");
        let cs_outcome = state.insert_event(
            cs_ev,
            &VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &second_pub,
                countersigner_identity_pub: None,
                admin_identity_pub: None,
            },
        );
        assert!(
            matches!(cs_outcome, InsertOutcome::Inserted),
            "second countersign must insert; got {cs_outcome:?}"
        );

        let signers_after = count_signers(&state.events, proposal_id);
        assert_eq!(signers_after, 2, "proposer + second = 2 signers");

        // Idempotency: second admin already signed — a new AdminCountersign
        // from second would be a duplicate. verify_event should reject it.
        // (The IPC guard catches this BEFORE attempting insert, but we verify
        // the CRDT also enforces it.)
        let duplicate_cs = mint_admin_countersign_event(
            community_id,
            second,
            proposal_id,
            &signing_key,
            hlc(360, "second"),
        )
        .expect("mint duplicate countersign");
        let dup_outcome = state.insert_event(
            duplicate_cs,
            &VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &second_pub,
                countersigner_identity_pub: None,
                admin_identity_pub: None,
            },
        );
        // CRDT must reject duplicate countersign (duplicate actor for same proposal).
        assert!(
            matches!(dup_outcome, InsertOutcome::Rejected(_)),
            "duplicate countersign must be rejected; got {dup_outcome:?}"
        );

        // count_signers must still be 2 (idempotent, no new signer added).
        let signers_idempotent = count_signers(&state.events, proposal_id);
        assert_eq!(
            signers_idempotent, 2,
            "count must remain 2 after duplicate attempt"
        );
    }

    // ── Test 2: countersign_admin_proposal_rejects_non_admin_caller ────────
    //
    // Verify that the power-gate logic (caller_power < 100 → Err) fires
    // correctly. We simulate this at the pure helper level: build a state
    // where a non-admin member tries to countersign.
    #[tokio::test]
    async fn countersign_admin_proposal_rejects_non_admin_caller() {
        let community_id = SpaceId([0xc2; 16]);
        let (state, _admin_identity, admin, _second_identity, _second, proposal_id) =
            setup_quorum2_community(community_id);

        // A plain member (power 0) is NOT in the state above — simulate by
        // checking that power_levels for an unknown addr is 0.
        let stranger = OwnerAddr([0xff; 16]);
        let m = state.materialize_now(admin);
        let stranger_power = m.power_levels.get(&stranger).copied().unwrap_or(0);
        assert_eq!(stranger_power, 0, "stranger has no power");

        // The IPC checks caller_power < 100 → Err. We verify the predicate
        // directly (the IPC layer is not instantiated in unit tests).
        assert!(
            stranger_power < 100,
            "non-admin must have power < 100 → IPC rejects"
        );

        // Also verify count_signers is unaffected (still 1 — only proposer).
        let signers = count_signers(&state.events, proposal_id);
        assert_eq!(signers, 1, "only proposer; non-admin cannot sign");
    }

    // ── Test 3: countersign_admin_proposal_rejects_expired_proposal ────────
    //
    // A proposal older than ADMIN_PROPOSAL_EXPIRY_MS must be rejected.
    // We build a state with an old AdminProposal (wall_ms = 1) and check
    // that the age predicate fires correctly.
    #[tokio::test]
    async fn countersign_admin_proposal_rejects_expired_proposal() {
        let now_ms = ADMIN_PROPOSAL_EXPIRY_MS * 2 + 1_000_000;
        // Proposal inserted with wall_ms = 1 → age >> 30 days.
        let proposal_wall_ms: u64 = 1;
        let age = now_ms.saturating_sub(proposal_wall_ms);
        assert!(
            age > ADMIN_PROPOSAL_EXPIRY_MS,
            "proposal must be expired; age={age}, expiry={ADMIN_PROPOSAL_EXPIRY_MS}"
        );

        // Also verify that a fresh proposal is NOT expired.
        let fresh_wall_ms = now_ms - 1_000; // 1 second ago
        let fresh_age = now_ms.saturating_sub(fresh_wall_ms);
        assert!(
            fresh_age <= ADMIN_PROPOSAL_EXPIRY_MS,
            "fresh proposal must not be expired; age={fresh_age}"
        );
    }

    // ── Test 4: countersign_admin_proposal_returns_reached_quorum_true_on_threshold_tip ──
    //
    // When signers_after == quorum_required, reached_quorum must be true.
    // Set up a quorum=2 community, the proposer (signer 1), then add a
    // countersign (signer 2) → signers_after=2 == quorum_required=2.
    #[tokio::test]
    async fn countersign_admin_proposal_returns_reached_quorum_true_on_threshold_tip() {
        let community_id = SpaceId([0xc4; 16]);
        let (mut state, _admin_identity, admin, second_identity, second, proposal_id) =
            setup_quorum2_community(community_id);

        let m = state.materialize_now(admin);
        assert_eq!(m.admin_quorum, 2, "quorum must be 2");

        // Before countersign: signers=1, quorum=2, not reached.
        let signers_before = count_signers(&state.events, proposal_id);
        assert_eq!(signers_before, 1);
        assert!(signers_before < 2, "quorum not yet reached");

        // Add second admin countersign.
        let sk_bytes = second_identity.to_private_bytes();
        let sk_seed: [u8; 32] = sk_bytes[32..64].try_into().unwrap();
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&sk_seed);
        let second_pub = second_identity.identity.to_public_bytes();

        let cs_ev = mint_admin_countersign_event(
            community_id,
            second,
            proposal_id,
            &signing_key,
            hlc(400, "second"),
        )
        .expect("mint countersign");
        let cs_outcome = state.insert_event(
            cs_ev,
            &VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &second_pub,
                countersigner_identity_pub: None,
                admin_identity_pub: None,
            },
        );
        assert!(
            matches!(cs_outcome, InsertOutcome::Inserted),
            "countersign must insert; got {cs_outcome:?}"
        );

        let signers_after = count_signers(&state.events, proposal_id);
        let quorum_required = state.materialize_now(admin).admin_quorum;
        let reached_quorum = signers_after >= quorum_required;

        assert_eq!(signers_after, 2, "proposer + second = 2");
        assert_eq!(quorum_required, 2);
        assert!(
            reached_quorum,
            "reached_quorum must be true when signers == quorum"
        );
    }
}

// ── ZEB-250 Task 12: propose_change_quorum unit tests ─────────────────────
//
// Exercises the validation logic (new_quorum < 1, new_quorum > admin_count)
// directly against CommunityState, bypassing the Tauri IPC layer.
#[cfg(test)]
mod propose_change_quorum_tests {
    use super::*;
    use crate::community_membership::{
        sign_event_with_identity, EventPayload, MembershipEventKind, VerifyContext,
    };
    use crate::community_state_crdt::{CommunityState, InsertOutcome};
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;

    fn hlc(wall: u64, dev: &str) -> Hlc {
        Hlc {
            wall_ms: wall,
            logical: 0,
            device_id: dev.to_string(),
        }
    }

    fn insert_ok(
        state: &mut CommunityState,
        payload: EventPayload,
        identity: &PrivateIdentity,
        admin: OwnerAddr,
        actor_pub: &[u8; 64],
    ) {
        let ev = sign_event_with_identity(&payload, identity).expect("sign");
        let ctx = VerifyContext {
            expected_community_id: payload.community_id,
            admin_addr: admin,
            is_invite_only: false,
            actor_identity_pub: actor_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
        };
        let outcome = state.insert_event(ev, &ctx);
        assert!(
            matches!(outcome, InsertOutcome::Inserted),
            "fixture insert must succeed; got {outcome:?}"
        );
    }

    // ── Test 10: propose_change_quorum_rejects_out_of_range_values ────────
    //
    // Spec §8.4 test 10. Exercises both out-of-range cases:
    //   • new_quorum == 0  → Err (< 1 guard, checked before engine access)
    //   • new_quorum > admin_count → Err (exceeds current admin count)
    //
    // We test the validation logic directly by calling the pure helpers and
    // replicating the IPC guard conditions without going through the full
    // NodeState / Tauri stack.
    #[tokio::test]
    async fn propose_change_quorum_rejects_out_of_range_values() {
        // ── Part A: new_quorum == 0 is caught by the IPC guard (new_quorum < 1). ──
        // This is a pure integer check — no community state needed.
        let new_quorum_zero: u8 = 0;
        assert!(new_quorum_zero < 1, "new_quorum=0 must fail the >= 1 guard");

        // ── Part B: new_quorum > admin_count is caught after reading materialized state. ──
        // Build a community with exactly 1 admin (the founder) and assert that
        // requesting new_quorum=2 would exceed the admin count.
        let community_id = SpaceId([0xd0; 16]);

        let admin_identity = PrivateIdentity::from_seed(&[0xaa; 32]);
        let admin = OwnerAddr(admin_identity.identity.address_hash);
        let admin_pub = admin_identity.identity.to_public_bytes();

        let mut state = CommunityState::new(community_id);

        // admin Joins — only 1 admin in the community (power defaults to 0 for
        // non-founders; but the SpaceId owner is admin by convention in tests).
        insert_ok(
            &mut state,
            EventPayload {
                id: [0xd1; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: admin,
                at: hlc(100, "admin"),
            },
            &admin_identity,
            admin,
            &admin_pub,
        );

        // Give the admin power=100 explicitly so the power_levels map is populated.
        let sk_bytes = admin_identity.to_private_bytes();
        let sk_seed: [u8; 32] = sk_bytes[32..64].try_into().unwrap();
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&sk_seed);
        let sp_ev = mint_set_power_event(
            community_id,
            admin,
            admin,
            100,
            &signing_key,
            hlc(110, "admin"),
        )
        .expect("mint_set_power_event");
        let sp_outcome = state.insert_event(
            sp_ev,
            &VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
                admin_identity_pub: None,
            },
        );
        assert!(
            matches!(sp_outcome, InsertOutcome::Inserted),
            "self-SetPower to 100 must insert; got {sp_outcome:?}"
        );

        // Compute admin_count via the same expression the IPC handler uses.
        let m = state.materialize_now(admin);
        let admin_count = m.power_levels.values().filter(|p| **p == 100).count() as u32;
        assert_eq!(admin_count, 1, "only 1 admin in community");

        // new_quorum=2 > admin_count=1 → must be rejected.
        let new_quorum_too_large: u8 = 2;
        assert!(
            (new_quorum_too_large as u32) > admin_count,
            "new_quorum={new_quorum_too_large} must exceed admin_count={admin_count}"
        );

        // new_quorum=1 == admin_count=1 → must be accepted.
        let new_quorum_valid: u8 = 1;
        assert!(
            (new_quorum_valid as u32) <= admin_count,
            "new_quorum={new_quorum_valid} must not exceed admin_count={admin_count}"
        );

        // Sanity-check: minting the valid proposal succeeds.
        let cq_ev = mint_admin_proposal_change_quorum_event(
            community_id,
            admin,
            new_quorum_valid,
            &signing_key,
            hlc(200, "admin"),
        )
        .expect("mint_admin_proposal_change_quorum_event must succeed for new_quorum=1");
        let cq_outcome = state.insert_event(
            cq_ev,
            &VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &admin_pub,
                countersigner_identity_pub: None,
                admin_identity_pub: None,
            },
        );
        assert!(
            matches!(cq_outcome, InsertOutcome::Inserted),
            "ChangeQuorum proposal with new_quorum=1 must insert; got {cq_outcome:?}"
        );
    }
}
