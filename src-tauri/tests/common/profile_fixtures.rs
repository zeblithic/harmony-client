//! Shared test fixtures for Sub-D Phase 4 (ZEB-281) integration tests.
//!
//! Mirrors `tests/common/library_fixtures.rs` (Sub-D Phase 1/2/3) in
//! shape: deterministic SigningKey + synthesized 64-byte identity bundle
//! from a 32-byte seed, then a signed `ProfileMembershipBroadcast`
//! produced via the production `sign_broadcast` primitive.
//!
//! Gated on the `test-fixtures` Cargo feature so it doesn't bloat
//! release binaries.

#![allow(dead_code)]

use ed25519_dalek::SigningKey;
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use harmony_app::profile_broadcast::{sign_broadcast, ProfileMembershipBroadcast};

/// Build a deterministic test identity bundle (signing key + 64-byte
/// `identity_pub` (X25519_pub(32) || Ed25519_pub(32)) + derived
/// `OwnerAddr`) from a 32-byte Ed25519 seed.
///
/// X25519 prefix `[0x55; 32]` keeps these fixtures orthogonal from
/// `library_directory::tests::build_test_identity_pub` (`[0x11; 32]`)
/// and `profile_broadcast::tests::build_identity` (`[0x33; 32]`) — for
/// the same Ed25519 seed, the three helpers produce DIFFERENT
/// OwnerAddrs (the address hash mixes the X25519 half), so a test that
/// crosses helpers won't silently coincide on identity.
///
/// `Identity::from_public_bytes` validates only the Ed25519 half
/// (the X25519 half is opaque bytes from address-derivation's
/// perspective), so any 32-byte prefix produces a parseable identity.
pub fn build_test_owner_identity(seed: [u8; 32]) -> (SigningKey, [u8; 64], OwnerAddr) {
    let signing = SigningKey::from_bytes(&seed);
    let verifying = signing.verifying_key().to_bytes();
    let mut bundle = [0u8; 64];
    bundle[..32].copy_from_slice(&[0x55; 32]);
    bundle[32..].copy_from_slice(&verifying);
    let addr = OwnerAddr(
        harmony_identity::Identity::from_public_bytes(&bundle)
            .expect("identity from public bytes")
            .address_hash,
    );
    (signing, bundle, addr)
}

/// Build a canonically-CBOR-encoded, Ed25519-signed
/// `ProfileMembershipBroadcast`. Returns
/// (cbor_bytes, broadcaster_owner_addr, decoded_broadcast).
///
/// The caller is responsible for HLC monotonicity in the calling test —
/// `sign_broadcast` itself does not enforce ordering. The publisher
/// state machine enforces it in production; tests can pass arbitrary
/// HLCs to exercise replay-defense paths.
pub fn mock_profile_broadcast(
    seed: [u8; 32],
    community_ids: Vec<SpaceId>,
    shared_at: Hlc,
) -> (Vec<u8>, OwnerAddr, ProfileMembershipBroadcast) {
    let (signer, identity_pub, addr) = build_test_owner_identity(seed);
    let b =
        sign_broadcast(&signer, identity_pub, community_ids, shared_at).expect("sign_broadcast");
    let bytes = canonical_cbor_encode(&b).expect("canonical_cbor_encode");
    (bytes, addr, b)
}

pub fn fixture_space_id(byte: u8) -> SpaceId {
    SpaceId([byte; 16])
}

pub fn fixture_hlc(wall_ms: u64, logical: u32) -> Hlc {
    Hlc {
        wall_ms,
        logical,
        device_id: "fix".into(),
    }
}
