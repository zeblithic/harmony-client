<script lang="ts">
  /**
   * ZEB-607 — shared governance confirm modal (spec D2). Replaces the
   * three verbatim .confirm-modal copies (Tier3ProposalPanel,
   * StatementComposer, StarRatificationBallot) and hosts
   * DelegationWidget's typed-"revoke" severity tier
   * (feedback_severe_action_confirmation: click = reversible,
   * typed = irreversible-by-consequence).
   */
  import type { Snippet } from 'svelte';
  import { trapFocus } from '../../actions/trap-focus';

  let {
    title,
    confirmLabel = 'Confirm',
    cancelLabel = 'Cancel',
    severity = 'click',
    typedMatch = 'revoke',
    busy = false,
    onConfirm,
    onCancel,
    children,
  }: {
    title: string;
    confirmLabel?: string;
    cancelLabel?: string;
    severity?: 'click' | 'typed';
    typedMatch?: string;
    busy?: boolean;
    onConfirm: () => void;
    onCancel: () => void;
    children?: Snippet;
  } = $props();

  // ZEB-647: ids for aria-labelledby/-describedby — alertdialog announces
  // the described-by content (the actual warning copy) on focus.
  const uid = $props.id();
  const titleId = `${uid}-title`;
  const bodyId = `${uid}-body`;

  let typedInput = $state('');
  // ZEB-647 (Qodo PR #433): explicit initial-focus targets — children render
  // before the controls, so a focusable element in the body copy would
  // otherwise steal initial focus by DOM order.
  let typedInputEl: HTMLInputElement | null = $state(null);
  let cancelEl: HTMLButtonElement | null = $state(null);
  let confirmEnabled = $derived.by(() => {
    if (busy) return false;
    if (severity === 'click') return true;
    const match = typedMatch.trim();
    // An empty match string must never enable the confirm — a caller
    // accidentally passing typedMatch="" would otherwise disable the
    // typed-confirm protection outright (PR #409 Qodo).
    if (match.length === 0) return false;
    return typedInput.trim().toLowerCase() === match.toLowerCase();
  });
</script>

<div class="confirm-modal">
  <div
    class="confirm-card"
    role="alertdialog"
    aria-modal="true"
    aria-labelledby={titleId}
    aria-describedby={children ? bodyId : undefined}
    use:trapFocus={{
      onCancel,
      canCancel: !busy,
      initialFocus: () => (severity === 'typed' ? typedInputEl : cancelEl),
    }}
  >
    <p class="confirm-title" id={titleId}>{title}</p>
    {#if children}
      <div class="confirm-body" id={bodyId}>
        {@render children()}
      </div>
    {/if}
    {#if severity === 'typed'}
      <input
        class="typed-input"
        type="text"
        bind:this={typedInputEl}
        bind:value={typedInput}
        placeholder={typedMatch}
        aria-label={`Type the word ${typedMatch} to confirm`}
        disabled={busy}
      />
    {/if}
    <div class="confirm-actions">
      <button
        type="button"
        class="cancel"
        bind:this={cancelEl}
        onclick={onCancel}
        disabled={busy}
      >
        {cancelLabel}
      </button>
      <button type="button" class="confirm" onclick={onConfirm} disabled={!confirmEnabled}>
        {confirmLabel}
      </button>
    </div>
  </div>
</div>

<style>
  .confirm-modal {
    position: fixed;
    inset: 0;
    background: var(--overlay);
    display: grid;
    place-items: center;
    z-index: 100;
  }
  .confirm-card {
    background: var(--surface-raised);
    border: 1px solid var(--border);
    box-shadow: var(--shadow-e2);
    padding: 1.25rem 1.5rem;
    border-radius: 10px;
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
    max-width: 480px;
  }
  .confirm-title {
    margin: 0;
    font-weight: 600;
    color: var(--text-primary);
  }
  .confirm-body {
    /* Mirrors .confirm-card's layout: consumers pass sibling elements
       (preview + caveat) that were direct flex children of the card before
       this wrapper existed — keep their 0.75rem rhythm. */
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
  }
  .typed-input {
    padding: 6px 10px;
    border: 1px solid var(--border);
    border-radius: 5px;
    background: var(--input-bg);
    color: var(--text-primary);
    font: inherit;
    max-width: 160px;
  }
  .typed-input:focus {
    outline: 1px solid var(--accent);
    outline-offset: 1px;
  }
  .confirm-actions {
    display: flex;
    gap: 0.5rem;
    justify-content: flex-end;
  }
  .confirm-actions button {
    padding: 6px 14px;
    border-radius: 7px;
    font: inherit;
    cursor: pointer;
  }
  .confirm-actions button:disabled {
    cursor: not-allowed;
    opacity: 0.5;
  }
  .cancel {
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-secondary);
  }
  .confirm {
    border: 1px solid var(--accent);
    background: var(--accent);
    color: var(--on-accent);
    font-weight: 600;
  }
</style>
