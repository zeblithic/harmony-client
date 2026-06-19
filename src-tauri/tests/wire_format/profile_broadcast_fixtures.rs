//! Sub-D Phase 4 (ZEB-281) wire-format pinning. Pinned bytes prevent
//! silent wire-format changes — if any test here fails, treat it as a
//! wire-protocol break and review carefully (cross-version compatibility,
//! peer interop).
//!
//! Mirrors `wire_format/library_directory_fixtures.rs` (Sub-D Phase 1+3).

use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::{Hlc, SpaceId};
use harmony_app::profile_broadcast::ProfileMembershipBroadcast;

fn fixture_hlc() -> Hlc {
    Hlc {
        wall_ms: 1700000000000,
        logical: 0,
        device_id: "fix".into(),
    }
}

/// Canonical CBOR round-trip + pinned prefix. Decoded back via
/// `ciborium::value::Value::Map` to assert the EXACT key ordering
/// (ai → cs → sa → sg as declared in the struct).
#[test]
fn profile_broadcast_canonical_cbor_pinned() {
    let b = ProfileMembershipBroadcast {
        owner_identity_pub: [0xaa; 64],
        community_ids: vec![SpaceId([0x11; 16]), SpaceId([0x22; 16])],
        shared_at: fixture_hlc(),
        signature: [0xbb; 64],
    };
    let bytes = canonical_cbor_encode(&b).expect("encode");

    // map(4) marker: 0xa4
    assert_eq!(
        bytes[0], 0xa4,
        "ProfileMembershipBroadcast must encode as map(4); got map({:#x}) prefix",
        bytes[0]
    );

    // Full canonical key ordering — declaration order.
    let value: ciborium::value::Value = ciborium::de::from_reader(&bytes[..]).expect("decode");
    let map = match value {
        ciborium::value::Value::Map(m) => m,
        other => panic!("expected CBOR map, got {other:?}"),
    };
    let observed_keys: Vec<String> = map
        .into_iter()
        .map(|(k, _)| match k {
            ciborium::value::Value::Text(s) => s,
            other => panic!("non-text map key: {other:?}"),
        })
        .collect();
    let expected_keys: Vec<&str> = vec!["ai", "cs", "sa", "sg"];
    assert_eq!(
        observed_keys, expected_keys,
        "ProfileMembershipBroadcast must encode keys in this exact declaration order \
         (signature portability depends on canonical CBOR encoding)"
    );
}

/// 2-char key invariant. Every key at this nesting level must be 2 chars
/// so `canonical_cbor_encode`'s same-length-keys precondition holds.
/// Mirrors `phase3_wrapped_entry_two_char_keys_audit`.
#[test]
fn profile_broadcast_field_keys_are_2char() {
    let b = ProfileMembershipBroadcast {
        owner_identity_pub: [0u8; 64],
        community_ids: vec![],
        shared_at: Hlc {
            wall_ms: 0,
            logical: 0,
            device_id: String::new(),
        },
        signature: [0u8; 64],
    };
    let bytes = canonical_cbor_encode(&b).expect("encode");
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
            other => panic!("non-text map key: {other:?}"),
        }
    }
    // Confirm we observed exactly the 4 expected keys.
    let expected: std::collections::BTreeSet<String> = ["ai", "cs", "sa", "sg"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(keys, expected, "expected exactly 4 keys (ai/cs/sa/sg)");
}
