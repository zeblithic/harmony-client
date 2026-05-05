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

use ed25519_dalek::SigningKey;
use harmony_app::community_membership::{sign_event, sign_event_with_identity, EventPayload};
use harmony_identity::PrivateIdentity;

/// Build a deterministic test identity from a one-byte seed.
/// Returns (private, identity_pub, owner_addr) where owner_addr ==
/// OwnerAddr(identity.address_hash). The identity_pub is the canonical
/// 64-byte combined `X25519_pub || Ed25519_pub` blob.
///
/// Use this anywhere a test needs a real OwnerAddr/identity_pub pair
/// — verify_signature and verify_countersig now derive address_hash
/// from identity_pub and check it against event.actor / cs.signer,
/// so arbitrary OwnerAddr bytes will not pass the binding check.
fn make_test_identity(seed: u8) -> (PrivateIdentity, [u8; 64], OwnerAddr) {
    let private = PrivateIdentity::from_seed(&[seed; 32]);
    let identity_pub = private.identity.to_public_bytes();
    let owner_addr = OwnerAddr(private.identity.address_hash);
    (private, identity_pub, owner_addr)
}

#[test]
fn sign_event_produces_signature_verifiable_with_pubkey() {
    // Low-level sign_event taking SigningKey. Production callers use
    // sign_event_with_identity (covered by the verify_signature_* tests
    // below). This test pins the SigningKey path for completeness.
    let signing_key = SigningKey::from_bytes(&[42u8; 32]);
    let pubkey = signing_key.verifying_key();
    let actor = OwnerAddr([0u8; 16]); // arbitrary — sign_event doesn't bind

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
    attach_countersig_with_identity, verify_countersig, verify_signature, VerifyError,
};

#[test]
fn verify_signature_accepts_valid_event() {
    let (private, identity_pub, actor) = make_test_identity(42);

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

    let event = sign_event_with_identity(&payload, &private).expect("sign");
    verify_signature(&event, &identity_pub).expect("must verify");
}

#[test]
fn verify_signature_rejects_tampered_event() {
    let (private, identity_pub, actor) = make_test_identity(42);

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

    let mut event = sign_event_with_identity(&payload, &private).expect("sign");
    // Tamper with the kind: flip Join to Leave. Sig was over the
    // original payload; verify must reject.
    event.kind = MembershipEventKind::Leave;

    let err = verify_signature(&event, &identity_pub).expect_err("must reject tampered");
    assert_eq!(err, VerifyError::SignatureInvalid);
}

#[test]
fn verify_signature_rejects_pubkey_not_matching_actor() {
    // Bob signs an event but claims actor=alice. The Ed25519 sig
    // would verify against bob's pubkey alone, but verify_signature
    // now derives the OwnerAddr from the supplied identity_pub and
    // checks it matches event.actor — so passing bob's identity_pub
    // for an event with actor=alice surfaces as ActorPubkeyMismatch
    // (defends against caller cache-lookup bugs and key-substitution
    // attacks where attacker's pubkey is paired with victim's claimed
    // identity).
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);
    let (_bob_priv, bob_id_pub, _bob) = make_test_identity(2);

    let payload = EventPayload {
        id: [11u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor: alice,
        at: Hlc {
            wall_ms: 1000,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &alice_priv).expect("sign");

    let err = verify_signature(&event, &bob_id_pub).expect_err("must reject wrong identity");
    assert_eq!(err, VerifyError::ActorPubkeyMismatch);
}

#[test]
fn attach_and_verify_countersig_round_trip() {
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);
    let (admin_priv, admin_id_pub, admin) = make_test_identity(100);

    let payload = EventPayload {
        id: [11u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor: alice,
        at: Hlc {
            wall_ms: 1000,
            logical: 0,
            device_id: "d".into(),
        },
    };

    let event = sign_event_with_identity(&payload, &alice_priv).expect("sign");
    let event_with_cs = attach_countersig_with_identity(&event, &admin_priv).expect("countersign");

    assert!(event_with_cs.countersig.is_some());
    let cs = event_with_cs.countersig.as_ref().unwrap();
    assert_eq!(cs.signer, admin);

    verify_countersig(&event_with_cs, &admin_id_pub).expect("countersig must verify");
}

#[test]
fn verify_countersig_rejects_when_payload_changed_after_countersign() {
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);
    let (admin_priv, admin_id_pub, _admin) = make_test_identity(100);

    let payload = EventPayload {
        id: [11u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor: alice,
        at: Hlc {
            wall_ms: 1000,
            logical: 0,
            device_id: "d".into(),
        },
    };

    let event = sign_event_with_identity(&payload, &alice_priv).expect("sign");
    let mut event_with_cs =
        attach_countersig_with_identity(&event, &admin_priv).expect("countersign");

    // Mutate the payload after counter-signing: change `at`. The
    // countersig was over the original payload bytes; verify must reject.
    event_with_cs.at = Hlc {
        wall_ms: 9999,
        logical: 0,
        device_id: "d".into(),
    };

    let err =
        verify_countersig(&event_with_cs, &admin_id_pub).expect_err("must reject mutated payload");
    // verify_countersig may surface this as CounterSigInvalid (the
    // attached sig doesn't match the new payload bytes).
    assert!(matches!(
        err,
        VerifyError::CounterSigInvalid | VerifyError::SignatureInvalid
    ));
}

#[test]
fn verify_countersig_rejects_pubkey_not_matching_signer() {
    // The attack from PR #82 review (Qodo + qodo-code-review): valid
    // countersig from key A, but cs.signer claims address B (typically
    // a higher-power signer). Without binding, the Ed25519 verify
    // would pass (sig is from A, pubkey is A) and the power lookup
    // would credit B's authority — bypassing invite-only authorization.
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);
    let (admin_priv, _admin_id_pub, _admin) = make_test_identity(100);
    let (_carol_priv, carol_id_pub, carol) = make_test_identity(50);

    let payload = EventPayload {
        id: [11u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor: alice,
        at: Hlc {
            wall_ms: 1000,
            logical: 0,
            device_id: "d".into(),
        },
    };

    let event = sign_event_with_identity(&payload, &alice_priv).expect("sign");
    // Forge a countersig with cs.signer=carol but signed by admin's key.
    // (Real attack: admin signs, claims carol, hopes verifier looks up
    //  carol's pubkey and rejects only on signature mismatch — but the
    //  binding check fires first.)
    let mut event = event;
    let payload_for_cs = EventPayload {
        id: event.id,
        community_id: event.community_id,
        kind: event.kind.clone(),
        actor: event.actor,
        at: event.at.clone(),
    };
    let bytes = canonical_cbor_encode(&payload_for_cs).expect("encode");
    let admin_sig = admin_priv.sign(&bytes);
    event.countersig = Some(harmony_app::community_membership::CounterSignature {
        signer: carol,  // claim carol
        sig: admin_sig, // but signed by admin
    });

    let err = verify_countersig(&event, &carol_id_pub).expect_err("must reject");
    // Either:
    //   - verifier picks up carol's identity_pub (matches cs.signer ✓)
    //     but the sig is from admin → CounterSigInvalid
    // OR (the bot-flagged scenario where caller passes admin's pubkey
    // by mistake):
    //   - verifier passes admin's identity_pub → derives admin's address
    //     hash → mismatches cs.signer (carol) → CounterSignerPubkeyMismatch
    // The first path fires here (we passed carol_id_pub). Both are
    // correct rejections.
    assert!(matches!(
        err,
        VerifyError::CounterSigInvalid | VerifyError::CounterSignerPubkeyMismatch
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

use harmony_app::community_membership::{event_sort_key, materialize, prior_state_at_event};
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
fn prior_state_at_event_excludes_target_and_later_events() {
    // The helper computes the materialized state STRICTLY BEFORE the
    // target event in canonical (HLC, EventId, sig) order. Same-HLC
    // events that sort *after* the target must NOT be in the prior
    // state.
    let admin = OwnerAddr([100u8; 16]);
    let alice = OwnerAddr([1u8; 16]);
    let bob = OwnerAddr([2u8; 16]);

    let admin_join = make_signed(1, MembershipEventKind::Join, admin, 100);
    let alice_join = make_signed(2, MembershipEventKind::Join, alice, 200);
    let target = make_signed(3, MembershipEventKind::Join, bob, 300);
    let after_target = make_signed(4, MembershipEventKind::Leave, bob, 400);

    let prior = prior_state_at_event(
        &[
            after_target.clone(),
            target.clone(),
            alice_join.clone(),
            admin_join.clone(),
        ],
        &target,
        admin,
    );

    assert_eq!(
        prior.members.get(&admin).map(|s| s.status),
        Some(MemberStatus::Joined)
    );
    assert_eq!(
        prior.members.get(&alice).map(|s| s.status),
        Some(MemberStatus::Joined)
    );
    assert_eq!(
        prior.members.get(&bob),
        None,
        "target itself must be excluded"
    );
}

#[test]
fn event_sort_key_total_order_matches_materialize() {
    // The exposed sort_key must agree with materialize's internal sort
    // — that's the contract callers depend on. This test pins the
    // equivalence so any future divergence (e.g., adding a 6th
    // tiebreaker without updating the public helper) breaks loudly.
    let admin = OwnerAddr([100u8; 16]);
    let admin_key = SigningKey::from_bytes(&[100u8; 32]);

    let mk = |id: u8, level: u8| {
        let payload = EventPayload {
            id: [id; 16],
            community_id: SpaceId([3u8; 16]),
            kind: MembershipEventKind::SetPower {
                target: OwnerAddr([99u8; 16]),
                level,
            },
            actor: admin,
            at: Hlc {
                wall_ms: 500,
                logical: 0,
                device_id: "d".into(),
            },
        };
        sign_event(&payload, &admin_key).expect("sign")
    };

    let a = mk(1, 30);
    let b = mk(2, 70);

    // a's id [1; 16] < b's id [2; 16] under the comparator.
    assert!(event_sort_key(&a) < event_sort_key(&b));

    // Replay both orders and confirm they converge — same property
    // materialize delivers internally.
    let bootstrap = vec![make_signed(0, MembershipEventKind::Join, admin, 100)];
    let mut o1 = bootstrap.clone();
    o1.push(a.clone());
    o1.push(b.clone());
    let mut o2 = bootstrap.clone();
    o2.push(b.clone());
    o2.push(a.clone());
    assert_eq!(materialize(&o1, admin), materialize(&o2, admin));
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
fn materialize_total_order_holds_when_event_id_is_reused() {
    // EventId is caller-supplied. A buggy or malicious peer could emit
    // two distinct SignedMembershipEvents with identical (HLC, EventId)
    // but conflicting content (different `level` on the same target).
    // The sort must still produce a total order across replicas —
    // appending sig as the final tiebreaker makes the final state
    // independent of input order.
    let admin = OwnerAddr([100u8; 16]);
    let admin_key = SigningKey::from_bytes(&[100u8; 32]);
    let bob = OwnerAddr([2u8; 16]);

    // Two SetPower events on the same target with same id + same HLC
    // but different levels → they produce different sigs (sig is over
    // canonical CBOR which includes level), so sig-based tiebreaking
    // picks a deterministic winner.
    let mk = |level: u8| {
        let payload = EventPayload {
            id: [1u8; 16], // collide on EventId
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

    let bootstrap = vec![make_signed(0, MembershipEventKind::Join, admin, 100)];
    let collide_a = mk(30);
    let collide_b = mk(70);

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
        "materialize must converge even when EventId is reused — sig is the final tiebreaker"
    );
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
    let (_admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, alice_id_pub, alice) = make_test_identity(1);

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
    let event = sign_event_with_identity(&payload, &alice_priv).expect("sign");

    let ctx = VerifyContext {
        expected_community_id: SpaceId([3u8; 16]),
        is_invite_only: false,
        actor_identity_pub: &alice_id_pub,
        countersigner_identity_pub: None,
    };

    verify_event(&event, &prior_state, &ctx).expect("must accept");
}

#[test]
fn verify_event_rejects_invite_only_join_without_countersig() {
    let (_admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, alice_id_pub, alice) = make_test_identity(1);

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
    let event = sign_event_with_identity(&payload, &alice_priv).expect("sign");

    let ctx = VerifyContext {
        expected_community_id: SpaceId([3u8; 16]),
        is_invite_only: true,
        actor_identity_pub: &alice_id_pub,
        countersigner_identity_pub: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::CounterSigRequired);
}

#[test]
fn verify_event_accepts_invite_only_join_with_valid_countersig() {
    let (admin_priv, admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, alice_id_pub, alice) = make_test_identity(1);

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
    let event = sign_event_with_identity(&payload, &alice_priv).expect("sign");
    let event = attach_countersig_with_identity(&event, &admin_priv).expect("countersign");

    let ctx = VerifyContext {
        expected_community_id: SpaceId([3u8; 16]),
        is_invite_only: true,
        actor_identity_pub: &alice_id_pub,
        countersigner_identity_pub: Some(&admin_id_pub),
    };

    verify_event(&event, &prior_state, &ctx).expect("must accept");
}

#[test]
fn verify_event_rejects_kick_when_actor_power_below_threshold() {
    let (_admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, alice_id_pub, alice) = make_test_identity(1);
    let (_bob_priv, _bob_id_pub, bob) = make_test_identity(2);

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
    let event = sign_event_with_identity(&payload, &alice_priv).expect("sign");

    let ctx = VerifyContext {
        expected_community_id: SpaceId([3u8; 16]),
        is_invite_only: false,
        actor_identity_pub: &alice_id_pub,
        countersigner_identity_pub: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::ActorPowerInsufficient);
}

#[test]
fn verify_event_rejects_kick_on_target_who_never_joined() {
    // An admin can sign a Kick targeting any OwnerAddr they please —
    // power and actor-membership checks pass. But if target ∉ members,
    // materialize() would insert a brand-new MemberState with status
    // Banned and joined_at=kick_time, claiming the target was a member
    // when they never were. Reject at verify time to keep state honest.
    let (admin_priv, admin_id_pub, admin) = make_test_identity(100);
    let stranger = OwnerAddr([0xEEu8; 16]); // never appeared in any event

    let prior_state = materialize(
        &[make_signed(1, MembershipEventKind::Join, admin, 100)],
        admin,
    );
    assert!(
        prior_state.members.get(&stranger).is_none(),
        "test setup: stranger must not be in prior state"
    );

    let payload = EventPayload {
        id: [4u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Kick {
            target: stranger,
            reason: None,
        },
        actor: admin,
        at: Hlc {
            wall_ms: 200,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &admin_priv).expect("sign");

    let ctx = VerifyContext {
        expected_community_id: SpaceId([3u8; 16]),
        is_invite_only: false,
        actor_identity_pub: &admin_id_pub,
        countersigner_identity_pub: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::KickTargetNotMember);
}

#[test]
fn verify_event_accepts_kick_on_left_member() {
    // Banning a recently-Left member is a legitimate use case (admin
    // wants to make sure they don't come back). Target = Left should
    // still pass the membership check — they ARE in the members map,
    // just not Joined. Power-side checks then govern the rest.
    let (admin_priv, admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);

    let alice_join = {
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
        sign_event_with_identity(&payload, &alice_priv).expect("sign")
    };
    let alice_leave = {
        let payload = EventPayload {
            id: [3u8; 16],
            community_id: SpaceId([3u8; 16]),
            kind: MembershipEventKind::Leave,
            actor: alice,
            at: Hlc {
                wall_ms: 300,
                logical: 0,
                device_id: "d".into(),
            },
        };
        sign_event_with_identity(&payload, &alice_priv).expect("sign")
    };
    let prior_state = materialize(
        &[
            make_signed(1, MembershipEventKind::Join, admin, 100),
            alice_join,
            alice_leave,
        ],
        admin,
    );
    assert_eq!(
        prior_state.members.get(&alice).map(|s| s.status),
        Some(MemberStatus::Left),
    );

    let payload = EventPayload {
        id: [4u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Kick {
            target: alice,
            reason: Some("post-departure ban".into()),
        },
        actor: admin,
        at: Hlc {
            wall_ms: 400,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &admin_priv).expect("sign");

    let ctx = VerifyContext {
        expected_community_id: SpaceId([3u8; 16]),
        is_invite_only: false,
        actor_identity_pub: &admin_id_pub,
        countersigner_identity_pub: None,
    };

    verify_event(&event, &prior_state, &ctx).expect("must accept Left → Banned");
}

#[test]
fn verify_event_rejects_kick_when_target_power_equals_actor() {
    let (admin_priv, admin_id_pub, admin) = make_test_identity(100);
    let (_admin2_priv, _admin2_id_pub, admin2) = make_test_identity(99);

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
    let event = sign_event_with_identity(&payload, &admin_priv).expect("sign");

    let ctx = VerifyContext {
        expected_community_id: SpaceId([3u8; 16]),
        is_invite_only: false,
        actor_identity_pub: &admin_id_pub,
        countersigner_identity_pub: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::KickTargetPowerNotLower);
}

#[test]
fn verify_event_rejects_setpower_when_actor_power_insufficient() {
    let (_admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, alice_id_pub, alice) = make_test_identity(1);
    let (_bob_priv, _bob_id_pub, bob) = make_test_identity(2);

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
    let event = sign_event_with_identity(&payload, &alice_priv).expect("sign");

    let ctx = VerifyContext {
        expected_community_id: SpaceId([3u8; 16]),
        is_invite_only: false,
        actor_identity_pub: &alice_id_pub,
        countersigner_identity_pub: None,
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
fn verify_event_rejects_invite_from_non_joined_actor() {
    // Under v1 POWER_THRESHOLDS.invite = 0, the power check alone
    // accepts anyone — so a non-member can otherwise emit a valid
    // Invite. Membership must be the operative gate.
    let (_admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, alice_id_pub, alice) = make_test_identity(1);
    let (_bob_priv, _bob_id_pub, bob) = make_test_identity(2);

    // Only admin has joined; alice has NOT.
    let prior_state = materialize(
        &[make_signed(1, MembershipEventKind::Join, admin, 100)],
        admin,
    );

    let payload = EventPayload {
        id: [2u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Invite { target: bob },
        actor: alice, // non-member!
        at: Hlc {
            wall_ms: 200,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &alice_priv).expect("sign");

    let ctx = VerifyContext {
        expected_community_id: SpaceId([3u8; 16]),
        is_invite_only: false,
        actor_identity_pub: &alice_id_pub,
        countersigner_identity_pub: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::ActorNotJoined);
}

#[test]
fn verify_event_rejects_kick_from_non_joined_actor() {
    let (_admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, alice_id_pub, alice) = make_test_identity(1);
    let (_bob_priv, _bob_id_pub, bob) = make_test_identity(2);

    // Alice has high power (somehow assigned), but never Joined.
    // She must still be rejected from kicking — power without
    // membership is meaningless.
    let prior_state = materialize(
        &[
            make_signed(1, MembershipEventKind::Join, admin, 100),
            make_signed(2, MembershipEventKind::Join, bob, 200),
            make_signed(
                3,
                MembershipEventKind::SetPower {
                    target: alice,
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
            target: bob,
            reason: None,
        },
        actor: alice, // non-member!
        at: Hlc {
            wall_ms: 400,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &alice_priv).expect("sign");

    let ctx = VerifyContext {
        expected_community_id: SpaceId([3u8; 16]),
        is_invite_only: false,
        actor_identity_pub: &alice_id_pub,
        countersigner_identity_pub: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::ActorNotJoined);
}

#[test]
fn verify_event_rejects_setpower_from_non_joined_actor() {
    let (_admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, alice_id_pub, alice) = make_test_identity(1);
    let (_bob_priv, _bob_id_pub, bob) = make_test_identity(2);

    let prior_state = materialize(
        &[
            make_signed(1, MembershipEventKind::Join, admin, 100),
            make_signed(2, MembershipEventKind::Join, bob, 200),
            make_signed(
                3,
                MembershipEventKind::SetPower {
                    target: alice,
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
        kind: MembershipEventKind::SetPower {
            target: bob,
            level: 50,
        },
        actor: alice, // non-member!
        at: Hlc {
            wall_ms: 400,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &alice_priv).expect("sign");

    let ctx = VerifyContext {
        expected_community_id: SpaceId([3u8; 16]),
        is_invite_only: false,
        actor_identity_pub: &alice_id_pub,
        countersigner_identity_pub: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::ActorNotJoined);
}

#[test]
fn verify_event_rejects_invite_only_join_with_non_joined_countersigner() {
    // Even if the countersig is cryptographically valid, the
    // countersigner must be a current Joined member — otherwise an
    // attacker who somehow obtained an invite token from a non-member
    // could vouch for arbitrary joiners.
    let (_admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, alice_id_pub, alice) = make_test_identity(1);
    let (outsider_priv, outsider_id_pub, outsider) = make_test_identity(99);

    // Only admin Joined; outsider exists in power_levels (e.g., from a
    // pre-departure SetPower) but has never Joined.
    let prior_state = materialize(
        &[
            make_signed(1, MembershipEventKind::Join, admin, 100),
            make_signed(
                2,
                MembershipEventKind::SetPower {
                    target: outsider,
                    level: 50,
                },
                admin,
                200,
            ),
        ],
        admin,
    );

    let payload = EventPayload {
        id: [3u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor: alice,
        at: Hlc {
            wall_ms: 300,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &alice_priv).expect("sign");
    let event = attach_countersig_with_identity(&event, &outsider_priv).expect("countersign");

    let ctx = VerifyContext {
        expected_community_id: SpaceId([3u8; 16]),
        is_invite_only: true,
        actor_identity_pub: &alice_id_pub,
        countersigner_identity_pub: Some(&outsider_id_pub),
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::CounterSignerNotJoined);
}

#[test]
fn verify_event_rejects_leave_from_banned_actor() {
    // Without this guard, a kicked (Banned) actor can sign a Leave
    // event — Leave has no power requirement and would be accepted.
    // materialize() would then set status=Left, masking the Ban.
    // A subsequent Join would no longer hit the Banned guard, letting
    // the kicked actor rejoin trivially. Reject Leave from Banned.
    let (admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, alice_id_pub, alice) = make_test_identity(1);

    // Set up: alice joined, then admin kicked her.
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
                sign_event_with_identity(&payload, &alice_priv).expect("sign")
            },
            {
                let payload = EventPayload {
                    id: [3u8; 16],
                    community_id: SpaceId([3u8; 16]),
                    kind: MembershipEventKind::Kick {
                        target: alice,
                        reason: None,
                    },
                    actor: admin,
                    at: Hlc {
                        wall_ms: 300,
                        logical: 0,
                        device_id: "d".into(),
                    },
                };
                sign_event_with_identity(&payload, &admin_priv).expect("sign")
            },
        ],
        admin,
    );
    assert_eq!(
        prior_state.members.get(&alice).map(|s| s.status),
        Some(MemberStatus::Banned),
        "test setup: alice must be Banned in prior_state"
    );

    // Alice (still has her key after the kick) signs a Leave event.
    let payload = EventPayload {
        id: [4u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Leave,
        actor: alice,
        at: Hlc {
            wall_ms: 400,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &alice_priv).expect("sign");

    let ctx = VerifyContext {
        expected_community_id: SpaceId([3u8; 16]),
        is_invite_only: false,
        actor_identity_pub: &alice_id_pub,
        countersigner_identity_pub: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::BannedActorLeave);
}

#[test]
fn materialize_preserves_banned_status_against_join_replay() {
    // Defense in depth, symmetric with the Leave handler's guard:
    // even if a Join from a Banned actor slips past verify_event
    // (e.g., loaded from a corrupted on-disk log, or replayed from
    // before the Ban arrived), materialize must NOT transition status
    // back to Joined. Banned is sticky until a dedicated unban flow
    // exists.
    let admin = OwnerAddr([100u8; 16]);
    let admin_key = SigningKey::from_bytes(&[100u8; 32]);
    let alice = OwnerAddr([1u8; 16]);
    let alice_key = SigningKey::from_bytes(&[1u8; 32]);

    let events = vec![
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
                    reason: None,
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
        // After the kick, alice's Join slips through (corrupted log,
        // missing verify, etc). materialize must keep Banned sticky.
        {
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
            sign_event(&payload, &alice_key).expect("sign")
        },
    ];

    let m = materialize(&events, admin);
    assert_eq!(
        m.members.get(&alice).map(|s| s.status),
        Some(MemberStatus::Banned),
        "Join must not override Banned — Banned is sticky"
    );
}

#[test]
fn materialize_preserves_banned_status_against_leave_replay() {
    // Defense in depth: even if a Leave from a Banned actor slips past
    // verify_event (e.g., loaded from a corrupted on-disk log, or replayed
    // from before the Ban arrived), materialize must NOT transition status
    // back to Left. Banned is sticky until an explicit unban flow exists.
    let admin = OwnerAddr([100u8; 16]);
    let admin_key = SigningKey::from_bytes(&[100u8; 32]);
    let alice = OwnerAddr([1u8; 16]);
    let alice_key = SigningKey::from_bytes(&[1u8; 32]);

    // Build the event log: alice joins (200), admin kicks her (300),
    // then alice's Leave event arrives (400). HLC order means Kick
    // applies before Leave. Leave must NOT override Banned.
    let join_alice = {
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
    };
    let kick_alice = {
        let payload = EventPayload {
            id: [3u8; 16],
            community_id: SpaceId([3u8; 16]),
            kind: MembershipEventKind::Kick {
                target: alice,
                reason: None,
            },
            actor: admin,
            at: Hlc {
                wall_ms: 300,
                logical: 0,
                device_id: "d".into(),
            },
        };
        sign_event(&payload, &admin_key).expect("sign")
    };
    let leave_alice = {
        let payload = EventPayload {
            id: [4u8; 16],
            community_id: SpaceId([3u8; 16]),
            kind: MembershipEventKind::Leave,
            actor: alice,
            at: Hlc {
                wall_ms: 400,
                logical: 0,
                device_id: "d".into(),
            },
        };
        sign_event(&payload, &alice_key).expect("sign")
    };

    let m = materialize(
        &[
            make_signed(1, MembershipEventKind::Join, admin, 100),
            join_alice,
            kick_alice,
            leave_alice,
        ],
        admin,
    );
    assert_eq!(
        m.members.get(&alice).map(|s| s.status),
        Some(MemberStatus::Banned),
        "Leave must not override Banned — Banned is sticky"
    );
}

#[test]
fn verify_event_rejects_join_replay_after_kick() {
    // After a kicked actor's status materializes to Banned, a replayed
    // (or fresh) Join from that actor must be rejected — otherwise
    // materialize() would silently overwrite Banned back to Joined,
    // making Kick effectively cosmetic.
    //
    // Kick = effective ban until an explicit unban flow exists
    // (deferred to a follow-up).
    let (admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, alice_id_pub, alice) = make_test_identity(1);

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
                sign_event_with_identity(&payload, &alice_priv).expect("sign")
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
                sign_event_with_identity(&payload, &admin_priv).expect("sign")
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
    let event = sign_event_with_identity(&payload, &alice_priv).expect("sign");

    let ctx = VerifyContext {
        expected_community_id: SpaceId([3u8; 16]),
        is_invite_only: false,
        actor_identity_pub: &alice_id_pub,
        countersigner_identity_pub: None,
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
    let (admin_priv, admin_id_pub, admin) = make_test_identity(100);
    let (_bob_priv, _bob_id_pub, bob) = make_test_identity(2);

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
    let event = sign_event_with_identity(&payload, &admin_priv).expect("sign");

    let ctx = VerifyContext {
        expected_community_id: SpaceId([3u8; 16]),
        is_invite_only: false,
        actor_identity_pub: &admin_id_pub,
        countersigner_identity_pub: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::PowerLevelOutOfRange);
}

#[test]
fn verify_event_accepts_setpower_at_max_boundary() {
    // Boundary check: level == POWER_THRESHOLDS.max (100) is allowed.
    let (admin_priv, admin_id_pub, admin) = make_test_identity(100);
    let (_bob_priv, _bob_id_pub, bob) = make_test_identity(2);

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
    let event = sign_event_with_identity(&payload, &admin_priv).expect("sign");

    let ctx = VerifyContext {
        expected_community_id: SpaceId([3u8; 16]),
        is_invite_only: false,
        actor_identity_pub: &admin_id_pub,
        countersigner_identity_pub: None,
    };

    verify_event(&event, &prior_state, &ctx).expect("must accept level == max");
}

#[test]
fn verify_event_rejects_countersig_on_open_community_join() {
    // Countersig is only meaningful for invite-only Join. On any
    // other event (Invite/Kick/SetPower) and on open-community Join,
    // the field MUST be None — otherwise the wire form is malleable
    // (sig excludes countersig, so a peer can append/strip/replace
    // it without invalidating the actor sig). Reject explicitly.
    let (admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, alice_id_pub, alice) = make_test_identity(1);

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
    let event = sign_event_with_identity(&payload, &alice_priv).expect("sign");
    // Append a countersig that wasn't requested (open community).
    let event = attach_countersig_with_identity(&event, &admin_priv).expect("countersig");

    let ctx = VerifyContext {
        expected_community_id: SpaceId([3u8; 16]),
        is_invite_only: false, // OPEN community
        actor_identity_pub: &alice_id_pub,
        countersigner_identity_pub: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::UnexpectedCounterSig);
}

#[test]
fn verify_event_rejects_countersig_on_invite_event() {
    // Even with the right context, Invite events never carry countersigs.
    let (admin_priv, admin_id_pub, admin) = make_test_identity(100);
    let (_bob_priv, _bob_id_pub, bob) = make_test_identity(2);

    let prior_state = materialize(
        &[make_signed(1, MembershipEventKind::Join, admin, 100)],
        admin,
    );

    let payload = EventPayload {
        id: [2u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Invite { target: bob },
        actor: admin,
        at: Hlc {
            wall_ms: 200,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &admin_priv).expect("sign");
    // Spurious countersig.
    let event = attach_countersig_with_identity(&event, &admin_priv).expect("countersig");

    let ctx = VerifyContext {
        expected_community_id: SpaceId([3u8; 16]),
        is_invite_only: false,
        actor_identity_pub: &admin_id_pub,
        countersigner_identity_pub: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::UnexpectedCounterSig);
}

#[test]
fn verify_event_rejects_event_for_wrong_community() {
    // The caller passes a prior_state and policy (is_invite_only) for
    // community A. If the verified event was signed for community B,
    // the authorization is wrong — the caller would otherwise grant
    // power lookups, invite-only countersigning, etc. against the
    // wrong community's state. Bind the verification to the expected
    // community_id at the top of verify_event.
    let (_admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, alice_id_pub, alice) = make_test_identity(1);

    let community_a = SpaceId([0xAA; 16]);
    let community_b = SpaceId([0xBB; 16]);

    // prior_state is for community A.
    let prior_state = materialize(
        &[make_signed(1, MembershipEventKind::Join, admin, 100)],
        admin,
    );

    // Alice signs a Join event for community B.
    let payload = EventPayload {
        id: [2u8; 16],
        community_id: community_b, // event for B
        kind: MembershipEventKind::Join,
        actor: alice,
        at: Hlc {
            wall_ms: 200,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &alice_priv).expect("sign");

    let ctx = VerifyContext {
        expected_community_id: community_a, // verify against A
        is_invite_only: false,
        actor_identity_pub: &alice_id_pub,
        countersigner_identity_pub: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::WrongCommunity);
}

#[test]
fn verify_event_rejects_when_actor_pubkey_doesnt_bind_to_actor() {
    // The bot-flagged scenario: alice signs a Join, but the caller
    // passes bob's identity_pub. Without binding, the Ed25519 sig
    // would also need to verify under bob's pubkey (it won't, so
    // SignatureInvalid would fire). With binding, address-hash check
    // fires first → ActorPubkeyMismatch.
    //
    // The original test name ("verify_event_rejects_when_actor_signature_invalid")
    // tested the without-binding path; under the binding fix the same
    // setup now surfaces a more specific error, which is the desired
    // behavior.
    let (_admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);
    let (_bob_priv, bob_id_pub, _bob) = make_test_identity(2);

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
    let event = sign_event_with_identity(&payload, &alice_priv).expect("sign");

    let ctx = VerifyContext {
        expected_community_id: SpaceId([3u8; 16]),
        is_invite_only: false,
        actor_identity_pub: &bob_id_pub, // wrong identity for actor=alice
        countersigner_identity_pub: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::ActorPubkeyMismatch);
}
