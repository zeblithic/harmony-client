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
