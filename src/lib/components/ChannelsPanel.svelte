<script lang="ts">
  /**
   * ZEB-965: the per-community channel list, rendered in CommunityView's
   * right-hand column as a toggleable view alongside the Members list. This
   * replaces the left-nav channel rows (NavTree no longer recurses into
   * communities), which fixes the "+ add channel" layout shift in the left
   * list and keeps channels reachable at narrow viewports where the left nav
   * collapses to icon squares.
   *
   * Fixed order per spec: proposals row on top (every community has one),
   * the community's channels in nav order (backend order: created_at asc,
   * general-first), then the ＋ add-channel row last for viewers with
   * sufficient power — pinned at the bottom so gaining/losing the permission
   * never shifts the rows above it.
   *
   * Channel rows reuse NavTree/NavNodeRow (mounted at parentId=communityId),
   * so unread/mention badges and the ⋯ rename/delete menu carry over from the
   * left nav unchanged.
   */
  import type { NavNode } from '../types';
  import NavTree from './NavTree.svelte';
  import ProposalsNavRow from './ProposalsNavRow.svelte';
  import AddChannelNavRow from './AddChannelNavRow.svelte';

  let {
    nodes,
    communityId,
    selectedChannelId,
    proposalsActive = false,
    proposalCount = undefined,
    canManage = false,
    initialSyncing = false,
    onSelectChannel,
    onSelectProposals,
    onAddChannel,
    onRenameChannel,
    onDeleteChannel,
  }: {
    /** Full nav-node array (NavService snapshot); NavTree scopes to communityId. */
    nodes: NavNode[];
    communityId: string;
    /** App-owned channel selection — drives the active row highlight. */
    selectedChannelId: string | null;
    /** True while the community's Proposals view is open. */
    proposalsActive?: boolean;
    /** Active Tier-2 proposal count; undefined = not yet known (no badge). */
    proposalCount?: number | undefined;
    /** May the viewer manage channels (add/rename/delete)? Power ≥ kick. */
    canManage?: boolean;
    /** ZEB-949 parity: freshly-joined community still syncing its channels. */
    initialSyncing?: boolean;
    onSelectChannel?: (channelId: string) => void;
    onSelectProposals?: () => void;
    onAddChannel?: () => void;
    onRenameChannel?: (communityId: string, channelId: string) => void;
    onDeleteChannel?: (communityId: string, channelId: string) => void;
  } = $props();

  let channelCount = $derived(
    nodes.filter((n) => n.parentId === communityId && n.type === 'channel').length,
  );
</script>

<aside class="channels-panel" aria-label="Community channels">
  <header class="panel-header">
    <span class="title">Channels</span>
  </header>
  <div class="channel-list">
    <ProposalsNavRow
      {communityId}
      indent={0}
      count={proposalCount}
      active={proposalsActive}
      onSelect={() => onSelectProposals?.()}
    />
    {#if initialSyncing && channelCount === 0}
      <!-- ZEB-949: freshly joined; channels are syncing in, not absent. -->
      <p class="channels-syncing" role="status" data-testid="channels-panel-syncing">
        Syncing channels…
      </p>
    {:else}
      <NavTree
        {nodes}
        parentId={communityId}
        activeNodeId={selectedChannelId}
        onClick={(id) => onSelectChannel?.(id)}
        canManageChannels={() => canManage}
        {onRenameChannel}
        {onDeleteChannel}
      />
    {/if}
    {#if canManage}
      <AddChannelNavRow {communityId} indent={0} onAdd={() => onAddChannel?.()} />
    {/if}
  </div>
</aside>

<style>
  /* Mirrors ChannelMembersPanel's panel anatomy so the two toggled views
     occupy the identical column footprint. */
  .channels-panel {
    display: flex;
    flex-direction: column;
    background: var(--bg-secondary);
    border-left: 1px solid var(--border);
    width: 200px;
    min-width: 0;
    overflow: hidden;
  }
  .panel-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 12px 14px 6px;
    border-bottom: 1px solid var(--border);
    color: var(--text-secondary);
    font-size: 0.75rem;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  .channel-list {
    display: flex;
    flex-direction: column;
    padding: 6px 0;
    overflow-y: auto;
    flex: 1;
  }
  .channels-syncing {
    margin: 0;
    padding: 10px 14px;
    color: var(--text-secondary);
    font-size: 0.8rem;
    font-style: italic;
  }
</style>
