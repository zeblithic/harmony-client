// src-tauri/src/api/ — ZEB-445 localhost control surface.
//
// Mode-agnostic: hosted by `harmony-app serve` (windowless) today and by the
// GUI process (opt-in) in a follow-up. Binds 127.0.0.1 only; bearer-token
// auth on every endpoint (see auth.rs for the trust-boundary rationale).
pub mod auth;
pub mod cli;
pub mod events;
pub mod gui_host;
pub mod lock;
pub mod rpc;
pub mod watch;

use crate::node_event_sink::NodeEventSink;
use crate::NodeState;
use axum::{
    extract::{Path, State, WebSocketUpgrade},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use std::sync::{Arc, Mutex};

/// Mode-agnostic access to the node state behind the server. serve mode
/// owns an `Arc<Mutex<NodeState>>`; the GUI host (ZEB-452) borrows Tauri's
/// managed state through its `AppHandle`. One method keeps it object-safe
/// so `ApiCtx` holds `Arc<dyn NodeStateAccess>` without generics leaking
/// into axum handler signatures.
pub trait NodeStateAccess: Send + Sync + 'static {
    fn node_state(&self) -> &Mutex<NodeState>;

    /// ZEB-719: an owned handle to the `NodeState` for the `'static` voting-tick
    /// auto-exec closure on the headless path. The GUI host borrows Tauri's managed
    /// state through its `AppHandle` and dispatches via that seam, so it keeps the
    /// default `None`. The `Arc<Self>` receiver stays object-safe for `Arc<dyn ..>`.
    fn node_state_arc(self: Arc<Self>) -> Option<Arc<Mutex<NodeState>>> {
        None
    }
}

impl NodeStateAccess for Mutex<NodeState> {
    fn node_state(&self) -> &Mutex<NodeState> {
        self
    }
    fn node_state_arc(self: Arc<Self>) -> Option<Arc<Mutex<NodeState>>> {
        Some(self)
    }
}

/// Shared server context, cloned into every handler via axum's `State`.
#[derive(Clone)]
pub struct ApiCtx {
    pub state: Arc<dyn NodeStateAccess>,
    pub sink: Arc<dyn NodeEventSink>,
    pub events: Arc<events::ApiEventSink>,
    pub registry: Arc<rpc::RpcRegistry>,
    pub token: Arc<String>,
    pub started: std::time::Instant,
    pub bound_port: u16,
    pub shutdown_tx: tokio::sync::mpsc::Sender<()>,
}

/// `GET /v1/status` response. camelCase on the wire like every other DTO.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusDto {
    running: bool,
    generation: u64,
    owner_id: Option<String>,
    uptime_secs: u64,
    port: u16,
    version: &'static str,
}

fn authed(ctx: &ApiCtx, headers: &axum::http::HeaderMap) -> bool {
    auth::check_bearer(
        &ctx.token,
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
    )
}

fn unauthorized() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({"error": "unauthorized"})),
    )
}

/// `POST /v1/rpc/{command}` — uniform dispatch into the curated registry.
/// Absent body (no Content-Type) → `Null` args, which the registry treats
/// as `{}` for no-arg commands.
async fn rpc_handler(
    State(ctx): State<ApiCtx>,
    Path(command): Path<String>,
    headers: axum::http::HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> (StatusCode, Json<serde_json::Value>) {
    if !authed(&ctx, &headers) {
        return unauthorized();
    }
    let args = body.map(|Json(v)| v).unwrap_or(serde_json::Value::Null);
    match ctx
        .registry
        .dispatch(&command, ctx.state.clone(), ctx.sink.clone(), args)
        .await
    {
        Ok(value) => (StatusCode::OK, Json(value)),
        Err(rpc::RpcError::UnknownCommand) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "unknown command"})),
        ),
        Err(rpc::RpcError::BadArgs(msg)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": msg})),
        ),
        Err(rpc::RpcError::Command(msg)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": msg})),
        ),
    }
}

/// `GET /v1/status` — node liveness + identity + server metadata.
async fn status_handler(
    State(ctx): State<ApiCtx>,
    headers: axum::http::HeaderMap,
) -> (StatusCode, Json<serde_json::Value>) {
    if !authed(&ctx, &headers) {
        return unauthorized();
    }
    let (running, generation, owner_id) = match ctx.state.node_state().lock() {
        Ok(guard) => (
            guard.node_is_running(),
            guard.generation_for_status(),
            guard.owner_id_hex_for_status(),
        ),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("state lock poisoned: {e}")})),
            );
        }
    };
    let dto = StatusDto {
        running,
        generation,
        owner_id,
        uptime_secs: ctx.started.elapsed().as_secs(),
        port: ctx.bound_port,
        version: env!("CARGO_PKG_VERSION"),
    };
    match serde_json::to_value(&dto) {
        Ok(v) => (StatusCode::OK, Json(v)),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("serialize status: {e}")})),
        ),
    }
}

/// `POST /v1/shutdown` — graceful process shutdown (the headless analogue
/// of the GUI's explicit quit). The send wakes `axum::serve`'s
/// graceful-shutdown future; serve_cli's select observes the server task
/// ending and runs the node teardown.
///
/// ZEB-703: owner-state is persisted BEFORE the 200 is sent. The ack is
/// the signal supervisors act on (`curl …/v1/shutdown; sleep N; relaunch`
/// — our own cross-WAN harness recipe), while `stop_inner`'s flush only
/// runs AFTER axum drains — i.e. after the caller already has its 200. A
/// supervisor that kills or relaunches on the ack would race the
/// process's final save point and silently lose any un-flushed owner-state
/// mutation (queued DMs — the observed ZEB-703 data loss). The pre-ack
/// `persist_now` closes that window: everything enqueued before the call
/// is durable by ack time. Bounded + best-effort: on timeout/error we
/// warn and proceed (the process is going down either way, and
/// `stop_inner`'s unconditional persist remains the backstop for the
/// well-behaved-supervisor path).
async fn shutdown_handler(
    State(ctx): State<ApiCtx>,
    headers: axum::http::HeaderMap,
) -> (StatusCode, Json<serde_json::Value>) {
    if !authed(&ctx, &headers) {
        return unauthorized();
    }
    // Snapshot the handles out of the sync-mutex guard (guard must not
    // live across an await); a poisoned lock degrades to no pre-flush.
    let (engine, stopping, inflight, dm_outbox) = match ctx.state.node_state().lock() {
        Ok(guard) => guard.pre_shutdown_flush_handles(),
        Err(e) => {
            tracing::warn!(error = %e, "ZEB-703: NodeState poisoned at shutdown; skipping pre-ack flush");
            (None, None, None, None)
        }
    };
    // Barrier (PR #485 round 1, CodeRabbit + Greptile P1): without it, a
    // DM mutation landing WHILE persist_now snapshots could miss the
    // persist and be lost to a kill-on-200 supervisor. Sequence mirrors
    // stop_inner's ZEB-234 teardown (idempotent when stop_inner repeats
    // it later):
    //   1. stopping=true — new fenced IPC mutations reject ("node
    //      stopping"), same Ordering::Release as stop_inner;
    //   2. drain-path gate — new drain ticks skip entirely, so no new
    //      detached Phase C task can spawn;
    //   3. drain the ZEB-234 send-fence permits — every in-flight fenced
    //      IPC mutation completes BEFORE the snapshot;
    //   4. drain the Phase C fence — every in-flight detached Phase C
    //      task (drain outcomes + deposit-rung acks) completes BEFORE
    //      the snapshot;
    //   5. persist_now — the snapshot now contains everything accepted
    //      before the shutdown call.
    // One 5s budget bounds the whole sequence; on timeout/error we warn +
    // proceed (the process is going down either way; a wedged engine task
    // starves stop_inner's backstop persist equally — the ZEB-509 fence
    // accepts the same degraded mode, see
    // fence_owner_state_flush_returns_on_stalled_engine).
    let barrier_and_persist = async {
        if let Some(flag) = stopping.as_ref() {
            flag.store(true, std::sync::atomic::Ordering::Release);
        }
        if let Some(outbox) = dm_outbox {
            let (gate, phase_c_sem) = {
                let guard = outbox.lock().await;
                guard.shutdown_fence_handles()
            };
            gate.store(true, std::sync::atomic::Ordering::Release);
            if let Some(sem) = inflight {
                crate::drain_dm_send_fence(sem).await;
            }
            // Wait for every in-flight detached Phase C task; permits are
            // dropped immediately — only the blocking effect is needed
            // (the gate prevents new spawns).
            match Arc::clone(&phase_c_sem)
                .acquire_many_owned(crate::dm_outbox::DRAIN_PHASE_C_FENCE_CAPACITY as u32)
                .await
            {
                Ok(permits) => drop(permits),
                Err(e) => {
                    tracing::warn!(error = %e, "ZEB-703: Phase C fence closed; proceeding");
                }
            }
        } else if let Some(sem) = inflight {
            crate::drain_dm_send_fence(sem).await;
        }
        match engine {
            Some(engine) => engine.persist_now().await.map_err(|e| e.to_string()),
            None => Ok(()),
        }
    };
    match tokio::time::timeout(std::time::Duration::from_secs(5), barrier_and_persist).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(
            error = %e,
            "ZEB-703: pre-ack owner-state persist failed; proceeding with shutdown"
        ),
        Err(_) => tracing::warn!(
            "ZEB-703: pre-ack barrier/persist timed out (5s); proceeding with shutdown"
        ),
    }
    // Err = shutdown already requested (channel full or receiver consumed) —
    // still report success; the process is going down either way.
    let _ = ctx.shutdown_tx.send(()).await;
    (
        StatusCode::OK,
        Json(serde_json::json!({"shuttingDown": true})),
    )
}

/// `GET /v1/events` — WS upgrade onto the event firehose. Auth is checked
/// from the upgrade request's headers BEFORE upgrading.
async fn events_handler(
    State(ctx): State<ApiCtx>,
    headers: axum::http::HeaderMap,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    if !authed(&ctx, &headers) {
        return unauthorized().into_response();
    }
    ws.on_upgrade(move |socket| events::forward_events(ctx.events.subscribe(), socket))
        .into_response()
}

pub fn router(ctx: ApiCtx) -> Router {
    Router::new()
        .route("/v1/rpc/{command}", post(rpc_handler))
        .route("/v1/status", get(status_handler))
        .route("/v1/shutdown", post(shutdown_handler))
        .route("/v1/events", get(events_handler))
        .with_state(ctx)
}

/// Discovery info returned by [`start_server`]: the actually-bound port and
/// the `<data-dir>/api` directory holding the `token` + `port` files.
pub struct ServerHandle {
    pub bound_port: u16,
    pub api_dir: std::path::PathBuf,
}

/// Bind 127.0.0.1:`requested_port` (0 = OS-assigned), write the discovery
/// files, and spawn the server task. The returned `JoinHandle` resolves when
/// graceful shutdown (one send on the shutdown channel) completes.
pub async fn start_server(
    data_dir: &std::path::Path,
    requested_port: u16,
    state: Arc<dyn NodeStateAccess>,
    sink: Arc<dyn NodeEventSink>,
    events: Arc<events::ApiEventSink>,
    shutdown_tx: tokio::sync::mpsc::Sender<()>,
    mut shutdown_rx: tokio::sync::mpsc::Receiver<()>,
) -> Result<(ServerHandle, tokio::task::JoinHandle<Result<(), String>>), String> {
    let api_dir = data_dir.join("api");
    // Bind BEFORE writing discovery files: the token file's existence signals
    // "server is live" to readers, so it must not precede the bind that
    // decides whether that's true (a failed bind would orphan a token that
    // authenticates to nothing).
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", requested_port))
        .await
        .map_err(|e| format!("bind 127.0.0.1:{requested_port}: {e}"))?;
    let bound_port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let token = auth::generate_token();
    auth::write_token_file(&api_dir, &token)?;
    std::fs::write(api_dir.join("port"), bound_port.to_string())
        .map_err(|e| format!("write port file: {e}"))?;
    let ctx = ApiCtx {
        state,
        sink,
        events,
        registry: Arc::new(rpc::build_registry()),
        token: Arc::new(token),
        started: std::time::Instant::now(),
        bound_port,
        shutdown_tx,
    };
    let app = router(ctx);
    // The task returns the serve outcome so the host (serve_cli) can exit
    // non-zero on abnormal termination instead of mistaking a server crash
    // for a clean shutdown.
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.recv().await;
            })
            .await
            .map_err(|e| format!("api server: {e}"))
    });
    Ok((
        ServerHandle {
            bound_port,
            api_dir,
        },
        task,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_event_sink::FanoutSink;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// ZEB-719: the `Mutex<NodeState>` impl (serve path) hands back the SAME
    /// allocation as an owned `Arc`, so the headless voting-tick closure dispatches
    /// against the very NodeState the node runs on.
    #[test]
    fn node_state_arc_mutex_impl_returns_same_arc() {
        let arc: Arc<Mutex<NodeState>> = Arc::new(Mutex::new(NodeState::default()));
        let dynned: Arc<dyn NodeStateAccess> = arc.clone();
        let recovered = dynned.node_state_arc().expect("Mutex impl yields Some");
        assert!(Arc::ptr_eq(&arc, &recovered), "same NodeState allocation");
    }

    /// The trait default (the GUI-host class, which dispatches via its `AppHandle`)
    /// returns `None` — so the headless closure falls through to the stub for it.
    #[test]
    fn node_state_arc_default_impl_returns_none() {
        struct NoArc;
        impl NodeStateAccess for NoArc {
            fn node_state(&self) -> &Mutex<NodeState> {
                unreachable!("not exercised")
            }
        }
        let d: Arc<dyn NodeStateAccess> = Arc::new(NoArc);
        assert!(
            d.node_state_arc().is_none(),
            "default is None (GUI-host class)"
        );
    }

    /// Boot a real server on an ephemeral port with a default (not-running)
    /// NodeState. Returns the handle, the server task, the token read back
    /// from the discovery file, and the tempdir (held to keep it alive).
    async fn boot() -> (
        ServerHandle,
        tokio::task::JoinHandle<Result<(), String>>,
        String,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = Arc::new(Mutex::new(NodeState::default()));
        let events = events::ApiEventSink::new();
        let sink: Arc<dyn NodeEventSink> = Arc::new(FanoutSink(vec![]));
        let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (handle, task) =
            start_server(dir.path(), 0, state, sink, events, shutdown_tx, shutdown_rx)
                .await
                .expect("start_server");
        let token = std::fs::read_to_string(handle.api_dir.join("token")).expect("read token file");
        (handle, task, token, dir)
    }

    /// Minimal raw HTTP/1.1 client: write the request, read to EOF.
    /// `Connection: close` in every request keeps the parsing trivial —
    /// no new dev-deps for an HTTP client.
    async fn raw_http(port: u16, request: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("read response");
        String::from_utf8_lossy(&buf).into_owned()
    }

    #[tokio::test]
    async fn status_requires_auth_and_reports_not_running() {
        let (handle, _task, token, _dir) = boot().await;

        let unauthed = raw_http(
            handle.bound_port,
            "GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(
            unauthed.contains("401"),
            "status without auth must be 401, got: {unauthed}"
        );
        assert!(
            unauthed.contains("unauthorized"),
            "401 body must carry the error, got: {unauthed}"
        );

        let authed = raw_http(
            handle.bound_port,
            &format!(
                "GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1\r\n\
                 Authorization: Bearer {token}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert!(
            authed.contains("200"),
            "authed status must be 200, got: {authed}"
        );
        assert!(
            authed.contains("\"running\":false"),
            "default NodeState must report not-running, got: {authed}"
        );
    }

    #[tokio::test]
    async fn unknown_rpc_is_404() {
        let (handle, _task, token, _dir) = boot().await;

        let resp = raw_http(
            handle.bound_port,
            &format!(
                "POST /v1/rpc/nope HTTP/1.1\r\nHost: 127.0.0.1\r\n\
                 Authorization: Bearer {token}\r\nContent-Type: application/json\r\n\
                 Content-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            ),
        )
        .await;
        assert!(
            resp.contains("404"),
            "unknown command must be 404, got: {resp}"
        );
        assert!(
            resp.contains("unknown command"),
            "404 body must say unknown command, got: {resp}"
        );
    }

    #[tokio::test]
    async fn shutdown_closes_server() {
        let (handle, task, token, _dir) = boot().await;

        let resp = raw_http(
            handle.bound_port,
            &format!(
                "POST /v1/shutdown HTTP/1.1\r\nHost: 127.0.0.1\r\n\
                 Authorization: Bearer {token}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert!(
            resp.contains("200"),
            "shutdown must respond 200, got: {resp}"
        );
        assert!(
            resp.contains("shuttingDown"),
            "shutdown body must confirm, got: {resp}"
        );

        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("server task must join within 5s of /v1/shutdown")
            .expect("server task must not panic")
            .expect("graceful shutdown must end the server cleanly (Ok)");
    }
}
