use crate::pairing::state_machine::JoinerEnrollResult;
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
}
