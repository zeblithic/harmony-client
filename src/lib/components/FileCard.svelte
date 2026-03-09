<script lang="ts">
  import type { ContentItem } from '../types';
  import { categoryIcon, formatBytes } from '../file-utils';
  import StalenessIndicator from './StalenessIndicator.svelte';

  let {
    item,
    onClick,
    selected = false,
  }: {
    item: ContentItem;
    onClick?: (cid: string) => void;
    selected?: boolean;
  } = $props();

  let icon = $derived(categoryIcon(item.category));
  let size = $derived(formatBytes(item.sizeBytes));
  let sensitivityIcon = $derived(
    item.sensitivity === 'public' ? '\uD83C\uDF10' : '\uD83D\uDD12'
  );
</script>

<button
  class="file-card"
  class:selected
  onclick={() => onClick?.(item.cid)}
  aria-label={item.name}
>
  <div class="file-card-overlay-top">
    <span class="file-card-sensitivity" aria-hidden="true">{sensitivityIcon}</span>
    <span class="file-card-staleness">
      <StalenessIndicator score={item.stalenessScore} pinned={item.pinned} />
    </span>
  </div>
  <div class="file-card-thumbnail" aria-hidden="true">
    <span class="file-card-icon">{icon}</span>
  </div>
  <div class="file-card-info">
    <span class="file-card-name">{item.name}</span>
    <span class="file-card-size">{size}</span>
  </div>
</button>

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
    cursor: pointer;
    font: inherit;
    color: inherit;
    text-align: left;
  }

  .file-card:hover {
    background: var(--bg-tertiary, #232428);
  }

  .file-card.selected {
    border-color: var(--accent, #5865f2);
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

  .file-card-size {
    font-size: 0.75rem;
    color: var(--text-muted, #949ba4);
  }
</style>
