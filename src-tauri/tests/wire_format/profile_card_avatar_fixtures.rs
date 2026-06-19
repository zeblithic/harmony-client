//! ZEB-343: pin the canonical CBOR wire format of ProfileCardBroadcast WITH an
//! avatar_cid set (7-entry map incl. "av"). The no-avatar case stays in
//! wire_format_profile_card_fixtures.rs (6-entry map, byte-identical to ZEB-341).
//!
//! ZEB-345 extends this with the additive `profile_page_root` ("pp") field:
//! - pp-only card -> 7-entry map (0xA7), keys incl. "pp" but NOT "av";
//! - avatar+pp card -> 8-entry map (0xA8), keys incl. both "av" and "pp".
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::Hlc;
use harmony_app::profile_card_broadcast::ProfileCardBroadcast;

#[test]
fn profile_card_with_avatar_pins_seven_keys_incl_av() {
    let owner = harmony_app::community_membership::mint_test_owner(0x7E);
    let card = ProfileCardBroadcast {
        owner_id: owner.owner.0,
        display_name: "Ann".into(),
        status_text: "hi".into(),
        avatar_cid: Some([0x33u8; 32]),
        profile_page_root: None,
        enrollment: owner.cert,
        shared_at: Hlc {
            wall_ms: 1234,
            logical: 0,
            device_id: "d".into(),
        },
        signature: [0u8; 64],
    };
    let bytes = canonical_cbor_encode(&card).expect("encode");
    assert_eq!(
        bytes[0], 0xA7,
        "expected 7-entry CBOR map header with avatar set"
    );
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
            ["oi", "dn", "st", "av", "en", "sa", "sg"].map(str::to_string)
        )
    );
    let back: ProfileCardBroadcast = ciborium::de::from_reader(&bytes[..]).expect("decode struct");
    assert_eq!(back.avatar_cid, Some([0x33u8; 32]));
}

#[test]
fn profile_card_with_page_root_only_pins_seven_keys_incl_pp() {
    // ZEB-345: avatar absent, profile_page_root present -> 7-entry map (0xA7),
    // keys include "pp" but NOT "av" (different key-set, same arity as avatar-only).
    let owner = harmony_app::community_membership::mint_test_owner(0x8E);
    let card = ProfileCardBroadcast {
        owner_id: owner.owner.0,
        display_name: "Ann".into(),
        status_text: "hi".into(),
        avatar_cid: None,
        profile_page_root: Some([0x44u8; 32]),
        enrollment: owner.cert,
        shared_at: Hlc {
            wall_ms: 1234,
            logical: 0,
            device_id: "d".into(),
        },
        signature: [0u8; 64],
    };
    let bytes = canonical_cbor_encode(&card).expect("encode");
    assert_eq!(
        bytes[0], 0xA7,
        "expected 7-entry CBOR map header with profile_page_root set"
    );
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
            ["oi", "dn", "st", "pp", "en", "sa", "sg"].map(str::to_string)
        )
    );
    let back: ProfileCardBroadcast = ciborium::de::from_reader(&bytes[..]).expect("decode struct");
    assert_eq!(back.avatar_cid, None);
    assert_eq!(back.profile_page_root, Some([0x44u8; 32]));
}

#[test]
fn profile_card_with_avatar_and_page_root_pins_eight_keys() {
    // ZEB-345: both avatar_cid AND profile_page_root present -> 8-entry map (0xA8),
    // keys include both "av" and "pp".
    let owner = harmony_app::community_membership::mint_test_owner(0x9E);
    let card = ProfileCardBroadcast {
        owner_id: owner.owner.0,
        display_name: "Ann".into(),
        status_text: "hi".into(),
        avatar_cid: Some([0x33u8; 32]),
        profile_page_root: Some([0x44u8; 32]),
        enrollment: owner.cert,
        shared_at: Hlc {
            wall_ms: 1234,
            logical: 0,
            device_id: "d".into(),
        },
        signature: [0u8; 64],
    };
    let bytes = canonical_cbor_encode(&card).expect("encode");
    assert_eq!(
        bytes[0], 0xA8,
        "expected 8-entry CBOR map header with avatar + profile_page_root set"
    );
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
            ["oi", "dn", "st", "av", "pp", "en", "sa", "sg"].map(str::to_string)
        )
    );
    let back: ProfileCardBroadcast = ciborium::de::from_reader(&bytes[..]).expect("decode struct");
    assert_eq!(back.avatar_cid, Some([0x33u8; 32]));
    assert_eq!(back.profile_page_root, Some([0x44u8; 32]));
}
