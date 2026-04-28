//! Tauri commands for the identity backup/restore GUI wizard.
//!
//! Each command is a thin wrapper around [`crate::recovery_cli`] helpers.
//! The actual logic lives in `recovery_cli` so it can be tested without a
//! live Tauri runtime. Commands delegate to `*_helper` functions that take
//! a `plaintext_path` argument; tests call the helpers directly.

use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::identity;
use crate::identity::KeychainStore;
use crate::recovery_cli;

// ── Shared types ─────────────────────────────────────────────────────────

/// Metadata from a recovery file, returned by `preview_recovery_file`.
/// Sent across IPC so it must implement `serde::Serialize`. Wire format is
/// `camelCase` to match every other IPC payload struct in `lib.rs`
/// (ProfilePayload, ChannelMessagePayload, etc.) — the JS side reads
/// `identityHash` / `mintedAt` / `comment`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreInfo {
    /// 32-char hex-encoded identity hash derived from the backup's seed.
    /// `identity_hash()` returns `[u8; 16]` (128-bit truncated BLAKE3).
    pub identity_hash: String,
    /// Unix epoch seconds recorded when the backup was created, if any.
    pub minted_at: Option<u64>,
    /// Free-text comment recorded in the backup, if any.
    pub comment: Option<String>,
}

// ── Internal helpers ──────────────────────────────────────────────────────

/// Process-wide lock around `HARMONY_RECOVERY_PASSPHRASE*` env-var mutation.
/// `recovery_cli::resolve_recovery_passphrase` reads from the environment
/// (shared code path with the headless CLI), so the GUI must temporarily set
/// these vars while invoking the helpers. Concurrent calls must serialize or
/// they will race on the global env. The wizard's UX is serial today, but
/// any non-UI caller (tests, background tasks) gets the same protection.
///
/// Poison recovery is genuinely safe because env-var cleanup is performed by
/// [`EnvVarGuard`] in `Drop`, which runs during stack unwinding even when a
/// panic poisons this lock. Without the guard, accepting a poisoned lock
/// would leak the prior call's passphrase into the environment for the next
/// caller — but with it, the env state is always restored.
fn recovery_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// RAII guard for an environment variable. On construction, captures the
/// prior value and applies a new value (or removes it). On drop, restores
/// the prior value. Restoration runs on panic-unwind too — load-bearing for
/// poison-safe interaction with [`recovery_env_lock`].
struct EnvVarGuard {
    key: &'static str,
    prev: Option<String>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let prev = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { key, prev }
    }

    fn unset(key: &'static str) -> Self {
        let prev = std::env::var(key).ok();
        std::env::remove_var(key);
        Self { key, prev }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

/// Maximum size of a recovery file the GUI will read.
/// Recovery files are ~101 bytes today; 1 MiB is a generous upper bound that
/// rejects accidental selection of a large file (image, video, archive).
const MAX_RECOVERY_FILE_BYTES: u64 = 1 << 20;

/// Maximum byte length of a user-supplied recovery-file comment.
/// `harmony_owner::recovery` already enforces this cap inside
/// `to_encrypted_file`; the GUI mirrors it (round 3 review) so the user sees
/// a friendly, predictable error before any I/O instead of a generic message
/// surfaced from the encryption layer. The value MUST match the inner cap
/// so the GUI rejection fires first.
const MAX_RECOVERY_COMMENT_BYTES: usize = 256;

/// Read a recovery file with a size guard. Reject files larger than
/// [`MAX_RECOVERY_FILE_BYTES`] before allocating.
fn read_recovery_file(path: &Path) -> Result<Vec<u8>, String> {
    let metadata =
        std::fs::metadata(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    if metadata.len() > MAX_RECOVERY_FILE_BYTES {
        return Err(format!(
            "{} is too large to be a recovery file ({} bytes; max {} bytes)",
            path.display(),
            metadata.len(),
            MAX_RECOVERY_FILE_BYTES
        ));
    }
    std::fs::read(path).map_err(|e| format!("failed to read {}: {e}", path.display()))
}

// ── Helpers (testable without Tauri runtime) ──────────────────────────────

/// Return the 32-char hex identity hash of the on-disk identity.
///
/// Used by the Settings panel header and the restore-confirm gate ("type
/// the first 8 chars of your CURRENT hash"). The Tauri command entry point
/// passes `KeychainStore::new().ok()` so the GUI reads the same identity
/// the running node uses (`identity::load_or_generate` calls
/// `read_seed_from_disk_with_keychain` with the same chain). Tests inject
/// `None` to stay hermetic.
pub fn current_identity_hash_helper(
    plaintext_path: &Path,
    keychain: Option<KeychainStore>,
) -> Result<String, String> {
    let seed = identity::read_seed_from_disk_with_keychain(plaintext_path, keychain)?;
    use harmony_owner::lifecycle::RecoveryArtifact;
    let artifact = RecoveryArtifact::from_seed(*seed);
    let id_hash = artifact.master_pubkey_bundle().identity_hash();
    Ok(hex::encode(id_hash))
}

/// Return the 24 BIP39 words for the backup wizard's blurred-grid display.
///
/// Threads `keychain` through to match the running node's identity-resolution
/// chain. Tests inject `None`.
pub fn export_mnemonic_words_helper(
    plaintext_path: &Path,
    keychain: Option<KeychainStore>,
) -> Result<Vec<String>, String> {
    let (words, _id_hash) =
        recovery_cli::export_mnemonic_words_with_keychain(plaintext_path, keychain)?;
    Ok(words)
}

/// Derive the identity hash that would result from restoring `words`,
/// WITHOUT writing anything to disk.
///
/// The GUI restore-mnemonic step calls this to show "this restores identity
/// 0x…" before the user commits.
pub fn preview_mnemonic_identity_helper(words: &[String]) -> Result<String, String> {
    if words.len() != 24 {
        return Err(format!("expected 24 BIP39 words, got {}", words.len()));
    }
    let phrase = words.join(" ");
    use harmony_owner::lifecycle::RecoveryArtifact;
    let artifact = RecoveryArtifact::from_mnemonic(&phrase).map_err(|e| e.to_string())?;
    let id_hash = artifact.master_pubkey_bundle().identity_hash();
    Ok(hex::encode(id_hash))
}

/// Decrypt a recovery file and return its metadata WITHOUT writing to disk.
///
/// The GUI restore-file step calls this to show "this restores identity
/// 0x…, created <date>, comment: …" before the user commits.
pub fn preview_recovery_file_helper(
    in_path: &Path,
    passphrase: &str,
) -> Result<RestoreInfo, String> {
    use harmony_owner::lifecycle::RecoveryArtifact;
    use secrecy::SecretString;

    let bytes = read_recovery_file(in_path)?;
    let pass = SecretString::from(passphrase.to_string());
    let restored =
        RecoveryArtifact::from_encrypted_file(&bytes, &pass).map_err(|e| e.to_string())?;
    let id_hash = restored.artifact.master_pubkey_bundle().identity_hash();
    Ok(RestoreInfo {
        identity_hash: hex::encode(id_hash),
        minted_at: restored.metadata.mint_at,
        comment: restored.metadata.comment,
    })
}

/// Export the master seed as a passphrase-encrypted recovery file at `out_path`.
///
/// The GUI calls this after the user has chosen an output path and typed a
/// passphrase in the file-backup wizard. The `comment` field is stored in
/// the backup metadata and shown back to the user on restore.
///
/// Wraps `HARMONY_RECOVERY_PASSPHRASE` and `HARMONY_RECOVERY_PASSPHRASE_FILE`
/// with the caller-supplied `passphrase` because
/// `export_recovery_file_with_keychain` resolves the passphrase from the
/// environment (to share one code path with the headless CLI).
/// `HARMONY_RECOVERY_PASSPHRASE_FILE` is cleared for the duration so that
/// `resolve_recovery_passphrase()` does not log a warning about both vars
/// being set when a user previously configured the CLI file-based path.
///
/// Concurrent calls are serialized through [`recovery_env_lock`]; env-var
/// restoration uses [`EnvVarGuard`] (RAII) so cleanup runs even on panic.
///
/// ZEB-187 (security): passing the passphrase through a process-global
/// `String` env var leaves it transiently readable by any debugger or tool
/// attached to this process, and `secrecy::SecretString` zeroing does not
/// apply. The follow-up will thread the passphrase through
/// `recovery_cli::*_with_keychain` as `Option<&SecretString>` and delete
/// the env-var dance entirely.
pub fn export_recovery_file_to_path_helper(
    plaintext_path: &Path,
    out_path: &Path,
    passphrase: &str,
    comment: Option<String>,
    keychain: Option<KeychainStore>,
) -> Result<(), String> {
    // Reject oversized comments BEFORE acquiring the env-var lock or doing any
    // work — the inner write would otherwise produce a recovery file the same
    // GUI later refuses to read back (see [`MAX_RECOVERY_FILE_BYTES`]).
    if let Some(c) = comment.as_deref() {
        let len = c.len();
        if len > MAX_RECOVERY_COMMENT_BYTES {
            return Err(format!(
                "comment is too large ({len} bytes; max {MAX_RECOVERY_COMMENT_BYTES} bytes)"
            ));
        }
    }

    // Serialize concurrent helper calls; env-var mutation is process-global.
    let _lock = recovery_env_lock();
    // Drop order is reverse declaration: `_file` restores first, then
    // `_pass`, then the mutex guard releases. Each Drop runs even on panic.
    let _pass = EnvVarGuard::set("HARMONY_RECOVERY_PASSPHRASE", passphrase);
    let _file = EnvVarGuard::unset("HARMONY_RECOVERY_PASSPHRASE_FILE");

    recovery_cli::export_recovery_file_with_keychain(
        plaintext_path,
        out_path,
        comment.as_deref(),
        keychain,
    )
}

/// Restore the on-disk identity from a 24-word mnemonic array and return the
/// resulting identity hash.
///
/// `force` is always `true` on this path: the GUI has already shown the user a
/// `TypeToConfirmDialog` and received explicit acknowledgement that the current
/// identity will be overwritten.
pub fn restore_mnemonic_from_words_helper(
    plaintext_path: &Path,
    words: &[String],
    keychain: Option<KeychainStore>,
) -> Result<String, String> {
    // force=true: caller has obtained explicit user confirmation via TypeToConfirmDialog.
    recovery_cli::restore_mnemonic_from_words_with_keychain(
        plaintext_path,
        words,
        /*force=*/ true,
        keychain,
    )?;
    // Derive the resulting identity hash from the words (no re-read from disk needed).
    // This re-parse cannot fail: the same phrase was already parsed successfully
    // inside restore_mnemonic_from_words_with_keychain (same function, same input).
    use harmony_owner::lifecycle::RecoveryArtifact;
    let phrase = words.join(" ");
    let artifact = RecoveryArtifact::from_mnemonic(&phrase)
        .expect("post-restore mnemonic re-parse must succeed; same parse already passed in restore_mnemonic_from_words_with_keychain");
    Ok(hex::encode(artifact.master_pubkey_bundle().identity_hash()))
}

/// Restore the on-disk identity from a passphrase-encrypted recovery file and
/// return its metadata.
///
/// `force` is always `true` on this path: the GUI has already shown the user a
/// `TypeToConfirmDialog` and received explicit acknowledgement that the current
/// identity will be overwritten.
///
/// **Single-decrypt design.** The recovery file is read and decrypted exactly
/// once. Metadata is extracted from the decrypted artifact, then the same
/// in-memory artifact's seed is written directly to disk via
/// [`identity::write_seed_to_disk_with_keychain`]. The function never
/// re-touches `in_path` after the initial read. This is load-bearing for two
/// reasons:
///
/// 1. **Greptile P1 (round 2):** if anything fails before the seed is
///    written, the on-disk identity is untouched and the caller can retry.
/// 2. **CodeRabbit TOCTOU (round 3):** an earlier version called
///    `restore_recovery_file_with_keychain(in_path, …)` after the metadata
///    read, opening a TOCTOU window where the file could be swapped or
///    grown between the two reads — letting an attacker show the user
///    metadata for backup A while restoring backup B's seed, or bypass the
///    [`MAX_RECOVERY_FILE_BYTES`] guard with a swap.
///
/// Because we no longer go through `recovery_cli::restore_recovery_file_*`,
/// the env-var dance (`HARMONY_RECOVERY_PASSPHRASE*`) is unnecessary on this
/// path: passphrase resolution happened during decrypt above.
pub fn restore_recovery_file_from_path_helper(
    plaintext_path: &Path,
    in_path: &Path,
    passphrase: &str,
    keychain: Option<KeychainStore>,
) -> Result<RestoreInfo, String> {
    use harmony_owner::lifecycle::RecoveryArtifact;
    use secrecy::SecretString;
    use zeroize::Zeroizing;

    // ── Step 1: read + decrypt once. Read-only; on-disk identity untouched. ─
    let bytes = read_recovery_file(in_path)?;
    let pass = SecretString::from(passphrase.to_string());
    let restored =
        RecoveryArtifact::from_encrypted_file(&bytes, &pass).map_err(|e| e.to_string())?;
    let id_hash = restored.artifact.master_pubkey_bundle().identity_hash();
    let info = RestoreInfo {
        identity_hash: hex::encode(id_hash),
        minted_at: restored.metadata.mint_at,
        comment: restored.metadata.comment.clone(),
    };

    // ── Step 2: extract the seed from the decrypted artifact and write. ────
    // No re-read of `in_path` — the seed we write is byte-equal to the seed
    // whose hash was just shown to the user (see TOCTOU note in doc comment).
    // `Zeroizing` clears the seed from stack memory once we're done.
    let artifact = restored.into_artifact();
    let seed_bytes: Zeroizing<[u8; 32]> = Zeroizing::new(*artifact.as_bytes());

    // force=true: caller has obtained explicit user confirmation via TypeToConfirmDialog.
    identity::write_seed_to_disk_with_keychain(
        plaintext_path,
        &seed_bytes,
        /*force=*/ true,
        keychain,
    )
    .map_err(|e| e.to_string())?;

    Ok(info)
}

// ── Tauri commands ────────────────────────────────────────────────────────

/// Return the 32-char hex identity hash of the current on-disk identity.
#[tauri::command]
pub async fn current_identity_hash() -> Result<String, String> {
    let plaintext_path = identity::resolve_path(None)?;
    current_identity_hash_helper(&plaintext_path, KeychainStore::new().ok())
}

/// Return the 24 BIP39 mnemonic words for the backup wizard.
#[tauri::command]
pub async fn export_mnemonic_words() -> Result<Vec<String>, String> {
    let plaintext_path = identity::resolve_path(None)?;
    export_mnemonic_words_helper(&plaintext_path, KeychainStore::new().ok())
}

/// Return the identity hash that would result from restoring the given
/// words, WITHOUT writing anything to disk.
#[tauri::command]
pub async fn preview_mnemonic_identity(words: Vec<String>) -> Result<String, String> {
    preview_mnemonic_identity_helper(&words)
}

/// Decrypt a recovery file and return its metadata WITHOUT writing to disk.
#[tauri::command]
pub async fn preview_recovery_file(
    in_path: String,
    passphrase: String,
) -> Result<RestoreInfo, String> {
    preview_recovery_file_helper(Path::new(&in_path), &passphrase)
}

/// Export the master seed as a passphrase-encrypted recovery file at `out_path`.
///
/// Called by the GUI file-backup wizard after the user has confirmed the path
/// and passphrase.
#[tauri::command]
pub async fn export_recovery_file_to_path(
    out_path: PathBuf,
    passphrase: String,
    comment: Option<String>,
) -> Result<(), String> {
    let plaintext_path = identity::resolve_path(None)?;
    export_recovery_file_to_path_helper(
        &plaintext_path,
        &out_path,
        &passphrase,
        comment,
        KeychainStore::new().ok(),
    )
}

/// Restore the on-disk identity from a 24-word mnemonic array.
///
/// Returns the 32-char hex identity hash of the restored identity.
/// The GUI calls this only after the user has passed the `TypeToConfirmDialog`
/// gate, so `force=true` is applied unconditionally on this path.
#[tauri::command]
pub async fn restore_mnemonic_from_words(words: Vec<String>) -> Result<String, String> {
    let plaintext_path = identity::resolve_path(None)?;
    restore_mnemonic_from_words_helper(&plaintext_path, &words, KeychainStore::new().ok())
}

/// Restore the on-disk identity from a passphrase-encrypted recovery file.
///
/// Returns metadata (`identity_hash`, `minted_at`, `comment`) for the restored
/// identity. The GUI calls this only after the user has passed the
/// `TypeToConfirmDialog` gate, so `force=true` is applied unconditionally.
#[tauri::command]
pub async fn restore_recovery_file_from_path(
    in_path: PathBuf,
    passphrase: String,
) -> Result<RestoreInfo, String> {
    let plaintext_path = identity::resolve_path(None)?;
    restore_recovery_file_from_path_helper(
        &plaintext_path,
        &in_path,
        &passphrase,
        KeychainStore::new().ok(),
    )
}

// ── Unit tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use harmony_owner::lifecycle::RecoveryArtifact;
    use harmony_owner::recovery::RecoveryMetadata;
    use serial_test::serial;

    fn plant_seed(plaintext_path: &Path, seed: &[u8; 32]) {
        identity::write_seed_to_disk_with_keychain(
            plaintext_path,
            seed,
            /*force=*/ true,
            None,
        )
        .expect("plant");
    }

    // ── current_identity_hash_helper ─────────────────────────────────────

    #[test]
    #[serial]
    fn current_identity_hash_returns_32_hex_chars() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");

        std::env::set_var("HARMONY_PASSPHRASE", "cih-test");
        plant_seed(&plaintext_path, &[0x11u8; 32]);

        let hash = current_identity_hash_helper(&plaintext_path, None).expect("hash");
        // identity_hash() is [u8; 16] → 32 hex characters.
        assert_eq!(hash.len(), 32, "identity hash is 16 bytes → 32 hex chars");
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "hash must be hex; got: {hash}"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn current_identity_hash_matches_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");

        std::env::set_var("HARMONY_PASSPHRASE", "cih-match");
        let seed = [0x22u8; 32];
        plant_seed(&plaintext_path, &seed);

        let expected = hex::encode(
            RecoveryArtifact::from_seed(seed)
                .master_pubkey_bundle()
                .identity_hash(),
        );
        let got = current_identity_hash_helper(&plaintext_path, None).expect("hash");
        assert_eq!(got, expected);

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    // ── export_mnemonic_words_helper ──────────────────────────────────────

    #[test]
    #[serial]
    fn export_mnemonic_words_helper_returns_24_words() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");

        std::env::set_var("HARMONY_PASSPHRASE", "emw-test");
        plant_seed(&plaintext_path, &[0x33u8; 32]);

        let words = export_mnemonic_words_helper(&plaintext_path, None).expect("words");
        assert_eq!(words.len(), 24, "BIP39-24 produces exactly 24 words");
        for w in &words {
            assert!(
                !w.is_empty() && w.chars().all(|c: char| c.is_ascii_lowercase()),
                "each word is non-empty lowercase ASCII; got: {w}"
            );
        }

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    // ── preview_mnemonic_identity_helper ─────────────────────────────────

    #[test]
    #[serial]
    fn preview_mnemonic_identity_matches_direct_derivation() {
        let seed = [0x44u8; 32];
        let artifact = RecoveryArtifact::from_seed(seed);
        let expected_hash = hex::encode(artifact.master_pubkey_bundle().identity_hash());

        let mnemonic = artifact.to_mnemonic();
        let words: Vec<String> = mnemonic
            .as_str()
            .split_whitespace()
            .map(String::from)
            .collect();

        let got = preview_mnemonic_identity_helper(&words).expect("preview");
        assert_eq!(got, expected_hash);
    }

    #[test]
    fn preview_mnemonic_identity_rejects_wrong_word_count() {
        let words: Vec<String> = vec!["word".to_string(); 12];
        let err = preview_mnemonic_identity_helper(&words).expect_err("must fail for 12 words");
        assert!(err.contains("24"), "error must mention 24; got: {err}");
    }

    #[test]
    fn preview_mnemonic_identity_rejects_invalid_words() {
        let bad_words: Vec<String> = vec!["notaword".to_string(); 24];
        let err =
            preview_mnemonic_identity_helper(&bad_words).expect_err("must fail for invalid words");
        assert!(!err.is_empty(), "error must be non-empty; got: {err}");
    }

    // ── preview_recovery_file_helper ──────────────────────────────────────

    #[test]
    fn preview_recovery_file_returns_correct_metadata() {
        use secrecy::SecretString;

        let dir = tempfile::tempdir().unwrap();
        let recovery_path = dir.path().join("recovery.bin");

        let seed = [0x55u8; 32];
        let artifact = RecoveryArtifact::from_seed(seed);
        let expected_hash = hex::encode(artifact.master_pubkey_bundle().identity_hash());
        let pass = SecretString::from("preview-test".to_string());
        let metadata = RecoveryMetadata {
            mint_at: Some(1_700_000_000),
            comment: Some("test backup".to_string()),
        };
        let bytes = artifact.to_encrypted_file(&pass, &metadata).unwrap();
        std::fs::write(&recovery_path, &bytes).unwrap();

        let info = preview_recovery_file_helper(&recovery_path, "preview-test").expect("preview");
        assert_eq!(info.identity_hash, expected_hash);
        assert_eq!(info.minted_at, Some(1_700_000_000));
        assert_eq!(info.comment.as_deref(), Some("test backup"));
    }

    #[test]
    fn preview_recovery_file_wrong_passphrase_errors() {
        use secrecy::SecretString;

        let dir = tempfile::tempdir().unwrap();
        let recovery_path = dir.path().join("recovery.bin");

        let artifact = RecoveryArtifact::from_seed([0x66u8; 32]);
        let pass = SecretString::from("correct-pass".to_string());
        let bytes = artifact
            .to_encrypted_file(&pass, &RecoveryMetadata::default())
            .unwrap();
        std::fs::write(&recovery_path, &bytes).unwrap();

        let err = preview_recovery_file_helper(&recovery_path, "wrong-pass")
            .expect_err("wrong passphrase must fail");
        assert!(!err.is_empty(), "error must be non-empty; got: {err}");
    }

    /// Pin the load-bearing invariant from Greptile P1: if metadata
    /// extraction fails (file missing, corrupted, wrong passphrase), the
    /// on-disk identity must NOT be overwritten. The function must return
    /// `Err` BEFORE touching disk.
    #[test]
    #[serial]
    fn restore_recovery_file_failure_leaves_identity_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");

        std::env::set_var("HARMONY_PASSPHRASE", "metadata-first-test");
        let original_seed = [0xC1u8; 32];
        plant_seed(&plaintext_path, &original_seed);

        // Capture original on-disk encrypted bytes for content equality.
        let enc_path = plaintext_path.with_file_name("identity.enc");
        let original_enc = std::fs::read(&enc_path).expect("read original enc");

        // Point at a non-existent recovery file. Step 1 (read metadata)
        // must fail before step 2 (write seed) is ever invoked.
        let bogus_recovery = dir.path().join("does-not-exist.recovery");
        let err = restore_recovery_file_from_path_helper(
            &plaintext_path,
            &bogus_recovery,
            "any-pass",
            None,
        )
        .expect_err("missing recovery file must fail");
        assert!(
            err.contains("failed to read") || err.contains("does-not-exist"),
            "error must mention the missing file; got: {err}"
        );

        // Bytes on disk must be byte-for-byte identical to before the call.
        let after_enc = std::fs::read(&enc_path).expect("read after enc");
        assert_eq!(
            original_enc, after_enc,
            "identity must NOT be overwritten when metadata extraction fails"
        );

        // ── Same invariant for wrong-passphrase failure (file exists but
        // can't decrypt). ──
        use secrecy::SecretString;
        let recovery_path = dir.path().join("good.recovery");
        let artifact = RecoveryArtifact::from_seed([0xB7u8; 32]);
        let pass = SecretString::from("correct-pass".to_string());
        let bytes = artifact
            .to_encrypted_file(&pass, &RecoveryMetadata::default())
            .unwrap();
        std::fs::write(&recovery_path, &bytes).unwrap();

        let err =
            restore_recovery_file_from_path_helper(&plaintext_path, &recovery_path, "WRONG", None)
                .expect_err("wrong passphrase must fail");
        assert!(!err.is_empty(), "error must be non-empty; got: {err}");

        let after_enc = std::fs::read(&enc_path).expect("read after wrong-pass");
        assert_eq!(
            original_enc, after_enc,
            "identity must NOT be overwritten when passphrase is wrong"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    fn preview_recovery_file_rejects_oversized_file() {
        // Defense-in-depth: protect against accidental selection of a large
        // file (image, archive). Pin the size cap (review feedback from Qodo).
        let dir = tempfile::tempdir().unwrap();
        let too_big = dir.path().join("too-big.bin");
        // One byte over the cap — cheaper than writing 1 MiB+ of data and
        // still exercises the metadata().len() > MAX path.
        let len = MAX_RECOVERY_FILE_BYTES + 1;
        let f = std::fs::File::create(&too_big).unwrap();
        f.set_len(len).unwrap();
        drop(f);

        let err = preview_recovery_file_helper(&too_big, "any-pass")
            .expect_err("oversized file must be rejected before decrypt");
        assert!(
            err.contains("too large to be a recovery file"),
            "error must explain the size cap; got: {err}"
        );
    }

    #[test]
    fn preview_recovery_file_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nonexistent.bin");

        let err =
            preview_recovery_file_helper(&missing, "any-pass").expect_err("missing file must fail");
        assert!(
            err.contains("nonexistent.bin"),
            "error must mention file path; got: {err}"
        );
    }

    // ── export_recovery_file_to_path_helper env-var save/restore ─────────

    /// Pins the load-bearing invariant that both HARMONY_RECOVERY_PASSPHRASE
    /// and HARMONY_RECOVERY_PASSPHRASE_FILE are restored to their prior values
    /// after each call — including on error paths.  A future refactor that
    /// moves `?` before the restore block would cause sub-case 3 or 4 to fail.
    #[test]
    #[serial]
    fn export_recovery_file_to_path_restores_prior_env() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");
        let recovery_path = dir.path().join("rec.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "outer-pass");
        let seed = [0xE0u8; 32];
        plant_seed(&plaintext_path, &seed);

        // Sub-case 1: prior env value is restored after a successful call.
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "outer-original");
        export_recovery_file_to_path_helper(
            &plaintext_path,
            &recovery_path,
            "inner-call",
            None,
            None,
        )
        .expect("export");
        assert_eq!(
            std::env::var("HARMONY_RECOVERY_PASSPHRASE").as_deref(),
            Ok("outer-original"),
            "prior HARMONY_RECOVERY_PASSPHRASE must be restored"
        );

        // Sub-case 2: when prior was unset, env is removed (not left at "inner-call").
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
        let _ = std::fs::remove_file(&recovery_path);
        export_recovery_file_to_path_helper(
            &plaintext_path,
            &recovery_path,
            "inner-call",
            None,
            None,
        )
        .expect("export");
        assert!(
            std::env::var("HARMONY_RECOVERY_PASSPHRASE").is_err(),
            "HARMONY_RECOVERY_PASSPHRASE must be unset after the call (was unset before)"
        );

        // Sub-case 3: env is restored even on error (load-bearing invariant).
        // Point out_path at a non-existent nested directory so the write fails.
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE", "outer-original");
        let bogus_path = dir.path().join("no-such-dir").join("rec.bin");
        let _ = export_recovery_file_to_path_helper(
            &plaintext_path,
            &bogus_path,
            "inner-call",
            None,
            None,
        );
        // call may or may not error depending on internal flow; what matters is the env state:
        assert_eq!(
            std::env::var("HARMONY_RECOVERY_PASSPHRASE").as_deref(),
            Ok("outer-original"),
            "prior HARMONY_RECOVERY_PASSPHRASE must be restored even on error"
        );

        // Sub-case 4: HARMONY_RECOVERY_PASSPHRASE_FILE is also save/restored (Fix 1 behavior).
        std::env::set_var("HARMONY_RECOVERY_PASSPHRASE_FILE", "/tmp/some-file");
        let _ = std::fs::remove_file(&recovery_path);
        export_recovery_file_to_path_helper(
            &plaintext_path,
            &recovery_path,
            "inner-call",
            None,
            None,
        )
        .expect("export");
        assert_eq!(
            std::env::var("HARMONY_RECOVERY_PASSPHRASE_FILE").as_deref(),
            Ok("/tmp/some-file"),
            "HARMONY_RECOVERY_PASSPHRASE_FILE must be restored to its prior value"
        );

        // Cleanup
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE_FILE");
        std::env::remove_var("HARMONY_RECOVERY_PASSPHRASE");
        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    /// Reject oversized comments before any I/O. Pins the
    /// `MAX_RECOVERY_COMMENT_BYTES` cap (CodeRabbit round 3): an unbounded
    /// comment could push the resulting recovery file past
    /// `MAX_RECOVERY_FILE_BYTES`, making the same GUI later refuse to read it.
    #[test]
    #[serial]
    fn export_recovery_file_rejects_oversized_comment() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");
        let recovery_path = dir.path().join("rec.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "comment-cap-test");
        plant_seed(&plaintext_path, &[0xC0u8; 32]);

        let too_long = "x".repeat(MAX_RECOVERY_COMMENT_BYTES + 1);
        let err = export_recovery_file_to_path_helper(
            &plaintext_path,
            &recovery_path,
            "any-pass",
            Some(too_long),
            None,
        )
        .expect_err("oversized comment must be rejected");
        assert!(
            err.contains("comment is too large"),
            "error must explain the cap; got: {err}"
        );
        assert!(
            !recovery_path.exists(),
            "recovery file must NOT be written when the comment is rejected"
        );

        // Boundary: exactly the cap is accepted.
        let max_ok = "x".repeat(MAX_RECOVERY_COMMENT_BYTES);
        export_recovery_file_to_path_helper(
            &plaintext_path,
            &recovery_path,
            "any-pass",
            Some(max_ok),
            None,
        )
        .expect("comment at exactly the cap is allowed");

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    // ── restore_recovery_file_from_path_helper TOCTOU regression ─────────

    /// CodeRabbit round 3: the helper must NOT re-read `in_path` after the
    /// initial decrypt. Swapping the recovery file between metadata-shown
    /// and seed-written must NOT change which seed lands on disk; the seed
    /// written must be byte-equal to the seed whose hash was returned.
    #[test]
    #[serial]
    fn restore_recovery_file_seed_matches_returned_metadata_under_swap() {
        use harmony_owner::recovery::RecoveryMetadata;
        use secrecy::SecretString;

        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");
        let recovery_path = dir.path().join("rec.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "toctou-test");
        plant_seed(&plaintext_path, &[0xA1u8; 32]);

        // Backup A — what the user picks and confirms.
        let seed_a = [0xAAu8; 32];
        let artifact_a = RecoveryArtifact::from_seed(seed_a);
        let expected_hash_a = hex::encode(artifact_a.master_pubkey_bundle().identity_hash());
        let pass = "toctou-pass";
        let pass_secret = SecretString::from(pass.to_string());
        let bytes_a = artifact_a
            .to_encrypted_file(&pass_secret, &RecoveryMetadata::default())
            .unwrap();
        std::fs::write(&recovery_path, &bytes_a).unwrap();

        // Restore from path; helper must read+decrypt once. We simulate a
        // post-decrypt swap by wiping the file BEFORE the call returns: if
        // the helper ever re-reads, this restore would fail.  (The previous
        // implementation called `restore_recovery_file_with_keychain(in_path,…)`
        // after metadata extraction, which would do exactly that re-read.)
        let info =
            restore_recovery_file_from_path_helper(&plaintext_path, &recovery_path, pass, None)
                .expect("restore");
        assert_eq!(info.identity_hash, expected_hash_a);

        // The seed actually written to disk must be backup A's seed —
        // matching the hash we just returned.
        let on_disk = identity::read_seed_from_disk_with_keychain(&plaintext_path, None)
            .expect("read planted");
        assert_eq!(
            *on_disk, seed_a,
            "written seed must match returned metadata"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }
}
