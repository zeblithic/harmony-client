<script lang="ts">
  import type { MailMessage } from '../types';

  let {
    message = null,
    loading = false,
  }: {
    message: MailMessage | null;
    loading?: boolean;
  } = $props();

  function formatTimestamp(timestamp: number): string {
    const date = new Date(timestamp * 1000);
    return date.toLocaleString([], {
      year: 'numeric',
      month: 'short',
      day: 'numeric',
      hour: '2-digit',
      minute: '2-digit',
    });
  }

  function truncateAddress(addr: string): string {
    if (addr.length <= 12) return addr;
    return addr.slice(0, 6) + '...' + addr.slice(-4);
  }
</script>

<div class="mail-detail">
  {#if loading}
    <div class="empty-state">
      <p>Loading...</p>
    </div>
  {:else if message}
    <header class="message-header">
      <h2 class="subject">{message.subject || '(no subject)'}</h2>
      <div class="meta">
        <span class="from">From: {truncateAddress(message.senderAddress)}</span>
        <span class="date">{formatTimestamp(message.timestamp)}</span>
      </div>
      {#if message.recipients.length > 0}
        <div class="recipients">
          To: {message.recipients.map(truncateAddress).join(', ')}
        </div>
      {/if}
      <div class="badges">
        {#if message.isReply}
          <span class="badge reply">Reply</span>
        {/if}
        {#if message.hasAttachments}
          <span class="badge attachment">Attachments</span>
        {/if}
      </div>
    </header>
    <div class="message-body">
      <pre class="body-text">{message.body}</pre>
    </div>
  {:else}
    <div class="empty-state">
      <p>Select a message to read</p>
    </div>
  {/if}
</div>

<style>
  .mail-detail {
    display: flex;
    flex-direction: column;
    height: 100%;
    overflow: hidden;
  }

  .empty-state {
    display: flex;
    align-items: center;
    justify-content: center;
    height: 100%;
    color: var(--text-muted, #949ba4);
  }

  .message-header {
    padding: 16px 20px;
    border-bottom: 1px solid var(--border, #2a2d31);
    flex-shrink: 0;
  }

  .subject {
    font-size: 18px;
    font-weight: 600;
    color: var(--text-primary, #dbdee1);
    margin: 0 0 8px 0;
  }

  .meta {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    gap: 12px;
    margin-bottom: 4px;
  }

  .from {
    font-size: 13px;
    color: var(--text-secondary, #b5bac1);
  }

  .date {
    font-size: 12px;
    color: var(--text-muted, #949ba4);
    flex-shrink: 0;
  }

  .recipients {
    font-size: 12px;
    color: var(--text-muted, #949ba4);
    margin-bottom: 4px;
  }

  .badges {
    display: flex;
    gap: 6px;
    margin-top: 6px;
  }

  .badge {
    font-size: 11px;
    padding: 2px 6px;
    border-radius: 3px;
    color: var(--text-primary, #dbdee1);
  }

  .badge.reply {
    background: rgba(88, 101, 242, 0.2);
  }

  .badge.attachment {
    background: rgba(87, 242, 135, 0.2);
  }

  .message-body {
    flex: 1;
    overflow-y: auto;
    padding: 16px 20px;
  }

  .body-text {
    font-family: inherit;
    font-size: 14px;
    line-height: 1.6;
    color: var(--text-primary, #dbdee1);
    white-space: pre-wrap;
    word-wrap: break-word;
    margin: 0;
  }
</style>
