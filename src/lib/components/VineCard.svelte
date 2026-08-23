<script lang="ts">
  import type { VineVideo } from '../types';
  import Avatar from './Avatar.svelte';
  import PeerName from './PeerName.svelte';
  import { relativeTime } from '../file-utils';
  import {
    resolveVineCreatorName,
    resolveVineOriginalCreatorName,
    formatVineDuration,
  } from '../vine-utils';

  let {
    vine, isPlaying = false, isViewed, videoUrl = null, duration = null,
    onActivate, onDuration,
    showFollowButton = false, isFollowed = false, onFollow, onUnfollow,
    reactionCount = 0, likedByMe = false, onToggleLike,
    reshareCount = 0, canReshare = false, resharing = false, onReshare,
    canDelete = false, deleting = false, onDelete,
    onViewOriginal,
    degreeLabel = null, viaLabel = null,
    resolveNickname, resolveCard,
  }: {
    vine: VineVideo;
    /** True when this card is the feed's single playing card. */
    isPlaying?: boolean;
    isViewed?: boolean;
    /** Blob URL — present only while the card is inside the feed's lazy window. */
    videoUrl?: string | null;
    /** Known duration in seconds (feed caches per CID once metadata loads). */
    duration?: number | null;
    /** Bring this card to center + play (click / Enter / Space). */
    onActivate?: (vine: VineVideo) => void;
    /** Reports the mounted video's metadata duration (honest badge source). */
    onDuration?: (cid: string, seconds: number) => void;
    showFollowButton?: boolean;
    isFollowed?: boolean;
    onFollow?: (address: string, name: string) => void;
    onUnfollow?: (address: string) => void;
    reactionCount?: number;
    likedByMe?: boolean;
    onToggleLike?: (vine: VineVideo) => void;
    reshareCount?: number;
    /** Whether the Reshare verb is offered (feed applies the own-original guard). */
    canReshare?: boolean;
    /** True while the feed's confirm→publish is in flight for THIS vine. */
    resharing?: boolean;
    /** Ask the feed to open the reshare confirmation for this vine. */
    onReshare?: (vine: VineVideo) => void;
    /** ZEB-670: whether the Delete verb is offered (feed applies the own-vine guard). */
    canDelete?: boolean;
    /** True while the feed's confirm→delete is in flight for THIS vine. */
    deleting?: boolean;
    /** Ask the feed to open the delete confirmation for this vine. */
    onDelete?: (vine: VineVideo) => void;
    onViewOriginal?: (vineId: string) => void;
    /** ZEB-671: Discover degree chip text ("2nd" / "3rd"); null hides it. */
    degreeLabel?: string | null;
    /** ZEB-671: provenance line ("Devin follows @ravi"); null hides it. */
    viaLabel?: string | null;
    /** ZEB-978: local petname for an owner_id — top ladder rung. */
    resolveNickname?: (id: string) => string | undefined;
    /** ZEB-978: verified broadcast profile card — second ladder rung. */
    resolveCard?: (id: string) => { displayName: string } | undefined;
  } = $props();

  let videoEl = $state<HTMLVideoElement | null>(null);

  let viewed = $derived(isViewed ?? vine.viewed);
  let showReshareCount = $derived(!vine.reshareOf && reshareCount > 0);
  let timeStr = $derived(relativeTime(vine.createdAt * 1000));
  // ZEB-978: names resolve through the shared ladder (petname ► card ►
  // wire ► hex) — the wire creatorName is unverified publisher text and
  // must never outrank a local petname or the verified card (spoof
  // defense). ZEB-561's never-blank floors live inside the resolvers.
  // `.label` serves the text-only sites (aria, Avatar, onFollow); the
  // visible name spans render through <PeerName> for provenance styling.
  let creatorName = $derived(resolveVineCreatorName(vine, resolveNickname, resolveCard));
  let creatorLabel = $derived(creatorName.label);
  let originalName = $derived(resolveVineOriginalCreatorName(vine, resolveNickname, resolveCard));
  let originalLabel = $derived(originalName.label);

  // Imperative play/pause on playing-state transitions. The `autoplay`
  // attribute only acts at element load, so a paused neighbor promoted to
  // the playing card needs an explicit play(). jsdom implements neither —
  // guard both (play may return undefined instead of a promise).
  $effect(() => {
    const el = videoEl;
    if (!el) return;
    try {
      if (isPlaying) void el.play()?.catch(() => {});
      else el.pause();
    } catch {
      // jsdom: HTMLMediaElement.play/pause are not implemented.
    }
  });

  function handleLoadedMetadata(e: Event) {
    const el = e.currentTarget as HTMLVideoElement;
    if (Number.isFinite(el.duration)) onDuration?.(vine.videoCid, el.duration);
  }

  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === 'Enter' || e.key === ' ') {
      // Don't intercept keyboard events from child buttons (follow/like/…).
      if (e.target instanceof HTMLButtonElement) return;
      e.preventDefault();
      onActivate?.(vine);
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

  function handleReshareClick(e: MouseEvent) {
    e.stopPropagation();
    onReshare?.(vine);
  }

  function handleDeleteClick(e: MouseEvent) {
    e.stopPropagation();
    onDelete?.(vine);
  }

  function handleViewOriginal(e: MouseEvent) {
    e.stopPropagation();
    if (vine.reshareOf) onViewOriginal?.(vine.reshareOf);
  }
</script>

<div
  class="vine-card"
  class:playing={isPlaying}
  class:viewed={viewed}
  role="button"
  tabindex="0"
  aria-label="{vine.title ?? 'Untitled vine'} by {creatorLabel}"
  onclick={() => onActivate?.(vine)}
  onkeydown={handleKeyDown}
>
  <div class="stage">
    {#if vine.originalRemoved}
      <!-- ZEB-670: the original was deleted by its creator — never play,
           even if a blob URL was resolved before the tombstone landed. -->
      <div class="removed-stub" role="note" data-testid="removed-stub">
        <span aria-hidden="true">🚫</span> Removed by creator
      </div>
    {:else if videoUrl}
      <!-- svelte-ignore a11y_media_has_caption -->
      <video
        class="stage-video"
        src={videoUrl}
        muted
        loop
        playsinline
        bind:this={videoEl}
        onloadedmetadata={handleLoadedMetadata}
        aria-label="Vine video"
        data-testid="stage-video"
      ></video>
    {:else}
      <span class="stage-placeholder" aria-hidden="true">▶</span>
    {/if}
    {#if !vine.originalRemoved && videoUrl && !isPlaying}
      <!-- Only over a loaded video — off-window cards show ▶, and stacking
           both centered glyphs would overlap (CodeRabbit PR #440). -->
      <span class="paused-glyph" aria-hidden="true">❚❚</span>
    {/if}
    {#if duration != null}
      <span class="duration-pill" data-testid="duration-pill">↻ {formatVineDuration(duration)}</span>
    {/if}
    {#if !viewed}
      <span class="unviewed-dot" aria-label="Unviewed"></span>
    {/if}
  </div>

  <div class="meta">
    <div class="creator-row">
      <Avatar address={vine.creatorAddress} size={26} displayName={creatorLabel} />
      <span class="creator-name"><PeerName name={creatorName} /></span>
      {#if degreeLabel}
        <span
          class="degree-chip"
          class:deg3={degreeLabel === '3rd'}
          data-testid="degree-chip"
        >{degreeLabel}</span>
      {/if}
      <span class="timestamp">{timeStr}</span>
      {#if showFollowButton}
        <button
          type="button"
          class="follow-btn"
          class:following={isFollowed}
          aria-label={isFollowed ? `Unfollow ${creatorLabel}` : `Follow ${creatorLabel}`}
          onclick={handleFollowClick}
        >
          {isFollowed ? 'Following' : 'Follow'}
        </button>
      {/if}
    </div>
    {#if viaLabel}
      <p class="via-line" data-testid="via-line">{viaLabel}</p>
    {/if}
    {#if vine.title}
      <p class="vine-title">{vine.title}</p>
    {/if}
    {#if vine.reshareOf}
      <!-- Resharer stays plain `.label` text: it duplicates the provenance-
           styled <PeerName> in the creator row directly above. -->
      <span class="attribution-row">
        <span aria-hidden="true">↻</span> {creatorLabel} reshared ·
        {#if onViewOriginal}
          <button
            type="button"
            class="attribution-link"
            onclick={handleViewOriginal}
            aria-label="view original by {originalLabel}"
          >view original by <PeerName name={originalName} /></button>
        {:else}
          view original by <PeerName name={originalName} />
        {/if}
      </span>
    {/if}
    <div class="action-rail">
      {#if onToggleLike && !vine.originalRemoved}
        <button
          type="button"
          class="rail-btn"
          class:liked={likedByMe}
          onclick={handleLikeClick}
          aria-label={likedByMe ? `Unlike ${vine.title ?? 'vine'}` : `Like ${vine.title ?? 'vine'}`}
        >
          {likedByMe ? '❤️' : '🤍'}{#if reactionCount > 0}<span class="rail-count">{reactionCount}</span>{/if}
        </button>
      {:else if reactionCount > 0}
        <span class="rail-chip">🤍 <span class="rail-count">{reactionCount}</span></span>
      {/if}
      {#if canReshare && onReshare && !vine.originalRemoved}
        <button
          type="button"
          class="rail-btn"
          onclick={handleReshareClick}
          disabled={resharing}
          aria-label="Reshare vine"
        >
          <span aria-hidden="true">↻</span> {resharing ? 'Resharing…' : 'Reshare'}{#if showReshareCount}<span class="rail-count">{reshareCount}</span>{/if}
        </button>
      {:else if showReshareCount}
        <span class="rail-chip" aria-label="reshare count {reshareCount}">
          <span aria-hidden="true">↻</span> <span class="rail-count">{reshareCount}</span>
        </span>
      {/if}
      {#if canDelete && onDelete}
        <button
          type="button"
          class="rail-btn rail-btn-danger"
          onclick={handleDeleteClick}
          disabled={deleting}
          aria-label="Delete vine"
        >
          <span aria-hidden="true">🗑</span> {deleting ? 'Deleting…' : 'Delete'}
        </button>
      {/if}
    </div>
  </div>
</div>

<style>
  .vine-card {
    position: relative;
    height: 100%;
    display: flex;
    flex-direction: column;
    justify-content: flex-end;
    background: var(--media-stage);
    border-radius: 8px;
    overflow: hidden;
    cursor: pointer;
  }

  .vine-card:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  .stage {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    transition: filter 0.2s ease;
  }

  /* Paused cards dim; viewed cards dim further while paused (spec §3). */
  .vine-card:not(.playing) .stage {
    filter: brightness(0.82);
  }

  .vine-card.viewed:not(.playing) .stage {
    filter: brightness(0.6);
  }

  .stage-video {
    width: 100%;
    height: 100%;
    object-fit: contain;
  }

  .stage-placeholder {
    color: color-mix(in srgb, var(--on-media-stage) 45%, transparent);
    font-size: 3rem;
  }

  .paused-glyph {
    position: absolute;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    font-size: 2rem;
    color: var(--on-media-stage);
    opacity: 0.85;
    pointer-events: none;
  }

  .duration-pill {
    position: absolute;
    top: 10px;
    left: 12px;
    font-family: var(--font-mono);
    font-size: 0.72rem;
    color: var(--on-media-stage);
    background: color-mix(in srgb, var(--media-stage) 72%, transparent);
    padding: 2px 10px;
    border-radius: 999px;
  }

  .unviewed-dot {
    position: absolute;
    top: 12px;
    right: 12px;
    width: 10px;
    height: 10px;
    background: var(--gov-clay);
    border-radius: 50%;
  }

  .meta {
    position: relative;
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 14px 16px;
    background: linear-gradient(
      transparent,
      color-mix(in srgb, var(--media-stage) 88%, transparent)
    );
    color: var(--on-media-stage);
  }

  .creator-row {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  /* ZEB-671: degree chips per the drawn Discover anatomy — 2nd = green
     (accent family), 3rd = clay/amber. Each chip uses a token PAIR
     designed for contrast in both themes: accent/on-accent, and
     clay-deep/clay-soft (which invert together across themes —
     CodeRabbit PR #447 flagged white-on-clay as borderline in light
     mode). */
  .degree-chip {
    font-family: var(--font-mono);
    font-size: 0.6rem;
    font-weight: 600;
    color: var(--on-accent);
    background: var(--accent);
    padding: 2px 8px;
    border-radius: 999px;
    flex-shrink: 0;
  }

  .degree-chip.deg3 {
    background: var(--gov-clay-deep);
    color: var(--gov-clay-soft);
  }

  .via-line {
    color: var(--text-secondary);
    font-size: 0.72rem;
    margin: 2px 0 0;
  }

  .creator-name {
    color: var(--on-media-stage);
    font-weight: 600;
    font-size: 0.85rem;
  }

  .timestamp {
    color: color-mix(in srgb, var(--on-media-stage) 62%, transparent);
    font-family: var(--font-mono);
    font-size: 0.72rem;
  }

  .vine-title {
    color: color-mix(in srgb, var(--on-media-stage) 88%, transparent);
    font-size: 0.85rem;
    margin: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .attribution-row {
    color: color-mix(in srgb, var(--on-media-stage) 75%, transparent);
    font-size: 0.75rem;
  }

  .attribution-link {
    background: none;
    border: none;
    padding: 0;
    margin: 0;
    color: var(--on-media-stage);
    font-size: 0.75rem;
    cursor: pointer;
    text-decoration: underline;
  }

  .attribution-link:hover {
    opacity: 0.85;
  }

  .attribution-link:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  .follow-btn {
    margin-left: auto;
    font-size: 0.7rem;
    font-weight: 600;
    padding: 2px 12px;
    border-radius: 999px;
    cursor: pointer;
    transition: background 0.15s, color 0.15s;
    background: var(--accent);
    color: var(--on-accent);
    border: 1px solid var(--accent);
  }

  .follow-btn:hover {
    opacity: 0.85;
  }

  .follow-btn.following {
    background: transparent;
    color: color-mix(in srgb, var(--on-media-stage) 70%, transparent);
    border-color: color-mix(in srgb, var(--on-media-stage) 45%, transparent);
  }

  .follow-btn.following:hover {
    border-color: var(--danger-vivid);
    color: var(--danger-vivid);
  }

  .action-rail {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 4px;
  }

  .rail-btn {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    background: transparent;
    border: 1px solid color-mix(in srgb, var(--on-media-stage) 28%, transparent);
    color: var(--on-media-stage);
    font-size: 0.78rem;
    padding: 4px 12px;
    border-radius: 999px;
    cursor: pointer;
    transition: background 0.15s;
  }

  .rail-btn:hover:not(:disabled) {
    background: color-mix(in srgb, var(--on-media-stage) 12%, transparent);
  }

  .rail-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  /* ZEB-670: destructive verb — same silhouette, danger accents. */
  .rail-btn-danger {
    border-color: color-mix(in srgb, var(--danger-vivid) 45%, transparent);
    color: color-mix(in srgb, var(--danger-vivid) 70%, var(--on-media-stage));
  }

  .rail-btn-danger:hover:not(:disabled) {
    background: color-mix(in srgb, var(--danger-vivid) 16%, transparent);
  }

  /* ZEB-670: "Removed by creator" stub fills the stage where the video
     would render. Muted on the media ground — informational, not alarming. */
  .removed-stub {
    display: flex;
    align-items: center;
    gap: 8px;
    color: color-mix(in srgb, var(--on-media-stage) 60%, transparent);
    font-size: 0.9rem;
  }

  .rail-btn.liked {
    border-color: color-mix(in srgb, var(--danger-vivid) 45%, transparent);
  }

  .rail-chip {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    color: color-mix(in srgb, var(--on-media-stage) 70%, transparent);
    font-size: 0.75rem;
  }

  .rail-count {
    font-family: var(--font-mono);
    font-size: 0.72rem;
  }
</style>
