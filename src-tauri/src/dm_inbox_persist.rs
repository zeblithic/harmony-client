//! On-disk persistence for DmInboxDoc and its replay tracker (ZEB-418 P1).
//! Atomic-rename + file fsync + parent-dir fsync via
//! `owner_state_persist::save_atomically`, mirroring `notes_persist`.
//!
//! Both files use a 1-byte schema-version prefix (plaintext CBOR) — identical
//! format to `owner_state_persist`'s `ReplayFileV1`. `DmInboxDoc` is not
//! encrypted at rest (owner-state CRDT is also plaintext CBOR on disk; the
//! deposited payloads inside are already sealed storage blobs).

use crate::dm_inbox_crdt::DmInboxDoc;
use crate::fleet_sync::SyncError;
use crate::owner_state_types::Hlc;
use ciborium::{from_reader, into_writer};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::Path;

/// File name for the persisted DmInboxDoc. Lives at
/// `<app_data_dir>/dm_inbox/dm_inbox.cbor`.
pub const DM_INBOX_FILENAME: &str = "dm_inbox.cbor";

/// File name for the persisted replay tracker. Lives alongside `dm_inbox.cbor`.
pub const DM_INBOX_REPLAY_FILENAME: &str = "dm_inbox_replay.cbor";

const DM_INBOX_SCHEMA_V1: u8 = 1;
const DM_INBOX_REPLAY_SCHEMA_V1: u8 = 1;

#[derive(Serialize, Deserialize)]
struct DmInboxFileV1(DmInboxDoc);

#[derive(Serialize, Deserialize)]
struct DmInboxReplayFileV1(BTreeMap<String, Hlc>);

// ── helpers ──────────────────────────────────────────────────────────────────

/// Atomic write with parent-directory fsync (crash-durable rename). Routes
/// through `owner_state_persist::save_atomically`, which fsyncs both the
/// tempfile and (on Unix) the parent directory entry.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), SyncError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| SyncError::Persist(format!("create_dir_all {}: {e}", path.display())))?;
    }
    crate::owner_state_persist::save_atomically(path, bytes)
        .map_err(|e| SyncError::Persist(e.to_string()))
}

// ── DmInboxDoc ───────────────────────────────────────────────────────────────

/// Load `DmInboxDoc` from `path`. Returns `Ok(DmInboxDoc::default())` if the
/// file does not exist yet.
pub fn load(path: &Path) -> Result<DmInboxDoc, SyncError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(DmInboxDoc::default()),
        Err(e) => return Err(SyncError::Persist(format!("read {}: {e}", path.display()))),
    };
    if bytes.is_empty() {
        return Err(SyncError::CborDecode(format!(
            "dm-inbox file is empty: {}",
            path.display()
        )));
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    match version {
        DM_INBOX_SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let file: DmInboxFileV1 = from_reader(&mut cursor)
                .map_err(|e| SyncError::CborDecode(format!("load {}: {e}", path.display())))?;
            // Reject trailing bytes after the CBOR value (mirrors
            // owner_state_crypto::canonical_cbor_decode): a corrupt file that is
            // valid-prefix + garbage must NOT decode as "valid".
            let pos = cursor.position() as usize;
            if pos != payload.len() {
                return Err(SyncError::CborDecode(format!(
                    "trailing bytes after dm-inbox value: consumed {} of {}",
                    pos,
                    payload.len()
                )));
            }
            Ok(file.0)
        }
        v => Err(SyncError::CborDecode(format!(
            "unknown dm-inbox schema version {v:#x} in {}",
            path.display()
        ))),
    }
}

/// Load the dm-inbox doc, recovering from genuine on-disk corruption.
///
/// - `Ok(doc)` on success (or `Ok(default)` when the file is missing).
/// - `Err(SyncError::CborDecode)` (permanent corruption: empty / unknown-schema
///   / decode-fail / trailing-bytes) → quarantine the bad file aside
///   (`.corrupt-<ms>`, bytes preserved) and self-heal to `Ok(default())` so the
///   app still boots and never bricks on a corrupt file.
/// - any other error — i.e. a transient I/O failure (`SyncError::Persist`) → the
///   file is left untouched and the error is propagated (ZEB-460). Quarantining
///   a transient failure would orphan real data and let the next persist write
///   an empty doc over it; the caller instead fails the boot loudly and retries
///   next launch with the file intact.
pub fn load_doc_or_recover(path: &Path) -> Result<DmInboxDoc, SyncError> {
    match load(path) {
        Ok(doc) => Ok(doc),
        Err(e @ SyncError::CborDecode(_)) => {
            quarantine(path, &e);
            Ok(DmInboxDoc::default())
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
        "dm-inbox persistence load failed; quarantining corrupt file and starting fresh (bytes preserved)");
    if let Err(re) = std::fs::rename(path, &corrupt) {
        tracing::warn!(path = %path.display(), error = %re, "failed to quarantine corrupt dm-inbox file");
    }
}

/// Save `DmInboxDoc` to `path` atomically (tempfile + fsync + parent-dir fsync
/// + rename). Creates parent directories if needed.
pub fn save(path: &Path, doc: &DmInboxDoc) -> Result<(), SyncError> {
    let mut bytes = vec![DM_INBOX_SCHEMA_V1];
    into_writer(&DmInboxFileV1(doc.clone()), &mut bytes)
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
            "dm-inbox replay file is empty: {}",
            path.display()
        )));
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    match version {
        DM_INBOX_REPLAY_SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let file: DmInboxReplayFileV1 = from_reader(&mut cursor).map_err(|e| {
                SyncError::CborDecode(format!("load_replay {}: {e}", path.display()))
            })?;
            // Reject trailing bytes after the CBOR value (mirrors
            // owner_state_crypto::canonical_cbor_decode).
            let pos = cursor.position() as usize;
            if pos != payload.len() {
                return Err(SyncError::CborDecode(format!(
                    "trailing bytes after dm-inbox replay value: consumed {} of {}",
                    pos,
                    payload.len()
                )));
            }
            Ok(file.0)
        }
        v => Err(SyncError::CborDecode(format!(
            "unknown dm-inbox replay schema version {v:#x} in {}",
            path.display()
        ))),
    }
}

/// Save the replay tracker to `path` atomically.
pub fn save_replay(path: &Path, tracker: &BTreeMap<String, Hlc>) -> Result<(), SyncError> {
    let mut bytes = vec![DM_INBOX_REPLAY_SCHEMA_V1];
    into_writer(&DmInboxReplayFileV1(tracker.clone()), &mut bytes)
        .map_err(|e| SyncError::CborEncode(format!("encode replay {}: {e}", path.display())))?;
    atomic_write(path, &bytes)
}

// ── first-observed sidecar (ZEB-862) ───────────────────────────────────────────

/// File name for the persisted LOCAL first-observation clock. Lives alongside
/// `dm_inbox.cbor`. Local-only: never replicated, never on the wire — it makes
/// the `#[serde(skip)]` `DmInboxDoc::first_observed_ms` TTL clock survive
/// restart instead of re-stamping `now` on the first post-boot sweep.
pub const DM_INBOX_FIRST_OBSERVED_FILENAME: &str = "dm_inbox_first_observed.cbor";

const DM_INBOX_FIRST_OBSERVED_SCHEMA_V1: u8 = 1;

#[derive(Serialize, Deserialize)]
struct DmInboxFirstObservedFileV1(BTreeMap<String, u64>);

/// Load the LOCAL first-observation clock from `path`. Returns
/// `Ok(BTreeMap::new())` if the file does not exist yet (→ today's re-stamp
/// behavior; no doc-file migration needed).
pub fn load_first_observed(path: &Path) -> Result<BTreeMap<String, u64>, SyncError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(SyncError::Persist(format!("read {}: {e}", path.display()))),
    };
    if bytes.is_empty() {
        return Err(SyncError::CborDecode(format!(
            "dm-inbox first-observed file is empty: {}",
            path.display()
        )));
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    match version {
        DM_INBOX_FIRST_OBSERVED_SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let file: DmInboxFirstObservedFileV1 = from_reader(&mut cursor).map_err(|e| {
                SyncError::CborDecode(format!("load_first_observed {}: {e}", path.display()))
            })?;
            // Reject trailing bytes after the CBOR value (mirrors `load`).
            let pos = cursor.position() as usize;
            if pos != payload.len() {
                return Err(SyncError::CborDecode(format!(
                    "trailing bytes after dm-inbox first-observed value: consumed {} of {}",
                    pos,
                    payload.len()
                )));
            }
            Ok(file.0)
        }
        v => Err(SyncError::CborDecode(format!(
            "unknown dm-inbox first-observed schema version {v:#x} in {}",
            path.display()
        ))),
    }
}

/// Same recovery contract as [`load_doc_or_recover`]: `CborDecode` corruption is
/// quarantined (`.corrupt-<ms>`, bytes preserved) and an empty map returned; a
/// transient `Persist` error is left untouched and propagated (ZEB-460). A
/// missing/empty clock is safe — the next sweep re-stamps `now`, exactly today's
/// behavior — so quarantine-to-empty never loses correctness, only punctuality.
pub fn load_first_observed_or_recover(path: &Path) -> Result<BTreeMap<String, u64>, SyncError> {
    match load_first_observed(path) {
        Ok(m) => Ok(m),
        Err(e @ SyncError::CborDecode(_)) => {
            quarantine(path, &e);
            Ok(BTreeMap::new())
        }
        Err(e) => Err(e),
    }
}

/// Save the LOCAL first-observation clock to `path` atomically.
pub fn save_first_observed(path: &Path, map: &BTreeMap<String, u64>) -> Result<(), SyncError> {
    let mut bytes = vec![DM_INBOX_FIRST_OBSERVED_SCHEMA_V1];
    into_writer(&DmInboxFirstObservedFileV1(map.clone()), &mut bytes).map_err(|e| {
        SyncError::CborEncode(format!("encode first-observed {}: {e}", path.display()))
    })?;
    atomic_write(path, &bytes)
}

// ── expired-tombstone sidecar (ZEB-925) ───────────────────────────────────────

/// File name for the persisted LOCAL expiry-tombstone map. Lives alongside
/// `dm_inbox.cbor`. Local-only: never replicated, never on the wire — it makes
/// the `#[serde(skip)]` `DmInboxDoc::expired_at_ms` suppression survive
/// restart (a tombstone that forgot across reboot would let a sibling's merge
/// resurrect the expired entry with a fresh TTL window).
pub const DM_INBOX_EXPIRED_FILENAME: &str = "dm_inbox_expired.cbor";

const DM_INBOX_EXPIRED_SCHEMA_V1: u8 = 1;

#[derive(Serialize, Deserialize)]
struct DmInboxExpiredFileV1(BTreeMap<String, u64>);

/// Load the LOCAL expiry-tombstone map from `path`. Returns
/// `Ok(BTreeMap::new())` if the file does not exist yet (→ no suppression;
/// worst case one extra TTL window per resurrection — exactly pre-ZEB-925
/// behavior, so no doc-file migration is needed).
pub fn load_expired(path: &Path) -> Result<BTreeMap<String, u64>, SyncError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(SyncError::Persist(format!("read {}: {e}", path.display()))),
    };
    if bytes.is_empty() {
        return Err(SyncError::CborDecode(format!(
            "dm-inbox expired file is empty: {}",
            path.display()
        )));
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    match version {
        DM_INBOX_EXPIRED_SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let file: DmInboxExpiredFileV1 = from_reader(&mut cursor).map_err(|e| {
                SyncError::CborDecode(format!("load_expired {}: {e}", path.display()))
            })?;
            // Reject trailing bytes after the CBOR value (mirrors `load`).
            let pos = cursor.position() as usize;
            if pos != payload.len() {
                return Err(SyncError::CborDecode(format!(
                    "trailing bytes after dm-inbox expired value: consumed {} of {}",
                    pos,
                    payload.len()
                )));
            }
            Ok(file.0)
        }
        v => Err(SyncError::CborDecode(format!(
            "unknown dm-inbox expired schema version {v:#x} in {}",
            path.display()
        ))),
    }
}

/// Same recovery contract as [`load_doc_or_recover`]: `CborDecode` corruption
/// is quarantined (`.corrupt-<ms>`, bytes preserved) and an empty map
/// returned; a transient `Persist` error is left untouched and propagated
/// (ZEB-460). A missing/empty map is safe — suppression is lost, retention
/// falls back to one TTL window per resurrection until re-tombstoned.
pub fn load_expired_or_recover(path: &Path) -> Result<BTreeMap<String, u64>, SyncError> {
    match load_expired(path) {
        Ok(m) => Ok(m),
        Err(e @ SyncError::CborDecode(_)) => {
            quarantine(path, &e);
            Ok(BTreeMap::new())
        }
        Err(e) => Err(e),
    }
}

/// Save the LOCAL expiry-tombstone map to `path` atomically.
pub fn save_expired(path: &Path, map: &BTreeMap<String, u64>) -> Result<(), SyncError> {
    let mut bytes = vec![DM_INBOX_EXPIRED_SCHEMA_V1];
    into_writer(&DmInboxExpiredFileV1(map.clone()), &mut bytes)
        .map_err(|e| SyncError::CborEncode(format!("encode expired {}: {e}", path.display())))?;
    atomic_write(path, &bytes)
}

// ── FleetPersist impl ─────────────────────────────────────────────────────────

/// Durability sink for the dm-inbox fleet-sync engine. Holds the absolute
/// paths for both the doc and replay-tracker files. The engine calls
/// `persist` inside a `spawn_blocking` (fleet_sync.rs), so this impl stays
/// synchronous like `NotesPersist`.
pub struct DmInboxPersist {
    pub doc_path: std::path::PathBuf,
    pub replay_path: std::path::PathBuf,
    /// ZEB-862: local-only first-observation clock sidecar (see
    /// `DM_INBOX_FIRST_OBSERVED_FILENAME`).
    pub first_observed_path: std::path::PathBuf,
    /// ZEB-925: local-only expiry-tombstone sidecar (see
    /// `DM_INBOX_EXPIRED_FILENAME`).
    pub expired_path: std::path::PathBuf,
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
        save_expired(&self.expired_path, state.expired_at_ms())?;
        save(&self.doc_path, state)?;
        save_replay(&self.replay_path, tracker)?;
        save_first_observed(&self.first_observed_path, state.first_observed_ms())?;
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dm_inbox_crdt::{DmInboxDoc, DmInboxEntry};
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
        assert_eq!(load(&path).unwrap(), DmInboxDoc::default());
        let doc = sample_doc();
        save(&path, &doc).unwrap();
        assert_eq!(load(&path).unwrap(), doc);
    }

    #[test]
    fn load_rejects_trailing_bytes_after_valid_value() {
        // A valid saved file with a stray byte appended must NOT decode as
        // "valid" — it should surface an Err so load_doc_or_recover quarantines
        // it (mirrors owner_state_crypto::canonical_cbor_decode strictness).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dm_inbox.cbor");
        let doc = sample_doc();
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
        assert_eq!(recovered, DmInboxDoc::default());
        assert!(!path.exists(), "corrupt file was quarantined");
    }

    #[test]
    fn replay_round_trips_and_missing_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dm_inbox_replay.cbor");
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
        // A corrupt dm-inbox file must NOT silently become an empty doc that
        // overwrites pending deposits on the next persist: the recovery path
        // renames the bad bytes aside and returns a fresh default.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dm_inbox.cbor");
        // Unknown schema version byte → load() returns Err(CborDecode).
        std::fs::write(&path, [0xFF_u8, 0x01, 0x02]).unwrap();
        let doc = load_doc_or_recover(&path).unwrap();
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
        // NotFound is NOT a corruption: load() returns Ok(default), so no
        // quarantine file should be created.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dm_inbox.cbor");
        assert_eq!(load_doc_or_recover(&path).unwrap(), DmInboxDoc::default());
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
        let path = dir.path().join("dm_inbox.cbor");
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
        let path = dir.path().join("dm_inbox_replay.cbor");
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
        let path = dir.path().join("dm_inbox_replay.cbor");
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
        assert_eq!(load(&p.doc_path).unwrap(), doc);
        assert_eq!(load_replay(&p.replay_path).unwrap(), t);
        assert_eq!(load_first_observed(&p.first_observed_path).unwrap(), fo);
    }

    // ── first-observed sidecar (ZEB-862) ──────────────────────────────────────

    #[test]
    fn first_observed_round_trips_and_missing_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dm_inbox_first_observed.cbor");
        assert!(load_first_observed(&path).unwrap().is_empty());
        let mut m = std::collections::BTreeMap::new();
        m.insert("k1".to_string(), 111u64);
        m.insert("k2".to_string(), 222u64);
        save_first_observed(&path, &m).unwrap();
        assert_eq!(load_first_observed(&path).unwrap(), m);
    }

    #[test]
    fn load_first_observed_rejects_trailing_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dm_inbox_first_observed.cbor");
        let mut m = std::collections::BTreeMap::new();
        m.insert("k".to_string(), 5u64);
        save_first_observed(&path, &m).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(0xFF);
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            load_first_observed(&path).unwrap_err(),
            SyncError::CborDecode(_)
        ));
        assert!(load_first_observed_or_recover(&path).unwrap().is_empty());
        assert!(!path.exists(), "corrupt sidecar was quarantined");
    }

    #[test]
    fn load_first_observed_or_recover_propagates_transient_io_without_quarantine() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fo.cbor");
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(
            load_first_observed_or_recover(&path).unwrap_err(),
            SyncError::Persist(_)
        ));
        assert!(path.is_dir(), "transient error leaves the path untouched");
    }

    // ── expired-tombstone sidecar (ZEB-925) ──────────────────────────────────

    #[test]
    fn expired_round_trips_and_missing_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dm_inbox_expired.cbor");
        assert!(load_expired(&path).unwrap().is_empty());
        let mut m = std::collections::BTreeMap::new();
        m.insert("k1".to_string(), 111u64);
        m.insert("k2".to_string(), 222u64);
        save_expired(&path, &m).unwrap();
        assert_eq!(load_expired(&path).unwrap(), m);
    }

    #[test]
    fn load_expired_rejects_trailing_bytes_and_recover_quarantines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dm_inbox_expired.cbor");
        let mut m = std::collections::BTreeMap::new();
        m.insert("k".to_string(), 5u64);
        save_expired(&path, &m).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(0xFF);
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            load_expired(&path).unwrap_err(),
            SyncError::CborDecode(_)
        ));
        assert!(load_expired_or_recover(&path).unwrap().is_empty());
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
        };
        use crate::fleet_sync::FleetPersist;
        let mut doc = sample_doc();
        let m: std::collections::BTreeMap<String, u64> =
            [("gone-key".to_string(), 7u64)].into_iter().collect();
        // Boot time near the stamp so the restore-time retention prune keeps it.
        doc.restore_expired(m.clone(), 7);
        p.persist(&doc, &std::collections::BTreeMap::new()).unwrap();
        assert_eq!(load_expired(&p.expired_path).unwrap(), m);
    }

    #[test]
    fn persist_then_reload_first_observed_drives_expiry() {
        // Full sidecar round-trip: an OLD stamp persisted by a prior run, then
        // reloaded via the boot shape and restored into a fresh doc, ages the
        // never-covered entry out on gc_expired(now, {}).
        let dir = tempfile::tempdir().unwrap();
        let fo_path = dir.path().join("dm_inbox_first_observed.cbor");
        let key = DmInboxDoc::key(&[1u8; 16], &[2u8; 32]);
        let seed: BTreeMap<String, u64> = [(key.clone(), 1u64)].into_iter().collect();
        save_first_observed(&fo_path, &seed).unwrap();

        let mut doc = DmInboxDoc::default();
        let mut e = sample_entry();
        e.ingested_by.clear(); // never covered → only TTL removes it
        doc.entries.insert(key.clone(), e);
        let now = crate::butler_deposit::INBOX_TTL_MS + 10_000;
        doc.restore_first_observed(load_first_observed_or_recover(&fo_path).unwrap(), now);
        doc.gc_expired(now, &std::collections::BTreeSet::new());
        assert!(
            !doc.entries.contains_key(&key),
            "reloaded old stamp aged the entry out"
        );
    }
}
