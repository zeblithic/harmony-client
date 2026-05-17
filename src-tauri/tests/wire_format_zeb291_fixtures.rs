//! ZEB-291 Phase 2: Byte-pinned canonical CBOR fixtures for Tier 2 voting events.
//!
//! Locks the canonical-CBOR wire encoding for the 4 Tier 2 envelopes
//! (Signal / Delegate / Undelegate / Tier 2 PollCreate) plus the two
//! inner payload shapes that Phase 1's fixture file doesn't already
//! cover (Tier2PollConfig + SignalPayload). Any failure here is a
//! wire-protocol break — review carefully before updating the pinned
//! bytes (cross-version compat, peer interop).
//!
//! Pattern mirrors `tests/wire_format_zeb290_fixtures.rs`: deterministic
//! signing-key-irrelevant envelopes (`sig` is a 64-byte zero placeholder,
//! matching Phase 1; signature semantics are exercised in the
//! envelope-signing tests in `community_voting_core.rs`'s own test
//! module, not in this wire-format pinning file), ciborium for canonical
//! encoding, hex constants pin the bytes so any future serde-rename or
//! wire-format change fails this test before it can land.
//!
//! ## Signal payload key — temporary `"sp"` divergence from spec §5
//!
//! Spec §5 (commit 2026-05-17, before this fixture file landed) shows
//! the Signal payload as `{ "pr": <proposal_id>, "s": true|false }`.
//! That violates the §3 same-length-keys invariant (`"pr"` is 2 chars,
//! `"s"` is 1 char) — the very invariant this file exists to pin. We
//! rename `"s"` → `"sp"` (for "support") here so the inner payload map
//! has uniform 2-char keys. Follow-up Linear ticket is to amend the spec
//! to match. Task 9 will canonicalize this payload type in
//! `community_voting_conviction.rs` and the wire-format pinning here
//! protects that future move against silent drift.
//!
//! Delegate uses `{"to", "sc"}` (both 2 chars — spec is self-consistent).
//! Undelegate is `{}` (no keys, invariant vacuously satisfied).

use harmony_app::community_voting_conviction::{AutoExecAction, Tier2PollConfig};
use harmony_app::community_voting_core::{
    Eligibility, PollEventKindCode, PollId, SignedVotingEvent, Tier,
};
use harmony_app::owner_state_types::{Hlc, OwnerAddr};

const FIXTURE_POLL_ID: PollId = PollId([0xab; 32]);
const FIXTURE_ACTOR: OwnerAddr = OwnerAddr([0xaa; 16]);
const FIXTURE_DELEGATE_TARGET: [u8; 32] = [0x77; 32];

// Q96.32 fixed-point constant from the Tier 2 PollConfig amendment.
const Q32: i128 = 1 << 32;

const EXPECTED_TIER2_POLLCONFIG_HEX: &str = "a8627074782650726f6d6f74652040616c69636520746f206d6f64657261746f722028706f7765722035302962686c1a00093a8062746e1b0000000a000000006274781b000003e8000000006262620262646cf5626178a3626b6b6273706274675042424242424242424242424242424242626e700562656ca1626d7001";
const EXPECTED_SIGNAL_PAYLOAD_HEX: &str =
    "a26270725820abababababababababababababababababababababababababababababababab627370f5";
const EXPECTED_ENVELOPE_SIGNAL_HEX: &str = "a862746761706276720162747202626b64627367626863a361771903e8616c006164616462616350aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa627064582aa26270725820abababababababababababababababababababababababababababababababab627370f5627367584000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
const EXPECTED_ENVELOPE_DELEGATE_HEX: &str = "a862746761706276720162747202626b64626467626863a361771903e8616c006164616462616350aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa627064582da262746f5820777777777777777777777777777777777777777777777777777777777777777762736363616c6c627367584000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
const EXPECTED_ENVELOPE_UNDELEGATE_HEX: &str = "a862746761706276720162747202626b64627564626863a361771903e8616c006164616462616350aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa62706441a0627367584000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
const EXPECTED_ENVELOPE_TIER2_POLLCREATE_HEX: &str = "a862746761706276720162747202626b64626372626863a361771903e8616c006164616462616350aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa627064587ea8627074782650726f6d6f74652040616c69636520746f206d6f64657261746f722028706f7765722035302962686c1a00093a8062746e1b0000000a000000006274781b000003e8000000006262620262646cf5626178a3626b6b6273706274675042424242424242424242424242424242626e700562656ca1626d7001627367584000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";

// ---------------------------------------------------------------------------
// Inline payload structs for Signal / Delegate / Undelegate.
//
// These are intentionally local to this fixture file: Task 9 will move
// the canonical versions into `community_voting_conviction.rs`. Pinning
// the wire format here first means Task 9's move is forced to preserve
// it byte-for-byte (or this test fails and surfaces the drift).
// ---------------------------------------------------------------------------

/// Signal payload (`pd` for `kd="sg"`, `tr=2`).
///
/// Uses `"sp"` for the support flag instead of the spec's `"s"` so the
/// payload satisfies §3's same-length-keys invariant — see file-level
/// note above.
#[derive(serde::Serialize, serde::Deserialize)]
struct SignalPayload {
    #[serde(rename = "pr")]
    proposal_id: PollId,
    #[serde(rename = "sp")]
    support: bool,
}

/// Delegate payload (`pd` for `kd="dg"`, `tr=2`).
///
/// `"to"` is the 32-byte ed25519 pubkey of the delegate (NOT the
/// 16-byte OwnerAddr; spec §5 explicitly says `<ed25519_pubkey>`).
/// `serde_bytes` is used so the field encodes as a CBOR byte string
/// (major type 2) instead of an array-of-u8 (major type 4) — same
/// rationale as Phase 1's `SignedVotingEvent.payload`/`.sig` fields.
#[derive(serde::Serialize, serde::Deserialize)]
struct DelegatePayload {
    #[serde(rename = "to", with = "serde_bytes")]
    to: Vec<u8>,
    #[serde(rename = "sc")]
    scope: String,
}

/// Undelegate payload (`pd` for `kd="ud"`, `tr=2`). Empty map per spec §5.
#[derive(serde::Serialize, serde::Deserialize)]
struct UndelegatePayload {}

// ---------------------------------------------------------------------------
// Helpers (mirror wire_format_zeb290_fixtures.rs).
// ---------------------------------------------------------------------------

fn fixture_hlc() -> Hlc {
    Hlc {
        wall_ms: 1_000,
        logical: 0,
        device_id: "d".into(),
    }
}

fn fixture_tier2_config() -> Tier2PollConfig {
    Tier2PollConfig {
        proposal_text: "Promote @alice to moderator (power 50)".into(),
        half_life_seconds: 604_800,
        threshold_min_q32: 10 * Q32,
        threshold_max_q32: 1_000 * Q32,
        beta: 2,
        delegation_allowed: true,
        auto_exec: AutoExecAction::SetPower {
            target_pubkey: OwnerAddr([0x42; 16]),
            new_power: 5,
        },
        eligibility: Eligibility {
            min_power: 1,
            min_vouching_depth: None,
            sortition_size: None,
        },
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
        tier: Tier::Conviction,
        kind,
        hlc: fixture_hlc(),
        actor: FIXTURE_ACTOR,
        payload,
        sig: vec![0u8; 64],
    };
    encode(&ev)
}

// ---------------------------------------------------------------------------
// Inner-payload-level fixtures.
// ---------------------------------------------------------------------------

#[test]
fn wire_format_zeb291_tier2_pollconfig_payload_fixture_pinned() {
    let actual_hex = hex::encode(encode(&fixture_tier2_config()));
    if EXPECTED_TIER2_POLLCONFIG_HEX.is_empty() {
        panic!("REGENERATE EXPECTED_TIER2_POLLCONFIG_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_TIER2_POLLCONFIG_HEX,
        "Tier2PollConfig wire format changed"
    );
}

#[test]
fn wire_format_zeb291_signal_payload_fixture_pinned() {
    let payload = SignalPayload {
        proposal_id: FIXTURE_POLL_ID,
        support: true,
    };
    let actual_hex = hex::encode(encode(&payload));
    if EXPECTED_SIGNAL_PAYLOAD_HEX.is_empty() {
        panic!("REGENERATE EXPECTED_SIGNAL_PAYLOAD_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_SIGNAL_PAYLOAD_HEX,
        "SignalPayload wire format changed"
    );
}

// ---------------------------------------------------------------------------
// Envelope-level fixtures.
// ---------------------------------------------------------------------------

#[test]
fn wire_format_zeb291_signal_envelope_fixture_pinned() {
    let payload = encode(&SignalPayload {
        proposal_id: FIXTURE_POLL_ID,
        support: true,
    });
    let encoded = encode_envelope(PollEventKindCode::Signal, payload);
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ENVELOPE_SIGNAL_HEX.is_empty() {
        panic!("REGENERATE EXPECTED_ENVELOPE_SIGNAL_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_ENVELOPE_SIGNAL_HEX,
        "Signal envelope wire format changed"
    );

    // Structural assertions: 8 top-level keys, all 2-char (mirrors Phase 1).
    let value: ciborium::Value = ciborium::from_reader(&encoded[..]).expect("decode");
    let map = value.as_map().expect("top-level is a CBOR map");
    assert_eq!(map.len(), 8);
    for (k, _) in map.iter() {
        assert_eq!(k.as_text().unwrap().len(), 2);
    }
}

#[test]
fn wire_format_zeb291_delegate_envelope_fixture_pinned() {
    let payload = encode(&DelegatePayload {
        to: FIXTURE_DELEGATE_TARGET.to_vec(),
        scope: "all".into(),
    });
    let encoded = encode_envelope(PollEventKindCode::Delegate, payload);
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ENVELOPE_DELEGATE_HEX.is_empty() {
        panic!("REGENERATE EXPECTED_ENVELOPE_DELEGATE_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_ENVELOPE_DELEGATE_HEX,
        "Delegate envelope wire format changed"
    );
}

#[test]
fn wire_format_zeb291_undelegate_envelope_fixture_pinned() {
    let payload = encode(&UndelegatePayload {});
    let encoded = encode_envelope(PollEventKindCode::Undelegate, payload);
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ENVELOPE_UNDELEGATE_HEX.is_empty() {
        panic!("REGENERATE EXPECTED_ENVELOPE_UNDELEGATE_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_ENVELOPE_UNDELEGATE_HEX,
        "Undelegate envelope wire format changed"
    );
}

#[test]
fn wire_format_zeb291_tier2_pollcreate_envelope_fixture_pinned() {
    let payload = encode(&fixture_tier2_config());
    let encoded = encode_envelope(PollEventKindCode::PollCreate, payload);
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ENVELOPE_TIER2_POLLCREATE_HEX.is_empty() {
        panic!("REGENERATE EXPECTED_ENVELOPE_TIER2_POLLCREATE_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_ENVELOPE_TIER2_POLLCREATE_HEX,
        "Tier 2 PollCreate envelope wire format changed"
    );

    // The Tier 2 envelope should also satisfy the §3 8-keys / 2-char
    // invariant. (PollCreate-on-Tier-1 already pins this in ZEB-290;
    // duplicated here so Tier 2's envelope shape can't drift independently.)
    let value: ciborium::Value = ciborium::from_reader(&encoded[..]).expect("decode");
    let map = value.as_map().expect("top-level is a CBOR map");
    assert_eq!(map.len(), 8);
    for (k, _) in map.iter() {
        assert_eq!(k.as_text().unwrap().len(), 2);
    }
}
