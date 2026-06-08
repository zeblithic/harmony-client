# ZEB-379: Desktop GUI tracing subscriber — design

**Status:** approved (brainstorm 2026-06-08)
**Issue:** [ZEB-379](https://linear.app/zeblith/issue/ZEB-379) — *Desktop GUI run() installs no tracing subscriber — RUST_LOG inert, zero field diagnostics* (High)
**Surfaced by:** ZEB-330 Koya↔Ildwyn cross-WAN bring-up (no runtime logs available to diagnose a pkarr publish/resolve failure).

---

## Problem

`harmony_app::run()` (`src-tauri/src/lib.rs:37195`) — the desktop GUI entrypoint — never installs a `tracing` subscriber. The only subscriber init, `init_tracing()` (`src-tauri/src/main.rs:204`), is called solely from the three CLI subcommand arms (rotate-passphrase / export / restore). The GUI is reached via the `None` and `Err` arms of `main()`, which do **not** call it.

Consequence: in the desktop app, `RUST_LOG` is completely inert and **no** `tracing` events are emitted at any level, dev or release, on any platform. Every field issue is undiagnosable from logs, and external alpha testers (ZEB-330 DoD) have no log channel to attach to feedback.

Note: the entire Rust core (`harmony_pkarr`, `iroh_dial_driver`, membership/voice subsystems) is instrumented with **`tracing`**, not the `log` facade. This rules out the official `tauri-plugin-log` (which bridges `log`, not `tracing`, and would silently capture none of our instrumentation). The fix must be a `tracing_subscriber` we own.

## Goal

Install a `tracing` subscriber in the GUI path so that:
1. `RUST_LOG` controls verbosity in the desktop app, defaulting to `info` when unset.
2. Logs are written to **stdout** (visible under `cargo tauri dev`) **and** to a rolling **file** under the app-data dir, so testers can attach logs to feedback.
3. A double subscriber init can never panic.

## Non-goals

- No change to what the core code logs (no new instrumentation, no log-level audits of existing call sites).
- No webview/JS console capture (frontend logging is out of scope).
- No remote/network log shipping. File-on-disk only; testers attach manually.
- No per-layer divergent filters (stdout and file share one `EnvFilter`).

---

## Design

### New module: `src-tauri/src/app_tracing.rs`

Keeps the change out of the 37k-line `lib.rs`. Declared via `mod app_tracing;` in `lib.rs`. Public surface:

```rust
/// Install the global tracing subscriber for the desktop GUI: an EnvFilter
/// (RUST_LOG, default "info") feeding a stdout layer and a daily rolling-file
/// layer under `<app_data_dir>/logs/`. Idempotent — safe to call more than once
/// (a second call is a no-op, never a panic). Degrades to stdout-only if the
/// log directory cannot be created.
pub fn init_app_tracing();

/// Pure resolver: the directory the rolling log files live in.
/// `dirs::data_dir()/net.zeblith.harmony/logs`, byte-identical to Tauri v2's
/// `app_data_dir()/logs` on macOS / Windows / Linux. `None` if no data dir.
fn log_dir() -> Option<std::path::PathBuf>;
```

### Subscriber composition

One `tracing_subscriber::registry()` with three components:

1. **`EnvFilter`** — `EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))`. Same default as the existing CLI `init_tracing()`. Applies to both layers.
2. **stdout `fmt` layer** — human-readable; visible in the dev terminal.
3. **rolling-file `fmt` layer** — `.with_ansi(false)`; non-blocking writer over a `tracing_appender` daily rolling appender.

Wired with `.try_init()`; the returned `Result` is **discarded** (`let _ = …`). If a subscriber is already set (e.g. a CLI arm initialized one earlier in the same process), `try_init` returns `Err` and we no-op — satisfying acceptance #3 without an explicit `OnceLock` guard.

### File layer specifics

- **Location:** `log_dir()` = `dirs::data_dir()/net.zeblith.harmony/logs`. Created with `create_dir_all`. The identifier `net.zeblith.harmony` is the bundle identifier from `tauri.conf.json`; centralized as a `const APP_IDENTIFIER` in the module with a doc comment cross-referencing the config so the coupling is explicit.
- **Rotation:** daily (`tracing_appender::rolling::Rotation::DAILY`), filename prefix `harmony`, suffix `log` → `harmony.YYYY-MM-DD.log`.
- **Retention:** keep the most recent **7** files (`Builder::max_log_files(7)`), so the dir is self-bounding.
- **Scope:** always-on (dev **and** release) — confirmed in brainstorm. No `cfg!(debug_assertions)` gate.
- **Non-blocking writer guard:** `tracing_appender::non_blocking` returns a `WorkerGuard` that must outlive the process or buffered lines are dropped at exit. Parked in a module-level `static LOG_GUARD: OnceLock<WorkerGuard>` (set once on first init). This is the only state the module holds.
- **Failure handling:** if `log_dir()` is `None` or `create_dir_all` fails, log a single `eprintln!` and fall back to **stdout-only** (build the registry without the file layer). Logging must never block app startup.

### Call site

`run()` calls `app_tracing::init_app_tracing()` as its **first statement**, before `tauri::Builder::default()`, so even early plugin-registration spans are captured. Because the log dir is resolved handle-free (via `dirs`, not the Tauri path resolver), this is a single-phase init — no deferred `setup()` step and no reload handle.

### CLI init hardening (defensive)

Change `main.rs::init_tracing()` from `.init()` to `.try_init().ok()`. CLI and GUI are mutually exclusive per process today, so this is insurance, not a live bug fix — it keeps every init path panic-free if call ordering ever changes.

### Dependencies

- **Add** `tracing-appender = "0.2"` (tokio-org; rolling appender + non-blocking writer). `max_log_files` requires ≥ 0.2.3.
- **Add** `dirs = "6"` as a *direct* dependency (already resolved at `6.0.0` in `Cargo.lock` as a transitive dep, so this adds no new compiled crate — only promotes it to a direct dep).

Both deps are well under the MSRV (`rust-version = "1.88"`); the `msrv` CI job confirms.

---

## Testing

Unit tests in `app_tracing.rs` (`#[cfg(test)]`):

1. **`log_dir` shape** — asserts the resolved path ends with `net.zeblith.harmony/logs` (skipped only if `dirs::data_dir()` is `None`, which doesn't occur on supported platforms). Pins the Tauri-co-location coupling.
2. **idempotent init** — calling `init_app_tracing()` twice does not panic. (Because the global subscriber is process-global and other tests may set one, this test only asserts no panic, not which subscriber wins.)

The stdout/file *wiring* is verified by the acceptance smoke-launch, not a unit test (installing a real global subscriber + spawning the appender thread is not unit-testable in isolation).

## Acceptance (from the issue)

1. `RUST_LOG=harmony_app=debug` on a dev launch emits logs to the terminal. → stdout layer + EnvFilter.
2. A release build writes a rolling log file under the app-data dir. → file layer (and, here, dev too).
3. Running a CLI subcommand does not panic on a double subscriber init. → `try_init` on both paths.

Manual smoke (release-process checklist, not CI):
- `RUST_LOG=harmony_app=debug cargo tauri dev` → terminal shows `harmony_app` logs; `~/Library/Application Support/net.zeblith.harmony/logs/harmony.<date>.log` appears and grows.
- A CLI subcommand (e.g. `export`) still runs and logs without panicking.

## File structure

- **Create:** `src-tauri/src/app_tracing.rs` — the module above.
- **Modify:** `src-tauri/src/lib.rs` — `mod app_tracing;` declaration + `init_app_tracing()` as `run()`'s first statement.
- **Modify:** `src-tauri/src/main.rs` — `init_tracing()` `.init()` → `.try_init().ok()`.
- **Modify:** `src-tauri/Cargo.toml` — add `tracing-appender`, promote `dirs` to a direct dep.

## Risks / notes

- **Path-derivation drift:** `log_dir()` reproduces Tauri's `app_data_dir()` derivation rather than calling it (no app handle at init time). Mitigated by centralizing the identifier as a `const` with a doc-comment pointer to `tauri.conf.json`; the identifier is stable and rarely changes. The alternative (two-phase init that defers the file layer to `setup()`) was rejected as materially more complex (reload handle / swappable writer) for a co-location guarantee a `const` already provides.
- **Disk growth:** bounded by daily rotation + `max_log_files(7)`.
- **No secret redaction pass:** existing `tracing` call sites are assumed not to log secrets; this change only routes them. If a later audit finds sensitive fields, that is a separate instrumentation fix, not a subscriber concern.
