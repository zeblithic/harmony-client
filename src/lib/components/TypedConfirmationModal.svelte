<script lang="ts">
  interface Props {
    title: string;
    description: string;
    requiredText: string;
    confirmLabel: string;
    onConfirm: () => void;
    onCancel: () => void;
  }

  let { title, description, requiredText, confirmLabel, onConfirm, onCancel }: Props = $props();
  let typed = $state('');
  let matches = $derived(typed.trimEnd() === requiredText);

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') onCancel();
  }
</script>

<svelte:window on:keydown={handleKeydown} />

<div class="modal-backdrop" onclick={onCancel} role="presentation">
  <div class="modal" onclick={(e) => e.stopPropagation()} role="dialog" aria-modal="true">
    <h3 class="modal-title">⚠ {title}</h3>
    <p class="modal-description">{description}</p>

    <p class="prompt">
      Type <strong class="required">{requiredText}</strong> to confirm:
    </p>
    <input
      type="text"
      bind:value={typed}
      placeholder="Type community name exactly..."
      class="typed-input"
    />

    <div class="action-row">
      <button class="confirm danger" disabled={!matches} onclick={onConfirm}>
        {confirmLabel}
      </button>
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
    max-width: 480px;
    width: 90%;
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.6);
  }
  .modal-title { margin: 0 0 10px 0; font-size: 15px; color: #cc7a7a; }
  .modal-description { margin: 0 0 16px 0; font-size: 12px; color: #ccc; }
  .prompt { margin: 0 0 6px 0; font-size: 13px; color: #ccc; }
  .required { font-family: monospace; background: #2a2a2a; padding: 1px 6px; border-radius: 3px; color: #eee; }
  .typed-input { width: 100%; margin-bottom: 16px; font-family: monospace; font-size: 13px; padding: 6px 8px; }
  .action-row { display: flex; gap: 8px; align-items: center; }
  .spacer { flex: 1; }
  .confirm.danger { background: #cc4a4a; color: white; border-color: #cc4a4a; }
  .confirm.danger:disabled { background: #553333; color: #888; opacity: 0.6; cursor: not-allowed; }
</style>
