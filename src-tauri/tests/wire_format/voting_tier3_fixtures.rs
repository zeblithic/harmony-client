//! ZEB-309 Phase 4a-main: canonical CBOR fixtures for Tier 3 wire format.
//!
//! Follows the ZEB-250 regen-on-first-run pattern (see
//! tests/wire_format/zeb250_fixtures.rs). To regenerate after
//! intentional wire-format changes:
//!
//!     REGENERATE_VOTING_TIER3_FIXTURES=1 cargo nextest run \
//!         --locked -p harmony-app --features test-fixtures \
//!         -E 'test(fixture_tier3)'
//!
//! Then commit the new .cbor files. Without REGENERATE set, the tests
//! compare each fixture against the pinned bytes and fail loudly if
//! the wire format changed.

use harmony_app::community_voting_core::{
    BridgingVoteCode, DeliberationStatementPayload, DeliberationVotePayload, DraftApprovalPayload,
    DraftCandidatePayload, Eligibility, MiniPublicDeclinePayload, PollClosePayload, PollId,
    RatificationBallotPayload, SortitionFailedPayload, SortitionSelectionPayload,
    Tier3PollConfigPayload,
};
use harmony_app::community_voting_star::{CandidateRef, StarResult};
use harmony_app::community_voting_tier3::Tier3PollResultPayload;
use harmony_app::owner_state_types::OwnerAddr;
use std::path::PathBuf;

const FIXTURE_DIR: &str = "tests/fixtures/voting_tier3";
const REGENERATE_ENV: &str = "REGENERATE_VOTING_TIER3_FIXTURES";

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

    // CBOR-encode the value
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(value, &mut bytes).expect("encode");

    // Sanity: decoding our own freshly-encoded bytes matches
    let decoded: T = ciborium::de::from_reader(bytes.as_slice()).expect("decode self");
    assert_eq!(*value, decoded, "self round-trip failed for {name}");

    if std::env::var(REGENERATE_ENV).is_ok() {
        // Regenerate mode: write to disk + panic with instructions
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixtures dir");
        }
        std::fs::write(&path, &bytes).expect("write fixture");
        panic!(
            "REGENERATED {name}: wrote {} bytes to {}. \
             Unset {REGENERATE_ENV} and re-run to verify; commit the binary fixture if intentional.",
            bytes.len(),
            path.display(),
        );
    }

    // Compare mode: pinned bytes must match
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

    // Decode pinned and verify round-trip
    let from_pin: T = ciborium::de::from_reader(pinned.as_slice()).expect("decode pinned");
    assert_eq!(*value, from_pin, "decoded pinned != value for {name}");
}

/// Structural CBOR-key check: confirm a payload uses 2-char same-length keys.
fn assert_two_char_keys(bytes: &[u8], expected_keys: &[&str]) {
    let value: ciborium::Value = ciborium::de::from_reader(bytes).expect("decode as Value");
    let map = value.as_map().expect("top-level is a CBOR map");
    let actual_keys: Vec<&str> = map.iter().filter_map(|(k, _)| k.as_text()).collect();
    for k in expected_keys {
        assert!(actual_keys.contains(k), "expected key {k:?} missing");
    }
    for k in &actual_keys {
        assert_eq!(k.len(), 2, "key {k:?} violates 2-char invariant");
    }
}

// ─── 11 fixtures ──────────────────────────────────────────────────

#[test]
fn fixture_tier3_poll_create_payload() {
    // Build a representative Tier3PollConfigPayload with retry_of=None
    // and ALL fields set (so the fixture covers maximum schema).
    let pd = Tier3PollConfigPayload {
        proposal_text: "Amend community charter §3: require 2/3 supermajority".into(),
        sortition_size: 100,
        deliberation_window_seconds: 1_209_600,
        drafting_window_seconds: 604_800,
        ratification_window_seconds: 1_209_600,
        privacy_mode: "pu".into(),
        incentive_mode: "d".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
        predecessor: None,
    };
    round_trip_or_regen("tier3_poll_create.cbor", &pd);
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&pd, &mut bytes).expect("encode");
    // 2-char keys
    assert_two_char_keys(&bytes, &["pt", "ss", "dw", "fw", "rw", "pm", "im", "el"]);
}

#[test]
fn fixture_tier3_poll_create_with_retry_of() {
    let pd = Tier3PollConfigPayload {
        proposal_text: "Retry: Amend §3".into(),
        sortition_size: 50,
        deliberation_window_seconds: 604_800,
        drafting_window_seconds: 604_800,
        ratification_window_seconds: 604_800,
        privacy_mode: "pu".into(),
        incentive_mode: "d".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: Some(PollId([0xab; 32])),
        predecessor: None,
    };
    round_trip_or_regen("tier3_poll_create_retry.cbor", &pd);
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&pd, &mut bytes).expect("encode");
    // "ro" key is present when retry_of is Some; all keys must be 2-char.
    // Cluster 8 fix (CodeRabbit minor, R1 bot review): add missing 2-char key check.
    assert_two_char_keys(
        &bytes,
        &["pt", "ss", "dw", "fw", "rw", "pm", "im", "el", "ro"],
    );
}

/// ZEB-1031 Task 7: relaunch-of-a-voided-poll form — `predecessor` ("pv")
/// present. A NEW pin (never edits the two above): optional-key evolution
/// must leave the `retry_of`-less/`predecessor`-less encodings byte-
/// identical, which the two fixtures above (both now carrying an explicit
/// `predecessor: None`, still absent from the wire) already prove.
#[test]
fn fixture_tier3_poll_create_with_predecessor() {
    let pd = Tier3PollConfigPayload {
        proposal_text: "Relaunch: Amend §3 (post-reset)".into(),
        sortition_size: 50,
        deliberation_window_seconds: 604_800,
        drafting_window_seconds: 604_800,
        ratification_window_seconds: 604_800,
        privacy_mode: "se".into(),
        incentive_mode: "d".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        retry_of: None,
        predecessor: Some(PollId([0xcd; 32])),
    };
    round_trip_or_regen("tier3_poll_create_predecessor.cbor", &pd);
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&pd, &mut bytes).expect("encode");
    // "pv" key is present when predecessor is Some; all keys must be 2-char.
    assert_two_char_keys(
        &bytes,
        &["pt", "ss", "dw", "fw", "rw", "pm", "im", "el", "pv"],
    );
}

#[test]
fn fixture_sortition_selection() {
    let pd = SortitionSelectionPayload {
        poll_id: PollId([0x42; 32]),
        primary: vec![
            OwnerAddr([0x01; 16]),
            OwnerAddr([0x02; 16]),
            OwnerAddr([0x03; 16]),
        ],
        backup: vec![
            OwnerAddr([0x04; 16]),
            OwnerAddr([0x05; 16]),
            OwnerAddr([0x06; 16]),
        ],
    };
    round_trip_or_regen("sortition_selection.cbor", &pd);
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&pd, &mut bytes).expect("encode");
    assert_two_char_keys(&bytes, &["pi", "pr", "bk"]);
}

#[test]
fn fixture_deliberation_statement() {
    let pd = DeliberationStatementPayload {
        poll_id: PollId([0x42; 32]),
        text: "I think we should weigh the precedent carefully before deciding.".into(),
    };
    round_trip_or_regen("deliberation_statement.cbor", &pd);
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&pd, &mut bytes).expect("encode");
    assert_two_char_keys(&bytes, &["pi", "tx"]);
}

#[test]
fn fixture_mini_public_decline() {
    let pd = MiniPublicDeclinePayload {
        poll_id: PollId([0x42; 32]),
        reason: Some("nt".into()),
    };
    round_trip_or_regen("mini_public_decline.cbor", &pd);
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&pd, &mut bytes).expect("encode");
    assert_two_char_keys(&bytes, &["pi", "rs"]);
}

#[test]
fn fixture_draft_candidate() {
    let pd = DraftCandidatePayload {
        poll_id: PollId([0x42; 32]),
        text: "Adopt a 2/3 supermajority threshold for moderator dismissals.".into(),
    };
    round_trip_or_regen("draft_candidate.cbor", &pd);
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&pd, &mut bytes).expect("encode");
    // Cluster 8 fix (CodeRabbit minor, R1 bot review): add missing 2-char key check.
    assert_two_char_keys(&bytes, &["pi", "tx"]);
}

#[test]
fn fixture_draft_approval() {
    let pd = DraftApprovalPayload {
        poll_id: PollId([0x42; 32]),
        candidate_event_hash: [0xcc; 32],
    };
    round_trip_or_regen("draft_approval.cbor", &pd);
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&pd, &mut bytes).expect("encode");
    assert_two_char_keys(&bytes, &["pi", "ch"]);
}

#[test]
fn fixture_sortition_failed() {
    let pd = SortitionFailedPayload {
        poll_id: PollId([0x42; 32]),
    };
    round_trip_or_regen("sortition_failed.cbor", &pd);
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&pd, &mut bytes).expect("encode");
    // Cluster 8 fix (CodeRabbit minor, R1 bot review): add missing 2-char key check.
    assert_two_char_keys(&bytes, &["pi"]);
}

#[test]
fn fixture_ratification_ballot() {
    let pd = RatificationBallotPayload {
        poll_id: PollId([0x42; 32]),
        scores: Some(vec![5, 3, 4, 0, 2]),
        ciphertexts_scores: None,
        ciphertexts_indicators: None,
        proof: None,
    };
    round_trip_or_regen("ratification_ballot.cbor", &pd);
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&pd, &mut bytes).expect("encode");
    assert_two_char_keys(&bytes, &["pi", "sc"]);
}

#[test]
fn fixture_tier3_poll_close() {
    let pd = PollClosePayload {
        poll_id: PollId([0x42; 32]),
    };
    round_trip_or_regen("tier3_poll_close.cbor", &pd);
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&pd, &mut bytes).expect("encode");
    assert_two_char_keys(&bytes, &["pi"]);
}

#[test]
fn fixture_tier3_poll_result() {
    let pd = Tier3PollResultPayload {
        poll_id: PollId([0x42; 32]),
        result: StarResult {
            winner: CandidateRef {
                event_hash: [0xaa; 32],
                approval_count: 3,
            },
            finalists: vec![
                CandidateRef {
                    event_hash: [0xaa; 32],
                    approval_count: 3,
                },
                CandidateRef {
                    event_hash: [0xbb; 32],
                    approval_count: 2,
                },
            ],
            total_scores: vec![15, 10],
            runoff_votes: vec![3, 1],
        },
    };
    round_trip_or_regen("tier3_poll_result.cbor", &pd);
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&pd, &mut bytes).expect("encode");
    assert_two_char_keys(&bytes, &["pi", "rs"]);
}

/// ZEB-294: canonical CBOR bytes for `DeliberationVotePayload` (kd=dv).
///
/// Pins the Agree variant (vote=0). Disagree (1) and Pass (2) share the
/// same payload shape — only the "vt" field value differs — so one
/// representative fixture is sufficient for wire-format regression
/// detection.
///
/// To regenerate:
///     REGENERATE_VOTING_TIER3_FIXTURES=1 cargo nextest run \
///         --locked --all-targets -p harmony-app --features test-fixtures \
///         -E 'test(fixture_tier3_deliberation_vote)'
#[test]
fn fixture_tier3_deliberation_vote() {
    let pd = DeliberationVotePayload {
        poll_id: PollId([0x42; 32]),
        statement_event_hash: [0x7c; 32],
        vote: BridgingVoteCode::Agree.as_u8(),
    };
    round_trip_or_regen("deliberation_vote.cbor", &pd);
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&pd, &mut bytes).expect("encode");
    // Keys: "pi" (poll_id), "sh" (statement_event_hash), "vt" (vote).
    assert_two_char_keys(&bytes, &["pi", "sh", "vt"]);
}
