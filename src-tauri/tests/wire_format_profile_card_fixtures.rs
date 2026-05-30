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
    for code in ["oi", "dn", "st", "en", "sa", "sg"] {
        let needle = [0x62, code.as_bytes()[0], code.as_bytes()[1]];
        assert!(
            bytes.windows(3).any(|w| w == needle),
            "missing field code {code}"
        );
    }
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
