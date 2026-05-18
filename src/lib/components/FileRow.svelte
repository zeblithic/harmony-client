<script lang="ts">
  import type { ContentItem } from '../types';
  import { categoryIcon, formatBytes, relativeTime, tierTarget, sensitivityIcon } from '../file-utils';
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
    onBeginRename,
    onCommitRename,
    onCancelRename,
  }: {
    item: ContentItem;
    onClick?: (item: ContentItem) => void;
    selected?: boolean;
    /** ZEB-162: drag-drop handlers wired by FileList → FileBrowser. */
    onRowDragStart?: (e: DragEvent, item: ContentItem) => void;
    onRowDrop?: (e: DragEvent, targetCid: string, targetSidecarId: string | null) => void;
    /** ZEB-299: inline rename. `editing` swaps the name span for an
     *  input; `editValue` two-way-binds the text back to FileBrowser. */
    editing?: boolean;
    editValue?: string;
    onBeginRename?: (item: ContentItem) => void;
    onCommitRename?: () => void;
    onCancelRename?: () => void;
  } = $props();

  let icon = $derived(categoryIcon(item.category));
  let size = $derived(formatBytes(item.sizeBytes));
  let lastAccessed = $derived(relativeTime(item.lastAccessed));
  let replication = $derived(`${item.replicaCount}/${tierTarget(item.replicationTier)}`);
  let sensIcon = $derived(sensitivityIcon(item.sensitivity));

  // ZEB-299 slow-click detection. Per-row state — each row tracks its
  // own name-click cadence so a slow-click on one row doesn't bleed
  // into another. The 300–800ms gap window sits above the browser
  // double-click threshold (~250ms) and below the "two separate
  // clicks" interpretation.
  let lastNameClickAt = 0;

  function handleDragOver(e: DragEvent) {
    if (!item.isFolder) return;
    // preventDefault is required to mark this element as a valid drop target.
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = 'move';
  }

  function handleNameClick(e: MouseEvent) {
    if (editing) return;
    if (!selected) return; // only an already-selected row can slow-click rename
    const now = performance.now();
    const gap = now - lastNameClickAt;
    lastNameClickAt = now;
    if (gap >= 300 && gap <= 800) {
      // Stop the row's onclick so the slow-click doesn't also fire
      // navigation/selection.
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
  class="file-row"
  class:selected
  role="row"
  draggable={editing ? 'false' : 'true'}
  onclick={() => onClick?.(item)}
  ondragstart={editing ? undefined : (e) => onRowDragStart?.(e, item)}
  ondragover={editing ? undefined : handleDragOver}
  ondrop={!editing && item.isFolder ? (e) => onRowDrop?.(e, item.cid, item.sidecarId ?? null) : undefined}
  aria-label={item.name}
>
  <span class="file-row-icon" aria-hidden="true">{icon}</span>
  {#if editing}
    <input
      class="file-row-name-input"
      type="text"
      bind:value={editValue}
      onkeydown={handleEditKey}
      onblur={() => onCancelRename?.()}
      onclick={(e) => e.stopPropagation()}
      use:autoFocus
    />
  {:else}
    <span
      class="file-row-name"
      class:bold={selected}
      onclick={handleNameClick}
      role="presentation"
    >{item.name}</span>
  {/if}
  <span class="file-row-size">{size}</span>
  <span class="file-row-accessed">{lastAccessed}</span>
  <StalenessIndicator score={item.stalenessScore} pinned={item.pinned} />
  <span class="file-row-replication">{replication}</span>
  <span class="file-row-sensitivity" aria-hidden="true">{sensIcon}</span>
</button>

<style>
  .file-row {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 6px 12px;
    background: transparent;
    border: none;
    width: 100%;
    text-align: left;
    font: inherit;
    color: inherit;
    cursor: pointer;
  }

  .file-row:hover {
    background: var(--bg-tertiary, #232428);
  }

  .file-row:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: -2px;
  }

  .file-row.selected {
    background: color-mix(in srgb, var(--accent, #5865f2) 10%, transparent);
  }

  .file-row-icon {
    flex-shrink: 0;
    width: 24px;
    text-align: center;
  }

  .file-row-name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .file-row-name.bold {
    font-weight: 600;
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

  .file-row-size,
  .file-row-accessed,
  .file-row-replication {
    flex-shrink: 0;
    font-size: 0.8rem;
    color: var(--text-muted, #949ba4);
    min-width: 60px;
    text-align: right;
  }

  .file-row-sensitivity {
    flex-shrink: 0;
    width: 24px;
    text-align: center;
  }
</style>
