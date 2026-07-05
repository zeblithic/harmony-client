# ZEB-631: rotating test selection — design (v1: local only)

Amortize regression detection across runs instead of paying the full ~4,100-test
suite on every local gate. Jake's mechanism (ticket, 2026-07-04): deterministic
rotating partition — every test runs at least once every k rounds (hard bound,
not probabilistic) — layered under an always-run set and over full-run
backstops.

**v1 scope settled with Jake 2026-07-05: LOCAL ONLY.** CI is untouched — the PR
`rust-test` job and merge-to-main keep the full suite; the pre-PR local sweep
stays full. Sampling applies to iterative local gates (SDD per-task, converge
rounds). Extend to CI as v2 only with escape-rate data.

## Key mechanic: nextest does the hashing

`cargo-nextest 0.9.133` natively supports `--partition hash:M/N`: stable
per-test hash → fixed bucket membership. Running bucket `(round mod k)+1` each
round IS the rotating partition — after k consecutive rounds every test has
run. `-E` filtersets compose with `--partition` (partition applies to the
filtered set), so the always-run set is subtracted from the sampled pass with
`-E 'not (<always-expr>)'`. No custom hashing, no giant generated expressions,
no expression-length limits.

## `scripts/test-select` (bash, executable, repo root)

```
scripts/test-select [--context task|round] [-k N] [--dry-run] [--full] [--force] [-- extra nextest args]
```

- **Contexts → k presets:** `task` → k=4 (SDD per-task gates), `round` → k=2
  (PR converge-round re-runs). `-k N` overrides. `--full` bypasses selection
  entirely (runs the standard full command) — the escape hatch when a gate
  MUST be comprehensive.
- **Round counter:** `.testselect/round` — a git-ignored, per-machine plain
  integer file. Read → use → increment. Missing file = round 0. Counter is
  shared across contexts deliberately: alternating task/round invocations
  still advance coverage; the k used per invocation only picks the bucket
  count for THAT run (`bucket = (round mod k) + 1`).
- **Always-run set** (layer 1), derived from
  `git diff --name-only $(git merge-base origin/main HEAD)` plus staged +
  working-tree changes:
  - `src-tauri/src/<mod>.rs` → `test(<mod>)` (module-name substring — the
    same convention our scoped per-task filters already use);
  - `src-tauri/tests/<name>.rs` → `binary(<name>)`;
  - `src-tauri/Cargo.toml`, `Cargo.lock`, `.cargo/`, `vendor/` → print a
    warning recommending `--full` (dependency-graph changes defeat
    module-mapping) and proceed with selection only if `--force`;
  - frontend/docs-only paths → contribute nothing (vitest is out of scope,
    see non-goals).
- **Execution:** two sequential nextest invocations (ONE cargo at a time —
  sequential is compliant), both `--locked -p harmony-app --features
  test-fixtures` in the standard local form:
  1. always-run pass: `-E '<union of changed-module terms>'` — skipped when
     the set is empty;
  2. sampled pass: `--partition hash:<bucket>/<k>`, with
     `-E 'not (<always-expr>)'` when the always-run set is non-empty (no
     double-running).
  Script exits non-zero if EITHER pass fails; `set -o pipefail` throughout
  (pipe exit codes lie).
- **Auditability:** before running, print one summary line —
  `round=<r> k=<k> bucket=<b>/<k> always-run=[<terms>|none]` — so task
  reports and converge notes record exactly what was selected. `--dry-run`
  prints the composed command(s) and the summary without executing (also the
  seam for validating the script itself).

## Adoption (docs)

- **CLAUDE.md**: new "Iterative test selection (ZEB-631)" subsection under the
  test-running guide: when to use (`task`/`round` contexts during iteration),
  when NOT to use (pre-PR final sweep, anything CI-shaped, release
  validation — those stay `--workspace --all-targets` full), the summary-line
  convention for reports, and the `--full`/`--force` escape hatches.
- **.gitignore**: `.testselect/`.

## Failure-detection math (for the record)

A regression in the sampled middle is caught in at most k rounds (hard bound);
in expectation k/2 rounds ≈ one task cycle at k=4 given per-task + fix + review
re-run cadence. A regression in code YOU touched is caught immediately (always-
run set). Escapes reach the pre-PR full sweep at the latest — which is the same
place `--lib`-scoped per-task gates already deferred full coverage to, so v1
strictly widens per-round coverage versus today's practice (scoped `-E` filters
cover only the touched area; the rotating bucket adds 1/k of everything else).

## Non-goals (v1)

CI adoption (v2, needs escape-rate data); vitest selection (full run is ~35s —
nothing to amortize); failed-in-last-N always-run tracking (needs a failure-
history file; revisit if escapes observed); dependency-graph RTS (the ticket's
zero-infrastructure stance); doctests (none in repo); automated tests for the
script itself beyond `--dry-run` validation recorded in the PR (bash test
harness is not worth the machinery for a dev-side tool with full-run
backstops).

## Risks & mitigations

- **Stale counter file** → none: any value works, coverage cadence just
  shifts.
- **Module-name substring over-match** (`test(dm_outbox)` also matches
  `dm_outbox_extra`) → over-inclusion only; never under-runs. Acceptable.
- **Renamed/moved test files** → the partition still covers them within k
  rounds; the always-run mapping misses only until merged (full sweeps
  backstop).
- **Two-pass build cost** → second pass reuses the build; only test execution
  differs.
