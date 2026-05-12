//! Mock library fixture for ZEB-218 Sub-D Phase 1 integration tests.
//!
//! Provides a deterministic builder for signed `LibraryDirectoryEntry`
//! records. Use these helpers from integration tests; the production
//! signing path lives off-client (libraries publish entries — we are
//! only the consumer).
//!
//! Gated on the `test-fixtures` Cargo feature so it doesn't bloat
//! release binaries.

#![allow(dead_code)]

use ed25519_dalek::{Signer, SigningKey};
use harmony_app::library_directory::{LibraryAnnounce, LibraryDirectoryEntry};
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

/// Build a test admin identity_pub from a 32-byte Ed25519 seed.
/// Returns `(signing_key, identity_pub)`. The X25519 half is set to
/// a stable constant (`0x11` × 32) — Phase 1 verifier ignores it.
pub fn build_test_admin_identity(seed: [u8; 32]) -> (SigningKey, [u8; 64]) {
    let ed_signing = SigningKey::from_bytes(&seed);
    let ed_verifying = ed_signing.verifying_key().to_bytes();
    let mut identity_pub = [0u8; 64];
    identity_pub[..32].copy_from_slice(&[0x11; 32]);
    identity_pub[32..].copy_from_slice(&ed_verifying);
    (ed_signing, identity_pub)
}

/// Construct a `LibraryDirectoryEntry`, signing over canonical CBOR
/// with `community_signature` zeroed at sign time (matching the
/// production verifier). Returns the signed entry ready to publish.
#[allow(clippy::too_many_arguments)]
pub fn mock_directory_entry(
    community_id: SpaceId,
    admin_seed: [u8; 32],
    listed_by: OwnerAddr,
    listed_at: Hlc,
    invite_url: String,
    name: &str,
    description: &str,
    topics: Vec<String>,
) -> LibraryDirectoryEntry {
    let (signing_key, identity_pub) = build_test_admin_identity(admin_seed);
    let mut entry = LibraryDirectoryEntry {
        community_id,
        community_admin_identity_pub: identity_pub,
        name: name.to_string(),
        description: description.to_string(),
        topics,
        invite_url,
        listed_by,
        listed_at,
        community_signature: [0u8; 64],
        library_identity_pub: None,
        library_signature: None,
    };
    let mut for_sig = entry.clone();
    for_sig.community_signature = [0u8; 64];
    let signed_bytes = canonical_cbor_encode(&for_sig).expect("encode");
    entry.community_signature = signing_key.sign(&signed_bytes).to_bytes();
    entry
}

/// Build a signed `LibraryAnnounce` ready to publish on
/// `harmony/discovery/library/announce`. Returns the encoded bytes
/// plus the derived library `OwnerAddr` (so callers can assert
/// `process_announce` returned the expected addr).
///
/// Mirrors the pattern in `tests/wire_format_library_announce_fixtures.rs`:
/// builds a deterministic 64-byte identity bundle
/// (X25519_pub || Ed25519_pub) inline from a 32-byte Ed25519 seed,
/// then signs canonical CBOR over the announce with the signature
/// field zeroed at sign time (matching `verify_announce`).
pub fn mock_library_announce(
    seed: [u8; 32],
    name: &str,
    description: &str,
    listed_at_wall_ms: u64,
) -> (Vec<u8>, OwnerAddr) {
    let signing_key = SigningKey::from_bytes(&seed);
    let ed_verifying = signing_key.verifying_key().to_bytes();
    let mut identity_pub = [0u8; 64];
    identity_pub[..32].copy_from_slice(&[0x11; 32]);
    identity_pub[32..].copy_from_slice(&ed_verifying);

    let mut announce = LibraryAnnounce {
        library_identity_pub: identity_pub,
        name: name.to_string(),
        description: description.to_string(),
        listed_at: Hlc {
            wall_ms: listed_at_wall_ms,
            logical: 0,
            device_id: "fixture-device".to_string(),
        },
        library_signature: [0u8; 64],
    };
    // Sign canonical CBOR with the signature field zeroed — verifier
    // re-zeroes before checking, so the sign/verify byte streams match.
    let signed_bytes = canonical_cbor_encode(&announce).expect("encode for sign");
    announce.library_signature = signing_key.sign(&signed_bytes).to_bytes();

    let bytes = canonical_cbor_encode(&announce).expect("encode for publish");
    let identity = harmony_identity::Identity::from_public_bytes(&identity_pub)
        .expect("identity from public bytes");
    let addr = OwnerAddr(identity.address_hash);
    (bytes, addr)
}
