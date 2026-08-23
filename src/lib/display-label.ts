/**
 * Treat an empty or whitespace-only string as ABSENT, so a label ladder built
 * with `??` falls through to the next source instead of rendering a blank name.
 *
 * Profile-card / petname names have no non-empty constraint at publish
 * (the backend card publish only caps length — `profile_card_broadcast.rs`), so
 * a peer can legitimately carry `display_name = ""` / `"   "`. Plain
 * `nullish ?? next` would treat that empty string as a valid label and stop the
 * fallback, rendering a blank member name + a blank avatar `aria-label`. Wrap
 * each ladder source in `nonEmpty()` so only a present, non-blank string wins.
 *
 * Shared by the community label-ladder surfaces (ChannelMembersPanel,
 * MemberRow, ChannelMessageFeed) so they resolve names identically (ZEB-432).
 */
export function nonEmpty(value: string | null | undefined): string | undefined {
  return value != null && value.trim() !== '' ? value : undefined;
}

/**
 * ZEB-977: where a resolved peer name CAME FROM. Carried alongside the label
 * (see {@link ResolvedName}) so render sites can style by provenance — the
 * anti-impersonation invariant is that a name the peer chose (`card` / `wire`)
 * must never be renderable in the style of a name YOU assigned (`petname`).
 * `PeerName.svelte` is the one place that styles by this field.
 *
 *  - `petname`: your local name for this identity (contacts dataset) —
 *    unforgeable by the peer; the only source that earns the petname badge.
 *  - `card`:    the displayName the peer broadcast in their signed profile
 *    card — verified as THEIRS, but not unique; anyone can publish any name.
 *  - `roster`:  a display name baked into a community-roster DTO (an older
 *    card capture) — same trust class as `card`.
 *  - `wire`:    a free-text name string carried in the message itself
 *    (channel `senderName`) — unverified.
 *  - `hex`:     the identity-prefix fallback.
 *  - `self`:    the locally-known label for the local user.
 */
export type NameSource = 'petname' | 'card' | 'roster' | 'wire' | 'hex' | 'self';

/** A resolved peer name plus its provenance. Text-only contexts (aria-labels,
 *  confirm dialogs, notification bodies) use `.label`; visual contexts render
 *  through `PeerName.svelte` so provenance styling stays centralized. */
export interface ResolvedName {
  label: string;
  source: NameSource;
}

/**
 * Name-ONLY ladder for the call/voice cluster: prefer a non-blank petname,
 * else a non-blank `cardDisplayName`, else `undefined`.
 *
 * Takes the two candidate names as VALUES, not resolvers, so a caller that has
 * already resolved the peer's card (e.g. for its avatar) passes
 * `card?.displayName` straight in rather than paying a second card lookup — the
 * voice roster re-resolves on every ~10 Hz drain tick, so a redundant per-member
 * resolver call there is not free.
 *
 * Unlike the full identity ladder (`resolveMentionLabel`, which ends in
 * `slice(0, 8)`), this deliberately omits a hex fallback so each call/voice leaf
 * keeps its own established hex format — the in-call bars render
 * `hex.slice(0, 6) + '…'`, the incoming-call toasts render `hex.slice(0, 8)`.
 * The `nonEmpty()` guards are load-bearing: a peer can broadcast a
 * whitespace-only card / petname, and a plain `nickname ?? cardDisplayName`
 * would render that blank string as a name. There is no roster rung — call/voice
 * peers are DM / group-DM / channel-voice participants with no roster-name
 * resolver in scope (mirroring `resolveAuthorLabel`, which drops the roster rung
 * too).
 */
export function resolveMemberName(
  nickname: string | undefined,
  cardDisplayName: string | undefined,
): ResolvedName | undefined {
  const petname = nonEmpty(nickname);
  if (petname !== undefined) return { label: petname, source: 'petname' };
  const card = nonEmpty(cardDisplayName);
  if (card !== undefined) return { label: card, source: 'card' };
  return undefined;
}
