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
            Ok(file.0)
        }
        v => Err(SyncError::CborDecode(format!(
            "unknown notes schema version {v:#x} in {}",
            path.display()
        ))),
    }
}

/// Load the notes doc, or — on a corruption/IO error (NOT NotFound) — log
/// loudly, quarantine the bad file (renamed aside, never overwritten), and
/// start fresh. Prevents the silent-data-loss path where a load error becomes
/// an empty doc that the next persist writes over the user's real notes.
pub fn load_doc_or_recover(path: &Path) -> NotesDoc {
    match load(path) {
        Ok(doc) => doc,
        Err(e) => {
            quarantine(path, &e);
            NotesDoc::default()
        }
    }
}

/// Same recovery contract as [`load_doc_or_recover`], but for the replay
/// tracker: on a corruption/IO error (NOT NotFound) the bad file is
/// quarantined and an empty tracker returned, never silently overwritten.
pub fn load_replay_or_recover(path: &Path) -> BTreeMap<String, Hlc> {
    match load_replay(path) {
        Ok(t) => t,
        Err(e) => {
            quarantine(path, &e);
            BTreeMap::new()
        }
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
        .map_err(|e| SyncError::CborDecode(format!("encode {}: {e}", path.display())))?;
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
        .map_err(|e| SyncError::CborDecode(format!("encode replay {}: {e}", path.display())))?;
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
        let doc = load_doc_or_recover(&path);
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
        assert_eq!(load_doc_or_recover(&path), NotesDoc::default());
        let any: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(any.is_empty(), "no quarantine on a missing file");
    }

    #[test]
    fn load_replay_or_recover_quarantines_corrupt_and_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes_replay.cbor");
        std::fs::write(&path, [0xFF_u8]).unwrap(); // unknown schema version
        let tracker = load_replay_or_recover(&path);
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
