# Harden the CI mold-install gate — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop a transient network failure on the `setup-mold` install step from reding a required CI/release gate, since mold is a pure speed optimization wired fail-closed.

**Architecture:** Add `continue-on-error: true` to the four mold-install steps so a failed install (which — verified — cannot corrupt `/usr/bin/ld` under the action's `errexit`) lets the job proceed on the distro-default linker. YAML-only; no build logic changes.

**Tech Stack:** GitHub Actions workflows (`ci.yml`, `release.yml`); `actionlint` for local validation.

## Global Constraints

- **Pin untouched:** do NOT change the action SHA (`rui314/setup-mold@9c9c13bf4c3f1adef0cc596abc155580bcb04444`), `mold-version` (`'2.41.0'`), or `make-default: true`.
- **YAML only:** no changes to Rust, frontend, job dependencies, or required-check config.
- **First `continue-on-error` in the repo:** every added `continue-on-error: true` carries a one-line comment stating the safety invariant so the rationale travels with the code.
- **Success path unchanged:** `continue-on-error` governs failure handling only; when the network is healthy mold installs and symlinks exactly as today.
- **Verification gate:** `actionlint` must pass clean on both edited files before commit.

---

### Task 1: Add `continue-on-error` to all four mold-install steps

**Files:**
- Modify: `.github/workflows/ci.yml` (steps at lines 87, 256, 407)
- Modify: `.github/workflows/release.yml` (step at line 148)

**Interfaces:**
- Consumes: nothing (leaf config change).
- Produces: four mold-install steps that no longer fail their job on a transient install failure.

Each of the four steps currently reads (identical shape):

```yaml
      - name: Install mold linker (ZEB-498)
        uses: rui314/setup-mold@9c9c13bf4c3f1adef0cc596abc155580bcb04444  # v1
        with:
          mold-version: '2.41.0'
          make-default: true
```

- [ ] **Step 1: Edit each of the four steps** to add a `continue-on-error` key with a safety-invariant comment, immediately after the `- name:` line, so each becomes:

```yaml
      - name: Install mold linker (ZEB-498)
        # ZEB-800: mold is a pure speed optimization. The pinned action runs
        # shell:bash (-eo pipefail), so a failed wget|tar download aborts before
        # its `ln -sf /usr/local/bin/mold "$(realpath /usr/bin/ld)"` line, so a
        # failed install keeps the distro-default linker and yields a
        # functionally equivalent (slower) build. Do not let a transient network
        # blip red a required gate.
        continue-on-error: true
        uses: rui314/setup-mold@9c9c13bf4c3f1adef0cc596abc155580bcb04444  # v1
        with:
          mold-version: '2.41.0'
          make-default: true
```

Apply verbatim to `ci.yml` (all three occurrences) and `release.yml` (the one occurrence). Do not alter the existing `# ZEB-498 …` rationale comment blocks that precede each step.

- [ ] **Step 2: Confirm exactly four steps changed and nothing else.**

Run: `git diff --stat` — expect `ci.yml` and `release.yml` only.
Run: `grep -c 'continue-on-error: true' .github/workflows/ci.yml` → `3`; `grep -c 'continue-on-error: true' .github/workflows/release.yml` → `1`.

- [ ] **Step 3: Lint both workflows.**

Run: `actionlint .github/workflows/ci.yml .github/workflows/release.yml`
Expected: no output (clean exit 0).

- [ ] **Step 4: Sanity-check the pins and knobs are untouched.**

Run: `git diff --unified=0 -- .github/workflows/ | grep -E '^[+-][^+-].*(setup-mold@|mold-version|make-default)'`
Expected: no output. `--unified=0` drops unchanged context lines (a plain `git diff` keeps them, so those pin lines — which sit next to the edited step — would match even when untouched); the `^[+-][^+-]` anchor matches only added/removed content, not the `+++`/`---` file headers. Any match therefore means a pin, `mold-version`, or `make-default` line actually changed.

- [ ] **Step 5: Commit.**

```bash
git add .github/workflows/ci.yml .github/workflows/release.yml
git commit -m "ci(ZEB-800): continue-on-error on mold install so a network blip can't red a required gate"
```

## Self-Review

**1. Spec coverage:** The spec's chosen approach (Option A: `continue-on-error: true` on the four steps, each with a safety comment) is fully covered by Task 1. Verification (`actionlint`) is Step 3. The "success path unchanged" and "pin untouched" constraints are enforced by Steps 4 and the Global Constraints. No spec requirement is unaddressed.

**2. Placeholder scan:** No TBD/TODO; the exact YAML to write is inline.

**3. Type consistency:** N/A (no code symbols). The step shape is quoted verbatim and matches the four in-tree occurrences.
