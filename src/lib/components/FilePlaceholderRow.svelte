<!--
  ZEB-166: inline "new folder" placeholder row at the top of FileList.
  Sibling to FileRow.svelte's edit-mode input. Stateless — controlled
  by FileBrowser via the value/error/inFlight props.
-->
<script lang="ts">
  import { categoryIcon } from '../file-utils';
  import { autoFocus } from '../actions/auto-focus';

  let {
    value = $bindable(''),
    error = null,
    inFlight = false,
    onCommit,
    onCancel,
  }: {
    value: string;
    error?: string | null;
    inFlight?: boolean;
    onCommit?: () => void;
    onCancel?: () => void;
  } = $props();

  let icon = categoryIcon('bundle');

  function handleEditKey(e: KeyboardEvent) {
    if (e.key === 'Enter') {
      // Gate Enter on !inFlight so repeated keystrokes during the
      // commit await don't fire duplicate create IPCs (one click of
      // Enter should create exactly one folder, not N).
      if (inFlight) return;
      e.preventDefault();
      onCommit?.();
    } else if (e.key === 'Escape') {
      e.preventDefault();
      onCancel?.();
    }
  }
</script>

<div class="file-row" role="row">
  <span class="file-row-icon" aria-hidden="true">{icon}</span>
  <input
    class="file-row-name-input"
    type="text"
    aria-label="Folder name"
    bind:value
    onkeydown={handleEditKey}
    onblur={() => { if (!inFlight) onCancel?.(); }}
    use:autoFocus
  />
</div>
{#if error}
  <div class="file-row-error" role="alert">{error}</div>
{/if}

<style>
  .file-row {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 6px 12px;
    width: 100%;
  }

  .file-row-icon {
    flex-shrink: 0;
    width: 24px;
    text-align: center;
  }

  .file-row-name-input {
    flex: 1;
    min-width: 0;
    font: inherit;
    color: inherit;
    background: transparent;
    border: 1px solid var(--accent, #5865f2);
    border-radius: 3px;
    padding: 2px 4px;
  }

  .file-row-error {
    padding: 2px 12px 6px 48px;
    font-size: 0.8rem;
    color: var(--danger, #f23f42);
  }
</style>
