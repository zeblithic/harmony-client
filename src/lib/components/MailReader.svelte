<script lang="ts">
  import type { MailMessageDetail } from '../types';
  // ZEB-946: the header time honors the owner's clock preference; the
  // word-month date stays locale-default (readable + order-unambiguous).
  import { formatClockTime, type TimeFormatPrefs } from '../time-format';
  import { timeFormatPrefs } from '../time-format-service';

  let {
    message = null,
    loading = false,
    error = null,
    onReply,
    onBack,
  }: {
    message: MailMessageDetail | null;
    loading?: boolean;
    error?: string | null;
    onReply?: (messageCid: string, messageId: string) => void;
    onBack?: () => void;
  } = $props();

  function formatDate(timestamp: number, prefs: TimeFormatPrefs): string {
    const ms = timestamp * 1000;
    const date = new Date(ms).toLocaleDateString([], {
      year: 'numeric',
      month: 'short',
      day: 'numeric',
    });
    return `${date}, ${formatClockTime(ms, prefs)}`;
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
  {#if loading}
    <div class="reader-loading">
      <button type="button" class="back-btn" onclick={() => onBack?.()}>
        &larr; Back
      </button>
      <span class="reader-spinner" aria-label="Loading">⟳</span>
      <span>Loading message…</span>
    </div>
  {:else if error}
    <div class="reader-error">
      <p>Failed to load message:</p>
      <pre class="error-msg">{error}</pre>
      <button type="button" class="back-btn" onclick={() => onBack?.()}>
        &larr; Back
      </button>
    </div>
  {:else if !message}
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
        <span class="date">{formatDate(message.timestamp, $timeFormatPrefs)}</span>
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
    padding: 16px;
  }

  .empty-state {
    display: flex;
    align-items: center;
    justify-content: center;
    height: 100%;
    color: var(--text-secondary);
  }

  .reader-toolbar {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 16px;
    flex-shrink: 0;
  }

  .back-btn, .reply-btn {
    padding: 6px 12px;
    border: 1px solid var(--border);
    border-radius: 5px;
    background: transparent;
    color: var(--text-primary);
    cursor: pointer;
    font-size: 0.8125rem;
  }

  .reply-btn {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--on-accent);
  }

  .reader-header {
    margin-bottom: 16px;
    padding-bottom: 12px;
    border-bottom: 1px solid var(--border);
  }

  .subject {
    margin: 0 0 8px;
    font-family: var(--font-display);
    font-size: 1.125rem;
    color: var(--text-primary);
  }

  .meta {
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-size: 0.8125rem;
    color: var(--text-secondary);
  }

  .from code {
    font-family: var(--font-mono);
    font-size: 0.75rem;
    color: var(--text-primary);
  }

  .recipients {
    margin-top: 4px;
    font-size: 0.75rem;
    color: var(--text-secondary);
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
    color: var(--text-primary);
    margin: 0;
  }

  .attachments {
    margin-top: 16px;
    padding-top: 12px;
    border-top: 1px solid var(--border);
  }

  .attachments h4 {
    margin: 0 0 8px;
    font-size: 0.8125rem;
    color: var(--text-secondary);
  }

  .attachment-item {
    display: flex;
    justify-content: space-between;
    padding: 4px 0;
    font-size: 0.8125rem;
  }

  .att-name {
    color: var(--accent);
  }

  .att-size {
    color: var(--text-secondary);
  }

  .reader-loading,
  .reader-error {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    height: 100%;
    gap: 8px;
    padding: 16px;
    color: var(--text-secondary);
  }

  .reader-spinner {
    display: inline-block;
    animation: reader-spin 1.5s linear infinite;
    font-size: 1.25rem;
  }
  @keyframes reader-spin {
    from { transform: rotate(0deg); }
    to { transform: rotate(360deg); }
  }

  .reader-error .error-msg {
    background: color-mix(in srgb, var(--mail-danger) 8%, transparent);
    border: 1px solid color-mix(in srgb, var(--mail-danger) 30%, transparent);
    border-radius: 5px;
    padding: 8px;
    font-size: 0.8125rem;
    color: var(--mail-error-text);
    white-space: pre-wrap;
    word-break: break-word;
    max-width: 40rem;
  }
</style>
