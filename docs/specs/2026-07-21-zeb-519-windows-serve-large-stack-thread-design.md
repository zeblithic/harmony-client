# ZEB-519 — CLI entry points: drive `block_on` on an explicit large-stack thread

**Status:** approved (widened to api/watch per owner decision 2026-07-21)
**Ticket:** ZEB-519 — *Windows: repo-root cargo build silently drops `/STACK` linker arg → serve main-thread stack overflow on boot*

## Problem

On Windows/MSVC, building `harmony-app` from the **repo root** produces a binary that crashes immediately on `serve` boot, before any log line:

```
thread 'main' (NNNN) has overflowed its stack
```

**Root cause (diagnosed in-ticket by Ildwyn + AVALON):** Cargo discovers `.cargo/config.toml` from the **cwd**, not the manifest path. The `/STACK:8388608` (8 MiB main-thread) MSVC linker arg lives only in `src-tauri/.cargo/config.toml`. A repo-root-cwd build picks up the *root* `.cargo/config.toml` (which sets only `[env] RUST_MIN_STACK`) and misses the inner one, so the binary links with the MSVC ~1 MiB default main-thread stack.

`RUST_MIN_STACK` does **not** help: `raise_min_stack()` (`main.rs`) and the env net only size **std-spawned** threads. The OS **main thread**'s stack is fixed at link time by `/STACK`. Tokio's `Runtime::block_on` drives the *root* future on the **calling thread** (only `tokio::spawn`ed tasks go to workers), so `serve_cli`'s heavy `start_node_inner` → `add_space`/DM async state machines run on the main thread and overflow the ~1 MiB default.

## Key observation

The **GUI path already solves this**. At `lib.rs:11304` the Tauri entry runs its whole node runtime on a `thread::Builder::new().name("harmony-runtime").stack_size(8 MiB)` thread, whose tokio runtime also sets `.thread_stack_size(8 MiB)`. `serve` (and the other CLI `block_on` entries) simply never inherited that pattern. **The fix is to make the CLI entries use the pattern the GUI already uses.**

## Scope (three CLI `block_on` entries)

| Entry | Runtime | `block_on` workload | Overflow risk |
|---|---|---|---|
| `serve_cli` (`lib.rs`) | `new_multi_thread` | **heavy** — full Zenoh/node bringup (`start_node_inner`, `add_space`, DM state machines) | **observed** (the bug) |
| `api_cli` (`api/cli.rs`) | `new_current_thread` | thin HTTP client (RPC / event stream) vs. the *separate* serve process | none observed |
| `api_watch` (`api/watch.rs`) | `new_current_thread` | thin HTTP streaming client | none observed |

`serve_cli` is the actual fix. Wrapping `api_cli`/`api_watch` is **defensive consistency** (owner decision to widen): it establishes a uniform invariant — *no CLI entry drives `block_on` on the unsized OS main thread* — so a future heavy addition to any CLI path cannot silently reintroduce the trap. It is not claimed to fix an observed crash for those two.

Out of scope: the recovery/export/restore/rotate commands (synchronous, no runtime); the GUI path (already wrapped at `lib.rs:11304`).

## Design

### 1. Shared helper (`lib.rs`, `pub`)

```rust
/// Run `f` on a dedicated 8 MiB-stack thread and return its process exit code.
///
/// Tokio's `Runtime::block_on` drives its root future on the CALLING thread.
/// On Windows/MSVC the OS main-thread stack is fixed at link time by `/STACK`
/// and is NOT sized by `RUST_MIN_STACK` (which only reaches std-spawned
/// threads); a repo-root cargo build that misses `src-tauri/.cargo/config.toml`
/// links the ~1 MiB MSVC default and overflows on serve boot (ZEB-519).
/// Driving CLI entry points on a thread we size ourselves removes the
/// dependency on the link-time `/STACK` value on every platform. Mirrors the
/// `harmony-runtime` GUI path (`lib.rs`).
pub fn run_on_large_stack<F>(name: &str, f: F) -> i32
where
    F: FnOnce() -> i32 + Send + 'static,
{
    match std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(f)
    {
        Ok(h) => h.join().unwrap_or_else(|_| {
            // The default panic hook already printed the payload + backtrace to
            // stderr before unwinding. Exit non-zero so a supervisor never
            // mistakes a serve crash for a clean shutdown (the `eval_server_join`
            // contract in `serve_cli`).
            eprintln!("harmony-app: {name} thread panicked");
            1
        }),
        Err(e) => {
            eprintln!("harmony-app: cannot spawn {name} thread: {e}");
            1
        }
    }
}
```

### 2. Wrap the three call sites (`main.rs`)

```rust
Some(Command::Serve { api_port }) =>
    std::process::exit(harmony_app::run_on_large_stack("serve-main", move || harmony_app::serve_cli(api_port))),
Some(Command::Api { command, args, events }) =>
    std::process::exit(harmony_app::run_on_large_stack("api-main", move || harmony_app::api_cli(command, args, events))),
// watch arm:
std::process::exit(harmony_app::run_on_large_stack("watch-main", move || harmony_app::api_watch(cfg))),
```

Wrapping at the entry (`main.rs`) covers the built binary and every launch path the e2e harness spawns. The entry functions keep their existing signatures — no `_inner` extraction, minimal review surface.

### 3. Worker coverage (`lib.rs`, `serve_cli` only)

Add `.thread_stack_size(8 * 1024 * 1024)` to `serve_cli`'s `new_multi_thread()` builder, matching the GUI runtime (`lib.rs:11324`). This sizes Zenoh/tokio **worker** threads independently of `RUST_MIN_STACK`. Not needed for `api_cli`/`api_watch` (`new_current_thread` has no worker pool).

### 4. Defense-in-depth + docs

- **Keep** `src-tauri/.cargo/config.toml`'s `/STACK` arg unchanged (it also carries the cwd-sensitive `/MANIFESTDEPENDENCY` arg that test binaries still need).
- **CLAUDE.md** note: after this fix, `serve` boot no longer depends on `/STACK`, but cargo should still run from `src-tauri/` because the inner config's MSVC linker args (notably `/MANIFESTDEPENDENCY`) remain cwd-discovered.

## Testing (honest scope)

The "8 MiB actually prevents the overflow" property is a Windows link-time fact **not unit-assertable in-process**: the test environment's ambient `RUST_MIN_STACK=8 MiB` (`.cargo/config.toml`) masks per-thread stack sizing, so a plain `thread::spawn` in a test also gets 8 MiB. Unit tests therefore target the **plumbing the refactor actually risks**, all portable:

1. `run_on_large_stack` returns the closure's exit code verbatim (guards against silently dropping/rewriting the code).
2. A panicking closure yields exit **1**, never 0 (the supervisor-safety contract). Safe under nextest's process-per-test isolation.
3. A closure using a multi-MiB stack frame completes through the helper (documents intent; weakly discriminating given ambient `RUST_MIN_STACK`).

The real boot backstop remains the `serve`-spawn e2e path + fleet/manual Windows validation — the same validation the untested-but-correct GUI `harmony-runtime` thread already relies on.

## Non-goals

- Not a cryptographic/runtime behavior change — pure thread placement.
- Does not remove the `/STACK` / `RUST_MIN_STACK` nets (belt-and-suspenders retained).
- Does not restructure `serve_cli`/`api_cli`/`api_watch` internals beyond the one worker-stack line.
