# ZEB-867 — Canonical-fold pu finalize Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the finalized pu poll result a pure function of the canonical event fold (closing the kd=rs verify/apply TOCTOU and resolving cross-replica pu divergence), without changing se-mode.

**Architecture:** Two behavior changes in the Tier-3 voting engine. (1) `apply_event`'s PollResult arm recomputes the pu tally from current state at apply (cheap `tally_star`, no decrypt) instead of storing the peer-claimed value; se stays verbatim. (2) The log-apply block records a post-finalize **backdated** (out-of-order) pu `kd=rb` that the terminal guard would drop, and re-materializes the projection in canonical order so the late ballot folds in — preserving ZEB-860 live == boot-restore.

**Tech Stack:** Rust (`src-tauri/`), `cargo nextest`, existing Tier-3 lifecycle/ratification test scaffolding.

**Spec:** `docs/superpowers/specs/2026-08-03-zeb-867-canonical-fold-pu-finalize-design.md`.

## Global Constraints

- Build/test from `src-tauri/`. MSRV **1.91**. Always `--locked`.
- `cargo fmt --all -- --check` clean; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` clean.
- Full CI-parity sweep before PR: `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Iterative gates may use `scripts/test-select --context task|round`.
- **se-mode is byte-for-byte unchanged.** Both changes are pu-gated.
- Invariants preserved: ZEB-320 (drops don't advance `last_hlc`), ZEB-860 (live == canonical fold == boot-restore), ZEB-858 (never hold `voting_log` across the se-mode decrypt), divergence-safety (every stored value a pure function of canonical order).
- No new dependencies. No serialization-format change. No public-API change outside `community_voting_tier3.rs` and `community_voting_log.rs`.

---

### Task 1: Component 1 — pu recompute-at-apply in `apply_event`

**Files:**
- Modify: `src-tauri/src/community_voting_tier3.rs:1227-1232` (PollResult arm of `apply_event`).
- Test: inline `#[cfg(test)]` in `community_voting_tier3.rs`.

**Interfaces:**
- Consumes: `expected_result_from_state(&Tier3PollState) -> Result<StarResult, VerifyError>` (`:1722`, same module — for pu it calls `tally_star` over `self.ratification_ballots`, cheap, no decrypt); `self.meta.config.privacy_mode: String`; `Tier3PollResultPayload { result: StarResult }`.
- Produces: no signature change; `apply_event` PollResult arm now stores the pu recompute.

- [ ] **Step 1: Write the failing tests.** Add to the tier3 test module. Build a **pu** poll advanced into a state where a `kd=rs` applies and `expected_result_from_state` returns `Ok` (drafting candidates + status_quo present, some `ratification_ballots`) — mirror the setup used by the existing se/pu ratification tests (e.g. the helpers behind `expected_result_from_state`/`recompute_expected_result` tests). Two tests:

```rust
#[test]
fn pu_apply_recomputes_result_ignoring_claimed_payload() {
    // Arrange: a pu poll with candidates + ratification ballots, ready to finalize.
    let mut poll = /* pu poll in ratification with ballots; reuse existing helper */;
    let correct = super::expected_result_from_state(&poll).expect("pu recompute Ok");
    // A kd=rs whose payload.result is deliberately WRONG (e.g. an empty/other StarResult).
    let rs = /* build kd=rs event with a bogus payload.result != correct */;
    // Act
    assert_eq!(poll.apply_event(&rs), Ok(super::ApplyOutcome::Applied));
    // Assert: apply stored the RECOMPUTE, not the bogus claim.
    assert_eq!(poll.result.as_ref(), Some(&correct));
    assert_eq!(poll.stage, super::Stage::Finalized);
}

#[test]
fn se_apply_stores_payload_result_verbatim() {
    // Arrange: an se poll ready to finalize; a kd=rs carrying a specific result.
    let mut poll = /* se poll in ratification; reuse existing se helper */;
    let claimed = /* some StarResult */;
    let rs = /* build kd=rs with payload.result = claimed */;
    // Act + Assert: se path is unchanged — stores payload.result verbatim.
    assert_eq!(poll.apply_event(&rs), Ok(super::ApplyOutcome::Applied));
    assert_eq!(poll.result.as_ref(), Some(&claimed));
}
```

- [ ] **Step 2: Run them → FAIL.** `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(pu_apply_recomputes_result_ignoring_claimed_payload) + test(se_apply_stores_payload_result_verbatim)'`. Expected: the pu test fails (verbatim store currently keeps the bogus claim); the se test passes already (guards against regression).

- [ ] **Step 3: Implement.** Replace the PollResult arm (`:1227-1232`):

```rust
            PollEventKindCode::PollResult => {
                let payload: Tier3PollResultPayload =
                    decode_payload(&ev.payload).map_err(ApplyError::PayloadDecode)?;
                // ZEB-867: pu finalize stores the tally RECOMPUTED from the
                // canonical ballot set at apply (cheap tally_star; no decrypt),
                // not the peer-claimed `payload.result` — so the finalized pu
                // result is a pure function of the fold and the verify/apply
                // TOCTOU cannot store a stale value. All valid kd=rb are
                // canonically pre-finalize (stage-gated to Ratification), so the
                // set-tally equals the canonical-prefix tally. se stays verbatim:
                // Lagrange-invariant, already validated by the ingest memo, and
                // must never decrypt under the apply lock (ZEB-858). On the
                // (unexpected) recompute Err — e.g. StatusQuoNotSynthesized in a
                // malformed pre-drafting state — fall back to the verbatim value
                // so this arm is never worse than before.
                let result = if self.meta.config.privacy_mode == "pu" {
                    expected_result_from_state(self).unwrap_or(payload.result)
                } else {
                    payload.result
                };
                self.result = Some(result);
                self.stage = Stage::Finalized;
            }
```

- [ ] **Step 4: Run them → PASS.** Same command as Step 2. Also run the existing PollResult/finalize tests (`-E 'test(finalize) + test(poll_result) + test(kd_rb) + test(se_mode)'`) → still pass.

- [ ] **Step 5: Task gate.** `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `scripts/test-select --context task` (paste the `round=… bucket=…` line into the task report).

- [ ] **Step 6: Commit.** `git add -A && git commit -m "ZEB-867 Task 1: pu recompute-at-apply in apply_event (se verbatim)"` (+ the standard trailers).

---

### Task 2: Component 2 — record-and-rebuild for post-finalize backdated pu `kd=rb`

**Files:**
- Modify: `src-tauri/src/community_voting_log.rs` — the Tier-3 apply block (`:481-497` captures; `:499-512` the apply + error mapping).
- Test: inline `#[cfg(test)]` in `community_voting_log.rs` (mirror the ZEB-860 rebuild tests' lifecycle construction).

**Interfaces:**
- Consumes (from Task 1): `apply_event` now recomputes the pu result, so a post-finalize rebuild re-derives the augmented tally. `Tier3PollState::rebuild_from_events(&[SignedVotingEvent])`, `as_tier3_mut()`, `sync_lifecycle_from_stage(&mut PollState)`, `tier3_state.meta.config.privacy_mode`, `tier3_state.max_applied`, the tier3 `ApplyError::PollInFinalizedState`, `PollEventKindCode::RatificationBallot`.
- Produces: no signature change; the Tier-3 branch now records + rebuilds a post-finalize backdated pu ballot instead of dropping it.

- [ ] **Step 1: Add the `is_pu` capture.** After the `trigger_kind` block (`:497`), while `tier3_state` is still borrowed:

```rust
            // ZEB-867 (Component 2): capture privacy_mode now (tier3_state is
            // borrowed here and `event` is moved below) for the post-finalize
            // backdated-ballot record-and-rebuild path. pu-gated: se keeps today's
            // drop-on-finalize behavior (se finalize is Lagrange-invariant).
            let is_pu = tier3_state.meta.config.privacy_mode == "pu";
```

- [ ] **Step 2: Restructure the apply.** Replace `let outcome = tier3_state.apply_event(&event).map_err(|e| match e { … })?;` (`:499-512`) with:

```rust
            // ZEB-867 (Component 2): a backdated kd=rb (canonically pre-finalize)
            // can arrive AFTER a pu poll finalized; apply_event rejects it with
            // PollInFinalizedState (terminal guard). Instead of dropping it, RECORD
            // it and re-materialize in canonical order so the late ballot folds into
            // the tally and re-finalizes — preserving live == boot-restore. Gated to
            // out-of-order + kd=rb + pu; se and genuinely post-close (not
            // out-of-order) events keep today's drop behavior. Failed is never
            // loosened. The terminal guard runs before any field write, so the
            // rejected apply leaves the projection untouched and the rebuild is the
            // sole mutation.
            let outcome = match tier3_state.apply_event(&event) {
                Ok(o) => o,
                Err(crate::community_voting_tier3::ApplyError::PollInFinalizedState)
                    if is_pu
                        && event.kind == PollEventKindCode::RatificationBallot
                        && prev_max.as_ref().is_some_and(|m| ev_key3 <= *m) =>
                {
                    state.events.push(event.clone());
                    self.events.push(event);
                    let state = self
                        .polls
                        .get_mut(&poll_id)
                        .expect("poll present (just appended)");
                    let events = std::mem::take(&mut state.events);
                    if let Some(t3) = state.tier_state.as_tier3_mut() {
                        t3.rebuild_from_events(&events);
                    }
                    state.events = events;
                    sync_lifecycle_from_stage(state);
                    return Ok(poll_id);
                }
                Err(e) => {
                    return Err(match e {
                        crate::community_voting_tier3::ApplyError::InvalidKindForTier3(_) => {
                            ApplyError::InvalidKindForTier3
                        }
                        crate::community_voting_tier3::ApplyError::PollInFailedState
                        | crate::community_voting_tier3::ApplyError::PollInFinalizedState
                        | crate::community_voting_tier3::ApplyError::HlcNotMonotonic => {
                            ApplyError::IllegalTransition
                        }
                        crate::community_voting_tier3::ApplyError::PayloadDecode(_) => {
                            ApplyError::PayloadDecode
                        }
                    });
                }
            };
```

- [ ] **Step 3: `cargo check`.** `cd src-tauri && cargo check --locked --features test-fixtures` — compiles (verifies the borrow flow: `tier3_state`'s borrow ends at the match scrutinee, freeing `state`/`self.polls` in the record arm, exactly as the existing rebuild block re-gets `state`).

- [ ] **Step 4: Write the failing integration tests.** Mirror the lifecycle construction in the existing ZEB-860 rebuild tests (drive a Tier-3 poll through sortition → deliberation → drafting → ratification, cast `kd=rb`, then `kd=rs`). Add:

```rust
#[test]
fn pu_backdated_ballot_after_finalize_refolds() {
    // Drive a PU poll to Ratification with ballots b1,b2 (HLCs t1<t2), then
    // finalize with a kd=rs at HLC t_rs > t2. Capture result_before.
    // Then apply a BACKDATED valid kd=rb b0 at HLC t0 < t_rs (still in the
    // Ratification window), arriving after finalize.
    let mut log = /* VotingLog with the finalized pu poll */;
    let result_before = /* poll.result */;
    let r = log.apply_with_snapshot(b0_event.clone(), &community_id, snapshot);
    assert!(r.is_ok(), "backdated pu ballot is recorded + rebuilt, not rejected");
    let after = /* poll.result */;
    assert_ne!(after, result_before, "late ballot changed the finalized tally");
    // live == a forced canonical rebuild
    let mut clone = /* the poll state */;
    let events = /* poll.events clone */;
    clone.rebuild_from_events(&events);
    assert_eq!(clone.result, after);
    assert_eq!(clone.stage, Stage::Finalized);
}

#[test]
fn pu_post_close_higher_hlc_ballot_excluded() {
    // Same finalized pu poll; a kd=rb with HLC t_hi > t_rs (NOT out-of-order).
    // apply_event rejects (terminal guard) and it is NOT recorded → dropped.
    let mut log = /* finalized pu poll */;
    let before = /* poll.result */;
    let r = log.apply_with_snapshot(b_hi_event, &community_id, snapshot);
    assert!(r.is_err(), "genuine post-close ballot stays dropped");
    assert_eq!(/* poll.result */, before, "result unchanged");
}

#[test]
fn se_late_ballot_after_finalize_is_unaffected() {
    // An se poll finalized; a late kd=rb after finalize is dropped (today's
    // behavior) — pu-gate means se never takes the record-and-rebuild path.
    let mut log = /* finalized se poll */;
    let before = /* poll.result */;
    let r = log.apply_with_snapshot(se_late_ballot, &community_id, snapshot);
    assert!(r.is_err(), "se late ballot dropped, unchanged from today");
    assert_eq!(/* poll.result */, before);
}

#[test]
fn pu_finalize_converges_under_reordered_delivery() {
    // Two Tier3PollState replicas fed the SAME event set {b0,b1,b2,rs} in two
    // different orders (one in-order, one with b0 delivered after rs) finalize
    // to the SAME result — the CRDT convergence property.
    let events_in_order = vec![/* b0,b1,b2,rs */];
    let events_reordered = vec![/* b1,b2,rs,b0 */];
    let a = /* apply events_in_order into replica A via the log path */;
    let b = /* apply events_reordered into replica B via the log path */;
    assert_eq!(a_result, b_result);
    assert_eq!(a_stage, Stage::Finalized);
}
```

- [ ] **Step 5: Run them → the reorder/backdated tests FAIL before, PASS after Step 2.** Since Step 2 is already implemented, run to confirm PASS: `cargo nextest run --locked --features test-fixtures -E 'test(pu_backdated_ballot_after_finalize_refolds) + test(pu_post_close_higher_hlc_ballot_excluded) + test(se_late_ballot_after_finalize_is_unaffected) + test(pu_finalize_converges_under_reordered_delivery)'`. (If iterating TDD-strict, stash Step 2, watch the pu tests fail, restore.)

- [ ] **Step 6: Run the ZEB-860 rebuild + ZEB-868 cache regression tests** → still pass: `-E 'test(rebuilt) + test(rebuild) + test(byzantine_backdated) + test(in_order_delivery) + test(rb_nizk)'`.

- [ ] **Step 7: Task gate.** fmt; clippy `--all-targets`; `scripts/test-select --context task`.

- [ ] **Step 8: Commit.** `git add -A && git commit -m "ZEB-867 Task 2: record + rebuild post-finalize backdated pu kd=rb"` (+ trailers).

---

### Final: whole-branch verification + PR

- [ ] Full CI-parity sweep: `cargo fmt --all -- --check` + `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` + `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
- [ ] Whole-branch self-review against the spec: pu recompute-at-apply + pu-gated record-and-rebuild; se byte-for-byte unchanged; live == restore; no ZEB-320/858 regression; `kd=rs` ingest path untouched.
- [ ] PR against `main`, `Closes ZEB-867`. CodeRabbit once at open; Greptile excluded; converge Qodo/CodeAnt in one push/round; never auto-merge.
