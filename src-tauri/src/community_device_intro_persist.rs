//! ZEB-495 (ZEB-340 Part 2) Unit 5b: on-disk persistence for
//! `CommunityDeviceIntroDoc` and its replay tracker. Thin wrappers over
//! `fleet_dataset_file` (ZEB-981), mirroring `notes_persist` /
//! `dm_inbox_persist`: sealed-at-rest v2 envelope under the pinned epoch-0
//! fleet KeyTree, atomic-rename + fsync durability, trailing-byte rejection,
//! and the CborDecode/tag-failure-quarantine recovery contract (corrupt →
//! move aside + start fresh; transient I/O → propagate untouched, ZEB-460).
//!
//! Legacy (pre-ZEB-981) plaintext v1 files are still read and are eagerly
//! re-sealed on first load.

use crate::community_device_intro_crdt::CommunityDeviceIntroDoc;
use crate::fleet_dataset_file::{self, DatasetCipher};
use crate::fleet_sync::SyncError;
use crate::owner_state_types::Hlc;
use std::collections::BTreeMap;
use std::path::Path;

/// File name for the persisted doc. Lives at
/// `<identity_dir>/community_device_intro.cbor`.
pub const FILENAME: &str = "community_device_intro.cbor";

/// File name for the persisted replay tracker. Lives alongside the doc.
pub const REPLAY_FILENAME: &str = "community_device_intro_replay.cbor";

const SCHEMA_V1: u8 = 1;
const REPLAY_SCHEMA_V1: u8 = 1;

// ── doc ──────────────────────────────────────────────────────────────────────

/// Load the doc from `path` (strict). Returns `Ok(default())` if the file does
/// not exist yet.
pub fn load(cipher: &DatasetCipher, path: &Path) -> Result<CommunityDeviceIntroDoc, SyncError> {
    fleet_dataset_file::load(cipher, path, FILENAME, SCHEMA_V1)
}

/// Load the doc with the ZEB-460 recovery contract (quarantine + self-heal on
/// corruption/tag failure; transient I/O propagated untouched; legacy
/// plaintext eagerly re-sealed) — see `fleet_dataset_file::load_or_recover`.
pub fn load_doc_or_recover(
    cipher: &DatasetCipher,
    path: &Path,
) -> Result<CommunityDeviceIntroDoc, SyncError> {
    fleet_dataset_file::load_or_recover(cipher, path, FILENAME, SCHEMA_V1)
}

/// Save the doc to `path` sealed + atomically.
pub fn save(
    cipher: &DatasetCipher,
    path: &Path,
    doc: &CommunityDeviceIntroDoc,
) -> Result<(), SyncError> {
    fleet_dataset_file::save(cipher, path, FILENAME, SCHEMA_V1, doc)
}

// ── replay tracker ───────────────────────────────────────────────────────────

/// Load the replay tracker from `path` (strict). Returns `Ok(BTreeMap::new())`
/// if the file does not exist yet.
pub fn load_replay(
    cipher: &DatasetCipher,
    path: &Path,
) -> Result<BTreeMap<String, Hlc>, SyncError> {
    fleet_dataset_file::load(cipher, path, REPLAY_FILENAME, REPLAY_SCHEMA_V1)
}

/// Same recovery contract as [`load_doc_or_recover`], for the replay tracker.
pub fn load_replay_or_recover(
    cipher: &DatasetCipher,
    path: &Path,
) -> Result<BTreeMap<String, Hlc>, SyncError> {
    fleet_dataset_file::load_or_recover(cipher, path, REPLAY_FILENAME, REPLAY_SCHEMA_V1)
}

/// Save the replay tracker to `path` sealed + atomically.
pub fn save_replay(
    cipher: &DatasetCipher,
    path: &Path,
    tracker: &BTreeMap<String, Hlc>,
) -> Result<(), SyncError> {
    fleet_dataset_file::save(cipher, path, REPLAY_FILENAME, REPLAY_SCHEMA_V1, tracker)
}

// ── FleetPersist impl ────────────────────────────────────────────────────────

/// Durability sink for the community-device-intro fleet-sync engine. Holds the
/// absolute paths for both the doc and replay-tracker files plus the sealing
/// context.
pub struct CommunityDeviceIntroPersist {
    pub doc_path: std::path::PathBuf,
    pub replay_path: std::path::PathBuf,
    pub cipher: DatasetCipher,
}

impl crate::fleet_sync::FleetPersist<CommunityDeviceIntroDoc> for CommunityDeviceIntroPersist {
    fn persist(
        &self,
        state: &CommunityDeviceIntroDoc,
        tracker: &BTreeMap<String, Hlc>,
    ) -> Result<(), SyncError> {
        save(&self.cipher, &self.doc_path, state)?;
        save_replay(&self.cipher, &self.replay_path, tracker)?;
        Ok(())
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_device_intro_crdt::{CommunityDeviceIntroDoc, CommunityDeviceIntroEntry};
    use crate::fleet_dataset_file::test_cipher;
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
        let c = test_cipher();
        assert_eq!(load(&c, &path).unwrap(), CommunityDeviceIntroDoc::default());
        let doc = sample_doc();
        save(&c, &path, &doc).unwrap();
        assert_eq!(load(&c, &path).unwrap(), doc);
        // ZEB-981: what hits the disk is the sealed envelope, not plaintext.
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(raw[0], crate::fleet_dataset_file::SEALED_SCHEMA_V2);
    }

    #[test]
    fn legacy_plaintext_device_intro_migrates_to_sealed_on_load() {
        // A pre-ZEB-981 build persisted `[0x01] ‖ plaintext CBOR`; first load
        // must return the doc AND rewrite the file sealed.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILENAME);
        let c = test_cipher();
        let mut v1 = vec![1u8];
        ciborium::into_writer(&sample_doc(), &mut v1).unwrap();
        std::fs::write(&path, &v1).unwrap();
        assert_eq!(load_doc_or_recover(&c, &path).unwrap(), sample_doc());
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(
            raw[0],
            crate::fleet_dataset_file::SEALED_SCHEMA_V2,
            "migrated on load"
        );
        assert_eq!(load_doc_or_recover(&c, &path).unwrap(), sample_doc());
    }

    #[test]
    fn load_rejects_trailing_bytes_after_valid_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILENAME);
        let c = test_cipher();
        // Legacy v1 image with a stray byte appended — plaintext path keeps
        // exercising the trailing-bytes rejection end-to-end.
        let mut v1 = vec![1u8];
        ciborium::into_writer(&sample_doc(), &mut v1).unwrap();
        v1.push(0xFF);
        std::fs::write(&path, &v1).unwrap();
        let err = load(&c, &path).unwrap_err();
        assert!(
            matches!(err, SyncError::CborDecode(_)),
            "trailing bytes must surface CborDecode, got {err:?}"
        );
        // Recovery quarantines it rather than silently starting fresh-on-write.
        let recovered = load_doc_or_recover(&c, &path).unwrap();
        assert_eq!(recovered, CommunityDeviceIntroDoc::default());
        assert!(!path.exists(), "corrupt file was quarantined");
    }

    #[test]
    fn replay_round_trips_and_missing_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(REPLAY_FILENAME);
        let c = test_cipher();
        assert!(load_replay(&c, &path).unwrap().is_empty());
        let mut t = BTreeMap::new();
        t.insert(
            "A".to_string(),
            Hlc {
                wall_ms: 9,
                logical: 1,
                device_id: "A".into(),
            },
        );
        save_replay(&c, &path, &t).unwrap();
        assert_eq!(load_replay(&c, &path).unwrap(), t);
    }

    #[test]
    fn load_doc_or_recover_quarantines_corrupt_and_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILENAME);
        let c = test_cipher();
        std::fs::write(&path, [0xFF_u8, 0x01, 0x02]).unwrap(); // unknown schema version
        let doc = load_doc_or_recover(&c, &path).unwrap();
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
        let c = test_cipher();
        std::fs::create_dir(&path).unwrap();
        let err = load_doc_or_recover(&c, &path).unwrap_err();
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
            cipher: test_cipher(),
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
        assert_eq!(load(&p.cipher, &p.doc_path).unwrap(), doc);
        assert_eq!(load_replay(&p.cipher, &p.replay_path).unwrap(), t);
    }
}
