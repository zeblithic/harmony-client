//! Phase 4 integration test for `add_space` DM/GroupDm dispatch
//! (the new `add_space_dm_inner` pure function that the
//! `add_space` `#[tauri::command]` shim delegates to).
//!
//! These tests target the inner pure function rather than the
//! `#[tauri::command]` shim — same rationale as
//! `dm_thread_integration.rs`: a fully-populated `NodeState` is an
//! order of magnitude more setup than a Phase-4 IPC behavior test
//! warrants. The tauri::command wrapper is a thin adapter — its
//! contract is "snapshot handles under sync mutex, drop, call inner,
//! push UnicastSendRequests into the outbound channel".
//!
//! Behaviors covered:
//!   1. Happy path — Dm kind generates content_key, builds Space CRDT
//!      entry with sorted self+recipient members + transport=None (deposit-only),
//!      applies it to OwnerState via `apply_space_with_canonicalization`,
//!      and emits one signed `DmInvite` per known recipient device.
//!   2. GroupDm with 15 recipients (16 total members at cap) succeeds.
//!   3. Validation — DM with 0 recipients, DM with >1 recipients, and
//!      GroupDm with 16+ recipients (over cap) all surface as Err.

use std::sync::Arc;

use ed25519_dalek::SigningKey;

use harmony_app::add_space_dm_inner;
use harmony_app::dm_envelope::{decode_packet, DmPacket};
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_types::{
    DeviceIdentityHash, Hlc, OwnerAddr, OwnerDeviceEntry, SpaceKind,
};

/// Build a complete identity for a participant in the test:
/// `OwnerAddr` + `DeviceIdentityHash` + 64-byte combined `identity_pub`
/// + Ed25519 `SigningKey`. Mirrors `dm_unicast_integration::make_party`
///   so the test exercises production cryptographic shape.
fn make_party(seed_byte: u8) -> (OwnerAddr, DeviceIdentityHash, [u8; 64], Arc<SigningKey>) {
    let seed = [seed_byte; 32];
    let private = harmony_identity::PrivateIdentity::from_seed(&seed);
    let public = private.public_identity();
    let identity_pub = public.to_public_bytes();
    let device_hash = DeviceIdentityHash(public.address_hash);
    let owner = OwnerAddr([seed_byte ^ 0xff; 16]);
    let private_bytes = private.to_private_bytes();
    let ed25519_seed: [u8; 32] = private_bytes[32..64]
        .try_into()
        .expect("PRIVATE_KEY_LENGTH - 32 == 32");
    let signing_key = Arc::new(SigningKey::from_bytes(&ed25519_seed));
    (owner, device_hash, identity_pub, signing_key)
}

/// Pre-populate `state`'s `OwnerDeviceCache` with `(owner → [device])`
/// + cached identity_pub. Single-device entries are already
///   sorted/deduped by construction, so this bypasses the LWW HLC dance
///   in `apply_owner_device_update`.
fn cache_party(
    state: &mut OwnerState,
    owner: OwnerAddr,
    device: DeviceIdentityHash,
    identity_pub: [u8; 64],
    learned_at_ms: u64,
    learner_dev: &str,
) {
    state.owner_device_cache.devices.insert(
        owner,
        OwnerDeviceEntry {
            devices: vec![device],
            device_identity_pubs: vec![Some(identity_pub)],
            device_tunnel_contacts: vec![None],
            learned_at: Hlc {
                wall_ms: learned_at_ms,
                logical: 0,
                device_id: learner_dev.into(),
            },
        },
    );
}

#[tokio::test]
async fn add_space_dm_kind_generates_content_key_and_dispatches_invite() {
    let (alice_owner, alice_device, alice_identity_pub, alice_signing_key) = make_party(0xa1);
    let (bob_owner, bob_device, bob_identity_pub, _bob_signing_key) = make_party(0xb2);

    let mut state = OwnerState::default();
    // Cache Alice's own device + Bob's device. add_space_dm_inner reads
    // recipient_devices from owner_device_cache; with no entry for Bob
    // the invite would dispatch zero packets (best-effort — outbox loop
    // recovers on first send_dm). For this happy-path test we want to
    // assert ≥ 1 outbound invite, so seed Bob's cache.
    cache_party(
        &mut state,
        alice_owner,
        alice_device,
        alice_identity_pub,
        100,
        "alice",
    );
    cache_party(
        &mut state,
        bob_owner,
        bob_device,
        bob_identity_pub,
        100,
        "alice",
    );

    let (space_id, fanout, was_merge) = add_space_dm_inner(
        &mut state,
        &alice_signing_key,
        &alice_identity_pub,
        alice_owner,
        alice_device,
        "alice-device",
        SpaceKind::Dm,
        "DM with Bob".into(),
        vec![bob_owner],
        500, // wall_now_ms
        None,
    )
    .expect("add_space_dm_inner must succeed");
    assert!(!was_merge, "first creation must not be a merge");

    // Space is in CRDT.
    let space = state.spaces.get(&space_id).expect("space inserted");
    assert_eq!(space.kind, SpaceKind::Dm);
    assert_eq!(space.members.len(), 2, "self + bob");
    assert!(space.members.contains(&alice_owner));
    assert!(space.members.contains(&bob_owner));
    // members must be sorted-ascending per Space invariants.
    assert!(space.members.windows(2).all(|w| w[0] < w[1]));
    assert!(space.content_key.is_some(), "DM must have content_key");
    assert!(space.prior_content_keys.is_empty());
    // ZEB-474: DM spaces carry transport=None (deposit-only; Reticulum variant removed).
    assert!(
        space.transport.is_none(),
        "DM space must have transport=None"
    );
    assert_eq!(space.name, "DM with Bob");
    assert!(space.validate_invariants().is_ok(), "Space invariants hold");

    // ZEB-482: the invite fan-out carries the signed wire bytes + the
    // recipient OWNERS (the caller resolves each owner → tunnel device(s)
    // and routes over the iroh tunnel; addressing is no longer per-device
    // Reticulum destination hashes).
    let (invite_wire, recipients) = fanout.expect("a fresh create returns Some(fanout)");
    assert_eq!(recipients, vec![bob_owner], "one recipient owner: Bob");

    // Decode the packet and check the DmInvite shape.
    let decoded = decode_packet(&invite_wire).expect("packet decodes");
    match decoded {
        DmPacket::Invite { signed, .. } => {
            assert_eq!(signed.space_id, space_id);
            assert_eq!(signed.kind, SpaceKind::Dm);
            assert_eq!(signed.members, space.members);
            assert_eq!(signed.inviter, alice_owner);
            assert_eq!(signed.signing_device_hash, alice_device);
            assert_eq!(signed.inviter_identity_pub, alice_identity_pub);
            // sender_devices should contain at least our signing device.
            assert!(signed.sender_devices.contains(&alice_device));
        }
        _ => panic!("expected DmPacket::Invite"),
    }
}

#[tokio::test]
async fn add_space_group_dm_with_15_recipients_succeeds() {
    let (alice_owner, alice_device, alice_identity_pub, alice_signing_key) = make_party(0xa1);

    let mut state = OwnerState::default();
    cache_party(
        &mut state,
        alice_owner,
        alice_device,
        alice_identity_pub,
        100,
        "alice",
    );

    // 15 recipients with distinct OwnerAddrs (≠ alice). 15 + self = 16
    // total members, exactly at the cap.
    let recipients: Vec<OwnerAddr> = (0..15u8).map(|i| OwnerAddr([0x10 + i; 16])).collect();

    let (space_id, _fanout, _was_merge) = add_space_dm_inner(
        &mut state,
        &alice_signing_key,
        &alice_identity_pub,
        alice_owner,
        alice_device,
        "alice-device",
        SpaceKind::GroupDm,
        "Big group".into(),
        recipients,
        500,
        None,
    )
    .expect("16 total members at cap must succeed");

    let space = state.spaces.get(&space_id).expect("space inserted");
    assert_eq!(space.kind, SpaceKind::GroupDm);
    assert_eq!(space.members.len(), 16);
    assert!(space.validate_invariants().is_ok());
}

#[tokio::test]
async fn add_space_dm_kind_rejects_zero_recipients() {
    let (alice_owner, alice_device, alice_identity_pub, alice_signing_key) = make_party(0xa1);
    let mut state = OwnerState::default();
    let err = add_space_dm_inner(
        &mut state,
        &alice_signing_key,
        &alice_identity_pub,
        alice_owner,
        alice_device,
        "alice-device",
        SpaceKind::Dm,
        "empty".into(),
        vec![], // 0 recipients
        500,
        None,
    )
    .expect_err("0 recipients must err");
    assert!(
        err.contains("recipient") || err.contains("members"),
        "error mentions recipient/members count: {err}"
    );
}

#[tokio::test]
async fn add_space_group_dm_rejects_16_or_more_recipients() {
    let (alice_owner, alice_device, alice_identity_pub, alice_signing_key) = make_party(0xa1);
    let mut state = OwnerState::default();
    // 16 recipients + self = 17 total → over cap.
    let recipients: Vec<OwnerAddr> = (0..16u8).map(|i| OwnerAddr([0x10 + i; 16])).collect();

    let err = add_space_dm_inner(
        &mut state,
        &alice_signing_key,
        &alice_identity_pub,
        alice_owner,
        alice_device,
        "alice-device",
        SpaceKind::GroupDm,
        "too big".into(),
        recipients,
        500,
        None,
    )
    .expect_err("17 total members must err");
    assert!(
        err.contains("16") || err.contains("cap"),
        "error mentions cap: {err}"
    );
}

#[tokio::test]
async fn add_space_dm_kind_rejects_more_than_one_recipient() {
    let (alice_owner, alice_device, alice_identity_pub, alice_signing_key) = make_party(0xa1);
    let mut state = OwnerState::default();
    // DM (1-on-1) requires exactly one recipient. 2 → reject.
    let err = add_space_dm_inner(
        &mut state,
        &alice_signing_key,
        &alice_identity_pub,
        alice_owner,
        alice_device,
        "alice-device",
        SpaceKind::Dm,
        "ambiguous".into(),
        vec![OwnerAddr([0x10; 16]), OwnerAddr([0x11; 16])],
        500,
        None,
    )
    .expect_err("Dm with 2 recipients must err");
    assert!(
        err.contains("Dm") || err.contains("GroupDm") || err.contains("recipient"),
        "error points at GroupDm or recipient count: {err}"
    );
}

#[tokio::test]
async fn add_space_dm_kind_rejects_self_in_recipients() {
    let (alice_owner, alice_device, alice_identity_pub, alice_signing_key) = make_party(0xa1);
    let mut state = OwnerState::default();
    let err = add_space_dm_inner(
        &mut state,
        &alice_signing_key,
        &alice_identity_pub,
        alice_owner,
        alice_device,
        "alice-device",
        SpaceKind::Dm,
        "self-loop".into(),
        vec![alice_owner], // self in recipients
        500,
        None,
    )
    .expect_err("self in recipients must err");
    assert!(
        err.contains("duplicate") || err.contains("self") || err.contains("recipient"),
        "error mentions self/duplicate: {err}"
    );
}

#[tokio::test]
async fn add_space_dm_kind_idempotent_on_duplicate_creation() {
    // Phase 4 / PR #81 review fix: creating the same DM twice (same
    // sorted members) must dedupe via apply_space_with_canonicalization.
    // The second call must:
    //   - return the SAME canonical SpaceId as the first (the existing
    //     Space's id, not a fresh one — so the outer command's
    //     state.spaces.get(&canonical_id) succeeds);
    //   - signal `was_merge=true`;
    //   - return `fanout=None` (no additional DmInvite tunnel-send — the
    //     existing Space was already invited at original creation;
    //     re-firing here would just produce duplicate invite handling on
    //     the recipient).
    let (alice_owner, alice_device, alice_identity_pub, alice_signing_key) = make_party(0xa1);
    let (bob_owner, bob_device, bob_identity_pub, _bob_signing_key) = make_party(0xb2);

    let mut state = OwnerState::default();
    cache_party(
        &mut state,
        alice_owner,
        alice_device,
        alice_identity_pub,
        100,
        "alice",
    );
    cache_party(
        &mut state,
        bob_owner,
        bob_device,
        bob_identity_pub,
        100,
        "alice",
    );

    // First create — Inserted.
    let (first_id, first_fanout, first_merge) = add_space_dm_inner(
        &mut state,
        &alice_signing_key,
        &alice_identity_pub,
        alice_owner,
        alice_device,
        "alice-device",
        SpaceKind::Dm,
        "DM with Bob".into(),
        vec![bob_owner],
        500,
        None,
    )
    .expect("first create must succeed");
    assert!(!first_merge, "first create must not be a merge");
    let (_first_wire, first_recipients) = first_fanout.expect("first create returns Some(fanout)");
    assert_eq!(
        first_recipients,
        vec![bob_owner],
        "first create fans the invite out to Bob"
    );

    // Second create with the same members (different name to prove
    // dedupe is on members, not name). Must merge into a single
    // canonical SpaceId and skip dispatch. The winner is
    // lex-min(first_id, second_minted_id) per ULID tie-break — the
    // load-bearing assertion is "second_id IS the live entry in
    // state.spaces", not "second_id == first_id" (which only holds
    // when the second mint loses the tie-break).
    let (second_id, second_fanout, second_merge) = add_space_dm_inner(
        &mut state,
        &alice_signing_key,
        &alice_identity_pub,
        alice_owner,
        alice_device,
        "alice-device",
        SpaceKind::Dm,
        "DM with Bob (again)".into(),
        vec![bob_owner],
        600, // later wall_ms — irrelevant; dedupe is by sorted members
        None,
    )
    .expect("second create must succeed (dedupe path)");
    assert!(second_merge, "second create must be a merge (dedupe)");
    assert!(
        state.spaces.contains_key(&second_id),
        "second create returns a canonical SpaceId that's live in state.spaces \
         (NOT a stale loser id whose entry was dropped by the dedupe merge)"
    );
    assert!(
        second_fanout.is_none(),
        "second create must NOT re-dispatch DmInvite (existing was already invited)"
    );

    // Post-condition: state.spaces has exactly one DM Space for this
    // member set (no orphan loser entry from the merge). The winner
    // is whichever of (first_id, second-minted-id) is lex-smaller —
    // both calls converge on the same canonical id.
    let dm_spaces: Vec<_> = state
        .spaces
        .values()
        .filter(|s| matches!(s.kind, SpaceKind::Dm))
        .collect();
    assert_eq!(dm_spaces.len(), 1, "dedupe collapsed to a single DM Space");
    assert_eq!(
        dm_spaces[0].id, second_id,
        "the live DM Space's id is the canonical id returned by the second call"
    );
    // Either the first or the second call's returned id won the
    // tie-break; the loser's id should NOT be in state.spaces.
    if first_id != second_id {
        assert!(
            !state.spaces.contains_key(&first_id),
            "the loser id (first_id) was dropped during dedupe merge"
        );
    }
}

#[tokio::test]
async fn add_space_group_dm_rejects_duplicate_recipients() {
    let (alice_owner, alice_device, alice_identity_pub, alice_signing_key) = make_party(0xa1);
    let mut state = OwnerState::default();
    let bob = OwnerAddr([0x10; 16]);
    let carol = OwnerAddr([0x11; 16]);
    let err = add_space_dm_inner(
        &mut state,
        &alice_signing_key,
        &alice_identity_pub,
        alice_owner,
        alice_device,
        "alice-device",
        SpaceKind::GroupDm,
        "dup".into(),
        vec![bob, carol, bob], // duplicate
        500,
        None,
    )
    .expect_err("duplicate recipient must err");
    assert!(
        err.contains("duplicate") || err.contains("recipient"),
        "error mentions duplicate: {err}"
    );
}
