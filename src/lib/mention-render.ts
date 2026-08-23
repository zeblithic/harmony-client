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

import { nonEmpty, type ResolvedName } from './display-label';

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
 *  Returns the BARE label (no leading '@'); the mention render template adds it.
 *
 *  ZEB-977: returns a `ResolvedName` — the label plus WHERE it came from — so
 *  visual sites can style by provenance via `PeerName.svelte` (a card-sourced
 *  name must never render in petname style). Text-only contexts use `.label`. */
export function resolveMentionLabel(
  ownerId: string,
  resolveNickname?: (id: string) => string | undefined,
  resolveCard?: (id: string) => { displayName: string } | undefined,
  resolveRosterName?: (id: string) => string | undefined,
): ResolvedName {
  const petname = nonEmpty(resolveNickname?.(ownerId));
  if (petname !== undefined) return { label: petname, source: 'petname' };
  const card = nonEmpty(resolveCard?.(ownerId)?.displayName);
  if (card !== undefined) return { label: card, source: 'card' };
  const roster = nonEmpty(resolveRosterName?.(ownerId));
  if (roster !== undefined) return { label: roster, source: 'roster' };
  return { label: ownerId.slice(0, 8), source: 'hex' };
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
 *
 * ZEB-977: returns a `ResolvedName` (label + provenance) — the wire-supplied
 * `senderName` rung surfaces as `source: 'wire'` so visual sites can mark it
 * unverified, and only the petname rung can earn the petname badge.
 */
export function resolveAuthorLabel(
  sender: { address: string; displayName: string },
  resolveNickname?: (id: string) => string | undefined,
  resolveCard?: (id: string) => { displayName: string } | undefined,
): ResolvedName {
  if (sender.address === SELF_ADDRESS) {
    return { label: sender.displayName, source: 'self' };
  }
  const petname = nonEmpty(resolveNickname?.(sender.address));
  if (petname !== undefined) return { label: petname, source: 'petname' };
  const card = nonEmpty(resolveCard?.(sender.address)?.displayName);
  if (card !== undefined) return { label: card, source: 'card' };
  const wire = nonEmpty(sender.displayName);
  if (wire !== undefined) return { label: wire, source: 'wire' };
  return { label: sender.address.slice(0, 8), source: 'hex' };
}
