//! Pinned-byte CBOR wire-format fixtures for community-sync types.
//! ZEB-256: envelope gained `publisher_addr` (bstr(16)) + `publisher_sig`
//! (bstr(64)). Old pinned bytes are wholly invalidated; this regen
//! commit IS the deliberate update. Mirrors community-membership wire
//! fixtures — locking the encoded bytes prevents silent wire-form drift
//! across phases.

use harmony_app::community_invite::{
    CommunityInvitePayload, InviteEpochSnapshot, MaterializedCommunityState,
};
use harmony_app::community_membership::{MembershipEventKind, RecipientCiphertext};
use harmony_app::community_state_sync::{
    CommunityRootPublishPayload, CommunityRootSignedPayload, EncryptedEnvelope,
};
use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use harmony_content::cid::ContentId;

#[test]
fn community_root_signed_payload_wire_bytes_pinned() {
    // 3-key map: rc (root_cid), pa (publisher_addr), at (Hlc).
    // All keys are 2 chars to satisfy the same-length-keys invariant.
    let cid = ContentId::from_bytes([0xAA; 32]);
    let p = CommunityRootSignedPayload {
        root_cid: cid,
        publisher_addr: OwnerAddr([0xBB; 16]),
        at: Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 7,
            device_id: "d1".into(),
        },
        // manifest_format: None — ZEB-814 added this with
        // skip_serializing_if = "Option::is_none", so the legacy (monolithic)
        // signed bytes below are byte-identical (the "mf" key is absent).
        manifest_format: None,
    };

    let bytes = canonical_cbor_encode(&p).expect("encode");
    // Lock the byte sequence — any structural change requires this
    // fixture to update intentionally. Paranoia check: every key code
    // is 2 chars (rc, pa, at).
    let expected = hex::decode(
        "a36272635820aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa62706150bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb626174a361771b0000018bcfe56800616c076164626431",
    )
    .expect("hex");
    assert_eq!(
        bytes,
        expected,
        "CommunityRootSignedPayload wire bytes drifted: {} vs {}",
        hex::encode(&bytes),
        hex::encode(&expected)
    );
    let decoded: CommunityRootSignedPayload = canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(decoded, p, "decoded payload must round-trip identically");
}

#[test]
fn community_root_publish_payload_wire_bytes_pinned() {
    // 4-key map: rc, pa, at, ps (publisher_sig).
    let cid = ContentId::from_bytes([0xAA; 32]);
    let p = CommunityRootPublishPayload {
        root_cid: cid,
        publisher_addr: OwnerAddr([0xBB; 16]),
        at: Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 7,
            device_id: "d1".into(),
        },
        publisher_sig: [0xCC; 64],
        // epoch: None — this field was added in ZEB-249 §10.6 with
        // skip_serializing_if = "Option::is_none" so legacy wire bytes are unchanged.
        epoch: None,
        // manifest_format: None — ZEB-814, same skip_serializing_if treatment,
        // so this legacy monolithic-root payload's bytes are unchanged.
        manifest_format: None,
    };

    let bytes = canonical_cbor_encode(&p).expect("encode");
    let expected = hex::decode(
        "a46272635820aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa62706150bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb626174a361771b0000018bcfe56800616c0761646264316270735840cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
    )
    .expect("hex");
    assert_eq!(
        bytes,
        expected,
        "CommunityRootPublishPayload wire bytes drifted: {} vs {}",
        hex::encode(&bytes),
        hex::encode(&expected)
    );
    let decoded: CommunityRootPublishPayload = canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(decoded, p, "decoded payload must round-trip identically");
}

#[test]
fn encrypted_envelope_wire_bytes_pinned_v2_null_ratchet() {
    let env = EncryptedEnvelope {
        epoch: 5,
        nonce: [0x10; 12],
        ciphertext: vec![0x20; 32],
        ratchet_generation: None,
    };
    let bytes = canonical_cbor_encode(&env).expect("encode");
    let expected_hex = "a362657005626e634c10101010101010101010101062637458202020202020202020202020202020202020202020202020202020202020202020";
    let expected = hex::decode(expected_hex).unwrap_or_else(|_| {
        eprintln!("\nACTUAL bytes for pinning: {}\n", hex::encode(&bytes));
        panic!("update PLACEHOLDER with the bytes above");
    });
    assert_eq!(
        bytes,
        expected,
        "drifted: {} vs {}",
        hex::encode(&bytes),
        hex::encode(&expected)
    );
}

#[test]
fn epoch_rotation_event_wire_bytes_pinned() {
    let triggered_by: [u8; 16] = [0xfa; 16];
    let kind = MembershipEventKind::EpochRotation {
        prior_epoch: 5,
        triggered_by,
        recipient_ciphertexts: vec![
            RecipientCiphertext {
                recipient: OwnerAddr([0xa1; 16]),
                sealed: vec![0xab; 92],
            },
            RecipientCiphertext {
                recipient: OwnerAddr([0xb1; 16]),
                sealed: vec![0xcd; 92],
            },
            RecipientCiphertext {
                recipient: OwnerAddr([0xc1; 16]),
                sealed: vec![0xef; 92],
            },
        ],
    };
    let bytes = canonical_cbor_encode(&kind).expect("encode");
    let expected_hex = "a2627467617262766ca36270650562747350fafafafafafafafafafafafafafafafa62726383a262726350a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1626374585cababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababa262726350b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1626374585ccdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcda262726350c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1626374585cefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef";
    let expected = hex::decode(expected_hex).unwrap_or_else(|_| {
        eprintln!("\nACTUAL bytes for pinning: {}\n", hex::encode(&bytes));
        panic!("update PLACEHOLDER with the bytes above");
    });
    assert_eq!(
        bytes,
        expected,
        "EpochRotation wire bytes drifted: {} vs {}",
        hex::encode(&bytes),
        hex::encode(&expected)
    );
}

#[test]
fn epoch_catchup_event_wire_bytes_pinned() {
    let triggered_by: [u8; 16] = [0xfa; 16];
    let kind = MembershipEventKind::EpochCatchup {
        epoch: 7,
        triggered_by,
        recipient_ciphertexts: vec![RecipientCiphertext {
            recipient: OwnerAddr([0xd1; 16]),
            sealed: vec![0xab; 92],
        }],
    };
    let bytes = canonical_cbor_encode(&kind).expect("encode");
    let expected_hex = "a2627467616662766ca36265700762747350fafafafafafafafafafafafafafafafa62726381a262726350d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1626374585cabababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababab";
    let expected = hex::decode(expected_hex).unwrap_or_else(|_| {
        eprintln!("\nACTUAL bytes for pinning: {}\n", hex::encode(&bytes));
        panic!("update PLACEHOLDER with the bytes above");
    });
    assert_eq!(
        bytes,
        expected,
        "EpochCatchup wire bytes drifted: {} vs {}",
        hex::encode(&bytes),
        hex::encode(&expected)
    );
}

#[test]
fn encrypted_envelope_wire_bytes_pinned_v3_with_ratchet() {
    let env = EncryptedEnvelope {
        epoch: 5,
        nonce: [0x10; 12],
        ciphertext: vec![0x20; 32],
        ratchet_generation: Some(42),
    };
    let bytes = canonical_cbor_encode(&env).expect("encode");
    let expected_hex = "a462657005626e634c10101010101010101010101062637458202020202020202020202020202020202020202020202020202020202020202020627267182a";
    let expected = hex::decode(expected_hex).unwrap_or_else(|_| {
        eprintln!("\nACTUAL bytes for pinning: {}\n", hex::encode(&bytes));
        panic!("update PLACEHOLDER with the bytes above");
    });
    assert_eq!(
        bytes,
        expected,
        "drifted: {} vs {}",
        hex::encode(&bytes),
        hex::encode(&expected)
    );
}

/// ZEB-249: Wire-format pinning for `CommunityInvitePayload` with the new
/// `epoch_snapshot: InviteEpochSnapshot` field (replacing v1's `membership_key: EpochKey`).
/// Minimal fixture: open community (no invite_token / admin_bootstrap / admin_identity_pub),
/// empty state_snapshot (no members / channels / power_levels).
///
/// Run with `--no-capture` to print the actual hex once, then replace PLACEHOLDER.
#[test]
fn invite_payload_with_epoch_snapshot_wire_bytes_pinned() {
    let payload = CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id: SpaceId([0xc0; 16]),
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: vec![0xab; 32],
            sealed_epoch_keys: Vec::new(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: OwnerAddr([0xd0; 16]),
        community_name: "pin".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: None,
        untargeted_decrypt_key: None,
    };
    let bytes = canonical_cbor_encode(&payload).expect("encode");
    let expected_hex = "a562636950c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0626573a36265700062736b5820abababababababababababababababababababababababababababababababab627373a2626d62a062706ca062616450d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0626e6d6370696e62696ff4";
    let expected = hex::decode(expected_hex).unwrap_or_else(|_| {
        eprintln!(
            "\nACTUAL bytes for pinning (copy into expected_hex above):\n  {}\n",
            hex::encode(&bytes)
        );
        panic!("update PLACEHOLDER with the bytes above");
    });
    assert_eq!(
        bytes,
        expected,
        "drifted: {} vs {}",
        hex::encode(&bytes),
        hex::encode(&expected)
    );
}
