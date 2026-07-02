<script lang="ts">
  import { untrack } from 'svelte';

  let {
    replyTo = null,
    initialTo = '',
    initialSubject = '',
    onSend,
    onCancel,
  }: {
    replyTo: string | null;
    initialTo?: string;
    initialSubject?: string;
    onSend?: (to: string[], subject: string, body: string, replyTo: string | null) => void | Promise<void>;
    onCancel?: () => void;
  } = $props();

  // Compose fields are one-shot seeded from props; user edits them locally.
  let toField = $state(untrack(() => initialTo));
  let subject = $state(untrack(() => initialSubject));
  let body = $state('');
  let sending = $state(false);
  let error = $state('');

  async function handleSend() {
    error = '';
    const recipients = toField
      .split(',')
      .map(s => s.trim())
      .filter(s => s.length > 0);

    if (recipients.length === 0) {
      error = 'At least one recipient required';
      return;
    }

    // Validate hex addresses (32 hex chars = 16 bytes)
    for (const addr of recipients) {
      if (!/^[0-9a-fA-F]{32}$/.test(addr)) {
        error = `Invalid address: ${addr} (expected 32 hex characters)`;
        return;
      }
    }

    if (!subject && !body) {
      error = 'Subject or body required';
      return;
    }

    sending = true;
    try {
      await onSend?.(recipients, subject, body, replyTo);
      toField = '';
      subject = '';
      body = '';
    } catch (err) {
      error = err instanceof Error ? err.message : 'Send failed';
    } finally {
      sending = false;
    }
  }
</script>

<div class="mail-compose">
  <div class="compose-header">
    <h3>New Message</h3>
    <button type="button" class="cancel-btn" onclick={() => onCancel?.()}>
      &times;
    </button>
  </div>

  <div class="compose-form">
    <div class="field">
      <label for="to-field">To</label>
      <input
        id="to-field"
        type="text"
        placeholder="Hex address (comma-separated for multiple)"
        bind:value={toField}
      />
    </div>

    <div class="field">
      <label for="subject-field">Subject</label>
      <input
        id="subject-field"
        type="text"
        placeholder="Subject"
        bind:value={subject}
      />
    </div>

    <div class="field body-field">
      <label for="body-field">Body</label>
      <textarea
        id="body-field"
        placeholder="Write your message... (plain text or markdown)"
        bind:value={body}
      ></textarea>
    </div>

    {#if error}
      <div class="error-msg">{error}</div>
    {/if}

    <div class="compose-actions">
      <button type="button" class="send-btn" onclick={handleSend} disabled={sending}>
        {sending ? 'Sending...' : 'Send'}
      </button>
    </div>
  </div>
</div>

<style>
  .mail-compose {
    display: flex;
    flex-direction: column;
    height: 100%;
    padding: 1rem;
  }

  .compose-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 1rem;
  }

  .compose-header h3 {
    margin: 0;
    font-size: 1rem;
    color: var(--text-primary);
  }

  .cancel-btn {
    border: none;
    background: transparent;
    color: var(--text-secondary);
    cursor: pointer;
    font-size: 1.25rem;
    padding: 0.25rem;
  }

  .compose-form {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
    flex: 1;
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
  }

  .field label {
    font-size: 0.75rem;
    color: var(--text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }

  .field input, .field textarea {
    padding: 0.5rem 0.625rem;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--surface, #1a1a1a);
    color: var(--text-primary);
    font-size: 0.875rem;
    font-family: inherit;
  }

  .field input:focus, .field textarea:focus {
    outline: none;
    border-color: var(--accent);
  }

  .body-field {
    flex: 1;
  }

  .body-field textarea {
    flex: 1;
    min-height: 12rem;
    resize: none;
  }

  .error-msg {
    color: var(--error, #e55);
    font-size: 0.8125rem;
  }

  .compose-actions {
    display: flex;
    justify-content: flex-end;
    padding-top: 0.5rem;
  }

  .send-btn {
    padding: 0.5rem 1.5rem;
    border: none;
    border-radius: 4px;
    background: var(--accent);
    color: white;
    cursor: pointer;
    font-size: 0.875rem;
  }

  .send-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
</style>
