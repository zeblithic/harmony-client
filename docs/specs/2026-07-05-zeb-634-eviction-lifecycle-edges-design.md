# ZEB-634: supervisor eviction lifecycle edges — design

Close the three residuals from the ZEB-627 hardening-bundle final review
(2026-07-04): the depart-while-connected Dormant leak, community-blind
eviction, and the record-less-peer `n > 0` gate. All three are lifecycle
edges around `SupervisorHandle::evict_peer` (PR #397).

## Item 1 + 3: record-less `Dropped` gate in `apply_trigger`

The ticket's fix shape (a), extended to existing slots so it genuinely
subsumes item 3.

**Mechanism.** In `reconnect_supervisor.rs::apply_trigger`, a `Dropped`
trigger for a peer with **no live resolver record**
(`resolver.resolve_by_node_id(&peer).is_none()` — the resolver is already a
parameter, used for ring markers) is a departure signal, not a reconnect
signal:

- **Unknown peer (None arm):** decline to create a slot. This is the
  headline leak — after a membership eviction, the departing conn's
  drop-watcher fires `Dropped`; today that recreates a fresh Retrying slot
  which resolve-misses ladder to Dormant, where it parks for process life
  (~one leaked slot per departed-while-connected peer).
- **Existing slot (Some arm):** remove the slot entirely instead of
  re-arming it. This closes item 3: an inbound-conn-only peer (slot created
  by `mark_connected`, zero resolver records, so the membership-eviction
  path can never name its node-id) is cleaned at conn-drop time — the only
  causally-available moment. It also covers a `Dropped` that lands between
  `remove_owner` and `evict_peer`.

**Why record-less ⇒ remove is safe.** A record-less peer is undialable
anyway (the dispatch pass soft-fails on resolve-miss and ladders to
Dormant — the slot is pure dead weight). Every legitimate future-interest
path recreates the slot: a `ReachabilityAnnounce` record-add kicks
`NewPeer`/`RecordChanged` (both fire only AFTER the resolver write, so the
gate can never race them into a false decline), and a live inbound accept
goes through `mark_connected`. `PresenceSweep` is never kicked per-peer
today, and `do_sweep` only re-arms existing slots.

**Gate scope: `Dropped` only.** `NewPeer`/`RecordChanged` kicks are
record-backed by construction; gating them would add a resolver probe per
kick for nothing. The ticket's option (b) — a periodic record-less-Dormant
sweep — is not needed once creation and re-arm are both gated (YAGNI).

**Accepted losses.**
- Slot removal drops `ever_connected`; a departed peer that later re-joins
  gets a first-connect (no `reconnected` marker). Correct semantics — a
  re-onboard is not a recovery.
- A *spurious* `Dropped` (transport delete while the conn is actually
  alive) on a record-less Connected peer removes the slot, so the panel
  undercounts Connected until the next real edge. Today's behavior for the
  same input is worse (slot ladders to a false Dormant); removal is no less
  accurate and costs nothing.
- Removal while `dial_in_flight` is safe: `apply_result` already returns on
  a missing slot, and the dial task's permit frees when the task ends.

## Item 2: membership consult before Leave/Kick eviction

**Mechanism.** New synchronous method on
`network_health.rs::MembershipProjection`:

```rust
/// True if `peer` is a Joined member of any community OTHER than
/// `excluding` that the local node is Joined in. The Leave/Kick arms use
/// it to skip reachability eviction for a peer who is still a co-member
/// elsewhere (ZEB-634 item 2).
pub fn is_joined_elsewhere(&self, peer: &[u8; 16], excluding: &SpaceId) -> bool
```

In the lib.rs membership-consumer Leave and Kick arms (~:5916/:5959):
before the capture/remove/evict block, consult
`membership_projection.is_joined_elsewhere(&addr.0, &community_id)` (Leave:
`addr` = `event.actor`; Kick: `addr` = `target`). If true → skip
`resolver.remove_owner`, the `evict_peer` loop, the Network Health notify,
and the `emit_changed` (nothing about reachability changed). The
per-community `community_relay_resolver.remove_advertiser` stays
unconditional — it is already community-scoped and correct.

**Why the projection and not resolver refcounts.** The projection (ZEB-329)
already maintains exactly the per-community joined-member sets, fed on
every delta and boot-replayed, readable synchronously. Refcounting inside
the resolver would duplicate membership state that can then drift; the
consult delivers the ticket's "per-community membership refcounting"
semantics with zero new state.

**Ordering.** For the SAME delta, the consumer's projection update runs
AFTER the eviction arm, so the departing community's set is stale
(pre-Leave) at consult time — hence the explicit `excluding` parameter
rather than relying on the leaver having been removed from that set.
Sequential Leaves across communities converge correctly in either order:
each community's projection entry is refreshed by its own delta before the
peer's LAST departure is processed, so the final Leave sees no other shared
community and evicts. (Deltas flow through one consumer; a pathological
concurrent interleave could at worst skip an eviction, leaking one slot for
a doubly-departed peer until process end — strictly better than today's
guaranteed leak, and item 1's gate stops the re-creation half.)

**Scope note (adjacent, unchanged).** A *friend* (DM peer) whose last
shared community departs still loses reachability records — pre-existing
behavior, untouched here; DM messaging degrades to the deposit path. If
live-tunnel-after-community-departure matters, that is a friend-graph
consult in the same seam — out of scope for ZEB-634.

## Doc updates

- `evict_peer`'s "KNOWN RESIDUAL … tracked as ZEB-634" paragraph rewritten
  to describe the closed lifecycle (creation-gated + membership consult).

## Tests

Supervisor (`reconnect_supervisor.rs` unit tests, existing harness —
`RecordingDialer`, `seed()`, paused clock):

1. `dropped_kick_recordless_unknown_creates_no_slot` — the headline
   sequence: seed + connect → `remove_owner` + `evict_peer` → `Dropped`
   kick → snapshot has NO entry for the peer (today: leaked Dormant slot).
2. `dropped_kick_with_record_still_creates_slot` — non-regression pin:
   `Dropped` for an unknown peer WITH a record arms Retrying at base.
3. `dropped_kick_recordless_removes_existing_slot` — `mark_connected`
   (inbound-only, no record) → `Dropped` kick → slot removed, not laddered;
   a later `seed()` + `NewPeer` kick recreates it at base (the revival
   path).
4. `recordless_drop_then_reannounce_recreates` — folded into (3)'s tail.

Projection (`network_health.rs` unit tests):

5. `is_joined_elsewhere` matrix — peer in A+B excluding A → true; peer only
   in A excluding A → false; unknown peer → false; empty projection →
   false.

Hook-logic mirrors (lib.rs test mod, same style as
`leave_delta_evicts_resolver_entries`):

6. Leave for a peer joined elsewhere → skip branch taken (records survive).
7. Leave for a peer's last shared community → evict branch taken (records
   removed) — pins that the consult does not over-protect.

## Non-goals

CI/vitest/frontend untouched (no DTO or UI change; `states_snapshot`
shrinks but its shape is unchanged). No periodic sweep. No friend-graph
protection (adjacent-scope note above). No change to the `n > 0` gate
itself — with item 1's Some-arm removal, the record-less population it
can't reach is cleaned at conn-drop instead.
