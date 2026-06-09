//! ZEB-419: local-only, per-owner friend nicknames.
//!
//! A purely-local label the user attaches to a friend for their own reference.
//! NEVER published, broadcast, or synced in this phase — the privacy guarantee
//! ("nobody sees the nickname you give a contact") is structural: these bytes
//! live in their OWN file, outside `OwnerState.friend_graph` (the published
//! CRDT). Entries carry a monotonic `updated_ms` LWW key so the ZEB-417
//! fleet-sync substrate can later adopt the whole map as a replicated dataset.
//!
//! Persistence mirrors `pkarr_settings.rs`: `load_or_default` tolerates a
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
    /// Load from `path`, or return an empty map when the file is missing or
    /// unparseable (never panics; a corrupt file must not brick the panel).
    pub fn load_or_default(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Atomically persist to `path` (write temp in the same dir, then rename).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| format!("encode: {e}"))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes).map_err(|e| format!("write tmp: {e}"))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))?;
        Ok(())
    }

    /// Upsert (`Some` non-blank) or clear (`None`/blank) a nickname. `owner_id_hex`
    /// is lowercased. Returns true when the map changed.
    pub fn set(&mut self, owner_id_hex: &str, nickname: Option<&str>, now_ms: u64) -> bool {
        let key = owner_id_hex.to_lowercase();
        match nickname.map(str::trim).filter(|s| !s.is_empty()) {
            Some(nick) => {
                let prev = self.entries.insert(
                    key,
                    NicknameEntry {
                        nickname: nick.to_string(),
                        updated_ms: now_ms,
                    },
                );
                !matches!(prev, Some(p) if p.nickname == nick)
            }
            None => self.entries.remove(&key).is_some(),
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
}
