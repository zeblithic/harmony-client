use std::sync::Mutex;
use std::thread;

use harmony_compute::InstructionBudget;
use harmony_content::book::MemoryBookStore;
use harmony_content::storage_tier::{ContentPolicy, FilterBroadcastConfig, StorageBudget};
use harmony_runtime::{NodeConfig, NodeRuntime};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

mod event_loop;
mod identity;

// ── Managed Tauri state ──────────────────────────────────────────────────

#[derive(Default)]
struct NodeState {
    /// Background thread running the event loop (NodeRuntime is !Send).
    thread: Option<thread::JoinHandle<()>>,
    /// Send `true` to shut down the event loop.
    shutdown_tx: Option<tokio::sync::watch::Sender<bool>>,
    /// Channel for routing publish requests through the event loop's session.
    publish_tx: Option<tokio::sync::mpsc::Sender<event_loop::PublishRequest>>,
    /// Monotonic connection generation (prevents stale stop_node races).
    generation: u64,
    /// Hex-encoded node address (set on startup, used to stamp outgoing messages).
    node_addr: String,
}

// ── Data types (shared with frontend via Tauri events) ───────────────────

/// Parsed capacity advertisement from a harmony-node.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapacityUpdate {
    pub node_addr: String,
    pub model_cid: String,
    pub ready: bool,
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
    let (shutdown_tx, thread, publish_tx) = {
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
        (guard.shutdown_tx.take(), guard.thread.take(), guard.publish_tx.take())
    };

    let had_node = shutdown_tx.is_some() || thread.is_some();
    drop(publish_tx); // drop sender so event loop's recv returns None
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

    let our_gen = {
        let mut guard = state.lock().map_err(|e| format!("lock error: {e}"))?;

        // Stop existing node — extract handles under lock, join outside.
        let old_shutdown = guard.shutdown_tx.take();
        let old_thread = guard.thread.take();
        let old_publish = guard.publish_tx.take();
        drop(guard);
        drop(old_publish);
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
            s3_enabled: false,
        };

        // Re-acquire lock and atomically register the new node.
        // Handles are stored BEFORE awaiting ready_rx so stop_node can
        // cancel an in-flight startup via shutdown_tx.
        let mut guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
        guard.generation += 1;

        let ep_clone = endpoint.clone();
        let app_clone = app.clone();
        let thread = thread::Builder::new()
            .name("harmony-runtime".to_string())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
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
                    )
                    .await;
                });
            })
            .map_err(|e| format!("failed to spawn runtime thread: {e}"))?;

        guard.thread = Some(thread);
        guard.shutdown_tx = Some(shutdown_tx);
        guard.publish_tx = Some(publish_tx);
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

#[tauri::command]
fn list_vine_videos() -> Vec<VineVideoDto> {
    // Future: return cached/persisted vines. Real data flows via vine-received events.
    Vec::new()
}

#[tauri::command]
fn follow_vine_creator(address: String) -> bool {
    // Future: subscribe to specific creator's vine key expression.
    let _ = address;
    true
}

#[tauri::command]
fn unfollow_vine_creator(address: String) -> bool {
    // Future: unsubscribe from specific creator's vine key expression.
    let _ = address;
    true
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

#[tauri::command]
fn list_content() -> Vec<serde_json::Value> {
    // Future (bead fkz): query runtime's cache + disk index via query channel.
    Vec::new()
}

#[tauri::command]
fn pin_content(cid: String) -> Result<bool, String> {
    // Future (bead fkz): send pin request to runtime via query channel.
    let _ = cid;
    Ok(true)
}

#[tauri::command]
fn unpin_content(cid: String) -> Result<bool, String> {
    // Future (bead fkz): send unpin request to runtime via query channel.
    let _ = cid;
    Ok(true)
}

#[tauri::command]
fn burn_content(cid: String) -> Result<bool, String> {
    // Future (bead fkz): send delete request to runtime via query channel.
    let _ = cid;
    Ok(true)
}

// ── App entry point ──────────────────────────────────────────────────────

pub fn run() {
    tauri::Builder::default()
        .manage(Mutex::new(NodeState::default()))
        .invoke_handler(tauri::generate_handler![
            list_vine_videos,
            follow_vine_creator,
            unfollow_vine_creator,
            mark_vine_viewed,
            publish_vine,
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
