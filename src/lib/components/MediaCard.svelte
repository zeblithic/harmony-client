<script lang="ts">
  import type { Message, MediaAttachment } from '../types';
  import Avatar from './Avatar.svelte';

  let { message, attachment, onLinkBack }: {
    message: Message;
    attachment: MediaAttachment;
    onLinkBack?: (messageId: string) => void;
  } = $props();

  let timeStr = $derived(
    new Date(message.timestamp).toLocaleTimeString([], {
      hour: '2-digit',
      minute: '2-digit',
    })
  );
</script>

<div class="media-card" id="media-{attachment.id}">
  <button class="card-header" onclick={() => onLinkBack?.(message.id)}>
    <Avatar
      address={message.sender.address}
      displayName={message.sender.displayName}
      size={20}
    />
    <span class="card-sender">{message.sender.displayName}</span>
    <span class="card-time">{timeStr}</span>
    <span class="link-back-icon" title="Jump to message">&#8599;</span>
  </button>

  <div class="card-content">
    {#if attachment.type === 'image'}
      <img
        src={attachment.url}
        alt={attachment.title ?? 'image'}
        class="card-image"
        loading="lazy"
      />
      {#if attachment.title}
        <p class="card-caption">{attachment.title}</p>
      {/if}
    {:else if attachment.type === 'link'}
      <a href={attachment.url} class="card-link" target="_blank" rel="noopener">
        <div class="link-preview">
          <div class="link-title">{attachment.title ?? attachment.url}</div>
          {#if attachment.domain}
            <div class="link-domain">{attachment.domain}</div>
          {/if}
        </div>
      </a>
    {:else if attachment.type === 'code'}
      <div class="code-block">
        {#if attachment.title}
          <div class="code-filename">{attachment.title}</div>
        {/if}
        <pre><code>{attachment.content}</code></pre>
      </div>
    {/if}
  </div>
</div>

<style>
  .media-card {
    background: var(--bg-tertiary);
    border-radius: 8px;
    overflow: hidden;
    scroll-margin-top: 12px;
  }

  .card-header {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 8px 12px;
    border: none;
    background: none;
    color: var(--text-secondary);
    font-size: 13px;
    cursor: pointer;
    width: 100%;
    text-align: left;
  }

  .card-header:hover {
    background: rgba(255, 255, 255, 0.03);
  }

  .card-sender {
    font-weight: 600;
    color: var(--text-primary);
    font-size: 13px;
  }

  .card-time {
    color: var(--text-muted);
    font-size: 11px;
  }

  .link-back-icon {
    margin-left: auto;
    font-size: 14px;
    color: var(--text-muted);
    opacity: 0;
    transition: opacity 0.15s;
  }

  .card-header:hover .link-back-icon {
    opacity: 1;
  }

  .card-content {
    padding: 0 12px 12px;
  }

  .card-image {
    width: 100%;
    border-radius: 4px;
    display: block;
  }

  .card-caption {
    margin-top: 6px;
    font-size: 12px;
    color: var(--text-muted);
  }

  .card-link {
    display: block;
    text-decoration: none;
    color: inherit;
  }

  .link-preview {
    border-left: 3px solid var(--accent);
    padding: 8px 12px;
    border-radius: 0 4px 4px 0;
    background: rgba(0, 0, 0, 0.15);
  }

  .link-title {
    color: var(--accent);
    font-size: 14px;
    font-weight: 500;
  }

  .link-domain {
    color: var(--text-muted);
    font-size: 12px;
    margin-top: 2px;
  }

  .card-link:hover .link-title {
    text-decoration: underline;
  }

  .code-block {
    background: rgba(0, 0, 0, 0.2);
    border-radius: 4px;
    overflow: hidden;
  }

  .code-filename {
    padding: 6px 12px;
    font-size: 12px;
    color: var(--text-muted);
    border-bottom: 1px solid var(--border);
    font-family: monospace;
  }

  .code-block pre {
    padding: 12px;
    margin: 0;
    font-size: 13px;
    line-height: 1.5;
    overflow-x: auto;
    color: var(--text-secondary);
  }
</style>
