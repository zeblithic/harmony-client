//! Golden CBOR fixtures for ZEB-1031 D-FROST committee-reset membership
//! wire types (`DfrostResetProposal` tag "o", `DfrostResetCosign` tag
//! "w", `DfrostResetResponse` tag "z"). Pinned bytes prevent silent
//! wire-format changes — if any of these tests fail, treat it as a
//! wire-protocol break and review carefully (cross-version
//! compatibility, peer interop, etc.).
//!
//! Mirrors src-tauri/tests/wire_format/community_fixtures.rs and
//! reachability_announce_fixtures.rs.

use harmony_app::community_membership::{
    MembershipEventKind, ResetVerdict, SignedMembershipEvent, RESET_VETO_WINDOW_FLOOR_MS,
};
use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

fn fixture_hlc() -> Hlc {
    Hlc {
        wall_ms: 1_700_000_000_000,
        logical: 0,
        device_id: "fix".into(),
    }
}

fn fixture_signed_event(kind: MembershipEventKind) -> SignedMembershipEvent {
    SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x42; 16],
        community_id: SpaceId([0x37; 16]),
        kind,
        actor: OwnerAddr([0x11; 16]),
        at: fixture_hlc(),
        sig: [0xBB; 64],
        countersig: None,
        enrollment: None,
    }
}

/// Structural CBOR-key check: confirm a `MembershipEventKind` payload's
/// variant fields use 2-char same-length keys. `bytes` is the canonical
/// encoding of a bare `MembershipEventKind`, which is adjacently tagged
/// (`#[serde(tag = "tg", content = "vl")]`) — `{"tg": "<code>", "vl":
/// {<fields>}}`. This walks past the `tg`/`vl` wrapper into the `vl`
/// field map before checking keys. Mirrors
/// `voting_tier3_fixtures.rs::assert_two_char_keys`.
fn assert_two_char_keys(bytes: &[u8], expected_keys: &[&str]) {
    let value: ciborium::Value = ciborium::de::from_reader(bytes).expect("decode as Value");
    let outer = value.as_map().expect("top-level is a CBOR map");
    let (_, content) = outer
        .iter()
        .find(|(k, _)| k.as_text() == Some("vl"))
        .expect("adjacently-tagged enum must carry a \"vl\" content entry");
    let map = content.as_map().expect("variant payload is a CBOR map");
    let actual_keys: Vec<&str> = map.iter().filter_map(|(k, _)| k.as_text()).collect();
    for k in expected_keys {
        assert!(actual_keys.contains(k), "expected key {k:?} missing");
    }
    for k in &actual_keys {
        assert_eq!(k.len(), 2, "key {k:?} violates 2-char invariant");
    }
}

#[test]
fn signed_event_dfrost_reset_proposal_wire_bytes_pinned() {
    let event = fixture_signed_event(MembershipEventKind::DfrostResetProposal {
        target_vk: [0xA1; 32],
        target_epoch: 7,
        new_members: vec![OwnerAddr([0x10; 16]), OwnerAddr([0x20; 16])],
        new_threshold: 2,
        veto_window_ms: RESET_VETO_WINDOW_FLOOR_MS,
    });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let decoded: SignedMembershipEvent = canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(decoded, event, "roundtrip identity");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_dfrost_reset_proposal hex: {hex}");
    assert_eq!(
        hex,
        "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea2627467616f62766ca56274765820a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a162746507626e6d8250101010101010101010101010101010105020202020202020202020202020202020626e74026276771a05265c006261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "DfrostResetProposal wire format changed"
    );
    assert_two_char_keys(
        &canonical_cbor_encode(&event.kind).expect("encode kind"),
        &["tv", "te", "nm", "nt", "vw"],
    );
}

#[test]
fn signed_event_dfrost_reset_cosign_wire_bytes_pinned() {
    let event = fixture_signed_event(MembershipEventKind::DfrostResetCosign {
        target_event_id: [0x30; 16],
    });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let decoded: SignedMembershipEvent = canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(decoded, event, "roundtrip identity");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_dfrost_reset_cosign hex: {hex}");
    assert_eq!(
        hex,
        "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea2627467617762766ca162746950303030303030303030303030303030306261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "DfrostResetCosign wire format changed"
    );
    assert_two_char_keys(
        &canonical_cbor_encode(&event.kind).expect("encode kind"),
        &["ti"],
    );
}

#[test]
fn signed_event_dfrost_reset_response_endorse_wire_bytes_pinned() {
    let event = fixture_signed_event(MembershipEventKind::DfrostResetResponse {
        target_event_id: [0x30; 16],
        verdict: ResetVerdict::Endorse,
        group_sig: [0xCC; 64],
        new_vk: None,
    });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let decoded: SignedMembershipEvent = canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(decoded, event, "roundtrip identity");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_dfrost_reset_response_endorse hex: {hex}");
    assert_eq!(
        hex,
        "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea2627467617a62766ca3627469503030303030303030303030303030303062766461656273675840cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc6261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "DfrostResetResponse (endorse, new_vk absent) wire format changed"
    );
    // new_vk is None => skip_serializing_if elides "nv" entirely.
    assert_two_char_keys(
        &canonical_cbor_encode(&event.kind).expect("encode kind"),
        &["ti", "vd", "sg"],
    );
}

#[test]
fn signed_event_dfrost_reset_response_veto_wire_bytes_pinned() {
    let event = fixture_signed_event(MembershipEventKind::DfrostResetResponse {
        target_event_id: [0x30; 16],
        verdict: ResetVerdict::Veto,
        group_sig: [0xCC; 64],
        new_vk: None,
    });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let decoded: SignedMembershipEvent = canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(decoded, event, "roundtrip identity");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_dfrost_reset_response_veto hex: {hex}");
    assert_eq!(
        hex,
        "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea2627467617a62766ca3627469503030303030303030303030303030303062766461766273675840cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc6261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "DfrostResetResponse (veto, new_vk absent) wire format changed"
    );
    // new_vk is None => skip_serializing_if elides "nv" entirely.
    assert_two_char_keys(
        &canonical_cbor_encode(&event.kind).expect("encode kind"),
        &["ti", "vd", "sg"],
    );
}

#[test]
fn signed_event_dfrost_reset_response_consumed_wire_bytes_pinned() {
    let event = fixture_signed_event(MembershipEventKind::DfrostResetResponse {
        target_event_id: [0x30; 16],
        verdict: ResetVerdict::Consumed,
        group_sig: [0xCC; 64],
        new_vk: Some([0xC1; 32]),
    });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let decoded: SignedMembershipEvent = canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(decoded, event, "roundtrip identity");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_dfrost_reset_response_consumed hex: {hex}");
    assert_eq!(
        hex,
        "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea2627467617a62766ca4627469503030303030303030303030303030303062766461636273675840cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc626e765820c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c16261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "DfrostResetResponse (consumed, new_vk present) wire format changed"
    );
    // new_vk is Some => "nv" is present alongside the other 3 keys.
    assert_two_char_keys(
        &canonical_cbor_encode(&event.kind).expect("encode kind"),
        &["ti", "vd", "sg", "nv"],
    );
}
