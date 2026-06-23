# ZEB-541 — Custom / CAS-backed emoji reactions — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to
> implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a member react to a channel message with a one-off custom image emoji,
stored as a small encrypted CAS blob referenced by the signed `React` event, rendered
inline in the reaction chip.

**Architecture:** Custom emoji = small encrypted CAS blob referenced by a signed,
power-gated `React` event (structurally identical to a channel attachment). Reuse the
ZEB-540 authorize/preview/blob-lifecycle pipeline. One additive optional wire field keeps
unicode reactions byte-stable.

**Tech Stack:** Rust (Tauri backend, ciborium canonical CBOR, ed25519), Svelte 5 runes
frontend, vitest.

**Spec:** `docs/specs/2026-06-22-zeb-541-custom-cas-backed-emoji-reactions-design.md`

**Gate (run from repo root unless noted):**
- Backend (from `src-tauri/`): `cargo fmt --all -- --check`;
  `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`;
  `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
- Frontend (from repo root): `npx tsc --noEmit`; `npx vitest run`.

**Code-map seam references** (origin/main `9f630bed`; line numbers approximate, confirm before editing):
- React event: `src-tauri/src/community_channel_log.rs` ~273–300 (`SignedChannelEvent::React`),
  ~409–417 (`ChannelReactPayload`), ~422–436 (`ChannelReactSignedSet`), ~542 (`sign_channel_react`).
- Verify: same file ~981–1161 (`verify_channel_event`; React arm ~1004, emoji cap ~1068–1075,
  power gate ~1098–1152), `MAX_REACTION_EMOJI_BYTES` ~160.
- ReactionIndex/DTO: same file ~821–899 (`ReactionIndex`, `ReactionEmojiMap`, `ReactionDto`,
  `reactions_for`).
- Attachment wire: same file ~203–216 (`ChannelAttachment`), `attachment_with_cid` ~204.
- Engine: `src-tauri/src/community_channel_log_engine.rs` ~1019–1078 (`react`), ~708–760
  (`find_attachment`), ~182–188 (`ChannelAttachmentDto`).
- lib.rs: `set_message_reaction` ~20163, `_impl` ~20333; `ingest_channel_artifact` ~20143,
  `_impl` ~20505–20631; `authorize_and_fetch_artifact` ~20727–20810; `decrypt_and_verify_artifact`;
  `preview_channel_artifact(_impl)` ~20127/20932; `MAX_PREVIEW_BYTES`/`MAX_ARTIFACT_BYTES` ~20389–20391;
  `generate_handler!` ~47339.
- Fixtures: `src-tauri/tests/wire_format/channel_log_fixtures.rs` ~227–262 (react),
  ~94–131 (post-with-attachments). Re-pin via `UPDATE_BACKFILL_FIXTURE=1`.
- Avatar: `src/lib/avatar-normalize.ts` (guards + `normalizeAvatar`); `ingest_avatar_bytes_inner`
  lib.rs ~12838.
- Reaction UI/service: `src/lib/components/ChannelMessageFeed.svelte` (~111–112 palette,
  ~427–449 toggle/pick, ~595–627 render); `src/lib/channel-message-service.ts` ~504–527
  (`reactToMessage`), ~9–51 (`ChannelMessageDto`); ZEB-540 blob lifecycle reference:
  `src/lib/components/MessageAttachments.svelte`, `src/lib/artifact-preview.ts`.

---

## Task 1: Backend wire field + verify gate + fixture

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs`
- Modify: `src-tauri/src/lib.rs` (add `MAX_CUSTOM_EMOJI_BYTES`)
- Modify/Test: `src-tauri/tests/wire_format/channel_log_fixtures.rs`

- [ ] **Step 1: Add the constant.** In `lib.rs` near `MAX_PREVIEW_BYTES`/`MAX_ARTIFACT_BYTES`:
  ```rust
  /// Hard cap on a custom reaction emoji blob (plaintext). Tiny by design; a 128x128
  /// PNG is well under this. Enforced at verify (write) and at preview (serve).
  pub(crate) const MAX_CUSTOM_EMOJI_BYTES: u64 = 256 * 1024;
  ```

- [ ] **Step 2: Extend the React wire event.** In `SignedChannelEvent::React`, add the field in
  **canonical key position** (`ea` sorts between `ci` and `em`), declared after `community_id`
  and before `emoji`:
  ```rust
  #[serde(rename = "ea", skip_serializing_if = "Option::is_none", default)]
  emoji_attachment: Option<ChannelAttachment>,
  ```
  Add the matching field to `ChannelReactPayload` (`pub emoji_attachment: Option<ChannelAttachment>`)
  and to `ChannelReactSignedSet<'a>` (borrowed: `emoji_attachment: &'a Option<ChannelAttachment>`),
  each in the same canonical position. Thread it through `sign_channel_react` and anywhere a
  `React`/payload is constructed (compiler will flag exhaustive constructions).
  - NOTE: `skip_serializing_if`+`default` is load-bearing — it keeps unicode reactions
    byte-identical so the existing fixture stays green and old/new peers interop.

- [ ] **Step 3: Verify gate.** In `verify_channel_event`'s React handling: relax the "no
  attachments" assertion to allow **at most one** `emoji_attachment`; when present, cheap
  pre-auth caps (alongside the existing `MAX_REACTION_EMOJI_BYTES` check):
  `emoji_attachment.size <= MAX_CUSTOM_EMOJI_BYTES` and `emoji_attachment.mime` starts with
  `"image/"`. Reject with clear distinct messages (e.g. `"custom emoji exceeds cap"`,
  `"custom emoji must be an image"`). Power/signature/membership gate unchanged.

- [ ] **Step 4: Keep the existing fixture green.** Run `react_packet_is_byte_stable`
  (`cargo nextest run -p harmony-app --features test-fixtures react_packet_is_byte_stable`).
  Expected: PASS unchanged (proves additive-field byte-stability). If it fails, the
  `skip_serializing_if`/`default` or canonical position is wrong — fix before continuing.

- [ ] **Step 5: New fixture.** Add `react_packet_with_emoji_attachment_is_byte_stable` mirroring
  the existing react fixture (deterministic seeds, `sign_channel_react` →
  `encrypt_channel_packet_with_nonce([0x11;12])`), with a `ChannelAttachment` emoji
  (cid `[0xB2;32]` style seed, `mime:"image/png"`, `name:""`, `size: 1024`). Pin hex via
  `UPDATE_BACKFILL_FIXTURE=1` run, then assert without the env var.

- [ ] **Step 6: Unit tests** (in `community_channel_log.rs` test module): verify rejects emoji
  `size > MAX_CUSTOM_EMOJI_BYTES`; rejects non-image mime; rejects two emoji attachments;
  accepts a valid custom-emoji React (signature verifies).

- [ ] **Step 7: Commit, then gate** (fmt + clippy + nextest as above). Commit message subject:
  `feat(emoji-reactions): React event carries optional custom emoji attachment + verify caps`.

---

## Task 2: Backend materialization + DTO

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs` (`ReactionIndex`, `ReactionDto`, `reactions_for`)

- [ ] **Step 1: DTO fields.** Add to `ReactionDto`: `pub emoji_cid: Option<String>` (hex) and
  `pub emoji_size: Option<u64>` (camelCase serde → `emojiCid`/`emojiSize`). Unicode reactions
  leave them `None`.

- [ ] **Step 2: Index grouping key + descriptor retention.** Reactions group by a key that is the
  unicode `emoji` string for unicode reactions and a CID-derived, non-collidable key for customs
  (e.g. `format!("\u{0}cid:{}", hex::encode(cid))`). Retain the emoji `ChannelAttachment`
  descriptor per custom key (identical across reactors by CID identity) so `reactions_for` can
  emit `emoji_cid`/`emoji_size`. Adjust the `ReactionIndex` value type accordingly (e.g. carry
  an `Option<ChannelAttachment>` per key).

- [ ] **Step 3: `reactions_for`.** Emit `emoji`/`emoji_cid`/`emoji_size` from the retained
  descriptor; for customs set `emoji` to the empty string (or the key) and `emoji_cid`=Some(hex),
  `emoji_size`=Some(size). Keep deterministic BTreeMap ordering.

- [ ] **Step 4: Unit tests.** Two reactors of the same custom emoji group into ONE `ReactionDto`
  with `count=2` and the correct `emoji_cid`; a unicode + a custom reaction on the same message
  yield two distinct DTOs; remove (`add=false`) decrements correctly.

- [ ] **Step 5: Commit + gate.** Subject:
  `feat(emoji-reactions): materialize custom emoji reactions (emojiCid/emojiSize DTO)`.

---

## Task 3: Backend engine.react + IPC param

**Files:**
- Modify: `src-tauri/src/community_channel_log_engine.rs` (`react`)
- Modify: `src-tauri/src/lib.rs` (`set_message_reaction`, `_impl`)

- [ ] **Step 1: Engine signature.** Extend `react(...)` to accept an optional emoji descriptor
  (`Option<ChannelAttachment>` — or `Option<(ContentId/[u8;32], String /*mime*/, u64 /*size*/)>`
  constructed into a `ChannelAttachment` with empty `name`). Build the `ChannelReactPayload` with
  `emoji_attachment`, sign via `sign_channel_react`, append/publish as today. Keep the existing
  `MAX_REACTION_EMOJI_BYTES` emoji-string check.

- [ ] **Step 2: IPC param.** Extend `set_message_reaction` + `_impl` with an optional emoji
  descriptor param. Prefer a single optional struct param to avoid 3 loose optionals:
  ```rust
  #[derive(serde::Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub(crate) struct ReactionEmojiInput { pub cid: String, pub mime: String, pub size: u64 }
  ```
  `_impl` decodes `cid` hex → bytes, validates `mime` starts with `image/` and
  `size <= MAX_CUSTOM_EMOJI_BYTES` (defense-in-depth before signing), builds the
  `ChannelAttachment`, passes to `engine.react`.

- [ ] **Step 2b:** Confirm `set_message_reaction` is already in `generate_handler!` (it is — no
  re-registration needed unless the command name changes; it does not).

- [ ] **Step 3: Unit test.** `set_message_reaction_impl` with a `ReactionEmojiInput`: rejects
  overlong/invalid cid hex; rejects non-image mime; rejects oversize; happy path appends a React
  carrying the emoji attachment (assert via the engine's reaction index / log).

- [ ] **Step 4: Commit + gate.** Subject:
  `feat(emoji-reactions): set_message_reaction accepts optional custom emoji descriptor`.

---

## Task 4: Backend serve/authorize + preview IPC

**Files:**
- Modify: `src-tauri/src/community_channel_log_engine.rs` (`find_attachment` / `attachment_with_cid`)
- Modify: `src-tauri/src/community_channel_log.rs` (if `attachment_with_cid` lives here)
- Modify: `src-tauri/src/lib.rs` (`preview_reaction_emoji`, `_impl`, `generate_handler!`)

- [ ] **Step 1: Extend the CID scan.** `attachment_with_cid` currently matches only `Post`
  attachments (React filtered). Add a `React` arm that returns its `emoji_attachment` when the
  CID matches. `find_attachment` then transparently authorizes emoji CIDs.

- [ ] **Step 2: Preview IPC.** Add:
  ```rust
  #[tauri::command]
  async fn preview_reaction_emoji(state_lock, community_id, channel_id, cid) -> Result<Vec<u8>, String>
  ```
  + `pub(crate) async fn preview_reaction_emoji_impl(...)` that calls
  `authorize_and_fetch_artifact(state, &community_id, &channel_id, &cid, MAX_CUSTOM_EMOJI_BYTES)`
  then `decrypt_and_verify_artifact(...)`. Hard-code the 256 KiB ceiling server-side (do NOT take
  a frontend cap). Doc comment: `/// ZEB-541 IPC seam for inline custom emoji rendering.`
  Register `preview_reaction_emoji,` in `generate_handler!`.

- [ ] **Step 3: Unit tests.** `preview_reaction_emoji_impl`: rejects overlong/invalid cid hex;
  rejects a CID not referenced by any React (`"unknown or unauthorized attachment"`); a React's
  emoji CID is authorized + returns plaintext bytes.

- [ ] **Step 4: Commit + gate.** Subject:
  `feat(emoji-reactions): authorize + preview_reaction_emoji (256 KiB server cap)`.

---

## Task 5: Backend bytes ingest

**Files:**
- Modify: `src-tauri/src/lib.rs` (`ingest_channel_artifact_impl` refactor + `ingest_channel_artifact_bytes`)

- [ ] **Step 1: Refactor to a bytes inner.** Extract the encrypt/ingest core of
  `ingest_channel_artifact_impl` into a helper taking in-memory `plaintext: Vec<u8>` + `name` +
  `mime` + `encrypt` + the resolved `space`/epoch key, returning `ChannelAttachmentDto`. The
  existing path-based command reads the file (with its `MAX_ARTIFACT_BYTES` cap) then calls the
  helper — behavior unchanged.

- [ ] **Step 2: Bytes command.** Add:
  ```rust
  #[tauri::command]
  async fn ingest_channel_artifact_bytes(state_lock, community_id, bytes: Vec<u8>, name, mime, encrypt)
      -> Result<ChannelAttachmentDto, String>
  ```
  + `_impl`. Cap `bytes.len() as u64 <= MAX_ARTIFACT_BYTES`, then call the shared helper. Register
  in `generate_handler!`. (Emoji ingest will call this with `encrypt=true`, `mime="image/png"`,
  `name=""`; the in-memory cap is the artifact cap — emoji size is separately capped at react time.)

- [ ] **Step 3: Tests.** Round-trip: `ingest_channel_artifact_bytes` (encrypted) → `find_attachment`
  is NOT expected (no event yet), but `preview_reaction_emoji` after a React referencing the cid
  returns the bytes (covered in Task 6). Here: assert the returned DTO has correct `size`
  (plaintext len), `encrypted=true`, and a valid hex cid; assert oversize bytes are rejected.

- [ ] **Step 4: Commit + gate.** Subject:
  `feat(emoji-reactions): ingest_channel_artifact_bytes (in-memory encrypted CAS ingest)`.

---

## Task 6: Backend two-engine integration test

**Files:**
- Test: add to the existing channel-config / reaction integration test module (mirror the
  ZEB-540 two-engine artifact test).

- [ ] **Step 1:** Engine A ingests an emoji blob (via the bytes path or a direct CAS ingest),
  reacts to a message with the emoji `ChannelAttachment`; engine B receives + verifies the React,
  materializes a `ReactionDto` with `emoji_cid`=Some, and `preview_reaction_emoji`/authorize on B
  returns the plaintext emoji bytes. Assert the round-trip byte-equality.

- [ ] **Step 2: Commit + gate** (full `cargo nextest run --workspace`). Subject:
  `test(emoji-reactions): two-engine custom emoji react + cross-engine preview`.

---

## Task 7: Frontend normalizeEmoji

**Files:**
- Create: `src/lib/emoji-normalize.ts`
- Test: `src/lib/__tests__/emoji-normalize.test.ts`

- [ ] **Step 1:** Implement `EMOJI_EDGE = 128` and
  `export async function normalizeEmoji(file: File): Promise<Uint8Array>`:
  input gate (`<= AVATAR_MAX_INPUT_BYTES`, must be image — import from `avatar-normalize.ts`);
  `assertHeaderDimsOk(bytes)` → `createImageBitmap(file)` → `assertDecodedDimsOk(w,h)`;
  **contain-fit** resize (preserve aspect, no crop) within 128×128 on a transparent canvas;
  `toBlob('image/png')` → `Uint8Array`. Reuse the exported guards from `avatar-normalize.ts`
  (do not duplicate them).

- [ ] **Step 2: Tests** (mirror `avatar-normalize`/`avatar-resolver` test stubs:
  `vi.stubGlobal('createImageBitmap', ...)`, canvas/`toBlob` stub): header guard fires on
  oversize-dim PNG header; decoded guard fires; output is PNG; non-image input rejected; oversize
  bytes rejected; contain-fit preserves aspect (no crop) — assert target dims.

- [ ] **Step 3: Commit + gate** (`npx tsc --noEmit && npx vitest run`). Subject:
  `feat(emoji-reactions): normalizeEmoji (contain-fit 128px PNG, reused decode-bomb guards)`.

---

## Task 8: Frontend service

**Files:**
- Modify: `src/lib/channel-message-service.ts`
- Test: `src/lib/__tests__/channel-message-service.test.ts`

- [ ] **Step 1: DTO types.** Add `emojiCid?: string` and `emojiSize?: number` to the reactions
  item type on `ChannelMessageDto` and any reaction-received payload type.

- [ ] **Step 2: Methods.**
  - `reactToMessage(communityId, channelId, messageId, emoji, add, emojiCid?)` — when `emojiCid`
    present, pass `emoji: { cid, mime: 'image/png', size }` to `set_message_reaction` (the service
    must also receive `size`; thread an `emojiSize?` param or accept a small descriptor object).
    Prefer: `reactToMessage(..., add, emojiInput?: { cid: string; mime: string; size: number })`.
  - `ingestEmojiBytes(communityId, bytes): Promise<{ cid: string; size: number }>` — invokes
    `ingest_channel_artifact_bytes` with `{ communityId, bytes: Array.from(bytes), name:'',
    mime:'image/png', encrypt:true }`, returns the DTO's cid+size.
  - `previewReactionEmoji(communityId, channelId, cid): Promise<Uint8Array>` — invokes
    `preview_reaction_emoji`, converts `number[]`→`Uint8Array`. Error extraction
    `e instanceof Error ? e.message : String(e)`.

- [ ] **Step 3: Tests.** `reactToMessage` with an emoji input invokes `set_message_reaction` with
  the descriptor; `ingestEmojiBytes` round-trips the mocked DTO; `previewReactionEmoji` converts
  bytes + extracts errors. Mock `invoke` per the existing test patterns.

- [ ] **Step 4: Commit + gate.** Subject:
  `feat(emoji-reactions): channel service emoji ingest + react-with-cid + preview`.

---

## Task 9: Frontend UI (picker + image chip + blob lifecycle)

**Files:**
- Modify: `src/lib/components/ChannelMessageFeed.svelte`
- Create: a small reusable emoji-image resolver/component (e.g.
  `src/lib/components/ReactionEmojiImage.svelte`) so the chip render + blob lifecycle isn't
  inlined/duplicated.
- Test: `src/lib/components/__tests__/ReactionEmojiImage.test.ts` (+ feed wiring assertions if
  practical).

- [ ] **Step 1: Picker custom affordance.** Add a "custom" button in the emoji picker →
  `@tauri-apps/plugin-dialog` `open({ multiple:false })` → read file → `normalizeEmoji` →
  `ingestEmojiBytes` → `reactToMessage(..., add:true, { cid, mime:'image/png', size })`. Guard the
  async pick/ingest with the existing channel-switch epoch pattern used by the attachment compose
  path.

- [ ] **Step 2: `ReactionEmojiImage.svelte`.** Props: `{ communityId, channelId, cid }`. On mount/
  cid-change, `previewReactionEmoji` → `assertHeaderDimsOk` → `createImageBitmap` →
  `assertDecodedDimsOk` → revoke-prev → `URL.createObjectURL` → `<img>`. Apply the **exact ZEB-540
  blob lifecycle discipline** (`MessageAttachments.svelte`): `isLive` post-await guards, revoke on
  unmount (`$effect` cleanup), revoke on cid-prop change, graceful placeholder until resolved/on
  error. Render a neutral fallback chip while pending.

- [ ] **Step 3: Chip render.** In the reactions render, `{#if r.emojiCid}
  <ReactionEmojiImage .../> {:else} {r.emoji} {/if}`; clicking a custom chip toggles
  `reactToMessage(..., add: !r.mine, { cid: r.emojiCid, mime:'image/png', size: r.emojiSize })`.

- [ ] **Step 4: Tests.** Mock `previewReactionEmoji` + `createImageBitmap` +
  `URL.createObjectURL/revokeObjectURL` (avatar-resolver pattern). Assert: renders `<img>` for a
  cid; revokes on unmount; revokes + re-fetches on cid change; teardown-race (unmount mid-fetch →
  no `createObjectURL`).

- [ ] **Step 5: Commit + gate** (`npx tsc --noEmit && npx vitest run`). Subject:
  `feat(emoji-reactions): picker custom affordance + inline emoji chip render`.

---

## Task 10: Full gate + PR

- [ ] **Step 1: Full workspace gate.** From `src-tauri/`: fmt + clippy (`--all-targets
  --features test-fixtures`) + `cargo nextest run --locked --workspace --all-targets --features
  test-fixtures`. From root: `npx tsc --noEmit` + `npx vitest run`. All green.
- [ ] **Step 2: Final code review** (subagent) over the whole branch diff.
- [ ] **Step 3: Open PR** (title + body ZEB-free; reference spec/plan by path + commit, predecessors
  ZEB-536/535/540 in body prose is fine). Then run the bot loop (Qodo+CodeAnt → address → one
  CodeRabbit final review). Hold at Jake's merge gate.

## Self-review notes (plan author)

- **Type consistency:** `ReactionEmojiInput {cid,mime,size}` (Rust) ↔ `{ cid, mime, size }`
  (TS service param) ↔ `ReactionDto.emoji_cid/emoji_size` (camelCase `emojiCid/emojiSize`) ↔
  `ChannelMessageDto.reactions[].emojiCid/emojiSize`. Consistent.
- **Byte-stability:** Task 1 Step 4 explicitly re-runs the existing react fixture before adding the
  new one — the canary for the additive-field invariant.
- **Server-side cap:** `preview_reaction_emoji` hard-codes `MAX_CUSTOM_EMOJI_BYTES`; verify +
  `set_message_reaction_impl` also enforce it — three independent enforcement points.
- **No placeholder caps/sizes:** all constants pinned (256 KiB store, 128 px edge, 10 MiB input,
  8192 decoded dim).
