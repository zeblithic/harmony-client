# Sidecar `SidecarId` Refactor — Design

**Ticket:** [ZEB-164](https://linear.app/zeblith/issue/ZEB-164) (parent: ZEB-158)

## Goal

Re-key the client sidecar (`content-index.json`) by an opaque per-entry
`SidecarId` instead of by content CID, so multiple sidecar entries can
share a CID. This delivers the symlink-style mental model promised by
ZEB-158 slice 1 — distinct user-visible names ("Photos", "Documents")
can both reference the same content-addressed bytes (e.g., the empty
folder bundle).

## Architecture

`ContentIndex` is keyed by `SidecarId` (UUID v4). CID becomes a regular
field on `ContentIndexEntry`, no longer the primary key. The
content-addressed storage layer is unchanged: bytes still address by CID.
The sidecar becomes a "user-facing entry directory" on top of CAS,
analogous to an inode-table-vs-dentry split: CIDs are inodes, sidecar
entries are dentries.

Internal storage uses a single `HashMap<SidecarId, ContentIndexEntry>`.
CID-derived queries ("any entry pinned for X?", "any entry referencing
X?") scan the map. At expected sidecar sizes (hundreds to low thousands
of entries), scan latency is microseconds; a secondary CID→sidecar_ids
index is a self-contained future optimization if profile data warrants
it.

## Data Model

### `SidecarId`

```rust
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SidecarId(Uuid);

impl SidecarId {
    pub fn new() -> Self { Self(Uuid::new_v4()) }
    pub fn parse_str(s: &str) -> Result<Self, uuid::Error> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

impl std::fmt::Display for SidecarId {
    // Hyphenated lowercase, e.g. "8b4f7c2e-1a3d-4f5b-9c0e-1234567890ab".
}
```

UUID v4 chosen for: cross-restart stability, cross-device uniqueness
(future-proofs sidecar sync without changes to identifier shape), and
opacity (callers can't conflate identity with content). Tracing logs
render short-form (`uuid[..8]`) for readability.

### `ContentIndexEntry`

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentIndexEntry {
    pub sidecar_id: SidecarId,
    #[serde(with = "hex_cid")]
    pub cid: [u8; 32],
    pub file_name: String,
    pub size_bytes: u64,
    pub stored_at_ms: u64,
    pub sensitivity: Sensitivity,
    pub replication_tier: ReplicationTier,
    pub licensed: bool,
    pub archived: bool,
    pub pinned: bool,
    pub kind: ContentKind,
}
```

`sidecar_id` is a required field; no `#[serde(default)]`. The on-disk
schema stays `version: 1` — there are no v1-shaped sidecars in the
wild apart from two test files that get re-uploaded post-deploy. Any
v1-without-`sidecar_id` file fails deserialization → `read_file`
returns `None` → `load()` falls back to empty (the existing
malformed-JSON path).

### `ContentIndex` API

| Method | Signature | Notes |
|---|---|---|
| `insert` | `fn insert(&mut self, entry) -> bool` | unchanged shape; collision means duplicate `sidecar_id` (effectively impossible with UUID v4) |
| `get` | `fn get(&self, id: &SidecarId) -> Option<&Entry>` | replaces CID-keyed `get` |
| `remove` | `fn remove(&mut self, id: &SidecarId) -> bool` | replaces CID-keyed `remove` |
| `set_pinned` | `fn set_pinned(&mut self, id: &SidecarId, pinned: bool) -> bool` | sidecar_id-keyed |
| `set_archived` | `fn set_archived(&mut self, id: &SidecarId, archived: bool) -> bool` | sidecar_id-keyed |
| `set_replication_tier` | `fn set_replication_tier(&mut self, ids: &[SidecarId], tier) -> usize` | sidecar_id-keyed |
| `rekey` | `fn rekey(&mut self, id: &SidecarId, new_cid, size, stored_at_ms) -> Result<(), RekeyError>` | sidecar_id-keyed; `RekeyError::Collision` removed |
| `entries_for_cid` | `fn entries_for_cid(&self, cid: &[u8; 32]) -> impl Iterator<Item = &Entry>` | new; backs OR-join logic |
| `is_cid_pinned_by_any` | `fn is_cid_pinned_by_any(&self, cid: &[u8; 32]) -> bool` | new; convenience over `entries_for_cid` |
| `entries` | unchanged | iteration order undefined |

`RekeyError` simplifies to a single variant:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RekeyError {
    OldMissing,
}
```

`Collision` is gone — two entries sharing a CID is now legal by design,
so `rekey` cannot clobber an unrelated entry.

## Wire Format & Tauri Commands

### `ContentItemWire`

```rust
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentItemWire {
    pub sidecar_id: String,       // UUID v4 hyphenated; "" for manifest-derived rows
    pub cid: String,              // hex
    pub name: String,
    pub size_bytes: u64,
    pub stored_at: u64,
    pub sensitivity: String,
    pub replication_tier: String,
    pub pinned: bool,
    pub licensed: bool,
    pub archived: bool,
    pub kind: String,
}
```

Manifest-derived rows from `list_folder` (children that have no sidecar
entry of their own) emit `sidecar_id: ""`. Frontend treats empty
`sidecarId` as "no sidecar mutations apply" — pin/burn/archive buttons
gated on truthy sidecarId. Rejected alternative: `Option<String>` /
`null`. The `""` sentinel keeps the type uniform and adds one explicit
parse-time guard at the IPC seam.

### Tauri command signature changes

| Command | Before | After |
|---|---|---|
| `pin_content` | `cid: String` | `sidecar_id: String` |
| `unpin_content` | `cid: String` | `sidecar_id: String` |
| `burn_content` | `cid: String` | `sidecar_id: String` |
| `archive_content` | `cid: String` | `sidecar_id: String` |
| `set_replication_tier` | `cids: Vec<String>` | `sidecar_ids: Vec<String>` |
| `export_content` | `cid: String, file_name: String` | unchanged (CID-addressed bytes) |
| `list_content` | `folder_cid: Option<String>` | unchanged (CID-addressed access) |
| `create_folder_at_root` | `name, child_cids` | unchanged params; return widens to `{ sidecarId, cid }` |
| `create_folder_nested` | `parent_cid, name, child_cids` | `parent_sidecar_id, name, child_cids` |

Helpers:
- `parse_sidecar_id(s: &str) -> Result<SidecarId, String>` — new; rejects empty string, malformed UUID.
- `parse_cid_hex` retained for CID-typed parameters.

`create_folder_at_root` returns the new entry's `sidecarId` alongside
its `cid` so the frontend can stash the sidecar_id immediately for
follow-up operations on the just-created folder.

## Pin / Burn / Rekey Semantics

**Invariant:** Runtime `pin_intent` contains CID `X` if and only if some
sidecar entry references `X` with `pinned == true`.

Every command that mutates the sidecar's pin landscape restores this
invariant via `Pin` / `Unpin` / `Burn` runtime verbs.

### Per-row display

The wire's `pinned` field becomes `entry.pinned` only. The
`joined_pinned` helper is deleted — its `runtime_pinned` parameter
becomes unused once the OR-join with `runtime_pinned.contains(cid)` is
removed. `list_root` and `list_folder` read `entry.pinned` directly.

With shared CIDs the old OR-join would surprise users: Alpha pinned,
Beta's row also lights up because the cache holds the shared CID. Slack
between sidecar intent and runtime effect that the OR-join used to
surface is now precluded by the invariant — every command keeps runtime
pin_intent consistent with the OR-of-entries.

### `pin_content`

1. `idx.set_pinned(sidecar_id, true)`.
2. Dispatch runtime `Pin { cid: entry.cid }`. Idempotent if cid already
   in pin_intent (e.g., another sidecar entry already pins the same
   CID).

### `unpin_content`

1. `idx.set_pinned(sidecar_id, false)`.
2. Look up the entry's CID. If `idx.is_cid_pinned_by_any(cid) == false`,
   dispatch runtime `Unpin { cid }`. Otherwise skip — another entry
   still wants it.

### `burn_content` (B-conservative)

Three-branch logic:

1. Read the entry's CID.
2. `idx.remove(sidecar_id)`.
3. Branch on remaining state:
   - **No entries reference this CID** → dispatch runtime `Burn { cid }`
     (drops cache + clears pin_intent — current behavior).
   - **Entries remain, none still pin** → dispatch runtime `Unpin { cid }`
     (cache stays warm, eviction unblocked).
   - **Entries remain, at least one still pins** → no runtime action.

### Rekey (called by `create_folder_nested` and future move/rename)

1. Read entry's old CID.
2. `idx.rekey(sidecar_id, new_cid, size, stored_at_ms)`.
3. For `old_cid`: if `is_cid_pinned_by_any(old_cid) == false`, dispatch
   runtime `Unpin { cid: old_cid }`.
4. For `new_cid`: if `is_cid_pinned_by_any(new_cid) == true`, dispatch
   runtime `Pin { cid: new_cid }`.

The rekey + Pin/Unpin sequence isn't transactional, but the invariant
only needs to hold *eventually* — runtime pin_intent is a hint; W-TinyLFU
still evicts based on access patterns. We tolerate brief skew (consistent
with how slice 1's rekey + sidecar pin sync already operates).

### Pin restoration on `start_node` (ZEB-155 path)

Today: iterate entries with `pinned=true` and dispatch one `Pin` per
entry. Post-ZEB-164 multiple entries can share a CID; switch to
dedupe-via-HashSet then dispatch one `Pin` per unique CID. Cleaner debug
logs, identical effect.

### Empty-folder workaround removal

The slice-1 guard in `create_folder_at_root` ("a folder with identical
contents already exists; add content to it before creating another empty
folder") is removed. Two `create_folder("Photos", [])` calls each produce
a fresh `SidecarId`, both pointing to the same empty-bundle CID. Both
rows visible in the root list with their own names.

## Frontend (`src/lib/`)

`types.ts`: `ContentItem` adds `sidecarId: string` (mirrors wire).

`file-manager-service.ts` invocation surface:

- `pinContent({ sidecarId })`, `unpinContent({ sidecarId })`,
  `burnContent({ sidecarId })`, `archiveContent({ sidecarId })`.
- `setReplicationTier({ sidecarIds, tier })`.
- `exportContent({ cid, fileName })` and `listFolderContents({ folderCid })`
  unchanged (CID-addressed).
- `createFolderAtRoot` return type: `{ sidecarId: string; cid: string }`.
- `createFolderNested` parameter: `{ parentSidecarId }`.

`FileBrowser.svelte`:

- `{#each items as item (item.sidecarId)}` — sidecarId is the stable key
  for selection state, pin/burn/archive callbacks, and selection sets.
- `currentFolderCid` keeps using CID (navigation is CID-addressed).
- `exportContent` keeps using CID.
- Manifest rows (`sidecarId === ''`) keep "no sidecar" styling — pin/burn/
  archive buttons gated on truthy sidecarId.

## Tests

### `content_index.rs` unit tests

- `load_v1_without_sidecar_id_returns_empty` — fixture with old schema
  fails deserialization gracefully.
- `insert_assigns_unique_ids_round_trips` — two entries with identical
  content CID round-trip with their distinct sidecar_ids preserved.
- `get_remove_by_sidecar_id` — basic identity-keyed CRUD.
- `rekey_by_sidecar_id_preserves_user_state` — file_name, sensitivity,
  replication_tier, licensed, archived, pinned, kind carry forward.
- `rekey_no_collision_when_target_cid_already_used` — two entries can
  converge on a shared CID without error (replaces current
  `rekey_refuses_collision` negative test).
- `entries_for_cid_returns_all_matching` — three entries, two share a
  CID, scan returns those two.
- `is_cid_pinned_by_any_or_joins_entries` — false when all unpinned,
  true when one pinned, true when many.
- `rekey_by_sidecar_id_old_missing` — `RekeyError::OldMissing` surfaces
  when no entry matches.

### `lib.rs` unit tests

- (no `joined_pinned` test — the helper is deleted; per-row `pinned`
  reads `entry.pinned` directly in `list_root` / `list_folder`.)
- `parse_sidecar_id_valid_invalid` — UUID parse error path; empty string
  rejected.

### Integration scenarios

Covered by Rust integration tests where feasible, otherwise as documented
manual smoke checklist:

- Create two empty folders → both succeed, distinct sidecar_ids, shared CID.
- Pin Alpha (shared CID with Beta) → Alpha's row pinned, Beta's row
  unpinned, runtime pin_intent has CID.
- Burn Alpha when Beta unpinned → Beta intact, runtime gets `Unpin`
  (not `Burn`).
- Burn Alpha when Beta pinned → Beta intact, no runtime action.
- Burn last reference → runtime `Burn` dispatched (unchanged).
- Add content to Alpha (shared CID with Beta) → Alpha rekeys, Beta
  intact, pin_intent migrates from old_cid to new_cid.

## Sequencing

- **Land before ZEB-162 (move):** Move's stable user-facing identity
  surface wants `sidecar_id` first.
- **Land before ZEB-156 (root-pin-set cascade):** ZEB-156 builds
  per-entry pin semantics on top of the sidecar_id key.
- **Bundles `start_node` dedupe tweak from ZEB-155:** small change,
  fits inside this ticket.
- **Orthogonal to ZEB-167 (nested rekey rollback):** independent.

## Out of Scope

- **"Linked entries" UI distinction.** Two folders sharing a CID render
  as independent rows in v1. UX research can revisit.
- **Cross-device sidecar sync.** UUID v4 makes that future-compatible at
  zero cost; actual sync is a separate project.
- **Secondary CID→sidecar_ids index.** Promotion path open if profile
  data warrants it.
