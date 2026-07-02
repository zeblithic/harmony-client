<script lang="ts">
  import type { ContentItem } from '../types';
  import { categoryIcon, formatBytes, sensitivityIcon } from '../file-utils';
  import { autoFocus } from '../actions/auto-focus';
  import StalenessIndicator from './StalenessIndicator.svelte';

  let {
    item,
    onClick,
    selected = false,
    onRowDragStart,
    onRowDrop,
    editing = false,
    editValue = $bindable(''),
    renameInFlight = false,
    onBeginRename,
    onCommitRename,
    onCancelRename,
  }: {
    item: ContentItem;
    onClick?: (item: ContentItem) => void;
    selected?: boolean;
    /** ZEB-162: drag-drop handlers wired by FileGrid → FileBrowser. */
    onRowDragStart?: (e: DragEvent, item: ContentItem) => void;
    onRowDrop?: (e: DragEvent, targetCid: string, targetSidecarId: string | null) => void;
    /** ZEB-299: inline rename. See FileRow.svelte for the contract. */
    editing?: boolean;
    editValue?: string;
    /** Round 5: see FileRow.svelte — suppresses blur-cancel while
     *  commitRename's IPC is in flight so a failure keeps the input
     *  visible for retry. */
    renameInFlight?: boolean;
    onBeginRename?: (item: ContentItem) => void;
    onCommitRename?: () => void;
    onCancelRename?: () => void;
  } = $props();

  let icon = $derived(categoryIcon(item.category));
  let size = $derived(formatBytes(item.sizeBytes));
  let sensIcon = $derived(sensitivityIcon(item.sensitivity));

  // ZEB-299 slow-click state — per-card so a slow-click on one card
  // doesn't leak to another. Same 300–800ms gap as FileRow.
  let lastNameClickAt = 0;

  function handleDragOver(e: DragEvent) {
    if (!item.isFolder) return;
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = 'move';
  }

  function handleNameClick(e: MouseEvent) {
    if (editing) return;
    if (!selected) return;
    const now = performance.now();
    const gap = now - lastNameClickAt;
    lastNameClickAt = now;
    if (gap >= 300 && gap <= 800) {
      e.stopPropagation();
      onBeginRename?.(item);
    }
  }

  function handleEditKey(e: KeyboardEvent) {
    if (e.key === 'Enter') {
      e.preventDefault();
      onCommitRename?.();
    } else if (e.key === 'Escape') {
      e.preventDefault();
      onCancelRename?.();
    }
  }
</script>

<button
  class="file-card"
  class:selected
  draggable={editing ? 'false' : 'true'}
  onclick={() => onClick?.(item)}
  ondragstart={editing ? undefined : (e) => onRowDragStart?.(e, item)}
  ondragover={editing ? undefined : handleDragOver}
  ondrop={!editing && item.isFolder ? (e) => onRowDrop?.(e, item.cid, item.sidecarId ?? null) : undefined}
  aria-label={item.name}
>
  <div class="file-card-overlay-top">
    <span class="file-card-sensitivity" aria-hidden="true">{sensIcon}</span>
    <span class="file-card-staleness">
      <StalenessIndicator score={item.stalenessScore} pinned={item.pinned} />
    </span>
  </div>
  <div class="file-card-thumbnail" aria-hidden="true">
    <span class="file-card-icon">{icon}</span>
  </div>
  <div class="file-card-info">
    {#if editing}
      <input
        class="file-card-name-input"
        type="text"
        bind:value={editValue}
        onkeydown={handleEditKey}
        onblur={() => { if (!renameInFlight) onCancelRename?.(); }}
        onclick={(e) => e.stopPropagation()}
        use:autoFocus
      />
    {:else}
      <span
        class="file-card-name"
        onclick={handleNameClick}
        role="presentation"
      >{item.name}</span>
    {/if}
    <span class="file-card-size">{size}</span>
  </div>
</button>

<style>
  .file-card {
    position: relative;
    display: flex;
    flex-direction: column;
    min-width: 140px;
    background: var(--bg-secondary);
    border-radius: 8px;
    padding: 8px;
    border: 2px solid transparent;
    cursor: pointer;
    font: inherit;
    color: inherit;
    text-align: left;
  }

  .file-card:hover {
    background: var(--bg-tertiary);
  }

  .file-card:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  .file-card.selected {
    border-color: var(--accent);
  }

  .file-card-overlay-top {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 4px;
  }

  .file-card-sensitivity {
    font-size: 0.75rem;
  }

  .file-card-staleness {
    display: flex;
    align-items: center;
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

  .file-card-name {
    font-size: 0.85rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .file-card-name-input {
    font: inherit;
    font-size: 0.85rem;
    color: inherit;
    background: transparent;
    border: 1px solid var(--accent);
    border-radius: 3px;
    padding: 2px 4px;
    min-width: 0;
    width: 100%;
    box-sizing: border-box;
  }

  .file-card-size {
    font-size: 0.75rem;
    color: var(--text-muted);
  }
</style>
