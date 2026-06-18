# ZEB-498 Slice 1: Self-Contained CI Compile-Cache + mold Link-Speed — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cut the cold-compile wall-clock of the three Rust CI jobs (`rust-check`, `rust-test`, `msrv`) so they clear the timeout with margin, using two pure-config, secret-free levers.

**Architecture:** Two independent, additive changes to `.github/workflows/ci.yml`: (1) install the `mold` linker via the pinned `rui314/setup-mold` action with `make-default: true` (symlinks `/usr/bin/ld → mold`, no `RUSTFLAGS` change → `rust-cache` key untouched) to slash the ~118-test-binary relink; (2) set `CARGO_INCREMENTAL=0` workspace-wide so `target/` (and thus each rust-cache key) shrinks enough to fit under GitHub's 10 GB/repo quota and stop evicting. No Rust source or `Cargo.toml` changes.

**Tech Stack:** GitHub Actions, `rui314/setup-mold@v1` (SHA `9c9c13bf4c3f1adef0cc596abc155580bcb04444`, mold 2.41.0), `Swatinem/rust-cache`, cargo.

**Spec:** `docs/specs/2026-06-18-zeb-498-ci-compile-cache-design.md`

**Validation note:** This is a CI-workflow change; there is no local test that proves the speedup — CI itself is the validation. Local checks are limited to YAML/workflow well-formedness. The real success signal is per-job timing on the post-merge runs (Task 2).

**Branch / sequencing:** Work happens on `zeb-498-ci-compile-cache` (already created off `origin/main`). Its PR opens only **after** PR #289 (apt-robustness) merges — one PR per repo at a time. Rebase onto the post-#289 `main` before opening (Task 2).

---

### Task 1: Add mold linker + disable incremental compilation in `ci.yml`

**Files:**
- Modify: `.github/workflows/ci.yml` (top-level `env:` ~lines 15-16; `rust-check` / `rust-test` / `msrv` jobs)

- [ ] **Step 1: Add `CARGO_INCREMENTAL: '0'` to the top-level `env:` block**

Replace:

```yaml
env:
  CARGO_TERM_COLOR: always
```

with:

```yaml
env:
  CARGO_TERM_COLOR: always
  # ZEB-498: incremental compilation buys nothing on a once-through CI build,
  # and its artifacts bloat target/ — exactly what Swatinem/rust-cache tarballs
  # and saves. Disabling it shrinks each per-job cache (check/test/msrv) so the
  # three keys fit under GitHub's 10 GB/repo Actions-cache quota with headroom →
  # far less LRU eviction → PR runs reliably restore a warm dependency cache.
  CARGO_INCREMENTAL: '0'
```

- [ ] **Step 2: Add the mold install step to the `rust-check` job**

Replace:

```yaml
      - name: Install Tauri Linux deps
        uses: ./.github/actions/install-linux-tauri-deps

      - uses: dtolnay/rust-toolchain@3c5f7ea28cd621ae0bf5283f0e981fb97b8a7af9  # master
        with:
          toolchain: stable
          components: rustfmt, clippy
```

with:

```yaml
      - name: Install Tauri Linux deps
        uses: ./.github/actions/install-linux-tauri-deps

      # ZEB-498: mold is a drop-in ELF linker ~2-5x faster than the default
      # bfd/gold, and the gap widens on this workspace's ~118 statically-linked
      # integration test binaries — the relink no compile cache can skip (every
      # lib-touching PR relinks all of them). make-default:true symlinks
      # /usr/bin/ld -> mold so it needs no RUSTFLAGS, which keeps rust-cache's
      # key (and the warm dep cache) intact. Installed via the action's own
      # retrying wget — deliberately NOT apt, to stay off the mirror-flaky
      # install-linux-tauri-deps step (ZEB-498 P0 / PR #289).
      - name: Install mold linker (ZEB-498)
        uses: rui314/setup-mold@9c9c13bf4c3f1adef0cc596abc155580bcb04444  # v1
        with:
          mold-version: '2.41.0'
          make-default: true

      - uses: dtolnay/rust-toolchain@3c5f7ea28cd621ae0bf5283f0e981fb97b8a7af9  # master
        with:
          toolchain: stable
          components: rustfmt, clippy
```

- [ ] **Step 3: Add the mold install step to the `rust-test` job**

Replace:

```yaml
      - name: Install Tauri Linux deps
        uses: ./.github/actions/install-linux-tauri-deps

      - uses: dtolnay/rust-toolchain@3c5f7ea28cd621ae0bf5283f0e981fb97b8a7af9  # master
        with:
          toolchain: stable

      - uses: taiki-e/install-action@ec28e287910af896fd98e04056d31fa68607e7ad  # v2.77.4
```

with:

```yaml
      - name: Install Tauri Linux deps
        uses: ./.github/actions/install-linux-tauri-deps

      # ZEB-498: mold fast linker — see the rust-check job for the full
      # rationale. This is the only job that LINKS all ~118 integration test
      # binaries, so it benefits most. make-default:true → no RUSTFLAGS → the
      # rust-cache key stays intact.
      - name: Install mold linker (ZEB-498)
        uses: rui314/setup-mold@9c9c13bf4c3f1adef0cc596abc155580bcb04444  # v1
        with:
          mold-version: '2.41.0'
          make-default: true

      - uses: dtolnay/rust-toolchain@3c5f7ea28cd621ae0bf5283f0e981fb97b8a7af9  # master
        with:
          toolchain: stable

      - uses: taiki-e/install-action@ec28e287910af896fd98e04056d31fa68607e7ad  # v2.77.4
```

- [ ] **Step 4: Add the mold install step to the `msrv` job**

Replace:

```yaml
      - name: Install Tauri Linux deps
        uses: ./.github/actions/install-linux-tauri-deps

      - name: Read MSRV from Cargo.toml
```

with:

```yaml
      - name: Install Tauri Linux deps
        uses: ./.github/actions/install-linux-tauri-deps

      # ZEB-498: mold fast linker — see the rust-check job for the full
      # rationale. mold is invoked by the cc driver, so the MSRV toolchain
      # version is irrelevant. make-default:true → no RUSTFLAGS → rust-cache
      # key intact.
      - name: Install mold linker (ZEB-498)
        uses: rui314/setup-mold@9c9c13bf4c3f1adef0cc596abc155580bcb04444  # v1
        with:
          mold-version: '2.41.0'
          make-default: true

      - name: Read MSRV from Cargo.toml
```

- [ ] **Step 5: Verify the workflow is still well-formed YAML**

Run:

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml')); print('ci.yml: valid YAML')"
```

Expected: `ci.yml: valid YAML` (non-zero exit + a traceback means a YAML/indent error — fix before committing).

Then confirm exactly three mold steps and one `CARGO_INCREMENTAL` line were added:

```bash
grep -c "setup-mold@9c9c13bf" .github/workflows/ci.yml   # expect 3
grep -c "CARGO_INCREMENTAL" .github/workflows/ci.yml      # expect 1
```

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add .github/workflows/ci.yml
git commit -m "$(cat <<'EOF'
ci(zeb-498): mold linker + CARGO_INCREMENTAL=0 to clear CI compile timeout

Slice 1 of 3. Two self-contained, secret-free levers:
- rui314/setup-mold (make-default) on rust-check/rust-test/msrv: mold is a
  drop-in ELF linker ~2-5x faster than bfd/gold; it attacks the ~118
  static test-binary relink that no compile cache can skip. make-default
  symlinks /usr/bin/ld -> mold, so no RUSTFLAGS change -> rust-cache key
  (and the warm dep cache) stays intact. Installed via the action's own
  retrying wget, off the mirror-flaky apt step.
- CARGO_INCREMENTAL=0 workspace-wide: incremental gives nothing on a
  once-through CI build but bloats target/ (what rust-cache saves);
  disabling it shrinks the three cache keys to fit under GitHub's 10 GB
  quota -> less eviction -> reliably warm dep cache on PR runs.

Spec: docs/specs/2026-06-18-zeb-498-ci-compile-cache-design.md

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Rebase, open PR, and measure the speedup

**Files:** none (process / observation task)

**Precondition:** PR #289 (apt-robustness) is merged to `main`. Do not open this PR before then.

- [ ] **Step 1: Rebase onto the post-#289 `main`**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git fetch origin
git rebase origin/main
```

Expected: clean rebase (this branch only touches `ci.yml`; #289 only touched `.github/actions/install-linux-tauri-deps/action.yml` — no overlap). If a conflict appears, it is unexpected — stop and inspect.

- [ ] **Step 2: Push and open the PR**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git push -u origin zeb-498-ci-compile-cache
gh pr create --repo zeblithic/harmony-client \
  --title "ZEB-498 slice 1: mold linker + CARGO_INCREMENTAL=0 (CI compile-cache)" \
  --body "$(cat <<'EOF'
## What

Slice 1 of 3 of the durable ZEB-498 CI fix (cache/compile → binary consolidation → tiered split). Two self-contained, secret-free levers in `ci.yml`:

1. **`mold` fast linker** via pinned `rui314/setup-mold@v1` (`make-default: true`) on `rust-check` / `rust-test` / `msrv`. Attacks the ~118 statically-linked integration-test-binary relink — the cost no compile cache can skip (every lib-touching PR relinks all of them). `make-default` symlinks `/usr/bin/ld → mold`, so there is **no `RUSTFLAGS` change** and the `rust-cache` key (and warm dep cache) stays intact. Installed via the action's own retrying `wget`, deliberately off the mirror-flaky apt step.
2. **`CARGO_INCREMENTAL=0`** workspace-wide. Incremental compilation buys nothing on a once-through CI build but bloats `target/` — exactly what `rust-cache` tarballs. Disabling it shrinks the three per-job caches so they fit under GitHub's 10 GB/repo quota → less LRU eviction → PR runs reliably restore a warm dep cache.

No Rust source or `Cargo.toml` changes.

## Why

The three Rust jobs each cold-compile the workspace + ~118 test binaries under `--all-targets` and sit at the (stopgap 45-min) timeout edge; runner + apt variance tips them over (ZEB-498). This removes the cold-compile pressure rather than just widening the timeout.

## Validation

CI is the validation — watch the per-job timings on this PR and the first few `main` runs, plus the ZEB-440 `df` headroom line. Target: the three Rust jobs comfortably under ~20 min cold with a warm dep cache. If that holds ~a week, a follow-up walks `timeout-minutes` back 45 → 30 (reverting PR #288).

Spec: `docs/specs/2026-06-18-zeb-498-ci-compile-cache-design.md`
Plan: `docs/plans/2026-06-18-zeb-498-ci-compile-cache.md`

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

(Note: keep `Closes ZEB-498` out of the body — ZEB-498 has further slices; reference it in a PR comment instead so the Linear cascade doesn't close the parent.)

- [ ] **Step 2a: Add a Linear-reference comment (not in the body)**

```bash
gh pr comment <new-pr-number> --repo zeblithic/harmony-client \
  --body "Slice 1 of ZEB-498 (self-contained cache/compile). Follow-ups: binary consolidation + tiered split (ZEB-498), external-infra R2 sccache + cross-machine warming (ZEB-499)."
```

- [ ] **Step 3: Read the speedup off the first run**

After the PR's CI completes, compare the three Rust jobs' durations against the pre-change baseline (the `cec1aea2`/#287 run: nextest ~23 min, MSRV ~12-20 min, rust-check near the 30-45 min edge). Capture mold's effect:

```bash
gh run view --repo zeblithic/harmony-client <run-id> 2>/dev/null | grep -E "Rust|MSRV"
# In the rust-test job log, confirm mold ran and the dep cache was restored:
gh run view --repo zeblithic/harmony-client --job <rust-test-job-id> --log 2>/dev/null | grep -iE "mold|cache restored|Cache hit|nextest run" | head
```

Success = the three Rust jobs land comfortably under ~20 min cold with a restored (warm) dep cache and no eviction churn. Record the numbers in a PR comment for the eventual timeout-walkback follow-up.

- [ ] **Step 4: If a Rust job regresses or fails on mold**

mold incompatibility (extremely unlikely) surfaces as a link error in the job log. Fallback (per spec Risks): set `make-default: false` on the three setup-mold steps and add `RUSTFLAGS: -C link-arg=-fuse-ld=mold` to the top-level `env:` (scopes mold to Rust links only, at the cost of a one-time `rust-cache` rekey). Re-push and re-measure. Do not abandon mold without trying this scoped form first.

---

## Self-Review

- **Spec coverage:** Lever 1 (mold) → Task 1 Steps 2-4. Lever 2 (`CARGO_INCREMENTAL=0`) → Task 1 Step 1. Out-of-scope items carry no tasks (correct). Validation/success criteria → Task 2 Step 3. Rollback → Task 2 Step 4. All spec sections covered.
- **Placeholder scan:** `<new-pr-number>`, `<run-id>`, `<rust-test-job-id>` are runtime values the executor fills from prior command output — not design placeholders. No "TBD"/"handle edge cases" left.
- **Consistency:** the setup-mold SHA `9c9c13bf4c3f1adef0cc596abc155580bcb04444`, tag `v1`, and `mold-version: '2.41.0'` are identical across all three job edits and the spec.
