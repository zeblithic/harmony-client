<script lang="ts">
  /**
   * ZEB-606: right-rail host for messages mode — a third occupant of the
   * existing Layout media cell, implemented entirely App-side so
   * Layout.svelte (resize/collapse/prefs/aria contract) is untouched.
   *
   * When a community is active AND a votingAdapter exists, a two-button
   * segmented toggle (aria-pressed, matching the nav footer mode-switcher;
   * ZEB-646) swaps AssemblyRail vs the existing MediaFeed; the last choice persists
   * device-scoped (harmony-rail-tab). Outside community contexts (DMs, no
   * selection) the rail is media-only with no tab chrome — pixel-identical
   * to the pre-ZEB-606 experience.
   */
  import type { CommunityMember, Message } from '../types';
  import type { TrustService } from '../trust-service';
  import type { ResolvedCard } from '../member-card-service';
  import type { VotingAdapter } from '../voting-adapter';
  import { loadRailTab, saveRailTab, type RailTab } from '../media-panel-prefs';
  import AssemblyRail from './AssemblyRail.svelte';
  import MediaFeed from './MediaFeed.svelte';

  let {
    communityId = null,
    votingAdapter,
    myAddr = '',
    communityMembers = [],
    onViewAllProposals,
    messages,
    trustService,
    trustVersion = 0,
    threadMessageIds = new Set<string>(),
    onLinkBack,
    onAvatarClick,
    onTrustChange,
    resolveNickname,
    resolveCard,
  }: {
    /** Active community, or null outside community contexts (media-only). */
    communityId?: string | null;
    votingAdapter?: VotingAdapter;
    /** Caller's OwnerAddr + roster — delegate context for the rail cards
     *  (PR #408 Greptile P1). */
    myAddr?: string;
    communityMembers?: CommunityMember[];
    /** Deep-link to the community's Proposals view ("View all"). */
    onViewAllProposals?: (communityId: string) => void;
    /* MediaFeed pass-through (same contract as before ZEB-606): */
    messages: Message[];
    trustService: TrustService;
    trustVersion?: number;
    threadMessageIds?: Set<string>;
    onLinkBack?: (messageId: string) => void;
    onAvatarClick?: (address: string, event: MouseEvent) => void;
    onTrustChange?: () => void;
    // ZEB-962: author-ladder resolvers, passed through to the media cards.
    resolveNickname?: (ownerIdHex: string) => string | undefined;
    resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined;
  } = $props();

  let railTab = $state<RailTab>(loadRailTab());
  let assemblyAvailable = $derived(communityId != null && votingAdapter != null);
  let showAssembly = $derived(assemblyAvailable && railTab === 'assembly');

  function selectTab(tab: RailTab) {
    railTab = tab;
    saveRailTab(tab);
  }
</script>

{#if assemblyAvailable}
  <div class="rail-tabs" role="group" aria-label="Right rail content">
    <button
      type="button"
      class="rail-tab"
      class:active={railTab === 'assembly'}
      aria-pressed={railTab === 'assembly'}
      onclick={() => selectTab('assembly')}
    >⚖ Assembly</button>
    <button
      type="button"
      class="rail-tab"
      class:active={railTab === 'media'}
      aria-pressed={railTab === 'media'}
      onclick={() => selectTab('media')}
    >Media</button>
  </div>
{/if}
{#if showAssembly && communityId != null && votingAdapter}
  <AssemblyRail
    {communityId}
    adapter={votingAdapter}
    {myAddr}
    {communityMembers}
    onViewAllProposals={() => onViewAllProposals?.(communityId)}
  />
{:else}
  <MediaFeed {messages} {trustService} {trustVersion} {threadMessageIds} {onLinkBack} {onAvatarClick} {onTrustChange} {resolveNickname} {resolveCard} />
{/if}

<style>
  .rail-tabs {
    display: inline-flex;
    border: 1px solid var(--border);
    border-radius: 6px;
    overflow: hidden;
    font-size: 0.75rem;
    margin-bottom: 10px;
    align-self: flex-start;
  }
  .rail-tab {
    padding: 4px 12px;
    border: none;
    background: none;
    cursor: pointer;
    color: var(--text-secondary);
    user-select: none;
  }
  .rail-tab:not(:last-child) {
    border-right: 1px solid var(--border);
  }
  .rail-tab.active {
    background: var(--primary-soft);
    color: var(--primary-deep);
  }
</style>
