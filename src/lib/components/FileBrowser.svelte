<script lang="ts">
  import { untrack } from 'svelte';
  import type { FileViewMode, ContentSection, ContentCategory, ReplicationTier, ContentItem } from '../types';
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

  // Live folder contents fetched from the backend when navigating into a folder.
  // Null means "not yet fetched" (while navigating); an empty array means the
  // folder exists but is empty. Used only when currentFolderCid != null.
  let folderItems = $state<ContentItem[] | null>(null);

  // Explicit navigation stack of {cid, name} from root sentinel down to the
  // current folder. Maintained locally because nested folder rows are not in
  // the sidecar (Option Y) and cannot be walked via parentCid. handleItemClick
  // stashes the (cid, name) before calling onNavigateFolder so the $effect
  // below can extend the stack with a real name. Truncates on back-navigation
  // (clicking a breadcrumb), resets when currentFolderCid → null.
  let navStack = $state<Array<{ cid: string; name: string }>>([]);
  let pendingNav: { cid: string; name: string } | null = null;

  // Sync navStack with currentFolderCid (driven by the parent component).
  // The effect's only reactive dependency is currentFolderCid; navStack /
  // pendingNav / items are read inside untrack() so writes to navStack
  // don't trigger an infinite re-run loop.
  $effect(() => {
    const cid = currentFolderCid;
    untrack(() => {
      if (cid === null) {
        navStack = [];
        pendingNav = null;
        return;
      }
      // Back navigation: cid is already somewhere in the stack → truncate.
      const idx = navStack.findIndex((seg) => seg.cid === cid);
      if (idx >= 0) {
        navStack = navStack.slice(0, idx + 1);
        pendingNav = null;
        return;
      }
      // Forward navigation: prefer the (cid, name) stashed at click time.
      if (pendingNav && pendingNav.cid === cid) {
        navStack = [...navStack, pendingNav];
        pendingNav = null;
        return;
      }
      // Programmatic jump (no click stash): try to look up the name from
      // current items, then root-level items, then fall back to a placeholder.
      const item =
        items.find((i) => i.cid === cid) ??
        service.getContents().find((i) => i.cid === cid);
      navStack = [...navStack, { cid, name: item?.name ?? '(folder)' }];
    });
  });

  // Whenever currentFolderCid OR serviceVersion changes, fetch live folder
  // contents from the backend. The serviceVersion dependency catches
  // pin/unpin/burn/archive/tier mutations on items inside the current folder,
  // which bump the service's version counter but don't change currentFolderCid.
  $effect(() => {
    void serviceVersion; // re-fetch on cache mutation
    const cid = currentFolderCid;
    if (!cid) {
      folderItems = null;
      return;
    }
    folderItems = null; // reset while fetching
    service.listFolderContents(cid).then((result) => {
      // Guard: only update if we're still in the same folder
      if (currentFolderCid === cid) {
        folderItems = result;
      }
    });
  });

  let publishedItems = $derived.by(() => {
    void serviceVersion;
    return service.getPublishedContent();
  });

  function applyFiltersAndSort(contents: ContentItem[]): ContentItem[] {
    // Defensive copy: callers may pass folderItems (a $state proxy) directly
    // when no filter is applied, and our trailing .sort() would otherwise
    // mutate the proxy in-place inside a $derived.by — Svelte 5 flags this
    // as a reactivity cycle. Copying once here is cheaper than copying inside
    // every filter branch.
    contents = [...contents];
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
  }

  // When inside a folder:
  //   - If folderItems is non-null (backend fetch completed), use that.
  //   - If folderItems is null (fetching in progress or no adapter), fall back
  //     to the cached service data so tests/Storybook see content immediately.
  // When at root, always use the cached service data.
  let items = $derived.by(() => {
    void serviceVersion;
    if (currentFolderCid !== null) {
      return applyFiltersAndSort(folderItems ?? service.getContents(currentFolderCid));
    }
    return applyFiltersAndSort(service.getContents(null));
  });

  let quota = $derived.by(() => {
    void serviceVersion;
    return service.getQuotaStatus();
  });

  let cleanupRecommendations = $derived.by(() => {
    void serviceVersion;
    return service.getCleanupRecommendations();
  });

  // Breadcrumb path = root sentinel + the explicit nav stack. No sidecar
  // walk: nested folder rows aren't persisted (Option Y) so parentCid can't
  // reconstruct the chain reliably.
  let breadcrumbPath = $derived.by<Array<{ cid: string | null; name: string }>>(
    () => [{ cid: null, name: 'My Content' }, ...navStack],
  );

  // The stack of ancestor CIDs from root down to the current folder's parent.
  // Used by createFolder so the backend can cascade the CID update up the tree.
  let breadcrumbStack = $derived.by<string[]>(() => navStack.map((seg) => seg.cid));

  function handleItemClick(cid: string) {
    const item = items.find((i) => i.cid === cid);
    if (item?.isFolder) {
      // Stash (cid, name) for the navStack $effect to consume on the next tick.
      pendingNav = { cid: item.cid, name: item.name };
      onNavigateFolder(item.cid);
      return;
    }
    onItemClick(cid);
  }

  async function handleNewFolder() {
    const name = window.prompt('Folder name:');
    if (!name || !name.trim()) return;

    try {
      await service.createFolder(name.trim(), breadcrumbStack);
    } catch (err) {
      // Surface known backend errors to the user; identical-contents
      // collisions and similar are returned as plain Err strings.
      const msg = err instanceof Error ? err.message : String(err);
      window.alert(`Could not create folder: ${msg}`);
      return;
    }

    // Nested create: the backend's ancestor cascade rewrites every CID
    // along the path including currentFolderCid, so refetching the same
    // CID would just re-read the now-stale bundle. Until ZEB-164 lands a
    // stable sidecar identity, navigate back to the root view — the new
    // folder appears in the root's refreshed listing (createFolder already
    // called refetchRoot internally) and the user can re-enter the path.
    if (currentFolderCid) {
      onNavigateFolder(null);
    }
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
    onNewFolderClick={handleNewFolder}
    {showCleanup}
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
