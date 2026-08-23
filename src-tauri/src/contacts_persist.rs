//! On-disk persistence for ContactsDoc and its replay tracker (ZEB-977).
//! Line-for-line mirror of `notes_persist.rs`: atomic-rename + file fsync +
//! parent-dir fsync via `owner_state_persist::save_atomically`, 1-byte schema
//! version prefix, plaintext CBOR (same at-rest posture as notes.cbor and the
//! owner-state CRDT), corrupt-file quarantine, and the ZEB-460
//! transient-vs-corrupt recovery contract.

use crate::contacts_crdt::ContactsDoc;
use crate::fleet_sync::SyncError;
use crate::owner_state_types::Hlc;
use ciborium::{from_reader, into_writer};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::Path;

/// File name for the persisted ContactsDoc. Lives in the identity dir beside
/// `notes.cbor`.
pub const CONTACTS_FILENAME: &str = "contacts.cbor";

/// File name for the persisted replay tracker. Lives alongside `contacts.cbor`.
pub const CONTACTS_REPLAY_FILENAME: &str = "contacts_replay.cbor";

const CONTACTS_SCHEMA_V1: u8 = 1;
const CONTACTS_REPLAY_SCHEMA_V1: u8 = 1;

#[derive(Serialize, Deserialize)]
struct ContactsFileV1(ContactsDoc);

#[derive(Serialize, Deserialize)]
struct ContactsReplayFileV1(BTreeMap<String, Hlc>);

// ── helpers ──────────────────────────────────────────────────────────────────

/// Atomic write with parent-directory fsync (crash-durable rename), via
/// `owner_state_persist::save_atomically`.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), SyncError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| SyncError::Persist(format!("create_dir_all {}: {e}", path.display())))?;
    }
    crate::owner_state_persist::save_atomically(path, bytes)
        .map_err(|e| SyncError::Persist(e.to_string()))
}

// ── ContactsDoc ──────────────────────────────────────────────────────────────

/// Load `ContactsDoc` from `path`. Returns `Ok(ContactsDoc::default())` if the
/// file does not exist yet.
pub fn load(path: &Path) -> Result<ContactsDoc, SyncError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ContactsDoc::default()),
        Err(e) => return Err(SyncError::Persist(format!("read {}: {e}", path.display()))),
    };
    if bytes.is_empty() {
        return Err(SyncError::CborDecode(format!(
            "contacts file is empty: {}",
            path.display()
        )));
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    match version {
        CONTACTS_SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let file: ContactsFileV1 = from_reader(&mut cursor)
                .map_err(|e| SyncError::CborDecode(format!("load {}: {e}", path.display())))?;
            // Reject trailing bytes after the CBOR value (mirrors
            // owner_state_crypto::canonical_cbor_decode): valid-prefix +
            // garbage must NOT decode as "valid".
            let pos = cursor.position() as usize;
            if pos != payload.len() {
                return Err(SyncError::CborDecode(format!(
                    "trailing bytes after contacts value: consumed {} of {}",
                    pos,
                    payload.len()
                )));
            }
            Ok(file.0)
        }
        v => Err(SyncError::CborDecode(format!(
            "unknown contacts schema version {v:#x} in {}",
            path.display()
        ))),
    }
}

/// Load the contacts doc, recovering from genuine on-disk corruption.
///
/// - `Ok(doc)` on success (or `Ok(default)` when the file is missing).
/// - `Err(SyncError::CborDecode)` (permanent corruption) → quarantine the bad
///   file aside (`.corrupt-<ms>`, bytes preserved) and self-heal to
///   `Ok(default())` so the app still boots.
/// - any other error — a transient I/O failure (`SyncError::Persist`) → the
///   file is left untouched and the error propagated (ZEB-460); quarantining
///   would orphan real data and let the next persist overwrite it.
pub fn load_doc_or_recover(path: &Path) -> Result<ContactsDoc, SyncError> {
    match load(path) {
        Ok(doc) => Ok(doc),
        Err(e @ SyncError::CborDecode(_)) => {
            quarantine(path, &e);
            Ok(ContactsDoc::default())
        }
        Err(e) => Err(e),
    }
}

/// Same recovery contract as [`load_doc_or_recover`], for the replay tracker.
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
    // Timestamped `.corrupt-<ms>` suffix: never clobbers a prior quarantine or
    // the live file; preserves the bytes for manual recovery.
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut corrupt = path.as_os_str().to_os_string();
    corrupt.push(format!(".corrupt-{ms}"));
    tracing::error!(path = %path.display(), error = %err,
        "contacts persistence load failed; quarantining corrupt file and starting fresh (bytes preserved)");
    if let Err(re) = std::fs::rename(path, &corrupt) {
        tracing::warn!(path = %path.display(), error = %re, "failed to quarantine corrupt contacts file");
    }
}

/// Save `ContactsDoc` to `path` atomically (tempfile + fsync + parent-dir
/// fsync + rename). Creates parent directories if needed.
pub fn save(path: &Path, doc: &ContactsDoc) -> Result<(), SyncError> {
    let mut bytes = vec![CONTACTS_SCHEMA_V1];
    into_writer(&ContactsFileV1(doc.clone()), &mut bytes)
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
            "contacts replay file is empty: {}",
            path.display()
        )));
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    match version {
        CONTACTS_REPLAY_SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let file: ContactsReplayFileV1 = from_reader(&mut cursor).map_err(|e| {
                SyncError::CborDecode(format!("load_replay {}: {e}", path.display()))
            })?;
            // Reject trailing bytes after the CBOR value.
            let pos = cursor.position() as usize;
            if pos != payload.len() {
                return Err(SyncError::CborDecode(format!(
                    "trailing bytes after contacts value: consumed {} of {}",
                    pos,
                    payload.len()
                )));
            }
            Ok(file.0)
        }
        v => Err(SyncError::CborDecode(format!(
            "unknown contacts replay schema version {v:#x} in {}",
            path.display()
        ))),
    }
}

/// Save the replay tracker to `path` atomically.
pub fn save_replay(path: &Path, tracker: &BTreeMap<String, Hlc>) -> Result<(), SyncError> {
    let mut bytes = vec![CONTACTS_REPLAY_SCHEMA_V1];
    into_writer(&ContactsReplayFileV1(tracker.clone()), &mut bytes)
        .map_err(|e| SyncError::CborEncode(format!("encode replay {}: {e}", path.display())))?;
    atomic_write(path, &bytes)
}

// ── FleetPersist impl ─────────────────────────────────────────────────────────

/// Durability sink for the contacts fleet-sync engine. Holds the absolute
/// paths for both the doc and replay-tracker files.
pub struct ContactsPersist {
    pub doc_path: std::path::PathBuf,
    pub replay_path: std::path::PathBuf,
}

impl crate::fleet_sync::FleetPersist<ContactsDoc> for ContactsPersist {
    fn persist(
        &self,
        state: &ContactsDoc,
        tracker: &BTreeMap<String, Hlc>,
    ) -> Result<(), SyncError> {
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

    fn hlc(w: u64, d: &str) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: d.into(),
        }
    }

    fn sample_doc() -> ContactsDoc {
        let mut doc = ContactsDoc::default();
        doc.apply_annotation("aa", Some(Some("Koya".into())), None, hlc(1, "A"), 7);
        doc
    }

    #[test]
    fn doc_round_trips_and_missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contacts.cbor");
        assert_eq!(load(&path).unwrap(), ContactsDoc::default());
        let doc = sample_doc();
        save(&path, &doc).unwrap();
        assert_eq!(load(&path).unwrap(), doc);
    }

    #[test]
    fn load_rejects_trailing_bytes_after_valid_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contacts.cbor");
        let doc = sample_doc();
        save(&path, &doc).unwrap();
        assert_eq!(load(&path).unwrap(), doc);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(0xFF);
        std::fs::write(&path, &bytes).unwrap();
        let err = load(&path).unwrap_err();
        assert!(
            matches!(err, SyncError::CborDecode(_)),
            "trailing bytes must surface CborDecode, got {err:?}"
        );
        let recovered = load_doc_or_recover(&path).unwrap();
        assert_eq!(recovered, ContactsDoc::default());
        assert!(!path.exists(), "corrupt file was quarantined");
    }

    #[test]
    fn replay_round_trips_and_missing_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contacts_replay.cbor");
        assert!(load_replay(&path).unwrap().is_empty());
        let mut t = std::collections::BTreeMap::new();
        t.insert("A".to_string(), hlc(9, "A"));
        save_replay(&path, &t).unwrap();
        assert_eq!(load_replay(&path).unwrap(), t);
    }

    #[test]
    fn load_doc_or_recover_quarantines_corrupt_and_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contacts.cbor");
        std::fs::write(&path, [0xFF_u8, 0x01, 0x02]).unwrap(); // unknown schema
        let doc = load_doc_or_recover(&path).unwrap();
        assert_eq!(doc, ContactsDoc::default(), "recovers to a fresh empty doc");
        assert!(
            !path.exists(),
            "corrupt file moved aside, not left in place"
        );
        let quarantined: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("contacts.cbor.corrupt-")
            })
            .collect();
        assert_eq!(quarantined.len(), 1, "exactly one quarantine file");
        assert_eq!(
            std::fs::read(quarantined[0].path()).unwrap(),
            vec![0xFF_u8, 0x01, 0x02],
            "quarantined bytes preserved verbatim"
        );
    }

    #[test]
    fn load_doc_or_recover_missing_is_default_no_quarantine() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contacts.cbor");
        assert_eq!(load_doc_or_recover(&path).unwrap(), ContactsDoc::default());
        let any: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(any.is_empty(), "no quarantine on a missing file");
    }

    #[test]
    fn load_doc_or_recover_propagates_transient_io_without_quarantine() {
        // ZEB-460: transient I/O must surface Err untouched — quarantining
        // would orphan real data and let the next persist overwrite it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.cbor");
        std::fs::create_dir(&path).unwrap(); // read on a dir = non-NotFound error
        let err = load_doc_or_recover(&path).unwrap_err();
        assert!(
            matches!(err, SyncError::Persist(_)),
            "transient I/O must surface Persist, got {err:?}"
        );
        assert!(path.is_dir(), "file left untouched, not quarantined");
        let quarantined: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
            .collect();
        assert!(quarantined.is_empty(), "no quarantine on a transient error");
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
        assert!(path.is_dir(), "replay file left untouched, not quarantined");
    }

    #[test]
    fn load_replay_or_recover_quarantines_corrupt_and_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contacts_replay.cbor");
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
                    .contains("contacts_replay.cbor.corrupt-")
            })
            .collect();
        assert_eq!(quarantined.len(), 1, "exactly one quarantine file");
    }

    #[test]
    fn contacts_persist_writes_both_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = ContactsPersist {
            doc_path: dir.path().join("contacts.cbor"),
            replay_path: dir.path().join("contacts_replay.cbor"),
        };
        use crate::fleet_sync::FleetPersist;
        let doc = sample_doc();
        let mut t = std::collections::BTreeMap::new();
        t.insert("A".to_string(), hlc(1, "A"));
        p.persist(&doc, &t).unwrap();
        assert_eq!(load(&p.doc_path).unwrap(), doc);
        assert_eq!(load_replay(&p.replay_path).unwrap(), t);
    }
}
