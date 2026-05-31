//! ZEB-345: pin the exact canonical-CBOR wire bytes of a fixed v1 `ProfilePageDoc`.
//!
//! The CID of a profile doc is the hash of these canonical bytes, so any
//! accidental schema change (key rename, field reorder, encoder swap) would
//! silently invalidate every published profile-page CID. This fixture pins the
//! encoder output for one fixed v1 document against a hardcoded `&[u8]`, so such
//! drift is caught at test time.
//!
//! Map arity: `ProfilePageDoc` has 4 declaration-order keys (vn/bo/ln/fl), all
//! required (no `skip_serializing_if`), so the top-level map header is always
//! `0xA4`.
use harmony_app::profile_page_doc::{
    decode_profile_doc, encode_profile_doc, ProfileField, ProfileLink, ProfilePageDoc,
    PROFILE_DOC_VERSION,
};

/// The single pinned fixture doc. Kept small + deterministic.
fn fixture_doc() -> ProfilePageDoc {
    ProfilePageDoc {
        version: PROFILE_DOC_VERSION,
        bio: "hi\nthere".into(),
        links: vec![ProfileLink {
            label: "Site".into(),
            url: "https://example.com".into(),
        }],
        fields: vec![ProfileField {
            key: "Pronouns".into(),
            value: "she/her".into(),
        }],
    }
}

/// Exact canonical-CBOR encoding of `fixture_doc()`. Captured from the encoder
/// (see `dump_fixture_bytes`, ignored). Updating this without an intentional
/// schema change is a bug.
const PINNED_BYTES: &[u8] = &[
    0xA4, 0x62, 0x76, 0x6E, 0x01, 0x62, 0x62, 0x6F, 0x68, 0x68, 0x69, 0x0A, 0x74, 0x68, 0x65, 0x72,
    0x65, 0x62, 0x6C, 0x6E, 0x81, 0xA2, 0x62, 0x6C, 0x62, 0x64, 0x53, 0x69, 0x74, 0x65, 0x62, 0x75,
    0x72, 0x73, 0x68, 0x74, 0x74, 0x70, 0x73, 0x3A, 0x2F, 0x2F, 0x65, 0x78, 0x61, 0x6D, 0x70, 0x6C,
    0x65, 0x2E, 0x63, 0x6F, 0x6D, 0x62, 0x66, 0x6C, 0x81, 0xA2, 0x62, 0x6B, 0x79, 0x68, 0x50, 0x72,
    0x6F, 0x6E, 0x6F, 0x75, 0x6E, 0x73, 0x62, 0x76, 0x6C, 0x67, 0x73, 0x68, 0x65, 0x2F, 0x68, 0x65,
    0x72,
];

#[test]
fn profile_page_doc_pins_exact_canonical_bytes() {
    let bytes = encode_profile_doc(&fixture_doc()).expect("encode fixture doc");
    assert_eq!(
        bytes, PINNED_BYTES,
        "profile_page_doc canonical wire bytes changed — intentional schema change? \
         update PINNED_BYTES and bump PROFILE_DOC_VERSION if the format is incompatible"
    );
}

#[test]
fn profile_page_doc_top_level_map_is_four_entries() {
    let bytes = encode_profile_doc(&fixture_doc()).expect("encode fixture doc");
    assert_eq!(
        bytes[0], 0xA4,
        "ProfilePageDoc must serialize as a 4-entry CBOR map (vn/bo/ln/fl)"
    );
}

#[test]
fn pinned_bytes_round_trip_back_to_fixture_doc() {
    assert_eq!(
        decode_profile_doc(PINNED_BYTES).expect("decode pinned bytes"),
        fixture_doc()
    );
}

/// Run with `--ignored --nocapture` to regenerate `PINNED_BYTES` after an
/// intentional schema change.
#[test]
#[ignore]
fn dump_fixture_bytes() {
    let bytes = encode_profile_doc(&fixture_doc()).expect("encode fixture doc");
    let hex: Vec<String> = bytes.iter().map(|b| format!("0x{b:02X}")).collect();
    println!("len={}", bytes.len());
    println!("[{}]", hex.join(", "));
}
