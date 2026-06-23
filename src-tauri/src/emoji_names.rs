//! Local-only, per-user custom-emoji nicknames (CID → personal name).
//!
//! A purely-local label the user attaches to a PUBLIC custom emoji so they can
//! find/reuse it by name. NEVER published, broadcast, or synced in this phase —
//! the privacy guarantee is structural: these bytes live in their OWN file,
//! outside `OwnerState`. Entries carry a monotonic `updated_ms` LWW key so the
//! ZEB-417 fleet-sync substrate can later adopt the whole map as a replicated
//! dataset (parity with `friend_nicknames.rs`). Persistence mirrors
//! `friend_nicknames.rs`: `load_or_default` tolerates a missing/corrupt file
//! (→ empty), `save` writes atomically.

use std::collections::BTreeMap;
use std::path::Path;

/// Max emoji-name length, in chars. Matches the spec charset bound.
pub const MAX_EMOJI_NAME_LEN: usize = 32;

/// True iff `name` is a valid emoji nickname: 1..=32 chars of `[A-Za-z0-9_-]`.
pub fn valid_emoji_name(name: &str) -> bool {
    let n = name.chars().count();
    (1..=MAX_EMOJI_NAME_LEN).contains(&n)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EmojiNames {
    /// cid hex (lowercase, 64 chars) -> entry.
    #[serde(default)]
    pub entries: BTreeMap<String, EmojiNameEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EmojiNameEntry {
    pub name: String,
    /// MIME of the emoji image (e.g. "image/png") — stored so reuse needs no
    /// re-fetch; advisory (the renderer detects the real format from the header).
    pub mime: String,
    /// Plaintext byte length — the signed React descriptor size, supplied by the
    /// caller (a chip's `emojiSize` or the upload's returned size). Authoritative
    /// for re-reacting; the serve path re-derives + hard-caps regardless.
    pub size: u64,
    /// Wall-clock ms at last write — local LWW key (see module docs).
    pub updated_ms: u64,
}

impl EmojiNames {
    /// Load from `path`. Missing file → empty (normal first run). Corrupt/unreadable
    /// → empty + WARN (a bad file can't brick the picker, but a real problem stays
    /// visible). Mirrors `FriendNicknames::load_or_default`.
    pub fn load_or_default(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        error = %e, path = %path.display(),
                        "emoji_names: corrupt file; using empty set"
                    );
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!(
                    error = %e, path = %path.display(),
                    "emoji_names: read failed; using empty set (names may be temporarily unavailable)"
                );
                Self::default()
            }
        }
    }

    /// Atomically persist to `path`, creating the parent dir first. Reuses
    /// `owner_state_persist::save_atomically` (parity with `FriendNicknames::save`).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| format!("encode: {e}"))?;
        crate::owner_state_persist::save_atomically(path, &bytes)
            .map_err(|e| format!("save emoji_names: {e}"))
    }

    /// Upsert (`Some` name) or clear (`None`) the name for `cid_hex` (lowercased).
    /// `mime`/`size` are recorded with a set and ignored on clear. The caller is
    /// responsible for validating the name (`valid_emoji_name`) and the CID's
    /// public-ness before calling.
    pub fn set(&mut self, cid_hex: &str, name: Option<&str>, mime: &str, size: u64, now_ms: u64) {
        let key = cid_hex.to_lowercase();
        match name {
            Some(n) => {
                self.entries.insert(
                    key,
                    EmojiNameEntry {
                        name: n.to_string(),
                        mime: mime.to_string(),
                        size,
                        updated_ms: now_ms,
                    },
                );
            }
            None => {
                self.entries.remove(&key);
            }
        }
    }

    /// The entry for `cid_hex`, if any.
    pub fn get(&self, cid_hex: &str) -> Option<&EmojiNameEntry> {
        self.entries.get(&cid_hex.to_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_name_charset_and_length() {
        assert!(valid_emoji_name("catjam"));
        assert!(valid_emoji_name("ship_it-2"));
        assert!(valid_emoji_name("a"));
        assert!(valid_emoji_name(&"x".repeat(32)));
        assert!(!valid_emoji_name("")); // empty
        assert!(!valid_emoji_name(&"x".repeat(33))); // too long
        assert!(!valid_emoji_name("has space"));
        assert!(!valid_emoji_name("emoji!")); // punctuation
        assert!(!valid_emoji_name("café")); // non-ascii
    }

    #[test]
    fn set_get_roundtrips_and_lowercases_cid() {
        let mut m = EmojiNames::default();
        m.set("AABB", Some("catjam"), "image/png", 200, 100);
        let e = m.get("aabb").expect("entry");
        assert_eq!(e.name, "catjam");
        assert_eq!(e.mime, "image/png");
        assert_eq!(e.size, 200);
        assert_eq!(e.updated_ms, 100);
        assert_eq!(m.get("AABB").unwrap().name, "catjam");
    }

    #[test]
    fn none_clears() {
        let mut m = EmojiNames::default();
        m.set("aa", Some("x"), "image/png", 1, 1);
        m.set("aa", None, "", 0, 2);
        assert!(m.get("aa").is_none());
    }

    #[test]
    fn updated_ms_and_fields_advance_on_reset() {
        let mut m = EmojiNames::default();
        m.set("aa", Some("x"), "image/png", 10, 10);
        m.set("aa", Some("y"), "image/jpeg", 20, 20);
        let e = m.get("aa").unwrap();
        assert_eq!(e.name, "y");
        assert_eq!(e.mime, "image/jpeg");
        assert_eq!(e.size, 20);
        assert_eq!(e.updated_ms, 20);
    }

    #[test]
    fn two_cids_may_share_a_name() {
        let mut m = EmojiNames::default();
        m.set("aa", Some("dup"), "image/png", 1, 1);
        m.set("bb", Some("dup"), "image/png", 1, 1);
        assert_eq!(m.get("aa").unwrap().name, "dup");
        assert_eq!(m.get("bb").unwrap().name, "dup");
    }

    #[test]
    fn load_or_default_tolerates_missing_and_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("emoji_names.json");
        assert!(EmojiNames::load_or_default(&path).entries.is_empty());
        std::fs::write(&path, b"not json").unwrap();
        assert!(EmojiNames::load_or_default(&path).entries.is_empty());
    }

    #[test]
    fn save_then_load_roundtrips_and_creates_parent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/sub/emoji_names.json");
        let mut m = EmojiNames::default();
        m.set("aa", Some("catjam"), "image/png", 7, 7);
        m.save(&path).expect("save creates the parent dir");
        let loaded = EmojiNames::load_or_default(&path);
        assert_eq!(loaded.get("aa").unwrap().name, "catjam");
        assert_eq!(loaded.get("aa").unwrap().size, 7);
    }
}
