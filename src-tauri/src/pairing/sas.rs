use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

#[derive(Debug, Clone)]
pub struct SasDerivation {
    pub session_key: [u8; 32],
    pub sas_digits: String, // exactly 6 ASCII digits
}

/// Derive the session_key and 6-digit SAS from a local ephemeral X25519
/// secret + the peer's ephemeral X25519 public key.
///
/// Both sides MUST pass the same role-symmetric inputs (i.e., the function
/// is symmetric: `derive(a_sk, b_pk) == derive(b_sk, a_pk)`).
///
/// Returns `Err` if the peer pubkey is a low-order point (the resulting
/// shared secret would be all-zero, which both sides would derive the same
/// publicly-known session_key from — letting an active attacker decrypt
/// CONFIRM/ENROLL traffic and forge an ENROLL the Joiner installs). The
/// caller MUST surface this as a hard pairing failure (Failed state) and
/// not fall back to a derived key.
pub fn derive_sas(local_sk: &StaticSecret, peer_pk: &PublicKey) -> Result<SasDerivation, String> {
    let shared = local_sk.diffie_hellman(peer_pk);
    // Constant-time check that the peer's pubkey is non-low-order. If a peer
    // sends a low-order point (e.g. order 1, 2, 4, 8) the shared secret is in
    // a small public set; HKDF over a public input gives a public session key
    // — a complete handshake compromise. `was_contributory` is the
    // x25519-dalek 2.x API for this check (see crate docs).
    if !shared.was_contributory() {
        return Err("peer X25519 pubkey is low-order; refusing to derive session key".to_string());
    }
    // PR #63 review: use a protocol-specific HKDF salt for domain binding.
    // The default zero-salt is technically fine here (the IKM is an
    // ephemeral, never-reused ECDH shared secret), but tying the derived
    // keys to "this is harmony-pairing v2 over LAN" makes a future cross-
    // protocol confusion (e.g., a hypothetical v3 LAN protocol or non-LAN
    // transport reusing the same X25519 pair) inert by construction.
    let hk = Hkdf::<Sha256>::new(Some(b"harmony-pairing-v2-lan"), shared.as_bytes());

    let mut session_key = [0u8; 32];
    hk.expand(b"session-v2", &mut session_key)
        .expect("HKDF session-v2 expand cannot fail for 32 bytes");

    let mut sas_bytes = [0u8; 4];
    hk.expand(b"sas-v2", &mut sas_bytes)
        .expect("HKDF sas-v2 expand cannot fail for 4 bytes");

    let sas_int = u32::from_be_bytes(sas_bytes) % 1_000_000;
    let sas_digits = format!("{:06}", sas_int);

    Ok(SasDerivation {
        session_key,
        sas_digits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    #[test]
    fn sas_is_symmetric() {
        let a_sk = StaticSecret::random_from_rng(OsRng);
        let b_sk = StaticSecret::random_from_rng(OsRng);
        let a_pk = PublicKey::from(&a_sk);
        let b_pk = PublicKey::from(&b_sk);

        let from_a = derive_sas(&a_sk, &b_pk).unwrap();
        let from_b = derive_sas(&b_sk, &a_pk).unwrap();

        assert_eq!(from_a.session_key, from_b.session_key);
        assert_eq!(from_a.sas_digits, from_b.sas_digits);
    }

    #[test]
    fn sas_is_deterministic() {
        // Same inputs always produce the same outputs.
        let b_pk = PublicKey::from(&StaticSecret::from([42u8; 32]));
        let r1 = derive_sas(&StaticSecret::from([7u8; 32]), &b_pk).unwrap();
        let r2 = derive_sas(&StaticSecret::from([7u8; 32]), &b_pk).unwrap();
        assert_eq!(r1.session_key, r2.session_key);
        assert_eq!(r1.sas_digits, r2.sas_digits);
    }

    #[test]
    fn sas_differs_under_mitm() {
        // Simulate a MitM doing two separate ECDH exchanges.
        let a_sk = StaticSecret::random_from_rng(OsRng);
        let b_sk = StaticSecret::random_from_rng(OsRng);
        let mitm_sk = StaticSecret::random_from_rng(OsRng);
        let mitm_pk = PublicKey::from(&mitm_sk);

        let a_view = derive_sas(&a_sk, &mitm_pk).unwrap(); // A thinks it's talking to mitm_pk
        let b_view = derive_sas(&b_sk, &mitm_pk).unwrap(); // B same

        // The user looking at both screens sees DIFFERENT 6-digit codes
        // and clicks "no don't match" → MitM detected.
        assert_ne!(a_view.sas_digits, b_view.sas_digits);
    }

    #[test]
    fn sas_digits_format() {
        // Always exactly 6 ASCII digits, even when the int is < 100000.
        // Use a real (non-low-order) keypair on both sides so we don't trip
        // the contributory-check Err path.
        let a_sk = StaticSecret::from([1u8; 32]);
        let b_sk = StaticSecret::from([2u8; 32]);
        let b_pk = PublicKey::from(&b_sk);
        let result = derive_sas(&a_sk, &b_pk).unwrap();
        assert_eq!(result.sas_digits.len(), 6);
        assert!(result.sas_digits.chars().all(|c| c.is_ascii_digit()));
    }

    /// Security regression (PR #63): a peer that sends a low-order X25519
    /// pubkey (here, the all-zero point of order 1) produces a non-
    /// contributory shared secret. Without rejection, both sides would
    /// derive the same publicly-known session_key — a complete handshake
    /// compromise. `derive_sas` MUST return Err.
    #[test]
    fn sas_rejects_low_order_pubkey() {
        let a_sk = StaticSecret::random_from_rng(OsRng);
        // The all-zero point is order 1 → low-order. Other low-order points
        // exist (RFC 7748 lists 7); the all-zero one is the simplest to
        // construct and is sufficient to exercise the rejection path.
        let low_order_pk = PublicKey::from([0u8; 32]);
        let err = derive_sas(&a_sk, &low_order_pk).expect_err("must reject low-order");
        assert!(
            err.contains("low-order"),
            "expected low-order rejection, got: {err}"
        );
    }
}
