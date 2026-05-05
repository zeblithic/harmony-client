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
