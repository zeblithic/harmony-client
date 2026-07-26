# ZEB-811: Vine Relay Fan-out — Design

**Ticket:** ZEB-811 — Following a vine creator has no transport consequence; cross-WAN follow-only delivery has never worked.
**Approved by:** Jake, 2026-07-26 (brainstorm session; Approach 1 of 3, with three product calls recorded below).
**Sibling spec:** `2026-07-26-zeb-815-community-address-book-design.md` (Spec A). Shared principle, zero code dependency; A ships first, B follows or overlaps.

## Product decisions (Jake, 2026-07-26)

1. **Relay-mediated fan-out** — a follow does not open a connection to the
   creator; followers pull from relays.
2. **Public-read** — vines are public content; relays serve descriptors and video
   to anonymous pulls, no auth. Moderation/visibility stays a feed-level concern.
3. **Own devices now, protocol takes N** — v1 relay set = the creator's own
   always-on devices; the ad and pull protocol are provenance-agnostic lists so
   friend/community volunteers are a later list-append, not a protocol change.

### The v1 privacy caveat (signed off explicitly)

With v1 relays being the creator's own devices, a follower's pull lands on
creator hardware. The pull request carries **no requester identity** and iroh
node ids are pseudonymous — the creator observes that *some node* pulled and can
map node→person only if they already share a community/friendship with that
node. v1 privacy is therefore **pseudonymous, not unlinkable**. Full
unlinkability arrives exactly when third-party volunteer relays exist (deferred,
§6). This spec states the property honestly rather than implying the stronger
one.

## The gap being closed (from the ZEB-811 investigation)

- Vine descriptors are wildcard zenoh pub/sub (`harmony/vines/{creator}` →
  `harmony/vines/*`) over whatever peer mesh already exists; video bytes are
  zenoh CAS GETs (`harmony/content/{shard}/{cid}`) over the same mesh.
- A follow is a literal event-loop no-op (`FollowRequest::Follow => {}`) — it
  builds no network path. `vine_follow_graph.rs` is *content* reachability (pure
  graph math), unrelated to *network* reachability.
- The resolver's pkarr fallback requires a shared community context, so even
  `resolve_async` cannot acquire a followed-only creator.
- Net: vines reach only peers you're connected to for an unrelated reason.
  Cross-WAN follow-only delivery has never worked; LAN multicast masked it until
  ZEB-809 disabled scouting.

Both surfaces — descriptors AND content — must therefore travel the relay path.

## 1. Discovery: a fifth pkarr slot flavor — `vines`

New publisher module (`pkarr_vines_publisher.rs`) following the four existing
flavors (identity / community-epoch / friend / invite).

- **Slot key:** derived **publicly** from the owner address (contrast friend
  slots' secret derivation) — anyone holding `creator_addr` can resolve it.
  Derivation mirrors the identity slot's pattern with handle `"vines"`.
- **Record payload** (canonical CBOR, 2-char keys, same conventions as
  `ReachabilityAnnouncePayload`):

  ```
  rs  relay_set: Vec<VineRelayEntry>   // ≤ VINE_RELAY_SET_MAX = 4
  ts  issued_at_ms: u64
  sg  identity_signature: [u8; 64]     // #3 identity key over canonical CBOR
  ```

  `VineRelayEntry { ep iroh_node_id: [u8;32], hr home_relay: String }` —
  provenance-agnostic: nothing marks an entry "own device" vs "volunteer".
  Size check: 4 entries ≈ 4 × (32 B + relay URL) + envelope, comfortably inside
  pkarr's ~1 KB record bound; enforced at publish with a descriptive error.
- **Population (v1):** each of the creator's devices that (a) has vines locally
  and (b) is opted in, includes itself. Republish triggers mirror
  `ReachabilityPublisher`: startup, network change (2 s debounce), 60 min idle
  backstop, force on settings change.
- **Gate:** a `share_vines_publicly` toggle on `VineSettings` (default follows
  the existing `share_follows` convention — default **true**, consistent with
  vines being public content; flipping it off stops publication and lets the
  record expire).

## 2. Serve side: public-read vine-relay ALPN

New iroh ALPN `harmony/vine-relay/v1`, structurally mirroring the community
relay pull protocol (length-prefixed CBOR frames over `open_bi`) but
**unauthenticated by design** — the request carries no requester identity.

Frames:

- `VinePullQuery { creator_addr: OwnerAddr, since_created_at: u64, limit: u16 }`
  → `VinePullResponse { descriptors: Vec<WireVineDescriptor> }` — wire-form
  descriptors with their existing dual signatures (identity `#3` + device `#2`)
  intact, so the puller verifies authenticity independent of the relay.
- `VineContentRequest { cid: ContentId }` → chunked content frames. **Allowlist:**
  only CIDs referenced by `video_cid` of descriptors this node serves for that
  creator — the serve loop resolves the allowlist from its own vine store, not
  from the requester's claims. (Same posture as `put_serveable`: serving is an
  explicit decision per CID class.)
- Caps: `limit ≤ 256` descriptors/page; per-connection byte budget and a
  concurrent-sessions cap on the acceptor (values chosen at implementation with
  the other acceptors' caps as precedent); idle timeout = the existing
  `DEFAULT_RELAY_IO_DEADLINE_MS` (30 s) per exchange.

v1 relays are the creator's own devices, which already store the descriptors
(vine store) and video (CAS) — "hold" is serving what is already there. No
ingestion machinery is built in v1 (that is the volunteer half, deferred §6).

## 3. Follower side: vine pull driver

New driver module mirroring `community_relay_pull_driver`'s proven shape — same
cadence family (`COMMUNITY_RELAY_AD_REFRESH_MS` = 7 m 30 s), same telemetry
pattern (passes/sessions/ok/failed/recent ring), same one-loop-no-per-peer-tasks
structure.

Per cadence, for each followed creator:

1. **Skip if mesh-live:** if the creator's descriptors are already arriving via
   the wildcard subscription (shared community/friendship peers), the pull is a
   no-op — cheap check against feed-cache recency for that creator; pull runs
   anyway on first follow (cursor 0) to backfill history the mesh never carried.
2. **Resolve** the creator's `vines` pkarr slot — per-creator cooldown
   `PKARR_REFRESH_COOLDOWN` (15 min, existing constant); cache the relay set.
3. **Select a relay** — freshest-first; **skip any entry whose `iroh_node_id`
   equals a local endpoint id** (the ZEB-806 lesson, applied from day one — with
   a unit test, not a comment).
4. **Pull session:** query since the per-creator cursor; verify each descriptor
   through the **same validation/ingest path the wildcard subscriber uses**
   (signature checks, feed-cache admission, id-keyed LWW) so mesh and relay
   arrivals deduplicate naturally and no second trust path exists. Advance the
   cursor only past durably-ingested descriptors.
5. **Video fetch fallback:** the content-fetch path gains one fallback — if the
   mesh GET fails AND the CID belongs to a followed creator's vine, fetch over a
   vine-relay session to a relay from that creator's cached set. No change to
   the happy path.

Persistence: per-creator cursor + cached relay set in a small sidecar
(`vine_pull.cbor`), loss-safe (re-pull from 0 is idempotent through the id-keyed
cache).

## 4. Testing

- **Unit:** pkarr `vines` record round-trip + size-cap error; slot-key
  derivation stability; pull-frame codecs; self-entry skip; cursor advance only
  on durable ingest; allowlist refuses non-vine CIDs.
- **Integration:** pull driver against a mock serve ctx (mirroring the ZEB-806
  test structure): full-fidelity drain, dedupe against mesh-delivered
  descriptors, mesh-GET fallback selection.
- **e2e (the regression guard the ticket asked for):** `s_vines_follow_only` —
  two nodes, **no community, no friendship**. Alice publishes vines + `vines`
  slot; Bob follows by address; the pull driver delivers; view + reshare legs
  assert on the pulled copies. The existing
  `s_vines_publish_feed_view_reshare` keeps its community-join preamble — it is
  now honestly the *mesh-path* test.

## 5. Failure modes

- **Creator fully offline (all v1 relays down):** pull fails; telemetry records
  it; feed simply doesn't advance — the documented v1 availability trade-off of
  "own devices now" (phone-only creators degrade). Not an error state.
- **Stale pkarr record:** resolve honors record TTL; a follow of a
  vanished/never-published creator is a quiet no-op per cadence with the 15-min
  resolve cooldown bounding the cost.
- **Hostile relay:** can withhold or serve garbage; garbage fails descriptor
  signature verification at ingest; withholding is indistinguishable from
  offline (retry next cadence, try another entry).
- **Descriptor/content mismatch:** a descriptor whose `video_cid` no relay
  serves renders as unfetchable video — same failure class that exists on the
  mesh today; surfaced by the existing feed UI, not new handling.

## 6. Explicitly deferred

- **Volunteer relays:** the ad list and ALPN are provenance-agnostic by design;
  the missing half is volunteer-side ingestion/hold and a volunteering surface.
  Separate ticket when wanted.
- **Reverse channel:** a follow-only follower's *reactions* (and any
  follower→creator signal) do not reach the creator in v1 — that is a
  deposit-shaped design, later.
- **Tombstones / follow-list propagation over the relay path:** v1 relays serve
  descriptors + video only. Tombstone propagation to follow-only followers is
  accepted-latent (they miss deletions until a shared mesh exists) — worth its
  own look when volunteer relays land.
- **Live push:** pull cadence only; minutes-scale latency is the accepted feed
  semantics (Jake, 2026-07-26).
