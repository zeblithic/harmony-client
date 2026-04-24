//! Client-side sidecar for self-ingested content metadata (ZEB-146).
//!
//! Persists a map of `cid -> ContentIndexEntry` as JSON under
//! `app_data_dir/content-index.json` so the File Manager UI can surface
//! filenames, ingest timestamps, and user-set flags (sensitivity,
//! replication tier, licensed, archived) for content that the runtime's
//! RAM-only cache doesn't know about.
//!
//! Authority split:
//! - Sidecar is authoritative for membership and size_bytes (CIDs are
//!   immutable, so size never drifts from the ingest-time value).
//! - Runtime cache is authoritative for pinned state (pin is an eviction
//!   concept the cache owns).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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

mod hex_cid {
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
}
