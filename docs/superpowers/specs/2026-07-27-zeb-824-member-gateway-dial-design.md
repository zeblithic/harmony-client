# ZEB-824: Member rendezvous-beacon dial ("gateway dial") — design

**Ticket:** ZEB-824 (Urgent) — post-ZEB-815 flag-day, existing members have no session
bootstrap: the dial supervisor idles on an empty address book and the fleet partitions with
zero dial attempts.

**Decision of record (Jake, 2026-07-27, on the ticket):** fix direction 1 — member gateway
dial. Members resolve the community pkarr record and dial the way open-join does, so session
bootstrap never presupposes announce history. Directions 2 (persisted dial cache) and
3 (addrbook snapshot over iroh) were explicitly not chosen.

**Design decisions from the live session (Jake, 2026-07-27):** zero-session recovery (a
standing per-community self-healing loop, not a boot-only escape hatch), per-community
member-aware starved predicate, Approach 1 (feeder driver into the existing session
machinery — no new wire surface).

## 1. The deadlock this closes

- ZEB-809 disabled zenoh LAN scouting in production (`event_loop.rs:1317` opt-in
  `HARMONY_ZENOH_ENABLE_LAN_SCOUTING=1`): sessions come only from routing-record → iroh dial.
- ZEB-815 (flag-day, no dual-write) moved Reachability/CommunityRelay announces out of the
  membership event log into the per-community address book. The boot seed for the dial path
  is now: `addrbook.cbor` sidecar → `ReachabilityResolver` (`lib.rs:8404-8536`) →
  `seed_boot_peers_into_supervisor` (`event_loop.rs:1548`,
  `iroh_zenoh_registration.rs:134`). A rebuilt node has no sidecar, so the resolver is
  empty and zero peers are kicked.
- The reconnect supervisor is record-gated: a dial dispatch requires
  `resolver.resolve_by_node_id(peer)` to return `Some` (`reconnect_supervisor.rs:551`);
  unknown peers never dial.
- The address book fills only over zenoh — live subscriber
  (`address_book_sync.rs:615`) and snapshot GET (`address_book_sync.rs:910`, remote-only) —
  both of which require the session the node is trying to bootstrap.

No sessions → empty addrbook → zero candidates → zero dials → no sessions. The fix inserts
one session-independent candidate source: the community rendezvous slots on pkarr.

## 2. Shape of the fix

One new module, `src-tauri/src/community_gateway_dial_driver.rs`. It is a **feeder, not a
dialer**: each pass it finds starved communities, resolves the community's rendezvous
beacon from pkarr, verifies it, seeds it into the `ReachabilityResolver`, and kicks the
reconnect supervisor. Everything downstream — the record-gated dial, the zenoh session, the
addrbook subscriber and snapshot query, state sync — is existing machinery and unchanged.

Explicitly rejected shapes:

- **Reusing the open-join handshake end-to-end.** The `harmony/handshake/v1` dial is a
  one-shot app-level stream that returns a membership snapshot and closes
  (`open_join_dial.rs:97-369`); it never creates a zenoh session, and its admission path
  hard-rejects invite-only communities (`iroh_invite_acceptor.rs:540`). Members need the
  session, not the snapshot; reusing the handshake would require a new packet type and
  acceptor changes for nothing the session doesn't already deliver.
- **Boot-only seeding.** Rejected by the trigger-scope decision: it cannot heal a node that
  loses all peers mid-run.

### Terminology

The code has no "gateway" role (that word means the mail/zenoh gateway in this repo). The
dialable entity is the **rendezvous beacon**: a community relay volunteer
(`RelayOptInDoc::is_opted_in`, `community_relay_optin.rs:44`) that is a Joined member and
ranks at slot index < 4 in the sorted advertiser set (`slot_for_advertiser`,
`community_rendezvous.rs:67`). Beacons publish their own iroh reachability into 4 enumerated
pkarr slots keyed only by the community epoch key
(`community_rendezvous_publisher.rs:158 refresh_slot`, cadence ~7.5 min via the relay
publisher loop at `lib.rs:11290-11303`). "Gateway dial" is the ticket's alias; the spec and
code use *beacon*.

### Which pkarr record (premise correction vs the ticket)

The ticket's "Case C" phrasing is ambiguous. The member-keyed Case-C slots
(`info = identity_pub(64) ‖ epoch_id(8)`, `pkarr_resolver_adapter.rs:100-103`) require the
target member's 64-byte identity pub to derive the slot key — exactly what an empty address
book denies. The **rendezvous slot family**
(`info = "harmony.rendezvous.v1" ‖ slot_index_be(2) ‖ epoch_id_be(8)`,
`community_rendezvous.rs:36`) is keyed only by the epoch key and is the record open-join
already dials. This design uses the rendezvous family exclusively. Its routing blob is a
`ReachabilityAnnouncePayload` (iroh node id, home relay URL, direct addresses) — dialable
as-is via the existing machinery.

## 3. Driver architecture

Spawn shape: byte-for-byte the `community_relay_pull_driver.rs:502` /
`vine_pull_driver.rs:837` pattern — `Arc<Self>::spawn() -> JoinHandle<()>` running an
immediate `run_one_pass(now_ms)`, then `interval` with `MissedTickBehavior::Skip` +
`select!` on a `Notify` wake handle. Spawned from `start_node_inner` via `tokio::spawn`;
handle stored on `NodeState` and aborted on node stop (the
`community_relay_refresher_handle_opt` precedent, `lib.rs:13058`).

**Boot-hazard compliance:** the driver's awaits touch pkarr HTTP, engine state mutexes, and
sync resolver/supervisor methods only — never an event-loop channel — so the start_node
inline-await hazard (`lib.rs:6094-6117` canonical note) does not apply.

Constructor inputs:

| input | source |
|---|---|
| joined-communities snapshot | the existing `JoinedCommunitiesFn` seam (`community_relay_pull_driver.rs:184`); share the 60 s refresher snapshot at `lib.rs:10852` |
| engine access (membership key, materialized members, admin addr) | `CommunitySyncRegistry` (`community_state_sync.rs:5642 known_ids` and per-id engine lookup) |
| pkarr resolver | `Arc<harmony_pkarr::PkarrResolver>` (same Arc open-join uses) |
| reachability resolver + supervisor | `Arc<ReachabilityResolver>`; supervisor via `ReachabilityResolver::supervisor()` (`reachability_resolver.rs:363`) |
| self iroh endpoint id | `[u8; 32]`, for the self-filter (vine-driver shape, `vine_pull_driver.rs:575`) |
| self owner addr | `OwnerAddr`, secondary self-guard + solo-community check |
| telemetry | new `GatewayBootstrapTelemetry` (see §7) |
| now_ms | injected clock fn, test seam (established driver pattern) |

## 4. Starved predicate (per community)

Community X is **starved** iff both:

1. X has at least one Joined member other than self (from the engine's locally materialized
   membership — persisted CRDT state, survives rebuilds; pattern at
   `pkarr_resolver_adapter.rs:200-215`), and
2. no member of X currently maps to a `Connected` supervisor peer: member `OwnerAddr` →
   `ReachabilityResolver` entry → iroh node id → supervisor peer state, checked against a
   supervisor states snapshot (the `ProdSupervisorSnapshot` seam used by
   `network_health`, `lib.rs:12568`).

Properties this buys:

- Empty resolver ⇒ no member maps to anything ⇒ trivially starved (the flag-day shape).
- Stale addrbook rows whose dials all fail ⇒ still starved ⇒ the beacon record (refreshed
  every ~7.5 min, vs addrbook rows up to 24 h stale) is a genuinely fresher escape hatch.
- Sessions to *other* communities never mask X (zenoh does not route X's queries through
  non-members).
- Solo communities are never starved — no pkarr traffic for them.

A community that is not starved contributes zero IO to the pass (the predicate reads only
in-memory state).

## 5. The bootstrap pass

For each joined community, in `run_one_pass(now_ms)`:

1. Skip if not starved, resetting that community's backoff ladder if it was previously
   starved (healed).
2. Skip if starved but the per-community ladder says the next resolve attempt is not yet
   due.
3. **Resolve.** `resolve_rendezvous`-equivalent call with a new client-side decode closure
   (see below), epoch key = **`engine.membership_key()`** — this MUST match the rendezvous
   publisher, which keys slots on `membership_key()` at `lib.rs:11298`. Do not use
   `live_epoch_key` here: ZEB-597 moved only the member-keyed Case-C publisher to the live
   key; the rendezvous publisher stayed on the spawn-time key, and resolving under a
   different key than the publisher derives different slot keypairs and misses every
   record. Reuse `rendezvous_config_from_env()` (`community_rendezvous.rs:79`) unchanged.
4. **No beacon** (all slots empty, unverifiable, or self — see the decode closure): record
   the outcome, advance the ladder, done for this community.
5. **Beacon found** `(payload, beacon_identity_pub)`:
   a. Derive the beacon's owner address:
      `harmony_identity::Identity::from_public_bytes(beacon_identity_pub).address_hash`
      (the `community_invite.rs:1934` pattern).
   b. Secondary self-guard: if the derived owner addr equals our own actor, skip (the
      iroh-node-id filter in the decode closure is primary; this catches a same-owner
      sibling device record, which is a candidate the fleet-sibling seed path already
      covers).
   c. **Membership gate:** require the derived owner to be a Joined member of X in the
      materialized membership. The record already proves its writer holds the epoch key
      (outer BEP44 sig) *and* the claimed identity's signing key (inner sig,
      `record.rs:92 verify_inner_sig` runs inside `PkarrResolver::resolve`); this gate
      additionally ensures a leaked epoch key cannot steer our dials to an attacker
      endpoint claiming a non-member identity. Rejection: telemetry + ladder advance, no
      seed.
   d. **Seed:** `reachability_resolver.seed_from_pkarr(owner_addr,
      DeviceIdentityHash([0u8; 16]), payload)` — the zero device-hash placeholder is the
      invite-path precedent (`lib.rs:59154`).
   e. **Kick:** `supervisor.kick(payload.iroh_node_id, ReconnectTrigger::NewPeer)`
      explicitly. If the seed already auto-kicked via the resolver's update gate
      (`reachability_resolver.rs:475-486`), the second kick coalesces harmlessly
      (`SupervisorHandle::kick` is a lossless coalescing dirty-set insert,
      `reconnect_supervisor.rs:260`). If `ReachabilityResolver::supervisor()` returns
      `None` (pre-install race, §6), skip the kick — the seed still landed, and the pass
      retries next tick.

The pass then ends; the reconnect supervisor owns the dial (record gate now passes), the
session comes up, the addrbook subscriber and snapshot requester find a responder, rows
flow, the predicate turns healthy, and the driver goes quiet for that community.

### The decode closure (client-side only, no core-crate change)

`resolve_rendezvous` currently decodes with
`|blob| ciborium::from_reader::<ReachabilityAnnouncePayload, _>(blob).ok()`
(`community_rendezvous.rs:125`), discarding the outer `PkarrRoutingRecord` — and with it
the `harmony_identity_pub` this design needs. The core driver
(`harmony-pkarr/src/rendezvous.rs:96 resolve_rendezvous_with`) is generic over the payload
type, so the client adds a sibling entry point (e.g.
`resolve_rendezvous_identified`) in `community_rendezvous.rs` whose closure:

- decodes the payload **and** captures the outer record's `harmony_identity_pub`, yielding
  `(ReachabilityAnnouncePayload, [u8; 64])`;
- returns `None` when `payload.iroh_node_id == self_endpoint_id` — the **primary
  self-filter**. Placing it inside the closure means a self-owned slot reads as *empty* to
  the escalating-batch driver ([1, 2, 4] batches, first-responder-wins), which then
  naturally falls through to the other three slots. This closes the self-dial hazard that
  open-join never had (a joiner is by definition not a beacon) — the ZEB-806 /
  `self_relay_entry_is_never_dialed` lesson applied at the resolve layer.

The open-join call site keeps the existing unidentified variant; its trust argument
(`community_rendezvous.rs:108-111`: identity binding deferred to admission) is unchanged.
Our variant does not add `verify_identity_match` either — there is no *expected* identity
for a rendezvous slot — but the inner-sig + membership gate in §5 gives the member-side
equivalent.

**As implemented (Task 1).** Same architecture — client-only, no core-crate change — but a
different mechanism than "a different closure". The core `PkarrSlotResolver`'s decode
closure receives only the routing *blob*, so it can never see the outer record's
`harmony_identity_pub` no matter what closure is passed. The identity-preserving variant
therefore shipped as a client-side `SlotResolver` **impl**: `IdentifiedSlotResolver` in
`community_rendezvous.rs`, which mirrors the core probe (same slot keys, same batch
escalation, same freshness window) and applies the self-filter itself, yielding
`IdentifiedBeacon { payload, beacon_identity_pub }`. The entry point is
`resolve_rendezvous_identified`, and the self-filter still lives at the decode step, so a
self-owned slot reads as empty and the escalating driver widens past it exactly as
described above.

## 6. Failure modes and edges

- **No beacon resolves, or the only beacon is us.** Stay on the ladder, quietly. If we are
  the only live beacon we are reachable and peers dial *us*; retrying is correct and cheap.
  A resolve attempt is bounded (~7.5 s worst case: three batches × 2.5 s per-batch
  deadline).
- **Non-member identity in a beacon record.** Rejected by the membership gate; telemetry;
  ladder advances. The next attempt re-resolves — first-responder-wins may surface a
  different slot.
- **Supervisor not yet installed.** The driver spawns from `start_node_inner`; the
  supervisor handle is installed by the event loop at `event_loop.rs:1536`. A first pass
  racing that install seeds the resolver and skips the kick; the event loop's own boot seed
  (`event_loop.rs:1548`) runs after install, reads the resolver, and kicks the seeded peer
  itself. Benign in both interleavings.
- **Registry has no engine for a listed community.** Skip with a debug log (transient
  registration gap).
- **Resolve error / relay timeout.** Treated as no-beacon; ladder advances.
- **Epoch-key mismatch after a mid-run rotation on a long-lived beacon.** Pre-existing wart
  (§9); the miss reads as no-beacon and the ladder keeps retrying — heals when either side
  restarts.

## 7. Cadence, backoff, observability

- **Tick:** 30 s interval; predicate-only (no IO) unless a community is starved and due.
  Startup pass runs immediately at spawn, so a boot-starved node attempts its first beacon
  resolve within seconds of boot (fleet evidence: heal ~26 s once candidates existed).
- **Per-community resolve ladder while starved:** 30 s base, doubling, 600 s cap — the
  `channel_backfill.rs` constants shape (`BACKFILL_RETRY_BASE_MS` / `BACKFILL_RETRY_CAP_MS`
  precedent). Reset to base on starved→healthy transition.
- **Telemetry** (`GatewayBootstrapTelemetry`, the ZEB-803 lesson — alive-but-idle must be
  distinguishable from dead): a pass counter incremented **before** the joined-set read
  (`community_relay_pull_driver.rs:288-292` precedent); per-community last outcome with
  timestamps. As shipped the outcome vocabulary is the seven wire strings
  `healthy | starvedWaiting | noBeacon | beaconSeeded | rejectedNonMember | soloCommunity |
  engineUnregistered` — there is no separate `resolving` state (a resolve is synchronous
  within the pass and lands on its terminal outcome), and the two not-actionable skips
  (`soloCommunity`, `engineUnregistered`) are named so absence from `perCommunity` keeps
  meaning "never evaluated" rather than "fine". INFO logs on state transitions (starved
  detected, beacon seeded, healed); steady-state skips are recorded in telemetry rather
  than logged, the one exception being the engine-unregistered skip, which carries a DEBUG
  log.
- **Surface:** a `gatewayBootstrap` block in `network_health_snapshot` (per-community
  outcome + pass counter + last-attempt age), so a fleet node is diagnosable without log
  access (the ZEB-804 lesson). Serde camelCase keys as usual for the DTO.

## 8. Testing

Unit (driver module, mock resolver/supervisor seams — the reconnect supervisor's
`PeerDialer` trait and injected-clock patterns already exist):

- Predicate: empty resolver ⇒ starved; Connected member ⇒ healthy; Connected **non**-member
  does not mask starvation; solo community never starved; healed community resets its
  ladder.
- Decode closure: self record filtered ⇒ batch escalation reaches a later slot's record
  (reuse the mock-relay publish helper from
  `tests/misc/community_open_join_cross_wan_integration.rs:674`); identity captured
  alongside payload.
- Membership gate: beacon record with a non-member identity is not seeded and telemetry
  records the rejection.
- Ladder: repeated no-beacon passes space attempts 30 s → 60 s → … → 600 s cap (logical
  time / injected clock — no wall-clock budgets).
- Seed+kick: a starved pass with a valid member beacon calls `seed_from_pkarr` once and
  kicks `NewPeer` for the beacon's node id; a pass with no supervisor installed still
  seeds.

Integration (headline scenario, in-process, mock pkarr relay): member node with empty
addrbook and scouting off + a beacon record published under the community epoch key ⇒ one
`run_one_pass` seeds the resolver and the supervisor dirty-set contains the beacon node id
with `NewPeer`.

Live verification (post-merge, not CI): fleet cross-machine heal with
`HARMONY_ZENOH_ENABLE_LAN_SCOUTING` unset — the ZEB-815-style operational check, evidence
posted to the ticket.

Gates: module suites via
`cargo nextest run --locked --features test-fixtures -E 'test(community_gateway_dial_driver)'`
(plus touched-module suites), `cargo fmt --all -- --check`, CI-exact
`cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`.

## 9. Boundaries (documented, deliberately out of scope)

- **Gateway-offline bootstrap** (zero live beacons anywhere): deferred per the decision of
  record — the relay-volunteer set / `should_self_promote` driver
  (`community_rendezvous_publisher.rs:268`, deliberately unbuilt) covers it later. Note
  the window is narrow: beacons keep publishing while partitioned (each volunteer
  self-ingests its own relay announce via `publish_own_rows`
  (`address_book_sync.rs:519`), sees itself as a rank-0 advertiser, and refreshes its slot
  every ~7.5 min), so "nothing to resolve" requires every volunteer down for the full
  record TTL (7 days).
- **Rendezvous publisher epoch-key wart:** publisher keys on spawn-time
  `membership_key()`; ZEB-597 moved only the member-keyed publisher to the live key. This
  design *matches the publisher* rather than fixing the asymmetry; a mid-run epoch
  rotation on a long-lived beacon strands its slots until restart. Pre-existing, filed
  thinking only — if it bites, it is its own ticket.
- **`DeviceIdentityHash([0u8;16])` placeholder** on seeded entries — invite-path parity,
  not new debt.
- **DM / friend-path resolution** untouched; the member-keyed Case-C machinery untouched.
- **The LAN-scouting flag** (`HARMONY_ZENOH_ENABLE_LAN_SCOUTING=1`) remains the
  operational fallback of last resort and is unchanged by this design.
