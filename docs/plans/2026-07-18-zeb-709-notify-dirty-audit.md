# ZEB-709: owner-state notify_dirty discipline audit — findings + fix plan

**Ticket:** ZEB-709. **Branch:** `zeb-709-notify-dirty-audit` off `main@4a4c47ff`.
Closes the durability epic (ZEB-703 headless → ZEB-708 GUI → this audit).

## Audit method

Three parallel read-only sweeps (2026-07-18): (A) dm_inbox_ingest prod wiring
trace, (B) CidNotify-handler orphan verification, (C) exhaustive
`crdt_state` LOCAL-mutation sweep. Every NO-NOTIFY verdict below was then
re-verified first-hand before being fixed.

**The rule (re-confirmed in fleet_sync.rs task loop):** the owner-state
`SyncEngine` persists ONLY on `notify_dirty` (debounced) or explicit
`persist_now`/`flush_now`. There is NO periodic checkpoint. An un-notified
LOCAL mutation reaches disk only if something else arms the dirty bit first —
or at the ZEB-703/708 shutdown persist, which crash/SIGKILL skips.

## Confirmed NO-NOTIFY sites and fixes

### Group A — local user actions, no backstop (highest severity)

| Site | Mutates | Fix |
|---|---|---|
| A1 `fork_community` (community_fork.rs:769) | `spaces` (forked community Space) | thread `sync_engine` into the handle snapshot + `fence_owner_state_flush` after commit — exact `create_community` / ZEB-393 precedent (its fenced twin) |
| A2 `add_space_impl` (lib.rs:14106 spaces + 14213 invite outbox entry) | every DM/GroupDM creation | same: snapshot gains `sync_engine`, fence after the commit block |
| A3 `add_library` (lib.rs:36838) | `libraries` insert | `notify_dirty` + DELETE the false comment ("SyncEngine debounces its own checkpoint" — no such mechanism exists; it also cites un-notified `add_space` as precedent) |
| A4 `remove_library` (lib.rs:36902) | `libraries` LWW tombstone | `notify_dirty`, symmetric with A3 |

Fence vs plain notify: fences for A1/A2 (durable-on-commit user actions that
hand out identifiers/invites referencing the Space — same class as
create/redeem/leave community, all already fenced); plain notify for A3/A4
(LWW flags, same class as `set_friend_referrable`).

### Group D — DM-receive ingest (permanent-loss window, subtle)

The "deposit rung covers durability" comment is UNSOUND as written. Ordering
today: `apply_inbox` (owner-state, in-memory, un-notified) →
`ingested_by.insert(self)` (dm-inbox dataset doc) → sweep `notify_dirty` (the
DATASET engine) → the ACK persists + replicates within ~250 ms while the
PAYLOAD waits for an unrelated owner-state flush. Crash in that window →
restart: sweeper skips the entry (`ingested_by.contains(self)`), coverage-GC
destroys it, deposit clears — the durable ack has destroyed the only recovery
path. The revocation arm already notifies for exactly this reason (no
backstop).

Fix: invoke the EXISTING `notify_owner_state_dirty` hook after every
owner-state write in the ingest flows. This does not make the two engines
atomic — it flips the failure direction to safe: if the payload persists and
the ack doesn't, the retried ingest dedupes (`apply_inbox` is idempotent on
`(space_id, message_cid)`; invite applies are idempotent upserts). Residual
exposure shrinks from unbounded to ±one debounce window, in the safe
direction.

Sites:
- `ProdDmInboxIngestCtx` (sweeper): `apply_inbox` (dm_inbox_ingest.rs:1181),
  `apply_invite_only`→`apply_invite` (:1084), `verify`→`apply_deposited_invite`
  (:962). Hook already threaded (`notify_owner_state_dirty`, wired to the
  owner-state engine at lib.rs:5318).
- `ingest_dm_packet` (live tunnel): `apply_inbox` (:802), invite arm (:539).
  Caller already passes `Some(&mark_owner_state_dirty)` (lib.rs:9906).
- `ProdRelayIngestCtx::ingest_recovered` (community_relay_prod.rs:401):
  `apply_invite` (:443), `apply_deposited_invite` (:558), `apply_inbox`
  (:646). Struct holds NO owner-state handle — thread the same
  `Arc<dyn Fn()>` notify closure in and call it at all three sites.

Notify-on-change only (insert/accepted outcomes), matching the revocation
arm's dirty-once discipline that `prod_ctx_with_dirty` tests pin.

### Groups B/C — backstopped cache writes (document, no code change)

- B1 `kick_from_community_impl` epoch rotation (lib.rs:39970), B2
  `apply_remote_epoch_event` (lib.rs:42146/42214): owner-state epoch-key
  cache; re-projected from the community engine's authoritative state at next
  boot. Comment documents the reliance.
- C1 `maybe_spawn_pending_join_clear` (community_state_sync.rs:2154):
  pending_join clear; boot C3-heal re-derives (its own comment already says
  so). Comment cross-references this audit.

### Leg B — CidNotify handlers: ORPHANED (no live bug)

`DmOutbox::handle_cidnotify_lifted` / `handle_unicast` have ZERO production
callers (every live CidNotify producer feeds `dm_inbox_ingest::ingest_dm_packet`,
which carries the notify). The missing notify at dm_outbox.rs:2085 is dead
code, not a live bug. The deletion cascade is large (~15 unit tests + 2
integration files that exercise real verify/decrypt logic through the
handlers), so this PR only CORRECTS the stale doc comment ("spawned by
event_loop's pre-decode branch" — that dispatch no longer exists) and marks
the handlers as test-only harness surface; deletion is follow-up cleanup.

## Deferred (stay on ZEB-709 or follow-ups)

- Direct-persist seam (handle-held `persist_now` bypassing the engine's
  select! loop; Qodo starvation addendum) — structural fleet_sync change,
  separate PR.
- Generic dirty-window tripwire (state-hash compare at persist vs last
  notify) — the per-site dirty-counter/runtime-persist pins added here cover
  the audited surface; the generic seam is follow-up hardening.
- Fence residues from #485 (permit-exhaustion visibility, stop_inner
  try_lock no-fence) — documented pathological cases, unchanged.
- Orphaned-handler deletion (above).

## Tests (TDD, red-first)

1. A2: `add_space` runtime-persist pin — Space + invite outbox entry reach
   `owner_state_crdt.cbor` without any shutdown (ZEB-703 test style; RED
   today).
2. A1: fork commit marks owner-state dirty (seam-level; fork_community is
   IPC-heavy — pin at the narrowest testable seam).
3. A3/A4: library add/remove mark owner-state dirty (mock-IPC or impl-level).
4. D sweeper: `ingest_pending` message ingest marks owner-state dirty exactly
   once on insert, zero on idempotent re-sweep — extend the existing
   `prod_ctx_with_dirty` dirty-counter harness (RED today; currently only
   revocation arms are pinned).
5. D tunnel: `ingest_dm_packet` message insert marks owner-state dirty
   (extend the existing tunnel dirty tests; RED today).
6. D relay: `ingest_recovered` marks owner-state dirty at its three sites
   (community_relay_prod test harness).
