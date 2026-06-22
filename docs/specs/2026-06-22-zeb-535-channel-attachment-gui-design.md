# ZEB-535 GUI phase — Channel artifact attachments (design)

**Status:** approved 2026-06-22
**Parent:** ZEB-535 (In-channel artifact sharing via CAS) → epic ZEB-533 (Harmony fleet collaboration features)
**Predecessors (merged):** #312 (CAS backend) · #313 (IPC + frontend service, folded ZEB-539 re-serve hardening)
**Branch:** `cas-channel-artifact-gui` off main `13fe1007`

## Goal

Let a human in the desktop app **send** a file into a channel and **see + download** files others
send — completing the deferred GUI half of ZEB-535. The ticket explicitly scoped "receivers fetch +
render it … GUI rendering later"; the backend, Tauri IPC, and frontend service all landed in
#312/#313. This phase is **frontend-only**.

## Scope (v1)

In:
- Render each attachment on a received/own message as a **download chip**: mime icon + name +
  human-readable size + 🔒 badge when encrypted + a Download button.
- A compose-time **attach** affordance: file picker → ingest into CAS → pending chips → send.

Out (deferred, fast-follow ticket):
- **Inline preview** (image thumbnails, first-N-lines of a text/diff). There is no frontend path
  today to pull CAS artifact bytes into memory — `downloadArtifact` writes straight to disk. Inline
  preview needs a *new* bounded fetch-to-memory IPC (decrypt in memory, hard size cap) plus
  blob-URL lifecycle. That is its own ticket; this v1 ships zero new backend.

## Non-goals

- No Rust changes, no new IPC. Reuse `ingest_channel_artifact` / `download_channel_artifact` and the
  existing `ChannelMessageService.ingestArtifact` / `downloadArtifact` facades verbatim.
- No client-side size-cap reimplementation — the backend already enforces a 1 GiB cap; we surface its
  error rather than duplicating the limit.

## Architecture

One new self-contained render component, a contained composer extension in the existing feed, and
reuse of the shipped service facades. Isolating all rendering in a new component keeps the footprint
in `ChannelMessageFeed.svelte` — which AVALON's reactions PR #316 also edits — down to a single
insertion line, minimising the merge conflict.

### Existing surface this builds on (already on main)

- `ChannelMessageDto.attachments?: ChannelAttachmentDto[]` rides on every message
  (`src/lib/channel-message-service.ts`).
- `ChannelAttachmentDto = { cid: string; mime: string; name: string; size: number; encrypted: boolean }`.
- `ChannelMessageService.postMessage(communityId, channelId, body, replyTo?, mentions?, attachments?)`
  — already accepts and signs `attachments`.
- `ChannelMessageService.ingestArtifact(communityId, sourcePath, opts?) → ChannelAttachmentDto`
  (encrypt defaults true).
- `ChannelMessageService.downloadArtifact(communityId, channelId, attachment, destPath, maxBytes?) → number`
  (backend authorizes the CID against the channel's signed log, derives authoritative size, decrypts,
  writes to `destPath`).
- `@tauri-apps/plugin-dialog` `open` / `save` are deps (used by `FileBrowser`, `IdentityPanel`,
  `MintLedger`, `DiagnosticExportModal`).
- `formatBytes(bytes)` in `src/lib/file-utils.ts`. `CATEGORY_ICONS` / `SENSITIVITY_ICONS` there too.

## Components & files

### New: `src/lib/components/MessageAttachments.svelte`

Props: `{ communityId: string; channelId: string; attachments: ChannelAttachmentDto[]; channelMessageService: ChannelMessageService }`.

Renders one chip per attachment:
- `mimeCategoryIcon(att.mime)` (new helper, below) + `att.name` + `formatBytes(att.size)`
- 🔒 badge when `att.encrypted` (reuse `SENSITIVITY_ICONS.private`)
- Download button.

Per-chip download state machine, keyed by `att.cid` (a message can carry several attachments and
each downloads independently): `idle → downloading → saved | error`. `error` exposes a Retry.
Download handler:
1. `save({ defaultPath: att.name, filters: <extension-derived> })` from `@tauri-apps/plugin-dialog`.
2. If the user cancels (`save` resolves `null`) → no-op, stay `idle`.
3. Else `await channelMessageService.downloadArtifact(communityId, channelId, att, destPath)`;
   on resolve → `saved`; on reject → `error` with the extracted message.

The component owns no global state and no listeners — pure props in, dialog + facade out.

### New helper in `src/lib/file-utils.ts`: `mimeCategoryIcon(mime: string): string`

`categoryIcon` takes a `ContentCategory` enum, **not** a mime, so it can't be called with a mime
directly. Add a thin mapper: `image/*` → image icon, `text/*` → text icon, `audio/*` → music,
`video/*` → video, else the default text/file icon (📄). Reuses `CATEGORY_ICONS` so icon glyphs stay
in one place. Pure function → unit-testable.

### Modify: `src/lib/components/ChannelMessageFeed.svelte`

1. **Render (one insertion line):** in the message row, after `<p class="body">`, add
   `{#if msg.attachments?.length}<MessageAttachments communityId={communityId} channelId={channelId} attachments={msg.attachments} channelMessageService={channelMessageService} />{/if}`.
   Positioned **above** the (future) reactions row, per the coordination note to AVALON.
2. **Compose affordance:** a 📎 attach button beside the textarea. New state
   `pendingAttachments: ChannelAttachmentDto[]` and `ingesting: boolean`. Click →
   `open({ multiple: true })`; for each picked path → `ingestArtifact(communityId, path)` → push to
   `pendingAttachments`. A pending-chips strip above the input shows each pending attachment with a
   remove (×). While `ingesting`, the attach button + send are disabled (so we never post a
   half-ingested attachment).
3. **Send:** `handleCompose` guard becomes `(text || pendingAttachments.length) && !posting`. Pass
   `pendingAttachments` to `postMessage(communityId, channelId, text, undefined, undefined, pendingAttachments)`.
   On success clear both `composeText` and `pendingAttachments`. Empty body + ≥1 attachment is a
   valid post.

## Data flow

- **Send:** pick file(s) → `ingestArtifact` (CAS ingest, encrypt, returns descriptor) → pending chip
  → `postMessage(attachments)` → signed channel-log event carries `attachments[]` → propagates to
  peers via the existing channel-log sync.
- **Receive:** `channel-message-received` → `ChannelMessageService` cache → `ChannelMessageDto`
  carries `attachments` → `MessageAttachments` renders chips.
- **Download:** click ⤓ → save dialog → `downloadArtifact` (backend authorizes CID against the
  channel's verified log, fetches from CAS, decrypts, writes to chosen path, returns byte count).

## Error handling

- **Ingest failure** (size cap exceeded / unreadable file): surface on the composer error line; the
  file is not added to `pendingAttachments`. Use the IPC error-extraction rule
  (`e instanceof Error ? e.message : String(e)`).
- **Download failure** (peer offline / not serveable / size mismatch): per-chip `error` state with a
  Retry button; the message text itself is unaffected.
- **Save dialog cancelled:** silent no-op.
- **Empty post guard:** send is rejected unless there's body text or ≥1 attachment.
- **Concurrent ingest:** send disabled while any ingest is in flight.

## Testing (vitest only — no Rust change)

`src/lib/components/__tests__/MessageAttachments.test.ts` (new):
- Renders a chip per attachment with name, `formatBytes(size)`, and the mime-derived icon.
- 🔒 badge present iff `encrypted`.
- Download button → `save` returns a path → `downloadArtifact` called with `(communityId, channelId, att, path)`; chip goes `saved`.
- `save` returns `null` (cancel) → `downloadArtifact` NOT called; chip stays `idle`.
- `downloadArtifact` rejects → chip shows `error` + Retry; Retry re-invokes.
- Independent per-cid state: downloading one chip doesn't change a sibling's state.

`src/lib/components/__tests__/ChannelMessageFeed.test.ts` (extend the existing harness):
- Attach button → `open` returns paths → `ingestArtifact` called per path → pending chips shown.
- Remove (×) drops a pending attachment.
- Send includes `pendingAttachments` in the `post_channel_message` invoke and clears them after.
- Send allowed with empty text + ≥1 attachment.
- Ingest rejection → composer error line shown, nothing added to pending.
- Received message with `attachments` renders `MessageAttachments`.

Mock `@tauri-apps/plugin-dialog` (`open`/`save`) via `vi.mock` — no test in the suite mocks it yet,
so each test file installs its own. The feed test already has a `makeAdapter()` + `setup()` harness
to extend.

`src/lib/__tests__/file-utils` (or a colocated test): `mimeCategoryIcon` mapping for each prefix +
default.

## Unit boundaries

- `MessageAttachments.svelte` — renders an attachment list + drives per-chip download. Depends only on
  props + `plugin-dialog` + the service facade. Swappable/testable in isolation.
- `mimeCategoryIcon` — pure mime → glyph. No deps beyond `CATEGORY_ICONS`.
- `ChannelMessageFeed.svelte` — owns compose + pending-attachment lifecycle; delegates all rendering
  to `MessageAttachments`.

## Coordination

AVALON's reactions PR #316 (`zeb-536-spec2-reactions-ui`) also edits `ChannelMessageFeed.svelte`,
`channel-message-service.ts`, and `ChannelMessageFeed.test.ts`. Mitigation: all new render markup
lives in `MessageAttachments.svelte`; the feed change is one insertion line + the composer block.
Whoever merges second rebases and resolves the feed file; attachments render above the reactions row.

## Ticket close-out

This delivers ZEB-535's "GUI render affordance." On merge: **close ZEB-535**; file a **separate
fast-follow** for inline image/text preview (the deferred fetch-to-memory path). Parent epic ZEB-533
stays open. One PR; PR title/body kept free of ZEB-NNN per the Linear auto-close convention.
