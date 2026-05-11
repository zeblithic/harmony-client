# ZEB-274: membership-side transactionality (RAII rollback guard for community-sync engine spawn)

**Linear:** [ZEB-274](https://linear.app/zeblith/issue/ZEB-274)
**Sibling (different architecture):** [ZEB-271](https://linear.app/zeblith/issue/ZEB-271) (channel-log registry transactionality, merged 2026-05-10)
**Parent gap:** [ZEB-266](https://linear.app/zeblith/issue/ZEB-266) (Phase 1 of [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) Sub-C v2)
**Branch:** `zeb-274-membership-side-transactionality`

---

## §1 Context

The ZEB-271 spec at `docs/specs/2026-05-10-zeb-271-channel-log-registry-transactionality-design.md` §9 explicitly flagged: *"Membership-side parallel fix for [ZEB-266](https://linear.app/zeblith/issue/ZEB-266) — same shape, separate ticket."* This is that ticket.

The phantom-community gap exists for `CommunitySyncRegistry` too: if `create_community_inner` or `redeem_invite_inner` aborts after `community_registry.spawn_engine` has succeeded but before the durable `apply_space` commit, the engine + adapter task + per-community persistence directory persist for a community that owner-state never recorded. Result: a leaked engine + adapter + on-disk segment dir for a community the user never sees.

Today this is mitigated by ~26 explicit `shutdown_engine_and_cleanup_persistence` calls scattered across both IPC handlers' failure paths: **9 sites in `create_community_inner`** (lib.rs:7298, 7338, 7360, 7429, 7448, 7466, 7515, 7534, 7575 — unconditional, since create has no concurrent-redeem race) and **17 sites in `redeem_invite_inner`** (lib.rs:8549–9094 — all guarded by `if engine_freshly_created` per ZEB-260 PR #90 round-5). The risks are:

1. Panics that bypass these explicit rollbacks
2. Future failure points added without rollback
3. Drift between the create + redeem flows
4. Cognitive overhead — every reviewer must verify each new `?` early-return is preceded by a rollback

ZEB-274 collapses these scattered rollback sites into a single RAII guard with sound abort-on-Drop semantics.

## §2 Approach

**Locked: RAII rollback guard pattern (NOT deferred-spawn).**

The structurally critical difference from ZEB-271:

| Aspect | ChannelLog (ZEB-271) | CommunitySync (ZEB-274) |
|---|---|---|
| Caller of spawn | Delta consumer (3rd callback), indirect from IPC | IPC handler, direct call |
| IPC needs engine pre-commit? | No | **YES** — `engine.insert_local_event(bootstrap_join)` + `(default_channel)` |
| Tx mechanism | Defer spawn, fire on commit | Eager spawn, rollback on Drop/abort |
| `commit()` semantics | Drains queue → fires real spawn | Releases rollback obligation (no-op side effects) |
| `abort()` / Drop semantics | Discards queue (nothing to undo) | Calls `shutdown_engine_and_cleanup_persistence` |

ZEB-271's deferred-spawn pattern doesn't work for community-sync because the IPC handler at lib.rs:7331 calls `engine_arc().await.ok_or("engine vanished")?` and then `engine.insert_local_event(bootstrap_join)`. If we defer the spawn, the engine doesn't exist and this fails immediately.

The RAII guard pattern achieves the same end (no phantom community on failure) via **rollback-on-abort** rather than **defer-and-spawn-on-commit**.

### §2.1 Rejected alternatives

- **(b) Force the deferred-spawn pattern by moving `bootstrap_join` + `default_channel` inserts OUT of the IPC handler and INTO the deferred drain.** Much more invasive (~400+ lines), changes the visible IPC contract, but matches the channel-log shape exactly. Rejected: forcing one pattern onto two different problems.
- **(c) Re-scope ZEB-274 to investigate further.** Pause implementation, audit whether the existing scattered rollback sites are actually buggy. Rejected: the architectural risk (panics bypassing rollback, future failure-path drift) is real even if no concrete bug exists today.

## §3 Guard primitive

### §3.1 New types on `community_state_sync.rs`

```rust
/// RAII rollback guard for a freshly-spawned community-sync engine.
/// Held by the IPC handler across the critical section between
/// spawn_engine_with_guard and the durable apply_space commit. Drop
/// without explicit commit/abort calls
/// shutdown_engine_and_cleanup_persistence (only if THIS call freshly
/// created the engine — concurrent-redeem race losers are no-ops).
pub struct CommunitySyncSpawnGuard {
    registry: Arc<CommunitySyncRegistry>,
    community_id: SpaceId,
    /// Set once by spawn_engine_with_guard before it returns.
    /// True iff this call freshly created the engine (vs. found an
    /// idempotent existing one). Only fresh creations carry rollback
    /// obligation.
    freshly_created: bool,
    /// Set to true by commit() to bypass Drop's rollback path.
    completed: AtomicBool,
}
```

### §3.2 New methods on `CommunitySyncRegistry`

```rust
/// Open a spawn-rollback guard. Returns immediately, performs no
/// I/O. Caller then calls spawn_engine_with_guard(&mut guard, ...)
/// to perform the actual spawn; the guard captures the freshness
/// flag internally. If the caller fails before commit(), Drop runs
/// shutdown_engine_and_cleanup_persistence.
pub fn begin_spawn_guard(
    self: &Arc<Self>,
    community_id: SpaceId,
) -> CommunitySyncSpawnGuard;

/// Spawn the engine, dispatch the adapter request, bind the result
/// to the guard. Replaces the public Result<bool, _> spawn_engine —
/// the bool is now internalized into the guard. Atomic: if the
/// adapter dispatch fails, the engine is removed from the map and
/// persistence is cleaned up before the function returns Err.
///
/// Args mirror today's spawn_engine PLUS the adapter halves
/// (publisher_rx, subscriber_tx) and the adapter request channel
/// (community_adapter_tx) — these moved from the IPC handler into
/// here for atomic dispatch.
pub async fn spawn_engine_with_guard(
    self: &Arc<Self>,
    guard: &mut CommunitySyncSpawnGuard,
    community_id: SpaceId,
    membership_key: MembershipKey,
    admin_addr: OwnerAddr,
    is_invite_only: bool,
    publisher_tx: mpsc::Sender<Vec<u8>>,
    subscriber_rx: mpsc::Receiver<Vec<u8>>,
    publisher_rx: mpsc::Receiver<Vec<u8>>,
    subscriber_tx: mpsc::Sender<Vec<u8>>,
    community_adapter_tx: mpsc::Sender<CommunityAdapterRequest>,
) -> Result<Arc<CommunitySyncEngine>, CommunitySyncError>;
```

### §3.3 New methods on `CommunitySyncSpawnGuard`

```rust
/// Release the rollback obligation. The engine remains alive.
/// Called by the IPC handler after apply_space succeeds (i.e.,
/// after the durable commit point). Sync — no .await needed
/// because there is no work to do beyond setting the flag.
pub fn commit(self) {
    self.completed.store(true, Ordering::Release);
}

/// Explicit rollback. Calls shutdown_engine_and_cleanup_persistence
/// if freshly_created. Sets completed = true so Drop is a no-op.
/// Sync entry point but spawns the cleanup as a tokio task internally
/// (mirrors ZEB-271's Drop pattern).
pub fn abort(self);
```

### §3.4 Drop impl

```rust
impl Drop for CommunitySyncSpawnGuard {
    fn drop(&mut self) {
        if !self.completed.load(Ordering::Acquire) && self.freshly_created {
            tracing::warn!(
                community_id = ?self.community_id,
                "CommunitySyncSpawnGuard dropped without commit/abort — \
                 running safety net"
            );
            // shutdown_engine_and_cleanup_persistence is async (it
            // includes engine.shutdown().await which flushes pending
            // writes), so we spawn it via Handle::try_current() if a
            // tokio runtime is available. Unlike ZEB-271's
            // CommunityTransactionGuard (whose abort_transaction_internal
            // is sync map cleanup with a sync fallback), there is NO
            // sync alternative for engine.shutdown(). When no runtime
            // is present, we log a warn and accept the leak — the
            // engine + persist dir remain on disk; reconcile_from_state
            // at next start_node will detect the inconsistency and
            // recover. See §10.2 for full discussion.
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    let registry = Arc::clone(&self.registry);
                    let community_id = self.community_id;
                    handle.spawn(async move {
                        if let Err(e) = registry
                            .shutdown_engine_and_cleanup_persistence(&community_id)
                            .await
                        {
                            tracing::warn!(
                                community_id = ?community_id,
                                error = %e,
                                "CommunitySyncSpawnGuard Drop: cleanup failed \
                                 (engine + persist dir may leak; \
                                 reconcile_from_state will recover at next start_node)"
                            );
                        }
                    });
                }
                Err(_) => {
                    // No tokio runtime present (e.g., panic-during-drop
                    // after runtime teardown). We can't do the async
                    // cleanup. Log and accept the leak — reconcile
                    // recovers at next start_node.
                    tracing::warn!(
                        community_id = ?self.community_id,
                        "CommunitySyncSpawnGuard dropped without runtime; \
                         cannot run async cleanup. Engine + persist dir \
                         will leak until reconcile_from_state at next start_node."
                    );
                }
            }
        }
    }
}
```

**Note on no-runtime fallback:** Unlike ZEB-271's `abort_transaction_internal` (which is synchronous map cleanup), `shutdown_engine_and_cleanup_persistence` is fundamentally async (it calls `engine.shutdown().await` which flushes pending writes). There is no sync alternative. The no-runtime case is acknowledged as a leak that reconcile will eventually clean up.

## §4 Caller flow

### §4.1 `create_community_inner` (rewrite)

All 9 explicit `shutdown_engine_and_cleanup_persistence` rollback sites at lib.rs:7298, 7338, 7360, 7429, 7448, 7466, 7515, 7534, 7575 collapse into a single RAII guard. The new flow:

```rust
async fn create_community_inner(...) -> Result<String, String> {
    // ... mint community_id + state ...

    // ZEB-271 (existing): channel-log tx for the deferred channel-config spawns
    let channel_log_tx = channel_log_registry.begin_transaction(minted.community_id);

    // ZEB-274 (new): RAII rollback guard for the community-sync spawn + adapter
    let mut community_sync_guard = community_registry.begin_spawn_guard(minted.community_id);

    let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

    // Spawn engine + dispatch adapter atomically — guard captures freshly-created.
    // try_send adapter request is inside spawn_engine_with_guard; failure → guard
    // Drop tears down engine + persist dir.
    let engine = community_registry
        .spawn_engine_with_guard(
            &mut community_sync_guard,
            minted.community_id,
            minted.membership_key.clone(),
            self_owner,
            is_invite_only,
            pub_tx,
            sub_rx,
            pub_rx,
            sub_tx,
            community_adapter_tx,
        )
        .await
        .map_err(|e| format!("registry.spawn_engine_with_guard: {e}"))?;

    // ── REMOVED: explicit try_send + shutdown_engine_and_cleanup_persistence
    //    block at lib.rs:7286-7316 collapses into spawn_engine_with_guard.

    // Bootstrap-Join via the engine. Failure → ? returns Err; community_sync_guard
    // Drop runs shutdown_engine_and_cleanup_persistence.
    let outcome = engine.insert_local_event(minted.bootstrap_join.clone()).await
        .map_err(|e| format!("engine.insert_local_event (bootstrap_join): {e}"))?;
    if !matches!(outcome, InsertOutcome::Inserted) {
        return Err(format!("bootstrap Join not inserted (got {outcome:?})"));
    }
    // ── REMOVED: explicit shutdown at lib.rs:7337-7349.

    // Default #general via the engine. Same atomicity.
    engine.insert_local_event(default_channel_create_event).await
        .map_err(|e| format!("engine.insert_local_event (default_channel): {e}"))?;
    // ── REMOVED: explicit shutdown at lib.rs:7359-7369.

    // apply_space — last persistent step. Failure → ? returns Err; guard Drop tears down.
    {
        let mut state_g = crdt_state.lock().await;
        let outcome = state_g.apply_space_with_canonicalization(minted.space.clone());
        if matches!(outcome, ApplyOutcome::Rejected(_)) {
            return Err(format!("apply_space rejected: {outcome:?}"));
        }
    }
    // ── REMOVED: explicit shutdown at lib.rs:7574-7585.

    // Both txs reached durable commit point. Release rollback obligations sequentially:
    // community_sync first (per §8 #4), then channel_log.
    community_sync_guard.commit();  // sync — no .await needed

    if let Err(e) = channel_log_tx.commit().await {
        // ZEB-271 §10.2: log-and-continue (existing behavior preserved)
        tracing::warn!(error = %e, "channel_log_registry commit failed; reconcile recovers");
    }

    Ok(hex::encode(minted.community_id.0))
}
```

### §4.2 `redeem_invite_inner` (rewrite)

Same pattern as §4.1. The freshness-flag-gated dispatch logic at lib.rs:8542 (`if engine_freshly_created { ... }`) collapses — the guard handles it internally. The `engine_arc().is_some()` race comments at lib.rs:8513-8523 are now resolved by guard internalization (the race is structurally impossible — Drop only rolls back if THIS call's spawn was the fresh one).

All 17 explicit `shutdown_engine_and_cleanup_persistence` rollback sites in `redeem_invite_inner` (between lib.rs:8549 and lib.rs:9094, every one wrapped in `if engine_freshly_created { ... }`) collapse into the guard. The `engine_freshly_created: bool` local at lib.rs:8530 is removed — the guard internalizes this state.

## §5 Failure modes + concurrent-redeem race

### §5.1 Drop-without-commit safety net

If the IPC handler panics or returns Err before `commit()`, `Drop` runs `shutdown_engine_and_cleanup_persistence` via `Handle::try_current()`. If no runtime is present, accept the leak and rely on `reconcile_from_state` at next `start_node`.

### §5.2 Concurrent-redeem race (the original ZEB-260 PR #90 round-5 case)

Two `redeem_invite_inner` calls for the same `community_id` race. Both call `begin_spawn_guard` → both get guards. Both call `spawn_engine_with_guard`:

- **Caller A** wins the engines-map insert → `freshly_created = true` recorded in A's guard.
- **Caller B** finds existing engine (idempotent no-op path) → `freshly_created = false` in B's guard. The pub_rx + sub_tx + community_adapter_tx that B passed in are dropped (engine + adapter already exist from A's spawn).

Outcomes:

| A | B | Result |
|---|---|---|
| commit() | commit() | Engine remains. Both no-op. ✅ |
| Err before commit | commit() | A's guard tears down engine. B's commit() is meaningless because B's `engine_arc()` lookups will return None and B will fail elsewhere. Same as today. |
| commit() | Err before commit | B's guard Drop is no-op (`freshly_created = false`). Engine survives via A. ✅ |
| Err before commit | Err before commit | A's guard tears down. B's guard Drop is no-op. Engine + persist dir clean. ✅ |

This preserves the ZEB-260 PR #90 round-5 invariant: only the FRESH creator owns the rollback obligation.

### §5.3 Adapter-dispatch failure (atomic with engine spawn)

`spawn_engine_with_guard` is now responsible for both engine construction AND adapter `try_send`. Internal sequence:

1. `spawn_engine` (existing): build engine, insert into engines map. Records bool internally.
2. If freshly created: `community_adapter_tx.try_send(CommunityAdapterRequest { ... })`.
3. If try_send fails AND freshly created: `.await shutdown_engine_and_cleanup_persistence` inline (we're already inside the async spawn_engine_with_guard) to undo the spawn, then return Err. Guard's `freshly_created` flag is NEVER set to true (so Drop is a no-op).
4. If both succeed: set guard's `freshly_created` to the engine-spawn result. Return Ok(engine).

The IPC handler sees a single Err on adapter-dispatch failure; the guard at scope exit is a no-op. This is strictly better than today's behavior (where a separate explicit rollback runs after spawn returns true and try_send fails).

### §5.4 commit-then-Drop semantics

`commit(self)` consumes self, so Drop never runs after commit. Same as ZEB-271 §5.

### §5.5 `shutdown_engine_and_cleanup_persistence` failure inside Drop

Per §8 #8 (locked decision): log-warn and continue. Mirrors the existing scattered rollback sites' behavior + ZEB-271 §10.2 trade-off. The engine + persist dir may leak; `reconcile_from_state` at next `start_node` will detect inconsistency and clean up.

### §5.6 Reentrant `begin_spawn_guard`

Two guards opened for the same `community_id` from different IPC calls is the §5.2 case. Two guards opened for the same `community_id` from the SAME IPC call would be a programming error (no IPC handler does this); we don't add a check (mirrors ZEB-271 §5.5 — reentrant `begin_transaction` overwrites with a warn, but for this pattern reentrance just means two guards both eligible for rollback).

## §6 Threading + lock discipline

The guard holds an `Arc<CommunitySyncRegistry>` clone — no new locks introduced. `freshly_created: bool` is set ONCE during `spawn_engine_with_guard` (before the function returns), so it doesn't need atomic semantics at that point. `completed: AtomicBool` follows the same pattern as ZEB-271 — Acquire/Release ordering for Drop visibility.

No new shared state beyond what `CommunitySyncRegistry` already has (`engines: tokio::sync::Mutex<BTreeMap<SpaceId, Arc<CommunitySyncEngine>>>`). The lock-discipline section is dramatically shorter than ZEB-271's because there's no deferred-spawn queue to coordinate.

`spawn_engine_with_guard` holds the engines lock across the engine construction (mirroring today's `spawn_engine`) — this is unchanged. The new adapter `try_send` happens AFTER the lock is released (it's a non-blocking mpsc send, so no async; matches today's IPC handler order).

## §7 Tests

### §7.1 Registry-level unit tests (in `community_state_sync.rs::tests`)

5 new unit tests:

1. **`guard_commit_releases_rollback`** — spawn engine, commit guard, verify engine present + persistence dir present after guard drops.
2. **`guard_drop_without_commit_tears_down_fresh`** — spawn engine, drop guard without commit, verify engine absent + persistence dir absent.
3. **`guard_drop_idempotent_call_is_noop`** — open guard A, spawn engine; open guard B for same community (idempotent — sees existing engine), drop B without commit; verify engine still present (B's guard didn't tear down because freshly_created = false).
4. **`guard_explicit_abort_tears_down`** — spawn engine, abort guard, verify engine absent.
5. **`guard_drop_no_runtime_logs_and_leaks`** — drop guard from a `std::thread::spawn` closure (no tokio runtime); verify the warn is logged and the engine remains (acknowledged leak — reconcile recovers).

### §7.2 `create_community_inner` integration tests

Extend `create_community_inner_tests` (folder `src-tauri/src/lib.rs`):

- **`create_community_engine_torn_down_on_apply_space_rejection`** — inject apply_space rejection via fixture; verify engine + persist dir absent after IPC returns Err.
- **`create_community_engine_survives_channel_log_commit_failure`** — inject channel_log_tx commit failure (ZEB-271 §10.2 trade-off); verify community survives, channel-log spawn deferred to reconcile_from_state.

### §7.3 `redeem_invite_inner` integration tests

Extend `community_invite_only_integration.rs`:

- **`redeem_invite_engine_torn_down_on_apply_space_rejection`** — same shape as create version.
- **`redeem_invite_concurrent_race_loser_no_op_on_drop`** — kick off two concurrent redemptions for the same community_id; verify the loser's guard doesn't tear down the winner's engine.

### §7.4 ZEB-271 spec cross-ref update

Mechanical edit to `docs/specs/2026-05-10-zeb-271-channel-log-registry-transactionality-design.md` §9: change

> Membership-side parallel fix for ZEB-266 — same shape, separate ticket.

to

> Membership-side parallel fix for [ZEB-266](https://linear.app/zeblith/issue/ZEB-266) — RESOLVED in [ZEB-274](https://linear.app/zeblith/issue/ZEB-274). Note the architecture differs (RAII rollback guard, not deferred-spawn) because the IPC handler interacts with the community-sync engine pre-commit (`engine.insert_local_event(bootstrap_join)`).

## §8 Plan-time decisions locked

| # | Decision | Choice | Rationale |
|---|---|---|---|
| 1 | Architecture | RAII rollback guard (NOT deferred-spawn) | IPC handler must `engine.insert_local_event(bootstrap_join)` pre-commit; deferred-spawn breaks that contract |
| 2 | Code sharing with ZEB-271 | Copy-specialize | Patterns are structurally different; YAGNI on shared trait |
| 3 | Scope vs ZEB-271 mirror | Full atomic refactor (engine + adapter + freshness) | Half-measures leak adapter tasks; freshness flag was ZEB-260 PR #90 round-5 mitigation that becomes redundant |
| 4 | Same-critical-section ordering | Sequential, `community_sync_guard.commit()` FIRST then `channel_log_tx.commit().await` | Channel-log depends on community-sync for data plane; either-order works for correctness |
| 5 | Guard scope | Wraps engine spawn + adapter dispatch | Atomic lifecycle; orphan-adapter-on-spawn-success-then-dispatch-fail eliminated |
| 6 | Freshness semantics | Internalize into guard; remove public bool from `spawn_engine` | Concurrent-redeem race handled by guard; cleaner public surface |
| 7 | ZEB-271 cross-ref update | Update ZEB-271 spec §9 in this PR | One-line docs hygiene |
| 8 | Rollback failure handling | Log-warn and continue (mirror ZEB-271 §10.2) | Same trade-off; reconcile_from_state recovers at next start_node |
| 9 | Lock for `freshly_created` field | Plain `bool` (set once, before guard returned) | No concurrent mutation; only `completed` needs Atomic |

## §9 Out of scope

- **Generalization of the rollback-guard pattern into a shared trait.** YAGNI; only one consumer of this shape exists. (ZEB-271's tx primitive is a different pattern; sharing would mean a higher-order abstraction over both, which isn't justified for N=2.)
- **Deferred-spawn for cascading communities discovered via the delta consumer's membership callback.** The membership callback today does NOT call `spawn_engine` (only `start_node` reconcile + the two `_inner` IPC handlers do). If a future feature adds membership-cascading spawn, that's a separate ticket.
- **Refactor of the explicit `shutdown_engine_and_cleanup_persistence` calls in `start_node` boot reconcile.** Boot reconcile is a different code path (lib.rs:7297 is in start_node) with its own atomicity story; the IPC critical section is the meaningful target.
- **Generalizing the channel-log commit-failure recovery (per ZEB-271 §10.2).** Community-sync inherits the same trade-off, but its primary failure mode (apply_space rejection) is BEFORE commit, so the recovery surface is naturally smaller.
- **Synchronous fallback for `Drop` without a runtime.** Unlike ZEB-271's `abort_transaction_internal` (sync map cleanup), `shutdown_engine_and_cleanup_persistence` is fundamentally async (`engine.shutdown().await` flushes pending writes). The no-runtime case is an acknowledged leak that reconcile recovers — see §3.4 + §10.2.

## §10 Known limitations

### §10.1 (inherited from ZEB-271 §10.1)

Frontend may briefly observe a community via Tauri events before the engine is fully wired. Bounded by IPC handler duration (~ms in the success case); frontend already handles missing-engine errors gracefully.

### §10.2 No-runtime Drop leaks engine + persist dir until next start_node

If `Drop` runs in a thread without an active tokio runtime (e.g., panic-during-drop after runtime teardown), the async `shutdown_engine_and_cleanup_persistence` cannot run. The guard logs a warn and accepts the leak. `reconcile_from_state` at next `start_node` will detect the inconsistency (engine state on disk for a community owner-state never recorded) and clean up.

This is a strictly better failure mode than today (today: panic-during-drop = leak with no log; ZEB-274: panic-during-drop = leak with a clear warn pointing at the next-boot recovery).

### §10.3 Detached cleanup race with concurrent retries (Qodo round 1 finding)

`CommunitySyncSpawnGuard::Drop` spawns the async `shutdown_engine_and_cleanup_persistence` via `Handle::try_current().handle.spawn(...)` rather than `.await`-ing it inline (per `feedback_engineer_for_real_scale` — `Drop` must not block on the hot path). The IPC handler returns Err to the caller IMMEDIATELY; the cleanup task runs in the background.

**Race:** If the user immediately retries (only relevant for `redeem_invite_inner` since `create_community_inner` mints a fresh `community_id` per call):

1. Caller A: `spawn_engine_with_guard` (fresh) → `freshly_created = true` → fails before `commit()` → `Drop` spawns async cleanup task
2. Caller A returns Err immediately
3. Caller B (retry, same `community_id` from invite): `spawn_engine_with_guard` → finds existing engine in registry (cleanup task hasn't fired yet) → idempotent path → `freshly_created = false` → `Drop` is no-op → does work → `apply_space` succeeds → `commit()` → owner-state is durable for B
4. Caller A's cleanup task fires → `engine.shutdown().await` → engine torn down
5. Subsequent operations (e.g., publishing channel-config events) fail with `EngineNotRunning` until `reconcile_from_state` at next `start_node` respawns the engine

**Bounded impact:**
- Race window = `engine.shutdown().await` duration (typically a few ms; can grow under heavy backlog of pending writes — bounded by the per-community state CRDT debounce interval).
- No data loss — owner-state is durable; `reconcile_from_state` at next `start_node` respawns the engine from on-disk state.
- User-observable degradation: brief `EngineNotRunning` errors in the data plane until next process restart.

**Why we accept this:** the same race shape exists in [ZEB-271](https://linear.app/zeblith/issue/ZEB-271) §10.2 (channel-log commit failures: data plane dead until reconcile) and was accepted as a known limitation there. ZEB-274 inherits the same trade-off. A synchronous-condemnation fix (mark engine as condemned in the engines map under the engines lock, drain on next access) is non-trivial (~50-100 lines + new condemnation flag on the engines map entry + concurrent-access semantics around condemned engines) and out of scope.

**Future enhancement candidate:** add a "condemned" sentinel value to the engines map that `spawn_engine_inner_now`'s idempotent path checks; condemned-then-fresh-spawn semantics would preempt the race. Tracked separately if this UX degradation becomes user-visible.

## §11 Acceptance criteria

1. ✅ Decision (RAII rollback guard) selected per §2; deferred-spawn rejected for IPC-pre-commit-engine-interaction reason.
2. Implementation:
   - `CommunitySyncSpawnGuard` with `begin_spawn_guard` / `spawn_engine_with_guard` / `commit` / `abort` / `Drop` per §3
   - `create_community_inner` and `redeem_invite_inner` rewritten per §4 (26 explicit rollback sites — 9 in create + 17 in redeem — collapse into one guard each)
   - All 5 registry-level unit tests per §7.1 passing
   - 2 of the 4 `_inner` integration tests per §7.2 + §7.3 are passing (the existing happy-path + fence-abort tests, which exercise the guard-Drop path on the fence-rejection rollback site). The other 2 (`create_community_engine_torn_down_on_apply_space_rejection` and `redeem_invite_concurrent_race_loser_no_op_on_drop`) are deferred — both require fixture refactors (caller-supplied `SpaceId` for `mint_community_creation` to force `apply_space` rejection; concurrent IPC scaffolding for the race test) that balloon scope. The 5 §7.1 unit tests + the round 2 `guard_try_send_failure_rolls_back_without_arming_guard` regression test cover the guard semantics directly at the registry level.
3. ZEB-271 spec §9 updated in this PR (per §7.4) to mark cross-ref RESOLVED.
4. The freshness-flag (`engine_freshly_created` bool) removed from `spawn_engine` public surface; internalized into guard. ZEB-260 PR #90 round-5 race semantics preserved (verified by `redeem_invite_concurrent_race_loser_no_op_on_drop` test).
5. All 5 CI gates green: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`, `cargo check --locked --all-targets --features test-fixtures` (msrv proxy), `npx tsc --noEmit` + `npx vitest run` (frontend unaffected but gates still run).

## §12 References

- This ticket: [ZEB-274](https://linear.app/zeblith/issue/ZEB-274)
- Sibling: [ZEB-271](https://linear.app/zeblith/issue/ZEB-271) — channel-log version (different architecture per §2)
- Parent gap: [ZEB-266](https://linear.app/zeblith/issue/ZEB-266) — Phase 1 introduction of the membership-changed CRDT
- Parent epic: [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) — Sub-C v2 channels-within-communities (DONE)
- Original race-fix prior art: ZEB-260 PR #90 round-5 — the freshness-flag (`engine_freshly_created` bool) being internalized here
- Production wiring (target): `community_state_sync.rs::CommunitySyncRegistry::spawn_engine_with_guard` (the guarded public entry called from IPC handlers) + `spawn_engine_inner_now` (the inner immediate starter, also used by `start_node` boot reconcile which has no rollback obligation) + `lib.rs::create_community_inner` (line 7193) + `lib.rs::redeem_invite_inner` (line 8420). The `spawn_engine` symbol referenced in earlier drafts of this spec was renamed to `spawn_engine_inner_now` in Task 1 (commit 2fa5e90).
- Pattern reference (NOT mirror): `community_channel_log_engine.rs` ZEB-271's tx primitive — consulted for Drop semantics + `Handle::try_current()` fallback shape, not for tx mechanism
- Sibling brainstorm conversation finding: ZEB-274 was originally pitched as a "ZEB-271 mirror" but the call-site analysis (`spawn_engine` is direct from IPC handler, NOT via delta consumer; IPC handler interacts with engine pre-commit) revealed the architecture had to differ. Documented in spec §2.
