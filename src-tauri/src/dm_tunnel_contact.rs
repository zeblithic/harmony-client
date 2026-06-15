//! ZEB-461: this node's own contact material for the friend handshake.
//!
//! The friend handshake (request + accept) advertises the local node's device
//! bundle so a friend can open a cross-WAN DM tunnel to us without a separate
//! discovery-cache round-trip. Task 5 added the wire fields + signature binding
//! (the device bundle is hashed into `devices_digest`, which the handshake sig
//! covers). This module derives the *self* bundle that Task 6 places on the wire.

use crate::owner_state_types::DeviceIdentityHash;

/// The local node's own single-device bundle for the friend handshake.
///
/// Returns the parallel `(devices, identity_pubs)` vecs that go into the
/// `FriendLinkRequest` / `FriendLinkAccepted` (and into `friend_devices_digest`,
/// which the handshake signature binds). `identity_pub_64` is the canonical
/// `X25519_pub(32) || Ed25519_pub(32)` combined identity public-bytes value.
///
/// (Multi-device enumeration is a future refinement; alpha nodes are
/// single-device — they advertise exactly their own identity.)
pub fn self_device_bundle(
    identity_pub_64: [u8; 64],
) -> (Vec<DeviceIdentityHash>, Vec<Option<[u8; 64]>>) {
    // Our own identity pub is minted in-process and is normally always valid, so
    // a `None` here means our identity material is corrupt (e.g. truncated /
    // bit-flipped on disk). Degrade gracefully — advertise an EMPTY bundle (the
    // receive side treats that as "no devices advertised" and skips it) rather
    // than `.expect()`-panicking the whole node. Loud `error!` so the corruption
    // is still visible.
    match crate::dm_signing::derive_device_hash_from_identity_pub(&identity_pub_64) {
        Some(hash) => (vec![hash], vec![Some(identity_pub_64)]),
        None => {
            tracing::error!(
                "self identity pub did not derive a device hash (corrupt identity?); \
                 advertising an empty device bundle"
            );
            (vec![], vec![])
        }
    }
}

/// The self-side values a `FriendLinkRequest` carries (ZEB-461): the SIGNED
/// device bundle (`sender_devices` + `device_identity_pubs`, bound into the
/// handshake signature via `friend_devices_digest`) plus the UNSIGNED iroh
/// reachability + PQ routing hints.
pub struct SelfRequestBundle {
    pub sender_devices: Vec<DeviceIdentityHash>,
    pub device_identity_pubs: Vec<Option<[u8; 64]>>,
    pub iroh_node_id: [u8; 32],
    pub home_relay_url: Option<String>,
    pub pq_dsa_pubkey: Vec<u8>,
    pub pq_kem_pubkey: Vec<u8>,
}

/// Build the self-side request bundle from this node's handshake reachability,
/// in ONE place so the two request-build sites (`connectivity_redeem_invite_*`
/// token-redeem and `connectivity_add_friend_by_key_inner`) can't drift.
///
/// With `Some`, fills the real device bundle (derived from `identity_pub_64`)
/// together with the reachability and PQ keys. With `None` (tests / pre-identity),
/// yields the EMPTY bundle and zero/`None` hints — byte-identical to the per-site
/// `match` blocks it replaces. The caller still computes the signed digest from
/// `sender_devices`/`device_identity_pubs`, so the wire signature is unchanged.
pub fn self_request_bundle(
    self_reachability: Option<&crate::iroh_friend_acceptor::SelfHandshakeReachability>,
) -> SelfRequestBundle {
    match self_reachability {
        Some(r) => {
            let (sender_devices, device_identity_pubs) = self_device_bundle(r.identity_pub_64);
            SelfRequestBundle {
                sender_devices,
                device_identity_pubs,
                iroh_node_id: r.iroh_node_id,
                home_relay_url: r.home_relay_url.clone(),
                pq_dsa_pubkey: r.pq_dsa_pubkey.clone(),
                pq_kem_pubkey: r.pq_kem_pubkey.clone(),
            }
        }
        None => SelfRequestBundle {
            sender_devices: vec![],
            device_identity_pubs: vec![],
            iroh_node_id: [0u8; 32],
            home_relay_url: None,
            pq_dsa_pubkey: vec![],
            pq_kem_pubkey: vec![],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_device_bundle_is_single_device_with_matching_pub() {
        // A valid identity public-bytes value (so harmony_identity can parse it
        // and derive a device hash). Same construction the dm_signing tests use.
        let private = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let pub64 = private.public_identity().to_public_bytes();
        let (devices, pubs) = self_device_bundle(pub64);
        assert_eq!(devices.len(), 1);
        assert_eq!(pubs, vec![Some(pub64)]);
        let expected = crate::dm_signing::derive_device_hash_from_identity_pub(&pub64).unwrap();
        assert_eq!(devices[0], expected);
    }

    #[test]
    fn self_device_bundle_degrades_to_empty_on_unparseable_pub() {
        // CodeAnt: a corrupt self identity must NOT abort the process. `from_bytes`
        // rejects an Ed25519 half that doesn't decompress to a curve point (~half
        // of arbitrary encodings), so a corrupt-on-disk pub can reach the `None`
        // branch. Search for such a value (varying the Ed25519 half), then assert
        // the bundle degrades to EMPTY rather than panicking.
        let bad = (0u32..2000)
            .map(|i| {
                let mut p = [0u8; 64];
                p[32..36].copy_from_slice(&i.to_le_bytes());
                p
            })
            .find(|p| crate::dm_signing::derive_device_hash_from_identity_pub(p).is_none())
            .expect("an off-curve Ed25519 half must exist in the search space");
        let (devices, pubs) = self_device_bundle(bad);
        assert!(devices.is_empty());
        assert!(pubs.is_empty());
    }

    #[test]
    fn self_request_bundle_matches_per_field_extraction() {
        let private = harmony_identity::PrivateIdentity::from_seed(&[0x42; 32]);
        let pub64 = private.public_identity().to_public_bytes();
        let reach = crate::iroh_friend_acceptor::SelfHandshakeReachability {
            identity_pub_64: pub64,
            iroh_node_id: [7u8; 32],
            home_relay_url: Some("https://relay.example".to_string()),
            pq_dsa_pubkey: vec![1, 2, 3],
            pq_kem_pubkey: vec![4, 5, 6],
        };
        // `Some` mirrors the device bundle + reachability the old match blocks built.
        let b = self_request_bundle(Some(&reach));
        let (devices, pubs) = self_device_bundle(pub64);
        assert_eq!(b.sender_devices, devices);
        assert_eq!(b.device_identity_pubs, pubs);
        assert_eq!(b.iroh_node_id, [7u8; 32]);
        assert_eq!(b.home_relay_url.as_deref(), Some("https://relay.example"));
        assert_eq!(b.pq_dsa_pubkey, vec![1, 2, 3]);
        assert_eq!(b.pq_kem_pubkey, vec![4, 5, 6]);

        // `None` is the pre-identity / test path: EMPTY bundle + zero/None hints.
        let n = self_request_bundle(None);
        assert!(n.sender_devices.is_empty());
        assert!(n.device_identity_pubs.is_empty());
        assert_eq!(n.iroh_node_id, [0u8; 32]);
        assert!(n.home_relay_url.is_none());
        assert!(n.pq_dsa_pubkey.is_empty());
        assert!(n.pq_kem_pubkey.is_empty());
    }
}
