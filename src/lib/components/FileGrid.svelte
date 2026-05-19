<script lang="ts">
  import type { ContentItem } from '../types';
  import { matchesEditing } from '../file-utils';
  import FileCard from './FileCard.svelte';
  import FilePlaceholderCard from './FilePlaceholderCard.svelte';

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
    /** ZEB-162: per-card drag handlers, wired by FileBrowser. */
    onRowDragStart?: (e: DragEvent, item: ContentItem) => void;
    onRowDrop?: (e: DragEvent, targetCid: string, targetSidecarId: string | null) => void;
    /** ZEB-299: inline rename — see FileList.svelte for the contract. */
    editingItem?: ContentItem | null;
    editingValue?: string;
    renameInFlight?: boolean;
    onBeginRename?: (item: ContentItem) => void;
    onCommitRename?: () => void;
    onCancelRename?: () => void;
    /** ZEB-166: inline new-folder placeholder — see FileList.svelte for
     *  the contract. */
    creatingFolder?: boolean;
    newFolderName?: string;
    newFolderError?: string | null;
    creatingFolderInFlight?: boolean;
    onCommitCreateFolder?: () => void;
    onCancelCreateFolder?: () => void;
  } = $props();
</script>

<div class="file-grid" aria-label="File grid">
  {#if creatingFolder}
    <FilePlaceholderCard
      bind:value={newFolderName}
      error={newFolderError}
      inFlight={creatingFolderInFlight}
      onCommit={onCommitCreateFolder}
      onCancel={onCancelCreateFolder}
    />
  {/if}
  {#each items as item (item.sidecarId || `nested:${item.cid}:${item.name}`)}
    <FileCard
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
  .file-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(160px, 1fr));
    gap: 12px;
    padding: 12px;
    flex: 1;
    overflow-y: auto;
    align-content: start;
  }
</style>
