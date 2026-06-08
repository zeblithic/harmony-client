//! Persistence and IPC types for the owner-binding registry.
//!
//! Layered alongside `crate::identity` (per-device transport identity); does
//! not modify it. See `docs/specs/2026-04-28-zeb-170-track-b-devices-panel-v1-design.md`.

use serde::{Deserialize, Serialize};

/// Wire-format view of the owner identity + bound devices, mirrored to JS.
///
/// `canBackUp` reflects whether the master seed is still on this device:
/// `true` after a fresh mint, `false` after a future "Wipe master from
/// device" action. v1 does not ship the wipe; the field is here so the
/// panel renders the degraded state correctly when it does land.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OwnerStateView {
    pub owner_id: String,
    pub owner_display_name: String,
    pub devices: Vec<DeviceView>,
    pub can_back_up: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DeviceView {
    pub device_id: String,
    pub display_name: String,
    pub is_this_device: bool,
    pub trust_decision: TrustDecisionView,
    pub enrolled_at: u64,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TrustDecisionView {
    pub kind: TrustKind,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
// `camelCase` does NOT lowercase single-word PascalCase variants (e.g. "Full" stays "Full").
// Use `lowercase` to produce the conventional JSON discriminant form ("full" / "provisional" /
// "refused") that the TypeScript consumer does strict equality against.
#[serde(rename_all = "lowercase")]
pub enum TrustKind {
    Full,
    Provisional,
    Refused,
}

// ── Token cache for recovery-artifact bytes ───────────────────────────────
//
// The master seed is generated inside `mint_owner_identity()` and must reach
// `export_owner_recovery_file_to_path()` without ever crossing the IPC
// boundary as plaintext. The token (an opaque UUID string) is what the GUI
// ferries between those two commands; the backend resolves the token to the
// cached seed on the export call. This mirrors the `PreviewedRecovery` pattern
// in `identity_commands.rs` (PR-61).

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;
use zeroize::Zeroizing;

const TOKEN_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_LIVE_TOKENS: usize = 8;

struct TokenEntry {
    seed: Zeroizing<[u8; 32]>,
    inserted_at: Instant,
}

static TOKEN_CACHE: LazyLock<Mutex<HashMap<Uuid, TokenEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Acquire the cache lock, recovering from poisoning. A single panicked
/// request handler should not brick the cache for all subsequent requests
/// in the same process — mirrors PR-61's `preview_cache_lock` policy.
fn token_cache_lock() -> std::sync::MutexGuard<'static, HashMap<Uuid, TokenEntry>> {
    TOKEN_CACHE.lock().unwrap_or_else(|p| p.into_inner())
}

/// Insert a master seed into the token cache, returning a fresh single-use
/// token. Caller hands the token to the GUI; GUI presents it back via
/// `take_token`. Single-use semantics prevent token replay.
///
/// `seed` is `Zeroizing<[u8; 32]>` so the caller holds the zeroize-on-drop
/// guarantee from the moment the seed is materialized — not only after it
/// enters the cache. Mirrors PR-61's `insert_preview` signature.
pub fn insert_token(seed: Zeroizing<[u8; 32]>) -> Uuid {
    let token = Uuid::new_v4();
    let mut cache = token_cache_lock();
    evict_expired(&mut cache);
    if cache.len() >= MAX_LIVE_TOKENS {
        // Oldest entry is the LRU candidate; drop it.
        let oldest = cache
            .iter()
            .min_by_key(|(_, e)| e.inserted_at)
            .map(|(k, _)| *k);
        if let Some(k) = oldest {
            cache.remove(&k);
        }
    }
    cache.insert(
        token,
        TokenEntry {
            seed,
            inserted_at: Instant::now(),
        },
    );
    token
}

/// Consume a token: returns the master seed exactly once. Subsequent
/// `take_token(same_uuid)` returns `None`.
pub fn take_token(token: &Uuid) -> Option<Zeroizing<[u8; 32]>> {
    let mut cache = token_cache_lock();
    evict_expired(&mut cache);
    cache.remove(token).map(|e| e.seed)
}

fn evict_expired(cache: &mut HashMap<Uuid, TokenEntry>) {
    cache.retain(|_, e| e.inserted_at.elapsed() < TOKEN_TTL);
}

#[doc(hidden)]
#[cfg(test)]
pub(crate) fn clear_token_cache() {
    token_cache_lock().clear();
}

// ── Path token cache for export-save-dialog confirmations ────────────────
//
// Mirror of TOKEN_CACHE but for user-confirmed save paths. The
// `request_export_save_path` IPC opens the OS save dialog server-side and
// inserts the chosen PathBuf here; `export_*_to_path` consumes the token
// at commit time. The renderer never names a write path directly.
//
// Two separate caches (rather than one polymorphic enum) because the value
// types are semantically different: master-seed tokens hold
// `Zeroizing<[u8; 32]>` and need zeroize-on-drop; path tokens hold
// `PathBuf` and don't. Type-level separation prevents "wrong token type"
// runtime bugs.

// Couple to TOKEN_TTL/MAX_LIVE_TOKENS so the two caches age and evict in
// lockstep — separate constants invite silent drift if one is updated
// later without the other. If path tokens ever warrant a different
// lifetime (e.g., shorter because paths are non-secret), decouple here.
const PATH_TOKEN_TTL: Duration = TOKEN_TTL;
const MAX_LIVE_PATH_TOKENS: usize = MAX_LIVE_TOKENS;

struct PathTokenEntry {
    path: std::path::PathBuf,
    inserted_at: Instant,
}

static PATH_TOKEN_CACHE: LazyLock<Mutex<HashMap<Uuid, PathTokenEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn path_token_cache_lock() -> std::sync::MutexGuard<'static, HashMap<Uuid, PathTokenEntry>> {
    PATH_TOKEN_CACHE.lock().unwrap_or_else(|p| p.into_inner())
}

/// Insert a save path into the path-token cache, returning a fresh
/// single-use token. Caller hands the token to the GUI; GUI presents it
/// back via `take_path_token` on commit.
pub fn insert_path_token(path: std::path::PathBuf) -> Uuid {
    let token = Uuid::new_v4();
    let mut cache = path_token_cache_lock();
    evict_expired_paths(&mut cache);
    if cache.len() >= MAX_LIVE_PATH_TOKENS {
        let oldest = cache
            .iter()
            .min_by_key(|(_, e)| e.inserted_at)
            .map(|(k, _)| *k);
        if let Some(k) = oldest {
            cache.remove(&k);
        }
    }
    cache.insert(
        token,
        PathTokenEntry {
            path,
            inserted_at: Instant::now(),
        },
    );
    token
}

/// Consume a path token: returns the user-confirmed save path exactly once.
pub fn take_path_token(token: &Uuid) -> Option<std::path::PathBuf> {
    let mut cache = path_token_cache_lock();
    evict_expired_paths(&mut cache);
    cache.remove(token).map(|e| e.path)
}

fn evict_expired_paths(cache: &mut HashMap<Uuid, PathTokenEntry>) {
    cache.retain(|_, e| e.inserted_at.elapsed() < PATH_TOKEN_TTL);
}

#[doc(hidden)]
#[cfg(test)]
pub(crate) fn clear_path_token_cache() {
    path_token_cache_lock().clear();
}

#[cfg(test)]
mod token_cache_tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn insert_then_take_returns_seed_once() {
        clear_token_cache();
        let seed = [0xAAu8; 32];
        let token = insert_token(Zeroizing::new(seed));
        let taken = take_token(&token).expect("first take must succeed");
        assert_eq!(*taken, seed);
        assert!(
            take_token(&token).is_none(),
            "second take must return None (single-use)"
        );
    }

    #[test]
    #[serial]
    fn nonexistent_token_returns_none() {
        clear_token_cache();
        let bogus = Uuid::new_v4();
        assert!(take_token(&bogus).is_none());
    }

    #[test]
    #[serial]
    fn lru_evicts_when_max_live_tokens_exceeded() {
        // Insert MAX_LIVE_TOKENS + 2 tokens; the cap must hold and the
        // newest must survive.
        //
        // We deliberately do NOT assert which specific tokens were evicted:
        // `Instant::now()` resolution varies across systems (millisecond
        // granularity on some Linux/CI environments), so a tight insert
        // loop can produce ties on `inserted_at`. With ties, `min_by_key`
        // picks any of them — making the *identity* of the evicted entry
        // non-deterministic. The cap and newest-preserving invariants are
        // what the cache actually guarantees.
        clear_token_cache();
        let mut tokens = Vec::new();
        for i in 0..(MAX_LIVE_TOKENS + 2) {
            tokens.push(insert_token(Zeroizing::new([i as u8; 32])));
        }
        let last_token = tokens[MAX_LIVE_TOKENS + 1];
        // Newest-preserving: the most recently inserted token must still
        // be in the cache (LRU evicts oldest, not newest).
        assert!(
            take_token(&last_token).is_some(),
            "newest-inserted token must remain after cap-exceed insert"
        );
        // Cap invariant: total survivors must equal MAX_LIVE_TOKENS - 1
        // (we just consumed one above; before that, there were
        // MAX_LIVE_TOKENS in the cache).
        let remaining: usize = tokens
            .iter()
            .filter(|t| **t != last_token)
            .filter(|t| take_token(t).is_some())
            .count();
        assert_eq!(
            remaining,
            MAX_LIVE_TOKENS - 1,
            "after MAX_LIVE_TOKENS+2 inserts, exactly MAX_LIVE_TOKENS must survive"
        );
    }
}

#[cfg(test)]
mod path_token_cache_tests {
    use super::*;
    use serial_test::serial;
    use std::path::PathBuf;

    #[test]
    #[serial]
    fn insert_then_take_returns_path_once() {
        clear_path_token_cache();
        let path = PathBuf::from("/tmp/example-recovery.bin");
        let token = insert_path_token(path.clone());
        let taken = take_path_token(&token).expect("first take must succeed");
        assert_eq!(taken, path);
        assert!(
            take_path_token(&token).is_none(),
            "second take must return None (single-use)"
        );
    }

    #[test]
    #[serial]
    fn nonexistent_token_returns_none() {
        clear_path_token_cache();
        let bogus = Uuid::new_v4();
        assert!(take_path_token(&bogus).is_none());
    }

    #[test]
    #[serial]
    fn lru_evicts_when_max_live_path_tokens_exceeded() {
        // Mirrors the seed-token test: newest-preserving + cap invariant.
        // Same Instant::now() non-determinism caveat — we don't assert which
        // tokens were evicted, only that the cap holds and the newest survives.
        clear_path_token_cache();
        let mut tokens = Vec::new();
        for i in 0..(MAX_LIVE_PATH_TOKENS + 2) {
            tokens.push(insert_path_token(PathBuf::from(format!("/tmp/{i}.bin"))));
        }
        let last_token = tokens[MAX_LIVE_PATH_TOKENS + 1];
        assert!(
            take_path_token(&last_token).is_some(),
            "newest-inserted token must remain after cap-exceed insert"
        );
        let remaining: usize = tokens
            .iter()
            .filter(|t| **t != last_token)
            .filter(|t| take_path_token(t).is_some())
            .count();
        assert_eq!(
            remaining,
            MAX_LIVE_PATH_TOKENS - 1,
            "after MAX_LIVE_PATH_TOKENS+2 inserts, exactly MAX_LIVE_PATH_TOKENS must survive"
        );
    }
}

// ── Persistence layer (load + atomic save) ────────────────────────────────
//
// Encapsulates the atomicity contract from the spec:
//   1. Keychain writes first (device_signing_key, master_seed)
//   2. `.cbor` file last via `write_atomic_0600`
// The `.cbor` file's presence is the minted-marker — its absence means the
// natural un-minted state.

use crate::identity::{write_atomic_0600, EncryptedFileStore, KeyStore, KeychainStore};
use ed25519_dalek::SigningKey;
use harmony_owner::cbor;
use harmony_owner::state::OwnerState;
use std::path::Path;

const KEYCHAIN_OWNER_SERVICE: &str = "harmony.owner";
const KEYCHAIN_DEVICE_SK: &str = "device_signing_key";
const KEYCHAIN_MASTER_SEED: &str = "master_seed";
const OWNER_STATE_FILENAME: &str = "owner_state.cbor";

/// Returned by `load_owner_state` when a persisted identity is found.
// Debug is derived so test assertions can use `.expect_err()` / `.expect()`.
// OwnerState: Debug (derived), SigningKey: Debug (manual impl in ed25519-dalek).
#[derive(Debug)]
pub struct LoadedOwnerState {
    pub state: OwnerState,
    pub device_signing_key: SigningKey,
    /// `None` when the master seed has been wiped from this device but
    /// the rest of the owner state remains. v1 does not ship the wipe
    /// action; this case is reachable only via manual file deletion.
    /// Wrapped in `Zeroizing` so the seed is wiped on drop — matches the
    /// token cache's `Zeroizing<[u8; 32]>` discipline.
    pub master_seed: Option<Zeroizing<[u8; 32]>>,
}

/// Load the persisted OwnerState if present. Returns `Ok(None)` for the
/// natural un-minted state (no `.cbor` file). Returns `Err` for corrupt
/// files or inconsistent state (`.cbor` present but signing key missing).
///
/// `keychain`: `Some(_)` enables the OS keychain primary store; `None`
/// falls through directly to the encrypted-file fallback (used in tests).
pub fn load_owner_state(
    identity_dir: &Path,
    keychain: Option<KeychainStore>,
) -> Result<Option<LoadedOwnerState>, String> {
    let cbor_path = identity_dir.join(OWNER_STATE_FILENAME);
    if !cbor_path.exists() {
        return Ok(None);
    }
    let cbor_bytes = std::fs::read(&cbor_path)
        .map_err(|e| format!("failed to read {}: {e}", cbor_path.display()))?;
    let state: OwnerState =
        cbor::from_bytes(&cbor_bytes).map_err(|e| format!("owner_state.cbor is corrupt: {e}"))?;

    // Inconsistent-state checks: state present implies signing key MUST be
    // findable; master seed MAY be absent (degraded but functional).
    let signing_key_bytes =
        load_secret(&keychain, KEYCHAIN_DEVICE_SK, identity_dir, "device_sk.enc")?.ok_or_else(
            || {
                "owner_state.cbor present but device_signing_key missing — inconsistent state"
                    .to_string()
            },
        )?;

    // SigningKey::from_bytes copies; the Zeroizing wrapper around
    // signing_key_bytes ensures the source heap buffer wipes on drop.
    let device_signing_key = SigningKey::from_bytes(&signing_key_bytes);

    let master_seed = load_secret(
        &keychain,
        KEYCHAIN_MASTER_SEED,
        identity_dir,
        "master_seed.enc",
    )?;

    Ok(Some(LoadedOwnerState {
        state,
        device_signing_key,
        master_seed,
    }))
}

/// Atomically persist a freshly-minted owner identity.
///
/// Order: keychain entries first, `.cbor` last. The `.cbor` file's
/// presence is the minted-marker for `load_owner_state`. If a keychain
/// write fails, no `.cbor` file is created and the next launch sees
/// the natural empty state. If `.cbor` write fails, keychain entries
/// remain; tolerated and overwritten on next mint attempt.
pub fn save_owner_state_atomic(
    identity_dir: &Path,
    state: &OwnerState,
    device_signing_key: &SigningKey,
    master_seed: Option<&[u8; 32]>,
    keychain: Option<KeychainStore>,
) -> Result<(), String> {
    save_secret(
        &keychain,
        KEYCHAIN_DEVICE_SK,
        identity_dir,
        "device_sk.enc",
        &device_signing_key.to_bytes(),
    )?;
    if let Some(seed) = master_seed {
        save_secret(
            &keychain,
            KEYCHAIN_MASTER_SEED,
            identity_dir,
            "master_seed.enc",
            seed,
        )?;
    } else {
        // Joiner case: cert-only model. We must NOT leave a stale master_seed
        // from a previous identity behind; if we did, `load_owner_state` would
        // pick it up and `canBackUp` would lie about backup eligibility.
        clear_secret(
            &keychain,
            KEYCHAIN_MASTER_SEED,
            identity_dir,
            "master_seed.enc",
        )?;
    }
    let cbor_bytes =
        cbor::to_canonical(state).map_err(|e| format!("CBOR encode of OwnerState failed: {e}"))?;
    let cbor_path = identity_dir.join(OWNER_STATE_FILENAME);
    write_atomic_0600(&cbor_path, &cbor_bytes)
        .map_err(|e| format!("failed to write {}: {e}", cbor_path.display()))?;
    Ok(())
}

/// Persist only the `OwnerState` CRDT to `owner_state.cbor` (canonical CBOR,
/// atomic 0600). Unlike [`save_owner_state_atomic`], this does NOT touch the
/// `device_signing_key` / `master_seed` keychain entries — it is for callers
/// (e.g. the ZEB-342 liveness refresh) that mutate only the CRDT and must not
/// risk clearing the master seed via the `master_seed == None` joiner branch.
/// Callers MUST hold `OWNER_STATE_WRITE_LOCK`.
pub fn save_owner_state_cbor_only(identity_dir: &Path, state: &OwnerState) -> Result<(), String> {
    let cbor_bytes =
        cbor::to_canonical(state).map_err(|e| format!("CBOR encode of OwnerState failed: {e}"))?;
    let cbor_path = identity_dir.join(OWNER_STATE_FILENAME);
    write_atomic_0600(&cbor_path, &cbor_bytes)
        .map_err(|e| format!("failed to write {}: {e}", cbor_path.display()))?;
    Ok(())
}

/// Ensure the local device (derived from `device_sk`) has a fresh `LivenessCert`
/// in `state`. Returns `true` if it mutated `state` (caller must then persist via
/// [`save_owner_state_cbor_only`]).
///
/// ZEB-342: without an active (liveness-bearing) local device, `evaluate_trust`
/// refuses the sole device with `StaleTrustState` (the fresh-mint "● refused"
/// badge). Publishes when the local device has no liveness or its liveness is
/// older than `DEFAULT_FRESHNESS_WINDOW_SECS / 2` (~15 days), bounding writes to
/// ~once per boot per fortnight. On a signing/add error it logs at warn and
/// returns `false` — the panel falls back to today's behavior rather than failing.
pub fn refresh_self_liveness(state: &mut OwnerState, device_sk: &SigningKey, now: u64) -> bool {
    use harmony_owner::certs::LivenessCert;
    use harmony_owner::pubkey_bundle::PubKeyBundle;

    let device_id =
        PubKeyBundle::classical_only(device_sk.verifying_key().to_bytes()).identity_hash();
    let threshold = harmony_owner::trust::DEFAULT_FRESHNESS_WINDOW_SECS / 2;
    let stale = match state.liveness.get(&device_id) {
        Some(c) => c.timestamp < now.saturating_sub(threshold),
        None => true,
    };
    if !stale {
        return false;
    }
    let cert = match LivenessCert::sign(device_sk, state.owner_id, now) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "refresh_self_liveness: liveness sign failed");
            return false;
        }
    };
    match state.add_liveness(cert) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(error = %e, "refresh_self_liveness: add_liveness failed");
            false
        }
    }
}

/// Load a 32-byte secret from keychain primary, encrypted-file fallback.
/// Returns `Ok(None)` when neither source has the secret.
///
/// Returns `Zeroizing<[u8; 32]>` so the secret zeros on drop through the
/// entire call chain — matches the discipline applied elsewhere in this
/// module and in `crate::identity`.
///
/// Keychain errors other than `NoEntry` (locked keychain, permission denied,
/// flaky backend) fall through to the encrypted-file fallback rather than
/// hard-failing — matches the pattern in `crate::identity` which made
/// keychain integration robust on partially-broken systems.
///
/// TODO(ZEB-189): the `keychain` parameter is currently used as a boolean
/// sentinel; the injected `KeychainStore` value is bypassed in favor of a
/// raw `keyring::Entry::new(...)`. Future cleanup will properly delegate.
fn load_secret(
    keychain: &Option<KeychainStore>,
    keychain_name: &str,
    identity_dir: &Path,
    fallback_filename: &str,
) -> Result<Option<Zeroizing<[u8; 32]>>, String> {
    // Track non-NoEntry keychain errors so we can propagate them rather
    // than silently masking a locked/permission-denied keychain as an
    // un-minted state when no encrypted-file fallback is configured.
    let mut keychain_err: Option<String> = None;
    if keychain.is_some() {
        let entry = keyring::Entry::new(KEYCHAIN_OWNER_SERVICE, keychain_name)
            .map_err(|e| format!("keychain entry creation for {keychain_name}: {e}"))?;
        match entry.get_secret() {
            Ok(raw_bytes) => {
                // Wrap the heap Vec immediately so it zeroes on drop, matching
                // the discipline applied to keychain reads in `crate::identity`.
                let bytes = Zeroizing::new(raw_bytes);
                if bytes.len() != 32 {
                    return Err(format!(
                        "keychain entry {KEYCHAIN_OWNER_SERVICE}/{keychain_name} length is {} bytes, expected 32",
                        bytes.len()
                    ));
                }
                let mut arr = Zeroizing::new([0u8; 32]);
                arr.copy_from_slice(&bytes);
                return Ok(Some(arr));
            }
            Err(keyring::Error::NoEntry) => {}
            Err(e) => {
                // Don't hard-fail: a flaky/locked keychain shouldn't break
                // load when the encrypted-file fallback is configured.
                let msg = format!("keychain read {KEYCHAIN_OWNER_SERVICE}/{keychain_name}: {e}");
                tracing::warn!("{msg}; falling through to encrypted-file fallback");
                keychain_err = Some(msg);
            }
        }
    }
    // Fallback: encrypted file under HARMONY_PASSPHRASE.
    let path = identity_dir.join(fallback_filename);
    let store_opt = EncryptedFileStore::from_env(path.clone())
        .map_err(|e| format!("encrypted-file fallback for {fallback_filename}: {e}"))?;
    let store = match store_opt {
        Some(s) => s,
        None => {
            // No fallback configured. If the keychain READ failed earlier
            // (locked / permission denied / other), surface that — otherwise
            // load_owner_state would misclassify a locked keychain as a
            // wiped master_seed and disable backup with the wrong remediation.
            return match keychain_err {
                Some(e) => Err(e),
                None => Ok(None), // genuine "no secret anywhere"
            };
        }
    };
    match store.load() {
        Ok(seed_bytes) => Ok(seed_bytes), // already Option<Zeroizing<[u8; 32]>>
        Err(e) => Err(format!("read {fallback_filename}: {e}")),
    }
}

// TODO(ZEB-189): the `keychain` parameter is currently used as a boolean
// sentinel; the injected `KeychainStore` value is bypassed in favor of a
// raw `keyring::Entry::new(...)`. Future cleanup will properly delegate.
fn save_secret(
    keychain: &Option<KeychainStore>,
    keychain_name: &str,
    identity_dir: &Path,
    fallback_filename: &str,
    bytes: &[u8; 32],
) -> Result<(), String> {
    // Mirror load_secret's error-preservation: if the keychain WRITE fails AND
    // no encrypted-file fallback is configured, surface the keychain error
    // (locked / permission denied / etc) instead of the generic "HARMONY_PASSPHRASE
    // not set" message — otherwise mint reports the wrong remediation.
    let mut keychain_err: Option<String> = None;
    if keychain.is_some() {
        let entry = keyring::Entry::new(KEYCHAIN_OWNER_SERVICE, keychain_name)
            .map_err(|e| format!("keychain entry creation for {keychain_name}: {e}"))?;
        match entry.set_secret(bytes) {
            Ok(()) => return Ok(()),
            Err(e) => {
                // Don't hard-fail: a flaky/locked keychain shouldn't block
                // mint when the encrypted-file fallback is configured.
                let msg = format!("keychain write {KEYCHAIN_OWNER_SERVICE}/{keychain_name}: {e}");
                tracing::warn!("{msg}; falling through to encrypted-file fallback");
                keychain_err = Some(msg);
            }
        }
    }
    let path = identity_dir.join(fallback_filename);
    let store_opt = EncryptedFileStore::from_env(path.clone())
        .map_err(|e| format!("encrypted-file fallback for {fallback_filename}: {e}"))?;
    let store = match store_opt {
        Some(s) => s,
        None => {
            return match keychain_err {
                Some(e) => Err(e),
                None => Err(format!(
                    "HARMONY_PASSPHRASE not set; cannot encrypt {fallback_filename}"
                )),
            };
        }
    };
    store
        .save(bytes)
        .map_err(|e| format!("write {fallback_filename}: {e}"))?;
    Ok(())
}

/// Remove a previously-persisted secret from BOTH the keychain primary AND
/// the encrypted-file fallback. Idempotent: NoEntry / NotFound are silent.
///
/// Used for the `master_seed = None` branch of `save_owner_state_atomic`
/// (cert-only model — Joiner enrollment): if a prior identity left a
/// `master_seed.enc` (or keychain entry), we must wipe it so subsequent
/// `load_owner_state` correctly reports no master and `canBackUp: false`.
///
/// On keychain delete failure (other than `NoEntry`): we still try
/// best-effort to remove the encrypted-file fallback before propagating
/// the keychain error. PR #63 review pointed out that returning early on
/// keychain Err meant the fallback file was left untouched on disk —
/// `load_secret` falls through to the file when keychain is unavailable
/// (locked, removed credential), so a stale `master_seed.enc` resurrects
/// the master on the next "no keychain" load. The combined attempt
/// minimises residue: at least one of the two stores ends up clean even
/// when the other fails. The function still ultimately returns the
/// keychain error so callers know the keychain side wasn't fully cleared.
fn clear_secret(
    keychain: &Option<KeychainStore>,
    keychain_name: &str,
    identity_dir: &Path,
    fallback_filename: &str,
) -> Result<(), String> {
    let mut keychain_err: Option<String> = None;
    if keychain.is_some() {
        let entry = keyring::Entry::new(KEYCHAIN_OWNER_SERVICE, keychain_name)
            .map_err(|e| format!("keychain entry creation for {keychain_name}: {e}"))?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(e) => {
                keychain_err = Some(format!(
                    "keychain delete {KEYCHAIN_OWNER_SERVICE}/{keychain_name}: {e}"
                ));
            }
        }
    }
    let path = identity_dir.join(fallback_filename);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("delete {}: {e}", path.display())),
    }
    match keychain_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;
    use harmony_owner::lifecycle::{mint_owner, MintResult};
    use serial_test::serial;
    use tempfile::tempdir;

    /// RAII guard: sets an env var on construction and removes it on drop (including on panic).
    /// Prevents a test-panic from leaking env vars into the next `#[serial]` test.
    struct EnvVarGuard {
        name: &'static str,
    }

    impl EnvVarGuard {
        fn set(name: &'static str, value: &str) -> Self {
            std::env::set_var(name, value);
            Self { name }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            std::env::remove_var(self.name);
        }
    }

    #[test]
    #[serial]
    fn save_then_load_roundtrip_preserves_state() {
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "test-pp-1");
        let dir = tempdir().unwrap();

        let MintResult {
            state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(1_700_000_000).unwrap();
        let master_seed = *recovery_artifact.as_bytes();

        save_owner_state_atomic(
            dir.path(),
            &state,
            &device_signing_key,
            Some(&master_seed),
            None,
        )
        .expect("save");

        let loaded = load_owner_state(dir.path(), None)
            .expect("load")
            .expect("must be Some after save");
        assert_eq!(loaded.state.owner_id, state.owner_id);
        assert_eq!(loaded.state.enrollments.len(), 1);
        assert_eq!(
            loaded.device_signing_key.to_bytes(),
            device_signing_key.to_bytes()
        );
        assert_eq!(loaded.master_seed.as_deref(), Some(&master_seed));
    }

    #[test]
    #[serial]
    fn cbor_only_persists_state_without_touching_keychain() {
        // ZEB-342: the liveness refresh persists ONLY the CRDT via the cbor-only
        // writer; it must NOT clear the master-seed keychain entry the way
        // save_owner_state_atomic(None) does.
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "cbor-only-test-pp");
        let dir = tempdir().unwrap();
        let MintResult {
            mut state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(1_700_000_111).unwrap();
        // Full save first: writes device_sk + master_seed keychain entries + cbor.
        save_owner_state_atomic(
            dir.path(),
            &state,
            &device_signing_key,
            Some(recovery_artifact.as_bytes()),
            None,
        )
        .unwrap();

        // Mutate the CRDT (simulate a liveness refresh) and persist cbor-only.
        state.liveness.clear();
        save_owner_state_cbor_only(dir.path(), &state).unwrap();

        // Reload: cbor reflects the mutation AND the master seed survived.
        let loaded = load_owner_state(dir.path(), None).unwrap().expect("must be Some");
        assert_eq!(
            loaded.state.liveness.len(),
            0,
            "cbor-only write must persist the CRDT mutation"
        );
        assert!(
            loaded.master_seed.is_some(),
            "cbor-only write must NOT clear the master seed"
        );
    }

    #[test]
    fn refresh_self_liveness_publishes_when_missing_then_full() {
        let now = 1_700_000_222;
        let MintResult {
            mut state,
            device_signing_key,
            ..
        } = mint_owner(now).unwrap();
        state.liveness.clear(); // simulate a legacy identity with no liveness
        let device_id = *state.enrollments.keys().next().unwrap();

        let mutated = refresh_self_liveness(&mut state, &device_signing_key, now);
        assert!(mutated, "missing liveness must trigger a publish");
        assert_eq!(state.liveness.len(), 1);
        assert_eq!(
            harmony_owner::trust::evaluate_trust(
                &state,
                device_id,
                now,
                harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS,
                harmony_owner::trust::DEFAULT_FRESHNESS_WINDOW_SECS,
            ),
            harmony_owner::trust::TrustDecision::Full,
        );
    }

    #[test]
    fn refresh_self_liveness_is_noop_when_fresh() {
        let now = 1_700_000_333;
        let MintResult {
            mut state,
            device_signing_key,
            ..
        } = mint_owner(now).unwrap();
        // Ensure a fresh liveness exists (independent of mint_owner's own behavior).
        refresh_self_liveness(&mut state, &device_signing_key, now);
        let mutated = refresh_self_liveness(&mut state, &device_signing_key, now);
        assert!(!mutated, "fresh liveness must NOT be re-published");
    }

    #[test]
    fn refresh_self_liveness_resigns_when_stale() {
        let mint_t = 1_700_000_000;
        let MintResult {
            mut state,
            device_signing_key,
            ..
        } = mint_owner(mint_t).unwrap();
        refresh_self_liveness(&mut state, &device_signing_key, mint_t);
        let device_id = *state.enrollments.keys().next().unwrap();
        let old_ts = state.liveness.get(&device_id).unwrap().timestamp;

        // Advance past the refresh threshold (freshness / 2).
        let later = mint_t + harmony_owner::trust::DEFAULT_FRESHNESS_WINDOW_SECS / 2 + 1;
        let mutated = refresh_self_liveness(&mut state, &device_signing_key, later);
        assert!(mutated, "stale liveness must be re-signed");
        assert!(state.liveness.get(&device_id).unwrap().timestamp > old_ts);
    }

    #[test]
    #[serial]
    fn load_returns_none_when_no_cbor_present() {
        // No HARMONY_PASSPHRASE needed: this test exits before any secret load.
        let dir = tempdir().unwrap();
        let result = load_owner_state(dir.path(), None).expect("load");
        assert!(result.is_none(), "un-minted state must be None, not Err");
    }

    #[test]
    #[serial]
    fn load_returns_err_when_cbor_corrupt() {
        // No HARMONY_PASSPHRASE needed: this test exits before any secret load.
        let dir = tempdir().unwrap();
        let cbor_path = dir.path().join(OWNER_STATE_FILENAME);
        std::fs::write(&cbor_path, b"not-cbor-bytes").unwrap();
        let err = load_owner_state(dir.path(), None).expect_err("must be Err");
        assert!(err.contains("corrupt"), "actual: {err}");
    }

    #[test]
    #[serial]
    fn load_returns_err_when_signing_key_missing() {
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "test-pp-2");
        let dir = tempdir().unwrap();

        let MintResult {
            state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(1_700_000_001).unwrap();
        save_owner_state_atomic(
            dir.path(),
            &state,
            &device_signing_key,
            Some(recovery_artifact.as_bytes()),
            None,
        )
        .unwrap();

        // Manually wipe the device_sk.enc fallback file to simulate the
        // "inconsistent state" condition.
        let _ = std::fs::remove_file(dir.path().join("device_sk.enc"));

        let err = load_owner_state(dir.path(), None).expect_err("must be Err");
        assert!(
            err.contains("device_signing_key missing") || err.contains("inconsistent"),
            "actual: {err}"
        );
    }

    #[test]
    #[serial]
    fn load_returns_some_with_none_master_seed_when_only_seed_missing() {
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "test-pp-3");
        let dir = tempdir().unwrap();

        let MintResult {
            state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(1_700_000_002).unwrap();
        save_owner_state_atomic(
            dir.path(),
            &state,
            &device_signing_key,
            Some(recovery_artifact.as_bytes()),
            None,
        )
        .unwrap();

        // Wipe ONLY the master seed fallback — simulates the future "wipe master" action.
        let _ = std::fs::remove_file(dir.path().join("master_seed.enc"));

        let loaded = load_owner_state(dir.path(), None)
            .expect("load")
            .expect("must be Some");
        assert!(
            loaded.master_seed.is_none(),
            "degraded state: master seed gone, signing key present"
        );
    }

    #[test]
    #[serial]
    fn save_with_none_master_seed_clears_existing_seed() {
        // Regression: PR #63 review found that `save_owner_state_atomic`
        // with `master_seed: None` skipped writing but did NOT delete a
        // prior `master_seed.enc`, so a Joiner-style overwrite of a
        // previously-minted identity would silently retain the master
        // and report `canBackUp: true` — violating the cert-only model.
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "stale-test-pp");
        let dir = tempdir().unwrap();

        // Step 1: persist a fresh mint WITH master_seed.
        let MintResult {
            state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(1_700_000_500).unwrap();
        save_owner_state_atomic(
            dir.path(),
            &state,
            &device_signing_key,
            Some(recovery_artifact.as_bytes()),
            None,
        )
        .unwrap();
        assert!(
            dir.path().join("master_seed.enc").exists(),
            "sanity: master_seed.enc written by initial save"
        );

        // Step 2: simulate the Joiner-style overwrite — same identity_dir,
        // master_seed = None.
        save_owner_state_atomic(dir.path(), &state, &device_signing_key, None, None).unwrap();

        // The encrypted-file fallback MUST be gone; otherwise reload would
        // happily resurrect it and lie about backup eligibility.
        assert!(
            !dir.path().join("master_seed.enc").exists(),
            "master_seed.enc must be removed when save is called with None"
        );

        // Reload: master_seed must be None.
        let loaded = load_owner_state(dir.path(), None)
            .unwrap()
            .expect("must be Some");
        assert!(
            loaded.master_seed.is_none(),
            "loaded master_seed must be None after Joiner-style save"
        );
    }

    #[test]
    #[serial]
    fn load_returns_none_when_cbor_missing_but_keychain_orphans_present() {
        // Spec: if keychain entries exist without owner_state.cbor (e.g., from a
        // partial save_owner_state_atomic that crashed mid-sequence), load
        // returns Ok(None) — the .cbor file is the minted-marker. Orphan
        // keychain entries are tolerated and overwritten by the next mint.
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "orphan-test-pp");
        let dir = tempdir().unwrap();

        // Successful save first (writes both keychain entries + .cbor).
        let MintResult {
            state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(1_700_000_999).unwrap();
        save_owner_state_atomic(
            dir.path(),
            &state,
            &device_signing_key,
            Some(recovery_artifact.as_bytes()),
            None,
        )
        .unwrap();

        // Now simulate the partial-failure end state: delete the .cbor minted-marker
        // but leave the device_sk.enc and master_seed.enc files in place. This
        // mirrors what a save_owner_state_atomic that crashes after the keychain
        // writes but before write_atomic_0600 leaves on disk.
        std::fs::remove_file(dir.path().join(OWNER_STATE_FILENAME)).unwrap();

        let result = load_owner_state(dir.path(), None).expect("load");
        assert!(
            result.is_none(),
            "orphan keychain entries with absent .cbor must be Ok(None) (un-minted), \
             not Err. The .cbor file is the minted-marker; its absence means the \
             system is in the un-minted state regardless of keychain residue."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_serialize_with_camelcase() {
        let view = OwnerStateView {
            owner_id: "owner-hex".into(),
            owner_display_name: "zeblith".into(),
            devices: vec![DeviceView {
                device_id: "device-hex".into(),
                display_name: "KRILE".into(),
                is_this_device: true,
                trust_decision: TrustDecisionView {
                    kind: TrustKind::Full,
                    reason: None,
                },
                enrolled_at: 1_700_000_000,
                fingerprint: "3e2f·7a91".into(),
            }],
            can_back_up: true,
        };
        let json = serde_json::to_string(&view).unwrap();
        // The wire format MUST be camelCase — JS depends on this.
        assert!(json.contains("\"ownerId\""), "expected ownerId, got {json}");
        assert!(
            json.contains("\"canBackUp\""),
            "expected canBackUp, got {json}"
        );
        assert!(
            json.contains("\"isThisDevice\""),
            "expected isThisDevice, got {json}"
        );
        assert!(
            json.contains("\"trustDecision\""),
            "expected trustDecision, got {json}"
        );
        assert!(
            !json.contains("owner_id"),
            "snake_case must not leak: {json}"
        );
        // TrustKind must serialize as lowercase — camelCase does NOT lowercase single-word variants.
        assert!(
            json.contains("\"full\""),
            "expected lowercase \"full\" on wire, got {json}"
        );
        assert!(
            !json.contains("\"Full\""),
            "PascalCase \"Full\" must not appear on wire: {json}"
        );
    }
}
