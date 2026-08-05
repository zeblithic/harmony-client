//! ZEB-827: membership vouch — a beacon's enrolled community device key `D`
//! signs a binding over its published transport identity `T`, so a resolving
//! member can confirm offline that the beacon belongs to a Joined member
//! without any live session or peer-interaction cache. See
//! `docs/superpowers/specs/2026-08-05-zeb-827-beacon-membership-binding-design.md`.

use crate::owner_state_types::SpaceId;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const MEMBERSHIP_VOUCH_V1: u8 = 1;
const MEMBERSHIP_VOUCH_DOMAIN: &[u8] = b"harmony.rendezvous.membership-vouch.v1";

/// Proof, signed by an enrolled community device key, that a rendezvous
/// beacon's transport identity belongs to a Joined member. Rides in the
/// client-controlled rendezvous routing blob (spec §2.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MembershipVouch {
    #[serde(rename = "vn")]
    pub version: u8,
    /// D: the beacon's enrolled Ed25519 device VERIFY key (matches an entry in
    /// some Joined member's `enrolled_device_keys`).
    #[serde(rename = "dv", with = "vouch_bytes::arr32")]
    pub device_vk: [u8; 32],
    #[serde(rename = "ia")]
    pub issued_at_ms: u64,
    #[serde(rename = "vu")]
    pub valid_until_ms: u64,
    /// Ed25519 signature by `device_vk` over the raw-byte preimage (below).
    #[serde(rename = "sg", with = "vouch_bytes::arr64")]
    pub sig: [u8; 64],
}

/// Fixed raw-byte signature preimage (NOT CBOR — determinism without
/// canonicalization concerns): domain ‖ community_id(16) ‖ transport_pub(64)
/// ‖ issued_at_be(8) ‖ valid_until_be(8).
fn vouch_signed_bytes(
    community_id: SpaceId,
    transport_pub: &[u8; 64],
    issued_at_ms: u64,
    valid_until_ms: u64,
) -> Vec<u8> {
    let mut m = Vec::with_capacity(MEMBERSHIP_VOUCH_DOMAIN.len() + 16 + 64 + 16);
    m.extend_from_slice(MEMBERSHIP_VOUCH_DOMAIN);
    m.extend_from_slice(&community_id.0);
    m.extend_from_slice(transport_pub);
    m.extend_from_slice(&issued_at_ms.to_be_bytes());
    m.extend_from_slice(&valid_until_ms.to_be_bytes());
    m
}

pub fn mint_membership_vouch(
    device_sk: &SigningKey,
    community_id: SpaceId,
    transport_pub: &[u8; 64],
    issued_at_ms: u64,
    valid_until_ms: u64,
) -> MembershipVouch {
    let preimage = vouch_signed_bytes(community_id, transport_pub, issued_at_ms, valid_until_ms);
    let sig: Signature = device_sk.sign(&preimage);
    MembershipVouch {
        version: MEMBERSHIP_VOUCH_V1,
        device_vk: device_sk.verifying_key().to_bytes(),
        issued_at_ms,
        valid_until_ms,
        sig: sig.to_bytes(),
    }
}

/// The full strict check (spec §2.3 steps 2–6). `record_transport_pub` is the
/// resolving side's `PkarrRoutingRecord::harmony_identity_pub`; `enrolled_keys`
/// is the union of Joined (non-self) members' effective enrolled device keys.
pub fn verify_membership_vouch(
    vouch: &MembershipVouch,
    community_id: SpaceId,
    record_transport_pub: &[u8; 64],
    enrolled_keys: &HashSet<[u8; 32]>,
    now_ms: u64,
) -> bool {
    if vouch.version != MEMBERSHIP_VOUCH_V1 {
        return false;
    }
    if now_ms < vouch.issued_at_ms || now_ms > vouch.valid_until_ms {
        return false;
    }
    if !enrolled_keys.contains(&vouch.device_vk) {
        return false;
    }
    let Ok(vk) = VerifyingKey::from_bytes(&vouch.device_vk) else {
        return false;
    };
    let preimage = vouch_signed_bytes(
        community_id,
        record_transport_pub,
        vouch.issued_at_ms,
        vouch.valid_until_ms,
    );
    let sig = Signature::from_bytes(&vouch.sig);
    vk.verify_strict(&preimage, &sig).is_ok()
}

/// serde lacks built-in impls for `[u8; N > 32]`; encode the fixed arrays as
/// CBOR byte strings (bstr) via `serialize_bytes`.
mod vouch_bytes {
    pub mod arr32 {
        pub fn serialize<S: serde::Serializer>(b: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
            s.serialize_bytes(b)
        }
        pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
            let v: serde_bytes::ByteBuf = serde::Deserialize::deserialize(d)?;
            v.into_vec()
                .try_into()
                .map_err(|_| serde::de::Error::custom("expected 32 bytes"))
        }
    }
    pub mod arr64 {
        pub fn serialize<S: serde::Serializer>(b: &[u8; 64], s: S) -> Result<S::Ok, S::Error> {
            s.serialize_bytes(b)
        }
        pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<[u8; 64], D::Error> {
            let v: serde_bytes::ByteBuf = serde::Deserialize::deserialize(d)?;
            v.into_vec()
                .try_into()
                .map_err(|_| serde::de::Error::custom("expected 64 bytes"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::SpaceId;
    use ed25519_dalek::SigningKey;
    use std::collections::HashSet;

    fn sk(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }
    fn tpub(seed: u8) -> [u8; 64] {
        [seed; 64]
    }

    #[test]
    fn mint_then_verify_roundtrips() {
        let d = sk(1);
        let dvk = d.verifying_key().to_bytes();
        let cid = SpaceId([7u8; 16]);
        let t = tpub(9);
        let v = mint_membership_vouch(&d, cid, &t, 1_000, 2_000);
        let enrolled: HashSet<[u8; 32]> = [dvk].into_iter().collect();
        assert!(verify_membership_vouch(&v, cid, &t, &enrolled, 1_500));
    }

    #[test]
    fn wrong_version_fails() {
        let d = sk(1);
        let cid = SpaceId([7u8; 16]);
        let t = tpub(9);
        let mut v = mint_membership_vouch(&d, cid, &t, 1_000, 2_000);
        v.version = 2;
        let enrolled: HashSet<[u8; 32]> = [d.verifying_key().to_bytes()].into_iter().collect();
        assert!(!verify_membership_vouch(&v, cid, &t, &enrolled, 1_500));
    }

    #[test]
    fn wrong_community_fails() {
        let d = sk(1);
        let t = tpub(9);
        let v = mint_membership_vouch(&d, SpaceId([7u8; 16]), &t, 1_000, 2_000);
        let enrolled: HashSet<[u8; 32]> = [d.verifying_key().to_bytes()].into_iter().collect();
        assert!(!verify_membership_vouch(
            &v,
            SpaceId([8u8; 16]),
            &t,
            &enrolled,
            1_500
        ));
    }

    #[test]
    fn wrong_transport_pub_fails() {
        let d = sk(1);
        let cid = SpaceId([7u8; 16]);
        let v = mint_membership_vouch(&d, cid, &tpub(9), 1_000, 2_000);
        let enrolled: HashSet<[u8; 32]> = [d.verifying_key().to_bytes()].into_iter().collect();
        // resolver passes the RECORD's transport pub, which differs from the signed one.
        assert!(!verify_membership_vouch(
            &v,
            cid,
            &tpub(10),
            &enrolled,
            1_500
        ));
    }

    #[test]
    fn stale_or_early_window_fails() {
        let d = sk(1);
        let cid = SpaceId([7u8; 16]);
        let t = tpub(9);
        let v = mint_membership_vouch(&d, cid, &t, 1_000, 2_000);
        let enrolled: HashSet<[u8; 32]> = [d.verifying_key().to_bytes()].into_iter().collect();
        assert!(!verify_membership_vouch(&v, cid, &t, &enrolled, 999)); // before issued_at
        assert!(!verify_membership_vouch(&v, cid, &t, &enrolled, 2_001)); // after valid_until
    }

    #[test]
    fn device_not_enrolled_fails() {
        let d = sk(1);
        let cid = SpaceId([7u8; 16]);
        let t = tpub(9);
        let v = mint_membership_vouch(&d, cid, &t, 1_000, 2_000);
        let enrolled: HashSet<[u8; 32]> = [sk(2).verifying_key().to_bytes()].into_iter().collect();
        assert!(!verify_membership_vouch(&v, cid, &t, &enrolled, 1_500));
    }

    #[test]
    fn tampered_sig_fails() {
        let d = sk(1);
        let cid = SpaceId([7u8; 16]);
        let t = tpub(9);
        let mut v = mint_membership_vouch(&d, cid, &t, 1_000, 2_000);
        v.sig[0] ^= 0xFF;
        let enrolled: HashSet<[u8; 32]> = [d.verifying_key().to_bytes()].into_iter().collect();
        assert!(!verify_membership_vouch(&v, cid, &t, &enrolled, 1_500));
    }

    #[test]
    fn substituted_device_key_fails() {
        // A valid vouch by D1, but device_vk swapped to an enrolled D2: the sig
        // no longer verifies under D2, so it must fail (can't borrow another
        // member's enrollment).
        let d1 = sk(1);
        let d2 = sk(2);
        let cid = SpaceId([7u8; 16]);
        let t = tpub(9);
        let mut v = mint_membership_vouch(&d1, cid, &t, 1_000, 2_000);
        v.device_vk = d2.verifying_key().to_bytes();
        let enrolled: HashSet<[u8; 32]> = [d2.verifying_key().to_bytes()].into_iter().collect();
        assert!(!verify_membership_vouch(&v, cid, &t, &enrolled, 1_500));
    }

    #[test]
    fn cbor_roundtrips() {
        let v = mint_membership_vouch(&sk(1), SpaceId([7u8; 16]), &tpub(9), 1_000, 2_000);
        let mut buf = Vec::new();
        ciborium::into_writer(&v, &mut buf).unwrap();
        let back: MembershipVouch = ciborium::from_reader(&buf[..]).unwrap();
        assert_eq!(v, back);
    }
}
