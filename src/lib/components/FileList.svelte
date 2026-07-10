<script lang="ts">
  import type { ContentItem } from '../types';
  import { matchesEditing } from '../file-utils';
  import FileRow from './FileRow.svelte';
  import FilePlaceholderRow from './FilePlaceholderRow.svelte';

  let {
    items,
    selectedCid,
    selectedSidecarId = null,
    onItemClick,
    onRowDragStart,
    onRowDrop,
    editingItem = null,
    editingValue = $bindable(''),
    renameInFlight = false,
    onBeginRename,
    onCommitRename,
    onCancelRename,
    creatingFolder = false,
    newFolderName = $bindable(''),
    newFolderError = null,
    creatingFolderInFlight = false,
    onCommitCreateFolder,
    onCancelCreateFolder,
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
     *  to FileBrowser. `renameInFlight` (round 5) suppresses the blur
     *  cancel while the commit IPC is awaiting so a failure keeps the
     *  input visible for retry. */
    editingItem?: ContentItem | null;
    editingValue?: string;
    renameInFlight?: boolean;
    onBeginRename?: (item: ContentItem) => void;
    onCommitRename?: () => void;
    onCancelRename?: () => void;
    /** ZEB-166: inline new-folder placeholder. `creatingFolder` toggles
     *  a FilePlaceholderRow at the top of the list; `newFolderName`
     *  two-way-binds the input back up to FileBrowser. The pair of
     *  callbacks mirror the ZEB-299 commit/cancel shape. */
    creatingFolder?: boolean;
    newFolderName?: string;
    newFolderError?: string | null;
    creatingFolderInFlight?: boolean;
    onCommitCreateFolder?: () => void;
    onCancelCreateFolder?: () => void;
  } = $props();
</script>

<div class="file-list" role="table" aria-label="File list">
  <div class="file-list-header" role="row">
    <span class="header-icon" aria-hidden="true"></span>
    <span class="header-name" role="columnheader">Name</span>
    <span class="header-cid" role="columnheader">CID</span>
    <span class="header-size" role="columnheader">Size</span>
    <span class="header-replicas" role="columnheader">Replication</span>
    <span class="header-sensitivity" aria-hidden="true"></span>
  </div>
  {#if creatingFolder}
    <FilePlaceholderRow
      bind:value={newFolderName}
      error={newFolderError}
      inFlight={creatingFolderInFlight}
      onCommit={onCommitCreateFolder}
      onCancel={onCancelCreateFolder}
    />
  {/if}
  {#each items as item (item.sidecarId || `nested:${item.cid}:${item.name}`)}
    <FileRow
      {item}
      onClick={onItemClick}
      selected={selectedSidecarId !== null ? selectedSidecarId === item.sidecarId : selectedCid === item.cid}
      {onRowDragStart}
      {onRowDrop}
      editing={matchesEditing(editingItem, item)}
      bind:editValue={editingValue}
      {renameInFlight}
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
    border-bottom: 1px solid var(--border);
  }

  .file-list-header span {
    font-size: 0.75rem;
    color: var(--text-muted);
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
  .header-replicas {
    flex-shrink: 0;
    min-width: 60px;
    text-align: right;
  }

  .header-cid {
    flex-shrink: 0;
    min-width: 130px;
  }

  .header-sensitivity {
    flex-shrink: 0;
    width: 24px;
  }
</style>
