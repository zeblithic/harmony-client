//! ZEB-250: Byte-pinned canonical CBOR fixtures for AdminProposal,
//! ProposalKind, AdminCountersign.
//!
//! These tests lock the canonical-CBOR wire encoding for the new
//! ZEB-250 types. Any failure here is a wire-protocol break — review
//! carefully before updating the pinned bytes (cross-version compat,
//! peer interop).
//!
//! Uses deterministic test bytes (zero or repeated-byte values) so the
//! encoded bytes are byte-stable across runs. The tests do NOT verify
//! cryptographic validity — they pin BYTE LAYOUT only.

use harmony_app::community_membership::{MembershipEventKind, ProposalKind};
use harmony_app::community_state_crdt::CommunityState;
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::{OwnerAddr, SpaceId};

const FIXTURE_TARGET_ADDR: OwnerAddr = OwnerAddr([0x11; 16]);
const FIXTURE_TARGET_EVENT_ID: [u8; 16] = [0x66; 16];
const FIXTURE_COMMUNITY_ID: SpaceId = SpaceId([0x77; 16]);

const EXPECTED_COMMUNITY_STATE_WITH_ADMIN_QUORUM_HEX: &str =
    "a3626369507777777777777777777777777777777762617103626576a0";

// EXPECTED_*_HEX constants are populated by running the test once with
// "FILL_AFTER" as the value; the panic message prints the actual hex
// to paste back in. Regen-on-first-run pattern from ZEB-254.

const EXPECTED_ADMIN_PROPOSAL_SETPOWER_HEX: &str = "a2627467617162766ca162706ba2626b646173626264a26274675011111111111111111111111111111111626c761864";
const EXPECTED_ADMIN_PROPOSAL_KICK_HEX: &str = "a2627467617162766ca162706ba2626b64616b626264a262746750111111111111111111111111111111116272736e76696f6c617465642072756c6573";
const EXPECTED_ADMIN_PROPOSAL_CHANGE_QUORUM_HEX: &str =
    "a2627467617162766ca162706ba2626b646163626264a1626e7103";
const EXPECTED_ADMIN_COUNTERSIGN_HEX: &str =
    "a2627467616e62766ca16274695066666666666666666666666666666666";

#[test]
fn admin_proposal_setpower_canonical_cbor() {
    let kind = MembershipEventKind::AdminProposal {
        proposal_kind: ProposalKind::SetPower {
            target: FIXTURE_TARGET_ADDR,
            level: 100,
        },
    };
    let encoded = canonical_cbor_encode(&kind).expect("encode");
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ADMIN_PROPOSAL_SETPOWER_HEX.contains("FILL_AFTER") {
        panic!(
            "REGENERATE EXPECTED_ADMIN_PROPOSAL_SETPOWER_HEX = \"{}\";",
            actual_hex
        );
    }
    assert_eq!(
        actual_hex, EXPECTED_ADMIN_PROPOSAL_SETPOWER_HEX,
        "AdminProposal+SetPower wire format changed"
    );

    // Structural sanity: confirm the encoding shape. The serde
    // representation choices for the layered enums determine what
    // outer/inner keys appear; the asserts below verify the
    // ProposalKind discriminator (`kd`) + body (`bd`) keys exist
    // somewhere in the encoded structure. If these asserts fail,
    // re-read the enum serde attributes before updating the bytes —
    // a wire-format change here breaks peer interop.
    let value: ciborium::Value = ciborium::de::from_reader(&encoded[..]).expect("decode as value");

    // Top-level: adjacently-tagged MembershipEventKind → map with "tg" + "vl" keys.
    let outer_map = value.as_map().expect("top level is a CBOR map");
    assert_eq!(
        outer_map.len(),
        2,
        "adjacent tag → exactly two outer keys (tg + vl)"
    );
    assert!(
        outer_map.iter().any(|(k, _)| k.as_text() == Some("tg")),
        "outer map missing \"tg\" key"
    );
    let vl_entry = outer_map.iter().find(|(k, _)| k.as_text() == Some("vl"));
    let inner_value = &vl_entry.expect("outer map missing \"vl\" key").1;
    assert_eq!(
        outer_map
            .iter()
            .find(|(k, _)| k.as_text() == Some("tg"))
            .and_then(|(_, v)| v.as_text()),
        Some("q"),
        "AdminProposal variant tag should be \"q\""
    );

    // Inner: AdminProposal struct-variant content → map with key "pk".
    let inner_map = inner_value.as_map().expect("AdminProposal value is a map");
    assert!(
        inner_map.iter().any(|(k, _)| k.as_text() == Some("pk")),
        "AdminProposal inner map missing \"pk\" key"
    );
}

#[test]
fn admin_proposal_kick_canonical_cbor() {
    let kind = MembershipEventKind::AdminProposal {
        proposal_kind: ProposalKind::Kick {
            target: FIXTURE_TARGET_ADDR,
            reason: Some("violated rules".to_string()),
        },
    };
    let encoded = canonical_cbor_encode(&kind).expect("encode");
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ADMIN_PROPOSAL_KICK_HEX.contains("FILL_AFTER") {
        panic!(
            "REGENERATE EXPECTED_ADMIN_PROPOSAL_KICK_HEX = \"{}\";",
            actual_hex
        );
    }
    assert_eq!(
        actual_hex, EXPECTED_ADMIN_PROPOSAL_KICK_HEX,
        "AdminProposal+Kick wire format changed"
    );
}

#[test]
fn admin_proposal_change_quorum_canonical_cbor() {
    let kind = MembershipEventKind::AdminProposal {
        proposal_kind: ProposalKind::ChangeQuorum { new_quorum: 3 },
    };
    let encoded = canonical_cbor_encode(&kind).expect("encode");
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ADMIN_PROPOSAL_CHANGE_QUORUM_HEX.contains("FILL_AFTER") {
        panic!(
            "REGENERATE EXPECTED_ADMIN_PROPOSAL_CHANGE_QUORUM_HEX = \"{}\";",
            actual_hex
        );
    }
    assert_eq!(
        actual_hex, EXPECTED_ADMIN_PROPOSAL_CHANGE_QUORUM_HEX,
        "AdminProposal+ChangeQuorum wire format changed"
    );
}

#[test]
fn admin_countersign_canonical_cbor() {
    let kind = MembershipEventKind::AdminCountersign {
        target_event_id: FIXTURE_TARGET_EVENT_ID,
    };
    let encoded = canonical_cbor_encode(&kind).expect("encode");
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ADMIN_COUNTERSIGN_HEX.contains("FILL_AFTER") {
        panic!(
            "REGENERATE EXPECTED_ADMIN_COUNTERSIGN_HEX = \"{}\";",
            actual_hex
        );
    }
    assert_eq!(
        actual_hex, EXPECTED_ADMIN_COUNTERSIGN_HEX,
        "AdminCountersign wire format changed"
    );

    let value: ciborium::Value = ciborium::de::from_reader(&encoded[..]).expect("decode as value");

    // Top-level: adjacently-tagged MembershipEventKind → map with "tg" + "vl" keys.
    let outer_map = value.as_map().expect("top level is a CBOR map");
    assert_eq!(
        outer_map.len(),
        2,
        "adjacent tag → exactly two outer keys (tg + vl)"
    );
    assert_eq!(
        outer_map
            .iter()
            .find(|(k, _)| k.as_text() == Some("tg"))
            .and_then(|(_, v)| v.as_text()),
        Some("n"),
        "AdminCountersign variant tag should be \"n\""
    );
    let vl_entry = outer_map.iter().find(|(k, _)| k.as_text() == Some("vl"));
    let inner_value = &vl_entry.expect("outer map missing \"vl\" key").1;

    // Inner: AdminCountersign struct-variant content → map with key "ti".
    let inner_map = inner_value
        .as_map()
        .expect("AdminCountersign value is a map");
    assert!(
        inner_map.iter().any(|(k, _)| k.as_text() == Some("ti")),
        "AdminCountersign inner map missing \"ti\" key"
    );
}

#[test]
fn community_state_with_admin_quorum_canonical_cbor() {
    let mut state = CommunityState::new(FIXTURE_COMMUNITY_ID);
    state.admin_quorum = 3;
    let encoded = canonical_cbor_encode(&state).expect("encode");
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_COMMUNITY_STATE_WITH_ADMIN_QUORUM_HEX.contains("FILL_AFTER") {
        panic!(
            "REGENERATE EXPECTED_COMMUNITY_STATE_WITH_ADMIN_QUORUM_HEX = \"{}\";",
            actual_hex
        );
    }
    assert_eq!(
        actual_hex, EXPECTED_COMMUNITY_STATE_WITH_ADMIN_QUORUM_HEX,
        "CommunityState with admin_quorum != 1 wire format changed"
    );

    // Structural sanity: the "aq" key must appear when admin_quorum != 1.
    let value: ciborium::Value = ciborium::de::from_reader(&encoded[..]).expect("decode as value");
    let map = value.as_map().expect("outer is map");
    assert!(
        map.iter().any(|(k, _)| k.as_text() == Some("aq")),
        "CommunityState with non-default admin_quorum should emit 'aq' key"
    );
}

#[test]
fn community_state_default_quorum_omits_aq_key() {
    // Byte-compat with pre-ZEB-250 communities: admin_quorum == 1 must
    // NOT emit the "aq" key. Encoding is byte-identical to a state
    // serialized before ZEB-250 existed.
    let state = CommunityState::new(FIXTURE_COMMUNITY_ID);
    assert_eq!(state.admin_quorum, 1, "default admin_quorum must be 1");

    let encoded = canonical_cbor_encode(&state).expect("encode");
    let value: ciborium::Value = ciborium::de::from_reader(&encoded[..]).expect("decode as value");
    let map = value.as_map().expect("outer is map");
    assert!(
        !map.iter().any(|(k, _)| k.as_text() == Some("aq")),
        "CommunityState with default admin_quorum=1 must omit 'aq' key (byte-compat)"
    );
}
