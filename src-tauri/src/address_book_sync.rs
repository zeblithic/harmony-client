//! ZEB-815 Task 3: sealed wire codec for the community address book.
//!
//! One codec serves both a live single-row publish and a full snapshot: a
//! live record publish is [`seal_records`] with a 1-element slice; a snapshot
//! is the same codec over the full row set. Mirrors the presence beacon seal
//! (`community_presence.rs`'s `seal_presence_beacon`/`open_presence_beacon`)
//! over `encrypt_voice_packet`/`decrypt_voice_packet`, with its own HKDF
//! `info` label, sentinel channel, and AAD domain so an address-book packet
//! can never be confused with (or opened as) a presence beacon.

use crate::community_address_book::AddressBookRow;
use crate::community_channel_log::ChannelKey;
use crate::community_membership::ChannelId;
use crate::owner_state_types::{EpochKey, SpaceId};
use crate::voice_crypto::{decrypt_voice_packet, encrypt_voice_packet};
use hkdf::Hkdf;
use sha2::Sha256;

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
}
