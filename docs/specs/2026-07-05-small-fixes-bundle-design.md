# Small-Fixes Bundle: ZEB-643 / ZEB-642 / ZEB-637 / ZEB-625 — Design

**Date:** 2026-07-05
**Branch:** `zeb-625-637-642-643-small-fixes` (off main 8816d3b9)
**Tickets:** ZEB-643 (eviction interleave), ZEB-642 (DM-invite purge symmetrization),
ZEB-637 (self row in network-health peers), ZEB-625 (kick-vs-floor test pins)

One bundled PR of four independent Low-priority residuals, per the standing
bundle-small-PRs rule. Each section is self-contained; there are no
cross-section interfaces.

---

## 1. ZEB-643 — record-less slot creation via NewPeer/eviction interleave

### Problem

The Leave/Kick eviction arms in `lib.rs` capture `departed_nodes =
resolver.resolve(actor)` and then call `resolver.remove_owner(actor)` as two
separate lock acquisitions (`resolve` takes the `inner` read lock and drops
it; `remove_owner` takes the write lock). A concurrent
`resolver.update(actor, payload{n2})` for a NEW device can commit between
them: `remove_owner` then deletes n2's record, but n2 was never captured, so
`evict_peer(n2)` is never called and n2's pending `NewPeer` kick drains
record-less into an orphan Retrying slot that ladders to a process-lifetime
Dormant (the `apply_trigger` ZEB-634 gate is `Dropped`-only).

### Mechanism: `remove_owner` returns the removed node-ids

`ReachabilityResolver::remove_owner` (`reachability_resolver.rs:593`) already
collects the exact keys it deletes — `to_remove: Vec<ResolverKey>` where
`ResolverKey = (OwnerAddr, [u8; 32])` and the second element IS the iroh
node-id — under a single `self.inner.write()` hold. Change the signature:

```rust
pub fn remove_owner(&self, actor: &OwnerAddr) -> Vec<[u8; 32]>
```

returning `to_remove.iter().map(|k| k.1).collect()` (keys are unique per
node-id; no dedupe needed). The decision "what to remove" and the removal
itself happen under one write-lock hold, so no `update` can interleave — the
returned set is the authoritative deleted set. The generation bump condition
becomes `if !removed.is_empty()`.

Both production call sites (Leave arm `lib.rs:~5959`, Kick arm `lib.rs:~6007`)
then:
- DELETE the `departed_nodes` pre-capture (`resolver.resolve(...)` +
  map-to-node-ids) entirely — one fewer resolver read;
- bind `let departed_nodes = resolver.remove_owner(...)`;
- replace the `n > 0` gate with `!departed_nodes.is_empty()`;
- keep the eviction loop / notify / `emit_changed` bodies unchanged.

The obsolete "capture BEFORE the resolver forgets" comments are replaced with
a ZEB-643 comment stating the atomic-return contract.

### Why this closes the race causally

`SupervisorHandle::evict_peer` (`reconnect_supervisor.rs:335`) removes the
peer's slot AND its already-queued pending kick (`dirty.remove`) under both
mutexes. In the interleave, n2 is now in the returned set → `evict_peer(n2)`
runs → the pending `NewPeer(n2)` kick is cleared before it can drain. A
LATER legitimate re-announce still fires a fresh `NewPeer` after the resolver
write, which correctly recreates the slot.

### Bonus closure

`resolve()` returns the durable-preferred payload view while `remove_owner`
deletes the full key range; the two could diverge independent of the race.
Returning the deleted keys' node-ids also eliminates that view-vs-keyset
mismatch.

### Callers to mechanically update (return type change)

Tests asserting the old count use `.len()`:
`lib.rs:59233` (n==2), `lib.rs:59280`, `lib.rs:59320` (n==1),
`lib.rs:59389` (n==1); `reconnect_supervisor.rs:1797` (ignores return);
`reachability_resolver.rs` self-tests at 919, 924, 1199, 1214, 1217, 1219,
1475.

### Tests

1. `remove_owner_returns_removed_node_ids` (reachability_resolver.rs): seed
   owner A with two devices (n1 durable, n2 pkarr-only), owner B with one;
   `remove_owner(&A)` returns exactly {n1, n2} (order-insensitive), B's
   record survives, second call returns empty vec.
2. `eviction_uses_removed_set_clears_interleaved_newpeer_kick`
   (reconnect_supervisor.rs, paused-clock style of the ZEB-634 tests): seed
   resolver with A→n1; simulate the interleave by adding A→n2 AFTER a
   hypothetical capture point (i.e., just before removal), queue
   `kick(n2, NewPeer)`; call `remove_owner(&A)` and evict every returned
   node-id; drain; assert NO slot exists for n1 or n2 and n2's pending
   trigger is cleared.
3. Existing hook-mirror tests in `lib.rs` (`zeb_321_event_loop_wiring_tests`)
   updated to the new return type; assertions preserved via `.len()`.

### Non-goals

- No change to `apply_trigger`'s gate (stays `Dropped`-only; ticket fix
  shape (a) rejected — it adds a resolver probe to the hot record-backed
  path for a race now closed at the source).
- The ZEB-643 "watch-seam" note (projection staleness if a future
  forget-community path bypasses deltas) is a documentation-only watch item;
  no code. It remains recorded on the Linear ticket.

---

## 2. ZEB-642 — DM-invite staging purge symmetrization (items 1–3)

### Item 1: purge in the three direct `IgnoredExistingSpace` arms

The co-deposit legs already purge a stale staged entry when an invite
resolves against an already-existing space (via `apply_deposited_invite`'s
`Ok(None)` conflation). The three DIRECT arms are log-only no-ops:

| Site | Store handle | Sink |
|---|---|---|
| `dm_inbox_ingest.rs:532` (tunnel ingest) | `pending_invites` | `sink` |
| `dm_inbox_ingest.rs:1008` (deposit-recover apply; returns `Ok(())`) | `self.pending_dm_invites` | `self.sink` |
| `community_relay_prod.rs:471` (relay recover) | `self.pending_dm_invites` | `self.sink` |

Fix: in each arm, after the existing `tracing::debug!`, call the existing
helper exactly as the adjacent `Accepted` arm does:

```rust
crate::pending_dm_invites::purge_stale_staged_on_accept(
    <store>.as_ref(),
    <sink>.as_ref(),
    &invite_space_id,
);
```

Lock-safety: the helper's caller contract (crdt lock dropped) is already
satisfied at these exact match positions — the `Accepted` arm immediately
above each one calls the same helper inline. The helper emits
`dm-invite-list-changed` only when a row was actually removed, so repeated
redeliveries stay event-quiet. Update each arm's `// no-op` comment to a
ZEB-642 purge rationale (a staged row is stale by definition once the space
exists — the same argument that blessed the co-deposit conflation).

The FOURTH `IgnoredExistingSpace` arm (`dm_outbox.rs:1836`, dormant
`handle_invite`) has no store/sink wired and is OUT OF SCOPE.

### Item 2: skip-window doc comments (doc-only)

At the two byte-identical `purge_stale_staged` flag declarations, append one
differentiating line each:

- `dm_inbox_ingest.rs:826`: the purge runs BEFORE blob↔packet binding
  (step 4 at :919-931 is after the purge at :912) — dm_inbox's skip-window
  closes before blob-binding.
- `community_relay_prod.rs:519`: the lock scope (:520-645) encloses
  blob-binding, decrypt, and `apply_inbox`, all of which can `return Err`
  before the purge at :650 — relay's skip-window extends through
  `apply_inbox`.

### Item 3: tombstone-staging test pin

New test in `dm_outbox.rs` `mod tests`, mirroring
`non_friend_invite_for_existing_space_is_ignored_not_staged` (:5990):
`non_friend_invite_for_tombstoned_space_still_stages` — build the fixture
invite (`build_valid_dm_invite`), call
`state.tombstone_space(SpaceId([7; 16]))` (the fixture's space id), run
`apply_invite` from a NON-friend, assert the outcome is `Staged` (not
`IgnoredExistingSpace`) and canonical state bytes are unchanged
(`owner_state_persist::canonicalize` before/after). This pins the
`state.spaces.contains_key` gate comment at `dm_outbox.rs:2344-2347`
(tombstoned spaces are NOT in `spaces`, so they still stage; accept later
surfaces the permanent rejection).

### Non-goals

Ticket item 4 (CrdtRejected error-message wording on the
refresh-entitled partial-mutation path) — explicitly no-action per ticket.

---

## 3. ZEB-637 — filter the self row out of `network_health_snapshot.peers[]`

### Problem

The membership consumer's `ReachabilityAnnounce` arm applies the node's OWN
announces to its own resolver (no self-skip at `lib.rs:5843` and none in the
resolver), and the membership projection includes self in every joined set,
so `communities_shared_with(self)` is non-empty and the self record passes
`filter_peers_by_shared_membership`. No connection source ever matches self
(self-test never pings self; liveness tracks remotes only), so the row is
permanently `noConnection`. Bitten twice (GCE suite any-peer assert; both
Windows agents flagged it during the flag-day).

### Decision: filter, not tag

Filter the self owner out of `peers[]`. The panel already carries self
health in top-level snapshot fields; no UI logic or vitest fixture consumes
a self row (verified: `NetworkHealthView.svelte` renders rows generically;
no fixture contains one). Tagging `role: "self"` would extend the DTO for a
row nobody wants rendered.

We deliberately KEEP the self record in the resolver itself (the announce
arm stays self-blind): the resolver's self entry is harmless bookkeeping,
and filtering at snapshot assembly is the narrowest change that fixes every
consumer of `peers[]`.

### Mechanism

1. `filter_peers_by_shared_membership` (`network_health.rs:553`) gains a
   parameter `self_owner: Option<&[u8; 16]>`; the record loop skips
   `r.owner_addr == *self` records before the shared-communities check.
   (Filter-level placement keeps it unit-testable with the six existing
   filter tests as templates.)
2. `NetworkHealthService` gains a `self_owner: Option<[u8; 16]>` field with
   a `set_self_owner(&mut self, owner: [u8; 16])` setter, following the
   existing `set_*_source` additive-setter pattern (constructor arity
   unchanged). `snapshot` threads `self.self_owner.as_ref()` into the
   filter call at `network_health.rs:1087`.
3. Construction site (`lib.rs:9899`): after `NetworkHealthService::new`,
   `if let Some(o) = guard.dm_self_owner { nh.set_self_owner(o.0); }` —
   `dm_self_owner` is populated at `lib.rs:9557`, before this site, with
   zero new plumbing. `None` (no identity loaded) means no filtering —
   tolerated by construction.
4. GCE suite comment refresh: `scripts/gce-xwan/run-tests.sh:286-295`
   documents the self row as the reason `is_direct_with` is peer-scoped.
   The owner-scoped `select` keeps working; update the comment to note the
   self row is filtered as of ZEB-637 (peer-scoping retained as
   belt-and-braces). Script logic unchanged.

### Tests

1. `filter_peers_drops_self_owner_row` (filter-level, mirrors
   `filter_peers_excludes_peers_with_no_shared_community` at :2598): two
   records sharing a community, one of which is the self owner → only the
   other survives; `self_owner: None` keeps both (no-identity tolerance).
2. Snapshot-level pin: extend/add alongside
   `snapshot_with_three_peers_sorted_by_last_seen_desc` (:3016): a service
   with `set_self_owner` wired and a resolver containing the self record →
   `snap.peers` contains no row whose `owner_addr` equals the self owner
   hex. Existing snapshot tests that don't set a self owner stay untouched
   (Option-gated behavior).

### Non-goals

- No frontend changes (no fixture/UI depends on the self row).
- No resolver-level self-skip (see Decision).
- NAT/relay-RTT enrichment remains iroh-blocked, untouched.

---

## 4. ZEB-625 — kick-vs-floor invariant test pins (test-only)

### Invariant under pin

"A presence kick must never advance the persisted resync floor": in both
`run_backfill_driver` and `run_root_fetch_driver`, the presence-kick arms
(Idle and WaitUntil/mid-backoff) do `latch.reset(..)` only; ONLY the
`resync_tick` arm calls `resync_persist.on_full_reconcile` and advances
`resync_deadline` (backfill :695-697; root :1101-1103). Code-verified,
never test-pinned; the existing suite has floor-fire tests and kick tests
but NONE that combines a presence kick WITH a wired `resync_persist`.

### Tests (all `#[tokio::test(start_paused = true)]`, in
`channel_backfill.rs` `mod tests` at :1183, inline-harness style of
`backfill_persist_floor_first_fire_at_deadline_then_interval` :2663)

1. `backfill_presence_kick_does_not_advance_persisted_floor`
   (`run_backfill_driver`): wire `ResyncPersist { first_deadline_ms:
   DEADLINE }` with a `fired: Arc<StdMutex<Vec<u64>>>` probe AND a presence
   watch. Let the initial request complete; advance past
   `EPOCH_REARM_COOLDOWN_MS`; send a presence kick; assert the kick
   produced a request (`since=None`) and `fired` is STILL EMPTY; advance to
   the original absolute DEADLINE; assert `fired` records exactly one fire
   at ≥ DEADLINE (the kick neither added a fire nor moved the deadline).
2. `root_presence_kick_does_not_advance_persisted_floor`
   (`run_root_fetch_driver`): same shape (template:
   `root_driver_persisted_floor_first_fire_at_deadline` :2868 + kick
   pattern of `root_driver_presence_kick_rearms` :2818).
3. `root_presence_kick_mid_backoff_rearms` (`run_root_fetch_driver`
   WaitUntil arm :1146-1162, currently covered only by
   mirror-faithfulness): make `request_root` fail so the latch backs off
   into WaitUntil; send a presence kick mid-backoff (past cooldown); assert
   a new root request fires without waiting out the backoff target.

Timing discipline per the wall-clock-budget rule: paused time only,
absolute-deadline asserts, no real-time sleeps.

### Scope change vs ticket: `mark_unchanged` polish DROPPED

The ticket's optional polish ("`mark_unchanged()` on the presence receiver
clone handed to a late-spawned root driver, for parity with the
transport-epoch receivers") rests on a refuted premise: `mark_unchanged`
appears NOWHERE in the tree; the transport-epoch receivers are handed over
by plain clone with no freshness-marking (community_state_sync.rs:4946 vs
:4950 — identical idiom). There is no parity precedent to restore, the
behavior is bounded + cooldown-gated (ticket's own words), and both drivers
issue their initial request from the latch before awaiting any bump. To be
noted on the Linear ticket at PR time.

---

## Gates (per CLAUDE.md + standing rules)

- Per-task iterative: `scripts/test-select --context task` from repo root;
  scoped clippy + `cargo fmt --all` from `src-tauri/`.
- Final sweep: `cargo fmt --all -- --check`, CI-form clippy
  (`--locked --all-targets --features test-fixtures --no-deps -- -D warnings`),
  full `cargo nextest run --locked --workspace --all-targets --features
  test-fixtures`. No frontend changes expected → tsc/vitest only if a TS
  file is touched (none planned).
- One commit per task; commit-before-gate.
