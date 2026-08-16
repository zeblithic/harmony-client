<script lang="ts">
  import { onDestroy } from 'svelte';
  import type { Message, MediaAttachment } from '../types';
  import { formatMessageTimestamp, formatFullTimestamp } from '../time-format';
  import { dayClock } from '../day-clock';
  import { timeFormatPrefs } from '../time-format-service';
  import Avatar from './Avatar.svelte';

  let { message, attachment, onLinkBack, onAvatarClick, onLoad }: {
    message: Message;
    attachment: MediaAttachment;
    onLinkBack?: (messageId: string) => void;
    onAvatarClick?: (address: string, event: MouseEvent) => void;
    onLoad?: (attachmentId: string) => void;
  } = $props();

  // ZEB-943: format against the app-wide day clock so the label reclassifies at
  // local midnight without a remount (day-clock.ts). No per-message timer.
  let timeStr = $derived(formatMessageTimestamp(message.timestamp, $dayClock, $timeFormatPrefs));

  type LoadState = 'blocked' | 'confirming' | 'cooldown';
  let loadState = $state<LoadState>('blocked');
  let cooldownTimer: ReturnType<typeof setTimeout> | null = null;

  function handleShow() {
    loadState = 'cooldown';
    cooldownTimer = setTimeout(() => {
      loadState = 'confirming';
    }, 1000);
  }

  function handleConfirm() {
    if (loadState !== 'confirming') return;
    onLoad?.(attachment.id);
  }

  function handleCancel() {
    loadState = 'blocked';
    if (cooldownTimer) {
      clearTimeout(cooldownTimer);
      cooldownTimer = null;
    }
  }

  onDestroy(() => {
    if (cooldownTimer) {
      clearTimeout(cooldownTimer);
    }
  });

  const TYPE_LABELS: Record<string, string> = {
    image: 'image',
    link: 'link',
  };
</script>

<div
  class="untrusted-card"
  id="media-{attachment.id}"
  aria-label="Blocked media, {TYPE_LABELS[attachment.type] ?? attachment.type}, from {message.sender.displayName}"
>
  <button class="card-header" onclick={() => onLinkBack?.(message.id)}>
    <Avatar
      address={message.sender.address}
      displayName={message.sender.displayName}
      avatarUrl={message.sender.avatarUrl}
      size={20}
      onclick={(e) => { e.stopPropagation(); onAvatarClick?.(message.sender.address, e); }}
    />
    <span class="card-sender">{message.sender.displayName}</span>
    <time class="card-time" datetime={new Date(message.timestamp).toISOString()} title={formatFullTimestamp(message.timestamp, $timeFormatPrefs)}>{timeStr}</time>
    <span class="link-back-icon" title="Jump to message">&#8599;</span>
  </button>

  <div class="card-body">
    <span class="lock-icon">&#128274;</span>
    <span class="blocked-label">Blocked media &mdash; {TYPE_LABELS[attachment.type] ?? attachment.type}</span>
  </div>

  <div class="card-actions">
    {#if loadState === 'blocked'}
      <button class="action-btn" onclick={handleShow}>Show</button>
    {:else if loadState === 'cooldown'}
      <button class="action-btn confirming" disabled aria-disabled="true">Confirm load</button>
      <button class="cancel-btn" onclick={handleCancel}>Cancel</button>
    {:else if loadState === 'confirming'}
      <button class="action-btn confirming" onclick={handleConfirm}>Confirm load</button>
      <button class="cancel-btn" onclick={handleCancel}>Cancel</button>
    {/if}
    <span class="sr-only" aria-live="polite">{loadState === 'confirming' ? 'Confirm load is now available' : ''}</span>
  </div>
</div>

<style>
  .untrusted-card {
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-left: 3px solid var(--gov-clay);
    border-radius: 8px;
    box-shadow: var(--shadow-e1);
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

  .card-body {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 12px;
    color: var(--text-muted);
    font-size: 13px;
  }

  .lock-icon {
    font-size: 16px;
  }

  .blocked-label {
    font-style: italic;
  }

  .card-actions {
    display: flex;
    gap: 8px;
    padding: 0 12px 12px;
  }

  .action-btn {
    padding: 6px 16px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--bg-secondary);
    color: var(--text-primary);
    font-size: 12px;
    cursor: pointer;
  }

  .action-btn:hover:not(:disabled) {
    background: var(--bg-primary);
  }

  .action-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .action-btn.confirming:not(:disabled) {
    border-color: var(--gov-clay);
    color: var(--gov-clay);
  }

  .cancel-btn {
    padding: 6px 12px;
    border: none;
    border-radius: 4px;
    background: none;
    color: var(--text-muted);
    font-size: 12px;
    cursor: pointer;
  }

  .cancel-btn:hover {
    color: var(--text-primary);
  }

  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0, 0, 0, 0);
    white-space: nowrap;
    border: 0;
  }
</style>
