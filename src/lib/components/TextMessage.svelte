<script lang="ts">
  import type { Message } from '../types';
  import { TrustService } from '../trust-service';
  import { sanitizeHref } from '../url-sanitize';
  import Avatar from './Avatar.svelte';

  let { message, collapsed = false, onMediaClick, onAvatarClick, trustService, trustVersion = 0, allMessages = [], onScrollToMessage }: {
    message: Message;
    collapsed?: boolean;
    onMediaClick?: (mediaId: string) => void;
    onAvatarClick?: (address: string, event: MouseEvent) => void;
    trustService?: TrustService;
    trustVersion?: number;
    allMessages?: Message[];
    onScrollToMessage?: (messageId: string) => void;
  } = $props();

  let timeStr = $derived(
    new Date(message.timestamp).toLocaleTimeString([], {
      hour: '2-digit',
      minute: '2-digit',
    })
  );

  let parentMessage = $derived(
    message.replyTo ? allMessages.find(m => m.id === message.replyTo) : undefined
  );

  let parentPreview = $derived(
    parentMessage ? parentMessage.text.slice(0, 50) + (parentMessage.text.length > 50 ? '...' : '') : ''
  );

  function isBlocked(attachment: import('../types').MediaAttachment): boolean {
    void trustVersion;
    if (!TrustService.isGated(attachment)) return false;
    if (!trustService) return true;  // fail-closed: no trust service means block
    const level = trustService.resolve(message.sender.address);
    if (level === 'trusted') return false;
    return !trustService.isLoaded(attachment.id);
  }
</script>

<div class="text-message" class:loud={message.priority === 'loud'} id="msg-{message.id}">
  <Avatar
    address={message.sender.address}
    displayName={message.sender.displayName}
    avatarUrl={message.sender.avatarUrl}
    size={24}
    onclick={onAvatarClick ? (e) => onAvatarClick(message.sender.address, e) : undefined}
  />
  <div class="message-content">
    <div class="message-header">
      <span class="sender-name">{message.sender.displayName}</span>
      <span class="timestamp">{timeStr}</span>
    </div>
    {#if parentMessage}
      <button
        class="reply-to-header"
        onclick={() => onScrollToMessage?.(parentMessage.id)}
        aria-label="In reply to {parentMessage.sender.displayName}: {parentPreview}"
      >
        <span class="reply-to-icon">↩</span>
        <span class="reply-to-sender">{parentMessage.sender.displayName}</span>
        <span class="reply-to-text">{parentPreview}</span>
      </button>
    {/if}
    <div class="message-text">{message.text}</div>
    {#if message.media.length > 0}
      <div class="media-indicators">
        {#each message.media as attachment (attachment.id)}
          {#if collapsed}
            {#if isBlocked(attachment)}
              <div class="inline-embed inline-blocked">
                <span class="blocked-inline-icon">&#128274;</span>
                <span class="blocked-inline-label">Blocked {attachment.type}</span>
              </div>
            {:else}
              <div class="inline-embed">
                {#if attachment.type === 'image'}
                  <img src={attachment.url} alt={attachment.title ?? 'image'} class="inline-image" referrerpolicy="no-referrer" />
                {:else if attachment.type === 'link'}
                  {@const href = sanitizeHref(attachment.url)}
                  {#if href}
                    <a {href} class="inline-link" target="_blank" rel="noopener noreferrer">
                      {attachment.title ?? attachment.url}
                    </a>
                  {:else}
                    <span class="inline-link-blocked">{attachment.title ?? attachment.url}</span>
                  {/if}
                {:else if attachment.type === 'code'}
                  <pre class="inline-code"><code>{attachment.content}</code></pre>
                {/if}
              </div>
            {/if}
          {:else}
            {#if isBlocked(attachment)}
              <button
                class="media-pill blocked"
                onclick={() => onMediaClick?.(attachment.id)}
              >
                <span class="pill-icon">&#128274;</span> blocked {attachment.type}
              </button>
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

  .media-pill.blocked {
    color: var(--text-muted);
    font-style: italic;
  }

  .pill-icon {
    font-size: 12px;
  }

  .inline-embed {
    margin-top: 8px;
  }

  .inline-blocked {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 8px 12px;
    background: var(--bg-tertiary);
    border-radius: 6px;
    color: var(--text-muted);
    font-size: 12px;
    font-style: italic;
  }

  .blocked-inline-icon {
    font-size: 14px;
  }

  .blocked-inline-label {
    font-size: 12px;
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

  .inline-link-blocked {
    color: var(--text-muted);
    font-size: 12px;
  }

  .inline-code {
    background: var(--bg-tertiary);
    padding: 8px 12px;
    border-radius: 6px;
    font-size: 13px;
    overflow-x: auto;
    color: var(--text-secondary);
  }

  .reply-to-header {
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 2px 0;
    margin-bottom: 2px;
    border: none;
    background: none;
    color: var(--text-muted);
    font-size: 11px;
    cursor: pointer;
    text-align: left;
  }

  .reply-to-header:hover {
    color: var(--accent);
  }

  .reply-to-icon {
    font-size: 12px;
  }

  .reply-to-sender {
    font-weight: 600;
  }

  .reply-to-text {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 200px;
  }

  .text-message.loud {
    border-left: 2px solid var(--accent);
    padding-left: 14px;
  }

  .text-message.loud .sender-name {
    font-weight: 700;
  }
</style>
