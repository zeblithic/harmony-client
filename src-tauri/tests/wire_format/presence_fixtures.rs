//! ZEB-537: pinned wire format for the community-presence beacon. Mirrors
//! voice_fixtures.rs. Guards against silent CBOR/struct drift.
#![cfg(feature = "test-fixtures")]

use harmony_app::community_channel_log::derive_presence_key;
use harmony_app::community_presence::{
    open_presence_beacon, seal_presence_beacon_with_nonce, sign_presence_beacon, PresenceBeacon,
    SignedPresenceBeacon,
};
use harmony_app::owner_state_types::{EpochKey, Hlc, SpaceId};

fn fixture_signed_beacon() -> SignedPresenceBeacon {
    let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
    let beacon = PresenceBeacon {
        owner: [0xa1; 16],
        device: sk.verifying_key().to_bytes(),
        started_hlc: Hlc {
            wall_ms: 1000,
            logical: 0,
            device_id: "aa".repeat(32),
        },
        seq: 1,
    };
    sign_presence_beacon(beacon, &sk).expect("sign")
}

#[test]
fn presence_beacon_wire_bytes_pinned() {
    let c = SpaceId([0xc0; 16]);
    let key = derive_presence_key(&EpochKey::new([0x11; 32]), &c);
    let signed = fixture_signed_beacon();
    let sealed = seal_presence_beacon_with_nonce(&key, &c, &signed, [0u8; 12]).expect("seal");
    let actual = hex::encode(&sealed);
    let expected = "000000000000000000000000042dcb7345f622a63bb0f3459c961f11893da2777fcaf08aafb6ce77118f685b74ea58d9bfbd0f55d825d4957d825d1d90043ea506958572a088be922800b5d98b6e9081bec5c58343556e39b8ce5d57852cb8e47ddbc0e4f8a00f3fd7b92db56058108297f63cbfd72a5f50771c19348dd572d3ee8572e4bd5880d957b5711ea4f0af93ce3e8489a1021580b6612716c669c4489aef30be5101cf905c63864e8720e34a7a565ca677c2faf933d43e1a6bdfb726146beec51bdb03d03d7e03073ee3cc0965da83bdfcf45694fe6d7016c87849d86c476710f1fb0ec04ae7fe33b600106abb8198";
    assert_eq!(
        actual, expected,
        "sealed presence-beacon wire format drifted"
    );

    let opened = open_presence_beacon(&key, &c, &sealed).expect("open pinned beacon");
    assert_eq!(opened, signed, "pinned beacon decoded to a different value");
}
