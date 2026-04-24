# Folder primitive (slice 1 of ZEB-158): design

## Goal

Add first-class folders to the File Manager: a user can create folders at the root or inside other folders, navigate into them, and see their contents. Pin/archive/burn on a folder cascades to its contents through existing ZEB-155/ZEB-154 machinery. No changes to `harmony-content`; the folder primitive is a client-only concept built on top of the existing bundle + book formats.

This is **slice 1** of the [ZEB-158](https://linear.app/zeblith/issue/ZEB-158) umbrella. Move-between-folders is [ZEB-162](https://linear.app/zeblith/issue/ZEB-162), folder-as-root OS upload is [ZEB-163](https://linear.app/zeblith/issue/ZEB-163), nested-bundle ingest for oversized files is [ZEB-161](https://linear.app/zeblith/issue/ZEB-161), shared-leaf cascade is [ZEB-156](https://linear.app/zeblith/issue/ZEB-156).

## Context

ZEB-146 wired File Manager backend commands to a flat listing of root CIDs; ZEB-154 added transparent chunked-ingest of large files (flat bundles, `MAX_BUNDLE_ENTRIES × min_chunk` ≈ 8 GiB cap); ZEB-155 added persisted pin intent on sidecar entries. The File Manager UI (`src/lib/components/FileBrowser.svelte`) was written anticipating folder navigation: `currentFolderCid` state, `parentCid` on `ContentItem`, `isFolder` sort, breadcrumb component. The backend-side scaffolding is the missing piece.

All folder entries in the current UI are mock data. This spec makes folders real end-to-end for the local case.

## Design decisions

Seven open design questions from ZEB-158; slice 1 answers Q1, Q2, Q4, Q6, Q7. Q3 and Q5 are deferred to ZEB-162 / ZEB-163.

1. **Directory manifest format (Q1)** → **A-alt: folder is a Bundle whose child-0 is a manifest Book.**
   A folder's CID is a `Bundle` (depth ≥ 1) with child-0 = a Book carrying a JSON manifest and child-1..N = the folder's child CIDs (files and sub-folder bundles). The manifest enumerates `(cid, name, kind)` for each child, and the bundle's child-1..N list mirrors the manifest's `cid` list in order.

   Why A-alt over plain A (folder = manifest Book only): `runtime.pin_content`'s cascade uses `collect_descendants`, which walks bundle children but does not traverse book payloads. Under plain A, pinning a folder would only pin the manifest book's bytes — not any files inside it — silently breaking the "pin folder" affordance. Under A-alt, `collect_descendants` walks the bundle naturally, pinning manifest + every descendant (including sub-folder manifests, recursively).

   Why A-alt over extended bundle format (Option B): `ContentId` has zero free bits (4 mode + 6 depth + 20 size + 2 checksum, all allocated). Adding a first-class `Directory` CidType would force a cross-repo bit-layout redesign. A-alt uses only existing types (book, bundle) and keeps `harmony-content` untouched.

2. **Sidecar model (Q2)** → **Only user-promoted roots get a `ContentIndexEntry`.**
   Top-level ingested files and top-level created folders get a full sidecar row. Items nested inside a folder have no sidecar row — they are discovered by parsing the folder's manifest book on demand. One new field on `ContentIndexEntry`: `kind: ContentKind` (`Leaf` | `Folder`) with `#[serde(default)]` for backward compat with v1 sidecars.

   Why not "every item gets a row" (Option X): the manifest already carries authoritative `(cid, name, kind)` and is part of the content-addressed CID. A parallel sidecar row for nested items would be redundant and would get out of sync during mutations (renaming a nested file changes the parent folder's CID atomically; a sidecar-side `file_name` field for a nested file can't participate in that atomicity). Keeping nested state purely manifest-side keeps the content-addressed and local-state layers cleanly separated.

   Why not "every item gets a row, just lightweight for nested items" (Option Z hybrid): the only motivation for nested sidecar rows would be individually-pinned or individually-archived nested files. Slice 1 does not support that — pin/archive operate at folder-root granularity. ZEB-156's root-pin-set model will define the correct semantics; Option Z can be retrofitted then if needed.

3. **Move semantics (Q3)** → deferred to [ZEB-162](https://linear.app/zeblith/issue/ZEB-162).

4. **Empty folders (Q4)** → **Representable.** An empty folder is `Bundle[manifest_book]` where the manifest's `entries` array is `[]`. The bundle satisfies the ≥ 1 child constraint (the manifest itself is child-0); the manifest says "no children."

5. **Folder-as-root upload (Q5)** → deferred to [ZEB-163](https://linear.app/zeblith/issue/ZEB-163).

6. **File-name authority (Q6)** → **Manifest authoritative for nested items; sidecar `file_name` authoritative for roots.**
   A root has no parent manifest, so its `file_name` lives in the sidecar. A nested item's display name is read from its parent folder's manifest. Renaming a nested item changes the parent's manifest bytes, which changes the parent's bundle CID — an atomic content-addressed rewrite of the parent. There is no separate sidecar field to keep in sync.

7. **Interaction with pin persistence (Q7)** → **Composes cleanly with ZEB-155; no new mechanism needed.** See the Pin/archive/burn semantics section below.

## Architecture

A folder is a `Bundle` of depth ≥ 1. Its first child is a `Book` carrying a JSON manifest; the remaining children are the folder's contents in manifest order. The client constructs the manifest + bundle at folder-creation time and parses the manifest at folder-navigation time. `harmony-content` sees folders as ordinary bundles.

```text
                  Folder CID (= bundle CID, depth ≥ 1)
                          │
              ┌───────────┼───────────┬───────────┐
              │           │           │           │
          child-0      child-1     child-2     child-N
        (manifest)    (leaf or    (sub-folder  ...
                      sub-folder   bundle CID)
                      bundle CID)
              │
              ▼
       ┌─────────────────────────────────────┐
       │  manifest Book payload (UTF-8 JSON):│
       │                                     │
       │  {                                  │
       │    "folder_manifest": {             │
       │      "version": 1,                  │
       │      "entries": [                   │
       │        {                            │
       │          "cid": "hex64",            │
       │          "name": "foo.txt",         │
       │          "kind": "leaf"             │
       │        },                           │
       │        {                            │
       │          "cid": "hex64",            │
       │          "name": "photos",          │
       │          "kind": "folder"           │
       │        }                            │
       │      ]                              │
       │    }                                │
       │  }                                  │
       └─────────────────────────────────────┘
```

The outer `"folder_manifest"` key is a self-identifier — a reader with only the bundle bytes can disambiguate a folder from a chunked-ingest-without-metadata bundle by attempting to decode child-0's payload as this shape. Valid folder manifest → folder. Anything else → not a folder.

The sidecar stores one `ContentIndexEntry` per user-promoted root: ingested files (`kind: Leaf`) and top-level created folders (`kind: Folder`). Nested items have no sidecar row.

## Manifest schema

```rust
// src-tauri/src/folders.rs

#[derive(Serialize, Deserialize)]
pub struct FolderManifest {
    pub folder_manifest: ManifestBody,
}

#[derive(Serialize, Deserialize)]
pub struct ManifestBody {
    pub version: u32,
    pub entries: Vec<ManifestEntry>,
}

#[derive(Serialize, Deserialize)]
pub struct ManifestEntry {
    #[serde(with = "hex")]
    pub cid: [u8; 32],
    pub name: String,
    pub kind: ContentKind,
}
```

Schema version starts at 1. Future additions (thumbnails, mtime, sensitivity override, etc.) are additive via `#[serde(default)]` on new fields; version bumps only for breaking schema changes.

The manifest's `entries[i].cid` must equal the bundle's `child[i+1]` CID for all `i`. Validation at parse time: if the two lists diverge (wrong length, wrong CIDs), the manifest is malformed — slice 1 returns an error on the list call for that folder. Building is always-consistent because the bundle is constructed from the manifest.

## Sidecar changes

One additive change to `ContentIndexEntry`:

```rust
// src-tauri/src/content_index.rs

#[derive(Serialize, Deserialize, Default)]
pub enum ContentKind {
    #[default]
    Leaf,
    Folder,
}

pub struct ContentIndexEntry {
    pub cid: [u8; 32],
    pub file_name: String,
    pub size_bytes: u64,
    pub stored_at_ms: u64,
    pub sensitivity: Sensitivity,
    pub replication_tier: ReplicationTier,
    pub licensed: bool,
    pub archived: bool,
    pub pinned: bool,
    #[serde(default)]
    pub kind: ContentKind,  // NEW in ZEB-158 slice 1
}
```

`#[serde(default)]` on `kind` matches the backward-compat pattern from ZEB-155's `pinned` field. Legacy entries (from before slice 1) deserialize as `kind: Leaf` — correct, because folders didn't exist when they were written. No sidecar version bump.

## Tauri commands

### New command: `create_folder`

```rust
#[tauri::command]
async fn create_folder(
    name: String,
    parent_path: Vec<String>,   // empty = create at root; else CIDs from top-level root down to immediate parent
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<String, String>    // returns the new folder's CID (hex) if at root, or the new root CID if nested
```

**Root creation (`parent_path: []`):**

1. Build empty `FolderManifest` (entries: []). Serialize to JSON bytes.
2. Compute manifest book CID via `ContentId::for_book(manifest_bytes)`. Ingest through the existing ingest channel.
3. Build bundle `[manifest_cid]` via `BundleBuilder::new().add(manifest_cid).build()`. Ingest bundle bytes.
4. Insert `ContentIndexEntry` with `cid: bundle_cid`, `file_name: name`, `size_bytes: bundle_bytes.len()`, `kind: Folder`, other fields default.
5. Sidecar atomic save (existing path).
6. Return `bundle_cid` hex.

**Nested creation (`parent_path: [root, ..., immediate_parent]`):**

1. Same as root-creation steps 1–3: produce the new empty sub-folder's bundle CID `new_child_cid`.
2. Walk `parent_path` from the deepest element (`parent_path.last()` = the immediate parent) up to the root (`parent_path.first()`). Track two variables across the walk: `prev_old_cid` (the ancestor's current CID on disk, as recorded in `parent_path` before the mutation) and `prev_new_cid` (the CID that replaces it).
   - Initialize: `prev_old_cid = immediate_parent_cid`, `prev_new_cid = new_child_cid` (conceptually, the "mutation" at this layer is *appending* the new sub-folder, not replacing an existing entry).
   - Iteration over ancestors (deepest first):
     - **Deepest ancestor (immediate parent only):** fetch `prev_old_cid`'s bundle, parse manifest, **append** `ManifestEntry { cid: new_child_cid, name, kind: Folder }`. Rebuild manifest and bundle → `anc_new_cid`. Ingest both. Set `prev_old_cid = immediate_parent_cid`, `prev_new_cid = anc_new_cid`, advance to the next ancestor up.
     - **Higher ancestors:** fetch the ancestor's bundle, parse manifest, find the entry whose `cid` equals `prev_old_cid`, **replace** it with a new entry carrying the same `name` / `kind` but `cid: prev_new_cid`. Rebuild manifest and bundle → `anc_new_cid`. Ingest both. Set `prev_old_cid` to the ancestor's own old CID, `prev_new_cid = anc_new_cid`, advance.
3. After the walk completes, `prev_new_cid` is the new top-level root CID. Update the sidecar: remove the entry keyed by the old top-level root CID (= `parent_path.first()`), insert a new entry keyed by `prev_new_cid`, carrying forward `file_name`, `pinned`, `archived`, `sensitivity`, `replication_tier`, `licensed`. Update `size_bytes` (new bundle's byte length) and `stored_at_ms` (now).
4. Event-loop side: dispatch `Unpin(old_root_cid)` through the verb channel, and — if the old entry had `pinned: true` — also dispatch `Pin(prev_new_cid)`. ZEB-155's existing Pin/Unpin arms update `pin_intent` atomically (remove old, insert new).
5. Sidecar atomic save.
6. Return `prev_new_cid` hex.

Bounded by `MAX_BUNDLE_DEPTH = 62` (creates more than 62 levels deep will fail at `BundleBuilder::build()` with the existing depth-check).

### Updated command: `list_content`

```rust
#[tauri::command]
async fn list_content(
    folder_cid: Option<String>,   // None = top-level (root) listing; Some(cid) = contents of that folder
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<ContentItemWire>, String>
```

**`folder_cid: None` (root listing):**

Unchanged from ZEB-155's implementation, plus the new `kind` field in the wire.

**`folder_cid: Some(cid)` (folder listing):**

1. Parse `cid` hex → `[u8; 32]`.
2. Fetch the bundle bytes from `runtime.storage_tier().cache()`. If not present, return an empty vec with a `"folder not in cache"` diagnostic. (In slice 1 this only happens if the user navigates into a folder whose bundle has been evicted — acceptable; ZEB-159 will add transparent re-fetch when wiring `fetch_rx` into the store.)
3. Parse the bundle as `concat(CID₀, CID₁, …, CIDₙ)`. Take `CID₀` as the manifest book CID.
4. Fetch the manifest book bytes. Parse as `FolderManifest`.
5. Validate: `manifest.entries[i].cid == bundle.child[i+1]` for all `i`. On mismatch, return `"malformed folder manifest"` error.
6. For each manifest entry, synthesize a `ContentItemWire` with `cid`, `name`, `kind`. For pin/archive state, join against the runtime's `pinned_set`: `pinned: runtime_pinned.contains(&entry.cid)`. `archived` is always `false` for nested items in slice 1 (the sidecar-archived flag only exists on roots).
7. Other fields (`size_bytes`, `stored_at_ms`, `sensitivity`, `replication_tier`, `licensed`) are not available for nested items in slice 1 — reported as `0` / defaults. A follow-up can pull per-item size by fetching each child CID and measuring, but this is not a slice-1 concern.

### Existing commands that need no change

- `pin_content(cid)`: works as-is on folder root CIDs. `set_pinned` flips the sidecar flag; `runtime.pin_content` cascades through the bundle walker. **This is A-alt's payoff.**
- `unpin_content(cid)`: same.
- `burn_content(cid)`: same. Effectively "delete folder" when called on a folder root CID.
- `archive_content(cid)`: sidecar-only flag, unchanged.

## Pin / archive / burn semantics

**Pin a top-level folder.** Sidecar entry's `pinned` flag flips to `true` (ZEB-155 path unchanged). Event-loop `pin_intent.insert(folder_cid)`. Runtime `pin_content(folder_cid)` → `collect_descendants` walks the bundle, pinning child-0 (manifest book) and recursing into children (leaves get pinned; sub-folder bundles get walked, their manifest books pinned, their children pinned, and so on).

**Restart with a pinned folder.** Sidecar reload seeds `pin_intent` with the folder's root CID (ZEB-155 startup path). Display OR-join shows `pinned: true` on the folder row. If the folder's bundle bytes are still in cache, listing and cascade continue to work within the session. If the cache was cold and the user later fetches the folder, ZEB-155's fetch-completion hook triggers a re-pin cascade across the refetched tree.

**Known gap (inherited from ZEB-155).** The fetch-completion hook's practical reach is gated on [ZEB-159](https://linear.app/zeblith/issue/ZEB-159) admitting fetched bytes into `ContentStore`. Today, `fetch_rx` returns bytes to the Tauri caller without admission; `collect_descendants` then walks an empty cache for the folder CID and the repin is a no-op. Slice 1 inherits this gap unchanged and documents it in the same place ZEB-155 already does.

**Pin a nested item (inside a folder).** Under Option Y there is no sidecar entry for the nested CID. `pin_content(nested_cid)` calls `set_pinned`, which returns `false` (no entry to flip) — consistent with ZEB-155's existing contract for CIDs without sidecar entries. The runtime-side pin still dispatches and works within-session; on restart, the pin is lost because nothing was persisted. [ZEB-156](https://linear.app/zeblith/issue/ZEB-156)'s root-pin-set model will define the correct persisted semantics for nested pins.

**Shared leaves across folders.** A leaf referenced in two folders (via CDC dedup or explicit future reference) under slice 1's cascade-via-`collect_descendants` model has the same hazard ZEB-154 accepted: unpinning one parent unpins the shared leaf even when another parent is still pinned. Slice 1 does not fix this. ZEB-156 owns it.

**Burn (delete) a top-level folder.** Existing `burn_content` command. Runtime cascade walks the bundle tree and evicts every descendant; sidecar entry is removed. No new command or semantic. Nested-folder deletion (deleting a sub-folder from inside its parent) is not in slice 1 — it requires the same ancestor-cascade re-ingest as `create_folder`'s nested path. Deferred to ZEB-162 (which needs the same machinery).

## Frontend wiring

The UI shell already has `currentFolderCid`, breadcrumbs, and folder-first sort; slice 1 only has to make calls to the new backend surface.

**`src/lib/types.ts`** — replace `isFolder: boolean` with `kind: "leaf" | "folder"` on `ContentItem`, matching the new wire shape.

**`src/lib/services/content.ts`** (or wherever the Tauri `invoke` wrappers live) — update `listContents` to pass `folder_cid` (hex string or null) and add `createFolder(name, parentPath)`.

**`src/lib/components/FileBrowser.svelte`** — stop reading mock data; call the backend. Pass `parent_path` to `createFolder` computed from the current breadcrumb stack. On folder double-click, push the folder's CID onto the breadcrumb stack and re-call `listContents(folder_cid=newTop)`.

**`src/lib/components/Breadcrumbs.svelte`** — already accepts `{ cid, name }` segments; no change.

**Mock data removal** — delete `src/lib/mock-file-data.ts` or gate it behind a dev-only import.

## Testing

### Unit tests (`folders.rs`)

1. `empty_manifest_round_trip` — serialize an empty `FolderManifest`, parse it back, verify `entries: []`.
2. `manifest_with_mixed_entries_round_trip` — three entries (leaf + folder + leaf), serialize, parse, verify order preserved and kinds correct.
3. `build_empty_folder` — call the folder-build helper with no children, verify it produces (manifest bytes + manifest_cid, bundle bytes + bundle_cid) and that the bundle bytes are exactly the 32 bytes of `manifest_cid`.
4. `build_folder_with_two_children` — supply two child CIDs, verify the manifest enumerates them in order and the bundle bytes are `concat(manifest_cid, child_0, child_1)` (96 bytes).

### Unit tests (`content_index.rs`)

5. `kind_defaults_to_leaf_on_legacy_sidecar` — craft a raw JSON entry without the `kind` field, verify it deserializes with `kind: ContentKind::Leaf`.
6. `save_persists_kind_field` — insert a folder entry, round-trip through save/load, verify `kind: Folder` survives.

### Unit tests (`lib.rs`)

7. `list_content_root_includes_kind_in_wire` — a sidecar with one leaf and one folder; `list_content(None)` returns both with the correct `kind` wire value.
8. `list_content_folder_cid_not_in_cache_returns_empty_with_diagnostic` — pass a random CID not in the cache; verify empty `Vec` returned with no panic.

### Integration tests (`src-tauri/tests/folder_primitive_integration.rs` — new file)

9. `create_folder_at_root_then_list_shows_it` — boot a node, call `create_folder("Photos", [])`, call `list_content(None)`, verify one row with `file_name: "Photos"`, `kind: Folder`.
10. `create_nested_folder_updates_top_level_root_cid` — create "Photos" at root (get root_v1 CID), create "2026" inside it (parent_path = [root_v1]), verify the sidecar now has a Photos entry keyed by a **different** CID (root_v2), verify `list_content(Some(root_v2))` returns one entry for "2026".
11. `pin_folder_cascades_to_nested_leaf` — create folder with a leaf child (via low-level ingest + create), pin the folder, assert via the test harness that the leaf's CID is in the runtime's `pinned_set`.
12. `empty_folder_listing_returns_empty_vec` — create empty folder, call `list_content(Some(folder_cid))`, verify `Ok(vec![])`.
13. `pin_intent_survives_restart_for_folder` — extends ZEB-155's `pin_intent_survives_reload` to a folder root: pin the folder, drop/reload sidecar, verify the reloaded entry has `pinned: true` and `kind: Folder`.
14. `malformed_manifest_returns_error` — inject a bundle whose child-0 is not valid manifest JSON (e.g., a chunked-file sentinel CID); call `list_content(Some(bundle_cid))`, verify it returns an error and doesn't panic.

Integration tests use `pub` items only, consistent with ZEB-154's integration-test pattern.

## Out of scope

- **Nested-folder deletion or rename.** Requires the same ancestor-cascade re-ingest as `create_folder`'s nested path plus orphan-child handling. Tracked separately as part of [ZEB-162](https://linear.app/zeblith/issue/ZEB-162) (move shares this machinery).
- **Moving items between folders.** [ZEB-162](https://linear.app/zeblith/issue/ZEB-162).
- **Dragging an OS folder in.** [ZEB-163](https://linear.app/zeblith/issue/ZEB-163).
- **Per-nested-item pin / archive state.** Needs sidecar rows for nested items or a separate state store; deferred with ZEB-156's root-pin-set model.
- **Nested-bundle ingest for files > 8 GiB.** Orthogonal axis; [ZEB-161](https://linear.app/zeblith/issue/ZEB-161).
- **Correct cascade for shared leaves across folders.** [ZEB-156](https://linear.app/zeblith/issue/ZEB-156).
- **Transparent re-fetch when navigating into an evicted folder.** Requires [ZEB-159](https://linear.app/zeblith/issue/ZEB-159) (fetch admission into ContentStore); slice 1 returns empty + diagnostic instead.
- **Per-nested-item size / mtime / sensitivity in the listing wire.** Requires fetching each child's bytes or reading extended manifest metadata; a cheap follow-up that doesn't affect the primitive's shape.
- **Sidecar migration for users who had mock folders.** Mock folders never landed in anyone's sidecar — they were frontend-only. No migration needed.

## References

- [ZEB-158](https://linear.app/zeblith/issue/ZEB-158) — umbrella ticket and sequencing decision.
- [ZEB-146](https://linear.app/zeblith/issue/ZEB-146) — File Manager backend wiring (prerequisite).
- [ZEB-154](https://linear.app/zeblith/issue/ZEB-154) — flat-bundle chunked ingest (the bundle walker A-alt reuses).
- [ZEB-155](https://linear.app/zeblith/issue/ZEB-155) — persisted pin intent (the sidecar shape and fetch-completion hook this composes with).
- [ZEB-156](https://linear.app/zeblith/issue/ZEB-156) — root-pin-set cascade (owns the shared-leaf fix slice 1 defers).
- [ZEB-159](https://linear.app/zeblith/issue/ZEB-159) — fetch admission into ContentStore (owns the cache-warmup gap slice 1 documents).
