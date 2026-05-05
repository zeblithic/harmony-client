//! Unit tests for community_state_crdt.rs Phase 2 types.

use harmony_app::community_membership::{
    sign_event_with_identity, EventPayload, MembershipEventKind, VerifyContext, VerifyError,
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

fn make_test_identity() -> (PrivateIdentity, [u8; 64], OwnerAddr) {
    let identity = PrivateIdentity::from_seed(&[0xa1; 32]);
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
    let (identity, identity_pub, addr) = make_test_identity();
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
    let (identity, identity_pub, addr) = make_test_identity();
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

    assert!(matches!(outcome, InsertOutcome::Inserted));
    assert_eq!(state.events.len(), 1);
    assert!(state.events.contains_key(&event_id));
}

#[test]
fn insert_is_idempotent_on_duplicate_event_id() {
    let (identity, identity_pub, addr) = make_test_identity();
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
    assert!(matches!(
        state.insert_event(event.clone(), &ctx),
        InsertOutcome::Inserted
    ));
    assert!(matches!(
        state.insert_event(event, &ctx),
        InsertOutcome::AlreadyKnown
    ));
    assert_eq!(state.events.len(), 1);
}
