//! CLI subcommand entry points for identity backup/restore.
//!
//! Each entry point composes [`crate::identity::read_seed_from_disk`] /
//! [`crate::identity::write_seed_to_disk`] with the appropriate
//! [`harmony_owner::recovery`] API. The recovery passphrase
//! (`HARMONY_RECOVERY_PASSPHRASE` / `HARMONY_RECOVERY_PASSPHRASE_FILE`) is
//! resolved separately from the at-rest passphrase
//! (`HARMONY_PASSPHRASE` / `HARMONY_PASSPHRASE_FILE`) — neither variable
//! falls back to the other.

use std::path::Path;

use harmony_owner::lifecycle::RecoveryArtifact;
use harmony_owner::recovery::RecoveryMetadata;
use secrecy::SecretString;
use zeroize::Zeroizing;

use crate::identity;

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
    let seed = identity::read_seed_from_disk(plaintext_path)?;
    let artifact = RecoveryArtifact::from_seed(*seed);
    let mnemonic = artifact.to_mnemonic();
    let id_hash = artifact.master_pubkey_bundle().identity_hash();

    eprintln!("*** Identity recovery mnemonic ***");
    eprintln!("Write these 24 words on paper. Anyone with these");
    eprintln!("words can impersonate you. Storing in a digital");
    eprintln!("file is dangerous.");
    eprintln!();
    eprintln!("identity-hash: {}", hex::encode(id_hash));

    println!("{}", mnemonic.as_str());
    Ok(())
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
) -> Result<(), String> {
    let seed = identity::read_seed_from_disk(plaintext_path)?;
    let passphrase = resolve_recovery_passphrase()?;
    let artifact = RecoveryArtifact::from_seed(*seed);
    let metadata = RecoveryMetadata {
        mint_at: None,
        comment: comment.map(str::to_string),
    };
    let bytes = artifact
        .to_encrypted_file(&passphrase, &metadata)
        .map_err(|e| format!("Error: {e}"))?;
    let id_hash = artifact.master_pubkey_bundle().identity_hash();

    std::fs::write(out, &bytes)
        .map_err(|e| format!("Error: failed to write {}: {e}", out.display()))?;

    eprintln!("wrote {} ({} bytes)", out.display(), bytes.len());
    eprintln!("identity-hash: {}", hex::encode(id_hash));
    Ok(())
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
    // Read the mnemonic file. Wrap in Zeroizing so the contents do not linger.
    let raw = std::fs::read_to_string(mnemonic_file)
        .map_err(|e| format!("Error: failed to read {}: {e}", mnemonic_file.display()))?;
    let raw = Zeroizing::new(raw);

    let artifact = RecoveryArtifact::from_mnemonic(raw.as_str())
        .map_err(|e| format!("Error: {e}"))?;
    let seed_bytes: Zeroizing<[u8; 32]> = Zeroizing::new(*artifact.as_bytes());
    let id_hash = artifact.master_pubkey_bundle().identity_hash();

    identity::write_seed_to_disk(plaintext_path, &seed_bytes, force)?;
    eprintln!("restored identity-hash: {}", hex::encode(id_hash));
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
) -> Result<(), String> {
    let bytes = std::fs::read(in_path)
        .map_err(|e| format!("Error: failed to read {}: {e}", in_path.display()))?;
    let passphrase = resolve_recovery_passphrase()?;
    let restored = RecoveryArtifact::from_encrypted_file(&bytes, &passphrase)
        .map_err(|e| format!("Error: {e}"))?;
    let artifact = restored.into_artifact();
    let seed_bytes: Zeroizing<[u8; 32]> = Zeroizing::new(*artifact.as_bytes());
    let id_hash = artifact.master_pubkey_bundle().identity_hash();

    identity::write_seed_to_disk(plaintext_path, &seed_bytes, force)?;
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

        export_recovery_file_cli(&plaintext_path, &recovery_out, Some("test")).expect("export");
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

        restore_mnemonic_cli(&plaintext_path, &mnemonic_path, /*force=*/ false).expect("restore");

        let reloaded_seed = identity::read_seed_from_disk(&plaintext_path).unwrap();
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

        let err = restore_mnemonic_cli(&plaintext_path, &mnemonic_path, /*force=*/ false)
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

        restore_mnemonic_cli(&plaintext_path, &mnemonic_path, /*force=*/ true).expect("force succeeds");
        let reloaded_seed = identity::read_seed_from_disk(&plaintext_path).unwrap();
        let reloaded = RecoveryArtifact::from_seed(*reloaded_seed);
        assert_eq!(reloaded.master_pubkey_bundle().identity_hash(), original_id);

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn export_mnemonic_round_trips_via_recovery_artifact() {
        use crate::identity;

        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");

        // Plant a known seed.
        std::env::set_var("HARMONY_PASSPHRASE", "mnemonic-export-test");
        let planted = [0xA7u8; 32];
        identity::write_seed_to_disk_with_keychain(
            &plaintext_path,
            &planted,
            /*force=*/ true,
            None,
        )
        .unwrap();

        // Call the CLI entry point. We cannot capture stdout/stderr from the
        // unit test directly without process indirection, but we can confirm
        // the function returns Ok and that the seed-from-disk derives an
        // artifact whose mnemonic round-trips back to the same seed.
        export_mnemonic_cli(&plaintext_path).expect("export must succeed");

        // Re-derive the artifact and verify the mnemonic encodes back to the
        // planted seed — this is the behavioral contract export_mnemonic_cli
        // promises to operators.
        let seed = identity::read_seed_from_disk(&plaintext_path).unwrap();
        let artifact = RecoveryArtifact::from_seed(*seed);
        let mnemonic = artifact.to_mnemonic();
        let parsed = RecoveryArtifact::from_mnemonic(mnemonic.as_str()).unwrap();
        assert_eq!(*parsed.as_bytes(), planted);

        std::env::remove_var("HARMONY_PASSPHRASE");
    }
}
