# ZEB-234 + ZEB-243: DM correctness implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Each implementation task ends with a commit; Task 0 is verification-only.

**Goal:** Land the shutdown-fence (ZEB-234) and outbox-tombstone (ZEB-243) fixes in one PR per the spec at `docs/specs/2026-05-12-zeb-234-zeb-243-dm-correctness-design.md`.

**Architecture:** Two server-side correctness fixes sharing the just-touched DM/outbox surface. ZEB-234 is concurrency/lifecycle (semaphore-based fence in `NodeState`); ZEB-243 is CRDT data-model (`outbox_tombstones` field on `OwnerState`, LWW HLC, merge-first ordering). No frontend changes.

**Tech Stack:** Rust (tokio, std::sync). All local gates (CI is disabled per `feedback_ci_disabled`).

---

## Task 0 — Preflight + green baseline (no commit)

**Files:** N/A (verification only).

- [ ] **Step 1: Confirm branch state**

```bash
git rev-parse --abbrev-ref HEAD
# Expected: zeb-234-zeb-243-dm-correctness
git log --oneline origin/main..HEAD
# Expected: 1 commit — the spec commit
```

- [ ] **Step 2: Run all local gates from clean baseline**

From `src-tauri/`:
```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo check --locked --all-targets --features test-fixtures
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
From repo root:
```bash
npx tsc --noEmit
npx vitest run
```

Expected: all green. Capture baseline test counts (Rust + vitest) for regression check at the end.

- [ ] **Step 3: Note test counts**

Record the Rust nextest "Summary [Ns] M tests run: M passed, K skipped" line and the vitest "Test Files N passed (N)" line. Will be referenced in Task 7's verification.

---

## Task 1 — NodeState fence fields + start_node init

**Files:**
- Modify: `src-tauri/src/lib.rs` (NodeState struct around line 176; start_node around line 864)

- [ ] **Step 1: Add `DM_SEND_FENCE_CAPACITY` const**

Near the top of `lib.rs` (after the existing top-level `pub const`s or near the start of the NodeState region), add:

```rust
/// ZEB-234: shutdown-fence permit count for `send_dm`. Practical
/// "unbounded" for typical IPC concurrency; exhaustion awaits rather
/// than rejects. `stop_inner` drains all permits via `acquire_many`
/// to guarantee no in-flight `send_dm` is mid-write when
/// `SyncEngine::shutdown` runs.
pub const DM_SEND_FENCE_CAPACITY: usize = 1024;
```

- [ ] **Step 2: Add the two new NodeState fields**

In the `pub struct NodeState { ... }` block at `lib.rs:176`, add (near the existing DM-related fields like `dm_outbox`, `dm_transport`, etc.):

```rust
    /// ZEB-234: shutdown fence. `send_dm` acquires a permit for the
    /// duration of mutation + post-check; `stop_inner` sets `stopping`
    /// then drains all permits before `SyncEngine::shutdown`.
    /// `Some(_)` while running, `None` after stop.
    dm_send_inflight: Option<std::sync::Arc<tokio::sync::Semaphore>>,
    /// ZEB-234: paired stopping flag. Set synchronously in
    /// `stop_inner` BEFORE the permit drain so newly-arriving
    /// `send_dm` calls early-reject. Cleared (None'd) in symmetry
    /// with `dm_send_inflight`.
    dm_send_stopping: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
```

- [ ] **Step 3: Update the NodeState default/initializer**

Search for the NodeState constructor / Default impl. Add the two new fields initialized to `None` (matching the existing pattern for other Optional handles).

- [ ] **Step 4: Initialize in `start_node`**

Within `start_node` at `lib.rs:864`, find the block where DM-related handles are written into the `NodeState` guard (look for `guard.dm_outbox = Some(...)`). Add adjacent to those:

```rust
guard.dm_send_inflight = Some(std::sync::Arc::new(
    tokio::sync::Semaphore::new(DM_SEND_FENCE_CAPACITY),
));
guard.dm_send_stopping = Some(std::sync::Arc::new(
    std::sync::atomic::AtomicBool::new(false),
));
```

- [ ] **Step 5: Verify compile + fmt + clippy**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo check --locked --all-targets --features test-fixtures
```

Expected: all green. (No new tests yet, so nextest is unchanged.)

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-234): add NodeState fence fields + start_node init

Adds dm_send_inflight (Arc<Semaphore>) and dm_send_stopping
(Arc<AtomicBool>) fields to NodeState. Initialized at start_node
with DM_SEND_FENCE_CAPACITY=1024 permits and stopping=false.
Cleared by stop_inner in a later task (this commit just lands the
shape; send_dm + stop_inner integration in subsequent commits).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2 — send_dm: fence-acquire + early-reject

**Files:**
- Modify: `src-tauri/src/lib.rs` (`send_dm` around line 2930)

- [ ] **Step 1: Write the failing test first**

Add to wherever `send_dm` tests live (search for existing `send_dm` test module — likely an integration test at `src-tauri/tests/` or a `#[cfg(test)]` module in `lib.rs`). Test:

```rust
#[tokio::test]
async fn send_dm_rejects_after_stopping_flag_set() {
    // Build a NodeState fixture with fence handles initialized.
    // Set dm_send_stopping to true.
    // Call send_dm — assert Err with "node stopping" in the message
    // and no mutation happened (outbox empty).
}
```

Use existing test scaffolding patterns from prior send_dm tests if any; otherwise wire the minimum NodeState the IPC needs. If no clear existing test harness, place test in a new file `src-tauri/tests/send_dm_fence_integration.rs` with a minimal fixture.

- [ ] **Step 2: Run test to verify it fails**

```bash
cd src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(send_dm_rejects_after_stopping_flag_set)'
```

Expected: FAIL (send_dm doesn't yet check stopping flag).

- [ ] **Step 3: Update `send_dm` to snapshot fence handles**

In the existing snapshot block in `send_dm` (around `lib.rs:2945`), extend the destructuring to include `dm_send_inflight` and `dm_send_stopping`:

```rust
let (
    dm_outbox,
    _dm_transport,
    crdt_state,
    hlc_tracker,
    device_id,
    _self_owner,
    cas,
    snapshot_generation,
    dm_send_inflight,
    dm_send_stopping,
) = {
    let g = state_lock
        .lock()
        .map_err(|e| format!("NodeState poisoned: {e}"))?;
    (
        g.dm_outbox.clone().ok_or("node not running or no owner identity")?,
        g.dm_transport.clone().ok_or("dm_transport missing")?,
        g.crdt_state.clone().ok_or("crdt_state missing")?,
        g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
        g.dm_device_id.clone().ok_or("dm_device_id missing")?,
        g.dm_self_owner.ok_or("dm_self_owner missing")?,
        g.content_store.clone().ok_or("content_store missing")?,
        g.generation,
        g.dm_send_inflight.clone().ok_or("node not running (no fence)")?,
        g.dm_send_stopping.clone().ok_or("node not running (no fence)")?,
    )
};
```

- [ ] **Step 4: Add pre-check + permit acquire + re-check**

After the snapshot block, before the existing hex-decode work:

```rust
// ZEB-234: shutdown fence. Pre-check the stopping flag — if set,
// short-circuit before any work. Then acquire a permit for the
// duration of mutation + post-check; stop_inner's acquire_many
// drain blocks on this permit, preventing it from racing the
// flush. Re-check stopping after acquire (could have been set
// during the await).
use std::sync::atomic::Ordering;
if dm_send_stopping.load(Ordering::Acquire) {
    return Err("node stopping; send_dm rejected".into());
}
let _fence_permit = dm_send_inflight
    .clone()
    .acquire_owned()
    .await
    .map_err(|_| "node stopping (semaphore closed)".to_string())?;
if dm_send_stopping.load(Ordering::Acquire) {
    return Err("node stopping; send_dm rejected".into());
}
```

The `_fence_permit` is held until function return; all `Err(...)?` paths drop it correctly via Drop.

- [ ] **Step 5: Run test to verify it passes**

```bash
cargo nextest run --locked --features test-fixtures -E 'test(send_dm_rejects_after_stopping_flag_set)'
```

Expected: PASS.

- [ ] **Step 6: Verify full gate set still green**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

- [ ] **Step 7: Commit**

```bash
git add -u
git commit -m "feat(zeb-234): send_dm acquires fence permit + early-rejects on stopping

Snapshot dm_send_inflight + dm_send_stopping from NodeState.
Pre-check stopping (fast-path Err before any work). Acquire owned
permit; re-check stopping after acquire (race window during
await). Permit held until IPC return, drained by stop_inner.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3 — stop_inner: set stopping flag + drain in-flight

**Files:**
- Modify: `src-tauri/src/lib.rs` (`stop_inner` around line 545)

- [ ] **Step 1: Write the failing test**

Add a test that asserts `stop_inner` blocks until an in-flight `send_dm` completes:

```rust
#[tokio::test]
async fn stop_inner_blocks_until_in_flight_send_dm_completes() {
    // Build NodeState fixture with fence handles + a dm_outbox that
    // hangs on a signal (use a oneshot channel or similar).
    // Spawn a task that calls send_dm — it acquires the permit and
    // blocks inside the mutation, awaiting the test's signal.
    // Spawn a second task that calls stop_inner — it should set
    // stopping and try to acquire_many(CAPACITY), blocking until
    // the first task completes.
    // Assert: stop_inner has NOT returned while send_dm is blocked.
    // Signal the send_dm task to complete; assert stop_inner now
    // returns.
}
```

If the test harness is too involved, an acceptable alternative: a unit test that exercises just the drain logic (extract the drain block into a small helper fn that takes `Arc<Semaphore>` + `Arc<AtomicBool>` and verify via a controlled scenario). Implementer picks whichever fits the existing test idioms.

- [ ] **Step 2: Run test to verify failure**

Expected: FAIL (stop_inner doesn't drain yet).

- [ ] **Step 3: Take fence handles in stop_inner's lock block**

Inside `stop_inner` at `lib.rs:545`, in the existing destructuring tuple where other handles are `.take()`'d (around lines 594-622), append:

```rust
guard.dm_send_inflight.take(),
guard.dm_send_stopping.take(),
```

And bind them at the destructure site: `..., dm_send_inflight, dm_send_stopping, ) = { ... };`

- [ ] **Step 4: Set stopping flag immediately after lock-drop**

Right after the lock-scope ends (after the `let (...) = { ... };` block):

```rust
// ZEB-234: signal stopping synchronously so any send_dm currently
// in its pre-acquire pre-check fast-rejects without queuing.
if let Some(stopping) = &dm_send_stopping {
    stopping.store(true, std::sync::atomic::Ordering::Release);
}
```

- [ ] **Step 5: Drain in-flight in a thread::scope block**

Before the existing `SyncEngine::shutdown` block at lib.rs:671 (or wherever the existing thread::scope+ephemeral-runtime pattern starts for SyncEngine shutdown), add a similar drain for the fence:

```rust
// ZEB-234: drain in-flight send_dm permits before SyncEngine
// final flush. `acquire_many(CAPACITY)` blocks until every
// outstanding permit has been returned — guaranteeing no
// send_dm is mid-mutation when the flush runs. Mirror the
// existing ephemeral-runtime pattern below for !Send-safe
// awaiting inside this sync function.
if let Some(sem) = dm_send_inflight {
    std::thread::scope(|s| {
        s.spawn(|| {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => {
                    let _drain = rt.block_on(
                        sem.acquire_many(DM_SEND_FENCE_CAPACITY as u32)
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        ?e,
                        "ZEB-234: failed to build drain runtime; \
                         proceeding with shutdown (in-flight \
                         send_dm may produce duplicates)"
                    );
                }
            }
        });
    });
}
```

- [ ] **Step 6: Run drain test to verify pass**

Expected: PASS.

- [ ] **Step 7: Run full Rust gate set**

- [ ] **Step 8: Commit**

```bash
git add -u
git commit -m "feat(zeb-234): stop_inner drains in-flight send_dm before flush

Take fence handles inside the existing lock block; set the
stopping flag synchronously after lock-drop; then acquire_many
(CAPACITY) on the semaphore inside an ephemeral current-thread
runtime (mirror lib.rs:671/738 pattern). Drain blocks until every
in-flight send_dm has dropped its permit — guarantees
SyncEngine::shutdown does not flush a mid-mutation crdt_state.

Closes the duplicate-DM race documented in send_dm's residual-
TOCTOU comment.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 4 — OwnerState: add `outbox_tombstones` field + persistence

**Files:**
- Modify: `src-tauri/src/owner_state_crdt.rs` (OwnerState struct around line 23; ApplyOutcome / RejectionReason)
- Modify: persistence file (implementer locates — search for `impl OwnerState` + serde or canonical-CBOR encode/decode)

- [ ] **Step 1: Write the failing persistence round-trip test**

In whichever module tests `OwnerState` persistence (search for existing roundtrip tests for `OwnerState`):

```rust
#[test]
fn outbox_tombstones_round_trip_via_canonical_cbor() {
    let mut state = OwnerState::default();
    let id = OutboxEntryId([0x42; 16]);
    let hlc = Hlc { wall_ms: 1_000, logical: 5, device_id: "dev-a".into() };
    state.outbox_tombstones.insert(id, hlc.clone());

    // Encode then decode via the same path normal OwnerState uses.
    let bytes = /* canonical CBOR encode */;
    let recovered: OwnerState = /* canonical CBOR decode */;

    assert_eq!(recovered.outbox_tombstones.get(&id), Some(&hlc));
}
```

- [ ] **Step 2: Run test, expect FAIL** (field doesn't exist yet).

- [ ] **Step 3: Add `outbox_tombstones` field**

In `OwnerState` struct definition at `owner_state_crdt.rs:23`:

```rust
    /// ZEB-243: tombstones for deleted OutboxEntries. Map from
    /// OutboxEntryId to the HLC at which the delete was applied
    /// locally. LWW semantics on merge: an older tombstone HLC
    /// loses to a newer one. `apply_outbox` rejects incoming
    /// entries whose `created_at` HLC is strictly older than
    /// their matching tombstone.
    ///
    /// ULIDs are unique per send, so collisions across honest
    /// peers are impossible; the HLC comparison is defensive
    /// against clock skew. No GC in this PR — outbox itself is
    /// bounded by 30-day expiration, so tombstone growth is low.
    #[serde(default)]
    pub outbox_tombstones: std::collections::BTreeMap<OutboxEntryId, Hlc>,
```

Note `#[serde(default)]` if serde is used directly; otherwise mirror the persistence approach for backward-compat (legacy snapshots with no tombstones field decode as empty map).

- [ ] **Step 4: Add `RejectionReason::Tombstoned` variant**

Find the `RejectionReason` enum definition (in `owner_state_crdt.rs` near `ApplyOutcome`) and add:

```rust
    /// ZEB-243: incoming entry has a matching tombstone with HLC
    /// strictly greater than the entry's `created_at` HLC.
    Tombstoned,
```

- [ ] **Step 5: Ensure encoder/decoder threads the new field**

Implementer locates the OwnerState persistence path (most likely canonical-CBOR via `crate::owner_state_crypto::canonical_cbor_encode` or similar). If `#[serde(default)]` covers backward-compat, no further changes; else add explicit handling.

- [ ] **Step 6: Run test, expect PASS**

- [ ] **Step 7: Verify full gate set green**

- [ ] **Step 8: Commit**

```bash
git add -u
git commit -m "feat(zeb-243): add outbox_tombstones field + Tombstoned rejection

OwnerState gains BTreeMap<OutboxEntryId, Hlc>. Persistence round-
trips via #[serde(default)] (legacy snapshots decode as empty).
RejectionReason::Tombstoned variant for apply_outbox path (wired
in next commit).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 5 — apply_outbox tombstone check + delete writes tombstone

**Files:**
- Modify: `src-tauri/src/owner_state_crdt.rs` (`apply_outbox` around line 284)
- Modify: `src-tauri/src/dm_outbox.rs` (`delete_dm_outbox_entry` around line 643)

- [ ] **Step 1: Write failing test for apply_outbox tombstone rejection**

In `owner_state_crdt.rs`'s test module:

```rust
#[test]
fn apply_outbox_rejects_entry_older_than_tombstone() {
    let mut state = OwnerState::default();
    let id = OutboxEntryId([0x11; 16]);
    let entry_hlc = Hlc { wall_ms: 1_000, logical: 0, device_id: "dev".into() };
    let tomb_hlc = Hlc { wall_ms: 2_000, logical: 0, device_id: "dev".into() };
    state.outbox_tombstones.insert(id, tomb_hlc);

    let entry = /* OutboxEntry with id and created_at = entry_hlc */;
    let outcome = state.apply_outbox(entry);

    assert!(matches!(outcome, ApplyOutcome::Rejected(RejectionReason::Tombstoned)));
    assert!(!state.outbox.contains_key(&id));
}

#[test]
fn apply_outbox_accepts_entry_newer_than_tombstone() {
    // Mirror but with tomb_hlc < entry_hlc → expect Applied + outbox contains id.
}
```

- [ ] **Step 2: Run, expect FAIL**

- [ ] **Step 3: Add tombstone check at top of `apply_outbox`**

In `owner_state_crdt.rs:284`'s `apply_outbox`, before any existing logic:

```rust
pub fn apply_outbox(&mut self, incoming: OutboxEntry) -> ApplyOutcome {
    // ZEB-243: tombstone gate. Strict-greater-than semantics —
    // tombstone wins iff its HLC is strictly newer than the
    // entry's `created_at`. Equal HLCs (theoretically impossible
    // since tombstones are written after entries) fall through.
    if let Some(tombstone_hlc) = self.outbox_tombstones.get(&incoming.id) {
        if tombstone_hlc > &incoming.created_at {
            return ApplyOutcome::Rejected(RejectionReason::Tombstoned);
        }
    }
    // ... existing apply logic continues unchanged ...
}
```

- [ ] **Step 4: Run rejection test, expect PASS**

- [ ] **Step 5: Write failing test for delete writes tombstone**

In `dm_outbox.rs`'s test module:

```rust
#[tokio::test]
async fn delete_dm_outbox_entry_writes_tombstone() {
    // Build OwnerState with an entry; call delete_dm_outbox_entry;
    // assert outbox empty AND outbox_tombstones contains the id
    // with HLC >= the entry's created_at.
}
```

- [ ] **Step 6: Run, expect FAIL**

- [ ] **Step 7: Update `delete_dm_outbox_entry` to write tombstone**

In `dm_outbox.rs:643`'s `delete_dm_outbox_entry`, after the existing `state.outbox.remove(...)`:

```rust
// ZEB-243: write a tombstone so paired devices' sync doesn't
// resurrect the entry. HLC sourced via the same tracker the
// entry's `created_at` was minted from — guarantees the tombstone
// HLC is strictly later than the deleted entry's HLC on this
// device.
let tombstone_hlc = /* next HLC from the tracker — implementer
    locates the tracker access pattern in this function's scope */;
state.outbox_tombstones.insert(entry_id, tombstone_hlc);
```

If `delete_dm_outbox_entry` doesn't currently have HLC tracker access in scope, thread it through the function signature (or accept a `wall_now_ms` and construct an HLC from it + the device's tracker). Match the pattern `send_dm` uses for minting HLCs (`hlc_tracker.get(&device_id) → next`).

- [ ] **Step 8: Run delete test, expect PASS**

- [ ] **Step 9: Verify any callers of `delete_dm_outbox_entry` still compile**

Search `delete_dm_outbox_entry` callers (probably an IPC handler `delete_dm_outbox` in `lib.rs`). Update call site signature if the function gained a parameter.

- [ ] **Step 10: Full gate set green**

- [ ] **Step 11: Commit**

```bash
git add -u
git commit -m "feat(zeb-243): apply_outbox honors tombstones; delete writes one

apply_outbox: strict-greater-than tombstone-HLC vs entry.created_
at comparison; rejects with RejectionReason::Tombstoned. Equal-
HLC theoretically impossible (tombstone written after entry on
same device); falls through if it occurs.

delete_dm_outbox_entry: writes tombstone alongside the local
removal, HLC sourced from the same tracker as entry creation so
the tombstone is guaranteed strictly later than the entry.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 6 — merge_remote_into_local: tombstone-first ordering

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs` (`merge_remote_into_local` around line 538)

- [ ] **Step 1: Write failing convergence test**

```rust
#[tokio::test]
async fn merge_remote_tombstones_sweep_local_outbox() {
    let mut local = OwnerState::default();
    let id = OutboxEntryId([0x22; 16]);
    let entry_hlc = Hlc { wall_ms: 1_000, logical: 0, device_id: "a".into() };
    let tomb_hlc = Hlc { wall_ms: 2_000, logical: 0, device_id: "b".into() };
    local.outbox.insert(id, /* OutboxEntry with created_at=entry_hlc */);

    let mut remote = OwnerState::default();
    remote.outbox_tombstones.insert(id, tomb_hlc.clone());

    merge_remote_into_local(&mut local, remote);

    assert!(!local.outbox.contains_key(&id), "local entry must be swept");
    assert_eq!(local.outbox_tombstones.get(&id), Some(&tomb_hlc));
}
```

Also: a full convergence test where A creates, syncs to B, B deletes, A merges B's state, both empty.

- [ ] **Step 2: Run, expect FAIL**

- [ ] **Step 3: Update `merge_remote_into_local` ordering**

In `owner_state_sync.rs:538`, before the existing outbox-merge loop:

```rust
// ZEB-243: apply remote tombstones FIRST. LWW per id by HLC;
// sweep matching local outbox entries whose created_at is
// strictly older than the merged tombstone. Must precede the
// outbox merge loop below — without this ordering, a remote
// entry that's about to be tombstoned could re-insert via
// apply_outbox before the tombstone arrives.
for (id, remote_hlc) in &remote.outbox_tombstones {
    let merged_hlc = match local.outbox_tombstones.get(id) {
        Some(existing) if existing >= remote_hlc => existing.clone(),
        _ => {
            local.outbox_tombstones.insert(*id, remote_hlc.clone());
            remote_hlc.clone()
        }
    };
    // Sweep local outbox if entry HLC < tombstone HLC.
    if local.outbox.get(id).is_some_and(|e| e.created_at < merged_hlc) {
        local.outbox.remove(id);
    }
}

// Existing outbox merge loop (now respects merged tombstones via apply_outbox).
for (_, entry) in outbox {
    local.apply_outbox(entry);
}
```

Update the merge-ordering doc comment in the file ("spaces first → outbox → inbox → markers → tombstones") to insert outbox_tombstones BEFORE outbox.

- [ ] **Step 4: Run tests, expect PASS**

- [ ] **Step 5: Full gate set green**

- [ ] **Step 6: Commit**

```bash
git add -u
git commit -m "feat(zeb-243): merge_remote_into_local applies tombstones before outbox

Tombstone-first ordering: per-id LWW merge into local
outbox_tombstones, then sweep matching local outbox entries whose
created_at HLC is strictly older than the merged tombstone HLC.
Subsequent outbox merge loop honors the tombstones via the
apply_outbox gate from the prior commit.

Convergence test: A creates, B deletes, sync — both states end
with empty outbox + tombstone for the deleted id.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 7 — Final verification + push + PR

**Files:** N/A (verification + git operations only).

- [ ] **Step 1: Run all local gates one final time**

From `src-tauri/`:
```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo check --locked --all-targets --features test-fixtures
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
From repo root:
```bash
npx tsc --noEmit
npx vitest run
```

Compare nextest test count vs Task 0 baseline; expected delta = number of new tests added (≈6 from Tasks 2/3/4/5/6).

- [ ] **Step 2: Push branch**

```bash
git push -u origin zeb-234-zeb-243-dm-correctness
```

- [ ] **Step 3: Create PR**

```bash
gh pr create \
  --title "ZEB-234 + ZEB-243: shutdown fence + convergent outbox delete" \
  --body "$(cat <<'EOF'
## Summary

- [ZEB-234](https://linear.app/zeblith/issue/ZEB-234): semaphore-based shutdown fence between `send_dm` and `stop_inner`. Closes the microsecond race that could persist+broadcast an entry while `send_dm` returns `Err` (duplicate-DM bug surfaced by CodeRabbit on PR #79 round 4).
- [ZEB-243](https://linear.app/zeblith/issue/ZEB-243): convergent OutboxEntry delete via tombstones. `OwnerState` gains `outbox_tombstones: BTreeMap<OutboxEntryId, Hlc>`; `delete_dm_outbox_entry` writes one alongside the local removal; `apply_outbox` rejects entries older than their matching tombstone; `merge_remote_into_local` applies tombstones first (LWW + sweep) then outbox. Fixes the paired-device resurrection bug surfaced by Qodo on PR #81.

No frontend changes — both are server-side correctness fixes.

## Design + plan

- Spec: [`docs/specs/2026-05-12-zeb-234-zeb-243-dm-correctness-design.md`](docs/specs/2026-05-12-zeb-234-zeb-243-dm-correctness-design.md)
- Plan: [`docs/plans/2026-05-12-zeb-234-zeb-243-dm-correctness-plan.md`](docs/plans/2026-05-12-zeb-234-zeb-243-dm-correctness-plan.md)

## Test plan

- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [x] `cargo check --locked --all-targets --features test-fixtures`
- [x] `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- [x] `npx tsc --noEmit`
- [x] `npx vitest run`

New tests (~6): fence early-reject, fence drain blocks shutdown, tombstone persistence roundtrip, apply_outbox rejection + acceptance, delete writes tombstone, merge sweeps local on remote tombstone.

Closes [ZEB-234](https://linear.app/zeblith/issue/ZEB-234) and [ZEB-243](https://linear.app/zeblith/issue/ZEB-243).
EOF
)"
```

- [ ] **Step 4: Capture the PR URL**

The PR URL output by `gh pr create` will be referenced in the autonomous monitoring loop. Return it to the controlling agent.

---

## Notes for implementer subagents

- **CI is disabled** (`feedback_ci_disabled`): all gates are local. Pretend CI is green. Bots (CodeRabbit, Cursor Bugbot, CodeAnt-AI, Qodo) still run; their feedback comes on the PR after push.
- **Cargo paths**: cargo commands run from `src-tauri/`. tsc/vitest from repo root.
- **No worktrees** per user memory rule — use `git checkout` in the main repo.
- **Test placement**: prefer co-located `#[cfg(test)]` modules if existing patterns in the file already use them; otherwise use a dedicated `src-tauri/tests/` integration test file. Don't fight the existing conventions.
- **HLC source for delete tombstone**: if `delete_dm_outbox_entry` doesn't already have HLC tracker access, thread it via parameter — match `send_dm`'s pattern at `lib.rs:2989`.
- **The `_fence_permit` MUST outlive all the existing locks in `send_dm`**. Declared at the top of the function body after the snapshot block; dropped at the end. All early-Err paths (`?`) drop it correctly via Drop.
- **Don't add unrelated test fixes or refactors**. If you discover a pre-existing issue, file a Linear follow-up rather than folding it in (per `feedback_unrelated_test_failures`).
