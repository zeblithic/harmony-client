//! ZEB-458 Phase A Task 6: canonical CBOR byte-pins for the community relay
//! wire types.
//!
//! Pinned bytes prevent silent wire-format regressions. If any test fails
//! after an intentional schema change, regenerate via a temporary
//! `eprintln!("{}", hex::encode(&bytes));`, capture the output, and replace
//! the expected hex below — then re-run to confirm the pin holds.
//!
//! Follows the exact pattern of `wire_format_community_fixtures.rs` and
//! `wire_format_channel_log_fixtures.rs`: deterministic struct fields,
//! `canonical_cbor_encode`, hex assertion.
//!
//! All structs use fixed literal byte fields so the output is deterministic
//! across platforms and Rust versions.
//!
//! **No `test-fixtures` gate is needed here (deliberate).** Unlike
//! `wire_format_channel_log_fixtures.rs` — which gates the specific items that
//! import/call the deterministic-nonce helper `encrypt_channel_packet_with_nonce`
//! on `#[cfg(feature = "test-fixtures")]` — this suite calls NO deterministic-nonce
//! crypto. It constructs the wire types from fixed byte literals and runs
//! `canonical_cbor_encode` only. The target compiles and runs identically with or
//! without `--features test-fixtures` (verified by building the test target with
//! the feature off), so there is no deterministic-nonce variant for the
//! `tests/wire_format_*_fixtures.rs` guard-rail to keep out of production.

use harmony_app::community_relay::{
    RelayDepositFrame, RelayHeldBlob, RelayPullAckFrame, RelayPullQuery, RelayPullResponse,
};
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::SpaceId;

// =====================================================================
// Deterministic fixtures
// =====================================================================

fn fixture_community_id() -> SpaceId {
    SpaceId([0xCC; 16])
}

/// A deterministic `RelayDepositFrame` with fixed byte fields.
/// - `recipient_owner`: all-0x11 (16 bytes)
/// - `sender_owner`: all-0x22 (16 bytes)
/// - `community_id`: all-0xCC (16 bytes)
/// - `sender_enrollment_cert`: 4-byte sentinel `[0xDE, 0xAD, 0xBE, 0xEF]`
/// - `sig`: all-0x09 (64 bytes)
/// - `sealed_blob`: 3-byte sentinel `[0xAA, 0xBB, 0xCC]`
fn fixture_relay_deposit_frame() -> RelayDepositFrame {
    RelayDepositFrame {
        recipient_owner: [0x11; 16],
        sender_owner: [0x22; 16],
        community_id: fixture_community_id(),
        sender_enrollment_cert: vec![0xDE, 0xAD, 0xBE, 0xEF],
        sig: vec![0x09; 64],
        sealed_blob: vec![0xAA, 0xBB, 0xCC],
    }
}

/// A deterministic `RelayPullQuery` with fixed byte fields.
/// - `recipient_owner`: all-0x11 (16 bytes)
/// - `community_id`: all-0xCC (16 bytes)
/// - `requester_enrollment_cert`: 3-byte sentinel `[0xA1, 0xA2, 0xA3]`
/// - `sig`: all-0x07 (64 bytes)
fn fixture_relay_pull_query() -> RelayPullQuery {
    RelayPullQuery {
        recipient_owner: [0x11; 16],
        community_id: fixture_community_id(),
        requester_enrollment_cert: vec![0xA1, 0xA2, 0xA3],
        sig: vec![0x07; 64],
    }
}

/// A deterministic `RelayPullResponse` with two `RelayHeldBlob` entries.
/// - Entry 0: `sender_owner` all-0x22, `sealed_blob` = `[0x01, 0x02, 0x03]`
/// - Entry 1: `sender_owner` all-0x33, `sealed_blob` = `[0x04, 0x05]`
fn fixture_relay_pull_response() -> RelayPullResponse {
    RelayPullResponse {
        entries: vec![
            RelayHeldBlob {
                sender_owner: [0x22; 16],
                sealed_blob: vec![0x01, 0x02, 0x03],
            },
            RelayHeldBlob {
                sender_owner: [0x33; 16],
                sealed_blob: vec![0x04, 0x05],
            },
        ],
    }
}

/// A deterministic `RelayPullAckFrame` with fixed byte fields.
/// - `recipient_owner`: all-0x11 (16 bytes)
/// - `community_id`: all-0xCC (16 bytes)
/// - `requester_enrollment_cert`: 3-byte sentinel `[0xA1, 0xA2, 0xA3]`
/// - `content_ids`: two entries, all-0xAA and all-0xBB (32 bytes each)
/// - `sig`: all-0x07 (64 bytes)
fn fixture_relay_pull_ack_frame() -> RelayPullAckFrame {
    RelayPullAckFrame {
        recipient_owner: [0x11; 16],
        community_id: fixture_community_id(),
        requester_enrollment_cert: vec![0xA1, 0xA2, 0xA3],
        content_ids: vec![[0xAA; 32], [0xBB; 32]],
        sig: vec![0x07; 64],
    }
}

// =====================================================================
// Pin tests
// =====================================================================

/// Pin the canonical CBOR of `RelayDepositFrame`.
///
/// Field order (ciborium emits in declaration order, matching 2-char serde
/// key ordering):
///   `ci` (community_id SpaceId 16 bytes),
///   `ec` (sender_enrollment_cert bytes),
///   `ro` (recipient_owner 16 bytes),
///   `sb` (sealed_blob bytes),
///   `sg` (sig 64 bytes),
///   `so` (sender_owner 16 bytes).
///
/// ciborium emits struct fields in their Rust declaration order, not
/// canonical key order — so the actual byte order follows the struct
/// declaration in `community_relay.rs`.
#[test]
fn relay_deposit_frame_wire_bytes_pinned() {
    let frame = fixture_relay_deposit_frame();
    let bytes = canonical_cbor_encode(&frame).expect("encode");
    let hex = hex::encode(&bytes);
    eprintln!("relay_deposit_frame hex: {hex}");
    // Pin: re-run the test with `eprintln!` active to capture this value on
    // first generation, then hardcode it here.
    assert_eq!(
        hex,
        "a662726f901111111111111111111111111111111162736f90182218221822182218221822182218221822182218221822182218221822182262636950cccccccccccccccccccccccccccccccc62656344deadbeef62736758400909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090962736243aabbcc",
        "RelayDepositFrame wire format changed — re-pin deliberately"
    );
}

/// Pin the canonical CBOR of `RelayPullQuery`.
#[test]
fn relay_pull_query_wire_bytes_pinned() {
    let query = fixture_relay_pull_query();
    let bytes = canonical_cbor_encode(&query).expect("encode");
    let hex = hex::encode(&bytes);
    eprintln!("relay_pull_query hex: {hex}");
    assert_eq!(
        hex,
        "a462726f901111111111111111111111111111111162636950cccccccccccccccccccccccccccccccc62656343a1a2a3627367584007070707070707070707070707070707070707070707070707070707070707070707070707070707070707070707070707070707070707070707070707070707",
        "RelayPullQuery wire format changed — re-pin deliberately"
    );
}

/// Pin the canonical CBOR of `RelayPullResponse` (two entries).
#[test]
fn relay_pull_response_wire_bytes_pinned() {
    let resp = fixture_relay_pull_response();
    let bytes = canonical_cbor_encode(&resp).expect("encode");
    let hex = hex::encode(&bytes);
    eprintln!("relay_pull_response hex: {hex}");
    assert_eq!(
        hex,
        "a162656e82a262736f90182218221822182218221822182218221822182218221822182218221822182262736243010203a262736f901833183318331833183318331833183318331833183318331833183318331833627362420405",
        "RelayPullResponse wire format changed — re-pin deliberately"
    );
}

/// Pin the canonical CBOR of `RelayPullAckFrame`.
#[test]
fn relay_pull_ack_frame_wire_bytes_pinned() {
    let frame = fixture_relay_pull_ack_frame();
    let bytes = canonical_cbor_encode(&frame).expect("encode");
    let hex = hex::encode(&bytes);
    eprintln!("relay_pull_ack_frame hex: {hex}");
    assert_eq!(
        hex,
        "a562726f901111111111111111111111111111111162636950cccccccccccccccccccccccccccccccc62656343a1a2a362636482982018aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa982018bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb627367584007070707070707070707070707070707070707070707070707070707070707070707070707070707070707070707070707070707070707070707070707070707",
        "RelayPullAckFrame wire format changed — re-pin deliberately"
    );
}

// =====================================================================
// Round-trip sanity (separate from the pins — confirms encode+decode
// is byte-stable, not just that the types serialize at all)
// =====================================================================

#[test]
fn relay_deposit_frame_encode_decode_round_trips() {
    use harmony_app::community_relay::{decode_relay_deposit_frame, encode_relay_deposit_frame};
    let frame = fixture_relay_deposit_frame();
    let bytes = encode_relay_deposit_frame(&frame).expect("encode");
    let back = decode_relay_deposit_frame(&bytes).expect("decode");
    assert_eq!(back, frame);
}

#[test]
fn relay_pull_query_encode_decode_round_trips() {
    use harmony_app::community_relay::{decode_relay_pull_query, encode_relay_pull_query};
    let query = fixture_relay_pull_query();
    let bytes = encode_relay_pull_query(&query).expect("encode");
    let back = decode_relay_pull_query(&bytes).expect("decode");
    assert_eq!(back, query);
}

#[test]
fn relay_pull_response_encode_decode_round_trips() {
    use harmony_app::community_relay::{decode_relay_pull_response, encode_relay_pull_response};
    let resp = fixture_relay_pull_response();
    let bytes = encode_relay_pull_response(&resp).expect("encode");
    let back = decode_relay_pull_response(&bytes).expect("decode");
    assert_eq!(back, resp);
}

#[test]
fn relay_pull_ack_frame_encode_decode_round_trips() {
    use harmony_app::community_relay::{decode_relay_pull_ack_frame, encode_relay_pull_ack_frame};
    let frame = fixture_relay_pull_ack_frame();
    let bytes = encode_relay_pull_ack_frame(&frame).expect("encode");
    let back = decode_relay_pull_ack_frame(&bytes).expect("decode");
    assert_eq!(back, frame);
}
