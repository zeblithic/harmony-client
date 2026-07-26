//! ZEB-815 Task 3: sealed wire codec for the community address book.
//!
//! One codec serves both a live single-row publish and a full snapshot: a
//! live record publish is [`seal_records`] with a 1-element slice; a snapshot
//! is the same codec over the full row set. Mirrors the presence beacon seal
//! (`community_presence.rs`'s `seal_presence_beacon`/`open_presence_beacon`)
//! over `encrypt_voice_packet`/`decrypt_voice_packet`, with its own HKDF
//! `info` label, sentinel channel, and AAD domain so an address-book packet
//! can never be confused with (or opened as) a presence beacon.

use crate::community_address_book::{
    AddressBookEntry, AddressBookRow, CommunityAddressBook, UpsertOutcome,
};
use crate::community_channel_log::ChannelKey;
use crate::community_membership::ChannelId;
use crate::community_relay_resolver::CommunityRelayResolver;
use crate::community_state_sync::CommunitySyncRegistry;
use crate::owner_state_types::{EpochKey, SpaceId};
use crate::reachability_resolver::ReachabilityResolver;
use crate::voice_crypto::{decrypt_voice_packet, encrypt_voice_packet};
use hkdf::Hkdf;
use sha2::Sha256;
use std::sync::Arc;

/// Domain separator for sealed address-book packets (records + snapshots
/// alike — one codec, no second format).
pub const ADDRBOOK_AAD: &[u8] = b"harmony-addrbook-v1";

/// The address book has no channel, so the AEAD seam (which is
/// `(community, channel)` scoped) is bound with this sentinel. Distinct from
/// `community_presence.rs`'s `PRESENCE_SENTINEL_CHANNEL` ([0u8; 16]) so the
/// two domains never collide even before `ADDRBOOK_AAD` is considered.
pub const ADDRBOOK_SENTINEL_CHANNEL: ChannelId = ChannelId([1u8; 16]);

/// Minimum interval between full-snapshot publishes for a given community.
pub const ADDRBOOK_SNAPSHOT_COOLDOWN_MS: u64 = 60_000;

/// Upper bound on a sealed address-book packet (record or snapshot) accepted
/// for decryption. Enforced before any AEAD open to bound allocation from a
/// peer flooding the topic.
pub const ADDRBOOK_SNAPSHOT_MAX_BYTES: usize = 1_048_576;

/// HKDF-SHA256 derivation of the per-community address-book key from the
/// community epoch (membership) key. Mirrors `derive_presence_key` — same
/// salt (`community_id`), distinct `info` label — so the address-book key is
/// independent of the presence key (and every channel key) for the same
/// `(mk, community_id)`.
pub fn derive_addrbook_key(mk: &EpochKey, community_id: &SpaceId) -> ChannelKey {
    let salt = community_id.0;
    let info = b"addrbook:";
    let mut out = zeroize::Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(Some(&salt), mk.as_bytes())
        .expand(info, out.as_mut())
        .expect("32 <= 8160");
    ChannelKey::from_bytes(*out)
}

/// Seal `rows` (a single record or a full snapshot — same codec either way)
/// under the per-community address-book key.
pub fn seal_records(
    key: &ChannelKey,
    community: &SpaceId,
    rows: &[AddressBookRow],
) -> Result<Vec<u8>, String> {
    let mut plain = Vec::new();
    ciborium::into_writer(&rows, &mut plain).map_err(|e| format!("addrbook encode: {e}"))?;
    encrypt_voice_packet(
        key,
        community,
        &ADDRBOOK_SENTINEL_CHANNEL,
        ADDRBOOK_AAD,
        &plain,
    )
    .map_err(|e| format!("addrbook seal: {e}"))
}

/// Open + decode a sealed address-book packet. Returns `None` on any failure
/// (wrong key, wrong scope, tamper, or oversize) — callers drop silently.
pub fn open_records(
    key: &ChannelKey,
    community: &SpaceId,
    packet: &[u8],
) -> Option<Vec<AddressBookRow>> {
    if packet.len() > ADDRBOOK_SNAPSHOT_MAX_BYTES {
        return None;
    }
    let plain = decrypt_voice_packet(
        key,
        community,
        &ADDRBOOK_SENTINEL_CHANNEL,
        ADDRBOOK_AAD,
        packet,
    )
    .ok()?;
    ciborium::from_reader(plain.as_slice()).ok()
}

/// Result of ingesting one address-book row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngestOutcome {
    Applied(UpsertOutcome),
    BadSignature,
    NotMember,
    Malformed,
}

/// Pure ingest core for one already-unsealed, already-membership-checked row:
/// verify the entry's inner signature against `row.device` (the device's
/// Ed25519 verifying-key bytes), then upsert into `book` and — on a
/// materialized change (`Inserted`/`Replaced`) — fan out to the matching
/// resolver, mirroring the ReachabilityAnnounce/CommunityRelayAnnounce delta
/// arms this replaces (`lib.rs`).
///
/// Membership is NOT checked here — the caller ([`ingest_sealed_packet`])
/// gates on it per-row before calling in.
pub fn ingest_verified_row(
    book: &CommunityAddressBook,
    reachability_resolver: &ReachabilityResolver,
    community_relay_resolver: &CommunityRelayResolver,
    community: SpaceId,
    row: AddressBookRow,
    now_ms: u64,
) -> IngestOutcome {
    let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&row.device) else {
        return IngestOutcome::BadSignature;
    };
    let verified = match &row.entry {
        AddressBookEntry::Reachability(p) => {
            crate::reachability_record::verify_inner_signature(p, &row.actor, &row.at, &vk).is_ok()
        }
        AddressBookEntry::Relay(p) => {
            crate::community_relay_announce::verify_inner_signature(p, &row.actor, &row.at, &vk)
                .is_ok()
        }
    };
    if !verified {
        return IngestOutcome::BadSignature;
    }

    let outcome = book.upsert(community, row.clone(), now_ms);
    if matches!(outcome, UpsertOutcome::Inserted | UpsertOutcome::Replaced) {
        match row.entry {
            AddressBookEntry::Reachability(p) => {
                reachability_resolver.update(row.actor, p, row.at);
            }
            AddressBookEntry::Relay(p) => {
                community_relay_resolver.update(community, row.actor, p, row.at);
            }
        }
    }
    IngestOutcome::Applied(outcome)
}

/// Async wrapper: unseal `packet` under the community's current membership
/// key, then per-row gate on live membership before dispatching to
/// [`ingest_verified_row`]. Used by the event-loop wiring (Tasks 5/6).
///
/// A missing engine or an unseal/decode failure yields one `Malformed`
/// outcome for the whole packet (nothing to iterate); a row whose signer
/// is not a currently-enrolled, Joined member yields `NotMember` for that
/// row and is skipped (never reaches signature verification or the store).
pub async fn ingest_sealed_packet(
    registry: &Arc<CommunitySyncRegistry>,
    book: &CommunityAddressBook,
    reachability_resolver: &ReachabilityResolver,
    community_relay_resolver: &CommunityRelayResolver,
    community: SpaceId,
    packet: &[u8],
    now_ms: u64,
) -> Vec<IngestOutcome> {
    let Some(engine) = registry.engine_arc(&community).await else {
        return vec![IngestOutcome::Malformed];
    };
    let mk = engine.membership_key();
    let key = derive_addrbook_key(&mk, &community);
    let Some(rows) = open_records(&key, &community, packet) else {
        return vec![IngestOutcome::Malformed];
    };

    let mut outcomes = Vec::with_capacity(rows.len());
    for row in rows {
        let is_member = crate::voice_presence::beacon_signer_is_member(
            registry,
            &community,
            &row.actor,
            &row.device,
        )
        .await;
        if !is_member {
            outcomes.push(IngestOutcome::NotMember);
            continue;
        }
        outcomes.push(ingest_verified_row(
            book,
            reachability_resolver,
            community_relay_resolver,
            community,
            row,
            now_ms,
        ));
    }
    outcomes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_address_book::AddressBookEntry;
    use crate::community_channel_log::derive_presence_key;
    use crate::owner_state_types::{Hlc, OwnerAddr};
    use crate::reachability_record::ReachabilityAnnouncePayload;

    fn hlc(ms: u64) -> Hlc {
        Hlc {
            wall_ms: ms,
            logical: 0,
            device_id: "d".into(),
        }
    }

    fn row(seed: u8, ts: u64) -> AddressBookRow {
        AddressBookRow {
            entry: AddressBookEntry::Reachability(ReachabilityAnnouncePayload {
                iroh_node_id: [seed; 32],
                home_relay_url: "https://derp.example/".into(),
                direct_addresses: vec![],
                announced_at_ms: ts,
                identity_signature: [0; 64],
                butler_set: Vec::new(),
                bs_at: 0,
            }),
            actor: OwnerAddr([seed; 16]),
            device: [seed; 32],
            at: hlc(ts),
            stamped_at_ms: ts,
        }
    }

    fn fixture_key(community: &SpaceId) -> ChannelKey {
        derive_addrbook_key(&EpochKey::new([7u8; 32]), community)
    }

    #[test]
    fn seal_open_round_trip_single_and_many() {
        let c = SpaceId([0xc0; 16]);
        let key = fixture_key(&c);

        let one = vec![row(1, 1_000)];
        let sealed_one = seal_records(&key, &c, &one).unwrap();
        assert_eq!(open_records(&key, &c, &sealed_one), Some(one));

        let many: Vec<AddressBookRow> = (1..=5u8).map(|i| row(i, 1_000 + i as u64)).collect();
        let sealed_many = seal_records(&key, &c, &many).unwrap();
        assert_eq!(open_records(&key, &c, &sealed_many), Some(many));
    }

    #[test]
    fn wrong_key_fails_open() {
        let c = SpaceId([0xc0; 16]);
        let key = fixture_key(&c);
        let other = derive_addrbook_key(&EpochKey::new([9u8; 32]), &c);

        let rows = vec![row(1, 1_000)];
        let sealed = seal_records(&key, &c, &rows).unwrap();
        assert_eq!(open_records(&other, &c, &sealed), None);
    }

    #[test]
    fn tampered_packet_fails_open() {
        let c = SpaceId([0xc0; 16]);
        let key = fixture_key(&c);

        let rows = vec![row(1, 1_000)];
        let mut sealed = seal_records(&key, &c, &rows).unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01;
        assert_eq!(open_records(&key, &c, &sealed), None);
    }

    #[test]
    fn oversize_packet_rejected_before_decrypt() {
        let c = SpaceId([0xc0; 16]);
        let key = fixture_key(&c);
        let oversize = vec![0u8; ADDRBOOK_SNAPSHOT_MAX_BYTES + 1];
        assert_eq!(open_records(&key, &c, &oversize), None);
    }

    #[test]
    fn distinct_from_presence_seal() {
        let c = SpaceId([0xc0; 16]);
        let mk = EpochKey::new([7u8; 32]);
        let presence_key = derive_presence_key(&mk, &c);
        let addrbook_key = derive_addrbook_key(&mk, &c);
        assert_ne!(presence_key.as_bytes(), addrbook_key.as_bytes());
    }

    // ── Task 4: ingest gate (pure core) ─────────────────────────────

    use crate::community_relay_announce::{
        build_signed_community_relay_announce, CommunityRelayEntry,
    };
    use crate::reachability_record::build_signed_payload_with_key;

    #[test]
    fn verified_reachability_row_lands_in_book_and_resolver() {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x11; 32]);
        let vk = signing_key.verifying_key();
        let actor = OwnerAddr([0x01; 16]);
        let community = SpaceId([0xC0; 16]);
        let ts = 1_700_000_000_000u64;
        let at = hlc(ts);

        let payload = build_signed_payload_with_key(
            [0xAB; 32],
            "https://derp.example/".into(),
            vec![],
            ts,
            &actor,
            &at,
            Vec::new(),
            0,
            &signing_key,
        )
        .expect("build signed reachability payload");

        let row = AddressBookRow {
            entry: AddressBookEntry::Reachability(payload),
            actor,
            device: vk.to_bytes(),
            at,
            stamped_at_ms: ts,
        };

        let book = CommunityAddressBook::new();
        let reach_resolver = ReachabilityResolver::new();
        let relay_resolver = CommunityRelayResolver::new();

        let outcome =
            ingest_verified_row(&book, &reach_resolver, &relay_resolver, community, row, ts);
        assert_eq!(outcome, IngestOutcome::Applied(UpsertOutcome::Inserted));
        assert!(!reach_resolver.resolve(&actor).is_empty());
    }

    #[test]
    fn verified_relay_row_lands_in_book_and_relay_resolver() {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x22; 32]);
        let vk = signing_key.verifying_key();
        let actor = OwnerAddr([0x02; 16]);
        let community = SpaceId([0xC1; 16]);
        let ts = 1_700_000_000_000u64;
        let at = hlc(ts);

        let relay_entry = CommunityRelayEntry {
            relay_device_id: [0x44; 16],
            iroh_endpoint_id: [0x55; 32],
            relay_device_ed25519_verify: vk.to_bytes(),
            home_relay: "https://r.example/".into(),
        };
        let payload =
            build_signed_community_relay_announce(relay_entry, ts, &actor, &at, &signing_key)
                .expect("build signed relay payload");

        let row = AddressBookRow {
            entry: AddressBookEntry::Relay(payload),
            actor,
            device: vk.to_bytes(),
            at,
            stamped_at_ms: ts,
        };

        let book = CommunityAddressBook::new();
        let reach_resolver = ReachabilityResolver::new();
        let relay_resolver = CommunityRelayResolver::new();

        let outcome =
            ingest_verified_row(&book, &reach_resolver, &relay_resolver, community, row, ts);
        assert_eq!(outcome, IngestOutcome::Applied(UpsertOutcome::Inserted));
        assert!(!relay_resolver
            .relays_for_community(&community, ts)
            .is_empty());
    }

    #[test]
    fn bad_signature_rejected_nothing_stored() {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x33; 32]);
        let vk = signing_key.verifying_key();
        let actor = OwnerAddr([0x03; 16]);
        let community = SpaceId([0xC2; 16]);
        let ts = 1_700_000_000_000u64;
        let at = hlc(ts);

        let mut payload = build_signed_payload_with_key(
            [0xCD; 32],
            "https://derp.example/".into(),
            vec![],
            ts,
            &actor,
            &at,
            Vec::new(),
            0,
            &signing_key,
        )
        .expect("build signed reachability payload");
        payload.identity_signature[0] ^= 0xFF; // corrupt

        let row = AddressBookRow {
            entry: AddressBookEntry::Reachability(payload),
            actor,
            device: vk.to_bytes(),
            at,
            stamped_at_ms: ts,
        };

        let book = CommunityAddressBook::new();
        let reach_resolver = ReachabilityResolver::new();
        let relay_resolver = CommunityRelayResolver::new();

        let outcome =
            ingest_verified_row(&book, &reach_resolver, &relay_resolver, community, row, ts);
        assert_eq!(outcome, IngestOutcome::BadSignature);
        assert!(book.rows_for_community(&community, ts).is_empty());
        assert!(reach_resolver.resolve(&actor).is_empty());
    }

    #[test]
    fn older_row_ignored_resolver_not_double_fed() {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x44; 32]);
        let vk = signing_key.verifying_key();
        let actor = OwnerAddr([0x04; 16]);
        let community = SpaceId([0xC3; 16]);
        let node_id = [0xEE; 32];

        let book = CommunityAddressBook::new();
        let reach_resolver = ReachabilityResolver::new();
        let relay_resolver = CommunityRelayResolver::new();

        let now_ms = 1_700_000_000_000u64;
        let at1 = hlc(now_ms);
        let payload1 = build_signed_payload_with_key(
            node_id,
            "https://derp.example/".into(),
            vec![],
            now_ms,
            &actor,
            &at1,
            Vec::new(),
            0,
            &signing_key,
        )
        .expect("build signed reachability payload (first)");
        let row1 = AddressBookRow {
            entry: AddressBookEntry::Reachability(payload1),
            actor,
            device: vk.to_bytes(),
            at: at1,
            stamped_at_ms: now_ms,
        };
        let outcome1 = ingest_verified_row(
            &book,
            &reach_resolver,
            &relay_resolver,
            community,
            row1,
            now_ms,
        );
        assert_eq!(outcome1, IngestOutcome::Applied(UpsertOutcome::Inserted));

        // Second ingest, same key (actor, node_id), OLDER stamp — must be
        // ignored by the store and never reach the resolver.
        let older_ts = now_ms - 5_000;
        let at2 = hlc(older_ts);
        let payload2 = build_signed_payload_with_key(
            node_id,
            "https://derp.example/".into(),
            vec![],
            older_ts,
            &actor,
            &at2,
            Vec::new(),
            0,
            &signing_key,
        )
        .expect("build signed reachability payload (second, older)");
        let row2 = AddressBookRow {
            entry: AddressBookEntry::Reachability(payload2),
            actor,
            device: vk.to_bytes(),
            at: at2,
            stamped_at_ms: older_ts,
        };
        let outcome2 = ingest_verified_row(
            &book,
            &reach_resolver,
            &relay_resolver,
            community,
            row2,
            now_ms,
        );
        assert_eq!(
            outcome2,
            IngestOutcome::Applied(UpsertOutcome::IgnoredOlder)
        );

        // The resolver must still show the FIRST (newer) announced_at_ms —
        // the ignored second row was never fanned out to it.
        let resolved = reach_resolver.resolve(&actor);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].announced_at_ms, now_ms);
    }
}
