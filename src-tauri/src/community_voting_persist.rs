//! ZEB-718: per-community voting-log disk persistence.
//!
//! Mirrors `crate::community_state_persist`: plaintext CBOR on disk,
//! atomic-write-via-rename so a crash mid-save can't corrupt the live
//! file, and quarantine-corrupted-then-default on load so a bad file
//! self-heals instead of blocking boot.
//!
//! Only the serde-clean subset of `VotingLog` is persisted:
//! `events: Vec<SignedVotingEvent>` (already the CBOR wire type) plus
//! `policy` (`CommunityVotingPolicy`, set via IPC — not derivable from
//! events). The materialized `polls`/`delegation_graph` are NOT
//! persisted; they are rebuilt by replaying `events` through
//! `VotingLog::apply_with_snapshot` on boot (`reconcile_voting_from_state`).
//! Replay is both safer than trusting serialized fixed-point conviction
//! state and the only serde-clean option — `TierState::Tier3` holds a
//! non-serde `Arc<dyn CommitteeOracle>`.
//!
//! Files live at `identity_dir/communities/{id_hex}/voting.cbor`,
//! alongside `crdt.cbor` / `replay.cbor` (same layout as
//! `community_state_sync::paths_for`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::community_voting_conviction::CommunityVotingPolicy;
use crate::community_voting_core::SignedVotingEvent;
use crate::community_voting_log::VotingLog;
use crate::owner_state_types::SpaceId;

/// Current on-disk schema version. Bump on any breaking layout change;
/// an unknown version decodes-as-corrupt → quarantine + default.
const VOTING_LOG_SCHEMA_VERSION: u8 = 1;

/// The persisted record. `version` + `community_id` ride inside the
/// CBOR (not a raw prefix byte) — same idiom as `CommunityState`, which
/// carries its own `community_id` for the routing check.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedVotingLog {
    version: u8,
    community_id: SpaceId,
    events: Vec<SignedVotingEvent>,
    policy: CommunityVotingPolicy,
}

#[derive(thiserror::Error, Debug)]
pub enum PersistError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("CBOR encode: {0}")]
    CborEncode(String),
}

/// `identity_dir/communities/{id_hex}/voting.cbor` — matches the
/// `crdt.cbor` layout (`community_state_sync::paths_for`). `hex::encode`
/// is the codebase convention for `SpaceId` rendering.
pub fn voting_path_for(identity_dir: &Path, community_id: &SpaceId) -> PathBuf {
    let id_hex = hex::encode(community_id.0);
    identity_dir
        .join("communities")
        .join(id_hex)
        .join("voting.cbor")
}

/// Persist the serde-clean subset of `log` for `community_id` via
/// atomic write. Plaintext CBOR at rest — voting events are signed
/// (tamper-evident); the codebase convention encrypts only raw private
/// keys, not materialized/log state (channel-log segments are likewise
/// plaintext at rest).
pub fn save_voting_log(
    path: &Path,
    log: &VotingLog,
    community_id: &SpaceId,
) -> Result<(), PersistError> {
    let record = PersistedVotingLog {
        version: VOTING_LOG_SCHEMA_VERSION,
        community_id: *community_id,
        events: log.events.clone(),
        policy: log.policy().clone(),
    };
    let mut bytes = Vec::new();
    ciborium::into_writer(&record, &mut bytes)
        .map_err(|e| PersistError::CborEncode(e.to_string()))?;
    write_atomic(path, &bytes)
}

/// Load the persisted `(events, policy)` for `expected_id`.
///
/// - **Missing file** → `(vec![], default policy)`. First-boot or a
///   community that never voted; surfacing `NotFound` would force every
///   caller to special-case it.
/// - **Decode error / unknown version / community_id mismatch** →
///   quarantine the file aside (`<path>.corrupt.<unix_ms>`) and return
///   the empty default. Voting is now peer-recoverable via backfill, so
///   a corrupt local file self-heals; blocking boot would be worse than
///   starting empty. The `community_id` check catches a misrouted file
///   (the path encodes the community, so an internal-id mismatch is
///   corruption of *this* slot — safe to quarantine, unlike
///   `community_state_persist` where a foreign file could legitimately
///   sit in the wrong directory during manual recovery).
pub fn load_voting_log(
    path: &Path,
    expected_id: &SpaceId,
) -> Result<(Vec<SignedVotingEvent>, CommunityVotingPolicy), PersistError> {
    // Single-syscall NotFound handling (no TOCTOU between exists+read).
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), CommunityVotingPolicy::default()));
        }
        Err(e) => return Err(PersistError::Io(e)),
    };
    match ciborium::from_reader::<PersistedVotingLog, _>(bytes.as_slice()) {
        Ok(record) if record.version != VOTING_LOG_SCHEMA_VERSION => {
            quarantine_corrupted(path, &format!("unknown schema version {}", record.version));
            Ok((Vec::new(), CommunityVotingPolicy::default()))
        }
        Ok(record) if record.community_id != *expected_id => {
            quarantine_corrupted(
                path,
                &format!(
                    "community_id {:?} != expected {:?}",
                    record.community_id, expected_id
                ),
            );
            Ok((Vec::new(), CommunityVotingPolicy::default()))
        }
        Ok(record) => Ok((record.events, record.policy)),
        Err(decode_err) => {
            quarantine_corrupted(path, &decode_err.to_string());
            Ok((Vec::new(), CommunityVotingPolicy::default()))
        }
    }
}

/// Move a corrupted / misrouted file aside under `<path>.corrupt.<unix_ms>`
/// so the next `write_atomic` lands cleanly while preserving the
/// original bytes for forensics. Failures here are logged and swallowed
/// — the caller still gets default state, so the engine spawns and
/// resyncs from peers via backfill.
fn quarantine_corrupted(path: &Path, reason: &str) {
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut quarantine = path.as_os_str().to_owned();
    quarantine.push(format!(".corrupt.{suffix}"));
    let quarantine_path = PathBuf::from(quarantine);
    match std::fs::rename(path, &quarantine_path) {
        Ok(()) => tracing::warn!(
            ?path,
            quarantine = ?quarantine_path,
            reason = %reason,
            "voting persist: corrupted file quarantined; recovering with default state"
        ),
        Err(rename_err) => tracing::error!(
            ?path,
            reason = %reason,
            rename_error = %rename_err,
            "voting persist: failed to quarantine corrupted file; recovering with default state anyway"
        ),
    }
}

/// Atomically replace `path` with `bytes` via temp-file + rename.
///
/// `create_dir_all` first so a fresh per-community directory doesn't
/// ENOENT. The temp file shares the parent dir so `rename` is an
/// in-volume atomic op. Like `community_state_persist::write_atomic`
/// (and unlike `owner_state_persist::save_atomically`) we skip the
/// dir-fsync: the voting log is peer-recoverable via backfill, so the
/// per-mutation dir-fsync cost doesn't pencil out. Single writer per
/// (community_id, file) — the engine serializes persist calls — so the
/// fixed `.tmp` name can't race.
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
    use crate::community_voting_core::{PollEventKindCode, Tier};
    use crate::owner_state_types::{Hlc, OwnerAddr};

    fn test_event(wall: u64, logical: u32, device: &str) -> SignedVotingEvent {
        SignedVotingEvent {
            tag: 'v',
            version: 1,
            tier: Tier::Approval,
            kind: PollEventKindCode::PollCreate,
            hlc: Hlc {
                wall_ms: wall,
                logical,
                device_id: device.to_string(),
            },
            actor: OwnerAddr([7u8; 16]),
            payload: vec![1, 2, 3],
            sig: vec![9, 9, 9],
        }
    }

    #[test]
    fn save_then_load_round_trips_events_and_policy() {
        let dir = tempfile::tempdir().unwrap();
        let cid = SpaceId([3u8; 16]);
        let path = voting_path_for(dir.path(), &cid);
        let mut log = VotingLog::default();
        log.events.push(test_event(100, 0, "d1"));
        log.events.push(test_event(200, 1, "d2"));
        // policy stays default (fields are private) — round-trip still
        // exercises the serde path and Eq confirms fidelity.
        save_voting_log(&path, &log, &cid).unwrap();
        let (events, policy) = load_voting_log(&path, &cid).unwrap();
        assert_eq!(events, log.events);
        assert_eq!(&policy, log.policy());
    }

    #[test]
    fn load_missing_file_returns_empty_default() {
        let dir = tempfile::tempdir().unwrap();
        let cid = SpaceId([5u8; 16]);
        let path = voting_path_for(dir.path(), &cid);
        let (events, policy) = load_voting_log(&path, &cid).unwrap();
        assert!(events.is_empty());
        assert_eq!(policy, CommunityVotingPolicy::default());
    }

    #[test]
    fn load_wrong_community_id_quarantines_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cid_a = SpaceId([1u8; 16]);
        let cid_b = SpaceId([2u8; 16]);
        // Write under A's path, but read expecting B → internal-id mismatch.
        let path = voting_path_for(dir.path(), &cid_a);
        let mut log = VotingLog::default();
        log.events.push(test_event(1, 0, "d"));
        save_voting_log(&path, &log, &cid_a).unwrap();
        let (events, _policy) = load_voting_log(&path, &cid_b).unwrap();
        assert!(
            events.is_empty(),
            "mismatch must not surface foreign events"
        );
        let quarantined = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".corrupt."));
        assert!(quarantined, "mismatched file must be quarantined aside");
    }

    #[test]
    fn load_bad_version_quarantines_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cid = SpaceId([4u8; 16]);
        let path = voting_path_for(dir.path(), &cid);
        // A record with an unknown version byte.
        let record = PersistedVotingLog {
            version: 0xEE,
            community_id: cid,
            events: vec![test_event(1, 0, "d")],
            policy: CommunityVotingPolicy::default(),
        };
        let mut bytes = Vec::new();
        ciborium::into_writer(&record, &mut bytes).unwrap();
        write_atomic(&path, &bytes).unwrap();
        let (events, _) = load_voting_log(&path, &cid).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn load_truncated_cbor_quarantines_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cid = SpaceId([6u8; 16]);
        let path = voting_path_for(dir.path(), &cid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, [0xDE, 0xAD, 0xBE, 0xEF]).unwrap();
        let (events, policy) = load_voting_log(&path, &cid).unwrap();
        assert!(events.is_empty());
        assert_eq!(policy, CommunityVotingPolicy::default());
    }
}
