//! ZEB-495 (ZEB-340 Part 2) Unit 5b: on-disk persistence for
//! `CommunityDeviceIntroDoc` and its replay tracker. Mirrors `notes_persist` /
//! `dm_inbox_persist` exactly: atomic-rename + file fsync + parent-dir fsync via
//! `owner_state_persist::save_atomically`, a 1-byte schema-version prefix
//! (plaintext CBOR), trailing-byte rejection, and the CborDecode-quarantine
//! recovery contract (corrupt → move aside + start fresh; transient I/O →
//! propagate untouched, ZEB-460).

use crate::community_device_intro_crdt::CommunityDeviceIntroDoc;
use crate::fleet_sync::SyncError;
use crate::owner_state_types::Hlc;
use ciborium::{from_reader, into_writer};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::Path;

/// File name for the persisted doc. Lives at
/// `<identity_dir>/community_device_intro.cbor`.
pub const FILENAME: &str = "community_device_intro.cbor";

/// File name for the persisted replay tracker. Lives alongside the doc.
pub const REPLAY_FILENAME: &str = "community_device_intro_replay.cbor";

const SCHEMA_V1: u8 = 1;
const REPLAY_SCHEMA_V1: u8 = 1;

#[derive(Serialize, Deserialize)]
struct DocFileV1(CommunityDeviceIntroDoc);

#[derive(Serialize, Deserialize)]
struct ReplayFileV1(BTreeMap<String, Hlc>);

// ── helpers ──────────────────────────────────────────────────────────────────

/// Atomic write with parent-directory fsync (crash-durable rename) — routes
/// through `owner_state_persist::save_atomically`.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), SyncError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| SyncError::Persist(format!("create_dir_all {}: {e}", path.display())))?;
    }
    crate::owner_state_persist::save_atomically(path, bytes)
        .map_err(|e| SyncError::Persist(e.to_string()))
}

// ── doc ──────────────────────────────────────────────────────────────────────

/// Load the doc from `path`. Returns `Ok(default())` if the file does not
/// exist yet.
pub fn load(path: &Path) -> Result<CommunityDeviceIntroDoc, SyncError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CommunityDeviceIntroDoc::default())
        }
        Err(e) => return Err(SyncError::Persist(format!("read {}: {e}", path.display()))),
    };
    if bytes.is_empty() {
        return Err(SyncError::CborDecode(format!(
            "community-device-intro file is empty: {}",
            path.display()
        )));
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    match version {
        SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let file: DocFileV1 = from_reader(&mut cursor)
                .map_err(|e| SyncError::CborDecode(format!("load {}: {e}", path.display())))?;
            let pos = cursor.position() as usize;
            if pos != payload.len() {
                return Err(SyncError::CborDecode(format!(
                    "trailing bytes after community-device-intro value: consumed {} of {}",
                    pos,
                    payload.len()
                )));
            }
            Ok(file.0)
        }
        v => Err(SyncError::CborDecode(format!(
            "unknown community-device-intro schema version {v:#x} in {}",
            path.display()
        ))),
    }
}

/// Load the doc, recovering from genuine on-disk corruption: `CborDecode`
/// quarantines the bad file (`.corrupt-<ms>`, bytes preserved) and returns a
/// fresh default; any other (transient I/O) error is left untouched and
/// propagated (ZEB-460). Same contract as `notes_persist::load_doc_or_recover`.
pub fn load_doc_or_recover(path: &Path) -> Result<CommunityDeviceIntroDoc, SyncError> {
    match load(path) {
        Ok(doc) => Ok(doc),
        Err(e @ SyncError::CborDecode(_)) => {
            quarantine(path, &e);
            Ok(CommunityDeviceIntroDoc::default())
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
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut corrupt = path.as_os_str().to_os_string();
    corrupt.push(format!(".corrupt-{ms}"));
    tracing::error!(path = %path.display(), error = %err,
        "community-device-intro load failed; quarantining corrupt file and starting fresh (bytes preserved)");
    if let Err(re) = std::fs::rename(path, &corrupt) {
        tracing::warn!(path = %path.display(), error = %re, "failed to quarantine corrupt community-device-intro file");
    }
}

/// Save the doc to `path` atomically (tempfile + fsync + parent-dir fsync +
/// rename). Creates parent directories if needed.
pub fn save(path: &Path, doc: &CommunityDeviceIntroDoc) -> Result<(), SyncError> {
    let mut bytes = vec![SCHEMA_V1];
    into_writer(&DocFileV1(doc.clone()), &mut bytes)
        .map_err(|e| SyncError::CborEncode(format!("encode {}: {e}", path.display())))?;
    atomic_write(path, &bytes)
}

// ── replay tracker ───────────────────────────────────────────────────────────

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
            "community-device-intro replay file is empty: {}",
            path.display()
        )));
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    match version {
        REPLAY_SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let file: ReplayFileV1 = from_reader(&mut cursor).map_err(|e| {
                SyncError::CborDecode(format!("load_replay {}: {e}", path.display()))
            })?;
            let pos = cursor.position() as usize;
            if pos != payload.len() {
                return Err(SyncError::CborDecode(format!(
                    "trailing bytes after community-device-intro replay value: consumed {} of {}",
                    pos,
                    payload.len()
                )));
            }
            Ok(file.0)
        }
        v => Err(SyncError::CborDecode(format!(
            "unknown community-device-intro replay schema version {v:#x} in {}",
            path.display()
        ))),
    }
}

/// Save the replay tracker to `path` atomically.
pub fn save_replay(path: &Path, tracker: &BTreeMap<String, Hlc>) -> Result<(), SyncError> {
    let mut bytes = vec![REPLAY_SCHEMA_V1];
    into_writer(&ReplayFileV1(tracker.clone()), &mut bytes)
        .map_err(|e| SyncError::CborEncode(format!("encode replay {}: {e}", path.display())))?;
    atomic_write(path, &bytes)
}

// ── FleetPersist impl ────────────────────────────────────────────────────────

/// Durability sink for the community-device-intro fleet-sync engine. Holds the
/// absolute paths for both the doc and replay-tracker files.
pub struct CommunityDeviceIntroPersist {
    pub doc_path: std::path::PathBuf,
    pub replay_path: std::path::PathBuf,
}

impl crate::fleet_sync::FleetPersist<CommunityDeviceIntroDoc> for CommunityDeviceIntroPersist {
    fn persist(
        &self,
        state: &CommunityDeviceIntroDoc,
        tracker: &BTreeMap<String, Hlc>,
    ) -> Result<(), SyncError> {
        save(&self.doc_path, state)?;
        save_replay(&self.replay_path, tracker)?;
        Ok(())
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_device_intro_crdt::{CommunityDeviceIntroDoc, CommunityDeviceIntroEntry};
    use crate::owner_state_types::SpaceId;
    use std::collections::BTreeSet;

    fn sample_doc() -> CommunityDeviceIntroDoc {
        let mut doc = CommunityDeviceIntroDoc::default();
        let mut relayed = BTreeSet::new();
        relayed.insert("dev-a".to_string());
        doc.entries.insert(
            CommunityDeviceIntroDoc::key(&SpaceId([1; 16]), "device2-64hex"),
            CommunityDeviceIntroEntry {
                signed_event: vec![1, 2, 3, 4],
                community_id: SpaceId([1; 16]),
                deposited_at: Hlc {
                    wall_ms: 9,
                    logical: 0,
                    device_id: "device2".into(),
                },
                relayed_by: relayed,
            },
        );
        doc
    }

    #[test]
    fn doc_round_trips_and_missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILENAME);
        assert_eq!(load(&path).unwrap(), CommunityDeviceIntroDoc::default());
        let doc = sample_doc();
        save(&path, &doc).unwrap();
        assert_eq!(load(&path).unwrap(), doc);
    }

    #[test]
    fn load_rejects_trailing_bytes_after_valid_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILENAME);
        save(&path, &sample_doc()).unwrap();
        assert_eq!(load(&path).unwrap(), sample_doc());
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(0xFF);
        std::fs::write(&path, &bytes).unwrap();
        let err = load(&path).unwrap_err();
        assert!(
            matches!(err, SyncError::CborDecode(_)),
            "trailing bytes must surface CborDecode, got {err:?}"
        );
        // Recovery quarantines it rather than silently starting fresh-on-write.
        let recovered = load_doc_or_recover(&path).unwrap();
        assert_eq!(recovered, CommunityDeviceIntroDoc::default());
        assert!(!path.exists(), "corrupt file was quarantined");
    }

    #[test]
    fn replay_round_trips_and_missing_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(REPLAY_FILENAME);
        assert!(load_replay(&path).unwrap().is_empty());
        let mut t = BTreeMap::new();
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILENAME);
        std::fs::write(&path, [0xFF_u8, 0x01, 0x02]).unwrap(); // unknown schema version
        let doc = load_doc_or_recover(&path).unwrap();
        assert_eq!(doc, CommunityDeviceIntroDoc::default());
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
                    .contains("community_device_intro.cbor.corrupt-")
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
    fn load_doc_or_recover_propagates_transient_io_without_quarantine() {
        // Force a SyncError::Persist by pointing load() at a directory.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.cbor");
        std::fs::create_dir(&path).unwrap();
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
        assert!(
            quarantined.is_empty(),
            "a transient error must not create a quarantine file"
        );
    }

    #[test]
    fn persist_writes_both_files() {
        use crate::fleet_sync::FleetPersist;
        let dir = tempfile::tempdir().unwrap();
        let p = CommunityDeviceIntroPersist {
            doc_path: dir.path().join(FILENAME),
            replay_path: dir.path().join(REPLAY_FILENAME),
        };
        let doc = sample_doc();
        let mut t = BTreeMap::new();
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
