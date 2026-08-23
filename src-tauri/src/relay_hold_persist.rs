//! On-disk persistence for RelayHoldDoc and its replay tracker (ZEB-458 P4 B),
//! plus the two LOCAL sidecars (first-observed clock ZEB-862, expiry
//! tombstones ZEB-924). Thin wrappers over `fleet_dataset_file` (ZEB-981),
//! mirroring `dm_inbox_persist`: sealed-at-rest v2 envelope under the pinned
//! epoch-0 fleet KeyTree, atomic-rename + fsync durability, strict
//! trailing-byte rejection, corrupt-file quarantine, and the ZEB-460
//! transient-vs-corrupt recovery contract. (The held `sealed_blob`s inside are
//! already sealed to the recipient device — the envelope additionally hides
//! sender/recipient/community metadata from filesystem readers.)
//!
//! Legacy (pre-ZEB-981) plaintext v1 files are still read and are eagerly
//! re-sealed on first load.

use crate::community_relay_hold_crdt::RelayHoldDoc;
use crate::fleet_dataset_file::{self, DatasetCipher};
use crate::fleet_sync::SyncError;
use crate::owner_state_types::Hlc;
use std::collections::BTreeMap;
use std::path::Path;

/// File name for the persisted RelayHoldDoc. Lives at
/// `<identity_dir>/relay_hold.cbor`.
pub const RELAY_HOLD_FILENAME: &str = "relay_hold.cbor";

/// File name for the persisted replay tracker. Lives alongside
/// `relay_hold.cbor`.
pub const RELAY_HOLD_REPLAY_FILENAME: &str = "relay_hold_replay.cbor";

const RELAY_HOLD_SCHEMA_V1: u8 = 1;
const RELAY_HOLD_REPLAY_SCHEMA_V1: u8 = 1;

// ── RelayHoldDoc ───────────────────────────────────────────────────────────────

/// Load `RelayHoldDoc` from `path` (strict). Returns
/// `Ok(RelayHoldDoc::default())` if the file does not exist yet.
pub fn load(cipher: &DatasetCipher, path: &Path) -> Result<RelayHoldDoc, SyncError> {
    fleet_dataset_file::load(cipher, path, RELAY_HOLD_FILENAME, RELAY_HOLD_SCHEMA_V1)
}

/// Load the relay-hold doc with the ZEB-460 recovery contract (quarantine +
/// self-heal on corruption/tag failure; transient I/O propagated untouched;
/// legacy plaintext eagerly re-sealed) — see `fleet_dataset_file::load_or_recover`.
pub fn load_doc_or_recover(cipher: &DatasetCipher, path: &Path) -> Result<RelayHoldDoc, SyncError> {
    fleet_dataset_file::load_or_recover(cipher, path, RELAY_HOLD_FILENAME, RELAY_HOLD_SCHEMA_V1)
}

/// Save `RelayHoldDoc` to `path` sealed + atomically.
pub fn save(cipher: &DatasetCipher, path: &Path, doc: &RelayHoldDoc) -> Result<(), SyncError> {
    fleet_dataset_file::save(cipher, path, RELAY_HOLD_FILENAME, RELAY_HOLD_SCHEMA_V1, doc)
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
        RELAY_HOLD_REPLAY_FILENAME,
        RELAY_HOLD_REPLAY_SCHEMA_V1,
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
        RELAY_HOLD_REPLAY_FILENAME,
        RELAY_HOLD_REPLAY_SCHEMA_V1,
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
        RELAY_HOLD_REPLAY_FILENAME,
        RELAY_HOLD_REPLAY_SCHEMA_V1,
        tracker,
    )
}

// ── first-observed sidecar (ZEB-862) ───────────────────────────────────────────

/// File name for the persisted LOCAL first-observation clock. Lives alongside
/// `relay_hold.cbor`. Local-only: never replicated, never on the wire — it
/// makes the `#[serde(skip)]` `RelayHoldDoc::first_observed_ms` TTL clock
/// survive restart instead of re-stamping `now` on the first post-boot sweep.
pub const RELAY_HOLD_FIRST_OBSERVED_FILENAME: &str = "relay_hold_first_observed.cbor";

const RELAY_HOLD_FIRST_OBSERVED_SCHEMA_V1: u8 = 1;

/// Load the LOCAL first-observation clock from `path` (strict). Returns
/// `Ok(BTreeMap::new())` if the file does not exist yet (→ today's re-stamp
/// behavior; no doc-file migration needed).
pub fn load_first_observed(
    cipher: &DatasetCipher,
    path: &Path,
) -> Result<BTreeMap<String, u64>, SyncError> {
    fleet_dataset_file::load(
        cipher,
        path,
        RELAY_HOLD_FIRST_OBSERVED_FILENAME,
        RELAY_HOLD_FIRST_OBSERVED_SCHEMA_V1,
    )
}

/// Same recovery contract as [`load_doc_or_recover`]. A missing/empty clock is
/// safe — the next sweep re-stamps `now`, exactly today's behavior — so
/// quarantine-to-empty never loses correctness, only punctuality.
pub fn load_first_observed_or_recover(
    cipher: &DatasetCipher,
    path: &Path,
) -> Result<BTreeMap<String, u64>, SyncError> {
    fleet_dataset_file::load_or_recover(
        cipher,
        path,
        RELAY_HOLD_FIRST_OBSERVED_FILENAME,
        RELAY_HOLD_FIRST_OBSERVED_SCHEMA_V1,
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
        RELAY_HOLD_FIRST_OBSERVED_FILENAME,
        RELAY_HOLD_FIRST_OBSERVED_SCHEMA_V1,
        map,
    )
}

// ── expiry-tombstone sidecar (ZEB-924) ─────────────────────────────────────────

/// File name for the persisted LOCAL expiry tombstones. Lives alongside
/// `relay_hold.cbor`. Local-only: never replicated, never on the wire — it
/// makes the `#[serde(skip)]` `RelayHoldDoc::expired_at_ms` resurrection
/// suppression survive restart (RAM-only tombstones would re-arm a full TTL
/// window per restart).
pub const RELAY_HOLD_EXPIRED_FILENAME: &str = "relay_hold_expired.cbor";

const RELAY_HOLD_EXPIRED_SCHEMA_V1: u8 = 1;

/// Load the LOCAL expiry tombstones from `path` (strict). Returns
/// `Ok(BTreeMap::new())` if the file does not exist yet (→ no suppression;
/// the next TTL expiry starts recording).
pub fn load_expired(
    cipher: &DatasetCipher,
    path: &Path,
) -> Result<BTreeMap<String, u64>, SyncError> {
    fleet_dataset_file::load(
        cipher,
        path,
        RELAY_HOLD_EXPIRED_FILENAME,
        RELAY_HOLD_EXPIRED_SCHEMA_V1,
    )
}

/// Same recovery contract as [`load_doc_or_recover`]. A missing/empty
/// tombstone set is safe-but-slower — the next resurrection re-arms one TTL
/// window, then re-tombstones — so quarantine-to-empty never loses
/// correctness, only punctuality.
pub fn load_expired_or_recover(
    cipher: &DatasetCipher,
    path: &Path,
) -> Result<BTreeMap<String, u64>, SyncError> {
    fleet_dataset_file::load_or_recover(
        cipher,
        path,
        RELAY_HOLD_EXPIRED_FILENAME,
        RELAY_HOLD_EXPIRED_SCHEMA_V1,
    )
}

/// Save the LOCAL expiry tombstones to `path` sealed + atomically.
pub fn save_expired(
    cipher: &DatasetCipher,
    path: &Path,
    map: &BTreeMap<String, u64>,
) -> Result<(), SyncError> {
    fleet_dataset_file::save(
        cipher,
        path,
        RELAY_HOLD_EXPIRED_FILENAME,
        RELAY_HOLD_EXPIRED_SCHEMA_V1,
        map,
    )
}

// ── FleetPersist impl ─────────────────────────────────────────────────────────

/// Durability sink for the relay-hold fleet-sync engine. Holds the absolute
/// paths for both the doc and replay-tracker files plus the sealing context.
/// The engine calls `persist` inside a `spawn_blocking` (fleet_sync.rs), so
/// this impl stays synchronous like `DmInboxPersist`.
pub struct RelayHoldPersist {
    pub doc_path: std::path::PathBuf,
    pub replay_path: std::path::PathBuf,
    /// ZEB-862: local-only first-observation clock sidecar (see
    /// `RELAY_HOLD_FIRST_OBSERVED_FILENAME`).
    pub first_observed_path: std::path::PathBuf,
    /// ZEB-924: local-only expiry-tombstone sidecar (see
    /// `RELAY_HOLD_EXPIRED_FILENAME`).
    pub expired_path: std::path::PathBuf,
    pub cipher: DatasetCipher,
}

impl crate::fleet_sync::FleetPersist<RelayHoldDoc> for RelayHoldPersist {
    fn persist(
        &self,
        state: &RelayHoldDoc,
        tracker: &BTreeMap<String, Hlc>,
    ) -> Result<(), SyncError> {
        // ZEB-924 (PR #667 R1): tombstones are written BEFORE the doc. Each
        // write is individually atomic but the sequence is not; a crash after
        // the doc but before the tombstones would durably drop a TTL-expired
        // entry while LOSING its fresh tombstone — the one ordering
        // `restore_expired` cannot heal (a peer merge could then re-arm a
        // fresh TTL). Tombstone-first inverts the window: a crash leaves the
        // tombstone durable with a stale doc still holding the entry, which
        // boot restoration removes (the tombstone wins).
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
    use crate::community_relay_hold_crdt::{RelayHoldDoc, RelayHoldEntry};
    use crate::fleet_dataset_file::test_cipher;
    use crate::fleet_sync::SyncError;
    use crate::owner_state_types::{Hlc, SpaceId};

    fn sample_entry() -> RelayHoldEntry {
        RelayHoldEntry {
            recipient_owner: [9u8; 16],
            sender_owner: [7u8; 16],
            community_id: SpaceId([3u8; 16]),
            sealed_blob: vec![4, 5, 6],
            held_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "A".into(),
            },
            held_by: "relay-dev".into(),
            pulled_by: ["dev-1".to_string()].into_iter().collect(),
        }
    }

    fn sample_doc() -> RelayHoldDoc {
        let mut doc = RelayHoldDoc::default();
        doc.entries
            .insert(RelayHoldDoc::key(&[9u8; 16], &[2u8; 32]), sample_entry());
        doc
    }

    #[test]
    fn doc_round_trips_and_missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay_hold.cbor");
        let c = test_cipher();
        assert_eq!(load(&c, &path).unwrap(), RelayHoldDoc::default());
        let doc = sample_doc();
        save(&c, &path, &doc).unwrap();
        assert_eq!(load(&c, &path).unwrap(), doc);
        // ZEB-981: what hits the disk is the sealed envelope, not plaintext.
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(raw[0], crate::fleet_dataset_file::SEALED_SCHEMA_V2);
    }

    #[test]
    fn legacy_plaintext_relay_hold_migrates_to_sealed_on_load() {
        // A pre-ZEB-981 build persisted `[0x01] ‖ plaintext CBOR`; first load
        // must return the doc AND rewrite the file sealed.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay_hold.cbor");
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
        let path = dir.path().join("relay_hold.cbor");
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
        assert_eq!(recovered, RelayHoldDoc::default());
        assert!(!path.exists(), "corrupt file was quarantined");
    }

    #[test]
    fn replay_round_trips_and_missing_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay_hold_replay.cbor");
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
        let path = dir.path().join("relay_hold.cbor");
        let c = test_cipher();
        std::fs::write(&path, [0xFF_u8, 0x01, 0x02]).unwrap();
        let doc = load_doc_or_recover(&c, &path).unwrap();
        assert_eq!(
            doc,
            RelayHoldDoc::default(),
            "recovers to a fresh empty doc"
        );
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
                    .contains("relay_hold.cbor.corrupt-")
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
        let path = dir.path().join("relay_hold.cbor");
        let c = test_cipher();
        assert_eq!(
            load_doc_or_recover(&c, &path).unwrap(),
            RelayHoldDoc::default()
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
        let path = dir.path().join("relay_hold_replay.cbor");
        let c = test_cipher();
        std::fs::write(&path, [0xFF_u8]).unwrap();
        let tracker = load_replay_or_recover(&c, &path).unwrap();
        assert!(tracker.is_empty(), "recovers to an empty tracker");
        assert!(!path.exists(), "corrupt replay file moved aside");
        let quarantined: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("relay_hold_replay.cbor.corrupt-")
            })
            .collect();
        assert_eq!(quarantined.len(), 1, "exactly one quarantine file");
    }

    #[test]
    fn relay_hold_persist_writes_all_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = RelayHoldPersist {
            doc_path: dir.path().join("relay_hold.cbor"),
            replay_path: dir.path().join("relay_hold_replay.cbor"),
            first_observed_path: dir.path().join("relay_hold_first_observed.cbor"),
            expired_path: dir.path().join(RELAY_HOLD_EXPIRED_FILENAME),
            cipher: test_cipher(),
        };
        use crate::fleet_sync::FleetPersist;
        let mut doc = sample_doc();
        // Key the stamp to the sample entry so `restore_first_observed`'s
        // orphan-prune (ZEB-862 Q-2) keeps it; `u64::MAX` avoids the Q-1
        // future-stamp clamp.
        let fo: BTreeMap<String, u64> = [(RelayHoldDoc::key(&[9u8; 16], &[2u8; 32]), 9u64)]
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
        let path = dir.path().join("relay_hold_first_observed.cbor");
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
        let path = dir.path().join("relay_hold_first_observed.cbor");
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
        // recover quarantines and returns empty
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

    #[test]
    fn persist_then_reload_first_observed_drives_expiry() {
        // Full sidecar round-trip: an OLD stamp persisted by a prior run, then
        // reloaded via the boot shape and restored into a fresh doc, ages the
        // never-covered entry out on gc(now).
        let dir = tempfile::tempdir().unwrap();
        let fo_path = dir.path().join("relay_hold_first_observed.cbor");
        let c = test_cipher();
        let key = RelayHoldDoc::key(&[1u8; 16], &[2u8; 32]);
        let seed: BTreeMap<String, u64> = [(key.clone(), 1u64)].into_iter().collect();
        save_first_observed(&c, &fo_path, &seed).unwrap();

        let mut doc = RelayHoldDoc::default();
        let mut e = sample_entry();
        e.pulled_by.clear(); // never covered → only TTL removes it
        doc.entries.insert(key.clone(), e);
        let now = crate::community_relay::RELAY_HOLD_TTL_MS + 10_000;
        doc.restore_first_observed(load_first_observed_or_recover(&c, &fo_path).unwrap(), now);
        doc.gc(now);
        assert!(
            !doc.entries.contains_key(&key),
            "reloaded old stamp aged the entry out"
        );
    }

    // ── expiry-tombstone sidecar (ZEB-924) ────────────────────────────────────

    #[test]
    fn expired_round_trips_and_missing_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RELAY_HOLD_EXPIRED_FILENAME);
        let c = test_cipher();
        assert!(
            load_expired(&c, &path).unwrap().is_empty(),
            "missing file → empty"
        );
        let mut m: BTreeMap<String, u64> = BTreeMap::new();
        m.insert("k1".into(), 42);
        save_expired(&c, &path, &m).unwrap();
        assert_eq!(load_expired(&c, &path).unwrap(), m);
    }

    #[test]
    fn load_expired_rejects_trailing_bytes_and_recover_quarantines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RELAY_HOLD_EXPIRED_FILENAME);
        let c = test_cipher();
        // Legacy v1 sidecar with a stray byte appended.
        let m: BTreeMap<String, u64> = [("k1".to_string(), 42u64)].into_iter().collect();
        let mut v1 = vec![1u8];
        ciborium::into_writer(&m, &mut v1).unwrap();
        v1.push(0x00);
        std::fs::write(&path, &v1).unwrap();
        assert!(matches!(
            load_expired(&c, &path).unwrap_err(),
            SyncError::CborDecode(_)
        ));
        assert!(load_expired_or_recover(&c, &path).unwrap().is_empty());
        assert!(!path.exists(), "corrupt file quarantined away");
    }

    #[test]
    fn persist_writes_expired_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let p = RelayHoldPersist {
            doc_path: dir.path().join("relay_hold.cbor"),
            replay_path: dir.path().join("relay_hold_replay.cbor"),
            first_observed_path: dir.path().join("relay_hold_first_observed.cbor"),
            expired_path: dir.path().join(RELAY_HOLD_EXPIRED_FILENAME),
            cipher: test_cipher(),
        };
        let mut doc = RelayHoldDoc::default();
        let m: BTreeMap<String, u64> = [("gone-key".to_string(), 7u64)].into_iter().collect();
        // Boot time near the stamp so restore's retention prune keeps it.
        doc.restore_expired(m.clone(), 7);
        use crate::fleet_sync::FleetPersist;
        p.persist(&doc, &BTreeMap::new()).unwrap();
        assert_eq!(load_expired(&p.cipher, &p.expired_path).unwrap(), m);
    }
}
