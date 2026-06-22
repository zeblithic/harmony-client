# ZEB-541 — Custom / CAS-backed emoji reactions (Reactions Spec 3) — Design

**Status:** approved (Jake, 2026-06-22)
**Parent:** ZEB-533 (Harmony fleet collaboration features)
**Builds on:** ZEB-536 (reaction event/log + LWW + UI, PRs #314/#318) · ZEB-535/#312/#313/#317 (channel attachment CAS) · ZEB-540/#319 (in-memory artifact preview + blob lifecycle + decode-bomb guards)

## Goal

Let a member react to a channel message with a **custom image emoji** beyond the
fixed unicode palette (👍/👎/✅/❌/👀/🎉/🙏/🚀/❤️/😄). v1 shape (Jake, 2026-06-22):
**one-off image upload at react-time** — pick an image in the reaction picker, it is
normalized to a small PNG, stored in the channel's CAS (encrypted, content-addressed),
and the signed `React` event references its CID. Other members resolve the CID → image
in the reaction chip, falling back gracefully until the blob is fetched.

Out of scope for v1 (deferred follow-ups): per-community **named** emoji sets/registry
(`:partyparrot:`), and **animated** (GIF/APNG) emoji.

## Core insight

A custom emoji is structurally identical to a channel attachment: a small encrypted
CAS blob referenced by a **signed, power-gated** channel-log event. The `React` event
is already signed and verified through the same `write_power` gate as `Post`
(`community_channel_log.rs::verify_channel_event`), so a CID it references is
self-authorizing exactly the way a `Post` attachment's CID is. This lets us **reuse the
entire ZEB-540 serve/preview pipeline** (`authorize_and_fetch_artifact` →
`decrypt_and_verify_artifact` → blob URL → decode-bomb-guarded `<img>`) instead of
building a new subsystem. The feature reduces to: one additive optional wire field,
extending one CID scan, one tight-capped preview IPC, a bytes-ingest entry point, and
frontend picker + image-chip render reusing the existing blob lifecycle.

## Decisions

1. **Wire model:** extend the existing `React` event with **one additive optional field**
   `emoji_attachment: Option<ChannelAttachment>` (reusing the existing attachment
   descriptor `{cid, mime, name, size}`; `name` is empty for emoji). Not a new event kind.
2. **Backward/forward compatibility:** `#[serde(skip_serializing_if = "Option::is_none", default)]`
   so unicode reactions serialize **byte-identically** to today (existing
   `react_packet_is_byte_stable` fixture stays green; old/new peers interop for unicode).
3. **Static PNG only (v1):** `normalizeEmoji` re-encodes to a static PNG, reusing the
   avatar PNG/JPEG decode-bomb guards verbatim. Animated is a clean future extension.
4. **Encryption:** emoji blob is stored encrypted with the per-space epoch key and
   `serveable: true`, identical to encrypted channel attachments.
5. **Authorization:** serve/preview authorizes the emoji CID by confirming a signed
   `React` event in the channel log references it (extend the existing attachment scan).
6. **Server-side hard cap:** a dedicated `preview_reaction_emoji` IPC caps the
   in-memory fetch at `MAX_CUSTOM_EMOJI_BYTES` (256 KiB) **server-side**, independent of
   any frontend-supplied bound.

## Wire format (Rust — `src-tauri/src/community_channel_log.rs`)

Extend three coupled definitions, all in **canonical CBOR key order** (RFC 8949
bytewise). The existing React keys are `ad, at, au, ch, ci, em, id, sg`. The new key
`ea` (emoji attachment) sorts between `ci` and `em`, so declare the field in that
position in each struct.

`SignedChannelEvent::React` (currently ~lines 273–300) gains:
```rust
#[serde(rename = "ea", skip_serializing_if = "Option::is_none", default)]
emoji_attachment: Option<ChannelAttachment>,
```
declared after `community_id` (`ci`) and before `emoji` (`em`).

`ChannelReactPayload` (~409–417) gains `pub emoji_attachment: Option<ChannelAttachment>`
and `ChannelReactSignedSet<'a>` (~422–436) gains the corresponding borrowed field — so
the CID is covered by the signature (tamper-proof reaction→emoji binding). `sign_channel_react`
(~542) threads it through.

`ChannelAttachment` (~203–216) is reused unchanged: `{cid:[u8;32]("cd"), mime("mi"),
name("nm"), size("sz")}`.

## Verification (`verify_channel_event`)

React currently asserts it carries **no** attachments (~line 1004) and caps the unicode
`emoji` string at `MAX_REACTION_EMOJI_BYTES` (158 bytes). Change for React:

- Allow **at most one** `emoji_attachment`.
- Cheap pre-auth caps when present: `emoji_attachment.size <= MAX_CUSTOM_EMOJI_BYTES`
  (256 KiB) and `emoji_attachment.mime` is an image type (`image/...`). Reject otherwise
  with a clear error (mirrors the existing emoji-size pre-auth check).
- A custom-emoji React MAY carry an empty `emoji` string (the unicode grouping key is
  unused for customs; grouping is by CID — see Materialization).
- Power/signature/membership gate is **unchanged** (same `write_power` path as Post/React
  today). The signature now covers `emoji_attachment`.

## Authorization + serve (`src-tauri/src/lib.rs`, reusing ZEB-540)

- **Extend the attachment scan.** `attachment_with_cid` (~line 204) and
  `find_attachment` (`community_channel_log_engine.rs` ~708–760) currently match only
  `Post` attachments (React explicitly filtered). Extend them to also yield a `React`
  event's `emoji_attachment` when its CID matches. This is the single change that makes
  emoji CIDs serve-authorizable to anyone who can read the channel.
- **New thin IPC** `preview_reaction_emoji(community_id, channel_id, cid) -> Result<Vec<u8>, String>`
  + `preview_reaction_emoji_impl`, registered in `generate_handler!`. Implementation reuses
  `authorize_and_fetch_artifact(state, community_id, channel_id, cid, MAX_CUSTOM_EMOJI_BYTES)`
  then `decrypt_and_verify_artifact(...)`. The 256 KiB ceiling is hard-coded server-side;
  it does **not** trust a frontend-supplied cap. (Doc comment: `/// ZEB-541 IPC seam ...`.)
- Anti-OOM defense-in-depth: write-time `size <= cap` in verify + content-addressing
  (CID = hash of bytes) + the 256 KiB **fetch** cap + the `decrypt_and_verify` length
  check together bound the render path regardless of a malicious signer.

## Ingest (frontend-normalized bytes → encrypted CAS)

- **Refactor** the path-based `ingest_channel_artifact_impl` (~20505–20631) to share a
  bytes-taking inner, and add a bytes entry point
  `ingest_channel_artifact_bytes(community_id, bytes, name, mime, encrypt) -> ChannelAttachmentDto`
  (encrypts with the per-space epoch key, `serveable: true`, authoritative `size` =
  plaintext len — same as the file path). The emoji ingest calls this with
  `encrypt = true`, `mime = "image/png"`, `name = ""`.
- **Frontend `normalizeEmoji(file): Promise<Uint8Array>`** (new, in `avatar-normalize.ts`
  or a sibling `emoji-normalize.ts`, sharing the exported guards):
  - Reuse `validateAvatarInput`-style input gate (`<= AVATAR_MAX_INPUT_BYTES` 10 MiB, must
    be image).
  - `assertHeaderDimsOk(bytes)` (pre-decode bomb guard) → `createImageBitmap` →
    `assertDecodedDimsOk(w,h)` (post-decode, `<= AVATAR_MAX_DECODED_DIM` 8192).
  - Resize **contain-fit** (preserve aspect, no crop) to fit within **128×128**, draw on a
    transparent canvas, `toBlob('image/png')` → `Uint8Array`.
  - Identical normalized bytes → identical CID → automatic dedup of popular emoji
    (CAS books are keyed by content hash).

## Materialization + DTO

- `ReactionIndex` (`community_channel_log.rs` ~821–899) keys reactions by a grouping key:
  unicode → the `emoji` string (unchanged); custom → a CID-derived key
  (e.g. a non-emoji-collidable `"\u{0}cid:" + hex(cid)`). The index retains the emoji
  `ChannelAttachment` descriptor per custom key (identical across reactors by CID identity)
  so the DTO can surface it.
- `ReactionDto` (~824–831) gains `emoji_cid: Option<String>` (hex) and `emoji_size: Option<u64>`.
  Unicode reactions: `emoji_cid = None`, behavior unchanged.
- Frontend `ChannelMessageDto.reactions` item type (`channel-message-service.ts`) gains
  `emojiCid?: string` (+ `emojiSize?: number`).

## Frontend UI (`ChannelMessageFeed.svelte` + service)

- **Picker custom affordance:** the existing emoji picker gains a "custom" button →
  `@tauri-apps/plugin-dialog` `open({multiple:false})` → `normalizeEmoji` → ingest bytes →
  `reactToMessage(..., emojiCid=cid, add=true)`. Reuse the channel-switch epoch guard
  pattern already in the compose path during the async pick/ingest.
- **Service:** `reactToMessage(communityId, channelId, messageId, emoji, add, emojiCid?)`
  gains the optional CID; passes it (with the descriptor needed to build
  `emoji_attachment`) to `set_message_reaction`. `set_message_reaction(_impl)` gains an
  optional emoji descriptor param (cid hex + mime + size); engine `react()` builds the
  `emoji_attachment` and signs it. (Size is frontend-supplied from ingest but capped in
  verify, and content-bound by the CID — same trust model as Post attachments.)
- **Chip render:** `{#if r.emojiCid}<img class="reaction-emoji-img">{:else}{r.emoji}`. The
  `<img>` resolves its blob URL via `preview_reaction_emoji`, reusing the **exact blob-URL
  lifecycle discipline from ZEB-540 `MessageAttachments.svelte`**: per-cid state, `isLive`
  post-await guards, revoke on unmount (`$effect` cleanup), dedup per cid, decode-bomb
  guards before/after `createImageBitmap`. Factor a small reusable emoji-image resolver/
  component so the feed isn't duplicated. Graceful fallback (a neutral placeholder chip)
  until the blob resolves or if it fails.
- Toggling an existing custom reaction chip toggles the local owner's reaction for that
  CID (`reactToMessage` with the same `emojiCid`).

## Caps (constants)

| Constant | Value | Where |
| --- | --- | --- |
| Source input (pre-normalize) | 10 MiB | reuse `AVATAR_MAX_INPUT_BYTES` (frontend) |
| Decoded dim guard | 8192 px | reuse `AVATAR_MAX_DECODED_DIM` (frontend) |
| Normalized emoji edge | 128 px (contain) | `EMOJI_EDGE` (frontend) |
| Stored/served emoji blob | 256 KiB | `MAX_CUSTOM_EMOJI_BYTES` (Rust, verify + preview) |

A 128×128 PNG is typically well under 64 KiB; 256 KiB is generous headroom.

## Security

- Authorize-first: `preview_reaction_emoji` rejects a CID not referenced by any signed
  React in the channel log before any fetch (reuses `authorize_and_fetch_artifact`).
- Server-side cap (256 KiB) is independent of frontend input; combined with
  content-addressing and the decrypt length check, bounds the in-memory render path.
- Signature covers `emoji_attachment` → a peer cannot rebind a reaction to a different
  emoji CID without invalidating the signature.
- Decode-bomb guards (header dims pre-decode + decoded dims post-decode) applied on the
  render path exactly as ZEB-540; a byte cap alone does not stop a decompression bomb.
- SVG remains excluded (vector; raster guards don't bound it) — normalize re-encodes to
  PNG regardless, so non-PNG/JPEG inputs that decode are re-emitted as bounded PNG.

## Testing

**Rust**
- Wire fixtures (`tests/wire_format/channel_log_fixtures.rs`): existing
  `react_packet_is_byte_stable` (unicode) **must stay green** (proves additive-field
  byte-stability); add `react_packet_with_emoji_attachment_is_byte_stable` (new pinned hex).
- `verify_channel_event`: rejects emoji `size > MAX_CUSTOM_EMOJI_BYTES`; rejects non-image
  mime; rejects more than one emoji attachment; accepts a valid custom-emoji React.
- `find_attachment`/authorize: a React's `emoji_attachment` CID is found and authorized;
  an unreferenced CID is rejected.
- Engine `react()` roundtrip carrying an `emoji_attachment`; `ReactionIndex` groups two
  reactors of the same custom emoji by CID and emits `emoji_cid` in the DTO.
- Two-engine integration: engine A reacts with a custom emoji; engine B materializes the
  `ReactionDto` with `emoji_cid` and can `preview_reaction_emoji` the bytes.
- `preview_reaction_emoji_impl`: rejects overlong/invalid CID hex; rejects unauthorized
  CID; enforces the 256 KiB cap.

**Frontend (vitest)**
- `normalizeEmoji`: header/decoded dim guards fire; output is PNG; resizes within 128×128;
  rejects oversize/non-image input.
- service `reactToMessage` with `emojiCid` invokes `set_message_reaction` with the cid.
- chip render: unicode path unchanged; custom path renders `<img>`; blob URL revoked on
  unmount and on prop change; teardown-race liveness guard (unmount mid-fetch → no
  `createObjectURL`). Reuse the ZEB-540 `MessageAttachments.test.ts` patterns.

## Files touched

- `src-tauri/src/community_channel_log.rs` — React wire field, payload, signed set, sign,
  verify, ReactionIndex key + descriptor retention, ReactionDto.
- `src-tauri/src/community_channel_log_engine.rs` — `react()` accepts emoji descriptor;
  `find_attachment` scans React; ChannelAttachmentDto unchanged.
- `src-tauri/src/lib.rs` — `MAX_CUSTOM_EMOJI_BYTES`; `ingest_channel_artifact_bytes` (+ shared
  inner); `set_message_reaction(_impl)` optional emoji descriptor; `preview_reaction_emoji(_impl)`;
  `attachment_with_cid` React arm; `generate_handler!` registrations.
- `src-tauri/tests/wire_format/channel_log_fixtures.rs` — new custom-emoji React fixture.
- `src/lib/avatar-normalize.ts` (or `src/lib/emoji-normalize.ts`) — `normalizeEmoji`, `EMOJI_EDGE`.
- `src/lib/channel-message-service.ts` — `reactToMessage` cid param; `ingestArtifactBytes`/
  emoji ingest facade; `previewReactionEmoji`; DTO type additions.
- `src/lib/components/ChannelMessageFeed.svelte` — picker custom affordance, image chip,
  blob lifecycle (factor a reusable emoji-image resolver/component).
- Test files alongside the above.

## Out of scope (future)

- Per-community **named** custom emoji registry/management UI (`:name:`), with a registry
  CRDT and name→CID dedup.
- **Animated** (GIF/APNG) emoji (carry real mime, skip re-encode, animated decode bounds).
- Reaction-emoji garbage collection / unreferenced-blob reaping.
