//! Client-side sidecar for self-ingested content metadata (ZEB-146).
//!
//! Persists a map of `cid -> ContentIndexEntry` as JSON under
//! `app_data_dir/content-index.json` so the File Manager UI can surface
//! filenames, ingest timestamps, and user-set flags (sensitivity,
//! replication tier, licensed, archived) for content that the runtime's
//! RAM-only cache doesn't know about.
//!
//! Authority split:
//! - Sidecar is authoritative for membership, size_bytes, and pin *intent*
//!   (CIDs are immutable, so size never drifts from the ingest-time value;
//!   intent is what the user asked for and must survive restart).
//! - Runtime cache is authoritative for pin *effect* — the active eviction
//!   protection. `list_content` OR-joins the two sources at display time;
//!   see `ContentIndexEntry.pinned` for the full shape.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
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

const INDEX_FILE: &str = "content-index.json";
const FILE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sensitivity {
    Private,
    Confidential,
    Public,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplicationTier {
    Expendable,
    Light,
    Default,
    High,
    Ultra,
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentIndexEntry {
    #[serde(with = "hex_cid")]
    pub cid: [u8; 32],
    pub file_name: String,
    pub size_bytes: u64,
    pub stored_at_ms: u64,
    pub sensitivity: Sensitivity,
    pub replication_tier: ReplicationTier,
    pub licensed: bool,
    pub archived: bool,
    /// ZEB-155: persisted pin intent. True when the user has asked for
    /// this content to remain pinned across restarts. The runtime cache's
    /// `PinnedSet` is still authoritative for active eviction protection —
    /// this field is "the user wants this pinned whenever bytes are
    /// resident," joined with the runtime set at list_content time.
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

#[derive(Debug, Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    entries: Vec<ContentIndexEntry>,
}

pub struct ContentIndex {
    path: PathBuf,
    entries: HashMap<[u8; 32], ContentIndexEntry>,
}

/// Errors returned by [`ContentIndex::rekey`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RekeyError {
    /// The `old` CID wasn't in the index — nothing to rekey.
    OldMissing,
    /// The `new` CID is already present (and differs from `old`); the
    /// rekey would overwrite an unrelated entry. Caller should surface
    /// a "identical contents already exists" message.
    Collision,
}

impl ContentIndex {
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(INDEX_FILE);
        // Symmetric with save(): if data_dir is empty, `path` is the bare
        // filename "content-index.json" and would resolve to CWD. Don't read
        // a stray CWD sidecar into the default/uninitialised state.
        let path_is_bare = path
            .parent()
            .map_or(true, |p| p.as_os_str().is_empty());
        let entries = if path_is_bare {
            HashMap::new()
        } else {
            Self::read_file(&path).unwrap_or_default()
        };
        ContentIndex { path, entries }
    }

    fn read_file(path: &Path) -> Option<HashMap<[u8; 32], ContentIndexEntry>> {
        let data = std::fs::read(path).ok()?;
        let file: IndexFile = serde_json::from_slice(&data).ok()?;
        if file.version != FILE_VERSION {
            return None;
        }
        let mut map = HashMap::with_capacity(file.entries.len());
        for entry in file.entries {
            if map.insert(entry.cid, entry).is_some() {
                tracing::warn!("duplicate CID in content-index.json; last-write-wins");
            }
        }
        Some(map)
    }

    fn save(&self) {
        // Guard against the default/uninitialised state: NodeState::default()
        // constructs a ContentIndex with an empty data_dir before start_node
        // loads the real one. In that state `self.path` resolves to the
        // bare filename "content-index.json", which would land in the
        // process's current working directory. A properly-initialised path
        // always has a non-empty parent; we use that as the liveness check.
        let path_is_bare = self
            .path
            .parent()
            .map_or(true, |p| p.as_os_str().is_empty());
        if path_is_bare {
            tracing::warn!(
                "content-index save called before start_node initialised the sidecar path; \
                 dropping write (mutation lost)"
            );
            return;
        }
        // Sort by CID for deterministic on-disk ordering; HashMap iteration
        // order would otherwise churn the file on every save and make diffs
        // (and future snapshotting) noisy.
        let mut sorted: Vec<ContentIndexEntry> = self.entries.values().cloned().collect();
        sorted.sort_by_key(|e| e.cid);
        let file = IndexFile {
            version: FILE_VERSION,
            entries: sorted,
        };
        let json = match serde_json::to_vec_pretty(&file) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(err = %e, "content-index serialize failed; changes not persisted");
                return;
            }
        };

        let tmp_path = {
            let mut name = self.path.file_name().unwrap_or_default().to_os_string();
            name.push(".tmp");
            self.path.with_file_name(name)
        };
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&tmp_path, &json) {
            tracing::warn!(err = %e, "content-index write failed; changes not persisted");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp_path, &self.path) {
            tracing::warn!(err = %e, "content-index rename failed; tmp file may be stale");
        }
    }

    /// Insert a new entry. Returns `true` if added, `false` if the CID
    /// was already present (no mutation in that case). Callers that want
    /// overwrite semantics should remove first.
    pub fn insert(&mut self, entry: ContentIndexEntry) -> bool {
        if self.entries.contains_key(&entry.cid) {
            return false;
        }
        self.entries.insert(entry.cid, entry);
        self.save();
        true
    }

    /// Remove an entry by CID. Returns `true` if present before the call.
    pub fn remove(&mut self, cid: &[u8; 32]) -> bool {
        let removed = self.entries.remove(cid).is_some();
        if removed {
            self.save();
        }
        removed
    }

    /// Flip the `archived` flag. Returns `true` if the flag changed;
    /// `false` if already at the target state or the CID is unknown.
    pub fn set_archived(&mut self, cid: &[u8; 32], archived: bool) -> bool {
        let Some(entry) = self.entries.get_mut(cid) else {
            return false;
        };
        if entry.archived == archived {
            return false;
        }
        entry.archived = archived;
        self.save();
        true
    }

    /// ZEB-158 slice 1: atomically replace an entry's CID while
    /// preserving user-state fields (file_name, sensitivity,
    /// replication_tier, licensed, archived, pinned, kind). Used when a
    /// folder mutation produces a new top-level root CID (nested
    /// `create_folder`, future move/rename operations). One save() for
    /// the whole replacement — remove-then-insert would give two.
    ///
    /// Refuses on collision: if `new` is already present in the index
    /// (and `new != *old`), the call is a no-op and returns
    /// `Err(RekeyError::Collision)`. Without this guard, the inner
    /// `HashMap::insert` would silently overwrite the existing entry
    /// under `new`, dropping a different user-visible row. The collision
    /// happens for real under content-addressing: nested folder
    /// mutations can produce a CID that already names another sidecar
    /// root (two distinct paths converging on identical bundle
    /// contents). Symlink-style multiple-entries-per-CID is tracked in
    /// ZEB-164.
    pub fn rekey(
        &mut self,
        old: &[u8; 32],
        new: [u8; 32],
        new_size_bytes: u64,
        new_stored_at_ms: u64,
    ) -> Result<(), RekeyError> {
        if old != &new && self.entries.contains_key(&new) {
            return Err(RekeyError::Collision);
        }
        let Some(mut entry) = self.entries.remove(old) else {
            return Err(RekeyError::OldMissing);
        };
        entry.cid = new;
        entry.size_bytes = new_size_bytes;
        entry.stored_at_ms = new_stored_at_ms;
        self.entries.insert(new, entry);
        self.save();
        Ok(())
    }

    /// Flip the `pinned` flag. Returns `true` if the flag changed;
    /// `false` if already at the target state or the CID is unknown.
    pub fn set_pinned(&mut self, cid: &[u8; 32], pinned: bool) -> bool {
        let Some(entry) = self.entries.get_mut(cid) else {
            return false;
        };
        if entry.pinned == pinned {
            return false;
        }
        entry.pinned = pinned;
        self.save();
        true
    }

    /// Set replication tier on a batch. Returns the count of entries
    /// whose tier actually changed (missing or already-at-tier entries
    /// are skipped silently).
    pub fn set_replication_tier(
        &mut self,
        cids: &[[u8; 32]],
        tier: ReplicationTier,
    ) -> usize {
        let mut changed = 0;
        for cid in cids {
            if let Some(entry) = self.entries.get_mut(cid) {
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

    /// Look up a single entry by CID.
    pub fn get(&self, cid: &[u8; 32]) -> Option<&ContentIndexEntry> {
        self.entries.get(cid)
    }

    /// Iterate over all entries. **Order is not guaranteed** (HashMap-backed).
    /// Callers that surface results to users must sort — for example, by
    /// `stored_at_ms` descending in the File Manager list view.
    pub fn entries(&self) -> impl Iterator<Item = &ContentIndexEntry> {
        self.entries.values()
    }
}

pub(crate) mod hex_cid {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(cid: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(cid))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
        <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
            serde::de::Error::custom(format!("expected 32-byte hex CID, got {}", s.len() / 2))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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

    #[test]
    fn load_missing_file_returns_empty() {
        let dir = tempdir().unwrap();
        let idx = ContentIndex::load(dir.path());
        assert!(idx.entries.is_empty());
    }

    #[test]
    fn save_then_load_roundtrips_entries() {
        let dir = tempdir().unwrap();
        let entry = sample_entry([0xAA; 32]);

        let mut idx = ContentIndex::load(dir.path());
        idx.entries.insert(entry.cid, entry.clone());
        idx.save();

        let reloaded = ContentIndex::load(dir.path());
        assert_eq!(reloaded.entries.len(), 1);
        assert_eq!(reloaded.entries.get(&entry.cid), Some(&entry));
    }

    #[test]
    fn load_malformed_json_returns_empty() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(INDEX_FILE), b"{ not valid json").unwrap();
        let idx = ContentIndex::load(dir.path());
        assert!(idx.entries.is_empty());
    }

    #[test]
    fn load_wrong_version_returns_empty() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join(INDEX_FILE),
            br#"{"version": 99, "entries": []}"#,
        )
        .unwrap();
        let idx = ContentIndex::load(dir.path());
        assert!(idx.entries.is_empty());
    }

    #[test]
    fn insert_adds_entry_and_returns_true() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xBB; 32]);
        assert!(idx.insert(entry.clone()));
        assert_eq!(idx.get(&entry.cid), Some(&entry));
    }

    #[test]
    fn insert_duplicate_cid_returns_false() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xCC; 32]);
        assert!(idx.insert(entry.clone()));
        assert!(!idx.insert(entry));
    }

    #[test]
    fn remove_returns_true_when_present_false_otherwise() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xDD; 32]);
        idx.insert(entry.clone());
        assert!(idx.remove(&entry.cid));
        assert!(!idx.remove(&entry.cid));
    }

    #[test]
    fn set_archived_flips_flag_and_reports_change() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xEE; 32]);
        idx.insert(entry.clone());

        assert!(idx.set_archived(&entry.cid, true));  // flipped
        assert!(idx.get(&entry.cid).unwrap().archived);
        assert!(!idx.set_archived(&entry.cid, true)); // idempotent
    }

    #[test]
    fn set_archived_missing_cid_returns_false() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        assert!(!idx.set_archived(&[0xFF; 32], true));
    }

    #[test]
    fn set_replication_tier_counts_updated_entries() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let a = sample_entry([0x01; 32]);
        let b = sample_entry([0x02; 32]);
        idx.insert(a.clone());
        idx.insert(b.clone());

        // Both are Default; bumping to Ultra should update 2.
        let updated = idx.set_replication_tier(&[a.cid, b.cid], ReplicationTier::Ultra);
        assert_eq!(updated, 2);

        // Same call again: tier already Ultra, so 0 updated.
        let again = idx.set_replication_tier(&[a.cid, b.cid], ReplicationTier::Ultra);
        assert_eq!(again, 0);

        // Missing CID is skipped, not an error.
        let with_missing =
            idx.set_replication_tier(&[a.cid, [0xAA; 32]], ReplicationTier::Expendable);
        assert_eq!(with_missing, 1);
    }

    #[test]
    fn save_is_noop_on_empty_path() {
        // NodeState::default() constructs a ContentIndex with an empty
        // path. Ensure mutations on that degenerate state don't
        // accidentally write content-index.json into CWD.
        let mut idx = ContentIndex::load(Path::new(""));
        assert!(idx.insert(sample_entry([0xFE; 32])));
        // No file should have been created in CWD.
        assert!(!Path::new("content-index.json").exists());
    }

    #[test]
    fn save_persists_mutations() {
        let dir = tempdir().unwrap();
        {
            let mut idx = ContentIndex::load(dir.path());
            idx.insert(sample_entry([0xA1; 32]));
            idx.insert(sample_entry([0xA2; 32]));
            idx.remove(&[0xA1; 32]);
            assert!(idx.set_archived(&[0xA2; 32], true));
            assert_eq!(
                idx.set_replication_tier(&[[0xA2; 32]], ReplicationTier::Ultra),
                1
            );
        }
        let reloaded = ContentIndex::load(dir.path());
        assert_eq!(reloaded.entries.len(), 1);
        let entry = reloaded.get(&[0xA2; 32]).expect("A2 persisted");
        assert!(entry.archived, "archived flag persisted");
        assert_eq!(
            entry.replication_tier,
            ReplicationTier::Ultra,
            "tier mutation persisted"
        );
    }

    #[test]
    fn set_pinned_flips_flag_and_reports_change() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xB1; 32]);
        idx.insert(entry.clone());

        assert!(idx.set_pinned(&entry.cid, true));  // flipped
        assert!(idx.get(&entry.cid).unwrap().pinned);
        assert!(!idx.set_pinned(&entry.cid, true)); // idempotent, no change
        assert!(idx.set_pinned(&entry.cid, false)); // flipped back
        assert!(!idx.get(&entry.cid).unwrap().pinned);
    }

    #[test]
    fn set_pinned_missing_cid_returns_false() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        assert!(!idx.set_pinned(&[0xB2; 32], true));
    }

    #[test]
    fn save_persists_pin_mutations() {
        let dir = tempdir().unwrap();
        {
            let mut idx = ContentIndex::load(dir.path());
            idx.insert(sample_entry([0xB3; 32]));
            assert!(idx.set_pinned(&[0xB3; 32], true));
        }
        let reloaded = ContentIndex::load(dir.path());
        assert!(
            reloaded.get(&[0xB3; 32]).expect("B3 persisted").pinned,
            "pinned flag must survive save/load"
        );
    }

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

        let result = idx.rekey(
            &[0x01; 32],
            [0x02; 32],
            /* new_size_bytes */ 999,
            /* new_stored_at_ms */ 1234,
        );
        assert!(result.is_ok(), "rekey must succeed when old key exists");

        assert!(idx.get(&[0x01; 32]).is_none(), "old key removed");
        let after = idx.get(&[0x02; 32]).expect("new key present");
        assert_eq!(after.file_name, "Folder", "file_name carried forward");
        assert_eq!(after.kind, ContentKind::Folder, "kind carried forward");
        assert!(after.pinned, "pinned carried forward");
        assert_eq!(after.size_bytes, 999, "size_bytes updated");
        assert_eq!(after.stored_at_ms, 1234, "stored_at_ms updated");

        // Non-existent old key returns OldMissing.
        assert_eq!(
            idx.rekey(&[0xFF; 32], [0xEE; 32], 0, 0),
            Err(RekeyError::OldMissing),
        );
    }

    #[test]
    fn rekey_refuses_collision_instead_of_overwriting() {
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());

        // Two distinct entries: keeper at 0xAA, victim-target at 0xBB.
        let mut keeper = sample_entry([0xAA; 32]);
        keeper.file_name = "Keeper".into();
        idx.insert(keeper);

        let mut other = sample_entry([0xBB; 32]);
        other.file_name = "OtherRoot".into();
        idx.insert(other);

        // Try to rekey OtherRoot from 0xBB → 0xAA. Without the collision
        // guard this would clobber Keeper. With it we get a Collision
        // error and both entries remain intact.
        let result = idx.rekey(&[0xBB; 32], [0xAA; 32], 0, 0);
        assert_eq!(result, Err(RekeyError::Collision));

        // Both entries still present and unchanged.
        assert_eq!(idx.get(&[0xAA; 32]).unwrap().file_name, "Keeper");
        assert_eq!(idx.get(&[0xBB; 32]).unwrap().file_name, "OtherRoot");
    }

    #[test]
    fn rekey_old_equals_new_is_a_self_update_not_a_collision() {
        // rekey(old=X, new=X) should be allowed — it's a metadata refresh
        // (size_bytes / stored_at_ms only). The collision guard checks
        // `old != &new` before refusing, so this path is not blocked.
        let dir = tempdir().unwrap();
        let mut idx = ContentIndex::load(dir.path());
        let entry = sample_entry([0xCC; 32]);
        idx.insert(entry);

        let result = idx.rekey(&[0xCC; 32], [0xCC; 32], 12345, 67890);
        assert!(result.is_ok());
        let after = idx.get(&[0xCC; 32]).expect("entry still present");
        assert_eq!(after.size_bytes, 12345);
        assert_eq!(after.stored_at_ms, 67890);
    }

    #[test]
    fn legacy_sidecar_without_pinned_field_loads_as_unpinned() {
        // Simulate a pre-ZEB-155 sidecar: version 1, entries with every field
        // EXCEPT pinned. `#[serde(default)]` on the new field must make this
        // deserialize cleanly with pinned=false.
        let dir = tempdir().unwrap();
        let legacy_json = br#"{
            "version": 1,
            "entries": [
                {
                    "cid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "file_name": "legacy.txt",
                    "size_bytes": 10,
                    "stored_at_ms": 1700000000000,
                    "sensitivity": "private",
                    "replication_tier": "default",
                    "licensed": false,
                    "archived": false
                }
            ]
        }"#;
        std::fs::write(dir.path().join(INDEX_FILE), legacy_json).unwrap();

        let idx = ContentIndex::load(dir.path());
        let entry = idx.get(&[0xAA; 32]).expect("legacy entry must load");
        assert!(!entry.pinned, "legacy entries must read as pinned=false");
        assert_eq!(entry.file_name, "legacy.txt");
    }

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
}
