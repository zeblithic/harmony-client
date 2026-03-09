<script lang="ts">
  import type { ContentDetail } from '../types';
  import { categoryIcon, formatBytes, relativeTime } from '../file-utils';

  let {
    item,
  }: {
    item: ContentDetail;
  } = $props();

  let icon = $derived(categoryIcon(item.category));
  let size = $derived(formatBytes(item.sizeBytes));
  let storedDate = $derived(new Date(item.storedAt).toLocaleDateString());
  let lastAccessedRelative = $derived(relativeTime(item.lastAccessed));

  const ORIGIN_LABELS: Record<string, string> = {
    'self-created': 'Self-created',
    'peer-replicated': 'Peer-replicated',
    downloaded: 'Downloaded',
    'cached-in-transit': 'Cached in transit',
  };

  let originLabel = $derived(ORIGIN_LABELS[item.origin] ?? item.origin);
</script>

<div class="file-metadata">
  <h3 class="file-name">{item.name}</h3>

  <div class="metadata-row">
    <span class="metadata-label">Category</span>
    <span class="metadata-value">
      <span aria-hidden="true">{icon}</span>
      {item.category}
    </span>
  </div>

  <div class="metadata-row">
    <span class="metadata-label">Size</span>
    <span class="metadata-value">{size}</span>
  </div>

  <div class="metadata-row">
    <span class="metadata-label">Stored</span>
    <span class="metadata-value">{storedDate}</span>
  </div>

  <div class="metadata-row">
    <span class="metadata-label">Last accessed</span>
    <span class="metadata-value">{lastAccessedRelative}</span>
  </div>

  <div class="metadata-row">
    <span class="metadata-label">Access count</span>
    <span class="metadata-value">{item.accessCount}</span>
  </div>

  <div class="metadata-row">
    <span class="metadata-label">Origin</span>
    <span class="metadata-value">{originLabel}</span>
  </div>
</div>

<style>
  .file-metadata {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .file-name {
    margin: 0;
    font-size: 1rem;
    font-weight: 600;
    color: var(--text-primary, #f2f3f5);
    word-break: break-word;
  }

  .metadata-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-size: 0.85rem;
  }

  .metadata-label {
    color: var(--text-secondary, #b5bac1);
  }

  .metadata-value {
    color: var(--text-primary, #f2f3f5);
    text-transform: capitalize;
  }
</style>
