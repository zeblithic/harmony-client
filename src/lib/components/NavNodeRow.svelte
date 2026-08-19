<script lang="ts">
  import type { NavNode, DisplayMode, SortOrder } from '../types';
  import { navPaletteColor } from '../nav-utils';
  import { appliedTheme } from '../theme-service';
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
    canManageChannel,
    onRenameChannel,
    onDeleteChannel,
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
    /** ZEB-663: may the viewer manage THIS channel node (rename/delete)?
     *  Resolves true only for the selected community's channels at power ≥ kick. */
    canManageChannel?: (node: NavNode) => boolean;
    /** ZEB-663: open rename dialog for a channel node. */
    onRenameChannel?: (communityId: string, channelId: string) => void;
    /** ZEB-663: open delete-confirm for a channel node. */
    onDeleteChannel?: (communityId: string, channelId: string) => void;
  } = $props();

  let showSortMenu = $state(false);

  // ZEB-663: channel-row moderation context menu (rename/delete).
  let canManage = $derived(node.type === 'channel' && (canManageChannel?.(node) ?? false));
  let channelMenu = $state<{ x: number; y: number } | null>(null);
  let channelMenuEl: HTMLElement | undefined = $state();
  // ZEB-664: visible, tabbable menu trigger (⋯) — the keyboard entry point.
  let channelTriggerEl: HTMLButtonElement | undefined = $state();
  let rowEl: HTMLElement | undefined = $state();
  // Event-maintained: by the time the demotion $effect runs, the trigger is
  // already unmounted (unmount fires no blur), so containment checks against
  // document.activeElement can't see it — this flag can.
  let triggerFocused = false;

  // §6.8: close a stale moderation menu when the viewer is demoted. If focus
  // was inside the menu (auto-focused on open) or on the trigger, both of
  // which unmount here, park it on the row instead of letting it fall to body.
  $effect(() => {
    if (!canManage) {
      const active = document.activeElement;
      const inMenu = !!(channelMenu && channelMenuEl && active && channelMenuEl.contains(active));
      if (inMenu || triggerFocused) {
        rowEl?.focus();
        triggerFocused = false;
      }
      channelMenu = null;
    }
  });

  $effect(() => {
    if (!channelMenu) return;
    function onDocClick(e: MouseEvent) {
      const target = e.target as Node | null;
      if (channelMenuEl && target && channelMenuEl.contains(target)) return;
      // The trigger's own click must reach its toggle handler (this listener
      // is capture-phase, so a plain close here would race a bubble reopen).
      if (channelTriggerEl && target && channelTriggerEl.contains(target)) return;
      channelMenu = null;
    }
    document.addEventListener('click', onDocClick, true);
    return () => document.removeEventListener('click', onDocClick, true);
  });

  // ZEB-664: menu-pattern focus management — focus lands on the first item
  // when the menu opens (right-click and trigger paths behave alike).
  $effect(() => {
    if (channelMenu && channelMenuEl) {
      channelMenuEl.querySelector('button')?.focus();
    }
  });

  function onChannelContextMenu(e: MouseEvent) {
    if (!canManage) return;
    e.preventDefault();
    e.stopPropagation();
    channelMenu = { x: e.clientX, y: e.clientY };
  }

  function toggleChannelMenuFromTrigger(e: MouseEvent) {
    e.stopPropagation();
    if (channelMenu) {
      channelMenu = null;
      return;
    }
    const rect = channelTriggerEl?.getBoundingClientRect();
    channelMenu = rect ? { x: rect.left, y: rect.bottom + 2 } : { x: 0, y: 0 };
  }

  function onMenuKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      e.stopPropagation();
      channelMenu = null;
      channelTriggerEl?.focus();
      return;
    }
    if (e.key === 'Tab') {
      // Menu-button pattern: Tab moves on, so the popup must not linger.
      // No preventDefault — focus proceeds to the next tabbable naturally.
      channelMenu = null;
      return;
    }
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      const items = Array.from(channelMenuEl?.querySelectorAll('button') ?? []);
      if (items.length === 0) return;
      const idx = items.indexOf(document.activeElement as HTMLButtonElement);
      const delta = e.key === 'ArrowDown' ? 1 : -1;
      items[(idx + delta + items.length) % items.length]?.focus();
    }
  }

  function channelRename() {
    channelMenu = null;
    if (node.parentId) onRenameChannel?.(node.parentId, node.id);
  }
  function channelDelete() {
    channelMenu = null;
    if (node.parentId) onDeleteChannel?.(node.parentId, node.id);
  }

  let paddingLeft = $derived(colorAncestry.length * 4 + 8);
  // Color-band hex per ancestry index, theme-reactive (ZEB-645): void
  // $appliedTheme re-runs the map on a flip (navPaletteColor -> tokenColor).
  let bandColors = $derived.by(() => {
    void $appliedTheme;
    return colorAncestry.map(navPaletteColor);
  });
  // ZEB-600: whether to show the "online" presence dot for this node.
  let showPresenceDot = $derived(presenceOnline?.(node) ?? false);

  function handleClick(e: MouseEvent | KeyboardEvent) {
    e.stopPropagation();
    if (node.type === 'folder') {
      onToggle?.(node.id);
    } else {
      // Community/channel/DM click selects. (ZEB-965: the community
      // expand/collapse chevron is retired — communities are flat rows.)
      onClick?.(node.id);
    }
  }

  function typeIcon(n: NavNode): string {
    // ZEB-663: voice channels get the speaker glyph (matches ChannelSubSidebar);
    // ZEB-612: town halls get the scales glyph (matches CreateChannelDialog).
    if (n.type === 'channel') {
      if (n.channelKind === 'voice') return '\uD83D\uDD0A';
      if (n.channelKind === 'townhall') return '\u2696';
      return '#';
    }
    if (n.type === 'dm' || n.type === 'group-chat') return '@';
    if (n.type === 'folder') return n.expanded ? '\u25BE' : '\u25B8';
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
  bind:this={rowEl}
  class="nav-row"
  class:active
  class:pending={node.pending}
  role="button"
  tabindex="0"
  data-testid="nav-row-{node.id}"
  onclick={handleClick}
  onkeydown={(e) => { if (e.key === 'Enter' || e.key === ' ') handleClick(e); }}
  oncontextmenu={onChannelContextMenu}
>
  <!-- Color bands -->
  {#each bandColors as bandColor, i}
    <span
      class="color-band"
      style="left: {i * 4}px; background: {bandColor}"
    ></span>
  {/each}

  <!-- Row content -->
  <!-- managed: reserve a right gutter so the ⋯ trigger never overlays the
       mention/unread badges (constant, so nothing shifts on hover). -->
  <span class="row-content" class:managed={canManage} style="padding-left: {paddingLeft}px">
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
      <!-- ZEB-965: communities are flat rows (channels live in the right-hand
           ChannelsPanel), so the ZEB-606 expand/collapse chevron is retired —
           the letter chip is the community's type identifier. -->
      {#if node.type === 'community'}
        <span class="community-chip" aria-hidden="true">{node.name.charAt(0).toUpperCase()}</span>
      {:else}
        <span class="type-icon">{typeIcon(node)}</span>
      {/if}
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

    <!-- ZEB-662: mention badge — clay-toned, higher-signal than unread. -->
    {#if node.mentionCount > 0}
      <span
        class="mention-badge"
        data-testid="mention-badge"
        aria-label={`${node.mentionCount} unread mention${node.mentionCount === 1 ? '' : 's'}`}
      >@{node.mentionCount}</span>
    {/if}

    <!-- ZEB-357: missed-call badge on DM rows — the missed-class subset of
         unread, so the phone glyph disambiguates "you missed a call" from
         ordinary unread messages at a glance. -->
    {#if (node.missedCallCount ?? 0) > 0}
      <span
        class="missed-call-badge"
        data-testid="missed-call-badge"
        aria-label={`${node.missedCallCount} missed call${node.missedCallCount === 1 ? '' : 's'}`}
      >📞{node.missedCallCount}</span>
    {/if}

    <!-- Unread indicators (ZEB-665: display caps at 99+; the tracker's
         internal set is capped at 100, so 100 means "at least 100"). -->
    {#if node.unreadLevel === 'standard' && node.unreadCount > 0}
      <span class="unread-badge">{node.unreadCount > 99 ? '99+' : node.unreadCount}</span>
    {:else if node.unreadLevel === 'loud' && node.unreadCount > 0}
      <span class="unread-badge loud">{node.unreadCount > 99 ? '99+' : node.unreadCount}</span>
    {:else if node.unreadLevel === 'quiet' && node.unreadCount > 0}
      <span class="unread-dot"></span>
    {/if}
  </span>

  <!-- ZEB-664: keyboard-accessible channel-moderation trigger. Revealed on
       row hover AND keyboard focus (focus-within); opens the same menu as
       right-click. -->
  {#if canManage}
    <button
      bind:this={channelTriggerEl}
      class="channel-menu-trigger"
      data-testid="channel-menu-trigger-{node.id}"
      aria-haspopup="menu"
      aria-expanded={channelMenu !== null}
      aria-label="Channel options for {node.name}"
      title="Channel options"
      onclick={toggleChannelMenuFromTrigger}
      onfocus={() => (triggerFocused = true)}
      onblur={() => (triggerFocused = false)}
      onkeydown={(e) => {
        // Enter/Space activate the button natively (click); without this
        // stop they'd also bubble to the row's keydown and select the channel.
        if (e.key === 'Enter' || e.key === ' ') e.stopPropagation();
      }}
    >⋯</button>
  {/if}

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

{#if channelMenu}
  <div
    bind:this={channelMenuEl}
    class="channel-context-menu"
    role="menu"
    tabindex="-1"
    style="left: {channelMenu.x}px; top: {channelMenu.y}px"
    onkeydown={onMenuKeydown}
  >
    <button type="button" role="menuitem" onclick={channelRename}>Rename</button>
    <button type="button" role="menuitem" class="destructive" onclick={channelDelete}>Delete</button>
  </div>
{/if}

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
    background: var(--primary-soft);
    color: var(--primary-deep);
  }

  /* ZEB-606: Commons community letter chip — replaces the 🏛️ type icon. */
  .community-chip {
    flex-shrink: 0;
    width: 20px;
    height: 20px;
    border-radius: 6px;
    background: var(--accent);
    color: var(--on-accent);
    font-size: 11px;
    font-weight: 700;
    display: flex;
    align-items: center;
    justify-content: center;
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

  .row-content.managed {
    padding-right: 22px;
  }

  .type-icon {
    flex-shrink: 0;
    width: 16px;
    text-align: center;
    color: var(--text-muted);
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
    color: var(--on-accent);
    font-size: 11px;
    font-weight: 700;
    padding: 1px 6px;
    border-radius: 8px;
    flex-shrink: 0;
  }

  /* ZEB-662: nav mention badge — clay-toned so it's distinct from the accent
     unread badge. gov-clay-deep is dark-in-light / light-in-dark, so it pairs
     with --on-accent for AA-safe contrast in both themes. */
  .mention-badge {
    background: var(--gov-clay-deep);
    color: var(--on-accent);
    font-size: 11px;
    font-weight: 700;
    padding: 1px 6px;
    border-radius: 8px;
    flex-shrink: 0;
  }

  /* ZEB-357: missed-call badge — mention-badge geometry with the accent
     palette (the phone glyph itself carries the distinction). */
  .missed-call-badge {
    background: var(--accent);
    color: var(--on-accent);
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

  /* ZEB-664: channel-moderation trigger — mirrors .mode-toggle's hover reveal,
     but must ALSO reveal on keyboard focus (focus-within covers both the row
     and the trigger itself) or keyboard users can't find it. */
  .channel-menu-trigger {
    position: absolute;
    right: 8px;
    border: none;
    background: none;
    color: var(--text-muted);
    font-size: 14px;
    line-height: 1;
    cursor: pointer;
    opacity: 0;
    transition: opacity 0.15s ease;
    padding: 2px 4px;
  }

  .nav-row:hover .channel-menu-trigger,
  .nav-row:focus-within .channel-menu-trigger,
  .channel-menu-trigger[aria-expanded='true'] {
    opacity: 1;
  }

  .channel-menu-trigger:hover {
    color: var(--text-primary);
  }

  .channel-menu-trigger:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
    border-radius: 2px;
    opacity: 1;
  }

  /* ZEB-663: channel-row moderation context menu (mirrors ChannelSubSidebar). */
  .channel-context-menu {
    position: fixed;
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 4px;
    box-shadow: 0 2px 8px var(--shadow-mid);
    z-index: 1000;
    min-width: 140px;
    padding: 4px 0;
  }
  .channel-context-menu button {
    display: block;
    width: 100%;
    background: none;
    border: none;
    text-align: left;
    padding: 6px 12px;
    color: var(--text-primary);
    cursor: pointer;
    font-size: 0.85rem;
  }
  .channel-context-menu button:hover { background: var(--bg-tertiary); }
  .channel-context-menu button.destructive { color: var(--danger-muted); }
</style>
