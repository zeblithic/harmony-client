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

use harmony_app::community_channel_log::{
    sign_channel_event, sign_channel_react, ChannelPostPayload, ChannelReactPayload, MessageId,
    SignedChannelEvent,
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
use harmony_app::community_channel_log::{derive_channel_key, encrypt_channel_packet_with_nonce};
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
