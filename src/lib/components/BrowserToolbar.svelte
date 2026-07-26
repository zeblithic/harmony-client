<script lang="ts">
  import type { FileViewMode, ContentSection } from '../types';

  let {
    viewMode,
    onViewModeChange,
    searchQuery,
    onSearchChange,
    onUploadClick,
    onCleanupClick,
    onNewFolderClick,
    onAddFolderClick,
    showCleanup = false,
    section,
    onSectionChange,
    sharedUnreadCount = 0,
  }: {
    viewMode: FileViewMode;
    onViewModeChange: (mode: FileViewMode) => void;
    searchQuery: string;
    onSearchChange: (query: string) => void;
    onUploadClick: (encrypted?: boolean) => void;
    onCleanupClick: () => void;
    onNewFolderClick?: () => void;
    onAddFolderClick?: () => void;
    showCleanup?: boolean;
    section: ContentSection;
    onSectionChange: (section: ContentSection) => void;
    /** ZEB-723: count of shared-with-me files newer than last-seen; a
     *  numeric pill on the "Shared with me" tab when > 0. */
    sharedUnreadCount?: number;
  } = $props();

  // ZEB-674 Gap A: local toggle controlling whether the *next* "Add files"
  // upload routes through the encrypted (private, per-file-DEK) ingest path
  // instead of the default unencrypted one. Defaults OFF so existing
  // behavior is unchanged unless a user opts in. Encrypted files stay
  // private and can be shared with specific people via the file detail
  // panel's "Shared with" list; unencrypted files become public once
  // published.
  let encryptOnUpload = $state(false);
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
    <button
      class="section-btn"
      class:active={section === 'sharedWithMe'}
      aria-pressed={section === 'sharedWithMe'}
      onclick={() => onSectionChange('sharedWithMe')}
    >Shared with me{#if sharedUnreadCount > 0}<span class="section-badge">{sharedUnreadCount}</span>{/if}</button>
  </div>

  {#if section === 'private' && !showCleanup}
    <input
      class="search-input"
      type="search"
      placeholder="Search files or paste a CID…"
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
        <button class="action-btn" onclick={() => onUploadClick(encryptOnUpload)} aria-label="Add files">⤓ Add files</button>
        <label
          class="encrypt-toggle"
          title="Encrypted files are private and can be shared with specific people via &quot;Shared with&quot;. Unencrypted files are public once published."
        >
          <input
            type="checkbox"
            checked={encryptOnUpload}
            onchange={(e) => { encryptOnUpload = e.currentTarget.checked; }}
          />
          Encrypt (private)
        </label>
        {#if onNewFolderClick}
          <button class="action-btn" onclick={onNewFolderClick} aria-label="New Folder">📁 New Folder</button>
        {/if}
        {#if onAddFolderClick}
          <button
            class="action-btn"
            onclick={onAddFolderClick}
            aria-label="Add folder from disk"
            title="Add folder from disk"
          >📥 Add folder…</button>
        {/if}
      {/if}
      <button class="action-btn" class:active={showCleanup} onclick={onCleanupClick} aria-label="Cleanup" aria-pressed={showCleanup}>🧹 Cleanup</button>
    {/if}
  </div>
</div>

<style>
  /* ZEB-766: wrap instead of clipping. `.toolbar-left` and `.toolbar-right`
     are both `flex-shrink: 0`, so the only flexible item is `.search-input`
     (`flex: 1; min-width: 0`) — once it collapses to zero there is nothing
     left to give and the two rigid groups overflow the row. The row itself
     was `overflow-x: visible`, so the surplus was silently clipped rather
     than scrollable: at the default 1200x800 window that hid "New Folder"
     and "Add folder…" entirely.

     Wrapping (rather than `overflow-x: auto`) matches the ZEB-333 remedy in
     NavPanel's `.mode-toggles` and needs no scroll affordance. `flex-shrink: 0`
     keeps the now-taller row from being squeezed by `.file-browser`'s column
     flex. */
  .browser-toolbar {
    display: flex;
    flex-wrap: wrap;
    flex-shrink: 0;
    align-items: center;
    gap: 8px;
    padding: 8px 12px;
    background: var(--bg-secondary);
    border-bottom: 1px solid var(--border);
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
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-muted);
    cursor: pointer;
    border-radius: 4px;
  }

  .section-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  .section-btn.active {
    background: var(--accent);
    color: var(--on-accent);
    border-color: var(--accent);
  }

  /* ZEB-723: unread pill on the "Shared with me" tab. Reuses NavNodeRow's
     .unread-badge tokens (accent bg / on-accent fg) so count badges read
     consistently across the app. */
  .section-badge {
    display: inline-block;
    margin-left: 0.4em;
    background: var(--accent);
    color: var(--on-accent);
    font-size: 11px;
    font-weight: 700;
    padding: 1px 6px;
    border-radius: 8px;
  }

  .section-btn.active .section-badge {
    background: var(--on-accent);
    color: var(--accent);
  }

  .search-input {
    flex: 1;
    min-width: 0;
    padding: 4px 8px;
    font: inherit;
    font-size: 0.85rem;
    background: var(--bg-tertiary);
    color: var(--text-primary);
    border: 1px solid var(--border);
    border-radius: 4px;
    outline: none;
  }

  .search-input::placeholder {
    color: var(--text-muted);
  }

  .search-input:focus {
    border-color: var(--accent);
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
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-muted);
    cursor: pointer;
    border-radius: 4px;
  }

  .view-btn[aria-pressed='true'] {
    background: var(--accent);
    color: var(--on-accent);
    border-color: var(--accent);
  }

  .action-btn {
    padding: 4px 10px;
    font: inherit;
    font-size: 0.85rem;
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-muted);
    cursor: pointer;
    border-radius: 4px;
  }

  .action-btn:hover {
    background: var(--bg-tertiary);
  }

  .action-btn.active {
    background: var(--accent);
    color: var(--on-accent);
    border-color: var(--accent);
  }

  .action-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  .encrypt-toggle {
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 4px 6px;
    font-size: 0.85rem;
    color: var(--text-muted);
    cursor: pointer;
    white-space: nowrap;
  }

  .encrypt-toggle input {
    cursor: pointer;
  }

  .encrypt-toggle:has(input:focus-visible) {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
    border-radius: 4px;
  }

  .view-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
</style>
