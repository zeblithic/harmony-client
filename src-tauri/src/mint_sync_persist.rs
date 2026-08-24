//! On-disk persistence for MintSyncState.
//! Atomic-rename + fsync via `tempfile`, mirroring owner_state_persist.
//!
//! `replay_tracker` is stored as a `BTreeMap<String, Hlc>` — the same
//! representation used by `owner_state_persist::save_replay`/`load_replay` —
//! because `RootReplayTracker` itself is not (de)serializable.

use crate::mint_sync_types::{MintSyncError, MintSyncState};
use std::path::Path;

/// File name for the persisted state. Lives at `<app_data_dir>/mint/mint_sync_state.cbor`.
pub const MINT_SYNC_STATE_FILENAME: &str = "mint_sync_state.cbor";

/// Load state from disk. Returns `Ok(default)` if the file doesn't exist yet.
///
/// ZEB-982: the file is device-sealed (v3 envelope); a legacy bare-CBOR file
/// (first byte is a map header, never the sentinel) still parses and is
/// eagerly re-sealed after a successful load. Envelope failures map onto the
/// existing hard-error contract: read I/O → `Io`, AEAD/content → `Cbor` —
/// both take the caller's disarm path (`break 'mint_init`), unchanged.
pub fn load(
    cipher: &crate::device_dataset_file::DeviceCipher,
    path: &Path,
) -> Result<MintSyncState, MintSyncError> {
    let image = match crate::device_dataset_file::read_image(cipher, path, MINT_SYNC_STATE_FILENAME)
    {
        Ok(None) => return Ok(MintSyncState::default()),
        Ok(Some(img)) => img,
        Err(crate::device_dataset_file::ImageError::Io(e)) => return Err(e.into()),
        Err(crate::device_dataset_file::ImageError::Crypto(e)) => {
            return Err(MintSyncError::Cbor(format!("load {}: {e}", path.display())))
        }
    };
    let state: MintSyncState = ciborium::from_reader(&image.bytes[..])
        .map_err(|e| MintSyncError::Cbor(format!("load {}: {e}", path.display())))?;
    if state.schema_version > crate::mint_sync_types::MINT_SCHEMA_VERSION {
        return Err(MintSyncError::SchemaTooNew {
            remote: state.schema_version,
            local_max: crate::mint_sync_types::MINT_SCHEMA_VERSION,
        });
    }
    crate::device_dataset_file::reseal_if_legacy(cipher, path, MINT_SYNC_STATE_FILENAME, &image);
    Ok(state)
}

/// Save state to disk via atomic-rename. Writes a tempfile in the
/// parent directory, fsyncs, then atomically renames over `<path>`.
///
/// **Caller contract:** this function is NOT internally synchronized.
/// Callers must serialize concurrent invocations on the same path
/// (e.g. via the engine's `TokioMutex` around `MintSyncState`).
/// Concurrent unprotected calls will race and the later `persist()` to
/// complete will silently clobber the earlier.
pub fn save(
    cipher: &crate::device_dataset_file::DeviceCipher,
    path: &Path,
    state: &MintSyncState,
) -> Result<(), MintSyncError> {
    let mut bytes = Vec::new();
    ciborium::into_writer(state, &mut bytes)
        .map_err(|e| MintSyncError::Cbor(format!("save {}: {e}", path.display())))?;
    crate::device_dataset_file::write_image(cipher, path, MINT_SYNC_STATE_FILENAME, &bytes)
        .map_err(MintSyncError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_returns_default_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MINT_SYNC_STATE_FILENAME);
        let state = load(&crate::device_dataset_file::test_cipher(), &path).unwrap();
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
        save(&crate::device_dataset_file::test_cipher(), &path, &state).unwrap();
        let loaded = load(&crate::device_dataset_file::test_cipher(), &path).unwrap();
        assert_eq!(state, loaded);
    }

    #[test]
    fn load_returns_schema_too_new_for_future_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MINT_SYNC_STATE_FILENAME);
        let future = MintSyncState {
            schema_version: 999,
            ..MintSyncState::default()
        };
        save(&crate::device_dataset_file::test_cipher(), &path, &future).unwrap();
        let result = load(&crate::device_dataset_file::test_cipher(), &path);
        assert!(
            matches!(
                result,
                Err(MintSyncError::SchemaTooNew {
                    remote: 999,
                    local_max: _,
                })
            ),
            "expected SchemaTooNew error, got: {result:?}"
        );
    }

    #[test]
    fn save_does_not_leave_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MINT_SYNC_STATE_FILENAME);
        save(
            &crate::device_dataset_file::test_cipher(),
            &path,
            &MintSyncState::default(),
        )
        .unwrap();
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
