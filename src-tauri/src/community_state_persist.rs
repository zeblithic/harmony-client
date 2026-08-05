//! Per-community CRDT + replay-tracker disk persistence.
//!
//! Mirrors the shape of `crate::owner_state_persist`: canonical-CBOR
//! on-disk format, atomic-write-via-rename so a crash mid-save can't
//! corrupt the live file, and load-tolerates-missing-file so a
//! first-boot or just-joined community starts with a fresh in-memory
//! state instead of surfacing a fatal error.
//!
//! Per-community files live under
//! `identity_dir/communities/{community_id_hex}/{crdt|replay}.cbor`.
//! The directory layout is owned by the `CommunitySyncRegistry` (Task
//! 11); this module is path-agnostic and operates on whatever `&Path`
//! the engine hands it.
//!
//! Three load behaviors that are deliberately distinct:
//! - **Missing file**: returns the empty default (`CommunityState::new`
//!   for the CRDT, `CommunityRootHlcTracker::default()` for the
//!   replay tracker). First-boot for a community is the common case;
//!   surfacing it as `Err` would force every caller to special-case
//!   `NotFound`.
//! - **Decode error**: surfaces as `PersistError::CborDecode`.
//!   Truncated / corrupted files MUST be loud, not silent — eating a
//!   decode error and starting empty would mask a real bug (disk
//!   corruption, schema mismatch, etc.).
//! - **community_id mismatch**: surfaces as
//!   `PersistError::CommunityIdMismatch`. Guards against misrouted
//!   files (e.g., a directory copied into the wrong slot during
//!   manual recovery, or a typo in the registry's path derivation).
//!   The wire form decoded cleanly — the failure is routing, not
//!   format — so a distinct variant lets operators chase the right
//!   class of bug.

use std::path::Path;

use crate::community_state_crdt::CommunityState;
use crate::community_state_segments::SegmentIndex;
use crate::community_state_sync::CommunityRootHlcTracker;
use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use crate::owner_state_types::SpaceId;

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

/// Save the per-community CRDT to `path` via atomic write. Encodes the
/// state as canonical CBOR (deterministic byte order — required so a
/// future "did anything change?" file-hash check would be meaningful)
/// and writes through `write_atomic` so a crash mid-save can't
/// corrupt the live file.
pub fn save_crdt(path: &Path, state: &CommunityState) -> Result<(), PersistError> {
    let bytes =
        canonical_cbor_encode(state).map_err(|e| PersistError::CborEncode(e.to_string()))?;
    write_atomic(path, &bytes)
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
///   publish (`write_atomic` skips fsync precisely because of this
///   peer-recoverability), so leaving the engine unable to spawn
///   would maroon the community despite the data being available.
///   The original bytes are preserved on disk under the `.corrupt.*`
///   suffix for forensic analysis.
/// - `community_id` mismatch → returns
///   `PersistError::CommunityIdMismatch`. Guards against misrouted
///   files (wrong directory copied in manually, registry-path bug,
///   etc.) — the bytes parsed, but the file belongs elsewhere; we
///   intentionally do NOT auto-quarantine here because the file
///   probably belongs to a different community and overwriting it
///   would lose that community's state too.
pub fn load_crdt(path: &Path, expected_id: SpaceId) -> Result<CommunityState, PersistError> {
    // Single-syscall NotFound handling: avoids the TOCTOU window
    // between `path.exists()` and `read` if the file is unlinked
    // between the two calls.
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CommunityState::new(expected_id));
        }
        Err(e) => return Err(PersistError::Io(e)),
    };
    match canonical_cbor_decode::<CommunityState>(&bytes) {
        Ok(state) => {
            if state.community_id != expected_id {
                return Err(PersistError::CommunityIdMismatch {
                    found: state.community_id,
                    expected: expected_id,
                });
            }
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
pub fn save_replay(path: &Path, tracker: &CommunityRootHlcTracker) -> Result<(), PersistError> {
    let bytes =
        canonical_cbor_encode(tracker).map_err(|e| PersistError::CborEncode(e.to_string()))?;
    write_atomic(path, &bytes)
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
pub fn load_replay(path: &Path) -> Result<CommunityRootHlcTracker, PersistError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CommunityRootHlcTracker::default());
        }
        Err(e) => return Err(PersistError::Io(e)),
    };
    match canonical_cbor_decode::<CommunityRootHlcTracker>(&bytes) {
        Ok(t) => Ok(t),
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
pub fn save_segment_index(path: &Path, index: &SegmentIndex) -> Result<(), PersistError> {
    let bytes =
        canonical_cbor_encode(index).map_err(|e| PersistError::CborEncode(e.to_string()))?;
    write_atomic(path, &bytes)
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
pub fn load_segment_index(path: &Path) -> Result<SegmentIndex, PersistError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SegmentIndex::default());
        }
        Err(e) => return Err(PersistError::Io(e)),
    };
    match canonical_cbor_decode::<SegmentIndex>(&bytes) {
        Ok(idx) => Ok(idx),
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

/// Atomically replace `path` with `bytes` via temp-file + rename.
///
/// `create_dir_all` first so a fresh per-community directory (e.g.
/// just after a successful Join, before any prior persist has run)
/// doesn't ENOENT here. The temp-file is `path.with_extension("tmp")`
/// — same parent directory so `rename` is an in-volume atomic op
/// (cross-device rename would fall back to copy + unlink, defeating
/// the atomicity guarantee).
///
/// **Concurrent-write safety:** the fixed `.tmp` name is only safe
/// because each `CommunitySyncEngine`'s internal task is single-
/// threaded — there's exactly one writer per (community_id, file)
/// pair, and persist calls are serialized by the engine's `select!`
/// loop. If a future caller invokes `save_*` from outside the engine
/// (e.g., a parallel migration tool) the fixed temp-name would race;
/// switch to `tempfile::NamedTempFile::new_in(parent)` at that point.
///
/// On Unix `rename` is atomic at the directory-entry level; on
/// Windows NTFS in-volume rename is similarly atomic. We don't fsync
/// the directory entry here (unlike `owner_state_persist`'s heavier
/// `save_atomically`) — the per-community CRDT is fully recoverable
/// from peers via the next state-root publish, so the cost-benefit
/// of an extra dir-fsync per publish doesn't pencil out at this
/// granularity.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), PersistError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_state_segments::{EventBoundary, SealedEntry};

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
        save_segment_index(&path, &idx).unwrap();
        assert_eq!(load_segment_index(&path).unwrap(), idx);
    }

    #[test]
    fn segment_index_missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.cbor");
        assert_eq!(load_segment_index(&path).unwrap(), SegmentIndex::default());
    }

    #[test]
    fn segment_index_corrupt_quarantines_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("segments.cbor");
        std::fs::write(&path, b"\xff not valid cbor \xff\xff").unwrap();
        assert_eq!(load_segment_index(&path).unwrap(), SegmentIndex::default());
        let quarantined = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".corrupt."));
        assert!(quarantined, "corrupt sidecar should be quarantined aside");
    }
}
