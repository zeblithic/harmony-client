//! Persisted user preferences for Phase 2 pkarr policies.
//!
//! Only case B (opt-in identity-keyed discoverability) needs persistence today.
//! Lives at `<app_data_dir>/connectivity-settings.json`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PkarrSettings {
    /// Case B (identity-keyed discoverability) — opt-in, default OFF.
    #[serde(default)]
    pub identity_discoverable: bool,
    /// ZEB-371 Task 12 (spec §7.1): per-user "auto-accept known requesters"
    /// toggle for the Path-A (no-token) friend flow. A KNOWN requester (already
    /// an Active|Pending friend) is accepted inline without prompting when this
    /// is ON; an UNKNOWN requester is NEVER auto-accepted regardless. Jake's
    /// "Both" choice — default ON.
    #[serde(default = "default_friend_auto_accept_known")]
    pub friend_auto_accept_known: bool,
}

/// Default for [`PkarrSettings::friend_auto_accept_known`]: ON (spec §7.1).
fn default_friend_auto_accept_known() -> bool {
    true
}

impl Default for PkarrSettings {
    fn default() -> Self {
        Self {
            identity_discoverable: false,
            friend_auto_accept_known: default_friend_auto_accept_known(),
        }
    }
}

impl PkarrSettings {
    pub fn load_or_default(path: &PathBuf) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &PathBuf) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn defaults_to_not_discoverable() {
        let settings = PkarrSettings::default();
        assert!(!settings.identity_discoverable);
    }

    #[test]
    fn defaults_to_auto_accept_known_on() {
        // ZEB-371 spec §7.1: auto-accept KNOWN requesters defaults ON.
        let settings = PkarrSettings::default();
        assert!(settings.friend_auto_accept_known);
    }

    #[test]
    fn missing_auto_accept_field_defaults_on() {
        // An older settings file (pre-ZEB-371) has no `friend_auto_accept_known`
        // key; serde's field default must fill it ON so existing users keep the
        // spec default rather than silently flipping to OFF.
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("legacy.json");
        std::fs::write(&path, r#"{"identity_discoverable":true}"#).expect("write");
        let loaded = PkarrSettings::load_or_default(&path);
        assert!(loaded.identity_discoverable);
        assert!(loaded.friend_auto_accept_known);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("nonexistent.json");
        let settings = PkarrSettings::load_or_default(&path);
        assert!(!settings.identity_discoverable);
    }

    #[test]
    fn round_trip_save_then_load() {
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("connectivity-settings.json");
        let settings = PkarrSettings {
            identity_discoverable: true,
            friend_auto_accept_known: false,
        };
        settings.save(&path).expect("save");

        let loaded = PkarrSettings::load_or_default(&path);
        assert_eq!(loaded, settings);
    }

    #[test]
    fn load_corrupted_file_returns_default() {
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("bad.json");
        std::fs::write(&path, "not json {{").expect("write");
        let settings = PkarrSettings::load_or_default(&path);
        assert!(!settings.identity_discoverable);
    }
}
