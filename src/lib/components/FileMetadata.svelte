<script lang="ts">
  import type { ContentDetail } from '../types';
  import { categoryIcon, formatBytes } from '../file-utils';
  // ZEB-946: the "Stored" date honors the owner's date-order preference.
  import { formatDateOnly } from '../time-format';
  import { timeFormatPrefs } from '../time-format-service';

  let {
    item,
  }: {
    item: ContentDetail;
  } = $props();

  let icon = $derived(categoryIcon(item.category));
  let size = $derived(formatBytes(item.sizeBytes));
  let storedDate = $derived(formatDateOnly(item.storedAt, $timeFormatPrefs));
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
    color: var(--text-primary);
    word-break: break-word;
  }

  .metadata-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-size: 0.85rem;
  }

  .metadata-label {
    color: var(--text-secondary);
  }

  .metadata-value {
    color: var(--text-primary);
    text-transform: capitalize;
  }
</style>
