//! Pinned-byte CBOR wire-format fixtures for community-sync types.
//! Mirrors src-tauri/tests/wire_format_community_fixtures.rs from
//! Phase 1 — locking the encoded bytes of new types prevents silent
//! wire-form drift across phases.

use harmony_app::community_state_sync::CommunityRootPublishPayload;
use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use harmony_app::owner_state_types::Hlc;
use harmony_content::cid::ContentId;

#[test]
fn community_root_publish_payload_wire_bytes_pinned() {
    // ContentId is 32 bytes (4-byte header + 28-byte hash). For the
    // fixture we use a fully synthetic byte pattern — `from_bytes` is
    // the standard test constructor.
    let cid = ContentId::from_bytes([0xAA; 32]);
    let p = CommunityRootPublishPayload {
        root_cid: cid,
        at: Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 7,
            device_id: "d1".into(),
        },
    };

    let bytes = canonical_cbor_encode(&p).expect("encode");
    // Lock the byte sequence — any structural change to the wire
    // form (field codes, encoding order, ContentId byte layout)
    // will require this fixture to update intentionally.
    let expected = hex::decode(
        "a26272635820aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa626174a361771b0000018bcfe56800616c076164626431",
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
