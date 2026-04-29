//! Tauri command surface for the Devices panel.
//!
//! Wraps `crate::owner_state` operations with async + state-injection
//! plumbing. Long-running ops go through `crate::identity_commands::run_blocking`.

use crate::identity::KeychainStore;
use crate::identity_commands::run_blocking;
use crate::owner_state::{
    insert_token, load_owner_state, save_owner_state_atomic, take_token, DeviceView,
    LoadedOwnerState, OwnerStateView, TrustDecisionView, TrustKind,
};
use harmony_owner::lifecycle::{mint_owner, MintResult, RecoveryArtifact};
use harmony_owner::recovery::RecoveryMetadata;
use harmony_owner::trust;
use secrecy::SecretString;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::Manager;
use uuid::Uuid;
use zeroize::Zeroizing;

/// Process-wide mutex for the mint flow. The design spec calls for a
/// single-flight guard: two concurrent `mint_owner_identity` calls (e.g., via
/// rapid double-click reaching the `spawn_blocking` thread pool) must not
/// both pass the file-existence check and race to write competing OwnerStates.
/// Held across the entire check-and-write window.
static MINT_OWNER_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

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

/// Refuse the call if the node event-loop thread is running.
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
            let decision =
                trust::evaluate_trust(&loaded.state, cert.device_id, now, active_window, freshness);
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
        owner_display_name: this_device_name,
        devices,
        can_back_up: loaded.master_seed.is_some(),
    }
}

fn derive_this_device_id(sk: &ed25519_dalek::SigningKey) -> [u8; 16] {
    use harmony_owner::pubkey_bundle::PubKeyBundle;
    PubKeyBundle::classical_only(sk.verifying_key().to_bytes()).identity_hash()
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
pub async fn get_owner_state(_app: tauri::AppHandle) -> Result<Option<OwnerStateView>, String> {
    let identity_dir = resolve_identity_dir()?;
    let display_name = "this device".to_string();
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
    // Fast-fail outside the blocking pool: gives the user a quick error if
    // the node is already running, without waiting on the spawn_blocking
    // queue. The authoritative check happens INSIDE the mint mutex below.
    require_node_stopped(&state)?;
    let identity_dir = resolve_identity_dir()?;
    let display_name = "this device".to_string();
    run_blocking(move || {
        // Hold the process-wide mint mutex for the entire check-and-write
        // window. Without this, concurrent mints could both observe an
        // absent owner_state.cbor and race to write competing OwnerStates.
        // Recover from poisoning so a panic in one handler doesn't brick
        // future mints (mirrors PR-61's preview_cache_lock policy).
        let _mint_guard = MINT_OWNER_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Re-check node status under the mint lock to close the TOCTOU
        // window between the outer fast-fail check and acquiring the lock:
        // another command could have started the node in that gap.
        let node_state = app.state::<Mutex<crate::NodeState>>();
        require_node_stopped(node_state.inner())?;
        // Refuse if already minted (idempotent failure).
        if identity_dir.join("owner_state.cbor").exists() {
            return Err(
                "Owner identity already exists on this device. Wipe via Settings to re-mint."
                    .to_string(),
            );
        }
        let MintResult {
            state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(now_unix()).map_err(|e| format!("mint_owner: {e}"))?;
        let master_seed: Zeroizing<[u8; 32]> = Zeroizing::new(*recovery_artifact.as_bytes());
        save_owner_state_atomic(
            &identity_dir,
            &state,
            &device_signing_key,
            Some(&*master_seed),
            KeychainStore::new().ok(),
        )?;
        let token = insert_token(master_seed.clone());
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
    // Validate passphrase length BEFORE consuming the token.
    // Use Unicode codepoint count (not byte count) so the "characters"
    // error wording matches the check, and so multibyte passphrases
    // (emoji, CJK) round-trip identically with the JS frontend's
    // [...str].length check.
    if passphrase.chars().count() < MIN_RECOVERY_PASSPHRASE_LEN {
        return Err(format!(
            "Recovery passphrase must be at least {MIN_RECOVERY_PASSPHRASE_LEN} characters."
        ));
    }
    // Validate comment length BEFORE consuming the token.
    // 256-BYTE cap matches harmony-owner's hard limit on the underlying
    // field. Frontend mirrors with a TextEncoder byte count before submit.
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
        let seed = take_token(&token).ok_or_else(|| {
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
    use crate::owner_state::clear_token_cache;
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
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-cmd-test-pp");
        let token = insert_token(Zeroizing::new([0xCDu8; 32]));
        // Use a too-short passphrase.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(export_owner_recovery_file_to_path(
            token.to_string(),
            "/tmp/should-not-write".into(),
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
            take_token(&token).is_some(),
            "weak-passphrase rejection must not consume token"
        );
    }

    #[test]
    #[serial]
    fn export_with_invalid_token_errors() {
        clear_token_cache();
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-cmd-test-pp");
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
        assert!(
            err.contains("expired") || err.contains("invalid"),
            "actual: {err}"
        );
    }

    #[test]
    #[serial]
    fn comment_over_cap_rejected() {
        clear_token_cache();
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-cmd-test-pp");
        let token = insert_token(Zeroizing::new([0xEEu8; 32]));
        let comment = "x".repeat(257);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(export_owner_recovery_file_to_path(
            token.to_string(),
            "/tmp/should-not-write".into(),
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
            take_token(&token).is_some(),
            "comment-over-cap rejection must not consume token"
        );
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
