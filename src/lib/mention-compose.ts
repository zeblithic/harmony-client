/**
 * Pure compose-side helpers for @-mentions in channel messages (ZEB-588,
 * ZEB-594). `detectMentionTrigger` finds an active `@query` at the caret;
 * `filterCandidates` ranks the roster for the dropdown; `serializeSegments`
 * turns the contenteditable's segments into `<@ownerId>` wire tokens + the
 * denormalized mentions array on send. All pure, no DOM. (ZEB-594 retired the
 * flat-text span model — applyMentionPick / shiftTrackedSpans / reconcileCompose
 * — for atomic chip DOM nodes; see mention-dom.ts + MentionInput.svelte.)
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
