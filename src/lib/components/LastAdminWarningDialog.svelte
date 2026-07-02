<script lang="ts">
  import Modal from './Modal.svelte';

  let {
    open = $bindable(),
    action,
    communityName,
    onConfirm,
    onCancel,
  }: {
    open: boolean;
    action: 'demote' | 'leave';
    communityName: string;
    onConfirm: () => Promise<void>;
    onCancel: () => void;
  } = $props();

  let typedToken = $state('');
  let submitting = $state(false);
  let submitError = $state<string | null>(null);

  const titleId = `last-admin-warning-title-${Math.random().toString(36).slice(2)}`;

  let requiredToken = $derived(action === 'demote' ? 'DEMOTE' : 'LEAVE');
  let canProceed = $derived(typedToken === requiredToken && !submitting);

  async function handleConfirm() {
    if (!canProceed) return;
    submitting = true;
    submitError = null;
    try {
      await onConfirm();
      typedToken = '';
      open = false;
    } catch (e) {
      submitError = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
    }
  }

  function handleCancel() {
    if (submitting) return;
    typedToken = '';
    submitError = null;
    open = false;
    onCancel();
  }
</script>

{#if open}
  <Modal canCancel={!submitting} ariaLabelledby={titleId} onCancel={handleCancel}>
    <h2 class="dialog-title" id={titleId}>
      &#9888; You are the last admin of "{communityName}"
    </h2>

    <p class="warning-body">
      After this action, the community will be locked: no one will be able to issue moderation
      actions, including restoring admin tier. Recovery is possible by forking the community
      (coming soon &mdash; <a
        href="https://linear.app/zeblith/issue/ZEB-285"
        target="_blank"
        rel="noreferrer"
      >ZEB-285</a>).
    </p>

    <label class="token-label" for="last-admin-token-input">
      To proceed, type <code class="token-code">{requiredToken}</code> below:
    </label>
    <input
      id="last-admin-token-input"
      type="text"
      class="token-input"
      bind:value={typedToken}
      autocomplete="off"
      autocapitalize="off"
      spellcheck={false}
      disabled={submitting}
      aria-describedby="last-admin-token-help"
    />
    <p id="last-admin-token-help" class="hint">Case-sensitive. Exact match required.</p>

    {#if submitError}
      <p class="error" role="alert">{submitError}</p>
    {/if}

    <div class="actions-row">
      <button
        type="button"
        class="cancel-btn"
        onclick={handleCancel}
        disabled={submitting}
      >Cancel</button>
      <span class="spacer"></span>
      <button
        type="button"
        class="proceed-btn"
        onclick={handleConfirm}
        disabled={!canProceed}
      >
        {#if submitting}
          <span class="spinner" aria-hidden="true"></span>
          Proceeding...
        {:else}
          Proceed
        {/if}
      </button>
    </div>
  </Modal>
{/if}

<style>
  .dialog-title {
    color: var(--text-primary);
    font-size: 1rem;
    margin: 0 0 12px;
    font-weight: 600;
  }
  .warning-body {
    font-size: 0.85rem;
    color: var(--text-secondary);
    margin: 0 0 16px;
    line-height: 1.5;
  }
  .warning-body a {
    color: var(--accent);
    text-decoration: underline;
  }
  .warning-body a:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
    border-radius: 2px;
  }
  .token-label {
    display: block;
    font-size: 0.8rem;
    color: var(--text-secondary);
    margin-bottom: 6px;
  }
  .token-code {
    font-family: monospace;
    background: var(--bg-tertiary);
    padding: 1px 5px;
    border-radius: 3px;
    color: var(--text-primary);
    font-size: 0.85em;
  }
  .token-input {
    width: 100%;
    padding: 8px 10px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-primary);
    font-size: 0.875rem;
    font-family: monospace;
    box-sizing: border-box;
  }
  .token-input:focus {
    outline: 2px solid var(--accent);
    outline-offset: -1px;
  }
  .token-input:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .hint {
    font-size: 0.7rem;
    color: var(--text-secondary);
    margin: 4px 0 0;
  }
  .error {
    color: var(--danger-text-muted);
    font-size: 0.8rem;
    margin: 8px 0 0;
  }
  .actions-row {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 20px;
  }
  .spacer {
    flex: 1;
  }
  .cancel-btn,
  .proceed-btn {
    border: none;
    padding: 8px 16px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.875rem;
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .cancel-btn {
    background: var(--bg-tertiary);
    color: var(--text-secondary);
  }
  .cancel-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .proceed-btn {
    background: var(--danger-muted);
    color: var(--text-bright);
  }
  .proceed-btn:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }
  .cancel-btn:focus-visible,
  .proceed-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .spinner {
    display: inline-block;
    width: 12px;
    height: 12px;
    border: 2px solid var(--border-bright);
    border-top-color: var(--text-bright);
    border-radius: 50%;
    animation: spin 0.6s linear infinite;
    flex-shrink: 0;
  }
  @keyframes spin {
    to { transform: rotate(360deg); }
  }
</style>
