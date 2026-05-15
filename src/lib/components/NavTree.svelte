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
  }: {
    nodes: NavNode[];
    parentId: string | null;
    activeNodeId?: string | null;
    onToggle?: (id: string) => void;
    onClick?: (id: string) => void;
    onDisplayModeChange?: (nodeId: string, mode: DisplayMode) => void;
    onSortOrderChange?: (nodeId: string, order: SortOrder) => void;
    profileLookup?: (address: string) => string | undefined;
  } = $props();

  let sortedChildren = $derived.by(() => {
    const children = getChildNodes(nodes, parentId);
    const order = parentId ? getInheritedSortOrder(nodes, parentId) : 'activity';
    return sortNodes(children, order);
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
  />

  {#if (child.type === 'folder' || child.type === 'community') && child.expanded}
    <NavTree nodes={nodes} parentId={child.id} {activeNodeId} {onToggle} {onClick} {onDisplayModeChange} {onSortOrderChange} {profileLookup} />
  {/if}
{/each}
