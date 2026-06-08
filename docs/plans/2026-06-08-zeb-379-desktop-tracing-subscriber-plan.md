# ZEB-379: Desktop GUI tracing subscriber — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Install a `tracing` subscriber in the desktop GUI entrypoint so `RUST_LOG` works in the shipped app and a rolling log file is written for tester diagnostics.

**Architecture:** A new `src-tauri/src/app_tracing.rs` module composes one `tracing_subscriber::registry()` — a `RUST_LOG`/`info` `EnvFilter`, a stdout `fmt` layer, and a daily rolling-file `fmt` layer under `dirs::data_dir()/net.zeblith.harmony/logs` (handle-free, byte-identical to Tauri's `app_data_dir()/logs`). `run()` calls it first thing; `try_init()` makes it panic-safe against a double init.

**Tech Stack:** Rust, `tracing` / `tracing-subscriber` (registry + EnvFilter + fmt), `tracing-appender` (daily rolling + non-blocking), `dirs`.

**Spec:** `docs/specs/2026-06-08-zeb-379-desktop-tracing-subscriber-design.md`

**Per-task gates (harmony-app relink discipline):** lib changes relink ~97 integration binaries under `--all-targets`, so per task run only:
- `cd src-tauri && cargo fmt --all -- --check`
- `cd src-tauri && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`
- `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures` (optionally scoped `-E 'test(app_tracing)'`)

The full `--all-targets` clippy/nextest + MSRV matrix is **CI's job** (the local `--all-targets` clippy is ~27 min — do **not** run it per task). Commit before each gate; 10-min wall-clock kill switch on any cargo step.

---

## File Structure

- **Create:** `src-tauri/src/app_tracing.rs` — the subscriber installer + log-dir resolver + unit tests. Single responsibility: stand up GUI logging.
- **Modify:** `src-tauri/src/lib.rs` — add `mod app_tracing;` and call `app_tracing::init_app_tracing()` as the first statement of `run()`.
- **Modify:** `src-tauri/src/main.rs` — harden CLI `init_tracing()` from `.init()` to `.try_init()` (panic-safe).
- **Modify:** `src-tauri/Cargo.toml` — add `tracing-appender`, promote `dirs` to a direct dep.

---

### Task 1: Add dependencies

**Files:**
- Modify: `src-tauri/Cargo.toml`

- [ ] **Step 1: Add the two deps**

In `src-tauri/Cargo.toml`, in the `[dependencies]` table, add these lines (place `tracing-appender` next to the existing `tracing-subscriber` line ~106; `dirs` anywhere alphabetically sensible in the table):

```toml
tracing-appender = "0.2"
dirs = "6"
```

Context — the existing tracing deps already present (do not duplicate):
```toml
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```
`dirs` is already resolved at `6.0.0` transitively in `Cargo.lock`; this only promotes it to a direct dep (no new compiled crate). `tracing-appender` is genuinely new (tokio-org; `max_log_files` needs ≥ 0.2.3, satisfied by `"0.2"`).

- [ ] **Step 2: Verify it resolves and compiles**

Run: `cd src-tauri && cargo check --locked -p harmony-app --features test-fixtures`
Expected: compiles clean (first run pulls `tracing-appender` + `time`; a few minutes). If it exceeds the 10-min budget, that's the cold dep build — let it finish once.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "build(zeb-379): add tracing-appender + promote dirs to direct dep"
```

---

### Task 2: `app_tracing` module — subscriber installer + tests

**Files:**
- Create: `src-tauri/src/app_tracing.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod app_tracing;`)
- Test: inline `#[cfg(test)] mod tests` in `app_tracing.rs`

- [ ] **Step 1: Declare the module in `lib.rs`**

Find the existing `mod` declarations: `grep -n "^mod \|^pub mod " src-tauri/src/lib.rs | head`. Add this line alongside them (alphabetical placement is fine, e.g. just before `mod content_index;`):

```rust
mod app_tracing;
```

- [ ] **Step 2: Create `src-tauri/src/app_tracing.rs` with the full module + tests**

```rust
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
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

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
        assert!(dir.ends_with("logs"), "log dir must end with /logs: {dir:?}");
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
```

- [ ] **Step 3: Run the module tests — expect PASS**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(app_tracing)'`
Expected: `2 tests run: 2 passed`. (If `log_dir_is_under_identifier_and_logs` fails, the `dirs::data_dir()/<id>/logs` path logic is wrong — fix `log_dir`.)

- [ ] **Step 4: Lint + format**

Run: `cd src-tauri && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`
Expected: no warnings. (Watch for: needless `Vec` type annotation — keep the `Box<dyn Layer<_> ...>` annotation, it's load-bearing for inference.)
Run: `cd src-tauri && cargo fmt --all`

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/app_tracing.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-379): app_tracing module — stdout + rolling-file subscriber"
```

---

### Task 3: Wire into `run()` + harden CLI init

**Files:**
- Modify: `src-tauri/src/lib.rs:37195` (`run()`)
- Modify: `src-tauri/src/main.rs:204` (`init_tracing()`)

- [ ] **Step 1: Call `init_app_tracing()` first thing in `run()`**

In `src-tauri/src/lib.rs`, find `pub fn run() {` (currently line 37195). Insert the call as the first statement, before `tauri::Builder::default()`:

```rust
pub fn run() {
    // ZEB-379: install the tracing subscriber before anything else so RUST_LOG
    // works in the desktop app and early-boot spans land in the log file.
    app_tracing::init_app_tracing();

    tauri::Builder::default()
        // ZEB-356: single-instance MUST be registered first. ...
```

(Leave the existing `tauri::Builder` chain unchanged.)

- [ ] **Step 2: Harden the CLI `init_tracing()` against double init**

In `src-tauri/src/main.rs`, replace the body of `init_tracing()` (lines 204–211):

```rust
fn init_tracing() {
    // try_init (not init) so a second subscriber install never panics; the GUI
    // path (lib.rs run()) installs its own via app_tracing. ZEB-379.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
}
```

- [ ] **Step 3: Build, lint, format**

Run: `cd src-tauri && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`
Expected: clean. (`--lib` compiles the lib incl. `run()`; the `main.rs` change is in the `harmony-app` bin — verify it too:)
Run: `cd src-tauri && cargo check --locked --bin harmony-app --features test-fixtures`
Expected: compiles clean.
Run: `cd src-tauri && cargo fmt --all`

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/main.rs
git commit -m "feat(zeb-379): install tracing subscriber in run(); CLI init_tracing try_init"
```

---

### Task 4: Final gate sweep + PR + bot loop

**Files:** none (verification + ship)

- [ ] **Step 1: Local gate sweep (scoped — CI owns --all-targets/MSRV)**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```
Expected: fmt clean; clippy 0 warnings; nextest all pass (incl. the 2 `app_tracing` tests). Use `set -o pipefail` / `$pipestatus` if piping — never trust a piped exit code.

Do **not** run the local `--all-targets` clippy (≈27 min). The CI matrix (`rust-check` fmt+clippy `--all-targets`, `rust-test` nextest, `msrv` 1.88, `frontend`) covers it. This PR has no frontend changes.

- [ ] **Step 2: Manual smoke (release-process note — not a CI gate)**

Document for the next interactive session (cannot run headless):
1. `RUST_LOG=harmony_app=debug cargo tauri dev` → the terminal shows `harmony_app` log lines, and `~/Library/Application Support/net.zeblith.harmony/logs/harmony.<date>.log` is created and grows.
2. A CLI subcommand (e.g. the export path) still runs and logs without panicking.

If `docs/` contains a release-process / smoke checklist doc (`ls docs/ | grep -i 'release\|smoke\|checklist'`), append a one-line "verify a log file appears under app-data/logs" item; otherwise skip (do not create a new doc).

- [ ] **Step 3: Push + open PR**

```bash
git push -u origin zeb-379-desktop-tracing-subscriber
```
Open a PR titled `ZEB-379: install tracing subscriber in desktop GUI (RUST_LOG + rolling log file)`. Body references the spec + plan, the three acceptance criteria, and notes it's backend-only (no `App.svelte`/`Layout.svelte` — clear of PR #204). `Closes ZEB-379.`

- [ ] **Step 4: Autonomous bot-review loop**

Watch CodeRabbit / Cursor / CodeAnt / Qodo (never Greptile). Scan all three comment buckets. Address genuinely-new ≥Medium findings (one push per round, commit-before-gate, scoped `--lib` gates). Pushover at ready-to-merge. Do **not** self-merge (Jake's gate).

---

## Self-Review

- **Spec coverage:** stdout layer + EnvFilter default-info → Task 2 (acceptance #1). Rolling file under app-data → Task 2 (acceptance #2). `try_init` double-init safety → Task 2 + Task 3 (acceptance #3). Handle-free `log_dir` co-located with Tauri → Task 2. Always-on file (dev+release) → Task 2 (no `cfg` gate). Deps (`tracing-appender`, `dirs`) → Task 1. CLI hardening → Task 3. New module keeps `lib.rs` from growing → Task 2.
- **Placeholder scan:** none — all code blocks are concrete and compile-ready.
- **Type consistency:** `init_app_tracing()` (pub) → `install_subscriber(Option<PathBuf>)` → `log_dir() -> Option<PathBuf>` consistent across Task 2 (impl), Task 2 tests, and Task 3 (call site `app_tracing::init_app_tracing()`). `APP_IDENTIFIER` used in `log_dir` + test. `LOG_GUARD: OnceLock<WorkerGuard>` set once. The `Vec<Box<dyn Layer<_> + Send + Sync + 'static>>` + `.boxed()` is the canonical tracing_subscriber conditional-layers pattern (inference fixes `_` = `Layered<EnvFilter, Registry>`).
