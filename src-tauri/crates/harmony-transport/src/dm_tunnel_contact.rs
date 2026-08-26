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
/// `FriendLinkRequest` / `FriendLinkAccepted` (and into `contact_digest`,
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

/// The self-side values a `FriendLinkRequest` carries (ZEB-461/473): the device
/// bundle (`sender_devices` + `device_identity_pubs`) plus the iroh reachability +
/// PQ keys. ZEB-473 §6.3: ALL of these are now SIGNED — every field is folded into
/// `contact_digest`, which the handshake signature binds.
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

/// ZEB-473 Task 5: build a peer's [`DeviceTunnelContact`] from the reachability +
/// PQ keys it advertised over the friend handshake, or `None` when it advertised
/// nothing dialable.
///
/// A contact is only meaningful — and only worth persisting — when the peer gave
/// us a non-zero iroh `node_id` (the dial target) AND both PQ public keys: the
/// ML-DSA key (to authenticate the PQ tunnel handshake) and the ML-KEM key (for
/// the ML-KEM key agreement that establishes the tunnel). A peer that omits any
/// of the three cannot be tunnel-dialed, so we return `None` rather than
/// fabricating a half-populated contact that would always fail the handshake.
/// Returning `None` (vs a partial contact) is what lets the CRDT skip-on-empty
/// rule fire: a contactless/older peer never LWW-clobbers a previously-known-good
/// contact for that device.
///
/// These fields are now SIGNED into the handshake (the `contact_digest` preimage,
/// §6.3), so a value reaching here has already passed signature verification —
/// this helper only decides "present enough to dial?", never trusts an unsigned
/// hint.
pub fn peer_handshake_contact(
    iroh_node_id: [u8; 32],
    home_relay_url: Option<String>,
    pq_dsa_pubkey: Vec<u8>,
    pq_kem_pubkey: Vec<u8>,
) -> Option<crate::owner_state_types::DeviceTunnelContact> {
    // A zero node id is a legitimately-absent (undialable) contact → None.
    if iroh_node_id == [0u8; 32] {
        return None;
    }
    let contact = crate::owner_state_types::DeviceTunnelContact {
        iroh_node_id,
        home_relay_url,
        pq_dsa_pubkey,
        pq_kem_pubkey,
    };
    // Reject malformed/oversized signed inputs (exact PQ key sizes + bounded
    // relay URL) BEFORE they can enter replicated CRDT state, where they would
    // repeatedly fail dial attempts and bloat owner-state payloads. ZEB-473.
    if !contact.has_valid_key_sizes() {
        return None;
    }
    Some(contact)
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

    /// CodeAnt/Qodo F3 + CR5 (ZEB-473): a contact is only dialable with a
    /// non-zero node id AND both PQ keys at their EXACT canonical sizes (ML-DSA
    /// for handshake auth, ML-KEM for key agreement). Any missing/wrong-size
    /// field → `None` so an undialable/malformed contact never LWW-clobbers a
    /// previously-known-good one nor bloats replicated owner-state.
    #[test]
    fn peer_handshake_contact_requires_node_id_and_both_pq_keys() {
        use crate::owner_state_types::{ML_DSA_65_PUBKEY_LEN, ML_KEM_768_PUBKEY_LEN};
        let node = [7u8; 32];
        let dsa = vec![1u8; ML_DSA_65_PUBKEY_LEN];
        let kem = vec![2u8; ML_KEM_768_PUBKEY_LEN];

        // Happy path: all three present + correctly sized → Some.
        let c = peer_handshake_contact(node, None, dsa.clone(), kem.clone())
            .expect("a fully-populated, correctly-sized contact must be Some");
        assert_eq!(c.iroh_node_id, node);
        assert_eq!(c.pq_dsa_pubkey, dsa);
        assert_eq!(c.pq_kem_pubkey, kem);

        // Zero node id → None.
        assert!(peer_handshake_contact([0u8; 32], None, dsa.clone(), kem.clone()).is_none());
        // Empty DSA → None.
        assert!(peer_handshake_contact(node, None, vec![], kem.clone()).is_none());
        // Empty KEM → None (the F3 regression: was previously Some).
        assert!(
            peer_handshake_contact(node, None, dsa.clone(), vec![]).is_none(),
            "an empty ML-KEM key must skip the contact (undialable)"
        );
    }

    /// CR5 (ZEB-473): wrong-but-non-empty PQ key sizes and an oversized relay
    /// URL must all be rejected (`None`), so only canonically-sized contacts
    /// reach CRDT state.
    #[test]
    fn peer_handshake_contact_rejects_wrong_key_sizes_and_long_relay() {
        use crate::owner_state_types::{
            MAX_TUNNEL_RELAY_URL_LEN, ML_DSA_65_PUBKEY_LEN, ML_KEM_768_PUBKEY_LEN,
        };
        let node = [7u8; 32];
        let dsa = vec![1u8; ML_DSA_65_PUBKEY_LEN];
        let kem = vec![2u8; ML_KEM_768_PUBKEY_LEN];

        // DSA one byte short → None.
        assert!(
            peer_handshake_contact(node, None, vec![1u8; ML_DSA_65_PUBKEY_LEN - 1], kem.clone())
                .is_none(),
            "an under-sized ML-DSA key must be rejected"
        );
        // KEM one byte long → None.
        assert!(
            peer_handshake_contact(
                node,
                None,
                dsa.clone(),
                vec![2u8; ML_KEM_768_PUBKEY_LEN + 1]
            )
            .is_none(),
            "an over-sized ML-KEM key must be rejected"
        );
        // Oversized relay URL → None.
        let long_relay = "h".repeat(MAX_TUNNEL_RELAY_URL_LEN + 1);
        assert!(
            peer_handshake_contact(node, Some(long_relay), dsa.clone(), kem.clone()).is_none(),
            "an oversized home_relay_url must be rejected"
        );
        // A relay URL at exactly the cap is accepted.
        let max_relay = "h".repeat(MAX_TUNNEL_RELAY_URL_LEN);
        assert!(
            peer_handshake_contact(node, Some(max_relay), dsa, kem).is_some(),
            "a relay URL at exactly the cap must be accepted"
        );
    }
}
