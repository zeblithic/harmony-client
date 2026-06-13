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
    let exe = if cfg!(windows) {
        "harmony-app.exe"
    } else {
        "harmony-app"
    };
    for profile in ["release", "debug"] {
        let cand = manifest
            .join("..")
            .join("src-tauri")
            .join("target")
            .join(profile)
            .join(exe);
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
