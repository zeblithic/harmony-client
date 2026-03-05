<script lang="ts">
  import type { Message } from '../types';
  import { TrustService } from '../trust-service';
  import MediaCard from './MediaCard.svelte';
  import UntrustedMediaCard from './UntrustedMediaCard.svelte';

  let { messages, trustService, trustVersion = 0, threadMessageIds = new Set<string>(), onLinkBack, onAvatarClick, onTrustChange }: {
    messages: Message[];
    trustService: TrustService;
    trustVersion?: number;
    threadMessageIds?: Set<string>;
    onLinkBack?: (messageId: string) => void;
    onAvatarClick?: (address: string, event: MouseEvent) => void;
    onTrustChange?: () => void;
  } = $props();

  let mediaItems = $derived(
    messages
      .filter((m) => m.media.length > 0)
      .flatMap((m) => m.media.map((a) => ({ message: m, attachment: a })))
  );

  function isTrustGated(attachment: import('../types').MediaAttachment, senderAddress: string): boolean {
    void trustVersion;
    return TrustService.isGated(attachment)
      && trustService.resolve(senderAddress) !== 'trusted'
      && !trustService.isLoaded(attachment.id);
  }

  function handleLoad(attachmentId: string) {
    trustService.markLoaded(attachmentId);
    onTrustChange?.();
  }
</script>

<div class="media-feed">
  {#if mediaItems.length === 0}
    <div class="empty-state">No media yet</div>
  {:else}
    {#each mediaItems as { message, attachment } (attachment.id)}
      <div class={threadMessageIds.has(message.id) ? 'thread-card' : ''}>
        <!-- communityId not yet available on Message; per-community trust will apply once content transport provides context -->
        {#if isTrustGated(attachment, message.sender.address)}
          <UntrustedMediaCard {message} {attachment} {onLinkBack} {onAvatarClick} onLoad={handleLoad} />
        {:else}
          <MediaCard {message} {attachment} {onLinkBack} {onAvatarClick} />
        {/if}
        {#if threadMessageIds.has(message.id)}
          <span class="thread-tag">in thread</span>
        {/if}
      </div>
    {/each}
  {/if}
</div>

<style>
  .media-feed {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }

  .empty-state {
    color: var(--text-muted);
    text-align: center;
    padding: 40px 20px;
    font-size: 14px;
  }

  .thread-card {
    border-left: 3px solid var(--accent);
    border-radius: 0 8px 8px 0;
    position: relative;
    transition: opacity 0.2s ease, max-height 0.3s ease;
    overflow: hidden;
  }

  .thread-card.exiting {
    opacity: 0;
    max-height: 0 !important;
    padding: 0;
    margin: 0;
  }

  @media (prefers-reduced-motion: reduce) {
    .thread-card {
      transition: none;
    }
    .thread-card.exiting {
      display: none;
    }
  }

  .thread-tag {
    position: absolute;
    top: 8px;
    right: 12px;
    font-size: 11px;
    color: var(--accent);
    opacity: 0.7;
  }
</style>
