<script lang="ts">
  import type { VineVideo } from '../types';
  import Avatar from './Avatar.svelte';
  import { relativeTime } from '../file-utils';

  let { vine, onPlay, isViewed, showFollowButton = false, isFollowed = false, onFollow, onUnfollow, reactionCount = 0, likedByMe = false, onToggleLike }: {
    vine: VineVideo;
    onPlay: (vine: VineVideo) => void;
    isViewed?: boolean;
    showFollowButton?: boolean;
    isFollowed?: boolean;
    onFollow?: (address: string, name: string) => void;
    onUnfollow?: (address: string) => void;
    reactionCount?: number;
    likedByMe?: boolean;
    onToggleLike?: (vine: VineVideo) => void;
  } = $props();

  let viewed = $derived(isViewed ?? vine.viewed);

  let timeStr = $derived(relativeTime(vine.createdAt * 1000));

  function handleClick() {
    onPlay(vine);
  }

  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === 'Enter' || e.key === ' ') {
      // Don't intercept keyboard events from child buttons (e.g. follow button)
      if (e.target instanceof HTMLButtonElement) return;
      e.preventDefault();
      onPlay(vine);
    }
  }

  function handleFollowClick(e: MouseEvent) {
    e.stopPropagation();
    if (isFollowed) {
      onUnfollow?.(vine.creatorAddress);
    } else {
      onFollow?.(vine.creatorAddress, vine.creatorName);
    }
  }

  function handleLikeClick(e: MouseEvent) {
    e.stopPropagation();
    onToggleLike?.(vine);
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
    {#if showFollowButton}
      <button
        type="button"
        class="follow-btn"
        class:following={isFollowed}
        aria-label={isFollowed ? `Unfollow ${vine.creatorName}` : `Follow ${vine.creatorName}`}
        onclick={handleFollowClick}
      >
        {isFollowed ? 'Following' : 'Follow'}
      </button>
    {/if}
    {#if reactionCount > 0 || likedByMe}
      <div class="card-like-row">
        <button
          type="button"
          class="card-heart"
          onclick={handleLikeClick}
          aria-label={likedByMe ? `Unlike ${vine.title ?? 'vine'}` : `Like ${vine.title ?? 'vine'}`}
        >
          {likedByMe ? '❤️' : '🤍'}
        </button>
        <span class="card-like-count">{reactionCount}</span>
      </div>
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

  .follow-btn {
    display: inline-block;
    font-size: 0.7rem;
    font-weight: 600;
    padding: 2px 10px;
    border-radius: 12px;
    cursor: pointer;
    width: fit-content;
    transition: background 0.15s, color 0.15s;
    background: var(--accent);
    color: white;
    border: 1px solid var(--accent);
  }

  .follow-btn:hover {
    opacity: 0.85;
  }

  .follow-btn.following {
    background: transparent;
    color: var(--text-muted);
    border-color: var(--text-muted);
  }

  .follow-btn.following:hover {
    border-color: #e74c3c;
    color: #e74c3c;
  }

  .card-like-row {
    display: flex;
    align-items: center;
    gap: 4px;
    margin-top: 2px;
  }

  .card-heart {
    background: none;
    border: none;
    cursor: pointer;
    font-size: 0.8rem;
    padding: 0;
    line-height: 1;
    transition: transform 0.15s;
  }

  .card-heart:hover {
    transform: scale(1.2);
  }

  .card-like-count {
    font-size: 0.7rem;
    color: var(--text-muted);
    font-weight: 500;
  }
</style>
