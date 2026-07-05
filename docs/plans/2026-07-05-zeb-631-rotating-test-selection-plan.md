# ZEB-631 Rotating Test Selection Implementation Plan (v1 local-only)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `scripts/test-select` — deterministic rotating-partition local test gates (k× cheaper iterative runs, every test still runs within k rounds) + docs adoption. CI untouched (v1 scope settled with Jake).

**Architecture:** One bash script wrapping two sequential `cargo nextest` invocations: an always-run pass (`-E` over changed-module terms from the branch diff) and a sampled pass (`--partition hash:<bucket>/<k>` with the always-run set subtracted via `-E 'not (…)'`). Round counter in git-ignored `.testselect/round`. Spec: `docs/specs/2026-07-05-zeb-631-rotating-test-selection-design.md` (commit 98f60113) — the contract; read it first.

**Tech Stack:** bash (macOS /bin/bash is 3.2 — see constraints), cargo-nextest 0.9.133 (`--partition hash:M/N` verified present).

## Global Constraints

- **macOS bash 3.2 compatibility**: no `mapfile`, no associative arrays; empty-array expansion under `set -u` must use the `${arr[@]+"${arr[@]}"}` guard. Shebang `#!/usr/bin/env bash`.
- `set -euo pipefail` (pipe exit codes lie).
- The script's nextest base form is the repo-standard local scope: `cargo nextest run --locked -p harmony-app --features test-fixtures`, run from `src-tauri/`.
- ONE cargo invocation at a time — the two passes run sequentially, never in parallel.
- `--dry-run` must NOT bump the round counter and must NOT run cargo.
- No CI workflow changes; no vitest changes; no Rust source changes.
- Commit per task; branch `zeb-631-rotating-test-selection` off main@a038c754; no worktrees.

---

### Task 1: `scripts/test-select`

**Files:**
- Create: `scripts/test-select` (mode 755)

**Interfaces:**
- Produces: CLI per the spec usage line: `scripts/test-select [--context task|round] [-k N] [--dry-run] [--full] [--force] [-- extra nextest args]`. Task 2's docs and Task 3's validation consume exactly this surface.

- [ ] **Step 1: Write the script.** Complete implementation (adapt mechanically only where bash-3.2 demands):

```bash
#!/usr/bin/env bash
# ZEB-631: rotating test selection for LOCAL iterative gates only.
# Full runs remain the law for pre-PR sweeps, CI, and release validation.
# Design: docs/specs/2026-07-05-zeb-631-rotating-test-selection-design.md
set -euo pipefail

usage() {
  cat <<'EOF'
usage: scripts/test-select [--context task|round] [-k N] [--dry-run] [--full] [--force] [-- extra nextest args]
  --context task   k=4 preset (SDD per-task gates)   [default]
  --context round  k=2 preset (PR converge-round re-runs)
  -k N             explicit bucket count (overrides preset)
  --dry-run        print selection summary + composed commands; no cargo, no counter bump
  --full           bypass selection; run the standard full local command
  --force          proceed with selection despite dependency-graph changes
EOF
}

k="" context="task" dry_run=0 full=0 force=0
extra=()
while [ $# -gt 0 ]; do
  case "$1" in
    --context) context="$2"; shift 2 ;;
    -k) k="$2"; shift 2 ;;
    --dry-run) dry_run=1; shift ;;
    --full) full=1; shift ;;
    --force) force=1; shift ;;
    --) shift; extra=("$@"); break ;;
    -h|--help) usage; exit 0 ;;
    *) echo "test-select: unknown arg: $1" >&2; usage >&2; exit 2 ;;
  esac
done

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root/src-tauri"
base_cmd=(cargo nextest run --locked -p harmony-app --features test-fixtures)

emit_cmd() { printf 'test-select:'; printf ' %q' "$@"; printf '\n'; }
run_cmd() {
  emit_cmd "$@"
  [ "$dry_run" -eq 1 ] && return 0
  "$@"
}

if [ "$full" -eq 1 ]; then
  echo "test-select: FULL run (selection bypassed)"
  run_cmd "${base_cmd[@]}" ${extra[@]+"${extra[@]}"}
  exit $?
fi

case "$context" in
  task)  [ -n "$k" ] || k=4 ;;
  round) [ -n "$k" ] || k=2 ;;
  *) echo "test-select: unknown --context: $context" >&2; exit 2 ;;
esac
case "$k" in (*[!0-9]*|'') echo "test-select: invalid k: $k" >&2; exit 2 ;; esac
[ "$k" -ge 1 ] || { echo "test-select: invalid k: $k" >&2; exit 2; }

counter_file="$repo_root/.testselect/round"
round=0
[ -f "$counter_file" ] && round="$(cat "$counter_file" 2>/dev/null || echo 0)"
case "$round" in (*[!0-9]*|'') round=0 ;; esac
bucket=$(( (round % k) + 1 ))
if [ "$dry_run" -eq 0 ]; then
  mkdir -p "$repo_root/.testselect"
  echo $(( round + 1 )) > "$counter_file"
fi

# Always-run set: branch diff vs merge-base + staged + working tree.
merge_base="$(git merge-base origin/main HEAD 2>/dev/null || git rev-parse HEAD)"
changed="$( { git -C "$repo_root" diff --name-only "$merge_base" HEAD; \
              git -C "$repo_root" diff --name-only; \
              git -C "$repo_root" diff --name-only --cached; } | sort -u )"

terms="" warn_deps=0
while IFS= read -r f; do
  [ -z "$f" ] && continue
  case "$f" in
    src-tauri/Cargo.toml|src-tauri/Cargo.lock|src-tauri/.cargo/*|vendor/*)
      warn_deps=1 ;;
    src-tauri/src/*.rs)
      m="$(basename "$f" .rs)"
      [ "$m" = "lib" ] || [ "$m" = "main" ] || terms="$terms test($m)" ;;
    src-tauri/tests/*.rs)
      terms="$terms binary($(basename "$f" .rs))" ;;
  esac
done <<EOF
$changed
EOF

if [ "$warn_deps" -eq 1 ] && [ "$force" -eq 0 ]; then
  echo "test-select: dependency-graph files changed (Cargo.toml/lock/.cargo/vendor)." >&2
  echo "test-select: module mapping is unreliable — use --full (or --force to proceed)." >&2
  exit 2
fi

always_expr=""
if [ -n "$terms" ]; then
  always_expr="$(printf '%s\n' $terms | sort -u | sed 's/$/ or/' | tr '\n' ' ')"
  always_expr="${always_expr% or }"
fi

echo "test-select: round=$round k=$k bucket=$bucket/$k always-run=[${always_expr:-none}]"

if [ -n "$always_expr" ]; then
  run_cmd "${base_cmd[@]}" -E "$always_expr" ${extra[@]+"${extra[@]}"}
  run_cmd "${base_cmd[@]}" -E "not ($always_expr)" --partition "hash:$bucket/$k" ${extra[@]+"${extra[@]}"}
else
  run_cmd "${base_cmd[@]}" --partition "hash:$bucket/$k" ${extra[@]+"${extra[@]}"}
fi
```

Notes binding the implementer:
- `lib.rs`/`main.rs` are excluded from module terms (`test(lib)` would match half the suite); a change there effectively relies on the sampled pass + full backstops — acceptable v1 coarseness, note it in a comment.
- The `%q`-quoted `emit_cmd` output is the `--dry-run` validation surface — keep it exact.
- `chmod 755 scripts/test-select`.

- [ ] **Step 2: Validate with --dry-run matrix** (no cargo runs): on this branch (which has spec/plan doc changes only → always-run should be `none`): `scripts/test-select --dry-run`, `--dry-run --context round`, `--dry-run -k 8`, `--dry-run --full`. Then `touch`-free real-diff check: temporarily `git stash` nothing — instead run with a scratch edit: `echo '// x' >> src-tauri/src/pending_dm_invites.rs && scripts/test-select --dry-run` → expect `always-run=[test(pending_dm_invites)]` and the two composed commands; then `git checkout -- src-tauri/src/pending_dm_invites.rs`. Verify counter file did NOT appear (all dry runs).
- [ ] **Step 3: One real sampled run**: `scripts/test-select --context task` → expect the sampled pass to run ~1/4 of the suite and pass; verify `.testselect/round` now contains `1`. Record the printed summary + tail of nextest output in the report.
- [ ] **Step 4: Commit** `git add scripts/test-select && git commit -m "ZEB-631: scripts/test-select — rotating-partition local test selection"`.

### Task 2: docs adoption

**Files:**
- Modify: `.gitignore` (add `.testselect/`)
- Modify: `CLAUDE.md` (new subsection under "Test running guide")

- [ ] **Step 1:** `.gitignore`: append `.testselect/` with a `# ZEB-631 rotating test-selection round counter (per-machine)` comment, near other local-state entries.
- [ ] **Step 2:** CLAUDE.md subsection "Iterative test selection (ZEB-631)" placed after "Per-feature scoping": what it is (2 sentences, hard k-round coverage bound), when to use (iterative dev, SDD per-task gates → `scripts/test-select --context task`; PR converge-round re-runs → `--context round`), when NOT to use (pre-PR final sweep, CI-shaped validation, release checks — those stay the full `--workspace --all-targets` commands), the summary-line convention (paste the `round=… bucket=…` line into task reports), and the `--full` / `--force` escape hatches (dependency-graph changes demand `--full`). Include one example invocation block. Match the file's existing tone and heading depth.
- [ ] **Step 3:** Commit `"ZEB-631: adopt test-select in CLAUDE.md test guide + gitignore counter dir"`.

### Task 3: final validation sweep

- [ ] `scripts/test-select --dry-run` and `--dry-run --context round` still correct post-docs (counter untouched by dry runs; bumped exactly once total from Task 1's real run).
- [ ] `bash -n scripts/test-select` (syntax) — and confirm the script has no bash-4-isms: grep for `mapfile|declare -A|\$\{[a-zA-Z_]*\[@\],,\}`.
- [ ] Standard repo gates are unaffected (no Rust/frontend source changed): run `cd src-tauri && cargo fmt --all -- --check` only (fast confirmation nothing drifted); full sweeps not required for a scripts+docs branch — CI runs them anyway.
- [ ] Spec cross-check: every spec bullet (contexts/k, counter semantics, always-run mapping incl. deps-warning, two-pass composition, dry-run purity, summary line, gitignore, CLAUDE.md guidance, non-goals honored) maps to shipped content.
