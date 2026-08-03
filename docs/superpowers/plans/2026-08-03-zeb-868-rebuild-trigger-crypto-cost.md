# ZEB-868 — Bound out-of-order rebuild cost Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Tier-3 poll projection rebuilds crypto-free by memoizing the dominant per-ballot crypto, without changing any accept/reject outcome.

**Architecture:** One behavior-preserving optimization in the Tier-3 voting engine — memoize the se-mode `rb` NIZK verdict in `Tier3PollState`, keyed on `(event_hash, epoch)`, preserved across the rebuild reset, with **accept-only admission** so the cache stays bounded.

> **Scope note (PR #592 review, 2026-08-03):** the plan originally had two
> tasks — a trigger-gate (Task 1) and the verify-cache (Task 2). Adversarial
> review (CodeAnt Critical) found the **trigger-gate divergence-unsafe** (see
> spec §3), so it was **dropped/reverted**. The shipped change is the
> verify-cache only, which fully closes the cost-model gap.

**Tech Stack:** Rust (`src-tauri/`), `cargo nextest`, existing Tier-3 NIZK/committee-oracle fixtures.

**Spec:** `docs/superpowers/specs/2026-08-03-zeb-868-rebuild-trigger-crypto-cost-design.md`.

## Global Constraints

- Build/test from `src-tauri/`. MSRV **1.91**. Always `--locked`.
- `cargo fmt --all -- --check` clean; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` clean.
- Full CI-parity sweep before PR: `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Iterative gates may use `scripts/test-select --context task|round` — paste its `round=… bucket=…` summary line into the task report.
- **No accept/reject decision may change** — the change is a strict optimization; every existing test must still pass unchanged.
- No new dependencies. No serialization-format change (the new cache field is never persisted; `Tier3PollState` has no serde). No public-API change outside the two Tier-3 modules.
- Invariants preserved: ZEB-320 (drops don't advance `last_hlc`), ZEB-860 (live projection == canonical fold == boot-restore), divergence-safety (ZEB-847 family).

---

### Task 1 (REMOVED): trigger-gate on own-HLC stage

Originally: gate the `{ss,md,ds,dv}` rebuild trigger on
`current_stage_at(&event.hlc).is_pre_ratification()`. **Dropped** after PR #592
review — divergence-unsafe for `ss` (LWW overwrite) and `md` (per-actor-cap
retention is order-dependent, consumed by `current_mini_public` at Ratification
reads). See spec §3. The ZEB-860 unconditional rebuild trigger is retained (now
crypto-free thanks to Task 2).

---

### Task 2: rb-NIZK verify-cache (accept-only admission)

**Files:**
- Modify: `src-tauri/src/community_voting_tier3.rs` — add `rb_nizk_verdicts` field, init in `new_from_create`, preserve in `rebuild_from_events`, extend hand-rolled `Debug`, wire the cache into the `rb` se-mode arm (look up before verify; insert **only on the accept path**).
- Test: inline `#[cfg(test)]` in `community_voting_tier3.rs`.

**Interfaces:**
- Produces: `Tier3PollState.rb_nizk_verdicts: BTreeMap<([u8;32], u64), bool>` (`pub(crate)`).
- Consumes: `sha256_of_signing_bytes(ev) -> [u8;32]`, `committee_oracle.latest_epoch() -> Option<u64>`, `committee_oracle.committee_at_epoch(u64) -> Option<CommitteePublicState>` (owned), `verify_ballot_bundle`, `MAX_RATIFICATION_BALLOTS_PER_ACTOR`, the fixture `CommitteeOracle`.

- [ ] **Step 1: Add the field, init, preserve, Debug.**
  - Field on `Tier3PollState` (after `committee_oracle`):
    `pub(crate) rb_nizk_verdicts: std::collections::BTreeMap<([u8; 32], u64), bool>` — doc: memoizes se-mode rb NIZK verdicts; only ACCEPTED ballots admitted; preserved across rebuild; never serialized.
  - `new_from_create`: `rb_nizk_verdicts: std::collections::BTreeMap::new(),`.
  - `rebuild_from_events`: `let verdicts = std::mem::take(&mut self.rb_nizk_verdicts);` before the reset, `self.rb_nizk_verdicts = verdicts;` after.
  - `Debug`: `.field("rb_nizk_verdicts", &self.rb_nizk_verdicts.len())`.

- [ ] **Step 2:** `cargo check --locked --features test-fixtures` — compiles.

- [ ] **Step 3: Write the failing cache tests.**
  - `rb_nizk_verdict_preserved_across_rebuild` — apply a valid se ballot (cache len 1), force `rebuild_from_events(&[])`, assert the cache survived the reset (== snapshot).
  - `rb_nizk_cache_admits_only_accepted_ballots` — accepted ballot memoizes `true`; invalid-NIZK ballot NOT cached; over-cap valid ballots NOT cached (cache size == `MAX_RATIFICATION_BALLOTS_PER_ACTOR`); `(event_hash, epoch+1)` lookup misses.

- [ ] **Step 4:** run the cache tests → FAIL (cache not populated).

- [ ] **Step 5: Wire the cache into the rb se-mode arm.** Look up `(ev_hash, epoch)` before verify; on miss, verify **without inserting**; produce `nizk_ok`. On the **accept path only** (valid NIZK AND under the per-actor cap, right after the `ratification_ballots.push`), `self.rb_nizk_verdicts.insert((ev_hash, e), true)`. This keeps admission bounded (invalid/over-cap events never cached).

- [ ] **Step 6:** run the cache tests → PASS; run existing se-mode rb tests → still PASS.

- [ ] **Step 7: Task gate** — fmt, clippy (`--all-targets` for inline-test lints), scoped tests.

- [ ] **Step 8: Commit.**

---

### Final: whole-branch verification + PR

- [ ] Full CI-parity sweep: `fmt --check` + `clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` + `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
- [ ] Whole-branch self-review against the spec: behavior-preserving; reset-vs-preserve discipline (caps reset, cache preserved); bounded admission; ts uncached; no ZEB-860 trigger change.
- [ ] PR against `main`, `Closes ZEB-868`. CodeRabbit once at open; Greptile excluded; converge Qodo/CodeAnt in one push/round; never auto-merge.
