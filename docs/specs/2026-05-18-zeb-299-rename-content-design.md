# Rename files/folders (ZEB-299): design

## Goal

Let users rename a file or folder in place — the row's CID does not change, only its display name. Two cases: (a) the renamed item is a top-level sidecar entry, so the name lives in `ContentIndexEntry.file_name` and the rename is a single sidecar field write; (b) the renamed item is nested inside one or more folders, so the name lives in the immediate parent's manifest entry and the rename rebuilds that manifest, walks every ancestor up to the top-level root, and CAS-rekeys the top-level sidecar.

This is the rename slice of the [ZEB-158](https://linear.app/zeblith/issue/ZEB-158) umbrella. It was pulled out of [ZEB-162](https://linear.app/zeblith/issue/ZEB-162) (move) during design on 2026-05-18 because move and rename share the same ancestor-rekey machinery but are logically distinct user operations — separate PRs keep bot-review scope tight.

## Context

ZEB-162 (PR #133, merged 2026-05-18) extracted the ancestor-walk helper out of `create_folder_nested` into a reusable shape:

- `AncestorEdit` enum (`src-tauri/src/lib.rs:6121`) — `Remove { child_cid, child_name }`, `Replace { old_child_cid, new_child_cid }`, `Append { entry }`. The `Remove` variant carries `child_name` because sibling manifest entries can legitimately share a CID — name disambiguates which sibling.
- `walk_and_rebuild_chain` (`src-tauri/src/lib.rs:6156`) — takes a path of ancestor CIDs (top-level → immediate parent, inclusive) plus a `deepest_edit`. Applies the deepest edit to the bottom of the chain, propagates the resulting CID change up via auto-`Replace` at each higher level, and accumulates rebuilt `(manifest, bundle)` pairs into a caller-provided `pending_ingests` buffer (drain-then-rekey ordering).
- `read_child_manifest_entry(parent_cid, child_cid, child_name)` and `read_manifest_entries(parent_cid)` (`src-tauri/src/lib.rs:6956`, `6978`).
- `move_content_impl` (`src-tauri/src/lib.rs:6699`) — load-bearing reference for how to compose these primitives end-to-end.

Sidecar field mutators that are not CAS-protected — relevant because rename of a top-level row falls in this category:

- `ContentIndex::set_archived(id, archived) -> bool` (`src-tauri/src/content_index.rs:288`)
- `ContentIndex::set_pinned(id, pinned) -> bool` (`src-tauri/src/content_index.rs:379`)
- `ContentIndex::set_replication_tier(ids, tier) -> usize` (`src-tauri/src/content_index.rs:393`)

The rename operation reuses this machinery wholesale. The only new backend surface is one new `AncestorEdit::Rename` variant, one new `ContentIndex::set_file_name` method, and the `rename_content` IPC command. The frontend gets an inline-edit affordance on rows and cards.

## Cases

| Case | Item | Sidecar touched | Ancestor walk | Top-level CID changes |
|---|---|---|---|---|
| top-level | sidecar entry at root | one field write on `file_name` | none | no — top-level rename never changes the CID |
| nested | manifest entry inside some folder | one CAS rekey on the cascade root | one full chain rebuild | yes — every ancestor's CID changes because the immediate parent's manifest contents changed |

Dispatch is inferred from the input shape — `src_path.len() == 1 && src_path[0] == src_child_cid` is the top-level case, everything else is nested. This mirrors `move_content`'s Case C ("source IS the top-level") detection.

## IPC surface

One new Tauri command. Both cases dispatch through it:

```rust
#[tauri::command]
async fn rename_content(
    src_sidecar_id: String,              // top-level sidecar holding the renamed item
    src_path: Vec<String>,               // top-level CID (= src_sidecar_id's current CID)
                                         //   → immediate parent CID (inclusive).
                                         //   For top-level case: [src_child_cid].
                                         //   For nested case: [top_cid, ..., immediate_parent_cid].
    src_child_cid: String,               // CID of the item being renamed
    src_child_name: String,              // current name — disambiguator for shared-CID siblings,
                                         //   same role as in move_content
    new_name: String,                    // proposed new name (after frontend trim)
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<RenameContentResult, String>;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameContentResult {
    /// New top-level CID after the ancestor walk + rekey. `None` for the
    /// top-level case (rename of a sidecar row's `file_name` never changes
    /// the top-level CID — the name lives in the sidecar, not the manifest).
    pub src_new_cid: Option<String>,
}
```

The IPC parameter list intentionally mirrors `move_content`'s for the source side. Frontend can build the request from exactly the same fields the drag handler computed for move.

## Validation (at the IPC boundary, before any cache reads)

1. **`new_name` non-empty after trim.** Frontend trims; backend rejects empty (whitespace-only after trim) as `"name cannot be empty"`. Mirrors `create_folder`.
2. **`new_name == src_child_name`.** Treated as a no-op success: returns `RenameContentResult { src_new_cid: None }` for the top-level case, `RenameContentResult { src_new_cid: Some(<current top-level CID>) }` for the nested case. No CAS rekey, no sidecar write. (Frontend short-circuits this case before invoking the IPC for cleanliness; the backend re-checks defensively.)
3. **`src_path` non-empty.** `Vec<String>` is well-typed but length-0 is meaningless. Mirrors `move_content`.
4. **`src_sidecar_id` resolves and `entry.cid == src_path[0]`.** Same CAS-style boundary check as `move_content`.
5. **Duplicate-sibling name reject.**
   - Top-level case: scan `ContentIndex::entries()`, reject if any entry other than `src_sidecar_id` has `file_name == new_name`. (Skip the renamed row itself by `sidecar_id`, not by name.)
   - Nested case: read the immediate parent's manifest entries via `read_manifest_entries`, reject if any entry other than the renamed one has `name == new_name`. Skip the renamed entry by `(name, cid)` match on `(src_child_name, src_child_cid)` — same self-exclusion strategy `move_content` uses for its destination collision check.
6. **For the nested case, the entry at `(src_path[-1], src_child_cid, src_child_name)` must exist in the immediate parent's manifest.** Boundary check via `read_child_manifest_entry`.

## Backend algorithm

### Top-level case

1. Boundary checks (rules 1–5, top-level branch).
2. `ContentIndex::set_file_name(&src_sidecar_id, new_name) -> bool` — returns `true` if a field was changed, `false` if the sidecar_id wasn't found OR the name was already at the target value. Both `false` cases are non-errors here: the sidecar-not-found case is caught by rule 4 (boundary), and the same-name case is caught by rule 2 (no-op). If `set_file_name` returns `false` after we already passed rules 4 and 2, something racy happened — return an error.
3. Return `RenameContentResult { src_new_cid: None }`.

No ingest, no rekey, no pin-cascade maintenance — the bundle bytes are unchanged.

### Nested case

1. Boundary checks (rules 1–6, nested branch).
2. Read the immediate parent's manifest entries via `read_manifest_entries(verb_tx, src_path[-1])`. Used both for the duplicate-name reject (rule 5) and for the entry-exists check (rule 6).
3. Drive `walk_and_rebuild_chain(verb_tx, &src_path_cids, AncestorEdit::Rename { child_cid, child_name, new_name }, &mut pending_ingests)`. The walker:
   - Reads each ancestor's bundle + manifest.
   - At the deepest (immediate parent), applies the `Rename` edit: finds the entry by `(child_name, child_cid)` and changes its `name`.
   - At each higher level, applies the auto-`Replace { old: previous_anc_cid, new: rebuilt_anc_cid }`.
   - Returns the new top-level CID + size.
4. Drain `pending_ingests` via `send_ingest`. Drain-then-rekey ordering, same correctness reasoning as `move_content` and `create_folder_nested`.
5. Single CAS rekey: `ContentIndex::rekey(&src_sidecar_id, src_root_old, new_top_level_cid, new_top_level_size, stored_at_ms)`. On `OldMissing` / `Conflict` return the same error shape `move_content` uses.
6. `maintain_pin_invariant(verb_tx, index, src_root_old, Some(new_top_level_cid))` — best-effort Unpin(old) + Pin(new) on the verb channel, exactly the existing helper.
7. Return `RenameContentResult { src_new_cid: Some(hex::encode(new_top_level_cid)) }`.

No two-stage commit, no compensating undo — there is exactly one mutation: the top-level sidecar CAS rekey. If it succeeds, the rename committed. If it fails, no state changed (pending_ingests was already drained but those bytes are orphans, which W-TinyLFU evicts under pressure; same trade-off as `create_folder_nested`'s drain ordering).

## `AncestorEdit::Rename` variant

Added to the existing enum:

```rust
enum AncestorEdit {
    Remove {
        child_cid: [u8; 32],
        child_name: String,
    },
    Replace {
        old_child_cid: [u8; 32],
        new_child_cid: [u8; 32],
    },
    Append {
        entry: folders::ManifestEntry,
    },
    /// Rename an existing entry in place — find by (child_name, child_cid),
    /// change its `name` to `new_name`. Carries both the current name (the
    /// disambiguator) and the new name. CID and kind are unchanged.
    Rename {
        child_cid: [u8; 32],
        child_name: String,
        new_name: String,
    },
}
```

In `walk_and_rebuild_chain`'s deepest-edit branch:

```rust
AncestorEdit::Rename { child_cid, child_name, new_name } => {
    let target_idx = manifest.folder_manifest.entries.iter()
        .position(|e| e.name == child_name && e.cid == child_cid)
        .ok_or_else(|| format!(
            "ancestor {} has no entry named '{}' pointing to child {}",
            hex::encode(anc_cid), child_name, hex::encode(child_cid),
        ))?;
    manifest.folder_manifest.entries[target_idx].name = new_name;
}
```

Lookup semantics match `Remove`: `(name, cid)` together identifies the row, with name as the unique-within-a-folder key.

## `ContentIndex::set_file_name`

New method on `ContentIndex`, modeled on `set_archived` / `set_pinned`:

```rust
/// Update the `file_name` of an existing sidecar entry. Returns `true`
/// if the name changed, `false` if the sidecar_id is unknown OR the name
/// was already at the target value. Not CAS-protected on the name field —
/// consistent with set_archived/set_pinned/set_replication_tier (all
/// non-CID field mutations on the sidecar are non-CAS). The race hazard
/// for concurrent renames is lower-stakes than for concurrent CID rekeys:
/// worst case, the second rename wins, and the user can re-rename.
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

## Frontend: inline rename affordance

### Entry points (this slice)

- **F2 with a row selected.** Standard desktop file-manager affordance.
- **Slow-click on the name** (already-selected row → click name again, delay-gated, not a double-click). Matches Finder/Explorer.
- The drag-and-drop pathway is unaffected — the name span isn't a drag handle.

Touch/mobile rename via context-menu or modal is explicitly **out of scope for this slice** but the implementation must leave the door open: the inline-edit flow calls `service.renameContent(...)`, so a future context-menu Rename → modal flow can call the same service method without refactor.

### Edit-mode state

A single piece of `$state` in `FileBrowser.svelte`:

```typescript
let editingItem: ContentItem | null = $state(null);
let editingValue: string = $state('');
let renameError: string | null = $state(null);
```

When `editingItem` is non-null and the row's `(sidecar_id, cid)` matches, `FileRow` and `FileCard` render an `<input>` in place of the name span. `editingValue` two-way-binds to the input. `renameError` surfaces backend errors inline (no toast, no modal — same pattern as `error` for move).

### `FileRow` / `FileCard` changes

Replace the name span with a conditional render:

```svelte
{#if editing}
  <input
    class="file-row-name-input"
    type="text"
    bind:value={editValue}
    onkeydown={handleEditKey}
    onblur={handleEditBlur}
    use:autoFocus
  />
{:else}
  <span class="file-row-name" class:bold={selected}>{item.name}</span>
{/if}
```

The `editing` boolean is a `$derived` based on whether the parent's `editingItem` matches this row. `editValue` is `bind`-passed from the parent. `handleEditKey` handles Enter (commit) and Escape (cancel); `handleEditBlur` cancels (mirrors Finder's behavior when you click elsewhere).

`autoFocus` is a tiny Svelte action that calls `node.focus()` + `node.select()` on mount — used here to put the cursor in the input immediately and select the existing name for fast retype.

### Service layer

```typescript
// src/lib/file-manager-service.ts
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

Same shape and refresh pattern as `moveContent`.

### `FileBrowser` flow

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
  // Compute (srcSidecarId, srcPath, srcChildCid, srcChildName) from
  // the current navStack + the item being renamed. Same logic as the
  // drag-start payload computation in handleRowDragStart.
  const srcPath = navStack.length === 0 ? [item.cid] : navStack.map((s) => s.cid);
  const srcSidecarId =
    navStack.length === 0 ? item.sidecarId : navStack[0].sidecarId ?? '';
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

F2 keyboard handler at the FileBrowser level (gated on having a selected row and no active edit):

```typescript
function handleKeyDown(e: KeyboardEvent) {
  if (e.key === 'F2' && !editingItem) {
    const selected = items.find((i) => i.sidecarId === selectedSidecarId || i.cid === selectedCid);
    if (selected) {
      e.preventDefault();
      beginRename(selected);
    }
  }
}
```

The slow-click affordance is implemented in `FileRow` / `FileCard`: track `lastClickAt` and if the user clicks the name span twice with `300ms < gap < 800ms`, call `onBeginRename(item)`. Faster gaps are treated as double-clicks (existing folder-navigation behavior should override); slower gaps just don't trigger rename.

## Tests

### Rust integration tests (`src-tauri/tests/rename_content_integration.rs`, new file)

Mirrors the structure of `move_content_integration.rs`. Each test seeds a folder tree through the runtime's ingest channel, sets up the sidecar via `ContentIndex`, drives `rename_content_impl`, and verifies the result.

1. `rename_top_level_file` — top-level file `L` named `"hello.txt"`, rename to `"world.txt"`. Verify `ContentIndex.get(l_sid).file_name == "world.txt"` and `cid` unchanged.
2. `rename_top_level_folder` — same as above but folder. Verify the folder's children are unaffected (CID-stable).
3. `rename_nested_one_level_deep` — `T` contains `F` named `"foo"`, rename to `"bar"`. Verify `T`'s new CID matches the manifest with the renamed entry; `T`'s sidecar entry rekeyed; `F`'s underlying CID unchanged.
4. `rename_nested_two_levels_deep` — `T` contains `A` contains `L`. Rename `L`. Verify the whole chain rekeys.
5. `rename_disambiguates_siblings_with_shared_cid` — `T` contains two empty folders `EmptyA` and `EmptyB` (same CID, different names). Rename `EmptyB` to `"Renamed"`. Verify `T`'s rebuilt manifest contains `EmptyA` and `Renamed` (not two `EmptyA`s).
6. `rename_empty_name_rejected` — IPC rejects `""` and `"   "` (whitespace-only).
7. `rename_same_name_nested_no_op` — caller passes `new_name == current name`. Verify no rekey happened (sidecar CID unchanged, no new bytes ingested).
8. `rename_same_name_top_level_no_op` — same for the top-level case.
9. `rename_duplicate_sibling_rejected_nested` — `T` contains `A` and `B`. Rename `A` to `"B"`. Reject with name-collision error.
10. `rename_duplicate_sibling_rejected_top_level` — two top-level sidecar rows `A` and `B`. Rename `A` to `"B"`. Reject.
11. `rename_name_mismatch_rejected` — caller passes `src_child_name` that doesn't match the manifest. Reject with the same error shape `move_content` uses for the same mismatch.
12. `rename_concurrent_rekey_conflict` — arm the rekey-conflict hook on the cascade-root sidecar. Verify the IPC surfaces the conflict and no state changed.

### Frontend tests

- `src/lib/file-manager-service.rename-content.test.ts` — wire-shape tests mirroring `move-content.test.ts`. One example each of top-level and nested. Verify camelCase serialization, refetchRoot fires onChange on success, errors propagate.
- `src/lib/components/__tests__/file-browser-rename.test.ts` — UI integration. F2 with a selected row enters edit mode; Enter commits via `service.renameContent`; Escape cancels; blur cancels; empty-after-trim surfaces inline error; same-name skips IPC; backend rejection keeps edit mode open with the error displayed.

### Rust unit tests (in `lib.rs` `walk_and_rebuild_chain_tests` mod)

- `walk_and_rebuild_chain_rename_deepest` — single-level folder containing one entry, walk with `Rename { ... }`, verify rebuild has the new name and same CID.
- `walk_and_rebuild_chain_rename_missing_child_in_manifest` — pass a `(name, cid)` pair that doesn't exist, verify error.

## Out of scope

- **Context menu and modal dialog Rename** — deferred to a future slice once we add other context-menu actions (delete, properties) and once touch/mobile support is being designed. The IPC + service-layer methods this slice ships are reusable by that future flow without refactor.
- **Cross-folder rename (i.e., rename-and-move).** Move and rename are kept logically separate. A user who wants both should rename first, then move (or vice-versa).
- **Bulk rename / multi-select rename.** One row at a time.
- **Undo.** No history. User re-types the old name to revert.
- **Character-set escaping / illegal-name validation beyond trim+nonempty.** The manifest schema is `String`; whatever the user types is stored. If we later need OS-level safety (paths, etc.), that's a separate ticket.
- **Manual test items** — appended to ZEB-224 (manual testing checklist, long-running) on PR merge, not during the design phase.

## Sequencing

Depends on ZEB-162 (PR #133) for the `walk_and_rebuild_chain` extraction. That's merged, so this can start immediately.

Does not block ZEB-156 (root-pin-set model) — rename doesn't touch the pin cascade beyond the existing `maintain_pin_invariant` helper.

Does not block ZEB-163 (folder-as-root OS upload) — different code area.

## Design questions resolved during the 2026-05-18 conversation

1. **UX entry points** → Inline edit only for this slice (F2 + slow-click). Context-menu/modal flow deferred to a future slice when touch/mobile is being designed; reuses the same `service.renameContent` IPC.
2. **Same-name no-op** → Silent client-side skip — don't fire the IPC.
3. **IPC contract** → Mirror `move_content`'s shape for the source side.
4. **Walker extension** → New `AncestorEdit::Rename` variant; reuses `walk_and_rebuild_chain`.
5. **Top-level sidecar API** → New non-CAS `ContentIndex::set_file_name`, consistent with `set_archived`/`set_pinned`/`set_replication_tier`.
6. **Duplicate-sibling check** → Nested: scan immediate parent's manifest entries, skip self by `(name, cid)`. Top-level: scan sidecar entries, skip self by `sidecar_id`.
