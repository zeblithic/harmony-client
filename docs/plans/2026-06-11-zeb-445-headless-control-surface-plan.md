# ZEB-445 PR 1: Headless Agent Control Surface — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `harmony-app serve` runs a windowless node driven over localhost HTTP RPC + WebSocket events, with token auth and a profile lock — per the approved spec `docs/specs/2026-06-11-zeb-445-headless-control-surface-design.md`.

**Architecture:** A `NodeEventSink` trait replaces `AppHandle` at every emission seam (event loop, MailSync, ChannelLog engines, curated commands), so the node boots without Tauri. A mode-agnostic `api/` module (axum) exposes `POST /v1/rpc/{command}` over a registry of non-gated `*_impl` fns (the existing house seam pattern, un-cfg-gated) plus a WS firehose fed by a broadcast-backed sink impl.

**Tech Stack:** Rust, axum 0.8 (`ws` feature), tokio (already present), fd-lock 4 (advisory file lock), rand 0.8 (already present). Dev: reqwest + tokio-tungstenite.

**Branch:** `zeb-445-headless-control-surface` (off main `71c2c7c4`; spec committed at `9d33f884`).

---

## House rules for every task

- Work directly in the main repo (NO worktrees). Commit BEFORE running gates.
- Per-task gates (10-minute wall-clock kill switch per command, `set -o pipefail` when piping):
  ```bash
  cargo fmt --all
  cargo clippy -p harmony-app --lib --features test-fixtures -- -D warnings
  cargo nextest run -p harmony-app --lib --features test-fixtures
  ```
  Task 10 additionally runs `--test api_server`. Task 11 runs the full sweep. Do NOT run `--all-targets` clippy/nextest per-task (~25 min relink, ZEB-420/383/374 flakes are known — record, don't chase).
- Statuses: DONE / DONE_WITH_CONCERNS / NEEDS_CONTEXT / BLOCKED. NO pushes by implementers.
- `#[tauri::command]` wrapper names, parameter names, and DTOs must NOT change — the frontend is untouched in PR 1.

## File structure (what exists / what's created)

| File | Role |
|---|---|
| `src-tauri/src/node_event_sink.rs` (NEW) | `NodeEventSink` trait + AppHandle impl + fan-out + test sink |
| `src-tauri/src/api/mod.rs` (NEW) | server assembly: router, auth middleware, status, shutdown, discovery files, `serve_core` |
| `src-tauri/src/api/auth.rs` (NEW) | token generate/write(0600)/verify |
| `src-tauri/src/api/lock.rs` (NEW) | profile lock via fd-lock + PID breadcrumb |
| `src-tauri/src/api/events.rs` (NEW) | `EventFrame`, `ApiEventSink` (broadcast + seq), WS handler |
| `src-tauri/src/api/rpc.rs` (NEW) | registry, dispatch, arg structs, the 27 registrations |
| `src-tauri/src/lib.rs` (MOD) | sink adapters, `*_impl` extractions, `start_node_inner` generalization, `serve_cli`, `resolve_app_data_dir` |
| `src-tauri/src/event_loop.rs` (MOD) | `run()` de-Tauri'd: `Arc<dyn NodeEventSink>` instead of `AppHandle<R>` (31 emit sites) |
| `src-tauri/src/mail_sync.rs` (MOD) | `MailSync<R>` → `MailSync` holding the sink |
| `src-tauri/src/community_channel_log_engine.rs` (MOD) | `ChannelLogEngine<R>`/`ChannelLogRegistry<R>` de-genericized to the sink |
| `src-tauri/src/main.rs` (MOD) | `Serve { api_port }` clap variant |
| `src-tauri/src/app_tracing.rs` (MOD) | `init_serve_tracing()` (stderr + file, stdout-pure) |
| `src-tauri/Cargo.toml` (MOD) | axum/fd-lock prod deps; reqwest/tokio-tungstenite dev-deps |
| `src-tauri/tests/api_server.rs` (NEW) | end-to-end integration test |
| `docs/headless-install.md`, `docs/troubleshooting.md` (MOD) | docs |

**The curated 27 (all confirmed to exist; wrappers at these lib.rs anchors):**
lifecycle `start_node`:2269, `stop_node`:7582; identity `get_owner_state` (owner_commands.rs:160); communities `create_community`:18482, `list_owner_communities`:15492, `list_community_members`:15606, `generate_invite`:17584, `redeem_invite`:20719, `join_open_community`:20992, `leave_community`:24718; channels `create_channel`:16635, `list_channels`:17237, `list_channel_messages`:17426, `post_channel_message`:17346; friends `list_friends`:38455, `generate_friend_token`:38244, `redeem_friend_token`:38331, `add_friend_by_key`:39668, `list_pending_friend_requests`:39040, `accept_friend_request`:39061, `decline_friend_request` (adjacent to accept), DMs `add_space`:9036 (seam `add_space_dm_inner`:8760 exists), `send_dm`:8085, `read_dm_thread`:8387 (seam `read_dm_thread_inner`:8325 exists); diagnostics `connectivity_get_my_reachability_record`:40762, `connectivity_list_peer_reachability`:40808, `network_health_snapshot`:40933, `network_health_run_self_test`:40979.

---

### Task 1: `NodeEventSink` trait + impls

**Files:**
- Create: `src-tauri/src/node_event_sink.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod node_event_sink;` next to the other module decls)

- [ ] **Step 1: Write the module with trait, impls, and unit tests**

```rust
// src-tauri/src/node_event_sink.rs
//
// ZEB-445: mode-agnostic event emission. The GUI emits to the webview via
// AppHandle; serve mode emits to the WS broadcast; a GUI instance with the
// API enabled fans out to both. Payloads are serde_json::Value at the trait
// boundary so the trait stays object-safe.

use serde::Serialize;

pub trait NodeEventSink: Send + Sync + 'static {
    fn emit(&self, event: &str, payload: serde_json::Value);
}

/// Serialize-then-emit helper. Serialization failure is logged and dropped —
/// emission is fire-and-forget everywhere today (`let _ = app.emit(...)`).
pub fn emit_ser<T: Serialize>(sink: &dyn NodeEventSink, event: &str, payload: &T) {
    match serde_json::to_value(payload) {
        Ok(v) => sink.emit(event, v),
        Err(e) => tracing::warn!(event, error = %e, "event payload serialization failed"),
    }
}

impl<R: tauri::Runtime> NodeEventSink for tauri::AppHandle<R> {
    fn emit(&self, event: &str, payload: serde_json::Value) {
        use tauri::Emitter;
        if let Err(e) = tauri::Emitter::emit(self, event, payload) {
            tracing::warn!(event, error = %e, "tauri emit failed");
        }
    }
}

/// Fan-out to several sinks (GUI + API simultaneously).
pub struct FanoutSink(pub Vec<std::sync::Arc<dyn NodeEventSink>>);

impl NodeEventSink for FanoutSink {
    fn emit(&self, event: &str, payload: serde_json::Value) {
        for s in &self.0 {
            s.emit(event, payload.clone());
        }
    }
}

#[cfg(test)]
pub(crate) struct RecordingSink(pub std::sync::Mutex<Vec<(String, serde_json::Value)>>);

#[cfg(test)]
impl RecordingSink {
    pub(crate) fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self(std::sync::Mutex::new(Vec::new())))
    }
}

#[cfg(test)]
impl NodeEventSink for std::sync::Arc<RecordingSink> {
    fn emit(&self, event: &str, payload: serde_json::Value) {
        self.0.lock().unwrap().push((event.to_string(), payload));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct P {
        some_field: u32,
    }

    struct Rec(std::sync::Mutex<Vec<(String, serde_json::Value)>>);
    impl NodeEventSink for Rec {
        fn emit(&self, event: &str, payload: serde_json::Value) {
            self.0.lock().unwrap().push((event.to_string(), payload));
        }
    }

    #[test]
    fn emit_ser_preserves_camel_case_dto_shape() {
        let r = std::sync::Arc::new(Rec(std::sync::Mutex::new(vec![])));
        emit_ser(r.as_ref(), "x", &P { some_field: 7 });
        let got = r.0.lock().unwrap();
        assert_eq!(got[0].1["someField"], 7);
    }

    #[test]
    fn fanout_delivers_to_all_sinks_in_order() {
        let a = std::sync::Arc::new(Rec(std::sync::Mutex::new(vec![])));
        let b = std::sync::Arc::new(Rec(std::sync::Mutex::new(vec![])));
        let f = FanoutSink(vec![a.clone(), b.clone()]);
        f.emit("e", serde_json::json!({"k": 1}));
        assert_eq!(a.0.lock().unwrap().len(), 1);
        assert_eq!(b.0.lock().unwrap().len(), 1);
    }
}
```

Note: `impl NodeEventSink for Arc<Rec>` style impls require the blanket-free direct impls above; `Arc<dyn NodeEventSink>` itself is used by callers via `.as_ref()` or by implementing for the concrete type and wrapping in `Arc` at the call site. Where a call site holds `Arc<dyn NodeEventSink>`, call `emit_ser(&*sink, ...)`.

- [ ] **Step 2: Register the module and run gates**

Add `pub mod node_event_sink;` in lib.rs beside the existing `mod` declarations. Commit (`feat(zeb-445): NodeEventSink trait + fanout`), then run the three per-task gates. Expected: green.

---

### Task 2: De-Tauri the emission path (one compile/gate unit)

This is the big mechanical refactor. The compiler drives it: change the seam types, then fix every error. Do all four parts before gating; commit at the end of each part anyway (the tree may not compile between parts — that's accepted within this task, like ZEB-434 Tasks 4+5).

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (signature ~line 639 + 31 emit sites)
- Modify: `src-tauri/src/mail_sync.rs` (drop `<R>`)
- Modify: `src-tauri/src/community_channel_log_engine.rs` (drop `<R>`)
- Modify: `src-tauri/src/lib.rs` (start_node_inner ~2284, NodeState fields ~509-757, adapters, `resolve_app_data_dir`)

- [ ] **Step 1: Add the data-dir helper + trait adapters in lib.rs**

```rust
/// ZEB-445: Tauri-free equivalent of `app.path().app_data_dir()`. Tauri
/// resolves the app-data dir as `dirs::data_dir()/<identifier>` with
/// identifier "net.zeblith.harmony" (tauri.conf.json:5). serve mode must
/// resolve the IDENTICAL path or GUI and headless would split-brain the
/// profile.
pub(crate) fn resolve_app_data_dir() -> Result<std::path::PathBuf, String> {
    dirs::data_dir()
        .map(|d| d.join("net.zeblith.harmony"))
        .ok_or_else(|| "cannot resolve platform data dir".to_string())
}

impl crate::community_invite::AppHandleEmit for std::sync::Arc<dyn crate::node_event_sink::NodeEventSink> {
    fn emit_degraded(&self, community_id_hex: &str, reason_tag: &'static str) {
        self.emit(
            "community-state-sync-degraded",
            serde_json::json!({ "communityId": community_id_hex, "reason": reason_tag }),
        );
    }
}

impl crate::iroh_friend_acceptor::FriendEventEmit for std::sync::Arc<dyn crate::node_event_sink::NodeEventSink> {
    fn emit_friend_list_changed(&self) {
        self.emit("friend-list-changed", serde_json::Value::Null);
    }
    fn emit_friend_request_received(&self) {
        self.emit("friend-request-received", serde_json::Value::Null);
    }
}
```

(If `dirs` is not already a direct dependency — app_tracing.rs uses it, so it should be — add `dirs = "5"` to `[dependencies]` matching the version in Cargo.lock.)

Check the unit `()` impls of those traits still exist for tests; do not remove them. Note: `emit_friend_list_changed` currently emits payload `()` — Tauri serializes `()` as `null`, so `Value::Null` is wire-identical.

- [ ] **Step 2: De-genericize `MailSync` and `ChannelLogEngine`/`ChannelLogRegistry`**

Transformation rule (apply to mail_sync.rs and community_channel_log_engine.rs):
1. Remove `<R: tauri::Runtime>` / `<R: Runtime>` from every struct, impl, fn, and type in the file (`MailSync<R>` → `MailSync`; `ChannelLogEngine<R>` → `ChannelLogEngine`; same for `ChannelLogEngineParams`, `ChannelLogRegistryConfig`, `EngineEntry`, `CommunityTransactionGuard`, `ChannelLogRegistry`, `SpawnOutcome`).
2. Replace every field/param `app: AppHandle<R>` / `app: tauri::AppHandle<R>` (mail_sync.rs:71,80; community_channel_log_engine.rs:302,331,1117) with `sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>`.
3. Replace every `self.app.emit(name, &payload)` / `app.emit(name, payload)` with `crate::node_event_sink::emit_ser(&*self.sink, name, &payload)`. Where the old code checked the emit `Result` and warned (e.g. mail_sync.rs:637-638), delete the check — `emit_ser`/the AppHandle impl now log internally.
4. Remove the ZEB-307 `PhantomData<fn() -> R>` field (community_channel_log_engine.rs:146 region) and any `use tauri::{...Runtime...}` imports that become unused.
5. `app_handle_for_test` (community_channel_log_engine.rs:1058) becomes `sink_for_test(&self) -> &Arc<dyn NodeEventSink>`; fix its callers in tests.
6. `DfrostLogEngine`/`DfrostLogRegistry` (community_dfrost_log_engine.rs) are NOT touched — they stay `<R: tauri::Runtime>`, constructed only by voting commands via `NodeState.app_handle_wry` (out of v1 scope).

- [ ] **Step 3: De-Tauri `event_loop::run` and `start_node_inner`**

In event_loop.rs:
1. `pub async fn run<R: Runtime>(... app: AppHandle<R>, ... mail_sync: Option<Arc<crate::mail_sync::MailSync<R>>>, ...)` → drop the `<R: Runtime>` generic entirely; `app: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>`; `mail_sync: Option<Arc<crate::mail_sync::MailSync>>`.
2. Every `let _ = app.emit("name", payload);` / `app_late.emit(...)` / `app_for_task.emit(...)` etc. (31 sites, lines 454–5453 — verify with `grep -c "\.emit(" src/event_loop.rs`) becomes `crate::node_event_sink::emit_ser(&*app, "name", &payload);` (clones like `let app_late = app.clone();` keep working — `Arc` clones).
3. Any `R`-typed helper fns inside event_loop.rs (e.g. `process_inbound<R>`-style if present in this file) lose the generic the same way.

In lib.rs `start_node_inner` (2284):
1. Signature: `pub(crate) async fn start_node_inner(endpoint: Option<String>, app: &AppHandle, state: &Mutex<NodeState>)` → `pub(crate) async fn start_node_inner(endpoint: Option<String>, sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>, wry_handle: Option<tauri::AppHandle<tauri::Wry>>, state: &Mutex<NodeState>) -> Result<StartNodeResponse, String>`.
2. Line 2346 `app.path().app_data_dir()` → `crate::resolve_app_data_dir()?`.
3. Line 2770 `MailSync::new(..., app.clone())` → `MailSync::new(..., sink.clone())`.
4. The `event_loop::run(...)` spawn passes `sink.clone()` instead of the AppHandle clone.
5. Line ~6789-region `guard.app_handle_wry = Some(app.clone())` → `guard.app_handle_wry = wry_handle;` (serve passes `None`; voting/D-FROST commands then fail with their existing "app_handle_wry missing" error — correct degraded behavior).
6. The `zenoh-status` emit at ~7551 → `emit_ser(&*sink, ...)`.
7. ChannelLogRegistry/community-engine construction sites that passed `app.clone()` now pass `sink.clone()`; sites passing the AppHandle as `impl AppHandleEmit`/`FriendEventEmit` pass `sink.clone()` (the Task-2-Step-1 adapters make `Arc<dyn NodeEventSink>` satisfy both traits).
8. The `start_node` wrapper (2269) builds the sink from its AppHandle: `let sink: Arc<dyn NodeEventSink> = Arc::new(app.clone()); start_node_inner(endpoint, sink, Some(app.clone()), state.inner()).await` — note the wrapper's `app: AppHandle` is `AppHandle<Wry>` in production. The other internal caller (`owner_commands::mint_owner_identity` restart closure) gets the same treatment.
9. `stop_node` (7582) keeps emitting via its own AppHandle directly — unchanged in this task (Task 5 extracts its impl).

- [ ] **Step 4: Fix NodeState + remaining compile errors**

In lib.rs NodeState (509–757): `channel_log_registry: Option<Arc<ChannelLogRegistry<tauri::Wry>>>` → `Option<Arc<ChannelLogRegistry>>`. `app_handle_wry` stays as-is. Chase every remaining compiler error mechanically (`cargo check -p harmony-app --lib --features test-fixtures` in a loop); existing unit tests that constructed engines with `tauri::test::mock_app()` handles can now pass a `RecordingSink`-style stub or `Arc::new(())`? — no: implement the test fix per file using the `RecordingSink` from Task 1 or a local stub implementing `NodeEventSink`.

- [ ] **Step 5: Commit + gates**

Commit `refactor(zeb-445): NodeEventSink seam through event loop, MailSync, channel-log engines`. Run the three per-task gates. Expected: green (existing lib tests prove no behavior change). Verify zero leftover direct-AppHandle emits in the de-Tauri'd files: `grep -n "app.emit\|app_handle.emit" src/event_loop.rs src/mail_sync.rs src/community_channel_log_engine.rs` → expect no matches.

---

### Task 3: `*_impl` extraction A — communities + channels (11 commands)

**Files:** Modify `src-tauri/src/lib.rs` only.

Extraction recipe (NOT cfg-gated — unlike the `voting_*_impl` precedents at 32495+, these are production seams; the existing non-gated precedent is `move_content_impl`:11761):

For each command: move the entire body into `pub(crate) async fn <name>_impl(state: &std::sync::Mutex<NodeState>, [sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,] <args...>) -> <same return>`, replacing `state_lock.lock()` with `state.lock()` and any `app.emit(...)` with `crate::node_event_sink::emit_ser(&*sink, ...)`. The `#[tauri::command]` wrapper shrinks to a delegation:

- [ ] **Step 1: Worked example — `list_channels` (pure query, no sink param)**

```rust
#[tauri::command]
async fn list_channels(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
) -> Result<Vec<ChannelInfoDto>, String> {
    list_channels_impl(state_lock.inner(), community_id).await
}

pub(crate) async fn list_channels_impl(
    state: &std::sync::Mutex<NodeState>,
    community_id: String,
) -> Result<Vec<ChannelInfoDto>, String> {
    // ... exact former body, with `state_lock.lock()` → `state.lock()` ...
}
```

- [ ] **Step 2: Worked example — `create_community` (emits `nav-updated`, takes sink)**

```rust
#[tauri::command]
async fn create_community(
    app: tauri::AppHandle,
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    name: String,
    is_invite_only: bool,
) -> Result<String, String> {
    let sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink> =
        std::sync::Arc::new(app);
    create_community_impl(state_lock.inner(), sink, name, is_invite_only).await
}

pub(crate) async fn create_community_impl(
    state: &std::sync::Mutex<NodeState>,
    sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    name: String,
    is_invite_only: bool,
) -> Result<String, String> {
    // ... exact former body; the `app.emit("nav-updated", ...)` at ~18586
    // becomes crate::node_event_sink::emit_ser(&*sink, "nav-updated", &payload)
}
```

- [ ] **Step 3: Apply to the remaining nine**

Queries (state-only): `list_owner_communities`:15492, `list_community_members`:15606, `generate_invite`:17584, `create_channel`:16635, `list_channel_messages`:17426, `post_channel_message`:17346. With sink (they emit or call emitting inners): `redeem_invite`:20719 (nav-updated ~20854), `join_open_community`:20992, `leave_community`:24718. Where a command already delegates to an `*_inner` (e.g. `redeem_invite_inner`:19653) the `_impl` wraps the same call the wrapper used to make — do NOT restructure the inner.

- [ ] **Step 4: Commit + gates**

Commit `refactor(zeb-445): _impl seams for community + channel commands`. Gates green; existing unit tests at lib.rs:42841+/43610+ still pass (they exercise the same logic).

---

### Task 4: `*_impl` extraction B — friends + DMs (10 commands)

**Files:** Modify `src-tauri/src/lib.rs` only.

- [ ] **Step 1: Apply the Task 3 recipe**

State-only: `list_friends`:38455, `generate_friend_token`:38244, `list_pending_friend_requests`:39040, `send_dm`:8085, `read_dm_thread`:8387 (delegate through `read_dm_thread_inner`:8325 exactly as the wrapper does today). With sink: `redeem_friend_token`:38331, `add_friend_by_key`:39668, `accept_friend_request`:39061 (friend-list-changed ~39079), `decline_friend_request`, `add_space`:9036 (delegates `add_space_dm_inner`:8760; emits nav-updated). For commands using the `FriendEventEmit` trait generically, pass `sink.clone()` (Task 2 adapter satisfies the bound).

- [ ] **Step 2: Commit + gates**

Commit `refactor(zeb-445): _impl seams for friend + DM commands`. Gates green (friend serialization tests at 39800/40676 unaffected).

---

### Task 5: `*_impl` extraction C — lifecycle, identity, diagnostics (6 commands)

**Files:** Modify `src-tauri/src/lib.rs`, `src-tauri/src/owner_commands.rs`.

- [ ] **Step 1: lifecycle**

`start_node`'s RPC entry IS `start_node_inner` (post-Task 2) — no new extraction; rpc.rs will call it with the API sink and `wry_handle: None`. Extract `stop_node_impl(state: &Mutex<NodeState>, sink: Arc<dyn NodeEventSink>) -> Result<(), String>` from `stop_node`:7582: same body, the `zenoh-status` "disconnected" emit via `emit_ser`.

- [ ] **Step 2: identity + diagnostics**

`get_owner_state` (owner_commands.rs:160, `_app` param is unused): extract `pub(crate) async fn get_owner_state_impl(state: &Mutex<NodeState>) -> Result<Option<OwnerStateView>, String>`. State-only diagnostics: `connectivity_get_my_reachability_record`:40762, `connectivity_list_peer_reachability`:40808, `network_health_run_self_test`:40979 → straight recipe. `network_health_snapshot`:40933 takes `app: AppHandle<R>` — check its body: if the handle is unused (or used only for emit), extract with the standard recipe (sink if it emits, plain if not).

- [ ] **Step 3: Commit + gates**

Commit `refactor(zeb-445): _impl seams for lifecycle + identity + diagnostics`. Gates green.

---

### Task 6: deps + `api/auth.rs` + `api/lock.rs`

**Files:**
- Modify: `src-tauri/Cargo.toml`
- Create: `src-tauri/src/api/mod.rs` (skeleton: `pub mod auth; pub mod lock;` only for now), `src-tauri/src/api/auth.rs`, `src-tauri/src/api/lock.rs`
- Modify: `src-tauri/src/lib.rs` (`pub mod api;`)

- [ ] **Step 1: Cargo.toml**

```toml
# [dependencies] — ZEB-445 API control surface
axum = { version = "0.8", features = ["ws"] }
fd-lock = "4"
```

(axum 0.8.9 already resolves in Cargo.lock via dev-deps; this promotes it. tokio's existing `rt/net/time/sync` features suffice for axum on an existing runtime.)

- [ ] **Step 2: auth.rs**

```rust
// src-tauri/src/api/auth.rs — ZEB-445 bearer-token auth.
//
// Trust boundary = same user on the same machine (matches the keychain's).
// The token lives in a 0600 file so browser pages (which CAN open localhost
// WebSockets without CORS preflight) cannot obtain it.

use rand::RngCore;
use std::path::Path;

pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub fn write_token_file(dir: &Path, token: &str) -> Result<std::path::PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let path = dir.join("token");
    std::fs::write(&path, token).map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    }
    // Windows: user-profile default ACLs already restrict to the owner.
    Ok(path)
}

/// Constant-time-ish comparison is unnecessary here (local trust boundary,
/// 256-bit random token), plain equality is fine.
pub fn check_bearer(expected: &str, header_value: Option<&str>) -> bool {
    matches!(header_value, Some(h) if h.strip_prefix("Bearer ") == Some(expected))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_64_hex_chars_and_random() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    #[cfg(unix)]
    fn token_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = write_token_file(dir.path(), "t").unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn bearer_check_accepts_exact_and_rejects_everything_else() {
        assert!(check_bearer("abc", Some("Bearer abc")));
        assert!(!check_bearer("abc", Some("Bearer abd")));
        assert!(!check_bearer("abc", Some("abc")));
        assert!(!check_bearer("abc", None));
    }
}
```

(`tempfile` — confirm it is already a dev-dependency; mint_owner_lifecycle.rs uses TempDir. If it's only in dev-deps that's fine: these are `#[cfg(test)]` tests. `hex` is already a dependency — used throughout lib.rs.)

- [ ] **Step 3: lock.rs**

```rust
// src-tauri/src/api/lock.rs — ZEB-445 one-node-per-profile lock.
//
// fd-lock = OS advisory lock: released automatically on process death, so
// stale-lock reclaim needs no PID-liveness logic. The file CONTENT (pid) is
// purely for the human-readable refusal message.

use fd_lock::RwLock;
use std::io::Write;
use std::path::Path;

pub struct ProfileLock {
    // Held for the process lifetime; dropping releases the OS lock.
    _guard: fd_lock::RwLockWriteGuard<'static, std::fs::File>,
}

pub fn acquire(dir: &Path) -> Result<ProfileLock, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let path = dir.join("serve.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    // One leaked RwLock per acquire is intentional: the lock is held for the
    // full process lifetime by design, and the OS releases it on process
    // death (that's the stale-reclaim story — no PID-liveness logic needed).
    // Dropping the returned ProfileLock still releases the OS lock (tests).
    let lock: &'static mut RwLock<std::fs::File> = Box::leak(Box::new(RwLock::new(file)));
    let guard = lock.try_write().map_err(|_| read_holder_message(&path))?;
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| format!("rewrite {}: {e}", path.display()))?;
        let _ = writeln!(f, "{}", std::process::id());
    }
    Ok(ProfileLock { _guard: guard })
}

fn read_holder_message(path: &Path) -> String {
    let holder = std::fs::read_to_string(path).unwrap_or_default();
    format!(
        "profile already in use (lock {}, holder pid {}): another harmony-app \
         (serve or GUI-with-API) owns this profile; stop it first",
        path.display(),
        holder.trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_fails_while_held_then_succeeds_after_drop() {
        let dir = tempfile::tempdir().unwrap();
        let l1 = acquire(dir.path()).expect("first acquire");
        let e = acquire(dir.path()).expect_err("second must refuse");
        assert!(e.contains("already in use"), "got: {e}");
        drop(l1);
        let _l2 = acquire(dir.path()).expect("reacquire after drop");
    }
}
```

- [ ] **Step 4: Commit + gates**

`pub mod api;` in lib.rs; `api/mod.rs` contains only the two `pub mod`s for now. Commit `feat(zeb-445): api auth token + profile lock`. Gates green.

---

### Task 7: `api/events.rs` — frames, sink, WS handler

**Files:** Create `src-tauri/src/api/events.rs`; modify `src-tauri/src/api/mod.rs` (`pub mod events;`).

- [ ] **Step 1: Write module + tests**

```rust
// src-tauri/src/api/events.rs — ZEB-445 WS event firehose.

use crate::node_event_sink::NodeEventSink;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Debug, serde::Serialize)]
pub struct EventFrame {
    pub seq: u64,
    pub event: String,
    pub payload: serde_json::Value,
}

/// Bounded: a slow WS client lags and gets an explicit `_lagged` frame
/// rather than stalling the node or silently dropping.
pub const EVENT_CHANNEL_CAPACITY: usize = 1024;

pub struct ApiEventSink {
    tx: tokio::sync::broadcast::Sender<EventFrame>,
    seq: AtomicU64,
}

impl ApiEventSink {
    pub fn new() -> std::sync::Arc<Self> {
        let (tx, _) = tokio::sync::broadcast::channel(EVENT_CHANNEL_CAPACITY);
        std::sync::Arc::new(Self { tx, seq: AtomicU64::new(0) })
    }
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<EventFrame> {
        self.tx.subscribe()
    }
}

impl NodeEventSink for std::sync::Arc<ApiEventSink> {
    fn emit(&self, event: &str, payload: serde_json::Value) {
        let frame = EventFrame {
            seq: self.seq.fetch_add(1, Ordering::Relaxed),
            event: event.to_string(),
            payload,
        };
        // No subscribers is fine (send returns Err) — fire-and-forget.
        let _ = self.tx.send(frame);
    }
}

/// Forward frames to one WS connection until it closes. On lag, emit the
/// explicit `_lagged` marker frame and continue from the live edge.
pub async fn forward_events(
    mut rx: tokio::sync::broadcast::Receiver<EventFrame>,
    mut ws: axum::extract::ws::WebSocket,
) {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        match rx.recv().await {
            Ok(frame) => {
                let txt = match serde_json::to_string(&frame) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                if ws.send(axum::extract::ws::Message::Text(txt.into())).await.is_err() {
                    return; // client gone
                }
            }
            Err(RecvError::Lagged(missed)) => {
                let marker = serde_json::json!({
                    "seq": serde_json::Value::Null,
                    "event": "_lagged",
                    "payload": { "missed": missed },
                });
                if ws
                    .send(axum::extract::ws::Message::Text(marker.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(RecvError::Closed) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_event_sink::NodeEventSink as _;

    #[tokio::test]
    async fn seq_is_monotonic_from_zero() {
        let sink = ApiEventSink::new();
        let mut rx = sink.subscribe();
        sink.emit("a", serde_json::json!(1));
        sink.emit("b", serde_json::json!(2));
        assert_eq!(rx.recv().await.unwrap().seq, 0);
        assert_eq!(rx.recv().await.unwrap().seq, 1);
    }

    #[tokio::test]
    async fn lag_is_reported_not_silent() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<EventFrame>(1);
        tx.send(EventFrame { seq: 0, event: "x".into(), payload: serde_json::Value::Null }).unwrap();
        tx.send(EventFrame { seq: 1, event: "y".into(), payload: serde_json::Value::Null }).unwrap();
        // capacity 1 → first recv reports the lag
        match rx.recv().await {
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => assert_eq!(n, 1),
            other => panic!("expected Lagged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn emit_with_no_subscribers_does_not_panic() {
        let sink = ApiEventSink::new();
        sink.emit("a", serde_json::json!(1));
    }
}
```

- [ ] **Step 2: Commit + gates**

Commit `feat(zeb-445): WS event frames + broadcast sink`. Gates green.

---

### Task 8: `api/rpc.rs` — registry, dispatch, the 27 registrations

**Files:** Create `src-tauri/src/api/rpc.rs`; modify `src-tauri/src/api/mod.rs` (`pub mod rpc;`).

- [ ] **Step 1: Registry core + error mapping + tests**

```rust
// src-tauri/src/api/rpc.rs — ZEB-445 uniform RPC: POST /v1/rpc/{command}.
//
// Same command names, same camelCase JSON args, same DTOs, same error
// strings as the Tauri IPC — one mental model across GUI and API.

use crate::node_event_sink::NodeEventSink;
use crate::NodeState;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub enum RpcError {
    UnknownCommand,
    BadArgs(String),
    Command(String),
}

pub type RpcFuture = Pin<Box<dyn Future<Output = Result<serde_json::Value, RpcError>> + Send>>;
pub type RpcHandler =
    Box<dyn Fn(Arc<Mutex<NodeState>>, Arc<dyn NodeEventSink>, serde_json::Value) -> RpcFuture + Send + Sync>;

pub struct RpcRegistry {
    handlers: HashMap<&'static str, RpcHandler>,
}

impl RpcRegistry {
    pub async fn dispatch(
        &self,
        command: &str,
        state: Arc<Mutex<NodeState>>,
        sink: Arc<dyn NodeEventSink>,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        let h = self.handlers.get(command).ok_or(RpcError::UnknownCommand)?;
        h(state, sink, args).await
    }

    pub fn command_names(&self) -> Vec<&'static str> {
        let mut v: Vec<_> = self.handlers.keys().copied().collect();
        v.sort_unstable();
        v
    }
}

/// Registration macro. `$args_ty` derives Deserialize with camelCase renames
/// (matching what the frontend sends over Tauri IPC). An empty/absent body
/// deserializes into arg structs whose fields are all Option/defaulted, or
/// `EmptyArgs`.
macro_rules! rpc {
    ($map:expr, $name:literal, $args_ty:ty, |$state:ident, $sink:ident, $args:ident| $call:expr) => {
        $map.insert(
            $name,
            Box::new(
                move |$state: Arc<Mutex<NodeState>>,
                      $sink: Arc<dyn NodeEventSink>,
                      raw: serde_json::Value| {
                    Box::pin(async move {
                        let raw = if raw.is_null() { serde_json::json!({}) } else { raw };
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

#[derive(serde::Deserialize)]
pub struct EmptyArgs {}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommunityIdArgs {
    pub community_id: String,
}
// ... one small arg struct per distinct shape; see Step 2 list ...

pub fn build_registry() -> RpcRegistry {
    let mut m: HashMap<&'static str, RpcHandler> = HashMap::new();

    // ---- lifecycle ----
    rpc!(m, "stop_node", EmptyArgs, |state, sink, _a| crate::stop_node_impl(&state, sink));
    // start_node: special-cased because start_node_inner takes &Mutex not Arc:
    rpc!(m, "start_node", StartNodeArgs, |state, sink, a| async move {
        crate::start_node_inner(a.endpoint, sink, None, &state).await
    });

    // ---- queries (state-only) ----
    rpc!(m, "list_channels", CommunityIdArgs, |state, _sink, a| crate::list_channels_impl(
        &state,
        a.community_id
    ));
    // ... etc for every state-only command ...

    // ---- mutations with sink ----
    rpc!(m, "create_community", CreateCommunityArgs, |state, sink, a| {
        crate::create_community_impl(&state, sink, a.name, a.is_invite_only)
    });
    // ... etc ...

    RpcRegistry { handlers: m }
}
```

NOTE on the closure body: `&state` above is `&Arc<Mutex<NodeState>>` which derefs to `&Mutex<NodeState>` — the `_impl` signatures take `&Mutex<NodeState>`, so `&state` works directly.

- [ ] **Step 2: Register all 27 with their arg structs**

Arg structs (all `#[derive(serde::Deserialize)] #[serde(rename_all = "camelCase")]`) mirror the wrapper params exactly — copy each wrapper's param list:
`StartNodeArgs { endpoint: Option<String> }`; `CreateCommunityArgs { name: String, is_invite_only: bool }`; `GenerateInviteArgs { community_id: String, invitee_hint: Option<String>, expires_at: Option<u64> }`; `RedeemInviteArgs { url: String }`; `CreateChannelArgs { community_id: String, name: String, write_power: u8, kind: Option<String> }`; `ListChannelMessagesArgs { community_id: String, channel_id: String, since: Option<HlcDto>, limit: u32 }`; `PostChannelMessageArgs { community_id: String, channel_id: String, body: Vec<u8>, reply_to: Option<String> }`; `SendDmArgs { space_id: String, content: Vec<u8>, mime_type: String }`; `ReadDmThreadArgs { space_id: String, limit: usize, before_hlc: Option<u64> }`; `OwnerIdHexArgs { owner_id_hex: String }` (accept/decline friend request); plus the `add_space`, `generate_friend_token`, `redeem_friend_token`, `add_friend_by_key`, `join_open_community`, `leave_community`, `list_community_members` shapes copied from their wrappers. Commands with no args use `EmptyArgs`: `list_owner_communities`, `list_friends`, `list_pending_friend_requests`, `get_owner_state`, `connectivity_get_my_reachability_record`, `connectivity_list_peer_reachability`, `network_health_snapshot`, `network_health_run_self_test`.

- [ ] **Step 3: Unit tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_event_sink::FanoutSink;

    fn sink() -> Arc<dyn NodeEventSink> {
        Arc::new(FanoutSink(vec![]))
    }

    #[tokio::test]
    async fn unknown_command_is_distinct_from_command_error() {
        let r = build_registry();
        let state = Arc::new(Mutex::new(NodeState::default()));
        match r.dispatch("no_such_cmd", state, sink(), serde_json::json!({})).await {
            Err(RpcError::UnknownCommand) => {}
            other => panic!("expected UnknownCommand, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bad_args_reports_serde_message() {
        let r = build_registry();
        let state = Arc::new(Mutex::new(NodeState::default()));
        match r
            .dispatch("list_channels", state, sink(), serde_json::json!({"communityId": 42}))
            .await
        {
            Err(RpcError::BadArgs(msg)) => assert!(msg.contains("expected a string") || !msg.is_empty()),
            other => panic!("expected BadArgs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn command_error_passes_through_ipc_error_string() {
        // list_channels on a default NodeState fails with the same string the
        // GUI would see (owner not loaded) — proves error-parity.
        let r = build_registry();
        let state = Arc::new(Mutex::new(NodeState::default()));
        match r
            .dispatch(
                "list_channels",
                state,
                sink(),
                serde_json::json!({"communityId": "00".repeat(16)}),
            )
            .await
        {
            Err(RpcError::Command(msg)) => assert!(!msg.is_empty()),
            other => panic!("expected Command error, got {other:?}"),
        }
    }

    #[test]
    fn registry_has_exactly_the_curated_v1_surface() {
        let names = build_registry().command_names();
        assert_eq!(names.len(), 27, "curated v1 surface drifted: {names:?}");
        for required in ["start_node", "stop_node", "get_owner_state", "create_community",
            "redeem_invite", "post_channel_message", "send_dm", "read_dm_thread",
            "network_health_run_self_test"] {
            assert!(names.contains(&required), "missing {required}");
        }
    }
}
```

- [ ] **Step 4: Commit + gates**

Commit `feat(zeb-445): RPC registry with curated 27-command surface`. Gates green.

---

### Task 9: server assembly + `serve` subcommand

**Files:**
- Modify: `src-tauri/src/api/mod.rs` (full server), `src-tauri/src/lib.rs` (`serve_cli`), `src-tauri/src/main.rs` (clap variant), `src-tauri/src/app_tracing.rs` (`init_serve_tracing`)

- [ ] **Step 1: `api/mod.rs` server**

```rust
// src-tauri/src/api/mod.rs — ZEB-445 localhost control surface.
pub mod auth;
pub mod events;
pub mod lock;
pub mod rpc;

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

#[derive(Clone)]
pub struct ApiCtx {
    pub state: Arc<Mutex<NodeState>>,
    pub sink: Arc<dyn NodeEventSink>,
    pub events: Arc<events::ApiEventSink>,
    pub registry: Arc<rpc::RpcRegistry>,
    pub token: Arc<String>,
    pub started: std::time::Instant,
    pub bound_port: u16,
    pub shutdown_tx: tokio::sync::mpsc::Sender<()>,
}

fn unauthorized() -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "unauthorized"})))
}

fn authed(ctx: &ApiCtx, headers: &axum::http::HeaderMap) -> bool {
    auth::check_bearer(
        &ctx.token,
        headers.get(axum::http::header::AUTHORIZATION).and_then(|v| v.to_str().ok()),
    )
}

async fn rpc_handler(
    State(ctx): State<ApiCtx>,
    Path(command): Path<String>,
    headers: axum::http::HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> impl IntoResponse {
    if !authed(&ctx, &headers) {
        return unauthorized();
    }
    let args = body.map(|Json(v)| v).unwrap_or(serde_json::Value::Null);
    match ctx.registry.dispatch(&command, ctx.state.clone(), ctx.sink.clone(), args).await {
        Ok(v) => (StatusCode::OK, Json(v)),
        Err(rpc::RpcError::UnknownCommand) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "unknown command"})),
        ),
        Err(rpc::RpcError::BadArgs(m)) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": m}))),
        Err(rpc::RpcError::Command(m)) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": m})))
        }
    }
}

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

async fn status_handler(State(ctx): State<ApiCtx>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    if !authed(&ctx, &headers) {
        return unauthorized();
    }
    let (running, generation, owner_id) = {
        let g = match ctx.state.lock() {
            Ok(g) => g,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": format!("NodeState poisoned: {e}")})),
                )
            }
        };
        (g.node_is_running(), g.generation_for_status(), g.owner_id_hex_for_status())
    };
    (
        StatusCode::OK,
        Json(serde_json::to_value(StatusDto {
            running,
            generation,
            owner_id,
            uptime_secs: ctx.started.elapsed().as_secs(),
            port: ctx.bound_port,
            version: env!("CARGO_PKG_VERSION"),
        })
        .expect("StatusDto serializes")),
    )
}

async fn shutdown_handler(State(ctx): State<ApiCtx>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    if !authed(&ctx, &headers) {
        return unauthorized();
    }
    let _ = ctx.shutdown_tx.send(()).await;
    (StatusCode::OK, Json(serde_json::json!({"shuttingDown": true})))
}

async fn events_handler(
    State(ctx): State<ApiCtx>,
    headers: axum::http::HeaderMap,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    if !authed(&ctx, &headers) {
        return unauthorized().into_response();
    }
    let rx = ctx.events.subscribe();
    ws.on_upgrade(move |socket| events::forward_events(rx, socket))
}

pub fn router(ctx: ApiCtx) -> Router {
    Router::new()
        .route("/v1/rpc/{command}", post(rpc_handler))
        .route("/v1/status", get(status_handler))
        .route("/v1/shutdown", post(shutdown_handler))
        .route("/v1/events", get(events_handler))
        .with_state(ctx)
}

/// Bind 127.0.0.1:<port>, write discovery files, serve until `shutdown_rx`.
/// Returns the bound port via the oneshot once listening.
pub struct ServerHandle {
    pub bound_port: u16,
    pub api_dir: std::path::PathBuf,
}

pub async fn start_server(
    data_dir: &std::path::Path,
    requested_port: u16,
    state: Arc<Mutex<NodeState>>,
    sink: Arc<dyn NodeEventSink>,
    events: Arc<events::ApiEventSink>,
    shutdown_tx: tokio::sync::mpsc::Sender<()>,
    mut shutdown_rx: tokio::sync::mpsc::Receiver<()>,
) -> Result<(ServerHandle, tokio::task::JoinHandle<()>), String> {
    let api_dir = data_dir.join("api");
    let token = auth::generate_token();
    auth::write_token_file(&api_dir, &token)?;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", requested_port))
        .await
        .map_err(|e| format!("bind 127.0.0.1:{requested_port}: {e}"))?;
    let bound_port = listener.local_addr().map_err(|e| e.to_string())?.port();
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
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.recv().await;
            })
            .await;
    });
    Ok((ServerHandle { bound_port, api_dir }, task))
}
```

NodeState helper methods (add in lib.rs near NodeState impl): `pub(crate) fn node_is_running(&self) -> bool { self.thread.is_some() }`, `pub(crate) fn generation_for_status(&self) -> u64 { self.generation }`, `pub(crate) fn owner_id_hex_for_status(&self) -> Option<String>` (hex of `dm_self_owner` if set — match the existing field type; see NodeState 509-757).

- [ ] **Step 2: `serve_cli` in lib.rs + tracing + main.rs**

app_tracing.rs — add:

```rust
/// ZEB-445 serve mode: stderr + rolling file, stdout stays pure.
pub fn init_serve_tracing() {
    install_subscriber_stderr(log_dir());
}
```

(implement `install_subscriber_stderr` as a sibling of `install_subscriber` whose fmt layer writes to `std::io::stderr` instead of stdout; same file layer + EnvFilter.)

lib.rs — `serve_cli` (public, called from main.rs):

```rust
/// ZEB-445: headless serve mode. Returns the process exit code.
pub fn serve_cli(api_port: Option<u16>) -> i32 {
    crate::app_tracing::init_serve_tracing();
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("serve: tokio runtime: {e}");
            return 1;
        }
    };
    rt.block_on(async move {
        let data_dir = match resolve_app_data_dir() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("serve: {e}");
                return 1;
            }
        };
        let _lock = match crate::api::lock::acquire(&data_dir.join("api")) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("serve: {e}");
                return 1;
            }
        };
        let state = std::sync::Arc::new(std::sync::Mutex::new(NodeState::default()));
        let events = crate::api::events::ApiEventSink::new();
        let sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink> =
            std::sync::Arc::new(events.clone());

        if let Err(e) = start_node_inner(None, sink.clone(), None, &state).await {
            eprintln!("serve: node start failed: {e}");
            return 1;
        }

        let port = api_port
            .or_else(|| std::env::var("HARMONY_API_PORT").ok().and_then(|v| v.parse().ok()))
            .unwrap_or(7420);
        let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (handle, server_task) = match crate::api::start_server(
            &data_dir, port, state.clone(), sink, events, shutdown_tx.clone(), shutdown_rx,
        )
        .await
        {
            Ok(x) => x,
            Err(e) => {
                eprintln!("serve: {e}");
                stop_inner(&state, None);
                return 1;
            }
        };
        tracing::info!(port = handle.bound_port, "harmony serve listening on 127.0.0.1");

        // Wait for /v1/shutdown (closes the server) or SIGINT/SIGTERM.
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        #[cfg(unix)]
        tokio::select! {
            _ = ctrl_c => { let _ = shutdown_tx.send(()).await; }
            _ = sigterm.recv() => { let _ = shutdown_tx.send(()).await; }
            _ = server_task => {}
        }
        #[cfg(not(unix))]
        tokio::select! {
            _ = ctrl_c => { let _ = shutdown_tx.send(()).await; }
            _ = server_task => {}
        }

        stop_inner(&state, None);
        let _ = std::fs::remove_file(handle.api_dir.join("port"));
        let _ = std::fs::remove_file(handle.api_dir.join("token"));
        0
    })
}
```

NOTE: the `#[cfg(unix)]` select awaits `server_task` by value in one branch but `shutdown` path also needs it awaited — after the select, add `// server task winds down via graceful_shutdown; give it a bounded join:` `let _ = tokio::time::timeout(std::time::Duration::from_secs(10), async {}).await;` — implementer: restructure so `server_task` is awaited exactly once (e.g. select on signals only, then `let _ = server_task.await;` after sending shutdown). Keep it simple and correct; a small `async fn wait_for_exit_signal()` helper that returns on any of ctrl_c/sigterm makes the select clean.

main.rs — add the variant + dispatch:

```rust
    /// Run a windowless node exposing the localhost HTTP+WS control surface
    /// (ZEB-445). Token + bound port are written to <data-dir>/api/.
    Serve {
        /// Port for the API server (default 7420; 0 = OS-assigned).
        #[arg(long, value_name = "PORT")]
        api_port: Option<u16>,
    },
```

```rust
        Some(Command::Serve { api_port }) => {
            std::process::exit(harmony_app::serve_cli(api_port));
        }
```

(`Serve` does NOT call main.rs's `init_tracing()` — `serve_cli` installs its own stderr+file subscriber.)

- [ ] **Step 3: GUI is NOT wired in this PR**

Confirm `run()` is untouched (GUI opt-in is PR 2 / the follow-up child ticket). The fan-out sink exists (Task 1) but nothing constructs it yet — that's expected; do not add dead wiring.

- [ ] **Step 4: Commit + gates**

Commit `feat(zeb-445): harmony-app serve — localhost HTTP+WS control surface`. Gates green. Also verify the binary builds: `cargo build -p harmony-app --bin harmony-app` (10-min kill switch; if the bin target name differs, check `[[bin]]` in Cargo.toml / src/main.rs convention).

---

### Task 10: integration test `tests/api_server.rs`

**Files:**
- Create: `src-tauri/tests/api_server.rs`
- Modify: `src-tauri/Cargo.toml` dev-deps: add `reqwest = { version = "0.12", default-features = false, features = ["json"] }` and `tokio-tungstenite = "0.24"` (0.24 already in Cargo.lock as transitive).

- [ ] **Step 1: Write the test**

One `#[tokio::test(flavor = "multi_thread")]` test fn (env vars are process-global; a single fn avoids ordering hazards — nextest runs this binary in its own process):

```rust
// tests/api_server.rs — ZEB-445: end-to-end proof the node boots Tauri-free
// and is drivable over the localhost API. Temp-HOME, keychain-hermetic
// (ZEB-428: HARMONY_PASSPHRASE file-store path; KeychainStore refuses in
// test builds).
mod common; // EnvVarGuard

use common::EnvVarGuard;

#[tokio::test(flavor = "multi_thread")]
async fn serve_core_drives_full_flow_over_http_and_ws() {
    let home = tempfile::tempdir().unwrap();
    let _g1 = EnvVarGuard::set("HOME", home.path());
    let _g2 = EnvVarGuard::set("USERPROFILE", home.path());
    let _g3 = EnvVarGuard::set_str("HARMONY_PASSPHRASE", "api-server-test-pp");

    // ZEB-347: first iroh bind per process pays a one-time global init
    // (~10s CI / ~30s macOS) — warm it up before any asserted timeout.
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    // Boot the serve core in-process on an ephemeral port.
    let state = std::sync::Arc::new(std::sync::Mutex::new(harmony_app::NodeState::default()));
    let events = harmony_app::api::events::ApiEventSink::new();
    let sink: std::sync::Arc<dyn harmony_app::node_event_sink::NodeEventSink> =
        std::sync::Arc::new(events.clone());
    harmony_app::start_node_inner_for_test(None, sink.clone(), &state)
        .await
        .expect("headless node boots without Tauri");

    let data_dir = harmony_app::resolve_app_data_dir_for_test().expect("data dir");
    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);
    let (handle, _task) = harmony_app::api::start_server(
        &data_dir, 0, state.clone(), sink, events, shutdown_tx.clone(), shutdown_rx,
    )
    .await
    .expect("server binds ephemeral port");

    let token = std::fs::read_to_string(handle.api_dir.join("token")).unwrap();
    let port: u16 = std::fs::read_to_string(handle.api_dir.join("port"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(port, handle.bound_port, "port discovery file matches bound port");
    let base = format!("http://127.0.0.1:{port}");
    let http = reqwest::Client::new();

    // 1) auth is enforced — status without token is 401.
    let r = http.get(format!("{base}/v1/status")).send().await.unwrap();
    assert_eq!(r.status(), 401);

    // 2) status with token: running, owner minted at first boot.
    let bearer = format!("Bearer {}", token.trim());
    let r = http
        .get(format!("{base}/v1/status"))
        .header("authorization", &bearer)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let status: serde_json::Value = r.json().await.unwrap();
    assert_eq!(status["running"], true);

    // 3) connect the WS firehose (auth on handshake) BEFORE acting.
    let ws_req = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        format!("ws://127.0.0.1:{port}/v1/events"),
    )
    .map(|mut req| {
        req.headers_mut()
            .insert("authorization", bearer.parse().unwrap());
        req
    })
    .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_req).await.expect("ws connects");

    // 4) unknown command → 404; bad args → 400.
    let r = http
        .post(format!("{base}/v1/rpc/no_such_command"))
        .header("authorization", &bearer)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // 5) full flow: get_owner_state → create_community → create_channel →
    //    post → read back.
    let rpc = |cmd: &str, body: serde_json::Value| {
        let http = http.clone();
        let base = base.clone();
        let bearer = bearer.clone();
        let cmd = cmd.to_string();
        async move {
            http.post(format!("{base}/v1/rpc/{cmd}"))
                .header("authorization", &bearer)
                .json(&body)
                .send()
                .await
                .unwrap()
        }
    };

    let r = rpc("get_owner_state", serde_json::json!({})).await;
    assert_eq!(r.status(), 200);
    let owner: serde_json::Value = r.json().await.unwrap();
    assert!(owner["ownerId"].is_string(), "owner minted at first boot: {owner}");

    let r = rpc("create_community", serde_json::json!({"name": "api-e2e", "isInviteOnly": false})).await;
    assert_eq!(r.status(), 200);
    let community_id: String = r.json().await.unwrap();

    let r = rpc(
        "create_channel",
        serde_json::json!({"communityId": community_id, "name": "general", "writePower": 0, "kind": "text"}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let channel_id: String = r.json().await.unwrap();

    let body_bytes: Vec<u8> = b"hello headless".to_vec();
    let r = rpc(
        "post_channel_message",
        serde_json::json!({"communityId": community_id, "channelId": channel_id, "body": body_bytes, "replyTo": null}),
    )
    .await;
    assert_eq!(r.status(), 200);

    let r = rpc(
        "list_channel_messages",
        serde_json::json!({"communityId": community_id, "channelId": channel_id, "since": null, "limit": 10}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let msgs: serde_json::Value = r.json().await.unwrap();
    assert_eq!(msgs.as_array().unwrap().len(), 1, "posted message reads back: {msgs}");

    // 6) the WS saw real frames (zenoh-status from boot and/or nav-updated
    //    from create_community), with monotonic seq.
    use futures_util::StreamExt;
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline && seen.len() < 1 {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t)))) => {
                let f: serde_json::Value = serde_json::from_str(&t).unwrap();
                seen.push(f);
            }
            _ => break,
        }
    }
    assert!(
        seen.iter().any(|f| f["event"] == "nav-updated" || f["event"] == "zenoh-status"),
        "expected at least one real event frame, got: {seen:?}"
    );

    // 7) shutdown endpoint closes the server.
    let r = http
        .post(format!("{base}/v1/shutdown"))
        .header("authorization", &bearer)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}
```

Adjustments the implementer must make from ground truth, not guesses: (a) `start_node_inner` is `pub(crate)` — export a thin `pub` test seam in lib.rs gated `#[cfg(any(test, feature = "test-fixtures"))]` named `start_node_inner_for_test` (and `resolve_app_data_dir_for_test`) OR make `start_node_inner`/`resolve_app_data_dir` `pub` directly (preferred if clippy allows: they're behind the lib boundary, document with a ZEB-445 comment). (b) `create_community` returns the community id as a plain JSON string — confirm DTO shape from the wrapper return type (`Result<String, String>` → JSON string). (c) `get_owner_state` returns `Option<OwnerStateView>` — `null` until minted; if mint hasn't completed by first call, poll with a bounded retry loop (15s) instead of asserting immediately. (d) `futures_util` — tokio-tungstenite re-exports a stream API requiring futures-util; add `futures-util = "0.3"` to dev-deps if not already transitive-accessible. (e) WS payload `Message::Text` wraps `Utf8Bytes` in 0.24 — adjust pattern match accordingly.

- [ ] **Step 2: Run the integration test**

```bash
cargo nextest run -p harmony-app --test api_server --features test-fixtures
```
Expected: PASS (1 test). Budget note: compile+link of one integration binary is minutes, not the 25-min all-targets relink. 10-min kill switch applies to the RUN; if compile exceeds it, commit first and report DONE_WITH_CONCERNS with the timing.

- [ ] **Step 3: Commit**

`test(zeb-445): end-to-end api_server integration — headless boot + RPC + WS`

---

### Task 11: docs + full sweep

**Files:**
- Modify: `docs/headless-install.md` (new "API control surface (serve mode)" section: how to start `harmony-app serve`, where token/port live, curl + websocat examples, the single-profile caveat + plain-GUI lock caveat, ZEB-450 warning pointer), `docs/troubleshooting.md` (one pointer under "Window + app lifecycle": serve mode quits via `POST /v1/shutdown` or SIGTERM, never by closing a window)

- [ ] **Step 1: Write the docs section**

Include a copy-pasteable agent quickstart:

```bash
harmony-app serve &
TOKEN=$(cat "$HOME/Library/Application Support/net.zeblith.harmony/api/token")
PORT=$(cat "$HOME/Library/Application Support/net.zeblith.harmony/api/port")
curl -s -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:$PORT/v1/status"
curl -s -H "Authorization: Bearer $TOKEN" -X POST \
  -H 'Content-Type: application/json' -d '{"name":"test","isInviteOnly":false}' \
  "http://127.0.0.1:$PORT/v1/rpc/create_community"
```

(plus the Windows `%APPDATA%\net.zeblith.harmony\api\` path variant and a websocat one-liner for `/v1/events`.)

- [ ] **Step 2: Full sweep (the only --all-targets gate)**

```bash
set -o pipefail
cargo fmt --all -- --check
cargo clippy -p harmony-app --lib --features test-fixtures -- -D warnings
cargo nextest run -p harmony-app --lib --features test-fixtures
cargo nextest run -p harmony-app --test api_server --features test-fixtures
cargo check --locked --all-targets --features test-fixtures
```
Known-flake allowance: ZEB-420/383/374 orphan transport flakes in OTHER test binaries are out of scope (we only run lib + api_server here; the check is compile-only).

- [ ] **Step 3: Commit**

`docs(zeb-445): serve-mode API quickstart + lifecycle notes`

---

## After all tasks (controller)

Final code review → push branch → open PR. **PR body references ONLY ZEB-445** (no other ZEB-NNN anywhere in the body — Linear's GitHub integration closes EVERY referenced issue on merge; no closing keywords). Never write the at-mention form of greptile (plain "Greptile" only; never trigger it). Then the autonomous bot+CI convergence loop: scan all three comment buckets (inline review threads + PR issue-comments + PR reviews), ScheduleWakeup self-pacing (never Bash-sleep), one push per review round with visible hold-push signals, wait for CI green AND bot convergence, pushover Jake at ready-to-merge via `~/work/pushover-notify.sh`. Do NOT merge (Jake's gate).
