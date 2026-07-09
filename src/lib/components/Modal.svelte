<script lang="ts">
  import type { Snippet } from 'svelte';
  import { trapFocus } from '../actions/trap-focus';

  let {
    onCancel,
    canCancel = true,
    canDismissOnBackdrop = false,
    ariaLabelledby,
    wide = false,
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
    /** ZEB-649: opt-in wide layout (genealogy graph); default keeps the
     *  480px dialog every existing consumer expects. */
    wide?: boolean;
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
    class:modal-wide={wide}
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
    /* ZEB-656: Commons elevation — warm raised paper + soft shadow, matching
       the already-aligned governance/GovConfirmModal. App-wide: every
       Modal-based dialog gains this lift. Radius (8px) + border already correct. */
    background: var(--surface-raised);
    padding: 24px;
    border-radius: 8px;
    max-width: 480px;
    border: 1px solid var(--border);
    box-shadow: var(--shadow-e2);
  }
  .modal-wide {
    max-width: min(920px, 94vw);
  }
</style>
