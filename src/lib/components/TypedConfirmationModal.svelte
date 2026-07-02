<script lang="ts">
  import Modal from './Modal.svelte';

  let {
    title,
    description,
    requiredText,
    confirmLabel,
    onConfirm,
    onCancel,
  }: {
    title: string;
    description: string;
    requiredText: string;
    confirmLabel: string;
    onConfirm: () => void;
    onCancel: () => void;
  } = $props();

  let typed = $state('');
  // Spec (docs/specs/2026-05-08-zeb-263-community-frontend-design.md
  // §"Tier 3"): "case-sensitive, trim-trailing-whitespace". Trailing
  // whitespace is forgivable (autocomplete, keyboard quirks); leading
  // whitespace is deliberate input that should fail the match. This
  // is the highest-security confirmation tier, so we resist a more
  // permissive trim() that an earlier review pass had suggested.
  let matches = $derived(typed.trimEnd() === requiredText);
  const titleId = `typed-confirmation-title-${Math.random().toString(36).slice(2)}`;
</script>

<Modal {onCancel} ariaLabelledby={titleId}>
  <h3 class="modal-title" id={titleId}>⚠ {title}</h3>
  <p class="modal-description">{description}</p>

  <p class="prompt">
    Type <strong class="required">{requiredText}</strong> to confirm:
  </p>
  <input
    type="text"
    bind:value={typed}
    placeholder="Type community name exactly..."
    class="typed-input"
    aria-label="Type to confirm"
  />

  <div class="action-row">
    <button class="confirm-btn danger" disabled={!matches} onclick={onConfirm}>
      {confirmLabel}
    </button>
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
    margin: 0 0 16px;
  }
  .prompt {
    color: var(--text-secondary);
    font-size: 0.875rem;
    margin: 0 0 8px;
  }
  .required {
    font-family: monospace;
    background: var(--bg-tertiary);
    padding: 2px 6px;
    border-radius: 3px;
    color: var(--text-primary);
  }
  .typed-input {
    width: 100%;
    padding: 8px 12px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-primary);
    font-family: monospace;
    font-size: 0.875rem;
    margin-bottom: 16px;
    box-sizing: border-box;
  }
  .typed-input:focus {
    outline: 2px solid var(--accent);
    outline-offset: -1px;
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
  .confirm-btn.danger { background: #d83c3e; }
  .confirm-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
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
  .cancel-btn:focus-visible,
  .confirm-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
</style>
