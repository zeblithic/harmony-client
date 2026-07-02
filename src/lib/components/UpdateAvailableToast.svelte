<script lang="ts">
  import type { Update } from "@tauri-apps/plugin-updater";
  import { dismissVersion } from "../updater-adapter";

  interface Props {
    update: Pick<Update, "version" | "downloadAndInstall">;
    onDismiss: () => void;
  }

  const { update, onDismiss }: Props = $props();
  let applying = $state(false);
  let applyError = $state<string | null>(null);

  async function restart() {
    applying = true;
    applyError = null;
    try {
      await update.downloadAndInstall();
      // downloadAndInstall restarts the app; we normally never reach here.
    } catch (e) {
      applyError = e instanceof Error ? e.message : String(e);
    } finally {
      // ZEB-328 PR #160 R3 (Greptile): always reset `applying` so the
      // toast doesn't freeze if downloadAndInstall resolves without
      // triggering the OS-level restart (test mocks, edge cases where
      // the restart silently fails, etc.). Without this, the user gets
      // a permanently-disabled toast with no path forward.
      applying = false;
    }
  }

  function later() {
    onDismiss();
  }

  function skip() {
    dismissVersion(update.version);
    onDismiss();
  }
</script>

<div class="update-toast" role="status" aria-live="polite">
  <div class="title">Harmony v{update.version} is available.</div>
  {#if applyError}
    <div class="error">Update could not be applied: {applyError}</div>
  {/if}
  <div class="actions">
    <button onclick={restart} disabled={applying}>
      {applying ? "Applying…" : "Restart to update"}
    </button>
    <button onclick={later} disabled={applying}>Later</button>
    <button onclick={skip} disabled={applying}>Skip this version</button>
  </div>
</div>

<style>
  .update-toast {
    position: fixed;
    bottom: 1rem;
    right: 1rem;
    max-width: 380px;
    padding: 0.75rem 1rem;
    background: var(--bg-secondary);
    color: var(--text-primary);
    border-radius: 6px;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.3);
    z-index: 1000;
  }
  .title { font-weight: 600; margin-bottom: 0.5rem; }
  .error {
    color: var(--fg-error, #ff6b6b);
    font-size: 0.875rem;
    margin-bottom: 0.5rem;
  }
  .actions { display: flex; gap: 0.5rem; flex-wrap: wrap; }
  .actions button {
    padding: 0.375rem 0.75rem;
    border-radius: 4px;
    border: 1px solid var(--border-default, #555);
    background: transparent;
    color: inherit;
    cursor: pointer;
    font-size: 0.875rem;
  }
  .actions button:hover:not(:disabled) {
    background: var(--bg-hover);
  }
  .actions button:disabled { opacity: 0.5; cursor: not-allowed; }
</style>
