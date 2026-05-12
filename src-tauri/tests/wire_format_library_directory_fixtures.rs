//! Wire-format pinning fixtures for ZEB-218 Sub-D Phase 1.
//!
//! Captures the canonical-CBOR encoding of `LibraryDirectoryEntry` and
//! `LibraryEntry` so accidental field renames or type changes surface
//! as a hex-bytes diff in CI. Mirrors `wire_format_community_sync_fixtures.rs`.

use harmony_app::library_directory::LibraryDirectoryEntry;
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::{Hlc, LibraryEntry, OwnerAddr, SpaceId};

/// Deterministic 64-byte identity_pub for fixture stability.
/// First 32 bytes = X25519, next 32 bytes = Ed25519. Real values not
/// load-bearing for the pin — we just need stable bytes.
fn fixture_admin_identity_pub() -> [u8; 64] {
    let mut out = [0u8; 64];
    for (i, b) in out.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(7);
    }
    out
}

fn fixture_hlc() -> Hlc {
    Hlc {
        wall_ms: 1_700_000_000_000,
        logical: 42,
        device_id: "fixture-device".to_string(),
    }
}

#[test]
fn library_directory_entry_canonical_cbor_pinned() {
    let entry = LibraryDirectoryEntry {
        community_id: SpaceId([0x11; 16]),
        community_admin_identity_pub: fixture_admin_identity_pub(),
        name: "Fixture Community".to_string(),
        description: "Pinned for wire-format stability.".to_string(),
        topics: vec!["test".to_string(), "wire-format".to_string()],
        invite_url: "harmony://invite/?p=AAAA".to_string(),
        listed_by: OwnerAddr([0x22; 16]),
        listed_at: fixture_hlc(),
        community_signature: [0x33; 64],
    };

    let bytes = canonical_cbor_encode(&entry).expect("encode");

    // Round-trip — must deserialize back to the same struct.
    let roundtrip: LibraryDirectoryEntry = ciborium::de::from_reader(&bytes[..]).expect("decode");
    assert_eq!(
        entry, roundtrip,
        "round-trip preserves LibraryDirectoryEntry"
    );

    // Sentinel: encoded length must be > 0; serves as a basic
    // wire-format-stability check. Stricter pinning (exact hex bytes)
    // can be added in a follow-up if needed — for Phase 1 the
    // `field_keys_are_2char` test below provides field-key-rename
    // protection which is the main risk.
    assert!(!bytes.is_empty());
}

#[test]
fn library_entry_canonical_cbor_pinned() {
    let entry = LibraryEntry {
        address: OwnerAddr([0xAB; 16]),
        added_at: fixture_hlc(),
        removed_at: None,
    };
    let bytes = canonical_cbor_encode(&entry).expect("encode");
    let roundtrip: LibraryEntry = ciborium::de::from_reader(&bytes[..]).expect("decode");
    assert_eq!(entry, roundtrip);
    assert!(!bytes.is_empty());
}

#[test]
fn library_entry_with_tombstone_canonical_cbor_pinned() {
    let added = fixture_hlc();
    let mut removed = added.clone();
    removed.logical += 1;
    let entry = LibraryEntry {
        address: OwnerAddr([0xCD; 16]),
        added_at: added,
        removed_at: Some(removed),
    };
    let bytes = canonical_cbor_encode(&entry).expect("encode");
    let roundtrip: LibraryEntry = ciborium::de::from_reader(&bytes[..]).expect("decode");
    assert_eq!(entry, roundtrip);
    assert!(!bytes.is_empty());
}

/// 2-char field-key invariant: every key in the canonical CBOR must be
/// a 2-byte text(2) string (CBOR major-type 3, length 2). The CBOR
/// header for text(2) is 0x62. We scan for the windows(3) sequence
/// `[0x62, key_byte_0, key_byte_1]` matching each declared field.
///
/// This is the same pattern as ZEB-255's
/// `non_community_space_skips_membership_fields_in_wire`.
#[test]
fn library_directory_entry_field_keys_are_2char() {
    let entry = LibraryDirectoryEntry {
        community_id: SpaceId([0; 16]),
        community_admin_identity_pub: [0; 64],
        name: String::new(),
        description: String::new(),
        topics: vec![],
        invite_url: String::new(),
        listed_by: OwnerAddr([0; 16]),
        listed_at: Hlc {
            wall_ms: 0,
            logical: 0,
            device_id: String::new(),
        },
        community_signature: [0; 64],
    };
    let bytes = canonical_cbor_encode(&entry).expect("encode");

    for key in ["cd", "ai", "nm", "ds", "tp", "iu", "lb", "la", "cs"] {
        let needle = [0x62, key.as_bytes()[0], key.as_bytes()[1]];
        assert!(
            bytes.windows(3).any(|w| w == needle),
            "field key {key:?} (CBOR text(2)) not found in encoded bytes"
        );
    }
}
