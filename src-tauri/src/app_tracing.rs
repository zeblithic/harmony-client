//! ZEB-379: tracing subscriber for the desktop GUI entrypoint.
//!
//! `harmony_app::run()` historically installed no `tracing` subscriber, so
//! `RUST_LOG` was inert in the shipped app and zero runtime logs were emitted
//! (the only `init_tracing()` lives in the CLI arms of `main.rs`). This module
//! installs a subscriber from `run()` that writes to **stdout** (visible under
//! `cargo tauri dev`) and to a daily-rolling **file** under the app-data dir so
//! external testers can attach logs to feedback (the desktop app has no console).

use std::path::{Path, PathBuf};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter, Layer};

/// Bundle identifier from `tauri.conf.json` (`"identifier"`). Tauri v2 keys
/// `app_data_dir()` on this, so `dirs::data_dir()/<APP_IDENTIFIER>` reproduces
/// the same directory without a built `App` handle. Keep in sync with
/// `tauri.conf.json`.
const APP_IDENTIFIER: &str = "net.zeblith.harmony";

/// Pure path join — `<base>/net.zeblith.harmony/logs`. Split out from `log_dir`
/// so it can be unit-tested deterministically without depending on the host's
/// data dir.
fn log_dir_in(base: &Path) -> PathBuf {
    base.join(APP_IDENTIFIER).join("logs")
}

/// Directory the rolling log files live in:
/// `dirs::data_dir()/net.zeblith.harmony/logs`, byte-identical to Tauri v2's
/// `app_data_dir()/logs` on macOS / Windows / Linux. `None` when the platform
/// data dir can't be resolved.
fn log_dir() -> Option<PathBuf> {
    Some(log_dir_in(&dirs::data_dir()?))
}

/// Install the global tracing subscriber for the desktop GUI. Idempotent: a
/// second call (e.g. a CLI arm already initialized one in-process) is a no-op,
/// never a panic. Degrades to stdout-only if the log dir can't be created.
pub fn init_app_tracing() {
    install_subscriber(log_dir());
}

/// Core installer, parameterized on the log directory so tests can pass `None`
/// (stdout-only, zero filesystem side effects). Uses `try_init()`, discarding
/// the error so a double init never panics.
fn install_subscriber(log_dir: Option<PathBuf>) {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // Stdout always; rolling file when a usable log dir is available.
    let mut layers: Vec<Box<dyn Layer<_> + Send + Sync + 'static>> = Vec::new();
    layers.push(fmt::layer().boxed());

    if let Some(dir) = log_dir {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("harmony: cannot create log dir {}: {e}", dir.display());
        } else {
            match tracing_appender::rolling::Builder::new()
                .rotation(tracing_appender::rolling::Rotation::DAILY)
                .filename_prefix("harmony")
                .filename_suffix("log")
                .max_log_files(7)
                .build(&dir)
            {
                // Synchronous writer: RollingFileAppender implements MakeWriter
                // directly. We deliberately skip tracing_appender::non_blocking —
                // its WorkerGuard would have to be parked for the process lifetime
                // and is never dropped (Rust statics aren't), so the final buffered
                // lines wouldn't flush at shutdown anyway. A desktop GUI logs at low
                // enough volume that synchronous writes are fine, and the last line
                // before a crash is already on disk — better for diagnostics.
                Ok(appender) => {
                    layers.push(fmt::layer().with_ansi(false).with_writer(appender).boxed());
                }
                Err(e) => eprintln!("harmony: cannot build rolling log appender: {e}"),
            }
        }
    }

    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(layers)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_dir_in_is_base_then_identifier_then_logs() {
        // Deterministic: no dependency on the host data dir. Pins the structure
        // `<base>/net.zeblith.harmony/logs` (and the literal identifier).
        let base = Path::new("base");
        assert_eq!(
            log_dir_in(base),
            base.join("net.zeblith.harmony").join("logs")
        );
    }

    #[test]
    fn install_subscriber_none_is_idempotent() {
        // `None` => stdout-only, no filesystem side effects. Calling twice must
        // not panic even though the second hits an already-set global subscriber
        // (try_init swallows the error). nextest isolates each test in its own
        // process, so this global subscriber does not leak to other tests.
        install_subscriber(None);
        install_subscriber(None);
    }
}
