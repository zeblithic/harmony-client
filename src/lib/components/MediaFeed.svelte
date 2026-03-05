<script lang="ts">
  import type { Message } from '../types';
  import { TrustService } from '../trust-service';
  import MediaCard from './MediaCard.svelte';
  import UntrustedMediaCard from './UntrustedMediaCard.svelte';

  let { messages, trustService, trustVersion = 0, onLinkBack, onAvatarClick, onTrustChange }: {
    messages: Message[];
    trustService: TrustService;
    trustVersion?: number;
    onLinkBack?: (messageId: string) => void;
    onAvatarClick?: (address: string, event: MouseEvent) => void;
    onTrustChange?: () => void;
  } = $props();

  let mediaItems = $derived(
    messages
      .filter((m) => m.media.length > 0)
      .flatMap((m) => m.media.map((a) => ({ message: m, attachment: a })))
  );

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
      <!-- communityId not yet available on Message; per-community trust will apply once content transport provides context -->
      {#if TrustService.isGated(attachment) && trustService.resolve(message.sender.address) !== 'trusted' && !trustService.isLoaded(attachment.id)}
        <UntrustedMediaCard {message} {attachment} {onLinkBack} {onAvatarClick} onLoad={handleLoad} />
      {:else}
        <MediaCard {message} {attachment} {onLinkBack} {onAvatarClick} />
      {/if}
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
</style>
