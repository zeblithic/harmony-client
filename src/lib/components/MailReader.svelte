<script lang="ts">
  import type { MailMessageDetail } from '../types';

  let {
    message = null,
    onReply,
    onBack,
  }: {
    message: MailMessageDetail | null;
    onReply?: (messageCid: string, messageId: string) => void;
    onBack?: () => void;
  } = $props();

  function formatDate(timestamp: number): string {
    return new Date(timestamp * 1000).toLocaleString([], {
      year: 'numeric',
      month: 'short',
      day: 'numeric',
      hour: '2-digit',
      minute: '2-digit',
    });
  }

  function shortAddr(addr: string): string {
    if (addr.length <= 12) return addr;
    return addr.slice(0, 8) + '...' + addr.slice(-4);
  }

  function formatSize(bytes: number): string {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  }
</script>

<div class="mail-reader">
  {#if !message}
    <div class="empty-state">Select a message to read</div>
  {:else}
    <div class="reader-toolbar">
      <button type="button" class="back-btn" onclick={() => onBack?.()}>
        &larr; Back
      </button>
      <button
        type="button"
        class="reply-btn"
        onclick={() => onReply?.(message.messageCid, message.messageId)}
      >
        Reply
      </button>
    </div>

    <div class="reader-header">
      <h2 class="subject">{message.subject || '(no subject)'}</h2>
      <div class="meta">
        <span class="from">From: <code>{shortAddr(message.senderAddress)}</code></span>
        <span class="date">{formatDate(message.timestamp)}</span>
      </div>
      {#if message.recipients.length > 0}
        <div class="recipients">
          To: {message.recipients.map(r => shortAddr(r.address)).join(', ')}
        </div>
      {/if}
    </div>

    <div class="reader-body">
      <pre class="body-text">{message.body}</pre>
    </div>

    {#if message.attachments.length > 0}
      <div class="attachments">
        <h4>Attachments ({message.attachments.length})</h4>
        {#each message.attachments as att}
          <div class="attachment-item">
            <span class="att-name">{att.filename}</span>
            <span class="att-size">{formatSize(att.size)}</span>
          </div>
        {/each}
      </div>
    {/if}
  {/if}
</div>

<style>
  .mail-reader {
    display: flex;
    flex-direction: column;
    height: 100%;
    overflow-y: auto;
    padding: 1rem;
  }

  .empty-state {
    display: flex;
    align-items: center;
    justify-content: center;
    height: 100%;
    color: var(--text-secondary, #888);
  }

  .reader-toolbar {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    margin-bottom: 1rem;
    flex-shrink: 0;
  }

  .back-btn, .reply-btn {
    padding: 0.375rem 0.75rem;
    border: 1px solid var(--border, #333);
    border-radius: 4px;
    background: transparent;
    color: var(--text-primary, #eee);
    cursor: pointer;
    font-size: 0.8125rem;
  }

  .reply-btn {
    background: var(--accent, #5b8def);
    border-color: var(--accent, #5b8def);
    color: white;
  }

  .reader-header {
    margin-bottom: 1rem;
    padding-bottom: 0.75rem;
    border-bottom: 1px solid var(--border, #333);
  }

  .subject {
    margin: 0 0 0.5rem;
    font-size: 1.125rem;
    color: var(--text-primary, #eee);
  }

  .meta {
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-size: 0.8125rem;
    color: var(--text-secondary, #888);
  }

  .from code {
    font-family: monospace;
    font-size: 0.75rem;
    color: var(--text-primary, #ccc);
  }

  .recipients {
    margin-top: 0.25rem;
    font-size: 0.75rem;
    color: var(--text-secondary, #888);
  }

  .reader-body {
    flex: 1;
  }

  .body-text {
    white-space: pre-wrap;
    word-break: break-word;
    font-family: inherit;
    font-size: 0.875rem;
    line-height: 1.5;
    color: var(--text-primary, #ddd);
    margin: 0;
  }

  .attachments {
    margin-top: 1rem;
    padding-top: 0.75rem;
    border-top: 1px solid var(--border, #333);
  }

  .attachments h4 {
    margin: 0 0 0.5rem;
    font-size: 0.8125rem;
    color: var(--text-secondary, #888);
  }

  .attachment-item {
    display: flex;
    justify-content: space-between;
    padding: 0.25rem 0;
    font-size: 0.8125rem;
  }

  .att-name {
    color: var(--accent, #5b8def);
  }

  .att-size {
    color: var(--text-secondary, #888);
  }
</style>
