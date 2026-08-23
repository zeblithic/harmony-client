//! ZEB-419: local-only, per-owner friend nicknames.
//!
//! **LEGACY (ZEB-977):** superseded by the fleet-synced contacts dataset
//! (`contacts_crdt.rs` / `contacts_commands.rs`). This module is read only by
//! the one-time migration (`contacts_commands::migrate_friend_nicknames_to_
//! contacts`), which imports `friend_nicknames.json` into `contacts.cbor` and
//! renames the legacy file to `*.json.migrated`. No live write path remains.
//!
//! A purely-local label the user attaches to a friend for their own reference.
//! NEVER published or broadcast — the privacy guarantee ("nobody sees the
//! nickname you give a contact") is structural: these bytes live in their OWN
//! file, outside `OwnerState.friend_graph` (the published CRDT). Entries carry
//! a monotonic `updated_ms` LWW key, which the migration turns into the
//! imported entries' HLC wall clock.
//!
//! Persistence mirrors `connectivity_settings.rs`: `load_or_default` tolerates a
//! missing/corrupt file (→ empty), `save` writes atomically (temp + rename).

use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FriendNicknames {
    /// owner_id hex (lowercase, 32 chars) -> entry.
    #[serde(default)]
    pub entries: BTreeMap<String, NicknameEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NicknameEntry {
    pub nickname: String,
    /// Wall-clock ms at last write — local LWW key (see module docs).
    pub updated_ms: u64,
}

impl FriendNicknames {
    /// Load from `path`. A MISSING file is the normal first-run case → empty map,
    /// silently. A corrupt/unparseable file or an UNEXPECTED read error (e.g.
    /// permissions) also yields an empty map so a bad file can't brick the panel,
    /// but is logged at WARN so a real problem stays visible rather than silent.
    pub fn load_or_default(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        error = %e, path = %path.display(),
                        "friend_nicknames: corrupt file; using empty set"
                    );
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!(
                    error = %e, path = %path.display(),
                    "friend_nicknames: read failed; using empty set (nicknames may be temporarily unavailable)"
                );
                Self::default()
            }
        }
    }

    /// Atomically persist to `path`, creating the parent dir first. Reuses the
    /// shared `owner_state_persist::save_atomically` helper (NamedTempFile +
    /// fsync + atomic persist) rather than a hand-rolled temp+rename.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        // save_atomically requires the parent dir to exist; on a fresh profile
        // the owner data dir may not be created yet.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| format!("encode: {e}"))?;
        crate::owner_state_persist::save_atomically(path, &bytes)
            .map_err(|e| format!("save friend_nicknames: {e}"))
    }

    /// Upsert (`Some` non-blank) or clear (`None`/blank) a nickname. `owner_id_hex`
    /// is lowercased on the way in. No return value: every set advances
    /// `updated_ms`, so a "did the map change?" bool would be misleading (and was
    /// unused by callers).
    pub fn set(&mut self, owner_id_hex: &str, nickname: Option<&str>, now_ms: u64) {
        let key = owner_id_hex.to_lowercase();
        match nickname.map(str::trim).filter(|s| !s.is_empty()) {
            Some(nick) => {
                self.entries.insert(
                    key,
                    NicknameEntry {
                        nickname: nick.to_string(),
                        updated_ms: now_ms,
                    },
                );
            }
            None => {
                self.entries.remove(&key);
            }
        }
    }

    /// The nickname for `owner_id_hex`, if any.
    pub fn get(&self, owner_id_hex: &str) -> Option<&str> {
        self.entries
            .get(&owner_id_hex.to_lowercase())
            .map(|e| e.nickname.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_roundtrips_and_lowercases() {
        let mut n = FriendNicknames::default();
        n.set("AABB", Some("Koya"), 100);
        assert_eq!(n.get("aabb"), Some("Koya"));
        assert_eq!(n.get("AABB"), Some("Koya")); // get also lowercases
    }

    #[test]
    fn blank_or_none_clears() {
        let mut n = FriendNicknames::default();
        n.set("aa", Some("x"), 1);
        n.set("aa", Some("   "), 2); // whitespace clears
        assert_eq!(n.get("aa"), None);
        n.set("aa", Some("y"), 3);
        n.set("aa", None, 4); // None clears
        assert_eq!(n.get("aa"), None);
    }

    #[test]
    fn updated_ms_advances_on_reset() {
        let mut n = FriendNicknames::default();
        n.set("aa", Some("x"), 10);
        n.set("aa", Some("y"), 20);
        assert_eq!(n.entries["aa"].updated_ms, 20);
        assert_eq!(n.entries["aa"].nickname, "y");
    }

    #[test]
    fn load_or_default_tolerates_missing_and_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("friend_nicknames.json");
        assert!(FriendNicknames::load_or_default(&path).entries.is_empty());
        std::fs::write(&path, b"not json").unwrap();
        assert!(FriendNicknames::load_or_default(&path).entries.is_empty());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("friend_nicknames.json");
        let mut n = FriendNicknames::default();
        n.set("aa", Some("Koya"), 7);
        n.save(&path).unwrap();
        let loaded = FriendNicknames::load_or_default(&path);
        assert_eq!(loaded.get("aa"), Some("Koya"));
        assert_eq!(loaded.entries["aa"].updated_ms, 7);
    }

    #[test]
    fn save_creates_missing_parent_dir() {
        // A fresh profile may not have the settings dir yet; save() must create
        // it rather than fail with ENOENT.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/sub/friend_nicknames.json");
        let mut n = FriendNicknames::default();
        n.set("aa", Some("Koya"), 1);
        n.save(&path).expect("save creates the parent dir");
        assert_eq!(
            FriendNicknames::load_or_default(&path).get("aa"),
            Some("Koya")
        );
    }
}
