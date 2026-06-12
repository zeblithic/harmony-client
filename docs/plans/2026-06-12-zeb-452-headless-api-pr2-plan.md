# ZEB-452: Headless API PR 2 — GUI-mode opt-in server + `harmony-app api` CLI — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A GUI instance opts into hosting the same localhost API server `harmony-app serve` runs (via `HARMONY_API_PORT`), with every node event mirrored to the WS firehose; and a thin `harmony-app api` CLI drives any running server (RPC + events) with strict stdout purity.

**Architecture:** Three additions to the PR-1 machinery (spec: `docs/specs/2026-06-11-zeb-445-headless-control-surface-design.md`, §Host 2 + §Thin CLI). (1) A `NodeStateAccess` trait lets `ApiCtx` borrow Tauri's managed `NodeState` instead of requiring an owned `Arc<Mutex<NodeState>>`. (2) GUI parity is achieved at the **sink impl**, not per-wrapper: `impl NodeEventSink for AppHandle` mirrors every emission onto the broadcast when an active `ApiHost` is managed — zero wrapper churn, no site can be forgotten (there are 6+ wrapper-level sink constructions; an emission added next month is covered automatically). (3) `api/cli.rs` holds testable async client fns; `main.rs` adds the `api` clap variant.

**Tech Stack:** axum 0.8 (already prod), reqwest 0.12 + tokio-tungstenite 0.24 (promoted dev→prod, TLS-free), clap, tauri 2 (`test` feature already in dev-deps for mock runtime).

**Spec deviations (decided at plan time, document in PR body):**
1. GUI lock contention does NOT abort the GUI — it disables the API server with a loud `tracing::error`. A windowed launch is the user's foreground intent; serve exits because the API *is* its purpose. (Spec §Lifecycle says GUI takes the same lock; it doesn't define the contention behavior.)
2. `POST /v1/shutdown` on a GUI host quits the whole app via the established `quit-requested` → FE teardown → `quit_app` flow (with the same 3s fallback exit the tray Quit arms). Spec calls shutdown "the headless analogue of the GUI's quit_app" — this is that analogue, honestly.
3. Fan-out is implemented as AppHandle-impl mirroring (see Architecture) rather than a literal `FanoutSink` at each wrapper. Same observable behavior; `FanoutSink` remains for tests/other hosts.

**House rules (apply to every task):**
- Work on branch `zeb-452-headless-api-pr2` in `/Users/zeblith/work/zeblithic/harmony-client`. No worktrees. NO pushes.
- Commit BEFORE running gates (commit-before-gate). Amend if a gate forces changes.
- `set -o pipefail` on every piped shell command. Cargo commands run from `src-tauri/`.
- Per-task gates (~1-2 min): `cargo fmt --all` then `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings` then `cargo nextest run --locked -p harmony-app --lib --features test-fixtures`. Task-specific additions listed per task. Do NOT run `--all-targets` per task (97-binary relink, ~25 min) — the final sweep covers it.
- 10-minute wall-clock kill switch per gate command (Bash timeout param). If a gate exceeds it, report DONE_WITH_CONCERNS with the partial output.
- Statuses: DONE / DONE_WITH_CONCERNS / NEEDS_CONTEXT / BLOCKED.

---

### Task 1: `NodeStateAccess` trait — let the server borrow managed state

**Files:**
- Modify: `src-tauri/src/api/mod.rs` (trait + `ApiCtx.state` type + `status_handler` + `start_server` signature)
- Modify: `src-tauri/src/api/rpc.rs` (`RpcHandler` type + `dispatch` signature + `rpc!` macro)

The serve path owns `Arc<Mutex<NodeState>>`; the GUI's `NodeState` lives in Tauri's managed-state container (`.manage(Mutex::new(NodeState::default()))`, lib.rs:41824) with no `Arc` to clone out. Changing 183 command signatures to manage an `Arc` is not acceptable churn; a one-method object-safe trait is.

- [ ] **Step 1: Add the trait to `api/mod.rs`** (below the `use` block, above `ApiCtx`):

```rust
/// Mode-agnostic access to the node state behind the server. serve mode
/// owns an `Arc<Mutex<NodeState>>`; the GUI host (ZEB-452) borrows Tauri's
/// managed state through its `AppHandle`. One method keeps it object-safe
/// so `ApiCtx` holds `Arc<dyn NodeStateAccess>` without generics leaking
/// into axum handler signatures.
pub trait NodeStateAccess: Send + Sync + 'static {
    fn node_state(&self) -> &Mutex<NodeState>;
}

impl NodeStateAccess for Mutex<NodeState> {
    fn node_state(&self) -> &Mutex<NodeState> {
        self
    }
}
```

- [ ] **Step 2: Re-type `ApiCtx.state` and `start_server`'s `state` param**

In `ApiCtx`: `pub state: Arc<dyn NodeStateAccess>,` (was `Arc<Mutex<NodeState>>`).
In `start_server`: `state: Arc<dyn NodeStateAccess>,` (same positional slot). The `ApiCtx { state, ... }` construction is unchanged.

- [ ] **Step 3: Fix `status_handler`'s lock**

```rust
    let (running, generation, owner_id) = match ctx.state.node_state().lock() {
```
(rest of the match unchanged).

- [ ] **Step 4: Re-type the registry plumbing in `api/rpc.rs`**

```rust
pub type RpcHandler = Box<
    dyn Fn(Arc<dyn super::NodeStateAccess>, Arc<dyn NodeEventSink>, serde_json::Value) -> RpcFuture
        + Send
        + Sync,
>;
```

`dispatch`: `state: Arc<dyn super::NodeStateAccess>,` (body unchanged — `h(state, sink, args).await`).

`rpc!` macro: the closure takes the access object; the async block rebinds `$state` to the borrowed `&Mutex<NodeState>`, so every registry entry's `&state` call-site expression (`&&Mutex` → deref-coerces to `&Mutex`) compiles unchanged:

```rust
macro_rules! rpc {
    ($map:expr, $name:literal, $args_ty:ty, |$state:ident, $sink:ident, $args:ident| $call:expr) => {
        $map.insert(
            $name,
            Box::new(
                move |__access: Arc<dyn super::NodeStateAccess>,
                      $sink: Arc<dyn NodeEventSink>,
                      raw: serde_json::Value| {
                    Box::pin(async move {
                        let $state = __access.node_state();
                        let raw = if raw.is_null() {
                            serde_json::json!({})
                        } else {
                            raw
                        };
                        let $args: $args_ty = serde_json::from_value(raw)
                            .map_err(|e| RpcError::BadArgs(e.to_string()))?;
                        let out = $call.await.map_err(RpcError::Command)?;
                        serde_json::to_value(out)
                            .map_err(|e| RpcError::Command(format!("serialize: {e}")))
                    }) as RpcFuture
                },
            ) as RpcHandler,
        );
    };
}
```

- [ ] **Step 5: Verify the callers need no change, then build**

`serve_cli` (lib.rs ~15468) passes `state.clone()` where `state: Arc<Mutex<NodeState>>` — unsize-coerces to `Arc<dyn NodeStateAccess>` at the call. Same for `api/mod.rs::tests::boot()`, `rpc.rs::tests` dispatch calls (via `test_state()`), and `tests/api_server.rs`. Expect zero edits there; if the compiler disagrees, add an explicit `as Arc<dyn crate::api::NodeStateAccess>` at the call site rather than re-typing test helpers.

Run: `cargo check --locked -p harmony-app --features test-fixtures` — expect clean.

- [ ] **Step 6: Commit, then gates**

```bash
git add -A && git commit -m "refactor(zeb-452): NodeStateAccess seam — ApiCtx borrows managed or owned NodeState"
cargo fmt --all
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --lib --features test-fixtures
cargo nextest run --locked -p harmony-app --features test-fixtures --test api_server
```
Expected: lib suite green (2451+ tests), api_server 1/1 green (~25-40s; it boots a real node). Amend the commit if fmt changed files.

---

### Task 2: GUI host — `api/gui_host.rs`, AppHandle mirroring, `run()` wiring

**Files:**
- Create: `src-tauri/src/api/gui_host.rs`
- Modify: `src-tauri/src/api/mod.rs` (add `pub mod gui_host;`)
- Modify: `src-tauri/src/node_event_sink.rs` (AppHandle impl mirrors to broadcast; new test)
- Modify: `src-tauri/src/lib.rs` (`run()` setup hook wiring, after the ZEB-433 tray block ~line 41820)

- [ ] **Step 1: Create `api/gui_host.rs`**

```rust
// src-tauri/src/api/gui_host.rs — ZEB-452: GUI-mode opt-in API host.
//
// When HARMONY_API_PORT is set, the GUI process hosts the same localhost
// API server `harmony-app serve` runs: same bearer auth, same profile
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

        let frame = rx.try_recv().expect("frame must be mirrored to the broadcast");
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
```

- [ ] **Step 2: Declare the module** — in `api/mod.rs`, after `pub mod events;`: `pub mod gui_host;`

- [ ] **Step 3: Mirror in the AppHandle sink impl** — replace the existing impl in `node_event_sink.rs`:

```rust
impl<R: tauri::Runtime> NodeEventSink for tauri::AppHandle<R> {
    fn emit(&self, event: &str, payload: serde_json::Value) {
        // ZEB-452: GUI-mode API parity. When this process also hosts the
        // localhost API (HARMONY_API_PORT), every GUI-bound event is
        // mirrored onto the WS broadcast here — at the sink, not per
        // wrapper — so no emission site (current or future) can miss the
        // stream. ApiHost is managed in both modes; `events` is None when
        // the API is off, and try_state covers early emissions before
        // setup manages it.
        if let Some(host) = tauri::Manager::try_state::<crate::api::gui_host::ApiHost>(self) {
            if let Some(events) = &host.events {
                NodeEventSink::emit(events, event, payload.clone());
            }
        }
        // Fully-qualified call into tauri's Emitter trait — NOT a recursive
        // call into NodeEventSink::emit.
        if let Err(e) = tauri::Emitter::emit(self, event, payload) {
            tracing::warn!(event, error = %e, "tauri emit failed");
        }
    }
}
```

- [ ] **Step 4: Wire `run()`'s setup hook** — in lib.rs, inside `.setup(|app| { ... })`, immediately after the ZEB-433 tray block (`app.manage(TrayActive(...));`, ~line 41819) and before the closing `Ok(())`:

```rust
            // ── ZEB-452: GUI-mode opt-in localhost API (HARMONY_API_PORT). ──
            // ApiHost is managed unconditionally (events: None when off) so
            // the AppHandle event sink can query it without a panic.
            {
                use tauri::Manager;
                let host = match crate::api::gui_host::gui_api_port_from_env() {
                    Some(port) => {
                        crate::api::gui_host::start_gui_api(app.handle().clone(), port)
                    }
                    None => crate::api::gui_host::ApiHost::disabled(),
                };
                app.manage(host);
            }
```

(If `use tauri::Manager;` is already in scope inside the setup closure from the tray block, drop the duplicate import — match what compiles cleanly.)

- [ ] **Step 5: Commit, then gates**

```bash
git add -A && git commit -m "feat(zeb-452): GUI-mode opt-in API server (HARMONY_API_PORT) + AppHandle event mirroring"
cargo fmt --all
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```
Expected: lib suite green including the three new tests. Amend if fmt changed files.

---

### Task 3: `harmony-app api` CLI — deps, `api/cli.rs`, main.rs variant

**Files:**
- Modify: `src-tauri/Cargo.toml` (promote reqwest/tokio-tungstenite/futures-util dev→prod)
- Create: `src-tauri/src/api/cli.rs`
- Modify: `src-tauri/src/api/mod.rs` (`pub mod cli;`)
- Modify: `src-tauri/src/lib.rs` (re-export `api_cli` next to `serve_cli`)
- Modify: `src-tauri/src/main.rs` (clap `Api` variant + dispatch arm)

- [ ] **Step 1: Promote the client deps.** In `[dependencies]` (next to the ZEB-445 axum/fd-lock block):

```toml
# ZEB-452 `harmony-app api` CLI client: reqwest for the one-shot RPC POST,
# tokio-tungstenite for the events stream. TLS-free on purpose — the API
# is loopback-only HTTP by design (spec §Auth).
reqwest = { version = "0.12", default-features = false, features = ["json"] }
tokio-tungstenite = "0.24"
futures-util = "0.3"
```

DELETE the three corresponding entries from `[dev-dependencies]` (production deps are visible to tests; keeping both would shadow-drift). Keep the explanatory comment lines that still apply or fold them into the new block. Run `cargo tree -p harmony-app -i reqwest --depth 1` only if resolution errors appear; `Cargo.lock` should update minimally (`cargo check` regenerates it — `--locked` gates come after the lockfile change is committed; for THIS task run the first `cargo check` WITHOUT `--locked` to let the lockfile pick up the dep-section move, then commit the lockfile).

- [ ] **Step 2: Create `api/cli.rs`**

```rust
// src-tauri/src/api/cli.rs — ZEB-452: `harmony-app api` thin client.
//
// Stdout purity (PR #231 discipline): the RPC result JSON / event frames
// are the ONLY stdout; every diagnostic goes to stderr. Exit codes:
//   0 — success (HTTP 200 / clean stream close)
//   1 — server-reported error (any non-200 RPC response; body → stderr)
//   2 — local failure (discovery files missing, connect refused, bad usage)
// Agents that prefer curl/websocat ignore this entirely — same wire
// contract (spec §Thin CLI).

use std::path::Path;

#[derive(Clone)]
pub struct Discovery {
    pub port: u16,
    pub token: String,
}

/// Read `<data-dir>/api/{port,token}`. Error text names the likely cause:
/// only a live server writes these files (and removes them on shutdown).
pub fn read_discovery(data_dir: &Path) -> Result<Discovery, String> {
    let api_dir = data_dir.join("api");
    let port_path = api_dir.join("port");
    let port_raw = std::fs::read_to_string(&port_path).map_err(|e| {
        format!(
            "read {}: {e} — is `harmony-app serve` (or a GUI with HARMONY_API_PORT) running?",
            port_path.display()
        )
    })?;
    let port: u16 = port_raw.trim().parse().map_err(|e| {
        format!(
            "port file {} is corrupt ({e}): {port_raw:?}",
            port_path.display()
        )
    })?;
    let token_path = api_dir.join("token");
    let token = std::fs::read_to_string(&token_path)
        .map_err(|e| format!("read {}: {e}", token_path.display()))?
        .trim()
        .to_string();
    if token.is_empty() {
        return Err(format!("token file {} is empty", token_path.display()));
    }
    Ok(Discovery { port, token })
}

/// One-shot RPC POST. `Ok((status, body))` for ANY HTTP response — the
/// caller maps non-200 to exit 1. `Err` is transport/local-validation
/// failure (exit 2). Args are validated as JSON locally so a typo'd shell
/// quote fails fast with a useful message instead of a server 400.
pub async fn rpc_call(
    d: &Discovery,
    command: &str,
    args_json: Option<&str>,
) -> Result<(u16, String), String> {
    let body: serde_json::Value = match args_json {
        None => serde_json::json!({}),
        Some(raw) => {
            serde_json::from_str(raw).map_err(|e| format!("args are not valid JSON: {e}"))?
        }
    };
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/rpc/{}", d.port, command))
        .bearer_auth(&d.token)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("POST /v1/rpc/{command}: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("read response body: {e}"))?;
    Ok((status, text))
}

/// Stream `/v1/events` frames into `on_frame` until the server closes the
/// socket (`Ok`) or the connection fails (`Err`). Interruption is the
/// process-level ctrl-c default — no special handling needed for a
/// read-only stream.
pub async fn stream_events(
    d: &Discovery,
    mut on_frame: impl FnMut(&str),
) -> Result<(), String> {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = format!("ws://127.0.0.1:{}/v1/events", d.port)
        .into_client_request()
        .map_err(|e| format!("build WS request: {e}"))?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", d.token)
            .parse()
            .map_err(|e| format!("auth header: {e}"))?,
    );
    let (ws, _resp) = tokio_tungstenite::connect_async(req)
        .await
        .map_err(|e| format!("connect /v1/events: {e}"))?;
    let (_write, mut read) = ws.split();
    while let Some(msg) = read.next().await {
        match msg {
            Ok(tokio_tungstenite::tungstenite::Message::Text(txt)) => on_frame(&txt),
            Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => return Ok(()),
            Ok(_) => {} // ping/pong/binary — not part of the frame contract
            Err(e) => return Err(format!("events stream: {e}")),
        }
    }
    Ok(())
}

/// Blocking CLI entry (called from main.rs). Validates the mode, resolves
/// discovery, runs the client on a current-thread runtime, prints, and
/// returns the process exit code.
pub fn api_cli(command: Option<String>, args_json: Option<String>, events: bool) -> i32 {
    if events && command.is_some() {
        eprintln!("api: --events takes no command (use one or the other)");
        return 2;
    }
    if !events && command.is_none() {
        eprintln!(
            "api: missing command (or --events). Example: harmony-app api get_owner_state"
        );
        return 2;
    }
    let data_dir = match crate::resolve_app_data_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("api: {e}");
            return 2;
        }
    };
    let d = match read_discovery(&data_dir) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("api: {e}");
            return 2;
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("api: cannot build tokio runtime: {e}");
            return 2;
        }
    };
    rt.block_on(async move {
        if events {
            use std::io::Write;
            match stream_events(&d, |frame| {
                // Explicit flush: stdout is block-buffered when piped, and
                // agents tail this stream live.
                let mut out = std::io::stdout();
                let _ = writeln!(out, "{frame}");
                let _ = out.flush();
            })
            .await
            {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("api: {e}");
                    2
                }
            }
        } else {
            let command = command.expect("mode validated above");
            match rpc_call(&d, &command, args_json.as_deref()).await {
                Ok((200, body)) => {
                    println!("{body}");
                    0
                }
                Ok((status, body)) => {
                    eprintln!("api: HTTP {status}: {body}");
                    1
                }
                Err(e) => {
                    eprintln!("api: {e}");
                    2
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_discovery_missing_files_names_the_server() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = read_discovery(dir.path()).expect_err("no files must err");
        assert!(
            err.contains("harmony-app serve"),
            "error must hint at the server: {err}"
        );
    }

    #[test]
    fn read_discovery_rejects_corrupt_port() {
        let dir = tempfile::tempdir().expect("tempdir");
        let api = dir.path().join("api");
        std::fs::create_dir_all(&api).expect("mkdir");
        std::fs::write(api.join("port"), "not-a-port").expect("write port");
        std::fs::write(api.join("token"), "deadbeef").expect("write token");
        let err = read_discovery(dir.path()).expect_err("corrupt port must err");
        assert!(err.contains("corrupt"), "{err}");
    }

    #[test]
    fn read_discovery_happy_path_trims_whitespace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let api = dir.path().join("api");
        std::fs::create_dir_all(&api).expect("mkdir");
        std::fs::write(api.join("port"), "7420\n").expect("write port");
        std::fs::write(api.join("token"), "abc123\n").expect("write token");
        let d = read_discovery(dir.path()).expect("happy path");
        assert_eq!(d.port, 7420);
        assert_eq!(d.token, "abc123");
    }

    #[test]
    fn read_discovery_rejects_empty_token() {
        let dir = tempfile::tempdir().expect("tempdir");
        let api = dir.path().join("api");
        std::fs::create_dir_all(&api).expect("mkdir");
        std::fs::write(api.join("port"), "7420").expect("write port");
        std::fs::write(api.join("token"), "\n").expect("write token");
        let err = read_discovery(dir.path()).expect_err("empty token must err");
        assert!(err.contains("empty"), "{err}");
    }

    /// Mode validation happens before any filesystem/network access, so
    /// these run hermetically.
    #[test]
    fn api_cli_rejects_bad_mode_combinations() {
        assert_eq!(api_cli(Some("x".into()), None, true), 2);
        assert_eq!(api_cli(None, None, false), 2);
    }
}
```

- [ ] **Step 3: Declare + re-export.** `api/mod.rs`: `pub mod cli;`. In lib.rs, directly after `serve_cli`'s closing brace: `pub use crate::api::cli::api_cli;`

- [ ] **Step 4: main.rs — clap variant + dispatch.** Add to `enum Command` (after `Serve`):

```rust
    /// Drive a running node's localhost API (ZEB-445): POST one RPC
    /// command (result JSON on stdout) or stream the event firehose with
    /// --events. Reads <data-dir>/api/{port,token} written by `serve` or
    /// a GUI launched with HARMONY_API_PORT.
    Api {
        /// RPC command name, e.g. get_owner_state (surface list:
        /// docs/headless-install.md).
        command: Option<String>,
        /// JSON object with the command's camelCase args, e.g.
        /// '{"name":"my community","isInviteOnly":true}'.
        args: Option<String>,
        /// Stream /v1/events as one JSON frame per line until interrupted.
        #[arg(long)]
        events: bool,
    },
```

Dispatch arm (after the `Serve` arm):

```rust
            Some(Command::Api {
                command,
                args,
                events,
            }) => {
                init_tracing();
                std::process::exit(harmony_app::api_cli(command, args, events));
            }
```

- [ ] **Step 5: Commit, then gates (note the extra `--bins` clippy — main.rs is not covered by `--lib`)**

```bash
git add -A && git commit -m "feat(zeb-452): harmony-app api CLI (rpc one-shot + --events stream)"
cargo fmt --all
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cargo clippy --locked -p harmony-app --bins --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```
Expected: green; five new cli unit tests pass. Amend if fmt changed files.

---

### Task 4: Integration test — `tests/api_cli.rs`

**Files:**
- Create: `src-tauri/tests/api_cli.rs`
- Modify: `src-tauri/src/api/events.rs` (add `receiver_count` accessor — 4 lines)

This boots the real `api::start_server` on an ephemeral port against a **default (not-running) NodeState** — the contract under test is discovery + transport + exit-code mapping, not node behavior (`tests/api_server.rs` owns the full-node flow; no iroh warm-up needed here, so this test is seconds, not half a minute). Keychain-hermetic per ZEB-428: tempdir HOME + `HARMONY_PASSPHRASE`; no node ever starts, so no identity is written.

- [ ] **Step 1: Add the subscriber-count accessor** to `ApiEventSink` in `api/events.rs` (after `subscribe`):

```rust
    /// Live WS subscriber count — lets tests wait for a subscription
    /// condition-style instead of sleeping a fixed interval.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
```

- [ ] **Step 2: Create `tests/api_cli.rs`.** Copy the env-guard phase from `tests/api_server.rs` (its Phase 1, ~lines 49-68: `_g1` HOME, `_g2` USERPROFILE, `_g3` HARMONY_PASSPHRASE, `_g4` XDG_DATA_HOME, `_g5` APPDATA — all via `common::set_env`). The file:

```rust
// tests/api_cli.rs — ZEB-452: `harmony-app api` client ↔ API server.
//
// Boots api::start_server on an ephemeral port against a default
// (not-running) NodeState: the CLI contract is discovery + transport +
// status mapping, not node behavior (tests/api_server.rs owns the
// full-node flow). ZEB-428 hermetic: tempdir HOME + HARMONY_PASSPHRASE,
// and no node is ever started so no identity is written anywhere.

mod common;

use std::sync::{Arc, Mutex};

#[tokio::test(flavor = "multi_thread")]
async fn cli_client_drives_rpc_and_event_stream_against_live_server() {
    // ── Phase 1: hermetic env (pattern from tests/api_server.rs) ────────
    let home = tempfile::tempdir().expect("tempdir HOME");
    let home_str = home.path().to_string_lossy().into_owned();
    let _g1 = common::set_env("HOME", &home_str);
    let _g2 = common::set_env("USERPROFILE", &home_str);
    let _g3 = common::set_env("HARMONY_PASSPHRASE", "api-cli-test-pp");
    let _g4 = common::set_env("XDG_DATA_HOME", &format!("{home_str}/xdg-data"));
    let _g5 = common::set_env("APPDATA", &format!("{home_str}/appdata"));

    let data_dir = harmony_app::resolve_app_data_dir().expect("data dir under temp HOME");
    std::fs::create_dir_all(&data_dir).expect("create data dir");

    // ── Phase 2: live server, ephemeral port, default NodeState ─────────
    let state = Arc::new(Mutex::new(harmony_app::NodeState::default()));
    let events = harmony_app::api::events::ApiEventSink::new();
    let sink: Arc<dyn harmony_app::node_event_sink::NodeEventSink> =
        Arc::new(events.clone());
    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (handle, server_task) = harmony_app::api::start_server(
        &data_dir,
        0,
        state,
        sink,
        events.clone(),
        shutdown_tx.clone(),
        shutdown_rx,
    )
    .await
    .expect("start_server");

    // ── Phase 3: discovery reads what the server wrote ───────────────────
    let d = harmony_app::api::cli::read_discovery(&data_dir).expect("discovery files");
    assert_eq!(d.port, handle.bound_port, "port file must carry the bound port");

    // ── Phase 4: RPC status mapping ──────────────────────────────────────
    // Unknown command → 404 (CLI exit-1 class).
    let (status, body) =
        harmony_app::api::cli::rpc_call(&d, "definitely_not_a_command", None)
            .await
            .expect("transport must succeed");
    assert_eq!(status, 404, "body: {body}");
    assert!(body.contains("unknown command"), "{body}");

    // Wrong token → 401.
    let bad = harmony_app::api::cli::Discovery {
        port: d.port,
        token: "wrong-token".into(),
    };
    let (status, body) = harmony_app::api::cli::rpc_call(&bad, "get_owner_state", None)
        .await
        .expect("transport must succeed");
    assert_eq!(status, 401, "body: {body}");
    assert!(body.contains("unauthorized"), "{body}");

    // Happy 200: pre-mint owner state on a fresh profile is JSON null
    // (get_owner_state_impl is disk-backed and node-independent).
    let (status, body) = harmony_app::api::cli::rpc_call(&d, "get_owner_state", None)
        .await
        .expect("transport must succeed");
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(body.trim(), "null", "fresh profile must be pre-mint: {body}");

    // Invalid args JSON is a LOCAL error — no HTTP round-trip.
    let err = harmony_app::api::cli::rpc_call(&d, "get_owner_state", Some("{not json"))
        .await
        .expect_err("local JSON validation must reject");
    assert!(err.contains("not valid JSON"), "{err}");

    // ── Phase 5: event stream end-to-end ─────────────────────────────────
    let (frames_tx, mut frames_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let d2 = d.clone();
    let streamer = tokio::spawn(async move {
        harmony_app::api::cli::stream_events(&d2, |frame| {
            let _ = frames_tx.send(frame.to_string());
        })
        .await
    });
    // Condition-based wait for the WS subscription (ZEB-453 lesson: the
    // ceiling is generous, the happy path breaks early).
    for _ in 0..100 {
        if events.receiver_count() > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        events.receiver_count() > 0,
        "WS client must subscribe within 10s"
    );

    use harmony_app::node_event_sink::NodeEventSink as _;
    events.emit("cli-test-1", serde_json::json!({"n": 1}));
    events.emit("cli-test-2", serde_json::json!({"n": 2}));

    let f1 = tokio::time::timeout(std::time::Duration::from_secs(10), frames_rx.recv())
        .await
        .expect("frame 1 within 10s")
        .expect("frame 1");
    let f2 = tokio::time::timeout(std::time::Duration::from_secs(10), frames_rx.recv())
        .await
        .expect("frame 2 within 10s")
        .expect("frame 2");
    let v1: serde_json::Value = serde_json::from_str(&f1).expect("frame 1 is JSON");
    let v2: serde_json::Value = serde_json::from_str(&f2).expect("frame 2 is JSON");
    assert_eq!(v1["event"], "cli-test-1");
    assert_eq!(v2["event"], "cli-test-2");
    assert!(
        v2["seq"].as_u64().expect("seq") > v1["seq"].as_u64().expect("seq"),
        "seq must be monotonic: {f1} then {f2}"
    );

    // ── Phase 6: graceful shutdown ends the stream cleanly ──────────────
    shutdown_tx.send(()).await.expect("request shutdown");
    let stream_result =
        tokio::time::timeout(std::time::Duration::from_secs(10), streamer)
            .await
            .expect("stream task must join within 10s of shutdown")
            .expect("stream task must not panic");
    assert!(
        stream_result.is_ok(),
        "server-initiated close must be a clean Ok end: {stream_result:?}"
    );
    let join = tokio::time::timeout(std::time::Duration::from_secs(10), server_task)
        .await
        .expect("server task must join within 10s")
        .expect("server task must not panic");
    assert!(join.is_ok(), "graceful shutdown must be Ok: {join:?}");
}
```

Visibility prerequisites (verify, fix if needed): `lib.rs` must export `pub mod api` (or re-export the used items) and `api/mod.rs` must have `pub mod cli;` / `pub mod events;` (Task 3 / PR 1 already did). `NodeState::default()` and `resolve_app_data_dir` are pub from PR 1.

- [ ] **Step 3: Commit, then gates**

```bash
git add -A && git commit -m "test(zeb-452): api_cli integration — discovery, status mapping, WS stream, clean close"
cargo fmt --all
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --features test-fixtures --test api_cli
```
Expected: 1/1 green in well under a minute (no node, no iroh). Amend if fmt changed files.

---

### Task 5: Docs + final sweep

**Files:**
- Modify: `docs/headless-install.md` ("API control surface" section — add GUI-mode + CLI subsections)
- Modify: `docs/troubleshooting.md` (serve-mode lifecycle subsection — add GUI/CLI pointers)

- [ ] **Step 1: `docs/headless-install.md`** — inside the existing "API control surface" section, append two subsections (adapt heading levels to the file's existing hierarchy):

```markdown
### GUI-mode opt-in (`HARMONY_API_PORT`)

A windowed (GUI) instance hosts the identical API when launched with
`HARMONY_API_PORT` set (env var only in v1 — no flag):

​```bash
HARMONY_API_PORT=7420 harmony-app        # fixed port
HARMONY_API_PORT=0 harmony-app           # OS-assigned; read <data-dir>/api/port
​```

Behavior notes:

- Auth, discovery files, and the profile lock are identical to `serve`.
- Every node event reaches the webview **and** the WS firehose.
- If the profile lock is already held (e.g. a `serve` process on the same
  profile), the GUI launches normally but the API stays **disabled** — look
  for `ZEB-452: profile lock unavailable` in the log. The lock exists
  because two nodes on one profile race state files; don't mix `serve` and
  a GUI on the same profile.
- `POST /v1/shutdown` quits the whole app gracefully (the API analogue of
  tray → Quit). A GUI quit by any other path may leave stale
  `<data-dir>/api/{port,token}` files behind; they are rewritten on every
  server start and are safe to ignore or delete.

### CLI client (`harmony-app api`)

No new binary — two subcommand forms on the existing app, reading
`<data-dir>/api/{port,token}`:

​```bash
# One-shot RPC: result JSON on stdout, exit 0.
harmony-app api get_owner_state
harmony-app api create_community '{"name":"testbed","isInviteOnly":true}'

# Event firehose: one {seq,event,payload} JSON frame per line until ^C.
harmony-app api --events
​```

Exit codes: `0` success · `1` server-reported error (the error JSON goes
to stderr — same strings the GUI sees) · `2` local failure (server not
running / discovery files missing / args not valid JSON / bad usage).

Stdout carries ONLY the result JSON or event frames (PR #231 stdout-purity
discipline); logs and errors go to stderr, so `harmony-app api ... | jq .`
is always safe. `curl`/`websocat` remain first-class alternatives.
```

(The `​```` fences above are shown indented to nest in this plan — write them as normal fences in the doc.)

- [ ] **Step 2: `docs/troubleshooting.md`** — in the serve-mode lifecycle subsection, append:

```markdown
- **`harmony-app api` exits 2 with "is `harmony-app serve` ... running?"** —
  no live server has written `<data-dir>/api/{port,token}`. Start `serve`
  (or a GUI with `HARMONY_API_PORT`), then retry.
- **GUI launched with `HARMONY_API_PORT` but no API answers** — check the
  log for `ZEB-452: profile lock unavailable`: another process (usually a
  `serve`) holds the profile. One node per profile in v1.
```

- [ ] **Step 3: Commit docs**

```bash
git add -A && git commit -m "docs(zeb-452): GUI-mode opt-in + api CLI sections"
```

- [ ] **Step 4: Final sweep (the `--all-targets` gate is load-bearing — CI parity)**

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --lib --features test-fixtures
cargo nextest run --locked -p harmony-app --features test-fixtures --test api_server --test api_cli
```
Expected: all green. clippy `--all-targets` takes several minutes (relinks test binaries) — this is the one place it runs locally; budget for it. Fix anything it finds, amend or add a fixup commit, re-run the failed gate.

---

## Self-review checklist (controller, after Task 5)

1. Spec §Host 2: HARMONY_API_PORT env opt-in ✓ (Task 2) — fan-out via sink mirror (deviation 3) ✓ — auth + lock identical ✓ (same `start_server`/`acquire`).
2. Spec §Thin CLI: two forms ✓ (Task 3) — stdout purity ✓ — discovery-file contract ✓.
3. Ticket DoD: ZEB-447 can drive a dev GUI instance via the API ✓ (Task 2) and gets curl-free ergonomics ✓ (Task 3).
4. Types consistent across tasks: `NodeStateAccess` (1) used by `GuiStateAccess` (2); `ApiHost.events` (2) read by node_event_sink.rs (2); `Discovery`/`rpc_call`/`stream_events` (3) exercised by (4); `receiver_count` (4) on `ApiEventSink` (PR 1).
5. PR body: reference ONLY ZEB-452. Note deviations 1-3. Unchecked manual box: live GUI smoke with HARMONY_API_PORT on Windows (Ildwyn/AVALON errand).
