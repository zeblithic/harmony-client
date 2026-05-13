# ZEB-234 + ZEB-243: DM correctness — shutdown fence + convergent OutboxEntry delete

**Date:** 2026-05-12
**Branch:** `zeb-234-zeb-243-dm-correctness`
**Parents:** [ZEB-234](https://linear.app/zeblith/issue/ZEB-234) (parent epic [ZEB-225](https://linear.app/zeblith/issue/ZEB-225)) + [ZEB-243](https://linear.app/zeblith/issue/ZEB-243) (parent epic [ZEB-228](https://linear.app/zeblith/issue/ZEB-228))

## 1. Goal

Close two DM-correctness gaps surfaced by Sub-B bot reviewers, bundled into one PR because both touch the just-shipped outbox layer:

1. **ZEB-234 — shutdown fence for `send_dm`.** A race between `send_dm`'s mutation+post-check and `stop_inner`'s `SyncEngine::shutdown` flush can persist + broadcast an entry while `send_dm` returns `Err` — caller retries against the restarted node and the recipient sees a duplicate DM.
2. **ZEB-243 — convergent OutboxEntry delete via tombstones.** `DmOutbox::delete_dm_outbox_entry` removes locally but `merge_remote_into_local` doesn't see deletions — a paired device with the deleted entry will re-sync it, resurrecting the bubble.

## 2. Context — current state

### 2.1 ZEB-234 race window

Documented in code at [`src-tauri/src/lib.rs:3026-3038`](../../src-tauri/src/lib.rs):

```
T0: send_dm mutates crdt_state via cloned Arcs (apply_outbox)
T1: stop_inner fires SyncEngine::shutdown which final-flushes the
    cloned crdt_state — includes the just-installed entry
T2: send_dm's post-check sees generation changed, returns Err
T3: Caller retries against the new node — mints a fresh OutboxEntry
T4: Recipient receives both → duplicate DM
```

Window is microseconds wide (tracker insert + drop chain + sync mutex re-acquire). Phase 2 shipped with this race unreachable from any UI flow; Phase 4 ([ZEB-281](https://linear.app/zeblith/issue/ZEB-281), now merged) opens it for users who quickly click "send" then trigger a restart.

The current post-check is necessary but not sufficient: it can convert success into `Err` but cannot prevent the write that already happened.

### 2.2 ZEB-243 convergence gap

`DmOutbox::delete_dm_outbox_entry` at [`src-tauri/src/dm_outbox.rs:643`](../../src-tauri/src/dm_outbox.rs) (approximate) removes `state.outbox[entry_id]` locally and persists. Sync merge at [`src-tauri/src/owner_state_sync.rs:538-554`](../../src-tauri/src/owner_state_sync.rs) iterates remote.outbox via `apply_outbox`:

```rust
for (_, entry) in outbox {
    local.apply_outbox(entry);
}
```

Missing keys on the remote are not treated as deletions. A paired device that still has the deleted `OutboxEntry` re-inserts it on the next sync round, undoing the user's manual delete from the originating device.

No tombstone mechanism exists for `OutboxEntry`. Other CRDTs in the codebase use tombstone patterns (e.g., the channel-log layer); `OwnerState` does not yet.

## 3. Design — ZEB-234 shutdown fence

### 3.1 New `NodeState` fields

```rust
pub struct NodeState {
    // ... existing fields ...

    /// ZEB-234: shutdown fence. `send_dm` acquires a permit for the
    /// duration of mutation + post-check; `stop_inner` sets `stopping`
    /// and drains all permits before invoking `SyncEngine::shutdown`,
    /// guaranteeing no in-flight `send_dm` is mid-write when the final
    /// flush runs.
    ///
    /// `Some(_)` while a node is running; `None` after stop. Permits
    /// are bounded large (1024) — practical "unbounded" given typical
    /// IPC concurrency. Exhaustion would await rather than reject.
    dm_send_inflight: Option<std::sync::Arc<tokio::sync::Semaphore>>,
    /// ZEB-234: paired stopping flag. Set to `true` synchronously
    /// inside `stop_inner` BEFORE the permit drain, so freshly-
    /// arriving `send_dm` calls early-reject without waiting on the
    /// semaphore. Cleared (None'd) at the same point as
    /// `dm_send_inflight` for restart symmetry.
    dm_send_stopping: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}
```

Const: `pub const DM_SEND_FENCE_CAPACITY: usize = 1024;`

### 3.2 `start_node` initialization

Within `start_node`'s NodeState population (alongside the other handle inserts):

```rust
guard.dm_send_inflight = Some(std::sync::Arc::new(
    tokio::sync::Semaphore::new(DM_SEND_FENCE_CAPACITY),
));
guard.dm_send_stopping = Some(std::sync::Arc::new(
    std::sync::atomic::AtomicBool::new(false),
));
```

### 3.3 `send_dm` fence-acquire + double-check

Modify `send_dm` ([`src-tauri/src/lib.rs:2930`](../../src-tauri/src/lib.rs)) to acquire a permit BEFORE the mutation block and hold it until the IPC returns. Pseudo-shape:

```rust
async fn send_dm(...) -> Result<SendDmResult, String> {
    // Snapshot fence handles paired with existing handles.
    let (dm_outbox, ..., dm_send_inflight, dm_send_stopping) = {
        let g = state_lock.lock()...?;
        (..., 
         g.dm_send_inflight.clone().ok_or("node not running")?,
         g.dm_send_stopping.clone().ok_or("node not running")?)
    };

    // Pre-check: reject if stop_inner already initiated.
    if dm_send_stopping.load(Acquire) {
        return Err("node stopping; send_dm rejected".into());
    }

    // Acquire permit. Held via a guard `_permit` until the function
    // returns — including all error paths.
    let _permit = dm_send_inflight.clone()
        .acquire_owned().await
        .map_err(|_| "node stopping (semaphore closed)".to_string())?;

    // Re-check after acquire — stopping could have been set during the
    // acquire await. With permit held, stop_inner's drain blocks until
    // this re-check completes; either we reject here (releasing the
    // permit so drain proceeds) or we run with the guarantee that
    // `acquire_many(CAPACITY)` blocks until we drop our permit.
    if dm_send_stopping.load(Acquire) {
        return Err("node stopping; send_dm rejected".into());
    }

    // ... existing mutation + post-check logic unchanged ...
}
```

The existing post-check (generation + handles re-check) stays — it now covers the orthogonal "stop_inner happened but didn't run the fence" path (defensive).

### 3.4 `stop_inner` drain

Inside `stop_inner` ([`src-tauri/src/lib.rs:545`](../../src-tauri/src/lib.rs)), before the existing `SyncEngine::shutdown` block, drain in-flight `send_dm`s:

```rust
// Take the fence handles under the lock alongside other handles.
let (dm_send_inflight_for_drain, dm_send_stopping_for_drain) = {
    let mut guard = state.lock()...;
    (guard.dm_send_inflight.take(), guard.dm_send_stopping.take())
};

// Signal stopping (synchronous; new send_dm calls will early-reject).
if let Some(stopping) = &dm_send_stopping_for_drain {
    stopping.store(true, Release);
}

// Drain in-flight on an ephemeral current-thread runtime, mirroring
// the existing pattern at lib.rs:671 + lib.rs:738.
if let Some(sem) = dm_send_inflight_for_drain {
    std::thread::scope(|s| {
        s.spawn(|| {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => {
                    // acquire_many(CAPACITY) blocks until all in-flight
                    // permits have been returned. Drop the guard
                    // immediately — we just want the await-for-drain.
                    let _drain = rt.block_on(
                        sem.acquire_many(DM_SEND_FENCE_CAPACITY as u32)
                    );
                }
                Err(e) => {
                    tracing::warn!(?e, "failed to build drain runtime; \
                        proceeding with shutdown anyway (in-flight \
                        send_dm may produce duplicates)");
                }
            }
        });
    });
}

// ... existing SyncEngine::shutdown logic continues below ...
```

After the drain returns: no in-flight `send_dm` can be mid-write. `SyncEngine::shutdown`'s final flush is safe.

## 4. Design — ZEB-243 convergent delete

### 4.1 New `OwnerState` field

```rust
pub struct OwnerState {
    // ... existing fields ...

    /// ZEB-243: tombstones for deleted OutboxEntries. Map from
    /// OutboxEntryId to the HLC at which the delete occurred. LWW
    /// semantics on merge (older tombstone HLC loses to newer
    /// tombstone HLC, defensive against clock skew across paired
    /// devices). `apply_outbox` checks this map: an incoming entry
    /// with `created_at` HLC strictly older than its matching
    /// tombstone is rejected.
    ///
    /// Entry IDs are 16-byte ULIDs minted fresh per send — collision
    /// is not a concern. Tombstones are retained indefinitely (no GC
    /// in this PR; outbox is bounded by 30-day expiration so the
    /// growth rate is low).
    pub outbox_tombstones: std::collections::BTreeMap<OutboxEntryId, Hlc>,
}
```

### 4.2 `delete_dm_outbox_entry` writes tombstone

Modify [`src-tauri/src/dm_outbox.rs:643`](../../src-tauri/src/dm_outbox.rs)'s `delete_dm_outbox_entry`:

After the existing local `state.outbox.remove(&entry_id)`, insert the tombstone:

```rust
state.outbox.remove(&entry_id);
state.outbox_tombstones.insert(entry_id, now_hlc);
```

`now_hlc` MUST come from the same HLC source as outbox entry creation (i.e., the `hlc_tracker`'s next HLC for this device), so a tombstone is guaranteed strictly later than any entry it could be deleting on this device.

### 4.3 `apply_outbox` honors tombstones

Modify [`src-tauri/src/owner_state_crdt.rs:284`](../../src-tauri/src/owner_state_crdt.rs)'s `apply_outbox`:

```rust
pub fn apply_outbox(&mut self, incoming: OutboxEntry) -> ApplyOutcome {
    // Tombstone check first — strict-greater-than semantics.
    if let Some(tombstone_hlc) = self.outbox_tombstones.get(&incoming.id) {
        if tombstone_hlc > &incoming.created_at {
            return ApplyOutcome::Rejected(RejectionReason::Tombstoned);
        }
    }
    // ... existing apply logic unchanged ...
}
```

Add `RejectionReason::Tombstoned` variant. Note: ULIDs are unique-per-send so a true "tombstone older than incoming" case shouldn't arise from honest peers — but defensive comparison covers clock skew + future use cases where IDs might be reused.

### 4.4 `merge_remote_into_local` applies tombstones first

Modify [`src-tauri/src/owner_state_sync.rs:538`](../../src-tauri/src/owner_state_sync.rs)'s `merge_remote_into_local`. Order:

1. **Apply remote tombstones (NEW first step):**
   - For each `(id, remote_hlc)` in `remote.outbox_tombstones`:
     - LWW merge into local: `local.outbox_tombstones.entry(id).and_modify(|h| if remote_hlc > *h { *h = remote_hlc.clone() }).or_insert(remote_hlc)`
     - Sweep matching local outbox: if `local.outbox.get(&id).is_some_and(|e| e.created_at < merged_tombstone_hlc)`, `local.outbox.remove(&id)`
2. Existing outbox application loop (now respects merged tombstones).
3. Existing remaining merge steps (spaces, inbox, markers, etc.).

The current merge ordering comment in the file says "spaces first → outbox → inbox → markers → tombstones" — the new step inserts an `outbox_tombstones` step BEFORE the existing `outbox` step. Update the comment.

### 4.5 Persistence round-trip

The new field must round-trip wherever `OwnerState` is serialized. Implementer locates the persistence path (canonical-CBOR encode/decode for OwnerState) and threads the new field through:

- Serialization: include `outbox_tombstones` in encode.
- Deserialization: tolerate absent field for backward-compat with pre-tombstone snapshots (default = empty map).

## 5. Implementation order

1. **Backend ZEB-234 — fence** (smaller, contained in lib.rs):
   - Add NodeState fields + DM_SEND_FENCE_CAPACITY const
   - Update `start_node` initialization
   - Update `send_dm` to acquire+hold permit + early-reject on stopping
   - Update `stop_inner` to set stopping + drain
   - Tests: in-flight send blocks shutdown until complete; new sends rejected after stopping
2. **Backend ZEB-243 — tombstones**:
   - Add `outbox_tombstones` field to OwnerState
   - Add `RejectionReason::Tombstoned`
   - Update `apply_outbox` with tombstone check
   - Update `delete_dm_outbox_entry` to write tombstone alongside removal
   - Update `merge_remote_into_local` with tombstone-first ordering
   - Persistence round-trip
   - Tests: persistence round-trip; CRDT convergence (A creates, B deletes, sync, both empty); apply_outbox rejects old vs accepts new

No frontend changes — both are server-side correctness fixes.

## 6. Testing

### 6.1 ZEB-234 unit tests (in `lib.rs` test module or a dedicated `tests/` integration test)

- `send_dm_blocks_until_in_flight_completes`: spawn a slow `send_dm` (mock the inner work to await for a signal); call `stop_inner` from another task; assert `stop_inner` blocks until the `send_dm` future is allowed to complete + drop its permit.
- `send_dm_rejects_after_stopping_set`: set the stopping flag; call `send_dm`; assert immediate Err with "node stopping" message.
- `start_node_after_stop_initializes_fresh_fence`: stop, start again; assert fence handles are fresh (not the prior stopping=true semaphore).

### 6.2 ZEB-243 unit + integration tests

- `outbox_tombstones_persist_across_save_load_roundtrip` (`owner_state_crdt` tests or persistence test file).
- `apply_outbox_rejects_entry_older_than_tombstone` (`owner_state_crdt` tests).
- `apply_outbox_accepts_entry_newer_than_tombstone` (sanity counter-case).
- `delete_dm_outbox_entry_writes_tombstone` (`dm_outbox` tests).
- `merge_remote_into_local_applies_tombstones_before_outbox_and_sweeps_local` (`owner_state_sync` tests).
- **CRDT convergence test** (could live in `owner_state_sync` integration tests):
  - Device A creates OutboxEntry X
  - Sync to device B (B now has X)
  - Device B deletes X via `delete_dm_outbox_entry` (writes tombstone at HLC T2 > X.created_at)
  - Sync A ← B (A receives tombstone, sweeps X)
  - Assert: both states have empty `outbox`, tombstones map has X with HLC T2
  - Sync B ← A (idempotent, no change)
  - Assert: both states still converged

## 7. Acceptance criteria

1. New IPC behavior:
   - `send_dm` returns `Err("node stopping ...")` if called after `stop_inner` initiated, BEFORE any state mutation.
   - `stop_inner` blocks (synchronously) until all in-flight `send_dm` calls complete + drop their permits, BEFORE invoking `SyncEngine::shutdown`.
2. `OwnerState.outbox_tombstones` field exists, persists across save/load, default-empty for legacy snapshots.
3. `delete_dm_outbox_entry` writes a tombstone alongside the removal.
4. `apply_outbox` rejects an entry whose `created_at` HLC is strictly less than its matching tombstone's HLC.
5. `merge_remote_into_local` applies tombstones (with sweep of matching local outbox entries) BEFORE merging remote outbox entries.
6. CRDT convergence: device A creates, device B deletes, sync — both end with empty `outbox`.
7. All local gates green: `cargo fmt`, `cargo clippy`, `cargo nextest`, `cargo check`, `npx tsc`, `npx vitest`.

## 8. Out of scope

- **Tombstone GC.** Outbox is bounded by 30-day expiration; tombstone growth is naturally bounded. A future PR can add age-based GC if it becomes a problem.
- **Inbox tombstones.** Same convergence gap exists for `InboxEntry` deletes but the user-facing surface is smaller; defer to a separate ticket if/when manual InboxEntry deletes are exposed.
- **send_dm permit budgeting.** 1024 is "practical unbounded"; if pathological concurrency saturates, await-on-permit is acceptable behavior (not a refusal). Reduce only if profiling shows contention.
- **Fence semantics for other DM IPCs.** Only `send_dm` writes via `apply_outbox` from the IPC layer; the drain IPC is internal to the event_loop tick. If future IPCs add similar write paths, they get their own permit acquire.

## 9. References

- [ZEB-234 ticket](https://linear.app/zeblith/issue/ZEB-234) — fence design recommendation
- [ZEB-243 ticket](https://linear.app/zeblith/issue/ZEB-243) — tombstone design recommendation
- `src-tauri/src/lib.rs:2930` — current `send_dm` body with documented race
- `src-tauri/src/lib.rs:545` — current `stop_inner` (sync barrier)
- `src-tauri/src/lib.rs:671` + `src-tauri/src/lib.rs:738` — existing `thread::scope` + ephemeral-runtime pattern
- `src-tauri/src/owner_state_crdt.rs:284` — current `apply_outbox`
- `src-tauri/src/owner_state_sync.rs:538` — current `merge_remote_into_local`
- `src-tauri/src/dm_outbox.rs:643` — current `delete_dm_outbox_entry`
