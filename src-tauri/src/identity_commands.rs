//! Tauri commands for the identity backup/restore GUI wizard.
//!
//! Each command is a thin wrapper around [`crate::recovery_cli`] helpers.
//! The actual logic lives in `recovery_cli` so it can be tested without a
//! live Tauri runtime. Commands delegate to `*_helper` functions that take
//! a `plaintext_path` argument; tests call the helpers directly.
//!
//! Tauri commands wrap each helper in [`tokio::task::spawn_blocking`] —
//! the helpers do file I/O, Argon2id KDF, and XChaCha20-Poly1305 work that
//! would otherwise stall the async executor and starve other tasks
//! (zenoh sync, IPC, UI events).

use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;
use zeroize::Zeroizing;

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

/// Successful preview of a recovery file. Returned by `preview_recovery_file`.
///
/// The `previewToken` ties this preview to the seed that the GUI just showed
/// the user (hash + comment + mintedAt). The frontend passes the token back
/// to `restore_recovery_from_preview_token` — the backend looks up the
/// already-decrypted seed from the in-memory cache and writes THAT seed,
/// without ever re-reading the path. This closes the two-IPC TOCTOU window
/// where a swap of the recovery file between preview and commit would let
/// the wizard show backup A's hash while restoring backup B's seed
/// (CodeRabbit Critical, round 4).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewedRecovery {
    /// Opaque token identifying the cached preview. The frontend treats this
    /// as a string and passes it back unchanged on commit.
    pub preview_token: String,
    /// Identity metadata for the cached seed (shown in the confirm step).
    #[serde(flatten)]
    pub info: RestoreInfo,
}

// ── Internal helpers ──────────────────────────────────────────────────────

/// In-memory cache of decrypted recovery seeds keyed by preview token.
///
/// Purpose: bind the `preview_recovery_file` IPC call to the
/// `restore_recovery_from_preview_token` IPC call so the seed the user sees
/// metadata for is byte-equal to the seed the backend writes. The naïve
/// preview-then-commit-by-path flow is TOCTOU-vulnerable (swap between
/// IPCs).
///
/// Lifetime: an entry survives at most [`PREVIEW_TTL`]; expired entries are
/// pruned lazily on every preview/commit. A successful commit removes the
/// entry, so the seed cannot be replayed. The cached seed is wrapped in
/// [`Zeroizing`] so it is wiped from memory on drop.
///
/// Capacity: bounded by [`MAX_PREVIEW_ENTRIES`]. If the user previews more
/// distinct files than the cap, the oldest entry is evicted. The wizard's
/// serial UX makes this exceedingly rare, but the cap protects against a
/// memory-leak attack from a misbehaving frontend.
struct PreviewEntry {
    created_at: Instant,
    seed: Zeroizing<[u8; 32]>,
    info: RestoreInfo,
}

const PREVIEW_TTL: Duration = Duration::from_secs(300);
const MAX_PREVIEW_ENTRIES: usize = 16;

static PREVIEW_CACHE: LazyLock<Mutex<HashMap<Uuid, PreviewEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn preview_cache_lock() -> std::sync::MutexGuard<'static, HashMap<Uuid, PreviewEntry>> {
    PREVIEW_CACHE.lock().unwrap_or_else(|p| p.into_inner())
}

/// Insert a freshly-decrypted seed into the preview cache and return the
/// generated token. Evicts expired entries first; if still over capacity,
/// drops the single oldest entry.
fn insert_preview(seed: Zeroizing<[u8; 32]>, info: RestoreInfo) -> Uuid {
    let mut cache = preview_cache_lock();
    cache.retain(|_, entry| entry.created_at.elapsed() < PREVIEW_TTL);
    if cache.len() >= MAX_PREVIEW_ENTRIES {
        if let Some(oldest_uuid) = cache
            .iter()
            .min_by_key(|(_, e)| e.created_at)
            .map(|(k, _)| *k)
        {
            cache.remove(&oldest_uuid);
        }
    }
    let token = Uuid::new_v4();
    cache.insert(
        token,
        PreviewEntry {
            created_at: Instant::now(),
            seed,
            info,
        },
    );
    token
}

/// Remove and return a cached preview by token. Returns `None` if the token
/// is unknown or the entry has expired (eviction is performed first).
fn take_preview(token: Uuid) -> Option<(Zeroizing<[u8; 32]>, RestoreInfo)> {
    let mut cache = preview_cache_lock();
    cache.retain(|_, entry| entry.created_at.elapsed() < PREVIEW_TTL);
    cache.remove(&token).map(|e| (e.seed, e.info))
}

/// Clear the preview cache. Used by tests to keep state isolated; production
/// callers rely on TTL + take-on-commit to bound exposure.
#[cfg(test)]
fn clear_preview_cache() {
    preview_cache_lock().clear();
}

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

/// Read a recovery file with a size guard.
///
/// Opens the path **once** and uses the same descriptor for both the size
/// check (`file.metadata()`) and the bounded read. Doing two separate path
/// lookups (`std::fs::metadata(path)` then `std::fs::read(path)`) is
/// TOCTOU-vulnerable: a swap between the two calls could let an attacker
/// pass the metadata cap with a small file and then have the process
/// allocate and read an arbitrary file (CodeRabbit Major, round 4).
///
/// The bounded `take()` is belt-and-suspenders: even if the file grows
/// underneath the open descriptor on a filesystem that supports it, the
/// read stops at `MAX_RECOVERY_FILE_BYTES + 1` and we reject above the cap.
fn read_recovery_file(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;

    let mut file =
        std::fs::File::open(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    if metadata.len() > MAX_RECOVERY_FILE_BYTES {
        return Err(format!(
            "{} is too large to be a recovery file ({} bytes; max {} bytes)",
            path.display(),
            metadata.len(),
            MAX_RECOVERY_FILE_BYTES
        ));
    }
    // Cap the read at MAX + 1 so a file growing concurrently still cannot
    // force an unbounded allocation here.
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(MAX_RECOVERY_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    if bytes.len() as u64 > MAX_RECOVERY_FILE_BYTES {
        return Err(format!(
            "{} is too large to be a recovery file ({} bytes; max {} bytes)",
            path.display(),
            bytes.len(),
            MAX_RECOVERY_FILE_BYTES
        ));
    }
    Ok(bytes)
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

/// Decrypt a recovery file, cache the resulting seed in [`PREVIEW_CACHE`],
/// and return a token + metadata WITHOUT writing to disk.
///
/// The GUI restore-file step calls this to show "this restores identity
/// 0x…, created <date>, comment: …" before the user commits. The returned
/// `previewToken` must be passed back to
/// [`restore_recovery_from_preview_token_helper`] on commit — that handler
/// looks up the same decrypted seed from the cache, never re-reading
/// `in_path`. This is what closes the two-IPC TOCTOU window between
/// preview-and-commit.
pub fn preview_recovery_file_helper(
    in_path: &Path,
    passphrase: &str,
) -> Result<PreviewedRecovery, String> {
    use harmony_owner::lifecycle::RecoveryArtifact;
    use secrecy::SecretString;

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

    // Cache the seed (zeroized on drop) and mint a token that the GUI will
    // pass back on commit. The seed never leaves this process.
    let artifact = restored.into_artifact();
    let seed: Zeroizing<[u8; 32]> = Zeroizing::new(*artifact.as_bytes());
    let token = insert_preview(seed, info.clone());

    Ok(PreviewedRecovery {
        preview_token: token.to_string(),
        info,
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

/// Commit the seed cached under `preview_token` to disk.
///
/// `force` is always `true` on this path: the GUI has already shown the
/// user a `TypeToConfirmDialog` and received explicit acknowledgement that
/// the current identity will be overwritten.
///
/// **No re-read.** The seed written here was decrypted during
/// [`preview_recovery_file_helper`] and held in [`PREVIEW_CACHE`] until
/// commit. `in_path` is not consulted at all on this code path — the
/// preview-and-commit pair is bound through the token, not the filename.
/// A swap of the recovery file between IPCs cannot affect what gets
/// restored (CodeRabbit Critical, round 4). The token is single-use:
/// [`take_preview`] removes it from the cache, so a successful or partial
/// commit cannot be replayed.
///
/// Errors:
/// - **invalid token** (unknown UUID, expired, or already consumed): the
///   on-disk identity is untouched. The GUI will surface this and ask the
///   user to re-pick the recovery file.
/// - **disk write failure**: the cache entry has already been consumed
///   (we cannot tell whether the partial write succeeded), so the user
///   must re-preview before retrying. Identity may be partially written;
///   retry with the same backup is safe (`force=true`).
pub fn restore_recovery_from_preview_token_helper(
    plaintext_path: &Path,
    preview_token: &str,
    keychain: Option<KeychainStore>,
) -> Result<RestoreInfo, String> {
    let token = Uuid::parse_str(preview_token)
        .map_err(|_| "invalid preview token (re-pick the recovery file)".to_string())?;
    let (seed, info) = take_preview(token).ok_or_else(|| {
        "preview expired or already used; re-pick the recovery file to continue".to_string()
    })?;

    // force=true: caller has obtained explicit user confirmation via TypeToConfirmDialog.
    identity::write_seed_to_disk_with_keychain(
        plaintext_path,
        &seed,
        /*force=*/ true,
        keychain,
    )
    .map_err(|e| e.to_string())?;

    Ok(info)
}

// ── Tauri commands ────────────────────────────────────────────────────────

/// Run a synchronous helper on the blocking thread pool.
///
/// Each helper does file I/O, Argon2id KDF, and/or
/// XChaCha20-Poly1305 work — running these directly on the async executor
/// would stall other tasks (zenoh sync, IPC handling, UI events) for the
/// hundreds of milliseconds that a single Argon2id derivation takes
/// (Cursor Bugbot, round 4).
async fn run_blocking<F, T>(f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("background task failed: {e}"))?
}

/// Return the 32-char hex identity hash of the current on-disk identity.
#[tauri::command]
pub async fn current_identity_hash() -> Result<String, String> {
    let plaintext_path = identity::resolve_path(None)?;
    run_blocking(move || current_identity_hash_helper(&plaintext_path, KeychainStore::new().ok()))
        .await
}

/// Return the 24 BIP39 mnemonic words for the backup wizard.
#[tauri::command]
pub async fn export_mnemonic_words() -> Result<Vec<String>, String> {
    let plaintext_path = identity::resolve_path(None)?;
    run_blocking(move || export_mnemonic_words_helper(&plaintext_path, KeychainStore::new().ok()))
        .await
}

/// Return the identity hash that would result from restoring the given
/// words, WITHOUT writing anything to disk.
#[tauri::command]
pub async fn preview_mnemonic_identity(words: Vec<String>) -> Result<String, String> {
    run_blocking(move || preview_mnemonic_identity_helper(&words)).await
}

/// Decrypt a recovery file and return a preview token + metadata WITHOUT
/// writing to disk. The token must be passed back to
/// `restore_recovery_from_preview_token` to commit.
#[tauri::command]
pub async fn preview_recovery_file(
    in_path: String,
    passphrase: String,
) -> Result<PreviewedRecovery, String> {
    run_blocking(move || preview_recovery_file_helper(Path::new(&in_path), &passphrase)).await
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
    run_blocking(move || {
        export_recovery_file_to_path_helper(
            &plaintext_path,
            &out_path,
            &passphrase,
            comment,
            KeychainStore::new().ok(),
        )
    })
    .await
}

/// Restore the on-disk identity from a 24-word mnemonic array.
///
/// Returns the 32-char hex identity hash of the restored identity.
/// The GUI calls this only after the user has passed the `TypeToConfirmDialog`
/// gate, so `force=true` is applied unconditionally on this path.
#[tauri::command]
pub async fn restore_mnemonic_from_words(words: Vec<String>) -> Result<String, String> {
    let plaintext_path = identity::resolve_path(None)?;
    run_blocking(move || {
        restore_mnemonic_from_words_helper(&plaintext_path, &words, KeychainStore::new().ok())
    })
    .await
}

/// Commit the seed cached under `preview_token` to disk.
///
/// Returns metadata (`identityHash`, `mintedAt`, `comment`) for the
/// restored identity. The GUI calls this only after the user has passed
/// the `TypeToConfirmDialog` gate, so `force=true` is applied
/// unconditionally. The token comes from a prior `preview_recovery_file`
/// IPC call; see [`restore_recovery_from_preview_token_helper`] for the
/// TOCTOU rationale.
#[tauri::command]
pub async fn restore_recovery_from_preview_token(
    preview_token: String,
) -> Result<RestoreInfo, String> {
    let plaintext_path = identity::resolve_path(None)?;
    run_blocking(move || {
        restore_recovery_from_preview_token_helper(
            &plaintext_path,
            &preview_token,
            KeychainStore::new().ok(),
        )
    })
    .await
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

        let preview =
            preview_recovery_file_helper(&recovery_path, "preview-test").expect("preview");
        assert_eq!(preview.info.identity_hash, expected_hash);
        assert_eq!(preview.info.minted_at, Some(1_700_000_000));
        assert_eq!(preview.info.comment.as_deref(), Some("test backup"));
        // Token must be a parseable UUID — the GUI treats it as an opaque
        // string but the backend parses it back on commit.
        Uuid::parse_str(&preview.preview_token).expect("token must parse as UUID");
        clear_preview_cache();
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

    /// Pin the load-bearing invariant from Greptile P1: if preview fails
    /// (file missing, corrupted, wrong passphrase), no token is issued —
    /// so commit cannot run, and the on-disk identity is untouched.
    /// The token-cache architecture (round 4) makes this property
    /// architectural rather than ordering-dependent: there's literally no
    /// way to write the seed without first having a valid preview.
    #[test]
    #[serial]
    fn preview_failure_issues_no_token_and_leaves_identity_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");

        std::env::set_var("HARMONY_PASSPHRASE", "preview-fail-test");
        plant_seed(&plaintext_path, &[0xC1u8; 32]);

        let enc_path = plaintext_path.with_file_name("identity.enc");
        let original_enc = std::fs::read(&enc_path).expect("read original enc");

        // Sub-case 1: missing recovery file — preview returns Err.
        let bogus_recovery = dir.path().join("does-not-exist.recovery");
        let err = preview_recovery_file_helper(&bogus_recovery, "any-pass")
            .expect_err("missing recovery file must fail");
        assert!(
            err.contains("failed to read") || err.contains("does-not-exist"),
            "error must mention the missing file; got: {err}"
        );

        // Sub-case 2: wrong passphrase — preview returns Err.
        use secrecy::SecretString;
        let recovery_path = dir.path().join("good.recovery");
        let artifact = RecoveryArtifact::from_seed([0xB7u8; 32]);
        let pass = SecretString::from("correct-pass".to_string());
        let bytes = artifact
            .to_encrypted_file(&pass, &RecoveryMetadata::default())
            .unwrap();
        std::fs::write(&recovery_path, &bytes).unwrap();

        let err = preview_recovery_file_helper(&recovery_path, "WRONG")
            .expect_err("wrong passphrase must fail");
        assert!(!err.is_empty(), "error must be non-empty; got: {err}");

        // Identity bytes are byte-for-byte identical to before the call —
        // preview never touches plaintext_path.
        let after_enc = std::fs::read(&enc_path).expect("read after enc");
        assert_eq!(
            original_enc, after_enc,
            "identity must NOT be overwritten when preview fails"
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

    // ── token-cache TOCTOU regression (round 4) ──────────────────────────

    /// CodeRabbit round 4 (Critical): if the recovery file is swapped between
    /// `preview_recovery_file` and `restore_recovery_from_preview_token`, the
    /// commit must restore the seed shown to the user during preview, NOT
    /// the seed that's currently on disk.
    ///
    /// This test performs an **actual** swap: write A → preview → overwrite
    /// the path with B → commit by token → assert on-disk seed is A's. In
    /// the old path-based implementation the commit would re-read the path
    /// and restore B's seed, failing this test.
    #[test]
    #[serial]
    fn token_commit_uses_cached_seed_not_current_file_contents() {
        use secrecy::SecretString;

        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");
        let recovery_path = dir.path().join("rec.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "swap-test");
        plant_seed(&plaintext_path, &[0xA1u8; 32]);
        clear_preview_cache();

        // Backup A — the one the user picks, previews, and confirms.
        let seed_a = [0xAAu8; 32];
        let artifact_a = RecoveryArtifact::from_seed(seed_a);
        let expected_hash_a = hex::encode(artifact_a.master_pubkey_bundle().identity_hash());
        let pass_a = SecretString::from("pass-a".to_string());
        let bytes_a = artifact_a
            .to_encrypted_file(&pass_a, &RecoveryMetadata::default())
            .unwrap();
        std::fs::write(&recovery_path, &bytes_a).unwrap();

        // Step 1: preview A → get a token bound to A's seed.
        let preview = preview_recovery_file_helper(&recovery_path, "pass-a").expect("preview");
        assert_eq!(preview.info.identity_hash, expected_hash_a);

        // Step 2: SWAP. An attacker (or buggy app) replaces the file at the
        // same path with backup B, encrypted under a different passphrase.
        let seed_b = [0xBBu8; 32];
        let artifact_b = RecoveryArtifact::from_seed(seed_b);
        let pass_b = SecretString::from("pass-b".to_string());
        let bytes_b = artifact_b
            .to_encrypted_file(&pass_b, &RecoveryMetadata::default())
            .unwrap();
        std::fs::write(&recovery_path, &bytes_b).unwrap();

        // Step 3: commit by token. Backend must use the cached A seed; it
        // must NEVER re-read recovery_path, so B's contents are irrelevant.
        let info = restore_recovery_from_preview_token_helper(
            &plaintext_path,
            &preview.preview_token,
            None,
        )
        .expect("restore by token");
        assert_eq!(
            info.identity_hash, expected_hash_a,
            "commit must report A's hash (the previewed one), not B's"
        );

        // Step 4: verify the on-disk seed is A's, not B's.
        let on_disk =
            identity::read_seed_from_disk_with_keychain(&plaintext_path, None).expect("read seed");
        assert_eq!(
            *on_disk, seed_a,
            "on-disk seed must be A (previewed), not B (current file contents)"
        );
        assert_ne!(
            *on_disk, seed_b,
            "swap to B between preview and commit must NOT influence the restore"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    /// Token must be single-use: a successful commit removes the entry, so
    /// a second commit attempt with the same token fails. Defends against
    /// replay attacks (e.g. the GUI accidentally double-submitting).
    #[test]
    #[serial]
    fn token_commit_is_single_use() {
        use secrecy::SecretString;

        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");
        let recovery_path = dir.path().join("rec.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "single-use-test");
        plant_seed(&plaintext_path, &[0xC2u8; 32]);
        clear_preview_cache();

        let artifact = RecoveryArtifact::from_seed([0xCCu8; 32]);
        let pass = SecretString::from("pass".to_string());
        let bytes = artifact
            .to_encrypted_file(&pass, &RecoveryMetadata::default())
            .unwrap();
        std::fs::write(&recovery_path, &bytes).unwrap();

        let preview = preview_recovery_file_helper(&recovery_path, "pass").expect("preview");
        // First commit: succeeds.
        restore_recovery_from_preview_token_helper(&plaintext_path, &preview.preview_token, None)
            .expect("first commit");
        // Second commit with the SAME token: fails — the entry was consumed.
        let err = restore_recovery_from_preview_token_helper(
            &plaintext_path,
            &preview.preview_token,
            None,
        )
        .expect_err("second commit must fail");
        assert!(
            err.contains("expired") || err.contains("already used"),
            "error must explain replay protection; got: {err}"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    /// An unknown / malformed token must be rejected before any disk write.
    #[test]
    #[serial]
    fn token_commit_rejects_unknown_token() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext_path = dir.path().join("identity.key");

        std::env::set_var("HARMONY_PASSPHRASE", "unknown-token-test");
        plant_seed(&plaintext_path, &[0xD3u8; 32]);
        clear_preview_cache();

        let enc_path = plaintext_path.with_file_name("identity.enc");
        let original_enc = std::fs::read(&enc_path).expect("read original enc");

        // Random UUID that was never issued.
        let bogus = Uuid::new_v4().to_string();
        let err = restore_recovery_from_preview_token_helper(&plaintext_path, &bogus, None)
            .expect_err("unknown token must fail");
        assert!(!err.is_empty(), "error must be non-empty; got: {err}");

        // Identity untouched.
        let after_enc = std::fs::read(&enc_path).expect("read after enc");
        assert_eq!(
            original_enc, after_enc,
            "identity must NOT be overwritten on unknown token"
        );

        // Non-UUID garbage is also rejected.
        let err = restore_recovery_from_preview_token_helper(&plaintext_path, "not-a-uuid", None)
            .expect_err("malformed token must fail");
        assert!(
            err.contains("invalid preview token"),
            "error must say invalid token; got: {err}"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }
}
