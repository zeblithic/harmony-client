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
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            std::process::id()
        );
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("e2e-runs")
            .join(format!("{scenario}-{runid}"));
        std::fs::create_dir_all(&path)?;
        let keep = std::env::var("HARMONY_E2E_KEEP")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false);
        Ok(Self {
            path,
            keep,
            succeeded: false,
        })
    }

    /// Path for a node's captured stdout/stderr (pass into `NodeConfig.log_dir`).
    pub fn log_dir(&self) -> PathBuf {
        self.path.clone()
    }

    pub fn write_jsonl(&self, name: &str, value: &serde_json::Value) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path.join(name))
        {
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
