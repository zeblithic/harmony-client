<script lang="ts">
  import type { NavNode, DisplayMode, SortOrder } from '../types';
  import { getChildNodes, sortNodes, getColorAncestry, getInheritedDisplayMode, getInheritedSortOrder } from '../nav-utils';
  import NavNodeRow from './NavNodeRow.svelte';
  import NavTree from './NavTree.svelte';

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
  />

  {#if (child.type === 'folder' || child.type === 'community') && child.expanded}
    <NavTree nodes={nodes} parentId={child.id} {activeNodeId} {onToggle} {onClick} {onDisplayModeChange} {onSortOrderChange} {profileLookup} {presenceOnline} />
  {/if}
{/each}
