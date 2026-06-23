<script lang="ts">
  import type { AppMode, NavNode, DisplayMode, SortOrder, ContentItem, ContentSection, StorageBuddy } from '../types';
  import { getChildNodes, findNode } from '../nav-utils';
  import { resolveNavModeFlags } from '../feature-flags';
  import NavTree from './NavTree.svelte';
  import FolderTree from './FolderTree.svelte';
  import QuickFilters from './QuickFilters.svelte';
  import StorageBuddySummary from './StorageBuddySummary.svelte';
  import { WebviewWindow } from '@tauri-apps/api/webviewWindow';

  let {
    nodes,
    collapsed = false,
    activeNodeId,
    onNodeClick,
    onSettingsClick,
    onModeChange,
    profileLookup,
    appMode = 'messages',
    contentItems,
    storageBuddies,
    fileSection,
    currentFolderCid,
    onFolderSelect,
    onFilterChange,
    filters,
    onManageBuddies,
    onNewDm,
    onNewGroupDm,
    onNewCommunity,
    onRedeemInvite,
    onBrowseLibraries,
    onSelectNotes,
    notesActive = false,
  }: {
    nodes: NavNode[];
    collapsed: boolean;
    activeNodeId?: string | null;
    onNodeClick?: (id: string) => void;
    onSettingsClick?: () => void;
    onModeChange?: (mode: AppMode) => void;
    profileLookup?: (address: string) => string | undefined;
    appMode?: AppMode;
    contentItems?: ContentItem[];
    storageBuddies?: StorageBuddy[];
    fileSection?: ContentSection;
    currentFolderCid?: string | null;
    onFolderSelect?: (cid: string | null) => void;
    onFilterChange?: (filters: Record<string, unknown>) => void;
    filters?: Record<string, unknown>;
    onManageBuddies?: () => void;
    /** ZEB-263: FAB fan-out menu callbacks. */
    onNewDm?: () => void;
    onNewGroupDm?: () => void;
    onNewCommunity?: () => void;
    onRedeemInvite?: () => void;
    /** ZEB-218 Sub-D Phase 1: opens the library directory browser. */
    onBrowseLibraries?: () => void;
    /** ZEB-334: select the private self-notes space. */
    onSelectNotes?: () => void;
    /** ZEB-334: whether the self-notes space is the active view. */
    notesActive?: boolean;
  } = $props();

  // ZEB-544: resolve the nav-rail gate map once per mount (a single localStorage
  // read) rather than calling the per-mode check inline for each button.
  const navFlags = resolveNavModeFlags();

  // ── ZEB-263 FAB + fan-out menu ──────────────────────────────────────
  // Click "+" → opens a 4-item popover (DM / Group DM / Community /
  // Redeem invite). Dismisses on Escape, click-outside, or item-click.
  let menuOpen = $state(false);
  let menuButtonEl = $state<HTMLButtonElement | null>(null);
  let menuPopoverEl = $state<HTMLDivElement | null>(null);

  function openMenu() {
    menuOpen = true;
  }

  function closeMenu() {
    if (!menuOpen) return;
    menuOpen = false;
    // Return focus to the FAB so keyboard users don't get stranded
    // on a now-detached menu item.
    menuButtonEl?.focus();
  }

  function handleMenuItem(cb: (() => void) | undefined) {
    closeMenu();
    cb?.();
  }

  function handleWindowKeyDown(e: KeyboardEvent) {
    if (e.key === 'Escape' && menuOpen) {
      closeMenu();
    }
  }

  function handleWindowMouseDown(e: MouseEvent) {
    if (!menuOpen) return;
    const target = e.target as Node | null;
    if (!target) return;
    // Don't dismiss when the click is on the FAB itself (its onclick will toggle).
    if (menuButtonEl && menuButtonEl.contains(target)) return;
    if (menuPopoverEl && menuPopoverEl.contains(target)) return;
    closeMenu();
  }

  // Local mirror of the nodes prop: user interactions (toggle/display/sort)
  // mutate navNodes directly, so we can't use $derived here. $effect.pre runs
  // synchronously before the DOM commit, so navNodes is populated on first
  // render and re-synced whenever the prop changes.
  let navNodes = $state<NavNode[]>([]);
  let searchQuery = $state('');

  $effect.pre(() => {
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

  /** Open the network visualization in a second Tauri window. */
  async function openNetworkWindow() {
    const existing = await WebviewWindow.getByLabel('network-viz');
    if (existing) {
      await existing.setFocus();
      return;
    }
    const url = 'src/network.html';
    new WebviewWindow('network-viz', {
      url,
      title: 'Harmony — Network',
      width: 1200,
      height: 800,
      minWidth: 800,
      minHeight: 600,
    });
  }

  /**
   * Filter nodes by search query. Shows matching nodes and all their
   * ancestor folders (auto-expanded so matches are visible).
   */
  let filteredNodes = $derived.by(() => {
    const q = searchQuery.trim().toLowerCase();
    if (!q) return navNodes;

    // Find nodes whose names match and their ancestor folders
    const matchIds = new Set<string>();
    const ancestorIds = new Set<string>();
    for (const node of navNodes) {
      if (node.name.toLowerCase().includes(q)) {
        matchIds.add(node.id);
        // Walk up to include all ancestors
        let current = node;
        while (current.parentId !== null) {
          matchIds.add(current.parentId);
          ancestorIds.add(current.parentId);
          const parent = findNode(navNodes, current.parentId);
          if (!parent) break;
          current = parent;
        }
      }
    }

    // Return matching nodes, with ancestor folders/communities expanded.
    // Communities render their children (channels) like folders since
    // ZEB-263, so they need the same auto-expand treatment to reveal
    // search hits nested under a manually-collapsed community.
    return navNodes
      .filter((n) => matchIds.has(n.id))
      .map((n) =>
        (n.type === 'folder' || n.type === 'community') && ancestorIds.has(n.id)
          ? { ...n, expanded: true }
          : n
      );
  });

  let topLevelNodes = $derived(getChildNodes(navNodes, null));
</script>

<svelte:window onkeydown={handleWindowKeyDown} onmousedown={handleWindowMouseDown} />

<div class="nav-panel">
  {#if !collapsed}
    <div class="nav-header">
      <input
        class="search-input"
        type="text"
        placeholder="Search"
        bind:value={searchQuery}
      />
      <span class="divider"></span>
      <button
        type="button"
        class="fab-btn"
        aria-label="Create new"
        aria-expanded={menuOpen}
        aria-haspopup="menu"
        bind:this={menuButtonEl}
        onclick={() => (menuOpen ? closeMenu() : openMenu())}
      >+</button>
      {#if menuOpen}
        <div class="fab-popover" role="menu" bind:this={menuPopoverEl}>
          <button type="button" role="menuitem" onclick={() => handleMenuItem(onNewDm)}>💬 New direct message</button>
          <button type="button" role="menuitem" onclick={() => handleMenuItem(onNewGroupDm)}>👥 New group DM</button>
          <hr />
          <button type="button" role="menuitem" onclick={() => handleMenuItem(onNewCommunity)}>🏛️ New community</button>
          <button type="button" role="menuitem" onclick={() => handleMenuItem(onRedeemInvite)}>🔗 Redeem invite link</button>
          <button type="button" role="menuitem" onclick={() => handleMenuItem(onBrowseLibraries)}>📚 Browse libraries</button>
        </div>
      {/if}
      <button class="settings-btn" onclick={() => onSettingsClick?.()} aria-label="Notification settings">⚙</button>
    </div>
    <nav class="nav-tree-container">
      {#if appMode === 'files'}
        {#if fileSection !== 'published'}
          <FolderTree items={contentItems ?? []} {onFolderSelect} selectedCid={currentFolderCid ?? null} />
          <QuickFilters {onFilterChange} {filters} />
        {/if}
      {:else if appMode === 'spellbook'}
        <!-- Spellbook mode uses its own tab content -->
      {:else}
        {#if appMode === 'messages' && onSelectNotes}
          <button
            type="button"
            class="notes-nav-row"
            class:active={notesActive}
            aria-pressed={notesActive}
            onclick={() => onSelectNotes?.()}
          >
            <span class="notes-nav-icon" aria-hidden="true">📝</span>
            <span>Notes</span>
          </button>
        {/if}
        <NavTree
          nodes={filteredNodes}
          parentId={null}
          {activeNodeId}
          onToggle={toggleFolder}
          onClick={onNodeClick}
          onDisplayModeChange={changeDisplayMode}
          onSortOrderChange={changeSortOrder}
          {profileLookup}
        />
      {/if}
    </nav>
    {#if appMode === 'files'}
      <StorageBuddySummary buddies={storageBuddies ?? []} {onManageBuddies} />
    {/if}
    <div class="nav-footer">
      <!-- ZEB-544: the rail surface is gated by `isNavModeEnabled` so the alpha
           shows a focused Communities-first set. `messages` is the home and is
           never gated; the rest are hidden by default unless a device-local
           override re-enables them. Hiding the rail button does not remove a
           feature's backend or its contextual entry points. -->
      <div class="mode-toggles" role="group" aria-label="App mode">
        <button type="button" class="nav-action-btn mode-toggle" class:active={appMode === 'messages'}
          aria-label="Messages" aria-pressed={appMode === 'messages'}
          onclick={() => onModeChange?.('messages')}>Messages</button>
        {#if navFlags.vines}
          <button type="button" class="nav-action-btn mode-toggle" class:active={appMode === 'vines'}
            aria-label="Vines" aria-pressed={appMode === 'vines'}
            onclick={() => onModeChange?.('vines')}>Vines</button>
        {/if}
        {#if navFlags.files}
          <button type="button" class="nav-action-btn mode-toggle" class:active={appMode === 'files'}
            aria-label="Files" aria-pressed={appMode === 'files'}
            onclick={() => onModeChange?.('files')}>Files</button>
        {/if}
        {#if navFlags.mail}
          <button type="button" class="nav-action-btn mode-toggle" class:active={appMode === 'mail'}
            aria-label="Mail" aria-pressed={appMode === 'mail'}
            onclick={() => onModeChange?.('mail')}>Mail</button>
        {/if}
        {#if navFlags.spellbook}
          <button type="button" class="nav-action-btn mode-toggle" class:active={appMode === 'spellbook'}
            aria-label="Spellbook" aria-pressed={appMode === 'spellbook'}
            onclick={() => onModeChange?.('spellbook')}>Spellbook</button>
        {/if}
        {#if navFlags.mint}
          <button type="button" class="nav-action-btn mode-toggle" class:active={appMode === 'mint'}
            aria-label="Mint" aria-pressed={appMode === 'mint'}
            onclick={() => onModeChange?.('mint')}>💰 Mint</button>
        {/if}
        {#if navFlags.network}
          <button type="button" class="nav-action-btn mode-toggle" class:active={appMode === 'network'}
            aria-label="Network" aria-pressed={appMode === 'network'}
            onclick={() => onModeChange?.('network')}>Network</button>
        {/if}
      </div>
      <button
        type="button"
        class="nav-action-btn"
        aria-label="Open network visualization"
        onclick={openNetworkWindow}
      >
        Network Viz
      </button>
    </div>
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
    display: flex;
    gap: 8px;
    align-items: center;
  }

  .settings-btn {
    border: none;
    background: none;
    color: var(--text-muted);
    font-size: 16px;
    cursor: pointer;
    padding: 4px;
    flex-shrink: 0;
  }

  .settings-btn:hover {
    color: var(--text-primary);
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

  /* ZEB-334: pinned, always-present self-notes row at the top of the
     messages nav. Not a NavNode — a fixed affordance for the private
     scratchpad that doubles as the zero-community default view. */
  .notes-nav-row {
    display: flex;
    align-items: center;
    gap: 8px;
    width: calc(100% - 16px);
    margin: 0 8px 4px;
    padding: 6px 8px;
    border: none;
    border-radius: 4px;
    background: transparent;
    color: var(--text-primary);
    font-size: 0.875rem;
    text-align: left;
    cursor: pointer;
  }
  .notes-nav-row:hover { background: var(--bg-tertiary); }
  .notes-nav-row.active { background: var(--accent); color: var(--text-primary); }
  .notes-nav-icon { font-size: 0.95rem; }

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

  .nav-footer {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 8px 12px;
    border-top: 1px solid var(--border);
  }

  .nav-action-btn {
    width: 100%;
    padding: 6px 10px;
    border: none;
    border-radius: 4px;
    background: var(--bg-tertiary);
    color: var(--text-muted);
    font-size: 13px;
    cursor: pointer;
    text-align: left;
  }

  .nav-action-btn:hover {
    background: var(--accent);
    color: var(--text-primary);
  }

  /* ZEB-333: wrap the mode toggles into a responsive grid so every mode
     (including the trailing Mint + Network buttons) stays visible at the
     narrow default nav width on Windows. The previous non-wrapping flex
     row (flex:1 + min-width:auto) overflowed and clipped the last buttons,
     forcing a horizontal scroll to reach Mint / Peer Network. */
  .mode-toggles {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(72px, 1fr));
    gap: 4px;
  }
  .mode-toggles .mode-toggle {
    font-size: 0.75rem;
    padding: 4px 6px;
    text-align: center;
  }

  .mode-toggle.active {
    background: var(--accent);
    color: var(--text-primary);
  }

  /* ── ZEB-263: FAB + fan-out menu ──────────────────────────────────── */
  .nav-header {
    position: relative;
  }
  .divider {
    width: 1px;
    height: 18px;
    background: var(--border);
    margin: 0 4px;
    flex-shrink: 0;
  }
  .fab-btn {
    font-size: 0.9rem;
    padding: 4px 10px;
    background: var(--accent);
    color: var(--text-primary);
    border: none;
    border-radius: 4px;
    cursor: pointer;
    flex-shrink: 0;
  }
  .fab-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }
  .fab-popover {
    position: absolute;
    right: 12px;
    top: 42px;
    background: var(--bg-secondary);
    border: 1px solid var(--accent);
    border-radius: 6px;
    padding: 4px;
    min-width: 200px;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.5);
    z-index: 50;
    display: flex;
    flex-direction: column;
  }
  .fab-popover button {
    padding: 8px 12px;
    background: transparent;
    border: none;
    color: var(--text-primary);
    text-align: left;
    cursor: pointer;
    border-radius: 3px;
    font-size: 0.875rem;
  }
  .fab-popover button:hover {
    background: var(--bg-tertiary);
  }
  .fab-popover hr {
    border: none;
    border-top: 1px solid var(--border);
    margin: 4px 0;
  }
</style>
