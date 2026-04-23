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

// Dormant until Task B3 wires up mutations (insert/remove/set_archived/
// set_replication_tier) and Task B6+ adds the Tauri command callers.
// Remove this allow once those callers exist.
#![allow(dead_code)]

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
    Minimal,
    Default,
    Durable,
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
        let entries = Self::read_file(&path).unwrap_or_default();
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
        let file = IndexFile {
            version: FILE_VERSION,
            entries: self.entries.values().cloned().collect(),
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
}
