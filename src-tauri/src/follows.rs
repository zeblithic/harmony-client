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
    /// When the file was unreadable at load (transient Io), or corrupt bytes could not
    /// be quarantined aside, writes are frozen so the still-good on-disk follow graph is
    /// never overwritten with the empty in-memory default (ZEB-986).
    disk_write_frozen: bool,
}

impl FollowManager {
    /// Load the follow list from `data_dir/follows.json`.
    ///
    /// A missing file starts empty (first run). A transient read error starts empty but
    /// FREEZES writes (the on-disk graph may still be good). Malformed/undecodable bytes
    /// are quarantined aside (`follows.json.corrupt-<now_ms>`) and heal on the next write.
    /// A parseable-but-unsupported `version` (forward/foreign build) FREEZES in place —
    /// left intact, not quarantined — so the next `save()` cannot overwrite it with an
    /// empty current-version file (a downgrade or later support can still use it).
    pub fn load(data_dir: &Path, now_ms: u64) -> Self {
        let path = data_dir.join(FOLLOWS_FILE);
        // Parse only — the version check happens after so an unsupported-but-parseable
        // version freezes in place rather than being quarantined-and-healed (which would
        // drop the foreign-version follows on the next mutation).
        let recovered = crate::recoverable_load::load_or_recover::<Option<FollowsFile>>(
            &path,
            now_ms,
            |bytes| {
                serde_json::from_slice::<FollowsFile>(bytes)
                    .map(Some)
                    .map_err(|e| e.to_string())
            },
        );
        let (follows, disk_write_frozen) = match recovered.value {
            Some(file) if file.version == FILE_VERSION => {
                (file.follows, recovered.disk_write_frozen)
            }
            Some(file) => {
                tracing::warn!(
                    version = file.version,
                    expected = FILE_VERSION,
                    "follows: unexpected version; freezing writes to preserve the foreign-version file in place"
                );
                (Vec::new(), true)
            }
            None => (Vec::new(), recovered.disk_write_frozen),
        };
        FollowManager {
            path,
            follows,
            disk_write_frozen,
        }
    }

    /// Atomically write the follow list to disk (temp file + rename).
    fn save(&self) {
        if self.disk_write_frozen {
            tracing::warn!(
                path = ?self.path,
                "follows save skipped — file unreadable at load; preserving existing graph"
            );
            return;
        }
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
    #[allow(dead_code)] // pre-existing; tracked for cleanup
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
        let mgr = FollowManager::load(&dir, 1);
        assert!(mgr.list().is_empty());
    }

    #[test]
    fn follow_and_list() {
        let dir = temp_dir();
        let mut mgr = FollowManager::load(&dir, 1);
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
        let mut mgr = FollowManager::load(&dir, 1);
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
        let mut mgr = FollowManager::load(&dir, 1);
        mgr.follow("addr1".to_string(), None);
        let result = mgr.unfollow("addr1");
        assert!(result);
        assert!(!mgr.is_followed("addr1"));
    }

    #[test]
    fn unfollow_returns_false_when_absent() {
        let dir = temp_dir();
        let mut mgr = FollowManager::load(&dir, 1);
        let result = mgr.unfollow("nonexistent");
        assert!(!result);
    }

    #[test]
    fn persistence_round_trip() {
        let dir = temp_dir();
        {
            let mut mgr = FollowManager::load(&dir, 1);
            mgr.follow("addr1".to_string(), Some("Alice".to_string()));
            mgr.follow("addr2".to_string(), None);
            // mgr drops here, file is already saved after each follow()
        }
        // Reload from disk
        let mgr2 = FollowManager::load(&dir, 1);
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
        let mut mgr = FollowManager::load(&dir, 1);
        mgr.follow("addr1".to_string(), None);
        mgr.follow("addr2".to_string(), None);
        mgr.follow("addr3".to_string(), None);
        let addrs = mgr.addresses();
        assert_eq!(addrs.len(), 3);
        assert!(addrs.contains(&"addr1".to_string()));
        assert!(addrs.contains(&"addr2".to_string()));
        assert!(addrs.contains(&"addr3".to_string()));
    }

    #[test]
    fn corrupt_follows_quarantined_and_heals() {
        let dir = temp_dir();
        std::fs::write(dir.join(FOLLOWS_FILE), b"{ not json").unwrap();
        let mut mgr = FollowManager::load(&dir, 4_242);
        assert!(mgr.list().is_empty(), "corrupt file starts empty");
        assert!(
            dir.join(format!("{FOLLOWS_FILE}.corrupt-4242")).exists(),
            "corrupt bytes quarantined aside"
        );
        // Not frozen: the next follow() heals by writing a fresh file.
        mgr.follow("addr1".to_string(), Some("Alice".to_string()));
        let reloaded = FollowManager::load(&dir, 5_000);
        assert!(reloaded.is_followed("addr1"), "healed write persisted");
    }

    #[cfg(unix)]
    #[test]
    fn io_error_freezes_follows_no_overwrite() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir();
        {
            let mut mgr = FollowManager::load(&dir, 1);
            mgr.follow("keepme".to_string(), None);
        }
        let p = dir.join(FOLLOWS_FILE);
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
        let mut mgr = FollowManager::load(&dir, 3);
        // restore perms so the frozen no-op and reload can read the untouched file
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        mgr.follow("newguy".to_string(), None); // frozen: save is a no-op
        let reloaded = FollowManager::load(&dir, 5);
        assert!(
            reloaded.is_followed("keepme"),
            "frozen save preserved original graph"
        );
        assert!(
            !reloaded.is_followed("newguy"),
            "frozen: new follow not persisted"
        );
    }

    #[test]
    fn wrong_version_freezes_and_preserves_follows() {
        // ZEB-986: a parseable-but-unsupported version is frozen in place (not
        // quarantined), so a downgrade cannot silently drop the foreign build's follows.
        let dir = temp_dir();
        let forward =
            br#"{"version":999,"follows":[{"address":"keepme","name":null,"followed_at":1}]}"#;
        let p = dir.join(FOLLOWS_FILE);
        std::fs::write(&p, forward).unwrap();

        let mut mgr = FollowManager::load(&dir, 5);
        assert!(
            mgr.list().is_empty(),
            "unsupported version starts empty in-memory"
        );
        // Frozen: the next follow()'s save is a no-op, so the file stays byte-identical.
        mgr.follow("newguy".to_string(), None);
        assert_eq!(
            std::fs::read(&p).unwrap(),
            forward,
            "foreign-version file left byte-identical"
        );
        assert!(
            !dir.join(format!("{FOLLOWS_FILE}.corrupt-5")).exists(),
            "unsupported version frozen in place, not quarantined"
        );
    }
}
