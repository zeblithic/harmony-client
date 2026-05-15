# ZEB-221 `start_node` Generation Race Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the `start_node` orphan-resource race by reserving the generation under lock-1 and validating under lock-2; on supersede, await shutdown on the four background-task-owning resources and return `Err`.

**Architecture:** Extract two synchronous helpers (`reserve_node_generation`, `check_generation_or_supersede`) for deterministic unit tests. Wire them into `start_node`'s existing two-lock structure. Reuse the existing `thread_install_failure` cleanup path for supersede.

**Tech Stack:** Rust 2021, `std::sync::Mutex<NodeState>`, `tokio` for async shutdown awaits, `cargo-nextest`.

---

## Task 0: Pre-flight + green baseline

**Goal:** Confirm working tree is clean on the `zeb-221-start-node-generation-race` branch, with the spec commit (`4f64a75`) at HEAD~1 and this plan at HEAD. Verify the five CI gates are all green on the just-cut branch BEFORE any code changes.

**Files:** None — read-only verification.

- [ ] **Step 0.1: Verify branch + lineage**

```bash
git status
git log --oneline -5
git rev-parse HEAD
git rev-parse origin/main
```

Expected:
- `On branch zeb-221-start-node-generation-race`
- Working tree clean
- HEAD = plan-doc commit (this file)
- HEAD~1 = spec-doc commit `4f64a75`
- HEAD~2 = `3a04ce9` (ZEB-213 merge)
- `origin/main` = `3a04ce9`

- [ ] **Step 0.2: Five-gate baseline**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

```bash
# from repo root
npx tsc --noEmit
npx vitest run
```

Expected: all five gates green. If any fails: STOP and surface the failure — that's pre-existing breakage on main, not for this PR. Per `feedback_test_drift_is_our_fault` memory rule, file a follow-up ticket and surface; do NOT fold the fix into ZEB-221.

**No commit.**

---

## Task 1: Extract helpers + add unit tests

**Goal:** Introduce two synchronous helpers + a `SupersededError` enum at the top of `lib.rs`. Add 5 unit tests in a `#[cfg(test)]` module. The helpers are NOT yet called from `start_node` — that's Task 2.

**Files:**
- Modify: `src-tauri/src/lib.rs` (add helpers + `#[cfg(test)] mod start_node_race_tests`)

- [ ] **Step 1.1: Write the failing tests FIRST**

Add a `#[cfg(test)] mod start_node_race_tests` block at the very bottom of `src-tauri/src/lib.rs` (after the final `}` of the existing top-level module, before any existing `#[cfg(test)]` block, OR inside the existing one — verify by `grep -n '^#\[cfg(test)\]' src-tauri/src/lib.rs` and use the first existing site, or create a new one if none).

Test code (paste verbatim — these tests reference the helpers + `SupersededError` enum which will be defined in Step 1.2):

```rust
#[cfg(test)]
mod start_node_race_tests {
    use super::*;
    use std::sync::Mutex;

    /// Build a minimal `NodeState` for race-helper tests. Only `generation`
    /// is meaningful; everything else is default / None / empty.
    fn fresh_node_state() -> Mutex<NodeState> {
        Mutex::new(NodeState {
            thread: None,
            shutdown_tx: None,
            publish_tx: None,
            fetch_tx: None,
            ingest_tx: None,
            content_verb_tx: None,
            follow_tx: None,
            voice_tx: None,
            voice_channel_tx: None,
            follow_mgr: None,
            followed_set: None,
            vine_feed_cache: None,
            mail_mgr: None,
            mail_sync: None,
            content_index: std::sync::Arc::new(std::sync::Mutex::new(
                content_index::ContentIndex::new(),
            )),
            generation: 0,
            node_addr: String::new(),
            pairing_handle: None,
            sync_engine: None,
            community_registry: None,
            community_delta_tx: None,
            dm_outbox: None,
            dm_transport: None,
            crdt_state: None,
            hlc_tracker: None,
            dm_device_id: None,
            dm_self_owner: None,
            content_store: None,
            unicast_send_tx: None,
            dm_send_inflight: None,
            dm_send_stopping: None,
            dm_identity_pub_64: None,
            community_adapter_request_tx: None,
            channel_log_registry: None,
            library_directory: None,
            profile_broadcast_publisher: None,
            profile_broadcast_cache: None,
            profile_broadcast_request_tx: None,
            profile_broadcast_next_subscription_id: std::sync::Arc::new(
                std::sync::atomic::AtomicU64::new(1),
            ),
        })
    }

    #[test]
    fn reserve_generation_bumps_and_returns() {
        let state = fresh_node_state();
        let n = reserve_node_generation(&state).expect("reserve");
        assert_eq!(n, 1);
        assert_eq!(state.lock().unwrap().generation, 1);
    }

    #[test]
    fn reserve_generation_is_monotonic() {
        let state = fresh_node_state();
        assert_eq!(reserve_node_generation(&state).unwrap(), 1);
        assert_eq!(reserve_node_generation(&state).unwrap(), 2);
        assert_eq!(reserve_node_generation(&state).unwrap(), 3);
        assert_eq!(state.lock().unwrap().generation, 3);
    }

    #[test]
    fn check_or_supersede_accepts_match() {
        let state = fresh_node_state();
        let my_gen = reserve_node_generation(&state).unwrap();
        let guard = check_generation_or_supersede(&state, my_gen)
            .expect("should accept matching generation");
        assert_eq!(guard.generation, my_gen);
    }

    #[test]
    fn check_or_supersede_rejects_stale() {
        let state = fresh_node_state();
        let my_gen = reserve_node_generation(&state).unwrap();
        let _later = reserve_node_generation(&state).unwrap();
        let err = check_generation_or_supersede(&state, my_gen)
            .expect_err("stale my_gen must be superseded");
        match err {
            SupersededError::Superseded { my_gen: g, current } => {
                assert_eq!(g, 1);
                assert_eq!(current, 2);
            }
            other => panic!("unexpected error: {:?}", other),
        }
    }

    #[test]
    fn check_or_supersede_rejects_zero_when_generation_advanced() {
        let state = fresh_node_state();
        // simulate prior reservations without calling our helper
        state.lock().unwrap().generation = 5;
        let err = check_generation_or_supersede(&state, 0)
            .expect_err("my_gen=0 against generation=5 must be superseded");
        match err {
            SupersededError::Superseded { my_gen: 0, current: 5 } => {}
            other => panic!("unexpected error: {:?}", other),
        }
    }
}
```

If the existing lib.rs ALREADY has a `#[cfg(test)] mod` near the bottom that you can extend instead of creating a new one, do that and put the new tests + `fresh_node_state` helper inside it. Otherwise add a new test module as shown. The test names must remain exactly as written.

- [ ] **Step 1.2: Run the tests to verify they fail (compile errors)**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(start_node_race_tests)'
```

Expected: compile errors on `reserve_node_generation`, `check_generation_or_supersede`, `SupersededError` (these don't exist yet). Five test cases listed but not run.

- [ ] **Step 1.3: Implement the helpers**

Add this block to `src-tauri/src/lib.rs` near the top — after the `NodeState` struct definition (which ends around line 396) and BEFORE the next major function. Pick a spot just before `fn start_node`. Use this exact code:

```rust
/// Bump `state.generation` and return the new value.
///
/// Called under lock-1 of `start_node` to reserve a generation slot
/// BEFORE doing any async work outside the lock. The reserved value is
/// later validated under lock-2 via [`check_generation_or_supersede`]
/// so a concurrent `start_node` cannot orphan our spawned resources.
///
/// See [ZEB-221](https://linear.app/zeblith/issue/ZEB-221) for the
/// full race analysis.
fn reserve_node_generation(state: &Mutex<NodeState>) -> Result<u64, String> {
    let mut guard = state
        .lock()
        .map_err(|e| format!("reserve_node_generation lock error: {e}"))?;
    guard.generation += 1;
    Ok(guard.generation)
}

/// Lock `state` and verify the caller's reserved generation still matches.
///
/// Returns the guard on match. Returns
/// [`SupersededError::Superseded`] if a later
/// [`reserve_node_generation`] has bumped past `my_gen`, indicating a
/// concurrent `start_node` has reserved a higher generation slot and
/// the caller must abort + clean up the resources it built outside the
/// lock.
fn check_generation_or_supersede(
    state: &Mutex<NodeState>,
    my_gen: u64,
) -> Result<std::sync::MutexGuard<'_, NodeState>, SupersededError> {
    let guard = state
        .lock()
        .map_err(|e| SupersededError::LockError(format!("{e}")))?;
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

impl std::fmt::Display for SupersededError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SupersededError::LockError(msg) => write!(f, "node-state lock error: {msg}"),
            SupersededError::Superseded { my_gen, current } => write!(
                f,
                "start_node superseded by concurrent call (my_gen={my_gen}, current={current})"
            ),
        }
    }
}
```

If the surrounding file uses a different import convention for `Mutex` (e.g. `use std::sync::Mutex;` at the top vs. fully-qualified), match the surrounding style. The `Mutex<NodeState>` parameter should resolve the same way `start_node` resolves it.

- [ ] **Step 1.4: Run tests to verify they pass**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(start_node_race_tests)'
```

Expected: all 5 tests pass.

- [ ] **Step 1.5: Run full nextest sweep to confirm no regression**

```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: green. The existing test count should grow by exactly 5.

- [ ] **Step 1.6: Run the three companion gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```

Expected: green. If clippy flags the new code, fix in-place (typical: `needless_return`, doc-comment style, `missing_docs_in_private_items` — match the surrounding file's lint discipline).

- [ ] **Step 1.7: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
test(zeb-221): start_node generation reservation helpers + unit tests

Adds reserve_node_generation + check_generation_or_supersede + a
SupersededError enum at the top of lib.rs. Five unit tests pin the
reservation/check semantics under a fresh NodeState. Helpers are NOT yet
called from start_node — Task 2 wires them in.
EOF
)"
```

---

## Task 2: Wire helpers into `start_node` + supersede cleanup branch

**Goal:** Replace the inline `guard.generation += 1` at `lib.rs:2316` with a `check_generation_or_supersede` call at the top of the lock-2 block. Hoist a `reserve_node_generation` call to the top of the lock-1 block (replacing the inline bump that doesn't currently exist there). On supersede, route through the existing cleanup path that already drains `sync_engine_arc` / `community_registry_arc` / `channel_log_registry_arc` / `profile_broadcast_publisher_arc`.

**Files:**
- Modify: `src-tauri/src/lib.rs:1075-1155` (lock-1, add bump)
- Modify: `src-tauri/src/lib.rs:2315-2622` (lock-2, replace inline bump with check; route supersede through cleanup)
- Modify: `src-tauri/src/lib.rs:2623-2673` (post-lock cleanup, extend trigger to `superseded`)

- [ ] **Step 2.1: Add `my_gen` reservation under lock-1**

Find `lib.rs:1075`:

```rust
let mut guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
```

Just AFTER this line and BEFORE the existing `old_dm_send_inflight = guard.dm_send_inflight.take();` (line 1079), insert:

```rust
        // ZEB-221: reserve our generation slot under lock-1 so a concurrent
        // start_node cannot install over us in lock-2 after we've spent
        // wall-clock building SyncEngine / CommunitySyncRegistry /
        // ChannelLogRegistry / ProfileBroadcastPublisher outside the lock.
        // The matching `check_generation_or_supersede` call at the top of
        // the lock-2 block validates this reservation; mismatch routes
        // through the existing thread_install_failure cleanup path.
        guard.generation += 1;
        let my_gen = guard.generation;
```

Match the surrounding indentation (8 spaces inside the destructuring block — verify by reading the surrounding lines).

The `my_gen` binding must escape the lock-1 scope. Confirm by checking: the existing tuple destructure at `lib.rs:1052-1116` returns `(old_shutdown, old_thread, …, old_unicast_send_tx)`. The `my_gen` value is captured INSIDE the same block scope but is bound at the OUTER function level so it's reachable by the lock-2 code. Achieve this by declaring `let my_gen: u64;` ABOVE the lock-1 block (e.g. just before `let old_dm_send_inflight: Option<...>;` at line 1050) and assigning to it inside the lock. Pseudo-pattern:

```rust
        let my_gen: u64;
        // ... existing outer-scope `let old_xxx: Option<...>;` declarations ...
        let (...tuple...) = {
            let mut guard = state.lock()...;
            guard.generation += 1;
            my_gen = guard.generation;
            // existing take() calls ...
            (...)
        };
```

If the existing structure has the outer-scope `let` declarations in a specific order, place `my_gen` next to them.

- [ ] **Step 2.2: Replace the inline bump at lock-2 with `check_generation_or_supersede`**

Find `lib.rs:2315-2316`:

```rust
        let mut guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
        guard.generation += 1;
```

Replace with:

```rust
        // ZEB-221: validate our lock-1 reservation. If a later start_node
        // has bumped past `my_gen` while we were building SyncEngine et al.
        // outside the lock, set the `superseded` sentinel and skip the
        // install block; post-lock cleanup will await shutdown on each of
        // the four background-task-owning Arcs.
        let mut guard;
        let superseded;
        match check_generation_or_supersede(&state, my_gen) {
            Ok(g) => {
                guard = g;
                superseded = false;
            }
            Err(SupersededError::Superseded { .. }) => {
                // Re-acquire the lock (we still need a guard for the
                // tuple-return below, even though we'll skip the install
                // block). The check_generation_or_supersede helper consumed
                // its lock acquisition on the Err path.
                guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
                superseded = true;
            }
            Err(SupersededError::LockError(msg)) => {
                return Err(msg);
            }
        }
```

- [ ] **Step 2.3: Skip the install block on supersede**

The existing code at `lib.rs:2330-2607` is "load content_index → spawn thread → install handles into guard". On supersede we must skip ALL of this so no thread is spawned and no handles are installed.

Wrap the existing block (from line 2330 `let content_index = ...` through the closing of the `match thread_result { ... }` at line 2607) in `if !superseded { ... }`. The `thread_install_failure: Option<String>` declared at line 2511 must still be initialized — declare it as `None` at the top of the `if !superseded` block, or move its declaration to before the `if !superseded` (initialized to `None`) so it's available either way.

Approach (cleaner): declare `let mut thread_install_failure: Option<String> = None;` BEFORE the `if !superseded { ... }` block. Inside the `if`, the existing `match thread_result` arms set it. Outside, on supersede, it stays `None` (the supersede branch will use a separate signal — `superseded` itself — for cleanup-triggering).

```rust
        let mut thread_install_failure: Option<String> = None;
        if !superseded {
            // ... existing code from line 2330 through line 2607 ...
        }
```

- [ ] **Step 2.4: Extend the tuple-return to carry `superseded`**

Find the tuple at `lib.rs:2614-2621`:

```rust
        (
            guard.generation,
            thread_install_failure,
            sync_engine_arc.clone(),
            community_registry_arc.clone(),
            channel_log_registry_arc.clone(),
            profile_broadcast_publisher_arc.clone(),
        )
```

Extend to include `superseded`:

```rust
        (
            guard.generation,
            thread_install_failure,
            superseded,
            sync_engine_arc.clone(),
            community_registry_arc.clone(),
            channel_log_registry_arc.clone(),
            profile_broadcast_publisher_arc.clone(),
        )
```

- [ ] **Step 2.5: Extend the destructure at line 2623**

Find:

```rust
    let (
        our_gen,
        thread_spawn_failure,
        engine_for_cleanup,
        registry_for_cleanup,
        channel_log_registry_for_cleanup,
        profile_broadcast_publisher_for_cleanup,
    ) = our_gen;
```

(Note the variable shadowing — outer `our_gen` is the tuple, inner `our_gen` is the u64.)

Extend to:

```rust
    let (
        our_gen,
        thread_spawn_failure,
        superseded,
        engine_for_cleanup,
        registry_for_cleanup,
        channel_log_registry_for_cleanup,
        profile_broadcast_publisher_for_cleanup,
    ) = our_gen;
```

- [ ] **Step 2.6: Trigger cleanup on supersede**

Find the existing cleanup branch at `lib.rs:2632`:

```rust
    if let Some(msg) = thread_spawn_failure {
        // ...existing 4-Arc cleanup...
        return Err(msg);
    }
```

Refactor to handle BOTH supersede and thread spawn failure via the same cleanup. Choose ONE of the following equally-acceptable shapes:

**Option A (explicit branches, recommended):**

```rust
    // ZEB-221 + thread-spawn-failure cleanup: both paths require the same
    // shutdown-then-drop sequence on the four background-task-owning Arcs
    // built outside the lock. Branch on which trigger fired (only one will
    // be set; if both somehow fire, the supersede message wins because it
    // names the more specific cause).
    let cleanup_msg: Option<String> = if superseded {
        Some("start_node superseded by concurrent call".to_string())
    } else {
        thread_spawn_failure
    };
    if let Some(msg) = cleanup_msg {
        // ZEB-281 Sub-D Phase 4: abort the profile-broadcast publisher
        // FIRST — its background task holds a clone of `publish_tx`
        // (now orphaned because the runtime thread never spawned), so
        // aborting it deterministically releases the clone before the
        // other registries shut down.
        if let Some(publisher) = profile_broadcast_publisher_for_cleanup {
            publisher.shutdown().await;
        }
        // ZEB-270 Phase 3 Task 4.5: shutdown the channel-log registry
        // FIRST so each per-channel engine's final flush completes
        // before the per-community state engines (which back the
        // verify chain) tear down. Mirrors stop_inner's ordering.
        if let Some(registry) = channel_log_registry_for_cleanup {
            if let Err(e) = registry.shutdown_all().await {
                tracing::error!(
                    error = %e,
                    "ChannelLogRegistry cleanup after start_node failure"
                );
            }
        }
        // ZEB-217 Sub-C Phase 2: shutdown the registry FIRST so each
        // community engine's final flush completes before the owner
        // SyncEngine tears down. Mirrors stop_inner's ordering.
        if let Some(registry) = registry_for_cleanup {
            if let Err(e) = registry.shutdown_all().await {
                tracing::error!(
                    error = %e,
                    "CommunitySyncRegistry cleanup after start_node failure"
                );
            }
        }
        if let Some(engine) = engine_for_cleanup {
            if let Err(e) = engine.shutdown().await {
                tracing::error!(
                    error = %e,
                    "SyncEngine cleanup after start_node failure"
                );
            }
        }
        return Err(msg);
    }
```

Comment text in the four shutdown sites was updated from "after runtime-thread spawn failure" → "after start_node failure" to reflect the broader trigger. (Implementer: do an exact-string find/replace on those four call sites; preserve indentation.)

**Option B (route superseded through the existing thread_spawn_failure)**: if implementer prefers minimal diff, just do `let thread_spawn_failure = if superseded { Some("start_node superseded by concurrent call".to_string()) } else { thread_spawn_failure };` immediately after the destructure at Step 2.5. Then the existing `if let Some(msg) = thread_spawn_failure { ... }` block runs unchanged. Tradeoff: the variable name lies about its semantics but the diff is smaller.

Pick whichever shape is cleaner against the actual file. Both are correct; spec compliance is satisfied either way.

- [ ] **Step 2.7: Compile + run the targeted tests**

```bash
cd src-tauri && cargo check --locked --features test-fixtures
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(start_node_race_tests)'
```

Expected: compiles clean (warnings OK at this step, fix in next step); all 5 unit tests still pass.

If you get "cannot find value `my_gen`" or scope errors, re-check Step 2.1's outer-scope `let my_gen: u64;` declaration site.

- [ ] **Step 2.8: Run the full nextest sweep**

```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: green. Total test count = baseline + 5 (from Task 1).

If a previously-green test now fails, the supersede branch is being triggered incorrectly. Most likely root cause: `my_gen` is not being captured at the right scope, or the `if !superseded { ... }` wrap is too tight (excluding code that must run on the happy path).

- [ ] **Step 2.9: Run clippy + fmt**

```bash
cd src-tauri && cargo fmt --all
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```

Fix in-place if clippy complains (typical: `single_match`, `needless_borrow`, doc-comment style).

- [ ] **Step 2.10: Run the frontend gates (sanity — should not be affected)**

```bash
# from repo root
npx tsc --noEmit
npx vitest run
```

Expected: green. No frontend code touched, but run these to confirm no surprising shared-state break.

- [ ] **Step 2.11: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
fix(zeb-221): tighten start_node generation race window

Bumps generation under lock-1 (was: lock-2), capturing my_gen. Under
lock-2, validates `guard.generation == my_gen` before installing. On
mismatch, sets a `superseded` sentinel; the post-lock cleanup branch
awaits shutdown on the four background-task-owning Arcs (SyncEngine,
CommunitySyncRegistry, ChannelLogRegistry, ProfileBroadcastPublisher)
and returns Err("start_node superseded by concurrent call"). Reuses
the existing thread_install_failure cleanup pattern — same shutdown
ordering, broader trigger.

Single-call behavior unchanged. Concurrent start_node calls now lose
deterministically with no orphan tokio tasks.
EOF
)"
```

---

## Task 3: Final 5-gate sweep + push + PR

**Goal:** Verify all five CI gates pass on the final commit. Push the branch and open the PR with markdown-linked Linear refs.

**Files:** None modified (verification + push).

- [ ] **Step 3.1: Five-gate sweep**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

```bash
# from repo root
npx tsc --noEmit
npx vitest run
```

All five must be green. If any fails, fix in-place and create a fixup commit (do NOT amend — per memory rules, prefer new commits over amending).

- [ ] **Step 3.2: Push the branch**

```bash
git push -u origin zeb-221-start-node-generation-race
```

- [ ] **Step 3.3: Open the PR**

```bash
gh pr create --title "ZEB-221: tighten start_node generation race window" --body "$(cat <<'EOF'
## Summary

Fixes the `start_node` orphan-resource race ([ZEB-221](https://linear.app/zeblith/issue/ZEB-221)). When two `start_node` calls overlap, the loser's `SyncEngine`, `CommunitySyncRegistry`, `ChannelLogRegistry`, and `ProfileBroadcastPublisher` — each with a spawned tokio task — would orphan and leak CPU/memory until process exit.

**Fix:** reservation pattern. Bump `guard.generation` under lock-1, capture as `my_gen`. Under lock-2, check `guard.generation == my_gen` before installing. On mismatch, skip the install block, await shutdown on the four background-task-owning Arcs, and return `Err("start_node superseded by concurrent call")`. Reuses the existing `thread_install_failure` cleanup path with a broader trigger.

The partial mitigation at `lib.rs:2812-2814` (pairing handle install) stays — it remains correct under the new scheme because `our_gen == my_gen` is preserved.

## Design

See [`docs/specs/2026-05-14-zeb-221-start-node-generation-race-design.md`](./docs/specs/2026-05-14-zeb-221-start-node-generation-race-design.md) (commit `4f64a75`).

## Tests

5 new deterministic unit tests in `src-tauri/src/lib.rs` `start_node_race_tests` mod:
- `reserve_generation_bumps_and_returns`
- `reserve_generation_is_monotonic`
- `check_or_supersede_accepts_match`
- `check_or_supersede_rejects_stale`
- `check_or_supersede_rejects_zero_when_generation_advanced`

The supersede *cleanup* path (await shutdowns) shares its logic with the existing `thread_install_failure` branch — that branch is already exercised by code review of the cleanup ordering comments. Full integration-test coverage requires `tauri::test::mock_app` round-trip (tracked as [ZEB-232](https://linear.app/zeblith/issue/ZEB-232)).

## Test plan

- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [x] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (+5 tests)
- [x] `npx tsc --noEmit`
- [x] `npx vitest run`
- [ ] Manual smoke: launch the app twice in quick succession; verify the second start logs the supersede path or wins cleanly (depending on timing) with no orphan threads (`Activity Monitor` or `ps -L` shows expected thread count).

## Related

- Surfaced during [ZEB-215](https://linear.app/zeblith/issue/ZEB-215) Sub-A Phase 3a PR #74 review (CodeRabbit)
- Follow-up: [ZEB-232](https://linear.app/zeblith/issue/ZEB-232) — full `tauri::test::mock_app` round-trip will allow concurrent-`start_node` integration tests

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3.4: Capture PR URL + report**

After `gh pr create` returns, save the PR URL. Report:
- Commits on branch (should be 3: spec, plan, Task 1 commit, Task 2 commit — actually 4 total)
- All five gates green
- PR URL

Hand control back to the calling agent. Per the `feedback_autonomous_pr_monitoring_loop` memory rule, the calling agent enters the autonomous bot-review monitoring loop (CodeRabbit, Cursor, CodeAnt, Qodo — NOT Greptile, NOT CI) and sends a pushover when the PR converges + becomes mergeable.

---

## Self-review

After completing all three tasks, do a final read-through of the diff:

```bash
git diff origin/main..HEAD
```

Check:
1. **Spec coverage:** Every acceptance criterion from the spec corresponds to a code change or test. The 5 unit tests cover the 5 listed test cases. The supersede cleanup awaits all four shutdowns. The Err message matches.
2. **Single-call regression:** No path that succeeds for a single `start_node` was changed semantically. Lock ordering is preserved. Generation still advances exactly once per successful `start_node` (just under a different lock).
3. **Helper isolation:** The two new helpers are private (`fn`, not `pub fn`) and only called from `start_node` + the test module. No accidental public-API expansion.
4. **No accidental scope creep:** Only `lib.rs` is modified. No frontend files. No new modules. No new IPCs.
5. **Memory-rule compliance:** No worktree, no Linear ID invented, no CI bypass, branch on `origin/main` lineage. Pull-before-work satisfied.
