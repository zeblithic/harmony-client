<script lang="ts">
  import type { Channel } from '../types';

  let { channels, collapsed = false }: {
    channels: Channel[];
    collapsed: boolean;
  } = $props();
</script>

<div class="nav-panel">
  <div class="nav-header">
    {#if !collapsed}
      <h2>Harmony Dev</h2>
    {:else}
      <span class="nav-icon">H</span>
    {/if}
  </div>
  <nav class="channel-list">
    {#each channels as channel (channel.id)}
      <button class="channel-item" class:has-unread={channel.unreadCount > 0}>
        <span class="channel-hash">#</span>
        {#if !collapsed}
          <span class="channel-name">{channel.name}</span>
          {#if channel.unreadCount > 0}
            <span class="unread-badge">{channel.unreadCount}</span>
          {/if}
        {/if}
      </button>
    {/each}
  </nav>
</div>

<style>
  .nav-panel {
    display: flex;
    flex-direction: column;
    height: 100%;
  }

  .nav-header {
    padding: 12px 16px;
    border-bottom: 1px solid var(--border);
    min-height: 48px;
    display: flex;
    align-items: center;
  }

  .nav-header h2 {
    font-size: 15px;
    font-weight: 600;
    color: var(--text-primary);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .nav-icon {
    font-size: 18px;
    font-weight: 700;
    color: var(--text-primary);
    width: 100%;
    text-align: center;
  }

  .channel-list {
    flex: 1;
    padding: 8px 0;
    overflow-y: auto;
  }

  .channel-item {
    display: flex;
    align-items: center;
    gap: 6px;
    width: 100%;
    padding: 6px 16px;
    border: none;
    background: none;
    color: var(--text-muted);
    font-size: 14px;
    cursor: pointer;
    text-align: left;
  }

  .channel-item:hover {
    background: var(--bg-tertiary);
    color: var(--text-primary);
  }

  .channel-item.has-unread {
    color: var(--text-primary);
    font-weight: 600;
  }

  .channel-hash {
    color: var(--text-muted);
    font-size: 16px;
    flex-shrink: 0;
  }

  .channel-name {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
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
</style>
