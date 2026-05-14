<script lang="ts">
  import Modal from './Modal.svelte';
  import type { VineVideo } from '../types';
  import { resolveOriginalCreator } from '../vine-utils';

  let {
    vine,
    onConfirm,
    onCancel,
  }: {
    vine: VineVideo;
    onConfirm: () => void;
    onCancel: () => void;
  } = $props();

  const titleId = `reshare-confirm-title-${Math.random().toString(36).slice(2)}`;

  // Use the shared resolver so the attribution displayed here matches
  // what `publish()` will actually attach to the new reshare descriptor.
  // For a reshare-of-a-reshare with full originalCreator* fields, this
  // surfaces the true origin; for an original vine or a partial/legacy
  // reshare payload, it falls back to the immediate creator name.
  let attribution = $derived(resolveOriginalCreator(vine).originalCreatorName);
</script>

<Modal {onCancel} ariaLabelledby={titleId} canDismissOnBackdrop={true}>
  <h3 class="modal-title" id={titleId}>Reshare this vine?</h3>
  <p class="modal-description">
    {#if vine.title}
      <strong>{vine.title}</strong>
      <br />
    {/if}
    Originally by {attribution}. Your reshare will preserve attribution to them.
  </p>

  <div class="action-row">
    <button class="confirm-btn" onclick={onConfirm}>Reshare</button>
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
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }
</style>
