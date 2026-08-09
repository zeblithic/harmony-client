# ZEB-888 — Single-use invite claim as a convergent materialization invariant

**Status:** approved (design) — 2026-08-08
**Ticket:** [ZEB-888](https://linear.app/zeblith/issue/ZEB-888) (High) — "Invite single-use claim not enforced on the CRDT state-root merge path — a second distinct actor can join a single-use invite (ZEB-875 gap)"
**Related:** ZEB-875 (#621, the local-path claim), ZEB-874 (#619), ZEB-876 (Tier-3 tombstone), ZEB-526 (bootstrap admit)

## Problem

ZEB-875 claimed a *claimant-bound atomic single-use invite claim*: the first
claimant of an invite-token signature wins; a distinct actor is refused. That
holds **only** for redeems that arrive through the local mint path
(`insert_local_claim_bound_pending_join`). It is **not** enforced on the durable
state-root CRDT merge path, so a second distinct actor B can still materialize as
`Joined` on the same single-use, untargeted invite.

### Where the claim lives today (and why it leaks)

The claim is a single `LocalInsertPrecheck::NoConflictingClaimantForInvite`
(`community_state_sync.rs:877`) that scans the raw event log for a `PendingJoin`
carrying the same `invite_token.sig` from a different actor and refuses it under
the `state` lock. Three facts let B through:

1. **Merge path** (`community_state_sync.rs:4747`) calls `state.insert_event`
   with `precheck = None`. Every non-local insert bypasses the check.
2. **Auto-countersign eligibility** (`spawn_auto_counter_sign_task`,
   `community_state_sync.rs:2266`) dedupes on `target_event_id == pending_id` —
   *per PendingJoin event id, not per invite token* — and *any* joined member
   (invite tier 0), not only the host, runs it. So a fresh PendingJoin from B is
   auto-countersigned.
3. **Materialization** (`community_membership.rs:2658`, `:3380`) turns "any
   `JoinCountersign` targets this event id" straight into `Joined`, with no
   notion that the token is single-use.

### Failure scenario (untargeted invite; both parties hold the link)

1. Actor A redeems via iroh → claim recorded for A, host auto-countersigns,
   invite burned. Correct.
2. Actor B redeems the same link via iroh → host refuses; B's read times out →
   `RedemptionOutcome { status: "inviter_unreachable" }`.
3. The UI treats `inviter_unreachable` as recoverable and shows the "Try via
   local network" button. B clicks it → `redeem_invite_inner` mints a fresh
   PendingJoin, inserts into B's own engine, publishes its state root; on
   countersign timeout it does **not** roll back.
4. Host merges B's state root → ZEB-526 `bootstrap_admit_invite_only_publisher`
   admits B → merge loop inserts B's PendingJoin at `:4747` with no precheck →
   auto-countersign fires → B is countersigned → **B materializes as Joined**.

One single-use token has admitted two distinct actors. The pkarr burn does not
help: `redeem_invite_inner` decodes the invite URL directly and never consults
pkarr. Any peer holding the URL who reaches the host over Zenoh state-sync can do
this. Targeted invites are unaffected (`invitee_hint` is enforced in
`verify_event` P3 on both paths); the gap is specific to **untargeted /
"controlled open" single-use invites**.

## Invariant (new, precise)

> For any single-use invite token, at most one actor materializes as `Joined`;
> and that actor is fixed by the **earliest countersign**, so a `Joined` member
> cannot be displaced by a later claimant of the same token.

## Design constraint: gate the view, not the store

The tempting fix — reject B's PendingJoin at `verify_event`/`insert_event` when a
conflicting PendingJoin already exists — is a **CRDT trap**. Accept/reject-at-
insert based on other events is *order-dependent*: a replica that receives B
before A accepts B and has nothing to reject A against; a replica that receives A
first rejects B. Result: replica-1 = `{A}`, replica-2 = `{B}` → **permanent
state-root divergence**. You also cannot "reject" an event already durably in a
peer's log.

Therefore the event log stays **grow-only** (both PendingJoins live in it), and
single-use is enforced where materialization computes `Joined` — a **pure
function of the event set**, hence convergent by construction. This mirrors the
forward-skew lesson (ZEB-831/#621): gate the display *view*, never delete/reject
at the *store*.

## Architecture: three layers, one authoritative

| Layer | Site | Role | Convergent? |
|---|---|---|---|
| **1. Materialization gate** | `community_membership.rs` `materialize_with_now` pre-pass + PendingJoin arm | **Authoritative fence** | ✅ pure function of event set |
| **2. Auto-countersign guard** | `community_state_sync.rs` `spawn_auto_counter_sign_task` eligibility block | Defense-in-depth: stop emitting phantom countersigns for already-claimed tokens | best-effort (racy by nature) |
| **3. Local precheck** | `LocalInsertPrecheck::NoConflictingClaimantForInvite` | Fast-fail UX on local mint | unchanged |

### Layer 1 — materialization gate (the fence)

A new pre-pass runs alongside the existing `countersigned_pending_ids` pre-pass
in `materialize_with_now` and computes a **canonical claimant per token**:

```
pending_by_id : EventId -> (token_sig: [u8;64], actor: OwnerAddr)   // from PendingJoin events
// For each JoinCountersign C whose target_event_id is a known PendingJoin
// carrying token T, track the C minimal by event_sort_key(C), keyed by T.
// canonical_pending_ids = { that minimal C's target_event_id : for each T }
```

The PendingJoin materialization arm then changes exactly one line of intent:

```rust
// was: let countersigned = countersigned_pending_ids.contains(&event.id);
let countersigned = canonical_pending_ids.contains(&event.id);
```

A countersigned-but-**non-canonical** PendingJoin (B) falls through to the
existing `else if !expired` branch → renders as `PendingJoin`, never `Joined`.

- `event_sort_key` = `(wall_ms, logical, device_id, id, sig)` — the identical
  total order the main pass already sorts by (`community_membership.rs:2260`,
  `:2747`); no new ordering vocabulary is introduced.
- **Tiebreak is on the countersign, not the PendingJoin.** This defeats HLC-
  backdated displacement: an outside actor B controls their self-signed
  PendingJoin's clock but cannot author a valid `JoinCountersign` (they are not
  `Joined` → `JoinCountersignActorNotJoined`). So the earliest countersign — in
  the honest flow, the host's countersign of A — permanently fixes the winner.

Placement is the shared `materialize_with_now` core so both live-status and
`prior_state_at_hlc` (verify_event P6) see the same rule. This does not change
verify semantics: P6 reads only the actor's *own* prior state, and a non-canonical
B is `PendingJoin`/`None` in prior state either way; A is `Joined` either way.

### Layer 2 — auto-countersign guard (defense-in-depth)

In the eligibility block of `spawn_auto_counter_sign_task` (already under the
`state` lock, already scanning `events()` for `already_signed`), look up
`pending_id`'s token and skip (do not emit a countersign) if any `JoinCountersign`
already targets a *different-actor* PendingJoin carrying that same token. The
function signature is unchanged — everything is derived from `state.events()`.

This layer is **best-effort**: two joined members can each countersign B before
seeing each other's countersign. That race is exactly why Layer 1 is the real
fence; Layer 2 only keeps the log clean (no phantom countersigns) in the common
single-host case.

### Layer 3 — local precheck (unchanged)

`LocalInsertPrecheck::NoConflictingClaimantForInvite` stays as-is: a local
fast-fail so a node that already knows of a conflicting claim refuses to mint one
locally. It is local-only and therefore cannot cause divergence; it is a UX
optimization, not the security boundary.

## Semantics / error handling

- **No new `VerifyError` variant** — deliberately. Insert-time rejection is the
  CRDT trap above.
- **B's converged outcome:** `PendingJoin` until the 30-day materialize expiry
  (`MATERIALIZE_PENDING_EXPIRY_MS`), then hidden. Actively tombstoning B's
  PendingJoin/countersign is **ZEB-876's** scope (reversing a *committed*
  countersign) and out of scope here.
- **Transient:** on a replica that observes B's countersign before A's, B may
  render `Joined` until A's (lower-`event_sort_key`) countersign propagates —
  normal eventual consistency. The *converged* state always satisfies the
  invariant.
- **Same actor, same token, multiple PendingJoins** (A re-redeems): all carry
  A's actor, so canonical resolves to one of A's PendingJoins and A is `Joined`
  once — no second actor, no regression.

## Testing

Materialize unit tests (`community_membership.rs`):

1. **At-most-one:** two distinct actors, one token, both PendingJoins
   countersigned → exactly one `Joined` (the earliest-countersign actor), the
   other `PendingJoin`.
2. **Displacement resistance:** A's countersign at `wall_ms=100`; B's PendingJoin
   backdated to `wall_ms=1` but B's countersign at `wall_ms=200` → A stays
   `Joined`, B does not.
3. **Same-actor idempotence:** one actor, two PendingJoins on one token, both
   countersigned → `Joined` once, no panic.
4. **Convergence property:** shuffle the input event order → identical
   `MaterializedMembership`.

Engine tests (`community_state_sync.rs`):

5. **Auto-countersign guard:** state already holds A's countersigned PendingJoin
   for token T; merge B's PendingJoin for T → the host emits **no**
   `JoinCountersign` targeting B.
6. **Merge-path regression:** reproduce the ticket scenario at the state-sync
   level (B's state root merged into the host) → B never materializes as
   `Joined`. Extends the existing ZEB-875 `insert_local_claim_bound_pending_join`
   suite (`community_state_sync.rs:10610+`), which covers only the local path.

## Out of scope

- Reversing an already-committed wrong countersign / tombstoning a phantom member
  (ZEB-876).
- B's client-side redeem UX / ack-rollback leg (ZEB-874/875/889 territory). B's
  local node will hold a perpetual pending state; improving that message is a
  separate concern.
- Any change to targeted invites (already protected by `invitee_hint`).
