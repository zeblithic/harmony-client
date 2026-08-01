//! ZEB-311: pin CBOR wire-format encoding of Tier3PollExport + Tier3PollSummary.
//!
//! These types are camelCased serde structs that flow through Tauri IPC to
//! the frontend. The CBOR round-trip is what guarantees JS-side field names
//! match the spec. Any field rename or default change must be deliberate and
//! reflected in this fixture.

use harmony_app::{
    DeliberationStatementExport, MyDeliberationVoteExport, Tier3MyRole, Tier3PollExport,
    Tier3PollSummary, Tier3StageTag,
};

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

/// Decode a CBOR buffer and return the sorted keys of the FIRST element of the
/// array under top-level key `array_key` (each element a CBOR map). Pins the
/// camelCase field names of a nested export struct (e.g.
/// `DeliberationStatementExport`) that the top-level key check never reaches.
fn cbor_first_element_keys(buf: &[u8], array_key: &str) -> Vec<String> {
    let v: ciborium::value::Value = ciborium::from_reader(buf).expect("decode as Value");
    let map = match v {
        ciborium::value::Value::Map(m) => m,
        other => panic!("expected CBOR map at root, got {other:?}"),
    };
    let arr = map
        .into_iter()
        .find_map(|(k, val)| match k {
            ciborium::value::Value::Text(s) if s == array_key => Some(val),
            _ => None,
        })
        .unwrap_or_else(|| panic!("top-level key {array_key:?} not found"));
    let elems = match arr {
        ciborium::value::Value::Array(a) => a,
        other => panic!("expected array under {array_key:?}, got {other:?}"),
    };
    let first = elems
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("array under {array_key:?} is empty"));
    let inner = match first {
        ciborium::value::Value::Map(m) => m,
        other => panic!("expected map element under {array_key:?}, got {other:?}"),
    };
    let mut keys: Vec<String> = inner
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
        // ZEB-790 (CodeRabbit #10): non-default HLC tuple so the round-trip
        // actually exercises the logical + device_id fields (all-zero could
        // pass even if the fields were silently dropped).
        poll_create_hlc_logical: 7,
        poll_create_hlc_device_id: "dev-fixture".to_string(),
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
        deliberation_statements: vec![DeliberationStatementExport {
            statement_event_hash: "ab".repeat(32),
            author: "cd".repeat(16),
            text: "Test statement".to_string(),
            created_at_hlc_ms: 1_700_000_100_000,
            // ZEB-790 (CodeRabbit #10): non-default nested HLC tuple.
            created_at_hlc_logical: 5,
            created_at_hlc_device_id: "stmt-dev".to_string(),
            agree_count: 3,
            disagree_count: 1,
            pass_count: 0,
        }],
        my_deliberation_statement_count: 1,
        my_deliberation_votes: vec![MyDeliberationVoteExport {
            statement_event_hash: "ab".repeat(32),
            vote: "agree".to_string(),
        }],
        winner_event_hash: None,
        runner_up_event_hash: None,
        // ZEB-295 Phase 6 Task 9 / spec §6.6: ballot-secret status fields.
        privacy_mode: "pu".to_string(),
        encrypted_tally_share_count: 0,
        encrypted_tally_threshold: 0,
        encrypted_tally_committee_size: 0,
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&export, &mut buf).expect("encode");
    let decoded: Tier3PollExport = ciborium::from_reader(&buf[..]).expect("decode");
    assert_eq!(decoded.poll_id, export.poll_id);
    assert_eq!(decoded.stage, Tier3StageTag::Drafting);
    assert_eq!(decoded.my_role, Tier3MyRole::MiniPublic);
    // ZEB-790 (CodeRabbit #10): the non-default HLC tuple must round-trip —
    // guards against the logical/device_id fields silently dropping.
    assert_eq!(decoded.poll_create_hlc_logical, 7);
    assert_eq!(decoded.poll_create_hlc_device_id, "dev-fixture");
    assert_eq!(decoded.deliberation_statements.len(), 1);
    assert_eq!(decoded.deliberation_statements[0].created_at_hlc_logical, 5);
    assert_eq!(
        decoded.deliberation_statements[0].created_at_hlc_device_id,
        "stmt-dev"
    );

    // Pin the exact camelCase keys the frontend expects. Adding a field
    // requires updating this list; renaming a field via serde will fail
    // this assertion before it can silently break the UI.
    let expected: Vec<&str> = vec![
        "backupPool",
        "communityId",
        "declined",
        "deliberationStatements",
        "deliberationWindowSeconds",
        "draftCandidates",
        "draftingWindowSeconds",
        // ZEB-295 Phase 6 Task 9 / spec §6.6: ballot-secret status fields.
        "encryptedTallyCommitteeSize",
        "encryptedTallyShareCount",
        "encryptedTallyThreshold",
        "incentiveMode",
        "miniPublic",
        "myDeliberationStatementCount",
        "myDeliberationVotes",
        "myDraftingApprovals",
        "myRatificationScores",
        "myRole",
        "pollCreateHlcDeviceId",
        "pollCreateHlcLogical",
        "pollCreateHlcMs",
        "pollId",
        "privacyMode",
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

    // ZEB-790 (CodeRabbit #10): pin the NESTED DeliberationStatementExport
    // camelCase keys too — the top-level check never descends into the
    // `deliberationStatements` array, so a rename of the nested HLC fields
    // (createdAtHlcLogical / createdAtHlcDeviceId) would otherwise slip through
    // and silently break the TS DTO.
    let statement_keys = cbor_first_element_keys(&buf, "deliberationStatements");
    let expected_statement: Vec<&str> = vec![
        "agreeCount",
        "author",
        "createdAtHlcDeviceId",
        "createdAtHlcLogical",
        "createdAtHlcMs",
        "disagreeCount",
        "passCount",
        "statementEventHash",
        "text",
    ];
    assert_eq!(
        statement_keys,
        expected_statement
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
        "DeliberationStatementExport CBOR key drift — update TS in lockstep",
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
        // ZEB-790 (CodeRabbit #10): non-default HLC tuple (see export test).
        poll_create_hlc_logical: 7,
        poll_create_hlc_device_id: "dev-fixture".to_string(),
        sortition_size: 100,
        winner_text: None,
        privacy_mode: "pu".to_string(),
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&summary, &mut buf).expect("encode");
    let decoded: Tier3PollSummary = ciborium::from_reader(&buf[..]).expect("decode");
    assert_eq!(decoded.stage, Tier3StageTag::Ratification);
    assert_eq!(decoded.winner_text, None);
    // ZEB-790 (CodeRabbit #10): non-default HLC tuple must round-trip.
    assert_eq!(decoded.poll_create_hlc_logical, 7);
    assert_eq!(decoded.poll_create_hlc_device_id, "dev-fixture");

    let expected: Vec<&str> = vec![
        "communityId",
        "pollCreateHlcDeviceId",
        "pollCreateHlcLogical",
        "pollCreateHlcMs",
        "pollId",
        "privacyMode",
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
