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

use harmony_app::community_membership::materialize;
use harmony_app::community_membership::{verify_event, VerifyContext};

fn make_signed(
    id: u8,
    kind: MembershipEventKind,
    actor: OwnerAddr,
    at_ms: u64,
) -> SignedMembershipEvent {
    let signing_key = SigningKey::from_bytes(&[42u8; 32]);
    let payload = EventPayload {
        id: [id; 16],
        community_id: SpaceId([3u8; 16]),
        kind,
        actor,
        at: Hlc {
            wall_ms: at_ms,
            logical: 0,
            device_id: "d".into(),
        },
    };
    sign_event(&payload, &signing_key).expect("sign")
}

#[test]
fn materialize_join_marks_actor_joined() {
    let admin = OwnerAddr([100u8; 16]);
    let alice = OwnerAddr([1u8; 16]);

    let events = vec![
        make_signed(1, MembershipEventKind::Join, admin, 100),
        make_signed(2, MembershipEventKind::Join, alice, 200),
    ];

    let m = materialize(&events, admin);
    assert_eq!(
        m.members.get(&admin).map(|s| s.status),
        Some(MemberStatus::Joined)
    );
    assert_eq!(
        m.members.get(&alice).map(|s| s.status),
        Some(MemberStatus::Joined)
    );
}

#[test]
fn materialize_leave_marks_actor_left_with_left_at() {
    let admin = OwnerAddr([100u8; 16]);
    let alice = OwnerAddr([1u8; 16]);

    let events = vec![
        make_signed(1, MembershipEventKind::Join, admin, 100),
        make_signed(2, MembershipEventKind::Join, alice, 200),
        make_signed(3, MembershipEventKind::Leave, alice, 300),
    ];

    let m = materialize(&events, admin);
    let alice_state = m.members.get(&alice).expect("alice present");
    assert_eq!(alice_state.status, MemberStatus::Left);
    assert_eq!(alice_state.left_at.as_ref().map(|h| h.wall_ms), Some(300));
}

#[test]
fn materialize_kick_marks_target_banned() {
    let admin = OwnerAddr([100u8; 16]);
    let bob = OwnerAddr([2u8; 16]);

    let events = vec![
        make_signed(1, MembershipEventKind::Join, admin, 100),
        make_signed(2, MembershipEventKind::Join, bob, 200),
        make_signed(
            3,
            MembershipEventKind::Kick {
                target: bob,
                reason: Some("spam".into()),
            },
            admin,
            300,
        ),
    ];

    let m = materialize(&events, admin);
    let bob_state = m.members.get(&bob).expect("bob present");
    assert_eq!(bob_state.status, MemberStatus::Banned);
    assert_eq!(bob_state.left_at.as_ref().map(|h| h.wall_ms), Some(300));
}

#[test]
fn materialize_invite_marks_target_invited() {
    let admin = OwnerAddr([100u8; 16]);
    let carol = OwnerAddr([3u8; 16]);

    let events = vec![
        make_signed(1, MembershipEventKind::Join, admin, 100),
        make_signed(2, MembershipEventKind::Invite { target: carol }, admin, 200),
    ];

    let m = materialize(&events, admin);
    let carol_state = m.members.get(&carol).expect("carol present");
    assert_eq!(carol_state.status, MemberStatus::Invited);
    assert!(carol_state.left_at.is_none());
}

#[test]
fn materialize_setpower_updates_power_level() {
    let admin = OwnerAddr([100u8; 16]);
    let bob = OwnerAddr([2u8; 16]);

    let events = vec![
        make_signed(1, MembershipEventKind::Join, admin, 100),
        make_signed(2, MembershipEventKind::Join, bob, 200),
        make_signed(
            3,
            MembershipEventKind::SetPower {
                target: bob,
                level: 75,
            },
            admin,
            300,
        ),
    ];

    let m = materialize(&events, admin);
    assert_eq!(m.power_levels.get(&bob).copied(), Some(75));
}

#[test]
fn materialize_bootstrap_grants_admin_power_100_even_with_zero_events() {
    let admin = OwnerAddr([100u8; 16]);
    let m = materialize(&[], admin);
    assert_eq!(m.power_levels.get(&admin).copied(), Some(100));
    // But admin is NOT a member until they Join (intentional — admin
    // is a power designation, not a membership status).
    assert!(m.members.is_empty());
}

#[test]
fn materialize_setpower_overrides_admin_bootstrap() {
    let admin = OwnerAddr([100u8; 16]);
    let new_admin = OwnerAddr([99u8; 16]);

    let events = vec![
        make_signed(1, MembershipEventKind::Join, admin, 100),
        make_signed(2, MembershipEventKind::Join, new_admin, 200),
        make_signed(
            3,
            MembershipEventKind::SetPower {
                target: new_admin,
                level: 100,
            },
            admin,
            300,
        ),
        make_signed(
            4,
            MembershipEventKind::SetPower {
                target: admin,
                level: 0,
            },
            admin,
            400,
        ),
    ];

    let m = materialize(&events, admin);
    assert_eq!(m.power_levels.get(&admin).copied(), Some(0));
    assert_eq!(m.power_levels.get(&new_admin).copied(), Some(100));
}

#[test]
fn materialize_total_order_uses_event_id_tiebreaker_when_hlc_collides() {
    // Two SetPower events on the same target with identical HLC tuples
    // but different EventIds. Sort must be deterministic — the same
    // input order should produce the same final state regardless of
    // how DAG-sync presented the events to us.
    //
    // Fixed total-order rule: (wall_ms, logical, device_id, event.id).
    // EventId is the deterministic, unique tiebreaker.
    let admin = OwnerAddr([100u8; 16]);
    let bob = OwnerAddr([2u8; 16]);
    let admin_key = SigningKey::from_bytes(&[100u8; 32]);

    let mk = |id: u8, level: u8| {
        let payload = EventPayload {
            id: [id; 16],
            community_id: SpaceId([3u8; 16]),
            kind: MembershipEventKind::SetPower { target: bob, level },
            actor: admin,
            at: Hlc {
                wall_ms: 500,
                logical: 0,
                device_id: "d".into(),
            },
        };
        sign_event(&payload, &admin_key).expect("sign")
    };

    // ID [1; 16] sorts before [2; 16]; under tiebreaker, the level-from-id-1
    // event applies first and the level-from-id-2 event wins.
    let bootstrap = vec![make_signed(0, MembershipEventKind::Join, admin, 100)];
    let collide_a = mk(1, 30);
    let collide_b = mk(2, 70);

    let mut order_1 = bootstrap.clone();
    order_1.push(collide_a.clone());
    order_1.push(collide_b.clone());

    let mut order_2 = bootstrap.clone();
    order_2.push(collide_b.clone());
    order_2.push(collide_a.clone());

    let m1 = materialize(&order_1, admin);
    let m2 = materialize(&order_2, admin);

    assert_eq!(
        m1, m2,
        "materialize must converge regardless of input order when HLCs collide"
    );
    assert_eq!(
        m1.power_levels.get(&bob).copied(),
        Some(70),
        "later EventId (id=2) must win the tie"
    );
}

#[test]
fn materialize_replays_in_hlc_order_not_input_order() {
    // Events arrive in a different order than they should apply.
    // Materialization must re-sort by HLC.
    let admin = OwnerAddr([100u8; 16]);
    let bob = OwnerAddr([2u8; 16]);

    let events = vec![
        // Out of order: kick at 300 listed BEFORE join at 200.
        make_signed(
            3,
            MembershipEventKind::Kick {
                target: bob,
                reason: None,
            },
            admin,
            300,
        ),
        make_signed(2, MembershipEventKind::Join, bob, 200),
        make_signed(1, MembershipEventKind::Join, admin, 100),
    ];

    let m = materialize(&events, admin);
    // Despite the input order, the replay walks HLC ascending, so:
    // 100: admin joins, 200: bob joins, 300: bob is kicked.
    assert_eq!(
        m.members.get(&bob).map(|s| s.status),
        Some(MemberStatus::Banned)
    );
}

#[test]
fn verify_event_accepts_valid_join_in_open_community() {
    let admin = OwnerAddr([100u8; 16]);
    let alice = OwnerAddr([1u8; 16]);
    let alice_key = SigningKey::from_bytes(&[1u8; 32]);

    // Pre-existing materialized state: admin has joined.
    let prior_state = materialize(
        &[make_signed(1, MembershipEventKind::Join, admin, 100)],
        admin,
    );

    // Alice signs her join event.
    let payload = EventPayload {
        id: [2u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor: alice,
        at: Hlc {
            wall_ms: 200,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event(&payload, &alice_key).expect("sign");

    let alice_pubkey = alice_key.verifying_key();
    let ctx = VerifyContext {
        is_invite_only: false,
        actor_pubkey: &alice_pubkey,
        countersigner_pubkey: None,
    };

    verify_event(&event, &prior_state, &ctx).expect("must accept");
}

#[test]
fn verify_event_rejects_invite_only_join_without_countersig() {
    let admin = OwnerAddr([100u8; 16]);
    let alice = OwnerAddr([1u8; 16]);
    let alice_key = SigningKey::from_bytes(&[1u8; 32]);

    let prior_state = materialize(&[], admin);

    let payload = EventPayload {
        id: [2u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor: alice,
        at: Hlc {
            wall_ms: 200,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event(&payload, &alice_key).expect("sign");

    let alice_pubkey = alice_key.verifying_key();
    let ctx = VerifyContext {
        is_invite_only: true,
        actor_pubkey: &alice_pubkey,
        countersigner_pubkey: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::CounterSigRequired);
}

#[test]
fn verify_event_accepts_invite_only_join_with_valid_countersig() {
    let admin = OwnerAddr([100u8; 16]);
    let admin_key = SigningKey::from_bytes(&[100u8; 32]);
    let alice = OwnerAddr([1u8; 16]);
    let alice_key = SigningKey::from_bytes(&[1u8; 32]);

    let prior_state = materialize(
        &[make_signed(1, MembershipEventKind::Join, admin, 100)],
        admin,
    );

    let payload = EventPayload {
        id: [2u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor: alice,
        at: Hlc {
            wall_ms: 200,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event(&payload, &alice_key).expect("sign");
    let event = attach_countersig(&event, admin, &admin_key).expect("countersign");

    let alice_pubkey = alice_key.verifying_key();
    let admin_pubkey = admin_key.verifying_key();
    let ctx = VerifyContext {
        is_invite_only: true,
        actor_pubkey: &alice_pubkey,
        countersigner_pubkey: Some(&admin_pubkey),
    };

    verify_event(&event, &prior_state, &ctx).expect("must accept");
}

#[test]
fn verify_event_rejects_kick_when_actor_power_below_threshold() {
    let admin = OwnerAddr([100u8; 16]);
    let alice = OwnerAddr([1u8; 16]);
    let bob = OwnerAddr([2u8; 16]);
    let alice_key = SigningKey::from_bytes(&[1u8; 32]);

    let prior_state = materialize(
        &[
            make_signed(1, MembershipEventKind::Join, admin, 100),
            make_signed(2, MembershipEventKind::Join, alice, 200),
            make_signed(3, MembershipEventKind::Join, bob, 300),
        ],
        admin,
    );

    let payload = EventPayload {
        id: [4u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Kick {
            target: bob,
            reason: None,
        },
        actor: alice,
        at: Hlc {
            wall_ms: 400,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event(&payload, &alice_key).expect("sign");

    let alice_pubkey = alice_key.verifying_key();
    let ctx = VerifyContext {
        is_invite_only: false,
        actor_pubkey: &alice_pubkey,
        countersigner_pubkey: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::ActorPowerInsufficient);
}

#[test]
fn verify_event_rejects_kick_when_target_power_equals_actor() {
    let admin = OwnerAddr([100u8; 16]);
    let admin_key = SigningKey::from_bytes(&[100u8; 32]);
    let admin2 = OwnerAddr([99u8; 16]);

    let prior_state = materialize(
        &[
            make_signed(1, MembershipEventKind::Join, admin, 100),
            make_signed(2, MembershipEventKind::Join, admin2, 200),
            make_signed(
                3,
                MembershipEventKind::SetPower {
                    target: admin2,
                    level: 100,
                },
                admin,
                300,
            ),
        ],
        admin,
    );

    let payload = EventPayload {
        id: [4u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Kick {
            target: admin2,
            reason: None,
        },
        actor: admin,
        at: Hlc {
            wall_ms: 400,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event(&payload, &admin_key).expect("sign");

    let admin_pubkey = admin_key.verifying_key();
    let ctx = VerifyContext {
        is_invite_only: false,
        actor_pubkey: &admin_pubkey,
        countersigner_pubkey: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::KickTargetPowerNotLower);
}

#[test]
fn verify_event_rejects_setpower_when_actor_power_insufficient() {
    let admin = OwnerAddr([100u8; 16]);
    let alice = OwnerAddr([1u8; 16]);
    let bob = OwnerAddr([2u8; 16]);
    let alice_key = SigningKey::from_bytes(&[1u8; 32]);

    let prior_state = materialize(
        &[
            make_signed(1, MembershipEventKind::Join, admin, 100),
            make_signed(2, MembershipEventKind::Join, alice, 200),
        ],
        admin,
    );

    let payload = EventPayload {
        id: [3u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::SetPower {
            target: bob,
            level: 50,
        },
        actor: alice,
        at: Hlc {
            wall_ms: 300,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event(&payload, &alice_key).expect("sign");

    let alice_pubkey = alice_key.verifying_key();
    let ctx = VerifyContext {
        is_invite_only: false,
        actor_pubkey: &alice_pubkey,
        countersigner_pubkey: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::ActorPowerInsufficient);
}

// Note: VerifyError::CounterSigPowerInsufficient is unreachable
// under v1's hardcoded POWER_THRESHOLDS.invite = 0 because every
// owner address (whether a joined member or not) materializes to
// power ≥ 0. The variant is reserved for ZEB-251 (per-community
// threshold customization). When ZEB-251 ships, add a test here
// that constructs a custom-threshold scenario and exercises this
// rejection path.

#[test]
fn verify_event_rejects_join_replay_after_kick() {
    // After a kicked actor's status materializes to Banned, a replayed
    // (or fresh) Join from that actor must be rejected — otherwise
    // materialize() would silently overwrite Banned back to Joined,
    // making Kick effectively cosmetic.
    //
    // Kick = effective ban until an explicit unban flow exists
    // (deferred to a follow-up).
    let admin = OwnerAddr([100u8; 16]);
    let admin_key = SigningKey::from_bytes(&[100u8; 32]);
    let alice = OwnerAddr([1u8; 16]);
    let alice_key = SigningKey::from_bytes(&[1u8; 32]);

    // alice joined, then was kicked by admin.
    let prior_state = materialize(
        &[
            make_signed(1, MembershipEventKind::Join, admin, 100),
            {
                let payload = EventPayload {
                    id: [2u8; 16],
                    community_id: SpaceId([3u8; 16]),
                    kind: MembershipEventKind::Join,
                    actor: alice,
                    at: Hlc {
                        wall_ms: 200,
                        logical: 0,
                        device_id: "d".into(),
                    },
                };
                sign_event(&payload, &alice_key).expect("sign")
            },
            {
                let payload = EventPayload {
                    id: [3u8; 16],
                    community_id: SpaceId([3u8; 16]),
                    kind: MembershipEventKind::Kick {
                        target: alice,
                        reason: Some("spam".into()),
                    },
                    actor: admin,
                    at: Hlc {
                        wall_ms: 300,
                        logical: 0,
                        device_id: "d".into(),
                    },
                };
                sign_event(&payload, &admin_key).expect("sign")
            },
        ],
        admin,
    );
    assert_eq!(
        prior_state.members.get(&alice).map(|s| s.status),
        Some(MemberStatus::Banned),
        "test setup: alice must be Banned in prior_state"
    );

    // Alice (or someone with her key) re-publishes a Join.
    let payload = EventPayload {
        id: [4u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor: alice,
        at: Hlc {
            wall_ms: 400,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event(&payload, &alice_key).expect("sign");

    let alice_pubkey = alice_key.verifying_key();
    let ctx = VerifyContext {
        is_invite_only: false,
        actor_pubkey: &alice_pubkey,
        countersigner_pubkey: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::BannedActorJoin);
}

#[test]
fn verify_event_rejects_setpower_when_level_exceeds_max() {
    // POWER_THRESHOLDS.max = 100. An admin (power 100) authorized to
    // SetPower must NOT be able to assign 200/255/etc. — the cap is
    // structural to the moderation model (no member can hold a power
    // higher than admin can revoke).
    let admin = OwnerAddr([100u8; 16]);
    let admin_key = SigningKey::from_bytes(&[100u8; 32]);
    let bob = OwnerAddr([2u8; 16]);

    let prior_state = materialize(
        &[
            make_signed(1, MembershipEventKind::Join, admin, 100),
            make_signed(2, MembershipEventKind::Join, bob, 200),
        ],
        admin,
    );

    let payload = EventPayload {
        id: [3u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::SetPower {
            target: bob,
            level: 200, // > POWER_THRESHOLDS.max (100)
        },
        actor: admin,
        at: Hlc {
            wall_ms: 300,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event(&payload, &admin_key).expect("sign");

    let admin_pubkey = admin_key.verifying_key();
    let ctx = VerifyContext {
        is_invite_only: false,
        actor_pubkey: &admin_pubkey,
        countersigner_pubkey: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::PowerLevelOutOfRange);
}

#[test]
fn verify_event_accepts_setpower_at_max_boundary() {
    // Boundary check: level == POWER_THRESHOLDS.max (100) is allowed.
    let admin = OwnerAddr([100u8; 16]);
    let admin_key = SigningKey::from_bytes(&[100u8; 32]);
    let bob = OwnerAddr([2u8; 16]);

    let prior_state = materialize(
        &[
            make_signed(1, MembershipEventKind::Join, admin, 100),
            make_signed(2, MembershipEventKind::Join, bob, 200),
        ],
        admin,
    );

    let payload = EventPayload {
        id: [3u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::SetPower {
            target: bob,
            level: POWER_THRESHOLDS.max, // exactly 100
        },
        actor: admin,
        at: Hlc {
            wall_ms: 300,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event(&payload, &admin_key).expect("sign");

    let admin_pubkey = admin_key.verifying_key();
    let ctx = VerifyContext {
        is_invite_only: false,
        actor_pubkey: &admin_pubkey,
        countersigner_pubkey: None,
    };

    verify_event(&event, &prior_state, &ctx).expect("must accept level == max");
}

#[test]
fn verify_event_rejects_when_actor_signature_invalid() {
    let admin = OwnerAddr([100u8; 16]);
    let alice = OwnerAddr([1u8; 16]);
    let alice_key = SigningKey::from_bytes(&[1u8; 32]);
    let bob_key = SigningKey::from_bytes(&[2u8; 32]); // different signer

    let prior_state = materialize(
        &[make_signed(1, MembershipEventKind::Join, admin, 100)],
        admin,
    );

    let payload = EventPayload {
        id: [2u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor: alice,
        at: Hlc {
            wall_ms: 200,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event(&payload, &alice_key).expect("sign");

    let bob_pubkey = bob_key.verifying_key();
    let ctx = VerifyContext {
        is_invite_only: false,
        actor_pubkey: &bob_pubkey, // wrong pubkey for actor
        countersigner_pubkey: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::SignatureInvalid);
}
