<script lang="ts">
  import type { VineVideo } from '../types';
  import Avatar from './Avatar.svelte';
  import { relativeTime } from '../file-utils';

  let { vine, onPlay, isViewed }: {
    vine: VineVideo;
    onPlay: (vine: VineVideo) => void;
    isViewed?: boolean;
  } = $props();

  let viewed = $derived(isViewed ?? vine.viewed);

  let timeStr = $derived(relativeTime(vine.createdAt * 1000));

  function handleClick() {
    onPlay(vine);
  }

  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      onPlay(vine);
    }
  }
</script>

<div
  class="vine-card"
  class:viewed={viewed}
  role="button"
  tabindex="0"
  aria-label="{vine.title ?? 'Untitled vine'} by {vine.creatorName}"
  onclick={handleClick}
  onkeydown={handleKeyDown}
>
  <div class="thumbnail">
    <span class="play-icon" aria-hidden="true">▶</span>
    {#if !viewed}
      <span class="unviewed-dot" aria-label="Unviewed"></span>
    {/if}
  </div>
  <div class="card-info">
    <div class="creator-row">
      <Avatar address={vine.creatorAddress} size={18} displayName={vine.creatorName} />
      <span class="creator-name">{vine.creatorName}</span>
      <span class="timestamp">{timeStr}</span>
    </div>
    {#if vine.title}
      <p class="vine-title">{vine.title}</p>
    {/if}
    {#if vine.reshareOf}
      <span class="reshare-badge">reshare</span>
    {/if}
  </div>
</div>

<style>
  .vine-card {
    display: flex;
    gap: 10px;
    padding: 10px;
    border-radius: 8px;
    cursor: pointer;
    transition: background 0.15s;
  }

  .vine-card:hover,
  .vine-card:focus-visible {
    background: var(--bg-tertiary);
    outline: none;
  }

  .vine-card.viewed {
    opacity: 0.7;
  }

  .thumbnail {
    width: 56px;
    height: 72px;
    flex-shrink: 0;
    background: var(--bg-tertiary);
    border-radius: 6px;
    display: flex;
    align-items: center;
    justify-content: center;
    position: relative;
  }

  .play-icon {
    color: var(--text-muted);
    font-size: 1.2rem;
  }

  .unviewed-dot {
    position: absolute;
    top: 4px;
    right: 4px;
    width: 8px;
    height: 8px;
    background: var(--accent);
    border-radius: 50%;
  }

  .card-info {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }

  .creator-row {
    display: flex;
    align-items: center;
    gap: 6px;
  }

  .creator-name {
    color: var(--text-primary);
    font-weight: 600;
    font-size: 0.85rem;
  }

  .timestamp {
    color: var(--text-muted);
    font-size: 0.75rem;
  }

  .vine-title {
    color: var(--text-secondary);
    font-size: 0.8rem;
    margin: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .reshare-badge {
    display: inline-block;
    color: var(--text-muted);
    font-size: 0.7rem;
    background: var(--bg-secondary);
    padding: 1px 6px;
    border-radius: 4px;
    width: fit-content;
  }
</style>
