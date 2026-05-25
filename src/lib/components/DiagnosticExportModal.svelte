<script lang="ts">
  /**
   * ZEB-329 — Diagnostic export modal (spec §5.4 + §7.4).
   *
   * Default: redacted markdown (server-side via include_full_ids=false).
   * Toggle "Include full identifiers" → re-fetch with include_full_ids=true.
   * Copy → navigator.clipboard.writeText(markdown).
   * Save → Tauri dialog plugin save() → write file.
   */
  import { exportPayload } from '../network-health-adapter';
  import { save as saveDialog } from '@tauri-apps/plugin-dialog';
  import { writeTextFile } from '@tauri-apps/plugin-fs';

  interface Props {
    onClose: () => void;
  }
  const { onClose }: Props = $props();

  let includeFullIds = $state(false);
  let markdown = $state<string>('');
  let loading = $state(true);
  let error = $state<string | null>(null);
  let copiedToast = $state(false);
  // PR #161 R1 (CodeRabbit Major): monotonic request counter. Rapid
  // toggling of `includeFullIds` can overlap `load()` invocations; a
  // slower older `exportPayload` response could otherwise finish last
  // and overwrite the current redaction state. We capture the
  // requestId at call time and only assign state when it still
  // matches the latest issued id.
  //
  // NOT `$state`: this is internal control-flow bookkeeping, not UI
  // state. Wrapping in `$state` would cause the `$effect` below to
  // re-fire on every `load()` (effect_update_depth_exceeded).
  let latestRequest = 0;

  async function load(full: boolean): Promise<void> {
    const requestId = ++latestRequest;
    loading = true;
    error = null;
    try {
      const result = await exportPayload(full);
      if (requestId !== latestRequest) return; // superseded by a newer load()
      markdown = result;
    } catch (e) {
      if (requestId !== latestRequest) return;
      error = e instanceof Error ? e.message : String(e);
    } finally {
      if (requestId === latestRequest) loading = false;
    }
  }

  // Re-fetch whenever includeFullIds toggles. Pass the value explicitly
  // so $effect's dependency tracking captures it (reading `markdown` /
  // `loading` inside `load` would otherwise trigger a feedback loop).
  $effect(() => {
    void load(includeFullIds);
  });

  async function copy(): Promise<void> {
    try {
      await navigator.clipboard.writeText(markdown);
      copiedToast = true;
      setTimeout(() => (copiedToast = false), 2000);
    } catch (e) {
      error = `Couldn't copy: ${e instanceof Error ? e.message : String(e)}. Use Save instead.`;
    }
  }

  async function saveToFile(): Promise<void> {
    try {
      const path = await saveDialog({
        defaultPath: 'harmony-diagnostics.txt',
        filters: [{ name: 'Text', extensions: ['txt'] }],
      });
      if (path) {
        await writeTextFile(path, markdown);
      }
      // Cancel → no-op silent dismiss
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }
</script>

<div class="modal-backdrop" data-testid="export-modal">
  <div class="modal-content">
    <h2>Diagnostic export</h2>
    <p>Review what you're about to share:</p>
    {#if loading}
      <p>Loading…</p>
    {:else if error}
      <p class="error" data-testid="export-error">{error}</p>
    {:else}
      <pre class="markdown-preview" data-testid="export-preview">{markdown}</pre>
    {/if}
    <label>
      <input
        type="checkbox"
        bind:checked={includeFullIds}
        data-testid="export-full-toggle"
      />
      Include full identifiers (default off)
    </label>
    <div class="actions">
      <button onclick={copy} disabled={loading} data-testid="export-copy">Copy</button>
      <button onclick={saveToFile} disabled={loading} data-testid="export-save">Save as .txt</button>
      <button onclick={onClose} data-testid="export-cancel">Cancel</button>
    </div>
    {#if copiedToast}
      <p class="toast">Copied!</p>
    {/if}
  </div>
</div>

<style>
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }
  .modal-content {
    background: var(--bg-secondary, #2a2a2a);
    color: var(--text-primary, #fff);
    padding: 1.5rem;
    border-radius: 8px;
    max-width: 640px;
    max-height: 80vh;
    overflow-y: auto;
  }
  .markdown-preview {
    background: #111;
    color: #fff;
    padding: 1rem;
    border-radius: 4px;
    max-height: 320px;
    overflow-y: auto;
    white-space: pre-wrap;
  }
  .actions {
    display: flex;
    gap: 0.5rem;
    margin-top: 1rem;
  }
  .error {
    color: crimson;
  }
  .toast {
    color: lightgreen;
    margin-top: 0.5rem;
  }
</style>
