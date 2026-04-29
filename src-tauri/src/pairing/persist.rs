use crate::pairing::state_machine::{InviterEnrollResult, JoinerEnrollResult};
use std::path::Path;

/// Persist the Joiner's signing key + OwnerState to disk. Mirrors the
/// atomicity contract from ZEB-170: keychain first, .cbor last.
///
/// On failure mid-write, the keychain entry may remain orphaned; subsequent
/// `load_owner_state` will treat the absence of `.cbor` as un-bound and
/// re-pairing will overwrite the keychain entry.
///
/// The Joiner has NO master_seed (cert-only model — see ZEB-197 design).
/// Passing `master_seed: None` to save_owner_state_atomic ensures the
/// master_seed.enc file is never written, so load_owner_state correctly
/// reports `canBackUp: false` for this device.
pub fn install_joiner_state(identity_dir: &Path, result: JoinerEnrollResult) -> Result<(), String> {
    crate::owner_state::save_owner_state_atomic(
        identity_dir,
        &result.owner_state,
        &result.our_signing_key,
        None, // no master_seed on Joiner
        None, // no KeychainStore — fall back to encrypted-file via HARMONY_PASSPHRASE
    )?;
    Ok(())
}

/// Persist the Inviter's freshly-mutated OwnerState back to disk after a
/// successful enrollment. Unlike the Joiner path, the Inviter already has
/// its own signing key on disk (from the original mint); we reload it via
/// `load_owner_state` and rewrite the state file atomically alongside the
/// existing key. The master seed is preserved (Inviter HAS the master in
/// the cert-only model — only the Joiner is restricted).
///
/// Errors if no existing owner state is on disk: an Inviter that never
/// minted should never have reached `Complete`, so this is an inconsistent
/// state that surfaces as a hard failure.
pub fn install_inviter_state(
    identity_dir: &Path,
    result: InviterEnrollResult,
) -> Result<(), String> {
    // Reload the existing signing key (the Inviter already has its keys on disk).
    // No KeychainStore — fall back to encrypted-file via HARMONY_PASSPHRASE,
    // matching the Joiner path's discipline.
    let loaded = crate::owner_state::load_owner_state(identity_dir, None)?
        .ok_or_else(|| "no existing owner state to update".to_string())?;
    crate::owner_state::save_owner_state_atomic(
        identity_dir,
        &result.owner_state,        // the FRESH state with the new enrollment
        &loaded.device_signing_key, // the EXISTING signing key
        Some(&*result.master_seed), // Inviter keeps master
        None,                       // no KeychainStore — fall back to encrypted-file
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::state_machine::JoinerEnrollResult;
    use ed25519_dalek::SigningKey;
    use harmony_owner::lifecycle::{mint_owner, MintResult};
    use harmony_owner::pubkey_bundle::PubKeyBundle;
    use rand::rngs::OsRng;
    use serial_test::serial;
    use tempfile::tempdir;

    /// Reuse the EnvVarGuard pattern (and replace once ZEB-193 lands).
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

    #[tokio::test]
    #[serial]
    async fn install_writes_owner_state_cbor() {
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "test-pp");
        let dir = tempdir().unwrap();

        let MintResult { state, .. } = mint_owner(1_700_000_000).unwrap();
        let joiner_sk = SigningKey::generate(&mut OsRng);
        let joiner_pubkey = PubKeyBundle::classical_only(joiner_sk.verifying_key().to_bytes());
        let joiner_id = joiner_pubkey.identity_hash();

        let result = JoinerEnrollResult {
            our_signing_key: joiner_sk,
            owner_state: state,
            our_device_id: joiner_id,
        };
        install_joiner_state(dir.path(), result).unwrap();

        let cbor_path = dir.path().join("owner_state.cbor");
        assert!(cbor_path.exists(), "OwnerState cbor written");

        // The Joiner's master_seed.enc must NOT exist (cert-only model).
        let master_path = dir.path().join("master_seed.enc");
        assert!(
            !master_path.exists(),
            "master_seed must not exist on Joiner"
        );

        // The Joiner's device_sk.enc MUST exist (signing key persisted).
        let device_path = dir.path().join("device_sk.enc");
        assert!(device_path.exists(), "device_sk.enc written");
    }

    /// Inviter-side persistence: simulates the Inviter having already minted
    /// (so device_sk.enc + master_seed.enc + owner_state.cbor exist on disk),
    /// then a pairing handshake completes and produces an
    /// `InviterEnrollResult` carrying a mutated `OwnerState`. After
    /// `install_inviter_state`, the on-disk `.cbor` MUST reflect the new
    /// enrollment, and the existing signing key + master seed MUST remain in
    /// place.
    #[tokio::test]
    #[serial]
    async fn install_inviter_writes_updated_owner_state() {
        use crate::owner_state::{load_owner_state, save_owner_state_atomic};
        use harmony_owner::certs::EnrollmentCert;
        use harmony_owner::pubkey_bundle::PubKeyBundle;
        use harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS;
        use zeroize::Zeroizing;

        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "test-pp");
        let dir = tempdir().unwrap();

        // Simulate a freshly-minted Inviter: original state, signing key,
        // and master seed all on disk.
        let MintResult {
            state: original_state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(1_700_000_000).unwrap();
        let master_seed_bytes: [u8; 32] = *recovery_artifact.as_bytes();
        save_owner_state_atomic(
            dir.path(),
            &original_state,
            &device_signing_key,
            Some(&master_seed_bytes),
            None,
        )
        .unwrap();
        let original_signing_bytes = device_signing_key.to_bytes();

        // Now produce a mutated OwnerState that adds a fresh peer enrollment
        // (mirroring what the SM does after signing the cert for a Joiner).
        let mut mutated_state = original_state.clone();
        let joiner_sk = SigningKey::generate(&mut OsRng);
        let joiner_pubkey = PubKeyBundle::classical_only(joiner_sk.verifying_key().to_bytes());
        let now = 1_700_000_001;
        // Reuse the existing signing infrastructure: build a cert signed by
        // the original device for the new joiner pubkey.
        let master_seed_for_sign = Zeroizing::new(master_seed_bytes);
        let cert: EnrollmentCert = crate::pairing::cert::sign_enrollment_for_joiner(
            &master_seed_for_sign,
            &mutated_state,
            joiner_pubkey.clone(),
            now,
        )
        .unwrap();
        mutated_state
            .add_enrollment(cert, now, DEFAULT_ACTIVE_WINDOW_SECS)
            .unwrap();
        let new_device_id = joiner_pubkey.identity_hash();
        assert!(
            mutated_state.enrollments.contains_key(&new_device_id),
            "in-memory state has new enrollment before persist"
        );

        let result = InviterEnrollResult {
            owner_state: mutated_state,
            master_seed: Zeroizing::new(master_seed_bytes),
        };
        install_inviter_state(dir.path(), result).unwrap();

        // Reload from disk and assert the new enrollment is present.
        let reloaded = load_owner_state(dir.path(), None)
            .unwrap()
            .expect("loaded state");
        assert!(
            reloaded.state.enrollments.contains_key(&new_device_id),
            "persisted state contains the new enrollment"
        );
        // Signing key on disk MUST be unchanged.
        assert_eq!(
            reloaded.device_signing_key.to_bytes(),
            original_signing_bytes,
            "Inviter's signing key preserved across persist"
        );
        // Master seed on disk MUST be preserved (Inviter keeps master).
        let reloaded_seed = reloaded.master_seed.expect("master seed preserved");
        assert_eq!(
            *reloaded_seed, master_seed_bytes,
            "Inviter's master seed preserved across persist"
        );
    }

    /// Calling `install_inviter_state` against a directory with no existing
    /// owner state must fail rather than silently create one — an Inviter
    /// that never minted should never reach `Complete`.
    #[tokio::test]
    #[serial]
    async fn install_inviter_errors_when_no_existing_state() {
        use zeroize::Zeroizing;

        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "test-pp");
        let dir = tempdir().unwrap();

        let MintResult { state, .. } = mint_owner(1_700_000_000).unwrap();
        let result = InviterEnrollResult {
            owner_state: state,
            master_seed: Zeroizing::new([0u8; 32]),
        };
        let err = install_inviter_state(dir.path(), result).expect_err("must error");
        assert!(
            err.contains("no existing owner state"),
            "expected 'no existing owner state' error, got: {err}"
        );
    }
}
