# ZEB-815: Community Address Book — Design

**Ticket:** ZEB-815 — Move Reachability/CommunityRelay announces out of the membership event log into a bounded community address book.
**Approved by:** Jake, 2026-07-26 (brainstorm session; Approach 1 of 3).
**Sibling spec:** `2026-07-26-zeb-811-vine-relay-fanout-design.md` (Spec B). The two share one principle and no code dependency; this spec (A) ships first.

## The principle (shared with Spec B)

*Community-scoped routing data lives in a per-community address book; owner-scoped
public routing data lives in pkarr.* Routing data is current-state with a TTL, not
history. It does not belong in a permanent membership log — ZEB-813 was that tension
detonating (announces were 94% of the fleet community's CRDT log and pushed the root
blob over the 1 MiB ContentId cap, killing publish and serve fleet-wide for ~42 h).
ZEB-813's supersession compaction bounds the log; this design removes the category
error.

## Goal

Reachability and community-relay announces move to a bounded, sealed, per-community
**address book** with its own sync surface. The membership `VerifiedLog` stops
carrying routing data entirely and shrinks to true community history. Consumer
contracts (dial supervisor, deposit ordering, relay pull) are unchanged.

## Non-goals

- No change to announce payload *contents* (`ReachabilityAnnouncePayload`,
  `CommunityRelayAnnouncePayload` keep their wire structs and identity signatures).
- No change to the fleet-sibling or pkarr resolver feeds (only the community-CRDT
  feed is replaced).
- No RBSR/set-reconciliation machinery — the book is small by construction; full
  snapshots are the catch-up mechanism.
- No history rewrite: existing announce events in old logs stay decodable forever.

## 1. Data model

Per-community store, keyed exactly like ZEB-813's supersession keys (deliberate
continuity — same identity spaces, same discriminators):

```rust
enum AddressBookKey {
    /// One row per (member, iroh node): current dial info for that node.
    Reachability(OwnerAddr, [u8; 32] /* iroh_node_id */),
    /// One row per (advertiser, relay device): current relay ad.
    Relay(OwnerAddr, [u8; 16] /* relay_device_id */),
}

struct AddressBookRow {
    payload: AddressBookPayload,   // Reachability(..) | Relay(..) — existing structs
    stamped_at_ms: u64,            // announced_at_ms / ad_at, skew-clamped at ingest
}
```

- **LWW per key** by the payload's own timestamp (`announced_at_ms` / `ad_at`),
  clamped to `now + FUTURE_SKEW_TOLERANCE_MS` (5 min — the resolver's existing
  constant) at ingest. Older/equal stamps are ignored.
- **Verification at ingest** (mirrors presence-beacon gating): payload identity
  signature must verify AND the signer must be a materialized **Joined** member of
  the community (`beacon_signer_is_member` pattern). Non-member records are
  rejected, not stored.
- **Bounds** (storage-enforced, not read-time-only):
  - `ADDRBOOK_MAX_NODES_PER_MEMBER = 8` — oldest-stamp eviction beyond it.
  - `ADDRBOOK_MAX_ROWS = 4096` hard cap per community (expected occupancy is
    `members × nodes-per-member + relay ads`, far below it; the cap guards
    against a hostile flood that slips past the membership gate).
  - Relay read-cap unchanged: `COMMUNITY_RELAY_ADVERTISERS_MAX = 4` stays applied
    on read in `CommunityRelayResolver`.
- **TTL at storage:** rows whose stamp is older than their class TTL
  (reachability 24 h, relay ad `COMMUNITY_RELAY_AD_FRESHNESS_MS` = 15 min) are
  dropped at load and swept opportunistically on write. Freshness filtering on
  read in the resolvers is unchanged (defense in depth, and read-side semantics
  stay identical).

## 2. Sync surface

Two paths, both bounded. No barrier between them — live pub heals what snapshot
missed and vice versa.

**Live:** each record is published (sealed) on a new zenoh topic

```
harmony/addrbook/{community_id_hex}/records
```

sealed with a membership-derived key: `derive_addrbook_key(membership_key,
community_id)` — the same derivation pattern as presence beacons, so epoch
rotation applies automatically and non-members cannot read routing data.

**Catch-up:** a snapshot queryable

```
harmony/addrbook/{community_id_hex}/snapshot
```

returns the responder's full book for that community (sealed, same key). Fired:
on join (first connect), on reconnect, and on presence roster change — with a
per-community cooldown of 60 s (reuse `EPOCH_REARM_COOLDOWN_MS`'s value; separate
constant `ADDRBOOK_SNAPSHOT_COOLDOWN_MS`). Snapshot responses go through the same
per-record ingest gate as live records (signature + membership + LWW), so a
malicious or stale responder can only contribute rows that verify individually.

**Persistence:** `addrbook.cbor` sidecar per community, next to `crdt.cbor`
(same directory), written debounced after ingest batches, loaded at boot with TTL
filtering. Loss of the file is safe: the book refills from live pubs + snapshot.

## 3. Publisher swap (flag-day)

`ReachabilityPublisher` (and the community-relay publisher) keep **all** existing
triggers — startup immediate, network-change with 2 s debounce, 60 min idle
backstop, force-notify — but their `PublishFn` changes from *"sign a
`ReachabilityAnnounce` membership event into each joined community's CRDT"* to
*"upsert into the local address book + publish the sealed record on the topic"*.

- `MembershipEventKind::ReachabilityAnnounce` / `CommunityRelayAnnounce` are **no
  longer minted and no longer consumed**. The enum variants and their decode/
  materialize (no-op) paths remain forever — old logs must stay verifiable, and
  ZEB-813's supersession keeps them compacted.
- **Fleet flag-day** (like the iroh 1.0 wire flip): we control all nodes; no
  dual-write window. An old node paired with a new node degrades softly — the old
  node's announces still replicate via CRDT but the new node ignores them, and
  vice versa; peers converge when both rebuild. Acceptable for a fleet this size;
  called out in the rollout checklist.
- The ZEB-813 watermark stack (`RootSizeWatermark`, near-cap telemetry) **stays**
  as a regression guard on the now-small log.

## 4. Consumers and bootstrap

Address-book ingest calls the exact functions the CRDT membership-delta hook
calls today:

- `ReachabilityResolver::update(actor, payload, hlc)` — preserving first-learn →
  `NewPeer` and changed-addressing → `RecordChanged` supervisor kicks. The dial
  supervisor is untouched.
- `CommunityRelayResolver::update(community, advertiser, payload, hlc)` — read
  semantics (freshness, cap, deliberately-unfiltered self entries for ZEB-524
  deposit ordering and the ZEB-806 local self-drain) untouched.
- Boot: resolvers seed from the persisted `addrbook.cbor` instead of CRDT replay.
  The CRDT replay hook for announce events is removed.

**New-joiner bootstrap improves:** today a joiner learns who-to-dial from the
community root blob — the exact object that hit ZEB-813's 1 MiB cliff. After this
change: pkarr resolves the inviter (exists today, Case A) → first connect →
snapshot query fills the book (KB-scale) → resolver kicks → dials. Routing
bootstrap no longer depends on community-state root publish at all.

## 5. Eviction

- **Kick/leave of a member:** drop all rows keyed by that member's `OwnerAddr`,
  at the same membership-delta consumer that today feeds the resolver (it already
  observes kicks/leaves). Also call `CommunityRelayResolver::remove_advertiser`
  (exists) and the equivalent reachability removal so in-memory views match.
- **Own leave/kick:** drop the whole community's book + sidecar.
- **Epoch rotation:** nothing special — the seal key derivation rotates with the
  membership key exactly as presence does; old-epoch records already ingested
  remain valid rows (they were verified at ingest).

## 6. Failure modes

- **Snapshot no-responder** (solo node, first of a fleet to rebuild): retry with
  backoff; live pubs still fill the book; not an error state. (ZEB-813's "no
  responder" lesson: this line must be INFO-with-context, not silent, and not a
  WARN that trains operators to ignore it.)
- **Hostile/stale snapshot responder:** can only contribute individually-verified,
  membership-gated, LWW-checked rows. Worst case is withholding (same as no
  responder).
- **Clock skew:** stamp clamped at ingest (5 min tolerance); a wildly-future stamp
  cannot pin a row unevictably.
- **Record flood:** membership gate first, then per-member node cap, then the
  4096-row hard cap. A hostile *member* can at most churn their own 8 rows.

## 7. Testing

- **Unit:** LWW ordering incl. skew clamp; TTL load-filter + sweep; per-member cap
  eviction; kick/leave eviction; seal/unseal round-trip incl. epoch rotation;
  snapshot ingest = live ingest (same gate).
- **Integration:** publisher swap (triggers produce book rows + topic pubs, zero
  membership events minted); resolver re-feed parity (same kicks as the old CRDT
  hook — assert against the supervisor's coalesced trigger map); boot from
  sidecar; join → snapshot → dial chain.
- **e2e (two-node):** join a community on the new build and assert (a) the member
  dials its peer with **zero** announce events in either membership log, and
  (b) a kick evicts the kicked member's rows from the survivor's book.
- **Fleet validation:** post-deploy, assert membership log growth ≈ 0 events/day
  on the fleet community (vs ~28–48/day pre-change) and root blob size stays flat.

## Rollout

1. Land core changes (if any turn out to be needed in `harmony` — expected: none;
   the book is client-side) and client PR behind the normal gates.
2. Fleet flag-day: rebuild Koya/Ildwyn/AVALON nodes together (coordination post on
   the fleet board, same protocol as the iroh 1.0 flip).
3. Post-deploy verification on the fleet community per §7's fleet validation.
4. ZEB-814 (root chunking) is *relieved* but not closed by this — the membership
   log still has a 1 MiB root; it just stops growing per-routing-refresh.
