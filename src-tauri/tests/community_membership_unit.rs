//! Unit-style integration tests for community_membership.rs.
//! Phase 1 (ZEB-217 Sub-C) — types, materialization, verification.

use harmony_app::community_membership::MembershipEventKind;
use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use harmony_app::owner_state_types::OwnerAddr;

#[test]
fn membership_event_kind_round_trips_all_variants() {
    let target = OwnerAddr([7u8; 16]);

    let kinds = vec![
        MembershipEventKind::Join,
        MembershipEventKind::Leave,
        MembershipEventKind::Invite { target },
        MembershipEventKind::Kick {
            target,
            reason: Some("spam".to_string()),
        },
        MembershipEventKind::Kick {
            target,
            reason: None,
        },
        MembershipEventKind::SetPower { target, level: 50 },
    ];

    for k in kinds {
        let encoded = canonical_cbor_encode(&k).expect("encode");
        let decoded: MembershipEventKind = canonical_cbor_decode(&encoded).expect("decode");
        assert_eq!(decoded, k, "round-trip mismatch for {k:?}");
    }
}

use harmony_app::community_membership::{CounterSignature, EventId, SignedMembershipEvent};
use harmony_app::owner_state_types::{Hlc, SpaceId};

#[test]
fn signed_event_round_trips_through_canonical_cbor() {
    let event = SignedMembershipEvent {
        id: [9u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor: OwnerAddr([1u8; 16]),
        at: Hlc {
            wall_ms: 12345,
            logical: 7,
            device_id: "phone".into(),
        },
        sig: [0xAA; 64],
        countersig: None,
    };

    let bytes = canonical_cbor_encode(&event).expect("encode");
    let decoded: SignedMembershipEvent = canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(decoded, event);
}

#[test]
fn signed_event_with_countersig_round_trips() {
    let countersig = CounterSignature {
        signer: OwnerAddr([42u8; 16]),
        sig: [0xBB; 64],
    };

    let event = SignedMembershipEvent {
        id: [9u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor: OwnerAddr([1u8; 16]),
        at: Hlc {
            wall_ms: 12345,
            logical: 7,
            device_id: "phone".into(),
        },
        sig: [0xAA; 64],
        countersig: Some(countersig.clone()),
    };

    let bytes = canonical_cbor_encode(&event).expect("encode");
    let decoded: SignedMembershipEvent = canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(decoded, event);
    assert_eq!(
        decoded.countersig.as_ref().map(|c| c.signer),
        Some(countersig.signer)
    );
}

#[test]
fn event_id_type_is_16_bytes() {
    let id: EventId = [0u8; 16];
    assert_eq!(std::mem::size_of_val(&id), 16);
}

use ed25519_dalek::{SigningKey, VerifyingKey};
use harmony_app::community_membership::{sign_event, EventPayload};

#[test]
fn sign_event_produces_signature_verifiable_with_pubkey() {
    let signing_key = SigningKey::from_bytes(&[42u8; 32]);
    let pubkey: VerifyingKey = signing_key.verifying_key();
    let actor = OwnerAddr({
        // Simplified: use first 16 bytes of raw pubkey as actor.
        // Real OwnerAddr is BLAKE3(pubkey)[..16] but sign_event
        // doesn't care, it just signs whatever bytes you hand it.
        let pk_bytes = pubkey.to_bytes();
        let mut a = [0u8; 16];
        a.copy_from_slice(&pk_bytes[..16]);
        a
    });

    let payload = EventPayload {
        id: [11u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor,
        at: Hlc {
            wall_ms: 1000,
            logical: 0,
            device_id: "d".into(),
        },
    };

    let event = sign_event(&payload, &signing_key).expect("sign");
    assert_eq!(event.id, payload.id);
    assert_eq!(event.actor, payload.actor);
    assert_eq!(event.kind, payload.kind);
    assert_eq!(event.countersig, None);

    // Verify the signature manually using ed25519-dalek directly.
    let signed_bytes = canonical_cbor_encode(&payload).expect("encode payload");
    pubkey
        .verify_strict(
            &signed_bytes,
            &ed25519_dalek::Signature::from_bytes(&event.sig),
        )
        .expect("signature must verify against signer pubkey");
}

use harmony_app::community_membership::{
    attach_countersig, verify_countersig, verify_signature, VerifyError,
};

#[test]
fn verify_signature_accepts_valid_event() {
    let signing_key = SigningKey::from_bytes(&[42u8; 32]);
    let pubkey = signing_key.verifying_key();

    let pk_bytes = pubkey.to_bytes();
    let mut actor_bytes = [0u8; 16];
    actor_bytes.copy_from_slice(&pk_bytes[..16]);
    let actor = OwnerAddr(actor_bytes);

    let payload = EventPayload {
        id: [11u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor,
        at: Hlc {
            wall_ms: 1000,
            logical: 0,
            device_id: "d".into(),
        },
    };

    let event = sign_event(&payload, &signing_key).expect("sign");
    verify_signature(&event, &pubkey).expect("must verify");
}

#[test]
fn verify_signature_rejects_tampered_event() {
    let signing_key = SigningKey::from_bytes(&[42u8; 32]);
    let pubkey = signing_key.verifying_key();
    let pk_bytes = pubkey.to_bytes();
    let mut actor_bytes = [0u8; 16];
    actor_bytes.copy_from_slice(&pk_bytes[..16]);
    let actor = OwnerAddr(actor_bytes);

    let payload = EventPayload {
        id: [11u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor,
        at: Hlc {
            wall_ms: 1000,
            logical: 0,
            device_id: "d".into(),
        },
    };

    let mut event = sign_event(&payload, &signing_key).expect("sign");
    // Tamper with the kind: flip Join to Leave. Sig was over the
    // original payload; verify must reject.
    event.kind = MembershipEventKind::Leave;

    let err = verify_signature(&event, &pubkey).expect_err("must reject tampered");
    assert!(matches!(err, VerifyError::SignatureInvalid));
}

#[test]
fn verify_signature_rejects_wrong_pubkey() {
    let signing_key_a = SigningKey::from_bytes(&[42u8; 32]);
    let signing_key_b = SigningKey::from_bytes(&[99u8; 32]);
    let pubkey_b = signing_key_b.verifying_key();

    let pk_bytes = signing_key_a.verifying_key().to_bytes();
    let mut actor_bytes = [0u8; 16];
    actor_bytes.copy_from_slice(&pk_bytes[..16]);
    let actor = OwnerAddr(actor_bytes);

    let payload = EventPayload {
        id: [11u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor,
        at: Hlc {
            wall_ms: 1000,
            logical: 0,
            device_id: "d".into(),
        },
    };

    let event = sign_event(&payload, &signing_key_a).expect("sign");
    let err = verify_signature(&event, &pubkey_b).expect_err("must reject wrong pubkey");
    assert!(matches!(err, VerifyError::SignatureInvalid));
}

#[test]
fn attach_and_verify_countersig_round_trip() {
    let actor_key = SigningKey::from_bytes(&[42u8; 32]);
    let inviter_key = SigningKey::from_bytes(&[55u8; 32]);
    let inviter_pubkey = inviter_key.verifying_key();

    let pk_bytes = actor_key.verifying_key().to_bytes();
    let mut actor_bytes = [0u8; 16];
    actor_bytes.copy_from_slice(&pk_bytes[..16]);
    let actor = OwnerAddr(actor_bytes);

    let inviter_pk_bytes = inviter_pubkey.to_bytes();
    let mut inviter_addr_bytes = [0u8; 16];
    inviter_addr_bytes.copy_from_slice(&inviter_pk_bytes[..16]);
    let inviter = OwnerAddr(inviter_addr_bytes);

    let payload = EventPayload {
        id: [11u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor,
        at: Hlc {
            wall_ms: 1000,
            logical: 0,
            device_id: "d".into(),
        },
    };

    let event = sign_event(&payload, &actor_key).expect("sign");
    let event_with_cs = attach_countersig(&event, inviter, &inviter_key).expect("countersign");

    assert!(event_with_cs.countersig.is_some());
    let cs = event_with_cs.countersig.as_ref().unwrap();
    assert_eq!(cs.signer, inviter);

    verify_countersig(&event_with_cs, &inviter_pubkey).expect("countersig must verify");
}

#[test]
fn verify_countersig_rejects_when_payload_changed_after_countersign() {
    let actor_key = SigningKey::from_bytes(&[42u8; 32]);
    let inviter_key = SigningKey::from_bytes(&[55u8; 32]);
    let inviter_pubkey = inviter_key.verifying_key();

    let pk_bytes = actor_key.verifying_key().to_bytes();
    let mut actor_bytes = [0u8; 16];
    actor_bytes.copy_from_slice(&pk_bytes[..16]);
    let actor = OwnerAddr(actor_bytes);

    let inviter_pk_bytes = inviter_pubkey.to_bytes();
    let mut inviter_addr_bytes = [0u8; 16];
    inviter_addr_bytes.copy_from_slice(&inviter_pk_bytes[..16]);
    let inviter = OwnerAddr(inviter_addr_bytes);

    let payload = EventPayload {
        id: [11u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor,
        at: Hlc {
            wall_ms: 1000,
            logical: 0,
            device_id: "d".into(),
        },
    };

    let event = sign_event(&payload, &actor_key).expect("sign");
    let mut event_with_cs = attach_countersig(&event, inviter, &inviter_key).expect("countersign");

    // Mutate the payload after counter-signing: change `at`. The
    // countersig was over the original payload bytes; verify must reject.
    event_with_cs.at = Hlc {
        wall_ms: 9999,
        logical: 0,
        device_id: "d".into(),
    };

    let err = verify_countersig(&event_with_cs, &inviter_pubkey)
        .expect_err("must reject mutated payload");
    // Note: verify_countersig may surface this as CounterSigInvalid
    // (since the attached countersig is stale) OR SignatureInvalid
    // (since the underlying ed25519 verify fails). The exact discriminant
    // depends on implementation. Accept either.
    assert!(matches!(
        err,
        VerifyError::CounterSigInvalid | VerifyError::SignatureInvalid
    ));
}

use harmony_app::community_membership::{
    MaterializedMembership, MemberStatus, PowerThresholds, POWER_THRESHOLDS,
};

#[test]
fn materialized_membership_is_constructible_and_default_empty() {
    let m = MaterializedMembership::default();
    assert!(m.members.is_empty());
    assert!(m.power_levels.is_empty());
}

#[test]
fn member_status_round_trips_through_canonical_cbor() {
    let statuses = [
        MemberStatus::Joined,
        MemberStatus::Invited,
        MemberStatus::Left,
        MemberStatus::Banned,
    ];
    for s in &statuses {
        let bytes = canonical_cbor_encode(s).expect("encode");
        let decoded: MemberStatus = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(decoded, *s);
    }
}

#[test]
fn power_thresholds_match_spec_defaults() {
    assert_eq!(POWER_THRESHOLDS.invite, 0);
    assert_eq!(POWER_THRESHOLDS.kick, 50);
    assert_eq!(POWER_THRESHOLDS.set_power, 100);
    assert_eq!(POWER_THRESHOLDS.max, 100);
}

#[test]
fn power_thresholds_struct_constructible() {
    let custom = PowerThresholds {
        invite: 10,
        kick: 60,
        set_power: 90,
        max: 100,
    };
    assert_eq!(custom.invite, 10);
}
