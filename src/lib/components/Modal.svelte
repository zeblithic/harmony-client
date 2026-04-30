<script lang="ts">
  import type { Snippet } from 'svelte';
  import { trapFocus } from '../actions/trap-focus';

  let {
    onCancel,
    canCancel = true,
    ariaLabelledby,
    children,
  }: {
    onCancel: () => void;
    canCancel?: boolean;
    ariaLabelledby: string;
    children?: Snippet;
  } = $props();
</script>

<div class="modal-overlay">
  <div
    class="modal"
    role="dialog"
    aria-modal="true"
    aria-labelledby={ariaLabelledby}
    use:trapFocus={{ onCancel, canCancel }}
  >
    {@render children?.()}
  </div>
</div>

<style>
  .modal-overlay {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }
  .modal {
    background: var(--bg-secondary);
    padding: 24px;
    border-radius: 8px;
    max-width: 480px;
    border: 1px solid var(--border);
  }
</style>
