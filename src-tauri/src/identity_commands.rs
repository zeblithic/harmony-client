//! Read-only Tauri commands for the identity backup/restore GUI wizard.
//!
//! Each command is a thin wrapper around [`crate::recovery_cli`] helpers.
//! The actual logic lives in `recovery_cli` so it can be tested without a
//! live Tauri runtime. Commands delegate to `*_helper` functions that take
//! a `plaintext_path` argument; tests call the helpers directly.

use serde::Serialize;
use std::path::Path;

use crate::identity;
use crate::recovery_cli;

// ── Shared types ─────────────────────────────────────────────────────────

/// Metadata from a recovery file, returned by `preview_recovery_file`.
/// Sent across IPC so it must implement `serde::Serialize`.
#[derive(Debug, Clone, Serialize)]
pub struct RestoreInfo {
    /// 32-char hex-encoded identity hash derived from the backup's seed.
    /// `identity_hash()` returns `[u8; 16]` (128-bit truncated BLAKE3).
    pub identity_hash: String,
    /// Unix epoch seconds recorded when the backup was created, if any.
    pub minted_at: Option<u64>,
    /// Free-text comment recorded in the backup, if any.
    pub comment: Option<String>,
}

// ── Helpers (testable without Tauri runtime) ──────────────────────────────

/// Return the 32-char hex identity hash of the on-disk identity.
///
/// Used by the Settings panel header and the restore-confirm gate ("type
/// the first 8 chars of your CURRENT hash").
pub fn current_identity_hash_helper(plaintext_path: &Path) -> Result<String, String> {
    let seed = identity::read_seed_from_disk_with_keychain(plaintext_path, None)?;
    use harmony_owner::lifecycle::RecoveryArtifact;
    let artifact = RecoveryArtifact::from_seed(*seed);
    let id_hash = artifact.master_pubkey_bundle().identity_hash();
    Ok(hex::encode(id_hash))
}

/// Return the 24 BIP39 words for the backup wizard's blurred-grid display.
pub fn export_mnemonic_words_helper(plaintext_path: &Path) -> Result<Vec<String>, String> {
    let (words, _id_hash) =
        recovery_cli::export_mnemonic_words_with_keychain(plaintext_path, None)?;
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

    let bytes =
        std::fs::read(in_path).map_err(|e| format!("failed to read {}: {e}", in_path.display()))?;
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

// ── Tauri commands ────────────────────────────────────────────────────────

/// Return the 32-char hex identity hash of the current on-disk identity.
#[tauri::command]
pub async fn current_identity_hash() -> Result<String, String> {
    let plaintext_path = identity::resolve_path(None)?;
    current_identity_hash_helper(&plaintext_path)
}

/// Return the 24 BIP39 mnemonic words for the backup wizard.
#[tauri::command]
pub async fn export_mnemonic_words() -> Result<Vec<String>, String> {
    let plaintext_path = identity::resolve_path(None)?;
    export_mnemonic_words_helper(&plaintext_path)
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

        let hash = current_identity_hash_helper(&plaintext_path).expect("hash");
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
        let got = current_identity_hash_helper(&plaintext_path).expect("hash");
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

        let words = export_mnemonic_words_helper(&plaintext_path).expect("words");
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
}
