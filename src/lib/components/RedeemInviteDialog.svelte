<script lang="ts">
  import { untrack } from 'svelte';
  import Modal from './Modal.svelte';
  import { mapRedeemInviteError } from '../redeem-invite-errors';

  let {
    onSubmit,
    onCancel,
    error = null,
    pending = false,
    initialUrl = '',
  }: {
    onSubmit: (url: string) => void;
    onCancel: () => void;
    error?: string | null;
    pending?: boolean;
    initialUrl?: string;
  } = $props();

  let url = $state(untrack(() => initialUrl));
  let canSubmit = $derived(url.trim().startsWith('harmony://invite/') && !pending);
  let mapped = $derived(error ? mapRedeemInviteError(error) : null);
  const titleId = `redeem-invite-title-${Math.random().toString(36).slice(2)}`;

  function handleSubmit() {
    if (!canSubmit) return;
    onSubmit(url.trim());
  }
</script>

<Modal {onCancel} canCancel={!pending} ariaLabelledby={titleId}>
  <h3 class="dialog-title" id={titleId}>Redeem invite link</h3>

  {#if mapped}
    <div class="error-banner">
      <p class="summary">{mapped.summary}</p>
      {#if mapped.hint}<p class="hint">{mapped.hint}</p>{/if}
      <details>
        <summary>Show details</summary>
        <div class="diagnostic">
          <div>Telemetry tag: <code>{mapped.tag}</code></div>
          <div>Raw error: <code>{mapped.raw}</code></div>
        </div>
      </details>
    </div>
  {/if}

  <textarea
    placeholder="harmony://invite/v1?..."
    bind:value={url}
    class="url-input"
    rows="3"
    disabled={pending}
  ></textarea>

  {#if pending}
    <div class="pending-row">
      <div class="spinner" role="status" aria-label="Verifying invite"></div>
      <span>Verifying invite...</span>
    </div>
  {/if}

  <div class="dialog-actions">
    <button class="cancel-btn" onclick={onCancel} disabled={pending}>Cancel</button>
    <button class="confirm-btn" onclick={handleSubmit} disabled={!canSubmit}>Redeem</button>
  </div>
</Modal>

<style>
  .dialog-title {
    color: var(--text-primary);
    font-size: 1.1rem;
    margin: 0 0 16px;
  }
  .url-input {
    width: 100%;
    padding: 8px 12px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-primary);
    font-family: monospace;
    font-size: 0.75rem;
    margin-bottom: 12px;
    box-sizing: border-box;
    resize: vertical;
  }
  .url-input:focus {
    outline: 2px solid var(--accent);
    outline-offset: -1px;
  }
  .error-banner {
    background: var(--bg-tertiary);
    border: 1px solid #d83c3e;
    padding: 10px 12px;
    border-radius: 4px;
    margin-bottom: 12px;
  }
  .error-banner .summary {
    margin: 0 0 4px 0;
    color: #d83c3e;
    font-size: 0.85rem;
  }
  .error-banner .hint {
    margin: 0 0 8px 0;
    color: var(--text-secondary);
    font-size: 0.8rem;
  }
  .error-banner details {
    font-size: 0.75rem;
    color: var(--text-secondary);
  }
  .error-banner details summary {
    cursor: pointer;
  }
  .diagnostic {
    padding: 8px 0 0 0;
    font-family: monospace;
  }
  .diagnostic code {
    color: var(--text-primary);
  }
  .pending-row {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 12px;
    color: var(--text-secondary);
    font-size: 0.8rem;
  }
  .spinner {
    width: 12px;
    height: 12px;
    border: 2px solid var(--accent);
    border-top-color: transparent;
    border-radius: 50%;
    animation: spin 0.8s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
  .dialog-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }
  .cancel-btn {
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    border: none;
    padding: 8px 16px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.875rem;
  }
  .confirm-btn {
    background: var(--accent);
    color: var(--text-primary);
    border: none;
    padding: 8px 16px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.875rem;
  }
  .confirm-btn:disabled,
  .cancel-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .cancel-btn:focus-visible,
  .confirm-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }
</style>
