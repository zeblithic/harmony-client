//! ZEB-311: pin CBOR wire-format encoding of Tier3PollExport + Tier3PollSummary.
//!
//! These types are camelCased serde structs that flow through Tauri IPC to
//! the frontend. The CBOR round-trip is what guarantees JS-side field names
//! match the spec. Any field rename or default change must be deliberate and
//! reflected in this fixture.

use harmony_app::{Tier3MyRole, Tier3PollExport, Tier3PollSummary, Tier3StageTag};

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
