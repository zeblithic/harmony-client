<script lang="ts">
  import Modal from './Modal.svelte';

  let {
    open = $bindable(),
    action,
    targetName,
    communityName,
    onConfirm,
    onCancel,
  }: {
    open: boolean;
    action: 'kick' | 'unban';
    targetName: string;
    communityName: string;
    onConfirm: (reason: string | null) => Promise<void>;
    onCancel: () => void;
  } = $props();

  let reason = $state('');
  let submitting = $state(false);
  let submitError = $state<string | null>(null);

  const titleId = `moderation-dialog-title-${Math.random().toString(36).slice(2)}`;
  let actionLabel = $derived(action === 'kick' ? 'Kick' : 'Unban');
  // Progressive form: "Unban" → "Unbanning" (double-n), not the naive
  // `${actionLabel}ing` which renders as "Unbaning".
  let actionLabelProgressive = $derived(action === 'kick' ? 'Kicking' : 'Unbanning');

  async function handleConfirm() {
    // Guard against double-click / rapid re-trigger: if a submission is
    // already in-flight, the button's `disabled={submitting}` re-render may
    // not have propagated yet when a second click event fires. Without this
    // early-return, two `onConfirm` calls could race and duplicate the
    // kick/unban IPC. `LastAdminWarningDialog` gets the same protection via
    // its `canProceed && !submitting` derived gate.
    if (submitting) return;
    submitting = true;
    submitError = null;
    try {
      await onConfirm(reason.trim() || null);
      reason = '';
      open = false;
    } catch (e) {
      submitError = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
    }
  }

  function handleCancel() {
    if (submitting) return;
    reason = '';
    submitError = null;
    open = false;
    onCancel();
  }
</script>

{#if open}
  <Modal canCancel={!submitting} ariaLabelledby={titleId} onCancel={handleCancel}>
    <h2 class="dialog-title" id={titleId}>
      {actionLabel} {targetName} from "{communityName}"?
    </h2>

    <label class="reason-label" for="moderation-reason-textarea">
      Optional: reason (visible to {targetName} and other mods)
    </label>
    <textarea
      id="moderation-reason-textarea"
      class="reason-textarea"
      bind:value={reason}
      maxlength={280}
      placeholder="e.g., repeated spam in #general"
      rows={3}
      disabled={submitting}
    ></textarea>

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
        class="primary-btn {action}"
        onclick={handleConfirm}
        disabled={submitting}
      >
        {#if submitting}
          <span class="spinner" aria-hidden="true"></span>
          {actionLabelProgressive}...
        {:else}
          {actionLabel}
        {/if}
      </button>
    </div>
  </Modal>
{/if}

<style>
  .dialog-title {
    color: var(--text-primary);
    font-size: 1rem;
    margin: 0 0 16px;
    font-weight: 600;
  }
  .reason-label {
    display: block;
    font-size: 0.8rem;
    color: var(--text-secondary);
    margin-bottom: 6px;
  }
  .reason-textarea {
    width: 100%;
    padding: 8px 10px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-primary);
    font-size: 0.85rem;
    resize: vertical;
    box-sizing: border-box;
    font-family: inherit;
    line-height: 1.4;
  }
  .reason-textarea:focus {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: -1px;
  }
  .reason-textarea:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .error {
    color: #cc7a7a;
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
  .primary-btn {
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
  .primary-btn {
    background: #d83c3e;
    color: #fff;
  }
  .primary-btn.unban {
    background: var(--accent, #5865f2);
    color: var(--text-primary);
  }
  .primary-btn:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }
  .cancel-btn:focus-visible,
  .primary-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }
  .spinner {
    display: inline-block;
    width: 12px;
    height: 12px;
    border: 2px solid rgba(255, 255, 255, 0.3);
    border-top-color: #fff;
    border-radius: 50%;
    animation: spin 0.6s linear infinite;
    flex-shrink: 0;
  }
  .primary-btn.unban .spinner {
    border-color: rgba(255, 255, 255, 0.3);
    border-top-color: var(--text-primary);
  }
  @keyframes spin {
    to { transform: rotate(360deg); }
  }
</style>
