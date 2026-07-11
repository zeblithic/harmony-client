//! ZEB-671: persisted vine feature settings — currently just
//! `share_follows`, the public + opt-out switch for follow-list wire
//! publication (Jake's call on ZEB-671: publish by default, one toggle
//! to stop publishing and retract).
//!
//! Same atomic-write posture as `follows.rs`: write a temp file next to
//! the target, then rename. Missing/corrupt/wrong-version files load as
//! defaults.

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
}

impl Default for VineSettings {
    fn default() -> Self {
        VineSettings {
            share_follows: true,
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

/// Atomically persist `settings` to `path` (temp file + rename).
/// Best-effort: failures are logged, never surfaced — the in-memory
/// value on `NodeState` remains authoritative for the session.
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
    let tmp_path = {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".tmp");
        path.with_file_name(name)
    };
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&tmp_path, &json) {
        Ok(()) => {
            if let Err(e) = std::fs::rename(&tmp_path, path) {
                tracing::error!(error = %e, "vine settings rename failed");
            }
        }
        Err(e) => tracing::error!(error = %e, "vine settings write failed"),
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
            },
        );
        assert!(!load_or_default(&path).share_follows);
        save(
            &path,
            &VineSettings {
                share_follows: true,
            },
        );
        assert!(load_or_default(&path).share_follows);
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
