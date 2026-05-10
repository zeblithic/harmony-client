# ZEB-271 Channel-Log Registry Transactionality — Design

**Linear:** [ZEB-271](https://linear.app/zeblith/issue/ZEB-271)
**Parent:** [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) Sub-C v2 — channels-within-communities (DONE 2026-05-10)
**Branch:** `zeb-271-channel-log-registry-transactionality` cut from `origin/main` `d89409b`

## §1 Context

`ChannelLogRegistry::spawn` is currently called from the `run_community_delta_consumer`'s 3rd callback when `ChannelConfigChangeAction::Created` materializes (production wiring at `lib.rs:1453+`, registry implementation at `community_channel_log_engine.rs:1065`). This couples the channel-log lifecycle to event materialization, but **not** to durable community-creation commit. If `create_community_inner` aborts after inserting the default `ChannelCreate` event but before its final `apply_space_with_canonicalization` commit (fence generation-changed, fence registry-gone, apply_space rejection, panic), the registry has already spawned a per-channel engine + adapter task for a community that never durably committed.

Concrete leaks per phantom community:

* In-memory `ChannelLogEngine` retained in `ChannelLogRegistry::engines` indefinitely (until app stop).
* Per-channel adapter task polling the closing flag indefinitely.
* Brief UI flash via the `channel-config-updated` Tauri event.

The on-disk segment dir leak is _partially_ mitigated already — `community_registry.shutdown_engine_and_cleanup_persistence` does `tokio::fs::remove_dir_all("communities/{id_hex}/")` (`community_state_sync.rs:2369`), which encompasses the channel-log subdir at `communities/{id_hex}/channels/{channel_id_hex}/`. But there's a race: if the channel-log spawn arrives _after_ the `remove_dir_all` completes (the consumer task is independent of `_inner`'s control flow), the channel-log re-creates the directory.

Same shape for `redeem_invite_inner`: any remote `ChannelCreate` events that arrive via Zenoh sync between `community_registry.spawn_engine` and `apply_space` leak the same way.

Surfaced by CodeRabbit during PR #96 (ZEB-270 Phase 3) round-2 review at `lib.rs:1606`. Marked DEFER because the architectural fix is heavier than Phase 3's scope. ZEB-271 is the follow-up.

## §2 Approach

Approach (a) from the ZEB-271 ticket body: **defer `registry.spawn` until a durable-commit signal**. Implementation lives inside `ChannelLogRegistry`; the delta consumer's wiring is unchanged.

Rejected alternatives:

* **(b) Symmetric rollback at every failure site.** Needs a tombstone or flush primitive to handle the async race where the consumer hasn't yet processed the deferred spawn when rollback runs. Mechanical at every site, but requires the same coordination primitive (a) provides natively. Strictly more code at strictly less architectural elegance.
* **(c) Document as WAI.** The leak is bounded but persistent across the app session (memory + adapter task per phantom community). Not acceptable for a long-running daemon.

## §3 Transaction protocol

`ChannelLogRegistry` gains a per-community deferred-spawn queue and an explicit transaction lifecycle.

### §3.1 New state on `ChannelLogRegistry`

```rust
pending_transactions: std::sync::Mutex<HashMap<SpaceId, PendingTransaction>>,
next_tx_id: std::sync::atomic::AtomicU64,
```

`std::sync::Mutex` (sync) is the right choice here — critical sections never span an `.await`, and `begin_transaction` is itself sync (see §5.3).

```rust
struct PendingTransaction {
    tx_id: u64,
    queue: Vec<DeferredSpawn>,
}

struct DeferredSpawn {
    channel_id: ChannelId,
    channel_key: ChannelKey,
    state_at_hlc: Arc<dyn CommunityStateAtHlc + Send + Sync>,
    resolver: Arc<dyn ChannelIdentityResolver + Send + Sync>,
    hlc_tracker: Arc<tokio::sync::Mutex<BTreeMap<String, Hlc>>>,
}
```

### §3.2 New methods

```rust
pub fn begin_transaction(self: &Arc<Self>, community_id: SpaceId) -> CommunityTransactionGuard<R>;
```

Synchronous. Acquires `pending_transactions`, generates a fresh `tx_id` from `next_tx_id`, inserts an empty `PendingTransaction` keyed by `community_id`. If an entry already exists, overwrites with `tracing::warn!` carrying the prior `tx_id` for forensics. Returns a `CommunityTransactionGuard` carrying `(Arc<Self>, community_id, tx_id, AtomicBool completed)`.

```rust
pub async fn commit(self) -> Result<(), ChannelLogEngineError>;
```

Consuming method on `CommunityTransactionGuard`. Acquires the lock, removes the entry _only if_ `tx_id` matches; on mismatch, no-op with `tracing::warn!` (stale guard, transaction already overwritten). Drains the queue, releases the lock, then sequentially invokes the inner spawn body for each `DeferredSpawn`. **Continues attempting all remaining spawns even after an error**: the first error encountered is captured and surfaced as the `Err` return; subsequent errors are logged at `warn` (`additional deferred-spawn failure during commit drain (ignored, first error already captured)`) but do not abort the loop. This matches `shutdown_all`'s "drain everything, return last error" pattern — bailing on first error would leave later channels permanently un-spawned for this session, with the affected set varying per run depending on `HashMap` iteration order. Sets `completed = true` so `Drop` skips the safety net.

```rust
pub fn abort(self);
```

Consuming method on `CommunityTransactionGuard`. Sync (the body has no `.await` points; the `self`-by-value receiver still bypasses the `Drop` safety net). Acquires the lock, removes the entry _only if_ `tx_id` matches. Sets `completed = true`.

```rust
impl<R: tauri::Runtime> Drop for CommunityTransactionGuard<R> {
    fn drop(&mut self) {
        if !self.completed.load(Ordering::Acquire) {
            tracing::warn!(
                community_id = ?self.community_id,
                tx_id = self.tx_id,
                "CommunityTransactionGuard dropped without commit/abort — running async safety net"
            );
            let registry = Arc::clone(&self.registry);
            let community_id = self.community_id;
            let tx_id = self.tx_id;
            tokio::spawn(async move {
                registry.abort_transaction_internal(community_id, tx_id);
            });
        }
    }
}
```

`abort_transaction_internal` is sync (just removes from the parking_lot map under tx_id check); the `tokio::spawn` wrapper exists only to detach the operation from `Drop`'s sync context — it could equally be a sync direct call, but `tokio::spawn` keeps Drop fast and predictable for the success case where contention is unlikely.

### §3.3 Modified behavior of `ChannelLogRegistry::spawn`

Before any heavy work (dir-create, engine construction, adapter dispatch), check `pending_transactions`:

* If `community_id` has an entry, append a `DeferredSpawn` to its queue and return `Ok(SpawnOutcome::DeferredForCommit)`.
* Else: existing fast-path. Returns `Ok(SpawnOutcome::Spawned(Arc<ChannelLogEngine<R>>))`.

Return type changes from `Result<Arc<ChannelLogEngine<R>>, _>` to `Result<SpawnOutcome<R>, _>`:

```rust
pub enum SpawnOutcome<R: tauri::Runtime> {
    Spawned(Arc<ChannelLogEngine<R>>),
    DeferredForCommit,
}
```

The two existing production callers (delta consumer's 3rd callback at `lib.rs:1453+`, and `ChannelLogRegistry::reconcile_from_state`) update with one-line shape adjustments. The consumer already discards the `Arc<ChannelLogEngine>`. `reconcile_from_state` runs at start_node time outside any transaction, so its caller can `unreachable!` on `DeferredForCommit` (or treat as Ok and trust §5.6's debug_assert to catch the unexpected path).

## §4 Caller flow

### §4.1 `create_community_inner`

```rust
let tx = channel_log_registry.begin_transaction(minted.community_id);

// EXISTING: community_registry.spawn_engine, adapter dispatch, bootstrap_join,
// default-channel mint + insert, fence, apply_space …
// All existing rollback paths just `return Err(...)` — the dropped tx auto-aborts.

// SUCCESS: after apply_space succeeds, before Ok return.
// apply_space is the LAST PERSISTENT step — the community is committed.
// If commit() fails, log and continue: the deferred channel-log spawns
// (e.g., default #general) will be re-attempted by reconcile_from_state
// at next start_node. Returning Err here would surface the create as
// failed even though the community exists, leading to retry → duplicate
// community.
if let Err(e) = tx.commit().await {
    tracing::warn!(
        community_id = %hex::encode(minted.community_id.0),
        error = %e,
        "channel_log_registry commit failed after durable community create; \
         pending channel-log spawns will be re-attempted via \
         reconcile_from_state at next start_node"
    );
}
Ok(hex::encode(minted.community_id.0))
```

The `begin_transaction` line goes BEFORE `community_registry.spawn_engine` (per §5.1).

### §4.2 `redeem_invite_inner`

Same shape. `begin_transaction(minted.community_id)` immediately after `mint_redemption` (around current line 7969), log-and-continue pattern on commit failure (not `?` propagation — see §4.1 rationale). The redemption Space is durable at apply_space; returning Err would cause a non-idempotent retry → second self-Join append (ZEB-260 nominal-cost path).

For invite-only redemption that walks through the counter-sign hop, the transaction stays open across the unicast send + counter-sign wait. Acceptable: no new `ChannelCreate` events arrive locally during that window (the engine is only sending unicast requests; no remote sync until the counter-signed Join lands).

## §5 Failure-mode + reentrancy

### §5.1 begin_transaction position

Called BEFORE `community_registry.spawn_engine`. Defensive — the community engine's spawn task may emit synthesized deltas as it loads disk replay state on start; queueing must be active before any such delta can fire.

### §5.2 Drop safety net

* Sync `Drop` (cannot be async).
* Uses `Handle::try_current()` to decide which abort path to take:
  * If a runtime is present: spawns a Tokio task via `handle.spawn(...)` to call `abort_transaction_internal`. The task captures `Arc<Self>` so the registry stays alive.
  * If no runtime is present (e.g., panic-during-drop after runtime teardown, or a future sync caller of `begin_transaction`): calls `abort_transaction_internal` synchronously on the calling thread. Both paths converge on the same map-cleanup outcome.
* Logs `tracing::warn!` so dropped transactions are loud in tests and prod.
* Real failure paths in `_inner` functions are early-returns; the safety net only matters for panics or future refactors.

### §5.3 Lock acquisition in begin_transaction

`begin_transaction` is _synchronous_ and uses `std::sync::Mutex` for `pending_transactions`. Critical section: HashMap insert + atomic increment. Never spans an `.await`. No tokio worker thread is parked.

### §5.4 tx_id-tagged transactions

A stale guard's deferred `tokio::spawn(abort)` could clobber a fresh transaction's queue if the same `community_id` is re-used (real for `redeem_invite_inner` retries). Tagging every transaction with a monotonic `tx_id: u64` from `AtomicU64` and verifying the tag on commit/abort closes the race:

```text
T1: redeem_invite_inner #1 begins → map[C] = (tx_id=42, [])
T2: redeem_invite_inner #1 fails → drop guard → tokio::spawn(abort_internal(C, 42))
T3: redeem_invite_inner #2 begins → map[C] = (tx_id=43, [])  // overwrite warn
T4: spawn(C, channel_a) → map[C].queue.push(...)
T5: tokio::spawn from T2 runs → abort_internal(C, 42) sees map[C].tx_id=43, no-op
T6: redeem_invite_inner #2 commits → drains tx_id=43's queue → channel_a spawns
```

### §5.5 Reentrant begin_transaction

If `begin_transaction(C)` is called when an entry for C already exists, overwrite with `tracing::warn!` carrying both `tx_id`s. The prior guard's commit/abort becomes a no-op due to tx_id mismatch (per §5.4) — no clobbering.

### §5.6 Regression detection

Forgetting to call `begin_transaction` in a transactional path is a programmer error caught by the §7.2/§7.3 integration tests — they assert the registry has no leaked engine after a failure path, which is impossible if `begin_transaction` is missing. No runtime diagnostic needed.

## §6 Threading

### §6.1 NodeState (no schema change)

The `channel_log_registry: Option<Arc<ChannelLogRegistry<tauri::Wry>>>` field already exists from ZEB-270 Phase 3 (`lib.rs:307-308`).

### §6.2 IPC handlers

`create_community` and `redeem_invite` snapshot `channel_log_registry` from NodeState alongside `community_registry`. Same `.ok_or("channel_log_registry missing — node not running?")?` pattern.

### §6.3 Inner function generics

`create_community_inner` and `redeem_invite_inner` become `<R: tauri::Runtime>` to accept `Arc<ChannelLogRegistry<R>>`. Bodies don't otherwise depend on R; the change is purely additive at the type level. Production passes `R = tauri::Wry`; tests pass `R = tauri::test::MockRuntime`.

### §6.4 Test fixtures

Existing tests in `create_community_inner_tests` and `redeem_invite_inner_tests` gain a `ChannelLogRegistry<MockRuntime>` fixture and pass it to the inner call. Reuse the construction pattern from `community_channel_log_engine::tests::registry_*` (referenced by the docstring at `NodeState.channel_log_registry`).

## §7 Tests

### §7.1 Registry-level unit tests (in `community_channel_log_engine::tests`)

Eight new tests:

1. `begin_commit_drains_queued_spawn` — open tx, spawn(channel_a), commit, assert engine for channel_a present in `engines` map.
2. `begin_abort_drops_queued_spawn` — open tx, spawn(channel_a), abort, assert no engine present, no on-disk dir.
3. `dropped_guard_safety_net_aborts` — open tx, spawn(channel_a), drop without commit/abort, await one tokio yield, assert no engine present.
4. `spawn_outside_transaction_immediate` — spawn(channel_a) with no tx open, assert engine immediately present.
5. `stale_guard_commit_no_ops` — open tx_A, then begin_transaction (overwrite) → tx_B; commit tx_A; assert tx_B's pending entry intact.
6. `reentrant_begin_transaction_warns_and_overwrites` — open tx_A, begin_transaction(same C) → tx_B; assert map carries tx_B's tx_id; assert tracing event recorded (use `tracing-test` or capture mechanism already in use).
7. `multiple_deferred_spawns_drain_in_order` — open tx, spawn(a), spawn(b), spawn(c), commit; assert a, b, c present in insertion order.
8. `commit_partial_failure_continues` — inject failure into the second deferred spawn (e.g., dir-create error via tampered identity_dir); commit; assert error returned but third spawn still attempted (and logged on failure).

### §7.2 `create_community_inner` failure-path integration tests

Three new + extend existing happy path:

1. `happy_path_spawns_default_channel_engine` (extend existing) — assert the registry has the #general engine after `create_community_inner` returns Ok.
2. `apply_space_rejected_no_channel_log_leak` — construct a crdt_state where apply_space rejects (e.g., pre-apply a conflicting Space row); verify `channel_log_registry.engines` is empty after the call returns Err.
3. `fence_generation_changed_no_channel_log_leak` — bump `NodeState.generation` between snapshot and fence; verify no leak.

### §7.3 `redeem_invite_inner` integration test

One new:

1. `happy_path_no_pending_transaction_after_success` — assert registry's `pending_transactions` map is empty after success (proxy: tx was committed).

Failure paths covered by §7.1's protocol tests; redeem doesn't mint `ChannelCreate` locally inside the tx — leak protection is for remote sync events that need a full Zenoh harness to test, deferred.

### §7.4 Mechanical updates

Every existing test in `create_community_inner_tests` and `redeem_invite_inner_tests` gains one line constructing a `ChannelLogRegistry<MockRuntime>` fixture and passing it.

## §8 Plan-time decisions locked

| # | Decision | Rationale |
|---|---|---|
| D1 | Approach (a) — defer-spawn via commit signal | Race-free at the source; (b) requires same coordination primitive at strictly more sites |
| D2 | Queue lives in `ChannelLogRegistry`, not in the delta consumer | Single source of truth; consumer wiring unchanged |
| D3 | `tx_id` tagging via `AtomicU64`, verified on commit/abort | Closes the stale-abort-clobbers-fresh-tx race for `redeem_invite_inner` retries |
| D4 | Sync `Drop` + `tokio::spawn(abort_transaction_internal)` safety net with `tracing::warn!` | Catches forgotten cleanup; loud in logs |
| D5 | `std::sync::Mutex` for `pending_transactions` | Sync, brief critical sections, no `.await` spanning the lock; matches codebase convention (NodeState, etc.); no new dep |
| D6 | `begin_transaction` is sync, called BEFORE `community_registry.spawn_engine` | Defensive against disk-replay deltas during engine spawn |
| D7 | `_inner` functions become `<R: tauri::Runtime>` generic | Lets tests use `MockRuntime` while production uses `Wry` |
| D8 | `redeem_invite_inner` failure-path coverage limited to §7.1 protocol tests | Full Zenoh-driven scenario deferred; protocol-level coverage is sufficient |

## §9 Out of scope

* Generalizing the transaction primitive to other CRDT consumers (membership-changed, owner-state-replicated). This pattern is specific to the channel-log spawn callback shape; ZEB-266 has the same membership shape but a separate fix.
* Cancellation of in-flight remote sync during transaction abort (transaction protocol only governs the channel-log spawn; remote events themselves still arrive via Zenoh and are processed by the community engine).
* Persistence of pending transactions across app restarts (an in-flight transaction whose process dies behaves identically to an aborted transaction — the next start will not see the unspawned channels).
* Membership-side parallel fix for ZEB-266 — same shape, separate ticket.

## §10 Known limitations

These are deliberate trade-offs, surfaced and accepted during round 2 review of PR #99. Future tickets MAY address them; this spec does not.

### §10.1 `channel-config-updated` may emit before the channel-log engine is alive

`run_community_delta_consumer`'s second callback emits `channel-config-updated` immediately after the third callback returns. When the third callback returns `SpawnOutcome::DeferredForCommit`, the engine is not yet running — the frontend can observe the new channel and immediately call `list_channel_messages` / `post_channel_message` against it before `commit()` drains the queue, hitting `EngineNotRunning`.

The race window is bounded by `commit()` duration (small ms in the success case), and the frontend already handles `EngineNotRunning` gracefully (the data plane returns an empty list / surfaces the error to the user via the standard IPC error path). The race existed pre-ZEB-271 too — `DeferredForCommit` makes it consistent rather than introducing it.

A future fix would suppress the `channel-config-updated` emit when the third callback returns `DeferredForCommit` and re-emit it from `commit()`'s drain loop after each successful `spawn_inner_now`. This requires extending the consumer's callback contract (a fourth callback or a richer return type from the third) and is out of scope for ZEB-271. (CodeRabbit Major round 2 outside-diff.)

### §10.2 `commit()` failures are recovery-deferred to the next `start_node`

When `channel_log_tx.commit().await` returns `Err` from inside `create_community_inner` / `redeem_invite_inner`, the IPC handler logs the error at `warn` and **still returns `Ok`** with the durably-committed `community_id`. Deferred spawns that did not run are lost for this session — they are re-attempted on the next process start via `reconcile_from_state`, which iterates the materialized membership and spawns each non-deleted channel.

The alternative — propagating `commit()` errors as IPC `Err` — would tell the caller the create/redeem failed even though `apply_space` had already durably committed. The user's retry would mint a duplicate community (in the create case) or append a second self-Join (in the OPEN-invite redeem case, which is explicitly non-idempotent). The log-and-continue pattern is the lesser evil, but the user-observable consequence is real: between the failed-commit and the next `start_node`, the affected channels are visible (the `channel-config-updated` emit fires regardless) but their data plane is dead. The frontend's `EngineNotRunning` handling is the only end-user signal.

Future enhancement candidates: (a) a per-process retry loop that re-runs `spawn_inner_now` for failed commits with bounded backoff; (b) emitting a `channel-log-spawn-failed` event the frontend can show as a banner. Both are out of scope for ZEB-271. (CodeRabbit Major round 1; Greptile P2 round 2 — both surfaced this; the user explicitly signed off on log-and-continue at convergence.)

## §11 Acceptance criteria (mirror ZEB-271)

1. ✅ Decision (a) selected: defer-spawn via commit signal, queue lives in `ChannelLogRegistry`. Approach (b) and (c) rejected (per §2).
2. Implementation:
   * `CommunityTransactionGuard` with `begin_transaction` / `commit` / `abort` / `Drop` per §3
   * `create_community_inner` and `redeem_invite_inner` updated per §4 (and threaded per §6)
   * All registry-level unit tests per §7.1 passing
   * All `_inner` integration tests per §7.2 / §7.3 passing
3. (n/a — approach (a) selected; (c)'s documentation requirement folded into this spec's §1)
4. Same approach applied to `redeem_invite_inner` — see §4.2 + §7.3.

## §12 References

* This ticket: [ZEB-271](https://linear.app/zeblith/issue/ZEB-271)
* Parent: [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) Sub-C v2 — channels-within-communities (DONE)
* Phase 3 PR (where deferred from): https://github.com/zeblithic/harmony-client/pull/96
* CodeRabbit comment that surfaced this: PR #96 round-2 review at `lib.rs:1606`
* Sibling Phase 1: [ZEB-266](https://linear.app/zeblith/issue/ZEB-266) — same-shape membership-changed transactional gap (acknowledged but not resolved by this spec)
* Production wiring: `lib.rs:1453+` (delta consumer's 3rd callback), `community_channel_log_engine.rs:1065` (`ChannelLogRegistry::spawn`), `community_state_sync.rs:2369` (`shutdown_engine_and_cleanup_persistence`)
