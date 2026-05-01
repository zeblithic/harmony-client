//! On-disk persistence for the Phase-2 OwnerState CRDT and the
//! RootReplayTracker (ZEB-215 Sub-A Phase 3a).
//!
//! See `docs/specs/2026-05-01-zeb-215-sub-a-phase3a-sync-design.md`
//! §"Persistence layer". Two files written via atomic-rename + fsync,
//! each prefixed with a 1-byte schema version.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use tempfile;

#[derive(thiserror::Error, Debug)]
pub enum PersistError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("CBOR decode: {0}")]
    CborDecode(String),
    #[error("CBOR encode: {0}")]
    CborEncode(String),
    #[error("file corrupt (truncated or invalid CBOR)")]
    Corrupt,
    #[error("unknown schema version byte: {0:#x}")]
    UnknownSchemaVersion(u8),
}

/// Atomically replace `path` with `bytes`. Writes to a sibling
/// tempfile, fsyncs, renames into place, then fsyncs the directory
/// entry so the rename itself is durable.
pub fn save_atomically(path: &Path, bytes: &[u8]) -> Result<(), PersistError> {
    let dir = path.parent().expect("save_atomically: path has no parent");
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .map_err(|e| PersistError::Io(std::io::Error::other(e)))?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

use crate::owner_state_crdt::OwnerState;
use ciborium::{from_reader, into_writer};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;

const CRDT_FILE_SCHEMA_V1: u8 = 1;

#[derive(Serialize, Deserialize)]
struct CrdtFileV1 {
    spaces: BTreeMap<crate::owner_state_types::SpaceId, crate::owner_state_types::Space>,
    outbox:
        BTreeMap<crate::owner_state_types::OutboxEntryId, crate::owner_state_types::OutboxEntry>,
    inbox: BTreeMap<crate::owner_state_types::InboxKey, crate::owner_state_types::InboxEntry>,
    markers: BTreeMap<crate::owner_state_types::SpaceId, crate::owner_state_types::ReadMarker>,
    tombstones: BTreeSet<crate::owner_state_types::SpaceId>,
}

impl From<&OwnerState> for CrdtFileV1 {
    fn from(s: &OwnerState) -> Self {
        Self {
            spaces: s.spaces.clone(),
            outbox: s.outbox.clone(),
            inbox: s.inbox.clone(),
            markers: s.markers.clone(),
            tombstones: s.tombstones.clone(),
        }
    }
}

impl From<CrdtFileV1> for OwnerState {
    fn from(f: CrdtFileV1) -> Self {
        OwnerState {
            spaces: f.spaces,
            outbox: f.outbox,
            inbox: f.inbox,
            markers: f.markers,
            tombstones: f.tombstones,
        }
    }
}

pub fn save_crdt(path: &Path, state: &OwnerState) -> Result<(), PersistError> {
    let file = CrdtFileV1::from(state);
    let mut bytes = vec![CRDT_FILE_SCHEMA_V1];
    into_writer(&file, &mut bytes).map_err(|e| PersistError::CborEncode(e.to_string()))?;
    save_atomically(path, &bytes)
}

pub fn load_crdt(path: &Path) -> Result<OwnerState, PersistError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(OwnerState::default()),
        Err(e) => return Err(e.into()),
    };
    if bytes.is_empty() {
        return Err(PersistError::Corrupt);
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    match version {
        CRDT_FILE_SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let file: CrdtFileV1 =
                from_reader(&mut cursor).map_err(|e| PersistError::CborDecode(e.to_string()))?;
            // Reject trailing bytes — defensive against truncation
            // edge cases that decode "successfully" but stop short.
            if (cursor.position() as usize) != payload.len() {
                return Err(PersistError::Corrupt);
            }
            Ok(file.into())
        }
        v => Err(PersistError::UnknownSchemaVersion(v)),
    }
}

use crate::owner_state_types::Hlc;

const REPLAY_FILE_SCHEMA_V1: u8 = 1;

#[derive(Serialize, Deserialize)]
struct ReplayFileV1(BTreeMap<String, Hlc>);

pub fn save_replay(path: &Path, tracker: &BTreeMap<String, Hlc>) -> Result<(), PersistError> {
    let file = ReplayFileV1(tracker.clone());
    let mut bytes = vec![REPLAY_FILE_SCHEMA_V1];
    into_writer(&file, &mut bytes).map_err(|e| PersistError::CborEncode(e.to_string()))?;
    save_atomically(path, &bytes)
}

pub fn load_replay(path: &Path) -> Result<BTreeMap<String, Hlc>, PersistError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e.into()),
    };
    if bytes.is_empty() {
        return Err(PersistError::Corrupt);
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    match version {
        REPLAY_FILE_SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let file: ReplayFileV1 =
                from_reader(&mut cursor).map_err(|e| PersistError::CborDecode(e.to_string()))?;
            if (cursor.position() as usize) != payload.len() {
                return Err(PersistError::Corrupt);
            }
            Ok(file.0)
        }
        v => Err(PersistError::UnknownSchemaVersion(v)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_crdt::OwnerState;
    use crate::owner_state_types::{
        ContentId, DeliveryStatus, Hlc, OutboxEntry, OutboxEntryId, OwnerAddr, ReadMarker, Space,
        SpaceId, SpaceKind, TransportBinding,
    };

    #[test]
    fn save_atomically_creates_file_with_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        save_atomically(&path, b"hello world").unwrap();
        let read_back = std::fs::read(&path).unwrap();
        assert_eq!(read_back, b"hello world");
    }

    #[test]
    fn save_atomically_replaces_existing_file_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        save_atomically(&path, b"old").unwrap();
        save_atomically(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn dropped_tempfile_does_not_corrupt_existing_file() {
        // Crash-survival: simulate a save that begins (creates a tempfile)
        // but is dropped before persist. The original file must remain
        // intact.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        save_atomically(&path, b"original").unwrap();

        // Simulate a partial save: create a tempfile, write, but drop
        // without persist (mimics a crash mid-save).
        {
            let mut tmp = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
            tmp.write_all(b"partial junk").unwrap();
            // tmp drops here — tempfile auto-deletes
        }

        assert_eq!(std::fs::read(&path).unwrap(), b"original");
    }

    fn hlc(w: u64) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: "alice".into(),
        }
    }

    fn sample_state() -> OwnerState {
        let mut s = OwnerState::default();
        let folder = Space {
            id: SpaceId([1; 16]),
            kind: SpaceKind::Folder,
            parent: None,
            community_id: None,
            name: "Root".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(100),
            updated_at: hlc(100),
        };
        s.spaces.insert(folder.id, folder);
        s.outbox.insert(
            OutboxEntryId([7; 16]),
            OutboxEntry {
                id: OutboxEntryId([7; 16]),
                space_id: SpaceId([1; 16]),
                recipient_owners: vec![OwnerAddr([2; 16])],
                message_cid: ContentId([3; 32]),
                created_at: hlc(100),
                delivered_to: Default::default(),
                delivery_status: DeliveryStatus::Pending,
            },
        );
        s.markers.insert(
            SpaceId([1; 16]),
            ReadMarker {
                space_id: SpaceId([1; 16]),
                last_read_at: hlc(150),
            },
        );
        let _ = (TransportBinding::Reticulum {
            participants: vec![],
        },); // ensure import isn't dead
        s
    }

    #[test]
    fn crdt_round_trip_preserves_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner_state_crdt.cbor");
        let original = sample_state();
        save_crdt(&path, &original).unwrap();
        let loaded = load_crdt(&path).unwrap();
        assert_eq!(loaded, original);
    }

    #[test]
    fn crdt_load_missing_file_returns_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never_written.cbor");
        let loaded = load_crdt(&path).unwrap();
        assert_eq!(loaded, OwnerState::default());
    }

    #[test]
    fn replay_round_trip_preserves_tracker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state_root_replay.cbor");
        let mut original: BTreeMap<String, Hlc> = BTreeMap::new();
        original.insert("alice-laptop".into(), hlc(100));
        original.insert("bob-phone".into(), hlc(200));
        save_replay(&path, &original).unwrap();
        let loaded = load_replay(&path).unwrap();
        assert_eq!(loaded, original);
    }

    #[test]
    fn replay_load_missing_file_returns_empty_map() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never_written.cbor");
        let loaded = load_replay(&path).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn crdt_load_unknown_schema_version_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.cbor");
        // 0xFF is reserved-future; v1 is 0x01.
        std::fs::write(&path, [0xFF_u8, 0x00, 0x01]).unwrap();
        let err = load_crdt(&path).expect_err("should error");
        assert!(matches!(err, PersistError::UnknownSchemaVersion(0xFF)));
    }

    #[test]
    fn crdt_load_truncated_cbor_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated.cbor");
        // Schema v1 + arbitrary CBOR-like junk that won't decode.
        std::fs::write(&path, [CRDT_FILE_SCHEMA_V1, 0xA1, 0x66]).unwrap();
        let err = load_crdt(&path).expect_err("should error");
        assert!(matches!(
            err,
            PersistError::CborDecode(_) | PersistError::Corrupt
        ));
    }

    #[test]
    fn crdt_load_empty_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.cbor");
        std::fs::write(&path, []).unwrap();
        let err = load_crdt(&path).expect_err("should error");
        assert!(matches!(err, PersistError::Corrupt));
    }

    #[test]
    fn replay_load_unknown_schema_version_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future_replay.cbor");
        std::fs::write(&path, [0xFE_u8]).unwrap();
        let err = load_replay(&path).expect_err("should error");
        assert!(matches!(err, PersistError::UnknownSchemaVersion(0xFE)));
    }

    #[test]
    fn crdt_load_trailing_bytes_after_valid_cbor_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("with_tail.cbor");
        // Save a valid file, then append a junk byte.
        save_crdt(&path, &OwnerState::default()).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(0xFF);
        std::fs::write(&path, bytes).unwrap();
        let err = load_crdt(&path).expect_err("should error");
        assert!(matches!(err, PersistError::Corrupt));
    }
}
