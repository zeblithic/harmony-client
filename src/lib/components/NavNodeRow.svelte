<script lang="ts">
  import type { NavNode, DisplayMode } from '../types';
  import { NAV_PALETTE } from '../nav-utils';

  let {
    node,
    colorAncestry,
    displayMode,
    isLastChild,
    onToggle,
    onClick,
  }: {
    node: NavNode;
    colorAncestry: number[];
    displayMode: DisplayMode;
    isLastChild: boolean;
    onToggle?: (id: string) => void;
    onClick?: (id: string) => void;
  } = $props();

  let paddingLeft = $derived(colorAncestry.length * 4 + 8);

  function handleClick(e: MouseEvent) {
    e.stopPropagation();
    if (node.type === 'folder') {
      onToggle?.(node.id);
    } else {
      onClick?.(node.id);
    }
  }

  function typeIcon(n: NavNode): string {
    if (n.type === 'channel') return '#';
    if (n.type === 'dm' || n.type === 'group-chat') return '@';
    if (n.type === 'folder') return n.expanded ? '\u25BE' : '\u25B8';
    return '';
  }
</script>

<button
  class="nav-row"
  data-testid="nav-row-{node.id}"
  onclick={handleClick}
>
  <!-- Color bands -->
  {#each colorAncestry as colorIdx, i}
    <span
      class="color-band"
      style="left: {i * 4}px; background: {NAV_PALETTE[colorIdx]}"
    ></span>
  {/each}

  <!-- Row content -->
  <span class="row-content" style="padding-left: {paddingLeft}px">
    {#if displayMode === 'icon'}
      <!-- Icon grid cell -->
      <span class="icon-cell">
        {node.name.charAt(0).toUpperCase()}
      </span>
      {#if (node.type === 'dm' || node.type === 'group-chat') && node.peer?.avatarUrl}
        <img class="dm-avatar" src={node.peer.avatarUrl} alt={node.name} />
      {/if}
    {:else}
      <!-- Text or both mode -->
      <span class="type-icon">{typeIcon(node)}</span>
      <span class="node-name">{node.name}</span>
      {#if displayMode === 'both' && (node.type === 'dm' || node.type === 'group-chat') && node.peer?.avatarUrl}
        <img class="dm-avatar" src={node.peer.avatarUrl} alt={node.name} />
      {/if}
    {/if}

    <!-- Unread indicators -->
    {#if node.unreadLevel === 'standard' && node.unreadCount > 0}
      <span class="unread-badge">{node.unreadCount}</span>
    {:else if node.unreadLevel === 'loud' && node.unreadCount > 0}
      <span class="unread-badge loud">{node.unreadCount}</span>
    {:else if node.unreadLevel === 'quiet'}
      <span class="unread-dot"></span>
    {/if}
  </span>

  <!-- Bracket markers -->
  {#if node.type === 'folder' && node.expanded}
    <span class="bracket bracket-open">{'\u250C'}</span>
  {/if}
  {#if isLastChild && colorAncestry.length > 0}
    <span class="bracket bracket-close">{'\u2518'}</span>
  {/if}
</button>

<style>
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

  .node-name {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
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

  .dm-avatar {
    width: 20px;
    height: 20px;
    border-radius: 50%;
    flex-shrink: 0;
  }

  .unread-badge {
    background: var(--accent);
    color: white;
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
</style>
