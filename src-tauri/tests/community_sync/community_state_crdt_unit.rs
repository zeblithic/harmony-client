//! Unit tests for community_state_crdt.rs Phase 2 types.

use harmony_app::community_membership::{
    mint_test_owner, sign_event, EventPayload, MemberStatus, MembershipEventKind,
    SignedMembershipEvent, TestOwner, VerifyContext, VerifyError,
};
use harmony_app::community_state_crdt::{CommunityState, InsertOutcome};
use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

/// ZEB-339: build a deterministic enrolled-device owner. Returns
/// `(TestOwner, dummy_pub, owner_addr)` so existing `(identity, pub, addr)`
/// destructures keep compiling; the actor is `owner.owner` (owner_id) and
/// events are signed by the enrolled device key (#2).
fn make_test_identity(seed: u8) -> (TestOwner, [u8; 64], OwnerAddr) {
    let owner = mint_test_owner(seed);
    let addr = owner.owner;
    (owner, [0u8; 64], addr)
}

/// ZEB-339: sign a membership event with the owner's enrolled device key,
/// attaching the Master cert on identity-introducing events (Join/PendingJoin)
/// so materialize/insert populates `enrolled_device_keys`.
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

#[test]
fn empty_community_state_round_trips() {
    let s = CommunityState::new(SpaceId([1u8; 16]));
    let bytes = canonical_cbor_encode(&s).expect("encode");
    let decoded: CommunityState = canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(decoded.community_id, s.community_id);
    assert!(decoded.events_is_empty());
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
    let (identity, _identity_pub, addr) = make_test_identity(0xa1);
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
        },
    );

    assert!(matches!(
        outcome,
        InsertOutcome::Rejected(VerifyError::WrongCommunity)
    ));
    assert!(
        state.events_is_empty(),
        "rejected event must not land in log"
    );
}

#[test]
fn insert_accepts_admin_self_join_in_open_community() {
    let (identity, _identity_pub, addr) = make_test_identity(0xa1);
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
        },
    );

    assert_eq!(outcome, InsertOutcome::Inserted);
    assert_eq!(state.event_count(), 1);
    assert!(state.contains_event(&event_id));
}

#[test]
fn insert_is_idempotent_on_duplicate_event_id() {
    let (identity, _identity_pub, addr) = make_test_identity(0xa1);
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
    };
    assert_eq!(
        state.insert_event(event.clone(), &ctx),
        InsertOutcome::Inserted
    );
    assert_eq!(state.insert_event(event, &ctx), InsertOutcome::AlreadyKnown);
    assert_eq!(state.event_count(), 1);
}

/// Build a `CommunityState` containing one verified admin self-Join.
/// Returned tuple matches the call-site shape of the other tests so
/// future tests can extend the log without rewriting setup.
fn state_with_admin_self_join(seed: u8, community_id: SpaceId) -> (CommunityState, OwnerAddr) {
    let (identity, _identity_pub, addr) = make_test_identity(seed);
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
    let (identity, _identity_pub, addr) = make_test_identity(0xa1);
    let community_id = SpaceId([1u8; 16]);
    let mut state = CommunityState::new(community_id);

    let v0 = state.materialized_version();
    let m1 = state.materialized(addr);
    let m2 = state.materialized(addr);
    assert_eq!(
        state.materialized_version(),
        v0,
        "version unchanged on read"
    );
    assert_eq!(m1, m2, "cache hit returns equal value");

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
        },
    );

    let v1 = state.materialized_version();
    assert!(v1 > v0, "version bumps on successful insert");

    let m_after = state.materialized(addr);
    assert_eq!(m_after.members.len(), 1, "Join event materialized");
}

// ZEB-287 Phase 2: CommunityState.parent_lineage + forked_at_wall_ms tests
mod zeb287_lineage_fields {
    use harmony_app::community_invite::ParentLineageEntry;
    use harmony_app::community_state_crdt::CommunityState;
    use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
    use harmony_app::owner_state_types::SpaceId;

    #[test]
    fn community_state_with_no_lineage_omits_fa_and_fl_keys() {
        let state = CommunityState::new(SpaceId([0x77; 16]));
        let bytes = canonical_cbor_encode(&state).expect("encode");
        assert!(
            !bytes.windows(2).any(|w| w == b"fa"),
            "skip_serializing_if failed to drop `fa` for None"
        );
        assert!(
            !bytes.windows(2).any(|w| w == b"fl"),
            "skip_serializing_if failed to drop `fl` for empty Vec"
        );
    }

    #[test]
    fn community_state_with_populated_lineage_roundtrips() {
        let mut state = CommunityState::new(SpaceId([0x88; 16]));
        state.forked_from = Some(SpaceId([0x22; 16]));
        state.forked_at_wall_ms = Some(1_715_811_234_567);
        state.parent_lineage = vec![ParentLineageEntry {
            space_id: SpaceId([0x11; 16]),
            name: "Project Cool".to_string(),
            forked_at_wall_ms: None,
            reason: None,
        }];

        let bytes = canonical_cbor_encode(&state).expect("encode");
        let decoded: CommunityState = canonical_cbor_decode(&bytes).expect("decode");

        assert_eq!(state, decoded);
    }

    #[test]
    fn community_state_manual_clone_preserves_lineage() {
        let mut state = CommunityState::new(SpaceId([0x99; 16]));
        state.parent_lineage = vec![
            ParentLineageEntry {
                space_id: SpaceId([0x11; 16]),
                name: "C".to_string(),
                forked_at_wall_ms: None,
                reason: None,
            },
            ParentLineageEntry {
                space_id: SpaceId([0x22; 16]),
                name: "B".to_string(),
                forked_at_wall_ms: Some(123_456),
                reason: None,
            },
        ];
        state.forked_at_wall_ms = Some(789_012);

        let cloned = state.clone();
        assert_eq!(state.parent_lineage, cloned.parent_lineage);
        assert_eq!(state.forked_at_wall_ms, cloned.forked_at_wall_ms);
    }
}
