//! Unit tests for community_invite.rs Phase 1 types.

use harmony_app::community_invite::{CommunityInvitePayload, InviteToken};
use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use harmony_app::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};

#[test]
fn community_invite_payload_round_trips_open_form() {
    let p = CommunityInvitePayload {
        community_id: SpaceId([1u8; 16]),
        membership_key: MembershipKey::new([2u8; 32]),
        admin_addr: OwnerAddr([3u8; 16]),
        community_name: "harmony-design".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
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
        sig: [0xCC; 64],
    };

    let p = CommunityInvitePayload {
        community_id: SpaceId([1u8; 16]),
        membership_key: MembershipKey::new([2u8; 32]),
        admin_addr: OwnerAddr([3u8; 16]),
        community_name: "private".into(),
        is_invite_only: true,
        expires_at: Some(Hlc {
            wall_ms: 9999,
            logical: 0,
            device_id: "d".into(),
        }),
        invite_token: Some(token.clone()),
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
    assert_eq!(decoded.invite_token.as_ref().map(|t| t.inviter), Some(token.inviter));
    assert_eq!(decoded.invite_token.as_ref().map(|t| t.sig), Some(token.sig));
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
        sig: [0u8; 64],
    };

    let bytes = canonical_cbor_encode(&t).expect("encode");
    let decoded: InviteToken = canonical_cbor_decode(&bytes).expect("decode");

    assert_eq!(decoded.invitee_hint, None);
    assert_eq!(decoded.inviter, OwnerAddr([1u8; 16]));
}
