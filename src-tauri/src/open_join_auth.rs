//! Capability proof for tokenless open-community join.
//!
//! The open invite link's `epoch_key` is the capability. A joiner proves it
//! holds the link by binding its identity + a fresh nonce/timestamp under a key
//! derived from `epoch_key`. A beacon (which also holds `epoch_key`) recomputes
//! and rejects on mismatch — so a party that merely learned a beacon's iroh
//! address cannot join without the link.
//!
//! `epoch_auth = HMAC( HKDF(epoch_key, "open-join-auth"),
//!                     community_id || joiner_identity_pub || nonce || timestamp_be )`

use crate::owner_state_types::{EpochKey, SpaceId};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub const EPOCH_AUTH_INFO: &[u8] = b"open-join-auth";

type HmacSha256 = Hmac<Sha256>;

fn auth_preimage(
    community_id: &SpaceId,
    joiner_identity_pub: &[u8; 64],
    nonce: &[u8; 16],
    timestamp_ms: u64,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(16 + 64 + 16 + 8);
    msg.extend_from_slice(&community_id.0);
    msg.extend_from_slice(joiner_identity_pub);
    msg.extend_from_slice(nonce);
    msg.extend_from_slice(&timestamp_ms.to_be_bytes());
    msg
}

pub fn mint_epoch_auth(
    epoch_key: &EpochKey,
    community_id: &SpaceId,
    joiner_identity_pub: &[u8; 64],
    nonce: &[u8; 16],
    timestamp_ms: u64,
) -> [u8; 32] {
    // HKDF-Extract+Expand a per-purpose MAC key from the epoch key.
    let mut mac_key = zeroize::Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(Some(&community_id.0), epoch_key.as_bytes())
        .expand(EPOCH_AUTH_INFO, mac_key.as_mut())
        .expect("32 <= 8160");
    let mut mac =
        <HmacSha256 as Mac>::new_from_slice(mac_key.as_ref()).expect("HMAC accepts any key length");
    mac.update(&auth_preimage(
        community_id,
        joiner_identity_pub,
        nonce,
        timestamp_ms,
    ));
    mac.finalize().into_bytes().into()
}

pub fn verify_epoch_auth(
    epoch_key: &EpochKey,
    community_id: &SpaceId,
    joiner_identity_pub: &[u8; 64],
    nonce: &[u8; 16],
    timestamp_ms: u64,
    presented: &[u8; 32],
) -> bool {
    let expected = mint_epoch_auth(
        epoch_key,
        community_id,
        joiner_identity_pub,
        nonce,
        timestamp_ms,
    );
    // Constant-time compare (no early-exit on first mismatched byte).
    let mut diff = 0u8;
    for (a, b) in expected.iter().zip(presented.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ek() -> EpochKey {
        EpochKey::new([3u8; 32])
    }
    fn cid() -> SpaceId {
        SpaceId([1u8; 16])
    }

    #[test]
    fn valid_auth_round_trips() {
        let id = [5u8; 64];
        let nonce = [9u8; 16];
        let tag = mint_epoch_auth(&ek(), &cid(), &id, &nonce, 1000);
        assert!(verify_epoch_auth(&ek(), &cid(), &id, &nonce, 1000, &tag));
    }

    #[test]
    fn wrong_epoch_key_is_rejected() {
        let id = [5u8; 64];
        let nonce = [9u8; 16];
        let tag = mint_epoch_auth(&ek(), &cid(), &id, &nonce, 1000);
        let wrong = EpochKey::new([4u8; 32]);
        assert!(!verify_epoch_auth(&wrong, &cid(), &id, &nonce, 1000, &tag));
    }

    #[test]
    fn tampered_fields_are_rejected() {
        let id = [5u8; 64];
        let nonce = [9u8; 16];
        let tag = mint_epoch_auth(&ek(), &cid(), &id, &nonce, 1000);
        // Different timestamp, nonce, identity, or community each break it.
        assert!(!verify_epoch_auth(&ek(), &cid(), &id, &nonce, 1001, &tag));
        assert!(!verify_epoch_auth(
            &ek(),
            &cid(),
            &id,
            &[8u8; 16],
            1000,
            &tag
        ));
        assert!(!verify_epoch_auth(
            &ek(),
            &cid(),
            &[6u8; 64],
            &nonce,
            1000,
            &tag
        ));
        assert!(!verify_epoch_auth(
            &ek(),
            &SpaceId([2u8; 16]),
            &id,
            &nonce,
            1000,
            &tag
        ));
    }
}
