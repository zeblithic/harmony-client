use harmony_owner::certs::EnrollmentCert;
use harmony_owner::lifecycle::RecoveryArtifact;
use harmony_owner::pubkey_bundle::PubKeyBundle;
use harmony_owner::state::OwnerState;
use zeroize::Zeroizing;

/// Sign an EnrollmentCert for the joiner using the master seed (transient).
/// Master signing key is reconstructed from the seed, used immediately, and
/// dropped at end of function (Zeroizing + scope exit).
///
/// Returns the cert; the caller is responsible for adding it to OwnerState
/// via `OwnerState::add_enrollment` and propagating to the Joiner.
pub fn sign_enrollment_for_joiner(
    master_seed: &Zeroizing<[u8; 32]>,
    state: &OwnerState,
    joiner_pubkey: PubKeyBundle,
    now_unix: u64,
) -> Result<EnrollmentCert, String> {
    let artifact = RecoveryArtifact::from_seed(**master_seed);
    let master_pubkey = artifact.master_pubkey_bundle();

    if master_pubkey.identity_hash() != state.owner_id {
        return Err(format!(
            "master/state owner_id mismatch: master={} state={}",
            hex::encode(master_pubkey.identity_hash()),
            hex::encode(state.owner_id),
        ));
    }

    let device_id = joiner_pubkey.identity_hash();
    let master_sk = artifact.master_signing_key();
    let cert = EnrollmentCert::sign_master(
        &master_sk,
        master_pubkey,
        device_id,
        joiner_pubkey,
        now_unix,
        None,
    )
    .map_err(|e| format!("sign_master: {e}"))?;
    drop(master_sk);
    drop(artifact); // defense-in-depth: bound master key material lifetime regardless of RecoveryArtifact's internal Zeroize impl

    Ok(cert)
}

/// Verify a received EnrollmentCert before persisting it on the Joiner side.
/// Checks: cert.owner_id matches expected, cert.device_id matches our pubkey,
/// signature verifies against the embedded master pubkey.
///
/// The signature check is performed by calling `OwnerState::add_enrollment`
/// on a fresh throwaway state. This works because `add_enrollment` validates
/// the cert against the master pubkey embedded in the cert itself — it does
/// not depend on any prior enrollments existing in the state. The probe
/// state is discarded; only the verification result matters.
pub fn verify_cert_for_self(
    cert: &EnrollmentCert,
    expected_owner_id: [u8; 16],
    our_device_pubkey: &PubKeyBundle,
    now_unix: u64,
    active_window_secs: u64,
) -> Result<(), String> {
    if cert.owner_id != expected_owner_id {
        return Err(format!(
            "cert owner_id mismatch: cert={} expected={}",
            hex::encode(cert.owner_id),
            hex::encode(expected_owner_id),
        ));
    }
    let our_device_id = our_device_pubkey.identity_hash();
    if cert.device_id != our_device_id {
        return Err(format!(
            "cert device_id mismatch: cert={} ours={}",
            hex::encode(cert.device_id),
            hex::encode(our_device_id),
        ));
    }
    // Construct a throwaway state with our owner_id and try add_enrollment.
    // Its internal verification checks the master signature against the
    // embedded master pubkey and rejects if invalid.
    let mut probe = OwnerState::new(expected_owner_id);
    probe
        .add_enrollment(cert.clone(), now_unix, active_window_secs)
        .map_err(|e| format!("verify (via add_enrollment probe): {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use harmony_owner::lifecycle::{mint_owner, MintResult};
    use harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS;
    use rand::rngs::OsRng;

    fn fresh_joiner() -> (SigningKey, PubKeyBundle) {
        let sk = SigningKey::generate(&mut OsRng);
        let pubkey = PubKeyBundle::classical_only(sk.verifying_key().to_bytes());
        (sk, pubkey)
    }

    #[test]
    fn sign_and_verify_round_trip() {
        let MintResult {
            state,
            recovery_artifact,
            ..
        } = mint_owner(1_700_000_000).unwrap();
        let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());
        let (_joiner_sk, joiner_pubkey) = fresh_joiner();

        let cert =
            sign_enrollment_for_joiner(&master_seed, &state, joiner_pubkey.clone(), 1_700_000_001)
                .unwrap();

        verify_cert_for_self(
            &cert,
            state.owner_id,
            &joiner_pubkey,
            1_700_000_002,
            DEFAULT_ACTIVE_WINDOW_SECS,
        )
        .unwrap();
    }

    #[test]
    fn verify_rejects_wrong_owner() {
        let MintResult {
            state,
            recovery_artifact,
            ..
        } = mint_owner(1_700_000_000).unwrap();
        let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());
        let (_joiner_sk, joiner_pubkey) = fresh_joiner();

        let cert =
            sign_enrollment_for_joiner(&master_seed, &state, joiner_pubkey.clone(), 1_700_000_001)
                .unwrap();

        let wrong_owner = [0xFFu8; 16];
        let err = verify_cert_for_self(
            &cert,
            wrong_owner,
            &joiner_pubkey,
            1_700_000_002,
            DEFAULT_ACTIVE_WINDOW_SECS,
        )
        .unwrap_err();
        assert!(err.contains("owner_id mismatch"));
    }

    #[test]
    fn verify_rejects_wrong_device() {
        let MintResult {
            state,
            recovery_artifact,
            ..
        } = mint_owner(1_700_000_000).unwrap();
        let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());
        let (_joiner_sk, joiner_pubkey) = fresh_joiner();
        let (_other_sk, other_pubkey) = fresh_joiner();

        let cert =
            sign_enrollment_for_joiner(&master_seed, &state, joiner_pubkey, 1_700_000_001).unwrap();

        let err = verify_cert_for_self(
            &cert,
            state.owner_id,
            &other_pubkey, // pretend we're a different device
            1_700_000_002,
            DEFAULT_ACTIVE_WINDOW_SECS,
        )
        .unwrap_err();
        assert!(err.contains("device_id mismatch"));
    }

    #[test]
    fn verify_rejects_tampered_cert() {
        let MintResult {
            state,
            recovery_artifact,
            ..
        } = mint_owner(1_700_000_000).unwrap();
        let master_seed = Zeroizing::new(*recovery_artifact.as_bytes());
        let (_joiner_sk, joiner_pubkey) = fresh_joiner();

        let mut cert =
            sign_enrollment_for_joiner(&master_seed, &state, joiner_pubkey.clone(), 1_700_000_001)
                .unwrap();

        // Flip a bit in the issued_at to invalidate the signature.
        cert.issued_at = cert.issued_at.wrapping_add(1);

        let err = verify_cert_for_self(
            &cert,
            state.owner_id,
            &joiner_pubkey,
            1_700_000_002,
            DEFAULT_ACTIVE_WINDOW_SECS,
        )
        .unwrap_err();
        assert!(
            err.to_lowercase().contains("verif")
                || err.to_lowercase().contains("signature")
                || err.to_lowercase().contains("invalid")
        );
    }
}
