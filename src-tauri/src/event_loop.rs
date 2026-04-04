//! Simplified NodeRuntime event loop for the Tauri desktop client.
//!
//! Adapted from harmony-node's event_loop.rs, stripped down to:
//! - UDP socket (Reticulum mesh broadcast/unicast)
//! - Zenoh session (pub/sub, queryables, content fetch)
//! - 250ms timer tick
//!
//! No disk/archive/S3 persistence, no inference, no iroh tunnels.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use harmony_content::book::MemoryBookStore;
use harmony_runtime::{NodeRuntime, RuntimeAction, RuntimeEvent};
use tauri::{AppHandle, Emitter};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, watch};

/// Well-known UDP port for Reticulum mesh — must match harmony-node default
/// so that all nodes on the LAN broadcast/listen on the same port.
const RETICULUM_UDP_PORT: u16 = 4242;

/// A publish request sent from the Tauri command thread into the event loop.
pub struct PublishRequest {
    pub key_expr: String,
    pub payload: Vec<u8>,
    pub reply: oneshot::Sender<Result<(), String>>,
}

/// A content-fetch request sent from the Tauri command thread into the event loop.
pub struct FetchRequest {
    pub cid_hex: String,
    pub reply: oneshot::Sender<Result<Vec<u8>, String>>,
}

/// A content-ingest request: store local file bytes in the runtime's storage tier.
pub struct IngestRequest {
    pub cid_hex: String,
    pub data: Vec<u8>,
    pub reply: oneshot::Sender<Result<(), String>>,
}

/// Events bridged from spawned Zenoh tasks back to the main select loop.
enum ZenohEvent {
    Query { key_expr: String, payload: Vec<u8> },
    ComputeQuery { key_expr: String, payload: Vec<u8> },
    Subscription { key_expr: String, payload: Vec<u8>, source_zid: Option<String> },
    FetchResponse {
        cid: [u8; 32],
        is_module: bool,
        result: Result<Vec<u8>, String>,
    },
}

/// Run the NodeRuntime event loop as a background task.
///
/// Sends `Ok(())` on `ready_tx` once UDP + Zenoh + startup actions are
/// all initialized, or `Err(msg)` if any startup step fails.
/// Returns when shutdown signal fires.
pub async fn run(
    mut runtime: NodeRuntime<MemoryBookStore>,
    startup_actions: Vec<RuntimeAction>,
    app: AppHandle,
    endpoint: Option<String>,
    ready_tx: oneshot::Sender<Result<(), String>>,
    mut shutdown: watch::Receiver<bool>,
    mut publish_rx: mpsc::Receiver<PublishRequest>,
    mut fetch_rx: mpsc::Receiver<FetchRequest>,
    mut ingest_rx: mpsc::Receiver<IngestRequest>,
) {
    // ── Startup: bind UDP, open Zenoh ────────────────────────────────
    // Each async step is raced against shutdown so stop_node can cancel
    // a slow or stuck zenoh::open without hanging on thread.join().
    macro_rules! cancellable {
        ($fut:expr, $msg:expr) => {
            tokio::select! {
                result = $fut => result,
                _ = shutdown.changed() => {
                    let e = format!("cancelled during {}", $msg);
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            }
        };
    }

    let udp = match cancellable!(
        UdpSocket::bind(format!("0.0.0.0:{RETICULUM_UDP_PORT}")),
        "UDP bind"
    ) {
        Ok(s) => s,
        Err(e) => {
            let e = format!("UDP bind on port {RETICULUM_UDP_PORT} failed: {e}");
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    if let Err(e) = udp.set_broadcast(true) {
        let e = format!("UDP set_broadcast failed: {e}");
        let _ = ready_tx.send(Err(e));
        return;
    }
    let broadcast_addr: SocketAddr = format!("255.255.255.255:{RETICULUM_UDP_PORT}")
        .parse()
        .expect("static broadcast addr");
    tracing::info!(port = RETICULUM_UDP_PORT, "UDP socket bound");

    let mut config = zenoh::Config::default();
    if let Some(ref ep) = endpoint {
        match serde_json::to_string(ep) {
            Ok(ep_json) => {
                if let Err(e) = config.insert_json5("connect/endpoints", &format!("[{ep_json}]")) {
                    let e = format!("zenoh config error: {e}");
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            }
            Err(e) => {
                let e = format!("endpoint serialize error: {e}");
                let _ = ready_tx.send(Err(e));
                return;
            }
        }
    }

    let session = match cancellable!(zenoh::open(config), "zenoh::open") {
        Ok(s) => s,
        Err(e) => {
            let e = format!("zenoh open failed: {e}");
            let _ = ready_tx.send(Err(e.clone()));
            let _ = app.emit(
                "zenoh-status",
                &crate::ZenohStatus {
                    status: "error".to_string(),
                    endpoint: None,
                    error: Some(e),
                },
            );
            return;
        }
    };
    tracing::info!("Zenoh session opened");

    // Own Zenoh session ID — attached to capacity publications so receivers
    // can determine hop distance by comparing against their peers_zid().
    let own_zid = session.zid().to_string();

    // Shared flag: set to true during intentional shutdown so spawned
    // subscriber/queryable tasks don't emit false session-lost errors.
    let closing = Arc::new(AtomicBool::new(false));

    // Channel from spawned Zenoh tasks → main select loop.
    let (zenoh_tx, mut zenoh_rx) = mpsc::channel::<ZenohEvent>(256);

    // ── Process startup actions (declare queryables + subscribers) ────
    for action in startup_actions {
        dispatch_action(
            action,
            &session,
            &zenoh_tx,
            &udp,
            &broadcast_addr,
            &app,
            &closing,
            &own_zid,
        )
        .await;
    }

    // Subscribe to community channel messages for real-time messaging.
    dispatch_action(
        RuntimeAction::Subscribe {
            key_expr: "harmony/community/*/channels/*".to_string(),
        },
        &session,
        &zenoh_tx,
        &udp,
        &broadcast_addr,
        &app,
        &closing,
        &own_zid,
    )
    .await;

    // Subscribe to vine descriptors for the vine feed.
    dispatch_action(
        RuntimeAction::Subscribe {
            key_expr: "harmony/vines/*".to_string(),
        },
        &session,
        &zenoh_tx,
        &udp,
        &broadcast_addr,
        &app,
        &closing,
        &own_zid,
    )
    .await;

    // Subscribe to content availability announcements for the file manager.
    dispatch_action(
        RuntimeAction::Subscribe {
            key_expr: "harmony/announce/*".to_string(),
        },
        &session,
        &zenoh_tx,
        &udp,
        &broadcast_addr,
        &app,
        &closing,
        &own_zid,
    )
    .await;

    // Signal the caller that startup fully succeeded — UDP bound, Zenoh
    // session open, all queryables and subscribers declared.
    let _ = ready_tx.send(Ok(()));

    // ── Timer (250ms = 4 ticks/sec, same as harmony-node) ────────────
    let mut timer = tokio::time::interval(Duration::from_millis(250));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let start = std::time::Instant::now();

    let mut udp_buf = vec![0u8; 65535];

    // Directly connected Zenoh peers — refreshed every 20 timer ticks (~5s).
    // Used to derive hop distance: ZID in this set → hop 1, else → hop 2.
    // Eagerly populated so capacity updates arriving before the first refresh
    // aren't misclassified as hop 2.
    let mut direct_peer_zids: std::collections::HashSet<String> = session
        .info()
        .peers_zid()
        .await
        .into_iter()
        .map(|z| z.to_string())
        .collect();
    let mut peer_refresh_counter: u64 = 0;

    tracing::info!("event loop running");

    loop {
        let mut should_tick = false;

        tokio::select! {
            // ── UDP inbound ──────────────────────────────────────────
            // Intentionally does NOT set should_tick — matches harmony-node.
            // Packets are buffered and processed on the next 250ms timer tick.
            // This ensures tick_count and filter broadcast timers advance at
            // wall-clock rate regardless of packet arrival rate.
            result = udp.recv_from(&mut udp_buf) => {
                match result {
                    Ok((len, _addr)) => {
                        let now = start.elapsed().as_millis() as u64;
                        runtime.push_event(RuntimeEvent::InboundPacket {
                            interface_name: "udp0".to_string(),
                            raw: udp_buf[..len].to_vec(),
                            now,
                        });
                    }
                    Err(e) => {
                        tracing::warn!(err = %e, "UDP recv error");
                    }
                }
            }

            // ── 250ms timer tick ─────────────────────────────────────
            _ = timer.tick() => {
                let now = start.elapsed().as_millis() as u64;
                let unix_now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                runtime.push_event(RuntimeEvent::TimerTick { now, unix_now });
                should_tick = true;

                // Refresh direct peer set every 20 timer ticks (~5 seconds).
                // Driven by timer only (not Zenoh events) to avoid excessive
                // peers_zid() calls under high message traffic.
                peer_refresh_counter += 1;
                if peer_refresh_counter % 20 == 0 {
                    direct_peer_zids = session
                        .info()
                        .peers_zid()
                        .await
                        .into_iter()
                        .map(|z| z.to_string())
                        .collect();
                }
            }

            // ── Zenoh events (from spawned tasks) ────────────────────
            Some(event) = zenoh_rx.recv() => {
                match event {
                    ZenohEvent::Query { key_expr, payload } => {
                        runtime.push_event(RuntimeEvent::QueryReceived {
                            query_id: 0,
                            key_expr,
                            payload,
                        });
                    }
                    ZenohEvent::ComputeQuery { key_expr, payload } => {
                        runtime.push_event(RuntimeEvent::ComputeQuery {
                            query_id: 0,
                            key_expr,
                            payload,
                        });
                    }
                    ZenohEvent::Subscription { key_expr, payload, source_zid } => {
                        let hop_distance = source_zid.as_ref().map(|zid| {
                            if direct_peer_zids.contains(zid) { 1u8 } else { 2u8 }
                        });
                        emit_frontend_event(&app, &key_expr, &payload, hop_distance);
                        runtime.push_event(RuntimeEvent::SubscriptionMessage {
                            key_expr,
                            payload,
                        });
                    }
                    ZenohEvent::FetchResponse { cid, is_module, result } => {
                        if is_module {
                            runtime.push_event(RuntimeEvent::ModuleFetchResponse {
                                cid,
                                result,
                            });
                        } else {
                            runtime.push_event(RuntimeEvent::ContentFetchResponse {
                                cid,
                                result,
                            });
                        }
                    }
                }
                should_tick = true;
            }

            // ── Publish requests from Tauri commands ─────────────────
            Some(req) = publish_rx.recv() => {
                let result = session
                    .put(&req.key_expr, req.payload)
                    .await
                    .map_err(|e| format!("put failed: {e}"));
                let _ = req.reply.send(result);
            }

            // ── Content-fetch requests from Tauri commands ──────────
            Some(req) = fetch_rx.recv() => {
                let prefix = req.cid_hex.get(1..2).unwrap_or("");
                let key_expr = format!("harmony/content/{prefix}/{}", req.cid_hex);
                let session = session.clone();
                tokio::spawn(async move {
                    let result = fetch_via_zenoh(&session, &key_expr).await;
                    let _ = req.reply.send(result);
                });
            }

            // ── Content-ingest requests from Tauri commands ────────
            Some(req) = ingest_rx.recv() => {
                let key_expr = format!("harmony/content/publish/{}", req.cid_hex);
                runtime.push_event(RuntimeEvent::SubscriptionMessage {
                    key_expr,
                    payload: req.data,
                });
                let _ = req.reply.send(Ok(()));
                should_tick = true;
            }

            // ── Shutdown signal ──────────────────────────────────────
            _ = shutdown.changed() => {
                tracing::info!("shutdown signal received");
                break;
            }
        }

        if should_tick {
            let actions = runtime.tick();
            for action in actions {
                dispatch_action(
                    action,
                    &session,
                    &zenoh_tx,
                    &udp,
                    &broadcast_addr,
                    &app,
                    &closing,
                    &own_zid,
                )
                .await;
            }
        }
    }

    // Mark intentional shutdown so spawned tasks don't emit false errors.
    closing.store(true, Ordering::SeqCst);
    let _ = session.close().await;
    tracing::info!("event loop stopped");
}

/// Dispatch a single RuntimeAction to the platform I/O layer.
async fn dispatch_action(
    action: RuntimeAction,
    session: &zenoh::Session,
    zenoh_tx: &mpsc::Sender<ZenohEvent>,
    udp: &UdpSocket,
    broadcast_addr: &SocketAddr,
    app: &AppHandle,
    closing: &Arc<AtomicBool>,
    own_zid: &str,
) {
    match action {
        // ── Network: Reticulum packet send ───────────────────────────
        RuntimeAction::SendOnInterface { raw, weight, .. } => {
            if let Some(w) = weight {
                if rand::random::<f32>() >= w {
                    return;
                }
            }
            let _ = udp.send_to(&raw, broadcast_addr).await;
        }

        // ── Zenoh: publish ───────────────────────────────────────────
        RuntimeAction::Publish { key_expr, payload } => {
            let session = session.clone();
            // Attach our ZenohId to capacity publications so receivers can
            // determine hop distance by comparing against their peers_zid().
            let zid_attachment = if key_expr.starts_with(crate::CAPACITY_PREFIX) {
                Some(own_zid.to_string())
            } else {
                None
            };
            tokio::spawn(async move {
                let mut builder = session.put(&key_expr, payload);
                if let Some(zid) = zid_attachment {
                    builder = builder.attachment(zid.as_bytes());
                }
                if let Err(e) = builder.await {
                    tracing::warn!(%key_expr, err = %e, "zenoh put failed");
                }
            });
        }

        // ── Zenoh: declare queryable ─────────────────────────────────
        RuntimeAction::DeclareQueryable { key_expr } => {
            let is_compute = key_expr.starts_with("harmony/compute/");
            let tx = zenoh_tx.clone();
            let app = app.clone();
            let closing = closing.clone();
            match session.declare_queryable(&key_expr).await {
                Ok(qbl) => {
                    tokio::spawn(async move {
                        while let Ok(query) = qbl.recv_async().await {
                            let qkey = query.key_expr().to_string();
                            let payload = query
                                .payload()
                                .map(|p| p.to_bytes().to_vec())
                                .unwrap_or_default();
                            let ev = if is_compute {
                                ZenohEvent::ComputeQuery {
                                    key_expr: qkey,
                                    payload,
                                }
                            } else {
                                ZenohEvent::Query {
                                    key_expr: qkey,
                                    payload,
                                }
                            };
                            if tx.send(ev).await.is_err() {
                                break;
                            }
                        }
                        // Only emit session-lost if this wasn't an intentional shutdown.
                        if !closing.load(Ordering::SeqCst) {
                            emit_session_lost(&app, "queryable closed unexpectedly");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!(%key_expr, err = %e, "declare_queryable failed");
                }
            }
        }

        // ── Zenoh: subscribe ─────────────────────────────────────────
        RuntimeAction::Subscribe { key_expr } => {
            let tx = zenoh_tx.clone();
            let app = app.clone();
            let closing = closing.clone();
            match session.declare_subscriber(&key_expr).await {
                Ok(sub) => {
                    tokio::spawn(async move {
                        while let Ok(sample) = sub.recv_async().await {
                            let skey = sample.key_expr().to_string();
                            let payload = sample.payload().to_bytes().to_vec();
                            // Extract publisher's ZenohId from attachment (if present).
                            let source_zid = sample
                                .attachment()
                                .and_then(|a| String::from_utf8(a.to_bytes().to_vec()).ok());
                            if tx
                                .send(ZenohEvent::Subscription {
                                    key_expr: skey,
                                    payload,
                                    source_zid,
                                })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        if !closing.load(Ordering::SeqCst) {
                            emit_session_lost(&app, "subscriber closed unexpectedly");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!(%key_expr, err = %e, "declare_subscriber failed");
                }
            }
        }

        // ── Zenoh: fetch content by CID ──────────────────────────────
        RuntimeAction::FetchContent { cid } => {
            let cid_hex = hex::encode(cid);
            // Uses second hex nibble as shard prefix — matches harmony-zenoh fetch_key().
            let prefix = cid_hex.get(1..2).unwrap_or("");
            let key_expr = format!("harmony/content/{prefix}/{cid_hex}");
            let tx = zenoh_tx.clone();
            let session = session.clone();
            tokio::spawn(async move {
                let result = fetch_via_zenoh(&session, &key_expr).await;
                let _ = tx
                    .send(ZenohEvent::FetchResponse {
                        cid,
                        is_module: false,
                        result,
                    })
                    .await;
            });
        }

        RuntimeAction::FetchModule { cid } => {
            let cid_hex = hex::encode(cid);
            let prefix = cid_hex.get(1..2).unwrap_or("");
            let key_expr = format!("harmony/content/{prefix}/{cid_hex}");
            let tx = zenoh_tx.clone();
            let session = session.clone();
            tokio::spawn(async move {
                let result = fetch_via_zenoh(&session, &key_expr).await;
                let _ = tx
                    .send(ZenohEvent::FetchResponse {
                        cid,
                        is_module: true,
                        result,
                    })
                    .await;
            });
        }

        // ── SendReply: stub (same as harmony-node) ───────────────────
        RuntimeAction::SendReply { .. } => {
            tracing::trace!("SendReply not yet implemented in client");
        }

        // ── Actions not applicable to desktop client ─────────────────
        _ => {}
    }
}

/// Fetch content via Zenoh get() with a 30s timeout.
async fn fetch_via_zenoh(session: &zenoh::Session, key_expr: &str) -> Result<Vec<u8>, String> {
    let replies = session
        .get(key_expr)
        .await
        .map_err(|e| format!("zenoh get error: {e}"))?;

    let deadline = Duration::from_secs(30);
    tokio::time::timeout(deadline, async {
        while let Ok(reply) = replies.recv_async().await {
            match reply.result() {
                Ok(sample) => {
                    return Ok(sample.payload().to_bytes().to_vec());
                }
                Err(err) => {
                    let msg = String::from_utf8_lossy(&err.payload().to_bytes()).into_owned();
                    tracing::warn!(%key_expr, err = %msg, "zenoh fetch reply error");
                }
            }
        }
        Err(format!("no successful reply for '{key_expr}'"))
    })
    .await
    .unwrap_or_else(|_| Err(format!("fetch '{key_expr}' timed out after 30s")))
}

/// Emit zenoh-status error when a Zenoh session appears to have been lost.
fn emit_session_lost(app: &AppHandle, reason: &str) {
    let _ = app.emit(
        "zenoh-status",
        &crate::ZenohStatus {
            status: "error".to_string(),
            endpoint: None,
            error: Some(format!("session lost: {reason}")),
        },
    );
}

/// Bridge Zenoh subscription messages to Tauri frontend events.
fn emit_frontend_event(app: &AppHandle, key_expr: &str, payload: &[u8], hop_distance: Option<u8>) {
    if key_expr.starts_with("harmony/compute/capacity/") {
        if let Some(mut update) = crate::parse_capacity(key_expr, payload) {
            update.hop_distance = hop_distance;
            let _ = app.emit("capacity-update", &update);
        }
    } else if key_expr.starts_with("harmony/profile/") {
        if let Ok(profile) = serde_json::from_slice::<crate::ProfilePayload>(payload) {
            let _ = app.emit("profile-update", &profile);
        }
    } else if key_expr.starts_with("harmony/community/") {
        if let Ok(msg) = serde_json::from_slice::<crate::ChannelMessagePayload>(payload) {
            let _ = app.emit("message-received", &msg);
        }
    } else if key_expr.starts_with("harmony/vines/") {
        if let Ok(vine) = serde_json::from_slice::<crate::VineDescriptorPayload>(payload) {
            let _ = app.emit("vine-received", &vine);
        }
    } else if key_expr.starts_with("harmony/announce/") {
        if let Some(announcement) = crate::parse_content_announcement(key_expr, payload) {
            let _ = app.emit("content-announced", &announcement);
        }
    } else if key_expr.contains("/telemetry/") {
        if let Some(event) = crate::parse_telemetry(payload) {
            let _ = app.emit("telemetry-event", &event);
        }
    }
}
