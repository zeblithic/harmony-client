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
    let expected_hex = "a2627467617062766ca8626964501111111111111111111111111111111162636950c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0626368500101010101010101010101010101010162617550a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1626174a361771a000186a0616c00616465612d646576626b64006262646568656c6c6f6273675840d5ea4aa5c07c79f626575bfd7e5608b32a8681ed0a53ed8e658996456a8184d86b82cbe1ea92a183ea04fa67b02cbf8d9bbf162bfb5de715bbe6fc6e27cdf30e";
    assert_eq!(hex::encode(&bytes), expected_hex);
}
