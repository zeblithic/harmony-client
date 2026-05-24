//! Persisted user preferences for Phase 2 pkarr policies.
//!
//! Only case B (opt-in identity-keyed discoverability) needs persistence today.
//! Lives at `<app_data_dir>/connectivity-settings.json`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PkarrSettings {
    /// Case B (identity-keyed discoverability) — opt-in, default OFF.
    #[serde(default)]
    pub identity_discoverable: bool,
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
