//! ZEB-371 Phase 1b: per-friendship rendezvous secret (ephemeral X25519 ECDH).
//!
//! Both handshake sides exchange a fresh ephemeral X25519 public key and derive
//! an identical 32-byte `friendship_secret` via ECDH → HKDF, bound to the two
//! authenticated owner identities (sorted, so requester/accepter agree). The
//! secret is stored (KeyTree-sealed) in the `FriendEntry` and used later to key
//! a private Case-D pkarr rendezvous slot.

use crate::owner_state_types::OwnerAddr;
use chacha20poly1305::{aead::Aead, ChaCha20Poly1305, KeyInit, Nonce};
use ed25519_dalek::SigningKey;
use harmony_pkarr::{derive_ephemeral_key, PkarrCase};
use hkdf::Hkdf;
use rand::{rngs::OsRng, RngCore};
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

/// Case-D HKDF `info`: `epoch_be ‖ owner_id`. Publishing uses the PUBLISHER's
/// own owner_id (so their friends — who know it — can resolve them); resolving
/// uses the FRIEND's owner_id. Direction-specific so each side is the sole
/// writer of its own DHT slot.
pub(crate) fn case_d_info(epoch: u64, owner_id: &[u8; 16]) -> Vec<u8> {
    let mut info = Vec::with_capacity(8 + 16);
    info.extend_from_slice(&epoch.to_be_bytes());
    info.extend_from_slice(owner_id);
    info
}

/// Slot signing key I PUBLISH under so `self_owner`'s friends can find me.
pub fn case_d_publish_key(secret: &[u8; 32], epoch: u64, self_owner: &[u8; 16]) -> SigningKey {
    derive_ephemeral_key(PkarrCase::Friend, secret, &case_d_info(epoch, self_owner))
}

/// Slot signing key I RESOLVE to find `friend_owner`'s published slot.
pub fn case_d_resolve_key(secret: &[u8; 32], epoch: u64, friend_owner: &[u8; 16]) -> SigningKey {
    derive_ephemeral_key(PkarrCase::Friend, secret, &case_d_info(epoch, friend_owner))
}

/// Sub-key for sealing the Case-D routing payload (HKDF of the friendship secret).
fn case_d_payload_key(secret: &[u8; 32]) -> Zeroizing<[u8; 32]> {
    let hk = Hkdf::<Sha256>::new(Some(b"harmony.friend.v1.case-d-payload"), secret);
    let mut k = Zeroizing::new([0u8; 32]);
    hk.expand(b"", k.as_mut()).expect("HKDF 32 bytes");
    k
}

/// Seal the Case-D routing payload to the friendship secret (defense-in-depth on
/// top of the secret-derived slot). RANDOM 12-byte nonce prepended; epoch in the
/// AAD. Random (not deterministic) because the routing blob may change within an
/// epoch — a deterministic nonce would reuse the keystream and break ChaCha.
/// Layout `nonce(12) ‖ ciphertext+tag`.
pub fn seal_case_d_payload(
    secret: &[u8; 32],
    epoch: u64,
    plaintext: &[u8],
) -> Result<Vec<u8>, String> {
    let key = case_d_payload_key(secret);
    let cipher = ChaCha20Poly1305::new_from_slice(key.as_ref()).expect("32-byte key");
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            chacha20poly1305::aead::Payload {
                msg: plaintext,
                aad: &epoch.to_be_bytes(),
            },
        )
        .map_err(|_| "case-d seal failed".to_string())?;
    let mut out = Vec::with_capacity(12 + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open a payload produced by [`seal_case_d_payload`] for `epoch`.
pub fn open_case_d_payload(
    secret: &[u8; 32],
    epoch: u64,
    sealed: &[u8],
) -> Result<Vec<u8>, String> {
    if sealed.len() < 12 + 16 {
        return Err("case-d payload too short".to_string());
    }
    let key = case_d_payload_key(secret);
    let cipher = ChaCha20Poly1305::new_from_slice(key.as_ref()).expect("32-byte key");
    let (nonce, ct) = sealed.split_at(12);
    cipher
        .decrypt(
            Nonce::from_slice(nonce),
            chacha20poly1305::aead::Payload {
                msg: ct,
                aad: &epoch.to_be_bytes(),
            },
        )
        .map_err(|_| "case-d open failed".to_string())
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

    #[test]
    fn case_d_reference_vector() {
        let secret = [0u8; 32];
        let key = case_d_publish_key(&secret, 12345, &[0x11; 16]);
        let vk_hex = hex::encode(key.verifying_key().to_bytes());
        assert_eq!(
            vk_hex, "cd972d4f9b4dc0b3eb8abac165b18fbdb91ad67d88a34c490e23689c835f49a9",
            "case-d v1 keying must not drift"
        );
    }

    #[test]
    fn case_d_publish_matches_friends_resolve() {
        let secret = [7u8; 32];
        let a = [0xAA; 16];
        assert_eq!(
            case_d_publish_key(&secret, 100, &a).verifying_key(),
            case_d_resolve_key(&secret, 100, &a).verifying_key()
        );
        let b = [0xBB; 16];
        assert_ne!(
            case_d_publish_key(&secret, 100, &a).verifying_key(),
            case_d_publish_key(&secret, 100, &b).verifying_key()
        );
        // epoch separation
        assert_ne!(
            case_d_publish_key(&secret, 100, &a).verifying_key(),
            case_d_publish_key(&secret, 101, &a).verifying_key()
        );
    }

    #[test]
    fn case_d_payload_seal_round_trip() {
        let secret = [3u8; 32];
        let plaintext = b"iroh-reachability-blob";
        let sealed = seal_case_d_payload(&secret, 100, plaintext).expect("seal");
        assert_ne!(&sealed[12..], &plaintext[..], "must be ciphertext");
        let opened = open_case_d_payload(&secret, 100, &sealed).expect("open");
        assert_eq!(opened, plaintext);
        // Wrong epoch (AAD) fails.
        assert!(open_case_d_payload(&secret, 101, &sealed).is_err());
    }

    #[test]
    fn case_d_payload_uses_random_nonce() {
        // Sealing the same plaintext twice in the same epoch must yield different
        // ciphertext (random nonce) — never nonce-reuse under the same key.
        let secret = [4u8; 32];
        let s1 = seal_case_d_payload(&secret, 9, b"same").expect("s1");
        let s2 = seal_case_d_payload(&secret, 9, b"same").expect("s2");
        assert_ne!(s1, s2);
        assert_eq!(open_case_d_payload(&secret, 9, &s1).expect("o1"), b"same");
        assert_eq!(open_case_d_payload(&secret, 9, &s2).expect("o2"), b"same");
    }
}
