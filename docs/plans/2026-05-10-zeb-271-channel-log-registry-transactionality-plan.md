# ZEB-271 Channel-Log Registry Transactionality — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `CommunityTransactionGuard` primitive to `ChannelLogRegistry` that defers per-channel spawns until a durable-commit signal, eliminating the phantom-engine leak when `create_community_inner` or `redeem_invite_inner` aborts after inserting `ChannelCreate` events.

**Architecture:** `ChannelLogRegistry` gains a `pending_transactions: Mutex<HashMap<SpaceId, PendingTransaction>>` map and a `next_tx_id: AtomicU64` counter. `begin_transaction(community_id)` returns a `CommunityTransactionGuard` whose `commit().await` drains the queue (firing real spawns) and whose `abort()` (sync — or `Drop` safety net) discards the queue. `ChannelLogRegistry::spawn` checks the map: if a transaction is open, it appends a `DeferredSpawn` and returns `SpawnOutcome::DeferredForCommit`; otherwise the existing fast-path runs and returns `SpawnOutcome::Spawned(Arc<ChannelLogEngine>)`. `tx_id`-tagged guards close the stale-abort-clobbers-fresh-tx race for `redeem_invite_inner` retries.

**Tech Stack:** Rust 2021, `tokio` (already in use), `std::sync::Mutex` for the sync map, `tracing` for diagnostics, `tauri::test::MockRuntime` for tests.

**Spec:** `docs/specs/2026-05-10-zeb-271-channel-log-registry-transactionality-design.md` (commit `7c65e05`)

**Branch:** `zeb-271-channel-log-registry-transactionality` already cut from `origin/main` `d89409b`. Spec at `7c65e05`. No further branching needed.

---

## File structure

**Create:** none (all changes go into existing files).

**Modify:**

1. `src-tauri/src/community_channel_log_engine.rs` (+~250L)
   - Add `SpawnOutcome<R>` enum (~10L)
   - Add `DeferredSpawn` struct (~12L)
   - Add `PendingTransaction` struct (~8L)
   - Add `CommunityTransactionGuard<R>` struct + impl (~120L)
   - Modify `ChannelLogRegistry` struct: new `pending_transactions` + `next_tx_id` fields (~5L)
   - Modify `ChannelLogRegistry::new` to initialize the new fields (~5L)
   - Modify `ChannelLogRegistry::spawn` to check + queue when transaction open (~30L)
   - Modify `RegistryFixture` + `spawn_under_fixture` test helpers to handle the SpawnOutcome variant (~10L)
   - Add 8 new unit tests in the existing `tests` mod (~250L)

2. `src-tauri/src/lib.rs` (+~120L net)
   - `create_community_inner` (line 7184): add `<R: tauri::Runtime>` generic + `channel_log_registry: Arc<ChannelLogRegistry<R>>` param + `let tx = ...; ... tx.commit().await?;` wiring (~6L delta in the function body, ~3L generic/param)
   - `create_community` IPC handler (line 7598): snapshot `channel_log_registry` from NodeState (9-tuple → 10-tuple); pass to inner (~3L)
   - `redeem_invite_inner` (line 7936): same pattern as create_community_inner (~9L)
   - `redeem_invite` IPC handler (line 8670): same snapshot extension (~3L)
   - Delta consumer's 3rd callback at lib.rs:1490+ (production wiring): handle `SpawnOutcome` match (~5L)
   - `create_community_inner_tests` (line 7686): mechanical fixture updates across all existing tests + 3 new failure-path tests (~250L new test code; ~30L mechanical fixture passes)
   - `redeem_invite_inner_tests` (line 8790): mechanical fixture updates + 1 new test (~50L new test code; ~30L mechanical fixture passes)

3. `src-tauri/src/community_channel_log_engine.rs::ChannelLogRegistry::reconcile_from_state`
   - One-line update to handle the new SpawnOutcome return shape from spawn (~3L)

**No changes to:**

- `src-tauri/Cargo.toml` (no new deps; std::sync::Mutex per spec D5)
- `src-tauri/src/community_state_sync.rs` (community-engine registry untouched)
- `src-tauri/src/event_loop.rs` (delta consumer wiring is in lib.rs; the bridge in event_loop is unaffected)
- The frontend (this is a backend-only change; no IPC contract change visible to JS)
- The wire format (`channel-config-updated` event still emits the same payload)

---

## Task 0: Pre-flight + green baseline

**Files:** none modified; verification only.

**Goal:** Confirm all five CI gates green on the freshly-cut `zeb-271-channel-log-registry-transactionality` branch so any later red is unambiguously our doing. **No commit at the end of this task.**

- [ ] **Step 1: Verify branch state**

```bash
git branch --show-current
# Expected: zeb-271-channel-log-registry-transactionality

git log --oneline -3
# Expected (most recent first):
#   7c65e05 docs(zeb-271): use std::sync::Mutex (codebase convention) instead of parking_lot
#   e12f9a9 docs(zeb-271): channel-log registry transactionality design spec
#   d89409b ZEB-273: split rust CI job + cargo-nextest (#98)
```

- [ ] **Step 2: cargo fmt check**

  ```bash
  cd src-tauri && cargo fmt --all -- --check
  ```

  Expected: no output, exit 0.

- [ ] **Step 3: cargo clippy**

  ```bash
  cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
  ```

  Expected: clean build, no warnings, exit 0.

- [ ] **Step 4: cargo nextest run**

  ```bash
  cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast
  ```

  Expected: all tests pass.

- [ ] **Step 5: cargo check (msrv proxy)**

  The MSRV check uses the declared `rust-version` in `Cargo.toml`; locally we run with whatever stable is installed, which is fine for a baseline confirmation (CI catches a real MSRV regression).

  ```bash
  cd src-tauri && cargo check --locked --all-targets --features test-fixtures
  ```

  Expected: clean exit 0.

- [ ] **Step 6: Frontend gates** (run from repo root)

  ```bash
  npx tsc --noEmit
  npx vitest run
  ```

  Expected: clean exits.

**No commit.** Move to Task 1.

---

## Task 1: ChannelLogRegistry transaction primitives

**Files:**

- Modify: `src-tauri/src/community_channel_log_engine.rs` (struct/method additions; modified `spawn`; new tests)

**Goal:** Add the transaction protocol to `ChannelLogRegistry` (begin_transaction, commit, abort, Drop safety net, modified spawn). Behavior of all existing spawn callsites is preserved (transaction-less spawn is the existing fast-path). **The migration of existing callers to the new `SpawnOutcome` return type happens in Task 2** — Task 1's job is to ship the API + unit tests.

> **Plan-vs-implementation note (CodeRabbit round 3):** Changing `ChannelLogRegistry::spawn`'s return type to `Result<SpawnOutcome<R>, _>` is technically a breaking change for the existing `lib.rs` and integration-test callsites — Task 1 as scoped here would land a compile-broken tree if pushed in isolation. In the actual implementation, Task 1 and Task 2 were folded into a **single commit** (`d482f49`) so the workspace stays compile-clean at every commit boundary. The plan's task split is preserved here for reading order; treat Task 1 + Task 2 as a single atomic unit when executing.

**Strategy:** TDD — write the 8 unit tests from spec §7.1 first as failing scaffolds (compile errors are fine), then implement just enough to make them pass one bucket at a time.

- [ ] **Step 1: Write all 8 failing tests as scaffolds**

Add the following test bodies at the end of the existing `tests` mod in `src-tauri/src/community_channel_log_engine.rs` (after `registry_reconcile_continues_past_spawn_failure` at line ~2820). Test 8's failure injection requires a tampered `identity_dir` — see helper.

```rust
    // ZEB-271: transaction-protocol tests. These verify that
    // begin_transaction → spawn → commit fires the deferred spawn,
    // begin_transaction → spawn → abort drops it, and corner cases
    // (drop safety net, stale tx_id, reentrancy, ordering, partial
    // failure) all converge on the documented behavior. See spec §7.1.

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tx_begin_commit_drains_queued_spawn() {
        let fix = build_registry_fixture().await;
        let community_id = SpaceId([0xc1; 16]);
        let channel_id = ChannelId([0xa1; 16]);

        let tx = Arc::clone(&fix.registry).begin_transaction(community_id);

        let key = derive_channel_key(&fix.membership_key, &community_id, &channel_id);
        let outcome = Arc::clone(&fix.registry)
            .spawn(
                community_id,
                channel_id,
                key,
                Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                Arc::clone(&fix.resolver) as Arc<dyn ChannelIdentityResolver + Send + Sync>,
                Arc::clone(&fix.hlc_tracker),
            )
            .await
            .expect("spawn");

        assert!(
            matches!(outcome, SpawnOutcome::DeferredForCommit),
            "spawn during open transaction must return DeferredForCommit"
        );

        // Pre-commit: engine must NOT be in the registry yet.
        assert!(
            fix.registry.engine(&community_id, &channel_id).await.is_none(),
            "deferred spawn should not be visible in engines map before commit"
        );

        tx.commit().await.expect("commit");

        // Post-commit: engine must be in the registry.
        assert!(
            fix.registry.engine(&community_id, &channel_id).await.is_some(),
            "deferred spawn must be visible after commit drains the queue"
        );

        fix.registry
            .stop(&community_id, &channel_id)
            .await
            .expect("stop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tx_begin_abort_drops_queued_spawn() {
        let fix = build_registry_fixture().await;
        let community_id = SpaceId([0xc2; 16]);
        let channel_id = ChannelId([0xa2; 16]);

        let tx = Arc::clone(&fix.registry).begin_transaction(community_id);

        let key = derive_channel_key(&fix.membership_key, &community_id, &channel_id);
        Arc::clone(&fix.registry)
            .spawn(
                community_id,
                channel_id,
                key,
                Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                Arc::clone(&fix.resolver) as Arc<dyn ChannelIdentityResolver + Send + Sync>,
                Arc::clone(&fix.hlc_tracker),
            )
            .await
            .expect("spawn");

        tx.abort();

        assert!(
            fix.registry.engine(&community_id, &channel_id).await.is_none(),
            "aborted transaction must not spawn the queued engine"
        );

        // No on-disk dir for the channel either (the registry's spawn
        // body never ran, so no fs::create_dir_all happened).
        let channel_dir = fix
            ._tmp
            .path()
            .join("communities")
            .join(hex::encode(community_id.0))
            .join("channels")
            .join(hex::encode(channel_id.0));
        assert!(
            !channel_dir.exists(),
            "aborted transaction must not create the channel-log on-disk dir"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tx_dropped_guard_safety_net_aborts() {
        let fix = build_registry_fixture().await;
        let community_id = SpaceId([0xc3; 16]);
        let channel_id = ChannelId([0xa3; 16]);

        {
            let _tx = Arc::clone(&fix.registry).begin_transaction(community_id);
            let key = derive_channel_key(&fix.membership_key, &community_id, &channel_id);
            Arc::clone(&fix.registry)
                .spawn(
                    community_id,
                    channel_id,
                    key,
                    Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                    Arc::clone(&fix.resolver) as Arc<dyn ChannelIdentityResolver + Send + Sync>,
                    Arc::clone(&fix.hlc_tracker),
                )
                .await
                .expect("spawn");
            // _tx drops here without explicit commit/abort.
        }

        // Drop spawned a tokio task to call abort_transaction_internal.
        // Yield repeatedly to let it run. Per spec §3.2 / §5.2 the safety
        // net is fire-and-forget; one yield is usually enough but a small
        // sleep removes flake risk.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(
            fix.registry.engine(&community_id, &channel_id).await.is_none(),
            "dropped transaction guard must trigger safety-net abort"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tx_spawn_outside_transaction_immediate() {
        let fix = build_registry_fixture().await;
        let community_id = SpaceId([0xc4; 16]);
        let channel_id = ChannelId([0xa4; 16]);

        let key = derive_channel_key(&fix.membership_key, &community_id, &channel_id);
        let outcome = Arc::clone(&fix.registry)
            .spawn(
                community_id,
                channel_id,
                key,
                Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                Arc::clone(&fix.resolver) as Arc<dyn ChannelIdentityResolver + Send + Sync>,
                Arc::clone(&fix.hlc_tracker),
            )
            .await
            .expect("spawn");

        match outcome {
            SpawnOutcome::Spawned(engine) => {
                assert_eq!(engine.community_id(), community_id);
                assert_eq!(engine.channel_id(), channel_id);
            }
            SpawnOutcome::DeferredForCommit => {
                panic!("spawn outside a transaction must return Spawned, not Deferred");
            }
        }

        fix.registry
            .stop(&community_id, &channel_id)
            .await
            .expect("stop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tx_stale_guard_commit_no_ops() {
        let fix = build_registry_fixture().await;
        let community_id = SpaceId([0xc5; 16]);
        let channel_a = ChannelId([0xa5; 16]);
        let channel_b = ChannelId([0xb5; 16]);

        // Open tx_A.
        let tx_a = Arc::clone(&fix.registry).begin_transaction(community_id);

        // Spawn a channel under tx_A's queue.
        let key_a = derive_channel_key(&fix.membership_key, &community_id, &channel_a);
        Arc::clone(&fix.registry)
            .spawn(
                community_id,
                channel_a,
                key_a,
                Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                Arc::clone(&fix.resolver) as Arc<dyn ChannelIdentityResolver + Send + Sync>,
                Arc::clone(&fix.hlc_tracker),
            )
            .await
            .expect("spawn a");

        // Open tx_B for the same community_id (overwrites tx_A's entry).
        let tx_b = Arc::clone(&fix.registry).begin_transaction(community_id);

        // Spawn a different channel under tx_B's queue.
        let key_b = derive_channel_key(&fix.membership_key, &community_id, &channel_b);
        Arc::clone(&fix.registry)
            .spawn(
                community_id,
                channel_b,
                key_b,
                Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                Arc::clone(&fix.resolver) as Arc<dyn ChannelIdentityResolver + Send + Sync>,
                Arc::clone(&fix.hlc_tracker),
            )
            .await
            .expect("spawn b");

        // Commit tx_A — should be a no-op (tx_id mismatch).
        tx_a.commit().await.expect("stale commit no-ops");

        // Channel a was queued in tx_A's overwritten entry — it should
        // NOT be in the registry (tx_A's queue was dropped on overwrite).
        assert!(
            fix.registry.engine(&community_id, &channel_a).await.is_none(),
            "stale tx_A.commit must not resurrect tx_A's overwritten queue"
        );

        // tx_B's queue is intact — channel b can still be committed.
        tx_b.commit().await.expect("commit b");

        assert!(
            fix.registry.engine(&community_id, &channel_b).await.is_some(),
            "tx_B.commit must drain tx_B's queue"
        );

        fix.registry
            .stop(&community_id, &channel_b)
            .await
            .expect("stop b");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tx_reentrant_begin_transaction_overwrites() {
        let fix = build_registry_fixture().await;
        let community_id = SpaceId([0xc6; 16]);

        let tx_a = Arc::clone(&fix.registry).begin_transaction(community_id);
        let tx_id_a = tx_a.tx_id_for_test();

        let tx_b = Arc::clone(&fix.registry).begin_transaction(community_id);
        let tx_id_b = tx_b.tx_id_for_test();

        assert_ne!(
            tx_id_a, tx_id_b,
            "reentrant begin_transaction must mint a fresh tx_id"
        );

        // Drop both without explicit cleanup (safety net handles either).
        drop(tx_a);
        drop(tx_b);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Map entry should be gone (one of the safety-net aborts removed
        // it; the other no-ops on tx_id mismatch — both correct).
        assert!(
            !fix.registry.has_pending_transaction_for_test(&community_id),
            "after both drops, no pending transaction entry remains"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tx_multiple_deferred_spawns_drain_in_order() {
        let fix = build_registry_fixture().await;
        let community_id = SpaceId([0xc7; 16]);
        let channels: [ChannelId; 3] = [
            ChannelId([0x01; 16]),
            ChannelId([0x02; 16]),
            ChannelId([0x03; 16]),
        ];

        let tx = Arc::clone(&fix.registry).begin_transaction(community_id);

        for ch in channels.iter() {
            let key = derive_channel_key(&fix.membership_key, &community_id, ch);
            Arc::clone(&fix.registry)
                .spawn(
                    community_id,
                    *ch,
                    key,
                    Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                    Arc::clone(&fix.resolver)
                        as Arc<dyn ChannelIdentityResolver + Send + Sync>,
                    Arc::clone(&fix.hlc_tracker),
                )
                .await
                .expect("spawn");
        }

        tx.commit().await.expect("commit");

        for ch in channels.iter() {
            assert!(
                fix.registry.engine(&community_id, ch).await.is_some(),
                "channel {:?} must be present after commit",
                ch
            );
            fix.registry.stop(&community_id, ch).await.expect("stop");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tx_commit_partial_failure_continues() {
        // Failure injection: pre-create the second channel's dir as a
        // FILE (not a directory) so the inner spawn's
        // `std::fs::create_dir_all` fails on it. The first and third
        // channels' dirs are untouched, so their spawns succeed.
        let fix = build_registry_fixture().await;
        let community_id = SpaceId([0xc8; 16]);
        let channels: [ChannelId; 3] = [
            ChannelId([0x11; 16]),
            ChannelId([0x22; 16]),
            ChannelId([0x33; 16]),
        ];

        // Sabotage channel 2's path — create a file at the path that
        // create_dir_all would want to be a directory.
        let bad_dir = fix
            ._tmp
            .path()
            .join("communities")
            .join(hex::encode(community_id.0))
            .join("channels");
        std::fs::create_dir_all(&bad_dir).unwrap();
        let bad_path = bad_dir.join(hex::encode(channels[1].0));
        std::fs::write(&bad_path, b"sabotage").unwrap();

        let tx = Arc::clone(&fix.registry).begin_transaction(community_id);
        for ch in channels.iter() {
            let key = derive_channel_key(&fix.membership_key, &community_id, ch);
            Arc::clone(&fix.registry)
                .spawn(
                    community_id,
                    *ch,
                    key,
                    Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                    Arc::clone(&fix.resolver)
                        as Arc<dyn ChannelIdentityResolver + Send + Sync>,
                    Arc::clone(&fix.hlc_tracker),
                )
                .await
                .expect("spawn (queueing always succeeds during a tx)");
        }

        let result = tx.commit().await;
        assert!(
            result.is_err(),
            "commit must surface the first error from the partial-failure drain"
        );

        // First channel spawned successfully; third should also have
        // been attempted and succeeded.
        assert!(
            fix.registry
                .engine(&community_id, &channels[0])
                .await
                .is_some(),
            "first channel must spawn before the second's failure"
        );
        assert!(
            fix.registry
                .engine(&community_id, &channels[2])
                .await
                .is_some(),
            "third channel must still be attempted after the second's failure"
        );
        assert!(
            fix.registry
                .engine(&community_id, &channels[1])
                .await
                .is_none(),
            "the second (sabotaged) channel must not be present"
        );

        fix.registry.stop(&community_id, &channels[0]).await.ok();
        fix.registry.stop(&community_id, &channels[2]).await.ok();
    }
```

These tests reference symbols that don't exist yet (`SpawnOutcome`, `begin_transaction`, `tx_id_for_test`, `has_pending_transaction_for_test`). That's expected — TDD. The compile failure tells us exactly what API surface to add.

- [ ] **Step 2: Confirm compile failures**

  ```bash
  cd src-tauri && cargo nextest list --features test-fixtures 2>&1 | tail -20
  ```

  Expected: compile errors naming `SpawnOutcome`, `begin_transaction`, `tx_id_for_test`, `has_pending_transaction_for_test`. This is the "tests fail" baseline.

- [ ] **Step 3: Add `SpawnOutcome` enum + `DeferredSpawn` + `PendingTransaction`**

Add near the top of `src-tauri/src/community_channel_log_engine.rs`, just after the existing `ChannelLogEngineError` definition (so the types are reachable from both the registry impl and the tests mod). Use the existing `Arc<Mutex<BTreeMap<...>>>` shape from the spawn signature for `hlc_tracker`.

```rust
/// Outcome of `ChannelLogRegistry::spawn`. Per ZEB-271 spec §3.3, a
/// spawn during an open community transaction is deferred until commit;
/// a spawn outside a transaction follows the existing fast-path and
/// returns the live engine.
pub enum SpawnOutcome<R: tauri::Runtime> {
    /// The engine was constructed and inserted into the registry.
    Spawned(Arc<ChannelLogEngine<R>>),
    /// A community transaction is open for this `community_id`; the
    /// spawn was queued and will fire on `commit()`.
    DeferredForCommit,
}

/// One queued spawn within a `PendingTransaction`. Captures every
/// argument the registry's spawn body needs so commit can replay it.
struct DeferredSpawn {
    channel_id: ChannelId,
    channel_key: ChannelKey,
    state_at_hlc: Arc<dyn CommunityStateAtHlc + Send + Sync>,
    resolver: Arc<dyn ChannelIdentityResolver + Send + Sync>,
    hlc_tracker: Arc<tokio::sync::Mutex<BTreeMap<String, Hlc>>>,
}

/// One open community transaction. `tx_id` tags every guard so a stale
/// guard's deferred abort cannot clobber a fresh transaction's queue
/// (spec §5.4).
struct PendingTransaction {
    tx_id: u64,
    queue: Vec<DeferredSpawn>,
}
```

- [ ] **Step 4: Add `pending_transactions` + `next_tx_id` to `ChannelLogRegistry`**

Modify the existing `ChannelLogRegistry<R>` struct (search for `pub struct ChannelLogRegistry<R: tauri::Runtime>`):

```rust
pub struct ChannelLogRegistry<R: tauri::Runtime> {
    config: ChannelLogRegistryConfig<R>,
    engines: tokio::sync::Mutex<HashMap<(SpaceId, ChannelId), EngineEntry<R>>>,
    // ZEB-271: per-community deferred-spawn queue gated by an explicit
    // CommunityTransactionGuard. See spec §3.1 for the rationale.
    // std::sync::Mutex (not tokio) — critical sections never span an
    // .await, and matching the codebase's NodeState convention.
    pending_transactions: std::sync::Mutex<HashMap<SpaceId, PendingTransaction>>,
    next_tx_id: std::sync::atomic::AtomicU64,
}
```

Update `ChannelLogRegistry::new` (around line 1046) to initialize the new fields:

```rust
pub fn new(config: ChannelLogRegistryConfig<R>) -> Arc<Self> {
    Arc::new(Self {
        config,
        engines: tokio::sync::Mutex::new(HashMap::new()),
        pending_transactions: std::sync::Mutex::new(HashMap::new()),
        next_tx_id: std::sync::atomic::AtomicU64::new(1),
    })
}
```

- [ ] **Step 5: Add `CommunityTransactionGuard` + `begin_transaction` + `commit` + `abort` + `Drop`**

Add the guard as a sibling type to `ChannelLogRegistry`, in the same file. Reads the existing-entry slot atomically under the std lock so reentrancy + tx_id-mismatch are handled coherently.

```rust
/// RAII handle to an open community transaction. Drop without
/// explicit `commit().await` or `abort()` triggers the
/// `tokio::spawn` safety-net abort with a `tracing::warn!` (spec §5.2).
///
/// `tx_id` tags the guard so a stale guard's deferred abort is a no-op
/// after a fresh `begin_transaction(same community_id)` has overwritten
/// the slot (spec §5.4).
pub struct CommunityTransactionGuard<R: tauri::Runtime> {
    registry: Arc<ChannelLogRegistry<R>>,
    community_id: SpaceId,
    tx_id: u64,
    completed: std::sync::atomic::AtomicBool,
}

impl<R: tauri::Runtime> CommunityTransactionGuard<R> {
    /// Drain the queue and fire the deferred spawns sequentially.
    /// Continues attempting all remaining spawns even after an error;
    /// the first error encountered is captured and surfaced as `Err`,
    /// subsequent errors are logged at `warn`. Sets `completed` so
    /// `Drop` skips the safety net.
    pub async fn commit(self) -> Result<(), ChannelLogEngineError> {
        let drained = {
            let mut map = self.registry.pending_transactions.lock().expect(
                "pending_transactions poisoned",
            );
            // tx_id-tag check: only drain if the slot still belongs to
            // this guard (spec §5.4).
            match map.get(&self.community_id) {
                Some(pt) if pt.tx_id == self.tx_id => {
                    let pt = map.remove(&self.community_id).expect("just-checked");
                    pt.queue
                }
                Some(pt) => {
                    tracing::warn!(
                        community_id = ?self.community_id,
                        guard_tx_id = self.tx_id,
                        slot_tx_id = pt.tx_id,
                        "stale CommunityTransactionGuard.commit — slot \
                         was overwritten; no-op"
                    );
                    self.completed
                        .store(true, std::sync::atomic::Ordering::Release);
                    return Ok(());
                }
                None => {
                    // Already aborted (or never queued anything). Treat
                    // as success.
                    self.completed
                        .store(true, std::sync::atomic::Ordering::Release);
                    return Ok(());
                }
            }
        };

        // Lock dropped. Replay each deferred spawn. We invoke a helper
        // that performs the inner-spawn body (everything except the
        // pending_transactions check). On first error, log and continue
        // with remaining items so a single failure doesn't strand the
        // rest, but surface the first error from commit.
        let mut first_err: Option<ChannelLogEngineError> = None;
        for ds in drained {
            match self
                .registry
                .spawn_inner_now(self.community_id, ds)
                .await
            {
                Ok(_) => {}
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    } else {
                        tracing::warn!(
                            community_id = ?self.community_id,
                            "additional deferred-spawn failure during commit drain (ignored, first error already captured)"
                        );
                    }
                }
            }
        }

        self.completed
            .store(true, std::sync::atomic::Ordering::Release);
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Abort the transaction. Discards the queue. Sets `completed` so
    /// `Drop` skips the safety net.
    ///
    /// Sync (not `async`): the body has no `.await` points; callers do
    /// not need to `.await` it. The `self`-by-value receiver still
    /// guarantees the `Drop` safety net is bypassed.
    pub fn abort(self) {
        self.registry
            .abort_transaction_internal(self.community_id, self.tx_id);
        self.completed
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Test-only accessor for the internal `tx_id`. Used by the
    /// reentrancy unit test.
    #[cfg(test)]
    fn tx_id_for_test(&self) -> u64 {
        self.tx_id
    }
}

impl<R: tauri::Runtime> Drop for CommunityTransactionGuard<R> {
    fn drop(&mut self) {
        if !self.completed.load(std::sync::atomic::Ordering::Acquire) {
            tracing::warn!(
                community_id = ?self.community_id,
                tx_id = self.tx_id,
                "CommunityTransactionGuard dropped without commit/abort — \
                 running async safety net"
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

Now add `begin_transaction`, `abort_transaction_internal`, and the test-only accessor `has_pending_transaction_for_test` to the `impl<R: tauri::Runtime> ChannelLogRegistry<R>` block (after `new`):

```rust
    /// Open a community transaction. Subsequent `spawn` calls for this
    /// `community_id` are queued in the transaction's deferred-spawn
    /// list; they fire on `commit().await` and are dropped on
    /// `abort()` or guard drop. See spec §3.2.
    ///
    /// If a transaction for `community_id` is already open,
    /// `begin_transaction` overwrites the slot with a `tracing::warn!`
    /// (spec §5.5); the prior guard's commit/abort becomes a no-op due
    /// to tx_id mismatch.
    pub fn begin_transaction(
        self: &Arc<Self>,
        community_id: SpaceId,
    ) -> CommunityTransactionGuard<R> {
        let tx_id = self
            .next_tx_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        {
            let mut map = self
                .pending_transactions
                .lock()
                .expect("pending_transactions poisoned");
            if let Some(prev) = map.insert(
                community_id,
                PendingTransaction {
                    tx_id,
                    queue: Vec::new(),
                },
            ) {
                tracing::warn!(
                    community_id = ?community_id,
                    prev_tx_id = prev.tx_id,
                    new_tx_id = tx_id,
                    queued = prev.queue.len(),
                    "begin_transaction overwrote an existing pending transaction \
                     (reentrant — see spec §5.5)"
                );
            }
        }
        CommunityTransactionGuard {
            registry: Arc::clone(self),
            community_id,
            tx_id,
            completed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Internal: remove the pending-transaction slot iff `tx_id`
    /// matches. Called from `Drop` (via `tokio::spawn`) and from
    /// `abort()`. tx_id mismatch is a no-op (stale-guard race).
    fn abort_transaction_internal(&self, community_id: SpaceId, tx_id: u64) {
        let mut map = self
            .pending_transactions
            .lock()
            .expect("pending_transactions poisoned");
        match map.get(&community_id) {
            Some(pt) if pt.tx_id == tx_id => {
                map.remove(&community_id);
            }
            Some(pt) => {
                tracing::warn!(
                    community_id = ?community_id,
                    guard_tx_id = tx_id,
                    slot_tx_id = pt.tx_id,
                    "abort_transaction_internal: stale guard, no-op"
                );
            }
            None => {
                // Already gone — fine.
            }
        }
    }

    /// Test-only — `true` if a `PendingTransaction` exists for
    /// `community_id`.
    #[cfg(test)]
    fn has_pending_transaction_for_test(&self, community_id: &SpaceId) -> bool {
        let map = self
            .pending_transactions
            .lock()
            .expect("pending_transactions poisoned");
        map.contains_key(community_id)
    }
```

- [ ] **Step 6: Refactor `ChannelLogRegistry::spawn` into outer + inner**

The existing `spawn` body becomes the inner; add an outer that checks `pending_transactions` and either queues (returning `DeferredForCommit`) or delegates to the inner (returning `Spawned`).

Modify the existing `pub async fn spawn(...)` (line ~1065) to:

```rust
    /// Spawn a per-channel engine + adapter for `(community_id, channel_id)`.
    ///
    /// **ZEB-271 transaction-aware:** if `community_id` has an open
    /// transaction (see `begin_transaction`), the spawn is queued and
    /// fires on `commit().await`. Returns `DeferredForCommit` in that
    /// case; the caller (delta consumer or reconcile) treats both
    /// outcomes as success. See spec §3.3.
    ///
    /// On error the engine + adapter are not registered (the partial
    /// `engines` insert is the commit point); the caller may retry.
    pub async fn spawn(
        self: &Arc<Self>,
        community_id: SpaceId,
        channel_id: ChannelId,
        channel_key: ChannelKey,
        state_at_hlc: Arc<dyn CommunityStateAtHlc + Send + Sync>,
        resolver: Arc<dyn ChannelIdentityResolver + Send + Sync>,
        hlc_tracker: Arc<tokio::sync::Mutex<BTreeMap<String, Hlc>>>,
    ) -> Result<SpawnOutcome<R>, ChannelLogEngineError> {
        // ZEB-271: queue iff an open transaction targets this community.
        // Sync lock — critical section is just a HashMap mutation.
        {
            let mut map = self
                .pending_transactions
                .lock()
                .expect("pending_transactions poisoned");
            if let Some(pt) = map.get_mut(&community_id) {
                pt.queue.push(DeferredSpawn {
                    channel_id,
                    channel_key,
                    state_at_hlc,
                    resolver,
                    hlc_tracker,
                });
                return Ok(SpawnOutcome::DeferredForCommit);
            }
        }

        // No open transaction — fast-path. Do the work and return the
        // engine.
        let ds = DeferredSpawn {
            channel_id,
            channel_key,
            state_at_hlc,
            resolver,
            hlc_tracker,
        };
        let engine = self.spawn_inner_now(community_id, ds).await?;
        Ok(SpawnOutcome::Spawned(engine))
    }

    /// Inner spawn body — the existing pre-ZEB-271 `spawn` content,
    /// minus the parameter list (DeferredSpawn carries everything).
    /// Called both from the fast-path of the outer `spawn` AND from
    /// `CommunityTransactionGuard::commit` to drain the deferred queue.
    async fn spawn_inner_now(
        self: &Arc<Self>,
        community_id: SpaceId,
        ds: DeferredSpawn,
    ) -> Result<Arc<ChannelLogEngine<R>>, ChannelLogEngineError> {
        // < move the EXISTING spawn body here verbatim, replacing
        //   parameter references:
        //     channel_id           → ds.channel_id
        //     channel_key          → ds.channel_key
        //     state_at_hlc         → ds.state_at_hlc
        //     resolver             → ds.resolver
        //     hlc_tracker          → ds.hlc_tracker
        // >
        let key = (community_id, ds.channel_id);

        // Cheap pre-check under the engines lock — returns Arc-cloned
        // existing engine on the duplicate path so we skip dir-creation,
        // engine construction, adapter spawn, and the second insert.
        {
            let engines = self.engines.lock().await;
            if let Some(existing) = engines.get(&key) {
                return Ok(Arc::clone(&existing.engine));
            }
        }

        let community_id_hex = hex::encode(community_id.0);
        let channel_id_hex = hex::encode(ds.channel_id.0);
        let root_dir = self
            .config
            .identity_dir
            .join("communities")
            .join(&community_id_hex)
            .join("channels")
            .join(&channel_id_hex);
        std::fs::create_dir_all(&root_dir).map_err(|e| {
            ChannelLogEngineError::Persist(ChannelLogPersistError::Io(e.to_string()))
        })?;

        let (publisher_tx, publisher_rx) = mpsc::channel::<Vec<u8>>(64);
        let (subscriber_tx, subscriber_rx) = mpsc::channel::<Vec<u8>>(64);
        let (query_request_tx, query_request_rx) = mpsc::channel::<BackfillQueryRequest>(8);

        let params = ChannelLogEngineParams {
            community_id,
            channel_id: ds.channel_id,
            channel_key: Arc::new(ds.channel_key),
            root_dir,
            state_at_hlc: ds.state_at_hlc,
            resolver: ds.resolver,
            self_owner: self.config.self_owner,
            self_device_id: self.config.self_device_id.clone(),
            signing_key: Arc::clone(&self.config.signing_key),
            hlc_tracker: ds.hlc_tracker,
            app: self.config.app.clone(),
            config: self.config.engine_config.clone(),
            publisher_tx,
            subscriber_rx,
            query_request_tx,
        };
        let engine = ChannelLogEngine::new(params).await?;

        // < remainder of the existing spawn body verbatim: read_for_query
        //   construction, closing AtomicBool, second engines.lock check,
        //   adapter request send. Returns Ok(Arc::clone(&engine)) at the
        //   end. >
        // ... [verbatim rest of pre-ZEB-271 spawn body] ...

        Ok(engine)
    }
```

The intent: zero-content change inside `spawn_inner_now` — just relocate the existing body and rename `channel_id`/`channel_key`/etc. to `ds.field` references. The outer `spawn` does only the transaction-check + dispatch.

- [ ] **Step 7: Update `RegistryFixture::spawn_under_fixture` helper**

The existing helper (line ~2540) returns `Arc<ChannelLogEngine<MockRuntime>>` directly. Update it to handle the new SpawnOutcome:

```rust
    async fn spawn_under_fixture(
        fix: &RegistryFixture,
        community_id: SpaceId,
        channel_id: ChannelId,
    ) -> Arc<ChannelLogEngine<tauri::test::MockRuntime>> {
        let key = derive_channel_key(&fix.membership_key, &community_id, &channel_id);
        match Arc::clone(&fix.registry)
            .spawn(
                community_id,
                channel_id,
                key,
                Arc::clone(&fix.state) as Arc<dyn CommunityStateAtHlc + Send + Sync>,
                Arc::clone(&fix.resolver) as Arc<dyn ChannelIdentityResolver + Send + Sync>,
                Arc::clone(&fix.hlc_tracker),
            )
            .await
            .expect("spawn")
        {
            SpawnOutcome::Spawned(engine) => engine,
            SpawnOutcome::DeferredForCommit => {
                panic!("spawn_under_fixture used during a transaction; tests \
                        that exercise transactions should call spawn() directly")
            }
        }
    }
```

- [ ] **Step 8: Run tests until all 8 pass**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_channel_log_engine)'
```

Expected: existing tests still pass + 8 new `tx_*` tests pass.

If the existing tests (`registry_*`) fail because the `spawn_under_fixture` helper now panics on `DeferredForCommit` — they shouldn't, since they don't open transactions, and the early-return-on-existing-engine path inside `spawn_inner_now` returns `Spawned` (no transaction).

If `tx_dropped_guard_safety_net_aborts` is flaky — increase the `tokio::time::sleep` to 100ms, or use `tokio::task::yield_now().await` in a loop.

- [ ] **Step 9: Run full local CI gate**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast
cd src-tauri && cargo check --locked --all-targets --features test-fixtures
```

All four commands must exit 0. The frontend gates are unaffected by this task; skip them until Task 5.

- [ ] **Step 10: Commit**

```bash
git add src-tauri/src/community_channel_log_engine.rs
git commit -m "$(cat <<'EOF'
feat(zeb-271): ChannelLogRegistry transaction primitives

Adds CommunityTransactionGuard, begin_transaction/commit/abort/Drop, and
the SpawnOutcome enum so per-channel spawns can be deferred until a
durable-commit signal. ChannelLogRegistry.spawn checks the
pending_transactions map: if an entry exists, the spawn is queued; on
commit the queue is drained and the inner spawn body fires for each.
tx_id-tagged guards close the stale-abort-clobbers-fresh-tx race for
redeem_invite_inner retries.

Eight new unit tests cover begin/commit/abort/drop, stale-guard no-op,
reentrancy, in-order drain, and partial-failure continuation. Existing
spawn callers are migrated to the new SpawnOutcome variant in Task 2;
this commit ships the API + tests only.

Spec: docs/specs/2026-05-10-zeb-271-channel-log-registry-transactionality-design.md
EOF
)"
```

---

## Task 2: Migrate existing spawn callers to SpawnOutcome

**Files:**

- Modify: `src-tauri/src/lib.rs` (delta consumer's 3rd callback at line ~1490+)
- Modify: `src-tauri/src/community_channel_log_engine.rs` (`reconcile_from_state` body)

**Goal:** Update the two production callers of `ChannelLogRegistry::spawn` to handle the new `SpawnOutcome` return type. Both callers run outside any transaction in the current state of the world (Task 4/5 wires `begin_transaction` for `_inner` paths), so both currently expect `Spawned`. Migration is mechanical.

- [ ] **Step 1: Locate the delta consumer's 3rd callback**

```bash
grep -n "registry.spawn\|registry\.spawn(\|\.spawn(\s*\?" src-tauri/src/lib.rs | grep -v "//" | head -10
```

Identify the exact callsite — should be around `lib.rs:1490+` inside the `tokio::spawn(run_community_delta_consumer(` block.

- [ ] **Step 2: Update the callback to handle SpawnOutcome**

The callback currently calls `registry.spawn(...).await` and matches on `Err` only (the `Ok(Arc<engine>)` return value is discarded). Update to match on the `SpawnOutcome` variant:

```rust
// inside the third closure passed to run_community_delta_consumer:
move |payload: ChannelConfigChangePayload| {
    let registry = Arc::clone(&channel_log_registry_for_consumer);
    // ... existing setup of state_at_hlc, resolver, hlc_tracker, etc. ...
    async move {
        match payload.action {
            ChannelConfigChangeAction::Created => {
                match registry
                    .spawn(
                        payload.community_id,
                        payload.channel_id,
                        channel_key,
                        state_at_hlc,
                        resolver,
                        hlc_tracker,
                    )
                    .await
                {
                    Ok(crate::community_channel_log_engine::SpawnOutcome::Spawned(_)) => {
                        // Engine immediately available — no transaction in flight.
                    }
                    Ok(crate::community_channel_log_engine::SpawnOutcome::DeferredForCommit) => {
                        // Deferred until create_community_inner / redeem_invite_inner
                        // commits its transaction. Treated as success.
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = ?e,
                            community_id = ?payload.community_id,
                            channel_id = ?payload.channel_id,
                            "registry.spawn failed for ChannelCreate event"
                        );
                    }
                }
            }
            ChannelConfigChangeAction::Modified => {
                // existing no-op
            }
            ChannelConfigChangeAction::Deleted => {
                // existing registry.stop call
            }
        }
    }
}
```

The exact existing match shape may differ slightly — the **goal** is: `Ok(Arc<_>)` returns become `Ok(SpawnOutcome::Spawned(_))` and a new `Ok(SpawnOutcome::DeferredForCommit)` arm is added that is also success.

- [ ] **Step 3: Update `reconcile_from_state` to handle SpawnOutcome**

Locate `reconcile_from_state` (line ~1383 in `community_channel_log_engine.rs`). It calls `self.spawn(...)` for each channel discovered in the state CRDT. Update each call site:

```rust
match self
    .spawn(community_id, channel_id, key, state_at_hlc, resolver, hlc_tracker)
    .await
{
    Ok(SpawnOutcome::Spawned(_)) => { /* existing success path */ }
    Ok(SpawnOutcome::DeferredForCommit) => {
        // reconcile_from_state runs at start_node init, outside any
        // transaction. DeferredForCommit here would mean someone left
        // a stale transaction in pending_transactions across an app
        // restart, which is impossible (the state is in-memory).
        unreachable!(
            "reconcile_from_state must run outside any transaction; \
             a deferred spawn here is a bug"
        );
    }
    Err(e) => { /* existing error path */ }
}
```

- [ ] **Step 4: Verify all existing tests still pass**

```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

No new tests; the existing tests act as regression coverage. Both `reconcile_from_state` tests (`registry_reconcile_*`) MUST pass since they exercise the migrated path.

- [ ] **Step 5: Run local CI gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo check --locked --all-targets --features test-fixtures
```

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/community_channel_log_engine.rs
git commit -m "$(cat <<'EOF'
refactor(zeb-271): migrate spawn callers to SpawnOutcome

Two production callers of ChannelLogRegistry::spawn — the delta
consumer's 3rd callback (lib.rs) and reconcile_from_state — now
match on SpawnOutcome::Spawned vs DeferredForCommit. Both currently
run outside any transaction, so DeferredForCommit is unexpected
(reconcile asserts unreachable; the consumer treats it as success
in anticipation of the wiring in Tasks 3 + 4).

No behavior change in this commit; transaction wiring lands in
subsequent tasks.
EOF
)"
```

---

## Task 3: create_community_inner threading + transaction wiring

**Files:**

- Modify: `src-tauri/src/lib.rs`
  - `create_community_inner` (line 7184) — add R generic + `channel_log_registry` param + begin_transaction + commit
  - `create_community` IPC handler (line 7598) — snapshot `channel_log_registry` from NodeState
  - `create_community_inner_tests` (line 7686+) — mechanical fixture updates + 3 new failure-path tests

**Goal:** Plumb the channel-log registry handle through `create_community_inner` and wire `begin_transaction`/`tx.commit()` so a phantom default-#general spawn is impossible.

- [ ] **Step 1: Write the 3 new failure-path tests (failing scaffolds)**

Add to `create_community_inner_tests` (the existing `mod create_community_inner_tests` at line ~7687). The test fixture builder (likely `setup_node_state_for_create_community` or similar — search the existing module for the helper) needs an extra arg for the ChannelLogRegistry. Add the helper extension first, then the 3 tests.

```rust
    // ZEB-271: failure-path coverage that the channel-log registry
    // does NOT leak a phantom engine when create_community_inner
    // aborts after the default #general ChannelCreate insert. See
    // spec §7.2.

    #[tokio::test]
    async fn happy_path_spawns_default_channel_engine() {
        // Extend the existing happy-path test (whatever it's called)
        // by ALSO asserting that fixture.channel_log_registry has
        // the #general engine after create_community_inner returns Ok.
        // The actual default channel_id is generated inside
        // create_community_inner; tests look up by community_id and
        // assert exactly one engine exists for it.
        let fixture = build_create_community_test_fixture().await;
        let community_id_hex = create_community_inner(
            "test-community".to_string(),
            /* is_invite_only */ false,
            // ... existing args ...
            Arc::clone(&fixture.channel_log_registry),
            &fixture.node_state,
        )
        .await
        .expect("create_community_inner happy path");

        let community_id = parse_community_id(&community_id_hex);
        let engines_for_community = fixture
            .channel_log_registry
            .engines_for_community_for_test(&community_id)
            .await;
        assert_eq!(
            engines_for_community.len(),
            1,
            "happy path must leave exactly one channel-log engine (the default #general)"
        );
    }

    #[tokio::test]
    async fn apply_space_rejected_no_channel_log_leak() {
        // Construct a crdt_state where apply_space rejects (e.g.,
        // pre-apply a Space row with the same id). Verify the registry
        // has no engines for the would-be community after
        // create_community_inner returns Err.
        let fixture = build_create_community_test_fixture().await;
        // Inject a conflicting Space row first.
        // ... (test helper that pre-poisons the crdt_state) ...
        let result = create_community_inner(
            "rejected-community".to_string(),
            false,
            // ... existing args ...
            Arc::clone(&fixture.channel_log_registry),
            &fixture.node_state,
        )
        .await;
        assert!(result.is_err(), "create_community_inner must Err on apply_space rejection");

        // Allow the dropped CommunityTransactionGuard's safety-net
        // tokio::spawn to run.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let leaked = fixture.channel_log_registry.engines_count_for_test().await;
        assert_eq!(
            leaked, 0,
            "no channel-log engines leaked after apply_space rejection"
        );
    }

    #[tokio::test]
    async fn fence_generation_changed_no_channel_log_leak() {
        let fixture = build_create_community_test_fixture().await;
        let snapshot_generation = fixture.snapshot_generation();
        // Bump generation to force fence-abort.
        fixture.bump_node_state_generation();

        let result = create_community_inner(
            "fence-aborted-community".to_string(),
            false,
            // ... existing args using snapshot_generation ...
            Arc::clone(&fixture.channel_log_registry),
            &fixture.node_state,
        )
        .await;
        assert!(result.is_err(), "fence change must abort the create");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let leaked = fixture.channel_log_registry.engines_count_for_test().await;
        assert_eq!(
            leaked, 0,
            "no channel-log engines leaked after fence-aborted create"
        );
    }
```

The test refs `engines_for_community_for_test` and `engines_count_for_test` — small test-only accessors on `ChannelLogRegistry` that the existing `engine` method shape can be extended into. Add them to the registry impl (test-only):

```rust
    #[cfg(test)]
    pub async fn engines_count_for_test(&self) -> usize {
        let engines = self.engines.lock().await;
        engines.len()
    }

    #[cfg(test)]
    pub async fn engines_for_community_for_test(
        &self,
        community_id: &SpaceId,
    ) -> Vec<(ChannelId, Arc<ChannelLogEngine<R>>)> {
        let engines = self.engines.lock().await;
        engines
            .iter()
            .filter(|((cid, _), _)| cid == community_id)
            .map(|((_, chid), entry)| (*chid, Arc::clone(&entry.engine)))
            .collect()
    }
```

- [ ] **Step 2: Run the new tests to confirm they fail (compile errors fine)**

```bash
cd src-tauri && cargo nextest list --features test-fixtures 2>&1 | grep -E "create_community_inner_tests|^error" | head -20
```

Expected errors: `create_community_inner` argument count mismatch, missing fixture fields/methods.

- [ ] **Step 3: Add `<R: tauri::Runtime>` generic + `channel_log_registry` param to `create_community_inner`**

Modify the function signature (line 7184). Place `channel_log_registry` second-to-last (just before `node_state`):

```rust
#[allow(clippy::too_many_arguments)]
pub async fn create_community_inner<R: tauri::Runtime>(
    name: String,
    is_invite_only: bool,
    crdt_state: std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    hlc_tracker: std::sync::Arc<
        tokio::sync::Mutex<std::collections::BTreeMap<String, crate::owner_state_types::Hlc>>,
    >,
    device_id: String,
    self_owner: crate::owner_state_types::OwnerAddr,
    signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    community_registry: std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
    community_adapter_tx: tokio::sync::mpsc::Sender<crate::event_loop::CommunityAdapterRequest>,
    channel_log_registry: std::sync::Arc<crate::community_channel_log_engine::ChannelLogRegistry<R>>,
    snapshot_generation: u64,
    node_state: &std::sync::Mutex<NodeState>,
) -> Result<String, String> {
    // ... existing body ...
}
```

- [ ] **Step 4: Add `begin_transaction` call BEFORE `community_registry.spawn_engine`**

Insert immediately before `community_registry.spawn_engine` (currently line ~7258):

```rust
    // ZEB-271: open a channel-log transaction so any ChannelCreate
    // events that materialize during this critical section are queued
    // and only fire on commit. Drop on early-return triggers the
    // safety-net abort. See spec §3-§5.
    let channel_log_tx = channel_log_registry.begin_transaction(minted.community_id);

    let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    // ... existing community_registry.spawn_engine + body ...
```

- [ ] **Step 5: Add `tx.commit().await?` before final `Ok(...)`**

Replace the final `Ok(hex::encode(minted.community_id.0))` (line ~7579) with:

```rust
    // ZEB-271: durable commit reached. Drain the deferred-spawn queue
    // and fire the real ChannelLogRegistry::spawn for each queued
    // channel-create that materialized during this critical section.
    //
    // Log-and-continue (NOT `?`): apply_space has already durably
    // committed the community by this point. Returning Err here would
    // tell the caller the create failed, but the community is real;
    // the user's retry would mint a duplicate. So a commit() failure
    // is treated as recovery deferred to the next start_node, which
    // re-runs reconcile_from_state. (CodeRabbit Major round 1.)
    if let Err(e) = channel_log_tx.commit().await {
        tracing::warn!(
            community_id = %hex::encode(minted.community_id.0),
            error = %e,
            "channel_log_registry commit failed after durable community create; \
             pending channel-log spawns will be re-attempted via reconcile_from_state \
             on next start_node"
        );
    }

    Ok(hex::encode(minted.community_id.0))
}
```

- [ ] **Step 6: Update `create_community` IPC handler — snapshot the registry**

Modify the snapshot tuple (line 7610). The current 8-tuple becomes a 9-tuple with `channel_log_registry`:

```rust
    let (
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        community_registry,
        community_adapter_tx,
        channel_log_registry,
        dm_outbox,
        snapshot_generation,
    ) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.community_adapter_request_tx
                .clone()
                .ok_or("community_adapter_request_tx missing")?,
            g.channel_log_registry
                .clone()
                .ok_or("channel_log_registry missing — node not running?")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.generation,
        )
    };
```

Pass `channel_log_registry` to `create_community_inner` in the new positional slot:

```rust
    let community_id = create_community_inner(
        name,
        is_invite_only,
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        signing_key,
        community_registry,
        community_adapter_tx,
        channel_log_registry,
        snapshot_generation,
        &state_lock,
    )
    .await?;
```

- [ ] **Step 7: Update `create_community_inner_tests` mechanical fixture passes**

Every existing test in this module (likely 5-10 tests covering happy path, adapter dispatch failure, bootstrap-Join failure, default-channel failure, fence path, apply_space-rejected) needs:

1. The fixture builder to construct a `ChannelLogRegistry<MockRuntime>` and store it as `fixture.channel_log_registry`
2. Each test's call to `create_community_inner` to pass `Arc::clone(&fixture.channel_log_registry)` in the new positional slot

Reuse `build_registry_fixture` from `community_channel_log_engine::tests` — the fixture there returns a `RegistryFixture` with a `registry: Arc<ChannelLogRegistry<MockRuntime>>` field. Either:
- Extract `build_registry_fixture` to a shared module so `create_community_inner_tests` can call it, OR
- Duplicate the registry-construction code inline in the test fixture builder (pragmatic if the existing fixture is only used in one place).

Recommend extraction: move `build_registry_fixture` (and `RegistryFixture`) to a new `tests/common/channel_log_registry_fixture.rs` shared helper if the integration test side wants to use it; OR keep it inline in `community_channel_log_engine::tests` and have `create_community_inner_tests`'s fixture builder construct a registry directly using the same recipe (~30L of duplicate setup, but contained). Go with **inline duplicate** unless the type system forces the extraction (less code-graph churn).

- [ ] **Step 8: Run all `create_community_inner_tests` until passing**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(create_community_inner)'
```

Expected: every existing test passes (after fixture updates) + the 3 new tests pass.

- [ ] **Step 9: Run full local CI gate**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast
cd src-tauri && cargo check --locked --all-targets --features test-fixtures
```

- [ ] **Step 10: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/community_channel_log_engine.rs
git commit -m "$(cat <<'EOF'
feat(zeb-271): create_community_inner channel-log transaction

Threads ChannelLogRegistry through create_community_inner and wires
begin_transaction (before community_registry.spawn_engine) +
tx.commit() (after apply_space). Drop on early-return triggers the
safety-net abort, eliminating the phantom default-#general engine
that the prior shape leaked on fence/apply_space failure.

Three new failure-path tests assert engines map empty after rollback;
existing tests get mechanical fixture updates to construct + pass a
ChannelLogRegistry<MockRuntime>.

create_community_inner becomes <R: tauri::Runtime> generic so tests
use MockRuntime while production uses Wry; bodies are R-agnostic.

Spec §4.1, §6.2-§6.4, §7.2.
EOF
)"
```

---

## Task 4: redeem_invite_inner threading + transaction wiring

**Files:**

- Modify: `src-tauri/src/lib.rs`
  - `redeem_invite_inner` (line 7936) — add R generic + `channel_log_registry` param + begin_transaction + commit
  - `redeem_invite` IPC handler (line 8670) — snapshot `channel_log_registry` (9-tuple → 10-tuple)
  - `redeem_invite_inner_tests` (line 8790+) — mechanical fixture updates + 1 new test

**Goal:** Same shape as Task 3, applied to `redeem_invite_inner`. Smaller scope because invite-redemption doesn't mint a default ChannelCreate — the transaction protects against remote sync events that arrive between spawn-engine and apply_space.

- [ ] **Step 1: Write the new test (`happy_path_no_pending_transaction_after_success`)**

Add to `redeem_invite_inner_tests`:

```rust
    #[tokio::test]
    async fn happy_path_no_pending_transaction_after_success() {
        let fixture = build_redeem_invite_test_fixture().await;
        // ... existing happy-path setup: build a valid OPEN-community
        //     invite URL, prime the joiner's NodeState, etc. ...

        let result = redeem_invite_inner(
            invite_url,
            // ... existing args ...
            Arc::clone(&fixture.channel_log_registry),
            // ... fence_check etc. ...
        )
        .await;
        assert!(result.is_ok(), "redeem_invite_inner happy path");

        // Proxy assertion for "tx was committed": no pending entry
        // remaining for this community_id. (Spec §7.3 — full Zenoh-
        // driven failure-path coverage deferred.)
        let community_id = parse_community_id_from(&result.unwrap());
        assert!(
            !fixture.channel_log_registry.has_pending_transaction_for_test(&community_id),
            "happy path must commit the transaction (no lingering entry)"
        );
    }
```

- [ ] **Step 2: Confirm compile failures**

```bash
cd src-tauri && cargo nextest list --features test-fixtures 2>&1 | grep redeem_invite | head -10
```

- [ ] **Step 3: Add `<R: tauri::Runtime>` generic + `channel_log_registry` param**

Modify `redeem_invite_inner` (line 7936):

```rust
pub async fn redeem_invite_inner<R: tauri::Runtime, F>(
    url: String,
    crdt_state: std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    hlc_tracker: std::sync::Arc<
        tokio::sync::Mutex<std::collections::BTreeMap<String, crate::owner_state_types::Hlc>>,
    >,
    device_id: String,
    self_owner: crate::owner_state_types::OwnerAddr,
    signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    community_registry: std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
    community_adapter_tx: tokio::sync::mpsc::Sender<crate::event_loop::CommunityAdapterRequest>,
    unicast_send_tx: tokio::sync::mpsc::Sender<crate::dm_outbox::UnicastSendRequest>,
    dm_outbox: std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>,
    channel_log_registry: std::sync::Arc<crate::community_channel_log_engine::ChannelLogRegistry<R>>,
    fence_check: F,
) -> Result<RedeemInviteResultDto, String>
where
    F: Fn() -> Result<(), String>,
{
    // ... existing body ...
}
```

- [ ] **Step 4: Add `begin_transaction` call right after `mint_redemption`**

After line ~7969 (`let minted = mint_redemption(...);`):

```rust
    let minted = mint_redemption(&payload, self_owner, signing_key.as_ref(), join_hlc)?;

    // ZEB-271: open a channel-log transaction. Protects against remote
    // ChannelCreate events that arrive via Zenoh sync between
    // spawn_engine and apply_space — they're queued, only fire on
    // commit. See spec §4.2.
    let channel_log_tx = channel_log_registry.begin_transaction(minted.community_id);

    // ZEB-267 (replaces the prior ZEB-258 comment): ...
```

- [ ] **Step 5: Add `tx.commit().await?` before final `Ok(...)`**

Locate the final `Ok(RedeemInviteResultDto { ... })` in `redeem_invite_inner` (ending around line 8170 — search the file for the actual end). Replace with:

```rust
    // ZEB-271: durable commit reached. Drain any queued channel-log
    // spawns that the engine sync surfaced during this redemption.
    //
    // Log-and-continue (NOT `?`): the redemption Space is already
    // durable. Returning Err would tell the UI the invite failed even
    // though the user already joined; the OPEN-invite retry path is
    // explicitly non-idempotent and would append a second self-Join.
    // (CodeRabbit Major round 1.)
    if let Err(e) = channel_log_tx.commit().await {
        tracing::warn!(
            community_id = %hex::encode(community_id.0),
            error = %e,
            "channel_log_registry commit failed after durable invite redemption; \
             pending channel-log spawns will be re-attempted via reconcile_from_state \
             on next start_node"
        );
    }

    Ok(RedeemInviteResultDto { /* existing fields */ })
}
```

- [ ] **Step 6: Update `redeem_invite` IPC handler**

Modify the snapshot tuple (line ~8679) to include `channel_log_registry`:

```rust
    let (
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        community_registry,
        community_adapter_tx,
        unicast_send_tx,
        channel_log_registry,
        dm_outbox,
        snapshot_generation,
    ) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.community_adapter_request_tx
                .clone()
                .ok_or("community_adapter_request_tx missing")?,
            g.unicast_send_tx
                .clone()
                .ok_or("unicast_send_tx missing — no owner identity?")?,
            g.channel_log_registry
                .clone()
                .ok_or("channel_log_registry missing — node not running?")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.generation,
        )
    };
```

Pass `channel_log_registry` in the new positional slot to `redeem_invite_inner`:

```rust
    let dto = redeem_invite_inner(
        url,
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        signing_key,
        community_registry,
        community_adapter_tx,
        unicast_send_tx,
        dm_outbox,
        channel_log_registry,
        fence_check,
    )
    .await?;
```

- [ ] **Step 7: Update `redeem_invite_inner_tests` mechanical fixture passes**

Each existing test in `redeem_invite_inner_tests` needs:
1. Fixture builder constructs a `ChannelLogRegistry<MockRuntime>`.
2. Each test passes it to `redeem_invite_inner`.

Reuse the same registry-construction recipe used in `create_community_inner_tests` (Task 3 Step 7).

- [ ] **Step 8: Run all `redeem_invite_inner_tests` until passing**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(redeem_invite_inner)'
```

- [ ] **Step 9: Run full local CI gate**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast
cd src-tauri && cargo check --locked --all-targets --features test-fixtures
```

- [ ] **Step 10: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-271): redeem_invite_inner channel-log transaction

Threads ChannelLogRegistry through redeem_invite_inner and wires
begin_transaction (after mint_redemption) + tx.commit() (before final
Ok). Protects against remote ChannelCreate events that arrive via
Zenoh sync between community_registry.spawn_engine and apply_space —
they're queued, only fire on commit, dropped on rollback.

Existing tests get mechanical fixture updates; one new test asserts no
lingering pending transaction after happy-path success (proxy for
"commit was called"). Failure-path coverage is delegated to the
registry-level protocol tests in Task 1; full Zenoh-driven scenario
deferred per spec §7.3.

Spec §4.2, §6.2-§6.4, §7.3.
EOF
)"
```

---

## Task 5: Final verification + push + PR

**Files:** none modified; verification + git operations only.

**Goal:** Run every CI gate locally, push the branch, open PR #99 (or whatever the next PR number is) with the required body shape.

- [ ] **Step 1: Final cargo gates from `src-tauri/`**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast
cd src-tauri && cargo check --locked --all-targets --features test-fixtures
```

All four must exit 0.

- [ ] **Step 2: Frontend gates from repo root**

```bash
npx tsc --noEmit
npx vitest run
```

Both must exit 0. (Run from the repo root. The frontend is unchanged but the gates run anyway as a regression sanity check.)

- [ ] **Step 3: Verify branch state**

```bash
git log --oneline origin/main..HEAD
# Expected commits (top to bottom):
#   <sha> feat(zeb-271): redeem_invite_inner channel-log transaction
#   <sha> feat(zeb-271): create_community_inner channel-log transaction
#   <sha> refactor(zeb-271): migrate spawn callers to SpawnOutcome
#   <sha> feat(zeb-271): ChannelLogRegistry transaction primitives
#   7c65e05 docs(zeb-271): use std::sync::Mutex (codebase convention) instead of parking_lot
#   e12f9a9 docs(zeb-271): channel-log registry transactionality design spec
git status
# Expected: clean.
```

- [ ] **Step 4: Push branch**

```bash
git push -u origin zeb-271-channel-log-registry-transactionality
```

- [ ] **Step 5: Create PR**

```bash
gh pr create --title "ZEB-271: channel-log registry transactionality" --body "$(cat <<'EOF'
## Summary

Adds a `CommunityTransactionGuard` primitive to `ChannelLogRegistry` that defers per-channel spawns until a durable-commit signal, eliminating the phantom-engine leak when `create_community_inner` or `redeem_invite_inner` aborts after inserting `ChannelCreate` events.

* `ChannelLogRegistry::spawn` now returns `SpawnOutcome<R>`: either `Spawned(Arc<Engine>)` for the existing fast-path (no transaction open) or `DeferredForCommit` (a transaction is open; spawn is queued).
* `begin_transaction(community_id) → CommunityTransactionGuard` opens a per-community deferred-spawn queue. `commit().await` drains the queue and fires the real spawns; `abort()` (sync) discards them. `Drop` runs a safety-net abort via `Handle::try_current()` with a `tracing::warn!` (sync fallback if no runtime).
* `tx_id`-tagged guards close the stale-abort-clobbers-fresh-tx race for `redeem_invite_inner` retries.
* `create_community_inner` and `redeem_invite_inner` open transactions before `community_registry.spawn_engine` and commit after `apply_space`. All early-return rollback paths now auto-abort via the dropped guard.

Resolves ZEB-271 (deferred from PR #96 round-2 review at `lib.rs:1606`).

## Test plan

- [x] 8 new unit tests in `community_channel_log_engine::tests` exercise the protocol (begin/commit/abort, drop safety net, stale-guard no-op, reentrancy, drain-in-order, partial-failure continuation)
- [x] 3 new failure-path integration tests in `create_community_inner_tests` assert `engines` map empty after `apply_space`-rejected and fence-generation-changed rollbacks; happy-path test extended to assert the default `#general` engine is present after success
- [x] 1 new test in `redeem_invite_inner_tests` asserts no lingering pending transaction after happy-path success
- [x] All existing tests in both `_inner_tests` modules updated mechanically with the new fixture (no semantic changes)
- [x] `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`, `cargo check --locked --all-targets --features test-fixtures` (MSRV proxy) all green locally
- [x] `npx tsc --noEmit` + `npx vitest run` green (frontend unchanged but gates run)

## References

* Linear: ZEB-271
* Parent epic: [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) Sub-C v2 — channels-within-communities (DONE 2026-05-10)
* Spec: `docs/specs/2026-05-10-zeb-271-channel-log-registry-transactionality-design.md`
* Plan: `docs/plans/2026-05-10-zeb-271-channel-log-registry-transactionality-plan.md`
* Phase 3 PR (where deferred from): #96
* CodeRabbit comment that surfaced this: PR #96 round-2 review at `lib.rs:1606`

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

**Note:** Per the `feedback_linear_pr_auto_close` memory rule, the parent ZEB-248 reference is a markdown-linked URL `[ZEB-248](https://linear.app/...)` so Linear's gh-integration does NOT cascade-close it (ZEB-248 is already closed; cascading is moot here, but the discipline is consistent with prior PRs). The phase ticket itself (ZEB-271) appears in the PR title as a bare token — the merge will close it correctly.

- [ ] **Step 6: Capture PR URL**

The `gh pr create` output ends with the PR URL. Note it for the autonomous-PR-monitoring loop kickoff.

- [ ] **Step 7: Stop and return control**

Per `feedback_autonomous_pr_monitoring_loop`, the calling agent enters the bot-review/CI loop after PR creation. **Do not push fixups in this task.** The autonomous loop's first wakeup will fetch bot reviews, dispatch fixup subagents, etc.

---

## Acceptance criteria recap (from spec §11)

1. ✅ Decision (a) selected: defer-spawn via commit signal, queue lives in `ChannelLogRegistry`. Approach (b) and (c) rejected.
2. Implementation (after Tasks 1-4 land):
   - ✅ `CommunityTransactionGuard` with `begin_transaction` / `commit` / `abort` / `Drop` (Task 1)
   - ✅ `create_community_inner` updated (Task 3)
   - ✅ `redeem_invite_inner` updated (Task 4)
   - ✅ All registry-level unit tests passing (Task 1)
   - ✅ All `_inner` integration tests passing (Tasks 3 + 4)
3. (n/a — approach (a) selected)
4. ✅ Same approach applied to `redeem_invite_inner` (Task 4)
