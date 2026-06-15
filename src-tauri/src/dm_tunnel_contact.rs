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
    let hash = crate::dm_signing::derive_device_hash_from_identity_pub(&identity_pub_64)
        .expect("our own identity pub must derive a device hash");
    (vec![hash], vec![Some(identity_pub_64)])
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
}
