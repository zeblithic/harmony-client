//! Golden CBOR fixtures for ZEB-321 Phase 1 ReachabilityAnnounce wire type.
//! Pinned bytes prevent silent wire-format changes — if any of these
//! tests fail, treat it as a wire-protocol break and review carefully
//! (cross-version compatibility, peer interop, etc.).
//!
//! Mirrors src-tauri/tests/wire_format/community_fixtures.rs.

use harmony_app::community_membership::{MembershipEventKind, SignedMembershipEvent};
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use harmony_app::reachability_record::{
    build_signed_payload, verify_inner_signature, ButlerSetEntry, ReachabilityAnnouncePayload,
};
use harmony_identity::PrivateIdentity;

fn fixture_hlc() -> Hlc {
    Hlc {
        wall_ms: 1_700_000_000_000,
        logical: 0,
        device_id: "fix".into(),
    }
}

fn fixture_payload() -> ReachabilityAnnouncePayload {
    ReachabilityAnnouncePayload {
        iroh_node_id: [0xAB; 32],
        home_relay_url: "https://derp.example/".into(),
        direct_addresses: vec![],
        announced_at_ms: 1_700_000_000_000,
        identity_signature: [0xCD; 64],
        butler_set: Vec::new(),
        bs_at: 0,
    }
}

fn fixture_signed_event(kind: MembershipEventKind) -> SignedMembershipEvent {
    SignedMembershipEvent {
        id: [0x42; 16],
        community_id: SpaceId([0x37; 16]),
        kind,
        actor: OwnerAddr([0x11; 16]),
        at: fixture_hlc(),
        sig: [0xBB; 64],
        countersig: None,
        enrollment: None,
    }
}

#[test]
fn reachability_announce_payload_wire_bytes_pinned() {
    let payload = fixture_payload();
    let bytes = canonical_cbor_encode(&payload).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("reachability_announce_payload hex: {hex}");
    assert_eq!(
        hex,
        "a5626e645820abababababababababababababababababababababababababababababababab62726c7568747470733a2f2f646572702e6578616d706c652f626461806274731b0000018bcfe568006273675840cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd",
        "ReachabilityAnnouncePayload wire format changed"
    );
}

/// ZEB-418 durable seal-targets: a `ReachabilityAnnouncePayload` signed WITH a
/// non-empty butler set must (a) round-trip through canonical CBOR carrying the
/// butler set + freshness stamp intact, and (b) still pass `verify_inner_
/// signature` after decode — proving the inner identity signature binds the
/// seal-targets so a relay/co-member can't forge or strip them.
#[test]
fn signed_reachability_with_butler_set_round_trips_and_verifies() {
    let identity = PrivateIdentity::from_seed(&[0xAA; 32]);
    let public = identity.public_identity();
    let actor = OwnerAddr(public.address_hash);
    let hlc = fixture_hlc();
    let butler = ButlerSetEntry {
        device_id: [0x10; 16],
        iroh_endpoint_id: [0x11; 32],
        device_ed25519_verify: [0x12; 32],
        home_relay: "https://use1-1.relay.iroh.network./".into(),
        pinned: false,
    };

    let payload = build_signed_payload(
        [0xAB; 32],
        "https://derp.example/".into(),
        vec![],
        1_700_000_000_000,
        &actor,
        &hlc,
        vec![butler.clone()],
        1_700_000_000_000,
        &identity,
    )
    .expect("sign");

    // Inner sig verifies on the freshly-built payload.
    verify_inner_signature(&payload, &actor, &hlc, &public.verifying_key).expect("verify");

    // Canonical-encode → decode: butler set + stamp must survive.
    let bytes = canonical_cbor_encode(&payload).expect("encode");
    let decoded: ReachabilityAnnouncePayload =
        ciborium::de::from_reader(&bytes[..]).expect("decode");
    assert_eq!(decoded.butler_set, vec![butler]);
    assert_eq!(decoded.bs_at, 1_700_000_000_000);

    // Inner sig still verifies after the round-trip.
    verify_inner_signature(&decoded, &actor, &hlc, &public.verifying_key)
        .expect("verify after round-trip");
}

#[test]
fn signed_event_reachability_announce_wire_bytes_pinned() {
    let event = fixture_signed_event(MembershipEventKind::ReachabilityAnnounce {
        payload: fixture_payload(),
    });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_reachability_announce hex: {hex}");
    assert_eq!(
        hex,
        "a662696450424242424242424242424242424242426263695037373737373737373737373737373737626b6ea2627467616162766ca162706ca5626e645820abababababababababababababababababababababababababababababababab62726c7568747470733a2f2f646572702e6578616d706c652f626461806274731b0000018bcfe568006273675840cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd6261635011111111111111111111111111111111626174a361771b0000018bcfe56800616c006164636669786273675840bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "SignedMembershipEvent::ReachabilityAnnounce wire format changed"
    );
}
