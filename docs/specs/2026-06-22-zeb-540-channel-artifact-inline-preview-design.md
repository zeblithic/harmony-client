# ZEB-540 — Inline preview for channel artifacts (design)

**Status:** approved 2026-06-22
**Parent:** ZEB-540 (Inline preview for channel artifacts) → epic ZEB-533 (Harmony fleet collaboration)
**Predecessor (merged):** ZEB-535 — CAS channel artifact sharing. Backend #312, IPC/service #313, download-only GUI #317.
**Branch:** `cas-artifact-preview` off main `a17dddb5`

## Goal

Let a human glance at a shared channel artifact — an image or a text/diff/log — **inline in the
feed**, without the save-dialog + open-file roundtrip that download-only requires. This completes the
preview half deferred from ZEB-535 (#317 shipped download-only chips).

## Decisions (settled with Jake 2026-06-22)

1. **Click-to-preview** (not auto-thumbnail). A Preview affordance on the chip; clicking fetches the
   bytes into memory and expands the preview inline; clicking again collapses and frees it. **Nothing
   fetches until the user asks** — one code path, scale-safe (no auto-bandwidth in a busy channel),
   cheapest. Auto-thumbnails for images can be a later refinement.
2. **4 MiB in-memory cap.** Artifacts larger than the cap stay **download-only** (no Preview button).
   Bounds the in-memory fetch+decrypt (~2× at decrypt) and the `number[]` IPC transport cost.
3. **Previewable types:** `image/*` (rendered image) and `text/*` (decoded head). All other mimes —
   and any artifact over the cap — keep today's download-only chip.

## Scope

In:
- New backend IPC `preview_channel_artifact` — authorize-first (same gate as download), fetch +
  decrypt **in memory**, hard cap, return the plaintext bytes.
- `ChannelMessageService.previewArtifact(...)` facade.
- Preview UI in `MessageAttachments.svelte`: a Preview button (shown only when previewable & ≤ cap),
  a per-cid preview state machine, inline image / text-head render, blob-URL lifecycle.

Out (not this PR):
- Auto-thumbnails / lazy-on-scroll fetch.
- Ranged/partial fetch (text "head" still fetches the whole artifact, bounded by the cap — a huge log
  over the cap is download-only, not head-previewed).
- A headless RPC verb for preview (GUI feature; add later only if cheap parity is wanted).
- Raw-bytes (`tauri::ipc::Response`) IPC transport — see "IPC transport" below.

## Non-goals

- No new fetch/serve primitive. Reuse the existing `FetchRequest` path with `serveable: false`.
- No re-serve allowlisting on preview (unlike download). Preview is a lightweight read; the explicit
  download action remains the "I want this and will help swarm it" path. (A fetched block may still be
  cached locally by the existing fetch path — that's unchanged; we simply never add the subtree to the
  serve allowlist on a preview.)
- No client-side reimplementation of the size cap as the source of truth — the backend enforces it;
  the frontend mirrors the constant only to decide whether to show the Preview button.

## Architecture

### Backend (`src-tauri/src/lib.rs`)

The shipped `download_channel_artifact_impl` already does, in order: validate cid/community/channel
hex → `find_attachment` against the signed channel log (authoritative size; rejects unauthorized
CIDs) → cap check → resolve epoch key if encrypted → `FetchRequest{ serveable: false }` →
`finalize_artifact` (decrypt + size-verify + atomic disk write) → post-validation re-serve allowlist.

Preview reuses everything up to and including the fetch, then **returns plaintext instead of writing
to disk**, with a tighter cap and no re-serve step. To keep the security-critical authorize-first
gate in exactly one place, factor two helpers out of the download path (pure extractions, no behavior
change — the existing download tests are the regression net):

1. **`decrypt_and_verify_artifact(bytes, encrypted, epoch_key_opt, expected_size) -> Result<Vec<u8>, String>`**
   — sync, pure. The decrypt-if-flagged + plaintext-length-equals-`expected_size` check currently
   inline in `finalize_artifact`. `finalize_artifact` calls it then writes; preview calls it then
   returns. Unit-testable without a `NodeState`.

2. **`authorize_and_fetch_artifact(state, community_id, channel_id, cid, cap) -> Result<ArtifactFetch, String>`**
   where `ArtifactFetch { ciphertext_or_plain: Vec<u8>, encrypted: bool, epoch_key_opt: Option<EpochKey>, expected_size: u64, content_id: ContentId }`.
   The shared authorize-first + cap + epoch-key + fetch block, parameterized by `cap`. Both
   `download_channel_artifact_impl` and the new preview impl call it.
   - `download` then: `finalize_artifact(...)` + (if encrypted) `allow_serve_subtree(content_id)`.
   - `preview` then: `decrypt_and_verify_artifact(...)` and returns the plaintext.

New constant and command:

- **`pub(crate) const MAX_PREVIEW_BYTES: u64 = 4 * 1024 * 1024;`** (4 MiB). Distinct from
  `MAX_ARTIFACT_BYTES` (1 GiB, the download cap).
- **`#[tauri::command] async fn preview_channel_artifact(state, community_id, channel_id, cid, max_bytes: Option<u64>) -> Result<Vec<u8>, String>`**
  → thin delegate to `pub(crate) async fn preview_channel_artifact_impl(...)`. The impl uses
  `cap = max_bytes.map(|m| m.min(MAX_PREVIEW_BYTES)).unwrap_or(MAX_PREVIEW_BYTES)` (same shape as
  download, but clamped to the *preview* ceiling), authorizes+fetches via the shared helper, then
  `decrypt_and_verify_artifact` → `Ok(plaintext)`.
- Register `preview_channel_artifact` in the `invoke_handler!` list (beside `download_channel_artifact`).

The over-cap rejection happens in `authorize_and_fetch_artifact` (`expected_size > cap`) **before any
byte is fetched**, using the authoritative signed size — never a client value.

### IPC transport

Return `Result<Vec<u8>, String>` exactly like the existing `fetch_avatar` / `fetch_content` commands;
over the Tauri v2 boundary it arrives as `number[]`. This mirrors the established avatar pipeline
(`avatar-resolver.ts`: `number[]` → `Uint8Array` → `Blob` → `createObjectURL`) and reuses its test
patterns. The `number[]` JSON cost (~several MB for a near-cap artifact) is acceptable for a
one-at-a-time, user-initiated click and is bounded by the 4 MiB cap. A raw-bytes
(`tauri::ipc::Response`) transport is a possible future optimization but adds a Tauri-API-specific
path with no current precedent in the repo; out of scope.

### Frontend

**`src/lib/channel-message-service.ts`** — new facade:

```
async previewArtifact(communityId, channelId, attachment, maxBytes?): Promise<Uint8Array>
```
invokes `preview_channel_artifact` with `{ communityId, channelId, cid: attachment.cid, maxBytes }`,
converts the `number[]` reply to `Uint8Array`, and applies the standard IPC error extraction
(`e instanceof Error ? e.message : String(e)`) — same shape as `downloadArtifact`.

**`src/lib/artifact-preview.ts`** (new, pure/testable):
- `export const PREVIEW_MAX_BYTES = 4 * 1024 * 1024;` — frontend mirror of the backend cap (comment
  cross-references `MAX_PREVIEW_BYTES`), used only to gate the Preview button.
- `isPreviewable(att): boolean` — `att.size > 0 && att.size <= PREVIEW_MAX_BYTES && (att.mime starts with 'image/' || att.mime starts with 'text/')`.
- `isImage(att) / isText(att)` helpers.
- `decodeTextHead(bytes: Uint8Array, maxLines = 40, maxChars = 4000): { head: string; full: string; truncated: boolean }`
  — `TextDecoder().decode`, then head = first `maxLines` lines capped at `maxChars`; `truncated` true
  if either bound clipped; `full` is the whole decoded string (we already hold all bytes).

**`src/lib/components/MessageAttachments.svelte`** — extend the existing chip:
- Add a **Preview** button on the chip, rendered only when `isPreviewable(att)`. (Download button
  stays for everything.)
- Per-cid preview state: `previewStates[cid]: 'idle' | 'loading' | 'shown' | 'error'`, plus
  `previewUrls[cid]` (image blob URL), `previewTexts[cid]` ({head, full, truncated}),
  `previewExpanded[cid]` (text "show more" toggle), `previewErrors[cid]`.
- Click Preview (idle/error) → `loading` → `previewArtifact(...)`:
  - **image:** `assertHeaderDimsOk(bytes)` → `createImageBitmap(blob)` → `assertDecodedDimsOk(w,h)` →
    `bmp.close()` → `URL.createObjectURL(blob)` (blob typed from `att.mime`) → store url → `shown`.
    Mirrors `avatar-resolver.fetchCid`'s decode-bomb guards verbatim (8192px limit). A guard throw →
    `error`.
  - **text:** `decodeTextHead(bytes)` → store → `shown`.
  - reject → `error` with extracted message + Retry.
- Click Preview (shown) → collapse: `URL.revokeObjectURL(previewUrls[cid])` (if image), clear the
  per-cid preview entries, back to `idle`.
- Render when `shown`: image → `<img src={url} alt={att.name} class="att-preview-img">` (CSS
  max-width/max-height bounds it); text → `<pre class="att-preview-text">` showing head, plus a
  "show more / show less" toggle (renders `full`) when `truncated`.
- **Blob lifecycle:** an `$effect` cleanup (runs on component destroy — i.e. on channel switch /
  message churn, when feed re-renders unmount these children) revokes every URL in `previewUrls`.
  This is the leak-safety net beyond per-collapse revocation, mirroring `AvatarResolver.destroy()`.

The existing CID-dedup (`uniqueAttachments` $derived) and download chip are unchanged.

## Data flow (preview)

1. User clicks Preview on a chip whose `att` is previewable and ≤ 4 MiB.
2. `previewArtifact` → `preview_channel_artifact` IPC.
3. Backend authorizes the CID against the channel's signed log (authoritative size), rejects if
   `size > cap`, fetches (`serveable:false`), decrypts in memory, size-verifies, returns plaintext.
4. Frontend: image → decode-bomb guards → blob URL → `<img>`; text → decoded head in `<pre>`.
5. Collapse / channel-switch → blob URL revoked.

## Error handling

- **Over cap:** the Preview button is not shown (frontend gate); the backend also rejects defensively
  (`artifact size N exceeds cap M`) — surfaced as the per-cid `error` if ever reached.
- **Unauthorized / unknown CID:** backend `"unknown or unauthorized attachment"` → per-cid `error` +
  Retry. (Cannot normally happen for an attachment that rode in on the signed message, but the gate is
  unconditional.)
- **Fetch failure (peer offline / not serveable):** per-cid `error` + Retry; the message + download
  chip are unaffected.
- **Decode bomb (image):** `assertHeaderDimsOk` / `assertDecodedDimsOk` throw → per-cid `error` (never
  reaches an `<img>`); the download chip still works.
- **Decrypt / size mismatch:** backend `Err` → per-cid `error`.

## Testing (no Rust behavior change beyond the new command)

**Rust (`src-tauri/src/lib.rs` test module):**
- `decrypt_and_verify_artifact`: encrypted round-trip (decrypt → bytes), public passthrough,
  size-mismatch → Err. (Extraction parity — existing `finalize_artifact` tests still pass.)
- `preview_channel_artifact_rejects_unauthorized_cid` (mirror `download_..._rejects_unauthorized_cid`).
- `preview_channel_artifact_rejects_oversized` (signed size > `MAX_PREVIEW_BYTES` → Err before fetch).
- `preview_channel_artifact_rejects_overlong_cid_hex` / bad community/channel hex (boundary parity).
- Existing `download_channel_artifact_*` tests must remain green (the helper extraction's regression
  net).

**Frontend (vitest):**
- `artifact-preview.test.ts`: `isPreviewable` matrix (image/text under cap → true; over cap → false;
  other mime → false; size 0 → false); `decodeTextHead` head/truncate/full; `PREVIEW_MAX_BYTES` value.
- `channel-message-service.test.ts`: `previewArtifact` invokes `preview_channel_artifact` with
  `{communityId, channelId, cid, maxBytes}`, returns `Uint8Array` from `number[]`, error extraction.
- `MessageAttachments.test.ts` (extend): Preview button shown only when previewable; image click →
  `createObjectURL` → `<img>` with `src=blob:` shown; text click → head in `<pre>`; collapse →
  `revokeObjectURL` + button back to Preview; preview error → error line + Retry re-invokes;
  decode-bomb image (header dims over limit) → no `<img>`, error shown; over-cap/non-previewable
  attachment → no Preview button (download chip only); blob URL revoked on unmount.
  Stub `createImageBitmap` + `URL.createObjectURL/revokeObjectURL` per the avatar-resolver test.

## Unit boundaries

- `preview_channel_artifact_impl` — authorize+fetch (shared helper) + decrypt-verify + return.
- `authorize_and_fetch_artifact` / `decrypt_and_verify_artifact` — shared, single-source gate +
  decrypt contract for both download and preview.
- `artifact-preview.ts` — pure mime/size/text helpers; no DOM, no IPC.
- `channel-message-service.previewArtifact` — IPC facade only.
- `MessageAttachments.svelte` — owns the per-cid preview state machine + blob lifecycle + render.

## Coordination

`MessageAttachments.svelte` is a ZEB-535 component; AVALON's reactions UI (ZEB-536) does not touch it
(reactions render on the message row in `ChannelMessageFeed.svelte`, not the attachment chip). No
expected conflict. This PR does not touch `ChannelMessageFeed.svelte`.

## Ticket close-out

Delivers ZEB-540. On merge: close ZEB-540; parent epic ZEB-533 stays open. One PR; PR title/body kept
free of ZEB-NNN per the Linear auto-close convention.
