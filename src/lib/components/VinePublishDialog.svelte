<script lang="ts">
  import { onMount } from 'svelte';

  let { onPublish, onClose }: {
    onPublish: (videoCid: string, title?: string) => void;
    onClose: () => void;
  } = $props();

  let videoCid = $state('');
  let title = $state('');
  let error = $state('');
  let cidInput: HTMLInputElement;

  onMount(() => cidInput?.focus());

  function handleSubmit() {
    const cid = videoCid.trim();
    if (!cid) {
      error = 'Video CID is required';
      return;
    }
    error = '';
    onPublish(cid, title.trim() || undefined);
    videoCid = '';
    title = '';
    onClose();
  }

  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === 'Escape') onClose();
  }

  function handleOverlayClick(e: MouseEvent) {
    if (e.target === e.currentTarget) onClose();
  }
</script>

<svelte:window onkeydown={handleKeyDown} />

<div class="dialog-overlay" role="dialog" aria-label="Publish vine" aria-modal="true" onclick={handleOverlayClick}>
  <div class="dialog-card">
    <header class="dialog-header">
      <h3>Publish a Vine</h3>
      <button type="button" class="close-btn" onclick={onClose} aria-label="Close">✕</button>
    </header>

    <form class="dialog-body" onsubmit={(e) => { e.preventDefault(); handleSubmit(); }}>
      <label class="field">
        <span class="field-label">Video CID</span>
        <input
          type="text"
          bind:value={videoCid}
          bind:this={cidInput}
          placeholder="Hex-encoded content ID"
          class="field-input"
          class:field-error={!!error}
        />
        {#if error}
          <span class="error-text" role="alert">{error}</span>
        {/if}
      </label>

      <label class="field">
        <span class="field-label">Title <span class="optional">(optional)</span></span>
        <input
          type="text"
          bind:value={title}
          placeholder="Short description"
          maxlength={140}
          class="field-input"
        />
        <span class="char-count">{title.length}/140</span>
      </label>

      <div class="dialog-actions">
        <button type="button" class="btn btn-secondary" onclick={onClose}>Cancel</button>
        <button type="submit" class="btn btn-primary">Publish</button>
      </div>
    </form>
  </div>
</div>

<style>
  .dialog-overlay {
    position: fixed;
    inset: 0;
    z-index: 100;
    background: rgba(0, 0, 0, 0.7);
    display: flex;
    align-items: center;
    justify-content: center;
  }

  .dialog-card {
    background: var(--bg-secondary);
    border-radius: 12px;
    width: 100%;
    max-width: 420px;
    box-shadow: 0 8px 32px rgba(0, 0, 0, 0.4);
  }

  .dialog-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 16px 20px 12px;
    border-bottom: 1px solid var(--bg-tertiary);
  }

  .dialog-header h3 {
    color: var(--text-primary);
    font-size: 1rem;
    font-weight: 600;
    margin: 0;
  }

  .close-btn {
    background: none;
    border: none;
    color: var(--text-muted);
    font-size: 1rem;
    cursor: pointer;
    padding: 4px 8px;
    border-radius: 4px;
  }

  .close-btn:hover {
    color: var(--text-primary);
    background: var(--bg-tertiary);
  }

  .dialog-body {
    padding: 16px 20px 20px;
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .field-label {
    color: var(--text-secondary);
    font-size: 0.8rem;
    font-weight: 500;
  }

  .optional {
    color: var(--text-muted);
    font-weight: 400;
  }

  .field-input {
    background: var(--bg-primary);
    border: 1px solid var(--bg-tertiary);
    border-radius: 6px;
    color: var(--text-primary);
    font-size: 0.85rem;
    padding: 8px 10px;
    outline: none;
    transition: border-color 0.15s;
  }

  .field-input:focus {
    border-color: var(--accent);
  }

  .field-input.field-error {
    border-color: #ed4245;
  }

  .error-text {
    color: #ed4245;
    font-size: 0.75rem;
  }

  .char-count {
    color: var(--text-muted);
    font-size: 0.7rem;
    text-align: right;
  }

  .dialog-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 4px;
  }

  .btn {
    border: none;
    border-radius: 6px;
    font-size: 0.85rem;
    font-weight: 500;
    padding: 8px 18px;
    cursor: pointer;
    transition: opacity 0.15s;
  }

  .btn:hover {
    opacity: 0.85;
  }

  .btn-primary {
    background: var(--accent);
    color: white;
  }

  .btn-secondary {
    background: var(--bg-tertiary);
    color: var(--text-secondary);
  }
</style>
