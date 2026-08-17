//! ZEB-950: a moderator (non-admin) can originate an invite that redeems
//! without the admin signing anything in the redemption.
//!
//! The receive/consensus side was already inviter-agnostic (ZEB-911 puts
//! admission authority on the finalizing witness's power, and the invite token
//! is verified against the INVITER's enrolled device keys resolved from
//! materialized membership — not against the admin). This test pins that: a
//! moderator raised to power 50 mints an invite-only token; the joiner's
//! `verify_admin_bootstrap` still roots trust in the community's admin, and the
//! token verifies against the moderator's keys — with the admin authoring
//! nothing in the redemption artifacts.
#![cfg(test)]

use crate::community_invite::{
    verify_admin_bootstrap, CommunityInvitePayload, InviteEpochSnapshot, MaterializedCommunityState,
};
use crate::community_membership::{
    materialize, mint_test_owner, sign_event, verify_invite_token_sig_with_enrolled, EventPayload,
    MemberStatus, MembershipEventKind, VerifyContext,
};
use crate::community_state_crdt::{CommunityState, InsertOutcome};
use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

fn hlc(wall_ms: u64, dev: &str) -> Hlc {
    Hlc {
        wall_ms,
        logical: 0,
        device_id: dev.to_string(),
    }
}

/// An OPEN invite carrying the raw 32-byte membership key (the shape a member
/// redeems to Join). Mirrors the Phase-2 slim open shape.
fn open_invite(
    community_id: SpaceId,
    admin_addr: OwnerAddr,
    key_bytes: Vec<u8>,
) -> CommunityInvitePayload {
    CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id,
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: key_bytes,
            sealed_epoch_keys: Vec::new(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr,
        community_name: "Comm".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        inviter_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: None,
        untargeted_decrypt_key: None,
    }
}

#[test]
fn moderator_originated_invite_verifies_admin_absent() {
    // Admin creates the community.
    let admin = mint_test_owner(1);
    let created = crate::mint_community_creation(
        "Comm",
        false,
        admin.owner,
        &admin.device_key,
        &admin.cert,
        hlc(100_000, "admin"),
    )
    .expect("create");
    let community_id = created.community_id;

    // A moderator joins via an open invite, then the admin raises them to power 50.
    let moderator = mint_test_owner(2);
    let mod_join = crate::mint_redemption(
        &open_invite(
            community_id,
            admin.owner,
            created.membership_key.as_bytes().to_vec(),
        ),
        moderator.owner,
        &moderator.device_key,
        &moderator.cert,
        hlc(200_000, "mod"),
    )
    .expect("mod redeem")
    .bootstrap_join;

    let set_power = sign_event(
        &EventPayload {
            id: [3u8; 16],
            community_id,
            kind: MembershipEventKind::SetPower {
                target: moderator.owner,
                level: 50,
            },
            actor: admin.owner,
            at: hlc(250_000, "admin"),
        },
        &admin.device_key,
    )
    .expect("sign set_power");

    // Build + verify the log (every event passes verify_event on insert).
    let mut state = CommunityState::new(community_id);
    let ctx = VerifyContext {
        expected_community_id: community_id,
        admin_addr: admin.owner,
        is_invite_only: false,
        now_ms: None,
    };
    for (label, ev) in [
        ("admin bootstrap", created.bootstrap_join.clone()),
        ("moderator Join", mod_join.clone()),
        ("admin SetPower(moderator, 50)", set_power.clone()),
    ] {
        assert!(
            matches!(state.insert_event(ev, &ctx), InsertOutcome::Inserted),
            "log build rejected {label}"
        );
    }

    let events: Vec<_> = state.events().cloned().collect();
    let mat = materialize(&events, admin.owner);
    assert_eq!(
        mat.members.get(&moderator.owner).map(|m| m.status),
        Some(MemberStatus::Joined),
        "moderator must be a Joined member"
    );
    assert_eq!(
        mat.power_levels.get(&moderator.owner).copied(),
        Some(50),
        "moderator must sit at power 50 (>= default invite threshold 0)"
    );

    // The MODERATOR (not the admin) mints an invite-only token.
    let token = crate::invite_mint::mint_invite_token(
        moderator.owner,
        None,
        hlc(300_000, "mod"),
        Some(u64::MAX),
        &moderator.device_key,
    )
    .expect("moderator mints token");
    assert_eq!(token.inviter, moderator.owner);
    assert_ne!(
        token.inviter, admin.owner,
        "the inviter is the moderator, not the admin"
    );

    // A moderator-originated invite-only payload: admin_bootstrap is the ADMIN's
    // self-Join (trust anchor, pulled from the synced log — the moderator has
    // it), while the token + identity + enrollment are the MODERATOR's.
    let mut mod_identity_pub = [0u8; 64];
    mod_identity_pub[..32].copy_from_slice(&moderator.device_key.verifying_key().to_bytes());
    mod_identity_pub[32..].copy_from_slice(&moderator.device_key.verifying_key().to_bytes());
    let payload = CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id,
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: created.membership_key.as_bytes().to_vec(),
            sealed_epoch_keys: Vec::new(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: admin.owner,
        community_name: "Comm".into(),
        is_invite_only: true,
        expires_at: None,
        invite_token: Some(token.clone()),
        admin_bootstrap: Some(created.bootstrap_join.clone()),
        inviter_identity_pub: Some(mod_identity_pub),
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: Some(moderator.cert.clone()),
        untargeted_decrypt_key: None,
    };

    // (a) The joiner's trust anchor is intact: admin_bootstrap verifies as the
    //     ADMIN's self-Join regardless of who the inviter is.
    let (boot_ev, _admin_id) =
        verify_admin_bootstrap(&payload).expect("admin bootstrap verifies regardless of inviter");
    assert_eq!(
        boot_ev.actor, admin.owner,
        "the trust anchor is the admin's self-Join"
    );

    // (b) The invite token verifies against the MODERATOR's enrolled device keys
    //     (resolved from materialized membership) — the receive path is
    //     inviter-agnostic, so a moderator's token is accepted.
    verify_invite_token_sig_with_enrolled(&token, &mat)
        .expect("moderator token verifies via the moderator's enrolled keys");
}
