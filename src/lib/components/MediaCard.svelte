<script lang="ts">
  import type { Message, MediaAttachment } from '../types';
  import { sanitizeHref } from '../url-sanitize';
  import { formatMessageTimestamp, formatFullTimestamp } from '../time-format';
  import { dayClock } from '../day-clock';
  import { timeFormatPrefs } from '../time-format-service';
  import Avatar from './Avatar.svelte';
  import PeerName from './PeerName.svelte';
  import { resolveAuthorLabel } from '../mention-render';
  import type { ResolvedCard } from '../member-card-service';

  let { message, attachment, onLinkBack, onAvatarClick, resolveNickname, resolveCard }: {
    message: Message;
    attachment: MediaAttachment;
    onLinkBack?: (messageId: string) => void;
    onAvatarClick?: (address: string, event: MouseEvent) => void;
    // ZEB-962: DM senders carry a blank baked name; resolve the author label at
    // render through the same ladder TextMessage uses.
    resolveNickname?: (ownerIdHex: string) => string | undefined;
    resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined;
  } = $props();

  // ZEB-943: format against the app-wide day clock so the label reclassifies at
  // local midnight without a remount (day-clock.ts). No per-message timer.
  let timeStr = $derived(formatMessageTimestamp(message.timestamp, $dayClock, $timeFormatPrefs));
  let senderName = $derived(resolveAuthorLabel(message.sender, resolveNickname, resolveCard));
  let senderLabel = $derived(senderName.label);
</script>

<div class="media-card" id="media-{attachment.id}">
  <button class="card-header" onclick={() => onLinkBack?.(message.id)}>
    <Avatar
      address={message.sender.address}
      displayName={senderLabel}
      avatarUrl={message.sender.avatarUrl}
      size={20}
      onclick={(e) => { e.stopPropagation(); onAvatarClick?.(message.sender.address, e); }}
    />
    <span class="card-sender"><PeerName name={senderName} /></span>
    <time class="card-time" datetime={new Date(message.timestamp).toISOString()} title={formatFullTimestamp(message.timestamp, $timeFormatPrefs)}>{timeStr}</time>
    <span class="link-back-icon" title="Jump to message">&#8599;</span>
  </button>

  <div class="card-content">
    {#if attachment.type === 'image'}
      <img
        src={attachment.url}
        alt={attachment.title ?? 'image'}
        class="card-image"
        loading="lazy"
        referrerpolicy="no-referrer"
      />
      {#if attachment.title}
        <p class="card-caption">{attachment.title}</p>
      {/if}
    {:else if attachment.type === 'link'}
      {@const href = sanitizeHref(attachment.url)}
      {#if href}
        <a {href} class="card-link" target="_blank" rel="noopener noreferrer">
          <div class="link-preview">
            <div class="link-title">{attachment.title ?? attachment.url}</div>
            {#if attachment.domain}
              <div class="link-domain">{attachment.domain}</div>
            {/if}
          </div>
        </a>
      {:else}
        <div class="link-preview">
          <div class="link-title">{attachment.title ?? attachment.url}</div>
          {#if attachment.domain}
            <div class="link-domain">{attachment.domain}</div>
          {/if}
        </div>
      {/if}
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
    background: var(--bg-highlight-faint);
  }

  .card-sender {
    font-weight: 600;
    color: var(--text-primary);
    font-size: 13px;
    /* ZEB-943: date-aware timestamps widen the header — let the sender name
       truncate so the timestamp and jump control never clip on narrow feeds. */
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .card-time {
    color: var(--text-muted);
    font-size: 11px;
    flex-shrink: 0;
    white-space: nowrap;
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
    background: var(--shadow-soft);
    border-radius: 4px;
    overflow: hidden;
  }

  .code-filename {
    padding: 6px 12px;
    font-size: 12px;
    color: var(--text-muted);
    border-bottom: 1px solid var(--border);
    font-family: var(--font-mono);
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
