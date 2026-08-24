//! Last-backup tracking + 14-day staleness logic.
//!
//! Backs the GUI staleness banner. See spec §"Staleness warning".

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::owner_state_crdt::OwnerState;
use crate::owner_state_types::Hlc;

/// 14 days in milliseconds. Trigger threshold for the staleness banner.
pub const STALENESS_THRESHOLD_MS: u64 = 14 * 86_400_000;

/// Schema for `~/.harmony/last_backup.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastBackup {
    /// HLC at which the last successful `export recovery-file` ran.
    pub at: Hlc,
    /// Whether the last export included a state sidecar.
    pub include_state: bool,
    /// Absolute path of the last export's HRMR file (for UX, not security).
    pub out_path: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BackupStateError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON decode: {0}")]
    JsonDecode(#[from] serde_json::Error),
}

/// Read `last_backup.json` from disk. Returns `Ok(None)` if the file
/// doesn't exist (fresh install or no backups yet).
pub fn load_last_backup(path: &Path) -> Result<Option<LastBackup>, BackupStateError> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let parsed: LastBackup = serde_json::from_slice(&bytes)?;
            Ok(Some(parsed))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Atomically replace `last_backup.json` with the supplied record.
pub fn save_last_backup(path: &Path, record: &LastBackup) -> Result<(), BackupStateError> {
    let bytes = serde_json::to_vec_pretty(record)?;
    crate::owner_state_persist::save_atomically(path, &bytes)
        .map_err(|e| BackupStateError::Io(std::io::Error::other(e.to_string())))
}

/// Find the maximum `wall_ms` across all mutating entries in an owner-state.
/// Returns 0 if the state is empty.
///
/// Scans every HLC-bearing collection on `OwnerState`:
/// - `spaces` (Space.updated_at)
/// - `outbox` (OutboxEntry.created_at)
/// - `inbox` (InboxEntry.received_at)
/// - `markers` (ReadMarker.last_read_at)
/// - `owner_device_cache.devices` (OwnerDeviceEntry.learned_at)
/// - `libraries` (LibraryEntry.added_at, plus removed_at if Some)
/// - `outbox_tombstones` (values are Hlc directly)
///
/// Missing a collection here causes false-negative staleness for users
/// whose only recent mutations were in that collection (e.g. "added a
/// library" or "rotated a bound device").
pub fn last_mutation_wall_ms(state: &OwnerState) -> u64 {
    let mut max_ms = 0u64;
    for s in state.spaces.values() {
        max_ms = max_ms.max(s.updated_at.wall_ms);
    }
    for o in state.outbox.values() {
        max_ms = max_ms.max(o.created_at.wall_ms);
    }
    for i in state.inbox.values() {
        max_ms = max_ms.max(i.received_at.wall_ms);
    }
    for m in state.markers.values() {
        max_ms = max_ms.max(m.last_read_at.wall_ms);
    }
    for entry in state.owner_device_cache.devices.values() {
        max_ms = max_ms.max(entry.learned_at.wall_ms);
    }
    for lib in state.libraries.values() {
        max_ms = max_ms.max(lib.added_at.wall_ms);
        if let Some(rm) = lib.removed_at.as_ref() {
            max_ms = max_ms.max(rm.wall_ms);
        }
    }
    for hlc in state.outbox_tombstones.values() {
        max_ms = max_ms.max(hlc.wall_ms);
    }
    max_ms
}

/// Trigger result returned to the IPC layer + GUI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StalenessResult {
    pub is_stale: bool,
    /// Whole days since the last backup. 0 if no `last_backup.json` exists
    /// AND no CRDT mutations happened.
    pub days_since: u32,
}

/// Decide whether the staleness banner should appear.
///
/// `now_wall_ms` is the system wall-clock for the comparison (caller injects;
/// production wires `std::time::SystemTime::now()`).
/// `dismiss_until_wall_ms` is the localStorage-tracked dismissal expiry
/// (or `None` if the user has never dismissed). When `Some(t)` and `t >
/// now_wall_ms`, suppress the banner regardless of staleness.
///
/// **`include_state` semantics** — a `LastBackup` record with
/// `include_state == false` reflects an identity-only export (HRMR only;
/// `--no-state` or GUI toggle off). Those backups DO NOT reset
/// state-staleness because they didn't back up owner-state. The function
/// treats them as if no backup had been taken for staleness purposes
/// (falling through to the `None` branch's last-mutation-baseline logic).
/// Bots: C1 (Qodo/CodeAnt/CodeRabbit/Cursor, 4-way agreement).
pub fn should_warn_about_stale_backup(
    now_wall_ms: u64,
    last_backup: Option<&LastBackup>,
    state: &OwnerState,
    dismiss_until_wall_ms: Option<u64>,
) -> StalenessResult {
    if let Some(until) = dismiss_until_wall_ms {
        if until > now_wall_ms {
            return StalenessResult {
                is_stale: false,
                days_since: 0,
            };
        }
    }

    let last_mutation = last_mutation_wall_ms(state);
    // An identity-only backup (include_state == false) does NOT reset
    // state-staleness — owner-state was not backed up, so the staleness
    // baseline remains the last mutation (just like the `None` branch).
    // See C1 in the round-1 bot findings.
    let effective_backup = last_backup.filter(|b| b.include_state);
    match effective_backup {
        None => {
            // No state-backup baseline. Apply the 14-day grace window
            // against the last mutation — a fresh-install user (or a
            // user whose only backups were identity-only) gets the banner
            // when their mutations have been unbacked-up for ≥14 days.
            // Without this grace, the first DM would trigger an immediate
            // "BACKUP NOW" banner on day-0 installs.
            let stale = last_mutation > 0 && now_wall_ms > last_mutation + STALENESS_THRESHOLD_MS;
            let days = if last_mutation > 0 {
                ((now_wall_ms.saturating_sub(last_mutation)) / 86_400_000) as u32
            } else {
                0
            };
            StalenessResult {
                is_stale: stale,
                days_since: days,
            }
        }
        Some(b) => {
            // Stale iff there have been mutations since the last backup
            // AND those mutations are older than 14 days ago.
            let last_backup_ms = b.at.wall_ms;
            let stale = last_mutation > last_backup_ms
                && now_wall_ms > last_backup_ms + STALENESS_THRESHOLD_MS;
            let days = ((now_wall_ms.saturating_sub(last_backup_ms)) / 86_400_000) as u32;
            StalenessResult {
                is_stale: stale,
                days_since: days,
            }
        }
    }
}

/// ZEB-975: evaluate staleness from the directory the WRITERS use.
///
/// `harmony_dir` is the identity dir (`~/.harmony[/profiles/<p>]`) — the
/// directory hosting `owner_state_crdt.cbor` (written by the boot engine and
/// `recovery_cli` restore) and `last_backup.json` (written by the export
/// path). Production resolves it via `owner_commands::resolve_identity_dir()`;
/// tests pass a tempdir they exported into. Keeping load + decide in one
/// dir-keyed helper makes "reader follows writer" a testable property — the
/// pre-fix reader resolved Tauri's app-data dir here, which nothing writes,
/// so the staleness banner could never fire.
///
/// Missing/corrupt files degrade exactly like the fresh-install case: an
/// unreadable CRDT evaluates as an empty `OwnerState`, a missing
/// `last_backup.json` as "never backed up" — both feed the grace-window
/// logic in [`should_warn_about_stale_backup`].
pub fn staleness_from_dir(
    harmony_dir: &Path,
    now_wall_ms: u64,
    dismiss_until_wall_ms: Option<u64>,
) -> StalenessResult {
    // PR #728 review (CA-5): a read-only staleness query must not create a
    // node identity as a side effect — get_or_derive fresh-generates when
    // none exists. Absent file → the same default-state answer the loader
    // gives for a missing file, with zero derives.
    let state_path = crate::recovery_cli::owner_state_path(harmony_dir);
    let state = if state_path.exists() {
        crate::device_dataset_file::get_or_derive(harmony_dir)
            .ok()
            .and_then(|cipher| crate::owner_state_persist::load_crdt(&cipher, &state_path).ok())
            .unwrap_or_default()
    } else {
        OwnerState::default()
    };
    let last =
        load_last_backup(&crate::recovery_cli::last_backup_path(harmony_dir)).unwrap_or(None);
    should_warn_about_stale_backup(now_wall_ms, last.as_ref(), &state, dismiss_until_wall_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::{
        LibraryEntry, OutboxEntryId, OwnerAddr, Space, SpaceId, SpaceKind,
    };

    fn hlc(w: u64) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: "test".into(),
        }
    }

    fn state_with_mutation_at(wall_ms: u64) -> OwnerState {
        let mut s = OwnerState::default();
        if wall_ms > 0 {
            let sp = Space {
                id: SpaceId([1; 16]),
                kind: SpaceKind::Folder,
                parent: None,
                community_id: None,
                name: "x".into(),
                transport: None,
                members: vec![],
                custom_name: None,
                notification_pref: None,
                left_at: None,
                created_at: hlc(wall_ms),
                updated_at: hlc(wall_ms),
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
            s.spaces.insert(sp.id, sp);
        }
        s
    }

    #[test]
    fn staleness_warning_triggers_after_14_days() {
        let now_ms = 100 * 86_400_000;
        let backup_at = 80 * 86_400_000; // 20 days ago
        let mutation_at = 85 * 86_400_000; // 15 days ago, after the backup
        let last = LastBackup {
            at: hlc(backup_at),
            include_state: true,
            out_path: "/tmp/recovery.bin".into(),
        };
        let state = state_with_mutation_at(mutation_at);
        let r = should_warn_about_stale_backup(now_ms, Some(&last), &state, None);
        assert!(r.is_stale, "should warn: {r:?}");
        assert_eq!(r.days_since, 20);

        // 13 days ago: not yet stale.
        let now_ms = 80 * 86_400_000 + 13 * 86_400_000;
        let r = should_warn_about_stale_backup(now_ms, Some(&last), &state, None);
        assert!(!r.is_stale, "13d should not warn: {r:?}");
    }

    #[test]
    fn staleness_warning_handles_missing_file() {
        // 100 days expressed in ms — comfortably past the 14-day window
        // so the subtractions below don't underflow.
        let now_ms = 100 * 86_400_000;
        // No `last_backup.json`, no mutations: don't nag.
        let empty = OwnerState::default();
        let r = should_warn_about_stale_backup(now_ms, None, &empty, None);
        assert!(!r.is_stale, "fresh install, no mutations -> no warn");

        // No `last_backup.json`, mutations 1 day ago: not yet 14 days stale.
        let recent = state_with_mutation_at(now_ms - 86_400_000);
        let r = should_warn_about_stale_backup(now_ms, None, &recent, None);
        assert!(
            !r.is_stale,
            "1-day-old mutation, no backup, still under 14d threshold"
        );

        // No `last_backup.json`, mutations 15 days ago: NOW warn.
        let stale = state_with_mutation_at(now_ms - 15 * 86_400_000);
        let r = should_warn_about_stale_backup(now_ms, None, &stale, None);
        assert!(r.is_stale, "15-day-old mutation, no backup -> warn");
    }

    #[test]
    fn dismiss_window_suppresses_warning() {
        let now_ms = 100 * 86_400_000;
        let last = LastBackup {
            at: hlc(80 * 86_400_000),
            include_state: true,
            out_path: "/tmp/r.bin".into(),
        };
        let state = state_with_mutation_at(85 * 86_400_000);

        // Dismiss until 5 days from now → suppressed.
        let dismiss = Some(now_ms + 5 * 86_400_000);
        let r = should_warn_about_stale_backup(now_ms, Some(&last), &state, dismiss);
        assert!(!r.is_stale, "dismiss window active -> no warn");

        // Dismiss expired 1 day ago → re-appears.
        let dismiss = Some(now_ms - 86_400_000);
        let r = should_warn_about_stale_backup(now_ms, Some(&last), &state, dismiss);
        assert!(r.is_stale, "dismiss expired -> warn again");
    }

    #[test]
    fn last_backup_json_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last_backup.json");
        let record = LastBackup {
            at: hlc(1_700_000_000),
            include_state: true,
            out_path: "/tmp/recovery.bin".into(),
        };
        save_last_backup(&path, &record).unwrap();
        let loaded = load_last_backup(&path).unwrap().expect("present");
        assert_eq!(loaded, record);
    }

    #[test]
    fn last_mutation_includes_libraries_and_tombstones() {
        // Start from a state with NO spaces/outbox/inbox/markers mutations
        // but a library added 100ms ago.
        let mut state = OwnerState::default();
        let addr = OwnerAddr([7u8; 16]);
        let library_at = 100u64;
        state.libraries.insert(
            addr,
            LibraryEntry {
                address: addr,
                added_at: hlc(library_at),
                removed_at: None,
            },
        );
        assert_eq!(
            last_mutation_wall_ms(&state),
            library_at,
            "library add should drive last_mutation_wall_ms"
        );

        // A later outbox_tombstone should now win.
        let tomb_at = 500u64;
        state
            .outbox_tombstones
            .insert(OutboxEntryId([9u8; 16]), hlc(tomb_at));
        assert_eq!(
            last_mutation_wall_ms(&state),
            tomb_at,
            "later outbox_tombstone HLC should dominate library add"
        );
    }

    #[test]
    fn load_last_backup_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never_written.json");
        let r = load_last_backup(&path).unwrap();
        assert!(r.is_none());
    }

    /// Round-1 bot finding C1: a `LastBackup` record with
    /// `include_state == false` (identity-only export) must NOT suppress
    /// the state-staleness banner — owner-state was never backed up, so
    /// the staleness baseline is the last mutation, not `b.at`.
    ///
    /// 4-way bot agreement (Qodo + CodeAnt + CodeRabbit + Cursor).
    #[test]
    fn staleness_with_identity_only_backup_does_not_suppress_banner() {
        let now_ms = 100 * 86_400_000;
        // Recent identity-only "backup" 1 day ago. Under the pre-fix
        // behavior, this would suppress staleness because `b.at` was
        // less than 14 days old. Under the fix, the `include_state ==
        // false` flag makes the function treat this as "no state-backup
        // baseline" and fall through to the last-mutation logic.
        let last = LastBackup {
            at: hlc(now_ms - 86_400_000), // 1 day ago — RECENT
            include_state: false,
            out_path: "/tmp/r.bin".into(),
        };
        // Mutations from 20 days ago — older than the 14-day staleness
        // threshold. Should produce `is_stale: true`.
        let state = state_with_mutation_at(now_ms - 20 * 86_400_000);
        let r = should_warn_about_stale_backup(now_ms, Some(&last), &state, None);
        assert!(
            r.is_stale,
            "identity-only backup must NOT reset state-staleness; got: {r:?}"
        );
        assert_eq!(
            r.days_since, 20,
            "days_since must reflect last-mutation baseline (20d), not last-backup baseline (1d)"
        );

        // Sanity: same setup but `include_state: true` — backup IS the
        // baseline and the banner is suppressed (1 day old < 14 days).
        let last_with_state = LastBackup {
            at: hlc(now_ms - 86_400_000),
            include_state: true,
            out_path: "/tmp/r.bin".into(),
        };
        let r = should_warn_about_stale_backup(now_ms, Some(&last_with_state), &state, None);
        assert!(
            !r.is_stale,
            "state-inclusive backup 1 day ago should suppress; got: {r:?}"
        );
    }
}
