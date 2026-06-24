/**
 * ZEB-553 item 11: decide whether a completing roster fetch may clear the
 * `membersLoading` flag.
 *
 * `membersLoading` is owned by `refreshCommunityMembers` — set when an initial
 * (empty-roster) fetch starts, cleared when it settles. Across a community
 * switch two fetches can be in flight at once, so a completing fetch must NOT
 * clear the flag when a *different* community is now selected: that newer
 * switch's own fetch owns its loading state, and clearing here would wipe the
 * loading row the new community is still showing.
 *
 * It DOES clear when:
 *  - the fetch is still for the active community (`active === fetchId`), or
 *  - no community is selected at all (`active === null`) — the user navigated
 *    away to Notes/DMs mid-fetch. Nothing community-scoped is rendered then, so
 *    the flag has no owner and must not stay stuck `true` until the next switch
 *    happens to reset it (Qodo finding, PR #332).
 */
export function shouldClearMembersLoading(
  activeCommunityId: string | null,
  fetchCommunityId: string,
): boolean {
  return activeCommunityId === fetchCommunityId || activeCommunityId === null;
}
