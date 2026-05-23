//! ZEB-321 Phase 1: side-projection of ReachabilityAnnounce CRDT events.
//!
//! See `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` §5.4
//! (LWW projection) and §7.4 (resolver consumed by zenoh-over-iroh transport).
//!
//! ## Multi-device keying (PR #157 round 5)
//!
//! Each harmony owner identity (ZEB-173) may be bound to multiple devices,
//! each with its own iroh `EndpointId`. The resolver is therefore keyed
//! by the pair `(OwnerAddr, iroh_node_id)`, not by `OwnerAddr` alone —
//! otherwise device B's announce would overwrite device A's, and only
//! the most-recently-announced device would be dialable. The LWW
//! comparator runs per-key, so each (owner, device) pair maintains its
//! own latest record independently.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use crate::owner_state_types::{Hlc, OwnerAddr};
use crate::reachability_record::ReachabilityAnnouncePayload;

/// Composite key: harmony owner + iroh endpoint. Same-owner-different-
/// device entries coexist; same-owner-same-device updates are LWW.
type ResolverKey = (OwnerAddr, [u8; 32]);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolverEntry {
    pub payload: ReachabilityAnnouncePayload,
    pub hlc: Hlc,
}

#[derive(Default, Debug)]
pub struct ReachabilityResolver {
    inner: Arc<RwLock<BTreeMap<ResolverKey, ResolverEntry>>>,
}

impl Clone for ReachabilityResolver {
    fn clone(&self) -> Self {
        ReachabilityResolver {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl ReachabilityResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// LWW update — higher HLC wins; ties broken by announced_at_ms then
    /// lexicographic iroh_node_id. See spec §5.4.
    ///
    /// Per PR #157 round 5: keyed by `(actor, payload.iroh_node_id)` so
    /// same-owner-different-device announces don't overwrite each other.
    pub fn update(&self, actor: OwnerAddr, payload: ReachabilityAnnouncePayload, hlc: Hlc) {
        let key: ResolverKey = (actor, payload.iroh_node_id);
        let mut map = self.inner.write().expect("resolver write lock");
        let next = ResolverEntry { payload, hlc };
        match map.get(&key) {
            Some(prev) if !should_replace(prev, &next) => { /* keep prev */ }
            _ => {
                map.insert(key, next);
            }
        }
    }

    /// Returns ALL device records for `actor`. Multi-device owners
    /// (per ZEB-173) may have multiple entries; callers that need to
    /// dial the owner should try each in turn (Phase 2 will add ranking
    /// based on heartbeat/liveness).
    pub fn resolve(&self, actor: &OwnerAddr) -> Vec<ReachabilityAnnouncePayload> {
        let map = self.inner.read().expect("resolver read lock");
        map.range((*actor, [0u8; 32])..=(*actor, [0xFFu8; 32]))
            .map(|(_, v)| v.payload.clone())
            .collect()
    }

    pub fn list_active_peers(&self) -> Vec<(OwnerAddr, ReachabilityAnnouncePayload)> {
        let map = self.inner.read().expect("resolver read lock");
        map.iter()
            .map(|((owner, _node_id), v)| (*owner, v.payload.clone()))
            .collect()
    }

    /// Reverse lookup: given an iroh `EndpointId` byte representation,
    /// find the matching `(OwnerAddr, payload)` entry.
    ///
    /// Used by [`crate::zenoh_iroh_transport::IrohZenohLinkManager`] (Task 6),
    /// where Zenoh hands us a locator carrying the iroh `EndpointId`
    /// (not the harmony `OwnerAddr`) — see spec §7.3.
    ///
    /// Per PR #157 round 5: the composite-key map could enable an O(log N)
    /// secondary-index lookup, but we keep the linear scan for now since
    /// `N` (joined community member count × device count) is expected to
    /// stay under ~10⁴ in Phase 1. Phase 2 should profile and add a
    /// `BTreeMap<[u8; 32], OwnerAddr>` secondary index if hot.
    pub fn resolve_by_node_id(
        &self,
        node_id_bytes: &[u8; 32],
    ) -> Option<(OwnerAddr, ReachabilityAnnouncePayload)> {
        let map = self.inner.read().expect("resolver read lock");
        map.iter()
            .find(|((_, key_node_id), _)| key_node_id == node_id_bytes)
            .map(|((owner, _), v)| (*owner, v.payload.clone()))
    }

    /// Evict every device record for `actor`. Called from the membership-
    /// delta consumer on Leave / Kick events (PR #157 round 5 Cursor
    /// finding) so departed members don't linger in
    /// `connectivity_list_peer_reachability` or `resolve_by_node_id`
    /// until restart bootstrap re-filters them.
    ///
    /// Returns the number of entries removed (across all the owner's
    /// devices) so callers can log / decide whether to emit a UI hint.
    pub fn remove_owner(&self, actor: &OwnerAddr) -> usize {
        let mut map = self.inner.write().expect("resolver write lock");
        let to_remove: Vec<ResolverKey> = map
            .range((*actor, [0u8; 32])..=(*actor, [0xFFu8; 32]))
            .map(|(k, _)| *k)
            .collect();
        let n = to_remove.len();
        for k in to_remove {
            map.remove(&k);
        }
        n
    }
}

/// LWW comparator. `Hlc` does not derive `Ord` (canonical-CBOR keying
/// constraints — see `owner_state_types.rs::Hlc`), so we compare by the
/// same lexicographic tuple `(wall_ms, logical, device_id)` used by
/// `Hlc::is_strictly_newer_than`. Ties on HLC fall through to
/// `announced_at_ms` then lex `iroh_node_id`, per spec §5.4.
fn should_replace(prev: &ResolverEntry, next: &ResolverEntry) -> bool {
    let prev_key = (
        prev.hlc.wall_ms,
        prev.hlc.logical,
        prev.hlc.device_id.as_str(),
    );
    let next_key = (
        next.hlc.wall_ms,
        next.hlc.logical,
        next.hlc.device_id.as_str(),
    );
    match next_key.cmp(&prev_key) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => match next
            .payload
            .announced_at_ms
            .cmp(&prev.payload.announced_at_ms)
        {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => next.payload.iroh_node_id > prev.payload.iroh_node_id,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_payload(node_id_byte: u8, announced_at_ms: u64) -> ReachabilityAnnouncePayload {
        ReachabilityAnnouncePayload {
            iroh_node_id: [node_id_byte; 32],
            home_relay_url: "https://derp.example/".into(),
            direct_addresses: vec![],
            announced_at_ms,
            identity_signature: [0; 64],
        }
    }

    fn make_hlc(wall_ms: u64, logical: u32, device: &str) -> Hlc {
        Hlc {
            wall_ms,
            logical,
            device_id: device.into(),
        }
    }

    /// Same owner, SAME device (same iroh_node_id) — later HLC wins per LWW.
    #[test]
    fn lww_higher_hlc_wins_per_device() {
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        // Both announces from the SAME device (node_id_byte = 1) — second
        // has higher HLC and should overwrite.
        r.update(actor, make_payload(1, 1000), make_hlc(1000, 0, "a"));
        r.update(actor, make_payload(1, 2000), make_hlc(2000, 0, "a"));
        let records = r.resolve(&actor);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].announced_at_ms, 2000);
    }

    #[test]
    fn lww_lower_hlc_ignored_per_device() {
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        r.update(actor, make_payload(1, 2000), make_hlc(2000, 0, "a"));
        r.update(actor, make_payload(1, 1000), make_hlc(1000, 0, "a"));
        let records = r.resolve(&actor);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].announced_at_ms, 2000);
    }

    /// Same owner, DIFFERENT devices — both records coexist. This is the
    /// multi-device case from PR #157 round 5 (Cursor): per ZEB-173 a
    /// harmony identity binds to multiple devices, each with its own
    /// iroh `EndpointId`. The resolver must keep all of them so any
    /// of the owner's devices is dialable.
    #[test]
    fn multi_device_same_owner_coexist() {
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        let device_a_node: u8 = 0x01;
        let device_b_node: u8 = 0x02;
        r.update(
            actor,
            make_payload(device_a_node, 1000),
            make_hlc(1000, 0, "a"),
        );
        r.update(
            actor,
            make_payload(device_b_node, 1500),
            make_hlc(1500, 0, "b"),
        );

        let records = r.resolve(&actor);
        assert_eq!(records.len(), 2, "both devices' records must survive");

        let node_ids: Vec<[u8; 32]> = records.iter().map(|p| p.iroh_node_id).collect();
        assert!(node_ids.contains(&[device_a_node; 32]));
        assert!(node_ids.contains(&[device_b_node; 32]));

        // Each device's payload preserved with its own HLC slot.
        for r_p in &records {
            if r_p.iroh_node_id == [device_a_node; 32] {
                assert_eq!(r_p.announced_at_ms, 1000);
            } else {
                assert_eq!(r_p.announced_at_ms, 1500);
            }
        }

        // list_active_peers reports one row per (owner, device).
        let active = r.list_active_peers();
        assert_eq!(active.len(), 2);
        assert!(active.iter().all(|(o, _)| *o == actor));
    }

    #[test]
    fn determinism_across_orders() {
        // Same intent as the original: applying the same set of
        // ReachabilityAnnounce events in arbitrary order converges to
        // the same map. Under composite keying, each (owner, device)
        // pair is its own slot, so the convergence guarantee is even
        // stronger — distinct events never overwrite each other.
        let events: Vec<(OwnerAddr, ReachabilityAnnouncePayload, Hlc)> = vec![
            (
                OwnerAddr([0x11; 16]),
                make_payload(1, 1000),
                make_hlc(1000, 0, "a"),
            ),
            (
                OwnerAddr([0x22; 16]),
                make_payload(3, 1500),
                make_hlc(1500, 0, "b"),
            ),
            (
                OwnerAddr([0x11; 16]),
                make_payload(1, 2000),
                make_hlc(2000, 0, "a"),
            ),
            (
                OwnerAddr([0x22; 16]),
                make_payload(3, 2500),
                make_hlc(2500, 0, "b"),
            ),
        ];

        let mut orders = vec![
            vec![0, 1, 2, 3],
            vec![3, 2, 1, 0],
            vec![1, 3, 0, 2],
            vec![2, 0, 3, 1],
        ];

        let mut final_states: Vec<Vec<(OwnerAddr, [u8; 32], u64)>> = Vec::new();
        for order in orders.drain(..) {
            let r = ReachabilityResolver::new();
            for i in order {
                let (a, p, h) = &events[i];
                r.update(*a, p.clone(), h.clone());
            }
            let mut s: Vec<(OwnerAddr, [u8; 32], u64)> = r
                .list_active_peers()
                .into_iter()
                .map(|(a, p)| (a, p.iroh_node_id, p.announced_at_ms))
                .collect();
            s.sort();
            final_states.push(s);
        }

        for w in final_states.windows(2) {
            assert_eq!(w[0], w[1], "ReachabilityResolver is not order-independent");
        }
    }

    #[test]
    fn lww_announced_at_ms_breaks_hlc_tie() {
        // Spec §5.4 tie-break #1: equal HLC, higher announced_at_ms wins.
        // Same-device case (same iroh_node_id) so both updates target the
        // same composite-key slot.
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        let hlc = make_hlc(1000, 0, "a");
        r.update(actor, make_payload(1, 1500), hlc.clone());
        r.update(actor, make_payload(1, 2500), hlc.clone());
        let records = r.resolve(&actor);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].announced_at_ms, 2500);

        // Reverse order: lower announced_at_ms must NOT overwrite higher.
        let r2 = ReachabilityResolver::new();
        r2.update(actor, make_payload(1, 2500), hlc.clone());
        r2.update(actor, make_payload(1, 1500), hlc);
        let records2 = r2.resolve(&actor);
        assert_eq!(records2.len(), 1);
        assert_eq!(records2[0].announced_at_ms, 2500);
    }

    #[test]
    fn lww_iroh_node_id_tie_broken_in_payload_collision() {
        // Spec §5.4 tie-break #2 used to mean "same HLC + same
        // announced_at_ms, lex-greater iroh_node_id wins". Under composite
        // keying, two payloads with DIFFERENT iroh_node_ids land in
        // different slots and both survive. This tie-break only fires
        // when the SAME composite key sees two competing updates whose
        // HLC and announced_at_ms both collide AND their iroh_node_id
        // matches — which only happens for same-device replays. We
        // assert that the resolver remains deterministic in that case
        // (no panic, last-write semantics by HLC tie).
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        let hlc = make_hlc(1000, 0, "a");
        // Same device, same announced_at_ms, same HLC — second `update`
        // is a no-op (should_replace returns false on full equality).
        r.update(actor, make_payload(0x01, 2000), hlc.clone());
        r.update(actor, make_payload(0x01, 2000), hlc);
        let records = r.resolve(&actor);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].iroh_node_id, [0x01; 32]);
    }

    #[test]
    fn resolve_by_node_id_finds_inserted_entry() {
        // CodeRabbit PR #157 round 1: positive lookup. The Phase 1
        // transport reaches peers exclusively via node_id (the locator
        // form `iroh/<hex-32>`), so resolve_by_node_id is on the hot
        // path of every outbound `new_link` call.
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x55; 16]);
        let payload = make_payload(0xAB, 1500);
        r.update(actor, payload.clone(), make_hlc(1500, 0, "a"));
        let hit = r.resolve_by_node_id(&[0xAB; 32]);
        assert!(hit.is_some());
        let (got_addr, got_payload) = hit.unwrap();
        assert_eq!(got_addr, actor);
        assert_eq!(got_payload.iroh_node_id, [0xAB; 32]);
        assert_eq!(got_payload.announced_at_ms, payload.announced_at_ms);
    }

    #[test]
    fn resolve_by_node_id_returns_none_for_unknown() {
        // CodeRabbit PR #157 round 1: negative lookup.
        let r = ReachabilityResolver::new();
        r.update(
            OwnerAddr([0x11; 16]),
            make_payload(0xAB, 1500),
            make_hlc(1500, 0, "a"),
        );
        assert!(r.resolve_by_node_id(&[0xCD; 32]).is_none());
    }

    /// PR #157 round 5 (Cursor): Leave/Kick events should drop the
    /// departed member's records from the resolver. `remove_owner`
    /// removes ALL device entries for the owner in a single call.
    #[test]
    fn remove_owner_evicts_all_devices() {
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        let other = OwnerAddr([0x22; 16]);
        // Three devices for `actor` + one for `other` (the latter must
        // NOT be evicted).
        r.update(actor, make_payload(0x01, 1000), make_hlc(1000, 0, "a"));
        r.update(actor, make_payload(0x02, 1000), make_hlc(1000, 0, "b"));
        r.update(actor, make_payload(0x03, 1000), make_hlc(1000, 0, "c"));
        r.update(other, make_payload(0xCC, 1000), make_hlc(1000, 0, "d"));

        let removed = r.remove_owner(&actor);
        assert_eq!(removed, 3, "all three of actor's devices evicted");

        assert!(r.resolve(&actor).is_empty());
        assert_eq!(r.resolve(&other).len(), 1, "other owner untouched");
        // Reverse lookups for the evicted device-ids must now miss.
        assert!(r.resolve_by_node_id(&[0x01; 32]).is_none());
        assert!(r.resolve_by_node_id(&[0xCC; 32]).is_some());
    }

    #[test]
    fn remove_owner_is_idempotent() {
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        // No entries → returns 0, doesn't panic.
        assert_eq!(r.remove_owner(&actor), 0);

        r.update(actor, make_payload(0x01, 1000), make_hlc(1000, 0, "a"));
        assert_eq!(r.remove_owner(&actor), 1);
        // Second remove is a no-op — also 0.
        assert_eq!(r.remove_owner(&actor), 0);
    }
}
