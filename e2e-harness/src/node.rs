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
    /// ZEB-720: extra env vars injected into the spawned node (e.g. short
    /// voting cadence). Layered on top of the hardcoded `.env(...)` set in
    /// `spawn`; the child already inherits the parent env (no env_clear).
    pub extra_env: Vec<(String, String)>,
}

impl NodeConfig {
    pub fn new(home: PathBuf, profile: &str) -> Self {
        Self {
            home,
            profile: profile.to_string(),
            passphrase: "e2e-test-passphrase".to_string(),
            log_dir: None,
            extra_env: Vec::new(),
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
        // Relaunch hygiene: a SIGKILL'd previous run (e.g. the offline step of a
        // restart/catch-up scenario) leaves its `api/{port,token}` discovery files
        // on disk (only a graceful shutdown removes them). Without clearing them,
        // `wait_for_api_dir` would instantly return the DEAD process's port/token
        // and we'd poll `/v1/status` on a dead port forever. Remove them so we
        // wait for the freshly-spawned process to write its own.
        remove_stale_discovery_files(&config.home);
        let (stdout, stderr) = match &config.log_dir {
            Some(dir) => {
                tokio::fs::create_dir_all(dir)
                    .await
                    .with_context(|| format!("creating log dir {}", dir.display()))?;
                let out =
                    std::fs::File::create(dir.join(format!("{}.stdout.log", config.profile)))?;
                let err = std::fs::File::create(dir.join(stderr_log_filename(&config.profile)))?;
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
            // App-data dir: pin it UNDER the temp HOME so the node's
            // `api/{port,token}` always land where `wait_for_api_dir` walks, even
            // when the dev/CI env already sets data-dir vars (CodeAnt/Qodo: data-dir
            // not isolated). ZEB-465: the old `.env("APPDATA", …)` override was a
            // NO-OP on Windows — `dirs::data_dir()` (v6) reads the Roaming AppData
            // *known folder* via Win32, ignores `APPDATA`, and returned None in this
            // stripped child env, so `serve` aborted at boot with "cannot resolve
            // platform data dir". `HARMONY_DATA_DIR` is the deterministic cross-OS
            // override the node honors before `dirs`; XDG_DATA_HOME stays as
            // belt-and-braces for the Linux path (the override wins everywhere).
            .env("HARMONY_DATA_DIR", config.home.join("data"))
            .env("XDG_DATA_HOME", config.home.join("xdg-data"))
            .env("HARMONY_PASSPHRASE", &config.passphrase)
            .env("HARMONY_RETICULUM_PORT", "0")
            .env("HARMONY_API_PORT", "0")
            // ZEB-809: production disables zenoh LAN scouting by default; a
            // runner whose shell exports the opt-in would silently re-enable it
            // for every spawned node, handing co-located tests a peer path
            // production doesn't have (exactly the false-positive the old s5c
            // skip-guard existed to avoid). Strip it so hermeticity doesn't
            // depend on the runner's environment. Layered BEFORE `extra_env`,
            // so a test that deliberately wants scouting can still set it
            // per-node.
            .env_remove("HARMONY_ZENOH_ENABLE_LAN_SCOUTING")
            // ZEB-720: per-node extra env (e.g. short voting cadence), layered
            // LAST — chained after the isolation `.env(...)` calls above, so a
            // colliding key would win. Keep `extra_env` to non-isolation vars
            // (today only the two HARMONY_VOTING_* cadence knobs).
            .envs(
                config
                    .extra_env
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str())),
            )
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
        let token = tokio::fs::read_to_string(api_dir.join("token"))
            .await?
            .trim()
            .to_string();
        let base_url = format!("http://127.0.0.1:{port}");
        // Per-request timeout so a wedged server surfaces as an error instead of
        // hanging an `rpc()`/`status()` await indefinitely (Qodo: no HTTP timeout).
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building reqwest client")?;

        let handle = Self {
            config,
            port,
            token,
            base_url,
            child: Some(child),
            http,
        };
        handle
            .wait_until_status_running(Duration::from_secs(90))
            .await?;
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
        // `text()` can fail after a 200 (truncated/dropped body); propagate it
        // instead of masking a transport failure as an empty body (CodeRabbit).
        let body = resp.text().await.with_context(|| {
            format!(
                "rpc {cmd}: reading response body (HTTP {})",
                status.as_u16()
            )
        })?;
        if status.as_u16() == 200 {
            // An empty 200 body is a legitimate null result (e.g. start_node);
            // a non-empty body that fails to parse is a real error, not a silent
            // null (Qodo: rpc hides parse errors).
            if body.trim().is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&body)
                .with_context(|| format!("rpc {cmd}: 200 body is not valid JSON: {body}"));
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
        // Check the HTTP code (a 401/500 is a real server error, not a non-ready
        // node) and propagate body-read failures (Cursor: status skips code check).
        let code = resp.status();
        let body = resp
            .text()
            .await
            .with_context(|| format!("status: reading response body (HTTP {})", code.as_u16()))?;
        if code.as_u16() != 200 {
            anyhow::bail!("status -> HTTP {}: {body}", code.as_u16());
        }
        serde_json::from_str(&body)
            .with_context(|| format!("status: body is not valid JSON: {body}"))
    }

    async fn wait_until_status_running(&self, timeout: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;
        // Remember the last status error so a persistent server error (e.g. a 401)
        // surfaces its message on timeout instead of a bare "never running".
        // Connection-refused while the node is still binding is the expected early
        // case and is simply retried.
        let mut last_err = String::from("(status never answered)");
        loop {
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "node {} never reported running=true within {timeout:?}; last status: {last_err}",
                    self.config.profile
                );
            }
            match self.status().await {
                Ok(s) if s.get("running").and_then(Value::as_bool) == Some(true) => return Ok(()),
                Ok(_) => last_err = "running=false".to_string(),
                Err(e) => last_err = e.to_string(),
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

    /// Take the node offline (real process kill, if still alive) and bring it
    /// back from the SAME config so its on-disk identity + app-data rehydrate.
    /// Returns a fresh handle (new port/token after re-discovery). Models the
    /// offline→online half of the ZEB-487 deposit→recover scenario.
    pub async fn relaunch(mut self) -> anyhow::Result<Self> {
        let _ = self.kill().await;
        NodeHandle::spawn(self.config.clone()).await
    }
}

impl Drop for NodeHandle {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

/// Single source of the per-profile stderr capture filename — used by BOTH the
/// spawn-side writer and `stderr_log_contains`, so the two can't drift
/// (CodeRabbit #671).
fn stderr_log_filename(profile: &str) -> String {
    format!("{profile}.stderr.log")
}

/// Strip ANSI SGR escape sequences (`\x1b[ … m`) from a log line so field-scoped
/// substring matches aren't split by tracing's colour codes. Non-`m` CSI final
/// bytes are tolerated too; anything after `\x1b[` up to the final byte is
/// dropped (ZEB-927).
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Consume the CSI sequence up to and including its final byte
            // (an ASCII letter for SGR/`m`); if there's no `[`, drop the ESC.
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Remove any stale `api/port` + `api/token` discovery files under `home` left
/// by a previously-killed process, so a relaunch waits for the new process's
/// fresh files instead of latching onto the dead one's port/token.
fn remove_stale_discovery_files(home: &std::path::Path) {
    for entry in walkdir::WalkDir::new(home).into_iter().flatten() {
        let name = entry.file_name();
        if (name == "port" || name == "token")
            && entry
                .path()
                .parent()
                .and_then(|p| p.file_name())
                .map(|f| f == "api")
                .unwrap_or(false)
        {
            let _ = std::fs::remove_file(entry.path());
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
            anyhow::bail!(
                "discovery files (api/port, api/token) never appeared under {}",
                home.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

impl NodeHandle {
    /// ZEB-912: the node's iroh node id (64-hex) from `/v1/status.nodeId`.
    /// `Ok(None)` while the node is (re)booting — poll if you need it settled —
    /// but ALSO when the node runs DEGRADED with iroh transport down (iroh boot
    /// failure is non-fatal server-side), so a poll that never settles means
    /// transport-init failure, not slowness (Greptile PR #671). A MISSING field
    /// is a loud error (a binary predating the field), never a silent `None` —
    /// the camelCase-trap discipline (ZEB-462).
    pub async fn node_id(&self) -> anyhow::Result<Option<String>> {
        let s = self.status().await?;
        match s.get("nodeId") {
            None => {
                anyhow::bail!("/v1/status has no nodeId field (stale harmony-app binary?): {s}")
            }
            Some(Value::Null) => Ok(None),
            Some(v) => Ok(Some(
                v.as_str()
                    .with_context(|| format!("nodeId is not a string: {s}"))?
                    .to_string(),
            )),
        }
    }

    /// ZEB-912: does this node's captured stderr contain `needle`? Requires the
    /// node to have been spawned with `log_dir` set (the run-dir capture).
    /// Reads the live file — poll it; a needle mid-write can miss one tick.
    pub fn stderr_log_contains(&self, needle: &str) -> anyhow::Result<bool> {
        let dir = self
            .config
            .log_dir
            .as_ref()
            .context("stderr_log_contains requires log_dir capture")?;
        let path = dir.join(stderr_log_filename(&self.config.profile));
        // The file is being written live: a tail cut mid-UTF-8 must not turn a
        // poll tick into a hard error (read_to_string -> InvalidData would
        // abort a poll_until through `?`), so decode lossily. (CodeRabbit #671.)
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        Ok(String::from_utf8_lossy(&bytes).contains(needle))
    }

    /// ZEB-927: does any single captured stderr LINE contain every needle?
    /// Stronger than [`Self::stderr_log_contains`] when a field must belong to a
    /// specific log event — e.g. asserting a denylist message names a specific
    /// peer id, rather than matching the message and the id on unrelated lines.
    /// ANSI SGR codes are stripped before matching (tracing's default fmt colours
    /// field names/values, so `peer=<hex>` and `entries=2` are split by escape
    /// sequences on the raw line). Lossy-decodes like `stderr_log_contains`.
    pub fn stderr_log_line_contains_all(&self, needles: &[&str]) -> anyhow::Result<bool> {
        let dir = self
            .config
            .log_dir
            .as_ref()
            .context("stderr_log_line_contains_all requires log_dir capture")?;
        let path = dir.join(stderr_log_filename(&self.config.profile));
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        Ok(String::from_utf8_lossy(&bytes).lines().any(|line| {
            let clean = strip_ansi(line);
            needles.iter().all(|n| clean.contains(n))
        }))
    }

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
