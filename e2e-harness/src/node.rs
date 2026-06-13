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
                let out =
                    std::fs::File::create(dir.join(format!("{}.stdout.log", config.profile)))?;
                let err =
                    std::fs::File::create(dir.join(format!("{}.stderr.log", config.profile)))?;
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
        let token = tokio::fs::read_to_string(api_dir.join("token"))
            .await?
            .trim()
            .to_string();
        let base_url = format!("http://127.0.0.1:{port}");
        let http = reqwest::Client::new();

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
            anyhow::bail!(
                "discovery files (api/port, api/token) never appeared under {}",
                home.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

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
