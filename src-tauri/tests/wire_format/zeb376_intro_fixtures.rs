//! ZEB-376 (Friends Phase 2b): byte-pinned canonical CBOR fixtures for the new
//! active-introduction wire types on the `harmony/friend-pex/v1` sub-protocol.
//!
//! Pins the on-the-wire encoding for `IntroduceRequest`, `Introduction`, and the
//! tagged `PexFrame::IntroduceRequest` wrapper so any accidental wire-format
//! change (field rename, key-length change, reorder, embedded-cert layout
//! change) is caught here. A failure in this file is a wire-protocol break —
//! review carefully before updating the pinned bytes (cross-version compat,
//! peer interop).
//!
//! Two flavours of pin:
//!   * EXACT hex — `encode_*`/`ciborium::into_writer` of a fully deterministic
//!     value, compared byte-for-byte.
//!   * STRUCTURAL — a `ciborium::Value` map-key assertion (the encoding shape),
//!     used in addition to the exact hex as a human-readable description of the
//!     map and as a second line of defence.
//!
//! ## Determinism of `mint_test_owner` (the embedded `EnrollmentCert`)
//!
//! `IntroduceRequest` and `Introduction` EMBED one or two harmony-owner
//! `EnrollmentCert`s. The cert's own wire format is pinned upstream in
//! harmony-owner's tests; here we only need the cert bytes to be byte-stable so
//! the surrounding intro payload can be exact-hex-pinned.
//!
//! `community_membership::mint_test_owner(seed)` IS fully deterministic:
//!   * the master key is `SigningKey::from_bytes(&[seed; 32])` and the device key
//!     is `from_bytes(&[seed ^ 0xFF; 32])` — derived from the seed, no RNG;
//!   * `EnrollmentCert::sign_master` signs with a fixed `issued_at` constant (not
//!     the wall clock) and Ed25519's signature is deterministic per RFC 8032 (the
//!     nonce is derived from the secret key + message — no randomness).
//!
//! The companion `mint_test_owner_is_deterministic` test in
//! `wire_format/zeb370_fixtures.rs` proves this empirically; the same property
//! lets us EXACT-HEX-PIN the cert-carrying intro types here.
//!
//! These fixtures use FIXED field values (fixed seeds / repeated-byte addresses
//! / a fixed `sig: [0x09; 64]`) so the encoded bytes are byte-stable across
//! runs. They pin BYTE LAYOUT only; they do NOT compute or assert real
//! signatures.

use harmony_app::community_membership::mint_test_owner;
use harmony_app::friend_intro::{
    encode_introduce_request, encode_introduction, encode_pex_frame, IntroduceRequest,
    Introduction, PexFrame,
};
use harmony_app::owner_state_types::{Hlc, OwnerAddr};
use harmony_app::reachability_record::ReachabilityAnnouncePayload;

// Seed for the deterministic test owner whose EnrollmentCert is embedded as the
// `IntroduceRequest`/`Introduction` requester cert. Matches the seed used by the
// zeb370/zeb375 fixtures.
const CERT_OWNER_SEED: u8 = 0x42;
// Distinct seed for the second embedded cert on `Introduction`
// (`voucher_enrollment`), so the fixture exercises two DIFFERENT cert-owning
// identities (subject vs. voucher), same as the real 2b flow.
const VOUCHER_CERT_OWNER_SEED: u8 = 0x43;

// EXPECTED_*_HEX constants are populated by running the test once with
// "FILL_AFTER" as the value; the panic message prints the actual hex to paste
// back in. Regen-on-first-run pattern, mirroring the ZEB-370/375 fixtures.
const EXPECTED_INTRODUCE_REQUEST_HEX: &str = "a66161501111111111111111111111111111111161645022222222222222222222222222222222617850333333333333333333333333333333336172a5626e645820111111111111111111111111111111111111111111111111111111111111111162726c6968747470733a2f2f7262646180627473016273675840222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222226163a86776657273696f6e01686f776e65725f69645027e5774a6d3b6f6a32246db7518bae50696465766963655f696450d39675728ecef89687069f489157d5d16e6465766963655f7075626b657973a269636c6173736963616ca26e656432353531395f7665726966795820623e770b1719760cffd2aff3955ee52843c9725d0e991826d50b8a5012368e706a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6696973737565645f61741a6553f1006a657870697265735f6174f666697373756572a1664d6173746572a16d6d61737465725f7075626b6579a269636c6173736963616ca26e656432353531395f76657269667958202152f8d19b791d24453242e15f2eab6cb7cffa7b6a5ed30097960e069881db126a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6697369676e61747572655840f7aa268c3010a6649f3942757cf4036f4cb0a999ee3d1630565168c487c4e3487e3bc4bc2927f51190867ef24a1887814998c543c0e4805aeb9279d08ffd0b0a6173584009090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909";
const EXPECTED_INTRODUCTION_HEX: &str = "a86176501111111111111111111111111111111161645022222222222222222222222222222222617550333333333333333333333333333333336163a86776657273696f6e01686f776e65725f69645027e5774a6d3b6f6a32246db7518bae50696465766963655f696450d39675728ecef89687069f489157d5d16e6465766963655f7075626b657973a269636c6173736963616ca26e656432353531395f7665726966795820623e770b1719760cffd2aff3955ee52843c9725d0e991826d50b8a5012368e706a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6696973737565645f61741a6553f1006a657870697265735f6174f666697373756572a1664d6173746572a16d6d61737465725f7075626b6579a269636c6173736963616ca26e656432353531395f76657269667958202152f8d19b791d24453242e15f2eab6cb7cffa7b6a5ed30097960e069881db126a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6697369676e61747572655840f7aa268c3010a6649f3942757cf4036f4cb0a999ee3d1630565168c487c4e3487e3bc4bc2927f51190867ef24a1887814998c543c0e4805aeb9279d08ffd0b0a6172a5626e645820111111111111111111111111111111111111111111111111111111111111111162726c6968747470733a2f2f7262646180627473016273675840222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222226174a3617707616c006164616461735840090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909096165a86776657273696f6e01686f776e65725f696450197fd931c7c568f538c06bd3caaf2f57696465766963655f696450e2acf018117541c8a8d5b527e454288f6e6465766963655f7075626b657973a269636c6173736963616ca26e656432353531395f7665726966795820020dacf84f4470aefa785551da567c1e50e7b44332c3a2f70be10d2cd78cac8e6a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6696973737565645f61741a6553f1006a657870697265735f6174f666697373756572a1664d6173746572a16d6d61737465725f7075626b6579a269636c6173736963616ca26e656432353531395f766572696679582022fc297792f0b6ffc0bfcfdb7edb0c0aa14e025a365ec0e342e86e3829cb74b66a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6697369676e61747572655840450edc5187089437ebf5b3b3ab0f1553d8fc14868ca8ae73e4ce3355dd5cb91c3c94b77ec6dd168a67da91e61a7806c679bf1e67adf2972b248a563515c9ac0b";
const EXPECTED_PEX_FRAME_INTRODUCTION_HEX: &str = "a162696ea86176501111111111111111111111111111111161645022222222222222222222222222222222617550333333333333333333333333333333336163a86776657273696f6e01686f776e65725f69645027e5774a6d3b6f6a32246db7518bae50696465766963655f696450d39675728ecef89687069f489157d5d16e6465766963655f7075626b657973a269636c6173736963616ca26e656432353531395f7665726966795820623e770b1719760cffd2aff3955ee52843c9725d0e991826d50b8a5012368e706a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6696973737565645f61741a6553f1006a657870697265735f6174f666697373756572a1664d6173746572a16d6d61737465725f7075626b6579a269636c6173736963616ca26e656432353531395f76657269667958202152f8d19b791d24453242e15f2eab6cb7cffa7b6a5ed30097960e069881db126a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6697369676e61747572655840f7aa268c3010a6649f3942757cf4036f4cb0a999ee3d1630565168c487c4e3487e3bc4bc2927f51190867ef24a1887814998c543c0e4805aeb9279d08ffd0b0a6172a5626e645820111111111111111111111111111111111111111111111111111111111111111162726c6968747470733a2f2f7262646180627473016273675840222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222226174a3617707616c006164616461735840090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909096165a86776657273696f6e01686f776e65725f696450197fd931c7c568f538c06bd3caaf2f57696465766963655f696450e2acf018117541c8a8d5b527e454288f6e6465766963655f7075626b657973a269636c6173736963616ca26e656432353531395f7665726966795820020dacf84f4470aefa785551da567c1e50e7b44332c3a2f70be10d2cd78cac8e6a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6696973737565645f61741a6553f1006a657870697265735f6174f666697373756572a1664d6173746572a16d6d61737465725f7075626b6579a269636c6173736963616ca26e656432353531395f766572696679582022fc297792f0b6ffc0bfcfdb7edb0c0aa14e025a365ec0e342e86e3829cb74b66a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6697369676e61747572655840450edc5187089437ebf5b3b3ab0f1553d8fc14868ca8ae73e4ce3355dd5cb91c3c94b77ec6dd168a67da91e61a7806c679bf1e67adf2972b248a563515c9ac0b";
const EXPECTED_PEX_FRAME_INTRODUCE_REQUEST_HEX: &str = "a1626972a66161501111111111111111111111111111111161645022222222222222222222222222222222617850333333333333333333333333333333336172a5626e645820111111111111111111111111111111111111111111111111111111111111111162726c6968747470733a2f2f7262646180627473016273675840222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222222226163a86776657273696f6e01686f776e65725f69645027e5774a6d3b6f6a32246db7518bae50696465766963655f696450d39675728ecef89687069f489157d5d16e6465766963655f7075626b657973a269636c6173736963616ca26e656432353531395f7665726966795820623e770b1719760cffd2aff3955ee52843c9725d0e991826d50b8a5012368e706a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6696973737565645f61741a6553f1006a657870697265735f6174f666697373756572a1664d6173746572a16d6d61737465725f7075626b6579a269636c6173736963616ca26e656432353531395f76657269667958202152f8d19b791d24453242e15f2eab6cb7cffa7b6a5ed30097960e069881db126a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6697369676e61747572655840f7aa268c3010a6649f3942757cf4036f4cb0a999ee3d1630565168c487c4e3487e3bc4bc2927f51190867ef24a1887814998c543c0e4805aeb9279d08ffd0b0a6173584009090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909";

fn hlc(w: u64) -> Hlc {
    Hlc {
        wall_ms: w,
        logical: 0,
        device_id: "d".into(),
    }
}

/// A deterministic `ReachabilityAnnouncePayload` built from all-constant inputs.
fn fixture_reachability() -> ReachabilityAnnouncePayload {
    ReachabilityAnnouncePayload {
        iroh_node_id: [0x11; 32],
        home_relay_url: "https://r".into(),
        direct_addresses: vec![],
        announced_at_ms: 1,
        identity_signature: [0x22; 64],
        butler_set: vec![],
        bs_at: 0,
    }
}

/// A deterministic `IntroduceRequest` with fixed addresses, a fixed embedded
/// cert, and a fixed `sig`.
fn fixture_introduce_request() -> IntroduceRequest {
    IntroduceRequest {
        from_addr: OwnerAddr([0x11; 16]),
        to_addr: OwnerAddr([0x22; 16]),
        target: OwnerAddr([0x33; 16]),
        reachability: fixture_reachability(),
        enrollment: mint_test_owner(CERT_OWNER_SEED).cert,
        sig: [0x09; 64],
        signer_certs: Vec::new(),
    }
}

/// A deterministic `Introduction` with fixed addresses, two fixed embedded
/// certs (subject vs. voucher), and a fixed `sig`.
fn fixture_introduction() -> Introduction {
    Introduction {
        voucher: OwnerAddr([0x11; 16]),
        to_addr: OwnerAddr([0x22; 16]),
        subject: OwnerAddr([0x33; 16]),
        subject_cert: mint_test_owner(CERT_OWNER_SEED).cert,
        reachability: fixture_reachability(),
        at: hlc(7),
        sig: [0x09; 64],
        voucher_enrollment: mint_test_owner(VOUCHER_CERT_OWNER_SEED).cert,
        signer_certs: Vec::new(),
        // ZEB-376 (Q2): empty subject signer bundle — omitted from the wire
        // (skip_serializing_if empty), so the pinned bytes are unchanged and the
        // additive field stays backward-compatible for the common master-issued
        // subject case.
        subject_signer_certs: Vec::new(),
    }
}

/// Assert `actual_hex` against `expected`, panicking with a paste-ready
/// regeneration line while `expected` still holds the `FILL_AFTER` sentinel.
fn pin_hex(name: &str, actual_hex: &str, expected: &str) {
    if expected.contains("FILL_AFTER") {
        panic!("REGENERATE {name} = \"{actual_hex}\";");
    }
    assert_eq!(actual_hex, expected, "{name}: wire format changed");
}

#[test]
fn introduce_request_cbor() {
    let req = fixture_introduce_request();
    let encoded = encode_introduce_request(&req).expect("encode");
    pin_hex(
        "EXPECTED_INTRODUCE_REQUEST_HEX",
        &hex::encode(&encoded),
        EXPECTED_INTRODUCE_REQUEST_HEX,
    );

    // Structural: IntroduceRequest → map with single-char keys a (from_addr) /
    // d (to_addr) / x (target) / r (reachability) / c (enrollment) / s (sig),
    // in that order. `b` (signer_certs) is omitted (skip_serializing_if empty).
    let value: ciborium::Value = ciborium::de::from_reader(&encoded[..]).expect("decode as value");
    let map = value.as_map().expect("IntroduceRequest is a CBOR map");
    let keys: Vec<&str> = map.iter().filter_map(|(k, _)| k.as_text()).collect();
    assert_eq!(
        keys,
        ["a", "d", "x", "r", "c", "s"],
        "IntroduceRequest top-level keys must be exactly [\"a\", \"d\", \"x\", \"r\", \"c\", \"s\"] in order"
    );
}

#[test]
fn introduction_cbor() {
    let intro = fixture_introduction();
    let encoded = encode_introduction(&intro).expect("encode");
    pin_hex(
        "EXPECTED_INTRODUCTION_HEX",
        &hex::encode(&encoded),
        EXPECTED_INTRODUCTION_HEX,
    );

    // Structural: Introduction → map with single-char keys v (voucher) /
    // d (to_addr) / u (subject) / c (subject_cert) / r (reachability) /
    // t (at) / s (sig) / e (voucher_enrollment), in that order. `b`
    // (signer_certs) and `g` (subject_signer_certs) are omitted (both empty,
    // skip_serializing_if).
    let value: ciborium::Value = ciborium::de::from_reader(&encoded[..]).expect("decode as value");
    let map = value.as_map().expect("Introduction is a CBOR map");
    let keys: Vec<&str> = map.iter().filter_map(|(k, _)| k.as_text()).collect();
    assert_eq!(
        keys,
        ["v", "d", "u", "c", "r", "t", "s", "e"],
        "Introduction top-level keys must be exactly [\"v\", \"d\", \"u\", \"c\", \"r\", \"t\", \"s\", \"e\"] in order"
    );
}

/// ZEB-376 (#13): byte-pin the tagged `PexFrame::Introduction` wrapper too (the
/// bare `Introduction` above pins the inner payload; this pins the enum framing X
/// actually decodes off the wire). A single-key `{"in": <Introduction>}` map —
/// the same shape `decode_pex_frame_or_catalog` uses to disambiguate a tagged 2b
/// frame from a bare 2a `CatalogRequest`.
#[test]
fn pex_frame_introduction_cbor() {
    let frame = PexFrame::Introduction(Box::new(fixture_introduction()));
    let encoded = encode_pex_frame(&frame).expect("encode");
    pin_hex(
        "EXPECTED_PEX_FRAME_INTRODUCTION_HEX",
        &hex::encode(&encoded),
        EXPECTED_PEX_FRAME_INTRODUCTION_HEX,
    );

    let value: ciborium::Value = ciborium::de::from_reader(&encoded[..]).expect("decode as value");
    let map = value
        .as_map()
        .expect("PexFrame::Introduction is a CBOR map");
    let keys: Vec<&str> = map.iter().filter_map(|(k, _)| k.as_text()).collect();
    assert_eq!(
        keys,
        ["in"],
        "PexFrame::Introduction top-level keys must be exactly [\"in\"]"
    );
}

#[test]
fn pex_frame_introduce_request_cbor() {
    let frame = PexFrame::IntroduceRequest(Box::new(fixture_introduce_request()));
    let encoded = encode_pex_frame(&frame).expect("encode");
    pin_hex(
        "EXPECTED_PEX_FRAME_INTRODUCE_REQUEST_HEX",
        &hex::encode(&encoded),
        EXPECTED_PEX_FRAME_INTRODUCE_REQUEST_HEX,
    );

    // Structural: PexFrame is an externally-tagged enum → a single-key map
    // { "ir": <IntroduceRequest> }. This single-key shape is what
    // `decode_pex_frame_or_catalog` uses to disambiguate a tagged 2b frame from
    // a bare (multi-key) 2a `CatalogRequest`.
    let value: ciborium::Value = ciborium::de::from_reader(&encoded[..]).expect("decode as value");
    let map = value
        .as_map()
        .expect("PexFrame::IntroduceRequest is a CBOR map");
    let keys: Vec<&str> = map.iter().filter_map(|(k, _)| k.as_text()).collect();
    assert_eq!(
        keys,
        ["ir"],
        "PexFrame::IntroduceRequest top-level keys must be exactly [\"ir\"]"
    );
}
