<script lang="ts">
  let {
    title,
    firstMessage,
    secondMessage,
    confirmLabel,
    destructive = false,
    onConfirm,
    onCancel,
  }: {
    title: string;
    firstMessage: string;
    secondMessage: string;
    confirmLabel: string;
    destructive?: boolean;
    onConfirm: () => void;
    onCancel: () => void;
  } = $props();

  let gate = $state(1);
  const titleId = 'dialog-title';
</script>

<div class="dialog-overlay">
  <div class="dialog" role="dialog" aria-modal="true" aria-labelledby={titleId}>
    <h2 class="dialog-title" id={titleId}>{title}</h2>
    {#if gate === 1}
      <p class="dialog-message">{firstMessage}</p>
      <div class="dialog-actions">
        <button class="cancel-btn" onclick={onCancel}>Cancel</button>
        <button class="confirm-btn" onclick={() => gate = 2}>Continue</button>
      </div>
    {:else}
      <p class="dialog-message">{secondMessage}</p>
      <div class="dialog-actions">
        <button class="cancel-btn" onclick={onCancel}>Cancel</button>
        <button
          class="confirm-btn"
          class:destructive
          onclick={onConfirm}
        >
          {confirmLabel}
        </button>
      </div>
    {/if}
  </div>
</div>

<style>
  .dialog-overlay {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.6);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 100;
  }

  .dialog {
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 24px;
    max-width: 480px;
    width: 100%;
  }

  .dialog-title {
    color: var(--text-primary);
    font-size: 1.1rem;
    margin: 0 0 12px;
  }

  .dialog-message {
    color: var(--text-secondary);
    font-size: 0.9rem;
    line-height: 1.5;
    margin: 0 0 20px;
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

  .confirm-btn.destructive {
    background: #d83c3e;
  }

  .confirm-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .cancel-btn:focus-visible,
  .confirm-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }
</style>
