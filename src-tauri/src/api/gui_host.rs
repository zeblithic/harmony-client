// src-tauri/src/api/gui_host.rs — ZEB-452: GUI-mode opt-in API host.
//
// When HARMONY_API_PORT is set, the GUI process hosts the same localhost
// API server `harmony-app serve` already runs: same bearer auth, same profile
// lock, same discovery files. Event parity is implemented at the sink:
// `impl NodeEventSink for AppHandle` (node_event_sink.rs) mirrors every
// emission onto `ApiHost.events`, so the webview and the WS firehose see
// one vocabulary with no per-wrapper fan-out plumbing.

use super::events::ApiEventSink;
use crate::NodeState;
use std::sync::{Arc, Mutex};

/// Managed in Tauri state in BOTH modes (`events: None` when the API is
/// disabled) so the AppHandle sink impl can query it unconditionally via
/// `try_state` without a missing-state panic.
pub struct ApiHost {
    pub events: Option<Arc<ApiEventSink>>,
    /// Held for the process lifetime: dropping it would let a second node
    /// onto this profile (the ZEB-420/ZEB-165 state-race class). `None`
    /// when the API is disabled — plain-GUI launches don't take the
    /// profile lock (documented v1 caveat, spec §Lifecycle).
    _lock: Option<super::lock::ProfileLock>,
}

impl ApiHost {
    pub fn disabled() -> Self {
        Self {
            events: None,
            _lock: None,
        }
    }
}

/// HARMONY_API_PORT parse, pure for unit-testability. `None` = API stays
/// off. An unparseable value disables the API *loudly* rather than
/// silently picking a default: the operator who set the var wanted a
/// server — give them a diagnosable refusal, not a surprise port.
pub(crate) fn parse_api_port(raw: Option<&str>) -> Option<u16> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    match raw.parse::<u16>() {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(
                value = raw,
                error = %e,
                "HARMONY_API_PORT is not a valid port; API server disabled"
            );
            None
        }
    }
}

pub fn gui_api_port_from_env() -> Option<u16> {
    parse_api_port(std::env::var("HARMONY_API_PORT").ok().as_deref())
}

/// Borrow Tauri's managed NodeState for the API server. The managed value
/// lives as long as the app; `State::inner` returns a borrow tied to the
/// `AppHandle` this struct owns, which satisfies `node_state`'s elided
/// `&self` lifetime.
struct GuiStateAccess(tauri::AppHandle);

impl super::NodeStateAccess for GuiStateAccess {
    fn node_state(&self) -> &Mutex<NodeState> {
        use tauri::Manager;
        self.0.state::<Mutex<NodeState>>().inner()
    }
}

/// Start the API server inside the GUI process (called from `run()`'s
/// setup hook when HARMONY_API_PORT is set). Returns the `ApiHost` for
/// `app.manage`.
///
/// Failure policy: every failure path returns `ApiHost::disabled()` and
/// logs at error level — a windowed launch is the user's foreground
/// intent, so the GUI never aborts over the opt-in API. (`serve` exits on
/// the same failures because the API *is* its purpose.)
pub fn start_gui_api(app: tauri::AppHandle, port: u16) -> ApiHost {
    let data_dir = match crate::resolve_app_data_dir() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "ZEB-452: cannot resolve data dir; API server disabled");
            return ApiHost::disabled();
        }
    };
    let lock = match super::lock::acquire(&data_dir.join("api")) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(
                error = %e,
                "ZEB-452: profile lock unavailable; API server disabled \
                 (is `harmony-app serve` already running on this profile?)"
            );
            return ApiHost::disabled();
        }
    };
    let events = ApiEventSink::new();
    let host = ApiHost {
        events: Some(events.clone()),
        _lock: Some(lock),
    };

    let state: Arc<dyn super::NodeStateAccess> = Arc::new(GuiStateAccess(app.clone()));
    // The server's RPC dispatch emits through the AppHandle sink — the
    // SAME sink the Tauri wrappers use — so API-triggered events reach the
    // webview AND (via the mirror in node_event_sink.rs) the WS stream.
    // Passing `events` directly here would skip the webview.
    let sink: Arc<dyn crate::node_event_sink::NodeEventSink> = Arc::new(app.clone());
    tauri::async_runtime::spawn(async move {
        let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (handle, server_task) = match super::start_server(
            &data_dir,
            port,
            state,
            sink,
            events,
            shutdown_tx,
            shutdown_rx,
        )
        .await
        {
            Ok(x) => x,
            Err(e) => {
                tracing::error!(error = %e, "ZEB-452: GUI API server failed to start");
                return;
            }
        };
        tracing::info!(
            port = handle.bound_port,
            "ZEB-452: GUI API server listening on 127.0.0.1"
        );
        let join = server_task.await;
        // Best-effort discovery cleanup on any exit: stale files are only
        // a confusing client error (rewritten on every server start).
        let _ = std::fs::remove_file(handle.api_dir.join("port"));
        let _ = std::fs::remove_file(handle.api_dir.join("token"));
        match join {
            Ok(Ok(())) => {
                // Graceful end = /v1/shutdown: the headless analogue of
                // quit_app (spec §Lifecycle). Quit exactly the way tray
                // Quit does — emit for FE voice/call teardown, arm the 3s
                // fallback exit in case the FE listener never initialized
                // (mirrors lib.rs run()'s tray "quit" handler).
                tracing::info!("ZEB-452: /v1/shutdown received; quitting app");
                let _ = tauri::Emitter::emit(&app, "quit-requested", ());
                let app2 = app.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    app2.exit(0);
                });
            }
            Ok(Err(e)) => {
                tracing::error!(error = %e, "ZEB-452: GUI API server failed; GUI continues without API");
            }
            Err(e) => {
                tracing::error!(error = %e, "ZEB-452: GUI API server task aborted; GUI continues without API");
            }
        }
    });
    host
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_event_sink::NodeEventSink;

    #[test]
    fn parse_api_port_accepts_valid_rejects_garbage() {
        assert_eq!(parse_api_port(None), None);
        assert_eq!(parse_api_port(Some("")), None);
        assert_eq!(parse_api_port(Some("   ")), None);
        assert_eq!(parse_api_port(Some("7421")), Some(7421));
        assert_eq!(parse_api_port(Some(" 7421 ")), Some(7421));
        assert_eq!(parse_api_port(Some("0")), Some(0), "0 = ephemeral, valid");
        assert_eq!(parse_api_port(Some("notaport")), None);
        assert_eq!(parse_api_port(Some("70000")), None, "u16 overflow");
        assert_eq!(parse_api_port(Some("-1")), None);
    }

    /// The load-bearing GUI-parity property: an emission through the
    /// AppHandle sink lands on the WS broadcast when an active ApiHost is
    /// managed (and the seq counter is the API sink's own).
    #[test]
    fn app_handle_sink_mirrors_onto_ws_broadcast_when_host_active() {
        use tauri::Manager;
        let app = tauri::test::mock_app();
        let events = ApiEventSink::new();
        let mut rx = events.subscribe();
        app.manage(ApiHost {
            events: Some(events),
            _lock: None,
        });
        let handle = app.handle().clone();

        NodeEventSink::emit(&handle, "mirror-test", serde_json::json!({"k": 1}));

        let frame = rx
            .try_recv()
            .expect("frame must be mirrored to the broadcast");
        assert_eq!(frame.event, "mirror-test");
        assert_eq!(frame.seq, 0);
        assert_eq!(frame.payload, serde_json::json!({"k": 1}));
    }

    /// Without a managed ApiHost (and with a disabled one), the AppHandle
    /// sink must not panic — try_state, not state.
    #[test]
    fn app_handle_sink_tolerates_absent_and_disabled_host() {
        use tauri::Manager;
        let app = tauri::test::mock_app();
        let handle = app.handle().clone();
        // No ApiHost managed at all:
        NodeEventSink::emit(&handle, "no-host", serde_json::json!(null));
        // Disabled host:
        app.manage(ApiHost::disabled());
        NodeEventSink::emit(&handle, "disabled-host", serde_json::json!(null));
    }
}
