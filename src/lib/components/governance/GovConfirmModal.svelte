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

  let typedInput = $state('');
  let confirmEnabled = $derived(
    !busy &&
      (severity === 'click' ||
        typedInput.trim().toLowerCase() === typedMatch.toLowerCase()),
  );
</script>

<div class="confirm-modal" role="dialog" aria-modal="true" aria-label={title}>
  <div class="confirm-card">
    <p class="confirm-title">{title}</p>
    {#if children}
      {@render children()}
    {/if}
    {#if severity === 'typed'}
      <input
        class="typed-input"
        type="text"
        bind:value={typedInput}
        placeholder={typedMatch}
        aria-label={`Type the word ${typedMatch} to confirm`}
        disabled={busy}
      />
    {/if}
    <div class="confirm-actions">
      <button type="button" class="cancel" onclick={onCancel} disabled={busy}>
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
    color: var(--text-bright);
    font-weight: 600;
  }
</style>
