<script lang="ts">
  /**
   * ZEB-606: right-rail host for messages mode — a third occupant of the
   * existing Layout media cell, implemented entirely App-side so
   * Layout.svelte (resize/collapse/prefs/aria contract) is untouched.
   *
   * Renders AssemblyRail when a community is active AND a votingAdapter
   * exists; otherwise the rail is empty (outside community contexts, or
   * when no voting adapter is available). The chat media surface that
   * used to occupy this slot's non-assembly fallback was removed — it
   * was frontend-mock-only with no backend.
   */
  import type { CommunityMember } from '../types';
  import type { VotingAdapter } from '../voting-adapter';
  import AssemblyRail from './AssemblyRail.svelte';

  let {
    communityId = null,
    votingAdapter,
    myAddr = '',
    communityMembers = [],
    onViewAllProposals,
  }: {
    /** Active community, or null outside community contexts. */
    communityId?: string | null;
    votingAdapter?: VotingAdapter;
    /** Caller's OwnerAddr + roster — delegate context for the rail cards
     *  (PR #408 Greptile P1). */
    myAddr?: string;
    communityMembers?: CommunityMember[];
    /** Deep-link to the community's Proposals view ("View all"). */
    onViewAllProposals?: (communityId: string) => void;
  } = $props();

  let assemblyAvailable = $derived(communityId != null && votingAdapter != null);
</script>

{#if assemblyAvailable && communityId != null && votingAdapter}
  <AssemblyRail
    {communityId}
    adapter={votingAdapter}
    {myAddr}
    {communityMembers}
    onViewAllProposals={() => onViewAllProposals?.(communityId)}
  />
{/if}
