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

/// ZEB-280 Phase 3: build a `LibraryDirectoryEntry` with both layers
/// of signatures. Pass `library_signer = None` to produce a Phase 1-
/// shaped (unwrapped) entry equivalent to `mock_directory_entry`.
/// Pass `Some((signing_key, identity_bundle))` to produce a fully
/// wrapped Phase 3 entry.
///
/// Admin sig signs over canonical CBOR with cs=0, li=None, ls=None
/// (skip_serializing_if omits the Optional fields). Library sig
/// signs over canonical CBOR with ls=None, li populated, cs populated.
///
/// Spec §5.
#[allow(clippy::too_many_arguments)]
pub fn mock_library_entry_wrapped(
    community_id: SpaceId,
    admin_seed: [u8; 32],
    listed_by: OwnerAddr,
    listed_at: Hlc,
    invite_url: String,
    name: &str,
    description: &str,
    topics: Vec<String>,
    library_signer: Option<(&SigningKey, [u8; 64])>,
) -> LibraryDirectoryEntry {
    let (admin_signing_key, admin_identity_pub) = build_test_admin_identity(admin_seed);

    // Phase 1 admin sig (Optional fields None — skip_serializing_if
    // omits them, so admin sig is identical to Phase 1).
    let mut entry = LibraryDirectoryEntry {
        community_id,
        community_admin_identity_pub: admin_identity_pub,
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
    let admin_signed = canonical_cbor_encode(&for_sig).expect("encode admin sign");
    entry.community_signature = admin_signing_key.sign(&admin_signed).to_bytes();

    // Phase 3 wrapping sig (if library_signer provided).
    if let Some((library_signing_key, library_identity_bundle)) = library_signer {
        entry.library_identity_pub = Some(library_identity_bundle);
        entry.library_signature = None;
        let lib_signed = canonical_cbor_encode(&entry).expect("encode library sign");
        entry.library_signature = Some(library_signing_key.sign(&lib_signed).to_bytes());
    }

    entry
}

/// ZEB-280 Phase 3: take an already-admin-signed entry (presumed
/// produced by `mock_directory_entry` or `mock_library_entry_wrapped`)
/// and replace its wrapping sig with a new library's signature.
/// This is the verbatim re-syndication primitive: library A
/// republishes library B's entry by re-signing over the same
/// admin-signed bytes with A's own key, advertising A as the
/// broadcaster.
///
/// Spec §3 / §5.
pub fn mock_library_entry_republished_by(
    original: &LibraryDirectoryEntry,
    new_library_signing_key: &SigningKey,
    new_library_identity_bundle: [u8; 64],
) -> LibraryDirectoryEntry {
    let mut wrapped = original.clone();
    wrapped.library_identity_pub = Some(new_library_identity_bundle);
    wrapped.library_signature = None;
    let lib_signed = canonical_cbor_encode(&wrapped).expect("encode library sign");
    wrapped.library_signature = Some(new_library_signing_key.sign(&lib_signed).to_bytes());
    wrapped
}

/// ZEB-280 Phase 3: build a deterministic library identity bundle +
/// signing key. Mirrors `build_test_admin_identity` and uses the same
/// X25519 prefix (`0x11 × 32`) — ZEB-280 R1 (CodeAnt) flagged the
/// previous `0x22 × 32` prefix as a footgun: for the same Ed25519 seed,
/// `build_test_library_identity` and `mock_library_announce` would
/// derive *different* `OwnerAddr`s, silently breaking any test that
/// crosses the two helpers.
pub fn build_test_library_identity(seed: [u8; 32]) -> (SigningKey, [u8; 64]) {
    let signing = SigningKey::from_bytes(&seed);
    let verifying = signing.verifying_key().to_bytes();
    let mut bundle = [0u8; 64];
    bundle[..32].copy_from_slice(&[0x11; 32]);
    bundle[32..].copy_from_slice(&verifying);
    (signing, bundle)
}

/// ZEB-280 Phase 3: build a library identity + derive its `OwnerAddr`
/// in one shot. Bundles the common test prelude: signer, identity
/// bundle, derived `OwnerAddr`.
///
/// Distinct from `build_test_library_identity` only in that it ALSO
/// returns the derived `OwnerAddr` — saves callers from doing
/// `OwnerAddr(harmony_identity::Identity::from_public_bytes(&bundle).expect(...).address_hash)`
/// at every call site.
pub fn build_test_library_addr(seed: [u8; 32]) -> (SigningKey, [u8; 64], OwnerAddr) {
    let (signing_key, bundle) = build_test_library_identity(seed);
    let addr = OwnerAddr(
        harmony_identity::Identity::from_public_bytes(&bundle)
            .expect("identity from public bytes")
            .address_hash,
    );
    (signing_key, bundle, addr)
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
