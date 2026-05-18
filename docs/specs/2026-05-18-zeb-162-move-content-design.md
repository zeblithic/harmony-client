# Move content between folders (ZEB-162): design

## Goal

Let users move a file or sub-folder from one File Manager location to another. Source and destination may be (a) two positions inside the same top-level folder, (b) two different top-level folders, (c) the root and a folder, or (d) a folder and the root. The move never changes the moved child's CID; it always rewrites one or two ancestor chains and rekeys one or two top-level sidecar entries.

This is the move slice of the [ZEB-158](https://linear.app/zeblith/issue/ZEB-158) umbrella. Rename (in-place name change) was pulled out into [ZEB-299](https://linear.app/zeblith/issue/ZEB-299). Folder-as-root OS upload remains [ZEB-163](https://linear.app/zeblith/issue/ZEB-163).

## Context

Slice 1 of ZEB-158 (`docs/specs/2026-04-24-folder-primitive-design.md`, shipped in PR #55) established the folder primitive: a folder is `Bundle[manifest_book, child_1, …, child_N]` with the manifest book carrying versioned JSON `(cid, name, kind)` tuples. The sidecar (`content_index.rs`) keeps one `ContentIndexEntry` per **user-promoted root**. Nested children have no sidecar row — their existence is purely manifest-derived.

ZEB-164 (PR follow-up to ZEB-158) flipped the sidecar key from `[u8;32] CID` to opaque `SidecarId` (UUID v4), allowing multiple sidecar rows to share a CID and adding the CAS-style `ContentIndex::rekey(sidecar_id, expected_old_cid, new_cid, …)` with `RekeyError::{OldMissing, Conflict}`.

`create_folder_nested` in `src-tauri/src/lib.rs:6192` is the load-bearing reference implementation of the ancestor-rekey pattern:

1. Caller supplies `(parent_sidecar_id, parent_path: Vec<CID-hex>)` — top-level CID down to immediate parent.
2. Verify `parent_sidecar_id` still maps to `parent_path[0]`.
3. Bottom-up walk: read each ancestor's bundle + manifest from runtime cache (`read_cached_bytes`), edit the manifest (deepest gets an *append*, higher levels get a *CID replacement*), rebuild manifest + bundle, accumulate into `pending_ingests`.
4. Drain `pending_ingests` (drain-then-rekey ordering is intentional — see lib.rs:6346–6360 for why a rekey-then-ingest failure would be strictly worse).
5. CAS rekey of the top-level sidecar entry. Conflict → return error, don't silently overwrite.
6. Best-effort Unpin(old_root) / Pin(new_root) on the verb channel to maintain `pin_intent` OR-join with `is_cid_pinned_by_any`.

The move operation is the same machinery applied to **one or two** ancestor chains, with the same drain-then-rekey ordering and the same CAS posture. Two-rekey cases need a compensating-undo for the residual partial-failure window.

## Cases

Four cases distinguished by where source and destination are rooted. The IPC accepts all four through a single command shape:

| Case | Source | Destination | Top-level sidecars touched | Atomicity |
|---|---|---|---|---|
| A | nested inside `T` | nested inside `T` | one (`T`) | naturally atomic — one ancestor walk, one rekey |
| B | nested inside `T1` | nested inside `T2` (≠ `T1`) | two (`T1`, `T2`) | best-effort transactional — drain both chains, rekey `T2` then `T1`, compensating undo on `T1` failure |
| C | top-level (`T_src` itself) | nested inside `T_dst` | two (`T_src` removed, `T_dst` rekeyed) | drain dst chain, rekey `T_dst`, then delete `T_src` sidecar entry; compensating undo on sidecar-delete failure reverts `T_dst` |
| D | nested inside `T_src` | top-level (root) | two (`T_src` rekeyed, new top-level minted) | drain src chain, mint new top-level sidecar entry for the moved child, then rekey `T_src`; compensating undo on `T_src` failure deletes the freshly-minted top-level entry |

Case A is mechanically a single-chain edit: the source-side removal and destination-side append happen as two manifest edits in the SAME ancestor walk. The deepest common ancestor of `src_path` and `dst_path` is where the two edits both apply; above it, only one CID propagation happens; below it the two sides diverge into independent leaf-side walks. Conceptually a Y-shape walk. Detailed algorithm in §"Case A algorithm" below.

Cases B, C, D each touch two independent top-level sidecar rows. They share a common "drain everything, then commit in two stages, with compensating undo on the second-stage failure" structure.

## IPC surface

One new Tauri command. All four cases dispatch through it; the case is inferred from the (src_sidecar_id, dst_sidecar_id, dst_path) tuple:

```rust
#[tauri::command]
async fn move_content(
    src_sidecar_id: String,              // top-level sidecar holding the moved child today
    src_path: Vec<String>,               // top-level CID (= src_sidecar_id's current CID)
                                         //   → immediate parent CID (inclusive).
                                         //   For Case C, src_path = [src_top_level_cid] (length 1, the moved child IS the top-level).
                                         //   For Case A/B/D, src_path is the chain down to the parent of the moved child.
    src_child_cid: String,               // the moved child's CID — identifies its row in src_immediate_parent's manifest
    dst_sidecar_id: Option<String>,      // None = destination is root (Case D); Some = destination is inside an existing top-level (Cases A/B/C)
    dst_path: Vec<String>,               // top-level CID → immediate destination parent CID (inclusive). Empty when dst_sidecar_id is None (Case D).
    new_name: Option<String>,            // RESERVED, must be None in this slice (rename pulled out to ZEB-299). Reject if Some.
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<MoveContentResult, String>;

#[derive(Serialize)]
struct MoveContentResult {
    /// New CID of the source top-level after the move. None for Case C (source top-level was deleted).
    src_new_cid: Option<String>,
    /// SidecarId of the (possibly new) destination top-level entry.
    /// For Cases A, B, C: equals `dst_sidecar_id` arg.
    /// For Case D: the freshly minted sidecar_id for the moved child as a new top-level.
    dst_sidecar_id: String,
    /// New CID of the destination top-level after the move.
    /// For Cases A, B, C: the rekeyed CID.
    /// For Case D: equals `src_child_cid` (the moved child is now top-level under its own CID).
    dst_new_cid: String,
}
```

### Case dispatch

```
if dst_sidecar_id.is_none() && dst_path.is_empty():
    Case D (nested → root)
elif src_path.len() == 1 && src_sidecar_id maps to src_path[0] && src_path[0] == src_child_cid:
    Case C (root → nested)
elif src_sidecar_id == dst_sidecar_id.as_deref():
    Case A (same top-level)
else:
    Case B (across two top-levels)
```

Each case has explicit validation at the boundary; ambiguous inputs (e.g., `src_sidecar_id == dst_sidecar_id` but `src_path[0] != dst_path[0]`) get rejected with a specific error.

## Validation (boundary checks, fail fast before any mutation)

1. **`new_name` must be `None`** in this slice. Rename is ZEB-299. Reject with `"new_name is reserved for ZEB-299; pass null"`.
2. **`src_child_cid` must be valid hex.** 64 chars, lowercase. Existing `parse_cid_hex` helper.
3. **`src_path` non-empty**; first element equals the sidecar entry's current CID for `src_sidecar_id`.
4. **For Case C** (root→nested): `src_path.len() == 1` AND `src_path[0] == src_child_cid` AND `dst_sidecar_id.is_some()` AND `dst_path.first()` equals the dst sidecar entry's current CID.
5. **For Cases A/B/D**: `src_path.last()` must be a valid folder bundle in cache; reading its manifest must succeed; the manifest must contain an entry with `cid == src_child_cid`.
6. **Cycle check.** If the moved child is itself a folder, ensure it does not appear anywhere in `dst_path` (moving a folder into itself or any of its descendants). Surface as `"cannot move folder into its own descendant"`.
7. **No-op check.** Reject when `(src_sidecar_id, src_path, src_child_cid) == (dst_sidecar_id, dst_path, src_child_cid)`. Specific error: `"source and destination are identical"`.
8. **Name-collision check at destination.** Read the destination immediate parent's manifest (for Cases A/B/C) or the sidecar's top-level entries (for Case D). If any entry has the same `name` as the moved child's manifest name (resolved from the source manifest's `entries[i].name` row, or for Case C from the source's sidecar `file_name`), reject with `"destination already has an entry named '<name>'"`. The frontend is expected to surface this and prompt the user to rename first; this slice does not auto-suffix.

## Algorithms

The `create_folder_nested` walker rebuilds one chain. Move needs to rebuild one (Case A) or two (B/C/D) chains. Both shapes reuse the same per-ancestor primitive — read bundle+manifest, edit manifest entries, rebuild bundle, push to pending_ingests. We extract that as a shared helper.

### Shared helper

```rust
/// Per-ancestor manifest edit. `Remove` drops the entry whose CID matches; `Replace` flips the CID
/// of the entry whose CID matches `old_child_cid`; `Append` appends a new entry at the tail.
enum AncestorEdit {
    Remove { child_cid: [u8; 32] },
    Replace { old_child_cid: [u8; 32], new_child_cid: [u8; 32] },
    Append { entry: folders::ManifestEntry },
}

/// Walk an ancestor chain bottom-up, rebuilding each ancestor's manifest+bundle.
/// `path` is top-level → immediate parent (inclusive). `deepest_edit` applies to
/// the deepest ancestor (`path.last()`); higher ancestors get an automatic
/// `Replace { old_child_cid: anc_below_old_cid, new_child_cid: anc_below_new_cid }`.
///
/// Returns the new top-level CID and the bundle size of the new top-level (for sidecar update),
/// pushing every rebuilt (manifest_bytes, bundle_bytes) into the provided `pending_ingests`.
///
/// On any cache miss or malformed-manifest condition returns Err WITHOUT mutating
/// pending_ingests (single-chain helper guarantees no partial state).
async fn walk_and_rebuild_chain(
    verb_tx: &tokio::sync::mpsc::Sender<event_loop::ContentVerbRequest>,
    path: &[[u8; 32]],
    deepest_edit: AncestorEdit,
    pending_ingests: &mut Vec<(String, Vec<u8>)>,
) -> Result<WalkedChain, String>;

struct WalkedChain {
    new_top_level_cid: [u8; 32],
    new_top_level_size: u64,
}
```

`create_folder_nested` refactors to call this helper with `AncestorEdit::Append { … }` at the deepest level; behaviour and tests unchanged.

### Case A algorithm (Y-shape walk, single rekey)

Let `lca_idx` = the index in `src_path` of the deepest common ancestor of `src_path` and `dst_path` (computed by comparing prefix CIDs). `lca_idx` is guaranteed ≥ 0 because both paths share `src_path[0] == dst_path[0]` (same top-level). The walker is:

1. **Source-side leaf-down-to-LCA walk.** For each ancestor in `src_path[lca_idx+1..]` reversed (deepest first up to but excluding the LCA): if it's the deepest, edit = `Remove { child_cid: src_child_cid }`; else, edit = `Replace { old_child_cid: prev_src_old, new_child_cid: prev_src_new }`. Rebuild, push to `pending_ingests`. Track `prev_src_old`, `prev_src_new`.

   Edge case: source's immediate parent is the LCA. Then this loop has zero iterations and the source-side "result" is just `src_child_cid` removed from the LCA's manifest directly. Track `src_immediate_parent_cid = LCA` and treat the source-side LCA edit as "remove `src_child_cid`."

2. **Destination-side leaf-down-to-LCA walk.** Symmetric. For each ancestor in `dst_path[lca_idx+1..]` reversed: if deepest, edit = `Append { entry: src_child_manifest_entry }`; else, edit = `Replace { old_child_cid: prev_dst_old, new_child_cid: prev_dst_new }`. Rebuild, push, track `prev_dst_old`, `prev_dst_new`.

   Edge case: destination's immediate parent is the LCA. The destination-side LCA edit is "append `src_child_manifest_entry`."

3. **LCA edit** combines both sides. Read the LCA bundle + manifest. Apply: (a) remove the entry whose `cid == src_immediate_old_cid_below_lca` if source-side walk produced a `prev_src_new`, OR remove the entry whose `cid == src_child_cid` if the LCA *is* the source's immediate parent; (b) if dest-side walk produced a `prev_dst_new`, replace the entry whose `cid == dst_immediate_old_cid_below_lca` with `prev_dst_new`, OR if the LCA *is* the destination's immediate parent, append a new entry. Rebuild manifest + bundle, push to `pending_ingests`.

4. **Above-LCA walk.** For each ancestor in `src_path[..lca_idx]` reversed: `Replace { old_child_cid: prev_lca_old, new_child_cid: prev_lca_new }`. Rebuild and push.

5. After the walk completes, `prev_lca_new` (or the rebuilt LCA CID, if `lca_idx == 0`) is the new top-level CID.

6. Drain `pending_ingests`. CAS rekey of the top-level sidecar entry with `expected_old_cid = src_path[0]`. Maintain pin OR-join (unpin old top, pin new top if `is_cid_pinned_by_any`).

Case A is naturally atomic because there's exactly one rekey at the end. The drain-then-rekey ordering inherits the same correctness argument as `create_folder_nested`.

### Cases B, C, D algorithm (two-chain with compensating undo)

Cases B/C/D each rebuild two independent chains and need two sidecar mutations. The structure:

```
1. Validate everything (incl. cycle, name conflict, no-op).
2. Build BOTH chains locally into pending_ingests:
   - For Case B: walk_and_rebuild_chain on src_path with Remove(src_child_cid); walk on dst_path with Append.
   - For Case C: no src-side walk (src is the top-level); walk dst_path with Append. Read the source's manifest name+kind for the appended entry from the source sidecar entry's file_name and kind.
   - For Case D: walk src_path with Remove(src_child_cid); no dst-side walk (dst is root). Compute the moved child's manifest name from src_immediate_parent's manifest (already read during the source-side walk).
3. Drain ALL pending_ingests at once. Failure here aborts cleanly — no sidecar mutation has happened yet.
4. STAGE 1: rekey destination side.
   - Case B: CAS rekey(dst_sidecar_id, dst_path[0], dst_new_top_level_cid, dst_new_size, now).
   - Case C: CAS rekey(dst_sidecar_id, dst_path[0], dst_new_top_level_cid, dst_new_size, now).
   - Case D: mint new SidecarId; insert ContentIndexEntry { sidecar_id, cid: src_child_cid, file_name, kind, … carried-over fields … }.
   If STAGE 1 fails: return error. No sidecar mutation has happened on src side yet, so abort is clean.
5. STAGE 2: source-side sidecar mutation.
   - Case B: CAS rekey(src_sidecar_id, src_path[0], src_new_top_level_cid, src_new_size, now).
   - Case C: ContentIndex::remove(src_sidecar_id). Returns whether the entry was present; absence ≠ error in the API but here it would mean concurrent burn — surface as a specific error.
   - Case D: CAS rekey(src_sidecar_id, src_path[0], src_new_top_level_cid, src_new_size, now).
   If STAGE 2 fails: enter COMPENSATING UNDO.
6. COMPENSATING UNDO:
   - Case B: CAS rekey(dst_sidecar_id, dst_new_top_level_cid, dst_path[0], dst_old_size, dst_old_stored_at_ms). If this CAS fails too (a third party rekeyed dst between our STAGE 1 commit and our compensating undo — rare but possible), fall through to step 7's "both folders show the child" state and return a specific error that tells the user.
   - Case C: ContentIndex::remove(dst_sidecar_id) effectively un-does the append? No — the append actually changed dst's top-level CID, so undo is the same shape as Case B: CAS rekey dst back to its old CID. Same fall-through.
   - Case D: ContentIndex::remove(new_top_level_sidecar_id). This is a simple sidecar mutation; failure would require concurrent insertion of an identical SidecarId (impossible — UUID v4) or sidecar I/O failure. If undo fails, fall through to step 7.
7. FALLTHROUGH (both undo attempts failed): persist sidecar in current state. Move bytes orphaned by the failed stage will be evicted under cache pressure. Return error message that names both side states so the user can manually reconcile.
8. Maintain pin OR-join on every rekeyed/removed/added top-level CID. Best-effort with tracing::warn on failure (matches create_folder_nested).
```

The undo failure window is narrow: between two sequential `idx.lock()` calls with no .await in between, so a third-party rekey would need to land in single-digit microseconds. We accept it as documented residual risk; the worst observable user state is "child appears in both folders" which is content-addressed-honest and self-recoverable.

## Sidecar interactions

- `ContentIndex::rekey` (existing) handles all CAS-style rekeys.
- `ContentIndex::insert` (existing) handles Case D's new top-level minting.
- `ContentIndex::remove` (existing) handles Case C's source delete and the Case D compensating undo.
- No new sidecar API needed.

For Case D, the new top-level entry carries forward what's available from the moved manifest entry:
- `cid`: `src_child_cid` (unchanged).
- `file_name`: the moved child's manifest `name`.
- `kind`: the moved child's manifest `kind`.
- `size_bytes`: bundle size of the moved child if known (will need a `read_cached_bytes` of the moved child's bundle to measure; if cache-miss, default to 0 and let the next refresh repopulate — non-blocking).
- `stored_at_ms`: now.
- `sensitivity: Sensitivity::Private`, `replication_tier: ReplicationTier::Default`, `licensed: false`, `archived: false`, `pinned: false`: defaults. The slice does not inherit pin/archive state from the source ancestor because pin/archive in the current model are root-level only and the moved child wasn't independently pinned before.

For Case C, removing the source's sidecar entry: pin OR-join must be maintained AFTER the remove — if the source root was the only entry pinning some descendant CIDs (now under the destination root), an Unpin dispatch may be redundant or premature. We just call `is_cid_pinned_by_any(&src_path[0])` after the remove — if false, dispatch Unpin; otherwise leave alone. Same pattern as the existing create_folder_nested code.

## Pin / archive / burn interactions

- The moved child's CID never changes. No re-cascade triggered by the move itself.
- Source top-level's CID changes (or the source top-level is removed in Case C). Existing Unpin(old_src_root) + (conditional) Pin(new_src_root) dispatch handles this.
- Destination top-level's CID changes (or a new top-level is minted in Case D). Same pattern: Pin(new_dst_root) only if the dst sidecar entry's pin intent is true.
- **Shared-leaf hazard inherited from ZEB-156.** A leaf appearing in both source and destination subtrees: the source-side Unpin cascade walks via `collect_descendants` and unpins everything below the old source root, INCLUDING any leaf that's now also under the destination root. If destination root is currently pinned, the next fetch-completion hook (ZEB-155 + ZEB-159 gating) will repin. If destination root is unpinned, the leaf loses its pin. This is the same hazard ZEB-156 owns; ZEB-162 does not fix it.
- **Burn** of a top-level whose subtree contained a moved child does the right thing: the moved child's CID, now under a different top-level, is still referenced through that other top-level's bundle, and `collect_descendants` from the burned root cannot reach it.

## Frontend wiring

The UI surface adds drag-drop on file/folder rows. Context menu and keyboard cut/paste are deferred.

### Service-layer additions

`src/lib/file-manager-service.ts` gains:

```ts
async moveContent(args: {
    srcSidecarId: string;
    srcPath: string[];            // top-level CID hex → immediate parent CID hex (inclusive)
    srcChildCid: string;
    dstSidecarId: string | null;
    dstPath: string[];
    newName?: null;               // must remain null in this slice
}): Promise<{ srcNewCid: string | null; dstSidecarId: string; dstNewCid: string }>
```

Service refreshes the visible-list state after a successful move by re-listing the affected top-level(s), same pattern the create_folder path already uses.

### Drag-drop UX

`FileBrowser.svelte` owns the drag-drop coordination. Per-row drag handlers live in `FileList.svelte` and `FileGrid.svelte`; the drop targets are:

1. **Other folder rows in the current folder view.** Drop = "move into that folder."
2. **Breadcrumb segments.** Drop = "move out into the ancestor at that level." (For a top-down nav stack `[root, A, B]` with the user inside B, dragging onto "A" moves the dropped item from B into A; onto the root sentinel moves out to top-level.)
3. **Empty area of the current folder list.** No-op (or visual feedback only). Avoid "drop into self" ambiguity.

Drag payload: a JSON blob with `{ srcSidecarId, srcPath, srcChildCid, srcChildName }`. Stored in `dataTransfer.setData('application/x-harmony-content', JSON.stringify(payload))` with `dataTransfer.effectAllowed = 'move'`. We deliberately use a custom MIME type instead of `text/plain` so OS-level text drops (URLs, snippets) don't accidentally fire a move.

Drop handlers compute `dstSidecarId`, `dstPath`, and invoke `service.moveContent`. On success, the service-level refresh repaints the list. On error (cycle, name conflict, CAS conflict, partial-failure undo), surface as an inline toast/banner — same pattern ZEB-166 will eventually formalize. For this slice we can use a simple `error` $state with an inline `<div class="error">…</div>` to avoid blocking on ZEB-166.

### Component touchpoints

- `FileBrowser.svelte` — error state, drag/drop event wiring, refresh after move.
- `FileList.svelte` and `FileGrid.svelte` — per-row `draggable=true`, `dragstart`, `dragend`. Drop targets on rows whose `kind === 'folder'`.
- `Breadcrumbs.svelte` — per-segment drop target.

### Wire-shape changes

No `ContentItem` shape change. `file-manager-service.ts`'s internal types gain `moveContent` and that's it. The Tauri command is added to the `invokeHandler` list in `src-tauri/src/lib.rs`.

## Testing

### Rust unit tests (`folders.rs` or `lib.rs`-private)

1. `walk_and_rebuild_chain_remove_deepest` — synthetic 2-level chain, deepest edit Remove, verify rebuilt chain.
2. `walk_and_rebuild_chain_replace_deepest` — synthetic chain, edit Replace, verify.
3. `walk_and_rebuild_chain_append_deepest` — Append at deepest; verify the existing `create_folder_nested` refactor still works against this helper (covered by existing create_folder_nested integration tests, but worth one new direct unit test).
4. `walk_and_rebuild_chain_cache_miss_returns_err_without_mutating_pending` — verify the helper does not push to pending_ingests on cache-miss failure.
5. `walk_and_rebuild_chain_missing_child_in_manifest` — non-deepest ancestor doesn't contain the CID we expected to replace; helper returns the existing-shape error message.

### Rust integration tests (`src-tauri/tests/folder_primitive_integration.rs` or a new `move_content_integration.rs`)

6. `move_a_within_same_top_level_one_level_deep` — top-level `T` with folder `A` and folder `B`; folder `A` contains leaf `L`. Move `L` into `B`. Verify `T`'s new manifest shows `B` containing `L`; `A` is empty; sidecar's `T` entry is rekeyed.
7. `move_b_across_top_levels` — top-level `T1` containing leaf `L`, top-level `T2` empty. Move `L` from `T1` into `T2`. Verify both top-level sidecar entries are rekeyed.
8. `move_c_root_to_nested` — top-level leaf `L`, top-level folder `F` empty. Move `L` into `F`. Verify `L`'s sidecar entry is gone, `F`'s sidecar entry is rekeyed to a manifest listing `L`.
9. `move_d_nested_to_root` — top-level folder `F` containing leaf `L`. Move `L` out to root. Verify a new top-level sidecar entry exists for `L` with `kind: Leaf`, `F`'s sidecar entry is rekeyed to empty manifest.
10. `move_b_dst_rekey_conflict_compensating_undo_reverts` — simulate a concurrent rekey on dst between our drain and our STAGE 1 commit (test harness pre-rekeys dst), verify STAGE 1 fails, no src mutation happens, error is returned. (This case skips the compensating undo path because STAGE 1 failed; the undo path is exercised by 11.)
11. `move_b_src_rekey_conflict_after_dst_commit_undo_reverts_dst` — pre-rekey src between STAGE 1 and STAGE 2 (use a barrier inside the test harness). Verify dst was rekeyed forward then rekeyed back (compensating undo succeeded); src is untouched; error names "concurrent rekey on src" and the visible state matches "nothing moved."
12. `move_cycle_rejected` — top-level folder `T` containing folder `F`. Attempt to move `T` into `F`. Verify rejection at the boundary.
13. `move_no_op_rejected` — call move with src == dst (same sidecar_id, same path, same child). Verify rejection.
14. `move_name_collision_rejected` — destination folder already has an entry named "foo.txt"; try to move a different leaf also named "foo.txt" in. Verify rejection BEFORE any ingest.
15. `move_pin_cascade_a_within_same_root` — top-level `T` pinned, contains folder `A` (with leaf `L`) and folder `B`. Move `L` from `A` to `B`. Verify `L` remains in the runtime pinned set (because the rebuilt `T` still contains `L` via its new child path through `B`, and pin re-cascade picks it up).
16. `move_d_new_top_level_pin_defaults_unpinned` — move `L` from inside pinned `T` out to root. Verify the new top-level sidecar entry for `L` has `pinned: false` (this slice does not inherit pin state across moves; ZEB-156 owns the proper semantics).

### Frontend tests (`src/lib/__tests__/` and `src/lib/components/__tests__/`)

17. `file-manager-service.move-content.test.ts` — given a mocked `invoke`, verify `moveContent` issues the right wire shape and refresh-on-success behaviour. Cover Case A, B, C, D dispatch shapes (each case has slightly different arg combinations).
18. `file-browser-drag-drop.test.ts` — render `FileBrowser` with a fixture service, simulate dragstart on a file row, simulate drop on a folder row, verify `service.moveContent` was called with the right args. Cover the four drop-target classes (folder row, breadcrumb, empty area no-op).

### Manual-test items (append to ZEB-224 checklist)

19. Drag a leaf from a nested folder into a sibling folder; verify it appears in the destination and disappears from the source after the next refresh.
20. Drag a leaf out to root via the breadcrumb root sentinel.
21. Drag a folder onto its own descendant — verify the cycle-rejection inline error surfaces.
22. With a pinned top-level and a leaf inside it, move the leaf to a different unpinned top-level; verify the leaf stays accessible after restart (current sliced behaviour — formally fixed under ZEB-156).
23. Two clients with the same identity (after ZEB-215 multi-device sync lands) — move from one, verify the other reflects the move within sync latency. (May be infeasible until multi-device is more mature; mark as "future" in the checklist.)

## Out of scope

- **Rename** (ZEB-299).
- **Multi-select move.** UX layer; can ship later without backend changes.
- **Move between File Manager and NavService spaces/channels/DMs.** Different model, different commands.
- **Context menu and keyboard-driven move.** UX layer; deferred.
- **Undo affordance.** Content-addressing makes "move it back" trivially possible; no dedicated undo.
- **Auto-suffix on name conflict.** Reject with a clear error; user renames source first.
- **Cross-device move conflict resolution.** Inherits whatever conflict semantics multi-device sync (post-ZEB-215) eventually adopts.
- **Per-nested-item pin state inheritance.** ZEB-156 owns root-pin-set; this slice keeps the slice-1 cascade semantics unchanged.

## References

- [ZEB-158](https://linear.app/zeblith/issue/ZEB-158) — folder primitive umbrella.
- [ZEB-162](https://linear.app/zeblith/issue/ZEB-162) — this ticket.
- [ZEB-164](https://linear.app/zeblith/issue/ZEB-164) — sidecar by SidecarId (the rekey CAS lives here).
- [ZEB-156](https://linear.app/zeblith/issue/ZEB-156) — root-pin-set cascade (owns the shared-leaf fix this slice inherits).
- [ZEB-159](https://linear.app/zeblith/issue/ZEB-159) — fetch admission (gates the repin-after-eviction case).
- [ZEB-299](https://linear.app/zeblith/issue/ZEB-299) — rename (pulled out of this ticket on 2026-05-18).
- `docs/specs/2026-04-24-folder-primitive-design.md` — folder primitive design.
- `docs/specs/2026-04-24-sidecar-id-refactor-design.md` — SidecarId refactor.
- `src-tauri/src/lib.rs:6192` — `create_folder_nested` (the reference implementation of the ancestor-rekey pattern).
