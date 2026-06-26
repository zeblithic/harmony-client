//! Open-community cross-WAN first-contact: enumerated DHT rendezvous slots.
//!
//! A community-keyed rendezvous record lets a link-only joiner (holding the
//! community `epoch_key`) resolve a live serving "beacon" from the DHT. Slot
//! keys reuse `PkarrCase::Community` (its `ikm` contract IS the epoch key) with
//! a distinct, length-disjoint info-prefix so they can never collide with the
//! member-keyed records (`info = identity_pub(64) || epoch_id(8)`, 72 bytes).

use crate::community_relay_announce::COMMUNITY_RELAY_ADVERTISERS_MAX;
use crate::owner_state_types::EpochKey;
use crate::owner_state_types::OwnerAddr;
use harmony_pkarr::derive::{derive_ephemeral_key, PkarrCase};

/// Number of enumerated rendezvous slots == the relay-advertiser cap.
pub const RENDEZVOUS_SLOT_COUNT: usize = COMMUNITY_RELAY_ADVERTISERS_MAX;

/// Domain-separation prefix for rendezvous slot derivation. Length-disjoint
/// from the 72-byte member-keyed `info`, so rendezvous keys can never alias a
/// member's reachability key even though both reuse the `Community` salt.
pub const RENDEZVOUS_INFO_PREFIX: &[u8] = b"harmony.rendezvous.v1";

/// `info = RENDEZVOUS_INFO_PREFIX || slot_index_be(2) || epoch_id_be(8)`.
fn rendezvous_info(slot_index: u16, epoch_id: u64) -> Vec<u8> {
    let mut info = Vec::with_capacity(RENDEZVOUS_INFO_PREFIX.len() + 2 + 8);
    info.extend_from_slice(RENDEZVOUS_INFO_PREFIX);
    info.extend_from_slice(&slot_index.to_be_bytes());
    info.extend_from_slice(&epoch_id.to_be_bytes());
    info
}

pub fn rendezvous_slot_key(
    epoch_key: &EpochKey,
    slot_index: u16,
    epoch_id: u64,
) -> ed25519_dalek::SigningKey {
    let info = rendezvous_info(slot_index, epoch_id);
    derive_ephemeral_key(PkarrCase::Community, epoch_key.as_bytes(), &info)
}

pub fn rendezvous_slot_verifying_key(
    epoch_key: &EpochKey,
    slot_index: u16,
    epoch_id: u64,
) -> ed25519_dalek::VerifyingKey {
    rendezvous_slot_key(epoch_key, slot_index, epoch_id).verifying_key()
}

/// Deterministic slot claim: sort the advertiser set ascending by address; the
/// position of `me` is its slot index. `None` if `me` is absent or ranks at/
/// beyond the slot cap. Because the advertiser set is CRDT-replicated, every
/// member computes the same ordering, so each slot has exactly one writer.
pub fn slot_for_advertiser(advertisers: &[OwnerAddr], me: &OwnerAddr) -> Option<u16> {
    let mut sorted: Vec<OwnerAddr> = advertisers.to_vec();
    sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    sorted.dedup_by(|a, b| a.0 == b.0);
    let rank = sorted.iter().position(|a| a.0 == me.0)?;
    if rank >= RENDEZVOUS_SLOT_COUNT {
        return None;
    }
    Some(rank as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ek() -> EpochKey {
        EpochKey::new([7u8; 32])
    }

    #[test]
    fn slot_key_is_deterministic() {
        let a = rendezvous_slot_verifying_key(&ek(), 0, 42);
        let b = rendezvous_slot_verifying_key(&ek(), 0, 42);
        assert_eq!(a.to_bytes(), b.to_bytes());
    }

    #[test]
    fn distinct_slots_and_epochs_give_distinct_keys() {
        let s0 = rendezvous_slot_verifying_key(&ek(), 0, 42).to_bytes();
        let s1 = rendezvous_slot_verifying_key(&ek(), 1, 42).to_bytes();
        let s0_next = rendezvous_slot_verifying_key(&ek(), 0, 43).to_bytes();
        assert_ne!(s0, s1, "different slot index must yield a different key");
        assert_ne!(s0, s0_next, "different epoch must yield a different key");
    }

    #[test]
    fn rendezvous_key_is_disjoint_from_member_keyed_record() {
        // Member-keyed info is identity_pub(64) || epoch_id(8) = 72 bytes under
        // the SAME Community salt. Reconstruct it and confirm no rendezvous slot
        // collides with it.
        let epoch_id = 42u64;
        let identity_pub = [9u8; 64];
        let mut member_info = Vec::with_capacity(72);
        member_info.extend_from_slice(&identity_pub);
        member_info.extend_from_slice(&epoch_id.to_be_bytes());
        let member_key = derive_ephemeral_key(PkarrCase::Community, ek().as_bytes(), &member_info)
            .verifying_key()
            .to_bytes();
        for slot in 0..RENDEZVOUS_SLOT_COUNT as u16 {
            let rk = rendezvous_slot_verifying_key(&ek(), slot, epoch_id).to_bytes();
            assert_ne!(rk, member_key, "slot {slot} aliased a member key");
        }
    }

    #[test]
    fn slot_count_tracks_advertiser_cap() {
        assert_eq!(RENDEZVOUS_SLOT_COUNT, COMMUNITY_RELAY_ADVERTISERS_MAX);
    }

    fn addr(b: u8) -> OwnerAddr {
        OwnerAddr([b; 16])
    }

    #[test]
    fn slot_assignment_is_deterministic_across_members() {
        // Two members compute the SAME ordering from the same (unordered) set.
        let set_a = vec![addr(3), addr(1), addr(2)];
        let set_b = vec![addr(2), addr(3), addr(1)];
        for who in [addr(1), addr(2), addr(3)] {
            assert_eq!(
                slot_for_advertiser(&set_a, &who),
                slot_for_advertiser(&set_b, &who),
                "ordering disagreed for {who:?}"
            );
        }
        // Sorted ascending: addr(1)->0, addr(2)->1, addr(3)->2.
        assert_eq!(slot_for_advertiser(&set_a, &addr(1)), Some(0));
        assert_eq!(slot_for_advertiser(&set_a, &addr(2)), Some(1));
        assert_eq!(slot_for_advertiser(&set_a, &addr(3)), Some(2));
    }

    #[test]
    fn not_in_set_returns_none() {
        let set = vec![addr(1), addr(2)];
        assert_eq!(slot_for_advertiser(&set, &addr(9)), None);
    }

    #[test]
    fn rank_beyond_cap_returns_none() {
        // RENDEZVOUS_SLOT_COUNT (=4) advertisers fill slots 0..3; a 5th (highest
        // address) ranks 4 >= cap and claims no slot.
        let set: Vec<OwnerAddr> = (1..=(RENDEZVOUS_SLOT_COUNT as u8 + 1)).map(addr).collect();
        let highest = addr(RENDEZVOUS_SLOT_COUNT as u8 + 1);
        assert_eq!(slot_for_advertiser(&set, &highest), None);
        // The one just under the cap still gets the last valid slot.
        let last_valid = addr(RENDEZVOUS_SLOT_COUNT as u8);
        assert_eq!(
            slot_for_advertiser(&set, &last_valid),
            Some((RENDEZVOUS_SLOT_COUNT - 1) as u16)
        );
    }

    #[test]
    fn duplicate_addresses_do_not_shift_ranks() {
        // Defensive: a duplicated advertiser must not change anyone's slot.
        let set = vec![addr(1), addr(2), addr(2), addr(3)];
        assert_eq!(slot_for_advertiser(&set, &addr(1)), Some(0));
        assert_eq!(slot_for_advertiser(&set, &addr(2)), Some(1));
        assert_eq!(slot_for_advertiser(&set, &addr(3)), Some(2));
    }
}
