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

/** Filter+rank the roster: case-insensitive substring on label; prefix matches
 *  first (stable partition); capped to `limit`. */
export function filterCandidates(
  candidates: MentionCandidate[],
  query: string,
  limit = 8,
): MentionCandidate[] {
  const q = query.trim().toLowerCase();
  if (q === '') return candidates.slice(0, limit);
  const matches = candidates.filter((c) => c.label.toLowerCase().includes(q));
  const prefix = matches.filter((c) => c.label.toLowerCase().startsWith(q));
  const rest = matches.filter((c) => !c.label.toLowerCase().startsWith(q));
  return [...prefix, ...rest].slice(0, limit);
}

/** Reconcile the textarea text + picks into the wire payload. Single
 *  left-to-right scan; at each index, among UNCONSUMED tracked entries whose
 *  '@<label>' matches there, take the longest label (prefix safety), tie-broken
 *  by insertion order (FIFO → same-label entries map left-to-right). Unmatched
 *  picks degrade to plain text; the mentions array dedupes in first-seen order. */
export function reconcileCompose(
  text: string,
  tracked: TrackedMention[],
): { body: string; mentions: string[] } {
  const consumed = new Array(tracked.length).fill(false);
  const order = tracked
    .map((_, idx) => idx)
    .sort((a, b) => {
      const d = tracked[b].label.length - tracked[a].label.length;
      return d !== 0 ? d : a - b; // longer first; original order tiebreak
    });
  let body = '';
  const mentions: string[] = [];
  let i = 0;
  while (i < text.length) {
    let pick = -1;
    for (const idx of order) {
      if (consumed[idx]) continue;
      const label = tracked[idx].label;
      if (!text.startsWith(`@${label}`, i)) continue;
      // Left boundary: the '@' must start the text or follow whitespace, so a
      // mention merged into a word/email ("a@Jake") is never rewritten.
      if (i !== 0 && !/\s/.test(text[i - 1])) continue;
      // Right boundary: the char after the label must be end-of-text or
      // whitespace, so an edited pick ("@JakeX" / "@Jake2") degrades to plain
      // text instead of corrupting the body with "<@id>X" (Qodo/CodeRabbit).
      const j = i + 1 + label.length;
      if (j !== text.length && !/\s/.test(text[j])) continue;
      pick = idx;
      break;
    }
    if (pick >= 0) {
      consumed[pick] = true;
      body += `<@${tracked[pick].ownerId}>`;
      if (!mentions.includes(tracked[pick].ownerId)) mentions.push(tracked[pick].ownerId);
      i += tracked[pick].label.length + 1; // skip '@' + label
    } else {
      body += text[i];
      i++;
    }
  }
  return { body, mentions };
}
