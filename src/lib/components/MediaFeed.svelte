<script lang="ts">
  import type { Message } from '../types';
  import MediaCard from './MediaCard.svelte';

  let { messages, onLinkBack, onAvatarClick }: {
    messages: Message[];
    onLinkBack?: (messageId: string) => void;
    onAvatarClick?: (address: string, event: MouseEvent) => void;
  } = $props();

  let mediaItems = $derived(
    messages
      .filter((m) => m.media.length > 0)
      .flatMap((m) => m.media.map((a) => ({ message: m, attachment: a })))
  );
</script>

<div class="media-feed">
  {#if mediaItems.length === 0}
    <div class="empty-state">No media yet</div>
  {:else}
    {#each mediaItems as { message, attachment } (attachment.id)}
      <MediaCard {message} {attachment} {onLinkBack} {onAvatarClick} />
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
