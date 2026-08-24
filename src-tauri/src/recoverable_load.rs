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
}
