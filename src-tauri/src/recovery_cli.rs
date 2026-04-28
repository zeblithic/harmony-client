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

use crate::identity::{self, KeychainStore};

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
) -> Result<(), String> {
    export_recovery_file_with_keychain(
        plaintext_path,
        out,
        comment,
        /*passphrase=*/ None,
        KeychainStore::new().ok(),
    )
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
) -> Result<(), String> {
    restore_recovery_file_with_keychain(plaintext_path, in_path, force, KeychainStore::new().ok())
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
}
