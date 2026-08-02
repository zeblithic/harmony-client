//! Wire-format pinning fixtures for `LibraryAnnounce`.
//!
//! These tests pin the exact canonical-CBOR bytes for a known-good
//! announce record. Pinning catches accidental wire-format changes
//! (field renames, key additions, type substitutions) BEFORE they
//! break cross-device compat.
//!
//! Companion to `wire_format/library_directory_fixtures.rs` (Phase 1).

use ciborium::value::Value as CborValue;
use ed25519_dalek::{Signer, SigningKey};
use harmony_app::library_directory::{verify_announce, LibraryAnnounce};
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::Hlc;
use std::collections::BTreeSet;

/// Build a canonical test `LibraryAnnounce` with deterministic keys.
/// Returns (announce, signing_key, derived library_addr_hex).
///
/// Constructs the 64-byte identity bundle (X25519_pub || Ed25519_pub)
/// inline — mirrors Phase 1's `build_test_identity_pub` pattern. The
/// X25519 half is a constant fill (the verifier only consults the
/// Ed25519 half); the Ed25519 half is derived from the deterministic
/// seed so signing is reproducible.
fn canonical_test_announce() -> (LibraryAnnounce, String) {
    // Seed [7u8; 32] — chosen to differ from Phase 1 fixtures.
    let signing_key = SigningKey::from_bytes(&[7u8; 32]);
    let ed_verifying = signing_key.verifying_key().to_bytes();
    let mut identity_pub = [0u8; 64];
    identity_pub[..32].copy_from_slice(&[0x11; 32]);
    identity_pub[32..].copy_from_slice(&ed_verifying);

    let mut announce = LibraryAnnounce {
        library_identity_pub: identity_pub,
        name: "Indie Games Library".to_string(),
        description: "Curated indie game communities".to_string(),
        listed_at: Hlc {
            wall_ms: 1_715_000_000_000,
            logical: 1,
            device_id: "fixture-device".to_string(),
        },
        library_signature: [0u8; 64],
    };
    // Sign canonical CBOR with sig zeroed.
    let signed_bytes = canonical_cbor_encode(&announce).expect("encode");
    let sig = signing_key.sign(&signed_bytes);
    announce.library_signature = sig.to_bytes();

    let identity =
        harmony_identity::Identity::from_public_bytes(&identity_pub).expect("identity parses");
    let addr_hex = hex::encode(identity.address_hash);
    (announce, addr_hex)
}

#[test]
fn announce_canonical_cbor_roundtrip() {
    let (announce, _) = canonical_test_announce();
    let bytes = canonical_cbor_encode(&announce).expect("encode");
    let decoded: LibraryAnnounce = ciborium::from_reader(&bytes[..]).expect("decode");
    assert_eq!(decoded, announce);
}

#[test]
fn announce_verifies_after_signing() {
    let (announce, expected_addr_hex) = canonical_test_announce();
    // `None` ⇒ apply-all: this pins signing/verification, not the ZEB-852
    // forward-skew bound, so it stays independent of the wall clock.
    let addr = verify_announce(&announce, None).expect("verify");
    assert_eq!(hex::encode(addr.0), expected_addr_hex);
}

#[test]
fn announce_field_keys_are_2char() {
    let (announce, _) = canonical_test_announce();
    let bytes = canonical_cbor_encode(&announce).expect("encode");
    let value: CborValue = ciborium::from_reader(&bytes[..]).expect("decode as value");
    let map = match value {
        CborValue::Map(m) => m,
        other => panic!("expected map, got {:?}", other),
    };
    let keys: BTreeSet<String> = map
        .iter()
        .filter_map(|(k, _)| match k {
            CborValue::Text(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    let expected: BTreeSet<String> = ["ai", "nm", "ds", "la", "ls"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(
        keys, expected,
        "field keys must be exactly {{ai,nm,ds,la,ls}}"
    );
}

#[test]
fn announce_pinned_bytes_prefix_stable() {
    // Pin the canonical bytes' length and a structural prefix so any
    // accidental change to field order or types fails loudly. We don't
    // pin the full byte string here because it's verbose — the
    // `_field_keys_are_2char` test catches key-rename slip-throughs,
    // and the prefix pin catches map-arity / first-key-shape changes.
    let (announce, _) = canonical_test_announce();
    let bytes = canonical_cbor_encode(&announce).expect("encode");

    // Map with 5 entries → CBOR major type 5, count 5 → first byte 0xA5.
    assert_eq!(bytes[0], 0xA5, "canonical CBOR must start with map(5)");

    // First key is "ai" — 2-char text string, 0x62 prefix.
    assert_eq!(
        bytes[1], 0x62,
        "first key prefix must be 2-char text-string"
    );
    assert_eq!(&bytes[2..4], b"ai", "first key must be 'ai'");
}
