//! Plaintext load-or-recover primitive (ZEB-986).
//!
//! Generalizes two recovery disciplines already proven elsewhere in the tree, for
//! plaintext `serde_json` / `ciborium` app-data stores:
//!
//! * **Io-vs-content freeze discrimination** (`persistent_card_store`, in `harmony-app`):
//!   a transient *read* error must not lead to overwriting the (possibly still-good)
//!   on-disk bytes with an empty in-memory default. Such a load *freezes* writes.
//! * **Quarantine-aside on content corruption** (`fleet_dataset_file::load_or_recover`,
//!   `friend_requests`): genuinely corrupt bytes are renamed aside so the store can
//!   heal on the next write — unless the rename fails, in which case we freeze rather
//!   than clobber (the ZEB-784 rule).
//!
//! [`load_or_recover`] is the plaintext primitive. [`load_sealed_or_recover`] (ZEB-986
//! PR-3) is its sibling for files under the ZEB-982 device envelope: it reads through
//! [`crate::device_dataset_file::read_image`] so the transient-vs-content split comes from
//! the envelope layer, and it *freezes* (never quarantines) a sealed image that will not
//! decrypt — a wrong/rotated key must not wipe the store.

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
    /// transient read error, and on a quarantine-rename failure (we could not move the
    /// corrupt file aside, so healing over it would clobber recoverable bytes).
    pub disk_write_frozen: bool,
}

/// Load `path`, classifying failures so a possibly-good file is never silently clobbered.
///
/// - **missing file** → `(default, frozen = false)` — first run, silent.
/// - **read `Io` error** → `(default, frozen = true)` + warn — preserve maybe-good bytes.
/// - **`parse` `Err`** → quarantine aside (`<path>.corrupt-<now_ms>`) and heal on next write → `(default, frozen = false)`; if the rename fails, freeze instead (never heal over a file we could not move aside — ZEB-784).
/// - **`parse` `Ok(v)`** → `(v, frozen = false)`.
///
/// `parse` performs deserialize plus any version/shape validation, returning `Err(reason)`
/// for content corruption. A store that wants an unsupported-but-parseable file *frozen in
/// place* (rather than quarantined) — e.g. a forward-version file — parses it as `Ok` and
/// decides to freeze at its own layer. This function never panics.
pub fn load_or_recover<T: Default>(
    path: &Path,
    now_ms: u64,
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
        Err(reason) => {
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
    }
}

/// Sealed sibling of [`load_or_recover`] for files under the ZEB-982 device envelope.
///
/// Reads through [`crate::device_dataset_file::read_image`], so the transient-vs-content
/// split is supplied by the envelope layer:
///
/// - **missing file** → `(default, frozen = false)` — first run, silent.
/// - **read `Io` error** → `(default, frozen = true)` + warn — preserve maybe-good bytes.
/// - **sealed-image `Crypto` error** (bad AEAD tag, truncated envelope, wrong/rotated key) → `(default, frozen = true)` + warn. FREEZE, never quarantine: a sealed file that will not decrypt must not be wiped — it may be a key rotation not yet reflected, and these stores re-derive from the network. This is the key divergence from [`load_or_recover`], whose content-error path quarantines.
/// - **`parse` `Err` on a legacy plaintext file** (`was_legacy == true`) → quarantine aside (`<path>.corrupt-<now_ms>`) and heal on next write → `(default, frozen = false)`; rename-fail → freeze.
/// - **`parse` `Err` on a sealed file** (`was_legacy == false` — it decrypted, but its inner payload does not parse) → FREEZE and preserve in place, never quarantine (quarantining unfreezes and lets the next save replace an authenticated file).
/// - **`parse` `Ok(v)`** → [`crate::device_dataset_file::reseal_if_legacy`] (lazy plaintext→sealed migration), then `(v, frozen = false)`.
///
/// `filename` is the canonical name bound as the envelope AAD; it must match the file the
/// bytes live in. Version/shape validation past a successful parse stays at the store layer
/// (a parseable-but-unsupported version freezes in place there). This function never panics.
///
/// `cipher` is `None` on a pre-identity boot (the device seed does not exist yet, so no key
/// can be derived without fresh-generating an identity as a side effect). In that case an
/// existing file is left frozen (never wiped) and an absent file is first-run empty.
pub fn load_sealed_or_recover<T: Default>(
    cipher: Option<&crate::device_dataset_file::DeviceCipher>,
    path: &Path,
    filename: &str,
    now_ms: u64,
    parse: impl FnOnce(&[u8]) -> Result<T, String>,
) -> Recovered<T> {
    use crate::device_dataset_file::{read_image, reseal_if_legacy, ImageError};
    let Some(cipher) = cipher else {
        // No device cipher (pre-identity boot): we cannot decrypt. Never wipe an existing
        // file — freeze if one is present; otherwise treat as first-run empty.
        let frozen = path.exists();
        if frozen {
            tracing::warn!(
                path = ?path,
                "recoverable_load: no device cipher available; FREEZING to preserve the existing file"
            );
        }
        return Recovered {
            value: T::default(),
            disk_write_frozen: frozen,
        };
    };
    let image = match read_image(cipher, path, filename) {
        Ok(None) => {
            return Recovered {
                value: T::default(),
                disk_write_frozen: false,
            };
        }
        Ok(Some(image)) => image,
        Err(ImageError::Io(e)) => {
            tracing::warn!(
                path = ?path,
                error = %e,
                "recoverable_load: sealed read failed; starting empty and FREEZING writes to preserve maybe-good bytes"
            );
            return Recovered {
                value: T::default(),
                disk_write_frozen: true,
            };
        }
        Err(ImageError::Crypto(msg)) => {
            tracing::warn!(
                path = ?path,
                error = %msg,
                "recoverable_load: sealed image would not decrypt; FREEZING (not quarantining) to avoid wiping on a possible wrong/rotated key"
            );
            return Recovered {
                value: T::default(),
                disk_write_frozen: true,
            };
        }
    };
    match parse(&image.bytes) {
        Ok(value) => {
            reseal_if_legacy(cipher, path, filename, &image);
            Recovered {
                value,
                disk_write_frozen: false,
            }
        }
        // A SEALED file that decrypted cleanly (`was_legacy == false`) but whose inner
        // content does not parse is NOT legacy-plaintext corruption to heal — it is an
        // authenticated file whose payload we cannot use. Preserve it (freeze), never
        // quarantine: quarantining renames it aside and unfreezes, so the next save would
        // replace it. Only genuine legacy-plaintext parse failures quarantine-and-heal.
        Err(reason) if !image.was_legacy => {
            tracing::warn!(
                path = ?path,
                reason = %reason,
                "recoverable_load: sealed content unparseable after decrypt; FREEZING to preserve (not quarantining)"
            );
            Recovered {
                value: T::default(),
                disk_write_frozen: true,
            }
        }
        Err(reason) => {
            if quarantine(path, now_ms) {
                tracing::warn!(
                    path = ?path,
                    reason = %reason,
                    "recoverable_load: legacy-plaintext content unparseable; quarantined aside, healing on next write"
                );
                Recovered {
                    value: T::default(),
                    disk_write_frozen: false,
                }
            } else {
                tracing::warn!(
                    path = ?path,
                    reason = %reason,
                    "recoverable_load: legacy-plaintext content unparseable but quarantine rename FAILED; FREEZING to avoid clobber"
                );
                Recovered {
                    value: T::default(),
                    disk_write_frozen: true,
                }
            }
        }
    }
}

/// Rename `path` → `<path>.corrupt-<stamp>` (dash dialect, matching the fleet/sync
/// majority). Best-effort; returns `false` and warns on rename failure.
///
/// `std::fs::rename` replaces the destination on Unix (and can fail on Windows) if it
/// already exists, so two corrupt loads of the same `path` at the same `now_ms` — e.g.
/// under a stuck clock — would otherwise clobber the first sidecar's bytes. Probe for a
/// free `<stamp>` (incrementing keeps the name parseable by [`sweep_corrupt_sidecars`])
/// so both payloads stay recoverable. If the whole window is occupied, returns `false`
/// (freeze) rather than overwriting an existing sidecar.
///
/// The `exists()` probe then `rename` is not atomic, but each store is loaded exactly
/// once at boot and there is a single process per data dir, so no two recoveries race the
/// same `path` concurrently; the sequential same-`now_ms` case is what the probe covers.
fn quarantine(path: &Path, now_ms: u64) -> bool {
    let mut stamp = now_ms;
    let aside = loop {
        let mut candidate = path.as_os_str().to_os_string();
        candidate.push(format!(".corrupt-{stamp}"));
        if !Path::new(&candidate).exists() {
            break candidate;
        }
        stamp = stamp.saturating_add(1);
        if stamp.saturating_sub(now_ms) > 1_000 {
            // Pathological (1001 sidecars already occupy this timestamp window): refuse
            // to overwrite an existing sidecar — that would discard retained recovery
            // evidence. Leave the file in place and signal a freeze, same posture as a
            // failed rename (ZEB-784).
            tracing::warn!(
                path = ?path,
                "recoverable_load: no free quarantine name; freezing instead of clobbering a sidecar"
            );
            return false;
        }
    };
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
        let r = load_or_recover::<Vec<u32>>(&dir.path().join("nope.json"), 1_000, parse_json);
        assert!(r.value.is_empty());
        assert!(!r.disk_write_frozen);
    }

    #[test]
    fn good_file_loads_not_frozen() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("v.json");
        std::fs::write(&p, b"[1,2,3]").unwrap();
        let r = load_or_recover::<Vec<u32>>(&p, 1_000, parse_json);
        assert_eq!(r.value, vec![1, 2, 3]);
        assert!(!r.disk_write_frozen);
    }

    #[test]
    fn corrupt_quarantine_and_heal_moves_aside_not_frozen() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("v.json");
        std::fs::write(&p, b"not json").unwrap();
        let r = load_or_recover::<Vec<u32>>(&p, 4_242, parse_json);
        assert!(r.value.is_empty());
        assert!(!r.disk_write_frozen);
        assert!(!p.exists(), "original renamed aside");
        let aside = dir.path().join("v.json.corrupt-4242");
        assert_eq!(std::fs::read(&aside).unwrap(), b"not json");
    }

    #[cfg(unix)]
    #[test]
    fn read_io_error_freezes() {
        // A directory at the read path makes std::fs::read fail with a non-NotFound error.
        let dir = tempfile::tempdir().unwrap();
        let subdir = dir.path().join("asdir");
        std::fs::create_dir(&subdir).unwrap();
        let r = load_or_recover::<Vec<u32>>(&subdir, 1_000, parse_json);
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
        let r = load_or_recover::<Vec<u32>>(&p, 4_242, parse_json);
        // restore perms before assertions so tempdir cleanup succeeds regardless
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            r.disk_write_frozen,
            "failed quarantine falls back to freeze"
        );
        assert!(p.exists(), "original left in place");
    }

    #[test]
    fn quarantine_same_timestamp_keeps_both_payloads() {
        // Two corruptions of the same path at the SAME now_ms must not clobber each
        // other's sidecar (stuck-clock robustness).
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("v.json");
        std::fs::write(&p, b"first-corrupt").unwrap();
        let _ = load_or_recover::<Vec<u32>>(&p, 100, parse_json);
        std::fs::write(&p, b"second-corrupt").unwrap();
        let _ = load_or_recover::<Vec<u32>>(&p, 100, parse_json);
        assert_eq!(
            std::fs::read(dir.path().join("v.json.corrupt-100")).unwrap(),
            b"first-corrupt",
            "first sidecar preserved"
        );
        assert_eq!(
            std::fs::read(dir.path().join("v.json.corrupt-101")).unwrap(),
            b"second-corrupt",
            "second sidecar got a collision-free name"
        );
    }

    #[test]
    fn quarantine_all_names_occupied_freezes_without_clobbering() {
        // Pathological: every candidate sidecar name in the probe window is occupied by
        // prior recovery evidence. Rather than overwrite one, the load freezes and leaves
        // the current corrupt file in place (ZEB-784: never discard recovery evidence).
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("v.json");
        std::fs::write(&p, b"corrupt-current").unwrap();
        for s in 0..=1000u64 {
            std::fs::write(dir.path().join(format!("v.json.corrupt-{s}")), b"evidence").unwrap();
        }
        let r = load_or_recover::<Vec<u32>>(&p, 0, parse_json);
        assert!(r.disk_write_frozen, "exhausted probe window freezes");
        assert_eq!(
            std::fs::read(&p).unwrap(),
            b"corrupt-current",
            "current corrupt file left in place, not renamed over a sidecar"
        );
        assert_eq!(
            std::fs::read(dir.path().join("v.json.corrupt-1000")).unwrap(),
            b"evidence",
            "prior evidence sidecar intact"
        );
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

    // ── Sealed variant (ZEB-986 PR-3) ────────────────────────────────────────
    mod sealed {
        use super::super::*;
        use crate::device_dataset_file::{
            test_cipher, write_image, DeviceCipher, SEALED_DEVICE_SCHEMA_V3,
        };

        fn parse_json(bytes: &[u8]) -> Result<Vec<u32>, String> {
            serde_json::from_slice(bytes).map_err(|e| e.to_string())
        }

        #[test]
        fn sealed_round_trip_value() {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("v.json");
            let cipher = test_cipher();
            write_image(&cipher, &p, "v.json", b"[1,2,3]").unwrap();
            assert_eq!(
                std::fs::read(&p).unwrap()[0],
                SEALED_DEVICE_SCHEMA_V3,
                "written sealed"
            );
            let r =
                load_sealed_or_recover::<Vec<u32>>(Some(&cipher), &p, "v.json", 1_000, parse_json);
            assert_eq!(r.value, vec![1, 2, 3]);
            assert!(!r.disk_write_frozen);
        }

        #[test]
        fn missing_is_default_unfrozen() {
            let dir = tempfile::tempdir().unwrap();
            let cipher = test_cipher();
            let r = load_sealed_or_recover::<Vec<u32>>(
                Some(&cipher),
                &dir.path().join("nope.json"),
                "nope.json",
                1_000,
                parse_json,
            );
            assert!(r.value.is_empty());
            assert!(!r.disk_write_frozen);
        }

        #[test]
        fn no_cipher_missing_file_defaults_unfrozen() {
            let dir = tempfile::tempdir().unwrap();
            let r = load_sealed_or_recover::<Vec<u32>>(
                None,
                &dir.path().join("nope.json"),
                "nope.json",
                1_000,
                parse_json,
            );
            assert!(r.value.is_empty());
            assert!(
                !r.disk_write_frozen,
                "absent file, no cipher: first-run empty"
            );
        }

        #[test]
        fn no_cipher_existing_file_freezes_and_preserves() {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("v.json");
            write_image(&test_cipher(), &p, "v.json", b"[1,2,3]").unwrap();
            let before = std::fs::read(&p).unwrap();
            let r = load_sealed_or_recover::<Vec<u32>>(None, &p, "v.json", 1_000, parse_json);
            assert!(r.value.is_empty());
            assert!(
                r.disk_write_frozen,
                "existing file, no cipher: freeze, never wipe"
            );
            assert_eq!(
                std::fs::read(&p).unwrap(),
                before,
                "file left byte-identical"
            );
        }

        #[test]
        fn foreign_cipher_freezes_no_quarantine() {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("v.json");
            write_image(&test_cipher(), &p, "v.json", b"[1,2,3]").unwrap();
            let foreign = DeviceCipher::derive(&[9u8; 32]).unwrap();
            let r =
                load_sealed_or_recover::<Vec<u32>>(Some(&foreign), &p, "v.json", 4_242, parse_json);
            assert!(r.value.is_empty());
            assert!(r.disk_write_frozen, "undecryptable sealed file freezes");
            assert!(p.exists(), "sealed file preserved (not quarantined)");
            assert!(
                !dir.path().join("v.json.corrupt-4242").exists(),
                "no quarantine sidecar for a sealed decrypt failure"
            );
            assert_eq!(
                std::fs::read(&p).unwrap()[0],
                SEALED_DEVICE_SCHEMA_V3,
                "sealed bytes left intact"
            );
        }

        #[cfg(unix)]
        #[test]
        fn io_error_freezes() {
            // A directory at the read path → read_image Io error (non-NotFound).
            let dir = tempfile::tempdir().unwrap();
            let sub = dir.path().join("asdir");
            std::fs::create_dir(&sub).unwrap();
            let cipher = test_cipher();
            let r =
                load_sealed_or_recover::<Vec<u32>>(Some(&cipher), &sub, "asdir", 1_000, parse_json);
            assert!(r.disk_write_frozen);
        }

        #[test]
        fn legacy_plaintext_corrupt_quarantines_and_heals() {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("v.json");
            // Legacy plaintext (first byte 'n' != sealed sentinel), unparseable.
            std::fs::write(&p, b"not json").unwrap();
            let cipher = test_cipher();
            let r =
                load_sealed_or_recover::<Vec<u32>>(Some(&cipher), &p, "v.json", 4_242, parse_json);
            assert!(r.value.is_empty());
            assert!(!r.disk_write_frozen);
            assert!(!p.exists(), "corrupt plaintext renamed aside");
            assert_eq!(
                std::fs::read(dir.path().join("v.json.corrupt-4242")).unwrap(),
                b"not json"
            );
        }

        #[test]
        fn sealed_unparseable_inner_freezes_not_quarantined() {
            // A valid seal over garbage inner bytes (was_legacy == false): decrypt succeeds
            // but parse fails. Must FREEZE and preserve in place — never quarantine (which
            // would let the next save replace an authenticated file).
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("v.json");
            let cipher = test_cipher();
            write_image(&cipher, &p, "v.json", b"not json at all").unwrap();
            let before = std::fs::read(&p).unwrap();
            let r =
                load_sealed_or_recover::<Vec<u32>>(Some(&cipher), &p, "v.json", 4_242, parse_json);
            assert!(r.value.is_empty());
            assert!(r.disk_write_frozen, "sealed-unparseable freezes");
            assert!(
                !dir.path().join("v.json.corrupt-4242").exists(),
                "sealed-unparseable is NOT quarantined"
            );
            assert_eq!(
                std::fs::read(&p).unwrap(),
                before,
                "sealed file left byte-identical"
            );
        }

        #[test]
        fn legacy_plaintext_valid_reseals_and_loads() {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("v.json");
            std::fs::write(&p, b"[7,8]").unwrap(); // valid legacy plaintext
            let cipher = test_cipher();
            let r =
                load_sealed_or_recover::<Vec<u32>>(Some(&cipher), &p, "v.json", 1_000, parse_json);
            assert_eq!(r.value, vec![7, 8]);
            assert!(!r.disk_write_frozen);
            assert_eq!(
                std::fs::read(&p).unwrap()[0],
                SEALED_DEVICE_SCHEMA_V3,
                "legacy plaintext migrated to sealed on load"
            );
            // Reloads cleanly through the sealed path.
            let r2 =
                load_sealed_or_recover::<Vec<u32>>(Some(&cipher), &p, "v.json", 1_000, parse_json);
            assert_eq!(r2.value, vec![7, 8]);
            assert!(!r2.disk_write_frozen);
        }
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
