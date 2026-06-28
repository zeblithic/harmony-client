# ZEB-588 — @-Mentions (community channels) — Design Spec

**Status:** approved design (2026-06-28), pending spec review → implementation plan.
**Ticket:** ZEB-588. **Scope this pass:** community channel messages only.

## Goal

Let a user @-mention another member while composing a channel message, and render
those mentions as resolved, human-readable names — so the mention is **identity-stable
on the wire** (keyed by owner-id, not a display string) but **human-readable on screen**,
and a member can *see* when they were pinged.

## Background — what already exists (verified)

The wire format and backend are already built; this feature only wires the two missing UI halves.

- **DTO + IPC carry mentions already.** `ChannelMessageDto.mentions?: string[]` (owner-id
  hexes) exists in both Rust (`community_channel_log_engine.rs`) and TS
  (`channel-message-service.ts`). `ChannelMessageService.postMessage(communityId,
  channelId, body: string, replyTo?, mentions?: string[], attachments?)` already passes
  `mentions` to the `post_channel_message` IPC (empty→`undefined` so signed bytes match a
  mention-less post). The backend validates hex + enforces a cap (rejection text "too many
  mentions: N (max 64)") and derives "mentions me" as `selfOwnerHex ∈ mentions`.
- **Resolution ladder already exists.** `ChannelMessageFeed.authorLabel()` resolves
  `resolveNickname(ownerIdHex) → resolveCard(ownerIdHex)?.displayName → ownerIdHex.slice(0,8)`.
  `resolveNickname` is the shipped local-friend-nickname feature (ZEB-419,
  `friend_nicknames.rs`); `resolveCard` is `MemberCardService` (broadcast profile name).
  This is exactly the ticket's priority order and is reused verbatim.
- **Compose + render are plain text today.** Compose: `composeText = $state('')` →
  `handleCompose()` → `postMessage(..., text, undefined /*replyTo*/, undefined /*mentions*/, ...)`.
  Render: `<p class="body">{bodyToText(msg.body)}</p>` where `bodyToText` UTF-8-decodes the
  byte array. No tokenizing, markdown, or sanitizer — Svelte's `{}` interpolation
  auto-escapes.
- **`ChannelMessageFeed` props** already include `communityId`, `channelId`, `ownAddress`
  (self owner-id hex), `resolveCard?`, `resolveNickname?`. The member **roster** for the
  autocomplete is the one new input the component needs (App owns the member cache via
  `CommunityService.listCommunityMembers`).

## Wire format

A message body is UTF-8 text that may contain stable inline mention tokens:

```text
<@<ownerIdHex>>      e.g.  <@a1f2c3d4e5f60718a9b0c1d2e3f40516>
```

- `ownerIdHex` = 32 lowercase hex chars (16-byte OwnerAddr). Matching regex:
  `/<@([0-9a-f]{32})>/g`.
- Angle-bracket sentinel → effectively zero collision with hand-typed text; the user never
  types it directly (compose inserts the human-facing `@label`; send-time reconciliation
  emits the `<@ownerIdHex>` wire token).
- The `ChannelMessageDto.mentions` array = the set of owner-ids referenced by the tokens.
  The body tokens carry *position* (for rendering); the array is the denormalized set the
  backend already uses for "mentions me". On send we produce both from one reconcile pass,
  so they are always consistent.

No Rust/wire changes. (Backend already accepts and validates the array.)

## Architecture

Two new pure-function modules (fully unit-testable, no Svelte/DOM), one new presentational
component, and edits to `ChannelMessageFeed` + `App.svelte`.

### New: `src/lib/mention-compose.ts` (pure)

Owns compose-time logic. No DOM.

```ts
export interface MentionCandidate { ownerId: string; label: string; }
export interface TrackedMention { ownerId: string; label: string; }

/** Detect an active @-trigger at the caret. Returns the query (text after '@',
 *  non-whitespace) and the '@' index, or null if the caret is not in a trigger.
 *  A trigger requires the '@' to be at start-of-text or preceded by whitespace
 *  (so "a@b.com" / "x@y" never trigger), and the run from '@' to the caret to be
 *  all non-whitespace and contain no second '@'. */
export function detectMentionTrigger(
  text: string, caret: number,
): { query: string; atIndex: number } | null;

/** Insert a picked candidate at an active trigger: replaces the '@query' range
 *  with '@<label> ' and returns the new text, the new caret index, and the
 *  TrackedMention to append. Labels may contain spaces; the trigger query never
 *  does, which is what makes multi-word labels safe. */
export function applyMentionPick(
  text: string, atIndex: number, caret: number, candidate: MentionCandidate,
): { text: string; caret: number; tracked: TrackedMention };

/** Filter+rank the roster for the autocomplete: case-insensitive substring
 *  match on the candidate label, prefix matches sorted ahead of mid-string
 *  matches, capped to `limit` (default 8). Pure → the parent calls it so it owns
 *  the exact list the active-index/Enter selection maps onto. */
export function filterCandidates(
  candidates: MentionCandidate[], query: string, limit?: number,
): MentionCandidate[];

/** Reconcile the final textarea text + the picks into the wire payload.
 *  Single left-to-right scan; at each index, among tracked entries whose
 *  `@<label>` matches at that index, take the LONGEST (so a longer label is not
 *  pre-empted by a shorter one that is its prefix). On match, emit `<@ownerId>`,
 *  advance past the label, and record the ownerId; otherwise copy the char.
 *  Picks whose `@<label>` no longer appears (user edited/deleted it) are simply
 *  not emitted → they degrade to plain text. Returns the wire body and the
 *  de-duplicated mentions array (insertion order). */
export function reconcileCompose(
  text: string, tracked: TrackedMention[],
): { body: string; mentions: string[] };
```

### New: `src/lib/mention-render.ts` (pure)

Owns render-time parsing + the shared resolution ladder.

```ts
export type BodySegment =
  | { type: 'text'; text: string }
  | { type: 'mention'; ownerId: string };

/** Split a wire body into alternating text/mention segments by the
 *  /<@([0-9a-f]{32})>/g token. A body with no tokens yields one text segment. */
export function tokenizeBody(text: string): BodySegment[];

/** The single shared resolution ladder used by compose candidates, render, and
 *  authorLabel: local nickname → broadcast profile name → `ownerId.slice(0,8)`.
 *  Returns the BARE label (no leading '@'); the mention render template adds the
 *  '@'. This is `authorLabel`'s exact current behavior, extracted. */
export function resolveMentionLabel(
  ownerId: string,
  resolveNickname?: (id: string) => string | undefined,
  resolveCard?: (id: string) => { displayName: string } | undefined,
): string;
```

`ChannelMessageFeed.authorLabel` is refactored to call `resolveMentionLabel` (identical
behavior — author labels show the bare name; the mention span prepends '@') so there is
exactly one ladder.

### New: `src/lib/components/MentionAutocomplete.svelte`

A **purely presentational** dropdown. **Props:** `candidates: MentionCandidate[]` (the
*already-filtered* list from `filterCandidates`), `activeIndex: number`,
`onPick: (c: MentionCandidate) => void`. **Behavior:** renders the given candidates,
highlights row `activeIndex`, click a row → `onPick`. No filtering, no keyboard, no data
fetching inside. The parent owns everything stateful: it calls `filterCandidates`, owns the
open/closed state and `activeIndex`, and intercepts ↑/↓/Enter/Esc on the shared compose
textarea (↑/↓ move active, Enter picks `candidates[activeIndex]`, Esc closes). Splitting it
this way keeps the keyboard handling on the textarea that has focus while the selection list
stays a trivial, testable render.

### Modified: `src/lib/components/ChannelMessageFeed.svelte`

- **New prop:** `mentionCandidates?: MentionCandidate[]` (the channel/community roster as
  `{ownerId, label}`, label pre-resolved via the ladder by the parent). Empty/absent →
  autocomplete never shows (feature degrades to plain text).
- **Compose:** on `composeText` input / caret move, call `detectMentionTrigger`; if active,
  show `<MentionAutocomplete>` anchored at the input with the filtered candidates and a live
  `query`. On pick, `applyMentionPick` updates `composeText` + caret and appends to a local
  `tracked: TrackedMention[]`. ↑/↓/Enter/Esc are intercepted while the dropdown is open
  (Enter picks instead of sending; Esc closes the dropdown, not the compose).
- **Send (`handleCompose`):** `const { body, mentions } = reconcileCompose(composeText,
  tracked)`; `postMessage(communityId, channelId, body, replyTo, mentions, attachments)`;
  clear `composeText` and `tracked`.
- **Render:** replace `{bodyToText(msg.body)}` with
  `{#each tokenizeBody(bodyToText(msg.body)) as seg}` → `text` segments render as
  `{seg.text}` (auto-escaped, XSS-safe); `mention` segments render
  `<span class="mention" class:self={seg.ownerId === ownAddress}>@{resolveMentionLabel(seg.ownerId, resolveNickname, resolveCard)}</span>`.
- **Self-mention highlight:** the message row gets a `mentions-me` class when
  `msg.mentions?.includes(ownAddress)`.
- **Styles:** scoped `.mention` (accent chip), `.mention.self` (stronger emphasis),
  `.mentions-me` (subtle row highlight).

### Modified: `src/App.svelte`

Build the `mentionCandidates` for the selected community from the existing member cache
(`CommunityService.listCommunityMembers` results already held in `App`), each entry
`{ ownerId: member.address, label: resolveMentionLabel(member.address, resolveNickname, resolveCard) }`,
and pass it to `<ChannelMessageFeed>`. Reuses the existing reactive member roster + the
nickname/card maps already wired into the feed — no new fetch path.

## Data flow

```text
type '@jak'  → detectMentionTrigger → MentionAutocomplete(candidates filtered by 'jak')
   ↓ pick "Jake (Koya)"
applyMentionPick → composeText "hey @Jake (Koya) ", tracked += {id, "Jake (Koya)"}
   ↓ send
reconcileCompose → body "hey <@a1f2…> ", mentions ["a1f2…"]
   ↓ postMessage (existing IPC; backend validates + stores; derives mentions-me)
channel-log → ChannelMessageDto { body: bytes("hey <@a1f2…> "), mentions:["a1f2…"] }
   ↓ render
tokenizeBody → [text "hey ", mention a1f2…, text " "]
   → "hey " + <span.mention>@Jake (Koya)</span> + " ";  row.mentions-me if self ∈ mentions
```

## Edge cases (all covered by unit tests)

- No mentions → body unchanged, `mentions` empty (→ `undefined` at IPC).
- Mention at start / end / adjacent to another mention / repeated same mention.
- Two distinct members with the **same label** → two tracked entries, mapped left-to-right
  to their respective ids (order-based; acceptable for MVP).
- One label is a **prefix** of another ("Jake" vs "Jake (Koya)") → longest-match wins in the
  reconcile scan.
- User **edits inside** an inserted label → its `@label` no longer matches → degrades to
  plain text (not a mention); never a wrong-id mention.
- `@` in emails/code ("a@b.com") → never triggers the autocomplete, never tokenized.
- A user who literally types `<@<32 hex>>` by hand renders as a mention — accepted known
  minor (astronomically unlikely; documented, not handled).
- XSS: all text rendered via `{}` interpolation (escaped); the resolved label is text, not
  HTML; no `innerHTML` anywhere.

## Error handling

- `reconcileCompose` is total (never throws); a broken pick just doesn't emit a token.
- Backend rejection (e.g. "too many mentions: N (max 64)") surfaces through the existing
  `postMessage` catch → existing compose error UI. We also soft-cap the picker so a user
  can't exceed the cap (best-effort; backend is authoritative).
- Autocomplete with an empty/absent roster simply never opens.

## Testing

- `mention-compose.test.ts` — `detectMentionTrigger` (boundary/whitespace/email/no-@/second-@),
  `applyMentionPick` (caret math, multi-word label), `filterCandidates` (substring match,
  prefix-first ordering, cap, empty query), `reconcileCompose` (every edge case above:
  prefixes, duplicates, edited picks, order-based same-label mapping, dedupe order).
- `mention-render.test.ts` — `tokenizeBody` (none / start / end / adjacent / multiple /
  malformed near-tokens), `resolveMentionLabel` ladder precedence + fallbacks.
- `MentionAutocomplete.test.ts` — filtering, keyboard nav, pick + close, empty → hidden.
- `ChannelMessageFeed` tests — compose `@`→pick→send emits the right `body`+`mentions`;
  render shows a styled `@Name` span resolving via the ladder; self-mention adds the
  `self` / `mentions-me` classes.
- Gates: `tsc --noEmit`, full `vitest run`. Frontend-only (no Rust changed).

## Out of scope (deferred, per ticket)

- DM / group-DM mentions (the DM send path has no mentions field — needs separate backend
  work).
- `@everyone` / `@here` broadcast mentions.
- Click-a-mention-to-open-profile (mentions are display-only this pass).
- Mention **notification** settings/routing/DND, and the community-layer nickname tier.

## File summary

| File | Change |
|---|---|
| `src/lib/mention-compose.ts` | **new** — `detectMentionTrigger`, `applyMentionPick`, `reconcileCompose` (pure) |
| `src/lib/mention-render.ts` | **new** — `tokenizeBody`, `resolveMentionLabel` (pure) |
| `src/lib/components/MentionAutocomplete.svelte` | **new** — dropdown UI |
| `src/lib/components/ChannelMessageFeed.svelte` | compose wiring + render path + self-highlight + `mentionCandidates` prop; `authorLabel` → `resolveMentionLabel` |
| `src/App.svelte` | build + pass `mentionCandidates` from the existing member cache |
| `*.test.ts` (4) | unit + component coverage |
