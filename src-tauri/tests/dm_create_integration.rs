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
//!      entry with sorted self+recipient members + Reticulum transport,
//!      applies it to OwnerState via `apply_space_with_canonicalization`,
//!      and emits one signed `DmInvite` per known recipient device.
//!   2. GroupDm with 15 recipients (16 total members at cap) succeeds.
//!   3. Validation — DM with 0 recipients, DM with >1 recipients, and
//!      GroupDm with 16+ recipients (over cap) all surface as Err.

use std::sync::Arc;

use ed25519_dalek::SigningKey;

use harmony_app::add_space_dm_inner;
use harmony_app::dm_envelope::{decode_packet, DmPacket};
use harmony_app::dm_signing;
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_types::{
    DeviceIdentityHash, Hlc, OwnerAddr, OwnerDeviceEntry, SpaceKind, TransportBinding,
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

    let (space_id, sends) = add_space_dm_inner(
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
    assert!(matches!(
        space.transport,
        Some(TransportBinding::Reticulum { .. })
    ));
    assert_eq!(space.name, "DM with Bob");
    assert!(space.validate_invariants().is_ok(), "Space invariants hold");

    // One DmInvite was dispatched to Bob's known device.
    assert_eq!(sends.len(), 1, "one recipient device → one outbound");
    let send = &sends[0];
    assert_eq!(
        send.destination_hash,
        dm_signing::compute_dm_destination_hash(bob_device.0),
        "dispatched to Bob's DM destination"
    );

    // Decode the packet and check the DmInvite shape.
    let decoded = decode_packet(&send.packet).expect("packet decodes");
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

    let (space_id, _sends) = add_space_dm_inner(
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
