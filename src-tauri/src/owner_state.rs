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
    /// ZEB-668 S5: the fleet's current KeyTree epoch (0 = never bumped).
    #[serde(default)]
    pub fleet_epoch: u32,
    /// ZEB-668 S5: true when any revocation postdates the last epoch bump —
    /// that device still holds decryptable fleet material. Seed-holders see
    /// the rotate action; other devices a passive note.
    #[serde(default)]
    pub fleet_epoch_stale: bool,
    /// ZEB-677 S3: whether THIS device holds the master seed. Identical to
    /// `can_back_up` today by design — kept distinct because their future
    /// semantics diverge (spec §5): `can_back_up` speaks to backup
    /// affordances, `self_is_master` gates the quorum-ceremony surfaces.
    #[serde(default)]
    pub self_is_master: bool,
    /// ZEB-677 S4: whether THIS device may arm an enrollment co-sign window
    /// — master-less fleet, this device Master-certed, and ≥1 other active
    /// Master-certed sibling to act as the inviter. Gates the DevicesPanel
    /// "Approve adding a device" affordance (spec §5.1).
    #[serde(default)]
    pub can_arm_enrollment: bool,
    /// ZEB-677 S3: pending quorum co-sign requests (unexpired), rendered
    /// as the DevicesPanel co-sign banner / pending notes.
    #[serde(default)]
    pub quorum_requests: Vec<QuorumRequestView>,
    /// ZEB-677 S3 (surface for the S4 arm flow): wall-ms this device's
    /// enrollment co-sign window closes; `None` when not armed.
    #[serde(default)]
    pub quorum_armed_until_ms: Option<u64>,
    /// ZEB-721: seconds THIS device's own liveness cert is stamped in the future
    /// relative to the host clock at snapshot time — the host clock regressed
    /// behind an already-signed cert, pausing liveness renewal until it recovers.
    /// `None` when healthy. Drives the DevicesPanel clock-regressed banner.
    #[serde(default)]
    pub self_clock_regressed_skew_secs: Option<u64>,
}

/// ZEB-677 S3: one pending quorum co-sign request, pre-joined server-side
/// so the panel stays presentation-only (`can_cosign` encodes the whole
/// eligibility rule). Device ids are the 32-hex identity-hash form —
/// joinable against `DeviceView.device_id` for petname display.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QuorumRequestView {
    pub request_id: String,
    /// "revocation" (S4 adds "enrollment").
    pub kind: String,
    pub target_device_id: String,
    pub initiator_device_id: String,
    /// Wire label ("decommissioned" | "lost" | "compromised").
    pub reason: String,
    pub expires_at_ms: u64,
    pub initiated_by_me: bool,
    pub signed_by_me: bool,
    pub declined_by_me: bool,
    /// ANY device declined — the request is tombstoned (spec §3) and the
    /// initiator's pending note renders it as declined.
    pub declined: bool,
    /// At least one co-signature has arrived (initiator-side: completion
    /// is imminent).
    pub cosigner_signed: bool,
    /// This device may approve right now: not the initiator/target, holds
    /// a master-issued cert, has a co-sign slot, and nobody declined.
    pub can_cosign: bool,
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
    /// ZEB-418 P2 D17: true iff this device is the owner's pinned butler
    /// (fleet-net-v1 `pinned` LWW field). False when fleet-net is cold or
    /// the node is not running. Additive field — older consumers that don't
    /// read this field are unaffected.
    #[serde(default)]
    pub butler_pinned: bool,
    /// 64-char lowercase hex of the device's 32-byte ed25519 verify key —
    /// the SP1 device-id form that `set_butler_pin` validates against and
    /// the fleet-net doc keys on. `device_id` (the 16-byte identity hash)
    /// cannot be inverted to this form, so it is carried explicitly.
    #[serde(default)]
    pub device_vk_hex: String,
    /// ZEB-668 S2: revocation surface for the Removed-devices section.
    /// A revoked device keeps its enrollment row (the CRDT never deletes
    /// enrollments); these fields let the panel split active vs removed.
    #[serde(default)]
    pub revoked: bool,
    #[serde(default)]
    pub revoked_at: Option<u64>,
    /// Wire label of the revocation reason ("decommissioned" | "lost" |
    /// "compromised"; free text for the crate's `Other`).
    #[serde(default)]
    pub revoked_reason: Option<String>,
    /// ZEB-668 S4: fleet-synced petname (LWW, trimmed). `None` = never
    /// named; `Some("")` = explicitly cleared (the distinction gates the
    /// panel's one-shot local-label migration); non-empty = display name.
    #[serde(default)]
    pub pet_name: Option<String>,
    /// ZEB-668 S4: wall-clock ms of the device's last fleet-net heartbeat
    /// (`FleetNetRow.seen_at`, ~7.5-min cadence). None = never fleet-synced —
    /// the panel renders NOTHING (honesty rule), never a fabricated time.
    #[serde(default)]
    pub last_seen_ms: Option<u64>,
    /// ZEB-668 S4: true iff the device's iroh endpoint currently holds a
    /// Connected peer-liveness slot (Degraded does not count).
    #[serde(default)]
    pub connected_now: bool,
    /// ZEB-677 S3: this sibling row may be removed via the quorum co-sign
    /// ceremony — the master seed is absent here, THIS device holds a
    /// master-issued cert, and at least one other active master-certed
    /// sibling (excluding this row) can co-sign (spec §4.1). Always false
    /// on seed-holding devices (they remove directly) and on self/revoked
    /// rows.
    #[serde(default)]
    pub quorum_removable: bool,
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

use crate::identity::{write_atomic_0600, EncryptedFileStore, KeyStore, KeychainStore, VaultSlot};
use ed25519_dalek::SigningKey;
use harmony_owner::cbor;
use harmony_owner::state::OwnerState;
use std::path::Path;

const KEYCHAIN_OWNER_SERVICE: &str = "harmony.owner";
const KEYCHAIN_DEVICE_SK: &str = "device_signing_key";
const KEYCHAIN_MASTER_SEED: &str = "master_seed";
const OWNER_STATE_FILENAME: &str = "owner_state.cbor";
/// Encrypted-file fallback for the distributed fleet KeyTree material on a
/// cert-only enrolled device (ZEB-492). Variable-length HRMI `v0x02` envelope
/// (NOT the 32-byte `EncryptedFileStore` format the `*_secret` helpers use).
const FLEET_KEYTREE_FILENAME: &str = "fleet_keytree.enc";

/// Returned by `load_owner_state` when a persisted identity is found.
pub struct LoadedOwnerState {
    pub state: OwnerState,
    pub device_signing_key: SigningKey,
    /// `None` when the master seed has been wiped from this device but
    /// the rest of the owner state remains. v1 does not ship the wipe
    /// action; this case is reachable only via manual file deletion.
    /// Wrapped in `Zeroizing` so the seed is wiped on drop — matches the
    /// token cache's `Zeroizing<[u8; 32]>` discipline.
    pub master_seed: Option<Zeroizing<[u8; 32]>>,
    /// Distributed fleet KeyTree material set (ZEB-492; multi-epoch since
    /// ZEB-668 S5 — epoch-0 + current, + previous during a bump window).
    /// `Some` (non-empty) on a cert-only enrolled device given the KeyTree at
    /// pairing; `None` on the minting device + pre-ZEB-492 devices.
    pub fleet_keytree: Option<Vec<crate::owner_state_crypto::FleetKeyMaterial>>,
}

// Manual Debug so test assertions can use `.expect()` / `.expect_err()` WITHOUT
// printing key material: `FleetKeyMaterial` has no `Debug` by design (it would
// leak the KeyTree key bytes), and the master seed is likewise redacted to
// presence-only. `OwnerState`/`SigningKey` are non-secret here.
impl std::fmt::Debug for LoadedOwnerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedOwnerState")
            .field("state", &self.state)
            .field("device_signing_key", &self.device_signing_key)
            .field(
                "master_seed",
                &self.master_seed.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "fleet_keytree",
                &self.fleet_keytree.as_ref().map(|set| {
                    format!(
                        "<redacted epochs={:?}>",
                        set.iter().map(|m| m.epoch).collect::<Vec<_>>()
                    )
                }),
            )
            .finish()
    }
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
    let signing_key_bytes = load_secret(
        &keychain,
        VaultSlot::Device,
        KEYCHAIN_DEVICE_SK,
        identity_dir,
        "device_sk.enc",
    )?
    .ok_or_else(|| {
        "owner_state.cbor present but device_signing_key missing — inconsistent state".to_string()
    })?;

    // SigningKey::from_bytes copies; the Zeroizing wrapper around
    // signing_key_bytes ensures the source heap buffer wipes on drop.
    let device_signing_key = SigningKey::from_bytes(&signing_key_bytes);

    let master_seed = load_secret(
        &keychain,
        VaultSlot::OwnerMasterSeed,
        KEYCHAIN_MASTER_SEED,
        identity_dir,
        "master_seed.enc",
    )?;

    // ZEB-492 (Greptile finding): only a cert-only device (no master seed) uses
    // distributed fleet material — the boot gate treats the seed as authoritative
    // and ignores any stored material when a seed is present. So a seed-holder must
    // NOT load it: a stale/corrupt fleet_keytree.enc, a locked keychain, or a
    // missing HARMONY_PASSPHRASE must never block a seed-based boot.
    let fleet_keytree = if master_seed.is_some() {
        None
    } else {
        match load_fleet_keytree(&keychain, identity_dir)? {
            Some(bytes) => Some(
                crate::owner_state_crypto::decode_fleet_material_set(bytes.as_slice())
                    .map_err(|e| format!("decode fleet_keytree: {e}"))?,
            ),
            None => None,
        }
    };

    Ok(Some(LoadedOwnerState {
        state,
        device_signing_key,
        master_seed,
        fleet_keytree,
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
    // Snapshot the prior secrets so a mid-sequence failure on an OVERWRITE
    // (ZEB-439 forced re-adoption) can be rolled back: a fresh device key must
    // never be left orphaned against the previous, still-on-disk
    // owner_state.cbor (which is written LAST and so survives every pre-cbor
    // failure). On a fresh install (mint) both read back as `None` and the
    // rollback below is a no-op. Best-effort: an unreadable prior secret simply
    // cannot be rolled back (the status quo before this guard).
    let prev_device = load_secret(
        &keychain,
        VaultSlot::Device,
        KEYCHAIN_DEVICE_SK,
        identity_dir,
        "device_sk.enc",
    )
    .ok()
    .flatten();
    let prev_seed = load_secret(
        &keychain,
        VaultSlot::OwnerMasterSeed,
        KEYCHAIN_MASTER_SEED,
        identity_dir,
        "master_seed.enc",
    )
    .ok()
    .flatten();

    let write = || -> Result<(), String> {
        save_secret(
            &keychain,
            VaultSlot::Device,
            KEYCHAIN_DEVICE_SK,
            identity_dir,
            "device_sk.enc",
            &device_signing_key.to_bytes(),
        )?;
        if let Some(seed) = master_seed {
            save_secret(
                &keychain,
                VaultSlot::OwnerMasterSeed,
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
                VaultSlot::OwnerMasterSeed,
                KEYCHAIN_MASTER_SEED,
                identity_dir,
                "master_seed.enc",
            )?;
        }
        let cbor_bytes = cbor::to_canonical(state)
            .map_err(|e| format!("CBOR encode of OwnerState failed: {e}"))?;
        let cbor_path = identity_dir.join(OWNER_STATE_FILENAME);
        write_atomic_0600(&cbor_path, &cbor_bytes)
            .map_err(|e| format!("failed to write {}: {e}", cbor_path.display()))?;
        Ok(())
    };

    let result = write();
    if result.is_err() {
        // Roll EACH secret slot back to its prior state so the unchanged
        // owner_state.cbor stays consistent. The inverse of a write is "restore
        // the previous bytes if they existed, otherwise CLEAR the slot": leaving
        // a newly-written secret whose prior state was absent (e.g. a fresh
        // master_seed.enc over a cert-only/joiner install that had none) would
        // make `canBackUp` lie. Best-effort; the original error is preserved.
        match prev_device.as_deref() {
            Some(prev) => {
                let _ = save_secret(
                    &keychain,
                    VaultSlot::Device,
                    KEYCHAIN_DEVICE_SK,
                    identity_dir,
                    "device_sk.enc",
                    prev,
                );
            }
            None => {
                let _ = clear_secret(
                    &keychain,
                    VaultSlot::Device,
                    KEYCHAIN_DEVICE_SK,
                    identity_dir,
                    "device_sk.enc",
                );
            }
        }
        match prev_seed.as_deref() {
            Some(prev) => {
                let _ = save_secret(
                    &keychain,
                    VaultSlot::OwnerMasterSeed,
                    KEYCHAIN_MASTER_SEED,
                    identity_dir,
                    "master_seed.enc",
                    prev,
                );
            }
            None => {
                let _ = clear_secret(
                    &keychain,
                    VaultSlot::OwnerMasterSeed,
                    KEYCHAIN_MASTER_SEED,
                    identity_dir,
                    "master_seed.enc",
                );
            }
        }
    }
    result
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

/// Load ONLY the `OwnerState` CRDT from `owner_state.cbor` — no keys, no
/// keychain. ZEB-668 S1: the trust-replication engine's FileOnly access
/// mode and tests need a doc-only reader; [`load_owner_state`] insists on
/// the device signing key being present, which CLI/file-mode trust
/// mutations don't require.
pub fn load_owner_state_cbor(identity_dir: &Path) -> Result<OwnerState, String> {
    let cbor_path = identity_dir.join(OWNER_STATE_FILENAME);
    let cbor_bytes = std::fs::read(&cbor_path)
        .map_err(|e| format!("failed to read {}: {e}", cbor_path.display()))?;
    cbor::from_bytes(&cbor_bytes).map_err(|e| format!("owner_state.cbor is corrupt: {e}"))
}

/// Derive the local device's 16-byte id from its ed25519 signing key.
///
/// Single source of truth for the device-id mapping, shared by the Devices-panel
/// view (`owner_commands::derive_this_device_id`) and [`refresh_self_liveness`].
/// Keeping it in one place stops the two derivations drifting — drift would make
/// the refresh sign liveness under a different id than the enrolled device and
/// silently stop self-healing trust.
pub fn device_id_from_signing_key(device_sk: &SigningKey) -> [u8; 16] {
    harmony_owner::pubkey_bundle::PubKeyBundle::classical_only(device_sk.verifying_key().to_bytes())
        .identity_hash()
}

/// Re-adopt an existing owner identity from its 32-byte master `seed`
/// (ZEB-439 restore). A constrained variant of
/// [`harmony_owner::lifecycle::mint_owner`] that takes the seed as input
/// instead of generating it: the reconstructed [`OwnerState`] keeps the SAME
/// `owner_id` (= `RecoveryArtifact::from_seed(seed).master_pubkey_bundle()
/// .identity_hash()` — the invariant `pairing/cert.rs::sign_enrollment_for_joiner`
/// enforces), but a FRESH device key is minted and master-signed into a new
/// enrollment. The old device's key is never recovered: this is re-adoption of
/// the owner on a new/wiped device. An initial `LivenessCert` is stamped so the
/// sole device evaluates `Full`, not `Refused(StaleTrustState)` (ZEB-342 parity).
///
/// Returns the reconstructed `(OwnerState, device_signing_key)`; the caller
/// persists via [`save_owner_state_atomic`] with `Some(seed)`.
pub fn remint_owner_from_seed(
    seed: &[u8; 32],
    now: u64,
) -> Result<(OwnerState, SigningKey), String> {
    use harmony_owner::certs::LivenessCert;
    use harmony_owner::lifecycle::{enroll_via_master, RecoveryArtifact};
    use harmony_owner::pubkey_bundle::{ClassicalKeys, PubKeyBundle};
    use harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS;

    let artifact = RecoveryArtifact::from_seed(*seed);
    let owner_id = artifact.master_pubkey_bundle().identity_hash();
    let mut state = OwnerState::new(owner_id);

    // Fresh per-device key — the old device's key is intentionally NOT
    // recovered. The seed reconstructs the OWNER (owner_id), not the device.
    let device_sk = SigningKey::generate(&mut rand::rngs::OsRng);
    let device_x25519 =
        crate::dm_signing::ed25519_pub_to_x25519(&device_sk.verifying_key().to_bytes())
            .map_err(|e| format!("device x25519 derivation failed: {e}"))?;
    let device_bundle = PubKeyBundle {
        classical: ClassicalKeys {
            ed25519_verify: device_sk.verifying_key().to_bytes(),
            x25519_pub: device_x25519,
        },
        post_quantum: None,
    };

    // enroll_via_master reconstructs the master key from the artifact, enforces
    // `master.identity_hash() == state.owner_id` (returns WrongOwner otherwise),
    // master-signs the enrollment, and drops the master key. For a fresh restore
    // there are no active siblings, so `auto_vouch_certs` is empty.
    let enrolled = enroll_via_master(
        &state,
        &artifact,
        &device_sk,
        device_bundle,
        now,
        DEFAULT_ACTIVE_WINDOW_SECS,
    )
    .map_err(|e| format!("enroll device under restored owner failed: {e}"))?;

    state
        .add_enrollment(enrolled.enrollment_cert, now, DEFAULT_ACTIVE_WINDOW_SECS)
        .map_err(|e| format!("add_enrollment failed: {e}"))?;
    for vouch in enrolled.auto_vouch_certs {
        state
            .add_vouching(vouch)
            .map_err(|e| format!("add_vouching failed: {e}"))?;
    }

    // ZEB-342 parity: stamp initial liveness so the sole device evaluates Full,
    // not Refused(StaleTrustState).
    let liveness = LivenessCert::sign(&device_sk, owner_id, now)
        .map_err(|e| format!("liveness sign failed: {e}"))?;
    state
        .add_liveness(liveness)
        .map_err(|e| format!("add_liveness failed: {e}"))?;

    Ok((state, device_sk))
}

/// Read just the `owner_id` from a persisted `owner_state.cbor`, without
/// touching the keychain or the device/seed secrets.
///
/// Returns `Ok(None)` for the natural un-minted state (no `.cbor` marker).
/// Used by the ZEB-439 restore overwrite-guard, which must compare the
/// mnemonic-derived owner_id against any identity already on this device
/// *before* deciding whether `--force` is required — a check that needs the
/// recorded owner_id but neither the device key nor the master seed.
pub fn read_persisted_owner_id(identity_dir: &Path) -> Result<Option<[u8; 16]>, String> {
    let cbor_path = identity_dir.join(OWNER_STATE_FILENAME);
    if !cbor_path.exists() {
        return Ok(None);
    }
    let cbor_bytes = std::fs::read(&cbor_path)
        .map_err(|e| format!("failed to read {}: {e}", cbor_path.display()))?;
    let state: OwnerState =
        cbor::from_bytes(&cbor_bytes).map_err(|e| format!("owner_state.cbor is corrupt: {e}"))?;
    Ok(Some(state.owner_id))
}

/// ZEB-491 (secondary): read the LIVE enrolled-device set from `owner_state.cbor`
/// as 64-hex ed25519 verify keys — the same derivation `start_node` uses to seed
/// `NodeState.fleet_net_enrolled`, but read on demand so it reflects enrollments
/// that landed AFTER boot (e.g. a device just paired in this session).
///
/// Keychain-free by design (sibling of [`read_persisted_owner_id`]): it only
/// parses the public `OwnerState` CBOR, so it's safe to call on hot validation
/// paths like `set_butler_pin` without touching the credential store. Returns an
/// empty set when no identity has been minted yet.
pub fn read_enrolled_device_vk_hex(
    identity_dir: &Path,
) -> Result<std::collections::BTreeSet<String>, String> {
    let cbor_path = identity_dir.join(OWNER_STATE_FILENAME);
    if !cbor_path.exists() {
        return Ok(std::collections::BTreeSet::new());
    }
    let cbor_bytes = std::fs::read(&cbor_path)
        .map_err(|e| format!("failed to read {}: {e}", cbor_path.display()))?;
    let state: OwnerState =
        cbor::from_bytes(&cbor_bytes).map_err(|e| format!("owner_state.cbor is corrupt: {e}"))?;
    Ok(state
        .enrollments
        .values()
        .map(|cert| hex::encode(cert.device_pubkeys.classical.ed25519_verify))
        .collect())
}

/// Outcome of a self-liveness refresh attempt (ZEB-721). `Refreshed` is the only
/// variant that mutated `state`; the others explain *why* nothing was written so
/// callers can persist-on-write and surface clock health from one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivenessRefreshOutcome {
    /// Re-signed a fresh cert at `now`. Caller MUST persist + notify_dirty.
    Refreshed,
    /// Existing cert is still fresh (< freshness/2 old). Healthy steady state.
    Fresh,
    /// Our own cert is stamped in the FUTURE relative to `now` — the host clock
    /// regressed behind it. Not re-signed (a lower timestamp loses the liveness
    /// CRDT merge, and fabricating time is not our posture, ZEB-721). Self-heals
    /// when the clock recovers; surfaced instead of a silent no-op.
    ClockRegressed { skew_secs: u64 },
    /// Signing or `add_liveness` failed (already warn-logged). A no-op to callers.
    SignFailed,
}

impl LivenessRefreshOutcome {
    /// True iff the call mutated `state` (caller must persist + notify_dirty).
    pub fn wrote(self) -> bool {
        matches!(self, Self::Refreshed)
    }
}

/// Seconds this device's own liveness cert is stamped in the FUTURE relative to
/// `now` — i.e. the host clock regressed behind our own cert. `None` when there
/// is no self-cert or it is at/behind `now` (healthy). Shared by the refresh
/// decision and the `OwnerStateView` surfacing so both agree. ZEB-721.
pub fn self_liveness_future_skew_secs(
    state: &OwnerState,
    device_sk: &SigningKey,
    now: u64,
) -> Option<u64> {
    let device_id = device_id_from_signing_key(device_sk);
    state
        .liveness
        .get(&device_id)
        .and_then(|c| c.timestamp.checked_sub(now))
        .filter(|&skew| skew > 0)
}

/// Ensure the local device (derived from `device_sk`) has a fresh `LivenessCert`
/// in `state`. Returns a [`LivenessRefreshOutcome`]; only `Refreshed` mutated
/// `state` (caller must then persist via [`save_owner_state_cbor_only`] /
/// `notify_dirty`).
///
/// ZEB-342: without an active (liveness-bearing) local device, `evaluate_trust`
/// refuses the sole device with `StaleTrustState` (the fresh-mint "● refused"
/// badge). Re-signs when the local device has no liveness or its liveness is
/// older than `DEFAULT_FRESHNESS_WINDOW_SECS / 2` (~15 days), bounding writes to
/// ~once per boot per fortnight.
///
/// ZEB-721: if our own cert is stamped in the *future* relative to `now`, the
/// host clock regressed behind it. We do NOT re-sign (a lower timestamp loses the
/// liveness CRDT merge, and fabricating time is not our posture) — we report
/// `ClockRegressed { skew_secs }` so callers can surface it. Logging is left to
/// the callers so it can be deduplicated: the heartbeat warns only on the
/// healthy→regressed transition, and the Devices panel shows a banner. This is
/// the single detection point shared by the panel-load and heartbeat call sites.
pub fn refresh_self_liveness(
    state: &mut OwnerState,
    device_sk: &SigningKey,
    now: u64,
) -> LivenessRefreshOutcome {
    use harmony_owner::certs::LivenessCert;

    let device_id = device_id_from_signing_key(device_sk);
    let threshold = harmony_owner::trust::DEFAULT_FRESHNESS_WINDOW_SECS / 2;
    match state.liveness.get(&device_id) {
        // Regressed clock: cert looks fresh forever → renewal suppressed. Report
        // it (callers log/surface, deduplicated) instead of a silent no-op;
        // self-heals when the clock recovers.
        Some(cert) if cert.timestamp > now => LivenessRefreshOutcome::ClockRegressed {
            skew_secs: cert.timestamp - now,
        },
        // Fresh enough — nothing to do.
        Some(cert) if cert.timestamp >= now.saturating_sub(threshold) => {
            LivenessRefreshOutcome::Fresh
        }
        // Stale or missing → (re-)sign.
        _ => match LivenessCert::sign(device_sk, state.owner_id, now) {
            Ok(cert) => match state.add_liveness(cert) {
                Ok(()) => LivenessRefreshOutcome::Refreshed,
                Err(e) => {
                    tracing::warn!(error = %e, "refresh_self_liveness: add_liveness failed");
                    LivenessRefreshOutcome::SignFailed
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "refresh_self_liveness: liveness sign failed");
                LivenessRefreshOutcome::SignFailed
            }
        },
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
    slot: VaultSlot,
    keychain_name: &str,
    identity_dir: &Path,
    fallback_filename: &str,
) -> Result<Option<Zeroizing<[u8; 32]>>, String> {
    // Track non-NoEntry keychain errors so we can propagate them rather
    // than silently masking a locked/permission-denied keychain as an
    // un-minted state when no encrypted-file fallback is configured.
    let mut keychain_err: Option<String> = None;
    if keychain.is_some() {
        // ZEB-363: this owner secret is consolidated into the single
        // `harmony`/`identity` keychain vault slot. `vault_load_slot` reads the
        // slot, folding in (and deleting after a verified read-back) the legacy
        // `harmony.owner`/<name> item if the vault doesn't have it yet.
        let legacy = keyring::Entry::new(KEYCHAIN_OWNER_SERVICE, keychain_name)
            .map_err(|e| format!("keychain entry creation for {keychain_name}: {e}"))?;
        match crate::identity::vault_load_slot(slot, &legacy) {
            Ok(Some(key)) => return Ok(Some(key)),
            Ok(None) => {}
            Err(e) => {
                // Don't hard-fail: a flaky/locked keychain shouldn't break load
                // when the encrypted-file fallback is configured.
                let msg = format!("vault slot read {KEYCHAIN_OWNER_SERVICE}/{keychain_name}: {e}");
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
    slot: VaultSlot,
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
    // True whenever we fall through to the encrypted-file fallback instead of
    // landing the secret in the keychain vault — either there is no vault item
    // (`Ok(false)`) OR the vault is unreadable (`Err`). In BOTH states the
    // symmetric `load_secret` (via `vault_load_slot`) reads the legacy
    // `harmony.owner`/<name> item directly (the no-vault and corrupt-vault paths
    // both degrade to the legacy item), so a stale legacy entry would shadow the
    // value we are about to write to the file. We clear it after a successful
    // file write (below). (Cursor.)
    let mut fell_through_to_enc = false;
    if keychain.is_some() {
        // ZEB-363: write into the consolidated harmony/identity vault slot
        // (read-modify-write, preserving the other slots). Ok(false) => there is
        // no keychain vault item (keychain-less seed) — fall through to the file.
        match crate::identity::vault_save_slot(slot, bytes) {
            Ok(true) => return Ok(()),
            Ok(false) => fell_through_to_enc = true,
            Err(e) => {
                // Don't hard-fail: a flaky/locked/unreadable keychain shouldn't
                // block mint when the encrypted-file fallback is configured.
                let msg = format!("vault slot write {KEYCHAIN_OWNER_SERVICE}/{keychain_name}: {e}");
                tracing::warn!("{msg}; falling through to encrypted-file fallback");
                keychain_err = Some(msg);
                fell_through_to_enc = true;
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
    // The secret now lives durably in the encrypted file. Since we fell through to
    // it (no vault item, or an unreadable one), best-effort delete the legacy
    // `harmony.owner`/<name> keychain item so a later `vault_load_slot` (which
    // reads the legacy item directly on both the no-vault and corrupt-vault paths)
    // reads the file rather than a stale legacy entry. Only after the file write
    // succeeds (no data loss on a failed write). Best-effort: a delete failure is
    // logged, not fatal — on a corrupt-but-readable keychain the delete succeeds
    // and removes the shadow; on a locked keychain it no-ops.
    if fell_through_to_enc {
        if let Ok(legacy) = keyring::Entry::new(KEYCHAIN_OWNER_SERVICE, keychain_name) {
            if let Err(e) = legacy.delete_credential() {
                if !matches!(e, keyring::Error::NoEntry) {
                    tracing::warn!(
                        "could not delete stale legacy owner keychain item \
                         {KEYCHAIN_OWNER_SERVICE}/{keychain_name} after encrypted-file save: {e}"
                    );
                }
            }
        }
    }
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
    slot: VaultSlot,
    keychain_name: &str,
    identity_dir: &Path,
    fallback_filename: &str,
) -> Result<(), String> {
    let mut keychain_err: Option<String> = None;
    if keychain.is_some() {
        // ZEB-363: clear the consolidated vault slot AND best-effort delete the
        // legacy `harmony.owner` item, so a stale legacy entry can't resurrect
        // the master seed on a later no-keychain load.
        let legacy = keyring::Entry::new(KEYCHAIN_OWNER_SERVICE, keychain_name)
            .map_err(|e| format!("keychain entry creation for {keychain_name}: {e}"))?;
        if let Err(e) = crate::identity::vault_clear_slot(slot, &legacy) {
            keychain_err = Some(format!(
                "vault slot clear {KEYCHAIN_OWNER_SERVICE}/{keychain_name}: {e}"
            ));
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

// ── Distributed fleet KeyTree persistence (ZEB-492) ────────────────────────
//
// The fleet KeyTree material (CBOR of `FleetKeyMaterial`, ~161 bytes) is
// VARIABLE-LENGTH, so it cannot use the 32-byte `*_secret` helpers above.
// These mirror their keychain-preferred + encrypted-file-fallback structure
// but operate on the variable-length `SecretVault::fleet_keytree` field and the
// HRMI `v0x02` envelope (`identity::encrypt_vault_bytes` /
// `decrypt_vault_bytes`) instead of `EncryptedFileStore` (which is 32-byte).

/// Persist the distributed fleet KeyTree `material` (CBOR of `FleetKeyMaterial`)
/// for a cert-only enrolled device. Keychain-vault-preferred, falling back to a
/// variable-length encrypted file (`HARMONY_PASSPHRASE` / `HARMONY_PASSPHRASE_FILE`).
///
/// Live as of ZEB-492 Task 4: `pairing::persist::install_joiner_state_inner`
/// calls this when the inviter sealed fleet material into the ENROLL payload.
pub(crate) fn save_fleet_keytree(
    keychain: &Option<KeychainStore>,
    identity_dir: &Path,
    material: &[u8],
) -> Result<(), String> {
    // Mirror save_secret's error-preservation: if the keychain WRITE fails AND
    // no encrypted-file fallback is configured, surface the original keychain
    // error (locked / permission denied / etc) instead of the generic
    // "HARMONY_PASSPHRASE not set" message — otherwise the caller reports the
    // wrong remediation.
    let mut keychain_err: Option<String> = None;
    // True whenever we fall through to the encrypted file instead of landing the
    // material in the keychain vault — either no vault item (`Ok(false)`) OR the
    // vault write failed (`Err`). In BOTH states, `load_fleet_keytree` prefers
    // the keychain vault over the file, so a stale vault value would SHADOW the
    // fresh file we are about to write (e.g. re-pair with new material under a
    // transient keychain-write failure). We best-effort clear the vault slot
    // after a successful file write (below). Mirrors `save_secret`'s
    // `fell_through_to_enc` intent (ZEB-492 Qodo/CodeAnt round 1, FIX C).
    let mut fell_through_to_enc = false;
    if keychain.is_some() {
        match crate::identity::vault_save_fleet_keytree(material) {
            Ok(true) => return Ok(()),
            // No keychain vault item (keychain-less seed) — fall through to file.
            Ok(false) => fell_through_to_enc = true,
            Err(e) => {
                let msg = format!("vault fleet-keytree write: {e}");
                tracing::warn!("{msg}; falling through to file");
                keychain_err = Some(msg);
                fell_through_to_enc = true;
            }
        }
    }
    let passphrase = match crate::identity::resolve_passphrase_env()
        .map_err(|e| format!("fleet-keytree fallback: {e}"))?
    {
        Some(p) => p,
        None => {
            return Err(keychain_err.unwrap_or_else(|| {
                "HARMONY_PASSPHRASE not set; cannot encrypt fleet_keytree.enc".to_string()
            }));
        }
    };
    let blob = crate::identity::encrypt_vault_bytes(
        secrecy::ExposeSecret::expose_secret(&passphrase).as_bytes(),
        material,
    );
    let path = identity_dir.join(FLEET_KEYTREE_FILENAME);
    write_atomic_0600(&path, &blob).map_err(|e| format!("write {}: {e}", path.display()))?;
    // The material now lives durably in the encrypted file. Since we fell through
    // to it (no vault item, or an unreadable/failed-write one), best-effort CLEAR
    // any stale keychain vault fleet-keytree slot so a later `load_fleet_keytree`
    // (which prefers the vault) cannot return STALE material and ignore the fresh
    // file. Only after the file write succeeds (no data loss on a failed write).
    // Best-effort: a clear failure is logged, not fatal — on a locked keychain it
    // can't clear, but it also can't read (vault_load_fleet_keytree propagates
    // that Err), so the stale-shadow window is bounded by the keychain itself.
    if fell_through_to_enc {
        if let Err(e) = crate::identity::vault_clear_fleet_keytree() {
            tracing::warn!(
                "could not clear stale keychain fleet-keytree slot after encrypted-file save: {e}"
            );
        }
    }
    Ok(())
}

/// Clear any persisted fleet KeyTree material from BOTH the keychain vault slot
/// AND the encrypted-file fallback. Idempotent: absence is silent.
///
/// ZEB-492 carry-forward (Task-2 review): a re-pairing / re-adoption that
/// carries NO fleet material must not leave a stale `fleet_keytree.enc` (or
/// vault slot) from a prior identity masquerading as this device's KeyTree —
/// mirrors how `save_owner_state_atomic` clears `master_seed` when `None`.
/// Best-effort like `clear_secret`: a vault clear failure is captured and
/// returned AFTER the file is removed, so at least one store ends up clean.
pub(crate) fn clear_fleet_keytree(
    keychain: &Option<KeychainStore>,
    identity_dir: &Path,
) -> Result<(), String> {
    let mut keychain_err: Option<String> = None;
    if keychain.is_some() {
        if let Err(e) = crate::identity::vault_clear_fleet_keytree() {
            keychain_err = Some(format!("vault fleet-keytree clear: {e}"));
        }
    }
    let path = identity_dir.join(FLEET_KEYTREE_FILENAME);
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

/// Load the distributed fleet KeyTree material for a cert-only enrolled device.
/// `Ok(None)` on the minting device + pre-ZEB-492 devices (neither the vault nor
/// the encrypted file carries it). Keychain-vault-preferred, falling back to the
/// variable-length encrypted file. The returned buffer is `Zeroizing`.
pub(crate) fn load_fleet_keytree(
    keychain: &Option<KeychainStore>,
    identity_dir: &Path,
) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
    // Mirror load_secret's keychain-error preservation (ZEB-492 Qodo/CodeAnt
    // round 1, FIX B): a locked/unreadable keychain must NOT be masked as
    // "genuinely no material" — that would silently boot a cert-only device with
    // no fleet engines. Capture the read error, try the file fallback, and
    // surface the keychain error only if no file fallback is usable.
    let mut keychain_err: Option<String> = None;
    if keychain.is_some() {
        match crate::identity::vault_load_fleet_keytree() {
            Ok(Some(v)) => return Ok(Some(v)),
            // Genuine absence in the vault — fall through to the file.
            Ok(None) => {}
            Err(e) => {
                let msg = format!("vault fleet-keytree read: {e}");
                tracing::warn!("{msg}; trying file fallback");
                keychain_err = Some(msg);
            }
        }
    }
    let path = identity_dir.join(FLEET_KEYTREE_FILENAME);
    if !path.exists() {
        // No file fallback. If the keychain READ failed earlier (locked /
        // permission denied / corrupt vault), surface it — otherwise the boot
        // gate would misclassify an unreadable keychain as "no fleet material"
        // and silently build no fleet engines. Genuine absence (Ok(None) from
        // the vault + no file) still returns Ok(None).
        return match keychain_err {
            Some(e) => Err(e),
            None => Ok(None),
        };
    }
    let passphrase = crate::identity::resolve_passphrase_env()
        .map_err(|e| format!("fleet-keytree fallback: {e}"))?
        .ok_or_else(|| "fleet_keytree.enc present but HARMONY_PASSPHRASE not set".to_string())?;
    let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let plaintext = crate::identity::decrypt_vault_bytes(
        secrecy::ExposeSecret::expose_secret(&passphrase).as_bytes(),
        &bytes,
    )
    .map_err(|e| format!("decrypt {}: {e}", path.display()))?;
    Ok(Some(plaintext))
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
    fn fleet_keytree_save_load_round_trips_via_encrypted_file() {
        // keychain: None forces the variable-length encrypted-file fallback (no
        // real keychain reached — ZEB-428). EnvVarGuard + #[serial] keep the
        // HARMONY_PASSPHRASE mutation from racing other persistence tests.
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "test-pass-zeb492");
        let dir = tempdir().unwrap();
        let material = vec![0xABu8; 161];
        save_fleet_keytree(&None, dir.path(), &material).expect("save");
        let loaded = load_fleet_keytree(&None, dir.path()).expect("load");
        assert_eq!(loaded.as_deref().map(Vec::as_slice), Some(&material[..]));
    }

    #[test]
    #[serial]
    fn fleet_keytree_absent_loads_none() {
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "test-pass-zeb492b");
        let dir = tempdir().unwrap();
        assert!(load_fleet_keytree(&None, dir.path())
            .expect("load")
            .is_none());
    }

    /// ZEB-492 carry-forward #3: a corrupt `fleet_keytree.enc` (garbage, not a
    /// valid v0x02 envelope) must surface as a clear Err — NOT a panic, and NOT
    /// silently-None (which would strand a cert-only device with no fleet
    /// engines while masking the on-disk corruption).
    #[test]
    #[serial]
    fn fleet_keytree_corrupt_file_errors_not_panics() {
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "test-pass-zeb492c");
        let dir = tempdir().unwrap();
        let path = dir.path().join(FLEET_KEYTREE_FILENAME);
        std::fs::write(&path, b"not a valid v0x02 envelope at all").unwrap();
        let err = load_fleet_keytree(&None, dir.path())
            .expect_err("corrupt fleet_keytree.enc must error");
        assert!(
            err.contains("decrypt") && err.contains(FLEET_KEYTREE_FILENAME),
            "error must name the decrypt failure + file, got: {err}"
        );
    }

    /// ZEB-492 (Qodo/CodeAnt round 1, FIX A): a TRUNCATED `fleet_keytree.enc`
    /// (shorter than the envelope header) routed through `load_fleet_keytree`
    /// must surface as an Err — NOT a panic. The prior corrupt-file test used a
    /// long-enough buffer that failed at the AEAD step; this SHORT-input case
    /// exercises the `MIN_LEN`/header out-of-bounds guard added to
    /// `decrypt_vault_bytes`. 20 bytes is below the envelope minimum.
    #[test]
    #[serial]
    fn fleet_keytree_short_file_errors_not_panics() {
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "test-pass-zeb492d");
        let dir = tempdir().unwrap();
        let path = dir.path().join(FLEET_KEYTREE_FILENAME);
        std::fs::write(&path, [0u8; 20]).unwrap();
        let err = load_fleet_keytree(&None, dir.path())
            .expect_err("a 20-byte fleet_keytree.enc must error, not panic");
        assert!(
            err.contains("decrypt") && err.contains(FLEET_KEYTREE_FILENAME),
            "error must name the decrypt failure + file, got: {err}"
        );
    }

    /// ZEB-492 (Greptile finding): a seed-holding device must boot even when a
    /// corrupt `fleet_keytree.enc` is present on disk. The boot gate uses the
    /// authoritative master seed and ignores distributed fleet material, so
    /// `load_owner_state` must NOT attempt to load (and fail on) that material
    /// when a seed is present. Realistic trigger: reuse a profile that once held
    /// cert-only fleet material, then re-adopt it with a seed.
    #[test]
    #[serial]
    fn seed_holder_boots_despite_corrupt_fleet_keytree() {
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "seed-corrupt-fleet-pp");
        let dir = tempdir().unwrap();
        let MintResult {
            state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(1_700_000_222).unwrap();
        // Construct a seed-holding owner identity on disk (master_seed present).
        save_owner_state_atomic(
            dir.path(),
            &state,
            &device_signing_key,
            Some(recovery_artifact.as_bytes()),
            None,
        )
        .unwrap();

        // Garbage fleet material: would surface an Err for a cert-only device
        // (round-1 FIX B), but must be IGNORED for a seed-holder.
        std::fs::write(dir.path().join(FLEET_KEYTREE_FILENAME), [0xFFu8; 20]).unwrap();

        let loaded = load_owner_state(dir.path(), None)
            .expect("seed-holder must boot despite corrupt fleet_keytree.enc")
            .expect("must be Some");
        assert!(
            loaded.master_seed.is_some(),
            "seed must be loaded (authoritative boot material)"
        );
        assert!(
            loaded.fleet_keytree.is_none(),
            "corrupt fleet material must NOT be loaded for a seed-holder"
        );
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
    fn remint_from_seed_preserves_owner_id_with_fresh_full_trust_device() {
        // ZEB-439: re-adopting an owner from its master seed must reproduce the
        // SAME owner_id (so peers/communities still recognize the identity),
        // mint a DIFFERENT device key (the old device key is not recovered),
        // and stamp liveness so the sole device evaluates Full (ZEB-342 parity).
        let MintResult {
            state: original,
            recovery_artifact,
            device_signing_key: original_device,
        } = mint_owner(1_700_000_000).unwrap();
        let seed = *recovery_artifact.as_bytes();

        let now = 1_700_500_000;
        let (restored, restored_device) =
            remint_owner_from_seed(&seed, now).expect("remint from seed");

        assert_eq!(
            restored.owner_id, original.owner_id,
            "restored identity must keep the same owner_id"
        );
        assert_ne!(
            restored_device.to_bytes(),
            original_device.to_bytes(),
            "restore mints a fresh device key; it must not equal the old one"
        );
        assert_eq!(
            restored.enrollments.len(),
            1,
            "exactly one device enrolled after restore"
        );

        let device_id = device_id_from_signing_key(&restored_device);
        assert_eq!(
            harmony_owner::trust::evaluate_trust(
                &restored,
                device_id,
                now,
                harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS,
                harmony_owner::trust::DEFAULT_FRESHNESS_WINDOW_SECS,
            ),
            harmony_owner::trust::TrustDecision::Full,
            "the sole restored device must evaluate to Full trust"
        );
    }

    #[test]
    #[serial]
    fn save_owner_state_atomic_rolls_back_secrets_when_cbor_write_fails() {
        // ZEB-439 / CodeRabbit: an OVERWRITE that fails AFTER the secret writes
        // but during the cbor write must not orphan a fresh device key against
        // the prior owner_state.cbor. Inject a cbor-write failure (replace the
        // marker with a non-empty directory so write_atomic_0600's rename onto
        // it fails — the secret file writes still land) and assert the secret
        // slots roll back to the prior identity.
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "rollback-test-pp");
        let dir = tempdir().unwrap();

        // Plant owner A.
        let a = mint_owner(1_700_000_000).unwrap();
        let a_seed = *a.recovery_artifact.as_bytes();
        let a_device = a.device_signing_key.to_bytes();
        save_owner_state_atomic(
            dir.path(),
            &a.state,
            &a.device_signing_key,
            Some(&a_seed),
            None,
        )
        .expect("plant A");

        // Turn owner_state.cbor into a non-empty directory so the final rename fails.
        let cbor = dir.path().join(OWNER_STATE_FILENAME);
        std::fs::remove_file(&cbor).unwrap();
        std::fs::create_dir(&cbor).unwrap();
        std::fs::write(cbor.join("blocker"), b"x").unwrap();

        // Attempt to overwrite with a DIFFERENT owner B — must fail at the cbor write.
        let b = mint_owner(1_700_000_100).unwrap();
        let b_seed = *b.recovery_artifact.as_bytes();
        let err = save_owner_state_atomic(
            dir.path(),
            &b.state,
            &b.device_signing_key,
            Some(&b_seed),
            None,
        )
        .expect_err("cbor write onto a non-empty dir must fail");
        assert!(
            err.contains("owner_state.cbor"),
            "expected a cbor write error; got: {err}"
        );

        // Restore A's cbor marker, then load: the secret slots must be A's
        // (rolled back), NOT B's.
        std::fs::remove_dir_all(&cbor).unwrap();
        save_owner_state_cbor_only(dir.path(), &a.state).expect("re-plant A cbor");
        let loaded = load_owner_state(dir.path(), None)
            .expect("load")
            .expect("Some");
        assert_eq!(
            loaded.device_signing_key.to_bytes(),
            a_device,
            "device key must roll back to A's after the failed overwrite"
        );
        assert_eq!(
            loaded.master_seed.as_deref(),
            Some(&a_seed),
            "master seed must roll back to A's after the failed overwrite"
        );
    }

    #[test]
    #[serial]
    fn save_owner_state_atomic_rollback_clears_seed_absent_in_prior_state() {
        // Cursor: if the PRIOR state had no master seed (cert-only / joiner) and
        // a failed overwrite wrote a fresh master_seed.enc, rollback must CLEAR
        // it — not just skip it. Leaving the new seed would make `canBackUp` lie
        // (seed on disk, but the unchanged owner_state.cbor never had one). The
        // correct inverse of a write is "restore prior value OR remove if none".
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "rollback-clear-test-pp");
        let dir = tempdir().unwrap();

        // Plant a prior state with NO master seed (cert-only): save with `None`.
        let a = mint_owner(1_700_000_000).unwrap();
        save_owner_state_atomic(dir.path(), &a.state, &a.device_signing_key, None, None)
            .expect("plant cert-only A");
        assert!(
            !dir.path().join("master_seed.enc").exists(),
            "precondition: prior state has no master seed on disk"
        );

        // Make the cbor write fail.
        let cbor = dir.path().join(OWNER_STATE_FILENAME);
        std::fs::remove_file(&cbor).unwrap();
        std::fs::create_dir(&cbor).unwrap();
        std::fs::write(cbor.join("blocker"), b"x").unwrap();

        // Attempt an overwrite that WRITES a master seed, failing at the cbor.
        let b = mint_owner(1_700_000_100).unwrap();
        let b_seed = *b.recovery_artifact.as_bytes();
        let err = save_owner_state_atomic(
            dir.path(),
            &b.state,
            &b.device_signing_key,
            Some(&b_seed),
            None,
        )
        .expect_err("cbor write onto a non-empty dir must fail");
        assert!(
            err.contains("owner_state.cbor"),
            "expected a cbor write error; got: {err}"
        );

        // Rollback must have CLEARED the newly-written seed (prior had none).
        std::fs::remove_dir_all(&cbor).unwrap();
        save_owner_state_cbor_only(dir.path(), &a.state).expect("re-plant A cbor");
        let loaded = load_owner_state(dir.path(), None)
            .expect("load")
            .expect("Some");
        assert!(
            loaded.master_seed.is_none(),
            "a master seed written during a failed overwrite must be rolled back (cleared) \
             when the prior state had none"
        );
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
        let loaded = load_owner_state(dir.path(), None)
            .unwrap()
            .expect("must be Some");
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

    /// ZEB-491 (secondary): `read_enrolled_device_vk_hex` must reflect an
    /// enrollment added AFTER the initial mint (i.e. a device paired in-session)
    /// without any restart. This is the disk-side seam that lets `set_butler_pin`
    /// validate against the LIVE enrolled set instead of the boot snapshot, so a
    /// freshly-paired device can be pinned immediately.
    #[test]
    fn read_enrolled_vk_hex_sees_post_boot_enrollment() {
        use harmony_owner::lifecycle::{enroll_via_master, RecoveryArtifact};
        use harmony_owner::pubkey_bundle::{ClassicalKeys, PubKeyBundle};
        use harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS;

        let dir = tempdir().unwrap();

        // Empty/unminted dir → empty set (no owner_state.cbor yet).
        assert!(
            read_enrolled_device_vk_hex(dir.path()).unwrap().is_empty(),
            "no identity minted yet → empty enrolled set"
        );

        // Mint: exactly one enrollment (the boot/first device).
        let MintResult {
            mut state,
            recovery_artifact,
            ..
        } = mint_owner(1_700_000_000).unwrap();
        let seed = *recovery_artifact.as_bytes();
        save_owner_state_cbor_only(dir.path(), &state).unwrap();

        let after_mint = read_enrolled_device_vk_hex(dir.path()).unwrap();
        assert_eq!(after_mint.len(), 1, "boot snapshot has the first device");

        // Simulate an in-session pairing: master-sign + add a SECOND device's
        // enrollment, then persist cbor-only (no remint, no restart).
        let new_device_sk = SigningKey::generate(&mut rand::rngs::OsRng);
        let new_device_x25519 =
            crate::dm_signing::ed25519_pub_to_x25519(&new_device_sk.verifying_key().to_bytes())
                .unwrap();
        let new_device_vk_hex = hex::encode(new_device_sk.verifying_key().to_bytes());
        let new_bundle = PubKeyBundle {
            classical: ClassicalKeys {
                ed25519_verify: new_device_sk.verifying_key().to_bytes(),
                x25519_pub: new_device_x25519,
            },
            post_quantum: None,
        };
        let artifact = RecoveryArtifact::from_seed(seed);
        let enrolled = enroll_via_master(
            &state,
            &artifact,
            &new_device_sk,
            new_bundle,
            1_700_000_500,
            DEFAULT_ACTIVE_WINDOW_SECS,
        )
        .expect("master-sign new device enrollment");
        state
            .add_enrollment(
                enrolled.enrollment_cert,
                1_700_000_500,
                DEFAULT_ACTIVE_WINDOW_SECS,
            )
            .expect("add second enrollment");
        save_owner_state_cbor_only(dir.path(), &state).unwrap();

        // The LIVE read now sees BOTH devices, including the post-boot one — no
        // restart required.
        let after_pair = read_enrolled_device_vk_hex(dir.path()).unwrap();
        assert_eq!(
            after_pair.len(),
            2,
            "live read sees the post-boot device too"
        );
        assert!(
            after_pair.contains(&new_device_vk_hex),
            "freshly-paired device's vk hex must be in the live enrolled set"
        );
    }

    #[test]
    fn cbor_only_returns_err_on_unwritable_path() {
        // Guards the fail-open contract in get_owner_state: the writer surfaces a
        // recoverable Err (not a panic) when persistence fails, so the caller can
        // log-and-continue instead of blocking the Devices panel.
        let MintResult { state, .. } = mint_owner(1_700_000_888).unwrap();
        let dir = tempdir().unwrap();
        // Use a regular FILE as the identity dir: joining owner_state.cbor onto a
        // file path can never be created/written, so the writer must return Err
        // (not panic) on all platforms — independent of write_atomic_0600's
        // parent-dir-creation behavior.
        let file_as_dir = dir.path().join("not-a-directory");
        std::fs::write(&file_as_dir, b"x").unwrap();
        let result = save_owner_state_cbor_only(&file_as_dir, &state);
        assert!(
            result.is_err(),
            "cbor-only write under a non-directory path must return Err, not panic"
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

        assert_eq!(
            refresh_self_liveness(&mut state, &device_signing_key, now),
            LivenessRefreshOutcome::Refreshed,
            "missing liveness must trigger a publish"
        );
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
        assert_eq!(
            refresh_self_liveness(&mut state, &device_signing_key, now),
            LivenessRefreshOutcome::Fresh,
            "fresh liveness must NOT be re-published"
        );
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
        assert_eq!(
            refresh_self_liveness(&mut state, &device_signing_key, later),
            LivenessRefreshOutcome::Refreshed,
            "stale liveness must be re-signed"
        );
        assert!(state.liveness.get(&device_id).unwrap().timestamp > old_ts);
    }

    #[test]
    fn refresh_self_liveness_reports_clock_regressed_and_does_not_resign() {
        let mint_t = 1_700_000_000;
        let MintResult {
            mut state,
            device_signing_key,
            ..
        } = mint_owner(mint_t).unwrap();
        let device_id = *state.enrollments.keys().next().unwrap();
        // mint_owner already signs a self-liveness cert at `mint_t` — that is the
        // monotonic floor the regression must not move.
        assert_eq!(state.liveness.get(&device_id).unwrap().timestamp, mint_t);
        // Host clock regresses 100 days behind the already-signed cert.
        let regressed = mint_t - 100 * 24 * 60 * 60;
        let out = refresh_self_liveness(&mut state, &device_signing_key, regressed);
        assert_eq!(
            out,
            LivenessRefreshOutcome::ClockRegressed {
                skew_secs: mint_t - regressed
            },
            "a future-stamped cert must report the regression"
        );
        assert!(!out.wrote(), "a regressed clock must not write");
        assert_eq!(
            state.liveness.get(&device_id).unwrap().timestamp,
            mint_t,
            "the cert timestamp must not move backwards"
        );
    }

    #[test]
    fn self_liveness_future_skew_secs_some_when_future_none_when_healthy() {
        let t = 1_700_000_000;
        let MintResult {
            mut state,
            device_signing_key,
            ..
        } = mint_owner(t).unwrap();
        let _ = refresh_self_liveness(&mut state, &device_signing_key, t);
        assert_eq!(
            self_liveness_future_skew_secs(&state, &device_signing_key, t),
            None,
            "cert stamped exactly at `now` is healthy"
        );
        assert_eq!(
            self_liveness_future_skew_secs(&state, &device_signing_key, t - 10),
            Some(10),
            "cert 10s ahead of `now` = 10s of regression skew"
        );
        assert_eq!(
            self_liveness_future_skew_secs(&state, &device_signing_key, t + 10),
            None,
            "cert behind `now` is healthy (no skew)"
        );
    }

    #[test]
    #[serial]
    fn legacy_identity_self_heals_to_full_on_load_and_persists() {
        // Mirrors get_owner_state's load -> refresh -> persist sequence for a
        // legacy on-disk identity (enrolled, NO liveness, has master seed).
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "self-heal-test-pp");
        let dir = tempdir().unwrap();
        let MintResult {
            mut state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(1_700_000_444).unwrap();
        state.liveness.clear();
        save_owner_state_atomic(
            dir.path(),
            &state,
            &device_signing_key,
            Some(recovery_artifact.as_bytes()),
            None,
        )
        .unwrap();

        let now = 1_700_000_500;
        let mut loaded = load_owner_state(dir.path(), None).unwrap().expect("Some");
        let device_id = *loaded.state.enrollments.keys().next().unwrap();
        assert_eq!(
            harmony_owner::trust::evaluate_trust(
                &loaded.state,
                device_id,
                now,
                harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS,
                harmony_owner::trust::DEFAULT_FRESHNESS_WINDOW_SECS,
            ),
            harmony_owner::trust::TrustDecision::Refused(
                harmony_owner::trust::RefusalReason::StaleTrustState
            ),
            "precondition: legacy identity is Refused before refresh"
        );

        if refresh_self_liveness(&mut loaded.state, &loaded.device_signing_key, now).wrote() {
            save_owner_state_cbor_only(dir.path(), &loaded.state).unwrap();
        }

        // Reload from disk: now Full + persisted + master seed intact.
        let reloaded = load_owner_state(dir.path(), None).unwrap().expect("Some");
        assert_eq!(
            reloaded.state.liveness.len(),
            1,
            "liveness must be persisted"
        );
        assert_eq!(
            harmony_owner::trust::evaluate_trust(
                &reloaded.state,
                device_id,
                now,
                harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS,
                harmony_owner::trust::DEFAULT_FRESHNESS_WINDOW_SECS,
            ),
            harmony_owner::trust::TrustDecision::Full,
        );
        assert!(
            reloaded.master_seed.is_some(),
            "master seed must survive the refresh-persist"
        );
    }

    #[test]
    fn bumped_mint_owner_stamps_initial_liveness() {
        // ZEB-342: post-bump, mint_owner publishes device #1 liveness WITHOUT
        // any client-side refresh. Tripwire against a harmony dep downgrade.
        let result = mint_owner(1_700_000_777).unwrap();
        assert_eq!(
            result.state.liveness.len(),
            1,
            "bumped mint_owner must stamp device #1 liveness"
        );
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
                butler_pinned: false,
                device_vk_hex: "cc".repeat(32),
                // ZEB-668 S2: non-default values so a serde-rename regression
                // on the revocation trio cannot pass unnoticed.
                revoked: true,
                revoked_at: Some(1_700_000_100),
                revoked_reason: Some("lost".into()),
                // ZEB-668 S4: same non-default trick for the fleet-join trio.
                pet_name: Some("Koya".into()),
                last_seen_ms: Some(1_700_000_200_000),
                connected_now: true,
                // ZEB-677 S3: non-default so the rename is pinned.
                quorum_removable: true,
            }],
            can_back_up: true,
            // ZEB-668 S5: non-default values so the epoch pair's renames
            // are pinned too.
            fleet_epoch: 3,
            fleet_epoch_stale: true,
            // ZEB-677 S3: non-default values pin the quorum trio's renames.
            self_is_master: true,
            can_arm_enrollment: true,
            quorum_requests: vec![QuorumRequestView {
                request_id: "ab".repeat(16),
                kind: "revocation".into(),
                target_device_id: "11".repeat(16),
                initiator_device_id: "22".repeat(16),
                reason: "lost".into(),
                expires_at_ms: 1_700_000_300_000,
                initiated_by_me: false,
                signed_by_me: false,
                declined_by_me: false,
                declined: false,
                cosigner_signed: false,
                can_cosign: true,
            }],
            quorum_armed_until_ms: Some(1_700_000_400_000),
            self_clock_regressed_skew_secs: Some(7200),
        };
        let json = serde_json::to_string(&view).unwrap();
        // The wire format MUST be camelCase — JS depends on this.
        assert!(json.contains("\"ownerId\""), "expected ownerId, got {json}");
        assert!(
            json.contains("\"selfClockRegressedSkewSecs\":7200"),
            "expected selfClockRegressedSkewSecs:7200, got {json}"
        );
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
            json.contains("\"deviceVkHex\""),
            "expected deviceVkHex, got {json}"
        );
        // ZEB-668 S5 epoch pair, non-default values above.
        assert!(
            json.contains("\"fleetEpoch\":3"),
            "expected fleetEpoch:3, got {json}"
        );
        assert!(
            json.contains("\"fleetEpochStale\":true"),
            "expected fleetEpochStale:true, got {json}"
        );
        // ZEB-668 S2 revocation trio, pinned with the non-default values above.
        assert!(
            json.contains("\"revoked\":true"),
            "expected revoked:true, got {json}"
        );
        assert!(
            json.contains("\"revokedAt\":1700000100"),
            "expected revokedAt, got {json}"
        );
        assert!(
            json.contains("\"revokedReason\":\"lost\""),
            "expected revokedReason, got {json}"
        );
        // ZEB-668 S4 fleet-join trio, pinned with the non-default values above.
        assert!(
            json.contains("\"petName\":\"Koya\""),
            "expected petName, got {json}"
        );
        assert!(
            json.contains("\"lastSeenMs\":1700000200000"),
            "expected lastSeenMs, got {json}"
        );
        assert!(
            json.contains("\"connectedNow\":true"),
            "expected connectedNow, got {json}"
        );
        // ZEB-677 S3 quorum surfaces, pinned with the non-default values above.
        assert!(
            json.contains("\"selfIsMaster\":true"),
            "expected selfIsMaster:true, got {json}"
        );
        assert!(
            json.contains("\"canArmEnrollment\":true"),
            "expected canArmEnrollment:true, got {json}"
        );
        assert!(
            json.contains("\"quorumRemovable\":true"),
            "expected quorumRemovable:true, got {json}"
        );
        assert!(
            json.contains("\"quorumArmedUntilMs\":1700000400000"),
            "expected quorumArmedUntilMs, got {json}"
        );
        assert!(
            json.contains("\"quorumRequests\""),
            "expected quorumRequests, got {json}"
        );
        assert!(
            json.contains("\"requestId\"")
                && json.contains("\"targetDeviceId\"")
                && json.contains("\"initiatorDeviceId\"")
                && json.contains("\"expiresAtMs\"")
                && json.contains("\"initiatedByMe\"")
                && json.contains("\"signedByMe\"")
                && json.contains("\"declinedByMe\"")
                && json.contains("\"cosignerSigned\"")
                && json.contains("\"canCosign\":true"),
            "expected camelCase QuorumRequestView keys, got {json}"
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
