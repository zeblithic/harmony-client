//! Follow list persistence — tracks followed addresses with optional names.
//!
//! Uses atomic writes (write temp file, then rename) to avoid partial writes.
//! The JSON format is:
//! ```json
//! {
//!   "version": 1,
//!   "follows": [
//!     { "address": "aa3f7b21...", "name": "Alice", "followed_at": 1712450000 }
//!   ]
//! }
//! ```

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const FOLLOWS_FILE: &str = "follows.json";
const FILE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FollowEntry {
    pub address: String,
    #[serde(default)]
    pub name: Option<String>,
    pub followed_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct FollowsFile {
    version: u32,
    follows: Vec<FollowEntry>,
}

pub struct FollowManager {
    path: PathBuf,
    follows: Vec<FollowEntry>,
}

impl FollowManager {
    /// Load the follow list from `data_dir/follows.json`.
    /// Returns an empty manager if the file is missing or corrupt.
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(FOLLOWS_FILE);
        let follows = Self::read_file(&path).unwrap_or_default();
        FollowManager { path, follows }
    }

    fn read_file(path: &Path) -> Option<Vec<FollowEntry>> {
        let data = std::fs::read(path).ok()?;
        let file: FollowsFile = serde_json::from_slice(&data).ok()?;
        if file.version != FILE_VERSION {
            return None;
        }
        Some(file.follows)
    }

    /// Atomically write the follow list to disk (temp file + rename).
    fn save(&self) {
        let file = FollowsFile {
            version: FILE_VERSION,
            follows: self.follows.clone(),
        };
        let json = match serde_json::to_vec_pretty(&file) {
            Ok(j) => j,
            Err(_) => return,
        };

        // Build a temp path alongside the real path.
        let tmp_path = {
            let mut name = self.path.file_name().unwrap_or_default().to_os_string();
            name.push(".tmp");
            self.path.with_file_name(name)
        };

        // Ensure parent directory exists.
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(parent);
        }

        if std::fs::write(&tmp_path, &json).is_ok() {
            let _ = std::fs::rename(&tmp_path, &self.path);
        }
    }

    /// Follow an address. Returns `true` if newly added, `false` if already followed.
    pub fn follow(&mut self, address: String, name: Option<String>) -> bool {
        if self.follows.iter().any(|e| e.address == address) {
            return false;
        }
        let followed_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.follows.push(FollowEntry {
            address,
            name,
            followed_at,
        });
        self.save();
        true
    }

    /// Unfollow an address. Returns `true` if it was present, `false` if not.
    pub fn unfollow(&mut self, address: &str) -> bool {
        let before = self.follows.len();
        self.follows.retain(|e| e.address != address);
        let removed = self.follows.len() < before;
        if removed {
            self.save();
        }
        removed
    }

    /// Returns `true` if the address is currently followed.
    pub fn is_followed(&self, address: &str) -> bool {
        self.follows.iter().any(|e| e.address == address)
    }

    /// Returns all follow entries sorted by `followed_at` ascending.
    pub fn list(&self) -> Vec<FollowEntry> {
        let mut entries = self.follows.clone();
        entries.sort_by_key(|e| e.followed_at);
        entries
    }

    /// Returns all followed addresses.
    pub fn addresses(&self) -> Vec<String> {
        self.follows.iter().map(|e| e.address.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Create a unique temporary directory for each test.
    fn temp_dir() -> PathBuf {
        use rand::Rng;
        let id: u64 = rand::thread_rng().gen();
        let dir = std::env::temp_dir().join(format!("harmony_follows_test_{id}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn load_empty_dir_returns_empty_manager() {
        let dir = temp_dir();
        let mgr = FollowManager::load(&dir);
        assert!(mgr.list().is_empty());
    }

    #[test]
    fn follow_and_list() {
        let dir = temp_dir();
        let mut mgr = FollowManager::load(&dir);
        mgr.follow("addr1".to_string(), Some("Alice".to_string()));
        mgr.follow("addr2".to_string(), None);
        let list = mgr.list();
        assert_eq!(list.len(), 2);
        assert!(list
            .iter()
            .any(|e| e.address == "addr1" && e.name.as_deref() == Some("Alice")));
        assert!(list
            .iter()
            .any(|e| e.address == "addr2" && e.name.is_none()));
    }

    #[test]
    fn follow_is_idempotent() {
        let dir = temp_dir();
        let mut mgr = FollowManager::load(&dir);
        let first = mgr.follow("addr1".to_string(), Some("Alice".to_string()));
        let second = mgr.follow("addr1".to_string(), Some("Alice Again".to_string()));
        assert!(first, "first follow should return true");
        assert!(
            !second,
            "second follow should return false (already followed)"
        );
        assert_eq!(mgr.list().len(), 1);
    }

    #[test]
    fn unfollow_returns_true_when_present() {
        let dir = temp_dir();
        let mut mgr = FollowManager::load(&dir);
        mgr.follow("addr1".to_string(), None);
        let result = mgr.unfollow("addr1");
        assert!(result);
        assert!(!mgr.is_followed("addr1"));
    }

    #[test]
    fn unfollow_returns_false_when_absent() {
        let dir = temp_dir();
        let mut mgr = FollowManager::load(&dir);
        let result = mgr.unfollow("nonexistent");
        assert!(!result);
    }

    #[test]
    fn persistence_round_trip() {
        let dir = temp_dir();
        {
            let mut mgr = FollowManager::load(&dir);
            mgr.follow("addr1".to_string(), Some("Alice".to_string()));
            mgr.follow("addr2".to_string(), None);
            // mgr drops here, file is already saved after each follow()
        }
        // Reload from disk
        let mgr2 = FollowManager::load(&dir);
        let list = mgr2.list();
        assert_eq!(list.len(), 2);
        assert!(mgr2.is_followed("addr1"));
        assert!(mgr2.is_followed("addr2"));
        let alice = list.iter().find(|e| e.address == "addr1").unwrap();
        assert_eq!(alice.name.as_deref(), Some("Alice"));
    }

    #[test]
    fn addresses_returns_all_followed() {
        let dir = temp_dir();
        let mut mgr = FollowManager::load(&dir);
        mgr.follow("addr1".to_string(), None);
        mgr.follow("addr2".to_string(), None);
        mgr.follow("addr3".to_string(), None);
        let addrs = mgr.addresses();
        assert_eq!(addrs.len(), 3);
        assert!(addrs.contains(&"addr1".to_string()));
        assert!(addrs.contains(&"addr2".to_string()));
        assert!(addrs.contains(&"addr3".to_string()));
    }
}
