<script lang="ts">
  /**
   * ZEB-218 Sub-D Phase 1: paste-an-address dialog for adding a library
   * to the user's trust set. Validates the 32-hex-char OwnerAddr at the
   * frontend before round-tripping; surfaces backend errors inline so
   * the user sees them without dismissing the modal.
   */
  let {
    onSubmit,
    onCancel,
    pending = false,
    error = null,
  }: {
    onSubmit: (libraryAddr: string) => void;
    onCancel: () => void;
    pending?: boolean;
    error?: string | null;
  } = $props();

  let inputAddr = $state('');
  const HEX_32 = /^[0-9a-fA-F]{32}$/;

  let isValid = $derived(HEX_32.test(inputAddr.trim()));
  let canSubmit = $derived(isValid && !pending);

  function handleSubmit(e?: Event) {
    e?.preventDefault();
    if (!canSubmit) return;
    onSubmit(inputAddr.trim().toLowerCase());
  }
</script>

<div class="dialog" role="dialog" aria-modal="true" aria-labelledby="add-library-title">
  <h3 id="add-library-title">Add a library</h3>
  <p class="subtitle">
    Libraries publish curated catalogs of communities. Paste a library's
    32-character address.
  </p>

  <form onsubmit={handleSubmit}>
    <label class="sr-only" for="library-addr-input">Library address (32 hex chars)</label>
    <input
      id="library-addr-input"
      type="text"
      placeholder="32-character library address (hex)"
      bind:value={inputAddr}
      disabled={pending}
      class="addr-input"
      class:invalid={inputAddr.length > 0 && !isValid}
      autofocus
    />
    {#if inputAddr.length > 0 && !isValid}
      <p class="validation">Address must be exactly 32 hex characters.</p>
    {/if}
    {#if error}
      <p class="error-banner">{error}</p>
    {/if}
    <div class="actions">
      <button type="button" onclick={onCancel} disabled={pending}>Cancel</button>
      <button type="submit" class="primary" disabled={!canSubmit}>
        {pending ? 'Adding…' : 'Add library'}
      </button>
    </div>
  </form>
</div>

<style>
  .dialog { padding: 16px; max-width: 420px; }
  .subtitle { color: var(--text-secondary); font-size: 0.85rem; margin-top: -4px; }
  .sr-only { position: absolute; width: 1px; height: 1px; clip: rect(0,0,0,0); }
  .addr-input {
    width: 100%; box-sizing: border-box; padding: 8px 10px;
    font-family: var(--font-mono); font-size: 0.9rem;
    background: var(--bg-tertiary); border: 1px solid var(--border);
    color: var(--text-primary); border-radius: 4px;
  }
  .addr-input.invalid { border-color: var(--danger-muted); }
  .validation { color: var(--danger-muted); font-size: 0.75rem; margin-top: 4px; }
  .error-banner {
    background: var(--bg-tertiary); border: 1px solid var(--danger-muted);
    color: var(--danger-muted); padding: 8px 10px; border-radius: 4px;
    font-size: 0.8rem; margin-top: 8px;
  }
  .actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 12px; }
  .actions button { padding: 6px 12px; }
  .primary { background: color-mix(in srgb, var(--library-accent) 40%, transparent); }
  .primary:disabled { opacity: 0.4; cursor: not-allowed; }
</style>
