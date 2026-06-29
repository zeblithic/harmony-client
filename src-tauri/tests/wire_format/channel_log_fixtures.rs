//! ZEB-269: canonical-CBOR pin tests for SignedChannelEvent.
//! ZEB-270 Phase 3 §14.3: backfill-reply (= live-broadcast) packet pin.
//!
//! Any field-order change, key rename, or encoding shift in
//! SignedChannelEvent::Post will deliberately break the inner pin. If the
//! wire format genuinely needs to change, regenerate the hex via a
//! temporary `eprintln!("{}", hex::encode(&bytes));` and paste the
//! captured value into the assertion below.
//!
//! ## Re-pinning the backfill-reply (encrypted-packet) test
//!
//! The `backfill_reply_packet_wire_bytes_pinned` test pins the bytes of
//! a fully-encrypted `ChannelKey`-wrapped `SignedChannelEvent::Post`.
//! It uses `encrypt_channel_packet_with_nonce` (a `#[doc(hidden)]`
//! deterministic-nonce variant of the production random-nonce
//! `encrypt_channel_packet`) so a fixed input deterministically
//! produces a fixed output.
//!
//! To regenerate the hex after an intentional schema change:
//!
//! 1. `UPDATE_BACKFILL_FIXTURE=1 cargo test --test wire_format_tests \
//!     backfill_reply_packet_wire_bytes_pinned -- --nocapture`
//! 2. Copy the hex printed to stderr.
//! 3. Replace the `expected_hex` literal in the assertion below.
//! 4. Re-run without the env var to confirm the pin holds.

use harmony_app::channel_rbsr::{
    encode_message, RbsrMessage, RbsrMode, RbsrRange, RBSR_PROTOCOL_VERSION,
};
use harmony_app::community_channel_log::{
    sign_channel_event, sign_channel_react, ChannelAttachment, ChannelPostPayload,
    ChannelReactPayload, MessageId, SignedChannelEvent,
};
use harmony_app::community_membership::ChannelId;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

// `encrypt_channel_packet_with_nonce` is a deterministic-nonce variant
// of the production AEAD helper; it is `#[cfg]`-gated to
// `any(test, feature = "test-fixtures")` because random-nonce reuse
// under ChaCha20-Poly1305 is catastrophic. Integration tests compile
// against the crate's public API and cannot see `#[cfg(test)]`-only
// items, so the `test-fixtures` feature is what makes this import
// resolvable here. CI enables the feature; local `cargo test` without
// `--features test-fixtures` skips just the backfill-packet pin and
// leaves the SignedChannelEvent CBOR pin running.
#[cfg(feature = "test-fixtures")]
use harmony_app::community_channel_log::{
    derive_channel_key, encrypt_channel_packet_with_nonce, seal_rbsr_message_with_nonce,
    seal_watermark_vector_with_nonce, WatermarkVector,
};
#[cfg(feature = "test-fixtures")]
use harmony_app::owner_state_types::EpochKey;

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
        mentions: None,
        attachments: None,
    };
    sign_channel_event(&payload, &key).expect("sign")
}

fn fixture_with_mentions() -> SignedChannelEvent {
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
        mentions: Some(vec![OwnerAddr([0xb2; 16]), OwnerAddr([0xc3; 16])]),
        attachments: None,
    };
    sign_channel_event(&payload, &key).expect("sign")
}

// ZEB-535: a populated-`attachments` post. Mirrors `fixture_with_mentions`'s
// deterministic seeds; the only difference is `attachments: Some(..)` (one
// `ChannelAttachment`) and `mentions: None`, so the new wire pin isolates the
// `pa` key's encoding.
fn fixture_with_attachments() -> SignedChannelEvent {
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
        body: "see log",
        reply_to: None,
        mentions: None,
        attachments: Some(vec![ChannelAttachment {
            cid: [0xb2; 32],
            mime: "text/plain".to_string(),
            name: "log.txt".to_string(),
            size: 42,
        }]),
    };
    sign_channel_event(&payload, &key).expect("sign")
}

#[test]
fn signed_channel_event_post_with_attachments_wire_bytes_pinned() {
    let event = fixture_with_attachments();
    let mut bytes = Vec::new();
    ciborium::into_writer(&event, &mut bytes).expect("encode");
    // ZEB-535: the `pa` array sits between kd and rt (skipped) / sg. To
    // regenerate this hex, temporarily uncomment the eprintln below, run with
    // --nocapture, paste the printed value, then re-comment the eprintln.
    // eprintln!("WITH_ATTACHMENTS: {}", hex::encode(&bytes));
    let expected_hex = "a2627467617062766ca9626174a361771a000186a0616c00616465612d64657662617550a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a162626467736565206c6f67626368500101010101010101010101010101010162636950c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c06269645011111111111111111111111111111111626b640062706181a46263645820b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2626d696a746578742f706c61696e626e6d676c6f672e74787462737a182a62736758404143bba35a43ca437f0faa59d53039dd5465dacd22e0550fcac06acc41aeabacd3239a20ae12d31c0fe4b091a4dbdba1aef0359e078432ec8ec1277d92139603";
    assert_eq!(hex::encode(&bytes), expected_hex);
}

#[test]
fn signed_channel_event_post_with_mentions_wire_bytes_pinned() {
    let event = fixture_with_mentions();
    let mut bytes = Vec::new();
    ciborium::into_writer(&event, &mut bytes).expect("encode");
    // Field order: at, au, bd, ch, ci, id, kd, mn, rt (skipped), sg.
    // The `mn` array sits between kd and sg. To regenerate this hex,
    // temporarily uncomment the eprintln below, run with --nocapture,
    // paste the printed value, then re-comment the eprintln.
    // eprintln!("WITH_MENTIONS: {}", hex::encode(&bytes));
    let expected_hex = "a2627467617062766ca9626174a361771a000186a0616c00616465612d64657662617550a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a16262646568656c6c6f626368500101010101010101010101010101010162636950c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c06269645011111111111111111111111111111111626b6400626d6e8250b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b250c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c36273675840ff85e0f77f2ae27494afadf68bfdaa99564680f84fe0a26dbe31027dcec6e04d10dfd494ccc2337a965d38e2675ca88164750baee5c0c556d02ddb6f593db308";
    assert_eq!(hex::encode(&bytes), expected_hex);
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

/// Per spec §17.1: backfill-query replies are per-event packets,
/// wire-identical to live-broadcast packets. This pin asserts the
/// encrypted packet format (12B nonce || ChaCha20-Poly1305 ciphertext)
/// is byte-stable under fixed inputs (signing key, identity, HLC, body,
/// channel key, nonce). Drift-guards the format against silent changes
/// from Phase 4 / later work.
///
/// Gated on `test-fixtures` because it calls
/// `encrypt_channel_packet_with_nonce` — see the import block above
/// for the nonce-reuse rationale. CI enables the feature.
///
/// Re-pin procedure documented in the file's header comment.
#[cfg(feature = "test-fixtures")]
#[test]
fn backfill_reply_packet_wire_bytes_pinned() {
    // Deterministic seeds — match the existing wire-format test
    // fixture's conventions for SignedChannelEvent::Post.
    let community_id = SpaceId([0xc0; 16]);
    let channel_id = ChannelId([0x01; 16]);
    let owner = OwnerAddr([0xa1; 16]);
    let mk = EpochKey::new([0x77; 32]);
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0xa1; 32]);
    let key = derive_channel_key(&mk, &community_id, &channel_id);

    let payload = ChannelPostPayload {
        id: MessageId([0x11; 16]),
        community_id,
        channel_id,
        author: owner,
        at: Hlc {
            wall_ms: 100_000,
            logical: 0,
            device_id: "a-dev".to_string(),
        },
        content_kind: 0,
        body: "hello",
        reply_to: None,
        mentions: None,
        attachments: None,
    };
    let event: SignedChannelEvent = sign_channel_event(&payload, &signing_key).expect("sign");

    // Fixed nonce — `encrypt_channel_packet` randomizes per call, but
    // `encrypt_channel_packet_with_nonce` accepts a caller-supplied
    // nonce. Identical AEAD path; identical output bytes for fixed
    // inputs.
    let packet = encrypt_channel_packet_with_nonce(&key, &event, [0u8; 12]).expect("encrypt");
    let actual_hex = hex::encode(&packet);

    // Allow `UPDATE_BACKFILL_FIXTURE=1` to print the hex for re-pin
    // bootstrapping (see file header for the procedure).
    if std::env::var("UPDATE_BACKFILL_FIXTURE").is_ok() {
        eprintln!("UPDATE_BACKFILL_FIXTURE: {actual_hex}");
    }

    let expected_hex = "0000000000000000000000009e1346ff7948ffc988d513ef82108308da9ed03dedb25bffdc757f43faa47092cd8f034cc228ce8d5d7532cbdc4fc9225cdb08b0cc8c96bb87e5bcf3d52ceb6543820356e6b99085d91163604e22848c34fdfd278358f9ad5e86e28d922c6b664b7abe4e8c231604aee983121be43a0490773740768b0be22630bca89b113889d8cf76ffd72fafc2ed1353617c14c1b137a8a21223dac93af5e6c9df9e16115c462030f52c224772429a60dcd6cd8cffc7692dfe98290ea5d54de848c5b577deba8e856096f7b34c7b483873647aa87ca7f4";
    assert_eq!(
        actual_hex, expected_hex,
        "backfill reply wire format drifted; re-pin via \
         UPDATE_BACKFILL_FIXTURE=1 (see file header for procedure)"
    );
}

/// ZEB-585: wire-format pin for a sealed `WatermarkVector`. Asserts the
/// AEAD-sealed catch-up watermark payload is byte-stable under fixed
/// inputs (channel key + deterministic nonce + a 2-lane vector keyed by
/// `(author, device)`) so a future CBOR/AAD/nonce-layout change is caught
/// immediately. Mirrors `backfill_reply_packet_wire_bytes_pinned`; re-pin
/// the same way (`UPDATE_BACKFILL_FIXTURE=1`).
#[cfg(feature = "test-fixtures")]
#[test]
fn watermark_vector_sealed_wire_bytes_pinned() {
    let community_id = SpaceId([0xc0; 16]);
    let channel_id = ChannelId([0x01; 16]);
    let mk = EpochKey::new([0x77; 32]);
    let key = derive_channel_key(&mk, &community_id, &channel_id);

    let mut v: WatermarkVector = WatermarkVector::new();
    v.insert((OwnerAddr([0xa1; 16]), "a-dev".to_string()), (100_000, 0));
    v.insert((OwnerAddr([0xb2; 16]), "b-dev".to_string()), (250_000, 7));
    let sealed = seal_watermark_vector_with_nonce(&key, &v, [0u8; 12]).expect("seal");
    let actual_hex = hex::encode(&sealed);

    if std::env::var("UPDATE_BACKFILL_FIXTURE").is_ok() {
        eprintln!("UPDATE_WMV_FIXTURE: {actual_hex}");
    }

    let expected_hex = "0000000000000000000000009ef36239b9993c1e45dcd02f571243de613f70de2cfe539acb930022d646b5e42dbec4aed13bdd9e4e6621d8cf5cda314fc8cc73838a97a86d9ad09c6ade8ff67ad93fcc41ee712754759ad15d4636";
    assert_eq!(
        actual_hex, expected_hex,
        "watermark-vector wire format drifted; re-pin via \
         UPDATE_BACKFILL_FIXTURE=1 (see file header for procedure)"
    );
}

/// ZEB-592: a fixed RBSR message for the wire-format pins below.
fn fixed_rbsr_message() -> RbsrMessage {
    let k = |w: u64, b: u8| (w, 0u32, "dev".to_string(), [b; 32]);
    RbsrMessage {
        version: RBSR_PROTOCOL_VERSION,
        ranges: vec![
            RbsrRange {
                upper: k(100, 1),
                mode: RbsrMode::Skip,
            },
            RbsrRange {
                upper: k(200, 2),
                mode: RbsrMode::Fingerprint([7u8; 16]),
            },
            RbsrRange {
                upper: k(300, 3),
                mode: RbsrMode::Have(vec![k(210, 9), k(220, 9)]),
            },
        ],
    }
}

/// ZEB-592: canonical-CBOR pin for an RBSR message. Locks the wire encoding
/// against a field reorder / serde-rename. Re-pin via UPDATE_BACKFILL_FIXTURE=1.
#[test]
fn rbsr_message_canonical_cbor_pinned() {
    let actual_hex = hex::encode(encode_message(&fixed_rbsr_message()));
    if std::env::var("UPDATE_BACKFILL_FIXTURE").is_ok() {
        eprintln!("UPDATE_RBSR_CBOR: {actual_hex}");
    }
    let expected_hex = "a2617601617283a26175841864006364657698200101010101010101010101010101010101010101010101010101010101010101616d6173a261758418c8006364657698200202020202020202020202020202020202020202020202020202020202020202616da161669007070707070707070707070707070707a261758419012c006364657698200303030303030303030303030303030303030303030303030303030303030303616da16168828418d20063646576982009090909090909090909090909090909090909090909090909090909090909098418dc006364657698200909090909090909090909090909090909090909090909090909090909090909";
    assert_eq!(
        actual_hex, expected_hex,
        "RBSR message CBOR drifted; re-pin via UPDATE_BACKFILL_FIXTURE=1"
    );
}

/// ZEB-592: AEAD-sealed wire pin for an RBSR message (deterministic nonce).
#[cfg(feature = "test-fixtures")]
#[test]
fn rbsr_sealed_wire_bytes_pinned() {
    let community_id = SpaceId([0xc0; 16]);
    let channel_id = ChannelId([0x01; 16]);
    let mk = EpochKey::new([0x77; 32]);
    let key = derive_channel_key(&mk, &community_id, &channel_id);
    let sealed =
        seal_rbsr_message_with_nonce(&key, &fixed_rbsr_message(), [0u8; 12]).expect("seal");
    let actual_hex = hex::encode(&sealed);
    if std::env::var("UPDATE_BACKFILL_FIXTURE").is_ok() {
        eprintln!("UPDATE_RBSR_SEALED: {actual_hex}");
    }
    let expected_hex = "0000000000000000000000009e104499794a1e1d8508f59692b3811ba5e8499b4cd236febc101b23d6c114e5aeef771d62886e2dfdd5926b7cef6982fc7ba810cf8393ad4de1a51ba286886e77e674cfc7ba9386da1260634d21878f379d9c4cd19a3b6f9c44204f50eea9a489b87c8c4e2012c19f9e02040df22c1286612156609d1df43055d06efa06cff781a383e8fac9062534da55db1bead4795709f2375ef59cd2af18c1925e2776cd013e2373474a3c4d02801fa2f7eec0cfa0a52cbfabe0e2878d477892616ab55c938d0c1aaafdfd55e6a462d3e02333d061984044f1274249481a075a559260907e523e82f952097d8370fc652f45073e767bbdfef9e46bc9e01874d251e1ed27dda30770756bc24c16163445e70044b55ec898";
    assert_eq!(
        actual_hex, expected_hex,
        "RBSR sealed wire format drifted; re-pin via UPDATE_BACKFILL_FIXTURE=1"
    );
}

/// ZEB-536: wire-format pin for a React packet. Ensures that
/// `sign_channel_react` + `encrypt_channel_packet_with_nonce` produce
/// byte-stable output under fixed inputs so a future field reorder
/// or key-rename is caught immediately.
#[cfg(feature = "test-fixtures")]
#[test]
fn react_packet_is_byte_stable() {
    let community_id = SpaceId([0xc0; 16]);
    let channel_id = ChannelId([0x01; 16]);
    let owner = OwnerAddr([0xa1; 16]);
    let mk = EpochKey::new([0x77; 32]);
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0xa1; 32]);
    let key = derive_channel_key(&mk, &community_id, &channel_id);

    let payload = ChannelReactPayload {
        target: MessageId([0x07; 16]),
        community_id,
        channel_id,
        author: owner,
        at: Hlc {
            wall_ms: 100_000,
            logical: 0,
            device_id: "a-dev".to_string(),
        },
        // ZEB-541: a unicode reaction carries no custom-emoji descriptor. The
        // `skip_serializing_if`/`default` on `ea` keeps these bytes identical
        // to the pre-feature React — this fixture is the additive-field canary.
        emoji_attachment: None,
        emoji: "👍".to_string(),
        add: true,
    };
    let event = sign_channel_react(&payload, &signing_key).expect("sign react");
    let packet = encrypt_channel_packet_with_nonce(&key, &event, [0x11; 12]).expect("encrypt");
    let decoded =
        harmony_app::community_channel_log::decrypt_channel_packet(&key, &packet).expect("decrypt");
    assert_eq!(decoded, event, "react packet must round-trip");
    let actual_hex = hex::encode(&packet);
    let expected_hex = "1111111111111111111111119618335845c3f3e24629060f5af3f24bd4331d0747e919016e09d1472032a8eab95676ee115eb12d80766a500a3f69625469e13cc8a46734d7169d786378966ca04448604e8c3677404dcf27098557528ad90067215217db4e6ecf3d7e188e54c3432c2f9ca42d2991171d07b220c2b858204148fea1507c92acf5ccf5f8d6ce3a300b3a030607747180964c63b7751e222e46326772edb982a1ebd1b94811d27501bcd6484d927fd472dab74bd447f76923206c109044f66cfaf1f3fd66320a6cd26cfd60d95c7c33d4ad359988c7599a";
    assert_eq!(actual_hex, expected_hex, "react packet wire format drifted");
}

/// ZEB-541: wire-format pin for a React packet carrying a custom-emoji CAS
/// descriptor (`ea`). Mirrors `react_packet_is_byte_stable`'s deterministic
/// seeds + fixed nonce, the only difference being `emoji_attachment: Some(..)`,
/// so this pin isolates the additive `ea` key's encoding (it sorts between `ci`
/// and `em`). To regenerate the hex after an intentional schema change, run with
/// `UPDATE_BACKFILL_FIXTURE=1` (see file header) and paste the printed value.
#[cfg(feature = "test-fixtures")]
#[test]
fn react_packet_with_emoji_attachment_is_byte_stable() {
    let community_id = SpaceId([0xc0; 16]);
    let channel_id = ChannelId([0x01; 16]);
    let owner = OwnerAddr([0xa1; 16]);
    let mk = EpochKey::new([0x77; 32]);
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0xa1; 32]);
    let key = derive_channel_key(&mk, &community_id, &channel_id);

    let payload = ChannelReactPayload {
        target: MessageId([0x07; 16]),
        community_id,
        channel_id,
        author: owner,
        at: Hlc {
            wall_ms: 100_000,
            logical: 0,
            device_id: "a-dev".to_string(),
        },
        emoji_attachment: Some(ChannelAttachment {
            cid: [0xB2; 32],
            mime: "image/png".to_string(),
            name: String::new(),
            size: 1024,
        }),
        // A custom-emoji React may carry an empty unicode grouping key.
        emoji: String::new(),
        add: true,
    };
    let event = sign_channel_react(&payload, &signing_key).expect("sign react");
    let packet = encrypt_channel_packet_with_nonce(&key, &event, [0x11; 12]).expect("encrypt");
    let decoded =
        harmony_app::community_channel_log::decrypt_channel_packet(&key, &packet).expect("decrypt");
    assert_eq!(decoded, event, "custom-emoji react packet must round-trip");
    let actual_hex = hex::encode(&packet);
    if std::env::var("UPDATE_BACKFILL_FIXTURE").is_ok() {
        eprintln!("UPDATE_BACKFILL_FIXTURE (custom emoji react): {actual_hex}");
    }
    let expected_hex = "1111111111111111111111119618335845c3f3e24628060f5af3f24bd4331d0747e919016e09d1472032a8eab95676ee115eb12d80766a500a3f69625469e13cc8a46734d7169d786378966ca04448604e8c3677404dcf27098557528ad90067215217db4e6ecf3d7e188e54c34320ef0e58d8fcd3cccbe50795770ded95f4fd4b14e5c92719407925390324c89951aaa4817682f44a71b7abd8e6c000451247e7dfa6742bcc2f2dd615340154222b27b0fab32a5056a3710a7c200aa5131ac6b90e966e458ad6cead6c031d3c485b7d2567ce3c2d495cb9347deb149c056c756136b6417d5de21153f30294c7bb64b0635caa0822fd712bb84c706947293b288271b5548613c5422a7a8079a1516085182797e48111c5a1db";
    assert_eq!(
        actual_hex, expected_hex,
        "custom-emoji react packet wire format drifted; re-pin via \
         UPDATE_BACKFILL_FIXTURE=1 (see file header for procedure)"
    );
}
