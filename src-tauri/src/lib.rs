use std::sync::Mutex;

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::task::JoinHandle;

struct ZenohState {
    session: Option<zenoh::Session>,
    task: Option<JoinHandle<()>>,
}

impl Default for ZenohState {
    fn default() -> Self {
        Self {
            session: None,
            task: None,
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

async fn disconnect_inner(app: &AppHandle, state: &Mutex<ZenohState>) {
    let task = {
        let mut guard = match state.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        // Drop session → subscriber's recv_async() returns Err → task exits cleanly
        guard.session.take();
        guard.task.take()
    }; // Guard dropped here, before the await
    if let Some(task) = task {
        let _ = task.await;
    }
    let _ = app.emit(
        "zenoh-status",
        &ZenohStatus {
            status: "disconnected".to_string(),
            endpoint: None,
            error: None,
        },
    );
}

#[tauri::command]
async fn connect_zenoh(
    endpoint: String,
    app: AppHandle,
    state: tauri::State<'_, Mutex<ZenohState>>,
) -> Result<(), String> {
    // Disconnect if already connected
    disconnect_inner(&app, &state).await;

    // Build zenoh config
    let mut config = zenoh::Config::default();
    config
        .insert_json5("connect/endpoints", &format!("[\"{}\"]", endpoint))
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

    // Subscribe to capacity advertisements
    let subscriber = session
        .declare_subscriber("harmony/compute/capacity/*")
        .await
        .map_err(|e| {
            let msg = format!("subscribe failed: {e}");
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

    // Store session BEFORE spawning task to prevent race with disconnect.
    // If disconnect_zenoh is called between spawn and store, it would miss
    // the session and leave an orphaned connection.
    {
        let mut guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
        guard.session = Some(session);
    }

    // Spawn subscriber task
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
                Err(_) => break, // Session closed, exit cleanly
            }
        }
    });

    // Store task handle (session already stored above)
    {
        let mut guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
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
    disconnect_inner(&app, &state).await;
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
