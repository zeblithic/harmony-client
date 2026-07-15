//! ZEB-510 step 2: on-disk persistence for `FleetPeerSeedDoc`. Same idiom as
//! `fleet_net_persist`: 1-byte schema-version prefix + plaintext CBOR, atomic
//! write via `owner_state_persist::save_atomically`, corrupt-file quarantine on
//! decode failure. Plaintext at rest is deliberate (dialing coordinates, not a
//! secret — same class as `fleet_net.cbor`, and captured over the SAS channel).

use crate::fleet_peer_seed::FleetPeerSeedDoc;
use crate::fleet_sync::SyncError;
use ciborium::{from_reader, into_writer};
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use std::path::Path;

/// File name for the persisted seed store. Lives at `<identity_dir>/…`.
pub const FLEET_PEER_SEED_FILENAME: &str = "fleet_peer_seed.cbor";

const FLEET_PEER_SEED_SCHEMA_V1: u8 = 1;

#[derive(Serialize, Deserialize)]
struct FleetPeerSeedFileV1(FleetPeerSeedDoc);

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), SyncError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| SyncError::Persist(format!("create_dir_all {}: {e}", path.display())))?;
    }
    crate::owner_state_persist::save_atomically(path, bytes)
        .map_err(|e| SyncError::Persist(e.to_string()))
}

/// Load the seed doc. Returns `Ok(default())` when the file does not exist.
pub fn load(path: &Path) -> Result<FleetPeerSeedDoc, SyncError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FleetPeerSeedDoc::default())
        }
        Err(e) => return Err(SyncError::Persist(format!("read {}: {e}", path.display()))),
    };
    if bytes.is_empty() {
        return Err(SyncError::CborDecode(format!(
            "fleet-peer-seed file is empty: {}",
            path.display()
        )));
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    match version {
        FLEET_PEER_SEED_SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let file: FleetPeerSeedFileV1 = from_reader(&mut cursor)
                .map_err(|e| SyncError::CborDecode(format!("load {}: {e}", path.display())))?;
            let pos = cursor.position() as usize;
            if pos != payload.len() {
                return Err(SyncError::CborDecode(format!(
                    "trailing bytes after fleet-peer-seed value: consumed {} of {}",
                    pos,
                    payload.len()
                )));
            }
            Ok(file.0)
        }
        v => Err(SyncError::CborDecode(format!(
            "unknown fleet-peer-seed schema version {v:#x} in {}",
            path.display()
        ))),
    }
}

/// Load the seed doc, quarantining a genuinely-corrupt file and self-healing to
/// `default()` so boot never bricks; transient I/O errors are propagated.
pub fn load_doc_or_recover(path: &Path) -> Result<FleetPeerSeedDoc, SyncError> {
    match load(path) {
        Ok(doc) => Ok(doc),
        Err(e @ SyncError::CborDecode(_)) => {
            quarantine(path, &e);
            Ok(FleetPeerSeedDoc::default())
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
        "fleet-peer-seed load failed; quarantining corrupt file and starting fresh (bytes preserved)");
    if let Err(re) = std::fs::rename(path, &corrupt) {
        tracing::warn!(path = %path.display(), error = %re, "failed to quarantine corrupt fleet-peer-seed file");
    }
}

/// Save the seed doc atomically (tempfile + fsync + parent-dir fsync + rename).
pub fn save(path: &Path, doc: &FleetPeerSeedDoc) -> Result<(), SyncError> {
    let mut bytes = vec![FLEET_PEER_SEED_SCHEMA_V1];
    into_writer(&FleetPeerSeedFileV1(doc.clone()), &mut bytes)
        .map_err(|e| SyncError::CborEncode(format!("encode {}: {e}", path.display())))?;
    atomic_write(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet_peer_seed::FleetPeerSeedRow;

    #[test]
    fn save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FLEET_PEER_SEED_FILENAME);
        // Missing file → default.
        assert_eq!(load(&path).unwrap(), FleetPeerSeedDoc::default());

        let mut doc = FleetPeerSeedDoc::default();
        doc.seeds.insert(
            "ab".repeat(32),
            FleetPeerSeedRow {
                iroh_node_id: [0xAB; 32],
                home_relay: "r".into(),
                observed_at_ms: 7,
            },
        );
        save(&path, &doc).unwrap();
        assert_eq!(load(&path).unwrap(), doc);
    }

    #[test]
    fn corrupt_file_is_quarantined_and_recovers_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FLEET_PEER_SEED_FILENAME);
        std::fs::write(&path, [0x01, 0xff, 0xff, 0xff]).unwrap(); // valid version byte, junk CBOR
        let recovered = load_doc_or_recover(&path).unwrap();
        assert_eq!(recovered, FleetPeerSeedDoc::default());
        // Original path is gone (renamed to .corrupt-*).
        assert!(!path.exists());
    }

    #[test]
    fn trailing_bytes_after_value_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FLEET_PEER_SEED_FILENAME);
        // Write a valid doc, then append one extra CBOR token after the value.
        save(&path, &FleetPeerSeedDoc::default()).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(0x00); // integer 0 — a distinct trailing value
        std::fs::write(&path, &bytes).unwrap();
        match load(&path) {
            Err(SyncError::CborDecode(msg)) => {
                assert!(msg.contains("trailing bytes"), "got: {msg}")
            }
            other => panic!("expected CborDecode trailing-bytes, got {other:?}"),
        }
    }

    #[test]
    fn transient_io_error_propagates_not_quarantined() {
        // Reading a path that IS a directory fails with a non-NotFound,
        // non-decode IO error → load_doc_or_recover must PROPAGATE it, not
        // self-heal to default() (which is reserved for genuinely-corrupt files).
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load_doc_or_recover(dir.path()).is_err(),
            "transient IO error must propagate, not quarantine-to-default"
        );
    }
}
