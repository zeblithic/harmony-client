//! ZEB-254: Byte-pinned canonical CBOR fixtures for PendingJoin,
//! JoinCountersign, MemberStatus::PendingJoin, Space.pending_join_at.
//!
//! These tests lock the canonical-CBOR wire encoding for the new ZEB-254
//! types. Any failure here is a wire-protocol break — review carefully
//! before updating the pinned bytes (cross-version compat, peer interop).
//!
//! Uses deterministic test bytes (zero or repeated-byte sigs) so the
//! encoded bytes are byte-stable across runs. The tests do NOT verify
//! cryptographic validity — they pin BYTE LAYOUT only.

use harmony_app::community_invite::InviteToken;
use harmony_app::community_membership::{MemberStatus, MembershipEventKind};
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, Space, SpaceId, SpaceKind};
use std::collections::BTreeMap;

// Deterministic synthetic values for fixture stability.
const FIXTURE_ADMIN_ADDR: OwnerAddr = OwnerAddr([0x11; 16]);
const FIXTURE_JOINER_ADDR: OwnerAddr = OwnerAddr([0x22; 16]);
const FIXTURE_COMMUNITY: SpaceId = SpaceId([0x33; 16]);
const FIXTURE_TARGET_EVENT_ID: [u8; 16] = [0x66; 16];
const FIXTURE_MINTED_AT_MS: u64 = 1_700_000_000_000;
const FIXTURE_EXPIRES_AT_MS: u64 = 1_700_000_604_800_000;

fn synth_invite_token() -> InviteToken {
    InviteToken {
        inviter: FIXTURE_ADMIN_ADDR,
        invitee_hint: Some(FIXTURE_JOINER_ADDR),
        minted_at: Hlc {
            wall_ms: FIXTURE_MINTED_AT_MS,
            logical: 0,
            device_id: "admin".to_string(),
        },
        expires_at: Some(FIXTURE_EXPIRES_AT_MS),
        sig: [0x44; 64],
    }
}

// ZEB-339: the InviteToken no longer carries the joiner `jp` (joiner_pub)
// field — joiner identity now rides in the invite envelope, and membership
// signer resolution uses the enrolled-device cert / materialized keys. The
// canonical encoding dropped that 64-byte field (2-key inner map → 1-key).
const EXPECTED_PENDING_JOIN_HEX: &str = "a2627467616762766ca1626974a562697650111111111111111111111111111111116269685022222222222222222222222222222222626d74a361771b0000018bcfe56800616c0061646561646d696e6278611b00060a243c2ac400627367584044444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444";
const EXPECTED_JOIN_COUNTERSIGN_HEX: &str =
    "a2627467617962766ca16274675066666666666666666666666666666666";
const EXPECTED_MEMBER_STATUS_PENDING_JOIN_HEX: &str = "6170";
const EXPECTED_SPACE_PENDING_JOIN_HEX: &str = "b06269645033333333333333333333333333333333626b6e6163627061f6626369f6626e6d716669787475726520636f6d6d756e697479627472f6626d658062636ef6626e70f6626c61f6626361a361771b0000018bcfe56800616c0061646561646d696e627561a361771b0000018bcfe56800616c0061646561646d696e62636500626164501111111111111111111111111111111162696ff562706aa361771b0000018bcfe569f4616c006164666a6f696e6572";

#[test]
fn pending_join_canonical_cbor_pinned() {
    let kind = MembershipEventKind::PendingJoin {
        invite_token: synth_invite_token(),
    };
    let encoded = canonical_cbor_encode(&kind).expect("encode");
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_PENDING_JOIN_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_PENDING_JOIN_HEX = \"{}\";", actual_hex);
    }
    assert_eq!(
        actual_hex, EXPECTED_PENDING_JOIN_HEX,
        "PendingJoin wire format changed"
    );
}

#[test]
fn join_countersign_canonical_cbor_pinned() {
    let kind = MembershipEventKind::JoinCountersign {
        target_event_id: FIXTURE_TARGET_EVENT_ID,
    };
    let encoded = canonical_cbor_encode(&kind).expect("encode");
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_JOIN_COUNTERSIGN_HEX.contains("FILL_AFTER") {
        panic!(
            "REGENERATE EXPECTED_JOIN_COUNTERSIGN_HEX = \"{}\";",
            actual_hex
        );
    }
    assert_eq!(
        actual_hex, EXPECTED_JOIN_COUNTERSIGN_HEX,
        "JoinCountersign wire format changed"
    );
}

#[test]
fn member_status_pending_join_canonical_cbor_pinned() {
    let status = MemberStatus::PendingJoin;
    let encoded = canonical_cbor_encode(&status).expect("encode");
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_MEMBER_STATUS_PENDING_JOIN_HEX.contains("FILL_AFTER") {
        panic!(
            "REGENERATE EXPECTED_MEMBER_STATUS_PENDING_JOIN_HEX = \"{}\";",
            actual_hex
        );
    }
    assert_eq!(
        actual_hex, EXPECTED_MEMBER_STATUS_PENDING_JOIN_HEX,
        "MemberStatus::PendingJoin wire format changed"
    );
}

#[test]
fn space_with_pending_join_at_canonical_cbor_pinned() {
    let space = Space {
        id: FIXTURE_COMMUNITY,
        kind: SpaceKind::Community,
        parent: None,
        community_id: None,
        name: "fixture community".to_string(),
        transport: None,
        members: vec![],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc {
            wall_ms: FIXTURE_MINTED_AT_MS,
            logical: 0,
            device_id: "admin".to_string(),
        },
        updated_at: Hlc {
            wall_ms: FIXTURE_MINTED_AT_MS,
            logical: 0,
            device_id: "admin".to_string(),
        },
        content_key: None,
        prior_content_keys: vec![],
        current_epoch: Some(0),
        current_epoch_key: None,
        old_epoch_keys: BTreeMap::new(),
        admin_addr: Some(FIXTURE_ADMIN_ADDR),
        is_invite_only: Some(true),
        shared_in_profile: false,
        read_receipt_pref: None,
        pending_join_at: Some(Hlc {
            wall_ms: FIXTURE_MINTED_AT_MS + 500,
            logical: 0,
            device_id: "joiner".to_string(),
        }),
    };
    let encoded = canonical_cbor_encode(&space).expect("encode");
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_SPACE_PENDING_JOIN_HEX.contains("FILL_AFTER") {
        panic!(
            "REGENERATE EXPECTED_SPACE_PENDING_JOIN_HEX = \"{}\";",
            actual_hex
        );
    }
    assert_eq!(
        actual_hex, EXPECTED_SPACE_PENDING_JOIN_HEX,
        "Space.pending_join_at wire format changed"
    );
}
