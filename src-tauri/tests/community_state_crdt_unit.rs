//! Unit tests for community_state_crdt.rs Phase 2 types.

use harmony_app::community_membership::{
    sign_event_with_identity, EventPayload, MemberStatus, MembershipEventKind, VerifyContext,
    VerifyError,
};
use harmony_app::community_state_crdt::{CommunityState, InsertOutcome};
use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use harmony_identity::PrivateIdentity;

#[test]
fn empty_community_state_round_trips() {
    let s = CommunityState::new(SpaceId([1u8; 16]));
    let bytes = canonical_cbor_encode(&s).expect("encode");
    let decoded: CommunityState = canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(decoded.community_id, s.community_id);
    assert!(decoded.events.is_empty());
}

fn make_test_identity(seed: u8) -> (PrivateIdentity, [u8; 64], OwnerAddr) {
    let identity = PrivateIdentity::from_seed(&[seed; 32]);
    let identity_pub = identity.identity.to_public_bytes();
    let addr = OwnerAddr(identity.identity.address_hash);
    (identity, identity_pub, addr)
}

fn hlc(wall_ms: u64) -> Hlc {
    Hlc {
        wall_ms,
        logical: 0,
        device_id: "d".into(),
    }
}

#[test]
fn insert_rejects_event_with_wrong_community() {
    let (identity, identity_pub, addr) = make_test_identity(0xa1);
    let community_id = SpaceId([1u8; 16]);
    let other_community = SpaceId([2u8; 16]);

    let payload = EventPayload {
        id: [3u8; 16],
        community_id: other_community,
        kind: MembershipEventKind::Join,
        actor: addr,
        at: hlc(100),
    };
    let event = sign_event_with_identity(&payload, &identity).expect("sign");

    let mut state = CommunityState::new(community_id);
    let outcome = state.insert_event(
        event,
        &VerifyContext {
            expected_community_id: community_id,
            admin_addr: addr,
            is_invite_only: false,
            actor_identity_pub: &identity_pub,
            countersigner_identity_pub: None,
        },
    );

    assert!(matches!(
        outcome,
        InsertOutcome::Rejected(VerifyError::WrongCommunity)
    ));
    assert!(
        state.events.is_empty(),
        "rejected event must not land in log"
    );
}

#[test]
fn insert_accepts_admin_self_join_in_open_community() {
    let (identity, identity_pub, addr) = make_test_identity(0xa1);
    let community_id = SpaceId([1u8; 16]);

    let payload = EventPayload {
        id: [3u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: addr,
        at: hlc(100),
    };
    let event = sign_event_with_identity(&payload, &identity).expect("sign");
    let event_id = event.id;

    let mut state = CommunityState::new(community_id);
    let outcome = state.insert_event(
        event,
        &VerifyContext {
            expected_community_id: community_id,
            admin_addr: addr,
            is_invite_only: false,
            actor_identity_pub: &identity_pub,
            countersigner_identity_pub: None,
        },
    );

    assert_eq!(outcome, InsertOutcome::Inserted);
    assert_eq!(state.events.len(), 1);
    assert!(state.events.contains_key(&event_id));
}

#[test]
fn insert_is_idempotent_on_duplicate_event_id() {
    let (identity, identity_pub, addr) = make_test_identity(0xa1);
    let community_id = SpaceId([1u8; 16]);

    let payload = EventPayload {
        id: [3u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: addr,
        at: hlc(100),
    };
    let event = sign_event_with_identity(&payload, &identity).expect("sign");

    let mut state = CommunityState::new(community_id);
    let ctx = VerifyContext {
        expected_community_id: community_id,
        admin_addr: addr,
        is_invite_only: false,
        actor_identity_pub: &identity_pub,
        countersigner_identity_pub: None,
    };
    assert_eq!(
        state.insert_event(event.clone(), &ctx),
        InsertOutcome::Inserted
    );
    assert_eq!(state.insert_event(event, &ctx), InsertOutcome::AlreadyKnown);
    assert_eq!(state.events.len(), 1);
}

/// Build a `CommunityState` containing one verified admin self-Join.
/// Returned tuple matches the call-site shape of the other tests so
/// future tests can extend the log without rewriting setup.
fn state_with_admin_self_join(seed: u8, community_id: SpaceId) -> (CommunityState, OwnerAddr) {
    let (identity, identity_pub, addr) = make_test_identity(seed);
    let payload = EventPayload {
        id: [3u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: addr,
        at: hlc(100),
    };
    let event = sign_event_with_identity(&payload, &identity).expect("sign");
    let mut state = CommunityState::new(community_id);
    let outcome = state.insert_event(
        event,
        &VerifyContext {
            expected_community_id: community_id,
            admin_addr: addr,
            is_invite_only: false,
            actor_identity_pub: &identity_pub,
            countersigner_identity_pub: None,
        },
    );
    assert_eq!(outcome, InsertOutcome::Inserted);
    (state, addr)
}

#[test]
fn materialize_now_reflects_admin_self_join() {
    let community_id = SpaceId([1u8; 16]);
    let (state, addr) = state_with_admin_self_join(0xa1, community_id);

    let view = state.materialize_now(addr);
    let member = view
        .members
        .get(&addr)
        .expect("admin self-Join should appear in materialized view");
    assert_eq!(member.status, MemberStatus::Joined);
}

#[test]
fn materialized_cache_returns_same_object_until_insert() {
    let (identity, identity_pub, addr) = make_test_identity(0xa1);
    let community_id = SpaceId([1u8; 16]);
    let mut state = CommunityState::new(community_id);

    let v0 = state.materialized_version();
    let _m1 = state.materialized(addr);
    let _m2 = state.materialized(addr);
    assert_eq!(
        state.materialized_version(),
        v0,
        "version unchanged on read"
    );

    let payload = EventPayload {
        id: [3u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: addr,
        at: hlc(100),
    };
    let event = sign_event_with_identity(&payload, &identity).expect("sign");
    state.insert_event(
        event,
        &VerifyContext {
            expected_community_id: community_id,
            admin_addr: addr,
            is_invite_only: false,
            actor_identity_pub: &identity_pub,
            countersigner_identity_pub: None,
        },
    );

    let v1 = state.materialized_version();
    assert!(v1 > v0, "version bumps on successful insert");

    let m_after = state.materialized(addr);
    assert_eq!(m_after.members.len(), 1, "Join event materialized");
}
