# ZEB-274 Membership-Side Transactionality Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace 26 scattered `shutdown_engine_and_cleanup_persistence` rollback sites in `create_community_inner` + `redeem_invite_inner` with a single RAII rollback guard (`CommunitySyncSpawnGuard`) on `CommunitySyncRegistry`.

**Architecture:** RAII guard pattern — eager spawn (preserves IPC handler's pre-commit `engine.insert_local_event` flow), with `Drop`/`abort()` running the async `shutdown_engine_and_cleanup_persistence` cleanup. The guard internalizes the freshness flag (formerly the `Result<bool, _>` from `spawn_engine`), so concurrent-redeem race losers' guards are no-ops on Drop. Per spec §2, this is structurally different from ZEB-271's deferred-spawn pattern because the IPC handler must call `engine.insert_local_event(bootstrap_join)` BEFORE commit.

**Tech Stack:** Rust 2021, tokio async runtime, `std::sync::atomic::AtomicBool` for `completed` flag (mirrors ZEB-271), `tokio::runtime::Handle::try_current()` for async-context Drop with sync-fallback (logs warn + accepts leak per spec §10.2 — no sync alternative for `engine.shutdown().await`).

---

## File Structure

| File | Role | Change |
|---|---|---|
| `src-tauri/src/community_state_sync.rs` | Owner of `CommunitySyncRegistry` + new `CommunitySyncSpawnGuard` primitive | Add: guard struct + 4 methods + Drop impl + new `spawn_engine_with_guard` + 5 unit tests. Modify: `spawn_engine` becomes the internal helper (renamed → `spawn_engine_inner_now`). |
| `src-tauri/src/lib.rs` | IPC handlers | Modify: `create_community_inner` (collapse 9 rollback sites; replace `engine_arc().await.ok_or(...)` with engine handle from guard). Modify: `redeem_invite_inner` (collapse 17 rollback sites; remove `engine_freshly_created` local; replace bool-gated rollbacks). |
| `src-tauri/src/lib.rs` (tests submodule) | Unit-test fixture for `_inner` functions | Add: 4 integration tests per spec §7.2 + §7.3 (apply_space rejection cleanup; channel_log commit failure survival; concurrent-redeem race loser no-op; redeem apply_space rejection cleanup). |
| `src-tauri/tests/community_invite_only_integration.rs` | End-to-end redemption integration tests | Modify: existing fixtures pass new `community_adapter_tx` arg through `spawn_engine_with_guard` (or use a fixture that bypasses adapter dispatch). |
| `src-tauri/tests/community_sync_integration.rs` | End-to-end community-sync integration tests | Modify: existing fixtures adapted for new `spawn_engine_with_guard` signature (test code that calls registry directly may need to mint a fake `community_adapter_tx`). |
| `docs/specs/2026-05-10-zeb-271-channel-log-registry-transactionality-design.md` | Sibling ZEB-271 spec | Modify line 269: replace "same shape, separate ticket" with RESOLVED note pointing to ZEB-274 + architecture-differs reasoning. |

---

## Task 0: Pre-flight + green-baseline confirm

**Files:** None (verification only).

**Goal:** Confirm all 5 CI gates green on the just-cut branch (`zeb-274-membership-side-transactionality` at HEAD `74d5bc9` after spec-amend) so any later red is unambiguously caused by THIS work, not pre-existing drift.

- [ ] **Step 1: Confirm branch + clean working tree**

```bash
git status
git log --oneline -3
```

Expected: `On branch zeb-274-membership-side-transactionality` + working tree clean. HEAD shows the spec commit (sha will start with one of `74d5bc9` or whatever the amend produced).

- [ ] **Step 2: cargo fmt check**

```bash
cd src-tauri && cargo fmt --all -- --check
```

Expected: no output, exit 0.

- [ ] **Step 3: cargo clippy**

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```

Expected: `Finished` line, no warnings, exit 0.

- [ ] **Step 4: cargo nextest**

```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast
```

Expected: `957 tests run: 957 passed, 2 skipped` (matches ZEB-271 PR #99 final tally).

- [ ] **Step 5: cargo check (msrv proxy)**

```bash
cd src-tauri && cargo check --locked --all-targets --features test-fixtures
```

Expected: `Finished` line, no errors, exit 0.

- [ ] **Step 6: frontend gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit
cd /Users/zeblith/work/zeblithic/harmony-client && npx vitest run
```

Expected: tsc no output exit 0; vitest summary "Tests passed" with no failures.

(NOTE: replace `/Users/zeblith/work/zeblithic/harmony-client` with `$REPO_ROOT/harmony-client` or whatever path convention the implementer prefers — the absolute path is the implementer's machine-specific path.)

**Do not commit.** This task is verification only.

---

## Task 1: `CommunitySyncSpawnGuard` primitive

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs` (struct/method additions; new `spawn_engine_with_guard`; modified `spawn_engine` body shape)
- Test: `src-tauri/src/community_state_sync.rs` (5 new unit tests in the existing `tests` mod)

**Goal:** Add the RAII guard primitive (`CommunitySyncSpawnGuard`) + the new public method (`spawn_engine_with_guard`) that wraps engine spawn + adapter dispatch atomically. Keep the existing `spawn_engine` method's body (will be called internally — rename to `spawn_engine_inner_now`). Add 5 unit tests from spec §7.1.

**Strategy:** TDD — write the 5 unit tests first as failing scaffolds (compile errors are fine), then implement just enough to make them pass. Same pattern as ZEB-271 PR #99 Task 1.

- [ ] **Step 1: Find an insertion point in `community_state_sync.rs`**

Run: `grep -n "// ── CommunityTransactionGuard ──\|impl CommunitySyncRegistry" src-tauri/src/community_state_sync.rs | head -5`
Expected: shows the `impl CommunitySyncRegistry` line at ~2299 and shutdown_engine_and_cleanup_persistence at ~2369. The new guard primitive will go AFTER the existing `CommunitySyncRegistry` struct definition (~line 2270) and BEFORE the `impl CommunitySyncRegistry` block (~line 2299), mirroring ZEB-271's `// ── CommunityTransactionGuard ──` placement at line 1037.

- [ ] **Step 2: Add the `CommunitySyncSpawnGuard` struct**

Insert just before `impl CommunitySyncRegistry` (at ~line 2298 — adjust if line numbers drifted):

```rust
// ── CommunitySyncSpawnGuard (ZEB-274) ─────────────────────────────────────────

/// RAII rollback guard for a freshly-spawned community-sync engine.
/// Held by an IPC handler across the critical section between
/// `spawn_engine_with_guard` and the durable `apply_space` commit.
///
/// Drop without explicit `commit()` or `abort()` triggers a
/// `Handle::try_current()` safety-net that calls
/// `shutdown_engine_and_cleanup_persistence` (only if THIS call
/// freshly created the engine — concurrent-redeem race losers per
/// ZEB-260 PR #90 round-5 are no-ops on Drop).
///
/// **No-runtime fallback:** unlike ZEB-271's `CommunityTransactionGuard`
/// (whose `abort_transaction_internal` is sync map cleanup),
/// `shutdown_engine_and_cleanup_persistence` is fundamentally async
/// (`engine.shutdown().await` flushes pending writes). When `Drop`
/// runs without a tokio runtime, we log a warn and accept the leak —
/// `reconcile_from_state` at next `start_node` will detect the
/// inconsistency and clean up. See spec §10.2.
pub struct CommunitySyncSpawnGuard {
    registry: std::sync::Arc<CommunitySyncRegistry>,
    community_id: SpaceId,
    /// Set ONCE by `spawn_engine_with_guard` before it returns. True
    /// iff this call freshly created the engine (vs. the idempotent
    /// no-op path that found an existing engine). Only fresh
    /// creations carry the rollback obligation. Plain `bool` (not
    /// `AtomicBool`): no concurrent mutation — set ONCE before
    /// `spawn_engine_with_guard` returns the guard to the caller, then
    /// only read by Drop.
    freshly_created: bool,
    /// Set to `true` by `commit()` to bypass `Drop`'s rollback path.
    /// `AtomicBool` (mirrors ZEB-271) for Acquire/Release ordering
    /// across the Drop visibility boundary.
    completed: std::sync::atomic::AtomicBool,
}
```

- [ ] **Step 3: Add `begin_spawn_guard` to `impl CommunitySyncRegistry`**

Insert inside the existing `impl CommunitySyncRegistry { ... }` block (after the `new(...)` constructor at ~line 2300):

```rust
/// Open a spawn-rollback guard. Returns immediately, performs no I/O.
/// Caller then calls `spawn_engine_with_guard(&mut guard, ...)` to
/// perform the actual spawn — the guard captures the freshness flag
/// internally. If the caller fails before `commit()`, `Drop` runs
/// `shutdown_engine_and_cleanup_persistence`. See spec §3.1, §3.2.
///
/// `begin_spawn_guard` is sync (no I/O, no lock acquisition) — the
/// guard is created with `freshly_created = false` (set later by
/// `spawn_engine_with_guard` if the spawn was the fresh one).
pub fn begin_spawn_guard(
    self: &std::sync::Arc<Self>,
    community_id: SpaceId,
) -> CommunitySyncSpawnGuard {
    CommunitySyncSpawnGuard {
        registry: std::sync::Arc::clone(self),
        community_id,
        freshly_created: false,
        completed: std::sync::atomic::AtomicBool::new(false),
    }
}
```

- [ ] **Step 4: Add `commit` + `abort` on `CommunitySyncSpawnGuard`**

Insert immediately after the `CommunitySyncSpawnGuard` struct definition (Step 2):

```rust
impl CommunitySyncSpawnGuard {
    /// Release the rollback obligation. The engine remains alive.
    /// Called by the IPC handler after `apply_space` succeeds (after
    /// the durable commit point). Sync — no `.await` needed because
    /// there is no work to do beyond setting the flag. Consumes self
    /// so Drop never runs after commit.
    pub fn commit(self) {
        self.completed
            .store(true, std::sync::atomic::Ordering::Release);
        // self drops here; Drop sees completed=true and runs no cleanup.
    }

    /// Explicit rollback. Calls `shutdown_engine_and_cleanup_persistence`
    /// if `freshly_created`. Sets `completed = true` so `Drop` is a
    /// no-op. Sync entry point but spawns the async cleanup as a tokio
    /// task internally (mirrors ZEB-271 `CommunityTransactionGuard::abort`
    /// shape). If no tokio runtime is present, logs a warn and accepts
    /// the leak (per spec §10.2 — no sync alternative for
    /// `engine.shutdown().await`).
    pub fn abort(self) {
        if self.freshly_created {
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    let registry = std::sync::Arc::clone(&self.registry);
                    let community_id = self.community_id;
                    handle.spawn(async move {
                        if let Err(e) = registry
                            .shutdown_engine_and_cleanup_persistence(&community_id)
                            .await
                        {
                            tracing::warn!(
                                community_id = ?community_id,
                                error = %e,
                                "CommunitySyncSpawnGuard::abort cleanup failed \
                                 (engine + persist dir may leak; \
                                 reconcile_from_state will recover at next start_node)"
                            );
                        }
                    });
                }
                Err(_) => {
                    tracing::warn!(
                        community_id = ?self.community_id,
                        "CommunitySyncSpawnGuard::abort called without runtime; \
                         cannot run async cleanup. Engine + persist dir will leak \
                         until reconcile_from_state at next start_node."
                    );
                }
            }
        }
        self.completed
            .store(true, std::sync::atomic::Ordering::Release);
        // self drops here; Drop sees completed=true.
    }
}
```

- [ ] **Step 5: Add `Drop` impl for `CommunitySyncSpawnGuard`**

Insert immediately after the `impl CommunitySyncSpawnGuard { ... }` block:

```rust
impl Drop for CommunitySyncSpawnGuard {
    fn drop(&mut self) {
        if !self.completed.load(std::sync::atomic::Ordering::Acquire)
            && self.freshly_created
        {
            tracing::warn!(
                community_id = ?self.community_id,
                "CommunitySyncSpawnGuard dropped without commit/abort — \
                 running safety net (spec §5.1)"
            );
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    let registry = std::sync::Arc::clone(&self.registry);
                    let community_id = self.community_id;
                    handle.spawn(async move {
                        if let Err(e) = registry
                            .shutdown_engine_and_cleanup_persistence(&community_id)
                            .await
                        {
                            tracing::warn!(
                                community_id = ?community_id,
                                error = %e,
                                "CommunitySyncSpawnGuard Drop cleanup failed \
                                 (engine + persist dir may leak; \
                                 reconcile_from_state will recover at next start_node)"
                            );
                        }
                    });
                }
                Err(_) => {
                    tracing::warn!(
                        community_id = ?self.community_id,
                        "CommunitySyncSpawnGuard dropped without runtime; \
                         cannot run async cleanup. Engine + persist dir will leak \
                         until reconcile_from_state at next start_node (spec §10.2)."
                    );
                }
            }
        }
    }
}
```

- [ ] **Step 6: Rename existing `spawn_engine` → `spawn_engine_inner_now`**

The existing `spawn_engine` method body becomes the inner helper. Rename it (currently at line ~2442):

Find the line `pub async fn spawn_engine(` at ~line 2442 and change to:

```rust
/// **ZEB-274**: this is the inner helper. Public callers should use
/// `spawn_engine_with_guard` to get atomic spawn + adapter dispatch +
/// rollback guard. Boot-time `start_node` reconcile (lib.rs:1747) is
/// allowed to call this directly because it has no rollback obligation
/// (boot reconcile recovers state, doesn't introduce new state).
///
/// Returns `Ok(true)` when this call freshly created the engine,
/// `Ok(false)` when an engine for `community_id` was already present
/// (idempotent no-op). The bool is set under the engines-map lock —
/// see ZEB-260 PR #90 round-5 and the moved comment-block.
pub(crate) async fn spawn_engine_inner_now(
    &self,
    community_id: SpaceId,
    membership_key: MembershipKey,
    admin_addr: OwnerAddr,
    is_invite_only: bool,
    publisher_tx: mpsc::Sender<Vec<u8>>,
    subscriber_rx: mpsc::Receiver<Vec<u8>>,
) -> Result<bool, CommunitySyncError> {
    // ... existing body unchanged ...
}
```

(Body text from line 2451 to end of function stays the same. ONLY change: signature line + docstring.)

- [ ] **Step 7: Find any external callers of the old `spawn_engine` method name and update**

Run: `grep -rn "\.spawn_engine\(" src-tauri/ | grep -v "spawn_engine_with_guard\|spawn_engine_inner_now"`
Expected output (these are the production call sites that need fixing):

```text
src-tauri/src/lib.rs:1747:                            .spawn_engine(space_id, mk, admin, is_invite_only, pub_tx, sub_rx)
src-tauri/src/lib.rs:7275:        .spawn_engine(
src-tauri/src/lib.rs:8531:        .spawn_engine(
```

For now, leave lib.rs:7275 + 8531 alone (Tasks 2 + 3 will rewrite those callers to use `spawn_engine_with_guard`). Update the boot reconcile at lib.rs:1747:

Find:

```rust
                            .spawn_engine(space_id, mk, admin, is_invite_only, pub_tx, sub_rx)
```

Replace with:

```rust
                            .spawn_engine_inner_now(space_id, mk, admin, is_invite_only, pub_tx, sub_rx)
```

The boot reconcile is allowed to call the inner helper directly because it has no rollback obligation (it spawns from existing on-disk state; nothing to roll back).

- [ ] **Step 8: Find any test callers of the old `spawn_engine` method name and update**

Run: `grep -rn "\.spawn_engine\(" src-tauri/tests/ src-tauri/src/community_state_sync.rs | grep -v "spawn_engine_with_guard\|spawn_engine_inner_now"`

Expected: tests in `community_invite_only_integration.rs` + `community_sync_integration.rs` + the in-file test mod that call the old name. For each match, replace `.spawn_engine(` with `.spawn_engine_inner_now(`. Tests use the inner helper directly because they don't go through the IPC handler's RAII guard.

- [ ] **Step 9: Add `spawn_engine_with_guard` method on `CommunitySyncRegistry`**

Insert in `impl CommunitySyncRegistry { ... }` block after `begin_spawn_guard`:

```rust
/// Atomic spawn + adapter dispatch with RAII rollback. Replaces the
/// old `spawn_engine` public surface. Internalizes the freshness flag
/// (formerly the `Result<bool, _>` return) into the guard so concurrent
/// callers can't race on rollback obligation. See spec §3.2, §5.3.
///
/// Sequence (atomic from caller's perspective):
///   1. `spawn_engine_inner_now` builds the engine, inserts into the
///      map. Returns `bool` for freshly-created.
///   2. If freshly created, `community_adapter_tx.try_send(...)` to
///      dispatch the adapter request to event_loop.
///   3. If try_send fails AND freshly created, immediately `.await`
///      `shutdown_engine_and_cleanup_persistence` to undo the spawn.
///      Returns Err. Guard's `freshly_created` flag is NEVER set to
///      true (so Drop is a no-op).
///   4. On full success, set `guard.freshly_created = true` (or false
///      for the idempotent path) and return Ok(engine).
pub async fn spawn_engine_with_guard(
    self: &std::sync::Arc<Self>,
    guard: &mut CommunitySyncSpawnGuard,
    community_id: SpaceId,
    membership_key: MembershipKey,
    admin_addr: OwnerAddr,
    is_invite_only: bool,
    publisher_tx: mpsc::Sender<Vec<u8>>,
    subscriber_rx: mpsc::Receiver<Vec<u8>>,
    publisher_rx: mpsc::Receiver<Vec<u8>>,
    subscriber_tx: mpsc::Sender<Vec<u8>>,
    community_adapter_tx: mpsc::Sender<crate::event_loop::CommunityAdapterRequest>,
) -> Result<std::sync::Arc<CommunitySyncEngine>, CommunitySyncError> {
    // Defensive: guard must be for the same community_id. Programming
    // error if not — the IPC handler should always pair them.
    debug_assert_eq!(
        guard.community_id, community_id,
        "spawn_engine_with_guard guard/community_id mismatch — programming error"
    );

    // Step 1: spawn the engine via the inner helper.
    let freshly_created = self
        .spawn_engine_inner_now(
            community_id,
            membership_key,
            admin_addr,
            is_invite_only,
            publisher_tx,
            subscriber_rx,
        )
        .await?;

    // Step 2: if fresh, dispatch the adapter request.
    if freshly_created {
        if let Err(send_err) = community_adapter_tx.try_send(
            crate::event_loop::CommunityAdapterRequest {
                id_hex: hex::encode(community_id.0),
                publisher_rx,
                subscriber_tx,
            },
        ) {
            // Step 3: try_send failed → undo the spawn before returning.
            // Inline `.await` (we're already inside an async fn).
            if let Err(stop_err) = self
                .shutdown_engine_and_cleanup_persistence(&community_id)
                .await
            {
                tracing::warn!(
                    community_id = ?community_id,
                    error = %stop_err,
                    "spawn_engine_with_guard: cleanup after adapter try_send failure also failed — \
                     engine + persist dir may leak (reconcile recovers at next start_node)"
                );
            }
            // Guard's freshly_created stays FALSE → Drop is a no-op.
            return Err(CommunitySyncError::Persist(format!(
                "community_adapter_tx.try_send failed: {send_err}"
            )));
        }
    }
    // ELSE: engine pre-existed (idempotent path); the publisher_rx +
    // subscriber_tx + community_adapter_tx args are dropped (the
    // existing engine + adapter already own their channels).

    // Step 4: bind the freshness flag to the guard. Now the guard
    // carries the rollback obligation if freshly_created = true.
    guard.freshly_created = freshly_created;

    // Recover the engine handle for the caller. The inner helper
    // doesn't return it directly to preserve the existing return-type
    // shape on the inner; we look it up from the registry. The lookup
    // is guaranteed to succeed (the engine was just inserted under
    // the engines lock and we haven't yielded since — for the
    // freshly_created path. For the idempotent path, the existing
    // engine is what we return.)
    let engine = self.engine_arc(&community_id).await.ok_or_else(|| {
        CommunitySyncError::Persist(format!(
            "engine vanished immediately after spawn_engine_inner_now \
             (community_id = {community_id:?}) — registry race or programming error"
        ))
    })?;
    Ok(engine)
}
```

- [ ] **Step 10: Run cargo check to confirm compilation**

```bash
cd src-tauri && cargo check --locked --features test-fixtures 2>&1 | tail -10
```

Expected: clean compile (no errors). May show warnings about unused imports if `event_loop::CommunityAdapterRequest` wasn't already imported in `community_state_sync.rs` — if so, add the import.

- [ ] **Step 11: Add unit test scaffolds (5 tests, failing initially is fine — Step 12 confirms they pass)**

Find the existing `tests` mod in `community_state_sync.rs` (search for `#[cfg(test)]\nmod tests`). Insert these 5 tests at the end of the mod (before the closing `}`):

```rust
    // ── ZEB-274 spawn-rollback-guard tests ─────────────────────────

    /// Spec §7.1 #1: spawn engine, commit guard, verify engine present
    /// + persistence dir present after guard drops.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_commit_releases_rollback() {
        let fix = build_test_fixture().await;
        let community_id = SpaceId([0xc1; 16]);

        let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        let mut guard = std::sync::Arc::clone(&fix.registry).begin_spawn_guard(community_id);
        let engine = std::sync::Arc::clone(&fix.registry)
            .spawn_engine_with_guard(
                &mut guard,
                community_id,
                fix.membership_key.clone(),
                fix.admin_addr,
                false,
                pub_tx,
                sub_rx,
                pub_rx,
                sub_tx,
                fix.community_adapter_tx.clone(),
            )
            .await
            .expect("spawn_engine_with_guard");

        guard.commit();
        // engine handle still valid
        drop(engine);

        // Engine must still be in the registry (commit released the rollback obligation).
        assert!(
            fix.registry.has_engine(&community_id).await,
            "engine must remain after commit"
        );
        // Persistence dir must still exist.
        let dir = fix.identity_dir.join("communities").join(hex::encode(community_id.0));
        assert!(dir.exists(), "persistence dir must remain after commit");
    }

    /// Spec §7.1 #2: spawn engine, drop guard without commit, verify
    /// engine absent + persistence dir absent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_drop_without_commit_tears_down_fresh() {
        let fix = build_test_fixture().await;
        let community_id = SpaceId([0xc2; 16]);

        let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        {
            let mut guard = std::sync::Arc::clone(&fix.registry).begin_spawn_guard(community_id);
            let _engine = std::sync::Arc::clone(&fix.registry)
                .spawn_engine_with_guard(
                    &mut guard,
                    community_id,
                    fix.membership_key.clone(),
                    fix.admin_addr,
                    false,
                    pub_tx,
                    sub_rx,
                    pub_rx,
                    sub_tx,
                    fix.community_adapter_tx.clone(),
                )
                .await
                .expect("spawn_engine_with_guard");
            // guard drops here without commit → Drop spawns cleanup task
        }

        // Poll up to 500ms for the cleanup task to clear the engine
        // (mirrors ZEB-271's tx_dropped_guard_safety_net_aborts pattern).
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while fix.registry.has_engine(&community_id).await
            && std::time::Instant::now() < deadline
        {
            tokio::task::yield_now().await;
        }
        assert!(
            !fix.registry.has_engine(&community_id).await,
            "engine must be torn down after guard drops without commit"
        );

        // Persistence dir must also be cleaned up.
        let dir = fix.identity_dir.join("communities").join(hex::encode(community_id.0));
        assert!(!dir.exists(), "persistence dir must be removed after guard drops");
    }

    /// Spec §7.1 #3: open guard A, spawn engine; open guard B for the
    /// same community (idempotent — sees existing engine), drop B
    /// without commit; verify engine still present (B's guard didn't
    /// tear down because freshly_created = false).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_drop_idempotent_call_is_noop() {
        let fix = build_test_fixture().await;
        let community_id = SpaceId([0xc3; 16]);

        let (pub_tx_a, pub_rx_a) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx_a, sub_rx_a) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        // Caller A: spawns the engine fresh.
        let mut guard_a = std::sync::Arc::clone(&fix.registry).begin_spawn_guard(community_id);
        let _engine_a = std::sync::Arc::clone(&fix.registry)
            .spawn_engine_with_guard(
                &mut guard_a,
                community_id,
                fix.membership_key.clone(),
                fix.admin_addr,
                false,
                pub_tx_a,
                sub_rx_a,
                pub_rx_a,
                sub_tx_a,
                fix.community_adapter_tx.clone(),
            )
            .await
            .expect("spawn_engine_with_guard A");
        guard_a.commit();

        // Caller B: spawns idempotently (engine pre-existing).
        let (pub_tx_b, pub_rx_b) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx_b, sub_rx_b) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        {
            let mut guard_b = std::sync::Arc::clone(&fix.registry).begin_spawn_guard(community_id);
            let _engine_b = std::sync::Arc::clone(&fix.registry)
                .spawn_engine_with_guard(
                    &mut guard_b,
                    community_id,
                    fix.membership_key.clone(),
                    fix.admin_addr,
                    false,
                    pub_tx_b,
                    sub_rx_b,
                    pub_rx_b,
                    sub_tx_b,
                    fix.community_adapter_tx.clone(),
                )
                .await
                .expect("spawn_engine_with_guard B (idempotent)");
            // guard_b drops here without commit. freshly_created = false → Drop is no-op.
        }

        // Engine must STILL be present (B's guard didn't tear down A's engine).
        assert!(
            fix.registry.has_engine(&community_id).await,
            "engine must remain after idempotent caller B's guard drops uncommitted"
        );
    }

    /// Spec §7.1 #4: spawn engine, abort guard, verify engine absent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_explicit_abort_tears_down() {
        let fix = build_test_fixture().await;
        let community_id = SpaceId([0xc4; 16]);

        let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        let mut guard = std::sync::Arc::clone(&fix.registry).begin_spawn_guard(community_id);
        let _engine = std::sync::Arc::clone(&fix.registry)
            .spawn_engine_with_guard(
                &mut guard,
                community_id,
                fix.membership_key.clone(),
                fix.admin_addr,
                false,
                pub_tx,
                sub_rx,
                pub_rx,
                sub_tx,
                fix.community_adapter_tx.clone(),
            )
            .await
            .expect("spawn_engine_with_guard");

        guard.abort();

        // Poll up to 500ms for abort's spawned cleanup task.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while fix.registry.has_engine(&community_id).await
            && std::time::Instant::now() < deadline
        {
            tokio::task::yield_now().await;
        }
        assert!(
            !fix.registry.has_engine(&community_id).await,
            "engine must be torn down after explicit abort"
        );
    }

    /// Spec §7.1 #5: drop guard from a non-tokio thread; verify the
    /// no-runtime path runs (logs warn) and the engine remains
    /// (acknowledged leak per spec §10.2 — reconcile recovers at next
    /// start_node).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_drop_no_runtime_logs_and_leaks() {
        let fix = build_test_fixture().await;
        let community_id = SpaceId([0xc5; 16]);

        let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        let mut guard = std::sync::Arc::clone(&fix.registry).begin_spawn_guard(community_id);
        let _engine = std::sync::Arc::clone(&fix.registry)
            .spawn_engine_with_guard(
                &mut guard,
                community_id,
                fix.membership_key.clone(),
                fix.admin_addr,
                false,
                pub_tx,
                sub_rx,
                pub_rx,
                sub_tx,
                fix.community_adapter_tx.clone(),
            )
            .await
            .expect("spawn_engine_with_guard");

        // Move the guard into a synchronous (non-tokio) thread and
        // drop it there. The Drop impl's Handle::try_current() must
        // return Err and take the no-runtime fallback (log + leak).
        let drop_thread = std::thread::spawn(move || {
            drop(guard);
        });
        drop_thread.join().expect("drop thread");

        // Engine MUST still be present (no-runtime path can't tear down).
        // This is the acknowledged leak per spec §10.2.
        assert!(
            fix.registry.has_engine(&community_id).await,
            "engine must remain after no-runtime Drop (leak acknowledged per spec §10.2)"
        );

        // Cleanup for test isolation: tear down explicitly via the registry.
        fix.registry
            .shutdown_engine_and_cleanup_persistence(&community_id)
            .await
            .expect("explicit cleanup for test isolation");
    }
```

NOTE: the tests reference `build_test_fixture()` which must exist in the test mod. Look for the existing fixture builder (commonly named `build_test_fixture`, `build_fixture`, or similar). If it doesn't already create a `community_adapter_tx`, extend the fixture struct + builder to include one. Specifically: `community_adapter_tx: mpsc::Sender<crate::event_loop::CommunityAdapterRequest>`. The fixture can drop the receiver (tests don't need to consume the dispatched adapter requests).

If the existing test mod doesn't have a fixture builder at all, scaffold one mirroring the test pattern at `community_channel_log_engine.rs` lines 2200+ (for ZEB-271's tests).

- [ ] **Step 12: Run the new tests + verify they pass**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_state_sync::tests::guard_)'
```

Expected: 5 tests pass. If any fail, diagnose and fix the implementation. Common issues:
- `spawn_engine_with_guard` returning `Err` for the idempotent path (should be `Ok` with `freshly_created = false`)
- Drop's `Handle::try_current()` returning `Ok` even in the std::thread::spawn closure (verify that test #5 actually moves the guard cross-thread)
- Persistence-dir-removal race with the post-Drop check (the 500ms poll loop is the mitigation)

- [ ] **Step 13: Run all 5 gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast
cd src-tauri && cargo check --locked --all-targets --features test-fixtures
cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit && npx vitest run
```

Expected: all 5 green. Total tests should be 957 (baseline) + 5 (new guard tests) = 962. Plus any test fixture changes that touched existing tests should still pass.

- [ ] **Step 14: Commit**

```bash
git add src-tauri/src/community_state_sync.rs
git commit -m "$(cat <<'EOF'
feat(zeb-274): CommunitySyncSpawnGuard primitive + spawn_engine_with_guard

Adds the RAII rollback guard primitive on CommunitySyncRegistry per
spec §3:
- CommunitySyncSpawnGuard struct with begin_spawn_guard constructor
- commit() (sync — releases rollback obligation)
- abort() (sync entry point; spawns async cleanup via Handle, with
  no-runtime warn-and-leak fallback per spec §10.2)
- Drop impl mirroring abort() but with safety-net warn

Public method spawn_engine_with_guard on CommunitySyncRegistry that
wraps engine spawn + adapter dispatch atomically. Internalizes the
freshness flag (formerly the Result<bool, _> return from
spawn_engine), removing it from the public surface.

Renames existing public spawn_engine → pub(crate) spawn_engine_inner_now
(used by boot reconcile + the new spawn_engine_with_guard internally).
Updates lib.rs:1747 boot reconcile call site to use the inner helper.

Adds 5 unit tests per spec §7.1:
- guard_commit_releases_rollback
- guard_drop_without_commit_tears_down_fresh
- guard_drop_idempotent_call_is_noop
- guard_explicit_abort_tears_down
- guard_drop_no_runtime_logs_and_leaks

Verification: all 5 CI gates green; 962 tests pass (957 baseline +
5 new).
EOF
)"
```

---

## Task 2: `create_community_inner` rewrite — collapse 9 rollback sites

**Files:**
- Modify: `src-tauri/src/lib.rs:7193-7620` (the `create_community_inner` function)
- Test: `src-tauri/src/lib.rs` (test mod near `create_community_inner_tests` — add 1 spec §7.2 test)

**Goal:** Replace 9 explicit `shutdown_engine_and_cleanup_persistence` rollback sites with a single RAII guard. The guard wraps the entire critical section from spawn through apply_space. If anything fails, `?` early-return triggers Drop which runs cleanup.

**Strategy:** Make the changes in one shot (the rollback sites all share the same pattern — collapse to `?`). Verify compilation after each major step. The `engine_arc().await.ok_or(...)` lookup that immediately follows spawn_engine becomes unnecessary because `spawn_engine_with_guard` returns the engine handle directly.

- [ ] **Step 1: Open guard at start of critical section**

Find the line at lib.rs:7264 that begins the channel_log_tx (per ZEB-271):

```rust
    let channel_log_tx = channel_log_registry.begin_transaction(minted.community_id);
```

Insert immediately after:

```rust
    // ZEB-274: RAII rollback guard for the community-sync spawn + adapter
    // dispatch. If anything between here and `community_sync_guard.commit()`
    // below fails (including panics), Drop runs
    // shutdown_engine_and_cleanup_persistence. Replaces the 9 scattered
    // explicit rollback sites that this function previously had.
    let mut community_sync_guard = community_registry.begin_spawn_guard(minted.community_id);
```

- [ ] **Step 2: Replace `spawn_engine` + `try_send` block with `spawn_engine_with_guard`**

Find the existing block (lib.rs:7274-7316):

```rust
    community_registry
        .spawn_engine(
            minted.community_id,
            minted.membership_key.clone(),
            self_owner,
            is_invite_only,
            pub_tx,
            sub_rx,
        )
        .await
        .map_err(|e| format!("registry.spawn_engine: {e}"))?;

    if let Err(e) = community_adapter_tx.try_send(crate::event_loop::CommunityAdapterRequest {
        id_hex: hex::encode(minted.community_id.0),
        publisher_rx: pub_rx,
        subscriber_tx: sub_tx,
    }) {
        // Engine is in the registry but adapter wiring failed. Tear it
        // down so we don't accumulate a zombie engine. ZEB-258 win:
        // owner-state is still untouched at this point. ZEB-262 Task 7:
        // shutdown_engine_and_cleanup_persistence also removes the
        // orphan per-community persistence dir, closing the disk-leak
        // gap that the bare stop_engine call tolerated.
        if let Err(stop_err) = community_registry
            .shutdown_engine_and_cleanup_persistence(&minted.community_id)
            .await
        {
            tracing::warn!(
                error = %stop_err,
                community_id = %hex::encode(minted.community_id.0),
                "shutdown_engine_and_cleanup_persistence failed during create_community \
                 adapter-dispatch rollback"
            );
        }
        return Err(match e {
            tokio::sync::mpsc::error::TrySendError::Full(_) => {
                "community_adapter_tx full".to_string()
            }
            tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                "community_adapter_tx closed".to_string()
            }
        });
    }
```

Replace with:

```rust
    let engine_arc = community_registry
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
```

This collapses ~40 lines into ~14. The guard's atomic spawn-then-dispatch-then-on-failure-rollback handles everything that was scattered.

- [ ] **Step 3: Remove the `engine_arc()` lookup that immediately follows**

Find the existing block (lib.rs:7324-7349 region — was an `engine_arc().await.ok_or(...)` followed by a manual rollback on the insert_local_event Err):

```rust
    let engine_arc = community_registry
        .engine_arc(&minted.community_id)
        .await
        .ok_or("engine vanished immediately after spawn — registry race")?;
    // CodeRabbit P0: a `?` early-return here would leave the spawned
    // engine + persistence dir behind. Wrap the Result and tear down
    // on Err before returning.
    let outcome = match engine_arc
        .insert_local_event(minted.bootstrap_join.clone())
        .await
    {
        Ok(o) => o,
        Err(e) => {
            if let Err(stop_err) = community_registry
                .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                .await
            {
                tracing::warn!(
                    error = %stop_err,
                    community_id = %hex::encode(minted.community_id.0),
                    "shutdown failed during create_community_inner insert-err rollback"
                );
            }
            return Err(format!("engine.insert_local_event: {e}"));
        }
    };
```

Replace with:

```rust
    // ZEB-274: engine_arc() lookup removed — spawn_engine_with_guard above
    // returned the engine handle directly. The CodeRabbit P0 manual
    // rollback collapses into the guard's Drop on `?` early-return.
    let outcome = engine_arc
        .insert_local_event(minted.bootstrap_join.clone())
        .await
        .map_err(|e| format!("engine.insert_local_event (bootstrap_join): {e}"))?;
```

- [ ] **Step 4: Replace the bootstrap-Join insert-rejection rollback**

Find (lib.rs:7350-7371 region):

```rust
    if !matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        // Bootstrap Join didn't insert — engine state is inconsistent
        // with the user-visible "creator just made this community"
        // expectation. Tear down + bail. Owner-state still untouched.
        // ZEB-262 Task 7: cleanup also removes the per-community
        // persist dir.
        if let Err(stop_err) = community_registry
            .shutdown_engine_and_cleanup_persistence(&minted.community_id)
            .await
        {
            tracing::warn!(
                error = %stop_err,
                community_id = %hex::encode(minted.community_id.0),
                "shutdown_engine_and_cleanup_persistence failed during create_community \
                 rollback (bootstrap-Join not inserted)"
            );
        }
        return Err(format!("bootstrap Join not inserted (got {outcome:?})"));
    }
```

Replace with:

```rust
    if !matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        // ZEB-274: rollback collapses into community_sync_guard Drop on early-return.
        return Err(format!("bootstrap Join not inserted (got {outcome:?})"));
    }
```

- [ ] **Step 5: Replace the default-channel insert error + rejection rollbacks**

Find the two similar blocks for the default `#general` channel (lib.rs:7420-7475 region, two calls to shutdown). Each follows the same pattern:

```rust
    let default_outcome = match engine_arc
        .insert_local_event(default_channel_create_event.clone())
        .await
    {
        Ok(o) => o,
        Err(e) => {
            if let Err(stop_err) = community_registry
                .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                .await
            {
                tracing::warn!(...);
            }
            return Err(format!("engine.insert_local_event (default_channel): {e}"));
        }
    };
    if !matches!(default_outcome, crate::community_state_crdt::InsertOutcome::Inserted) {
        if let Err(stop_err) = community_registry
            .shutdown_engine_and_cleanup_persistence(&minted.community_id)
            .await
        {
            tracing::warn!(...);
        }
        return Err(format!("default channel-create not inserted (got {default_outcome:?})"));
    }
```

Replace BOTH the Err arm's manual rollback AND the not-Inserted manual rollback with `?` + plain `return Err(...)`:

```rust
    let default_outcome = engine_arc
        .insert_local_event(default_channel_create_event.clone())
        .await
        .map_err(|e| format!("engine.insert_local_event (default_channel): {e}"))?;
    if !matches!(default_outcome, crate::community_state_crdt::InsertOutcome::Inserted) {
        return Err(format!("default channel-create not inserted (got {default_outcome:?})"));
    }
```

(Implementer: search the existing code for the actual variable names — they may be `default_channel_outcome` or `channel_create_event` etc. The shape of the collapse is the same regardless.)

- [ ] **Step 6: Replace the apply_space rejection rollback**

Find (lib.rs:7561-7593 region):

```rust
    {
        let mut state_g = crdt_state.lock().await;
        let outcome = state_g.apply_space_with_canonicalization(minted.space.clone());
        if matches!(outcome, crate::owner_state_crdt::ApplyOutcome::Rejected(_)) {
            drop(state_g);
            if let Err(stop_err) = community_registry
                .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                .await
            {
                tracing::warn!(
                    error = %stop_err,
                    community_id = %hex::encode(minted.community_id.0),
                    "shutdown_engine_and_cleanup_persistence failed during \
                     create_community rollback (apply_space rejected)"
                );
            }
            return Err(format!("apply_space rejected new community: {outcome:?}"));
        }
    }
```

Replace with:

```rust
    {
        let mut state_g = crdt_state.lock().await;
        let outcome = state_g.apply_space_with_canonicalization(minted.space.clone());
        if matches!(outcome, crate::owner_state_crdt::ApplyOutcome::Rejected(_)) {
            // ZEB-274: rollback collapses into community_sync_guard Drop on early-return.
            return Err(format!("apply_space rejected new community: {outcome:?}"));
        }
    }
```

- [ ] **Step 7: Add `community_sync_guard.commit()` BEFORE `channel_log_tx.commit().await`**

Find the existing channel_log_tx commit block (lib.rs:7595-7610 region):

```rust
    // ZEB-271: post-durable-commit drain. apply_space above is the LAST
    // PERSISTENT step — the community is committed. If commit() fails,
    // log and continue: the deferred channel-log spawns (e.g., default
    // #general) will be re-attempted by reconcile_from_state at next
    // start_node. Returning Err here would surface the create as failed
    // even though the community exists, leading to retry → duplicate
    // community.
    if let Err(e) = channel_log_tx.commit().await {
        tracing::warn!(
            community_id = %hex::encode(minted.community_id.0),
            error = %e,
            "channel_log_registry commit failed after durable community create; \
             pending channel-log spawns will be re-attempted via reconcile_from_state \
             on next start_node"
        );
    }
```

Insert immediately BEFORE this block:

```rust
    // ZEB-274: release the community-sync rollback obligation. apply_space
    // succeeded — the community is durable. Sync (no .await needed). Per
    // spec §8 #4: community_sync_guard.commit() FIRST, then channel_log_tx.
    community_sync_guard.commit();
```

- [ ] **Step 8: Sweep for any remaining `shutdown_engine_and_cleanup_persistence` calls in `create_community_inner`**

```bash
sed -n '7193,7620p' src-tauri/src/lib.rs | grep -c "\.shutdown_engine_and_cleanup_persistence("
```

Expected: `0`. (If non-zero, find the remaining sites — they should all be removable. The `_inner_tests` mod might still call it for test cleanup; that's fine since the test mod is below the function body.)

- [ ] **Step 9: Run cargo check to confirm compilation**

```bash
cd src-tauri && cargo check --locked --features test-fixtures 2>&1 | tail -10
```

Expected: clean compile. If errors:
- "cannot find function `engine_arc`" — verify the lookup was actually removed in Step 3
- "cannot find variable `engine_arc`" — verify the rebinding from `spawn_engine_with_guard` return is correct in Step 2
- borrow-checker errors around `community_sync_guard` and `channel_log_tx` — verify the order in Step 7 (community_sync_guard first)

- [ ] **Step 10: Add the `create_community_engine_torn_down_on_apply_space_rejection` integration test**

Find the existing `create_community_inner_tests` mod in lib.rs (search for `mod create_community_inner_tests` or similar). Add a new test that injects an apply_space rejection and verifies the engine + persist dir are torn down.

The fixture details depend on the existing test infrastructure — copy the shape of an existing happy-path test and modify the fixture to make `apply_space_with_canonicalization` return `Rejected(...)`. The most robust way is to construct a `crdt_state` whose internal invariants would reject the new Space (e.g., a duplicate space_id, or a deliberately-built invalid space). If the existing test fixtures don't support injection, file a follow-up note in the commit message and skip this test (the unit tests in Task 1 already cover the guard's Drop path; the integration test is bonus coverage).

If you can write the test cleanly:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_community_engine_torn_down_on_apply_space_rejection() {
    let fix = build_create_community_test_fixture().await;
    // Pre-load crdt_state with a Space that conflicts with what
    // create_community_inner will mint (forcing apply_space rejection).
    // [Implementer fills in fixture-specific detail.]

    let result = create_community_inner(...).await;
    assert!(result.is_err(), "create_community must fail on apply_space rejection");

    // Poll up to 500ms for the guard's Drop cleanup task.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    while fix.community_registry.has_engine(&community_id).await
        && std::time::Instant::now() < deadline
    {
        tokio::task::yield_now().await;
    }
    assert!(
        !fix.community_registry.has_engine(&community_id).await,
        "engine must be torn down after apply_space rejection (guard Drop ran cleanup)"
    );
}
```

If the test cannot be written cleanly with the existing fixture, document the deviation in the commit message: "Spec §7.2 test deferred — existing fixture doesn't support apply_space rejection injection. The Task 1 unit tests cover the guard's Drop path, so the integration coverage is bonus rather than load-bearing."

- [ ] **Step 11: Run the new test (or skip if deferred)**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(create_community_engine_torn_down)'
```

Expected: passes. (Skip if Step 10 was deferred.)

- [ ] **Step 12: Run all 5 gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast
cd src-tauri && cargo check --locked --all-targets --features test-fixtures
cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit && npx vitest run
```

Expected: all 5 green.

- [ ] **Step 13: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-274): create_community_inner — collapse 9 rollback sites into RAII guard

Replaces 9 scattered shutdown_engine_and_cleanup_persistence rollback
sites in create_community_inner with one CommunitySyncSpawnGuard
(introduced in prior commit). Each `if let Err ... shutdown ... return`
block (~10 lines each) collapses to `?` + plain `return Err(...)`.

Sites collapsed:
- adapter try_send failure (lib.rs:7286-7316)
- engine.insert_local_event(bootstrap_join) Err (lib.rs:7331-7349)
- bootstrap_join not Inserted (lib.rs:7350-7371)
- engine.insert_local_event(default_channel) Err (~7420)
- default_channel not Inserted (~7445)
- (additional similar sites at ~7466, 7515, 7534, 7575)

Also removes the now-unnecessary engine_arc().await.ok_or(...) lookup
post-spawn — spawn_engine_with_guard returns the engine handle directly.

Adds community_sync_guard.commit() BEFORE channel_log_tx.commit().await
per spec §8 #4 (sequential, community-sync first).

Plus integration test (or deferred per implementer note in fixture).

Verification: all 5 CI gates green; lib.rs lost ~80 lines of rollback
boilerplate.
EOF
)"
```

---

## Task 3: `redeem_invite_inner` rewrite — collapse 17 rollback sites + remove `engine_freshly_created` plumbing

**Files:**
- Modify: `src-tauri/src/lib.rs:8420-9130` (the `redeem_invite_inner` function)
- Test: `src-tauri/src/lib.rs` test mod (add 1 spec §7.3 race-condition test)

**Goal:** Same collapse-pattern as Task 2 but for the much larger `redeem_invite_inner`. The 17 rollback sites are all guarded by `if engine_freshly_created` — the guard's internalized freshness directly replaces this. The local `engine_freshly_created: bool` at lib.rs:8530 is removed entirely.

**Strategy:** Same as Task 2. Make the changes in one pass — every `if engine_freshly_created { ... shutdown ... return Err(...) }` block collapses to a plain `return Err(...)` (the guard's Drop handles the rollback).

- [ ] **Step 1: Open guard at start of critical section**

Find the line in `redeem_invite_inner` that begins the channel_log_tx (look for `channel_log_registry.begin_transaction(minted.community_id)` — should be around lib.rs:8488):

Insert immediately after:

```rust
    // ZEB-274: RAII rollback guard for the community-sync spawn + adapter
    // dispatch. Same pattern as create_community_inner. Internalizes
    // the freshness flag — the local `engine_freshly_created: bool`
    // (was lib.rs:8530, ZEB-260 PR #90 round-5) is removed; the guard
    // tracks it and its Drop only runs cleanup if THIS call's spawn
    // was the fresh one. Concurrent-redeem race losers' guards are
    // no-ops on Drop (spec §5.2).
    let mut community_sync_guard = community_registry.begin_spawn_guard(minted.community_id);
```

- [ ] **Step 2: Replace `spawn_engine` + freshness-flag capture with `spawn_engine_with_guard`**

Find the existing spawn block (lib.rs:8527-8540):

```rust
    let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

    let engine_freshly_created = community_registry
        .spawn_engine(
            minted.community_id,
            minted.membership_key.clone(),
            payload.admin_addr,
            payload.is_invite_only,
            pub_tx,
            sub_rx,
        )
        .await
        .map_err(|e| format!("registry.spawn_engine: {e}"))?;
```

Replace with:

```rust
    let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

    // ZEB-274: spawn engine + dispatch adapter atomically via the guard.
    // Internalizes the freshness flag — no separate engine_freshly_created
    // local (the guard tracks it).
    let engine_arc = community_registry
        .spawn_engine_with_guard(
            &mut community_sync_guard,
            minted.community_id,
            minted.membership_key.clone(),
            payload.admin_addr,
            payload.is_invite_only,
            pub_tx,
            sub_rx,
            pub_rx,
            sub_tx,
            community_adapter_tx,
        )
        .await
        .map_err(|e| format!("registry.spawn_engine_with_guard: {e}"))?;
```

- [ ] **Step 3: Remove the original adapter try_send block + its rollback**

Find the existing block at lib.rs:8542-8570 region (begins with `if engine_freshly_created { if let Err(e) = community_adapter_tx.try_send(...)`):

```rust
    if engine_freshly_created {
        if let Err(e) = community_adapter_tx.try_send(crate::event_loop::CommunityAdapterRequest {
            id_hex: hex::encode(minted.community_id.0),
            publisher_rx: pub_rx,
            subscriber_tx: sub_tx,
        }) {
            if let Err(stop_err) = community_registry
                .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                .await
            {
                tracing::warn!(...);
            }
            return Err(match e {
                tokio::sync::mpsc::error::TrySendError::Full(_) => "...".to_string(),
                tokio::sync::mpsc::error::TrySendError::Closed(_) => "...".to_string(),
            });
        }
    }
```

Delete this entire block. The adapter try_send is now inside `spawn_engine_with_guard` (Task 1 Step 9). The rollback is via the guard's Drop.

- [ ] **Step 4: Find every remaining `if engine_freshly_created { ... shutdown ... return Err(...) }` block and collapse**

Run: `grep -n "if engine_freshly_created" src-tauri/src/lib.rs`
Expected: 0 matches after this step (currently shows 17).

For EACH match found, the pattern is:

```rust
    [some condition] {
        if engine_freshly_created {
            if let Err(stop_err) = community_registry
                .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                .await
            {
                tracing::warn!(
                    error = %stop_err,
                    community_id = %hex::encode(minted.community_id.0),
                    "shutdown failed during redeem_invite ... rollback"
                );
            }
        }
        return Err(format!("...message..."));
    }
```

Replace EACH match with:

```rust
    [some condition] {
        // ZEB-274: rollback collapses into community_sync_guard Drop on early-return.
        return Err(format!("...message..."));
    }
```

The 17 sites are mechanically identical except for the inner condition + return error message. Implementer: do this as a careful pass through each site rather than a regex replace (the surrounding code structure varies — sometimes the rollback is in a `match` arm, sometimes in an `if let Err`, sometimes after a `match`-then-arm).

Concrete site list (from grep at planning time — line numbers may have shifted with prior edits):

1. lib.rs:8591 (engine_arc-vanished rollback, OPEN branch)
2. lib.rs:8615 (insert_local_event Err, OPEN branch)
3. lib.rs:8636 (insert not Inserted, OPEN branch)
4. lib.rs:8659 (missing invite_token, INVITE-ONLY branch)
5. lib.rs:8692 (verify_admin_bootstrap fail)
6. lib.rs:8723 (admin bootstrap insert_local_event Err)
7. lib.rs:8758 (admin bootstrap not Inserted)
8. lib.rs:8779 (verify self_join fail)
9. lib.rs:8846 (CommunityInvitePacket build fail)
10. lib.rs:8867 (PrivateIdentity::random fail)
11. lib.rs:8891 (Reticulum unicast send fail)
12. lib.rs:8941 (oneshot wait timeout, INVITE-ONLY branch)
13. lib.rs:8977 (engine_arc post-wait vanished)
14. lib.rs:9026 (countersig insert_local_event Err)
15. lib.rs:9064 (apply_space rejection)
16. lib.rs:9092 (channel_log_tx pre-commit error from related path)

Plus 1 site at lib.rs:8549 that's outside the `if engine_freshly_created` guard. Implementer: read the surrounding context before deleting to make sure each match is genuinely a rollback site (some grep matches may be inside a comment).

- [ ] **Step 5: Remove the `engine_freshly_created` local variable**

After all 17 rollback sites are collapsed, the local at lib.rs:8530 is unused. Delete:

```rust
    let engine_freshly_created = community_registry
        .spawn_engine(...)
```

(Already replaced by the `let engine_arc = ...spawn_engine_with_guard(...)` in Step 2.)

If clippy then complains about an unused `engine_freshly_created` variable somewhere, find and remove that residual usage.

- [ ] **Step 6: Add `community_sync_guard.commit()` BEFORE `channel_log_tx.commit().await`**

Find the existing channel_log_tx commit at the bottom of `redeem_invite_inner` (around lib.rs:9120):

```rust
    if let Err(e) = channel_log_tx.commit().await {
        tracing::warn!(
            community_id = %hex::encode(community_id.0),
            error = %e,
            "channel_log_registry commit failed ..."
        );
    }
```

Insert immediately BEFORE:

```rust
    // ZEB-274: release the community-sync rollback obligation. apply_space
    // succeeded — the redemption is durable. Sync (no .await needed). Per
    // spec §8 #4: community_sync_guard.commit() FIRST, then channel_log_tx.
    community_sync_guard.commit();
```

- [ ] **Step 7: Sweep for any remaining `shutdown_engine_and_cleanup_persistence` calls in `redeem_invite_inner`**

```bash
sed -n '8420,9130p' src-tauri/src/lib.rs | grep -c "\.shutdown_engine_and_cleanup_persistence("
```

Expected: `0`.

```bash
sed -n '8420,9130p' src-tauri/src/lib.rs | grep -c "engine_freshly_created"
```

Expected: `0`.

- [ ] **Step 8: Run cargo check**

```bash
cd src-tauri && cargo check --locked --features test-fixtures 2>&1 | tail -10
```

Expected: clean compile.

- [ ] **Step 9: Add the `redeem_invite_concurrent_race_loser_no_op_on_drop` integration test**

In the test mod, add (mirrors spec §7.3):

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redeem_invite_concurrent_race_loser_no_op_on_drop() {
    let fix = build_redeem_invite_test_fixture().await;
    // [Implementer: this test requires the ability to spawn two
    // concurrent redeem_invite_inner calls for the same community_id.
    // If the existing fixture doesn't support this directly, scaffold
    // it. Outline:]

    // 1. Build a valid invite payload + minted state for the same
    //    community_id, callable from two parallel tasks.
    let payload = ...;

    // 2. Spawn two concurrent redemptions.
    let fix_a = std::sync::Arc::clone(&fix);
    let fix_b = std::sync::Arc::clone(&fix);
    let payload_a = payload.clone();
    let payload_b = payload.clone();

    let task_a = tokio::spawn(async move { redeem_invite_inner(fix_a, payload_a).await });
    let task_b = tokio::spawn(async move { redeem_invite_inner(fix_b, payload_b).await });

    let (res_a, res_b) = tokio::join!(task_a, task_b);

    // 3. Verify exactly one succeeded (and the other returned a
    //    benign idempotency error or also succeeded — depending on
    //    the redeem semantics for double-redeem).
    let _r_a = res_a.expect("task_a panicked");
    let _r_b = res_b.expect("task_b panicked");

    // 4. Critically: the engine MUST exist (the winner's spawn is
    //    intact; the loser's guard didn't tear it down).
    assert!(
        fix.community_registry.has_engine(&community_id).await,
        "engine must exist after concurrent redeem race (loser guard was no-op)"
    );
}
```

If the fixture work to support concurrent redemption is too large to scope into this PR, document the deviation in the commit message and rely on Task 1 unit test #3 (`guard_drop_idempotent_call_is_noop`) for coverage of the race semantics — it tests the equivalent code path at the registry level rather than via the IPC handlers.

- [ ] **Step 10: Run the new test (or skip if deferred)**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(redeem_invite_concurrent_race)'
```

Expected: passes (if scaffolded). Otherwise skip per Step 9 deviation note.

- [ ] **Step 11: Run all 5 gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast
cd src-tauri && cargo check --locked --all-targets --features test-fixtures
cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit && npx vitest run
```

Expected: all 5 green.

- [ ] **Step 12: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-274): redeem_invite_inner — collapse 17 rollback sites + remove freshness plumbing

Replaces 17 scattered shutdown_engine_and_cleanup_persistence rollback
sites in redeem_invite_inner with one CommunitySyncSpawnGuard. Each
`if engine_freshly_created { ... shutdown ... return Err }` block
(~12 lines each) collapses to a plain `return Err(...)` because the
guard's Drop handles cleanup automatically.

Removes the local `engine_freshly_created: bool` (was lib.rs:8530,
ZEB-260 PR #90 round-5). The guard internalizes this state — the
concurrent-redeem race semantics are preserved: only the call that
freshly created the engine carries the rollback obligation.

Sites collapsed (line numbers from baseline 06fd617; may differ
post-edit):
- engine_arc-vanished (8591), insert Err + not-Inserted (8615, 8636)
- INVITE-ONLY: missing invite_token (8659), verify_admin_bootstrap fail
  (8692), admin-bootstrap insert Err + not-Inserted (8723, 8758)
- self_join verify fail (8779), CommunityInvitePacket build (8846),
  PrivateIdentity::random (8867), unicast send (8891)
- oneshot wait timeout (8941), engine_arc post-wait vanished (8977)
- countersig insert Err (9026), apply_space rejection (9064)
- channel_log_tx pre-commit error (9092)
- adapter try_send (8549; was outside engine_freshly_created guard,
  now also handled by spawn_engine_with_guard's atomic dispatch)

Adds community_sync_guard.commit() BEFORE channel_log_tx.commit().await
per spec §8 #4.

Plus race-condition integration test (or deferred per implementer note;
Task 1's guard_drop_idempotent_call_is_noop unit test covers the same
semantic at the registry level).

Verification: all 5 CI gates green; lib.rs lost ~200 lines of rollback
boilerplate.
EOF
)"
```

---

## Task 4: ZEB-271 spec §9 cross-ref update

**Files:**
- Modify: `docs/specs/2026-05-10-zeb-271-channel-log-registry-transactionality-design.md` line 269

**Goal:** Mechanical 1-line edit to mark the cross-reference RESOLVED with a pointer to ZEB-274's spec.

- [ ] **Step 1: Edit the line**

Find:

```markdown
* Membership-side parallel fix for ZEB-266 — same shape, separate ticket.
```

Replace with:

```markdown
* Membership-side parallel fix for [ZEB-266](https://linear.app/zeblith/issue/ZEB-266) — RESOLVED in [ZEB-274](https://linear.app/zeblith/issue/ZEB-274). Note the architecture differs (RAII rollback guard, not deferred-spawn) because the IPC handler interacts with the community-sync engine pre-commit (`engine.insert_local_event(bootstrap_join)`).
```

- [ ] **Step 2: Verify the edit**

```bash
grep -n "RESOLVED in.*ZEB-274\|same shape, separate ticket" docs/specs/2026-05-10-zeb-271-channel-log-registry-transactionality-design.md
```

Expected: shows the new line with "RESOLVED in [ZEB-274]" and 0 matches for "same shape, separate ticket".

- [ ] **Step 3: Commit**

```bash
git add docs/specs/2026-05-10-zeb-271-channel-log-registry-transactionality-design.md
git commit -m "$(cat <<'EOF'
docs(zeb-271): mark §9 membership-side cross-ref RESOLVED in ZEB-274

Per ZEB-274 spec §7.4 + §11 acceptance criterion #3. The membership-
side parallel fix uses a different architecture (RAII rollback guard
instead of ZEB-271's deferred-spawn) because the IPC handler interacts
with the community-sync engine pre-commit; the cross-ref note in
ZEB-271 §9 explains this for future readers.
EOF
)"
```

---

## Task 5: Final verification + push + PR

**Files:** None new. Verification + git operations only.

**Goal:** Confirm all 5 gates pass on the final branch state, push to remote, create PR with bare `Resolves ZEB-274` + linked refs to parent epics (per `feedback_linear_pr_auto_close` rule).

- [ ] **Step 1: Final pre-push gate sweep**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast
cd src-tauri && cargo check --locked --all-targets --features test-fixtures
cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit && npx vitest run
```

Expected: all 5 green. Test count should be 957 + 5 (Task 1) + 1 (Task 2 integration test, if scaffolded) + 1 (Task 3 race test, if scaffolded) = 962-964 depending on which integration tests landed.

- [ ] **Step 2: Sanity-check the diff against the spec**

```bash
git log --oneline origin/main..HEAD
git diff --stat origin/main..HEAD
```

Expected: 5 commits (spec + Task 1 + Task 2 + Task 3 + Task 4). Diff stat should show:
- `docs/specs/...zeb-274...design.md` ~415 insertions
- `docs/specs/...zeb-271...design.md` ~1 line changed
- `docs/plans/...zeb-274...plan.md` (this file) ~lines depending on plan length
- `src-tauri/src/community_state_sync.rs` ~+250 lines (guard + spawn_engine_with_guard + 5 tests)
- `src-tauri/src/lib.rs` net ~-200 lines (rollback boilerplate removed)

- [ ] **Step 3: Push the branch**

```bash
git push -u origin zeb-274-membership-side-transactionality
```

Expected: branch pushed; gh pr create now possible.

- [ ] **Step 4: Create PR**

```bash
gh pr create --title "ZEB-274: membership-side transactionality (RAII rollback guard)" --body "$(cat <<'EOF'
## Summary

- Adds `CommunitySyncSpawnGuard` RAII primitive on `CommunitySyncRegistry`. Wraps engine spawn + adapter dispatch atomically; Drop runs `shutdown_engine_and_cleanup_persistence` if this call freshly created the engine.
- Replaces 26 scattered `shutdown_engine_and_cleanup_persistence` rollback sites (9 in `create_community_inner`, 17 in `redeem_invite_inner`) with the guard. Removes the `engine_freshly_created: bool` plumbing entirely (internalized into the guard).
- Sibling fix to PR #99 ([ZEB-271](https://linear.app/zeblith/issue/ZEB-271)) — same problem (phantom community on aborted critical section) but different architecture per spec §2: RAII guard, not deferred-spawn, because the IPC handler interacts with the community-sync engine pre-commit.

Resolves ZEB-274.

## Architecture

Per `docs/specs/2026-05-10-zeb-274-membership-side-transactionality-design.md` (commit in this PR). Locked decisions in spec §8:

1. **RAII rollback guard** (NOT deferred-spawn). Engine MUST be live for `engine.insert_local_event(bootstrap_join)` to succeed.
2. **Copy-specialize** from [ZEB-271](https://linear.app/zeblith/issue/ZEB-271)'s Drop pattern. No shared trait (YAGNI; only one consumer of this shape).
3. **Full atomic refactor**: guard wraps engine spawn + adapter dispatch + freshness flag. The orphan-adapter-on-spawn-success-then-dispatch-fail case is eliminated.
4. **Sequential commits**: `community_sync_guard.commit()` (sync) FIRST, then `channel_log_tx.commit().await`.
5. **Freshness internalized**: removes the `Result<bool, _>` from `spawn_engine`'s public surface. Concurrent-redeem race losers' guards are no-ops on Drop (preserves [ZEB-260](https://linear.app/zeblith/issue/ZEB-260) PR #90 round-5 invariant).
6. **No-runtime Drop = acknowledged leak** per spec §10.2 (no sync alternative for `engine.shutdown().await`; reconcile recovers at next `start_node`).

Folds in a 1-line cross-ref update to [ZEB-271](https://linear.app/zeblith/issue/ZEB-271)'s spec §9 marking the "membership-side parallel fix" as RESOLVED.

Cross-references: parent epic [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) (Sub-C v2 channels-within-communities, DONE), Phase 1 [ZEB-266](https://linear.app/zeblith/issue/ZEB-266) (introduced the membership-changed CRDT), [ZEB-260](https://linear.app/zeblith/issue/ZEB-260) PR #90 round-5 (the freshness flag being internalized).

## Test plan

- [x] All 5 CI gates green: cargo fmt + clippy + nextest + check (msrv) from src-tauri/, npx tsc + vitest from repo root.
- [x] 5 new unit tests in `community_state_sync.rs::tests::guard_*` per spec §7.1: commit_releases_rollback, drop_without_commit_tears_down_fresh, drop_idempotent_call_is_noop, explicit_abort_tears_down, drop_no_runtime_logs_and_leaks.
- [x] 957 baseline tests still pass (no regression in existing community-sync coverage).
- [ ] Smoke test post-merge: create a community via the UI, verify it shows up + persists. Then trigger an invite-redeem failure path (e.g., malformed invite URL) and verify no leftover `~/.harmony/communities/<id>` directory.
EOF
)"
```

- [ ] **Step 5: Verify the PR was created**

```bash
gh pr view --json number,title,state,mergeable --jq '{number, title, state, mergeable}'
```

Expected: shows the new PR number with state "OPEN" and mergeable "MERGEABLE" (or "UNKNOWN" while CI runs initially).

**Do not push fixups in this task.** The autonomous loop's first wakeup will fetch bot reviews, dispatch fixup subagents, etc.

---

## Acceptance criteria recap (from spec §11)

1. ✅ Decision (RAII rollback guard) selected per §2; deferred-spawn rejected for IPC-pre-commit-engine-interaction reason. (Locked at brainstorming time.)
2. Implementation:
   - `CommunitySyncSpawnGuard` with `begin_spawn_guard` / `spawn_engine_with_guard` / `commit` / `abort` / `Drop` per §3 — Task 1
   - `create_community_inner` and `redeem_invite_inner` rewritten per §4 (26 explicit rollback sites collapse) — Tasks 2 + 3
   - All 5 registry-level unit tests per §7.1 passing — Task 1 Step 12
   - All 4 `_inner` integration tests per §7.2 + §7.3 passing (or deferred with implementer note) — Tasks 2 + 3
3. ZEB-271 spec §9 updated in this PR per §7.4 — Task 4
4. The freshness-flag (`engine_freshly_created` bool) removed from `spawn_engine` public surface; internalized into guard per §8 #6 — Task 1 Step 6 (rename) + Task 1 Step 9 (new method) + Task 3 Step 5 (remove local)
5. All 5 CI gates green per §11 #5 — Task 5 Step 1
