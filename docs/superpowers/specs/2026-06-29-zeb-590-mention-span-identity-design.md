# ZEB-590: stable per-insertion span identity for tracked mentions

**Status:** design approved (Jake, 2026-06-29). Successor epic ZEB-594 (contenteditable-chips) filed as the structural long-term direction; out of scope here.

## Goal

Fix the one residual @-mention corruption case left by ZEB-588 / PR #361: within a single channel session, deleting a picked mention's text entirely and then *manually re-typing the identical* `@label` causes the stale `TrackedMention` to claim the re-typed text — emitting a `<@ownerId>` token + a `mentions[]` entry for a mention the user never picked.

Root cause: `TrackedMention` is `{ ownerId, label }` with **no positional identity**, so `reconcileCompose` matches picks by **label string** at send time. A manual retype of the same label is indistinguishable from the original pick.

Fix: give each pick **stable span identity** (the offset of its inserted `@label`), maintain that span across edits, and **invalidate** a pick whose span is edited or deleted. Reconciliation then matches by position, not by label rescan.

## Non-goals

- The contenteditable-chips compose rewrite (structural atomicity) — tracked separately as **ZEB-594**. This spec stays entirely within the existing plain-`<textarea>` model.
- Any change to mention *rendering* (`mention-render.ts`), notifications, or the wire format (`<@ownerId>` tokens + `mentions[]` are unchanged).

## Architecture

All new logic lives in the existing pure module `src/lib/mention-compose.ts` (no DOM, exhaustively unit-testable). The Svelte consumer `src/lib/components/ChannelMessageFeed.svelte` gains a small amount of wiring to feed edits into the new pure function and to maintain a `prevComposeText` shadow. The fail-safe direction is preserved and strengthened: **the worst case is a mention that should have tokenized degrading to literal text; a wrong id is never emitted.**

### 1. Data model

`TrackedMention` gains one field:

```ts
export interface TrackedMention {
  ownerId: string;
  label: string;
  start: number; // offset of the '@' of this pick's inserted "@label" in the current compose text
}
```

A pick's **span** is the half-open range `[start, start + 1 + label.length)`, matching exactly the substring `@${label}`. The trailing space that `applyMentionPick` inserts (`@${label} `) is a **boundary**, not part of the span.

`applyMentionPick` sets `start = atIndex` (the offset where it inserts the `@`). Its `text`/`caret` results are otherwise unchanged.

### 2. `shiftTrackedSpans(prevText, nextText, tracked) → TrackedMention[]`

Pure function called on every compose edit. It derives the single contiguous edit between `prevText` and `nextText` and shifts / keeps / invalidates each tracked span accordingly.

**Edit derivation (common prefix + suffix):**
- `p` = length of the longest common prefix of `prevText` and `nextText`.
- `s` = length of the longest common suffix, capped so prefix and suffix do not overlap: `s = min(commonSuffixLen, prevText.length - p, nextText.length - p)`.
- The edit replaced the prev range `[p, prevText.length - s)` with the next range `[p, nextText.length - s)`.
- `delta = nextText.length - prevText.length`.
- (If `prevText === nextText`, `p = prevText.length`, `s = 0`, edit range is empty — all spans kept unchanged.)

**Per-span decision** (span `[start, end)`, `end = start + 1 + label.length`):
1. **Edit entirely at/after the span** — `p >= end`: keep unchanged.
2. **Edit entirely before the span** — `prevText.length - s <= start`: shift → `start += delta` (drop if the shift would make `start < 0`, which can't happen for a well-formed single edit but is guarded defensively).
3. **Edit overlaps the span** — otherwise: **drop** the entry (invalidate).

Returns the surviving (possibly shifted) entries, order preserved.

**Why this is correct and safe:**
- Typing/pasting *before* a mention shifts it (case 2). Typing immediately *after* it (`p == end`) leaves it (case 1); if that appended into the label (`@JakeX`), `reconcileCompose`'s right-boundary guard still refuses to tokenize it.
- Deleting or editing *inside* a mention overlaps the span → invalidated (case 3). This is precisely the delete-then-retype fix: the original entry is dropped on delete, and a manual retype produces no new entry (only `applyMentionPick` creates entries), so nothing claims the re-typed text.
- Multi-region edits (e.g. select-all-replace) collapse to one wide `[p, prevLen-s)` region; any span touching it is invalidated. Worst case an untouched mention far from a sweeping edit is dropped → degrades to plain text. Fail-safe, never a wrong id.

### 3. `reconcileCompose` — position-anchored matching

Replace the "longest-label / FIFO-among-unconsumed" label rescan with **position-anchored** matching:

- Build an index from `start` → surviving tracked entry.
- Single left-to-right scan of `text`. At position `i`, if a tracked entry is anchored at `i` **and** `text.startsWith('@' + label, i)` **and** the existing left/right boundary guards hold at `i` (left: `i === 0` or whitespace before; right: end-of-text or whitespace after the label), emit `<@ownerId>`, dedup-append `ownerId` to `mentions`, and advance `i` past the span. Otherwise copy `text[i]` and advance by one.

The boundary guards are retained as defense-in-depth (a span could in principle be left adjacent to appended text by case 1). The longest-label tiebreak and the `consumed[]` bookkeeping are removed — each entry owns an exact, distinct position, so there is nothing to disambiguate. `mentions[]` first-seen dedup is unchanged.

(Distinct spans never share a `start`: case-3 invalidation drops any entry whose span an edit touched, and `applyMentionPick` only ever inserts at a fresh caret position. A defensive first-wins on a duplicate `start` is acceptable and untestable in practice.)

### 4. Component wiring (`ChannelMessageFeed.svelte`)

- Add a `let prevComposeText = $state('')` shadow tracking the last text `shiftTrackedSpans` has seen.
- In `refreshTrigger` (the textarea `oninput` handler, which already reads the live `el.value`): before detecting the trigger, run
  `tracked = shiftTrackedSpans(prevComposeText, el.value, tracked);` then `prevComposeText = el.value;`
- In `applyMentionPick`'s handler (programmatic edit): after `composeText = r.text`, set `prevComposeText = r.text` so the next user-edit diff is taken against the post-insert text. The new entry's `start` is already in post-insert coordinates.
- On the two resets — channel/community switch and post-send — set `prevComposeText = ''` alongside `tracked = []`.
- The send-time call `reconcileCompose(composeText, tracked)` is unchanged at the call site (`composeText === prevComposeText` there, no pending edit).

## Testing

All core logic is pure → unit-tested in `src/lib/mention-compose.test.ts`.

**`shiftTrackedSpans` (new):**
- insert before a span → span shifts by `+delta`; reconcile still tokenizes.
- insert after a span (`p == end`) → unchanged; appended-into-label (`@JakeX`) still refuses to tokenize via boundary guard.
- delete the whole span → entry dropped.
- edit one char inside the span → entry dropped.
- **delete-then-retype regression (headline bug):** start with a tracked pick; `shiftTrackedSpans` through the deletion drops it; a subsequent identical `@label` text with the (now empty) tracked list reconciles to plain text, `mentions: []`.
- two mentions, edit between them → earlier unchanged, later shifted.
- paste-over-a-selection that spans a mention → that mention dropped, others adjusted.
- `prevText === nextText` (no-op edit) → all spans unchanged.

**`applyMentionPick`:** assert the returned `tracked.start === atIndex`.

**`reconcileCompose`:** existing cases updated to pass `start`; assert position-anchored behavior (a label-matching substring at a *different* offset than any tracked `start` is NOT tokenized — the positive form of the bug fix). Keep the existing boundary-guard and longest-label-coverage cases (now expressed via distinct `start`s).

**Component:** the logic is covered by the pure-function tests (matching the module's existing test strategy); no new Svelte-component test is required, but the wiring change must keep `npx tsc --noEmit` and `npx vitest run` green.

## Files

- Modify: `src/lib/mention-compose.ts` — `TrackedMention.start`; `applyMentionPick` sets `start`; new `shiftTrackedSpans`; `reconcileCompose` position-anchored.
- Modify: `src/lib/mention-compose.test.ts` — new `shiftTrackedSpans` suite; update `applyMentionPick` / `reconcileCompose` cases for `start`.
- Modify: `src/lib/components/ChannelMessageFeed.svelte` — `prevComposeText` shadow + `shiftTrackedSpans` call in the input handler + reset/insert bookkeeping.

## Gates

`npx tsc --noEmit` and `npx vitest run` green (the `frontend` CI job). No Rust changes.
