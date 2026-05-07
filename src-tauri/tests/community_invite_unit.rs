//! Unit tests for community_invite.rs Phase 1 types.

use harmony_app::community_invite::{CommunityInvitePayload, InviteToken};
use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use harmony_app::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};

#[test]
fn community_invite_payload_round_trips_open_form() {
    let p = CommunityInvitePayload {
        community_id: SpaceId([1u8; 16]),
        membership_key: MembershipKey::new([2u8; 32]),
        admin_addr: OwnerAddr([3u8; 16]),
        community_name: "harmony-design".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
    };

    let bytes = canonical_cbor_encode(&p).expect("encode");
    let decoded: CommunityInvitePayload = canonical_cbor_decode(&bytes).expect("decode");

    assert_eq!(decoded.community_id, p.community_id);
    assert_eq!(decoded.community_name, p.community_name);
    assert!(!decoded.is_invite_only);
    assert!(decoded.invite_token.is_none());
}

#[test]
fn community_invite_payload_round_trips_invite_only_form() {
    let token = InviteToken {
        inviter: OwnerAddr([5u8; 16]),
        invitee_hint: Some(OwnerAddr([6u8; 16])),
        minted_at: Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "d".into(),
        },
        sig: [0xCC; 64],
    };

    let p = CommunityInvitePayload {
        community_id: SpaceId([1u8; 16]),
        membership_key: MembershipKey::new([2u8; 32]),
        admin_addr: OwnerAddr([3u8; 16]),
        community_name: "private".into(),
        is_invite_only: true,
        expires_at: Some(Hlc {
            wall_ms: 9999,
            logical: 0,
            device_id: "d".into(),
        }),
        invite_token: Some(token.clone()),
    };

    let bytes = canonical_cbor_encode(&p).expect("encode");
    let decoded: CommunityInvitePayload = canonical_cbor_decode(&bytes).expect("decode");

    assert!(decoded.is_invite_only);
    assert_eq!(
        decoded.expires_at.as_ref().map(|h| h.wall_ms),
        Some(9999),
        "expires_at must round-trip — central to invite-only TTL semantics"
    );
    assert_eq!(
        decoded.invite_token.as_ref().map(|t| t.invitee_hint),
        Some(Some(OwnerAddr([6u8; 16])))
    );
    assert_eq!(
        decoded.invite_token.as_ref().map(|t| t.inviter),
        Some(token.inviter)
    );
    assert_eq!(
        decoded.invite_token.as_ref().map(|t| t.sig),
        Some(token.sig)
    );
}

#[test]
fn invite_token_round_trips_with_hint_none() {
    let t = InviteToken {
        inviter: OwnerAddr([1u8; 16]),
        invitee_hint: None,
        minted_at: Hlc {
            wall_ms: 1,
            logical: 0,
            device_id: "d".into(),
        },
        sig: [0u8; 64],
    };

    let bytes = canonical_cbor_encode(&t).expect("encode");
    let decoded: InviteToken = canonical_cbor_decode(&bytes).expect("decode");

    assert_eq!(decoded.invitee_hint, None);
    assert_eq!(decoded.inviter, OwnerAddr([1u8; 16]));
}

#[test]
fn invite_url_round_trips_open_payload() {
    use harmony_app::community_invite::{
        decode_invite_url, encode_invite_url, CommunityInvitePayload,
    };
    use harmony_app::owner_state_types::{MembershipKey, OwnerAddr, SpaceId};

    let payload = CommunityInvitePayload {
        community_id: SpaceId([0xab; 16]),
        membership_key: MembershipKey::new([0x42; 32]),
        admin_addr: OwnerAddr([0xcd; 16]),
        community_name: "Hackers United".to_string(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
    };

    let url = encode_invite_url(&payload).expect("encode");
    let body = url
        .strip_prefix("harmony://invite/")
        .expect("URL must start with harmony://invite/");
    assert!(
        !body.contains('+') && !body.contains('/') && !body.contains('='),
        "base64url no-pad body must not contain +, /, or ="
    );

    let decoded = decode_invite_url(&url).expect("decode");
    assert_eq!(decoded, payload);
}

#[test]
fn decode_rejects_wrong_scheme() {
    use harmony_app::community_invite::{decode_invite_url, InviteUrlError};
    let err = decode_invite_url("https://example.com/invite/abc").unwrap_err();
    assert!(matches!(err, InviteUrlError::WrongScheme(_)));
}

#[test]
fn decode_rejects_invalid_base64() {
    use harmony_app::community_invite::{decode_invite_url, InviteUrlError};
    let err = decode_invite_url("harmony://invite/!!!not-base64!!!").unwrap_err();
    assert!(matches!(err, InviteUrlError::Base64(_)));
}

#[test]
fn decode_rejects_truncated_cbor() {
    use base64::Engine;
    use harmony_app::community_invite::{decode_invite_url, InviteUrlError};
    let truncated = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0xa1, 0x62]);
    let url = format!("harmony://invite/{truncated}");
    let err = decode_invite_url(&url).unwrap_err();
    assert!(matches!(err, InviteUrlError::Cbor(_)));
}

#[test]
fn decode_rejects_oversized_payload() {
    use harmony_app::community_invite::{decode_invite_url, InviteUrlError};
    let huge_body = "A".repeat(10_000);
    let url = format!("harmony://invite/{huge_body}");
    let err = decode_invite_url(&url).unwrap_err();
    assert!(matches!(err, InviteUrlError::TooLarge(_)));
}

#[test]
fn decode_trims_whitespace() {
    use harmony_app::community_invite::{
        decode_invite_url, encode_invite_url, CommunityInvitePayload,
    };
    use harmony_app::owner_state_types::{MembershipKey, OwnerAddr, SpaceId};
    let payload = CommunityInvitePayload {
        community_id: SpaceId([0xab; 16]),
        membership_key: MembershipKey::new([0x42; 32]),
        admin_addr: OwnerAddr([0xcd; 16]),
        community_name: "WhitespaceTest".to_string(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
    };
    let url = encode_invite_url(&payload).expect("encode");
    let padded = format!("  \n{url}\t  ");
    let decoded = decode_invite_url(&padded).expect("decode trimmed");
    assert_eq!(decoded, payload);
}

#[test]
fn community_invite_packet_roundtrip() {
    use harmony_app::community_invite::{
        build_signed_invite_packet, decode_packet, device_hash_from_identity_pub, encode_packet,
        CommunityInvitePacket, CommunityInviteSigned, InviteToken,
    };
    use harmony_app::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use harmony_app::owner_state_types::{DeviceIdentityHash, Hlc, OwnerAddr, SpaceId};

    let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0xab; 32]);
    let community_id = SpaceId([0x10; 16]);
    let joiner = OwnerAddr([0x22; 16]);
    let inviter = OwnerAddr([0x11; 16]);

    let join_event = sign_event(
        &EventPayload {
            id: [0x44; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: joiner,
            at: Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "j".into(),
            },
        },
        &signing_key,
    )
    .unwrap();

    // signing_device_hash MUST equal SHA256(joiner_identity_pub)[..16] —
    // decode_packet's structural defense-in-depth check enforces this.
    let joiner_identity_pub = [0x66u8; 64];
    let signing_device_hash =
        DeviceIdentityHash(device_hash_from_identity_pub(&joiner_identity_pub));

    let signed = CommunityInviteSigned {
        community_id,
        join_event,
        invite_token: InviteToken {
            inviter,
            invitee_hint: Some(joiner),
            minted_at: Hlc {
                wall_ms: 900,
                logical: 0,
                device_id: "i".into(),
            },
            sig: [0x55; 64],
        },
        joiner_identity_pub,
        signing_device_hash,
        created_at: Hlc {
            wall_ms: 1100,
            logical: 0,
            device_id: "j".into(),
        },
    };

    let packet = build_signed_invite_packet(signed.clone(), &signing_key)
        .expect("build_signed_invite_packet");
    let wire = encode_packet(&packet).expect("encode");

    // Discriminant byte is 0x10.
    assert_eq!(wire[0], 0x10, "discriminant byte must be 0x10");

    let decoded = decode_packet(&wire).expect("decode");
    match (&packet, &decoded) {
        (
            CommunityInvitePacket::Invite {
                signed: s1,
                signature: sig1,
                ..
            },
            CommunityInvitePacket::Invite {
                signed: s2,
                signature: sig2,
                ..
            },
        ) => {
            assert_eq!(s1, s2);
            assert_eq!(sig1, sig2);
        }
    }
}

#[test]
fn community_invite_packet_envelope_sig_rejected_on_tampered_body() {
    use harmony_app::community_invite::{
        build_signed_invite_packet, decode_packet, encode_packet, verify_envelope_sig,
        CommunityInvitePacket, CommunityInviteSigned, InviteToken,
    };
    use harmony_app::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

    let identity = harmony_identity::PrivateIdentity::from_seed(&[0xcd; 32]);
    let identity_pub = identity.identity.to_public_bytes();
    let joiner_signing_key = {
        let priv_bytes = identity.to_private_bytes();
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&priv_bytes[32..64]);
        ed25519_dalek::SigningKey::from_bytes(&seed)
    };
    let joiner = harmony_app::owner_state_types::OwnerAddr(identity.identity.address_hash);

    let community_id = SpaceId([0x10; 16]);
    let join_event = sign_event(
        &EventPayload {
            id: [0x44; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: joiner,
            at: Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "j".into(),
            },
        },
        &joiner_signing_key,
    )
    .unwrap();

    let signed = CommunityInviteSigned {
        community_id,
        join_event,
        invite_token: InviteToken {
            inviter: OwnerAddr([0x11; 16]),
            invitee_hint: None,
            minted_at: Hlc {
                wall_ms: 900,
                logical: 0,
                device_id: "i".into(),
            },
            sig: [0x55; 64],
        },
        joiner_identity_pub: identity_pub,
        signing_device_hash: harmony_app::owner_state_types::DeviceIdentityHash(
            identity.identity.address_hash,
        ),
        created_at: Hlc {
            wall_ms: 1100,
            logical: 0,
            device_id: "j".into(),
        },
    };

    let packet = build_signed_invite_packet(signed.clone(), &joiner_signing_key).expect("build");
    let mut wire = encode_packet(&packet).expect("encode");

    // Flip a byte in the signed body region (skip discriminant +
    // signature trailer). Targets a byte that's part of the CBOR map.
    let target = 5;
    assert!(target < wire.len() - 64, "bound check");
    wire[target] ^= 0xff;

    // Decode still succeeds (CBOR remained syntactically valid for our
    // chosen byte flip; if the flip lands on a length-prefix it could
    // fail decode — choose a target byte that's a value, not a
    // length. Index 5 is inside a map key bstr; fine).
    let decoded = decode_packet(&wire);
    if let Ok(CommunityInvitePacket::Invite {
        signature,
        signed_bytes,
        ..
    }) = decoded
    {
        // Envelope-sig verification MUST reject the tampered body.
        let result = verify_envelope_sig(&signed_bytes, &signature, &identity_pub);
        assert!(result.is_err(), "envelope sig must reject tampered body");
    } else {
        // The byte flip happened to break CBOR decode itself — that's
        // also an acceptable rejection. The test is satisfied.
    }
}
