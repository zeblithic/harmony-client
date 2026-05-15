//! CLI subcommand entry points for identity backup/restore.
//!
//! Each entry point composes [`crate::identity::read_seed_from_disk`] /
//! [`crate::identity::write_seed_to_disk`] with the appropriate
//! [`harmony_owner::recovery`] API. The recovery passphrase
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

/// Export the master seed as a 24-word BIP39 English mnemonic.
///
/// Side effects:
///   - Reads the seed via the standard resolution chain (keychain → encrypted file).
///   - Writes the bare 24 words on a single line to stdout, terminated by `\n`.
///   - Writes a warning preamble + `identity-hash: <hex32>` to stderr.
///
/// Stdout/stderr separation is the load-bearing UX: `harmony-app export
/// mnemonic > backup.txt` writes only the words; running interactively shows
/// the warning + fingerprint on the terminal.
pub fn export_mnemonic_cli(plaintext_path: &Path) -> Result<(), String> {
    export_mnemonic_to_writers(
        plaintext_path,
        KeychainStore::new().ok(),
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    )
}

/// Inner entry point — accepts an injected keychain AND output writers so
/// tests can both stay hermetic AND assert the exact stdout/stderr contract.
/// Production callers go through [`export_mnemonic_cli`].
pub fn export_mnemonic_to_writers<W1: std::io::Write, W2: std::io::Write>(
    plaintext_path: &Path,
    keychain: Option<KeychainStore>,
    stdout: &mut W1,
    stderr: &mut W2,
) -> Result<(), String> {
    // Single source of truth for word derivation; identity_hash comes back
    // alongside the words so we avoid re-parsing the mnemonic.
    let (words, id_hash) = export_mnemonic_words_with_keychain(plaintext_path, keychain)?;
    let phrase = words.join(" ");

    let map_err = |stream: &'static str| move |e: std::io::Error| format!("{stream}: {e}");

    writeln!(stderr, "*** Identity recovery mnemonic ***").map_err(map_err("stderr"))?;
    writeln!(stderr, "Write these 24 words on paper. Anyone with these")
        .map_err(map_err("stderr"))?;
    writeln!(stderr, "words can impersonate you. Storing in a digital")
        .map_err(map_err("stderr"))?;
    writeln!(stderr, "file is dangerous.").map_err(map_err("stderr"))?;
    writeln!(stderr).map_err(map_err("stderr"))?;
    writeln!(stderr, "identity-hash: {}", hex::encode(id_hash)).map_err(map_err("stderr"))?;

    writeln!(stdout, "{phrase}").map_err(map_err("stdout"))?;
    Ok(())
}

/// Read the seed from disk and convert to 24 BIP39 words.
///
/// Returns `(words, identity_hash_bytes)` — the hash bytes are derived
/// directly from the artifact so callers (e.g. `export_mnemonic_to_writers`
/// and the GUI's `export_mnemonic_words` Tauri command) do not need to
/// re-parse the mnemonic just to obtain the fingerprint.
///
/// Used by the GUI wizard so the words never touch a temp file. The CLI's
/// `export_mnemonic_to_writers` delegates here.
pub fn export_mnemonic_words_with_keychain(
    plaintext_path: &Path,
    keychain: Option<KeychainStore>,
) -> Result<(Vec<String>, [u8; 16]), String> {
    let seed = identity::read_seed_from_disk_with_keychain(plaintext_path, keychain)?;
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

/// Export the master seed as a passphrase-encrypted recovery file at `out`.
///
/// Reads the seed via the standard resolution chain. The recovery passphrase
/// is read from `HARMONY_RECOVERY_PASSPHRASE` / `HARMONY_RECOVERY_PASSPHRASE_FILE`
/// (DISTINCT from the at-rest `HARMONY_PASSPHRASE`).
///
/// Stdout: nothing. Stderr: `wrote <PATH> (<NN> bytes)\nidentity-hash: <hex32>`.
pub fn export_recovery_file_cli(
    plaintext_path: &Path,
    out: &Path,
    comment: Option<&str>,
    include_state: bool,
    force: bool,
) -> Result<(), String> {
    let harmony_dir = plaintext_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let result = export_recovery_file_pair_with_keychain(
        plaintext_path,
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
    plaintext_path: &Path,
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
    let seed = identity::read_seed_from_disk_with_keychain(plaintext_path, keychain)?;
    let artifact = RecoveryArtifact::from_seed(*seed);
    let metadata = RecoveryMetadata {
        mint_at: None,
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

/// Owner-state directory + filename convention.
///
/// Production wires this to `~/.harmony/`. Tests pass a tempdir-rooted path.
/// The owner-state file is the SAME path the production engine reads at
/// boot (`lib.rs` uses `app_data_dir.join("owner_state.cbor")`).
pub fn owner_state_path(harmony_dir: &Path) -> PathBuf {
    harmony_dir.join("owner_state.cbor")
}

pub fn last_backup_path(harmony_dir: &Path) -> PathBuf {
    harmony_dir.join("last_backup.json")
}

/// Compose the sidecar HRSS path next to `out`. Matches the spec
/// convention `<HRMR_PATH>.state`.
pub fn sidecar_path(out: &Path) -> PathBuf {
    let mut s = out.as_os_str().to_owned();
    s.push(".state");
    PathBuf::from(s)
}

/// Export the master seed + (optionally) an owner-state sidecar.
///
/// `include_state == true` AND owner-state file exists ⇒ emit pair.
/// `include_state == false` OR owner-state file absent ⇒ emit HRMR only.
/// Refuses if the sidecar destination exists and `force == false`.
/// On HRSS-write failure, best-effort removes the just-written HRMR
/// so the operator isn't stranded with a mismatched half-pair.
#[allow(clippy::too_many_arguments)]
pub fn export_recovery_file_pair_with_keychain(
    plaintext_path: &Path,
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

    // 1. Read seed + write HRMR (same as today's flow).
    let seed = identity::read_seed_from_disk_with_keychain(plaintext_path, keychain)?;
    let artifact = RecoveryArtifact::from_seed(*seed);
    let metadata = RecoveryMetadata {
        mint_at: None,
        comment: comment.map(str::to_string),
    };
    let bytes = artifact
        .to_encrypted_file(&passphrase, &metadata)
        .map_err(|e| e.to_string())?;
    let id_hash = artifact.master_pubkey_bundle().identity_hash();
    crate::identity::write_atomic_0600(out, &bytes)
        .map_err(|e| format!("failed to write {}: {e}", out.display()))?;

    // 2. If no sidecar wanted, we're done.
    if !want_sidecar {
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
    let state = match load_crdt(&state_path) {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_file(out);
            return Err(format!(
                "failed to load owner-state from {} (HRMR rolled back): {e}",
                state_path.display()
            ));
        }
    };
    use secrecy::ExposeSecret;
    let addr = derive_owner_addr_from_seed(&seed);
    let at = now_hlc();
    let hrss_bytes = encode_snapshot(
        passphrase.expose_secret().as_bytes(),
        addr,
        at.clone(),
        &state,
    )
    .map_err(|e| {
        // HRSS encode failure — best-effort cleanup of HRMR.
        let _ = std::fs::remove_file(out);
        format!("failed to encode state sidecar: {e} (HRMR rolled back)")
    })?;

    if let Err(e) = crate::identity::write_atomic_0600(&sidecar, &hrss_bytes) {
        let _ = std::fs::remove_file(out);
        return Err(format!(
            "failed to write {}: {e} (HRMR rolled back)",
            sidecar.display()
        ));
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
pub fn restore_recovery_file_pair_with_keychain(
    plaintext_path: &Path,
    harmony_dir: &Path,
    in_path: &Path,
    force: bool,
    ignore_state: bool,
    keychain: Option<KeychainStore>,
) -> Result<RestoreResult, String> {
    // Restore identity first — same as today's path.
    let bytes =
        std::fs::read(in_path).map_err(|e| format!("failed to read {}: {e}", in_path.display()))?;
    let passphrase = resolve_recovery_passphrase()?;
    let restored =
        RecoveryArtifact::from_encrypted_file(&bytes, &passphrase).map_err(|e| e.to_string())?;
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

    // Pre-flight: if a sidecar is being restored AND owner_state.cbor exists,
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
    identity::write_seed_to_disk_with_keychain(plaintext_path, &seed_bytes, force, keychain)?;

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
        if let Err(e) = crate::owner_state_persist::save_atomically(&state_path, &snap.tree) {
            return Err(format!(
                "identity restored but owner-state write failed: {e}"
            ));
        }
        // Best-effort reload to count Spaces for the confirmation message.
        // A reload failure here MUST NOT convert a successful state save
        // into surfaced Err (per metadata-before-irreversible-write); we
        // degrade gracefully to "0 spaces" in the stderr report.
        load_crdt(&state_path)
            .ok()
            .map(|s| s.spaces.len())
            .unwrap_or(0)
    } else {
        0
    };

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

/// Restore the master seed from a 24-word mnemonic file.
///
/// Reads the mnemonic from `mnemonic_file` (whitespace-tolerant,
/// case-insensitive, ASCII-only — non-ASCII rejected). Writes the seed via
/// the standard resolution chain. Refuses if an identity already exists
/// unless `force` is true.
///
/// Stdout: nothing. Stderr: `restored identity-hash: <hex32>`.
pub fn restore_mnemonic_cli(
    plaintext_path: &Path,
    mnemonic_file: &Path,
    force: bool,
) -> Result<(), String> {
    restore_mnemonic_with_keychain(
        plaintext_path,
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
    plaintext_path: &Path,
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
    identity::write_seed_to_disk_with_keychain(plaintext_path, &seed_bytes, force, keychain)
        .map_err(|e| e.to_string())
}

/// Inner entry point — accepts an injected keychain so tests can stay
/// hermetic. Production callers go through [`restore_mnemonic_cli`].
pub fn restore_mnemonic_with_keychain(
    plaintext_path: &Path,
    mnemonic_file: &Path,
    force: bool,
    keychain: Option<KeychainStore>,
) -> Result<(), String> {
    // Read the mnemonic file. Wrap in Zeroizing so the contents do not linger.
    let raw = std::fs::read_to_string(mnemonic_file)
        .map_err(|e| format!("failed to read {}: {e}", mnemonic_file.display()))?;
    let raw = Zeroizing::new(raw);

    let words: Vec<String> = raw.split_whitespace().map(String::from).collect();
    restore_mnemonic_from_words_with_keychain(plaintext_path, &words, force, keychain)?;
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

/// Restore the master seed from a passphrase-encrypted recovery file.
///
/// Reads the encrypted file from `in_path`. Decrypts using the recovery
/// passphrase (`HARMONY_RECOVERY_PASSPHRASE` / `_FILE`). Writes the seed
/// via the standard resolution chain (using the at-rest
/// `HARMONY_PASSPHRASE` / `_FILE` for re-encryption). Refuses if an
/// identity already exists unless `force` is true.
///
/// Stdout: nothing. Stderr: `restored identity-hash: <hex32>`.
pub fn restore_recovery_file_cli(
    plaintext_path: &Path,
    in_path: &Path,
    force: bool,
    ignore_state: bool,
) -> Result<(), String> {
    let harmony_dir = plaintext_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    restore_recovery_file_pair_with_keychain(
        plaintext_path,
        &harmony_dir,
        in_path,
        force,
        ignore_state,
        KeychainStore::new().ok(),
    )
    .map(|_| ())
}

/// Inner entry point — accepts an injected keychain so tests can stay
/// hermetic. Production callers go through [`restore_recovery_file_cli`].
pub fn restore_recovery_file_with_keychain(
    plaintext_path: &Path,
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

    identity::write_seed_to_disk_with_keychain(plaintext_path, &seed_bytes, force, keychain)?;
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
        let plaintext_path = dir.path().join("identity.key");
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
            &plaintext_path,
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
        let plaintext_path = dir.path().join("identity.key");
        let recovery_out = dir.path().join("recovery.bin");

        // Plant a known seed via the at-rest passphrase env var. The
        // write_seed_to_disk_with_keychain helper resolves the encrypted-file
        // backend from HARMONY_PASSPHRASE internally.
        std::env::set_var("HARMONY_PASSPHRASE", "at-rest-pass");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery-pass");
        identity::write_seed_to_disk_with_keychain(
            &plaintext_path,
            &[0xCAu8; 32],
            /*force=*/ true,
            None,
        )
        .unwrap();

        // Use the keychain-injected variant with `None` — keeps the test
        // hermetic and prevents any read/write to the developer's real OS
        // keychain entry.
        export_recovery_file_with_keychain(
            &plaintext_path,
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
    fn restore_mnemonic_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");
        let mnemonic_path = dir.path().join("mnemonic.txt");

        std::env::set_var("HARMONY_PASSPHRASE", "restore-test");
        let original = RecoveryArtifact::from_seed([0xEFu8; 32]);
        std::fs::write(&mnemonic_path, original.to_mnemonic().as_str()).unwrap();
        let original_id = original.master_pubkey_bundle().identity_hash();

        restore_mnemonic_with_keychain(
            &plaintext_path,
            &mnemonic_path,
            /*force=*/ false,
            None,
        )
        .expect("restore");

        let reloaded_seed =
            identity::read_seed_from_disk_with_keychain(&plaintext_path, None).unwrap();
        let reloaded = RecoveryArtifact::from_seed(*reloaded_seed);
        assert_eq!(reloaded.master_pubkey_bundle().identity_hash(), original_id);

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn restore_refuses_when_identity_exists_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");
        let mnemonic_path = dir.path().join("mnemonic.txt");

        std::env::set_var("HARMONY_PASSPHRASE", "refuse-test");
        let original = RecoveryArtifact::from_seed([0x12u8; 32]);
        std::fs::write(&mnemonic_path, original.to_mnemonic().as_str()).unwrap();
        // Plant an existing identity.
        identity::write_seed_to_disk_with_keychain(
            &plaintext_path,
            &[0x99u8; 32],
            /*force=*/ true,
            None,
        )
        .unwrap();

        let err = restore_mnemonic_with_keychain(
            &plaintext_path,
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
        let plaintext_path = dir.path().join("identity.key");
        let mnemonic_path = dir.path().join("mnemonic.txt");

        std::env::set_var("HARMONY_PASSPHRASE", "force-test");
        let original = RecoveryArtifact::from_seed([0xDDu8; 32]);
        std::fs::write(&mnemonic_path, original.to_mnemonic().as_str()).unwrap();
        let original_id = original.master_pubkey_bundle().identity_hash();
        // Plant a different existing identity.
        identity::write_seed_to_disk_with_keychain(
            &plaintext_path,
            &[0x77u8; 32],
            /*force=*/ true,
            None,
        )
        .unwrap();

        restore_mnemonic_with_keychain(&plaintext_path, &mnemonic_path, /*force=*/ true, None)
            .expect("force succeeds");
        let reloaded_seed =
            identity::read_seed_from_disk_with_keychain(&plaintext_path, None).unwrap();
        let reloaded = RecoveryArtifact::from_seed(*reloaded_seed);
        assert_eq!(reloaded.master_pubkey_bundle().identity_hash(), original_id);

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn export_mnemonic_writes_warning_to_stderr_and_words_to_stdout() {
        use crate::identity;
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");

        std::env::set_var("HARMONY_PASSPHRASE", "mnemonic-export-test");
        let planted = [0xA7u8; 32];
        identity::write_seed_to_disk_with_keychain(
            &plaintext_path,
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
        export_mnemonic_to_writers(&plaintext_path, None, &mut stdout, &mut stderr)
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
        // markers.
        assert!(
            stderr_str.contains("Identity recovery mnemonic"),
            "stderr must include warning preamble; got: {stderr_str:?}"
        );
        assert!(
            stderr_str.contains("identity-hash:"),
            "stderr must include identity-hash; got: {stderr_str:?}"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    use crate::owner_state_crdt::OwnerState;

    /// Plant a usable owner-state file at `harmony_dir/owner_state.cbor`.
    fn plant_owner_state(harmony_dir: &Path) {
        let state = OwnerState::default();
        let state_path = super::owner_state_path(harmony_dir);
        crate::owner_state_persist::save_crdt(&state_path, &state).unwrap();
    }

    #[test]
    #[serial]
    fn export_emits_pair_when_state_exists() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");
        identity::write_seed_to_disk_with_keychain(&plaintext_path, &[0xCA; 32], true, None)
            .unwrap();
        plant_owner_state(dir.path());

        let result = super::export_recovery_file_pair_with_keychain(
            &plaintext_path,
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
        let plaintext_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");
        identity::write_seed_to_disk_with_keychain(&plaintext_path, &[0xCA; 32], true, None)
            .unwrap();
        // No plant_owner_state — owner_state.cbor absent.

        let result = super::export_recovery_file_pair_with_keychain(
            &plaintext_path,
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
        let plaintext_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");
        identity::write_seed_to_disk_with_keychain(&plaintext_path, &[0xCA; 32], true, None)
            .unwrap();
        plant_owner_state(dir.path());

        let result = super::export_recovery_file_pair_with_keychain(
            &plaintext_path,
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
        let plaintext_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");
        identity::write_seed_to_disk_with_keychain(&plaintext_path, &[0xCA; 32], true, None)
            .unwrap();
        plant_owner_state(dir.path());
        std::fs::write(super::sidecar_path(&out), b"stale").unwrap();

        let err = super::export_recovery_file_pair_with_keychain(
            &plaintext_path,
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
        let plaintext_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");
        let seed = [0xCA; 32];
        identity::write_seed_to_disk_with_keychain(&plaintext_path, &seed, true, None).unwrap();
        plant_owner_state(dir.path());
        super::export_recovery_file_pair_with_keychain(
            &plaintext_path,
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
        let _ = std::fs::remove_file(&plaintext_path);
        let _ = std::fs::remove_file(super::owner_state_path(dir.path()));
        let result = super::restore_recovery_file_pair_with_keychain(
            &plaintext_path,
            dir.path(),
            &out,
            /*force=*/ true,
            /*ignore_state=*/ false,
            None,
        )
        .expect("restore");
        assert!(result.sidecar_present);
        // Identity round-trip.
        let reloaded = identity::read_seed_from_disk_with_keychain(&plaintext_path, None).unwrap();
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
        let plaintext_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");
        identity::write_seed_to_disk_with_keychain(&plaintext_path, &[0x42; 32], true, None)
            .unwrap();
        // Skip plant_owner_state — HRSS will not be emitted.
        super::export_recovery_file_pair_with_keychain(
            &plaintext_path,
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

        let _ = std::fs::remove_file(&plaintext_path);
        let result = super::restore_recovery_file_pair_with_keychain(
            &plaintext_path,
            dir.path(),
            &out,
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
        let plaintext_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");
        identity::write_seed_to_disk_with_keychain(&plaintext_path, &[0x42; 32], true, None)
            .unwrap();
        plant_owner_state(dir.path());
        super::export_recovery_file_pair_with_keychain(
            &plaintext_path,
            dir.path(),
            &out,
            None,
            None,
            true,
            true,
            None,
        )
        .unwrap();

        let _ = std::fs::remove_file(&plaintext_path);
        let _ = std::fs::remove_file(super::owner_state_path(dir.path()));
        let result = super::restore_recovery_file_pair_with_keychain(
            &plaintext_path,
            dir.path(),
            &out,
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
        let plaintext_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");
        identity::write_seed_to_disk_with_keychain(&plaintext_path, &[0x42; 32], true, None)
            .unwrap();
        plant_owner_state(dir.path());
        super::export_recovery_file_pair_with_keychain(
            &plaintext_path,
            dir.path(),
            &out,
            None,
            None,
            true,
            true,
            None,
        )
        .unwrap();

        // Plant a different owner_state.cbor to force a "exists" collision.
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
        };
        state.spaces.insert(sp.id, sp);
        crate::owner_state_persist::save_crdt(&super::owner_state_path(dir.path()), &state)
            .unwrap();

        // force=true overwrites.
        super::restore_recovery_file_pair_with_keychain(
            &plaintext_path,
            dir.path(),
            &out,
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
        let plaintext_path_a = dir.path().join("a.key");
        let plaintext_path_b = dir.path().join("b.key");
        let out_a = dir.path().join("a.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");

        // Build owner A's identity + state + sidecar.
        identity::write_seed_to_disk_with_keychain(&plaintext_path_a, &[0xAA; 32], true, None)
            .unwrap();
        plant_owner_state(dir.path());
        super::export_recovery_file_pair_with_keychain(
            &plaintext_path_a,
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
        identity::write_seed_to_disk_with_keychain(&plaintext_path_b, &[0xBB; 32], true, None)
            .unwrap();
        // Now overwrite a.bin's HRMR with B's identity but keep A's HRSS.
        // Easiest: emit B's identity-only backup at out_a, then keep A's
        // sidecar at out_a.state.
        let out_b = dir.path().join("b.bin");
        super::export_recovery_file_pair_with_keychain(
            &plaintext_path_b,
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

        let _ = std::fs::remove_file(&plaintext_path_a);
        let err = super::restore_recovery_file_pair_with_keychain(
            &plaintext_path_a,
            dir.path(),
            &out_a,
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
        let plaintext_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");
        identity::write_seed_to_disk_with_keychain(&plaintext_path, &[0xCA; 32], true, None)
            .unwrap();
        plant_owner_state(dir.path());
        super::export_recovery_file_pair_with_keychain(
            &plaintext_path,
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

    #[test]
    #[serial]
    fn restore_refuses_state_overwrite_without_force_leaves_identity_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");

        // Plant identity A + state.
        identity::write_seed_to_disk_with_keychain(&plaintext_path, &[0xAA; 32], true, None)
            .unwrap();
        plant_owner_state(dir.path());
        super::export_recovery_file_pair_with_keychain(
            &plaintext_path,
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
        // owner_state.cbor that would block the sidecar restore.
        identity::write_seed_to_disk_with_keychain(&plaintext_path, &[0xBB; 32], true, None)
            .unwrap();
        let identity_before =
            identity::read_seed_from_disk_with_keychain(&plaintext_path, None).unwrap();
        // owner_state.cbor still exists from plant_owner_state above.

        // Restore without --force -- must refuse and leave identity B untouched.
        let err = super::restore_recovery_file_pair_with_keychain(
            &plaintext_path,
            dir.path(),
            &out,
            /*force=*/ false,
            /*ignore_state=*/ false,
            None,
        )
        .expect_err("must refuse when owner_state.cbor exists without --force");
        assert!(
            err.contains("owner-state file already exists") && err.contains("--force"),
            "expected metadata refusal: {err}"
        );

        // Identity B's seed must still be on disk — NOT overwritten with A's seed.
        let identity_after =
            identity::read_seed_from_disk_with_keychain(&plaintext_path, None).unwrap();
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
        let plaintext_path = dir.path().join("identity.key");
        let out = dir.path().join("recovery.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "at-rest");
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "recovery");

        // Plant + export from this dir.
        identity::write_seed_to_disk_with_keychain(&plaintext_path, &[0xCC; 32], true, None)
            .unwrap();
        plant_owner_state(dir.path());
        let export_result = super::export_recovery_file_pair_with_keychain(
            &plaintext_path,
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
}
