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

- **Install via the pinned `rui314/setup-mold` action** (`@9c9c13bf…`, tag `v1`, SHA-pinned per the workflow's pinning policy; `mold-version: '2.41.0'` pinned explicitly). Deliberately **not** via `apt` — this keeps mold off the `install-linux-tauri-deps` apt step that ZEB-498 P0 (PR #289) just hardened against mirror hangs; adding a package there would re-expand the exact fragility we just bounded. The action downloads the mold release tarball over its own retrying `wget`, independent of the Ubuntu apt mirrors.
- **Use the action's `make-default: true`** (its default), which symlinks `/usr/bin/ld → mold` so *every* link step (build scripts, final test binaries) uses mold automatically. This is preferred over a `RUSTFLAGS: -C link-arg=-fuse-ld=mold` opt-in because it does **not** change `RUSTFLAGS` — so it leaves `rust-cache`'s key untouched and the existing warm dependency cache stays valid (no one-time cold rebuild). Dependency `.rlib`s are linker-independent archives, so reusing the cache built under the previous linker is correct; only the final binary *link* changes, and that runs fresh every build regardless.

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
  - Add `CARGO_INCREMENTAL: '0'` to the existing top-level `env:` block (currently just `CARGO_TERM_COLOR`). (No `RUSTFLAGS` change — see Lever 1.)
  - Add a pinned `rui314/setup-mold` step (with `make-default: true`) to `rust-check`, `rust-test`, and `msrv`, placed after `Install Tauri Linux deps` and before the toolchain/build steps so mold is the default linker at link time. (The `frontend` job is untouched — Node, no linking.)
- No Rust source or `Cargo.toml` profile changes.

## Validation & success criteria

- Land it; read the **actual per-job timings** on the first few `main` + PR runs, plus the ZEB-440 `df` headroom line in `rust-test`.
- **Success** = the three Rust jobs land comfortably under ~20 min cold, with the dependency cache reliably warm (cache-hit reported on PR runs, no eviction churn).
- If that holds for ~a week, a follow-up walks `timeout-minutes` back from 45 → 30 (reverting the ZEB-498 stopgap from PR #288).

## Risks & mitigations

- **Global `ld` symlink** — `make-default: true` repoints `/usr/bin/ld` to mold for the whole runner, so every link (build scripts, C-dep linking, final binaries) uses mold. mold is a drop-in for GNU ld on standard ELF, and the C/asm deps here (ring, aws-lc-sys, webkit bindings) compile to objects that rustc links normally — no GNU-ld-specific behavior is relied on. Low risk on an ephemeral CI runner; CI surfaces any incompatibility immediately. If it ever bites, fall back to `make-default: false` + `RUSTFLAGS: -C link-arg=-fuse-ld=mold` (scopes mold to Rust links only, at the cost of a one-time cache rekey). The `msrv` job's older rustc is unaffected — mold is invoked by the cc driver, toolchain-version-independent.
- **Cache validity preserved** — because `RUSTFLAGS` is unchanged, `rust-cache`'s key is unchanged; the existing warm dependency cache stays valid. `CARGO_INCREMENTAL=0` changes only what `target/` holds (no incremental dir), so the first `main` save after merge stores the smaller tree and PR runs restore it — no cold-rebuild penalty.
- **`setup-mold` availability** — pinned to a release SHA. Treated as a required step (same as the toolchain action): if it were unavailable, CI fails fast and loudly rather than silently linking slow. Acceptable — matches how the repo already treats its other pinned actions.
