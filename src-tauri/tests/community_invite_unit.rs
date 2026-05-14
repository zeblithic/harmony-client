//! Unit tests for community_invite.rs Phase 1 types.

use harmony_app::community_invite::{
    CommunityInvitePayload, InviteEpochSnapshot, InviteToken, MaterializedCommunityState,
};
use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};

#[test]
fn community_invite_payload_round_trips_open_form() {
    let p = CommunityInvitePayload {
        community_id: SpaceId([1u8; 16]),
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: EpochKey::new([2u8; 32]).as_bytes().to_vec(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: OwnerAddr([3u8; 16]),
        community_name: "harmony-design".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
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
        expires_at: None,
        sig: [0xCC; 64],
    };

    let p = CommunityInvitePayload {
        community_id: SpaceId([1u8; 16]),
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: EpochKey::new([2u8; 32]).as_bytes().to_vec(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: OwnerAddr([3u8; 16]),
        community_name: "private".into(),
        is_invite_only: true,
        expires_at: Some(Hlc {
            wall_ms: 9999,
            logical: 0,
            device_id: "d".into(),
        }),
        invite_token: Some(token.clone()),
        admin_bootstrap: None,
        admin_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
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
        expires_at: None,
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
    use harmony_app::owner_state_types::{EpochKey, OwnerAddr, SpaceId};

    let payload = CommunityInvitePayload {
        community_id: SpaceId([0xab; 16]),
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: EpochKey::new([0x42; 32]).as_bytes().to_vec(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: OwnerAddr([0xcd; 16]),
        community_name: "Hackers United".to_string(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
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
    // Must exceed MAX_INVITE_BODY_B64_CHARS (85_333 = ≈64 KiB decoded).
    let huge_body = "A".repeat(90_000);
    let url = format!("harmony://invite/{huge_body}");
    let err = decode_invite_url(&url).unwrap_err();
    assert!(matches!(err, InviteUrlError::TooLarge(_)));
}

#[test]
fn decode_trims_whitespace() {
    use harmony_app::community_invite::{
        decode_invite_url, encode_invite_url, CommunityInvitePayload, InviteEpochSnapshot,
        MaterializedCommunityState,
    };
    use harmony_app::owner_state_types::{EpochKey, OwnerAddr, SpaceId};
    let payload = CommunityInvitePayload {
        community_id: SpaceId([0xab; 16]),
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: EpochKey::new([0x42; 32]).as_bytes().to_vec(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: OwnerAddr([0xcd; 16]),
        community_name: "WhitespaceTest".to_string(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
    };
    let url = encode_invite_url(&payload).expect("encode");
    let padded = format!("  \n{url}\t  ");
    let decoded = decode_invite_url(&padded).expect("decode trimmed");
    assert_eq!(decoded, payload);
}

#[test]
fn encode_rejects_invite_only_without_admin_bootstrap() {
    use harmony_app::community_invite::{encode_invite_url, InviteUrlError};
    // Build a known-valid invite-only payload, then mutate ONE field
    // (admin_bootstrap → None). Isolating the failing invariant to a
    // single field protects against assertion order-of-validation drift
    // — if encode_invite_url's check sequence reordered, this test
    // would still pin the InviteOnlyMissingBootstrap path.
    let mut payload = admin_bootstrap_helpers::good_invite_only_payload();
    payload.admin_bootstrap = None;
    assert!(matches!(
        encode_invite_url(&payload).unwrap_err(),
        InviteUrlError::InviteOnlyMissingBootstrap
    ));
}

#[test]
fn encode_rejects_invite_only_without_admin_identity_pub() {
    use harmony_app::community_invite::{encode_invite_url, InviteUrlError};
    // Symmetric to the missing-admin_bootstrap test above: mutate ONE
    // field (admin_identity_pub → None) on top of a known-valid
    // invite-only fixture. Both fields are required for invite-only
    // encoding; this test pins the symmetric branch of the same
    // InviteOnlyMissingBootstrap rejection.
    let mut payload = admin_bootstrap_helpers::good_invite_only_payload();
    payload.admin_identity_pub = None;
    assert!(matches!(
        encode_invite_url(&payload).unwrap_err(),
        InviteUrlError::InviteOnlyMissingBootstrap
    ));
}

#[test]
fn encode_rejects_open_community_with_admin_identity_pub_set() {
    use harmony_app::community_invite::{
        encode_invite_url, CommunityInvitePayload, InviteEpochSnapshot, InviteUrlError,
        MaterializedCommunityState,
    };
    use harmony_app::owner_state_types::{EpochKey, OwnerAddr, SpaceId};
    let payload = CommunityInvitePayload {
        community_id: SpaceId([0xab; 16]),
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: EpochKey::new([0x42; 32]).as_bytes().to_vec(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: OwnerAddr([0xcd; 16]),
        community_name: "WriterCheck".to_string(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: Some([0xAB; 64]),
        forked_from: None,
        pre_fork_snapshot: None,
    };
    assert!(matches!(
        encode_invite_url(&payload).unwrap_err(),
        InviteUrlError::OpenCommunityHasBootstrap
    ));
}

#[test]
fn encode_rejects_open_community_with_admin_bootstrap_set() {
    use harmony_app::community_invite::{
        encode_invite_url, CommunityInvitePayload, InviteEpochSnapshot, InviteUrlError,
        MaterializedCommunityState,
    };
    use harmony_app::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};
    // Synthesize a signed admin self-Join just so admin_bootstrap is
    // structurally well-formed; the writer check fires before any
    // signature inspection.
    let admin_addr = OwnerAddr([0xcd; 16]);
    let community_id = SpaceId([0xab; 16]);
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x33; 32]);
    let bs = sign_event(
        &EventPayload {
            id: [0xCC; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin_addr,
            at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        },
        &signing_key,
    )
    .expect("sign");
    let payload = CommunityInvitePayload {
        community_id,
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: EpochKey::new([0x42; 32]).as_bytes().to_vec(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr,
        community_name: "WriterCheck".to_string(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: Some(bs),
        admin_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
    };
    assert!(matches!(
        encode_invite_url(&payload).unwrap_err(),
        InviteUrlError::OpenCommunityHasBootstrap
    ));
}

#[test]
fn encode_rejects_invite_only_without_invite_token() {
    use harmony_app::community_invite::{encode_invite_url, InviteUrlError};
    // Mutate only invite_token → None on top of the good fixture so
    // the assertion isolates that invariant from the bootstrap-fields
    // and admin_addr checks. (Order-of-validation drift in
    // encode_invite_url would otherwise let this test pass for the
    // wrong reason.)
    let mut payload = admin_bootstrap_helpers::good_invite_only_payload();
    payload.invite_token = None;
    assert!(matches!(
        encode_invite_url(&payload).unwrap_err(),
        InviteUrlError::InviteOnlyMissingToken
    ));
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
            expires_at: None,
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
            expires_at: None,
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

mod verify_rejection_tests {
    use harmony_app::community_invite::{
        canonical_invite_token_bytes, verify_packet_pure, CommunityInviteSigned,
        CommunityInviteVerifyError, InviteToken,
    };
    use harmony_app::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use harmony_app::owner_state_types::{DeviceIdentityHash, Hlc, OwnerAddr, SpaceId};

    /// Common harness: build a fully valid CommunityInviteSigned + a
    /// matching InviteToken signed by `self_identity`. Tests then mutate
    /// one field and assert the right reject discriminant.
    fn make_valid_packet(
        self_identity: &harmony_identity::PrivateIdentity,
        joiner_identity: &harmony_identity::PrivateIdentity,
        community_id: SpaceId,
    ) -> CommunityInviteSigned {
        let self_owner = OwnerAddr(self_identity.identity.address_hash);
        let joiner_owner = OwnerAddr(joiner_identity.identity.address_hash);
        let joiner_pub = joiner_identity.identity.to_public_bytes();
        let joiner_sk = {
            let priv_bytes = joiner_identity.to_private_bytes();
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&priv_bytes[32..64]);
            ed25519_dalek::SigningKey::from_bytes(&seed)
        };
        let join_event = sign_event(
            &EventPayload {
                id: [0x44; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: joiner_owner,
                at: Hlc {
                    wall_ms: 1000,
                    logical: 0,
                    device_id: "j".into(),
                },
            },
            &joiner_sk,
        )
        .expect("sign Join");

        // Build an InviteToken signed by self over the same canonical bytes
        // verify_packet_pure reconstructs (mirrors the v1 single-shot
        // inviter-must-be-self contract).
        let unsigned_token = InviteToken {
            inviter: self_owner,
            invitee_hint: Some(joiner_owner),
            minted_at: Hlc {
                wall_ms: 900,
                logical: 0,
                device_id: "i".into(),
            },
            expires_at: None,
            sig: [0u8; 64],
        };
        let token_payload_bytes =
            canonical_invite_token_bytes(&unsigned_token).expect("encode token payload");
        let token_sig = self_identity.sign(&token_payload_bytes);
        let invite_token = InviteToken {
            sig: token_sig,
            ..unsigned_token
        };

        CommunityInviteSigned {
            community_id,
            join_event,
            invite_token,
            joiner_identity_pub: joiner_pub,
            signing_device_hash: DeviceIdentityHash(joiner_identity.identity.address_hash),
            created_at: Hlc {
                wall_ms: 1100,
                logical: 0,
                device_id: "j".into(),
            },
        }
    }

    fn now_ms() -> u64 {
        2000
    }

    #[test]
    fn community_invite_join_sig_invalid_rejected() {
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xa1; 32]);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xb2; 32]);
        let community_id = SpaceId([0x10; 16]);
        let mut signed = make_valid_packet(&self_id, &joiner_id, community_id);

        // Flip a byte in the inner Join sig.
        signed.join_event.sig[0] ^= 0xff;

        let err = verify_packet_pure(
            &signed,
            OwnerAddr(self_id.identity.address_hash),
            now_ms,
            &self_id,
        )
        .expect_err("must reject");
        assert!(matches!(err, CommunityInviteVerifyError::JoinSigInvalid));
    }

    #[test]
    fn community_invite_token_sig_invalid_rejected() {
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xa3; 32]);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xb4; 32]);
        let community_id = SpaceId([0x10; 16]);
        let mut signed = make_valid_packet(&self_id, &joiner_id, community_id);

        signed.invite_token.sig[0] ^= 0xff;

        let err = verify_packet_pure(
            &signed,
            OwnerAddr(self_id.identity.address_hash),
            now_ms,
            &self_id,
        )
        .expect_err("must reject");
        assert!(matches!(
            err,
            CommunityInviteVerifyError::InviteTokenSigInvalid
        ));
    }

    #[test]
    fn community_invite_signer_mismatch_rejected() {
        // InviteToken.signer is some other OwnerAddr (not self).
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xa5; 32]);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xb6; 32]);
        let community_id = SpaceId([0x10; 16]);
        let mut signed = make_valid_packet(&self_id, &joiner_id, community_id);

        signed.invite_token.inviter = OwnerAddr([0xaa; 16]); // not self

        let err = verify_packet_pure(
            &signed,
            OwnerAddr(self_id.identity.address_hash),
            now_ms,
            &self_id,
        )
        .expect_err("must reject");
        assert!(matches!(
            err,
            CommunityInviteVerifyError::InviteSignerMismatch { .. }
        ));
    }

    #[test]
    fn community_invite_id_mismatch_rejected() {
        // signed.community_id != signed.join_event.community_id.
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xa7; 32]);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xb8; 32]);
        let community_id = SpaceId([0x10; 16]);
        let mut signed = make_valid_packet(&self_id, &joiner_id, community_id);

        signed.community_id = SpaceId([0xff; 16]); // mismatch

        let err = verify_packet_pure(
            &signed,
            OwnerAddr(self_id.identity.address_hash),
            now_ms,
            &self_id,
        )
        .expect_err("must reject");
        assert!(matches!(
            err,
            CommunityInviteVerifyError::CommunityIdMismatch
        ));
    }

    #[test]
    fn community_invite_invitee_hint_mismatch_rejected() {
        // join_event.actor != invite_token.invitee_hint.
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xa9; 32]);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xba; 32]);
        let community_id = SpaceId([0x10; 16]);
        let mut signed = make_valid_packet(&self_id, &joiner_id, community_id);

        signed.invite_token.invitee_hint = Some(OwnerAddr([0xcc; 16]));

        let err = verify_packet_pure(
            &signed,
            OwnerAddr(self_id.identity.address_hash),
            now_ms,
            &self_id,
        )
        .expect_err("must reject");
        assert!(matches!(
            err,
            CommunityInviteVerifyError::InviteeHintMismatch
        ));
    }

    #[test]
    fn community_invite_expired_clock_skew_rejected() {
        // created_at.wall_ms is way in the future relative to now.
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xab; 32]);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xbc; 32]);
        let community_id = SpaceId([0x10; 16]);
        let mut signed = make_valid_packet(&self_id, &joiner_id, community_id);

        // Now is 2000 ms; created_at is set to 999_999_999 ms — way past
        // the 60_000 ms clock-skew tolerance.
        signed.created_at.wall_ms = 999_999_999;

        let err = verify_packet_pure(
            &signed,
            OwnerAddr(self_id.identity.address_hash),
            now_ms,
            &self_id,
        )
        .expect_err("must reject");
        assert!(matches!(err, CommunityInviteVerifyError::Expired));
    }

    #[test]
    fn community_invite_expired_token_rejected() {
        // InviteToken.expires_at is set; created_at is at-or-after that
        // value, so verify must reject. Spec: signed.created_at.wall_ms
        // >= signed.invite_token.expires_at → Expired.
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xc1; 32]);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xc2; 32]);
        let community_id = SpaceId([0x10; 16]);
        let self_owner = OwnerAddr(self_id.identity.address_hash);
        let joiner_owner = OwnerAddr(joiner_id.identity.address_hash);
        let joiner_pub = joiner_id.identity.to_public_bytes();
        let joiner_sk = {
            let priv_bytes = joiner_id.to_private_bytes();
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&priv_bytes[32..64]);
            ed25519_dalek::SigningKey::from_bytes(&seed)
        };
        let join_event = sign_event(
            &EventPayload {
                id: [0x44; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: joiner_owner,
                at: Hlc {
                    wall_ms: 1000,
                    logical: 0,
                    device_id: "j".into(),
                },
            },
            &joiner_sk,
        )
        .expect("sign Join");

        // expires_at = 1100 (= packet's created_at). Spec rejects on
        // created_at >= expires_at.
        let unsigned_token = InviteToken {
            inviter: self_owner,
            invitee_hint: Some(joiner_owner),
            minted_at: Hlc {
                wall_ms: 900,
                logical: 0,
                device_id: "i".into(),
            },
            expires_at: Some(1100),
            sig: [0u8; 64],
        };
        let token_bytes =
            canonical_invite_token_bytes(&unsigned_token).expect("encode token payload");
        let token_sig = self_id.sign(&token_bytes);
        let invite_token = InviteToken {
            sig: token_sig,
            ..unsigned_token
        };

        let signed = CommunityInviteSigned {
            community_id,
            join_event,
            invite_token,
            joiner_identity_pub: joiner_pub,
            signing_device_hash: DeviceIdentityHash(joiner_id.identity.address_hash),
            created_at: Hlc {
                wall_ms: 1100, // == expires_at — must reject (>=)
                logical: 0,
                device_id: "j".into(),
            },
        };

        let err =
            verify_packet_pure(&signed, self_owner, now_ms, &self_id).expect_err("must reject");
        assert!(matches!(err, CommunityInviteVerifyError::Expired));
    }

    #[test]
    fn community_invite_stripped_expires_at_breaks_token_sig() {
        // Defense-in-depth: an attacker who strips `expires_at` from a
        // signed token to extend the redemption window MUST trigger an
        // InviteTokenSigInvalid (the inviter's sig binds the canonical
        // bytes including `xa`).
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xc3; 32]);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xc4; 32]);
        let community_id = SpaceId([0x10; 16]);
        let self_owner = OwnerAddr(self_id.identity.address_hash);
        let joiner_owner = OwnerAddr(joiner_id.identity.address_hash);
        let joiner_pub = joiner_id.identity.to_public_bytes();
        let joiner_sk = {
            let priv_bytes = joiner_id.to_private_bytes();
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&priv_bytes[32..64]);
            ed25519_dalek::SigningKey::from_bytes(&seed)
        };
        let join_event = sign_event(
            &EventPayload {
                id: [0x44; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: joiner_owner,
                at: Hlc {
                    wall_ms: 1000,
                    logical: 0,
                    device_id: "j".into(),
                },
            },
            &joiner_sk,
        )
        .expect("sign Join");

        // Sign with expires_at = Some(...).
        let unsigned_with_expiry = InviteToken {
            inviter: self_owner,
            invitee_hint: Some(joiner_owner),
            minted_at: Hlc {
                wall_ms: 900,
                logical: 0,
                device_id: "i".into(),
            },
            expires_at: Some(5_000_000_000),
            sig: [0u8; 64],
        };
        let token_bytes =
            canonical_invite_token_bytes(&unsigned_with_expiry).expect("encode token payload");
        let token_sig = self_id.sign(&token_bytes);

        // Attacker swaps to expires_at = None but keeps the sig.
        let stripped_token = InviteToken {
            inviter: self_owner,
            invitee_hint: Some(joiner_owner),
            minted_at: Hlc {
                wall_ms: 900,
                logical: 0,
                device_id: "i".into(),
            },
            expires_at: None,
            sig: token_sig,
        };

        let signed = CommunityInviteSigned {
            community_id,
            join_event,
            invite_token: stripped_token,
            joiner_identity_pub: joiner_pub,
            signing_device_hash: DeviceIdentityHash(joiner_id.identity.address_hash),
            created_at: Hlc {
                wall_ms: 1100,
                logical: 0,
                device_id: "j".into(),
            },
        };

        let err = verify_packet_pure(&signed, self_owner, now_ms, &self_id)
            .expect_err("must reject — sig binds expires_at");
        assert!(matches!(
            err,
            CommunityInviteVerifyError::InviteTokenSigInvalid
        ));
    }

    #[test]
    fn community_invite_valid_packet_admits() {
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xad; 32]);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xbe; 32]);
        let community_id = SpaceId([0x10; 16]);
        let signed = make_valid_packet(&self_id, &joiner_id, community_id);

        let join_event = verify_packet_pure(
            &signed,
            OwnerAddr(self_id.identity.address_hash),
            now_ms,
            &self_id,
        )
        .expect("must admit");
        assert_eq!(join_event.actor, OwnerAddr(joiner_id.identity.address_hash));
    }

    /// Positive control for the `expires_at = Some(future)` admit path.
    /// A regression that rejected EVERY token with `expires_at = Some(...)`
    /// would still pass the rejection tests; this catches that.
    #[test]
    fn community_invite_valid_packet_with_future_expires_at_admits() {
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xc5; 32]);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xc6; 32]);
        let community_id = SpaceId([0x10; 16]);
        let self_owner = OwnerAddr(self_id.identity.address_hash);
        let joiner_owner = OwnerAddr(joiner_id.identity.address_hash);
        let joiner_pub = joiner_id.identity.to_public_bytes();
        let joiner_sk = {
            let priv_bytes = joiner_id.to_private_bytes();
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&priv_bytes[32..64]);
            ed25519_dalek::SigningKey::from_bytes(&seed)
        };
        let join_event = sign_event(
            &EventPayload {
                id: [0x44; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: joiner_owner,
                at: Hlc {
                    wall_ms: 1000,
                    logical: 0,
                    device_id: "j".into(),
                },
            },
            &joiner_sk,
        )
        .expect("sign Join");

        // expires_at = 1_000_000 (well after `now_ms` = 2000 and
        // `created_at = 1100`). Both arms (created_at < expires_at and
        // now < expires_at) admit.
        let unsigned_token = InviteToken {
            inviter: self_owner,
            invitee_hint: Some(joiner_owner),
            minted_at: Hlc {
                wall_ms: 900,
                logical: 0,
                device_id: "i".into(),
            },
            expires_at: Some(1_000_000),
            sig: [0u8; 64],
        };
        let token_bytes =
            canonical_invite_token_bytes(&unsigned_token).expect("encode token payload");
        let token_sig = self_id.sign(&token_bytes);
        let invite_token = InviteToken {
            sig: token_sig,
            ..unsigned_token
        };

        let signed = CommunityInviteSigned {
            community_id,
            join_event,
            invite_token,
            joiner_identity_pub: joiner_pub,
            signing_device_hash: DeviceIdentityHash(joiner_id.identity.address_hash),
            created_at: Hlc {
                wall_ms: 1100,
                logical: 0,
                device_id: "j".into(),
            },
        };

        let admitted = verify_packet_pure(&signed, self_owner, now_ms, &self_id)
            .expect("future expires_at must admit");
        assert_eq!(admitted.actor, joiner_owner);
    }

    /// Receive-time replay reject: a packet whose `created_at`
    /// pre-dated `expires_at` (so it would have been valid at mint)
    /// MUST be rejected if the receiver's wall clock is now at-or-past
    /// `expires_at`. Without this arm, an attacker who captures a
    /// freshly-minted invite-only packet can replay it indefinitely
    /// after the inviter's intended expiry window closes.
    #[test]
    fn community_invite_replay_after_expires_at_rejected() {
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xc7; 32]);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xc8; 32]);
        let community_id = SpaceId([0x10; 16]);
        let self_owner = OwnerAddr(self_id.identity.address_hash);
        let joiner_owner = OwnerAddr(joiner_id.identity.address_hash);
        let joiner_pub = joiner_id.identity.to_public_bytes();
        let joiner_sk = {
            let priv_bytes = joiner_id.to_private_bytes();
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&priv_bytes[32..64]);
            ed25519_dalek::SigningKey::from_bytes(&seed)
        };
        let join_event = sign_event(
            &EventPayload {
                id: [0x44; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: joiner_owner,
                at: Hlc {
                    wall_ms: 1000,
                    logical: 0,
                    device_id: "j".into(),
                },
            },
            &joiner_sk,
        )
        .expect("sign Join");

        // expires_at = 1500. created_at = 1100 (< 1500, valid at mint).
        // Receiver's now is supplied by a custom now_fn that returns
        // 1500 — past expiry. Verify must reject.
        let unsigned_token = InviteToken {
            inviter: self_owner,
            invitee_hint: Some(joiner_owner),
            minted_at: Hlc {
                wall_ms: 900,
                logical: 0,
                device_id: "i".into(),
            },
            expires_at: Some(1500),
            sig: [0u8; 64],
        };
        let token_bytes =
            canonical_invite_token_bytes(&unsigned_token).expect("encode token payload");
        let token_sig = self_id.sign(&token_bytes);
        let invite_token = InviteToken {
            sig: token_sig,
            ..unsigned_token
        };

        let signed = CommunityInviteSigned {
            community_id,
            join_event,
            invite_token,
            joiner_identity_pub: joiner_pub,
            signing_device_hash: DeviceIdentityHash(joiner_id.identity.address_hash),
            created_at: Hlc {
                wall_ms: 1100, // < 1500 — passes the created_at arm
                logical: 0,
                device_id: "j".into(),
            },
        };

        // Bump now to 1500: now >= expires_at must reject.
        fn now_after_expiry() -> u64 {
            1500
        }
        let err = verify_packet_pure(&signed, self_owner, now_after_expiry, &self_id)
            .expect_err("must reject — receive-time past expiry");
        assert!(matches!(err, CommunityInviteVerifyError::Expired));
    }
}

// =====================================================================
// ZEB-260 Phase 4 Task 3 — verify_admin_bootstrap unit tests
// =====================================================================

mod admin_bootstrap_helpers {
    use harmony_app::community_invite::{
        CommunityInvitePayload, InviteEpochSnapshot, InviteToken, MaterializedCommunityState,
    };
    use harmony_app::community_membership::{
        sign_event, EventPayload, MembershipEventKind, SignedMembershipEvent,
    };
    use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};

    /// Deterministic keys: `seed` selects the identity (e.g., 0xAA for
    /// admin in the test). Returns `(identity_pub_64, signing_key,
    /// owner_addr)`.
    pub fn identity_set(seed: u8) -> ([u8; 64], ed25519_dalek::SigningKey, OwnerAddr) {
        let private = harmony_identity::PrivateIdentity::from_seed(&[seed; 32]);
        let pub_bytes = private.identity.to_public_bytes();
        let priv_bytes = private.to_private_bytes();
        let mut ed_seed = [0u8; 32];
        ed_seed.copy_from_slice(&priv_bytes[32..64]);
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_seed);
        let addr = OwnerAddr(private.identity.address_hash);
        (pub_bytes, signing_key, addr)
    }

    pub fn fixture_hlc() -> Hlc {
        Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "admin-dev".into(),
        }
    }

    /// Build a known-good admin self-Join (signed) for the given
    /// community_id + admin identity.
    pub fn admin_bootstrap_event(
        community_id: SpaceId,
        admin_addr: OwnerAddr,
        signing_key: &ed25519_dalek::SigningKey,
    ) -> SignedMembershipEvent {
        let payload = EventPayload {
            id: [0xCC; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin_addr,
            at: fixture_hlc(),
        };
        sign_event(&payload, signing_key).expect("sign admin bootstrap")
    }

    /// Build a validly-signed admin Leave event. Used to trigger step 6
    /// (kind check) without tripping step 5 (signature check) — the
    /// signature is valid, but kind != Join.
    pub fn admin_leave_event(
        community_id: SpaceId,
        admin_addr: OwnerAddr,
        signing_key: &ed25519_dalek::SigningKey,
    ) -> SignedMembershipEvent {
        let payload = EventPayload {
            id: [0xCC; 16],
            community_id,
            kind: MembershipEventKind::Leave,
            actor: admin_addr,
            at: fixture_hlc(),
        };
        sign_event(&payload, signing_key).expect("sign admin leave")
    }

    /// Build a known-good invite-only `CommunityInvitePayload` with
    /// well-formed `admin_bootstrap` + `admin_identity_pub`. The 9
    /// per-branch tests below mutate one field at a time.
    pub fn good_invite_only_payload() -> CommunityInvitePayload {
        let (admin_pub, admin_sk, admin_addr) = identity_set(0xAA);
        let community_id = SpaceId([0x37; 16]);
        let bootstrap = admin_bootstrap_event(community_id, admin_addr, &admin_sk);

        CommunityInvitePayload {
            community_id,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: EpochKey::new([0xBB; 32]).as_bytes().to_vec(),
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr,
            community_name: "TestCom".into(),
            is_invite_only: true,
            expires_at: None,
            invite_token: Some(InviteToken {
                inviter: admin_addr,
                invitee_hint: None,
                minted_at: fixture_hlc(),
                expires_at: None,
                sig: [0xDD; 64],
            }),
            admin_bootstrap: Some(bootstrap),
            admin_identity_pub: Some(admin_pub),
            forked_from: None,
            pre_fork_snapshot: None,
        }
    }
}

#[cfg(test)]
mod verify_admin_bootstrap_tests {
    use super::admin_bootstrap_helpers::*;
    use harmony_app::community_invite::{verify_admin_bootstrap, RedeemBootstrapVerifyError};
    use harmony_app::community_membership::CounterSignature;
    use harmony_app::owner_state_types::{OwnerAddr, SpaceId};

    #[test]
    fn admits_well_formed_admin_bootstrap() {
        let p = good_invite_only_payload();
        let res = verify_admin_bootstrap(&p);
        assert!(
            res.is_ok(),
            "well-formed bootstrap should pass; got {res:?}"
        );
    }

    #[test]
    fn rejects_invite_only_without_admin_bootstrap() {
        let mut p = good_invite_only_payload();
        p.admin_bootstrap = None;
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapMissing
        );
    }

    #[test]
    fn rejects_invite_only_without_admin_identity_pub() {
        let mut p = good_invite_only_payload();
        p.admin_identity_pub = None;
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapMissing
        );
    }

    #[test]
    fn rejects_invalid_admin_pubkey() {
        let mut p = good_invite_only_payload();
        // Build a 64-byte pub where bytes 32-63 (the Ed25519 portion) are
        // [0x7F; 32]. This compressed point does not decompress to a valid
        // Ed25519 curve point under ed25519-dalek 2.x / curve25519-dalek 4.x
        // (verified empirically: Identity::from_public_bytes returns Err for
        // this input). The X25519 first half (all zeros) is always valid.
        let mut bad_pub = [0u8; 64];
        bad_pub[32..].copy_from_slice(&[0x7F; 32]);
        p.admin_identity_pub = Some(bad_pub);
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapInvalidPubkey
        );
    }

    #[test]
    fn rejects_admin_address_mismatch() {
        let mut p = good_invite_only_payload();
        // Use a different identity's pubkey but keep the original
        // admin_addr → the pubkey.address_hash will mismatch.
        let (other_pub, _other_sk, _other_addr) = identity_set(0xBB);
        p.admin_identity_pub = Some(other_pub);
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapAddressMismatch
        );
    }

    #[test]
    fn rejects_admin_actor_mismatch() {
        let mut p = good_invite_only_payload();
        // Mutate the bootstrap's actor to a different address. Admin's
        // signature was over the original actor field, so this would
        // also fail step 5 (signature) — but step 3 fires first because
        // the chain checks actor before sig.
        let mut bs = p.admin_bootstrap.as_ref().expect("bootstrap").clone();
        bs.actor = OwnerAddr([0xFF; 16]);
        p.admin_bootstrap = Some(bs);
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapActorMismatch
        );
    }

    #[test]
    fn rejects_admin_community_mismatch() {
        let mut p = good_invite_only_payload();
        let mut bs = p.admin_bootstrap.as_ref().expect("bootstrap").clone();
        bs.community_id = SpaceId([0xFF; 16]);
        p.admin_bootstrap = Some(bs);
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapCommunityMismatch
        );
    }

    #[test]
    fn rejects_invalid_admin_signature() {
        let mut p = good_invite_only_payload();
        let mut bs = p.admin_bootstrap.as_ref().expect("bootstrap").clone();
        // Flip a single bit in the signature.
        bs.sig[0] ^= 0x01;
        p.admin_bootstrap = Some(bs);
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapSignatureInvalid
        );
    }

    #[test]
    fn rejects_admin_bootstrap_with_countersig() {
        let mut p = good_invite_only_payload();
        let mut bs = p.admin_bootstrap.as_ref().expect("bootstrap").clone();
        // Inject a synthetic countersig. The sig itself can be garbage —
        // step 6 (kind/countersig sanity) fires before any crypto on the
        // countersig itself.
        bs.countersig = Some(CounterSignature {
            signer: OwnerAddr([0xEE; 16]),
            sig: [0x77; 64],
        });
        p.admin_bootstrap = Some(bs);
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapKindInvalid
        );
    }

    #[test]
    fn rejects_admin_bootstrap_non_join_kind() {
        // Build from scratch with identity_set so we have the signing key.
        // Replace the bootstrap with a validly-signed Leave event so that
        // step 5 (sig verify) passes and step 6 (kind check) fires.
        let (admin_pub, admin_sk, admin_addr) = identity_set(0xAA);
        let community_id = SpaceId([0x37; 16]);
        let leave_bootstrap = admin_leave_event(community_id, admin_addr, &admin_sk);

        let p = harmony_app::community_invite::CommunityInvitePayload {
            community_id,
            epoch_snapshot: harmony_app::community_invite::InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: harmony_app::owner_state_types::EpochKey::new([0xBB; 32])
                    .as_bytes()
                    .to_vec(),
                state_snapshot: harmony_app::community_invite::MaterializedCommunityState::default(
                ),
            },
            admin_addr,
            community_name: "TestCom".into(),
            is_invite_only: true,
            expires_at: None,
            invite_token: Some(harmony_app::community_invite::InviteToken {
                inviter: admin_addr,
                invitee_hint: None,
                minted_at: fixture_hlc(),
                expires_at: None,
                sig: [0xDD; 64],
            }),
            admin_bootstrap: Some(leave_bootstrap),
            admin_identity_pub: Some(admin_pub),
            forked_from: None,
            pre_fork_snapshot: None,
        };
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapKindInvalid
        );
    }
}
