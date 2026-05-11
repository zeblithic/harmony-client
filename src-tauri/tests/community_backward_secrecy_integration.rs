//! ZEB-249 end-to-end integration tests for community backward secrecy.
//!
//! These are crypto-level integration tests — they exercise the key-bootstrap
//! flow directly (EpochKey derivation, seal/open, encrypt/decrypt) without
//! spinning up a full IPC harness or Zenoh session. The full IPC path is
//! covered by `community_open_flow_integration.rs` and
//! `community_invite_only_integration.rs`.

#![cfg(feature = "test-fixtures")]

use harmony_app::community_invite::{InviteEpochSnapshot, MaterializedCommunityState};
use harmony_app::community_state_sync::{decrypt_for_topic, encrypt_for_topic, EpochError};
use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, Space, SpaceId, SpaceKind};

fn make_space_with_epoch(
    community_id: SpaceId,
    admin_addr: OwnerAddr,
    epoch: u64,
    epoch_key: EpochKey,
) -> Space {
    let hlc = Hlc {
        wall_ms: 1_700_000_000_000,
        logical: 0,
        device_id: "test-dev".into(),
    };
    Space {
        id: community_id,
        kind: SpaceKind::Community,
        parent: None,
        community_id: None,
        name: "TestCommunity".into(),
        transport: None,
        members: Vec::new(),
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: hlc.clone(),
        updated_at: hlc,
        content_key: None,
        prior_content_keys: Vec::new(),
        current_epoch: Some(epoch),
        current_epoch_key: Some(epoch_key),
        old_epoch_keys: std::collections::BTreeMap::new(),
        admin_addr: Some(admin_addr),
        is_invite_only: Some(false),
    }
}

/// ZEB-249 §5.1 happy path: admin generates an open-community invite at epoch 0,
/// Dave receives it, extracts the EpochKey from `sealed_epoch_key` (raw 32 bytes
/// for open communities), builds a local Space, and successfully decrypts an event
/// encrypted by admin under K(0).
#[test]
fn invite_bootstrap_at_current_epoch_decrypts_new_events() {
    // 1. Admin generates a fresh EpochKey for epoch 0.
    let community_id = SpaceId([0x11; 16]);
    let admin_identity = harmony_identity::PrivateIdentity::from_seed(&[0xAA; 32]);
    let admin_addr = OwnerAddr(admin_identity.identity.address_hash);
    let epoch_key_bytes = [0x42u8; 32];
    let epoch_key = EpochKey::new(epoch_key_bytes);

    // Build admin's Space at epoch 0.
    let admin_space = make_space_with_epoch(community_id, admin_addr, 0, epoch_key.clone());

    // 2. Admin builds an InviteEpochSnapshot for Dave (open community = raw 32-byte key).
    let snapshot = InviteEpochSnapshot {
        epoch: 0,
        sealed_epoch_key: epoch_key.as_bytes().to_vec(),
        state_snapshot: MaterializedCommunityState::default(),
    };

    // 3. Dave receives the snapshot and reconstructs his local Space.
    let sealed_key_bytes = &snapshot.sealed_epoch_key;
    assert_eq!(
        sealed_key_bytes.len(),
        32,
        "open community snapshot: 32 raw bytes"
    );
    let dave_epoch_key_bytes: [u8; 32] = sealed_key_bytes
        .as_slice()
        .try_into()
        .expect("must be 32 bytes for open community");
    let dave_epoch_key = EpochKey::new(dave_epoch_key_bytes);
    assert_eq!(snapshot.epoch, 0, "snapshot epoch must be 0");

    let _dave_addr = OwnerAddr([0xBB; 16]);
    let dave_space =
        make_space_with_epoch(community_id, admin_addr, snapshot.epoch, dave_epoch_key);

    // 4. Admin posts an event encrypted under K(0).
    let plaintext = b"hello from admin at epoch 0";
    let envelope = encrypt_for_topic(&admin_space, plaintext).expect("admin encrypt");
    assert_eq!(
        envelope.epoch, 0,
        "envelope epoch must match admin's current epoch"
    );

    // 5. Dave decrypts successfully using his bootstrapped Space.
    let decrypted = decrypt_for_topic(&dave_space, &envelope).expect("dave decrypt");
    assert_eq!(
        decrypted.as_slice(),
        plaintext,
        "Dave must recover admin's plaintext"
    );

    // Sanity: using the wrong key fails.
    let wrong_key = EpochKey::new([0xFF; 32]);
    let wrong_space = make_space_with_epoch(community_id, admin_addr, 0, wrong_key);
    let err =
        decrypt_for_topic(&wrong_space, &envelope).expect_err("wrong key must fail AEAD tag check");
    assert!(
        matches!(err, EpochError::DecryptionFailed(_)),
        "expected DecryptionFailed, got {err:?}"
    );
}

/// ZEB-249 §4.3 gap: an invite issued at epoch=0 that the admin hasn't upgraded
/// to epoch=1 yet cannot decrypt events encrypted under epoch=1. This test verifies
/// the gap exists — catchup synthesis (Task 6) is not tested here.
#[test]
fn stale_invite_unable_to_decrypt_new_events_without_catchup() {
    // 1. Admin creates community at epoch 0 with K(0).
    let community_id = SpaceId([0x22; 16]);
    let admin_identity = harmony_identity::PrivateIdentity::from_seed(&[0xAA; 32]);
    let admin_addr = OwnerAddr(admin_identity.identity.address_hash);
    let k0_bytes = [0x10u8; 32];
    let k0 = EpochKey::new(k0_bytes);

    let admin_space_epoch0 = make_space_with_epoch(community_id, admin_addr, 0, k0.clone());

    // 2. Admin issues an invite for Dave at epoch=0 (snapshot contains K(0)).
    let dave_snapshot = InviteEpochSnapshot {
        epoch: 0,
        sealed_epoch_key: k0.as_bytes().to_vec(),
        state_snapshot: MaterializedCommunityState::default(),
    };

    // 3. Admin advances to epoch 1 with a fresh K(1) (e.g., after kicking someone).
    let k1_bytes = [0x20u8; 32];
    let k1 = EpochKey::new(k1_bytes);
    let mut admin_space_epoch1 = admin_space_epoch0.clone();
    admin_space_epoch1.old_epoch_keys.insert(0, k0.clone());
    admin_space_epoch1.current_epoch = Some(1);
    admin_space_epoch1.current_epoch_key = Some(k1.clone());

    // 4. Dave redeems the invite (gets K(0) at epoch=0, before the rotation happened).
    let dave_k0 = EpochKey::new(
        dave_snapshot
            .sealed_epoch_key
            .as_slice()
            .try_into()
            .expect("32 bytes"),
    );
    let _dave_addr = OwnerAddr([0xCC; 16]);
    let dave_space = make_space_with_epoch(community_id, admin_addr, dave_snapshot.epoch, dave_k0);

    // Dave can still decrypt events from epoch=0 (before the rotation).
    let old_plaintext = b"message from admin at epoch 0";
    let old_envelope = encrypt_for_topic(&admin_space_epoch0, old_plaintext).expect("encrypt@0");
    let decrypted_old = decrypt_for_topic(&dave_space, &old_envelope).expect("dave can decrypt @0");
    assert_eq!(decrypted_old.as_slice(), old_plaintext);

    // 5. Admin posts a NEW event at epoch=1.
    let new_plaintext = b"new message from admin at epoch 1";
    let new_envelope = encrypt_for_topic(&admin_space_epoch1, new_plaintext).expect("encrypt@1");
    assert_eq!(new_envelope.epoch, 1, "new envelope must be epoch 1");

    // 6. Dave attempts to decrypt epoch=1 event → KeyNotAvailable(1).
    // Dave's space only knows K(0) at epoch=0 — he has no K(1).
    let err = decrypt_for_topic(&dave_space, &new_envelope)
        .expect_err("Dave must fail to decrypt epoch=1 without catchup");
    assert!(
        matches!(err, EpochError::KeyNotAvailable(1)),
        "expected KeyNotAvailable(1) (catchup synthesis is Task 6), got {err:?}"
    );

    // 7. (Catchup synthesis is Task 6 — this test only verifies the gap exists.)
    // The admin's K(1) would need to be delivered to Dave via an EpochCatchup event.
}
