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
4. **Vines are public, by intent and by default** (Jake, 2026-07-26): video
   content on Harmony is meant to be broadly, publicly shareable. Any future
   "private vines" support is an explicit afterthought and not a priority —
   which is why public-read (decision 2) is the design center, not a tier.

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

- `VinePullQuery { creator_addr: OwnerAddr, after: (u64, VineId), limit: u16 }`
  → `VinePullResponse { descriptors: Vec<WireVineDescriptor> }` — wire-form
  descriptors with their existing dual signatures (identity `#3` + device `#2`)
  intact, so the puller verifies authenticity independent of the relay. The
  cursor is the lossless tuple `(created_at_ms, vine descriptor id)` with
  strictly-greater tuple ordering server-side (`created_at` alone is not
  unique — equal-timestamp descriptors would be skipped); pages are served in
  ascending tuple order. First pull uses the tuple minimum.
- `VineContentRequest { cid: ContentId }` → chunked content frames. **Allowlist:**
  only CIDs referenced by `video_cid` of descriptors this node serves for that
  creator — the serve loop resolves the allowlist from its own vine store, not
  from the requester's claims. (Same posture as `put_serveable`: serving is an
  explicit decision per CID class.)
- Caps (concrete v1 defaults — named constants, tunable, but every bound MUST
  exist because this endpoint is public/unauthenticated):
  - `limit ≤ 256` descriptors/page.
  - Frame-size ceilings on every length-prefixed read: request + descriptor
    frames `VINE_QUERY_MAX_FRAME_BYTES = 64 KiB`; content chunk frames
    `VINE_CONTENT_MAX_FRAME_BYTES = 16 MiB` (precedent:
    `RELAY_PULL_MAX_FRAME_BYTES`, `community_relay.rs`). Oversize prefix →
    close the connection without allocating.
  - `VINE_RELAY_MAX_CONCURRENT_SESSIONS = 8` acceptor-wide (semaphore); at
    capacity, new connections are accepted and immediately closed with an
    app close code — the client treats it as offline and retries next
    cadence. (New bound: the community-relay acceptor bounds only by
    per-exchange deadline, which is insufficient for a public endpoint.)
  - Per-connection byte budget `VINE_RELAY_SESSION_BYTE_BUDGET = 256 MiB`
    served bytes; exceeded → close (a follower resumes from its cursor on
    the next session).
  - Idle timeout = the existing `DEFAULT_RELAY_IO_DEADLINE_MS` (30 s) per
    exchange.

v1 relays are the creator's own devices, which already store the descriptors
(vine store) and video (CAS) — "hold" is serving what is already there. No
ingestion machinery is built in v1 (that is the volunteer half, deferred §6).

## 3. Follower side: vine pull driver

New driver module mirroring `community_relay_pull_driver`'s proven shape — same
cadence family (`COMMUNITY_RELAY_AD_REFRESH_MS` = 7 m 30 s), same telemetry
pattern (passes/sessions/ok/failed/recent ring), same one-loop-no-per-peer-tasks
structure.

Per cadence, for each followed creator:

1. **Skip if mesh-live — bounded:** if the creator's descriptors are already
   arriving via the wildcard subscription (shared community/friendship peers),
   the pull may be skipped this cadence — cheap check against feed-cache
   recency for that creator. But recency is not completeness (one descriptor
   arriving over the mesh proves nothing about the ones the mesh missed), so
   the skip is bounded: at most `VINE_PULL_SKIP_MAX_CONSECUTIVE = 4`
   consecutive cadences (~30 min), after which a repair pull runs regardless
   and resets the counter. Redundant repair pulls are cheap — the since-cursor
   query returns little/nothing new and ingest is id-keyed-deduped. Pull runs
   unconditionally on first follow (cursor 0) to backfill history the mesh
   never carried.
2. **Resolve** the creator's `vines` pkarr slot — per-creator cooldown
   `PKARR_REFRESH_COOLDOWN` (15 min, existing constant); cache the relay set.
3. **Select a relay** — freshest-first; **skip any entry whose `iroh_node_id`
   equals a local endpoint id** (the ZEB-806 lesson, applied from day one — with
   a unit test, not a comment).
4. **Pull session:** query after the per-creator tuple cursor; verify each
   descriptor through the **same validation/ingest path the wildcard
   subscriber uses** (signature checks, feed-cache admission, id-keyed LWW) so
   mesh and relay arrivals deduplicate naturally and no second trust path
   exists. Cursor advance distinguishes two failure kinds: a descriptor that
   fails **verification** (bad signature — the relay served garbage) is
   logged, counted in telemetry, and **skipped**, and does NOT block the
   cursor (each descriptor is independently dual-signed, so skipping a
   poisoned entry cannot forge or hide later ones — a withholding relay could
   omit it anyway); a descriptor that verifies but fails **local ingest**
   (durability error) stops cursor advance at the last durable entry so the
   next session retries it. The cursor therefore advances past
   verified-and-ingested and verified-invalid-skipped entries only.
5. **Video fetch fallback:** the content-fetch path gains one fallback — if the
   mesh GET fails AND the CID belongs to a followed creator's vine, fetch over a
   vine-relay session to a relay from that creator's cached set. No change to
   the happy path.

Persistence: per-creator cursor + cached relay set in a small sidecar
(`vine_pull.cbor`), loss-safe (re-pull from 0 is idempotent through the id-keyed
cache).

## 4. Testing

- **Unit:** pkarr `vines` record round-trip + size-cap error; slot-key
  derivation stability; pull-frame codecs; self-entry skip; tuple-cursor
  ordering with equal `created_at` values (no skip, no duplicate); an
  injected invalid descriptor at the page high-watermark is skipped without
  blocking cursor advance, while a durability failure does block it;
  allowlist refuses non-vine CIDs; mesh-live skip counter forces a repair
  pull at `VINE_PULL_SKIP_MAX_CONSECUTIVE`.
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

## As-implemented notes (ZEB-811 branch)

1. **No inner `sg` field on the vines record.** §1's payload table lists an
   inner `sg identity_signature`, but the implementation carries only
   `rs`/`ts`. The pkarr envelope (`PkarrRoutingRecord`) already signs the
   blob with the `#3` identity key and embeds the identity pub, so an inner
   signature would be redundant — the reachability flavor already zero-fills
   its own inner signature on the pkarr path for the same reason.
   Authenticity is `verify_inner_sig()` + freshness + the
   identity-pub-to-address binding.

2. **Cursor is `(u64, String)`, not a typed pair.** `created_at` is stored
   and compared as seconds (`u64`), and vine descriptor ids are plain
   `String`s rather than a dedicated id type — so the per-creator pull
   cursor and the relay's page-ordering key are both the literal tuple
   `(u64, String)`. Equal-`created_at` ties break on the id string.

3. **On-disk descriptors retain their wire signatures.** The feed cache's
   on-disk row carries the descriptor's wire-form signature fields
   (previously dropped once ingest verified them), so a relay can still
   serve verifiable descriptors after a restart — §2's "hold is serving what
   is already there" assumed this implicitly. Rows written before this
   change load but are treated as unsigned and are not served over the
   relay path.

4. **Publish self-ingests into the feed cache.** After a successful zenoh
   publish, the creator's own descriptor is fed directly into their own vine
   feed cache (id-keyed, first-write-wins) rather than relying on a zenoh
   loopback delivery to populate it. This is best-effort — failures are
   swallowed — but means a creator's own feed reflects a publish
   immediately instead of waiting on a mesh round-trip.

5. **No network-change republish trigger in v1.** §1 describes republish
   triggers mirroring `ReachabilityPublisher` (startup, network-change
   debounce, idle backstop, forced on settings change). As implemented, the
   vines publisher registers at startup and republishes explicitly after a
   publish or a settings toggle; there is no network-change watcher. This is
   acceptable because a vines record's contents (endpoint id, home relay
   URL) are churn-stable compared to reachability's direct addresses, which
   change with the network path itself.

6. **Wake-on-follow lives in the IPC handlers, not the event loop.** The
   event loop's follow/unfollow arm stays the no-op it always was for
   network purposes. The pull driver's wake is instead signaled directly
   from the follow/unfollow IPC implementations, right after the existing
   follow-set mutation.

7. **Relay serve re-serializes rather than forwarding wire bytes.** The
   vine-relay serve path deserializes cached descriptors and re-serializes
   them for the wire response, rather than caching and forwarding the
   original bytes verbatim. This is safe because signature verification
   binds to the descriptor's deterministic canonical-CBOR encoding, not to
   incidental wire-byte layout, so re-encoding cannot invalidate a
   signature.

8. **The ingest-verdict type has four variants, not three.** Beyond
   fresh-insert and verification-failure, ingest distinguishes a
   mesh-duplicate case from a genuinely new descriptor: both advance the
   cursor, but only fresh inserts count toward ingest telemetry. A
   three-variant shape would have over-counted mesh-delivered duplicates as
   ingest activity.

9. **Video fetch fallback has extra hardening beyond §3 step 5.** Before
   allocating a buffer for a relay-served video, the fallback validates the
   relay's claimed size against the existing vine-video size cap (the same
   100 MiB ZEB-559 upload limit) and enforces a running-total cutoff while
   streaming, so a malicious or buggy relay cannot force an oversized
   allocation. All relay attempts for one fetch share a single I/O deadline
   budget rather than each attempt getting its own timeout; the final local
   content-store write runs outside that budget.

10. **The node state carries a handle to the pull driver.** This lets the
    video fetch fallback read the same cached relay set the pull driver
    maintains, rather than re-resolving it independently.

11. **e2e scenario notes.** `s_vines_follow_only` reads the creator's
    address off the creator's own feed after publish rather than a
    dedicated RPC — this is deliberately not the owner-state RPC's owner id,
    which is different key material with a different hash formula. The
    scenario also positively asserts on vine-relay pulling/serving telemetry
    (sessions, descriptors ingested/served) rather than only on the
    end-to-end outcome, so a pass can't be explained by an accidental
    non-relay delivery path.

12. **Multi-device creators publish only their own device's relay, not a
    fleet-aggregated set.** Each device signs and publishes
    `relay_set = [self]` under the same per-creator vines pkarr slot key, so
    a creator enrolled on more than one device produces last-writer-wins
    overwrites rather than the merged ≤4-entry set §1 describes — followers
    only ever see whichever device published most recently. Aggregating
    relay entries across a creator's fleet, so followers see every device's
    relay rather than just the last publisher's, is left as future work.
