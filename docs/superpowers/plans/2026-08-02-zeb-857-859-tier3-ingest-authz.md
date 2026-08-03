# Tier-3 Voting Ingest-Authz Completion (ZEB-857/858/859) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete tier-3 voting ingest-authz — gate `kd=cl` (859), bound se-mode `verify_sr` cost (858), and verify user-action kinds on the local-publish path (857).

**Architecture:** Additive verifier + cost-bound in the tier-3 voting subsystem. Each node recomputes the legitimate value (window elapse, tally) from state it already holds and rejects mismatches, clamping peer wall-stamps to `receiver-now + MAX_FORWARD_SKEW_MS`. See `docs/superpowers/specs/2026-08-02-zeb-857-859-tier3-ingest-authz.md`.

**Tech Stack:** Rust (Tauri backend), `cargo nextest`, in-process multi-engine integration tests.

## Global Constraints

- Backend-only; frontend untouched.
- Rust gates run from `src-tauri/`: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast`.
- Iterative test runs: `cargo nextest run --lib --features test-fixtures <filter>` (avoid relinking ~97 integration binaries). Full `--all-targets --workspace` sweep only before the final push.
- The 858 memo is ephemeral `VotingReplayTracker` state — NEVER persisted, replicated, or routed through `notify_dirty`/owner-state.
- Every wall comparison clamps a peer stamp to `receiver-now + MAX_FORWARD_SKEW_MS`; read the receiver clock once and thread it (testable); never compare against a raw peer wall.
- 859's close condition is ONE shared helper called by both the engine-auto trigger and `verify_cl`.
- Landing order is 859 → 858 → 857 (858's memo key `close_event_hash` is governed by 859).
- Files: `src-tauri/src/community_voting_tier3.rs` (verifiers, `Tier3PollState`, `VerifyError`, `Stage`, `recover_secret_tally`, apply), `src-tauri/src/community_voting_log_engine.rs` (ingest `inbound_eligibility_check`, `trigger_kd_cl`, `try_finalize_secret_tally`, `publish_event`, `VotingReplayTracker`). Anchors are from recon at HEAD `57a0d309` — verify exact line/signature against the current file before editing (content may have shifted).

---

### Task 1: ZEB-859 — shared close-condition helper + `verify_cl`

**Files:**
- Modify: `src-tauri/src/community_voting_tier3.rs` (add helper on `Tier3PollState`, add `verify_cl`, add `VerifyError::CloseConditionNotMet`)
- Modify: `src-tauri/src/community_voting_log_engine.rs` (`trigger_kd_cl` calls the new shared helper instead of its inline predicate)
- Test: inline `#[cfg(test)] mod tests` in `community_voting_tier3.rs`

**Interfaces:**
- Produces: `Tier3PollState::close_condition_met(&self, at_wall_ms: u64) -> bool`; `verify_cl(event: &SignedVotingEvent, poll_state: &Tier3PollState, receiver_now_ms: u64) -> Result<(), VerifyError>`; `VerifyError::CloseConditionNotMet`.
- Consumes: existing `Tier3PollState::current_stage_at(&Hlc) -> Stage`, `meta.config.{deliberation,drafting,ratification}_window_seconds`, `meta.poll_create_hlc.wall_ms`, `clock_trust::MAX_FORWARD_SKEW_MS`.

- [ ] **Step 1: Write the failing verifier unit tests.** In the tier3 test module, using existing fixtures (`make_event`, and the sortition/ratification arrangement helpers in `ts_apply_helpers`), add:
  - `verify_cl_accepts_legitimate_close`: poll in Ratification with `event.hlc.wall = poll_create_hlc.wall + total_window`; `verify_cl(ev, &ps, receiver_now = event_wall)` → `Ok(())`.
  - `verify_cl_rejects_premature_stage`: stage before Ratification (e.g. wall = create + deliberation only) → `Err(CloseConditionNotMet)`.
  - `verify_cl_rejects_window_not_elapsed`: Ratification stage boundary not reached → `Err(CloseConditionNotMet)`.
  - `verify_cl_rejects_future_stamped_wall`: `event.hlc.wall = create + total_window + 10h` but `receiver_now_ms = create` (real clock says window NOT elapsed) → clamp to `receiver_now + MAX_FORWARD_SKEW` → `Err(CloseConditionNotMet)`.
  - `verify_cl_matches_trigger_predicate`: for a wall where the engine trigger would fire, `close_condition_met` returns true (parity guard).

- [ ] **Step 2: Run tests to verify they fail** (helper + `verify_cl` + variant don't exist).
  Run: `cargo nextest run --lib --features test-fixtures verify_cl`
  Expected: FAIL to compile / unresolved.

- [ ] **Step 3: Add the shared helper + `verify_cl` + error variant.**
  - Add `VerifyError::CloseConditionNotMet` to the `VerifyError` enum (match the existing derive/Display style).
  - Add `Tier3PollState::close_condition_met(&self, at_wall_ms: u64) -> bool`: `total_window_ms = (deliberation + drafting + ratification)_window_seconds as u64 * 1000`; `let at_hlc = <Hlc with wall = at_wall_ms>` (mirror how the trigger builds its clamped hlc — reuse the poll's logical or 0 as the trigger does); `matches!(self.current_stage_at(&at_hlc), Stage::Ratification) && at_wall_ms >= self.meta.poll_create_hlc.wall_ms.saturating_add(total_window_ms)`. Keep the exact semantics of `trigger_kd_cl:1141-1157`.
  - Add `verify_cl(event, poll_state, receiver_now_ms)`: `let clamped = event.hlc.wall_ms.min(receiver_now_ms.saturating_add(clock_trust::MAX_FORWARD_SKEW_MS)); if poll_state.close_condition_met(clamped) { Ok(()) } else { Err(VerifyError::CloseConditionNotMet) }`.

- [ ] **Step 4: Refactor `trigger_kd_cl` to call the shared helper.** In `community_voting_log_engine.rs:1141`–`1157`, replace the inline stage+window predicate with a call to `t3.close_condition_met(last_wall)` where `last_wall` is the existing clamped value (keep the existing `last_hlc.wall_ms` → `now + MAX_FORWARD_SKEW_MS` clamp at `:1123`–`1135`). Behavior must be identical — this is a pure extraction.

- [ ] **Step 5: Run tests to verify they pass.**
  Run: `cargo nextest run --lib --features test-fixtures verify_cl close_condition`
  Expected: PASS. Also run existing `trigger`/close engine tests to confirm the extraction didn't change trigger behavior: `cargo nextest run --lib --features test-fixtures trigger_kd_cl`.

- [ ] **Step 6: Commit.** `feat(voting): ZEB-859 shared close-condition helper + verify_cl (not yet wired to ingest)`

---

### Task 2: ZEB-859 — enforce `verify_cl` in the `kd=cl` ingest arm

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` (`inbound_eligibility_check` Sortition arm, `PollClose` case at `:3428`)
- Test: inline tests in `community_voting_log_engine.rs`

**Interfaces:**
- Consumes: `verify_cl`, `clock_trust::receiver_now_ms() -> Option<u64>`, the `with_tier3` poll-state clone accessor.

- [ ] **Step 1: Write the failing ingest tests.** In the engine test module (near the existing `inbound_eligibility_check` arm tests), add:
  - `inbound_kd_cl_rejects_forged_premature_close`: build a poll not yet at its close boundary, a peer `kd=cl` event, drive `inbound_eligibility_check` → `Err` (message references the close condition).
  - `inbound_kd_cl_accepts_legitimate_close`: poll at/after boundary in Ratification → `Ok(())`.
  (Reuse whatever harness the existing `sf`/`rs` arm tests use to construct a `MembershipSnapshot` + `voting_log` + poll state.)

- [ ] **Step 2: Run to verify they fail** (arm currently `=> {}` so a forged close passes).
  Run: `cargo nextest run --lib --features test-fixtures inbound_kd_cl`
  Expected: FAIL (forged close currently returns Ok).

- [ ] **Step 3: Wire `verify_cl` into the arm.** Replace `PollClose => {}` (`:3428`) with: clone the poll state via `with_tier3` (as the `rs` arm does); `match clock_trust::receiver_now_ms() { Some(now) => verify_cl(&event, &poll_state, now).map_err(|e| format!("kd=cl rejected: {e:?}"))?, None => { /* receiver clock unavailable: fail-open per clock_trust contract, consistent with ZEB-846/852 */ } }`. If the poll state is absent (no such poll yet), keep the pre-existing behavior for that case (do not newly reject — match how other arms handle a missing poll).

- [ ] **Step 4: Run to verify they pass.**
  Run: `cargo nextest run --lib --features test-fixtures inbound_kd_cl`
  Expected: PASS.

- [ ] **Step 5: Regression — run the full inbound-arm + tier3 apply suites** to confirm no legitimate close/lifecycle test broke.
  Run: `cargo nextest run --lib --features test-fixtures inbound_eligibility apply_event close`
  Expected: PASS.

- [ ] **Step 6: Commit.** `feat(voting): ZEB-859 enforce verify_cl in kd=cl ingest arm`

---

### Task 3: ZEB-858 — split `verify_sr` recompute + disambiguate errors

**Files:**
- Modify: `src-tauri/src/community_voting_tier3.rs` (`verify_sr` at `:1400`; extract recompute; add `VerifyError::TallySharesNotReady`)
- Test: inline tier3 tests

**Interfaces:**
- Produces: `recompute_expected_result(poll_state: &Tier3PollState, ordered_candidates: &[...]) -> Option<ResultTy>` where `ResultTy` is the type of `payload.result`; `VerifyError::TallySharesNotReady`. `verify_sr` keeps its `(event, poll_state) -> Result<(), VerifyError>` signature (callers unchanged) but internally calls `recompute_expected_result`.
- Consumes: existing `recover_secret_tally`, `tally_star`, the candidate-ordering construction already in `verify_sr`.

- [ ] **Step 1: Write/adjust failing unit tests.**
  - `verify_sr_shares_not_ready_returns_distinct_error`: se-mode poll where committee shares are below threshold (`recover_secret_tally` → `None`) → `Err(TallySharesNotReady)` (was `TallyMismatch`).
  - `verify_sr_forgery_returns_tally_mismatch`: se-mode poll with recoverable tally but a `kd=rs` claiming a wrong `result` → `Err(TallyMismatch)`.
  - Update the existing `verify_sr_*` tests (recon: `verify_sr_tally_mismatch_rejected_tally_mismatch` :3595 and siblings) if they asserted `TallyMismatch` for the not-ready case — retarget to `TallySharesNotReady`.

- [ ] **Step 2: Run to verify they fail.**
  Run: `cargo nextest run --lib --features test-fixtures verify_sr`
  Expected: FAIL (new variant absent / current code returns `TallyMismatch` for not-ready).

- [ ] **Step 3: Implement.**
  - Add `VerifyError::TallySharesNotReady`.
  - Extract `recompute_expected_result(poll_state, ordered_candidates) -> Option<ResultTy>`: `match privacy_mode { "se" => recover_secret_tally(poll_state, ordered_candidates), _ => Some(tally_star(ordered_candidates, &poll_state.ratification_ballots)) }`.
  - In `verify_sr`: keep R1 (`close_event_hash.is_some()` else `NotInClosedStage`); build `ordered_candidates` as today; `let expected = recompute_expected_result(poll_state, &ordered_candidates).ok_or(VerifyError::TallySharesNotReady)?; if expected != payload.result { return Err(VerifyError::TallyMismatch); }`.

- [ ] **Step 4: Run to verify they pass.**
  Run: `cargo nextest run --lib --features test-fixtures verify_sr recover_secret_tally`
  Expected: PASS.

- [ ] **Step 5: Commit.** `refactor(voting): ZEB-858 extract recompute_expected_result + split TallySharesNotReady from TallyMismatch`

---

### Task 4: ZEB-858 — post-finalize early-out in the `kd=rs` ingest arm

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` (`kd=rs` arm at `:3481`–`3488`; add `VerifyError::PollAlreadyFinalized` in tier3.rs)
- Test: inline engine tests

**Interfaces:**
- Consumes: `Tier3PollState.stage` / `Stage::Finalized`, the `with_tier3` poll-state clone already used by the `rs` arm.
- Produces: `VerifyError::PollAlreadyFinalized`.

- [ ] **Step 1: Write the failing test.** `inbound_kd_rs_finalized_poll_skips_verify_sr`: arrange an se-mode poll already `Stage::Finalized` (result set); drive `inbound_eligibility_check` with a fresh distinct-signed `kd=rs`; assert it is rejected **cheaply** — i.e. returns the `PollAlreadyFinalized`-derived error, and (to prove no decrypt) instrument via a test-visible counter on `recover_secret_tally` calls OR assert the reject happens for a poll whose committee shares are absent (so a real `verify_sr` would return `TallySharesNotReady`, not the finalized error — distinguishing the early-out from the verifier).

- [ ] **Step 2: Run to verify it fails.**
  Run: `cargo nextest run --lib --features test-fixtures inbound_kd_rs_finalized`
  Expected: FAIL (currently runs verify_sr).

- [ ] **Step 3: Implement.** In the `kd=rs` arm, before calling `verify_sr`: clone poll state (already done); `if matches!(poll_state.stage, Stage::Finalized) { return Err("kd=rs for already-finalized poll".into()); }` — surfacing `VerifyError::PollAlreadyFinalized` mapped to the arm's `Err(String)`. Place it strictly before the `verify_sr` call so no recompute runs.

- [ ] **Step 4: Run to verify it passes.**
  Run: `cargo nextest run --lib --features test-fixtures inbound_kd_rs`
  Expected: PASS.

- [ ] **Step 5: Commit.** `feat(voting): ZEB-858 post-finalize early-out before verify_sr in kd=rs ingest arm`

---

### Task 5: ZEB-858 — memoize the se recompute by `(poll_id, close_event_hash)`

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` (`VotingReplayTracker` struct + a memo field; thread it into `inbound_eligibility_check`; use it in the `kd=rs` arm)
- Test: inline engine tests

**Interfaces:**
- Produces: `VotingReplayTracker.verify_sr_memo: HashMap<(PollId, [u8; 32]), ResultTy>` (or a small dedicated type) + accessor(s) to get-or-insert; a new param on `inbound_eligibility_check` for the tracker/memo handle.
- Consumes: `recompute_expected_result` (Task 3), `close_event_hash`.

- [ ] **Step 1: Write the failing tests.**
  - `verify_sr_memo_recomputes_once_for_same_close`: two distinct-signed `kd=rs` with the SAME correct result for the same `(poll, close_hash)`; assert `recover_secret_tally` runs exactly once (counter/spy), both events accepted.
  - `verify_sr_memo_still_rejects_forged_result`: after a legitimate `kd=rs` populates the memo, a second distinct-signed `kd=rs` with a FORGED `result` for the same `(poll, close_hash)` is still `Err(TallyMismatch)` (memo caches the recomputed tally, not a pass-bit; comparison still runs).

- [ ] **Step 2: Run to verify they fail.**
  Run: `cargo nextest run --lib --features test-fixtures verify_sr_memo`
  Expected: FAIL (no memo yet / recompute runs twice).

- [ ] **Step 3: Implement.**
  - Add `verify_sr_memo: HashMap<(PollId, [u8;32]), ResultTy>` to `VotingReplayTracker` (document: ephemeral, never persisted/replicated, rebuilt-empty-on-restart is correct — do NOT `notify_dirty`).
  - Thread a `&Mutex<VotingReplayTracker>` (or a dedicated `&Mutex<VerifySrMemo>` handle) into `inbound_eligibility_check` (add the param; update both call sites `process_inbound:2859` and `apply_backfilled_event:2994`, which already hold the tracker).
  - In the `kd=rs` arm (after the Task-4 early-out): compute `key = (poll_id, close_event_hash)` (close_event_hash must be `Some` — R1); `let expected = { lock memo; if let Some(v) = memo.get(&key) { v.clone() } else { drop lock; let v = recompute_expected_result(&poll_state, &ordered)?; lock memo; memo.insert(key, v.clone()); v } }` — hold the memo lock ONLY for map ops, NEVER across the decrypt. Then compare `expected != payload.result` → `TallyMismatch`. Replace the direct `verify_sr` call in this arm with this memoized path (verify_sr itself stays intact for its unit tests and any other caller).
  - Bound the memo: cap its size (e.g. reuse the `MAX_WINDOW_KEYS`-style bound or a per-poll eviction) so it can't grow unboundedly across many polls — mirror the tracker's existing bounding discipline; if the tracker already prunes by poll lifecycle, hook the memo into the same pruning.

- [ ] **Step 4: Run to verify they pass.**
  Run: `cargo nextest run --lib --features test-fixtures verify_sr_memo inbound_kd_rs`
  Expected: PASS.

- [ ] **Step 5: Commit.** `feat(voting): ZEB-858 memoize se-mode recompute by (poll_id, close_event_hash) on VotingReplayTracker`

---

### Task 6: ZEB-857 — surgical local-path verify for user-action kinds

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` (`publish_event` at `:1893`, before `apply_with_snapshot` at `:2002`)
- Test: in-process integration test under `tests/community_voting/` (mirror `community_voting_tier3_ipc_integration.rs`) + optionally an inline unit test

**Interfaces:**
- Consumes: `verify_sd`, `verify_da_candidate_exists`, `verify_ratification_ballot` (all sync, `(event, &Tier3PollState) -> Result<(), VerifyError>`), `with_tier3`.

- [ ] **Step 1: Write the failing test.** Mirror the ZEB-850 `ipc_full_lifecycle` divergence: two in-process engines; a member who has already recorded `kd=md` (decline) then locally publishes a `kd=da` (DraftApproval). Assert `engine_a.publish_event(kd=da)` returns `Err` (local rejection) instead of applying — and that after the flow engine_a's approval count matches engine_b's (no divergence). Also assert a LEGITIMATE `kd=da` from an in-mini-public member still applies (`Ok`).

- [ ] **Step 2: Run to verify it fails.**
  Run: `cargo nextest run --features test-fixtures <new_test_name>`
  Expected: FAIL (kd=da from declined member currently applies locally).

- [ ] **Step 3: Implement.** In `publish_event`, after the event kind is known and before `apply_with_snapshot`, add a match on the Tier-3 user-action kinds ONLY:
  ```
  match kind {
      MiniPublicDecline | DraftCandidate => { let ps = with_tier3(poll_id)?; verify_sd(&event, &ps).map_err(|e| format!("local publish rejected: {e:?}"))?; }
      DraftApproval => { let ps = with_tier3(poll_id)?; verify_sd(&event, &ps)...?; verify_da_candidate_exists(&event, &ps)...?; }
      RatificationBallot => { let ps = with_tier3(poll_id)?; verify_ratification_ballot(&event, &ps)...?; }
      _ => {} // creates, engine-auto (cl/rs/sf/ss/ts), tier1/2, ds/dv/ts — unchanged
  }
  ```
  The four kinds are disjoint from engine-auto self-mints, so no exemption logic is needed. If the poll state is absent for one of these kinds (shouldn't happen for a user action on an existing poll), preserve current behavior (don't newly hard-fail beyond what apply would do).

- [ ] **Step 4: Run to verify it passes.**
  Run: `cargo nextest run --features test-fixtures <new_test_name>`
  Expected: PASS.

- [ ] **Step 5: Regression — run the tier3 IPC/integration suites** to confirm no legitimate publish flow broke (creates, engine-auto lifecycle, deliberation).
  Run: `cargo nextest run --features test-fixtures community_voting_tier3`
  Expected: PASS.

- [ ] **Step 6: Commit.** `feat(voting): ZEB-857 verify user-action kinds on local publish path (surgical symmetry)`

---

## Final verification (before PR)

- [ ] Full CI-parity sweep from `src-tauri/`: `cargo fmt --all -- --check` && `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` && `cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast`.
- [ ] Confirm no persisted/replicated state was added (memo is on `VotingReplayTracker` only; grep for any `notify_dirty`/persist touching the memo — there must be none).
- [ ] Confirm the shared close-condition helper is the sole predicate for both trigger and `verify_cl` (no duplicated inline predicate remains).
