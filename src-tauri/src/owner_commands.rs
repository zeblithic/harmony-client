//! Tauri command surface for the Devices panel.
//!
//! Wraps `crate::owner_state` operations with async + state-injection
//! plumbing. Long-running ops go through `crate::identity_commands::run_blocking`.

use crate::identity::KeychainStore;
use crate::identity_commands::run_blocking;
use crate::owner_state::{
    insert_token, load_owner_state, refresh_self_liveness, save_owner_state_atomic,
    save_owner_state_cbor_only, take_token, DeviceView, LoadedOwnerState, OwnerStateView,
    TrustDecisionView, TrustKind,
};
use crate::recovery_policy::{MAX_RECOVERY_COMMENT_BYTES, MIN_RECOVERY_PASSPHRASE_LEN};
use harmony_owner::lifecycle::{mint_owner, MintResult, RecoveryArtifact};
use harmony_owner::recovery::RecoveryMetadata;
use harmony_owner::trust;
use secrecy::SecretString;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;
use zeroize::Zeroizing;

/// Process-wide mutex for **all writers of `owner_state.cbor` and its
/// companion seed/key entries** — mint plus the pairing-persist drainer
/// that calls `install_inviter_state` / `install_joiner_state` after a
/// successful pair. Without this serialization (ZEB-199), two concurrent
/// pairing-Completes can both load the pre-mutation OwnerState, each add
/// their own enrollment, and one writer's enrollment is silently lost
/// when the second `save_owner_state_atomic` overwrites the first.
///
/// Originally introduced as `MINT_OWNER_LOCK` in PR #62 to guard the
/// mint check-and-write window against rapid double-click; renamed to
/// reflect its broader role during ZEB-199 review. Held across each
/// caller's entire load+save window. Recover from poisoning so a panic
/// in one handler doesn't brick future writes (mirrors PR-61's
/// preview_cache_lock policy).
///
/// Note: this lock does NOT cover `rotate_passphrase` /
/// `restore_recovery_from_preview_token`, which write the encrypted-file
/// fallback (`identity.key.enc`) but not `owner_state.cbor`. See ZEB-201
/// for the parallel race on those paths.
pub(crate) static OWNER_STATE_WRITE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

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
    pub path: String,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build an `OwnerStateView` from a loaded state.
///
/// `pinned_device_id_hex`: the 64-hex fleet-net `pinned` field (from the
/// in-memory `FleetNetDoc`). When `Some`, the matching device row receives
/// `butler_pinned: true`. Defaults to `false` when `None` (fleet-net cold
/// or node not running).
fn build_owner_state_view(
    loaded: &LoadedOwnerState,
    this_device_name: String,
    pinned_device_id_hex: Option<String>,
) -> OwnerStateView {
    let now = now_unix();
    let active_window = trust::DEFAULT_ACTIVE_WINDOW_SECS;
    let freshness = trust::DEFAULT_FRESHNESS_WINDOW_SECS;
    let this_device_id = derive_this_device_id(&loaded.device_signing_key);

    let devices: Vec<DeviceView> = loaded
        .state
        .enrollments
        .values()
        .map(|cert| {
            let decision =
                trust::evaluate_trust(&loaded.state, cert.device_id, now, active_window, freshness);
            let (kind, reason) = match decision {
                trust::TrustDecision::Full => (TrustKind::Full, None),
                trust::TrustDecision::Provisional => (TrustKind::Provisional, None),
                trust::TrustDecision::Refused(r) => (TrustKind::Refused, Some(format!("{r:?}"))),
            };
            // The fleet-net doc keys on hex of the 32-byte ed25519 verify key;
            // enrollment certs carry the ed25519 verify key directly (first 32
            // bytes of the classical bundle). Compare device_id hex forms.
            let dev_id_hex = hex::encode(cert.device_pubkeys.classical.ed25519_verify);
            let butler_pinned = pinned_device_id_hex
                .as_deref()
                .map(|p| p == dev_id_hex)
                .unwrap_or(false);
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
                butler_pinned,
                // Round-2 Greptile P1: the toggle must send THIS value to
                // `set_butler_pin` — `device_id` above is the 16-byte
                // identity hash, which the enrolled-set check rejects.
                device_vk_hex: dev_id_hex,
            }
        })
        .collect();

    OwnerStateView {
        owner_id: hex::encode(loaded.state.owner_id),
        owner_display_name: this_device_name,
        devices,
        can_back_up: loaded.master_seed.is_some(),
    }
}

fn derive_this_device_id(sk: &ed25519_dalek::SigningKey) -> [u8; 16] {
    // Delegates to the single source of truth so the Devices-panel view and the
    // liveness refresh can never derive the local device id differently.
    crate::owner_state::device_id_from_signing_key(sk)
}

/// Format the first 4 bytes of a 16-byte device_id as `xxxx·xxxx`
/// for display. The full id is internal plumbing — see the
/// "Two-address world" section of the design spec.
fn format_fingerprint(id: &[u8; 16]) -> String {
    let hex = hex::encode(id);
    format!("{}·{}", &hex[..4], &hex[4..8])
}

/// Resolve the directory where owner_state.cbor + companion files live.
///
/// Workaround: `crate::identity::identity_dir(AppHandle)` does not exist;
/// instead, take the parent of the per-device identity key path. Assumes
/// `identity.key` is never at the filesystem root — true on every Tauri-
/// supported OS (macOS / Linux / Windows).
pub(crate) fn resolve_identity_dir() -> Result<PathBuf, String> {
    let key_path = crate::identity::resolve_path(None)?;
    key_path
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| "identity key path has no parent directory".to_string())
}

#[tauri::command]
pub async fn get_owner_state(
    _app: tauri::AppHandle,
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<Option<OwnerStateView>, String> {
    get_owner_state_impl(state.inner()).await
}

/// ZEB-445: shared IPC/RPC seam.
pub(crate) async fn get_owner_state_impl(
    state: &std::sync::Mutex<crate::NodeState>,
) -> Result<Option<OwnerStateView>, String> {
    // ZEB-418 P2 D17: snapshot the fleet-net pinned device ID before entering
    // the blocking task. Reads under the NodeState lock; the Arc clone is cheap
    // and the tokio Mutex lock is async — we do it here (async context) and pass
    // the resolved `Option<String>` into the blocking closure (no async in there).
    let pinned_device_id_hex: Option<String> = {
        let fleet_net_doc_arc = {
            let g = state
                .lock()
                .map_err(|e| format!("NodeState poisoned: {e}"))?;
            g.fleet_net_doc.clone()
        };
        match fleet_net_doc_arc {
            Some(arc) => arc.lock().await.pinned.clone(),
            None => None,
        }
    };
    let identity_dir = resolve_identity_dir()?;
    let display_name = "this device".to_string();
    run_blocking(move || {
        // ZEB-342: hold the write lock only across load+refresh+save, so the cbor
        // write stays serialized with mint / pairing-install (loading inside the
        // lock closes the read-modify-write race). The lock is released at the end
        // of this block — build_owner_state_view below only reads the already-local
        // `loaded` snapshot (trust eval + formatting), which needs no serialization.
        let loaded = {
            let _guard = OWNER_STATE_WRITE_LOCK
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let mut loaded = match load_owner_state(&identity_dir, KeychainStore::new().ok())? {
                Some(l) => l,
                None => return Ok(None),
            };
            if refresh_self_liveness(&mut loaded.state, &loaded.device_signing_key, now_unix()) {
                // Fail open: the in-memory state already carries the fresh liveness, so
                // the panel renders correctly even if persistence fails. A persist error
                // must NOT block the Devices panel (it didn't before this change); the
                // next load retries the refresh + write.
                if let Err(e) = save_owner_state_cbor_only(&identity_dir, &loaded.state) {
                    tracing::warn!(
                        error = %e,
                        "get_owner_state: failed to persist refreshed liveness; rendering from in-memory state"
                    );
                }
            }
            loaded
        };
        Ok(Some(build_owner_state_view(
            &loaded,
            display_name,
            pinned_device_id_hex,
        )))
    })
    .await
}

#[tauri::command]
pub async fn mint_owner_identity(
    app: tauri::AppHandle,
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<MintIpcResult, String> {
    // Forward to the testable inner fn (symmetric with `start_node_inner`).
    // The restart step is injected as a closure so the inner fn can be
    // driven from a headless integration test (where a real `AppHandle<Wry>`
    // — required by `start_node_inner` — cannot be constructed). Production
    // passes the real node restart.
    let state_ref: &Mutex<crate::NodeState> = state.inner();
    // ZEB-428: the real keychain is acquired HERE (production wrapper) and
    // injected, mirroring pairing/persist.rs's install_joiner_state — the
    // inner fn must never construct it internally, so test drivers can't
    // reach the developer's real credential store.
    mint_owner_identity_inner(state_ref, KeychainStore::new().ok(), || async {
        // ZEB-445: wrap the AppHandle as the event sink (same shape as the
        // `start_node` command wrapper).
        let sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink> =
            std::sync::Arc::new(app.clone());
        crate::start_node_inner(None, sink, Some(app.clone()), state_ref)
            .await
            .map(|_| ())
    })
    .await
}

/// Core of `mint_owner_identity`, extracted for testability (mirrors
/// `start_node_inner`). Flow: stop node → mint+persist (under the
/// owner-state write lock) → restart node.
///
/// `restart` performs the node restart given the already-persisted owner
/// state on disk. Production supplies a closure that calls
/// `crate::start_node_inner`; tests supply a closure that records invocation
/// (so they can assert "restart happens after mint, with cbor on disk") or
/// deliberately fails (to lock the no-rollback invariant below).
///
/// `keychain` is injected by the caller (ZEB-428): production passes
/// `KeychainStore::new().ok()`, the test shim passes `None`. The inner fn
/// must never construct the real store itself — the OS keychain is a
/// process-global resource that a test's HOME-to-tempdir redirect cannot
/// scope, and an internal `new()` here once let a full-suite run overwrite
/// a developer's real owner identity.
pub(crate) async fn mint_owner_identity_inner<F, Fut>(
    state: &Mutex<crate::NodeState>,
    keychain: Option<KeychainStore>,
    restart: F,
) -> Result<MintIpcResult, String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let identity_dir = resolve_identity_dir()?;
    let display_name = "this device".to_string();

    // Idempotent failure if already minted — existing guard, kept. The hard
    // gate (frontend) means this is normally unreachable, but a race or a
    // direct DevicesPanel call could hit it.
    if identity_dir.join("owner_state.cbor").exists() {
        return Err(
            "Owner identity already exists on this device. Wipe via Settings to re-mint."
                .to_string(),
        );
    }

    // ── Phase 1: stop the node ──────────────────────────────────────────
    // ZEB-338: mint takes responsibility for the node lifecycle so the user
    // never has to "stop the node" by hand (the old require_node_stopped
    // dead-end). `stop_inner` is async-context-safe — it drives its async
    // shutdown on an ephemeral runtime inside std::thread::scope, so calling
    // it from this async fn does NOT panic with a nested runtime.
    // `None` = stop unconditionally (no generation check).
    crate::stop_inner(state, None);

    // ── Phase 2: mint + persist ─────────────────────────────────────────
    // Held under OWNER_STATE_WRITE_LOCK to serialize concurrent mints.
    // metadata-before-irreversible-write note (feedback_metadata_before_
    // irreversible_write): the cbor + keychain write here IS the desired
    // irreversible write. If Phase 3 (restart) fails afterward we do NOT roll
    // it back — rolling back would lose the user's freshly minted identity
    // (spec §7.1). The cost of a failed restart is a manual relaunch, which
    // is strictly better than identity loss.
    let mint_dir = identity_dir.clone();
    let mint_result = run_blocking(move || {
        // Hold the process-wide owner-state write mutex for the entire
        // check-and-write window. Without this, concurrent mints could both
        // observe an absent owner_state.cbor and race to write competing
        // OwnerStates; pairing-persist callers (ZEB-199) take the same lock
        // for the same reason on the load+save side. Recover from
        // poisoning so a panic in one handler doesn't brick future writes
        // (mirrors PR-61's preview_cache_lock policy).
        let _owner_write_guard = OWNER_STATE_WRITE_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // Re-check under the lock (TOCTOU: another caller could have minted
        // between the outer check and acquiring the lock).
        if mint_dir.join("owner_state.cbor").exists() {
            return Err(
                "Owner identity already exists on this device. Wipe via Settings to re-mint."
                    .to_string(),
            );
        }
        let MintResult {
            state: owner_state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(now_unix()).map_err(|e| format!("mint_owner: {e}"))?;
        let master_seed: Zeroizing<[u8; 32]> = Zeroizing::new(*recovery_artifact.as_bytes());
        save_owner_state_atomic(
            &mint_dir,
            &owner_state,
            &device_signing_key,
            Some(&*master_seed),
            keychain,
        )?;
        let token = insert_token(master_seed.clone());
        let loaded = LoadedOwnerState {
            state: owner_state,
            device_signing_key,
            master_seed: Some(master_seed),
        };
        Ok(MintIpcResult {
            // Mint happens before the node restarts — fleet-net is not yet
            // running so `butler_pinned` is always false here (fresh identity).
            state: build_owner_state_view(&loaded, display_name, None),
            recovery_token: token.to_string(),
        })
    })
    .await?;

    // ── Phase 3: restart the node — now loads owner_state.cbor ──────────
    // NO ROLLBACK on failure: the mint above already wrote the identity to
    // disk. If the restart errors we surface it but leave the minted
    // identity in place (see Phase 2 note + spec §7.1).
    restart()
        .await
        .map_err(|e| format!("Node restart failed after mint: {e}"))?;

    Ok(mint_result)
}

/// Test-only public shim over [`mint_owner_identity_inner`] so headless
/// integration tests (a separate crate, no `pub(crate)` visibility) can
/// drive the mint lifecycle with an injected restart closure. Never
/// compiled into production (gated behind `test-fixtures`).
///
/// ZEB-428: the shim hard-codes `keychain: None` — the mint persists
/// through the encrypted-file fallback inside the test's tempdir HOME,
/// never the developer's real OS keychain. (Defense-in-depth: even if a
/// future caller bypassed this shim, `KeychainStore::new()` refuses in
/// test-fixtures builds.)
#[cfg(feature = "test-fixtures")]
pub async fn mint_owner_identity_inner_for_test<F, Fut>(
    state: &Mutex<crate::NodeState>,
    restart: F,
) -> Result<MintIpcResult, String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    mint_owner_identity_inner(state, None, restart).await
}

#[tauri::command]
pub async fn export_owner_recovery_file_to_path(
    recovery_token: String,
    path_token: String,
    passphrase: String,
    comment: Option<String>,
) -> Result<ExportInfo, String> {
    // Validate passphrase length BEFORE consuming any token (existing).
    // Use Unicode codepoint count (not byte count) so the "characters"
    // error wording matches the check, and so multibyte passphrases
    // (emoji, CJK) round-trip identically with the JS frontend's
    // [...str].length check.
    if passphrase.chars().count() < MIN_RECOVERY_PASSPHRASE_LEN {
        return Err(format!(
            "Recovery passphrase must be at least {MIN_RECOVERY_PASSPHRASE_LEN} characters."
        ));
    }
    // Validate comment length BEFORE consuming any token (existing).
    // 256-BYTE cap matches harmony-owner's hard limit on the underlying
    // field. Frontend mirrors with a TextEncoder byte count before submit.
    let comment_validated = match comment {
        Some(c) if c.len() > MAX_RECOVERY_COMMENT_BYTES => {
            return Err(format!(
                "Recovery comment must be at most {MAX_RECOVERY_COMMENT_BYTES} bytes."
            ));
        }
        c => c,
    };
    let recovery_uuid: Uuid = recovery_token
        .parse()
        .map_err(|e| format!("invalid recovery token: {e}"))?;
    let path_uuid: Uuid = path_token
        .parse()
        .map_err(|e| format!("invalid path token: {e}"))?;
    run_blocking(move || {
        // Consume path_token FIRST so a downstream seed-token consumption
        // failure does not leave a path token live in the cache pointing
        // at the user's chosen file (ZEB-194 ordering invariant — see test
        // `export_consumes_path_token_even_when_seed_token_invalid`).
        let out = crate::owner_state::take_path_token(&path_uuid).ok_or_else(|| {
            "Save path token expired or invalid. Please re-trigger backup.".to_string()
        })?;
        let seed = take_token(&recovery_uuid).ok_or_else(|| {
            "Recovery token expired or invalid. Please re-trigger backup from the Devices panel."
                .to_string()
        })?;
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
            path: out.display().to_string(),
        })
    })
    .await
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueTokenResult {
    pub recovery_token: String,
}

#[tauri::command]
pub async fn issue_owner_recovery_token(
    _app: tauri::AppHandle,
) -> Result<IssueTokenResult, String> {
    let identity_dir = resolve_identity_dir()?;
    run_blocking(move || {
        let loaded = load_owner_state(&identity_dir, KeychainStore::new().ok())?
            .ok_or_else(|| "Owner identity has not been minted on this device.".to_string())?;
        let seed = loaded.master_seed.ok_or_else(|| {
            "Master seed has been wiped from this device — backup is no longer possible."
                .to_string()
        })?;
        let token = insert_token(seed);
        Ok(IssueTokenResult {
            recovery_token: token.to_string(),
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state::{
        clear_path_token_cache, clear_token_cache, insert_path_token, take_path_token,
    };
    use serial_test::serial;

    /// RAII guard: sets an env var on construction, removes it on drop (even on panic).
    /// Prevents a panicking test from leaking HARMONY_PASSPHRASE into the next
    /// `#[serial]` test.
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
    fn export_with_too_short_passphrase_errors_without_consuming_token() {
        clear_token_cache();
        clear_path_token_cache();
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-cmd-test-pp");
        let recovery_uuid = insert_token(Zeroizing::new([0xCDu8; 32]));
        let path_uuid = insert_path_token(PathBuf::from("/tmp/should-not-write"));
        // Use a too-short passphrase.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(export_owner_recovery_file_to_path(
            recovery_uuid.to_string(),
            path_uuid.to_string(),
            "short".into(),
            None,
        ));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("at least"),
            "error must mention passphrase length; got: {err}"
        );
        // Token must NOT have been consumed (validation precedes take).
        assert!(
            take_token(&recovery_uuid).is_some(),
            "weak-passphrase rejection must not consume token"
        );
        // Path token must ALSO survive: validation runs before any cache
        // consumption, so neither token must be consumed on this path.
        assert!(
            take_path_token(&path_uuid).is_some(),
            "weak-passphrase rejection must not consume path token"
        );
    }

    #[test]
    #[serial]
    fn export_with_invalid_token_errors() {
        clear_token_cache();
        clear_path_token_cache();
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-cmd-test-pp");
        let bogus = Uuid::new_v4();
        let path_uuid = insert_path_token(PathBuf::from("/tmp/should-not-write"));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(export_owner_recovery_file_to_path(
            bogus.to_string(),
            path_uuid.to_string(),
            "passphrase-12+".into(),
            None,
        ));
        assert!(result.is_err());
        let err = result.unwrap_err();
        // Path token consumes first and succeeds, so this error originates
        // from the recovery_token consumption.
        assert!(
            err.contains("expired") || err.contains("invalid"),
            "actual: {err}"
        );
    }

    #[test]
    #[serial]
    fn comment_over_cap_rejected() {
        clear_token_cache();
        clear_path_token_cache();
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-cmd-test-pp");
        let recovery_uuid = insert_token(Zeroizing::new([0xEEu8; 32]));
        let path_uuid = insert_path_token(PathBuf::from("/tmp/should-not-write"));
        let comment = "x".repeat(257);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(export_owner_recovery_file_to_path(
            recovery_uuid.to_string(),
            path_uuid.to_string(),
            "passphrase-12+".into(),
            Some(comment),
        ));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("256") || err.contains("at most"),
            "error must mention comment cap; got: {err}"
        );
        // Token must NOT have been consumed.
        assert!(
            take_token(&recovery_uuid).is_some(),
            "comment-over-cap rejection must not consume token"
        );
        // Path token must ALSO survive: validation runs before any cache
        // consumption, so neither token must be consumed on this path.
        assert!(
            take_path_token(&path_uuid).is_some(),
            "comment-over-cap rejection must not consume path token"
        );
    }

    #[test]
    #[serial]
    fn export_with_invalid_path_token_errors() {
        clear_token_cache();
        clear_path_token_cache();
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-cmd-test-pp");
        let recovery_uuid = insert_token(Zeroizing::new([0xAAu8; 32]));
        let bogus_path_uuid = Uuid::new_v4();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(export_owner_recovery_file_to_path(
            recovery_uuid.to_string(),
            bogus_path_uuid.to_string(),
            "passphrase-12+".into(),
            None,
        ));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_lowercase().contains("path token")
                && (err.contains("expired") || err.contains("invalid")),
            "error must mention path token expired/invalid; got: {err}"
        );
        // Recovery token MUST survive: path-token consumption happens first
        // and fails, so seed-token consumption never runs.
        assert!(
            take_token(&recovery_uuid).is_some(),
            "invalid path-token must not consume recovery token"
        );
    }

    #[test]
    #[serial]
    fn export_consumes_path_token_even_when_seed_token_invalid() {
        // Pins the consumption ORDER: path_token taken first; if that succeeds
        // and seed-token consumption fails, the path token is still gone (so a
        // later replay of either token is impossible). This documents the
        // invariant against future refactors that might reorder consumption.
        clear_token_cache();
        clear_path_token_cache();
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-cmd-test-pp");
        let bogus_recovery_uuid = Uuid::new_v4();
        let path_uuid = insert_path_token(PathBuf::from("/tmp/zeb194-ordering-test.bin"));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(export_owner_recovery_file_to_path(
            bogus_recovery_uuid.to_string(),
            path_uuid.to_string(),
            "passphrase-12+".into(),
            None,
        ));
        assert!(result.is_err());
        // Path token MUST have been consumed (taken first) even though the
        // overall command failed.
        assert!(
            take_path_token(&path_uuid).is_none(),
            "path token must be consumed even when subsequent seed-token consumption fails"
        );
    }

    #[test]
    #[serial]
    fn export_consumes_both_tokens_on_success() {
        // Drives a real write_atomic_0600 into a tempdir to verify the
        // happy path end-to-end: both tokens consumed AND ExportInfo.path
        // echoes the user-confirmed save location. The tempdir Drop
        // cleans up at scope exit.
        clear_token_cache();
        clear_path_token_cache();
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-cmd-test-pp");
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("recovery.bin");
        let recovery_uuid = insert_token(Zeroizing::new([0xBBu8; 32]));
        let path_uuid = insert_path_token(out.clone());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(export_owner_recovery_file_to_path(
            recovery_uuid.to_string(),
            path_uuid.to_string(),
            "passphrase-12+".into(),
            None,
        ));
        assert!(result.is_ok(), "export must succeed; got: {result:?}");
        // Both caches must no longer hold the consumed UUIDs.
        assert!(
            take_token(&recovery_uuid).is_none(),
            "recovery token must be consumed"
        );
        assert!(
            take_path_token(&path_uuid).is_none(),
            "path token must be consumed"
        );
        // ExportInfo.path must echo the chosen path.
        let info = result.unwrap();
        assert_eq!(info.path, out.display().to_string());
    }

    /// ZEB-418 P2 round-2 (Greptile P1): cross-layer contract test pinning the
    /// device-ID FORMAT across the toggle round trip. The view's
    /// `device_vk_hex` must be the exact string that `set_butler_pin_inner`
    /// validates against (64-hex ed25519 verify key, the SP1 form the
    /// fleet-net doc keys on) AND the value that, fed back as
    /// `pinned_device_id_hex`, lights up `butler_pinned` on the same row.
    /// The pre-fix bug shipped `device_id` (the 16-byte identity hash) to the
    /// toggle — rejected for every device — and an opaque-ID test would miss
    /// it again, so this test derives everything from ONE enrollment-cert
    /// fixture.
    #[tokio::test]
    async fn device_vk_hex_round_trips_through_set_butler_pin() {
        use crate::fleet_net::FleetNetDoc;
        use harmony_owner::pubkey_bundle::PubKeyBundle;
        use std::collections::BTreeSet;

        // ── One fixture: mint an owner (1 enrollment) + enroll a 2nd device ─
        let MintResult {
            mut state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(1_700_000_000).expect("mint");
        let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());

        let joiner_sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let joiner_pubkey = PubKeyBundle::classical_only(joiner_sk.verifying_key().to_bytes());
        let joiner_cert = crate::pairing::cert::sign_enrollment_for_joiner(
            &master_seed,
            &state,
            joiner_pubkey,
            1_700_000_001,
        )
        .expect("sign joiner enrollment");
        let joiner_identity_hash_hex = hex::encode(joiner_cert.device_id);
        state.enrollments.insert(joiner_cert.device_id, joiner_cert);

        let loaded = LoadedOwnerState {
            state,
            device_signing_key,
            master_seed: Some(master_seed),
        };

        // ── (a) Build the view with no pin; take the joiner's device_vk_hex ─
        let view = build_owner_state_view(&loaded, "this device".into(), None);
        assert_eq!(view.devices.len(), 2, "mint device + joiner");
        let joiner_row = view
            .devices
            .iter()
            .find(|d| d.device_id == joiner_identity_hash_hex)
            .expect("joiner row present in view");
        let vk_hex = joiner_row.device_vk_hex.clone();
        // The two ID forms must be distinct: 64-hex VK vs 32-hex identity hash.
        assert_eq!(vk_hex.len(), 64, "device_vk_hex is the 64-hex VK form");
        assert_eq!(
            joiner_row.device_id.len(),
            32,
            "device_id is the 32-hex identity-hash form"
        );
        assert!(view.devices.iter().all(|d| !d.butler_pinned), "no pin yet");

        // ── (b) Derive the enrolled set EXACTLY as start_node does ──────────
        let enrolled: BTreeSet<String> = loaded
            .state
            .enrollments
            .values()
            .map(|cert| hex::encode(cert.device_pubkeys.classical.ed25519_verify))
            .collect();
        // Sanity: the identity-hash form (the pre-fix toggle payload) is NOT
        // in the enrolled set — that is the bug this test pins against.
        assert!(
            !enrolled.contains(&joiner_identity_hash_hex),
            "identity-hash form must not be a valid pin id"
        );

        // ── (c) set_butler_pin_inner must ACCEPT the view's device_vk_hex ───
        let doc = tokio::sync::Mutex::new(FleetNetDoc::default());
        crate::set_butler_pin_inner(&doc, &enrolled, Some(vk_hex.clone()), "self-dev", 1_000)
            .await
            .expect("set_butler_pin_inner must accept DeviceView.device_vk_hex");
        let pinned = doc.lock().await.pinned.clone();
        assert_eq!(pinned.as_deref(), Some(vk_hex.as_str()));

        // ── (d) Feed the doc's pinned value back; exactly the joiner pins ───
        let view2 = build_owner_state_view(&loaded, "this device".into(), pinned);
        for d in &view2.devices {
            assert_eq!(
                d.butler_pinned,
                d.device_id == joiner_identity_hash_hex,
                "butler_pinned must be true for the pinned joiner only (row {})",
                d.device_id
            );
        }
    }

    #[test]
    #[serial]
    fn issue_token_errors_when_owner_state_does_not_exist() {
        clear_token_cache();
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "issue-test-pp");
        let _dir = tempfile::tempdir().unwrap();
        // Note: this test cannot easily call issue_owner_recovery_token directly
        // because that command resolves identity_dir from real OS paths. Instead,
        // we test the underlying invariant: load_owner_state on an empty dir
        // returns Ok(None), and the command errors when None.
        let result = crate::owner_state::load_owner_state(_dir.path(), None);
        assert!(matches!(result, Ok(None)), "empty dir → Ok(None)");
    }
}
