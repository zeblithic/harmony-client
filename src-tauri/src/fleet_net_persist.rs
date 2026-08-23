//! On-disk persistence for FleetNetDoc and its replay tracker (ZEB-418 P2).
//! Thin wrappers over `fleet_dataset_file` (ZEB-981), mirroring
//! `dm_outhold_persist`: sealed-at-rest v2 envelope under the pinned epoch-0
//! fleet KeyTree, atomic-rename + fsync durability, corrupt-file quarantine,
//! and the ZEB-460 transient-vs-corrupt recovery contract. (The device network
//! info inside is already published in the pkarr routing record — sealed
//! uniformly with the rest of the dataset family; the petname map is not
//! public, though.)
//!
//! Legacy (pre-ZEB-981) plaintext v1 files are still read and are eagerly
//! re-sealed on first load.

use crate::fleet_dataset_file::{self, DatasetCipher};
use crate::fleet_net::FleetNetDoc;
use crate::fleet_sync::SyncError;
use crate::owner_state_types::Hlc;
use std::collections::BTreeMap;
use std::path::Path;

/// File name for the persisted FleetNetDoc. Lives in the identity dir beside
/// `notes.cbor`.
pub const FLEET_NET_FILENAME: &str = "fleet_net.cbor";

/// File name for the persisted replay tracker. Lives alongside `fleet_net.cbor`.
pub const FLEET_NET_REPLAY_FILENAME: &str = "fleet_net_replay.cbor";

const FLEET_NET_SCHEMA_V1: u8 = 1;
const FLEET_NET_REPLAY_SCHEMA_V1: u8 = 1;

// ── FleetNetDoc ───────────────────────────────────────────────────────────────

/// Load `FleetNetDoc` from `path` (strict). Returns
/// `Ok(FleetNetDoc::default())` if the file does not exist yet.
pub fn load(cipher: &DatasetCipher, path: &Path) -> Result<FleetNetDoc, SyncError> {
    fleet_dataset_file::load(cipher, path, FLEET_NET_FILENAME, FLEET_NET_SCHEMA_V1)
}

/// Load the fleet-net doc with the ZEB-460 recovery contract (quarantine +
/// self-heal on corruption/tag failure; transient I/O propagated untouched;
/// legacy plaintext eagerly re-sealed) — see `fleet_dataset_file::load_or_recover`.
pub fn load_doc_or_recover(cipher: &DatasetCipher, path: &Path) -> Result<FleetNetDoc, SyncError> {
    fleet_dataset_file::load_or_recover(cipher, path, FLEET_NET_FILENAME, FLEET_NET_SCHEMA_V1)
}

/// Save `FleetNetDoc` to `path` sealed + atomically.
pub fn save(cipher: &DatasetCipher, path: &Path, doc: &FleetNetDoc) -> Result<(), SyncError> {
    fleet_dataset_file::save(cipher, path, FLEET_NET_FILENAME, FLEET_NET_SCHEMA_V1, doc)
}

// ── Replay tracker ────────────────────────────────────────────────────────────

/// Load the replay tracker from `path` (strict). Returns `Ok(BTreeMap::new())`
/// if the file does not exist yet.
pub fn load_replay(
    cipher: &DatasetCipher,
    path: &Path,
) -> Result<BTreeMap<String, Hlc>, SyncError> {
    fleet_dataset_file::load(
        cipher,
        path,
        FLEET_NET_REPLAY_FILENAME,
        FLEET_NET_REPLAY_SCHEMA_V1,
    )
}

/// Same recovery contract as [`load_doc_or_recover`], for the replay tracker.
pub fn load_replay_or_recover(
    cipher: &DatasetCipher,
    path: &Path,
) -> Result<BTreeMap<String, Hlc>, SyncError> {
    fleet_dataset_file::load_or_recover(
        cipher,
        path,
        FLEET_NET_REPLAY_FILENAME,
        FLEET_NET_REPLAY_SCHEMA_V1,
    )
}

/// Save the replay tracker to `path` sealed + atomically.
pub fn save_replay(
    cipher: &DatasetCipher,
    path: &Path,
    tracker: &BTreeMap<String, Hlc>,
) -> Result<(), SyncError> {
    fleet_dataset_file::save(
        cipher,
        path,
        FLEET_NET_REPLAY_FILENAME,
        FLEET_NET_REPLAY_SCHEMA_V1,
        tracker,
    )
}

// ── FleetPersist impl ─────────────────────────────────────────────────────────

/// Durability sink for the fleet-net fleet-sync engine. Holds the absolute
/// paths for both the doc and replay-tracker files plus the sealing context.
/// The engine calls `persist` inside a `spawn_blocking` (fleet_sync.rs), so
/// this impl stays synchronous like `DmOutholdPersist`.
pub struct FleetNetPersist {
    pub doc_path: std::path::PathBuf,
    pub replay_path: std::path::PathBuf,
    pub cipher: DatasetCipher,
}

impl crate::fleet_sync::FleetPersist<FleetNetDoc> for FleetNetPersist {
    fn persist(
        &self,
        state: &FleetNetDoc,
        tracker: &BTreeMap<String, Hlc>,
    ) -> Result<(), SyncError> {
        save(&self.cipher, &self.doc_path, state)?;
        save_replay(&self.cipher, &self.replay_path, tracker)?;
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet_dataset_file::test_cipher;
    use crate::fleet_net::{FleetNetDoc, FleetNetRow};
    use crate::fleet_sync::SyncError;
    use crate::owner_state_types::Hlc;

    fn sample_row() -> FleetNetRow {
        FleetNetRow {
            iroh_endpoint_id: [0x01u8; 32],
            home_relay: "relay.example.com".into(),
            seen_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "dev-a".into(),
            },
            feed_binding: None,
        }
    }

    fn sample_doc() -> FleetNetDoc {
        let mut doc = FleetNetDoc::default();
        doc.devices.insert("dev-a".into(), sample_row());
        // ZEB-668 S4: a petname entry so persistence round-trips cover the
        // additive map.
        doc.petnames.insert(
            "dev-a".into(),
            crate::fleet_net::FleetNetPetname {
                name: "sample".into(),
                set_at: Hlc {
                    wall_ms: 2,
                    logical: 0,
                    device_id: "dev-a".into(),
                },
            },
        );
        doc
    }

    #[test]
    fn doc_round_trips_and_missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet_net.cbor");
        let c = test_cipher();
        assert_eq!(load(&c, &path).unwrap(), FleetNetDoc::default());
        let doc = sample_doc();
        save(&c, &path, &doc).unwrap();
        assert_eq!(load(&c, &path).unwrap(), doc);
        // ZEB-981: what hits the disk is the sealed envelope, not plaintext.
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(raw[0], crate::fleet_dataset_file::SEALED_SCHEMA_V2);
    }

    #[test]
    fn legacy_plaintext_fleet_net_migrates_to_sealed_on_load() {
        // A pre-ZEB-981 build persisted `[0x01] ‖ plaintext CBOR`; first load
        // must return the doc AND rewrite the file sealed.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet_net.cbor");
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
        // Trailing garbage in the (legacy) image must surface CborDecode so
        // load_doc_or_recover quarantines it; envelope tampering is covered by
        // the fleet_dataset_file tag-tamper test.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet_net.cbor");
        let c = test_cipher();
        let mut v1 = vec![1u8];
        ciborium::into_writer(&sample_doc(), &mut v1).unwrap();
        v1.push(0xFF);
        std::fs::write(&path, &v1).unwrap();
        let err = load(&c, &path).unwrap_err();
        assert!(
            matches!(err, SyncError::CborDecode(_)),
            "trailing bytes must surface CborDecode, got {err:?}"
        );
        let recovered = load_doc_or_recover(&c, &path).unwrap();
        assert_eq!(recovered, FleetNetDoc::default());
        assert!(!path.exists(), "corrupt file was quarantined");
    }

    #[test]
    fn replay_round_trips_and_missing_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet_net_replay.cbor");
        let c = test_cipher();
        assert!(load_replay(&c, &path).unwrap().is_empty());
        let mut t = std::collections::BTreeMap::new();
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
        let path = dir.path().join("fleet_net.cbor");
        let c = test_cipher();
        // Unknown schema version byte → strict load returns Err(CborDecode).
        std::fs::write(&path, [0xFF_u8, 0x01, 0x02]).unwrap();
        let doc = load_doc_or_recover(&c, &path).unwrap();
        assert_eq!(doc, FleetNetDoc::default(), "recovers to a fresh empty doc");
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
                    .contains("fleet_net.cbor.corrupt-")
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet_net.cbor");
        let c = test_cipher();
        assert_eq!(
            load_doc_or_recover(&c, &path).unwrap(),
            FleetNetDoc::default()
        );
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
        let c = test_cipher();
        std::fs::create_dir(&path).unwrap();
        let err = load_doc_or_recover(&c, &path).unwrap_err();
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
        let c = test_cipher();
        std::fs::create_dir(&path).unwrap();
        let err = load_replay_or_recover(&c, &path).unwrap_err();
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
        let path = dir.path().join("fleet_net_replay.cbor");
        let c = test_cipher();
        std::fs::write(&path, [0xFF_u8]).unwrap(); // unknown schema version
        let tracker = load_replay_or_recover(&c, &path).unwrap();
        assert!(tracker.is_empty(), "recovers to an empty tracker");
        assert!(!path.exists(), "corrupt replay file moved aside");
        let quarantined: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("fleet_net_replay.cbor.corrupt-")
            })
            .collect();
        assert_eq!(quarantined.len(), 1, "exactly one quarantine file");
    }

    #[test]
    fn fleet_net_persist_writes_both_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = FleetNetPersist {
            doc_path: dir.path().join("fleet_net.cbor"),
            replay_path: dir.path().join("fleet_net_replay.cbor"),
            cipher: test_cipher(),
        };
        use crate::fleet_sync::FleetPersist;
        let doc = sample_doc();
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
        assert_eq!(load(&p.cipher, &p.doc_path).unwrap(), doc);
        assert_eq!(load_replay(&p.cipher, &p.replay_path).unwrap(), t);
    }
}
