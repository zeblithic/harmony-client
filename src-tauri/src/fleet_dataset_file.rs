//! ZEB-981: shared persistence for fleet-synced dataset files — versioned,
//! SEALED at rest, atomic, with the ZEB-460 transient-vs-corrupt recovery
//! contract. Replaces the load/save/quarantine code previously copy-pasted
//! across the per-dataset `*_persist.rs` modules.
//!
//! On-disk format:
//!
//! - v1 (legacy, read-only): `[inner_schema u8] ‖ plaintext CBOR`
//! - v2 (written):           `[0x02] ‖ nonce(12) ‖ AEAD(inner) ‖ tag(16)`
//!
//! where the AEAD plaintext ("inner") is the complete former v1 image.
//! Sealing wraps the whole v1 image so content-schema evolution stays
//! orthogonal to encryption and migration is "seal the bytes you already
//! have". Key: the pinned epoch-0 fleet KeyTree's `friend_aead` sub-key with
//! a per-file AAD label ([`crate::owner_state_crypto::seal_dataset_file`]) —
//! see `docs/specs/2026-08-23-zeb-981-dataset-at-rest-encryption-design.md`.

use crate::fleet_sync::SyncError;
use crate::owner_state_crypto::KeyTree;
use ciborium::{from_reader, into_writer};
use serde::{de::DeserializeOwned, Serialize};
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use zeroize::Zeroizing;

/// Outer version byte for sealed files. Inner (legacy) schema bytes are
/// per-dataset and must never equal this value (all are 1 today).
pub const SEALED_SCHEMA_V2: u8 = 2;

/// Sealing context shared by every dataset persist: the pinned epoch-0
/// KeyTree (epoch-independent like friend secrets — a tag failure is never
/// "rotated epoch", only corruption or a foreign-identity file). Cheap to
/// clone.
#[derive(Clone)]
pub struct DatasetCipher {
    keys: Arc<KeyTree>,
}

impl DatasetCipher {
    pub fn new(keys: Arc<KeyTree>) -> Self {
        Self { keys }
    }
}

/// Deterministic cipher for tests (unit + integration via `test-fixtures`).
#[cfg(any(test, feature = "test-fixtures"))]
pub fn test_cipher() -> DatasetCipher {
    DatasetCipher::new(Arc::new(
        KeyTree::derive(&[0u8; 32]).expect("test KeyTree derives"),
    ))
}

/// Atomic write with parent-dir creation + crash-durable rename, via
/// `owner_state_persist::save_atomically`.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), SyncError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| SyncError::Persist(format!("create_dir_all {}: {e}", path.display())))?;
    }
    crate::owner_state_persist::save_atomically(path, bytes)
        .map_err(|e| SyncError::Persist(e.to_string()))
}

/// Serialize + seal + atomic-write. Always writes v2.
pub fn save<T: Serialize>(
    cipher: &DatasetCipher,
    path: &Path,
    label: &'static str,
    inner_schema: u8,
    value: &T,
) -> Result<(), SyncError> {
    debug_assert_ne!(
        inner_schema, SEALED_SCHEMA_V2,
        "inner schema byte is reserved for the sealed envelope"
    );
    let mut inner = Zeroizing::new(vec![inner_schema]);
    into_writer(value, &mut *inner)
        .map_err(|e| SyncError::CborEncode(format!("encode {}: {e}", path.display())))?;
    let sealed = crate::owner_state_crypto::seal_dataset_file(&cipher.keys, label, &inner)
        .map_err(|e| SyncError::Crypto(format!("seal {}: {e}", path.display())))?;
    let mut bytes = Vec::with_capacity(1 + sealed.len());
    bytes.push(SEALED_SCHEMA_V2);
    bytes.extend_from_slice(&sealed);
    atomic_write(path, &bytes)
}

enum Loaded<T> {
    Missing,
    Value { value: T, legacy: bool },
}

fn load_impl<T: DeserializeOwned>(
    cipher: &DatasetCipher,
    path: &Path,
    label: &'static str,
    inner_schema: u8,
) -> Result<Loaded<T>, SyncError> {
    let raw = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Loaded::Missing),
        Err(e) => return Err(SyncError::Persist(format!("read {}: {e}", path.display()))),
    };
    if raw.is_empty() {
        return Err(SyncError::CborDecode(format!(
            "{label} file is empty: {}",
            path.display()
        )));
    }
    // The inner image: owned+zeroizing for the sealed path, borrowed from
    // `raw` for a legacy plaintext file (those bytes were plaintext on disk
    // anyway, so zeroization buys nothing there).
    let (inner_owned, legacy): (Option<Zeroizing<Vec<u8>>>, bool) = match raw[0] {
        SEALED_SCHEMA_V2 => {
            let opened =
                crate::owner_state_crypto::open_dataset_file(&cipher.keys, label, &raw[1..])
                    .map_err(|e| SyncError::Crypto(format!("open {}: {e}", path.display())))?;
            (Some(opened), false)
        }
        v if v == inner_schema => (None, true),
        v => {
            return Err(SyncError::CborDecode(format!(
                "unknown {label} schema version {v:#x} in {}",
                path.display()
            )))
        }
    };
    let inner: &[u8] = inner_owned.as_deref().map(|z| &z[..]).unwrap_or(&raw);
    // The inner image must itself start with the expected legacy schema byte.
    let payload = match inner.split_first() {
        Some((&v, rest)) if v == inner_schema => rest,
        _ => {
            return Err(SyncError::CborDecode(format!(
                "sealed {label} inner image has bad schema byte in {}",
                path.display()
            )))
        }
    };
    let mut cursor = Cursor::new(payload);
    let value: T = from_reader(&mut cursor)
        .map_err(|e| SyncError::CborDecode(format!("load {}: {e}", path.display())))?;
    // Reject trailing bytes after the CBOR value (mirrors
    // owner_state_crypto::canonical_cbor_decode): valid-prefix + garbage
    // must NOT decode as "valid".
    let pos = cursor.position() as usize;
    if pos != payload.len() {
        return Err(SyncError::CborDecode(format!(
            "trailing bytes after {label} value: consumed {} of {}",
            pos,
            payload.len()
        )));
    }
    Ok(Loaded::Value { value, legacy })
}

/// Strict load: missing → default; any failure → Err. Never writes.
pub fn load<T: DeserializeOwned + Default>(
    cipher: &DatasetCipher,
    path: &Path,
    label: &'static str,
    inner_schema: u8,
) -> Result<T, SyncError> {
    match load_impl(cipher, path, label, inner_schema)? {
        Loaded::Missing => Ok(T::default()),
        Loaded::Value { value, .. } => Ok(value),
    }
}

/// Load with the ZEB-460 recovery contract, extended for sealing (ZEB-981):
///
/// - `CborDecode` (corrupt payload / unknown version / empty / trailing
///   bytes) AND `Crypto` (AEAD tag failure: corruption, cross-file swap,
///   foreign-identity file) → quarantine the bytes aside (`.corrupt-<ms>`)
///   and self-heal to default — fleet sync re-populates content from peers.
/// - transient I/O (`Persist`) → propagate untouched; quarantining would
///   orphan real data and let the next persist overwrite it.
/// - a legacy v1 file that parses is EAGERLY re-sealed as v2 (migration on
///   first post-upgrade load, even if never mutated); a failed migration
///   write only warns — the plaintext stays in place and retries later.
pub fn load_or_recover<T: DeserializeOwned + Serialize + Default>(
    cipher: &DatasetCipher,
    path: &Path,
    label: &'static str,
    inner_schema: u8,
) -> Result<T, SyncError> {
    match load_impl::<T>(cipher, path, label, inner_schema) {
        Ok(Loaded::Missing) => Ok(T::default()),
        Ok(Loaded::Value { value, legacy }) => {
            if legacy {
                if let Err(e) = save(cipher, path, label, inner_schema, &value) {
                    tracing::warn!(path = %path.display(), error = %e,
                        "ZEB-981: failed to re-seal legacy plaintext dataset; leaving v1 in place (retries on next persist)");
                }
            }
            Ok(value)
        }
        Err(e @ (SyncError::CborDecode(_) | SyncError::Crypto(_))) => {
            quarantine(path, label, &e);
            Ok(T::default())
        }
        Err(e) => Err(e),
    }
}

/// Timestamped `.corrupt-<ms>` rename: never clobbers a prior quarantine or
/// the live file; preserves the bytes for manual recovery.
fn quarantine(path: &Path, label: &str, err: &SyncError) {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut corrupt = path.as_os_str().to_os_string();
    corrupt.push(format!(".corrupt-{ms}"));
    tracing::error!(path = %path.display(), label, error = %err,
        "dataset load failed; quarantining corrupt file and starting fresh (bytes preserved; fleet sync re-populates)");
    if let Err(re) = std::fs::rename(path, &corrupt) {
        tracing::warn!(path = %path.display(), error = %re, "failed to quarantine corrupt dataset file");
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }
    type Doc = BTreeMap<String, u64>;
    fn sample() -> Doc {
        [("k".to_string(), 7u64)].into_iter().collect()
    }
    const LABEL: &str = "test_ds.cbor";

    #[test]
    fn v2_round_trip_and_missing_is_default() {
        let d = tmp();
        let p = d.path().join(LABEL);
        let c = test_cipher();
        assert_eq!(load::<Doc>(&c, &p, LABEL, 1).unwrap(), Doc::default());
        save(&c, &p, LABEL, 1, &sample()).unwrap();
        let raw = std::fs::read(&p).unwrap();
        assert_eq!(raw[0], SEALED_SCHEMA_V2, "written files are sealed");
        assert_eq!(load::<Doc>(&c, &p, LABEL, 1).unwrap(), sample());
        assert_eq!(load_or_recover::<Doc>(&c, &p, LABEL, 1).unwrap(), sample());
    }

    #[test]
    fn legacy_v1_loads_and_is_eagerly_resealed() {
        let d = tmp();
        let p = d.path().join(LABEL);
        let c = test_cipher();
        let mut v1 = vec![1u8];
        ciborium::into_writer(&sample(), &mut v1).unwrap();
        std::fs::write(&p, &v1).unwrap();
        assert_eq!(load_or_recover::<Doc>(&c, &p, LABEL, 1).unwrap(), sample());
        let raw = std::fs::read(&p).unwrap();
        assert_eq!(raw[0], SEALED_SCHEMA_V2, "migrated on load");
        assert_eq!(load_or_recover::<Doc>(&c, &p, LABEL, 1).unwrap(), sample());
    }

    #[test]
    fn strict_load_reads_v1_without_migrating() {
        let d = tmp();
        let p = d.path().join(LABEL);
        let c = test_cipher();
        let mut v1 = vec![1u8];
        ciborium::into_writer(&sample(), &mut v1).unwrap();
        std::fs::write(&p, &v1).unwrap();
        assert_eq!(load::<Doc>(&c, &p, LABEL, 1).unwrap(), sample());
        assert_eq!(std::fs::read(&p).unwrap(), v1, "strict load never writes");
    }

    #[test]
    fn tag_tamper_quarantines_preserving_bytes() {
        let d = tmp();
        let p = d.path().join(LABEL);
        let c = test_cipher();
        save(&c, &p, LABEL, 1, &sample()).unwrap();
        let mut raw = std::fs::read(&p).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        std::fs::write(&p, &raw).unwrap();
        assert_eq!(load_or_recover::<Doc>(&c, &p, LABEL, 1).unwrap(), Doc::default());
        assert!(!p.exists(), "quarantined");
        let q: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
            .collect();
        assert_eq!(q.len(), 1);
        assert_eq!(
            std::fs::read(q[0].path()).unwrap(),
            raw,
            "bytes preserved verbatim"
        );
    }

    #[test]
    fn label_cross_swap_quarantines() {
        let d = tmp();
        let c = test_cipher();
        let a = d.path().join("a.cbor");
        let b = d.path().join("b.cbor");
        save(&c, &a, "a.cbor", 1, &sample()).unwrap();
        std::fs::rename(&a, &b).unwrap();
        assert_eq!(
            load_or_recover::<Doc>(&c, &b, "b.cbor", 1).unwrap(),
            Doc::default()
        );
        assert!(!b.exists(), "cross-labeled ciphertext quarantined");
    }

    #[test]
    fn foreign_key_quarantines() {
        let d = tmp();
        let p = d.path().join(LABEL);
        let c1 = DatasetCipher::new(Arc::new(KeyTree::derive(&[1u8; 32]).unwrap()));
        let c2 = DatasetCipher::new(Arc::new(KeyTree::derive(&[2u8; 32]).unwrap()));
        save(&c1, &p, LABEL, 1, &sample()).unwrap();
        assert_eq!(
            load_or_recover::<Doc>(&c2, &p, LABEL, 1).unwrap(),
            Doc::default()
        );
        assert!(!p.exists());
    }

    #[test]
    fn transient_io_propagates_without_quarantine() {
        let d = tmp();
        let c = test_cipher();
        let p = d.path().join("as_dir.cbor");
        std::fs::create_dir(&p).unwrap();
        let err = load_or_recover::<Doc>(&c, &p, LABEL, 1).unwrap_err();
        assert!(matches!(err, SyncError::Persist(_)), "got {err:?}");
        assert!(p.is_dir(), "left untouched");
    }

    #[test]
    fn empty_unknown_version_and_trailing_bytes_quarantine() {
        let d = tmp();
        let c = test_cipher();
        let p_empty = d.path().join("empty.cbor");
        std::fs::write(&p_empty, []).unwrap();
        assert_eq!(
            load_or_recover::<Doc>(&c, &p_empty, "empty.cbor", 1).unwrap(),
            Doc::default()
        );
        assert!(!p_empty.exists(), "empty file quarantined");
        let p_unknown = d.path().join("unknown.cbor");
        std::fs::write(&p_unknown, [0xFFu8, 1, 2]).unwrap();
        assert_eq!(
            load_or_recover::<Doc>(&c, &p_unknown, "unknown.cbor", 1).unwrap(),
            Doc::default()
        );
        assert!(!p_unknown.exists(), "unknown-version file quarantined");
        // Trailing bytes INSIDE the sealed image: seal a valid v1 image with
        // appended garbage — the envelope opens, the inner parse must reject.
        let p_trail = d.path().join("trail.cbor");
        let mut inner = vec![1u8];
        ciborium::into_writer(&sample(), &mut inner).unwrap();
        inner.push(0xFF);
        let kt = KeyTree::derive(&[0u8; 32]).unwrap();
        let sealed =
            crate::owner_state_crypto::seal_dataset_file(&kt, "trail.cbor", &inner).unwrap();
        let mut raw = vec![SEALED_SCHEMA_V2];
        raw.extend_from_slice(&sealed);
        std::fs::write(&p_trail, &raw).unwrap();
        assert_eq!(
            load_or_recover::<Doc>(&c, &p_trail, "trail.cbor", 1).unwrap(),
            Doc::default()
        );
        assert!(!p_trail.exists(), "trailing-bytes image quarantined");
    }

    #[cfg(unix)]
    #[test]
    fn migration_write_failure_still_returns_doc() {
        // Read-only parent dir: v1 parses, re-seal fails, doc returned, v1
        // left intact for the next attempt.
        use std::os::unix::fs::PermissionsExt;
        let d = tmp();
        let c = test_cipher();
        let p = d.path().join(LABEL);
        let mut v1 = vec![1u8];
        ciborium::into_writer(&sample(), &mut v1).unwrap();
        std::fs::write(&p, &v1).unwrap();
        let mut perms = std::fs::metadata(d.path()).unwrap().permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(d.path(), perms.clone()).unwrap();
        let got = load_or_recover::<Doc>(&c, &p, LABEL, 1).unwrap();
        perms.set_mode(0o755);
        std::fs::set_permissions(d.path(), perms).unwrap();
        assert_eq!(got, sample());
        assert_eq!(std::fs::read(&p).unwrap(), v1, "plaintext left in place");
    }
}
