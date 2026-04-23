use std::sync::Mutex;
use std::thread;

use harmony_compute::InstructionBudget;
use harmony_content::book::MemoryBookStore;
use harmony_content::storage_tier::{ContentPolicy, FilterBroadcastConfig, StorageBudget};
use harmony_runtime::{NodeConfig, NodeRuntime};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

pub mod content_index;
pub mod event_loop;
mod follows;
mod identity;
pub mod mail;
pub mod mail_sync;
pub mod voice;

// ── Chunked ingest (ZEB-154) ──────────────────────────────────────────────

/// Maximum bytes supported by the v1 flat-bundle chunked-ingest path.
///
/// = MAX_BUNDLE_ENTRIES × MAX_PAYLOAD_SIZE ≈ 32 GiB. Files larger than this
/// need nested bundles, which land with folder/directory support (ZEB-156
/// et al). A flat-bundle-only v1 is intentional; see
/// docs/specs/2026-04-23-chunked-ingest-design.md (Q1).
pub(crate) const FLAT_BUNDLE_MAX: u64 = (harmony_content::bundle::MAX_BUNDLE_ENTRIES as u64)
    * (harmony_content::cid::MAX_PAYLOAD_SIZE as u64);

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
            "file too large ({} bytes). v1 flat-bundle cap is {} bytes (~32 GiB). \
             Support for larger files lands with folder/nested-bundle support.",
            size, FLAT_BUNDLE_MAX
        ));
    }
    if size as usize > harmony_content::cid::MAX_PAYLOAD_SIZE {
        Ok(IngestDispatch::Chunked)
    } else {
        Ok(IngestDispatch::Single)
    }
}

// ── Managed Tauri state ──────────────────────────────────────────────────

struct NodeState {
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
    let (shutdown_tx, thread, publish_tx, fetch_tx, ingest_tx, content_verb_tx, follow_tx, voice_tx, voice_channel_tx, _follow_mgr, _followed_set, _mail_sync) = {
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
        )
    };

    let had_node = shutdown_tx.is_some() || thread.is_some();
    drop(publish_tx); // drop sender so event loop's recv returns None
    drop(fetch_tx);
    drop(ingest_tx);
    drop(content_verb_tx);
    drop(follow_tx);
    drop(voice_tx);
    drop(voice_channel_tx);
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

    // Load the follow list from disk and create the shared followed set.
    let app_data_dir = {
        use tauri::Manager;
        app.path().app_data_dir().map_err(|e| format!("app_data_dir: {e}"))?
    };
    std::fs::create_dir_all(&app_data_dir).map_err(|e| format!("create app_data_dir: {e}"))?;
    let follow_mgr = follows::FollowManager::load(&app_data_dir);
    let followed_set = std::sync::Arc::new(std::sync::Mutex::new(
        follow_mgr.addresses().into_iter().collect::<std::collections::HashSet<String>>(),
    ));
    let content_index = std::sync::Arc::new(std::sync::Mutex::new(
        content_index::ContentIndex::load(&app_data_dir),
    ));
    let followed_set_clone = followed_set.clone();

    // MailManager will be initialized after identity loading (needs owner address).
    // Placeholder — set below once we have our_addr_bytes.
    let mail_mgr: std::sync::Arc<std::sync::Mutex<mail::MailManager>>;

    let our_gen = {
        let mut guard = state.lock().map_err(|e| format!("lock error: {e}"))?;

        // Stop existing node — extract handles under lock, join outside.
        let old_shutdown = guard.shutdown_tx.take();
        let old_thread = guard.thread.take();
        let old_publish = guard.publish_tx.take();
        let old_fetch = guard.fetch_tx.take();
        let old_ingest = guard.ingest_tx.take();
        let old_content_verb = guard.content_verb_tx.take();
        let old_follow = guard.follow_tx.take();
        let old_voice = guard.voice_tx.take();
        let old_voice_channel = guard.voice_channel_tx.take();
        let _old_follow_mgr = guard.follow_mgr.take();
        let _old_followed_set = guard.followed_set.take();
        let _old_mail_mgr = guard.mail_mgr.take();
        let _old_mail_sync = guard.mail_sync.take();
        drop(guard);
        drop(old_publish);
        drop(old_fetch);
        drop(old_ingest);
        drop(old_content_verb);
        drop(old_follow);
        drop(old_voice);
        drop(old_voice_channel);
        stop_handles(old_shutdown, old_thread);

        // ── Identity (serialized by the lock — no concurrent generation race)
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

        let reticulum_identity_bytes =
            Some(zeroize::Zeroizing::new(ed25519.to_private_bytes()));
        drop(ed25519);

        tracing::info!(address = %node_addr, path = %id_path.display(), "identity loaded");

        // Initialize mail manager (needs owner address from identity).
        mail_mgr = std::sync::Arc::new(std::sync::Mutex::new(
            mail::MailManager::load(&app_data_dir.join("mail"), our_addr_bytes),
        ));

        // Construct MailSync now that identity, mail_mgr, and the refresh
        // channel are all available. Owns a clone of fetch_tx (so commands
        // keep their own sender in AppState) and the sole refresh_tx.
        let mail_sync = std::sync::Arc::new(mail_sync::MailSync::new(
            fetch_tx.clone(),
            mail_refresh_tx,
            std::sync::Arc::clone(&mail_mgr),
            app.clone(),
        ));

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

        let ep_clone = endpoint.clone();
        let app_clone = app.clone();
        let mail_mgr_clone = mail_mgr.clone();
        let mail_sync_for_loop = std::sync::Arc::clone(&mail_sync);
        let thread = thread::Builder::new()
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
                        follow_rx,
                        voice_rx,
                        voice_channel_rx,
                        followed_set_clone,
                        mail_mgr_clone,
                        Some(mail_sync_for_loop),
                        mail_refresh_rx,
                    )
                    .await;
                });
            })
            .map_err(|e| format!("failed to spawn runtime thread: {e}"))?;

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
        guard.generation
    };

    // Wait for the event loop to report startup success or failure.
    // stop_node can cancel this by signaling shutdown_tx (now registered).
    let result = match ready_rx.await {
        Ok(Ok(())) => {
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
        Err(_) => {
            Err("runtime thread exited before reporting startup status".to_string())
        }
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
fn stop_node(
    app: AppHandle,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
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
    let payload =
        serde_json::to_vec(&profile).map_err(|e| format!("serialize: {e}"))?;

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
        id: format!("msg-{}-{now_ms}-{:08x}", &node_addr[..8.min(node_addr.len())], rand::random::<u32>()),
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
    let payload =
        serde_json::to_vec(&wire).map_err(|e| format!("serialize: {e}"))?;

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
fn get_node_addr(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<String, String> {
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
    let payload =
        serde_json::to_vec(&wire).map_err(|e| format!("serialize: {e}"))?;

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
        if tx.try_send(event_loop::FollowRequest::Follow { address }).is_err() {
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
        if tx.try_send(event_loop::FollowRequest::Unfollow { address }).is_err() {
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
pub fn parse_content_announcement(key_expr: &str, payload: &[u8]) -> Option<ContentAnnouncementPayload> {
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
/// cache's pinned state snapshot.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentItemWire {
    pub cid: String,              // hex
    pub name: String,
    pub size_bytes: u64,
    pub stored_at: u64,           // ms since epoch
    pub sensitivity: String,      // "private" | "confidential" | "public"
    pub replication_tier: String, // "minimal" | "default" | "durable"
    pub pinned: bool,
    pub licensed: bool,
    pub archived: bool,
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

fn parse_cid_hex(cid_hex: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(cid_hex).map_err(|_| "invalid cid hex".to_string())?;
    <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| "cid must be 32 bytes".to_string())
}

/// Result returned to the frontend after a successful file ingest.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestResult {
    pub cid: String,
    pub file_name: String,
    pub size_bytes: u64,
}

#[tauri::command]
async fn list_content(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<ContentItemWire>, String> {
    // 1. Snapshot pinned CIDs from the runtime cache.
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

    // 2. Join sidecar entries with pinned state and shape the wire.
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let mut entries: Vec<ContentItemWire> = {
        let idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.entries()
            .map(|e| ContentItemWire {
                cid: hex::encode(e.cid),
                name: e.file_name.clone(),
                size_bytes: e.size_bytes,
                stored_at: e.stored_at_ms,
                sensitivity: sensitivity_wire(e.sensitivity).to_string(),
                replication_tier: replication_tier_wire(e.replication_tier).to_string(),
                pinned: pinned_set.contains(&e.cid),
                licensed: e.licensed,
                archived: e.archived,
            })
            .collect()
    };
    // `ContentIndex::entries()` iterates a HashMap, so order is not
    // deterministic. Sort by stored_at descending (newest first) so the
    // File Manager UI sees a stable list across renders.
    entries.sort_by(|a, b| b.stored_at.cmp(&a.stored_at));
    Ok(entries)
}

#[tauri::command]
async fn pin_content(
    cid: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let cid_bytes = parse_cid_hex(&cid)?;
    let verb_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .content_verb_tx
            .clone()
            .ok_or_else(|| "runtime unavailable".to_string())?
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    verb_tx
        .send(event_loop::ContentVerbRequest::Pin {
            cid: cid_bytes,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;
    reply_rx
        .await
        .map_err(|_| "event loop dropped pin request".to_string())?
}

#[tauri::command]
async fn unpin_content(
    cid: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let cid_bytes = parse_cid_hex(&cid)?;
    let verb_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .content_verb_tx
            .clone()
            .ok_or_else(|| "runtime unavailable".to_string())?
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    verb_tx
        .send(event_loop::ContentVerbRequest::Unpin {
            cid: cid_bytes,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;
    reply_rx
        .await
        .map_err(|_| "event loop dropped unpin request".to_string())?
}

#[tauri::command]
async fn burn_content(
    cid: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let cid_bytes = parse_cid_hex(&cid)?;

    // 1. Unpin in the runtime cache so W-TinyLFU can reclaim the RAM.
    let verb_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .content_verb_tx
            .clone()
            .ok_or_else(|| "runtime unavailable".to_string())?
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    verb_tx
        .send(event_loop::ContentVerbRequest::Burn {
            cid: cid_bytes,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;
    reply_rx
        .await
        .map_err(|_| "event loop dropped burn request".to_string())??;

    // 2. Remove the sidecar entry. `Ok(true)` iff the sidecar had the
    //    entry (so the frontend knows whether the burn actually removed
    //    something or was a no-op on an already-unknown CID).
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let removed = {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.remove(&cid_bytes)
    };
    Ok(removed)
}

#[tauri::command]
async fn archive_content(
    cid: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let cid_bytes = parse_cid_hex(&cid)?;
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let flipped = {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.set_archived(&cid_bytes, true)
    };
    Ok(flipped)
}

#[tauri::command]
async fn set_replication_tier(
    cids: Vec<String>,
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
    let mut parsed_cids: Vec<[u8; 32]> = Vec::with_capacity(cids.len());
    for c in &cids {
        parsed_cids.push(parse_cid_hex(c)?);
    }
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let updated = {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.set_replication_tier(&parsed_cids, parsed_tier)
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
    tokio::fs::write(path, &bytes).await.map_err(|e| format!("write failed: {e}"))?;

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
    if meta.len() > harmony_content::cid::MAX_PAYLOAD_SIZE as u64 {
        return Err(format!(
            "file too large ({} bytes, max {})",
            meta.len(),
            harmony_content::cid::MAX_PAYLOAD_SIZE,
        ));
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| format!("read failed: {e}"))?;
    let size_bytes = bytes.len() as u64;

    // 3. Compute CID (single-book, public+durable, blake3 hash).
    let cid = ContentId::for_book(&bytes, ContentFlags::default())
        .map_err(|e| format!("CID error: {e:?}"))?;
    let cid_hex = hex::encode(cid.to_bytes());

    // 4. Store in the runtime via the ingest channel.
    let ingest_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .ingest_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    ingest_tx
        .send(event_loop::IngestRequest {
            cid_hex: cid_hex.clone(),
            data: bytes,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;
    reply_rx
        .await
        .map_err(|_| "event loop dropped ingest request".to_string())??;

    // Record sidecar metadata so `list_content` can surface this entry.
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let stored_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let cid_bytes: [u8; 32] = cid.to_bytes();
    {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.insert(content_index::ContentIndexEntry {
            cid: cid_bytes,
            file_name: file_name.clone(),
            size_bytes,
            stored_at_ms,
            sensitivity: content_index::Sensitivity::Private,
            replication_tier: content_index::ReplicationTier::Default,
            licensed: false,
            archived: false,
        });
    }

    Ok(IngestResult {
        cid: cid_hex,
        file_name,
        size_bytes,
    })
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
        HarmonyMessage, MailMessageType, MessageFlags, Recipient, RecipientType,
        unique_message_id,
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
            let bytes = hex::decode(addr_hex)
                .map_err(|e| format!("bad recipient {addr_hex}: {e}"))?;
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
        guard.mail_mgr.clone().ok_or_else(|| "mail not initialized".to_string())?
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
        guard.mail_mgr.clone().ok_or_else(|| "mail not initialized".to_string())?
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
async fn refresh_mail(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
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
        guard.mail_mgr.clone().ok_or_else(|| "mail not initialized".to_string())?
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
        guard.mail_mgr.clone().ok_or_else(|| "mail not initialized".to_string())?
    };
    let mgr = mgr_arc.lock().map_err(|e| format!("mail lock: {e}"))?;
    Ok(mgr.folder_counts())
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
        let result = parse_capacity(
            "harmony/compute/capacity/node42",
            &make_payload(0x00),
        );
        let update = result.unwrap();
        assert_eq!(update.node_addr, "node42");
        assert!(!update.ready);
    }

    #[test]
    fn parse_capacity_truncated() {
        let result = parse_capacity(
            "harmony/compute/capacity/node1",
            &[0xAA; 10],
        );
        assert!(result.is_none());
    }

    #[test]
    fn parse_capacity_wrong_prefix() {
        let result = parse_capacity(
            "harmony/telemetry/node1/health",
            &make_payload(0x01),
        );
        assert!(result.is_none());
    }

    #[test]
    fn parse_capacity_empty_payload() {
        let result = parse_capacity(
            "harmony/compute/capacity/node1",
            &[],
        );
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
        assert!(json.contains("\"displayName\""), "expected camelCase: {json}");
        assert!(!json.contains("\"display_name\""), "unexpected snake_case: {json}");
        assert!(!json.contains("statusText"), "None field should be skipped: {json}");
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
        assert!(json.contains("\"senderAddress\""), "expected camelCase: {json}");
        assert!(json.contains("\"replyTo\""), "replyTo should be present: {json}");
        assert!(!json.contains("\"sender_address\""), "unexpected snake_case: {json}");
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
        assert!(json.contains("\"creatorAddress\""), "expected camelCase: {json}");
        assert!(json.contains("\"videoCid\""), "expected camelCase: {json}");
        assert!(json.contains("\"reshareOf\""), "reshareOf should be present: {json}");
        assert!(!json.contains("\"creator_address\""), "unexpected snake_case: {json}");
        assert!(!json.contains("\"title\""), "None title should be skipped: {json}");
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
        assert!(json.contains("\"reactorAddress\""), "expected camelCase: {json}");
        assert!(json.contains("\"reactorName\""), "expected camelCase: {json}");
        assert!(!json.contains("\"vine_id\""), "unexpected snake_case: {json}");
        assert!(!json.contains("\"reactor_address\""), "unexpected snake_case: {json}");
        assert!(!json.contains("\"reactor_name\""), "unexpected snake_case: {json}");
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
        let result = parse_content_announcement(
            "harmony/announce/aabbccdd11223344",
            &payload,
        );
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
        assert!(!json.contains("\"size_bytes\""), "unexpected snake_case: {json}");
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
        assert!(err.contains("32 GiB") || err.contains("flat-bundle"),
                "message should explain the cap origin, got: {err}");
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
        // harmony-content limits surfaces here.
        assert_eq!(
            FLAT_BUNDLE_MAX,
            (MAX_BUNDLE_ENTRIES as u64) * (MAX_PAYLOAD_SIZE as u64)
        );
    }
}
