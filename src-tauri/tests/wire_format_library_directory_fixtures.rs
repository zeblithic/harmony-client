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
        library_identity_pub: None,
        library_signature: None,
    };

    let bytes = canonical_cbor_encode(&entry).expect("encode");

    // Round-trip — must deserialize back to the same struct.
    let roundtrip: LibraryDirectoryEntry = ciborium::de::from_reader(&bytes[..]).expect("decode");
    assert_eq!(
        entry, roundtrip,
        "round-trip preserves LibraryDirectoryEntry"
    );

    // R3 F5: pin EXACT canonical bytes. Round-trip alone would pass
    // even if encoder + decoder changed together. The expected hex is
    // captured from the deterministic fixture above; an encoder rev
    // that shifts any byte must update this string deliberately.
    assert_eq!(
        hex::encode(&bytes),
        "a96263645011111111111111111111111111111111626169584000070e151c232a31383f464d545b626970777e858c939aa1a8afb6bdc4cbd2d9e0e7eef5fc030a11181f262d343b424950575e656c737a81888f969da4abb2b9626e6d714669787475726520436f6d6d756e697479626473782150696e6e656420666f7220776972652d666f726d61742073746162696c6974792e6274708264746573746b776972652d666f726d617462697578186861726d6f6e793a2f2f696e766974652f3f703d41414141626c625022222222222222222222222222222222626c61a361771b0000018bcfe56800616c182a61646e666978747572652d646576696365626373584033333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333",
        "canonical CBOR bytes drifted from pinned fixture"
    );
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
    // R3 F5: pin exact canonical bytes (see LibraryDirectoryEntry test).
    assert_eq!(
        hex::encode(&bytes),
        "a262616450abababababababababababababababab626174a361771b0000018bcfe56800616c182a61646e666978747572652d646576696365",
        "canonical CBOR bytes drifted from pinned fixture"
    );
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
    // R3 F5: pin exact canonical bytes (see LibraryDirectoryEntry test).
    assert_eq!(
        hex::encode(&bytes),
        "a362616450cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd626174a361771b0000018bcfe56800616c182a61646e666978747572652d64657669636562726da361771b0000018bcfe56800616c182b61646e666978747572652d646576696365",
        "canonical CBOR bytes drifted from pinned fixture"
    );
}

/// 2-char field-key invariant: every key in the canonical CBOR must be
/// a 2-byte text(2) string.
///
/// R4 F1: previous implementation scanned for `[0x62, k0, k1]` byte
/// patterns via `windows(3)`, which could false-pass if a payload byte
/// sequence coincidentally matched the header+key triple. The strict
/// approach is to decode the canonical CBOR into a `Value::Map`, walk
/// its keys, and assert exact set-equality with the expected keys.
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
        library_identity_pub: None,
        library_signature: None,
    };
    let bytes = canonical_cbor_encode(&entry).expect("encode");

    let value: ciborium::value::Value = ciborium::de::from_reader(&bytes[..]).expect("decode");
    let map = match value {
        ciborium::value::Value::Map(m) => m,
        other => panic!("expected CBOR map, got {other:?}"),
    };

    let mut keys = std::collections::BTreeSet::new();
    for (k, _) in map {
        match k {
            ciborium::value::Value::Text(s) => {
                assert_eq!(s.len(), 2, "field key must be 2 chars: {s:?}");
                keys.insert(s);
            }
            other => panic!("non-text map key in LibraryDirectoryEntry encoding: {other:?}"),
        }
    }

    let expected: std::collections::BTreeSet<String> =
        ["cd", "ai", "nm", "ds", "tp", "iu", "lb", "la", "cs"]
            .into_iter()
            .map(str::to_string)
            .collect();
    assert_eq!(keys, expected, "field keys must match exactly");
}
