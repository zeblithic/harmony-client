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
