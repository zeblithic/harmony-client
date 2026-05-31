# CAS-serve primitive + profile avatars — design

**Status:** approved (brainstormed 2026-05-31; revised 2026-05-30 after code validation of the leading-bit serve paradigm + single-PR directive)
**Author:** Jake (J Eng) + Claude
**Predecessor:** ZEB-341 (resolvable per-identity profile cards, merged PR #171, main `c82416a8`)
**Linear:** to be filed (one fresh ticket — this ships as a single PR)

## 1. Summary

Add **profile pictures (avatars)** to Harmony, resolved peer-to-peer by content ID. The
avatar is the deliberate *forcing function* for the real deliverable: a **general,
reusable peer-to-peer content-addressed-storage (CAS) serve primitive** that has never
once worked end-to-end. Avatars are its first consumer; long-form profile pages and
peer-to-peer file-sharing are the eventual consumers.

The `ProfileCardBroadcast` wire format (ZEB-341) was built additive-safe to carry a
future `avatar_cid: Option<[u8; 32]>` (reserved serde code `"av"`). This work fills that
slot and builds the missing transport underneath it.

**Ships as a single PR** (Jake's directive: one larger PR is set up to run autonomously
through the night and will get further than two PRs in series). The phased decomposition
in §9 is the *task* ordering within that one PR, not a PR split.

## 2. Problem & context

ZEB-341 made a community member's **display name + status** resolve from their `owner_id`
and render (members list, message authors, profile popover), with an identicon as the
avatar placeholder. The natural next step — and the originally-reserved extension — is a
real **profile picture**, stored as a content-addressed object and fetched by CID from
peers.

Harmony already has substantial CAS machinery:

- **`ContentId`** — a 32-byte content identifier (`harmony-content` crate, `cid.rs`): a
  4-byte header + a 28-byte hash. Content is chunked into **bundles** (`bundle.rs`:
  `ContentId::for_book` / `for_bundle` / `build_with_flags`) and fetched **recursively**
  (`fetch_recursive`, `MAX_BUNDLE_DEPTH` in `event_loop.rs`).
- **`StorageTier`** — a *sans-I/O* state machine in `harmony-content/storage_tier.rs` that
  integrates the `ContentStore` cache with Zenoh patterns. It already models the serve path
  (`StorageTierEvent::ContentQuery` → `StorageTierAction::SendReply`), startup queryable
  declarations (`DeclareQueryables`), and a **class-based admission policy** (`ContentPolicy`
  / documented rule: *"PublicDurable (00): always persist, always announce"*).
- **Local store** — `content_store.rs` (`ContentStore` trait, `RuntimeContentStore`), backed
  by the `StorageTier` cache. Ingest admits a blob locally (`CasOp::PutLocal`).
- **Fetch-by-CID** — `fetch_via_zenoh` issues `session.get("harmony/content/{prefix}/{cid_hex}")`
  (a Zenoh GET / query), wrapped by `CasOp::GetOrFetch` (cache-first, then network), with a
  self-admit back into the cache. Exposed to the frontend via the `fetch_content` IPC.
- **Frontend resolver** — `avatar-resolver.ts` maps a CID hex → blob URL via `fetch_content`;
  `nav-service.ts` already consumes it (`resolveAvatarUrl`) for DM/peer avatars;
  `Avatar.svelte` renders `<img>`-or-identicon; the peer-profile wire carries `avatarUrl`.

**But CAS peer-to-peer has never been proven to work** (confirmed by Jake; surfaced during
Koya↔KRILE alpha testing). The store half and the fetch half both exist — but a fetch only
returns data if *someone answers the query*, and (§3) nothing does.

## 3. Root cause — the serve reply is computed, then discarded

The serve **design** already exists in `harmony-content`: `StorageTier` is a sans-I/O state
machine that, given a `StorageTierEvent::ContentQuery { query_id, cid }`, returns a
`StorageTierAction::SendReply { query_id, payload }`, alongside `DeclareQueryables { key_exprs }`
startup actions and a class-based `class_admits()` policy. harmony-client wraps it via
`NodeRuntime` and already feeds it ingest / `PublishContent`.

**What is missing is the client-side serve bridge.** Two concrete gaps:

1. `RuntimeAction::SendReply` is a **stub** — `event_loop.rs:2813`:
   `tracing::trace!("SendReply not yet implemented in client")`. The reply payload the runtime
   computes is dropped on the floor.
2. The generic `RuntimeAction::DeclareQueryable` handler (`event_loop.rs:2675`) **discards the
   Zenoh `Query` object**, forwarding only `{key_expr, payload}` as a `ZenohEvent::Query` with a
   hardcoded `query_id: 0` — so even a declared content queryable has no handle to reply through.

Net: a `fetch_via_zenoh` GET on `harmony/content/{prefix}/{cid_hex}` reaches no responder that
ever replies, and every fetch times out. Content is admitted **locally only**
(`harmony/content/publish/{cid}` is a local `runtime.push_event`, not a network publish).

The two queryables that *do* reply — mail-root and the channel-log `since/**` backfill
(`event_loop.rs:4214-4289`) — keep their `Query` and call `query.reply()` **inline**. That
inline-reply pattern is the template for the content-serve queryable (§5.1).

## 4. Chosen design (approved)

A **general, reusable public-tier CAS-serve primitive**, validated **prove-first**, with
avatars as its first consumer.

Four decisions, all approved in brainstorming:

1. **CAS p2p is unproven** → the serve queryable is the core deliverable; a two-node proof
   test gates everything.
2. **General primitive, not avatar-specific** → built as reusable serve-by-CID infrastructure;
   long-form profile pages + file-sharing reuse it unchanged.
3. **Public-tier re-serve** → a node serves content it owns *and* public content it has
   cached; encrypted content is never served. Avatars resolve even when their owner is offline.
4. **Prove-first sequencing** → Phase 0 is a minimal serve + two-node fetch test, before any
   avatar code.

The public/encrypted boundary (decision 3) is **the CID's own leading bit** (§5.2), which is
Jake's stated paradigm — validated to be exactly how `harmony-content` already classifies
content.

## 5. Architecture — the CAS-serve primitive

### 5.1 The serve queryable (inline-reply, mirroring channel-log)

A dedicated content-serve queryable task modeled on the channel-log queryable
(`event_loop.rs:4214-4289`), declared in the startup-actions block. It declares a Zenoh
queryable on the content shard patterns (`harmony/content/{prefix}/**` — sourced from
`harmony-content`'s `zenoh_bridge::content_queryable_key_exprs()` / `all_shard_patterns()`,
the same key family `fetch_via_zenoh` already GETs against). On each incoming query:

1. Parse `{cid_hex}` from the query key → 32-byte `ContentId` (skip malformed selectors, as
   the channel-log queryable rejects malformed backfill keys).
2. **Gate on `!cid.flags().encrypted`** (§5.2). An encrypted CID is never served — reply nothing.
3. Look up the bytes in the **local store** *without triggering a network fetch* — via a new
   read-only `CasOp::GetLocal { cid, reply }` variant routed through the existing `cas_op_tx`
   channel. The event-loop's CAS handler reads `runtime.storage_tier().cache().get(&cid)` and
   replies over the oneshot. (Routing through `cas_op_tx` keeps the `NodeRuntime` single-owned
   by the event loop, and a read-only `GetLocal` cannot recursively trigger another fetch the
   way `GetOrFetch` would.)
4. **Re-verify `hash(bytes) == cid`** before replying — never serve corrupt bytes. Cheap
   insurance; the cache should already hold verified bytes (§5.3), but the serve side is the
   last gate before bytes leave this node.
5. `query.reply(query.key_expr(), bytes)`. On a miss (not held locally) reply nothing — the
   querier's GET falls through to other responders or times out.

**Why inline-cache-read rather than the sans-I/O `SendReply` route:** the full `StorageTier`
sans-I/O serve path (keep a `pending_queries: HashMap<u64, Query>`, feed
`RuntimeEvent::ContentQuery` with a real `query_id`, implement `RuntimeAction::SendReply` to
pop the pending `Query` and reply) would inherit disk-tier fallback + `queries_served` metrics
for free — but it re-plumbs the intricate sans-I/O bridge and the hardcoded `query_id: 0`. For
this cut (avatars are small, single-chunk, recently-ingested public content that lives in the
in-memory cache), the inline cache read mirrors a proven template at far lower risk. The
sans-I/O route is the documented future refinement (§12), tracked but explicitly out of scope.

Bounds: replies are size-capped by a `MAX_SERVE_BYTES` guard (paralleling `MAX_CARD_WIRE_BYTES`);
the lookup is read-only and fast (cache hit). Known limitation: content evicted from the
in-memory cache to disk is not served by the inline path (acceptable for avatars; see §13).

### 5.2 Public-tier servable set — the CID's own encrypted bit

**Jake's paradigm, validated against the code:** anything in CAS registered as **unencrypted
is publicly shareable / re-shareable.** This is not a separate registry — it is encoded
**intrinsically in the CID itself.**

A `ContentId` is 32 bytes: a 4-byte header + a 28-byte hash (`harmony-content/cid.rs`). The
**top bit of the header** (`0x80`, bit 31 — the literal leading bit) is the `encrypted` flag
(`ContentFlags::from_bits`, `cid.rs:74`: `encrypted: byte & 0x80 != 0`). `ContentId::content_class()`
maps `(encrypted, ephemeral)` → `PublicDurable` / `PublicEphemeral` / `EncryptedDurable` /
`EncryptedEphemeral`. The library's `ContentPolicy` / `class_admits()` already encodes the
intended rule: *"PublicDurable (00): always persist, always announce"* and *"EncryptedEphemeral
(11): always rejected — never stored or announced."*

So the serve-admission gate is simply:

> **serve iff `!cid.flags().encrypted`** (i.e. `content_class()` is `PublicDurable` or
> `PublicEphemeral`).

**This dissolves the chunk-membership problem.** Every chunk CID independently carries its own
`encrypted` bit — a chunk of an unencrypted bundle is itself `PublicDurable` and **self-attests
as servable**. No CID→public membership set is needed; the serve queryable inspects the
requested CID's own header byte. And the gate is unforgeable-by-omission: you cannot serve
encrypted bytes *as* public without changing the flags, which changes the header, which changes
the CID — so the requester would be asking for a different CID entirely.

**Relationship to `content_index::Sensitivity`** (`content_index.rs:73`: `Private` / `Confidential`
/ `Public`): that enum is an *index-level* durability/announce annotation, **orthogonal** to
serve admission. Serve admission keys on the **CID bit** (intrinsic, per-chunk, unforgeable),
not on the index entry (which is per-root and may not exist for child chunks). The two do not
conflict; the CID bit is the sole serve gate. (The original spec's reliance on `Sensitivity::Public`
is superseded by this finding.)

### 5.3 Verify-on-fetch — the security keystone

Currently nothing verifies that fetched bytes hash to the requested CID before they enter the
cache — `fetch_via_zenoh` returns the raw reply payload unchecked. Add a `hash(bytes) ==
requested_CID` check in the fetch/admit path (`CasOp::GetOrFetch` success arm and the
`fetch_recursive` admit wrapper), using `harmony-content`'s CID derivation (`ContentId::for_book`
/ the same hasher the CID was minted with). A reply whose bytes don't match the requested CID is
**dropped, not cached**.

This is what makes public re-serve safe: any peer may serve Alice's avatar bytes, but a
tampered reply fails the CID check and never poisons the cache. Integrity is decoupled from
availability — the serve side need only be *available*, never *trusted*. This check is a
required part of the primitive, not a nicety, and pairs with the serve-side re-verify (§5.1
step 4) for defense in depth on both ends.

### 5.4 Transport

**Zenoh GET** (the existing `fetch_via_zenoh` / `fetch_recursive` path). Avatars are small
(§6.2), so a normalized avatar fits comfortably in Zenoh reply payload(s); the existing
bundle/recursive-fetch machinery handles multi-chunk content if a blob ever exceeds one
chunk. iroh blob-transfer is **not** introduced here — it's the right tool for large
file-sharing later, but it's unbuilt and overkill for avatars. The primitive's serve/fetch
interface is transport-agnostic enough that a future iroh path can slot in without changing
consumers.

## 6. Architecture — the avatar feature

### 6.1 Card field

Add to `ProfileCardBroadcast` (`profile_card_broadcast.rs`):

```rust
// Encoded as a CBOR bstr(32), the same byte-array-as-bstr family used for
// owner_id ([u8;16] via owner_state_types::serialize_bytes_as_bstr /
// deserialize_bytes_from_bstr) — here wrapped for Option<[u8;32]> so `None`
// omits the key entirely.
#[serde(rename = "av", skip_serializing_if = "Option::is_none",
        with = "avatar_cid_bstr_opt")]
pub avatar_cid: Option<[u8; 32]>,
```

- **Signed**: `avatar_cid` is inside the canonical-CBOR signed bytes (the existing
  whole-struct sign/verify with the signature field zeroed). So an avatar can't be spoofed
  onto someone's card — the owner's device-#2 signature covers it. `verify_card` is unchanged.
- **Backward-compatible**: with `skip_serializing_if = Option::is_none`, a user with no avatar
  produces **byte-identical** encoding to the ZEB-341 cards already on the wire. `"av"` sorts
  before `"dn"`, so canonically it lands as field 2 when present.
- **Wire-format fixture**: `wire_format_profile_card_fixtures.rs` gains a pinned case with an
  avatar set, plus the existing no-avatar case (proving byte-identity).

### 6.2 Upload / ingest pipeline

1. User picks an image (file picker).
2. **Frontend normalizes** via canvas: downscale to a max **256×256**, re-encode to **PNG**
   (universal, lossless). Reject inputs over an input cap (~10 MB pre-downscale) and
   non-image types. Output is bounded (~tens of KB).
3. Ingest the normalized **bytes** into CAS, yielding a `ContentId`. Avatars come from canvas
   as an in-memory blob, not a file on disk, so this uses a **bytes-ingest path** (a thin
   `ingest_bytes` IPC, or the existing `streaming_ingest` driven by an in-memory reader — the
   plan picks one). Ingest uses **default `ContentFlags`** (`encrypted: false`) → the resulting
   CID is `PublicDurable`, so it is self-serveable under §5.2 with no extra flagging. (Validated:
   the existing ingest/bundle path already builds with `ContentFlags::default()` throughout.)
4. Set `avatar_cid` on the owner card; re-sign + republish via the existing
   `republish_owner_card` path, extended to carry the CID.
5. **Self-seed** the avatar immediately (local blob URL from the just-ingested bytes) so the
   user's own row/messages show their picture with zero network — same self-first principle as
   name/status.

Frontend normalization means **no Rust image dependency** and a hard bound on served bytes.

### 6.3 Render path

`MemberCardService` (frontend) gains avatar resolution: when a resolved card carries
`avatar_cid`, it kicks off `fetch_content(cid_hex)` → (backend verifies §5.3) → blob URL via
the existing `avatar-resolver.ts` → stored in the reactive card map alongside name/status.
`Avatar.svelte` already renders `avatarUrl`-or-identicon, so consumers pass the resolved URL:

- **MemberRow.svelte** — member avatar.
- **ChannelMessageFeed.svelte** — message-author avatar.
- **ProfilePopover.svelte** (owner-card variant) — large avatar.

**Fallback to identicon** whenever: no `avatar_cid`, fetch pending, or unresolvable (owner
offline *and* no cached re-server). The UI degrades gracefully and never blocks on a fetch.

## 7. End-to-end data flow

**Publish (owner sets avatar):** pick image → normalize → ingest as default-flags (PublicDurable)
bundle → CID → `republish_owner_card(name, status, avatar_cid)` → device-#2-signed
`ProfileCardBroadcast` published on `harmony/discovery/profile/owner/{owner_id}/card`; the serve
queryable now answers GETs for the avatar's CID (and any chunk CIDs) because each is unencrypted.

**Resolve (peer renders avatar):** peer's `MemberCardService` receives + verifies the card →
sees `avatar_cid` → `fetch_content(cid)` → `CasOp::GetOrFetch`: cache miss → `fetch_via_zenoh`
GET on `harmony/content/{prefix}/{cid}` → **served by the owner (or any public-cache peer)** →
bytes verified `hash==CID` → admitted to cache (now this peer can re-serve too) → blob URL →
`Avatar.svelte`.

## 8. Security model

- **Self-verifying integrity:** every fetched chunk is checked `hash(bytes) == CID` before
  admit (§5.3), and re-checked serve-side before reply (§5.1). A malicious server cannot poison
  the cache.
- **No encrypted leakage:** only content whose CID has `encrypted == 0` is served (§5.2).
  Encrypted content is never answered, and the gate is intrinsic to the CID (cannot be forged
  by relabeling).
- **Avatar authenticity:** `avatar_cid` is inside the device-#2-signed card; a peer can't
  forge an avatar onto another owner's card.
- **Availability ≠ trust:** because integrity is guaranteed by the CID, *any* peer can serve
  public content. Re-serve from cache is therefore safe and improves availability.

## 9. Phased decomposition (task ordering within one PR)

These phases are the **task sequence inside the single PR**, not separate PRs.

- **Phase 0 — Proof slice (hard gate).** Minimal serve queryable (serve any locally-held CID)
  + `CasOp::GetLocal` + a two-node integration test (mirroring `two_engines_exchange_via_iroh_zenoh`):
  node A ingests a blob, node B fetches it by CID and receives the verified bytes. Must be
  green before proceeding — this kills the "does Zenoh GET work p2p at all?" unknown.
- **Phase 1 — Harden into the public-tier primitive.** Add the `!cid.flags().encrypted` serve
  gate; add verify-on-fetch (`hash==CID`, both sides); serve-path size bounds. Tests: encrypted
  content is *not* served; tampered bytes are rejected.
- **Phase 2 — Card field.** `avatar_cid` on `ProfileCardBroadcast`; wire-format fixture;
  sign/verify round-trip + backward-compat (no-avatar byte-identity).
- **Phase 3 — Upload pipeline.** Frontend pick → canvas normalize (256² PNG, input cap) →
  bytes-ingest (default flags → PublicDurable) → `republish_owner_card` with CID → self-seed.
- **Phase 4 — Render.** `MemberCardService` avatar resolution → `Avatar.svelte` in MemberRow /
  ChannelMessageFeed / ProfilePopover; identicon fallback.
- **Phase 5 — Cross-peer e2e + full gate sweep + PR.** Two owners: A publishes an avatar card,
  B verifies + resolves + renders.

## 10. Testing strategy

- **Phase 0:** two-node fetch-by-CID integration test (the proof).
- **Phase 1:** serve-policy tests (unencrypted served / encrypted not); verify-on-fetch
  (tampered bytes dropped, not cached, on both fetch and serve sides).
- **Phase 2:** wire-format fixture (avatar + no-avatar) + sign/verify round-trip + backward-compat.
- **Phase 3:** frontend upload/normalize vitest (canvas resize bound, input cap, format);
  bytes-ingest backend test (resulting CID is `PublicDurable`).
- **Phase 4:** `MemberCardService` avatar-resolution vitest; render/fallback component tests.
- **Phase 5:** cross-peer e2e (two owners: A publishes avatar card, B verifies + resolves +
  renders).
- **Gates (every phase):** fmt / clippy / nextest / large-tests / MSRV / frontend tsc + vitest.

## 11. Scope

**In scope:** the general public-tier CAS-serve primitive (content-serve queryable +
`!cid.flags().encrypted` gate + verify-on-fetch); the two-node p2p proof; `avatar_cid` on the
signed owner card; avatar upload (pick → normalize → bytes-ingest → republish); render across
MemberRow / ChannelMessageFeed / ProfilePopover; identicon fallback; self-seed. **One PR.**

**Out of scope (this cut):**

- Long-form profile page (`profile_page_root`) — reserved slot, the next CAS consumer.
- New DM-avatar UI — the existing `nav-service` avatar path lights up *for free* once the serve
  primitive lands (it uses the same fetch), but no new DM avatar UI is added here.
- The full sans-I/O `StorageTier` `SendReply` serve route with disk-tier fallback + metrics —
  the inline-cache-read serve (§5.1) is sufficient for avatars; the sans-I/O route is the
  documented future refinement (§12).
- iroh / large-file transport — Zenoh GET is sufficient for small avatars; revisit for
  file-sharing.
- Avatar moderation, animated avatars, avatar history/versioning.
- Disk-persisted public-servable set across restarts beyond what the content index already
  persists (the CID bit is intrinsic, so nothing extra is needed for the gate; this only
  concerns the inline path's in-memory-cache limitation, §13).

## 12. CAS extensibility (forward-look)

This primitive is the foundation the card wire format was designed for. After avatars:

- **`profile_page_root: Option<[u8; 32]>`** (reserved serde `"pp"`) — a long-form "personal
  page" as a public CAS bundle root, resolved by the *same* serve primitive.
- **Peer-to-peer file-sharing** — public file bundles served by the same queryable; large
  blobs motivate (a) the full sans-I/O `StorageTier` `SendReply` serve route (disk-tier
  fallback for content too large to stay cached) and (b) possibly the iroh transport path
  behind the same serve/fetch interface.

Each reuses the serve primitive unchanged; only the consumer + (for files) the transport/tier
plumbing differ.

## 13. Open questions / risks

- **In-memory-cache limitation of the inline serve path (§5.1):** the inline-cache-read serve
  only answers for content currently in the `StorageTier` in-memory cache; content evicted to
  disk is not served until the sans-I/O `SendReply` route lands (§12). Acceptable for avatars
  (small, pinned/recent, public). Pin self-published avatar bytes so the owner reliably serves
  its own avatar.
- **Zenoh GET semantics under multiple responders:** with public re-serve, several peers may
  answer the same CID query. `fetch_via_zenoh` already consumes the first successful reply;
  confirm consolidation/dedup behavior in the two-node → N-node case (Phase 0/1 validates).
- **Serve-path load:** a popular avatar could draw many GETs. Bounded by small payloads + cache
  hits; revisit rate-limiting if it surfaces (out of scope unless observed).
