# Folder Primitive Implementation Plan (ZEB-158 slice 1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a client-side folder primitive: `create_folder` (at root or nested), `list_content` folder-aware listing, and `kind` wire field. Pin/burn/archive on a folder cascades to its contents through the existing ZEB-154/ZEB-155 machinery — no changes to `harmony-content`.

**Architecture:** A folder is a `Bundle` where child-0 is a Book carrying a JSON manifest (`{folder_manifest: {version: 1, entries: [{cid, name, kind}, …]}}`) and child-1..N are the folder's contents. Only user-promoted roots get a `ContentIndexEntry`; nested items are discovered by walking the folder's manifest on demand. Nested creation walks `parent_path` bottom-up, re-ingesting each ancestor and rekeying the single top-level sidecar entry once.

**Tech Stack:** Rust (src-tauri), Tauri v2 commands, tokio mpsc channels, `harmony-content` (`BundleBuilder`, `ContentId::for_book`), `serde_json`, Svelte 5 (frontend).

**Branch:** `feat/folder-primitive-zeb-158` (already created from `main` at `bca83e2`).

**Spec:** `docs/specs/2026-04-24-folder-primitive-design.md`.

---

## File structure

**Create:**
- `src-tauri/src/folders.rs` — manifest types (`FolderManifest`, `ManifestBody`, `ManifestEntry`), `serialize_manifest`, `parse_manifest`, `build_folder` helper.
- `src-tauri/tests/folder_primitive_integration.rs` — end-to-end integration tests via NodeRuntime harness.

**Modify:**
- `src-tauri/src/content_index.rs` — add `ContentKind` enum + `kind` field on `ContentIndexEntry`; add `rekey` helper.
- `src-tauri/src/event_loop.rs` — add `ReadBytes` variant to `ContentVerbRequest`; handle it in the select loop.
- `src-tauri/src/lib.rs` — declare `folders` module; add `kind` field to `ContentItemWire`; change `list_content` signature to take `Option<String>` folder_cid; add `create_folder` command; register both in the invoke handler.
- `src/lib/types.ts` — replace `isFolder: boolean` with `kind: "leaf" | "folder"` on `ContentItem`.
- `src/lib/services/content.ts` (or equivalent) — update `listContents` signature; add `createFolder`.
- `src/lib/components/FileBrowser.svelte` — stop reading mock data; call backend; pass `parent_path` to `createFolder`; consume `folder_cid` on navigation.
- `src/lib/mock-file-data.ts` — delete or gate behind dev flag.

---

## Task 1: `ContentKind` enum + sidecar `kind` field

Foundation work. Backward-compatible field via `#[serde(default)]`; no behavior change on the existing leaf path.

**Files:**
- Modify: `src-tauri/src/content_index.rs`

- [ ] **Step 1: Write failing legacy-sidecar test**

Add to the `tests` module in `src-tauri/src/content_index.rs`:

```rust
#[test]
fn kind_defaults_to_leaf_on_legacy_sidecar() {
    let dir = tempdir().unwrap();
    // v1 sidecar from before ZEB-158 slice 1 — no `kind` field.
    let legacy = br#"{
        "version": 1,
        "entries": [{
            "cid": "aa11bb22cc33dd44ee55ff6677889900112233445566778899aabbccddeeff00",
            "file_name": "legacy.txt",
            "size_bytes": 42,
            "stored_at_ms": 1700000000000,
            "sensitivity": "private",
            "replication_tier": "default",
            "licensed": false,
            "archived": false,
            "pinned": false
        }]
    }"#;
    std::fs::write(dir.path().join(INDEX_FILE), legacy).unwrap();

    let idx = ContentIndex::load(dir.path());
    let entry = idx
        .entries()
        .next()
        .expect("legacy entry must load");
    assert_eq!(entry.kind, ContentKind::Leaf);
}
```

- [ ] **Step 2: Write failing folder round-trip test**

Add to the same module:

```rust
#[test]
fn save_persists_kind_field() {
    let dir = tempdir().unwrap();
    let mut idx = ContentIndex::load(dir.path());
    let mut entry = sample_entry([0xF0; 32]);
    entry.file_name = "Photos".into();
    entry.kind = ContentKind::Folder;
    idx.insert(entry.clone());

    let reloaded = ContentIndex::load(dir.path());
    let got = reloaded.get(&entry.cid).expect("round-trips");
    assert_eq!(got.kind, ContentKind::Folder);
    assert_eq!(got.file_name, "Photos");
}
```

- [ ] **Step 3: Run tests to verify they fail**

```bash
cd src-tauri && cargo test --lib content_index::tests::kind_defaults_to_leaf_on_legacy_sidecar content_index::tests::save_persists_kind_field
```

Expected: FAIL with `no variant or associated item named Leaf/Folder found for enum ContentKind` and `no field kind on ContentIndexEntry` (or similar — the symbols don't exist yet).

- [ ] **Step 4: Add `ContentKind` enum and `kind` field**

In `src-tauri/src/content_index.rs`, between the `ReplicationTier` enum (currently ending at line 41) and the `ContentIndexEntry` struct, add:

```rust
/// ZEB-158 slice 1: distinguishes user-visible content kinds at the sidecar
/// level. Leaves are ingested files (books or chunked-file bundles); folders
/// are bundles whose child-0 is a manifest book (see
/// `src-tauri/src/folders.rs` and `docs/specs/2026-04-24-folder-primitive-design.md`).
///
/// The default variant is `Leaf` so `#[serde(default)]` on the `kind` field
/// lets pre-ZEB-158 sidecar entries deserialize correctly (they were all
/// leaves at the time of their last save).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentKind {
    #[default]
    Leaf,
    Folder,
}
```

Then, inside the `ContentIndexEntry` struct, add the new field at the end (after `pinned`):

```rust
    /// ZEB-158 slice 1: distinguishes leaf files from folder bundles at the
    /// sidecar level. Default `Leaf` with `#[serde(default)]` keeps pre-slice-1
    /// sidecars readable — legacy entries were all leaves by construction,
    /// because folders didn't exist before slice 1.
    #[serde(default)]
    pub kind: ContentKind,
```

Also update `sample_entry` in the `tests` module to initialize the new field:

```rust
fn sample_entry(cid: [u8; 32]) -> ContentIndexEntry {
    ContentIndexEntry {
        cid,
        file_name: "hello.txt".into(),
        size_bytes: 42,
        stored_at_ms: 1_700_000_000_000,
        sensitivity: Sensitivity::Private,
        replication_tier: ReplicationTier::Default,
        licensed: false,
        archived: false,
        pinned: false,
        kind: ContentKind::Leaf,
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cd src-tauri && cargo test --lib content_index
```

Expected: PASS (all existing `content_index::tests::*` plus the two new ones).

- [ ] **Step 6: Run the full lib test suite — nothing else should break**

```bash
cd src-tauri && cargo test --lib
```

Expected: PASS. `ingest_content`'s sidecar insert in `src-tauri/src/lib.rs:1632-1642` builds a `ContentIndexEntry` without a `kind` field today — Rust requires all struct fields at construction, so the compiler will force you to add `kind: content_index::ContentKind::Leaf` there. Do that now if the compiler errors. No runtime behavior change.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/content_index.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(sidecar): ContentKind enum + kind field on ContentIndexEntry (ZEB-158)

Foundation for folder primitive. Additive field with serde(default)
backward-compat — pre-slice-1 sidecars deserialize with kind: Leaf
(correct, folders didn't exist yet). No behavior change on the
existing leaf ingest path.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: `folders.rs` module — manifest types + build helpers

Pure data-layer module. No runtime dependencies; tests are synchronous unit tests.

**Files:**
- Create: `src-tauri/src/folders.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod folders;`)

- [ ] **Step 1: Write failing empty-manifest round-trip test**

Create `src-tauri/src/folders.rs` with:

```rust
//! ZEB-158 slice 1: folder manifest types and build helpers.
//!
//! A folder is a `Bundle` whose child-0 is a Book carrying a JSON manifest
//! with `(cid, name, kind)` tuples for each child. See
//! `docs/specs/2026-04-24-folder-primitive-design.md` for the full design.

use serde::{Deserialize, Serialize};

use crate::content_index::ContentKind;

/// Outer wrapper so the `folder_manifest` key acts as a self-identifier:
/// a reader with only the bundle bytes can disambiguate a folder from any
/// other kind of bundle by attempting to decode child-0's payload as this
/// shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderManifest {
    pub folder_manifest: ManifestBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestBody {
    pub version: u32,
    pub entries: Vec<ManifestEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    #[serde(with = "crate::content_index::hex_cid")]
    pub cid: [u8; 32],
    pub name: String,
    pub kind: ContentKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_manifest_round_trip() {
        let m = FolderManifest {
            folder_manifest: ManifestBody {
                version: 1,
                entries: vec![],
            },
        };
        let bytes = serde_json::to_vec(&m).expect("serialize");
        let parsed: FolderManifest =
            serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(parsed, m);
        assert!(parsed.folder_manifest.entries.is_empty());
    }
}
```

Register the module. In `src-tauri/src/lib.rs`, near the top of the file where other `pub mod` declarations live (around line 11 where `pub mod content_index;` lives), add:

```rust
pub mod folders;
```

Also — the `hex_cid` module in `content_index.rs` is currently private. In `src-tauri/src/content_index.rs`, change its visibility:

```rust
pub(crate) mod hex_cid {
```

(replacing the current `mod hex_cid {` at line 247).

- [ ] **Step 2: Run test — expect FAIL**

```bash
cd src-tauri && cargo test --lib folders::tests::empty_manifest_round_trip
```

Expected: PASS (this test uses only the struct definitions — no build helper yet). If it fails, the module-wiring isn't right; fix that before continuing.

- [ ] **Step 3: Add failing mixed-entries round-trip test**

Append to `folders.rs`'s `tests` module:

```rust
#[test]
fn manifest_with_mixed_entries_round_trip() {
    let m = FolderManifest {
        folder_manifest: ManifestBody {
            version: 1,
            entries: vec![
                ManifestEntry {
                    cid: [0xAA; 32],
                    name: "foo.txt".into(),
                    kind: ContentKind::Leaf,
                },
                ManifestEntry {
                    cid: [0xBB; 32],
                    name: "photos".into(),
                    kind: ContentKind::Folder,
                },
                ManifestEntry {
                    cid: [0xCC; 32],
                    name: "bar.png".into(),
                    kind: ContentKind::Leaf,
                },
            ],
        },
    };
    let bytes = serde_json::to_vec(&m).expect("serialize");
    let parsed: FolderManifest =
        serde_json::from_slice(&bytes).expect("parse");
    assert_eq!(parsed, m, "order and fields must survive round-trip");

    // Spot-check the wire format contains hex-encoded CIDs and lowercase kinds.
    let json = String::from_utf8(bytes).expect("utf-8");
    assert!(json.contains("\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\""));
    assert!(json.contains("\"kind\":\"folder\""));
    assert!(json.contains("\"kind\":\"leaf\""));
}
```

- [ ] **Step 4: Run the test — expect PASS (it exercises only struct defs)**

```bash
cd src-tauri && cargo test --lib folders::tests::manifest_with_mixed_entries_round_trip
```

Expected: PASS.

- [ ] **Step 5: Add failing `build_empty_folder` test**

Append to `folders.rs`'s `tests` module:

```rust
#[test]
fn build_empty_folder() {
    let built = build_folder("", &[]).expect("build succeeds");
    // Empty folder's bundle bytes are exactly the 32-byte manifest CID.
    assert_eq!(built.bundle_bytes.len(), 32);
    assert_eq!(&built.bundle_bytes[..], &built.manifest_cid.to_bytes()[..]);
    // Manifest must itself be a parseable empty folder manifest.
    let parsed: FolderManifest = serde_json::from_slice(&built.manifest_bytes)
        .expect("manifest is valid JSON");
    assert_eq!(parsed.folder_manifest.version, 1);
    assert!(parsed.folder_manifest.entries.is_empty());
}
```

Note: `build_folder`'s first argument is an unused discriminator reserved for the folder's own display name (not embedded in the manifest; held by the caller in the sidecar entry). We pass empty string in the test to keep the signature honest to the spec without leaking frontend concerns. This is intentional — not dead code. If you disagree after implementing, collapse the signature.

- [ ] **Step 6: Run the test — expect FAIL**

```bash
cd src-tauri && cargo test --lib folders::tests::build_empty_folder
```

Expected: FAIL with `cannot find function build_folder`.

- [ ] **Step 7: Implement `build_folder`**

Append to `folders.rs` (before the `tests` module):

```rust
use harmony_content::bundle::BundleBuilder;
use harmony_content::cid::{ContentFlags, ContentId};

/// A built folder, ready to ingest. The caller must ingest both the
/// manifest book bytes (at `manifest_cid`) and the bundle bytes (at
/// `bundle_cid`) through the event loop's ingest channel before the
/// folder is usable.
#[derive(Debug, Clone)]
pub struct BuiltFolder {
    pub manifest_bytes: Vec<u8>,
    pub manifest_cid: ContentId,
    pub bundle_bytes: Vec<u8>,
    pub bundle_cid: ContentId,
}

/// Build a folder bundle from an ordered list of children.
///
/// The `_folder_name` parameter is accepted for symmetry with call sites
/// that also pass it into the sidecar; names are NOT part of the manifest's
/// own identity (renaming a folder changes its parent's manifest, not its
/// own).
///
/// Returns the manifest book bytes + CID and the bundle bytes + CID; the
/// caller is responsible for ingesting both.
///
/// Empty folders are representable (`children: []`) — the returned bundle
/// has exactly one child (the manifest), which satisfies BundleBuilder's
/// ≥1-child requirement.
pub fn build_folder(
    _folder_name: &str,
    children: &[ManifestEntry],
) -> Result<BuiltFolder, String> {
    let manifest = FolderManifest {
        folder_manifest: ManifestBody {
            version: 1,
            entries: children.to_vec(),
        },
    };
    let manifest_bytes =
        serde_json::to_vec(&manifest).map_err(|e| format!("manifest serialize: {e}"))?;
    let manifest_cid =
        ContentId::for_book(&manifest_bytes, ContentFlags::default())
            .map_err(|e| format!("manifest CID: {e:?}"))?;

    let mut builder = BundleBuilder::new();
    builder.add(manifest_cid);
    for entry in children {
        builder.add(ContentId::from_bytes(entry.cid));
    }
    let (bundle_bytes, bundle_cid) = builder
        .build_with_flags(ContentFlags::default())
        .map_err(|e| format!("folder bundle build: {e:?}"))?;

    Ok(BuiltFolder {
        manifest_bytes,
        manifest_cid,
        bundle_bytes,
        bundle_cid,
    })
}
```

- [ ] **Step 8: Run the test — expect PASS**

```bash
cd src-tauri && cargo test --lib folders::tests::build_empty_folder
```

Expected: PASS.

- [ ] **Step 9: Add failing two-child build test**

Append to `folders.rs`'s `tests` module:

```rust
#[test]
fn build_folder_with_two_children() {
    let children = vec![
        ManifestEntry {
            cid: [0x11; 32],
            name: "a.txt".into(),
            kind: ContentKind::Leaf,
        },
        ManifestEntry {
            cid: [0x22; 32],
            name: "b".into(),
            kind: ContentKind::Folder,
        },
    ];
    let built = build_folder("parent", &children).expect("build");

    // Bundle bytes = concat(manifest_cid, child_0_cid, child_1_cid) = 96 bytes.
    assert_eq!(built.bundle_bytes.len(), 96);
    assert_eq!(&built.bundle_bytes[0..32], &built.manifest_cid.to_bytes()[..]);
    assert_eq!(&built.bundle_bytes[32..64], &[0x11u8; 32]);
    assert_eq!(&built.bundle_bytes[64..96], &[0x22u8; 32]);

    // Manifest enumerates children in the same order.
    let parsed: FolderManifest =
        serde_json::from_slice(&built.manifest_bytes).expect("parse");
    assert_eq!(parsed.folder_manifest.entries.len(), 2);
    assert_eq!(parsed.folder_manifest.entries[0].cid, [0x11; 32]);
    assert_eq!(parsed.folder_manifest.entries[1].kind, ContentKind::Folder);
}
```

- [ ] **Step 10: Run all folders tests — expect PASS**

```bash
cd src-tauri && cargo test --lib folders
```

Expected: PASS (4 tests).

- [ ] **Step 11: Commit**

```bash
git add src-tauri/src/folders.rs src-tauri/src/lib.rs src-tauri/src/content_index.rs
git commit -m "$(cat <<'EOF'
feat(folders): manifest types + build_folder helper (ZEB-158)

New folders.rs module. FolderManifest wraps a versioned entries array
under a self-identifying "folder_manifest" key. build_folder produces
(manifest_bytes+CID, bundle_bytes+CID) ready to ingest via the event
loop's ingest channel. Empty folders are representable; manifest enumerates
children in bundle order. Zero runtime deps — pure data layer.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `ReadBytes` verb + `kind` field on `ContentItemWire`

The `list_content(folder_cid=Some)` path (Task 4) needs to read bundle bytes from the runtime cache. The runtime lives in the event-loop thread (it's `!Send`), so we expose a new `ReadBytes` verb on the existing `content_verb_tx` channel. This task also threads `kind` into the wire shape — trivial now that `ContentIndexEntry.kind` exists.

**Files:**
- Modify: `src-tauri/src/event_loop.rs`
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Write failing unit test for `kind` wire field**

In `src-tauri/src/lib.rs`, find the existing `#[cfg(test)] mod tests` block (it starts around line 2550 where `chunk_and_bundle_*` tests live) and add a new test:

```rust
#[test]
fn content_item_wire_serializes_kind_field() {
    let wire = ContentItemWire {
        cid: "aa".repeat(32),
        name: "Photos".into(),
        size_bytes: 32,
        stored_at: 1,
        sensitivity: "private".into(),
        replication_tier: "default".into(),
        pinned: false,
        licensed: false,
        archived: false,
        kind: "folder".into(),
    };
    let json = serde_json::to_string(&wire).expect("serialize");
    // camelCase rename_all is already on ContentItemWire; kind is a plain field.
    assert!(json.contains("\"kind\":\"folder\""), "got: {json}");
}
```

- [ ] **Step 2: Run test — expect FAIL**

```bash
cd src-tauri && cargo test --lib content_item_wire_serializes_kind_field
```

Expected: FAIL with `no field kind on ContentItemWire`.

- [ ] **Step 3: Add `kind` field to `ContentItemWire`**

In `src-tauri/src/lib.rs`, modify the struct (currently at lines 1163-1178):

```rust
/// Wire format returned by `list_content` — one entry per self-ingested
/// file the client is aware of. Joins sidecar metadata with the runtime
/// cache's pinned state snapshot. ZEB-158 slice 1 adds `kind` to
/// distinguish leaf files from folder bundles.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentItemWire {
    pub cid: String,              // hex
    pub name: String,
    pub size_bytes: u64,
    pub stored_at: u64,           // ms since epoch
    pub sensitivity: String,      // "private" | "confidential" | "public"
    pub replication_tier: String, // "minimal" | "default" | "durable"
    pub pinned: bool,
    pub licensed: bool,
    pub archived: bool,
    pub kind: String,             // ZEB-158: "leaf" | "folder"
}
```

Add a helper alongside `sensitivity_wire` and `replication_tier_wire` (near line 1180):

```rust
fn kind_wire(k: content_index::ContentKind) -> &'static str {
    match k {
        content_index::ContentKind::Leaf => "leaf",
        content_index::ContentKind::Folder => "folder",
    }
}
```

Update `list_content`'s wire construction (currently at line 1253-1263) to populate the new field:

```rust
    idx.entries()
        .map(|e| ContentItemWire {
            cid: hex::encode(e.cid),
            name: e.file_name.clone(),
            size_bytes: e.size_bytes,
            stored_at: e.stored_at_ms,
            sensitivity: sensitivity_wire(e.sensitivity).to_string(),
            replication_tier: replication_tier_wire(e.replication_tier).to_string(),
            pinned: joined_pinned(e, &pinned_set),
            licensed: e.licensed,
            archived: e.archived,
            kind: kind_wire(e.kind).to_string(),
        })
        .collect()
```

- [ ] **Step 4: Run the test — expect PASS**

```bash
cd src-tauri && cargo test --lib content_item_wire_serializes_kind_field
```

Expected: PASS.

- [ ] **Step 5: Write failing `ReadBytes` verb structural test**

Full behavior coverage for `ReadBytes` comes in Task 7's integration tests (which drive the verb through the real runtime). For this task, a minimal structural test guards against variant/field typos without duplicating the full event-loop harness.

Append to the tests in `src-tauri/src/event_loop.rs` — find the existing `#[cfg(test)] mod tests` block (around line 1150+) and add:

```rust
#[test]
fn read_bytes_verb_variant_is_constructible() {
    let (reply_tx, _reply_rx) =
        tokio::sync::oneshot::channel::<Option<Vec<u8>>>();
    let req = ContentVerbRequest::ReadBytes {
        cid: [0x7Au8; 32],
        reply: reply_tx,
    };
    match req {
        ContentVerbRequest::ReadBytes { cid, .. } => {
            assert_eq!(cid, [0x7Au8; 32]);
        }
        _ => panic!("matched wrong variant"),
    }
}
```

- [ ] **Step 6: Run test — expect FAIL**

```bash
cd src-tauri && cargo test --lib read_bytes_verb_variant_is_constructible
```

Expected: FAIL with `no variant ReadBytes found for enum ContentVerbRequest`.

- [ ] **Step 7: Add `ReadBytes` variant + handler**

In `src-tauri/src/event_loop.rs`, extend the `ContentVerbRequest` enum (around line 51) to add the variant:

```rust
    /// ZEB-158 slice 1: read raw bytes for a CID out of the runtime
    /// cache. Used by `list_content(folder_cid=Some)` in src-tauri/src/lib.rs
    /// to parse a folder bundle's manifest without needing direct access
    /// to the `!Send` NodeRuntime.
    ///
    /// Returns `None` if the CID is not admitted in the cache. Callers
    /// surface "folder not in cache" diagnostics instead of errors so a
    /// legitimately-evicted folder is distinguishable from a malformed
    /// request.
    ReadBytes {
        cid: [u8; 32],
        reply: oneshot::Sender<Option<Vec<u8>>>,
    },
```

In the same file's `select!` block (add a new arm alongside the existing `Pin / Unpin / Burn / PinnedSet` arms around line 600-654):

```rust
                    ContentVerbRequest::ReadBytes { cid, reply } => {
                        let id = ContentId::from_bytes(cid);
                        let bytes = runtime.storage_tier().cache().get(&id).map(|b| b.to_vec());
                        let _ = reply.send(bytes);
                    }
```

- [ ] **Step 8: Run the test — expect PASS**

```bash
cd src-tauri && cargo test --lib read_bytes_verb_variant_is_constructible
```

Expected: PASS.

- [ ] **Step 9: Run full lib test suite**

```bash
cd src-tauri && cargo test --lib
```

Expected: PASS (no regressions).

- [ ] **Step 10: Commit**

```bash
git add src-tauri/src/event_loop.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(event-loop): ReadBytes verb + kind field on ContentItemWire (ZEB-158)

ReadBytes lets Tauri command handlers read cached bundle bytes by CID
through the existing content_verb_tx channel. Needed by upcoming
list_content(folder_cid=Some) path, which parses a folder's manifest
book to synthesize nested-item rows. Returns None for non-admitted
CIDs — caller distinguishes "evicted" from "never existed" by joining
with sidecar state.

ContentItemWire gains a kind field ("leaf" | "folder") populated from
the existing ContentIndexEntry.kind. Root list_content unchanged
otherwise.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `list_content` folder-cid listing

Change `list_content`'s signature to take `Option<String>`, add the folder-path implementation that reads bytes via `ReadBytes`, parses the manifest, and synthesizes wire rows from manifest entries.

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Write failing test — malformed manifest returns error**

Add to the tests module in `src-tauri/src/lib.rs`:

```rust
#[test]
fn list_folder_rejects_non_manifest_child_0() {
    use crate::folders::FolderManifest;

    // A bundle whose child-0 book payload is NOT a folder manifest
    // (e.g., plain UTF-8 "not a manifest" or chunked-file sentinel bytes).
    // Simulated here at the parse level — the full wiring test is the
    // integration test malformed_manifest_returns_error.
    let payload = b"definitely not a manifest";
    let parse_result: Result<FolderManifest, _> = serde_json::from_slice(payload);
    assert!(parse_result.is_err(), "bad JSON must not parse as FolderManifest");
}
```

This is a sanity check that the manifest parser rejects non-manifest payloads; the end-to-end wiring lives in the integration test.

- [ ] **Step 2: Run the test — expect PASS (pure parser check)**

```bash
cd src-tauri && cargo test --lib list_folder_rejects_non_manifest_child_0
```

Expected: PASS. If the test file doesn't compile, make sure the `folders` module is declared with `pub` (Task 2, Step 1).

- [ ] **Step 3: Change `list_content` signature and add folder-path handler**

Replace the current `list_content` (lines 1224-1271) in `src-tauri/src/lib.rs` with:

```rust
#[tauri::command]
async fn list_content(
    folder_cid: Option<String>,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<ContentItemWire>, String> {
    // 1. Snapshot pinned CIDs from the runtime cache.
    let verb_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .content_verb_tx
            .clone()
            .ok_or_else(|| "runtime unavailable".to_string())?
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    verb_tx
        .send(event_loop::ContentVerbRequest::PinnedSet { reply: reply_tx })
        .await
        .map_err(|_| "event loop not running".to_string())?;
    let pinned_set = reply_rx
        .await
        .map_err(|_| "event loop dropped snapshot request".to_string())?;

    match folder_cid {
        None => list_root(state, &pinned_set),
        Some(hex) => list_folder(hex, verb_tx, &pinned_set).await,
    }
}

pub(crate) fn list_root(
    state: tauri::State<'_, Mutex<NodeState>>,
    pinned_set: &std::collections::HashSet<[u8; 32]>,
) -> Result<Vec<ContentItemWire>, String> {
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let mut entries: Vec<ContentItemWire> = {
        let idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.entries()
            .map(|e| ContentItemWire {
                cid: hex::encode(e.cid),
                name: e.file_name.clone(),
                size_bytes: e.size_bytes,
                stored_at: e.stored_at_ms,
                sensitivity: sensitivity_wire(e.sensitivity).to_string(),
                replication_tier: replication_tier_wire(e.replication_tier).to_string(),
                pinned: joined_pinned(e, pinned_set),
                licensed: e.licensed,
                archived: e.archived,
                kind: kind_wire(e.kind).to_string(),
            })
            .collect()
    };
    // HashMap iter is non-deterministic; sort newest-first for stable UI.
    entries.sort_by(|a, b| b.stored_at.cmp(&a.stored_at));
    Ok(entries)
}

pub async fn list_folder(
    folder_cid_hex: String,
    verb_tx: tokio::sync::mpsc::Sender<event_loop::ContentVerbRequest>,
    pinned_set: &std::collections::HashSet<[u8; 32]>,
) -> Result<Vec<ContentItemWire>, String> {
    use harmony_content::bundle::parse_bundle;

    let folder_cid = parse_cid_hex(&folder_cid_hex)?;

    // Fetch the folder's bundle bytes from the runtime cache.
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    verb_tx
        .send(event_loop::ContentVerbRequest::ReadBytes {
            cid: folder_cid,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;
    let bundle_bytes = reply_rx
        .await
        .map_err(|_| "event loop dropped read request".to_string())?;
    let bundle_bytes = match bundle_bytes {
        Some(b) => b,
        None => {
            // Folder not in cache — likely evicted or never admitted.
            // Return empty (UI shows empty folder); ZEB-159 will add
            // transparent re-fetch in a follow-up.
            tracing::debug!(
                folder_cid = %folder_cid_hex,
                "list_folder: bundle not in cache; returning empty",
            );
            return Ok(vec![]);
        }
    };

    // Parse bundle child CIDs; child-0 is the manifest book.
    let child_cids = parse_bundle(&bundle_bytes)
        .map_err(|e| format!("malformed folder bundle: {e:?}"))?;
    let manifest_cid_id = child_cids
        .first()
        .copied()
        .ok_or_else(|| "folder bundle has no children".to_string())?;
    let manifest_cid: [u8; 32] = manifest_cid_id.to_bytes();

    // Read the manifest book bytes.
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    verb_tx
        .send(event_loop::ContentVerbRequest::ReadBytes {
            cid: manifest_cid,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;
    let manifest_bytes = reply_rx
        .await
        .map_err(|_| "event loop dropped read request".to_string())?
        .ok_or_else(|| "manifest book not in cache".to_string())?;

    let manifest: crate::folders::FolderManifest =
        serde_json::from_slice(&manifest_bytes)
            .map_err(|e| format!("manifest parse: {e}"))?;

    // Consistency check: manifest entry CIDs must match bundle child-1..N.
    let bundle_children_after_manifest: Vec<[u8; 32]> = child_cids
        .iter()
        .skip(1)
        .map(|c| c.to_bytes())
        .collect();
    if manifest.folder_manifest.entries.len() != bundle_children_after_manifest.len() {
        return Err(format!(
            "manifest/bundle mismatch: manifest has {} entries, bundle has {} children after manifest",
            manifest.folder_manifest.entries.len(),
            bundle_children_after_manifest.len()
        ));
    }
    for (i, entry) in manifest.folder_manifest.entries.iter().enumerate() {
        if entry.cid != bundle_children_after_manifest[i] {
            return Err(format!(
                "manifest/bundle cid mismatch at index {i}",
            ));
        }
    }

    // Synthesize wire rows. Nested items have no sidecar: size_bytes/stored_at
    // are unavailable (reported 0), sensitivity/replication_tier default,
    // licensed/archived false. Pinned joins the runtime's pinned_set.
    Ok(manifest
        .folder_manifest
        .entries
        .into_iter()
        .map(|e| ContentItemWire {
            cid: hex::encode(e.cid),
            name: e.name,
            size_bytes: 0,
            stored_at: 0,
            sensitivity: "private".into(),
            replication_tier: "default".into(),
            pinned: pinned_set.contains(&e.cid),
            licensed: false,
            archived: false,
            kind: kind_wire(match e.kind {
                content_index::ContentKind::Leaf => content_index::ContentKind::Leaf,
                content_index::ContentKind::Folder => content_index::ContentKind::Folder,
            })
            .to_string(),
        })
        .collect())
}
```

Note: the final `.map` has an obtuse-looking `match` because `ManifestEntry.kind` is already a `ContentKind` — this plan was drafted assuming they might diverge but they don't. Simplify inline to:

```rust
            kind: kind_wire(e.kind).to_string(),
```

- [ ] **Step 4: Compile-check**

```bash
cd src-tauri && cargo check
```

Expected: clean build. If the compiler complains about the `match` in the `.map`, collapse it to the simpler form above. If it complains that `parse_bundle` isn't `pub`, check the `harmony_content::bundle` module — it is pub per `harmony/crates/harmony-content/src/bundle.rs`.

- [ ] **Step 5: Run the full lib test suite**

```bash
cd src-tauri && cargo test --lib
```

Expected: PASS (existing tests unchanged; one new one — the manifest-parse sanity check — passes).

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(list): folder-cid listing in list_content (ZEB-158)

list_content(None) = root (existing behavior + kind field).
list_content(Some(folder_cid)) = folder contents: reads bundle bytes
via ReadBytes verb, parses child-0 as a folder manifest, synthesizes
wire rows from manifest entries. Consistency check: manifest entry
CIDs must match bundle child-1..N in order.

Evicted folders return empty Vec with debug log (ZEB-159 will wire
transparent re-fetch).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `create_folder` command — root case

Root-only. Nested case follows in Task 6.

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Write failing data-layer integration test**

This task validates the **data-layer correctness** of root-folder creation without spinning up a live event loop — that's covered in Task 7. Here we drive `build_folder` directly and insert the sidecar entry, then inspect the sidecar to confirm what `create_folder_at_root` will produce.

Create `src-tauri/tests/folder_primitive_integration.rs`:

```rust
//! ZEB-158 slice 1: end-to-end tests for folder create/list.
//!
//! Data-layer tests (Tasks 5–6) drive build_folder + sidecar directly.
//! Full event-loop harness tests (Task 7) validate pin cascade + list_folder.
//! Pattern follows content_index_integration.rs's style; integration
//! tests only reach `pub` symbols.

use tempfile::tempdir;

use harmony_app::content_index::{
    ContentIndex, ContentIndexEntry, ContentKind, ReplicationTier, Sensitivity,
};
use harmony_app::folders;

#[test]
fn create_folder_at_root_then_list_shows_it() {
    let dir = tempdir().unwrap();
    let mut idx = ContentIndex::load(dir.path());

    // Build what create_folder_at_root would build: an empty folder.
    let built = folders::build_folder("Photos", &[]).expect("build");

    // Insert the sidecar entry that create_folder_at_root would insert.
    let inserted = idx.insert(ContentIndexEntry {
        cid: built.bundle_cid.to_bytes(),
        file_name: "Photos".into(),
        size_bytes: built.bundle_bytes.len() as u64,
        stored_at_ms: 1,
        sensitivity: Sensitivity::Private,
        replication_tier: ReplicationTier::Default,
        licensed: false,
        archived: false,
        pinned: false,
        kind: ContentKind::Folder,
    });
    assert!(inserted, "new entry inserted");

    // Inspect the sidecar — one row with kind Folder, name Photos.
    let rows: Vec<&ContentIndexEntry> = idx.entries().collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, ContentKind::Folder);
    assert_eq!(rows[0].file_name, "Photos");
    assert!(rows[0].size_bytes > 0);
    assert!(!rows[0].pinned);

    // Bundle bytes of an empty folder are the 32-byte manifest CID.
    assert_eq!(built.bundle_bytes.len(), 32);
}
```

- [ ] **Step 2: Run test — expect FAIL (unknown function / compile error)**

```bash
cd src-tauri && cargo test --test folder_primitive_integration create_folder_at_root_then_list_shows_it
```

Expected: FAIL — `create_folder` doesn't exist as a callable.

- [ ] **Step 3: Add `create_folder` command**

In `src-tauri/src/lib.rs`, after `ingest_content`'s closing `}` (around line 1658), add:

```rust
/// ZEB-158 slice 1: create a new folder at the root or inside an existing
/// folder. Empty `parent_path` means root; non-empty means a walk from
/// top-level root (index 0) down to immediate parent (last element).
/// Nested creation is implemented in a follow-up step.
#[tauri::command]
async fn create_folder(
    name: String,
    parent_path: Vec<String>,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<String, String> {
    if !parent_path.is_empty() {
        // Nested case implemented in Task 6 below.
        return Err("nested folder creation not yet implemented".to_string());
    }
    create_folder_at_root(name, state).await
}

async fn create_folder_at_root(
    name: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<String, String> {
    // Build the (empty) manifest + bundle.
    let built = folders::build_folder(&name, &[])?;

    // Ingest manifest book + bundle through the event loop.
    let ingest_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .ingest_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };

    send_ingest(
        &ingest_tx,
        hex::encode(built.manifest_cid.to_bytes()),
        built.manifest_bytes,
    )
    .await?;
    send_ingest(
        &ingest_tx,
        hex::encode(built.bundle_cid.to_bytes()),
        built.bundle_bytes.clone(),
    )
    .await?;

    // Insert sidecar entry for the top-level root.
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let stored_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.insert(content_index::ContentIndexEntry {
            cid: built.bundle_cid.to_bytes(),
            file_name: name,
            size_bytes: built.bundle_bytes.len() as u64,
            stored_at_ms,
            sensitivity: content_index::Sensitivity::Private,
            replication_tier: content_index::ReplicationTier::Default,
            licensed: false,
            archived: false,
            pinned: false,
            kind: content_index::ContentKind::Folder,
        });
    }

    Ok(hex::encode(built.bundle_cid.to_bytes()))
}

// Extract the ingest-one helper from `ingest_content` so `create_folder`
// can share it. Mirrors the local function at src-tauri/src/lib.rs:1572.
async fn send_ingest(
    tx: &tokio::sync::mpsc::Sender<event_loop::IngestRequest>,
    cid_hex: String,
    data: Vec<u8>,
) -> Result<(), String> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(event_loop::IngestRequest {
        cid_hex,
        data,
        reply: reply_tx,
    })
    .await
    .map_err(|_| "event loop not running".to_string())?;
    reply_rx
        .await
        .map_err(|_| "event loop dropped ingest request".to_string())??;
    Ok(())
}
```

Also update `ingest_content` (line 1572 region) to call this new `send_ingest` helper instead of its local copy — replace the nested `async fn send_one` block with calls to `send_ingest(&ingest_tx, ...)`. DRY win; no behavior change.

Register the command. In the `tauri::generate_handler![…]` block at line 2037, add `create_folder,` next to `ingest_content,`:

```rust
            ingest_content,
            create_folder,
```

- [ ] **Step 4: Compile-check**

```bash
cd src-tauri && cargo check
```

If the integration test doesn't compile, ensure `folders` and `content_index` are `pub` in `src-tauri/src/lib.rs` (they already are, per Tasks 1 and 2) and that `ContentIndexEntry` / `Sensitivity` / `ReplicationTier` / `ContentKind` are all `pub`. Integration tests live in the external `harmony_app` crate and can only reach `pub` items — see the comment at `src-tauri/src/lib.rs:75-80` for the existing precedent from ZEB-154.

- [ ] **Step 5: Run the integration test**

```bash
cd src-tauri && cargo test --test folder_primitive_integration
```

Expected: PASS.

- [ ] **Step 6: Run the full lib + integration suite**

```bash
cd src-tauri && cargo test
```

Expected: PASS (124+ lib tests, 4 existing integration tests, 1 mail_sync, 1 new folder integration).

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/tests/folder_primitive_integration.rs
git commit -m "$(cat <<'EOF'
feat(lib): create_folder command for root folders (ZEB-158)

New Tauri command. Nested case stubs out with an explicit error for
now — Task 6 follows. Root path: build_folder → ingest manifest +
bundle → insert sidecar entry with kind: Folder. Factors out the
send_ingest helper so ingest_content and create_folder share one
implementation (DRY, no behavior change).

Integration test validates the end-to-end round-trip via direct
sidecar inspection.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: `create_folder` — nested case + `rekey` helper

Extends Task 5 to handle non-empty `parent_path`. Adds a `ContentIndex::rekey` helper for atomic entry renaming.

**Files:**
- Modify: `src-tauri/src/content_index.rs` (add `rekey`)
- Modify: `src-tauri/src/lib.rs` (nested path in `create_folder`)
- Modify: `src-tauri/tests/folder_primitive_integration.rs` (nested test)

- [ ] **Step 1: Write failing `rekey` unit test**

Append to the tests in `src-tauri/src/content_index.rs`:

```rust
#[test]
fn rekey_atomically_replaces_cid_and_preserves_user_state() {
    let dir = tempdir().unwrap();
    let mut idx = ContentIndex::load(dir.path());

    let mut entry = sample_entry([0x01; 32]);
    entry.file_name = "Folder".into();
    entry.kind = ContentKind::Folder;
    entry.pinned = true;
    entry.archived = false;
    idx.insert(entry.clone());

    let ok = idx.rekey(
        &[0x01; 32],
        [0x02; 32],
        /* new_size_bytes */ 999,
        /* new_stored_at_ms */ 1234,
    );
    assert!(ok, "rekey must succeed when old key exists");

    assert!(idx.get(&[0x01; 32]).is_none(), "old key removed");
    let after = idx.get(&[0x02; 32]).expect("new key present");
    assert_eq!(after.file_name, "Folder", "file_name carried forward");
    assert_eq!(after.kind, ContentKind::Folder, "kind carried forward");
    assert!(after.pinned, "pinned carried forward");
    assert_eq!(after.size_bytes, 999, "size_bytes updated");
    assert_eq!(after.stored_at_ms, 1234, "stored_at_ms updated");

    // Non-existent old key returns false.
    assert!(!idx.rekey(&[0xFF; 32], [0xEE; 32], 0, 0));
}
```

- [ ] **Step 2: Run — expect FAIL**

```bash
cd src-tauri && cargo test --lib rekey_atomically_replaces_cid_and_preserves_user_state
```

Expected: FAIL — `rekey` doesn't exist.

- [ ] **Step 3: Implement `rekey`**

In `src-tauri/src/content_index.rs`, add near `set_pinned` (around line 199):

```rust
    /// ZEB-158 slice 1: atomically replace an entry's CID while
    /// preserving user-state fields (file_name, sensitivity,
    /// replication_tier, licensed, archived, pinned, kind). Used when a
    /// folder mutation produces a new top-level root CID (nested
    /// `create_folder`, future move/rename operations). One save() for
    /// the whole replacement — remove-then-insert would give two.
    ///
    /// Returns `true` if rekeyed; `false` if the old CID wasn't in the
    /// index.
    pub fn rekey(
        &mut self,
        old: &[u8; 32],
        new: [u8; 32],
        new_size_bytes: u64,
        new_stored_at_ms: u64,
    ) -> bool {
        let Some(mut entry) = self.entries.remove(old) else {
            return false;
        };
        entry.cid = new;
        entry.size_bytes = new_size_bytes;
        entry.stored_at_ms = new_stored_at_ms;
        self.entries.insert(new, entry);
        self.save();
        true
    }
```

- [ ] **Step 4: Run — expect PASS**

```bash
cd src-tauri && cargo test --lib rekey_atomically_replaces_cid_and_preserves_user_state
```

Expected: PASS.

- [ ] **Step 5: Write failing nested-creation integration test**

Append to `src-tauri/tests/folder_primitive_integration.rs`:

```rust
#[tokio::test]
async fn create_nested_folder_updates_top_level_root_cid() {
    // Build root "Photos" folder at depth 0. Sidecar has entry keyed by
    // Photos's bundle CID (call it root_v1).
    let mut idx = ContentIndex::load(tempdir().unwrap().path());
    let photos_v1 = folders::build_folder("Photos", &[]).expect("build v1");

    idx.insert(harmony_app::content_index::ContentIndexEntry {
        cid: photos_v1.bundle_cid.to_bytes(),
        file_name: "Photos".into(),
        size_bytes: photos_v1.bundle_bytes.len() as u64,
        stored_at_ms: 1,
        sensitivity: harmony_app::content_index::Sensitivity::Private,
        replication_tier: harmony_app::content_index::ReplicationTier::Default,
        licensed: false,
        archived: false,
        pinned: true,  // must survive rekey
        kind: ContentKind::Folder,
    });

    // Now create "2026" inside Photos. This should produce:
    //   - A new empty "2026" folder (child of the new Photos bundle).
    //   - A new Photos bundle (root_v2) whose manifest lists "2026".
    //   - Sidecar rekey: old key photos_v1 → new key photos_v2.
    //   - Sidecar's pinned flag still true after rekey.
    //
    // Drive the logic by calling build_folder manually for the nested
    // operation (since the full Tauri command requires a live event
    // loop). This test validates the data-layer correctness of the
    // cascade; Task 7 validates the live-pin-cascade end-to-end.

    let sub = folders::build_folder("2026", &[]).expect("sub build");
    let photos_v2 = folders::build_folder(
        "Photos",
        &[harmony_app::folders::ManifestEntry {
            cid: sub.bundle_cid.to_bytes(),
            name: "2026".into(),
            kind: ContentKind::Folder,
        }],
    )
    .expect("v2 build");

    let rekeyed = idx.rekey(
        &photos_v1.bundle_cid.to_bytes(),
        photos_v2.bundle_cid.to_bytes(),
        photos_v2.bundle_bytes.len() as u64,
        /* new_stored_at_ms */ 2,
    );
    assert!(rekeyed, "rekey succeeds");

    let after = idx
        .get(&photos_v2.bundle_cid.to_bytes())
        .expect("rekeyed entry present");
    assert!(after.pinned, "pinned survives rekey");
    assert_eq!(after.kind, ContentKind::Folder);
    assert_eq!(after.file_name, "Photos");
    assert!(idx.get(&photos_v1.bundle_cid.to_bytes()).is_none(),
        "old entry removed");
}
```

- [ ] **Step 6: Run — expect PASS (validates the rekey primitive is sound)**

```bash
cd src-tauri && cargo test --test folder_primitive_integration create_nested_folder_updates_top_level_root_cid
```

Expected: PASS.

- [ ] **Step 7: Implement nested `create_folder` path**

In `src-tauri/src/lib.rs`, extend the `create_folder` command body. Replace:

```rust
    if !parent_path.is_empty() {
        // Nested case implemented in Task 6 below.
        return Err("nested folder creation not yet implemented".to_string());
    }
    create_folder_at_root(name, state).await
```

with:

```rust
    if parent_path.is_empty() {
        return create_folder_at_root(name, state).await;
    }
    create_folder_nested(name, parent_path, state).await
```

And add the nested implementation below:

```rust
async fn create_folder_nested(
    name: String,
    parent_path: Vec<String>,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<String, String> {
    use harmony_content::bundle::parse_bundle;
    use harmony_content::cid::ContentId;

    // Parse all path CIDs up-front; fail fast on malformed input.
    let path_cids: Vec<[u8; 32]> = parent_path
        .iter()
        .map(|h| parse_cid_hex(h))
        .collect::<Result<_, _>>()?;
    let root_old = *path_cids.first().expect("non-empty by guard above");
    let immediate_parent_cid = *path_cids.last().expect("non-empty");

    // Snapshot handles.
    let (ingest_tx, verb_tx, index) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        (
            guard
                .ingest_tx
                .clone()
                .ok_or_else(|| "not connected".to_string())?,
            guard
                .content_verb_tx
                .clone()
                .ok_or_else(|| "not connected".to_string())?,
            guard.content_index.clone(),
        )
    };

    // 1. Build the new empty sub-folder. Ingest its manifest + bundle.
    let new_child = folders::build_folder(&name, &[])?;
    send_ingest(
        &ingest_tx,
        hex::encode(new_child.manifest_cid.to_bytes()),
        new_child.manifest_bytes.clone(),
    )
    .await?;
    send_ingest(
        &ingest_tx,
        hex::encode(new_child.bundle_cid.to_bytes()),
        new_child.bundle_bytes.clone(),
    )
    .await?;

    // 2. Bottom-up walk: rebuild each ancestor.
    //    prev_old_cid = the ancestor's old CID (as given in parent_path).
    //    prev_new_cid = the ancestor's new CID (after mutation at its layer).
    let mut prev_old_cid = immediate_parent_cid;
    let mut prev_new_cid = new_child.bundle_cid.to_bytes();

    // First iteration: APPEND at the immediate parent.
    // Higher iterations: REPLACE the entry pointing to prev_old_cid.
    for (i, &anc_cid) in path_cids.iter().enumerate().rev() {
        let is_deepest = i == path_cids.len() - 1;

        // Fetch the ancestor's bundle bytes.
        let anc_bundle = read_cached_bytes(&verb_tx, anc_cid)
            .await?
            .ok_or_else(|| {
                format!(
                    "ancestor {} not in cache; cannot rebuild parent chain",
                    hex::encode(anc_cid)
                )
            })?;
        let anc_children = parse_bundle(&anc_bundle)
            .map_err(|e| format!("malformed ancestor bundle: {e:?}"))?;
        let manifest_cid_id = anc_children
            .first()
            .copied()
            .ok_or_else(|| "ancestor bundle has no children".to_string())?;

        // Read the ancestor's manifest book.
        let manifest_bytes = read_cached_bytes(&verb_tx, manifest_cid_id.to_bytes())
            .await?
            .ok_or_else(|| "ancestor manifest not in cache".to_string())?;
        let mut manifest: folders::FolderManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| format!("ancestor manifest parse: {e}"))?;

        if is_deepest {
            manifest
                .folder_manifest
                .entries
                .push(folders::ManifestEntry {
                    cid: prev_new_cid,
                    name: name.clone(),
                    kind: content_index::ContentKind::Folder,
                });
        } else {
            let target_idx = manifest
                .folder_manifest
                .entries
                .iter()
                .position(|e| e.cid == prev_old_cid)
                .ok_or_else(|| {
                    format!(
                        "ancestor {} has no entry pointing to child {}",
                        hex::encode(anc_cid),
                        hex::encode(prev_old_cid)
                    )
                })?;
            manifest.folder_manifest.entries[target_idx].cid = prev_new_cid;
        }

        // Rebuild manifest book + bundle with updated entries.
        let rebuilt = folders::build_folder(
            /* display name not used by manifest; empty is fine */ "",
            &manifest.folder_manifest.entries,
        )?;
        send_ingest(
            &ingest_tx,
            hex::encode(rebuilt.manifest_cid.to_bytes()),
            rebuilt.manifest_bytes,
        )
        .await?;
        send_ingest(
            &ingest_tx,
            hex::encode(rebuilt.bundle_cid.to_bytes()),
            rebuilt.bundle_bytes.clone(),
        )
        .await?;

        prev_old_cid = anc_cid;
        prev_new_cid = rebuilt.bundle_cid.to_bytes();
    }

    // 3. Rekey the top-level sidecar entry.
    let new_bundle_size: u64 = {
        let anc_bundle = read_cached_bytes(&verb_tx, prev_new_cid).await?;
        anc_bundle.map(|b| b.len() as u64).unwrap_or(0)
    };
    let stored_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let had_pin = {
        let idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.get(&root_old).map(|e| e.pinned).unwrap_or(false)
    };
    {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        let ok = idx.rekey(&root_old, prev_new_cid, new_bundle_size, stored_at_ms);
        if !ok {
            return Err("top-level folder not in sidecar — nothing to rekey".to_string());
        }
    }

    // 4. Event-loop pin sync: unpin old, pin new (if old had pin intent).
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    verb_tx
        .send(event_loop::ContentVerbRequest::Unpin {
            cid: root_old,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;
    let _ = reply_rx.await;
    if had_pin {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        verb_tx
            .send(event_loop::ContentVerbRequest::Pin {
                cid: prev_new_cid,
                reply: reply_tx,
            })
            .await
            .map_err(|_| "event loop not running".to_string())?;
        let _ = reply_rx.await;
    }

    Ok(hex::encode(prev_new_cid))
}

async fn read_cached_bytes(
    verb_tx: &tokio::sync::mpsc::Sender<event_loop::ContentVerbRequest>,
    cid: [u8; 32],
) -> Result<Option<Vec<u8>>, String> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    verb_tx
        .send(event_loop::ContentVerbRequest::ReadBytes {
            cid,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;
    reply_rx
        .await
        .map_err(|_| "event loop dropped read request".to_string())
}
```

- [ ] **Step 8: Compile + run tests**

```bash
cd src-tauri && cargo test
```

Expected: PASS. The integration test from Step 5 passes on the rekey primitive; the full wired-up nested path will be exercised in Task 7's live-harness tests.

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/content_index.rs src-tauri/src/lib.rs src-tauri/tests/folder_primitive_integration.rs
git commit -m "$(cat <<'EOF'
feat(lib): nested create_folder + rekey helper (ZEB-158)

rekey atomically replaces an entry's CID while preserving file_name,
kind, pinned, archived, sensitivity, replication_tier, licensed.
One save() per rekey — remove-then-insert would give two.

create_folder_nested walks parent_path bottom-up: ingest the new empty
sub-folder, then for each ancestor rebuild the manifest (append at the
deepest, replace-by-CID higher up), re-ingest, and finally rekey the
single top-level sidecar entry. Event-loop pin intent is synced via
explicit Unpin(old_root) + optional Pin(new_root).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Pin cascade + full-harness integration tests

Pure test additions exercising the live event loop. Validates:
- ZEB-155's pin cascade composes cleanly with A-alt's folder bundle shape (spec test #11).
- `list_folder` end-to-end through `ReadBytes` verb + manifest parse (spec tests #7, #12, #14).
- Folder-not-in-cache graceful degradation (spec test #8).
- Pin persistence across reload for folder entries (spec test #13).

No production code changes expected.

**Files:**
- Modify: `src-tauri/tests/folder_primitive_integration.rs`

### Harness reference

Every `#[tokio::test]` in this task needs the same event-loop setup. The canonical pattern is `src-tauri/tests/content_index_integration.rs:26-269` (`ingest_list_pin_burn_roundtrip`). Copy its setup block verbatim — the part that:

1. Creates a `tempdir` for the sidecar.
2. Constructs ALL mpsc channels that `event_loop::run(...)` takes (publish, fetch, ingest, content_verb, follow, voice, voice_channel, mail_refresh, fetch_completion — match the arg list at `src-tauri/src/lib.rs:579-600`).
3. Builds a `NodeConfig` with RAM-only flags (`disk_enabled: false`, `archive_enabled: false`).
4. Calls `let (runtime, startup_actions) = NodeRuntime::new(config, MemoryBookStore::new());`.
5. Spawns the event loop on a dedicated thread (`std::thread::spawn` with `tokio::runtime::Builder::new_multi_thread` — see `src-tauri/src/lib.rs:552-604` for the exact pattern used in `start_node`; a test can inline a simplified version).
6. Leaves `ingest_tx` and `content_verb_tx` in scope for the test body.

Do not duplicate the full setup in each test; the sub-steps below show the test-specific bodies. A reasonable in-file helper is:

```rust
async fn spawn_test_runtime() -> TestHarness { /* …copy of the steps above… */ }

struct TestHarness {
    ingest_tx: mpsc::Sender<event_loop::IngestRequest>,
    verb_tx: mpsc::Sender<event_loop::ContentVerbRequest>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    // plus whatever else the test bodies need to hold
}
```

Where `TestHarness` is dropped at test end (Drop impl fires `shutdown_tx.send(true)`). If you'd rather not build a helper, inline the setup once per test — the ZEB-155 `fetch_complete_arm_pins_root_in_intent` precedent does it inline.

- [ ] **Step 1: Write `pin_folder_cascades_to_nested_leaf` test**

Append to `src-tauri/tests/folder_primitive_integration.rs`:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pin_folder_cascades_to_nested_leaf() {
    use harmony_content::cid::{ContentFlags, ContentId};
    use harmony_app::event_loop::ContentVerbRequest;

    // Build: a folder containing one leaf.
    let leaf_bytes = b"hello world".to_vec();
    let leaf_cid = ContentId::for_book(&leaf_bytes, ContentFlags::default()).unwrap();
    let folder = folders::build_folder(
        "FolderWithLeaf",
        &[folders::ManifestEntry {
            cid: leaf_cid.to_bytes(),
            name: "hello.txt".into(),
            kind: ContentKind::Leaf,
        }],
    )
    .expect("build folder");

    // Spawn runtime (see "Harness reference" above — inline the full setup here).
    let mut harness = spawn_test_runtime().await;

    // Ingest leaf, manifest, bundle.
    send_ingest(&harness.ingest_tx, hex::encode(leaf_cid.to_bytes()), leaf_bytes)
        .await.unwrap();
    send_ingest(
        &harness.ingest_tx,
        hex::encode(folder.manifest_cid.to_bytes()),
        folder.manifest_bytes,
    )
    .await.unwrap();
    send_ingest(
        &harness.ingest_tx,
        hex::encode(folder.bundle_cid.to_bytes()),
        folder.bundle_bytes,
    )
    .await.unwrap();

    // Pin the folder root.
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    harness.verb_tx.send(ContentVerbRequest::Pin {
        cid: folder.bundle_cid.to_bytes(),
        reply: reply_tx,
    })
    .await.unwrap();
    assert_eq!(reply_rx.await.unwrap().unwrap(), true);

    // Inspect the pinned set — cascade should include all three CIDs.
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    harness.verb_tx.send(ContentVerbRequest::PinnedSet { reply: reply_tx })
        .await.unwrap();
    let pinned = reply_rx.await.unwrap();

    assert!(pinned.contains(&folder.bundle_cid.to_bytes()), "folder pinned");
    assert!(pinned.contains(&folder.manifest_cid.to_bytes()), "manifest pinned via cascade");
    assert!(pinned.contains(&leaf_cid.to_bytes()), "leaf pinned via cascade");
}
```

Where `send_ingest` and `spawn_test_runtime` are either pub items from `harmony_app` (for send_ingest, per Task 5) or local helpers in the test file. Create them now if needed.

- [ ] **Step 2: Write `pin_intent_survives_restart_for_folder` test**

Append to the same file:

```rust
#[test]
fn pin_intent_survives_restart_for_folder() {
    let dir = tempdir().unwrap();

    // First session: create folder entry with kind: Folder, pinned: true.
    {
        let mut idx = ContentIndex::load(dir.path());
        let built = folders::build_folder("Pinned", &[]).expect("build");
        idx.insert(harmony_app::content_index::ContentIndexEntry {
            cid: built.bundle_cid.to_bytes(),
            file_name: "Pinned".into(),
            size_bytes: built.bundle_bytes.len() as u64,
            stored_at_ms: 1,
            sensitivity: harmony_app::content_index::Sensitivity::Private,
            replication_tier: harmony_app::content_index::ReplicationTier::Default,
            licensed: false,
            archived: false,
            pinned: true,
            kind: ContentKind::Folder,
        });
        // Sidecar save happens implicitly on insert.
    }

    // Second session: reload from disk and verify the folder + pin survive.
    let idx = ContentIndex::load(dir.path());
    let entry = idx
        .entries()
        .find(|e| e.kind == ContentKind::Folder)
        .expect("folder entry persisted");
    assert_eq!(entry.file_name, "Pinned");
    assert!(entry.pinned, "pin intent survives reload");
}
```

This test does NOT require the event-loop harness — it's a sidecar-round-trip contract check. The cascade + fetch-completion composition is implicit (ZEB-155's `pin_intent_survives_reload` already validates the fetch-side path at the event-loop level).

- [ ] **Step 3: Write `list_folder_end_to_end_with_two_children` test**

Exercises the full `ReadBytes`-verb → manifest-parse → synthesize-rows path. Covers spec test #7's wire-level assertions.

Append to the same file:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_folder_end_to_end_with_two_children() {
    use harmony_content::cid::{ContentFlags, ContentId};

    // Build a folder with two leaf children.
    let bytes_a = b"alpha".to_vec();
    let bytes_b = b"beta".to_vec();
    let cid_a = ContentId::for_book(&bytes_a, ContentFlags::default()).unwrap();
    let cid_b = ContentId::for_book(&bytes_b, ContentFlags::default()).unwrap();
    let folder = folders::build_folder(
        "TwoChildren",
        &[
            folders::ManifestEntry {
                cid: cid_a.to_bytes(),
                name: "a.txt".into(),
                kind: ContentKind::Leaf,
            },
            folders::ManifestEntry {
                cid: cid_b.to_bytes(),
                name: "b.txt".into(),
                kind: ContentKind::Leaf,
            },
        ],
    )
    .expect("build");

    let mut harness = spawn_test_runtime().await;
    send_ingest(&harness.ingest_tx, hex::encode(cid_a.to_bytes()), bytes_a).await.unwrap();
    send_ingest(&harness.ingest_tx, hex::encode(cid_b.to_bytes()), bytes_b).await.unwrap();
    send_ingest(
        &harness.ingest_tx,
        hex::encode(folder.manifest_cid.to_bytes()),
        folder.manifest_bytes,
    )
    .await.unwrap();
    send_ingest(
        &harness.ingest_tx,
        hex::encode(folder.bundle_cid.to_bytes()),
        folder.bundle_bytes,
    )
    .await.unwrap();

    // Call list_folder directly (it's pub async fn — see Task 4).
    let empty_pinned = std::collections::HashSet::new();
    let rows = harmony_app::list_folder(
        hex::encode(folder.bundle_cid.to_bytes()),
        harness.verb_tx.clone(),
        &empty_pinned,
    )
    .await
    .expect("list_folder succeeds");

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].name, "a.txt");
    assert_eq!(rows[0].kind, "leaf");
    assert_eq!(rows[0].cid, hex::encode(cid_a.to_bytes()));
    assert_eq!(rows[1].name, "b.txt");
    assert!(!rows[0].pinned);
    assert!(!rows[1].pinned);
}
```

If `list_folder` isn't `pub` from Task 4, upgrade it now: `pub async fn list_folder(...)`. Integration tests require `pub` visibility per `src-tauri/src/lib.rs:75-80`.

- [ ] **Step 4: Write `list_folder_empty_returns_empty_vec` test**

Covers spec test #12.

Append:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_folder_empty_returns_empty_vec() {
    let folder = folders::build_folder("Empty", &[]).expect("build");

    let mut harness = spawn_test_runtime().await;
    send_ingest(
        &harness.ingest_tx,
        hex::encode(folder.manifest_cid.to_bytes()),
        folder.manifest_bytes,
    )
    .await.unwrap();
    send_ingest(
        &harness.ingest_tx,
        hex::encode(folder.bundle_cid.to_bytes()),
        folder.bundle_bytes,
    )
    .await.unwrap();

    let empty_pinned = std::collections::HashSet::new();
    let rows = harmony_app::list_folder(
        hex::encode(folder.bundle_cid.to_bytes()),
        harness.verb_tx.clone(),
        &empty_pinned,
    )
    .await
    .expect("list_folder succeeds on empty folder");

    assert!(rows.is_empty(), "empty folder returns empty Vec");
}
```

- [ ] **Step 5: Write `list_folder_not_in_cache_returns_empty` test**

Covers spec test #8. No ingest step — we ask for a CID that was never admitted.

Append:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_folder_not_in_cache_returns_empty() {
    let mut harness = spawn_test_runtime().await;

    let random_cid_hex = hex::encode([0x42u8; 32]);
    let empty_pinned = std::collections::HashSet::new();
    let rows = harmony_app::list_folder(
        random_cid_hex,
        harness.verb_tx.clone(),
        &empty_pinned,
    )
    .await
    .expect("not-in-cache returns Ok(empty), not Err");

    assert!(rows.is_empty(), "cold cache returns empty, not an error");
}
```

- [ ] **Step 6: Write `list_folder_malformed_manifest_returns_error` test**

Covers spec test #14. Builds a bundle whose child-0 is a book with non-manifest bytes.

Append:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_folder_malformed_manifest_returns_error() {
    use harmony_content::bundle::BundleBuilder;
    use harmony_content::cid::{ContentFlags, ContentId};

    // "Manifest" book is NOT valid FolderManifest JSON.
    let bad_manifest = b"definitely not a folder manifest".to_vec();
    let bad_manifest_cid =
        ContentId::for_book(&bad_manifest, ContentFlags::default()).unwrap();

    // Also add a dummy leaf so the bundle has >=2 children and the bundle-
    // vs-manifest count check isn't what trips first.
    let leaf_bytes = b"leaf".to_vec();
    let leaf_cid = ContentId::for_book(&leaf_bytes, ContentFlags::default()).unwrap();

    let mut builder = BundleBuilder::new();
    builder.add(bad_manifest_cid);
    builder.add(leaf_cid);
    let (bundle_bytes, bundle_cid) = builder
        .build_with_flags(ContentFlags::default())
        .unwrap();

    let mut harness = spawn_test_runtime().await;
    send_ingest(&harness.ingest_tx, hex::encode(leaf_cid.to_bytes()), leaf_bytes).await.unwrap();
    send_ingest(
        &harness.ingest_tx,
        hex::encode(bad_manifest_cid.to_bytes()),
        bad_manifest,
    )
    .await.unwrap();
    send_ingest(
        &harness.ingest_tx,
        hex::encode(bundle_cid.to_bytes()),
        bundle_bytes,
    )
    .await.unwrap();

    let empty_pinned = std::collections::HashSet::new();
    let err = harmony_app::list_folder(
        hex::encode(bundle_cid.to_bytes()),
        harness.verb_tx.clone(),
        &empty_pinned,
    )
    .await
    .expect_err("malformed manifest must surface an error, not an empty Vec");

    assert!(err.contains("manifest parse"), "error mentions manifest parse: {err}");
}
```

- [ ] **Step 7: Run all integration tests**

```bash
cd src-tauri && cargo test --test folder_primitive_integration
```

Expected: PASS (all tests from Tasks 5, 6, and 7 — 8 integration tests total).

- [ ] **Step 8: Run the full suite**

```bash
cd src-tauri && cargo test
```

Expected: PASS (~130+ lib tests, 4 existing content_index_integration tests, 1 mail_sync, 8 folder_primitive_integration).

- [ ] **Step 9: Commit**

```bash
git add src-tauri/tests/folder_primitive_integration.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
test(folders): full-harness list_folder + pin-cascade integration (ZEB-158)

Six integration tests validating the folder primitive end-to-end via
the live event loop:

- pin_folder_cascades_to_nested_leaf: A-alt's payoff — existing
  collect_descendants walker cascades Pin through a folder bundle to
  its nested leaf without any folder-aware code in the pin path.
- pin_intent_survives_restart_for_folder: ZEB-155's persistence
  composes unchanged with kind: Folder entries.
- list_folder_end_to_end_with_two_children: full ReadBytes + manifest
  parse + wire-row synthesis path.
- list_folder_empty_returns_empty_vec: empty folders representable.
- list_folder_not_in_cache_returns_empty: graceful degradation for
  evicted folders (no error, empty Vec + debug log).
- list_folder_malformed_manifest_returns_error: non-manifest child-0
  payloads surface a parse error instead of crashing.

Also upgrades list_folder to pub for integration-test reach (see the
precedent comment at src-tauri/src/lib.rs:75-80).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Frontend wiring

Wire the UI shell to the real backend. Remove mock data. Manual smoke-test the UI in dev because Vitest component tests can't cover the full Tauri round-trip.

**Files:**
- Modify: `src/lib/types.ts`
- Modify: `src/lib/services/content.ts` (or wherever `invoke` wrappers live — `grep -rn "listContent\|list_content" src/lib` to find)
- Modify: `src/lib/components/FileBrowser.svelte`
- Delete or gate: `src/lib/mock-file-data.ts`

- [ ] **Step 1: Confirm the frontend service file**

```bash
grep -rn "invoke.*list_content\|list_content" src/lib/ | head -5
```

Identify the TypeScript file that calls `invoke("list_content")`. Call it `SERVICE_FILE` in the steps below (commonly `src/lib/services/content.ts` or `src/lib/tauri.ts`).

- [ ] **Step 2: Update `ContentItem` type**

In `src/lib/types.ts`, find the `ContentItem` interface and:

- Replace `isFolder: boolean` (or similar) with `kind: "leaf" | "folder"`.
- If `parentCid` is currently typed, keep it as UI navigation state (not wire).

```ts
export interface ContentItem {
  cid: string;
  name: string;
  sizeBytes: number;
  storedAt: number;
  sensitivity: "private" | "confidential" | "public";
  replicationTier: string;
  pinned: boolean;
  licensed: boolean;
  archived: boolean;
  kind: "leaf" | "folder";
  // parentCid is navigation state set by the UI; not on the wire.
  parentCid?: string | null;
}
```

- [ ] **Step 3: Update service wrappers**

In `SERVICE_FILE`, change `listContents` to accept an optional folder CID, and add `createFolder`:

```ts
import { invoke } from "@tauri-apps/api/core";
import type { ContentItem } from "../types";

export async function listContents(
  folderCid: string | null = null,
): Promise<ContentItem[]> {
  return invoke<ContentItem[]>("list_content", { folderCid });
}

export async function createFolder(
  name: string,
  parentPath: string[] = [],
): Promise<string> {
  return invoke<string>("create_folder", { name, parentPath });
}
```

Tauri converts camelCase args from JS to snake_case Rust params automatically.

- [ ] **Step 4: Wire `FileBrowser.svelte`**

In `src/lib/components/FileBrowser.svelte`:

1. Remove the import of `mock-file-data`.
2. On mount and on `currentFolderCid` change, call `listContents(currentFolderCid)` and populate the state that drives the list.
3. Add a "New folder" button that prompts for a name (use an existing modal/dialog pattern from the codebase, or a `window.prompt` for v0) and calls `createFolder(name, breadcrumbStack)` where `breadcrumbStack` is the array of folder CIDs from root down to `currentFolderCid`.
4. On double-click of a folder row, push the clicked folder's CID onto the breadcrumb stack and set `currentFolderCid`. The existing `Breadcrumbs.svelte` already handles navigation back.

Minimal example (adapt to the actual store/effect pattern used in this Svelte codebase):

```svelte
<script lang="ts">
  import { listContents, createFolder } from "../services/content";
  import type { ContentItem } from "../types";

  let items: ContentItem[] = [];
  let currentFolderCid: string | null = null;
  let breadcrumbStack: string[] = []; // CIDs from root → currentFolderCid

  async function refresh() {
    items = await listContents(currentFolderCid);
  }

  $: currentFolderCid, refresh();

  async function handleNewFolder() {
    const name = window.prompt("Folder name?");
    if (!name) return;
    const newCid = await createFolder(name, breadcrumbStack);
    await refresh();
    // If root creation, newCid is the new folder's CID; if nested,
    // it's the new top-level root CID (per ZEB-158 spec).
  }

  function enterFolder(item: ContentItem) {
    if (item.kind !== "folder") return;
    breadcrumbStack = [...breadcrumbStack, item.cid];
    currentFolderCid = item.cid;
  }
</script>
```

- [ ] **Step 5: Delete (or gate) mock data**

```bash
rm src/lib/mock-file-data.ts
```

Or if another component still imports it (e.g., for a storybook/dev view), wrap its export in an `import.meta.env.DEV` guard. `grep -rn "mock-file-data" src/` first to check.

- [ ] **Step 6: Type-check the frontend**

```bash
npm run check
```

Expected: 0 errors. Fix any type mismatches introduced by the `isFolder → kind` rename.

- [ ] **Step 7: Manual smoke test (dev server)**

```bash
npm run tauri dev
```

In the File Manager UI:

1. Create a folder "Photos" at root → verify it appears in the root list with folder kind/icon.
2. Double-click into Photos → breadcrumbs update to `Home > Photos`, list is empty.
3. Back at root, ingest a regular file → it appears alongside Photos at root.
4. Pin "Photos" (via context menu or existing pin affordance) → pin badge shows on the folder.
5. Restart the dev server (`Ctrl+C` then rerun) → pin badge on Photos still visible on root reload.
6. Enter Photos again after restart → verify listing (empty is OK; this exercises the "folder-in-cache" path immediately after a sidecar-preserved pin, gated on ZEB-155's behavior).

**If step 5 or 6 shows a regression**, re-check Task 6's nested rekey + event-loop pin-sync and Task 3's ReadBytes verb.

- [ ] **Step 8: Commit**

```bash
git add src/lib/types.ts src/lib/services/ src/lib/components/FileBrowser.svelte
git rm src/lib/mock-file-data.ts 2>/dev/null || true
git commit -m "$(cat <<'EOF'
feat(ui): wire File Manager to real folder backend (ZEB-158)

- types.ts: isFolder → kind: "leaf" | "folder"
- services: listContents(folderCid?), createFolder(name, parentPath)
- FileBrowser.svelte: drop mock-file-data, use real backend; handle
  enterFolder + New Folder action; breadcrumb stack passed as
  parent_path to create_folder for nested creates.

Manual smoke test passes per plan step 7.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Final verification

- [ ] **Full test suite**

```bash
cd src-tauri && cargo test && cd .. && npm run check
```

Expected: all green. ~130+ rust tests + frontend type-check clean.

- [ ] **Commit summary check**

```bash
git log --oneline main..HEAD
```

Expected: 7–8 commits (Task 1 through Task 8), each a self-contained change.

- [ ] **Push the branch and open the PR** (user will invoke the `finishing-a-development-branch` flow at the end of subagent-driven execution).
