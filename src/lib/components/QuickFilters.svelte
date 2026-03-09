<script lang="ts">
  import type { ContentCategory, ReplicationTier } from '../types';

  let {
    onFilterChange,
  }: {
    onFilterChange?: (filters: Record<string, unknown>) => void;
  } = $props();

  const allCategories: { key: ContentCategory; label: string }[] = [
    { key: 'music', label: 'Music' },
    { key: 'video', label: 'Video' },
    { key: 'text', label: 'Text' },
    { key: 'image', label: 'Image' },
    { key: 'software', label: 'Software' },
    { key: 'dataset', label: 'Dataset' },
    { key: 'bundle', label: 'Bundle' },
  ];

  const allTiers: { key: ReplicationTier; label: string }[] = [
    { key: 'expendable', label: 'Expendable' },
    { key: 'light', label: 'Light' },
    { key: 'default', label: 'Default' },
    { key: 'high', label: 'High' },
    { key: 'ultra', label: 'Ultra' },
  ];

  // Section expand/collapse state — all open by default
  let categoryOpen = $state(true);
  let statusOpen = $state(true);
  let tierOpen = $state(true);

  // Filter state
  let selectedCategories = $state<Set<ContentCategory>>(new Set());
  let stale = $state(false);
  let pinned = $state(false);
  let licensed = $state(false);
  let underReplicated = $state(false);
  let selectedTiers = $state<Set<ReplicationTier>>(new Set());

  function emitFilters() {
    onFilterChange?.({
      categories: Array.from(selectedCategories),
      stale,
      pinned,
      licensed,
      underReplicated,
      tiers: Array.from(selectedTiers),
    });
  }

  function toggleCategory(cat: ContentCategory) {
    const next = new Set(selectedCategories);
    if (next.has(cat)) {
      next.delete(cat);
    } else {
      next.add(cat);
    }
    selectedCategories = next;
    emitFilters();
  }

  function toggleStatus(key: 'stale' | 'pinned' | 'licensed' | 'underReplicated') {
    if (key === 'stale') stale = !stale;
    else if (key === 'pinned') pinned = !pinned;
    else if (key === 'licensed') licensed = !licensed;
    else if (key === 'underReplicated') underReplicated = !underReplicated;
    emitFilters();
  }

  function toggleTier(tier: ReplicationTier) {
    const next = new Set(selectedTiers);
    if (next.has(tier)) {
      next.delete(tier);
    } else {
      next.add(tier);
    }
    selectedTiers = next;
    emitFilters();
  }
</script>

<section class="quick-filters" aria-label="Quick filters">
  <!-- Category section -->
  <div class="filter-section">
    <button
      type="button"
      class="section-header"
      aria-expanded={categoryOpen ? 'true' : 'false'}
      onclick={() => (categoryOpen = !categoryOpen)}
    >
      <span class="collapse-icon">{categoryOpen ? '▼' : '▶'}</span>
      Category
    </button>
    {#if categoryOpen}
      <div class="section-body">
        {#each allCategories as cat (cat.key)}
          <label class="filter-checkbox">
            <input
              type="checkbox"
              checked={selectedCategories.has(cat.key)}
              onchange={() => toggleCategory(cat.key)}
            />
            {cat.label}
          </label>
        {/each}
      </div>
    {/if}
  </div>

  <!-- Status section -->
  <div class="filter-section">
    <button
      type="button"
      class="section-header"
      aria-expanded={statusOpen ? 'true' : 'false'}
      onclick={() => (statusOpen = !statusOpen)}
    >
      <span class="collapse-icon">{statusOpen ? '▼' : '▶'}</span>
      Status
    </button>
    {#if statusOpen}
      <div class="section-body status-buttons">
        <button type="button" class="filter-btn" class:active={stale}
          aria-pressed={stale ? 'true' : 'false'}
          onclick={() => toggleStatus('stale')}>Stale</button>
        <button type="button" class="filter-btn" class:active={pinned}
          aria-pressed={pinned ? 'true' : 'false'}
          onclick={() => toggleStatus('pinned')}>Pinned</button>
        <button type="button" class="filter-btn" class:active={licensed}
          aria-pressed={licensed ? 'true' : 'false'}
          onclick={() => toggleStatus('licensed')}>Licensed</button>
        <button type="button" class="filter-btn" class:active={underReplicated}
          aria-pressed={underReplicated ? 'true' : 'false'}
          onclick={() => toggleStatus('underReplicated')}>Under-replicated</button>
      </div>
    {/if}
  </div>

  <!-- Tier section -->
  <div class="filter-section">
    <button
      type="button"
      class="section-header"
      aria-expanded={tierOpen ? 'true' : 'false'}
      onclick={() => (tierOpen = !tierOpen)}
    >
      <span class="collapse-icon">{tierOpen ? '▼' : '▶'}</span>
      Replication Tier
    </button>
    {#if tierOpen}
      <div class="section-body tier-buttons">
        {#each allTiers as tier (tier.key)}
          <button type="button" class="filter-btn" class:active={selectedTiers.has(tier.key)}
            aria-pressed={selectedTiers.has(tier.key) ? 'true' : 'false'}
            onclick={() => toggleTier(tier.key)}>{tier.label}</button>
        {/each}
      </div>
    {/if}
  </div>
</section>

<style>
  .quick-filters {
    padding: 4px 0;
  }

  .filter-section {
    border-bottom: 1px solid var(--border, #3f4147);
  }

  .filter-section:last-child {
    border-bottom: none;
  }

  .section-header {
    width: 100%;
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 6px 8px;
    border: none;
    background: none;
    color: var(--text-secondary, #b5bac1);
    font-size: 0.75rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    cursor: pointer;
    text-align: left;
  }

  .section-header:hover {
    color: var(--text-primary, #f2f3f5);
  }

  .collapse-icon {
    font-size: 0.55rem;
    width: 10px;
    flex-shrink: 0;
  }

  .section-body {
    padding: 2px 8px 8px 22px;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .filter-checkbox {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 0.8rem;
    color: var(--text-primary, #f2f3f5);
    cursor: pointer;
  }

  .filter-checkbox input[type="checkbox"] {
    accent-color: var(--accent, #5865f2);
  }

  .status-buttons,
  .tier-buttons {
    flex-direction: row;
    flex-wrap: wrap;
    gap: 4px;
  }

  .filter-btn {
    padding: 3px 8px;
    border: 1px solid var(--border, #3f4147);
    border-radius: 4px;
    background: none;
    color: var(--text-secondary, #b5bac1);
    font-size: 0.75rem;
    cursor: pointer;
    white-space: nowrap;
  }

  .filter-btn:hover {
    border-color: var(--text-muted, #949ba4);
  }

  .filter-btn.active {
    background: var(--accent, #5865f2);
    border-color: var(--accent, #5865f2);
    color: #fff;
  }
</style>
