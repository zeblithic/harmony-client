<script lang="ts">
  import type { NavNode, DisplayMode, SortOrder } from '../types';
  import { getChildNodes, sortNodes, getColorAncestry, getInheritedDisplayMode, getInheritedSortOrder } from '../nav-utils';
  import NavNodeRow from './NavNodeRow.svelte';
  import NavTree from './NavTree.svelte';
  import ProposalsNavRow from './ProposalsNavRow.svelte';
  import AddChannelNavRow from './AddChannelNavRow.svelte';

  let {
    nodes,
    parentId,
    activeNodeId,
    onToggle,
    onClick,
    onDisplayModeChange,
    onSortOrderChange,
    profileLookup,
    presenceOnline,
    filterTop,
    proposalCount,
    onSelectProposals,
    proposalsActiveFor,
    canManageChannels,
    onAddChannel,
    onRenameChannel,
    onDeleteChannel,
  }: {
    nodes: NavNode[];
    parentId: string | null;
    activeNodeId?: string | null;
    onToggle?: (id: string) => void;
    onClick?: (id: string) => void;
    onDisplayModeChange?: (nodeId: string, mode: DisplayMode) => void;
    onSortOrderChange?: (nodeId: string, order: SortOrder) => void;
    profileLookup?: (address: string) => string | undefined;
    /** ZEB-600: presence-dot resolver, threaded down to every NavNodeRow. */
    presenceOnline?: (node: NavNode) => boolean;
    /** ZEB-606: keep only matching nodes at THIS level. Passed by NavPanel at
     *  the root to partition top-level nodes into headed sections; recursive
     *  calls below do not thread it, so descendants are never filtered. */
    filterTop?: (n: NavNode) => boolean;
    /** ZEB-606: active Tier-2 count resolver (undefined = feature absent). */
    proposalCount?: (node: NavNode) => number | undefined;
    /** ZEB-606: open the community's Proposals view. */
    onSelectProposals?: (communityId: string) => void;
    /** ZEB-606: community id whose Proposals view is currently open. */
    proposalsActiveFor?: string | null;
    /** ZEB-663: may the viewer manage the given community's channels? */
    canManageChannels?: (communityId: string) => boolean;
    /** ZEB-663: open the create-channel dialog for a community. */
    onAddChannel?: (communityId: string) => void;
    /** ZEB-663: open rename dialog for a channel node. */
    onRenameChannel?: (communityId: string, channelId: string) => void;
    /** ZEB-663: open delete-confirm for a channel node. */
    onDeleteChannel?: (communityId: string, channelId: string) => void;
  } = $props();

  let sortedChildren = $derived.by(() => {
    const children = getChildNodes(nodes, parentId);
    const kept = filterTop ? children.filter(filterTop) : children;
    const order = parentId ? getInheritedSortOrder(nodes, parentId) : 'activity';
    return sortNodes(kept, order);
  });
</script>

{#each sortedChildren as child, i (child.id)}
  {@const ancestry = getColorAncestry(nodes, child.id)}
  {@const dm = getInheritedDisplayMode(nodes, child.id)}
  {@const isLast = i === sortedChildren.length - 1 && ancestry.length > 0}
  {@const forkParentNode = child.forkedFrom != null ? nodes.find((n) => n.id === child.forkedFrom) : undefined}

  <NavNodeRow
    node={child}
    colorAncestry={ancestry}
    displayMode={dm}
    isLastChild={isLast}
    active={activeNodeId === child.id}
    statusText={child.peer && profileLookup ? profileLookup(child.peer.address) : undefined}
    forkParentName={forkParentNode?.name ?? null}
    {onToggle}
    {onClick}
    {onDisplayModeChange}
    {onSortOrderChange}
    {presenceOnline}
    canManageChannel={(n) => (canManageChannels && n.parentId ? canManageChannels(n.parentId) : false)}
    {onRenameChannel}
    {onDeleteChannel}
  />

  {#if (child.type === 'folder' || child.type === 'community') && child.expanded}
    <NavTree nodes={nodes} parentId={child.id} {activeNodeId} {onToggle} {onClick} {onDisplayModeChange} {onSortOrderChange} {profileLookup} {presenceOnline} {proposalCount} {onSelectProposals} {proposalsActiveFor} {canManageChannels} {onAddChannel} {onRenameChannel} {onDeleteChannel} />
    {#if child.type === 'community' && proposalCount && onSelectProposals}
      <ProposalsNavRow
        communityId={child.id}
        indent={ancestry.length}
        count={proposalCount(child)}
        active={proposalsActiveFor === child.id}
        onSelect={() => onSelectProposals(child.id)}
      />
    {/if}
    {#if child.type === 'community' && canManageChannels && canManageChannels(child.id)}
      <AddChannelNavRow communityId={child.id} indent={ancestry.length} onAdd={onAddChannel} />
    {/if}
  {/if}
{/each}
