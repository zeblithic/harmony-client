//! ZEB-695: durable panic capture for diagnosing zenoh's masked `SIGABRT` flake.
//!
//! # Why this exists
//!
//! zenoh 1.9.0's `zlock!` / `zread!` / `zwrite!` macros are `.lock()/.read()/
//! .write().unwrap()` — so a panic *while holding* one of zenoh's routing locks
//! (`Tables::ctrl_lock`, `Tables::tables`) poisons it. A later `zlock!` on the
//! poisoned lock — reached on a `Drop`/teardown path during concurrent session
//! close — then re-panics on the `PoisonError`. Because that re-panic runs in a
//! non-unwinding (drop-during-unwind) context, Rust escalates it to `SIGABRT`,
//! and the abort **erases the ORIGINAL (causal) panic** from the captured
//! stderr. That first panic — the one that poisoned the lock — is the real bug,
//! and it has never been observed (see ZEB-695).
//!
//! The flake is rare (~1 per month across full sweeps), passes on rerun, and its
//! root cause lives in the upstream `zenoh` crate we build from the registry, so
//! it is not reproducible on demand and not fixable without widening the
//! vendored surface. The proportionate, in-repo response is therefore *not* to
//! guess a fix but to make the masked first panic **capturable**: install a
//! panic hook that appends every panic — thread name, location, message, and a
//! forced backtrace — to a flushed file *before* the abort can truncate stderr.
//! The next natural recurrence (a CI shard or a local sweep) then records the
//! causal panic and the exact `zenoh` frame that poisoned the lock.
//!
//! # Behaviour
//!
//! - The previous hook is **chained**, so normal stderr panic output is
//!   preserved — this only *adds* a durable side-record.
//! - Installed exactly once per process (`Once`), so callers on hot-ish paths
//!   (e.g. every zenoh session open) may call [`install_once`] unconditionally.
//! - Gated: installs only in debug builds (`debug_assertions`, which covers all
//!   `cargo test` / `cargo nextest` runs and CI) or when `HARMONY_PANIC_LOG` is
//!   set. A release build with the env unset is left completely untouched.
//! - The capture path never panics itself (all I/O errors are swallowed) — it
//!   must never be the thing that turns a recoverable panic into an abort.
//!
//! # Where the record lands
//!
//! `HARMONY_PANIC_LOG` (verbatim) if set, else a per-process file
//! `<temp_dir>/harmony-panics-<pid>-<nanos>.log`. The `<nanos>` (process-start
//! monotonic-ish suffix) makes the default name unique per launch — so a
//! recycled PID never appends to a stale run's file — and unguessable, so
//! nothing can pre-create it under our path on a shared box; on Unix it is
//! created mode `0600` (owner-only). Records are appended (never truncated) and
//! each open→write→flush is serialized by a process-global lock, so a
//! multi-panic abort sequence — the causal panic followed by the drop-path
//! re-panic — is preserved intact and in order within one process's file. After
//! a failed sweep, find the aborting process's file with e.g.
//! `grep -l gateway.rs "$TMPDIR"/harmony-panics-*.log`.

use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Env var naming the capture-log path; its mere presence also force-enables the
/// hook in release builds (for field diagnosis of an aborting node).
const ENV_LOG_PATH: &str = "HARMONY_PANIC_LOG";

static INSTALL: Once = Once::new();

/// Serializes the open→write→flush of one record. `write_all` under `O_APPEND`
/// is *not* atomic (it may split a multi-line record across `write` calls), so
/// without this two threads panicking near-simultaneously in the same process
/// could interleave and corrupt the very backtrace we exist to preserve.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// The default capture-log path, computed once per process and cached. Kept
/// stable across every panic in the process so a multi-panic abort sequence all
/// lands in one file.
static DEFAULT_LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Install the durable panic-capture hook exactly once per process.
///
/// Idempotent and cheap to call repeatedly (guarded by a `Once` plus a cheap
/// gate check), so it is safe to call on every zenoh session open. No-ops unless
/// this is a debug build or [`ENV_LOG_PATH`] is set.
pub fn install_once() {
    if !should_install() {
        return;
    }
    INSTALL.call_once(|| {
        // Chain the existing hook so default stderr reporting still happens.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Force a backtrace regardless of `RUST_BACKTRACE` (unset in CI) —
            // the backtrace is what names the zenoh frame that poisoned the lock.
            let backtrace = std::backtrace::Backtrace::force_capture().to_string();
            let location = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "<unknown location>".to_string());
            let record = format_record(
                unix_millis(),
                std::process::id(),
                &thread_label(),
                &location,
                &payload_message(info.payload()),
                &backtrace,
            );
            // Never let capture abort the process: swallow every I/O error.
            let _ = append_record(&log_path(), &record);
            previous(info);
        }));
    });
}

/// Whether the hook should be installed in this build/environment.
fn should_install() -> bool {
    cfg!(debug_assertions) || std::env::var_os(ENV_LOG_PATH).is_some()
}

/// Capture-log destination: `HARMONY_PANIC_LOG` verbatim (caller-managed) if
/// set, else a cached, per-process default under the temp dir.
fn log_path() -> PathBuf {
    match std::env::var_os(ENV_LOG_PATH) {
        Some(p) => PathBuf::from(p),
        None => default_log_path().clone(),
    }
}

/// The cached default path: `<temp_dir>/harmony-panics-<pid>-<nanos>.log`. The
/// nanosecond suffix (sampled once, on first use) makes the name unique per
/// process launch — a recycled PID can't append to a prior run's file — and
/// unpredictable, so nothing can plant a file/symlink under our path in advance.
fn default_log_path() -> &'static PathBuf {
    DEFAULT_LOG_PATH.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("harmony-panics-{}-{nanos}.log", std::process::id()))
    })
}

/// A human-readable label for the current thread — the poisoner is on a zenoh
/// `net-N` worker, so the name is a primary triage signal.
fn thread_label() -> String {
    let current = std::thread::current();
    match current.name() {
        Some(name) => format!("{name} ({:?})", current.id()),
        None => format!("<unnamed> ({:?})", current.id()),
    }
}

/// Best-effort extraction of a panic payload's message (the two shapes the std
/// panic machinery produces: `&str` for `panic!("literal")`, `String` for
/// formatted payloads).
fn payload_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Milliseconds since the Unix epoch (0 on the impossible pre-epoch clock).
fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Format one panic record. Pure (no I/O, no globals) so it is directly
/// testable; the delimiters make the causal panic greppable in a full sweep log.
fn format_record(
    ts_millis: u128,
    pid: u32,
    thread: &str,
    location: &str,
    message: &str,
    backtrace: &str,
) -> String {
    format!(
        "\n===== HARMONY PANIC (ZEB-695) t={ts_millis}ms pid={pid} =====\n\
         thread : {thread}\n\
         at     : {location}\n\
         message: {message}\n\
         backtrace:\n{backtrace}\n\
         ===== END HARMONY PANIC =====\n"
    )
}

/// Append a record to the capture log, creating it if needed and flushing so the
/// bytes survive an imminent abort.
///
/// The whole open→write→flush is held under [`WRITE_LOCK`] so concurrent panics
/// in the same process cannot interleave (`write_all` is not atomic under
/// `O_APPEND`). The lock is never held across foreign code, so it cannot be
/// poisoned in practice — but a hook must never `unwrap`, so a poisoned guard is
/// recovered rather than re-panicked on. On Unix the file is created `0600`
/// (owner-only); the mode is honoured only when this call creates the file, so
/// a caller-managed `HARMONY_PANIC_LOG` target keeps its own permissions.
fn append_record(path: &Path, record: &str) -> std::io::Result<()> {
    use std::io::Write;
    let _guard = WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(record.as_bytes())?;
    file.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_record_contains_all_fields() {
        let record = format_record(
            1_700_000_000_000,
            4242,
            "net-0 (ThreadId(9))",
            "zenoh-1.9.0/src/net/routing/gateway.rs:270:25",
            "called `Result::unwrap()` on an `Err` value: PoisonError { .. }",
            "  0: some::zenoh::frame\n  1: another::frame",
        );
        assert!(record.contains("HARMONY PANIC (ZEB-695)"));
        assert!(record.contains("pid=4242"));
        assert!(record.contains("net-0 (ThreadId(9))"));
        assert!(record.contains("gateway.rs:270:25"));
        assert!(record.contains("PoisonError"));
        assert!(record.contains("some::zenoh::frame"));
        assert!(record.contains("END HARMONY PANIC"));
    }

    #[test]
    fn payload_message_extracts_str_and_string() {
        // `panic!("literal")` produces a `&str` payload.
        let as_str: &(dyn Any + Send) = &"static boom";
        assert_eq!(payload_message(as_str), "static boom");

        // A formatted `panic!("{}", ..)` produces a `String` payload.
        let owned = String::from("formatted boom");
        let as_string: &(dyn Any + Send) = &owned;
        assert_eq!(payload_message(as_string), "formatted boom");

        // An exotic payload degrades gracefully rather than panicking.
        let as_other: &(dyn Any + Send) = &7u32;
        assert_eq!(payload_message(as_other), "<non-string panic payload>");
    }

    #[test]
    fn append_record_creates_and_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("panics.log");

        append_record(&path, "first\n").unwrap();
        append_record(&path, "second\n").unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "first\nsecond\n");
    }

    #[test]
    fn concurrent_appends_never_interleave() {
        // The whole point of the write lock: many threads panicking at once must
        // still each land a *complete*, unspliced multi-line record. Each record
        // is three lines that share one `{thread}-{seq}` tag (padded so a record
        // is large enough that an unserialized `write_all` could split it); if
        // any two records interleaved, the fixed 3-line grouping below would show
        // mismatched tags.
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join("panics.log"));
        let threads = 8;
        let writes_each = 25;
        let pad = "x".repeat(400);

        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let path = Arc::clone(&path);
                let pad = pad.clone();
                std::thread::spawn(move || {
                    for n in 0..writes_each {
                        let record = format!("R{t}-{n}-{pad}\nA{t}-{n}-{pad}\nB{t}-{n}-{pad}\n");
                        append_record(&path, &record).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let contents = std::fs::read_to_string(&*path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(
            lines.len(),
            threads * writes_each * 3,
            "torn or lost writes"
        );
        for chunk in lines.chunks(3) {
            // Drop the leading R/A/B marker; the remaining `{t}-{n}-{pad}` tag
            // must be identical across all three lines of one record.
            assert!(chunk[0].starts_with('R'));
            assert!(chunk[1].starts_with('A'));
            assert!(chunk[2].starts_with('B'));
            assert_eq!(&chunk[0][1..], &chunk[1][1..], "interleaved record");
            assert_eq!(&chunk[1][1..], &chunk[2][1..], "interleaved record");
        }
    }

    #[test]
    fn default_log_path_is_unique_per_process_and_under_temp_dir() {
        // No `HARMONY_PANIC_LOG` set here (nextest process isolation) → the
        // cached default is used. It must live under the temp dir and carry both
        // the pid and a nanosecond suffix so a recycled pid can't collide.
        let path = default_log_path();
        assert!(path.starts_with(std::env::temp_dir()));
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(
            name.starts_with(&format!("harmony-panics-{}-", std::process::id())),
            "unexpected default name: {name}"
        );
        assert!(name.ends_with(".log"));
        // Cached: a second call returns the same path.
        assert_eq!(path, default_log_path());
    }

    #[test]
    fn installed_hook_durably_records_a_caught_panic() {
        // nextest runs each test in its own process, so installing a global hook
        // and setting an env var here does not leak into sibling tests. The
        // chained default hook still prints to (captured) stderr; that noise is
        // expected and hidden unless the test fails.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("panics.log");
        std::env::set_var(ENV_LOG_PATH, &path);

        install_once();
        let caught = std::panic::catch_unwind(|| panic!("zeb695 synthetic boom"));
        assert!(caught.is_err(), "the panic should have been caught");

        let contents =
            std::fs::read_to_string(&path).expect("capture log should exist after the hook fired");
        assert!(
            contents.contains("zeb695 synthetic boom"),
            "record should carry the panic message; got:\n{contents}"
        );
        assert!(
            contents.contains("END HARMONY PANIC"),
            "record should be well-formed; got:\n{contents}"
        );

        // Restore so a shared-process runner (plain `cargo test`) stays clean.
        std::env::remove_var(ENV_LOG_PATH);
    }
}
