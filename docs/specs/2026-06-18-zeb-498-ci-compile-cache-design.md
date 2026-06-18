# ZEB-498 (slice 1): self-contained CI compile-cache + link-speed design

**Status:** approved 2026-06-18 (Jake)
**Ticket:** ZEB-498 — slice 1 of 3 (cache/compile → binary consolidation → tiered fast/slow split)
**Scope constraint:** self-contained — no new CI secrets, no external infra. Keeps `ci.yml`'s current secret-free, least-privilege (`contents: read`), SHA-pinned posture intact.

## Problem

Three of the four CI jobs — `rust-check` (fmt + clippy), `rust-test` (nextest), `msrv` (cargo check) — each *independently* compile the workspace + ~118 integration test binaries under `--all-targets`. A cold run has two distinct cost centers:

1. **Dependency-graph compile** (zenoh, iroh, tokio, wry, …) — ~10-15 min, *cacheable*.
2. **Compiling + linking ~118 statically-linked integration test binaries** (~90-108 MB each) — every lib-touching PR relinks all of them, and caching *cannot* help because their dependency (the `harmony-app` lib) changed that PR.

`Swatinem/rust-cache` is configured with three per-job keys (`check` / `test` / `msrv`), but they compete for GitHub's **10 GB/repo** Actions-cache quota → LRU eviction → PR runs frequently restore cold and pay the full dependency compile again. The jobs sit right at the timeout edge (bumped 30 → 45 min as the ZEB-498 stopgap, PR #288); runner-speed and apt-mirror variance tip them over intermittently — the original ZEB-498 symptom (`nextest` reported `cancelled` at 30m21s).

## Approach (this slice)

Two levers, each attacking one cost center, both pure-config and secret-free.

### Lever 1 — `mold` fast linker (attacks the relink)

Linking ~118 test binaries is the part caching fundamentally can't fix. `mold` is a drop-in ELF linker, typically 2-5× faster than the default `bfd`/`gold`, and the gap widens on many-binary workloads like ours.

- **Install via the pinned `rui314/setup-mold` action** (SHA-pinned per the workflow's pinning policy). Deliberately **not** via `apt` — this keeps mold off the `install-linux-tauri-deps` apt step that ZEB-498 P0 (PR #289) just hardened against mirror hangs; adding a package there would re-expand the exact fragility we just bounded.
- **Point the linux target's linker at mold for all three Rust jobs** via a shared top-level `RUSTFLAGS: -C link-arg=-fuse-ld=mold` in the workflow `env:`, so `check` / `test` / `msrv` all benefit and key their caches identically.

### Lever 2 — `CARGO_INCREMENTAL=0` (the cache tuning)

Incremental compilation yields nothing on a once-through CI build, but its artifacts bloat `target/` — exactly what `rust-cache` tarballs and saves. Disabling it:

- **Shrinks each of the three per-job caches** → they fit under the 10 GB quota with headroom → far less LRU eviction → PR runs reliably restore a warm dependency cache. This is the self-contained "cache tuning."
- Removes incremental's own per-build overhead on a build that never reuses it.

Set once in the workflow top-level `env:` so all jobs share it.

## Explicitly out of scope (deliberate)

- **`debug = 0`** — keep the `dev` profile at `line-tables-only` (ZEB-304) so CI panic backtraces stay actionable. mold does the speed work; we don't trade away CI debuggability for marginal link savings.
- **External infra** (R2-backed sccache, cross-machine cache warming, prebuilt Tauri-deps container image) → **ZEB-499**. Escaping the 10 GB cap entirely needs CI secrets — a separate posture decision Jake has deferred.
- **Binary consolidation** (~118 → a handful) → ZEB-498 slice 2.
- **Tiered fast/slow test split** → ZEB-498 slice 3.
- **Cache-pruning cron / shared-key merging** — only if post-change measurement shows the three caches still don't fit (YAGNI).

## Files touched

- `.github/workflows/ci.yml`
  - Add `RUSTFLAGS: -C link-arg=-fuse-ld=mold` and `CARGO_INCREMENTAL: '0'` to the existing top-level `env:` block (currently just `CARGO_TERM_COLOR`).
  - Add a pinned `rui314/setup-mold` step to `rust-check`, `rust-test`, and `msrv`, placed **before** the toolchain/cache steps so the linker is on `PATH` at link time. (The `frontend` job is untouched — Node, no linking.)
- No Rust source or `Cargo.toml` profile changes.

## Validation & success criteria

- Land it; read the **actual per-job timings** on the first few `main` + PR runs, plus the ZEB-440 `df` headroom line in `rust-test`.
- **Success** = the three Rust jobs land comfortably under ~20 min cold, with the dependency cache reliably warm (cache-hit reported on PR runs, no eviction churn).
- If that holds for ~a week, a follow-up walks `timeout-minutes` back from 45 → 30 (reverting the ZEB-498 stopgap from PR #288).

## Risks & mitigations

- **`-fuse-ld=mold` compatibility** — `gcc` on `ubuntu-latest` (24.04, gcc 12+) supports `-fuse-ld=mold`; mold links the same ELF the default linker does. Low risk. If the cc driver ever rejects the flag, fall back to mold's `--ld-path` form via a `[target.x86_64-unknown-linux-gnu]` cargo-config entry. The `msrv` job's older rustc is unaffected — the flag is a link-arg passed to the cc driver, toolchain-version-independent.
- **One-time cache invalidation** — changing `RUSTFLAGS` rekeys `rust-cache`; the first run after merge is cold by design, then warm. Expected, not a regression.
- **`setup-mold` availability** — pinned to a release SHA. Treated as a required step (same as the toolchain action): if it were unavailable, CI fails fast and loudly rather than silently linking slow. Acceptable — matches how the repo already treats its other pinned actions.
