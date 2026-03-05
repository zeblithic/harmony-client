<script lang="ts">
  import type { NavNode, DisplayMode, SortOrder } from '../types';
  import { getChildNodes, sortNodes, getColorAncestry, getInheritedDisplayMode, getInheritedSortOrder } from '../nav-utils';
  import NavNodeRow from './NavNodeRow.svelte';
  import NavTree from './NavTree.svelte';

  let {
    nodes,
    parentId,
    onToggle,
    onClick,
    onDisplayModeChange,
    onSortOrderChange,
  }: {
    nodes: NavNode[];
    parentId: string | null;
    onToggle?: (id: string) => void;
    onClick?: (id: string) => void;
    onDisplayModeChange?: (nodeId: string, mode: DisplayMode) => void;
    onSortOrderChange?: (nodeId: string, order: SortOrder) => void;
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

  <NavNodeRow
    node={child}
    colorAncestry={ancestry}
    displayMode={dm}
    isLastChild={isLast}
    {onToggle}
    {onClick}
    {onDisplayModeChange}
    {onSortOrderChange}
  />

  {#if child.type === 'folder' && child.expanded}
    <NavTree nodes={nodes} parentId={child.id} {onToggle} {onClick} {onDisplayModeChange} {onSortOrderChange} />
  {/if}
{/each}
