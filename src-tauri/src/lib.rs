use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::task::JoinHandle;

struct ZenohState {
    session: Option<zenoh::Session>,
    task: Option<JoinHandle<()>>,
    /// Set to true by disconnect_inner before closing the session.
    /// Each connection gets a fresh Arc; the old task keeps its copy.
    closing: Arc<AtomicBool>,
    /// Monotonic connection generation. Incremented on each connect.
    /// disconnect_zenoh captures the generation at call time and only
    /// tears down the session if the generation still matches, preventing
    /// a stale disconnect from closing a newer session.
    generation: Arc<AtomicU64>,
}

impl Default for ZenohState {
    fn default() -> Self {
        Self {
            session: None,
            task: None,
            closing: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }
}

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

const CAPACITY_PREFIX: &str = "harmony/compute/capacity/";

fn parse_capacity(key_expr: &str, payload: &[u8]) -> Option<CapacityUpdate> {
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

/// Disconnect the current session. If `expected_gen` is `Some(n)`, only
/// disconnect if the current generation matches — prevents a stale
/// disconnect from tearing down a newer session. Pass `None` to
/// unconditionally disconnect (used by connect_zenoh's internal teardown).
async fn disconnect_inner(
    app: &AppHandle,
    state: &Mutex<ZenohState>,
    expected_gen: Option<u64>,
) {
    let (session, task) = {
        let mut guard = match state.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        // If a generation was specified, only disconnect if it matches.
        // This prevents a stale disconnect_zenoh from tearing down a
        // newer session that was established after the disconnect was called.
        if let Some(gen) = expected_gen {
            if guard.generation.load(Ordering::SeqCst) != gen {
                return; // Stale disconnect — newer session exists
            }
        }
        guard.closing.store(true, Ordering::SeqCst);
        (guard.session.take(), guard.task.take())
    };

    // Explicitly close the session so all subscribers receive Err.
    let had_session = session.is_some();
    if let Some(session) = session {
        let _ = session.close().await;
    }
    if let Some(task) = task {
        let _ = task.await;
    }
    // Only emit disconnected if there was actually a session to disconnect.
    // Prevents spurious disconnected event during connect_zenoh's teardown
    // of a previous connection, which would flicker the UI.
    if had_session {
        let _ = app.emit(
            "zenoh-status",
            &ZenohStatus {
                status: "disconnected".to_string(),
                endpoint: None,
                error: None,
            },
        );
    }
}

#[tauri::command]
async fn connect_zenoh(
    endpoint: String,
    app: AppHandle,
    state: tauri::State<'_, Mutex<ZenohState>>,
) -> Result<(), String> {
    // Disconnect if already connected, then allocate a fresh closing flag
    // for this connection. Each connection gets its own Arc<AtomicBool> so
    // the old subscriber task's flag is independent from the new one.
    disconnect_inner(&app, &state, None).await;
    let closing = Arc::new(AtomicBool::new(false));
    {
        let mut guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
        guard.closing = closing.clone();
        // Increment generation so any stale disconnect_zenoh commands
        // (from before this connect) will see a mismatch and no-op.
        guard.generation.fetch_add(1, Ordering::SeqCst);
    }

    // Build zenoh config — use serde_json to safely serialize the endpoint
    let mut config = zenoh::Config::default();
    let endpoint_json = serde_json::to_string(&endpoint)
        .map_err(|e| format!("endpoint serialize error: {e}"))?;
    config
        .insert_json5("connect/endpoints", &format!("[{}]", endpoint_json))
        .map_err(|e| format!("config error: {e}"))?;

    // Open session
    let session = zenoh::open(config).await.map_err(|e| {
        let msg = format!("zenoh open failed: {e}");
        let _ = app.emit(
            "zenoh-status",
            &ZenohStatus {
                status: "error".to_string(),
                endpoint: Some(endpoint.clone()),
                error: Some(msg.clone()),
            },
        );
        msg
    })?;

    // Check if disconnect was called while we were awaiting zenoh::open.
    // Uses our local `closing` Arc — not re-read from state, which may
    // have been replaced by a subsequent connect call.
    let was_cancelled = closing.load(Ordering::SeqCst);
    if was_cancelled {
        let _ = session.close().await;
        return Ok(());
    }

    // Subscribe to capacity advertisements.
    // Close session explicitly on error — Drop alone can't do the full async close.
    let subscriber = match session.declare_subscriber("harmony/compute/capacity/*").await {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("subscribe failed: {e}");
            let _ = app.emit(
                "zenoh-status",
                &ZenohStatus {
                    status: "error".to_string(),
                    endpoint: Some(endpoint.clone()),
                    error: Some(msg.clone()),
                },
            );
            let _ = session.close().await;
            return Err(msg);
        }
    };

    // Check again after declare_subscriber await
    let was_cancelled = closing.load(Ordering::SeqCst);
    if was_cancelled {
        let _ = session.close().await;
        return Ok(());
    }

    // Store session, spawn task, and store task handle in a single lock scope.
    // The block ensures the MutexGuard is dropped before any subsequent await.
    {
        let mut guard = match state.lock() {
            Ok(g) => g,
            Err(e) => {
                // Can't await session.close() here (guard would cross await),
                // but the session will be dropped which is acceptable for a
                // poisoned-mutex edge case.
                return Err(format!("lock error: {e}"));
            }
        };
        guard.session = Some(session);
        let task_closing = closing.clone();

        let app_handle = app.clone();
        let task = tokio::spawn(async move {
            loop {
                match subscriber.recv_async().await {
                    Ok(sample) => {
                        let key = sample.key_expr().as_str();
                        let payload = sample.payload().to_bytes();
                        if let Some(update) = parse_capacity(key, &payload) {
                            let _ = app_handle.emit("capacity-update", &update);
                        }
                    }
                    Err(e) => {
                        if !task_closing.load(Ordering::SeqCst) {
                            let _ = app_handle.emit(
                                "zenoh-status",
                                &ZenohStatus {
                                    status: "error".to_string(),
                                    endpoint: None,
                                    error: Some(format!("session lost: {e}")),
                                },
                            );
                        }
                        break;
                    }
                }
            }
        });
        guard.task = Some(task);
    }

    let _ = app.emit(
        "zenoh-status",
        &ZenohStatus {
            status: "connected".to_string(),
            endpoint: Some(endpoint),
            error: None,
        },
    );

    Ok(())
}

#[tauri::command]
async fn disconnect_zenoh(
    app: AppHandle,
    state: tauri::State<'_, Mutex<ZenohState>>,
) -> Result<(), String> {
    // Capture the current generation — only disconnect if it still matches
    // when disconnect_inner runs. If a connect_zenoh happened between the
    // user clicking Disconnect and this command executing, the generation
    // will have changed and this becomes a no-op.
    let gen = {
        let guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
        guard.generation.load(Ordering::SeqCst)
    };
    disconnect_inner(&app, &state, Some(gen)).await;
    Ok(())
}

/// Vine video descriptor returned to the frontend.
///
/// Mirrors `harmony_content::vine::VineDescriptor` but uses hex-encoded
/// strings for CIDs and addresses (easier to consume from TypeScript).
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
    // Stub — returns empty until real content transport is wired up.
    // The frontend uses mock data in the meantime.
    Vec::new()
}

#[tauri::command]
fn follow_vine_creator(_address: String) -> bool {
    // Stub — will subscribe to vine announce key expression via zenoh.
    true
}

#[tauri::command]
fn unfollow_vine_creator(_address: String) -> bool {
    // Stub — will unsubscribe from vine announce key expression.
    true
}

#[tauri::command]
fn mark_vine_viewed(_vine_id: String) -> bool {
    // Stub — will update viewed state in VineFeed state machine.
    true
}

pub fn run() {
    tauri::Builder::default()
        .manage(Mutex::new(ZenohState::default()))
        .invoke_handler(tauri::generate_handler![
            list_vine_videos,
            follow_vine_creator,
            unfollow_vine_creator,
            mark_vine_viewed,
            connect_zenoh,
            disconnect_zenoh,
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
}
