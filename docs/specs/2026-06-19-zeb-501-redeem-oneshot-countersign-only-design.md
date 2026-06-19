# ZEB-501: Redeem oneshot must wake only on a real JoinCountersign

**Status:** design approved (Jake, 2026-06-19) — Option B, Symmetric fix scope
**Branch:** `zeb-501-redeem-oneshot-countersign-only` (off post-#293 `main` `8d0d5fe5`)

## Goal

Make the invite-only community **redeem** path's reported membership state —
`RedeemInviteResultDto.pending` and the `Space.pending_join_at` greying — reflect
the **true materialized membership**: `Joined` only once the admin's
`JoinCountersign` exists, `pending` (greyed) while awaiting an offline admin.
Today the redeem always reports `pending = false` because the joiner's own
`PendingJoin` insert self-satisfies the wait.

## Problem (root cause)

On the redeem path:

1. `redeem_invite_inner_with_overrides` registers a oneshot keyed on
   `bootstrap_join.id` (step 7a, `lib.rs:~22921`), intending to wait for the
   admin's `JoinCountersign` (whose `target_event_id == bootstrap_join.id`).
2. It then inserts the joiner's **own** `bootstrap_join` (step 7,
   `lib.rs:~22988` → `insert_local_event` → `insert_event_with_resolved_pubs`).
3. That insert's post-Inserted hook fires
   `notify_pending_redemption_in_map(pending, &event.id)`
   (`community_state_sync.rs:1389`). For the PendingJoin, `event.id ==
   bootstrap_join.id` — the exact key registered at 7a — so the joiner's own
   insert **synchronously resolves the oneshot before the step-7d timeout await
   even begins** (confirmed via a 1 ms `HARMONY_REDEEM_INVITE_TIMEOUT_MS` probe:
   still `pending: false`, so it is synchronous, not a race).
4. Step 7d therefore always takes the `Ok(Ok(()))` "counter-signed Join landed"
   arm → `pending_redemption_timed_out` stays false → step 9 commits
   `pending_join_at = None`.

### Why this is a correctness bug, not cosmetic

The membership materializer **already gates `Joined` on the countersign**:

- `MemberStatus::PendingJoin` exists for exactly "joiner minted a PendingJoin but
  no JoinCountersign is materialized" (`community_membership.rs:1514`).
- The PendingJoin arm renders `Joined` **only if** the id is in the
  `countersigned_pending_ids` pre-pass set (`community_membership.rs:1751`,
  arm at `:2348`); otherwise it stays terminal `PendingJoin`.
- An uncountersigned PendingJoin is intentionally **not accepted** as joined
  (`community_membership.rs:2285`).

So an admin's `JoinCountersign` genuinely gates `Joined` membership on every
peer. The self-fire makes the redeem report `pending = false` ("you're in")
while the joiner is actually in `PendingJoin` limbo on every peer — including the
joiner's own materializer — until the admin countersigns. With an offline admin
that is a perceived-membership split-brain, and it defeats the ZEB-254 greying
that was built to surface this state.

The durable local commit on an unreachable admin is **intended** (ZEB-254 /
ZEB-474 latched-pending model, confirmed not a ZEB-258 regression in ZEB-500).
Only the **reported `pending` flag** is wrong.

## Decision

**Option B — require countersign, surface pending.** Keep the
admin-countersign-gates-membership model (consistent with the materializer and
the just-merged ZEB-497 inviter-enrollment tightening). Fix the redeem so it
genuinely waits for a real `JoinCountersign`; `pending` / the greying then
reflect true materialized status.

Rejected: **Option A (optimistic join)** — to be coherent it would require
changing the materializer so an uncountersigned PendingJoin renders `Joined`,
dropping the countersign-gates-membership model entirely. That reverses ZEB-497's
direction and is a much larger governance change.

## Design

### The fix (production) — Symmetric scope

The `notify_pending_redemption_in_map(pending, &event.id)` calls are legacy
**ZEB-262 Phase-4** "fire on the event's own id," which predates countersigning.
The only semantically-correct waker is a `JoinCountersign` with matching
`target_event_id` — already present beside each `event.id` notify. Remove the
`event.id` notify at **both** sites, keep the `target_event_id` notify at both:

- `community_state_sync.rs:1389` — the shared local-insert body
  (`insert_event_with_resolved_pubs`, reached by `insert_local_event` and
  `insert_local_event_with_pubs`). This is the one the joiner's own
  `bootstrap_join` insert hits — **the bug**.
- `community_state_sync.rs:3459` — the remote-merge path
  (`process_inbound` / merge loop over `inserted_events`). Harmless in practice
  (the joiner's own PendingJoin echo arrives `AlreadyKnown`, and notify is gated
  on `Inserted`), but removed for symmetry and to delete the legacy footgun.

After the fix the redeem oneshot fires **only** when a `JoinCountersign` with
matching `target_event_id` is inserted (locally pre-delivered or via remote
sync).

#### Blast-radius confirmation

- Only one real registrant of the oneshot: the joiner's redeem at
  `lib.rs:22921` (the other two `register_pending_redemption` call sites are unit
  tests).
- The admin's iroh acceptor does **not** use the oneshot — it polls CRDT state
  directly (`iroh_invite_acceptor.rs:352`: "we cannot wait on the
  pending_redemptions oneshot; the engine's state is the canonical signal").
- The pair-insert path (`community_state_sync.rs:~1532/1543`) notifies on raw
  `first.id` / `second.id` and is used for atomic kick+rotation pairs — it never
  carries countersigns and no redeem waits on those ids. **Out of scope.**

### Test-timeout seam (avoids the env race #293 removed)

Removing the self-fire means a no-countersign test would block for the full 5 s
default. Instead of reintroducing the process-global `HARMONY_REDEEM_INVITE_TIMEOUT_MS`
override (the cross-test env race ZEB-500 / #293 just deleted), add an explicit
override field:

- Add `redeem_timeout: Option<std::time::Duration>` to `RedeemInviteOverrides`.
- In step 7d, the effective timeout is `overrides.redeem_timeout` if `Some`,
  else the existing `HARMONY_REDEEM_INVITE_TIMEOUT_MS`-or-`5000ms` default. No
  process-global mutation in tests; production behavior unchanged when `None`.

### Resulting behavior

- **Admin online / `pre_delivered_countersign` present:** the countersign is
  inserted → `target_event_id` notify → fast resolve → `pending = false`
  (Joined).
- **Admin offline (no countersign within the timeout):** step-7d timeout →
  `pending_redemption_timed_out = true` → commit `pending_join_at = Some` →
  `pending = true` (greyed). The ZEB-254 R3 (C2) and R5-2 TOCTOU rechecks
  (`lib.rs:~23205/23268`) become genuinely reachable and meaningful — they
  flip the flag back to `false` if a countersign lands during the commit window.
- **Late countersign (after timeout):** existing boot-heal / live materialization
  ungreys; unchanged.

## Testing

In `src-tauri/tests/community_sync_integration.rs`, using the existing fixture
helper and the new `redeem_timeout` override (short, e.g. 50 ms):

1. **Update** `redeem_invite_only_commits_durable_join_when_inviter_unreachable`:
   under the fix, an unreachable inviter → no countersign → timeout →
   `pending == true`, `pending_join_at.is_some()`, Space still committed
   (durable latched-pending). Rename to reflect "commits **pending** join."
   Keep the ZEB-501 cross-ref comment but invert it to describe the fixed
   behavior.
2. **Keep** `redeem_invite_only_rolls_back_owner_state_on_fence_failure`: still
   asserts the ZEB-258 rollback (`pre == post`, `Err`), now reaching the fence
   after a short timeout via the override.
3. **Add** a positive countersign test: with `pre_delivered_countersign:
   Some(..)` (+ the required `admin_identity_pub`), redeem returns
   `pending == false`, `pending_join_at.is_none()`, and the joiner materializes
   `Joined`. Locks in "countersign ⇒ joined ⇒ not greyed."
4. **Add** a remote-merge wake test: a `JoinCountersign` ingested via the
   inbound/merge path wakes a waiting redeem (proves the `target_event_id` notify
   at `:3459` survives the `event.id` removal).

Full gate (from `src-tauri/`): `cargo fmt --all -- --check` +
`cargo clippy --all-targets --features test-fixtures --no-deps -- -D warnings` +
`cargo nextest run --all-targets --features test-fixtures`.

## Files

- `src-tauri/src/community_state_sync.rs` — 2 deletions (`:1389`, `:3459`).
- `src-tauri/src/lib.rs` — add `redeem_timeout: Option<Duration>` to
  `RedeemInviteOverrides`; thread it into the step-7d `tokio::time::timeout(...)`;
  fix the misleading `RedeemInviteResultDto.pending` doc comment
  (`lib.rs:~22116-22122`) to describe the now-reachable offline-admin case.
- `src-tauri/tests/community_sync_integration.rs` — update 2 tests, add 2.

## Out of scope

- The pair-insert path (`community_state_sync.rs:~1532/1543`) — never carries
  countersigns.
- The `notify_pending_redemption` public method (`community_state_sync.rs:4279`)
  — key-explicit, callers control the key; unaffected.
- Any change to the materializer or the countersign-gates-membership model.

## Sequencing

#293 (ZEB-500 + ZEB-498) is merged. No other open harmony-client PR, so ZEB-501
proceeds straight through to its own PR after spec + plan review.
