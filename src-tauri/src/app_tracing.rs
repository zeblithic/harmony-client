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

/// ZEB-901: the fallback log filter used when `RUST_LOG` is unset — `info` plus
/// a span-directive that suppresses iroh's `QADv6` (QUIC Address Discovery over
/// IPv6) events at WARN and below. On IPv4-only hosts iroh probes n0 relays'
/// IPv6 addresses every ~75s, each failing `HostUnreachable`; the WARNs are
/// benign but bury real warnings during triage. Scoped to the `QADv6` span, so
/// the v4 paths and every other `noq_udp` send keep full WARN visibility. An
/// explicit `RUST_LOG` overrides this wholesale — the operator owns it.
///
/// Shared by both `EnvFilter` fallback sites (this module's subscriber and
/// `main.rs::init_tracing`) so the directive lives in exactly one place;
/// re-exported at the crate root as `harmony_app::DEFAULT_ENV_FILTER` for the
/// binary crate (`main.rs`), which can't see this private module directly.
pub const DEFAULT_ENV_FILTER: &str = "info,[QADv6]=error";

/// Pure path join — the profile-aware app-data dir + `/logs`. Split out
/// from `log_dir` so it can be unit-tested deterministically without
/// depending on the host's data dir. ZEB-446: delegates to the same
/// `app_data_dir_in` join `resolve_app_data_dir` uses, so logs always
/// live inside the active profile's app-data tree.
fn log_dir_in(base: &Path, profile: Option<&str>) -> PathBuf {
    crate::app_data_dir_in(base, profile).join("logs")
}

/// Directory the rolling log files live in:
/// `dirs::data_dir()/net.zeblith.harmony[/profiles/<p>]/logs`, byte-identical
/// to Tauri v2's `app_data_dir()/logs` on the default profile. `None` when
/// the platform data dir can't be resolved.
fn log_dir() -> Option<PathBuf> {
    Some(log_dir_in(
        &dirs::data_dir()?,
        crate::profile::active_profile(),
    ))
}

/// Install the global tracing subscriber for the desktop GUI. Idempotent: a
/// second call (e.g. a CLI arm already initialized one in-process) is a no-op,
/// never a panic. Degrades to stdout-only if the log dir can't be created.
pub fn init_app_tracing() {
    install_subscriber(log_dir());
}

/// ZEB-445: serve-mode subscriber. Identical layering to [`init_app_tracing`]
/// (same EnvFilter + same rolling file layer) EXCEPT the console fmt layer
/// writes to **stderr** — serve mode shares the CLI arms' stdout-purity
/// discipline (ZEB-430, see `init_tracing` in main.rs): stdout stays
/// machine-readable, log lines never interleave.
pub fn init_serve_tracing() {
    install_subscriber_to(log_dir(), ConsoleTarget::Stderr);
}

/// Where the console fmt layer writes. The GUI uses stdout (visible under
/// `cargo tauri dev`); serve mode uses stderr (ZEB-430 stdout purity).
enum ConsoleTarget {
    Stdout,
    Stderr,
}

/// Core installer, parameterized on the log directory so tests can pass `None`
/// (stdout-only, zero filesystem side effects). Uses `try_init()`, discarding
/// the error so a double init never panics.
fn install_subscriber(log_dir: Option<PathBuf>) {
    install_subscriber_to(log_dir, ConsoleTarget::Stdout);
}

/// Shared installer body, additionally parameterized on the console target so
/// `init_serve_tracing` reuses the exact same EnvFilter + file-layer wiring.
fn install_subscriber_to(log_dir: Option<PathBuf>, console: ConsoleTarget) {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_ENV_FILTER));

    // Console always; rolling file when a usable log dir is available.
    let mut layers: Vec<Box<dyn Layer<_> + Send + Sync + 'static>> = Vec::new();
    layers.push(match console {
        ConsoleTarget::Stdout => fmt::layer().boxed(),
        ConsoleTarget::Stderr => fmt::layer().with_writer(std::io::stderr).boxed(),
    });

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
        // `<base>/net.zeblith.harmony[/profiles/<p>]/logs`.
        let base = Path::new("base");
        assert_eq!(
            log_dir_in(base, None),
            base.join("net.zeblith.harmony").join("logs")
        );
        assert_eq!(
            log_dir_in(base, Some("coord")),
            base.join("net.zeblith.harmony")
                .join("profiles")
                .join("coord")
                .join("logs")
        );
    }

    #[test]
    fn default_env_filter_suppresses_qadv6_probe_noise() {
        // ZEB-901: the shared default filter must carry the QADv6 span
        // suppression. `EnvFilter::new` parses lossily (a malformed directive is
        // silently dropped, not an error), so guard the directive syntax itself
        // — otherwise a typo would leave the v6-probe WARN spam un-suppressed.
        assert!(
            "[QADv6]=error"
                .parse::<tracing_subscriber::filter::Directive>()
                .is_ok(),
            "QADv6 suppression directive must be well-formed"
        );
        assert!(
            DEFAULT_ENV_FILTER.contains("[QADv6]=error"),
            "default filter must suppress the QADv6 IPv6-probe noise (ZEB-901)"
        );
        // The full default string must also build as a filter.
        let _ = EnvFilter::new(DEFAULT_ENV_FILTER);
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
