# ZEB-594 — Contenteditable-chips channel compose (design)

**Status:** approved design (Jake, 2026-08-06), pending spec review → implementation plan.
**Ticket:** ZEB-594 (Low). **Surface:** frontend only. **Scope this pass:** community
channel compose only (`ChannelMessageFeed`), desktop-complete.

## Goal

Replace the plain-`<textarea>` channel compose with a **contenteditable surface where
picked mentions render as atomic, non-editable chips**. Mention atomicity becomes
*structural* — a chip is a single DOM object you cannot half-edit, partially delete, or
retype into — rather than *reconciled* after the fact by offset/label matching. The chip
carries its `ownerId` directly, so the wire payload reads straight off the DOM with nothing
to track, shift, or invalidate.

This is a **UX/structural upgrade, not a bug fix.** ZEB-590 (PR #369, shipped) already gave
each pick stable span identity + edit-time invalidation, which closed the known corruption
vectors in the textarea model. ZEB-594's value is (a) visible, dismissible chips that make
"a mention is one object" obvious, and (b) retiring the offset-tracking machinery that
exists only to fake atomicity over flat text.

## Background — verified against source (2026-08-06)

- **Two separate compose stacks; mentions live in only one.** Channel messages use
  `ChannelMessageFeed.svelte`'s own inline `<textarea>` (lines ~1194-1206) with the full
  mention pipeline. DMs / group-DMs / thread replies use the shared `ComposeBar.svelte`
  (plain textarea, **no mentions** — the DM wire payload has no `mentions` field). Adding
  chips to DMs is net-new backend work and is **out of scope**.
- **Wire format + render side are frozen.** A body is UTF-8 text carrying
  `<@<ownerIdHex>>` tokens (32 lowercase hex); the denormalized `ChannelMessageDto.mentions:
  string[]` is the deduped owner-id set. Render parses tokens back via `tokenizeBody`
  (`mention-render.ts`, regex `/<@([0-9a-f]{32})>/g`) and resolves labels via
  `resolveMentionLabel` (4-rung ladder). None of this changes.
- **Reusable pure logic** (`mention-compose.ts`): `detectMentionTrigger` (the `@`-trigger
  detector), `filterCandidates` (ZEB-774's label-prefix ▸ label-substring ▸ hex-prefix
  ranking). `MentionAutocomplete.svelte` (presentational dropdown) is reused unchanged.
- **Retired machinery** (exists only to fake atomicity over flat text):
  `TrackedMention.start`, `shiftTrackedSpans`, `applyMentionPick` (string splice),
  `reconcileCompose` (position-anchored scan), and `ChannelMessageFeed`'s `prevComposeText`
  shadow.
- **Compose gotchas** (drive the design): caret/selection is textarea-specific
  (`selectionStart`/`setSelectionRange`) and must move to the `Selection`/`Range` API; IME
  composition is unhandled today (latent "Enter mid-composition sends" bug, acute in
  contenteditable); no paste handling exists; the shared draft is deliberately preserved
  across channel switches; contenteditable loses native textbox a11y semantics. The emoji
  picker is **not** a compose-insertion path (reactions only) — nothing to preserve there.

## Decision — scope boundary (desktop-complete)

One clean PR that is complete for desktop:

- **In:** full rewrite of channel compose to inline contenteditable chips; autocomplete
  reuse; Enter-to-send / Shift+Enter-newline; plain-text paste; IME-safe Enter (also fixes
  today's latent bug); core a11y (`role=textbox`, `aria-multiline`, `aria-label`,
  CSS placeholder); the shared draft preserved across channel switches.
- **Deferred to a fast-follow** (the unbounded contenteditable tail, only if real usage
  needs it): mobile/touch-specific hardening and deep screen-reader chip-deletion
  semantics.
- **Out:** DM/group/thread mentions (no wire field); any change to the wire format,
  `mention-render.ts`, notifications, or upstream candidate-building.

## Architecture

### New component — `src/lib/components/MentionInput.svelte`

Owns the contenteditable surface end to end. `ChannelMessageFeed` keeps roster-building,
the `postMessage` call, and error UI; it drops its `<textarea>` and all span-tracking
wiring.

**DOM structure:**

```html
<div class="mention-input" contenteditable="true"
     role="textbox" aria-multiline="true" aria-label={ariaLabel}
     data-placeholder={placeholder}>
  hey <span class="mention-chip" contenteditable="false"
            data-owner-id="a1f2…32hex">@Jake (Koya)</span> are you around
</div>
```

- Chips are inline `contenteditable="false"` spans carrying identity in `data-owner-id`;
  `false` makes them atomic for caret movement for free (the caret can't land inside).
- Placeholder is CSS-driven: `.mention-input:empty::before { content: attr(data-placeholder) }`
  (contenteditable has no native `placeholder`).

**Interface:**

| Prop / handle | Purpose |
|---|---|
| `candidates: MentionCandidate[]` | roster for autocomplete (built by `CommunityView`, unchanged) |
| `placeholder`, `ariaLabel`, `disabled` | mirror today's textarea attrs |
| `onSend: (payload: { body: string; mentions: string[] }) => void` | fired on Enter; parent posts |
| `onInput?: () => void` | lets parent clear `composeError`, etc. |
| `clear()` (via `bind:this`) | parent calls after a **successful** post; on failure the draft stays |
| `focus()` (via `bind:this`) | focus management |

### Module split — `src/lib/mention-compose.ts`

- **Keep:** `detectMentionTrigger` (caller changes — now reads the current text node's
  content up to the caret via `Selection`/`Range` instead of `textarea.selectionStart`;
  because chips are atomic nodes, a typed `@query` always lives within one text node, so the
  matching logic is unchanged). Note the "start-of-text" trigger boundary now means
  **start of the current text node**, so an `@` typed immediately after a chip (no
  intervening space) is a valid trigger — the desired behavior.
- **Add pure function:** `serializeSegments(segments: Segment[]) → { body: string;
  mentions: string[] }`, where `Segment = { type: 'text'; text: string } | { type:
  'mention'; ownerId: string }`. Concatenates text verbatim, emits `<@ownerId>` per mention
  segment, and returns the first-seen-deduped `mentions[]`. The component does a thin DOM
  walk (`childNodes` → segments) that also **normalizes newlines**: `<br>` and block-element
  boundaries (from Shift+Enter, which contenteditable renders as `<br>`/`<div>` depending on
  browser) become `\n` in the text stream, so multi-line messages round-trip into the body.
  The walk then hands off to the pure `serializeSegments`.
- **Remove:** `TrackedMention.start`, `shiftTrackedSpans`, `applyMentionPick`,
  `reconcileCompose`.

Splitting `serialize()` into (thin DOM walk) + (pure `serializeSegments`) preserves the
codebase's pattern — all mention *logic* stays in pure, DOM-free, exhaustively-tested
modules — and lets the existing `reconcileCompose` output-contract tests port over almost
verbatim as `serializeSegments` tests, so the frozen wire contract is provably preserved.

## Interaction design

**Autocomplete trigger.** On `input`/`keyup`/`click`/selection-change, read the caret via
`window.getSelection()`, take the current text node's content up to the caret offset, pass
it to `detectMentionTrigger`. Dropdown stays anchored to the input (as today);
caret-following positioning is optional polish, out of v1.

**Pick → imperative chip splice** (replaces `applyMentionPick`): build a `Range` over the
active `@query` chars, `deleteContents()`, `insertNode(chip)`, insert a trailing space text
node, collapse the caret after the space, close the dropdown.

**Keyboard `keydown` — order preserved (autocomplete hijack *before* send):**

1. **Dropdown open:** ArrowUp/Down move the active index; **Enter/Tab pick** the active
   candidate (`preventDefault`, no send); Esc closes the dropdown only (doesn't clear input).
2. **Enter, dropdown closed:** if `e.isComposing || e.keyCode === 229` → ignore (IME still
   composing); else if not Shift → `preventDefault`, serialize + `onSend`; **Shift+Enter →
   default** (newline).
3. **Backspace/Delete at a chip boundary → atomic chip delete** (below).

**Chip deletion — single-press atomic.** With a collapsed caret immediately after a chip
and Backspace (or immediately before + Delete), intercept and remove the whole chip node in
one keystroke. `contenteditable="false"` already makes chips atomic for arrows and *mostly*
for deletion, but browsers disagree (some select-first, some step inside), so intercepting
makes it deterministic and matches the "cannot half-delete a mention" goal. (Alternative
not chosen: two-press select-then-delete.)

**Paste — plain text only.** Intercept `paste`, `preventDefault`, read
`clipboardData.getData('text/plain')` (strip all rich HTML), insert as a text node at the
caret. No auto-mention-detection in pasted text (the user re-triggers with `@`).

**No new escaping — deliberate parity.** `serializeSegments` concatenates text verbatim +
`<@id>` per chip, with **no** escaping of user-typed/pasted text. The render side is frozen,
so any write-side escaping would need an unescape step on a side we can't touch. A user who
literally types/pastes `<@32-hex>` gets it rendered as a mention — exactly today's behavior,
the "astronomically unlikely, documented, not handled" accepted-minor from the ZEB-588 spec.
Matching it keeps the wire contract provably unchanged.

**IME/composition.** Track `compositionstart`/`compositionend`; suppress trigger detection
during composition (run it once on `compositionend`) and guard Enter via `isComposing`.

## Draft persistence across channel switches

The draft (text + chips) lives in the `MentionInput` DOM. Today's shared draft is
deliberately preserved across channel switches (only the now-obsolete `tracked[]` reset), so
this "just works" as long as the component instance persists across switches — which it
already does. On `channelId` change we do **not** clear the input; `clear()` runs only after
a successful post. This is strictly better than today, which silently downgraded pending
mentions to plain text on switch.

**Accepted edge:** a preserved chip whose owner isn't in the newly-selected channel still
serializes as a valid `<@id>` — the backend validates hex + the 64-cap and computes "mentions
me" per-recipient, so membership isn't a client-side gate. Documented as an accepted minor
rather than adding cross-channel chip-scrubbing (out of scope, arguably worse UX).

## Accessibility

`role="textbox"`, `aria-multiline="true"`, `aria-label` (from prop, mirrors today's
"Channel message"), CSS placeholder via `data-placeholder` + `:empty::before`. Chip text
content is the human `@Label`, so screen readers read "@Jake (Koya)" inline. `disabled` /
posting → whole input `contenteditable="false"` + `aria-disabled="true"`. The dropdown keeps
its existing `role="listbox"`/`option`/`aria-selected`. (Deep SR chip-deletion announcements
= the deferred tail.)

## Edge cases (covered by tests)

- Empty / whitespace-only → no send (matches today; parent trims the body).
- Chip-only message → body `<@id>`, `mentions: [id]`.
- Adjacent chips / chip-at-start / chip-at-end → serialize correctly.
- Same member mentioned twice → two chips, `mentions[]` first-seen dedup.
- Backend "too many mentions: N (max 64)" → surfaced via the existing `composeError` path.
- Posting/disabled → Enter is a no-op; input non-editable.

## Testing (frontend-only — `npx tsc --noEmit` + `npx vitest run`, the `frontend` CI job)

- **Pure** — `mention-compose.test.ts`: retarget the `reconcileCompose` output-contract
  cases (dedup order, adjacency, chip-at-start/end, mixed text+chip, empty) onto
  `serializeSegments`; keep `detectMentionTrigger` / `filterCandidates` suites; delete the
  `shiftTrackedSpans` / `applyMentionPick` / position-scan tests (functions removed).
- **Component** — new `MentionInput.test.ts` (jsdom): `@` → open → pick inserts a chip with
  `data-owner-id`; Enter emits `{ body, mentions }`; Shift+Enter newline, no send;
  multi-line (Shift+Enter) content serializes with `\n` in the body; Backspace after a chip
  deletes the whole chip; paste inserts plain text (HTML stripped); **Enter during IME
  composition does not send**; empty/whitespace no send.
- **Existing** — `ChannelMessageFeed.test.ts`: rewire the compose-driving tests (Enter posts,
  Shift+Enter, pick→Enter → body-token + mentions, plain → no-mentions) to drive
  `MentionInput`; drop the obsolete span-rebase test; the render-side tests (token → chip,
  self / `mentions-me` highlight) stay untouched (render is frozen).

No Rust changes.

## File summary

| File | Change |
|---|---|
| `src/lib/components/MentionInput.svelte` | **new** — contenteditable surface, chips, caret/keyboard/paste/IME, autocomplete wiring |
| `src/lib/mention-compose.ts` | add pure `serializeSegments` + `Segment` type; remove `TrackedMention.start`, `shiftTrackedSpans`, `applyMentionPick`, `reconcileCompose`; keep `detectMentionTrigger`, `filterCandidates` |
| `src/lib/components/ChannelMessageFeed.svelte` | drop textarea + span wiring; render `<MentionInput>`; `handleCompose` → `onSend` handler + `clear()` on success |
| `src/lib/mention-compose.test.ts` | retarget output-contract tests to `serializeSegments`; drop retired-fn tests |
| `src/lib/components/__tests__/MentionInput.test.ts` | **new** — component behavior |
| `src/lib/components/__tests__/ChannelMessageFeed.test.ts` | rewire compose tests to `MentionInput`; keep render-side tests |

`mention-render.ts`, the wire format, `MentionAutocomplete.svelte`, and upstream
candidate-building (`CommunityView` / `TownHallView`) are untouched.

## Non-goals

- DM / group-DM / thread mentions (no wire field; separate backend work).
- Any change to the `<@id>` + `mentions[]` wire format or the render path.
- Mobile/touch-specific hardening and deep screen-reader chip-deletion semantics (deferred
  fast-follow).
- Caret-following dropdown positioning (input-anchored, as today).
