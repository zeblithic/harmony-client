//! Pins the on-the-wire bytes of harmony-client's routing_blob (the opaque
//! CBOR-encoded `ReachabilityAnnouncePayload` embedded in a
//! `PkarrRoutingRecord`).
//!
//! Mirrors the Phase 1 pin in `wire_format/reachability_announce_fixtures.rs`.
//! If this test fails, the routing_blob wire format changed — every pkarr
//! record published by harmony-client v1 would be undecodable by old resolvers
//! without a migration. Regenerate only with full understanding.

use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::reachability_record::ReachabilityAnnouncePayload;

fn fixture_routing_payload() -> ReachabilityAnnouncePayload {
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

#[test]
fn routing_blob_canonical_cbor_pinned() {
    let payload = fixture_routing_payload();
    // harmony-client encodes the routing_blob via ciborium::into_writer, which
    // is the same codec canonical_cbor_encode uses (ciborium's serde impl).
    let mut routing_blob = Vec::new();
    ciborium::into_writer(&payload, &mut routing_blob).expect("encode routing_blob");

    // Also verify round-trip via canonical_cbor_encode (must be byte-identical).
    let canonical_bytes = canonical_cbor_encode(&payload).expect("canonical_cbor_encode");
    assert_eq!(
        routing_blob, canonical_bytes,
        "ciborium::into_writer and canonical_cbor_encode must produce identical bytes"
    );

    let hex = routing_blob
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    eprintln!("routing_blob hex: {hex}");

    // Pin: this is the exact byte sequence a PkarrRoutingRecord.routing_blob
    // field will contain for alice's reachability record with the fixture inputs.
    // Matches Phase 1's wire_format/reachability_announce_fixtures.rs pin.
    assert_eq!(
        hex,
        "a5626e645820abababababababababababababababababababababababababababababababab62726c7568747470733a2f2f646572702e6578616d706c652f626461806274731b0000018bcfe568006273675840cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd",
        "routing_blob wire format changed — pkarr-encoded ReachabilityAnnouncePayload drift detected"
    );
}
