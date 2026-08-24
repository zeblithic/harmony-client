//! Plaintext load-or-recover primitive (ZEB-986).
//!
//! Generalizes two recovery disciplines already proven elsewhere in the tree, for
//! plaintext `serde_json` / `ciborium` app-data stores:
//!
//! * **Io-vs-content freeze discrimination** ([`persistent_card_store`]): a transient
//!   *read* error must not lead to overwriting the (possibly still-good) on-disk bytes
//!   with an empty in-memory default. Such a load *freezes* writes.
//! * **Quarantine-aside on content corruption** (`fleet_dataset_file::load_or_recover`,
//!   `friend_requests`): genuinely corrupt bytes are renamed aside so the store can
//!   heal on the next write — unless the rename fails, in which case we freeze rather
//!   than clobber (the ZEB-784 rule).
//!
//! Encryption of these families is a later pass (ZEB-983 `DeviceCipher`); this module
//! is deliberately plaintext-only.
//!
//! [`persistent_card_store`]: crate::persistent_card_store

use std::path::Path;

/// Current wall-clock time in milliseconds since the Unix epoch. Convenience for
/// production callers threading `now_ms` into [`load_or_recover`] / [`sweep_corrupt_sidecars`];
/// tests pass a fixed value instead so quarantine names and age gates stay deterministic.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Outcome of a recoverable load.
pub struct Recovered<T> {
    /// The loaded value, or `T::default()` when the file was missing, unreadable, or
    /// corrupt.
    pub value: T,
    /// When `true`, the store MUST treat its `save()` as a no-op: the on-disk bytes may
    /// still be good and must not be overwritten with `value` (a default). Set on a
    /// transient read error, on a content-corrupt file under [`CorruptPolicy::FreezeInPlace`],
    /// and on a quarantine-rename failure under [`CorruptPolicy::QuarantineAndHeal`].
    pub disk_write_frozen: bool,
}

/// What to do when the file's *content* is corrupt (bad bytes / parse error / version
/// mismatch). A read `Io` error always freezes regardless of policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CorruptPolicy {
    /// Rename the bad file aside (`<path>.corrupt-<now_ms>`), start from `T::default()`,
    /// and heal on the next write. If the rename fails, fall back to freeze (never heal
    /// over a file we could not move aside). For stores whose empty-default is
    /// non-destructive and user-rebuildable in-session: follows, friend_nicknames,
    /// vine_feed_cache, vine_pull.
    QuarantineAndHeal,
    /// Leave the bad file untouched, start from `T::default()`, and freeze writes. For
    /// stores where overwriting-with-empty is destructive to *other* data: content_index
    /// (an empty index orphans every stored blob; its sensitivity / provenance metadata
    /// cannot be rebuilt from a directory scan).
    FreezeInPlace,
}

/// Load `path`, classifying failures so a possibly-good file is never silently clobbered.
///
/// * missing file        → `(default, frozen = false)` — first run, silent.
/// * read `Io` error     → `(default, frozen = true)` + warn — preserve maybe-good bytes.
/// * `parse` `Err`       → per `policy` (see [`CorruptPolicy`]).
/// * `parse` `Ok(v)`     → `(v, frozen = false)`.
///
/// `parse` performs deserialize plus any version/shape validation, returning `Err(reason)`
/// for content corruption. This function never panics.
pub fn load_or_recover<T: Default>(
    path: &Path,
    now_ms: u64,
    policy: CorruptPolicy,
    parse: impl FnOnce(&[u8]) -> Result<T, String>,
) -> Recovered<T> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Recovered {
                value: T::default(),
                disk_write_frozen: false,
            };
        }
        Err(e) => {
            tracing::warn!(
                path = ?path,
                error = %e,
                "recoverable_load: read failed; starting empty and FREEZING writes to preserve maybe-good bytes"
            );
            return Recovered {
                value: T::default(),
                disk_write_frozen: true,
            };
        }
    };
    match parse(&bytes) {
        Ok(value) => Recovered {
            value,
            disk_write_frozen: false,
        },
        Err(reason) => match policy {
            CorruptPolicy::FreezeInPlace => {
                tracing::warn!(
                    path = ?path,
                    reason = %reason,
                    "recoverable_load: content corrupt; FREEZING in place (file preserved, degraded read-only)"
                );
                Recovered {
                    value: T::default(),
                    disk_write_frozen: true,
                }
            }
            CorruptPolicy::QuarantineAndHeal => {
                if quarantine(path, now_ms) {
                    tracing::warn!(
                        path = ?path,
                        reason = %reason,
                        "recoverable_load: content corrupt; quarantined aside, healing on next write"
                    );
                    Recovered {
                        value: T::default(),
                        disk_write_frozen: false,
                    }
                } else {
                    tracing::warn!(
                        path = ?path,
                        reason = %reason,
                        "recoverable_load: content corrupt but quarantine rename FAILED; FREEZING to avoid clobber"
                    );
                    Recovered {
                        value: T::default(),
                        disk_write_frozen: true,
                    }
                }
            }
        },
    }
}

/// Rename `path` → `<path>.corrupt-<now_ms>` (dash dialect, matching the fleet/sync
/// majority). Best-effort; returns `false` and warns on rename failure.
fn quarantine(path: &Path, now_ms: u64) -> bool {
    let mut aside = path.as_os_str().to_os_string();
    aside.push(format!(".corrupt-{now_ms}"));
    match std::fs::rename(path, &aside) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(path = ?path, error = %e, "recoverable_load: quarantine rename failed");
            false
        }
    }
}

/// Split a trailing `.corrupt.<digits>` or `.corrupt-<digits>` off a file name,
/// returning `(base_name, stamp_ms)`. Returns `None` if `name` is not a corrupt
/// sidecar (unrecognized names are never treated as sweepable).
fn split_corrupt_sidecar(name: &str) -> Option<(&str, u64)> {
    for sep in [".corrupt.", ".corrupt-"] {
        if let Some(idx) = name.rfind(sep) {
            let base = &name[..idx];
            let digits = &name[idx + sep.len()..];
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                if let Ok(stamp) = digits.parse::<u64>() {
                    return Some((base, stamp));
                }
            }
        }
    }
    None
}

/// Delete stale quarantine sidecars under `dir` (recursively). Matches both dialects
/// (`.corrupt.<ms>` dotted, `.corrupt-<ms>` dashed) and both files and directories
/// (the channel-log family quarantines a whole `<root>` dir).
///
/// Retention: within each `(parent dir, base name)` group, always keep the single
/// newest sidecar as a forensics floor; delete any other whose age (`now_ms` minus the
/// embedded `<ms>`) exceeds `max_age_ms`. Best-effort and non-fatal — an unreadable
/// subdirectory is skipped, and a failed delete is logged, never propagated. Names
/// whose `<ms>` suffix does not parse are never deleted.
pub fn sweep_corrupt_sidecars(dir: &Path, now_ms: u64, max_age_ms: u64) {
    let mut scanned = 0usize;
    let mut deleted = 0usize;
    let mut errors = 0usize;
    sweep_dir(
        dir,
        now_ms,
        max_age_ms,
        &mut scanned,
        &mut deleted,
        &mut errors,
    );
    if scanned > 0 || deleted > 0 {
        tracing::info!(dir = ?dir, scanned, deleted, errors, "sweep_corrupt_sidecars complete");
    }
}

fn sweep_dir(
    dir: &Path,
    now_ms: u64,
    max_age_ms: u64,
    scanned: &mut usize,
    deleted: &mut usize,
    errors: &mut usize,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return, // unreadable dir: skip, non-fatal
    };
    // Group corrupt sidecars in THIS dir by base name; recurse into non-corrupt subdirs.
    let mut groups: std::collections::HashMap<String, Vec<(u64, std::path::PathBuf, bool)>> =
        std::collections::HashMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if let Some((base, stamp)) = split_corrupt_sidecar(&name) {
            *scanned += 1;
            groups
                .entry(base.to_string())
                .or_default()
                .push((stamp, path, is_dir));
        } else if is_dir {
            // Recurse into ordinary subdirs (e.g. communities/{cid}/channels/...).
            sweep_dir(&path, now_ms, max_age_ms, scanned, deleted, errors);
        }
    }
    for (_base, mut group) in groups {
        group.sort_by_key(|(stamp, _, _)| *stamp);
        let newest_idx = group.len() - 1; // ascending sort → last is newest
        for (i, (stamp, path, is_dir)) in group.iter().enumerate() {
            if i == newest_idx {
                continue; // forensics floor: always keep the newest per base
            }
            if now_ms.saturating_sub(*stamp) <= max_age_ms {
                continue;
            }
            let res = if *is_dir {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            };
            match res {
                Ok(()) => *deleted += 1,
                Err(e) => {
                    *errors += 1;
                    tracing::warn!(path = ?path, error = %e, "sweep_corrupt_sidecars: delete failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_json(bytes: &[u8]) -> Result<Vec<u32>, String> {
        serde_json::from_slice(bytes).map_err(|e| e.to_string())
    }

    #[test]
    fn missing_file_defaults_not_frozen() {
        let dir = tempfile::tempdir().unwrap();
        let r = load_or_recover::<Vec<u32>>(
            &dir.path().join("nope.json"),
            1_000,
            CorruptPolicy::QuarantineAndHeal,
            parse_json,
        );
        assert!(r.value.is_empty());
        assert!(!r.disk_write_frozen);
    }

    #[test]
    fn good_file_loads_not_frozen() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("v.json");
        std::fs::write(&p, b"[1,2,3]").unwrap();
        let r =
            load_or_recover::<Vec<u32>>(&p, 1_000, CorruptPolicy::QuarantineAndHeal, parse_json);
        assert_eq!(r.value, vec![1, 2, 3]);
        assert!(!r.disk_write_frozen);
    }

    #[test]
    fn corrupt_quarantine_and_heal_moves_aside_not_frozen() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("v.json");
        std::fs::write(&p, b"not json").unwrap();
        let r =
            load_or_recover::<Vec<u32>>(&p, 4_242, CorruptPolicy::QuarantineAndHeal, parse_json);
        assert!(r.value.is_empty());
        assert!(!r.disk_write_frozen);
        assert!(!p.exists(), "original renamed aside");
        let aside = dir.path().join("v.json.corrupt-4242");
        assert_eq!(std::fs::read(&aside).unwrap(), b"not json");
    }

    #[test]
    fn corrupt_freeze_in_place_leaves_file_and_freezes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("v.json");
        std::fs::write(&p, b"not json").unwrap();
        let r = load_or_recover::<Vec<u32>>(&p, 4_242, CorruptPolicy::FreezeInPlace, parse_json);
        assert!(r.value.is_empty());
        assert!(r.disk_write_frozen);
        assert_eq!(std::fs::read(&p).unwrap(), b"not json", "file untouched");
        assert!(
            !dir.path().join("v.json.corrupt-4242").exists(),
            "no sidecar under freeze-in-place"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_io_error_freezes() {
        // A directory at the read path makes std::fs::read fail with a non-NotFound error.
        let dir = tempfile::tempdir().unwrap();
        let subdir = dir.path().join("asdir");
        std::fs::create_dir(&subdir).unwrap();
        let r = load_or_recover::<Vec<u32>>(
            &subdir,
            1_000,
            CorruptPolicy::QuarantineAndHeal,
            parse_json,
        );
        assert!(
            r.disk_write_frozen,
            "read error on a directory path freezes"
        );
    }

    #[cfg(unix)]
    #[test]
    fn quarantine_rename_failure_freezes_no_heal() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("ro");
        std::fs::create_dir(&sub).unwrap();
        let p = sub.join("v.json");
        std::fs::write(&p, b"not json").unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o555)).unwrap();
        let r =
            load_or_recover::<Vec<u32>>(&p, 4_242, CorruptPolicy::QuarantineAndHeal, parse_json);
        // restore perms before assertions so tempdir cleanup succeeds regardless
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            r.disk_write_frozen,
            "failed quarantine falls back to freeze"
        );
        assert!(p.exists(), "original left in place");
    }

    #[test]
    fn stamp_parses_both_dialects_and_rejects_others() {
        assert_eq!(
            split_corrupt_sidecar("follows.json.corrupt-4242"),
            Some(("follows.json", 4242))
        );
        assert_eq!(
            split_corrupt_sidecar("crdt.cbor.corrupt.99"),
            Some(("crdt.cbor", 99))
        );
        assert_eq!(split_corrupt_sidecar("follows.json"), None);
        assert_eq!(split_corrupt_sidecar("x.corrupt-notdigits"), None);
        assert_eq!(split_corrupt_sidecar("x.corrupt"), None);
    }

    #[test]
    fn sweep_deletes_old_keeps_newest_and_young() {
        let dir = tempfile::tempdir().unwrap();
        let day = 86_400_000u64;
        let now = 100 * day;
        std::fs::write(dir.path().join("a.json.corrupt-1"), b"x").unwrap(); // ancient, not newest → delete
        std::fs::write(
            dir.path().join(format!("a.json.corrupt-{}", 10 * day)),
            b"x",
        )
        .unwrap(); // old, not newest → delete
        std::fs::write(
            dir.path().join(format!("a.json.corrupt-{}", 99 * day)),
            b"x",
        )
        .unwrap(); // newest of a (age 1d) → keep
        std::fs::write(dir.path().join("b.cbor.corrupt.1"), b"x").unwrap(); // sole sidecar of b → keep (floor)
        std::fs::write(dir.path().join("keep.json"), b"x").unwrap(); // ordinary file → untouched

        sweep_corrupt_sidecars(dir.path(), now, 30 * day);

        assert!(!dir.path().join("a.json.corrupt-1").exists());
        assert!(!dir
            .path()
            .join(format!("a.json.corrupt-{}", 10 * day))
            .exists());
        assert!(
            dir.path()
                .join(format!("a.json.corrupt-{}", 99 * day))
                .exists(),
            "newest of a kept"
        );
        assert!(
            dir.path().join("b.cbor.corrupt.1").exists(),
            "sole sidecar of b kept as floor"
        );
        assert!(dir.path().join("keep.json").exists());
    }

    #[test]
    fn sweep_handles_corrupt_directories() {
        let dir = tempfile::tempdir().unwrap();
        let day = 86_400_000u64;
        let now = 100 * day;
        let d1 = dir.path().join("root.corrupt.1");
        std::fs::create_dir(&d1).unwrap();
        std::fs::write(d1.join("inner"), b"x").unwrap();
        let d2 = dir.path().join(format!("root.corrupt.{}", 99 * day));
        std::fs::create_dir(&d2).unwrap();
        sweep_corrupt_sidecars(dir.path(), now, 30 * day);
        assert!(!d1.exists(), "old corrupt dir removed whole");
        assert!(d2.exists(), "newest corrupt dir kept");
    }

    #[test]
    fn sweep_recurses_into_ordinary_subdirs() {
        let dir = tempfile::tempdir().unwrap();
        let day = 86_400_000u64;
        let now = 100 * day;
        let comm = dir.path().join("communities").join("abc");
        std::fs::create_dir_all(&comm).unwrap();
        std::fs::write(comm.join("voting.cbor.corrupt-1"), b"x").unwrap(); // old, not newest → delete
        std::fs::write(comm.join(format!("voting.cbor.corrupt-{}", 99 * day)), b"x").unwrap(); // newest → keep
        sweep_corrupt_sidecars(dir.path(), now, 30 * day);
        assert!(
            !comm.join("voting.cbor.corrupt-1").exists(),
            "nested old non-newest deleted"
        );
        assert!(comm
            .join(format!("voting.cbor.corrupt-{}", 99 * day))
            .exists());
    }
}
