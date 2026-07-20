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

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::community_voting_conviction::CommunityVotingPolicy;
use crate::community_voting_core::{PollId, PollMeta, SignedVotingEvent};
use crate::community_voting_log::{TierState, VotingLog};
use crate::owner_state_types::SpaceId;

/// Current on-disk schema version. Bump on any breaking layout change;
/// an unknown version decodes-as-corrupt → quarantine + default.
const VOTING_LOG_SCHEMA_VERSION: u8 = 1;

/// Per-poll state that replaying `events` does NOT reconstruct, because it is
/// set by tick-driven or engine-side in-place mutation rather than by any
/// event:
/// - `meta`: the whole `PollMeta` (Tier-1 auto-close → `Closed`, Tier-2
///   finalize → `Finalized` + `finalized_at_ms`, archive → `Archived`).
///   Without it a restart resurrects Finalized/Archived polls as active
///   `Open` ones (re-contestable / zombie).
/// - `tier2_timing`: the Tier-2 contestability clock in `tier_state`
///   (`threshold_reached_at_ms`, `last_unsignal_after_threshold_ms`). Without
///   it a mid-contestability proposal's 24h window resets on restart,
///   re-opening a reversion window. `None` for non-Tier-2 polls.
/// - `tier3_community_epoch`: the Tier-3 D-FROST epoch. `apply` sets it to `0`
///   at PollCreate; the engine then patches the real epoch via
///   `set_tier3_poll_epoch` (read from `DfrostLogRegistry`) — a non-event
///   mutation. Replay alone leaves it `0`, so a reloaded in-flight Tier-3 poll
///   would derive its beacon seed with epoch 0 (`derive_beacon_seed` /
///   `verify_ss` mismatch) and stall. `None` for non-Tier-3 polls. (`last_hlc`
///   and `poll_create_event_hash`, by contrast, ARE set inside `apply`, so
///   replay reconstructs them — they need no overlay.)
#[derive(Debug, Serialize, Deserialize)]
pub struct PollRestore {
    pub meta: PollMeta,
    pub tier2_timing: Option<(Option<i128>, Option<i128>)>,
    /// `#[serde(default)]` so a `voting.cbor` written before this field existed
    /// (interim dev builds of this branch) still decodes — a missing value
    /// means "no Tier-3 epoch overlay", identical to prior behaviour.
    #[serde(default)]
    pub tier3_community_epoch: Option<u64>,
}

/// The persisted record. `version` + `community_id` ride inside the
/// CBOR (not a raw prefix byte) — same idiom as `CommunityState`, which
/// carries its own `community_id` for the routing check. `poll_restore` is
/// the tick-driven-state overlay applied after replay (see `PollRestore`).
#[derive(Debug, Serialize, Deserialize)]
struct PersistedVotingLog {
    version: u8,
    community_id: SpaceId,
    events: Vec<SignedVotingEvent>,
    policy: CommunityVotingPolicy,
    poll_restore: HashMap<PollId, PollRestore>,
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

/// An owned, `Send + 'static` snapshot of the serde-clean subset of a
/// `VotingLog`, built while holding the log lock and then handed to
/// `write_snapshot` on a blocking thread. This split (snapshot under the
/// async lock → CBOR-encode + `std::fs` write off the Tokio worker via
/// `spawn_blocking`) is the codebase's persistence pattern (PRs #74/#380/
/// #381): it keeps blocking disk I/O from parking async worker threads and
/// avoids holding `voting_log`/`voting_logs` locks across the write.
pub struct VotingLogSnapshot(PersistedVotingLog);

/// Build a `VotingLogSnapshot` from `log`. Cheap-ish (clones `events`,
/// `policy`, and the per-poll `poll_restore` overlay) but does NO I/O, so
/// the caller holds the log lock only for the clone, not the disk write.
pub fn snapshot_for_persist(log: &VotingLog, community_id: &SpaceId) -> VotingLogSnapshot {
    let poll_restore: HashMap<PollId, PollRestore> = log
        .polls
        .iter()
        .map(|(pid, state)| {
            let tier2_timing = match &state.tier_state {
                TierState::Tier2(t2) => Some((
                    t2.threshold_reached_at_ms,
                    t2.last_unsignal_after_threshold_ms,
                )),
                _ => None,
            };
            let tier3_community_epoch = state
                .tier_state
                .as_tier3()
                .map(|t3| t3.meta.community_epoch);
            (
                *pid,
                PollRestore {
                    meta: state.meta.clone(),
                    tier2_timing,
                    tier3_community_epoch,
                },
            )
        })
        .collect();
    VotingLogSnapshot(PersistedVotingLog {
        version: VOTING_LOG_SCHEMA_VERSION,
        community_id: *community_id,
        events: log.events.clone(),
        policy: log.policy().clone(),
        poll_restore,
    })
}

/// CBOR-encode `snapshot` and atomically replace `path`. Blocking (`std::fs`
/// write + rename) — run under `spawn_blocking`, never on an async worker
/// while holding a lock. Plaintext CBOR at rest — voting events are signed
/// (tamper-evident); the codebase convention encrypts only raw private
/// keys, not materialized/log state (channel-log segments are likewise
/// plaintext at rest).
pub fn write_snapshot(path: &Path, snapshot: &VotingLogSnapshot) -> Result<(), PersistError> {
    let mut bytes = Vec::new();
    ciborium::into_writer(&snapshot.0, &mut bytes)
        .map_err(|e| PersistError::CborEncode(e.to_string()))?;
    write_atomic(path, &bytes)
}

/// Convenience sync wrapper: `snapshot_for_persist` + `write_snapshot` in one
/// call. Retained for the non-latency-critical / test call sites that already
/// hold the log by reference and don't need the blocking I/O moved off-worker.
/// Hot paths (`persist_now`, the tick archive sweep) use the split form so the
/// `std::fs` write runs on a blocking thread with no lock held.
pub fn save_voting_log(
    path: &Path,
    log: &VotingLog,
    community_id: &SpaceId,
) -> Result<(), PersistError> {
    write_snapshot(path, &snapshot_for_persist(log, community_id))
}

/// Persist ONLY a policy change, preserving the on-disk `events` +
/// `poll_restore` and NEVER clobbering a recoverable file. The IPC policy path
/// (`voting_set_notify_on_delegate_signal`) calls this instead of
/// `save_voting_log` because it can run against an un-reconciled in-memory log
/// (empty `events`) or after boot-reconcile disarmed the engine's persistence
/// on an unreadable file — writing the in-memory log directly would then
/// overwrite recoverable voting history with an empty event set. Behaviour by
/// on-disk state:
/// - **I/O error on an existing file** → `Err` (skip; present-but-unreadable —
///   the same disarm the engine applies, enforced independently here).
/// - **Missing / corrupt (quarantined)** → write `{ [], new_policy, {} }`
///   (policy-only durability; a corrupt file self-heals).
/// - **Readable** → rewrite the loaded `events` + `poll_restore` with
///   `new_policy` (preserves events even when the caller's in-memory log had
///   not loaded them).
pub fn save_policy_only(
    path: &Path,
    community_id: &SpaceId,
    new_policy: &CommunityVotingPolicy,
) -> Result<(), PersistError> {
    // `load_voting_log` returns `Err` ONLY on a non-NotFound I/O error
    // (present-but-unreadable); missing → empty, corrupt → quarantine + empty.
    let (events, _old_policy, poll_restore) = load_voting_log(path, community_id)?;
    let record = PersistedVotingLog {
        version: VOTING_LOG_SCHEMA_VERSION,
        community_id: *community_id,
        events,
        policy: new_policy.clone(),
        poll_restore,
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
#[allow(clippy::type_complexity)]
pub fn load_voting_log(
    path: &Path,
    expected_id: &SpaceId,
) -> Result<
    (
        Vec<SignedVotingEvent>,
        CommunityVotingPolicy,
        HashMap<PollId, PollRestore>,
    ),
    PersistError,
> {
    let empty = || (Vec::new(), CommunityVotingPolicy::default(), HashMap::new());
    // Single-syscall NotFound handling (no TOCTOU between exists+read).
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(empty());
        }
        Err(e) => return Err(PersistError::Io(e)),
    };
    match ciborium::from_reader::<PersistedVotingLog, _>(bytes.as_slice()) {
        Ok(record) if record.version != VOTING_LOG_SCHEMA_VERSION => {
            quarantine_corrupted(path, &format!("unknown schema version {}", record.version));
            Ok(empty())
        }
        Ok(record) if record.community_id != *expected_id => {
            quarantine_corrupted(
                path,
                &format!(
                    "community_id {:?} != expected {:?}",
                    record.community_id, expected_id
                ),
            );
            Ok(empty())
        }
        Ok(record) => Ok((record.events, record.policy, record.poll_restore)),
        Err(decode_err) => {
            quarantine_corrupted(path, &decode_err.to_string());
            Ok(empty())
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
        let (events, policy, _poll_meta) = load_voting_log(&path, &cid).unwrap();
        assert_eq!(events, log.events);
        assert_eq!(&policy, log.policy());
    }

    #[test]
    fn load_missing_file_returns_empty_default() {
        let dir = tempfile::tempdir().unwrap();
        let cid = SpaceId([5u8; 16]);
        let path = voting_path_for(dir.path(), &cid);
        let (events, policy, poll_meta) = load_voting_log(&path, &cid).unwrap();
        assert!(events.is_empty());
        assert!(poll_meta.is_empty());
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
        let (events, _policy, _pm) = load_voting_log(&path, &cid_b).unwrap();
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
            poll_restore: HashMap::new(),
        };
        let mut bytes = Vec::new();
        ciborium::into_writer(&record, &mut bytes).unwrap();
        write_atomic(&path, &bytes).unwrap();
        let (events, _, _) = load_voting_log(&path, &cid).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn load_truncated_cbor_quarantines_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cid = SpaceId([6u8; 16]);
        let path = voting_path_for(dir.path(), &cid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, [0xDE, 0xAD, 0xBE, 0xEF]).unwrap();
        let (events, policy, _pm) = load_voting_log(&path, &cid).unwrap();
        assert!(events.is_empty());
        assert_eq!(policy, CommunityVotingPolicy::default());
    }

    // ── ZEB-718 R2 (Greptile P1-2): policy persist must not clobber ────────

    #[test]
    fn save_policy_only_preserves_events_and_updates_policy() {
        // A community with persisted events; a later policy-only IPC must
        // update the policy WITHOUT dropping the events (the IPC's in-memory
        // log may not have loaded them).
        let dir = tempfile::tempdir().unwrap();
        let cid = SpaceId([9u8; 16]);
        let path = voting_path_for(dir.path(), &cid);
        let mut log = VotingLog::default();
        log.events.push(test_event(100, 0, "d1"));
        log.events.push(test_event(200, 1, "d2"));
        save_voting_log(&path, &log, &cid).unwrap();

        let new_policy = CommunityVotingPolicy {
            notify_on_delegate_signal: true,
            ..Default::default()
        };
        save_policy_only(&path, &cid, &new_policy).unwrap();

        let (events, policy, _pm) = load_voting_log(&path, &cid).unwrap();
        assert_eq!(events, log.events, "policy-only write must preserve events");
        assert_eq!(policy, new_policy, "policy must be updated");
    }

    #[test]
    fn save_policy_only_skips_unreadable_existing_file() {
        // Force a non-NotFound I/O read error by making voting.cbor a
        // directory (EISDIR). A policy write must then return Err and NOT
        // overwrite the recoverable file — mirrors the engine's disarm.
        let dir = tempfile::tempdir().unwrap();
        let cid = SpaceId([0xAu8; 16]);
        let path = voting_path_for(dir.path(), &cid);
        std::fs::create_dir_all(&path).unwrap();
        let new_policy = CommunityVotingPolicy {
            notify_on_delegate_signal: true,
            ..Default::default()
        };
        assert!(
            save_policy_only(&path, &cid, &new_policy).is_err(),
            "an unreadable existing file must not be clobbered by a policy write"
        );
        assert!(path.is_dir(), "the file must be left untouched");
    }
}
