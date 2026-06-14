//! On-disk persistence for NotesDoc and its replay tracker (ZEB-417 SP1).
//! Atomic-rename + file fsync + parent-dir fsync via
//! `owner_state_persist::save_atomically`, mirroring `owner_state_persist`.
//!
//! Both files use a 1-byte schema-version prefix (plaintext CBOR) — identical
//! format to `owner_state_persist`'s `ReplayFileV1`. `NotesDoc` is not
//! encrypted at rest (owner-state CRDT is also plaintext CBOR on disk).

use crate::fleet_sync::SyncError;
use crate::notes_crdt::NotesDoc;
use crate::owner_state_types::Hlc;
use ciborium::{from_reader, into_writer};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::Path;

/// File name for the persisted NotesDoc. Lives at `<app_data_dir>/notes/notes.cbor`.
pub const NOTES_FILENAME: &str = "notes.cbor";

/// File name for the persisted replay tracker. Lives alongside `notes.cbor`.
pub const NOTES_REPLAY_FILENAME: &str = "notes_replay.cbor";

const NOTES_SCHEMA_V1: u8 = 1;
const NOTES_REPLAY_SCHEMA_V1: u8 = 1;

#[derive(Serialize, Deserialize)]
struct NotesFileV1(NotesDoc);

#[derive(Serialize, Deserialize)]
struct NotesReplayFileV1(BTreeMap<String, Hlc>);

// ── helpers ──────────────────────────────────────────────────────────────────

/// Atomic write with parent-directory fsync (crash-durable rename). Routes
/// through `owner_state_persist::save_atomically`, which fsyncs both the
/// tempfile and (on Unix) the parent directory entry — the hand-rolled
/// version here did not fsync the directory, so the rename wasn't durable.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), SyncError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| SyncError::Persist(format!("create_dir_all {}: {e}", path.display())))?;
    }
    crate::owner_state_persist::save_atomically(path, bytes)
        .map_err(|e| SyncError::Persist(e.to_string()))
}

// ── NotesDoc ─────────────────────────────────────────────────────────────────

/// Load `NotesDoc` from `path`. Returns `Ok(NotesDoc::default())` if the file
/// does not exist yet.
pub fn load(path: &Path) -> Result<NotesDoc, SyncError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(NotesDoc::default()),
        Err(e) => return Err(SyncError::Persist(format!("read {}: {e}", path.display()))),
    };
    if bytes.is_empty() {
        return Err(SyncError::CborDecode(format!(
            "notes file is empty: {}",
            path.display()
        )));
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    match version {
        NOTES_SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let file: NotesFileV1 = from_reader(&mut cursor)
                .map_err(|e| SyncError::CborDecode(format!("load {}: {e}", path.display())))?;
            // Reject trailing bytes after the CBOR value (mirrors
            // owner_state_crypto::canonical_cbor_decode): a corrupt file that is
            // valid-prefix + garbage must NOT decode as "valid".
            let pos = cursor.position() as usize;
            if pos != payload.len() {
                return Err(SyncError::CborDecode(format!(
                    "trailing bytes after notes value: consumed {} of {}",
                    pos,
                    payload.len()
                )));
            }
            Ok(file.0)
        }
        v => Err(SyncError::CborDecode(format!(
            "unknown notes schema version {v:#x} in {}",
            path.display()
        ))),
    }
}

/// Load the notes doc, recovering from genuine on-disk corruption.
///
/// - `Ok(doc)` on success (or `Ok(default)` when the file is missing).
/// - `Err(SyncError::CborDecode)` (permanent corruption) → quarantine the bad
///   file aside (`.corrupt-<ms>`, bytes preserved) and self-heal to
///   `Ok(default())` so the app still boots and never bricks on corruption.
/// - any other error — a transient I/O failure (`SyncError::Persist`) → the file
///   is left untouched and the error is propagated (ZEB-460); quarantining it
///   would orphan the user's real notes and let the next persist overwrite them
///   with an empty doc, so the caller fails the boot loudly and retries next
///   launch with the file intact.
pub fn load_doc_or_recover(path: &Path) -> Result<NotesDoc, SyncError> {
    match load(path) {
        Ok(doc) => Ok(doc),
        Err(e @ SyncError::CborDecode(_)) => {
            quarantine(path, &e);
            Ok(NotesDoc::default())
        }
        Err(e) => Err(e),
    }
}

/// Same recovery contract as [`load_doc_or_recover`], but for the replay
/// tracker: `CborDecode` corruption is quarantined and an empty tracker
/// returned; a transient `Persist` error is left untouched and propagated
/// (ZEB-460).
pub fn load_replay_or_recover(path: &Path) -> Result<BTreeMap<String, Hlc>, SyncError> {
    match load_replay(path) {
        Ok(t) => Ok(t),
        Err(e @ SyncError::CborDecode(_)) => {
            quarantine(path, &e);
            Ok(BTreeMap::new())
        }
        Err(e) => Err(e),
    }
}

fn quarantine(path: &Path, err: &SyncError) {
    // Append a timestamped `.corrupt-<ms>` suffix so we never clobber a prior
    // quarantine or the live file; preserves the bytes for manual recovery.
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut corrupt = path.as_os_str().to_os_string();
    corrupt.push(format!(".corrupt-{ms}"));
    tracing::error!(path = %path.display(), error = %err,
        "notes persistence load failed; quarantining corrupt file and starting fresh (bytes preserved)");
    if let Err(re) = std::fs::rename(path, &corrupt) {
        tracing::warn!(path = %path.display(), error = %re, "failed to quarantine corrupt notes file");
    }
}

/// Save `NotesDoc` to `path` atomically (tempfile + fsync + parent-dir fsync
/// + rename). Creates parent directories if needed.
pub fn save(path: &Path, doc: &NotesDoc) -> Result<(), SyncError> {
    let mut bytes = vec![NOTES_SCHEMA_V1];
    into_writer(&NotesFileV1(doc.clone()), &mut bytes)
        .map_err(|e| SyncError::CborEncode(format!("encode {}: {e}", path.display())))?;
    atomic_write(path, &bytes)
}

// ── Replay tracker ────────────────────────────────────────────────────────────

/// Load the replay tracker from `path`. Returns `Ok(BTreeMap::new())` if the
/// file does not exist yet.
pub fn load_replay(path: &Path) -> Result<BTreeMap<String, Hlc>, SyncError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(SyncError::Persist(format!("read {}: {e}", path.display()))),
    };
    if bytes.is_empty() {
        return Err(SyncError::CborDecode(format!(
            "notes replay file is empty: {}",
            path.display()
        )));
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    match version {
        NOTES_REPLAY_SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let file: NotesReplayFileV1 = from_reader(&mut cursor).map_err(|e| {
                SyncError::CborDecode(format!("load_replay {}: {e}", path.display()))
            })?;
            // Reject trailing bytes after the CBOR value (mirrors
            // owner_state_crypto::canonical_cbor_decode).
            let pos = cursor.position() as usize;
            if pos != payload.len() {
                return Err(SyncError::CborDecode(format!(
                    "trailing bytes after notes value: consumed {} of {}",
                    pos,
                    payload.len()
                )));
            }
            Ok(file.0)
        }
        v => Err(SyncError::CborDecode(format!(
            "unknown notes replay schema version {v:#x} in {}",
            path.display()
        ))),
    }
}

/// Save the replay tracker to `path` atomically.
pub fn save_replay(path: &Path, tracker: &BTreeMap<String, Hlc>) -> Result<(), SyncError> {
    let mut bytes = vec![NOTES_REPLAY_SCHEMA_V1];
    into_writer(&NotesReplayFileV1(tracker.clone()), &mut bytes)
        .map_err(|e| SyncError::CborEncode(format!("encode replay {}: {e}", path.display())))?;
    atomic_write(path, &bytes)
}

// ── FleetPersist impl ─────────────────────────────────────────────────────────

/// Durability sink for the notes fleet-sync engine. Holds the absolute paths
/// for both the doc and replay-tracker files.
pub struct NotesPersist {
    pub doc_path: std::path::PathBuf,
    pub replay_path: std::path::PathBuf,
}

impl crate::fleet_sync::FleetPersist<NotesDoc> for NotesPersist {
    fn persist(&self, state: &NotesDoc, tracker: &BTreeMap<String, Hlc>) -> Result<(), SyncError> {
        save(&self.doc_path, state)?;
        save_replay(&self.replay_path, tracker)?;
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::Hlc;

    #[test]
    fn doc_round_trips_and_missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.cbor");
        assert_eq!(load(&path).unwrap(), crate::notes_crdt::NotesDoc::default());
        let mut doc = crate::notes_crdt::NotesDoc::default();
        doc.upsert(
            "n1".into(),
            "hi".into(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "A".into(),
            },
        );
        save(&path, &doc).unwrap();
        assert_eq!(load(&path).unwrap(), doc);
    }

    #[test]
    fn load_rejects_trailing_bytes_after_valid_value() {
        // A valid saved file with a stray byte appended must NOT decode as
        // "valid" — it should surface an Err so load_doc_or_recover quarantines
        // it (mirrors owner_state_crypto::canonical_cbor_decode strictness).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.cbor");
        let mut doc = crate::notes_crdt::NotesDoc::default();
        doc.upsert(
            "n1".into(),
            "hi".into(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "A".into(),
            },
        );
        save(&path, &doc).unwrap();
        // Sanity: the clean file loads.
        assert_eq!(load(&path).unwrap(), doc);
        // Append a stray byte after the valid CBOR value.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(0xFF);
        std::fs::write(&path, &bytes).unwrap();
        let err = load(&path).unwrap_err();
        assert!(
            matches!(err, SyncError::CborDecode(_)),
            "trailing bytes must surface CborDecode, got {err:?}"
        );
        // And recovery quarantines it rather than silently starting fresh-on-write.
        let recovered = load_doc_or_recover(&path).unwrap();
        assert_eq!(recovered, NotesDoc::default());
        assert!(!path.exists(), "corrupt file was quarantined");
    }

    #[test]
    fn replay_round_trips_and_missing_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes_replay.cbor");
        assert!(load_replay(&path).unwrap().is_empty());
        let mut t = std::collections::BTreeMap::new();
        t.insert(
            "A".to_string(),
            Hlc {
                wall_ms: 9,
                logical: 1,
                device_id: "A".into(),
            },
        );
        save_replay(&path, &t).unwrap();
        assert_eq!(load_replay(&path).unwrap(), t);
    }

    #[test]
    fn load_doc_or_recover_quarantines_corrupt_and_starts_fresh() {
        // A corrupt notes file must NOT silently become an empty doc that
        // overwrites the user's notes on the next persist: the recovery path
        // renames the bad bytes aside and returns a fresh default.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.cbor");
        // Unknown schema version byte → load() returns Err(CborDecode).
        std::fs::write(&path, [0xFF_u8, 0x01, 0x02]).unwrap();
        let doc = load_doc_or_recover(&path).unwrap();
        assert_eq!(doc, NotesDoc::default(), "recovers to a fresh empty doc");
        assert!(
            !path.exists(),
            "the corrupt file is moved aside, not left in place"
        );
        let quarantined: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("notes.cbor.corrupt-")
            })
            .collect();
        assert_eq!(quarantined.len(), 1, "exactly one quarantine file");
        assert_eq!(
            std::fs::read(quarantined[0].path()).unwrap(),
            vec![0xFF_u8, 0x01, 0x02],
            "quarantined bytes are preserved verbatim"
        );
    }

    #[test]
    fn load_doc_or_recover_missing_is_default_no_quarantine() {
        // NotFound is NOT a corruption: load() returns Ok(default), so no
        // quarantine file should be created.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.cbor");
        assert_eq!(load_doc_or_recover(&path).unwrap(), NotesDoc::default());
        let any: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(any.is_empty(), "no quarantine on a missing file");
    }

    #[test]
    fn load_doc_or_recover_propagates_transient_io_without_quarantine() {
        // A transient I/O error (NOT corruption) must surface as Err and must
        // NOT be quarantined: quarantining would orphan real data and let the
        // next persist overwrite it with an empty doc (ZEB-460). Force a
        // SyncError::Persist by pointing load() at a directory — std::fs::read
        // on a dir returns a non-NotFound error.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.cbor");
        std::fs::create_dir(&path).unwrap();
        let err = load_doc_or_recover(&path).unwrap_err();
        assert!(
            matches!(err, SyncError::Persist(_)),
            "transient I/O must surface Persist, got {err:?}"
        );
        assert!(path.is_dir(), "the file is left untouched, not quarantined");
        let quarantined: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
            .collect();
        assert!(
            quarantined.is_empty(),
            "a transient error must not create a quarantine file"
        );
    }

    #[test]
    fn load_replay_or_recover_propagates_transient_io_without_quarantine() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replay.cbor");
        std::fs::create_dir(&path).unwrap();
        let err = load_replay_or_recover(&path).unwrap_err();
        assert!(
            matches!(err, SyncError::Persist(_)),
            "transient I/O must surface Persist, got {err:?}"
        );
        assert!(
            path.is_dir(),
            "the replay file is left untouched, not quarantined"
        );
    }

    #[test]
    fn load_replay_or_recover_quarantines_corrupt_and_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes_replay.cbor");
        std::fs::write(&path, [0xFF_u8]).unwrap(); // unknown schema version
        let tracker = load_replay_or_recover(&path).unwrap();
        assert!(tracker.is_empty(), "recovers to an empty tracker");
        assert!(!path.exists(), "corrupt replay file moved aside");
        let quarantined: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("notes_replay.cbor.corrupt-")
            })
            .collect();
        assert_eq!(quarantined.len(), 1, "exactly one quarantine file");
    }

    #[test]
    fn notes_persist_writes_both_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = NotesPersist {
            doc_path: dir.path().join("notes.cbor"),
            replay_path: dir.path().join("notes_replay.cbor"),
        };
        use crate::fleet_sync::FleetPersist;
        let mut doc = crate::notes_crdt::NotesDoc::default();
        doc.upsert(
            "n1".into(),
            "hi".into(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "A".into(),
            },
        );
        let mut t = std::collections::BTreeMap::new();
        t.insert(
            "A".to_string(),
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "A".into(),
            },
        );
        p.persist(&doc, &t).unwrap();
        assert_eq!(load(&p.doc_path).unwrap(), doc);
        assert_eq!(load_replay(&p.replay_path).unwrap(), t);
    }
}
