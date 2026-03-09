<script lang="ts">
  import type { FileViewMode, ContentSection, CleanupRecommendation, ContentCategory, ReplicationTier } from '../types';
  import { FileManagerService } from '../file-manager-service';
  import { tierTarget } from '../file-utils';
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
    filters = {} as Record<string, unknown>,
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
    filters?: Record<string, unknown>;
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
    // Apply quick filters
    const cats = filters.categories as ContentCategory[] | undefined;
    if (cats && cats.length > 0) {
      const catSet = new Set(cats);
      contents = contents.filter((i) => i.isFolder || catSet.has(i.category));
    }
    const tiers = filters.tiers as ReplicationTier[] | undefined;
    if (tiers && tiers.length > 0) {
      const tierSet = new Set(tiers);
      contents = contents.filter((i) => i.isFolder || tierSet.has(i.replicationTier));
    }
    if (filters.stale) {
      contents = contents.filter((i) => i.isFolder || i.stalenessScore >= 0.5);
    }
    if (filters.pinned) {
      contents = contents.filter((i) => i.isFolder || i.pinned);
    }
    if (filters.licensed) {
      contents = contents.filter((i) => i.isFolder || i.licensed);
    }
    if (filters.underReplicated) {
      contents = contents.filter((i) => i.isFolder || i.replicaCount < tierTarget(i.replicationTier));
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
      const allContent = service.getContents();
      // Walk up the parent chain to build the full ancestor path
      const ancestors: Array<{ cid: string; name: string }> = [];
      let cid: string | null = currentFolderCid;
      while (cid) {
        const folder = allContent.find((i) => i.cid === cid);
        if (!folder) break;
        ancestors.unshift({ cid: folder.cid, name: folder.name });
        cid = folder.parentCid;
      }
      path.push(...ancestors);
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

      <QuotaBar
        usedBytes={quota.usedBytes}
        totalBytes={quota.totalBytes}
        {onCleanupClick}
      />
    {/if}
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
</style>
