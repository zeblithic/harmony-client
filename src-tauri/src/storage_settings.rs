//! ZEB-669 slice 2: persisted storage-buddy settings.
//!
//! `storage_settings.json` (the `vine_settings` pattern: version-1
//! envelope, tolerant load, atomic 0600 writes) holds everything the
//! LOCAL user declared — the shared budget, their pledges (device-local
//! v1), dismissed invites — plus the restart-persisted publish floors
//! that keep the per-record LWW clocks monotonic across restarts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const STORAGE_SETTINGS_FILE: &str = "storage_settings.json";
const FILE_VERSION: u32 = 1;

/// Spec §3 default for the enforced shared budget: 10 GB.
pub const DEFAULT_SHARED_BUDGET_BYTES: u64 = 10_000_000_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageSettings {
    /// User-configurable "storage I share with buddies" budget — the
    /// contribution meter's denominator, ENFORCED against buddy pins.
    pub shared_budget_bytes: u64,
    /// My pledges: buddy owner address → bytes I offer to host for them.
    /// A 0-byte pledge is a valid accept (spec §0.1).
    #[serde(default)]
    pub my_pledges: BTreeMap<String, u64>,
    /// Declined invites: inviter owner address → the invite's
    /// `updated_at` at dismissal. The inviter's record isn't ours to
    /// remove, so a dismissed invite simply stops surfacing unless
    /// re-issued with a newer `updated_at` (spec §4).
    #[serde(default)]
    pub dismissed_invites: BTreeMap<String, u64>,
    /// lastPublishedUpdatedAt floors, one per record type.
    #[serde(default)]
    pub pledge_floor: u64,
    #[serde(default)]
    pub backup_set_floor: u64,
    #[serde(default)]
    pub hosting_floor: u64,
}

impl Default for StorageSettings {
    fn default() -> Self {
        Self {
            shared_budget_bytes: DEFAULT_SHARED_BUDGET_BYTES,
            my_pledges: BTreeMap::new(),
            dismissed_invites: BTreeMap::new(),
            pledge_floor: 0,
            backup_set_floor: 0,
            hosting_floor: 0,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct StorageSettingsFile {
    version: u32,
    #[serde(flatten)]
    settings: StorageSettings,
}

pub fn settings_path(data_dir: &Path) -> PathBuf {
    data_dir.join(STORAGE_SETTINGS_FILE)
}

/// Missing, corrupt, or foreign-version files yield the default —
/// settings must never block boot.
pub fn load_or_default(path: &Path) -> StorageSettings {
    let Ok(bytes) = std::fs::read(path) else {
        return StorageSettings::default();
    };
    match serde_json::from_slice::<StorageSettingsFile>(&bytes) {
        Ok(file) if file.version == FILE_VERSION => file.settings,
        Ok(file) => {
            tracing::warn!("storage_settings version {} unsupported, using defaults", file.version);
            StorageSettings::default()
        }
        Err(e) => {
            tracing::warn!("storage_settings load failed, using defaults: {e}");
            StorageSettings::default()
        }
    }
}

/// Best-effort persist (errors logged, not surfaced — matching
/// `vine_settings::save`; callers treat settings persistence as
/// self-healing on the next write).
pub fn save(path: &Path, settings: &StorageSettings) {
    let file = StorageSettingsFile {
        version: FILE_VERSION,
        settings: settings.clone(),
    };
    match serde_json::to_vec_pretty(&file) {
        Ok(bytes) => {
            if let Err(e) = crate::identity::write_atomic_0600(path, &bytes) {
                tracing::error!("storage_settings save failed: {e}");
            }
        }
        Err(e) => tracing::error!("storage_settings serialize failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_10gb_and_empty() {
        let s = StorageSettings::default();
        assert_eq!(s.shared_budget_bytes, 10_000_000_000);
        assert!(s.my_pledges.is_empty());
        assert!(s.dismissed_invites.is_empty());
        assert_eq!(s.pledge_floor, 0);
    }

    #[test]
    fn roundtrip_save_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = settings_path(dir.path());
        let mut s = StorageSettings::default();
        s.shared_budget_bytes = 42;
        s.my_pledges.insert("buddy".into(), 1_000);
        s.dismissed_invites.insert("spammer".into(), 77);
        s.pledge_floor = 5;
        s.backup_set_floor = 6;
        s.hosting_floor = 7;
        save(&path, &s);

        let loaded = load_or_default(&path);
        assert_eq!(loaded.shared_budget_bytes, 42);
        assert_eq!(loaded.my_pledges.get("buddy"), Some(&1_000));
        assert_eq!(loaded.dismissed_invites.get("spammer"), Some(&77));
        assert_eq!(
            (loaded.pledge_floor, loaded.backup_set_floor, loaded.hosting_floor),
            (5, 6, 7)
        );
    }

    #[test]
    fn corrupt_or_missing_or_foreign_version_yields_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = settings_path(dir.path());
        assert_eq!(load_or_default(&path).shared_budget_bytes, DEFAULT_SHARED_BUDGET_BYTES);

        std::fs::write(&path, b"{broken").unwrap();
        assert_eq!(load_or_default(&path).shared_budget_bytes, DEFAULT_SHARED_BUDGET_BYTES);

        std::fs::write(&path, br#"{"version":9,"shared_budget_bytes":1}"#).unwrap();
        assert_eq!(load_or_default(&path).shared_budget_bytes, DEFAULT_SHARED_BUDGET_BYTES);
    }
}
