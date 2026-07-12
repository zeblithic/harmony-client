//! ZEB-375 (Friends Phase 2a): byte-pinned canonical CBOR fixtures for the new
//! referral-catalog wire types on the `harmony/friend-pex/v1` sub-protocol.
//!
//! Pins the on-the-wire encoding for `ReferralEntry`, `ReferralCatalog` and
//! `CatalogRequest` so any accidental wire-format change (field rename, key-length
//! change, reorder, embedded-cert layout change) is caught here. A failure in this
//! file is a wire-protocol break — review carefully before updating the pinned
//! bytes (cross-version compat, peer interop).
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
//! `ReferralCatalog` and `CatalogRequest` EMBED a harmony-owner `EnrollmentCert`.
//! The cert's own wire format is pinned upstream in harmony-owner's tests; here we
//! only need the cert bytes to be byte-stable so the surrounding referral payload
//! can be exact-hex-pinned.
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
//! lets us EXACT-HEX-PIN the cert-carrying referral types here.
//!
//! These fixtures use FIXED field values (fixed seeds / repeated-byte addresses /
//! a fixed `sig: [0x09; 64]`) so the encoded bytes are byte-stable across runs.
//! They pin BYTE LAYOUT only; they do NOT compute or assert real signatures.

use harmony_app::community_membership::mint_test_owner;
use harmony_app::owner_state_types::{Hlc, OwnerAddr};
use harmony_app::referral_catalog::{
    encode_catalog_request, encode_referral_catalog, CatalogRequest, ReferralCatalog, ReferralEntry,
};

// Seed for the deterministic test owner whose EnrollmentCert is embedded in the
// cert-carrying fixtures. Matches the seed used by the zeb370 fixtures.
const CERT_OWNER_SEED: u8 = 0x42;

// EXPECTED_*_HEX constants are populated by running the test once with
// "FILL_AFTER" as the value; the panic message prints the actual hex to paste
// back in. Regen-on-first-run pattern, mirroring the ZEB-370 fixtures.
const EXPECTED_REFERRAL_ENTRY_HEX: &str = "a2616f5031313131313131313131313131313131616e63626f62";
const EXPECTED_REFERRAL_CATALOG_HEX: &str = "a561615011111111111111111111111111111111616581a2616f5031313131313131313131313131313131616e63626f626174a3617707616c00616461646163a86776657273696f6e01686f776e65725f69645027e5774a6d3b6f6a32246db7518bae50696465766963655f696450d39675728ecef89687069f489157d5d16e6465766963655f7075626b657973a269636c6173736963616ca26e656432353531395f7665726966795820623e770b1719760cffd2aff3955ee52843c9725d0e991826d50b8a5012368e706a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6696973737565645f61741a6553f1006a657870697265735f6174f666697373756572a1664d6173746572a16d6d61737465725f7075626b6579a269636c6173736963616ca26e656432353531395f76657269667958202152f8d19b791d24453242e15f2eab6cb7cffa7b6a5ed30097960e069881db126a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6697369676e61747572655840f7aa268c3010a6649f3942757cf4036f4cb0a999ee3d1630565168c487c4e3487e3bc4bc2927f51190867ef24a1887814998c543c0e4805aeb9279d08ffd0b0a6173584009090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909";
const EXPECTED_CATALOG_REQUEST_HEX: &str = "a461615021212121212121212121212121212121616450424242424242424242424242424242426163a86776657273696f6e01686f776e65725f69645027e5774a6d3b6f6a32246db7518bae50696465766963655f696450d39675728ecef89687069f489157d5d16e6465766963655f7075626b657973a269636c6173736963616ca26e656432353531395f7665726966795820623e770b1719760cffd2aff3955ee52843c9725d0e991826d50b8a5012368e706a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6696973737565645f61741a6553f1006a657870697265735f6174f666697373756572a1664d6173746572a16d6d61737465725f7075626b6579a269636c6173736963616ca26e656432353531395f76657269667958202152f8d19b791d24453242e15f2eab6cb7cffa7b6a5ed30097960e069881db126a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6697369676e61747572655840f7aa268c3010a6649f3942757cf4036f4cb0a999ee3d1630565168c487c4e3487e3bc4bc2927f51190867ef24a1887814998c543c0e4805aeb9279d08ffd0b0a6173584009090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909";

fn hlc(w: u64) -> Hlc {
    Hlc {
        wall_ms: w,
        logical: 0,
        device_id: "d".into(),
    }
}

/// A deterministic `ReferralEntry` built from all-constant inputs.
fn fixture_referral_entry() -> ReferralEntry {
    ReferralEntry {
        peer_owner: OwnerAddr([0x31; 16]),
        display: Some("bob".into()),
    }
}

/// A deterministic `ReferralCatalog`. `author` is a DISTINCT fixed address
/// (`[0x11; 16]`) from the embedded cert's owner (minted from `0x42`); `sig` is a
/// fixed `[0x09; 64]` (we pin the CBOR shape, not a real signature).
fn fixture_referral_catalog() -> ReferralCatalog {
    ReferralCatalog {
        author: OwnerAddr([0x11; 16]),
        entries: vec![fixture_referral_entry()],
        at: hlc(7),
        enrollment: mint_test_owner(CERT_OWNER_SEED).cert,
        signer_certs: Vec::new(),
        sig: [0x09; 64],
    }
}

/// A deterministic `CatalogRequest` with fixed addresses and a fixed `sig`.
fn fixture_catalog_request() -> CatalogRequest {
    CatalogRequest {
        from_addr: OwnerAddr([0x21; 16]),
        to_addr: OwnerAddr([0x42; 16]),
        enrollment: mint_test_owner(CERT_OWNER_SEED).cert,
        signer_certs: Vec::new(),
        sig: [0x09; 64],
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
fn referral_entry_cbor() {
    // No `encode_referral_entry` exists; pin the sub-struct via plain
    // `ciborium::into_writer`, the same encoder `encode_referral_catalog` uses.
    let e = fixture_referral_entry();
    let mut encoded = Vec::new();
    ciborium::into_writer(&e, &mut encoded).expect("encode");
    pin_hex(
        "EXPECTED_REFERRAL_ENTRY_HEX",
        &hex::encode(&encoded),
        EXPECTED_REFERRAL_ENTRY_HEX,
    );

    // Structural: ReferralEntry → map with single-char keys o (peer_owner) /
    // n (display), in that order.
    let value: ciborium::Value = ciborium::de::from_reader(&encoded[..]).expect("decode as value");
    let map = value.as_map().expect("ReferralEntry is a CBOR map");
    let keys: Vec<&str> = map.iter().filter_map(|(k, _)| k.as_text()).collect();
    assert_eq!(
        keys,
        ["o", "n"],
        "ReferralEntry top-level keys must be exactly [\"o\", \"n\"] in order"
    );
}

#[test]
fn referral_catalog_cbor() {
    // Cert-carrying type. mint_test_owner is deterministic, so the embedded
    // EnrollmentCert bytes are stable and we exact-hex-pin the whole catalog.
    let cat = fixture_referral_catalog();
    let encoded = encode_referral_catalog(&cat).expect("encode");
    pin_hex(
        "EXPECTED_REFERRAL_CATALOG_HEX",
        &hex::encode(&encoded),
        EXPECTED_REFERRAL_CATALOG_HEX,
    );

    // Structural: ReferralCatalog → map with single-char keys a (author) /
    // e (entries) / t (at) / c (enrollment) / s (sig), in that order.
    let value: ciborium::Value = ciborium::de::from_reader(&encoded[..]).expect("decode as value");
    let map = value.as_map().expect("ReferralCatalog is a CBOR map");
    let keys: Vec<&str> = map.iter().filter_map(|(k, _)| k.as_text()).collect();
    assert_eq!(
        keys,
        ["a", "e", "t", "c", "s"],
        "ReferralCatalog top-level keys must be exactly [\"a\", \"e\", \"t\", \"c\", \"s\"] in order"
    );
}

#[test]
fn catalog_request_cbor() {
    // Cert-carrying type, pinned via the public `encode_catalog_request`.
    let req = fixture_catalog_request();
    let encoded = encode_catalog_request(&req).expect("encode");
    pin_hex(
        "EXPECTED_CATALOG_REQUEST_HEX",
        &hex::encode(&encoded),
        EXPECTED_CATALOG_REQUEST_HEX,
    );

    // Structural: CatalogRequest → map with single-char keys a (from_addr) /
    // d (to_addr) / c (enrollment) / s (sig), in that order.
    let value: ciborium::Value = ciborium::de::from_reader(&encoded[..]).expect("decode as value");
    let map = value.as_map().expect("CatalogRequest is a CBOR map");
    let keys: Vec<&str> = map.iter().filter_map(|(k, _)| k.as_text()).collect();
    assert_eq!(
        keys,
        ["a", "d", "c", "s"],
        "CatalogRequest top-level keys must be exactly [\"a\", \"d\", \"c\", \"s\"] in order"
    );
}
