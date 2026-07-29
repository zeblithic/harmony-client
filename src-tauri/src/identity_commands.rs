//! Tauri commands for the identity backup/restore GUI wizard.
//!
//! Each command is a thin wrapper around [`crate::recovery_cli`] helpers.
//! The actual logic lives in `recovery_cli` so it can be tested without a
//! live Tauri runtime. Commands delegate to `*_helper` functions that take
//! a `identity_path` argument; tests call the helpers directly.
//!
//! Tauri commands wrap each helper in [`tokio::task::spawn_blocking`] —
//! the helpers do file I/O, Argon2id KDF, and XChaCha20-Poly1305 work that
//! would otherwise stall the async executor and starve other tasks
//! (zenoh sync, IPC, UI events).

use secrecy::SecretString;
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
use crate::recovery_policy::{MAX_RECOVERY_COMMENT_BYTES, MIN_RECOVERY_PASSPHRASE_LEN};

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
    /// Path the user picked. Stored so commit can re-read the sidecar
    /// (TOCTOU-OK because the cached `seed` remains the source of truth
    /// for the identity write — see [`restore_recovery_from_preview_token_helper`]).
    in_path: PathBuf,
    /// Passphrase the user typed at preview. Stored so commit can decrypt
    /// the sidecar without re-prompting. `SecretString` is zeroized on drop.
    passphrase: SecretString,
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
fn insert_preview(
    seed: Zeroizing<[u8; 32]>,
    info: RestoreInfo,
    in_path: PathBuf,
    passphrase: SecretString,
) -> Uuid {
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
            in_path,
            passphrase,
        },
    );
    token
}

/// Remove and return a cached preview by token. Returns `None` if the token
/// is unknown or the entry has expired (eviction is performed first).
fn take_preview(token: Uuid) -> Option<(Zeroizing<[u8; 32]>, RestoreInfo, PathBuf, SecretString)> {
    let mut cache = preview_cache_lock();
    cache.retain(|_, entry| entry.created_at.elapsed() < PREVIEW_TTL);
    cache
        .remove(&token)
        .map(|e| (e.seed, e.info, e.in_path, e.passphrase))
}

/// Clear the preview cache. Used by tests to keep state isolated; production
/// callers rely on TTL + take-on-commit to bound exposure.
#[cfg(test)]
fn clear_preview_cache() {
    preview_cache_lock().clear();
}

/// Maximum size of a recovery file the GUI will read.
/// Recovery files are ~101 bytes today; 1 MiB is a generous upper bound that
/// rejects accidental selection of a large file (image, video, archive).
const MAX_RECOVERY_FILE_BYTES: u64 = 1 << 20;

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
    identity_path: &Path,
    keychain: Option<KeychainStore>,
) -> Result<String, String> {
    let seed = identity::read_seed_from_disk_with_keychain(identity_path, keychain)?;
    use harmony_owner::lifecycle::RecoveryArtifact;
    let artifact = RecoveryArtifact::from_seed(*seed);
    let id_hash = artifact.master_pubkey_bundle().identity_hash();
    Ok(hex::encode(id_hash))
}

/// Return the 24 BIP39 words for the backup wizard's blurred-grid display.
///
/// These are the RETICULUM IDENTITY seed words (node keypair) — the owner
/// master seed has its own backup flow (`owner_commands::export_owner_recovery`
/// in the GUI, `export owner-mnemonic` headless). ZEB-430.
///
/// Threads `keychain` through to match the running node's identity-resolution
/// chain. Tests inject `None`.
pub fn export_mnemonic_words_helper(
    identity_path: &Path,
    keychain: Option<KeychainStore>,
) -> Result<Vec<String>, String> {
    let (words, _id_hash) =
        recovery_cli::export_mnemonic_words_with_keychain(identity_path, keychain)?;
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
    //
    // Also cache `in_path` + `passphrase` so the commit step can re-read
    // the `.state` sidecar (ZEB-213) without re-prompting the user. The
    // cached `seed` remains the source of truth for the identity write,
    // so the previously-pinned TOCTOU property still holds for the HRMR.
    let artifact = restored.into_artifact();
    let seed: Zeroizing<[u8; 32]> = Zeroizing::new(*artifact.as_bytes());
    let token = insert_preview(seed, info.clone(), in_path.to_path_buf(), pass);

    Ok(PreviewedRecovery {
        preview_token: token.to_string(),
        info,
    })
}

/// Export the master seed (and optionally an owner-state sidecar) as a
/// passphrase-encrypted recovery file at `out_path`.
///
/// The GUI calls this after the user has chosen an output path and typed a
/// passphrase in the file-backup wizard. The `comment` field is stored in
/// the backup metadata and shown back to the user on restore.
///
/// `include_state == true` AND a sibling `owner_state_crdt.cbor` exists ⇒
/// also emit the `<out_path>.state` HRSS sidecar with nav-tree + DM history.
/// `include_state == false` OR no owner-state file ⇒ HRMR-only export
/// (atomic; HRMR is removed on sidecar-write failure so the operator
/// never sees a mismatched half-pair).
///
/// The passphrase is threaded directly into
/// [`recovery_cli::export_recovery_file_pair_with_keychain`] as a
/// `SecretString` — never via process-global env vars. `std::env::set_var`
/// is unsafe in a multithreaded program (per Rust docs: another thread
/// reading `getenv` concurrently can race), so a Tauri GUI process must
/// not touch it from a command handler (CodeRabbit round 5).
/// `secrecy::SecretString` also zeroizes the heap allocation on drop,
/// which a `String` env var does not.
pub fn export_recovery_file_to_path_helper(
    identity_path: &Path,
    out_path: &Path,
    passphrase: &str,
    comment: Option<String>,
    include_state: bool,
    keychain: Option<KeychainStore>,
) -> Result<recovery_cli::ExportResult, String> {
    use secrecy::SecretString;

    // Reject oversized comments before any I/O — the inner write would
    // otherwise produce a recovery file the same GUI later refuses to read
    // back (see [`MAX_RECOVERY_FILE_BYTES`]).
    if let Some(c) = comment.as_deref() {
        let len = c.len();
        if len > MAX_RECOVERY_COMMENT_BYTES {
            return Err(format!(
                "comment is too large ({len} bytes; max {MAX_RECOVERY_COMMENT_BYTES} bytes)"
            ));
        }
    }

    // The harmony dir hosts the owner-state CRDT and the last_backup.json
    // metadata file. Production wires identity_path = <harmony>/identity.key,
    // so identity_path.parent() is the harmony dir. Mirrors the CLI helper
    // `export_recovery_file_cli`.
    let harmony_dir = identity_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    let pass = SecretString::from(passphrase.to_string());
    recovery_cli::export_recovery_file_pair_with_keychain(
        identity_path,
        &harmony_dir,
        out_path,
        comment.as_deref(),
        Some(&pass),
        include_state,
        /*force=*/ true,
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
    identity_path: &Path,
    words: &[String],
    keychain: Option<KeychainStore>,
) -> Result<String, String> {
    // force=true: caller has obtained explicit user confirmation via TypeToConfirmDialog.
    recovery_cli::restore_mnemonic_from_words_with_keychain(
        identity_path,
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

/// Commit the seed cached under `preview_token` to disk, and (unless
/// `ignore_state == true`) restore the `<in_path>.state` sidecar that
/// the user picked during preview.
///
/// `force` is always `true` on this path: the GUI has already shown the
/// user a `TypeToConfirmDialog` and received explicit acknowledgement that
/// the current identity will be overwritten.
///
/// **HRMR has no re-read** — the seed written here was decrypted during
/// [`preview_recovery_file_helper`] and held in [`PREVIEW_CACHE`] until
/// commit. The identity write is bound through the token, not the
/// filename: a swap of the recovery file between IPCs cannot affect the
/// identity that gets restored (CodeRabbit Critical, round 4). The token
/// is single-use: [`take_preview`] removes it from the cache, so a
/// successful or partial commit cannot be replayed.
///
/// **The sidecar IS re-read at commit time** (ZEB-213). The plan calls
/// this out explicitly: `preview_recovery_state_sidecar` is informational
/// only (space count for UX); the actual sidecar restore happens here
/// and is verified for addr-binding against the cached seed's owner-addr.
///
/// **Order of operations** (round-1 bot finding C2 — Cursor High):
/// 1. Sidecar decode + AEAD verify (CBOR + key check)
/// 2. Sidecar addr-binding verify against cached seed's owner-addr
/// 3. Identity write
/// 4. owner-state write
///
/// Identity must NOT be written before sidecar verification — otherwise
/// a corrupt/swapped sidecar produces a partial-write state where the
/// identity is restored but owner-state silently fails. This mirrors
/// `recovery_cli::restore_recovery_file_pair_with_keychain` (which
/// already does sidecar metadata before identity write).
///
/// Errors:
/// - **invalid token** (unknown UUID, expired, or already consumed): the
///   on-disk identity is untouched. The GUI will surface this and ask the
///   user to re-pick the recovery file.
/// - **sidecar decode/verify failure** (missing-after-preview / corrupt /
///   addr-mismatch): on-disk identity is untouched. Error message tells
///   the operator what failed (no partial-success language since identity
///   wasn't written yet).
/// - **identity write failure**: cache entry already consumed (token is
///   single-use); identity may be partially written; retry with the same
///   backup is safe (`force=true`).
/// - **owner-state write failure** (only reachable AFTER identity is
///   already written): error prefixed with
///   `"identity restored but owner-state ..."` — operator-actionable
///   partial-success per `metadata-before-irreversible-write`. Round-1
///   bot finding C3.
pub fn restore_recovery_from_preview_token_helper(
    identity_path: &Path,
    preview_token: &str,
    ignore_state: bool,
    keychain: Option<KeychainStore>,
) -> Result<RestoreInfo, String> {
    use secrecy::ExposeSecret;

    let token = Uuid::parse_str(preview_token)
        .map_err(|_| "invalid preview token (re-pick the recovery file)".to_string())?;
    let (seed, info, in_path, passphrase) = take_preview(token).ok_or_else(|| {
        "preview expired or already used; re-pick the recovery file to continue".to_string()
    })?;

    // Sidecar restore path (ZEB-213). Mirrors the sidecar block in
    // `recovery_cli::restore_recovery_file_pair_with_keychain` — the
    // `.state` file IS re-read here, but BEFORE the identity write so
    // an addr-mismatch (or corrupt sidecar) cannot leave the user with
    // a partial restore (identity rewritten, owner-state untouched).
    //
    // Decoded snapshot stash for the post-identity-write step. `None`
    // when ignore_state is true or no sidecar exists.
    let harmony_dir = identity_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let snapshot: Option<crate::state_snapshot::OwnerStateSnapshot> = if !ignore_state {
        let sidecar = recovery_cli::sidecar_path(&in_path);
        if sidecar.exists() {
            let snap = crate::state_snapshot::decode_snapshot_file(
                passphrase.expose_secret().as_bytes(),
                &sidecar,
            )
            .map_err(|e| format!("state sidecar: {e}"))?;
            let addr = recovery_cli::derive_owner_addr_from_seed(&seed);
            crate::state_snapshot::verify_snapshot_addr(&snap, &addr.0)
                .map_err(|e| format!("state sidecar: {e}"))?;
            Some(snap)
        } else {
            // sidecar absent + !ignore_state: silent success — identity-only
            // restore. The GUI's "Restore both?" prompt would only have
            // offered ignore_state=false when a sidecar was present at
            // preview-time, but we don't error if it disappeared between
            // preview and commit (a missing sidecar is indistinguishable
            // from the legacy HRMR-only case).
            None
        }
    } else {
        None
    };

    // All sidecar metadata is verified — now safe to write identity.
    // force=true: caller has obtained explicit user confirmation via TypeToConfirmDialog.
    identity::write_seed_to_disk_with_keychain(
        identity_path,
        &seed,
        /*force=*/ true,
        keychain,
    )
    .map_err(|e| e.to_string())?;

    // Identity is now on disk. The owner-state write below is the only
    // operation that can leave a partial-success state; its error must
    // be prefixed accordingly so the operator knows identity DID land.
    if let Some(snap) = snapshot {
        let state_path = recovery_cli::owner_state_path(&harmony_dir);
        // force=true: same gate as the identity write above (user
        // already passed TypeToConfirmDialog). save_atomically writes
        // the canonical bytes verbatim — load_crdt parses that same
        // shape, mirroring the pair-helper's tempfile composition.
        if let Err(e) = crate::owner_state_persist::save_atomically(&state_path, &snap.tree) {
            return Err(format!(
                "identity restored but owner-state write failed: {e}"
            ));
        }
    }

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
pub(crate) async fn run_blocking<F, T>(f: F) -> Result<T, String>
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
    let identity_path = identity::resolve_path(None)?;
    run_blocking(move || current_identity_hash_helper(&identity_path, KeychainStore::new().ok()))
        .await
}

/// Return the 24 BIP39 mnemonic words for the backup wizard.
#[tauri::command]
pub async fn export_mnemonic_words() -> Result<Vec<String>, String> {
    let identity_path = identity::resolve_path(None)?;
    run_blocking(move || export_mnemonic_words_helper(&identity_path, KeychainStore::new().ok()))
        .await
}

/// ZEB-768: the string contract for [`identity_store_backend`]. The GUI
/// onboarding copy branches on these exact values, so pin them here.
fn identity_store_backend_label(keychain_available: bool) -> &'static str {
    if keychain_available {
        "keychain"
    } else {
        "encrypted-file"
    }
}

/// ZEB-768: report which identity-store backend onboarding copy should
/// describe, so the backup step tells the truth instead of unconditionally
/// asserting the OS keychain.
///
/// This reports keychain **availability** — whether a `KeychainStore`
/// constructs here. `HARMONY_DISABLE_KEYCHAIN`, a named profile, and hosts
/// with no keyring provider all make it refuse → `encrypted-file`. That is a
/// strict improvement over the old unconditional keychain claim and covers
/// ZEB-768's observed repro (kill-switch + passphrase file).
///
/// It is NOT proven persistence location, and callers must not read it as
/// such: the mint (`owner_state::save_secret`) still falls through to the
/// encrypted-file store when the keychain *write* fails even though the
/// handle constructed (`vault_save_slot` → `Ok(false)`/`Err`), and this
/// getter is queried before mint. Replacing it with a post-mint ground-truth
/// probe (mirroring `load_secret`'s keychain→file precedence) + a re-query
/// after mint is ZEB-830 — deferred because that probe's keychain branch is
/// only verifiable against a real OS keychain (the ZEB-428 isolation gate
/// blocks `KeychainStore::new()` in every test build, so CI cannot exercise
/// it).
///
/// Returns `"keychain"` or `"encrypted-file"`; the frontend falls back to
/// backend-neutral wording on any value it doesn't recognize or on error,
/// never an unearned keychain claim.
#[tauri::command]
pub async fn identity_store_backend() -> Result<String, String> {
    run_blocking(move || Ok(identity_store_backend_label(KeychainStore::new().is_ok()).to_string()))
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

/// Successful return shape for [`export_recovery_file_to_path`].
///
/// The renderer no longer knows the absolute path (single-use save-path
/// token replaced it), and as of ZEB-213 also doesn't know whether a
/// `.state` sidecar was emitted (depends on whether owner-state existed
/// on disk). Both pieces of information are surfaced so the success
/// screen can list both files with sizes.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryFileExportInfo {
    /// Absolute path of the HRMR (identity) recovery file that was written.
    pub saved_path: String,
    /// Absolute path of the HRSS state sidecar, when emitted. `None` when
    /// `include_state == false` or no owner-state CRDT existed on disk.
    pub sidecar_path: Option<String>,
    /// Bytes written to the sidecar (`0` when no sidecar was emitted).
    /// Surfaced so the GUI can display a "(NN KB)" hint next to the path.
    pub sidecar_bytes: usize,
}

/// Export the master seed (and optionally an owner-state sidecar) as a
/// passphrase-encrypted recovery file at the path the user confirmed via
/// `request_export_save_path`. The renderer passes back the opaque token
/// from that dialog command — it never names the path directly.
///
/// `include_state == true` AND a sibling owner-state CRDT exists ⇒ the
/// `<path>.state` sidecar is also written atomically (see
/// [`recovery_cli::export_recovery_file_pair_with_keychain`]).
///
/// Returns the paths that were actually written to and the sidecar byte
/// count, so the renderer (which no longer knows the path) can display
/// "saved to X / wrote sidecar Y (NN KB)" feedback.
#[tauri::command]
pub async fn export_recovery_file_to_path(
    path_token: String,
    passphrase: String,
    comment: Option<String>,
    include_state: bool,
) -> Result<RecoveryFileExportInfo, String> {
    // ZEB-202: reject under-length passphrases BEFORE any I/O or
    // token consumption. Codepoint count (`.chars().count()`)
    // matches the JS frontend's `[...str].length` check so
    // multibyte passphrases (emoji, CJK) round-trip identically.
    // Wording is verbatim with `owner_commands::export_owner_recovery_file_to_path`
    // so a user sees identical copy regardless of which side rejected.
    if passphrase.chars().count() < MIN_RECOVERY_PASSPHRASE_LEN {
        return Err(format!(
            "Recovery passphrase must be at least {MIN_RECOVERY_PASSPHRASE_LEN} characters."
        ));
    }
    let identity_path = identity::resolve_path(None)?;
    // Validate comment length BEFORE consuming the single-use path token.
    // Otherwise a too-long comment burns the token and forces the user
    // to re-pick a save path for a purely local validation error.
    // Mirrors the equivalent pre-take guards in
    // `owner_commands::export_owner_recovery_file_to_path`.
    if let Some(c) = comment.as_deref() {
        let len = c.len();
        if len > MAX_RECOVERY_COMMENT_BYTES {
            return Err(format!(
                "comment is too large ({len} bytes; max {MAX_RECOVERY_COMMENT_BYTES} bytes)"
            ));
        }
    }
    let path_uuid: Uuid = path_token
        .parse()
        .map_err(|e| format!("invalid path token: {e}"))?;
    run_blocking(move || {
        let out_path = crate::owner_state::take_path_token(&path_uuid).ok_or_else(|| {
            "Save path token expired or invalid. Please re-trigger backup.".to_string()
        })?;
        let result = export_recovery_file_to_path_helper(
            &identity_path,
            &out_path,
            &passphrase,
            comment,
            include_state,
            KeychainStore::new().ok(),
        )?;
        Ok(RecoveryFileExportInfo {
            saved_path: result.hrmr_path.display().to_string(),
            sidecar_path: result.hrss_path.map(|p| p.display().to_string()),
            sidecar_bytes: result.snapshot_bytes_written,
        })
    })
    .await
}

/// Error message when a restore IPC is invoked while the node is running.
/// The frontend matches on this exact text to surface a friendlier prompt
/// (and may eventually offer a "Stop node and retry" button), so don't
/// rephrase without updating the JS side.
const ERR_NODE_RUNNING: &str =
    "Stop the node before restoring identity (the running node still holds the old keys).";

/// Refuse the call if the node event-loop thread is running. The running
/// `NodeRuntime` caches the OLD identity's keys and zenoh subscriptions; a
/// silent identity overwrite would leave the user in an inconsistent state
/// where outgoing messages are stamped with the new identity hash but
/// signed by the old keys (CodeRabbit round 5).
fn require_node_stopped(state: &std::sync::Mutex<crate::NodeState>) -> Result<(), String> {
    let guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
    if guard.is_running() {
        return Err(ERR_NODE_RUNNING.to_string());
    }
    Ok(())
}

/// Restore the on-disk identity from a 24-word mnemonic array.
///
/// Returns the 32-char hex identity hash of the restored identity.
/// The GUI calls this only after the user has passed the `TypeToConfirmDialog`
/// gate, so `force=true` is applied unconditionally on this path.
///
/// Refuses while the node is running — see [`require_node_stopped`].
#[tauri::command]
pub async fn restore_mnemonic_from_words(
    words: Vec<String>,
    state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
) -> Result<String, String> {
    require_node_stopped(&state)?;
    let identity_path = identity::resolve_path(None)?;
    run_blocking(move || {
        restore_mnemonic_from_words_helper(&identity_path, &words, KeychainStore::new().ok())
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
///
/// `ignore_state == true` skips the `.state` sidecar restore even when
/// one is present next to the recovery file (ZEB-213). The GUI sets this
/// based on the user's "Restore both" / "Identity only" choice after
/// `preview_recovery_state_sidecar` reports whether a sidecar exists.
///
/// Refuses while the node is running — see [`require_node_stopped`].
#[tauri::command]
pub async fn restore_recovery_from_preview_token(
    preview_token: String,
    ignore_state: bool,
    state: tauri::State<'_, std::sync::Mutex<crate::NodeState>>,
) -> Result<RestoreInfo, String> {
    require_node_stopped(&state)?;
    let identity_path = identity::resolve_path(None)?;
    run_blocking(move || {
        restore_recovery_from_preview_token_helper(
            &identity_path,
            &preview_token,
            ignore_state,
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

    fn plant_seed(identity_path: &Path, seed: &[u8; 32]) {
        identity::write_seed_to_disk_with_keychain(identity_path, seed, /*force=*/ true, None)
            .expect("plant");
    }

    // ── identity_store_backend_label (ZEB-768) ───────────────────────────

    #[test]
    fn identity_store_backend_label_pins_the_frontend_string_contract() {
        // The GUI branches on these exact literals; a keychain-available
        // node reads "keychain", the file-store fallback reads
        // "encrypted-file". Anything else would make WelcomeModal fall back
        // to backend-neutral wording, so these two are the whole contract.
        assert_eq!(identity_store_backend_label(true), "keychain");
        assert_eq!(identity_store_backend_label(false), "encrypted-file");
    }

    // ── current_identity_hash_helper ─────────────────────────────────────

    #[test]
    #[serial]
    fn current_identity_hash_returns_32_hex_chars() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");

        std::env::set_var("HARMONY_PASSPHRASE", "cih-test");
        plant_seed(&identity_path, &[0x11u8; 32]);

        let hash = current_identity_hash_helper(&identity_path, None).expect("hash");
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
        let identity_path = dir.path().join("identity.key");

        std::env::set_var("HARMONY_PASSPHRASE", "cih-match");
        let seed = [0x22u8; 32];
        plant_seed(&identity_path, &seed);

        let expected = hex::encode(
            RecoveryArtifact::from_seed(seed)
                .master_pubkey_bundle()
                .identity_hash(),
        );
        let got = current_identity_hash_helper(&identity_path, None).expect("hash");
        assert_eq!(got, expected);

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    // ── export_mnemonic_words_helper ──────────────────────────────────────

    #[test]
    #[serial]
    fn export_mnemonic_words_helper_returns_24_words() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");

        std::env::set_var("HARMONY_PASSPHRASE", "emw-test");
        plant_seed(&identity_path, &[0x33u8; 32]);

        let words = export_mnemonic_words_helper(&identity_path, None).expect("words");
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
    #[serial]
    fn preview_recovery_file_returns_correct_metadata() {
        use secrecy::SecretString;

        let dir = tempfile::tempdir().unwrap();
        let recovery_path = dir.path().join("recovery.bin");

        // Defensive: clear at start in case a prior panic left state behind
        // (CodeRabbit round 5). #[serial] above prevents concurrent tests
        // from interfering with the cache during this run.
        clear_preview_cache();

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
        let identity_path = dir.path().join("identity.key");

        std::env::set_var("HARMONY_PASSPHRASE", "preview-fail-test");
        plant_seed(&identity_path, &[0xC1u8; 32]);

        let enc_path = identity_path.with_file_name("identity.enc");
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
        // preview never touches identity_path.
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

    // (env-var save/restore test removed in round 5 along with the
    // EnvVarGuard / recovery_env_lock infrastructure — passphrase is now
    // threaded directly via SecretString instead of process-global env.)

    /// Reject oversized comments before any I/O. Pins the
    /// `MAX_RECOVERY_COMMENT_BYTES` cap (CodeRabbit round 3): an unbounded
    /// comment could push the resulting recovery file past
    /// `MAX_RECOVERY_FILE_BYTES`, making the same GUI later refuse to read it.
    #[test]
    #[serial]
    fn export_recovery_file_rejects_oversized_comment() {
        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let recovery_path = dir.path().join("rec.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "comment-cap-test");
        plant_seed(&identity_path, &[0xC0u8; 32]);

        let too_long = "x".repeat(MAX_RECOVERY_COMMENT_BYTES + 1);
        let err = export_recovery_file_to_path_helper(
            &identity_path,
            &recovery_path,
            "any-pass",
            Some(too_long),
            /*include_state=*/ false,
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
            &identity_path,
            &recovery_path,
            "any-pass",
            Some(max_ok),
            /*include_state=*/ false,
            None,
        )
        .expect("comment at exactly the cap is allowed");

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    /// ZEB-194 Task 4: the IPC wrapper now consumes a `path_token` from
    /// `PATH_TOKEN_CACHE` instead of a renderer-supplied path. A bogus UUID
    /// must surface the "path token expired/invalid" error, NOT silently
    /// fall through to a write at an attacker-chosen location.
    #[test]
    #[serial]
    fn export_recovery_with_invalid_path_token_errors() {
        use crate::owner_state::clear_path_token_cache;
        clear_path_token_cache();
        let bogus = Uuid::new_v4();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(export_recovery_file_to_path(
            bogus.to_string(),
            "passphrase-12+".into(),
            None,
            /*include_state=*/ false,
        ));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_lowercase().contains("path token")
                && (err.contains("expired") || err.contains("invalid")),
            "error must mention path token expired/invalid; got: {err}"
        );
    }

    /// ZEB-202: an under-length passphrase must be rejected BEFORE the
    /// path token is consumed, so the user does not have to re-pick a
    /// save location for a purely local validation error.
    ///
    /// This mirrors the existing pre-take guards for path-token UUID
    /// parsing and comment length, and the established
    /// `owner_commands` pattern for the same validation.
    #[test]
    #[serial]
    fn export_recovery_rejects_short_passphrase_without_consuming_token() {
        use crate::owner_state::{clear_path_token_cache, insert_path_token, take_path_token};
        clear_path_token_cache();

        // Mint a real path token so we can prove the rejection didn't
        // consume it. The path itself is irrelevant — the test never
        // reaches the file-write step.
        let dir = tempfile::tempdir().unwrap();
        let recovery_path = dir.path().join("rec.bin");
        let token = insert_path_token(recovery_path);

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(export_recovery_file_to_path(
            token.to_string(),
            "short-pass".into(), // 10 codepoints, < MIN_RECOVERY_PASSPHRASE_LEN
            None,
            /*include_state=*/ false,
        ));
        let err = result.expect_err("under-length passphrase must be rejected");
        assert!(
            err.contains("at least") && err.contains("characters"),
            "error must explain the minimum length; got: {err}"
        );

        // The load-bearing assertion: the token survives the rejection.
        // If the guard ran AFTER `take_path_token`, this take would
        // return None and the test would fail.
        assert!(
            take_path_token(&token).is_some(),
            "rejection must NOT consume the path token (TOCTOU/UX invariant)"
        );
    }

    /// ZEB-202: pin the codepoint-vs-byte distinction the guard relies on.
    ///
    /// A 12-codepoint multibyte fixture has > 12 *bytes*. The IPC's guard
    /// uses `passphrase.chars().count()`, so this fixture clears the guard
    /// — that's the property the IPC needs.
    ///
    /// **What this test catches:** drift in the fixture itself (e.g. the
    /// CJK string is edited and silently loses or gains a codepoint), or
    /// a change to `MIN_RECOVERY_PASSPHRASE_LEN` that would invalidate
    /// the fixture's role as the boundary case.
    ///
    /// **What this test does NOT catch:** a refactor of the IPC's guard
    /// from `.chars().count()` to `.len()`. The fixture's `.len()` is 36,
    /// well above 12, so a `.len()` predicate would still admit it. A
    /// `.len()`-vs-`.chars().count()` drift in the actual guard would
    /// only break user-facing behavior on passphrases with 12 codepoints
    /// AND fewer than 12 bytes, which is impossible (a single codepoint
    /// is at least 1 byte). The real defense against that drift is the
    /// guard's own implementation comment plus owner-side parity.
    #[test]
    fn min_passphrase_len_check_uses_codepoint_count_not_byte_count() {
        // 12 CJK codepoints, 36 bytes.
        let multibyte = "日本語日本語日本語日本語";
        assert_eq!(
            multibyte.chars().count(),
            MIN_RECOVERY_PASSPHRASE_LEN,
            "fixture must be exactly MIN_RECOVERY_PASSPHRASE_LEN codepoints"
        );
        assert!(
            multibyte.len() > MIN_RECOVERY_PASSPHRASE_LEN,
            "fixture must exceed MIN_RECOVERY_PASSPHRASE_LEN BYTES — \
             that's the property that distinguishes codepoint vs byte counting"
        );
        assert!(
            multibyte.chars().count() >= MIN_RECOVERY_PASSPHRASE_LEN,
            "multibyte fixture must clear the codepoint-count guard"
        );
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
        let identity_path = dir.path().join("identity.key");
        let recovery_path = dir.path().join("rec.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "swap-test");
        plant_seed(&identity_path, &[0xA1u8; 32]);
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
        // ignore_state=true skips the sidecar (none exists here anyway —
        // this test pins identity TOCTOU, not sidecar TOCTOU).
        let info = restore_recovery_from_preview_token_helper(
            &identity_path,
            &preview.preview_token,
            /*ignore_state=*/ true,
            None,
        )
        .expect("restore by token");
        assert_eq!(
            info.identity_hash, expected_hash_a,
            "commit must report A's hash (the previewed one), not B's"
        );

        // Step 4: verify the on-disk seed is A's, not B's.
        let on_disk =
            identity::read_seed_from_disk_with_keychain(&identity_path, None).expect("read seed");
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
        let identity_path = dir.path().join("identity.key");
        let recovery_path = dir.path().join("rec.bin");

        std::env::set_var("HARMONY_PASSPHRASE", "single-use-test");
        plant_seed(&identity_path, &[0xC2u8; 32]);
        clear_preview_cache();

        let artifact = RecoveryArtifact::from_seed([0xCCu8; 32]);
        let pass = SecretString::from("pass".to_string());
        let bytes = artifact
            .to_encrypted_file(&pass, &RecoveryMetadata::default())
            .unwrap();
        std::fs::write(&recovery_path, &bytes).unwrap();

        let preview = preview_recovery_file_helper(&recovery_path, "pass").expect("preview");
        // First commit: succeeds.
        restore_recovery_from_preview_token_helper(
            &identity_path,
            &preview.preview_token,
            /*ignore_state=*/ true,
            None,
        )
        .expect("first commit");
        // Second commit with the SAME token: fails — the entry was consumed.
        let err = restore_recovery_from_preview_token_helper(
            &identity_path,
            &preview.preview_token,
            /*ignore_state=*/ true,
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
        let identity_path = dir.path().join("identity.key");

        std::env::set_var("HARMONY_PASSPHRASE", "unknown-token-test");
        plant_seed(&identity_path, &[0xD3u8; 32]);
        clear_preview_cache();

        let enc_path = identity_path.with_file_name("identity.enc");
        let original_enc = std::fs::read(&enc_path).expect("read original enc");

        // Random UUID that was never issued.
        let bogus = Uuid::new_v4().to_string();
        let err = restore_recovery_from_preview_token_helper(
            &identity_path,
            &bogus,
            /*ignore_state=*/ true,
            None,
        )
        .expect_err("unknown token must fail");
        assert!(!err.is_empty(), "error must be non-empty; got: {err}");

        // Identity untouched.
        let after_enc = std::fs::read(&enc_path).expect("read after enc");
        assert_eq!(
            original_enc, after_enc,
            "identity must NOT be overwritten on unknown token"
        );

        // Non-UUID garbage is also rejected.
        let err = restore_recovery_from_preview_token_helper(
            &identity_path,
            "not-a-uuid",
            /*ignore_state=*/ true,
            None,
        )
        .expect_err("malformed token must fail");
        assert!(
            err.contains("invalid preview token"),
            "error must say invalid token; got: {err}"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    /// Round-1 bot finding C2 (Cursor High Severity):
    /// `metadata-before-irreversible-write` requires the sidecar
    /// addr-binding check to happen BEFORE the identity is written. A
    /// mismatched sidecar (HRMR for seed A, HRSS for seed B, picked
    /// during the preview step somehow) must leave the on-disk identity
    /// untouched — not produce a partial-success state where identity
    /// got overwritten before the addr check failed.
    ///
    /// The setup:
    /// - Plant identity B on disk.
    /// - Synthesize a recovery file encrypting seed A.
    /// - Synthesize a sidecar bound to seed B's owner-addr.
    /// - Preview → cache seed A under a token.
    /// - Commit by token → sidecar's `oa` is B's, cached seed is A's → mismatch.
    /// - Assert identity B is still on disk byte-for-byte.
    #[test]
    #[serial]
    fn token_restore_addr_mismatch_leaves_identity_untouched() {
        use crate::owner_state_crdt::OwnerState;
        use crate::state_snapshot::encode_snapshot;
        use harmony_owner::lifecycle::RecoveryArtifact;
        use harmony_owner::recovery::RecoveryMetadata;
        use secrecy::SecretString;

        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join("identity.key");
        let recovery_path = dir.path().join("rec.bin");
        let sidecar_path = recovery_cli::sidecar_path(&recovery_path);
        let harmony_dir = identity_path.parent().unwrap();
        let state_path = recovery_cli::owner_state_path(harmony_dir);

        std::env::set_var("HARMONY_PASSPHRASE", "addr-mismatch-test");

        // Plant identity B (this is what's on disk before the restore attempt).
        let seed_b = [0xBBu8; 32];
        plant_seed(&identity_path, &seed_b);
        let enc_path = identity_path.with_file_name("identity.enc");
        let identity_b_bytes = std::fs::read(&enc_path).expect("read identity B");

        // Sanity: no owner-state file at start.
        let _ = std::fs::remove_file(&state_path);

        clear_preview_cache();

        // Synthesize a recovery file for seed A — DIFFERENT from B.
        let seed_a = [0xAAu8; 32];
        let artifact_a = RecoveryArtifact::from_seed(seed_a);
        let pass = SecretString::from("pass".to_string());
        let bytes_a = artifact_a
            .to_encrypted_file(&pass, &RecoveryMetadata::default())
            .unwrap();
        std::fs::write(&recovery_path, &bytes_a).unwrap();

        // Synthesize an HRSS sidecar bound to B's owner-addr, encrypted
        // with the same passphrase. Decode + addr-verify will pass the
        // CBOR/AEAD layer but fail the addr-binding check against the
        // CACHED seed (A's owner-addr).
        let addr_b = recovery_cli::derive_owner_addr_from_seed(&seed_b);
        let state = OwnerState::default();
        let hrss = encode_snapshot(
            b"pass",
            addr_b,
            crate::owner_state_types::Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "t".into(),
            },
            &state,
        )
        .expect("encode HRSS for B");
        std::fs::write(&sidecar_path, &hrss).unwrap();

        // Preview A. The cache now holds A's seed.
        let preview = preview_recovery_file_helper(&recovery_path, "pass").expect("preview");

        // Commit by token. The sidecar's `oa` is B's, the cached seed is
        // A's → addr-binding mismatch. Must fail BEFORE writing identity.
        let err = restore_recovery_from_preview_token_helper(
            &identity_path,
            &preview.preview_token,
            /*ignore_state=*/ false,
            None,
        )
        .expect_err("addr mismatch must fail");
        assert!(
            err.contains("state sidecar") && err.contains("identity mismatch"),
            "error must surface the addr-binding failure; got: {err}"
        );

        // Load-bearing assertion: identity B is still on disk byte-for-byte.
        // Pre-fix (identity written before sidecar verify), this would be A.
        let after = std::fs::read(&enc_path).expect("read after");
        assert_eq!(
            after, identity_b_bytes,
            "identity must NOT be overwritten when sidecar addr-binding fails"
        );

        std::env::remove_var("HARMONY_PASSPHRASE");
    }
}
