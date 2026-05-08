<script lang="ts">
  interface Props {
    title: string;
    description: string;
    confirmLabel: string;
    danger?: boolean;
    onConfirm: () => void;
    onCancel: () => void;
  }

  let { title, description, confirmLabel, danger = false, onConfirm, onCancel }: Props = $props();

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') onCancel();
  }
</script>

<svelte:window on:keydown={handleKeydown} />

<div class="modal-backdrop" onclick={onCancel} role="presentation">
  <div class="modal" onclick={(e) => e.stopPropagation()} role="dialog" aria-modal="true">
    <h3 class="modal-title">{title}</h3>
    <p class="modal-description">{description}</p>

    <div class="action-row">
      <button class="confirm" class:danger onclick={onConfirm}>{confirmLabel}</button>
      <div class="spacer"></div>
      <button class="cancel" onclick={onCancel}>Cancel</button>
    </div>
  </div>
</div>

<style>
  .modal-backdrop {
    position: fixed; inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex; align-items: center; justify-content: center;
    z-index: 1000;
  }
  .modal {
    background: var(--surface, #1e1e1e);
    border-radius: 8px;
    padding: 20px;
    max-width: 420px;
    width: 90%;
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.6);
  }
  .modal-title { margin: 0 0 10px 0; font-size: 15px; }
  .modal-description { margin: 0 0 20px 0; font-size: 13px; color: var(--text-muted, #ccc); }
  .action-row { display: flex; gap: 8px; align-items: center; }
  .spacer { flex: 1; }
  .confirm.danger { background: #cc4a4a; color: white; border-color: #cc4a4a; }
</style>
