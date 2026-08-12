//! ZEB-880 round 2: an AVALON-shaped multi-address host must be able to
//! publish its community rendezvous slot record through the REAL pkarr
//! packet build.
//!
//! Round 1 (#637) bounded every published record to `MAX_RECORD_CBOR_BYTES`
//! derived from `SignedPacket::MAX_BYTES = 1104` — but that outer bound
//! includes a 104-byte crypto envelope (pubkey + signature + timestamp) and
//! is only checked when *deserializing* a packet. The gate that actually
//! fails a publish is pkarr's builder (`signed_packet.rs`): the encoded DNS
//! packet must be ≤ 1000 bytes, and inside it the TXT record's name is
//! origin-normalized to `_r.<52-char z32 pubkey>.` (57 bytes of name, not
//! the bare `_r`). The true record ceiling is therefore 687 B of canonical
//! CBOR, not 729 — a record landing in 688..=729 passed round 1's bound yet
//! failed the real build with `RecordTooLarge` every publish cycle, which is
//! exactly AVALON's reopen evidence (its bounded rendezvous record ≈ 716 B).
//!
//! Round 1's tests asserted the CBOR budget without ever building a real
//! packet, so they stayed green while production failed. This test closes
//! that gap end-to-end: real `CommunityRendezvousPublisher` → real
//! `PkarrPublisher` → strict `MockPkarrRelay` (which verifies the relay
//! payload with `SignedPacket::from_relay_payload`), resolved back via
//! `PkarrResolver`.

use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use harmony_app::community_rendezvous::{decode_rendezvous_blob, rendezvous_slot_verifying_key};
use harmony_app::community_rendezvous_publisher::CommunityRendezvousPublisher;
use harmony_app::owner_state_types::{EpochKey, OwnerAddr, SpaceId};
use harmony_app::reachability_record::{ButlerSetEntry, ReachabilityAnnouncePayload};
use harmony_pkarr::{
    current_epoch_id, testing::MockPkarrRelay, PkarrPublisher, PkarrResolver, RelayClient,
    RelayPool,
};

/// iroh's `RelayUrl` serializes its host as an FQDN with a TRAILING DOT —
/// the field-measured form is `https://usw1-1.relay.n0.iroh.link./` (see the
/// OBS-Koya measurement on ZEB-880). The single extra byte per URL matters:
/// this fixture sits within a few bytes of pkarr's packet cap, exactly like
/// the real host it reproduces.
const RELAY_URL: &str = "https://usw1-1.relay.n0.iroh.link./";

fn butler(s: u8) -> ButlerSetEntry {
    ButlerSetEntry {
        device_id: [s; 16],
        iroh_endpoint_id: [s.wrapping_add(1); 32],
        device_ed25519_verify: [s.wrapping_add(2); 32],
        home_relay: RELAY_URL.to_string(),
        pinned: false,
    }
}

/// AVALON's exact advertised reachability at the reopen: 2 IPv4 legs (one
/// public, one RFC1918) + 3 global IPv6 legs, the production relay URL, and a
/// full 2-entry butler set (AVALON hosts the community AND advertises relay).
/// Returned as the bare payload CBOR the shared `blob_builder` emits — the
/// rendezvous publisher decodes, caps, bounds, and re-encodes it itself.
fn avalon_blob() -> Vec<u8> {
    let payload = ReachabilityAnnouncePayload {
        iroh_node_id: [0x03; 32],
        home_relay_url: RELAY_URL.to_string(),
        direct_addresses: vec![
            "165.162.82.51:35102".parse().unwrap(),
            "192.168.1.59:63933".parse().unwrap(),
            "[2603:8002:ddf0:3380::1787]:63934".parse().unwrap(),
            "[2603:8002:ddf0:3380:6b34:be5b:30f8:5f6e]:63934"
                .parse()
                .unwrap(),
            "[2603:8002:ddf0:3380:bc5e:7e59:7bfb:19a6]:63934"
                .parse()
                .unwrap(),
        ],
        announced_at_ms: now_ms(),
        // Zero-filled exactly as the production blob_builder writes it.
        identity_signature: [0u8; 64],
        butler_set: vec![butler(0x10), butler(0x20)],
        bs_at: now_ms(),
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&payload, &mut buf).expect("encode routing blob");
    buf
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[tokio::test]
async fn avalon_shaped_rendezvous_record_publishes_within_pkarr_cap() {
    let result = tokio::time::timeout(Duration::from_secs(45), async {
        // Strict mock: PUTs are validated as real pkarr relay payloads, so a
        // stored record proves the full build → sign → encode path succeeded.
        let relay = MockPkarrRelay::start_strict().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(RelayClient::new(pool));
        let publisher = Arc::new(PkarrPublisher::new(Arc::clone(&client)));
        let _ph = Arc::clone(&publisher).spawn();

        let id_sk = SigningKey::from_bytes(&[0x55; 32]);
        let mut id_pub = [0u8; 64];
        id_pub[32..].copy_from_slice(&id_sk.verifying_key().to_bytes());
        let device_sk = Arc::new(SigningKey::from_bytes(&[0x66; 32]));

        let rdv = CommunityRendezvousPublisher::new(
            Arc::clone(&publisher),
            id_sk,
            id_pub,
            device_sk,
            Arc::new(avalon_blob),
        );

        let community = SpaceId([0x6f; 16]);
        let epoch_key = EpochKey::new([0x42; 32]);
        let me = OwnerAddr([0x01; 16]);
        rdv.refresh_slot(community, epoch_key.clone(), vec![me], me)
            .await;

        // Condition-poll until the slot-0 record is resolvable from the relay.
        // A FRESH resolver per attempt sidesteps the resolver's 60 s negative
        // cache (a first-attempt miss would otherwise pin every later poll to
        // the cached None). Each attempt tries BOTH the current and previous
        // epoch's slot key: the publisher derives its key at publish time, so
        // across an epoch boundary (weekly — rare but CI-real) the record may
        // sit under either epoch's key depending on which side of the boundary
        // the publish landed, and the republish under the new key can be a
        // full publish cycle away (CodeRabbit #657).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let record = 'found: loop {
            let epoch_now = current_epoch_id(now_ms());
            for epoch_id in [epoch_now, epoch_now.saturating_sub(1)] {
                let vk = rendezvous_slot_verifying_key(&epoch_key, 0, epoch_id);
                let resolver = PkarrResolver::new(Arc::clone(&client));
                if let Ok(Some(rec)) = resolver.resolve(&vk).await {
                    break 'found rec;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "rendezvous slot record never reached the relay — the publish is \
                 failing inside the real pkarr packet build (RecordTooLarge): the \
                 bounded record still exceeds the 1000-byte encoded-DNS-packet \
                 gate (ZEB-880 round 2)"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };

        // The record that landed is the bounded rendezvous blob: vouch riding
        // along, butler capped to one seal target, and at least one direct
        // address surviving the trim.
        let (payload, vouch) =
            decode_rendezvous_blob(&record.routing_blob).expect("published blob decodes");
        assert!(
            vouch.is_some(),
            "membership vouch must ride the rendezvous blob"
        );
        assert_eq!(
            payload.butler_set.len(),
            1,
            "butler set capped to one seal target"
        );
        assert!(
            !payload.direct_addresses.is_empty(),
            "at least one direct address must survive the trim"
        );
    })
    .await;
    result.expect("test timed out");
}
