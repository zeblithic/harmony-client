//! ZEB-249 end-to-end integration tests for community backward secrecy.
//!
//! These are crypto-level integration tests — they exercise the key-bootstrap
//! flow directly (EpochKey derivation, seal/open, encrypt/decrypt) without
//! spinning up a full IPC harness or Zenoh session. The full IPC path is
//! covered by `community_open_flow_integration.rs` and
//! `community_invite_only_integration.rs`.

#![cfg(feature = "test-fixtures")]

use harmony_app::community_invite::{InviteEpochSnapshot, MaterializedCommunityState};
use harmony_app::community_membership::{
    materialize, sign_event, EventPayload, MembershipEventKind, RecipientCiphertext,
    SignedMembershipEvent,
};
use harmony_app::community_state_sync::{decrypt_for_topic, encrypt_for_topic, EpochError};
use harmony_app::dm_signing::{
    ed25519_priv_to_x25519, ed25519_pub_to_x25519, open_from_owner, seal_to_owner,
};
use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, Space, SpaceId, SpaceKind};
use harmony_app::SynthCatchupsSet;

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
        shared_in_profile: false,
        read_receipt_pref: None,
        pending_join_at: None,
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
        sealed_epoch_keys: Vec::new(),
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
        sealed_epoch_keys: Vec::new(),
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

// ── ZEB-249 Task 6 integration tests ─────────────────────────────────────────
//
// These tests exercise the full epoch-rotation protocol:
// seal/open of epoch keys, Space state advancement, and the
// materialize() pending_rotation_for / pending_catchup_for logic.
// Each test uses in-process "nodes" (Space + signing identity).

/// Helper: make a `SignedMembershipEvent` with a pre-filled sig for materialize.
/// Uses sign_event with a real key to get a valid signature.
fn make_signed_event(
    id_byte: u8,
    community_id: SpaceId,
    actor: OwnerAddr,
    kind: MembershipEventKind,
    wall_ms: u64,
    signing_key: &ed25519_dalek::SigningKey,
) -> SignedMembershipEvent {
    let mut id = [0u8; 16];
    id[15] = id_byte;
    let payload = EventPayload {
        id,
        community_id,
        kind,
        actor,
        at: Hlc {
            wall_ms,
            logical: 0,
            device_id: "test".into(),
        },
    };
    sign_event(&payload, signing_key).expect("sign event")
}

/// Apply an EpochRotation from an event's sealed ciphertexts to a recipient's Space.
/// The recipient decrypts their sealed K_next using their Ed25519 private key → X25519.
///
/// E5 (ZEB-249 §10.6 R3): validates that `space.current_epoch` matches the
/// event's `prior_epoch` before applying. Returns `false` for stale or
/// out-of-order rotations — mirrors the idempotency guard in
/// `apply_remote_epoch_event`.
fn apply_rotation_to_space(
    space: &mut Space,
    rotation_event: &SignedMembershipEvent,
    my_addr: OwnerAddr,
    my_signing_key: &ed25519_dalek::SigningKey,
) -> bool {
    let (prior_epoch, recipient_ciphertexts) = match &rotation_event.kind {
        MembershipEventKind::EpochRotation {
            prior_epoch,
            recipient_ciphertexts,
            ..
        } => (*prior_epoch, recipient_ciphertexts),
        _ => return false,
    };
    // E5: guard — only apply if space epoch matches prior_epoch.
    let current = space.current_epoch.unwrap_or(0);
    if current != prior_epoch {
        return false;
    }
    let my_entry = recipient_ciphertexts
        .iter()
        .find(|rc| rc.recipient == my_addr);
    let sealed = match my_entry {
        Some(rc) => &rc.sealed,
        None => return false, // not in recipient list (kicked / leaver)
    };
    let x25519_priv = ed25519_priv_to_x25519(my_signing_key);
    let k_next_bytes_vec = open_from_owner(&x25519_priv, sealed).expect("open sealed key");
    let k_next_bytes: [u8; 32] = k_next_bytes_vec
        .try_into()
        .expect("sealed key must be 32 bytes");
    let k_next = EpochKey::new(k_next_bytes);
    let prev_key = space.current_epoch_key.clone();
    space.current_epoch = Some(prior_epoch + 1);
    space.current_epoch_key = Some(k_next);
    if let Some(pk) = prev_key {
        space.old_epoch_keys.insert(prior_epoch, pk);
    }
    true
}

/// Apply an EpochCatchup (same shape as rotation, but targeted at joiners).
///
/// CR Major (PR #106 R6): before replacing the current key, archive the
/// previous (epoch, key) into `old_epoch_keys` if advancing. This preserves
/// the backward-access invariant — a member that held K(N) as their current
/// key can still decrypt epoch-N content after receiving a catchup to K(M>N).
/// Stale catchups (incoming epoch < current) are ignored.
fn apply_catchup_to_space(
    space: &mut Space,
    catchup_event: &SignedMembershipEvent,
    my_addr: OwnerAddr,
    my_signing_key: &ed25519_dalek::SigningKey,
) -> bool {
    let (epoch, recipient_ciphertexts) = match &catchup_event.kind {
        MembershipEventKind::EpochCatchup {
            epoch,
            recipient_ciphertexts,
            ..
        } => (*epoch, recipient_ciphertexts),
        _ => return false,
    };
    let my_entry = recipient_ciphertexts
        .iter()
        .find(|rc| rc.recipient == my_addr);
    let sealed = match my_entry {
        Some(rc) => &rc.sealed,
        None => return false,
    };
    let x25519_priv = ed25519_priv_to_x25519(my_signing_key);
    let k_bytes_vec = open_from_owner(&x25519_priv, sealed).expect("open sealed key");
    let k_bytes: [u8; 32] = k_bytes_vec.try_into().expect("sealed key must be 32 bytes");
    let k = EpochKey::new(k_bytes);

    // Ignore stale catchups (incoming epoch < current).
    if matches!(space.current_epoch, Some(current) if epoch < current) {
        return false;
    }
    // Archive previous (epoch, key) when advancing.
    if let (Some(prev_epoch), Some(prev_key)) =
        (space.current_epoch, space.current_epoch_key.clone())
    {
        if epoch > prev_epoch {
            space.old_epoch_keys.insert(prev_epoch, prev_key);
        }
    }
    // Catchup delivers the CURRENT epoch key (no increment).
    space.current_epoch = Some(epoch);
    space.current_epoch_key = Some(k);
    true
}

/// Seal K_next to a set of recipients given their Ed25519 signing keys.
/// Returns a Vec<(OwnerAddr, sealed_bytes)> ready for RecipientCiphertext.
fn seal_epoch_to_members(
    k_next: &EpochKey,
    recipients: &[(&OwnerAddr, &ed25519_dalek::SigningKey)],
) -> Vec<(OwnerAddr, Vec<u8>)> {
    recipients
        .iter()
        .map(|(addr, sk)| {
            let pub32 = sk.verifying_key().to_bytes(); // 32-byte Ed25519 pubkey
            let x25519_pub = ed25519_pub_to_x25519(&pub32).expect("ed25519_pub_to_x25519");
            let sealed = seal_to_owner(&x25519_pub, k_next.as_bytes()).expect("seal_to_owner");
            (**addr, sealed)
        })
        .collect()
}

/// ZEB-249 §4.1: Admin A kicks member B; B cannot decrypt events encrypted
/// under K(1) (the new epoch key). A and remaining member C can decrypt.
#[test]
fn two_node_kick_then_cannot_decrypt() {
    use harmony_identity::PrivateIdentity;

    let community_id = SpaceId([0x30; 16]);
    let admin_identity = PrivateIdentity::from_seed(&[0xAA; 32]);
    let admin_sk_bytes = admin_identity.to_private_bytes();
    let admin_ed_seed: [u8; 32] = admin_sk_bytes[32..64].try_into().unwrap();
    let admin_signing_key = ed25519_dalek::SigningKey::from_bytes(&admin_ed_seed);
    let admin_addr = OwnerAddr(admin_identity.identity.address_hash);

    let b_identity = PrivateIdentity::from_seed(&[0xBB; 32]);
    let b_sk_bytes = b_identity.to_private_bytes();
    let b_ed_seed: [u8; 32] = b_sk_bytes[32..64].try_into().unwrap();
    let b_signing_key = ed25519_dalek::SigningKey::from_bytes(&b_ed_seed);
    let b_addr = OwnerAddr(b_identity.identity.address_hash);

    // Epoch 0: both admin and B share K(0).
    let k0 = EpochKey::new([0x10u8; 32]);
    let mut admin_space = make_space_with_epoch(community_id, admin_addr, 0, k0.clone());
    let b_space = make_space_with_epoch(community_id, admin_addr, 0, k0.clone());

    // Admin encrypts a message under K(0) — both can decrypt.
    let msg_epoch0 = b"hello at epoch 0";
    let env0 = encrypt_for_topic(&admin_space, msg_epoch0).expect("encrypt@0");
    decrypt_for_topic(&b_space, &env0).expect("B decrypts epoch-0 msg");

    // Admin kicks B — build Kick + EpochRotation.
    let kick_event = make_signed_event(
        0x01,
        community_id,
        admin_addr,
        MembershipEventKind::Kick {
            target: b_addr,
            reason: None,
        },
        1000,
        &admin_signing_key,
    );

    // Generate K(1); seal only to admin (B excluded per spec §4.1).
    let k_next = EpochKey::new([0x20u8; 32]);
    let rotation_recipients = seal_epoch_to_members(&k_next, &[(&admin_addr, &admin_signing_key)]);
    let rotation_rcs: Vec<RecipientCiphertext> = rotation_recipients
        .into_iter()
        .map(|(addr, sealed)| RecipientCiphertext {
            recipient: addr,
            sealed,
        })
        .collect();

    let rotation_event = make_signed_event(
        0x02,
        community_id,
        admin_addr,
        MembershipEventKind::EpochRotation {
            prior_epoch: 0,
            triggered_by: kick_event.id,
            recipient_ciphertexts: rotation_rcs,
        },
        1001,
        &admin_signing_key,
    );

    // Admin applies the rotation to their own Space.
    apply_rotation_to_space(
        &mut admin_space,
        &rotation_event,
        admin_addr,
        &admin_signing_key,
    );
    assert_eq!(
        admin_space.current_epoch,
        Some(1),
        "admin must advance to epoch 1"
    );

    // Admin encrypts a new message under K(1).
    let msg_epoch1 = b"secret at epoch 1 (B excluded)";
    let env1 = encrypt_for_topic(&admin_space, msg_epoch1).expect("encrypt@1");
    assert_eq!(env1.epoch, 1);

    // B still has only K(0); epoch=1 message fails.
    let err = decrypt_for_topic(&b_space, &env1)
        .expect_err("B must not decrypt epoch-1 message after kick");
    assert!(
        matches!(err, EpochError::KeyNotAvailable(1)),
        "expected KeyNotAvailable(1), got {err:?}"
    );

    // B CAN still decrypt epoch=0 messages (backward: old messages preserved).
    decrypt_for_topic(&b_space, &env0).expect("B can still read old epoch-0 msg");

    // Sanity: applying the rotation for B (simulating getting the wrong sealed key
    // from a different member's sealed bytes) fails open_from_owner.
    let admin_pub64 = admin_signing_key.verifying_key().to_bytes();
    let x25519_admin_pub = ed25519_pub_to_x25519(&admin_pub64).expect("convert pub");
    let sealed_for_admin =
        seal_to_owner(&x25519_admin_pub, k_next.as_bytes()).expect("seal to admin");
    let b_x25519_priv = ed25519_priv_to_x25519(&b_signing_key);
    let result = open_from_owner(&b_x25519_priv, &sealed_for_admin);
    // Should either fail or produce wrong bytes (wrong recipient key).
    // The AEAD tag will fail since the sealed envelope is for admin's X25519 key.
    assert!(
        result.is_err(),
        "B must not be able to open a sealed key intended for admin"
    );
}

/// ZEB-249 §4.1: Admin A invites B and C at epoch 0. A kicks B at epoch 1.
/// C (remaining member) gets the rotation and can decrypt epoch-1 messages.
/// B cannot.
#[test]
fn three_node_selective_access() {
    use harmony_identity::PrivateIdentity;

    let community_id = SpaceId([0x31; 16]);

    let admin_identity = PrivateIdentity::from_seed(&[0xAA; 32]);
    let admin_sk_bytes = admin_identity.to_private_bytes();
    let admin_ed_seed: [u8; 32] = admin_sk_bytes[32..64].try_into().unwrap();
    let admin_signing_key = ed25519_dalek::SigningKey::from_bytes(&admin_ed_seed);
    let admin_addr = OwnerAddr(admin_identity.identity.address_hash);

    let b_identity = PrivateIdentity::from_seed(&[0xBB; 32]);
    let b_sk_bytes = b_identity.to_private_bytes();
    let b_ed_seed: [u8; 32] = b_sk_bytes[32..64].try_into().unwrap();
    let _b_signing_key = ed25519_dalek::SigningKey::from_bytes(&b_ed_seed);
    let b_addr = OwnerAddr(b_identity.identity.address_hash);

    let c_identity = PrivateIdentity::from_seed(&[0xCC; 32]);
    let c_sk_bytes = c_identity.to_private_bytes();
    let c_ed_seed: [u8; 32] = c_sk_bytes[32..64].try_into().unwrap();
    let c_signing_key = ed25519_dalek::SigningKey::from_bytes(&c_ed_seed);
    let c_addr = OwnerAddr(c_identity.identity.address_hash);

    // All three start with K(0).
    let k0 = EpochKey::new([0x10u8; 32]);
    let mut admin_space = make_space_with_epoch(community_id, admin_addr, 0, k0.clone());
    let b_space = make_space_with_epoch(community_id, admin_addr, 0, k0.clone());
    let mut c_space = make_space_with_epoch(community_id, admin_addr, 0, k0.clone());

    // Admin kicks B → generate K(1) sealed to admin and C only.
    let kick_event = make_signed_event(
        0x01,
        community_id,
        admin_addr,
        MembershipEventKind::Kick {
            target: b_addr,
            reason: None,
        },
        1000,
        &admin_signing_key,
    );
    let k1 = EpochKey::new([0x20u8; 32]);
    let rotation_recipients = seal_epoch_to_members(
        &k1,
        &[(&admin_addr, &admin_signing_key), (&c_addr, &c_signing_key)],
    );
    let rotation_rcs: Vec<RecipientCiphertext> = rotation_recipients
        .into_iter()
        .map(|(addr, sealed)| RecipientCiphertext {
            recipient: addr,
            sealed,
        })
        .collect();
    let rotation_event = make_signed_event(
        0x02,
        community_id,
        admin_addr,
        MembershipEventKind::EpochRotation {
            prior_epoch: 0,
            triggered_by: kick_event.id,
            recipient_ciphertexts: rotation_rcs,
        },
        1001,
        &admin_signing_key,
    );

    // Apply to admin and C.
    apply_rotation_to_space(
        &mut admin_space,
        &rotation_event,
        admin_addr,
        &admin_signing_key,
    );
    apply_rotation_to_space(&mut c_space, &rotation_event, c_addr, &c_signing_key);
    assert_eq!(admin_space.current_epoch, Some(1));
    assert_eq!(c_space.current_epoch, Some(1));

    // Admin encrypts at epoch 1.
    let msg = b"members-only at epoch 1";
    let env1 = encrypt_for_topic(&admin_space, msg).expect("encrypt@1");

    // C decrypts successfully.
    let decrypted = decrypt_for_topic(&c_space, &env1).expect("C decrypts epoch-1");
    assert_eq!(decrypted.as_slice(), msg);

    // B cannot decrypt (no K(1)).
    assert!(
        matches!(
            decrypt_for_topic(&b_space, &env1),
            Err(EpochError::KeyNotAvailable(1))
        ),
        "B must not decrypt epoch-1"
    );

    // B and C can still read epoch-0 messages.
    let env0 = encrypt_for_topic(
        &make_space_with_epoch(community_id, admin_addr, 0, k0.clone()),
        b"old msg",
    )
    .expect("encrypt@0");
    decrypt_for_topic(&b_space, &env0).expect("B reads old epoch-0 msg");
    decrypt_for_topic(&c_space, &env0).expect("C reads old epoch-0 msg");
}

/// ZEB-249 §4.6: Member B is offline during three consecutive rotations
/// (epochs 0→1→2→3). When B comes back online, an EpochCatchup delivers
/// K(3) and B can decrypt current epoch-3 messages.
///
/// Note: this is a protocol-level test that exercises the event-building and
/// key-derivation logic directly with pre-baked events. It does NOT drive the
/// self-healing observer — it verifies that a correctly-formed EpochCatchup
/// delivers the right key, decoupled from the async observer machinery.
#[test]
fn offline_catchup_through_multiple_rotations() {
    use harmony_identity::PrivateIdentity;

    let community_id = SpaceId([0x32; 16]);
    let admin_identity = PrivateIdentity::from_seed(&[0xAA; 32]);
    let admin_sk_bytes = admin_identity.to_private_bytes();
    let admin_ed_seed: [u8; 32] = admin_sk_bytes[32..64].try_into().unwrap();
    let admin_signing_key = ed25519_dalek::SigningKey::from_bytes(&admin_ed_seed);
    let admin_addr = OwnerAddr(admin_identity.identity.address_hash);

    let b_identity = PrivateIdentity::from_seed(&[0xBB; 32]);
    let b_sk_bytes = b_identity.to_private_bytes();
    let b_ed_seed: [u8; 32] = b_sk_bytes[32..64].try_into().unwrap();
    let b_signing_key = ed25519_dalek::SigningKey::from_bytes(&b_ed_seed);
    let b_addr = OwnerAddr(b_identity.identity.address_hash);

    // B joins at epoch=0 and goes offline.
    let k0 = EpochKey::new([0x10u8; 32]);
    let mut admin_space = make_space_with_epoch(community_id, admin_addr, 0, k0.clone());
    let mut b_space = make_space_with_epoch(community_id, admin_addr, 0, k0.clone());

    // Simulate 3 rotations driven by kicking some other member (X) each time.
    // B is not kicked — just offline. We simulate the admin issuing 3 rotations
    // but NOT including B in the recipient list (simulating B being unreachable).
    // In reality, admin would include B. For this test we deliberately exclude B
    // to prove the catchup mechanism works.
    let _x_addr = OwnerAddr([0xFF; 16]);
    let mut kick_id = [0u8; 16];
    kick_id[0] = 0x01;
    let mut current_key = k0;

    for i in 0..3u64 {
        let k_next = EpochKey::random();
        // Admin applies rotation (B excluded to simulate offline).
        let sealed_for_admin = seal_epoch_to_members(&k_next, &[(&admin_addr, &admin_signing_key)]);
        let rotation_rcs: Vec<RecipientCiphertext> = sealed_for_admin
            .into_iter()
            .map(|(addr, sealed)| RecipientCiphertext {
                recipient: addr,
                sealed,
            })
            .collect();
        let rotation_event = SignedMembershipEvent {
            signer_certs: Vec::new(),
            id: {
                let mut id = [0u8; 16];
                id[0] = 0x10 + i as u8;
                id
            },
            community_id,
            kind: MembershipEventKind::EpochRotation {
                prior_epoch: i,
                triggered_by: kick_id,
                recipient_ciphertexts: rotation_rcs,
            },
            actor: admin_addr,
            at: Hlc {
                wall_ms: 1000 + i,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0u8; 64],
            countersig: None,
            enrollment: None,
        };
        // Just advance admin's space directly (we're not calling insert_local_event).
        let prev_epoch = admin_space.current_epoch.unwrap_or(0);
        let prev_key = admin_space.current_epoch_key.clone();
        admin_space.current_epoch = Some(prev_epoch + 1);
        admin_space.current_epoch_key = Some(k_next.clone());
        if let Some(pk) = prev_key {
            admin_space.old_epoch_keys.insert(prev_epoch, pk);
        }
        let _ = rotation_event; // suppress unused warning
        current_key = k_next;
    }

    // Admin is now at epoch=3. B is still at epoch=0.
    assert_eq!(admin_space.current_epoch, Some(3));
    assert_eq!(b_space.current_epoch, Some(0));

    // Admin encrypts at epoch 3 — B can't decrypt.
    let msg3 = b"epoch 3 secret";
    let env3 = encrypt_for_topic(&admin_space, msg3).expect("encrypt@3");
    assert!(
        matches!(
            decrypt_for_topic(&b_space, &env3),
            Err(EpochError::KeyNotAvailable(3))
        ),
        "B must fail without catchup"
    );

    // B comes back online. Admin issues an EpochCatchup delivering K(3) to B.
    // The "triggered_by" would reference B's Join event id in production.
    let b_join_id = [0xFE; 16];
    let sealed_for_b = seal_epoch_to_members(&current_key, &[(&b_addr, &b_signing_key)]);
    let catchup_rcs: Vec<RecipientCiphertext> = sealed_for_b
        .into_iter()
        .map(|(addr, sealed)| RecipientCiphertext {
            recipient: addr,
            sealed,
        })
        .collect();
    let catchup_event = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0xCA; 16],
        community_id,
        kind: MembershipEventKind::EpochCatchup {
            epoch: 3,
            triggered_by: b_join_id,
            recipient_ciphertexts: catchup_rcs,
        },
        actor: admin_addr,
        at: Hlc {
            wall_ms: 5000,
            logical: 0,
            device_id: "test".into(),
        },
        sig: [0u8; 64],
        countersig: None,
        enrollment: None,
    };

    // B applies the catchup.
    apply_catchup_to_space(&mut b_space, &catchup_event, b_addr, &b_signing_key);
    assert_eq!(
        b_space.current_epoch,
        Some(3),
        "B must be at epoch 3 after catchup"
    );

    // B can now decrypt epoch-3 messages.
    let decrypted = decrypt_for_topic(&b_space, &env3).expect("B decrypts after catchup");
    assert_eq!(decrypted.as_slice(), msg3);
}

/// ZEB-249 §6.5 #5: Stale-invite catchup positive path.
///
/// Proves the FULL self-healing recovery cycle for a member who joined with a
/// stale invite snapshot (epoch=0) after the community has advanced to epoch=1.
///
/// Steps:
///   1. Admin creates community at epoch 0 with K(0).
///   2. Admin issues an invite for Dave at epoch=0 (snapshot contains K(0)).
///   3. Admin kicks Bob (advances to epoch 1 with K(1) — Dave excluded from rotation).
///   4. Dave redeems the stale invite — his local Space is stuck at epoch=0.
///   5. Dave's Join event lands in the engine; pending_catchup_for gains Dave.
///   6. The observer is driven via `self_heal_community_observer` directly with a
///      crdt_state that has K(1) as the current epoch key. The observer must seal
///      K(1) (not K(0) / spawn-time key) to Dave.
///   7. Dave applies the observer-synthesized EpochCatchup — his Space advances to epoch=1.
///   8. Dave decrypts an epoch-1 message successfully.
///
/// This test drives the ACTUAL observer (not mint_epoch_catchup_event directly) to
/// prove the fix: the observer reads the current epoch key from crdt_state, not the
/// engine's spawn-time membership_key.
#[tokio::test]
async fn stale_invite_catchup_unlocks_decryption_end_to_end() {
    use harmony_app::community_state_sync::{
        CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver, DEFAULT_DEBOUNCE_MS,
    };
    use harmony_app::content_store::{ContentStore, RuntimeContentStore};
    use harmony_app::owner_state_crdt::OwnerState;
    use harmony_app::self_heal_community_observer;
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    let community_id = SpaceId([0x37; 16]);

    // ── Identities (ZEB-339 enrolled-device owners) ──────────────────────────
    // actor = owner_id; events are signed by the enrolled device key (#2). The
    // Join events carry each owner's Master cert so the engine learns their
    // enrolled device key (epoch seal/open use the SAME device key).
    //
    // The 64-byte identity_pub resolved by the IdentityResolver carries the
    // DEVICE ed25519 verifying key in bytes [32..64] (and its derived X25519 in
    // [0..32]) — the observer seals each EpochCatchup to the recipient's X25519
    // derived from this, and the recipient opens with their device signing key.
    let pub64_for = |sk: &ed25519_dalek::SigningKey| -> [u8; 64] {
        let ed = sk.verifying_key().to_bytes();
        let x = harmony_app::dm_signing::ed25519_pub_to_x25519(&ed).expect("ed→x25519");
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(&x);
        out[32..].copy_from_slice(&ed);
        out
    };

    let admin = harmony_app::community_membership::mint_test_owner(0xAA);
    let admin_addr = admin.owner;
    let admin_signing_key = Arc::new(admin.device_key.clone());
    let admin_pub64 = pub64_for(&admin_signing_key);

    let bob = harmony_app::community_membership::mint_test_owner(0xBB);
    let bob_addr = bob.owner;
    let bob_signing_key = bob.device_key.clone();
    let bob_pub64 = pub64_for(&bob_signing_key);

    let dave = harmony_app::community_membership::mint_test_owner(0xDD);
    let dave_addr = dave.owner;
    let dave_signing_key = dave.device_key.clone();
    let dave_pub64 = pub64_for(&dave_signing_key);

    // ── Step 1: Epoch keys ───────────────────────────────────────────────────
    // k0: spawn-time key (the engine is spawned with this key — the observer's
    //     spawn-time membership_key). k1: post-rotation key that must be
    //     delivered to Dave via the catchup.
    let k0 = EpochKey::new([0x10u8; 32]);
    let k1 = EpochKey::new([0x20u8; 32]);

    // ── Step 2: Identity resolver knows all three members ────────────────────
    struct StaticResolver(std::collections::HashMap<OwnerAddr, [u8; 64]>);
    #[async_trait::async_trait]
    impl IdentityResolver for StaticResolver {
        async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
            self.0.get(addr).copied()
        }
    }
    let mut pub_map = std::collections::HashMap::new();
    pub_map.insert(admin_addr, admin_pub64);
    pub_map.insert(bob_addr, bob_pub64);
    pub_map.insert(dave_addr, dave_pub64);
    let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver(pub_map));

    // ── Step 3: Registry + engine (spawned with k0 as membership_key) ────────
    // CAS servicer: processes PutLocal / GetOrFetch ops so publish_root_now
    // (triggered by notify_dirty after each insert) doesn't deadlock waiting
    // for a reply that nobody sends.
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel::<harmony_app::content_store::CasOp>(64);
    tokio::spawn(async move {
        use harmony_app::content_store::CasOp;
        let mut store: std::collections::HashMap<harmony_content::cid::ContentId, Vec<u8>> =
            std::collections::HashMap::new();
        while let Some(op) = cas_op_rx.recv().await {
            match op {
                CasOp::PutLocal {
                    cid, blob, reply, ..
                } => {
                    store.insert(cid, blob);
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(()));
                    }
                }
                CasOp::GetOrFetch { cid, reply, .. } => {
                    let v = store.get(&cid).cloned();
                    let _ = reply.send(Ok(v));
                }
                CasOp::GetLocal { cid, reply } => {
                    let v = store.get(&cid).cloned();
                    let _ = reply.send(v);
                }
                CasOp::AllowServeSubtree { reply, .. } => {
                    let _ = reply.send(Ok(0));
                }
            }
        }
    });
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));
    let dir = tempfile::tempdir().expect("tempdir");
    // ZEB-790: one adoption floor per simulated node — this test models a
    // single node ("admin-dev"), so the registry and the self_heal observer
    // that mint/feed HLC share ONE floor.
    let adopt_floor = harmony_app::hlc_adopt_floor::HlcAdoptFloor::new();
    let registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: adopt_floor.clone(),
        device_id: "admin-dev".into(),
        content_store: cs,
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: admin_addr,
        signing_key: Arc::clone(&admin_signing_key),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    }));

    let (pub_tx, _pub_rx) = mpsc::channel(8);
    let (_sub_tx, sub_rx) = mpsc::channel(8);
    registry
        .spawn_engine_inner_now(
            community_id,
            k0.clone(), // spawn-time key: intentionally k0 (not k1)
            admin_addr,
            false,
            pub_tx,
            sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("engine spawn");
    let engine = registry
        .engine_arc(&community_id)
        .await
        .expect("engine arc");

    // ── Step 4: Populate engine with all events ───────────────────────────────
    // All events are inserted via insert_local_event_with_pubs (bypasses
    // resolver — resolution is done by the caller and passed directly).
    //
    // Event timeline:
    //   hlc(100)  admin Join
    //   hlc(200)  bob Join
    //   hlc(1000) admin kicks bob
    //   hlc(1001) admin EpochRotation (k0→k1, sealed to admin only — dave excluded)
    //   hlc(2000) dave Join (late — after rotation → pending_catchup_for)

    let join_admin = SignedMembershipEvent {
        enrollment: Some(admin.cert.clone()),
        ..make_signed_event(
            0x01,
            community_id,
            admin_addr,
            MembershipEventKind::Join,
            100,
            &admin_signing_key,
        )
    };
    engine
        .insert_local_event_with_pubs(join_admin.clone(), admin_pub64, None)
        .await
        .expect("insert admin join");

    let join_bob = SignedMembershipEvent {
        enrollment: Some(bob.cert.clone()),
        ..make_signed_event(
            0x02,
            community_id,
            bob_addr,
            MembershipEventKind::Join,
            200,
            &bob_signing_key,
        )
    };
    engine
        .insert_local_event_with_pubs(join_bob.clone(), bob_pub64, None)
        .await
        .expect("insert bob join");

    let kick_bob = make_signed_event(
        0x10,
        community_id,
        admin_addr,
        MembershipEventKind::Kick {
            target: bob_addr,
            reason: None,
        },
        1000,
        &admin_signing_key,
    );
    engine
        .insert_local_event_with_pubs(kick_bob.clone(), admin_pub64, None)
        .await
        .expect("insert kick_bob");

    // Rotation: admin only (dave excluded — simulates stale-invite scenario where
    // dave's pub wasn't known at rotation time). This puts dave into
    // pending_catchup_for once his Join lands.
    let sealed_for_admin = seal_epoch_to_members(&k1, &[(&admin_addr, &admin_signing_key)]);
    let rotation_rcs: Vec<RecipientCiphertext> = sealed_for_admin
        .into_iter()
        .map(|(addr, sealed)| RecipientCiphertext {
            recipient: addr,
            sealed,
        })
        .collect();
    let rotation_event = make_signed_event(
        0x11,
        community_id,
        admin_addr,
        MembershipEventKind::EpochRotation {
            prior_epoch: 0,
            triggered_by: kick_bob.id,
            recipient_ciphertexts: rotation_rcs,
        },
        1001,
        &admin_signing_key,
    );
    engine
        .insert_local_event_with_pubs(rotation_event.clone(), admin_pub64, None)
        .await
        .expect("insert rotation");

    // Dave's late Join (after the rotation → pending_catchup_for in materialize).
    let join_dave_late = SignedMembershipEvent {
        enrollment: Some(dave.cert.clone()),
        ..make_signed_event(
            0x04,
            community_id,
            dave_addr,
            MembershipEventKind::Join,
            2000,
            &dave_signing_key,
        )
    };
    engine
        .insert_local_event_with_pubs(join_dave_late.clone(), dave_pub64, None)
        .await
        .expect("insert dave join");

    // ── Step 5: crdt_state has K(1) for this community ───────────────────────
    // This is what the observer must use for the catchup — NOT k0 (spawn-time).
    let admin_space_k1 = make_space_with_epoch(community_id, admin_addr, 1, k1.clone());
    let mut owner_state = OwnerState::default();
    owner_state.apply_space_with_canonicalization(admin_space_k1);
    let crdt_state = Arc::new(tokio::sync::Mutex::new(owner_state));

    // Verify the accessor returns k1.
    {
        let g = crdt_state.lock().await;
        assert_eq!(
            g.current_epoch_key_for(community_id)
                .as_ref()
                .map(|k| k.as_bytes()),
            Some(k1.as_bytes()),
            "crdt_state must have K(1) for the community"
        );
    }

    // ── Step 6: Call the observer directly ───────────────────────────────────
    // The observer's catchup synthesis must use crdt_state's K(1), not the
    // engine's spawn-time K(0). This is the core assertion of this test.
    let hlc_tracker: Arc<tokio::sync::Mutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>> =
        Arc::new(tokio::sync::Mutex::new(
            harmony_crdt_sync::ReplayTracker::new("admin-dev".into()),
        ));
    let synth_rotations: Arc<
        Mutex<
            BTreeSet<(
                SpaceId,
                OwnerAddr,
                harmony_app::community_membership::EventId,
            )>,
        >,
    > = Arc::new(Mutex::new(BTreeSet::new()));
    let synth_catchups: SynthCatchupsSet = Arc::new(Mutex::new(BTreeSet::new()));

    self_heal_community_observer(
        community_id,
        Arc::clone(&registry),
        Arc::clone(&admin_signing_key),
        Arc::clone(&hlc_tracker),
        adopt_floor.clone(),
        "admin-dev".into(),
        admin_addr,
        Arc::clone(&crdt_state),
        Arc::clone(&synth_rotations),
        Arc::clone(&synth_catchups),
    )
    .await;

    // ── Step 7: Verify the observer synthesized an EpochCatchup for Dave ─────
    // The catchup must be in the engine's event log.
    let events: Vec<_> = {
        let state = engine.state();
        let g = state.lock().await;
        g.events().cloned().collect()
    };
    let catchup_event = events
        .iter()
        .find(|e| {
            matches!(
                &e.kind,
                MembershipEventKind::EpochCatchup {
                    recipient_ciphertexts,
                    ..
                } if recipient_ciphertexts.iter().any(|rc| rc.recipient == dave_addr)
            )
        })
        .cloned()
        .expect("observer must have synthesized an EpochCatchup for Dave");

    // The catchup must seal K(1) (not K(0)) to Dave.
    // We verify by applying the catchup to Dave's Space and checking the resulting key.
    let mut dave_space = make_space_with_epoch(community_id, admin_addr, 0, k0.clone());
    let applied = apply_catchup_to_space(
        &mut dave_space,
        &catchup_event,
        dave_addr,
        &dave_signing_key,
    );
    assert!(
        applied,
        "Dave must be able to apply the observer's catchup event"
    );
    assert_eq!(
        dave_space.current_epoch,
        Some(1),
        "Dave's space must advance to epoch=1 after catchup"
    );
    assert!(
        dave_space
            .current_epoch_key
            .as_ref()
            .map(|k| k.as_bytes() == k1.as_bytes())
            .unwrap_or(false),
        "Dave's current_epoch_key must be K(1) after catchup — observer used crdt_state key, not spawn-time key"
    );
    // CR Major (PR #106 R6): apply_catchup_to_space now archives the
    // previous (epoch, key) when advancing. Dave's space was initialized
    // at epoch=0 with K(0) before the catchup, so K(0) must be archived
    // in old_epoch_keys — the backward-access invariant requires it.
    assert_eq!(
        dave_space.old_epoch_keys.len(),
        1,
        "EpochCatchup advancing epoch must archive the prior key"
    );
    assert!(
        dave_space
            .old_epoch_keys
            .get(&0)
            .map(|k| k.as_bytes() == k0.as_bytes())
            .unwrap_or(false),
        "old_epoch_keys must contain K(0) at epoch 0 after catchup advances to epoch 1"
    );

    // ── Step 8: Dave decrypts epoch-1 messages ───────────────────────────────
    let admin_space_k1_for_enc = make_space_with_epoch(community_id, admin_addr, 1, k1.clone());
    let msg1 = b"epoch 1 secret - Dave has not got catchup yet";
    let env1 = encrypt_for_topic(&admin_space_k1_for_enc, msg1).expect("encrypt@1");
    assert_eq!(env1.epoch, 1, "envelope is epoch-1");

    // Dave's space (pre-catchup at k0) cannot decrypt epoch-1.
    let dave_space_k0 = make_space_with_epoch(community_id, admin_addr, 0, k0.clone());
    assert!(
        matches!(
            decrypt_for_topic(&dave_space_k0, &env1),
            Err(EpochError::KeyNotAvailable(1))
        ),
        "Dave must fail to decrypt epoch-1 before catchup"
    );

    // Dave's space (post-catchup at k1) can decrypt epoch-1.
    let decrypted1 =
        decrypt_for_topic(&dave_space, &env1).expect("Dave decrypts epoch-1 after catchup");
    assert_eq!(
        decrypted1.as_slice(),
        msg1,
        "Dave can decrypt epoch-1 after observer catchup delivery"
    );

    // Dedupe set check: synth_catchups must contain the (community_id, dave_addr, join_id, epoch) key.
    {
        let set = synth_catchups.lock().unwrap();
        assert!(
            set.iter()
                .any(|(sid, addr, _, _)| *sid == community_id && *addr == dave_addr),
            "synth_catchups must record the synthesized catchup for Dave"
        );
    }

    registry.shutdown_all().await.expect("shutdown");
}

/// ZEB-578: an INVITE-ONLY member admitted into a community whose epoch has
/// already rotated joins via a *countersigned* `PendingJoin` — never a `Join`.
/// The self-heal observer must still synthesize an `EpochCatchup` for them, or
/// they stay in `pending_catchup_for` forever and never receive the live key.
///
/// Drives the ACTUAL `self_heal_community_observer` against an invite-only engine
/// where Dave's membership is a `PendingJoin` + admin `JoinCountersign`. Pre-fix
/// the synthesizer matched only `Join` and skipped Dave ("no Join event found");
/// post-fix it accepts the countersigned PendingJoin as the catchup trigger.
#[tokio::test]
async fn invite_only_pending_join_catchup_synthesized_end_to_end() {
    use ed25519_dalek::Signer;
    use harmony_app::community_invite::{canonical_invite_token_bytes, InviteToken};
    use harmony_app::community_state_sync::{
        CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver, DEFAULT_DEBOUNCE_MS,
    };
    use harmony_app::content_store::{ContentStore, RuntimeContentStore};
    use harmony_app::owner_state_crdt::OwnerState;
    use harmony_app::self_heal_community_observer;
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    let community_id = SpaceId([0x57; 16]);

    let pub64_for = |sk: &ed25519_dalek::SigningKey| -> [u8; 64] {
        let ed = sk.verifying_key().to_bytes();
        let x = harmony_app::dm_signing::ed25519_pub_to_x25519(&ed).expect("ed→x25519");
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(&x);
        out[32..].copy_from_slice(&ed);
        out
    };

    let admin = harmony_app::community_membership::mint_test_owner(0xAA);
    let admin_addr = admin.owner;
    let admin_signing_key = Arc::new(admin.device_key.clone());
    let admin_pub64 = pub64_for(&admin_signing_key);

    let bob = harmony_app::community_membership::mint_test_owner(0xBB);
    let bob_addr = bob.owner;
    let bob_signing_key = bob.device_key.clone();
    let bob_pub64 = pub64_for(&bob_signing_key);

    let dave = harmony_app::community_membership::mint_test_owner(0xDD);
    let dave_addr = dave.owner;
    let dave_signing_key = dave.device_key.clone();
    let dave_pub64 = pub64_for(&dave_signing_key);

    // k0: engine spawn-time key. k1: post-rotation key the catchup must seal.
    let k0 = EpochKey::new([0x10u8; 32]);
    let k1 = EpochKey::new([0x20u8; 32]);

    struct StaticResolver(std::collections::HashMap<OwnerAddr, [u8; 64]>);
    #[async_trait::async_trait]
    impl IdentityResolver for StaticResolver {
        async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
            self.0.get(addr).copied()
        }
    }
    let mut pub_map = std::collections::HashMap::new();
    pub_map.insert(admin_addr, admin_pub64);
    pub_map.insert(bob_addr, bob_pub64);
    pub_map.insert(dave_addr, dave_pub64);
    let resolver: Arc<dyn IdentityResolver> = Arc::new(StaticResolver(pub_map));

    // CAS servicer so publish_root_now (notify_dirty after each insert) doesn't
    // deadlock waiting on a reply.
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel::<harmony_app::content_store::CasOp>(64);
    tokio::spawn(async move {
        use harmony_app::content_store::CasOp;
        let mut store: std::collections::HashMap<harmony_content::cid::ContentId, Vec<u8>> =
            std::collections::HashMap::new();
        while let Some(op) = cas_op_rx.recv().await {
            match op {
                CasOp::PutLocal {
                    cid, blob, reply, ..
                } => {
                    store.insert(cid, blob);
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(()));
                    }
                }
                CasOp::GetOrFetch { cid, reply, .. } => {
                    let _ = reply.send(Ok(store.get(&cid).cloned()));
                }
                CasOp::GetLocal { cid, reply } => {
                    let _ = reply.send(store.get(&cid).cloned());
                }
                CasOp::AllowServeSubtree { reply, .. } => {
                    let _ = reply.send(Ok(0));
                }
            }
        }
    });
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));
    let dir = tempfile::tempdir().expect("tempdir");
    // ZEB-790: one adoption floor per simulated node — this test models a
    // single node ("admin-dev"), so the registry and the self_heal observer
    // that mint/feed HLC share ONE floor.
    let adopt_floor = harmony_app::hlc_adopt_floor::HlcAdoptFloor::new();
    let registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        adopt_floor: adopt_floor.clone(),
        device_id: "admin-dev".into(),
        content_store: cs,
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: admin_addr,
        signing_key: Arc::clone(&admin_signing_key),
        crdt_state: None,
        nav_emitter: None,
        presence_resync_rx: None,
    }));

    let (pub_tx, _pub_rx) = mpsc::channel(8);
    let (_sub_tx, sub_rx) = mpsc::channel(8);
    registry
        .spawn_engine_inner_now(
            community_id,
            k0.clone(),
            admin_addr,
            true, // INVITE-ONLY — the scenario under test
            pub_tx,
            sub_rx,
            harmony_app::community_state_sync::CatchUpChannels::none(),
        )
        .await
        .expect("engine spawn");
    let engine = registry
        .engine_arc(&community_id)
        .await
        .expect("engine arc");

    // admin Join (root of trust; its cert seeds the enrolled key the InviteToken
    // sig is verified against).
    let join_admin = SignedMembershipEvent {
        enrollment: Some(admin.cert.clone()),
        ..make_signed_event(
            0x01,
            community_id,
            admin_addr,
            MembershipEventKind::Join,
            100,
            &admin_signing_key,
        )
    };
    engine
        .insert_local_event_with_pubs(join_admin, admin_pub64, None)
        .await
        .expect("insert admin join");

    // An admin-signed InviteToken (invite-only joins carry one). Reused for both
    // bob and Dave — only the inviter sig matters to verify, not the hint.
    let mint_admin_token = || {
        let mut tok = InviteToken {
            inviter: admin_addr,
            invitee_hint: None,
            minted_at: Hlc {
                wall_ms: 50,
                logical: 0,
                device_id: "admin-dev".into(),
            },
            expires_at: None,
            sig: [0u8; 64],
        };
        let bytes = canonical_invite_token_bytes(&tok).expect("encode token");
        tok.sig = admin_signing_key.sign(&bytes).to_bytes();
        tok
    };

    // bob joins INVITE-ONLY at epoch 0 (PendingJoin + admin countersign) so the
    // later kick is a real membership transition that triggers a rotation. A bare
    // `Join` from a non-admin is rejected in an invite-only community.
    let pending_bob = SignedMembershipEvent {
        enrollment: Some(bob.cert.clone()),
        ..make_signed_event(
            0x02,
            community_id,
            bob_addr,
            MembershipEventKind::PendingJoin {
                invite_token: mint_admin_token(),
            },
            200,
            &bob_signing_key,
        )
    };
    engine
        .insert_local_event_with_pubs(pending_bob.clone(), bob_pub64, None)
        .await
        .expect("insert bob pending join");
    let countersign_bob = make_signed_event(
        0x03,
        community_id,
        admin_addr,
        MembershipEventKind::JoinCountersign {
            target_event_id: pending_bob.id,
        },
        201,
        &admin_signing_key,
    );
    engine
        .insert_local_event_with_pubs(countersign_bob, admin_pub64, None)
        .await
        .expect("insert bob countersign");

    // admin kicks bob → rotation advances the community to epoch 1.
    let kick_bob = make_signed_event(
        0x10,
        community_id,
        admin_addr,
        MembershipEventKind::Kick {
            target: bob_addr,
            reason: None,
        },
        1000,
        &admin_signing_key,
    );
    engine
        .insert_local_event_with_pubs(kick_bob.clone(), admin_pub64, None)
        .await
        .expect("insert kick");

    let rotation_rcs: Vec<RecipientCiphertext> =
        seal_epoch_to_members(&k1, &[(&admin_addr, &admin_signing_key)])
            .into_iter()
            .map(|(addr, sealed)| RecipientCiphertext {
                recipient: addr,
                sealed,
            })
            .collect();
    let rotation_event = make_signed_event(
        0x11,
        community_id,
        admin_addr,
        MembershipEventKind::EpochRotation {
            prior_epoch: 0,
            triggered_by: kick_bob.id,
            recipient_ciphertexts: rotation_rcs,
        },
        1001,
        &admin_signing_key,
    );
    engine
        .insert_local_event_with_pubs(rotation_event, admin_pub64, None)
        .await
        .expect("insert rotation");

    // Dave joins INVITE-ONLY *after* the rotation: a PendingJoin carrying an
    // admin-signed InviteToken, then the admin's JoinCountersign. He materializes
    // to Joined and (epoch already 1) is enqueued into pending_catchup_for.
    let pending_dave = SignedMembershipEvent {
        enrollment: Some(dave.cert.clone()),
        ..make_signed_event(
            0x04,
            community_id,
            dave_addr,
            MembershipEventKind::PendingJoin {
                invite_token: mint_admin_token(),
            },
            2000,
            &dave_signing_key,
        )
    };
    engine
        .insert_local_event_with_pubs(pending_dave.clone(), dave_pub64, None)
        .await
        .expect("insert dave pending join");

    let countersign_dave = make_signed_event(
        0x05,
        community_id,
        admin_addr,
        MembershipEventKind::JoinCountersign {
            target_event_id: pending_dave.id,
        },
        2001,
        &admin_signing_key,
    );
    engine
        .insert_local_event_with_pubs(countersign_dave, admin_pub64, None)
        .await
        .expect("insert dave countersign");

    // Sanity: Dave is Joined via PendingJoin+countersign AND flagged for catchup
    // (he joined after the epoch rotated). This is the precondition the
    // synthesizer must act on.
    {
        let state = engine.state();
        let g = state.lock().await;
        let mat = g.materialize_now(admin_addr);
        assert!(
            matches!(
                mat.members.get(&dave_addr).map(|s| s.status),
                Some(harmony_app::community_membership::MemberStatus::Joined)
            ),
            "Dave must materialize to Joined via PendingJoin + countersign"
        );
        assert!(
            mat.pending_catchup_for.contains(&dave_addr),
            "Dave must be enqueued for catchup (joined after the epoch rotation)"
        );
    }

    // crdt_state carries K(1) — the key the observer must seal to Dave.
    let admin_space_k1 = make_space_with_epoch(community_id, admin_addr, 1, k1.clone());
    let mut owner_state = OwnerState::default();
    owner_state.apply_space_with_canonicalization(admin_space_k1);
    let crdt_state = Arc::new(tokio::sync::Mutex::new(owner_state));

    let hlc_tracker: Arc<tokio::sync::Mutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>> =
        Arc::new(tokio::sync::Mutex::new(
            harmony_crdt_sync::ReplayTracker::new("admin-dev".into()),
        ));
    let synth_rotations: Arc<
        Mutex<
            BTreeSet<(
                SpaceId,
                OwnerAddr,
                harmony_app::community_membership::EventId,
            )>,
        >,
    > = Arc::new(Mutex::new(BTreeSet::new()));
    let synth_catchups: SynthCatchupsSet = Arc::new(Mutex::new(BTreeSet::new()));

    self_heal_community_observer(
        community_id,
        Arc::clone(&registry),
        Arc::clone(&admin_signing_key),
        Arc::clone(&hlc_tracker),
        adopt_floor.clone(),
        "admin-dev".into(),
        admin_addr,
        Arc::clone(&crdt_state),
        Arc::clone(&synth_rotations),
        Arc::clone(&synth_catchups),
    )
    .await;

    // The observer must synthesize an EpochCatchup for Dave AND it must seal the
    // correct post-rotation key K(1) — not merely name him. A wrong trigger /
    // epoch could name Dave while sealing the wrong key; applying the catchup to
    // Dave's space guards trigger-selection correctness, mirroring the Join-path
    // sibling test. Pre-fix the synthesizer skipped Dave entirely (his only
    // membership event is a PendingJoin, not a Join).
    let catchup_event = {
        let state = engine.state();
        let g = state.lock().await;
        let ev = g
            .events()
            .find(|e| {
                matches!(
                    &e.kind,
                    MembershipEventKind::EpochCatchup { recipient_ciphertexts, .. }
                        if recipient_ciphertexts.iter().any(|rc| rc.recipient == dave_addr)
                )
            })
            .cloned();
        ev
    }
    .expect(
        "observer must synthesize an EpochCatchup for the invite-only (countersigned PendingJoin) joiner",
    );

    let mut dave_space = make_space_with_epoch(community_id, admin_addr, 0, k0.clone());
    assert!(
        apply_catchup_to_space(
            &mut dave_space,
            &catchup_event,
            dave_addr,
            &dave_signing_key
        ),
        "Dave must be able to apply the observer's catchup"
    );
    assert_eq!(
        dave_space.current_epoch,
        Some(1),
        "catchup must advance Dave to epoch 1"
    );
    assert!(
        dave_space
            .current_epoch_key
            .as_ref()
            .map(|k| k.as_bytes() == k1.as_bytes())
            .unwrap_or(false),
        "catchup must seal the post-rotation key K(1) to Dave (correct trigger + epoch)"
    );
    {
        let set = synth_catchups.lock().unwrap();
        assert!(
            set.iter()
                .any(|(sid, addr, _, _)| *sid == community_id && *addr == dave_addr),
            "synth_catchups must record the synthesized catchup for Dave"
        );
    }

    registry.shutdown_all().await.expect("shutdown");
}

/// ZEB-249 §4.3: Two admins A1 and A2 simultaneously kick X and Y. After
/// both rotations, the materialized pending_rotation_for is empty (self-heal
/// converged). Members who received the correct rotation can decrypt.
///
/// Note: this test verifies the CRDT materialize() tracking logic for
/// pending_rotation_for by directly calling materialize() with pre-assembled
/// event sets. It does NOT exercise the live self-healing observer or a full
/// multi-admin runtime race — convergence is validated at the CRDT layer.
#[test]
fn concurrent_kicks_self_heal_end_to_end() {
    use harmony_identity::PrivateIdentity;
    let community_id = SpaceId([0x33; 16]);
    let admin_identity = PrivateIdentity::from_seed(&[0xAA; 32]);
    let admin_sk_bytes = admin_identity.to_private_bytes();
    let admin_ed_seed: [u8; 32] = admin_sk_bytes[32..64].try_into().unwrap();
    let admin_signing_key = ed25519_dalek::SigningKey::from_bytes(&admin_ed_seed);
    let admin_addr = OwnerAddr(admin_identity.identity.address_hash);

    let x_addr = OwnerAddr([0x10; 16]);
    let y_addr = OwnerAddr([0x20; 16]);
    let z_addr = OwnerAddr([0x30; 16]); // survivor

    // Build the event log manually: admin + x + y + z join, then x and y kicked.
    // Admin starts at power=100, x/y/z at power=0.
    let hlc = |w: u64| Hlc {
        wall_ms: w,
        logical: 0,
        device_id: "test".into(),
    };

    let join_admin = make_signed_event(
        0x01,
        community_id,
        admin_addr,
        MembershipEventKind::Join,
        100,
        &admin_signing_key,
    );
    let join_x = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x11; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: x_addr,
        at: hlc(200),
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };
    let join_y = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x12; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: y_addr,
        at: hlc(300),
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };
    let join_z = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x13; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: z_addr,
        at: hlc(400),
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };

    // Admin kicks X and Y simultaneously.
    let kick_x = make_signed_event(
        0x20,
        community_id,
        admin_addr,
        MembershipEventKind::Kick {
            target: x_addr,
            reason: None,
        },
        1000,
        &admin_signing_key,
    );
    let kick_y = make_signed_event(
        0x21,
        community_id,
        admin_addr,
        MembershipEventKind::Kick {
            target: y_addr,
            reason: None,
        },
        1001,
        &admin_signing_key,
    );

    // After both kicks, materialize: both x and y should be in pending_rotation_for.
    let log_post_kicks = vec![
        join_admin.clone(),
        join_x.clone(),
        join_y.clone(),
        join_z.clone(),
        kick_x.clone(),
        kick_y.clone(),
    ];
    let mat_post_kicks = materialize(&log_post_kicks, admin_addr);
    assert!(
        mat_post_kicks.pending_rotation_for.contains(&x_addr),
        "x must be in pending_rotation_for"
    );
    assert!(
        mat_post_kicks.pending_rotation_for.contains(&y_addr),
        "y must be in pending_rotation_for"
    );
    assert_eq!(mat_post_kicks.pending_rotation_for.len(), 2);

    // Admin issues EpochRotation for X's kick (covering Y's kick too by referencing kick_x).
    // In the multi-kick case, spec §4.3 says any rotation for a pending kick clears it;
    // the admin issues one rotation per kick in practice, or one covers both via HLC order.
    // Here we test that a rotation for kick_x clears x from pending_rotation_for.
    let _k1 = EpochKey::new([0x20u8; 32]);
    let rot_x = make_signed_event(
        0x30,
        community_id,
        admin_addr,
        MembershipEventKind::EpochRotation {
            prior_epoch: 0,
            triggered_by: kick_x.id,
            recipient_ciphertexts: vec![
                RecipientCiphertext {
                    recipient: admin_addr,
                    sealed: vec![0u8; 60],
                },
                RecipientCiphertext {
                    recipient: z_addr,
                    sealed: vec![0u8; 60],
                },
            ],
        },
        1002,
        &admin_signing_key,
    );

    let log_with_rot_x = vec![
        join_admin.clone(),
        join_x.clone(),
        join_y.clone(),
        join_z.clone(),
        kick_x.clone(),
        kick_y.clone(),
        rot_x.clone(),
    ];
    let mat_with_rot_x = materialize(&log_with_rot_x, admin_addr);
    // x is cleared; y still pending.
    assert!(
        !mat_with_rot_x.pending_rotation_for.contains(&x_addr),
        "x must be cleared after rotation for kick_x"
    );
    assert!(
        mat_with_rot_x.pending_rotation_for.contains(&y_addr),
        "y still pending (only kick_x rotation issued so far)"
    );
    assert_eq!(mat_with_rot_x.current_epoch, Some(1), "epoch must advance");

    // Issue rotation for Y's kick.
    let rot_y = make_signed_event(
        0x31,
        community_id,
        admin_addr,
        MembershipEventKind::EpochRotation {
            prior_epoch: 1,
            triggered_by: kick_y.id,
            recipient_ciphertexts: vec![
                RecipientCiphertext {
                    recipient: admin_addr,
                    sealed: vec![0u8; 60],
                },
                RecipientCiphertext {
                    recipient: z_addr,
                    sealed: vec![0u8; 60],
                },
            ],
        },
        1003,
        &admin_signing_key,
    );

    let log_full = vec![
        join_admin, join_x, join_y, join_z, kick_x, kick_y, rot_x, rot_y,
    ];
    let mat_final = materialize(&log_full, admin_addr);
    assert!(
        mat_final.pending_rotation_for.is_empty(),
        "all kicks rotated: pending_rotation_for must be empty (self-heal converged)"
    );
    assert_eq!(mat_final.current_epoch, Some(2), "final epoch must be 2");
}

/// ZEB-249 §4.4: Member B cooperatively issues an EpochRotation on Leave.
/// After B's Leave + bundled rotation, admin does NOT need to act — the
/// rotation is already in the CRDT and pending_rotation_for is cleared.
#[test]
fn leaver_cooperative_rotation() {
    use harmony_identity::PrivateIdentity;

    let community_id = SpaceId([0x34; 16]);
    let admin_identity = PrivateIdentity::from_seed(&[0xAA; 32]);
    let admin_sk_bytes = admin_identity.to_private_bytes();
    let admin_ed_seed: [u8; 32] = admin_sk_bytes[32..64].try_into().unwrap();
    let admin_signing_key = ed25519_dalek::SigningKey::from_bytes(&admin_ed_seed);
    let admin_addr = OwnerAddr(admin_identity.identity.address_hash);

    let b_identity = PrivateIdentity::from_seed(&[0xBB; 32]);
    let b_sk_bytes = b_identity.to_private_bytes();
    let b_ed_seed: [u8; 32] = b_sk_bytes[32..64].try_into().unwrap();
    let b_signing_key = ed25519_dalek::SigningKey::from_bytes(&b_ed_seed);
    let b_addr = OwnerAddr(b_identity.identity.address_hash);

    let hlc = |w: u64| Hlc {
        wall_ms: w,
        logical: 0,
        device_id: "test".into(),
    };

    let join_admin = make_signed_event(
        0x01,
        community_id,
        admin_addr,
        MembershipEventKind::Join,
        100,
        &admin_signing_key,
    );
    let join_b = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x02; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: b_addr,
        at: hlc(200),
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };

    // B leaves cooperatively.
    let leave_b = make_signed_event(
        0x10,
        community_id,
        b_addr,
        MembershipEventKind::Leave,
        1000,
        &b_signing_key,
    );

    // B issues rotation excluding self (spec §4.4).
    let k1 = EpochKey::new([0x20u8; 32]);
    let sealed_for_admin = seal_epoch_to_members(&k1, &[(&admin_addr, &admin_signing_key)]);
    let rot_rcs: Vec<RecipientCiphertext> = sealed_for_admin
        .into_iter()
        .map(|(addr, sealed)| RecipientCiphertext {
            recipient: addr,
            sealed,
        })
        .collect();
    // Verify B is NOT in the rotation recipients.
    assert!(
        rot_rcs.iter().all(|rc| rc.recipient != b_addr),
        "leaver must not be in rotation recipients"
    );

    let rotation = make_signed_event(
        0x11,
        community_id,
        b_addr,
        MembershipEventKind::EpochRotation {
            prior_epoch: 0,
            triggered_by: leave_b.id,
            recipient_ciphertexts: rot_rcs,
        },
        1001,
        &b_signing_key,
    );

    // Materialize: pending_rotation_for should be cleared by B's rotation.
    let log = vec![join_admin, join_b, leave_b, rotation];
    let mat = materialize(&log, admin_addr);
    assert!(
        !mat.pending_rotation_for.contains(&b_addr),
        "cooperative rotation clears pending_rotation_for for leaver"
    );
    assert!(
        mat.pending_rotation_for.is_empty(),
        "no pending rotations after cooperative leave"
    );
    assert_eq!(
        mat.current_epoch,
        Some(1),
        "epoch advances on cooperative leave"
    );

    // Admin receives K(1) from B's sealed rotation.
    let k0 = EpochKey::new([0x10u8; 32]);
    let mut admin_space = make_space_with_epoch(community_id, admin_addr, 0, k0);
    // Build the rotation event as a SignedMembershipEvent (needed for apply_rotation_to_space).
    let rotation2 = {
        let sealed_for_admin2 = seal_epoch_to_members(&k1, &[(&admin_addr, &admin_signing_key)]);
        let rot_rcs2: Vec<RecipientCiphertext> = sealed_for_admin2
            .into_iter()
            .map(|(addr, sealed)| RecipientCiphertext {
                recipient: addr,
                sealed,
            })
            .collect();
        SignedMembershipEvent {
            signer_certs: Vec::new(),
            id: [0x12; 16],
            community_id,
            kind: MembershipEventKind::EpochRotation {
                prior_epoch: 0,
                triggered_by: [0x10; 16],
                recipient_ciphertexts: rot_rcs2,
            },
            actor: b_addr,
            at: Hlc {
                wall_ms: 1001,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        }
    };
    apply_rotation_to_space(&mut admin_space, &rotation2, admin_addr, &admin_signing_key);
    assert_eq!(admin_space.current_epoch, Some(1));

    // B's space is dropped (leaver doesn't keep the space).
    // Admin can encrypt at epoch 1; B has no access.
    let msg = b"epoch 1 post-leave";
    let env1 = encrypt_for_topic(&admin_space, msg).expect("encrypt@1");

    let b_space = make_space_with_epoch(community_id, admin_addr, 0, EpochKey::new([0x10u8; 32]));
    assert!(
        matches!(
            decrypt_for_topic(&b_space, &env1),
            Err(EpochError::KeyNotAvailable(1))
        ),
        "B cannot decrypt epoch-1 after leaving"
    );
}

/// ZEB-249 §4.4: B issues a Leave with a malicious rotation that includes
/// self as a recipient. The materialize() layer must reject this rotation
/// (target must NOT appear in recipient_ciphertexts). The admin self-healing
/// observer then synthesizes a valid rotation.
#[test]
fn leaver_malicious_self_include_rejected_admin_self_heals() {
    use harmony_identity::PrivateIdentity;

    let community_id = SpaceId([0x35; 16]);
    let admin_identity = PrivateIdentity::from_seed(&[0xAA; 32]);
    let admin_sk_bytes = admin_identity.to_private_bytes();
    let admin_ed_seed: [u8; 32] = admin_sk_bytes[32..64].try_into().unwrap();
    let admin_signing_key = ed25519_dalek::SigningKey::from_bytes(&admin_ed_seed);
    let admin_addr = OwnerAddr(admin_identity.identity.address_hash);

    let b_identity = PrivateIdentity::from_seed(&[0xBB; 32]);
    let b_sk_bytes = b_identity.to_private_bytes();
    let b_ed_seed: [u8; 32] = b_sk_bytes[32..64].try_into().unwrap();
    let b_signing_key = ed25519_dalek::SigningKey::from_bytes(&b_ed_seed);
    let b_addr = OwnerAddr(b_identity.identity.address_hash);

    let hlc = |w: u64| Hlc {
        wall_ms: w,
        logical: 0,
        device_id: "test".into(),
    };

    let join_admin = make_signed_event(
        0x01,
        community_id,
        admin_addr,
        MembershipEventKind::Join,
        100,
        &admin_signing_key,
    );
    let join_b = SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x02; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: b_addr,
        at: hlc(200),
        sig: [0; 64],
        countersig: None,
        enrollment: None,
    };
    let leave_b = make_signed_event(
        0x10,
        community_id,
        b_addr,
        MembershipEventKind::Leave,
        1000,
        &b_signing_key,
    );

    // B maliciously includes SELF in the rotation recipients.
    let k1 = EpochKey::new([0x20u8; 32]);
    let malicious_rcs = vec![
        RecipientCiphertext {
            recipient: admin_addr,
            sealed: vec![0u8; 60],
        },
        // B includes itself — this violates the spec §4.4 invariant.
        RecipientCiphertext {
            recipient: b_addr,
            sealed: vec![0u8; 60],
        },
    ];
    let malicious_rotation = make_signed_event(
        0x11,
        community_id,
        b_addr,
        MembershipEventKind::EpochRotation {
            prior_epoch: 0,
            triggered_by: leave_b.id,
            recipient_ciphertexts: malicious_rcs,
        },
        1001,
        &b_signing_key,
    );

    // Materialize: malicious rotation MUST be silently dropped.
    // B still appears in pending_rotation_for (rotation didn't count).
    let log_with_malicious = vec![
        join_admin.clone(),
        join_b.clone(),
        leave_b.clone(),
        malicious_rotation,
    ];
    let mat_malicious = materialize(&log_with_malicious, admin_addr);
    assert!(
        mat_malicious.pending_rotation_for.contains(&b_addr),
        "malicious rotation must be dropped; B stays in pending_rotation_for"
    );
    assert_eq!(
        mat_malicious.current_epoch, None,
        "epoch must NOT advance from a malicious rotation"
    );

    // Admin self-heals: issues a valid rotation excluding B.
    let sealed_for_admin = seal_epoch_to_members(&k1, &[(&admin_addr, &admin_signing_key)]);
    let valid_rcs: Vec<RecipientCiphertext> = sealed_for_admin
        .into_iter()
        .map(|(addr, sealed)| RecipientCiphertext {
            recipient: addr,
            sealed,
        })
        .collect();
    let valid_rotation = make_signed_event(
        0x12,
        community_id,
        admin_addr,
        MembershipEventKind::EpochRotation {
            prior_epoch: 0,
            triggered_by: leave_b.id,
            recipient_ciphertexts: valid_rcs,
        },
        1002,
        &admin_signing_key,
    );

    // Materialize with admin's valid rotation: pending_rotation_for cleared.
    let log_healed = vec![join_admin, join_b, leave_b, valid_rotation];
    let mat_healed = materialize(&log_healed, admin_addr);
    assert!(
        !mat_healed.pending_rotation_for.contains(&b_addr),
        "admin's valid rotation clears pending_rotation_for"
    );
    assert!(
        mat_healed.pending_rotation_for.is_empty(),
        "no pending rotations after admin self-heal"
    );
    assert_eq!(
        mat_healed.current_epoch,
        Some(1),
        "epoch advances after valid admin rotation"
    );

    // B cannot decrypt epoch-1 messages (admin's rotation excluded B).
    let k0 = EpochKey::new([0x10u8; 32]);
    let mut admin_space = make_space_with_epoch(community_id, admin_addr, 0, k0.clone());
    let rot_event = {
        let sealed_for_admin2 = seal_epoch_to_members(&k1, &[(&admin_addr, &admin_signing_key)]);
        let rot_rcs2: Vec<RecipientCiphertext> = sealed_for_admin2
            .into_iter()
            .map(|(addr, sealed)| RecipientCiphertext {
                recipient: addr,
                sealed,
            })
            .collect();
        SignedMembershipEvent {
            signer_certs: Vec::new(),
            id: [0x13; 16],
            community_id,
            kind: MembershipEventKind::EpochRotation {
                prior_epoch: 0,
                triggered_by: [0x10; 16],
                recipient_ciphertexts: rot_rcs2,
            },
            actor: admin_addr,
            at: Hlc {
                wall_ms: 1002,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        }
    };
    apply_rotation_to_space(&mut admin_space, &rot_event, admin_addr, &admin_signing_key);
    let msg = b"epoch 1 admin only";
    let env1 = encrypt_for_topic(&admin_space, msg).expect("encrypt@1");
    let b_space = make_space_with_epoch(community_id, admin_addr, 0, k0);
    assert!(
        matches!(
            decrypt_for_topic(&b_space, &env1),
            Err(EpochError::KeyNotAvailable(1))
        ),
        "B must not decrypt epoch-1 after malicious rotation rejected + admin self-heal"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// ZEB-249 §10.6 Phase D: cross-node apply_remote_epoch_event integration tests
// ──────────────────────────────────────────────────────────────────────────────

/// Phase D test 1: remote EpochRotation propagated via `apply_remote_epoch_event`
/// advances the receiving node's epoch and installs the new key.
///
/// Node A (admin) rotates epoch 0→1. Node B (member) receives the rotation
/// as a `SignedMembershipEvent` via CRDT sync. After calling
/// `apply_remote_epoch_event`, B's Space must reflect epoch 1, the new key
/// K(1), and K(0) archived in `old_epoch_keys`.
#[tokio::test]
async fn two_node_remote_rotation_propagates_new_key() {
    use harmony_app::apply_remote_epoch_event;
    use harmony_app::owner_state_crdt::OwnerState;
    use harmony_identity::PrivateIdentity;
    use std::sync::Arc;

    let community_id = SpaceId([0x50; 16]);

    let admin_identity = PrivateIdentity::from_seed(&[0xAA; 32]);
    let admin_addr = OwnerAddr(admin_identity.identity.address_hash);
    let admin_sk_bytes = admin_identity.to_private_bytes();
    let admin_ed_seed: [u8; 32] = admin_sk_bytes[32..64].try_into().unwrap();
    let admin_signing_key = ed25519_dalek::SigningKey::from_bytes(&admin_ed_seed);

    let b_identity = PrivateIdentity::from_seed(&[0xBB; 32]);
    let b_addr = OwnerAddr(b_identity.identity.address_hash);
    let b_sk_bytes = b_identity.to_private_bytes();
    let b_ed_seed: [u8; 32] = b_sk_bytes[32..64].try_into().unwrap();
    let b_signing_key = Arc::new(ed25519_dalek::SigningKey::from_bytes(&b_ed_seed));

    let k0 = EpochKey::new([0x10u8; 32]);
    let k1 = EpochKey::new([0x20u8; 32]);

    // B starts at epoch 0 with K(0).
    let b_space = make_space_with_epoch(community_id, admin_addr, 0, k0.clone());
    let mut owner_state = OwnerState::default();
    owner_state.apply_space_with_canonicalization(b_space);
    let crdt_state = Arc::new(tokio::sync::Mutex::new(owner_state));

    // Admin encrypts a message under K(1) (post-rotation).
    let mut admin_space = make_space_with_epoch(community_id, admin_addr, 1, k1.clone());
    admin_space.old_epoch_keys.insert(0, k0.clone());
    let msg = b"epoch-1 content only B can decrypt after rotation";
    let env1 = encrypt_for_topic(&admin_space, msg).expect("admin encrypt@1");

    // Build EpochRotation sealing K(1) to admin + B.
    let sealed_pairs = seal_epoch_to_members(
        &k1,
        &[(&admin_addr, &admin_signing_key), (&b_addr, &b_signing_key)],
    );
    let rotation_event = make_signed_event(
        0xA0,
        community_id,
        admin_addr,
        MembershipEventKind::EpochRotation {
            prior_epoch: 0,
            triggered_by: admin_addr.0,
            recipient_ciphertexts: sealed_pairs
                .into_iter()
                .map(|(recipient, sealed)| RecipientCiphertext { recipient, sealed })
                .collect(),
        },
        1000,
        &admin_signing_key,
    );

    // B receives the rotation — apply_remote_epoch_event must update B's Space.
    apply_remote_epoch_event(
        Arc::clone(&crdt_state),
        Arc::clone(&b_signing_key),
        community_id,
        &rotation_event,
        b_addr,
    )
    .await;

    // Verify B's Space now reflects epoch 1.
    {
        let state = crdt_state.lock().await;
        let space = state.spaces.get(&community_id).expect("space must exist");
        assert_eq!(
            space.current_epoch,
            Some(1),
            "B's epoch must have advanced to 1"
        );
        assert_eq!(
            space.current_epoch_key.as_ref().map(|k| k.as_bytes()),
            Some(k1.as_bytes()),
            "B's current_epoch_key must be K(1)"
        );
        assert_eq!(
            space.old_epoch_keys.get(&0).map(|k| k.as_bytes()),
            Some(k0.as_bytes()),
            "K(0) must be archived in old_epoch_keys at index 0"
        );
    }

    // B must now be able to decrypt epoch-1 content.
    let b_space_updated = {
        let state = crdt_state.lock().await;
        state.spaces.get(&community_id).cloned().expect("space")
    };
    let decrypted = decrypt_for_topic(&b_space_updated, &env1).expect("B decrypts epoch-1");
    assert_eq!(decrypted, msg, "decrypted content must match");
}

/// Phase D test 2: a node that was offline during rotation receives both the
/// rotation event and an EpochCatchup. `apply_remote_epoch_event` on the
/// catchup correctly installs the current epoch key without archiving.
///
/// Scenario: admin rotates 0→1 (B was offline). B comes online, receives
/// first the catchup (epoch=1, sealed K(1)) via delta consumer. After applying
/// the catchup, B can decrypt epoch-1 content.
#[tokio::test]
async fn offline_catchup_via_remote_rotation_observation() {
    use harmony_app::apply_remote_epoch_event;
    use harmony_app::owner_state_crdt::OwnerState;
    use harmony_identity::PrivateIdentity;
    use std::sync::Arc;

    let community_id = SpaceId([0x51; 16]);

    let admin_identity = PrivateIdentity::from_seed(&[0xAA; 32]);
    let admin_addr = OwnerAddr(admin_identity.identity.address_hash);
    let admin_sk_bytes = admin_identity.to_private_bytes();
    let admin_ed_seed: [u8; 32] = admin_sk_bytes[32..64].try_into().unwrap();
    let admin_signing_key = ed25519_dalek::SigningKey::from_bytes(&admin_ed_seed);

    let b_identity = PrivateIdentity::from_seed(&[0xBB; 32]);
    let b_addr = OwnerAddr(b_identity.identity.address_hash);
    let b_sk_bytes = b_identity.to_private_bytes();
    let b_ed_seed: [u8; 32] = b_sk_bytes[32..64].try_into().unwrap();
    let b_signing_key = Arc::new(ed25519_dalek::SigningKey::from_bytes(&b_ed_seed));

    let k0 = EpochKey::new([0x30u8; 32]);
    let k1 = EpochKey::new([0x40u8; 32]);

    // B was offline; its local Space is still at epoch 0 with K(0).
    let b_space_initial = make_space_with_epoch(community_id, admin_addr, 0, k0.clone());
    let mut owner_state = OwnerState::default();
    owner_state.apply_space_with_canonicalization(b_space_initial);
    let crdt_state = Arc::new(tokio::sync::Mutex::new(owner_state));

    // Admin's Space is at epoch 1 — they encrypt content B must eventually read.
    let admin_space_k1 = make_space_with_epoch(community_id, admin_addr, 1, k1.clone());
    let msg = b"epoch-1 message for offline B";
    let env1 = encrypt_for_topic(&admin_space_k1, msg).expect("admin encrypt@1");

    // Admin synthesizes a catchup for B (sealed K(1) directly to B).
    let sealed_pairs = seal_epoch_to_members(&k1, &[(&b_addr, &b_signing_key)]);
    let catchup_event = make_signed_event(
        0xB0,
        community_id,
        admin_addr,
        MembershipEventKind::EpochCatchup {
            epoch: 1,
            triggered_by: b_addr.0,
            recipient_ciphertexts: sealed_pairs
                .into_iter()
                .map(|(recipient, sealed)| RecipientCiphertext { recipient, sealed })
                .collect(),
        },
        2000,
        &admin_signing_key,
    );

    // B receives the catchup delta.
    apply_remote_epoch_event(
        Arc::clone(&crdt_state),
        Arc::clone(&b_signing_key),
        community_id,
        &catchup_event,
        b_addr,
    )
    .await;

    // Verify: B now has epoch=1 + K(1). B started at epoch=0 with K(0), so
    // CR Major (PR #106 R6): old_epoch_keys must contain K(0) — the
    // backward-access invariant requires archiving when the catchup advances.
    {
        let state = crdt_state.lock().await;
        let space = state.spaces.get(&community_id).expect("space must exist");
        assert_eq!(space.current_epoch, Some(1), "catchup must set epoch to 1");
        assert_eq!(
            space.current_epoch_key.as_ref().map(|k| k.as_bytes()),
            Some(k1.as_bytes()),
            "catchup must install K(1)"
        );
        assert_eq!(
            space.old_epoch_keys.len(),
            1,
            "EpochCatchup advancing epoch must archive the prior key"
        );
        assert!(
            space
                .old_epoch_keys
                .get(&0)
                .map(|k| k.as_bytes() == k0.as_bytes())
                .unwrap_or(false),
            "old_epoch_keys must contain K(0) at epoch 0 after catchup advances B to epoch 1"
        );
    }

    // B can now decrypt the epoch-1 envelope.
    let b_space_updated = {
        let state = crdt_state.lock().await;
        state.spaces.get(&community_id).cloned().expect("space")
    };
    let decrypted = decrypt_for_topic(&b_space_updated, &env1).expect("B decrypts@1");
    assert_eq!(decrypted, msg);
}

/// Phase D test 3: `apply_remote_epoch_event` is idempotent — applying the
/// same rotation twice leaves the Space unchanged on the second call.
///
/// This covers the duplicate CRDT delivery scenario: Zenoh may re-deliver
/// deltas; the function must not double-archive or advance the epoch counter
/// a second time.
#[tokio::test]
async fn remote_rotation_apply_is_idempotent() {
    use harmony_app::apply_remote_epoch_event;
    use harmony_app::owner_state_crdt::OwnerState;
    use harmony_identity::PrivateIdentity;
    use std::sync::Arc;

    let community_id = SpaceId([0x52; 16]);

    let admin_identity = PrivateIdentity::from_seed(&[0xAA; 32]);
    let admin_addr = OwnerAddr(admin_identity.identity.address_hash);
    let admin_sk_bytes = admin_identity.to_private_bytes();
    let admin_ed_seed: [u8; 32] = admin_sk_bytes[32..64].try_into().unwrap();
    let admin_signing_key = Arc::new(ed25519_dalek::SigningKey::from_bytes(&admin_ed_seed));

    let k0 = EpochKey::new([0x50u8; 32]);
    let k1 = EpochKey::new([0x60u8; 32]);

    // Admin starts at epoch 0.
    let admin_space = make_space_with_epoch(community_id, admin_addr, 0, k0.clone());
    let mut owner_state = OwnerState::default();
    owner_state.apply_space_with_canonicalization(admin_space);
    let crdt_state = Arc::new(tokio::sync::Mutex::new(owner_state));

    // Build a rotation event sealing K(1) to admin only.
    let sealed_pairs = seal_epoch_to_members(&k1, &[(&admin_addr, &admin_signing_key)]);
    let rotation_event = make_signed_event(
        0xC0,
        community_id,
        admin_addr,
        MembershipEventKind::EpochRotation {
            prior_epoch: 0,
            triggered_by: admin_addr.0,
            recipient_ciphertexts: sealed_pairs
                .into_iter()
                .map(|(recipient, sealed)| RecipientCiphertext { recipient, sealed })
                .collect(),
        },
        1000,
        &admin_signing_key,
    );

    // Apply once.
    apply_remote_epoch_event(
        Arc::clone(&crdt_state),
        Arc::clone(&admin_signing_key),
        community_id,
        &rotation_event,
        admin_addr,
    )
    .await;

    // Apply again (simulated duplicate delivery).
    apply_remote_epoch_event(
        Arc::clone(&crdt_state),
        Arc::clone(&admin_signing_key),
        community_id,
        &rotation_event,
        admin_addr,
    )
    .await;

    // Epoch must still be 1 (not 2), and old_epoch_keys must have exactly one entry.
    let state = crdt_state.lock().await;
    let space = state.spaces.get(&community_id).expect("space");
    assert_eq!(space.current_epoch, Some(1), "idempotent: epoch stays at 1");
    assert_eq!(
        space.old_epoch_keys.len(),
        1,
        "idempotent: only one archived key"
    );
    assert_eq!(
        space.old_epoch_keys.get(&0).map(|k| k.as_bytes()),
        Some(k0.as_bytes()),
        "K(0) archived at index 0"
    );
    assert_eq!(
        space.current_epoch_key.as_ref().map(|k| k.as_bytes()),
        Some(k1.as_bytes()),
        "current key remains K(1)"
    );
}

/// Phase D test 4: `apply_remote_epoch_event` is a no-op for events where
/// the local node is not in the recipient list (e.g., the node was kicked).
///
/// The local node's Space must remain unchanged.
#[tokio::test]
async fn remote_rotation_noop_when_not_in_recipient_list() {
    use harmony_app::apply_remote_epoch_event;
    use harmony_app::owner_state_crdt::OwnerState;
    use harmony_identity::PrivateIdentity;
    use std::sync::Arc;

    let community_id = SpaceId([0x53; 16]);

    let admin_identity = PrivateIdentity::from_seed(&[0xAA; 32]);
    let admin_addr = OwnerAddr(admin_identity.identity.address_hash);
    let admin_sk_bytes = admin_identity.to_private_bytes();
    let admin_ed_seed: [u8; 32] = admin_sk_bytes[32..64].try_into().unwrap();
    let admin_signing_key = Arc::new(ed25519_dalek::SigningKey::from_bytes(&admin_ed_seed));

    // B is the node that was kicked — NOT in the rotation's recipient list.
    let b_identity = PrivateIdentity::from_seed(&[0xBB; 32]);
    let b_addr = OwnerAddr(b_identity.identity.address_hash);
    let b_sk_bytes = b_identity.to_private_bytes();
    let b_ed_seed: [u8; 32] = b_sk_bytes[32..64].try_into().unwrap();
    let b_signing_key = Arc::new(ed25519_dalek::SigningKey::from_bytes(&b_ed_seed));

    let k0 = EpochKey::new([0x70u8; 32]);
    let k1 = EpochKey::new([0x80u8; 32]);

    // B starts at epoch 0 with K(0).
    let b_space = make_space_with_epoch(community_id, admin_addr, 0, k0.clone());
    let mut owner_state = OwnerState::default();
    owner_state.apply_space_with_canonicalization(b_space);
    let crdt_state = Arc::new(tokio::sync::Mutex::new(owner_state));

    // Rotation seals K(1) to admin only — B is NOT included (kicked).
    let sealed_pairs = seal_epoch_to_members(&k1, &[(&admin_addr, &admin_signing_key)]);
    let rotation_event = make_signed_event(
        0xD0,
        community_id,
        admin_addr,
        MembershipEventKind::EpochRotation {
            prior_epoch: 0,
            triggered_by: b_addr.0, // B was the triggered_by (kicked member)
            recipient_ciphertexts: sealed_pairs
                .into_iter()
                .map(|(recipient, sealed)| RecipientCiphertext { recipient, sealed })
                .collect(),
        },
        1000,
        &admin_signing_key,
    );

    // B receives the rotation event — but B is not in the recipient list.
    apply_remote_epoch_event(
        Arc::clone(&crdt_state),
        Arc::clone(&b_signing_key),
        community_id,
        &rotation_event,
        b_addr,
    )
    .await;

    // B's Space must be unchanged — still at epoch 0 with K(0).
    let state = crdt_state.lock().await;
    let space = state.spaces.get(&community_id).expect("space");
    assert_eq!(
        space.current_epoch,
        Some(0),
        "B's epoch must remain 0 (not in recipient list)"
    );
    assert_eq!(
        space.current_epoch_key.as_ref().map(|k| k.as_bytes()),
        Some(k0.as_bytes()),
        "B's key must remain K(0)"
    );
    assert!(
        space.old_epoch_keys.is_empty(),
        "no archiving when not in recipient list"
    );
}
