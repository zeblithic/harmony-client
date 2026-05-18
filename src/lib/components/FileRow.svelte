<script lang="ts">
  import type { ContentItem } from '../types';
  import { categoryIcon, formatBytes, relativeTime, tierTarget, sensitivityIcon } from '../file-utils';
  import StalenessIndicator from './StalenessIndicator.svelte';

  let {
    item,
    onClick,
    selected = false,
    onRowDragStart,
    onRowDrop,
  }: {
    item: ContentItem;
    onClick?: (item: ContentItem) => void;
    selected?: boolean;
    /** ZEB-162: drag-drop handlers wired by FileList → FileBrowser. */
    onRowDragStart?: (e: DragEvent, item: ContentItem) => void;
    onRowDrop?: (e: DragEvent, targetCid: string, targetSidecarId: string | null) => void;
  } = $props();

  let icon = $derived(categoryIcon(item.category));
  let size = $derived(formatBytes(item.sizeBytes));
  let lastAccessed = $derived(relativeTime(item.lastAccessed));
  let replication = $derived(`${item.replicaCount}/${tierTarget(item.replicationTier)}`);
  let sensIcon = $derived(sensitivityIcon(item.sensitivity));

  function handleDragOver(e: DragEvent) {
    if (!item.isFolder) return;
    // preventDefault is required to mark this element as a valid drop target.
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = 'move';
  }
</script>

<button
  class="file-row"
  class:selected
  role="row"
  draggable="true"
  onclick={() => onClick?.(item)}
  ondragstart={(e) => onRowDragStart?.(e, item)}
  ondragover={handleDragOver}
  ondrop={item.isFolder ? (e) => onRowDrop?.(e, item.cid, item.sidecarId ?? null) : undefined}
  aria-label={item.name}
>
  <span class="file-row-icon" aria-hidden="true">{icon}</span>
  <span class="file-row-name" class:bold={selected}>{item.name}</span>
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
