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
/// - Decode error → returns `PersistError::CborDecode`. Corrupted
///   files MUST be loud; silently starting empty would mask disk
///   corruption / schema drift.
/// - `community_id` mismatch → returns
///   `PersistError::CommunityIdMismatch`. Guards against misrouted
///   files (wrong directory copied in manually, registry-path bug,
///   etc.) — the bytes parsed, but the file belongs elsewhere.
pub fn load_crdt(path: &Path, expected_id: SpaceId) -> Result<CommunityState, PersistError> {
    if !path.exists() {
        return Ok(CommunityState::new(expected_id));
    }
    let bytes = std::fs::read(path)?;
    let state: CommunityState =
        canonical_cbor_decode(&bytes).map_err(|e| PersistError::CborDecode(e.to_string()))?;
    if state.community_id != expected_id {
        return Err(PersistError::CommunityIdMismatch {
            found: state.community_id,
            expected: expected_id,
        });
    }
    Ok(state)
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
/// - Decode error → returns `PersistError::CborDecode`. Same loud-
///   not-silent rationale as `load_crdt`.
///
/// No `community_id` guard here: the tracker doesn't carry a
/// `community_id` field (it's a flat per-device map), so the routing
/// check lives entirely on the CRDT side. The path itself encodes
/// the community via the registry's directory layout.
pub fn load_replay(path: &Path) -> Result<CommunityRootHlcTracker, PersistError> {
    if !path.exists() {
        return Ok(CommunityRootHlcTracker::default());
    }
    let bytes = std::fs::read(path)?;
    canonical_cbor_decode(&bytes).map_err(|e| PersistError::CborDecode(e.to_string()))
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
