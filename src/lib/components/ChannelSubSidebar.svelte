<script lang="ts">
  import type { ChannelInfo } from '../community-service';
  import { POWER_THRESHOLDS } from '../types';

  let {
    channels,
    activeChannelId,
    myPower,
    onSelect,
    onCreateClick,
    onModifyClick,
    onDeleteClick,
  }: {
    channels: ChannelInfo[];
    activeChannelId: string | null;
    myPower: number;
    onSelect: (channelId: string) => void;
    onCreateClick: () => void;
    onModifyClick: (channel: ChannelInfo) => void;
    onDeleteClick: (channel: ChannelInfo) => void;
  } = $props();

  let canModerate = $derived(myPower >= POWER_THRESHOLDS.kick);

  // Per spec §6.8: when local user is demoted below kick threshold,
  // close any open context menu (stale moderation surface).
  $effect(() => {
    if (!canModerate) {
      contextMenu = null;
    }
  });

  // Per spec §6.4: parent (CommunityView) hands us a list of joined-only
  // channels (deletedAt is filtered upstream). We just render in input
  // order.
  let visible = $derived(channels.filter((c) => c.deletedAt === undefined));

  let contextMenu = $state<{ channel: ChannelInfo; x: number; y: number } | null>(null);
  let menuEl: HTMLElement | undefined = $state();

  $effect(() => {
    if (!contextMenu) return;
    // Document-level outside-click listener — only active while menu is open.
    function onDocClick(e: MouseEvent) {
      // If the click landed inside the menu itself, don't dismiss
      // (the menu's own button onclick handlers will close it after action).
      const target = e.target as Node | null;
      if (menuEl && target && menuEl.contains(target)) return;
      contextMenu = null;
    }
    // capture: true so we beat the menu's own button onclick listeners
    // for clicks OUTSIDE the menu, while still letting menu-item clicks
    // through (since the contains() check short-circuits).
    document.addEventListener('click', onDocClick, true);
    return () => document.removeEventListener('click', onDocClick, true);
  });

  function handleContextMenu(e: MouseEvent, channel: ChannelInfo) {
    if (!canModerate) return;
    e.preventDefault();
    contextMenu = { channel, x: e.clientX, y: e.clientY };
  }

  function dismissContextMenu() {
    contextMenu = null;
  }

  function handleRename() {
    if (!contextMenu) return;
    const ch = contextMenu.channel;
    contextMenu = null;
    onModifyClick(ch);
  }

  function handleDelete() {
    if (!contextMenu) return;
    const ch = contextMenu.channel;
    contextMenu = null;
    onDeleteClick(ch);
  }
</script>

<svelte:window onkeydown={(e) => { if (e.key === 'Escape') dismissContextMenu(); }} />

<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<nav class="channel-sub-sidebar" aria-label="Channels">
  <ul class="channel-list">
    {#each visible as channel (channel.channelId)}
      <li>
        <button
          type="button"
          class="channel-item"
          class:active={channel.channelId === activeChannelId}
          onclick={(e) => { e.stopPropagation(); onSelect(channel.channelId); }}
          oncontextmenu={(e) => handleContextMenu(e, channel)}
        >
          <span class="channel-hash" aria-hidden="true">#</span>
          <span class="channel-name">{channel.name}</span>
        </button>
      </li>
    {/each}
  </ul>
  {#if canModerate}
    <button
      type="button"
      class="create-channel-btn"
      aria-label="Create channel"
      onclick={(e) => { e.stopPropagation(); onCreateClick(); }}
    >
      <span aria-hidden="true">+</span>
      <span class="create-label">Create channel</span>
    </button>
  {/if}
</nav>

{#if contextMenu}
  <div
    bind:this={menuEl}
    class="context-menu"
    role="menu"
    style="left: {contextMenu.x}px; top: {contextMenu.y}px"
  >
    <button type="button" role="menuitem" onclick={handleRename}>Rename</button>
    <button type="button" role="menuitem" onclick={handleDelete} class="destructive">Delete</button>
  </div>
{/if}

<style>
  .channel-sub-sidebar {
    display: flex;
    flex-direction: column;
    background: var(--bg-secondary);
    border-right: 1px solid var(--border);
    width: 200px;
    min-width: 0;
    overflow-y: auto;
  }
  .channel-list { list-style: none; margin: 0; padding: 6px 0; flex: 1; }
  .channel-item {
    display: flex;
    align-items: center;
    width: 100%;
    background: none;
    border: none;
    color: var(--text-secondary);
    padding: 6px 14px;
    cursor: pointer;
    font-size: 0.9rem;
    text-align: left;
  }
  .channel-item:hover { background: var(--bg-tertiary); color: var(--text-primary); }
  .channel-item.active { background: var(--bg-tertiary); color: var(--text-primary); font-weight: 500; }
  .channel-hash { color: var(--text-tertiary, var(--text-secondary)); margin-right: 6px; }
  .channel-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .create-channel-btn {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    background: none;
    border: none;
    padding: 8px 14px;
    color: var(--text-secondary);
    cursor: pointer;
    font-size: 0.85rem;
    border-top: 1px solid var(--border);
  }
  .create-channel-btn:hover { background: var(--bg-tertiary); color: var(--accent); }
  .context-menu {
    position: fixed;
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 4px;
    box-shadow: 0 2px 8px rgba(0, 0, 0, 0.3);
    z-index: 1000;
    min-width: 140px;
    padding: 4px 0;
  }
  .context-menu button {
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
  .context-menu button:hover { background: var(--bg-tertiary); }
  .context-menu button.destructive { color: #d83c3e; }
</style>
