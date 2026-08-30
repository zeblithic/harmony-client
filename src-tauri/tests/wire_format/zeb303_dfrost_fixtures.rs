//! ZEB-303: Byte-pinned canonical CBOR fixtures for D-FROST committee events.
//!
//! Locks the canonical-CBOR wire encoding for the 5 D-FROST committee
//! event kinds (DkgRound — round 1 + round 2 / DkgComplete /
//! ThresholdSign / VrfBeacon / ProactiveRefresh — round 1 + round 2).
//!
//! Pin the BYTE LAYOUT only — these fixtures use synthetic non-crypto
//! blobs (e.g. `vec![0xee; 32]` for `round1_package`) and a synthetic
//! all-zero 64-byte envelope `sig`. We do NOT verify FROST / Ed25519
//! validity here; that lives in the engine tests. Any failure here is a
//! wire-protocol break — review carefully before updating the pinned
//! bytes (cross-version compat, peer interop).
//!
//! Pattern matches `wire_format/zeb290_fixtures.rs` (the Phase 1 voting
//! envelope fixture set).

use harmony_app::community_dfrost_types::{
    CeremonyInitPayload, DfrostEventKind, DkgCompletePayload, DkgRoundPayload,
    MemberVerifyingShare, RefreshRoundPayload, RepairRoundPayload, SignedCommitteeEvent,
    ThresholdSignPayload, VrfBeaconPayload,
};
use harmony_app::community_membership::RecipientCiphertext;
use harmony_app::owner_state_types::{Hlc, OwnerAddr};

// ---------------------------------------------------------------------------
// Deterministic fixture inputs
// ---------------------------------------------------------------------------

const FIXTURE_ACTOR: OwnerAddr = OwnerAddr([0xaa; 16]);
const FIXTURE_CEREMONY_ID: [u8; 32] = [0x66; 32];
const FIXTURE_MESSAGE_HASH: [u8; 32] = [0x55; 32];
const FIXTURE_JOINT_VK: [u8; 32] = [0x44; 32];
const FIXTURE_VERIFYING_SHARE: [u8; 32] = [0x33; 32];
const FIXTURE_VRF_OUTPUT: [u8; 32] = [0x22; 32];
const FIXTURE_RECIPIENT: OwnerAddr = OwnerAddr([0xbb; 16]);

fn fixture_hlc() -> Hlc {
    Hlc {
        wall_ms: 1_000,
        logical: 0,
        device_id: "d".into(),
    }
}

fn fixture_recipient_ciphertext() -> RecipientCiphertext {
    RecipientCiphertext {
        recipient: FIXTURE_RECIPIENT,
        // Synthetic sealed-package bytes — NOT a real X25519 sealed blob;
        // this fixture pins byte layout, not crypto validity.
        sealed: vec![0xdd; 16],
    }
}

fn encode<T: serde::Serialize>(v: &T) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::into_writer(v, &mut out).expect("encode");
    out
}

fn encode_envelope(kind: DfrostEventKind, payload: Vec<u8>) -> Vec<u8> {
    let ev = SignedCommitteeEvent {
        tag: 'd',
        version: 1,
        committee_tier: 0,
        kind,
        hlc: fixture_hlc(),
        actor: FIXTURE_ACTOR,
        payload,
        sig: vec![0u8; 64],
    };
    encode(&ev)
}

/// Structural assertion shared by every envelope test: confirms the
/// 8-key / 2-char-key invariant + `tg="d"` + the expected `kd` code.
fn assert_envelope_structure(encoded: &[u8], expected_kd: &str) {
    let value: ciborium::Value = ciborium::from_reader(encoded).expect("decode envelope");
    let map = value.as_map().expect("top-level is a CBOR map");
    assert_eq!(map.len(), 8, "envelope must have exactly 8 top-level keys");

    let mut keys: Vec<&str> = Vec::with_capacity(8);
    for (k, _) in map.iter() {
        let s = k.as_text().expect("envelope key is text");
        assert_eq!(s.len(), 2, "envelope key {s:?} violates 2-char invariant");
        keys.push(s);
    }
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    let mut expected = ["tg", "vr", "tr", "kd", "hc", "ac", "pd", "sg"];
    expected.sort_unstable();
    assert_eq!(
        sorted, expected,
        "envelope key set must be exactly {{tg, vr, tr, kd, hc, ac, pd, sg}}"
    );

    let get = |needle: &str| -> &ciborium::Value {
        map.iter()
            .find(|(k, _)| k.as_text() == Some(needle))
            .map(|(_, v)| v)
            .unwrap_or_else(|| panic!("missing key {needle}"))
    };
    assert_eq!(
        get("tg").as_text(),
        Some("d"),
        "envelope tg must encode as text \"d\""
    );
    assert_eq!(
        get("kd").as_text(),
        Some(expected_kd),
        "envelope kd must encode as text {expected_kd:?}"
    );
}

// ---------------------------------------------------------------------------
// Pinned hex constants
// ---------------------------------------------------------------------------

const EXPECTED_DI_HEX: &str = "a762636958206666666666666666666666666666666666666666666666666666666666666666626d628250aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa50bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb62746802626d78026265700162776d1903e8626c6700";
const EXPECTED_DR_ROUND1_HEX: &str = "a36263695820666666666666666666666666666666666666666666666666666666666666666662726e0162706b5820eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
const EXPECTED_DR_ROUND2_HEX: &str = "a36263695820666666666666666666666666666666666666666666666666666666666666666662726e0262726381a262726350bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb62637450dddddddddddddddddddddddddddddddd";
const EXPECTED_DK_HEX: &str = "a76263695820666666666666666666666666666666666666666666666666666666666666666662766b5820444444444444444444444444444444444444444444444444444444444444444462767381a262696450aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa62766b5820333333333333333333333333333333333333333333333333333333333333333362657001626d628150aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa62746801626d7801";
const EXPECTED_TS_HEX: &str = "a462636958206666666666666666666666666666666666666666666666666666666666666666626d735820555555555555555555555555555555555555555555555555555555555555555562636d5820cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc6273685820bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const EXPECTED_VB_HEX: &str = "a462636958206666666666666666666666666666666666666666666666666666666666666666626d735820555555555555555555555555555555555555555555555555555555555555555562736758409999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999962766658202222222222222222222222222222222222222222222222222222222222222222";
const EXPECTED_RF_ROUND1_HEX: &str = "a36263695820666666666666666666666666666666666666666666666666666666666666666662726e0162706b5820eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
// ZEB-1028: rn=1 with a non-zero retry attempt gains the `at` key (a
// deadline-retry proposal). Attempt 0 omits the key — the unchanged
// EXPECTED_RF_ROUND1_HEX above IS the wire-stability pin for that.
/// ZEB-1034: `dk` with the community binding (`sp`) present — the
/// 8-key form new mints produce. The legacy 7-key pin (`EXPECTED_DK_HEX`)
/// is deliberately UNCHANGED: `space_id: None` must stay byte-identical
/// to pre-1034 events.
const EXPECTED_DK_BOUND_HEX: &str = "a86263695820666666666666666666666666666666666666666666666666666666666666666662766b5820444444444444444444444444444444444444444444444444444444444444444462767381a262696450aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa62766b5820333333333333333333333333333333333333333333333333333333333333333362657001626d628150aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa62746801626d780162737050dddddddddddddddddddddddddddddddd";

const EXPECTED_RF_ROUND1_RETRY_HEX: &str ="a46263695820666666666666666666666666666666666666666666666666666666666666666662726e0162706b5820eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee62617402";
const EXPECTED_RF_ROUND2_HEX: &str = "a36263695820666666666666666666666666666666666666666666666666666666666666666662726e0262726381a262726350bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb62637450dddddddddddddddddddddddddddddddd";

const EXPECTED_ENVELOPE_DI_HEX: &str = "a862746761646276720162747200626b64626469626863a361771903e8616c006164616462616350aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa6270645862a762636958206666666666666666666666666666666666666666666666666666666666666666626d628250aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa50bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb62746802626d78026265700162776d1903e8626c6700627367584000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
const EXPECTED_ENVELOPE_DR_ROUND1_HEX: &str = "a862746761646276720162747200626b64626472626863a361771903e8616c006164616462616350aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa627064584fa36263695820666666666666666666666666666666666666666666666666666666666666666662726e0162706b5820eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee627367584000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
const EXPECTED_ENVELOPE_DR_ROUND2_HEX: &str = "a862746761646276720162747200626b64626472626863a361771903e8616c006164616462616350aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa6270645857a36263695820666666666666666666666666666666666666666666666666666666666666666662726e0262726381a262726350bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb62637450dddddddddddddddddddddddddddddddd627367584000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
const EXPECTED_ENVELOPE_DK_HEX: &str = "a862746761646276720162747200626b6462646b626863a361771903e8616c006164616462616350aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa62706458aaa76263695820666666666666666666666666666666666666666666666666666666666666666662766b5820444444444444444444444444444444444444444444444444444444444444444462767381a262696450aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa62766b5820333333333333333333333333333333333333333333333333333333333333333362657001626d628150aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa62746801626d7801627367584000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
const EXPECTED_ENVELOPE_TS_HEX: &str = "a862746761646276720162747200626b64627473626863a361771903e8616c006164616462616350aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa6270645895a462636958206666666666666666666666666666666666666666666666666666666666666666626d735820555555555555555555555555555555555555555555555555555555555555555562636d5820cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc6273685820bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb627367584000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
const EXPECTED_ENVELOPE_VB_HEX: &str = "a862746761646276720162747200626b64627662626863a361771903e8616c006164616462616350aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa62706458b5a462636958206666666666666666666666666666666666666666666666666666666666666666626d735820555555555555555555555555555555555555555555555555555555555555555562736758409999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999962766658202222222222222222222222222222222222222222222222222222222222222222627367584000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
const EXPECTED_ENVELOPE_RF_ROUND1_HEX: &str = "a862746761646276720162747200626b64627266626863a361771903e8616c006164616462616350aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa627064584fa36263695820666666666666666666666666666666666666666666666666666666666666666662726e0162706b5820eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee627367584000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
const EXPECTED_ENVELOPE_RF_ROUND2_HEX: &str = "a862746761646276720162747200626b64627266626863a361771903e8616c006164616462616350aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa6270645857a36263695820666666666666666666666666666666666666666666666666666666666666666662726e0262726381a262726350bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb62637450dddddddddddddddddddddddddddddddd627367584000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
const EXPECTED_RP_ROUND1_HEX: &str = "a66263695820666666666666666666666666666666666666666666666666666666666666666662726e016265700162686c8150bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb62776d1903e8626c6700";
const EXPECTED_RP_ROUND2_HEX: &str = "a46263695820666666666666666666666666666666666666666666666666666666666666666662726e026265700162726381a262726350bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb62637450dddddddddddddddddddddddddddddddd";
const EXPECTED_ENVELOPE_RP_ROUND1_HEX: &str = "a862746761646276720162747200626b64627270626863a361771903e8616c006164616462616350aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa627064584da66263695820666666666666666666666666666666666666666666666666666666666666666662726e016265700162686c8150bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb62776d1903e8626c6700627367584000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
const EXPECTED_ENVELOPE_RP_ROUND2_HEX: &str = "a862746761646276720162747200626b64627270626863a361771903e8616c006164616462616350aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa627064585ba46263695820666666666666666666666666666666666666666666666666666666666666666662726e026265700162726381a262726350bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb62637450dddddddddddddddddddddddddddddddd627367584000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";

// ---------------------------------------------------------------------------
// Payload fixture builders
// ---------------------------------------------------------------------------

/// ZEB-1022: CeremonyInit (`di`) — the ceremony-bootstrap event that
/// carries the committee shape + payload-carried mint stamp.
/// FIXTURE_ACTOR (0xaa) < FIXTURE_RECIPIENT (0xbb): sorted, as
/// `apply_ceremony_init` requires.
fn di_payload() -> CeremonyInitPayload {
    CeremonyInitPayload {
        ceremony_id: FIXTURE_CEREMONY_ID,
        members: vec![FIXTURE_ACTOR, FIXTURE_RECIPIENT],
        threshold: 2,
        max_signers: 2,
        epoch: 1,
        minted_wall_ms: 1_000,
        minted_logical: 0,
    }
}

fn dr_round1_payload() -> DkgRoundPayload {
    DkgRoundPayload {
        ceremony_id: FIXTURE_CEREMONY_ID,
        round_num: 1,
        // Synthetic round1_package bytes — NOT a real frost::dkg::round1::Package.
        round1_package: Some(vec![0xee; 32]),
        recipient_ciphertexts: None,
    }
}

fn dr_round2_payload() -> DkgRoundPayload {
    DkgRoundPayload {
        ceremony_id: FIXTURE_CEREMONY_ID,
        round_num: 2,
        round1_package: None,
        recipient_ciphertexts: Some(vec![fixture_recipient_ciphertext()]),
    }
}

fn dk_payload() -> DkgCompletePayload {
    DkgCompletePayload {
        ceremony_id: FIXTURE_CEREMONY_ID,
        joint_verifying_key: FIXTURE_JOINT_VK,
        verifying_shares: vec![MemberVerifyingShare {
            member: FIXTURE_ACTOR,
            verifying_share: FIXTURE_VERIFYING_SHARE,
        }],
        epoch: 1,
        members: vec![FIXTURE_ACTOR],
        threshold: 1,
        max_signers: 1,
        // ZEB-1034: `None` keeps the legacy 7-key form — the pre-1034
        // pin above this field's introduction MUST stay byte-identical
        // (`skip_serializing_if` omits the `sp` key entirely).
        space_id: None,
    }
}

/// ZEB-1034: the community-bound form every post-upgrade mint produces —
/// same fixture with `sp` populated (appended 8th key, `bstr(16)`).
fn dk_bound_payload() -> DkgCompletePayload {
    DkgCompletePayload {
        space_id: Some(harmony_app::owner_state_types::SpaceId([0xdd; 16])),
        ..dk_payload()
    }
}

fn ts_payload() -> ThresholdSignPayload {
    ThresholdSignPayload {
        ceremony_id: FIXTURE_CEREMONY_ID,
        message_hash: FIXTURE_MESSAGE_HASH,
        // Synthetic commitment / share bytes — NOT real FROST blobs.
        commitment_bytes: vec![0xcc; 32],
        share_bytes: vec![0xbb; 32],
    }
}

fn vb_payload() -> VrfBeaconPayload {
    VrfBeaconPayload {
        ceremony_id: FIXTURE_CEREMONY_ID,
        message_hash: FIXTURE_MESSAGE_HASH,
        // Synthetic 64-byte Schnorr signature `R(32) || s(32)`.
        signature: vec![0x99; 64],
        vrf_output: FIXTURE_VRF_OUTPUT,
    }
}

// ZEB-1027: the refresh rounds carry the DKG-mirroring shapes — rn=1
// is the PUBLIC zero-sharing commitment (`pk`), rn=2 the sealed
// per-recipient packages (`rc`). Pre-1027 the two were inverted
// placeholders; the STRUCT is unchanged, so these pins cover the same
// field combinations as before with the round numbers swapped.
fn rf_round1_payload() -> RefreshRoundPayload {
    RefreshRoundPayload {
        ceremony_id: FIXTURE_CEREMONY_ID,
        round_num: 1,
        recipient_ciphertexts: None,
        package: Some(vec![0xee; 32]),
        attempt: 0,
    }
}

/// ZEB-1028: a deadline-retry proposal — attempt 2 of this epoch's
/// refresh (carries the `at` key; derives a distinct ceremony id).
fn rf_round1_retry_payload() -> RefreshRoundPayload {
    RefreshRoundPayload {
        attempt: 2,
        ..rf_round1_payload()
    }
}

fn rf_round2_payload() -> RefreshRoundPayload {
    RefreshRoundPayload {
        ceremony_id: FIXTURE_CEREMONY_ID,
        round_num: 2,
        recipient_ciphertexts: Some(vec![fixture_recipient_ciphertext()]),
        package: None,
        attempt: 0,
    }
}

// ZEB-1027: RTS share-repair rounds (`rp`).
fn rp_round1_payload() -> RepairRoundPayload {
    RepairRoundPayload {
        ceremony_id: FIXTURE_CEREMONY_ID,
        round_num: 1,
        epoch: 1,
        helpers: Some(vec![FIXTURE_RECIPIENT]),
        minted_wall_ms: Some(1_000),
        minted_logical: Some(0),
        recipient_ciphertexts: None,
    }
}

fn rp_round2_payload() -> RepairRoundPayload {
    RepairRoundPayload {
        ceremony_id: FIXTURE_CEREMONY_ID,
        round_num: 2,
        epoch: 1,
        helpers: None,
        minted_wall_ms: None,
        minted_logical: None,
        recipient_ciphertexts: Some(vec![fixture_recipient_ciphertext()]),
    }
}

// ---------------------------------------------------------------------------
// Payload byte-pinning tests
// ---------------------------------------------------------------------------

#[test]
fn di_canonical_cbor() {
    let actual_hex = hex::encode(encode(&di_payload()));
    if EXPECTED_DI_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_DI_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_DI_HEX,
        "CeremonyInitPayload wire format changed"
    );
}

#[test]
fn dr_round1_canonical_cbor() {
    let actual_hex = hex::encode(encode(&dr_round1_payload()));
    if EXPECTED_DR_ROUND1_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_DR_ROUND1_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_DR_ROUND1_HEX,
        "DkgRoundPayload (rn=1) wire format changed"
    );
}

#[test]
fn dr_round2_canonical_cbor() {
    let actual_hex = hex::encode(encode(&dr_round2_payload()));
    if EXPECTED_DR_ROUND2_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_DR_ROUND2_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_DR_ROUND2_HEX,
        "DkgRoundPayload (rn=2) wire format changed"
    );
}

#[test]
fn dk_canonical_cbor() {
    let actual_hex = hex::encode(encode(&dk_payload()));
    if EXPECTED_DK_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_DK_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_DK_HEX,
        "DkgCompletePayload wire format changed"
    );
}

#[test]
fn dk_bound_canonical_cbor_zeb1034() {
    let actual_hex = hex::encode(encode(&dk_bound_payload()));
    if EXPECTED_DK_BOUND_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_DK_BOUND_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_DK_BOUND_HEX,
        "community-bound DkgCompletePayload wire format changed"
    );
}

/// ZEB-1034 roundtrips: the bound form decodes back to `Some(space)`;
/// legacy 7-key bytes decode with `space_id: None` (the `default`).
#[test]
fn dk_space_binding_roundtrip_zeb1034() {
    let bound = dk_bound_payload();
    let decoded: DkgCompletePayload =
        ciborium::de::from_reader(&encode(&bound)[..]).expect("bound dk decodes");
    assert_eq!(decoded, bound);

    let legacy: DkgCompletePayload =
        ciborium::de::from_reader(&encode(&dk_payload())[..]).expect("legacy dk decodes");
    assert_eq!(legacy.space_id, None);
}

#[test]
fn ts_canonical_cbor() {
    let actual_hex = hex::encode(encode(&ts_payload()));
    if EXPECTED_TS_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_TS_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_TS_HEX,
        "ThresholdSignPayload wire format changed"
    );
}

#[test]
fn vb_canonical_cbor() {
    let actual_hex = hex::encode(encode(&vb_payload()));
    if EXPECTED_VB_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_VB_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_VB_HEX,
        "VrfBeaconPayload wire format changed"
    );
}

#[test]
fn rf_round1_canonical_cbor() {
    let actual_hex = hex::encode(encode(&rf_round1_payload()));
    if EXPECTED_RF_ROUND1_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_RF_ROUND1_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_RF_ROUND1_HEX,
        "RefreshRoundPayload (rn=1) wire format changed"
    );
}

#[test]
fn rf_round1_retry_canonical_cbor() {
    let actual_hex = hex::encode(encode(&rf_round1_retry_payload()));
    if EXPECTED_RF_ROUND1_RETRY_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_RF_ROUND1_RETRY_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_RF_ROUND1_RETRY_HEX,
        "RefreshRoundPayload (rn=1, attempt > 0) wire format changed"
    );
}

#[test]
fn rf_round2_canonical_cbor() {
    let actual_hex = hex::encode(encode(&rf_round2_payload()));
    if EXPECTED_RF_ROUND2_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_RF_ROUND2_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_RF_ROUND2_HEX,
        "RefreshRoundPayload (rn=2) wire format changed"
    );
}

#[test]
fn rp_round1_canonical_cbor() {
    let actual_hex = hex::encode(encode(&rp_round1_payload()));
    if EXPECTED_RP_ROUND1_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_RP_ROUND1_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_RP_ROUND1_HEX,
        "RepairRoundPayload (rn=1) wire format changed"
    );
}

#[test]
fn rp_round2_canonical_cbor() {
    let actual_hex = hex::encode(encode(&rp_round2_payload()));
    if EXPECTED_RP_ROUND2_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_RP_ROUND2_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_RP_ROUND2_HEX,
        "RepairRoundPayload (rn=2) wire format changed"
    );
}

// ---------------------------------------------------------------------------
// Envelope byte-pinning tests + structural assertions
// ---------------------------------------------------------------------------

#[test]
fn envelope_di_canonical_cbor() {
    let payload = encode(&di_payload());
    let encoded = encode_envelope(DfrostEventKind::CeremonyInit, payload);
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ENVELOPE_DI_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_ENVELOPE_DI_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_ENVELOPE_DI_HEX,
        "CeremonyInit envelope wire format changed"
    );
    assert_envelope_structure(&encoded, "di");
}

#[test]
fn envelope_dr_round1_canonical_cbor() {
    let payload = encode(&dr_round1_payload());
    let encoded = encode_envelope(DfrostEventKind::DkgRound, payload);
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ENVELOPE_DR_ROUND1_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_ENVELOPE_DR_ROUND1_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_ENVELOPE_DR_ROUND1_HEX,
        "DkgRound (rn=1) envelope wire format changed"
    );
    assert_envelope_structure(&encoded, "dr");
}

#[test]
fn envelope_dr_round2_canonical_cbor() {
    let payload = encode(&dr_round2_payload());
    let encoded = encode_envelope(DfrostEventKind::DkgRound, payload);
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ENVELOPE_DR_ROUND2_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_ENVELOPE_DR_ROUND2_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_ENVELOPE_DR_ROUND2_HEX,
        "DkgRound (rn=2) envelope wire format changed"
    );
    assert_envelope_structure(&encoded, "dr");
}

#[test]
fn envelope_dk_canonical_cbor() {
    let payload = encode(&dk_payload());
    let encoded = encode_envelope(DfrostEventKind::DkgComplete, payload);
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ENVELOPE_DK_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_ENVELOPE_DK_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_ENVELOPE_DK_HEX,
        "DkgComplete envelope wire format changed"
    );
    assert_envelope_structure(&encoded, "dk");
}

#[test]
fn envelope_ts_canonical_cbor() {
    let payload = encode(&ts_payload());
    let encoded = encode_envelope(DfrostEventKind::ThresholdSign, payload);
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ENVELOPE_TS_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_ENVELOPE_TS_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_ENVELOPE_TS_HEX,
        "ThresholdSign envelope wire format changed"
    );
    assert_envelope_structure(&encoded, "ts");
}

#[test]
fn envelope_vb_canonical_cbor() {
    let payload = encode(&vb_payload());
    let encoded = encode_envelope(DfrostEventKind::VrfBeacon, payload);
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ENVELOPE_VB_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_ENVELOPE_VB_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_ENVELOPE_VB_HEX,
        "VrfBeacon envelope wire format changed"
    );
    assert_envelope_structure(&encoded, "vb");
}

#[test]
fn envelope_rf_round1_canonical_cbor() {
    let payload = encode(&rf_round1_payload());
    let encoded = encode_envelope(DfrostEventKind::ProactiveRefresh, payload);
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ENVELOPE_RF_ROUND1_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_ENVELOPE_RF_ROUND1_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_ENVELOPE_RF_ROUND1_HEX,
        "ProactiveRefresh (rn=1) envelope wire format changed"
    );
    assert_envelope_structure(&encoded, "rf");
}

#[test]
fn envelope_rf_round2_canonical_cbor() {
    let payload = encode(&rf_round2_payload());
    let encoded = encode_envelope(DfrostEventKind::ProactiveRefresh, payload);
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ENVELOPE_RF_ROUND2_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_ENVELOPE_RF_ROUND2_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_ENVELOPE_RF_ROUND2_HEX,
        "ProactiveRefresh (rn=2) envelope wire format changed"
    );
    assert_envelope_structure(&encoded, "rf");
}

#[test]
fn envelope_rp_round1_canonical_cbor() {
    let payload = encode(&rp_round1_payload());
    let encoded = encode_envelope(DfrostEventKind::RepairShare, payload);
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ENVELOPE_RP_ROUND1_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_ENVELOPE_RP_ROUND1_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_ENVELOPE_RP_ROUND1_HEX,
        "RepairShare (rn=1) envelope wire format changed"
    );
    assert_envelope_structure(&encoded, "rp");
}

#[test]
fn envelope_rp_round2_canonical_cbor() {
    let payload = encode(&rp_round2_payload());
    let encoded = encode_envelope(DfrostEventKind::RepairShare, payload);
    let actual_hex = hex::encode(&encoded);
    if EXPECTED_ENVELOPE_RP_ROUND2_HEX.contains("FILL_AFTER") {
        panic!("REGENERATE EXPECTED_ENVELOPE_RP_ROUND2_HEX = \"{actual_hex}\";");
    }
    assert_eq!(
        actual_hex, EXPECTED_ENVELOPE_RP_ROUND2_HEX,
        "RepairShare (rn=2) envelope wire format changed"
    );
    assert_envelope_structure(&encoded, "rp");
}
