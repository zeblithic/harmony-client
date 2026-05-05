//! Golden CBOR fixtures for ZEB-217 Sub-C Phase 1 wire types.
//! Pinned bytes prevent silent wire-format changes — if any of these
//! tests fail, treat it as a wire-protocol break and review carefully
//! (cross-version compatibility, peer interop, etc.).
//!
//! Mirrors src-tauri/tests/wire_format_fixture.rs (owner-state).

use harmony_app::community_invite::{CommunityInvitePayload, InviteToken};
use harmony_app::community_membership::{
    CounterSignature, MembershipEventKind, SignedMembershipEvent,
};
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};

fn fixture_hlc() -> Hlc {
    Hlc {
        wall_ms: 1700000000000,
        logical: 0,
        device_id: "fix".into(),
    }
}

fn fixture_signed_event(kind: MembershipEventKind) -> SignedMembershipEvent {
    SignedMembershipEvent {
        id: [0x42; 16],
        community_id: SpaceId([0x37; 16]),
        kind,
        actor: OwnerAddr([0x11; 16]),
        at: fixture_hlc(),
        sig: [0xBB; 64],
        countersig: None,
    }
}

#[test]
fn membership_key_wire_bytes_pinned() {
    let k = MembershipKey::new([0xAA; 32]);
    let bytes = canonical_cbor_encode(&k).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("membership_key hex: {hex}");
    assert_eq!(
        hex, "5820aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "MembershipKey wire format changed"
    );
}

#[test]
fn signed_event_join_wire_bytes_pinned() {
    let event = fixture_signed_event(MembershipEventKind::Join);
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_join hex: {hex}");
    assert_eq!(hex, "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea1627467616a6261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "Join wire format changed");
}

#[test]
fn signed_event_leave_wire_bytes_pinned() {
    let event = fixture_signed_event(MembershipEventKind::Leave);
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_leave hex: {hex}");
    assert_eq!(hex, "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea1627467616c6261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "Leave wire format changed");
}

#[test]
fn signed_event_invite_wire_bytes_pinned() {
    let target = OwnerAddr([0x99; 16]);
    let event = fixture_signed_event(MembershipEventKind::Invite { target });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_invite hex: {hex}");
    assert_eq!(hex, "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea2627467616962766ca162746750999999999999999999999999999999996261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "Invite wire format changed");
}

#[test]
fn signed_event_kick_no_reason_wire_bytes_pinned() {
    let target = OwnerAddr([0x99; 16]);
    let event = fixture_signed_event(MembershipEventKind::Kick {
        target,
        reason: None,
    });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_kick_no_reason hex: {hex}");
    assert_eq!(
        hex,
        "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea2627467616b62766ca162746750999999999999999999999999999999996261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "Kick (no reason) wire format changed"
    );
}

#[test]
fn signed_event_kick_with_reason_wire_bytes_pinned() {
    let target = OwnerAddr([0x99; 16]);
    let event = fixture_signed_event(MembershipEventKind::Kick {
        target,
        reason: Some("spam".to_string()),
    });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_kick_with_reason hex: {hex}");
    assert_eq!(
        hex,
        "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea2627467616b62766ca26274675099999999999999999999999999999999627273647370616d6261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "Kick (with reason) wire format changed"
    );
}

#[test]
fn signed_event_setpower_wire_bytes_pinned() {
    let target = OwnerAddr([0x99; 16]);
    let event = fixture_signed_event(MembershipEventKind::SetPower { target, level: 50 });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_setpower hex: {hex}");
    assert_eq!(hex, "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea2627467617062766ca26274675099999999999999999999999999999999626c7618326261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "SetPower wire format changed");
}

#[test]
fn countersignature_wire_bytes_pinned() {
    let cs = CounterSignature {
        signer: OwnerAddr([0x77; 16]),
        sig: [0xCC; 64],
    };
    let bytes = canonical_cbor_encode(&cs).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("countersignature hex: {hex}");
    // Wire layout (post-PR#82 round 6 rename):
    //   a2                      ; map(2)
    //   62 73 6e 50 <16 bytes>  ; text(2) "sn" / bstr(16) signer
    //   62 73 67 58 40 <64 b>   ; text(2) "sg" / bstr(64) signature
    //
    // "sn" = signer, "sg" = signature — keeps "sg" semantically pinned
    // to "Ed25519 signature" at every nesting level (was previously
    // signer="sg" / sig="sx", which conflicted with
    // SignedMembershipEvent.sig also using "sg").
    assert_eq!(hex, "a262736e50777777777777777777777777777777776273675840cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc", "CounterSignature wire format changed");
}

#[test]
fn community_invite_payload_open_wire_bytes_pinned() {
    let p = CommunityInvitePayload {
        community_id: SpaceId([0x37; 16]),
        membership_key: MembershipKey::new([0xAA; 32]),
        admin_addr: OwnerAddr([0x11; 16]),
        community_name: "fix".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
    };
    let bytes = canonical_cbor_encode(&p).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("community_invite_payload_open hex: {hex}");
    assert_eq!(
        hex,
        "a56263695037373737373737373737373737373737626d6b5820aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa6261645011111111111111111111111111111111626e6d6366697862696ff4",
        "CommunityInvitePayload (open) wire format changed"
    );
}

#[test]
fn community_invite_payload_invite_only_wire_bytes_pinned() {
    let token = InviteToken {
        inviter: OwnerAddr([0x11; 16]),
        invitee_hint: Some(OwnerAddr([0x22; 16])),
        minted_at: fixture_hlc(),
        sig: [0xDD; 64],
    };
    let p = CommunityInvitePayload {
        community_id: SpaceId([0x37; 16]),
        membership_key: MembershipKey::new([0xAA; 32]),
        admin_addr: OwnerAddr([0x11; 16]),
        community_name: "fix".into(),
        is_invite_only: true,
        expires_at: Some(fixture_hlc()),
        invite_token: Some(token),
    };
    let bytes = canonical_cbor_encode(&p).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("community_invite_payload_invite_only hex: {hex}");
    assert_eq!(
        hex,
        "a76263695037373737373737373737373737373737626d6b5820aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa6261645011111111111111111111111111111111626e6d6366697862696ff5626578a361771b0000018bcfe56800616c0061646366697862746ba462697650111111111111111111111111111111116269685022222222222222222222222222222222626d74a361771b0000018bcfe56800616c006164636669786273675840dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        "CommunityInvitePayload (invite-only) wire format changed"
    );
}

#[test]
fn invite_token_targeted_wire_bytes_pinned() {
    let t = InviteToken {
        inviter: OwnerAddr([0x11; 16]),
        invitee_hint: Some(OwnerAddr([0x22; 16])),
        minted_at: fixture_hlc(),
        sig: [0xDD; 64],
    };
    let bytes = canonical_cbor_encode(&t).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("invite_token_targeted hex: {hex}");
    assert_eq!(
        hex,
        "a462697650111111111111111111111111111111116269685022222222222222222222222222222222626d74a361771b0000018bcfe56800616c006164636669786273675840dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        "InviteToken (targeted) wire format changed"
    );
}

#[test]
fn invite_token_open_wire_bytes_pinned() {
    let t = InviteToken {
        inviter: OwnerAddr([0x11; 16]),
        invitee_hint: None,
        minted_at: fixture_hlc(),
        sig: [0xDD; 64],
    };
    let bytes = canonical_cbor_encode(&t).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("invite_token_open hex: {hex}");
    assert_eq!(
        hex,
        "a36269765011111111111111111111111111111111626d74a361771b0000018bcfe56800616c006164636669786273675840dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        "InviteToken (open) wire format changed"
    );
}
