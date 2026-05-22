//! ZEB-295 Phase 6 Task 10: canonical CBOR byte-pins for the new ballot-secret
//! wire payloads (kd=rb se-mode at n=3,5 + kd=ts at n=3,5 + pre/post CHURP
//! rotation epoch encoding).
//!
//! Wire-format contract preservation:
//! - `RatificationBallotPayload` se-mode keys: `{pi, cs, in, pf}` (4 keys,
//!   2-char each).
//! - `TallySharePayload` keys: `{pi, ce, ts}` (3 keys, 2-char each).
//!
//! The ciphertext byte fields (`c1`, `c2`, `sh`, `dp`) are filled with
//! deterministic fixed bytes — the wire format does not validate that they
//! represent actual ElGamal points or DLEQ proofs at the CBOR layer. This
//! test pins **encoding shape**, not crypto soundness; the latter is covered
//! by the unit tests in `community_voting_tier3.rs`.
//!
//! ## Regen pattern
//!
//! Follows the same regen pattern as `tests/wire_format_voting_tier3_fixtures.rs`
//! (ZEB-309 Phase 4a-main). To regenerate the binary fixtures after an
//! intentional wire-format change:
//!
//!     REGENERATE_VOTING_TIER3_SECRET_FIXTURES=1 cargo nextest run \
//!         --locked -p harmony-app --features test-fixtures \
//!         --test wire_format_voting_tier3_secret_fixtures
//!
//! Then commit the new `.cbor` files in `tests/fixtures/voting_tier3_secret/`.
//! Without the env var set, each test compares against the pinned bytes and
//! fails loudly on drift.

#![cfg(feature = "test-fixtures")]

use harmony_app::community_voting_core::{
    BallotNIZKProof, EncCiphertext, PollId, RatificationBallotPayload, TallyShareEntry,
    TallySharePayload,
};
use harmony_app::community_voting_tier3_nizk::{ConsistencyProof, Range5Proof};
use std::path::PathBuf;

const FIXTURE_DIR: &str = "tests/fixtures/voting_tier3_secret";
const REGENERATE_ENV: &str = "REGENERATE_VOTING_TIER3_SECRET_FIXTURES";

fn fixture_path(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push(FIXTURE_DIR);
    p.push(name);
    p
}

fn round_trip_or_regen<T>(name: &str, value: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let path = fixture_path(name);
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(value, &mut bytes).expect("encode");
    let decoded: T = ciborium::de::from_reader(bytes.as_slice()).expect("decode self");
    assert_eq!(*value, decoded, "self round-trip failed for {name}");
    if std::env::var(REGENERATE_ENV).is_ok() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixtures dir");
        }
        std::fs::write(&path, &bytes).expect("write fixture");
        panic!(
            "REGENERATED {name}: wrote {} bytes to {}. \
             Unset {REGENERATE_ENV} and re-run to verify; commit the binary fixture.",
            bytes.len(),
            path.display(),
        );
    }
    let pinned = std::fs::read(&path).unwrap_or_else(|_| {
        panic!(
            "fixture not found at {}. Set {REGENERATE_ENV}=1 to generate.",
            path.display(),
        )
    });
    assert_eq!(
        bytes, pinned,
        "wire format drift for {name}: pin no longer matches encoding. \
         If intentional, regenerate with {REGENERATE_ENV}=1."
    );
    let from_pin: T = ciborium::de::from_reader(pinned.as_slice()).expect("decode pinned");
    assert_eq!(*value, from_pin, "decoded pinned != value for {name}");
}

// ─── Shared deterministic constants ──────────────────────────────────────────

const PI_BYTES: [u8; 32] = [0x42; 32];

fn enc_ct(c1_byte: u8, c2_byte: u8) -> EncCiphertext {
    EncCiphertext {
        c1: [c1_byte; 32],
        c2: [c2_byte; 32],
    }
}

fn ts_entry(share_byte: u8, proof_byte: u8) -> TallyShareEntry {
    TallyShareEntry {
        share: [share_byte; 32],
        dleq_proof: [proof_byte; 64],
    }
}

/// Build a deterministic se-mode RatificationBallotPayload at the given `n`.
/// Ciphertext byte fields are fixed (`0xAA`/`0xBB` for scores, `0xCC`/`0xDD`
/// for indicators); range/consistency proof blobs are `0xEE`/`0xFF` runs of
/// the contractual length (Range5Proof::SIZE·n and ConsistencyProof::SIZE·C(n,2)).
fn build_rb_se(n: usize) -> RatificationBallotPayload {
    let pair_count = n * (n - 1) / 2;
    RatificationBallotPayload {
        poll_id: PollId(PI_BYTES),
        scores: None,
        ciphertexts_scores: Some((0..n).map(|_| enc_ct(0xAA, 0xBB)).collect()),
        ciphertexts_indicators: Some((0..pair_count).map(|_| enc_ct(0xCC, 0xDD)).collect()),
        proof: Some(BallotNIZKProof {
            range_proofs: vec![0xEE; Range5Proof::SIZE * n],
            consistency_proofs: vec![0xFF; ConsistencyProof::SIZE * pair_count],
        }),
    }
}

/// Build a deterministic TallySharePayload at the given `n` and CHURP epoch.
/// Entry count is `n + C(n,2)` per spec.
fn build_ts(n: usize, epoch: u32) -> TallySharePayload {
    let pair_count = n * (n - 1) / 2;
    let total = n + pair_count;
    TallySharePayload {
        poll_id: PollId(PI_BYTES),
        committee_epoch: epoch,
        entries: (0..total).map(|_| ts_entry(0xA1, 0xB2)).collect(),
    }
}

// ─── Test 1: kd=rb (se-mode) at n=3 ──────────────────────────────────────────

/// n=3 → cs.len()=3, in.len()=3 (C(3,2)),
/// range_proofs = Range5Proof::SIZE·3, consistency_proofs = ConsistencyProof::SIZE·3.
#[test]
fn fixture_rb_se_n3_round_trip_and_byte_pin() {
    let payload = build_rb_se(3);
    round_trip_or_regen("rb_se_n3.cbor", &payload);
}

#[test]
fn fixture_rb_se_n5_round_trip_and_byte_pin() {
    // n=5 → cs.len()=5, in.len()=10 (C(5,2)),
    // range_proofs = Range5Proof::SIZE·5, consistency_proofs = ConsistencyProof::SIZE·10.
    // n=5 is the spec's documented max-supported candidate count.
    let payload = build_rb_se(5);
    round_trip_or_regen("rb_se_n5.cbor", &payload);
}

#[test]
fn fixture_ts_n3_round_trip_and_byte_pin() {
    // n=3 → entries.len() = 3 + 3 = 6. epoch=0 (sentinel "new committee").
    let payload = build_ts(3, 0);
    round_trip_or_regen("ts_n3.cbor", &payload);
}

#[test]
fn fixture_ts_n5_round_trip_and_byte_pin() {
    // n=5 → entries.len() = 5 + 10 = 15. epoch=0.
    let payload = build_ts(5, 0);
    round_trip_or_regen("ts_n5.cbor", &payload);
}

/// Pin a single payload against `tests/fixtures/voting_tier3_secret/<name>`,
/// **without** panicking on REGEN. Used by the rotation test which must
/// regenerate two fixtures in one run.
fn pin_or_regen_nopanic<T>(name: &str, value: &T) -> Result<(), String>
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let path = fixture_path(name);
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(value, &mut bytes).map_err(|e| format!("encode: {e}"))?;
    let decoded: T = ciborium::de::from_reader(bytes.as_slice())
        .map_err(|e| format!("decode self {name}: {e}"))?;
    if *value != decoded {
        return Err(format!("self round-trip failed for {name}"));
    }
    if std::env::var(REGENERATE_ENV).is_ok() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
        }
        std::fs::write(&path, &bytes).map_err(|e| format!("write fixture: {e}"))?;
        eprintln!(
            "REGENERATED {name}: wrote {} bytes to {}",
            bytes.len(),
            path.display(),
        );
        return Ok(());
    }
    let pinned =
        std::fs::read(&path).map_err(|_| format!("fixture not found at {}", path.display()))?;
    if bytes != pinned {
        return Err(format!(
            "wire format drift for {name}: pin no longer matches encoding"
        ));
    }
    let from_pin: T =
        ciborium::de::from_reader(pinned.as_slice()).map_err(|e| format!("decode pin: {e}"))?;
    if *value != from_pin {
        return Err(format!("decoded pinned != value for {name}"));
    }
    Ok(())
}

/// Same poll, same `n=3`, only the CHURP epoch differs. Bytes MUST differ
/// because the `ce` field is on the wire and must round-trip distinctly
/// per epoch (spec §5.3 multi-epoch fall-through). This is the CHURP-
/// rotation wire-encoding sentinel — if the `ce` field is ever silently
/// dropped from the encoder, the two byte streams would collide and
/// tally shares from different rotations would become indistinguishable.
#[test]
fn fixture_ts_pre_post_rotation_different_epoch_values() {
    let pre = build_ts(3, 7);
    let post = build_ts(3, 8);
    assert_eq!(pre.committee_epoch, 7);
    assert_eq!(post.committee_epoch, 8);
    let mut a = Vec::new();
    ciborium::ser::into_writer(&pre, &mut a).unwrap();
    let mut b = Vec::new();
    ciborium::ser::into_writer(&post, &mut b).unwrap();
    assert_ne!(
        a, b,
        "pre/post-rotation TallySharePayloads must encode to different bytes \
         (CHURP rotation discriminator)"
    );
    let dec_a: TallySharePayload = ciborium::de::from_reader(a.as_slice()).expect("decode pre");
    let dec_b: TallySharePayload = ciborium::de::from_reader(b.as_slice()).expect("decode post");
    assert_eq!(dec_a.committee_epoch, 7);
    assert_eq!(dec_b.committee_epoch, 8);
    assert_eq!(
        dec_a.poll_id, dec_b.poll_id,
        "non-epoch fields must round-trip identically"
    );
    assert_eq!(
        dec_a.entries, dec_b.entries,
        "entry-vector contents are identical aside from epoch"
    );
    // Pin both byte streams so subsequent epoch-encoding drift is surfaced
    // immediately. Uses the no-panic variant so REGEN mode generates BOTH
    // fixtures in a single test invocation.
    let r1 = pin_or_regen_nopanic("ts_n3_epoch_7.cbor", &pre);
    let r2 = pin_or_regen_nopanic("ts_n3_epoch_8.cbor", &post);
    if std::env::var(REGENERATE_ENV).is_ok() {
        // In REGEN mode, both succeeded above; panic-with-summary so the
        // test runner surfaces the regen step in the output exactly once.
        panic!(
            "REGENERATED ts_n3_epoch_7.cbor + ts_n3_epoch_8.cbor — unset \
             {REGENERATE_ENV} and re-run to verify."
        );
    }
    r1.expect("epoch=7 pin");
    r2.expect("epoch=8 pin");
}
