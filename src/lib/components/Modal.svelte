<script lang="ts">
  import type { Snippet } from 'svelte';
  import { trapFocus } from '../actions/trap-focus';

  let {
    onCancel,
    canCancel = true,
    canDismissOnBackdrop = false,
    ariaLabelledby,
    children,
  }: {
    onCancel: () => void;
    canCancel?: boolean;
    /**
     * Opt-in: click on the backdrop (outside the dialog) fires onCancel.
     * Defaults `false` to preserve existing consumer behavior — only the
     * Escape key and explicit cancel buttons dismiss by default.
     * Consumers requiring spec-driven backdrop dismissal (e.g.
     * ReshareConfirmDialog) opt in.
     */
    canDismissOnBackdrop?: boolean;
    ariaLabelledby: string;
    children?: Snippet;
  } = $props();

  function handleOverlayClick(e: MouseEvent) {
    if (!canDismissOnBackdrop || !canCancel) return;
    // Only fire when the click lands on the overlay itself, not when it
    // bubbles up from the inner .modal (or anything inside it).
    if (e.target === e.currentTarget) {
      onCancel();
    }
  }
</script>

<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="modal-overlay" onclick={handleOverlayClick}>
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
    background: var(--overlay);
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
