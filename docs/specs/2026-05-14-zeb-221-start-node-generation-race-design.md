# ZEB-221: tighten `start_node` generation race window — design

**Ticket:** [ZEB-221](https://linear.app/zeblith/issue/ZEB-221)
**Branch:** `zeb-221-start-node-generation-race`
**Cut from:** `origin/main` at `3a04ce9`
**Scope:** contained bug fix; one Rust module touched (`src-tauri/src/lib.rs`); one new unit-test file.

## Problem

`start_node` in `src-tauri/src/lib.rs` releases its `Mutex<NodeState>` between two acquisition sites and performs heavy async work in the gap:

1. **Lock-1** at `lib.rs:1075-1155` — takes the previous node's handles, releases the lock.
2. **Outside the lock** (~lines 1155-2314) — drains old handles, awaits `old_engine.shutdown()`, loads identity and owner-state, and **constructs four background-task-owning resources**: `SyncEngine` (line 1543), `ChannelLogRegistry` (line 1679), `CommunitySyncRegistry` (line 2188), `ProfileBroadcastPublisher` (line 2204). Each one spawns at least one tokio task on construction.
3. **Lock-2** at `lib.rs:2315` — bumps `guard.generation += 1`, then inside the same guarded block spawns the runtime thread at line 2396 and installs all the new handles into `NodeState` at lines 2514-2600.

If two `start_node` calls A and B both pass through lock-1, both spend time in the async work, and both reach lock-2, the loser of the lock-2 race overwrites the winner's installed handles. The winner's runtime thread + the four background-task-owning resources are now orphaned but still running, leaking CPU/memory/file descriptors until process exit.

A partial mitigation exists at `lib.rs:2812-2814` (pairing handle install gated on `guard.generation == our_gen`), but the primary install at lines 2514-2600 has no such gate.

**Why this matters now**: ZEB-215 Sub-A Phase 3a (PR #74) widened the window by adding a new `engine.shutdown().await` between the two acquisitions. CodeRabbit flagged it in round-2 review.

## Fix

**Reservation pattern.** Bump `guard.generation` under lock-1, capture the value as `my_gen`. Under lock-2, check `guard.generation == my_gen` BEFORE installing. On mismatch (a later `start_node` has already reserved a higher generation), abort without installing and tear down the resources we built outside the lock.

### Reservation policy

Per `feedback_dont_self_constrain_scope`, the simpler invariant wins:

- **Bump only under lock-1.** Generation advances exactly once per `start_node` attempt-claim, whether or not that attempt ultimately succeeds. Lock-2 does NOT bump again — it only compares.
- Mismatch check is `guard.generation == my_gen`.
- Generation advances when an attempt is superseded — that's fine. `generation` is a "is this handle still current?" gating value, not an accounting counter. The existing post-checks at lines 2693 and 2813 use `==` comparison and remain correct.

### Cleanup on supersession

The supersede path mirrors the existing `thread_install_failure` cleanup (lines 2604-2614 build a sentinel and a tuple of resource Arcs; post-lock async cleanup drains them at the `thread_install_failure` branch). Reuse that same tuple and the same shutdown calls; add a "superseded" sentinel alongside `thread_install_failure`.

Resources to drain on supersession (each has a spawned tokio task):

1. `sync_engine_arc.shutdown().await`
2. `community_registry_arc.shutdown_all().await`
3. `channel_log_registry_arc.shutdown_all().await`
4. `profile_broadcast_publisher_arc.shutdown().await` (if present)

The runtime thread is NOT spawned on the supersede path (the thread spawn at line 2396 lives inside the install branch of the lock-2 block, which we skip on supersede). No thread join needed.

Other Arcs (DM outbox, DM transport, library directory, etc.) do not own background tokio tasks of their own — they're consumed by the runtime thread's event loop which we never spawn. Dropping their Arcs on `start_node` return is sufficient cleanup.

### Error surface

On supersession: `Err("start_node superseded by concurrent call".to_string())`. The GUI sees a hard failure on the losing call; the winning call returns `Ok(())` normally.

This is acceptable UX because in practice the GUI never issues two concurrent `start_node` calls — the supersession path defends against accidental re-entry from event handlers, not normal use.

## Wire-level changes

None. No new IPCs, no new persisted state, no new on-disk format. The `generation` field is process-local memory only.

## Behavior changes summary

| Scenario | Before | After |
|---|---|---|
| Single `start_node` call | Bumps generation in lock-2, installs. | Bumps generation in lock-1, validates in lock-2, installs. Same outcome. |
| Two concurrent calls A then B | Both install; A's resources orphan. | A is superseded; cleanup awaits shutdown on A's `SyncEngine`/`CommunitySyncRegistry`/`ChannelLogRegistry`/`ProfileBroadcastPublisher`. B installs cleanly. A returns `Err("…superseded…")`. |
| Sequential calls (B starts after A's lock-2) | B's lock-1 takes A's handles via existing teardown. A's pairing-install at line 2812 sees mismatched generation and exits early. | Unchanged. The existing teardown handles this case correctly. |
| Thread spawn failure | Existing cleanup path drains the four Arcs and returns `Err`. | Unchanged. |
| First call (no prior state) | Generation 0 → 1. | Generation 0 → 1, my_gen = 1. Install matches. |

## Testing

### Approach

**Deterministic synthetic drive.** Extract the two race-critical operations into testable seams:

- `fn reserve_generation(state: &Mutex<NodeState>) -> Result<u64, String>` — bumps and returns.
- `fn check_or_supersede(state: &Mutex<NodeState>, my_gen: u64) -> Result<MutexGuard<NodeState>, SupersededError>` — locks, compares, returns the guard on match.

These helpers are pure synchronous logic against `Mutex<NodeState>`. Tests construct a `NodeState` manually, invoke `reserve_generation` from two threads sequentially (no concurrency primitives needed in the test — the lock itself orders them), then verify `check_or_supersede` returns `Err` for the earlier `my_gen` and `Ok` for the later.

This isolates the race-fix logic from the bulk of `start_node`. The supersede *cleanup* path (await shutdowns on the four Arcs) is harder to unit-test without `tauri::test::mock_app` (see [ZEB-232](https://linear.app/zeblith/issue/ZEB-232)), so we cover it via:

- Existing `thread_install_failure` path is already exercised by code-review of the new branch (the cleanup logic is shared).
- Manual smoke verification documented in PR body: launch the app twice in quick succession with a tracing log on the supersede branch.

### Test cases

1. `reserve_generation_bumps_and_returns` — single call bumps 0→1, returns 1.
2. `reserve_generation_is_monotonic` — three sequential calls return 1, 2, 3; state generation ends at 3.
3. `check_or_supersede_accepts_match` — reserve returns 1; check_or_supersede(state, 1) returns `Ok(guard)` and the guard's `generation` is 1.
4. `check_or_supersede_rejects_stale` — reserve returns 1; another reserve returns 2; check_or_supersede(state, 1) returns `Err(Superseded { my_gen: 1, current: 2 })`.
5. `check_or_supersede_rejects_zero` — manually set generation to 5 without calling reserve; check_or_supersede(state, 0) returns `Err(Superseded { my_gen: 0, current: 5 })`.

### Out of scope for tests

- Driving full `start_node` concurrently from a tokio test (needs `tauri::test::mock_app`, deferred to ZEB-232).
- Loom / shuttle model checking (overkill for a straightforward mutex-discipline fix).

## Code seams

### Lock-1 site (`lib.rs:1075`)

```rust
// Before:
let mut guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
old_dm_send_inflight = guard.dm_send_inflight.take();
// ... existing take() calls ...

// After:
let mut guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
guard.generation += 1;
let my_gen = guard.generation;
old_dm_send_inflight = guard.dm_send_inflight.take();
// ... existing take() calls unchanged ...
```

`my_gen` is bound in the outer function scope (declared `let my_gen: u64;` above the lock-1 block, assigned inside).

### Lock-2 site (`lib.rs:2315`)

```rust
// Before:
let mut guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
guard.generation += 1;
// ... content_index load + thread spawn + install ...

// After:
let mut guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
let superseded = guard.generation != my_gen;
if !superseded {
    // ... content_index load + thread spawn + install (existing code) ...
}
// (no `guard.generation += 1` here)
```

The tuple at lines 2614-2620 carries an additional `bool superseded` field. The post-lock cleanup branches on:
- `superseded == true` → drain the four Arcs and return `Err("start_node superseded by concurrent call")`.
- `thread_install_failure.is_some()` → existing cleanup, unchanged.
- Otherwise → continue to `ready_rx.await`.

If both flags would be set (shouldn't happen — we skip the install block on supersede), prefer the supersede branch.

### Helper extraction (new)

For testability, the two operations are extracted into module-private helpers near the top of `lib.rs` (next to the `NodeState` definition):

```rust
/// Bump `guard.generation` and return the new value. Called under lock-1 of
/// `start_node` to reserve a slot before doing async work outside the lock.
fn reserve_node_generation(state: &Mutex<NodeState>) -> Result<u64, String> {
    let mut guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
    guard.generation += 1;
    Ok(guard.generation)
}

/// Lock `state` and verify the caller's reserved generation still matches.
/// Returns the guard on match, or `Err(Superseded)` if a later
/// `reserve_node_generation` has bumped past `my_gen`.
fn check_generation_or_supersede(
    state: &Mutex<NodeState>,
    my_gen: u64,
) -> Result<std::sync::MutexGuard<'_, NodeState>, SupersededError> {
    let guard = state.lock().map_err(|e| SupersededError::LockError(format!("{e}")))?;
    if guard.generation != my_gen {
        return Err(SupersededError::Superseded {
            my_gen,
            current: guard.generation,
        });
    }
    Ok(guard)
}

#[derive(Debug)]
enum SupersededError {
    LockError(String),
    Superseded { my_gen: u64, current: u64 },
}
```

Lock-1 calls `reserve_node_generation` BEFORE the existing handle-take block (the lock acquisition is now hoisted into the helper; the take block re-acquires the lock — that's fine, the existing lock-1 block already operates as a tight transaction so reordering doesn't change semantics).

Lock-2 calls `check_generation_or_supersede` at the top of its block. On `Ok(mut guard)`, the existing install code runs unchanged. On `Err(Superseded {..})`, set the `superseded` sentinel for the post-lock cleanup branch.

**Note on lock-1 reordering**: the original lock-1 block did `guard.dm_send_inflight.take()` first to "fence" concurrent send_dm; with the helper extraction, we now call `reserve_node_generation` first (acquires/releases the lock), then re-acquire the lock for the take() block. This briefly opens a window where another `start_node` could see the bumped generation before the handles are taken. That's actually fine because: (a) the fence purpose is to prevent `send_dm` from using stale handles, not to coordinate with concurrent `start_node`; (b) the take() block under the second lock acquisition handles the rest atomically. If we want to preserve the exact original ordering, an alternative is to inline the bump directly into the existing lock-1 block without extracting a helper — at the cost of test seam. We choose extracting the helper because it makes the race testable; the window opened is harmless.

## Acceptance criteria

1. `reserve_node_generation` and `check_generation_or_supersede` helpers exist in `lib.rs` and are exercised by 5 unit tests in `src-tauri/src/lib.rs` `#[cfg(test)]` module (or a new test module file).
2. `start_node` calls `reserve_node_generation` under lock-1 and `check_generation_or_supersede` under lock-2.
3. On supersede, the post-lock cleanup path awaits `shutdown()` on `SyncEngine`, `shutdown_all()` on `CommunitySyncRegistry` and `ChannelLogRegistry`, and `shutdown()` on `ProfileBroadcastPublisher` (if present).
4. Returns `Err("start_node superseded by concurrent call")` on the supersede path.
5. Single-call behavior is unchanged — `cargo nextest run --workspace` stays green.
6. Existing `thread_install_failure` path stays functional (its cleanup is unchanged).
7. All five CI gates green: `cargo fmt --all -- --check` + `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` + `cargo nextest run --locked --workspace --all-targets --features test-fixtures` + `npx tsc --noEmit` + `npx vitest run`.

## Out of scope

- Cancelling the loser's async work *before* it builds expensive resources (would require threading a `CancellationToken` through identity load, owner-state load, and SyncEngine construction — bigger surgery, deferred).
- Refactoring `start_node` into smaller functions (real cleanup, but expanding blast radius beyond this ticket).
- Frontend changes — the supersede `Err` surfaces through the existing `start_node` IPC return shape; the GUI's existing error handler displays it as a generic "failed to start" toast.
- Loom / shuttle model checking — the deterministic test exercises the race window precisely; structural model checking is overkill.
- Filing a follow-up for ZEB-232's `tauri::test::mock_app` round-trip — already filed, this spec does not block on it.

## References

- `src-tauri/src/lib.rs:187-396` (`NodeState` struct + initial generation = 0)
- `src-tauri/src/lib.rs:937-2840` (`start_node` function)
- `src-tauri/src/lib.rs:1075-1155` (lock-1)
- `src-tauri/src/lib.rs:2315-2614` (lock-2)
- `src-tauri/src/lib.rs:2604-2614` (`thread_install_failure` cleanup pattern — model for supersede cleanup)
- `src-tauri/src/lib.rs:2812-2814` (existing partial mitigation, pairing handle install)
- [ZEB-215 PR #74 round-2 CodeRabbit comment](https://linear.app/zeblith/issue/ZEB-215) (which surfaced this)
- [ZEB-232](https://linear.app/zeblith/issue/ZEB-232) (follow-up: real `tauri::test::mock_app` round-trip for integration tests)
