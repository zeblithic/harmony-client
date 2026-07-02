<script lang="ts">
  import Modal from './Modal.svelte';

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
  const titleId = `dialog-title-${Math.random().toString(36).slice(2)}`;
  let contentEl: HTMLElement;

  // Re-focus first button when gate transitions 1→2 — trapFocus only acts
  // on mount/unmount, not on internal state changes that swap the visible
  // button set. On mount this no-ops because trapFocus already focused the
  // first button.
  $effect(() => {
    void gate;
    contentEl?.querySelector<HTMLElement>('button')?.focus();
  });
</script>

<Modal onCancel={onCancel} ariaLabelledby={titleId}>
  <h2 class="dialog-title" id={titleId}>{title}</h2>
  <div bind:this={contentEl}>
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
</Modal>

<style>
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
