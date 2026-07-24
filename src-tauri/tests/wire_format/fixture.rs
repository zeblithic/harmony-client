//! Pin the v1 wire format. Catches accidental byte-layout drift early.
//!
//! The fixture file at tests/fixtures/encrypted_v1.bin is generated once via
//! the GENERATE_FIXTURE flag below and then committed. Future runs assert
//! byte-equality against the committed fixture.
//!
//! To regenerate (only needed if the v1 format intentionally changes — and
//! at that point you should bump format_version to v2 and add a v2 fixture
//! instead): set the env var HARMONY_REGENERATE_WIRE_FIXTURE=1 and run this
//! test once. It will overwrite the fixture file. Then commit and run again
//! without the env var to confirm the assertion passes.

use harmony_app::identity::test_only::encrypt_with_params_for_test;
use std::path::PathBuf;

const TEST_PASSPHRASE: &[u8] = b"correct horse battery staple";
const TEST_SALT: [u8; 16] = [0xAB; 16];
const TEST_NONCE: [u8; 24] = [0xCD; 24];
const TEST_BLOB: [u8; 32] = [0x42; 32];

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("encrypted_v1.bin")
}

#[test]
fn wire_format_v1_pinned() {
    let bytes = encrypt_with_params_for_test(TEST_PASSPHRASE, &TEST_SALT, &TEST_NONCE, &TEST_BLOB);
    assert_eq!(bytes.len(), 101, "v1 format must be exactly 101 bytes");

    let path = fixture_path();

    if std::env::var("HARMONY_REGENERATE_WIRE_FIXTURE").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &bytes).expect("write fixture");
        eprintln!("Regenerated fixture at {}", path.display());
        return;
    }

    let expected = std::fs::read(&path).unwrap_or_else(|_| {
        panic!(
            "Fixture missing at {}.\n\
             First-time setup: run with HARMONY_REGENERATE_WIRE_FIXTURE=1 to generate, then commit.",
            path.display()
        )
    });

    assert_eq!(
        bytes, expected,
        "WIRE FORMAT CHANGED — bump format_version and add a v2 fixture before regenerating"
    );
}

/// Sub-D Phase 4 (ZEB-281) wire-compat invariant: a Space with
/// `shared_in_profile: false` (the default) encodes byte-identically to
/// a Space constructed before the field existed. Powered by
/// `#[serde(rename = "sp", default, skip_serializing_if =
/// "core::ops::Not::not")]` on `Space.shared_in_profile`.
///
/// If this test fails, the `skip_serializing_if` invariant has been
/// inadvertently changed — Phase 4 broke cross-version owner-state
/// CRDT compat. Fix the field attrs, don't update the test.
#[test]
fn space_shared_in_profile_default_false_byte_identical_to_pre_phase4() {
    use harmony_app::owner_state_crypto::canonical_cbor_encode;
    use harmony_app::owner_state_types::{Hlc, OwnerAddr, Space, SpaceId, SpaceKind};

    // Construct a minimal community Space with default-false
    // shared_in_profile. The encoded bytes MUST NOT contain a "sp" key.
    let space = Space {
        id: SpaceId([1u8; 16]),
        kind: SpaceKind::Community,
        parent: None,
        community_id: Some(SpaceId([2u8; 16])),
        name: "test".to_string(),
        transport: None,
        members: vec![OwnerAddr([3u8; 16])],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc {
            wall_ms: 1700000000000,
            logical: 0,
            device_id: "fix".into(),
        },
        updated_at: Hlc {
            wall_ms: 1700000000000,
            logical: 0,
            device_id: "fix".into(),
        },
        content_key: None,
        prior_content_keys: vec![],
        current_epoch: Some(0),
        current_epoch_key: None,
        old_epoch_keys: Default::default(),
        admin_addr: Some(OwnerAddr([3u8; 16])),
        is_invite_only: Some(false),
        shared_in_profile: false, // The Phase 4 field, default
        pending_join_at: None,
    };

    let bytes = canonical_cbor_encode(&space).expect("encode");

    // Decode and walk the CBOR map; "sp" key MUST NOT appear.
    let value: ciborium::value::Value = ciborium::de::from_reader(&bytes[..]).expect("decode");
    let map = match value {
        ciborium::value::Value::Map(m) => m,
        other => panic!("expected CBOR map, got {other:?}"),
    };
    let keys: Vec<String> = map
        .iter()
        .filter_map(|(k, _)| match k {
            ciborium::value::Value::Text(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert!(
        !keys.iter().any(|k| k == "sp"),
        "Space with default-false shared_in_profile must NOT emit \"sp\" key on the wire; \
         observed keys: {keys:?}"
    );
}

/// Companion test: a Space with `shared_in_profile: true` DOES emit "sp" → true.
#[test]
fn space_shared_in_profile_true_emits_sp_key() {
    use harmony_app::owner_state_crypto::canonical_cbor_encode;
    use harmony_app::owner_state_types::{Hlc, OwnerAddr, Space, SpaceId, SpaceKind};

    let space = Space {
        id: SpaceId([1u8; 16]),
        kind: SpaceKind::Community,
        parent: None,
        community_id: Some(SpaceId([2u8; 16])),
        name: "test".to_string(),
        transport: None,
        members: vec![OwnerAddr([3u8; 16])],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc {
            wall_ms: 1700000000000,
            logical: 0,
            device_id: "fix".into(),
        },
        updated_at: Hlc {
            wall_ms: 1700000000000,
            logical: 0,
            device_id: "fix".into(),
        },
        content_key: None,
        prior_content_keys: vec![],
        current_epoch: Some(0),
        current_epoch_key: None,
        old_epoch_keys: Default::default(),
        admin_addr: Some(OwnerAddr([3u8; 16])),
        is_invite_only: Some(false),
        shared_in_profile: true,
        pending_join_at: None,
    };

    let bytes = canonical_cbor_encode(&space).expect("encode");
    let value: ciborium::value::Value = ciborium::de::from_reader(&bytes[..]).expect("decode");
    let map = match value {
        ciborium::value::Value::Map(m) => m,
        other => panic!("expected CBOR map, got {other:?}"),
    };
    let sp_value = map
        .iter()
        .find_map(|(k, v)| match (k, v) {
            (ciborium::value::Value::Text(s), v) if s == "sp" => Some(v.clone()),
            _ => None,
        })
        .expect("Space with shared_in_profile: true must emit \"sp\" key");
    assert_eq!(sp_value, ciborium::value::Value::Bool(true));
}

use harmony_app::identity::test_only::encrypt_vault_with_params_for_test;

const V2_PASSPHRASE: &[u8] = b"correct horse battery staple";
const V2_SALT: [u8; 16] = [0x1A; 16];
const V2_NONCE: [u8; 24] = [0x2B; 24];
// Fixed arbitrary plaintext — the v0x02 envelope protects opaque bytes, so
// the pin is independent of SecretVault CBOR shape.
const V2_PLAINTEXT: [u8; 48] = [0x5C; 48];

fn fixture_v2_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("encrypted_v2.bin")
}

#[test]
fn wire_format_v2_pinned() {
    let bytes =
        encrypt_vault_with_params_for_test(V2_PASSPHRASE, &V2_PLAINTEXT, &V2_SALT, &V2_NONCE);
    // header(13) + salt(16) + nonce(24) + plaintext(48) + tag(16) = 117
    assert_eq!(bytes.len(), 117, "v0x02 envelope length");
    assert_eq!(&bytes[..4], b"HRMI", "magic");
    assert_eq!(bytes[4], 0x02, "v0x02 format version");

    let path = fixture_v2_path();
    if std::env::var("HARMONY_REGENERATE_WIRE_FIXTURE").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &bytes).expect("write fixture");
        eprintln!("Regenerated v2 fixture at {}", path.display());
        return;
    }
    let expected = std::fs::read(&path).unwrap_or_else(|_| {
        panic!(
            "Fixture missing at {}.\nFirst-time setup: run with HARMONY_REGENERATE_WIRE_FIXTURE=1 to generate, then commit.",
            path.display()
        )
    });
    assert_eq!(
        bytes, expected,
        "v0x02 WIRE FORMAT CHANGED — this envelope must stay byte-identical across the password_envelope rewire"
    );

    // Round-trip through the live decoder confirms the pinned bytes decrypt.
    // `decrypt_vault_bytes` is `pub(crate)`; use the gated test_only re-export.
    let back =
        harmony_app::identity::test_only::decrypt_vault_bytes_for_test(V2_PASSPHRASE, &bytes)
            .expect("decrypt pinned v2 envelope");
    assert_eq!(&back[..], &V2_PLAINTEXT[..], "round-trip plaintext");
}
