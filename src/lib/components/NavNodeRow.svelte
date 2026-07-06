<script lang="ts">
  import type { NavNode, DisplayMode, SortOrder } from '../types';
  import { navPaletteColor } from '../nav-utils';
  import Avatar from './Avatar.svelte';

  const DISPLAY_MODE_CYCLE: DisplayMode[] = ['text', 'icon', 'both'];
  const DISPLAY_MODE_ICON: Record<DisplayMode, string> = {
    text: '\u2630',
    icon: '\u229E',
    both: '\u2630\u229E',
  };

  const SORT_OPTIONS: { value: SortOrder; label: string }[] = [
    { value: 'activity', label: '\uD83D\uDD50 Activity' },
    { value: 'pinned', label: '\uD83D\uDCCC Pinned' },
    { value: 'alphabetical', label: '\uD83D\uDD24 A-Z' },
  ];

  let {
    node,
    colorAncestry,
    displayMode,
    isLastChild,
    active = false,
    onToggle,
    onClick,
    onDisplayModeChange,
    onSortOrderChange,
    statusText,
    forkParentName,
    presenceOnline,
  }: {
    node: NavNode;
    colorAncestry: number[];
    displayMode: DisplayMode;
    isLastChild: boolean;
    active?: boolean;
    onToggle?: (id: string) => void;
    onClick?: (id: string) => void;
    onDisplayModeChange?: (nodeId: string, mode: DisplayMode) => void;
    onSortOrderChange?: (nodeId: string, order: SortOrder) => void;
    statusText?: string;
    /** ZEB-285: resolved display name of the parent community for the fork
     *  glyph tooltip. Passed by NavTree when the parent is in the user's nav;
     *  null / undefined when the parent is absent (user left the original). */
    forkParentName?: string | null;
    /** ZEB-600: resolver — should this node show an "online" presence dot?
     *  Community rows: someone besides you is online there; DM rows: the
     *  counterparty is online in some shared community. App-provided so the
     *  nav stays agnostic of the presence service. */
    presenceOnline?: (node: NavNode) => boolean;
  } = $props();

  let showSortMenu = $state(false);

  let paddingLeft = $derived(colorAncestry.length * 4 + 8);
  // ZEB-600: whether to show the "online" presence dot for this node.
  let showPresenceDot = $derived(presenceOnline?.(node) ?? false);

  function handleClick(e: MouseEvent | KeyboardEvent) {
    e.stopPropagation();
    if (node.type === 'folder') {
      onToggle?.(node.id);
    } else {
      // Community click selects (opens overview); the chevron button
      // (rendered separately for community type) handles toggle so
      // every selection doesn't also flip expanded state.
      onClick?.(node.id);
    }
  }

  function toggleCommunity(e: MouseEvent | KeyboardEvent) {
    e.stopPropagation();
    onToggle?.(node.id);
  }

  function typeIcon(n: NavNode): string {
    if (n.type === 'channel') return '#';
    if (n.type === 'dm' || n.type === 'group-chat') return '@';
    if (n.type === 'folder') return n.expanded ? '\u25BE' : '\u25B8';
    if (n.type === 'community') return '🏛️';
    return '';
  }

  function cycleDisplayMode(e: MouseEvent) {
    e.stopPropagation();
    const idx = DISPLAY_MODE_CYCLE.indexOf(displayMode);
    const next = DISPLAY_MODE_CYCLE[(idx + 1) % DISPLAY_MODE_CYCLE.length];
    onDisplayModeChange?.(node.id, next);
  }

  function toggleSortMenu(e: MouseEvent) {
    e.stopPropagation();
    showSortMenu = !showSortMenu;
  }

  function selectSortOrder(e: MouseEvent, order: SortOrder) {
    e.stopPropagation();
    onSortOrderChange?.(node.id, order);
    showSortMenu = false;
  }
</script>

<div
  class="nav-row"
  class:active
  class:pending={node.pending}
  role="button"
  tabindex="0"
  data-testid="nav-row-{node.id}"
  onclick={handleClick}
  onkeydown={(e) => { if (e.key === 'Enter' || e.key === ' ') handleClick(e); }}
>
  <!-- Color bands -->
  {#each colorAncestry as colorIdx, i}
    <span
      class="color-band"
      style="left: {i * 4}px; background: {navPaletteColor(colorIdx)}"
    ></span>
  {/each}

  <!-- Row content -->
  <span class="row-content" style="padding-left: {paddingLeft}px">
    {#if displayMode === 'icon'}
      {#if (node.type === 'dm' || node.type === 'group-chat') && node.peer}
        <Avatar address={node.peer.address} displayName={node.peer.displayName} avatarUrl={node.peer.avatarUrl} size={32} />
      {:else}
        <span class="icon-cell">
          {node.name.charAt(0).toUpperCase()}
        </span>
      {/if}
    {:else}
      <!-- Text or both mode -->
      <!--
        Communities render BOTH the chevron button (▾/▸) and the type
        icon (🏛️). They encode different things: chevron is the
        expand/collapse affordance, 🏛️ is the type identifier
        (community vs folder vs channel). VSCode and macOS Finder
        use the same `[chevron] [type-icon] [name]` pattern. Folders
        elide the type icon because the chevron itself signals
        folder-ness — but if we did that here, communities would
        be visually indistinguishable from folders in the nav tree.
        Cursor flagged this as "redundant" on commit 502056e — it's
        not, but the comment is here so future review passes don't
        re-flag the design.
      -->
      {#if node.type === 'community'}
        <button
          class="community-chevron"
          aria-label={node.expanded ? 'Collapse community' : 'Expand community'}
          aria-expanded={node.expanded}
          onclick={toggleCommunity}
        >{node.expanded ? '▾' : '▸'}</button>
      {/if}
      <span class="type-icon">{typeIcon(node)}</span>
      {#if (node.type === 'dm' || node.type === 'group-chat') && node.peer}
        <Avatar address={node.peer.address} displayName={node.peer.displayName} avatarUrl={node.peer.avatarUrl} size={20} />
      {/if}
      {#if showPresenceDot}
        <span class="nav-presence-dot" role="img" aria-label="Online" title="Online"></span>
      {/if}
      <span
        class="node-name"
        title={node.forkedFrom != null
          ? `Forked from ${forkParentName ?? 'another community'}`
          : undefined}
      >
        <span class="name-text">
          {#if node.forkedFrom != null}<span class="fork-glyph" aria-hidden="true">↳ </span>{/if}{node.name}{#if node.pending}<span class="pending-badge" title="Waiting for admin to approve your join request" aria-label="pending approval">⏳</span>{/if}
        </span>
        {#if statusText}
          <span class="status-text">{statusText}</span>
        {/if}
      </span>
    {/if}

    <!-- Unread indicators -->
    {#if node.unreadLevel === 'standard' && node.unreadCount > 0}
      <span class="unread-badge">{node.unreadCount}</span>
    {:else if node.unreadLevel === 'loud' && node.unreadCount > 0}
      <span class="unread-badge loud">{node.unreadCount}</span>
    {:else if node.unreadLevel === 'quiet' && node.unreadCount > 0}
      <span class="unread-dot"></span>
    {/if}
  </span>

  <!-- Folder controls -->
  {#if node.type === 'folder'}
    <button class="sort-trigger" onclick={toggleSortMenu}>{'\u2195'}</button>
    <button class="mode-toggle" onclick={cycleDisplayMode}>{DISPLAY_MODE_ICON[displayMode]}</button>
    {#if showSortMenu}
      <div class="sort-menu">
        {#each SORT_OPTIONS as opt}
          <button
            class="sort-option {(node.sortOrder ?? 'activity') === opt.value ? 'active' : ''}"
            onclick={(e: MouseEvent) => selectSortOrder(e, opt.value)}
          >{opt.label}</button>
        {/each}
      </div>
    {/if}
  {/if}

  <!-- Bracket markers -->
  {#if node.type === 'folder' && node.expanded}
    <span class="bracket bracket-open">{'\u250C'}</span>
  {/if}
  {#if isLastChild && colorAncestry.length > 0}
    <span class="bracket bracket-close">{'\u2518'}</span>
  {/if}
</div>

<style>
  /* ZEB-600: nav presence dot — solid var(--presence-online) when the resolver reports online. */
  .nav-presence-dot {
    flex-shrink: 0;
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--presence-online);
  }
  .nav-row {
    position: relative;
    display: flex;
    align-items: center;
    width: 100%;
    min-height: 32px;
    border: none;
    background: none;
    color: var(--text-muted);
    font-size: 14px;
    cursor: pointer;
    text-align: left;
  }

  .nav-row:hover {
    background: var(--bg-tertiary);
    color: var(--text-primary);
  }

  .nav-row.active {
    background: var(--bg-tertiary);
    color: var(--text-primary);
  }

  /* ZEB-254: pending community — greyed and italic until countersign arrives. */
  .nav-row.pending {
    opacity: 0.55;
    font-style: italic;
  }

  .pending-badge {
    font-size: 0.8em;
    margin-left: 0.35em;
    font-style: normal;
  }

  .nav-row:hover .unread-dot {
    opacity: 1;
  }

  .color-band {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 4px;
  }

  .row-content {
    display: flex;
    align-items: center;
    gap: 6px;
    flex: 1;
    min-width: 0;
  }

  .type-icon {
    flex-shrink: 0;
    width: 16px;
    text-align: center;
    color: var(--text-muted);
  }

  /* Chevron button for community expand/collapse. Stops propagation so
     clicking it doesn't also fire the row's onClick (which selects the
     community). Folder click toggles via the row body, but folders
     don't have a separate select action so there's no ambiguity. */
  .community-chevron {
    flex-shrink: 0;
    width: 16px;
    text-align: center;
    color: var(--text-muted);
    background: none;
    border: none;
    padding: 0;
    margin: 0;
    cursor: pointer;
    font-size: inherit;
    line-height: 1;
  }
  .community-chevron:hover {
    color: var(--text-primary);
  }
  .community-chevron:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
    border-radius: 2px;
  }

  .node-name {
    flex: 1;
    overflow: hidden;
    min-width: 0;
  }

  .name-text {
    display: block;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .fork-glyph {
    color: var(--text-muted);
    font-size: 0.85em;
  }

  .icon-cell {
    width: 32px;
    height: 32px;
    border-radius: 6px;
    background: var(--bg-tertiary);
    display: flex;
    align-items: center;
    justify-content: center;
    font-weight: 700;
    font-size: 14px;
    color: var(--text-primary);
    flex-shrink: 0;
  }

  .status-text {
    display: block;
    font-size: 11px;
    font-weight: 400;
    color: var(--text-muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .unread-badge {
    background: var(--accent);
    color: var(--text-bright);
    font-size: 11px;
    font-weight: 700;
    padding: 1px 6px;
    border-radius: 8px;
    flex-shrink: 0;
  }

  .unread-badge.loud {
    animation: pulse 1.5s ease-in-out infinite;
  }

  @keyframes pulse {
    0%, 100% { transform: scale(1); }
    50% { transform: scale(1.15); }
  }

  .unread-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--text-muted);
    opacity: 0;
    flex-shrink: 0;
    transition: opacity 0.15s;
  }

  .bracket {
    position: absolute;
    right: 8px;
    color: var(--text-muted);
    font-size: 12px;
    line-height: 1;
  }

  .bracket-open {
    top: 2px;
  }

  .bracket-close {
    bottom: 2px;
  }

  .mode-toggle {
    position: absolute;
    right: 24px;
    border: none;
    background: none;
    color: var(--text-muted);
    font-size: 12px;
    cursor: pointer;
    opacity: 0;
    transition: opacity 0.15s ease;
    padding: 2px 4px;
  }

  .nav-row:hover .mode-toggle {
    opacity: 1;
  }

  .mode-toggle:hover {
    color: var(--text-primary);
  }

  .sort-trigger {
    position: absolute;
    right: 36px;
    border: none;
    background: none;
    color: var(--text-muted);
    font-size: 11px;
    cursor: pointer;
    opacity: 0;
    transition: opacity 0.15s ease;
    padding: 2px 4px;
  }

  .nav-row:hover .sort-trigger {
    opacity: 1;
  }

  .sort-menu {
    position: absolute;
    right: 8px;
    top: 28px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 4px;
    z-index: 10;
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 120px;
  }

  .sort-option {
    border: none;
    background: none;
    color: var(--text-secondary);
    padding: 6px 8px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 12px;
    text-align: left;
  }

  .sort-option:hover {
    background: var(--bg-secondary);
    color: var(--text-primary);
  }

  .sort-option.active {
    color: var(--accent);
  }
</style>
