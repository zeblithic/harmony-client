<!--
  ZEB-166: grid sibling of FilePlaceholderRow — same contract, card
  layout. See FilePlaceholderRow.svelte for the rationale.
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
      e.preventDefault();
      onCommit?.();
    } else if (e.key === 'Escape') {
      e.preventDefault();
      onCancel?.();
    }
  }
</script>

<div class="file-card" role="gridcell">
  <div class="file-card-thumbnail" aria-hidden="true">
    <span class="file-card-icon">{icon}</span>
  </div>
  <div class="file-card-info">
    <input
      class="file-card-name-input"
      type="text"
      bind:value
      onkeydown={handleEditKey}
      onblur={() => { if (!inFlight) onCancel?.(); }}
      use:autoFocus
    />
    {#if error}
      <span class="file-card-error" role="alert">{error}</span>
    {/if}
  </div>
</div>

<style>
  .file-card {
    position: relative;
    display: flex;
    flex-direction: column;
    min-width: 140px;
    background: var(--bg-secondary, #2b2d31);
    border-radius: 8px;
    padding: 8px;
    border: 2px solid transparent;
  }

  .file-card-thumbnail {
    display: flex;
    align-items: center;
    justify-content: center;
    height: 64px;
    margin-bottom: 8px;
  }

  .file-card-icon {
    font-size: 2rem;
  }

  .file-card-info {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }

  .file-card-name-input {
    font: inherit;
    font-size: 0.85rem;
    color: inherit;
    background: transparent;
    border: 1px solid var(--accent, #5865f2);
    border-radius: 3px;
    padding: 2px 4px;
    min-width: 0;
    width: 100%;
    box-sizing: border-box;
  }

  .file-card-error {
    font-size: 0.75rem;
    color: var(--danger, #f23f42);
  }
</style>
