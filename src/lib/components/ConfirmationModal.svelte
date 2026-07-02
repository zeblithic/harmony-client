<script lang="ts">
  import Modal from './Modal.svelte';

  let {
    title,
    description,
    confirmLabel,
    danger = false,
    onConfirm,
    onCancel,
  }: {
    title: string;
    description: string;
    confirmLabel: string;
    danger?: boolean;
    onConfirm: () => void;
    onCancel: () => void;
  } = $props();

  const titleId = `confirmation-modal-title-${Math.random().toString(36).slice(2)}`;
</script>

<Modal {onCancel} ariaLabelledby={titleId}>
  <h3 class="modal-title" id={titleId}>{title}</h3>
  <p class="modal-description">{description}</p>

  <div class="action-row">
    <button class="confirm-btn" class:danger onclick={onConfirm}>{confirmLabel}</button>
    <div class="spacer"></div>
    <button class="cancel-btn" onclick={onCancel}>Cancel</button>
  </div>
</Modal>

<style>
  .modal-title {
    color: var(--text-primary);
    font-size: 1rem;
    margin: 0 0 12px;
  }
  .modal-description {
    color: var(--text-secondary);
    font-size: 0.875rem;
    line-height: 1.5;
    margin: 0 0 20px;
  }
  .action-row {
    display: flex;
    gap: 8px;
    align-items: center;
  }
  .spacer { flex: 1; }
  .confirm-btn {
    background: var(--accent);
    color: var(--text-primary);
    border: none;
    padding: 8px 16px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.875rem;
  }
  .confirm-btn.danger { background: var(--danger-muted); }
  .cancel-btn {
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    border: none;
    padding: 8px 16px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.875rem;
  }
  .cancel-btn:focus-visible,
  .confirm-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
</style>
