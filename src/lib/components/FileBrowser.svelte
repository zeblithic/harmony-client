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

  // ZEB-162: custom MIME for drag-drop content moves. Deliberately NOT
  // text/plain — that would invite OS-level text drops (URLs, snippets)
  // to fire move handlers.
  const HARMONY_DRAG_MIME = 'application/x-harmony-content';

  /** Payload that rides on dataTransfer between the dragged row and the
   *  drop target. srcChildName and srcChildKind are carried for client-side
   *  pre-checks; the backend re-validates everything. */
  type DragPayload = {
    srcSidecarId: string;
    srcPath: string[];
    srcChildCid: string;
    srcChildName: string;
    srcChildKind: 'folder' | 'leaf';
  };

  /** `sidecarId` on a root-view folder-row drop disambiguates between
   *  multiple top-level sidecar entries that legitimately share a CID
   *  under ZEB-164's symlink-style model. Nested drops resolve their
   *  top-level via `navStack[0].sidecarId` so the field is optional
   *  there. */
  type DropTarget =
    | { kind: 'folder-row'; cid: string; sidecarId: string | null }
    | { kind: 'breadcrumb'; segIdx: number };

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

  // ZEB-162 drag-drop coordination state. `error` surfaces backend
  // failures inline (no window.alert; ZEB-166 bans those from this
  // component).
  let error = $state<string | null>(null);

  // ZEB-299 inline-rename state. `editingItem` non-null toggles the
  // matching row/card into edit mode; `editingValue` two-way-binds to
  // the input; `renameError` surfaces backend failures inline (same
  // banner pattern as `error`).
  let editingItem = $state<ContentItem | null>(null);
  let editingValue = $state('');
  let renameError = $state<string | null>(null);
  // Round 5: in-flight tracking for commitRename's await window.
  //
  // `renameInFlight` (derived) suppresses the blur-cancel on the
  // inline input so a click-away during the IPC keeps edit mode open
  // for the user to retry after a failure (the alternative — closing
  // edit mode on blur — orphaned the error banner with no input
  // visible).
  //
  // Round 6: count concurrent in-flight commits, not a single bool.
  // Slow-click on a different row CAN start a second commit before
  // the first resolves (Cursor finding 9a721e56). With a bool, the
  // first commit's `finally` would clear it while the second is
  // still awaiting, so a blur on the second commit would erroneously
  // cancel its edit mode. The counter makes the derived flag accurate
  // whenever any commit is pending.
  //
  // `renameCommitSeq` (plain) is a generation token. beginRename and
  // cancelRename bump it; commitRename captures it before its await
  // and discards the post-await state mutation if the captured token
  // is stale — covering folder navigation, blur-cancel, or a new
  // rename starting mid-IPC.
  let renameInFlightCount = $state(0);
  let renameInFlight = $derived(renameInFlightCount > 0);
  let renameCommitSeq = 0;

  // Sync navStack with currentFolderCid (driven by the parent component).
  // The effect's only reactive dependency is currentFolderCid; navStack /
  // pendingNav / items are read inside untrack() so writes to navStack
  // don't trigger an infinite re-run loop.
  $effect(() => {
    const cid = currentFolderCid;
    untrack(() => {
      // ZEB-162: auto-clear move-error banner on folder navigation —
      // the error referred to the previous folder context and is no
      // longer actionable here. ZEB-299: same goes for rename state
      // (editingItem / editingValue / renameError) — without this an
      // in-flight commitRename whose blur fires before the IPC
      // resolves would leave an orphaned banner that persists across
      // navigations.
      error = null;
      cancelRename();
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

  // ZEB-162: compute the destination args from the active drag and drop
  // target. Returns null when the drop is a no-op (drop on self, drop on
  // current parent) so the caller can silently skip without invoking the
  // backend and getting back a "source and destination are identical"
  // rejection. The backend's checks remain the final authority — this
  // pre-check is purely to avoid spurious error toasts.
  function computeMoveArgs(
    src: DragPayload,
    dropTarget: DropTarget,
  ): {
    srcSidecarId: string;
    srcPath: string[];
    srcChildCid: string;
    srcChildName: string;
    dstSidecarId: string | null;
    dstPath: string[];
  } | null {
    if (dropTarget.kind === 'folder-row') {
      // Drop on a folder row inside the current view. Destination is
      // navStack chain extended by the target folder's cid. The
      // top-level sidecar id is the same one we're currently inside;
      // when at root navStack is empty and the target folder IS the
      // top-level, so we use the dragged-from sidecarId vs the target
      // folder's sidecar — but the target row's own sidecar id isn't
      // known to this fn (the row only passes cid). For nested drops
      // (navStack non-empty) navStack[0].sidecarId is the top-level.
      // For root-view drops we re-look up the target folder's sidecar
      // from the items list.
      const dropOnSelf = src.srcChildCid === dropTarget.cid;
      if (dropOnSelf) return null;
      let dstSidecarId: string | null;
      let dstPath: string[];
      if (navStack.length === 0) {
        // Dropping onto a top-level folder row. Use the sidecarId the
        // row passed through the drop handler — under ZEB-164 multiple
        // top-level entries can share a CID, so resolving by CID alone
        // would pick the wrong sidecar branch.
        if (!dropTarget.sidecarId) return null;
        dstSidecarId = dropTarget.sidecarId;
        dstPath = [dropTarget.cid];
      } else {
        const topLevel = navStack[0].sidecarId;
        if (!topLevel) return null;
        dstSidecarId = topLevel;
        dstPath = [...navStack.map((s) => s.cid), dropTarget.cid];
      }
      // No-op when source's immediate parent equals destination parent
      // chain AND the dragged child is already a direct child. This
      // catches "drag onto the folder you're already inside of."
      if (
        src.srcSidecarId === dstSidecarId &&
        src.srcPath.length === dstPath.length &&
        src.srcPath.every((c, i) => c === dstPath[i])
      ) {
        return null;
      }
      return {
        srcSidecarId: src.srcSidecarId,
        srcPath: src.srcPath,
        srcChildCid: src.srcChildCid,
        srcChildName: src.srcChildName,
        dstSidecarId,
        dstPath,
      };
    }
    // Breadcrumb drop.
    const { segIdx } = dropTarget;
    if (segIdx === 0) {
      // Root sentinel → Case D (nested → root). When source is already
      // at root (srcPath.length === 1 && srcPath[0] === srcChildCid),
      // moving to root is a no-op.
      if (src.srcPath.length === 1 && src.srcPath[0] === src.srcChildCid) {
        return null;
      }
      return {
        srcSidecarId: src.srcSidecarId,
        srcPath: src.srcPath,
        srcChildCid: src.srcChildCid,
        srcChildName: src.srcChildName,
        dstSidecarId: null,
        dstPath: [],
      };
    }
    // Nested breadcrumb. segIdx-1 is the index into navStack (because
    // segIdx 0 is the root sentinel, segIdx 1 maps to navStack[0]).
    const navIdx = segIdx - 1;
    if (navIdx < 0 || navIdx >= navStack.length) return null;
    const topLevel = navStack[0].sidecarId;
    if (!topLevel) return null;
    const dstPath = navStack.slice(0, navIdx + 1).map((s) => s.cid);
    // Drop on the breadcrumb segment that equals the source's immediate
    // parent is a no-op (would re-add into the same folder).
    if (
      src.srcSidecarId === topLevel &&
      src.srcPath.length === dstPath.length &&
      src.srcPath.every((c, i) => c === dstPath[i])
    ) {
      return null;
    }
    return {
      srcSidecarId: src.srcSidecarId,
      srcPath: src.srcPath,
      srcChildCid: src.srcChildCid,
      srcChildName: src.srcChildName,
      dstSidecarId: topLevel,
      dstPath,
    };
  }

  async function handleMove(src: DragPayload, dropTarget: DropTarget) {
    const args = computeMoveArgs(src, dropTarget);
    if (args === null) return; // silent no-op
    try {
      await service.moveContent(args);
      error = null;
    } catch (e) {
      // Tauri error rejections are strings in production and `Error`
      // objects with `"Error: "` prefix in tests; normalise to a clean
      // message either way (per CLAUDE.md IPC error pattern).
      const raw = e instanceof Error ? e.message : String(e);
      error = raw.replace(/^Error:\s*/, '');
    }
  }

  // ZEB-299 inline-rename flow. Enter edit mode by selecting a row and
  // pressing F2 (handled here) or slow-click-on-name (handled in
  // FileRow/FileCard). The trio of helpers is the only place that
  // mutates editingItem/editingValue/renameError.
  function beginRename(item: ContentItem) {
    renameCommitSeq++;
    editingItem = item;
    editingValue = item.name;
    renameError = null;
  }

  function cancelRename() {
    renameCommitSeq++;
    editingItem = null;
    editingValue = '';
    renameError = null;
  }

  async function commitRename() {
    if (!editingItem) return;
    const item = editingItem;
    const trimmed = editingValue.trim();
    if (!trimmed) {
      renameError = 'Name cannot be empty';
      return;
    }
    if (trimmed === item.name) {
      // Same-name short-circuit — skip the IPC and exit edit mode.
      cancelRename();
      return;
    }
    // Path/sidecar computation mirrors handleRowDragStart: at root,
    // srcPath = [item.cid] and the sidecarId comes from the row;
    // nested, srcPath = navStack chain and the sidecarId is the
    // top-level root's.
    const srcPath =
      navStack.length === 0 ? [item.cid] : navStack.map((s) => s.cid);
    const srcSidecarId =
      navStack.length === 0 ? item.sidecarId : navStack[0].sidecarId ?? '';
    if (!srcSidecarId) {
      renameError = 'Cannot rename: folder identity not loaded';
      return;
    }
    // Capture a generation token before the await so a folder nav,
    // blur-cancel, or new rename that fires mid-IPC discards this
    // call's tail mutations. The in-flight count keeps the derived
    // flag accurate even with concurrent commits — see the field
    // comment above for the round-6 multi-commit hazard.
    const mySeq = ++renameCommitSeq;
    renameInFlightCount++;
    try {
      await service.renameContent({
        srcSidecarId,
        srcPath,
        srcChildCid: item.cid,
        srcChildName: item.name,
        newName: trimmed,
      });
      if (mySeq !== renameCommitSeq) return; // stale — context changed during await
      cancelRename();
    } catch (e) {
      if (mySeq !== renameCommitSeq) return; // stale — discard the rejection
      const raw = e instanceof Error ? e.message : String(e);
      renameError = raw.replace(/^Error:\s*/, '');
      // Keep edit mode open so the user can fix the name and retry.
    } finally {
      renameInFlightCount--;
    }
  }

  function handleKeyDown(e: KeyboardEvent) {
    // F2 enters rename mode for the selected row, matching desktop
    // file-manager convention. Gated on no active edit so an in-flight
    // rename can't be clobbered by a stray F2.
    if (e.key === 'F2' && !editingItem) {
      const selected = items.find(
        (i) =>
          (selectedSidecarId !== null && i.sidecarId === selectedSidecarId) ||
          (selectedSidecarId === null && i.cid === selectedCid),
      );
      if (selected) {
        e.preventDefault();
        beginRename(selected);
      }
    }
  }

  function handleRowDragStart(e: DragEvent, item: ContentItem) {
    if (!e.dataTransfer) return;
    // Build the source path: top-level CID → immediate parent CID
    // (inclusive). Root rows have no nav stack ⇒ srcPath = [item.cid]
    // (Case C source). Nested rows: navStack chain (top-level is
    // navStack[0]) — items in current view have srcPath = navStack chain.
    const srcPath =
      navStack.length === 0 ? [item.cid] : navStack.map((s) => s.cid);
    const srcSidecarId =
      navStack.length === 0 ? item.sidecarId : navStack[0].sidecarId ?? '';
    if (!srcSidecarId) return; // can't move without a top-level sidecar id
    const payload: DragPayload = {
      srcSidecarId,
      srcPath,
      srcChildCid: item.cid,
      srcChildName: item.name,
      srcChildKind: item.isFolder ? 'folder' : 'leaf',
    };
    e.dataTransfer.setData(HARMONY_DRAG_MIME, JSON.stringify(payload));
    e.dataTransfer.effectAllowed = 'move';
  }

  function handleRowDrop(e: DragEvent, targetCid: string, targetSidecarId: string | null) {
    e.preventDefault();
    const raw = e.dataTransfer?.getData(HARMONY_DRAG_MIME);
    if (!raw) return;
    let payload: DragPayload;
    try {
      payload = JSON.parse(raw) as DragPayload;
    } catch {
      return;
    }
    void handleMove(payload, {
      kind: 'folder-row',
      cid: targetCid,
      sidecarId: targetSidecarId,
    });
  }

  function handleBreadcrumbDrop(e: DragEvent, segIdx: number) {
    e.preventDefault();
    const raw = e.dataTransfer?.getData(HARMONY_DRAG_MIME);
    if (!raw) return;
    let payload: DragPayload;
    try {
      payload = JSON.parse(raw) as DragPayload;
    } catch {
      return;
    }
    void handleMove(payload, { kind: 'breadcrumb', segIdx });
  }

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

<!-- ZEB-299: the root container catches F2 so a row-level focus isn't required; the region role + label keep this discoverable to AT. -->
<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<div
  class="file-browser"
  role="region"
  aria-label="File browser"
  tabindex="-1"
  onkeydown={handleKeyDown}
>
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
      <Breadcrumbs
        path={breadcrumbPath}
        onNavigate={onNavigateFolder}
        onSegmentDrop={handleBreadcrumbDrop}
        dragMime={HARMONY_DRAG_MIME}
      />

      {#if error}
        <div class="file-browser-error" role="alert">{error}</div>
      {/if}

      {#if renameError}
        <div class="file-browser-error" role="alert">{renameError}</div>
      {/if}

      {#if viewMode === 'list'}
        <FileList
          {items}
          {selectedCid}
          {selectedSidecarId}
          onItemClick={handleItemClick}
          onRowDragStart={handleRowDragStart}
          onRowDrop={handleRowDrop}
          {editingItem}
          bind:editingValue
          {renameInFlight}
          onBeginRename={beginRename}
          onCommitRename={commitRename}
          onCancelRename={cancelRename}
        />
      {:else}
        <FileGrid
          {items}
          {selectedCid}
          {selectedSidecarId}
          onItemClick={handleItemClick}
          onRowDragStart={handleRowDragStart}
          onRowDrop={handleRowDrop}
          {editingItem}
          bind:editingValue
          {renameInFlight}
          onBeginRename={beginRename}
          onCommitRename={commitRename}
          onCancelRename={cancelRename}
        />
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

  .file-browser-error {
    margin: 8px 12px;
    padding: 8px 12px;
    border-radius: 4px;
    background: color-mix(in srgb, var(--danger, #ed4245) 12%, transparent);
    border: 1px solid var(--danger, #ed4245);
    color: var(--text-primary, #f2f3f5);
    font-size: 0.85rem;
  }
</style>
