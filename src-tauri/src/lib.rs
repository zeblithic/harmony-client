use std::sync::Mutex;
use std::thread;

use harmony_compute::InstructionBudget;
use harmony_content::book::MemoryBookStore;
use harmony_content::storage_tier::{ContentPolicy, FilterBroadcastConfig, StorageBudget};
use harmony_runtime::{NodeConfig, NodeRuntime};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

pub mod content_index;
pub mod content_store;
pub mod dm_envelope;
pub mod event_loop;
pub mod folders;
mod follows;
pub mod identity;
pub mod identity_commands;
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
pub mod recovery_cli;
pub mod recovery_policy;
mod save_dialog;
pub mod voice;

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
    /// Shared mail manager (read/written by event loop on receive, by commands for queries).
    mail_mgr: Option<std::sync::Arc<std::sync::Mutex<mail::MailManager>>>,
    /// Shared mail sync (walker + lazy body fetch). Stored here so Tauri
    /// commands (refresh_mail, fetch_mail_body) can reach it.
    mail_sync: Option<std::sync::Arc<mail_sync::MailSync>>,
    /// Disk-backed content index (pin/replication metadata).
    content_index: std::sync::Arc<std::sync::Mutex<content_index::ContentIndex>>,
    /// Monotonic connection generation (prevents stale stop_node races).
    generation: u64,
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
            mail_mgr: None,
            mail_sync: None,
            content_index: std::sync::Arc::new(std::sync::Mutex::new(
                content_index::ContentIndex::load(std::path::Path::new("")),
            )),
            generation: 0,
            node_addr: String::new(),
            pairing_handle: None,
            sync_engine: None,
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
        _mail_sync,
        pairing_handle,
        sync_engine,
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
        (
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
            // Drop mail_sync so refresh_mail / fetch_mail_body can't reach
            // a closed fetch_tx / refresh_tx after stop. Channels are
            // already gone above; the MailSync handle would just yield
            // "channel closed" errors until next start.
            guard.mail_sync.take(),
            guard.pairing_handle.take(),
            guard.sync_engine.take(),
        )
    };

    let had_node = shutdown_tx.is_some() || thread.is_some();
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

    // MailManager will be initialized after identity loading (needs owner address).
    // Placeholder — set below once we have our_addr_bytes.
    let mail_mgr: std::sync::Arc<std::sync::Mutex<mail::MailManager>>;

    // Stop existing node — extract handles under the lock in a tight
    // inner scope so the std `MutexGuard` (which is `!Send`) is fully
    // out of scope before the SyncEngine's `.await`. Without this
    // scoping, rustc's async generator analysis sees the guard's
    // storage slot as live across the await point and rejects the
    // function as not `Send`.
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
    ) = {
        let mut guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
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
        );
        let _old_follow_mgr = guard.follow_mgr.take();
        let _old_followed_set = guard.followed_set.take();
        let _old_mail_mgr = guard.mail_mgr.take();
        let _old_mail_sync = guard.mail_sync.take();
        tup
    };

    // Drop pairing_handle BEFORE publish_tx so the SM task's transport
    // sees its receiver close after the publish channel is gone — same
    // ordering as stop_inner.
    drop(old_pairing_handle);
    drop(old_publish);
    drop(old_fetch);
    drop(old_ingest);
    drop(old_content_verb);
    drop(old_follow);
    drop(old_voice);
    drop(old_voice_channel);
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

        let reticulum_identity_bytes = Some(zeroize::Zeroizing::new(ed25519.to_private_bytes()));
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

                    let engine = std::sync::Arc::new(crate::owner_state_sync::SyncEngine::new(
                        std::sync::Arc::clone(&kt),
                        device_id,
                        std::sync::Arc::clone(&crdt_state),
                        std::sync::Arc::clone(&tracker),
                        content_store,
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

        // Re-acquire lock and atomically register the new node.
        // Handles are stored BEFORE awaiting ready_rx so stop_node can
        // cancel an in-flight startup via shutdown_tx.
        let mut guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
        guard.generation += 1;

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
                    let (runtime, startup_actions) =
                        NodeRuntime::new(config, MemoryBookStore::new());
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
                        mail_mgr_clone,
                        Some(mail_sync_for_loop),
                        mail_refresh_rx,
                        pin_intent,
                        fetch_completion_tx,
                        fetch_completion_rx,
                        Some(pairing_in_tx),
                        sync_handles_for_loop,
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
        let thread_install_failure: Option<String>;
        match thread_result {
            Ok(thread) => {
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
                guard.mail_mgr = Some(mail_mgr);
                guard.mail_sync = Some(mail_sync);
                guard.node_addr = node_addr_for_state;
                guard.sync_engine = sync_engine_arc.clone();
                thread_install_failure = None;
            }
            Err(e) => {
                thread_install_failure = Some(format!("failed to spawn runtime thread: {e}"));
            }
        }
        // The third tuple element carries the SyncEngine Arc back out
        // of the block so the failure-cleanup path below can await
        // `shutdown()` on it without holding the std `MutexGuard`
        // across an await (the guard is `!Send`). On success this
        // Arc is discarded; NodeState already owns its own clone.
        (
            guard.generation,
            thread_install_failure,
            sync_engine_arc.clone(),
        )
    };
    let (our_gen, thread_spawn_failure, engine_for_cleanup) = our_gen;

    if let Some(msg) = thread_spawn_failure {
        if let Some(engine) = engine_for_cleanup {
            if let Err(e) = engine.shutdown().await {
                tracing::error!(
                    error = %e,
                    "SyncEngine cleanup after runtime-thread spawn failure"
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

#[tauri::command]
fn list_vine_videos() -> Vec<VineVideoDto> {
    // Future: return cached/persisted vines. Real data flows via vine-received events.
    Vec::new()
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

#[tauri::command]
fn mark_vine_viewed(vine_id: String) -> bool {
    // Future: persist viewed state + publish to network for cross-device sync.
    let _ = vine_id;
    true
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
    entries.sort_by(|a, b| b.stored_at.cmp(&a.stored_at));
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

// ── App entry point ──────────────────────────────────────────────────────

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
            identity_commands::export_recovery_file_to_path,
            identity_commands::restore_mnemonic_from_words,
            identity_commands::restore_recovery_from_preview_token,
            owner_commands::get_owner_state,
            owner_commands::mint_owner_identity,
            owner_commands::export_owner_recovery_file_to_path,
            owner_commands::issue_owner_recovery_token,
            save_dialog::request_export_save_path,
            pairing_commands::start_inviter_pairing,
            pairing_commands::start_joiner_pairing,
            pairing_commands::select_pairing_peer,
            pairing_commands::confirm_pairing_sas,
            pairing_commands::cancel_pairing,
            pairing_commands::get_pairing_state,
            #[cfg(debug_assertions)]
            e2e_close_window,
        ])
        .run(tauri::generate_context!())
        .expect("error while running harmony");
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
