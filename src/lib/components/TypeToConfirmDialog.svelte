<script lang="ts">
  let {
    title,
    message,
    confirmText,
    confirmLabel,
    destructive = false,
    onConfirm,
    onCancel,
  }: {
    title: string;
    message: string;
    confirmText: string;
    confirmLabel: string;
    destructive?: boolean;
    onConfirm: () => void;
    onCancel: () => void;
  } = $props();

  let typed = $state('');
  let matches = $derived(typed === confirmText);
  const titleId = `dialog-title-${Math.random().toString(36).slice(2)}`;
</script>

<div class="dialog-overlay">
  <div class="dialog" role="dialog" aria-modal="true" aria-labelledby={titleId}>
    <h2 class="dialog-title" id={titleId}>{title}</h2>
    <p class="dialog-message">{message}</p>
    <p class="dialog-hint">Type <code>{confirmText}</code> to confirm</p>
    <input
      class="dialog-input"
      type="text"
      aria-label="Type to confirm"
      bind:value={typed}
    />
    <div class="dialog-actions">
      <button class="cancel-btn" onclick={onCancel}>Cancel</button>
      <button
        class="confirm-btn"
        class:destructive
        disabled={!matches}
        onclick={onConfirm}
      >
        {confirmLabel}
      </button>
    </div>
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
    margin: 0 0 12px;
  }

  .dialog-hint {
    color: var(--text-secondary);
    font-size: 0.85rem;
    margin: 0 0 8px;
  }

  .dialog-hint code {
    background: var(--bg-tertiary);
    padding: 2px 6px;
    border-radius: 3px;
    font-family: monospace;
    color: var(--text-primary);
  }

  .dialog-input {
    width: 100%;
    padding: 8px 12px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-primary);
    font-size: 0.9rem;
    margin-bottom: 20px;
    box-sizing: border-box;
  }

  .dialog-input:focus {
    outline: 2px solid var(--accent);
    outline-offset: -1px;
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
