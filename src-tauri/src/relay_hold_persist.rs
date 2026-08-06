//! On-disk persistence for RelayHoldDoc and its replay tracker (ZEB-458 P4 B).
//! Mirrors `dm_inbox_persist` exactly: atomic-rename + file fsync + parent-dir
//! fsync via `owner_state_persist::save_atomically`, a 1-byte schema-version
//! prefix (plaintext CBOR), strict trailing-byte rejection, and a quarantine-
//! on-corruption recovery path so a bad load never silently overwrites the
//! relay's held blobs on the next persist.
//!
//! `RelayHoldDoc` is not encrypted at rest (the held `sealed_blob`s inside are
//! already sealed to the recipient device — the relay holds them opaque).

use crate::community_relay_hold_crdt::RelayHoldDoc;
use crate::fleet_sync::SyncError;
use crate::owner_state_types::Hlc;
use ciborium::{from_reader, into_writer};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::Path;

/// File name for the persisted RelayHoldDoc. Lives at
/// `<identity_dir>/relay_hold.cbor`.
pub const RELAY_HOLD_FILENAME: &str = "relay_hold.cbor";

/// File name for the persisted replay tracker. Lives alongside
/// `relay_hold.cbor`.
pub const RELAY_HOLD_REPLAY_FILENAME: &str = "relay_hold_replay.cbor";

const RELAY_HOLD_SCHEMA_V1: u8 = 1;
const RELAY_HOLD_REPLAY_SCHEMA_V1: u8 = 1;

#[derive(Serialize, Deserialize)]
struct RelayHoldFileV1(RelayHoldDoc);

#[derive(Serialize, Deserialize)]
struct RelayHoldReplayFileV1(BTreeMap<String, Hlc>);

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

// ── RelayHoldDoc ───────────────────────────────────────────────────────────────

/// Load `RelayHoldDoc` from `path`. Returns `Ok(RelayHoldDoc::default())` if
/// the file does not exist yet.
pub fn load(path: &Path) -> Result<RelayHoldDoc, SyncError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(RelayHoldDoc::default()),
        Err(e) => return Err(SyncError::Persist(format!("read {}: {e}", path.display()))),
    };
    if bytes.is_empty() {
        return Err(SyncError::CborDecode(format!(
            "relay-hold file is empty: {}",
            path.display()
        )));
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    match version {
        RELAY_HOLD_SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let file: RelayHoldFileV1 = from_reader(&mut cursor)
                .map_err(|e| SyncError::CborDecode(format!("load {}: {e}", path.display())))?;
            // Reject trailing bytes after the CBOR value (mirrors
            // owner_state_crypto::canonical_cbor_decode): a corrupt file that is
            // valid-prefix + garbage must NOT decode as "valid".
            let pos = cursor.position() as usize;
            if pos != payload.len() {
                return Err(SyncError::CborDecode(format!(
                    "trailing bytes after relay-hold value: consumed {} of {}",
                    pos,
                    payload.len()
                )));
            }
            Ok(file.0)
        }
        v => Err(SyncError::CborDecode(format!(
            "unknown relay-hold schema version {v:#x} in {}",
            path.display()
        ))),
    }
}

/// Load the relay-hold doc, recovering from genuine on-disk corruption.
///
/// - `Ok(doc)` on success (or `Ok(default)` when the file is missing).
/// - `Err(SyncError::CborDecode)` (permanent corruption) → quarantine the bad
///   file aside (`.corrupt-<ms>`, bytes preserved) and self-heal to
///   `Ok(default())` so the app still boots and never bricks on corruption.
/// - any other error — a transient I/O failure (`SyncError::Persist`) → the file
///   is left untouched and the error is propagated (ZEB-460); quarantining it
///   would orphan held blobs and let the next persist overwrite them with an
///   empty doc, so the caller fails the boot loudly and retries next launch with
///   the file intact.
pub fn load_doc_or_recover(path: &Path) -> Result<RelayHoldDoc, SyncError> {
    match load(path) {
        Ok(doc) => Ok(doc),
        Err(e @ SyncError::CborDecode(_)) => {
            quarantine(path, &e);
            Ok(RelayHoldDoc::default())
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
        "relay-hold persistence load failed; quarantining corrupt file and starting fresh (bytes preserved)");
    if let Err(re) = std::fs::rename(path, &corrupt) {
        tracing::warn!(path = %path.display(), error = %re, "failed to quarantine corrupt relay-hold file");
    }
}

/// Save `RelayHoldDoc` to `path` atomically (tempfile + fsync + parent-dir
/// fsync + rename). Creates parent directories if needed.
pub fn save(path: &Path, doc: &RelayHoldDoc) -> Result<(), SyncError> {
    let mut bytes = vec![RELAY_HOLD_SCHEMA_V1];
    into_writer(&RelayHoldFileV1(doc.clone()), &mut bytes)
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
            "relay-hold replay file is empty: {}",
            path.display()
        )));
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    match version {
        RELAY_HOLD_REPLAY_SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let file: RelayHoldReplayFileV1 = from_reader(&mut cursor).map_err(|e| {
                SyncError::CborDecode(format!("load_replay {}: {e}", path.display()))
            })?;
            // Reject trailing bytes after the CBOR value (mirrors
            // owner_state_crypto::canonical_cbor_decode).
            let pos = cursor.position() as usize;
            if pos != payload.len() {
                return Err(SyncError::CborDecode(format!(
                    "trailing bytes after relay-hold replay value: consumed {} of {}",
                    pos,
                    payload.len()
                )));
            }
            Ok(file.0)
        }
        v => Err(SyncError::CborDecode(format!(
            "unknown relay-hold replay schema version {v:#x} in {}",
            path.display()
        ))),
    }
}

/// Save the replay tracker to `path` atomically.
pub fn save_replay(path: &Path, tracker: &BTreeMap<String, Hlc>) -> Result<(), SyncError> {
    let mut bytes = vec![RELAY_HOLD_REPLAY_SCHEMA_V1];
    into_writer(&RelayHoldReplayFileV1(tracker.clone()), &mut bytes)
        .map_err(|e| SyncError::CborEncode(format!("encode replay {}: {e}", path.display())))?;
    atomic_write(path, &bytes)
}

// ── first-observed sidecar (ZEB-862) ───────────────────────────────────────────

/// File name for the persisted LOCAL first-observation clock. Lives alongside
/// `relay_hold.cbor`. Local-only: never replicated, never on the wire — it
/// makes the `#[serde(skip)]` `RelayHoldDoc::first_observed_ms` TTL clock
/// survive restart instead of re-stamping `now` on the first post-boot sweep.
pub const RELAY_HOLD_FIRST_OBSERVED_FILENAME: &str = "relay_hold_first_observed.cbor";

const RELAY_HOLD_FIRST_OBSERVED_SCHEMA_V1: u8 = 1;

#[derive(Serialize, Deserialize)]
struct RelayHoldFirstObservedFileV1(BTreeMap<String, u64>);

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
            "relay-hold first-observed file is empty: {}",
            path.display()
        )));
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    match version {
        RELAY_HOLD_FIRST_OBSERVED_SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let file: RelayHoldFirstObservedFileV1 = from_reader(&mut cursor).map_err(|e| {
                SyncError::CborDecode(format!("load_first_observed {}: {e}", path.display()))
            })?;
            // Reject trailing bytes after the CBOR value (mirrors `load`).
            let pos = cursor.position() as usize;
            if pos != payload.len() {
                return Err(SyncError::CborDecode(format!(
                    "trailing bytes after relay-hold first-observed value: consumed {} of {}",
                    pos,
                    payload.len()
                )));
            }
            Ok(file.0)
        }
        v => Err(SyncError::CborDecode(format!(
            "unknown relay-hold first-observed schema version {v:#x} in {}",
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
    let mut bytes = vec![RELAY_HOLD_FIRST_OBSERVED_SCHEMA_V1];
    into_writer(&RelayHoldFirstObservedFileV1(map.clone()), &mut bytes).map_err(|e| {
        SyncError::CborEncode(format!("encode first-observed {}: {e}", path.display()))
    })?;
    atomic_write(path, &bytes)
}

// ── FleetPersist impl ─────────────────────────────────────────────────────────

/// Durability sink for the relay-hold fleet-sync engine. Holds the absolute
/// paths for both the doc and replay-tracker files. The engine calls
/// `persist` inside a `spawn_blocking` (fleet_sync.rs), so this impl stays
/// synchronous like `DmInboxPersist`.
pub struct RelayHoldPersist {
    pub doc_path: std::path::PathBuf,
    pub replay_path: std::path::PathBuf,
    /// ZEB-862: local-only first-observation clock sidecar (see
    /// `RELAY_HOLD_FIRST_OBSERVED_FILENAME`).
    pub first_observed_path: std::path::PathBuf,
}

impl crate::fleet_sync::FleetPersist<RelayHoldDoc> for RelayHoldPersist {
    fn persist(
        &self,
        state: &RelayHoldDoc,
        tracker: &BTreeMap<String, Hlc>,
    ) -> Result<(), SyncError> {
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
    use crate::community_relay_hold_crdt::{RelayHoldDoc, RelayHoldEntry};
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
        assert_eq!(load(&path).unwrap(), RelayHoldDoc::default());
        let doc = sample_doc();
        save(&path, &doc).unwrap();
        assert_eq!(load(&path).unwrap(), doc);
    }

    #[test]
    fn load_rejects_trailing_bytes_after_valid_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay_hold.cbor");
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
        assert_eq!(recovered, RelayHoldDoc::default());
        assert!(!path.exists(), "corrupt file was quarantined");
    }

    #[test]
    fn replay_round_trips_and_missing_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay_hold_replay.cbor");
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay_hold.cbor");
        std::fs::write(&path, [0xFF_u8, 0x01, 0x02]).unwrap();
        let doc = load_doc_or_recover(&path).unwrap();
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
        assert_eq!(load_doc_or_recover(&path).unwrap(), RelayHoldDoc::default());
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
        let path = dir.path().join("relay_hold_replay.cbor");
        std::fs::write(&path, [0xFF_u8]).unwrap();
        let tracker = load_replay_or_recover(&path).unwrap();
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
        assert_eq!(load(&p.doc_path).unwrap(), doc);
        assert_eq!(load_replay(&p.replay_path).unwrap(), t);
        assert_eq!(load_first_observed(&p.first_observed_path).unwrap(), fo);
    }

    // ── first-observed sidecar (ZEB-862) ──────────────────────────────────────

    #[test]
    fn first_observed_round_trips_and_missing_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay_hold_first_observed.cbor");
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
        let path = dir.path().join("relay_hold_first_observed.cbor");
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
        // recover quarantines and returns empty
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

    #[test]
    fn persist_then_reload_first_observed_drives_expiry() {
        // Full sidecar round-trip: an OLD stamp persisted by a prior run, then
        // reloaded via the boot shape and restored into a fresh doc, ages the
        // never-covered entry out on gc(now).
        let dir = tempfile::tempdir().unwrap();
        let fo_path = dir.path().join("relay_hold_first_observed.cbor");
        let key = RelayHoldDoc::key(&[1u8; 16], &[2u8; 32]);
        let seed: BTreeMap<String, u64> = [(key.clone(), 1u64)].into_iter().collect();
        save_first_observed(&fo_path, &seed).unwrap();

        let mut doc = RelayHoldDoc::default();
        let mut e = sample_entry();
        e.pulled_by.clear(); // never covered → only TTL removes it
        doc.entries.insert(key.clone(), e);
        let now = crate::community_relay::RELAY_HOLD_TTL_MS + 10_000;
        doc.restore_first_observed(load_first_observed_or_recover(&fo_path).unwrap(), now);
        doc.gc(now);
        assert!(
            !doc.entries.contains_key(&key),
            "reloaded old stamp aged the entry out"
        );
    }
}
