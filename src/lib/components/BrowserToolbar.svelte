<script lang="ts">
  import type { FileViewMode, ContentSection } from '../types';

  let {
    viewMode,
    onViewModeChange,
    searchQuery,
    onSearchChange,
    onUploadClick,
    onCleanupClick,
    showCleanup = false,
    section,
    onSectionChange,
  }: {
    viewMode: FileViewMode;
    onViewModeChange: (mode: FileViewMode) => void;
    searchQuery: string;
    onSearchChange: (query: string) => void;
    onUploadClick: () => void;
    onCleanupClick: () => void;
    showCleanup?: boolean;
    section: ContentSection;
    onSectionChange: (section: ContentSection) => void;
  } = $props();
</script>

<div class="browser-toolbar">
  <div class="toolbar-left">
    <button
      class="section-btn"
      class:active={section === 'private'}
      aria-pressed={section === 'private'}
      onclick={() => onSectionChange('private')}
    >Private</button>
    <button
      class="section-btn"
      class:active={section === 'published'}
      aria-pressed={section === 'published'}
      onclick={() => onSectionChange('published')}
    >Published</button>
  </div>

  {#if section === 'private' && !showCleanup}
    <input
      class="search-input"
      type="search"
      placeholder="Search files..."
      aria-label="Search files"
      value={searchQuery}
      oninput={(e) => onSearchChange(e.currentTarget.value)}
    />
  {:else}
    <div class="search-spacer"></div>
  {/if}

  <div class="toolbar-right">
    {#if section === 'private'}
      {#if !showCleanup}
        <button
          class="view-btn"
          aria-label="List view"
          aria-pressed={viewMode === 'list'}
          onclick={() => onViewModeChange('list')}
        >☰</button>
        <button
          class="view-btn"
          aria-label="Grid view"
          aria-pressed={viewMode === 'grid'}
          onclick={() => onViewModeChange('grid')}
        >⊞</button>
        <button class="action-btn" onclick={onUploadClick} aria-label="Upload">⬆ Upload</button>
      {/if}
      <button class="action-btn" class:active={showCleanup} onclick={onCleanupClick} aria-label="Cleanup" aria-pressed={showCleanup}>🧹 Cleanup</button>
    {/if}
  </div>
</div>

<style>
  .browser-toolbar {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 12px;
    background: var(--bg-secondary, #2b2d31);
    border-bottom: 1px solid var(--border, #3f4147);
  }

  .toolbar-left {
    display: flex;
    gap: 2px;
    flex-shrink: 0;
  }

  .section-btn {
    padding: 4px 10px;
    font: inherit;
    font-size: 0.85rem;
    border: 1px solid var(--border, #3f4147);
    background: transparent;
    color: var(--text-muted, #949ba4);
    cursor: pointer;
    border-radius: 4px;
  }

  .section-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }

  .section-btn.active {
    background: var(--accent, #5865f2);
    color: var(--text-primary, #f2f3f5);
    border-color: var(--accent, #5865f2);
  }

  .search-input {
    flex: 1;
    min-width: 0;
    padding: 4px 8px;
    font: inherit;
    font-size: 0.85rem;
    background: var(--bg-tertiary, #232428);
    color: var(--text-primary, #f2f3f5);
    border: 1px solid var(--border, #3f4147);
    border-radius: 4px;
    outline: none;
  }

  .search-input::placeholder {
    color: var(--text-muted, #949ba4);
  }

  .search-input:focus {
    border-color: var(--accent, #5865f2);
  }

  .search-spacer {
    flex: 1;
  }

  .toolbar-right {
    display: flex;
    gap: 4px;
    flex-shrink: 0;
    align-items: center;
  }

  .view-btn {
    padding: 4px 8px;
    font: inherit;
    font-size: 0.85rem;
    border: 1px solid var(--border, #3f4147);
    background: transparent;
    color: var(--text-muted, #949ba4);
    cursor: pointer;
    border-radius: 4px;
  }

  .view-btn[aria-pressed='true'] {
    background: var(--accent, #5865f2);
    color: var(--text-primary, #f2f3f5);
    border-color: var(--accent, #5865f2);
  }

  .action-btn {
    padding: 4px 10px;
    font: inherit;
    font-size: 0.85rem;
    border: 1px solid var(--border, #3f4147);
    background: transparent;
    color: var(--text-muted, #949ba4);
    cursor: pointer;
    border-radius: 4px;
  }

  .action-btn:hover {
    background: var(--bg-tertiary, #232428);
  }

  .action-btn.active {
    background: var(--accent, #5865f2);
    color: var(--text-primary, #f2f3f5);
    border-color: var(--accent, #5865f2);
  }

  .action-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }

  .view-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }
</style>
