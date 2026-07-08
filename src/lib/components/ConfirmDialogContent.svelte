<script lang="ts">
  /**
   * ZEB-656 — shared inner content for the two byte-identical confirm dialogs
   * (ConfirmDialog + DoubleConfirmDialog). Collapses the previously-duplicated
   * `<style>` block into one Commons-aligned definition; the rendered DOM
   * matches the pre-refactor markup, so those dialogs' existing tests are the
   * regression net. The `<Modal>` wrapper stays in each parent —
   * DoubleConfirmDialog wraps a single Modal around both gates, so the
   * elevation must not live here.
   */
  let {
    title,
    titleId,
    message,
    confirmLabel,
    destructive = false,
    onConfirm,
    onCancel,
  }: {
    title: string;
    titleId: string;
    message: string;
    confirmLabel: string;
    destructive?: boolean;
    onConfirm: () => void;
    onCancel: () => void;
  } = $props();
</script>

<h2 class="dialog-title" id={titleId}>{title}</h2>
<p class="dialog-message">{message}</p>
<div class="dialog-actions">
  <button class="cancel-btn" onclick={onCancel}>Cancel</button>
  <button class="confirm-btn" class:destructive onclick={onConfirm}>
    {confirmLabel}
  </button>
</div>

<style>
  .dialog-title {
    color: var(--text-primary);
    font-family: var(--font-display);
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
    background: var(--surface-raised);
    color: var(--text-secondary);
    border: 1px solid var(--border);
    padding: 8px 16px;
    border-radius: 7px;
    cursor: pointer;
    font-size: 0.875rem;
  }

  .confirm-btn {
    background: var(--accent);
    color: var(--text-primary);
    border: none;
    padding: 8px 16px;
    border-radius: 7px;
    cursor: pointer;
    font-size: 0.875rem;
  }

  .confirm-btn.destructive {
    background: var(--danger-muted);
  }

  .confirm-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .cancel-btn:focus-visible,
  .confirm-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
</style>
