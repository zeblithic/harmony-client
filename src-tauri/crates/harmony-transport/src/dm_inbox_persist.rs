//! On-disk persistence for DmInboxDoc, its replay tracker (ZEB-418 P1), and
//! the two LOCAL sidecars (first-observed clock ZEB-862, expiry tombstones
//! ZEB-925). Thin wrappers over `fleet_dataset_file` (ZEB-981): sealed-at-rest
//! v2 envelope under the pinned epoch-0 fleet KeyTree, atomic-rename + fsync
//! durability, corrupt-file quarantine, and the ZEB-460 transient-vs-corrupt
//! recovery contract. (The deposited payloads inside the doc are already
//! sealed storage blobs; the envelope additionally hides sender/recipient
//! metadata and deposit timing from filesystem readers.)
//!
//! Legacy (pre-ZEB-981) plaintext v1 files are still read and are eagerly
//! re-sealed on first load.

use crate::dm_inbox_crdt::DmInboxDoc;
use crate::fleet_dataset_file::{self, DatasetCipher};
use crate::fleet_sync::SyncError;
use crate::owner_state_types::Hlc;
use std::collections::BTreeMap;
use std::path::Path;

/// File name for the persisted DmInboxDoc. Lives in the identity dir beside
/// `notes.cbor`.
pub const DM_INBOX_FILENAME: &str = "dm_inbox.cbor";

/// File name for the persisted replay tracker. Lives alongside `dm_inbox.cbor`.
pub const DM_INBOX_REPLAY_FILENAME: &str = "dm_inbox_replay.cbor";

const DM_INBOX_SCHEMA_V1: u8 = 1;
const DM_INBOX_REPLAY_SCHEMA_V1: u8 = 1;

// ── DmInboxDoc ───────────────────────────────────────────────────────────────

/// Load `DmInboxDoc` from `path` (strict). Returns `Ok(DmInboxDoc::default())`
/// if the file does not exist yet.
pub fn load(cipher: &DatasetCipher, path: &Path) -> Result<DmInboxDoc, SyncError> {
    fleet_dataset_file::load(cipher, path, DM_INBOX_FILENAME, DM_INBOX_SCHEMA_V1)
}

/// Load the dm-inbox doc with the ZEB-460 recovery contract (quarantine +
/// self-heal on corruption/tag failure; transient I/O propagated untouched;
/// legacy plaintext eagerly re-sealed) — see `fleet_dataset_file::load_or_recover`.
pub fn load_doc_or_recover(cipher: &DatasetCipher, path: &Path) -> Result<DmInboxDoc, SyncError> {
    fleet_dataset_file::load_or_recover(cipher, path, DM_INBOX_FILENAME, DM_INBOX_SCHEMA_V1)
}

/// Save `DmInboxDoc` to `path` sealed + atomically.
pub fn save(cipher: &DatasetCipher, path: &Path, doc: &DmInboxDoc) -> Result<(), SyncError> {
    fleet_dataset_file::save(cipher, path, DM_INBOX_FILENAME, DM_INBOX_SCHEMA_V1, doc)
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
        DM_INBOX_REPLAY_FILENAME,
        DM_INBOX_REPLAY_SCHEMA_V1,
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
        DM_INBOX_REPLAY_FILENAME,
        DM_INBOX_REPLAY_SCHEMA_V1,
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
        DM_INBOX_REPLAY_FILENAME,
        DM_INBOX_REPLAY_SCHEMA_V1,
        tracker,
    )
}

// ── first-observed sidecar (ZEB-862) ───────────────────────────────────────────

/// File name for the persisted LOCAL first-observation clock. Lives alongside
/// `dm_inbox.cbor`. Local-only: never replicated, never on the wire — it makes
/// the `#[serde(skip)]` `DmInboxDoc::first_observed_ms` TTL clock survive
/// restart instead of re-stamping `now` on the first post-boot sweep.
pub const DM_INBOX_FIRST_OBSERVED_FILENAME: &str = "dm_inbox_first_observed.cbor";

const DM_INBOX_FIRST_OBSERVED_SCHEMA_V1: u8 = 1;

/// Load the LOCAL first-observation clock from `path` (strict). Returns
/// `Ok(BTreeMap::new())` if the file does not exist yet (→ entries then
/// inherit their own deposit stamp as the observation floor at restore,
/// ZEB-998 Q-3; no doc-file migration needed).
pub fn load_first_observed(
    cipher: &DatasetCipher,
    path: &Path,
) -> Result<BTreeMap<String, u64>, SyncError> {
    fleet_dataset_file::load(
        cipher,
        path,
        DM_INBOX_FIRST_OBSERVED_FILENAME,
        DM_INBOX_FIRST_OBSERVED_SCHEMA_V1,
    )
}

/// Same recovery contract as [`load_doc_or_recover`]. A missing/empty clock is
/// safe — restore then seeds each entry's stamp from its own deposit floor
/// (ZEB-998 Q-3), which can only shorten retention relative to the lost local
/// stamps, never extend it — so quarantine-to-empty never loses correctness.
pub fn load_first_observed_or_recover(
    cipher: &DatasetCipher,
    path: &Path,
) -> Result<BTreeMap<String, u64>, SyncError> {
    fleet_dataset_file::load_or_recover(
        cipher,
        path,
        DM_INBOX_FIRST_OBSERVED_FILENAME,
        DM_INBOX_FIRST_OBSERVED_SCHEMA_V1,
    )
}

/// Save the LOCAL first-observation clock to `path` sealed + atomically.
pub fn save_first_observed(
    cipher: &DatasetCipher,
    path: &Path,
    map: &BTreeMap<String, u64>,
) -> Result<(), SyncError> {
    fleet_dataset_file::save(
        cipher,
        path,
        DM_INBOX_FIRST_OBSERVED_FILENAME,
        DM_INBOX_FIRST_OBSERVED_SCHEMA_V1,
        map,
    )
}

// ── expired-tombstone sidecar (ZEB-925) ───────────────────────────────────────

/// File name for the persisted LOCAL expiry-tombstone map. Lives alongside
/// `dm_inbox.cbor`. Local-only: never replicated, never on the wire — it makes
/// the `#[serde(skip)]` `DmInboxDoc::expired_at_ms` suppression survive
/// restart (a tombstone that forgot across reboot would let a sibling's merge
/// resurrect the expired entry with a fresh TTL window).
pub const DM_INBOX_EXPIRED_FILENAME: &str = "dm_inbox_expired.cbor";

const DM_INBOX_EXPIRED_SCHEMA_V1: u8 = 1;

/// Load the LOCAL expiry-tombstone map from `path` (strict). Returns
/// `Ok(BTreeMap::new())` if the file does not exist yet (→ no suppression;
/// worst case one extra TTL window per resurrection — exactly pre-ZEB-925
/// behavior, so no doc-file migration is needed).
pub fn load_expired(
    cipher: &DatasetCipher,
    path: &Path,
) -> Result<BTreeMap<String, u64>, SyncError> {
    fleet_dataset_file::load(
        cipher,
        path,
        DM_INBOX_EXPIRED_FILENAME,
        DM_INBOX_EXPIRED_SCHEMA_V1,
    )
}

/// Same recovery contract as [`load_doc_or_recover`]. A missing/empty map is
/// safe — suppression is lost, retention falls back to one TTL window per
/// resurrection until re-tombstoned.
pub fn load_expired_or_recover(
    cipher: &DatasetCipher,
    path: &Path,
) -> Result<BTreeMap<String, u64>, SyncError> {
    fleet_dataset_file::load_or_recover(
        cipher,
        path,
        DM_INBOX_EXPIRED_FILENAME,
        DM_INBOX_EXPIRED_SCHEMA_V1,
    )
}

/// Save the LOCAL expiry-tombstone map to `path` sealed + atomically.
pub fn save_expired(
    cipher: &DatasetCipher,
    path: &Path,
    map: &BTreeMap<String, u64>,
) -> Result<(), SyncError> {
    fleet_dataset_file::save(
        cipher,
        path,
        DM_INBOX_EXPIRED_FILENAME,
        DM_INBOX_EXPIRED_SCHEMA_V1,
        map,
    )
}

// ── FleetPersist impl ─────────────────────────────────────────────────────────

/// Durability sink for the dm-inbox fleet-sync engine. Holds the absolute
/// paths for both the doc and replay-tracker files plus the sealing context.
/// The engine calls `persist` inside a `spawn_blocking` (fleet_sync.rs), so
/// this impl stays synchronous like `NotesPersist`.
pub struct DmInboxPersist {
    pub doc_path: std::path::PathBuf,
    pub replay_path: std::path::PathBuf,
    /// ZEB-862: local-only first-observation clock sidecar (see
    /// `DM_INBOX_FIRST_OBSERVED_FILENAME`).
    pub first_observed_path: std::path::PathBuf,
    /// ZEB-925: local-only expiry-tombstone sidecar (see
    /// `DM_INBOX_EXPIRED_FILENAME`).
    pub expired_path: std::path::PathBuf,
    pub cipher: DatasetCipher,
}

impl crate::fleet_sync::FleetPersist<DmInboxDoc> for DmInboxPersist {
    fn persist(
        &self,
        state: &DmInboxDoc,
        tracker: &BTreeMap<String, Hlc>,
    ) -> Result<(), SyncError> {
        // ZEB-925: tombstones FIRST. A crash between writes then leaves
        // tombstone-present + stale-doc — healed by restore_expired at boot —
        // instead of fresh-doc + missing-tombstone, which resurrects the
        // expired entry with a fresh TTL window (un-healable).
        //
        // ZEB-998: the remaining torn window (doc landed, first_observed did
        // not) is healed at boot by restore_first_observed's Q-3 rule — a
        // stampless entry inherits its own deposit floor, so the tear cannot
        // extend its TTL. The four renames therefore need no commit marker.
        save_expired(&self.cipher, &self.expired_path, state.expired_at_ms())?;
        save(&self.cipher, &self.doc_path, state)?;
        save_replay(&self.cipher, &self.replay_path, tracker)?;
        save_first_observed(
            &self.cipher,
            &self.first_observed_path,
            state.first_observed_ms(),
        )?;
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dm_inbox_crdt::{DmInboxDoc, DmInboxEntry};
    use crate::fleet_dataset_file::test_cipher;
    use crate::fleet_sync::SyncError;
    use crate::owner_state_types::Hlc;

    fn sample_entry() -> DmInboxEntry {
        DmInboxEntry {
            sender_owner: [7u8; 16],
            cidnotify_packet: Some(vec![1, 2, 3]),
            storage_blob: vec![4, 5, 6],
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
            grant_revoke: None,
            deposited_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "A".into(),
            },
            deposited_by: "dev-a".into(),
            ingested_by: ["dev-1".to_string()].into(),
        }
    }

    fn sample_doc() -> DmInboxDoc {
        let mut doc = DmInboxDoc::default();
        doc.entries
            .insert(DmInboxDoc::key(&[1u8; 16], &[2u8; 32]), sample_entry());
        doc
    }

    #[test]
    fn doc_round_trips_and_missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dm_inbox.cbor");
        let c = test_cipher();
        assert_eq!(load(&c, &path).unwrap(), DmInboxDoc::default());
        let doc = sample_doc();
        save(&c, &path, &doc).unwrap();
        assert_eq!(load(&c, &path).unwrap(), doc);
        // ZEB-981: what hits the disk is the sealed envelope, not plaintext.
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(raw[0], crate::fleet_dataset_file::SEALED_SCHEMA_V2);
    }

    #[test]
    fn legacy_plaintext_dm_inbox_migrates_to_sealed_on_load() {
        // A pre-ZEB-981 build persisted `[0x01] ‖ plaintext CBOR`; first load
        // must return the doc AND rewrite the file sealed.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dm_inbox.cbor");
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
        let path = dir.path().join("dm_inbox.cbor");
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
        assert_eq!(recovered, DmInboxDoc::default());
        assert!(!path.exists(), "corrupt file was quarantined");
    }

    #[test]
    fn replay_round_trips_and_missing_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dm_inbox_replay.cbor");
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
        // A corrupt dm-inbox file must NOT silently become an empty doc that
        // overwrites pending deposits on the next persist: the recovery path
        // renames the bad bytes aside and returns a fresh default.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dm_inbox.cbor");
        let c = test_cipher();
        // Unknown schema version byte → strict load returns Err(CborDecode).
        std::fs::write(&path, [0xFF_u8, 0x01, 0x02]).unwrap();
        let doc = load_doc_or_recover(&c, &path).unwrap();
        assert_eq!(doc, DmInboxDoc::default(), "recovers to a fresh empty doc");
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
                    .contains("dm_inbox.cbor.corrupt-")
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
        let path = dir.path().join("dm_inbox.cbor");
        let c = test_cipher();
        assert_eq!(
            load_doc_or_recover(&c, &path).unwrap(),
            DmInboxDoc::default()
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
        let path = dir.path().join("dm_inbox.cbor");
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
        let path = dir.path().join("dm_inbox_replay.cbor");
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
        let path = dir.path().join("dm_inbox_replay.cbor");
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
                    .contains("dm_inbox_replay.cbor.corrupt-")
            })
            .collect();
        assert_eq!(quarantined.len(), 1, "exactly one quarantine file");
    }

    #[test]
    fn dm_inbox_persist_writes_all_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = DmInboxPersist {
            doc_path: dir.path().join("dm_inbox.cbor"),
            replay_path: dir.path().join("dm_inbox_replay.cbor"),
            first_observed_path: dir.path().join("dm_inbox_first_observed.cbor"),
            expired_path: dir.path().join("dm_inbox_expired.cbor"),
            cipher: test_cipher(),
        };
        use crate::fleet_sync::FleetPersist;
        let mut doc = sample_doc();
        // Key the stamp to the sample entry so `restore_first_observed`'s
        // orphan-prune (ZEB-862 Q-2) keeps it; `u64::MAX` avoids the Q-1
        // future-stamp clamp.
        let fo: BTreeMap<String, u64> = [(DmInboxDoc::key(&[1u8; 16], &[2u8; 32]), 9u64)]
            .into_iter()
            .collect();
        doc.restore_first_observed(fo.clone(), u64::MAX);
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
        assert_eq!(
            load_first_observed(&p.cipher, &p.first_observed_path).unwrap(),
            fo
        );
    }

    // ── first-observed sidecar (ZEB-862) ──────────────────────────────────────

    #[test]
    fn first_observed_round_trips_and_missing_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dm_inbox_first_observed.cbor");
        let c = test_cipher();
        assert!(load_first_observed(&c, &path).unwrap().is_empty());
        let mut m = std::collections::BTreeMap::new();
        m.insert("k1".to_string(), 111u64);
        m.insert("k2".to_string(), 222u64);
        save_first_observed(&c, &path, &m).unwrap();
        assert_eq!(load_first_observed(&c, &path).unwrap(), m);
    }

    #[test]
    fn load_first_observed_rejects_trailing_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dm_inbox_first_observed.cbor");
        let c = test_cipher();
        // Legacy v1 sidecar with a stray byte appended.
        let m: BTreeMap<String, u64> = [("k".to_string(), 5u64)].into_iter().collect();
        let mut v1 = vec![1u8];
        ciborium::into_writer(&m, &mut v1).unwrap();
        v1.push(0xFF);
        std::fs::write(&path, &v1).unwrap();
        assert!(matches!(
            load_first_observed(&c, &path).unwrap_err(),
            SyncError::CborDecode(_)
        ));
        assert!(load_first_observed_or_recover(&c, &path)
            .unwrap()
            .is_empty());
        assert!(!path.exists(), "corrupt sidecar was quarantined");
    }

    #[test]
    fn load_first_observed_or_recover_propagates_transient_io_without_quarantine() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fo.cbor");
        let c = test_cipher();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(
            load_first_observed_or_recover(&c, &path).unwrap_err(),
            SyncError::Persist(_)
        ));
        assert!(path.is_dir(), "transient error leaves the path untouched");
    }

    // ── expired-tombstone sidecar (ZEB-925) ──────────────────────────────────

    #[test]
    fn expired_round_trips_and_missing_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dm_inbox_expired.cbor");
        let c = test_cipher();
        assert!(load_expired(&c, &path).unwrap().is_empty());
        let mut m = std::collections::BTreeMap::new();
        m.insert("k1".to_string(), 111u64);
        m.insert("k2".to_string(), 222u64);
        save_expired(&c, &path, &m).unwrap();
        assert_eq!(load_expired(&c, &path).unwrap(), m);
    }

    #[test]
    fn load_expired_rejects_trailing_bytes_and_recover_quarantines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dm_inbox_expired.cbor");
        let c = test_cipher();
        // Legacy v1 sidecar with a stray byte appended.
        let m: BTreeMap<String, u64> = [("k".to_string(), 5u64)].into_iter().collect();
        let mut v1 = vec![1u8];
        ciborium::into_writer(&m, &mut v1).unwrap();
        v1.push(0xFF);
        std::fs::write(&path, &v1).unwrap();
        assert!(matches!(
            load_expired(&c, &path).unwrap_err(),
            SyncError::CborDecode(_)
        ));
        assert!(load_expired_or_recover(&c, &path).unwrap().is_empty());
        assert!(!path.exists(), "corrupt sidecar was quarantined");
    }

    #[test]
    fn persist_writes_expired_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let p = DmInboxPersist {
            doc_path: dir.path().join("dm_inbox.cbor"),
            replay_path: dir.path().join("dm_inbox_replay.cbor"),
            first_observed_path: dir.path().join("dm_inbox_first_observed.cbor"),
            expired_path: dir.path().join("dm_inbox_expired.cbor"),
            cipher: test_cipher(),
        };
        use crate::fleet_sync::FleetPersist;
        let mut doc = sample_doc();
        let m: std::collections::BTreeMap<String, u64> =
            [("gone-key".to_string(), 7u64)].into_iter().collect();
        // Boot time near the stamp so the restore-time retention prune keeps it.
        doc.restore_expired(m.clone(), 7);
        p.persist(&doc, &std::collections::BTreeMap::new()).unwrap();
        assert_eq!(load_expired(&p.cipher, &p.expired_path).unwrap(), m);
    }

    #[test]
    fn crash_between_doc_and_first_observed_writes_cannot_extend_ttl() {
        // ZEB-998 regression: simulate the torn multi-file write. Generation 1
        // persists one stamped entry; generation 2 adds a second entry and
        // persists — but the crash "lands" the doc rename and not the
        // first-observed rename, which we simulate by putting generation 1's
        // sidecar bytes back. The boot shape must give the orphaned entry its
        // deposit floor, not the boot `now` (which would restart its TTL).
        let dir = tempfile::tempdir().unwrap();
        let p = DmInboxPersist {
            doc_path: dir.path().join("dm_inbox.cbor"),
            replay_path: dir.path().join("dm_inbox_replay.cbor"),
            first_observed_path: dir.path().join("dm_inbox_first_observed.cbor"),
            expired_path: dir.path().join("dm_inbox_expired.cbor"),
            cipher: test_cipher(),
        };
        use crate::fleet_sync::FleetPersist;
        let k1 = DmInboxDoc::key(&[1u8; 16], &[2u8; 32]);
        let k2 = DmInboxDoc::key(&[3u8; 16], &[4u8; 32]);

        // Generation 1: one entry, locally observed at t=500.
        let mut doc = DmInboxDoc::default();
        doc.entries.insert(k1.clone(), sample_entry());
        doc.restore_first_observed([(k1.clone(), 500u64)].into_iter().collect(), 1_000);
        p.persist(&doc, &std::collections::BTreeMap::new()).unwrap();
        let gen1_fo = std::fs::read(&p.first_observed_path).unwrap();

        // Generation 2: a second entry deposited at wall_ms=7_777 arrives and
        // is observed; persist writes all four files...
        let mut e2 = sample_entry();
        e2.deposited_at.wall_ms = 7_777;
        doc.entries.insert(k2.clone(), e2);
        doc.gc_expired(8_000, &std::collections::BTreeSet::new());
        p.persist(&doc, &std::collections::BTreeMap::new()).unwrap();
        // ...but the crash tears the write: the first-observed rename never
        // happened, so generation 1's sidecar is what boot finds.
        std::fs::write(&p.first_observed_path, &gen1_fo).unwrap();

        // Boot shape (mirrors lib.rs): doc, expired, then first-observed.
        let boot_now = 5_000_000u64;
        let mut booted = load_doc_or_recover(&p.cipher, &p.doc_path).unwrap();
        booted.restore_expired(
            load_expired_or_recover(&p.cipher, &p.expired_path).unwrap(),
            boot_now,
        );
        booted.restore_first_observed(
            load_first_observed_or_recover(&p.cipher, &p.first_observed_path).unwrap(),
            boot_now,
        );
        assert_eq!(
            booted.first_observed_ms()[&k1],
            500,
            "covered entry keeps its persisted stamp"
        );
        assert_eq!(
            booted.first_observed_ms()[&k2],
            7_777,
            "torn-write entry inherits its deposit floor, not boot now"
        );
    }

    #[test]
    fn persist_then_reload_first_observed_drives_expiry() {
        // Full sidecar round-trip: an OLD stamp persisted by a prior run, then
        // reloaded via the boot shape and restored into a fresh doc, ages the
        // never-covered entry out on gc_expired(now, {}).
        let dir = tempfile::tempdir().unwrap();
        let fo_path = dir.path().join("dm_inbox_first_observed.cbor");
        let c = test_cipher();
        let key = DmInboxDoc::key(&[1u8; 16], &[2u8; 32]);
        let seed: BTreeMap<String, u64> = [(key.clone(), 1u64)].into_iter().collect();
        save_first_observed(&c, &fo_path, &seed).unwrap();

        let mut doc = DmInboxDoc::default();
        let mut e = sample_entry();
        e.ingested_by.clear(); // never covered → only TTL removes it
        doc.entries.insert(key.clone(), e);
        let now = crate::butler_deposit::INBOX_TTL_MS + 10_000;
        doc.restore_first_observed(load_first_observed_or_recover(&c, &fo_path).unwrap(), now);
        doc.gc_expired(now, &std::collections::BTreeSet::new());
        assert!(
            !doc.entries.contains_key(&key),
            "reloaded old stamp aged the entry out"
        );
    }
}
