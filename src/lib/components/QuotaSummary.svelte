<script lang="ts">
  import type { QuotaStatus, ContentCategory } from '../types';
  import { formatBytes, categoryIcon } from '../file-utils';

  let {
    quota,
  }: {
    quota: QuotaStatus;
  } = $props();

  let percent = $derived(quota.totalBytes > 0 ? Math.min(100, Math.round((quota.usedBytes / quota.totalBytes) * 100)) : 0);

  const CATEGORY_COLORS: Record<ContentCategory, string> = {
    music: '#1abc9c',
    video: '#3498db',
    text: '#9b59b6',
    image: '#e67e22',
    software: '#2ecc71',
    dataset: '#f1c40f',
    bundle: '#95a5a6',
  };

  let segments = $derived.by(() => {
    const entries: Array<{ category: ContentCategory; bytes: number; color: string; widthPercent: number }> = [];
    for (const [cat, bytes] of Object.entries(quota.byCategory)) {
      if (bytes && bytes > 0) {
        entries.push({
          category: cat as ContentCategory,
          bytes,
          color: CATEGORY_COLORS[cat as ContentCategory],
          widthPercent: quota.totalBytes > 0 ? (bytes / quota.totalBytes) * 100 : 0,
        });
      }
    }
    // Sort largest first for visual clarity
    entries.sort((a, b) => b.bytes - a.bytes);
    return entries;
  });
</script>

<section class="quota-summary" aria-label="Storage quota summary">
  <div class="quota-header">
    <h3 class="quota-title">Storage Usage</h3>
    <span class="quota-text">{formatBytes(quota.usedBytes)} of {formatBytes(quota.totalBytes)} ({percent}%)</span>
  </div>

  <div class="category-bar" role="img" aria-label="Storage breakdown by category">
    {#each segments as seg (seg.category)}
      <div
        class="category-segment"
        style:width="{seg.widthPercent}%"
        style:background={seg.color}
        title="{seg.category}: {formatBytes(seg.bytes)}"
      ></div>
    {/each}
  </div>

  <ul class="category-legend">
    {#each segments as seg (seg.category)}
      <li class="legend-item">
        <span class="legend-swatch" style:background={seg.color}></span>
        <span class="legend-icon" aria-hidden="true">{categoryIcon(seg.category)}</span>
        <span class="legend-label">{seg.category}</span>
        <span class="legend-size">{formatBytes(seg.bytes)}</span>
      </li>
    {/each}
  </ul>
</section>

<style>
  .quota-summary {
    padding: 16px;
    background: var(--bg-secondary);
    border-bottom: 1px solid var(--border);
  }

  .quota-header {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    margin-bottom: 8px;
  }

  .quota-title {
    margin: 0;
    font-size: 0.95rem;
    color: var(--text-primary);
    font-weight: 600;
  }

  .quota-text {
    font-size: 0.8rem;
    color: var(--text-muted);
  }

  .category-bar {
    display: flex;
    height: 8px;
    border-radius: 4px;
    overflow: hidden;
    background: var(--bg-tertiary);
    gap: 1px;
  }

  .category-segment {
    height: 100%;
    min-width: 2px;
    transition: width 0.3s;
  }

  .category-legend {
    list-style: none;
    margin: 8px 0 0;
    padding: 0;
    display: flex;
    flex-wrap: wrap;
    gap: 8px 16px;
  }

  .legend-item {
    display: flex;
    align-items: center;
    gap: 4px;
    font-size: 0.75rem;
  }

  .legend-swatch {
    display: inline-block;
    width: 8px;
    height: 8px;
    border-radius: 2px;
    flex-shrink: 0;
  }

  .legend-icon {
    font-size: 0.8rem;
  }

  .legend-label {
    color: var(--text-secondary);
    text-transform: capitalize;
  }

  .legend-size {
    color: var(--text-muted);
  }
</style>
