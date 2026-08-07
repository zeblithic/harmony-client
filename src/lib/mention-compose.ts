/**
 * ZEB-588 — pure compose-side helpers for @-mentions in channel messages.
 *
 * The compose flow over a plain textarea: `detectMentionTrigger` finds an active
 * `@query` at the caret; `filterCandidates` ranks the roster for the dropdown;
 * `applyMentionPick` inserts the chosen `@Label ` and yields the TrackedMention;
 * `reconcileCompose` rewrites the still-intact picks into stable `<@ownerId>`
 * wire tokens + the denormalized mentions array on send. All pure, no DOM.
 */

export interface MentionCandidate {
  ownerId: string;
  label: string;
}

/** A compose segment: free text, or an atomic mention (chip) carrying its ownerId.
 *  The structural successor to the flat-text `TrackedMention` model (ZEB-594). */
export type Segment =
  | { type: 'text'; text: string }
  | { type: 'mention'; ownerId: string };

/** Serialize compose segments into the frozen wire payload: text verbatim, each
 *  mention as a `<@ownerId>` token, plus the first-seen-deduped mentions array.
 *  A chip carries its ownerId directly, so there is nothing to reconcile — this
 *  replaces reconcileCompose. No escaping of text: the render side is frozen and a
 *  literal `<@32hex>` rendering as a mention is the documented accepted-minor. */
export function serializeSegments(segments: Segment[]): { body: string; mentions: string[] } {
  let body = '';
  const mentions: string[] = [];
  for (const seg of segments) {
    if (seg.type === 'mention') {
      body += `<@${seg.ownerId}>`;
      if (!mentions.includes(seg.ownerId)) mentions.push(seg.ownerId);
    } else {
      body += seg.text;
    }
  }
  return { body, mentions };
}

export interface TrackedMention {
  ownerId: string;
  label: string;
  /** Offset of the '@' of this pick's inserted "@label" in the current compose
   *  text. Span = [start, start + 1 + label.length), matching "@label" exactly
   *  (the trailing space is a boundary). Maintained by shiftTrackedSpans across
   *  edits; reconcileCompose matches by this offset, not by label rescan. */
  start: number;
}

/** Detect an active @-trigger at the caret. The '@' must be at start-of-text or
 *  preceded by whitespace; everything from '@' to the caret must be
 *  non-whitespace. Returns the query (after '@') and the '@' index, or null. */
export function detectMentionTrigger(
  text: string,
  caret: number,
): { query: string; atIndex: number } | null {
  for (let i = caret - 1; i >= 0; i--) {
    const ch = text[i];
    if (ch === '@') {
      const before = i === 0 ? '' : text[i - 1];
      if (i === 0 || /\s/.test(before)) {
        return { query: text.slice(i + 1, caret), atIndex: i };
      }
      return null; // '@' not at a word boundary (e.g. email)
    }
    if (/\s/.test(ch)) return null; // whitespace before any '@' → no trigger
  }
  return null;
}

/** Replace the '@query' range [atIndex, caret) with '@<label> ' and return the
 *  new text/caret + the TrackedMention to append. */
export function applyMentionPick(
  text: string,
  atIndex: number,
  caret: number,
  candidate: MentionCandidate,
): { text: string; caret: number; tracked: TrackedMention } {
  const insert = `@${candidate.label} `;
  const newText = text.slice(0, atIndex) + insert + text.slice(caret);
  return {
    text: newText,
    caret: atIndex + insert.length,
    tracked: { ownerId: candidate.ownerId, label: candidate.label, start: atIndex },
  };
}

/** Maintain tracked-mention spans across a single compose edit.
 *
 * Derives the one contiguous edit between `prevText` and `nextText` (longest
 * common prefix `p` + capped common suffix `s`), then for each span
 * [start, start + 1 + label.length):
 *   1. edit entirely at/after the span (p >= end)           → keep unchanged
 *   2. edit entirely before the span (prevLen - s <= start) → shift by delta
 *   3. edit overlaps the span                               → drop (invalidate)
 * Order is preserved. Fail-safe: an edit that touches a span removes it, so the
 * span can never be matched against text it no longer covers. */
export function shiftTrackedSpans(
  prevText: string,
  nextText: string,
  tracked: TrackedMention[],
): TrackedMention[] {
  if (prevText === nextText) return tracked;
  const prevLen = prevText.length;
  const nextLen = nextText.length;
  // Longest common prefix.
  let p = 0;
  const maxP = Math.min(prevLen, nextLen);
  while (p < maxP && prevText[p] === nextText[p]) p++;
  // Longest common suffix, capped so prefix and suffix don't overlap.
  let s = 0;
  const maxS = Math.min(prevLen - p, nextLen - p);
  while (s < maxS && prevText[prevLen - 1 - s] === nextText[nextLen - 1 - s]) s++;
  const delta = nextLen - prevLen;
  const editEnd = prevLen - s; // exclusive end of the edited region in prevText
  const out: TrackedMention[] = [];
  for (const m of tracked) {
    const end = m.start + 1 + m.label.length;
    if (p >= end) {
      out.push(m); // case 1: edit at/after the span
    } else if (editEnd <= m.start) {
      const shifted = m.start + delta; // case 2: edit before the span
      if (shifted >= 0) out.push({ ...m, start: shifted });
    }
    // else: case 3 — overlap → drop
  }
  return out;
}

/** Filter+rank the roster for the dropdown. Matches, in rank order:
 *  (1) label prefix, (2) label substring, (3) owner-id hex prefix — ZEB-774, so
 *  a peer still shown as raw hex (`@2e9a…`) is findable by the hex the user can
 *  see, not just by a name that hasn't propagated yet. The partition is mutually
 *  exclusive: a candidate matched by its label is never re-surfaced as a hex
 *  match, so name matches keep their exact prior order and hex-only matches are
 *  purely additive. Case-insensitive; capped to `limit`. */
export function filterCandidates(
  candidates: MentionCandidate[],
  query: string,
  limit = 8,
): MentionCandidate[] {
  const q = query.trim().toLowerCase();
  if (q === '') return candidates.slice(0, limit);
  const labelPrefix: MentionCandidate[] = [];
  const labelSubstr: MentionCandidate[] = [];
  const hexPrefix: MentionCandidate[] = [];
  for (const c of candidates) {
    const label = c.label.toLowerCase();
    if (label.startsWith(q)) labelPrefix.push(c);
    else if (label.includes(q)) labelSubstr.push(c);
    else if (c.ownerId.toLowerCase().startsWith(q)) hexPrefix.push(c);
  }
  return [...labelPrefix, ...labelSubstr, ...hexPrefix].slice(0, limit);
}

/** Reconcile the textarea text + picks into the wire payload. Each pick owns the
 *  exact offset where its '@label' was inserted (`start`), so we match a pick
 *  ONLY at its anchor — a manually-retyped identical label elsewhere is never
 *  claimed. Boundary guards are retained as defense-in-depth: a span left
 *  adjacent to appended text (e.g. "@JakeX") still degrades to plain text rather
 *  than corrupting the body. Unmatched picks degrade to plain text; the mentions
 *  array dedupes in first-seen order. */
export function reconcileCompose(
  text: string,
  tracked: TrackedMention[],
): { body: string; mentions: string[] } {
  const byStart = new Map<number, TrackedMention>();
  for (const m of tracked) {
    if (!byStart.has(m.start)) byStart.set(m.start, m); // first-wins on a dup start
  }
  let body = '';
  const mentions: string[] = [];
  let i = 0;
  while (i < text.length) {
    const m = byStart.get(i);
    if (m && text.startsWith(`@${m.label}`, i)) {
      // Left boundary: '@' must start the text or follow whitespace.
      const leftOk = i === 0 || /\s/.test(text[i - 1]);
      // Right boundary: end-of-text or whitespace after the label.
      const j = i + 1 + m.label.length;
      const rightOk = j === text.length || /\s/.test(text[j]);
      if (leftOk && rightOk) {
        body += `<@${m.ownerId}>`;
        if (!mentions.includes(m.ownerId)) mentions.push(m.ownerId);
        i = j;
        continue;
      }
    }
    body += text[i];
    i++;
  }
  return { body, mentions };
}
