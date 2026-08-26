//! On-disk persistence for the Phase-2 OwnerState CRDT and the
//! RootReplayTracker (ZEB-215 Sub-A Phase 3a).
//!
//! See `docs/specs/2026-05-01-zeb-215-sub-a-phase3a-sync-design.md`
//! §"Persistence layer". Two files written via atomic-rename + fsync,
//! each prefixed with a 1-byte schema version.

use std::path::Path;

// ZEB-548 Stage 1: the durable atomic-write primitive moved to
// harmony-foundation (a broadly-shared leaf beneath every tier). Re-exported
// here so the existing crate::owner_state_persist::save_atomically call path —
// used by 9 modules across the app — resolves unchanged. Foundation's variant
// returns std::io::Result; callers convert at the boundary (PersistError:
// From<io::Error> covers the `?` sites).
pub use harmony_foundation::save_atomically;

#[derive(thiserror::Error, Debug)]
pub enum PersistError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("CBOR decode: {0}")]
    CborDecode(String),
    #[error("CBOR encode: {0}")]
    CborEncode(String),
    #[error("file corrupt (truncated or invalid CBOR)")]
    Corrupt,
    #[error("unknown schema version byte: {0:#x}")]
    UnknownSchemaVersion(u8),
}

use crate::owner_state_crdt::OwnerState;
use ciborium::{from_reader, into_writer};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;

/// Phase 3a's CRDT file format. Persisted ContentId fields (in `inbox`
/// keys via `InboxKey.message_cid` AND `outbox` values via
/// `OutboxEntry.message_cid`) used the local `ContentId([u8; 32])`
/// newtype, where the 32 bytes were a raw BLAKE3 hash of message content.
/// Phase 3b reinterprets those bytes as harmony-content's structured
/// CID (header[4] + SHA-256-MSB-truncated hash[28]), so any V1 file
/// persisted by Phase 3a contains ContentId values whose bytes don't
/// have valid harmony-content structure. Loading a V1 file in Phase 3b
/// is therefore a discard-on-load operation: log WARN, return
/// `OwnerState::default()`. CRDT eventual consistency carries the
/// recovery — the next state-root publish from any peer rebuilds local
/// state.
const CRDT_FILE_SCHEMA_V1: u8 = 1;

/// Phase 3b CRDT file format. Identical struct shape to V1 but
/// ContentId fields now carry harmony-content's structured CID. The
/// version byte changed because the SEMANTICS of the bytes-on-disk
/// changed even though the CBOR shape did not.
const CRDT_FILE_SCHEMA_V2: u8 = 2;

#[derive(Serialize, Deserialize)]
struct CrdtFileV2 {
    spaces: BTreeMap<crate::owner_state_types::SpaceId, crate::owner_state_types::Space>,
    outbox:
        BTreeMap<crate::owner_state_types::OutboxEntryId, crate::owner_state_types::OutboxEntry>,
    inbox: BTreeMap<crate::owner_state_types::InboxKey, crate::owner_state_types::InboxEntry>,
    markers: BTreeMap<crate::owner_state_types::SpaceId, crate::owner_state_types::ReadMarker>,
    tombstones: BTreeSet<crate::owner_state_types::SpaceId>,
    /// ZEB-216 Sub-B Phase 1: owner device identity cache. Absent in
    /// pre-Task-8 V2 files; `serde(default)` loads those as an empty
    /// cache. `skip_serializing_if` omits the field when empty so files
    /// written with an empty cache stay compact.
    ///
    /// Field name uses the default Rust identifier (no `rename`) for
    /// consistency with the other five `CrdtFileV2` fields. Short renames
    /// are reserved for `OwnerState`'s wire/AAD encoding (where Reticulum
    /// MTU pressure justifies the abbreviation); `CrdtFileV2` is the
    /// on-disk wrapper using plain ciborium with no MTU constraint.
    #[serde(
        skip_serializing_if = "crate::owner_state_types::OwnerDeviceCache::is_empty",
        default
    )]
    owner_device_cache: crate::owner_state_types::OwnerDeviceCache,
    /// ZEB-218 Sub-D Phase 1: persisted per-OwnerAddr trusted-library
    /// list. Absent in pre-Task-1 V2 files; `serde(default)` loads those
    /// as an empty map. `skip_serializing_if` omits when empty.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    libraries:
        BTreeMap<crate::owner_state_types::OwnerAddr, crate::owner_state_types::LibraryEntry>,
    /// ZEB-243: persisted outbox tombstones. Absent in pre-ZEB-243 V2
    /// files; `serde(default)` loads those as an empty map for backward
    /// compatibility. `skip_serializing_if` omits the field when empty
    /// so existing file shapes stay compact.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    outbox_tombstones:
        BTreeMap<crate::owner_state_types::OutboxEntryId, crate::owner_state_types::Hlc>,
    /// ZEB-370 Phase 1: persisted Friend Graph sub-CRDT. Absent in
    /// pre-ZEB-370 V2 files; `serde(default)` loads those as an empty
    /// graph (no schema-version bump needed — absent == empty).
    /// `skip_serializing_if` omits the field when empty so existing file
    /// shapes stay compact.
    #[serde(
        skip_serializing_if = "crate::friend_graph::FriendGraph::is_empty",
        default
    )]
    friend_graph: crate::friend_graph::FriendGraph,
    /// ZEB-685 (S3): persisted friend-scoped DM device revocations (owner →
    /// revoked #2 ed25519 keys). Absent in pre-ZEB-685 V2 files; `serde(default)`
    /// loads those as empty (no schema-version bump — absent == empty).
    /// `skip_serializing_if` keeps existing file shapes compact.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    revoked_dm_devices: BTreeMap<crate::owner_state_types::OwnerAddr, BTreeSet<[u8; 32]>>,
    /// ZEB-674 Task 1 (C1): persisted per-file sealed DEK store (root CID
    /// bytes → KeyTree-sealed DEK blob). Absent in pre-ZEB-674 V2 files;
    /// `serde(default)` loads those as empty (no schema-version bump — absent
    /// == empty). `skip_serializing_if` keeps existing file shapes compact.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    file_deks: BTreeMap<[u8; 32], Vec<u8>>,
    /// ZEB-674 Task 2 (C2): persisted per-file grant records (root CID bytes →
    /// grant list). Absent in pre-Task-2 V2 files; `serde(default)` loads those
    /// as empty (no schema-version bump — absent == empty). `skip_serializing_if`
    /// keeps existing file shapes compact.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    file_grants: BTreeMap<[u8; 32], Vec<crate::owner_state_types::GrantEntry>>,
    /// ZEB-674 Task 4 (C4): persisted received-file grants (root CID bytes →
    /// received grant record). Absent in pre-Task-4 V2 files; `serde(default)`
    /// loads those as empty (no schema-version bump — absent == empty).
    /// `skip_serializing_if` keeps existing file shapes compact.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    received_file_grants: BTreeMap<[u8; 32], crate::owner_state_types::ReceivedFileGrant>,
    /// ZEB-722: persisted burn tombstones (root CID bytes). Absent in
    /// pre-ZEB-722 V2 files; `serde(default)` loads those as empty (no
    /// schema-version bump — absent == empty). `skip_serializing_if` keeps
    /// existing file shapes compact.
    #[serde(skip_serializing_if = "BTreeSet::is_empty", default)]
    burned_content: BTreeSet<[u8; 32]>,
    /// ZEB-727: persisted received-grant dismiss tombstones (root CID bytes →
    /// dismissed_at ms). Absent in pre-ZEB-727 V2 files; `serde(default)` loads
    /// those as empty (no schema-version bump — absent == empty).
    /// `skip_serializing_if` keeps existing file shapes compact.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    dismissed_received_grants: BTreeMap<[u8; 32], u64>,
    /// ZEB-1000: persisted dedupe-key tombstones (deleted dedupe identity →
    /// deletion HLC). Absent in pre-ZEB-1000 V2 files; `serde(default)` loads
    /// those as empty (no schema-version bump — absent == empty).
    /// `skip_serializing_if` keeps existing file shapes compact.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    dedupe_tombstones: BTreeMap<crate::owner_state_types::DedupeKey, crate::owner_state_types::Hlc>,
}

impl From<&OwnerState> for CrdtFileV2 {
    fn from(s: &OwnerState) -> Self {
        Self {
            spaces: s.spaces.clone(),
            outbox: s.outbox.clone(),
            inbox: s.inbox.clone(),
            markers: s.markers.clone(),
            tombstones: s.tombstones.clone(),
            owner_device_cache: s.owner_device_cache.clone(),
            libraries: s.libraries.clone(),
            outbox_tombstones: s.outbox_tombstones.clone(),
            friend_graph: s.friend_graph.clone(),
            revoked_dm_devices: s.revoked_dm_devices.clone(),
            file_deks: s.file_deks.clone(),
            file_grants: s.file_grants.clone(),
            received_file_grants: s.received_file_grants.clone(),
            burned_content: s.burned_content.clone(),
            dismissed_received_grants: s.dismissed_received_grants.clone(),
            dedupe_tombstones: s.dedupe_tombstones.clone(),
        }
    }
}

impl From<CrdtFileV2> for OwnerState {
    fn from(f: CrdtFileV2) -> Self {
        OwnerState {
            spaces: f.spaces,
            outbox: f.outbox,
            inbox: f.inbox,
            markers: f.markers,
            tombstones: f.tombstones,
            owner_device_cache: f.owner_device_cache,
            libraries: f.libraries,
            outbox_tombstones: f.outbox_tombstones,
            friend_graph: f.friend_graph,
            revoked_dm_devices: f.revoked_dm_devices,
            file_deks: f.file_deks,
            file_grants: f.file_grants,
            received_file_grants: f.received_file_grants,
            burned_content: f.burned_content,
            dismissed_received_grants: f.dismissed_received_grants,
            dedupe_tombstones: f.dedupe_tombstones,
        }
    }
}

/// ZEB-982: filename labels binding the AAD of the sealed envelopes. These
/// are the canonical basenames in the identity dir; sealed copies at other
/// paths (temp files, backups) still open because the AAD binds the LABEL,
/// not the path.
pub const CRDT_FILENAME: &str = "owner_state_crdt.cbor";
pub const REPLAY_FILENAME: &str = "state_root_replay.cbor";

/// Map an envelope failure onto this module's error contract: transient
/// read I/O stays `Io` (never discards state), everything content-shaped
/// is `Corrupt` — which callers already treat as boot-fatal (`lib.rs`
/// loads with `?`), preserving the no-quarantine owner-family contract.
fn image_err(e: crate::device_dataset_file::ImageError) -> PersistError {
    match e {
        crate::device_dataset_file::ImageError::Io(io) => PersistError::Io(io),
        crate::device_dataset_file::ImageError::Crypto(_) => PersistError::Corrupt,
    }
}

pub fn save_crdt(
    cipher: &crate::device_dataset_file::DeviceCipher,
    path: &Path,
    state: &OwnerState,
) -> Result<(), PersistError> {
    let bytes = canonicalize(state)?;
    crate::device_dataset_file::write_image(cipher, path, CRDT_FILENAME, &bytes)
        .map_err(PersistError::Io)
}

/// Encode `OwnerState` to its on-disk canonical byte representation
/// without touching the filesystem. Identical bytes to what `save_crdt`
/// would write (same V2 schema header + ciborium CBOR body), so two
/// snapshots produced from byte-equal states are byte-equal here.
///
/// Used by ZEB-258 atomic-rollback regression tests to assert
/// owner-state is byte-identical pre/post a failed mutation IPC. Pure
/// — no I/O, no allocations beyond the returned `Vec<u8>`.
pub fn canonicalize(state: &OwnerState) -> Result<Vec<u8>, PersistError> {
    let file = CrdtFileV2::from(state);
    let mut bytes = vec![CRDT_FILE_SCHEMA_V2];
    into_writer(&file, &mut bytes).map_err(|e| PersistError::CborEncode(e.to_string()))?;
    Ok(bytes)
}

pub fn load_crdt(
    cipher: &crate::device_dataset_file::DeviceCipher,
    path: &Path,
) -> Result<OwnerState, PersistError> {
    let image = match crate::device_dataset_file::read_image(cipher, path, CRDT_FILENAME)
        .map_err(image_err)?
    {
        Some(img) => img,
        None => return Ok(OwnerState::default()),
    };
    let bytes = &image.bytes;
    if bytes.is_empty() {
        return Err(PersistError::Corrupt);
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    // NO reseal here (PR #728 review): this loader is called from paths
    // that hold no write lock (backup staleness, recovery export, the
    // sidecar preview). A reseal there could race a concurrent writer and
    // atomically replace a NEWER file with this stale snapshot. Migration
    // lives exclusively in [`load_crdt_migrating`], which only boot's
    // serial pre-engine load calls.
    load_crdt_inner(path, version, payload)
}

/// [`load_crdt`] + eager byte-lossless migration. ONLY for call sites that
/// are provably serial with every writer — in practice boot's pre-engine
/// load (`start_node`), which runs before any engine or command path can
/// write. The reseal happens AFTER the inner parse succeeded (a rejected
/// image must never be laundered into a valid envelope); best-effort —
/// failure warns inside and the plaintext stays for the next boot.
pub fn load_crdt_migrating(
    cipher: &crate::device_dataset_file::DeviceCipher,
    path: &Path,
) -> Result<OwnerState, PersistError> {
    let image = match crate::device_dataset_file::read_image(cipher, path, CRDT_FILENAME)
        .map_err(image_err)?
    {
        Some(img) => img,
        None => return Ok(OwnerState::default()),
    };
    let bytes = &image.bytes;
    if bytes.is_empty() {
        return Err(PersistError::Corrupt);
    }
    let state = load_crdt_inner(path, bytes[0], &bytes[1..])?;
    crate::device_dataset_file::reseal_if_legacy(cipher, path, CRDT_FILENAME, &image);
    Ok(state)
}

fn load_crdt_inner(path: &Path, version: u8, payload: &[u8]) -> Result<OwnerState, PersistError> {
    match version {
        CRDT_FILE_SCHEMA_V2 => {
            let mut cursor = Cursor::new(payload);
            let file: CrdtFileV2 =
                from_reader(&mut cursor).map_err(|e| PersistError::CborDecode(e.to_string()))?;
            // Reject trailing bytes — defensive against truncation
            // edge cases that decode "successfully" but stop short.
            if (cursor.position() as usize) != payload.len() {
                return Err(PersistError::Corrupt);
            }
            Ok(file.into())
        }
        CRDT_FILE_SCHEMA_V1 => {
            // Phase 3a → 3b discard-on-load. Phase 3a persisted ContentId
            // values as raw BLAKE3 bytes inside both `inbox` keys and
            // `outbox` values; Phase 3b's harmony-content CID has
            // structured (header + SHA-256-MSB) bytes. Trying to load
            // V1 data into Phase 3b's types would produce ContentIds
            // whose bytes don't have valid harmony-content structure.
            // Discard rather than reinterpret. CRDT eventual consistency
            // recovers via the next state-root from any peer.
            tracing::warn!(
                path = %path.display(),
                "discarding Phase 3a CRDT file (schema v1) — incompatible \
                 ContentId semantics with Phase 3b. Local CRDT state reset; \
                 cross-device sync will rebuild on next state-root publish."
            );
            Ok(OwnerState::default())
        }
        v => Err(PersistError::UnknownSchemaVersion(v)),
    }
}

use crate::owner_state_types::Hlc;

const REPLAY_FILE_SCHEMA_V1: u8 = 1;

#[derive(Serialize, Deserialize)]
struct ReplayFileV1(BTreeMap<String, Hlc>);

pub fn save_replay(
    cipher: &crate::device_dataset_file::DeviceCipher,
    path: &Path,
    tracker: &BTreeMap<String, Hlc>,
) -> Result<(), PersistError> {
    let file = ReplayFileV1(tracker.clone());
    let mut bytes = vec![REPLAY_FILE_SCHEMA_V1];
    into_writer(&file, &mut bytes).map_err(|e| PersistError::CborEncode(e.to_string()))?;
    crate::device_dataset_file::write_image(cipher, path, REPLAY_FILENAME, &bytes)
        .map_err(PersistError::Io)
}

pub fn load_replay(
    cipher: &crate::device_dataset_file::DeviceCipher,
    path: &Path,
) -> Result<BTreeMap<String, Hlc>, PersistError> {
    let image = match crate::device_dataset_file::read_image(cipher, path, REPLAY_FILENAME)
        .map_err(image_err)?
    {
        Some(img) => img,
        None => return Ok(BTreeMap::new()),
    };
    let bytes = &image.bytes;
    if bytes.is_empty() {
        return Err(PersistError::Corrupt);
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    let tracker = match version {
        REPLAY_FILE_SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let file: ReplayFileV1 =
                from_reader(&mut cursor).map_err(|e| PersistError::CborDecode(e.to_string()))?;
            if (cursor.position() as usize) != payload.len() {
                return Err(PersistError::Corrupt);
            }
            file.0
        }
        v => return Err(PersistError::UnknownSchemaVersion(v)),
    };
    // See load_crdt: NO reseal in the lock-free loader.
    let _ = &image;
    Ok(tracker)
}

/// [`load_replay`] + eager migration — boot's serial pre-engine load only
/// (see [`load_crdt_migrating`]).
pub fn load_replay_migrating(
    cipher: &crate::device_dataset_file::DeviceCipher,
    path: &Path,
) -> Result<BTreeMap<String, Hlc>, PersistError> {
    let image = match crate::device_dataset_file::read_image(cipher, path, REPLAY_FILENAME)
        .map_err(image_err)?
    {
        Some(img) => img,
        None => return Ok(BTreeMap::new()),
    };
    let tracker = {
        let bytes = &image.bytes;
        if bytes.is_empty() {
            return Err(PersistError::Corrupt);
        }
        match bytes[0] {
            REPLAY_FILE_SCHEMA_V1 => {
                let payload = &bytes[1..];
                let mut cursor = Cursor::new(payload);
                let file: ReplayFileV1 = from_reader(&mut cursor)
                    .map_err(|e| PersistError::CborDecode(e.to_string()))?;
                if (cursor.position() as usize) != payload.len() {
                    return Err(PersistError::Corrupt);
                }
                file.0
            }
            v => return Err(PersistError::UnknownSchemaVersion(v)),
        }
    };
    crate::device_dataset_file::reseal_if_legacy(cipher, path, REPLAY_FILENAME, &image);
    Ok(tracker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_crdt::{ApplyOutcome, OwnerState};
    use crate::owner_state_types::{
        ContentId, DeliveryStatus, Hlc, OutboxEntry, OutboxEntryId, OwnerAddr, ReadMarker, Space,
        SpaceId, SpaceKind,
    };

    /// ZEB-982: a legacy plaintext CRDT file (first byte = CRDT_FILE_SCHEMA_V2
    /// — the exact value the sealed sentinel must never be confused with)
    /// loads, is eagerly re-sealed byte-losslessly, and reloads identically.
    #[test]
    fn legacy_plaintext_crdt_migrates_to_sealed_losslessly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner_state_crdt.cbor");
        let cipher = crate::device_dataset_file::test_cipher();

        let mut state = OwnerState::default();
        state.tombstones.insert(SpaceId([9; 16]));
        let legacy_image = canonicalize(&state).unwrap();
        assert_eq!(legacy_image[0], CRDT_FILE_SCHEMA_V2, "collision-critical");
        std::fs::write(&path, &legacy_image).unwrap();

        let loaded = load_crdt_migrating(&cipher, &path).unwrap();
        assert!(loaded.tombstones.contains(&SpaceId([9; 16])));

        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(on_disk[0], 3, "file now carries the sealed sentinel");
        let img = crate::device_dataset_file::read_image(&cipher, &path, CRDT_FILENAME)
            .unwrap()
            .unwrap();
        assert!(!img.was_legacy);
        assert_eq!(&img.bytes[..], &legacy_image[..], "inner image verbatim");
        let reloaded = load_crdt(&cipher, &path).unwrap();
        assert!(reloaded.tombstones.contains(&SpaceId([9; 16])));
    }

    /// ZEB-982 contract pin: a sealed-corrupt CRDT file is a hard error —
    /// never quarantined, never defaulted (the owner family has no peer
    /// re-sync to recover discarded state from).
    #[test]
    fn sealed_corrupt_crdt_is_hard_error_no_quarantine() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner_state_crdt.cbor");
        let cipher = crate::device_dataset_file::test_cipher();
        save_crdt(&cipher, &path, &OwnerState::default()).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();

        assert!(matches!(
            load_crdt(&cipher, &path),
            Err(PersistError::Corrupt)
        ));
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(
            entries,
            vec!["owner_state_crdt.cbor".to_string()],
            "no quarantine sidecar, file left in place: {entries:?}"
        );
    }

    fn hlc(w: u64) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: "alice".into(),
        }
    }

    fn sample_state() -> OwnerState {
        let mut s = OwnerState::default();
        let folder = Space {
            id: SpaceId([1; 16]),
            kind: SpaceKind::Folder,
            parent: None,
            community_id: None,
            name: "Root".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(100),
            updated_at: hlc(100),
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            read_receipt_pref: None,
            pending_join_at: None,
        };
        s.spaces.insert(folder.id, folder);
        s.outbox.insert(
            OutboxEntryId([7; 16]),
            OutboxEntry {
                id: OutboxEntryId([7; 16]),
                space_id: SpaceId([1; 16]),
                recipient_owners: vec![OwnerAddr([2; 16])],
                message_cid: Some(ContentId::from_bytes([3; 32])),
                created_at: hlc(100),
                delivered_to: Default::default(),
                delivery_status: DeliveryStatus::Pending,
            },
        );
        s.markers.insert(
            SpaceId([1; 16]),
            ReadMarker {
                space_id: SpaceId([1; 16]),
                last_read_at: hlc(150),
            },
        );
        // ZEB-474: TransportBinding::Reticulum variant removed (flag-day-for-alpha);
        // the Zenoh variant keeps the import alive via the Space fixtures above.
        s
    }

    #[test]
    fn crdt_round_trip_preserves_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner_state_crdt.cbor");
        let original = sample_state();
        save_crdt(&crate::device_dataset_file::test_cipher(), &path, &original).unwrap();
        let loaded = load_crdt(&crate::device_dataset_file::test_cipher(), &path).unwrap();
        assert_eq!(loaded, original);
    }

    #[test]
    fn crdt_load_missing_file_returns_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never_written.cbor");
        let loaded = load_crdt(&crate::device_dataset_file::test_cipher(), &path).unwrap();
        assert_eq!(loaded, OwnerState::default());
    }

    #[test]
    fn crdt_file_v2_round_trips_friend_graph() {
        use crate::friend_graph::{
            owner_id_from_master_ed25519, FriendEntry, FriendOrigin, FriendStatus,
        };
        // Derive a valid (addr, master_ed25519) pair so apply_friend_update's
        // key↔master-key correspondence invariant is satisfied (an arbitrary
        // addr would be rejected and the graph would stay empty).
        let master_ed25519 = ed25519_dalek::SigningKey::from_bytes(&[0xd1; 32])
            .verifying_key()
            .to_bytes();
        let friend_addr = owner_id_from_master_ed25519(&master_ed25519);
        let mut s = OwnerState::default();
        let outcome = s.apply_friend_update(
            friend_addr,
            FriendEntry {
                master_ed25519,
                display: Some("dave".into()),
                status: FriendStatus::Active,
                established_via: FriendOrigin::Token,
                referrable: true,
                learned_at: hlc(42),
                sealed_secret: None,
            },
        );
        assert!(matches!(outcome, ApplyOutcome::Inserted));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner_state_crdt.cbor");
        save_crdt(&crate::device_dataset_file::test_cipher(), &path, &s).unwrap();
        let loaded = load_crdt(&crate::device_dataset_file::test_cipher(), &path).unwrap();
        assert_eq!(loaded.friend_graph, s.friend_graph);
        assert!(!loaded.friend_graph.is_empty());
    }

    #[test]
    fn crdt_file_v2_round_trips_revoked_dm_devices() {
        // ZEB-685 (S3): the friend-scoped DM-revocation store must survive
        // save->load or boot-replay re-seeds nothing and the cutoff regresses
        // on restart. Guards the CrdtFileV2 threading + both From impls.
        let mut s = OwnerState::default();
        let owner = crate::owner_state_types::OwnerAddr([0x77; 16]);
        assert!(s.apply_revoked_dm_device(owner, [0x11; 32]));
        assert!(s.apply_revoked_dm_device(owner, [0x22; 32]));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner_state_crdt.cbor");
        save_crdt(&crate::device_dataset_file::test_cipher(), &path, &s).unwrap();
        let loaded = load_crdt(&crate::device_dataset_file::test_cipher(), &path).unwrap();
        assert_eq!(loaded.revoked_dm_devices, s.revoked_dm_devices);
        assert_eq!(loaded.revoked_dm_devices.get(&owner).unwrap().len(), 2);
    }

    #[test]
    fn crdt_file_v2_round_trips_received_file_grants() {
        // ZEB-674 (C4): the grantee-side received-file-grants store must survive
        // save->load or a restart drops "shared with me" grants (they have no
        // deposit-rung backstop). Guards the CrdtFileV2 threading + both From
        // impls. The `sk` (sealed_dek) value is the KeyTree-sealed DEK.
        let mut s = OwnerState::default();
        let cid = [0x66u8; 32];
        s.received_file_grants.insert(
            cid,
            crate::owner_state_types::ReceivedFileGrant {
                granter_owner: crate::owner_state_types::OwnerAddr([0x88; 16]),
                cid,
                file_name: "shared.md".into(),
                file_size: 4242,
                mime: "text/markdown".into(),
                sealed_dek: vec![0xDE, 0xAD, 0xBE, 0xEF],
                received_at: 1_700_000_000_000,
            },
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner_state_crdt.cbor");
        save_crdt(&crate::device_dataset_file::test_cipher(), &path, &s).unwrap();
        let loaded = load_crdt(&crate::device_dataset_file::test_cipher(), &path).unwrap();
        assert_eq!(loaded.received_file_grants, s.received_file_grants);
        assert_eq!(
            loaded.received_file_grants.get(&cid).unwrap().file_size,
            4242
        );
    }

    #[test]
    fn crdt_file_v2_round_trips_burned_content() {
        // ZEB-722: the burn tombstone set must survive save->load, or a restart
        // re-loads the GC'd DEK/grant entries as live and boot-replay never
        // re-burns them — the map resurrects across restarts. Guards the
        // CrdtFileV2 threading + both From impls. A default OwnerState (empty
        // set, skipped on the wire) implicitly covers the pre-ZEB-722
        // backward-compat "loads empty" case.
        let mut s = OwnerState::default();
        s.burn_gc([0x5cu8; 32]);
        s.burn_gc([0x5du8; 32]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner_state_crdt.cbor");
        save_crdt(&crate::device_dataset_file::test_cipher(), &path, &s).unwrap();
        let loaded = load_crdt(&crate::device_dataset_file::test_cipher(), &path).unwrap();
        assert_eq!(loaded.burned_content, s.burned_content);
        assert_eq!(loaded.burned_content.len(), 2);
    }

    #[test]
    fn crdt_file_v2_round_trips_dismissed_received_grants() {
        // ZEB-727: the dismiss tombstone map must survive save->load, or a restart
        // re-loads a dismissed "shared with me" grant as live (it has no
        // deposit-rung backstop) and it resurrects across restarts. Guards the
        // CrdtFileV2 threading + both From impls. A default OwnerState (empty map,
        // skipped on the wire) implicitly covers the pre-ZEB-727 backward-compat
        // "loads empty" case.
        let mut s = OwnerState::default();
        s.dismissed_received_grants
            .insert([0x5eu8; 32], 1_700_000_000_000);
        s.dismissed_received_grants
            .insert([0x5fu8; 32], 1_700_000_000_042);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner_state_crdt.cbor");
        save_crdt(&crate::device_dataset_file::test_cipher(), &path, &s).unwrap();
        let loaded = load_crdt(&crate::device_dataset_file::test_cipher(), &path).unwrap();
        assert_eq!(
            loaded.dismissed_received_grants,
            s.dismissed_received_grants
        );
        assert_eq!(loaded.dismissed_received_grants.len(), 2);
    }

    #[test]
    fn crdt_file_v2_round_trips_dedupe_tombstones() {
        // ZEB-1000: the dedupe-key tombstone map must survive save->load, or a
        // restart forgets every permanent deletion's dedupe identity and a
        // stale sibling can resurrect a deleted DM via a fresh SpaceId.
        // Guards the CrdtFileV2 threading + both From impls. A default
        // OwnerState (empty map, skipped on the wire) implicitly covers the
        // pre-ZEB-1000 backward-compat "loads empty" case below.
        let mut s = OwnerState::default();
        s.dedupe_tombstones.insert(
            crate::owner_state_types::DedupeKey::SortedMembers(vec![
                crate::owner_state_types::OwnerAddr([1; 16]),
                crate::owner_state_types::OwnerAddr([2; 16]),
            ]),
            crate::owner_state_types::Hlc {
                wall_ms: 2_000,
                logical: 1,
                device_id: "dev-a".into(),
            },
        );
        s.dedupe_tombstones.insert(
            crate::owner_state_types::DedupeKey::Topic("zenoh/topic".into()),
            crate::owner_state_types::Hlc {
                wall_ms: 3_000,
                logical: 0,
                device_id: "dev-b".into(),
            },
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner_state_crdt.cbor");
        save_crdt(&crate::device_dataset_file::test_cipher(), &path, &s).unwrap();
        let loaded = load_crdt(&crate::device_dataset_file::test_cipher(), &path).unwrap();
        assert_eq!(loaded.dedupe_tombstones, s.dedupe_tombstones);
        assert_eq!(loaded.dedupe_tombstones.len(), 2);
    }

    #[test]
    fn pre_dedupe_tombstones_snapshot_loads_empty() {
        // A V2 file serialized WITHOUT the dedupe-tombstone map (skipped on
        // the wire when empty) must load to an empty map — backward-compat
        // with snapshots written before ZEB-1000.
        let s = OwnerState::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner_state_crdt.cbor");
        save_crdt(&crate::device_dataset_file::test_cipher(), &path, &s).unwrap();
        let loaded = load_crdt(&crate::device_dataset_file::test_cipher(), &path).unwrap();
        assert!(loaded.dedupe_tombstones.is_empty());
    }

    #[test]
    fn pre_dismissed_received_grants_snapshot_loads_empty() {
        // A V2 file serialized WITHOUT the dismiss-tombstone map (skipped on the
        // wire when empty) must load to an empty map — backward-compat with
        // snapshots written before ZEB-727.
        let s = OwnerState::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner_state_crdt.cbor");
        save_crdt(&crate::device_dataset_file::test_cipher(), &path, &s).unwrap();
        let loaded = load_crdt(&crate::device_dataset_file::test_cipher(), &path).unwrap();
        assert!(loaded.dismissed_received_grants.is_empty());
    }

    #[test]
    fn pre_received_file_grants_snapshot_loads_empty() {
        // A V2 file serialized WITHOUT the received-file-grants store (skipped on
        // the wire when empty) must load to an empty map — backward-compat with
        // snapshots written before ZEB-674 Task 4.
        let s = OwnerState::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner_state_crdt.cbor");
        save_crdt(&crate::device_dataset_file::test_cipher(), &path, &s).unwrap();
        let loaded = load_crdt(&crate::device_dataset_file::test_cipher(), &path).unwrap();
        assert!(loaded.received_file_grants.is_empty());
    }

    #[test]
    fn pre_revoked_dm_devices_snapshot_loads_empty() {
        // A V2 file serialized WITHOUT the revoked-DM store (skipped on the wire
        // when empty) must load to an empty map — backward-compat with snapshots
        // written before ZEB-685.
        let s = OwnerState::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner_state_crdt.cbor");
        save_crdt(&crate::device_dataset_file::test_cipher(), &path, &s).unwrap();
        let loaded = load_crdt(&crate::device_dataset_file::test_cipher(), &path).unwrap();
        assert!(loaded.revoked_dm_devices.is_empty());
    }

    #[test]
    fn pre_friendgraph_snapshot_loads_empty() {
        // A V2 file serialized WITHOUT any friend graph (the field is
        // skipped on the wire when empty) must load to an empty graph —
        // backward-compat with pre-ZEB-370 snapshots.
        let s = OwnerState::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner_state_crdt.cbor");
        save_crdt(&crate::device_dataset_file::test_cipher(), &path, &s).unwrap();
        let loaded = load_crdt(&crate::device_dataset_file::test_cipher(), &path).unwrap();
        assert!(loaded.friend_graph.is_empty());
    }

    #[test]
    fn replay_round_trip_preserves_tracker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state_root_replay.cbor");
        let mut original: BTreeMap<String, Hlc> = BTreeMap::new();
        original.insert("alice-laptop".into(), hlc(100));
        original.insert("bob-phone".into(), hlc(200));
        save_replay(&crate::device_dataset_file::test_cipher(), &path, &original).unwrap();
        let loaded = load_replay(&crate::device_dataset_file::test_cipher(), &path).unwrap();
        assert_eq!(loaded, original);
    }

    #[test]
    fn replay_load_missing_file_returns_empty_map() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never_written.cbor");
        let loaded = load_replay(&crate::device_dataset_file::test_cipher(), &path).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn crdt_load_unknown_schema_version_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.cbor");
        // 0xFF is reserved-future; v1 is 0x01.
        std::fs::write(&path, [0xFF_u8, 0x00, 0x01]).unwrap();
        let err =
            load_crdt(&crate::device_dataset_file::test_cipher(), &path).expect_err("should error");
        assert!(matches!(err, PersistError::UnknownSchemaVersion(0xFF)));
    }

    #[test]
    fn crdt_load_truncated_cbor_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated.cbor");
        // Schema v2 + arbitrary CBOR-like junk that won't decode.
        // (V1 is now the discard path, so we use V2 to exercise the
        // CBOR-decode error path.)
        std::fs::write(&path, [CRDT_FILE_SCHEMA_V2, 0xA1, 0x66]).unwrap();
        let err =
            load_crdt(&crate::device_dataset_file::test_cipher(), &path).expect_err("should error");
        assert!(matches!(
            err,
            PersistError::CborDecode(_) | PersistError::Corrupt
        ));
    }

    #[test]
    fn crdt_load_v1_discards_returns_empty_state() {
        // Phase 3a → 3b migration: V1 files are discarded on load.
        // The file body is irrelevant — even valid Phase-3a CBOR
        // never gets decoded.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phase3a.cbor");
        std::fs::write(&path, [CRDT_FILE_SCHEMA_V1, 0x42, 0x43, 0x44]).unwrap();
        let loaded = load_crdt(&crate::device_dataset_file::test_cipher(), &path)
            .expect("V1 load returns Ok with default state, not error");
        assert!(
            loaded.spaces.is_empty(),
            "V1 load should produce empty spaces"
        );
        assert!(
            loaded.outbox.is_empty(),
            "V1 load should produce empty outbox"
        );
        assert!(
            loaded.inbox.is_empty(),
            "V1 load should produce empty inbox"
        );
        assert!(
            loaded.markers.is_empty(),
            "V1 load should produce empty markers"
        );
        assert!(
            loaded.tombstones.is_empty(),
            "V1 load should produce empty tombstones"
        );
    }

    #[test]
    fn crdt_load_empty_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.cbor");
        std::fs::write(&path, []).unwrap();
        let err =
            load_crdt(&crate::device_dataset_file::test_cipher(), &path).expect_err("should error");
        assert!(matches!(err, PersistError::Corrupt));
    }

    #[test]
    fn replay_load_unknown_schema_version_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future_replay.cbor");
        std::fs::write(&path, [0xFE_u8]).unwrap();
        let err = load_replay(&crate::device_dataset_file::test_cipher(), &path)
            .expect_err("should error");
        assert!(matches!(err, PersistError::UnknownSchemaVersion(0xFE)));
    }

    #[test]
    fn crdt_load_legacy_trailing_bytes_after_valid_cbor_errors() {
        // PR #728 review: the trailing-bytes rejection must hold on the
        // LEGACY (plaintext) path too, not only inside a sealed image.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy_trailing.cbor");
        let mut bytes = canonicalize(&OwnerState::default()).unwrap();
        bytes.push(0xFF);
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            load_crdt(&crate::device_dataset_file::test_cipher(), &path),
            Err(PersistError::Corrupt)
        ));
    }

    #[test]
    fn crdt_load_trailing_bytes_after_valid_cbor_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("with_tail.cbor");
        // Save a valid file, then append a junk byte.
        save_crdt(
            &crate::device_dataset_file::test_cipher(),
            &path,
            &OwnerState::default(),
        )
        .unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(0xFF);
        std::fs::write(&path, bytes).unwrap();
        let err =
            load_crdt(&crate::device_dataset_file::test_cipher(), &path).expect_err("should error");
        assert!(matches!(err, PersistError::Corrupt));
    }

    /// Verifies that the new Phase 1 fields (Space.content_key,
    /// Space.prior_content_keys, OwnerState.owner_device_cache) survive a
    /// full persistence round-trip through save_crdt / load_crdt.
    ///
    /// This test was written first (TDD) and was expected to fail on the
    /// owner_device_cache assertions before the CrdtFileV2 fix in Task 8.
    #[test]
    fn persist_round_trip_with_dm_state() {
        use crate::owner_state_types::{DeviceIdentityHash, DmContentKey};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dm_state.cbor");

        let mut state = OwnerState::default();

        // Insert a DM Space with content_key + prior_content_keys.
        let dm_space = Space {
            id: SpaceId([1; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "alice-bob".into(),
            transport: None,
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(1),
            updated_at: hlc(1),
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: vec![DmContentKey::new([0xbb; 32])],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            read_receipt_pref: None,
            pending_join_at: None,
        };
        state.apply_space_with_canonicalization(dm_space);

        // Insert OwnerDeviceCache entries. Seed a non-empty
        // `device_identity_pubs` parallel vec — Some + None mix exercises
        // both branches of the bstr-or-null encoder. Without this seed
        // the test goes green even if persist drops the parallel vec
        // entirely (regression-of-omission).
        //
        // Real (hash, pub) pairs derived from PrivateIdentity so the
        // pub-derives-to-hash invariant in apply_owner_device_update
        // accepts the seed.
        let private_a = harmony_identity::PrivateIdentity::from_seed(&[0xa1; 32]);
        let public_a = private_a.public_identity();
        let pub_a = public_a.to_public_bytes();
        let hash_a = DeviceIdentityHash(public_a.address_hash);
        let private_b = harmony_identity::PrivateIdentity::from_seed(&[0xb2; 32]);
        let public_b = private_b.public_identity();
        let hash_b = DeviceIdentityHash(public_b.address_hash);
        // OD3 (ZEB-473): also seed a NON-empty tunnel contact (correctly-sized
        // PQ keys so the apply-time key-size gate accepts it), parallel to the
        // pubs vec, so the round-trip catches a regression that drops
        // `device_tunnel_contacts` on save/load. Pre-sort all three vecs so the
        // post-apply order is deterministic for the assertions below (apply sorts
        // ascending by hash).
        let contact_a = crate::owner_state_types::DeviceTunnelContact {
            iroh_node_id: [0xa1; 32],
            home_relay_url: Some("https://relay.example/a".into()),
            pq_dsa_pubkey: vec![0x11; crate::owner_state_types::ML_DSA_65_PUBKEY_LEN],
            pq_kem_pubkey: vec![0x22; crate::owner_state_types::ML_KEM_768_PUBKEY_LEN],
        };
        let (sorted_hashes, sorted_pubs, sorted_contacts) = if hash_a < hash_b {
            (
                vec![hash_a, hash_b],
                vec![Some(pub_a), None],
                vec![Some(contact_a.clone()), None],
            )
        } else {
            (
                vec![hash_b, hash_a],
                vec![None, Some(pub_a)],
                vec![None, Some(contact_a.clone())],
            )
        };
        state.apply_owner_device_update(
            OwnerAddr([2; 16]),
            sorted_hashes.clone(),
            sorted_pubs.clone(),
            sorted_contacts.clone(),
            hlc(1),
        );

        // Round-trip through the production file-based save/load path.
        save_crdt(&crate::device_dataset_file::test_cipher(), &path, &state).unwrap();
        let loaded = load_crdt(&crate::device_dataset_file::test_cipher(), &path).unwrap();

        // -- Space fields --
        let loaded_space = loaded
            .spaces
            .get(&SpaceId([1; 16]))
            .expect("DM Space should round-trip");
        assert_eq!(
            loaded_space.content_key.as_ref().map(|k| *k.as_bytes()),
            Some([0xaa; 32]),
            "Space.content_key must persist",
        );
        assert_eq!(
            loaded_space.prior_content_keys.len(),
            1,
            "Space.prior_content_keys must persist (len)",
        );
        assert_eq!(
            loaded_space.prior_content_keys[0].as_bytes(),
            &[0xbb; 32],
            "Space.prior_content_keys[0] must persist",
        );

        // -- OwnerDeviceCache --
        let cache_entry = loaded
            .owner_device_cache
            .devices
            .get(&OwnerAddr([2; 16]))
            .expect("OwnerDeviceCache entry should round-trip");
        assert_eq!(
            cache_entry.devices.len(),
            2,
            "OwnerDeviceCache entry should have 2 devices",
        );
        // apply_owner_device_update sorts ascending by hash; we pre-sorted
        // above so sorted_hashes[0] < sorted_hashes[1].
        assert_eq!(
            cache_entry.devices, sorted_hashes,
            "device hashes round-trip in sorted order",
        );
        // Pin parallel-vec round-trip: persist must preserve the Some/None
        // shape exactly. Without this assertion the test would go green
        // even if persist dropped device_identity_pubs entirely.
        assert_eq!(
            cache_entry.device_identity_pubs, sorted_pubs,
            "device_identity_pubs parallel vec must persist with Some + None preserved",
        );
        // OD3: the tunnel-contact parallel vec must ALSO survive save/load with
        // its Some + None shape preserved (regression-of-omission guard).
        assert_eq!(
            cache_entry.device_tunnel_contacts, sorted_contacts,
            "device_tunnel_contacts parallel vec must persist with Some + None preserved",
        );
    }

    /// Verifies backward compatibility: a V2 file written WITHOUT the
    /// `owner_device_cache` field (pre-Task-8 format) loads cleanly with
    /// an empty cache.
    #[test]
    fn crdt_load_v2_without_owner_device_cache_field_yields_empty_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old_v2.cbor");

        // Write a file using an OwnerState with an empty cache — the
        // skip_serializing_if will omit the field entirely, mimicking
        // a pre-Task-8 file on disk.
        let state_no_cache = OwnerState::default();
        save_crdt(
            &crate::device_dataset_file::test_cipher(),
            &path,
            &state_no_cache,
        )
        .unwrap();

        // Confirm the field key was omitted by scanning the decrypted INNER
        // image for the literal UTF-8 of the CBOR text key (PR #728 review:
        // scanning the raw file is vacuous now that it is ciphertext).
        let image = crate::device_dataset_file::read_image(
            &crate::device_dataset_file::test_cipher(),
            &path,
            CRDT_FILENAME,
        )
        .unwrap()
        .unwrap();
        let key = b"owner_device_cache";
        assert!(
            !image.bytes.windows(key.len()).any(|w| w == key),
            "`owner_device_cache` key should not appear in the image when cache is empty",
        );

        // Loading must succeed with an empty cache, not error.
        let loaded = load_crdt(&crate::device_dataset_file::test_cipher(), &path).unwrap();
        assert!(
            loaded.owner_device_cache.is_empty(),
            "pre-Task-8 V2 files must load with empty owner_device_cache",
        );
    }

    /// ZEB-218 Sub-D R3 F6: backward-compat for the new `libraries`
    /// field. A pre-Task-1 V2 file (written before `libraries` existed)
    /// must load cleanly with an empty BTreeMap. `serde(default)` on
    /// the field is what makes this work — this test pins that
    /// behavior so a future refactor can't quietly remove the default
    /// and break load on every existing user's disk.
    #[test]
    fn crdt_load_v2_without_libraries_field_yields_empty_libraries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old_v2_no_libs.cbor");

        // OwnerState::default() has libraries == empty; the
        // skip_serializing_if = "BTreeMap::is_empty" omits the field
        // entirely, mimicking a pre-Task-1 file on disk.
        let state_no_libs = OwnerState::default();
        save_crdt(
            &crate::device_dataset_file::test_cipher(),
            &path,
            &state_no_libs,
        )
        .unwrap();

        // Confirm the field key was omitted by scanning raw bytes
        // for the literal UTF-8 of the CBOR text key (CBOR text
        // strings include the field name verbatim in the byte stream).
        let image = crate::device_dataset_file::read_image(
            &crate::device_dataset_file::test_cipher(),
            &path,
            CRDT_FILENAME,
        )
        .unwrap()
        .unwrap();
        let key = b"libraries";
        assert!(
            !image.bytes.windows(key.len()).any(|w| w == key),
            "`libraries` key should not appear in the image when empty",
        );

        // Loading must succeed with an empty BTreeMap, not error on
        // missing field.
        let loaded = load_crdt(&crate::device_dataset_file::test_cipher(), &path).unwrap();
        assert!(
            loaded.libraries.is_empty(),
            "pre-Task-1 V2 files must load with empty libraries",
        );
    }
}
