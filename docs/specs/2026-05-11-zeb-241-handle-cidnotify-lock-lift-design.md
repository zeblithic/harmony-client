# ZEB-241: Lift CAS fetch + ack fan-out outside OwnerState/DmOutbox locks in `handle_cidnotify`

**Status:** approved 2026-05-11
**Parent:** ZEB-216 (Sub-B DM transport)
**Linear:** [ZEB-241](https://linear.app/zeblith/issue/ZEB-241)

---

## 1. Problem

`event_loop::handle_runtime_action_or_dispatch`'s `RuntimeAction::UnicastReceived` branch holds both `DmOutbox` and `OwnerState` mutex guards across the entire `handle_unicast` call. For CidNotify packets, that call descends into `handle_cidnotify` which does:
1. **Step 9 (slow):** `cas.get(&signed.message_cid)` under a 500ms `tokio::time::timeout` — production path goes through Zenoh DAG-sync.
2. **Step 13b (already-fixed cheap):** `unicast_send_tx.try_send` for ack fan-out — non-blocking since PR #80 fix-up `6af76e2`.

The 500ms CAS-fetch hold remains. Under load (overlapping inbound DMs + drain ticks + `send_dm` IPC), this triggers the `try_lock`-fail-then-requeue path more often than necessary, increasing inbound DM latency and pressuring the bounded retry buffer.

## 2. Architecture

**Three-phase lift via spawned task.** event_loop pre-decodes the inbound packet (cheap CBOR decode); for CidNotify variants it spawns a fire-and-forget task that does the full lift. Other DM kinds (Invite, Ack) continue through the existing synchronous `handle_unicast` path with locks held — they have no slow operations.

```
event_loop (UnicastReceived):
  decode_packet(bytes)?
  match packet:
    CidNotify { signed, sig, signed_bytes } =>
      tokio::spawn(handle_cidnotify_lifted(
          outbox_arc.clone(), state_arc.clone(),
          cas.clone(), tx.clone(), app.clone(),
          signed, sig, signed_bytes, wall_now_ms))
    Invite | Ack =>
      try_lock outbox + state (existing behavior)
      handle_unicast_locked(&mut outbox, &mut state, cas, tx, packet, ...)

handle_cidnotify_lifted (spawned task):
  Phase A — locked, fast (~µs):
    let outbox = outbox_arc.lock().await;
    let mut state = state_arc.lock().await;
    verify signature + resolve owner + check sender match (steps 7a-7c)
    let space = state.spaces.get(&signed.space_id).cloned()  ← Phase A snapshot
    membership gate (resolved_owner ∈ space.members)
    capture (space, identity_pub, resolved_owner) into local
    drop(state); drop(outbox);
  Phase B — unlocked, slow (≤500ms):
    let blob = tokio::time::timeout(500ms, cas.get(message_cid)).await??
  Phase C — re-locked, fast (~ms):
    let outbox = outbox_arc.lock().await;
    let mut state = state_arc.lock().await;
    re-fetch Space (TOCTOU window: could have rotated content_key, lost membership, or been deleted)
    decrypt using current Space + prior_content_keys fallback
    verify sender binding
    apply_owner_device_update (cache refresh — step 8)
    apply_inbox → DrainOutcome
    build ack + try_send fan-out
    drop(state); drop(outbox);
  IPC emit:
    if newly_received: app.emit("dm-received", ...) for each
```

Mirrors the per-engine spawned-task pattern already in `community_state_sync` (each `CommunitySyncEngine` runs its own task with bounded channels).

## 3. Lock dance details

**Phase A uses `.lock().await`, NOT `try_lock`.** Inside the spawned task, `.await` is fine — it doesn't block the event_loop (which is now back in its `select!`). If `send_dm` IPC or another spawned cidnotify task holds the lock, this task waits, but other event_loop work proceeds. Net throughput improves.

**Phase C uses `.lock().await` for the same reason.** Inbound DM throughput is bounded by lock-acquisition latency, not by event_loop responsiveness. If Phase C waits 50ms for `send_dm` to finish, that's strictly better than the status quo where the entire event_loop blocks for 500ms during the CAS fetch.

**Existing `try_lock` branch in event_loop becomes Invite/Ack-only.** The retry buffer + drop semantics on contention stay unchanged for those packet kinds. The decode-then-dispatch pattern is the only structural change to event_loop.

## 4. TOCTOU handling

Three things can change between Phase A's snapshot and Phase C's re-acquisition:

1. **Space.content_key rotation.** Decrypt in Phase C uses the CURRENT Space's `content_key` PLUS its `prior_content_keys` list — the existing fallback (per `dm_crypto::decrypt_dm_message`) handles this. No new logic needed. Spec §6 verifies this with a regression test.
2. **Space.members shrinks (GroupDm member removed).** For DM kinds (`SpaceKind::Dm`), members is fixed at create time — no shrinkage possible per `apply_space_with_canonicalization` invariants. For `SpaceKind::GroupDm`, members CAN shrink via Sub-C v1 kick. Phase C re-checks `space.members.contains(&resolved_owner)` and returns `SenderNotInSpaceMembers` if the sender lost membership in the TOCTOU window. This matches the existing membership gate's semantics.
3. **Space deleted (we left the room between phases).** Phase C re-fetches Space; if absent, returns `SpaceNotFound`. Acceptable: we received a notify for a Space we no longer participate in; nothing to do.

The Phase A snapshot is used ONLY for the early sanity gates (saving a slow path on cleanly-rejected packets). Phase C's authoritative read is what gates the decrypt + apply.

## 5. Out of scope

1. **Invite/Ack lift.** Neither has a slow operation; staying inside the synchronous handle_unicast path is correct. Spec §2 leaves them untouched.
2. **Refactoring `handle_unicast`'s signature** to accept Arc handles uniformly. Would force every caller to change. Keeping the existing &mut path for non-CidNotify minimizes blast radius.
3. **Bounded queue for spawned cidnotify tasks.** Tokio task spawn is cheap (~µs) and the bounded `unicast_send_tx` channel + Reticulum CidNotify retransmit semantics already provide back-pressure if the system is overloaded. Adding a per-cidnotify task queue would be premature optimization.
4. **Lifting handle_invite's potential CAS fetch** if any. Per current code, handle_invite does no CAS fetch — out of scope.
5. **Improving the CAS fetch's 500ms timeout.** Whether 500ms is the right value is orthogonal — same value, different concurrency posture.

## 6. Tests

### Existing tests must continue to pass
All `handle_cidnotify_*` tests in `dm_outbox.rs::tests` (decrypt fallback, sender-binding, atomic-emit semantics, ack fan-out, missing-Space drops). The lift preserves observable behavior.

### New regression test: content_key rotation between Phase A and Phase C

```rust
#[tokio::test]
async fn handle_cidnotify_lifted_decrypts_via_prior_keys_when_content_key_rotates_during_lift() {
    // 1. Set up DM Space S with content_key K1, prior_content_keys=[].
    // 2. Sender encrypts a message with K1; produces signed CidNotify + storage_blob.
    // 3. Receiver:
    //    a. Phase A snapshots Space with content_key K1 (we don't observe this directly, but
    //       it's the captured state at lock-drop time).
    //    b. Between Phase A and Phase C, the test rotates Space's content_key to K2 with
    //       prior_content_keys=[K1] (simulating a Sub-C v1 key rotation event landing during
    //       the CAS fetch window).
    //    c. Phase C decrypts using K2 + prior_content_keys=[K1]; the K1-encrypted blob
    //       MUST decrypt successfully via the prior-keys fallback.
    // 4. Assert: drain_outcome.newly_received contains the message; no DecryptFailed error.
}
```

### New regression test: Space deleted between Phase A and Phase C

```rust
#[tokio::test]
async fn handle_cidnotify_lifted_returns_space_not_found_when_space_deleted_during_lift() {
    // 1. Set up DM Space S with valid Phase A snapshot.
    // 2. Sender produces signed CidNotify + blob.
    // 3. Receiver Phase A passes (Space exists).
    // 4. During Phase B (test injects), Space S is removed from state.
    // 5. Phase C re-checks; returns SpaceNotFound; no apply_inbox; no ack fan-out.
}
```

### New regression test: GroupDm member kicked between Phase A and Phase C

```rust
#[tokio::test]
async fn handle_cidnotify_lifted_returns_sender_not_in_members_when_kicked_during_lift() {
    // Similar shape: GroupDm Space, sender ∈ members at Phase A, removed at Phase C.
    // Returns SenderNotInSpaceMembers; no apply_inbox; no ack fan-out.
}
```

### Concurrency smoke test (optional, may defer)

```rust
#[tokio::test]
async fn handle_cidnotify_lifted_concurrent_inbound_dms_dont_serialize_on_locks() {
    // Spawn 5 concurrent handle_cidnotify_lifted tasks on a slow CAS stub
    // (200ms delay each). Total wall-clock should be ~250-300ms (1× CAS round
    // + lock-acquisition serialization for Phase A/C), NOT ~1000ms (5× serial).
    // Acceptable threshold: total < 500ms (well below 1000ms but with slack).
}
```

## 7. Implementation surface

**New files:** none.

**Modified files:**

| File | Change |
|---|---|
| `src-tauri/src/dm_outbox.rs` | Add new `handle_cidnotify_lifted` async fn taking `Arc<Mutex<DmOutbox>>` + `Arc<Mutex<OwnerState>>` + `Arc<dyn ContentStore>` + `Sender<UnicastSendRequest>` + `AppHandle<R>`. Internally manages Phase A → B → C. The existing `handle_cidnotify(&mut self, &mut state, ...)` remains for unit-test ergonomics. |
| `src-tauri/src/event_loop.rs` | In `handle_runtime_action_or_dispatch`'s `UnicastReceived` block: pre-decode packet via `dm_envelope::decode_packet`. If `DmPacket::CidNotify`, spawn a task calling `handle_cidnotify_lifted`. Otherwise fall through to existing try_lock + handle_unicast path (now Invite/Ack-only). |

The existing `handle_cidnotify(&mut self, &mut state, ...)` path is kept as a thin internal helper so existing unit tests don't have to be rewritten — `handle_cidnotify_lifted` calls it for Phase A's verify+snapshot work and Phase C's apply work, with explicit lock acquisitions around each call site.

Actually — simpler: since `handle_cidnotify_lifted` needs custom Phase A (snapshot only, don't apply) and Phase C (apply with re-fetched Space), it implements both phases inline rather than re-entering the existing handle_cidnotify monolith. This avoids partial-execution complexity. The existing `handle_cidnotify` is then either:
(a) Removed (since `handle_unicast`'s CidNotify dispatch is also gone — event_loop short-circuits CidNotify before handle_unicast is called), OR
(b) Retained as a "synchronous-test-only" helper for the existing unit tests.

**Recommendation:** option (a) — remove. Migrate the existing unit tests to call `handle_cidnotify_lifted` with stub Arcs. The test surface stays the same; the production path is single-source-of-truth.

## 8. Acceptance criteria

1. `handle_cidnotify_lifted` exists in `dm_outbox.rs` with the Phase A → B → C structure described in §2.
2. `event_loop.rs::handle_runtime_action_or_dispatch` pre-decodes CidNotify packets and spawns the lifted task; Invite/Ack continue through the existing try_lock + handle_unicast path.
3. The OLD `handle_cidnotify(&mut self, &mut state, ...)` is REMOVED. handle_unicast's match on CidNotify is removed (event_loop short-circuits it).
4. All 5 CI gates green: `cargo fmt --check`, `cargo clippy --features test-fixtures -D warnings`, `cargo nextest run --features test-fixtures`, `cargo check --features test-fixtures` (MSRV), `npx tsc --noEmit`, `npx vitest run`.
5. New regression tests added (per §6): content_key rotation between phases, Space-deleted between phases, GroupDm member kicked between phases. All pass.
6. Existing `handle_cidnotify_*` unit tests in `dm_outbox.rs::tests` pass after migration to `handle_cidnotify_lifted` (test signature changes only — observable behavior unchanged for non-TOCTOU paths).
7. Concurrency smoke test (per §6) passes if included.

## 9. Notable design decisions (do NOT relitigate)

1. **Spawn-per-cidnotify, not in-line lift.** Reason: in-line `.await` on locks would re-block the event_loop, defeating the lift's purpose.
2. **Phase A snapshot is advisory; Phase C is authoritative.** Saves a slow CAS fetch on cleanly-rejectable packets but does not commit to Phase A's view of state.
3. **Existing `prior_content_keys` fallback handles content_key rotation TOCTOU.** No new decrypt logic needed.
4. **Removing the old monolithic `handle_cidnotify`.** Single-source-of-truth for the production path; test migration is mechanical (swap &mut for Arc).
5. **Invite/Ack stay synchronous.** No slow operations; lift would be over-rotation.
6. **Phase C uses `.lock().await`, not try_lock + retry.** Inside a spawned task this is correct — does not block event_loop.

## 10. Known limitations

1. **Spawned-task panic recovery.** If `handle_cidnotify_lifted` panics, the task dies silently (`tokio::spawn` swallows panics by default unless joined). Wrap the task body in a `panic::catch_unwind`-equivalent or at minimum a top-level `tracing::error!` inside the task to surface failures. Aligns with the existing panic-handling pattern in `community_state_sync`.
2. **Multiple concurrent inbound CidNotify tasks** can interleave Phase C work. This is fine — `apply_inbox` is composite-keyed (space_id + message_cid + from), so duplicate CIDs collapse via `Merged` semantics, and order within distinct CIDs is HLC-driven not arrival-order-driven.
3. **No bounded queue for spawned tasks.** Under pathological load (e.g., adversary sending thousands of CidNotify per second), unbounded task spawning could exhaust memory. Mitigated by: (a) Reticulum's transport-layer rate-limiting upstream; (b) the bounded `unicast_send_tx` channel back-pressuring ack fan-out; (c) per-task lifetime is bounded by 500ms CAS timeout. If this becomes a problem, follow up with a `tokio::sync::Semaphore` to cap concurrent in-flight cidnotify tasks. Not in scope for this PR.

## 11. Verification

Local pre-push:
- `cd src-tauri && cargo fmt --all -- --check` — 0
- `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` — 0
- `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` — all pass (existing + 3 new TOCTOU tests + optional concurrency smoke)
- `cd src-tauri && cargo check --locked --all-targets --features test-fixtures` (MSRV) — 0
- `npx tsc --noEmit` (from repo root) — 0
- `npx vitest run` (from repo root) — all pass

CI: 5 gates green on PR.
