//! ZEB-949 Phase 2 — regression coverage for slim-bootstrap invites.
//!
//! Proves the receive-side membership-at-HLC gate needs only `admin_bootstrap`
//! + the P2P-synced event log — never the inlined roster snapshot. Exercises the
//! real gate (`CommunityState::insert_event` -> `verify_event` against the
//! strictly-prior materialized state). Also pins the size property of a slim
//! invite (the size fixture below).
#![cfg(test)]

use std::collections::{BTreeMap, BTreeSet};

use crate::community_invite::{
    encode_invite_url, CommunityInvitePayload, InviteEpochSnapshot, MaterializedCommunityState,
};
use crate::community_membership::{
    materialize, mint_test_owner, MemberState, MemberStatus, VerifyContext,
};
use crate::community_state_crdt::{CommunityState, InsertOutcome};
use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

/// An OPEN invite whose inlined roster snapshot is EMPTY — the Phase-2 slim shape.
fn slim_open_invite(
    community_id: SpaceId,
    admin_addr: OwnerAddr,
    membership_key_bytes: Vec<u8>,
) -> CommunityInvitePayload {
    CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id,
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: membership_key_bytes,
            sealed_epoch_keys: Vec::new(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr,
        community_name: "SlimComm".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: None,
        untargeted_decrypt_key: None,
    }
}

fn hlc(wall_ms: u64, device_id: &str) -> Hlc {
    Hlc {
        wall_ms,
        logical: 0,
        device_id: device_id.to_string(),
    }
}

/// A fresh empty-log joiner (no roster) accepts a member-authored GATED event
/// and materializes the full roster — from the synced log alone.
#[test]
fn slim_bootstrap_joiner_verifies_full_community_from_synced_log_alone() {
    let admin = mint_test_owner(1);
    let minted_admin = crate::mint_community_creation(
        "SlimComm",
        false,
        admin.owner,
        &admin.device_key,
        &admin.cert,
        hlc(100_000, "admin-dev"),
    )
    .expect("mint create");
    let community_id = minted_admin.community_id;
    let membership_key = minted_admin.membership_key.clone();

    let bob = mint_test_owner(2);
    let invite = slim_open_invite(
        community_id,
        admin.owner,
        membership_key.as_bytes().to_vec(),
    );
    let minted_bob = crate::mint_redemption(
        &invite,
        bob.owner,
        &bob.device_key,
        &bob.cert,
        hlc(200_000, "bob-dev"),
    )
    .expect("mint redeem");

    let bob_leave = crate::mint_leave_event(
        community_id,
        bob.owner,
        &bob.device_key,
        hlc(300_000, "bob-dev"),
    )
    .expect("mint leave");

    let mut joiner = CommunityState::new(community_id);
    let ctx = VerifyContext {
        expected_community_id: community_id,
        admin_addr: admin.owner,
        is_invite_only: false,
        now_ms: None,
    };

    for (label, ev) in [
        ("admin bootstrap Join", minted_admin.bootstrap_join.clone()),
        ("Bob redemption Join", minted_bob.bootstrap_join.clone()),
        ("Bob-authored Leave (gated)", bob_leave.clone()),
    ] {
        let outcome = joiner.insert_event(ev, &ctx);
        assert!(
            matches!(outcome, InsertOutcome::Inserted),
            "roster-less joiner's gate rejected {label}: {outcome:?}"
        );
    }

    let events: Vec<_> = joiner.events().cloned().collect();
    assert_eq!(events.len(), 3);
    let mat = materialize(&events, admin.owner);
    assert_eq!(
        mat.members.get(&admin.owner).map(|m| m.status),
        Some(MemberStatus::Joined)
    );
    assert_eq!(
        mat.members.get(&bob.owner).map(|m| m.status),
        Some(MemberStatus::Left),
        "Bob's gated Leave verified + applied — the gate needed no inlined roster"
    );
}

/// The gate is verify-ON-INSERT: an out-of-order member event is Rejected and
/// recovers on re-delivery once the Join lands. This is why Phase-2 sync must
/// deliver Join-before-authored (a sort-ordered state-root batch merge does) and
/// why the engine defers-not-drops (ZEB-526) unknown publishers.
#[test]
fn gate_is_on_insert_out_of_order_member_event_rejected_then_recovers() {
    let admin = mint_test_owner(1);
    let minted_admin = crate::mint_community_creation(
        "SlimComm",
        false,
        admin.owner,
        &admin.device_key,
        &admin.cert,
        hlc(100_000, "admin-dev"),
    )
    .expect("mint create");
    let community_id = minted_admin.community_id;
    let membership_key = minted_admin.membership_key.clone();

    let bob = mint_test_owner(2);
    let invite = slim_open_invite(
        community_id,
        admin.owner,
        membership_key.as_bytes().to_vec(),
    );
    let minted_bob = crate::mint_redemption(
        &invite,
        bob.owner,
        &bob.device_key,
        &bob.cert,
        hlc(200_000, "bob-dev"),
    )
    .expect("mint redeem");
    let bob_leave = crate::mint_leave_event(
        community_id,
        bob.owner,
        &bob.device_key,
        hlc(300_000, "bob-dev"),
    )
    .expect("mint leave");

    let mut joiner = CommunityState::new(community_id);
    let ctx = VerifyContext {
        expected_community_id: community_id,
        admin_addr: admin.owner,
        is_invite_only: false,
        now_ms: None,
    };

    assert!(matches!(
        joiner.insert_event(minted_admin.bootstrap_join.clone(), &ctx),
        InsertOutcome::Inserted
    ));

    let early = joiner.insert_event(bob_leave.clone(), &ctx);
    assert!(
        matches!(early, InsertOutcome::Rejected(_)),
        "out-of-order member event must be rejected by the on-insert gate, got {early:?}"
    );

    assert!(matches!(
        joiner.insert_event(minted_bob.bootstrap_join.clone(), &ctx),
        InsertOutcome::Inserted
    ));

    {
        let events: Vec<_> = joiner.events().cloned().collect();
        let mat = materialize(&events, admin.owner);
        assert_eq!(
            mat.members.get(&bob.owner).map(|m| m.status),
            Some(MemberStatus::Joined)
        );
    }

    assert!(matches!(
        joiner.insert_event(bob_leave.clone(), &ctx),
        InsertOutcome::Inserted
    ));
    let events: Vec<_> = joiner.events().cloned().collect();
    let mat = materialize(&events, admin.owner);
    assert_eq!(
        mat.members.get(&bob.owner).map(|m| m.status),
        Some(MemberStatus::Left)
    );
}

// ── ZEB-949 Task 2: codec size fixture ──────────────────────────────────────

/// Build a synthetic MaterializedCommunityState with `n` members carrying
/// pseudo-random (incompressible) device-key + owner-addr bytes, so the size
/// measurement reflects the real cryptographic-core cost per member.
fn synthetic_roster(n: usize) -> MaterializedCommunityState {
    let mut members = BTreeMap::new();
    for i in 0..n {
        // Cheap deterministic spread across all bytes (no rng dependency).
        let mut addr = [0u8; 16];
        for (j, b) in addr.iter_mut().enumerate() {
            *b = i
                .wrapping_mul(2_654_435_761)
                .wrapping_add(j.wrapping_mul(97)) as u8;
        }
        let mut key = [0u8; 32];
        for (j, b) in key.iter_mut().enumerate() {
            *b = i
                .wrapping_mul(40_503)
                .wrapping_add(j.wrapping_mul(131))
                .wrapping_add(7) as u8;
        }
        let mut keys = BTreeSet::new();
        keys.insert(key);
        members.insert(
            OwnerAddr(addr),
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 100 + i as u64,
                    logical: 0,
                    device_id: "d".into(),
                },
                left_at: None,
                enrolled_device_keys: keys,
                revoked_device_keys: BTreeSet::new(),
            },
        );
    }
    MaterializedCommunityState {
        members,
        channels: BTreeMap::new(),
        power_levels: BTreeMap::new(),
    }
}

/// A payload with a given snapshot; everything else fixed and minimal.
fn payload_with_snapshot(snapshot: MaterializedCommunityState) -> CommunityInvitePayload {
    let mut p = slim_open_invite(SpaceId([7u8; 16]), OwnerAddr([1u8; 16]), vec![0u8; 32]);
    p.epoch_snapshot.state_snapshot = snapshot;
    p
}

#[test]
fn slim_invite_fits_cap_while_old_full_roster_blows_it() {
    // Slim (empty snapshot): under Discord's 2000-char cap.
    let slim = encode_invite_url(&payload_with_snapshot(MaterializedCommunityState::default()))
        .expect("encode slim");
    assert!(
        slim.len() < 2000,
        "slim invite must fit the 2000-char cap: {} chars",
        slim.len()
    );

    // Old full-roster shape at N=500: exceeds the cap (the regression Phase 2 fixes).
    let full =
        encode_invite_url(&payload_with_snapshot(synthetic_roster(500))).expect("encode full");
    assert!(
        full.len() > 2000,
        "old full-roster N=500 should exceed the cap: {} chars",
        full.len()
    );

    // Slim size is content-independent (the roster is simply not present).
    let slim_again =
        encode_invite_url(&payload_with_snapshot(MaterializedCommunityState::default()))
            .expect("encode slim again");
    assert_eq!(
        slim.len(),
        slim_again.len(),
        "slim size does not depend on community size"
    );
}
