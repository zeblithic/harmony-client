# ZEB-297 Implementation Plan: Tier 2 Auto-Exec — Admin-Only Mint

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop non-admin replicas from minting auto-exec `SetPower` events that they cannot verify, so a Tier 2 `Finalized` proposal's `AutoExecAction::SetPower` actually lands in materialized community state on every replica via the admin race + HLC LWW dedup.

**Architecture:** Gate the mint inside `apply_auto_exec_set_power`: read the engine's materialized `power_levels` for `self_owner`, skip with a clear outcome variant if power < `POWER_THRESHOLDS.set_power` (100). Admins race to mint; the membership log's existing Zenoh sync delivers the first mint to all replicas. No wire-format change, no cross-log dependency.

**Tech Stack:** Rust (cargo), existing CommunitySyncEngine + community_membership materialize, voting_tick's `AutoExecSetPowerFn` callback.

**Linear**: [ZEB-297](https://linear.app/zeblith/issue/ZEB-297) (parent: [ZEB-291](https://linear.app/zeblith/issue/ZEB-291))

**Option chosen**: **Option 2** from the ZEB-297 ticket — "gate auto-exec execution to replicas that can actually satisfy the membership verifier." Explicitly endorsed by CodeRabbit in the PR #131 review thread.

- **Option 1 (admin co-sign)** rejected as heavy — introduces a new admin-watcher + race-to-broadcast machinery that adds centralization and coordination overhead.
- **Option 3 (consensus-finalized SetPower variant)** rejected as wire-format-heavy — would require a new event variant or modifier on `SetPower` plus a cross-log dependency in `verify_event` (membership log reads voting log state). Significant architectural lift, deferred to a future "voting-derived authorization" design pass if/when polycentric communities need admin-independent enactment.

**Spec context**: §5 auto-exec describes a "best-effort apply with fallback to manual" model (line 822: `PollExecutionFailed` event, admins enact manually). This change extends the same model — non-admin replicas don't even attempt the mint, which avoids burning CPU on guaranteed-rejection events.

**Acceptance criteria** (from ticket):

1. Auto-exec `SetPower` lands in materialized community state on every replica, regardless of which one ran the contestability tick. **Satisfied by**: admin race + existing membership log Zenoh sync delivers the mint everywhere.
2. No non-admin replica burns CPU minting events that will self-reject at verify time. **Satisfied by**: power-level guard at the top of `apply_auto_exec_set_power`.
3. Two-engine integration test: admin A + non-admin B both observe finalization; SetPower target's power_level converges. **Partial — see deferral below.**

**Deferral**: Acceptance criterion #3 (the full two-engine integration test via Zenoh sync) is **entangled with ZEB-298**: the voting-log inbound `verify_event` path is feature-gated dead code per `community_voting_log_engine.rs:265-272`. Until ZEB-298 lands, B cannot OBSERVE A's finalization via voting-log sync — the necessary observation channel is dead. We satisfy the spirit of #3 with single-process unit tests that exercise both the admin-mint and non-admin-skip paths against the same engine state; the full Zenoh-sync test lands as part of ZEB-298 (which already needs the same two-engine scaffold). This is noted in the PR body.

---

## File Structure

- **Modify**: `src-tauri/src/community_membership.rs`
  - Add `AutoExecOutcome` enum.
  - Refactor `apply_auto_exec_set_power` to return `Result<AutoExecOutcome, String>`.
  - Insert the admin power-level guard.
  - Add unit tests covering both Applied and SkippedNotAdmin paths.
- **Modify**: `src-tauri/src/community_voting_tick.rs`
  - Update `AutoExecSetPowerFn` type alias to the new return type.
  - Update the dispatch match in `run_voting_tick` to handle the new variant.
  - Add `tier2_auto_execs_skipped_not_admin: u32` field to `TickStats`.
  - Update test fixture `auto_exec_set_power` closures to return `Ok(AutoExecOutcome::Applied)`.
  - Add a tick-level unit test verifying the skip path doesn't increment `tier2_auto_execs_succeeded`.
- **Modify**: `src-tauri/src/lib.rs`
  - Production wiring at `:3397` adapts the new return type into the existing `Result<(), String>` callback shape, mapping `Ok(Skipped)` to a stats-counted no-op.

No new files. No wire-format / fixture / spec changes.

---

## Tasks

### Task 0: Pre-flight (no commit)

**Goal**: Confirm working tree state + green baseline.

- [ ] **Step 1: Verify branch state**

```bash
git status --short
git log -1 --oneline
# Expected: clean tree, HEAD on the new branch off origin/main (7ad4a44 ZEB-292 Phase 3)
```

- [ ] **Step 2: Confirm green baseline** (cargo gates from src-tauri/; tsc + vitest from repo root)

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
npx tsc --noEmit
npx vitest run
# Expected: all five green. Frontend is unchanged so tsc + vitest are noise-immune.
```

No commit. This task only proves the baseline.

---

### Task 1: Admin-only guard + outcome enum + plumbing

**Goal**: Single coherent change shipping the fix end-to-end.

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (~80 lines of changes incl. tests)
- Modify: `src-tauri/src/community_voting_tick.rs` (~30 lines of changes incl. tests)
- Modify: `src-tauri/src/lib.rs` (~10 lines, production wiring)

- [ ] **Step 1: Write failing test — non-admin call to `apply_auto_exec_set_power` returns `SkippedNotAdmin` without minting**

Add to `community_membership.rs::auto_exec_tests` mod:

```rust
/// ZEB-297: when the local user's power level in the community is below the
/// admin threshold, `apply_auto_exec_set_power` must skip (returning
/// `AutoExecOutcome::SkippedNotAdmin`) WITHOUT minting any event. Without
/// this guard, a non-admin replica would mint a SetPower that its own
/// verify_event would reject (actor_power < set_power threshold), wasting
/// CPU + cluttering the engine's local event log.
#[tokio::test]
async fn apply_auto_exec_set_power_skips_when_local_actor_is_not_admin() {
    // Set up: local user is a Joined community member with default power (0),
    // i.e. emphatically not an admin. Use the existing test scaffolding for
    // NodeState + a single community engine.
    let (node_state, community_id, target) = build_non_admin_node_state_for_test().await;

    let outcome = apply_auto_exec_set_power(&node_state, community_id, target, 50)
        .await
        .expect("non-admin call must not fail — it skips");

    assert!(
        matches!(outcome, AutoExecOutcome::SkippedNotAdmin),
        "expected SkippedNotAdmin, got {outcome:?}"
    );

    // Verify no SetPower event was added to the engine's local log.
    let event_log_len_after = read_membership_event_count(&node_state, &community_id).await;
    let event_log_len_before = 0; // build_non_admin_node_state_for_test starts empty.
    assert_eq!(
        event_log_len_after, event_log_len_before,
        "non-admin replica must not mint a SetPower event"
    );
}
```

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(apply_auto_exec_set_power_skips_when_local_actor_is_not_admin)'`
Expected: **FAIL — `AutoExecOutcome` not in scope; helper fns don't exist.**

- [ ] **Step 2: Introduce `AutoExecOutcome` enum**

Add to `community_membership.rs` near the existing `apply_auto_exec_set_power` definition (around line 2970):

```rust
/// ZEB-297: outcome of an auto-exec `SetPower` dispatch from a Tier 2
/// finalization. Distinguishes "the mint actually happened" from "this
/// replica intentionally skipped" so the tick can keep accurate metrics
/// and operators can tell admin replicas from non-admin replicas in logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoExecOutcome {
    /// Local actor satisfied `POWER_THRESHOLDS.set_power`; SetPower
    /// event was minted, signed, and inserted into the engine's local
    /// log. Peers receive it via Zenoh sync on the membership topic.
    Applied,
    /// Local actor's power level in this community is below the
    /// `set_power` threshold (100), so this replica cannot produce a
    /// SetPower event that any verifier will accept. Skip silently —
    /// admins race to mint, HLC LWW dedupes, and the first one's event
    /// propagates to every replica via the existing membership log
    /// sync. This is the intentional "wrong replica" path, not a
    /// failure.
    SkippedNotAdmin,
}
```

- [ ] **Step 3: Refactor `apply_auto_exec_set_power` signature + insert the guard**

Change the return type to `Result<AutoExecOutcome, String>` and add the guard.

After the existing `engine_arc` lookup (around line 3048) and BEFORE the `event_hlc` reservation (line 3038 — note: reorder needed; do the guard check before burning an HLC):

```rust
// ZEB-297: admin-only mint. Read the engine's materialized
// power_levels for the local actor; skip if below the set_power
// threshold. The engine's materialize cache uses the engine's
// admin_addr as the bootstrap anchor.
let actor_power = {
    let state_arc = engine_arc.state();
    let state_g = state_arc.lock().await;
    let mat = state_g.materialized(engine_arc.admin_addr());
    mat.power_levels.get(&self_owner).copied().unwrap_or(0)
};
if actor_power < crate::community_membership::POWER_THRESHOLDS.set_power {
    tracing::info!(
        community = %hex::encode(community_id.0),
        target = %hex::encode(target_pubkey.0),
        new_power,
        actor_power,
        "auto_exec_set_power: skipping — local actor is not admin in this community (deferring to admin race)"
    );
    return Ok(AutoExecOutcome::SkippedNotAdmin);
}
```

Update the existing success path to return `Ok(AutoExecOutcome::Applied)` instead of `Ok(())`.

- [ ] **Step 4: Update production wiring + the tick callback contract**

In `community_voting_tick.rs`:

```rust
// Existing type alias — update return type:
pub type AutoExecSetPowerFn = Arc<
    dyn Fn(
            SpaceId,
            OwnerAddr,
            u32,
        ) -> Pin<Box<dyn Future<Output = Result<crate::community_membership::AutoExecOutcome, String>> + Send>>
        + Send
        + Sync,
>;
```

Update the dispatch match (around line 285-303):

```rust
AutoExecAction::SetPower {
    target_pubkey,
    new_power,
} => {
    stats.tier2_auto_execs_attempted += 1;
    match (ctx.auto_exec_set_power)(cid, *target_pubkey, *new_power).await {
        Ok(crate::community_membership::AutoExecOutcome::Applied) => {
            stats.tier2_auto_execs_succeeded += 1;
        }
        Ok(crate::community_membership::AutoExecOutcome::SkippedNotAdmin) => {
            stats.tier2_auto_execs_skipped_not_admin += 1;
        }
        Err(e) => {
            tracing::warn!(
                community = %hex::encode(cid.0),
                proposal = %hex::encode(pid.0),
                error = %e,
                "auto_exec_set_power failed"
            );
        }
    }
}
```

Add `tier2_auto_execs_skipped_not_admin: u32` to `TickStats`. Match initialization.

In `lib.rs` (around line 3397) update the wiring closure to match the new return type:

```rust
let auto_exec_fn: AutoExecSetPowerFn = {
    let node_state = std::sync::Arc::clone(&node_state);
    Arc::new(move |cid, target, power| {
        let node_state = std::sync::Arc::clone(&node_state);
        Box::pin(async move {
            crate::community_membership::apply_auto_exec_set_power(
                &node_state, cid, target, power,
            )
            .await
        })
    })
};
```

(Note: the wiring already returns whatever `apply_auto_exec_set_power` returns; only the inferred type changes.)

- [ ] **Step 5: Update existing tests to the new contract**

The existing test `apply_auto_exec_set_power_rejects_out_of_range` expects `Err` — unchanged.

The existing test `apply_auto_exec_set_power_missing_handles_returns_err` expects `Err` — unchanged.

The voting_tick test fixture `auto_exec_set_power` closure at line 494 must return `Ok(AutoExecOutcome::Applied)`. Update accordingly.

- [ ] **Step 6: Add positive-path test — admin call mints + applies**

```rust
/// ZEB-297 positive-path companion to the skip test: when the local
/// user IS an admin in the community, `apply_auto_exec_set_power`
/// returns `AutoExecOutcome::Applied` and the SetPower event lands in
/// the engine's local event log. This test pins the boundary condition
/// at `actor_power == POWER_THRESHOLDS.set_power` (100) by minting a
/// SetPower from the bootstrap admin.
#[tokio::test]
async fn apply_auto_exec_set_power_applies_when_local_actor_is_admin() {
    let (node_state, community_id, target) = build_admin_node_state_for_test().await;
    let outcome = apply_auto_exec_set_power(&node_state, community_id, target, 50)
        .await
        .expect("admin call must succeed");
    assert!(matches!(outcome, AutoExecOutcome::Applied));
    let event_log_len = read_membership_event_count(&node_state, &community_id).await;
    assert_eq!(event_log_len, 1, "admin replica must mint exactly one SetPower event");
}
```

- [ ] **Step 7: Add test helpers (`build_admin_node_state_for_test`, `build_non_admin_node_state_for_test`, `read_membership_event_count`)**

These helpers go in the existing `auto_exec_tests` mod. Implementer should follow the existing pattern in `apply_auto_exec_set_power_missing_handles_returns_err` (which already builds a partial NodeState). The bootstrap path:

1. Construct a `NodeState` with `dm_self_owner`, `dm_device_id`, `hlc_tracker`, `dm_outbox`, `community_registry` all populated using deterministic test fixtures.
2. Spawn a `CommunitySyncEngine` with the local user as either admin or non-admin via the engine config's `admin_addr` field.
3. For the admin variant: `admin_addr == self_owner` so the bootstrap rule gives self power 100.
4. For the non-admin variant: `admin_addr != self_owner` so self has default power 0. Local user must still be Joined; either preload a Join event or treat the membership state as "self is a community member with power 0."

Implementer judgment call: if the existing test scaffolding doesn't already support spawning a `CommunitySyncEngine` in unit context, prefer using `community_state_sync::tests::CommunitySyncEngine` test constructors (look for `#[cfg(test)] fn build_test_engine` or similar). Failing that, gate the new tests behind `#[cfg(feature = "test-fixtures")]` to use the existing fixture helpers.

- [ ] **Step 8: Add tick-level test — `SkippedNotAdmin` increments the skip counter, not the success counter**

In `community_voting_tick.rs::tests`:

```rust
#[tokio::test]
async fn community_voting_tick_tier2_finalize_with_non_admin_local_does_not_count_as_succeeded() {
    // ZEB-297: ensure tick stats reflect the new outcome variant.
    // The captured auto_exec callback returns SkippedNotAdmin, so the
    // tick must increment tier2_auto_execs_skipped_not_admin and leave
    // tier2_auto_execs_succeeded unchanged.

    let cid = SpaceId([0xaa; 16]);
    let pid = PollId([0xbb; 32]);
    let auto_exec = AutoExecAction::SetPower {
        target_pubkey: OwnerAddr([0xcc; 16]),
        new_power: 50,
    };
    let mut log = VotingLog::new();
    let mut t2_state = Tier2ProposalState::empty();
    t2_state.threshold_reached_at_ms = Some(0);
    log.polls.insert(
        pid,
        make_tier2_poll(cid, pid, Lifecycle::ThresholdReached, t2_state),
    );
    {
        let state = log.polls.get_mut(&pid).unwrap();
        state.meta.tier_config = Some(make_tier2_config(auto_exec));
    }

    let mut logs = HashMap::new();
    logs.insert(cid, Arc::new(Mutex::new(log)));

    // Build ctx where auto_exec_set_power always returns SkippedNotAdmin.
    let (mut ctx, _events, _captured_auto_exec) = make_ctx_with_logs(logs, 0);
    ctx.auto_exec_set_power = Arc::new(|_, _, _| {
        Box::pin(async {
            Ok(crate::community_membership::AutoExecOutcome::SkippedNotAdmin)
        })
    });

    let now_ms = CONTESTABILITY_WINDOW_MS + 1;
    let stats = run_voting_tick(&ctx, now_ms).await.unwrap();

    assert_eq!(stats.tier2_proposals_finalized, 1);
    assert_eq!(stats.tier2_auto_execs_attempted, 1);
    assert_eq!(stats.tier2_auto_execs_succeeded, 0);
    assert_eq!(stats.tier2_auto_execs_skipped_not_admin, 1);
}
```

- [ ] **Step 9: Run all five gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
npx tsc --noEmit
npx vitest run
# Expected: all five green.
```

- [ ] **Step 10: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
fix(zeb-297): Tier 2 auto-exec mints only on admin replicas

Phase 2's apply_auto_exec_set_power minted a SetPower event signed by
the local user regardless of their power level in the community. On any
non-admin replica, verify_event self-rejected the freshly-minted event
(actor_power < POWER_THRESHOLDS.set_power), so finalization only landed
in materialized state on whichever admin replica's tick ran first. On
every other replica, the SetPower was silently dropped.

Fix: read the engine's materialized power_levels for the local actor at
the top of apply_auto_exec_set_power; if below the admin threshold,
return Ok(AutoExecOutcome::SkippedNotAdmin) without minting. Admins
race to mint, HLC LWW dedupes, and the first admin's SetPower
propagates to every replica via the existing membership log sync.

New AutoExecOutcome enum distinguishes Applied from SkippedNotAdmin so
the voting tick keeps accurate metrics (new
tier2_auto_execs_skipped_not_admin counter). Production wiring in
lib.rs adapts the new return type; tick fixture callbacks updated to
return Ok(Applied).

Unit tests cover both branches: non-admin → Skipped, no event minted;
admin → Applied, exactly one SetPower in the local log. Tick-level
test verifies the skip variant increments the skip counter, not the
success counter.

The full two-engine Zenoh-sync integration test from the acceptance
criteria is entangled with ZEB-298 (voting-log inbound verify_event is
dead code until that ticket lands); deferred there.

Closes ZEB-297

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

Note: the body says "Closes ZEB-297" (not "Resolves") per the `feedback_linear_pr_auto_close` memory rule — this auto-closes ZEB-297 on merge without cascading to the parent ZEB-291.

---

### Task 2: PR creation

**Goal**: Push the branch + open PR with cross-refs.

- [ ] **Step 1: Push branch**

```bash
git push -u origin zeb-297-tier2-autoexec-admin-only-mint
```

- [ ] **Step 2: Create PR**

```bash
gh pr create --title "ZEB-297: Tier 2 auto-exec mints only on admin replicas" --body "$(cat <<'EOF'
## Summary

Closes [ZEB-297](https://linear.app/zeblith/issue/ZEB-297).

Phase 2's `apply_auto_exec_set_power` minted a `SetPower` event regardless of the local user's power level. On any non-admin replica, `verify_event` self-rejected (`actor_power < POWER_THRESHOLDS.set_power`), so a finalized Tier 2 proposal's `AutoExecAction::SetPower` only landed in materialized state on whichever admin replica's tick ran first. CodeRabbit flagged this on PR #131.

**Fix** (Option 2 from the ticket — gate auto-exec to admin replicas):
- Read the engine's materialized `power_levels` for `self_owner` at the top of `apply_auto_exec_set_power`.
- Below admin threshold → return `Ok(AutoExecOutcome::SkippedNotAdmin)` without minting.
- Admin replicas race to mint; HLC LWW dedupes; first admin's `SetPower` propagates via the existing membership log sync.

No wire-format change, no cross-log dependency, no spec change. New `AutoExecOutcome` enum + `tier2_auto_execs_skipped_not_admin` tick counter for accurate metrics.

## Why Option 2 (not 1 or 3)

- **Option 1** (admin co-sign) introduces a new admin-watcher state machine + race-to-broadcast — heavy, contradicts polycentric.
- **Option 3** (consensus-finalized SetPower carrier) requires a new event variant + cross-log dependency in `verify_event` (membership reading voting). Significant architectural lift, deferred to a future design pass if needed.
- **Option 2** is exactly what CodeRabbit endorsed: "gate auto-exec execution to replicas that can actually satisfy the membership verifier."

## Acceptance criteria

| Criterion | Status |
|---|---|
| Auto-exec SetPower lands on every replica via admin race + HLC LWW | ✓ via existing membership log sync |
| No non-admin replica burns CPU minting events that self-reject | ✓ guard at top of helper |
| Two-engine Zenoh-sync integration test | **Deferred to [ZEB-298](https://linear.app/zeblith/issue/ZEB-298)** — voting-log inbound `verify_event` is feature-gated dead code (`community_voting_log_engine.rs:265-272`); B cannot observe finalization via voting-log sync until ZEB-298 lands. Unit tests cover both branches in single-process. |

## Test plan

- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [x] `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- [x] `npx tsc --noEmit`
- [x] `npx vitest run`
- [x] New unit test: `apply_auto_exec_set_power_skips_when_local_actor_is_not_admin`
- [x] New unit test: `apply_auto_exec_set_power_applies_when_local_actor_is_admin`
- [x] New tick test: `community_voting_tick_tier2_finalize_with_non_admin_local_does_not_count_as_succeeded`

## References

- Parent epic: [ZEB-289](https://linear.app/zeblith/issue/ZEB-289)
- Parent phase: [ZEB-291](https://linear.app/zeblith/issue/ZEB-291)
- Related follow-up: [ZEB-298](https://linear.app/zeblith/issue/ZEB-298) (engine-inbound voting verify_event + two-engine integration test)
- Spec: `docs/specs/2026-05-16-zeb-289-voting-polling-design.md` §5

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## Self-Review

**Spec coverage:** Spec §5 auto-exec specifies `AutoExecAction::SetPower` as the v1 auto-exec; this fix preserves that semantic (community decision triggers SetPower mint) but corrects WHO mints. Line 822's `PollExecutionFailed` model is the conceptual sibling — both treat auto-exec as best-effort with admin fallback.

**Placeholder scan:** No TBDs. Test helper function bodies are sketched but the implementer is expected to use existing fixture patterns from `apply_auto_exec_set_power_missing_handles_returns_err` and `auto_exec_set_power_signing_path_produces_verifiable_signature`.

**Type consistency:** `AutoExecOutcome` referenced uniformly across community_membership.rs (defined), community_voting_tick.rs (consumed in match), lib.rs (passed through wiring). New stat field `tier2_auto_execs_skipped_not_admin` mirrors the existing `tier2_auto_execs_succeeded` / `tier2_auto_execs_attempted` naming.
