<script lang="ts">
  import type { ContentItem } from '../types';
  import FileCard from './FileCard.svelte';

  let {
    items,
    selectedCid,
    selectedSidecarId = null,
    onItemClick,
  }: {
    items: ContentItem[];
    selectedCid: string | null;
    selectedSidecarId?: string | null;
    onItemClick: (item: ContentItem) => void;
  } = $props();
</script>

<div class="file-grid" aria-label="File grid">
  {#each items as item (item.sidecarId || item.cid)}
    <FileCard {item} onClick={onItemClick} selected={selectedSidecarId !== null ? selectedSidecarId === item.sidecarId : selectedCid === item.cid} />
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
