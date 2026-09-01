<script lang="ts">
  import type { ContentDetail } from '../types';
  import ConfirmDialog from './ConfirmDialog.svelte';

  let {
    item,
    onBurn,
    onArchive,
    onPin,
    onUnpin,
    onExport,
  }: {
    item: ContentDetail;
    onBurn?: () => void;
    onArchive?: () => void;
    onPin?: () => void;
    onUnpin?: () => void;
    onExport?: () => void;
  } = $props();

  let showBurnConfirm = $state(false);

  function confirmBurn() {
    showBurnConfirm = false;
    onBurn?.();
  }
</script>

<div class="file-actions">
  <button class="action-btn destructive" onclick={() => { showBurnConfirm = true; }} aria-label="Burn">
    <span class="action-icon" aria-hidden="true">{'\u{1F525}'}</span>
    Burn
  </button>

  <button class="action-btn" onclick={() => onArchive?.()} aria-label="Archive">
    <span class="action-icon" aria-hidden="true">{'\u{1F4E6}'}</span>
    Archive
  </button>

  {#if item.pinned}
    <button class="action-btn" onclick={() => onUnpin?.()} aria-label="Unpin">
      <span class="action-icon" aria-hidden="true">{'\u{1F4CC}'}</span>
      Unpin
    </button>
  {:else}
    <button class="action-btn" onclick={() => onPin?.()} aria-label="Pin">
      <span class="action-icon" aria-hidden="true">{'\u{1F4CC}'}</span>
      Pin
    </button>
  {/if}

  <button class="action-btn" onclick={() => onExport?.()} aria-label="Export">
    <span class="action-icon" aria-hidden="true">{'\u{1F4E4}'}</span>
    Export
  </button>
</div>

{#if showBurnConfirm}
  <ConfirmDialog
    title="Burn Content"
    message="This will permanently delete &quot;{item.name}&quot; and free its storage quota. This cannot be undone."
    confirmLabel="Burn"
    destructive={true}
    onConfirm={confirmBurn}
    onCancel={() => { showBurnConfirm = false; }}
  />
{/if}

<style>
  .file-actions {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .action-btn {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    padding: 8px;
    border-radius: 4px;
    border: none;
    background: var(--bg-tertiary);
    color: var(--text-primary);
    font-size: 0.85rem;
    cursor: pointer;
    font: inherit;
    text-align: left;
  }

  .action-btn:hover {
    background: var(--bg-tertiary-hover);
    color: var(--accent);
  }

  .action-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  .action-btn.destructive {
    background: var(--danger-muted);
    color: var(--text-bright);
  }

  .action-btn.destructive:hover {
    background: #c23234;
    color: var(--text-bright);
  }

  .action-icon {
    flex-shrink: 0;
    width: 1.2em;
    text-align: center;
  }
</style>
