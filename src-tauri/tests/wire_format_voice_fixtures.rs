//! ZEB-350 Voice V2 wire-format pins. Locks the sealed voice-packet framing
//! (and the signed+sealed presence beacon). A drift here means
//! the on-the-wire format changed — bump the version domain and re-pin
//! deliberately, never silently.
#![cfg(feature = "test-fixtures")]

use harmony_app::community_channel_log::derive_channel_key;
use harmony_app::community_membership::ChannelId;
use harmony_app::owner_state_types::{EpochKey, Hlc, SpaceId};
use harmony_app::voice_crypto::{encrypt_voice_packet_with_nonce, VOICE_PACKET_AAD};
use harmony_app::voice_presence::{
    open_presence_beacon, seal_presence_beacon_with_nonce, sign_presence_beacon,
    SignedVoicePresenceBeacon, VoicePresenceBeacon,
};

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

/// Fully deterministic signed+sealed presence beacon: fixed device-#2 key
/// `[7u8; 32]`, fixed `joined_hlc`, `seq = 1`, sealed with a zeroed nonce.
fn fixture_signed_beacon() -> SignedVoicePresenceBeacon {
    let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
    let beacon = VoicePresenceBeacon {
        owner: [0xa1; 16],
        device: sk.verifying_key().to_bytes(),
        muted: true,
        joined_hlc: Hlc {
            wall_ms: 1000,
            logical: 0,
            device_id: "aa".repeat(32),
        },
        seq: 1,
        left: false,
    };
    sign_presence_beacon(beacon, &sk).expect("sign")
}

#[test]
fn presence_beacon_wire_bytes_pinned() {
    let key = derive_channel_key(
        &EpochKey::new([0x11; 32]),
        &SpaceId([0xc0; 16]),
        &ChannelId([0xc1; 16]),
    );
    let signed = fixture_signed_beacon();
    let sealed = seal_presence_beacon_with_nonce(
        &key,
        &SpaceId([0xc0; 16]),
        &ChannelId([0xc1; 16]),
        &signed,
        [0u8; 12],
    )
    .expect("seal");
    let actual = hex::encode(&sealed);
    let expected = "0000000000000000000000000e3878f4aa6cc7facd295c54d9b2ead07b7da6190b227548eee70d1a3bfb1a62432a5f6b46b27fa0a1a1cf8ea5ff27cf90893c2046dcbd473b2f4a6e69cc3094336f6cd54117eb973e6c9e1c18c23fccdff02a868b284897577a96163281e6eddc67c2a47da57b2b938ef9bbbdadc266f939bf5e5a54614313b1d70941bb27261068d5070606039a01f2c5308184bddf60ea0684e9db40867cc001209223b99ba529ab5980077bbd429679a05471ea51da8cdf57754623a2f68c60093d5750ea241e18297557bcd5ab8464431dfac8173db8a49d2fc414d4022287148b61d9f4297c5353c37b9deefe2910";
    assert_eq!(
        actual, expected,
        "sealed presence-beacon wire format drifted"
    );

    // Back-compat decode: the pinned bytes must still open to the expected
    // beacon (guards against silent struct/field drift).
    let opened = open_presence_beacon(&key, &SpaceId([0xc0; 16]), &ChannelId([0xc1; 16]), &sealed)
        .expect("open pinned beacon");
    assert_eq!(opened, signed, "pinned beacon decoded to a different value");
}
