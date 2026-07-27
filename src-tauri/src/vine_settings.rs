//! ZEB-671: persisted vine feature settings — currently just
//! `share_follows`, the public + opt-out switch for follow-list wire
//! publication (Jake's call on ZEB-671: publish by default, one toggle
//! to stop publishing and retract).
//!
//! Writes go through `identity::write_atomic_0600` (per-write unique
//! temp + `create_new` + fsync + rename). Missing/corrupt/wrong-version
//! files load as defaults.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const VINE_SETTINGS_FILE: &str = "vine_settings.json";
const FILE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VineSettings {
    /// Publish the local follow list on `harmony/vines/{owner}/follows`.
    /// Defaults to `true`; turning it off publishes an empty-list
    /// retraction and suppresses all further publishes (including the
    /// boot republish) until re-enabled.
    pub share_follows: bool,
    /// Highest `updated_at` this node has ever signed into a follow-list
    /// record. Seeds `NodeState::follow_list_clock` at boot so the
    /// LWW floor survives restarts — without it, a wall clock that moved
    /// backwards across a restart could publish records every receiver
    /// ignores (`<=` is stale), silently stalling updates and the
    /// opt-out retraction (Qodo PR #447).
    #[serde(default)]
    pub last_published_updated_at: u64,
    /// ZEB-811: publish a pkarr relay record so followers outside this
    /// owner's communities can fetch their vines. Vines are public by
    /// intent (spec product decision 4), so — unlike `share_follows`,
    /// which is opt-out by user preference — a LEGACY settings file that
    /// predates this field must default to `true`, not serde's normal
    /// bool default of `false`. `#[serde(default)]` would silently
    /// un-publish every pre-ZEB-811 node on first load.
    #[serde(default = "default_true")]
    pub share_vines_publicly: bool,
}

fn default_true() -> bool {
    true
}

impl Default for VineSettings {
    fn default() -> Self {
        VineSettings {
            share_follows: true,
            last_published_updated_at: 0,
            share_vines_publicly: true,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct VineSettingsFile {
    version: u32,
    #[serde(flatten)]
    settings: VineSettings,
}

/// Path of the settings file under `data_dir`.
pub fn settings_path(data_dir: &Path) -> PathBuf {
    data_dir.join(VINE_SETTINGS_FILE)
}

/// Load settings from `path`; missing, corrupt, or wrong-version files
/// yield the defaults.
pub fn load_or_default(path: &Path) -> VineSettings {
    let Ok(data) = std::fs::read(path) else {
        return VineSettings::default();
    };
    match serde_json::from_slice::<VineSettingsFile>(&data) {
        Ok(file) if file.version == FILE_VERSION => file.settings,
        _ => VineSettings::default(),
    }
}

/// Atomically persist `settings` to `path` via the repo's per-write
/// unique-temp pattern (`identity::write_atomic_0600` — a fixed `.tmp`
/// sibling would let two concurrent saves clobber each other's staging
/// file; Qodo PR #447). Best-effort: failures are logged, never
/// surfaced — the in-memory value on `NodeState` remains authoritative
/// for the session.
pub fn save(path: &Path, settings: &VineSettings) {
    let file = VineSettingsFile {
        version: FILE_VERSION,
        settings: settings.clone(),
    };
    let json = match serde_json::to_vec_pretty(&file) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!(error = %e, "vine settings serialize failed");
            return;
        }
    };
    if let Err(e) = crate::identity::write_atomic_0600(path, &json) {
        tracing::error!(error = %e, "vine settings write failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        use rand::Rng;
        let id: u64 = rand::thread_rng().gen();
        let dir = std::env::temp_dir().join(format!("harmony_vine_settings_test_{id}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn missing_file_defaults_to_sharing_on() {
        let path = settings_path(&temp_dir());
        assert!(load_or_default(&path).share_follows);
    }

    #[test]
    fn save_load_round_trip() {
        let path = settings_path(&temp_dir());
        save(
            &path,
            &VineSettings {
                share_follows: false,
                last_published_updated_at: 1_700_000_400,
                share_vines_publicly: false,
            },
        );
        let loaded = load_or_default(&path);
        assert!(!loaded.share_follows);
        assert_eq!(
            loaded.last_published_updated_at, 1_700_000_400,
            "LWW floor must survive restarts"
        );
        assert!(!loaded.share_vines_publicly);
        save(
            &path,
            &VineSettings {
                share_follows: true,
                last_published_updated_at: 0,
                share_vines_publicly: true,
            },
        );
        let loaded = load_or_default(&path);
        assert!(loaded.share_follows);
        assert!(loaded.share_vines_publicly);
    }

    #[test]
    fn legacy_file_without_floor_field_defaults_to_zero() {
        let dir = temp_dir();
        let path = settings_path(&dir);
        std::fs::write(&path, br#"{"version": 1, "share_follows": false}"#).unwrap();
        let loaded = load_or_default(&path);
        assert!(!loaded.share_follows);
        assert_eq!(loaded.last_published_updated_at, 0);
    }

    #[test]
    fn legacy_file_without_share_vines_publicly_defaults_true() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vine_settings.json");
        std::fs::write(&path, r#"{"version":1,"share_follows":false,"last_published_updated_at":7}"#).unwrap();
        let s = load_or_default(&path);
        assert!(!s.share_follows);
        assert!(s.share_vines_publicly, "legacy files must default the new gate ON");
    }

    #[test]
    fn corrupt_or_wrong_version_defaults() {
        let dir = temp_dir();
        let path = settings_path(&dir);
        std::fs::write(&path, b"not json").unwrap();
        assert!(load_or_default(&path).share_follows);

        std::fs::write(&path, br#"{"version": 99, "share_follows": false}"#).unwrap();
        assert!(load_or_default(&path).share_follows);
    }
}
