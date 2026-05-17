//! ZEB-290: Byte-pinned canonical CBOR fixtures for Phase 1 voting events.
//!
//! Locks the canonical-CBOR wire encoding for the 6 Phase 1 event
//! kinds (PollCreate / PollOpen / PollExtend / PollClose / BallotCast
//! / PollResult). Any failure here is a wire-protocol break — review
//! carefully before updating the pinned bytes (cross-version compat,
//! peer interop).

use harmony_app::community_membership::ChannelId;
use harmony_app::community_voting_approval::{
    Tier1Ballot, Tier1PollConfig, Tier1PollResultPayload, Tier1Result,
};
use harmony_app::community_voting_core::{
    Eligibility, PollEventKindCode, PollId, SignedVotingEvent, Tier,
};
use harmony_app::owner_state_types::{Hlc, OwnerAddr};

const FIXTURE_POLL_ID: PollId = PollId([0xab; 32]);
const FIXTURE_ACTOR: OwnerAddr = OwnerAddr([0xaa; 16]);
const FIXTURE_CHANNEL_BYTES: [u8; 16] = [0xcc; 16];

const EXPECTED_TIER1_POLLCONFIG_HEX: &str = "a4616f836550697a7a6167427572676572736553757368696177190e1062656ca1626d700062636950cccccccccccccccccccccccccccccccc";
const EXPECTED_TIER1_BALLOT_HEX: &str = "a2627069982018ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab626170820002";
const EXPECTED_TIER1_POLLRESULT_HEX: &str = "a2627069982018ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab18ab627273a16177820002";
const EXPECTED_ENVELOPE_POLLCREATE_HEX: &str = "a862746761706276720162747268417070726f76616c626b64626372626863a361771903e8616c006164616462616350aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa627064983918a41861186f1883186518501869187a187a1861186718421875187218671865187218731865185318751873186818691861187718190e1018621865186c18a11862186d187000186218631869185018cc18cc18cc18cc18cc18cc18cc18cc18cc18cc18cc18cc18cc18cc18cc18cc627367984000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
const EXPECTED_ENVELOPE_BALLOTCAST_HEX: &str = "a862746761706276720162747268417070726f76616c626b6462626c626863a361771903e8616c006164616462616350aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa627064984c18a218621870186918981820181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab18621861187018820002627367984000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
const EXPECTED_ENVELOPE_POLLCLOSE_HEX: &str = "a862746761706276720162747268417070726f76616c626b6462636c626863a361771903e8616c006164616462616350aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa627064984618a118621870186918981820181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab181818ab627367984000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";

fn fixture_hlc() -> Hlc {
    Hlc {
        wall_ms: 1_000,
        logical: 0,
        device_id: "d".into(),
    }
}

fn fixture_config() -> Tier1PollConfig {
    Tier1PollConfig {
        options: vec!["Pizza".into(), "Burgers".into(), "Sushi".into()],
        window_seconds: 3600,
        quorum: None,
        threshold_percent: None,
        multi_winner: None,
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        },
        channel_id: ChannelId(FIXTURE_CHANNEL_BYTES),
    }
}

fn encode<T: serde::Serialize>(v: &T) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::into_writer(v, &mut out).expect("encode");
    out
}

fn encode_envelope(kind: PollEventKindCode, payload: Vec<u8>) -> Vec<u8> {
    let ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Approval,
        kind,
        hlc: fixture_hlc(),
        actor: FIXTURE_ACTOR,
        payload,
        sig: vec![0u8; 64],
    };
    encode(&ev)
}

#[test]
fn tier1_pollconfig_canonical_cbor() {
    let actual_hex = hex::encode(encode(&fixture_config()));
    if EXPECTED_TIER1_POLLCONFIG_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_TIER1_POLLCONFIG_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_TIER1_POLLCONFIG_HEX,
        "Tier1PollConfig wire format changed"
    );
}

#[test]
fn tier1_ballot_canonical_cbor() {
    let ballot = Tier1Ballot {
        poll_id: FIXTURE_POLL_ID,
        approved_indices: vec![0, 2],
    };
    let actual_hex = hex::encode(encode(&ballot));
    if EXPECTED_TIER1_BALLOT_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_TIER1_BALLOT_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_TIER1_BALLOT_HEX,
        "Tier1Ballot wire format changed"
    );
}

#[test]
fn tier1_pollresult_canonical_cbor() {
    let r = Tier1PollResultPayload {
        poll_id: FIXTURE_POLL_ID,
        result: Tier1Result::Winners(vec![0, 2]),
    };
    let actual_hex = hex::encode(encode(&r));
    if EXPECTED_TIER1_POLLRESULT_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_TIER1_POLLRESULT_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_TIER1_POLLRESULT_HEX,
        "Tier1PollResult wire format changed"
    );
}

#[test]
fn envelope_pollcreate_canonical_cbor() {
    let payload = encode(&fixture_config());
    let encoded = encode_envelope(PollEventKindCode::PollCreate, payload);
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ENVELOPE_POLLCREATE_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_ENVELOPE_POLLCREATE_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_ENVELOPE_POLLCREATE_HEX,
        "PollCreate envelope wire format changed"
    );

    // Structural assertions: 8 top-level keys, all 2-char.
    let value: ciborium::Value = ciborium::from_reader(&encoded[..]).expect("decode");
    let map = value.as_map().expect("top-level is a CBOR map");
    assert_eq!(map.len(), 8);
    for (k, _) in map.iter() {
        assert_eq!(k.as_text().unwrap().len(), 2);
    }
}

#[test]
fn envelope_ballotcast_canonical_cbor() {
    let ballot = Tier1Ballot {
        poll_id: FIXTURE_POLL_ID,
        approved_indices: vec![0, 2],
    };
    let payload = encode(&ballot);
    let encoded = encode_envelope(PollEventKindCode::BallotCast, payload);
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ENVELOPE_BALLOTCAST_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_ENVELOPE_BALLOTCAST_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_ENVELOPE_BALLOTCAST_HEX,
        "BallotCast envelope wire format changed"
    );
}

#[test]
fn envelope_pollclose_canonical_cbor() {
    #[derive(serde::Serialize)]
    struct CloseRef {
        #[serde(rename = "pi")]
        pi: PollId,
    }
    let payload = encode(&CloseRef {
        pi: FIXTURE_POLL_ID,
    });
    let encoded = encode_envelope(PollEventKindCode::PollClose, payload);
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ENVELOPE_POLLCLOSE_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_ENVELOPE_POLLCLOSE_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_ENVELOPE_POLLCLOSE_HEX,
        "PollClose envelope wire format changed"
    );
}
