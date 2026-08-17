//! Golden CBOR fixtures for ZEB-217 Sub-C Phase 1 wire types.
//! Pinned bytes prevent silent wire-format changes — if any of these
//! tests fail, treat it as a wire-protocol break and review carefully
//! (cross-version compatibility, peer interop, etc.).
//!
//! Mirrors src-tauri/tests/wire_format/fixture.rs (owner-state).

use harmony_app::community_invite::{
    CommunityInvitePayload, InviteEpochSnapshot, InviteToken, MaterializedCommunityState,
};
use harmony_app::community_membership::{
    ChannelId, ChannelKind, CounterSignature, MembershipEventKind, SignedMembershipEvent,
};
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};

fn fixture_hlc() -> Hlc {
    Hlc {
        wall_ms: 1700000000000,
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

#[test]
fn membership_key_wire_bytes_pinned() {
    let k = EpochKey::new([0xAA; 32]);
    let bytes = canonical_cbor_encode(&k).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("membership_key hex: {hex}");
    assert_eq!(
        hex, "5820aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "EpochKey wire format changed"
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
fn signed_event_unban_no_reason_wire_bytes_pinned() {
    let target = OwnerAddr([0x99; 16]);
    let event = fixture_signed_event(MembershipEventKind::Unban {
        target,
        reason: None,
    });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_unban_no_reason hex: {hex}");
    assert_eq!(
        hex,
        "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea2627467617562766ca162746750999999999999999999999999999999996261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "Unban (no reason) wire format changed"
    );
}

#[test]
fn signed_event_unban_with_reason_wire_bytes_pinned() {
    let target = OwnerAddr([0x99; 16]);
    let event = fixture_signed_event(MembershipEventKind::Unban {
        target,
        reason: Some("test".to_string()),
    });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_unban_with_reason hex: {hex}");
    assert_eq!(
        hex,
        "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea2627467617562766ca2627467509999999999999999999999999999999962727364746573746261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "Unban (with reason) wire format changed"
    );
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
        inviter_signer_certs: Vec::new(),
        community_id: SpaceId([0x37; 16]),
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: EpochKey::new([0xAA; 32]).as_bytes().to_vec(),
            sealed_epoch_keys: Vec::new(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: OwnerAddr([0x11; 16]),
        community_name: "fix".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        inviter_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: None,
        untargeted_decrypt_key: None,
    };
    let bytes = canonical_cbor_encode(&p).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("community_invite_payload_open hex: {hex}");
    // ZEB-249: wire format updated — "mk" (EpochKey) replaced by "es"
    // (InviteEpochSnapshot). Re-pinned after Task 5 struct change.
    let expected = hex::decode("a56263695037373737373737373737373737373737626573a36265700062736b5820aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa627373a2626d62a062706ca06261645011111111111111111111111111111111626e6d6366697862696ff4").unwrap_or_else(|_| {
        eprintln!("\nACTUAL bytes for open payload pinning:\n  hex = \"{hex}\"\n");
        panic!("update PLACEHOLDER_OPEN with the hex above in wire_format/community_fixtures.rs");
    });
    assert_eq!(
        bytes, expected,
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
        signer_certs: Vec::new(),
        id: [0xCC; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::Join,
        actor: OwnerAddr([0x11; 16]),
        at: fixture_hlc(),
        sig: [0xEE; 64],
        countersig: None,
        enrollment: None,
    };

    let p = CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id: SpaceId([0x37; 16]),
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: EpochKey::new([0xAA; 32]).as_bytes().to_vec(),
            sealed_epoch_keys: Vec::new(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: OwnerAddr([0x11; 16]),
        community_name: "fix".into(),
        is_invite_only: true,
        expires_at: Some(fixture_hlc()),
        invite_token: Some(token),
        admin_bootstrap: Some(admin_bootstrap),
        inviter_identity_pub: Some([0xAB; 64]),
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: None,
        untargeted_decrypt_key: None,
    };
    let bytes = canonical_cbor_encode(&p).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("community_invite_payload_invite_only hex: {hex}");

    // ZEB-249: wire format updated — "mk" (EpochKey) replaced by "es"
    // (InviteEpochSnapshot). Re-pinned after Task 5 struct change.
    let expected = hex::decode("a96263695037373737373737373737373737373737626573a36265700062736b5820aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa627373a2626d62a062706ca06261645011111111111111111111111111111111626e6d6366697862696ff5626578a361771b0000018bcfe56800616c0061646366697862746ba462697650111111111111111111111111111111116269685022222222222222222222222222222222626d74a361771b0000018bcfe56800616c006164636669786273675840dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd626162a662696450cccccccccccccccccccccccccccccccc6263695037373737373737373737373737373737626b6ea1627467616a6261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee6261705840abababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababab").unwrap_or_else(|_| {
        eprintln!("\nACTUAL bytes for invite-only payload pinning:\n  hex = \"{hex}\"\n");
        panic!(
            "update PLACEHOLDER_INVITEONLY with the hex above in wire_format/community_fixtures.rs"
        );
    });
    assert_eq!(
        bytes, expected,
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
        // ZEB-349: an explicit Text kind MUST serialize byte-identically to the
        // pre-ZEB-349 wire (kind is skipped when Text). The original pinned hex
        // below is the byte-identity proof — it is unchanged.
        kind: ChannelKind::Text,
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
fn signed_event_channel_create_voice_wire_bytes_pinned() {
    // ZEB-349: a Voice ChannelCreate differs from the Text fixture above (whose
    // hex is byte-identical to pre-ZEB-349 wire) by exactly the added `ck`->0x01
    // map entry: the inner `c`-payload map header bumps from a3 (ch, nm, wp) to
    // a4 (ch, nm, wp, ck) and `62636b01` (b"ck" -> 0x01) is appended after `wp`.
    // (ciborium preserves serde field-declaration order, so `ck` lands last.)
    let ch_id = ChannelId([0x42; 16]);
    let event = fixture_signed_event(MembershipEventKind::ChannelCreate {
        channel_id: ch_id,
        name: "general".to_string(),
        write_power: 0,
        kind: ChannelKind::Voice,
    });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    assert_eq!(
        hex,
        "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea2627467616362766ca46263685042424242424242424242424242424242626e6d6767656e6572616c6277700062636b016261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "Voice ChannelCreate wire format changed"
    );
}

#[test]
fn signed_event_channel_create_townhall_wire_bytes_pinned() {
    // ZEB-612: a Townhall ChannelCreate differs from the Voice fixture above
    // by exactly the `ck` value byte: `62636b01` (b"ck" -> 0x01) becomes
    // `62636b02` (b"ck" -> 0x02). The Text fixture's unchanged hex remains the
    // byte-identity proof that kind stays omitted for Text.
    let ch_id = ChannelId([0x42; 16]);
    let event = fixture_signed_event(MembershipEventKind::ChannelCreate {
        channel_id: ch_id,
        name: "general".to_string(),
        write_power: 0,
        kind: ChannelKind::Townhall,
    });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    assert_eq!(
        hex,
        "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea2627467616362766ca46263685042424242424242424242424242424242626e6d6767656e6572616c6277700062636b026261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "Townhall ChannelCreate wire format changed"
    );
}

/// ZEB-349 back-compat: a pre-ZEB-349 `ChannelCreate` carries no `ck` key
/// (the field did not exist). Such bytes MUST still decode, with `kind`
/// defaulting to `ChannelKind::Text` (the `default` serde attr's job).
///
/// Mirrors `signed_event_join_certless_back_compat_decodes` (ZEB-339): pin the
/// pre-feature hex (here, the same bytes asserted by
/// `signed_event_channel_create_wire_bytes_pinned`, which is byte-identical to
/// pre-ZEB-349 Text wire), decode it, and assert the new field defaults.
#[test]
fn signed_event_channel_create_back_compat_decodes_text_default() {
    use harmony_app::community_membership::SignedMembershipEvent;
    use harmony_app::owner_state_crypto::canonical_cbor_decode;

    // Identical to the hex asserted by signed_event_channel_create_wire_bytes_pinned.
    // These bytes predate ZEB-349 (no `ck` key in the inner ChannelCreate map).
    let pre_zeb349_text_hex = "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea2627467616362766ca36263685042424242424242424242424242424242626e6d6767656e6572616c627770006261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let bytes = hex::decode(pre_zeb349_text_hex).expect("decode hex");
    let event: SignedMembershipEvent = canonical_cbor_decode(&bytes).expect("back-compat decode");
    match event.kind {
        MembershipEventKind::ChannelCreate { kind, .. } => {
            assert_eq!(
                kind,
                ChannelKind::Text,
                "pre-ZEB-349 ChannelCreate without `ck` key must decode with kind=Text"
            );
        }
        other => panic!("expected ChannelCreate, got {other:?}"),
    }
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

// ── ZEB-339 wire-format fixtures ──────────────────────────────────────────────
//
// Pin: (a) a Join event WITH an EnrollmentCert in the `en` field, and
//      (b) confirm a certless Join (enrollment=None) still encodes without
//          the `en` key + decodes cleanly (backward compat).
//
// Determinism note: `mint_test_owner(seed)` uses:
//   - master_sk = SigningKey::from_bytes(&[seed; 32])  — deterministic
//   - device_sk = SigningKey::from_bytes(&[seed ^ 0xFF; 32])  — deterministic
//   - EnrollmentCert::sign_master signs with ed25519 (RFC 8032 deterministic)
// → the full cert bytes are byte-deterministic for a fixed seed.
// Full-byte pinning is therefore safe and preferred.

/// ZEB-339: a deterministic Join event carrying an EnrollmentCert (`en` key
/// present). Built from `mint_test_owner(0x5A)` so the cert + event sig are
/// byte-deterministic. Pinned bytes prove the `en` field survives a
/// canonical encode → decode round-trip and that the CBOR map now contains
/// the `en` key.
///
/// If this assertion fires after a deliberate wire-format change, rebuild by
/// running the test with `-- --nocapture`, reading the "ACTUAL" line, and
/// updating PINNED below. Wire drift without a deliberate change is a
/// regression — debug before regenerating.
#[test]
fn signed_event_join_with_cert_wire_bytes_pinned() {
    use harmony_app::community_membership::{mint_test_owner, sign_event, EventPayload};
    use harmony_app::owner_state_crypto::canonical_cbor_decode;

    let o = mint_test_owner(0x5A);

    let payload = EventPayload {
        id: [0x5Au8; 16],
        community_id: SpaceId([0x5Bu8; 16]),
        kind: MembershipEventKind::Join,
        actor: o.owner,
        at: Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "zeb339".into(),
        },
    };

    let ev_unsigned = sign_event(&payload, &o.device_key).expect("sign");
    let event = harmony_app::community_membership::SignedMembershipEvent {
        enrollment: Some(o.cert.clone()),
        ..ev_unsigned
    };

    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_join_with_cert hex: {hex}");

    // Round-trip: decode back and verify structural equality.
    let decoded: harmony_app::community_membership::SignedMembershipEvent =
        canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(decoded, event, "round-trip identity");
    assert!(
        decoded.enrollment.is_some(),
        "decoded event must carry enrollment cert"
    );
    assert_eq!(
        decoded.enrollment.as_ref().unwrap().owner_id,
        o.owner.0,
        "decoded cert.owner_id must match actor"
    );
    // Verify the decoded cert still passes structural cert verification.
    decoded
        .enrollment
        .as_ref()
        .unwrap()
        .verify(0)
        .expect("decoded cert must pass cert.verify()");

    // Pin the canonical bytes. ed25519 is RFC 8032 deterministic, and
    // `mint_test_owner` uses fixed-seed keys, so the cert + event sig are
    // byte-deterministic for a given seed. These bytes were captured from
    // a first-run eprintln and pinned here. Regenerate ONLY on a deliberate
    // wire-format change; drift without a deliberate change is a regression.
    const PINNED_HEX: &str = "a7626964505a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a626369505b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b626b6ea1627467616a6261635003f9be208e69c938081b0ec4c5fc8d16626174a361771b0000018bcfe56800616c006164667a65623333396273675840f9a284b7cea8ac457776efd9dd8dc4bf14e8ff7251f8e1ac1a4c4157411e2a46f2d8b4f4f7695a080f4716e5dae871880725f21045e50f609ce5d43f913ba40862656ea86776657273696f6e01686f776e65725f69645003f9be208e69c938081b0ec4c5fc8d16696465766963655f6964509ef262c19db67df0c5bf0d32bf8a0f2b6e6465766963655f7075626b657973a269636c6173736963616ca26e656432353531395f766572696679582029e5833a915a6429a4e3a7948475c338ef436eb82be89c92f059704403db9d556a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6696973737565645f61741a6553f1006a657870697265735f6174f666697373756572a1664d6173746572a16d6d61737465725f7075626b6579a269636c6173736963616ca26e656432353531395f76657269667958200d7550754e0800a5d237eef5826035766b9b3e5a15868a940ab289958788e3b06a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6697369676e617475726558407dae3cbf1e7f31a44634d654434939fdf997aef51cdf1039338aaa91d1aeb306861b2822d47eb72678310617a21d47173e73c1dc06a36f30bd2178673c4e0d08";
    assert_eq!(
        hex, PINNED_HEX,
        "ZEB-339 Join-with-cert wire format changed — \
         debug encoder drift, regen ONLY on deliberate wire-format change"
    );
}

/// ZEB-339 back-compat: a certless Join event (enrollment=None) must still
/// encode WITHOUT the `en` key and decode cleanly.
///
/// Confirms old pre-ZEB-339 events (no `en`/`ek` fields) remain decodeable after
/// the field was added with `skip_serializing_if = "Option::is_none"` and `default`.
/// Mirrors the existing `signed_event_join_wire_bytes_pinned` fixture but explicitly
/// checks absence of `en` and round-trip re-encode identity.
#[test]
fn signed_event_join_certless_back_compat_decodes() {
    use harmony_app::community_membership::SignedMembershipEvent;
    use harmony_app::owner_state_crypto::canonical_cbor_decode;

    // The existing pinned certless-join bytes from signed_event_join_wire_bytes_pinned.
    // These bytes were produced before ZEB-339 (enrollment=None, no `en` key).
    let pinned_certless_hex = "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea1627467616a6261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let bytes = hex::decode(pinned_certless_hex).expect("decode hex");
    let event: SignedMembershipEvent = canonical_cbor_decode(&bytes).expect("certless decode");
    assert_eq!(
        event.enrollment, None,
        "pre-ZEB-339 event without `en` key must decode with enrollment=None"
    );
    assert_eq!(event.kind, MembershipEventKind::Join, "kind must be Join");
    // Re-encode and confirm it round-trips to identical bytes (no `en` key injected).
    let reencoded = canonical_cbor_encode(&event).expect("re-encode");
    assert_eq!(
        reencoded, bytes,
        "certless Join must re-encode to identical bytes (no `en` key injected)"
    );
}
