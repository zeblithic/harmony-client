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
use zeroize::Zeroizing;

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
fn mint_save_export_decrypt_roundtrip() {
    let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "owner-integration-pp");
    let dir = tempdir().unwrap();

    // 1. Mint
    let MintResult {
        state,
        recovery_artifact,
        device_signing_key,
    } = mint_owner(1_700_000_000).expect("mint");
    let master_seed = *recovery_artifact.as_bytes();

    // 2. Persist
    save_owner_state_atomic(
        dir.path(),
        &state,
        &device_signing_key,
        Some(&master_seed),
        None,
    )
    .expect("save");

    // 3. Reload — confirm load round-trip
    let loaded = load_owner_state(dir.path(), None)
        .expect("load")
        .expect("Some");
    assert_eq!(loaded.state.owner_id, state.owner_id);

    // 4. Export via token cache
    let token = insert_token(Zeroizing::new(master_seed));
    let recovered = take_token(&token).expect("token must redeem once");
    assert_eq!(*recovered, master_seed);

    let secret = SecretString::from("integration-test-recovery-pp".to_string());
    let artifact_for_export = RecoveryArtifact::from_seed(*recovered);
    let metadata = RecoveryMetadata {
        mint_at: None,
        comment: Some("integration".into()),
    };
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
    assert_eq!(
        *artifact.as_bytes(),
        master_seed,
        "round-trip must yield identical master seed"
    );
}
