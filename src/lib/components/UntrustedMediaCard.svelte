<script lang="ts">
  import { onDestroy } from 'svelte';
  import type { Message, MediaAttachment } from '../types';
  import Avatar from './Avatar.svelte';

  let { message, attachment, onLinkBack, onAvatarClick, onLoad }: {
    message: Message;
    attachment: MediaAttachment;
    onLinkBack?: (messageId: string) => void;
    onAvatarClick?: (address: string, event: MouseEvent) => void;
    onLoad?: (attachmentId: string) => void;
  } = $props();

  let timeStr = $derived(
    new Date(message.timestamp).toLocaleTimeString([], {
      hour: '2-digit',
      minute: '2-digit',
    })
  );

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
  <div class="card-header" role="button" tabindex="0" onclick={() => onLinkBack?.(message.id)} onkeydown={(e) => { if ((e.key === 'Enter' || e.key === ' ') && !(e.target as HTMLElement).closest('.avatar')) { e.preventDefault(); onLinkBack?.(message.id); } }}>
    <Avatar
      address={message.sender.address}
      displayName={message.sender.displayName}
      avatarUrl={message.sender.avatarUrl}
      size={20}
      onclick={(e) => { e.stopPropagation(); onAvatarClick?.(message.sender.address, e); }}
    />
    <span class="card-sender">{message.sender.displayName}</span>
    <span class="card-time">{timeStr}</span>
    <span class="link-back-icon" title="Jump to message">&#8599;</span>
  </div>

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
      <button class="action-btn confirming" onclick={handleConfirm} aria-live="polite">Confirm load</button>
      <button class="cancel-btn" onclick={handleCancel}>Cancel</button>
    {/if}
  </div>
</div>

<style>
  .untrusted-card {
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
    border-color: var(--accent);
    color: var(--accent);
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
</style>
