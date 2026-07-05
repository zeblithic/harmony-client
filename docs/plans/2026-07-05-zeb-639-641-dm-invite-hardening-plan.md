# ZEB-639/640/641 DM-Invite Hardening Bundle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the kicked-GroupDm re-admit/HLC-pinning vector (ZEB-639), fix the two staging lifecycle edges (ZEB-640), and pay down the remaining staging test debt (ZEB-641) — one PR on branch `zeb-639-641-dm-invite-hardening`.

**Architecture:** All behavior changes concentrate in `dm_outbox.rs::apply_invite`/`run_invite_accept_tail` (one new outcome variant + one HLC clamp), `pending_dm_invites.rs` (one new purge helper), and `lib.rs::accept_dm_invite_impl` (error discrimination). Callers adapt via compiler-driven exhaustive matches. Spec: `docs/specs/2026-07-05-zeb-639-641-dm-invite-hardening-design.md` (commit d741693e) — the contract; read it first.

**Tech Stack:** Rust (tauri backend), vitest (frontend test only — no production frontend changes).

## Global Constraints

- Cargo commands run from `src-tauri/`, ONE cargo invocation at a time, always `--locked`.
- Per-task test scope: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E '<filter>'` (harmony-app relink cost — full `--all-targets` sweep happens once in Task 7).
- Clippy gate (CI form): `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`. Run `cargo fmt --all` before every commit.
- Frontend: `npx tsc --noEmit` + `npx vitest run` from repo root (Task 6 only).
- The #402 golden byte-parity tests (`dm_outbox.rs` accept-parity tests) MUST stay green UNMODIFIED — the clamp is a no-op for their past-dated fixtures. If one goes red, the change is wrong, not the test.
- Never remove/weaken `#[cfg(any(test, feature = "test-fixtures"))]` gates; keychain access only via `*_inner` seams; `HARMONY_PASSPHRASE` in tests touching identity persistence.
- Tauri IPC: Rust `snake_case` params ↔ JS `camelCase`; DTO must never expose `content_key`/`inviter_identity_pub`.
- Commit per task; no worktrees; branch `zeb-639-641-dm-invite-hardening` off main@7121610e.

---

### Task 1: `ApplyInviteOutcome::IgnoredExistingSpace` — space-exists staging gate (ZEB-639.1)

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` (enum near `ApplyInviteOutcome`; gate at ~:2323 in `apply_invite`; dormant arm ~:1826; `apply_deposited_invite` ~:2546; tests in the ZEB-236 tier-fork test cluster ~:5820+)
- Modify: `src-tauri/src/dm_inbox_ingest.rs` (match arms ~:512-519, ~:944-952)
- Modify: `src-tauri/src/community_relay_prod.rs` (match arm ~:451-458)

**Interfaces:**
- Produces: `ApplyInviteOutcome::IgnoredExistingSpace` (unit variant) — Tasks 3/5 must leave its arms untouched.
- Consumes: `state.spaces: BTreeMap<SpaceId, Space>` (`owner_state_crdt.rs:25`).

- [ ] **Step 1: Write the failing test** (in the ZEB-236 tier test cluster in `dm_outbox.rs`, mirroring the existing non-friend staging test's fixture setup):

```rust
#[test]
fn non_friend_invite_for_existing_space_is_ignored_not_staged() {
    // Arrange exactly like the existing "non-friend invite → Staged" test,
    // but FIRST apply a Space with the same space_id to `state`
    // (state.apply_space_with_canonicalization(space) with a minimal valid
    // Dm/GroupDm Space — copy the shape the accept tail builds).
    // Act: apply_invite(...) with a NON-friend inviter for that space_id.
    // Assert: matches!(outcome, ApplyInviteOutcome::IgnoredExistingSpace)
    // and the canonical OwnerState bytes are UNCHANGED by the call.
}

#[test]
fn friend_invite_for_existing_space_still_accepts_redelivery_merge() {
    // Same arrangement with an ACTIVE-friend inviter: outcome must be
    // ApplyInviteOutcome::Accepted (the idempotent redelivery contract is
    // NOT gated).
}
```

- [ ] **Step 2: Run to verify failure**: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(existing_space)'` → FAIL (variant does not exist → compile error is the expected first failure).

- [ ] **Step 3: Implement.** Add the variant with doc comment:

```rust
/// ZEB-639: a structurally-valid NON-FRIEND invite for a space that already
/// exists locally. Never staged: we are already a member, so there is no
/// consent to ask for — and a consent prompt here is exactly the kicked
/// GroupDm co-member re-admit vector (forged fresh invite for the existing
/// space_id). Legit roster changes arrive via Space CRDT sync, not invites.
/// Matches the co-deposit path's semantics (it only stages on SpaceNotFound).
/// Friend-tier invites are NOT gated — idempotent redelivery merge contract.
IgnoredExistingSpace,
```

Gate in `apply_invite` (inside the existing `if !inviter_is_active_friend {` block, BEFORE the `return Ok(ApplyInviteOutcome::Staged(...))`):

```rust
if state.spaces.contains_key(&signed.space_id) {
    return Ok(ApplyInviteOutcome::IgnoredExistingSpace);
}
```

Caller arms (compiler-driven — build until every non-exhaustive match is fixed):
- `dm_inbox_ingest.rs` ~:519 and ~:952, `community_relay_prod.rs` ~:458: add `ApplyInviteOutcome::IgnoredExistingSpace => {}` (or `=> Ok(())`/matching the `Accepted` arm's shape at that site) with a one-line `tracing::debug!` naming the space_id.
- `dm_outbox.rs` ~:1826 dormant: `IgnoredExistingSpace => Ok(DrainOutcome::default())` with a `tracing::debug!` (mirror the Staged arm's logging style).
- `apply_deposited_invite` ~:2546: `IgnoredExistingSpace => Ok(None)` (same as `Accepted` — nothing to stage).

- [ ] **Step 4: Run the tests**: same filter → PASS; also run `-E 'test(tier) or test(staged) or test(invite)'` scoped pass to catch regressions in the #402 tier tests.
- [ ] **Step 5: `cargo fmt --all`, commit** `git commit -m "ZEB-639: gate non-friend invite staging on space non-existence (IgnoredExistingSpace)"`.

### Task 2: `updated_at` local-clock clamp in the accept tail (ZEB-639.2)

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` (`run_invite_accept_tail` ~:2371-2393; tests near the #402 golden-parity cluster)

**Interfaces:**
- Consumes: `Hlc { wall_ms, logical, device_id }` (same construction as `learned_at` at ~:2435).
- Produces: nothing new — behavior-only change inside the tail.

- [ ] **Step 1: Failing tests**:

```rust
#[test]
fn forged_far_future_created_at_is_clamped_on_accept() {
    // Build a non-friend invite whose created_at.wall_ms = u64::MAX / 2,
    // stage-then-accept via run_invite_accept_tail directly (call it the way
    // the existing golden-parity test does, with wall_now_ms = a known value).
    // Assert: state.spaces[&id].updated_at.wall_ms == wall_now_ms
    // and updated_at.device_id == local device_id (NOT the invite's HLC).
    // created_at keeps the claimed value (provenance).
}

#[test]
fn legit_update_wins_lww_after_clamped_accept() {
    // After the clamped accept above, apply a Space update with
    // updated_at.wall_ms = wall_now_ms + 1 via apply_space_with_canonicalization.
    // Assert ApplyOutcome is an accept/merge (NOT Rejected/stale) and the
    // update's field change (e.g. custom_name) is visible.
}
```

- [ ] **Step 2: Verify failure** (`-E 'test(clamp) or test(far_future)'`): first test FAILS — updated_at currently echoes the forged HLC.
- [ ] **Step 3: Implement** in `run_invite_accept_tail`, replacing `updated_at: signed.created_at` in the Space literal:

```rust
// SECURITY (ZEB-639): clamp the Space's LWW driver to a local-clock
// ceiling. `lww_merge_space` is LWW-by-`updated_at` and GroupDm dedupe_key
// is id-derived (members ARE mutable on the same SpaceId), so echoing the
// invite-controlled `created_at` would let one forged far-future HLC pin
// this Space against every future legitimate update — the same
// denial-of-updates attack the cache `learned_at` rule below already
// defeats. Legit invites have past created_at → clamp is a no-op (golden
// parity tests pin this). `created_at` keeps the claimed value: it is
// provenance/display and does not drive LWW.
let updated_at = if signed.created_at.wall_ms > wall_now_ms {
    Hlc {
        wall_ms: wall_now_ms,
        logical: 0,
        device_id: device_id.to_string(),
    }
} else {
    signed.created_at.clone()
};
```

and use `updated_at` in the Space literal (`created_at: signed.created_at.clone()` stays).

- [ ] **Step 4: Run**: clamp tests PASS + `-E 'test(parity) or test(golden)'` (the #402 byte-parity tests) PASS UNMODIFIED.
- [ ] **Step 5: fmt + commit** `"ZEB-639: clamp accept-tail updated_at to local clock (anti HLC-pinning)"`.

### Task 3: purge stale staged entry on friend-tier accept (ZEB-640.1)

**Files:**
- Modify: `src-tauri/src/pending_dm_invites.rs` (new helper below `stage_and_emit_staged_invite`; unit tests in its `mod tests`)
- Modify: `src-tauri/src/dm_inbox_ingest.rs` (`Accepted` arms ~:519, ~:952; co-deposit `Ok(None)` branch in the ~:851-870 region)
- Modify: `src-tauri/src/community_relay_prod.rs` (`Accepted` arm ~:458; co-deposit `Ok(None)` branch in the ~:545-560 region)

**Interfaces:**
- Produces: `pub(crate) fn purge_stale_staged_on_accept(pending: Option<&std::sync::Arc<PendingDmInvites>>, sink: &dyn crate::node_event_sink::NodeEventSink, space_id: &crate::owner_state_types::SpaceId)` — signature mirrors `stage_and_emit_staged_invite` exactly (same `Option<&Arc>` store handle, same caller contract: crdt lock already dropped).

- [ ] **Step 1: Failing unit tests** (in `pending_dm_invites.rs::tests`, using the existing fixtures):

```rust
#[test]
fn purge_on_accept_removes_and_emits_once() {
    // stage() an invite; call purge_stale_staged_on_accept with a recording
    // sink; assert store list() empty and EXACTLY one "dm-invite-list-changed"
    // (and NO "dm-invite-received") on the sink.
}

#[test]
fn purge_on_accept_is_silent_when_nothing_staged() {
    // Empty store → call → assert NO events (hot-path noise guard: every
    // friend-tier DM redelivery hits the Accepted arms).
}
```

- [ ] **Step 2: Verify failure** (`-E 'test(purge_on_accept)'`) → compile error (helper missing).
- [ ] **Step 3: Implement** the helper (doc comment: befriend-then-redeliver leaves a stale pending row for a Space that now exists in nav — ZEB-640; same caller contract paragraph as the stage helper), then wire it at the five live sites: the three `Accepted` match arms and the two co-deposit `Ok(None)` branches (the ones adjacent to where those sites currently call `stage_and_emit_staged_invite` on `Ok(Some(staged))`). The dormant path (`dm_outbox.rs` ~:1826) has NO store — leave untouched.
- [ ] **Step 4: One integration-shape test** at the tunnel site pattern (copy the existing tunnel staging test): stage a non-friend invite → befriend the inviter (set FriendStatus::Active in the fixture's friend_graph) → redeliver the same invite through the same ingest path → assert store empty + one `dm-invite-list-changed` from the purge.
- [ ] **Step 5: Run** `-E 'test(purge) or test(staged)'` → PASS; fmt + commit `"ZEB-640: purge stale staged invite on friend-tier auto-accept"`.

### Task 4: permanent accept failures drop instead of re-stage (ZEB-640.2)

**Files:**
- Modify: `src-tauri/src/lib.rs` (`accept_dm_invite_impl` failure branch, ~:49043-49047)
- Test: same file's ZEB-236 IPC test cluster (near the `list_pending_dm_invites_inner` tests, ~:50560+) or `dm_outbox.rs` if state plumbing is easier there.

**Interfaces:**
- Consumes: `DmReceiveError::CrdtRejected(String)` (`dm_outbox.rs:~2870` cluster) — the tail's ONLY current error channel.

- [ ] **Step 1: Failing test**:

```rust
#[tokio::test]
async fn accept_of_tombstoned_space_drops_pending_row_with_distinct_error() {
    // Build a NodeState harness the way the existing accept_dm_invite_impl
    // tests do (real PendingDmInvites + real crdt_state + recording sink).
    // Stage a non-friend invite; then tombstone that space_id in OwnerState
    // (apply the tombstone the way existing owner_state_crdt tombstone tests
    // do). Call accept_dm_invite_impl.
    // Assert: (a) Err message starts with "invite no longer applicable";
    // (b) the store is EMPTY (no re-stage);
    // (c) exactly one "dm-invite-list-changed" was emitted (both UI surfaces
    //     must drop the dead row);
    // (d) a SECOND accept_dm_invite_impl call returns the standard
    //     "no pending DM invite for space" error.
}
```

- [ ] **Step 2: Verify failure** (`-E 'test(tombstoned_space_drops)'`): today the row is re-staged and the error is the generic `"accept failed: …"`.
- [ ] **Step 3: Implement** — replace the current uniform re-stage branch:

```rust
if let Err(e) = apply_result {
    return match e {
        // ZEB-640: CRDT rejects are deterministic-permanent (pure function
        // of state + input — Tombstoned, invariant violations). Re-staging
        // would wedge the row: every Accept re-errors forever and only
        // Decline clears it. Drop the row and tell both surfaces.
        crate::dm_outbox::DmReceiveError::CrdtRejected(reason) => {
            crate::node_event_sink::emit_ser(sink.as_ref(), "dm-invite-list-changed", &());
            Err(format!("invite no longer applicable: {reason}"))
        }
        // Defensive arm: the tail's only current Err is CrdtRejected, but a
        // future genuinely-transient failure must re-stage — a silently
        // lost accept is indistinguishable from a decline (spec).
        other => {
            store.stage(restage_copy);
            Err(format!("accept failed: {other:?}"))
        }
    };
}
```

(Keep `restage_copy` exactly as-is; it feeds the defensive arm.)

- [ ] **Step 4: Run** `-E 'test(accept_dm_invite) or test(tombstoned)'` → PASS (including the existing transient re-stage test if one exists — if it asserts re-stage on CrdtRejected specifically, UPDATE it to the new contract and say so in the report).
- [ ] **Step 5: fmt + commit** `"ZEB-640: permanent accept failures drop the pending row (no re-stage wedge)"`.

### Task 5: real-store staging tests for the three uncovered ingest sites (ZEB-641.1)

**Files:**
- Test: `src-tauri/src/dm_inbox_ingest.rs` (prod-ingest `apply_invite_only` site ~:944)
- Test: `src-tauri/src/community_relay_prod.rs` (relay direct arm ~:451; relay co-deposit arm ~:556)

**Interfaces:** consumes only existing test fixtures — copy the tunnel-site staging test (dm_inbox_ingest) and the butler co-deposit staging test (already full-fidelity per the ZEB-641 ticket) as templates.

- [ ] **Step 1:** For each of the three sites, add one test through the REAL site wiring (not `apply_invite` directly): non-friend invite arrives via that route → assert staged in a real `PendingDmInvites` (list() len 1, right space_id) + exactly one `dm-invite-received` and one `dm-invite-list-changed` on a recording sink; a second identical delivery emits NOTHING further (keep-first idempotence at the site level).
- [ ] **Step 2:** Run `-E 'test(staging) or test(stages)'` → all PASS (new tests fail only if wiring is broken — that is the point: these are wiring pins).
- [ ] **Step 3:** fmt + commit `"ZEB-641: staging wiring tests for prod-ingest + relay direct + relay co-deposit sites"`.

### Task 6: FriendsPanel in-flight-guard test (ZEB-641.3)

**Files:**
- Test: `src/lib/components/FriendsPanel.test.ts` (DM-invites describe block, ~:481+)

- [ ] **Step 1: Write the test** (deferred-promise mock):

```typescript
it('guards against double-invoke while accept is in flight', async () => {
  let resolveAccept!: () => void;
  const accept = vi.fn().mockImplementation(
    () => new Promise<void>((r) => { resolveAccept = r; }),
  );
  const listPending = vi.fn().mockResolvedValue([INVITE]);
  const dmInviteService = mockDmInviteService({ listPending, accept });
  const { findByTestId, getByTestId } = render(FriendsPanel, {
    props: { service: mockService(), dmInviteService },
  });

  await findByTestId('dm-invite-list');
  await fireEvent.click(getByTestId('dm-invite-accept-btn'));
  await fireEvent.click(getByTestId('dm-invite-accept-btn')); // in-flight
  expect(accept).toHaveBeenCalledTimes(1);
  resolveAccept();
});
```

- [ ] **Step 2: Run** `npx vitest run src/lib/components/FriendsPanel.test.ts` → PASS (the guard exists — this pins it; if it FAILS, the guard is broken: STOP and report).
- [ ] **Step 3:** `npx tsc --noEmit` clean; commit `"ZEB-641: FriendsPanel DM-invite in-flight-guard test"`.

### Task 7: final gates sweep

- [ ] `cd src-tauri && cargo fmt --all -- --check`
- [ ] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (the ONE full sweep)
- [ ] From repo root: `npx tsc --noEmit && npx vitest run`
- [ ] Spec cross-check: every contract bullet in the design doc maps to code + a test; the #402 golden parity tests are untouched.
- [ ] Commit any fmt-only fallout; otherwise no commit.
