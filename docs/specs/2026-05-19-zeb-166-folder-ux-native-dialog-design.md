# ZEB-166 — Replace `window.prompt`/`window.alert` folder UX (design)

**Date:** 2026-05-19
**Ticket:** [ZEB-166](https://linear.app/zeblith/issue/ZEB-166)
**Parent:** ZEB-158 (File Manager slice 1)
**Predecessor:** ZEB-299 (inline rename) — completed PR #134, 2026-05-19

## Goal

Remove every `window.prompt` and `window.alert` call from `FileBrowser.svelte`. Replace with in-app UI that:

- creates folders without leaving app chrome,
- surfaces folder-load errors non-modally with a retry affordance,
- surfaces folder-create errors inline, in the same shape ZEB-299 established for rename.

This is purely a UX-layer change. The backend `create_folder` IPC and the `listFolderContents` service path are untouched.

## Context — what's there now

Three call sites in `FileBrowser.svelte`:

1. **`handleNewFolder` (line 638)** — `window.prompt('Folder name:')` for input; on backend error, `window.alert('Could not create folder: …')`. The "+" button in `BrowserToolbar` dispatches `onNewFolderClick`, which is wired to `handleNewFolder`.
2. **Folder-fetch effect (line 264)** — when `service.listFolderContents(cid)` rejects (malformed manifest, consistency check, event-loop drop), it logs and `window.alert('Could not load folder: …')`, then sets `folderItems = { cid, items: [] }` so the UI doesn't keep painting the previous folder's contents.
3. **Folder-identity-not-loaded guard inside `handleNewFolder`** — when a nested create is requested but the top-level sidecar id isn't known yet (rare race: programmatic nav before `list_content` settles), `window.alert` tells the user to navigate to root and back.

The accessibility argument is the headline: `window.prompt` has no focus trap, no programmatic label, no inline validation, no way to keep the input open after a backend error. Cross-platform styling is also inconsistent.

## Design decisions (confirmed 2026-05-19)

| Decision | Choice | Rationale |
|---|---|---|
| New-folder input shape | **Finder-style inline placeholder row** at top of file list/grid | Mirrors ZEB-299 rename — users just learned the inline-edit pattern. No new floating-modal infra. |
| Folder-load error surface | **Inline retry block** in the file-list area | Ticket's explicit recommendation. Replaces the empty/items render so the user can't act on stale content; gives a clear `[Retry]` action. |
| Folder-create backend error | **Inline error near the placeholder row** | Symmetric with rename's `renameError`. Keeps placeholder open so the user can fix the name and retry. |

## Frontend design

### State (new in `FileBrowser.svelte`)

```ts
// ZEB-166 inline new-folder state. Mirrors the ZEB-299 rename
// state shape — see beginRename/cancelRename/commitRename.
let creatingFolder = $state(false);
let newFolderName = $state('');
let newFolderError = $state<string | null>(null);

// Counter mirrors renameInFlightCount — the create IPC can be
// in-flight while the user clicks elsewhere; we suppress the
// placeholder-row blur-cancel during the await window.
let creatingFolderInFlightCount = $state(0);
let creatingFolderInFlight = $derived(creatingFolderInFlightCount > 0);

// Tagged with the cid the failure belongs to, so a fast nav-away
// auto-discards the stale banner (mirrors folderItems.cid guard).
let folderLoadError = $state<{ cid: string; message: string } | null>(null);

// Bump-to-retry token consumed by the fetch effect's dep list.
let folderFetchRetryToken = $state(0);
```

### Helpers (in `FileBrowser.svelte`)

```ts
function beginCreateFolder() {
  creatingFolder = true;
  newFolderName = '';
  newFolderError = null;
}

function cancelCreateFolder() {
  creatingFolder = false;
  newFolderName = '';
  newFolderError = null;
}

async function commitCreateFolder() {
  if (!creatingFolder) return;
  const trimmed = newFolderName.trim();
  if (!trimmed) {
    newFolderError = 'Name cannot be empty';
    return;
  }

  const wasNestedCreate = breadcrumbStack.length > 0;
  const parentSidecarId = wasNestedCreate
    ? navStack[0]?.sidecarId ?? null
    : null;

  if (wasNestedCreate && !parentSidecarId) {
    newFolderError =
      'Folder identity not yet loaded. Return to root and navigate back, then retry.';
    return;
  }

  creatingFolderInFlightCount++;
  try {
    await service.createFolder(trimmed, parentSidecarId, breadcrumbStack);
    cancelCreateFolder();
    if (wasNestedCreate) onNavigateFolder(null);
  } catch (err) {
    const raw = err instanceof Error ? err.message : String(err);
    newFolderError = raw.replace(/^Error:\s*/, '');
    // Keep placeholder open so the user can fix and retry.
  } finally {
    creatingFolderInFlightCount--;
  }
}

function retryFolderLoad() {
  folderLoadError = null;
  folderFetchRetryToken++; // re-trigger the fetch effect
}
```

The trio (`beginCreateFolder` / `cancelCreateFolder` / `commitCreateFolder`) is the only place that mutates the new-folder state.

### Wiring `handleNewFolder` → `beginCreateFolder`

```ts
// Replaces the window.prompt body. The "+" toolbar button now opens
// inline edit instead of a browser dialog.
function handleNewFolder() {
  beginCreateFolder();
}
```

### Folder-fetch effect (replace `window.alert` path)

The existing effect already tracks `currentFolderCid` and `serviceVersion`. Add `folderFetchRetryToken` as a third dep, and on reject set the tagged error instead of alerting:

```ts
$effect(() => {
  void serviceVersion;
  void folderFetchRetryToken;
  const cid = currentFolderCid;
  const mySeq = ++folderFetchSeq;
  if (!cid) {
    folderItems = null;
    folderLoadError = null;
    return;
  }
  // Clear any prior load error for the new fetch attempt.
  folderLoadError = null;
  service
    .listFolderContents(cid)
    .then((result) => {
      if (currentFolderCid === cid && mySeq === folderFetchSeq) {
        folderItems = { cid, items: result };
      }
    })
    .catch((err) => {
      if (currentFolderCid === cid && mySeq === folderFetchSeq) {
        const msg = err instanceof Error ? err.message : String(err);
        console.error('listFolderContents failed:', err);
        folderLoadError = { cid, message: msg };
        folderItems = { cid, items: [] }; // clear stale items
      }
    });
});
```

`folderLoadError` is rendered only when `folderLoadError.cid === currentFolderCid`, so a fast nav-away discards it on the next render.

### Auto-clear on navigation

The existing nav `$effect` already nulls `error` (ZEB-162 move) and calls `cancelRename()` (ZEB-299, round 4). Extend it:

```ts
$effect(() => {
  const cid = currentFolderCid;
  untrack(() => {
    error = null;
    cancelRename();
    cancelCreateFolder();   // round-trip the placeholder away on nav
    folderLoadError = null; // stale across folders, even before fetch fires
    // ...rest unchanged
  });
});
```

### Inline placeholder row in `FileList.svelte` and `FileGrid.svelte`

Two new props on each:

```ts
creatingFolder?: boolean;
newFolderName?: string; // $bindable
newFolderError?: string | null;
creatingFolderInFlight?: boolean;
onCommitCreateFolder?: () => void;
onCancelCreateFolder?: () => void;
```

Rendered before the items list (pre-pended, since folders sort first):

```svelte
{#if creatingFolder}
  <FilePlaceholderRow
    bind:value={newFolderName}
    error={newFolderError}
    inFlight={creatingFolderInFlight}
    onCommit={onCommitCreateFolder}
    onCancel={onCancelCreateFolder}
  />
{/if}
{#each items as item ...}
  ...
{/each}
```

`FilePlaceholderRow.svelte` (new) is a thin wrapper:

- folder icon (matches `categoryIcon('bundle')` so the visual lines up)
- `<input>` with `use:autoFocus`, two-way bound to `value`
- Enter → `onCommit`, Escape → `onCancel`, blur → `onCancel` (gated on `!inFlight`, same as rename)
- error message rendered below the input when `error` is non-null (the only inline error surface — no banner)

`FileGrid.svelte` gets the same prop set + a sibling `FilePlaceholderCard.svelte` for visual symmetry.

### Inline retry block

In `FileBrowser.svelte`'s render section, where `<FileList>` / `<FileGrid>` are chosen, branch on `folderLoadError`:

```svelte
{#if folderLoadError && folderLoadError.cid === currentFolderCid}
  <FolderLoadError
    message={folderLoadError.message}
    onRetry={retryFolderLoad}
  />
{:else if viewMode === 'list'}
  <FileList ... />
{:else}
  <FileGrid ... />
{/if}
```

`FolderLoadError.svelte` (new) renders the inline ⚠ block + `[Retry]` button. Plain component, no two-way binding.

### Out of scope (deliberately deferred)

- A unified toast/notification system. Current pattern of per-feature `$state<string | null>` banners stays — three banners (move, rename, plus our new inline-create error) is the bar; not enough yet to justify a notification abstraction.
- A keyboard shortcut to enter create mode (no `Ctrl+N` etc.). Toolbar button stays the entry point.
- Disabling the "+" button preemptively when nested-create can't succeed (no `parentSidecarId`). Validation fires on commit instead — simpler than maintaining a disabled state for an edge case.
- Distinguishing transient vs permanent fetch errors (the ticket mentions this; deferred — backend error strings don't carry a discriminant yet).
- Animation/transitions for the placeholder row. Plain show/hide.

## Backend

None. `create_folder` and `list_content` IPCs are unchanged.

## Test plan

### Frontend (vitest, `src/lib/components/__tests__/file-browser-create-folder.test.ts` — new file)

Mirror the ZEB-299 `file-browser-rename.test.ts` patterns. Each test renders the real `FileBrowser` with a mocked adapter, just like the rename tests.

| # | Test | What it asserts |
|---|---|---|
| 1 | click `+` opens placeholder row | input rendered, focused, value empty |
| 2 | Enter on valid name fires `create_folder` IPC | adapter invoked with `{name, parentSidecarId, parentPath}` |
| 3 | Enter on empty/whitespace shows inline `'Name cannot be empty'` | no IPC fired |
| 4 | Escape cancels — placeholder gone, no IPC | |
| 5 | Blur cancels — placeholder gone | matches rename's "Finder parity" test |
| 6 | Blur during in-flight IPC is a no-op | input survives a rejection (mirror rename's round-5 test) |
| 7 | Backend rejection keeps placeholder open with the error inline | `'A folder named X already exists'` rendered below input |
| 8 | Navigating away while creating clears placeholder state | `cancelCreateFolder` called from nav effect |
| 9 | Folder-load error renders the inline retry block | `[Retry]` button visible |
| 10 | Retry button re-fires the fetch | `listFolderContents` called twice |
| 11 | Nav-away while error showing clears the banner | `folderLoadError = null` after nav |

### No new Rust tests

Backend IPCs are unchanged. The existing `create_folder` integration coverage stays load-bearing.

## Acceptance (from the ticket)

- ✓ No `window.prompt` or `window.alert` calls remain in `FileBrowser.svelte`.
- ✓ New-folder flow works without leaving app chrome.
- ✓ Folder-load errors are surfaced non-modally with retry.

## File inventory

**Modified:**
- `src/lib/components/FileBrowser.svelte` — state + helpers + render branch + nav-effect extension
- `src/lib/components/FileList.svelte` — new prop set + placeholder render
- `src/lib/components/FileGrid.svelte` — new prop set + placeholder render

**Created:**
- `src/lib/components/FilePlaceholderRow.svelte` — input row used by `FileList`
- `src/lib/components/FilePlaceholderCard.svelte` — input card used by `FileGrid`
- `src/lib/components/FolderLoadError.svelte` — inline retry block
- `src/lib/components/__tests__/file-browser-create-folder.test.ts` — 11 UI tests

**Deleted:** none.

**Unchanged:**
- `src/lib/file-manager-service.ts` — `createFolder` already returns/throws cleanly
- `src-tauri/src/lib.rs` — `create_folder` IPC unchanged
- `src/lib/components/BrowserToolbar.svelte` — `onNewFolderClick` wiring keeps the same contract (the FileBrowser handler just changes its body)
