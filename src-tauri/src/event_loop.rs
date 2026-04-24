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
use tauri::{AppHandle, Emitter, Runtime};
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

/// Content-verb requests sent from Tauri commands into the event loop.
///
/// The event loop mutates the runtime's cache (pin/unpin) and snapshots
/// pinned state in response. Sidecar-only mutations (archive, replication
/// tier) are NOT routed through this channel — they run directly against
/// the `Arc<Mutex<ContentIndex>>` from the Tauri command handler.
pub enum ContentVerbRequest {
    Pin {
        cid: [u8; 32],
        reply: oneshot::Sender<Result<bool, String>>,
    },
    Unpin {
        cid: [u8; 32],
        reply: oneshot::Sender<Result<bool, String>>,
    },
    Burn {
        cid: [u8; 32],
        reply: oneshot::Sender<Result<bool, String>>,
    },
    /// Snapshot the set of currently-pinned CIDs in the runtime cache.
    /// Used by `list_content` to fill the `pinned` field per entry.
    PinnedSet {
        reply: oneshot::Sender<std::collections::HashSet<[u8; 32]>>,
    },
    /// ZEB-158 slice 1: read raw bytes for a CID out of the runtime
    /// cache. Used by `list_content(folder_cid=Some)` in src-tauri/src/lib.rs
    /// to parse a folder bundle's manifest without needing direct access
    /// to the `!Send` NodeRuntime.
    ///
    /// Returns `None` if the CID is not admitted in the cache. Callers
    /// surface "folder not in cache" diagnostics instead of errors so a
    /// legitimately-evicted folder is distinguishable from a malformed
    /// request.
    ReadBytes {
        cid: [u8; 32],
        reply: oneshot::Sender<Option<Vec<u8>>>,
    },
}

/// A follow/unfollow request sent from the Tauri command thread into the event loop.
pub enum FollowRequest {
    Follow { address: String },
    Unfollow { address: String },
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
pub async fn run<R: Runtime>(
    mut runtime: NodeRuntime<MemoryBookStore>,
    startup_actions: Vec<RuntimeAction>,
    app: AppHandle<R>,
    endpoint: Option<String>,
    ready_tx: oneshot::Sender<Result<(), String>>,
    mut shutdown: watch::Receiver<bool>,
    mut publish_rx: mpsc::Receiver<PublishRequest>,
    mut fetch_rx: mpsc::Receiver<FetchRequest>,
    mut ingest_rx: mpsc::Receiver<IngestRequest>,
    mut content_verb_rx: mpsc::Receiver<ContentVerbRequest>,
    mut follow_rx: mpsc::Receiver<FollowRequest>,
    mut voice_rx: mpsc::Receiver<crate::voice::VoiceOutbound>,
    mut voice_channel_rx: mpsc::Receiver<crate::voice::VoiceChannelRequest>,
    followed_set: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    mail_mgr: std::sync::Arc<std::sync::Mutex<crate::mail::MailManager>>,
    mail_sync: Option<Arc<crate::mail_sync::MailSync<R>>>,
    mut refresh_rx: mpsc::Receiver<crate::mail_sync::RefreshRequest>,
    mut pin_intent: std::collections::HashSet<[u8; 32]>,
    fetch_completion_tx: mpsc::Sender<[u8; 32]>,
    mut fetch_completion_rx: mpsc::Receiver<[u8; 32]>,
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

    // Subscribe to vine reactions (likes/unlikes).
    dispatch_action(
        RuntimeAction::Subscribe {
            key_expr: "harmony/vines/*/reactions/**".to_string(),
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

    // Note: per-creator Zenoh subscriptions are not used yet because the
    // publish path (harmony/vines/{addr}) does not include /announce/.
    // Once harmony-node adopts the full keyspace protocol
    // (harmony/vines/{addr}/announce/{cid}), per-creator subscriptions can
    // be added here for write-side filtering. For now, the wildcard
    // subscription above catches all vines and we route by followed_set.

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

    // Subscribe to inbound mail for this node's address, plus the /root
    // pointer that the Phase 2 MailSync walker consumes. Both keys are
    // hoisted to the loop scope so the emit_frontend_event filter can
    // dispatch exact-match by string comparison.
    //
    // Poison fallback: empty strings, guarded with `!key.is_empty()` in
    // the filter. Subscriptions are skipped rather than panicking —
    // mail functionality degrades but the rest of the node stays alive.
    let (own_mail_key, own_root_key) = match mail_mgr.lock() {
        Ok(g) => {
            let own_hex = g.owner_address_hex();
            drop(g);
            (
                format!("harmony/mail/v1/{own_hex}"),
                format!("harmony/mail/v1/{own_hex}/root"),
            )
        }
        Err(e) => {
            tracing::error!(error = %e, "mail_mgr mutex poisoned at startup; mail subs disabled");
            (String::new(), String::new())
        }
    };
    if !own_mail_key.is_empty() {
        dispatch_action(
            RuntimeAction::Subscribe {
                key_expr: own_mail_key.clone(),
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
        dispatch_action(
            RuntimeAction::Subscribe {
                key_expr: own_root_key.clone(),
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
    }

    // Signal the caller that startup fully succeeded — UDP bound, Zenoh
    // session open, all queryables and subscribers declared.
    let _ = ready_tx.send(Ok(()));

    // Phase 2: cold-start root query. Pulls current root via Zenoh `get` in
    // case the gateway last published before this client subscribed. 10s
    // budget — on failure or timeout, the normal publish-on-next-write
    // flow still delivers eventually.
    if let Some(ref sync) = mail_sync {
        if !own_root_key.is_empty() {
            let sync = Arc::clone(sync);
            let session_clone = session.clone();
            let key = own_root_key.clone();
            tokio::spawn(async move {
                match query_mail_root(&session_clone, &key, "startup").await {
                    Ok(Some(payload)) => {
                        sync.handle_startup_query_reply(Some(&payload)).await
                    }
                    Ok(None) => {
                        tracing::warn!(
                            "startup root query: no responder — live push will catch up on next gateway publish"
                        );
                        sync.report_query_error(
                            "no gateway responded to startup query".to_string(),
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "startup root query failed");
                        sync.report_query_error(format!("startup query failed: {e}"));
                    }
                }
            });
        }
    }

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

    // Dynamic voice channel subscriptions — keyed by channel_id.
    let mut voice_subs: std::collections::HashMap<String, tokio::task::JoinHandle<()>> =
        std::collections::HashMap::new();

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
                        emit_frontend_event(
                            &app,
                            &key_expr,
                            &payload,
                            hop_distance,
                            &followed_set,
                            &mail_mgr,
                            &own_mail_key,
                            &own_root_key,
                            mail_sync.as_ref(),
                        );
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
                let session = session.clone();
                let cid_hex = req.cid_hex;
                // ZEB-155: clone the completion sender so the spawned
                // task can notify the main loop after a successful fetch.
                let completion_tx = fetch_completion_tx.clone();
                tokio::spawn(async move {
                    // Parse hex → 32-byte CID. Reply with an error if malformed.
                    let cid_bytes = match hex::decode(&cid_hex)
                        .ok()
                        .and_then(|b| <[u8; 32]>::try_from(b).ok())
                    {
                        Some(b) => b,
                        None => {
                            let _ = req.reply.send(Err(format!("invalid CID hex: {cid_hex}")));
                            return;
                        }
                    };
                    let root = ContentId::from_bytes(cid_bytes);

                    // Closure that does one Zenoh GET for a single CID.
                    let fetch_one = move |cid: ContentId| {
                        let session = session.clone();
                        async move {
                            let cid_hex = hex::encode(cid.to_bytes());
                            let prefix = cid_hex.get(1..2).unwrap_or("");
                            let key = format!("harmony/content/{prefix}/{cid_hex}");
                            fetch_via_zenoh(&session, &key).await
                        }
                    };

                    let result = fetch_recursive(fetch_one, root).await;
                    // ZEB-155: reply to the fetch caller FIRST so a full
                    // completion channel never delays the fetch reply.
                    // Then best-effort-notify via try_send. If the
                    // completion channel is full (rare — main loop drain
                    // is O(1) per select pass), we lose this chance to
                    // auto-repin; the next user action or next start_node
                    // reconverges. try_send also returns Err on closed,
                    // which is fine (event loop shutting down).
                    let is_ok = result.is_ok();
                    let _ = req.reply.send(result);
                    if is_ok {
                        let _ = completion_tx.try_send(cid_bytes);
                    }
                });
            }

            // ── Manual mail refresh from MailSync::refresh_now ──────
            Some(reply_tx) = refresh_rx.recv() => {
                if own_root_key.is_empty() {
                    let _ = reply_tx.send(Err("own_root_key unavailable".to_string()));
                } else {
                    let session_clone = session.clone();
                    let key = own_root_key.clone();
                    tokio::spawn(async move {
                        let result = query_mail_root(&session_clone, &key, "refresh").await;
                        let _ = reply_tx.send(result);
                    });
                }
            }

            // ── Content-ingest requests from Tauri commands ────────
            Some(req) = ingest_rx.recv() => {
                // Validate the CID hex decodes to exactly 32 bytes — this is
                // the only precondition for parse_subscription_event to route
                // the message into StorageTierEvent::PublishContent.
                let cid_ok = hex::decode(&req.cid_hex)
                    .ok()
                    .and_then(|b| <[u8; 32]>::try_from(b).ok())
                    .is_some();
                if !cid_ok {
                    let _ = req.reply.send(Err(format!("invalid CID hex: {}", req.cid_hex)));
                } else {
                    let key_expr = format!("harmony/content/publish/{}", req.cid_hex);
                    runtime.push_event(RuntimeEvent::SubscriptionMessage {
                        key_expr,
                        payload: req.data,
                    });
                    // Tick immediately so content is committed before replying.
                    for action in runtime.tick() {
                        dispatch_action(
                            action, &session, &zenoh_tx, &udp,
                            &broadcast_addr, &app, &closing, &own_zid,
                        ).await;
                    }
                    let _ = req.reply.send(Ok(()));
                }
            }

            // ── Content-verb requests (pin/unpin/burn/snapshot) ────
            Some(req) = content_verb_rx.recv() => {
                use harmony_content::cid::ContentId;
                match req {
                    ContentVerbRequest::Pin { cid, reply } => {
                        // ZEB-155: record intent in the event-loop cache so
                        // fetch-completion can auto-repin after a resurrect.
                        //
                        // This may contain CIDs not in the sidecar (e.g. a
                        // pin on a cached DM attachment for which no
                        // sidecar entry exists). That drift self-heals on
                        // the next start_node, which rebuilds pin_intent
                        // from the sidecar — sidecar remains authoritative.
                        pin_intent.insert(cid);
                        let root = ContentId::from_bytes(cid);
                        let all = collect_descendants(runtime.storage_tier().cache(), root);
                        let mut any_failed = false;
                        for id in all {
                            if !runtime.pin_content(id) {
                                any_failed = true;
                            }
                        }
                        let _ = reply.send(Ok(!any_failed));
                    }
                    ContentVerbRequest::Unpin { cid, reply } => {
                        // ZEB-155: clear intent so a later fetch doesn't re-pin.
                        pin_intent.remove(&cid);
                        let root = ContentId::from_bytes(cid);
                        let all = collect_descendants(runtime.storage_tier().cache(), root);
                        for id in all {
                            runtime.unpin_content(&id);
                        }
                        let _ = reply.send(Ok(true));
                    }
                    ContentVerbRequest::Burn { cid, reply } => {
                        // Burn on a RAM-only client cascades the runtime-side
                        // unpin; the sidecar-removal side of burn continues to
                        // happen in the Tauri command handler.
                        // ZEB-155: also drop any persisted intent (the Tauri
                        // command removes the sidecar entry, but this keeps
                        // the in-memory set consistent if the orders diverge).
                        pin_intent.remove(&cid);
                        let root = ContentId::from_bytes(cid);
                        let all = collect_descendants(runtime.storage_tier().cache(), root);
                        for id in all {
                            runtime.unpin_content(&id);
                        }
                        let _ = reply.send(Ok(true));
                    }
                    ContentVerbRequest::PinnedSet { reply } => {
                        let cache = runtime.storage_tier().cache();
                        let pinned: std::collections::HashSet<[u8; 32]> = cache
                            .iter_admitted()
                            .filter(|id| cache.is_pinned(id))
                            .map(|id| id.to_bytes())
                            .collect();
                        let _ = reply.send(pinned);
                    }
                    ContentVerbRequest::ReadBytes { cid, reply } => {
                        let id = ContentId::from_bytes(cid);
                        let bytes = runtime.storage_tier().cache().get(&id).map(|b| b.to_vec());
                        let _ = reply.send(bytes);
                    }
                }
            }

            // ── Fetch-completion replay hook (ZEB-155) ─────────────
            // Spawned fetch tasks send on fetch_completion_tx after
            // fetch_recursive returns Ok. If pin_intent contains the
            // root, re-run the pin cascade now that bytes are resident.
            //
            // NOTE: today's fetch_rx path does NOT admit fetched bytes
            // into ContentStore — it returns them to the Tauri caller.
            // So in production this cascade walks an empty cache for the
            // fetched CID and pin_content is a no-op. The hook is
            // architecturally correct and test-proven in isolation (see
            // fetch_complete_arm_pins_root_in_intent), but its practical
            // reach depends on ZEB-159, which will wire fetch success
            // to cache admission.
            Some(root_bytes) = fetch_completion_rx.recv() => {
                if pin_intent.contains(&root_bytes) {
                    let root = ContentId::from_bytes(root_bytes);
                    let all = collect_descendants(runtime.storage_tier().cache(), root);
                    for id in all {
                        runtime.pin_content(id);
                    }
                }
            }

            // Follow/unfollow updates are applied to followed_set directly
            // by the Tauri command handlers. When per-creator Zenoh
            // subscriptions are added (once the publish path includes
            // /announce/), the follow_rx channel will drive Subscribe/
            // Unsubscribe actions here.
            Some(_req) = follow_rx.recv() => {}

            // ── Voice frame relay (frontend → Zenoh) ────────────────
            // Await directly instead of spawning per-frame tasks — preserves
            // ordering and applies natural backpressure from Zenoh.
            Some(voice) = voice_rx.recv() => {
                if voice.frame.len() >= 23 {
                    let node_addr = hex::encode(&voice.frame[7..23]);
                    let key_expr = format!("harmony/voice/{}/{}", voice.channel_id, node_addr);
                    if let Err(e) = session.put(&key_expr, voice.frame).await {
                        tracing::warn!(%key_expr, err = %e, "voice publish failed");
                    }
                }
            }

            // ── Voice channel join/leave ────────────────────────────
            Some(req) = voice_channel_rx.recv() => {
                match req {
                    crate::voice::VoiceChannelRequest::Join { channel_id } => {
                        let key_expr = format!("harmony/voice/{}/*", channel_id);
                        let app = app.clone();
                        let closing = closing.clone();
                        match session.declare_subscriber(&key_expr).await {
                            Ok(sub) => {
                                let handle = tokio::spawn(async move {
                                    while let Ok(sample) = sub.recv_async().await {
                                        let payload = sample.payload().to_bytes().to_vec();
                                        let _ = app.emit("voice-frame-received", serde_json::json!({
                                            "frameBytes": payload,
                                        }));
                                    }
                                    if !closing.load(std::sync::atomic::Ordering::SeqCst) {
                                        tracing::warn!("voice subscriber closed unexpectedly");
                                    }
                                });
                                if let Some(old) = voice_subs.insert(channel_id, handle) {
                                    old.abort();
                                }
                            }
                            Err(e) => {
                                tracing::error!(%key_expr, err = %e, "voice subscribe failed");
                            }
                        }
                    }
                    crate::voice::VoiceChannelRequest::Leave { channel_id } => {
                        if let Some(handle) = voice_subs.remove(&channel_id) {
                            handle.abort();
                        }
                    }
                }
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
    for (_, handle) in voice_subs.drain() {
        handle.abort();
    }
    let _ = session.close().await;
    tracing::info!("event loop stopped");
}

/// Dispatch a single RuntimeAction to the platform I/O layer.
async fn dispatch_action<R: Runtime>(
    action: RuntimeAction,
    session: &zenoh::Session,
    zenoh_tx: &mpsc::Sender<ZenohEvent>,
    udp: &UdpSocket,
    broadcast_addr: &SocketAddr,
    app: &AppHandle<R>,
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

/// Query a mail root key with a 10-second budget.
///
/// Distinct from `fetch_via_zenoh` because the mail-root protocol treats an
/// empty reply as a valid sentinel ("no mail for this address yet") whereas
/// fetch_via_zenoh requires a successful non-empty reply. Returns:
/// - `Ok(Some(payload))` — at least one responder replied successfully. A
///   non-empty payload is the current root CID; an empty payload is the
///   explicit "no mail yet" sentinel from the gateway's queryable.
/// - `Ok(None)` — no responder replied at all. The caller surfaces this as
///   a failed query (e.g., no gateway with this queryable declared).
/// - `Err(msg)` — the `get` call itself failed, the 10s budget elapsed, or
///   every responder returned an error reply (no successful reply seen).
///
/// Multiple responders are tolerated via `ConsolidationMode::None`. A
/// non-empty success reply is preferred over the empty sentinel; either
/// success outcome is preferred over an error-only outcome.
///
/// Used by both the cold-start query and the manual refresh path. `op_label`
/// appears in the timeout message for log disambiguation ("startup" vs
/// "refresh").
async fn query_mail_root(
    session: &zenoh::Session,
    key: &str,
    op_label: &str,
) -> Result<Option<Vec<u8>>, String> {
    use zenoh::query::ConsolidationMode;

    let label = op_label.to_string();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        async {
            let replies = session
                .get(key)
                .consolidation(ConsolidationMode::None)
                .await
                .map_err(|e| format!("get: {e}"))?;

            // Drain all replies. Track three outcomes so an all-errors
            // result doesn't silently collapse into "no responder":
            //   - non_empty: a real root CID (best — short-circuits)
            //   - saw_empty: gateway explicitly says "no mail"
            //   - reply_error: every reply that landed was an Err
            let mut non_empty: Option<Vec<u8>> = None;
            let mut saw_empty = false;
            let mut reply_error: Option<String> = None;
            while let Ok(reply) = replies.recv_async().await {
                match reply.result() {
                    Ok(sample) => {
                        let bytes = sample.payload().to_bytes().to_vec();
                        if bytes.is_empty() {
                            saw_empty = true;
                        } else {
                            non_empty = Some(bytes);
                            break;
                        }
                    }
                    Err(err) => {
                        // Keep the first error message for the surfaced Err.
                        reply_error.get_or_insert_with(|| {
                            String::from_utf8_lossy(&err.payload().to_bytes())
                                .into_owned()
                        });
                    }
                }
            }
            if let Some(bytes) = non_empty {
                Ok(Some(bytes))
            } else if saw_empty {
                Ok(Some(Vec::new()))
            } else if let Some(err) = reply_error {
                Err(format!("{label} root query reply error: {err}"))
            } else {
                Ok(None)
            }
        },
    )
    .await
    .map_err(|_| format!("{op_label} root query timed out (10s)"))
    .and_then(|r| r)
}

/// Emit zenoh-status error when a Zenoh session appears to have been lost.
fn emit_session_lost<R: Runtime>(app: &AppHandle<R>, reason: &str) {
    let _ = app.emit(
        "zenoh-status",
        &crate::ZenohStatus {
            status: "error".to_string(),
            endpoint: None,
            error: Some(format!("session lost: {reason}")),
        },
    );
}

use harmony_content::book::BookStore;
use harmony_content::bundle;
use harmony_content::cache::ContentStore;
use harmony_content::cid::{CidType, ContentId};

/// Walk every CID in the tree rooted at `cid`, reading bundle payloads from
/// the local content store. Returns root + every descendant in DFS order.
///
/// Bundle payloads not in the store are silently skipped — their subtrees
/// are unreachable and the caller's verb can't act on them anyway. A
/// malformed bundle payload is treated the same: log-worthy but not fatal.
pub(crate) fn collect_descendants<S: BookStore>(
    store: &ContentStore<S>,
    cid: ContentId,
) -> Vec<ContentId> {
    use harmony_content::cid::MAX_BUNDLE_DEPTH;

    let mut out = Vec::new();
    let mut stack: Vec<(ContentId, u8)> = vec![(cid, 0)];
    while let Some((id, depth)) = stack.pop() {
        if depth > MAX_BUNDLE_DEPTH {
            tracing::warn!(
                cid_depth = depth,
                max = MAX_BUNDLE_DEPTH,
                "collect_descendants aborting subtree past MAX_BUNDLE_DEPTH; data is corrupt"
            );
            continue;
        }
        out.push(id);
        if matches!(id.cid_type(), CidType::Bundle(_)) {
            if let Some(bytes) = store.get(&id) {
                match bundle::parse_bundle(bytes) {
                    Ok(children) => {
                        for child in children.iter().copied() {
                            stack.push((child, depth + 1));
                        }
                    }
                    Err(e) => tracing::warn!(
                        err = ?e,
                        "malformed bundle payload; subtree skipped"
                    ),
                }
            }
        }
    }
    out
}

/// Fetch the bytes of a content tree by repeatedly calling `fetch_one` per
/// CID and concatenating leaf payloads in bundle-child order.
///
/// Iterative (not async-recursive) to avoid `Pin<Box<dyn Future>>` friction.
/// The order-preserving DFS is "push children in reverse, pop in child
/// order" — so for a bundle `[L1, L2, L3]` we emit bytes `L1 || L2 || L3`.
///
/// Depth-capped at `MAX_BUNDLE_DEPTH` for defensive safety — the write side
/// already enforces this, so legitimate trees never trip the guard.
///
/// Returns `Err` — rather than logging and skipping — on depth overflow or
/// a malformed bundle payload, in contrast to `collect_descendants`. Fetch
/// reassembly cannot produce a correct result with any subtree missing.
pub(crate) async fn fetch_recursive<F, Fut>(
    fetch_one: F,
    root: ContentId,
) -> Result<Vec<u8>, String>
where
    F: Fn(ContentId) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>, String>>,
{
    use harmony_content::cid::MAX_BUNDLE_DEPTH;

    let mut out = Vec::new();
    let mut stack: Vec<(ContentId, u8)> = vec![(root, 0)];

    while let Some((cid, depth)) = stack.pop() {
        if depth > MAX_BUNDLE_DEPTH {
            return Err(format!(
                "bundle depth {depth} exceeds MAX_BUNDLE_DEPTH {MAX_BUNDLE_DEPTH}"
            ));
        }
        let bytes = fetch_one(cid).await?;
        if matches!(cid.cid_type(), CidType::Bundle(_)) {
            let children = bundle::parse_bundle(&bytes)
                .map_err(|e| format!("malformed bundle: {e:?}"))?;
            for child in children.iter().rev() {
                stack.push((*child, depth + 1));
            }
        } else {
            out.extend_from_slice(&bytes);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod descendants_tests {
    use super::collect_descendants;
    use harmony_content::book::BookStore;
    use harmony_content::bundle::BundleBuilder;
    use harmony_content::cache::ContentStore;
    use harmony_content::cid::{ContentFlags, ContentId};
    use harmony_content::book::MemoryBookStore;

    fn new_store() -> ContentStore<MemoryBookStore> {
        ContentStore::new(MemoryBookStore::new(), 1024)
    }

    #[test]
    fn returns_just_the_root_for_a_leaf() {
        let mut store = new_store();
        let leaf = store
            .insert_with_flags(b"hello", ContentFlags::default())
            .unwrap();

        let all = collect_descendants(&store, leaf);
        assert_eq!(all, vec![leaf]);
    }

    #[test]
    fn walks_a_flat_bundle() {
        let mut store = new_store();
        let a = store.insert_with_flags(b"aaa", ContentFlags::default()).unwrap();
        let b = store.insert_with_flags(b"bbb", ContentFlags::default()).unwrap();
        let c = store.insert_with_flags(b"ccc", ContentFlags::default()).unwrap();

        let mut builder = BundleBuilder::new();
        builder.add(a).add(b).add(c);
        let (payload, root) = builder
            .build_with_flags(ContentFlags::default())
            .unwrap();
        store.store(root, payload);

        let all = collect_descendants(&store, root);
        // Order is unspecified; compare as sets.
        use std::collections::HashSet;
        let got: HashSet<ContentId> = all.into_iter().collect();
        let expected: HashSet<ContentId> = [root, a, b, c].into_iter().collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn skips_subtrees_whose_bundle_payload_is_missing() {
        let mut store = new_store();
        let a = store.insert_with_flags(b"aaa", ContentFlags::default()).unwrap();
        let b = store.insert_with_flags(b"bbb", ContentFlags::default()).unwrap();

        let mut builder = BundleBuilder::new();
        builder.add(a).add(b);
        let (_payload, root) = builder
            .build_with_flags(ContentFlags::default())
            .unwrap();
        // Deliberately DO NOT store the bundle payload.

        let all = collect_descendants(&store, root);
        // Walker should still include the root itself; children are
        // unreachable and therefore silently skipped.
        assert_eq!(all, vec![root]);
    }
}

#[cfg(test)]
mod fetch_recursive_tests {
    use super::fetch_recursive;
    use harmony_content::bundle::BundleBuilder;
    use harmony_content::cid::{ContentFlags, ContentId};
    use std::collections::HashMap;

    #[tokio::test]
    async fn leaf_only_fetch_returns_single_payload() {
        let leaf = ContentId::for_book(b"hello", ContentFlags::default()).unwrap();
        let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
        store.insert(leaf, b"hello".to_vec());

        let fetcher = move |cid: ContentId| {
            let bytes = store.get(&cid).cloned();
            std::future::ready(bytes.ok_or_else(|| format!("missing cid: {cid:?}")))
        };

        let got = fetch_recursive(fetcher, leaf).await.unwrap();
        assert_eq!(got, b"hello");
    }

    #[tokio::test]
    async fn bundle_fetch_concatenates_children_in_order() {
        let a_bytes = b"aaa".to_vec();
        let b_bytes = b"bbbb".to_vec();
        let c_bytes = b"ccccc".to_vec();
        let a = ContentId::for_book(&a_bytes, ContentFlags::default()).unwrap();
        let b = ContentId::for_book(&b_bytes, ContentFlags::default()).unwrap();
        let c = ContentId::for_book(&c_bytes, ContentFlags::default()).unwrap();

        let mut builder = BundleBuilder::new();
        builder.add(a).add(b).add(c);
        let (payload, root) = builder
            .build_with_flags(ContentFlags::default())
            .unwrap();

        let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
        store.insert(a, a_bytes.clone());
        store.insert(b, b_bytes.clone());
        store.insert(c, c_bytes.clone());
        store.insert(root, payload);

        let fetcher = move |cid: ContentId| {
            let bytes = store.get(&cid).cloned();
            std::future::ready(bytes.ok_or_else(|| format!("missing cid: {cid:?}")))
        };

        let got = fetch_recursive(fetcher, root).await.unwrap();
        let mut expected = a_bytes;
        expected.extend_from_slice(&b_bytes);
        expected.extend_from_slice(&c_bytes);
        assert_eq!(got, expected);
    }

    #[tokio::test]
    async fn missing_leaf_propagates_error() {
        let a = ContentId::for_book(b"aaa", ContentFlags::default()).unwrap();
        let b = ContentId::for_book(b"bbb", ContentFlags::default()).unwrap();
        let mut builder = BundleBuilder::new();
        builder.add(a).add(b);
        let (payload, root) = builder
            .build_with_flags(ContentFlags::default())
            .unwrap();

        let mut store: HashMap<ContentId, Vec<u8>> = HashMap::new();
        // Deliberately omit `b`.
        store.insert(a, b"aaa".to_vec());
        store.insert(root, payload);

        let fetcher = move |cid: ContentId| {
            let bytes = store.get(&cid).cloned();
            std::future::ready(bytes.ok_or_else(|| format!("missing cid: {cid:?}")))
        };

        let err = fetch_recursive(fetcher, root).await.unwrap_err();
        assert!(err.contains("missing cid"), "got: {err}");
    }
}

#[cfg(test)]
mod content_verb_tests {
    use super::ContentVerbRequest;

    #[test]
    fn read_bytes_verb_variant_is_constructible() {
        let (reply_tx, _reply_rx) =
            tokio::sync::oneshot::channel::<Option<Vec<u8>>>();
        let req = ContentVerbRequest::ReadBytes {
            cid: [0x7Au8; 32],
            reply: reply_tx,
        };
        match req {
            ContentVerbRequest::ReadBytes { cid, .. } => {
                assert_eq!(cid, [0x7Au8; 32]);
            }
            _ => panic!("matched wrong variant"),
        }
    }
}

/// Bridge Zenoh subscription messages to Tauri frontend events.
#[allow(clippy::too_many_arguments)]
fn emit_frontend_event<R: Runtime>(
    app: &AppHandle<R>,
    key_expr: &str,
    payload: &[u8],
    hop_distance: Option<u8>,
    followed_set: &std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    mail_mgr: &std::sync::Arc<std::sync::Mutex<crate::mail::MailManager>>,
    own_mail_key: &str,
    own_root_key: &str,
    mail_sync: Option<&Arc<crate::mail_sync::MailSync<R>>>,
) {
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
        if key_expr.contains("/reactions/") {
            // Vine reaction event — emit directly to frontend.
            if let Ok(reaction) = serde_json::from_slice::<crate::VineReactionPayload>(payload) {
                let _ = app.emit("vine-reaction-received", &reaction);
            }
        } else {
            // Vine descriptor — deserialize as typed payload first to reject malformed data,
            // then re-serialize with the source tag injected.
            if let Ok(vine) = serde_json::from_slice::<crate::VineDescriptorPayload>(payload) {
                let is_followed = {
                    let set = followed_set.lock().unwrap();
                    set.contains(vine.creator_address.as_str())
                };
                let source = if is_followed { "followed" } else { "discover" };
                // Re-serialize to Value so we can inject the source field
                if let Ok(mut val) = serde_json::to_value(&vine) {
                    if let Some(obj) = val.as_object_mut() {
                        obj.insert("source".to_string(), serde_json::Value::String(source.to_string()));
                    }
                    let _ = app.emit("vine-received", &val);
                }
            }
        }
    } else if key_expr.starts_with("harmony/announce/") {
        if let Some(announcement) = crate::parse_content_announcement(key_expr, payload) {
            let _ = app.emit("content-announced", &announcement);
        }
    } else if key_expr.contains("/telemetry/") {
        if let Some(event) = crate::parse_telemetry(payload) {
            let _ = app.emit("telemetry-event", &event);
        }
    } else if !own_root_key.is_empty() && key_expr == own_root_key {
        // Phase 2: root CID push for this node's mailbox. Forward to
        // MailSync which re-walks the tree and registers header-only
        // entries for any new descendants. Spawn so the event loop
        // keeps pumping while the walker runs.
        if let Some(sync) = mail_sync {
            let sync = Arc::clone(sync);
            let payload = payload.to_vec();
            tokio::spawn(async move {
                sync.handle_root_push(&payload).await;
            });
        } else {
            tracing::debug!("got root push but mail_sync not initialized; ignoring");
        }
    } else if !own_mail_key.is_empty() && key_expr == own_mail_key {
        // Inbound mail delivery — store in MailManager and notify frontend.
        // NOTE: receive_message performs blocking disk I/O (blob write + index
        // persist) while holding the mutex. Acceptable for Phase 0 since mail
        // is infrequent. Phase 1 should offload to spawn_blocking or a
        // dedicated writer thread to avoid stalling the event loop under burst.
        //
        // Emit `mail-received` only on a fresh Insert. A Promoted outcome
        // means the walker already surfaced this row via register_header_only,
        // so re-emitting would duplicate the notification the user already saw.
        let mut mgr = match mail_mgr.lock() {
            Ok(g) => g,
            Err(e) => {
                tracing::error!(error = %e, "mail_mgr mutex poisoned");
                return;
            }
        };
        match mgr.receive_message(payload) {
            Ok(crate::mail::ReceiveOutcome::Inserted(entry)) => {
                let _ = app.emit("mail-received", &entry);
            }
            Ok(crate::mail::ReceiveOutcome::Promoted(_entry)) => {
                tracing::debug!(
                    key_expr,
                    "live push promoted Pending to Local (no emit)"
                );
            }
            Err(e) => {
                tracing::debug!(key_expr, error = %e, "mail receive skipped");
            }
        }
    }
}
