# Owner-state persist-only durability fence — implementation plan

**Goal:** Make `redeem_invite`/`create_community` owner-state durability survive a
crash under "no responder" by fencing with a publish-independent persist, fixing
the ZEB-509 epoch-loss deadlock.

**Architecture:** Add a persist-only path to the generic `FleetSyncEngine<S>`
(mirroring the community engine's `persist_now`), expose it on the
`owner_state_sync::SyncEngine` wrapper, and switch `fence_owner_state_flush` from
`flush_now` (publish-then-persist) to `persist_now` (+ best-effort `notify_dirty`
for propagation).

**Tech stack:** Rust, tokio mpsc/oneshot, `cargo nextest`. All cargo from
`src-tauri/`.

**Reference:** design at
`docs/specs/2026-06-20-owner-state-persist-only-durability-fence-design.md`.
Templates to mirror: community `persist_now` method
(`community_state_sync.rs:1596`), its task arm (`:2276`), its unit test
`persist_now_fences_crdt_without_publishing_zeb462` (`:5453`); `FleetSyncEngine`
`flush_now` plumbing (`fleet_sync.rs:163/198/222/229/258/362/438`).

---

## Task 1: `FleetSyncEngine<S>` persist-only path + unit test

**Files:**
- Modify: `src-tauri/src/fleet_sync.rs`

TDD — write the failing test first, then the plumbing.

**Step 1 — failing unit test.** Add to the `fleet_sync.rs` test module. Mirror the
harness of an existing `fleet_sync.rs` test for engine construction
(`FleetSyncConfig`, mock/inspectable `persist` backend, and the `publisher_tx`
channel) and the assertion shape of
`community_state_sync.rs::persist_now_fences_crdt_without_publishing_zeb462`. The
test MUST assert all of:
1. After a local state mutation, `engine.persist_now().await` returns `Ok(())`.
2. The persist backend received the mutated state (state reached disk).
3. The `publisher_tx` (outbound publish channel) received **zero** messages
   during `persist_now` (no publish happened).
4. `persist_now` completes even when the `publisher_tx` channel is pre-saturated
   to capacity (the property `flush_now` lacks — a saturated publisher would
   block `flush_now`'s publish leg). Construct the engine with a bounded
   publisher channel, fill it to capacity, then call `persist_now` and assert it
   still returns `Ok` promptly. (If the existing harness uses an unbounded or
   drained channel, add a bounded-and-saturated variant for this assertion.)

Name it `persist_now_persists_without_publishing` (+ a `_under_saturated_publisher`
variant if cleaner as two tests).

**Step 2 — run, expect compile failure** (`persist_now` undefined on
`FleetSyncEngine`). Run: `cd src-tauri && cargo nextest run --locked -p harmony-app
--lib --features test-fixtures -E 'test(persist_now_persists_without_publishing)'`.

**Step 3 — add the struct field.** In `pub struct FleetSyncEngine<S>` after
`flush_now_tx` (`fleet_sync.rs:163`):

```rust
    persist_now_tx: mpsc::Sender<tokio::sync::oneshot::Sender<Result<(), SyncError>>>,
```

**Step 4 — create the channel + wire it in `new`.** After the `flush_now` channel
(`:198`):

```rust
        let (persist_now_tx, persist_now_rx) = mpsc::channel(8);
```

In the `Ctx { ... }` passed to `internal_task`, after `flush_now_rx,` (`:222`):

```rust
            persist_now_rx,
```

In the returned `FleetSyncEngine { ... }`, after `flush_now_tx,` (`:229`):

```rust
            persist_now_tx,
```

**Step 5 — add the `Ctx` field.** In `struct Ctx<S>` after `flush_now_rx`
(`:362`):

```rust
    persist_now_rx: mpsc::Receiver<tokio::sync::oneshot::Sender<Result<(), SyncError>>>,
```

**Step 6 — add the public method.** After `flush_now` (`:265`):

```rust
    /// Force an immediate durable persist WITHOUT publishing. Returns when the
    /// on-disk write has completed. Unlike `flush_now`, this never touches the
    /// network publish path, so durability cannot be starved by a stalled publish
    /// (e.g. no zenoh responder). Used by the durable-on-commit fences where the
    /// only requirement is that local state reach disk; any pending state-root
    /// publish still fires on the next debounce. Mirrors
    /// `community_state_sync::CommunitySyncEngine::persist_now`.
    pub async fn persist_now(&self) -> Result<(), SyncError> {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        self.persist_now_tx
            .send(resp_tx)
            .await
            .map_err(|_| SyncError::TransportClosed)?;
        resp_rx.await.map_err(|_| SyncError::TransportClosed)?
    }
```

**Step 7 — add the `select!` arm.** In `internal_task`, alongside the `flush_now_rx`
arm (`:438`), add:

```rust
            Some(resp_tx) = ctx.persist_now_rx.recv() => {
                // Publish-INDEPENDENT durable persist (durable-on-commit fence).
                // Persists state + tracker to disk without the publish leg, so a
                // stalled publish (no zenoh responder) can never starve durability
                // — the ZEB-509 bug this path fixes. Deliberately does NOT touch
                // `has_pending_dirty`: any pending state-root publish still fires
                // on the next debounce / flush_now. Mirrors the community engine's
                // persist_now arm.
                let persist_result = persist_now(&ctx).await;
                let _ = resp_tx.send(persist_result);
            }
```

**Step 8 — run the test, expect PASS.** Same command as Step 2.

**Step 9 — commit.** `git add -A && git commit` (message: persist-only path on
FleetSyncEngine + test; do NOT put a ZEB id in the commit message).

---

## Task 2: expose `persist_now` on `owner_state_sync::SyncEngine`

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs`

**Step 1 — add the delegate.** After the `flush_now` delegate (`:183-185`):

```rust
    /// Durably persist owner-state to disk WITHOUT publishing. See
    /// `FleetSyncEngine::persist_now`. Used by `fence_owner_state_flush` so a
    /// stalled state-root publish can't starve owner-state durability (ZEB-509).
    pub async fn persist_now(&self) -> Result<(), SyncError> {
        self.inner.persist_now().await
    }
```

**Step 2 — compile check.** `cd src-tauri && cargo check --locked -p harmony-app
--lib --features test-fixtures`. Expected: clean.

**Step 3 — commit.**

---

## Task 3: switch `fence_owner_state_flush` to persist-only + best-effort publish

**Files:**
- Modify: `src-tauri/src/lib.rs` (`fence_owner_state_flush`, `:39486`)

**Step 1 — replace the helper body + doc comment.** Replace the existing
`fence_owner_state_flush` (and its doc comment at `:39475-39485`) with:

```rust
/// ZEB-509 durable-on-commit fence, bounded. Persist a just-committed owner-state
/// Space mutation (community create / join / leave-adjacent write) to
/// `owner_state_crdt.cbor` before the calling IPC returns, using the
/// publish-INDEPENDENT persist path.
///
/// Why persist-only: `flush_now` publishes before it persists, so under
/// "no responder" the publish leg back-pressures past `timeout`, the future is
/// cancelled mid-publish, and persist never runs — losing the just-committed
/// Space on a later crash (ZEB-509: redeemer reloads `spaces: {}` →
/// `LiveEpochKeyMissing` → "no responder" deadlock). `persist_now` writes to disk
/// without the publish leg, so durability always lands. Then `notify_dirty` arms
/// the debounce so the owner-state root still propagates to sibling devices,
/// best-effort, never blocking durability. Mirrors `fence_community_crdt_persist`.
///
/// Bounded + non-fatal: a wedged engine task can't hang create/redeem; on persist
/// error/timeout we log + leave the debounce armed.
///
/// `context` names the calling IPC in the warning; `community_id` is the hex space
/// id for log correlation.
pub(crate) async fn fence_owner_state_flush(
    engine: &crate::owner_state_sync::SyncEngine,
    timeout: std::time::Duration,
    context: &'static str,
    community_id: &str,
) {
    match tokio::time::timeout(timeout, engine.persist_now()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!(
                error = %e,
                community_id = %community_id,
                "{context}: owner-state persist_now failed; debounce left armed"
            );
        }
        Err(_elapsed) => {
            tracing::warn!(
                timeout_ms = timeout.as_millis() as u64,
                community_id = %community_id,
                "{context}: owner-state persist_now timed out; debounce left armed"
            );
        }
    }
    // Best-effort propagation of the owner-state root to sibling devices — the
    // publish `flush_now` used to do synchronously. Fires on the next debounce;
    // never blocks durability. Also serves as the re-arm on the error/timeout
    // arms above.
    engine.notify_dirty();
}
```

(The helper name `fence_owner_state_flush` is intentionally retained to keep the
diff scoped — all 7 call sites are unchanged.)

**Step 2 — gate (lib-scoped).** `cd src-tauri && cargo nextest run --locked -p
harmony-app --lib --features test-fixtures` and `cargo clippy --locked -p
harmony-app --lib --features test-fixtures --no-deps -- -D warnings` and `cargo fmt
--all -- --check`. Expected: green.

**Step 3 — commit.**

---

## Task 4: full gate + final review

**Step 1 — full workspace gate** from `src-tauri/`:
`cargo fmt --all -- --check`
`cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
`cargo nextest run --locked --workspace --all-targets --features test-fixtures`

Expected: all green. (Pre-existing iroh/zenoh transport orphan-flakes, if any, are
non-blocking — confirm any failure is unrelated to this change before proceeding.)

**Step 2 — self-review the diff** against the design spec: persist-only fence,
publish preserved via `notify_dirty`, §10.6 serve guard untouched, no ZEB ids in
commit messages.

**Step 3 — commit any fixups.**
