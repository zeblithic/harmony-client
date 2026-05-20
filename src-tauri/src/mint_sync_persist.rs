//! On-disk persistence for MintSyncState.
//! Atomic-rename + fsync via `tempfile`, mirroring owner_state_persist.
//!
//! `replay_tracker` is stored as a `BTreeMap<String, Hlc>` — the same
//! representation used by `owner_state_persist::save_replay`/`load_replay` —
//! because `RootReplayTracker` itself is not (de)serializable.

use crate::mint_sync_types::{MintSyncError, MintSyncState};
use std::io::Write;
use std::path::Path;

/// File name for the persisted state. Lives at `<app_data_dir>/mint/mint_sync_state.cbor`.
pub const MINT_SYNC_STATE_FILENAME: &str = "mint_sync_state.cbor";

/// Load state from disk. Returns `Ok(default)` if the file doesn't exist yet.
pub fn load(path: &Path) -> Result<MintSyncState, MintSyncError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(MintSyncState::default());
        }
        Err(e) => return Err(e.into()),
    };
    let state: MintSyncState =
        ciborium::from_reader(&bytes[..]).map_err(|e| MintSyncError::Cbor(format!("load: {e}")))?;
    Ok(state)
}

/// Save state to disk via atomic-rename. Writes to a sibling tempfile,
/// fsyncs, then renames over `path`. The directory is created if absent.
pub fn save(path: &Path, state: &MintSyncState) -> Result<(), MintSyncError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut bytes = Vec::new();
    ciborium::into_writer(state, &mut bytes)
        .map_err(|e| MintSyncError::Cbor(format!("save: {e}")))?;
    let dir = path.parent().expect("save: path has no parent");
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(&bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .map_err(|e| MintSyncError::Io(std::io::Error::other(e)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_returns_default_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MINT_SYNC_STATE_FILENAME);
        let state = load(&path).unwrap();
        assert_eq!(state, MintSyncState::default());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MINT_SYNC_STATE_FILENAME);
        let mut state = MintSyncState::default();
        state
            .account_deletion_floor
            .insert("a1".into(), "2026-05-02T00:00:00Z".into());
        save(&path, &state).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(state, loaded);
    }

    #[test]
    fn save_does_not_leave_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MINT_SYNC_STATE_FILENAME);
        save(&path, &MintSyncState::default()).unwrap();
        // tempfile uses random names in the same dir, not a fixed .tmp suffix.
        // Verify no files remain other than the target file.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "only the final file should exist; found: {entries:?}"
        );
        assert_eq!(entries[0].to_str().unwrap(), MINT_SYNC_STATE_FILENAME);
    }
}
