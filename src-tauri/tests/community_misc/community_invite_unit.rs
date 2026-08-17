//! Unit tests for community_invite.rs Phase 1 types.

use harmony_app::community_invite::{
    CommunityInvitePayload, InviteEpochSnapshot, InviteToken, MaterializedCommunityState,
};
use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};

/// Structural CBOR top-level-key check. Decodes `bytes` to
/// `ciborium::Value::Map` and asserts that the named string keys are
/// present / absent. Mirrors the helper of the same name in
/// `tests/wire_format_zeb285_fixtures.rs` (R1 fix). Both copies exist
/// because each integration-test file is a separate binary; sharing
/// across them would require a `tests/common/mod.rs` module — kept local
/// for now since the helper is small and self-contained.
///
/// Byte-substring matching (`bytes.windows(2).any(|w| w == b"pl")`) is
/// brittle: a data value containing the bytes "pl" (e.g., a name field
/// "Polite Project") false-positives. Decoding to `ciborium::Value` and
/// inspecting the map keys is robust against data-content coincidences.
fn assert_cbor_top_level_keys(bytes: &[u8], present: &[&str], absent: &[&str], label: &str) {
    let decoded: ciborium::Value = ciborium::de::from_reader(bytes)
        .unwrap_or_else(|e| panic!("{label}: decode as Value failed: {e}"));
    let pairs = match decoded {
        ciborium::Value::Map(m) => m,
        other => panic!("{label}: expected CBOR map at top level, got {other:?}"),
    };
    // Collect string-typed keys (canonical encoding uses Text keys for
    // these fields). Non-Text keys would be a wire-format break in itself.
    let keys: std::collections::BTreeSet<String> = pairs
        .iter()
        .filter_map(|(k, _)| match k {
            ciborium::Value::Text(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    for key in present {
        assert!(
            keys.contains(*key),
            "{label}: expected key `{key}` to be present (top-level keys: {keys:?})"
        );
    }
    for key in absent {
        assert!(
            !keys.contains(*key),
            "{label}: expected key `{key}` to be absent (top-level keys: {keys:?})"
        );
    }
}

#[test]
fn community_invite_payload_round_trips_open_form() {
    let p = CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id: SpaceId([1u8; 16]),
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: EpochKey::new([2u8; 32]).as_bytes().to_vec(),
            sealed_epoch_keys: Vec::new(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: OwnerAddr([3u8; 16]),
        community_name: "harmony-design".into(),
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
        inviter_signer_certs: Vec::new(),
        community_id: SpaceId([1u8; 16]),
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: EpochKey::new([2u8; 32]).as_bytes().to_vec(),
            sealed_epoch_keys: Vec::new(),
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
        inviter_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: None,
        untargeted_decrypt_key: None,
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
        inviter_signer_certs: Vec::new(),
        community_id: SpaceId([0xab; 16]),
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: EpochKey::new([0x42; 32]).as_bytes().to_vec(),
            sealed_epoch_keys: Vec::new(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: OwnerAddr([0xcd; 16]),
        community_name: "Hackers United".to_string(),
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
    use harmony_app::community_invite::{
        decode_invite_url, InviteUrlError, MAX_INVITE_BODY_B64_CHARS,
    };
    // Must exceed MAX_INVITE_BODY_B64_CHARS (2_800_000 = ≈2 MiB decoded, Phase 1).
    let huge_body = "A".repeat(MAX_INVITE_BODY_B64_CHARS + 1);
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
        inviter_signer_certs: Vec::new(),
        community_id: SpaceId([0xab; 16]),
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: EpochKey::new([0x42; 32]).as_bytes().to_vec(),
            sealed_epoch_keys: Vec::new(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: OwnerAddr([0xcd; 16]),
        community_name: "WhitespaceTest".to_string(),
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
fn encode_rejects_invite_only_without_inviter_identity_pub() {
    use harmony_app::community_invite::{encode_invite_url, InviteUrlError};
    // Symmetric to the missing-admin_bootstrap test above: mutate ONE
    // field (inviter_identity_pub → None) on top of a known-valid
    // invite-only fixture. Both fields are required for invite-only
    // encoding; this test pins the symmetric branch of the same
    // InviteOnlyMissingBootstrap rejection.
    let mut payload = admin_bootstrap_helpers::good_invite_only_payload();
    payload.inviter_identity_pub = None;
    assert!(matches!(
        encode_invite_url(&payload).unwrap_err(),
        InviteUrlError::InviteOnlyMissingBootstrap
    ));
}

#[test]
fn encode_rejects_open_community_with_inviter_identity_pub_set() {
    use harmony_app::community_invite::{
        encode_invite_url, CommunityInvitePayload, InviteEpochSnapshot, InviteUrlError,
        MaterializedCommunityState,
    };
    use harmony_app::owner_state_types::{EpochKey, OwnerAddr, SpaceId};
    let payload = CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id: SpaceId([0xab; 16]),
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: EpochKey::new([0x42; 32]).as_bytes().to_vec(),
            sealed_epoch_keys: Vec::new(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: OwnerAddr([0xcd; 16]),
        community_name: "WriterCheck".to_string(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        inviter_identity_pub: Some([0xAB; 64]),
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: None,
        untargeted_decrypt_key: None,
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
        inviter_signer_certs: Vec::new(),
        community_id,
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: EpochKey::new([0x42; 32]).as_bytes().to_vec(),
            sealed_epoch_keys: Vec::new(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr,
        community_name: "WriterCheck".to_string(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: Some(bs),
        inviter_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: None,
        untargeted_decrypt_key: None,
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
        other => panic!("expected a pair of Invite packets, got {other:?}"),
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

    /// Extract the ed25519 signing key embedded in a `PrivateIdentity`
    /// (bytes 32..64 of `to_private_bytes()`). Used to produce a device-key
    /// analogue for counter-signer identities in unit tests where a full
    /// `TestOwner` (mint_test_owner) isn't needed.
    fn device_sk_from_identity(
        id: &harmony_identity::PrivateIdentity,
    ) -> ed25519_dalek::SigningKey {
        let priv_bytes = id.to_private_bytes();
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&priv_bytes[32..64]);
        ed25519_dalek::SigningKey::from_bytes(&seed)
    }

    /// Mint a ZEB-339 enrolled-device Join event for use in verify_packet tests.
    ///
    /// **Dual-identity pattern**: this helper mints a `TestOwner` (seed derived
    /// from `joiner_identity`) which carries the enrolled device key (#2) used
    /// as the event actor and signer. The returned `joiner_pub` is the SEPARATE
    /// Reticulum `joiner_identity_pub` (64-byte combined [x25519 || ed25519]),
    /// used for DM/transport layer. Importantly, `joiner.owner` (the community
    /// actor) does NOT equal the `OwnerAddr` derived from `joiner_identity` (the
    /// Reticulum transport identity) — they are from different key systems.
    /// This mirrors production: a user has a harmony-owner master key (community
    /// actor) and a separate Reticulum identity key (DM/transport).
    fn minted_join_event(
        joiner_identity: &harmony_identity::PrivateIdentity,
        community_id: SpaceId,
    ) -> (
        harmony_app::community_membership::SignedMembershipEvent,
        OwnerAddr,
        [u8; 64],
    ) {
        minted_join_event_at(joiner_identity, community_id, 1000)
    }

    /// Like `minted_join_event`, but lets the caller set the Join event's own
    /// `at.wall_ms` — used to exercise the ZEB-846 join-event forward-skew
    /// bound independently of the packet envelope's `created_at`.
    fn minted_join_event_at(
        joiner_identity: &harmony_identity::PrivateIdentity,
        community_id: SpaceId,
        wall_ms: u64,
    ) -> (
        harmony_app::community_membership::SignedMembershipEvent,
        OwnerAddr,
        [u8; 64],
    ) {
        let seed = joiner_identity.identity.address_hash[0] | 0x01;
        let joiner = harmony_app::community_membership::mint_test_owner(seed);
        let join_event = sign_event(
            &EventPayload {
                id: [0x44; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: joiner.owner,
                at: Hlc {
                    wall_ms,
                    logical: 0,
                    device_id: "j".into(),
                },
            },
            &joiner.device_key,
        )
        .expect("sign Join");
        let join_event = harmony_app::community_membership::SignedMembershipEvent {
            enrollment: Some(joiner.cert.clone()),
            ..join_event
        };
        let joiner_pub = joiner_identity.identity.to_public_bytes();
        (join_event, joiner.owner, joiner_pub)
    }

    /// Build a fully valid `CommunityInviteSigned`. The InviteToken is signed
    /// by `self_device_sk` (ZEB-339: must match what `verify_packet_pure` step
    /// 6 verifies — the counter-signer's enrolled device key, not the Reticulum
    /// identity key).
    fn make_valid_packet(
        self_identity: &harmony_identity::PrivateIdentity,
        self_device_sk: &ed25519_dalek::SigningKey,
        joiner_identity: &harmony_identity::PrivateIdentity,
        community_id: SpaceId,
    ) -> CommunityInviteSigned {
        make_valid_packet_with_join_wall_ms(
            self_identity,
            self_device_sk,
            joiner_identity,
            community_id,
            1000,
        )
    }

    /// Like `make_valid_packet`, but lets the caller set the inner Join
    /// event's own `at.wall_ms` — used to test the ZEB-846 join-event
    /// forward-skew bound independently of the packet envelope's
    /// `created_at` (which stays fixed at 1100, well within the freshness
    /// tolerance relative to `now_ms() == 2000`).
    fn make_valid_packet_with_join_wall_ms(
        self_identity: &harmony_identity::PrivateIdentity,
        self_device_sk: &ed25519_dalek::SigningKey,
        joiner_identity: &harmony_identity::PrivateIdentity,
        community_id: SpaceId,
        join_wall_ms: u64,
    ) -> CommunityInviteSigned {
        use ed25519_dalek::Signer as _;
        let self_owner = OwnerAddr(self_identity.identity.address_hash);
        let (join_event, joiner_owner, joiner_pub) =
            minted_join_event_at(joiner_identity, community_id, join_wall_ms);

        // Build an InviteToken signed by the counter-signer's enrolled device
        // key (#2). ZEB-339: verify_packet_pure step 6 now verifies against
        // self_device_sk.verifying_key(), consistent with verify_event's P5 gate.
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
        let token_sig = self_device_sk.sign(&token_payload_bytes).to_bytes();
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
        let self_device_sk = device_sk_from_identity(&self_id);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xb2; 32]);
        let community_id = SpaceId([0x10; 16]);
        let mut signed = make_valid_packet(&self_id, &self_device_sk, &joiner_id, community_id);

        // Flip a byte in the inner Join sig.
        signed.join_event.sig[0] ^= 0xff;

        let err = verify_packet_pure(
            &signed,
            now_ms,
            &[self_device_sk.verifying_key().to_bytes()],
        )
        .expect_err("must reject");
        assert!(matches!(err, CommunityInviteVerifyError::JoinSigInvalid));
    }

    #[test]
    fn community_invite_token_sig_invalid_rejected() {
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xa3; 32]);
        let self_device_sk = device_sk_from_identity(&self_id);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xb4; 32]);
        let community_id = SpaceId([0x10; 16]);
        let mut signed = make_valid_packet(&self_id, &self_device_sk, &joiner_id, community_id);

        signed.invite_token.sig[0] ^= 0xff;

        let err = verify_packet_pure(
            &signed,
            now_ms,
            &[self_device_sk.verifying_key().to_bytes()],
        )
        .expect_err("must reject");
        assert!(matches!(
            err,
            CommunityInviteVerifyError::InviteTokenSigInvalid
        ));
    }

    /// ZEB-911: a witness (whose own identity plays no role in the pure
    /// verify) accepts a packet whose token was minted by the admin, as long
    /// as the caller-resolved `token_signer_keys` slice contains the admin's
    /// enrolled key. The former step-4 "token.inviter == self" policy is
    /// deleted; there is no self identity in this function at all.
    #[test]
    fn zeb911_witness_accepts_admin_minted_token_via_key_slice() {
        let admin_id = harmony_identity::PrivateIdentity::from_seed(&[0xa5; 32]);
        let admin_device_sk = device_sk_from_identity(&admin_id);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xb6; 32]);
        let community_id = SpaceId([0x10; 16]);
        let signed = make_valid_packet(&admin_id, &admin_device_sk, &joiner_id, community_id);
        let expected_actor = signed.join_event.actor;

        // The witness resolves the ADMIN's enrolled key from its own
        // materialized membership and passes it here — its own device key
        // never enters the check.
        let join_event = verify_packet_pure(
            &signed,
            now_ms,
            &[admin_device_sk.verifying_key().to_bytes()],
        )
        .expect("witness must accept an admin-minted token via the key slice");
        assert_eq!(join_event.actor, expected_actor);
    }

    /// ZEB-911: no key in the caller-resolved slice verifies the token sig →
    /// `InviteTokenSigInvalid` (the step-6 rejection now covers what the old
    /// step-4 identity mismatch used to smuggle in).
    #[test]
    fn zeb911_token_sig_no_matching_key_rejected() {
        let admin_id = harmony_identity::PrivateIdentity::from_seed(&[0xa5; 32]);
        let admin_device_sk = device_sk_from_identity(&admin_id);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xb6; 32]);
        let community_id = SpaceId([0x10; 16]);
        let signed = make_valid_packet(&admin_id, &admin_device_sk, &joiner_id, community_id);

        let stranger = ed25519_dalek::SigningKey::from_bytes(&[0x77; 32]);
        let err = verify_packet_pure(&signed, now_ms, &[stranger.verifying_key().to_bytes()])
            .expect_err("must reject");
        assert!(matches!(
            err,
            CommunityInviteVerifyError::InviteTokenSigInvalid
        ));

        // Empty slice: nothing can verify — same rejection.
        let err =
            verify_packet_pure(&signed, now_ms, &[]).expect_err("empty key slice must reject");
        assert!(matches!(
            err,
            CommunityInviteVerifyError::InviteTokenSigInvalid
        ));
    }

    /// ZEB-911: the slice is try-each (mirrors
    /// `verify_invite_token_sig_with_enrolled`) — a match anywhere in the
    /// slice admits, including last position.
    #[test]
    fn zeb911_token_sig_multi_key_last_matches_accepted() {
        let admin_id = harmony_identity::PrivateIdentity::from_seed(&[0xa5; 32]);
        let admin_device_sk = device_sk_from_identity(&admin_id);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xb6; 32]);
        let community_id = SpaceId([0x10; 16]);
        let signed = make_valid_packet(&admin_id, &admin_device_sk, &joiner_id, community_id);

        let wrong_a = ed25519_dalek::SigningKey::from_bytes(&[0x71; 32]);
        let wrong_b = ed25519_dalek::SigningKey::from_bytes(&[0x72; 32]);
        verify_packet_pure(
            &signed,
            now_ms,
            &[
                wrong_a.verifying_key().to_bytes(),
                wrong_b.verifying_key().to_bytes(),
                admin_device_sk.verifying_key().to_bytes(),
            ],
        )
        .expect("a matching key anywhere in the slice must admit");
    }

    #[test]
    fn community_invite_id_mismatch_rejected() {
        // signed.community_id != signed.join_event.community_id.
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xa7; 32]);
        let self_device_sk = device_sk_from_identity(&self_id);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xb8; 32]);
        let community_id = SpaceId([0x10; 16]);
        let mut signed = make_valid_packet(&self_id, &self_device_sk, &joiner_id, community_id);

        signed.community_id = SpaceId([0xff; 16]); // mismatch

        let err = verify_packet_pure(
            &signed,
            now_ms,
            &[self_device_sk.verifying_key().to_bytes()],
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
        let self_device_sk = device_sk_from_identity(&self_id);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xba; 32]);
        let community_id = SpaceId([0x10; 16]);
        let mut signed = make_valid_packet(&self_id, &self_device_sk, &joiner_id, community_id);

        signed.invite_token.invitee_hint = Some(OwnerAddr([0xcc; 16]));

        let err = verify_packet_pure(
            &signed,
            now_ms,
            &[self_device_sk.verifying_key().to_bytes()],
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
        let self_device_sk = device_sk_from_identity(&self_id);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xbc; 32]);
        let community_id = SpaceId([0x10; 16]);
        let mut signed = make_valid_packet(&self_id, &self_device_sk, &joiner_id, community_id);

        // Now is 2000 ms; created_at is set to 999_999_999 ms — way past
        // the 60_000 ms clock-skew tolerance.
        signed.created_at.wall_ms = 999_999_999;

        let err = verify_packet_pure(
            &signed,
            now_ms,
            &[self_device_sk.verifying_key().to_bytes()],
        )
        .expect_err("must reject");
        assert!(matches!(err, CommunityInviteVerifyError::Expired));
    }

    /// ZEB-846 Task 7: `signed.join_event.at.wall_ms` — the Join's OWN wall —
    /// must be bounded even when `created_at` (the envelope) is fresh. This is
    /// the timestamp that actually lands in the persisted membership log, so a
    /// skewed/malicious inviter-redeem packet with a fresh envelope wrapped
    /// around a far-future-walled Join must still be rejected.
    #[test]
    fn community_invite_join_event_future_skew_rejected() {
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xc7; 32]);
        let self_device_sk = device_sk_from_identity(&self_id);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xc8; 32]);
        let community_id = SpaceId([0x10; 16]);

        // now_ms() == 2000; MAX_FORWARD_SKEW_MS == 300_000 — one past the bound.
        let just_outside = now_ms() + harmony_app::clock_trust::MAX_FORWARD_SKEW_MS + 1;
        let signed = make_valid_packet_with_join_wall_ms(
            &self_id,
            &self_device_sk,
            &joiner_id,
            community_id,
            just_outside,
        );
        // created_at (1100) stays fresh — the rejection must be attributable
        // to the join_event's own forward-skew bound, not the envelope check.

        let err = verify_packet_pure(
            &signed,
            now_ms,
            &[self_device_sk.verifying_key().to_bytes()],
        )
        .expect_err("must reject");
        assert!(matches!(
            err,
            CommunityInviteVerifyError::JoinEventFutureSkew
        ));
    }

    /// Boundary companion to the above: `join_event.at.wall_ms` exactly at
    /// `now + MAX_FORWARD_SKEW_MS` must still admit — `reject_future`'s
    /// boundary is inclusive.
    #[test]
    fn community_invite_join_event_at_skew_boundary_admits() {
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xc9; 32]);
        let self_device_sk = device_sk_from_identity(&self_id);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xca; 32]);
        let community_id = SpaceId([0x10; 16]);

        let at_boundary = now_ms() + harmony_app::clock_trust::MAX_FORWARD_SKEW_MS;
        let signed = make_valid_packet_with_join_wall_ms(
            &self_id,
            &self_device_sk,
            &joiner_id,
            community_id,
            at_boundary,
        );
        let expected_actor = signed.join_event.actor;

        let join_event = verify_packet_pure(
            &signed,
            now_ms,
            &[self_device_sk.verifying_key().to_bytes()],
        )
        .expect("join_event.at.wall_ms exactly at the skew boundary must admit");
        assert_eq!(join_event.actor, expected_actor);
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

        let self_device_sk = device_sk_from_identity(&self_id);
        let err = verify_packet_pure(
            &signed,
            now_ms,
            &[self_device_sk.verifying_key().to_bytes()],
        )
        .expect_err("must reject");
        assert!(matches!(err, CommunityInviteVerifyError::Expired));
    }

    #[test]
    fn community_invite_stripped_expires_at_breaks_token_sig() {
        // Defense-in-depth: an attacker who strips `expires_at` from a
        // signed token to extend the redemption window MUST trigger an
        // InviteTokenSigInvalid (the inviter's sig binds the canonical
        // bytes including `xa`).
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xc3; 32]);
        let self_device_sk = device_sk_from_identity(&self_id);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xc4; 32]);
        let community_id = SpaceId([0x10; 16]);
        let self_owner = OwnerAddr(self_id.identity.address_hash);
        let (join_event, joiner_owner, joiner_pub) = minted_join_event(&joiner_id, community_id);

        // Sign with expires_at = Some(...) using the counter-signer's device key.
        use ed25519_dalek::Signer as _;
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
        let token_sig = self_device_sk.sign(&token_bytes).to_bytes();

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

        let err = verify_packet_pure(
            &signed,
            now_ms,
            &[self_device_sk.verifying_key().to_bytes()],
        )
        .expect_err("must reject — sig binds expires_at");
        assert!(matches!(
            err,
            CommunityInviteVerifyError::InviteTokenSigInvalid
        ));
    }

    #[test]
    fn community_invite_valid_packet_admits() {
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xad; 32]);
        let self_device_sk = device_sk_from_identity(&self_id);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xbe; 32]);
        let community_id = SpaceId([0x10; 16]);
        let signed = make_valid_packet(&self_id, &self_device_sk, &joiner_id, community_id);
        let expected_actor = signed.join_event.actor;

        let join_event = verify_packet_pure(
            &signed,
            now_ms,
            &[self_device_sk.verifying_key().to_bytes()],
        )
        .expect("must admit");
        assert_eq!(join_event.actor, expected_actor);
    }

    /// Positive control for the `expires_at = Some(future)` admit path.
    /// A regression that rejected EVERY token with `expires_at = Some(...)`
    /// would still pass the rejection tests; this catches that.
    #[test]
    fn community_invite_valid_packet_with_future_expires_at_admits() {
        use ed25519_dalek::Signer as _;
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xc5; 32]);
        let self_device_sk = device_sk_from_identity(&self_id);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xc6; 32]);
        let community_id = SpaceId([0x10; 16]);
        let self_owner = OwnerAddr(self_id.identity.address_hash);
        let (join_event, joiner_owner, joiner_pub) = minted_join_event(&joiner_id, community_id);

        // expires_at = 1_000_000 (well after `now_ms` = 2000 and
        // `created_at = 1100`). Both arms (created_at < expires_at and
        // now < expires_at) admit. Token signed by the device key (ZEB-339).
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
        let token_sig = self_device_sk.sign(&token_bytes).to_bytes();
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

        let admitted = verify_packet_pure(
            &signed,
            now_ms,
            &[self_device_sk.verifying_key().to_bytes()],
        )
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
        let self_device_sk = device_sk_from_identity(&self_id);
        let err = verify_packet_pure(
            &signed,
            now_after_expiry,
            &[self_device_sk.verifying_key().to_bytes()],
        )
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
        mint_test_owner, sign_event, EventPayload, MembershipEventKind, SignedMembershipEvent,
        TestOwner,
    };
    use harmony_app::owner_state_types::{EpochKey, Hlc, SpaceId};

    pub fn fixture_hlc() -> Hlc {
        Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "admin-dev".into(),
        }
    }

    /// ZEB-339: produce a deterministic enrolled-device admin owner.
    /// Returns a `TestOwner` whose `owner` field is the master hash and
    /// whose `cert` binds the device signing key to that master.
    pub fn admin_owner(seed: u8) -> TestOwner {
        mint_test_owner(seed)
    }

    /// Build a cert-bearing admin self-Join (signed by device key) for
    /// the given community_id + admin TestOwner. ZEB-339: the event must
    /// carry the admin's EnrollmentCert so `enrolled_key_from_cert` can
    /// extract and verify the signer.
    pub fn admin_bootstrap_event(
        community_id: SpaceId,
        admin: &TestOwner,
    ) -> SignedMembershipEvent {
        let payload = EventPayload {
            id: [0xCC; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin.owner,
            at: fixture_hlc(),
        };
        let mut ev = sign_event(&payload, &admin.device_key).expect("sign admin bootstrap");
        ev.enrollment = Some(admin.cert.clone());
        ev
    }

    /// Build a cert-bearing admin Leave event (signed by device key,
    /// carries cert). Used to trigger step 6 (kind check) without
    /// tripping step 5 (cert-based sig check) — the signature and cert
    /// are valid, but kind != Join.
    pub fn admin_leave_event(community_id: SpaceId, admin: &TestOwner) -> SignedMembershipEvent {
        let payload = EventPayload {
            id: [0xCC; 16],
            community_id,
            kind: MembershipEventKind::Leave,
            actor: admin.owner,
            at: fixture_hlc(),
        };
        let mut ev = sign_event(&payload, &admin.device_key).expect("sign admin leave");
        ev.enrollment = Some(admin.cert.clone());
        ev
    }

    /// Build a known-good invite-only `CommunityInvitePayload` with
    /// well-formed `admin_bootstrap` (cert-bearing, ZEB-339 model) +
    /// `inviter_identity_pub`. The per-branch tests below mutate one field
    /// at a time.
    pub fn good_invite_only_payload() -> CommunityInvitePayload {
        let admin = admin_owner(0xAA);
        let admin_addr = admin.owner;
        // inviter_identity_pub: 64-byte [x25519 || ed25519] representation.
        // The engine ignores it post-ZEB-339 (VerifyContext no longer carries
        // it), but the field must be Some for step 1 (BootstrapMissing gate).
        // The x25519 half (combined[0..32]) is intentionally zeroed because
        // x25519 is unused in post-ZEB-339 verify_admin_bootstrap — verification
        // uses the cert's device key exclusively. No check reads combined[0..32].
        let admin_pub = {
            let ed25519_pub = admin.cert.device_pubkeys.classical.ed25519_verify;
            let mut combined = [0u8; 64];
            combined[32..].copy_from_slice(&ed25519_pub);
            combined
        };
        let community_id = SpaceId([0x37; 16]);
        let bootstrap = admin_bootstrap_event(community_id, &admin);

        CommunityInvitePayload {
            inviter_signer_certs: Vec::new(),
            community_id,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: EpochKey::new([0xBB; 32]).as_bytes().to_vec(),
                sealed_epoch_keys: Vec::new(),
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
            inviter_identity_pub: Some(admin_pub),
            forked_from: None,
            pre_fork_snapshot: None,
            inviter_enrollment: None,
            untargeted_decrypt_key: None,
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
    fn rejects_invite_only_without_inviter_identity_pub() {
        let mut p = good_invite_only_payload();
        p.inviter_identity_pub = None;
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapMissing
        );
    }

    // NOTE: rejects_invalid_admin_pubkey and rejects_admin_address_mismatch
    // have been removed (ZEB-339). Step 2 (the flat identity_pub address_hash
    // == admin_addr gate) no longer exists in verify_admin_bootstrap; the
    // inviter_identity_pub field is no longer gated against admin_addr.
    // Cryptographic binding is now enforced by step 5 (cert model) instead.

    #[test]
    fn rejects_admin_actor_mismatch() {
        let mut p = good_invite_only_payload();
        // Mutate the bootstrap's actor to a different address. Step 3 fires
        // first (actor != admin_addr) before step 5 (cert check).
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
        // Flip a single bit in the signature. The cert is still valid (carries
        // the correct enrolled device key), but verify_membership_signer rejects
        // the corrupted sig.
        bs.sig[0] ^= 0x01;
        p.admin_bootstrap = Some(bs);
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapSignatureInvalid
        );
    }

    #[test]
    fn rejects_bootstrap_missing_cert() {
        // ZEB-339: a Join event without an EnrollmentCert cannot pass step 5
        // (enrolled_key_from_cert requires a cert). The error folds into
        // BootstrapSignatureInvalid.
        let mut p = good_invite_only_payload();
        let mut bs = p.admin_bootstrap.as_ref().expect("bootstrap").clone();
        bs.enrollment = None; // strip the cert
        p.admin_bootstrap = Some(bs);
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapSignatureInvalid
        );
    }

    #[test]
    fn rejects_admin_bootstrap_with_countersig() {
        // ZEB-339: the bootstrap carries a cert, so step 5 passes; step 6
        // (kind/countersig sanity) fires because countersig is Some.
        let admin = admin_owner(0xAA);
        let community_id = SpaceId([0x37; 16]);
        let mut bs = admin_bootstrap_event(community_id, &admin);
        // Inject a synthetic countersig. Step 6 fires before any crypto on the
        // countersig itself.
        bs.countersig = Some(CounterSignature {
            signer: OwnerAddr([0xEE; 16]),
            sig: [0x77; 64],
        });
        let admin_pub = {
            let ed25519_pub = admin.cert.device_pubkeys.classical.ed25519_verify;
            let mut combined = [0u8; 64];
            combined[32..].copy_from_slice(&ed25519_pub);
            combined
        };
        let mut p = good_invite_only_payload();
        p.admin_bootstrap = Some(bs);
        p.admin_addr = admin.owner;
        p.inviter_identity_pub = Some(admin_pub);
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapKindInvalid
        );
    }

    #[test]
    fn rejects_admin_bootstrap_non_join_kind() {
        // ZEB-339: use a cert-bearing Leave event so that step 5 (cert check +
        // sig verify) passes, and step 6 (kind check) fires.
        let admin = admin_owner(0xAA);
        let community_id = SpaceId([0x37; 16]);
        let leave_bootstrap = admin_leave_event(community_id, &admin);
        let admin_pub = {
            let ed25519_pub = admin.cert.device_pubkeys.classical.ed25519_verify;
            let mut combined = [0u8; 64];
            combined[32..].copy_from_slice(&ed25519_pub);
            combined
        };

        let p = harmony_app::community_invite::CommunityInvitePayload {
            inviter_signer_certs: Vec::new(),
            community_id,
            epoch_snapshot: harmony_app::community_invite::InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: harmony_app::owner_state_types::EpochKey::new([0xBB; 32])
                    .as_bytes()
                    .to_vec(),
                sealed_epoch_keys: Vec::new(),
                state_snapshot: harmony_app::community_invite::MaterializedCommunityState::default(
                ),
            },
            admin_addr: admin.owner,
            community_name: "TestCom".into(),
            is_invite_only: true,
            expires_at: None,
            invite_token: Some(harmony_app::community_invite::InviteToken {
                inviter: admin.owner,
                invitee_hint: None,
                minted_at: fixture_hlc(),
                expires_at: None,
                sig: [0xDD; 64],
            }),
            admin_bootstrap: Some(leave_bootstrap),
            inviter_identity_pub: Some(admin_pub),
            forked_from: None,
            pre_fork_snapshot: None,
            inviter_enrollment: None,
            untargeted_decrypt_key: None,
        };
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapKindInvalid
        );
    }
}

// ZEB-287 Phase 2: ParentLineageEntry roundtrip tests
mod zeb287_parent_lineage_entry {
    use harmony_app::community_invite::ParentLineageEntry;
    use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
    use harmony_app::owner_state_types::SpaceId;

    #[test]
    fn parent_lineage_entry_roundtrip_with_forked_at() {
        let entry = ParentLineageEntry {
            space_id: SpaceId([0x42; 16]),
            name: "Cool Community".to_string(),
            forked_at_wall_ms: Some(1_715_811_234_567),
            reason: None,
        };
        let bytes = canonical_cbor_encode(&entry).expect("encode");
        let decoded: ParentLineageEntry = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(entry, decoded);
    }

    #[test]
    fn parent_lineage_entry_roundtrip_root_omits_at() {
        let entry = ParentLineageEntry {
            space_id: SpaceId([0x11; 16]),
            name: "Project Cool".to_string(),
            forked_at_wall_ms: None,
            reason: None,
        };
        let bytes_no_at = canonical_cbor_encode(&entry).expect("encode");
        let decoded: ParentLineageEntry = canonical_cbor_decode(&bytes_no_at).expect("decode");
        assert_eq!(entry, decoded);

        // The serialized form must NOT contain a CBOR Text key "at"
        // since the field is skip-if-none. Structural decode (via the
        // top-level `assert_cbor_top_level_keys` helper) defends against
        // false positives where the bytes "at" appear inside data values
        // (e.g., a name like "atomic"); the previous `windows(2)` byte-
        // substring check was brittle to such collisions.
        super::assert_cbor_top_level_keys(
            &bytes_no_at,
            &["si", "nm"],
            &["at"],
            "Root ParentLineageEntry",
        );
    }
}

// ZEB-287 Phase 2: PreForkSnapshot.parent_lineage backwards-compat + roundtrip tests
mod zeb287_pre_fork_snapshot_lineage {
    use harmony_app::community_invite::{
        BoundedChannelLogSnapshot, ParentLineageEntry, PreForkSnapshot,
    };
    use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
    use harmony_app::owner_state_types::{Hlc, SpaceId};
    use std::collections::BTreeMap;

    fn empty_pre_fork_snapshot_for_test() -> PreForkSnapshot {
        PreForkSnapshot {
            original_community_id: SpaceId([0x42; 16]),
            original_community_name: "Cool Community".to_string(),
            membership_events: Vec::new(),
            channel_log: BoundedChannelLogSnapshot::default(),
            identity_pubs: BTreeMap::new(),
            forked_at: Hlc {
                wall_ms: 1_715_811_234_567,
                logical: 0,
                device_id: "d".into(),
            },
            parent_lineage: Vec::new(),
            fork_reason: None,
        }
    }

    #[test]
    fn pre_fork_snapshot_with_empty_lineage_omits_pl_key() {
        let snap = empty_pre_fork_snapshot_for_test();
        let bytes = canonical_cbor_encode(&snap).expect("encode");
        // Structural decode: the `pl` CBOR Text key must be absent.
        // Defends against a value containing the bytes "pl" inside e.g.
        // a name field ("Polite Project") false-positiving the previous
        // `windows(2)` byte-substring check.
        super::assert_cbor_top_level_keys(
            &bytes,
            &[],
            &["pl"],
            "PreForkSnapshot with empty parent_lineage",
        );
    }

    #[test]
    fn pre_fork_snapshot_with_populated_lineage_roundtrips() {
        let mut snap = empty_pre_fork_snapshot_for_test();
        snap.parent_lineage = vec![
            ParentLineageEntry {
                space_id: SpaceId([0x11; 16]),
                name: "Project Cool".to_string(),
                forked_at_wall_ms: None,
                reason: None,
            },
            ParentLineageEntry {
                space_id: SpaceId([0x22; 16]),
                name: "Cool Community".to_string(),
                forked_at_wall_ms: Some(1_715_000_000_000),
                reason: None,
            },
        ];

        let bytes = canonical_cbor_encode(&snap).expect("encode");
        // Structural decode: the `pl` CBOR Text key must be present at
        // the top-level map. Robust against the bytes "pl" appearing
        // inside data values (the previous `windows(2)` check would
        // false-positive on e.g. a name like "Polite Project").
        super::assert_cbor_top_level_keys(
            &bytes,
            &["pl"],
            &[],
            "PreForkSnapshot with populated parent_lineage",
        );

        let decoded: PreForkSnapshot = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(snap.parent_lineage, decoded.parent_lineage);
        assert_eq!(snap.original_community_id, decoded.original_community_id);
    }
}

// ZEB-287 Phase 2 Task 4: build_fork_snapshot lineage construction logic +
// 16-deep cap. These tests pin the algorithm shape against
// community_fork.rs::fork_community's Task 4 block via the SHARED
// `build_parent_lineage` / `apply_lineage_cap` helpers (R1-4).
mod zeb287_lineage_build_logic {
    use harmony_app::community_invite::{
        apply_lineage_cap, build_parent_lineage, ParentLineageEntry, MAX_LINEAGE_DEPTH,
    };
    use harmony_app::owner_state_types::SpaceId;

    #[test]
    fn build_fork_snapshot_lineage_extends_forker_chain() {
        // Simulate forker's CommunityState.parent_lineage = [C-entry] and
        // forker's community is B (forked from C). After fork into A_fork:
        //   A_fork.parent_lineage = [C-entry, B-entry]
        // Driven through the production helper so a regression in
        // production logic surfaces here.
        let c_entry = ParentLineageEntry {
            space_id: SpaceId([0x11; 16]),
            name: "C".to_string(),
            forked_at_wall_ms: None, // C is root
            reason: None,
        };
        let forker_lineage = vec![c_entry.clone()];
        let b_id = SpaceId([0x22; 16]);
        let b_name = "B";
        let b_forked_at = Some(1_700_000_000_000u64);

        // ZEB-649: the forker's own fork_reason rides on its pushed entry.
        let new_lineage = build_parent_lineage(
            &forker_lineage,
            b_id,
            b_name,
            b_forked_at,
            Some("B split from C".to_string()),
        );

        assert_eq!(new_lineage.len(), 2);
        assert_eq!(new_lineage[0], c_entry);
        assert_eq!(new_lineage[1].space_id, b_id);
        assert_eq!(new_lineage[1].name, b_name);
        assert_eq!(new_lineage[1].forked_at_wall_ms, b_forked_at);
        assert_eq!(new_lineage[1].reason.as_deref(), Some("B split from C"));
    }

    #[test]
    fn lineage_cap_drops_oldest_root_side_entries() {
        // Construct a 20-deep lineage; verify cap keeps newest 16 via the
        // shared helper.
        let mut overlong: Vec<ParentLineageEntry> = (0u8..20)
            .map(|i| ParentLineageEntry {
                space_id: SpaceId([i; 16]),
                name: format!("ancestor_{i}"),
                forked_at_wall_ms: if i == 0 { None } else { Some(i as u64) },
                reason: None,
            })
            .collect();

        apply_lineage_cap(&mut overlong);

        assert_eq!(overlong.len(), MAX_LINEAGE_DEPTH);
        // First entry should be ancestor_4 (oldest 4 dropped: 0,1,2,3).
        assert_eq!(overlong[0].name, "ancestor_4");
        // Last entry should be ancestor_19 (newest preserved).
        assert_eq!(overlong[15].name, "ancestor_19");
    }

    #[test]
    fn build_parent_lineage_extends_and_caps_correctly() {
        // R1-4 helper unit test: exercise both short and overlong inputs.

        // Short input: chain of 3 entries + push 1 → result has 4 entries,
        // no cap applied.
        let short_chain: Vec<ParentLineageEntry> = (0u8..3)
            .map(|i| ParentLineageEntry {
                space_id: SpaceId([i; 16]),
                name: format!("a_{i}"),
                forked_at_wall_ms: Some(i as u64),
                reason: None,
            })
            .collect();
        let new_short =
            build_parent_lineage(&short_chain, SpaceId([0xff; 16]), "forker", Some(99), None);
        assert_eq!(new_short.len(), 4);
        assert_eq!(new_short[0].name, "a_0");
        assert_eq!(new_short[3].name, "forker");
        assert_eq!(new_short[3].forked_at_wall_ms, Some(99));

        // Overlong input: chain of MAX_LINEAGE_DEPTH entries + push 1 →
        // result has MAX_LINEAGE_DEPTH entries with the OLDEST dropped.
        let max_chain: Vec<ParentLineageEntry> = (0u8..MAX_LINEAGE_DEPTH as u8)
            .map(|i| ParentLineageEntry {
                space_id: SpaceId([i; 16]),
                name: format!("a_{i}"),
                forked_at_wall_ms: if i == 0 { None } else { Some(i as u64) },
                reason: None,
            })
            .collect();
        let new_max =
            build_parent_lineage(&max_chain, SpaceId([0xfe; 16]), "forker", Some(999), None);
        assert_eq!(new_max.len(), MAX_LINEAGE_DEPTH);
        // a_0 dropped: first entry is a_1.
        assert_eq!(new_max[0].name, "a_1");
        // Last entry is the new forker entry.
        assert_eq!(new_max[MAX_LINEAGE_DEPTH - 1].name, "forker");

        // Empty input: just the new entry.
        let new_empty = build_parent_lineage(&[], SpaceId([0xfd; 16]), "first", None, None);
        assert_eq!(new_empty.len(), 1);
        assert_eq!(new_empty[0].name, "first");
        assert_eq!(new_empty[0].forked_at_wall_ms, None);
    }

    #[test]
    fn redeem_overlong_lineage_payload_truncates_to_cap() {
        // R1-2: a malicious or future-protocol-revision PreForkSnapshot
        // payload could carry > MAX_LINEAGE_DEPTH entries. The redeem path
        // applies `apply_lineage_cap` defensively so the joiner's local
        // CommunityState.parent_lineage stays bounded regardless of payload
        // length. This unit test mirrors the helper call site in
        // `lib.rs::redeem_invite_inner`.
        let mut payload_lineage: Vec<ParentLineageEntry> = (0u8..20)
            .map(|i| ParentLineageEntry {
                space_id: SpaceId([i; 16]),
                name: format!("evil_{i}"),
                forked_at_wall_ms: Some(i as u64),
                reason: None,
            })
            .collect();
        assert_eq!(payload_lineage.len(), 20);

        apply_lineage_cap(&mut payload_lineage);

        assert_eq!(payload_lineage.len(), MAX_LINEAGE_DEPTH);
        // Oldest 4 dropped: first is evil_4, last is evil_19.
        assert_eq!(payload_lineage[0].name, "evil_4");
        assert_eq!(payload_lineage[15].name, "evil_19");
    }
}
