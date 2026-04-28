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
pub(crate) fn insert_token(seed: Zeroizing<[u8; 32]>) -> Uuid {
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
    cache.insert(token, TokenEntry { seed, inserted_at: Instant::now() });
    token
}

/// Consume a token: returns the master seed exactly once. Subsequent
/// `take_token(same_uuid)` returns `None`.
pub(crate) fn take_token(token: &Uuid) -> Option<Zeroizing<[u8; 32]>> {
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
        assert!(take_token(&token).is_none(), "second take must return None (single-use)");
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
        assert!(second_taken.is_none(), "second-oldest must have been evicted");
        assert!(last_taken.is_some(), "newest must remain");
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
                trust_decision: TrustDecisionView { kind: TrustKind::Full, reason: None },
                enrolled_at: 1_700_000_000,
                fingerprint: "3e2f·7a91".into(),
            }],
            can_back_up: true,
        };
        let json = serde_json::to_string(&view).unwrap();
        // The wire format MUST be camelCase — JS depends on this.
        assert!(json.contains("\"ownerId\""), "expected ownerId, got {json}");
        assert!(json.contains("\"canBackUp\""), "expected canBackUp, got {json}");
        assert!(json.contains("\"isThisDevice\""), "expected isThisDevice, got {json}");
        assert!(json.contains("\"trustDecision\""), "expected trustDecision, got {json}");
        assert!(!json.contains("owner_id"), "snake_case must not leak: {json}");
        // TrustKind must serialize as lowercase — camelCase does NOT lowercase single-word variants.
        assert!(json.contains("\"full\""), "expected lowercase \"full\" on wire, got {json}");
        assert!(!json.contains("\"Full\""), "PascalCase \"Full\" must not appear on wire: {json}");
    }
}
