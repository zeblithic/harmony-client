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

> **Why `src-tauri/` specifically (Windows/MSVC):** Cargo discovers `.cargo/config.toml` from the **cwd**, not the manifest path. `src-tauri/.cargo/config.toml` carries MSVC-only linker args a repo-root-cwd build silently misses. After ZEB-519, `serve`/`api`/`watch` boot no longer depends on that config's `/STACK` arg — those entry points now drive their `block_on` on an explicit 8 MiB-stack thread (`run_on_large_stack`) regardless of how the binary was linked. But the inner config's `/MANIFESTDEPENDENCY` arg (comctl32 v6 for GUI-linking **test** binaries) is still cwd-discovered, so a repo-root `cargo test` can still fail to link test binaries on some Windows hosts. Running cargo from `src-tauri/` keeps every MSVC linker arg in effect.

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

### The fast inner loop: `--lib` (one binary)

When iterating on a single library module, add `--lib` to build and run **only
the crate's own unit tests** — the inline `#[cfg(test)] mod tests` blocks
compiled into the lib target:

```bash
cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(<module>)'
```

Why it is faster: `--lib` links **exactly one** test binary. The default
`--all-targets` set also links every `tests/*.rs` integration binary (~25 of
them), and touching `src/lib.rs` forces **all of them to re-link** against the
rebuilt ~88k-line crate. Measured on this workspace (warm tree, hot sccache):

| Relink after touching `src/lib.rs` (build + link only) | Time |
|---|---|
| `--lib` (1 binary) | ~15 s |
| `--all-targets` (~25 binaries) | ~31 s |

On a warm tree the gap is modest — the extra 24 re-links cost ~16 s total.
It blows out when the tree is **cold**, or when a public-API change forces
every integration binary to *recompile* (not just re-link) from scratch: the
`--all-targets` set then runs into the 10-min range while `--lib` stays in the
tens of seconds. That cold case is the tax the inner loop is built to avoid.

**Backstop discipline (load-bearing — see [`--all-targets` is
load-bearing](#--all-targets-is-load-bearing)):** `--lib` compiles **none** of
the integration tests, so it *cannot* catch a break in `tests/*.rs` — exactly
the ZEB-164 failure mode where main stayed "green" for two days under lib-only
local runs. `--lib` is the innermost loop **only**. The mandatory ladder:

| Loop | Command | Catches |
|---|---|---|
| Innermost — one module | `… --lib -E 'test(<module>)'` | lib unit tests only |
| Per-task gate | `scripts/test-select --context task` | changed-file tests + a rotating `--all-targets` partition |
| Pre-PR / CI backstop | `cargo nextest run --locked --workspace --all-targets --features test-fixtures` | everything |

Never open a PR on the strength of a `--lib` run alone.

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

### OS keychain isolation in tests (ZEB-428)

The OS keychain is a **process-global resource** — a test that redirects `HOME`/`USERPROFILE` to a tempdir still reaches the developer's *real* credential store through `KeychainStore::new()`'s fixed service/account names. A full-suite run once silently overwrote a developer's real owner identity this way (unrecoverable; see ZEB-428).

Guard rails now in place:

1. **`KeychainStore::new()` refuses in test builds** (`cfg(test)` or the `test-fixtures` feature — and every integration-test compilation requires the latter). Gated runs behave exactly like Linux CI, where no keychain backend exists: callers fall back to the `HARMONY_PASSPHRASE` encrypted-file store.
2. **`HARMONY_DISABLE_KEYCHAIN=1`** disables the keychain in *any* build (operator kill-switch; beats all overrides).
3. **`HARMONY_ALLOW_REAL_KEYCHAIN=1`** re-enables the real keychain in a test build — set it only for a test that deliberately exercises the real credential store, and never in a suite that runs `mint`/`save_owner_state_atomic` paths.

Rules when writing tests that touch identity persistence:
- Inject `keychain: None` (or a `#[cfg(test)]` mock) through the `*_inner`/`*_with_keychain` seams — never construct `KeychainStore::new()` inside code reachable from tests.
- Set `HARMONY_PASSPHRASE` in the test so the encrypted-file fallback has a passphrase (see `tests/mint_owner_lifecycle.rs::home_override`).
- `tests/keychain_isolation.rs` pins the constructor gate; don't weaken it.

## CI architecture

The `.github/workflows/ci.yml` workflow runs these jobs in parallel (timings measured 2026-07, ZEB-676 recalibration):

| Job | Purpose | Typical wall-clock |
|---|---|---|
| `rust-check` | `cargo fmt --check` + `cargo clippy -D warnings` | ~4 min |
| `rust-test` ×3 shards | `cargo nextest run --partition hash:k/3` | ~10-13 min per shard |
| `rust-test-gate` ("Rust — test (nextest)") | roll-up: green iff all 3 shards pass | seconds |
| `msrv` | `cargo check` against declared MSRV | ~3.5 min |
| `frontend` | `npx tsc --noEmit` + `npx vitest run` | ~3.5 min |

**Total wall-clock = max of the jobs ≈ 12 min** on a warm cache. A cold sccache miss (toolchain bump) can push a Rust job toward its 35-min timeout — that is slow, not hung.

**Why rust-test is sharded (ZEB-676):** by 2026-07 the single rust-test job ran ~21-22 min while every other job took ~4 — and the cost was **test execution, not compilation** (measured on a zero-Rust-diff PR: compile+link 3m47s at a 100% sccache/R2 hit rate vs ~16 min executing 4,438 tests on the 4-vCPU runner). Each shard still compiles the full workspace (`--all-targets` compile coverage stays complete everywhere, cheap off R2) and runs a stable hash-partition third of the tests. When a shard trends past ~15 min, bump the shard count in ci.yml (matrix + the `/3` denominators together).

**Why split rust-check from rust-test (ZEB-273):** clippy and test each compile the workspace independently. Running them in series doubles the rust-side wall-clock; parallelizing across runners trades CI minutes for engineer wall-clock (standard runners are free on public repos, so the trade costs nothing).

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

Always include `--all-targets` in clippy and full-gate test commands (the
`--lib` inner loop above and the ZEB-631 iterative selection below are the
documented exceptions — both are inner-loop-only and backstopped by full runs).
Without it, integration test compile errors slip through the lib-only `cargo test` invocation. ZEB-164's SidecarId migration proved this: main stayed "green" for two days while contributors only ran `cargo test --lib` locally; the breakage was in `tests/content_index_integration.rs` and `tests/folder_primitive_integration.rs`.

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

### Iterative test selection (ZEB-631)

`scripts/test-select` amortizes regression detection across iterative runs
instead of paying the full ~4,100-test suite on every local gate: it runs an
**always-run set** (tests mapped from your branch's changed files) plus a
**rotating partition** (nextest's stable `--partition hash:<bucket>/<k>`, with
the bucket advanced by a per-machine round counter in git-ignored
`.testselect/`). Every test is guaranteed to run at least once every k rounds —
a hard bound, not a probability.

```bash
scripts/test-select --context task    # k=4 — per-task gates during iterative dev
scripts/test-select --context round   # k=2 — PR converge-round re-runs
scripts/test-select --dry-run         # print selection + commands; no cargo, no counter bump
scripts/test-select --full            # bypass selection: CI-parity full sweep (--workspace --all-targets)
```

**Use it for:** iterative development gates — per-task test cycles, review-fix
re-runs, PR converge rounds. Paste the printed `round=… bucket=…` summary line
into task reports so the selection is auditable.

**Do NOT use it for:** the final pre-PR sweep, anything CI-shaped, or release
validation — those remain the full `--workspace --all-targets` commands (CI is
untouched by design; full runs are the scheme's backstop).

**Caveats:** dependency-graph changes (`Cargo.toml`/`Cargo.lock`/`.cargo/`/
`src-tauri/vendor/`) make module mapping unreliable — the script exits and
tells you to use `--full` (or `--force` to proceed anyway). `git add` new test
files before gating: untracked files are invisible to the always-run set.

### Reclaiming build-tree disk (ZEB-765)

`src-tauri/target/` grows without bound and nothing prunes it automatically. On
AVALON it reached **94.7 GB / 99,793 files** before anyone looked, from a tree
whose newest artifact was three weeks stale. Two mechanisms compound:

- **`incremental/`** accumulates per-session directories (695 of them, 48 GB)
  that Cargo's lazy GC never keeps up with — and that are worthless the moment
  the source moves on.
- **`deps/`** is effectively append-only: artifacts are keyed by
  (package, version, feature-set, compiler) and superseded entries are never
  evicted. After an `iroh 0.98.2 → 1.0.1` bump, *both* versions' artifacts sit
  on disk permanently. Every dependency bump ratchets the floor upward.

So the steady state is not "large", it is "monotonically increasing".

```bash
scripts/build-gc                  # report size + staleness; deletes NOTHING  [default]
scripts/build-gc --incremental    # drop incremental/ only; keeps deps/ rebuild speed
scripts/build-gc --all            # full cargo clean; next build is COLD
scripts/build-gc --all --yes      # skip the confirmation prompt
```

The report prints per-profile sizes, the `incremental/` session-dir count, and
**when each profile last produced a binary** — that staleness line, not the size,
is what tells you whether you are looking at a live cache or a corpse. Pruning
always requires an explicit tier flag, and refuses to run non-interactively
without `--yes` (exit 3).

**Which tier:** `--incremental` during active work on a branch (deps/ stay warm).
`--all` when the tree has gone stale across a dependency bump — once `Cargo.lock`
has moved, most of `deps/` is artifacts for versions the lockfile no longer
references, and the next build is near-cold whether you keep them or not.

`CARGO_TARGET_DIR` is honoured, so a machine that redirects its target tree is
still collectable.

> **Note:** this is *not* ZEB-440. That was **CI runner** disk exhaustion, fixed
> with `CARGO_INCREMENTAL=0` + debuginfo trims in `ci.yml`. Those mitigations are
> exactly why CI does not accumulate this and workstations do — they never
> applied locally.

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

First compile after install populates the cache; subsequent compiles of the same dep graph are near-instant. CI uses sccache backed by Cloudflare R2 (ZEB-499) — see [`docs/ci-sccache.md`](docs/ci-sccache.md).

## macOS XprotectService — REQUIRED one-time setup

On macOS, `XprotectService` (the system malware/Gatekeeper scanner) synchronously inspects every freshly-linked Mach-O binary on its first execution. For this workspace's 55+ integration test binaries, that means `cargo nextest run --locked --workspace --all-targets --features test-fixtures` from a fresh build can hang indefinitely — each binary blocks in `_dyld_start` for several minutes while XprotectService inspects it. Investigated and fixed in [ZEB-304](https://linear.app/zeblith/issue/ZEB-304).

**Required one-time setup for any macOS contributor:**

```bash
spctl developer-mode enable-terminal
```

Then in **System Settings → Privacy & Security → Developer Tools**, toggle your terminal (Terminal.app, iTerm2, Warp, etc.) **ON** and **quit + relaunch** so the entitlement applies to child processes. Verified speedup: full-workspace `cargo nextest list --locked --all-targets --features test-fixtures` went from `>20 min, hangs` → `~40 sec`.

If you skip this step, every cold cargo build will appear to hang. There is no workaround other than waiting out the XprotectService queue (which on a fresh checkout can take 30+ minutes).

## References

- Architecture map (how the pieces fit together): [`docs/architecture/README.md`](docs/architecture/README.md)
- Active CI workflow: [`.github/workflows/ci.yml`](.github/workflows/ci.yml)
- `cargo-nextest` documentation: <https://nexte.st/>
- `cargo-watch`: <https://github.com/watchexec/cargo-watch>
- `sccache`: <https://github.com/mozilla/sccache>
