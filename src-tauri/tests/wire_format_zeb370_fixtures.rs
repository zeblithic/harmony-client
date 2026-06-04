//! ZEB-370: Byte-pinned canonical CBOR fixtures for the friend wire types.
//!
//! Pins the on-the-wire encoding for the new ZEB-370 friend types so any
//! accidental wire-format change (field rename, key-length change, reorder,
//! enum tag rename, embedded-cert layout change) is caught here. A failure in
//! this file is a wire-protocol break — review carefully before updating the
//! pinned bytes (cross-version compat, peer interop).
//!
//! Two flavours of pin:
//!   * EXACT hex — `canonical_cbor_encode`/`ciborium::into_writer` of a fully
//!     deterministic value, compared byte-for-byte.
//!   * STRUCTURAL — a `ciborium::Value` map-key assertion (the encoding shape),
//!     used in addition to the exact hex as a human-readable description of the
//!     map and as a second line of defence.
//!
//! ## Determinism of `mint_test_owner` (the embedded `EnrollmentCert`)
//!
//! `friend_token::FriendTokenPayload`, `iroh_friend_acceptor::FriendLinkRequest`
//! and `iroh_friend_acceptor::FriendLinkAccepted` EMBED a harmony-owner
//! `EnrollmentCert`. The cert's own wire format is pinned upstream in
//! harmony-owner's tests; here we only need the cert bytes to be byte-stable so
//! the surrounding friend payload can be exact-hex-pinned.
//!
//! `community_membership::mint_test_owner(seed)` IS fully deterministic:
//!   * the master key is `SigningKey::from_bytes(&[seed; 32])` and the device
//!     key is `from_bytes(&[seed ^ 0xFF; 32])` — derived from the seed, no RNG;
//!   * `EnrollmentCert::sign_master` signs with `issued_at = 1_700_000_000`
//!     (a fixed constant, not the wall clock) and Ed25519's signature is
//!     deterministic per RFC 8032 (the nonce is derived from the secret key +
//!     message — no randomness).
//!
//! The `mint_test_owner_is_deterministic` test below proves this empirically
//! (mint twice → identical cert bytes). Because of this we EXACT-HEX-PIN the
//! cert-carrying types too (not just structural pins). If a future harmony-owner
//! bump changes the cert layout, the exact-hex asserts here will fire — that is
//! intended (it IS a wire change), and the regen-on-first-run sentinel makes
//! re-pinning a one-line paste.
//!
//! Uses deterministic test bytes (fixed seeds / repeated-byte values) so the
//! encoded bytes are byte-stable across runs. These tests pin BYTE LAYOUT only;
//! they do NOT assert cryptographic validity.

use harmony_app::community_invite::InviteToken;
use harmony_app::community_membership::mint_test_owner;
use harmony_app::friend_graph::{
    owner_id_from_master_ed25519, FriendEntry, FriendGraph, FriendOrigin, FriendStatus,
};
use harmony_app::friend_token::FriendTokenPayload;
use harmony_app::iroh_friend_acceptor::{
    encode_friend_accepted, encode_friend_request, FriendLinkAccepted, FriendLinkRequest,
};
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::Hlc;

// Fixed master Ed25519 seed for the FriendEntry / FriendGraph fixtures. The map
// key (the friend's `owner_id`) is derived from this via
// `owner_id_from_master_ed25519`, mirroring the binding `apply_friend_update`
// enforces.
const FRIEND_MASTER_SEED: [u8; 32] = [0x11; 32];

// Seed for the deterministic test owner whose EnrollmentCert is embedded in the
// cert-carrying fixtures. Kept in 0x01..=0xFE and away from `N ^ 0xFF` pairing.
const CERT_OWNER_SEED: u8 = 0x42;

// EXPECTED_*_HEX constants are populated by running the test once with
// "FILL_AFTER" as the value; the panic message prints the actual hex to paste
// back in. Regen-on-first-run pattern from ZEB-250.
const EXPECTED_FRIEND_ENTRY_HEX: &str = "a6616d58201111111111111111111111111111111111111111111111111111111111111111616e65616c696365617366616374697665617665746f6b656e6172f4616ca3617707616c0061646164";
const EXPECTED_FRIEND_GRAPH_HEX: &str = "a16166a1509e5e0d484ac3a08f6a853c8caa462f94a6616d58201111111111111111111111111111111111111111111111111111111111111111616e65616c696365617366616374697665617665746f6b656e6172f4616ca3617707616c0061646164";
const EXPECTED_FRIEND_TOKEN_PAYLOAD_HEX: &str = "a46269615027e5774a6d3b6f6a32246db7518bae5062646e63626f6262746ba36269765027e5774a6d3b6f6a32246db7518bae50626d74a3617701616c0061646164627367584003030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303626965a86776657273696f6e01686f776e65725f69645027e5774a6d3b6f6a32246db7518bae50696465766963655f696450d39675728ecef89687069f489157d5d16e6465766963655f7075626b657973a269636c6173736963616ca26e656432353531395f7665726966795820623e770b1719760cffd2aff3955ee52843c9725d0e991826d50b8a5012368e706a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6696973737565645f61741a6553f1006a657870697265735f6174f666697373756572a1664d6173746572a16d6d61737465725f7075626b6579a269636c6173736963616ca26e656432353531395f76657269667958202152f8d19b791d24453242e15f2eab6cb7cffa7b6a5ed30097960e069881db126a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6697369676e61747572655840f7aa268c3010a6649f3942757cf4036f4cb0a999ee3d1630565168c487c4e3487e3bc4bc2927f51190867ef24a1887814998c543c0e4805aeb9279d08ffd0b0a";
const EXPECTED_FRIEND_LINK_REQUEST_HEX: &str = "a56966726f6d5f616464725027e5774a6d3b6f6a32246db7518bae5067646973706c617965616c69636569746f6b656e5f7369675840070707070707070707070707070707070707070707070707070707070707070707070707070707070707070707070707070707070707070707070707070707076a656e726f6c6c6d656e74a86776657273696f6e01686f776e65725f69645027e5774a6d3b6f6a32246db7518bae50696465766963655f696450d39675728ecef89687069f489157d5d16e6465766963655f7075626b657973a269636c6173736963616ca26e656432353531395f7665726966795820623e770b1719760cffd2aff3955ee52843c9725d0e991826d50b8a5012368e706a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6696973737565645f61741a6553f1006a657870697265735f6174f666697373756572a1664d6173746572a16d6d61737465725f7075626b6579a269636c6173736963616ca26e656432353531395f76657269667958202152f8d19b791d24453242e15f2eab6cb7cffa7b6a5ed30097960e069881db126a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6697369676e61747572655840f7aa268c3010a6649f3942757cf4036f4cb0a999ee3d1630565168c487c4e3487e3bc4bc2927f51190867ef24a1887814998c543c0e4805aeb9279d08ffd0b0a63736967584009090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909";
const EXPECTED_FRIEND_LINK_ACCEPTED_HEX: &str = "a46966726f6d5f616464725027e5774a6d3b6f6a32246db7518bae5067646973706c617963626f626a656e726f6c6c6d656e74a86776657273696f6e01686f776e65725f69645027e5774a6d3b6f6a32246db7518bae50696465766963655f696450d39675728ecef89687069f489157d5d16e6465766963655f7075626b657973a269636c6173736963616ca26e656432353531395f7665726966795820623e770b1719760cffd2aff3955ee52843c9725d0e991826d50b8a5012368e706a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6696973737565645f61741a6553f1006a657870697265735f6174f666697373756572a1664d6173746572a16d6d61737465725f7075626b6579a269636c6173736963616ca26e656432353531395f76657269667958202152f8d19b791d24453242e15f2eab6cb7cffa7b6a5ed30097960e069881db126a7832353531395f707562582000000000000000000000000000000000000000000000000000000000000000006c706f73745f7175616e74756df6697369676e61747572655840f7aa268c3010a6649f3942757cf4036f4cb0a999ee3d1630565168c487c4e3487e3bc4bc2927f51190867ef24a1887814998c543c0e4805aeb9279d08ffd0b0a6373696758400b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b";

fn hlc(w: u64) -> Hlc {
    Hlc {
        wall_ms: w,
        logical: 0,
        device_id: "d".into(),
    }
}

/// A deterministic `FriendEntry` built from all-constant inputs.
fn fixture_friend_entry() -> FriendEntry {
    FriendEntry {
        master_ed25519: FRIEND_MASTER_SEED,
        display: Some("alice".into()),
        status: FriendStatus::Active,
        established_via: FriendOrigin::Token,
        referrable: false,
        learned_at: hlc(7),
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

/// Empirically prove the embedded `EnrollmentCert` is byte-stable for a fixed
/// seed, justifying exact-hex pins for the cert-carrying friend types. If this
/// ever fails, `mint_test_owner` (or harmony-owner's signing) became
/// non-deterministic and the cert-carrying pins below must switch to structural.
#[test]
fn mint_test_owner_is_deterministic() {
    let a = mint_test_owner(CERT_OWNER_SEED);
    let b = mint_test_owner(CERT_OWNER_SEED);
    // EnrollmentCert is a harmony-owner type (no CanonicalPayload impl), so
    // encode it with plain ciborium to compare bytes.
    let mut a_bytes = Vec::new();
    let mut b_bytes = Vec::new();
    ciborium::into_writer(&a.cert, &mut a_bytes).expect("encode cert a");
    ciborium::into_writer(&b.cert, &mut b_bytes).expect("encode cert b");
    assert_eq!(
        a_bytes, b_bytes,
        "mint_test_owner must produce byte-identical certs for a fixed seed"
    );
    assert_eq!(
        a.owner, b.owner,
        "owner addr must be stable for a fixed seed"
    );
}

#[test]
fn friend_entry_canonical_cbor() {
    let e = fixture_friend_entry();
    let encoded = canonical_cbor_encode(&e).expect("encode");
    pin_hex(
        "EXPECTED_FRIEND_ENTRY_HEX",
        &hex::encode(&encoded),
        EXPECTED_FRIEND_ENTRY_HEX,
    );

    // Structural: FriendEntry → map with single-char keys m/n/s/v/r/l.
    let value: ciborium::Value = ciborium::de::from_reader(&encoded[..]).expect("decode as value");
    let map = value.as_map().expect("FriendEntry is a CBOR map");
    let keys: Vec<&str> = map.iter().filter_map(|(k, _)| k.as_text()).collect();
    for k in ["m", "n", "s", "v", "r", "l"] {
        assert!(keys.contains(&k), "FriendEntry map missing \"{k}\" key");
    }
    assert_eq!(
        map.len(),
        6,
        "FriendEntry should encode exactly 6 keys (display present)"
    );
}

#[test]
fn friend_graph_canonical_cbor() {
    // Build a single-entry graph whose map key equals the friend's owner_id,
    // derived from a fixed master Ed25519 key (the apply_friend_update binding).
    let mut g = FriendGraph::default();
    let key = owner_id_from_master_ed25519(&FRIEND_MASTER_SEED);
    g.friends.insert(key, fixture_friend_entry());

    let encoded = canonical_cbor_encode(&g).expect("encode");
    pin_hex(
        "EXPECTED_FRIEND_GRAPH_HEX",
        &hex::encode(&encoded),
        EXPECTED_FRIEND_GRAPH_HEX,
    );

    // Structural: FriendGraph → map with single key "f" (the friends BTreeMap).
    let value: ciborium::Value = ciborium::de::from_reader(&encoded[..]).expect("decode as value");
    let map = value.as_map().expect("FriendGraph is a CBOR map");
    assert_eq!(map.len(), 1, "FriendGraph should encode exactly one key");
    assert_eq!(
        map[0].0.as_text(),
        Some("f"),
        "FriendGraph outer key should be \"f\""
    );
}

#[test]
fn friend_token_payload_canonical_cbor() {
    // Cert-carrying type. mint_test_owner is deterministic (proven by
    // mint_test_owner_is_deterministic), so the embedded EnrollmentCert bytes
    // are stable and we exact-hex-pin the whole payload.
    let owner = mint_test_owner(CERT_OWNER_SEED);
    let payload = FriendTokenPayload {
        inviter_addr: owner.owner,
        display_hint: Some("bob".into()),
        token: InviteToken {
            inviter: owner.owner,
            invitee_hint: None,
            minted_at: hlc(1),
            expires_at: None,
            sig: [0x03; 64],
        },
        inviter_enrollment: owner.cert,
    };
    let encoded = canonical_cbor_encode(&payload).expect("encode");
    pin_hex(
        "EXPECTED_FRIEND_TOKEN_PAYLOAD_HEX",
        &hex::encode(&encoded),
        EXPECTED_FRIEND_TOKEN_PAYLOAD_HEX,
    );

    // Structural: top-level keys are ia / dn / tk / ie (display_hint present).
    let value: ciborium::Value = ciborium::de::from_reader(&encoded[..]).expect("decode as value");
    let map = value.as_map().expect("FriendTokenPayload is a CBOR map");
    let keys: Vec<&str> = map.iter().filter_map(|(k, _)| k.as_text()).collect();
    for k in ["ia", "dn", "tk", "ie"] {
        assert!(keys.contains(&k), "FriendTokenPayload missing \"{k}\" key");
    }
    assert_eq!(
        map.len(),
        4,
        "FriendTokenPayload should encode 4 keys (display_hint present)"
    );
}

#[test]
fn friend_link_request_cbor() {
    // FriendLinkRequest is encoded with plain `ciborium::into_writer` via the
    // public `encode_friend_request` (NOT canonical_cbor_encode) — pin the
    // bytes that actually go on the `harmony/friend/v1` ALPN.
    let owner = mint_test_owner(CERT_OWNER_SEED);
    let req = FriendLinkRequest {
        from_addr: owner.owner,
        display: Some("alice".into()),
        token_sig: [0x07; 64],
        enrollment: owner.cert,
        sig: [0x09; 64],
    };
    let encoded = encode_friend_request(&req).expect("encode");
    pin_hex(
        "EXPECTED_FRIEND_LINK_REQUEST_HEX",
        &hex::encode(&encoded),
        EXPECTED_FRIEND_LINK_REQUEST_HEX,
    );

    // Structural: field names (no rename) → from_addr / display / token_sig /
    // enrollment / sig.
    let value: ciborium::Value = ciborium::de::from_reader(&encoded[..]).expect("decode as value");
    let map = value.as_map().expect("FriendLinkRequest is a CBOR map");
    let keys: Vec<&str> = map.iter().filter_map(|(k, _)| k.as_text()).collect();
    for k in ["from_addr", "display", "token_sig", "enrollment", "sig"] {
        assert!(keys.contains(&k), "FriendLinkRequest missing \"{k}\" key");
    }
    assert_eq!(
        map.len(),
        5,
        "FriendLinkRequest should encode 5 keys (display present)"
    );
}

#[test]
fn friend_link_accepted_cbor() {
    let owner = mint_test_owner(CERT_OWNER_SEED);
    let acc = FriendLinkAccepted {
        from_addr: owner.owner,
        display: Some("bob".into()),
        enrollment: owner.cert,
        sig: [0x0b; 64],
    };
    let encoded = encode_friend_accepted(&acc).expect("encode");
    pin_hex(
        "EXPECTED_FRIEND_LINK_ACCEPTED_HEX",
        &hex::encode(&encoded),
        EXPECTED_FRIEND_LINK_ACCEPTED_HEX,
    );

    // Structural: field names (no rename) → from_addr / display / enrollment /
    // sig.
    let value: ciborium::Value = ciborium::de::from_reader(&encoded[..]).expect("decode as value");
    let map = value.as_map().expect("FriendLinkAccepted is a CBOR map");
    let keys: Vec<&str> = map.iter().filter_map(|(k, _)| k.as_text()).collect();
    for k in ["from_addr", "display", "enrollment", "sig"] {
        assert!(keys.contains(&k), "FriendLinkAccepted missing \"{k}\" key");
    }
    assert_eq!(
        map.len(),
        4,
        "FriendLinkAccepted should encode 4 keys (display present)"
    );
}
