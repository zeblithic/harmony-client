# CAS-serve primitive + profile avatars — design

**Status:** approved (brainstormed 2026-05-31)
**Author:** Jake (J Eng) + Claude
**Predecessor:** ZEB-341 (resolvable per-identity profile cards, merged PR #171, main `c82416a8`)
**Linear:** to be filed (this work is a fresh ticket — likely two: the CAS-serve primitive, then avatars on top)

## 1. Summary

Add **profile pictures (avatars)** to Harmony, resolved peer-to-peer by content ID. The
avatar is the deliberate *forcing function* for the real deliverable: a **general,
reusable peer-to-peer content-addressed-storage (CAS) serve primitive** that has never
once worked end-to-end. Avatars are its first consumer; long-form profile pages and
peer-to-peer file-sharing are the eventual consumers.

The `ProfileCardBroadcast` wire format (ZEB-341) was built additive-safe to carry a
future `avatar_cid: Option<[u8; 32]>` (reserved serde code `"av"`). This work fills that
slot and builds the missing transport underneath it.

## 2. Problem & context

ZEB-341 made a community member's **display name + status** resolve from their `owner_id`
and render (members list, message authors, profile popover), with an identicon as the
avatar placeholder. The natural next step — and the originally-reserved extension — is a
real **profile picture**, stored as a content-addressed object and fetched by CID from
peers.

Harmony already has substantial CAS machinery:

- **`ContentId`** — a 32-byte content identifier (`harmony-content` crate; `content_index.rs`
  stores `cid: [u8; 32]`). Content is chunked into **bundles** (`harmony-content/bundle.rs`:
  `chunk_count`, `ContentId::for_book`) and fetched **recursively** (`fetch_recursive`,
  `MAX_BUNDLE_DEPTH` in `event_loop.rs`).
- **Local store** — `content_store.rs` (`ContentStore` trait, `RuntimeContentStore`), backed
  by harmony-runtime's `StorageTier` cache. Ingest admits a blob locally
  (`CasOp::PutLocal` → `runtime.push_event(SubscriptionMessage)` → StorageTier).
- **Fetch-by-CID** — `fetch_via_zenoh` issues `session.get("harmony/content/{prefix}/{cid_hex}")`
  (a Zenoh GET / query), wrapped by `CasOp::GetOrFetch` (cache-first, then network), with a
  self-admit back into the cache. Exposed to the frontend via the `fetch_content` IPC.
- **Frontend resolver** — `avatar-resolver.ts` maps a CID hex → blob URL via `fetch_content`;
  `nav-service.ts` already consumes it (`resolveAvatarUrl`) for DM/peer avatars;
  `Avatar.svelte` renders `<img>`-or-identicon; the peer-profile wire carries `avatarUrl`.

**But CAS peer-to-peer has never been proven to work** (confirmed by Jake; surfaced during
Koya↔KRILE alpha testing). The store half and the fetch half both exist — but a fetch only
returns data if *someone answers the query*.

## 3. Root cause — the serve half was never built

There are exactly **two** Zenoh queryables in the client:

1. the mail-root query, and
2. the channel-log `since/**` backfill queryable (`event_loop.rs:4222`,
   `harmony/channels/{cid}/{ch_id}/since/**`).

**Neither serves content.** When a node calls `fetch_via_zenoh` on
`harmony/content/{prefix}/{cid_hex}`, *no node has a queryable declared on that key*, so the
GET has no responder and times out. Content is admitted **locally only**
(`harmony/content/publish/{cid}` is a local `runtime.push_event`, not a network publish).

The missing piece is a **content-serve queryable**: a responder that, given a CID query,
looks up the bytes in the local store and replies. Building it is what makes every existing
`fetch_via_zenoh` call finally get an answer — and is the load-bearing work here.

## 4. Chosen design (approved)

A **general, reusable public-tier CAS-serve primitive**, validated **prove-first** with
avatars as its first consumer.

Four decisions, all approved in brainstorming:

1. **CAS p2p is unproven** → the serve queryable is the core deliverable; a two-node proof
   test gates everything.
2. **General primitive, not avatar-specific** → built as reusable serve-by-CID infrastructure;
   long-form profile pages + file-sharing reuse it unchanged.
3. **Public-tier re-serve** → a node serves content it owns *and* public content it has
   cached; private content is never served. Avatars resolve even when their owner is offline.
4. **Prove-first sequencing** → Phase 0 is a minimal serve + two-node fetch test, before any
   avatar code.

## 5. Architecture — the CAS-serve primitive

### 5.1 The serve queryable

A new Zenoh queryable declared on `harmony/content/{prefix}/**`, mirroring the existing
channel-log `since/**` queryable (`event_loop.rs:4222`, declared in the startup-actions
block). On each incoming query:

1. Parse the `{cid_hex}` out of the query key → 32-byte `ContentId`.
2. Look up the CID in the **local store** *without triggering a network fetch* — via a new
   `CasOp::GetLocal { cid, reply }` variant routed through the existing `cas_op_tx` channel
   (so the serve path cannot recursively trigger another fetch).
3. Gate on the **public-tier servable set** (§5.2). If the CID is held locally **and**
   publicly servable, `query.reply(key, bytes)`. Otherwise reply nothing (the querier's GET
   simply gets no reply from this node and falls through to other responders / times out).

Bounds: replies are size-capped (reject/skip serving a chunk larger than a `MAX_SERVE_BYTES`
guard, paralleling `MAX_CARD_WIRE_BYTES`); the lookup is read-only and fast (cache hit).

### 5.2 Public-tier servable set

Reuse the existing `Sensitivity` enum (`content_index.rs:73`: `Private`, `Confidential`,
`Public`). A chunk is servable iff it belongs to content recorded as `Sensitivity::Public` in
the content index, *and* is held in the local cache.

**Bundle nuance:** content is chunked; a `ContentIndexEntry` records the *root* CID +
sensitivity, while child chunk CIDs are addressable but may lack their own index entry.
The servable set must therefore cover the whole bundle (root **and** chunks) for public
content. At ingest time, when content is admitted as `Public`, every chunk CID in its bundle
is recorded in a CID→public membership set (or equivalent) so the serve queryable can answer
chunk-level queries. (Avatars are small — typically a single chunk — so this is trivial for
the avatar consumer; it matters for the general primitive with large content. The exact
membership representation is a plan-level detail.)

### 5.3 Verify-on-fetch — the security keystone

Currently nothing verifies that fetched bytes hash to the requested CID before they enter the
cache. Add a `hash(bytes) == requested_CID` check in the fetch/admit path
(`CasOp::GetOrFetch` success arm and the `fetch_recursive` admit wrapper), using
`harmony-content`'s CID derivation. A reply whose bytes don't match the requested CID is
**dropped, not cached**.

This is what makes public re-serve safe: any peer may serve Alice's avatar bytes, but a
tampered reply fails the CID check and never poisons the cache. Integrity is decoupled from
availability — the serve side need only be *available*, never *trusted*. This check is a
required part of the primitive, not a nicety.

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
3. Ingest the normalized bytes into CAS as a **`Public`** bundle (existing ingest path),
   yielding a `ContentId`.
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

**Publish (owner sets avatar):** pick image → normalize → ingest as `Public` bundle → CID →
`republish_owner_card(name, status, avatar_cid)` → device-#2-signed `ProfileCardBroadcast`
published on `harmony/discovery/profile/owner/{owner_id}/card`; chunk CIDs enter the public
servable set; serve queryable now answers GETs for them.

**Resolve (peer renders avatar):** peer's `MemberCardService` receives + verifies the card →
sees `avatar_cid` → `fetch_content(cid)` → `CasOp::GetOrFetch`: cache miss → `fetch_via_zenoh`
GET on `harmony/content/{prefix}/{cid}` → **served by the owner (or any public-cache peer)** →
bytes verified `hash==CID` → admitted to cache (now this peer can re-serve too) → blob URL →
`Avatar.svelte`.

## 8. Security model

- **Self-verifying integrity:** every fetched chunk is checked `hash(bytes) == CID` before
  admit (§5.3). A malicious server cannot poison the cache.
- **No private leakage:** only `Sensitivity::Public` content is served (§5.2). Private/
  confidential content is never answered.
- **Avatar authenticity:** `avatar_cid` is inside the device-#2-signed card; a peer can't
  forge an avatar onto another owner's card.
- **Availability ≠ trust:** because integrity is guaranteed by the CID, *any* peer can serve
  public content. Re-serve from cache is therefore safe and improves availability.

## 9. Phased decomposition

- **Phase 0 — Proof slice (hard gate).** Minimal serve queryable (serve any locally-held CID)
  + `CasOp::GetLocal` + a two-node integration test (mirroring `two_engines_exchange_via_iroh_zenoh`):
  node A ingests a blob, node B fetches it by CID and receives the verified bytes. Must be
  green before proceeding — this kills the "does Zenoh GET work p2p at all?" unknown.
- **Phase 1 — Harden into the public-tier primitive.** Add the `Sensitivity::Public` servable-set
  filter; add verify-on-fetch (`hash==CID`); serve-path bounds. Tests: private content is *not*
  served; tampered bytes are rejected.
- **Phase 2 — Card field.** `avatar_cid` on `ProfileCardBroadcast`; wire-format fixture;
  sign/verify round-trip + backward-compat (no-avatar byte-identity).
- **Phase 3 — Upload pipeline.** Frontend pick → canvas normalize (256² PNG, input cap) →
  ingest as public bundle → `republish_owner_card` with CID → self-seed.
- **Phase 4 — Render.** `MemberCardService` avatar resolution → `Avatar.svelte` in MemberRow /
  ChannelMessageFeed / ProfilePopover; identicon fallback.
- **Phase 5 — Cross-peer e2e + gate sweep + PR.**

**Natural PR split:** Phases 0–1 (the general CAS-serve primitive + p2p proof) are
independently mergeable and valuable to every CAS consumer — so this likely ships as **two
PRs**: the primitive first, then avatars (Phases 2–5). The implementation plan firms this up.

## 10. Testing strategy

- **Phase 0:** two-node fetch-by-CID integration test (the proof).
- **Phase 1:** serve-policy tests (public served / private not); verify-on-fetch (tampered
  bytes dropped, not cached).
- **Phase 2:** wire-format fixture (avatar + no-avatar) + sign/verify round-trip + backward-compat.
- **Phase 3:** frontend upload/normalize vitest (canvas resize bound, input cap, format).
- **Phase 4:** `MemberCardService` avatar-resolution vitest; render/fallback component tests.
- **Phase 5:** cross-peer e2e (two owners: A publishes avatar card, B verifies + resolves +
  renders).
- **Gates (every phase):** fmt / clippy / nextest / large-tests / MSRV / frontend tsc + vitest.

## 11. Scope

**In scope:** the general public-tier CAS-serve primitive (serve queryable + `Sensitivity::Public`
filter + verify-on-fetch); the two-node p2p proof; `avatar_cid` on the signed owner card; avatar
upload (pick → normalize → ingest → republish); render across MemberRow / ChannelMessageFeed /
ProfilePopover; identicon fallback; self-seed.

**Out of scope (this cut):**

- Long-form profile page (`profile_page_root`) — reserved slot, the next CAS consumer.
- New DM-avatar UI — the existing `nav-service` avatar path lights up *for free* once the serve
  primitive lands (it uses the same fetch), but no new DM avatar UI is added here.
- iroh / large-file transport — Zenoh GET is sufficient for small avatars; revisit for
  file-sharing.
- Avatar moderation, animated avatars, avatar history/versioning.
- Disk-persisted public-servable-set across restarts beyond what the content index already
  persists.

## 12. CAS extensibility (forward-look)

This primitive is the foundation the card wire format was designed for. After avatars:

- **`profile_page_root: Option<[u8; 32]>`** (reserved serde `"pp"`) — a long-form "personal
  page" as a public CAS bundle root, resolved by the *same* serve primitive.
- **Peer-to-peer file-sharing** — public file bundles served by the same queryable; large
  blobs may motivate the iroh transport path behind the same serve/fetch interface.

Each reuses the serve primitive unchanged; only the consumer + (for files) possibly the
transport differ.

## 13. Open questions / risks

- **Chunk-level public membership (§5.2):** exact representation of "which chunk CIDs are
  publicly servable" — a plan-level detail. Avatars (single-chunk) don't stress it; the general
  primitive does.
- **Zenoh GET semantics under multiple responders:** with public re-serve, several peers may
  answer the same CID query. `fetch_via_zenoh` already consumes the first successful reply;
  confirm consolidation/dedup behavior in the two-node → N-node case (Phase 0/1 validates).
- **Serve-path load:** a popular avatar could draw many GETs. Bounded by small payloads + cache
  hits; revisit rate-limiting if it surfaces (out of scope unless observed).
