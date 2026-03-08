<script lang="ts">
  import { onMount } from 'svelte';
  import type { VineVideo } from '../types';

  let { vine, onClose, onNext, onPrevious }: {
    vine: VineVideo;
    onClose: () => void;
    onNext?: () => void;
    onPrevious?: () => void;
  } = $props();

  let overlayEl: HTMLDivElement;

  onMount(() => overlayEl?.focus());

  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === 'Escape') onClose();
    else if (e.key === 'ArrowRight' && onNext) onNext();
    else if (e.key === 'ArrowLeft' && onPrevious) onPrevious();
  }

  let timeStr = $derived(
    new Date(vine.createdAt * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })
  );
</script>

<svelte:window onkeydown={handleKeyDown} />

<div class="player-overlay" role="dialog" aria-label="Vine player" aria-modal="true" tabindex="-1" bind:this={overlayEl}>
  <div class="player-header">
    <div class="creator-info">
      <span class="creator-name">{vine.creatorName}</span>
      <span class="timestamp">{timeStr}</span>
    </div>
    <button type="button" class="close-btn" onclick={onClose} aria-label="Close player">✕</button>
  </div>

  <div class="player-content">
    {#if onPrevious}
      <button type="button" class="nav-btn nav-prev" onclick={onPrevious} aria-label="Previous vine">‹</button>
    {/if}

    <div class="video-area" aria-label={vine.title ?? 'Untitled vine'}>
      <div class="placeholder-video">
        <span class="play-icon" aria-hidden="true">▶</span>
        <span class="cid-label">{vine.videoCid}</span>
      </div>
    </div>

    {#if onNext}
      <button type="button" class="nav-btn nav-next" onclick={onNext} aria-label="Next vine">›</button>
    {/if}
  </div>

  <div class="player-footer">
    {#if vine.title}
      <p class="vine-title">{vine.title}</p>
    {/if}
    {#if vine.reshareOf}
      <p class="reshare-label">Reshared</p>
    {/if}
  </div>
</div>

<style>
  .player-overlay {
    position: fixed;
    inset: 0;
    z-index: 100;
    background: rgba(0, 0, 0, 0.92);
    display: flex;
    flex-direction: column;
    align-items: center;
  }

  .player-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    width: 100%;
    padding: 16px 20px;
  }

  .creator-info {
    display: flex;
    align-items: baseline;
    gap: 8px;
  }

  .creator-name {
    color: var(--text-primary);
    font-weight: 600;
    font-size: 0.95rem;
  }

  .timestamp {
    color: var(--text-muted);
    font-size: 0.8rem;
  }

  .close-btn {
    background: none;
    border: none;
    color: var(--text-secondary);
    font-size: 1.2rem;
    cursor: pointer;
    padding: 4px 8px;
    border-radius: 4px;
  }

  .close-btn:hover {
    color: var(--text-primary);
    background: var(--bg-tertiary);
  }

  .player-content {
    flex: 1;
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 16px;
    width: 100%;
    padding: 0 20px;
    min-height: 0;
  }

  .nav-btn {
    background: none;
    border: none;
    color: var(--text-muted);
    font-size: 2.5rem;
    cursor: pointer;
    padding: 8px 12px;
    border-radius: 8px;
    flex-shrink: 0;
  }

  .nav-btn:hover {
    color: var(--text-primary);
    background: rgba(255, 255, 255, 0.08);
  }

  .video-area {
    max-width: 400px;
    width: 100%;
    aspect-ratio: 9 / 16;
    max-height: calc(100vh - 160px);
    border-radius: 12px;
    overflow: hidden;
  }

  .placeholder-video {
    width: 100%;
    height: 100%;
    background: var(--bg-tertiary);
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 12px;
  }

  .play-icon {
    font-size: 3rem;
    color: var(--text-muted);
  }

  .cid-label {
    font-size: 0.7rem;
    color: var(--text-muted);
    font-family: monospace;
    opacity: 0.6;
  }

  .player-footer {
    padding: 12px 20px 20px;
    text-align: center;
    width: 100%;
    max-width: 400px;
  }

  .vine-title {
    color: var(--text-primary);
    font-size: 0.95rem;
    margin: 0 0 4px;
  }

  .reshare-label {
    color: var(--text-muted);
    font-size: 0.8rem;
    margin: 0;
  }
</style>
