//! ZEB-311: pin CBOR wire-format encoding of Tier3PollExport + Tier3PollSummary.
//!
//! These types are camelCased serde structs that flow through Tauri IPC to
//! the frontend. The CBOR round-trip is what guarantees JS-side field names
//! match the spec. Any field rename or default change must be deliberate and
//! reflected in this fixture.

use harmony_app::{Tier3MyRole, Tier3PollExport, Tier3PollSummary, Tier3StageTag};

/// Decode a CBOR buffer as a map of `String -> Value` and return the
/// sorted set of top-level keys. We use this to assert exact serde
/// rename output, which guards against accidental snake_case drift that
/// would silently break the camelCase TS DTOs on the frontend side.
fn cbor_top_level_keys(buf: &[u8]) -> Vec<String> {
    let v: ciborium::value::Value = ciborium::from_reader(buf).expect("decode as Value");
    let map = match v {
        ciborium::value::Value::Map(m) => m,
        other => panic!("expected CBOR map at root, got {other:?}"),
    };
    let mut keys: Vec<String> = map
        .into_iter()
        .map(|(k, _)| match k {
            ciborium::value::Value::Text(s) => s,
            other => panic!("expected text key, got {other:?}"),
        })
        .collect();
    keys.sort();
    keys
}

#[test]
fn tier3_poll_export_round_trips_through_cbor() {
    let export = Tier3PollExport {
        poll_id: "aa".repeat(32),
        community_id: "11".repeat(16),
        proposal_text: "Amend charter §3".to_string(),
        proposer: "22".repeat(32),
        stage: Tier3StageTag::Drafting,
        poll_create_hlc_ms: 1_700_000_000_000,
        sortition_size: 100,
        deliberation_window_seconds: 1_209_600,
        drafting_window_seconds: 604_800,
        ratification_window_seconds: 1_209_600,
        incentive_mode: "d".to_string(),
        mini_public: vec!["33".repeat(32), "44".repeat(32)],
        backup_pool: vec!["55".repeat(32)],
        declined: vec![("44".repeat(32), 1_700_000_500_000)],
        draft_candidates: vec![],
        ratification_candidates: vec![],
        my_role: Tier3MyRole::MiniPublic,
        my_drafting_approvals: vec![],
        my_ratification_scores: None,
        winner_event_hash: None,
        runner_up_event_hash: None,
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&export, &mut buf).expect("encode");
    let decoded: Tier3PollExport = ciborium::from_reader(&buf[..]).expect("decode");
    assert_eq!(decoded.poll_id, export.poll_id);
    assert_eq!(decoded.stage, Tier3StageTag::Drafting);
    assert_eq!(decoded.my_role, Tier3MyRole::MiniPublic);

    // Pin the exact camelCase keys the frontend expects. Adding a field
    // requires updating this list; renaming a field via serde will fail
    // this assertion before it can silently break the UI.
    let expected: Vec<&str> = vec![
        "backupPool",
        "communityId",
        "declined",
        "deliberationWindowSeconds",
        "draftCandidates",
        "draftingWindowSeconds",
        "incentiveMode",
        "miniPublic",
        "myDraftingApprovals",
        "myRatificationScores",
        "myRole",
        "pollCreateHlcMs",
        "pollId",
        "proposalText",
        "proposer",
        "ratificationCandidates",
        "ratificationWindowSeconds",
        "runnerUpEventHash",
        "sortitionSize",
        "stage",
        "winnerEventHash",
    ];
    let actual = cbor_top_level_keys(&buf);
    assert_eq!(
        actual,
        expected.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "Tier3PollExport CBOR key drift — update TS Tier3PollExport in lockstep",
    );
}

#[test]
fn tier3_poll_summary_round_trips_through_cbor() {
    let summary = Tier3PollSummary {
        poll_id: "aa".repeat(32),
        community_id: "11".repeat(16),
        proposal_text: "Amend charter §3".to_string(),
        proposer: "22".repeat(32),
        stage: Tier3StageTag::Ratification,
        poll_create_hlc_ms: 1_700_000_000_000,
        sortition_size: 100,
        winner_text: None,
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&summary, &mut buf).expect("encode");
    let decoded: Tier3PollSummary = ciborium::from_reader(&buf[..]).expect("decode");
    assert_eq!(decoded.stage, Tier3StageTag::Ratification);
    assert_eq!(decoded.winner_text, None);

    let expected: Vec<&str> = vec![
        "communityId",
        "pollCreateHlcMs",
        "pollId",
        "proposalText",
        "proposer",
        "sortitionSize",
        "stage",
        "winnerText",
    ];
    let actual = cbor_top_level_keys(&buf);
    assert_eq!(
        actual,
        expected.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "Tier3PollSummary CBOR key drift — update TS Tier3PollSummary in lockstep",
    );
}

#[test]
fn tier3_stage_tag_serializes_as_two_char_string() {
    use serde::Serialize;
    use serde_json::{self, Serializer};
    let mut buf = Vec::new();
    let mut ser = Serializer::new(&mut buf);
    Tier3StageTag::Drafting.serialize(&mut ser).unwrap();
    assert_eq!(std::str::from_utf8(&buf).unwrap(), "\"dr\"");
}
