/**
 * Treat an empty or whitespace-only string as ABSENT, so a label ladder built
 * with `??` falls through to the next source instead of rendering a blank name.
 *
 * Profile-card / friend-nickname names have no non-empty constraint at publish
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
 * Name-ONLY ladder for the call/voice cluster: climb `nickname → card` and stop,
 * returning `undefined` when neither yields a non-blank name.
 *
 * Unlike the full identity ladder (`resolveMentionLabel`, which ends in
 * `slice(0, 8)`), this deliberately omits a hex fallback so each call/voice leaf
 * keeps its own established hex format — the in-call bars render
 * `hex.slice(0, 6) + '…'`, the incoming-call toasts render `hex.slice(0, 8)`.
 * The `nonEmpty()` guards are load-bearing: a peer can broadcast a
 * whitespace-only card / nickname, and a plain `nick ?? card` would render that
 * blank string as a name. There is no roster rung — call/voice peers are
 * DM / group-DM / channel-voice participants with no roster-name resolver in
 * scope (mirroring `resolveAuthorLabel`, which drops the roster rung too).
 */
export function resolveMemberName(
  ownerHex: string,
  resolveNickname?: (id: string) => string | undefined,
  resolveCard?: (id: string) => { displayName?: string } | undefined,
): string | undefined {
  return nonEmpty(resolveNickname?.(ownerHex)) ?? nonEmpty(resolveCard?.(ownerHex)?.displayName);
}
