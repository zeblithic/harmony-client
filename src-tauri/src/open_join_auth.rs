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
//!
//! The generic HKDF -> HMAC -> constant-time-verify kernel lives in
//! `harmony_crypto::capability` (ZEB-736); this module supplies only the
//! open-join-specific preimage layout and domain label.

use crate::owner_state_types::{EpochKey, SpaceId};
use harmony_crypto::capability::{capability_tag, verify_capability_tag};

pub const EPOCH_AUTH_INFO: &[u8] = b"open-join-auth";

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
    capability_tag(
        epoch_key.as_bytes(),
        Some(&community_id.0),
        EPOCH_AUTH_INFO,
        &auth_preimage(community_id, joiner_identity_pub, nonce, timestamp_ms),
    )
}

pub fn verify_epoch_auth(
    epoch_key: &EpochKey,
    community_id: &SpaceId,
    joiner_identity_pub: &[u8; 64],
    nonce: &[u8; 16],
    timestamp_ms: u64,
    presented: &[u8; 32],
) -> bool {
    verify_capability_tag(
        epoch_key.as_bytes(),
        Some(&community_id.0),
        EPOCH_AUTH_INFO,
        &auth_preimage(community_id, joiner_identity_pub, nonce, timestamp_ms),
        presented,
    )
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

    #[test]
    fn mint_matches_golden_vector() {
        // Byte-preservation anchor (ZEB-736): the epoch-auth tag is a frozen
        // wire field, so this exact value must survive extraction of the
        // HKDF->HMAC->constant-time-verify kernel into harmony-crypto. This is
        // the same tag asserted by
        // harmony-crypto `capability::tests::golden_vector_pins_the_construction`.
        let tag = mint_epoch_auth(&ek(), &cid(), &[5u8; 64], &[9u8; 16], 1000);
        assert_eq!(
            tag,
            [
                0xd1, 0x7d, 0x12, 0xde, 0x45, 0x61, 0x7c, 0x28, 0x20, 0x87, 0xe4, 0x1e, 0x36, 0x78,
                0x50, 0x5d, 0x6b, 0xbb, 0x11, 0xf0, 0xfa, 0x9d, 0xef, 0xd5, 0x5f, 0x1c, 0xfb, 0xc2,
                0x0a, 0x2c, 0x07, 0xed,
            ]
        );
    }
}
