//! ZEB-580 S2: a synchronous projection answering "is owner X's device D
//! revoked?" for the DM receive-path cutoff. A pure derivation of the community
//! materialized view (`MemberState.revoked_device_keys`), aggregated by owner
//! across every community this node is joined in. Sticky/monotonic within a
//! session (spec §5.1): a key present in any joined community's revoked set
//! stays revoked; leaving a community does not retract. Sibling in spirit to
//! `network_health::MembershipProjection`, but a standalone leaf (depends only
//! on `owner_state_types`) because its consumer is the DM receive path, not the
//! network-health panel — co-locating in `network_health` would force the only
//! `dm_* -> network_health` import edge (a layering inversion).
//!
//! ## Grow-only is a correctness property, not a leak (ZEB-1009)
//!
//! The projection is deliberately unbounded and never compacted. Evicting a
//! key — by LRU, TTL, per-owner cap, anything — would **un-revoke** a device
//! at the DM receive-path check site: `is_revoked` would return `false` for a
//! key the community state still condemns, re-admitting a revoked device's
//! traffic. That is the same reasoning that exempts tombstones from
//! sync-ingest validation — monotone deny-state must always apply.
//!
//! The memory bound comes from what feeds it, not from a policy here:
//!
//! * **Rebuilt each boot.** This is an in-memory pure derivation, never
//!   persisted. Boot re-derives it from the current community
//!   materializations + the DM-CRDT revocation replay, so the only growth
//!   beyond the CURRENT feed is intra-session leave-stickiness (keys retained
//!   after leaving the community that carried them), and that surplus resets
//!   at restart.
//! * **Bounded by resident state.** Every key here is cloned from
//!   `MemberState.revoked_device_keys` rows the node already holds in its
//!   materialized views — the projection is at most a constant factor over
//!   revoked-key data already resident, and introduces no new asymptotic
//!   growth.
//! * **Absolute sizing.** Entries are 32-byte keys under per-owner
//!   `BTreeSet`s (order ~100 B/key with tree overhead). Revocations are rare,
//!   per-owner lifetime events (device loss / retirement / compromise), not a
//!   rate: even a node sharing communities with 10⁶ distinct owners at a 10%
//!   lifetime-revocation incidence and ~2 keys each is ~2·10⁵ keys ≈ tens of
//!   MB worst-case, with O(log n) lookups. Typical fleets are orders of
//!   magnitude below that.

use crate::owner_state_types::OwnerAddr;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

#[derive(Clone, Debug, Default)]
pub struct RevokedDeviceProjection {
    // owner -> revoked #2 ed25519 verify keys. std RwLock (NOT tokio): the read
    // sits on the DM receive path inside the owner-state critical section and
    // must be synchronous / never held across an .await.
    by_owner: Arc<RwLock<BTreeMap<OwnerAddr, BTreeSet<[u8; 32]>>>>,
}

impl RevokedDeviceProjection {
    pub fn new() -> Self {
        Self::default()
    }

    /// Union each `(owner, revoked_keys)` into the projection. Sticky: existing
    /// keys are never removed, so an owner absent from `members` (node left the
    /// carrying community) or present with an empty set retains prior tombstones.
    /// Grow-only by design — see the module docs (ZEB-1009) for why eviction
    /// would be a correctness bug and why the size stays bounded anyway.
    pub fn union_from_members<'a, I>(&self, members: I)
    where
        I: IntoIterator<Item = (OwnerAddr, &'a BTreeSet<[u8; 32]>)>,
    {
        let mut guard = self.by_owner.write().unwrap_or_else(|e| e.into_inner());
        for (owner, keys) in members {
            if keys.is_empty() {
                continue;
            }
            guard.entry(owner).or_default().extend(keys.iter().copied());
        }
    }

    /// True iff `ed25519` is a revoked #2 key for `owner`. Synchronous.
    pub fn is_revoked(&self, owner: &OwnerAddr, ed25519: &[u8; 32]) -> bool {
        let guard = self.by_owner.read().unwrap_or_else(|e| e.into_inner());
        guard.get(owner).is_some_and(|s| s.contains(ed25519))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::OwnerAddr;
    use std::collections::BTreeSet;

    fn set(keys: &[[u8; 32]]) -> BTreeSet<[u8; 32]> {
        keys.iter().copied().collect()
    }

    #[test]
    fn union_and_is_revoked_roundtrip() {
        let p = RevokedDeviceProjection::new();
        let owner = OwnerAddr([0x11; 16]);
        let k = [0xaa; 32];
        assert!(
            !p.is_revoked(&owner, &k),
            "empty projection revokes nothing"
        );
        let s = set(&[k]);
        p.union_from_members(std::iter::once((owner, &s)));
        assert!(p.is_revoked(&owner, &k));
        assert!(
            !p.is_revoked(&owner, &[0xbb; 32]),
            "unrelated key not revoked"
        );
        assert!(
            !p.is_revoked(&OwnerAddr([0x22; 16]), &k),
            "revocation is per-owner"
        );
    }

    #[test]
    fn union_is_sticky_across_a_simulated_community_leave() {
        // A later materialize that omits the owner entirely (node left the
        // community that carried the revocation) must NOT un-revoke.
        let p = RevokedDeviceProjection::new();
        let owner = OwnerAddr([0x11; 16]);
        let k = [0xaa; 32];
        let s = set(&[k]);
        p.union_from_members(std::iter::once((owner, &s)));
        assert!(p.is_revoked(&owner, &k));
        // Next feed round carries no members at all (left every shared community).
        p.union_from_members(std::iter::empty());
        assert!(p.is_revoked(&owner, &k), "sticky: leave does not retract");
        // Next feed round carries the owner with an EMPTY revoked set.
        let empty = BTreeSet::new();
        p.union_from_members(std::iter::once((owner, &empty)));
        assert!(
            p.is_revoked(&owner, &k),
            "sticky: empty set does not retract"
        );
    }

    #[test]
    fn union_accumulates_across_owners_and_communities() {
        let p = RevokedDeviceProjection::new();
        let (o1, o2) = (OwnerAddr([1; 16]), OwnerAddr([2; 16]));
        let (k1, k2) = ([0x01; 32], [0x02; 32]);
        p.union_from_members(std::iter::once((o1, &set(&[k1]))));
        p.union_from_members(std::iter::once((o2, &set(&[k2]))));
        // A second community adds another key for o1.
        p.union_from_members(std::iter::once((o1, &set(&[[0x03; 32]]))));
        assert!(p.is_revoked(&o1, &k1));
        assert!(p.is_revoked(&o1, &[0x03; 32]));
        assert!(p.is_revoked(&o2, &k2));
    }

    #[test]
    fn clone_shares_state() {
        let p = RevokedDeviceProjection::new();
        let handle = p.clone();
        let owner = OwnerAddr([0x11; 16]);
        let k = [0xaa; 32];
        p.union_from_members(std::iter::once((owner, &set(&[k]))));
        assert!(
            handle.is_revoked(&owner, &k),
            "clone sees writes via shared Arc"
        );
    }
}
