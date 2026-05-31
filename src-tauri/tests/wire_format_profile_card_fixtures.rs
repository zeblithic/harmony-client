//! ZEB-341: pin the canonical CBOR wire format of ProfileCardBroadcast.
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::Hlc;
use harmony_app::profile_card_broadcast::ProfileCardBroadcast;

#[test]
fn profile_card_canonical_cbor_pins_field_codes() {
    let owner = harmony_app::community_membership::mint_test_owner(0x7C);
    let card = ProfileCardBroadcast {
        owner_id: owner.owner.0,
        display_name: "Ann".into(),
        status_text: "hi".into(),
        enrollment: owner.cert,
        shared_at: Hlc {
            wall_ms: 1234,
            logical: 0,
            device_id: "d".into(),
        },
        signature: [0u8; 64],
    };
    let bytes = canonical_cbor_encode(&card).expect("encode");
    assert_eq!(bytes[0], 0xA6, "expected 6-entry CBOR map header");
    // Decode into a CBOR map and assert the EXACT key-set. A raw byte-window
    // scan can false-pass on payload bytes that happen to match a field code.
    let value: ciborium::value::Value = ciborium::de::from_reader(&bytes[..]).expect("decode");
    let map = match value {
        ciborium::value::Value::Map(m) => m,
        other => panic!("expected CBOR map, got {other:?}"),
    };
    let keys: std::collections::HashSet<String> = map
        .iter()
        .filter_map(|(k, _)| match k {
            ciborium::value::Value::Text(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        keys,
        std::collections::HashSet::from_iter(
            ["oi", "dn", "st", "en", "sa", "sg"].map(str::to_string)
        )
    );
}

#[test]
fn profile_card_round_trips_through_canonical_cbor() {
    let owner = harmony_app::community_membership::mint_test_owner(0x7D);
    let card = ProfileCardBroadcast {
        owner_id: owner.owner.0,
        display_name: "Bo".into(),
        status_text: "".into(),
        enrollment: owner.cert,
        shared_at: Hlc {
            wall_ms: 9,
            logical: 1,
            device_id: "x".into(),
        },
        signature: [0x11; 64],
    };
    let bytes = canonical_cbor_encode(&card).expect("encode");
    let back: ProfileCardBroadcast = ciborium::de::from_reader(&bytes[..]).expect("decode");
    assert_eq!(back, card);
}
