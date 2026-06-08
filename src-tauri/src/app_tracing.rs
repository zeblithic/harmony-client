//! ZEB-379: tracing subscriber for the desktop GUI entrypoint.
//!
//! `harmony_app::run()` historically installed no `tracing` subscriber, so
//! `RUST_LOG` was inert in the shipped app and zero runtime logs were emitted
//! (the only `init_tracing()` lives in the CLI arms of `main.rs`). This module
//! installs a subscriber from `run()` that writes to **stdout** (visible under
//! `cargo tauri dev`) and to a daily-rolling **file** under the app-data dir so
//! external testers can attach logs to feedback (the desktop app has no console).

use std::path::PathBuf;
use std::sync::OnceLock;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter, Layer};

/// Bundle identifier from `tauri.conf.json` (`"identifier"`). Tauri v2 keys
/// `app_data_dir()` on this, so `dirs::data_dir()/<APP_IDENTIFIER>` reproduces
/// the same directory without a built `App` handle. Keep in sync with
/// `tauri.conf.json`.
const APP_IDENTIFIER: &str = "net.zeblith.harmony";

/// Keeps the non-blocking file-writer worker alive for the process lifetime.
/// `tracing_appender::non_blocking` drops buffered lines when its `WorkerGuard`
/// is dropped, so the guard must outlive the app.
static LOG_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// Directory the rolling log files live in:
/// `dirs::data_dir()/net.zeblith.harmony/logs`, byte-identical to Tauri v2's
/// `app_data_dir()/logs` on macOS / Windows / Linux. `None` when the platform
/// data dir can't be resolved.
fn log_dir() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join(APP_IDENTIFIER).join("logs"))
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
                Ok(appender) => {
                    let (non_blocking, guard) = tracing_appender::non_blocking(appender);
                    // Park the guard for the process lifetime so buffered lines flush.
                    let _ = LOG_GUARD.set(guard);
                    layers.push(
                        fmt::layer()
                            .with_ansi(false)
                            .with_writer(non_blocking)
                            .boxed(),
                    );
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
    fn log_dir_is_under_identifier_and_logs() {
        // dirs::data_dir() resolves on all supported platforms (incl. CI, which
        // sets HOME). If the path logic regresses, this fails.
        let dir = log_dir().expect("platform data dir resolvable");
        assert!(
            dir.ends_with("logs"),
            "log dir must end with /logs: {dir:?}"
        );
        assert!(
            dir.to_string_lossy().contains(APP_IDENTIFIER),
            "log dir must be under the bundle identifier: {dir:?}"
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
