<script lang="ts">
  import { onMount } from 'svelte';
  import type { VineVideo } from '../types';
  import Avatar from './Avatar.svelte';
  import ReshareConfirmDialog from './ReshareConfirmDialog.svelte';
  import { relativeTime } from '../file-utils';

  let { vine, onClose, onNext, onPrevious, onReshare, resolveVideo, onToggleLike, reactionCount = 0, likedByMe = false, onViewOriginal, ownAddress }: {
    vine: VineVideo;
    onClose: () => void;
    onNext?: () => void;
    onPrevious?: () => void;
    onReshare?: (vine: VineVideo) => Promise<void> | void;
    resolveVideo?: (cid: string) => Promise<string>;
    onToggleLike?: (vine: VineVideo) => void;
    reactionCount?: number;
    likedByMe?: boolean;
    onViewOriginal?: (vineId: string) => void;
    /**
     * Local node's hex address — when supplied, hex-keyed self-authored
     * vines (those whose `creatorAddress` matches `ownAddress` but
     * weren't remapped to the magic `'self'` value by `wireToVine`,
     * typically because they arrived before `ownAddress` was set on
     * the service) also hide the Reshare button. Without this prop,
     * the `'self'` magic value is the only signal — see FIX 2 in
     * PR #120 round 1.
     */
    ownAddress?: string;
  } = $props();

  let overlayEl: HTMLDivElement;
  let resharing = $state(false);
  let reshareError = $state('');
  let reshareGeneration = 0;
  let showReshareConfirm = $state(false);

  let isOwnOriginal = $derived(
    !vine.reshareOf && (
      vine.creatorAddress === 'self'
      || (ownAddress != null && vine.creatorAddress === ownAddress)
    )
  );
  let canReshare = $derived(!!onReshare && !isOwnOriginal);

  // ── Video resolution state ──────────────────────────────────────────
  let videoUrl = $state<string | null>(null);
  let videoLoading = $state(false);
  let videoError = $state('');
  let videoGeneration = 0;

  $effect(() => {
    const cid = vine.videoCid;
    const resolver = resolveVideo;
    if (!resolver || !cid) return;

    // Invalidate any in-flight fetch from a previous vine.
    const gen = ++videoGeneration;
    videoUrl = null;
    videoLoading = true;
    videoError = '';

    let ownUrl: string | null = null;

    resolver(cid).then((url) => {
      if (gen !== videoGeneration) { URL.revokeObjectURL(url); return; }
      ownUrl = url;
      videoUrl = url;
      videoLoading = false;
    }).catch((err) => {
      if (gen !== videoGeneration) return;
      videoError = err instanceof Error ? err.message : 'Failed to load video';
      videoLoading = false;
    });

    return () => {
      // Revoke blob URL when vine changes or component unmounts.
      // Uses local ownUrl (not reactive videoUrl) so cleanup works
      // even if the component unmounts while a fetch is in-flight.
      if (ownUrl) URL.revokeObjectURL(ownUrl);
    };
  });

  // Clear reshare state and invalidate in-flight calls when navigating.
  $effect(() => { void vine; reshareGeneration++; resharing = false; reshareError = ''; });

  onMount(() => overlayEl?.focus());

  function handleReshare() {
    if (resharing) return;
    showReshareConfirm = true;
  }

  async function confirmReshare() {
    showReshareConfirm = false;
    if (!onReshare || resharing) return;
    resharing = true;
    reshareError = '';
    const generation = ++reshareGeneration;
    try {
      await onReshare(vine);
    } catch (err) {
      if (generation === reshareGeneration) {
        reshareError = err instanceof Error ? err.message : 'Reshare failed';
      }
    } finally {
      if (generation === reshareGeneration) resharing = false;
    }
  }

  function cancelReshare() {
    showReshareConfirm = false;
  }

  function handleKeyDown(e: KeyboardEvent) {
    // Suspend player hotkeys while the reshare confirmation dialog is
    // open — otherwise Arrow keys would navigate the underlying feed
    // and Escape would close the player behind the modal, letting a
    // subsequent confirm run against the wrong active vine.
    if (showReshareConfirm) return;
    if (e.key === 'Escape') onClose();
    else if (e.key === 'ArrowRight' && onNext) { e.preventDefault(); onNext(); }
    else if (e.key === 'ArrowLeft' && onPrevious) { e.preventDefault(); onPrevious(); }
  }

  let timeStr = $derived(relativeTime(vine.createdAt * 1000));
</script>

<svelte:window onkeydown={handleKeyDown} />

<div class="player-overlay" role="dialog" aria-label="Vine player" aria-modal="true" tabindex="-1" bind:this={overlayEl}>
  <div class="player-header">
    <div class="creator-info">
      <Avatar address={vine.creatorAddress} size={28} displayName={vine.creatorName} />
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
      {#if videoUrl}
        <!-- svelte-ignore a11y_media_has_caption -->
        <video
          class="vine-video"
          src={videoUrl}
          controls
          autoplay
          loop
          playsinline
          aria-label="Vine video"
        ></video>
      {:else if videoLoading}
        <div class="placeholder-video">
          <span class="loading-label">Loading video…</span>
          <span class="cid-label">{vine.videoCid}</span>
        </div>
      {:else if videoError}
        <div class="placeholder-video">
          <span class="error-label">{videoError}</span>
          <span class="cid-label">{vine.videoCid}</span>
        </div>
      {:else}
        <div class="placeholder-video">
          <span class="play-icon" aria-hidden="true">▶</span>
          <span class="placeholder-label">Video playback coming soon</span>
          <span class="cid-label">{vine.videoCid}</span>
        </div>
      {/if}
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
      {#if onViewOriginal}
        <button
          type="button"
          class="attribution-link"
          onclick={() => onViewOriginal?.(vine.reshareOf!)}
          aria-label="originally by {vine.originalCreatorName ?? vine.creatorName}"
        >
          <span aria-hidden="true">↗</span> originally by {vine.originalCreatorName ?? vine.creatorName}
        </button>
      {:else}
        <p class="attribution-row">
          <span aria-hidden="true">↗</span> originally by {vine.originalCreatorName ?? vine.creatorName}
        </p>
      {/if}
    {/if}
    <div class="footer-actions">
      {#if onToggleLike}
        <button
          type="button"
          class="action-btn like-btn"
          class:liked={likedByMe}
          onclick={() => onToggleLike?.(vine)}
          aria-label={likedByMe ? `Unlike ${vine.title ?? 'vine'}` : `Like ${vine.title ?? 'vine'}`}
        >
          <span class="heart">{likedByMe ? '❤️' : '🤍'}</span>
          {#if reactionCount > 0}
            <span class="like-count">{reactionCount}</span>
          {/if}
        </button>
      {/if}
      {#if canReshare}
        <button type="button" class="action-btn" onclick={handleReshare} disabled={resharing} aria-label="Reshare vine">
          <span aria-hidden="true">↗</span> {resharing ? 'Resharing\u2026' : 'Reshare'}
        </button>
      {/if}
      {#if reshareError}
        <span class="reshare-error" role="alert">{reshareError}</span>
      {/if}
    </div>
  </div>

  {#if showReshareConfirm}
    <ReshareConfirmDialog
      {vine}
      onConfirm={confirmReshare}
      onCancel={cancelReshare}
    />
  {/if}
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
    align-items: center;
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

  .vine-video {
    width: 100%;
    height: 100%;
    object-fit: contain;
    background: #000;
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

  .placeholder-label,
  .loading-label {
    font-size: 0.85rem;
    color: var(--text-muted);
  }

  .error-label {
    font-size: 0.85rem;
    color: #ed4245;
  }

  .cid-label {
    font-size: 0.7rem;
    color: var(--text-muted);
    font-family: monospace;
    opacity: 0.6;
    max-width: 80%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
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

  .attribution-link {
    background: none;
    border: none;
    padding: 0;
    margin: 0;
    color: var(--accent);
    font-size: 0.875rem;
    cursor: pointer;
    text-decoration: underline;
  }
  .attribution-link:hover { opacity: 0.85; }
  .attribution-link:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .attribution-row {
    color: var(--text-secondary);
    font-size: 0.875rem;
    margin: 0;
  }

  .footer-actions {
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 12px;
    margin-top: 8px;
  }

  .reshare-error {
    color: #ed4245;
    font-size: 0.75rem;
  }

  .action-btn {
    display: flex;
    align-items: center;
    gap: 4px;
    background: none;
    border: 1px solid var(--bg-tertiary);
    color: var(--text-secondary);
    font-size: 0.8rem;
    padding: 6px 14px;
    border-radius: 6px;
    cursor: pointer;
    transition: background 0.15s, color 0.15s;
  }

  .action-btn:hover:not(:disabled) {
    background: var(--bg-tertiary);
    color: var(--text-primary);
  }

  .action-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .like-btn.liked {
    color: #ed4245;
    border-color: rgba(237, 66, 69, 0.3);
  }

  .heart {
    font-size: 1rem;
  }

  .like-count {
    font-size: 0.8rem;
  }
</style>
