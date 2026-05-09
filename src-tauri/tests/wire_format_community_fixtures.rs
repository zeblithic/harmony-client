//! Golden CBOR fixtures for ZEB-217 Sub-C Phase 1 wire types.
//! Pinned bytes prevent silent wire-format changes — if any of these
//! tests fail, treat it as a wire-protocol break and review carefully
//! (cross-version compatibility, peer interop, etc.).
//!
//! Mirrors src-tauri/tests/wire_format_fixture.rs (owner-state).

use harmony_app::community_invite::{CommunityInvitePayload, InviteToken};
use harmony_app::community_membership::{
    ChannelId, CounterSignature, MembershipEventKind, SignedMembershipEvent,
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
        admin_bootstrap: None,
        admin_identity_pub: None,
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
        expires_at: None,
        sig: [0xDD; 64],
    };

    // Synthetic admin bootstrap with all-deterministic bytes so the
    // encoded payload is reproducible. NOT a real signature — this test
    // pins canonical wire bytes only.
    let admin_bootstrap = SignedMembershipEvent {
        id: [0xCC; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::Join,
        actor: OwnerAddr([0x11; 16]),
        at: fixture_hlc(),
        sig: [0xEE; 64],
        countersig: None,
    };

    let p = CommunityInvitePayload {
        community_id: SpaceId([0x37; 16]),
        membership_key: MembershipKey::new([0xAA; 32]),
        admin_addr: OwnerAddr([0x11; 16]),
        community_name: "fix".into(),
        is_invite_only: true,
        expires_at: Some(fixture_hlc()),
        invite_token: Some(token),
        admin_bootstrap: Some(admin_bootstrap),
        admin_identity_pub: Some([0xAB; 64]),
    };
    let bytes = canonical_cbor_encode(&p).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("community_invite_payload_invite_only hex: {hex}");

    assert_eq!(
        hex,
        "a96263695037373737373737373737373737373737626d6b5820aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa6261645011111111111111111111111111111111626e6d6366697862696ff5626578a361771b0000018bcfe56800616c0061646366697862746ba462697650111111111111111111111111111111116269685022222222222222222222222222222222626d74a361771b0000018bcfe56800616c006164636669786273675840dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd626162a662696450cccccccccccccccccccccccccccccccc6263695037373737373737373737373737373737626b6ea1627467616a6261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee6261705840abababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababab",
        "CommunityInvitePayload (invite-only) wire format changed"
    );
}

#[test]
fn invite_token_targeted_wire_bytes_pinned() {
    let t = InviteToken {
        inviter: OwnerAddr([0x11; 16]),
        invitee_hint: Some(OwnerAddr([0x22; 16])),
        minted_at: fixture_hlc(),
        expires_at: None,
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
        expires_at: None,
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

/// ZEB-262 Phase 4: pin the CommunityInviteSigned canonical CBOR bytes.
/// Mirrors community_membership_signed_event_canonical_roundtrip — the
/// fixture catches encoder drift across phases.
///
/// Re-run with `cargo test community_invite_signed_wire_bytes_pinned`
/// and update the pinned bytes IFF a deliberate wire-format change is
/// shipping. Pinned bytes diverging from the encoder output is a
/// regression — debug before regen.
#[test]
fn community_invite_signed_wire_bytes_pinned() {
    use harmony_app::community_invite::CommunityInviteSigned;
    use harmony_app::community_invite::InviteToken;
    use harmony_app::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
    use harmony_app::owner_state_types::{DeviceIdentityHash, Hlc, OwnerAddr, SpaceId};

    let community_id = SpaceId([0x10; 16]);
    let inviter = OwnerAddr([0x11; 16]);
    let joiner = OwnerAddr([0x22; 16]);

    // Build a Join event; sign with a deterministic test key so the
    // pinned bytes are stable.
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x33; 32]);
    let join_event = sign_event(
        &EventPayload {
            id: [0x44; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: joiner,
            at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "joiner-dev".into(),
            },
        },
        &signing_key,
    )
    .unwrap();

    // Build an InviteToken (sig is just deterministic test bytes —
    // wire-format pin doesn't validate the sig).
    let invite_token = InviteToken {
        inviter,
        invitee_hint: Some(joiner),
        minted_at: Hlc {
            wall_ms: 1_699_000_000_000,
            logical: 0,
            device_id: "inviter-dev".into(),
        },
        expires_at: None,
        sig: [0x55; 64],
    };

    let signed = CommunityInviteSigned {
        community_id,
        join_event,
        invite_token,
        joiner_identity_pub: [0x66; 64],
        signing_device_hash: DeviceIdentityHash([0x77; 16]),
        created_at: Hlc {
            wall_ms: 1_700_000_001_000,
            logical: 0,
            device_id: "joiner-dev".into(),
        },
    };

    let bytes = canonical_cbor_encode(&signed).expect("encode");
    let decoded: CommunityInviteSigned = canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(decoded, signed, "roundtrip identity");

    // Pin the canonical bytes byte-for-byte. If this assertion fires
    // with a deliberate wire-format change, regenerate by adding a
    // temporary `println!("{:?}", bytes);`, running with --nocapture,
    // pasting the slice into PINNED, and removing the println. Pinned
    // bytes diverging from the encoder output otherwise is a regression
    // — debug before regen.
    const PINNED: &[u8] = &[
        166, 98, 99, 105, 80, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 98,
        106, 101, 166, 98, 105, 100, 80, 68, 68, 68, 68, 68, 68, 68, 68, 68, 68, 68, 68, 68, 68,
        68, 68, 98, 99, 105, 80, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
        98, 107, 110, 161, 98, 116, 103, 97, 106, 98, 97, 99, 80, 34, 34, 34, 34, 34, 34, 34, 34,
        34, 34, 34, 34, 34, 34, 34, 34, 98, 97, 116, 163, 97, 119, 27, 0, 0, 1, 139, 207, 229, 104,
        0, 97, 108, 0, 97, 100, 106, 106, 111, 105, 110, 101, 114, 45, 100, 101, 118, 98, 115, 103,
        88, 64, 14, 26, 113, 163, 63, 49, 134, 165, 15, 179, 35, 97, 75, 104, 25, 74, 178, 43, 40,
        178, 205, 237, 93, 226, 3, 122, 184, 228, 142, 66, 248, 35, 91, 25, 143, 165, 197, 90, 27,
        214, 35, 204, 226, 37, 197, 253, 62, 181, 184, 129, 20, 132, 91, 114, 131, 79, 78, 184,
        186, 211, 111, 179, 33, 9, 98, 105, 116, 164, 98, 105, 118, 80, 17, 17, 17, 17, 17, 17, 17,
        17, 17, 17, 17, 17, 17, 17, 17, 17, 98, 105, 104, 80, 34, 34, 34, 34, 34, 34, 34, 34, 34,
        34, 34, 34, 34, 34, 34, 34, 98, 109, 116, 163, 97, 119, 27, 0, 0, 1, 139, 148, 74, 158, 0,
        97, 108, 0, 97, 100, 107, 105, 110, 118, 105, 116, 101, 114, 45, 100, 101, 118, 98, 115,
        103, 88, 64, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85,
        85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85,
        85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 85, 98,
        105, 112, 88, 64, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102,
        102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102,
        102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102,
        102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 102, 98, 100, 104, 80,
        119, 119, 119, 119, 119, 119, 119, 119, 119, 119, 119, 119, 119, 119, 119, 119, 98, 99, 97,
        163, 97, 119, 27, 0, 0, 1, 139, 207, 229, 107, 232, 97, 108, 0, 97, 100, 106, 106, 111,
        105, 110, 101, 114, 45, 100, 101, 118,
    ];
    assert_eq!(
        bytes.as_slice(),
        PINNED,
        "CommunityInviteSigned wire format drifted from pinned bytes — \
         debug encoder drift, regen the pin only on a deliberate wire-format change"
    );
}

#[test]
fn signed_event_channel_create_wire_bytes_pinned() {
    let ch_id = ChannelId([0x42; 16]);
    let event = fixture_signed_event(MembershipEventKind::ChannelCreate {
        channel_id: ch_id,
        name: "general".to_string(),
        write_power: 0,
    });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_channel_create hex: {hex}");
    assert_eq!(
        hex,
        "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea2627467616362766ca36263685042424242424242424242424242424242626e6d6767656e6572616c627770006261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "ChannelCreate wire format changed"
    );
}

#[test]
fn signed_event_channel_modify_full_wire_bytes_pinned() {
    let ch_id = ChannelId([0x42; 16]);
    let event = fixture_signed_event(MembershipEventKind::ChannelModify {
        channel_id: ch_id,
        name: Some("renamed".to_string()),
        write_power: Some(50),
    });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_channel_modify_full hex: {hex}");
    assert_eq!(
        hex,
        "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea2627467616d62766ca36263685042424242424242424242424242424242626e6d6772656e616d656462777018326261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "ChannelModify (full) wire format changed"
    );
}

#[test]
fn signed_event_channel_modify_name_only_wire_bytes_pinned() {
    let ch_id = ChannelId([0x42; 16]);
    let event = fixture_signed_event(MembershipEventKind::ChannelModify {
        channel_id: ch_id,
        name: Some("renamed".to_string()),
        write_power: None,
    });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_channel_modify_name_only hex: {hex}");
    assert_eq!(
        hex,
        "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea2627467616d62766ca26263685042424242424242424242424242424242626e6d6772656e616d65646261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "ChannelModify (name-only) wire format changed"
    );
}

#[test]
fn signed_event_channel_modify_power_only_wire_bytes_pinned() {
    let ch_id = ChannelId([0x42; 16]);
    let event = fixture_signed_event(MembershipEventKind::ChannelModify {
        channel_id: ch_id,
        name: None,
        write_power: Some(50),
    });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_channel_modify_power_only hex: {hex}");
    assert_eq!(
        hex,
        "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea2627467616d62766ca2626368504242424242424242424242424242424262777018326261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "ChannelModify (power-only) wire format changed"
    );
}

#[test]
fn signed_event_channel_delete_wire_bytes_pinned() {
    let ch_id = ChannelId([0x42; 16]);
    let event = fixture_signed_event(MembershipEventKind::ChannelDelete { channel_id: ch_id });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_channel_delete hex: {hex}");
    assert_eq!(
        hex,
        "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea2627467616462766ca162636850424242424242424242424242424242426261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "ChannelDelete wire format changed"
    );
}
