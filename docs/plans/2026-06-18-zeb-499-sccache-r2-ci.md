# ZEB-499: sccache → R2 CI Integration — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps are sequential; track execution status in Linear (ZEB-499), not in this file.

**Goal:** Make harmony-client CI cache `rustc` compilation (including the workspace + vendored crates that `Swatinem/rust-cache` can't) on Cloudflare R2 via sccache, so PR builds reuse compiled artifacts instead of recompiling from scratch.

**Architecture:** On the three Rust jobs (`rust-check`, `rust-test`, `msrv`), install sccache (pinned `taiki-e/install-action`), set the R2 backend env, and gate `RUSTC_WRAPPER=sccache` on the R2 secret being present (graceful no-secret degradation for forks). Flip `Swatinem/rust-cache` to registry-only (`cache-targets: false`) so R2 owns compilation caching and the 10 GB target-cache churn disappears. `CARGO_INCREMENTAL=0` (required by sccache) and mold already shipped in slice 1.

**Tech Stack:** GitHub Actions, sccache (S3/R2 backend), Cloudflare R2 (`harmony-sccache` bucket), `taiki-e/install-action`, `Swatinem/rust-cache`.

**Spec:** `docs/specs/2026-06-18-zeb-499-sccache-r2-ci-design.md`

**Secrets (confirmed present in repo):** `SCCACHE_R2_ACCESS_KEY_ID`, `SCCACHE_R2_SECRET_ACCESS_KEY`.

**Validation note:** CI is the validation — there is no local test. Local gate = workflow well-formedness (`actionlint` + YAML parse). The real signal is `sccache --show-stats` hit rate on a follow-up no-Rust-change PR (Task 4).

**Branch:** `zeb-499-sccache-r2-ci` (already created off `origin/main` @ `9a7f8221`). One PR.

---

### Task 1: Top-level static sccache config

**Files:** Modify `.github/workflows/ci.yml` (top-level `env:` block)

These four vars are not secrets (just backend config) and are harmless on the `frontend` job, so they live at workflow scope to avoid triplication. The `harmony-client` key prefix namespaces our entries in the bucket shared with the harmony repo.

**Step 1: Add the static sccache backend env**

Find (the top-level `env:` block — currently ends at `CARGO_INCREMENTAL: '0'`):

```yaml
  CARGO_INCREMENTAL: '0'
```

Replace with:

```yaml
  CARGO_INCREMENTAL: '0'
  # ZEB-499: sccache → Cloudflare R2 backend config (not secrets). The actual
  # R2 credentials are injected per-Rust-job from secrets; RUSTC_WRAPPER is
  # gated on their presence (see each Rust job's "Enable sccache" step), so the
  # frontend job and fork PRs that lack the secret simply ignore these.
  SCCACHE_BUCKET: harmony-sccache
  SCCACHE_ENDPOINT: https://1ba234d340e59c6bba1e0fe90b7db8db.r2.cloudflarestorage.com
  SCCACHE_REGION: auto
  SCCACHE_S3_KEY_PREFIX: harmony-client
```

**Step 2: Verify YAML still parses**

Run:

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml')); print('valid')"
```

Expected: `valid`

---

### Task 2: Wire sccache into the three Rust jobs

**Files:** Modify `.github/workflows/ci.yml` (`rust-check`, `rust-test`, `msrv` jobs)

For EACH of the three Rust jobs, four changes: (a) job-level `AWS_*` creds env, (b) install sccache, (c) gate `RUSTC_WRAPPER`, (d) `--show-stats`. Plus flip its `rust-cache` to registry-only. The reusable blocks are below; per-job placement follows.

**Reusable block A — job-level creds env** (the R2 token; scoped to Rust jobs only, never `frontend`):

```yaml
    env:
      AWS_ACCESS_KEY_ID: ${{ secrets.SCCACHE_R2_ACCESS_KEY_ID }}
      AWS_SECRET_ACCESS_KEY: ${{ secrets.SCCACHE_R2_SECRET_ACCESS_KEY }}
```

**Reusable block B — enable-sccache step** (gates `RUSTC_WRAPPER` on the secret; degrades gracefully when absent):

```yaml
      # ZEB-499: turn sccache on only when the R2 credential is present. Fork
      # PRs (and any run without the secret) leave RUSTC_WRAPPER unset and
      # compile normally — cold, but green. SCCACHE_ERROR_LOG surfaces backend
      # issues; sccache treats a backend error as a cache miss (compiles
      # directly), so a transient R2 outage can't fail the job.
      - name: Enable sccache when R2 creds present (ZEB-499)
        env:
          R2_KEY: ${{ secrets.SCCACHE_R2_ACCESS_KEY_ID }}
        run: |
          if [ -n "$R2_KEY" ]; then
            echo "RUSTC_WRAPPER=sccache" >> "$GITHUB_ENV"
            echo "SCCACHE_ERROR_LOG=/tmp/sccache.log" >> "$GITHUB_ENV"
            echo "sccache: R2 backend enabled (prefix=$SCCACHE_S3_KEY_PREFIX)"
          else
            echo "sccache: no R2 credentials — building without sccache"
          fi
```

**Reusable block C — show-stats step** (telemetry; safe when sccache is off):

```yaml
      - name: sccache stats (ZEB-499)
        if: always()
        run: sccache --show-stats || echo "sccache not active this run"
```

**Step 1: `rust-check` — add creds env**

Find:

```yaml
  rust-check:
    name: Rust — fmt + clippy
```

Replace with (insert block A right after the `name:` line):

```yaml
  rust-check:
    name: Rust — fmt + clippy
    env:
      AWS_ACCESS_KEY_ID: ${{ secrets.SCCACHE_R2_ACCESS_KEY_ID }}
      AWS_SECRET_ACCESS_KEY: ${{ secrets.SCCACHE_R2_SECRET_ACCESS_KEY }}
```

**Step 2: `rust-check` — install sccache + enable it (before rust-cache)**

Find (the rust-check toolchain step, which is followed by the rust-cache step):

```yaml
      - uses: dtolnay/rust-toolchain@3c5f7ea28cd621ae0bf5283f0e981fb97b8a7af9  # master
        with:
          toolchain: stable
          components: rustfmt, clippy

      # Per-job cache key so rust-check and rust-test don't trample each
```

Replace with:

```yaml
      - uses: dtolnay/rust-toolchain@3c5f7ea28cd621ae0bf5283f0e981fb97b8a7af9  # master
        with:
          toolchain: stable
          components: rustfmt, clippy

      # ZEB-499: install sccache (pinned, binary, off apt) — see docs/ci-sccache.md.
      - uses: taiki-e/install-action@ec28e287910af896fd98e04056d31fa68607e7ad  # v2.77.4
        with:
          tool: sccache

      # ZEB-499: turn sccache on only when the R2 credential is present. Fork
      # PRs (and any run without the secret) leave RUSTC_WRAPPER unset and
      # compile normally — cold, but green. SCCACHE_ERROR_LOG surfaces backend
      # issues; sccache treats a backend error as a cache miss (compiles
      # directly), so a transient R2 outage can't fail the job.
      - name: Enable sccache when R2 creds present (ZEB-499)
        env:
          R2_KEY: ${{ secrets.SCCACHE_R2_ACCESS_KEY_ID }}
        run: |
          if [ -n "$R2_KEY" ]; then
            echo "RUSTC_WRAPPER=sccache" >> "$GITHUB_ENV"
            echo "SCCACHE_ERROR_LOG=/tmp/sccache.log" >> "$GITHUB_ENV"
            echo "sccache: R2 backend enabled (prefix=$SCCACHE_S3_KEY_PREFIX)"
          else
            echo "sccache: no R2 credentials — building without sccache"
          fi

      # Per-job cache key so rust-check and rust-test don't trample each
```

**Step 3: `rust-check` — rust-cache registry-only**

Find:

```yaml
        with:
          workspaces: src-tauri
          key: check
```

Replace with:

```yaml
        with:
          workspaces: src-tauri
          key: check
          # ZEB-499: sccache+R2 now owns compilation caching, so stop tarballing
          # target/ — this is what kills the 10 GB Actions-cache churn (ZEB-440).
          # rust-cache is kept solely to warm ~/.cargo (registry/index), which
          # sccache does not cache.
          cache-targets: false
```

**Step 4: `rust-check` — add show-stats after clippy**

Find:

```yaml
      - name: cargo clippy --locked --all-targets
        run: cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```

Replace with:

```yaml
      - name: cargo clippy --locked --all-targets
        run: cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings

      - name: sccache stats (ZEB-499)
        if: always()
        run: sccache --show-stats || echo "sccache not active this run"
```

**Step 5: `rust-test` — add creds env**

Find:

```yaml
  rust-test:
    name: Rust — test (nextest)
```

Replace with:

```yaml
  rust-test:
    name: Rust — test (nextest)
    env:
      AWS_ACCESS_KEY_ID: ${{ secrets.SCCACHE_R2_ACCESS_KEY_ID }}
      AWS_SECRET_ACCESS_KEY: ${{ secrets.SCCACHE_R2_SECRET_ACCESS_KEY }}
```

**Step 6: `rust-test` — add sccache to the existing install-action + enable it**

Find:

```yaml
      - uses: taiki-e/install-action@ec28e287910af896fd98e04056d31fa68607e7ad  # v2.77.4
        with:
          tool: cargo-nextest
```

Replace with:

```yaml
      - uses: taiki-e/install-action@ec28e287910af896fd98e04056d31fa68607e7ad  # v2.77.4
        with:
          tool: cargo-nextest,sccache

      # ZEB-499: turn sccache on only when the R2 credential is present. Fork
      # PRs (and any run without the secret) leave RUSTC_WRAPPER unset and
      # compile normally — cold, but green. SCCACHE_ERROR_LOG surfaces backend
      # issues; sccache treats a backend error as a cache miss (compiles
      # directly), so a transient R2 outage can't fail the job.
      - name: Enable sccache when R2 creds present (ZEB-499)
        env:
          R2_KEY: ${{ secrets.SCCACHE_R2_ACCESS_KEY_ID }}
        run: |
          if [ -n "$R2_KEY" ]; then
            echo "RUSTC_WRAPPER=sccache" >> "$GITHUB_ENV"
            echo "SCCACHE_ERROR_LOG=/tmp/sccache.log" >> "$GITHUB_ENV"
            echo "sccache: R2 backend enabled (prefix=$SCCACHE_S3_KEY_PREFIX)"
          else
            echo "sccache: no R2 credentials — building without sccache"
          fi
```

**Step 7: `rust-test` — rust-cache registry-only**

Find:

```yaml
        with:
          workspaces: src-tauri
          key: test
```

Replace with:

```yaml
        with:
          workspaces: src-tauri
          key: test
          # ZEB-499: sccache+R2 owns compilation caching now — stop tarballing
          # target/ (kills the 10 GB churn, ZEB-440). rust-cache warms ~/.cargo only.
          cache-targets: false
```

**Step 8: `rust-test` — add show-stats after nextest**

Find:

```yaml
        run: cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast
```

Replace with:

```yaml
        run: cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast

      - name: sccache stats (ZEB-499)
        if: always()
        run: sccache --show-stats || echo "sccache not active this run"
```

**Step 9: `msrv` — add creds env**

Find:

```yaml
  msrv:
    name: MSRV — cargo check on declared rust-version
```

Replace with:

```yaml
  msrv:
    name: MSRV — cargo check on declared rust-version
    env:
      AWS_ACCESS_KEY_ID: ${{ secrets.SCCACHE_R2_ACCESS_KEY_ID }}
      AWS_SECRET_ACCESS_KEY: ${{ secrets.SCCACHE_R2_SECRET_ACCESS_KEY }}
```

**Step 10: `msrv` — install sccache + enable it (before rust-cache)**

Find (the msrv toolchain step, followed by the rust-cache step):

```yaml
      - uses: dtolnay/rust-toolchain@3c5f7ea28cd621ae0bf5283f0e981fb97b8a7af9  # master
        with:
          toolchain: ${{ steps.msrv.outputs.msrv }}

      - uses: Swatinem/rust-cache@42dc69e1aa15d09112580998cf2ef0119e2e91ae  # v2
        with:
          workspaces: src-tauri
          key: msrv
```

Replace with:

```yaml
      - uses: dtolnay/rust-toolchain@3c5f7ea28cd621ae0bf5283f0e981fb97b8a7af9  # master
        with:
          toolchain: ${{ steps.msrv.outputs.msrv }}

      # ZEB-499: install sccache (pinned, binary, off apt) — see docs/ci-sccache.md.
      - uses: taiki-e/install-action@ec28e287910af896fd98e04056d31fa68607e7ad  # v2.77.4
        with:
          tool: sccache

      # ZEB-499: turn sccache on only when the R2 credential is present. Fork
      # PRs (and any run without the secret) leave RUSTC_WRAPPER unset and
      # compile normally — cold, but green. SCCACHE_ERROR_LOG surfaces backend
      # issues; sccache treats a backend error as a cache miss (compiles
      # directly), so a transient R2 outage can't fail the job.
      - name: Enable sccache when R2 creds present (ZEB-499)
        env:
          R2_KEY: ${{ secrets.SCCACHE_R2_ACCESS_KEY_ID }}
        run: |
          if [ -n "$R2_KEY" ]; then
            echo "RUSTC_WRAPPER=sccache" >> "$GITHUB_ENV"
            echo "SCCACHE_ERROR_LOG=/tmp/sccache.log" >> "$GITHUB_ENV"
            echo "sccache: R2 backend enabled (prefix=$SCCACHE_S3_KEY_PREFIX)"
          else
            echo "sccache: no R2 credentials — building without sccache"
          fi

      - uses: Swatinem/rust-cache@42dc69e1aa15d09112580998cf2ef0119e2e91ae  # v2
        with:
          workspaces: src-tauri
          key: msrv
          # ZEB-499: sccache+R2 owns compilation caching now — stop tarballing
          # target/ (kills the 10 GB churn, ZEB-440). rust-cache warms ~/.cargo only.
          cache-targets: false
```

**Step 11: `msrv` — add show-stats after cargo check**

Find:

```yaml
      - name: cargo check --locked --all-targets
        run: cargo check --locked --all-targets --features test-fixtures
```

Replace with:

```yaml
      - name: cargo check --locked --all-targets
        run: cargo check --locked --all-targets --features test-fixtures

      - name: sccache stats (ZEB-499)
        if: always()
        run: sccache --show-stats || echo "sccache not active this run"
```

**Step 12: Validate the workflow**

Run:

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml')); print('valid YAML')"
grep -c "tool: sccache\|tool: cargo-nextest,sccache" .github/workflows/ci.yml   # expect 3
grep -c "Enable sccache when R2 creds present" .github/workflows/ci.yml          # expect 3
grep -c "cache-targets: false" .github/workflows/ci.yml                          # expect 3
grep -c "sccache --show-stats" .github/workflows/ci.yml                          # expect 3
command -v actionlint >/dev/null 2>&1 && actionlint .github/workflows/ci.yml && echo "actionlint: clean"
```

Expected: `valid YAML`, four `3`s, `actionlint: clean`.

**Step 13: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add .github/workflows/ci.yml
git commit -m "$(cat <<'EOF'
ci(zeb-499): cache rustc compilation on R2 via sccache

The real ZEB-498 bottleneck (confirmed on #290) is recompiling the
workspace + vendored crates + ~118 test binaries every run — which
Swatinem/rust-cache structurally cannot cache. Wire sccache (pinned
taiki-e/install-action) on rust-check/rust-test/msrv, backed by the
shared Cloudflare R2 bucket (key-prefixed harmony-client), gated on the
R2 secret so fork PRs degrade gracefully to a normal cold build. Flip
the three rust-cache steps to cache-targets:false: R2 now owns
compilation caching, which also eliminates the 10 GB target-cache churn
(ZEB-440). CARGO_INCREMENTAL=0 (sccache prerequisite) + mold shipped in
slice 1 (#290).

Spec: docs/specs/2026-06-18-zeb-499-sccache-r2-ci-design.md

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Operator doc + CLAUDE.md note

**Files:** Create `docs/ci-sccache.md`; modify `CLAUDE.md`

**Step 1: Write `docs/ci-sccache.md`**

```markdown
# CI compilation cache (sccache → Cloudflare R2)

harmony-client CI caches Rust compilation with [sccache](https://github.com/mozilla/sccache),
backed by the shared Cloudflare R2 bucket `harmony-sccache`. This caches
*every* crate's `rustc` output — including the workspace crate and the
vendored `zenoh-link` fork — which `Swatinem/rust-cache` cannot (it only
caches third-party dependency artifacts). See ZEB-499.

## How it's wired

- The three Rust jobs (`rust-check`, `rust-test`, `msrv`) install sccache via
  the pinned `taiki-e/install-action` and set `RUSTC_WRAPPER=sccache`.
- Backend config is workflow-level env: `SCCACHE_BUCKET=harmony-sccache`,
  `SCCACHE_ENDPOINT` (R2 S3 endpoint), `SCCACHE_REGION=auto`, and
  `SCCACHE_S3_KEY_PREFIX=harmony-client` (namespaces our entries vs the
  harmony repo, which shares the bucket).
- `CARGO_INCREMENTAL=0` is required — incremental artifacts are not cacheable
  by sccache.
- `SCCACHE_IDLE_TIMEOUT=0` keeps the sccache server alive for the whole job.
  Its 600s default otherwise expires during `rust-test`'s long
  test-execution phase, so the server self-terminates and the end-of-job
  `sccache --show-stats` spins up a fresh one reporting zeros (the compiles
  still cached to R2 — only the telemetry was lost).
- `Swatinem/rust-cache` is kept with `cache-targets: false` to warm `~/.cargo`
  (the crates.io index/downloads), which sccache does not cache. We no longer
  cache `target/` — R2 owns compiled artifacts, which also removed the 10 GB
  Actions-cache churn (ZEB-440).

## Credentials

Two repo secrets hold a single R2 API token scoped to the `harmony-sccache`
bucket with Object Read & Write:

- `SCCACHE_R2_ACCESS_KEY_ID`
- `SCCACHE_R2_SECRET_ACCESS_KEY`

They are exported as `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` at job
scope on the Rust jobs only. `RUSTC_WRAPPER=sccache` is set **only when the
secret is present**, so fork PRs (which GitHub denies secrets) compile
normally without sccache instead of failing.

**Rotating the token:** create a new R2 API token in the Cloudflare dashboard
(R2 → API Tokens, Object Read & Write on `harmony-sccache`), update both repo
secrets (`gh secret set SCCACHE_R2_ACCESS_KEY_ID` / `..._SECRET_ACCESS_KEY`),
then revoke the old token.

## Verifying it works

Each Rust job ends with `sccache --show-stats` and, when non-empty, dumps
`SCCACHE_ERROR_LOG` in a collapsed group — so backend/auth/network failures
(e.g. a bad R2 credential) are visible in the log, not just counted under
`Cache errors`. On a PR that doesn't change Rust source, expect a high
cache-hit rate; the first `main` run after a dependency or source change is
mostly misses (it uploads to R2 for next time).
```

**Step 2: Update the CLAUDE.md sccache note**

Find (in `CLAUDE.md`, the sccache productivity tip):

```markdown
First compile after install populates the cache; subsequent compiles of the same dep graph are near-instant. CI does not use sccache (yet — see ZEB-273 Tier 2 follow-up).
```

Replace with:

```markdown
First compile after install populates the cache; subsequent compiles of the same dep graph are near-instant. CI uses sccache backed by Cloudflare R2 (ZEB-499) — see [`docs/ci-sccache.md`](docs/ci-sccache.md).
```

**Step 3: Verify the CLAUDE.md edit landed**

Run:

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
grep -c "CI uses sccache backed by Cloudflare R2" CLAUDE.md   # expect 1
grep -c "CI does not use sccache" CLAUDE.md                   # expect 0
test -f docs/ci-sccache.md && echo "doc present"
```

Expected: `1`, `0`, `doc present`.

**Step 4: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add docs/ci-sccache.md CLAUDE.md
git commit -m "$(cat <<'EOF'
docs(zeb-499): operator doc for CI sccache + update CLAUDE.md note

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Open PR and measure cache hits

**Files:** none (process / observation)

**Step 1: Push and open the PR**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git push -u origin zeb-499-sccache-r2-ci
gh pr create --repo zeblithic/harmony-client --base main --head zeb-499-sccache-r2-ci \
  --title "ZEB-499: sccache -> R2 CI compilation cache" \
  --body "$(cat <<'EOF'
## What

Caches Rust `rustc` compilation on Cloudflare R2 via sccache, on `rust-check` / `rust-test` / `msrv`. Unlike `Swatinem/rust-cache` (third-party deps only), sccache caches **every** crate — the workspace crate and the vendored `zenoh-link` fork included — which is the real CI bottleneck confirmed on the previous slice. `rust-cache` flips to `cache-targets: false` (registry-only), so R2 owns compilation caching and the 10 GB target-cache churn disappears.

- Single R2 token (Object R/W on `harmony-sccache`), key-prefixed `harmony-client`, exported as `AWS_*` at job scope on the Rust jobs only.
- `RUSTC_WRAPPER=sccache` is gated on the secret being present → fork PRs compile normally (no hard fail).
- `CARGO_INCREMENTAL=0` (sccache prerequisite) + mold already on main.

## Validation

This PR's own run **populates** R2 (mostly misses). The proof is the next no-Rust-change PR: `sccache --show-stats` (now printed at the end of each Rust job) should show mostly hits, and the Rust jobs should drop well under the ~20-min target. If sustained, a tiny follow-up walks `timeout-minutes` 45 -> 30.

Spec: `docs/specs/2026-06-18-zeb-499-sccache-r2-ci-design.md`
Doc: `docs/ci-sccache.md`

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

(Keep `Closes ZEB-NNN` out of the body — branch-name link will associate ZEB-499; reopen on merge if any out-of-scope follow-ups remain. Put cross-refs in a PR comment.)

**Step 2: After CI completes, read the sccache stats**

In each Rust job's `sccache stats` step, read the hit/miss counts. This PR is expected to be mostly **misses** (cold R2 + the ci.yml change doesn't change Rust source, but R2 starts empty for the `harmony-client` prefix). Confirm sccache **connected to R2** (stats show "Cache location: ... harmony-sccache" / non-zero "Cache writes", no auth errors). The decisive measurement is Task 4 Step 3.

```bash
run_id=$(gh run list --repo zeblithic/harmony-client --branch zeb-499-sccache-r2-ci --workflow CI --limit 1 --json databaseId --jq '.[0].databaseId')
for j in "Rust — test (nextest)" "Rust — fmt + clippy"; do
  jid=$(gh run view "$run_id" --repo zeblithic/harmony-client --json jobs --jq ".jobs[]|select(.name==\"$j\")|.databaseId")
  echo "=== $j ==="; gh run view --repo zeblithic/harmony-client --job "$jid" --log 2>/dev/null | grep -iE "Compile requests|Cache hits|Cache misses|Cache writes|Cache location|sccache: R2 backend|error|Non-cacheable" | head -20
done
```

**Step 3: After merge, confirm the win on a follow-up no-Rust-change PR**

Once this is merged (main run populates R2), the next PR that touches no Rust source should show a **high hit rate** and substantially faster Rust jobs. Record the before/after Rust-job durations in a comment on ZEB-499. If the hit rate is high and jobs are comfortably under ~20 min, file the `timeout-minutes` 45→30 walk-back as the ZEB-498 finisher.

---

## Self-Review

- **Spec coverage:** core wiring → Task 2 Steps 1-11; static backend env → Task 1; `SCCACHE_S3_KEY_PREFIX` namespacing → Task 1 Step 1; rust-cache registry-only → Task 2 Steps 3/7/10; graceful degradation → reusable block B (gated `RUSTC_WRAPPER`); single R/W token job-scoped to Rust jobs → reusable block A; `--show-stats` telemetry → block C; operator doc + CLAUDE.md → Task 3; validation/success → Task 4. All spec sections covered.
- **Placeholder scan:** `<run-id>`/`<jid>` are runtime values the executor fills from prior output, not design gaps. No "TBD"/"handle errors" left.
- **Consistency:** the `taiki-e/install-action` SHA (`ec28e287…`), the `Swatinem/rust-cache` SHA (`42dc69e1…`), secret names, `SCCACHE_S3_KEY_PREFIX=harmony-client`, and the three `cache-targets: false` flips are identical everywhere they appear.
