<script lang="ts">
  import type { ContentItem } from '../types';
  import FileRow from './FileRow.svelte';

  let {
    items,
    selectedCid,
    selectedSidecarId = null,
    onItemClick,
    onRowDragStart,
    onRowDrop,
    editingItem = null,
    editingValue = $bindable(''),
    onBeginRename,
    onCommitRename,
    onCancelRename,
  }: {
    items: ContentItem[];
    selectedCid: string | null;
    selectedSidecarId?: string | null;
    onItemClick: (item: ContentItem) => void;
    /** ZEB-162: per-row drag handlers, wired by FileBrowser. */
    onRowDragStart?: (e: DragEvent, item: ContentItem) => void;
    onRowDrop?: (e: DragEvent, targetCid: string, targetSidecarId: string | null) => void;
    /** ZEB-299: inline rename. `editingItem` toggles the matching row
     *  into edit mode; `editingValue` two-way-binds the input back up
     *  to FileBrowser. */
    editingItem?: ContentItem | null;
    editingValue?: string;
    onBeginRename?: (item: ContentItem) => void;
    onCommitRename?: () => void;
    onCancelRename?: () => void;
  } = $props();

  // Edit-mode match: top-level rows carry a non-empty sidecarId (the
  // sidecar's unique key); manifest-derived nested rows carry "". For
  // top-level use sidecarId (two sidecar entries CAN share name+cid
  // because insert only dedupes by id). For nested fall back to
  // name+cid, which manifest invariants make unique within a folder.
  function matchesEditing(item: ContentItem): boolean {
    if (editingItem === null) return false;
    if (editingItem.sidecarId && item.sidecarId) {
      return editingItem.sidecarId === item.sidecarId;
    }
    if (!editingItem.sidecarId && !item.sidecarId) {
      return editingItem.name === item.name && editingItem.cid === item.cid;
    }
    return false;
  }
</script>

<div class="file-list" role="table" aria-label="File list">
  <div class="file-list-header" role="row">
    <span class="header-icon" aria-hidden="true"></span>
    <span class="header-name" role="columnheader">Name</span>
    <span class="header-size" role="columnheader">Size</span>
    <span class="header-accessed" role="columnheader">Last Accessed</span>
    <span class="header-staleness" role="columnheader">Staleness</span>
    <span class="header-replicas" role="columnheader">Replicas</span>
    <span class="header-sensitivity" aria-hidden="true"></span>
  </div>
  {#each items as item (item.sidecarId || `nested:${item.cid}:${item.name}`)}
    <FileRow
      {item}
      onClick={onItemClick}
      selected={selectedSidecarId !== null ? selectedSidecarId === item.sidecarId : selectedCid === item.cid}
      {onRowDragStart}
      {onRowDrop}
      editing={matchesEditing(item)}
      bind:editValue={editingValue}
      {onBeginRename}
      {onCommitRename}
      {onCancelRename}
    />
  {/each}
</div>

<style>
  .file-list {
    width: 100%;
    flex: 1;
    overflow-y: auto;
  }

  .file-list-header {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 4px 12px;
    border-bottom: 1px solid var(--border, #3f4147);
  }

  .file-list-header span {
    font-size: 0.75rem;
    color: var(--text-muted, #949ba4);
    text-transform: uppercase;
    letter-spacing: 0.05em;
    font-weight: 600;
  }

  .header-icon {
    flex-shrink: 0;
    width: 24px;
  }

  .header-name {
    flex: 1;
    min-width: 0;
  }

  .header-size,
  .header-accessed,
  .header-replicas {
    flex-shrink: 0;
    min-width: 60px;
    text-align: right;
  }

  .header-staleness {
    flex-shrink: 0;
    min-width: 8px;
  }

  .header-sensitivity {
    flex-shrink: 0;
    width: 24px;
  }
</style>
