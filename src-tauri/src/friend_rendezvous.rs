//! ZEB-371 Phase 1b: per-friendship rendezvous secret (ephemeral X25519 ECDH).
//!
//! Both handshake sides exchange a fresh ephemeral X25519 public key and derive
//! an identical 32-byte `friendship_secret` via ECDH → HKDF, bound to the two
//! authenticated owner identities (sorted, so requester/accepter agree). The
//! secret is stored (KeyTree-sealed) in the `FriendEntry` and used later to key
//! a private Case-D pkarr rendezvous slot.

use crate::owner_state_types::OwnerAddr;
use hkdf::Hkdf;
use rand::rngs::OsRng;
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey};
use zeroize::Zeroizing;

const FRIENDSHIP_SECRET_SALT: &[u8] = b"harmony.friend.v1.rendezvous";

/// Generate a single-use ephemeral X25519 keypair for one handshake. The secret
/// is consumed by [`derive_friendship_secret`]; only the 32-byte public half is
/// sent on the wire.
pub fn generate_ephemeral() -> (EphemeralSecret, [u8; 32]) {
    let sk = EphemeralSecret::random_from_rng(OsRng);
    let pk = PublicKey::from(&sk);
    (sk, pk.to_bytes())
}

/// HKDF `info`: the two owner_ids sorted so both parties compute the same value
/// regardless of who is requester vs accepter.
fn rendezvous_info(a: OwnerAddr, b: OwnerAddr) -> [u8; 32] {
    let (lo, hi) = if a.0 <= b.0 { (a.0, b.0) } else { (b.0, a.0) };
    let mut info = [0u8; 32];
    info[..16].copy_from_slice(&lo);
    info[16..].copy_from_slice(&hi);
    info
}

/// Derive the shared 32-byte friendship secret. `my_eph` is consumed (one-shot).
/// Binds to the two authenticated owner identities via the HKDF `info`.
pub fn derive_friendship_secret(
    my_eph: EphemeralSecret,
    their_eph_pub: &[u8; 32],
    owner_a: OwnerAddr,
    owner_b: OwnerAddr,
) -> Zeroizing<[u8; 32]> {
    let shared = my_eph.diffie_hellman(&PublicKey::from(*their_eph_pub));
    let hk = Hkdf::<Sha256>::new(Some(FRIENDSHIP_SECRET_SALT), shared.as_bytes());
    let mut out = Zeroizing::new([0u8; 32]);
    hk.expand(&rendezvous_info(owner_a, owner_b), out.as_mut())
        .expect("HKDF-SHA256 always produces 32 bytes for a 32-byte output");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::OwnerAddr;

    #[test]
    fn both_sides_derive_identical_secret() {
        let (a_sk, a_pub) = generate_ephemeral();
        let (b_sk, b_pub) = generate_ephemeral();
        let owner_a = OwnerAddr([0x11; 16]);
        let owner_b = OwnerAddr([0x22; 16]);
        // A derives with its sk + B's pub; B with its sk + A's pub. Owners may be
        // passed in either order — they are sorted internally.
        let s_a = derive_friendship_secret(a_sk, &b_pub, owner_a, owner_b);
        let s_b = derive_friendship_secret(b_sk, &a_pub, owner_b, owner_a);
        assert_eq!(s_a.as_ref(), s_b.as_ref());
    }

    #[test]
    fn rendezvous_info_is_owner_order_independent() {
        let x = OwnerAddr([1; 16]);
        let y = OwnerAddr([2; 16]);
        assert_eq!(rendezvous_info(x, y), rendezvous_info(y, x));
        assert_ne!(rendezvous_info(x, y), rendezvous_info(x, x));
    }

    #[test]
    fn distinct_ephemerals_distinct_secret() {
        let owner_a = OwnerAddr([1; 16]);
        let owner_b = OwnerAddr([2; 16]);
        let (a_sk, _) = generate_ephemeral();
        let (_, b_pub) = generate_ephemeral();
        let s_ab = derive_friendship_secret(a_sk, &b_pub, owner_a, owner_b);
        let (a_sk2, _) = generate_ephemeral();
        let (_, c_pub) = generate_ephemeral();
        let s_ac = derive_friendship_secret(a_sk2, &c_pub, owner_a, owner_b);
        assert_ne!(s_ab.as_ref(), s_ac.as_ref());
    }
}
