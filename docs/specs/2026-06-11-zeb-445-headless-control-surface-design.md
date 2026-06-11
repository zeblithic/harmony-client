# ZEB-445: Headless Agent Control Surface — Design

**Date:** 2026-06-11
**Ticket:** ZEB-445 (parent epic ZEB-451; consumers ZEB-446, ZEB-447)
**Status:** Approved by Jake 2026-06-11
**Branch:** `zeb-445-headless-control-surface` (off main `71c2c7c4`)

## Goal

Run harmony-client without a window and drive it through a local programmatic API, so any agent or tool on the same machine can mint/start/join/send/read/observe-events against a node first-class — no WebView/CDP required. The same API is available, opt-in, on a GUI instance, so the scenario suite (ZEB-447) drives dev GUI instances through the identical surface and Playwright/CDP shrinks to visual assertions only.

**Definition of done (from the ticket):** an agent on the same machine can mint/start/join/send/read/observe-events against a windowless node, and the scripted E2E scenario suite can run a full two-sided scenario over this surface instead of CDP.

## Settled decisions

| # | Decision | Choice (settled with Jake 2026-06-11) |
|---|----------|----------------------------------------|
| 1 | Transport | localhost HTTP (commands) + WebSocket (events) — settled pre-design |
| 2 | Process model | **No-Tauri `serve` subcommand.** Headless mode boots identity + event loop directly (the path integration tests already prove); no Tauri runtime, no WebView/GTK at runtime. |
| 3 | GUI parity | **Yes, opt-in.** The API server is a mode-agnostic module; GUI mode runs it when enabled. |
| 4 | API shape | **Curated set, uniform RPC:** `POST /v1/rpc/{command}` with the same JSON args/DTOs as the Tauri IPC, backed by a registry of inner fns. |
| 5 | Auth | **Token file in the data dir**, bearer-checked on every HTTP request and the WS handshake. |
| 6 | Vault sequencing | **v1 is keychain-backed, single profile per machine.** ZEB-449 (file vault) stays separate and is the prereq for multi-profile (ZEB-446) and RPi5/CI. |
| 7 | Event stream | Firehose (no server-side filtering), monotonic `seq`, explicit lag frame, no replay. |
| 8 | Port/discovery | Default **7420**, override via `--api-port` / `HARMONY_API_PORT`, `0` = ephemeral; bound port + token written to discovery files in the data dir. |
| 9 | Lifecycle | `serve` auto-starts the node; `GET /v1/status`; `POST /v1/shutdown`; PID lockfile enforces one instance per profile. |
| 10 | CLI | No new binary: `harmony-app api <command> [json]` + `harmony-app api --events` subcommands reading the discovery files. |

## Architecture

### The seam: `NodeEventSink` + inner fns

Every Tauri command already funnels through `Mutex<NodeState>` (lib.rs:509) — mpsc senders into the event loop plus `Arc` handles (CRDT state, content store, DM outbox, community registry). Events reach the UI via scattered `app_handle.emit(name, payload)` calls (~26 distinct event names). Integration tests boot `event_loop::run()` with no Tauri, stubbing emission behind small traits (`AppHandleEmit`, lib.rs:240). The design promotes that pattern to a first-class abstraction:

```rust
/// Mode-agnostic event emission. Replaces direct AppHandle::emit at the
/// event loop boundary and in curated commands' inner fns.
pub trait NodeEventSink: Send + Sync + 'static {
    fn emit(&self, event: &str, payload: serde_json::Value);
}
```

Three impls:

1. **GUI:** wraps `AppHandle` — emits to the webview exactly as today.
2. **API:** wraps a `tokio::sync::broadcast::Sender<EventFrame>` — feeds WS connections.
3. **Fan-out:** wraps a `Vec<Arc<dyn NodeEventSink>>` — GUI + API simultaneously when the GUI instance enables the server.

`event_loop::run` and the curated commands take `Arc<dyn NodeEventSink>` where they take `AppHandle` for emission today. Sites that use `AppHandle` for non-emission purposes (dialogs, tray) are GUI-only and out of scope — they stay behind the Tauri wrappers.

Each curated command gets the `*_inner` treatment (the house pattern from the ZEB-428 keychain seams and `start_node_inner`, lib.rs:2284):

```rust
async fn cmd_inner(
    state: &Mutex<NodeState>,
    sink: &Arc<dyn NodeEventSink>,   // only where the command emits
    args: CmdArgs,
) -> Result<CmdResult, String>
```

The Tauri wrapper calls the inner fn with `state.inner()`; the HTTP dispatcher calls it with its own `Arc<Mutex<NodeState>>`. Commands that don't emit don't take the sink. `start_node_inner`'s `&AppHandle` parameter is generalized to the sink trait the same way.

### The API server module

New module `src-tauri/src/api/` (focused files, not lib.rs growth):

- `api/mod.rs` — server assembly: bind, token generation, discovery files, axum router.
- `api/rpc.rs` — the command registry + dispatch.
- `api/events.rs` — WS upgrade, broadcast subscription, frame encoding, lag handling.
- `api/auth.rs` — token generation, file write (0600), bearer extraction/verification.
- `api/lock.rs` — profile PID lockfile.

The module binds to `Arc<Mutex<NodeState>>` + the broadcast sender. It does not know whether it lives in a `serve` process or the Tauri process.

New production dependency: `axum` (with `ws` feature) — currently only a dev-dependency via `MockPkarrRelay`; tokio is already present.

### Host 1: `harmony-app serve`

New clap subcommand in main.rs (alongside the recovery_cli subcommands):

```
harmony-app serve [--api-port PORT]
```

Boot sequence: init tracing (existing `app_tracing`, logs to stderr/file — stdout stays pure) → acquire profile lockfile → construct `Arc<Mutex<NodeState>>` → auto-start the node via the generalized `start_node_inner` with an API-only sink → bind the server → write discovery files → run until `POST /v1/shutdown` or SIGINT/SIGTERM, both of which stop the node gracefully (the existing `stop_inner` path) before exit. Owner mint auto-fires at first boot (confirmed during AVALON bring-up) — no UI interaction needed on a fresh profile.

### Host 2: GUI opt-in

When `HARMONY_API_PORT` is set (env var only in v1 — no GUI flag plumbing), `run()`'s setup hook spawns the same server module on the managed `NodeState`, with the fan-out sink so events reach both the webview and the WS stream. Auth applies identically.

## RPC protocol

`POST /v1/rpc/{command}`

- **Request body:** JSON object with the same camelCase argument names the frontend sends over Tauri IPC. Empty body allowed for arg-less commands.
- **Success:** 200 with the command's DTO serialized exactly as the IPC returns it (same serde derives, same camelCase).
- **Command error:** 500 with `{"error": "<the same Result::Err string the GUI would see>"}`. The node being stopped is a command error ("node not running"), not a server error.
- **Server errors:** 401 `{"error":"unauthorized"}` (missing/bad token), 404 `{"error":"unknown command"}` (not in registry), 400 `{"error":"<serde message>"}` (args fail to deserialize).

The registry is a static table mapping command name → dispatch closure (deserialize args → call inner fn → serialize result). Adding a command later is one registry entry plus its inner fn.

## v1 command surface

Curated, scenario-driven (~35 commands). The registry exposes the **same command names** as the Tauri IPC layer; the implementation plan enumerates the exact names from the `invoke_handler` registration (lib.rs:41418) per capability below — capability coverage is normative, the name list below is anchored where known:

| Capability | Commands (anchors) |
|---|---|
| Lifecycle | `start_node`, `stop_node` (status served by `GET /v1/status`, not RPC) |
| Identity | owner/identity status query (owner_id, device id, minted?) |
| Communities | create, list, members list, create invite, redeem invite, leave |
| Channels | create channel, list channels (lib.rs:17237), send message (lib.rs:7911), read channel history |
| Friends/DMs | friends list, friend request send/accept, send DM (lib.rs:8085), read DM thread |
| Diagnostics | network health snapshot, self-test, peers/reachability, community sync state |

**Excluded from v1:** voice (audio hardware on headless is its own project), voting/D-FROST, file ingest/CAS commands, notes, profile cards, and all GUI-affordance commands (dialogs, tray, window). All remain reachable later via registry lines once their inner fns exist.

## Event stream

`GET /v1/events` (authenticated) upgrades to WebSocket. Every `NodeEventSink::emit` on the API sink becomes one text frame:

```json
{"seq": 42, "event": "dm-received", "payload": { ... }}
```

- `seq` is monotonic per server lifetime; clients detect gaps.
- The broadcast channel is bounded; a slow client that lags receives `{"seq": n, "event": "_lagged", "payload": {"missed": k}}` and the stream continues — drops are explicit, never silent.
- Firehose: no server-side filtering or subscription protocol in v1; clients filter by `event`.
- No replay/backlog: agents connect before acting. (If a scenario needs history, that's a query command, not the stream.)

Event names and payloads are exactly the frontend event names (`dm-received`, `channel-message-received`, `community-members-changed`, `zenoh-status`, …) — one vocabulary across GUI and API.

## Auth

- On server start: generate a 32-byte random token, write to `<data-dir>/api/token`, mode 0600 (owner-only), then bind.
- Every HTTP request and the WS handshake require `Authorization: Bearer <token>`.
- **No unauthenticated endpoints** — including `/v1/status`. Rationale: browser pages can open WebSockets and fire simple POSTs at localhost without CORS preflight; a token a page can't read (filesystem) closes that hole entirely. The trust boundary is same-user-on-same-machine, matching the keychain's.
- Bind is always `127.0.0.1`. Remote access is explicitly out of scope (use an SSH tunnel).

## Port + discovery

- In v1 `<data-dir>` is the existing default profile directory (`~/.harmony`) — there is no `--data-dir` flag (decision 6).
- Default port **7420**; `--api-port` (serve) / `HARMONY_API_PORT` (both hosts) override; `0` = OS-assigned ephemeral.
- After bind, the **actually bound** port is written to `<data-dir>/api/port`.
- Client contract: read `<data-dir>/api/port` + `<data-dir>/api/token`, then talk. Two file reads regardless of configuration.
- Discovery files are rewritten on every server start and best-effort removed on graceful shutdown.

## Lifecycle, lock, and shutdown

- `serve` auto-starts the node — a serve process with a stopped node is a transient state (after `stop_node`), not the default.
- `GET /v1/status` (authenticated) returns: node running?, generation/install_seq, owner identity present + owner_id, uptime, bound port, app version.
- `POST /v1/shutdown` gracefully stops the node (existing shutdown watch + `stop_inner` path), removes discovery files and the lock, and exits the process. This is the headless analogue of the GUI's `quit_app` (ZEB-433: explicit quit paths only).
- **Profile lock:** `serve` takes `<data-dir>/api/serve.lock` (PID lockfile with liveness check — stale locks from crashed processes are reclaimed). If another process holds the profile, `serve` exits with a clear error. The GUI-with-API host takes the same lock. Rationale: v1 is single-profile (decision 6) — two nodes on one profile would race identity/state files and the fixed UDP 4242 bind (the ZEB-420/ZEB-165 class). The lock turns silent corruption into a loud refusal. Multi-profile arrives with ZEB-449 → ZEB-446.
- **Known v1 caveat:** a plain GUI launch (API not enabled) does not check the profile lock — its single-instance plugin only guards against other GUI launches. Launching the plain GUI while `serve` holds the profile is operator error in v1; the serve side refuses correctly, the GUI side is documented discipline until ZEB-446 plumbs profile awareness end to end.

## Thin CLI

Subcommands on the existing binary (no new artifact):

- `harmony-app api <command> [json-args]` — reads discovery files, POSTs the RPC, prints the result JSON to stdout (exit 0) or the error JSON to stderr (exit ≠ 0).
- `harmony-app api --events` — connects to the WS and prints one frame per line to stdout until interrupted.

PR #231's stdout-purity discipline applies: result JSON is the only stdout; all logs go to stderr. Agents that prefer `curl`/`websocat` ignore the CLI entirely.

## Error handling summary

| Layer | Behavior |
|---|---|
| RPC command failure | Same `Result<T, String>` strings as the GUI — agents and frontend debug identically (500 + `{"error"}`) |
| Unknown command / bad args / bad token | 404 / 400 / 401 with JSON error bodies |
| WS client lag | Explicit `_lagged` frame; stream continues |
| Server bind failure (port taken) | `serve` exits non-zero with a clear message |
| Profile already locked | `serve` exits non-zero naming the holding PID |
| Node stopped | Commands return "node not running" strings — not a server error |

## Testing

- **Unit (in `api/` modules):** token generation + file mode + verification; registry dispatch (unknown command, bad args, happy path against a stub); sink fan-out ordering; lockfile take/contend/reclaim-stale.
- **Integration (`--test api_server`):** boot the serve-mode core in-process with a temp profile (HOME override, `HARMONY_PASSPHRASE` file-store path per the ZEB-428 constructor gate — keychain-hermetic), ephemeral port; drive a real HTTP client through: status → identity minted → create community → create channel → send message → read it back; hold a WS connection throughout and assert the corresponding events arrived in order. This test doubles as the regression proof that the node boots Tauri-free. iroh note: the first `Endpoint::bind()` per process pays a one-time global init (~10s CI) — use the established `warm_up_iroh_global_init()` pattern before any asserted timeout (ZEB-347).
- **Gates:** per-task `cargo fmt --all` + `cargo clippy -p harmony-app --lib --features test-fixtures -D warnings` + `cargo nextest run -p harmony-app --lib --features test-fixtures`; `--test api_server` for the integration task; final `cargo check --locked --all-targets --features test-fixtures` sweep.
- **Two-sided cross-machine proof** lands in ZEB-447's scenario suite, not this ticket.

## Phasing

- **PR 1 (this branch):** `NodeEventSink` + inner-fn refactor of the curated set + `serve` subcommand + RPC + WS + auth + lock + discovery + integration test + docs (`docs/headless-install.md` gains an "API control surface" section; `docs/troubleshooting.md` pointer).
- **PR 2 (small, child ticket to file after spec approval):** GUI-mode opt-in server + `harmony-app api` CLI subcommands.

The ticket's five suggested sub-issues collapse into this: transport/schema/surface/events are one coherent PR; E2E-harness integration is ZEB-447's existing scope.

## Non-goals (v1)

- Voice, voting/D-FROST, file ingest/CAS, notes, profile-card commands.
- Multi-profile / `--data-dir` (gated on ZEB-449; then ZEB-446).
- True no-keychain headless (RPi5/CI) — gated on ZEB-449; `HARMONY_DISABLE_KEYCHAIN=1` is NOT a workaround (ZEB-450: kills iroh + blocks mint).
- Event replay/backlog, server-side event filtering.
- Remote (non-loopback) bind, TLS, multi-user auth.
- LLM bot bridges (ZEB-448 near-term, ZEB-171 parked).

## References

- NodeState: `src-tauri/src/lib.rs:509`; invoke_handler registry: `lib.rs:41418`; `start_node_inner`: `lib.rs:2284`; emission trait precedent: `lib.rs:240`; event loop: `src-tauri/src/event_loop.rs:639`.
- Headless boot precedent: `src-tauri/tests/community_invite_only_integration.rs:143`.
- Related tickets: ZEB-449 (file vault — prereq for multi-profile/RPi5), ZEB-450 (kill-switch hazard), ZEB-446 (side-by-side isolation), ZEB-447 (scenario suite), ZEB-433 (quit semantics), ZEB-428 (keychain-hermetic test gate).
