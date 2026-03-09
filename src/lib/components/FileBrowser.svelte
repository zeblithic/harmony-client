<script lang="ts">
  import type { FileViewMode, ContentSection, CleanupRecommendation } from '../types';
  import { FileManagerService } from '../file-manager-service';
  import BrowserToolbar from './BrowserToolbar.svelte';
  import Breadcrumbs from './Breadcrumbs.svelte';
  import FileList from './FileList.svelte';
  import FileGrid from './FileGrid.svelte';
  import QuotaBar from './QuotaBar.svelte';
  import PublishedView from './PublishedView.svelte';
  import CleanupView from './CleanupView.svelte';

  let {
    service,
    currentFolderCid = null,
    selectedCid = null,
    viewMode = 'list' as FileViewMode,
    section = 'private' as ContentSection,
    searchQuery = '',
    showCleanup = false,
    onItemClick,
    onNavigateFolder,
    onViewModeChange,
    onSearchChange,
    onSectionChange,
    onUploadClick,
    onCleanupClick,
    onCleanupAction,
    onBulkBurn,
    onBulkArchive,
    onBulkRelease,
    onBulkPublish,
    serviceVersion = 0,
  }: {
    service: FileManagerService;
    currentFolderCid?: string | null;
    selectedCid?: string | null;
    viewMode?: FileViewMode;
    section?: ContentSection;
    searchQuery?: string;
    showCleanup?: boolean;
    onItemClick: (cid: string) => void;
    onNavigateFolder: (cid: string | null) => void;
    onViewModeChange: (mode: FileViewMode) => void;
    onSearchChange: (query: string) => void;
    onSectionChange: (section: ContentSection) => void;
    onUploadClick: () => void;
    onCleanupClick: () => void;
    onCleanupAction?: (cid: string, action: string) => void;
    onBulkBurn?: (cids: string[]) => void;
    onBulkArchive?: (cids: string[]) => void;
    onBulkRelease?: (cids: string[]) => void;
    onBulkPublish?: (cids: string[]) => void;
    serviceVersion?: number;
  } = $props();

  let publishedItems = $derived.by(() => {
    void serviceVersion;
    return service.getPublishedContent();
  });

  let items = $derived.by(() => {
    void serviceVersion; // trigger reactivity
    let contents = service.getContents(currentFolderCid);
    if (searchQuery) {
      const q = searchQuery.toLowerCase();
      contents = contents.filter((i) => i.name.toLowerCase().includes(q));
    }
    // folders first, then files
    return contents.sort((a, b) => {
      if (a.isFolder !== b.isFolder) return a.isFolder ? -1 : 1;
      return a.name.localeCompare(b.name);
    });
  });

  let quota = $derived.by(() => {
    void serviceVersion;
    return service.getQuotaStatus();
  });

  let cleanupRecommendations = $derived.by(() => {
    void serviceVersion;
    return service.getCleanupRecommendations();
  });

  let breadcrumbPath = $derived.by(() => {
    void serviceVersion;
    const path: Array<{ cid: string | null; name: string }> = [
      { cid: null, name: 'My Content' },
    ];
    if (currentFolderCid) {
      const folder = service.getContents().find((i) => i.cid === currentFolderCid);
      if (folder) path.push({ cid: folder.cid, name: folder.name });
    }
    return path;
  });

  function handleItemClick(cid: string) {
    const item = items.find((i) => i.cid === cid);
    if (item?.isFolder) {
      onNavigateFolder(item.cid);
      return;
    }
    onItemClick(cid);
  }
</script>

<div class="file-browser">
  <BrowserToolbar
    {viewMode}
    {onViewModeChange}
    {searchQuery}
    {onSearchChange}
    {onUploadClick}
    {onCleanupClick}
    {section}
    {onSectionChange}
  />

  {#if section === 'private'}
    {#if showCleanup}
      <CleanupView
        {quota}
        recommendations={cleanupRecommendations}
        onAction={(cid, action) => onCleanupAction?.(cid, action)}
        onBulkBurn={(cids) => onBulkBurn?.(cids)}
        onBulkArchive={(cids) => onBulkArchive?.(cids)}
        onBulkRelease={(cids) => onBulkRelease?.(cids)}
        onBulkPublish={(cids) => onBulkPublish?.(cids)}
      />
    {:else}
      <Breadcrumbs path={breadcrumbPath} onNavigate={onNavigateFolder} />

      {#if viewMode === 'list'}
        <FileList {items} {selectedCid} onItemClick={handleItemClick} />
      {:else}
        <FileGrid {items} {selectedCid} onItemClick={handleItemClick} />
      {/if}
    {/if}

    <QuotaBar
      usedBytes={quota.usedBytes}
      totalBytes={quota.totalBytes}
      {onCleanupClick}
    />
  {:else}
    <PublishedView items={publishedItems} />
  {/if}
</div>

<style>
  .file-browser {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
  }

  .published-placeholder {
    flex: 1;
    display: flex;
    align-items: center;
    justify-content: center;
    color: var(--text-muted, #949ba4);
    font-size: 0.9rem;
  }
</style>
