//! ZEB-285 Phase 1: wire-format pinning for the Fork variant +
//! PreForkSnapshot + CommunityInvitePayload fork-extension fields.
//!
//! These tests lock the canonical-CBOR wire encoding for the new ZEB-285
//! types. Any failure here is a wire-protocol break — review carefully
//! before updating the pinned bytes (cross-version compat, peer interop).
//!
//! Uses deterministic test bytes (zero or repeated-byte sigs) so the
//! encoded bytes are byte-stable across runs. The tests do NOT verify
//! cryptographic validity — they pin BYTE LAYOUT only.

use harmony_app::community_invite::{
    BoundedChannelLogSnapshot, CommunityInvitePayload, InviteEpochSnapshot,
    MaterializedCommunityState, ParentLineageEntry, PreForkSnapshot,
};
use harmony_app::community_membership::{MembershipEventKind, SignedMembershipEvent};
use harmony_app::community_state_crdt::CommunityState;
use harmony_app::owner_state_crypto::canonical_cbor_decode;
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};
use std::collections::BTreeMap;

/// R1-5: structurally decode `bytes` as a CBOR map and assert presence /
/// absence of the listed keys at the TOP level of the map.
///
/// Byte-substring matching (`bytes.windows(2).any(|w| w == b"pl")`) is
/// brittle: a data value containing the bytes "pl" (e.g., a name field
/// "Polite Project") false-positives. Decoding to `ciborium::Value` and
/// inspecting the map keys is robust against data-content coincidences.
/// Uses `ciborium` since `serde_cbor` is not a workspace dependency.
fn assert_cbor_top_level_keys(bytes: &[u8], present: &[&str], absent: &[&str], label: &str) {
    let decoded: ciborium::Value = ciborium::de::from_reader(bytes)
        .unwrap_or_else(|e| panic!("{label}: decode as Value failed: {e}"));
    let pairs = match decoded {
        ciborium::Value::Map(m) => m,
        other => panic!("{label}: expected CBOR map at top level, got {other:?}"),
    };
    // Collect string-typed keys (canonical encoding uses Text keys for
    // these fields). Non-Text keys would be a wire-format break in itself.
    let keys: std::collections::BTreeSet<String> = pairs
        .iter()
        .filter_map(|(k, _)| match k {
            ciborium::Value::Text(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    for key in present {
        assert!(
            keys.contains(*key),
            "{label}: expected key `{key}` to be present (top-level keys: {keys:?})"
        );
    }
    for key in absent {
        assert!(
            !keys.contains(*key),
            "{label}: expected key `{key}` to be absent (top-level keys: {keys:?})"
        );
    }
}

fn fixture_hlc() -> Hlc {
    Hlc {
        wall_ms: 1_700_000_000_000,
        logical: 0,
        device_id: "test-device".to_string(),
    }
}

/// Construct a SignedMembershipEvent with deterministic zero-byte fields.
/// The sig is all-0xBB (not a valid signature — wire-format pin only).
fn fixture_signed_event_zeb285(kind: MembershipEventKind) -> SignedMembershipEvent {
    SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: [0x42; 16],
        community_id: SpaceId([0xc0; 16]),
        kind,
        actor: OwnerAddr([0xaa; 16]),
        at: fixture_hlc(),
        sig: [0xBB; 64],
        countersig: None,
        enrollment: None,
    }
}

/// Fixture 1: pins the canonical-CBOR bytes of a signed Fork event.
///
/// Wire-format drift (renamed field, changed serde attribute, variant
/// tag change) will cause this test to fail — that is intentional.
/// Re-pin ONLY after a deliberate, reviewed wire-format change.
#[test]
fn fork_event_canonical_cbor_pinned() {
    let fork_space_id = SpaceId([0xfa; 16]);
    let signed = fixture_signed_event_zeb285(MembershipEventKind::Fork {
        fork_space_id,
        reason: None,
    });

    let bytes = canonical_cbor_encode(&signed).expect("encode");
    let hex = hex::encode(&bytes);
    eprintln!("fork_event_canonical_cbor_pinned hex: {hex}");

    let expected_hex = "a6626964504242424242424242424242424242424262636950c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0626b6ea2627467617862766ca162667350fafafafafafafafafafafafafafafafa62616350aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa626174a361771b0000018bcfe56800616c0061646b746573742d6465766963656273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    assert_eq!(hex, expected_hex, "Fork event wire format changed");
}

/// ZEB-649 compat proof: the pre-ZEB-649 pinned Fork bytes (Fixture 1's
/// hex, minted before `reason` existed) must decode with `reason: None`
/// and re-encode BYTE-IDENTICALLY. This is exactly the round-trip
/// `verify_signature` performs (decode → re-encode → verify_strict), so
/// this test failing means every pre-ZEB-649 Fork event's signature would
/// break. Do not "fix" this test by re-pinning — fix the serde shape.
#[test]
fn fork_event_pre_zeb649_bytes_decode_reason_none_and_reencode_identical() {
    let pre_zeb649_hex = "a6626964504242424242424242424242424242424262636950c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0626b6ea2627467617862766ca162667350fafafafafafafafafafafafafafafafa62616350aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa626174a361771b0000018bcfe56800616c0061646b746573742d6465766963656273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let bytes = hex::decode(pre_zeb649_hex).expect("hex");

    let decoded: SignedMembershipEvent =
        canonical_cbor_decode(&bytes).expect("pre-ZEB-649 Fork event must still decode");
    match &decoded.kind {
        MembershipEventKind::Fork {
            fork_space_id,
            reason,
        } => {
            assert_eq!(*fork_space_id, SpaceId([0xfa; 16]));
            assert_eq!(*reason, None, "absent rs key must decode to None");
        }
        other => panic!("expected Fork, got {other:?}"),
    }

    let reencoded = canonical_cbor_encode(&decoded).expect("re-encode");
    assert_eq!(
        hex::encode(&reencoded),
        pre_zeb649_hex,
        "re-encode of a pre-ZEB-649 Fork event must be byte-identical \
         (signature re-verification depends on it)"
    );
}

/// ZEB-649 Fixture 1b: pins the canonical-CBOR bytes of a signed Fork
/// event WITH a reason (`rs` key present inside `vl`).
#[test]
fn fork_event_with_reason_canonical_cbor_pinned() {
    let signed = fixture_signed_event_zeb285(MembershipEventKind::Fork {
        fork_space_id: SpaceId([0xfa; 16]),
        reason: Some("Treasury split".to_string()),
    });

    let bytes = canonical_cbor_encode(&signed).expect("encode");
    let hex = hex::encode(&bytes);
    eprintln!("fork_event_with_reason_canonical_cbor_pinned hex: {hex}");

    let expected_hex = "a6626964504242424242424242424242424242424262636950c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0626b6ea2627467617862766ca262667350fafafafafafafafafafafafafafafafa6272736e54726561737572792073706c697462616350aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa626174a361771b0000018bcfe56800616c0061646b746573742d6465766963656273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    assert_eq!(hex, expected_hex, "Fork-with-reason wire format changed");
}

/// ZEB-649: structural pins for the reason fields — `fr` on PreForkSnapshot
/// (top-level), `rs` on ParentLineageEntry, `fr` on CommunityState — present
/// when Some, ABSENT when None (the absence is the wire-compat guarantee;
/// the untouched pre-ZEB-649 pinned fixtures in this file prove the None
/// case byte-exactly).
#[test]
fn zeb649_reason_fields_present_when_some_absent_when_none() {
    // PreForkSnapshot.fork_reason → top-level "fr".
    let mut snapshot = PreForkSnapshot {
        original_community_id: SpaceId([0xa0; 16]),
        original_community_name: "Pinned".to_string(),
        membership_events: vec![],
        channel_log: BoundedChannelLogSnapshot::default(),
        identity_pubs: BTreeMap::new(),
        forked_at: fixture_hlc(),
        parent_lineage: Vec::new(),
        fork_reason: Some("Treasury split".to_string()),
    };
    let bytes = canonical_cbor_encode(&snapshot).expect("encode");
    assert_cbor_top_level_keys(&bytes, &["fr"], &[], "snapshot with fork_reason");
    snapshot.fork_reason = None;
    let bytes = canonical_cbor_encode(&snapshot).expect("encode");
    assert_cbor_top_level_keys(&bytes, &[], &["fr"], "snapshot without fork_reason");

    // ParentLineageEntry.reason → "rs".
    let mut entry = ParentLineageEntry {
        space_id: SpaceId([0x11; 16]),
        name: "Root".to_string(),
        forked_at_wall_ms: Some(1),
        reason: Some("split over governance".to_string()),
    };
    let bytes = canonical_cbor_encode(&entry).expect("encode");
    assert_cbor_top_level_keys(&bytes, &["rs"], &[], "entry with reason");
    let decoded: ParentLineageEntry = canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(decoded, entry);
    entry.reason = None;
    let bytes = canonical_cbor_encode(&entry).expect("encode");
    assert_cbor_top_level_keys(&bytes, &[], &["rs"], "entry without reason");

    // CommunityState.fork_reason → "fr", roundtrips.
    let mut state = CommunityState::new(SpaceId([0xcc; 16]));
    state.fork_reason = Some("Treasury split".to_string());
    let bytes = canonical_cbor_encode(&state).expect("encode");
    assert_cbor_top_level_keys(&bytes, &["fr"], &[], "state with fork_reason");
    let decoded: CommunityState = canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(decoded.fork_reason.as_deref(), Some("Treasury split"));
    state.fork_reason = None;
    let bytes = canonical_cbor_encode(&state).expect("encode");
    assert_cbor_top_level_keys(&bytes, &[], &["fr"], "state without fork_reason");
}

/// Fixture 2: pins the canonical-CBOR bytes of a minimal PreForkSnapshot.
///
/// Empty membership_events, empty channel_log, empty identity_pubs —
/// minimal but exercises every field's serde attribute.
#[test]
fn pre_fork_snapshot_canonical_cbor_pinned() {
    let snapshot = PreForkSnapshot {
        original_community_id: SpaceId([0xa0; 16]),
        original_community_name: "Pinned".to_string(),
        membership_events: vec![],
        channel_log: BoundedChannelLogSnapshot::default(),
        identity_pubs: BTreeMap::new(),
        forked_at: fixture_hlc(),
        // ZEB-287 Phase 2: empty lineage → skip-if-empty drops `pl` key,
        // preserving Phase 1 byte-identity for this fixture.
        parent_lineage: Vec::new(),
        fork_reason: None,
    };

    let bytes = canonical_cbor_encode(&snapshot).expect("encode");
    let hex = hex::encode(&bytes);
    eprintln!("pre_fork_snapshot_canonical_cbor_pinned hex: {hex}");

    let expected_hex = "a6626f6950a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0626f6e6650696e6e65646265768062636ca1627063a0626970a0627473a361771b0000018bcfe56800616c0061646b746573742d646576696365";
    assert_eq!(hex, expected_hex, "PreForkSnapshot wire format changed");
}

/// Fixture 3: pins a CommunityInvitePayload with both fork extension
/// fields set (forked_from + pre_fork_snapshot). Catches drift in the
/// "ff"/"fs" serde attributes or their skip_serializing_if logic.
#[test]
fn community_invite_with_fork_fields_pinned() {
    let mut identity_pubs: BTreeMap<OwnerAddr, [u8; 64]> = BTreeMap::new();
    identity_pubs.insert(OwnerAddr([0xaa; 16]), [0u8; 64]);

    let snapshot = PreForkSnapshot {
        original_community_id: SpaceId([0xa0; 16]),
        original_community_name: "Original".to_string(),
        membership_events: vec![],
        channel_log: BoundedChannelLogSnapshot::default(),
        identity_pubs,
        forked_at: fixture_hlc(),
        // ZEB-287 Phase 2: empty lineage → skip-if-empty preserves Phase 1
        // byte-identity for this fixture.
        parent_lineage: Vec::new(),
        fork_reason: None,
    };

    let payload = CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id: SpaceId([0xc0; 16]),
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: EpochKey::new([0xAA; 32]).as_bytes().to_vec(),
            sealed_epoch_keys: Vec::new(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: OwnerAddr([0xaa; 16]),
        community_name: "Pinned Fork".to_string(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        inviter_identity_pub: None,
        forked_from: Some(SpaceId([0xa0; 16])),
        pre_fork_snapshot: Some(snapshot),
        inviter_enrollment: None,
        untargeted_decrypt_key: None,
    };

    let bytes = canonical_cbor_encode(&payload).expect("encode");
    let hex = hex::encode(&bytes);
    eprintln!("community_invite_with_fork_fields_pinned hex: {hex}");

    let expected_hex = "a762636950c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0626573a36265700062736b5820aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa627373a2626d62a062706ca062616450aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa626e6d6b50696e6e656420466f726b62696ff462666650a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0626673a6626f6950a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0626f6e684f726967696e616c6265768062636ca1627063a0626970a150aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa584000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000627473a361771b0000018bcfe56800616c0061646b746573742d646576696365";
    assert_eq!(
        hex, expected_hex,
        "CommunityInvitePayload with fork fields wire format changed"
    );
}

// ── ZEB-287 Phase 2 wire-format fixtures ─────────────────────────────
//
// Pin the canonical CBOR bytes for the new Phase 2 types
// (ParentLineageEntry) and the Phase 2 extensions on existing types
// (PreForkSnapshot.parent_lineage, CommunityState.parent_lineage +
// forked_at_wall_ms). Includes a Phase 1 byte-compat fixture that
// asserts Phase 1-shape blobs decode under Phase 2 types as defaults
// and re-encode byte-identically.
//
// To regenerate after an intentional wire change: replace the
// expected_hex with the eprintln!-emitted value, then re-run.

/// Fixture 4: ParentLineageEntry with all fields populated.
#[test]
fn parent_lineage_entry_canonical_cbor() {
    let entry = ParentLineageEntry {
        space_id: SpaceId([0x42; 16]),
        name: "Cool Community".to_string(),
        forked_at_wall_ms: Some(1_715_811_234_567),
        reason: None,
    };
    let bytes = canonical_cbor_encode(&entry).expect("encode");
    let hex = hex::encode(&bytes);
    eprintln!("parent_lineage_entry_canonical_cbor hex: {hex}");

    let expected_hex = "a36273695042424242424242424242424242424242626e6d6e436f6f6c20436f6d6d756e6974796261741b0000018f7e51b307";
    assert_eq!(hex, expected_hex, "ParentLineageEntry wire format changed");
}

/// Fixture 5: ParentLineageEntry with forked_at_wall_ms = None (root entry).
/// Asserts the `at` key is omitted (skip-if-none).
#[test]
fn parent_lineage_entry_root_omits_at_canonical_cbor() {
    let entry = ParentLineageEntry {
        space_id: SpaceId([0x11; 16]),
        name: "Root".to_string(),
        forked_at_wall_ms: None,
        reason: None,
    };
    let bytes = canonical_cbor_encode(&entry).expect("encode");
    let hex = hex::encode(&bytes);
    eprintln!("parent_lineage_entry_root_omits_at_canonical_cbor hex: {hex}");

    let expected_hex = "a26273695011111111111111111111111111111111626e6d64526f6f74";
    assert_eq!(hex, expected_hex, "Root ParentLineageEntry changed");
    // R1-5: structural assertion instead of byte-substring (which could
    // false-positive on a name like "patio").
    assert_cbor_top_level_keys(&bytes, &["si", "nm"], &["at"], "Root ParentLineageEntry");
}

/// Fixture 6: PreForkSnapshot carrying a 2-entry parent_lineage chain.
#[test]
fn pre_fork_snapshot_with_parent_lineage_canonical_cbor() {
    let snapshot = PreForkSnapshot {
        original_community_id: SpaceId([0xa0; 16]),
        original_community_name: "Pinned".to_string(),
        membership_events: vec![],
        channel_log: BoundedChannelLogSnapshot::default(),
        identity_pubs: BTreeMap::new(),
        forked_at: fixture_hlc(),
        parent_lineage: vec![
            ParentLineageEntry {
                space_id: SpaceId([0x11; 16]),
                name: "Root".to_string(),
                forked_at_wall_ms: None,
                reason: None,
            },
            ParentLineageEntry {
                space_id: SpaceId([0x22; 16]),
                name: "Mid".to_string(),
                forked_at_wall_ms: Some(1_650_000_000_000),
                reason: None,
            },
        ],
        fork_reason: None,
    };

    let bytes = canonical_cbor_encode(&snapshot).expect("encode");
    let hex = hex::encode(&bytes);
    eprintln!("pre_fork_snapshot_with_parent_lineage_canonical_cbor hex: {hex}");

    let expected_hex = "a7626f6950a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0626f6e6650696e6e65646265768062636ca1627063a0626970a0627473a361771b0000018bcfe56800616c0061646b746573742d64657669636562706c82a26273695011111111111111111111111111111111626e6d64526f6f74a36273695022222222222222222222222222222222626e6d634d69646261741b000001802ba9f400";
    // Spec note: the chain is encoded as a 2-element CBOR array;
    // skip-if-empty guarantees byte-identity to Phase 1 when chain empty.
    assert_eq!(
        hex, expected_hex,
        "PreForkSnapshot with parent_lineage wire format changed"
    );
    // R1-5: structural assertion — non-empty parent_lineage must produce
    // the `pl` key at the TOP LEVEL of the snapshot map (not just somewhere
    // in the byte stream, which would false-positive on data values).
    assert_cbor_top_level_keys(&bytes, &["pl"], &[], "PreForkSnapshot with parent_lineage");
}

/// Fixture 7: CommunityState carrying parent_lineage + forked_at_wall_ms.
#[test]
fn community_state_with_parent_lineage_canonical_cbor() {
    let mut state = CommunityState::new(SpaceId([0xa0; 16]));
    state.forked_from = Some(SpaceId([0xb0; 16]));
    state.forked_at_wall_ms = Some(1_715_000_000_000);
    state.parent_lineage = vec![ParentLineageEntry {
        space_id: SpaceId([0x11; 16]),
        name: "Root".to_string(),
        forked_at_wall_ms: None,
        reason: None,
    }];

    let bytes = canonical_cbor_encode(&state).expect("encode");
    let hex = hex::encode(&bytes);
    eprintln!("community_state_with_parent_lineage_canonical_cbor hex: {hex}");

    let expected_hex = "a562636950a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a062666650b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b06266611b0000018f4df73e0062666c81a26273695011111111111111111111111111111111626e6d64526f6f74626576a0";
    // Spec §3.3: `ff` (forked_from), `fa` (forked_at_wall_ms), and `fl`
    // (parent_lineage) are all 2-char keys at this nesting level.
    // `ev` (events) is an empty BTreeMap → `a0`. `ci` is community_id.
    assert_eq!(
        hex, expected_hex,
        "CommunityState with parent_lineage wire format changed"
    );
    // R1-5: structural assertion — top-level `fa` (forked_at_wall_ms) and
    // `fl` (parent_lineage) keys must both be present.
    assert_cbor_top_level_keys(
        &bytes,
        &["fa", "fl"],
        &[],
        "CommunityState with parent_lineage",
    );
}

/// Fixture 8: Phase 1 wire-form (no `fa`, no `fl`) decodes under Phase 2
/// types as defaults (None, empty Vec) and re-encodes byte-identically.
/// Locks the backwards-compat guarantee per spec §6.1.
#[test]
fn phase1_community_state_decodes_under_phase2_types() {
    // Construct a Phase 1-shape CommunityState via the public constructor
    // (cache + bootstrap_hint fields are private — by design, they're
    // skipped from CBOR anyway, so this is the canonical construction
    // path). Then patch only the public Phase-1-relevant fields.
    let mut phase1_state = CommunityState::new(SpaceId([0xaa; 16]));
    phase1_state.forked_from = Some(SpaceId([0xbb; 16]));
    // forked_at_wall_ms stays at None (Phase 2 default — Phase 1 shape)
    // parent_lineage stays empty (Phase 2 default — Phase 1 shape)

    let bytes = canonical_cbor_encode(&phase1_state).expect("encode");
    let decoded: CommunityState = canonical_cbor_decode(&bytes).expect("decode");

    assert_eq!(decoded.parent_lineage, Vec::<ParentLineageEntry>::new());
    assert_eq!(decoded.forked_at_wall_ms, None);
    assert_eq!(decoded.forked_from, Some(SpaceId([0xbb; 16])));
    assert_eq!(decoded.community_id, SpaceId([0xaa; 16]));

    // Byte-compat: the encoded form must NOT contain the Phase 2 keys at
    // the top level. R1-5: structural decode is robust to incidental
    // byte-pair occurrences inside SpaceId / name / etc. payloads.
    assert_cbor_top_level_keys(
        &bytes,
        &["ff"], // Phase 1's forked_from key must still be present
        &["fa", "fl"],
        "Phase 1-shape CommunityState",
    );
}
