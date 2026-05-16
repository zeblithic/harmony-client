//! ZEB-285 Phase 1: wire-format pinning for the Fork variant +
//! PreForkSnapshot + CommunityInvitePayload fork-extension fields.
//!
//! These tests lock the canonical-CBOR wire encoding for the new ZEB-285
//! types. Any failure here is a wire-protocol break — review carefully
//! before updating the pinned bytes (cross-version compat, peer interop).
//!
//! Uses deterministic test bytes (zero or repeated-byte sigs) so the
//! encoded bytes are byte-stable across runs. The tests do NOT verify
//! cryptographic validity — they pin BYTE LAYOUT only.

use harmony_app::community_invite::{
    BoundedChannelLogSnapshot, CommunityInvitePayload, InviteEpochSnapshot,
    MaterializedCommunityState, PreForkSnapshot,
};
use harmony_app::community_membership::{MembershipEventKind, SignedMembershipEvent};
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};
use std::collections::BTreeMap;

fn fixture_hlc() -> Hlc {
    Hlc {
        wall_ms: 1_700_000_000_000,
        logical: 0,
        device_id: "test-device".to_string(),
    }
}

/// Construct a SignedMembershipEvent with deterministic zero-byte fields.
/// The sig is all-0xBB (not a valid signature — wire-format pin only).
fn fixture_signed_event_zeb285(kind: MembershipEventKind) -> SignedMembershipEvent {
    SignedMembershipEvent {
        id: [0x42; 16],
        community_id: SpaceId([0xc0; 16]),
        kind,
        actor: OwnerAddr([0xaa; 16]),
        at: fixture_hlc(),
        sig: [0xBB; 64],
        countersig: None,
    }
}

/// Fixture 1: pins the canonical-CBOR bytes of a signed Fork event.
///
/// Wire-format drift (renamed field, changed serde attribute, variant
/// tag change) will cause this test to fail — that is intentional.
/// Re-pin ONLY after a deliberate, reviewed wire-format change.
#[test]
fn fork_event_canonical_cbor_pinned() {
    let fork_space_id = SpaceId([0xfa; 16]);
    let signed = fixture_signed_event_zeb285(MembershipEventKind::Fork { fork_space_id });

    let bytes = canonical_cbor_encode(&signed).expect("encode");
    let hex = hex::encode(&bytes);
    eprintln!("fork_event_canonical_cbor_pinned hex: {hex}");

    let expected_hex = "a6626964504242424242424242424242424242424262636950c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0626b6ea2627467617862766ca162667350fafafafafafafafafafafafafafafafa62616350aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa626174a361771b0000018bcfe56800616c0061646b746573742d6465766963656273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    assert_eq!(hex, expected_hex, "Fork event wire format changed");
}

/// Fixture 2: pins the canonical-CBOR bytes of a minimal PreForkSnapshot.
///
/// Empty membership_events, empty channel_log, empty identity_pubs —
/// minimal but exercises every field's serde attribute.
#[test]
fn pre_fork_snapshot_canonical_cbor_pinned() {
    let snapshot = PreForkSnapshot {
        original_community_id: SpaceId([0xa0; 16]),
        original_community_name: "Pinned".to_string(),
        membership_events: vec![],
        channel_log: BoundedChannelLogSnapshot::default(),
        identity_pubs: BTreeMap::new(),
        forked_at: fixture_hlc(),
        // ZEB-287 Phase 2: empty lineage → skip-if-empty drops `pl` key,
        // preserving Phase 1 byte-identity for this fixture.
        parent_lineage: Vec::new(),
    };

    let bytes = canonical_cbor_encode(&snapshot).expect("encode");
    let hex = hex::encode(&bytes);
    eprintln!("pre_fork_snapshot_canonical_cbor_pinned hex: {hex}");

    let expected_hex = "a6626f6950a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0626f6e6650696e6e65646265768062636ca1627063a0626970a0627473a361771b0000018bcfe56800616c0061646b746573742d646576696365";
    assert_eq!(hex, expected_hex, "PreForkSnapshot wire format changed");
}

/// Fixture 3: pins a CommunityInvitePayload with both fork extension
/// fields set (forked_from + pre_fork_snapshot). Catches drift in the
/// "ff"/"fs" serde attributes or their skip_serializing_if logic.
#[test]
fn community_invite_with_fork_fields_pinned() {
    let mut identity_pubs: BTreeMap<OwnerAddr, [u8; 64]> = BTreeMap::new();
    identity_pubs.insert(OwnerAddr([0xaa; 16]), [0u8; 64]);

    let snapshot = PreForkSnapshot {
        original_community_id: SpaceId([0xa0; 16]),
        original_community_name: "Original".to_string(),
        membership_events: vec![],
        channel_log: BoundedChannelLogSnapshot::default(),
        identity_pubs,
        forked_at: fixture_hlc(),
        // ZEB-287 Phase 2: empty lineage → skip-if-empty preserves Phase 1
        // byte-identity for this fixture.
        parent_lineage: Vec::new(),
    };

    let payload = CommunityInvitePayload {
        community_id: SpaceId([0xc0; 16]),
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: EpochKey::new([0xAA; 32]).as_bytes().to_vec(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: OwnerAddr([0xaa; 16]),
        community_name: "Pinned Fork".to_string(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: None,
        forked_from: Some(SpaceId([0xa0; 16])),
        pre_fork_snapshot: Some(snapshot),
    };

    let bytes = canonical_cbor_encode(&payload).expect("encode");
    let hex = hex::encode(&bytes);
    eprintln!("community_invite_with_fork_fields_pinned hex: {hex}");

    let expected_hex = "a762636950c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0626573a36265700062736b5820aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa627373a2626d62a062706ca062616450aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa626e6d6b50696e6e656420466f726b62696ff462666650a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0626673a6626f6950a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0626f6e684f726967696e616c6265768062636ca1627063a0626970a150aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa584000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000627473a361771b0000018bcfe56800616c0061646b746573742d646576696365";
    assert_eq!(
        hex, expected_hex,
        "CommunityInvitePayload with fork fields wire format changed"
    );
}
