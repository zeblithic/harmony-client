//! Pinned-byte CBOR wire-format fixtures for community-sync types.
//! ZEB-256: envelope gained `publisher_addr` (bstr(16)) + `publisher_sig`
//! (bstr(64)). Old pinned bytes are wholly invalidated; this regen
//! commit IS the deliberate update. Mirrors community-membership wire
//! fixtures — locking the encoded bytes prevents silent wire-form drift
//! across phases.

use harmony_app::community_state_sync::{CommunityRootPublishPayload, CommunityRootSignedPayload};
use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use harmony_app::owner_state_types::{Hlc, OwnerAddr};
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
