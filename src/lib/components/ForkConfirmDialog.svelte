<script lang="ts">
  import { untrack } from 'svelte';
  import Modal from './Modal.svelte';
  import TypedConfirmationModal from './TypedConfirmationModal.svelte';

  let {
    originalName,
    messageCount,
    onConfirm,
    onCancel,
  }: {
    originalName: string;
    messageCount: number;
    onConfirm: (opts: { name: string; silent: boolean; alsoLeave: boolean }) => void;
    onCancel: () => void;
  } = $props();

  // Intentionally snapshot the initial prop value — the user may freely edit
  // the fork name, so reactive re-derivation on prop change is undesirable.
  let name = $state(untrack(() => originalName) + ' (fork)');
  let silent = $state(false);
  let alsoLeave = $state(false);
  let typedConfirmOpen = $state(false);

  const titleId = `fork-confirm-title-${Math.random().toString(36).slice(2)}`;

  let nameValid = $derived(name.trim().length > 0);

  function handleCreateFork() {
    if (!nameValid) return;
    if (alsoLeave) {
      typedConfirmOpen = true;
    } else {
      onConfirm({ name: name.trim(), silent, alsoLeave: false });
    }
  }

  function handleTypedConfirm() {
    onConfirm({ name: name.trim(), silent, alsoLeave: true });
    typedConfirmOpen = false;
  }

  function handleTypedCancel() {
    typedConfirmOpen = false;
  }
</script>

{#if typedConfirmOpen}
  <TypedConfirmationModal
    title="Leave and fork community?"
    description="This will fork the community and also remove you from the original. This action cannot be easily undone."
    requiredText="leave"
    confirmLabel="Confirm"
    onConfirm={handleTypedConfirm}
    onCancel={handleTypedCancel}
  />
{:else}
  <Modal {onCancel} ariaLabelledby={titleId} canDismissOnBackdrop={true}>
    <div class="fork-header">
      <span class="fork-glyph" aria-hidden="true">⑂</span>
      <h3 class="modal-title" id={titleId}>Fork this community</h3>
    </div>

    <p class="modal-description">
      This creates a new community with a frozen copy of the history you can see
      in this one. Anyone you invite to the fork will see that history.
    </p>

    <div class="field">
      <label for="fork-name">Name:</label>
      <input
        id="fork-name"
        type="text"
        bind:value={name}
        class="name-input"
      />
    </div>

    <div class="checkbox-field">
      <label>
        <input type="checkbox" bind:checked={silent} />
        Fork silently (don't tell other members)
      </label>
    </div>

    <div class="checkbox-field">
      <label>
        <input type="checkbox" bind:checked={alsoLeave} />
        Also leave the original community
      </label>
    </div>

    <p class="snapshot-count">
      {#if messageCount > 0}
        Snapshot will include ~{messageCount} messages.
      {:else}
        Snapshot will include your accessible message history (up to 5000 messages).
      {/if}
    </p>
    <p class="snapshot-note">A frozen snapshot of every channel is always included.</p>

    <p class="consent-note">
      Forking writes a permanent divider into the new community's history. You
      become its first admin, and the original community is never affected.
    </p>

    <div class="action-row">
      <button class="confirm-btn" disabled={!nameValid} onclick={handleCreateFork}>
        Create fork
      </button>
      <div class="spacer"></div>
      <button class="cancel-btn" onclick={onCancel}>Cancel</button>
    </div>
  </Modal>
{/if}

<style>
  .fork-header {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-bottom: 12px;
  }
  .fork-glyph {
    flex: 0 0 auto;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 32px;
    height: 32px;
    border-radius: 8px;
    background: var(--gov-clay-soft);
    color: var(--gov-clay);
    font-size: 1rem;
  }
  .modal-title {
    color: var(--text-primary);
    font-family: var(--font-display);
    font-size: 1.15rem;
    margin: 0;
  }
  .modal-description {
    color: var(--text-secondary);
    font-size: 0.875rem;
    line-height: 1.5;
    margin: 0 0 16px;
  }
  .field {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 12px;
  }
  .field label {
    color: var(--text-secondary);
    font-size: 0.875rem;
    white-space: nowrap;
  }
  .name-input {
    flex: 1;
    padding: 6px 10px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-primary);
    font-size: 0.875rem;
  }
  .name-input:focus {
    outline: none;
    border-color: var(--accent);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 12%, transparent);
  }
  .checkbox-field {
    margin-bottom: 10px;
  }
  .checkbox-field label {
    display: flex;
    align-items: center;
    gap: 8px;
    color: var(--text-secondary);
    font-size: 0.875rem;
    cursor: pointer;
  }
  .snapshot-count {
    color: var(--text-secondary);
    font-size: 0.875rem;
    margin: 12px 0 16px;
  }
  .snapshot-note {
    color: var(--text-muted);
    font-size: 0.8rem;
    margin: 0 0 12px;
  }
  .consent-note {
    background: var(--primary-soft);
    border: 1px solid var(--primary-border);
    color: var(--primary-deep);
    border-radius: 8px;
    padding: 10px 12px;
    font-size: 0.8rem;
    line-height: 1.45;
    margin: 0 0 16px;
  }
  .action-row {
    display: flex;
    gap: 8px;
    align-items: center;
  }
  .spacer { flex: 1; }
  .confirm-btn {
    background: var(--gov-clay);
    color: var(--text-bright);
    border: none;
    padding: 8px 16px;
    border-radius: 7px;
    cursor: pointer;
    font-size: 0.875rem;
    font-weight: 600;
  }
  .confirm-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
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
  .cancel-btn:focus-visible,
  .confirm-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
</style>
