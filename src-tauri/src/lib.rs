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

    {
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
    }

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

    // On startup failure, clean up stale handles so NodeState accurately
    // reflects that no node is running.
    if result.is_err() {
        let _ = stop_inner(&state, None);
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

// ── Vine stubs (unchanged) ───────────────────────────────────────────────

/// Vine video descriptor returned to the frontend.
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

#[tauri::command]
fn list_vine_videos() -> Vec<VineVideoDto> {
    Vec::new()
}

#[tauri::command]
fn follow_vine_creator(_address: String) -> bool {
    true
}

#[tauri::command]
fn unfollow_vine_creator(_address: String) -> bool {
    true
}

#[tauri::command]
fn mark_vine_viewed(_vine_id: String) -> bool {
    true
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
            start_node,
            stop_node,
            connect_zenoh,
            disconnect_zenoh,
            publish_profile,
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
}
