# ZEB-499: sccache → Cloudflare R2 in harmony-client CI — design

**Status:** approved 2026-06-18 (Jake)
**Ticket:** ZEB-499 — the external-infra durable CI fix; promoted to lead lever after #290 confirmed the bottleneck is *compilation* of the workspace/vendored crates, which `Swatinem/rust-cache` structurally cannot cache.
**Scope:** the **sccache → R2 CI integration only**. Cross-machine (Ildwyn/AVALON) cache warming and the prebuilt Tauri-deps apt container are deliberately deferred to separate follow-ups.

## Problem — why sccache, not more rust-cache

ZEB-498 slice 1 (#290) confirmed the CI compile cost is dominated by recompiling the **workspace crate (`harmony-app`)**, the **vendored fork (`zenoh-link`)**, and the **~118 integration test binaries** — on *every* run, even a no-Rust-change PR. `Swatinem/rust-cache` caches only third-party dependency artifacts (it deliberately cleans local/path crates before saving), so it cannot touch this cost. mold (slice 1) only accelerates *linking*.

**sccache caches per-`rustc`-invocation**, keyed by a hash of (compiler, flags, preprocessed source), so it caches *every* crate — including the workspace and vendored ones. Backed by Cloudflare R2 (uncapped object storage), it also escapes GitHub's 10 GB/repo Actions-cache limit entirely. For a no-Rust-change PR, nearly the whole build becomes cache hits.

## Prior art (harmony repo)

harmony wired sccache→R2 into its **Nix devShell** (`flake.nix`: `RUSTC_WRAPPER=sccache`, `CARGO_INCREMENTAL=0`, "shared Rust compilation cache via Cloudflare R2") with R2 config documented in `docs/sccache-setup.md` (bucket `harmony-sccache`, S3-compatible `endpoint`, `region = auto`, scoped `aws_access_key_id`/`aws_secret_access_key`). That was **devShell-only — never CI**. The R2 backend config is reusable; the GitHub-Actions integration is new here. sccache entries are keyed by **target triple**, so this Linux CI shares the bucket only with other `x86_64-unknown-linux-gnu` builds (a key reason cross-machine warming from Windows/Mac is out of scope — different triples).

## Design

### Credentials & posture

- **One R2 API token**, scoped to the `harmony-sccache` bucket with **object read+write only** (no bucket/account admin). Jake provisions it in the Cloudflare dashboard (R2 → API Tokens).
- Stored as two GitHub Actions **repo secrets**: `SCCACHE_R2_ACCESS_KEY_ID`, `SCCACHE_R2_SECRET_ACCESS_KEY`.
- **All jobs (PR + main) read and write** — simplest, both branches warm the cache; acceptable for the current trusted-contributor model (no external forks). Documented future hardening if external forks ever participate: split read-only-PR / read-write-main tokens (mirrors the existing `save-if: main-only` rust-cache posture).
- Creds are exported at **job scope on the three Rust jobs only** (`rust-check`, `rust-test`, `msrv`) — never the `frontend` job. This is the deliberate, scoped posture change for this ticket; the workflow's `permissions: contents: read` and full SHA-pinning are otherwise preserved.

### sccache configuration (env on the 3 Rust jobs)

```
RUSTC_WRAPPER:           sccache          # exported only when creds present (see degradation)
CARGO_INCREMENTAL:       '0'              # already global from slice 1 — required by sccache
SCCACHE_BUCKET:          harmony-sccache
SCCACHE_ENDPOINT:        https://1ba234d340e59c6bba1e0fe90b7db8db.r2.cloudflarestorage.com
SCCACHE_REGION:          auto
SCCACHE_S3_KEY_PREFIX:   harmony-client   # namespace our entries vs the harmony repo (shared bucket)
AWS_ACCESS_KEY_ID:       <- secrets.SCCACHE_R2_ACCESS_KEY_ID
AWS_SECRET_ACCESS_KEY:   <- secrets.SCCACHE_R2_SECRET_ACCESS_KEY
```

- **Install:** reuse the already-pinned `taiki-e/install-action` with `tool: sccache` (consistent with the existing `cargo-nextest` install — pinned SHA, fast binary, no apt).
- **Stats:** run `sccache --show-stats` after the cargo step (telemetry: hit/miss rate, R2 round-trips) so the cache's effect is visible in the logs.

### rust-cache → registry-only

Flip the three existing `Swatinem/rust-cache` steps to **`cache-targets: false`**, keeping them solely to warm `~/.cargo` (the crates.io index + downloaded sources, which sccache does *not* cache). sccache+R2 takes over **all compilation caching**. Double win:
- **Eliminates the 10 GB target-cache churn** (ZEB-440's root problem) — `target/` is no longer tarballed/saved/restored.
- R2 (uncapped) holds the compiled artifacts instead, with no eviction.

Keep the existing per-job cache keys (`check`/`test`/`msrv`) for the registry cache; `save-if: main-only` can stay as-is (registry is small, but main-only avoids churn).

### Graceful degradation (forks / secret absent)

GitHub does not expose secrets to fork PRs. A **"Configure sccache"** step runs before the toolchain/build steps:
- If `secrets.SCCACHE_R2_ACCESS_KEY_ID` is non-empty → write `RUSTC_WRAPPER=sccache` and all `SCCACHE_*` / `AWS_*` vars to `$GITHUB_ENV`.
- Else → leave `RUSTC_WRAPPER` unset; the build compiles normally (cold, but green). No hard failure.
- R2 transient outage: sccache's default is to treat a cache-backend error as a miss and run the compiler directly (it does not fail the build); add `SCCACHE_ERROR_LOG` for visibility. Verify this fall-through holds for R2 auth/network errors during implementation and pin the exact soft-fail env if one is needed.

### Composition with existing levers

- **mold** (slice 1) is the linker; sccache wraps `rustc` compilation — orthogonal, both stay.
- **`CARGO_INCREMENTAL=0`** already shipped (slice 1) and is a hard prerequisite here.
- **ZEB-440 disk reclaim** stays. sccache streams artifacts to R2; its local dir is bounded — set a modest `SCCACHE_DIR`/`SCCACHE_CACHE_SIZE` only if disk telemetry shows pressure.

## Files touched

- `.github/workflows/ci.yml`
  - Add a `taiki-e/install-action` (`tool: sccache`) step + a "Configure sccache" env step to `rust-check`, `rust-test`, `msrv` (before toolchain/build).
  - Flip the three `Swatinem/rust-cache` steps to `cache-targets: false`.
  - Add a `sccache --show-stats` step after each cargo invocation.
- `docs/ci-sccache.md` (new) — short operator doc: the two secret names, the R2 token scope, how to rotate, and the degradation behavior. Mirrors harmony's `docs/sccache-setup.md`.
- `CLAUDE.md` — update the "CI does not use sccache yet (ZEB-273 Tier-2 follow-up)" note to point at this integration.

## Validation / success criteria

- First main run after merge: sccache populates R2 (mostly misses + uploads — slightly slower than steady-state).
- A follow-up **no-Rust-change PR**: `sccache --show-stats` shows **mostly hits**, and the three Rust jobs drop well under the ~20-min target.
- If sustained ~a week → walk `timeout-minutes` 45 → 30 (the ZEB-498 finisher) in a tiny follow-up.

## Risks & mitigations

- **Secret exposure to PR build code** — PR builds execute build scripts + test code with the R2 creds in job env. Accepted for the trusted-contributor model; the token is bucket-scoped read+write only (bounded blast radius — it can read/write cache objects, nothing else). Future hardening: split read-only-PR / read-write-main tokens.
- **Shared-bucket collision with harmony** — avoided via `SCCACHE_S3_KEY_PREFIX=harmony-client`.
- **R2 outage** — soft-fail to direct compilation (degradation step) keeps CI green (just slow).
- **First-build overhead** — initial main run is misses + uploads; steady-state is the win. Not a regression.
- **sccache + workspace path-deps** — sccache caches path/workspace crates fine (keyed by preprocessed source); this is exactly the gap rust-cache left. The vendored `zenoh-link` compiles like any crate and is cached.
