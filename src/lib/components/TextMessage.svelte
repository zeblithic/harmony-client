<script lang="ts">
  import type { Message } from '../types';
  import Avatar from './Avatar.svelte';

  let { message, collapsed = false, onMediaClick }: {
    message: Message;
    collapsed?: boolean;
    onMediaClick?: (mediaId: string) => void;
  } = $props();

  let timeStr = $derived(
    new Date(message.timestamp).toLocaleTimeString([], {
      hour: '2-digit',
      minute: '2-digit',
    })
  );
</script>

<div class="text-message" class:loud={message.priority === 'loud'} id="msg-{message.id}">
  <Avatar
    address={message.sender.address}
    displayName={message.sender.displayName}
    size={24}
  />
  <div class="message-content">
    <div class="message-header">
      <span class="sender-name">{message.sender.displayName}</span>
      <span class="timestamp">{timeStr}</span>
    </div>
    <div class="message-text">{message.text}</div>
    {#if message.media.length > 0}
      <div class="media-indicators">
        {#each message.media as attachment (attachment.id)}
          {#if collapsed}
            <div class="inline-embed">
              {#if attachment.type === 'image'}
                <img src={attachment.url} alt={attachment.title ?? 'image'} class="inline-image" />
              {:else if attachment.type === 'link'}
                <a href={attachment.url} class="inline-link" target="_blank" rel="noopener">
                  {attachment.title ?? attachment.url}
                </a>
              {:else if attachment.type === 'code'}
                <pre class="inline-code"><code>{attachment.content}</code></pre>
              {/if}
            </div>
          {:else}
            <button
              class="media-pill"
              onclick={() => onMediaClick?.(attachment.id)}
            >
              {#if attachment.type === 'image'}
                <span class="pill-icon">&#128444;</span> {attachment.title ?? 'image'}
              {:else if attachment.type === 'link'}
                <span class="pill-icon">&#128279;</span> {attachment.domain ?? 'link'}
              {:else if attachment.type === 'code'}
                <span class="pill-icon">&lt;/&gt;</span> {attachment.title ?? 'code'}
              {/if}
            </button>
          {/if}
        {/each}
      </div>
    {/if}
  </div>
</div>

<style>
  .text-message {
    display: flex;
    gap: 12px;
    padding: 4px 16px;
    scroll-margin-top: 8px;
  }

  .text-message:hover {
    background: var(--bg-secondary);
  }

  .message-content {
    flex: 1;
    min-width: 0;
  }

  .message-header {
    display: flex;
    align-items: baseline;
    gap: 8px;
  }

  .sender-name {
    font-weight: 600;
    font-size: 14px;
    color: var(--text-primary);
  }

  .timestamp {
    font-size: 11px;
    color: var(--text-muted);
  }

  .message-text {
    color: var(--text-secondary);
    font-size: 14px;
    word-wrap: break-word;
  }

  .media-indicators {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
    margin-top: 4px;
  }

  .media-pill {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    padding: 2px 8px;
    border: none;
    border-radius: 10px;
    background: var(--bg-tertiary);
    color: var(--text-muted);
    font-size: 12px;
    cursor: pointer;
  }

  .media-pill:hover {
    color: var(--accent);
    background: var(--bg-secondary);
  }

  .pill-icon {
    font-size: 12px;
  }

  .inline-embed {
    margin-top: 8px;
  }

  .inline-image {
    max-width: 100%;
    max-height: 300px;
    border-radius: 8px;
  }

  .inline-link {
    color: var(--accent);
    text-decoration: none;
  }

  .inline-link:hover {
    text-decoration: underline;
  }

  .inline-code {
    background: var(--bg-tertiary);
    padding: 8px 12px;
    border-radius: 6px;
    font-size: 13px;
    overflow-x: auto;
    color: var(--text-secondary);
  }

  .text-message.loud {
    border-left: 2px solid var(--accent);
    padding-left: 14px;
  }

  .text-message.loud .sender-name {
    font-weight: 700;
  }
</style>
