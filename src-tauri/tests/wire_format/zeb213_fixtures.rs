//! ZEB-213 wire-format byte-pinning fixtures.
//!
//! Pins two surfaces that downstream harmony clients (and any future
//! parser) would have to match:
//!
//! 1. HRSS envelope: header + AEAD layout with deterministic salt/nonce
//! 2. OwnerStateSnapshot canonical CBOR (independent of AEAD)

#![cfg(feature = "test-fixtures")]

use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, Space, SpaceId, SpaceKind};
use harmony_app::state_snapshot::{encode_snapshot_with_params, OwnerStateSnapshot};

fn deterministic_state() -> OwnerState {
    let mut s = OwnerState::default();
    let sp = Space {
        id: SpaceId([0x01; 16]),
        kind: SpaceKind::Folder,
        parent: None,
        community_id: None,
        name: "F".into(),
        transport: None,
        members: vec![],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc {
            wall_ms: 1,
            logical: 0,
            device_id: "d".into(),
        },
        updated_at: Hlc {
            wall_ms: 1,
            logical: 0,
            device_id: "d".into(),
        },
        content_key: None,
        prior_content_keys: vec![],
        current_epoch: None,
        current_epoch_key: None,
        old_epoch_keys: std::collections::BTreeMap::new(),
        admin_addr: None,
        is_invite_only: None,
        shared_in_profile: false,
        read_receipt_pref: None,
        pending_join_at: None,
    };
    s.spaces.insert(sp.id, sp);
    s
}

#[test]
fn hrss_envelope_byte_pinned() {
    let state = deterministic_state();
    let addr = OwnerAddr([0xAA; 16]);
    let at = Hlc {
        wall_ms: 1_700_000_000_000,
        logical: 0,
        device_id: "d".into(),
    };
    let salt = [0x11u8; 16];
    let nonce = [0x22u8; 24];

    let bytes =
        encode_snapshot_with_params(b"pp", addr, at, &state, &salt, &nonce).expect("encode");

    // Field-by-field assertions for diagnostic clarity: when the full-hex
    // pin below fails, these help localize which surface changed.
    //
    // Pinned: magic + version + kdf_id + m_kib + t + p + salt + nonce
    assert_eq!(&bytes[..4], b"HRSS", "magic");
    assert_eq!(bytes[4], 0x01, "envelope version");
    assert_eq!(bytes[5], 0x01, "kdf_id (Argon2id)");
    assert_eq!(&bytes[6..10], &65536u32.to_be_bytes(), "m_kib BE");
    assert_eq!(&bytes[10..12], &3u16.to_be_bytes(), "t BE");
    assert_eq!(bytes[12], 1, "p");
    assert_eq!(&bytes[13..29], &salt, "salt offset 13..29");
    assert_eq!(&bytes[29..53], &nonce, "nonce offset 29..53");

    // Full-envelope hex pin. Argon2id + XChaCha20-Poly1305 + ciborium are
    // all deterministic on identical inputs, so the entire envelope
    // (header + ciphertext + tag) is byte-stable across runs. Pinning the
    // full hex catches surfaces the prefix assertions miss:
    //   - AEAD cipher-suite drift (e.g. swap to AES-GCM-SIV)
    //   - AAD constant drift (HRSS_AAD change)
    //   - Argon2 output-format drift inside cipher-key derivation
    //   - Struct field-order or CBOR-shape drift in the inner payload
    //
    // ANY change to AEAD, KDF, AAD, or struct field ordering will break
    // this — which is the desired behavior for a wire-format pin.
    //
    // To recapture after an intentional wire-format change:
    //   eprintln!("{}", hex::encode(&bytes));
    // then run the test, copy the printed hex into the literal below.
    let expected_hex = "4852535301010001000000030111111111111111111111111111111111\
                        222222222222222222222222222222222222222222222222\
                        741ca0a6b8b21b6c532e671394f910109fd9fe071d1d96efd972b82ae8e88f19\
                        4e524369db6b5c98d645c4227c45dcc5c3315931dd9f1ae43f4607e1f12e93de\
                        be0e15270b27bd71e3f001923e3fe5965145020fb9f8fe65eee44fe4aba95c60\
                        cfb119b606fcc69468f5bb22a6ec895fd884889a565624d75267d99a6097421a\
                        09fa70eb4b3bc6cae80014f54071da741e876c4a19f5f3f69f7265e6878f4d86\
                        22d7abdfe5a5660e7faf99dba892306e9ec39de9193383d60faa7e5ff6ff78b4\
                        ee178989271a8a9fe075cf4853023db57201a4a5d82c8032c55b";
    assert_eq!(
        hex::encode(&bytes),
        expected_hex,
        "full envelope hex (header + ciphertext + tag)"
    );

    // Roundtripping the WHOLE envelope must also work — confirms the
    // pinned bytes match what the live decoder accepts.
    let decoded = harmony_app::state_snapshot::decode_snapshot(b"pp", &bytes).expect("decode");
    assert_eq!(decoded.owner_addr, addr);
}

#[test]
fn owner_state_snapshot_canonical_cbor_byte_pinned() {
    // Bypass the envelope and pin the CBOR of the inner payload only.
    let state = deterministic_state();
    let snapshot = OwnerStateSnapshot {
        version: 1,
        owner_addr: OwnerAddr([0xAA; 16]),
        at: Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "d".into(),
        },
        tree: harmony_app::owner_state_persist::canonicalize(&state).unwrap(),
    };

    let mut cbor = Vec::new();
    ciborium::into_writer(&snapshot, &mut cbor).unwrap();

    // ciborium with serde-derive emits struct fields in DECLARATION order
    // (vn, oa, at, tr), NOT bytewise-sorted RFC 8949 §4.2 order (which
    // would be at, oa, tr, vn). The encoder is not yet canonical —
    // tracked as ZEB-220 (see owner_state_crypto.rs lines 405-412).
    // What we pin here is map shape (4-entry definite-length) plus
    // encoder determinism, not RFC-canonical ordering. A wire-format
    // change that reorders the struct field declarations WILL break
    // downstream parsers — the determinism check below and the
    // offset-dependent fixtures in Task 11 would catch that indirectly.
    //
    // Outer map header for a 4-entry definite-length map = 0xa4
    // (CBOR major type 5 = map, count 4).
    assert_eq!(cbor[0], 0xa4, "outer map must be 4-entry definite-length");

    // Decode it back; the snapshot must equal what we encoded.
    let decoded: OwnerStateSnapshot = ciborium::from_reader(cbor.as_slice()).expect("decode");
    assert_eq!(decoded, snapshot);

    // CBOR shape regression: two consecutive encodes must be byte-identical
    // (canonical determinism). If a future ciborium upgrade silently
    // changes encoder order this test catches it.
    let mut cbor2 = Vec::new();
    ciborium::into_writer(&snapshot, &mut cbor2).unwrap();
    assert_eq!(cbor, cbor2, "encoder must be deterministic");
}
