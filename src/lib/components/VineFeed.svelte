<script lang="ts">
  import type { VineVideo } from '../types';
  import VineCard from './VineCard.svelte';
  import VinePlayer from './VinePlayer.svelte';

  let { vines }: {
    vines: VineVideo[];
  } = $props();

  let activeVine = $state<VineVideo | null>(null);
  // Initialize from prop, then maintained locally as user views vines
  let viewedIds = $state(new Set<string>(
    // eslint-disable-next-line svelte/valid-compile -- intentional: seed from initial prop value
    vines.filter(v => v.viewed).map(v => v.id)
  ));

  let sortedVines = $derived(
    [...vines].sort((a, b) => b.createdAt - a.createdAt)
  );

  let unviewedCount = $derived(
    vines.filter(v => !viewedIds.has(v.id)).length
  );

  let activeIndex = $derived(
    activeVine ? sortedVines.findIndex(v => v.id === activeVine!.id) : -1
  );

  function openPlayer(vine: VineVideo) {
    activeVine = vine;
    viewedIds = new Set([...viewedIds, vine.id]);
    // invoke('mark_vine_viewed', { vineId: vine.id }); // wire up when transport is ready
  }

  function closePlayer() {
    activeVine = null;
  }

  function nextVine() {
    if (activeIndex < sortedVines.length - 1) {
      openPlayer(sortedVines[activeIndex + 1]);
    }
  }

  function previousVine() {
    if (activeIndex > 0) {
      openPlayer(sortedVines[activeIndex - 1]);
    }
  }
</script>

<div class="vine-feed">
  <header class="feed-header">
    <h2 class="feed-title">Vines</h2>
    {#if unviewedCount > 0}
      <span class="unviewed-count" aria-label="{unviewedCount} unviewed">{unviewedCount} new</span>
    {/if}
  </header>

  {#if sortedVines.length === 0}
    <p class="empty-state">No vines yet. Follow creators to see their vines here.</p>
  {:else}
    <div class="feed-list" role="list" aria-label="Vine feed">
      {#each sortedVines as vine (vine.id)}
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
    onNext={activeIndex < sortedVines.length - 1 ? nextVine : undefined}
    onPrevious={activeIndex > 0 ? previousVine : undefined}
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
