//! ZEB-350 Voice V2 wire-format pins. Locks the sealed voice-packet framing
//! (and, in Task 5, the signed+sealed presence beacon). A drift here means
//! the on-the-wire format changed — bump the version domain and re-pin
//! deliberately, never silently.
#![cfg(feature = "test-fixtures")]

use harmony_app::community_channel_log::derive_channel_key;
use harmony_app::community_membership::ChannelId;
use harmony_app::owner_state_types::{EpochKey, SpaceId};
use harmony_app::voice_crypto::{encrypt_voice_packet_with_nonce, VOICE_PACKET_AAD};

#[test]
fn voice_packet_wire_bytes_pinned() {
    let key = derive_channel_key(
        &EpochKey::new([0x11; 32]),
        &SpaceId([0xc0; 16]),
        &ChannelId([0xc1; 16]),
    );
    // 23-byte header (flags|seq|ts|senderHash) + a short opus payload, all zeros
    // except markers — the relay seals the whole frame opaquely.
    let frame: Vec<u8> = (0u8..30).collect();
    let sealed = encrypt_voice_packet_with_nonce(
        &key,
        &SpaceId([0xc0; 16]),
        &ChannelId([0xc1; 16]),
        VOICE_PACKET_AAD,
        &frame,
        [0u8; 12],
    )
    .expect("seal");
    let actual = hex::encode(&sealed);
    let expected = "000000000000000000000000ac5b18940b0bae8a9581f7fe741e457ecacd15abbe96c2fe579c73777fc6b4542adb6477613e68651bd4237c586c";
    assert_eq!(actual, expected, "sealed voice-packet wire format drifted");
}
