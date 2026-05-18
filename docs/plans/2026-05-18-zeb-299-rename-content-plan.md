# ZEB-299 rename_content — implementation plan

Reference: `docs/specs/2026-05-18-zeb-299-rename-content-design.md`

## Strategy

Two parallel work streams that share an IPC contract:

- **Backend stream** — `AncestorEdit::Rename` variant, `walk_and_rebuild_chain` deepest-edit arm, `ContentIndex::set_file_name`, `rename_content` IPC, `rename_content_impl` testable seam, 2 walker unit tests, 12 integration tests.
- **Frontend stream** — `FileManagerService.renameContent`, inline-edit machinery in `FileBrowser` + `FileRow` + `FileCard`, `autoFocus` Svelte action, 2 test files.

The IPC shape is frozen by the spec — both streams can build against it without needing each other. Verification then runs locally on Windows (fmt, clippy, tsc, vitest) and via WSL Debian for the Rust serial nextest. Single commit, single push, single PR.

## Branch

`zeblith/zeb-299-harmony-client-rename-filesfolders-in-file-manager` — already created off `main` at `6d9f195` (the ZEB-162 merge commit).

## Backend changes (`src-tauri/src/lib.rs` + `src-tauri/src/content_index.rs`)

### 1. `AncestorEdit::Rename` enum variant (lib.rs ~line 6121)

Add to the existing enum after the `Replace` variant:

```rust
/// Rename an existing entry in place — find the entry by
/// `(child_name, child_cid)` and change its `name` to `new_name`. CID
/// and kind are unchanged. The lookup uses both name and CID for the
/// same shared-CID disambiguation reason `Remove` does.
Rename {
    child_cid: [u8; 32],
    child_name: String,
    new_name: String,
},
```

### 2. `walk_and_rebuild_chain` deepest-edit arm (lib.rs ~line 6201)

Add the new arm next to the existing `Remove` / `Replace` / `Append` arms:

```rust
AncestorEdit::Rename { child_cid, child_name, new_name } => {
    let target_idx = manifest
        .folder_manifest
        .entries
        .iter()
        .position(|e| e.name == child_name && e.cid == child_cid)
        .ok_or_else(|| {
            format!(
                "ancestor {} has no entry named '{}' pointing to child {}",
                hex::encode(anc_cid),
                child_name,
                hex::encode(child_cid)
            )
        })?;
    manifest.folder_manifest.entries[target_idx].name = new_name;
}
```

### 3. `ContentIndex::set_file_name` (content_index.rs, after `set_archived` around line 298)

```rust
/// Update the `file_name` of an existing sidecar entry. Returns
/// `true` if the name changed, `false` if the sidecar_id is unknown OR
/// the name was already at the target value. Not CAS-protected on the
/// name field — consistent with set_archived / set_pinned /
/// set_replication_tier (all non-CID field mutations on the sidecar
/// are non-CAS).
pub fn set_file_name(&mut self, id: &SidecarId, new_name: String) -> bool {
    let Some(entry) = self.entries.get_mut(id) else {
        return false;
    };
    if entry.file_name == new_name {
        return false;
    }
    entry.file_name = new_name;
    self.save();
    true
}
```

### 4. `RenameContentResult` struct (lib.rs, near `MoveContentResult` ~6627)

```rust
/// Result returned by `rename_content`. `src_new_cid` is `None` for the
/// top-level case (rename of a sidecar `file_name` doesn't change any
/// CID); `Some(_)` for the nested case (the immediate parent's manifest
/// was rebuilt, every ancestor's CID is new).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameContentResult {
    pub src_new_cid: Option<String>,
}
```

### 5. `rename_content` Tauri command + `rename_content_impl` (lib.rs, after `move_content`)

```rust
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn rename_content(
    src_sidecar_id: String,
    src_path: Vec<String>,
    src_child_cid: String,
    src_child_name: String,
    new_name: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<RenameContentResult, String> {
    let (ingest_tx, verb_tx, index) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        (
            guard.ingest_tx.clone().ok_or_else(|| "not connected".to_string())?,
            guard.content_verb_tx.clone().ok_or_else(|| "not connected".to_string())?,
            guard.content_index.clone(),
        )
    };
    rename_content_impl(
        src_sidecar_id, src_path, src_child_cid, src_child_name, new_name,
        ingest_tx, verb_tx, index,
    ).await
}

#[allow(clippy::too_many_arguments)]
pub async fn rename_content_impl(
    src_sidecar_id: String,
    src_path: Vec<String>,
    src_child_cid: String,
    src_child_name: String,
    new_name: String,
    ingest_tx: tokio::sync::mpsc::Sender<event_loop::IngestRequest>,
    verb_tx: tokio::sync::mpsc::Sender<event_loop::ContentVerbRequest>,
    index: std::sync::Arc<Mutex<content_index::ContentIndex>>,
) -> Result<RenameContentResult, String> {
    // Boundary validations
    let new_name_trimmed = new_name.trim();
    if new_name_trimmed.is_empty() {
        return Err("name cannot be empty".to_string());
    }
    if src_path.is_empty() {
        return Err("src_path cannot be empty".to_string());
    }
    let src_sid = parse_sidecar_id(&src_sidecar_id)?;
    let child_cid = parse_cid_hex(&src_child_cid)?;
    let src_cids: Vec<[u8; 32]> = src_path.iter().map(|h| parse_cid_hex(h)).collect::<Result<_, _>>()?;
    let src_root_old = src_cids[0];

    // Snapshot sidecar state
    let (src_entry_cid, src_entry_name) = {
        let idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        let entry = idx.get(&src_sid).ok_or_else(|| "src_sidecar_id not in sidecar".to_string())?;
        (entry.cid, entry.file_name.clone())
    };
    if src_entry_cid != src_root_old {
        return Err(format!(
            "src_sidecar_id refers to cid {} but src_path[0] is {}",
            hex::encode(src_entry_cid), hex::encode(src_root_old),
        ));
    }

    // Case dispatch: top-level vs nested
    let is_top_level = src_cids.len() == 1 && src_cids[0] == child_cid;

    if is_top_level {
        // Same-name no-op (defensive — frontend short-circuits)
        if src_entry_name == new_name_trimmed {
            return Ok(RenameContentResult { src_new_cid: None });
        }
        // Verify caller's claimed current name matches the sidecar
        if src_entry_name != src_child_name {
            return Err(format!(
                "src_child_name '{src_child_name}' does not match src sidecar entry name '{src_entry_name}'",
            ));
        }
        // Duplicate-sibling check across top-level entries
        {
            let idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
            if idx.entries().any(|e| e.sidecar_id != src_sid && e.file_name == new_name_trimmed) {
                return Err(format!("a top-level entry named '{new_name_trimmed}' already exists"));
            }
        }
        // Single sidecar field write
        let changed = {
            let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
            idx.set_file_name(&src_sid, new_name_trimmed.to_string())
        };
        if !changed {
            // Sidecar disappeared between snapshot + write OR same-name race
            return Err("rename failed: sidecar entry disappeared mid-flight".to_string());
        }
        return Ok(RenameContentResult { src_new_cid: None });
    }

    // Nested case
    let immediate_parent = src_cids[src_cids.len() - 1];
    let parent_entries = read_manifest_entries(&verb_tx, immediate_parent).await?;

    // Verify entry exists at (name, cid)
    let target_entry = parent_entries.iter().find(|e| e.name == src_child_name && e.cid == child_cid)
        .ok_or_else(|| format!(
            "src_path's immediate parent {} has no entry named '{}' pointing to child {}",
            hex::encode(immediate_parent), src_child_name, hex::encode(child_cid),
        ))?;
    let _ = target_entry; // bound for clarity; the find succeeded

    // Same-name no-op (defensive)
    if src_child_name == new_name_trimmed {
        return Ok(RenameContentResult { src_new_cid: Some(hex::encode(src_root_old)) });
    }
    // Duplicate-sibling check, skip self by (name, cid)
    if parent_entries.iter().any(|e| {
        e.name == new_name_trimmed && !(e.name == src_child_name && e.cid == child_cid)
    }) {
        return Err(format!(
            "parent folder already has an entry named '{new_name_trimmed}'",
        ));
    }

    // Drive the walker
    let mut pending_ingests: Vec<(String, Vec<u8>)> = Vec::new();
    let walked = walk_and_rebuild_chain(
        &verb_tx,
        &src_cids,
        AncestorEdit::Rename {
            child_cid,
            child_name: src_child_name,
            new_name: new_name_trimmed.to_string(),
        },
        &mut pending_ingests,
    ).await?;

    // Drain
    for (cid_hex, bytes) in pending_ingests {
        send_ingest(&ingest_tx, cid_hex, bytes).await?;
    }

    let stored_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    // Single CAS rekey
    {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        match idx.rekey(&src_sid, src_root_old, walked.new_top_level_cid, walked.new_top_level_size, stored_at_ms) {
            Ok(()) => {}
            Err(content_index::RekeyError::OldMissing) => {
                return Err("src_sidecar_id removed mid-flight — nothing to rekey".to_string());
            }
            Err(content_index::RekeyError::Conflict { actual }) => {
                return Err(format!(
                    "concurrent rekey on src_sidecar_id (now at cid {}); retry from refreshed state",
                    hex::encode(actual)
                ));
            }
        }
    }

    maintain_pin_invariant(&verb_tx, &index, src_root_old, Some(walked.new_top_level_cid)).await;

    Ok(RenameContentResult { src_new_cid: Some(hex::encode(walked.new_top_level_cid)) })
}
```

### 6. Register `rename_content` in the Tauri invoke handler

Find the `tauri::generate_handler![...]` block (search for `move_content,` to locate it) and add `rename_content,` adjacent.

### 7. Walker unit tests (lib.rs, inside `walk_and_rebuild_chain_tests` mod)

```rust
#[tokio::test]
async fn walk_and_rebuild_chain_rename_deepest() {
    // Folder F contains leaf L named "leaf". Walk [F] with
    // Rename(L → "renamed"). Verify the rebuild has the new name and
    // the leaf's CID is unchanged.
    let leaf_cid = [0x11; 32];
    let f = build_one_leaf_folder(leaf_cid);

    let mut store = HashMap::new();
    store.insert(f.bundle_cid.to_bytes(), f.bundle_bytes.clone());
    store.insert(f.manifest_cid.to_bytes(), f.manifest_bytes.clone());

    let verb_tx = spawn_seeded_verb_handler(store);
    let mut pending: Vec<(String, Vec<u8>)> = Vec::new();
    let walked = walk_and_rebuild_chain(
        &verb_tx,
        &[f.bundle_cid.to_bytes()],
        AncestorEdit::Rename {
            child_cid: leaf_cid,
            child_name: "leaf".into(),
            new_name: "renamed".into(),
        },
        &mut pending,
    ).await.expect("walk");

    let expected = folders::build_folder("", &[folders::ManifestEntry {
        cid: leaf_cid,
        name: "renamed".into(),
        kind: content_index::ContentKind::Leaf,
    }]).expect("build expected");
    assert_eq!(walked.new_top_level_cid, expected.bundle_cid.to_bytes());
    assert_eq!(pending.len(), 2);
}

#[tokio::test]
async fn walk_and_rebuild_chain_rename_missing_child() {
    let leaf_cid = [0x22; 32];
    let f = build_one_leaf_folder(leaf_cid);

    let mut store = HashMap::new();
    store.insert(f.bundle_cid.to_bytes(), f.bundle_bytes.clone());
    store.insert(f.manifest_cid.to_bytes(), f.manifest_bytes.clone());

    let verb_tx = spawn_seeded_verb_handler(store);
    let mut pending: Vec<(String, Vec<u8>)> = Vec::new();
    let err = walk_and_rebuild_chain(
        &verb_tx,
        &[f.bundle_cid.to_bytes()],
        AncestorEdit::Rename {
            child_cid: [0xEE; 32],   // not present
            child_name: "leaf".into(),
            new_name: "renamed".into(),
        },
        &mut pending,
    ).await.expect_err("missing child must error");
    assert!(err.contains("has no entry named 'leaf' pointing to child"), "got: {err}");
    assert!(pending.is_empty(), "pending must be untouched on error");
}
```

### 8. Integration tests (`src-tauri/tests/rename_content_integration.rs`, new file)

Follow `move_content_integration.rs` for harness setup (copy the same `spawn_test_runtime`, `ingest_folder`, `ingest_leaf`, `make_leaf`, `insert_top_level`, `fresh_index` helpers verbatim — keep the test file standalone, ZEB-183 owns the eventual extraction).

12 tests:

1. `rename_top_level_file` — top-level file, rename via IPC, verify sidecar `file_name` changed + CID unchanged.
2. `rename_top_level_folder` — top-level folder, same assertions plus verify the folder's children are unaffected.
3. `rename_nested_one_level_deep` — `T` contains `F`, rename `F`. Verify `T`'s sidecar rekeyed to the manifest-with-renamed-entry CID.
4. `rename_nested_two_levels_deep` — `T → A → L`, rename `L`. Verify whole chain rekeys.
5. `rename_disambiguates_siblings_with_shared_cid` — `T` contains `EmptyA` and `EmptyB` (same CID, different names). Rename `EmptyB` → `"Renamed"`. Verify rebuilt `T` has `EmptyA` and `Renamed` (the right sibling was renamed).
6. `rename_empty_name_rejected` — IPC with `""` and `"   "` both reject.
7. `rename_same_name_nested_no_op` — pass `new_name == src_child_name`. Verify no sidecar mutation, no new bytes ingested.
8. `rename_same_name_top_level_no_op` — top-level variant.
9. `rename_duplicate_sibling_rejected_nested` — `T` contains `A` and `B`, rename `A` → `"B"`. Reject.
10. `rename_duplicate_sibling_rejected_top_level` — two top-level rows `A` and `B`, rename `A` → `"B"`. Reject.
11. `rename_name_mismatch_rejected` — caller passes wrong `src_child_name`. Reject with the same error shape `move_content` uses.
12. `rename_concurrent_rekey_conflict` — arm `arm_next_rekey_conflict` on the cascade root. Verify the IPC surfaces `concurrent rekey on src_sidecar_id` and the sidecar CID is unchanged (no state mutation).

## Frontend changes

### 1. `src/lib/file-manager-service.ts`

Add `RenameContentResult` type near `MoveContentResult`:

```typescript
export type RenameContentResult = {
  srcNewCid: string | null;
};
```

Add `renameContent` method on `FileManagerService` (mirrors `moveContent`):

```typescript
async renameContent(args: {
  srcSidecarId: string;
  srcPath: string[];
  srcChildCid: string;
  srcChildName: string;
  newName: string;
}): Promise<RenameContentResult> {
  if (!this.adapter) throw new Error('adapter not connected');
  const result = (await this.adapter.invoke('rename_content', {
    srcSidecarId: args.srcSidecarId,
    srcPath: args.srcPath,
    srcChildCid: args.srcChildCid,
    srcChildName: args.srcChildName,
    newName: args.newName,
  })) as RenameContentResult;
  try {
    await this.refetchRoot();
  } catch (err) {
    console.warn('renameContent: refetchRoot failed (rename succeeded); UI may show stale list:', err);
  }
  return result;
}
```

### 2. `src/lib/actions/auto-focus.ts` (new file)

```typescript
/**
 * Svelte action: focus the node and select its content on mount.
 * Used by the inline rename input to put the cursor in the field
 * immediately and select-all so the user can retype the name fast.
 */
export function autoFocus(node: HTMLInputElement) {
  node.focus();
  node.select();
}
```

### 3. `src/lib/components/FileBrowser.svelte`

Add state (near `error`):

```typescript
let editingItem: ContentItem | null = $state(null);
let editingValue = $state('');
let renameError = $state<string | null>(null);
```

Helper functions (near `handleMove`):

```typescript
function beginRename(item: ContentItem) {
  editingItem = item;
  editingValue = item.name;
  renameError = null;
}

function cancelRename() {
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
    cancelRename();
    return;
  }
  // Path computation: top-level rename → [item.cid]; nested → navStack chain.
  const srcPath = navStack.length === 0 ? [item.cid] : navStack.map((s) => s.cid);
  const srcSidecarId = navStack.length === 0 ? item.sidecarId : navStack[0].sidecarId ?? '';
  if (!srcSidecarId) {
    renameError = 'Cannot rename: folder identity not loaded';
    return;
  }
  try {
    await service.renameContent({
      srcSidecarId,
      srcPath,
      srcChildCid: item.cid,
      srcChildName: item.name,
      newName: trimmed,
    });
    cancelRename();
  } catch (e) {
    const raw = e instanceof Error ? e.message : String(e);
    renameError = raw.replace(/^Error:\s*/, '');
    // Keep edit mode open so the user can fix the name and retry.
  }
}
```

F2 keyboard handler (gated on selection + no active edit):

```typescript
function handleKeyDown(e: KeyboardEvent) {
  if (e.key === 'F2' && !editingItem) {
    const selected = items.find((i) =>
      (selectedSidecarId !== null && i.sidecarId === selectedSidecarId) ||
      (selectedSidecarId === null && i.cid === selectedCid)
    );
    if (selected) {
      e.preventDefault();
      beginRename(selected);
    }
  }
}
```

Wire `onkeydown={handleKeyDown}` to the FileBrowser's root container (it's already a div with focus, so this works without tabindex changes).

Pass new props to `<FileList>` and `<FileGrid>`:

```svelte
<FileList
  ...existing props...
  {editingItem}
  bind:editingValue
  onBeginRename={beginRename}
  onCommitRename={commitRename}
  onCancelRename={cancelRename}
/>
```

Render `renameError` inline somewhere near the existing `error` display (use the same banner pattern).

### 4. `src/lib/components/FileList.svelte` + `FileGrid.svelte`

Add the four new props and forward them to `FileRow` / `FileCard`:

```typescript
let {
  ...existing props...,
  editingItem = null,
  editingValue = $bindable(''),
  onBeginRename,
  onCommitRename,
  onCancelRename,
}: {
  ...existing types...,
  editingItem?: ContentItem | null;
  editingValue?: string;
  onBeginRename?: (item: ContentItem) => void;
  onCommitRename?: () => void;
  onCancelRename?: () => void;
} = $props();
```

Then in each iterated row/card:

```svelte
<FileRow
  ...
  editing={editingItem !== null
    && (editingItem.sidecarId === item.sidecarId)
    && editingItem.cid === item.cid}
  bind:editValue={editingValue}
  {onCommitRename}
  {onCancelRename}
  {onBeginRename}
/>
```

### 5. `src/lib/components/FileRow.svelte` + `FileCard.svelte`

Replace the name span:

```svelte
<script>
  import { autoFocus } from '../actions/auto-focus';
  // ...existing props plus:
  let {
    ...existing...,
    editing = false,
    editValue = $bindable(''),
    onCommitRename,
    onCancelRename,
    onBeginRename,
  }: {
    ...existing types...,
    editing?: boolean;
    editValue?: string;
    onCommitRename?: () => void;
    onCancelRename?: () => void;
    onBeginRename?: (item: ContentItem) => void;
  } = $props();

  // Slow-click detection state
  let lastNameClickAt = 0;

  function handleNameClick(e: MouseEvent) {
    if (editing) return;
    if (!selected) return; // only slow-click-renames a selected row
    const now = performance.now();
    const gap = now - lastNameClickAt;
    lastNameClickAt = now;
    // 300–800ms gap → slow-click rename; faster is double-click (folder nav).
    if (gap >= 300 && gap <= 800) {
      e.stopPropagation();
      onBeginRename?.(item);
    }
  }

  function handleEditKey(e: KeyboardEvent) {
    if (e.key === 'Enter') {
      e.preventDefault();
      onCommitRename?.();
    } else if (e.key === 'Escape') {
      e.preventDefault();
      onCancelRename?.();
    }
  }
</script>
```

Then the name slot becomes:

```svelte
{#if editing}
  <input
    class="file-row-name-input"
    type="text"
    bind:value={editValue}
    onkeydown={handleEditKey}
    onblur={onCancelRename}
    onclick={(e) => e.stopPropagation()}
    use:autoFocus
  />
{:else}
  <span class="file-row-name" class:bold={selected} onclick={handleNameClick}>{item.name}</span>
{/if}
```

CSS for `.file-row-name-input`: inherit font, transparent background, 1px solid border in `--accent`, full width of the name column. Mirror for `.file-card-name-input`.

While `editing` is true, override `draggable="true"` to `draggable="false"` and skip the `ondragstart` / `ondragover` / `ondrop` handlers so drag-drop can't fire during an edit.

### 6. Frontend tests

**`src/lib/file-manager-service.rename-content.test.ts`** (new file)

Mirrors `file-manager-service.move-content.test.ts`. 5 tests:

1. Throws when no adapter connected.
2. Top-level rename: emits `rename_content` with `srcPath.length === 1 && srcPath[0] === srcChildCid`, returns `{srcNewCid: null}`.
3. Nested rename: emits with `srcPath.length >= 1`, returns `{srcNewCid: '<hex>'}`.
4. Refresh-on-success: refetchRoot fires onChange.
5. Propagates backend errors (e.g., duplicate-name) to the caller.

**`src/lib/components/__tests__/file-browser-rename.test.ts`** (new file)

Renders FileBrowser with mock data + adapter. 6 tests:

1. F2 with no selection: nothing happens.
2. F2 with selection: enters edit mode (input visible, name span gone).
3. Enter commits via `service.renameContent`.
4. Escape cancels (input gone, name span back).
5. blur cancels.
6. Empty-after-trim shows inline error, doesn't fire IPC.
7. Same-name skip: typing the same name + Enter cancels without firing IPC.

(Actually 7 tests — keep them tight.)

## Verification workflow

After all edits land:

1. **Windows local checks** (parallel where possible):
   - `cd src-tauri && cargo fmt --all` (mutate)
   - `cd src-tauri && cargo fmt --all -- --check` (verify clean)
   - `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
   - `npx tsc --noEmit` (filter for rename-related files, pre-existing voice/dialog errors are noise)
   - `npx vitest run src/lib/file-manager-service.rename-content.test.ts src/lib/components/__tests__/file-browser-rename.test.ts`

2. **WSL Debian serial nextest** (Windows test binary is unreliable; ZEB-165 silent-skip hazard):
   ```bash
   wsl -d Debian -- bash -lc "cd ~/work/zeblithic/harmony-client \
     && git fetch /mnt/c/zeblith/work/zeblithic/harmony-client zeblith/zeb-299-harmony-client-rename-filesfolders-in-file-manager \
     && git reset --hard FETCH_HEAD \
     && cd src-tauri \
     && cargo nextest run --locked --features test-fixtures -E 'test(rename_)' --test-threads=1"
   ```

3. **Commit** (single commit, message follows ZEB-162 convention).

4. **Push** to origin, **open PR** via `gh pr create`.

5. **Pushover no-block** notification when PR is up (per Jake's standing instructions).

6. **Monitoring loop** — same iterative bot-review cadence as ZEB-162 PR #133. Push ONE batch per round to minimize bot re-review load. Pushover convergence notification (block) when ready to merge.

## Notes / hazards

- **AncestorEdit::Rename ordering invariant.** The walker rebuilds the manifest in place — `entries[target_idx].name = new_name`. The manifest's serialization order is the entries order; changing the name but not the position is correct (move would re-position, rename doesn't).
- **Pin cascade unchanged for nested rename.** A rename rebuilds every ancestor up to root, so `maintain_pin_invariant` is called with `(old_root_cid, new_root_cid)` exactly like a move. For top-level rename the CID doesn't change, so no pin update needed.
- **No frontend tests against the AncestorEdit::Rename walker** — the walker is internal; tests are at the IPC and UI boundaries only.
- **Same-name no-op response shape.** Top-level returns `{ src_new_cid: None }` (no rekey happened). Nested returns `{ src_new_cid: Some(<current top-level CID>) }` to keep the frontend's CID-replacement logic uniform. Both are fine because the frontend's `refetchRoot` resyncs state from the backend regardless.
- **Slow-click vs double-click ambiguity.** Folder navigation uses single-click (existing behavior). The 300–800ms gap window for slow-click rename avoids the double-click region (browser double-click threshold is ~250ms, so 300ms+ never collides). Anything > 800ms is just two separate single-clicks.
