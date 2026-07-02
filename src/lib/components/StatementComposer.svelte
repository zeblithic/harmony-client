<script lang="ts">
  /**
   * ZEB-294 — composer for mini-public deliberation statements.
   * Click-confirm modal (immutable → forces deliberate composition).
   * Disabled when stage exits Deliberation or 5-cap reached.
   */
  import type { Tier3PollExport } from '../types/voting';
  import type { VotingAdapter } from '../voting-adapter';

  let {
    detail,
    adapter,
    onChange,
  }: {
    detail: Tier3PollExport;
    adapter: VotingAdapter;
    onChange: () => void;
  } = $props();

  let text = $state('');
  let confirming = $state(false);
  let submitting = $state(false);
  let submitError = $state<string | null>(null);

  // Count Unicode scalar values (matches Rust `chars().count()` at apply time),
  // not UTF-16 code units (`text.length`). `[...text]` iterates code points so
  // emoji and other supplementary-plane characters count as 1 instead of 2.
  let charCount = $derived([...text].length);
  let charsRemaining = $derived(280 - charCount);
  let canSubmit = $derived(
    text.trim().length > 0
      && charCount <= 280
      && detail.stage === 'de'
      && detail.myDeliberationStatementCount < 5
      && !submitting,
  );

  async function confirmSubmit() {
    confirming = false;
    submitting = true;
    submitError = null;
    try {
      await adapter.submitDeliberationStatement(detail.pollId, text);
      text = '';
      onChange();
    } catch (e) {
      submitError = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
    }
  }
</script>

<section class="composer">
  <h5>Compose statement</h5>
  <p class="cap-note">{detail.myDeliberationStatementCount} / 5 statements submitted</p>
  {#if detail.myDeliberationStatementCount >= 5}
    <p class="cap-warning">You've used all 5 statement slots for this poll.</p>
  {:else}
    <textarea
      placeholder="Up to 280 characters. Statements are immutable once submitted."
      bind:value={text}
      disabled={submitting}
      aria-invalid={charCount > 280}
    ></textarea>
    <div class="footer">
      <span class="char-count">{charsRemaining} chars left</span>
      <button type="button" disabled={!canSubmit} onclick={() => (confirming = true)}>
        {submitting ? 'Submitting…' : 'Submit'}
      </button>
    </div>
    {#if submitError}<p class="error">{submitError}</p>{/if}
  {/if}
</section>

{#if confirming}
  <div class="confirm-modal" role="dialog" aria-modal="true" aria-label="Confirm statement">
    <div class="confirm-card">
      <p>Confirm statement submission</p>
      <blockquote class="preview">{text}</blockquote>
      <p class="caveat">Statements are immutable — once submitted, you cannot edit or retract.</p>
      <div class="actions">
        <button type="button" onclick={() => (confirming = false)}>Cancel</button>
        <button type="button" onclick={confirmSubmit}>Confirm</button>
      </div>
    </div>
  </div>
{/if}

<style>
  .composer { background: var(--panel-bg); padding: 0.75rem; border-radius: 6px; }
  textarea { width: 100%; min-height: 80px; padding: 0.4rem; background: var(--input-bg); color: inherit; border: 1px solid var(--chip-bg); border-radius: 3px; }
  .footer { display: flex; justify-content: space-between; align-items: center; margin-top: 0.4rem; }
  .char-count { color: var(--text-faint); font-size: 0.85rem; }
  .cap-note { color: var(--text-faint); font-size: 0.8rem; margin: 0 0 0.4rem 0; }
  .cap-warning { color: #d9b438; font-size: 0.85rem; }
  .error { color: var(--danger-alt); }
  button { background: var(--accent); color: var(--text-bright); border: 0; padding: 0.35rem 0.9rem; border-radius: 3px; cursor: pointer; }
  button:disabled { opacity: 0.5; cursor: not-allowed; }
  .confirm-modal { position: fixed; inset: 0; background: var(--overlay); display: grid; place-items: center; z-index: 100; }
  .confirm-card { background: var(--panel-bg); padding: 1.25rem; border-radius: 8px; max-width: 480px; display: flex; flex-direction: column; gap: 0.6rem; }
  .preview { background: var(--input-bg); padding: 0.6rem; border-left: 3px solid var(--accent); margin: 0; font-style: normal; }
  .caveat { color: var(--text-faint); font-size: 0.8rem; }
  .actions { display: flex; gap: 0.5rem; justify-content: flex-end; }
  .actions button:last-child { background: var(--accent); color: var(--text-bright); }
</style>
