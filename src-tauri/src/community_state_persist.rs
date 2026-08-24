//! Per-community CRDT + replay-tracker disk persistence.
//!
//! Mirrors the shape of `crate::owner_state_persist`: canonical-CBOR
//! inner format, atomic writes, and load-tolerates-missing-file so a
//! first-boot or just-joined community starts with a fresh in-memory
//! state instead of surfacing a fatal error.
//!
//! ZEB-983: every file is sealed at rest under the ZEB-982 device
//! cipher (`device_dataset_file` v3 envelope). The envelope sits
//! BENEATH this module's recovery contracts: the inner image is the
//! exact legacy bare-CBOR form, envelope `Crypto` failures map onto
//! the existing quarantine branch, and envelope `Io` failures map onto
//! the existing hard-error branch. Legacy plaintext files migrate
//! eagerly (resealed after their own parse succeeds, byte-lossless).
//! The AAD label binds the identity-dir-relative path
//! (`communities/{cid_hex}/{filename}`) so a ciphertext copied across
//! communities fails the tag instead of parsing as the wrong
//! community's state.
//!
//! Per-community files live under
//! `identity_dir/communities/{community_id_hex}/{crdt|replay}.cbor`.
//! The directory layout is owned by the `CommunitySyncRegistry` (Task
//! 11); this module derives only the AAD label from the community id
//! and operates on whatever `&Path` the engine hands it.
//!
//! Three load behaviors that are deliberately distinct:
//! - **Missing file**: returns the empty default (`CommunityState::new`
//!   for the CRDT, `CommunityRootHlcTracker::default()` for the
//!   replay tracker). First-boot for a community is the common case;
//!   surfacing it as `Err` would force every caller to special-case
//!   `NotFound`.
//! - **Corrupt content** (CBOR decode failure, AEAD tag failure,
//!   malformed envelope): quarantine + default (self-heal; see
//!   `load_crdt`).
//! - **community_id mismatch**: surfaces as
//!   `PersistError::CommunityIdMismatch`. Guards against misrouted
//!   files (e.g., a directory copied into the wrong slot during
//!   manual recovery, or a typo in the registry's path derivation).
//!   The wire form decoded cleanly — the failure is routing, not
//!   format — so a distinct variant lets operators chase the right
//!   class of bug. (A *sealed* misrouted file fails the AAD first and
//!   lands in quarantine; this variant remains reachable for legacy
//!   plaintext files and same-community path bugs.)

use std::path::Path;

use crate::community_state_crdt::CommunityState;
use crate::community_state_segments::SegmentIndex;
use crate::community_state_sync::CommunityRootHlcTracker;
use crate::device_dataset_file::{read_image, reseal_if_legacy, write_image, DeviceCipher, Image, ImageError};
use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use crate::owner_state_types::SpaceId;

pub(crate) const CRDT_FILENAME: &str = "crdt.cbor";
pub(crate) const REPLAY_FILENAME: &str = "replay.cbor";
pub(crate) const SEGMENT_INDEX_FILENAME: &str = "segments.cbor";

/// AAD label for a per-community file: the stable identity-dir-relative
/// path. Derived from the community id — NEVER from the on-disk `&Path`
/// (test tempdirs and future layout moves must not change the label).
/// Must stay in lockstep with `CommunitySyncRegistry::paths_for`'s
/// `communities/{hex}` layout convention.
pub(crate) fn seal_label(community_id: &SpaceId, filename: &str) -> String {
    format!("communities/{}/{filename}", hex::encode(community_id.0))
}

#[derive(thiserror::Error, Debug)]
pub enum PersistError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("CBOR encode: {0}")]
    CborEncode(String),
    #[error("CBOR decode: {0}")]
    CborDecode(String),
    /// The on-disk file decoded cleanly but its `community_id` doesn't
    /// match what the engine expected. Distinct from `CborDecode`
    /// because the failure class is routing, not format — the bytes
    /// parsed, but the file belongs to a different community.
    #[error("on-disk community_id {found:?} != expected {expected:?}")]
    CommunityIdMismatch { found: SpaceId, expected: SpaceId },
}

/// Save the per-community CRDT to `path`, sealed (ZEB-983). Encodes the
/// state as canonical CBOR (deterministic byte order — required so a
/// future "did anything change?" file-hash check would be meaningful)
/// and writes through `write_image` → `save_atomically` so a crash
/// mid-save can't corrupt the live file.
pub fn save_crdt(cipher: &DeviceCipher, path: &Path, state: &CommunityState) -> Result<(), PersistError> {
    let bytes =
        canonical_cbor_encode(state).map_err(|e| PersistError::CborEncode(e.to_string()))?;
    write_image(cipher, path, &seal_label(&state.community_id, CRDT_FILENAME), &bytes)
        .map_err(PersistError::Io)
}

/// Open the sealed (or legacy plaintext) image at `path`, mapping the
/// envelope's error classes onto this module's contract: `Io` → hard
/// (transient — the bytes may be fine), `Crypto` (AEAD tag failure,
/// malformed envelope, over-cap) → quarantine + `None` (content
/// corruption — same branch as a CBOR decode failure). `Ok(None)` from
/// the envelope means the file is missing (caller returns its default).
fn open_family_image(
    cipher: &DeviceCipher,
    path: &Path,
    label: &str,
) -> Result<Option<Image>, PersistError> {
    match read_image(cipher, path, label) {
        Ok(opt) => Ok(opt),
        Err(ImageError::Io(e)) => Err(PersistError::Io(e)),
        Err(ImageError::Crypto(msg)) => {
            quarantine_corrupted(path, &msg);
            Ok(None)
        }
    }
}

/// Load the per-community CRDT from `path`.
///
/// - Missing file → returns `CommunityState::new(expected_id)`. This is
///   the first-boot or just-joined-community common case; surfacing
///   `NotFound` as an error would force every caller to special-case
///   it.
/// - Decode error → quarantine the corrupted file (rename to
///   `path.cbor.corrupt.<unix_ms>`) and return the empty default with
///   a `tracing::warn!`. Self-heal is correct here: a corrupted
///   per-community CRDT recovers from peers via the next state-root
///   publish, so leaving the engine unable to spawn would maroon the
///   community despite the data being available. An AEAD tag failure
///   (ZEB-983 sealed envelope) lands in this same branch — an
///   undecryptable file is content corruption, not a transient fault.
///   The original bytes are preserved on disk under the `.corrupt.*`
///   suffix for forensic analysis.
/// - `community_id` mismatch → returns
///   `PersistError::CommunityIdMismatch`. Guards against misrouted
///   files (wrong directory copied in manually, registry-path bug,
///   etc.) — the bytes parsed, but the file belongs elsewhere; we
///   intentionally do NOT auto-quarantine here because the file
///   probably belongs to a different community and overwriting it
///   would lose that community's state too.
pub fn load_crdt(
    cipher: &DeviceCipher,
    path: &Path,
    expected_id: SpaceId,
) -> Result<CommunityState, PersistError> {
    let label = seal_label(&expected_id, CRDT_FILENAME);
    let image = match open_family_image(cipher, path, &label)? {
        Some(image) => image,
        None => return Ok(CommunityState::new(expected_id)),
    };
    match canonical_cbor_decode::<CommunityState>(&image.bytes) {
        Ok(state) => {
            if state.community_id != expected_id {
                return Err(PersistError::CommunityIdMismatch {
                    found: state.community_id,
                    expected: expected_id,
                });
            }
            // Eager migration: reseal a legacy plaintext file only after
            // its own parse (and the routing check) succeeded — resealing
            // earlier would launder bytes the family rejected into a
            // valid envelope.
            reseal_if_legacy(cipher, path, &label, &image);
            Ok(state)
        }
        Err(decode_err) => {
            quarantine_corrupted(path, &decode_err.to_string());
            Ok(CommunityState::new(expected_id))
        }
    }
}

/// Save the per-community replay tracker to `path`. Same atomic-write
/// idiom as `save_crdt`; the tracker is small (one HLC per known
/// publisher device) but persisting it is load-bearing — without it,
/// next-boot would re-accept every previously-seen state-root publish
/// once and re-merge the (already-known) events.
pub fn save_replay(
    cipher: &DeviceCipher,
    path: &Path,
    community_id: &SpaceId,
    tracker: &CommunityRootHlcTracker,
) -> Result<(), PersistError> {
    let bytes =
        canonical_cbor_encode(tracker).map_err(|e| PersistError::CborEncode(e.to_string()))?;
    write_image(cipher, path, &seal_label(community_id, REPLAY_FILENAME), &bytes)
        .map_err(PersistError::Io)
}

/// Load the per-community replay tracker from `path`.
///
/// - Missing file → returns `CommunityRootHlcTracker::default()`
///   (empty `per_device` map). On first boot we haven't seen any
///   peer's HLCs yet; replay protection rebuilds organically as
///   publishes arrive.
/// - Decode error → quarantine + return default (same self-heal as
///   `load_crdt`). A corrupted tracker only causes us to re-merge
///   already-known events from the next root publish (cheap), so
///   surfacing a hard error would needlessly block engine spawn.
///
/// No `community_id` guard here: the tracker doesn't carry a
/// `community_id` field (it's a flat per-device map), so the routing
/// check lives entirely on the CRDT side. The path itself encodes
/// the community via the registry's directory layout.
pub fn load_replay(
    cipher: &DeviceCipher,
    path: &Path,
    community_id: &SpaceId,
) -> Result<CommunityRootHlcTracker, PersistError> {
    let label = seal_label(community_id, REPLAY_FILENAME);
    let image = match open_family_image(cipher, path, &label)? {
        Some(image) => image,
        None => return Ok(CommunityRootHlcTracker::default()),
    };
    match canonical_cbor_decode::<CommunityRootHlcTracker>(&image.bytes) {
        Ok(t) => {
            reseal_if_legacy(cipher, path, &label, &image);
            Ok(t)
        }
        Err(decode_err) => {
            quarantine_corrupted(path, &decode_err.to_string());
            Ok(CommunityRootHlcTracker::default())
        }
    }
}

/// Save the per-publisher segment index (ZEB-814 sidecar `segments.cbor`) via
/// atomic write. Gives per-publisher segment-CID stability across republishes:
/// the encoder reuses each sealed segment's `(K_s, cid)` so its own re-publish
/// re-`put`s only the changed tail segment.
pub fn save_segment_index(
    cipher: &DeviceCipher,
    path: &Path,
    community_id: &SpaceId,
    index: &SegmentIndex,
) -> Result<(), PersistError> {
    let bytes =
        canonical_cbor_encode(index).map_err(|e| PersistError::CborEncode(e.to_string()))?;
    write_image(
        cipher,
        path,
        &seal_label(community_id, SEGMENT_INDEX_FILENAME),
        &bytes,
    )
    .map_err(PersistError::Io)
}

/// Load the per-publisher segment index.
///
/// - Missing file → `SegmentIndex::default()` (first publish / just-joined —
///   the encoder will seal from scratch).
/// - Decode error → quarantine + default. Self-heal is correct and cheap: a
///   lost/corrupt sidecar only costs one O(total) re-upload of this publisher's
///   own segments on the next publish (fresh `K_s` → fresh CIDs); receivers
///   still decode every published manifest+segment regardless.
///
/// No `community_id` guard: the sidecar carries no `community_id` (it's a flat
/// per-publisher index), so the routing check lives on the CRDT side. The path
/// itself encodes the community via the registry's directory layout — same as
/// `load_replay`.
pub fn load_segment_index(
    cipher: &DeviceCipher,
    path: &Path,
    community_id: &SpaceId,
) -> Result<SegmentIndex, PersistError> {
    let label = seal_label(community_id, SEGMENT_INDEX_FILENAME);
    let image = match open_family_image(cipher, path, &label)? {
        Some(image) => image,
        None => return Ok(SegmentIndex::default()),
    };
    match canonical_cbor_decode::<SegmentIndex>(&image.bytes) {
        Ok(idx) => {
            reseal_if_legacy(cipher, path, &label, &image);
            Ok(idx)
        }
        Err(decode_err) => {
            quarantine_corrupted(path, &decode_err.to_string());
            Ok(SegmentIndex::default())
        }
    }
}

/// Move a corrupted CBOR file aside under `<path>.corrupt.<unix_ms>` so
/// the next `write_atomic` can land cleanly while preserving the
/// original bytes for forensic analysis. Failures here are logged and
/// swallowed — even if quarantine fails the caller still gets default
/// state, so the engine can spawn and resync from peers.
fn quarantine_corrupted(path: &Path, decode_err: &str) {
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut quarantine = path.as_os_str().to_owned();
    quarantine.push(format!(".corrupt.{suffix}"));
    let quarantine_path = std::path::PathBuf::from(quarantine);
    match std::fs::rename(path, &quarantine_path) {
        Ok(()) => tracing::warn!(
            ?path,
            quarantine = ?quarantine_path,
            error = %decode_err,
            "community persist: corrupted file quarantined; recovering with default state"
        ),
        Err(rename_err) => tracing::error!(
            ?path,
            decode_error = %decode_err,
            rename_error = %rename_err,
            "community persist: failed to quarantine corrupted file; recovering with default state anyway"
        ),
    }
}

// ZEB-983: the module's private `write_atomic` (fixed `.tmp` + rename, no
// fsync) is retired — every write now routes through
// `device_dataset_file::write_image` → `owner_state_persist::save_atomically`
// (randomized temp + file fsync + dir fsync, `create_dir_all` included).
// The old no-fsync rationale ("peer-recoverable") traded durability for a
// per-publish dir-fsync; persists are debounced by the engine, so the
// fsync cost is bounded by debounce cadence, and one write path is worth
// more than the syscall it saves. The fixed-temp-name single-writer
// constraint disappears with it.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_state_segments::{EventBoundary, SealedEntry};
    use crate::device_dataset_file::test_cipher;

    fn cid(byte: u8) -> SpaceId {
        SpaceId([byte; 16])
    }

    fn sample_index() -> SegmentIndex {
        SegmentIndex {
            version: 1,
            sealed: vec![SealedEntry {
                lo: EventBoundary {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "d".into(),
                    id: [1u8; 16],
                },
                hi: EventBoundary {
                    wall_ms: 9,
                    logical: 0,
                    device_id: "d".into(),
                    id: [9u8; 16],
                },
                count: 5,
                k_s: [7u8; 32],
                segment_cid: harmony_content::cid::ContentId::for_book(b"x", Default::default())
                    .unwrap(),
            }],
        }
    }

    #[test]
    fn segment_index_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("segments.cbor");
        let idx = sample_index();
        let cipher = test_cipher();
        save_segment_index(&cipher, &path, &cid(1), &idx).unwrap();
        assert_eq!(load_segment_index(&cipher, &path, &cid(1)).unwrap(), idx);
        // Sealed on disk: the raw file starts with the v3 sentinel, not CBOR.
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(
            raw.first(),
            Some(&crate::device_dataset_file::SEALED_DEVICE_SCHEMA_V3)
        );
    }

    #[test]
    fn segment_index_missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.cbor");
        assert_eq!(
            load_segment_index(&test_cipher(), &path, &cid(1)).unwrap(),
            SegmentIndex::default()
        );
    }

    #[test]
    fn segment_index_corrupt_quarantines_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("segments.cbor");
        std::fs::write(&path, b"\xff not valid cbor \xff\xff").unwrap();
        assert_eq!(
            load_segment_index(&test_cipher(), &path, &cid(1)).unwrap(),
            SegmentIndex::default()
        );
        let quarantined = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".corrupt."));
        assert!(quarantined, "corrupt sidecar should be quarantined aside");
    }

    /// ZEB-983: a legacy bare-CBOR file loads, then is resealed in place —
    /// byte-lossless (the re-opened inner image equals the legacy bytes).
    #[test]
    fn legacy_plaintext_crdt_migrates_to_sealed_losslessly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crdt.cbor");
        let state = CommunityState::new(cid(7));
        let legacy = canonical_cbor_encode(&state).unwrap();
        std::fs::write(&path, &legacy).unwrap();

        let cipher = test_cipher();
        let loaded = load_crdt(&cipher, &path, cid(7)).unwrap();
        assert_eq!(loaded.community_id, cid(7));

        let raw = std::fs::read(&path).unwrap();
        assert_eq!(
            raw.first(),
            Some(&crate::device_dataset_file::SEALED_DEVICE_SCHEMA_V3),
            "legacy file resealed on load"
        );
        let image = read_image(&cipher, &path, &seal_label(&cid(7), CRDT_FILENAME))
            .unwrap()
            .unwrap();
        assert_eq!(&*image.bytes, &legacy[..], "inner image byte-lossless");
        assert!(!image.was_legacy, "second open sees the sealed form");
    }

    /// Sealed-corrupt (AEAD failure) lands in the SAME quarantine branch
    /// as a CBOR decode failure — the envelope maps Crypto onto the
    /// family's corruption contract, dialect `.corrupt.<ms>` preserved.
    #[test]
    fn sealed_corrupt_crdt_quarantines_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crdt.cbor");
        let cipher = test_cipher();
        let state = CommunityState::new(cid(7));
        save_crdt(&cipher, &path, &state).unwrap();
        // Flip a ciphertext byte past the sentinel+nonce → tag failure.
        let mut raw = std::fs::read(&path).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0xFF;
        std::fs::write(&path, &raw).unwrap();

        let loaded = load_crdt(&cipher, &path, cid(7)).unwrap();
        assert_eq!(loaded, CommunityState::new(cid(7)), "defaults after quarantine");
        assert!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().contains(".corrupt.")),
            "AEAD-corrupt file quarantined with the community dialect"
        );
    }

    /// The routing asymmetry is preserved under sealing for LEGACY files:
    /// a plaintext file that parses as a DIFFERENT community hard-errors
    /// (never quarantined — the file probably belongs to that other
    /// community).
    #[test]
    fn legacy_crdt_id_mismatch_stays_hard_unquarantined() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crdt.cbor");
        let other = CommunityState::new(cid(9));
        std::fs::write(&path, canonical_cbor_encode(&other).unwrap()).unwrap();

        let err = load_crdt(&test_cipher(), &path, cid(7)).unwrap_err();
        assert!(matches!(err, PersistError::CommunityIdMismatch { .. }));
        assert!(path.exists(), "mismatched file left in place");
        assert!(
            !std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().contains(".corrupt.")),
            "id-mismatch must NOT quarantine"
        );
    }

    /// A SEALED file copied across communities fails the AAD (the label
    /// binds the community id) and quarantines — it can never parse as
    /// the wrong community's state.
    #[test]
    fn sealed_cross_community_swap_fails_tag_and_quarantines() {
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a").join("crdt.cbor");
        let path_b = dir.path().join("b").join("crdt.cbor");
        let cipher = test_cipher();
        let state_a = CommunityState::new(cid(0xAA));
        save_crdt(&cipher, &path_a, &state_a).unwrap();
        std::fs::create_dir_all(path_b.parent().unwrap()).unwrap();
        std::fs::copy(&path_a, &path_b).unwrap();

        // Loading community B's slot with A's ciphertext: AAD mismatch →
        // Crypto → quarantine + default (never CommunityIdMismatch).
        let loaded = load_crdt(&cipher, &path_b, cid(0xBB)).unwrap();
        assert_eq!(loaded, CommunityState::new(cid(0xBB)));
        assert!(
            std::fs::read_dir(path_b.parent().unwrap())
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().contains(".corrupt.")),
            "cross-community ciphertext quarantined"
        );
    }

    /// Io stays hard: a directory at the path is a deterministic
    /// non-NotFound read error and must propagate, never quarantine.
    #[test]
    fn transient_io_error_stays_hard() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crdt.cbor");
        std::fs::create_dir_all(&path).unwrap();
        let err = load_crdt(&test_cipher(), &path, cid(7)).unwrap_err();
        assert!(matches!(err, PersistError::Io(_)));
        assert!(path.exists(), "nothing relocated on an I/O failure");
    }

    #[test]
    fn replay_roundtrip_sealed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replay.cbor");
        let cipher = test_cipher();
        let tracker = CommunityRootHlcTracker::default();
        save_replay(&cipher, &path, &cid(3), &tracker).unwrap();
        // No PartialEq on the tracker DTO — a default round-trip plus the
        // sealed-sentinel pin is the contract here.
        let _loaded = load_replay(&cipher, &path, &cid(3)).unwrap();
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(
            raw.first(),
            Some(&crate::device_dataset_file::SEALED_DEVICE_SCHEMA_V3)
        );
    }
}
