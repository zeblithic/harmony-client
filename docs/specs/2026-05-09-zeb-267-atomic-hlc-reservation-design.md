# ZEB-267: Atomic HLC Reservation for Device-Monotone Event Minting

**Status:** approved 2026-05-09
**Predecessor:** ZEB-266 (Sub-C v2 Phase 1) PR #93 — surfaced the bug and deferred the fix
**Parent:** ZEB-217 (Sub-C v1) — established the device-HLC tracker pattern
**Branch:** `zeb-267-atomic-hlc-reservation` (cut from `origin/main` `b67468f`)

## 1. Problem

Eight power-gated community-membership IPCs in `src-tauri/src/lib.rs` mint signed events using a snapshot-then-release HLC pattern. (One of those IPCs — `create_community_inner` — mints two events in sequence, so the refactor touches nine reservation sites total.)

```rust
let prev_hlc = {
    let t = hlc_tracker.lock().await;
    t.get(&device_id).cloned()
};                                      // (a) tracker LOCK RELEASED
// ... wall_now_ms read, signing-key fetch, generation fence ...
let event = mint_*(..., wall_now_ms, &device_id, prev_hlc.as_ref())?;
let outcome = engine_arc.insert_local_event(event.clone()).await?;
if matches!(outcome, InsertOutcome::Inserted) {
    let mut t = hlc_tracker.lock().await;
    t.insert(device_id.clone(), event.at.clone());  // (b) tracker advanced
}
```

Between (a) and (b), a concurrent IPC from the same device can read the same `prev_hlc`, mint with the same `(wall_ms, logical, device_id)` HLC tuple, and produce two events that violate the per-device monotone-HLC invariant the receive-side `event_sort_key` ordering depends on. The duplicate is not rejected by `insert_event` (dedupe is by `event_id`, not HLC), so the divergence is silent.

`send_dm` already does the right thing — it holds the tracker lock through mint+update at `lib.rs:2188-2218`. Receive-side `next_hlc` calls in `owner_state_sync.rs:428` and `community_state_sync.rs:1291` already run under engine locks and are race-free. The bug is exclusive to the membership-IPC layer.

## 2. Goal

Replace the snapshot-then-release pattern with a single atomic reservation primitive used at every membership-event IPC site. After the refactor, the per-device HLC tracker is the canonical source of monotone HLCs, advanced under one lock per reservation, regardless of downstream insert outcome.

## 3. Architecture

### 3.1 Reservation primitive

New free function in `src-tauri/src/dm_outbox.rs` next to the existing `next_hlc` (consistent with the comment at line 1521 about future shared-module promotion — keeping both helpers co-located honors that deferral until a real shared-module justification arrives):

```rust
/// Atomically reserve the next HLC for a device.
///
/// Acquires `tracker`, reads the device's last-known HLC, computes
/// the successor via `next_hlc`, writes it back, and returns it —
/// all under a single lock acquisition. Replaces the
/// snapshot-then-release pattern at all power-gated community-event
/// IPCs (kick / leave / set_power / channel_* / redeem /
/// create_community).
///
/// Tracker is bumped at reservation time, regardless of whether the
/// caller's downstream `engine.insert_local_event` succeeds. A
/// rejected insert "burns" the reserved HLC — fine, since HLCs are
/// 64-bit logical and burning is already implicit on signature- or
/// verify-failure paths today.
pub async fn reserve_next_hlc_for_device(
    tracker: &std::sync::Arc<
        tokio::sync::Mutex<std::collections::BTreeMap<String, Hlc>>,
    >,
    device_id: &str,
    wall_now_ms: u64,
) -> Hlc {
    let mut t = tracker.lock().await;
    let prev = t.get(device_id).cloned();
    let next = next_hlc(prev.as_ref(), wall_now_ms, device_id);
    t.insert(device_id.to_string(), next.clone());
    next
}
```

**Lock discipline:** the helper takes only the tracker `tokio::sync::Mutex`, briefly. The pre-existing rule at `lib.rs:6516-6524` (tracker lock BEFORE `crdt_state` lock; never `await` while holding the `NodeState` `std::Mutex`) is preserved — IPCs continue to reserve before grabbing `crdt_state` / `outbox_g` / `engine_arc`.

**Concurrency guarantee:** N concurrent reservations on the same `tracker` produce N strictly-monotone, distinct HLCs. The `tokio::sync::Mutex` serializes the read-compute-write under one critical section; `next_hlc`'s wall-regression handling guarantees monotonicity even under clock skew.

**Burn semantics:** if `engine.insert_local_event` rejects the reserved HLC's event, the tracker is *not* rolled back. Future reservations from the same device skip the burned HLC and continue from the just-bumped tracker value. HLCs are 64-bit logical and not a finite resource. This already implicitly happens in production today on any pre-insert failure (signature error, generation fence, registry detach) — the spec just makes it the universal contract.

### 3.2 Mint helper signature simplification

All eight `mint_*` membership-event helpers in `src-tauri/src/lib.rs` have their `(wall_now_ms: u64, device_id: &str, prev_hlc: Option<&Hlc>)` parameter trio replaced with a single `hlc: Hlc`:

| Helper                            | Line       |
| --------------------------------- | ---------- |
| `mint_channel_create_event`       | 5531       |
| `mint_channel_modify_event`       | 5742       |
| `mint_channel_delete_event`       | 5780       |
| `mint_community_creation`         | 6434       |
| `mint_redemption`                 | 7187       |
| `mint_leave_event`                | 8235       |
| `mint_kick_event`                 | 8514       |
| `mint_set_power_event`            | 8676       |

Each helper drops its internal `next_hlc(prev_hlc, wall_now_ms, device_id)` call and uses the supplied `hlc` directly in the event payload. The mint helpers become pure on the HLC — caller is responsible for reserving it. Their unit tests update to pass an explicit `Hlc` constructed inline.

The `create_dm_inner` path at line 2902 (DM Space creation) is left in place: it inline-mints `creation_hlc` for both `created_at` and `updated_at` of a fresh DM Space, but `send_dm`'s caller already holds the tracker lock through mint+update — so the DM-Space-creation site is race-free for the same reason `send_dm` is. Out of scope.

### 3.3 IPC call-site refactor

Each of the nine reservation sites (across eight IPCs) replaces its snapshot-then-release block with a reservation, passes the reserved `Hlc` to the (newly-simplified) mint helper, and deletes the post-Inserted tracker advance:

```rust
// BEFORE
let prev_hlc = { let t = hlc_tracker.lock().await; t.get(&device_id).cloned() };
let event = mint_kick_event(..., wall_now_ms, &device_id, prev_hlc.as_ref())?;
// ... engine.insert_local_event ...
if matches!(outcome, InsertOutcome::Inserted) {
    let mut t = hlc_tracker.lock().await;
    t.insert(device_id.clone(), event.at.clone());
}

// AFTER
let hlc = reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;
let event = mint_kick_event(..., hlc)?;
// ... engine.insert_local_event ...
// (no post-block — tracker already advanced atomically at reservation)
```

Affected sites in `src-tauri/src/lib.rs`:

| Site                        | Line   | Notes                                               |
| --------------------------- | ------ | --------------------------------------------------- |
| `redeem_invite_inner`       | 7203   | Reserves once for the self-Join.                    |
| `leave_community`           | 8250   | One-event mint.                                     |
| `kick_from_community`       | 8531   | One-event mint.                                     |
| `set_power_level`           | 8693   | One-event mint.                                     |
| `create_channel`            | 5549   | One-event mint.                                     |
| `modify_channel`            | 5760   | One-event mint.                                     |
| `delete_channel`            | 5796   | One-event mint.                                     |
| `create_community_inner`    | 6456   | Reserves for `bootstrap_join`.                      |
| `create_community_inner`    | 6713   | Reserves a second time for default `#general` channel. The chained `next_hlc(Some(&bootstrap_join.at), ...)` call disappears. |

The `create_community_inner` two-reservation case works without a special `reserve_n` API because the second reservation reads the tracker — which the first reservation just bumped to `bootstrap_join.at` — and returns a strictly-greater HLC.

## 4. Out of scope

* `send_dm` and `DmOutbox::send_dm` (already race-free under tracker lock).
* `dm_outbox::apply_outbox` (called from `send_dm` under that same lock).
* Receive-side `next_hlc` callers in `owner_state_sync.rs:428` / `community_state_sync.rs:1291` (already serialized by their engine locks).
* Promoting `next_hlc` to a shared module — duplication between `dm_outbox.rs:1523`, `owner_state_sync.rs:452`, `community_state_sync.rs:1340` is parallel tech debt, deferred per the existing comment at `dm_outbox.rs:1521`.
* Wrapping the tracker in a `HlcTracker` newtype — separate refactor with its own justification.
* Receive-side handling of HLC ordering — covered by existing `event_sort_key` machinery, untouched here.

## 5. Testing strategy

### 5.1 Unit tests (`src-tauri/src/dm_outbox.rs::tests`)

* `reserve_next_hlc_for_device_advances_tracker_atomically` — sequential; reserve twice, assert second > first by `event_sort_key`, assert tracker holds the second.
* `reserve_next_hlc_for_device_concurrent_reservations_distinct` — spawn 64 concurrent reservations on a shared tracker via `tokio::task::JoinSet`; collect HLCs into a `BTreeSet`; assert size is 64; assert tracker's final value equals the max.
* `reserve_next_hlc_for_device_handles_wall_regression` — pre-seed tracker with `Hlc { wall_ms: 1000, logical: 5, device_id: "dev-A" }`, reserve with `wall_now_ms = 500`; assert returned HLC is `(1000, 6, "dev-A")` and tracker holds it.

### 5.2 Mint-helper unit tests

The eight existing `mint_*_produces_*` unit tests in `lib.rs` (e.g., `mint_creation_produces_consistent_id_join_event_and_space` at line 7035, `mint_redemption_produces_self_join_and_matching_space` at 8156, `mint_leave_produces_self_leave_event` at 8458) are updated to construct an explicit `Hlc` inline and pass it to the helper. Assertions remain unchanged — these tests verify mint correctness, not HLC reservation.

### 5.3 Integration test — concurrent-IPC race

New file `src-tauri/tests/community_hlc_race_integration.rs`:

* Stand up a single community on one engine, with one device acting as admin (power 100).
* Execute two `kick_from_community` futures concurrently via `tokio::join!` — both kicking *different* targets so neither rejects on `KickTargetPowerNotLower`.
* Assert both `Inserted`.
* Read both kick events from the engine's materialized state.
* Assert their `at` HLCs are distinct under `event_sort_key`.

This is the test that would have caught the original bug. It runs in milliseconds — no transport, no encryption, just two parallel mint+insert paths against one engine.

### 5.4 Existing integration tests

`community_invite_only_integration`, `community_channel_config_integration`, and `community_membership_unit` exercise the single-IPC-at-a-time path, which is behaviorally identical post-refactor. They pass unchanged. Their continued green is a regression gate against the refactor breaking the happy path.

## 6. Phasing & commit shape

Six commits on branch `zeb-267-atomic-hlc-reservation`, single PR:

1. **Task 0 — pre-flight + green baseline.** `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`. Confirm green from clean main; record baseline. No commit on a green pre-flight.
2. **Task 1 — `reserve_next_hlc_for_device` helper.** Add the helper to `dm_outbox.rs`. Add the three unit tests from §5.1. Helper unused by the rest of the codebase at this point — Task 1 commits and ships green in isolation.
3. **Task 2 — mint helper signature change.** Replace `(wall_now_ms, device_id, prev_hlc)` with `(hlc: Hlc)` across all eight `mint_*` helpers. Update each helper's local unit test to construct an `Hlc` and pass it. **The IPC sites do NOT compile after this commit** — that is intentional and fixed in Task 3.
4. **Task 3 — IPC call-site refactor.** At each of the nine reservation sites (across eight IPCs): replace the snapshot-then-release block with a `reserve_next_hlc_for_device` call; pass the returned `Hlc` to the mint helper; delete the `if matches!(outcome, InsertOutcome::Inserted) { tracker.insert(...) }` post-block. Also patch `create_community_inner` line 6713 to use the helper. After this commit, the workspace compiles and all existing tests pass green.
5. **Task 4 — concurrent-IPC integration test.** Add `community_hlc_race_integration.rs` per §5.3. As a TDD verification step (not a separate commit), the implementer may briefly stash the Task 3 changes to confirm the new test would fail against the old snapshot-then-release pattern, then restore Task 3 and confirm green — this is a sanity check that the test actually exercises the bug, not a required deliverable.
6. **Task 5 — final verification + push + PR.** Re-run all gates green. Push branch. Open PR with body cross-referencing ZEB-267 + ZEB-266 + ZEB-217 + the deferred CodeRabbit thread on PR #93.

Branch stays on `origin/main` lineage throughout (per the pull-before-work HARD RULE — already satisfied by the just-completed PR #93 merge fast-forward to `b67468f`).

## 7. Acceptance criteria (mirrors ZEB-267 ticket §"Acceptance criteria")

1. `reserve_next_hlc_for_device` exists in `dm_outbox.rs` with the signature in §3.1 and the three unit tests in §5.1 green.
2. All nine reservation sites enumerated in §3.3 have been refactored to call the helper. The post-Inserted tracker-advance block is deleted at every site.
3. The concurrent-IPC integration test in §5.3 demonstrates the bug is fixed.
4. `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --workspace --no-fail-fast` all green.
5. Existing `community_invite_only_integration` and `community_channel_config_integration` tests pass unchanged (single-IPC behavior preserved).
6. PR body cross-references ZEB-267 + ZEB-266 + parent ZEB-217 + the CodeRabbit thread on PR #93 that captured the deferred fix.

## 8. Risk register

* **Two-event `create_community_inner` ordering.** The old code chained `default_channel_at = next_hlc(Some(&bootstrap_join.at), ...)` to guarantee logical+1 ordering relative to bootstrap_join. The new code reserves twice; the second reservation reads the tracker (which the first reservation set to `bootstrap_join.at`) and returns a strictly-greater HLC. `next_hlc`'s wall-regression handling means the resulting HLC is `(bootstrap.wall_ms, bootstrap.logical+1, device_id)` — bit-identical to the old chained output in the no-clock-skew case, strictly later in the clock-advanced case. The Task 3 commit updates Task 7's atomic-rollback test (currently in `community_channel_config_integration.rs`) only if it inspects the default-channel HLC's exact value; the more likely case is that the test asserts presence + `created_at > bootstrap.at`, in which case it passes unchanged.
* **Test-only `mint_*` callsites.** The eight `mint_*_produces_*` unit tests in `lib.rs` need signature updates. None of the integration tests in `src-tauri/tests/` call the mint helpers directly — they exercise the IPCs end-to-end. So the test-surface change is contained to `lib.rs::tests` (~8 sites).
* **`create_dm_inner` (line 2902) inline mint.** Out of scope per §3.2 because `send_dm` already holds the tracker lock through mint+update. If a future refactor changes that, the `create_dm_inner` path needs the same treatment — flagged in §4 to surface that dependency.

## 9. References

* ZEB-267 Linear ticket — `https://linear.app/zeblith/issue/ZEB-267`
* ZEB-266 (predecessor, Sub-C v2 Phase 1) — surfaced the bug; deferred the fix.
* ZEB-217 (parent, Sub-C v1) — established the device-HLC tracker pattern.
* CodeRabbit thread on PR #93 — captured the cross-cutting nature and accepted the scope deferral.
* `src-tauri/src/dm_outbox.rs:1523` — existing `next_hlc` helper (unchanged).
* `src-tauri/src/lib.rs:2188-2218` — `send_dm` IPC (the existing race-free pattern this refactor brings the membership IPCs into alignment with).
* `src-tauri/src/lib.rs:6516-6524` — pre-existing lock-order discipline comment (preserved by this design).
