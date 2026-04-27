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
