<script lang="ts">
  import type { PublishedItem, ContentCategory } from '../types';
  import { formatBytes, categoryIcon, relativeTime } from '../file-utils';

  let {
    items,
  }: {
    items: PublishedItem[];
  } = $props();

  let searchQuery = $state('');
  let selectedCategory = $state<ContentCategory | null>(null);

  const allCategories: { key: ContentCategory; label: string }[] = [
    { key: 'music', label: 'Music' },
    { key: 'video', label: 'Video' },
    { key: 'text', label: 'Text' },
    { key: 'image', label: 'Image' },
    { key: 'software', label: 'Software' },
    { key: 'dataset', label: 'Dataset' },
    { key: 'bundle', label: 'Bundle' },
  ];

  let presentCategories = $derived(
    allCategories.filter(cat => items.some(item => item.category === cat.key))
  );

  let filteredItems = $derived.by(() => {
    let result = items;

    if (selectedCategory) {
      result = result.filter(item => item.category === selectedCategory);
    }

    if (searchQuery.trim()) {
      const query = searchQuery.trim().toLowerCase();
      result = result.filter(item => item.name.toLowerCase().includes(query));
    }

    return result;
  });

  let itemCount = $derived(filteredItems.length);
  let itemWord = $derived(itemCount === 1 ? 'item' : 'items');

  function publishModeIcon(mode: 'durable' | 'ephemeral'): string {
    return mode === 'durable' ? '🌐' : '🌬';
  }

  function publishModeLabel(mode: 'durable' | 'ephemeral'): string {
    return mode === 'durable' ? 'Durable' : 'Ephemeral';
  }

  function selectCategory(cat: ContentCategory | null) {
    selectedCategory = cat;
  }
</script>

<section class="published-view" aria-label="Published content catalog">
  <div class="published-header">
    <h2 class="published-title">Published Content</h2>
    <span class="item-count">{itemCount} {itemWord}</span>
  </div>

  <div class="published-controls">
    <input
      class="search-input"
      type="search"
      placeholder="Search published content..."
      aria-label="Search published content"
      value={searchQuery}
      oninput={(e) => (searchQuery = e.currentTarget.value)}
    />

    <div class="category-filters" role="toolbar" aria-label="Category filter">
      <button
        class="filter-btn"
        class:active={selectedCategory === null}
        aria-pressed={selectedCategory === null}
        onclick={() => selectCategory(null)}
      >All</button>
      {#each presentCategories as cat (cat.key)}
        <button
          class="filter-btn"
          class:active={selectedCategory === cat.key}
          aria-pressed={selectedCategory === cat.key}
          onclick={() => selectCategory(cat.key)}
        >{cat.label}</button>
      {/each}
    </div>
  </div>

  {#if filteredItems.length === 0}
    <p class="empty-message">No published content to display.</p>
  {:else}
    <div class="published-list" role="list">
      {#each filteredItems as item (item.cid)}
        <div class="published-row" role="listitem">
          <span class="publish-mode" aria-label={publishModeLabel(item.publishMode)}>
            {publishModeIcon(item.publishMode)}
          </span>
          <span class="category-icon" aria-hidden="true">
            {categoryIcon(item.category)}
          </span>
          <span class="item-name">{item.name}</span>
          <span class="item-size">{formatBytes(item.sizeBytes)}</span>
          <span class="item-date">{relativeTime(item.publishedAt)}</span>
        </div>
      {/each}
    </div>
  {/if}
</section>

<style>
  .published-view {
    display: flex;
    flex-direction: column;
    height: 100%;
    overflow: hidden;
    padding: 16px;
    gap: 12px;
  }

  .published-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }

  .published-title {
    margin: 0;
    font-size: 1rem;
    color: var(--text-primary);
    font-weight: 600;
  }

  .item-count {
    font-size: 0.8rem;
    color: var(--text-muted);
  }

  .published-controls {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .search-input {
    width: 100%;
    padding: 6px 10px;
    font: inherit;
    font-size: 0.85rem;
    background: var(--bg-tertiary);
    color: var(--text-primary);
    border: 1px solid var(--border);
    border-radius: 4px;
    outline: none;
    box-sizing: border-box;
  }

  .search-input::placeholder {
    color: var(--text-muted);
  }

  .search-input:focus {
    border-color: var(--accent);
  }

  .category-filters {
    display: flex;
    gap: 4px;
    flex-wrap: wrap;
  }

  .filter-btn {
    padding: 3px 8px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: none;
    color: var(--text-secondary);
    cursor: pointer;
    white-space: nowrap;
    font: inherit;
    font-size: 0.75rem;
  }

  .filter-btn:hover {
    border-color: var(--text-muted);
  }

  .filter-btn.active {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--on-accent);
  }

  .published-list {
    display: flex;
    flex-direction: column;
    gap: 2px;
    overflow-y: auto;
    flex: 1;
  }

  .published-row {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 10px;
    border-radius: 4px;
    background: var(--bg-secondary);
  }

  .published-row:hover {
    background: var(--bg-tertiary);
  }

  .publish-mode {
    font-size: 1.1rem;
    flex-shrink: 0;
    width: 24px;
    text-align: center;
  }

  .category-icon {
    font-size: 0.9rem;
    flex-shrink: 0;
    width: 20px;
    text-align: center;
  }

  .item-name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: 0.85rem;
    color: var(--text-primary);
  }

  .item-size {
    flex-shrink: 0;
    font-size: 0.8rem;
    color: var(--text-muted);
  }

  .item-date {
    flex-shrink: 0;
    font-size: 0.8rem;
    color: var(--text-muted);
    min-width: 50px;
    text-align: right;
  }

  .empty-message {
    text-align: center;
    color: var(--text-muted);
    font-size: 0.9rem;
    padding: 32px 16px;
    margin: 0;
  }
</style>
