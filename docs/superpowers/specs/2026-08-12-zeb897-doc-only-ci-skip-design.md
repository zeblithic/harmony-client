# ZEB-897: Skip full CI on doc-only pushes to main — design

**Ticket:** ZEB-897 · **Verified on:** main @ `ac1ff299` (2026-08-12)

## 1. Problem, with receipts

`.github/workflows/ci.yml` triggers on every `push: branches: [main]` and every
`pull_request` with no `paths`/`paths-ignore` filter. Under the doc-only-direct-to-main
convention (specs, plans, runbooks, doc fixes land straight on `main`), every such
commit burns the full ~12-minute matrix: rust-check (fmt + 2× clippy), 3× rust-test
nextest shards + rollup gate, msrv, frontend. Recent `gh run list` for main:

```
ac1ff299  success    ZEB-907+ZEB-921 (code merge)
e3efbfd8  success    docs(plans): ZEB-925 …          ← doc-only, full run
fb1cd863  success    docs(plans): ZEB-925 …          ← doc-only, full run
f8805fe9  cancelled  docs(specs): ZEB-925 …          ← doc-only
15b633a4  cancelled  ZEB-924 (code merge)            ← post-merge run KILLED by doc push
f9fd6379  success    docs(plans): ZEB-924 …          ← doc-only, full run
3a43df05  cancelled  docs(specs): ZEB-924 …          ← doc-only
```

Two distinct costs:

1. **Waste** — each doc-only push spins five jobs (~12 min wall, ~45 runner-minutes)
   validating code that did not change.
2. **Harm** — ci.yml's `concurrency` group (`ci-CI-…-refs/heads/main`,
   `cancel-in-progress: true`) means a doc push **cancels the still-running post-merge
   validation of the code merge before it**. Receipt above: the ZEB-924 squash-merge's
   main run (`15b633a4`) was cancelled by the ZEB-925 spec doc push. Post-merge CI on
   main is the only signal that a squash-merge (built and gated on a branch head)
   still passes as landed; doc pushes routinely destroy it.

## 2. Verified premises

- `ci.yml` `on:` block has no paths filters (read in full at `ac1ff299`).
- **`main` has no branch protection**: `GET /branches/main/protection` → 404
  "Branch not protected"; `GET /rules/branches/main` → `[]`. There are **no required
  status checks**, so the classic "paths-ignore leaves a required check stuck at
  Expected" wedge cannot occur today. The design below stays safe even if
  protection is added later.
- `release.yml` is `workflow_dispatch`-driven; no other workflow triggers on push
  to main. No merge queue (requires branch protection; none exists).

## 3. Design: `paths-ignore` on the push trigger ONLY

```yaml
on:
  push:
    branches: [main]
    paths-ignore:
      - 'docs/**'
      - '**.md'
      - 'LICENSE'
  pull_request:
    branches: [main]
```

This is the ticket's pattern 1, chosen over pattern 2 (filter both triggers + an
always-passing "CI (skipped)" reporter job):

- **`pull_request` stays unfiltered.** Every PR — even a hypothetical doc-only one —
  runs full CI, so no PR can ever wait on a check that will never report, under any
  future branch-protection config. Doc-only PRs are contrary to repo convention
  (docs go straight to main), so filtering the PR path would save nothing real
  while adding the one failure mode the ticket warns about.
- **No reporter job to maintain.** Pattern 2's skip-reporter must mirror the
  required-check name forever; with no required checks configured, it would be
  pure speculative complexity (YAGNI).

### Ignore list rationale

`docs/**` (all project docs incl. specs/plans/runbooks and their assets),
`**.md` (markdown at any depth — README, CLAUDE.md, crate READMEs; GitHub's
`**.md` glob matches root-level files, unlike `**/*.md`), `LICENSE`. Deliberately
conservative — the dangerous direction is *over*-skipping (a "doc" pattern that
matches something the build reads). Not included on purpose: `.gitignore`,
`.github/**` (workflow edits must validate themselves), asset/image paths outside
`docs/` (rare; a false full run is cheap, a false skip is not).

### Semantics worth pinning

- `paths-ignore` skips the run **only if every changed file in the push matches**
  the ignore list. A mixed push (code + docs) still runs CI. Multi-commit pushes
  are evaluated on the union of changed files across the push.
- A skipped doc push creates **no run object at all**, so it cannot trigger
  `cancel-in-progress` — closing the §1 harm as a side effect.

### Cache interaction: none

rust-cache saves are main-only (`save-if`) and Cargo.lock-keyed; a doc-only push
changes no Rust inputs, so the run it used to burn saved nothing the previous code
push hadn't already saved. sccache lives in R2, keyed by compile inputs —
orthogonal. Skipping doc runs loses zero cache warmth.

## 4. Verification

- **PR path intact:** the PR carrying this change exercises the unfiltered
  `pull_request` trigger itself — all 8 checks must appear and pass.
- **YAML validity:** `actionlint` locally if available; otherwise GitHub's parser
  rejects an invalid workflow at push (visible immediately in the Actions tab).
- **Post-merge observation (definition of done):** the first doc-only push to main
  after merge must produce **no** ci.yml run (`gh run list` shows nothing for that
  head SHA); the next code push must run normally.

## 5. Out of scope

- Filtering the `pull_request` trigger (rejected above).
- Adding branch protection / required checks (separate policy decision).
- release.yml (dispatch-only; nothing to filter).
