<script lang="ts">
  import { onMount } from 'svelte';

  let { onPublish, onClose, onPickVideo }: {
    onPublish: (videoCid: string, title?: string) => Promise<void> | void;
    onClose: () => void;
    /**
     * ZEB-559: open a native file picker, ingest the chosen video into CAS, and
     * resolve to its minted Video CID + display filename (or `null` if the user
     * cancelled the picker). Owned by App.svelte (which holds the Tauri adapter
     * + vineService) so this dialog stays presentational + testable. Optional so
     * the Advanced "paste a CID" path still works when no picker is wired.
     */
    onPickVideo?: () => Promise<{ cid: string; fileName: string } | null>;
  } = $props();

  let videoCid = $state('');
  let title = $state('');
  let error = $state('');
  let publishing = $state(false);
  let ingesting = $state(false);
  /** Name of the chosen file, shown once a video has been picked + ingested. */
  let pickedFileName = $state('');
  let showAdvanced = $state(false);
  let chooseBtn = $state<HTMLButtonElement | null>(null);

  // Focus the primary action (Choose video) on open so keyboard users land on it.
  onMount(() => chooseBtn?.focus());

  async function handleChooseVideo() {
    if (!onPickVideo || ingesting || publishing) return;
    error = '';
    ingesting = true;
    try {
      const result = await onPickVideo();
      if (!result) return; // user cancelled the picker — leave state as-is
      videoCid = result.cid;
      pickedFileName = result.fileName;
    } catch (err) {
      error = err instanceof Error ? err.message : 'Could not process the video';
    } finally {
      ingesting = false;
    }
  }

  async function handleSubmit() {
    const cid = videoCid.trim();
    if (!cid) {
      error = 'Choose a video first (or paste a Video CID under Advanced)';
      return;
    }
    if (publishing) return;
    error = '';
    publishing = true;
    try {
      await onPublish(cid, title.trim() || undefined);
      videoCid = '';
      title = '';
      pickedFileName = '';
      onClose();
    } catch (err) {
      error = err instanceof Error ? err.message : 'Publish failed';
    } finally {
      publishing = false;
    }
  }

  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === 'Escape') onClose();
  }

  function handleOverlayClick(e: MouseEvent) {
    if (e.target === e.currentTarget) onClose();
  }
</script>

<svelte:window onkeydown={handleKeyDown} />

<div class="dialog-overlay" role="presentation" onclick={handleOverlayClick}>
  <div class="dialog-card" role="dialog" aria-label="Publish vine" aria-modal="true">
    <header class="dialog-header">
      <h3>Publish a Vine</h3>
      <button type="button" class="close-btn" onclick={onClose} aria-label="Close">✕</button>
    </header>

    <form class="dialog-body" onsubmit={(e) => { e.preventDefault(); handleSubmit(); }}>
      <div class="field">
        <span class="field-label">Video</span>
        {#if pickedFileName}
          <div class="picked-file" data-testid="picked-file">
            <span class="picked-check" aria-hidden="true">✓</span>
            <span class="picked-name" title={pickedFileName}>{pickedFileName}</span>
            <button
              type="button"
              class="link-btn"
              onclick={handleChooseVideo}
              disabled={ingesting || publishing}
            >Change</button>
          </div>
        {:else}
          <button
            type="button"
            class="choose-video-btn"
            bind:this={chooseBtn}
            onclick={handleChooseVideo}
            disabled={ingesting || publishing}
            data-testid="choose-video"
          >
            {#if ingesting}
              <span class="spinner" aria-hidden="true"></span>Processing video…
            {:else}
              <span aria-hidden="true">🎬</span> Choose video…
            {/if}
          </button>
          <span class="field-hint">Pick a local video file (max 100&nbsp;MB).</span>
        {/if}
        {#if error}
          <span class="error-text" role="alert">{error}</span>
        {/if}
      </div>

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

      <details class="advanced" bind:open={showAdvanced}>
        <summary class="advanced-summary">Advanced: paste a Video CID</summary>
        <input
          type="text"
          bind:value={videoCid}
          oninput={() => { pickedFileName = ''; }}
          placeholder="Hex-encoded content ID"
          class="field-input advanced-input"
          aria-label="Video CID"
        />
        <span class="field-hint">For a content ID you already have (e.g. from the headless API).</span>
      </details>

      <div class="dialog-actions">
        <button type="button" class="btn btn-secondary" onclick={onClose}>Cancel</button>
        <button
          type="submit"
          class="btn btn-primary"
          disabled={publishing || ingesting || !videoCid.trim()}
        >{publishing ? 'Publishing…' : 'Publish'}</button>
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

  .field-hint {
    color: var(--text-muted);
    font-size: 0.72rem;
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

  /* ── ZEB-559 file picker ─────────────────────────────────────────── */
  .choose-video-btn {
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 8px;
    background: var(--bg-primary);
    border: 1px dashed var(--accent);
    border-radius: 6px;
    color: var(--text-primary);
    font-size: 0.85rem;
    font-weight: 500;
    padding: 12px;
    cursor: pointer;
    transition: background 0.15s, opacity 0.15s;
  }

  .choose-video-btn:hover:not(:disabled) {
    background: var(--bg-tertiary);
  }

  .choose-video-btn:disabled {
    opacity: 0.7;
    cursor: default;
  }

  .picked-file {
    display: flex;
    align-items: center;
    gap: 8px;
    background: var(--bg-primary);
    border: 1px solid var(--bg-tertiary);
    border-radius: 6px;
    padding: 8px 10px;
  }

  .picked-check {
    color: #3ba55d;
    font-weight: 700;
  }

  .picked-name {
    flex: 1;
    color: var(--text-primary);
    font-size: 0.85rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .link-btn {
    background: none;
    border: none;
    color: var(--accent);
    font-size: 0.78rem;
    cursor: pointer;
    padding: 2px 4px;
  }

  .link-btn:disabled {
    opacity: 0.6;
    cursor: default;
  }

  .spinner {
    width: 13px;
    height: 13px;
    border: 2px solid var(--text-muted);
    border-top-color: var(--accent);
    border-radius: 50%;
    animation: spin 0.7s linear infinite;
  }

  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }

  /* ── ZEB-559 advanced raw-CID disclosure ─────────────────────────── */
  .advanced {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .advanced-summary {
    color: var(--text-muted);
    font-size: 0.76rem;
    cursor: pointer;
    user-select: none;
  }

  .advanced-input {
    margin-top: 4px;
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

  .btn-primary:disabled {
    opacity: 0.55;
    cursor: default;
  }

  .btn-secondary {
    background: var(--bg-tertiary);
    color: var(--text-secondary);
  }
</style>
