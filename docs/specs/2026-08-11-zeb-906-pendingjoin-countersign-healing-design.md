# ZEB-906: known-PendingJoin countersign healing — design

**Status:** approved (Jake, 2026-08-11)
**Ticket:** ZEB-906 — host permanently strands a stuck PendingJoin member; no self-heal; all their traffic dropped
**Related:** ZEB-254 (auto-counter-sign machinery), ZEB-903 Part B (the lagged-roster form), ZEB-911 (joiner-side witness redeem), ZEB-888 (single-use claim fence), ZEB-526 (unknown-publisher salvage)

## 1. Problem

A host that records a joiner's `PendingJoin` but never counter-signs it strands
that member permanently: every publish they send is authorization-dropped
(`PublisherNotJoined` / `NotAuthorized`), their roster entry never promotes, and
nothing in the system ever re-attempts the promotion. Confirmed in production
dogfooding (v0.2.5, Koya↔Krile, 13+ hours stuck, membership CRDT frozen on
disk).

## 2. Verified root cause — four interlocking gaps

Each mechanism assumes another covers the hole; none does.

1. **The countersign spawn can legitimately skip.** `spawn_auto_counter_sign_task`
   returns early on the `closing` shutdown fence (ZEB-712) and on sign failure,
   with comments claiming "eligibility idempotently re-derives on next boot
   (C1)". The dogfood evidence matches the shutdown-fence skip exactly: the
   host's `crdt.cbor` mtime is the shutdown flush that persisted the
   `PendingJoin` whose countersign task was fenced.
2. **The boot re-derive does not exist.** C1 "restart-recovery"
   (`community_state_sync.rs` ~1803) fires only when a `PendingJoin` is
   **re-inserted** and returns `InsertOutcome::AlreadyKnown`. The C3 restart
   healing pass (`lib.rs`, BOOT-PROBE 09 region) walks only the **joiner's
   own** `Space.pending_join_at` entries. No path on the counter-signer side
   rescans loaded `PendingJoin`s.
3. **Gossip cannot trigger the re-insert C1 needs.** The pre-mutation ingest
   gate strict-rejects publishes from a known-`PendingJoin` publisher
   (`community_state_sync.rs` ~4295) **before** any per-event insert — its
   comment even asserts "no salvage needed". The ZEB-526 unknown-publisher
   salvage (`bootstrap_admit_invite_only_publisher`) deliberately covers only
   the `member_state.is_none()` arm.
4. **No external stimulus arrives.** The reconnect supervisor never dials
   non-`Joined` peers, and the joiner self-inserted its Join locally (believes
   it is joined) so it never re-redeems.

## 3. Design

Two small, complementary heals that both funnel into the **existing**
idempotent countersign machinery. No new authority, no new event kinds, no
wire changes.

### 3.1 Fix A — ingest-seam re-drive (live heal)

In the pre-mutation gate's strict-reject arm, when the publisher's
materialized status is specifically `MemberStatus::PendingJoin`:

* locate the publisher's **latest** `PendingJoin` event in the already-cloned
  log (max by `event_sort_key`, mirroring the C3 pass's R4-5 "this attempt,
  not any prior attempt" specificity), and
* call the existing `maybe_spawn_auto_counter_sign_for_ctx(ctx, pending_ev)`
  **before** returning the (unchanged) `PublisherNotJoined` rejection.

The triggering publish itself stays rejected — the member's root remains
unauthorized until the countersign materializes. Their next periodic publish
(30–60 min cadence in production; seconds in tests) then passes the gate. In
the dogfood case this heals the stall within one publish cycle with zero user
action.

Because counter-sign authority is general (ZEB-254; reaffirmed by ZEB-911),
**any** Joined member with power ≥ the community's invite tier heals the stall
this way — not only the admin. Non-eligible receivers spawn a task that
no-ops in its eligibility check.

`Left`/`Banned` publishers do **not** trigger the spawn: the re-drive is gated
on `status_now == Some(PendingJoin)` precisely, and the strict reject is
otherwise unchanged.

### 3.2 Fix B — boot-time countersigner healing pass (restart heal)

A new `CommunitySyncEngine` method, called from the same boot healing-pass
region as the existing joiner-side C3 pass (shared by GUI and headless via
`start_node`):

```text
recheck_uncountersigned_pending_joins():
    lock state; collect PendingJoin events that have NO self-authored
    JoinCountersign targeting their id; drop lock;
    for each candidate: maybe_spawn_auto_counter_sign(&event)
```

The pre-filter (no self-authored countersign) is a cheap tidiness guard under
one lock; the spawned task remains the authoritative idempotency/eligibility
gate. This makes the existing "re-derived on next boot (C1)" comments true;
those comments are updated to reference this pass.

### 3.3 Why this composition suffices (and the ticket's option 2 is dropped)

Fix A covers every case where the stranded member is alive and publishing
(the observed failure). Fix B covers the restart window and any case where
the skip happened while the member was silent. Together they subsume the
ticket's "host-side periodic sweep" (option 2) without a timer, and they
reuse machinery whose invariants are already reviewed and test-pinned.
Option 3 (reachability-driven re-handshake) remains the joiner-side ZEB-903
Part A follow-up; ZEB-911's witness ladder already gives joiners an
independent recovery path.

## 4. Invariants preserved

* **Single-use invite fence (ZEB-888):** untouched. The spawned task keeps its
  claimed-by-other-actor guard, and the authoritative canonical-claimant rule
  at materialization is unchanged.
* **At-event-HLC verification:** untouched. The countersign is verified by
  `verify_event` at insert exactly as an insert-time countersign would be.
* **Retroactive authorization is by design:** `joined_at` materializes as the
  `PendingJoin` event's HLC (`community_membership.rs` ~3577, ZEB-254), so
  once promoted, the member's republished history verifies. This is the same
  semantics as an insert-time countersign — the heal changes *when* the
  countersign lands, not what it authorizes.
* **Backward secrecy (§10.6):** un-countersigned `PendingJoin` members remain
  non-members until the countersign materializes; the heal mints no epoch
  material and triggers the existing catchup path (`pending_catchup_for`) via
  ordinary materialization.
* **Shutdown-flush guard (ZEB-712):** the spawned task keeps the `closing`
  fence; a skip during shutdown is now genuinely recovered by Fix B at next
  boot.

## 5. Cost and abuse analysis

* Fix A adds, per rejected known-`PendingJoin` publish, one spawned task whose
  eligibility check clones the event log once under the state lock — bounded
  by the same per-publish materialize the gate itself already performs, and a
  no-op after the first countersign lands (`already_signed`). A hostile
  `PendingJoin` holder spamming roots adds a constant factor to an
  already-per-publish-materializing path; no amplification, no unbounded
  state.
* Fix B is one log scan per community engine per boot plus at most one
  spawned no-op-able task per un-countersigned `PendingJoin`.

## 6. Observability

* Fix A logs `info!` when it fires: community, publisher, target event id
  ("ZEB-906: re-driving countersign for known-PendingJoin publisher").
* Fix B logs `info!` with the candidate count per community when non-zero.
* The misleading "no salvage needed" comment in the reject arm and the two
  "re-derived on next boot" comments are rewritten to describe the actual
  recovery paths.

## 7. Testing

Engine-level (existing `handle_incoming_publish` / engine harnesses):

1. Known-`PendingJoin` publisher root publish → publish rejected
   (`PublisherNotJoined`) **and** a self-authored `JoinCountersign` appears
   (poll); a subsequent publish from the same publisher is accepted; member
   materializes `Joined`.
2. `Left` and `Banned` publishers → still strict-rejected, **no** countersign
   spawned.
3. Idempotency: publish arriving while the countersign already exists → no
   duplicate countersign.
4. ZEB-888 guard: token claimed by a different actor → no countersign.
5. `recheck_uncountersigned_pending_joins`: state loaded with a foreign
   un-countersigned `PendingJoin`, eligible self → countersign appears;
   ineligible self (not Joined) → no-op; already-countersigned → no spawn.
6. Boot wiring: the healing pass calls the recheck for each community engine
   (exercised via the existing start_node boot-path coverage).

No frontend changes.
