//! ZEB-269: canonical-CBOR pin tests for SignedChannelEvent.
//!
//! Any field-order change, key rename, or encoding shift in
//! SignedChannelEvent::Post will deliberately break this pin. If the
//! wire format genuinely needs to change, regenerate the hex via a
//! temporary `eprintln!("{}", hex::encode(&bytes));` and paste the
//! captured value into the assertion below.

use harmony_app::community_channel_log::{
    sign_channel_event, ChannelPostPayload, MessageId, SignedChannelEvent,
};
use harmony_app::community_membership::ChannelId;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

fn fixture() -> SignedChannelEvent {
    let key = ed25519_dalek::SigningKey::from_bytes(&[0xa1; 32]);
    let payload = ChannelPostPayload {
        id: MessageId([0x11; 16]),
        community_id: SpaceId([0xc0; 16]),
        channel_id: ChannelId([0x01; 16]),
        author: OwnerAddr([0xa1; 16]),
        at: Hlc {
            wall_ms: 100_000,
            logical: 0,
            device_id: "a-dev".to_string(),
        },
        content_kind: 0,
        body: "hello",
        reply_to: None,
    };
    sign_channel_event(&payload, &key).expect("sign")
}

#[test]
fn signed_channel_event_post_wire_bytes_pinned() {
    let event = fixture();
    let mut bytes = Vec::new();
    ciborium::into_writer(&event, &mut bytes).expect("encode");
    // Pin the byte sequence. If this fails after intentional schema
    // change, regenerate via temporary `eprintln!("{}", hex::encode(&bytes));`.
    //
    // Field order in this hex matches RFC 8949 §4.2.1 canonical CBOR
    // ordering for our 2-char keys (bytewise lexicographic):
    // at, au, bd, ch, ci, id, kd, (rt skipped because None), sg.
    // ciborium emits in declaration order, so the SignedChannelEvent::Post
    // and ChannelPostSignedSet declarations are arranged to match.
    let expected_hex = "a2627467617062766ca8626174a361771a000186a0616c00616465612d64657662617550a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a16262646568656c6c6f626368500101010101010101010101010101010162636950c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c06269645011111111111111111111111111111111626b64006273675840f5744983df7ff9ca05b964fd16cb63a253267e2c56eb59fd0b4ec3326492441d1085686b783c437b12df404bceb47f1e012257ac9aba780399d3add6cb8b200a";
    assert_eq!(hex::encode(&bytes), expected_hex);
}
