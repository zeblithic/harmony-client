# Sidecar `SidecarId` Refactor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Re-key the client sidecar (`content-index.json`) by an opaque per-entry `SidecarId` (UUID v4) so multiple entries can share a CID — delivering the symlink-style mental model from ZEB-158 slice 1.

**Architecture:** `ContentIndex` becomes `HashMap<SidecarId, ContentIndexEntry>`. CID becomes a regular field. CAS layer unchanged. All Tauri commands keyed by `sidecar_id`; `export_content` and `list_content` keep CID-addressed access. A new "runtime pin_intent OR-join" invariant ties runtime pin state to the OR of all sidecar entries' `pinned` flags. The slice-1 empty-folder collision workaround is removed.

**Tech Stack:** Rust (Tauri v2), Svelte 5, `uuid` crate, content-addressed storage via `harmony-content`. See `docs/specs/2026-04-24-sidecar-id-refactor-design.md` for the full design.

---

## File Structure

| File | Change |
|---|---|
| `src-tauri/Cargo.toml` | Add `uuid = { version = "1", features = ["v4", "serde"] }` |
| `src-tauri/src/content_index.rs` | Add `SidecarId`; change `ContentIndex` to `HashMap<SidecarId, Entry>`; add `entries_for_cid`, `is_cid_pinned_by_any`; simplify `RekeyError` |
| `src-tauri/src/lib.rs` | `ContentItemWire.sidecarId`; Tauri commands flip CID→sidecar_id; OR-join invariant maintenance in pin/unpin/burn/rekey; delete `joined_pinned`; dedupe `start_node` pin restore; `create_folder_at_root` returns `{sidecarId, cid}`, removes empty-folder workaround; `create_folder_nested` takes `parent_sidecar_id` |
| `src/lib/types.ts` | `ContentItem.sidecarId: string` |
| `src/lib/file-manager-service.ts` | `ContentItemWire.sidecarId`; `wireToContentItem` maps it; pin/unpin/burn/archive/setReplicationTier invokes flip to sidecar_id; `createFolder` signature; `createFolder` return widens |
| `src/lib/components/FileBrowser.svelte` | `navStack` segments carry optional `sidecarId` (top-level root only); `{#each ... (item.sidecarId)}` keying; `handleNewFolder` passes parent_sidecar_id |

---

## Task 1: Add `uuid` dependency and `SidecarId` type

**Files:**
- Modify: `src-tauri/Cargo.toml` (add uuid dep)
- Modify: `src-tauri/src/content_index.rs` (add SidecarId at top of file, after `use` block)

- [ ] **Step 1: Add the failing tests first**

Add to the bottom of the existing `tests` module in `src-tauri/src/content_index.rs` (just before the closing `}`):

```rust
    #[test]
    fn sidecar_id_new_produces_unique_values() {
        let a = SidecarId::new();
        let b = SidecarId::new();
        assert_ne!(a, b, "two SidecarId::new() calls must produce distinct values");
    }

    #[test]
    fn sidecar_id_round_trips_through_display_and_parse() {
        let original = SidecarId::new();
        let s = original.to_string();
        let parsed = SidecarId::parse_str(&s).expect("must parse own Display output");
        assert_eq!(parsed, original);
    }

    #[test]
    fn sidecar_id_parse_str_rejects_garbage() {
        assert!(SidecarId::parse_str("").is_err());
        assert!(SidecarId::parse_str("not-a-uuid").is_err());
        assert!(SidecarId::parse_str("8b4f7c2e-1a3d-4f5b-9c0e-XXXXXXXXXXXX").is_err());
    }

    #[test]
    fn sidecar_id_serializes_as_hyphenated_string() {
        let id = SidecarId::new();
        let json = serde_json::to_string(&id).expect("serialize");
        // Hyphenated UUID is 38 chars wrapped in quotes: "<36 chars>"
        assert_eq!(json.len(), 38, "got {json}");
        assert!(json.starts_with('"') && json.ends_with('"'));
        // Round-trip via deserialization too.
        let back: SidecarId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo test --lib content_index::tests::sidecar_id`
Expected: compilation FAIL with "cannot find type `SidecarId` in this scope"

- [ ] **Step 3: Add the uuid dependency**

In `src-tauri/Cargo.toml`, find the `[dependencies]` block and add (alphabetically near other small crates):

```toml
uuid = { version = "1", features = ["v4", "serde"] }
```

- [ ] **Step 4: Implement `SidecarId`**

In `src-tauri/src/content_index.rs`, add at the top of the file directly under the existing `use` statements (line 17–20 area):

```rust
use uuid::Uuid;

/// Opaque per-entry stable identity for a sidecar row.
///
/// The sidecar key was `[u8; 32]` CID prior to ZEB-164, which forced one
/// entry per CID. With multiple user-visible entries (folders or otherwise)
/// allowed to share a CID — symlink-style — we need a stable identity that
/// is independent of content. UUID v4 is opaque (callers can't conflate
/// identity with content), survives restart, and is unique across devices
/// in case sidecars ever sync.
///
/// Tracing renders short-form (`uuid[..8]`) for log readability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SidecarId(Uuid);

impl SidecarId {
    /// Mint a fresh random SidecarId. Backend is the source of truth for
    /// minting; the frontend never generates these.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Parse the hyphenated lowercase Display form back into a SidecarId.
    /// Used at the IPC boundary when commands receive sidecar_id strings.
    pub fn parse_str(s: &str) -> Result<Self, uuid::Error> {
        Uuid::parse_str(s).map(Self)
    }
}

impl std::fmt::Display for SidecarId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hyphenated lowercase, e.g. "8b4f7c2e-1a3d-4f5b-9c0e-1234567890ab".
        write!(f, "{}", self.0.as_hyphenated())
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd src-tauri && cargo test --lib content_index::tests::sidecar_id`
Expected: 4 PASS.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/content_index.rs
git commit -m "$(cat <<'EOF'
feat(sidecar): add SidecarId UUID v4 type (ZEB-164)

Opaque per-entry identity, independent of content CID. Will become the
HashMap key for ContentIndex in the next task.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Re-key `ContentIndex` by `SidecarId`

This is the core mechanical refactor. `ContentIndexEntry` gains a `sidecar_id` field; the storage map flips from `HashMap<[u8;32], Entry>` to `HashMap<SidecarId, Entry>`; all CRUD methods take `SidecarId`. New helpers `entries_for_cid` and `is_cid_pinned_by_any` are added. `RekeyError::Collision` is removed (multiple entries can legally share a CID now).

**Files:**
- Modify: `src-tauri/src/content_index.rs`

- [ ] **Step 1: Update existing test scaffolding to mint a sidecar_id per entry**

In `src-tauri/src/content_index.rs`, replace the `sample_entry` helper inside the `tests` module (currently around line 339):

```rust
    fn sample_entry(cid: [u8; 32]) -> ContentIndexEntry {
        ContentIndexEntry {
            sidecar_id: SidecarId::new(),
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

(Note: `sidecar_id: SidecarId::new()` is the only added field.)

- [ ] **Step 2: Update existing tests to use sidecar_id keys**

Replace each existing test call that currently looks up by `&[0xAA; 32]` (or similar CID literal) with the entry's `sidecar_id`. Edit each impacted test:

`save_then_load_roundtrips_entries` (around line 362):

```rust
    #[test]
    fn save_then_load_roundtrips_entries() {
        let dir = tempdir().unwrap();
        let entry = sample_entry([0xAA; 32]);

        let mut idx = ContentIndex::load(dir.path());
        idx.entries.insert(entry.sidecar_id, entry.clone());
        idx.save();

        let reloaded = ContentIndex::load(dir.path());
        assert_eq!(reloaded.entries.len(), 1);
        assert_eq!(reloaded.entries.get(&entry.sidecar_id), Some(&entry));
    }
```

`insert_adds_entry_and_returns_true`:

```rust
    #[test]
    fn insert_adds_entry_and_returns_true() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xBB; 32]);
        let id = entry.sidecar_id;
        assert!(idx.insert(entry.clone()));
        assert_eq!(idx.get(&id), Some(&entry));
    }
```

`insert_duplicate_cid_returns_false` — RENAME and RESHAPE: it's now testing duplicate sidecar_id (effectively impossible), and the symmetric "two entries with same CID coexist" case is the positive test:

```rust
    #[test]
    fn insert_two_entries_with_same_cid_coexist() {
        // Pre-ZEB-164 this case was rejected via duplicate-CID guard. Now
        // each entry's identity is its sidecar_id, so two entries pointing
        // at the same CID are legal (symlink-style).
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());

        let mut alpha = sample_entry([0xCC; 32]);
        alpha.file_name = "Alpha".into();
        let mut beta = sample_entry([0xCC; 32]);
        beta.file_name = "Beta".into();

        assert_ne!(alpha.sidecar_id, beta.sidecar_id, "fresh ids must differ");
        assert!(idx.insert(alpha.clone()));
        assert!(idx.insert(beta.clone()));

        // Both entries are retrievable by their own sidecar_id.
        assert_eq!(idx.get(&alpha.sidecar_id).unwrap().file_name, "Alpha");
        assert_eq!(idx.get(&beta.sidecar_id).unwrap().file_name, "Beta");
    }

    #[test]
    fn insert_duplicate_sidecar_id_returns_false() {
        // Defense-in-depth: SidecarId collisions are practically impossible
        // for UUID v4, but the API still has to behave sensibly if a caller
        // re-uses one (e.g. tests cloning an entry verbatim).
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xCD; 32]);
        assert!(idx.insert(entry.clone()));
        assert!(!idx.insert(entry), "duplicate sidecar_id is rejected");
    }
```

`remove_returns_true_when_present_false_otherwise`:

```rust
    #[test]
    fn remove_returns_true_when_present_false_otherwise() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xDD; 32]);
        let id = entry.sidecar_id;
        idx.insert(entry);
        assert!(idx.remove(&id));
        assert!(!idx.remove(&id));
    }
```

`set_archived_flips_flag_and_reports_change`:

```rust
    #[test]
    fn set_archived_flips_flag_and_reports_change() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xEE; 32]);
        let id = entry.sidecar_id;
        idx.insert(entry);

        assert!(idx.set_archived(&id, true));  // flipped
        assert!(idx.get(&id).unwrap().archived);
        assert!(!idx.set_archived(&id, true)); // idempotent
    }
```

`set_archived_missing_cid_returns_false` — RENAME to `set_archived_missing_id_returns_false`:

```rust
    #[test]
    fn set_archived_missing_id_returns_false() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let bogus = SidecarId::new();
        assert!(!idx.set_archived(&bogus, true));
    }
```

`set_replication_tier_counts_updated_entries`:

```rust
    #[test]
    fn set_replication_tier_counts_updated_entries() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let a = sample_entry([0x01; 32]);
        let b = sample_entry([0x02; 32]);
        let a_id = a.sidecar_id;
        let b_id = b.sidecar_id;
        idx.insert(a);
        idx.insert(b);

        let updated = idx.set_replication_tier(&[a_id, b_id], ReplicationTier::Ultra);
        assert_eq!(updated, 2);

        let again = idx.set_replication_tier(&[a_id, b_id], ReplicationTier::Ultra);
        assert_eq!(again, 0);

        let bogus = SidecarId::new();
        let with_missing =
            idx.set_replication_tier(&[a_id, bogus], ReplicationTier::Expendable);
        assert_eq!(with_missing, 1);
    }
```

`save_persists_mutations`:

```rust
    #[test]
    fn save_persists_mutations() {
        let dir = tempdir().unwrap();
        let saved_id;
        {
            let mut idx = ContentIndex::load(dir.path());
            let a1 = sample_entry([0xA1; 32]);
            let a2 = sample_entry([0xA2; 32]);
            let a1_id = a1.sidecar_id;
            saved_id = a2.sidecar_id;
            idx.insert(a1);
            idx.insert(a2);
            idx.remove(&a1_id);
            assert!(idx.set_archived(&saved_id, true));
            assert_eq!(
                idx.set_replication_tier(&[saved_id], ReplicationTier::Ultra),
                1
            );
        }
        let reloaded = ContentIndex::load(dir.path());
        assert_eq!(reloaded.entries.len(), 1);
        let entry = reloaded.get(&saved_id).expect("saved entry persisted");
        assert!(entry.archived);
        assert_eq!(entry.replication_tier, ReplicationTier::Ultra);
    }
```

`set_pinned_flips_flag_and_reports_change`:

```rust
    #[test]
    fn set_pinned_flips_flag_and_reports_change() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xB1; 32]);
        let id = entry.sidecar_id;
        idx.insert(entry);

        assert!(idx.set_pinned(&id, true));
        assert!(idx.get(&id).unwrap().pinned);
        assert!(!idx.set_pinned(&id, true));
        assert!(idx.set_pinned(&id, false));
        assert!(!idx.get(&id).unwrap().pinned);
    }
```

`set_pinned_missing_cid_returns_false` — rename to `set_pinned_missing_id_returns_false`:

```rust
    #[test]
    fn set_pinned_missing_id_returns_false() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let bogus = SidecarId::new();
        assert!(!idx.set_pinned(&bogus, true));
    }
```

`save_persists_pin_mutations`:

```rust
    #[test]
    fn save_persists_pin_mutations() {
        let dir = tempdir().unwrap();
        let entry_id;
        {
            let mut idx = ContentIndex::load(dir.path());
            let entry = sample_entry([0xB3; 32]);
            entry_id = entry.sidecar_id;
            idx.insert(entry);
            assert!(idx.set_pinned(&entry_id, true));
        }
        let reloaded = ContentIndex::load(dir.path());
        assert!(
            reloaded.get(&entry_id).expect("persisted").pinned,
            "pinned flag must survive save/load"
        );
    }
```

`save_persists_kind_field`:

```rust
    #[test]
    fn save_persists_kind_field() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let mut entry = sample_entry([0xF0; 32]);
        entry.file_name = "Photos".into();
        entry.kind = ContentKind::Folder;
        let id = entry.sidecar_id;
        idx.insert(entry);

        let reloaded = ContentIndex::load(dir.path());
        let got = reloaded.get(&id).expect("round-trips");
        assert_eq!(got.kind, ContentKind::Folder);
        assert_eq!(got.file_name, "Photos");
    }
```

`rekey_atomically_replaces_cid_and_preserves_user_state`:

```rust
    #[test]
    fn rekey_atomically_replaces_cid_and_preserves_user_state() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());

        let mut entry = sample_entry([0x01; 32]);
        entry.file_name = "Folder".into();
        entry.kind = ContentKind::Folder;
        entry.pinned = true;
        let id = entry.sidecar_id;
        idx.insert(entry);

        let result = idx.rekey(&id, [0x02; 32], 999, 1234);
        assert!(result.is_ok());

        let after = idx.get(&id).expect("entry still present under same sidecar_id");
        assert_eq!(after.cid, [0x02; 32], "cid updated");
        assert_eq!(after.file_name, "Folder", "file_name carried forward");
        assert_eq!(after.kind, ContentKind::Folder, "kind carried forward");
        assert!(after.pinned, "pinned carried forward");
        assert_eq!(after.size_bytes, 999, "size_bytes updated");
        assert_eq!(after.stored_at_ms, 1234, "stored_at_ms updated");

        // Non-existent sidecar_id returns OldMissing.
        let bogus = SidecarId::new();
        assert_eq!(idx.rekey(&bogus, [0xEE; 32], 0, 0), Err(RekeyError::OldMissing));
    }
```

`rekey_refuses_collision_instead_of_overwriting` — DELETE this test entirely. The new behavior is "shared CIDs are legal" — covered by the new `rekey_target_cid_already_used_succeeds` test in step 3 below.

`rekey_old_equals_new_is_a_self_update_not_a_collision` — RENAME to `rekey_self_update_refreshes_size_and_stored_at` (the "not a collision" framing no longer applies):

```rust
    #[test]
    fn rekey_self_update_refreshes_size_and_stored_at() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xCC; 32]);
        let id = entry.sidecar_id;
        idx.insert(entry);

        let result = idx.rekey(&id, [0xCC; 32], 12345, 67890);
        assert!(result.is_ok());
        let after = idx.get(&id).expect("entry still present");
        assert_eq!(after.size_bytes, 12345);
        assert_eq!(after.stored_at_ms, 67890);
    }
```

`legacy_sidecar_without_pinned_field_loads_as_unpinned` — DELETE. This tested a v0→v1 migration that's no longer relevant; v1-without-sidecar_id files now fail deserialization entirely (covered by a new `load_v1_without_sidecar_id_returns_empty` test in step 3).

`kind_defaults_to_leaf_on_legacy_sidecar` — DELETE. Same reason: any v1 file lacking sidecar_id fails deserialization, regardless of which other fields it has.

- [ ] **Step 3: Add new tests for ZEB-164 behavior**

Append these to the `tests` module (after `sidecar_id_serializes_as_hyphenated_string` from Task 1):

```rust
    #[test]
    fn load_v1_without_sidecar_id_returns_empty() {
        // After ZEB-164, sidecar_id is a required field. v1 fixtures from
        // before this change fail deserialization → load() falls back to
        // empty (the existing malformed-JSON path). The two test sidecars
        // in dev are re-uploaded post-deploy.
        let dir = tempdir().unwrap();
        let pre_zeb164 = br#"{
            "version": 1,
            "entries": [{
                "cid": "aa11bb22cc33dd44ee55ff6677889900112233445566778899aabbccddeeff00",
                "file_name": "old.txt",
                "size_bytes": 42,
                "stored_at_ms": 1700000000000,
                "sensitivity": "private",
                "replication_tier": "default",
                "licensed": false,
                "archived": false,
                "pinned": false,
                "kind": "leaf"
            }]
        }"#;
        std::fs::write(dir.path().join(INDEX_FILE), pre_zeb164).unwrap();
        let idx = ContentIndex::load(dir.path());
        assert!(idx.entries.is_empty(), "legacy entry without sidecar_id must not load");
    }

    #[test]
    fn rekey_target_cid_already_used_succeeds() {
        // Pre-ZEB-164 this would have returned RekeyError::Collision. Now
        // multiple entries can share a CID — the rekey target collision
        // case is no longer an error.
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());

        let keeper = sample_entry([0xAA; 32]);
        let other = sample_entry([0xBB; 32]);
        let keeper_id = keeper.sidecar_id;
        let other_id = other.sidecar_id;
        idx.insert(keeper);
        idx.insert(other);

        // Rekey other from 0xBB → 0xAA. Both entries persist; both reference 0xAA.
        let result = idx.rekey(&other_id, [0xAA; 32], 0, 0);
        assert!(result.is_ok());

        assert_eq!(idx.get(&keeper_id).unwrap().cid, [0xAA; 32]);
        assert_eq!(idx.get(&other_id).unwrap().cid, [0xAA; 32]);
    }

    #[test]
    fn entries_for_cid_returns_all_matching() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());

        let a = sample_entry([0x10; 32]);
        let b = sample_entry([0x10; 32]); // shares CID with a
        let c = sample_entry([0x20; 32]); // different CID
        idx.insert(a.clone());
        idx.insert(b.clone());
        idx.insert(c.clone());

        let mut matched: Vec<SidecarId> = idx.entries_for_cid(&[0x10; 32])
            .map(|e| e.sidecar_id)
            .collect();
        matched.sort_by_key(|id| id.to_string());
        let mut expected = vec![a.sidecar_id, b.sidecar_id];
        expected.sort_by_key(|id| id.to_string());
        assert_eq!(matched, expected);

        let lone: Vec<SidecarId> = idx.entries_for_cid(&[0x20; 32])
            .map(|e| e.sidecar_id)
            .collect();
        assert_eq!(lone, vec![c.sidecar_id]);

        let none: Vec<SidecarId> = idx.entries_for_cid(&[0x99; 32])
            .map(|e| e.sidecar_id)
            .collect();
        assert!(none.is_empty());
    }

    #[test]
    fn is_cid_pinned_by_any_or_joins_entries() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());

        // Two entries share CID 0x30; neither pinned initially.
        let mut a = sample_entry([0x30; 32]);
        let mut b = sample_entry([0x30; 32]);
        let a_id = a.sidecar_id;
        let b_id = b.sidecar_id;
        a.pinned = false;
        b.pinned = false;
        idx.insert(a);
        idx.insert(b);

        assert!(!idx.is_cid_pinned_by_any(&[0x30; 32]), "neither pinned");

        idx.set_pinned(&a_id, true);
        assert!(idx.is_cid_pinned_by_any(&[0x30; 32]), "one pinned");

        idx.set_pinned(&b_id, true);
        assert!(idx.is_cid_pinned_by_any(&[0x30; 32]), "both pinned");

        idx.set_pinned(&a_id, false);
        assert!(idx.is_cid_pinned_by_any(&[0x30; 32]), "still one pinned");

        idx.set_pinned(&b_id, false);
        assert!(!idx.is_cid_pinned_by_any(&[0x30; 32]), "all unpinned");

        // Unknown CID returns false (not panic).
        assert!(!idx.is_cid_pinned_by_any(&[0x99; 32]));
    }
```

- [ ] **Step 4: Run tests to verify they fail**

Run: `cd src-tauri && cargo test --lib content_index`
Expected: compilation FAIL — references to `sidecar_id` field on `ContentIndexEntry`, `entries_for_cid` and `is_cid_pinned_by_any` methods on `ContentIndex`, `RekeyError` without `Collision`, etc.

- [ ] **Step 5: Implement the data-model changes**

In `src-tauri/src/content_index.rs`:

(a) Add the `sidecar_id` field to `ContentIndexEntry` (currently around line 60). Place `sidecar_id` first to match the on-disk convention "identity-then-content":

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
    /// ZEB-155: persisted pin intent. With ZEB-164's symlink model, the
    /// runtime cache's `pin_intent` derives from the OR of every sidecar
    /// entry's pinned flag for a given CID — see `is_cid_pinned_by_any`.
    /// Per-row UI uses this entry's flag directly; cross-entry computation
    /// happens on mutation paths (pin/unpin/burn/rekey) to keep the
    /// runtime invariant in sync.
    ///
    /// `#[serde(default)]` makes pre-ZEB-155 sidecars readable: legacy
    /// entries deserialize with pinned=false (correct — they weren't
    /// pinned at their last save, since the field didn't exist).
    #[serde(default)]
    pub pinned: bool,
    /// ZEB-158 slice 1: distinguishes leaf files from folder bundles at the
    /// sidecar level. Default `Leaf` with `#[serde(default)]` keeps pre-slice-1
    /// sidecars readable — legacy entries were all leaves by construction,
    /// because folders didn't exist before slice 1.
    #[serde(default)]
    pub kind: ContentKind,
}
```

(b) Change the storage map (line ~95):

```rust
pub struct ContentIndex {
    path: PathBuf,
    entries: HashMap<SidecarId, ContentIndexEntry>,
}
```

(c) Simplify `RekeyError` (line ~100):

```rust
/// Error returned by [`ContentIndex::rekey`].
///
/// `Collision` was retired in ZEB-164: multiple entries can legally
/// share a CID under the symlink-style sidecar model, so the rekey target
/// already being present is no longer an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RekeyError {
    /// The given `sidecar_id` doesn't refer to any known entry.
    OldMissing,
}
```

(d) Update `read_file` (line ~128) — entries now keyed by sidecar_id:

```rust
    fn read_file(path: &Path) -> Option<HashMap<SidecarId, ContentIndexEntry>> {
        let data = std::fs::read(path).ok()?;
        let file: IndexFile = serde_json::from_slice(&data).ok()?;
        if file.version != FILE_VERSION {
            return None;
        }
        let mut map = HashMap::with_capacity(file.entries.len());
        for entry in file.entries {
            if map.insert(entry.sidecar_id, entry).is_some() {
                tracing::warn!("duplicate sidecar_id in content-index.json; last-write-wins");
            }
        }
        Some(map)
    }
```

(e) Update `save` deterministic ordering (line ~164) — sort by sidecar_id (still deterministic, just keyed differently):

```rust
        let mut sorted: Vec<ContentIndexEntry> = self.entries.values().cloned().collect();
        sorted.sort_by_key(|e| e.sidecar_id.to_string());
```

(f) Replace the CRUD methods (lines ~196–315) with sidecar_id-keyed versions:

```rust
    /// Insert a new entry. Returns `true` if added, `false` if the
    /// `sidecar_id` was already present (no mutation in that case).
    pub fn insert(&mut self, entry: ContentIndexEntry) -> bool {
        if self.entries.contains_key(&entry.sidecar_id) {
            return false;
        }
        self.entries.insert(entry.sidecar_id, entry);
        self.save();
        true
    }

    /// Remove an entry by sidecar_id. Returns `true` if present before the call.
    pub fn remove(&mut self, id: &SidecarId) -> bool {
        let removed = self.entries.remove(id).is_some();
        if removed {
            self.save();
        }
        removed
    }

    /// Flip the `archived` flag. Returns `true` if the flag changed;
    /// `false` if already at the target state or the sidecar_id is unknown.
    pub fn set_archived(&mut self, id: &SidecarId, archived: bool) -> bool {
        let Some(entry) = self.entries.get_mut(id) else {
            return false;
        };
        if entry.archived == archived {
            return false;
        }
        entry.archived = archived;
        self.save();
        true
    }

    /// Atomically replace an entry's CID while preserving user-state
    /// (file_name, sensitivity, replication_tier, licensed, archived,
    /// pinned, kind). Used when a folder mutation produces a new top-level
    /// root CID (nested `create_folder`, future move/rename). One save()
    /// for the whole replacement.
    ///
    /// ZEB-164 retired `RekeyError::Collision`: under the symlink-style
    /// model, the new CID already being used by another entry is not an
    /// error — entries are identified by `sidecar_id`, not CID.
    pub fn rekey(
        &mut self,
        id: &SidecarId,
        new_cid: [u8; 32],
        new_size_bytes: u64,
        new_stored_at_ms: u64,
    ) -> Result<(), RekeyError> {
        let Some(entry) = self.entries.get_mut(id) else {
            return Err(RekeyError::OldMissing);
        };
        entry.cid = new_cid;
        entry.size_bytes = new_size_bytes;
        entry.stored_at_ms = new_stored_at_ms;
        self.save();
        Ok(())
    }

    /// Flip the `pinned` flag. Returns `true` if the flag changed;
    /// `false` if already at the target state or the sidecar_id is unknown.
    pub fn set_pinned(&mut self, id: &SidecarId, pinned: bool) -> bool {
        let Some(entry) = self.entries.get_mut(id) else {
            return false;
        };
        if entry.pinned == pinned {
            return false;
        }
        entry.pinned = pinned;
        self.save();
        true
    }

    /// Set replication tier on a batch of sidecar_ids. Returns the count
    /// of entries whose tier actually changed.
    pub fn set_replication_tier(
        &mut self,
        ids: &[SidecarId],
        tier: ReplicationTier,
    ) -> usize {
        let mut changed = 0;
        for id in ids {
            if let Some(entry) = self.entries.get_mut(id) {
                if entry.replication_tier != tier {
                    entry.replication_tier = tier;
                    changed += 1;
                }
            }
        }
        if changed > 0 {
            self.save();
        }
        changed
    }

    /// Look up a single entry by sidecar_id.
    pub fn get(&self, id: &SidecarId) -> Option<&ContentIndexEntry> {
        self.entries.get(id)
    }

    /// Iterate over every sidecar entry referencing this CID.
    ///
    /// With multiple entries allowed per CID (symlink-style), this is the
    /// natural shape for the OR-join logic that maintains the runtime
    /// `pin_intent` invariant. Linear scan; sidecar size is bounded by
    /// user library scale (hundreds to low thousands of entries) and
    /// scans run only on mutation paths, not on `list_content`.
    pub fn entries_for_cid(
        &self,
        cid: &[u8; 32],
    ) -> impl Iterator<Item = &ContentIndexEntry> {
        self.entries.values().filter(move |e| &e.cid == cid)
    }

    /// True iff some sidecar entry references this CID with `pinned == true`.
    /// Backs the runtime pin_intent invariant: runtime should hold this CID
    /// in `pin_intent` iff this returns true (assuming nothing else outside
    /// the sidecar pins it).
    pub fn is_cid_pinned_by_any(&self, cid: &[u8; 32]) -> bool {
        self.entries_for_cid(cid).any(|e| e.pinned)
    }

    /// Iterate over all entries. **Order is not guaranteed** (HashMap-backed).
    /// Callers that surface results to users must sort.
    pub fn entries(&self) -> impl Iterator<Item = &ContentIndexEntry> {
        self.entries.values()
    }
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cd src-tauri && cargo test --lib content_index`
Expected: all PASS. If any test still references CID-keyed lookups that should have been updated, fix it.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/content_index.rs
git commit -m "$(cat <<'EOF'
feat(sidecar): re-key ContentIndex by SidecarId (ZEB-164)

ContentIndexEntry gains a sidecar_id field; HashMap key flips from
[u8;32] CID to SidecarId; CRUD methods take SidecarId. Adds
entries_for_cid + is_cid_pinned_by_any helpers backing the runtime
pin_intent invariant. RekeyError::Collision retired — multiple entries
sharing a CID is now the design.

v1 sidecars without sidecar_id fail deserialization → load() returns
empty (existing malformed-JSON path). No migration code; the two test
sidecars in dev get re-uploaded.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Wire format and helper updates in `lib.rs`

Add `sidecarId` to `ContentItemWire`, introduce `parse_sidecar_id`, update `list_root` and `list_folder` to populate it, delete the now-trivial `joined_pinned` helper and retire its tests.

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Update the wire format failing test**

Replace the existing `content_item_wire_serializes_kind_field` test (around line 3168) with:

```rust
    #[test]
    fn content_item_wire_serializes_sidecar_id_and_kind() {
        let id = uuid::Uuid::new_v4().as_hyphenated().to_string();
        let wire = ContentItemWire {
            sidecar_id: id.clone(),
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
        assert!(json.contains(&format!("\"sidecarId\":\"{id}\"")), "got: {json}");
        assert!(json.contains("\"kind\":\"folder\""), "got: {json}");
    }
```

DELETE the existing `joined_pinned_*` tests (`joined_pinned_true_when_only_intent_is_set`,
`joined_pinned_true_when_only_runtime_effect_is_set`, `joined_pinned_true_when_both_agree`,
`joined_pinned_false_when_neither_says_so`). The helper goes away.

DELETE the `sidecar_entry` test helper at the top of `pin_persistence_tests` (around line 3152) since the only callers were the joined_pinned tests.

Also DELETE the unused `use std::collections::HashSet;` at the top of `pin_persistence_tests` — it backed the `runtime_pinned` arg in the joined_pinned tests.

ADD a new test for the `parse_sidecar_id` helper:

```rust
    #[test]
    fn parse_sidecar_id_accepts_hyphenated_uuid_rejects_garbage() {
        let id = uuid::Uuid::new_v4().as_hyphenated().to_string();
        assert!(parse_sidecar_id(&id).is_ok());
        assert!(parse_sidecar_id("").is_err(), "empty rejected");
        assert!(parse_sidecar_id("not-a-uuid").is_err(), "garbage rejected");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo test --lib pin_persistence_tests`
Expected: compilation FAIL — `sidecar_id` field on `ContentItemWire`, `parse_sidecar_id` not yet defined.

- [ ] **Step 3: Implement wire updates**

In `src-tauri/src/lib.rs`:

(a) Update `ContentItemWire` (line ~1170):

```rust
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentItemWire {
    /// ZEB-164: opaque per-entry stable handle. Empty string for
    /// manifest-derived rows (children inside a folder bundle that have
    /// no sidecar entry of their own). Frontend gates pin/burn/archive
    /// buttons on this being non-empty.
    pub sidecar_id: String,
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

(b) Add `parse_sidecar_id` near `parse_cid_hex` (line ~1208):

```rust
fn parse_sidecar_id(s: &str) -> Result<content_index::SidecarId, String> {
    if s.is_empty() {
        return Err("sidecar_id is empty (manifest-derived row?)".into());
    }
    content_index::SidecarId::parse_str(s)
        .map_err(|e| format!("invalid sidecar_id: {e}"))
}
```

(c) DELETE the `joined_pinned` helper (lines ~1227–1232).

(d) Update `list_root` (line ~1262) — populate sidecar_id, drop joined_pinned call:

```rust
pub(crate) fn list_root(
    state: tauri::State<'_, Mutex<NodeState>>,
    pinned_set: &std::collections::HashSet<[u8; 32]>,
) -> Result<Vec<ContentItemWire>, String> {
    // pinned_set is kept in the signature for interface stability with
    // list_folder (which still consults it for manifest-derived rows).
    // Top-level rows expose entry.pinned directly — every command that
    // touches pin state maintains the runtime pin_intent OR-join, so the
    // sidecar's own flag is the authoritative per-row signal.
    let _ = pinned_set;

    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let mut entries: Vec<ContentItemWire> = {
        let idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.entries()
            .map(|e| ContentItemWire {
                sidecar_id: e.sidecar_id.to_string(),
                cid: hex::encode(e.cid),
                name: e.file_name.clone(),
                size_bytes: e.size_bytes,
                stored_at: e.stored_at_ms,
                sensitivity: sensitivity_wire(e.sensitivity).to_string(),
                replication_tier: replication_tier_wire(e.replication_tier).to_string(),
                pinned: e.pinned,
                licensed: e.licensed,
                archived: e.archived,
                kind: kind_wire(e.kind).to_string(),
            })
            .collect()
    };
    entries.sort_by(|a, b| b.stored_at.cmp(&a.stored_at));
    Ok(entries)
}
```

(e) Update `list_folder` (line ~1338) — manifest rows emit empty sidecar_id, keep runtime OR-join for pin display since these aren't sidecar-backed:

```rust
    // Synthesize wire rows. Nested items have no sidecar: sidecar_id is
    // the empty-string sentinel ("frontend: no mutations apply"); size_bytes
    // /stored_at default to 0; sensitivity/replication_tier default;
    // licensed/archived false. For manifest-derived rows we DO consult the
    // runtime pinned set — those rows have no sidecar.pinned to read, and
    // a CID currently held in cache via some other entry's pin_intent is
    // the only signal of "this content is sticking around right now".
    Ok(manifest
        .folder_manifest
        .entries
        .into_iter()
        .map(|e| ContentItemWire {
            sidecar_id: String::new(),
            cid: hex::encode(e.cid),
            name: e.name,
            size_bytes: 0,
            stored_at: 0,
            sensitivity: "private".into(),
            replication_tier: "default".into(),
            pinned: pinned_set.contains(&e.cid),
            licensed: false,
            archived: false,
            kind: kind_wire(e.kind).to_string(),
        })
        .collect())
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo test --lib pin_persistence_tests`
Expected: 2 PASS (`content_item_wire_serializes_sidecar_id_and_kind`, `parse_sidecar_id_accepts_hyphenated_uuid_rejects_garbage`).

Run: `cd src-tauri && cargo build --lib`
Expected: build succeeds (the rest of `lib.rs` may still emit errors for Tauri commands using CID keys — that's Task 4's job; if `cargo build --lib` shows errors at this point, they should all be in the Tauri command sites we're about to fix).

If non-command sites (anything outside `pin_content`/`unpin_content`/`burn_content`/`archive_content`/`set_replication_tier`/`create_folder_*`) fail to compile, those need to be fixed in this task before moving on.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(wire): add sidecarId to ContentItemWire (ZEB-164)

list_root populates sidecar_id from each entry; list_folder emits ""
for manifest-derived rows. parse_sidecar_id helper at IPC boundary.
joined_pinned helper deleted — top-level rows use entry.pinned
directly; per-row runtime OR-join is no longer needed once mutation
paths maintain the pin_intent invariant.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Flip Tauri mutation commands to `sidecar_id`

`pin_content`, `unpin_content`, `archive_content`, `set_replication_tier` switch their wire parameter to `sidecar_id`/`sidecar_ids`. `burn_content` becomes the three-branch B-conservative variant. Each command maintains the runtime `pin_intent` OR-join invariant via `is_cid_pinned_by_any`.

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Implement `pin_content`**

Replace the `pin_content` command (line ~1357) with:

```rust
#[tauri::command]
async fn pin_content(
    sidecar_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let id = parse_sidecar_id(&sidecar_id)?;

    // ZEB-155 + ZEB-164: persist pin intent on the sidecar BEFORE the
    // runtime verb. After flipping the bit, look up the entry's CID so
    // we can dispatch Pin against it. The Pin verb is idempotent for
    // CIDs already in pin_intent (a sibling entry pinning the same CID
    // will have already added it).
    let (index, maybe_verb_tx) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        (guard.content_index.clone(), guard.content_verb_tx.clone())
    };
    let cid_bytes = {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.set_pinned(&id, true);
        idx.get(&id)
            .ok_or_else(|| "unknown sidecar_id".to_string())?
            .cid
    };

    let verb_tx = maybe_verb_tx.ok_or_else(|| "runtime unavailable".to_string())?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    verb_tx
        .send(event_loop::ContentVerbRequest::Pin {
            cid: cid_bytes,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;
    reply_rx
        .await
        .map_err(|_| "event loop dropped pin request".to_string())?
}
```

- [ ] **Step 2: Implement `unpin_content` (with OR-join check)**

Replace the `unpin_content` command (line ~1398):

```rust
#[tauri::command]
async fn unpin_content(
    sidecar_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let id = parse_sidecar_id(&sidecar_id)?;

    // ZEB-164: clear sidecar intent. Then check OR-join: if some other
    // sidecar entry STILL pins this CID, leave runtime pin_intent alone
    // (the bytes are still wanted). Only dispatch Unpin to the runtime
    // when no entry references this CID with pinned=true anymore.
    let (index, maybe_verb_tx) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        (guard.content_index.clone(), guard.content_verb_tx.clone())
    };
    let unpin_runtime_for: Option<[u8; 32]> = {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.set_pinned(&id, false);
        let cid = idx
            .get(&id)
            .ok_or_else(|| "unknown sidecar_id".to_string())?
            .cid;
        if idx.is_cid_pinned_by_any(&cid) {
            None
        } else {
            Some(cid)
        }
    };

    let Some(cid_bytes) = unpin_runtime_for else {
        return Ok(true); // sidecar updated; another entry still pins
    };

    let verb_tx = maybe_verb_tx.ok_or_else(|| "runtime unavailable".to_string())?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    verb_tx
        .send(event_loop::ContentVerbRequest::Unpin {
            cid: cid_bytes,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "event loop not running".to_string())?;
    reply_rx
        .await
        .map_err(|_| "event loop dropped unpin request".to_string())?
}
```

- [ ] **Step 3: Implement `burn_content` (three-branch B-conservative)**

Replace the `burn_content` command (line ~1438):

```rust
/// Burn a sidecar entry. With ZEB-164's symlink-style sidecar, burn is
/// "remove this entry from my list" — not "destroy the bytes everyone
/// shares." The runtime's `Burn` verb only fires when this entry was the
/// last reference to its CID. Otherwise we issue an `Unpin` (if the burn
/// drops the only pinning entry) or no runtime action (if siblings still
/// pin it).
#[tauri::command]
async fn burn_content(
    sidecar_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let id = parse_sidecar_id(&sidecar_id)?;

    let (index, verb_tx) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        let verb_tx = guard
            .content_verb_tx
            .clone()
            .ok_or_else(|| "runtime unavailable".to_string())?;
        (guard.content_index.clone(), verb_tx)
    };

    // Three-branch decision under a single lock acquisition: read entry's
    // CID, remove the entry, then inspect the post-state to decide which
    // (if any) runtime verb to dispatch.
    enum RuntimeAction {
        Burn([u8; 32]),
        Unpin([u8; 32]),
        Nothing,
    }
    let action = {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        let cid = match idx.get(&id) {
            Some(e) => e.cid,
            None => return Ok(false), // unknown sidecar_id; no-op
        };
        idx.remove(&id);
        if idx.entries_for_cid(&cid).next().is_none() {
            RuntimeAction::Burn(cid)
        } else if !idx.is_cid_pinned_by_any(&cid) {
            RuntimeAction::Unpin(cid)
        } else {
            RuntimeAction::Nothing
        }
    };

    match action {
        RuntimeAction::Burn(cid) => {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            verb_tx
                .send(event_loop::ContentVerbRequest::Burn {
                    cid,
                    reply: reply_tx,
                })
                .await
                .map_err(|_| "event loop not running".to_string())?;
            reply_rx
                .await
                .map_err(|_| "event loop dropped burn request".to_string())??;
        }
        RuntimeAction::Unpin(cid) => {
            // Sibling entries still reference this CID, but none pin it —
            // drop runtime pin_intent so W-TinyLFU can reclaim. Best-
            // effort: any failure here is a runtime/cache desync, not a
            // user-visible regression (the sidecar mutation already
            // committed). Log so the desync is diagnosable.
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            match verb_tx
                .send(event_loop::ContentVerbRequest::Unpin {
                    cid,
                    reply: reply_tx,
                })
                .await
            {
                Ok(()) => match reply_rx.await {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => tracing::warn!(
                        cid = %hex::encode(cid),
                        err = %e,
                        "burn_content: post-burn unpin failed; runtime may hold stale pin",
                    ),
                    Err(_) => tracing::warn!(
                        cid = %hex::encode(cid),
                        "burn_content: event loop dropped post-burn unpin reply",
                    ),
                },
                Err(_) => tracing::warn!(
                    cid = %hex::encode(cid),
                    "burn_content: event loop closed before post-burn unpin send",
                ),
            }
        }
        RuntimeAction::Nothing => {} // siblings still pin; runtime untouched
    }
    Ok(true)
}
```

- [ ] **Step 4: Implement `archive_content`**

Replace `archive_content` (line ~1480):

```rust
#[tauri::command]
async fn archive_content(
    sidecar_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<bool, String> {
    let id = parse_sidecar_id(&sidecar_id)?;
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let flipped = {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.set_archived(&id, true)
    };
    Ok(flipped)
}
```

- [ ] **Step 5: Implement `set_replication_tier`**

Replace `set_replication_tier` (line ~1497):

```rust
#[tauri::command]
async fn set_replication_tier(
    sidecar_ids: Vec<String>,
    tier: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<u32, String> {
    let parsed_tier = match tier.as_str() {
        "expendable" => content_index::ReplicationTier::Expendable,
        "light" => content_index::ReplicationTier::Light,
        "default" => content_index::ReplicationTier::Default,
        "high" => content_index::ReplicationTier::High,
        "ultra" => content_index::ReplicationTier::Ultra,
        other => return Err(format!("unknown replication tier: {other}")),
    };
    let mut parsed_ids: Vec<content_index::SidecarId> = Vec::with_capacity(sidecar_ids.len());
    for s in &sidecar_ids {
        parsed_ids.push(parse_sidecar_id(s)?);
    }
    let index = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.content_index.clone()
    };
    let updated = {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        idx.set_replication_tier(&parsed_ids, parsed_tier)
    };
    Ok(updated as u32)
}
```

- [ ] **Step 6: Run the build**

Run: `cd src-tauri && cargo build --lib`
Expected: build succeeds.

- [ ] **Step 7: Run the test suite**

Run: `cd src-tauri && cargo test --lib`
Expected: all PASS. The failures from earlier tasks should now be resolved; create_folder tests in lib.rs may still fail because Task 5 hasn't run yet — those will be addressed shortly.

If `create_folder` tests block this step, note them and proceed to Task 5; otherwise this step should be clean.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(commands): flip pin/unpin/burn/archive/replication to sidecar_id (ZEB-164)

pin_content + unpin_content + archive_content + set_replication_tier
take sidecar_id from the wire. burn_content becomes three-branch
B-conservative: full Burn only when this was the last entry for the
CID; Unpin when siblings remain but none still pin; no-op when siblings
still pin. unpin_content checks is_cid_pinned_by_any to maintain the
runtime pin_intent OR-join invariant.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Folder commands (`create_folder_at_root`, `create_folder_nested`)

`create_folder_at_root`: mint a sidecar_id, return `{ sidecarId, cid }`, drop the empty-folder collision workaround. `create_folder_nested`: take `parent_sidecar_id`, look up the top-level root's CID via the sidecar, post-rekey maintain the pin_intent OR-join invariant (recompute for both old and new CID).

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Add a result type for create_folder_at_root**

In `src-tauri/src/lib.rs`, near `IngestResult` (line ~1217):

```rust
/// Result returned by `create_folder` and `create_folder_at_root`. The
/// frontend stashes `sidecar_id` immediately so subsequent operations on
/// the just-created folder (pin, archive, future move/rename) have the
/// stable handle. `cid` is provided alongside for content-addressed reads.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateFolderResult {
    pub sidecar_id: String,
    pub cid: String,
}
```

- [ ] **Step 2: Update `create_folder` dispatch**

Replace the top-level `create_folder` Tauri command (line ~1752):

```rust
#[tauri::command]
async fn create_folder(
    name: String,
    parent_sidecar_id: Option<String>,
    parent_path: Vec<String>,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<CreateFolderResult, String> {
    // Defence-in-depth: the UI already trims and rejects blank names, but
    // the IPC surface is callable by anything with a Tauri handle. An empty
    // or whitespace-only label would produce folders that are hard to
    // distinguish in listings and breadcrumbs, so reject at the boundary.
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("folder name cannot be empty".to_string());
    }
    if parent_path.is_empty() {
        if parent_sidecar_id.is_some() {
            return Err("root creates must not provide parent_sidecar_id".into());
        }
        return create_folder_at_root(name, state).await;
    }
    let psid = parent_sidecar_id
        .ok_or_else(|| "nested creates require parent_sidecar_id".to_string())?;
    create_folder_nested(name, psid, parent_path, state).await
}
```

- [ ] **Step 3: Update `create_folder_at_root`**

Replace `create_folder_at_root` (line ~1772):

```rust
async fn create_folder_at_root(
    name: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<CreateFolderResult, String> {
    // Build the (empty) manifest + bundle locally. No runtime state
    // mutated yet — we can still bail cleanly on send_ingest failure.
    let built = folders::build_folder(&name, &[])?;

    let (ingest_tx, index) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        (
            guard
                .ingest_tx
                .clone()
                .ok_or_else(|| "not connected".to_string())?,
            guard.content_index.clone(),
        )
    };
    let bundle_size = built.bundle_bytes.len() as u64;
    let stored_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    // ZEB-164: every empty folder bundle has the same CID, but multiple
    // sidecar entries can now reference that shared CID — so the slice-1
    // collision workaround ("a folder with identical contents already
    // exists") is gone. We mint a fresh sidecar_id, reserve the slot
    // before publishing bytes, and roll back if either ingest fails.
    let sidecar_id = content_index::SidecarId::new();
    {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        let inserted = idx.insert(content_index::ContentIndexEntry {
            sidecar_id,
            cid: built.bundle_cid.to_bytes(),
            file_name: name,
            size_bytes: bundle_size,
            stored_at_ms,
            sensitivity: content_index::Sensitivity::Private,
            replication_tier: content_index::ReplicationTier::Default,
            licensed: false,
            archived: false,
            pinned: false,
            kind: content_index::ContentKind::Folder,
        });
        if !inserted {
            // Effectively impossible (UUID v4 collision); kept as a
            // sanity guard against future SidecarId construction bugs.
            return Err("sidecar_id collision".into());
        }
    }

    // Slot reserved — now publish the bytes. ZEB-155's fetch-completion
    // recovery hook is gated on ZEB-159, so an orphan sidecar entry
    // would be unrecoverable until the user manually burned it. Roll
    // back the reservation on any ingest failure so the sidecar never
    // points at bytes that don't exist.
    if let Err(e) = send_ingest(
        &ingest_tx,
        hex::encode(built.manifest_cid.to_bytes()),
        built.manifest_bytes,
    )
    .await
    {
        if let Ok(mut idx) = index.lock() {
            idx.remove(&sidecar_id);
        }
        return Err(e);
    }
    if let Err(e) = send_ingest(
        &ingest_tx,
        hex::encode(built.bundle_cid.to_bytes()),
        built.bundle_bytes,
    )
    .await
    {
        if let Ok(mut idx) = index.lock() {
            idx.remove(&sidecar_id);
        }
        return Err(e);
    }

    Ok(CreateFolderResult {
        sidecar_id: sidecar_id.to_string(),
        cid: hex::encode(built.bundle_cid.to_bytes()),
    })
}
```

- [ ] **Step 4: Update `create_folder_nested`**

Replace the `create_folder_nested` function (line ~1866). The function loses its `Collision` error branch and gains pin_intent OR-join recomputation post-rekey:

```rust
async fn create_folder_nested(
    name: String,
    parent_sidecar_id: String,
    parent_path: Vec<String>,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<CreateFolderResult, String> {
    use harmony_content::bundle::parse_bundle;

    let parent_id = parse_sidecar_id(&parent_sidecar_id)?;

    // Parse all path CIDs up-front; fail fast on malformed input.
    let path_cids: Vec<[u8; 32]> = parent_path
        .iter()
        .map(|h| parse_cid_hex(h))
        .collect::<Result<_, _>>()?;
    let root_old = *path_cids.first().expect("non-empty by guard above");
    let immediate_parent_cid = *path_cids.last().expect("non-empty");

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

    // Verify the caller's claim: parent_sidecar_id maps to root_old.
    {
        let idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        let entry = idx.get(&parent_id).ok_or_else(|| {
            "parent_sidecar_id not in sidecar".to_string()
        })?;
        if entry.cid != root_old {
            return Err(format!(
                "parent_sidecar_id refers to cid {} but parent_path[0] is {}",
                hex::encode(entry.cid),
                hex::encode(root_old),
            ));
        }
    }

    // 1. Build the new empty sub-folder LOCALLY. Defer all ingests so
    // that a downstream OldMissing during rekey doesn't leave orphan
    // bytes in the runtime cache (which could be announced over Zenoh
    // and waste capacity for content no sidecar entry will ever
    // reference).
    let new_child = folders::build_folder(&name, &[])?;
    let new_child_bundle_cid = new_child.bundle_cid;

    let mut pending_ingests: Vec<(String, Vec<u8>)> = Vec::new();
    pending_ingests.push((
        hex::encode(new_child.manifest_cid.to_bytes()),
        new_child.manifest_bytes,
    ));
    pending_ingests.push((
        hex::encode(new_child_bundle_cid.to_bytes()),
        new_child.bundle_bytes,
    ));

    // 2. Bottom-up walk: rebuild each ancestor LOCALLY (read-only verb
    // requests), accumulating into pending_ingests.
    let mut prev_old_cid = immediate_parent_cid;
    let mut prev_new_cid = new_child_bundle_cid.to_bytes();
    let mut last_bundle_size: u64 = pending_ingests
        .last()
        .map(|(_, b)| b.len() as u64)
        .unwrap_or(0);

    for (i, &anc_cid) in path_cids.iter().enumerate().rev() {
        let is_deepest = i == path_cids.len() - 1;

        let anc_bundle = read_cached_bytes(&verb_tx, anc_cid)
            .await?
            .ok_or_else(|| {
                format!(
                    "ancestor {} not in cache; cannot rebuild parent chain",
                    hex::encode(anc_cid)
                )
            })?;
        let anc_child_ids = parse_bundle(&anc_bundle)
            .map_err(|e| format!("malformed ancestor bundle: {e:?}"))?;
        let manifest_cid = anc_child_ids
            .first()
            .copied()
            .ok_or_else(|| "ancestor bundle has no children".to_string())?;
        let anc_children: Vec<[u8; 32]> =
            anc_child_ids.iter().map(|c| c.to_bytes()).collect();

        let manifest_bytes = read_cached_bytes(&verb_tx, manifest_cid.to_bytes())
            .await?
            .ok_or_else(|| "ancestor manifest not in cache".to_string())?;
        let mut manifest = folders::parse_manifest(&manifest_bytes)
            .map_err(|e| format!("ancestor {e}"))?;
        folders::validate_manifest_matches_bundle(&manifest, &anc_children)
            .map_err(|e| format!("ancestor {} {e}", hex::encode(anc_cid)))?;

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

        let rebuilt = folders::build_folder(
            "",
            &manifest.folder_manifest.entries,
        )?;
        let rebuilt_bundle_cid = rebuilt.bundle_cid;
        last_bundle_size = rebuilt.bundle_bytes.len() as u64;
        pending_ingests.push((
            hex::encode(rebuilt.manifest_cid.to_bytes()),
            rebuilt.manifest_bytes,
        ));
        pending_ingests.push((
            hex::encode(rebuilt_bundle_cid.to_bytes()),
            rebuilt.bundle_bytes,
        ));

        prev_old_cid = anc_cid;
        prev_new_cid = rebuilt_bundle_cid.to_bytes();
    }

    // 3. Rekey the top-level sidecar entry FIRST. With ZEB-164 the
    // CID-collision branch is gone — multiple entries sharing a CID is
    // legal. Only OldMissing remains.
    let new_bundle_size = last_bundle_size;
    let stored_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    {
        let mut idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        match idx.rekey(&parent_id, prev_new_cid, new_bundle_size, stored_at_ms) {
            Ok(()) => {}
            Err(content_index::RekeyError::OldMissing) => {
                return Err(
                    "parent_sidecar_id removed mid-flight — nothing to rekey".to_string(),
                );
            }
        }
    }

    // 4. Drain the deferred ingests now that the sidecar is committed.
    // ZEB-167 tracks proper rekey-rollback for ingest failures.
    for (cid_hex, bytes) in pending_ingests {
        send_ingest(&ingest_tx, cid_hex, bytes).await?;
    }

    // 5. Maintain the runtime pin_intent OR-join invariant for both
    // old and new CIDs. If no remaining entry pins root_old, drop it
    // from runtime pin_intent. If any entry pins prev_new_cid (this
    // entry might, depending on its persisted intent), add it.
    //
    // Both dispatches are best-effort: the sidecar has already
    // committed the rekey, so any failure here is a runtime/cache
    // desync rather than a user-visible regression. Log so the desync
    // is diagnosable. The fetch-completion hook (ZEB-155 + ZEB-159)
    // re-converges on the next fetch of the new root.
    let (drop_old, add_new) = {
        let idx = index.lock().map_err(|e| format!("index lock: {e}"))?;
        (
            !idx.is_cid_pinned_by_any(&root_old),
            idx.is_cid_pinned_by_any(&prev_new_cid),
        )
    };
    if drop_old {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        match verb_tx
            .send(event_loop::ContentVerbRequest::Unpin {
                cid: root_old,
                reply: reply_tx,
            })
            .await
        {
            Ok(()) => match reply_rx.await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!(
                    old_cid = %hex::encode(root_old),
                    err = %e,
                    "create_folder_nested: runtime unpin of old root failed; cache may hold stale pin",
                ),
                Err(_) => tracing::warn!(
                    old_cid = %hex::encode(root_old),
                    "create_folder_nested: event loop dropped unpin reply",
                ),
            },
            Err(_) => tracing::warn!(
                old_cid = %hex::encode(root_old),
                "create_folder_nested: event loop closed before unpin send; cache may hold stale pin",
            ),
        }
    }
    if add_new {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        match verb_tx
            .send(event_loop::ContentVerbRequest::Pin {
                cid: prev_new_cid,
                reply: reply_tx,
            })
            .await
        {
            Ok(()) => match reply_rx.await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!(
                    new_cid = %hex::encode(prev_new_cid),
                    err = %e,
                    "create_folder_nested: runtime pin of new root failed; sidecar pin intent will repin on next fetch",
                ),
                Err(_) => tracing::warn!(
                    new_cid = %hex::encode(prev_new_cid),
                    "create_folder_nested: event loop dropped pin reply",
                ),
            },
            Err(_) => tracing::warn!(
                new_cid = %hex::encode(prev_new_cid),
                "create_folder_nested: event loop closed before pin send; sidecar pin intent will repin on next fetch",
            ),
        }
    }

    Ok(CreateFolderResult {
        sidecar_id: parent_sidecar_id, // unchanged — same identity, new cid
        cid: hex::encode(prev_new_cid),
    })
}
```

- [ ] **Step 5: Run the build and tests**

Run: `cd src-tauri && cargo build --lib && cargo test --lib`
Expected: all PASS.

If existing folder integration tests in `lib.rs` reference the old `create_folder` return type (a `String`), they need updates to match `CreateFolderResult`. Apply targeted edits to those tests until the suite is green.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(folders): create_folder commands take/return sidecar_id (ZEB-164)

create_folder_at_root: mints a fresh SidecarId, returns
CreateFolderResult { sidecarId, cid }, drops the slice-1 empty-folder
collision workaround. create_folder_nested: takes parent_sidecar_id,
verifies it matches parent_path[0], post-rekey maintains the
pin_intent OR-join invariant for both old and new CIDs.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Dedupe pin_intent restoration on `start_node`

With multiple entries possibly sharing a CID, the existing pin restoration loop iterates entries and dispatches Pin per-entry — which can issue duplicate Pins for the same CID. Switch to a HashSet-backed unique-CID dispatch.

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Update the restoration block**

Replace the pin_intent computation block in `start_node` (line ~547):

```rust
        let pin_intent: std::collections::HashSet<[u8; 32]> = {
            let idx = content_index
                .lock()
                .map_err(|e| format!("content_index lock on startup: {e}"))?;
            // ZEB-164: multiple sidecar entries can pin the same CID. The
            // runtime pin_intent set is CID-keyed, so we dedupe here —
            // collecting into a HashSet drops duplicates without effect.
            // (Functionally identical to the pre-ZEB-164 path; the dedupe
            // is just made explicit so debug logs don't show repeated
            // restores for the same CID.)
            idx.entries()
                .filter(|e| e.pinned)
                .map(|e| e.cid)
                .collect()
        };
```

(The change is purely a comment addition; `collect::<HashSet<_>>()` already dedupes by construction. The annotation makes the intent explicit so future readers don't assume entries map 1:1 to runtime pins.)

- [ ] **Step 2: Run the build**

Run: `cd src-tauri && cargo build --lib && cargo test --lib`
Expected: all PASS.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
docs(pin): annotate dedupe intent in start_node restore (ZEB-164)

HashSet-collected pin_intent already dedupes by construction; comment
makes the intent explicit now that multiple entries can share a CID.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Frontend types and service

`ContentItem` adds `sidecarId`. `ContentItemWire` (TypeScript mirror) adds `sidecarId`. `wireToContentItem` maps it. All `invoke` calls flip from `cid` to `sidecarId`. `createFolder` signature widens to take `parentSidecarId` and return `{ sidecarId, cid }`.

**Files:**
- Modify: `src/lib/types.ts`
- Modify: `src/lib/file-manager-service.ts`

- [ ] **Step 1: Add `sidecarId` to `ContentItem` type**

In `src/lib/types.ts` at line 186:

```typescript
export interface ContentItem {
  /**
   * ZEB-164: opaque per-entry stable identity. Empty string for
   * manifest-derived rows (children of a folder bundle that have no
   * sidecar entry of their own). Backend mutations (pin, archive, burn,
   * setReplicationTier) take sidecarId, not cid.
   */
  sidecarId: string;
  cid: string;
  name: string;
  category: ContentCategory;
  sensitivity: ContentSensitivity;
  sizeBytes: number;
  storedAt: number;
  lastAccessed: number;
  accessCount: number;
  stalenessScore: number;
  replicationTier: ReplicationTier;
  replicaCount: number;
  pinned: boolean;
  licensed: boolean;
  archived?: boolean;
  parentCid: string | null;
  isFolder: boolean;
}
```

- [ ] **Step 2: Update `ContentItemWire` and `wireToContentItem`**

In `src/lib/file-manager-service.ts` at line 36:

```typescript
/** Wire format for entries returned by the list_content Tauri command. */
interface ContentItemWire {
  sidecarId: string;
  cid: string;
  name: string;
  sizeBytes: number;
  storedAt: number;
  sensitivity: 'private' | 'confidential' | 'public';
  replicationTier: ReplicationTier;
  pinned: boolean;
  licensed: boolean;
  archived: boolean;
  /** Source-of-truth node type from the backend. */
  kind: 'leaf' | 'folder';
}
```

And update `wireToContentItem` (line 67):

```typescript
function wireToContentItem(wire: ContentItemWire): ContentItem {
  return {
    sidecarId: wire.sidecarId,
    cid: wire.cid,
    name: wire.name,
    category: inferCategory(wire.name),
    sensitivity: wire.sensitivity,
    sizeBytes: wire.sizeBytes,
    storedAt: wire.storedAt,
    lastAccessed: wire.storedAt,
    accessCount: 0,
    stalenessScore: 0,
    replicationTier: wire.replicationTier,
    replicaCount: 1,
    pinned: wire.pinned,
    licensed: wire.licensed,
    archived: wire.archived,
    parentCid: null,
    isFolder: wire.kind === 'folder',
  };
}
```

- [ ] **Step 3: Add `sidecarId` to the offline ingest path**

In `file-manager-service.ts` at line 343 (the `ingest` method that constructs a `ContentItem` locally), add `sidecarId: ''` so the offline path keeps the type consistent. Real ingests from the backend will be picked up via `refetchRoot` on subsequent list_content calls (no service-side mutation here is needed because `ingest_content` is unchanged):

```typescript
    const item: ContentItem = {
      sidecarId: '',
      cid: result.cid,
      name: result.fileName,
      category: inferCategory(result.fileName),
      sensitivity: 'private',
      sizeBytes: result.sizeBytes,
      storedAt: Date.now(),
      lastAccessed: Date.now(),
      accessCount: 0,
      stalenessScore: 0,
      replicationTier: this.settings.defaultReplicationTier,
      replicaCount: 1,
      pinned: false,
      licensed: false,
      archived: false,
      parentCid: parentCid ?? null,
      isFolder: false,
    };
```

- [ ] **Step 4: Update `burn` to take sidecarIds**

Replace `burn` (line 225):

```typescript
  /** Permanently removes content items and frees their quota. */
  async burn(sidecarIds: string[]): Promise<void> {
    if (!this.adapter) {
      // Offline-only path: still mutate local state so tests/Storybook work.
      const idSet = new Set(sidecarIds);
      this.privateContent = this.privateContent.filter((i) => !idSet.has(i.sidecarId));
      this.onChange?.();
      return;
    }
    const results = await Promise.allSettled(
      sidecarIds.map((sidecarId) => this.adapter!.invoke('burn_content', { sidecarId })),
    );
    const succeeded = new Set(
      sidecarIds.filter((_, i) => {
        const r = results[i];
        return r.status === 'fulfilled' && r.value === true;
      }),
    );
    this.privateContent = this.privateContent.filter((i) => !succeeded.has(i.sidecarId));
    this.onChange?.();
  }
```

- [ ] **Step 5: Update `archive` to take sidecarIds**

Replace `archive` (line 248):

```typescript
  async archive(sidecarIds: string[]): Promise<void> {
    if (!this.adapter) {
      const idSet = new Set(sidecarIds);
      this.privateContent = this.privateContent.filter((i) => !idSet.has(i.sidecarId));
      this.onChange?.();
      return;
    }
    const results = await Promise.allSettled(
      sidecarIds.map((sidecarId) => this.adapter!.invoke('archive_content', { sidecarId })),
    );
    const succeeded = new Set(
      sidecarIds.filter((_, i) => {
        const r = results[i];
        return r.status === 'fulfilled' && r.value === true;
      }),
    );
    this.privateContent = this.privateContent.filter((i) => !succeeded.has(i.sidecarId));
    this.onChange?.();
  }
```

- [ ] **Step 6: Update `pin` and `unpin`**

Replace both (lines 279, 297):

```typescript
  /** Sets the pinned flag on a content item. */
  async pin(sidecarId: string): Promise<void> {
    if (!this.adapter) {
      const item = this.privateContent.find((i) => i.sidecarId === sidecarId);
      if (item) item.pinned = true;
      this.onChange?.();
      return;
    }
    const ok = (await this.adapter.invoke('pin_content', { sidecarId })) as boolean;
    if (ok === false) {
      throw new Error('pin quota exhausted');
    }
    const item = this.privateContent.find((i) => i.sidecarId === sidecarId);
    if (item) item.pinned = true;
    this.onChange?.();
  }

  /** Clears the pinned flag on a content item. */
  async unpin(sidecarId: string): Promise<void> {
    if (!this.adapter) {
      const item = this.privateContent.find((i) => i.sidecarId === sidecarId);
      if (item) item.pinned = false;
      this.onChange?.();
      return;
    }
    await this.adapter.invoke('unpin_content', { sidecarId });
    const item = this.privateContent.find((i) => i.sidecarId === sidecarId);
    if (item) item.pinned = false;
    this.onChange?.();
  }
```

- [ ] **Step 7: Update `setReplicationTier`**

Replace `setReplicationTier` (line 311):

```typescript
  async setReplicationTier(sidecarIds: string[], tier: ReplicationTier): Promise<void> {
    if (this.adapter) {
      await this.adapter.invoke('set_replication_tier', { sidecarIds, tier });
    }
    const idSet = new Set(sidecarIds);
    for (const item of this.privateContent) {
      if (idSet.has(item.sidecarId)) {
        item.replicationTier = tier;
      }
    }
    this.onChange?.();
  }
```

- [ ] **Step 8: Update `createFolder` signature and return type**

Replace `createFolder` (line 402):

```typescript
  /** Wire format for the create_folder Tauri command result. */
  // (declared near top of file; see below)
  /**
   * Create a new folder via the backend.
   *
   * @param name              folder display name
   * @param parentSidecarId   the top-level sidecar entry's id (root entry
   *                          owning the cascade), or null for root creation
   * @param parentPath        CID chain from top-level root (inclusive) down
   *                          to the immediate parent; empty for root creation
   *
   * Returns `{ sidecarId, cid }`. For nested creation, `sidecarId` is the
   * unchanged top-level entry's id; `cid` is the new top-level root CID
   * after the ancestor cascade.
   *
   * Refetches the root listing and emits onChange. Callers navigating
   * inside a folder at the time of creation should also refetch the
   * folder contents.
   */
  async createFolder(
    name: string,
    parentSidecarId: string | null,
    parentPath: string[],
  ): Promise<CreateFolderResult> {
    if (!this.adapter) throw new Error('adapter not connected');
    const result = (await this.adapter.invoke('create_folder', {
      name,
      parentSidecarId,
      parentPath,
    })) as CreateFolderResult;
    try {
      await this.refetchRoot();
    } catch (err) {
      console.warn(
        'createFolder: refetchRoot failed (folder was created); UI may show stale list:',
        err,
      );
    }
    return result;
  }
```

And add at the top of the file, just below `IngestResult` (line 33):

```typescript
/** Wire format returned by the create_folder Tauri command. */
export interface CreateFolderResult {
  sidecarId: string;
  cid: string;
}
```

- [ ] **Step 9: Run the type check**

Run: `npx tsc --noEmit -p .`
Expected: PASS. If any other call site references the old `cid` parameter on these methods (e.g. `getContentDetail(cid)`), update them — `getContentDetail` lookup logic stays CID-keyed since it's looking up by content; only mutation paths flip to sidecar_id.

`getContentDetail(cid)` lookup is unchanged — it consults privateContent by CID for display purposes (multiple sidecar entries sharing a CID would display the same detail; that's a v1 acceptable behavior per the spec's "no UI distinction").

- [ ] **Step 10: Commit**

```bash
git add src/lib/types.ts src/lib/file-manager-service.ts
git commit -m "$(cat <<'EOF'
feat(frontend): ContentItem.sidecarId; service flips mutations to sidecar_id (ZEB-164)

types.ts: ContentItem and the wire mirror gain sidecarId.
file-manager-service.ts: pin/unpin/burn/archive/setReplicationTier
take sidecarIds; createFolder takes (name, parentSidecarId, parentPath)
and returns { sidecarId, cid }.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: FileBrowser navigation and folder-creation wiring

`navStack` segments carry an optional `sidecarId` (only meaningful at index 0, the top-level root). `{#each ... (item.sidecarId || item.cid)}` keying. `handleNewFolder` extracts the top-level root's sidecarId from `navStack[0]` and passes it.

**Files:**
- Modify: `src/lib/components/FileBrowser.svelte`

- [ ] **Step 1: Widen `navStack` segment shape**

In `src/lib/components/FileBrowser.svelte` at line 77, change the navStack type:

```typescript
  // Explicit navigation stack from root sentinel down to the current
  // folder. Each segment carries cid (for content-addressed bundle
  // lookups during refetch and breadcrumb display) and an optional
  // sidecarId — only present on the FIRST segment after the sentinel,
  // which is the top-level sidecar entry. Nested segments (manifest-
  // derived rows below the top-level root) have no sidecar of their own.
  let navStack = $state<Array<{ cid: string; name: string; sidecarId?: string }>>([]);
  let pendingNav = $state<{ cid: string; name: string; sidecarId?: string } | null>(null);
```

- [ ] **Step 2: Update click-time stash to capture sidecarId**

In the same file, in `handleItemClick` (line 258):

```typescript
  function handleItemClick(cid: string) {
    const item = items.find((i) => i.cid === cid);
    if (item?.isFolder) {
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
    onItemClick(cid);
  }
```

- [ ] **Step 3: Update programmatic navigation lookup**

In the `$effect` that syncs navStack with currentFolderCid (line 84), the programmatic-jump branch already constructs a segment from items. Update it to carry sidecarId:

```typescript
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
      // Forward navigation: prefer the segment stashed at click time.
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
```

- [ ] **Step 4: Update `handleNewFolder` to pass parent_sidecar_id**

Replace `handleNewFolder` (line 269):

```typescript
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
        'Could not create folder: missing top-level folder identity. Try navigating to root and back, then retry.',
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
```

- [ ] **Step 5: Update `{#each}` keys for items**

The `FileList` and `FileGrid` components currently key on `item.cid`. With shared CIDs allowed, two top-level rows could collide on key. Update both to key on `(item.sidecarId || item.cid)` — the cid fallback covers manifest-derived rows (sidecarId === '') where collision can't happen within a single folder by construction (bundle child lists dedupe).

In `src/lib/components/FileList.svelte` line 26:

```svelte
  {#each items as item (item.sidecarId || item.cid)}
```

In `src/lib/components/FileGrid.svelte` line 17:

```svelte
  {#each items as item (item.sidecarId || item.cid)}
```

(Do **not** modify `PublishedView.svelte` — its `{#each}` operates on `publishedItems`, which is a separate list outside the ZEB-164 sidecar refactor.)

- [ ] **Step 6: Run the dev server and smoke test**

Run: `npm run tauri dev` (background)

Smoke checklist:
1. Click "New Folder" at root, name it "Photos" → row appears.
2. Click "New Folder" at root, name it "Documents" → second row appears (this would have failed pre-ZEB-164).
3. Pin "Photos" → row lights up; "Documents" stays unpinned.
4. Click into "Photos", click "New Folder" inside → cascades up to root; user lands back at root with refreshed list; the just-created nested chain appears.
5. Burn "Photos" → "Documents" remains intact; the empty-folder bytes stay in cache (until eventual eviction) because "Documents" still references that CID.

If any smoke step regresses, halt and fix before proceeding.

- [ ] **Step 7: Commit**

```bash
git add src/lib/components/FileBrowser.svelte src/lib/components/FileList.svelte src/lib/components/FileGrid.svelte
git commit -m "$(cat <<'EOF'
feat(ui): FileBrowser passes parent_sidecar_id; key rows by sidecar_id (ZEB-164)

navStack segments carry optional sidecarId (set at index 0 only —
the top-level root's sidecar entry). handleNewFolder extracts that and
passes it to service.createFolder. Item keys flip to sidecarId so two
rows sharing a CID render as distinct elements.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Final verification

- [ ] **Step 1: Full test suite (Rust)**

Run: `cd src-tauri && cargo test --lib`
Expected: all PASS.

- [ ] **Step 2: Full type check (frontend)**

Run: `npx tsc --noEmit -p .`
Expected: PASS.

- [ ] **Step 3: End-to-end smoke**

Run: `npm run tauri dev`

Verify:
1. Two empty folders coexist at root.
2. Pin one, the other's pin indicator stays off.
3. Burn one, the other persists; pinning state on the surviving entry is unaffected.
4. Add content to a shared-CID empty folder via "New Folder" inside it → only that entry rekeys; the other still references the (unchanged) empty-folder CID.

If any step regresses, fix before opening the PR.

- [ ] **Step 4: Open PR**

When all checks pass, push the branch and open a PR titled:

> `feat(sidecar): symlink-style sidecar entries via opaque SidecarId (ZEB-164)`
