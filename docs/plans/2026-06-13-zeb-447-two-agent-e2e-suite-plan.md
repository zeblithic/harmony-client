# ZEB-447 Two-Agent E2E Scenario Suite — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A standalone `e2e-harness/` Rust crate that spawns two real `harmony-app serve` processes under named profiles and drives them over the live HTTP/WS API to prove two-sided behavior (invite/join, friend+DM, offline channel catch-up) with no human in the loop.

**Architecture:** The crate is independent of the `harmony-app` package (this repo is a single-package repo, not a Cargo workspace — do NOT convert it). It spawns the *real built binary* (`harmony-app serve --profile <p> --api-port 0`), each under a per-run temp `HOME` with `HARMONY_PASSPHRASE` (named profiles use the encrypted-file vault — keychain-safe), finds the `api/{port,token}` discovery files by walking the temp HOME, then drives the node over `POST /v1/rpc/{cmd}` + the `/v1/events` WebSocket. Convergence is asserted by **polling read-RPCs** (not by assuming events exist). Scenario tests are gated behind `--features e2e`.

**Tech Stack:** Rust (edition 2021), tokio, reqwest (http-only), tokio-tungstenite (ws://), serde_json, hex, anyhow, tempfile, walkdir.

---

## Background the implementer needs

**Verified RPC contracts** (wire is camelCase; from `src-tauri/src/api/rpc.rs` + handler impls):

| Command | Args (camelCase) | Returns |
|---|---|---|
| `mint_owner_identity` | `{}` | `{ "state": {...}, "recoveryToken": String }` |
| `get_owner_state` | `{}` | `{ "ownerId": String, "ownerDisplayName": String, ... }` or `null` |
| `create_community` | `{ "name": String, "isInviteOnly": bool }` | community id `String` (hex) |
| `generate_invite` | `{ "communityId": String }` | invite URL `String` |
| `redeem_invite` | `{ "url": String }` | `{ "ownerIdHex": String /*=joined community id*/, "display": String? }` |
| `list_owner_communities` | `{}` | `[ { "id": String, "name": String, ... } ]` |
| `list_community_members` | `{ "communityId": String }` | `[ { "addr": String, "displayName": String?, "status": "Joined"|"Left"|"Invited"|"Banned"|"PendingJoin", "power": u8, "joinedAt": Hlc } ]` |
| `generate_friend_token` | `{}` | friend token URL `String` |
| `redeem_friend_token` | `{ "url": String }` | `{ "ownerIdHex": String, "display": String? }` |
| `list_pending_friend_requests` | `{}` | `[ { "ownerIdHex": String, "display": String?, "receivedAtMs": u64 } ]` |
| `accept_friend_request` | `{ "ownerIdHex": String }` | friend owner id `String` |
| `list_friends` | `{}` | `[ { "ownerIdHex": String, "display": String?, "nickname": String?, "status": "pending"|"active"|"revoked", ... } ]` |
| `add_space` | `{ "kind": "dm", "name": String, "members": [String] }` | space id `String` (hex) |
| `send_dm` | `{ "spaceId": String, "content": [u8], "mimeType": String }` | `{ "messageId": String, "messageCid": String }` |
| `read_dm_thread` | `{ "spaceId": String, "limit": usize, "beforeHlc": u64? }` | `[ { "from": String, "body": String /*HEX plaintext*/, "mimeType": String, "isSelfOutbound": bool, "sentAt": u64, "receivedAt": u64, "messageCid": String } ]` |
| `create_channel` | `{ "communityId": String, "name": String, "writePower": u8 }` | channel id `String` (hex) |
| `list_channels` | `{ "communityId": String }` | `[ { "id": String, "name": String, ... } ]` |
| `post_channel_message` | `{ "communityId": String, "channelId": String, "body": [u8] }` | message id `String` |
| `list_channel_messages` | `{ "communityId": String, "channelId": String, "limit": u32, "since": Hlc? }` | `[ { "author": String, "body": [u8] /*byte array*/, "at": Hlc, "messageId": String } ]` |

**CRITICAL wire facts:**
- `content`/`body` send args are `Vec<u8>` with **no** `serde_bytes` → serialize as a JSON **array of numbers** (`[104,105]`), not base64. In Rust, putting a `Vec<u8>` into `serde_json::json!` produces exactly this.
- `read_dm_thread[].body` is returned **hex-encoded** (`hex::decode` it to compare to sent bytes).
- `list_channel_messages[].body` is returned as a **byte array** (compare directly).
- A member's stable id in `list_community_members` is `addr`; it equals the peer's `ownerId` from `get_owner_state`. Membership "joined" = `status == "Joined"`.
- `Hlc` is `{ "wallMs": u64, "logical": u32, "deviceId": String }`.

**Verified runtime facts:**
- `--profile <name>` is a **global** flag (before the subcommand): `harmony-app --profile alice serve --api-port 0`.
- A **named** profile requires `HARMONY_PASSPHRASE` or `HARMONY_PASSPHRASE_FILE` (fail-fast at startup) and uses the encrypted-file vault, never the OS keychain. **Always use named profiles** — the default profile would touch the developer's real OS keychain.
- Discovery: `<app-data-dir>/api/port` (plain `"<u16>\n"`) and `<app-data-dir>/api/token` (64 hex chars, 0600). On macOS `<app-data-dir>` = `$HOME/Library/Application Support/net.zeblith.harmony/profiles/<p>`. The harness finds these by walking the temp `HOME` for a unique `api/port` file (avoids per-OS path logic).
- Auth: every request needs header `Authorization: Bearer <token>` (exact, case-sensitive).
- WS: `GET /v1/events`, bearer header required on the upgrade; frames are `{ "seq": u64|null, "event": String, "payload": <json> }`; a lag marker is `{ "seq": null, "event": "_lagged", "payload": { "missed": u64 } }`.
- Status: `GET /v1/status` → `{ "running": bool, "generation": u64, "ownerId": String|null, "uptimeSecs": u64, "port": u16, "version": String }`.
- Shutdown: `POST /v1/shutdown` → `{ "shuttingDown": true }`.
- `HARMONY_RETICULUM_PORT=0` disables Reticulum LAN discovery (avoids the fixed-4242 collision between two local nodes); iroh carries direct delivery.
- **Node boot is slow** (iroh first-bind global init can be ~10–30s on macOS) — readiness polls must allow ≥60s.

**Build prerequisite (run once before any `--features e2e` test):**
```bash
cd src-tauri && cargo build --bin harmony-app    # produces ../src-tauri/target/debug/harmony-app
```
The harness resolves the binary via `HARMONY_APP_BIN`, else `../src-tauri/target/{release,debug}/harmony-app`.

---

## File structure

```
e2e-harness/
  .gitignore                 # /target
  Cargo.toml                 # standalone manifest; feature `e2e` gates the scenario tests
  src/
    lib.rs                   # re-exports: bin resolver, NodeHandle, driver, run dir, poll_until
    bin_resolver.rs          # resolve_harmony_app_bin()
    node.rs                  # NodeConfig, NodeHandle (spawn/discovery/rpc/status/kill/shutdown/Drop)
    events.rs                # EventFrame, WS subscriber task, await_event
    driver.rs                # semantic RPC helpers + poll_until
    run_dir.rs               # RunDir artifact collector
  tests/
    e2e_two_node.rs          # #[tokio::test] scenarios S1..S4 (gated `#[cfg(feature = "e2e")]`)
  README.md                  # how to build the binary + run the suite
docs/
  playbooks/e2e-two-agent-suite.md   # cross-machine run protocol (dogfoods the coord instance)
```

All file paths below are relative to the repo root `/Users/zeblith/work/zeblithic/harmony-client`.

---

### Task 1: Crate scaffold + binary resolver

**Files:**
- Create: `e2e-harness/Cargo.toml`
- Create: `e2e-harness/.gitignore`
- Create: `e2e-harness/src/lib.rs`
- Create: `e2e-harness/src/bin_resolver.rs`
- Create: `e2e-harness/README.md`

- [ ] **Step 1: Create `e2e-harness/.gitignore`**

```
/target
```

- [ ] **Step 2: Create `e2e-harness/Cargo.toml`**

```toml
[package]
name = "e2e-harness"
version = "0.1.0"
edition = "2021"
publish = false
description = "ZEB-447 two-agent end-to-end scenario harness for harmony-client (spawns real `serve` nodes)."

[features]
# Scenario tests spawn the real harmony-app binary + real transport; they are
# slow and network-touching, so they are OFF by default. Run deliberately:
#   cargo nextest run --features e2e
e2e = []

[dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "macros", "process", "time", "net", "io-util", "fs"] }
reqwest = { version = "0.12", default-features = false, features = ["json"] }
tokio-tungstenite = "0.24"
futures-util = "0.3"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
hex = "0.4"
anyhow = "1"
tempfile = "3"
walkdir = "2"

[dev-dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "macros", "process", "time", "net", "io-util", "fs"] }
```

- [ ] **Step 3: Create `e2e-harness/src/bin_resolver.rs`**

```rust
//! Locate the built `harmony-app` binary. Because this is a standalone crate
//! (not a harmony-app test target), `CARGO_BIN_EXE_harmony-app` is unavailable.

use std::path::PathBuf;

/// Resolve the `harmony-app` binary path.
///
/// Priority: `HARMONY_APP_BIN` env override, else `../src-tauri/target/release`
/// then `../src-tauri/target/debug`, relative to this crate's manifest dir.
/// Returns an error (never a silent skip) if none exists.
pub fn resolve_harmony_app_bin() -> anyhow::Result<PathBuf> {
    if let Ok(p) = std::env::var("HARMONY_APP_BIN") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Ok(pb);
        }
        anyhow::bail!("HARMONY_APP_BIN is set but not a file: {}", pb.display());
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let exe = if cfg!(windows) { "harmony-app.exe" } else { "harmony-app" };
    for profile in ["release", "debug"] {
        let cand = manifest.join("..").join("src-tauri").join("target").join(profile).join(exe);
        if cand.is_file() {
            return Ok(cand);
        }
    }
    anyhow::bail!(
        "harmony-app binary not found. Build it first:\n  cd src-tauri && cargo build --bin harmony-app\n\
         or set HARMONY_APP_BIN to an explicit path."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_to_missing_file_errors() {
        // SAFETY: single-threaded unit test; restore after.
        std::env::set_var("HARMONY_APP_BIN", "/definitely/not/here/harmony-app");
        let err = resolve_harmony_app_bin().unwrap_err().to_string();
        std::env::remove_var("HARMONY_APP_BIN");
        assert!(err.contains("not a file"), "got: {err}");
    }
}
```

- [ ] **Step 4: Create `e2e-harness/src/lib.rs`**

```rust
//! ZEB-447 two-agent E2E harness. See `docs/specs/2026-06-13-zeb-447-two-agent-e2e-suite-design.md`.

pub mod bin_resolver;

pub use bin_resolver::resolve_harmony_app_bin;
```

- [ ] **Step 5: Create `e2e-harness/README.md`**

```markdown
# e2e-harness (ZEB-447)

Standalone harness that spawns two real `harmony-app serve` nodes under named
profiles and drives them over the live HTTP/WS API.

## Run

```bash
# 1. Build the binary the harness drives:
cd src-tauri && cargo build --bin harmony-app && cd ..

# 2. Run the scenario suite (slow, real transport):
cd e2e-harness && cargo nextest run --features e2e
```

Set `HARMONY_APP_BIN=/path/to/harmony-app` to override binary discovery.
Set `HARMONY_E2E_KEEP=1` to retain run artifacts on success
(`e2e-harness/target/e2e-runs/<scenario>-<runid>/`).
```

- [ ] **Step 6: Verify the crate builds and the unit test passes**

Run: `cd e2e-harness && cargo test bin_resolver`
Expected: compiles; `env_override_to_missing_file_errors` PASSES.

- [ ] **Step 7: Commit**

```bash
git add e2e-harness/
git commit -m "feat(zeb-447): e2e-harness crate scaffold + binary resolver"
```

---

### Task 2: `NodeHandle` — spawn, discovery, rpc(), status(), kill/shutdown, Drop

**Files:**
- Create: `e2e-harness/src/node.rs`
- Modify: `e2e-harness/src/lib.rs`
- Test: `e2e-harness/tests/e2e_two_node.rs` (add `single_node_mints_owner`)

- [ ] **Step 1: Create `e2e-harness/src/node.rs`**

```rust
//! One spawned `harmony-app serve` subprocess + an HTTP client driving it.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde_json::Value;
use tokio::process::{Child, Command};

use crate::bin_resolver::resolve_harmony_app_bin;

/// Persistent config for one node. Reused across kill+relaunch so on-disk state
/// rehydrates (the same `home` + `profile` + `passphrase`).
#[derive(Clone)]
pub struct NodeConfig {
    /// Temp HOME root; the node's identity + app-data live under here.
    pub home: PathBuf,
    /// Named profile (lowercase `[a-z0-9][a-z0-9_-]{0,31}`); never "default".
    pub profile: String,
    /// File-vault passphrase (required for named profiles).
    pub passphrase: String,
    /// Optional dir to capture child stdout/stderr into (artifacts).
    pub log_dir: Option<PathBuf>,
}

impl NodeConfig {
    pub fn new(home: PathBuf, profile: &str) -> Self {
        Self {
            home,
            profile: profile.to_string(),
            passphrase: "e2e-test-passphrase".to_string(),
            log_dir: None,
        }
    }
}

pub struct NodeHandle {
    pub config: NodeConfig,
    pub port: u16,
    pub token: String,
    pub base_url: String,
    child: Option<Child>,
    http: reqwest::Client,
}

impl NodeHandle {
    /// Spawn `harmony-app --profile <p> serve --api-port 0` and wait until the
    /// `api/{port,token}` discovery files appear and `/v1/status` answers.
    pub async fn spawn(config: NodeConfig) -> anyhow::Result<Self> {
        let bin = resolve_harmony_app_bin()?;
        let (stdout, stderr) = match &config.log_dir {
            Some(dir) => {
                tokio::fs::create_dir_all(dir).await.ok();
                let out = std::fs::File::create(dir.join(format!("{}.stdout.log", config.profile)))?;
                let err = std::fs::File::create(dir.join(format!("{}.stderr.log", config.profile)))?;
                (Stdio::from(out), Stdio::from(err))
            }
            None => (Stdio::null(), Stdio::null()),
        };

        let child = Command::new(&bin)
            .arg("--profile")
            .arg(&config.profile)
            .arg("serve")
            .arg("--api-port")
            .arg("0")
            .env("HOME", &config.home)
            // Windows identity dir uses USERPROFILE; keep both pointed at the temp root.
            .env("USERPROFILE", &config.home)
            .env("HARMONY_PASSPHRASE", &config.passphrase)
            .env("HARMONY_RETICULUM_PORT", "0")
            .env("HARMONY_API_PORT", "0")
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning {}", bin.display()))?;

        // Find the unique api/{port,token} under the temp HOME (avoids per-OS
        // app-data path derivation). Node boot + iroh init can take ~30s.
        let api_dir = wait_for_api_dir(&config.home, Duration::from_secs(90)).await?;
        let port: u16 = tokio::fs::read_to_string(api_dir.join("port"))
            .await?
            .trim()
            .parse()
            .context("parsing api/port")?;
        let token = tokio::fs::read_to_string(api_dir.join("token")).await?.trim().to_string();
        let base_url = format!("http://127.0.0.1:{port}");
        let http = reqwest::Client::new();

        let mut handle = Self { config, port, token, base_url, child: Some(child), http };
        handle.wait_until_status_running(Duration::from_secs(90)).await?;
        Ok(handle)
    }

    /// `POST /v1/rpc/{cmd}` with bearer auth + a camelCase JSON body. Returns the
    /// 200 result, or an Err carrying the server's error string (identical to the GUI's).
    pub async fn rpc(&self, cmd: &str, args: Value) -> anyhow::Result<Value> {
        let resp = self
            .http
            .post(format!("{}/v1/rpc/{cmd}", self.base_url))
            .bearer_auth(&self.token)
            .json(&args)
            .send()
            .await
            .with_context(|| format!("rpc {cmd}: request failed"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status.as_u16() == 200 {
            return Ok(serde_json::from_str(&body).unwrap_or(Value::Null));
        }
        let server_err = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str().map(str::to_string)))
            .unwrap_or(body);
        anyhow::bail!("rpc {cmd} -> HTTP {}: {server_err}", status.as_u16())
    }

    pub async fn status(&self) -> anyhow::Result<Value> {
        let resp = self
            .http
            .get(format!("{}/v1/status", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?;
        Ok(resp.json::<Value>().await?)
    }

    async fn wait_until_status_running(&self, timeout: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(s) = self.status().await {
                if s.get("running").and_then(Value::as_bool) == Some(true) {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                anyhow::bail!("node {} never reported running=true", self.config.profile);
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    /// Hard offline: SIGKILL the child (no graceful shutdown).
    pub async fn kill(&mut self) -> anyhow::Result<()> {
        if let Some(mut child) = self.child.take() {
            child.start_kill().ok();
            let _ = child.wait().await;
        }
        Ok(())
    }

    /// Graceful shutdown via the API, then ensure the child is gone.
    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        let _ = self
            .http
            .post(format!("{}/v1/shutdown", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await;
        if let Some(mut child) = self.child.take() {
            let _ = tokio::time::timeout(Duration::from_secs(15), child.wait()).await;
            child.start_kill().ok();
        }
        Ok(())
    }
}

impl Drop for NodeHandle {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

/// Poll for the unique `api/` dir (containing `port` + `token`) under `home`.
async fn wait_for_api_dir(home: &std::path::Path, timeout: Duration) -> anyhow::Result<PathBuf> {
    let deadline = Instant::now() + timeout;
    loop {
        for entry in walkdir::WalkDir::new(home).into_iter().flatten() {
            if entry.file_name() == "port" {
                let dir = entry.path().parent().map(PathBuf::from);
                if let Some(dir) = dir {
                    if dir.file_name().map(|f| f == "api").unwrap_or(false)
                        && dir.join("token").is_file()
                    {
                        return Ok(dir);
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("discovery files (api/port, api/token) never appeared under {}", home.display());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
```

- [ ] **Step 2: Export `node` from `lib.rs`**

Append to `e2e-harness/src/lib.rs`:
```rust
pub mod node;
pub use node::{NodeConfig, NodeHandle};
```

- [ ] **Step 3: Create `e2e-harness/tests/e2e_two_node.rs` with the first scenario**

```rust
//! ZEB-447 two-node E2E scenarios. Gated behind `--features e2e` (spawns the
//! real harmony-app binary + real transport). Build the binary first:
//!   cd src-tauri && cargo build --bin harmony-app

#![cfg(feature = "e2e")]

use std::path::PathBuf;

use e2e_harness::{NodeConfig, NodeHandle};
use serde_json::json;

fn fresh_home(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new().prefix(&format!("harmony-e2e-{tag}-")).tempdir().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_node_mints_owner() {
    let home = fresh_home("solo");
    let cfg = NodeConfig::new(PathBuf::from(home.path()), "alice");
    let node = NodeHandle::spawn(cfg).await.expect("spawn alice");

    let pre = node.status().await.expect("status");
    assert_eq!(pre.get("ownerId").cloned().unwrap_or(serde_json::Value::Null), serde_json::Value::Null,
        "owner should be unminted at first boot");

    let mint = node.rpc("mint_owner_identity", json!({})).await.expect("mint");
    assert!(mint.get("recoveryToken").and_then(|v| v.as_str()).is_some(), "mint returns recoveryToken");

    let owner = node.rpc("get_owner_state", json!({})).await.expect("get_owner_state");
    assert!(owner.get("ownerId").and_then(|v| v.as_str()).is_some(), "owner id set after mint");

    // keep `home` alive until here
    drop(node);
    drop(home);
}
```

- [ ] **Step 4: Build the binary, then run the test**

```bash
cd src-tauri && cargo build --bin harmony-app && cd ../e2e-harness
cargo nextest run --features e2e single_node_mints_owner
```
Expected: PASS (node boots, mints, reports an owner id). First run may take ~30–60s for iroh init.

- [ ] **Step 5: Per-crate gate (fast, no real binary needed)**

```bash
cd e2e-harness
cargo fmt
cargo clippy --all-targets --features e2e -- -D warnings
```
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add e2e-harness/
git commit -m "feat(zeb-447): NodeHandle spawn/discovery/rpc/status + single-node mint scenario"
```

---

### Task 3: WS event subscriber + `await_event`

**Files:**
- Create: `e2e-harness/src/events.rs`
- Modify: `e2e-harness/src/node.rs` (start subscriber in `spawn`, add `await_event`)
- Modify: `e2e-harness/src/lib.rs`
- Test: `e2e-harness/tests/e2e_two_node.rs` (add `mint_emits_mint_changed_event`)

- [ ] **Step 1: Create `e2e-harness/src/events.rs`**

```rust
//! `/v1/events` WebSocket subscriber.

use std::time::Duration;

use anyhow::Context;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug, Clone, Deserialize)]
pub struct EventFrame {
    pub seq: Option<u64>,
    pub event: String,
    pub payload: Value,
}

/// Connect to `ws://127.0.0.1:<port>/v1/events` with bearer auth and forward
/// each parsed frame to an mpsc receiver. The background task ends when the
/// receiver is dropped or the socket closes; the returned `JoinHandle` is for
/// the caller to keep alive (dropping it merely detaches — it does NOT abort).
pub async fn subscribe(port: u16, token: &str) -> anyhow::Result<(mpsc::UnboundedReceiver<EventFrame>, tokio::task::JoinHandle<()>)> {
    let url = format!("ws://127.0.0.1:{port}/v1/events");
    let mut req = url.into_client_request().context("ws request")?;
    req.headers_mut()
        .insert(AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
    let (ws, _resp) = tokio_tungstenite::connect_async(req).await.context("ws connect")?;

    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        // Move the whole stream in (no split — we never send, and splitting risks
        // dropping the write half early on some impls).
        let mut ws = ws;
        while let Some(Ok(msg)) = ws.next().await {
            if let Message::Text(txt) = msg {
                if let Ok(frame) = serde_json::from_str::<EventFrame>(&txt) {
                    if tx.send(frame).is_err() {
                        break;
                    }
                }
            }
        }
    });
    Ok((rx, task))
}

/// Drain the receiver until `pred` matches a frame or `timeout` elapses.
pub async fn await_event(
    rx: &mut mpsc::UnboundedReceiver<EventFrame>,
    timeout: Duration,
    pred: impl Fn(&EventFrame) -> bool,
) -> anyhow::Result<EventFrame> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("await_event timed out after {timeout:?}");
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(frame)) if pred(&frame) => return Ok(frame),
            Ok(Some(_)) => continue,
            Ok(None) => anyhow::bail!("event stream closed"),
            Err(_) => anyhow::bail!("await_event timed out after {timeout:?}"),
        }
    }
}
```

- [ ] **Step 2: Add an `events()` accessor on `NodeHandle`**

In `e2e-harness/src/node.rs`, add a method (NodeHandle does NOT hold the receiver — the scenario owns it so multiple awaits compose):
```rust
impl NodeHandle {
    /// Open a fresh event subscription. The caller owns the receiver + task.
    pub async fn events(
        &self,
    ) -> anyhow::Result<(
        tokio::sync::mpsc::UnboundedReceiver<crate::events::EventFrame>,
        tokio::task::JoinHandle<()>,
    )> {
        crate::events::subscribe(self.port, &self.token).await
    }
}
```

- [ ] **Step 3: Export `events` from `lib.rs`**

Append to `e2e-harness/src/lib.rs`:
```rust
pub mod events;
pub use events::{await_event, EventFrame};
```

- [ ] **Step 4: Add the event scenario to `tests/e2e_two_node.rs`**

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mint_emits_mint_changed_event() {
    use std::time::Duration;
    let home = fresh_home("evt");
    let cfg = NodeConfig::new(PathBuf::from(home.path()), "alice");
    let node = NodeHandle::spawn(cfg).await.expect("spawn");
    let (mut rx, _task) = node.events().await.expect("subscribe");

    node.rpc("mint_owner_identity", json!({})).await.expect("mint");

    e2e_harness::await_event(&mut rx, Duration::from_secs(20), |f| f.event == "mint-changed")
        .await
        .expect("mint-changed event");

    drop(node);
    drop(home);
}
```

- [ ] **Step 5: Run the new test**

```bash
cd e2e-harness && cargo nextest run --features e2e mint_emits_mint_changed_event
```
Expected: PASS (a `mint-changed` frame arrives over WS). If the event name differs, fix the predicate to the actual name observed in the captured `*.stderr.log` and update this step.

- [ ] **Step 6: Per-crate gate + commit**

```bash
cd e2e-harness && cargo fmt && cargo clippy --all-targets --features e2e -- -D warnings
git add e2e-harness/ && git commit -m "feat(zeb-447): WS event subscriber + await_event"
```

---

### Task 4: Run-dir artifact collector

**Files:**
- Create: `e2e-harness/src/run_dir.rs`
- Modify: `e2e-harness/src/lib.rs`
- Modify: `e2e-harness/tests/e2e_two_node.rs` (wire `RunDir` into a helper that builds two nodes)

- [ ] **Step 1: Create `e2e-harness/src/run_dir.rs`**

```rust
//! Per-scenario artifact directory: target/e2e-runs/<scenario>-<runid>/.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct RunDir {
    pub path: PathBuf,
    keep: bool,
    succeeded: bool,
}

impl RunDir {
    pub fn new(scenario: &str) -> anyhow::Result<Self> {
        let runid = format!(
            "{}-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
            std::process::id()
        );
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("e2e-runs")
            .join(format!("{scenario}-{runid}"));
        std::fs::create_dir_all(&path)?;
        let keep = std::env::var("HARMONY_E2E_KEEP").map(|v| v != "0" && !v.is_empty()).unwrap_or(false);
        Ok(Self { path, keep, succeeded: false })
    }

    /// Path for a node's captured stdout/stderr (pass into `NodeConfig.log_dir`).
    pub fn log_dir(&self) -> PathBuf {
        self.path.clone()
    }

    pub fn write_jsonl(&self, name: &str, value: &serde_json::Value) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.path.join(name)) {
            let _ = writeln!(f, "{value}");
        }
    }

    /// Mark the scenario as passed so artifacts are cleaned unless HARMONY_E2E_KEEP.
    pub fn mark_success(&mut self) {
        self.succeeded = true;
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        if self.succeeded && !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        } else {
            eprintln!("e2e artifacts: {}", self.path.display());
        }
    }
}
```

- [ ] **Step 2: Export `run_dir` from `lib.rs`**

Append to `e2e-harness/src/lib.rs`:
```rust
pub mod run_dir;
pub use run_dir::RunDir;
```

- [ ] **Step 3: Add a two-node setup helper to `tests/e2e_two_node.rs`**

```rust
use e2e_harness::RunDir;

/// Spawn two named-profile nodes, each under its OWN temp HOME (so discovery is
/// unambiguous), both minted, stdout/stderr captured into the run dir. Returns
/// (run_dir, alice_home, bob_home, alice, bob). Keep both homes alive until the
/// scenario ends.
async fn two_minted_nodes(
    scenario: &str,
) -> (RunDir, tempfile::TempDir, tempfile::TempDir, NodeHandle, NodeHandle) {
    let run = RunDir::new(scenario).expect("run dir");
    let alice_home = fresh_home(&format!("{scenario}-a"));
    let bob_home = fresh_home(&format!("{scenario}-b"));
    let mk = |home: &tempfile::TempDir, profile: &str| {
        let mut cfg = NodeConfig::new(PathBuf::from(home.path()), profile);
        cfg.log_dir = Some(run.log_dir());
        cfg
    };
    let alice = NodeHandle::spawn(mk(&alice_home, "alice")).await.expect("spawn alice");
    let bob = NodeHandle::spawn(mk(&bob_home, "bob")).await.expect("spawn bob");
    alice.rpc("mint_owner_identity", json!({})).await.expect("alice mint");
    bob.rpc("mint_owner_identity", json!({})).await.expect("bob mint");
    (run, alice_home, bob_home, alice, bob)
}

async fn owner_id(node: &NodeHandle) -> String {
    let o = node.rpc("get_owner_state", json!({})).await.expect("get_owner_state");
    o.get("ownerId").and_then(|v| v.as_str()).expect("ownerId").to_string()
}
```

- [ ] **Step 4: Smoke that the helper works (sanity, not committed as a permanent scenario)**

Temporarily add and run:
```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_boot_and_mint() {
    let (mut run, ah, bh, a, b) = two_minted_nodes("smoke").await;
    assert_ne!(owner_id(&a).await, owner_id(&b).await, "distinct owners");
    run.mark_success();
    drop((a, b, ah, bh));
}
```
Run: `cd e2e-harness && cargo nextest run --features e2e two_nodes_boot_and_mint`
Expected: PASS (two distinct nodes boot + mint distinct owners). Keep this test — it's a useful base sanity check.

- [ ] **Step 5: Per-crate gate + commit**

```bash
cd e2e-harness && cargo fmt && cargo clippy --all-targets --features e2e -- -D warnings
git add e2e-harness/ && git commit -m "feat(zeb-447): RunDir artifact collector + two-node setup helper"
```

---

### Task 5: Driver library — semantic helpers + `poll_until`

**Files:**
- Create: `e2e-harness/src/driver.rs`
- Modify: `e2e-harness/src/lib.rs`

- [ ] **Step 1: Create `e2e-harness/src/driver.rs`**

```rust
//! Semantic helpers over `NodeHandle::rpc` encoding the verified RPC contracts,
//! plus a generic `poll_until` convergence primitive.

use std::future::Future;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::node::NodeHandle;

/// Poll `f` until it yields `Some(T)` or `timeout` elapses (250ms interval).
pub async fn poll_until<F, Fut, T>(timeout: Duration, mut f: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<Option<T>>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f().await? {
            return Ok(v);
        }
        if Instant::now() >= deadline {
            anyhow::bail!("poll_until timed out after {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn as_str(v: &Value) -> anyhow::Result<String> {
    v.as_str().map(str::to_string).ok_or_else(|| anyhow::anyhow!("expected string, got {v}"))
}

pub async fn mint(node: &NodeHandle) -> anyhow::Result<()> {
    node.rpc("mint_owner_identity", json!({})).await.map(|_| ())
}

pub async fn create_community(node: &NodeHandle, name: &str, invite_only: bool) -> anyhow::Result<String> {
    as_str(&node.rpc("create_community", json!({ "name": name, "isInviteOnly": invite_only })).await?)
}

pub async fn generate_invite(node: &NodeHandle, community_id: &str) -> anyhow::Result<String> {
    as_str(&node.rpc("generate_invite", json!({ "communityId": community_id })).await?)
}

pub async fn redeem_invite(node: &NodeHandle, url: &str) -> anyhow::Result<Value> {
    node.rpc("redeem_invite", json!({ "url": url })).await
}

pub async fn list_community_members(node: &NodeHandle, community_id: &str) -> anyhow::Result<Vec<Value>> {
    let v = node.rpc("list_community_members", json!({ "communityId": community_id })).await?;
    Ok(v.as_array().cloned().unwrap_or_default())
}

/// True once `member_owner` appears in `community_id`'s roster with status "Joined".
pub async fn roster_has_joined(node: &NodeHandle, community_id: &str, member_owner: &str) -> anyhow::Result<bool> {
    let members = list_community_members(node, community_id).await?;
    Ok(members.iter().any(|m| {
        m.get("addr").and_then(Value::as_str) == Some(member_owner)
            && m.get("status").and_then(Value::as_str) == Some("Joined")
    }))
}

pub async fn generate_friend_token(node: &NodeHandle) -> anyhow::Result<String> {
    as_str(&node.rpc("generate_friend_token", json!({})).await?)
}

pub async fn redeem_friend_token(node: &NodeHandle, url: &str) -> anyhow::Result<Value> {
    node.rpc("redeem_friend_token", json!({ "url": url })).await
}

pub async fn list_friends(node: &NodeHandle) -> anyhow::Result<Vec<Value>> {
    Ok(node.rpc("list_friends", json!({})).await?.as_array().cloned().unwrap_or_default())
}

pub async fn friend_is_active(node: &NodeHandle, owner: &str) -> anyhow::Result<bool> {
    Ok(list_friends(node).await?.iter().any(|f| {
        f.get("ownerIdHex").and_then(Value::as_str) == Some(owner)
            && f.get("status").and_then(Value::as_str) == Some("active")
    }))
}

pub async fn accept_pending_from(node: &NodeHandle, owner: &str) -> anyhow::Result<bool> {
    let pending = node.rpc("list_pending_friend_requests", json!({})).await?;
    let has = pending.as_array().map(|a| a.iter().any(|p| p.get("ownerIdHex").and_then(Value::as_str) == Some(owner))).unwrap_or(false);
    if has {
        node.rpc("accept_friend_request", json!({ "ownerIdHex": owner })).await?;
    }
    Ok(has)
}

pub async fn add_dm_space(node: &NodeHandle, name: &str, peer_owner: &str) -> anyhow::Result<String> {
    as_str(&node.rpc("add_space", json!({ "kind": "dm", "name": name, "members": [peer_owner] })).await?)
}

pub async fn send_dm(node: &NodeHandle, space_id: &str, content: &[u8], mime: &str) -> anyhow::Result<Value> {
    node.rpc("send_dm", json!({ "spaceId": space_id, "content": content, "mimeType": mime })).await
}

/// Read the DM thread; returns decoded (from_owner, plaintext_bytes) pairs.
pub async fn read_dm_plaintext(node: &NodeHandle, space_id: &str) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    let v = node.rpc("read_dm_thread", json!({ "spaceId": space_id, "limit": 100 })).await?;
    let mut out = Vec::new();
    for m in v.as_array().cloned().unwrap_or_default() {
        let from = m.get("from").and_then(Value::as_str).unwrap_or_default().to_string();
        let body_hex = m.get("body").and_then(Value::as_str).unwrap_or_default();
        let bytes = hex::decode(body_hex).unwrap_or_default();
        out.push((from, bytes));
    }
    Ok(out)
}

pub async fn create_channel(node: &NodeHandle, community_id: &str, name: &str, write_power: u8) -> anyhow::Result<String> {
    as_str(&node.rpc("create_channel", json!({ "communityId": community_id, "name": name, "writePower": write_power })).await?)
}

pub async fn list_channels(node: &NodeHandle, community_id: &str) -> anyhow::Result<Vec<Value>> {
    Ok(node.rpc("list_channels", json!({ "communityId": community_id })).await?.as_array().cloned().unwrap_or_default())
}

pub async fn channels_contains(node: &NodeHandle, community_id: &str, channel_id: &str) -> anyhow::Result<bool> {
    Ok(list_channels(node, community_id).await?.iter().any(|c| c.get("id").and_then(Value::as_str) == Some(channel_id)))
}

pub async fn post_channel_message(node: &NodeHandle, community_id: &str, channel_id: &str, body: &[u8]) -> anyhow::Result<Value> {
    node.rpc("post_channel_message", json!({ "communityId": community_id, "channelId": channel_id, "body": body })).await
}

pub async fn list_channel_messages(node: &NodeHandle, community_id: &str, channel_id: &str) -> anyhow::Result<Vec<Value>> {
    let v = node.rpc("list_channel_messages", json!({ "communityId": community_id, "channelId": channel_id, "limit": 100 })).await?;
    Ok(v.as_array().cloned().unwrap_or_default())
}
```

- [ ] **Step 2: Export `driver` from `lib.rs`**

Append to `e2e-harness/src/lib.rs`:
```rust
pub mod driver;
pub use driver::poll_until;
```

- [ ] **Step 3: Verify the crate still compiles (driver has no real-node unit test)**

Run: `cd e2e-harness && cargo build --features e2e && cargo clippy --all-targets --features e2e -- -D warnings`
Expected: clean (driver is exercised by the scenarios in later tasks).

- [ ] **Step 4: Commit**

```bash
cd e2e-harness && cargo fmt
git add e2e-harness/ && git commit -m "feat(zeb-447): driver lib (semantic RPC helpers + poll_until)"
```

---

### Task 6: Scenario S1 — invite → cross-node join → roster convergence

**Files:**
- Modify: `e2e-harness/tests/e2e_two_node.rs`

- [ ] **Step 1: Add S1**

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s1_invite_join_roster_convergence() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    let (mut run, ah, bh, alice, bob) = two_minted_nodes("s1").await;
    let bob_owner = owner_id(&bob).await;
    let alice_owner = owner_id(&alice).await;

    // Alice mints a community and an invite.
    let community = create_community(&alice, "s1-community", true).await.expect("create community");
    let invite = generate_invite(&alice, &community).await.expect("generate invite");

    // Bob redeems → joins the same community.
    let redeemed = redeem_invite(&bob, &invite).await.expect("redeem invite");
    let joined_id = redeemed.get("ownerIdHex").and_then(|v| v.as_str()).expect("joined community id");
    assert_eq!(joined_id, community, "bob joined alice's community");

    // Roster converges both directions (poll — no assumed event).
    poll_until(Duration::from_secs(60), || async {
        Ok(roster_has_joined(&alice, &community, &bob_owner).await?.then_some(()))
    }).await.expect("alice sees bob joined");

    poll_until(Duration::from_secs(60), || async {
        Ok(roster_has_joined(&bob, &community, &alice_owner).await?.then_some(()))
    }).await.expect("bob sees alice joined");

    run.mark_success();
    drop((alice, bob, ah, bh));
}
```

- [ ] **Step 2: Run S1**

```bash
cd e2e-harness && cargo nextest run --features e2e s1_invite_join_roster_convergence
```
Expected: PASS. **This is the first-contact-over-loopback de-risk** — if redeem/join fails to converge, inspect `target/e2e-runs/s1-*/` (set `HARMONY_E2E_KEEP=1`); the two nodes may need pkarr/relay (internet) for first contact. The dev box has internet, so if it still fails, capture the node stderr and treat it as a finding before proceeding.

- [ ] **Step 3: Per-crate gate + commit**

```bash
cd e2e-harness && cargo fmt && cargo clippy --all-targets --features e2e -- -D warnings
git add e2e-harness/ && git commit -m "test(zeb-447): S1 invite -> join -> roster convergence"
```

---

### Task 7: Scenario S2 — friend-add → DM picker → DM exchange

**Files:**
- Modify: `e2e-harness/tests/e2e_two_node.rs`

- [ ] **Step 1: Add S2**

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s2_friend_dm_exchange() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    let (mut run, ah, bh, alice, bob) = two_minted_nodes("s2").await;
    let alice_owner = owner_id(&alice).await;
    let bob_owner = owner_id(&bob).await;

    // Friend handshake: Alice mints a friend token, Bob redeems it.
    let token = generate_friend_token(&alice).await.expect("friend token");
    redeem_friend_token(&bob, &token).await.expect("redeem friend token");

    // Token redemption may auto-accept OR raise a pending request on Alice's side.
    // Be robust to both: if Alice has a pending request from Bob, accept it.
    poll_until(Duration::from_secs(60), || async {
        accept_pending_from(&alice, &bob_owner).await?;
        Ok(friend_is_active(&alice, &bob_owner).await?.then_some(()))
    }).await.expect("alice has bob as active friend");

    poll_until(Duration::from_secs(60), || async {
        Ok(friend_is_active(&bob, &alice_owner).await?.then_some(()))
    }).await.expect("bob has alice as active friend (DM-picker class, ZEB-431)");

    // DM spaces are addressed by member set: both sides derive the same space id.
    let a_space = add_dm_space(&alice, "s2-dm", &bob_owner).await.expect("alice dm space");
    let b_space = add_dm_space(&bob, "s2-dm", &alice_owner).await.expect("bob dm space");
    assert_eq!(a_space, b_space, "DM space id is deterministic across both members");

    // Alice -> Bob.
    send_dm(&alice, &a_space, b"hello-from-alice", "text/plain").await.expect("alice send");
    poll_until(Duration::from_secs(60), || async {
        let msgs = read_dm_plaintext(&bob, &b_space).await?;
        Ok(msgs.iter().any(|(_, body)| body == b"hello-from-alice").then_some(()))
    }).await.expect("bob receives alice's dm");

    // Bob -> Alice.
    send_dm(&bob, &b_space, b"hello-from-bob", "text/plain").await.expect("bob send");
    poll_until(Duration::from_secs(60), || async {
        let msgs = read_dm_plaintext(&alice, &a_space).await?;
        Ok(msgs.iter().any(|(_, body)| body == b"hello-from-bob").then_some(()))
    }).await.expect("alice receives bob's dm");

    run.mark_success();
    drop((alice, bob, ah, bh));
}
```

- [ ] **Step 2: Run S2**

```bash
cd e2e-harness && cargo nextest run --features e2e s2_friend_dm_exchange
```
Expected: PASS. If `add_dm_space` ids differ across nodes, the assumption that DM spaces are member-set-deterministic is wrong — capture the two ids and adjust S2 to discover Bob's space id another way (inspect `list_owner_communities`/space listing in the stderr log) before proceeding. If the friend never reaches `active`, inspect whether token redemption needs an explicit accept on the *redeemer* side too.

- [ ] **Step 3: Per-crate gate + commit**

```bash
cd e2e-harness && cargo fmt && cargo clippy --all-targets --features e2e -- -D warnings
git add e2e-harness/ && git commit -m "test(zeb-447): S2 friend-add -> DM picker -> DM exchange"
```

---

### Task 8: Scenario S3 — channel created while peer offline → reconnect catch-up

**Files:**
- Modify: `e2e-harness/tests/e2e_two_node.rs`

- [ ] **Step 1: Add S3**

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s3_offline_channel_reconnect_catchup() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    let (mut run, ah, bh, alice, mut bob) = two_minted_nodes("s3").await;
    let bob_owner = owner_id(&bob).await;

    // Both in the community.
    let community = create_community(&alice, "s3-community", true).await.expect("create");
    let invite = generate_invite(&alice, &community).await.expect("invite");
    redeem_invite(&bob, &invite).await.expect("bob join");
    poll_until(Duration::from_secs(60), || async {
        Ok(roster_has_joined(&alice, &community, &bob_owner).await?.then_some(()))
    }).await.expect("alice sees bob joined");

    // Bob goes hard-offline (SIGKILL). Reuse his config to relaunch later.
    let bob_cfg = bob.config.clone();
    bob.kill().await.expect("kill bob");
    drop(bob);

    // Alice creates a channel while Bob is offline.
    let channel = create_channel(&alice, &community, "created-while-offline", 0).await.expect("create channel");

    // Bob comes back online against the SAME profile/data-dir.
    let bob = NodeHandle::spawn(bob_cfg).await.expect("relaunch bob");

    // Reconnect catch-up (ZEB-434): the new channel becomes visible to Bob.
    poll_until(Duration::from_secs(90), || async {
        Ok(channels_contains(&bob, &community, &channel).await?.then_some(()))
    }).await.expect("bob catches up the offline-created channel");

    run.mark_success();
    drop((alice, bob, ah, bh));
}
```

- [ ] **Step 2: Run S3**

```bash
cd e2e-harness && cargo nextest run --features e2e s3_offline_channel_reconnect_catchup
```
Expected: PASS — proves the ZEB-434 reconnect catch-up end-to-end with a real process kill (the clean headless "offline" the GUI never had). Allow up to 90s (catch-up has a backoff schedule). If it fails, the channel-set catch-up on reconnect regressed — a real finding; capture artifacts.

- [ ] **Step 3: Per-crate gate + commit**

```bash
cd e2e-harness && cargo fmt && cargo clippy --all-targets --features e2e -- -D warnings
git add e2e-harness/ && git commit -m "test(zeb-447): S3 offline channel -> reconnect catch-up"
```

---

### Task 9 (stretch): Scenario S4 — restart durability

**Files:**
- Modify: `e2e-harness/tests/e2e_two_node.rs`

Only do this task if S1–S3 are green and the harness is stable. It targets the ZEB-393 durability class.

- [ ] **Step 1: Add S4**

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s4_restart_durability() {
    use e2e_harness::driver::*;
    use std::time::Duration;

    let mut run = RunDir::new("s4").expect("run dir");
    let home = fresh_home("s4");
    let mut cfg = NodeConfig::new(PathBuf::from(home.path()), "alice");
    cfg.log_dir = Some(run.log_dir());

    let mut alice = NodeHandle::spawn(cfg.clone()).await.expect("spawn");
    mint(&alice).await.expect("mint");
    let community = create_community(&alice, "s4-durable", true).await.expect("create");

    // Hard kill shortly after create returns (stresses persistence-before-debounce).
    alice.kill().await.expect("kill");
    drop(alice);

    // Relaunch against the same profile/data-dir; the community must rehydrate.
    let alice = NodeHandle::spawn(cfg).await.expect("relaunch");
    poll_until(Duration::from_secs(60), || async {
        let comms = alice.rpc("list_owner_communities", serde_json::json!({})).await?;
        let found = comms.as_array().map(|a| a.iter().any(|c| c.get("id").and_then(|v| v.as_str()) == Some(community.as_str()))).unwrap_or(false);
        Ok(found.then_some(()))
    }).await.expect("community rehydrated after restart (ZEB-393)");

    run.mark_success();
    drop((alice, home));
}
```

- [ ] **Step 2: Run + gate + commit**

```bash
cd e2e-harness && cargo nextest run --features e2e s4_restart_durability
cargo fmt && cargo clippy --all-targets --features e2e -- -D warnings
git add e2e-harness/ && git commit -m "test(zeb-447): S4 (stretch) restart durability"
```
Expected: PASS. If the community does not rehydrate, that's a live ZEB-393-class durability finding — capture it; do not weaken the assertion to make it pass.

---

### Task 10: Cross-machine run playbook (dogfoods the coord instance)

**Files:**
- Create: `docs/playbooks/e2e-two-agent-suite.md`

This is the "agent pair" doc for the live Ildwyn↔AVALON proof (DoD item 3). It reuses the same scenario *logic* but with two `serve` instances on different machines, coordinated through the running `serve --profile coord` Harmony instance.

- [ ] **Step 1: Write the playbook**

Create `docs/playbooks/e2e-two-agent-suite.md` with these sections (full prose, not placeholders):

1. **Purpose & when to use** — proving S1–S3 across two physical machines (Koya/Ildwyn/AVALON) once AVALON is up (ZEB-444); the single-machine `e2e-harness` is the day-to-day path, this is the cross-WAN proof.
2. **Roles** — Agent A (machine 1) and Agent B (machine 2), each running one `harmony-app serve --profile <p>` and driving it with `harmony-app --profile <p> api <cmd> <json>`.
3. **Coordination channel (dogfood)** — both agents are members of a shared coordination community on the running `serve --profile coord` instance; they relay artifacts (invite URLs, friend tokens, owner ids) and turn-taking signals ("READY S1", "INVITE <url>", "JOINED") as messages there. Document the exact `api send_dm` / channel-post commands to post and `api --events` / `api read_dm_thread` to read. Manual relay (paste between transcripts) is the documented fallback.
4. **Per-scenario protocol** — for S1, S2, S3: the precondition setup, the ordered per-agent steps (who does what, what artifact to hand off, what to poll), the sync points, and the pass/fail assertion (same predicates as the Rust scenarios: roster `status:"Joined"`, friend `status:"active"`, DM body match, channel id present after reconnect).
5. **Offline in the cross-machine run** — "offline" = kill the remote `serve` process (real PID kill) and relaunch with the same `--profile`.
6. **Artifacts** — each agent collects `<data-dir>/logs/` + a transcript of the `api` calls; where to attach them on the ZEB-447 ticket.
7. **Reference: the single-machine harness** — point to `e2e-harness/README.md` and the four Rust scenarios as the canonical assertions.

- [ ] **Step 2: Commit**

```bash
git add docs/playbooks/e2e-two-agent-suite.md
git commit -m "docs(zeb-447): cross-machine two-agent E2E run playbook"
```

---

### Task 11: Final sweep + CI note

**Files:**
- Modify: `e2e-harness/README.md`

- [ ] **Step 1: Full crate gate**

```bash
cd e2e-harness
cargo fmt --all -- --check
cargo clippy --all-targets --features e2e -- -D warnings
```
Expected: clean.

- [ ] **Step 2: Run the whole suite once, serially (real transport — avoid port/discovery contention)**

```bash
cd src-tauri && cargo build --bin harmony-app && cd ../e2e-harness
cargo nextest run --features e2e --test-threads 1
```
Expected: S1, S2, S3 (+ S4 if built) PASS; artifacts cleaned on success.

- [ ] **Step 3: Document the CI shape in `e2e-harness/README.md`**

Append a `## CI` section: the suite is its own deliberately-invoked job (not on every push) that (1) builds `harmony-app`, (2) runs `cargo nextest run --features e2e --test-threads 1` from `e2e-harness/`, (3) uploads `target/e2e-runs/` on failure. Note it is excluded from the per-task `--lib` gate and from harmony-app's `--all-targets`.

- [ ] **Step 4: Commit**

```bash
cd e2e-harness && cargo fmt
git add e2e-harness/README.md && git commit -m "docs(zeb-447): final sweep notes + CI shape"
```

---

## Notes for the executor

- **Build the binary first.** Every `--features e2e` run needs `../src-tauri/target/{debug,release}/harmony-app`. Build it once up front; rebuild after any harmony-app change (there are none in this plan — the harness is external).
- **Generous timeouts.** Node boot + iroh first-bind can take ~30s on macOS; readiness + convergence polls allow 60–90s. Do not shorten them to "speed up" — they guard against real hangs, not flakes.
- **Serial scenario runs.** Real-transport scenarios should run `--test-threads 1` to avoid two scenarios contending on transport/discovery simultaneously. Per-scenario they're independent (own temp HOME + profiles).
- **Discovery by walk, not by path math.** Never hard-code the macOS `~/Library/Application Support/...` path; the `wait_for_api_dir` walk is intentional and cross-platform.
- **Keychain safety is non-negotiable.** Always named profiles + `HARMONY_PASSPHRASE`. Never spawn with the default profile — it would touch the developer's real OS keychain.
- **Findings are findings.** If S1 (first contact), S3 (catch-up), or S4 (durability) fails against real nodes, that is a real product finding — capture artifacts and surface it; never weaken an assertion to force green.
- **If an RPC contract is slightly off** (a field name, an enum case, the friend-accept semantics, or DM-space determinism), the captured node stderr log in the run dir shows the real error string (identical to the GUI's). Fix the driver helper to match the observed contract and note the correction in the task's step.
