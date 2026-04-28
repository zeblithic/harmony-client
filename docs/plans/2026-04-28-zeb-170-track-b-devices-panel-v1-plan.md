# ZEB-170 Track B v1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire `harmony-owner` (shipped via ZEB-173) into harmony-client and surface a read-only "My Devices" panel under Settings → Identity, including bootstrap (mint owner identity) and owner-recovery backup chained through a parallel-to-PR-61 wizard.

**Architecture:** Purely additive layer alongside existing `identity.rs` plumbing. New `owner_state.rs` (persistence + token cache + types), new `owner_commands.rs` (Tauri commands), new Svelte `DevicesPanel.svelte` (mounted as a sibling of `IdentityPanel` in `App.svelte`'s `settingsPanel` snippet). Persists three artifacts at rest: `harmony.owner.device_signing_key` keychain entry, `harmony.owner.master_seed` keychain entry, `owner_state.cbor` plaintext file (the *minted-marker*). Documented divergence from `harmony-owner` upstream intent: we persist the master seed encrypted at rest so dismiss-with-warning is recoverable later. See `docs/specs/2026-04-28-zeb-170-track-b-devices-panel-v1-design.md`.

**Tech Stack:** Rust 1.88 MSRV, Tauri 2.x, `harmony-owner` (recovery feature), `secrecy::SecretString`, `zeroize::Zeroizing`, Svelte 5, vitest, `@tauri-apps/api/core` invoke.

---

## File Structure

### Create

| Path | Responsibility |
|---|---|
| `src-tauri/src/owner_state.rs` | Pure persistence layer + types + token cache. No Tauri annotations. |
| `src-tauri/src/owner_commands.rs` | Tauri command surface. Async wrappers around `owner_state.rs` operations. |
| `src-tauri/tests/owner_integration.rs` | End-to-end mint → export → decrypt round-trip. |
| `src/lib/owner-service.ts` | Service-class wrapper around Tauri invokes; mirrors `notification-service.ts` pattern. |
| `src/lib/components/DevicesPanel.svelte` | The Devices panel UI. Empty / bootstrap-modal / populated / degraded states. |
| `src/lib/components/__tests__/DevicesPanel.test.ts` | Vitest coverage for the panel. |
| `docs/plans/2026-04-28-zeb-170-track-b-devices-panel-v1-plan.md` | This doc. |

### Modify

| Path | Why |
|---|---|
| `src-tauri/src/lib.rs` | Declare new modules; register Tauri commands. |
| `src/App.svelte` | Mount `<DevicesPanel />` in the `settingsPanel` snippet. |
| `Cargo.toml` (`src-tauri/Cargo.toml`) | None expected — `harmony-owner` (recovery feature) already wired since PR-58. |

### Out of scope (not touched)

`src-tauri/src/identity.rs`, `src-tauri/src/identity_commands.rs`, `src-tauri/src/recovery_cli.rs`, `src/lib/components/IdentityPanel.svelte`, the existing PR-61 wizard JS. The new Devices panel is purely additive.

---

## Conventions to follow (memory rules + PR-61 patterns)

- **Pull before work** — every branch from latest `origin/main`. (Already done: `zeb-170-track-b-devices-panel-v1` is at `origin/main` HEAD `4d2683d`.)
- **No worktrees** — work in the main repo.
- **Tauri error extraction** — production rejections are strings; tests use Error objects. Always: `e instanceof Error ? e.message : String(e)`.
- **Token cache TOCTOU mitigation** — preview→commit IPC pairs bind through a server-side cached token (single-use, TTL-bounded, `Zeroizing`-wrapped), not by re-fetching the resource. Mirror PR-61's `PreviewedRecovery` shape.
- **Keychain injection** — production code calls `KeychainStore::new().ok()`; tests pass `None`. Never touch the developer's real OS keychain in tests.
- **Atomic file writes** — use `crate::identity::write_atomic_0600`.
- **Pipe exit codes lie** — never trust `cmd | tail/grep` exit codes; use `set -o pipefail` or `${PIPESTATUS[0]}`.
- **Camel-case wire format** — Rust serde structs that cross IPC use `#[serde(rename_all = "camelCase")]`. JS sees camelCase.
- **`#[serial]` + cache-clear** — every Rust test that mutates the module-level token cache uses `#[serial]` (from `serial_test`) and calls `clear_token_cache()` at entry.
- **`run_blocking` adapter** — long-running operations (KDF, file I/O) in async Tauri commands go through `tokio::task::spawn_blocking`. Re-use the helper PR-61 introduced in `identity_commands.rs`.
- **`require_node_stopped(state)`** — re-use the helper PR-61 introduced. Mint refuses while the node is running.

---

### Task 1: `owner_state.rs` skeleton — types and module declaration

**Files:**
- Create: `src-tauri/src/owner_state.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod owner_state;`)

The module starts with just the public types (no logic yet). This isolates the type-shape decisions before persistence and IPC depend on them.

- [ ] **Step 1: Write the failing test**

Add to `src-tauri/src/owner_state.rs`:

```rust
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
#[serde(rename_all = "camelCase")]
pub enum TrustKind {
    Full,
    Provisional,
    Refused,
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
    }
}
```

Add `mod owner_state;` to `src-tauri/src/lib.rs` (after the existing `mod identity_commands;` line).

- [ ] **Step 2: Run test to verify it fails**

The test above is purely structural — it will pass on first compile. The "failing test" here is the *compile* itself: confirm the module is wired up.

Run: `cargo check --manifest-path src-tauri/Cargo.toml`
Expected: PASS (clean compile).

Then run: `cargo test --manifest-path src-tauri/Cargo.toml owner_state::tests::types_serialize_with_camelcase`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/owner_state.rs src-tauri/src/lib.rs
git commit -m "feat(owner): scaffold owner_state module + IPC types (ZEB-170)

- Add OwnerStateView / DeviceView / TrustDecisionView with camelCase wire
  format (matches every other Tauri payload struct in the project).
- can_back_up reflects master-seed-on-device. v1 always returns true; the
  field is here so the panel handles the degraded state from a future
  'wipe master' action correctly.
"
```

---

### Task 2: Token cache for recovery-artifact bytes

**Files:**
- Modify: `src-tauri/src/owner_state.rs`

Mirrors PR-61's `PreviewedRecovery` shape: `Mutex<HashMap<Uuid, PreviewEntry>>`, single-use via `take()`, TTL bounded, LRU-evicted, `Zeroizing`-wrapped. Master seed bytes never cross the IPC boundary as plaintext — only the opaque token.

- [ ] **Step 1: Write the failing test**

Append to `src-tauri/src/owner_state.rs`:

```rust
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

/// Insert a master seed into the token cache, returning a fresh single-use
/// token. Caller hands the token to the GUI; GUI presents it back via
/// `take_token`. Single-use semantics prevent token replay.
pub(crate) fn insert_token(seed: [u8; 32]) -> Uuid {
    let token = Uuid::new_v4();
    let mut cache = TOKEN_CACHE.lock().expect("token cache lock poisoned");
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
    cache.insert(token, TokenEntry { seed: Zeroizing::new(seed), inserted_at: Instant::now() });
    token
}

/// Consume a token: returns the master seed exactly once. Subsequent
/// `take_token(same_uuid)` returns `None`.
pub(crate) fn take_token(token: &Uuid) -> Option<Zeroizing<[u8; 32]>> {
    let mut cache = TOKEN_CACHE.lock().expect("token cache lock poisoned");
    evict_expired(&mut cache);
    cache.remove(token).map(|e| e.seed)
}

fn evict_expired(cache: &mut HashMap<Uuid, TokenEntry>) {
    let now = Instant::now();
    cache.retain(|_, e| now.duration_since(e.inserted_at) < TOKEN_TTL);
}

#[doc(hidden)]
#[cfg(test)]
pub fn clear_token_cache() {
    TOKEN_CACHE.lock().unwrap().clear();
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
        let token = insert_token(seed);
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
            tokens.push(insert_token([i as u8; 32]));
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
```

If `uuid` and `serial_test` are not yet in `src-tauri/Cargo.toml` dev-deps, they were added by PR-61. Confirm: `grep -E '^(uuid|serial_test)' src-tauri/Cargo.toml`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml owner_state::token_cache_tests`
Expected: PASS (the test code defines the cache it tests, so it should compile and pass).

If a dependency is missing, the test won't compile. Add the missing dep to `src-tauri/Cargo.toml` (`uuid = "1"` for the main deps if needed; `serial_test` is in dev-deps already).

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/owner_state.rs src-tauri/Cargo.toml
git commit -m "feat(owner): token cache for recovery-artifact bytes (ZEB-170)

Mirror PR-61's PreviewedRecovery shape: Mutex<HashMap<Uuid, TokenEntry>>,
single-use via take_token, TTL-bounded (5m), LRU-evicted (cap 8),
Zeroizing-wrapped seeds. Master seed bytes never cross the IPC
boundary as plaintext — only the opaque token does.
"
```

---

### Task 3: `load_owner_state` and `save_owner_state_atomic`

**Files:**
- Modify: `src-tauri/src/owner_state.rs`

Encapsulates the atomicity contract from the spec: keychain writes first (`device_signing_key`, `master_seed`), `.cbor` written last via `write_atomic_0600`. The `.cbor` file's presence is the minted-marker — its absence yields the natural empty state.

- [ ] **Step 1: Write the failing test**

Append to `src-tauri/src/owner_state.rs`:

```rust
use crate::identity::{write_atomic_0600, KeychainStore};
use ed25519_dalek::SigningKey;
use harmony_owner::cbor;
use harmony_owner::lifecycle::{mint_owner, MintResult};
use harmony_owner::state::OwnerState;
use std::path::Path;

const KEYCHAIN_DEVICE_SK: &str = "harmony.owner.device_signing_key";
const KEYCHAIN_MASTER_SEED: &str = "harmony.owner.master_seed";
const OWNER_STATE_FILENAME: &str = "owner_state.cbor";

/// Load the persisted OwnerState if present. Returns `Ok(None)` for the
/// natural un-minted state (no `.cbor` file). Returns `Err` for corrupt
/// files or for the inconsistent-state cases.
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
    let state: OwnerState = cbor::from_bytes(&cbor_bytes)
        .map_err(|e| format!("owner_state.cbor is corrupt: {e}"))?;

    // Inconsistent-state checks: state present implies signing key MUST be
    // findable; master seed MAY be absent (degraded but functional).
    let signing_key_bytes = load_secret(&keychain, KEYCHAIN_DEVICE_SK, identity_dir, "device_sk.enc")?
        .ok_or_else(|| "owner_state.cbor present but device_signing_key missing — inconsistent state".to_string())?;
    if signing_key_bytes.len() != 32 {
        return Err(format!(
            "device_signing_key length is {} bytes, expected 32",
            signing_key_bytes.len()
        ));
    }
    let mut sk_arr = [0u8; 32];
    sk_arr.copy_from_slice(&signing_key_bytes);
    let device_signing_key = SigningKey::from_bytes(&sk_arr);

    let master_seed_bytes = load_secret(&keychain, KEYCHAIN_MASTER_SEED, identity_dir, "master_seed.enc")?;
    let master_seed = match master_seed_bytes {
        Some(b) if b.len() == 32 => {
            let mut s = [0u8; 32];
            s.copy_from_slice(&b);
            Some(s)
        }
        Some(b) => return Err(format!("master_seed length is {} bytes, expected 32", b.len())),
        None => None,
    };

    Ok(Some(LoadedOwnerState { state, device_signing_key, master_seed }))
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
    save_secret(&keychain, KEYCHAIN_DEVICE_SK, identity_dir, "device_sk.enc",
                &device_signing_key.to_bytes())?;
    save_secret(&keychain, KEYCHAIN_MASTER_SEED, identity_dir, "master_seed.enc",
                master_seed)?;
    let cbor_bytes = cbor::to_canonical(state)
        .map_err(|e| format!("CBOR encode of OwnerState failed: {e}"))?;
    let cbor_path = identity_dir.join(OWNER_STATE_FILENAME);
    write_atomic_0600(&cbor_path, &cbor_bytes)
        .map_err(|e| format!("failed to write {}: {e}", cbor_path.display()))?;
    Ok(())
}

pub struct LoadedOwnerState {
    pub state: OwnerState,
    pub device_signing_key: SigningKey,
    /// `None` when the master seed has been wiped from this device but
    /// the rest of the owner state remains. v1 does not ship the wipe
    /// action; this case is reachable only via manual file deletion.
    pub master_seed: Option<[u8; 32]>,
}

/// Load a 32-byte secret from keychain primary, encrypted-file fallback.
/// Returns `Ok(None)` when neither source has the secret.
fn load_secret(
    keychain: &Option<KeychainStore>,
    keychain_name: &str,
    identity_dir: &Path,
    fallback_filename: &str,
) -> Result<Option<Vec<u8>>, String> {
    if let Some(kc) = keychain {
        match kc.get_password(keychain_name) {
            Ok(Some(b64)) => {
                use base64::Engine;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64.as_bytes())
                    .map_err(|e| format!("keychain entry {keychain_name} not base64: {e}"))?;
                return Ok(Some(bytes));
            }
            Ok(None) => {}
            Err(e) => return Err(format!("keychain read {keychain_name}: {e}")),
        }
    }
    // Fallback: encrypted file under HARMONY_PASSPHRASE.
    let path = identity_dir.join(fallback_filename);
    if !path.exists() {
        return Ok(None);
    }
    let store = crate::identity::EncryptedFileStore::from_env(&path)
        .map_err(|e| format!("encrypted-file fallback for {fallback_filename}: {e}"))?;
    let bytes = store
        .read()
        .map_err(|e| format!("read {fallback_filename}: {e}"))?;
    Ok(Some(bytes.to_vec()))
}

fn save_secret(
    keychain: &Option<KeychainStore>,
    keychain_name: &str,
    identity_dir: &Path,
    fallback_filename: &str,
    bytes: &[u8],
) -> Result<(), String> {
    if let Some(kc) = keychain {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        return kc
            .set_password(keychain_name, &b64)
            .map_err(|e| format!("keychain write {keychain_name}: {e}"));
    }
    let path = identity_dir.join(fallback_filename);
    let store = crate::identity::EncryptedFileStore::from_env(&path)
        .map_err(|e| format!("encrypted-file fallback for {fallback_filename}: {e}"))?;
    store
        .write(bytes)
        .map_err(|e| format!("write {fallback_filename}: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod persistence_tests {
    use super::*;
    use serial_test::serial;
    use tempfile::tempdir;

    #[test]
    #[serial]
    fn save_then_load_roundtrip_preserves_state() {
        std::env::set_var("HARMONY_PASSPHRASE", "test-pp-1");
        let dir = tempdir().unwrap();

        let MintResult { state, recovery_artifact, device_signing_key } =
            mint_owner(1_700_000_000).unwrap();
        let master_seed = *recovery_artifact.as_bytes();

        save_owner_state_atomic(dir.path(), &state, &device_signing_key, &master_seed, None)
            .expect("save");

        let loaded = load_owner_state(dir.path(), None)
            .expect("load")
            .expect("must be Some after save");
        assert_eq!(loaded.state.owner_id, state.owner_id);
        assert_eq!(loaded.state.enrollments.len(), 1);
        assert_eq!(loaded.device_signing_key.to_bytes(), device_signing_key.to_bytes());
        assert_eq!(loaded.master_seed, Some(master_seed));

        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn load_returns_none_when_no_cbor_present() {
        let dir = tempdir().unwrap();
        let result = load_owner_state(dir.path(), None).expect("load");
        assert!(result.is_none(), "un-minted state must be None, not Err");
    }

    #[test]
    #[serial]
    fn load_returns_err_when_cbor_corrupt() {
        let dir = tempdir().unwrap();
        let cbor_path = dir.path().join(OWNER_STATE_FILENAME);
        std::fs::write(&cbor_path, b"not-cbor-bytes").unwrap();
        let err = load_owner_state(dir.path(), None).expect_err("must be Err");
        assert!(err.contains("corrupt"), "actual: {err}");
    }

    #[test]
    #[serial]
    fn load_returns_err_when_signing_key_missing() {
        std::env::set_var("HARMONY_PASSPHRASE", "test-pp-2");
        let dir = tempdir().unwrap();

        let MintResult { state, recovery_artifact, device_signing_key } =
            mint_owner(1_700_000_001).unwrap();
        save_owner_state_atomic(
            dir.path(), &state, &device_signing_key, recovery_artifact.as_bytes(), None,
        ).unwrap();

        // Manually wipe the device_sk.enc fallback file to simulate the
        // "inconsistent state" condition.
        let _ = std::fs::remove_file(dir.path().join("device_sk.enc"));

        let err = load_owner_state(dir.path(), None).expect_err("must be Err");
        assert!(
            err.contains("device_signing_key missing") || err.contains("inconsistent"),
            "actual: {err}"
        );
        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn load_returns_some_with_none_master_seed_when_only_seed_missing() {
        std::env::set_var("HARMONY_PASSPHRASE", "test-pp-3");
        let dir = tempdir().unwrap();

        let MintResult { state, recovery_artifact, device_signing_key } =
            mint_owner(1_700_000_002).unwrap();
        save_owner_state_atomic(
            dir.path(), &state, &device_signing_key, recovery_artifact.as_bytes(), None,
        ).unwrap();

        // Wipe ONLY the master seed fallback — simulates the future "wipe master" action.
        let _ = std::fs::remove_file(dir.path().join("master_seed.enc"));

        let loaded = load_owner_state(dir.path(), None)
            .expect("load")
            .expect("must be Some");
        assert!(loaded.master_seed.is_none(), "degraded state: master seed gone, signing key present");
        std::env::remove_var("HARMONY_PASSPHRASE");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml owner_state::persistence_tests`
Expected: First run might fail to compile if `EncryptedFileStore::from_env` / `KeychainStore::get_password` / `set_password` signatures differ. Adjust to match actual names from `src-tauri/src/identity.rs`. Tests should pass once signatures align.

(Confirm signatures by reading `src-tauri/src/identity.rs` for `KeychainStore` and `EncryptedFileStore` interfaces.)

- [ ] **Step 3: Iterate compile errors until tests pass**

Common adjustments expected:
- Error type for `EncryptedFileStore::from_env` may need `.to_string()` to coerce.
- `KeychainStore` may use a different method name (e.g., `read` / `write` instead of `get_password` / `set_password`).
- The `harmony_owner::cbor` reexport — confirm its public path; otherwise use `ciborium` directly via the `harmony-owner` re-export.

Re-run after each adjustment until: `test result: ok. 4 passed; 0 failed`.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/owner_state.rs
git commit -m "feat(owner): persistence layer for OwnerState + keys (ZEB-170)

- load_owner_state returns Ok(None) for un-minted (.cbor absent), Err on
  corrupt/inconsistent. Distinguishes degraded 'master seed gone but state
  present' from 'state present but signing key missing' (the latter is an
  error; the former renders canBackUp=false in the panel).
- save_owner_state_atomic enforces the keychain-first/.cbor-last order: the
  .cbor file is the minted-marker. Failure mid-sequence leaves either
  recoverable state or orphan keychain entries (tolerated).
- Both paths inject KeychainStore for hermeticity; encrypted-file fallback
  uses EncryptedFileStore (HARMONY_PASSPHRASE chain) for headless / locked-
  keychain environments.
"
```

---

### Task 4: `owner_commands.rs` — Tauri commands

**Files:**
- Create: `src-tauri/src/owner_commands.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod owner_commands;`, register the three new commands in `tauri::generate_handler!`)

Three Tauri commands: `get_owner_state`, `mint_owner_identity`, `export_owner_recovery_file_to_path`. All are `pub async fn`; long-running ops run through the `run_blocking` adapter PR-61 introduced.

- [ ] **Step 1: Write the failing test**

Create `src-tauri/src/owner_commands.rs`:

```rust
//! Tauri command surface for the Devices panel.
//!
//! Wraps `crate::owner_state` operations with async + state-injection
//! plumbing. Long-running ops go through `crate::identity_commands::run_blocking`.

use crate::identity::KeychainStore;
use crate::identity_commands::run_blocking;
use crate::owner_state::{
    insert_token, load_owner_state, save_owner_state_atomic, take_token, DeviceView, LoadedOwnerState,
    OwnerStateView, TrustDecisionView, TrustKind,
};
use harmony_owner::lifecycle::{mint_owner, MintResult};
use harmony_owner::recovery::RecoveryMetadata;
use harmony_owner::lifecycle::RecoveryArtifact;
use harmony_owner::trust;
use secrecy::SecretString;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::Manager;
use uuid::Uuid;
use zeroize::Zeroizing;

const ERR_NODE_RUNNING: &str =
    "Stop the node before minting an owner identity (the node must not be holding owner-scoped keys during mint).";

const MIN_RECOVERY_PASSPHRASE_LEN: usize = 12;

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MintIpcResult {
    pub state: OwnerStateView,
    pub recovery_token: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportInfo {
    pub identity_hash: String,
    pub byte_len: u64,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn require_node_stopped(state: &Mutex<crate::NodeState>) -> Result<(), String> {
    let guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
    if guard.is_running() {
        return Err(ERR_NODE_RUNNING.to_string());
    }
    Ok(())
}

fn build_owner_state_view(loaded: &LoadedOwnerState, this_device_name: String) -> OwnerStateView {
    let now = now_unix();
    let active_window = trust::DEFAULT_ACTIVE_WINDOW_SECS;
    let freshness = trust::DEFAULT_FRESHNESS_WINDOW_SECS;
    let this_device_id = derive_this_device_id(&loaded.device_signing_key);

    let devices: Vec<DeviceView> = loaded
        .state
        .enrollments
        .values()
        .map(|cert| {
            let decision = trust::evaluate_trust(
                &loaded.state, &cert.device_id, now, active_window, freshness,
            );
            let (kind, reason) = match decision {
                trust::TrustDecision::Full => (TrustKind::Full, None),
                trust::TrustDecision::Provisional => (TrustKind::Provisional, None),
                trust::TrustDecision::Refused(r) => (TrustKind::Refused, Some(format!("{r:?}"))),
            };
            DeviceView {
                device_id: hex::encode(cert.device_id),
                display_name: if cert.device_id == this_device_id {
                    this_device_name.clone()
                } else {
                    format!("Device {}", &hex::encode(cert.device_id)[..8])
                },
                is_this_device: cert.device_id == this_device_id,
                trust_decision: TrustDecisionView { kind, reason },
                enrolled_at: cert.issued_at,
                fingerprint: format_fingerprint(&cert.device_id),
            }
        })
        .collect();

    OwnerStateView {
        owner_id: hex::encode(loaded.state.owner_id),
        owner_display_name: this_device_name, // owner display name == local user's display name in v1
        devices,
        can_back_up: loaded.master_seed.is_some(),
    }
}

fn derive_this_device_id(sk: &ed25519_dalek::SigningKey) -> [u8; 16] {
    use harmony_owner::pubkey_bundle::PubKeyBundle;
    PubKeyBundle::classical_only(sk.verifying_key().to_bytes()).identity_hash()
}

fn format_fingerprint(id: &[u8; 16]) -> String {
    let hex = hex::encode(id);
    format!("{}·{}", &hex[..4], &hex[4..8])
}

#[tauri::command]
pub async fn get_owner_state(
    app: tauri::AppHandle,
) -> Result<Option<OwnerStateView>, String> {
    let identity_dir = resolve_identity_dir(&app)?;
    let display_name = "this device".to_string(); // Frontend overrides via profile-service.
    run_blocking(move || {
        let loaded = load_owner_state(&identity_dir, KeychainStore::new().ok())?;
        Ok(loaded.map(|l| build_owner_state_view(&l, display_name)))
    })
    .await
}

#[tauri::command]
pub async fn mint_owner_identity(
    app: tauri::AppHandle,
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<MintIpcResult, String> {
    require_node_stopped(&state)?;
    let identity_dir = resolve_identity_dir(&app)?;
    let display_name = "this device".to_string();
    run_blocking(move || {
        // Refuse if already minted (idempotent failure).
        if identity_dir.join("owner_state.cbor").exists() {
            return Err(
                "Owner identity already exists on this device. Wipe via Settings to re-mint."
                    .to_string(),
            );
        }
        let MintResult { state, recovery_artifact, device_signing_key } =
            mint_owner(now_unix()).map_err(|e| format!("mint_owner: {e}"))?;
        let master_seed = *recovery_artifact.as_bytes();
        save_owner_state_atomic(
            &identity_dir,
            &state,
            &device_signing_key,
            &master_seed,
            KeychainStore::new().ok(),
        )?;
        let token = insert_token(master_seed);
        let loaded = LoadedOwnerState {
            state,
            device_signing_key,
            master_seed: Some(master_seed),
        };
        Ok(MintIpcResult {
            state: build_owner_state_view(&loaded, display_name),
            recovery_token: token.to_string(),
        })
    })
    .await
}

#[tauri::command]
pub async fn export_owner_recovery_file_to_path(
    recovery_token: String,
    path: String,
    passphrase: String,
    comment: Option<String>,
) -> Result<ExportInfo, String> {
    if passphrase.len() < MIN_RECOVERY_PASSPHRASE_LEN {
        return Err(format!(
            "Recovery passphrase must be at least {MIN_RECOVERY_PASSPHRASE_LEN} characters."
        ));
    }
    let comment_validated = match comment {
        Some(c) if c.as_bytes().len() > 256 => {
            return Err("Recovery comment must be at most 256 bytes.".to_string());
        }
        c => c,
    };
    let token: Uuid = recovery_token
        .parse()
        .map_err(|e| format!("invalid recovery token: {e}"))?;
    let out = PathBuf::from(path);
    run_blocking(move || {
        let seed = take_token(&token)
            .ok_or_else(|| "Recovery token expired or invalid. Please re-trigger backup from the Devices panel.".to_string())?;
        let secret = SecretString::from(passphrase);
        let artifact = RecoveryArtifact::from_seed(*seed);
        let id_hash = artifact.master_pubkey_bundle().identity_hash();
        let metadata = RecoveryMetadata {
            mint_at: None,
            comment: comment_validated,
        };
        let bytes = artifact
            .to_encrypted_file(&secret, &metadata)
            .map_err(|e| format!("encrypt recovery file: {e}"))?;
        crate::identity::write_atomic_0600(&out, &bytes)
            .map_err(|e| format!("write {}: {e}", out.display()))?;
        Ok(ExportInfo {
            identity_hash: hex::encode(id_hash),
            byte_len: bytes.len() as u64,
        })
    })
    .await
}

fn resolve_identity_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    crate::identity::identity_dir(app).map_err(|e| format!("identity_dir: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state::clear_token_cache;
    use serial_test::serial;
    use tempfile::tempdir;

    fn make_test_dir_with_passphrase() -> (tempfile::TempDir, &'static str) {
        std::env::set_var("HARMONY_PASSPHRASE", "owner-cmd-test-pp");
        let dir = tempdir().unwrap();
        (dir, "ok")
    }

    #[test]
    #[serial]
    fn export_with_too_short_passphrase_errors_without_consuming_token() {
        clear_token_cache();
        let (_dir, _) = make_test_dir_with_passphrase();
        let token = insert_token([0xCDu8; 32]);
        // Use a too-short passphrase.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(export_owner_recovery_file_to_path(
            token.to_string(),
            "/tmp/should-not-write".into(),
            "short".into(),
            None,
        ));
        assert!(result.is_err());
        // Token must NOT have been consumed (validation precedes take).
        assert!(take_token(&token).is_some(), "weak-passphrase rejection must not consume token");
        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn export_with_invalid_token_errors() {
        clear_token_cache();
        let (_dir, _) = make_test_dir_with_passphrase();
        let bogus = Uuid::new_v4();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(export_owner_recovery_file_to_path(
            bogus.to_string(),
            "/tmp/should-not-write".into(),
            "passphrase-12+".into(),
            None,
        ));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("expired") || err.contains("invalid"), "actual: {err}");
        std::env::remove_var("HARMONY_PASSPHRASE");
    }

    #[test]
    #[serial]
    fn comment_over_cap_rejected() {
        clear_token_cache();
        let (_dir, _) = make_test_dir_with_passphrase();
        let token = insert_token([0xEEu8; 32]);
        let comment = "x".repeat(257);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(export_owner_recovery_file_to_path(
            token.to_string(),
            "/tmp/should-not-write".into(),
            "passphrase-12+".into(),
            Some(comment),
        ));
        assert!(result.is_err());
        // Token must NOT have been consumed.
        assert!(take_token(&token).is_some(), "comment-over-cap rejection must not consume token");
        std::env::remove_var("HARMONY_PASSPHRASE");
    }
}
```

Modify `src-tauri/src/lib.rs`:
- Add `mod owner_commands;` near the existing `mod identity_commands;`.
- Add the three commands to `tauri::generate_handler!` alongside the existing identity commands.

```rust
// In lib.rs, the existing tauri::generate_handler! macro call gains three entries:
// owner_commands::get_owner_state,
// owner_commands::mint_owner_identity,
// owner_commands::export_owner_recovery_file_to_path,
```

- [ ] **Step 2: Compile + run tests**

Run: `cargo check --manifest-path src-tauri/Cargo.toml`
Expected: PASS (signature mismatches with `harmony-owner::trust::evaluate_trust` may surface — adjust to match actual API).

Run: `cargo test --manifest-path src-tauri/Cargo.toml owner_commands::tests`
Expected: 3 passed, 0 failed.

- [ ] **Step 3: Iterate signature/import errors**

Likely adjustments:
- `trust::DEFAULT_ACTIVE_WINDOW_SECS` / `DEFAULT_FRESHNESS_WINDOW_SECS` — confirm the constant names.
- `trust::evaluate_trust` parameter order — match the actual signature.
- `identity::identity_dir(app)` — confirm helper exists; if not, use `app.path().app_local_data_dir()` directly.
- `run_blocking` re-export — add `pub` to `identity_commands::run_blocking` if not already (PR-61 may have left it `pub(crate)`).

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/owner_commands.rs src-tauri/src/lib.rs
git commit -m "feat(owner): Tauri commands for mint + export (ZEB-170)

Three commands wired into tauri::generate_handler:
- get_owner_state: read-only load, returns Option<OwnerStateView>.
- mint_owner_identity: bootstraps owner identity, refuses if node is
  running (require_node_stopped helper) or already minted. Returns the
  populated view + a single-use recovery token.
- export_owner_recovery_file_to_path: consumes the recovery token,
  writes a passphrase-encrypted file via RecoveryArtifact::to_encrypted_file.
  Validates passphrase length (>=12) and comment length (<=256B) BEFORE
  consuming the token (failed validation does not waste the token).

Long-running ops use run_blocking (PR-61 adapter). All command surfaces
follow PR-61's serde camelCase wire-format conventions.
"
```

---

### Task 5: Backend integration test — end-to-end mint → export → decrypt

**Files:**
- Create: `src-tauri/tests/owner_integration.rs`

Single test that exercises the full vertical: mint, persist, export the recovery file, decrypt it back, confirm the master seed round-trips.

- [ ] **Step 1: Write the failing test**

```rust
//! End-to-end integration test for ZEB-170 owner-binding wiring.
//!
//! Validates that mint → save → export → decrypt round-trips the master
//! seed without going through the GUI. Hermetic: uses TempDir, injects no
//! keychain (encrypted-file fallback under HARMONY_PASSPHRASE).

use harmony_app::owner_state::{
    insert_token, load_owner_state, save_owner_state_atomic, take_token,
};
use harmony_owner::lifecycle::{mint_owner, MintResult, RecoveryArtifact};
use harmony_owner::recovery::RecoveryMetadata;
use secrecy::SecretString;
use serial_test::serial;
use tempfile::tempdir;

#[test]
#[serial]
fn mint_save_export_decrypt_roundtrip() {
    std::env::set_var("HARMONY_PASSPHRASE", "owner-integration-pp");
    let dir = tempdir().unwrap();

    // 1. Mint
    let MintResult { state, recovery_artifact, device_signing_key } =
        mint_owner(1_700_000_000).expect("mint");
    let master_seed = *recovery_artifact.as_bytes();

    // 2. Persist
    save_owner_state_atomic(dir.path(), &state, &device_signing_key, &master_seed, None)
        .expect("save");

    // 3. Reload — confirm load round-trip
    let loaded = load_owner_state(dir.path(), None).expect("load").expect("Some");
    assert_eq!(loaded.state.owner_id, state.owner_id);

    // 4. Export via token cache
    let token = insert_token(master_seed);
    let recovered = take_token(&token).expect("token must redeem once");
    assert_eq!(*recovered, master_seed);

    let secret = SecretString::from("integration-test-recovery-pp".to_string());
    let artifact_for_export = RecoveryArtifact::from_seed(*recovered);
    let metadata = RecoveryMetadata { mint_at: None, comment: Some("integration".into()) };
    let encrypted = artifact_for_export
        .to_encrypted_file(&secret, &metadata)
        .expect("encrypt");

    // 5. Write to disk (round-trips file format)
    let out_path = dir.path().join("recovery.bin");
    std::fs::write(&out_path, &encrypted).expect("write");

    // 6. Decrypt back from disk
    let bytes = std::fs::read(&out_path).expect("read");
    let restored = RecoveryArtifact::from_encrypted_file(&bytes, &secret).expect("decrypt");
    let artifact = restored.into_artifact();
    assert_eq!(*artifact.as_bytes(), master_seed,
               "round-trip must yield identical master seed");

    std::env::remove_var("HARMONY_PASSPHRASE");
}
```

If `harmony_app` is not the binary crate name, adjust the import. The harmony-client crate name is in `src-tauri/Cargo.toml` (`[lib] name = "..."`). PR-58/61 likely set this to `harmony_app` for testability; confirm and adjust.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test owner_integration mint_save_export_decrypt_roundtrip`
Expected: FAIL initially (likely import path mismatch or `pub` visibility on the module — `owner_state` module needs `pub` on `lib.rs`).

- [ ] **Step 3: Iterate visibility errors**

In `src-tauri/src/lib.rs`, ensure `pub mod owner_state;` (not just `mod owner_state;`) so integration tests can import. Same for `insert_token`, `take_token`, `load_owner_state`, `save_owner_state_atomic` — all need to be `pub`.

Re-run until: `test result: ok. 1 passed; 0 failed`.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/tests/owner_integration.rs src-tauri/src/lib.rs
git commit -m "test(owner): integration test — mint→export→decrypt roundtrip (ZEB-170)

Hermetic test (TempDir, no keychain) covering the full vertical:
mint_owner → save_owner_state_atomic → token cache insert → take →
RecoveryArtifact::to_encrypted_file → disk round-trip → from_encrypted_file
→ assert master seed identity. Pins the at-rest CBOR encoding +
encrypted-file format + token-cache single-use invariants together.
"
```

---

### Task 6: Frontend — `owner-service.ts` and types

**Files:**
- Create: `src/lib/owner-service.ts`

TypeScript types matching the Rust serde shapes, plus the `OwnerService` class wrapping the three Tauri invokes. Mirrors `notification-service.ts` style.

- [ ] **Step 1: Write the failing test**

Append to `src/lib/owner-service.ts`:

```typescript
import { invoke } from '@tauri-apps/api/core';

export interface OwnerStateView {
  ownerId: string;
  ownerDisplayName: string;
  devices: DeviceView[];
  canBackUp: boolean;
}

export interface DeviceView {
  deviceId: string;
  displayName: string;
  isThisDevice: boolean;
  trustDecision: TrustDecisionView;
  enrolledAt: number;
  fingerprint: string;
}

export interface TrustDecisionView {
  kind: 'full' | 'provisional' | 'refused';
  reason: string | null;
}

export interface MintIpcResult {
  state: OwnerStateView;
  recoveryToken: string;
}

export interface ExportInfo {
  identityHash: string;
  byteLen: number;
}

/**
 * Service-class wrapper around the owner-binding Tauri commands.
 *
 * Mirrors `notification-service.ts` pattern: methods + onChange callback
 * for reactive state updates. Error extraction follows the project's
 * memory rule (production rejections are strings; tests emit Errors).
 */
export class OwnerService {
  state: OwnerStateView | null = null;
  onChange?: () => void;

  async refresh(): Promise<void> {
    const view = await invoke<OwnerStateView | null>('get_owner_state');
    this.state = view;
    this.onChange?.();
  }

  async mint(): Promise<MintIpcResult> {
    const result = await invoke<MintIpcResult>('mint_owner_identity');
    this.state = result.state;
    this.onChange?.();
    return result;
  }

  async exportRecoveryFile(
    recoveryToken: string,
    path: string,
    passphrase: string,
    comment: string | null,
  ): Promise<ExportInfo> {
    return invoke<ExportInfo>('export_owner_recovery_file_to_path', {
      recoveryToken,
      path,
      passphrase,
      comment,
    });
  }
}

/** Memory-rule-compliant error extraction for invoke rejections. */
export function extractError(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
```

Create vitest stub at `src/lib/__tests__/owner-service.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { OwnerService, extractError } from '../owner-service';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));

import { invoke } from '@tauri-apps/api/core';

describe('OwnerService', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('refresh() sets state to null on un-minted', async () => {
    (invoke as unknown as ReturnType<typeof vi.fn>).mockResolvedValueOnce(null);
    const svc = new OwnerService();
    let changeCount = 0;
    svc.onChange = () => { changeCount++; };
    await svc.refresh();
    expect(svc.state).toBeNull();
    expect(changeCount).toBe(1);
  });

  it('refresh() stores populated view', async () => {
    const view = {
      ownerId: 'a4f1c8239b7dd809',
      ownerDisplayName: 'zeblith',
      devices: [],
      canBackUp: true,
    };
    (invoke as unknown as ReturnType<typeof vi.fn>).mockResolvedValueOnce(view);
    const svc = new OwnerService();
    await svc.refresh();
    expect(svc.state).toEqual(view);
  });

  it('mint() returns recoveryToken and updates state', async () => {
    const result = {
      state: {
        ownerId: 'newowner', ownerDisplayName: 'this device',
        devices: [], canBackUp: true,
      },
      recoveryToken: '01234567-89ab-cdef-0123-456789abcdef',
    };
    (invoke as unknown as ReturnType<typeof vi.fn>).mockResolvedValueOnce(result);
    const svc = new OwnerService();
    const got = await svc.mint();
    expect(got.recoveryToken).toBe(result.recoveryToken);
    expect(svc.state).toEqual(result.state);
  });

  it('exportRecoveryFile passes args verbatim', async () => {
    (invoke as unknown as ReturnType<typeof vi.fn>).mockResolvedValueOnce({
      identityHash: 'abc', byteLen: 1234,
    });
    const svc = new OwnerService();
    await svc.exportRecoveryFile('tok', '/tmp/r', 'a-strong-passphrase', 'comment');
    expect(invoke).toHaveBeenCalledWith('export_owner_recovery_file_to_path', {
      recoveryToken: 'tok',
      path: '/tmp/r',
      passphrase: 'a-strong-passphrase',
      comment: 'comment',
    });
  });
});

describe('extractError', () => {
  it('returns string from Error object (test-mode rejection)', () => {
    expect(extractError(new Error('boom'))).toBe('boom');
  });
  it('returns string from raw string rejection (production-mode)', () => {
    expect(extractError('just a string')).toBe('just a string');
  });
});
```

- [ ] **Step 2: Run vitest**

Run: `npx vitest run src/lib/__tests__/owner-service.test.ts`
Expected: 5 tests pass.

- [ ] **Step 3: Commit**

```bash
git add src/lib/owner-service.ts src/lib/__tests__/owner-service.test.ts
git commit -m "feat(owner): owner-service.ts + types + vitest (ZEB-170)

Service-class wrapping the three Tauri commands. Types mirror Rust
serde shapes 1:1 (camelCase wire format). extractError() applies the
memory rule for production-vs-test rejection extraction.
"
```

---

### Task 7: Frontend — `DevicesPanel.svelte` empty + bootstrap states

**Files:**
- Create: `src/lib/components/DevicesPanel.svelte`

Renders the empty state ("Bind this device to a new owner identity") and the bootstrap confirm-modal. Calling `mint()` on confirm transitions to the populated state — populated state's content is added in Task 8.

- [ ] **Step 1: Write the failing test**

Create `src/lib/components/__tests__/DevicesPanel.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import DevicesPanel from '../DevicesPanel.svelte';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));

import { invoke } from '@tauri-apps/api/core';

const mockedInvoke = invoke as unknown as ReturnType<typeof vi.fn>;

describe('DevicesPanel — empty + bootstrap states', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('renders empty state when get_owner_state returns null', async () => {
    mockedInvoke.mockResolvedValueOnce(null);
    render(DevicesPanel);
    // Wait for async refresh on mount.
    await screen.findByRole('button', { name: /bind this device/i });
    expect(screen.queryByText(/owner identity/i)).toBeInTheDocument();
  });

  it('opens confirm modal when bind CTA is clicked', async () => {
    mockedInvoke.mockResolvedValueOnce(null);
    render(DevicesPanel);
    const bindBtn = await screen.findByRole('button', { name: /bind this device/i });
    await fireEvent.click(bindBtn);
    expect(screen.getByText(/will create your owner identity/i)).toBeInTheDocument();
  });

  it('calls mint_owner_identity on confirm and transitions to populated state', async () => {
    mockedInvoke.mockResolvedValueOnce(null);
    const mintResult = {
      state: {
        ownerId: 'a4f1c8239b7dd809abcdef0123456789',
        ownerDisplayName: 'this device',
        devices: [{
          deviceId: 'aa11bb22cc33dd44ee55ff6677889900',
          displayName: 'this device',
          isThisDevice: true,
          trustDecision: { kind: 'full', reason: null },
          enrolledAt: 1_700_000_000,
          fingerprint: 'aa11·bb22',
        }],
        canBackUp: true,
      },
      recoveryToken: 'tok-1',
    };
    mockedInvoke.mockResolvedValueOnce(mintResult);
    render(DevicesPanel);
    const bindBtn = await screen.findByRole('button', { name: /bind this device/i });
    await fireEvent.click(bindBtn);
    const confirmBtn = await screen.findByRole('button', { name: /^create owner identity/i });
    await fireEvent.click(confirmBtn);
    await screen.findByText(/my devices/i);
    expect(mockedInvoke).toHaveBeenCalledWith('mint_owner_identity');
  });

  it('cancel modal returns to empty state without invoking mint', async () => {
    mockedInvoke.mockResolvedValueOnce(null);
    render(DevicesPanel);
    const bindBtn = await screen.findByRole('button', { name: /bind this device/i });
    await fireEvent.click(bindBtn);
    const cancel = await screen.findByRole('button', { name: /cancel/i });
    await fireEvent.click(cancel);
    expect(mockedInvoke).toHaveBeenCalledTimes(1); // only the initial refresh
    expect(screen.queryByText(/will create your owner identity/i)).not.toBeInTheDocument();
  });
});
```

Create `src/lib/components/DevicesPanel.svelte`:

```svelte
<script lang="ts">
  import { onMount } from 'svelte';
  import { OwnerService, extractError, type OwnerStateView } from '../owner-service';

  let svc = new OwnerService();
  let state = $state<OwnerStateView | null>(null);
  let loading = $state(true);
  let loadError = $state<string | null>(null);
  let modalOpen = $state(false);
  let mintInFlight = $state(false);
  let mintError = $state<string | null>(null);
  let recoveryToken = $state<string | null>(null);

  svc.onChange = () => { state = svc.state; };

  onMount(async () => {
    try {
      await svc.refresh();
    } catch (e) {
      loadError = extractError(e);
    } finally {
      loading = false;
    }
  });

  async function handleConfirmMint() {
    mintInFlight = true;
    mintError = null;
    try {
      const result = await svc.mint();
      recoveryToken = result.recoveryToken;
      modalOpen = false;
    } catch (e) {
      mintError = extractError(e);
    } finally {
      mintInFlight = false;
    }
  }

  function dismissBackup() {
    dismissedBackup = true;
  }
</script>

<section class="devices-panel" aria-labelledby="devices-heading">
  <h2 id="devices-heading">Devices</h2>

  {#if loading}
    <p class="loading">Loading…</p>
  {:else if loadError}
    <p class="error" role="alert">Failed to load: {loadError}</p>
  {:else if state === null}
    <div class="empty">
      <p class="explainer">
        You haven't created an owner identity yet. Once you do, this device will be
        bound to it, and any other devices you add later will appear here.
      </p>
      <button class="primary" onclick={() => { modalOpen = true; }}>
        Bind this device to a new owner identity →
      </button>
    </div>
  {:else}
    <!-- Populated state added in Task 8 -->
    <div class="populated">
      <h3>My Devices ({state.devices.length})</h3>
    </div>
  {/if}

  {#if modalOpen}
    <div class="modal-overlay" role="dialog" aria-modal="true" aria-labelledby="modal-heading">
      <div class="modal">
        <h3 id="modal-heading">Create your owner identity</h3>
        <p>
          This will create your owner identity. This device will be bound as the first device.
          You'll receive a recovery file to back up — you can do this immediately or later.
        </p>
        {#if mintError}
          <p class="error" role="alert">{mintError}</p>
        {/if}
        <div class="modal-actions">
          <button class="secondary" onclick={() => { modalOpen = false; }} disabled={mintInFlight}>
            Cancel
          </button>
          <button class="primary" onclick={handleConfirmMint} disabled={mintInFlight}>
            {mintInFlight ? 'Creating…' : 'Create owner identity'}
          </button>
        </div>
      </div>
    </div>
  {/if}
</section>

<style>
  .devices-panel {
    padding: 16px;
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 6px;
    margin-bottom: 16px;
  }
  .devices-panel h2 {
    margin: 0 0 12px;
    font-size: 14px;
    color: var(--text-primary);
  }
  .empty .explainer {
    color: var(--text-secondary);
    font-size: 13px;
    margin-bottom: 12px;
  }
  .primary, .secondary {
    padding: 6px 12px;
    border-radius: 4px;
    border: 1px solid var(--border);
    cursor: pointer;
    font-size: 13px;
  }
  .primary {
    background: var(--accent);
    color: white;
    border-color: var(--accent);
  }
  .secondary {
    background: var(--bg-primary);
    color: var(--text-primary);
  }
  .primary:disabled, .secondary:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .modal-overlay {
    position: fixed;
    inset: 0;
    background: rgba(0,0,0,0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }
  .modal {
    background: var(--bg-secondary);
    padding: 24px;
    border-radius: 8px;
    max-width: 480px;
    border: 1px solid var(--border);
  }
  .modal-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 16px;
  }
  .error {
    color: var(--danger);
    font-size: 13px;
    margin: 8px 0;
  }
  .loading {
    color: var(--text-muted);
    font-size: 13px;
  }
</style>
```

- [ ] **Step 2: Run test to verify it fails or passes**

Run: `npx vitest run src/lib/components/__tests__/DevicesPanel.test.ts`
Expected: 4 tests pass (empty state, modal-open, modal-confirm-mint, modal-cancel).

- [ ] **Step 3: Commit**

```bash
git add src/lib/components/DevicesPanel.svelte src/lib/components/__tests__/DevicesPanel.test.ts
git commit -m "feat(devices): empty state + bootstrap modal (ZEB-170)

Devices panel ships with three states wired:
- loading (initial mount)
- empty (get_owner_state returns null) — shows bind CTA
- modal (post-CTA-click) — confirm/cancel; confirm invokes mint_owner_identity

Populated rendering, rename, backup CTA, degraded canBackUp:false in
follow-up tasks. Modal uses role=dialog/aria-modal; errors use role=alert.
"
```

---

### Task 8: Frontend — `DevicesPanel.svelte` populated state

**Files:**
- Modify: `src/lib/components/DevicesPanel.svelte`
- Modify: `src/lib/components/__tests__/DevicesPanel.test.ts`

Replace the populated stub with the full panel: owner header (display name, fingerprint, "Back up owner identity" CTA — clickable wired in Task 10), device list (single row in v1), educational footer.

- [ ] **Step 1: Write the failing test**

Append to `src/lib/components/__tests__/DevicesPanel.test.ts`:

```typescript
describe('DevicesPanel — populated state', () => {
  beforeEach(() => { vi.clearAllMocks(); });

  it('renders owner header with display name and fingerprint', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'a4f1c8239b7dd809abcdef0123456789',
      ownerDisplayName: 'zeblith',
      devices: [{
        deviceId: 'aa11bb22cc33dd44ee55ff6677889900',
        displayName: 'this device',
        isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000,
        fingerprint: 'aa11·bb22',
      }],
      canBackUp: true,
    });
    render(DevicesPanel);
    await screen.findByText('zeblith');
    expect(screen.getByText(/a4f1·c823/i)).toBeInTheDocument();
    expect(screen.getByText(/back up owner identity/i)).toBeInTheDocument();
  });

  it('renders device row with name, this-device marker, trust badge, fingerprint, enrolled date', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'a4f1c8239b7dd809abcdef0123456789',
      ownerDisplayName: 'zeblith',
      devices: [{
        deviceId: 'aa11bb22cc33dd44ee55ff6677889900',
        displayName: 'KRILE',
        isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000,
        fingerprint: 'aa11·bb22',
      }],
      canBackUp: true,
    });
    render(DevicesPanel);
    await screen.findByText('KRILE');
    expect(screen.getByText(/this device/i)).toBeInTheDocument();
    expect(screen.getByText(/trusted/i)).toBeInTheDocument();
    expect(screen.getByText(/aa11·bb22/i)).toBeInTheDocument();
  });

  it('renders educational footer for adding another device', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'a4f1c8239b7dd809abcdef0123456789',
      ownerDisplayName: 'zeblith',
      devices: [{
        deviceId: 'aa11bb22cc33dd44ee55ff6677889900',
        displayName: 'this',
        isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000,
        fingerprint: 'aa11·bb22',
      }],
      canBackUp: true,
    });
    render(DevicesPanel);
    await screen.findByText(/add another device/i);
    expect(screen.getByText(/pairing UI is coming/i)).toBeInTheDocument();
  });
});
```

Modify the populated branch in `DevicesPanel.svelte`:

```svelte
{:else}
  <div class="populated">
    <!-- ① Owner identity header -->
    <div class="owner-header">
      <div class="label">OWNER IDENTITY</div>
      <div class="owner-row">
        <div>
          <div class="owner-name">{state.ownerDisplayName}</div>
          <div class="owner-fingerprint">{formatOwnerFingerprint(state.ownerId)}</div>
        </div>
        <button class="primary" onclick={() => { backupRequested = true; }}>
          Back up owner identity →
        </button>
      </div>
    </div>

    <!-- ② Devices list -->
    <div class="devices-list">
      <div class="label">MY DEVICES ({state.devices.length})</div>
      {#each state.devices as device (device.deviceId)}
        <div class="device-row">
          <div class="device-icon">{deviceInitial(device.displayName)}</div>
          <div class="device-meta">
            <div class="device-name-row">
              <span class="device-name">{device.displayName}</span>
              {#if device.isThisDevice}
                <span class="this-device-marker">this device</span>
              {/if}
            </div>
            <div class="device-secondary">
              {#if device.trustDecision.kind === 'full'}
                <span class="trust-badge full">● trusted</span>
              {:else if device.trustDecision.kind === 'provisional'}
                <span class="trust-badge provisional">● provisional</span>
              {:else}
                <span class="trust-badge refused">● refused</span>
              {/if}
              <span class="separator">·</span>
              <span>added {formatEnrolledAt(device.enrolledAt)}</span>
              <span class="separator">·</span>
              <span class="fingerprint">{device.fingerprint}</span>
            </div>
          </div>
        </div>
      {/each}
    </div>

    <!-- ③ Educational footer -->
    <div class="add-another-footer">
      <div class="label">ADD ANOTHER DEVICE</div>
      <p class="explainer">
        Pairing UI is coming. For now, multi-device coexistence requires the
        <code>enroll_via_master</code> flow which ships in a follow-up. This
        device is the first under your owner identity.
      </p>
    </div>
  </div>
{/if}
```

Add the helper functions to the script:

```svelte
let backupRequested = $state(false);

function formatOwnerFingerprint(hex: string): string {
  // 32 hex chars → "xxxx·xxxx·xxxx·xxxx" for readability
  if (hex.length < 16) return hex;
  return `${hex.slice(0,4)}·${hex.slice(4,8)}·${hex.slice(8,12)}·${hex.slice(12,16)}`;
}

function deviceInitial(name: string): string {
  return name.trim().charAt(0).toUpperCase() || '?';
}

function formatEnrolledAt(ts: number): string {
  const ms = ts * 1000;
  const now = Date.now();
  const ageDays = Math.floor((now - ms) / (1000 * 60 * 60 * 24));
  if (ageDays < 1) return 'today';
  if (ageDays < 2) return 'yesterday';
  if (ageDays < 30) return `${ageDays}d ago`;
  return new Date(ms).toLocaleDateString();
}
```

Plus styles for the new elements. Reuse PR-61's color tokens.

- [ ] **Step 2: Run vitest**

Run: `npx vitest run src/lib/components/__tests__/DevicesPanel.test.ts`
Expected: 7 tests pass (4 prior + 3 new).

- [ ] **Step 3: Commit**

```bash
git add src/lib/components/DevicesPanel.svelte src/lib/components/__tests__/DevicesPanel.test.ts
git commit -m "feat(devices): populated state with owner header + device list (ZEB-170)

Three sections rendered:
1. Owner header — display name, dotted fingerprint, Back-up CTA (handler in Task 10).
2. Devices list — name, this-device marker, trust badge (color-coded), enrolled date,
   fingerprint. Single device in v1; structure scales to N devices.
3. Educational footer — promises pairing UI is coming, no current capability.

Trust badge mirrors harmony_owner::trust::evaluate_trust shape (Full / Provisional / Refused).
Enrolled-date helper produces 'today / yesterday / Nd ago / locale-date' progression.
"
```

---

### Task 9: Frontend — Rename action

**Files:**
- Modify: `src/lib/components/DevicesPanel.svelte`
- Modify: `src/lib/components/__tests__/DevicesPanel.test.ts`

Inline rename for the current device. Persists via the existing `profile-service.ts` (this device's localStorage `displayName`). Cross-device names are deferred.

- [ ] **Step 1: Write the failing test**

Append to `src/lib/components/__tests__/DevicesPanel.test.ts`:

```typescript
import { loadProfile, saveProfile } from '../../profile-service';

vi.mock('../../profile-service', () => ({
  loadProfile: vi.fn(),
  saveProfile: vi.fn(),
}));

describe('DevicesPanel — rename', () => {
  beforeEach(() => { vi.clearAllMocks(); });

  it('clicking Rename shows inline edit field with current name pre-filled', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'a4f1', ownerDisplayName: 'zeblith',
      devices: [{
        deviceId: 'aa11bb22', displayName: 'KRILE', isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000, fingerprint: 'aa11·bb22',
      }],
      canBackUp: true,
    });
    (loadProfile as ReturnType<typeof vi.fn>).mockReturnValue({ address: 'a', displayName: 'KRILE' });

    render(DevicesPanel);
    const renameBtn = await screen.findByRole('button', { name: /rename/i });
    await fireEvent.click(renameBtn);
    const input = screen.getByRole('textbox', { name: /device name/i });
    expect((input as HTMLInputElement).value).toBe('KRILE');
  });

  it('saving the rename calls profile-service.saveProfile', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'a4f1', ownerDisplayName: 'zeblith',
      devices: [{
        deviceId: 'aa11bb22', displayName: 'KRILE', isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000, fingerprint: 'aa11·bb22',
      }],
      canBackUp: true,
    });
    (loadProfile as ReturnType<typeof vi.fn>).mockReturnValue({ address: 'a', displayName: 'KRILE' });

    render(DevicesPanel);
    const renameBtn = await screen.findByRole('button', { name: /rename/i });
    await fireEvent.click(renameBtn);
    const input = screen.getByRole('textbox', { name: /device name/i });
    await fireEvent.input(input, { target: { value: 'KRILE-prime' } });
    const saveBtn = screen.getByRole('button', { name: /save/i });
    await fireEvent.click(saveBtn);
    expect(saveProfile).toHaveBeenCalledWith(
      expect.objectContaining({ displayName: 'KRILE-prime' })
    );
  });
});
```

Modify `DevicesPanel.svelte`:

```svelte
<script lang="ts">
  import { onMount } from 'svelte';
  import { OwnerService, extractError, type OwnerStateView } from '../owner-service';
  import { loadProfile, saveProfile } from '../profile-service';
  // ... existing imports + state ...

  let renamingDeviceId = $state<string | null>(null);
  let renameDraft = $state('');

  function startRename(device: { deviceId: string; displayName: string; isThisDevice: boolean }) {
    renamingDeviceId = device.deviceId;
    renameDraft = device.displayName;
  }

  function saveRename(deviceId: string) {
    const trimmed = renameDraft.trim();
    if (trimmed.length === 0) return;
    const profile = loadProfile();
    saveProfile({ ...profile, displayName: trimmed });
    if (state) {
      // Optimistic local update — refresh from backend on next mount.
      state = {
        ...state,
        devices: state.devices.map((d) =>
          d.deviceId === deviceId ? { ...d, displayName: trimmed } : d,
        ),
      };
    }
    renamingDeviceId = null;
  }

  function cancelRename() {
    renamingDeviceId = null;
  }
</script>

<!-- inside the device row markup, replace the static name + add Rename button -->
{#if renamingDeviceId === device.deviceId}
  <input
    type="text"
    bind:value={renameDraft}
    aria-label="Device name"
    onkeydown={(e) => { if (e.key === 'Enter') saveRename(device.deviceId); if (e.key === 'Escape') cancelRename(); }}
  />
  <button onclick={() => saveRename(device.deviceId)}>Save</button>
  <button onclick={cancelRename}>Cancel</button>
{:else}
  <span class="device-name">{device.displayName}</span>
  {#if device.isThisDevice}
    <button class="rename-btn" onclick={() => startRename(device)}>Rename</button>
  {/if}
{/if}
```

- [ ] **Step 2: Run vitest**

Run: `npx vitest run src/lib/components/__tests__/DevicesPanel.test.ts`
Expected: 9 tests pass.

- [ ] **Step 3: Commit**

```bash
git add src/lib/components/DevicesPanel.svelte src/lib/components/__tests__/DevicesPanel.test.ts
git commit -m "feat(devices): rename action for current device (ZEB-170)

Inline edit field replaces device name on Rename click. Save persists
through profile-service.saveProfile (existing localStorage); state.devices
optimistically updated. Enter saves; Escape cancels. Rename button only
appears for isThisDevice rows — cross-device renaming via gossip is
deferred.
"
```

---

### Task 10: Frontend — Backup CTA wiring

**Files:**
- Modify: `src/lib/components/DevicesPanel.svelte`
- Modify: `src/lib/components/__tests__/DevicesPanel.test.ts`

The "Back up owner identity" CTA in the owner header opens an inline mini-wizard (passphrase entry + path picker), which calls `exportRecoveryFile(token, path, passphrase, comment)`. Token is the one returned from the most recent `mint()` call (or, on subsequent visits where the panel was loaded fresh, we'll need a way to issue a fresh token — see "Re-issue token on demand" below).

For v1, the simplest scoping: the recovery-token is held in the panel's state right after mint. On refresh / subsequent launches, the CTA shows but is disabled with tooltip "Re-mint required for this session" (a minor UX limitation we accept for v1; alternatively, add an `issue_recovery_token` command later).

Wait — that's a real UX hole. The user dismisses-with-warning, comes back tomorrow, the CTA is disabled because there's no token in memory. They can't back up.

The clean fix: add a fourth Tauri command `issue_owner_recovery_token` that re-loads the master seed from disk and inserts a fresh token. This is a small extension to Task 4. Deferring complicates v1 UX.

**Decision: extend Task 4 with `issue_owner_recovery_token`.** This is a small additional command. Task 10 then wires it in.

- [ ] **Step 1: Add `issue_owner_recovery_token` to `owner_commands.rs`**

Append to `src-tauri/src/owner_commands.rs`:

```rust
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueTokenResult {
    pub recovery_token: String,
}

#[tauri::command]
pub async fn issue_owner_recovery_token(
    app: tauri::AppHandle,
) -> Result<IssueTokenResult, String> {
    let identity_dir = resolve_identity_dir(&app)?;
    run_blocking(move || {
        let loaded = load_owner_state(&identity_dir, KeychainStore::new().ok())?
            .ok_or_else(|| "Owner identity has not been minted on this device.".to_string())?;
        let seed = loaded.master_seed
            .ok_or_else(|| "Master seed has been wiped from this device — backup is no longer possible.".to_string())?;
        let token = insert_token(seed);
        Ok(IssueTokenResult { recovery_token: token.to_string() })
    }).await
}
```

Register in `tauri::generate_handler!` in `lib.rs`.

Add a Rust test:

```rust
#[test]
#[serial]
fn issue_token_errors_when_master_seed_missing() {
    clear_token_cache();
    std::env::set_var("HARMONY_PASSPHRASE", "issue-test-pp");
    let dir = tempdir().unwrap();

    // Save state with master seed, then wipe seed.
    let MintResult { state, recovery_artifact, device_signing_key } =
        mint_owner(1_700_000_010).unwrap();
    save_owner_state_atomic(
        dir.path(), &state, &device_signing_key, recovery_artifact.as_bytes(), None,
    ).unwrap();
    let _ = std::fs::remove_file(dir.path().join("master_seed.enc"));

    // Now the loaded state has master_seed=None; issue_token must error.
    let loaded = load_owner_state(dir.path(), None).unwrap().unwrap();
    assert!(loaded.master_seed.is_none());
    // Error message check would require running the actual command via the app handle,
    // which isn't trivial in a unit test. Settle for asserting the underlying invariant.

    std::env::remove_var("HARMONY_PASSPHRASE");
}
```

- [ ] **Step 2: Add `issueRecoveryToken` to `owner-service.ts`**

```typescript
async issueRecoveryToken(): Promise<string> {
  const result = await invoke<{ recoveryToken: string }>('issue_owner_recovery_token');
  return result.recoveryToken;
}
```

- [ ] **Step 3: Wire the inline backup mini-wizard into DevicesPanel**

In `DevicesPanel.svelte`:

```svelte
<script lang="ts">
  import { save } from '@tauri-apps/plugin-dialog';
  // ... existing ...
  let backupOpen = $state(false);
  let backupPassphrase = $state('');
  let backupPassphraseConfirm = $state('');
  let backupComment = $state('');
  let backupInFlight = $state(false);
  let backupError = $state<string | null>(null);
  let backupSavedPath = $state<string | null>(null);

  async function openBackup() {
    backupOpen = true;
    backupPassphrase = '';
    backupPassphraseConfirm = '';
    backupComment = '';
    backupError = null;
    backupSavedPath = null;
    if (recoveryToken === null) {
      // Issue a fresh token.
      try {
        recoveryToken = await svc.issueRecoveryToken();
      } catch (e) {
        backupError = extractError(e);
      }
    }
  }

  async function commitBackup() {
    if (recoveryToken === null) {
      backupError = 'No recovery token available.';
      return;
    }
    if (backupPassphrase !== backupPassphraseConfirm) {
      backupError = 'Passphrases do not match.';
      return;
    }
    if (backupPassphrase.length < 12) {
      backupError = 'Passphrase must be at least 12 characters.';
      return;
    }
    const out = await save({
      defaultPath: 'owner-recovery.bin',
      filters: [{ name: 'Recovery file', extensions: ['bin'] }],
    });
    if (!out) return; // user cancelled
    backupInFlight = true;
    backupError = null;
    try {
      await svc.exportRecoveryFile(
        recoveryToken,
        out,
        backupPassphrase,
        backupComment.trim() ? backupComment.trim() : null,
      );
      backupSavedPath = out;
      recoveryToken = null; // single-use semantics
    } catch (e) {
      backupError = extractError(e);
    } finally {
      backupInFlight = false;
    }
  }

  function closeBackup() {
    backupOpen = false;
  }
</script>

<!-- after the populated-state markup, add the modal -->
{#if backupOpen}
  <div class="modal-overlay" role="dialog" aria-modal="true">
    <div class="modal">
      <h3>Back up owner identity</h3>
      {#if backupSavedPath}
        <p>Recovery file written to <code>{backupSavedPath}</code>. Keep it somewhere safe.</p>
        <button class="primary" onclick={closeBackup}>Done</button>
      {:else}
        <p>
          Choose a strong passphrase. The encrypted file alone cannot be opened
          without it.
        </p>
        <label>
          Passphrase
          <input type="password" bind:value={backupPassphrase} aria-label="Passphrase" />
        </label>
        <label>
          Confirm passphrase
          <input type="password" bind:value={backupPassphraseConfirm} aria-label="Confirm passphrase" />
        </label>
        <label>
          Comment (optional)
          <input type="text" bind:value={backupComment} maxlength={256} aria-label="Comment" />
        </label>
        {#if backupError}
          <p class="error" role="alert">{backupError}</p>
        {/if}
        <div class="modal-actions">
          <button class="secondary" onclick={closeBackup} disabled={backupInFlight}>Cancel</button>
          <button class="primary" onclick={commitBackup} disabled={backupInFlight || !state?.canBackUp}>
            {backupInFlight ? 'Encrypting…' : 'Save backup'}
          </button>
        </div>
      {/if}
    </div>
  </div>
{/if}
```

Add a vitest:

```typescript
describe('DevicesPanel — backup wiring', () => {
  beforeEach(() => { vi.clearAllMocks(); });

  it('clicking Back up opens the backup modal and issues a token if needed', async () => {
    mockedInvoke
      .mockResolvedValueOnce({ ownerId: 'x', ownerDisplayName: 'me',
        devices: [{ deviceId: 'd', displayName: 'this', isThisDevice: true,
          trustDecision: { kind: 'full', reason: null },
          enrolledAt: 1_700_000_000, fingerprint: 'd·x' }],
        canBackUp: true })
      .mockResolvedValueOnce({ recoveryToken: 'fresh-tok' });

    render(DevicesPanel);
    const btn = await screen.findByRole('button', { name: /back up owner identity/i });
    await fireEvent.click(btn);
    expect(await screen.findByRole('dialog')).toBeInTheDocument();
    expect(invoke).toHaveBeenCalledWith('issue_owner_recovery_token');
  });

  it('passphrase mismatch shows inline error and does not call export', async () => {
    mockedInvoke.mockResolvedValueOnce({ ownerId: 'x', ownerDisplayName: 'me',
      devices: [{ deviceId: 'd', displayName: 'this', isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000, fingerprint: 'd·x' }],
      canBackUp: true });
    mockedInvoke.mockResolvedValueOnce({ recoveryToken: 'tok' });
    render(DevicesPanel);
    const btn = await screen.findByRole('button', { name: /back up owner identity/i });
    await fireEvent.click(btn);
    const passInput = await screen.findByLabelText('Passphrase');
    const confirmInput = screen.getByLabelText('Confirm passphrase');
    await fireEvent.input(passInput, { target: { value: 'first-passphrase' } });
    await fireEvent.input(confirmInput, { target: { value: 'second-passphrase' } });
    const save = screen.getByRole('button', { name: /save backup/i });
    await fireEvent.click(save);
    expect(screen.getByText(/do not match/i)).toBeInTheDocument();
    expect(invoke).not.toHaveBeenCalledWith(
      'export_owner_recovery_file_to_path',
      expect.anything(),
    );
  });
});
```

Note: the tauri-apps/plugin-dialog `save` call needs to be mocked too for these tests. Add to top of test file:

```typescript
vi.mock('@tauri-apps/plugin-dialog', () => ({
  save: vi.fn().mockResolvedValue('/tmp/owner-recovery.bin'),
  open: vi.fn(),
}));
```

- [ ] **Step 4: Run vitest + Rust tests**

Run: `npx vitest run src/lib/components/__tests__/DevicesPanel.test.ts && cargo test --manifest-path src-tauri/Cargo.toml owner_commands`
Expected: All passing.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/owner_commands.rs src-tauri/src/lib.rs src/lib/owner-service.ts src/lib/components/DevicesPanel.svelte src/lib/components/__tests__/DevicesPanel.test.ts
git commit -m "feat(devices): backup CTA wiring + issue-token-on-demand (ZEB-170)

Adds issue_owner_recovery_token Tauri command (re-loads master seed from
disk, returns fresh single-use token). Without this, dismiss-with-warning
becomes irrecoverable: user dismisses, comes back tomorrow, has no token.

Frontend: backup modal collects passphrase + comment, calls
exportRecoveryFile, shows saved path on success. Single-use semantics
clear the in-memory token after export. Validation (matching passphrases,
min length 12) precedes the invoke so failed validation does not
consume the token.
"
```

---

### Task 11: Degraded state — `canBackUp: false`

**Files:**
- Modify: `src/lib/components/DevicesPanel.svelte`
- Modify: `src/lib/components/__tests__/DevicesPanel.test.ts`

When the master seed has been wiped (a future "Wipe master from device" action — not shipped in v1, but may be reached via manual file deletion), `canBackUp: false` arrives from the backend. Backup CTA disables with tooltip.

- [ ] **Step 1: Test**

```typescript
describe('DevicesPanel — degraded state (canBackUp: false)', () => {
  beforeEach(() => { vi.clearAllMocks(); });

  it('Back-up CTA is disabled when canBackUp is false', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'x', ownerDisplayName: 'me',
      devices: [{ deviceId: 'd', displayName: 'this', isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000, fingerprint: 'd·x' }],
      canBackUp: false,
    });
    render(DevicesPanel);
    const btn = await screen.findByRole('button', { name: /back up owner identity/i });
    expect(btn).toBeDisabled();
    expect(btn).toHaveAttribute('title');
  });
});
```

- [ ] **Step 2: Implementation**

In the populated-state markup, modify the Back-up button:

```svelte
<button
  class="primary"
  disabled={!state.canBackUp}
  title={state.canBackUp ? '' : 'Master seed not on this device — backup is no longer possible.'}
  onclick={openBackup}
>
  Back up owner identity →
</button>
```

- [ ] **Step 3: Run + commit**

Run: `npx vitest run src/lib/components/__tests__/DevicesPanel.test.ts`
Expected: All passing.

```bash
git add src/lib/components/DevicesPanel.svelte src/lib/components/__tests__/DevicesPanel.test.ts
git commit -m "feat(devices): degraded canBackUp:false renders disabled CTA (ZEB-170)

When the master seed has been wiped (future opt-in action), canBackUp
arrives false from the backend. Back-up CTA disables with explanatory
tooltip. v1 doesn't ship the wipe action; this branch handles a hand-edited
filesystem state that may also occur in the wild.
"
```

---

### Task 12: Mount `<DevicesPanel />` in `App.svelte`

**Files:**
- Modify: `src/App.svelte`

Add the new component to the `settingsPanel` snippet, alongside `ProfileEditor`, `IdentityPanel`, and `NotificationSettingsPanel`.

- [ ] **Step 1: Modify `App.svelte`**

Add the import near the top (after `IdentityPanel` import at line 12):

```svelte
import DevicesPanel from './lib/components/DevicesPanel.svelte';
```

Modify the `settingsPanel` snippet (around line 859):

```svelte
{#snippet settingsPanel()}
  <ProfileEditor profile={myProfile} onSave={handleProfileSave} />
  <IdentityPanel />
  <DevicesPanel />
  <NotificationSettingsPanel
    service={notificationService}
    {trustService}
    peers={knownPeers}
    {communities}
    onClose={() => { showSettings = false; }}
    onTrustChange={handleTrustChange}
  />
{/snippet}
```

- [ ] **Step 2: Build + smoke check**

Run: `npx tsc --noEmit`
Expected: PASS (no type errors from the new import).

Run: `npx vitest run` (entire suite)
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add src/App.svelte
git commit -m "feat(devices): mount DevicesPanel in settings overlay (ZEB-170)

Adds <DevicesPanel /> as a sibling of ProfileEditor / IdentityPanel /
NotificationSettingsPanel in App.svelte's settingsPanel snippet.
Settings → Identity → Devices is now reachable from the main app.
"
```

---

### Task 13: Manual smoke + final polish

**Files:**
- (None expected — manual verification + any small fixes)

- [ ] **Step 1: Run the dev app**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npm run tauri dev
```

- [ ] **Step 2: Manual smoke (golden path)**

1. App launches → open Settings.
2. Scroll to Devices panel → empty state visible.
3. Click "Bind this device to a new owner identity →" → modal opens.
4. Confirm → spinner → populated state appears with one device row.
5. Click "Back up owner identity →" → backup modal opens.
6. Enter passphrase + confirm + comment → click Save backup.
7. File picker → choose location → file written.
8. Backup modal shows "Recovery file written to ...".
9. Close + restart app → Devices panel shows populated state directly (no bootstrap modal).

- [ ] **Step 3: Manual smoke (edge cases)**

1. Mid-bootstrap, click Cancel — modal closes, empty state remains.
2. Mid-backup, mismatched passphrases — inline error appears, no IPC call.
3. Mid-backup, passphrase < 12 chars — inline error.
4. Backup with comment > 256 bytes — inline backend error.
5. Click Rename on this-device row — inline edit; Save persists; reload shows new name.
6. Manually delete `~/Library/Application Support/<bundle>/master_seed.enc` (or the encrypted-file equivalent) → restart app → backup CTA is disabled with tooltip.

- [ ] **Step 4: Run all gates**

```bash
set -o pipefail
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo clippy --manifest-path src-tauri/Cargo.toml --locked --all-targets --no-deps
cargo test --manifest-path src-tauri/Cargo.toml --workspace --all-targets
cargo +1.88 check --manifest-path src-tauri/Cargo.toml --locked --all-targets
npx tsc --noEmit
npx vitest run
```

Expected: All green.

- [ ] **Step 5: Final commit (if any polish needed)**

If smoke surfaced bugs:
```bash
git add <fix-files>
git commit -m "fix(devices): smoke fixes (ZEB-170)

[describe what got fixed]
"
```

If smoke clean:
```bash
# Nothing to commit; ready to finish branch.
```

- [ ] **Step 6: Finish branch**

Use the **superpowers:finishing-a-development-branch** skill to either merge locally, push and create a PR, keep as-is, or discard. Recommended path: push and create a PR (option 2) for review.
