# Long-form profile page over CAS (`profile_page_root`) — design

**Status:** approved (brainstormed 2026-05-31)
**Author:** Jake (J Eng) + Claude
**Predecessor:** ZEB-343 (CAS-serve primitive + profile avatars, merged PR #172, main `af1f1a5`)
**Linear:** ZEB-345 (single PR)

## 1. Summary

Add a **long-form profile page** to Harmony as the **second consumer** of the peer-to-peer
CAS-serve primitive that ZEB-343 proved. A member can author a `bio` (plain text), a list
of `links` (label + URL), and a set of typed `fields` (key/value). That content is
serialized to a canonical CBOR **profile document**, stored as a public content-addressed
object, and referenced from the same device-signed `ProfileCardBroadcast` via a new
additive field `profile_page_root: Option<[u8; 32]>` (serde `"pp"`) — exactly mirroring how
ZEB-343 added `avatar_cid` (`"av"`).

Peers fetch the document by CID over the **existing** ZEB-343 public-serve queryable (zero
new transport/serve code), verify it (`hash == CID`), and render it in a **right-side
profile panel** that preserves the surrounding community/channel context. The document is
fetched **lazily** — only when a profile is actually opened, never eagerly per member.

This is the architectural sibling of avatars: the card stays a tiny index of CIDs; the
weight lives in CAS and is pulled on demand.

## 2. Problem & context

ZEB-341 resolved a member's **display name + status** by `owner_id`; ZEB-343 added the
**avatar** and built the CAS p2p transport under it. The card today carries only
`display_name`, `status_text`, `avatar_cid`. There is no place for a richer "who is this
person" view — the `ProfilePopover` is a small hover/click card, and clicking a member has
nowhere deeper to go.

Both predecessors deliberately reserved this slot. ZEB-341's spec named
`profile_page_root: Option<[u8;32]>` as a future additive card field; ZEB-343's out-of-scope
list named the "long-form profile page" as the **next CAS consumer**. This ticket fills
that slot.

**Design decisions (brainstormed + approved 2026-05-31):**

1. **Shape** — full CAS-backed long-form profile (a `profile_page_root` CID on the card),
   not inline card fields.
2. **Content model** — `bio` (plain text) + `links` (label + URL) + `fields` (key/value).
   Text-only in v1.
3. **Visibility** — **public** / `PublicDurable` (unencrypted, leading-bit-zero), so it is
   served by the ZEB-343 public-serve queryable with **no new serve code**. Accepts the CAS
   reshare/permanence property (a fetched public object can be cached + re-served by any
   peer; "delete" only orphans the CID).
4. **Identity** — **global**: one document per `owner_id`, the same in every community. The
   card wire stays additive so a future *per-community override* is a non-breaking change.
5. **Bio rendering** — **plain text, escaped** (no markdown/HTML parsing). URLs live in the
   structured `links` field, scheme-allowlisted to `https:` and `harmony:`.
6. **Build approach** — **mirror the avatar pattern** (profile-specific resolver/ingest/codec).
   Rule of three: extract a shared CAS-document primitive only when a third consumer lands.
7. **Render surface** — **right-side profile panel** (third column), lazy on-demand fetch.

## 3. Wire format — additive card field

In `profile_card_broadcast.rs`, add one field to `ProfileCardBroadcast`, placed immediately
after `avatar_cid` so the two optional CIDs sit together:

```rust
#[serde(
    rename = "pp",
    default,
    skip_serializing_if = "Option::is_none",
    serialize_with = "crate::owner_state_types::serialize_optional_bytes_as_bstr",
    deserialize_with = "crate::owner_state_types::deserialize_optional_bytes_from_bstr"
)]
pub profile_page_root: Option<[u8; 32]>,
```

Identical attrs/helpers to `avatar_cid`. Because `skip_serializing_if` omits it when `None`,
the canonical CBOR is **byte-identical** to a ZEB-343 card whenever the page root is absent.
CBOR map-header matrix (required fields `oi/dn/st/en/sa/sg` = 6):

| `avatar_cid` | `profile_page_root` | map header | compat |
|---|---|---|---|
| none | none | `0xA6` | byte-identical to ZEB-341 cards |
| set  | none | `0xA7` | byte-identical to ZEB-343 avatar cards |
| none | set  | `0xA7` | new (different keys, same arity) |
| set  | set  | `0xA8` | new |

Threading (one field over from `avatar_cid`, every site it touches):

- `sign_card(...)` gains a `profile_page_root: Option<[u8; 32]>` param (after `avatar_cid`).
  No new length check (fixed-size CID). `verify_card` needs **no change** — it re-encodes the
  whole struct with the signature zeroed, so `pp` is covered automatically.
- `CachedCard` gains `profile_page_root: Option<[u8; 32]>`.
- `DiscoveredCardInfo` gains `#[serde(rename = "profilePageRoot", skip_serializing_if = "Option::is_none")] profile_page_root: Option<String>` (hex).
- `get_cached` hex-encodes `profile_page_root` like `avatar_cid`.

## 4. CAS document codec — `profile_page_doc.rs` (new)

A canonical-CBOR, versioned document. Encode authority lives **in Rust** so the CID is the
hash of canonical bytes (JS never serializes it — see §6).

```rust
pub const MAX_BIO_BYTES: usize = 4_096;
pub const MAX_LINKS: usize = 10;
pub const MAX_LINK_LABEL_BYTES: usize = 64;
pub const MAX_LINK_URL_BYTES: usize = 512;
pub const MAX_FIELDS: usize = 16;
pub const MAX_FIELD_KEY_BYTES: usize = 32;
pub const MAX_FIELD_VALUE_BYTES: usize = 256;
pub const MAX_PROFILE_DOC_BYTES: usize = 16_384;
pub const PROFILE_DOC_VERSION: u8 = 1;
/// URL scheme allowlist for links.
const ALLOWED_LINK_SCHEMES: [&str; 2] = ["https://", "harmony:"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfilePageDoc {
    #[serde(rename = "vn")] pub version: u8,
    #[serde(rename = "bo")] pub bio: String,
    #[serde(rename = "ln")] pub links: Vec<ProfileLink>,
    #[serde(rename = "fl")] pub fields: Vec<ProfileField>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileLink {
    #[serde(rename = "lb")] pub label: String,
    #[serde(rename = "ur")] pub url: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileField {
    #[serde(rename = "ky")] pub key: String,
    #[serde(rename = "vl")] pub value: String,
}
```

- `validate_profile_doc(&doc) -> Result<(), ProfileDocError>` — enforce **every** cap: bio
  bytes, link count + per-link label/url bytes, field count + per-field key/value bytes,
  every link URL `starts_with` an allowed scheme, and total canonical-encoded bytes
  ≤ `MAX_PROFILE_DOC_BYTES`. Each violation is a distinct error variant.
- `encode_profile_doc(&doc) -> Result<Vec<u8>, _>` — `canonical_cbor_encode` (declaration-order
  keys, *not* sorted — same canonical encoder as the card). Validates first.
- `decode_profile_doc(&[u8]) -> Result<ProfilePageDoc, _>` — reject `bytes.len() > MAX_PROFILE_DOC_BYTES`
  up front, parse, reject unknown `version`, then re-run `validate_profile_doc` so a fetched
  doc is bound by the same caps as an ingested one.

The document is **not an image**, so it has no decompression-bomb vector — its only abuse
axis is byte size, fully bounded by `MAX_PROFILE_DOC_BYTES` on both ingest and fetch.

## 5. Storage & serve — reuse, no new code

`ingest_profile_doc` stores the canonical bytes as a **`PublicDurable`** object (default
`ContentFlags`, leading bit clear), so its CID self-attests as serveable. The ZEB-343
public-serve queryable (`!cid.flags().encrypted` gate, inline reply, re-verify `hash == cid`)
serves it unchanged. Verify-on-fetch (`hash == CID`) from ZEB-343 T4 protects the fetch path.
**No transport, queryable, or serve-gate changes.**

## 6. Backend IPCs

Twins of the avatar IPCs (`ingest_avatar_bytes`, `republish_owner_card`):

- **`ingest_profile_doc(bio: String, links: Vec<LinkInput>, fields: Vec<FieldInput>) -> Result<String, String>`**
  Build `ProfilePageDoc { version: PROFILE_DOC_VERSION, .. }`, `validate_profile_doc`,
  `encode_profile_doc`, ingest via the streaming-ingest path used by `ingest_avatar_bytes`
  with default (`PublicDurable`) flags, return the CID hex. (Caller only invokes this when at
  least one of bio/links/fields is non-empty — see §7.)
- **`publish_owner_card` / `republish_owner_card`** gain a `profile_page_root: Option<String>`
  (hex) param, threaded into `sign_card` alongside `avatar_cid`. Decode hex → `[u8; 32]`
  **before** the Reticulum-side commit (same ordering care as the ZEB-343 round-2 avatar fix:
  a malformed-CID `?` must not fire after a partial publish).
- **`fetch_profile_doc(cid: String) -> Result<ProfilePageDocDto, String>`**
  Validate hex; fetch bytes via the existing `FetchRequest`/`fetch_content` path; reject
  `bytes.len() > MAX_PROFILE_DOC_BYTES`; `decode_profile_doc` (which re-validates caps);
  map to a camelCase DTO. Any failure → `Err` (frontend falls back to "no page"). Parsing
  the untrusted CBOR happens **only here, in Rust** — JS receives a structured, validated DTO.

```rust
pub struct ProfilePageDocDto {            // serde camelCase
    pub bio: String,
    pub links: Vec<ProfileLinkDto>,       // { label, url }
    pub fields: Vec<ProfileFieldDto>,     // { key, value }
}
```

## 7. Frontend

**`ProfilePageResolver`** (`src/lib/profile-page-resolver.ts`) — twin of `AvatarResolver`,
but **lazy and DTO-valued**:
```
resolve(cid) → cached DTO ?? (kick off invoke('fetch_profile_doc', { cid }))
            → on success: cache DTO by CID, fire onChange
            → on failure: 30s retry-cooldown, resolve() returns undefined
```
It is **not** wired into `MemberCardService`'s eager per-member loop. The profile panel calls
`resolve(root)` when it opens. `destroy()` clears the cache (DTOs, no blob URLs to revoke).

**Right-side `ProfilePanel.svelte`** — opened via an `openProfileOwnerId` state in `App.svelte`.
Given an `ownerIdHex`, it reads the resolved card (`displayName`, `statusText`, `avatarUrl`,
`ownerIdHex`, `profilePageRoot`) and, if `profilePageRoot` is set, the resolved doc DTO. Layout
matches the approved mockup: header (avatar, name, status, copyable owner id) + About (bio) +
Links + Fields. No `profilePageRoot`, or doc still resolving / failed → header only ("no page
content"). Entry point: `ProfilePopover` (owner-card mode) gains a **"View full profile"** action
that sets `openProfileOwnerId`.

**`ProfileEditor.svelte`** gains an **"About"** section below the identity fields: a `bio`
`<textarea>`, a repeatable **links** list (label + URL rows, add/remove), a repeatable **fields**
list (key + value rows, add/remove). On save:
```
if (bio || links.length || fields.length)
    root = await invoke('ingest_profile_doc', { bio, links, fields })
else
    root = undefined                        // empty profile → no doc, card byte-identical
// root flows into the existing save path → republish_owner_card({ ..., profile_page_root: root })
```
Per-field length counters mirror the existing display-name/status affordances. (Accessibility:
the bio textarea + counters; link/field rows are standard inputs — no sliders here.)

**`App.svelte`** wiring (mirrors the ZEB-343 avatar wiring): construct + `connectAdapter` the
`ProfilePageResolver`; `openProfileOwnerId` state renders `<ProfilePanel>`; `handleProfileSave`
+ `publishProfileToNetwork` carry `profile_page_root` (a CID hex — no `blob:` sanitization
concern, unlike avatars); `republishOwnerCard` sends `profilePageRoot`; self-seed so the owner
sees their own page immediately.

## 8. Render safety

- **Bio** — rendered as text (Svelte `{bio}` auto-escapes) with `white-space: pre-wrap` for
  newlines. No HTML, no markdown.
- **Links** — `<a href={url} rel="noopener noreferrer">`. `https:` opens externally; `harmony:`
  is dispatched through the **existing ZEB-338 deep-link router** (`preventDefault` →
  in-app navigation). Defense-in-depth: the frontend re-checks the scheme allowlist before
  building the `href`, even though `fetch_profile_doc` already rejected non-allowlisted schemes.
- **Fields** — escaped `key: value` rows.

## 9. Backward-compat & testing

- **Card wire fixtures** — pin `0xA7` (pp-only) and `0xA8` (avatar + pp); assert a no-page card
  is byte-identical to the ZEB-343 fixture (`0xA6`/`0xA7`).
- **`profile_page_doc` unit tests** — canonical round-trip; a pinned v1 byte fixture; a
  rejection test for **each** cap dimension (bio, link count, label, url, field count, key,
  value, total bytes); scheme-allowlist rejection; unknown-version rejection.
- **IPC tests** — `ingest_profile_doc` rejects over-cap input; `fetch_profile_doc` rejects an
  over-cap / malformed / wrong-version doc and a byte blob exceeding `MAX_PROFILE_DOC_BYTES`.
- **Cross-peer integration** (`profile_page_cross_peer_integration.rs`) — node A ingests a doc +
  publishes a card with `profile_page_root`; node B fetches the doc by CID and the decoded DTO
  matches A's input. Mirrors the ZEB-343 avatar cross-peer test.
- **Frontend** — `profile-page-resolver.test.ts` (lazy resolve → fetch → DTO cache, cooldown,
  no eager fetch); `ProfileEditor` about-section (ingest called only when non-empty; empty →
  `profile_page_root` undefined); `ProfilePanel` render (bio escaped + newlines, link scheme
  split, fields); `member-card-service` `profilePageRoot` threading.
- All gates green (fmt / clippy / nextest / large-tests / MSRV / frontend).

## 10. Phased decomposition (task ordering within the one PR)

- **T0** — pre-flight baseline (nextest + frontend green on the fresh branch).
- **T1** — `pp` wire field + `sign_card` param + card threading (`CachedCard`/`DiscoveredCardInfo`/`get_cached`) + wire fixtures (incl. byte-identical-when-`None`).
- **T2** — `profile_page_doc.rs` codec + caps + `validate`/`encode`/`decode` + unit tests + canonical fixture.
- **T3** — `ingest_profile_doc` IPC.
- **T4** — `fetch_profile_doc` IPC (fetch + byte cap + decode → DTO).
- **T5** — `publish`/`republish_owner_card` thread `profile_page_root`.
- **T6** — cross-peer integration test (author → fetch → DTO match).
- **T7** — `ProfilePageResolver` (frontend, lazy).
- **T8** — `ProfilePanel.svelte` right-side surface + `ProfilePopover` "View full profile" entry.
- **T9** — `ProfileEditor` "About" section (bio / links / fields editor).
- **T10** — `App.svelte` wiring (open state + resolver + panel + save→ingest→republish + self-seed).
- **T11** — render safety (escaping, link scheme split, `harmony:` deep-link dispatch) + tests.
- **T12** — final gate sweep + push + PR.

## 11. Out of scope

- Per-community profile override (the wire is kept additive for it).
- Markdown / rich-text bio; embedded media inside the doc; `avatar_mini_cid` thumbnail tier.
- Profile version history (latest-wins; the old CID is orphaned and ages out of the W-TinyLFU
  CAS cache, per ZEB-344).
- The receive-side caps **retrofit for the avatar path** stays ZEB-344. This ticket applies the
  byte cap to the *document* path from day one; the avatar image-decode guard is ZEB-344's.
