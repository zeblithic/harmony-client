<script lang="ts">
  import type { VineVideo } from '../types';
  import Avatar from './Avatar.svelte';
  import { relativeTime } from '../file-utils';
  import { vineCreatorLabel, vineOriginalCreatorLabel } from '../vine-utils';

  let { vine, onPlay, isViewed, showFollowButton = false, isFollowed = false, onFollow, onUnfollow, reactionCount = 0, likedByMe = false, onToggleLike, reshareCount = 0, onViewOriginal }: {
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
    reshareCount?: number;
    onViewOriginal?: (vineId: string) => void;
  } = $props();

  let viewed = $derived(isViewed ?? vine.viewed);
  let showReshareCount = $derived(!vine.reshareOf && reshareCount > 0);

  let timeStr = $derived(relativeTime(vine.createdAt * 1000));

  // ZEB-561: never render a blank creator/resharer — a descriptor published via
  // the headless RPC with `creatorName` omitted carries "", so fall back to a
  // truncated owner-hex.
  let creatorLabel = $derived(vineCreatorLabel(vine.creatorName, vine.creatorAddress));
  // Reuse the shared origin resolver (falls back to the source creator's
  // name/address pair) before the blank-guard, so "originally by" never shows
  // hex when a creator name is available (Qodo PR #337).
  let originalLabel = $derived(vineOriginalCreatorLabel(vine));

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
      onFollow?.(vine.creatorAddress, creatorLabel);
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
  aria-label="{vine.title ?? 'Untitled vine'} by {creatorLabel}"
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
      <Avatar address={vine.creatorAddress} size={18} displayName={creatorLabel} />
      <span class="creator-name">{creatorLabel}</span>
      <span class="timestamp">{timeStr}</span>
    </div>
    {#if vine.title}
      <p class="vine-title">{vine.title}</p>
    {/if}
    {#if vine.reshareOf}
      {#if onViewOriginal}
        <button
          type="button"
          class="attribution-link"
          onclick={(e) => { e.stopPropagation(); onViewOriginal?.(vine.reshareOf!); }}
          aria-label="originally by {originalLabel}"
        >
          <span aria-hidden="true">↗</span> originally by {originalLabel}
        </button>
      {:else}
        <span class="attribution-row">
          <span aria-hidden="true">↗</span> originally by {originalLabel}
        </span>
      {/if}
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
    {#if reactionCount > 0 || likedByMe || showReshareCount}
      <div class="card-like-row">
        {#if reactionCount > 0 || likedByMe}
          <button
            type="button"
            class="card-heart"
            onclick={handleLikeClick}
            aria-label={likedByMe ? `Unlike ${vine.title ?? 'vine'}` : `Like ${vine.title ?? 'vine'}`}
          >
            {likedByMe ? '❤️' : '🤍'}
          </button>
          <span class="card-like-count">{reactionCount}</span>
        {/if}
        {#if showReshareCount}
          <span class="reshare-count" aria-label="reshare count {reshareCount}">
            <span aria-hidden="true">↗</span> {reshareCount}
          </span>
        {/if}
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

  .attribution-link {
    background: none;
    border: none;
    padding: 0;
    margin: 0;
    color: var(--accent);
    font-size: 0.75rem;
    cursor: pointer;
    text-decoration: underline;
    text-align: left;
    width: fit-content;
  }

  .attribution-link:hover {
    opacity: 0.85;
  }

  .attribution-link:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  .attribution-row {
    color: var(--text-secondary);
    font-size: 0.75rem;
  }

  .reshare-count {
    font-size: 0.7rem;
    color: var(--text-muted);
    font-weight: 500;
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
    color: var(--text-bright);
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
    border-color: var(--danger-vivid);
    color: var(--danger-vivid);
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
