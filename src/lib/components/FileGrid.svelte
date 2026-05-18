<script lang="ts">
  import type { ContentItem } from '../types';
  import FileCard from './FileCard.svelte';

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
    /** ZEB-162: per-card drag handlers, wired by FileBrowser. */
    onRowDragStart?: (e: DragEvent, item: ContentItem) => void;
    onRowDrop?: (e: DragEvent, targetCid: string, targetSidecarId: string | null) => void;
    /** ZEB-299: inline rename — see FileList.svelte for the contract. */
    editingItem?: ContentItem | null;
    editingValue?: string;
    onBeginRename?: (item: ContentItem) => void;
    onCommitRename?: () => void;
    onCancelRename?: () => void;
  } = $props();

  // See FileList.svelte for the rationale — top-level uses sidecarId
  // (insert dedupes by id only), nested uses (name, cid) (manifest
  // invariants make name unique within a folder).
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

<div class="file-grid" aria-label="File grid">
  {#each items as item (item.sidecarId || `nested:${item.cid}:${item.name}`)}
    <FileCard
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
