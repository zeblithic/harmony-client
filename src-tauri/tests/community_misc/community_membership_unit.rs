//! Unit-style integration tests for community_membership.rs.
//! Phase 1 (ZEB-217 Sub-C) — types, materialization, verification.

use harmony_app::community_membership::{ChannelId, ChannelKind, MembershipEventKind};
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
        MembershipEventKind::ChannelCreate {
            channel_id: ChannelId([0xAB; 16]),
            name: "general".to_string(),
            write_power: 0,
            kind: ChannelKind::Text,
        },
        MembershipEventKind::ChannelModify {
            channel_id: ChannelId([0xAB; 16]),
            name: Some("renamed".to_string()),
            write_power: Some(50),
        },
        MembershipEventKind::ChannelModify {
            channel_id: ChannelId([0xAB; 16]),
            name: Some("renamed".to_string()),
            write_power: None,
        },
        MembershipEventKind::ChannelModify {
            channel_id: ChannelId([0xAB; 16]),
            name: None,
            write_power: Some(50),
        },
        MembershipEventKind::ChannelDelete {
            channel_id: ChannelId([0xAB; 16]),
        },
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
        signer_certs: Vec::new(),
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
        enrollment: None,
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
        signer_certs: Vec::new(),
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
        enrollment: None,
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

use ed25519_dalek::{Signer, SigningKey};
use harmony_app::community_membership::{mint_test_owner, sign_event, EventPayload, TestOwner};
use harmony_identity::PrivateIdentity;
use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    /// ZEB-339: registry mapping each minted owner's `OwnerAddr` to its
    /// `TestOwner` (owner_id + enrolled device key + Master cert). Populated by
    /// `make_test_identity` so steady-state helpers (`make_signed`) can resolve
    /// the actor's device key to sign with and the cert to attach on Join.
    static OWNER_REGISTRY: RefCell<HashMap<OwnerAddr, TestOwner>> = RefCell::new(HashMap::new());
}

fn register_owner(owner: &TestOwner) {
    OWNER_REGISTRY.with(|r| {
        r.borrow_mut().insert(owner.owner, owner.clone());
    });
}

/// ZEB-339: build a deterministic enrolled-device owner from a one-byte seed.
/// Returns `(TestOwner, dummy_pub, owner_addr)` so existing `(priv, pub, addr)`
/// destructures keep compiling. The middle element is a placeholder — under the
/// enrolled-device model `VerifyContext`/`verify_event` no longer consume a
/// 64-byte identity_pub. The actor is `owner.owner` (owner_id, NOT the device
/// address hash); events are signed by the enrolled device key (#2).
fn make_test_identity(seed: u8) -> (TestOwner, [u8; 64], OwnerAddr) {
    let owner = mint_test_owner(seed);
    register_owner(&owner);
    let addr = owner.owner;
    (owner, [0u8; 64], addr)
}

/// ZEB-339: legacy PrivateIdentity tuple for the low-level `verify_signature`
/// tests, which still take a 64-byte identity_pub and bind
/// `address_hash(pub) == actor`. Distinct from the enrolled-device model used
/// by the `verify_event` tests.
fn make_legacy_identity(seed: u8) -> (PrivateIdentity, [u8; 64], OwnerAddr) {
    let private = PrivateIdentity::from_seed(&[seed; 32]);
    let identity_pub = private.identity.to_public_bytes();
    let owner_addr = OwnerAddr(private.identity.address_hash);
    (private, identity_pub, owner_addr)
}

/// ZEB-339: sign a membership event with the owner's enrolled device key (#2),
/// attaching the owner's Master enrollment cert on identity-introducing events
/// (Join / PendingJoin) so `materialize` populates `enrolled_device_keys`.
fn sign_event_with_identity(
    payload: &EventPayload,
    owner: &TestOwner,
) -> Result<SignedMembershipEvent, harmony_app::owner_state_crypto::CryptoError> {
    let ev = sign_event(payload, &owner.device_key)?;
    Ok(match ev.kind {
        MembershipEventKind::Join | MembershipEventKind::PendingJoin { .. } => {
            SignedMembershipEvent {
                enrollment: Some(owner.cert.clone()),
                ..ev
            }
        }
        _ => ev,
    })
}

/// ZEB-339: attach a counter-signature produced by the signer's enrolled
/// device key (#2). The verifier resolves the signer's key from materialized
/// membership.
fn attach_countersig_with_identity(
    event: &SignedMembershipEvent,
    signer: &TestOwner,
) -> Result<SignedMembershipEvent, harmony_app::owner_state_crypto::CryptoError> {
    harmony_app::community_membership::attach_countersig_with_device_key(
        event,
        signer.owner,
        &signer.device_key,
    )
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

use harmony_app::community_membership::{verify_countersig, verify_signature, VerifyError};

#[test]
fn verify_signature_accepts_valid_event() {
    let (private, identity_pub, actor) = make_legacy_identity(42);

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

    let event = harmony_app::community_membership::sign_event_with_identity(&payload, &private)
        .expect("sign");
    verify_signature(&event, &identity_pub).expect("must verify");
}

#[test]
fn verify_signature_rejects_tampered_event() {
    let (private, identity_pub, actor) = make_legacy_identity(42);

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

    let mut event = harmony_app::community_membership::sign_event_with_identity(&payload, &private)
        .expect("sign");
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
    let (alice_priv, _alice_id_pub, alice) = make_legacy_identity(1);
    let (_bob_priv, bob_id_pub, _bob) = make_legacy_identity(2);

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
    let event = harmony_app::community_membership::sign_event_with_identity(&payload, &alice_priv)
        .expect("sign");

    let err = verify_signature(&event, &bob_id_pub).expect_err("must reject wrong identity");
    assert_eq!(err, VerifyError::ActorPubkeyMismatch);
}

#[test]
fn attach_and_verify_countersig_round_trip() {
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);
    let (admin_priv, _admin_id_pub, admin) = make_test_identity(100);

    // ZEB-339: verify_countersig resolves the signer's enrolled device key from
    // materialized membership, so the admin (countersigner) must be a Joined
    // member with their cert-bearing Join materialized.
    let admin_join = sign_event_with_identity(
        &EventPayload {
            id: [0xAD; 16],
            community_id: SpaceId([3u8; 16]),
            kind: MembershipEventKind::Join,
            actor: admin,
            at: Hlc {
                wall_ms: 500,
                logical: 0,
                device_id: "d".into(),
            },
        },
        &admin_priv,
    )
    .expect("sign");
    let prior = materialize(std::slice::from_ref(&admin_join), admin);

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

    verify_countersig(&event_with_cs, &prior).expect("countersig must verify");
}

#[test]
fn verify_countersig_rejects_when_payload_changed_after_countersign() {
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);
    let (admin_priv, _admin_id_pub, admin) = make_test_identity(100);

    // ZEB-339: admin must be a materialized member so verify_countersig can
    // resolve their enrolled device key.
    let admin_join = sign_event_with_identity(
        &EventPayload {
            id: [0xAD; 16],
            community_id: SpaceId([3u8; 16]),
            kind: MembershipEventKind::Join,
            actor: admin,
            at: Hlc {
                wall_ms: 500,
                logical: 0,
                device_id: "d".into(),
            },
        },
        &admin_priv,
    )
    .expect("sign");
    let prior = materialize(std::slice::from_ref(&admin_join), admin);

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

    let err = verify_countersig(&event_with_cs, &prior).expect_err("must reject mutated payload");
    // ZEB-339: the attached sig no longer matches the mutated payload bytes, so
    // none of admin's enrolled device keys verify → CounterSignerNotEnrolled.
    assert!(matches!(
        err,
        VerifyError::CounterSigInvalid
            | VerifyError::SignatureInvalid
            | VerifyError::CounterSignerNotEnrolled
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
    let (admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (carol_priv, _carol_id_pub, carol) = make_test_identity(50);

    // ZEB-339: carol is a materialized member with her own enrolled device key.
    // The forged countersig claims signer=carol but is signed by admin's key,
    // which is NOT in carol's enrolled key set → CounterSignerNotEnrolled.
    let admin_join = sign_event_with_identity(
        &EventPayload {
            id: [0xAD; 16],
            community_id: SpaceId([3u8; 16]),
            kind: MembershipEventKind::Join,
            actor: admin,
            at: Hlc {
                wall_ms: 400,
                logical: 0,
                device_id: "d".into(),
            },
        },
        &admin_priv,
    )
    .expect("sign");
    let carol_join = sign_event_with_identity(
        &EventPayload {
            id: [0xCA; 16],
            community_id: SpaceId([3u8; 16]),
            kind: MembershipEventKind::Join,
            actor: carol,
            at: Hlc {
                wall_ms: 500,
                logical: 0,
                device_id: "d".into(),
            },
        },
        &carol_priv,
    )
    .expect("sign");
    let prior = materialize(&[admin_join, carol_join], admin);

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
    // Forge a countersig with cs.signer=carol but signed by admin's device key.
    // (Real attack: admin signs, claims carol, hopes verifier credits carol's
    //  authority — but the enrolled-key binding check fires.)
    let mut event = event;
    let payload_for_cs = EventPayload {
        id: event.id,
        community_id: event.community_id,
        kind: event.kind.clone(),
        actor: event.actor,
        at: event.at.clone(),
    };
    let bytes = canonical_cbor_encode(&payload_for_cs).expect("encode");
    let admin_sig = admin_priv.device_key.sign(&bytes).to_bytes();
    event.countersig = Some(harmony_app::community_membership::CounterSignature {
        signer: carol,  // claim carol
        sig: admin_sig, // but signed by admin's device key
    });

    let err = verify_countersig(&event, &prior).expect_err("must reject");
    // ZEB-339: admin's key isn't in carol's enrolled key set, so no key
    // verifies the forged countersig → CounterSignerNotEnrolled. (The signer
    // binding is now enforced via materialized enrolled keys, not identity_pub.)
    assert!(matches!(
        err,
        VerifyError::CounterSigInvalid
            | VerifyError::CounterSignerPubkeyMismatch
            | VerifyError::CounterSignerNotEnrolled
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

use harmony_app::community_membership::{
    event_sort_key, materialize, prior_state_at_event, prior_state_at_hlc,
};
use harmony_app::community_membership::{verify_event, VerifyContext};

fn make_signed(
    id: u8,
    kind: MembershipEventKind,
    actor: OwnerAddr,
    at_ms: u64,
) -> SignedMembershipEvent {
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
    // ZEB-339: if the actor was minted via make_test_identity, sign with their
    // enrolled device key and attach the cert on Join/PendingJoin so
    // materialize() populates enrolled_device_keys and verify_event can resolve
    // the signer. Otherwise (pure structural/ordering tests using arbitrary
    // OwnerAddr literals) fall back to a fixed key — those events never go
    // through verify_event.
    let registered = OWNER_REGISTRY.with(|r| r.borrow().get(&actor).cloned());
    match registered {
        Some(owner) => sign_event_with_identity(&payload, &owner).expect("sign"),
        None => {
            let signing_key = SigningKey::from_bytes(&[42u8; 32]);
            sign_event(&payload, &signing_key).expect("sign")
        }
    }
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
fn prior_state_at_hlc_excludes_events_at_or_after_hlc() {
    // The HLC-keyed companion to `prior_state_at_event` excludes events
    // sharing the target's (wall_ms, logical, device_id) triple AND
    // anything strictly after. Events strictly before the triple are
    // included. Used by the receive-side membership-at-publish-HLC gate
    // where the caller has only `payload.at` (a bare `Hlc`), not a full
    // `SignedMembershipEvent`.
    let admin = OwnerAddr([100u8; 16]);
    let alice = OwnerAddr([1u8; 16]);

    // Three events: one strictly before, one at the exact target HLC,
    // and one strictly after.
    let before = make_signed(1, MembershipEventKind::Join, admin, 100);
    let at_target = make_signed(2, MembershipEventKind::Join, alice, 200);
    let after = make_signed(3, MembershipEventKind::Leave, alice, 300);

    let target_hlc = Hlc {
        wall_ms: 200,
        logical: 0,
        device_id: "d".into(), // matches `make_signed`'s device_id
    };

    let prior = prior_state_at_hlc(
        &[after.clone(), at_target.clone(), before.clone()],
        &target_hlc,
        admin,
    );

    // `before` (admin Join at 100) is included → admin is Joined.
    assert_eq!(
        prior.members.get(&admin).map(|s| s.status),
        Some(MemberStatus::Joined)
    );
    // `at_target` (alice Join at exact HLC) is excluded — strict prefix.
    // `after` (alice Leave at 300) is excluded.
    // Net: alice has no member entry at all.
    assert_eq!(
        prior.members.get(&alice),
        None,
        "events at the exact target HLC must be excluded"
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
fn materialize_kick_against_unknown_target_does_not_fabricate_phantom() {
    // verify_event already rejects KickTargetNotMember, but the
    // materialize Kick handler must also be defensive — corrupted
    // logs or unverified replays should not fabricate a phantom
    // MemberState with status=Banned and joined_at=kick_time for an
    // address that never joined. Mirrors the Join/Leave defense-in-
    // depth guards above.
    let admin = OwnerAddr([100u8; 16]);
    let admin_key = SigningKey::from_bytes(&[100u8; 32]);
    let stranger = OwnerAddr([0xEEu8; 16]);

    let bad_kick = {
        let payload = EventPayload {
            id: [2u8; 16],
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
        sign_event(&payload, &admin_key).expect("sign")
    };

    let m = materialize(
        &[
            make_signed(1, MembershipEventKind::Join, admin, 100),
            bad_kick,
        ],
        admin,
    );
    assert!(
        !m.members.contains_key(&stranger),
        "stranger must NOT appear in members — Kick must not fabricate phantom records"
    );
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
fn materialize_redundant_join_does_not_overwrite_joined_at() {
    // A member who is already Joined re-sending a Join event must NOT
    // reset their joined_at to the new event timestamp. Without this
    // guard, any actor could push their own join date forward by
    // re-signing a Join — there's no privilege gate that prevents it,
    // so anyone could falsify "I've been here since the start".
    let admin = OwnerAddr([100u8; 16]);
    let alice = OwnerAddr([1u8; 16]);
    let alice_key = SigningKey::from_bytes(&[1u8; 32]);

    let alice_join_t200 = {
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
    let alice_redundant_join_t900 = {
        let payload = EventPayload {
            id: [3u8; 16],
            community_id: SpaceId([3u8; 16]),
            kind: MembershipEventKind::Join,
            actor: alice,
            at: Hlc {
                wall_ms: 900,
                logical: 0,
                device_id: "d".into(),
            },
        };
        sign_event(&payload, &alice_key).expect("sign")
    };

    let m = materialize(
        &[
            make_signed(1, MembershipEventKind::Join, admin, 100),
            alice_join_t200,
            alice_redundant_join_t900,
        ],
        admin,
    );
    let s = m.members.get(&alice).expect("alice present");
    assert_eq!(s.status, MemberStatus::Joined);
    assert_eq!(
        s.joined_at.wall_ms, 200,
        "joined_at must pin to first Join — redundant re-Join is a no-op"
    );
}

#[test]
fn materialize_redundant_invite_does_not_overwrite_joined_at() {
    // Symmetric to redundant Join: re-inviting an already-Invited
    // target must not reset their joined_at. Otherwise an admin could
    // backdate (or push forward) a pending invitation timestamp.
    let admin = OwnerAddr([100u8; 16]);
    let admin_key = SigningKey::from_bytes(&[100u8; 32]);
    let bob = OwnerAddr([2u8; 16]);

    let invite_t200 = {
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
        sign_event(&payload, &admin_key).expect("sign")
    };
    let redundant_invite_t900 = {
        let payload = EventPayload {
            id: [3u8; 16],
            community_id: SpaceId([3u8; 16]),
            kind: MembershipEventKind::Invite { target: bob },
            actor: admin,
            at: Hlc {
                wall_ms: 900,
                logical: 0,
                device_id: "d".into(),
            },
        };
        sign_event(&payload, &admin_key).expect("sign")
    };

    let m = materialize(
        &[
            make_signed(1, MembershipEventKind::Join, admin, 100),
            invite_t200,
            redundant_invite_t900,
        ],
        admin,
    );
    let s = m.members.get(&bob).expect("bob present");
    assert_eq!(s.status, MemberStatus::Invited);
    assert_eq!(
        s.joined_at.wall_ms, 200,
        "joined_at must pin to first Invite — redundant re-Invite is a no-op"
    );
}

#[test]
fn materialize_invite_refreshes_left_member_to_invited() {
    // Re-inviting a former member (status = Left) should transition
    // them to Invited so the UI can show "alice has been re-invited".
    // The previous behavior used entry().or_insert() which silently
    // dropped re-invites for any address already in the members map.
    let admin = OwnerAddr([100u8; 16]);
    let alice = OwnerAddr([1u8; 16]);
    let admin_key = SigningKey::from_bytes(&[100u8; 32]);
    let alice_key = SigningKey::from_bytes(&[1u8; 32]);

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
        sign_event(&payload, &alice_key).expect("sign")
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
        sign_event(&payload, &alice_key).expect("sign")
    };
    let alice_reinvite = {
        let payload = EventPayload {
            id: [4u8; 16],
            community_id: SpaceId([3u8; 16]),
            kind: MembershipEventKind::Invite { target: alice },
            actor: admin,
            at: Hlc {
                wall_ms: 400,
                logical: 0,
                device_id: "d".into(),
            },
        };
        sign_event(&payload, &admin_key).expect("sign")
    };

    let m = materialize(
        &[
            make_signed(1, MembershipEventKind::Join, admin, 100),
            alice_join,
            alice_leave,
            alice_reinvite,
        ],
        admin,
    );
    let s = m.members.get(&alice).expect("alice present");
    assert_eq!(
        s.status,
        MemberStatus::Invited,
        "Left → re-invited must surface as Invited"
    );
    assert_eq!(
        s.joined_at.wall_ms, 400,
        "joined_at should reflect the new invite (entry effectively starts over)"
    );
    assert!(s.left_at.is_none(), "left_at cleared on re-invite");
}

#[test]
fn materialize_invite_no_op_on_joined_member() {
    // Inviting an already-Joined member is a no-op (they're past the
    // Invited stage). Their joined_at must NOT be overwritten.
    let admin = OwnerAddr([100u8; 16]);
    let alice = OwnerAddr([1u8; 16]);
    let admin_key = SigningKey::from_bytes(&[100u8; 32]);
    let alice_key = SigningKey::from_bytes(&[1u8; 32]);

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
        sign_event(&payload, &alice_key).expect("sign")
    };
    let redundant_invite = {
        let payload = EventPayload {
            id: [3u8; 16],
            community_id: SpaceId([3u8; 16]),
            kind: MembershipEventKind::Invite { target: alice },
            actor: admin,
            at: Hlc {
                wall_ms: 300,
                logical: 0,
                device_id: "d".into(),
            },
        };
        sign_event(&payload, &admin_key).expect("sign")
    };

    let m = materialize(
        &[
            make_signed(1, MembershipEventKind::Join, admin, 100),
            alice_join,
            redundant_invite,
        ],
        admin,
    );
    let s = m.members.get(&alice).expect("alice present");
    assert_eq!(s.status, MemberStatus::Joined);
    assert_eq!(s.joined_at.wall_ms, 200, "Joined wins; joined_at preserved");
}

#[test]
fn materialize_invite_no_op_on_banned_member() {
    // Banned must remain sticky against an Invite event — otherwise
    // a future admin (or a malicious one) could "re-invite" a banned
    // member, defeating the ban.
    let admin = OwnerAddr([100u8; 16]);
    let alice = OwnerAddr([1u8; 16]);
    let admin_key = SigningKey::from_bytes(&[100u8; 32]);
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
        {
            let payload = EventPayload {
                id: [4u8; 16],
                community_id: SpaceId([3u8; 16]),
                kind: MembershipEventKind::Invite { target: alice },
                actor: admin,
                at: Hlc {
                    wall_ms: 400,
                    logical: 0,
                    device_id: "d".into(),
                },
            };
            sign_event(&payload, &admin_key).expect("sign")
        },
    ];

    let m = materialize(&events, admin);
    assert_eq!(
        m.members.get(&alice).map(|s| s.status),
        Some(MemberStatus::Banned),
        "Banned must remain sticky against re-Invite"
    );
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
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);

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
        now_ms: None,
        expected_community_id: SpaceId([3u8; 16]),
        admin_addr: admin,
        is_invite_only: false,
    };

    verify_event(&event, &prior_state, &ctx).expect("must accept");
}

#[test]
fn verify_event_rejects_invite_only_join_without_countersig() {
    let (_admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);

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
        now_ms: None,
        expected_community_id: SpaceId([3u8; 16]),
        admin_addr: admin,
        is_invite_only: true,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::CounterSigRequired);
}

#[test]
fn verify_event_accepts_admin_self_join_in_invite_only_community_without_countersig() {
    // Bootstrap exemption: an invite-only community would otherwise be
    // unbootstrappable — every Join needs a countersig from a Joined
    // member, but no one is Joined initially. Admin is implicitly
    // trusted by virtue of being admin_addr; their self-Join is the
    // bootstrap event that puts the first Joined member into the CRDT.
    let (admin_priv, _admin_id_pub, admin) = make_test_identity(100);

    let prior_state = materialize(&[], admin); // empty community

    let payload = EventPayload {
        id: [1u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor: admin,
        at: Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &admin_priv).expect("sign");

    let ctx = VerifyContext {
        now_ms: None,
        expected_community_id: SpaceId([3u8; 16]),
        admin_addr: admin,
        is_invite_only: true,
    };

    verify_event(&event, &prior_state, &ctx).expect("admin self-Join must bootstrap");
}

#[test]
fn verify_event_rejects_admin_invite_only_join_with_spurious_countersig() {
    // Admin self-Join in invite-only is exempt from the countersig
    // requirement — but if a countersig IS attached, that's malformed
    // input. Reject as UnexpectedCounterSig (consistent with how
    // open-community Join with countersig is rejected). Defends against
    // wire-malleability via the countersig field.
    let (admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (someone_priv, _someone_id_pub, _someone) = make_test_identity(2);

    let prior_state = materialize(&[], admin);

    let payload = EventPayload {
        id: [1u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor: admin,
        at: Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &admin_priv).expect("sign");
    // Spurious countersig from someone else.
    let event = attach_countersig_with_identity(&event, &someone_priv).expect("countersig");

    let ctx = VerifyContext {
        now_ms: None,
        expected_community_id: SpaceId([3u8; 16]),
        admin_addr: admin,
        is_invite_only: true,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::UnexpectedCounterSig);
}

#[test]
fn verify_event_accepts_invite_only_join_with_valid_countersig() {
    let (admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);

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
        now_ms: None,
        expected_community_id: SpaceId([3u8; 16]),
        admin_addr: admin,
        is_invite_only: true,
    };

    verify_event(&event, &prior_state, &ctx).expect("must accept");
}

#[test]
fn verify_event_rejects_kick_when_actor_power_below_threshold() {
    let (_admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);
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
        now_ms: None,
        expected_community_id: SpaceId([3u8; 16]),
        admin_addr: admin,
        is_invite_only: false,
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
    let (admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let stranger = OwnerAddr([0xEEu8; 16]); // never appeared in any event

    let prior_state = materialize(
        &[make_signed(1, MembershipEventKind::Join, admin, 100)],
        admin,
    );
    assert!(
        !prior_state.members.contains_key(&stranger),
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
        now_ms: None,
        expected_community_id: SpaceId([3u8; 16]),
        admin_addr: admin,
        is_invite_only: false,
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
    let (admin_priv, _admin_id_pub, admin) = make_test_identity(100);
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
        now_ms: None,
        expected_community_id: SpaceId([3u8; 16]),
        admin_addr: admin,
        is_invite_only: false,
    };

    verify_event(&event, &prior_state, &ctx).expect("must accept Left → Banned");
}

#[test]
fn verify_event_rejects_kick_when_target_power_equals_actor() {
    let (admin_priv, _admin_id_pub, admin) = make_test_identity(100);
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
        now_ms: None,
        expected_community_id: SpaceId([3u8; 16]),
        admin_addr: admin,
        is_invite_only: false,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::KickTargetPowerNotLower);
}

#[test]
fn verify_event_rejects_setpower_when_actor_power_insufficient() {
    let (_admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);
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
        now_ms: None,
        expected_community_id: SpaceId([3u8; 16]),
        admin_addr: admin,
        is_invite_only: false,
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
fn verify_event_rejects_invite_targeting_banned_member() {
    // materialize() correctly no-ops Invite when the target is Banned
    // (Banned-sticky), but verify_event was returning Ok(()) for this
    // case — leaving the Phase-4 IPC caller to incorrectly assume the
    // invite took effect. Reject at verify time so the caller surfaces
    // a clear error and the UI can prompt admin to unban first.
    let (admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);

    // alice joined, then admin kicked her.
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
        "test setup: alice must be Banned"
    );

    // Admin tries to invite the banned alice.
    let payload = EventPayload {
        id: [4u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Invite { target: alice },
        actor: admin,
        at: Hlc {
            wall_ms: 400,
            logical: 0,
            device_id: "d".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &admin_priv).expect("sign");

    let ctx = VerifyContext {
        now_ms: None,
        expected_community_id: SpaceId([3u8; 16]),
        admin_addr: admin,
        is_invite_only: false,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::InviteTargetBanned);
}

#[test]
fn verify_event_rejects_invite_from_non_joined_actor() {
    // Under v1 POWER_THRESHOLDS.invite = 0, the power check alone
    // accepts anyone — so a non-member can otherwise emit a valid
    // Invite. Membership must be the operative gate.
    let (_admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);
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
        now_ms: None,
        expected_community_id: SpaceId([3u8; 16]),
        admin_addr: admin,
        is_invite_only: false,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    // ZEB-339: a non-member's event is rejected at signer resolution (their
    // enrolled key is not materialized) before the ActorNotJoined gate. The
    // security property (non-member's Invite rejected) is preserved.
    assert_eq!(err, VerifyError::SignerNotEnrolledForActor);
}

#[test]
fn verify_event_rejects_kick_from_non_joined_actor() {
    let (_admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);
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
        now_ms: None,
        expected_community_id: SpaceId([3u8; 16]),
        admin_addr: admin,
        is_invite_only: false,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    // ZEB-339: alice never Joined, so her enrolled key is not materialized →
    // rejected at signer resolution before the ActorNotJoined / power gate.
    assert_eq!(err, VerifyError::SignerNotEnrolledForActor);
}

#[test]
fn verify_event_rejects_setpower_from_non_joined_actor() {
    let (_admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);
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
        now_ms: None,
        expected_community_id: SpaceId([3u8; 16]),
        admin_addr: admin,
        is_invite_only: false,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    // ZEB-339: alice never Joined, so her enrolled key is not materialized →
    // rejected at signer resolution before the ActorNotJoined / power gate.
    assert_eq!(err, VerifyError::SignerNotEnrolledForActor);
}

#[test]
fn verify_event_rejects_invite_only_join_with_non_joined_countersigner() {
    // Even if the countersig is cryptographically valid, the
    // countersigner must be a current Joined member — otherwise an
    // attacker who somehow obtained an invite token from a non-member
    // could vouch for arbitrary joiners.
    let (_admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);
    let (outsider_priv, _outsider_id_pub, outsider) = make_test_identity(99);

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
        now_ms: None,
        expected_community_id: SpaceId([3u8; 16]),
        admin_addr: admin,
        is_invite_only: true,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    // ZEB-339: the outsider never Joined with a cert, so they have no
    // materialized enrolled device key — verify_countersig rejects at signer
    // resolution (CounterSignerNotEnrolled) before the CounterSignerNotJoined
    // status gate. The security property (non-member cannot vouch) is preserved.
    assert_eq!(err, VerifyError::CounterSignerNotEnrolled);
}

#[test]
fn verify_event_rejects_leave_from_banned_actor() {
    // Without this guard, a kicked (Banned) actor can sign a Leave
    // event — Leave has no power requirement and would be accepted.
    // materialize() would then set status=Left, masking the Ban.
    // A subsequent Join would no longer hit the Banned guard, letting
    // the kicked actor rejoin trivially. Reject Leave from Banned.
    let (admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);

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
        now_ms: None,
        expected_community_id: SpaceId([3u8; 16]),
        admin_addr: admin,
        is_invite_only: false,
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
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);

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
        now_ms: None,
        expected_community_id: SpaceId([3u8; 16]),
        admin_addr: admin,
        is_invite_only: false,
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
    let (admin_priv, _admin_id_pub, admin) = make_test_identity(100);
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
        now_ms: None,
        expected_community_id: SpaceId([3u8; 16]),
        admin_addr: admin,
        is_invite_only: false,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::PowerLevelOutOfRange);
}

#[test]
fn verify_event_accepts_setpower_at_max_boundary() {
    // Boundary check: level == POWER_THRESHOLDS.max (100) is allowed.
    let (admin_priv, _admin_id_pub, admin) = make_test_identity(100);
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
        now_ms: None,
        expected_community_id: SpaceId([3u8; 16]),
        admin_addr: admin,
        is_invite_only: false,
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
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);

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
        now_ms: None,
        expected_community_id: SpaceId([3u8; 16]),
        admin_addr: admin,
        is_invite_only: false, // OPEN community
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::UnexpectedCounterSig);
}

#[test]
fn verify_event_rejects_countersig_on_invite_event() {
    // Even with the right context, Invite events never carry countersigs.
    let (admin_priv, _admin_id_pub, admin) = make_test_identity(100);
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
        now_ms: None,
        expected_community_id: SpaceId([3u8; 16]),
        admin_addr: admin,
        is_invite_only: false,
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
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);

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
        now_ms: None,
        expected_community_id: community_a, // verify against A
        admin_addr: admin,
        is_invite_only: false,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::WrongCommunity);
}

#[test]
fn verify_event_rejects_when_actor_pubkey_doesnt_bind_to_actor() {
    // ZEB-339: the enrolled-device analogue of the old actor-pubkey-binding
    // check. A Join claims actor=alice but attaches BOB's enrollment cert (the
    // cert binds bob's owner_id, not alice's). enrolled_key_from_cert detects
    // owner_id != actor → EnrollmentOwnerMismatch, so an attacker cannot pair a
    // foreign cert with a claimed actor to smuggle in a non-enrolled key.
    let (_admin_priv, _admin_id_pub, admin) = make_test_identity(100);
    let (alice_priv, _alice_id_pub, alice) = make_test_identity(1);
    let (bob_priv, _bob_id_pub, _bob) = make_test_identity(2);

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
    // Sign with alice's device key but swap in BOB's cert (owner_id = bob).
    let event = sign_event(&payload, &alice_priv.device_key).expect("sign");
    let event = SignedMembershipEvent {
        enrollment: Some(bob_priv.cert.clone()),
        ..event
    };

    let ctx = VerifyContext {
        now_ms: None,
        expected_community_id: SpaceId([3u8; 16]),
        admin_addr: admin,
        is_invite_only: false,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::EnrollmentOwnerMismatch);
}

// ── ZEB-262 Phase 4 Task 3: kick + set_power edge regression-pins ─────
//
// These three tests pin existing Phase 1 invariants (KickTargetPowerNotLower,
// PowerLevelOutOfRange, admin-self-demote happy path). They exist as a
// regression harness so the kick_from_community / set_power_level IPCs
// land on a guaranteed-stable verify_event. Adapted from the plan to use
// the existing `make_test_identity` + `sign_event_with_identity` helpers
// (the canonical PrivateIdentity-driven path the rest of this file uses).

#[test]
fn kick_self_rejected_with_kick_target_power_not_lower() {
    // Admin kicks self — admin power 100, target (self) power 100, so
    // target.power not strictly less than actor.power → reject.
    let (admin_priv, _admin_id_pub, admin) = make_test_identity(0xa1);

    let prior_state = materialize(
        &[make_signed(1, MembershipEventKind::Join, admin, 1000)],
        admin,
    );

    let payload = EventPayload {
        id: [2u8; 16],
        community_id: SpaceId([0x77; 16]),
        kind: MembershipEventKind::Kick {
            target: admin,
            reason: None,
        },
        actor: admin,
        at: Hlc {
            wall_ms: 2000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &admin_priv).expect("sign");

    let ctx = VerifyContext {
        now_ms: None,
        expected_community_id: SpaceId([0x77; 16]),
        admin_addr: admin,
        is_invite_only: false,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::KickTargetPowerNotLower);
}

#[test]
fn set_power_out_of_range_rejected() {
    // SetPower with level=200 — exceeds POWER_THRESHOLDS.max (100). Even
    // an admin can't bypass this; it's a wire-format range check.
    let (admin_priv, _admin_id_pub, admin) = make_test_identity(0xa2);
    let target = OwnerAddr([0xbb; 16]);

    let prior_state = materialize(
        &[make_signed(1, MembershipEventKind::Join, admin, 1000)],
        admin,
    );

    let payload = EventPayload {
        id: [3u8; 16],
        community_id: SpaceId([0x88; 16]),
        kind: MembershipEventKind::SetPower { target, level: 200 },
        actor: admin,
        at: Hlc {
            wall_ms: 2000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &admin_priv).expect("sign");

    let ctx = VerifyContext {
        now_ms: None,
        expected_community_id: SpaceId([0x88; 16]),
        admin_addr: admin,
        is_invite_only: false,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::PowerLevelOutOfRange);
}

#[test]
fn set_power_admin_self_demote_inserts() {
    // Admin demotes self to power 50. Foot-gun, but verify_event MUST
    // accept (admin power 100 ≥ set_power_threshold 100; level 50 in
    // range; no power-level transition rule rejects it). The user-
    // visible warning lives in the future Phase 5 UI, not in
    // verify_event.
    let (admin_priv, _admin_id_pub, admin) = make_test_identity(0xa3);

    let prior_state = materialize(
        &[make_signed(1, MembershipEventKind::Join, admin, 1000)],
        admin,
    );

    let payload = EventPayload {
        id: [4u8; 16],
        community_id: SpaceId([0x99; 16]),
        kind: MembershipEventKind::SetPower {
            target: admin,
            level: 50,
        },
        actor: admin,
        at: Hlc {
            wall_ms: 2000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &admin_priv).expect("sign");

    let ctx = VerifyContext {
        now_ms: None,
        expected_community_id: SpaceId([0x99; 16]),
        admin_addr: admin,
        is_invite_only: false,
    };
    verify_event(&event, &prior_state, &ctx)
        .expect("admin self-demote must verify (foot-gun is allowed)");
}

#[test]
fn materialize_channel_create_adds_to_map() {
    // Build admin's bootstrap Join + a ChannelCreate by admin.
    let admin = OwnerAddr([0x10; 16]);
    let admin_join = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x01; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::Join,
        actor: admin,
        at: Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "admin-dev".into(),
        },
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };
    let ch_id = ChannelId([0xAB; 16]);
    let ch_create = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x02; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::ChannelCreate {
            channel_id: ch_id,
            name: "general".to_string(),
            write_power: 0,
            kind: ChannelKind::Text,
        },
        actor: admin,
        at: Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: "admin-dev".into(),
        },
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };

    let m: MaterializedMembership = materialize(&[admin_join, ch_create.clone()], admin);

    let info = m.channels.get(&ch_id).expect("channel materialized");
    assert_eq!(info.name, "general");
    assert_eq!(info.write_power, 0);
    assert_eq!(info.created_at.wall_ms, 2_000);
    assert!(info.deleted_at.is_none());
}

#[test]
fn materialize_channel_create_duplicate_is_first_wins_idempotent() {
    // Same channel_id appears twice with different name/write_power; the
    // second event must NOT mutate the existing entry — first-create-wins.
    // Locks the invariant that the materialize branch's doc comment
    // emphasises (preventing duplicate-emit from refreshing created_at).
    let admin = OwnerAddr([0x10; 16]);
    let admin_join = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x01; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::Join,
        actor: admin,
        at: Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "a".into(),
        },
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };
    let ch_id = ChannelId([0xAB; 16]);
    let ch_first = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x02; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::ChannelCreate {
            channel_id: ch_id,
            name: "first".to_string(),
            write_power: 0,
            kind: ChannelKind::Text,
        },
        actor: admin,
        at: Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: "a".into(),
        },
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };
    let ch_second = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x03; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::ChannelCreate {
            channel_id: ch_id, // same channel_id
            name: "second".to_string(),
            write_power: 50,
            kind: ChannelKind::Text,
        },
        actor: admin,
        at: Hlc {
            wall_ms: 3_000,
            logical: 0,
            device_id: "a".into(),
        },
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };

    let m = materialize(&[admin_join, ch_first, ch_second], admin);
    let info = m.channels.get(&ch_id).expect("channel materialized");
    assert_eq!(info.name, "first", "first ChannelCreate must win");
    assert_eq!(info.write_power, 0, "first ChannelCreate's wp must win");
    assert_eq!(
        info.created_at.wall_ms, 2_000,
        "first ChannelCreate's HLC must win"
    );
}

#[test]
fn materialize_channel_modify_partial_update_preserves_unmodified_field() {
    let admin = OwnerAddr([0x10; 16]);
    let admin_join = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x01; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::Join,
        actor: admin,
        at: Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "a".into(),
        },
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };
    let ch_id = ChannelId([0xAB; 16]);
    let ch_create = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x02; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::ChannelCreate {
            channel_id: ch_id,
            name: "general".to_string(),
            write_power: 0,
            kind: ChannelKind::Text,
        },
        actor: admin,
        at: Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: "a".into(),
        },
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };
    // Only modify name — write_power should stay at 0.
    let ch_modify = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x03; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::ChannelModify {
            channel_id: ch_id,
            name: Some("renamed".to_string()),
            write_power: None,
        },
        actor: admin,
        at: Hlc {
            wall_ms: 3_000,
            logical: 0,
            device_id: "a".into(),
        },
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };

    let m = materialize(&[admin_join, ch_create, ch_modify], admin);
    let info = m.channels.get(&ch_id).expect("channel still present");
    assert_eq!(info.name, "renamed");
    assert_eq!(info.write_power, 0); // preserved
    assert_eq!(info.created_at.wall_ms, 2_000); // preserved
    assert!(info.deleted_at.is_none());
}

#[test]
fn materialize_channel_create_records_kind() {
    // A Voice ChannelCreate materializes ChannelInfo.kind == Voice; a Text
    // (default) ChannelCreate materializes kind == Text. (ZEB-349.)
    let admin = OwnerAddr([0x10; 16]);
    let admin_join = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x01; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::Join,
        actor: admin,
        at: Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "a".into(),
        },
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };
    let ch_voice = ChannelId([0x42; 16]);
    let voice_create = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x02; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::ChannelCreate {
            channel_id: ch_voice,
            name: "hangout".to_string(),
            write_power: 0,
            kind: ChannelKind::Voice,
        },
        actor: admin,
        at: Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: "a".into(),
        },
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };
    let ch_text = ChannelId([0x43; 16]);
    let text_create = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x03; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::ChannelCreate {
            channel_id: ch_text,
            name: "general".to_string(),
            write_power: 0,
            kind: ChannelKind::Text,
        },
        actor: admin,
        at: Hlc {
            wall_ms: 3_000,
            logical: 0,
            device_id: "a".into(),
        },
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };

    let m = materialize(&[admin_join, voice_create, text_create], admin);
    assert_eq!(
        m.channels.get(&ch_voice).expect("voice channel").kind,
        ChannelKind::Voice
    );
    assert_eq!(
        m.channels.get(&ch_text).expect("text channel").kind,
        ChannelKind::Text
    );
}

#[test]
fn channel_modify_cannot_change_kind() {
    // Invariant: kind is immutable. ChannelModify has no kind field, so a
    // modify on a Voice channel leaves kind == Voice. (ZEB-349.)
    let admin = OwnerAddr([0x10; 16]);
    let admin_join = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x01; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::Join,
        actor: admin,
        at: Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "a".into(),
        },
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };
    let ch_id = ChannelId([0x42; 16]);
    let voice_create = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x02; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::ChannelCreate {
            channel_id: ch_id,
            name: "hangout".to_string(),
            write_power: 0,
            kind: ChannelKind::Voice,
        },
        actor: admin,
        at: Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: "a".into(),
        },
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };
    let modify = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x03; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::ChannelModify {
            channel_id: ch_id,
            name: Some("renamed".to_string()),
            write_power: Some(50),
        },
        actor: admin,
        at: Hlc {
            wall_ms: 3_000,
            logical: 0,
            device_id: "a".into(),
        },
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };

    let m = materialize(&[admin_join, voice_create, modify], admin);
    let info = m.channels.get(&ch_id).expect("channel still present");
    assert_eq!(info.name, "renamed");
    assert_eq!(info.write_power, 50);
    assert_eq!(info.kind, ChannelKind::Voice, "kind must be immutable");
}

#[test]
fn materialize_channel_delete_tombstones_in_place() {
    let admin = OwnerAddr([0x10; 16]);
    let admin_join = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x01; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::Join,
        actor: admin,
        at: Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "a".into(),
        },
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };
    let ch_id = ChannelId([0xAB; 16]);
    let ch_create = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x02; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::ChannelCreate {
            channel_id: ch_id,
            name: "general".to_string(),
            write_power: 0,
            kind: ChannelKind::Text,
        },
        actor: admin,
        at: Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: "a".into(),
        },
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };
    let ch_delete = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x03; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::ChannelDelete { channel_id: ch_id },
        actor: admin,
        at: Hlc {
            wall_ms: 4_000,
            logical: 0,
            device_id: "a".into(),
        },
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };

    let m = materialize(&[admin_join, ch_create, ch_delete], admin);
    let info = m
        .channels
        .get(&ch_id)
        .expect("channel still in map (tombstone, not removed)");
    assert_eq!(info.name, "general");
    assert_eq!(info.deleted_at.as_ref().map(|h| h.wall_ms), Some(4_000));
}

#[test]
fn materialize_channel_modify_on_unknown_channel_is_noop() {
    // ChannelModify referencing a channel that doesn't exist is silently
    // ignored — defense-in-depth against an event arriving before its
    // ChannelCreate. `verify_event` intentionally allows this case for
    // replica convergence (cross-blob ordering can deliver Modify before
    // Create), so materialize must remain a safe no-op until the missing
    // create shows up in a later replay.
    let admin = OwnerAddr([0x10; 16]);
    let admin_join = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x01; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::Join,
        actor: admin,
        at: Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "a".into(),
        },
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };
    let ch_modify = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x02; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::ChannelModify {
            channel_id: ChannelId([0xCC; 16]), // never created
            name: Some("ghost".into()),
            write_power: None,
        },
        actor: admin,
        at: Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: "a".into(),
        },
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };

    let m = materialize(&[admin_join, ch_modify], admin);
    assert!(
        m.channels.is_empty(),
        "modify on unknown channel must not synthesize a ghost entry"
    );
}

#[test]
fn verify_event_channel_create_succeeds_for_admin_at_bootstrap_power() {
    // Admin's bootstrap power is 100 (set in materialize). prior_state
    // built from just the admin's Join.
    let (admin_priv, _admin_pub, admin_addr) = make_test_identity(0xAA);
    let community_id = SpaceId([0x37; 16]);

    // ZEB-339: build admin's Join via the enrolled-device helper so its cert is
    // attached and materialize() populates admin's enrolled_device_keys, letting
    // verify_event resolve the ChannelCreate signer.
    let admin_join_payload = EventPayload {
        id: [0x01; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let admin_join = sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign");
    let prior_state = materialize(&[admin_join], admin_addr);

    let payload = EventPayload {
        id: [0x02; 16],
        community_id,
        kind: MembershipEventKind::ChannelCreate {
            channel_id: ChannelId([0xAB; 16]),
            name: "general".to_string(),
            write_power: 0,
            kind: ChannelKind::Text,
        },
        actor: admin_addr,
        at: Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &admin_priv).expect("sign");

    let ctx = VerifyContext {
        now_ms: None,
        expected_community_id: community_id,
        admin_addr,
        is_invite_only: false,
    };

    assert_eq!(verify_event(&event, &prior_state, &ctx), Ok(()));
}

#[test]
fn verify_event_channel_create_rejects_below_mod_power() {
    let (admin_priv, _admin_pub, admin_addr) = make_test_identity(0xAA);
    let (sub_priv, _sub_pub, sub_addr) = make_test_identity(0xBB);
    let community_id = SpaceId([0x37; 16]);

    // Admin Joins via signed payload to populate prior_state correctly.
    let admin_join_payload = EventPayload {
        id: [0x01; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let admin_join = sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign");

    // Sub-actor Joins (default power = 0).
    let sub_join_payload = EventPayload {
        id: [0x02; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: sub_addr,
        at: Hlc {
            wall_ms: 1_500,
            logical: 0,
            device_id: "b".into(),
        },
    };
    let sub_join = sign_event_with_identity(&sub_join_payload, &sub_priv).expect("sign");

    let prior_state = materialize(&[admin_join, sub_join], admin_addr);

    // Sub tries to ChannelCreate with power 0 (well below kick=50).
    let payload = EventPayload {
        id: [0x03; 16],
        community_id,
        kind: MembershipEventKind::ChannelCreate {
            channel_id: ChannelId([0xAB; 16]),
            name: "spam-channel".to_string(),
            write_power: 0,
            kind: ChannelKind::Text,
        },
        actor: sub_addr,
        at: Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: "b".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &sub_priv).expect("sign");

    let ctx = VerifyContext {
        now_ms: None,
        expected_community_id: community_id,
        admin_addr,
        is_invite_only: false,
    };

    assert_eq!(
        verify_event(&event, &prior_state, &ctx),
        Err(VerifyError::ChannelAdminInsufficientPower)
    );
}

#[test]
fn verify_event_channel_create_accepts_at_kick_threshold() {
    // A mod (power exactly 50, the kick threshold) is allowed to modify channels.
    let (admin_priv, _admin_pub, admin_addr) = make_test_identity(0xAA);
    let (mod_priv, _mod_pub, mod_addr) = make_test_identity(0xBB);
    let community_id = SpaceId([0x37; 16]);

    let admin_join_payload = EventPayload {
        id: [0x01; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let admin_join = sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign");

    let mod_join_payload = EventPayload {
        id: [0x02; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: mod_addr,
        at: Hlc {
            wall_ms: 1_500,
            logical: 0,
            device_id: "b".into(),
        },
    };
    let mod_join = sign_event_with_identity(&mod_join_payload, &mod_priv).expect("sign");

    // Admin SetPower to bring mod_addr to power 50 (kick threshold).
    let setpower_payload = EventPayload {
        id: [0x03; 16],
        community_id,
        kind: MembershipEventKind::SetPower {
            target: mod_addr,
            level: 50,
        },
        actor: admin_addr,
        at: Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let setpower = sign_event_with_identity(&setpower_payload, &admin_priv).expect("sign");

    let prior_state = materialize(&[admin_join, mod_join, setpower], admin_addr);

    let payload = EventPayload {
        id: [0x04; 16],
        community_id,
        kind: MembershipEventKind::ChannelCreate {
            channel_id: ChannelId([0xAB; 16]),
            name: "mods-channel".to_string(),
            write_power: 0,
            kind: ChannelKind::Text,
        },
        actor: mod_addr,
        at: Hlc {
            wall_ms: 3_000,
            logical: 0,
            device_id: "b".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &mod_priv).expect("sign");

    let ctx = VerifyContext {
        now_ms: None,
        expected_community_id: community_id,
        admin_addr,
        is_invite_only: false,
    };

    assert_eq!(verify_event(&event, &prior_state, &ctx), Ok(()));
}

// ── ZEB-248 Phase 1 review fixup: per-variant verify_event validation ───
//
// Tests for the VerifyError branches covering channel-config events:
// ChannelModifyNoOp (all-None), ChannelNameInvalid, plus
// PowerLevelOutOfRange reuse for channel write_power.
//
// Round 3 fixup (CodeRabbit, CRDT-safety): verify_event MUST NOT reject
// ChannelCreate-with-duplicate-id or ChannelDelete-on-unknown-channel —
// cross-blob ordering can deliver these before their predecessors, so
// rejecting at verify time would permanently drop the event from a
// replica's log and break CRDT-log convergence. materialize handles
// both cases idempotently (or_insert_with for Create, get_mut→None
// no-op for Delete). Tests below assert the new ALLOW-paths.
//
// Round 4 fixup (Cursor Bugbot, same CRDT-safety argument extended):
// verify_event MUST NOT reject ChannelDelete-on-already-tombstoned or
// value-matching ChannelModify either — same cross-blob divergence
// concern (replicas with first event reject the second; replicas in
// reverse order accept both). Only the all-None ChannelModifyNoOp
// rejection (content-intrinsic, no prior_state dependency) remains.

/// Helper: build admin's bootstrap-Join → ChannelCreate prior state.
fn admin_with_channel_prior_state(
    admin_priv: &TestOwner,
    admin_addr: OwnerAddr,
    community_id: SpaceId,
    ch_id: ChannelId,
    ch_name: &str,
) -> harmony_app::community_membership::MaterializedMembership {
    let admin_join_payload = EventPayload {
        id: [0x01; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let admin_join = sign_event_with_identity(&admin_join_payload, admin_priv).expect("sign join");
    let ch_create_payload = EventPayload {
        id: [0x02; 16],
        community_id,
        kind: MembershipEventKind::ChannelCreate {
            channel_id: ch_id,
            name: ch_name.to_string(),
            write_power: 0,
            kind: ChannelKind::Text,
        },
        actor: admin_addr,
        at: Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let ch_create = sign_event_with_identity(&ch_create_payload, admin_priv).expect("sign create");
    materialize(&[admin_join, ch_create], admin_addr)
}

#[test]
fn verify_event_channel_create_rejects_empty_name() {
    let (admin_priv, _admin_pub, admin_addr) = make_test_identity(0xAA);
    let community_id = SpaceId([0x37; 16]);
    let admin_join_payload = EventPayload {
        id: [0x01; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let admin_join = sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign");
    let prior_state = materialize(&[admin_join], admin_addr);

    // Whitespace-only name is treated as empty.
    let payload = EventPayload {
        id: [0x02; 16],
        community_id,
        kind: MembershipEventKind::ChannelCreate {
            channel_id: ChannelId([0xAB; 16]),
            name: "   ".to_string(),
            write_power: 0,
            kind: ChannelKind::Text,
        },
        actor: admin_addr,
        at: Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &admin_priv).expect("sign");
    let ctx = VerifyContext {
        now_ms: None,
        expected_community_id: community_id,
        admin_addr,
        is_invite_only: false,
    };

    assert_eq!(
        verify_event(&event, &prior_state, &ctx),
        Err(VerifyError::ChannelNameInvalid)
    );
}

#[test]
fn verify_event_channel_create_rejects_oversized_name() {
    let (admin_priv, _admin_pub, admin_addr) = make_test_identity(0xAA);
    let community_id = SpaceId([0x37; 16]);
    let admin_join_payload = EventPayload {
        id: [0x01; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let admin_join = sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign");
    let prior_state = materialize(&[admin_join], admin_addr);

    // 33-char ASCII name (just over the 32-char §12.3 cap).
    let oversized = "a".repeat(33);
    let payload = EventPayload {
        id: [0x02; 16],
        community_id,
        kind: MembershipEventKind::ChannelCreate {
            channel_id: ChannelId([0xAB; 16]),
            name: oversized,
            write_power: 0,
            kind: ChannelKind::Text,
        },
        actor: admin_addr,
        at: Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &admin_priv).expect("sign");
    let ctx = VerifyContext {
        now_ms: None,
        expected_community_id: community_id,
        admin_addr,
        is_invite_only: false,
    };

    assert_eq!(
        verify_event(&event, &prior_state, &ctx),
        Err(VerifyError::ChannelNameInvalid)
    );
}

#[test]
fn verify_event_channel_create_rejects_write_power_above_max() {
    let (admin_priv, _admin_pub, admin_addr) = make_test_identity(0xAA);
    let community_id = SpaceId([0x37; 16]);
    let admin_join_payload = EventPayload {
        id: [0x01; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let admin_join = sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign");
    let prior_state = materialize(&[admin_join], admin_addr);

    // write_power 200 is above POWER_THRESHOLDS.max (= 100).
    let payload = EventPayload {
        id: [0x02; 16],
        community_id,
        kind: MembershipEventKind::ChannelCreate {
            channel_id: ChannelId([0xAB; 16]),
            name: "general".to_string(),
            write_power: 200,
            kind: ChannelKind::Text,
        },
        actor: admin_addr,
        at: Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &admin_priv).expect("sign");
    let ctx = VerifyContext {
        now_ms: None,
        expected_community_id: community_id,
        admin_addr,
        is_invite_only: false,
    };

    assert_eq!(
        verify_event(&event, &prior_state, &ctx),
        Err(VerifyError::PowerLevelOutOfRange)
    );
}

#[test]
fn verify_event_channel_modify_rejects_all_none() {
    let (admin_priv, _admin_pub, admin_addr) = make_test_identity(0xAA);
    let community_id = SpaceId([0x37; 16]);
    let ch_id = ChannelId([0xAB; 16]);
    let prior_state =
        admin_with_channel_prior_state(&admin_priv, admin_addr, community_id, ch_id, "general");

    // ChannelModify with both name AND write_power None is a no-op.
    let payload = EventPayload {
        id: [0x03; 16],
        community_id,
        kind: MembershipEventKind::ChannelModify {
            channel_id: ch_id,
            name: None,
            write_power: None,
        },
        actor: admin_addr,
        at: Hlc {
            wall_ms: 3_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &admin_priv).expect("sign");
    let ctx = VerifyContext {
        now_ms: None,
        expected_community_id: community_id,
        admin_addr,
        is_invite_only: false,
    };

    assert_eq!(
        verify_event(&event, &prior_state, &ctx),
        Err(VerifyError::ChannelModifyNoOp)
    );
}

#[test]
fn verify_event_channel_modify_allows_unknown_channel_id() {
    // DAG-sync may deliver Modify before Create; verify_event must NOT
    // reject Modify-on-unknown — materialize safely no-ops on unknown.
    let (admin_priv, _admin_pub, admin_addr) = make_test_identity(0xAA);
    let community_id = SpaceId([0x37; 16]);
    let admin_join_payload = EventPayload {
        id: [0x01; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let admin_join = sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign");
    let prior_state = materialize(&[admin_join], admin_addr);
    // No ChannelCreate in prior_state — channel is unknown.

    let payload = EventPayload {
        id: [0x02; 16],
        community_id,
        kind: MembershipEventKind::ChannelModify {
            channel_id: ChannelId([0xCC; 16]), // never created
            name: Some("ghost".into()),
            write_power: None,
        },
        actor: admin_addr,
        at: Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let event = sign_event_with_identity(&payload, &admin_priv).expect("sign");
    let ctx = VerifyContext {
        now_ms: None,
        expected_community_id: community_id,
        admin_addr,
        is_invite_only: false,
    };

    assert_eq!(verify_event(&event, &prior_state, &ctx), Ok(()));
}

#[test]
fn verify_event_channel_delete_allows_already_tombstoned_for_replica_convergence() {
    // ChannelDelete on already-tombstoned channel must NOT reject at
    // verify_event. Two mods concurrently deleting the same channel
    // would otherwise diverge across replicas based on receive order;
    // materialize handles redundant deletes via first-delete-wins
    // idempotency.
    let (admin_priv, _admin_pub, admin_addr) = make_test_identity(0xAA);
    let community_id = SpaceId([0x37; 16]);

    let admin_join_payload = EventPayload {
        id: [0x01; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let admin_join = sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign");

    let ch_id = ChannelId([0xAB; 16]);
    let create_payload = EventPayload {
        id: [0x02; 16],
        community_id,
        kind: MembershipEventKind::ChannelCreate {
            channel_id: ch_id,
            name: "general".to_string(),
            write_power: 0,
            kind: ChannelKind::Text,
        },
        actor: admin_addr,
        at: Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let create = sign_event_with_identity(&create_payload, &admin_priv).expect("sign");

    let first_delete_payload = EventPayload {
        id: [0x03; 16],
        community_id,
        kind: MembershipEventKind::ChannelDelete { channel_id: ch_id },
        actor: admin_addr,
        at: Hlc {
            wall_ms: 3_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let first_delete = sign_event_with_identity(&first_delete_payload, &admin_priv).expect("sign");

    let prior_state = materialize(&[admin_join, create, first_delete], admin_addr);
    // Channel is now tombstoned in prior_state.

    let second_delete_payload = EventPayload {
        id: [0x04; 16],
        community_id,
        kind: MembershipEventKind::ChannelDelete { channel_id: ch_id },
        actor: admin_addr,
        at: Hlc {
            wall_ms: 4_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let second_delete =
        sign_event_with_identity(&second_delete_payload, &admin_priv).expect("sign");

    let ctx = VerifyContext {
        now_ms: None,
        expected_community_id: community_id,
        admin_addr,
        is_invite_only: false,
    };

    assert_eq!(verify_event(&second_delete, &prior_state, &ctx), Ok(()));
}

#[test]
fn verify_event_channel_modify_allows_value_matching_for_replica_convergence() {
    // ChannelModify with values exactly matching prior state must NOT
    // reject at verify_event. Two mods independently making the same
    // rename would otherwise diverge across replicas based on receive
    // order; materialize handles redundant modifies via the
    // get_mut + only-Some-applies pattern (no-op when values match).
    let (admin_priv, _admin_pub, admin_addr) = make_test_identity(0xAA);
    let community_id = SpaceId([0x37; 16]);

    let admin_join_payload = EventPayload {
        id: [0x01; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let admin_join = sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign");

    let ch_id = ChannelId([0xAB; 16]);
    let create_payload = EventPayload {
        id: [0x02; 16],
        community_id,
        kind: MembershipEventKind::ChannelCreate {
            channel_id: ch_id,
            name: "general".to_string(),
            write_power: 0,
            kind: ChannelKind::Text,
        },
        actor: admin_addr,
        at: Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let create = sign_event_with_identity(&create_payload, &admin_priv).expect("sign");

    let prior_state = materialize(&[admin_join, create], admin_addr);

    // Modify with values that exactly match current state — would
    // previously have been rejected with ChannelModifyNoOp; now accepted
    // for replica convergence.
    let modify_payload = EventPayload {
        id: [0x03; 16],
        community_id,
        kind: MembershipEventKind::ChannelModify {
            channel_id: ch_id,
            name: Some("general".to_string()),
            write_power: Some(0),
        },
        actor: admin_addr,
        at: Hlc {
            wall_ms: 3_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let modify = sign_event_with_identity(&modify_payload, &admin_priv).expect("sign");

    let ctx = VerifyContext {
        now_ms: None,
        expected_community_id: community_id,
        admin_addr,
        is_invite_only: false,
    };

    assert_eq!(verify_event(&modify, &prior_state, &ctx), Ok(()));
}

#[test]
fn verify_event_channel_modify_accepts_partial_change() {
    // Only one of (name, write_power) changes — should still be accepted.
    let (admin_priv, _admin_pub, admin_addr) = make_test_identity(0xAA);
    let community_id = SpaceId([0x37; 16]);

    let admin_join_payload = EventPayload {
        id: [0x01; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let admin_join = sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign");

    let ch_id = ChannelId([0xAB; 16]);
    let create_payload = EventPayload {
        id: [0x02; 16],
        community_id,
        kind: MembershipEventKind::ChannelCreate {
            channel_id: ch_id,
            name: "general".to_string(),
            write_power: 0,
            kind: ChannelKind::Text,
        },
        actor: admin_addr,
        at: Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let create = sign_event_with_identity(&create_payload, &admin_priv).expect("sign");

    let prior_state = materialize(&[admin_join, create], admin_addr);

    // Modify with name change only.
    let modify_payload = EventPayload {
        id: [0x03; 16],
        community_id,
        kind: MembershipEventKind::ChannelModify {
            channel_id: ch_id,
            name: Some("renamed".to_string()), // different from "general"
            write_power: None,                 // unchanged
        },
        actor: admin_addr,
        at: Hlc {
            wall_ms: 3_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let modify = sign_event_with_identity(&modify_payload, &admin_priv).expect("sign");

    let ctx = VerifyContext {
        now_ms: None,
        expected_community_id: community_id,
        admin_addr,
        is_invite_only: false,
    };

    assert_eq!(verify_event(&modify, &prior_state, &ctx), Ok(()));
}

#[test]
fn verify_event_channel_delete_allows_unknown_channel_for_dag_sync_safety() {
    // ChannelDelete on unknown channel must NOT reject at verify_event —
    // cross-blob ordering can deliver Delete before its corresponding
    // Create. materialize handles unknown delete as no-op; the eventual
    // arrival of Create + replay converges correctly.
    let (admin_priv, _admin_pub, admin_addr) = make_test_identity(0xAA);
    let community_id = SpaceId([0x37; 16]);

    let admin_join_payload = EventPayload {
        id: [0x01; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let admin_join = sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign");
    let prior_state = materialize(&[admin_join], admin_addr);

    let delete_payload = EventPayload {
        id: [0x02; 16],
        community_id,
        kind: MembershipEventKind::ChannelDelete {
            channel_id: ChannelId([0xCC; 16]), // never created
        },
        actor: admin_addr,
        at: Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let delete = sign_event_with_identity(&delete_payload, &admin_priv).expect("sign");

    let ctx = VerifyContext {
        now_ms: None,
        expected_community_id: community_id,
        admin_addr,
        is_invite_only: false,
    };

    assert_eq!(verify_event(&delete, &prior_state, &ctx), Ok(()));
}

#[test]
fn verify_event_channel_create_allows_duplicate_channel_id_for_replica_convergence() {
    // ChannelCreate with a channel_id already in prior_state must NOT
    // reject at verify_event. Materialize's or_insert_with idempotency
    // ensures convergence (first-create-wins). Verify-time rejection
    // would cause log divergence (some replicas store both, some only
    // one) without changing the materialized view.
    let (admin_priv, _admin_pub, admin_addr) = make_test_identity(0xAA);
    let community_id = SpaceId([0x37; 16]);

    let admin_join_payload = EventPayload {
        id: [0x01; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let admin_join = sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign");

    let ch_id = ChannelId([0xAB; 16]);
    let first_create_payload = EventPayload {
        id: [0x02; 16],
        community_id,
        kind: MembershipEventKind::ChannelCreate {
            channel_id: ch_id,
            name: "first".to_string(),
            write_power: 0,
            kind: ChannelKind::Text,
        },
        actor: admin_addr,
        at: Hlc {
            wall_ms: 2_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let first_create = sign_event_with_identity(&first_create_payload, &admin_priv).expect("sign");

    let prior_state = materialize(&[admin_join, first_create], admin_addr);
    // prior_state.channels now has ch_id → "first"

    // Second ChannelCreate with same channel_id (different EventId, different name).
    let second_create_payload = EventPayload {
        id: [0x03; 16],
        community_id,
        kind: MembershipEventKind::ChannelCreate {
            channel_id: ch_id,
            name: "second".to_string(),
            write_power: 50,
            kind: ChannelKind::Text,
        },
        actor: admin_addr,
        at: Hlc {
            wall_ms: 3_000,
            logical: 0,
            device_id: "a".into(),
        },
    };
    let second_create =
        sign_event_with_identity(&second_create_payload, &admin_priv).expect("sign");

    let ctx = VerifyContext {
        now_ms: None,
        expected_community_id: community_id,
        admin_addr,
        is_invite_only: false,
    };

    assert_eq!(verify_event(&second_create, &prior_state, &ctx), Ok(()));
}
