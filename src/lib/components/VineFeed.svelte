<script lang="ts">
  import type { VineVideo } from '../types';
  import VineCard from './VineCard.svelte';
  import VinePlayer from './VinePlayer.svelte';

  type FeedFilter = 'all' | 'unviewed';
  type FeedTab = 'following' | 'discover';

  let {
    followedVines = [],
    discoverVines = [],
    viewedIds,
    activeTab = 'following' as FeedTab,
    followedAddresses = new Set<string>(),
    onTabChange,
    onMarkViewed,
    onPublish,
    onReshare,
    onFollow,
    onUnfollow,
    resolveVideo,
    getReaction,
    onToggleLike,
    getReshareCount,
    onViewOriginal,
    playTarget = null,
    onPlayTargetConsumed,
  }: {
    followedVines?: VineVideo[];
    discoverVines?: VineVideo[];
    viewedIds: Set<string>;
    activeTab?: FeedTab;
    followedAddresses?: Set<string>;
    onTabChange?: (tab: FeedTab) => void;
    onMarkViewed?: (id: string) => void;
    onPublish?: () => void;
    onReshare?: (vine: VineVideo) => Promise<void> | void;
    onFollow?: (address: string, name: string) => void;
    onUnfollow?: (address: string) => void;
    resolveVideo?: (cid: string) => Promise<string>;
    getReaction?: (vineId: string) => { count: number; likedByMe: boolean };
    onToggleLike?: (vine: VineVideo) => void;
    getReshareCount?: (vineId: string) => number;
    onViewOriginal?: (vineId: string) => void;
    /**
     * Parent-controlled "open this vine in the player" request.
     *
     * Used by App.svelte's `handleViewOriginal` to drive the player
     * from outside the feed (e.g., when the user clicks an attribution
     * link and we resolve the original via `vineService.findVine`).
     * VineFeed watches this via `$effect`, calls its internal
     * `openPlayer` when it transitions to non-null, and notifies the
     * parent via `onPlayTargetConsumed` so the parent can null it back
     * out for the next click. The deliberate clear+microtask dance in
     * App keeps two clicks in a row from being a no-op.
     */
    playTarget?: VineVideo | null;
    onPlayTargetConsumed?: () => void;
  } = $props();

  let activeVine = $state<VineVideo | null>(null);
  let feedFilter = $state<FeedFilter>('all');
  let playerList = $state<VineVideo[]>([]);
  let activeIndex = $state(-1);

  // Parent-driven open: when `playTarget` transitions to a vine, open
  // it in the player. The seed playerList is the entire concat'd feed
  // because the original might live in either followedVines or
  // discoverVines (and the user may currently be on the other tab).
  // After consuming, we notify the parent so it can null the slot —
  // we never read `playTarget` for navigation, only for triggering the
  // open.
  $effect(() => {
    if (playTarget) {
      const target = playTarget;
      playerList = [...followedVines, ...discoverVines];
      activeIndex = playerList.findIndex(v => v.id === target.id);
      activeVine = target;
      onMarkViewed?.(target.id);
      onPlayTargetConsumed?.();
    }
  });

  let activeVines = $derived(
    activeTab === 'following' ? followedVines : discoverVines
  );

  let sortedVines = $derived(
    [...activeVines].sort((a, b) => b.createdAt - a.createdAt)
  );

  let filteredVines = $derived(
    activeTab === 'following' && feedFilter === 'unviewed'
      ? sortedVines.filter(v => !viewedIds.has(v.id))
      : sortedVines
  );

  let unviewedCount = $derived(
    followedVines.filter(v => !viewedIds.has(v.id)).length
  );

  function openPlayer(vine: VineVideo) {
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

  <div class="tab-bar">
    <button type="button" class="tab" class:active={activeTab === 'following'} onclick={() => onTabChange?.('following')}>Following</button>
    <button type="button" class="tab" class:active={activeTab === 'discover'} onclick={() => onTabChange?.('discover')}>Discover</button>
  </div>

  {#if activeTab === 'following'}
    <div class="filter-bar">
      <button type="button" class="filter-tab" class:active={feedFilter === 'all'} onclick={() => feedFilter = 'all'}>All</button>
      <button type="button" class="filter-tab" class:active={feedFilter === 'unviewed'} onclick={() => feedFilter = 'unviewed'}>
        Unviewed{#if unviewedCount > 0}&nbsp;({unviewedCount}){/if}
      </button>
    </div>
  {/if}

  {#if filteredVines.length === 0}
    <p class="empty-state">
      {#if activeTab === 'following'}
        {#if feedFilter === 'unviewed'}
          All caught up — no unviewed vines.
        {:else}
          Follow creators to build your feed. Check out the Discover tab to find people to follow.
        {/if}
      {:else}
        No vines on the network yet.
      {/if}
    </p>
  {:else}
    <div class="feed-list" role="list" aria-label="Vine feed">
      {#each filteredVines as vine (vine.id)}
        {@const reaction = getReaction?.(vine.id)}
        <div role="listitem">
          <VineCard
            {vine}
            onPlay={openPlayer}
            isViewed={viewedIds.has(vine.id)}
            showFollowButton={vine.creatorAddress !== 'self'}
            isFollowed={followedAddresses.has(vine.creatorAddress)}
            {onFollow}
            {onUnfollow}
            reactionCount={reaction?.count ?? 0}
            likedByMe={reaction?.likedByMe ?? false}
            {onToggleLike}
            reshareCount={getReshareCount?.(vine.id) ?? 0}
            {onViewOriginal}
          />
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
    {resolveVideo}
    reactionCount={getReaction?.(activeVine.id)?.count ?? 0}
    likedByMe={getReaction?.(activeVine.id)?.likedByMe ?? false}
    {onToggleLike}
    {onViewOriginal}
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

  .tab-bar {
    display: flex;
    gap: 0;
    padding: 0 16px 4px;
    border-bottom: 1px solid var(--bg-tertiary);
  }

  .tab {
    background: none;
    border: none;
    border-bottom: 2px solid transparent;
    color: var(--text-muted);
    font-size: 0.85rem;
    font-weight: 500;
    padding: 8px 16px;
    cursor: pointer;
    transition: color 0.15s, border-color 0.15s;
  }

  .tab:hover {
    color: var(--text-primary);
  }

  .tab.active {
    color: var(--text-primary);
    font-weight: 600;
    border-bottom-color: var(--accent);
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
