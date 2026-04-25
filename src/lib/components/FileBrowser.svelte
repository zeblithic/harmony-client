<script lang="ts">
  import { untrack } from 'svelte';
  import type {
    FileViewMode,
    ContentSection,
    ContentCategory,
    ReplicationTier,
    ContentItem,
    CleanupRecommendation,
  } from '../types';
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
    selectedSidecarId = null,
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
    selectedSidecarId?: string | null;
    viewMode?: FileViewMode;
    section?: ContentSection;
    searchQuery?: string;
    filters?: Record<string, unknown>;
    showCleanup?: boolean;
    onItemClick: (item: ContentItem) => void;
    onNavigateFolder: (cid: string | null) => void;
    onViewModeChange: (mode: FileViewMode) => void;
    onSearchChange: (query: string) => void;
    onSectionChange: (section: ContentSection) => void;
    onUploadClick: () => void;
    onCleanupClick: () => void;
    onCleanupAction?: (rec: CleanupRecommendation, action: string) => void;
    onBulkBurn?: (recs: CleanupRecommendation[]) => void;
    onBulkArchive?: (recs: CleanupRecommendation[]) => void;
    onBulkRelease?: (cids: string[]) => void;
    onBulkPublish?: (cids: string[]) => void;
    serviceVersion?: number;
  } = $props();

  // Live folder contents fetched from the backend, paired with the cid
  // they were fetched for. Tagging by cid lets the items derived reject
  // stale data during the gap between currentFolderCid changing and the
  // fetch effect kicking off — without the tag, a fresh render between
  // those two events would briefly paint the previous folder's items in
  // the new folder's view.
  //
  // Null means "no fetch result for the current folder is in hand". An
  // empty items array means the folder exists but is empty.
  let folderItems = $state<{ cid: string; items: ContentItem[] } | null>(null);

  // Explicit navigation stack from root sentinel down to the current
  // folder. Maintained locally because nested folder rows are not in the
  // sidecar (Option Y) and cannot be walked via parentCid. handleItemClick
  // stashes (cid, name, sidecarId) before calling onNavigateFolder so the
  // $effect below can extend the stack with a real name. Truncates on
  // back-navigation (clicking a breadcrumb), resets when currentFolderCid
  // → null.
  //
  // Each segment carries cid (for content-addressed bundle lookups during
  // refetch and breadcrumb display) and an optional sidecarId — only
  // present on the FIRST segment after the sentinel, which is the
  // top-level sidecar entry. Nested segments (manifest-derived rows below
  // the top-level root) have no sidecar of their own.
  let navStack = $state<Array<{ cid: string; name: string; sidecarId?: string }>>([]);
  // Plain let (not $state): written only inside untrack(), read only in
  // event handlers. No reactive consumer exists, so $state would add
  // overhead without benefit and would invite confusion about its
  // lifecycle.
  let pendingNav: { cid: string; name: string; sidecarId?: string } | null = null;

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
      // Programmatic jump (no click stash): try to look up the name and
      // sidecarId from current items, then root-level items, then fall
      // back to a placeholder.
      const item =
        items.find((i) => i.cid === cid) ??
        service.getContents().find((i) => i.cid === cid);
      navStack = [
        ...navStack,
        {
          cid,
          name: item?.name ?? '(folder)',
          sidecarId: item?.sidecarId || undefined,
        },
      ];
    });
  });

  // Monotonic token for in-flight listFolderContents calls. Rapid
  // serviceVersion bumps can queue multiple fetches for the same cid; the
  // cid-equality guard alone isn't enough because an older fetch can still
  // resolve last and clobber a newer snapshot. Every effect run bumps this
  // token, and each .then() only commits if its captured token is still the
  // latest — older resolutions are discarded.
  let folderFetchSeq = 0;

  // Whenever currentFolderCid OR serviceVersion changes, fetch live folder
  // contents from the backend. The serviceVersion dependency catches
  // pin/unpin/burn/archive/tier mutations on items inside the current folder,
  // which bump the service's version counter but don't change currentFolderCid.
  //
  // We deliberately do NOT clear folderItems on cid change here: clearing
  // would only happen in the post-render $effect, leaving the pre-effect
  // render painting the previous folder's items into the new folder's
  // view. Instead, the items derived guards on folderItems.cid ===
  // currentFolderCid; a mismatched tag is treated as "no data yet" and
  // the fallback fires. On serviceVersion bumps (same cid), the visible
  // list stays put until the re-fetch resolves.
  $effect(() => {
    void serviceVersion; // re-fetch on cache mutation
    const cid = currentFolderCid;
    const mySeq = ++folderFetchSeq;
    if (!cid) {
      folderItems = null;
      return;
    }
    service
      .listFolderContents(cid)
      .then((result) => {
        // Guards: still in the same folder AND this is the newest fetch.
        // Without the seq check an older resolution could overwrite a newer
        // snapshot after a rapid mutation burst.
        if (currentFolderCid === cid && mySeq === folderFetchSeq) {
          folderItems = { cid, items: result };
        }
      })
      .catch((err) => {
        // The service no longer swallows backend errors (malformed manifest,
        // consistency-check failures, event-loop drop). Surface them to the
        // user so a corrupted folder doesn't look indistinguishable from an
        // empty one, and clear the list so stale contents don't mislead.
        // Everything (state, log, alert) is gated on the same staleness
        // check so a rapid navigate-away doesn't pop a blocking alert
        // about a folder the user is no longer viewing — the error is
        // not actionable from elsewhere in the tree.
        if (currentFolderCid === cid && mySeq === folderFetchSeq) {
          folderItems = { cid, items: [] };
          const msg = err instanceof Error ? err.message : String(err);
          console.error('listFolderContents failed:', err);
          window.alert(`Could not load folder: ${msg}`);
        }
      });
  });

  let publishedItems = $derived.by(() => {
    void serviceVersion;
    return service.getPublishedContent();
  });

  function applyFiltersAndSort(contents: ContentItem[]): ContentItem[] {
    // Defensive copy: callers may pass folderItems.items (a $state proxy)
    // directly when no filter is applied, and our trailing .sort() would
    // otherwise mutate the proxy in-place inside a $derived.by — Svelte 5
    // flags this as a reactivity cycle. Copying once here is cheaper than
    // copying inside every filter branch.
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
  //   - If folderItems is tagged with the current cid (backend fetch
  //     resolved for THIS folder), use its items.
  //   - Otherwise (no fetch yet, or folderItems still tagged with the
  //     previous folder's cid because we just navigated and the fetch
  //     effect hasn't kicked in), fall back to the cached service data.
  //     For real adapters this returns [] during the brief inter-folder
  //     gap (better than flashing the prior folder's contents); for
  //     tests/Storybook with mock data populated by parentCid it returns
  //     the expected children synchronously.
  // When at root, always use the cached service data.
  let items = $derived.by(() => {
    void serviceVersion;
    if (currentFolderCid !== null) {
      const matching =
        folderItems?.cid === currentFolderCid ? folderItems.items : null;
      return applyFiltersAndSort(matching ?? service.getContents(currentFolderCid));
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

  function handleItemClick(item: ContentItem) {
    if (item.isFolder) {
      // Stash for the navStack $effect. sidecarId is only set when the
      // click came from the root listing (entries there have a sidecar
      // entry); manifest-derived rows pass empty sidecarId, which we
      // store as undefined so navStack[0]'s sidecarId is "the top-level
      // root's id, if available".
      pendingNav = {
        cid: item.cid,
        name: item.name,
        sidecarId: item.sidecarId || undefined,
      };
      onNavigateFolder(item.cid);
      return;
    }
    onItemClick(item);
  }

  async function handleNewFolder() {
    const name = window.prompt('Folder name:');
    if (!name || !name.trim()) return;

    // Capture pre-create state. breadcrumbStack drives whether this is a
    // nested create. parentSidecarId is the top-level root's id (the
    // sidecar entry that owns the cascade) — present iff breadcrumbStack
    // is non-empty (at root, parent_sidecar_id is null).
    const wasNestedCreate = breadcrumbStack.length > 0;
    const parentSidecarId = wasNestedCreate
      ? navStack[0]?.sidecarId ?? null
      : null;

    if (wasNestedCreate && !parentSidecarId) {
      // Nested create requires a top-level sidecar id. If we don't have
      // one (e.g., user navigated by URL/programmatic jump before the
      // first list_content settled), bail with a user-visible error.
      window.alert(
        'Could not create folder: folder identity not yet loaded. Click "My Content" in the breadcrumb to return to root, then navigate back into this folder and retry.',
      );
      return;
    }

    try {
      await service.createFolder(name.trim(), parentSidecarId, breadcrumbStack);
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      window.alert(`Could not create folder: ${msg}`);
      return;
    }

    if (wasNestedCreate) {
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
    onNewFolderClick={section === 'private' && !showCleanup
      ? handleNewFolder
      : undefined}
    {showCleanup}
    {section}
    {onSectionChange}
  />

  {#if section === 'private'}
    {#if showCleanup}
      <CleanupView
        {quota}
        recommendations={cleanupRecommendations}
        onAction={(rec, action) => onCleanupAction?.(rec, action)}
        onBulkBurn={(recs) => onBulkBurn?.(recs)}
        onBulkArchive={(recs) => onBulkArchive?.(recs)}
        onBulkRelease={(cids) => onBulkRelease?.(cids)}
        onBulkPublish={(cids) => onBulkPublish?.(cids)}
      />
    {:else}
      <Breadcrumbs path={breadcrumbPath} onNavigate={onNavigateFolder} />

      {#if viewMode === 'list'}
        <FileList {items} {selectedCid} {selectedSidecarId} onItemClick={handleItemClick} />
      {:else}
        <FileGrid {items} {selectedCid} {selectedSidecarId} onItemClick={handleItemClick} />
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
