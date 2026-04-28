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
        clear_token_cache();
        let mut tokens = Vec::new();
        for i in 0..(MAX_LIVE_TOKENS + 2) {
            tokens.push(insert_token(Zeroizing::new([i as u8; 32])));
        }
        // The first 2 inserted should have been LRU-evicted.
        let first_taken = take_token(&tokens[0]);
        let second_taken = take_token(&tokens[1]);
        let last_taken = take_token(&tokens[MAX_LIVE_TOKENS + 1]);
        assert!(first_taken.is_none(), "oldest must have been evicted");
        assert!(
            second_taken.is_none(),
            "second-oldest must have been evicted"
        );
        assert!(last_taken.is_some(), "newest must remain");
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

    let device_signing_key = SigningKey::from_bytes(&signing_key_bytes);

    let master_seed = load_secret(
        &keychain,
        KEYCHAIN_MASTER_SEED,
        identity_dir,
        "master_seed.enc",
    )?
    .map(|s| Zeroizing::new(s));

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
    master_seed: &[u8; 32],
    keychain: Option<KeychainStore>,
) -> Result<(), String> {
    save_secret(
        &keychain,
        KEYCHAIN_DEVICE_SK,
        identity_dir,
        "device_sk.enc",
        &device_signing_key.to_bytes(),
    )?;
    save_secret(
        &keychain,
        KEYCHAIN_MASTER_SEED,
        identity_dir,
        "master_seed.enc",
        master_seed,
    )?;
    let cbor_bytes =
        cbor::to_canonical(state).map_err(|e| format!("CBOR encode of OwnerState failed: {e}"))?;
    let cbor_path = identity_dir.join(OWNER_STATE_FILENAME);
    write_atomic_0600(&cbor_path, &cbor_bytes)
        .map_err(|e| format!("failed to write {}: {e}", cbor_path.display()))?;
    Ok(())
}

/// Load a 32-byte secret from keychain primary, encrypted-file fallback.
/// Returns `Ok(None)` when neither source has the secret.
fn load_secret(
    keychain: &Option<KeychainStore>,
    keychain_name: &str,
    identity_dir: &Path,
    fallback_filename: &str,
) -> Result<Option<[u8; 32]>, String> {
    if keychain.is_some() {
        let entry = keyring::Entry::new(KEYCHAIN_OWNER_SERVICE, keychain_name)
            .map_err(|e| format!("keychain entry creation for {keychain_name}: {e}"))?;
        match entry.get_secret() {
            Ok(bytes) => {
                if bytes.len() != 32 {
                    return Err(format!(
                        "keychain entry {KEYCHAIN_OWNER_SERVICE}/{keychain_name} length is {} bytes, expected 32",
                        bytes.len()
                    ));
                }
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                return Ok(Some(arr));
            }
            Err(keyring::Error::NoEntry) => {}
            Err(e) => return Err(format!("keychain read {keychain_name}: {e}")),
        }
    }
    // Fallback: encrypted file under HARMONY_PASSPHRASE.
    let path = identity_dir.join(fallback_filename);
    let store_opt = EncryptedFileStore::from_env(path.clone())
        .map_err(|e| format!("encrypted-file fallback for {fallback_filename}: {e}"))?;
    let store = match store_opt {
        Some(s) => s,
        None => {
            // HARMONY_PASSPHRASE not set — treat as "no fallback configured" → Ok(None).
            return Ok(None);
        }
    };
    match store.load() {
        Ok(Some(seed_bytes)) => Ok(Some(*seed_bytes)),
        Ok(None) => Ok(None), // file simply absent — natural un-minted state
        Err(e) => Err(format!("read {fallback_filename}: {e}")),
    }
}

fn save_secret(
    keychain: &Option<KeychainStore>,
    keychain_name: &str,
    identity_dir: &Path,
    fallback_filename: &str,
    bytes: &[u8; 32],
) -> Result<(), String> {
    if keychain.is_some() {
        let entry = keyring::Entry::new(KEYCHAIN_OWNER_SERVICE, keychain_name)
            .map_err(|e| format!("keychain entry creation for {keychain_name}: {e}"))?;
        return entry
            .set_secret(bytes)
            .map_err(|e| format!("keychain write {keychain_name}: {e}"));
    }
    let path = identity_dir.join(fallback_filename);
    let store = EncryptedFileStore::from_env(path.clone())
        .map_err(|e| format!("encrypted-file fallback for {fallback_filename}: {e}"))?
        .ok_or_else(|| format!("HARMONY_PASSPHRASE not set; cannot encrypt {fallback_filename}"))?;
    store
        .save(bytes)
        .map_err(|e| format!("write {fallback_filename}: {e}"))?;
    Ok(())
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

        save_owner_state_atomic(dir.path(), &state, &device_signing_key, &master_seed, None)
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
            recovery_artifact.as_bytes(),
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
            recovery_artifact.as_bytes(),
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
            recovery_artifact.as_bytes(),
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
