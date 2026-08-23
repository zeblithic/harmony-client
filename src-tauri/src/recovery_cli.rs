//! CLI subcommand entry points for identity backup/restore.
//!
//! Two distinct root secrets pass through here — do not conflate them
//! (ZEB-430):
//!
//! - The **Reticulum identity seed** (`identity.enc` / identity keychain
//!   slot): the node's network keypair. `export mnemonic`,
//!   `export recovery-file`, `restore mnemonic`, and `restore recovery-file`
//!   operate on THIS seed via [`crate::identity::read_seed_from_disk`] /
//!   [`crate::identity::write_seed_to_disk`].
//! - The **owner master seed** (`master_seed.enc` / owner keychain slot):
//!   the root of friendships, communities, and device enrollments.
//!   `export owner-mnemonic` reads this seed via
//!   [`crate::owner_state::load_owner_state`]; `restore owner-mnemonic`
//!   re-adopts the owner identity from it via
//!   [`crate::owner_state::remint_owner_from_seed`] (ZEB-439).
//!
//! The recovery passphrase
//! (`HARMONY_RECOVERY_PASSPHRASE` / `HARMONY_RECOVERY_PASSPHRASE_FILE`) is
//! resolved separately from the at-rest passphrase
//! (`HARMONY_PASSPHRASE` / `HARMONY_PASSPHRASE_FILE`) — neither variable
//! falls back to the other.

use std::path::{Path, PathBuf};

use harmony_owner::lifecycle::RecoveryArtifact;
use harmony_owner::recovery::RecoveryMetadata;
use secrecy::SecretString;
use zeroize::Zeroizing;

use crate::backup_state::{save_last_backup, LastBackup};
use crate::identity::{self, KeychainStore};
use crate::owner_state_persist::load_crdt;
use crate::owner_state_types::{Hlc, OwnerAddr};
use crate::state_snapshot::{
    decode_snapshot, encode_snapshot, verify_snapshot_addr, OwnerStateSnapshot,
};

/// Resolve the recovery passphrase from `HARMONY_RECOVERY_PASSPHRASE` or
/// `HARMONY_RECOVERY_PASSPHRASE_FILE`. Hard-fails if neither is set, with
/// a pointer to docs/headless-install.md.
///
/// Mirrors `EncryptedFileStore::from_env` but for the disjoint recovery vars.
pub(crate) fn resolve_recovery_passphrase() -> Result<SecretString, String> {
    let direct = std::env::var("HARMONY_RECOVERY_PASSPHRASE").ok();
    let file_path = std::env::var("HARMONY_RECOVERY_PASSPHRASE_FILE").ok();

    if direct.is_some() && file_path.is_some() {
        tracing::warn!(
            "both HARMONY_RECOVERY_PASSPHRASE and HARMONY_RECOVERY_PASSPHRASE_FILE are set; \
             HARMONY_RECOVERY_PASSPHRASE takes precedence"
        );
    }

    let s = if let Some(s) = direct {
        if s.is_empty() {
            return Err("HARMONY_RECOVERY_PASSPHRASE is set to an empty string".to_string());
        }
        s
    } else if let Some(file_path) = file_path {
        // parse_passphrase_file rejects empty content directly — no asymmetry
        // with the direct-var branch.
        identity::parse_passphrase_file(Path::new(&file_path))
            .map_err(|e| format!("HARMONY_RECOVERY_PASSPHRASE_FILE={file_path} {e}"))?
    } else {
        return Err(
            "neither HARMONY_RECOVERY_PASSPHRASE nor HARMONY_RECOVERY_PASSPHRASE_FILE is set — see docs/headless-install.md"
                .to_string(),
        );
    };

    Ok(SecretString::from(s))
}

/// Export the RETICULUM IDENTITY seed as a 24-word BIP39 English mnemonic.
///
/// This is the node keypair — NOT the owner master seed; for the owner
/// identity (friends/communities) see [`export_owner_mnemonic_cli`].
///
/// Side effects:
///   - Reads the identity seed via the standard resolution chain
///     (keychain → encrypted file), MINTING a fresh identity on an empty
///     install.
///   - Writes the bare 24 words on a single line to stdout, terminated by `\n`.
///   - Writes a warning preamble + `identity-hash: <hex32>` to stderr.
///
/// Stdout/stderr separation is the load-bearing UX: `harmony-app export
/// mnemonic > backup.txt` writes only the words; running interactively shows
/// the warning + fingerprint on the terminal.
pub fn export_mnemonic_cli(identity_path: &Path) -> Result<(), String> {
    export_mnemonic_to_writers(
        identity_path,
        KeychainStore::new().ok(),
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    )
}

/// Inner entry point — accepts an injected keychain AND output writers so
/// tests can both stay hermetic AND assert the exact stdout/stderr contract.
/// Production callers go through [`export_mnemonic_cli`].
pub fn export_mnemonic_to_writers<W1: std::io::Write, W2: std::io::Write>(
    identity_path: &Path,
    keychain: Option<KeychainStore>,
    stdout: &mut W1,
    stderr: &mut W2,
) -> Result<(), String> {
    // Single source of truth for word derivation; identity_hash comes back
    // alongside the words so we avoid re-parsing the mnemonic.
    let (words, id_hash) = export_mnemonic_words_with_keychain(identity_path, keychain)?;
    let phrase = words.join(" ");

    let map_err = |stream: &'static str| move |e: std::io::Error| format!("{stream}: {e}");

    writeln!(stderr, "*** Reticulum identity mnemonic ***").map_err(map_err("stderr"))?;
    writeln!(
        stderr,
        "These 24 words back up your network (Reticulum) keypair"
    )
    .map_err(map_err("stderr"))?;
    writeln!(
        stderr,
        "ONLY - NOT your owner identity (friends, communities,"
    )
    .map_err(map_err("stderr"))?;
    writeln!(
        stderr,
        "device enrollments). For that, run: export owner-mnemonic"
    )
    .map_err(map_err("stderr"))?;
    writeln!(stderr, "Write them on paper. Anyone with these words can")
        .map_err(map_err("stderr"))?;
    writeln!(
        stderr,
        "impersonate your node; a digital copy is dangerous."
    )
    .map_err(map_err("stderr"))?;
    writeln!(stderr).map_err(map_err("stderr"))?;
    writeln!(stderr, "identity-hash: {}", hex::encode(id_hash)).map_err(map_err("stderr"))?;

    writeln!(stdout, "{phrase}").map_err(map_err("stdout"))?;
    Ok(())
}

/// Read the RETICULUM IDENTITY seed from disk and convert to 24 BIP39 words.
///
/// Returns `(words, identity_hash_bytes)` — the hash bytes are derived
/// directly from the artifact so callers (e.g. `export_mnemonic_to_writers`
/// and the GUI's `export_mnemonic_words` Tauri command) do not need to
/// re-parse the mnemonic just to obtain the fingerprint.
///
/// Used by the GUI wizard so the words never touch a temp file. The CLI's
/// `export_mnemonic_to_writers` delegates here.
pub fn export_mnemonic_words_with_keychain(
    identity_path: &Path,
    keychain: Option<KeychainStore>,
) -> Result<(Vec<String>, [u8; 16]), String> {
    let seed = identity::read_seed_from_disk_with_keychain(identity_path, keychain)?;
    let artifact = RecoveryArtifact::from_seed(*seed);
    let id_hash = artifact.master_pubkey_bundle().identity_hash();
    let mnemonic = artifact.to_mnemonic();
    let words = mnemonic
        .as_str()
        .split_whitespace()
        .map(String::from)
        .collect();
    Ok((words, id_hash))
}

/// Export the OWNER master seed as a 24-word BIP39 English mnemonic (ZEB-430).
///
/// Headless counterpart of the GUI Devices-panel backup
/// (`owner_commands::export_owner_recovery`). Reads the owner master seed
/// (OS-keychain master-seed slot / `master_seed.enc`) via
/// [`crate::owner_state::load_owner_state`] — NOT the Reticulum identity
/// seed that `export mnemonic` prints.
///
/// Stdout/stderr contract mirrors [`export_mnemonic_cli`]: bare 24 words on
/// a single line to stdout; warning preamble + `owner-id: <hex32>` to
/// stderr. The owner-id is the fingerprint the UI surfaces everywhere
/// (`OwnerState.owner_id`), so an operator can eyeball-match the backup
/// against their profile during incident response.
pub fn export_owner_mnemonic_cli(identity_path: &Path) -> Result<(), String> {
    let identity_dir = identity_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    export_owner_mnemonic_to_writers(
        &identity_dir,
        KeychainStore::new().ok(),
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    )
}

/// Inner entry point — accepts an injected keychain AND output writers so
/// tests can both stay hermetic AND assert the exact stdout/stderr contract.
/// Production callers go through [`export_owner_mnemonic_cli`].
pub fn export_owner_mnemonic_to_writers<W1: std::io::Write, W2: std::io::Write>(
    identity_dir: &Path,
    keychain: Option<KeychainStore>,
    stdout: &mut W1,
    stderr: &mut W2,
) -> Result<(), String> {
    let (words, owner_id) = export_owner_mnemonic_words_with_keychain(identity_dir, keychain)?;
    let phrase = words.join(" ");

    let map_err = |stream: &'static str| move |e: std::io::Error| format!("{stream}: {e}");

    writeln!(stderr, "*** Owner identity recovery mnemonic ***").map_err(map_err("stderr"))?;
    writeln!(
        stderr,
        "These 24 words are your OWNER master seed - the root of"
    )
    .map_err(map_err("stderr"))?;
    writeln!(
        stderr,
        "your friendships, communities, and device enrollments."
    )
    .map_err(map_err("stderr"))?;
    writeln!(stderr, "Write them on paper. Anyone with these words can")
        .map_err(map_err("stderr"))?;
    writeln!(stderr, "impersonate you; a digital copy is dangerous.").map_err(map_err("stderr"))?;
    writeln!(stderr).map_err(map_err("stderr"))?;
    writeln!(stderr, "owner-id: {}", hex::encode(owner_id)).map_err(map_err("stderr"))?;

    writeln!(stdout, "{phrase}").map_err(map_err("stdout"))?;
    Ok(())
}

/// Read the owner master seed and convert to 24 BIP39 words.
///
/// Returns `(words, owner_id)`. Hard-fails when no owner identity has been
/// minted, when the master seed has been wiped (joiner / cert-only model),
/// or when the seed on disk no longer derives the `owner_id` recorded in
/// `owner_state.cbor` — a mismatched export would be a paper backup that
/// silently cannot restore this identity (same invariant
/// `pairing/cert.rs::sign_enrollment_for_joiner` enforces before signing).
pub fn export_owner_mnemonic_words_with_keychain(
    identity_dir: &Path,
    keychain: Option<KeychainStore>,
) -> Result<(Vec<String>, [u8; 16]), String> {
    let loaded = crate::owner_state::load_owner_state(identity_dir, keychain)?
        .ok_or_else(|| "Owner identity has not been minted on this device.".to_string())?;
    let seed = loaded.master_seed.ok_or_else(|| {
        "Master seed has been wiped from this device — backup is no longer possible.".to_string()
    })?;
    let artifact = RecoveryArtifact::from_seed(*seed);
    let owner_id = artifact.master_pubkey_bundle().identity_hash();
    if owner_id != loaded.state.owner_id {
        return Err(format!(
            "master seed / owner-state mismatch: seed derives owner-id {} but owner_state.cbor \
             records {} — refusing to export a backup that could not restore this identity",
            hex::encode(owner_id),
            hex::encode(loaded.state.owner_id),
        ));
    }
    let mnemonic = artifact.to_mnemonic();
    let words = mnemonic
        .as_str()
        .split_whitespace()
        .map(String::from)
        .collect();
    Ok((words, owner_id))
}

/// Export the RETICULUM IDENTITY seed as a passphrase-encrypted recovery
/// file at `out`.
///
/// Reads the identity seed via the standard resolution chain. The recovery passphrase
/// is read from `HARMONY_RECOVERY_PASSPHRASE` / `HARMONY_RECOVERY_PASSPHRASE_FILE`
/// (DISTINCT from the at-rest `HARMONY_PASSPHRASE`).
///
/// Stdout: nothing. Stderr: `wrote <PATH> (<NN> bytes)\nidentity-hash: <hex32>`.
pub fn export_recovery_file_cli(
    identity_path: &Path,
    out: &Path,
    comment: Option<&str>,
    include_state: bool,
    force: bool,
) -> Result<(), String> {
    let harmony_dir = identity_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let result = export_recovery_file_pair_with_keychain(
        identity_path,
        &harmony_dir,
        out,
        comment,
        None,
        include_state,
        force,
        KeychainStore::new().ok(),
    )?;
    eprintln!(
        "wrote {} ({} bytes)",
        result.hrmr_path.display(),
        std::fs::metadata(&result.hrmr_path)
            .map(|m| m.len())
            .unwrap_or(0)
    );
    if let Some(p) = result.hrss_path {
        eprintln!(
            "wrote {} ({} bytes)",
            p.display(),
            result.snapshot_bytes_written
        );
    } else if include_state {
        eprintln!("no owner-state to bundle; emitted identity-only backup");
    }
    eprintln!("identity-hash: {}", hex::encode(result.identity_hash));
    Ok(())
}

/// Inner entry point — accepts an injected keychain so tests can stay
/// hermetic. Production callers go through [`export_recovery_file_cli`].
///
/// `passphrase`: when `Some`, used directly. When `None`, resolved from
/// `HARMONY_RECOVERY_PASSPHRASE` / `HARMONY_RECOVERY_PASSPHRASE_FILE`
/// environment variables (the CLI path). The GUI passes `Some` — it would
/// otherwise have to mutate process-global env vars, which is unsafe in a
/// multithreaded program (CodeRabbit round 5). The env-var fallback is
/// kept for the headless CLI binary, which runs single-threaded and has
/// no other way to receive the secret.
pub fn export_recovery_file_with_keychain(
    identity_path: &Path,
    out: &Path,
    comment: Option<&str>,
    passphrase: Option<&SecretString>,
    keychain: Option<KeychainStore>,
) -> Result<(), String> {
    // Resolve the recovery passphrase BEFORE reading the seed: on an empty
    // install, read_seed_from_disk_with_keychain mints + persists a fresh
    // identity as a side effect. If we resolved the passphrase after the
    // read, a missing/invalid HARMONY_RECOVERY_PASSPHRASE would mutate the
    // identity store and then fail — the operator wanted to back up an
    // existing identity, not silently create one.
    let passphrase: SecretString = match passphrase {
        Some(p) => p.clone(),
        None => resolve_recovery_passphrase()?,
    };
    let seed = identity::read_seed_from_disk_with_keychain(identity_path, keychain)?;
    let artifact = RecoveryArtifact::from_seed(*seed);
    let metadata = RecoveryMetadata {
        // ZEB-180: stamp the export time so restore can surface it (spot a
        // stale backup vs. the live identity).
        mint_at: Some(mint_timestamp_secs()),
        comment: comment.map(str::to_string),
    };
    let bytes = artifact
        .to_encrypted_file(&passphrase, &metadata)
        .map_err(|e| e.to_string())?;
    let id_hash = artifact.master_pubkey_bundle().identity_hash();

    crate::identity::write_atomic_0600(out, &bytes)
        .map_err(|e| format!("failed to write {}: {e}", out.display()))?;

    eprintln!("wrote {} ({} bytes)", out.display(), bytes.len());
    eprintln!("identity-hash: {}", hex::encode(id_hash));
    Ok(())
}

/// Owner-state CRDT directory + filename convention.
///
/// Production wires this to `~/.harmony/owner_state_crdt.cbor`. Tests
/// pass a tempdir-rooted path. This is the SAME file the production
/// engine reads at boot (`lib.rs:1449`'s `crdt_path`).
///
/// NOT to be confused with `~/.harmony/owner_state_crdt.cbor` (owned by
/// `owner_state.rs`) which stores per-owner pairing/identity state
/// — a different file entirely.
pub fn owner_state_path(harmony_dir: &Path) -> PathBuf {
    harmony_dir.join("owner_state_crdt.cbor")
}

pub fn last_backup_path(harmony_dir: &Path) -> PathBuf {
    harmony_dir.join("last_backup.json")
}

/// Wall-clock seconds since the Unix epoch, stamped into a recovery file's
/// `mint_at` at export time so a later restore can surface *when* the backup
/// was made (ZEB-180). Uses the same `SystemTime::now()` idiom as the
/// owner-state export path below; saturates to 0 if the clock predates the
/// epoch (unreachable in practice — keeps stamping infallible).
///
/// `pub(crate)` so the GUI export command (`owner_commands::
/// export_owner_recovery_file_to_path`) stamps identically to the two CLI
/// exporters — one source of truth for the timestamp.
pub(crate) fn mint_timestamp_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format a recovery file's `mint_at` (Unix seconds) as an RFC 3339 UTC
/// timestamp for operator display on restore (ZEB-180). Falls back to raw
/// seconds if the value is outside chrono's representable range (unreachable
/// for real timestamps).
fn format_mint_at(secs: u64) -> String {
    // `i64::try_from` before chrono: a raw `secs as i64` would wrap values above
    // `i64::MAX` into negative (pre-1970) timestamps that chrono formats as a
    // plausible date, defeating the raw-seconds fallback the doc promises.
    i64::try_from(secs)
        .ok()
        .and_then(|s| chrono::DateTime::<chrono::Utc>::from_timestamp(s, 0))
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| format!("{secs} (unix seconds)"))
}

/// Compose the sidecar HRSS path next to `out`. Matches the spec
/// convention `<HRMR_PATH>.state`.
pub fn sidecar_path(out: &Path) -> PathBuf {
    let mut s = out.as_os_str().to_owned();
    s.push(".state");
    PathBuf::from(s)
}

/// Export the RETICULUM IDENTITY seed + (optionally) an owner-state sidecar.
///
/// `include_state == true` AND owner-state file exists ⇒ emit pair.
/// `include_state == false` OR owner-state file absent ⇒ emit HRMR only.
/// Refuses if the sidecar destination exists and `force == false`.
/// On HRSS-write failure, best-effort removes the just-written HRMR
/// so the operator isn't stranded with a mismatched half-pair.
///
/// **Overwrite-safe rollback** (round-1 bot finding C5 — Qodo High):
/// when `force == true` AND the destination (HRMR `out` and/or its
/// sidecar) already holds a valid backup, the function renames the
/// pre-existing files to `<path>.<random>.bak` BEFORE overwriting. On
/// failure of the new write, the .bak file is renamed back so the
/// operator keeps the old backup. On success, .bak files are removed.
/// Without this dance, `write_atomic_0600`'s tempfile-then-rename
/// silently replaces the old file and `remove_file(out)` on rollback
/// destroys the previous backup — leaving the user with NEITHER the
/// new nor the old backup. The `force == false` path is unaffected
/// (`write_atomic_0600` would refuse on existing file via the upstream
/// refusal at line ~283).
///
/// When `force == false`, no pre-existing file can be overwritten
/// (refusal already returned upstream), so the dance is skipped.
#[allow(clippy::too_many_arguments)]
pub fn export_recovery_file_pair_with_keychain(
    identity_path: &Path,
    harmony_dir: &Path,
    out: &Path,
    comment: Option<&str>,
    passphrase: Option<&SecretString>,
    include_state: bool,
    force: bool,
    keychain: Option<KeychainStore>,
) -> Result<ExportResult, String> {
    // Resolve passphrase first — same atomic-rollback rationale as the
    // existing export_recovery_file_with_keychain.
    let passphrase: SecretString = match passphrase {
        Some(p) => p.clone(),
        None => resolve_recovery_passphrase()?,
    };

    let state_path = owner_state_path(harmony_dir);
    let state_exists = state_path.exists();
    let want_sidecar = include_state && state_exists;

    let sidecar = sidecar_path(out);
    if want_sidecar && sidecar.exists() && !force {
        return Err(format!(
            "state sidecar already exists at {}; pass --force to overwrite",
            sidecar.display()
        ));
    }

    // C5: If `force == true` and the destination(s) already exist, move
    // them aside to `.bak` files. On any subsequent write failure we
    // restore them; on success we delete them. The `force == false`
    // case already refused above (sidecar) or would refuse inside
    // `write_atomic_0600` (HRMR — `create_new` on the tempfile prevents
    // ambiguity but the file at `out` would survive the rename failure).
    //
    // Random suffix mirrors the tempfile idiom in identity::write_atomic_0600
    // so two concurrent exports can't collide on a single `.bak` name.
    fn move_aside(p: &Path) -> Result<Option<PathBuf>, String> {
        if !p.exists() {
            return Ok(None);
        }
        let suffix = format!(".{:016x}.bak", rand::random::<u64>());
        let mut bak = p.as_os_str().to_owned();
        bak.push(&suffix);
        let bak = PathBuf::from(bak);
        // R2-1: a rename failure here means we'd lose the rollback
        // safety net for this file. Subsequent failures on the
        // HRMR/HRSS write would then leave the operator stranded with
        // a destroyed pre-existing artifact and no .bak to restore.
        // Abort before any irreversible write.
        match std::fs::rename(p, &bak) {
            Ok(()) => Ok(Some(bak)),
            Err(e) => Err(format!(
                "failed to back up existing {} before overwrite: {e}",
                p.display()
            )),
        }
    }

    let hrmr_bak: Option<PathBuf> = if force { move_aside(out)? } else { None };
    let hrss_bak: Option<PathBuf> = if force && want_sidecar {
        // ZEB-728: if the sidecar move-aside fails *after* the HRMR move-aside
        // already succeeded, roll back the HRMR backup before returning.
        // Otherwise this early `?` bypasses every `restore_bak` error path
        // below and orphans the operator's original recovery file under a
        // randomized `.bak` name — turning a failed export into backup loss.
        match move_aside(&sidecar) {
            Ok(bak) => bak,
            Err(e) => {
                // Roll back the HRMR move-aside. If the rollback ITSELF fails,
                // the operator's original recovery file is stranded under its
                // `.bak` — surface that path in the returned error rather than
                // reducing it to a tracing log, since silent HRMR loss is
                // exactly what ZEB-728 guards against.
                if let Some(b) = hrmr_bak.as_deref() {
                    if !restore_bak(out, b) {
                        return Err(format!(
                            "{e}; additionally FAILED to restore the original \
                             recovery file, which remains at {}",
                            b.display()
                        ));
                    }
                }
                return Err(e);
            }
        }
    } else {
        None
    };

    // Helper to restore a .bak file back to its original path. Used in
    // every error path below. Returns true if restoration succeeded.
    fn restore_bak(original: &Path, bak: &Path) -> bool {
        if let Err(e) = std::fs::rename(bak, original) {
            tracing::error!(
                "failed to restore {} from {}: {e}",
                original.display(),
                bak.display()
            );
            return false;
        }
        true
    }

    // 1. Read seed + write HRMR.
    let seed = match identity::read_seed_from_disk_with_keychain(identity_path, keychain) {
        Ok(s) => s,
        Err(e) => {
            // Pre-write failure — restore both backups (we haven't written
            // anything yet, so this is pure cleanup).
            if let Some(b) = hrmr_bak.as_deref() {
                restore_bak(out, b);
            }
            if let Some(b) = hrss_bak.as_deref() {
                restore_bak(&sidecar, b);
            }
            return Err(e);
        }
    };
    let artifact = RecoveryArtifact::from_seed(*seed);
    let metadata = RecoveryMetadata {
        // ZEB-180: stamp the export time so restore can surface it (spot a
        // stale backup vs. the live identity).
        mint_at: Some(mint_timestamp_secs()),
        comment: comment.map(str::to_string),
    };
    let bytes = match artifact.to_encrypted_file(&passphrase, &metadata) {
        Ok(b) => b,
        Err(e) => {
            if let Some(b) = hrmr_bak.as_deref() {
                restore_bak(out, b);
            }
            if let Some(b) = hrss_bak.as_deref() {
                restore_bak(&sidecar, b);
            }
            return Err(e.to_string());
        }
    };
    let id_hash = artifact.master_pubkey_bundle().identity_hash();
    if let Err(e) = crate::identity::write_atomic_0600(out, &bytes) {
        if let Some(b) = hrmr_bak.as_deref() {
            restore_bak(out, b);
        }
        if let Some(b) = hrss_bak.as_deref() {
            restore_bak(&sidecar, b);
        }
        return Err(format!("failed to write {}: {e}", out.display()));
    }

    // 2. If no sidecar wanted, we're done. Delete the HRMR backup
    // (overwrite succeeded). No HRSS backup exists on this branch.
    if !want_sidecar {
        // R2-2: clean up any stale `.state` sidecar BEFORE recording
        // last_backup.json — failure here must hard-fail the export
        // and roll back the just-written HRMR. Otherwise we'd return
        // success with a stale HRSS bound to a previous identity still
        // on disk; the next restore would auto-detect it and either
        // hard-fail with addr-mismatch or (worse) silently succeed
        // against a stale tree.
        //
        // M1: identity-only export must clean up any stale `.state` sidecar
        // at the same path. See bot finding M1.
        if sidecar.exists() {
            if let Err(e) = std::fs::remove_file(&sidecar) {
                let _ = std::fs::remove_file(out);
                if let Some(b) = hrmr_bak.as_deref() {
                    restore_bak(out, b);
                }
                return Err(format!(
                    "failed to remove stale state sidecar at {}: {e}",
                    sidecar.display()
                ));
            }
        }
        if let Some(b) = hrmr_bak.as_deref() {
            let _ = std::fs::remove_file(b);
        }
        let last = LastBackup {
            at: now_hlc(),
            include_state: false,
            out_path: out.display().to_string(),
        };
        if let Err(e) = save_last_backup(&last_backup_path(harmony_dir), &last) {
            tracing::warn!("failed to persist last_backup.json: {e}");
        }
        return Ok(ExportResult {
            hrmr_path: out.to_path_buf(),
            hrss_path: None,
            identity_hash: id_hash,
            snapshot_bytes_written: 0,
        });
    }

    // 3. Build snapshot + write HRSS.
    // Mirror the encode/write rollback paths: a load_crdt failure here
    // (e.g. a TOCTOU race where the state file is removed between the
    // exists() check above and this read) must roll back the just-written
    // HRMR so the operator isn't stranded with a mismatched half-pair.
    // C5: also restore .bak files for both HRMR and HRSS.
    let state = match crate::device_dataset_file::get_or_derive(harmony_dir)
        .map_err(|e| e.to_string())
        .and_then(|cipher| load_crdt(&cipher, &state_path).map_err(|e| e.to_string()))
    {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_file(out);
            if let Some(b) = hrmr_bak.as_deref() {
                restore_bak(out, b);
            }
            if let Some(b) = hrss_bak.as_deref() {
                restore_bak(&sidecar, b);
            }
            return Err(format!(
                "failed to load owner-state from {} (HRMR rolled back): {e}",
                state_path.display()
            ));
        }
    };
    use secrecy::ExposeSecret;
    let addr = derive_owner_addr_from_seed(&seed);
    let at = now_hlc();
    let hrss_bytes = match encode_snapshot(
        passphrase.expose_secret().as_bytes(),
        addr,
        at.clone(),
        &state,
    ) {
        Ok(b) => b,
        Err(e) => {
            let _ = std::fs::remove_file(out);
            if let Some(b) = hrmr_bak.as_deref() {
                restore_bak(out, b);
            }
            if let Some(b) = hrss_bak.as_deref() {
                restore_bak(&sidecar, b);
            }
            return Err(format!(
                "failed to encode state sidecar: {e} (HRMR rolled back)"
            ));
        }
    };

    if let Err(e) = crate::identity::write_atomic_0600(&sidecar, &hrss_bytes) {
        let _ = std::fs::remove_file(out);
        if let Some(b) = hrmr_bak.as_deref() {
            restore_bak(out, b);
        }
        if let Some(b) = hrss_bak.as_deref() {
            restore_bak(&sidecar, b);
        }
        return Err(format!(
            "failed to write {}: {e} (HRMR rolled back)",
            sidecar.display()
        ));
    }

    // Both writes succeeded. Delete the backups; they served their purpose.
    if let Some(b) = hrmr_bak.as_deref() {
        let _ = std::fs::remove_file(b);
    }
    if let Some(b) = hrss_bak.as_deref() {
        let _ = std::fs::remove_file(b);
    }

    let last = LastBackup {
        at,
        include_state: true,
        out_path: out.display().to_string(),
    };
    if let Err(e) = save_last_backup(&last_backup_path(harmony_dir), &last) {
        tracing::warn!("failed to persist last_backup.json: {e}");
    }

    Ok(ExportResult {
        hrmr_path: out.to_path_buf(),
        hrss_path: Some(sidecar.clone()),
        identity_hash: id_hash,
        snapshot_bytes_written: hrss_bytes.len(),
    })
}

#[derive(Debug, Clone)]
pub struct ExportResult {
    pub hrmr_path: PathBuf,
    pub hrss_path: Option<PathBuf>,
    pub identity_hash: [u8; 16],
    pub snapshot_bytes_written: usize,
}

/// Restore identity + (optionally) owner-state sidecar.
///
/// `ignore_state == true` skips sidecar lookup. Otherwise auto-detects
/// `<in_path>.state`. addr-binding hard-fails on mismatch. Unknown
/// snapshot version hard-fails. Wrong passphrase fails with the same
/// idiom as HRMR.
///
/// `passphrase == Some(p)` overrides env-var resolution — the Tauri GUI
/// path passes the user-typed passphrase directly (env vars are unsafe
/// to set from a multithreaded process). Mirrors the
/// [`export_recovery_file_pair_with_keychain`] override channel.
#[allow(clippy::too_many_arguments)]
pub fn restore_recovery_file_pair_with_keychain(
    identity_path: &Path,
    harmony_dir: &Path,
    in_path: &Path,
    passphrase: Option<&SecretString>,
    force: bool,
    ignore_state: bool,
    keychain: Option<KeychainStore>,
) -> Result<RestoreResult, String> {
    // Restore identity first — same as today's path.
    let bytes =
        std::fs::read(in_path).map_err(|e| format!("failed to read {}: {e}", in_path.display()))?;
    let passphrase: SecretString = match passphrase {
        Some(p) => p.clone(),
        None => resolve_recovery_passphrase()?,
    };
    let restored =
        RecoveryArtifact::from_encrypted_file(&bytes, &passphrase).map_err(|e| e.to_string())?;
    // ZEB-180: capture the recovery-file metadata before into_artifact()
    // discards it, so we can surface it to the operator (stderr + the returned
    // RestoreResult). mint_at is None for files exported before stamping
    // landed; comment is None unless --comment was passed at export.
    let mint_at = restored.metadata.mint_at;
    let comment = restored.metadata.comment.clone();
    let artifact = restored.into_artifact();
    let seed_bytes: Zeroizing<[u8; 32]> = Zeroizing::new(*artifact.as_bytes());
    let id_hash = artifact.master_pubkey_bundle().identity_hash();
    let addr_bytes = derive_owner_addr_from_seed(&seed_bytes);

    // BEFORE writing identity, peek at the sidecar to fail-fast on
    // addr-binding mismatch (metadata before irreversible write).
    let sidecar = sidecar_path(in_path);
    let want_sidecar = !ignore_state && sidecar.exists();
    let snapshot: Option<OwnerStateSnapshot> = if want_sidecar {
        use secrecy::ExposeSecret;
        let s_bytes = std::fs::read(&sidecar)
            .map_err(|e| format!("failed to read {}: {e}", sidecar.display()))?;
        let snap = decode_snapshot(passphrase.expose_secret().as_bytes(), &s_bytes)
            .map_err(|e| format!("state sidecar: {e}"))?;
        verify_snapshot_addr(&snap, &addr_bytes.0).map_err(|e| format!("state sidecar: {e}"))?;
        Some(snap)
    } else {
        None
    };

    // Pre-flight: if a sidecar is being restored AND owner_state_crdt.cbor exists,
    // refuse here (before any irreversible write) per
    // metadata-before-irreversible-write rule. The check is pure metadata —
    // it only consults the filesystem and the function arguments — so it can
    // run before the seed write.
    let state_path = owner_state_path(harmony_dir);
    if snapshot.is_some() && state_path.exists() && !force {
        return Err(format!(
            "owner-state file already exists at {}; pass --force to overwrite",
            state_path.display()
        ));
    }

    // Stash the snapshot's full HLC before the snapshot is consumed by the
    // save_atomically call below. The stderr report's wall_ms sources from
    // this value, NOT from last_backup.json — on a fresh-machine restore
    // (the canonical happy path) last_backup.json is absent and would
    // yield 0. The full HLC (logical + device_id) is preserved in
    // RestoreResult for future GUI callers (Task 9) that may want to
    // surface "exported from device X".
    let snap_at: Option<Hlc> = snapshot.as_ref().map(|s| s.at.clone());

    // Now safe to write identity.
    identity::write_seed_to_disk_with_keychain(identity_path, &seed_bytes, force, keychain)?;

    // Then write owner-state if present.
    let spaces_restored = if let Some(snap) = snapshot {
        // Reconstruct OwnerState from the tree bytes and persist.
        // canonicalize() returns [schema_v2, ...cbor]; load_crdt parses
        // that same shape — so we route through a tempfile of the
        // exact bytes rather than re-deserialize the tree via ciborium.
        //
        // Per the metadata-before-irreversible-write rule, identity has
        // ALREADY been written by this point. A save failure here is a
        // genuine error, but the message must make the partial-success
        // state explicit so the operator can recover (their identity is
        // restored; only the state sidecar didn't land).
        // ZEB-982: seal under the cipher derived from the seed written just
        // above — NOT get_or_derive, whose memo the seed write invalidated;
        // deriving directly from the in-hand seed is race-free and lock-free.
        let cipher = crate::device_dataset_file::DeviceCipher::derive(&seed_bytes)
            .map_err(|e| format!("identity restored but owner-state write failed: {e}"))?;
        if let Err(e) =
            crate::device_dataset_file::write_image(&cipher, &state_path, crate::owner_state_persist::CRDT_FILENAME, &snap.tree)
        {
            return Err(format!(
                "identity restored but owner-state write failed: {e}"
            ));
        }
        // Best-effort reload to count Spaces for the confirmation message.
        // A reload failure here MUST NOT convert a successful state save
        // into surfaced Err (per metadata-before-irreversible-write); we
        // degrade gracefully to "0 spaces" in the stderr report.
        load_crdt(&cipher, &state_path)
            .ok()
            .map(|s| s.spaces.len())
            .unwrap_or(0)
    } else {
        0
    };

    // ZEB-180: surface the recovery-file metadata so the operator can confirm
    // WHICH backup this is (comment) and WHEN it was minted (spot a stale
    // backup vs. the live identity), alongside the existing identity-hash line.
    if let Some(secs) = mint_at {
        eprintln!("recovery-file minted-at: {}", format_mint_at(secs));
    }
    if let Some(ref c) = comment {
        // The comment is decrypted from the (portable, possibly shared) recovery
        // file, so escape it before writing to the terminal — a crafted comment
        // could otherwise inject ANSI/OSC/CR sequences into the operator's
        // console. `escape_default` renders control chars visibly.
        eprintln!("recovery-file comment: {}", c.escape_default());
    }
    eprintln!("restored identity-hash: {}", hex::encode(id_hash));
    if let Some(ref hlc) = snap_at {
        eprintln!(
            "owner-state snapshot: {spaces_restored} spaces, exported {} ms wall-clock",
            hlc.wall_ms
        );
    } else if sidecar.exists() && ignore_state {
        eprintln!("state sidecar found but ignored per flag");
    } else if !sidecar.exists() && !ignore_state {
        eprintln!(
            "no state sidecar found at {}; nav tree will be empty post-restore",
            sidecar.display()
        );
    }

    Ok(RestoreResult {
        identity_hash: id_hash,
        spaces_restored,
        sidecar_present: want_sidecar,
        snapshot_at: snap_at,
        mint_at,
        comment,
    })
}

#[derive(Debug, Clone)]
pub struct RestoreResult {
    pub identity_hash: [u8; 16],
    pub spaces_restored: usize,
    pub sidecar_present: bool,
    /// Full export-time HLC from the snapshot (when a sidecar was
    /// restored). Preserves `logical` + `device_id` in addition to
    /// `wall_ms` so callers (Task 9 GUI) can surface "exported from
    /// device X at time T". CLI stderr surfaces only `wall_ms`.
    pub snapshot_at: Option<Hlc>,
    /// ZEB-180: recovery-file metadata parsed from the HRMR envelope.
    /// `mint_at` is Unix seconds stamped at export (None for files exported
    /// before stamping landed); `comment` is the operator's `--comment` at
    /// export time (None if omitted). Surfaced on restore for a GUI caller
    /// (the eventual Settings → Identity wizard) and asserted by tests; the
    /// CLI additionally prints both to stderr.
    pub mint_at: Option<u64>,
    pub comment: Option<String>,
}

/// Derive the 16-byte owner address from a 32-byte seed.
/// Mirrors `lib.rs`'s `OwnerAddr(ed25519.public_identity().address_hash)`
/// pattern.
pub fn derive_owner_addr_from_seed(seed: &[u8; 32]) -> OwnerAddr {
    let ed = harmony_identity::PrivateIdentity::from_seed(seed);
    OwnerAddr(ed.public_identity().address_hash)
}

/// Current HLC suitable for export-time `at`. Uses system wall-clock;
/// `logical = 0`; `device_id = "harmony-app"` (the CLI is single-device
/// per invocation).
pub fn now_hlc() -> Hlc {
    let wall_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Hlc {
        wall_ms,
        logical: 0,
        device_id: "harmony-app".into(),
    }
}

/// Restore the RETICULUM IDENTITY seed from a 24-word mnemonic file.
///
/// Reads the mnemonic from `mnemonic_file` (whitespace-tolerant,
/// case-insensitive, ASCII-only — non-ASCII rejected). Writes the seed via
/// the standard resolution chain. Refuses if an identity already exists
/// unless `force` is true.
///
/// Stdout: nothing. Stderr: `restored identity-hash: <hex32>`.
pub fn restore_mnemonic_cli(
    identity_path: &Path,
    mnemonic_file: &Path,
    force: bool,
) -> Result<(), String> {
    restore_mnemonic_with_keychain(
        identity_path,
        mnemonic_file,
        force,
        KeychainStore::new().ok(),
    )
}

/// Restore the on-disk identity from a 24-word array. Refuses to
/// overwrite an existing identity unless `force` is true. The CLI's
/// `restore_mnemonic_with_keychain` (which reads from a file path)
/// delegates here after reading the file.
pub fn restore_mnemonic_from_words_with_keychain(
    identity_path: &Path,
    words: &[String],
    force: bool,
    keychain: Option<KeychainStore>,
) -> Result<(), String> {
    if words.len() != 24 {
        return Err(format!("expected 24 BIP39 words, got {}", words.len()));
    }
    let phrase = words.join(" ");
    let artifact = RecoveryArtifact::from_mnemonic(&phrase).map_err(|e| e.to_string())?;
    let seed_bytes: Zeroizing<[u8; 32]> = Zeroizing::new(*artifact.as_bytes());
    identity::write_seed_to_disk_with_keychain(identity_path, &seed_bytes, force, keychain)
        .map_err(|e| e.to_string())
}

/// Inner entry point — accepts an injected keychain so tests can stay
/// hermetic. Production callers go through [`restore_mnemonic_cli`].
pub fn restore_mnemonic_with_keychain(
    identity_path: &Path,
    mnemonic_file: &Path,
    force: bool,
    keychain: Option<KeychainStore>,
) -> Result<(), String> {
    // Read the mnemonic file. Wrap in Zeroizing so the contents do not linger.
    let raw = std::fs::read_to_string(mnemonic_file)
        .map_err(|e| format!("failed to read {}: {e}", mnemonic_file.display()))?;
    let raw = Zeroizing::new(raw);

    let words: Vec<String> = raw.split_whitespace().map(String::from).collect();
    restore_mnemonic_from_words_with_keychain(identity_path, &words, force, keychain)?;
    // Derive the identity-hash for the confirmation message. We re-parse the
    // SAME normalized input the inner used (`words.join(" ")`), not the raw
    // file text — otherwise tabs or multiple spaces between words would let
    // the inner parse succeed (after `split_whitespace` normalization) while
    // the outer `from_mnemonic(raw.trim())` could fail and panic, regressing
    // a graceful error into a panic AFTER the irreversible disk write.
    let phrase = words.join(" ");
    let artifact = RecoveryArtifact::from_mnemonic(&phrase)
        .expect("post-restore mnemonic re-parse must succeed; same normalized input already parsed in restore_mnemonic_from_words_with_keychain");
    eprintln!(
        "restored identity-hash: {}",
        hex::encode(artifact.master_pubkey_bundle().identity_hash())
    );
    Ok(())
}

/// Restore the OWNER master seed from a 24-word mnemonic file (ZEB-439).
///
/// Re-adopts the owner identity encoded by the mnemonic: reconstructs
/// `owner_state.cbor` with the SAME `owner_id`, a fresh device key, and a
/// master-signed enrollment (see [`crate::owner_state::remint_owner_from_seed`]).
/// Distinct from [`restore_mnemonic_cli`], which restores the *Reticulum*
/// identity seed.
///
/// Refuses to overwrite an existing owner identity unless `force` is true, and
/// refuses even with `force` if the mnemonic derives a different `owner_id`
/// than the one already on this device (overwriting a *different* identity is
/// almost always a mistake).
///
/// Stdout: nothing. Stderr: `restored owner-id: <hex32>`.
pub fn restore_owner_mnemonic_cli(
    identity_path: &Path,
    mnemonic_file: &Path,
    force: bool,
) -> Result<(), String> {
    let identity_dir = identity_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    restore_owner_mnemonic_with_keychain(
        &identity_dir,
        mnemonic_file,
        force,
        KeychainStore::new().ok(),
    )
}

/// Derive the owner-id (hex32) a 24-word mnemonic would restore, WITHOUT
/// writing anything to disk (ZEB-454). The GUI restore wizard calls this to
/// show the owner-id for confirmation before the irreversible re-mint, and to
/// compare against the device's current owner-id. Pure derivation: no keychain,
/// no `owner_state.cbor` read.
pub fn preview_owner_mnemonic_owner_id(words: &[String]) -> Result<String, String> {
    if words.len() != 24 {
        return Err(format!("expected 24 BIP39 words, got {}", words.len()));
    }
    let phrase = Zeroizing::new(words.join(" "));
    let artifact = RecoveryArtifact::from_mnemonic(&phrase).map_err(|e| e.to_string())?;
    Ok(hex::encode(artifact.master_pubkey_bundle().identity_hash()))
}

/// The owner-overwrite guard, shared between the read-only preflight and the
/// authoritative under-the-lock restore. Reads the persisted `owner_id` from
/// `owner_state.cbor` (no keychain) and decides whether `derived_owner_id` may
/// be written: an empty install is fine; an existing owner requires `force`;
/// a *different* owner-id is refused even with `force`; an unreadable marker
/// blocks unless `force`. Never mutates anything.
fn owner_mnemonic_overwrite_guard(
    identity_dir: &Path,
    derived_owner_id: &[u8; 16],
    force: bool,
) -> Result<(), String> {
    match crate::owner_state::read_persisted_owner_id(identity_dir) {
        Ok(None) => Ok(()),
        Ok(Some(existing)) => {
            if !force {
                return Err(format!(
                    "an owner identity ({}) already exists on this device; pass --force to overwrite it",
                    hex::encode(existing)
                ));
            }
            if existing != *derived_owner_id {
                return Err(format!(
                    "this mnemonic derives owner-id {} but this device already holds owner-id {} — \
                     refusing to overwrite a different identity even with --force",
                    hex::encode(derived_owner_id),
                    hex::encode(existing),
                ));
            }
            Ok(())
        }
        Err(e) => {
            // `owner_state.cbor` exists but is unreadable/corrupt — can't compare
            // owner-ids. Without --force, surface an actionable error rather than
            // wedging recovery. With --force the operator has explicitly opted
            // into a destructive overwrite, so proceed (the mismatch check is
            // necessarily skipped). Mirrors the identity-seed restore.
            if !force {
                return Err(format!(
                    "an owner_state.cbor marker exists on this device but could not be read \
                     ({e}); pass --force to overwrite it with this mnemonic"
                ));
            }
            tracing::warn!(
                error = %e,
                "restore owner-mnemonic --force: overwriting an unreadable owner_state.cbor \
                 (owner-id mismatch check skipped)"
            );
            Ok(())
        }
    }
}

/// Read-only preflight for the GUI restore (ZEB-454): validate the 24 words and
/// run the overwrite guard WITHOUT stopping the node or writing anything, so a
/// doomed restore (bad words / refused identity) is rejected before the command
/// stops the running node. The authoritative re-check happens under the write
/// lock in [`restore_owner_mnemonic_from_words_with_keychain`] (TOCTOU-safe);
/// this only avoids a needless node stop.
pub fn preflight_owner_mnemonic_restore(
    identity_dir: &Path,
    words: &[String],
    force: bool,
) -> Result<(), String> {
    if words.len() != 24 {
        return Err(format!("expected 24 BIP39 words, got {}", words.len()));
    }
    let phrase = Zeroizing::new(words.join(" "));
    let artifact = RecoveryArtifact::from_mnemonic(&phrase).map_err(|e| e.to_string())?;
    let derived_owner_id = artifact.master_pubkey_bundle().identity_hash();
    drop(artifact);
    owner_mnemonic_overwrite_guard(identity_dir, &derived_owner_id, force)
}

/// Words-array variant of [`restore_owner_mnemonic_with_keychain`] — the GUI
/// Tauri command restores from a pasted 24-word array without a temp file
/// (ZEB-454). Holds the same guard/re-mint/persist core; returns the restored
/// owner-id hex (the file path delegates here and `eprintln!`s it). `phrase`
/// and `seed` are `Zeroizing`; the caller owns secret hygiene for `words`.
pub fn restore_owner_mnemonic_from_words_with_keychain(
    identity_dir: &Path,
    words: &[String],
    force: bool,
    keychain: Option<KeychainStore>,
) -> Result<String, String> {
    if words.len() != 24 {
        return Err(format!("expected 24 BIP39 words, got {}", words.len()));
    }
    let phrase = Zeroizing::new(words.join(" "));
    let artifact = RecoveryArtifact::from_mnemonic(&phrase).map_err(|e| e.to_string())?;
    let derived_owner_id = artifact.master_pubkey_bundle().identity_hash();
    let seed: Zeroizing<[u8; 32]> = Zeroizing::new(*artifact.as_bytes());
    drop(artifact);

    // Serialize the entire check-and-write window under the process-wide
    // owner-state write mutex — the same lock every other owner-state writer
    // (`mint_owner_identity`, the ZEB-342 liveness refresh, pairing-persist)
    // holds. Without it, a concurrent writer in the same process (e.g. a future
    // headless daemon exposing this path, ZEB-445/452) could pass the overwrite
    // guard and then race on `owner_state.cbor` / the key slots. Recover from
    // poisoning so a panic in one writer doesn't brick future ones (mirrors
    // `mint_owner_identity_inner`).
    let _owner_write_guard = crate::owner_commands::OWNER_STATE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());

    // Overwrite guard (under the lock, BEFORE any disk write) — shared with the
    // read-only `preflight_owner_mnemonic_restore` so the GUI command can reject
    // a doomed restore before stopping the node, while this authoritative check
    // re-runs under the lock (TOCTOU-safe).
    owner_mnemonic_overwrite_guard(identity_dir, &derived_owner_id, force)?;

    // Re-mint from the recovered seed: same owner_id, fresh device key.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (state, device_sk) = crate::owner_state::remint_owner_from_seed(&seed, now)?;
    crate::owner_state::save_owner_state_atomic(
        identity_dir,
        &state,
        &device_sk,
        Some(&seed),
        keychain,
    )?;

    Ok(hex::encode(state.owner_id))
}

/// Inner entry point — accepts an injected keychain so tests can stay
/// hermetic. Production callers go through [`restore_owner_mnemonic_cli`].
pub fn restore_owner_mnemonic_with_keychain(
    identity_dir: &Path,
    mnemonic_file: &Path,
    force: bool,
    keychain: Option<KeychainStore>,
) -> Result<(), String> {
    // Read the mnemonic file, then delegate to the words-array variant. Both
    // the file text (`raw`) and the per-word `String` copies (`words`) are
    // wrapped in `Zeroizing` so no plaintext mnemonic lingers on the heap after
    // this returns (`Vec<String>: Zeroize` wipes each element on drop).
    let raw = std::fs::read_to_string(mnemonic_file)
        .map_err(|e| format!("failed to read {}: {e}", mnemonic_file.display()))?;
    let raw = Zeroizing::new(raw);
    let words: Zeroizing<Vec<String>> =
        Zeroizing::new(raw.split_whitespace().map(String::from).collect());
    let owner_id_hex =
        restore_owner_mnemonic_from_words_with_keychain(identity_dir, &words, force, keychain)?;
    eprintln!("restored owner-id: {owner_id_hex}");
    Ok(())
}

/// Restore the RETICULUM IDENTITY seed from a passphrase-encrypted recovery file.
///
/// Reads the encrypted file from `in_path`. Decrypts using the recovery
/// passphrase (`HARMONY_RECOVERY_PASSPHRASE` / `_FILE`). Writes the seed
/// via the standard resolution chain (using the at-rest
/// `HARMONY_PASSPHRASE` / `_FILE` for re-encryption). Refuses if an
/// identity already exists unless `force` is true.
///
/// Stdout: nothing. Stderr: `restored identity-hash: <hex32>`.
pub fn restore_recovery_file_cli(
    identity_path: &Path,
    in_path: &Path,
    force: bool,
    ignore_state: bool,
) -> Result<(), String> {
    let harmony_dir = identity_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    restore_recovery_file_pair_with_keychain(
        identity_path,
        &harmony_dir,
        in_path,
        None,
        force,
        ignore_state,
        KeychainStore::new().ok(),
    )
    .map(|_| ())
}

/// Inner entry point — accepts an injected keychain so tests can stay
/// hermetic. Production callers go through [`restore_recovery_file_cli`].
pub fn restore_recovery_file_with_keychain(
    identity_path: &Path,
    in_path: &Path,
    force: bool,
    keychain: Option<KeychainStore>,
) -> Result<(), String> {
    let bytes =
        std::fs::read(in_path).map_err(|e| format!("failed to read {}: {e}", in_path.display()))?;
    let passphrase = resolve_recovery_passphrase()?;
    let restored =
        RecoveryArtifact::from_encrypted_file(&bytes, &passphrase).map_err(|e| e.to_string())?;
    let artifact = restored.into_artifact();
    let seed_bytes: Zeroizing<[u8; 32]> = Zeroizing::new(*artifact.as_bytes());
    let id_hash = artifact.master_pubkey_bundle().identity_hash();

    identity::write_seed_to_disk_with_keychain(identity_path, &seed_bytes, force, keychain)?;
    eprintln!("restored identity-hash: {}", hex::encode(id_hash));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn recovery_passphrase_neither_set_fails_with_pointer_to_docs() {
        // Ensure both env vars are unset before the assertion.
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE_FILE");

        let err = resolve_recovery_passphrase().expect_err("must hard-fail when neither is set");
        assert!(err.contains("HARMONY_RECOVERY_PASSPHRASE"), "actual: {err}");
        assert!(err.contains("docs/headless-install.md"), "actual: {err}");
    }

    #[test]
    #[serial]
    fn recovery_passphrase_env_var_resolution() {
        use secrecy::ExposeSecret;

        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "from-env-direct");
        let pp = resolve_recovery_passphrase().expect("env-direct resolves");
        assert_eq!(pp.expose_secret(), "from-env-direct");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recovery.passphrase");
        std::fs::write(&path, "from-env-file\n").unwrap();
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE_FILE", &path);
        let pp = resolve_recovery_passphrase().expect("env-file resolves");
        assert_eq!(pp.expose_secret(), "from-env-file");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE_FILE");
    }

    #[test]
    #[serial]
    fn export_recovery_file_does_not_mint_identity_when_recovery_passphrase_missing() {
        // Regression: export recovery-file used to read (and thus mint) the
        // seed BEFORE resolving the recovery passphrase. On an empty install
        // with HARMONY_RECOVERY_PASSPHRASE unset, that left a freshly-minted
        // identity on disk after a "failed" export. The fix resolves the
        // recovery passphrase first; this test pins the order.
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let enc_path = dir.path().join("identity.enc");
        let recovery_out = dir.path().join("recovery.bin");

        // Empty install: no .enc, no keychain entry. At-rest store IS
        // configured (so a successful read could mint), but recovery
        // passphrase env vars are deliberately unset.
        std::env::set_var("HARMONY_PASSPHRASE", "at-rest-pass");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE_FILE");
        assert!(!enc_path.exists(), "test setup: enc file must be absent");

        let err = export_recovery_file_with_keychain(
            &identity_path,
            &recovery_out,
            Some("rt"),
            /*passphrase=*/ None,
            /*keychain=*/ None,
        )
        .expect_err("must fail when recovery passphrase is unset");
        assert!(
            err.contains("HARMONY_RECOVERY_PASSPHRASE"),
            "must fail with recovery-passphrase error; got: {err}"
        );

        // The crucial invariant: no identity store was created as a side
        // effect of the failed precondition check.
        assert!(
            !enc_path.exists(),
            "export must not mint identity.enc when recovery passphrase is missing"
        );
        assert!(
            !recovery_out.exists(),
            "export must not write the output file when recovery passphrase is missing"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn export_recovery_file_with_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let recovery_out = dir.path().join("recovery.bin");

        // Plant a known seed via the at-rest passphrase env var. The
        // write_seed_to_disk_with_keychain helper resolves the encrypted-file
        // backend from HARMONY_PASSPHRASE internally.
        std::env::set_var("HARMONY_PASSPHRASE", "at-rest-pass");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery-pass");
        identity::write_seed_to_disk_with_keychain(
            &identity_path,
            &[0xCAu8; 32],
            /*force=*/ true,
            None,
        )
        .unwrap();

        // Use the keychain-injected variant with `None` — keeps the test
        // hermetic and prevents any read/write to the developer's real OS
        // keychain entry.
        export_recovery_file_with_keychain(
            &identity_path,
            &recovery_out,
            Some("test"),
            /*passphrase=*/ None,
            /*keychain=*/ None,
        )
        .expect("export");
        assert!(recovery_out.exists(), "recovery file must be written");

        // Decode the file back; it should round-trip to the same seed.
        let bytes = std::fs::read(&recovery_out).unwrap();
        use secrecy::SecretString;
        let restored = RecoveryArtifact::from_encrypted_file(
            &bytes,
            &SecretString::from("recovery-pass".to_string()),
        )
        .unwrap()
        .into_artifact();
        assert_eq!(restored.as_bytes(), &[0xCAu8; 32]);

        std::env::remove_var("HARMONY_PASSPHRASE");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn restore_surfaces_mint_at_and_comment() {
        // ZEB-180: export stamps mint_at + carries --comment, and restore
        // surfaces BOTH in the returned RestoreResult (the CLI additionally
        // prints them to stderr). Hermetic: injected keychain=None and the
        // recovery passphrase passed directly, so no env resolution is needed.
        use secrecy::SecretString;
        let dir = tempfile::tempdir().unwrap();
        let src_identity = dir.path().join("identity.key");
        let recovery_out = dir.path().join("recovery.bin");
        let restore_dir = dir.path().join("restore");
        std::fs::create_dir_all(&restore_dir).unwrap();
        let dst_identity = restore_dir.join("identity.key");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest-pass");
        identity::write_seed_to_disk_with_keychain(
            &src_identity,
            &[0xB1u8; 32],
            /*force=*/ true,
            None,
        )
        .unwrap();

        let recovery_pass = SecretString::from("recovery-pass".to_string());
        let before = mint_timestamp_secs();
        export_recovery_file_with_keychain(
            &src_identity,
            &recovery_out,
            Some("laptop-backup"),
            Some(&recovery_pass),
            /*keychain=*/ None,
        )
        .expect("export");

        let result = restore_recovery_file_pair_with_keychain(
            &dst_identity,
            &restore_dir,
            &recovery_out,
            Some(&recovery_pass),
            /*force=*/ false,
            /*ignore_state=*/ false,
            /*keychain=*/ None,
        )
        .expect("restore");

        assert_eq!(
            result.comment.as_deref(),
            Some("laptop-backup"),
            "restore must surface the export --comment"
        );
        let minted = result
            .mint_at
            .expect("restore must surface a mint_at stamped at export");
        assert!(
            minted >= before,
            "mint_at ({minted}) must be >= the pre-export timestamp ({before})"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    fn format_mint_at_falls_back_for_out_of_range_seconds() {
        // A normal timestamp formats as RFC 3339 UTC.
        assert_eq!(format_mint_at(0), "1970-01-01T00:00:00Z");
        // A u64 value above i64::MAX must fall back to raw seconds rather than
        // wrapping into a bogus pre-1970 date (review hardening).
        let out = format_mint_at(u64::MAX);
        assert!(
            out.contains("unix seconds"),
            "out-of-range mint_at must use the raw-seconds fallback, got: {out}"
        );
    }

    #[test]
    #[serial]
    fn restore_mnemonic_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let mnemonic_path = dir.path().join("mnemonic.txt");

        std::env::set_var("HARMONY_PASSPHRASE", "restore-test");
        let original = RecoveryArtifact::from_seed([0xEFu8; 32]);
        std::fs::write(&mnemonic_path, original.to_mnemonic().as_str()).unwrap();
        let original_id = original.master_pubkey_bundle().identity_hash();

        restore_mnemonic_with_keychain(&identity_path, &mnemonic_path, /*force=*/ false, None)
            .expect("restore");

        let reloaded_seed =
            identity::read_seed_from_disk_with_keychain(&identity_path, None).unwrap();
        let reloaded = RecoveryArtifact::from_seed(*reloaded_seed);
        assert_eq!(reloaded.master_pubkey_bundle().identity_hash(), original_id);

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn restore_refuses_when_identity_exists_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let mnemonic_path = dir.path().join("mnemonic.txt");

        std::env::set_var("HARMONY_PASSPHRASE", "refuse-test");
        let original = RecoveryArtifact::from_seed([0x12u8; 32]);
        std::fs::write(&mnemonic_path, original.to_mnemonic().as_str()).unwrap();
        // Plant an existing identity.
        identity::write_seed_to_disk_with_keychain(
            &identity_path,
            &[0x99u8; 32],
            /*force=*/ true,
            None,
        )
        .unwrap();

        let err = restore_mnemonic_with_keychain(
            &identity_path,
            &mnemonic_path,
            /*force=*/ false,
            None,
        )
        .expect_err("must refuse");
        assert!(err.contains("identity already exists"), "actual: {err}");

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn restore_with_force_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let mnemonic_path = dir.path().join("mnemonic.txt");

        std::env::set_var("HARMONY_PASSPHRASE", "force-test");
        let original = RecoveryArtifact::from_seed([0xDDu8; 32]);
        std::fs::write(&mnemonic_path, original.to_mnemonic().as_str()).unwrap();
        let original_id = original.master_pubkey_bundle().identity_hash();
        // Plant a different existing identity.
        identity::write_seed_to_disk_with_keychain(
            &identity_path,
            &[0x77u8; 32],
            /*force=*/ true,
            None,
        )
        .unwrap();

        restore_mnemonic_with_keychain(&identity_path, &mnemonic_path, /*force=*/ true, None)
            .expect("force succeeds");
        let reloaded_seed =
            identity::read_seed_from_disk_with_keychain(&identity_path, None).unwrap();
        let reloaded = RecoveryArtifact::from_seed(*reloaded_seed);
        assert_eq!(reloaded.master_pubkey_bundle().identity_hash(), original_id);

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn export_mnemonic_writes_warning_to_stderr_and_words_to_stdout() {
        use crate::identity;
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");

        std::env::set_var("HARMONY_PASSPHRASE", "mnemonic-export-test");
        let planted = [0xA7u8; 32];
        identity::write_seed_to_disk_with_keychain(
            &identity_path,
            &planted,
            /*force=*/ true,
            None,
        )
        .unwrap();

        // Capture stdout + stderr in-memory and assert against the published
        // CLI contract. This is what the spec promises operators: bare 24
        // words on stdout (so `harmony-app export mnemonic > backup.txt`
        // writes only the words), warning preamble + identity-hash on stderr.
        let mut stdout = Vec::<u8>::new();
        let mut stderr = Vec::<u8>::new();
        export_mnemonic_to_writers(&identity_path, None, &mut stdout, &mut stderr)
            .expect("export must succeed");

        let stdout_str = String::from_utf8(stdout).expect("stdout is utf-8");
        let stderr_str = String::from_utf8(stderr).expect("stderr is utf-8");

        // Stdout: bare 24 words on a single line, terminated by exactly one \n.
        assert!(
            stdout_str.ends_with('\n'),
            "stdout must end with newline; got: {stdout_str:?}"
        );
        let line = stdout_str.trim_end_matches('\n');
        assert!(
            !line.contains('\n'),
            "stdout must be a single line; got: {line:?}"
        );
        let words: Vec<&str> = line.split(' ').collect();
        assert_eq!(
            words.len(),
            24,
            "stdout must be exactly 24 words; got: {line:?}"
        );

        // Stdout must round-trip back to the planted seed via RecoveryArtifact.
        let parsed = RecoveryArtifact::from_mnemonic(line).expect("words parse");
        assert_eq!(
            *parsed.as_bytes(),
            planted,
            "stdout words must encode the planted seed"
        );

        // Stderr: warning preamble + identity-hash. Don't pin the exact prose
        // (it's user-facing text that may evolve) but pin the load-bearing
        // markers: the preamble must NAME the Reticulum identity seed and
        // point owner-backup seekers at `export owner-mnemonic` (ZEB-430 —
        // an operator following "back up your identity" used to get a
        // mnemonic that does not recover friends/communities, with nothing
        // telling them so).
        assert!(
            stderr_str.contains("Reticulum identity mnemonic"),
            "stderr must name the Reticulum identity seed; got: {stderr_str:?}"
        );
        assert!(
            stderr_str.contains("owner-mnemonic"),
            "stderr must point at export owner-mnemonic for owner backup; got: {stderr_str:?}"
        );
        assert!(
            stderr_str.contains("identity-hash:"),
            "stderr must include identity-hash; got: {stderr_str:?}"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    // ── ZEB-430: export owner-mnemonic (owner MASTER seed, not the
    // Reticulum identity seed) ─────────────────────────────────────────

    #[test]
    #[serial]
    fn export_owner_mnemonic_writes_words_to_stdout_and_owner_id_to_stderr() {
        use harmony_owner::lifecycle::{mint_owner, MintResult};
        let dir = tempfile::tempdir().unwrap();

        std::env::set_var("HARMONY_PASSPHRASE", "owner-mnemonic-export-test");
        let MintResult {
            state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(1_700_000_000).unwrap();
        let master_seed = *recovery_artifact.as_bytes();
        crate::owner_state::save_owner_state_atomic(
            dir.path(),
            &state,
            &device_signing_key,
            Some(&master_seed),
            None,
        )
        .unwrap();

        let mut stdout = Vec::<u8>::new();
        let mut stderr = Vec::<u8>::new();
        export_owner_mnemonic_to_writers(dir.path(), None, &mut stdout, &mut stderr)
            .expect("export must succeed");

        let stdout_str = String::from_utf8(stdout).expect("stdout is utf-8");
        let stderr_str = String::from_utf8(stderr).expect("stderr is utf-8");

        // Stdout: bare 24 words on a single line, terminated by exactly one \n
        // — same contract as `export mnemonic`, so `> backup.txt` is pure.
        assert!(
            stdout_str.ends_with('\n'),
            "stdout must end with newline; got: {stdout_str:?}"
        );
        let line = stdout_str.trim_end_matches('\n');
        assert!(
            !line.contains('\n'),
            "stdout must be a single line; got: {line:?}"
        );
        let words: Vec<&str> = line.split(' ').collect();
        assert_eq!(
            words.len(),
            24,
            "stdout must be exactly 24 words; got: {line:?}"
        );

        // The words must round-trip to the OWNER master seed — the whole
        // point of ZEB-430 is that this is NOT the Reticulum identity seed.
        let parsed = RecoveryArtifact::from_mnemonic(line).expect("words parse");
        assert_eq!(
            *parsed.as_bytes(),
            master_seed,
            "stdout words must encode the owner master seed"
        );

        // Stderr: owner-flavored warning + an `owner-id:` fingerprint that
        // matches OwnerState.owner_id — the id the user sees in the UI.
        assert!(
            stderr_str.contains("Owner identity recovery mnemonic"),
            "stderr must include owner warning preamble; got: {stderr_str:?}"
        );
        assert!(
            stderr_str.contains(&format!("owner-id: {}", hex::encode(state.owner_id))),
            "stderr owner-id must match OwnerState.owner_id; got: {stderr_str:?}"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn export_owner_mnemonic_errors_when_owner_not_minted() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HARMONY_PASSPHRASE", "owner-mnemonic-unminted-test");

        let mut stdout = Vec::<u8>::new();
        let mut stderr = Vec::<u8>::new();
        let err = export_owner_mnemonic_to_writers(dir.path(), None, &mut stdout, &mut stderr)
            .expect_err("must fail on un-minted install");
        assert!(err.contains("not been minted"), "actual: {err}");
        assert!(
            stdout.is_empty(),
            "no words may reach stdout on failure; got: {:?}",
            String::from_utf8_lossy(&stdout)
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn export_owner_mnemonic_errors_when_master_seed_wiped() {
        use harmony_owner::lifecycle::{mint_owner, MintResult};
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HARMONY_PASSPHRASE", "owner-mnemonic-wiped-test");

        // Joiner / cert-only model: owner state exists but no master seed
        // (save with None also clears any stale seed slot).
        let MintResult {
            state,
            device_signing_key,
            ..
        } = mint_owner(1_700_000_001).unwrap();
        crate::owner_state::save_owner_state_atomic(
            dir.path(),
            &state,
            &device_signing_key,
            None,
            None,
        )
        .unwrap();

        let mut stdout = Vec::<u8>::new();
        let mut stderr = Vec::<u8>::new();
        let err = export_owner_mnemonic_to_writers(dir.path(), None, &mut stdout, &mut stderr)
            .expect_err("must fail when master seed absent");
        assert!(err.contains("wiped"), "actual: {err}");
        assert!(stdout.is_empty(), "no words may reach stdout on failure");

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn export_owner_mnemonic_refuses_seed_that_mismatches_owner_id() {
        use harmony_owner::lifecycle::{mint_owner, MintResult};
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HARMONY_PASSPHRASE", "owner-mnemonic-mismatch-test");

        // Corrupt install: owner_state.cbor from mint A, master seed from
        // mint B. Exporting B's words would produce a "backup" that cannot
        // restore A's identity — the export must refuse, not hand the user
        // a paper artifact that silently fails years later.
        let MintResult {
            state,
            device_signing_key,
            ..
        } = mint_owner(1_700_000_002).unwrap();
        let MintResult {
            recovery_artifact: other_artifact,
            ..
        } = mint_owner(1_700_000_003).unwrap();
        crate::owner_state::save_owner_state_atomic(
            dir.path(),
            &state,
            &device_signing_key,
            Some(other_artifact.as_bytes()),
            None,
        )
        .unwrap();

        let mut stdout = Vec::<u8>::new();
        let mut stderr = Vec::<u8>::new();
        let err = export_owner_mnemonic_to_writers(dir.path(), None, &mut stdout, &mut stderr)
            .expect_err("must refuse mismatched seed/state");
        assert!(err.contains("mismatch"), "actual: {err}");
        assert!(stdout.is_empty(), "no words may reach stdout on refusal");

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    // ── ZEB-439: owner master-seed restore (re-adopt from mnemonic) ──────────

    /// Plant a minted owner in `dir` and return its 24-word owner mnemonic +
    /// owner_id. Mirrors the export-owner test setup (registry, file backend).
    fn plant_owner_and_export_words(dir: &Path, now: u64) -> (String, [u8; 16]) {
        use harmony_owner::lifecycle::{mint_owner, MintResult};
        let MintResult {
            state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(now).unwrap();
        let master_seed = *recovery_artifact.as_bytes();
        crate::owner_state::save_owner_state_atomic(
            dir,
            &state,
            &device_signing_key,
            Some(&master_seed),
            None,
        )
        .unwrap();
        let (words, owner_id) = export_owner_mnemonic_words_with_keychain(dir, None).unwrap();
        (words.join(" "), owner_id)
    }

    // ── ZEB-454: words-array variant + owner-id preview (GUI restore path) ──

    #[test]
    #[serial]
    fn preview_owner_mnemonic_owner_id_matches_exported_owner_id() {
        std::env::set_var("HARMONY_PASSPHRASE", "owner-preview-match");
        let dir = tempfile::tempdir().unwrap();
        let (phrase, owner_id) = plant_owner_and_export_words(dir.path(), 1_700_000_100);
        let words: Vec<String> = phrase.split_whitespace().map(String::from).collect();

        let previewed = preview_owner_mnemonic_owner_id(&words)
            .expect("preview must derive the owner-id without touching disk");
        assert_eq!(
            previewed,
            hex::encode(owner_id),
            "previewed owner-id must equal the identity's owner-id"
        );
        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    fn preview_owner_mnemonic_owner_id_rejects_wrong_word_count() {
        let words: Vec<String> = (0..23).map(|_| "abandon".to_string()).collect();
        let err = preview_owner_mnemonic_owner_id(&words)
            .expect_err("23 words must be rejected before any derivation");
        assert!(err.contains("24 BIP39 words"), "got: {err}");
    }

    #[test]
    #[serial]
    fn restore_owner_mnemonic_from_words_roundtrips_and_returns_owner_id() {
        std::env::set_var("HARMONY_PASSPHRASE", "owner-words-roundtrip");
        // Source: mint + export.
        let src = tempfile::tempdir().unwrap();
        let (phrase, owner_id) = plant_owner_and_export_words(src.path(), 1_700_000_110);
        let words: Vec<String> = phrase.split_whitespace().map(String::from).collect();

        // Destination: empty install, restore from the words array (GUI path).
        let dst = tempfile::tempdir().unwrap();
        let returned = restore_owner_mnemonic_from_words_with_keychain(
            dst.path(),
            &words,
            /*force=*/ false,
            None,
        )
        .expect("words-array restore must succeed onto an empty install");
        assert_eq!(
            returned,
            hex::encode(owner_id),
            "restore must return the restored owner-id hex"
        );
        let restored = crate::owner_state::load_owner_state(dst.path(), None)
            .unwrap()
            .expect("owner present after restore");
        assert_eq!(restored.state.owner_id, owner_id);
        let (restored_words, _) =
            export_owner_mnemonic_words_with_keychain(dst.path(), None).unwrap();
        assert_eq!(
            restored_words.join(" "),
            phrase,
            "round-trip yields same words"
        );
        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn restore_owner_mnemonic_from_words_force_refuses_different_owner() {
        std::env::set_var("HARMONY_PASSPHRASE", "owner-words-wrongowner");
        let dir = tempfile::tempdir().unwrap();
        let (_a_phrase, a_owner_id) = plant_owner_and_export_words(dir.path(), 1_700_000_120);
        let other = tempfile::tempdir().unwrap();
        let (b_phrase, b_owner_id) = plant_owner_and_export_words(other.path(), 1_700_000_121);
        assert_ne!(a_owner_id, b_owner_id, "test setup: A and B must differ");
        let b_words: Vec<String> = b_phrase.split_whitespace().map(String::from).collect();

        let err = restore_owner_mnemonic_from_words_with_keychain(
            dir.path(),
            &b_words,
            /*force=*/ true,
            None,
        )
        .expect_err("words-array restore must refuse a DIFFERENT owner even with force");
        assert!(err.contains("different identity"), "got: {err}");
        let still = crate::owner_state::load_owner_state(dir.path(), None)
            .unwrap()
            .unwrap();
        assert_eq!(
            still.state.owner_id, a_owner_id,
            "A untouched after refusal"
        );
        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn preflight_owner_mnemonic_restore_validates_without_writing() {
        std::env::set_var("HARMONY_PASSPHRASE", "owner-preflight");
        let src = tempfile::tempdir().unwrap();
        let (phrase, _owner_id) = plant_owner_and_export_words(src.path(), 1_700_000_200);
        let words: Vec<String> = phrase.split_whitespace().map(String::from).collect();

        // Empty install + valid words → Ok, and NOTHING is written.
        let fresh = tempfile::tempdir().unwrap();
        preflight_owner_mnemonic_restore(fresh.path(), &words, false)
            .expect("preflight passes on an empty install");
        assert!(
            !fresh.path().join("owner_state.cbor").exists(),
            "preflight must not write owner_state.cbor"
        );

        // Existing DIFFERENT owner, even with force → refused (no write).
        let dir = tempfile::tempdir().unwrap();
        let (_a_phrase, a_owner_id) = plant_owner_and_export_words(dir.path(), 1_700_000_201);
        let other = tempfile::tempdir().unwrap();
        let (b_phrase, b_owner_id) = plant_owner_and_export_words(other.path(), 1_700_000_202);
        assert_ne!(a_owner_id, b_owner_id, "test setup: A and B must differ");
        let b_words: Vec<String> = b_phrase.split_whitespace().map(String::from).collect();
        let err = preflight_owner_mnemonic_restore(dir.path(), &b_words, true)
            .expect_err("preflight refuses a different owner");
        assert!(err.contains("different identity"), "got: {err}");

        // Wrong word count → Err before any derivation.
        let short: Vec<String> = (0..23).map(|_| "abandon".to_string()).collect();
        assert!(preflight_owner_mnemonic_restore(fresh.path(), &short, false).is_err());

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn restore_owner_mnemonic_roundtrips_owner_id_onto_fresh_device() {
        std::env::set_var("HARMONY_PASSPHRASE", "owner-restore-roundtrip");

        // Source machine: mint + export the owner mnemonic.
        let src = tempfile::tempdir().unwrap();
        let (phrase, owner_id) = plant_owner_and_export_words(src.path(), 1_700_000_000);

        // Destination machine: empty install, restore from the words.
        let dst = tempfile::tempdir().unwrap();
        let mnemonic_file = dst.path().join("owner.txt");
        std::fs::write(&mnemonic_file, &phrase).unwrap();
        assert!(
            !dst.path().join("owner_state.cbor").exists(),
            "dst must start empty"
        );

        restore_owner_mnemonic_with_keychain(
            dst.path(),
            &mnemonic_file,
            /*force=*/ false,
            None,
        )
        .expect("restore must succeed onto an empty install");

        // Restored owner_id matches source; re-exporting yields the SAME words.
        let restored = crate::owner_state::load_owner_state(dst.path(), None)
            .expect("load")
            .expect("owner present after restore");
        assert_eq!(
            restored.state.owner_id, owner_id,
            "restored owner_id must match the source identity"
        );
        assert_eq!(
            restored.state.enrollments.len(),
            1,
            "exactly one (fresh) device enrolled after restore"
        );
        let (restored_words, _) =
            export_owner_mnemonic_words_with_keychain(dst.path(), None).unwrap();
        assert_eq!(
            restored_words.join(" "),
            phrase,
            "round-trip: re-export from the restored install yields the same words"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn restore_owner_mnemonic_refuses_existing_owner_without_force() {
        std::env::set_var("HARMONY_PASSPHRASE", "owner-restore-noforce");
        let dir = tempfile::tempdir().unwrap();
        let (phrase, _owner_id) = plant_owner_and_export_words(dir.path(), 1_700_000_010);
        let mnemonic_file = dir.path().join("owner.txt");
        std::fs::write(&mnemonic_file, &phrase).unwrap();

        let err = restore_owner_mnemonic_with_keychain(
            dir.path(),
            &mnemonic_file,
            /*force=*/ false,
            None,
        )
        .expect_err("must refuse to overwrite an existing owner without --force");
        assert!(
            err.contains("--force"),
            "error must point at --force; got: {err}"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn restore_owner_mnemonic_with_force_refuses_different_owner() {
        std::env::set_var("HARMONY_PASSPHRASE", "owner-restore-wrongowner");
        // `dir` holds owner A.
        let dir = tempfile::tempdir().unwrap();
        let (_a_phrase, a_owner_id) = plant_owner_and_export_words(dir.path(), 1_700_000_020);
        // A DIFFERENT owner B's mnemonic (minted in a separate dir).
        let other = tempfile::tempdir().unwrap();
        let (b_phrase, b_owner_id) = plant_owner_and_export_words(other.path(), 1_700_000_021);
        assert_ne!(a_owner_id, b_owner_id, "test setup: A and B must differ");

        let mnemonic_file = dir.path().join("b.txt");
        std::fs::write(&mnemonic_file, &b_phrase).unwrap();

        let err = restore_owner_mnemonic_with_keychain(
            dir.path(),
            &mnemonic_file,
            /*force=*/ true,
            None,
        )
        .expect_err("must refuse to overwrite a DIFFERENT owner even with --force");
        assert!(
            err.contains("different identity"),
            "error must explain the owner_id mismatch; got: {err}"
        );
        // A's identity must be intact after the refusal.
        let still = crate::owner_state::load_owner_state(dir.path(), None)
            .unwrap()
            .unwrap();
        assert_eq!(
            still.state.owner_id, a_owner_id,
            "A's identity must be untouched after a refused restore"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn restore_owner_mnemonic_with_force_readopts_same_owner() {
        std::env::set_var("HARMONY_PASSPHRASE", "owner-restore-readopt");
        let dir = tempfile::tempdir().unwrap();
        let (phrase, owner_id) = plant_owner_and_export_words(dir.path(), 1_700_000_030);
        let old_device = crate::owner_state::load_owner_state(dir.path(), None)
            .unwrap()
            .unwrap()
            .device_signing_key
            .to_bytes();

        let mnemonic_file = dir.path().join("owner.txt");
        std::fs::write(&mnemonic_file, &phrase).unwrap();

        restore_owner_mnemonic_with_keychain(
            dir.path(),
            &mnemonic_file,
            /*force=*/ true,
            None,
        )
        .expect("re-adopting the SAME owner with --force must succeed");

        let after = crate::owner_state::load_owner_state(dir.path(), None)
            .unwrap()
            .unwrap();
        assert_eq!(
            after.state.owner_id, owner_id,
            "owner_id unchanged after re-adoption"
        );
        assert_ne!(
            after.device_signing_key.to_bytes(),
            old_device,
            "re-adoption mints a fresh device key"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn restore_owner_mnemonic_corrupt_marker_blocks_without_force_then_force_overwrites() {
        // A corrupt owner_state.cbor must not WEDGE recovery: without --force it
        // surfaces an actionable error (not a bare parse failure); with --force
        // the operator's explicit destructive intent overwrites it. (Qodo #1)
        std::env::set_var("HARMONY_PASSPHRASE", "owner-restore-corrupt");
        let dir = tempfile::tempdir().unwrap();

        // A valid paper backup minted elsewhere.
        let src = tempfile::tempdir().unwrap();
        let (phrase, owner_id) = plant_owner_and_export_words(src.path(), 1_700_000_040);
        let mnemonic_file = dir.path().join("owner.txt");
        std::fs::write(&mnemonic_file, &phrase).unwrap();

        // Plant a CORRUPT owner_state.cbor marker in the destination.
        std::fs::write(dir.path().join("owner_state.cbor"), b"not-cbor-bytes").unwrap();

        // Without --force: actionable error pointing at --force, no clobber.
        let err = restore_owner_mnemonic_with_keychain(
            dir.path(),
            &mnemonic_file,
            /*force=*/ false,
            None,
        )
        .expect_err("a corrupt marker must block restore without --force");
        assert!(
            err.contains("--force") && err.contains("could not be read"),
            "error must be actionable about the unreadable marker; got: {err}"
        );

        // With --force: overwrite the corrupt marker and adopt the mnemonic's owner.
        restore_owner_mnemonic_with_keychain(
            dir.path(),
            &mnemonic_file,
            /*force=*/ true,
            None,
        )
        .expect("--force must overwrite a corrupt marker");
        let restored = crate::owner_state::load_owner_state(dir.path(), None)
            .unwrap()
            .unwrap();
        assert_eq!(
            restored.state.owner_id, owner_id,
            "forced restore over a corrupt marker must adopt the mnemonic's owner"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    use crate::owner_state_crdt::OwnerState;

    /// Plant a usable owner-state file at `harmony_dir/owner_state_crdt.cbor`.
    fn plant_owner_state(harmony_dir: &Path) {
        let state = OwnerState::default();
        let state_path = super::owner_state_path(harmony_dir);
        // Seal under the SAME per-dir cipher the code under test derives —
        // a fixed test cipher would AEAD-fail in export/restore paths.
        let cipher = crate::device_dataset_file::get_or_derive(harmony_dir).unwrap();
        crate::owner_state_persist::save_crdt(&cipher, &state_path, &state).unwrap();
    }

    #[test]
    #[serial]
    fn export_emits_pair_when_state_exists() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");
        identity::write_seed_to_disk_with_keychain(&identity_path, &[0xCA; 32], true, None)
            .unwrap();
        plant_owner_state(dir.path());

        let result = super::export_recovery_file_pair_with_keychain(
            &identity_path,
            dir.path(),
            &out,
            Some("rt"),
            None,
            /*include_state=*/ true,
            /*force=*/ false,
            None,
        )
        .expect("export");
        assert!(result.hrss_path.is_some());
        assert!(out.exists());
        let sidecar = super::sidecar_path(&out);
        assert!(sidecar.exists());
        assert!(result.snapshot_bytes_written > 0);

        std::env::remove_var("HARMONY_PASSPHRASE");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn export_emits_solo_when_no_state() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");
        identity::write_seed_to_disk_with_keychain(&identity_path, &[0xCA; 32], true, None)
            .unwrap();
        // No plant_owner_state — owner_state_crdt.cbor absent.

        let result = super::export_recovery_file_pair_with_keychain(
            &identity_path,
            dir.path(),
            &out,
            None,
            None,
            /*include_state=*/ true, // requested but file is missing
            /*force=*/ false,
            None,
        )
        .expect("export");
        assert!(result.hrss_path.is_none(), "no sidecar when state missing");
        assert!(out.exists());
        assert!(!super::sidecar_path(&out).exists());

        std::env::remove_var("HARMONY_PASSPHRASE");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn export_no_state_flag_skips_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");
        identity::write_seed_to_disk_with_keychain(&identity_path, &[0xCA; 32], true, None)
            .unwrap();
        plant_owner_state(dir.path());

        let result = super::export_recovery_file_pair_with_keychain(
            &identity_path,
            dir.path(),
            &out,
            None,
            None,
            /*include_state=*/ false, // explicit opt-out
            /*force=*/ false,
            None,
        )
        .expect("export");
        assert!(result.hrss_path.is_none(), "opt-out must skip sidecar");

        std::env::remove_var("HARMONY_PASSPHRASE");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn export_refuses_when_sidecar_exists_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");
        identity::write_seed_to_disk_with_keychain(&identity_path, &[0xCA; 32], true, None)
            .unwrap();
        plant_owner_state(dir.path());
        std::fs::write(super::sidecar_path(&out), b"stale").unwrap();

        let err = super::export_recovery_file_pair_with_keychain(
            &identity_path,
            dir.path(),
            &out,
            None,
            None,
            true,
            /*force=*/ false,
            None,
        )
        .expect_err("must refuse");
        assert!(
            err.contains("already exists") && err.contains("--force"),
            "error must direct to --force: {err}"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn restore_pair_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");
        let seed = [0xCA; 32];
        identity::write_seed_to_disk_with_keychain(&identity_path, &seed, true, None).unwrap();
        plant_owner_state(dir.path());
        super::export_recovery_file_pair_with_keychain(
            &identity_path,
            dir.path(),
            &out,
            None,
            None,
            true,
            true,
            None,
        )
        .unwrap();

        // Wipe identity + owner-state, then restore.
        let _ = std::fs::remove_file(&identity_path);
        let _ = std::fs::remove_file(super::owner_state_path(dir.path()));
        let result = super::restore_recovery_file_pair_with_keychain(
            &identity_path,
            dir.path(),
            &out,
            None,
            /*force=*/ true,
            /*ignore_state=*/ false,
            None,
        )
        .expect("restore");
        assert!(result.sidecar_present);
        // Identity round-trip.
        let reloaded = identity::read_seed_from_disk_with_keychain(&identity_path, None).unwrap();
        assert_eq!(&*reloaded, &seed);
        // Owner-state file restored.
        assert!(super::owner_state_path(dir.path()).exists());

        std::env::remove_var("HARMONY_PASSPHRASE");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn restore_ignores_missing_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");
        identity::write_seed_to_disk_with_keychain(&identity_path, &[0x42; 32], true, None)
            .unwrap();
        // Skip plant_owner_state — HRSS will not be emitted.
        super::export_recovery_file_pair_with_keychain(
            &identity_path,
            dir.path(),
            &out,
            None,
            None,
            true,
            true,
            None,
        )
        .unwrap();
        assert!(!super::sidecar_path(&out).exists());

        let _ = std::fs::remove_file(&identity_path);
        let result = super::restore_recovery_file_pair_with_keychain(
            &identity_path,
            dir.path(),
            &out,
            None,
            true,
            false,
            None,
        )
        .expect("restore identity-only ok");
        assert!(!result.sidecar_present);
        assert_eq!(result.spaces_restored, 0);

        std::env::remove_var("HARMONY_PASSPHRASE");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn restore_ignore_state_flag_skips_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");
        identity::write_seed_to_disk_with_keychain(&identity_path, &[0x42; 32], true, None)
            .unwrap();
        plant_owner_state(dir.path());
        super::export_recovery_file_pair_with_keychain(
            &identity_path,
            dir.path(),
            &out,
            None,
            None,
            true,
            true,
            None,
        )
        .unwrap();

        let _ = std::fs::remove_file(&identity_path);
        let _ = std::fs::remove_file(super::owner_state_path(dir.path()));
        let result = super::restore_recovery_file_pair_with_keychain(
            &identity_path,
            dir.path(),
            &out,
            None,
            true,
            /*ignore_state=*/ true,
            None,
        )
        .expect("restore");
        assert!(!result.sidecar_present);
        assert!(!super::owner_state_path(dir.path()).exists());

        std::env::remove_var("HARMONY_PASSPHRASE");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn restore_force_overwrites_existing_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");
        identity::write_seed_to_disk_with_keychain(&identity_path, &[0x42; 32], true, None)
            .unwrap();
        plant_owner_state(dir.path());
        super::export_recovery_file_pair_with_keychain(
            &identity_path,
            dir.path(),
            &out,
            None,
            None,
            true,
            true,
            None,
        )
        .unwrap();

        // Plant a different owner_state_crdt.cbor to force a "exists" collision.
        let mut state = OwnerState::default();
        let sp = crate::owner_state_types::Space {
            id: crate::owner_state_types::SpaceId([0x99; 16]),
            kind: crate::owner_state_types::SpaceKind::Folder,
            parent: None,
            community_id: None,
            name: "different".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: crate::owner_state_types::Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "x".into(),
            },
            updated_at: crate::owner_state_types::Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "x".into(),
            },
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
        state.spaces.insert(sp.id, sp);
        crate::owner_state_persist::save_crdt(
            &crate::device_dataset_file::get_or_derive(dir.path()).unwrap(),
            &super::owner_state_path(dir.path()),
            &state,
        )
            .unwrap();

        // force=true overwrites.
        super::restore_recovery_file_pair_with_keychain(
            &identity_path,
            dir.path(),
            &out,
            None,
            true,
            false,
            None,
        )
        .expect("force restore succeeds");

        std::env::remove_var("HARMONY_PASSPHRASE");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn restore_addr_mismatch_hard_fails() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path_a = dir.path().join("a.key");
        let identity_path_b = dir.path().join("b.key");
        let out_a = dir.path().join("a.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");

        // Build owner A's identity + state + sidecar.
        identity::write_seed_to_disk_with_keychain(&identity_path_a, &[0xAA; 32], true, None)
            .unwrap();
        plant_owner_state(dir.path());
        super::export_recovery_file_pair_with_keychain(
            &identity_path_a,
            dir.path(),
            &out_a,
            None,
            None,
            true,
            true,
            None,
        )
        .unwrap();

        // Build owner B's identity (different seed).
        identity::write_seed_to_disk_with_keychain(&identity_path_b, &[0xBB; 32], true, None)
            .unwrap();
        // Now overwrite a.bin's HRMR with B's identity but keep A's HRSS.
        // Easiest: emit B's identity-only backup at out_a, then keep A's
        // sidecar at out_a.state.
        let out_b = dir.path().join("b.bin");
        super::export_recovery_file_pair_with_keychain(
            &identity_path_b,
            dir.path(),
            &out_b,
            None,
            None,
            false, // identity-only for B
            true,
            None,
        )
        .unwrap();
        std::fs::copy(&out_b, &out_a).unwrap(); // a.bin = B's identity
                                                // a.bin.state is still A's sidecar (untouched).

        let _ = std::fs::remove_file(&identity_path_a);
        let err = super::restore_recovery_file_pair_with_keychain(
            &identity_path_a,
            dir.path(),
            &out_a,
            None,
            true,
            false,
            None,
        )
        .expect_err("addr mismatch must fail");
        assert!(
            err.contains("state sidecar identity mismatch") || err.contains("AddrMismatch"),
            "expected addr-mismatch in: {err}"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn export_state_persists_last_backup_record() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");
        identity::write_seed_to_disk_with_keychain(&identity_path, &[0xCA; 32], true, None)
            .unwrap();
        plant_owner_state(dir.path());
        super::export_recovery_file_pair_with_keychain(
            &identity_path,
            dir.path(),
            &out,
            None,
            None,
            true,
            true,
            None,
        )
        .unwrap();

        let last = crate::backup_state::load_last_backup(&super::last_backup_path(dir.path()))
            .unwrap()
            .expect("last_backup.json must be written");
        assert!(last.include_state);
        assert_eq!(last.out_path, out.display().to_string());

        std::env::remove_var("HARMONY_PASSPHRASE");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
    }

    /// ZEB-975 regression: the staleness evaluator must read the SAME
    /// directory the writers write — the harmony/identity dir. Before the
    /// fix, `get_backup_staleness` resolved Tauri's app-data dir, which no
    /// writer ever touches, so the banner always evaluated an empty
    /// `OwnerState` with no backup record (never stale, forever).
    ///
    /// Writes through the REAL paths — `export_recovery_file_pair_with_keychain`
    /// for `last_backup.json` and `owner_state_persist::save_crdt` (the same
    /// fn the boot engine uses) for the CRDT — then asserts the reader sees
    /// them, and that the pre-fix dir shape (a dir nothing wrote) reports the
    /// dead-banner default instead.
    #[test]
    #[serial]
    fn staleness_from_dir_sees_real_export_and_engine_writes() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");
        identity::write_seed_to_disk_with_keychain(&identity_path, &[0xEE; 32], true, None)
            .unwrap();
        plant_owner_state(dir.path());
        super::export_recovery_file_pair_with_keychain(
            &identity_path,
            dir.path(),
            &out,
            None,
            None,
            true,
            true,
            None,
        )
        .unwrap();

        // The export stamped `last_backup.json` with the real wall clock;
        // read it back so the assertions below are exact, not racy.
        let backup_at = crate::backup_state::load_last_backup(&super::last_backup_path(dir.path()))
            .unwrap()
            .expect("export writes last_backup.json")
            .at
            .wall_ms;

        // Plant a mutation AFTER the backup via the engine's own persist fn.
        // (`plant_owner_state` writes an EMPTY state, so insert a real Space —
        // `last_mutation_wall_ms` scans HLC-bearing entries, not file mtimes.)
        let state_path = super::owner_state_path(dir.path());
        let cipher = crate::device_dataset_file::get_or_derive(dir.path()).unwrap();
        let mut state = crate::owner_state_persist::load_crdt(&cipher, &state_path).unwrap();
        let mutated_at = crate::owner_state_types::Hlc {
            wall_ms: backup_at + 86_400_000,
            logical: 0,
            device_id: "test".into(),
        };
        let sp = crate::owner_state_types::Space {
            id: crate::owner_state_types::SpaceId([7; 16]),
            kind: crate::owner_state_types::SpaceKind::Folder,
            parent: None,
            community_id: None,
            name: "post-backup".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: mutated_at.clone(),
            updated_at: mutated_at,
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
        state.spaces.insert(sp.id, sp);
        crate::owner_state_persist::save_crdt(&cipher, &state_path, &state).unwrap();

        // 20 days later, with post-backup mutations: stale, 20 days since.
        let now = backup_at + 20 * 86_400_000;
        let r = crate::backup_state::staleness_from_dir(dir.path(), now, None);
        assert!(r.is_stale, "reader must see the writers' files: {r:?}");
        assert_eq!(r.days_since, 20);

        // The ZEB-975 failure shape: pointed at a dir nothing writes (what
        // resolving app-data amounted to), the evaluator sees defaults and
        // can never report staleness.
        let unwritten = tempfile::tempdir().unwrap();
        let r = crate::backup_state::staleness_from_dir(unwritten.path(), now, None);
        assert!(
            !r.is_stale,
            "a dir with no owner-state/backup files reports the dead-banner default: {r:?}"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn restore_refuses_state_overwrite_without_force_leaves_identity_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");

        // Plant identity A + state.
        identity::write_seed_to_disk_with_keychain(&identity_path, &[0xAA; 32], true, None)
            .unwrap();
        plant_owner_state(dir.path());
        super::export_recovery_file_pair_with_keychain(
            &identity_path,
            dir.path(),
            &out,
            None,
            None,
            true,
            true,
            None,
        )
        .unwrap();

        // Pre-existing identity B installed (different seed), and a pre-existing
        // owner_state_crdt.cbor that would block the sidecar restore.
        identity::write_seed_to_disk_with_keychain(&identity_path, &[0xBB; 32], true, None)
            .unwrap();
        let identity_before =
            identity::read_seed_from_disk_with_keychain(&identity_path, None).unwrap();
        // owner_state_crdt.cbor still exists from plant_owner_state above.

        // Restore without --force -- must refuse and leave identity B untouched.
        let err = super::restore_recovery_file_pair_with_keychain(
            &identity_path,
            dir.path(),
            &out,
            None,
            /*force=*/ false,
            /*ignore_state=*/ false,
            None,
        )
        .expect_err("must refuse when owner_state_crdt.cbor exists without --force");
        assert!(
            err.contains("owner-state file already exists") && err.contains("--force"),
            "expected metadata refusal: {err}"
        );

        // Identity B's seed must still be on disk — NOT overwritten with A's seed.
        let identity_after =
            identity::read_seed_from_disk_with_keychain(&identity_path, None).unwrap();
        assert_eq!(
            &*identity_before, &*identity_after,
            "identity must be untouched after the metadata-refusal path"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn restore_reports_snapshot_export_wall_ms_not_last_backup_json() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");

        // Plant + export from this dir.
        identity::write_seed_to_disk_with_keychain(&identity_path, &[0xCC; 32], true, None)
            .unwrap();
        plant_owner_state(dir.path());
        let export_result = super::export_recovery_file_pair_with_keychain(
            &identity_path,
            dir.path(),
            &out,
            None,
            None,
            true,
            true,
            None,
        )
        .unwrap();
        assert!(export_result.hrss_path.is_some());

        // Simulate fresh-machine restore: copy the artifacts into a SECOND tempdir
        // that has NO last_backup.json.
        let fresh = tempfile::tempdir().unwrap();
        let fresh_plaintext = fresh.path().join("identity.key");
        let fresh_out = fresh.path().join("recovery.bin");
        std::fs::copy(&out, &fresh_out).unwrap();
        std::fs::copy(super::sidecar_path(&out), super::sidecar_path(&fresh_out)).unwrap();

        let result = super::restore_recovery_file_pair_with_keychain(
            &fresh_plaintext,
            fresh.path(),
            &fresh_out,
            None,
            true,
            false,
            None,
        )
        .expect("restore");
        assert!(result.sidecar_present);
        let reported = result
            .snapshot_at
            .as_ref()
            .expect("restore result must carry snapshot HLC when sidecar restored")
            .wall_ms;
        // Must be the actual snapshot's at.wall_ms — a NONZERO sane value, NOT 0.
        assert!(
            reported > 0,
            "snapshot wall_ms must be the snapshot's own export-time HLC, not last_backup.json (which is absent on fresh machine)"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
    }

    /// Round-1 bot finding M1 (CodeRabbit + CodeAnt): if a previous paired
    /// export wrote `recovery.bin` + `recovery.bin.state`, then the user
    /// runs `--no-state` (or GUI toggles include_state off), the function
    /// must REMOVE the stale `.state` sidecar at the same path. Otherwise
    /// the next restore would auto-detect a sidecar bound to the previous
    /// identity and either hard-fail (addr mismatch) or, worse, apply a
    /// stale tree against the new identity.
    #[test]
    #[serial]
    fn export_no_state_removes_stale_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");
        identity::write_seed_to_disk_with_keychain(&identity_path, &[0x77; 32], true, None)
            .unwrap();

        // First export: paired (HRMR + HRSS sidecar).
        plant_owner_state(dir.path());
        super::export_recovery_file_pair_with_keychain(
            &identity_path,
            dir.path(),
            &out,
            None,
            None,
            /*include_state=*/ true,
            /*force=*/ true,
            None,
        )
        .expect("first export");
        let sidecar = super::sidecar_path(&out);
        assert!(
            sidecar.exists(),
            "first export must have written the sidecar"
        );

        // Second export: identity-only (include_state=false). The stale
        // sidecar must be removed even though it's not what we're writing.
        super::export_recovery_file_pair_with_keychain(
            &identity_path,
            dir.path(),
            &out,
            None,
            None,
            /*include_state=*/ false,
            /*force=*/ true,
            None,
        )
        .expect("second export");

        assert!(out.exists(), "new HRMR is on disk");
        assert!(
            !sidecar.exists(),
            "stale sidecar must be removed when include_state=false"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
    }

    /// Round-1 bot finding C5 (Qodo High Severity): when `force=true` and
    /// a pre-existing valid recovery file is on disk, a write failure
    /// AFTER the HRMR has been overwritten must NOT leave the user with
    /// no backup at all. The fix renames the old file aside as `.bak`,
    /// and on rollback restores it.
    ///
    /// We inject a failure by corrupting the `owner_state_crdt.cbor` AFTER
    /// the first export. The second export will:
    ///   1. Move the old recovery.bin aside as `.bak` (move_aside).
    ///   2. Successfully write the new HRMR.
    ///   3. Fail at `load_crdt(...)` (the corrupt CBOR).
    ///   4. Hit the rollback path — which under the fix restores the
    ///      .bak file back to the original recovery.bin location.
    ///
    /// Pre-fix, the rollback used to `remove_file(out)` and then return,
    /// leaving the user with no recovery file at all.
    #[test]
    #[serial]
    fn export_pair_failure_restores_preexisting_recovery_file() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");
        let state_path = super::owner_state_path(dir.path());

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");
        identity::write_seed_to_disk_with_keychain(&identity_path, &[0xC5; 32], true, None)
            .unwrap();
        plant_owner_state(dir.path());

        // First export: paired so we have a known-good recovery.bin baseline.
        super::export_recovery_file_pair_with_keychain(
            &identity_path,
            dir.path(),
            &out,
            None,
            None,
            /*include_state=*/ true,
            /*force=*/ true,
            None,
        )
        .expect("first export");
        let original_bytes = std::fs::read(&out).expect("read original recovery.bin");
        assert!(!original_bytes.is_empty());

        // Corrupt the owner-state CBOR. The second export's load_crdt()
        // call (in the post-HRMR rollback path) will fail, triggering
        // the C5 rollback dance.
        std::fs::write(&state_path, b"\x00\xFFnot-valid-cbor").unwrap();

        // Second export: paired (include_state=true). HRMR overwrite
        // succeeds, then load_crdt fails on the corrupt state file. The
        // C5 dance must restore the pre-existing recovery.bin.
        let err = super::export_recovery_file_pair_with_keychain(
            &identity_path,
            dir.path(),
            &out,
            None,
            None,
            /*include_state=*/ true,
            /*force=*/ true,
            None,
        )
        .expect_err("load_crdt must fail on corrupt owner-state");
        assert!(
            err.contains("HRMR rolled back") || err.contains("failed to load"),
            "error must mention rollback or load failure; got: {err}"
        );

        // Load-bearing assertion: the OLD recovery.bin is intact.
        // Pre-fix, this file would be gone (remove_file(out) on rollback
        // and no `.bak` restoration).
        let recovered = std::fs::read(&out).expect("read recovery.bin after failed export");
        assert_eq!(
            recovered, original_bytes,
            "C5: failed pair-export must restore the pre-existing recovery.bin"
        );

        // No leftover .bak files either (the C5 dance deletes them on
        // success AND restores them on failure).
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.starts_with("recovery.bin.") && name.ends_with(".bak")
            })
            .collect();
        assert!(
            entries.is_empty(),
            "C5: rollback must clean up its .bak files; got: {entries:?}"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
    }
}
