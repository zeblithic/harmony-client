# harmony-client developer guide

This file documents conventions, recommended tooling, and gotchas for working in `harmony-client`. CI runs the same gates locally — using these tools matches what CI checks.

## Quick reference

| Task | Local command | CI gate |
|---|---|---|
| Run all Rust tests | `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` | `rust-test` job |
| Run Rust lint | `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` | `rust-check` job |
| Run Rust formatter | `cd src-tauri && cargo fmt --all -- --check` (check) / `cargo fmt --all` (fix) | `rust-check` job |
| Run frontend tests | `npx vitest run` (from repo root) | `frontend` job |
| Run frontend type check | `npx tsc --noEmit` (from repo root) | `frontend` job |
| MSRV gate | `cd src-tauri && cargo check --locked --all-targets --features test-fixtures` (with declared MSRV toolchain) | `msrv` job |

Cargo commands run from `src-tauri/`. Frontend commands run from the repo root.

## Required tooling

### Rust side

- **Stable Rust toolchain** — install via [rustup](https://rustup.rs/).
- **`cargo-nextest`** — faster test runner than `cargo test`. Used by CI and recommended locally:

  ```bash
  cargo install cargo-nextest --locked
  # or via your platform's binary release: https://nexte.st/docs/installation/pre-built-binaries/
  ```

- **`cargo-watch`** *(optional, recommended)* — re-runs `cargo check` on file save:

  ```bash
  cargo install cargo-watch
  cd src-tauri && cargo watch -x check
  ```

### Frontend side

- **Node 20+**, npm.
- Run `npm ci` from the repo root after pulling new dependencies.

## Test running guide

### Why `cargo nextest` over `cargo test`

`cargo nextest` is a drop-in replacement that:
- Parallelizes test execution across binaries (cargo test runs test binaries serially by default).
- Common 30-60% wall-clock speedup on workspaces.
- Better failure output (per-test stack traces, no interleaving).

Both CI and local dev should use it. The two cases where `cargo test` is still required:
1. **Doctests:** `cargo test --doc` (nextest doesn't run them). `harmony-client` currently has zero Rust doctests, so this isn't a routine concern — but if you add `///` blocks with executable Rust examples, run `cargo test --doc` separately.
2. **`--no-run` test compile checks** during dev (nextest also supports this via `cargo nextest list`).

### Running tests in a single crate

```bash
cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures
```

The `-p` flag scopes to one workspace member — much faster than the full `--workspace` run during dev.

### Running a single test by name

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(=test_name)'
```

Or via path-prefix match:

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_channel)'
```

### The `test-fixtures` feature

Some integration tests (notably `tests/wire_format_channel_log_fixtures.rs`) need access to deterministic-nonce variants of crypto helpers that are gated behind `#[cfg(any(test, feature = "test-fixtures"))]`. The `test-fixtures` feature exposes those helpers to integration tests (which compile against the public API and can't see `#[cfg(test)]`-only items).

**Always include `--features test-fixtures` when running tests with `--all-targets` or running integration tests.** Without it, those test files won't compile.

## CI architecture

The `.github/workflows/ci.yml` workflow runs four parallel jobs:

| Job | Purpose | Typical wall-clock |
|---|---|---|
| `rust-check` | `cargo fmt --check` + `cargo clippy -D warnings` | 4-5 min |
| `rust-test` | `cargo nextest run` | 4-5 min |
| `msrv` | `cargo check` against declared MSRV | 2-3 min |
| `frontend` | `npx tsc --noEmit` + `npx vitest run` | 2-3 min |

All four run in parallel. **Total wall-clock = max of the four ≈ 5 min** (per ZEB-273 split — was 10 min before).

**Why split rust-check from rust-test:** clippy and test each compile the workspace independently (cargo doesn't share intermediate artifacts across subcommands without `sccache`). Running them in series doubles the rust-side wall-clock; parallelizing across two runners halves it at the cost of ~80% more CI minutes (acceptable trade since engineer wall-clock dominates).

## Code conventions

### Tauri IPC parameter naming

- **Rust IPC functions** declare parameters in `snake_case`: `async fn create_channel(community_id: String, ...)`.
- **JS callers** use `camelCase`: `adapter.invoke('create_channel', { communityId, ... })`.
- Tauri's IPC layer auto-converts at the boundary. Get this wrong and the parameter arrives as `undefined`.

### Tauri IPC error extraction

Production rejections are strings; tests use `Error` objects with `"Error: "` prefix. Always:

```typescript
catch (e) {
  const msg = e instanceof Error ? e.message : String(e);
  // ...
}
```

### Test fixture wire-format pinning

Wire-format pinning tests (`tests/wire_format_*_fixtures.rs`) use deterministic crypto helpers gated behind the `test-fixtures` Cargo feature. **Never remove the `#[cfg(any(test, feature = "test-fixtures"))]` gate** — production code must NEVER call deterministic-nonce variants (would catastrophically reuse nonces).

### `--all-targets` is load-bearing

Always include `--all-targets` in clippy and test commands. Without it, integration test compile errors slip through the lib-only `cargo test` invocation. ZEB-164's SidecarId migration proved this: main stayed "green" for two days while contributors only ran `cargo test --lib` locally; the breakage was in `tests/content_index_integration.rs` and `tests/folder_primitive_integration.rs`.

### `--locked` is load-bearing

Always include `--locked`. Without it, cargo can silently re-resolve patched/yanked deps, causing CI to test a different graph than contributors run locally.

## Productivity tips

### Live re-check during dev

```bash
cd src-tauri && cargo watch -x check -s 'cargo nextest list --features test-fixtures'
```

`cargo watch` re-runs the listed commands on every file save. `cargo check` is faster than `cargo build` (skips codegen); `cargo nextest list` enumerates tests without running them, so you catch test-binary compile errors early.

### Per-feature scoping

When working on a specific area, scope test runs to relevant test files:

```bash
# Just the channel-log tests
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(channel_log)'

# Just the integration tests
cd src-tauri && cargo nextest run --locked --features test-fixtures --test '*_integration'
```

### sccache (optional, larger speedup)

[`sccache`](https://github.com/mozilla/sccache) caches cargo's compile artifacts across `cargo` invocations and across projects that share dep trees. Worth installing if you switch between `harmony-client` and other Rust projects often:

```bash
# macOS
brew install sccache
# All platforms
cargo install sccache

# Then add to your shell rc:
export RUSTC_WRAPPER=sccache
```

First compile after install populates the cache; subsequent compiles of the same dep graph are near-instant. CI does not use sccache (yet — see ZEB-273 Tier 2 follow-up).

## macOS XprotectService — REQUIRED one-time setup

On macOS, `XprotectService` (the system malware/Gatekeeper scanner) synchronously inspects every freshly-linked Mach-O binary on its first execution. For this workspace's 55+ integration test binaries, that means `cargo nextest run --locked --workspace --all-targets --features test-fixtures` from a fresh build can hang indefinitely — each binary blocks in `_dyld_start` for several minutes while XprotectService inspects it. Investigated and fixed in [ZEB-304](https://linear.app/zeblith/issue/ZEB-304).

**Required one-time setup for any macOS contributor:**

```bash
spctl developer-mode enable-terminal
```

Then in **System Settings → Privacy & Security → Developer Tools**, toggle your terminal (Terminal.app, iTerm2, Warp, etc.) **ON** and **quit + relaunch** so the entitlement applies to child processes. Verified speedup: full-workspace `cargo nextest list --all-targets` went from `>20 min, hangs` → `~40 sec`.

If you skip this step, every cold cargo build will appear to hang. There is no workaround other than waiting out the XprotectService queue (which on a fresh checkout can take 30+ minutes).

## References

- Active CI workflow: [`.github/workflows/ci.yml`](.github/workflows/ci.yml)
- `cargo-nextest` documentation: <https://nexte.st/>
- `cargo-watch`: <https://github.com/watchexec/cargo-watch>
- `sccache`: <https://github.com/mozilla/sccache>
