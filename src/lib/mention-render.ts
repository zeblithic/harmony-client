/**
 * ZEB-588 — pure render-side helpers for @-mentions in channel messages.
 *
 * A message body is UTF-8 text that may contain stable inline mention tokens
 * `<@<ownerIdHex>>` (32 lowercase hex). `tokenizeBody` splits the body into
 * text/mention segments so the renderer can show a styled, resolved `@Name`
 * without any innerHTML. `resolveMentionLabel` is the single shared resolution
 * ladder (also used by `authorLabel`): local nickname → broadcast profile name
 * → short hex.
 */

export type BodySegment =
  | { type: 'text'; text: string }
  | { type: 'mention'; ownerId: string };

/** Split a wire body into alternating text/mention segments by the
 *  `/<@([0-9a-f]{32})>/g` token. A body with no tokens yields one text segment;
 *  an empty string yields `[]`. */
export function tokenizeBody(text: string): BodySegment[] {
  const segments: BodySegment[] = [];
  const re = /<@([0-9a-f]{32})>/g;
  let lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = re.exec(text)) !== null) {
    if (m.index > lastIndex) {
      segments.push({ type: 'text', text: text.slice(lastIndex, m.index) });
    }
    segments.push({ type: 'mention', ownerId: m[1] });
    lastIndex = m.index + m[0].length;
  }
  if (lastIndex < text.length) {
    segments.push({ type: 'text', text: text.slice(lastIndex) });
  }
  return segments;
}

function present(v: string | undefined): string | undefined {
  return v && v.trim() ? v : undefined;
}

/** The single shared resolution ladder: local nickname → broadcast profile
 *  displayName → `ownerId.slice(0, 8)`. Empty/whitespace values count as absent.
 *  Returns the BARE label (no leading '@'); the mention render template adds it. */
export function resolveMentionLabel(
  ownerId: string,
  resolveNickname?: (id: string) => string | undefined,
  resolveCard?: (id: string) => { displayName: string } | undefined,
): string {
  return (
    present(resolveNickname?.(ownerId)) ??
    present(resolveCard?.(ownerId)?.displayName) ??
    ownerId.slice(0, 8)
  );
}
