//! Locate the built `harmony-app` binary. Because this is a standalone crate
//! (not a harmony-app test target), `CARGO_BIN_EXE_harmony-app` is unavailable.

use std::path::{Path, PathBuf};

/// Resolve the `harmony-app` binary path, gated by a freshness check.
///
/// Priority: `HARMONY_APP_BIN` env override, else `../src-tauri/target/release`
/// then `../src-tauri/target/debug`, relative to this crate's manifest dir.
/// Returns an error (never a silent skip) if none exists. After locating the
/// binary, [`check_freshness`] hard-fails a stale artifact (ZEB-690).
pub fn resolve_harmony_app_bin() -> anyhow::Result<PathBuf> {
    let path = locate_harmony_app_bin()?;
    if !freshness_disabled() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let src_tauri = manifest.join("..").join("src-tauri");
        check_freshness(&path, &src_tauri)?;
    }
    Ok(path)
}

fn locate_harmony_app_bin() -> anyhow::Result<PathBuf> {
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

/// ZEB-690: `HARMONY_APP_FRESHNESS=off` disables the stale-binary guard.
fn freshness_disabled() -> bool {
    std::env::var("HARMONY_APP_FRESHNESS").ok().as_deref() == Some("off")
}

/// ZEB-690: guard against silently testing a stale `harmony-app`. `cargo nextest
/// --features e2e` rebuilds the harness but NOT the spawned binary, so a days-old
/// artifact can shadow freshly-edited source (this invalidated s7 gates on the
/// ZEB-510 branch). Hard-fail when `bin` is older than the newest first-party
/// source under `src_tauri` (`src/**/*.rs` + Cargo.toml/Cargo.lock/build.rs,
/// excluding target/ & vendor/). Skips (Ok) when the tree isn't locatable —
/// never a false failure in installed/packaged contexts.
fn check_freshness(bin: &Path, src_tauri: &Path) -> anyhow::Result<()> {
    let src_dir = src_tauri.join("src");
    if !src_dir.is_dir() {
        return Ok(()); // source tree not locatable → skip.
    }
    let bin_mtime = match std::fs::metadata(bin).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return Ok(()), // can't stat the binary → don't block.
    };
    let mut newest = newest_source_under(&src_dir);
    for f in ["Cargo.toml", "Cargo.lock", "build.rs"] {
        let p = src_tauri.join(f);
        if let Ok(t) = std::fs::metadata(&p).and_then(|m| m.modified()) {
            if newest.as_ref().is_none_or(|(n, _)| t > *n) {
                newest = Some((t, p));
            }
        }
    }
    if let Some((newest_t, newest_p)) = newest {
        if bin_mtime < newest_t {
            anyhow::bail!(
                "stale harmony-app binary: {}\n  is older than source: {}\n\
                 Rebuild it:  cd src-tauri && cargo build --bin harmony-app\n\
                 (bypass with HARMONY_APP_FRESHNESS=off)",
                bin.display(),
                newest_p.display()
            );
        }
    }
    Ok(())
}

/// Newest (mtime, path) among `*.rs` files under `dir`. Callers pass
/// `src-tauri/src`, so `target/` and `vendor/` (siblings of `src`, under
/// `src-tauri/`) are already outside this walk — no explicit prune needed, and
/// pruning by dir name here would wrongly skip a legitimate first-party
/// `src/target|vendor/` module. `None` if the dir has no readable `.rs` files.
fn newest_source_under(dir: &Path) -> Option<(std::time::SystemTime, PathBuf)> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_type().is_file() && e.path().extension().and_then(|x| x.to_str()) == Some("rs")
        })
        .filter_map(|e| {
            let t = std::fs::metadata(e.path())
                .and_then(|m| m.modified())
                .ok()?;
            Some((t, e.path().to_path_buf()))
        })
        .max_by_key(|(t, _)| *t)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;
    use std::time::{Duration, UNIX_EPOCH};

    /// Serializes the process-global env-var mutation in the tests below so they
    /// are safe under any test runner (nextest isolates by process, but plain
    /// `cargo test` shares threads). Poison-tolerant: a panicking test must not
    /// wedge the others.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn env_override_to_missing_file_errors() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("HARMONY_APP_BIN", "/definitely/not/here/harmony-app");
        let err = resolve_harmony_app_bin().unwrap_err().to_string();
        std::env::remove_var("HARMONY_APP_BIN");
        assert!(err.contains("not a file"), "got: {err}");
    }

    fn set_mtime(p: &Path, unix_secs: u64) {
        let t = UNIX_EPOCH + Duration::from_secs(unix_secs);
        std::fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    /// A src_tauri tempdir with `src/lib.rs` at `src_secs` and a binary at
    /// `bin_secs`. Returns (src_tauri_dir, bin_path); keep the TempDir alive.
    fn fixture(src_secs: u64, bin_secs: u64) -> (tempfile::TempDir, PathBuf) {
        let td = tempfile::tempdir().unwrap();
        let src = td.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let source = src.join("lib.rs");
        std::fs::write(&source, "// source").unwrap();
        set_mtime(&source, src_secs);
        let bin = td.path().join("harmony-app");
        std::fs::write(&bin, "ELF").unwrap();
        set_mtime(&bin, bin_secs);
        (td, bin)
    }

    #[test]
    fn fresh_binary_passes_freshness() {
        let (td, bin) = fixture(1_000, 2_000); // binary NEWER than source
        assert!(check_freshness(&bin, td.path()).is_ok());
    }

    #[test]
    fn stale_binary_fails_freshness() {
        let (td, bin) = fixture(2_000, 1_000); // binary OLDER than source
        let err = check_freshness(&bin, td.path()).unwrap_err().to_string();
        assert!(err.contains("stale harmony-app binary"), "got: {err}");
        assert!(
            err.contains("lib.rs"),
            "error names the newer source: {err}"
        );
    }

    #[test]
    fn missing_source_tree_skips_freshness() {
        // src_tauri dir with NO src/ subdir → not locatable → skip (Ok).
        let td = tempfile::tempdir().unwrap();
        let bin = td.path().join("harmony-app");
        std::fs::write(&bin, "ELF").unwrap();
        set_mtime(&bin, 1_000);
        assert!(check_freshness(&bin, td.path()).is_ok());
    }

    #[test]
    fn manifest_change_also_trips_freshness() {
        // A newer Cargo.toml (not just .rs) must trip the guard.
        let (td, bin) = fixture(1_000, 2_000); // src older, bin newer
        let manifest = td.path().join("Cargo.toml");
        std::fs::write(&manifest, "[package]").unwrap();
        set_mtime(&manifest, 3_000); // manifest NEWER than binary
        let err = check_freshness(&bin, td.path()).unwrap_err().to_string();
        assert!(err.contains("Cargo.toml"), "got: {err}");
    }

    #[test]
    fn freshness_disabled_reads_env() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("HARMONY_APP_FRESHNESS");
        assert!(!freshness_disabled());
        std::env::set_var("HARMONY_APP_FRESHNESS", "off");
        assert!(freshness_disabled());
        std::env::remove_var("HARMONY_APP_FRESHNESS");
    }
}
