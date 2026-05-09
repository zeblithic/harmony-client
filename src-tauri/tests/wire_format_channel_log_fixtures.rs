//! ZEB-269: canonical-CBOR pin tests for SignedChannelEvent.
//!
//! Any field-order change, key rename, or encoding shift in
//! SignedChannelEvent::Post will deliberately break this pin. If the
//! wire format genuinely needs to change, regenerate the hex via a
//! temporary `eprintln!("{}", hex::encode(&bytes));` and paste the
//! captured value into the assertion below.

use harmony_app::community_channel_log::{
    sign_channel_event, ChannelPostPayload, SignedChannelEvent,
};
use harmony_app::community_membership::ChannelId;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

fn fixture() -> SignedChannelEvent {
    let key = ed25519_dalek::SigningKey::from_bytes(&[0xa1; 32]);
    let payload = ChannelPostPayload {
        id: [0x11; 16],
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
    let expected_hex = "a2627467617062766ca8626964901111111111111111111111111111111162636950c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0626368500101010101010101010101010101010162617550a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1626174a361771a000186a0616c00616465612d646576626b64006262646568656c6c6f627367584052a3766f6f7ed326f3ead313176d35d61203641539bb31f55f550f838641273bff4b844193b85566e4b2cfa28cdd957488a07089833c3e004968890982263707";
    assert_eq!(hex::encode(&bytes), expected_hex);
}
