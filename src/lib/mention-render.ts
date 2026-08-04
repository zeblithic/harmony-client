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

import { nonEmpty } from './display-label';

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

/** The single shared resolution ladder: local nickname → broadcast profile
 *  displayName → roster-DTO displayName → `ownerId.slice(0, 8)`. Empty/whitespace
 *  values count as absent (via the shared `nonEmpty`, same as
 *  `ChannelMessageFeed.authorLabel` and `MemberRow` — ZEB-432/ZEB-774).
 *
 *  The optional `resolveRosterName` rung (ZEB-774) lets a member the roster
 *  already named (via `list_community_members`' `displayName`, ZEB-777) degrade
 *  to that name rather than raw hex while the profile card is still propagating.
 *  It sits below the live card so a fresher broadcast name always wins; it
 *  mirrors the 4-rung ladder in `MemberRow.svelte`. Callers that don't thread it
 *  keep the original 3-rung behavior.
 *
 *  Returns the BARE label (no leading '@'); the mention render template adds it. */
export function resolveMentionLabel(
  ownerId: string,
  resolveNickname?: (id: string) => string | undefined,
  resolveCard?: (id: string) => { displayName: string } | undefined,
  resolveRosterName?: (id: string) => string | undefined,
): string {
  return (
    nonEmpty(resolveNickname?.(ownerId)) ??
    nonEmpty(resolveCard?.(ownerId)?.displayName) ??
    nonEmpty(resolveRosterName?.(ownerId)) ??
    ownerId.slice(0, 8)
  );
}

/** Sentinel `Peer.address` for locally-authored messages (`message-service`
 *  maps the self echo onto it). It is NOT an owner_id, so it must never enter
 *  the resolution ladder — `'self'.slice(0, 8)` would render as "self". */
const SELF_ADDRESS = 'self';

/**
 * ZEB-839 — the author-label ladder for a feed `Message.sender`.
 *
 * Shares the {@link resolveMentionLabel} backbone (nickname ► broadcast
 * profile-card name ► short hex) but carries two message-specific rungs in place
 * of that ladder's community-roster rung (a DM peer has no community roster): the
 * `self` sentinel short-circuits to the locally-known label, and a wire-supplied
 * `senderName` (channel messages carry one; DMs do not) sits just above the hex
 * fallback.
 *
 * Call this at RENDER time, never at message-arrival time — that is what lets
 * the label fill in as cards and nicknames arrive. DM authors deliberately
 * carry no baked name (`message-service.ts`), so an unresolved DM peer lands
 * on the hex rung here rather than being frozen there at ingest.
 */
export function resolveAuthorLabel(
  sender: { address: string; displayName: string },
  resolveNickname?: (id: string) => string | undefined,
  resolveCard?: (id: string) => { displayName: string } | undefined,
): string {
  if (sender.address === SELF_ADDRESS) return sender.displayName;
  return (
    nonEmpty(resolveNickname?.(sender.address)) ??
    nonEmpty(resolveCard?.(sender.address)?.displayName) ??
    nonEmpty(sender.displayName) ??
    sender.address.slice(0, 8)
  );
}
