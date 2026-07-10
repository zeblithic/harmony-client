<script lang="ts">
  import { onMount } from 'svelte';
  import { probeVideoDuration } from '../video-metadata';

  let { onPublish, onClose, onPickVideo, resolveVideo, probeDuration = probeVideoDuration }: {
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
    /** Resolve a CID to a blob URL — enables the honest ≤6s duration gate. */
    resolveVideo?: (cid: string) => Promise<string>;
    /** Injectable metadata probe (tests stub it; jsdom fires no media events). */
    probeDuration?: (url: string) => Promise<number>;
  } = $props();

  /** Honest-client cap (spec §3): vines loop at six seconds or less. */
  const MAX_VINE_SECONDS = 6;

  let videoCid = $state('');
  let title = $state('');
  let error = $state('');
  let publishing = $state(false);
  let ingesting = $state(false);
  /** Name of the chosen file, shown once a video has been picked + ingested. */
  let pickedFileName = $state('');
  /** Measured metadata duration of the current CID; null = unmeasured. */
  let pickedDuration = $state<number | null>(null);
  let showAdvanced = $state(false);
  let chooseBtn = $state<HTMLButtonElement | null>(null);

  let overLimit = $derived(pickedDuration != null && pickedDuration > MAX_VINE_SECONDS);

  // Focus the primary action (Choose video) on open so keyboard users land on it.
  onMount(() => chooseBtn?.focus());

  function overLimitCopy(seconds: number): string {
    return `This clip is ${seconds.toFixed(1)}s — vines are 6 seconds or less. Trim it and re-ingest.`;
  }

  /**
   * Honest ≤6s gate (ZEB-612 S2): resolve the CID to a blob and read the
   * container's metadata duration. Same honest-client posture as voice
   * moderation — the network never enforces this; our client refuses to
   * publish dishonest data. Fail-OPEN on measurement failure (no resolver
   * wired, fetch failed, undecodable container): the gate is an honesty
   * courtesy, not security, and blocking legitimate publishes over a probe
   * failure would make nothing truer. Unmeasured clips show no ✓/duration.
   */
  async function measureDuration(cid: string): Promise<number | null> {
    if (!resolveVideo) return null;
    let url: string | null = null;
    try {
      url = await resolveVideo(cid);
      return await probeDuration(url);
    } catch {
      return null;
    } finally {
      if (url) URL.revokeObjectURL?.(url);
    }
  }

  async function handleChooseVideo() {
    if (!onPickVideo || ingesting || publishing) return;
    error = '';
    ingesting = true;
    try {
      const result = await onPickVideo();
      if (!result) return; // user cancelled the picker — leave state as-is
      videoCid = result.cid;
      pickedFileName = result.fileName;
      // Clear the previous clip's measurement before the await — the picked
      // block stays rendered while ingesting, and a stale duration would
      // show the new filename with the old clip's (possibly over-limit)
      // state (CodeRabbit PR #440).
      pickedDuration = null;
      pickedDuration = await measureDuration(result.cid);
      if (pickedDuration != null && pickedDuration > MAX_VINE_SECONDS) {
        error = overLimitCopy(pickedDuration);
      }
    } catch (err) {
      // Production Tauri rejections are strings (e.g. "video too large: …");
      // preserve them rather than collapsing to a generic message.
      error = err instanceof Error ? err.message : String(err);
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
    if (publishing || ingesting) return;
    error = '';
    publishing = true;
    try {
      // Pasted/Advanced CIDs haven't been measured yet — gate at submit.
      if (pickedDuration == null) {
        pickedDuration = await measureDuration(cid);
      }
      if (pickedDuration != null && pickedDuration > MAX_VINE_SECONDS) {
        error = overLimitCopy(pickedDuration);
        return;
      }
      await onPublish(cid, title.trim() || undefined);
      videoCid = '';
      title = '';
      pickedFileName = '';
      pickedDuration = null;
      onClose();
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
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
  <div class="dialog-card" role="dialog" aria-label="Share a vine" aria-modal="true">
    <header class="dialog-header">
      <span class="header-glyph" aria-hidden="true">🎞</span>
      <div class="header-text">
        <h3>Share a vine</h3>
        <p class="header-sub">≤ 6 seconds · loops forever</p>
      </div>
      <button type="button" class="close-btn" onclick={onClose} aria-label="Close">✕</button>
    </header>

    <form class="dialog-body" onsubmit={(e) => { e.preventDefault(); handleSubmit(); }}>
      <div class="field">
        <span class="field-label">Video</span>
        {#if onPickVideo}
          {#if pickedFileName}
            <div class="picked-file" class:over-limit={overLimit} data-testid="picked-file">
              <span class="picked-name" title={pickedFileName}>
                {#if pickedDuration != null && !overLimit}
                  {pickedFileName} · {pickedDuration.toFixed(1)}s ✓ · ingested to content store
                {:else if overLimit}
                  {pickedFileName} · {pickedDuration?.toFixed(1)}s
                {:else}
                  {pickedFileName} · ingested to content store
                {/if}
              </span>
              <button
                type="button"
                class="link-btn"
                onclick={handleChooseVideo}
                disabled={ingesting || publishing}
              >Replace clip</button>
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
        {:else}
          <!-- No native picker (web/dev or pre-connect): manual CID is the
               primary path, so there's no dead "Choose video…" control. -->
          <input
            type="text"
            bind:value={videoCid}
            oninput={() => { pickedFileName = ''; pickedDuration = null; }}
            placeholder="Hex-encoded content ID"
            class="field-input"
            aria-label="Video CID"
          />
          <span class="field-hint">Paste a Video CID (file picker unavailable here).</span>
        {/if}
        {#if error}
          <span class="error-text" role="alert">{error}</span>
        {/if}
      </div>

      <label class="field">
        <span class="field-label">Caption <span class="optional">(optional)</span></span>
        <input
          type="text"
          bind:value={title}
          placeholder="Short description"
          maxlength={140}
          class="field-input"
        />
        <span class="char-count">{title.length}/140</span>
      </label>

      {#if onPickVideo}
        <details class="advanced" bind:open={showAdvanced}>
          <summary class="advanced-summary">Advanced: paste a Video CID</summary>
          <input
            type="text"
            bind:value={videoCid}
            oninput={() => { pickedFileName = ''; pickedDuration = null; }}
            placeholder="Hex-encoded content ID"
            class="field-input advanced-input"
            aria-label="Video CID"
          />
          <span class="field-hint">For a content ID you already have (e.g. from the headless API).</span>
        </details>
      {/if}

      <div class="sovereign-note">
        <span aria-hidden="true">🔑</span>
        <p>Publishes to your sovereign identity and replicates peer-to-peer. There's no central server to take it down.</p>
      </div>

      <div class="dialog-actions">
        <button type="button" class="btn btn-secondary" onclick={onClose}>Cancel</button>
        <button
          type="submit"
          class="btn btn-primary"
          disabled={publishing || ingesting || !videoCid.trim() || overLimit}
        >{publishing ? 'Publishing…' : 'Publish vine'}</button>
      </div>
    </form>
  </div>
</div>

<style>
  .dialog-overlay {
    position: fixed;
    inset: 0;
    z-index: 100;
    background: var(--overlay);
    display: flex;
    align-items: center;
    justify-content: center;
  }

  .dialog-card {
    background: var(--bg-secondary);
    border-radius: 8px;
    width: 100%;
    max-width: 420px;
    box-shadow: 0 8px 32px var(--shadow-strong);
  }

  .dialog-header {
    display: flex;
    align-items: center;
    gap: 11px;
    padding: 16px 20px 12px;
    border-bottom: 1px solid var(--bg-tertiary);
  }

  .header-glyph {
    width: 34px;
    height: 34px;
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--primary-soft);
    border-radius: 8px;
    font-size: 1rem;
    flex-shrink: 0;
  }

  .header-text {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .dialog-header h3 {
    color: var(--text-primary);
    font-size: 1rem;
    font-weight: 600;
    margin: 0;
  }

  .header-sub {
    color: var(--text-muted);
    font-size: 0.72rem;
    font-weight: 400;
    margin: 0;
  }

  .close-btn {
    margin-left: auto;
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
    border-radius: 5px;
    color: var(--text-primary);
    font-size: 0.85rem;
    padding: 8px 10px;
    outline: none;
    transition: border-color 0.15s;
  }

  .field-input:focus {
    border-color: var(--accent);
  }

  /* ── ZEB-559 file picker (ZEB-612 S2: Commons drop-zone treatment) ── */
  .choose-video-btn {
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 8px;
    background: var(--primary-soft);
    border: 1.5px dashed var(--primary-border);
    border-radius: 8px;
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
    border-radius: 5px;
    padding: 8px 10px;
  }

  .picked-file.over-limit {
    border-color: var(--danger);
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

  /* ── ZEB-612 S2: true-claims-only sovereign note ─────────────────── */
  .sovereign-note {
    display: flex;
    gap: 9px;
    align-items: flex-start;
    background: var(--primary-soft);
    border: 1px solid var(--primary-border);
    border-radius: 8px;
    padding: 10px 12px;
  }

  .sovereign-note p {
    margin: 0;
    color: var(--text-secondary);
    font-size: 0.75rem;
    line-height: 1.5;
  }

  .error-text {
    color: var(--text-danger);
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
    border-radius: 5px;
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
    color: var(--on-accent);
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
