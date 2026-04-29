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
pub fn derive_sas(local_sk: &StaticSecret, peer_pk: &PublicKey) -> SasDerivation {
    let shared = local_sk.diffie_hellman(peer_pk);
    let hk = Hkdf::<Sha256>::new(None, shared.as_bytes());

    let mut session_key = [0u8; 32];
    hk.expand(b"session-v2", &mut session_key)
        .expect("HKDF session-v2 expand cannot fail for 32 bytes");

    let mut sas_bytes = [0u8; 4];
    hk.expand(b"sas-v2", &mut sas_bytes)
        .expect("HKDF sas-v2 expand cannot fail for 4 bytes");

    let sas_int = u32::from_be_bytes(sas_bytes) % 1_000_000;
    let sas_digits = format!("{:06}", sas_int);

    SasDerivation {
        session_key,
        sas_digits,
    }
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

        let from_a = derive_sas(&a_sk, &b_pk);
        let from_b = derive_sas(&b_sk, &a_pk);

        assert_eq!(from_a.session_key, from_b.session_key);
        assert_eq!(from_a.sas_digits, from_b.sas_digits);
    }

    #[test]
    fn sas_is_deterministic() {
        // Same inputs always produce the same outputs.
        let b_pk = PublicKey::from(&StaticSecret::from([42u8; 32]));
        let r1 = derive_sas(&StaticSecret::from([7u8; 32]), &b_pk);
        let r2 = derive_sas(&StaticSecret::from([7u8; 32]), &b_pk);
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

        let a_view = derive_sas(&a_sk, &mitm_pk); // A thinks it's talking to mitm_pk
        let b_view = derive_sas(&b_sk, &mitm_pk); // B same

        // The user looking at both screens sees DIFFERENT 6-digit codes
        // and clicks "no don't match" → MitM detected.
        assert_ne!(a_view.sas_digits, b_view.sas_digits);
    }

    #[test]
    fn sas_digits_format() {
        // Always exactly 6 ASCII digits, even when the int is < 100000.
        let a_sk = StaticSecret::from([0u8; 32]);
        let b_sk = StaticSecret::from([0u8; 32]);
        let b_pk = PublicKey::from(&b_sk);
        let result = derive_sas(&a_sk, &b_pk);
        assert_eq!(result.sas_digits.len(), 6);
        assert!(result.sas_digits.chars().all(|c| c.is_ascii_digit()));
    }
}
