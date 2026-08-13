# ZEB-897: Doc-Only CI Skip Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A doc-only push to `main` no longer spins up any ci.yml job (and can no longer cancel a code merge's post-merge run); PRs are untouched.

**Architecture:** One edit to `.github/workflows/ci.yml`'s `on:` block — `paths-ignore` on the `push` trigger only, per the spec (`docs/superpowers/specs/2026-08-12-zeb897-doc-only-ci-skip-design.md`).

**Tech Stack:** GitHub Actions workflow YAML.

## Global Constraints

- `pull_request` trigger MUST remain unfiltered (spec §3 — future-proof against required checks).
- Ignore list exactly: `docs/**`, `**.md`, `LICENSE` (spec §3 rationale; do not widen).
- No other change to ci.yml — jobs, concurrency, env, comments all stay as-is.

---

### Task 1: paths-ignore on the push trigger

**Files:**
- Modify: `.github/workflows/ci.yml` (the `on:` block, lines 3-6)

**Interfaces:**
- Consumes: nothing from other tasks (single-task plan).
- Produces: the trigger config below; nothing downstream consumes it except GitHub.

- [ ] **Step 1: Edit the trigger block**

Replace:

```yaml
on:
  push:
    branches: [main]
  pull_request:
    branches: [main]
```

with:

```yaml
on:
  push:
    branches: [main]
    # ZEB-897: doc-only pushes to main (specs/plans/runbooks land directly on
    # main by convention) used to burn the full ~12-min matrix AND — via the
    # cancel-in-progress concurrency group below — cancel the still-running
    # post-merge validation of the code merge before them. Skipped pushes
    # create no run object, so they can neither waste runners nor cancel
    # anything. The skip applies only if EVERY changed file in the push
    # matches. Deliberately push-only: pull_request stays unfiltered so no
    # PR can ever wait on a check that will never report (required-checks
    # wedge), under any future branch-protection config.
    paths-ignore:
      - 'docs/**'
      - '**.md'
      - 'LICENSE'
  pull_request:
    branches: [main]
```

- [ ] **Step 2: Validate the YAML**

Run from repo root: `actionlint .github/workflows/ci.yml` if installed, else
`python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml')); print('YAML OK')"`.
Expected: no errors.

- [ ] **Step 3: Confirm the diff is minimal**

Run: `git diff --stat` — exactly one file, `.github/workflows/ci.yml`, ~+13 lines.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ZEB-897: skip ci.yml on doc-only pushes to main (paths-ignore, push trigger only)"
```

- [ ] **Step 5: Push branch + open PR; verify the PR run itself**

The PR exercises the unfiltered `pull_request` trigger — all 8 checks
(rust-check, 3× rust-test shards, rust-test-gate, msrv, frontend, + bots) must
appear and go green. That is the live proof the PR path is untouched.

- [ ] **Step 6 (post-merge, deferred): observe the skip**

After Jake merges: the next doc-only push to main must produce no ci.yml run for
its head SHA (`gh run list --workflow ci.yml --branch main`); the next code push
runs normally. Record the observation on ZEB-897 before closing out.
