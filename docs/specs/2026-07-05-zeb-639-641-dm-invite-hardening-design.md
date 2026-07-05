# ZEB-639/640/641: DM-invite hardening bundle — design

Post-review follow-ups from the ZEB-236 branch's final whole-branch review
(2026-07-04, PR #402). Three tickets, one PR: close the kicked-GroupDm
re-admit/HLC-pinning vector (ZEB-639, Medium), fix two staging lifecycle
edges (ZEB-640, Low), and pay down the remaining staging test debt
(ZEB-641, Low). Fix shapes were settled in the tickets at filing time; this
doc pins the implementation choices.

Baseline: main@7121610e (PR #402 merged — tiered consent, `PendingDmInvites`
store, `stage_and_emit_staged_invite` helper, post-accept phantom-row purge).

## ZEB-639 (1): space-exists staging gate — in `apply_invite`, not per-caller

**Vector (final review Finding 3):** the tunnel `Invite` arm has no
equivalent of the co-deposit path's `SpaceNotFound` gate, so `apply_invite`
stages invites for spaces that already exist locally. A kicked GroupDm
co-member whose devices are still cached can forge a fresh invite for the
existing `space_id` (original `content_key`, self in roster, far-future
`created_at`); the user sees a plausible consent prompt, and accepting
re-admits her locally and HLC-pins the Space (see (2)).

**Fix:** gate inside `apply_invite`'s non-friend branch (`dm_outbox.rs:2323`,
after the five trust gates + friend check, under the same `&mut OwnerState`
borrow):

```rust
if !inviter_is_active_friend {
    if state.spaces.contains_key(&signed.space_id) {
        return Ok(ApplyInviteOutcome::IgnoredExistingSpace);
    }
    return Ok(ApplyInviteOutcome::Staged(/* unchanged */));
}
```

- **New variant `ApplyInviteOutcome::IgnoredExistingSpace`** — every caller
  arm handles it as an explicit no-op (tunnel `dm_inbox_ingest.rs:~512`,
  prod-ingest `:~944`, relay direct `community_relay_prod.rs:~451`, dormant
  `dm_outbox.rs:~1826` [debug-log like its Staged warn], and
  `apply_deposited_invite` `dm_outbox.rs:~2546` maps it to `Ok(None)` like
  `Accepted`). Exhaustive-match means the compiler finds every arm.
- **Why in `apply_invite` and not the tunnel arm** (ticket's literal shape):
  one seam covers ALL six arms uniformly where the `OwnerState` is already
  borrowed; the ticket's underlying claim is "non-friend invites for
  existing spaces must not prompt", not "only the tunnel arm".
- **Semantics match the co-deposit gate exactly:** co-deposit only stages on
  a `SpaceNotFound` receive error, i.e. never when the space exists. An
  invite for a space we already hold cannot require consent — we are
  already in it; legitimate roster changes arrive via Space CRDT sync, not
  via a fresh invite.
- **Friend tier unchanged:** active-friend invites keep running the accept
  tail even when the space exists — that is the established idempotent
  redelivery-merge contract (ZEB-483 co-deposits the invite with every
  message).
- **Tombstoned spaces still stage:** a tombstoned space is NOT in
  `state.spaces` (rejection happens at apply time, `RejectionReason::
  Tombstoned`), so the gate passes and accept later surfaces the permanent
  failure via ZEB-640 (2). Consistent end-to-end.

## ZEB-639 (2): `updated_at` clamp in the accept tail

**Vector:** `run_invite_accept_tail` builds the Space with
`updated_at: signed.created_at` (`dm_outbox.rs:2383`) — invite-controlled.
`lww_merge_space` is LWW-by-`updated_at` and GroupDm `dedupe_key` is
id-derived, so members ARE mutable on the same `SpaceId`: a forged
far-future `created_at` pins the Space against every future legitimate
update (denial-of-updates), exactly the attack the cache `learned_at`
SECURITY comment (`dm_outbox.rs:2425-2434`) already defeats for the
OwnerDeviceCache.

**Fix:** clamp `updated_at` to a local-clock ceiling in the tail:

```rust
let updated_at = if signed.created_at.wall_ms > wall_now_ms {
    Hlc { wall_ms: wall_now_ms, logical: 0, device_id: device_id.to_string() }
} else {
    signed.created_at.clone()
};
```

- `created_at` keeps the claimed value (provenance/display; it does not
  drive LWW).
- Applies to BOTH tiers (shared tail): legitimate invites have past
  `created_at` → clamp is a no-op → the #402 golden byte-parity tests are
  unaffected. Only a forged/skewed future timestamp is clamped.
- Test: forge `created_at.wall_ms = u64::MAX / 2` → accepted Space's
  `updated_at.wall_ms == wall_now_ms`; a subsequent legitimate Space update
  at `wall_now_ms + 1` WINS `lww_merge_space`.

**Note-only (from ZEB-639, no code):** a long-staged tunnel invite accepted
much later re-seeds the inviter's cache row from the stale invite;
self-heals on the next CidNotify. Documented, not fixed.

## ZEB-640 (1): purge stale staged entry on friend-tier auto-accept

**Edge (final review Finding 4):** stage a non-friend invite, then befriend
the inviter, then a redelivery auto-accepts (friend tier) — the staged
entry survives: a pending toast/row for a DM that already exists in nav.

**Fix:** helper in `pending_dm_invites.rs`:

```rust
pub(crate) fn purge_stale_staged_on_accept(
    pending: Option<&std::sync::Arc<PendingDmInvites>>,
    sink: &dyn NodeEventSink,
    space_id: &SpaceId,
) {
    let Some(store) = pending else {
        return; // store not wired on this path (defensive; silent — purge is best-effort)
    };
    if store.take(space_id).is_some() {
        crate::node_event_sink::emit_ser(sink, "dm-invite-list-changed", &());
    }
}
```

(Signature mirrors `stage_and_emit_staged_invite`'s optional-store seam so the
dormant/no-store path needs no dummy store.)

Called at every live arm that observes a friend-tier accept AND holds
store+sink: the `Accepted` match arms (`dm_inbox_ingest.rs:~519`, `:~952`,
`community_relay_prod.rs:~458`) and the co-deposit sites' `Ok(None)`
branches from `apply_deposited_invite` (`dm_inbox_ingest.rs:~863` region,
`community_relay_prod.rs:~556` region). The dormant path has no store
(existing warn stands). No emit when nothing was purged (no event noise on
the hot path — every friend-tier DM redelivery hits these arms).

## ZEB-640 (2): permanent accept failures must not re-stage

**Edge (final review Finding 5):** `accept_dm_invite_impl` re-stages on ANY
`run_invite_accept_tail` error. A space tombstoned between staging and
accept yields `CrdtRejected(Tombstoned)` forever: every Accept errors and
re-stages; only Decline clears the stuck row.

**Fix:** in `accept_dm_invite_impl` (`lib.rs:~49043`), match the error:

- `DmReceiveError::CrdtRejected(reason)` → **permanent**: do NOT re-stage;
  emit `dm-invite-list-changed` (the row must disappear from both
  surfaces); return `Err("invite no longer applicable: {reason}")` — a
  distinct, user-explainable message.
- Any other variant → re-stage as today (defensive arm; the tail's only
  current error channel is `CrdtRejected`, which is deterministic —
  same input, same reject — but future tail edits may add genuinely
  transient failures and silently-losing an accept must stay impossible).

CRDT rejects are deterministic-permanent by construction (pure functions of
state + input); there is no transient CRDT failure today. Frontend needs no
change: both surfaces already show the error and refresh on
`dm-invite-list-changed`.

## ZEB-641: remaining test debt (scope adjusted)

1. **Real-store + recording-sink staging tests** for the three uncovered
   ingest sites: prod-ingest `apply_invite_only` (`dm_inbox_ingest.rs:~944`),
   relay direct arm (`community_relay_prod.rs:~451`), relay co-deposit arm
   (`community_relay_prod.rs:~556`). (Tunnel arm + butler co-deposit already
   have full-fidelity tests from #402.) Each: non-friend invite through the
   real site wiring → staged in a real `PendingDmInvites` + exactly one
   `dm-invite-received` on a recording sink.
2. **FriendsPanel in-flight-guard test** (deferred-promise mock): double-click
   Accept resolves once — the second click while in flight must not
   double-invoke.
3. ~~`decline()` direct service test~~ — **already landed** in PR #402
   round 1 (CodeRabbit finding; `dm-invite-service.test.ts` now covers
   `decline()`, `destroy()`, and partial-registration rollback). No work.

New behaviors in this bundle carry their own tests in their tasks (gate,
clamp, purge, permanent-drop); ZEB-641 is strictly the inherited debt.

## Non-goals

Durable blocklist (spec v1 exclusion, unchanged); per-community membership
refcounting (ZEB-634 territory); stale-cache re-seed on late accept
(note-only above); group-DM member-level vetting (unchanged from ZEB-236);
dormant-path staging (no store there by design — warn stands).

## Test gates (unchanged from repo convention)

Per-task scoped `cargo nextest -p harmony-app` + `cargo fmt --all` +
clippy CI-form; full `--workspace --all-targets` sweep + `tsc` + full
vitest once at the end. Golden parity tests from #402 must stay green
untouched (the clamp is a no-op for their past-dated fixtures).
