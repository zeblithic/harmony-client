<script lang="ts">
  import type { VineVideo } from '../types';
  import VineCard from './VineCard.svelte';
  import VinePlayer from './VinePlayer.svelte';

  type FeedFilter = 'all' | 'unviewed';

  let { vines, viewedIds, onMarkViewed, onPublish, onReshare }: {
    vines: VineVideo[];
    viewedIds: Set<string>;
    onMarkViewed?: (id: string) => void;
    onPublish?: () => void;
    onReshare?: (vine: VineVideo) => Promise<void> | void;
  } = $props();

  let activeVine = $state<VineVideo | null>(null);
  let feedFilter = $state<FeedFilter>('all');
  // Snapshot of the list at the time the player opened — prevents stale
  // index when marking a vine viewed removes it from filteredVines.
  let playerList = $state<VineVideo[]>([]);
  let activeIndex = $state(-1);

  let sortedVines = $derived(
    [...vines].sort((a, b) => b.createdAt - a.createdAt)
  );

  let filteredVines = $derived(
    feedFilter === 'unviewed'
      ? sortedVines.filter(v => !viewedIds.has(v.id))
      : sortedVines
  );

  let unviewedCount = $derived(
    vines.filter(v => !viewedIds.has(v.id)).length
  );

  function openPlayer(vine: VineVideo) {
    // Only snapshot the list when opening fresh (not navigating within player).
    if (!activeVine) {
      playerList = [...filteredVines];
    }
    activeIndex = playerList.findIndex(v => v.id === vine.id);
    activeVine = vine;
    onMarkViewed?.(vine.id);
  }

  function closePlayer() {
    activeVine = null;
    playerList = [];
    activeIndex = -1;
  }

  function nextVine() {
    if (activeIndex >= 0 && activeIndex < playerList.length - 1) {
      const next = playerList[activeIndex + 1];
      activeIndex = activeIndex + 1;
      activeVine = next;
      onMarkViewed?.(next.id);
    }
  }

  function previousVine() {
    if (activeIndex > 0) {
      const prev = playerList[activeIndex - 1];
      activeIndex = activeIndex - 1;
      activeVine = prev;
      onMarkViewed?.(prev.id);
    }
  }
</script>

<div class="vine-feed">
  <header class="feed-header">
    <h2 class="feed-title">Vines</h2>
    {#if unviewedCount > 0}
      <span class="unviewed-count" aria-label="{unviewedCount} unviewed">{unviewedCount} new</span>
    {/if}
    <div class="header-spacer"></div>
    {#if onPublish}
      <button type="button" class="create-btn" onclick={onPublish} aria-label="Create vine">+</button>
    {/if}
  </header>

  <div class="filter-bar">
    <button type="button" class="filter-tab" class:active={feedFilter === 'all'} onclick={() => feedFilter = 'all'}>All</button>
    <button type="button" class="filter-tab" class:active={feedFilter === 'unviewed'} onclick={() => feedFilter = 'unviewed'}>
      Unviewed{#if unviewedCount > 0}&nbsp;({unviewedCount}){/if}
    </button>
  </div>

  {#if filteredVines.length === 0}
    <p class="empty-state">
      {#if feedFilter === 'unviewed'}
        All caught up — no unviewed vines.
      {:else}
        No vines yet. Follow creators to see their vines here.
      {/if}
    </p>
  {:else}
    <div class="feed-list" role="list" aria-label="Vine feed">
      {#each filteredVines as vine (vine.id)}
        <div role="listitem">
          <VineCard {vine} onPlay={openPlayer} isViewed={viewedIds.has(vine.id)} />
        </div>
      {/each}
    </div>
  {/if}
</div>

{#if activeVine}
  <VinePlayer
    vine={activeVine}
    onClose={closePlayer}
    onNext={activeIndex >= 0 && activeIndex < playerList.length - 1 ? nextVine : undefined}
    onPrevious={activeIndex > 0 ? previousVine : undefined}
    {onReshare}
  />
{/if}

<style>
  .vine-feed {
    display: flex;
    flex-direction: column;
    height: 100%;
  }

  .feed-header {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 16px 16px 8px;
  }

  .feed-title {
    color: var(--text-primary);
    font-size: 1rem;
    font-weight: 600;
    margin: 0;
  }

  .unviewed-count {
    color: var(--accent);
    font-size: 0.75rem;
    font-weight: 600;
    background: rgba(88, 101, 242, 0.15);
    padding: 2px 8px;
    border-radius: 10px;
  }

  .header-spacer {
    flex: 1;
  }

  .create-btn {
    background: var(--accent);
    color: white;
    border: none;
    width: 28px;
    height: 28px;
    border-radius: 50%;
    font-size: 1.1rem;
    font-weight: 600;
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    transition: opacity 0.15s;
  }

  .create-btn:hover {
    opacity: 0.85;
  }

  .filter-bar {
    display: flex;
    gap: 4px;
    padding: 0 16px 8px;
  }

  .filter-tab {
    background: none;
    border: none;
    color: var(--text-muted);
    font-size: 0.75rem;
    font-weight: 500;
    padding: 4px 10px;
    border-radius: 12px;
    cursor: pointer;
    transition: background 0.15s, color 0.15s;
  }

  .filter-tab:hover {
    background: var(--bg-tertiary);
  }

  .filter-tab.active {
    background: var(--bg-tertiary);
    color: var(--text-primary);
    font-weight: 600;
  }

  .empty-state {
    color: var(--text-muted);
    font-size: 0.85rem;
    text-align: center;
    padding: 32px 16px;
    margin: 0;
  }

  .feed-list {
    flex: 1;
    overflow-y: auto;
    padding: 0 6px 16px;
  }
</style>
