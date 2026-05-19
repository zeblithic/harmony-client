# ZEB-163 — Folder-as-root OS upload (design)

**Date:** 2026-05-19
**Ticket:** [ZEB-163](https://linear.app/zeblith/issue/ZEB-163)
**Parent:** ZEB-158 (File Manager slice 1)
**Predecessor:** ZEB-166 — completed PR #136, 2026-05-19

## Goal

Let users drag a folder from Finder/Explorer onto the Harmony File Manager (or pick one via a "Add folder…" toolbar button) and have the entire tree ingested in one operation: files become leaf entries, directories become folder manifests built bottom-up, and the whole tree appears as a single new folder entry under the drop target.

## Context — what's there now

From the codebase survey (2026-05-19):

- **Single-file ingest** (`ingest_content`, lib.rs:5930) is **dialog-gated** — opens `rfd` picker, reads bytes, drives `send_ingest()`. No path-based variant exists.
- **Folder manifest construction** (`folders.rs::build_folder`, line 71) accepts `children: &[ManifestEntry]` directly. The `create_folder` IPC (lib.rs:6094) wraps `build_folder` + sidecar record.
- **Sidecar model** (settled by ZEB-158 slice 1): one row per user-visible folder root; nested folders within a manifest **don't get sidecar rows** until the user navigates in and acts on them. This matters: the walker must distinguish "ingest a sub-directory's manifest as a bundle" (no sidecar) from "create the root folder under the drop target" (sidecar).
- **Flat-bundle cap** `FLAT_BUNDLE_MAX` (~8 GiB, lib.rs:91). `ingest_dispatch` returns a clean error above this. ZEB-161 (nested-bundle) isn't done, so the walker must pre-check and skip oversized leaves.
- **Tauri drag-drop wiring**: not present. `tauri::Builder::default()` has no `.on_window_event(...)`. We add it.
- **Progress-event precedent**: `channel-backfill-progress` (channel_message_service.ts:92) — Tauri `emit()` + frontend `adapter.listen()`. We follow the same shape.
- **Existing frontend drag-drop**: only Harmony-internal moves via `application/x-harmony-content` MIME (FileBrowser.svelte:25). OS-folder drops go through Tauri's window-event channel, not the HTML5 ondrop — no collision.

## Design decisions (confirmed 2026-05-19)

| Decision | Choice | Rationale |
|---|---|---|
| Entry surface | **Both: drag-drop + "Add folder…" picker** | Same backend walker for both, marginal extra cost. Picker is the reliability fallback if Tauri drag-drop misbehaves on Windows. |
| Atomicity on partial failure | **Best-effort: persist successes** | ZEB-157 (rollback) isn't done. Best-effort matches the ticket's "Decomposed out of ZEB-158" framing — deliver the primitive now, layer rollback later. |
| Progress + cancellation | **Modal progress with Cancel button** | A misclicked 50 GB photo library needs an out. Cancel sets a per-job `AtomicBool`; walker checks at each node and unwinds cleanly. |
| Filter policy | **Deny hidden/junk + skip symlinks + skip oversized** | Skip `.DS_Store`, `Thumbs.db`, `.git/**`, dotfiles. Don't follow symlinks (avoid loops + surprise data). Files > `FLAT_BUNDLE_MAX` skipped with a count. All skips appear in the summary modal. |

## Architecture

```text
┌──────────────────────────────────────────────────────────────────────┐
│  Frontend (FileBrowser.svelte)                                        │
│                                                                       │
│   ┌──────────────────┐   ┌────────────────────────┐                  │
│   │ BrowserToolbar   │   │ FileBrowser pane       │                  │
│   │ [+] [Add folder] │   │ <- OS drop zone        │                  │
│   └────────┬─────────┘   └────────┬───────────────┘                  │
│            │                       │                                  │
│            │ dialog.open           │ listen('os-folder-dropped')      │
│            │ ({directory:true})    │                                  │
│            ▼                       ▼                                  │
│       ┌─────────────────────────────────────────┐                    │
│       │  service.ingestFolderTree(              │                    │
│       │    path, parentSidecarId, parentPath    │                    │
│       │  ) -> Promise<IngestFolderTreeResult>   │                    │
│       └─────────────────────┬───────────────────┘                    │
│                             │                                         │
│             ┌───────────────┴────────────┐                           │
│             ▼                            ▼                           │
│   ┌──────────────────────┐    ┌──────────────────────┐              │
│   │ FolderIngestProgress │    │ FolderIngestSummary  │              │
│   │ Modal (during walk)  │    │ Modal (on settle)    │              │
│   │ - x of y, current    │    │ - succeeded N        │              │
│   │ - [Cancel] button    │    │ - skipped {h,s,o}    │              │
│   │ listen('folder-      │    │ - failed [...]       │              │
│   │  ingest-progress')   │    │                      │              │
│   └──────────┬───────────┘    └──────────────────────┘              │
│              │                                                       │
│              │ Cancel -> service.cancelFolderIngest(jobId)           │
└──────────────┼───────────────────────────────────────────────────────┘
               │
               ▼
┌──────────────────────────────────────────────────────────────────────┐
│  Backend (Tauri)                                                      │
│                                                                       │
│   ingest_folder_tree IPC                                             │
│   - receives jobId (UUID) minted by frontend                          │
│   - registers CancellationToken keyed by incoming jobId               │
│   - spawns walker task                                                │
│   - returns Promise that resolves with summary when walker settles    │
│                                                                       │
│   cancel_folder_ingest(jobId) IPC                                    │
│   - sets the registered token                                         │
│                                                                       │
│   on_window_event(WindowEvent::DragDrop(Drop{paths, position}))      │
│   - for each path: if dir, emit 'os-folder-dropped' { path, pos }    │
│                                                                       │
│   Walker (depth-first, bottom-up)                                    │
│   - per node: check cancel; check filter; check symlink              │
│   - dir: recurse children -> children[ManifestEntry] -> ingest        │
│     manifest as bundle -> ManifestEntry { cid, name, Folder }        │
│   - file: ingest_file_at_path -> ManifestEntry { cid, name, Leaf }   │
│   - root dir: call create_folder so sidecar row gets created         │
│   - emit 'folder-ingest-progress' { jobId, completed, total, path }  │
└──────────────────────────────────────────────────────────────────────┘
```

## Backend design

### New IPCs

```rust
// Frontend mints jobId via crypto.randomUUID() before invoking, so Cancel
// works from the first frame. Promise resolves with summary when walker settles.
#[tauri::command]
async fn ingest_folder_tree(
    app: AppHandle,
    state: State<'_, AppState>,
    job_id: String, // camelCase `jobId` in JS payload; minted by frontend
    root_path: String,
    parent_sidecar_id: Option<SidecarId>,
    parent_path: Vec<String>, // breadcrumbStack
) -> Result<IngestFolderTreeResult, String>;

#[tauri::command]
fn cancel_folder_ingest(
    state: State<'_, AppState>,
    job_id: String,
) -> Result<(), String>;
```

### Result shape

```rust
#[derive(Serialize)]
pub struct IngestFolderTreeResult {
    pub job_id: String,
    pub root_sidecar_id: Option<SidecarId>, // None if cancelled before root settled
    pub root_cid: Option<String>,
    pub root_name: String,
    pub total_files_seen: u64,
    pub pre_walk_total: i64, // pre-walk leaf count; -1 if pre-walk failed
    pub succeeded: u64,
    pub skipped: SkipCounts,
    pub failed: Vec<FailedEntry>, // path + message; bounded list (cap at 50, overflow counter)
    pub cancelled: bool,
}

#[derive(Serialize, Default)]
pub struct SkipCounts {
    pub hidden: u64,    // dotfiles + .DS_Store + Thumbs.db + .git/**
    pub symlink: u64,
    pub oversized: u64, // > FLAT_BUNDLE_MAX
    pub other: u64,     // FIFOs, sockets, block/char devices — non-addressable nodes
}

#[derive(Serialize)]
pub struct FailedEntry {
    pub path: String,
    pub message: String,
}
```

### Progress events

```rust
// Emitted per leaf-file ingest. Directory manifest builds are NOT
// counted here — pre_walk_count only enumerates non-filtered leaf
// files, so emitting on dir builds would push `completed` past `total`.
#[derive(Clone, Serialize)]
struct FolderIngestProgressEvent {
    job_id: String,
    completed: u64,
    total: i64,             // pre-walk count of non-filtered leaves; -1 if pre-walk failed
    current_path: String,   // relative to root_path
}
```

We do a **pre-walk** (just `walkdir` + filter, no I/O beyond stat) to compute `total` so the progress bar can be determinate. The pre-walk is cheap (~10k files in a few ms) and worth the determinism. If pre-walk fails (rare: permission errors), we ship `total = -1` and the modal shows an indeterminate bar.

### Cancellation registry

```rust
// In AppState:
pub folder_ingest_jobs: Arc<DashMap<String, Arc<AtomicBool>>>,
```

`ingest_folder_tree` inserts a fresh `AtomicBool` keyed by the incoming `job_id`; walker holds an `Arc<AtomicBool>` and checks it at each node entry. `cancel_folder_ingest` sets the flag. On walker exit (success/cancel/fail), it removes the entry from the registry.

### Internal helpers (Task 1)

```rust
// New in src-tauri/src/lib.rs — internal, not an IPC.
async fn ingest_file_at_path(
    state: &AppState,
    path: &Path,
    parent_sidecar_id: Option<SidecarId>,
    file_name: String,
) -> Result<IngestResult, IngestError> {
    let metadata = tokio::fs::metadata(path).await?;
    let size = metadata.len();
    if size > FLAT_BUNDLE_MAX {
        return Err(IngestError::Oversized { size, cap: FLAT_BUNDLE_MAX });
    }
    let bytes = tokio::fs::read(path).await?; // OK: pre-checked < cap
    // Reuse the existing ingest_dispatch path. Bypasses the rfd dialog.
    send_ingest_with_name(state, bytes, file_name, parent_sidecar_id).await
}
```

`send_ingest_with_name` is a thin extraction from the existing `ingest_content` body — the part *after* the dialog returns bytes. If `ingest_content` doesn't currently have that seam, Task 1 introduces it (refactor `ingest_content` to delegate to `send_ingest_with_name` after the dialog).

### Walker (Task 2)

```rust
async fn walk(
    state: &AppState,
    app: &AppHandle,
    job_id: &str,
    cancel: &Arc<AtomicBool>,
    path: &Path,
    name: String,
    is_root: bool,
    parent_sidecar_id: Option<SidecarId>,
    counters: &mut WalkCounters,
) -> WalkOutcome {
    if cancel.load(Ordering::Relaxed) {
        return WalkOutcome::Cancelled;
    }

    // Symlink check FIRST — symlinks to dirs would otherwise be walked.
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(m) => m,
        Err(e) => return WalkOutcome::Failed(e.to_string()),
    };
    if metadata.file_type().is_symlink() {
        counters.skipped.symlink += 1;
        return WalkOutcome::Skipped;
    }
    if should_filter_name(&name) {
        counters.skipped.hidden += 1;
        return WalkOutcome::Skipped;
    }

    if metadata.is_dir() {
        let mut children: Vec<ManifestEntry> = Vec::new();
        let mut entries = collect_sorted_dir_entries(path).await?;
        for entry in entries {
            match walk(state, app, job_id, cancel, &entry.path, entry.name.clone(),
                      false, None, counters).await {
                WalkOutcome::Ingested(manifest_entry) => children.push(manifest_entry),
                WalkOutcome::Skipped => {} // counted in counters
                WalkOutcome::Failed(msg) => counters.record_fail(&entry.path, msg),
                WalkOutcome::Cancelled => return WalkOutcome::Cancelled,
            }
        }

        // Build the directory's manifest.
        if is_root {
            // Root: full create_folder so a sidecar row appears under the drop target.
            let result = create_folder_with_children(
                state, name.clone(), parent_sidecar_id, children
            ).await?;
            counters.root_sidecar_id = Some(result.sidecar_id);
            counters.root_cid = Some(hex(result.cid));
            emit_progress(app, job_id, counters, path);
            WalkOutcome::Ingested(ManifestEntry { cid: result.cid, name, kind: Folder })
        } else {
            // Nested: manifest-only (no sidecar). build_folder returns the CID
            // for the parent manifest to reference.
            let cid = build_folder_manifest_only(state, name.clone(), children).await?;
            emit_progress(app, job_id, counters, path);
            WalkOutcome::Ingested(ManifestEntry { cid, name, kind: Folder })
        }
    } else {
        if metadata.len() > FLAT_BUNDLE_MAX {
            counters.skipped.oversized += 1;
            return WalkOutcome::Skipped;
        }
        match ingest_file_at_path(state, path, None, name.clone()).await {
            Ok(result) => {
                counters.succeeded += 1;
                emit_progress(app, job_id, counters, path);
                WalkOutcome::Ingested(ManifestEntry { cid: result.cid, name, kind: Leaf })
            }
            Err(e) => WalkOutcome::Failed(e.to_string()),
        }
    }
}

fn should_filter_name(name: &str) -> bool {
    name == ".DS_Store"
        || name == "Thumbs.db"
        || name == ".git"
        || name == "desktop.ini"
        || name.starts_with('.') // covers all dotfiles incl. .git for safety
}
```

**Note on `create_folder_with_children` / `build_folder_manifest_only`:** ZEB-158 slice 1 may or may not expose these helpers directly. Task 2 implementer should:
1. First read folders.rs and lib.rs:6094 (`create_folder` IPC body).
2. If `build_folder` already returns just a CID and `create_folder` wraps it with sidecar record, factor out the two paths.
3. If `build_folder` is monolithic, extract the manifest-only path as a sibling function.

### Drag-drop wiring (Task 3)

```rust
// In lib.rs setup, before .run():
.on_window_event(|window, event| {
    use tauri::{WindowEvent, DragDropEvent};
    if let WindowEvent::DragDrop(DragDropEvent::Drop { paths, position }) = event {
        for path in paths {
            let path_str = path.to_string_lossy().to_string();
            let is_dir = path.is_dir();
            let payload = serde_json::json!({
                "path": path_str,
                "x": position.x,
                "y": position.y,
            });
            let event_name = if is_dir { "os-folder-dropped" } else { "os-file-dropped" };
            let _ = window.emit(event_name, payload);
        }
    }
})
```

**Cross-platform notes:**
- macOS/Linux: `path` is the absolute filesystem path. Works as-is.
- Windows: `path` may have backslash separators; the Rust walker uses `Path` which handles both.
- `position` is window-relative pixel coords. Frontend uses this only to display the drop indicator briefly; v1 always drops into `currentFolderCid` (whatever the user is viewing), not a hover-resolved sub-folder.

## Frontend design

### State (in FileBrowser.svelte)

```ts
// ZEB-163 folder-ingest state — single in-flight ingest at a time.
let activeIngestJobId = $state<string | null>(null);
let activeIngestProgress = $state<{
  completed: number;
  total: number;
  currentPath: string;
} | null>(null);
let ingestSummary = $state<IngestFolderTreeResult | null>(null);
let cancelRequested = $state(false);
```

### Service additions (file-manager-service.ts)

```ts
async ingestFolderTree(
  jobId: string, // frontend-minted via crypto.randomUUID()
  rootPath: string,
  parentSidecarId: string | null,
  parentPath: string[],
): Promise<IngestFolderTreeResult> {
  return this.adapter.invoke('ingest_folder_tree', {
    jobId, rootPath, parentSidecarId, parentPath,
  });
}

async cancelFolderIngest(jobId: string): Promise<void> {
  return this.adapter.invoke('cancel_folder_ingest', { jobId });
}
```

### Entry handlers (in FileBrowser.svelte)

```ts
async function handleAddFolderClick() {
  // Guard MUST set a synchronous lock before the picker await, otherwise
  // two fast clicks both pass the in-flight check and both spawn ingests.
  // activeIngestJobId is set only by the first progress event (async), so
  // we use activeIngestProgress (set synchronously in startFolderIngest)
  // OR a pickerOpen flag here.
  if (activeIngestProgress || pickerOpen) return;
  pickerOpen = true;
  try {
    const picked = await dialog.open({ directory: true, multiple: false });
    if (!picked || typeof picked !== 'string') return;
    startFolderIngest(picked);
  } finally {
    pickerOpen = false;
  }
}

async function startFolderIngest(rootPath: string) {
  const isNestedIngest = breadcrumbStack.length > 0;
  const parentSidecarId = isNestedIngest ? navStack[0]?.sidecarId ?? null : null;
  if (isNestedIngest && !parentSidecarId) {
    error = 'Folder identity not yet loaded. Return to root and navigate back, then retry.';
    return;
  }
  // Frontend mints jobId synchronously so Cancel works from frame 1
  // (before the first progress event arrives). Listener guards on this
  // value to discard stale events from prior ingests.
  const jobId = crypto.randomUUID();
  activeIngestJobId = jobId;
  activeIngestProgress = { completed: 0, total: -1, currentPath: rootPath };
  cancelRequested = false;
  try {
    const result = await service.ingestFolderTree(jobId, rootPath, parentSidecarId, breadcrumbStack);
    ingestSummary = result;       // render summary BEFORE clearing guards
    resetIngestState();           // clears jobId/progress/cancelRequested
    serviceVersion++;             // re-fetch folder listing
  } catch (err) {
    resetIngestState();
    error = `Folder ingest failed: ${err instanceof Error ? err.message : String(err)}`;
  }
}

function handleCancelIngest() {
  if (!activeIngestJobId) return;
  cancelRequested = true;
  service.cancelFolderIngest(activeIngestJobId).catch(() => {
    // Cancel is best-effort; backend may have already settled.
  });
}
```

### OS-drop event listener

```ts
$effect(() => {
  const unsubscribe = adapter.listen<{ path: string; x: number; y: number }>(
    'os-folder-dropped',
    (event) => {
      if (activeIngestJobId) return; // already ingesting — ignore
      startFolderIngest(event.payload.path);
    },
  );
  return () => { unsubscribe.then(fn => fn()); };
});
```

### Progress event listener

```ts
$effect(() => {
  const unsubscribe = adapter.listen<FolderIngestProgressEvent>(
    'folder-ingest-progress',
    (event) => {
      if (event.payload.jobId !== activeIngestJobId) return;
      activeIngestProgress = {
        completed: event.payload.completed,
        total: event.payload.total,
        currentPath: event.payload.currentPath,
      };
    },
  );
  return () => { unsubscribe.then(fn => fn()); };
});
```

Note: `activeIngestJobId` is set synchronously by `startFolderIngest` (frontend mints the UUID via `crypto.randomUUID()` before invoking the IPC), so the progress-event guard `payload.jobId !== activeIngestJobId` is meaningful from the very first emit. This also makes Cancel work during the pre-walk window before any progress event would have arrived.

### Components

**`FolderIngestProgressModal.svelte`** — floating modal:
- progress bar (determinate when `total > 0`, indeterminate otherwise)
- "Ingesting *N* of *M* files…" line
- current file path (truncated middle, monospaced)
- `[Cancel]` button → disabled + label "Cancelling…" when `cancelRequested` true
- focus-trap; Escape triggers cancel; `role="dialog"` + `aria-labelledby`

**`FolderIngestSummaryModal.svelte`** — post-ingest modal:
- headline: "Added folder *Name* with *N* files" (or "Cancelled — added *N* of *M* files" if `result.cancelled`)
- collapsible **Skipped** section if any: hidden N, symlinks N, oversized N (each as a row with explanation tooltip)
- collapsible **Failed** section if any: list of path + message (cap at 50; "and X more" if overflow)
- `[Done]` button → closes modal, focus returns to the new folder if visible

### Toolbar

`BrowserToolbar.svelte`: add a new icon button after the existing "+" new-folder button. Icon: folder-with-down-arrow (or 📁⬇ as a placeholder if no icon system in place). Wires `onAddFolderClick` prop.

## File inventory

**Modified:**
- `src-tauri/src/lib.rs` — `.on_window_event` setup, two new IPCs, `AppState` field for cancel registry, refactor of `ingest_content` to expose path-based seam
- `src/lib/components/FileBrowser.svelte` — state + entry handlers + event listeners + modal render branches
- `src/lib/components/BrowserToolbar.svelte` — Add-folder button + prop
- `src/lib/file-manager-service.ts` — `ingestFolderTree`, `cancelFolderIngest`

**Created:**
- `src/lib/components/FolderIngestProgressModal.svelte`
- `src/lib/components/FolderIngestSummaryModal.svelte`
- `src/lib/components/__tests__/file-browser-folder-ingest.test.ts`
- `src-tauri/tests/folder_ingest_walker_integration.rs` (or extend `folder_primitive_integration.rs`)

**Possibly created** (deferred to implementer judgment in Task 1):
- `src-tauri/src/folder_ingest.rs` — if the walker + helpers grow past ~300 lines, split out of lib.rs

**Unchanged:**
- `folders.rs` — `build_folder` and `ManifestEntry` already do what we need
- `content_index.rs` — sidecar model unchanged
- `BrowserToolbar.svelte`'s existing `+` new-folder button — entirely separate flow

## Out of scope (deliberately deferred)

- **Atomic rollback on partial failure** — gated on ZEB-157.
- **Nested-bundle ingest for files > 8 GiB** — gated on ZEB-161; oversized files are skipped with a count.
- **Drop coordinate → sub-folder resolution** — v1 drops into the currently viewed folder. Hover-resolution to nested sub-folders is a polish follow-up.
- **Multiple concurrent ingests** — serialize at the frontend (one in-flight at a time). Backend supports multiple jobs in the registry; we just don't expose it in v1.
- **Pre-walk size estimate / "this will take ~X minutes"** — pre-walk gives file count for progress; size-based ETA is a follow-up.
- **Resume after cancel** — once cancelled, the partial tree is committed as-is. User must re-drop to add the rest.
- **De-dupe by CID across drops** — relies on existing chunk-store de-dup; no UI surface for "this folder is already partially ingested."

## Test plan

### Backend (Task 8)

`src-tauri/tests/folder_ingest_walker_integration.rs` — tempdir-based fixtures:

| # | Test | What it asserts |
|---|---|---|
| 1 | Flat dir of 3 leaves | manifest has 3 entries in sorted order; root sidecar created; counters: succeeded=3, skipped=0 |
| 2 | Nested 2-level tree (root/sub/leaf) | bottom-up order: leaf ingested first, sub manifest next, root manifest last; nested manifest has no sidecar |
| 3 | Empty subdir | manifest with `children: []` builds successfully |
| 4 | Deny-list `.DS_Store` + `Thumbs.db` + `.git/HEAD` | all 3 skipped; counters.skipped.hidden == 3; not in manifest |
| 5 | Symlink to file | skipped; counters.skipped.symlink == 1 |
| 6 | Symlink to directory | not followed; counters.skipped.symlink == 1; no descent |
| 7 | File > FLAT_BUNDLE_MAX (use a stub or env-var-overridden cap for the test) | skipped; counters.skipped.oversized == 1 |
| 8 | Cancel mid-walk | walker exits cleanly; partial result returned; `cancelled: true`; `root_sidecar_id: None` if cancel before root settled |
| 9 | Per-leaf I/O error (e.g., remove permission) | counted in `failed`; walk continues; parent manifest built with surviving children |
| 10 | Pre-walk fails (unreadable root) | IPC returns error before emitting any progress; no partial sidecar |

### Frontend (Task 9)

`file-browser-folder-ingest.test.ts` — mirrors create-folder/rename setup patterns:

| # | Test | What it asserts |
|---|---|---|
| 1 | "Add folder…" button click opens directory picker | `dialog.open({directory:true})` called |
| 2 | Picker resolution fires `ingestFolderTree` IPC | adapter invoked with `{ rootPath, parentSidecarId, parentPath }` |
| 3 | OS-folder-drop event triggers ingest | listener fires `ingestFolderTree` |
| 4 | Multiple drops while in-flight are ignored | only first call to `ingestFolderTree` |
| 5 | Progress event updates modal state | rendered "X of Y" updates after listen callback |
| 6 | Cancel button triggers `cancelFolderIngest` IPC | adapter invoked; button label switches to "Cancelling…" |
| 7 | Promise resolution opens summary modal | progress modal closed; summary modal rendered with all counts |
| 8 | Summary cancelled state | headline shows "Cancelled — added N of M" |
| 9 | Summary skipped section | renders hidden/symlink/oversized rows |
| 10 | Summary failed section | renders capped list + "and X more" overflow |
| 11 | Mid-ingest navigation doesn't dismiss modals | modal stays open; `currentFolderCid` change is independent |
| 12 | Service refetch fires on completion | `serviceVersion++` triggers folder-fetch effect |

### No new playwright/e2e tests in v1

Tauri drag-drop events aren't easily simulable from Playwright; backend tests cover the IPC contract, frontend tests cover the listener-to-state plumbing. End-to-end manual verification on macOS + Windows is the gate (see Acceptance below).

## Acceptance

- ✓ Dragging a folder from Finder (macOS) onto the FileBrowser ingests the tree and opens the summary modal.
- ✓ Dragging a folder from Explorer (Windows) does the same.
- ✓ "Add folder…" toolbar button opens native picker; selection ingests identically.
- ✓ Cancel button during a large drop halts the walker; summary shows partial state.
- ✓ `.DS_Store` / `Thumbs.db` / `.git` / dotfiles do not appear in the resulting Harmony folder.
- ✓ Symlinks are not followed.
- ✓ Files > 8 GiB are skipped and counted in the summary.
- ✓ Per-leaf failures (e.g., permission error) don't abort the whole drop.
- ✓ Resulting folder is navigable via the existing folder primitive (ZEB-158 slice 1).

## Build sequence

1. **Task 1** — backend path-based ingest helper (`ingest_file_at_path` + `send_ingest_with_name` extraction). No IPC. Unit-tests internal-only.
2. **Task 2** — walker IPC (`ingest_folder_tree`, `cancel_folder_ingest`) + cancellation registry + progress events. Depends on Task 1.
3. **Task 3** — `on_window_event` wiring + `os-folder-dropped` emit. Independent of Tasks 1–2.
4. **Task 4** — frontend picker entry + service additions. Depends on Task 2.
5. **Task 5** — frontend drop-event listener. Depends on Tasks 2 + 3.
6. **Task 6** — progress modal. Depends on Task 2.
7. **Task 7** — summary modal. Depends on Task 2.
8. **Task 8** — backend walker tests. Depends on Tasks 1 + 2.
9. **Task 9** — frontend ingest UI tests. Depends on Tasks 4 + 5 + 6 + 7.
10. **Task 10** — PR + monitor convergence loop.
