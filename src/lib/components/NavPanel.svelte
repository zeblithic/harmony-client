<script lang="ts">
  import type { NavNode, DisplayMode, SortOrder } from '../types';
  import { getChildNodes, findNode } from '../nav-utils';
  import NavTree from './NavTree.svelte';

  let {
    nodes,
    collapsed = false,
    onNodeClick,
  }: {
    nodes: NavNode[];
    collapsed: boolean;
    onNodeClick?: (id: string) => void;
  } = $props();

  let navNodes = $state<NavNode[]>([]);
  let searchQuery = $state('');

  $effect(() => {
    navNodes = [...nodes];
  });

  /** Toggle a folder's expanded state. */
  function toggleFolder(id: string) {
    navNodes = navNodes.map((n) =>
      n.id === id ? { ...n, expanded: !n.expanded } : n
    );
  }

  /** Change a folder's display mode. */
  function changeDisplayMode(nodeId: string, mode: DisplayMode) {
    navNodes = navNodes.map((n) =>
      n.id === nodeId ? { ...n, displayMode: mode } : n
    );
  }

  /** Change a folder's sort order. */
  function changeSortOrder(nodeId: string, order: SortOrder) {
    navNodes = navNodes.map((n) =>
      n.id === nodeId ? { ...n, sortOrder: order } : n
    );
  }

  /**
   * Filter nodes by search query. Shows matching nodes and all their
   * ancestor folders (auto-expanded so matches are visible).
   */
  let filteredNodes = $derived.by(() => {
    const q = searchQuery.trim().toLowerCase();
    if (!q) return navNodes;

    // Find nodes whose names match
    const matchIds = new Set<string>();
    for (const node of navNodes) {
      if (node.name.toLowerCase().includes(q)) {
        matchIds.add(node.id);
        // Walk up to include all ancestors
        let current = node;
        while (current.parentId !== null) {
          matchIds.add(current.parentId);
          const parent = findNode(navNodes, current.parentId);
          if (!parent) break;
          current = parent;
        }
      }
    }

    // Return matching nodes, with ancestor folders expanded
    return navNodes
      .filter((n) => matchIds.has(n.id))
      .map((n) =>
        n.type === 'folder' && matchIds.has(n.id)
          ? { ...n, expanded: true }
          : n
      );
  });

  let topLevelNodes = $derived(getChildNodes(navNodes, null));
</script>

<div class="nav-panel">
  {#if !collapsed}
    <div class="nav-header">
      <input
        class="search-input"
        type="text"
        placeholder="Search"
        bind:value={searchQuery}
      />
    </div>
    <nav class="nav-tree-container">
      <NavTree
        nodes={filteredNodes}
        parentId={null}
        onToggle={toggleFolder}
        onClick={onNodeClick}
        onDisplayModeChange={changeDisplayMode}
        onSortOrderChange={changeSortOrder}
      />
    </nav>
  {:else}
    <nav class="collapsed-icons">
      {#each topLevelNodes as node (node.id)}
        <button
          class="icon-button"
          onclick={() => onNodeClick?.(node.id)}
        >
          {node.name.charAt(0).toUpperCase()}
        </button>
      {/each}
    </nav>
  {/if}
</div>

<style>
  .nav-panel {
    display: flex;
    flex-direction: column;
    height: 100%;
  }

  .nav-header {
    padding: 8px 12px;
    border-bottom: 1px solid var(--border);
  }

  .search-input {
    width: 100%;
    padding: 6px 10px;
    border: none;
    border-radius: 4px;
    background: var(--bg-primary);
    color: var(--text-primary);
    font-size: 13px;
    outline: none;
  }

  .search-input::placeholder {
    color: var(--text-muted);
  }

  .nav-tree-container {
    flex: 1;
    padding: 4px 0;
    overflow-y: auto;
  }

  .collapsed-icons {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 4px;
    padding: 8px 0;
  }

  .icon-button {
    width: 40px;
    height: 40px;
    border: none;
    border-radius: 8px;
    background: var(--bg-tertiary);
    color: var(--text-primary);
    font-size: 16px;
    font-weight: 700;
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
  }

  .icon-button:hover {
    background: var(--accent);
  }
</style>
