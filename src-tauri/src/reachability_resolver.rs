//! ZEB-321 Phase 1: side-projection of ReachabilityAnnounce CRDT events.
//!
//! See `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` §5.4
//! (LWW projection) and §7.4 (resolver consumed by zenoh-over-iroh transport).

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use crate::owner_state_types::{Hlc, OwnerAddr};
use crate::reachability_record::ReachabilityAnnouncePayload;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolverEntry {
    pub payload: ReachabilityAnnouncePayload,
    pub hlc: Hlc,
}

#[derive(Default, Debug)]
pub struct ReachabilityResolver {
    inner: Arc<RwLock<BTreeMap<OwnerAddr, ResolverEntry>>>,
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
    pub fn update(&self, actor: OwnerAddr, payload: ReachabilityAnnouncePayload, hlc: Hlc) {
        let mut map = self.inner.write().expect("resolver write lock");
        let next = ResolverEntry { payload, hlc };
        match map.get(&actor) {
            Some(prev) if !should_replace(prev, &next) => { /* keep prev */ }
            _ => {
                map.insert(actor, next);
            }
        }
    }

    pub fn resolve(&self, actor: &OwnerAddr) -> Option<ReachabilityAnnouncePayload> {
        let map = self.inner.read().expect("resolver read lock");
        map.get(actor).map(|e| e.payload.clone())
    }

    pub fn list_active_peers(&self) -> Vec<(OwnerAddr, ReachabilityAnnouncePayload)> {
        let map = self.inner.read().expect("resolver read lock");
        map.iter().map(|(k, v)| (*k, v.payload.clone())).collect()
    }

    /// Reverse lookup: given an iroh `EndpointId` byte representation,
    /// find the matching `(OwnerAddr, payload)` entry.
    ///
    /// Used by [`crate::zenoh_iroh_transport::IrohZenohLinkManager`] (Task 6),
    /// where Zenoh hands us a locator carrying the iroh `EndpointId`
    /// (not the harmony `OwnerAddr`) — see spec §7.3. We scan
    /// `list_active_peers()` linearly: `N` is per-community member
    /// count, expected to stay under ~10³ in Phase 1. If profiling
    /// later shows this is hot, add a secondary BTreeMap keyed on
    /// `iroh_node_id` maintained alongside `inner`.
    // TODO Phase 2: secondary index keyed on `iroh_node_id` if profiling
    // shows the linear scan is hot.
    pub fn resolve_by_node_id(
        &self,
        node_id_bytes: &[u8; 32],
    ) -> Option<(OwnerAddr, ReachabilityAnnouncePayload)> {
        let map = self.inner.read().expect("resolver read lock");
        map.iter()
            .find(|(_, entry)| &entry.payload.iroh_node_id == node_id_bytes)
            .map(|(k, v)| (*k, v.payload.clone()))
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

    #[test]
    fn lww_higher_hlc_wins() {
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        r.update(actor, make_payload(1, 1000), make_hlc(1000, 0, "a"));
        r.update(actor, make_payload(2, 2000), make_hlc(2000, 0, "a"));
        assert_eq!(r.resolve(&actor).unwrap().iroh_node_id, [2; 32]);
    }

    #[test]
    fn lww_lower_hlc_ignored() {
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        r.update(actor, make_payload(2, 2000), make_hlc(2000, 0, "a"));
        r.update(actor, make_payload(1, 1000), make_hlc(1000, 0, "a"));
        assert_eq!(r.resolve(&actor).unwrap().iroh_node_id, [2; 32]);
    }

    #[test]
    fn determinism_across_orders() {
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
                make_payload(2, 2000),
                make_hlc(2000, 0, "a"),
            ),
            (
                OwnerAddr([0x22; 16]),
                make_payload(4, 2500),
                make_hlc(2500, 0, "b"),
            ),
        ];

        let mut orders = vec![
            vec![0, 1, 2, 3],
            vec![3, 2, 1, 0],
            vec![1, 3, 0, 2],
            vec![2, 0, 3, 1],
        ];

        let mut final_states: Vec<Vec<(OwnerAddr, [u8; 32])>> = Vec::new();
        for order in orders.drain(..) {
            let r = ReachabilityResolver::new();
            for i in order {
                let (a, p, h) = &events[i];
                r.update(*a, p.clone(), h.clone());
            }
            let mut s: Vec<(OwnerAddr, [u8; 32])> = r
                .list_active_peers()
                .into_iter()
                .map(|(a, p)| (a, p.iroh_node_id))
                .collect();
            s.sort_by_key(|(a, _)| *a);
            final_states.push(s);
        }

        for w in final_states.windows(2) {
            assert_eq!(w[0], w[1], "ReachabilityResolver is not order-independent");
        }
    }

    #[test]
    fn lww_announced_at_ms_breaks_hlc_tie() {
        // Spec §5.4 tie-break #1: equal HLC, higher announced_at_ms wins.
        // (HLC equality "shouldn't happen with monotonic clocks" but the
        // path is spec-mandated and must be deterministic.)
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        let hlc = make_hlc(1000, 0, "a");
        r.update(actor, make_payload(1, 1500), hlc.clone());
        // Second update has same HLC + higher announced_at_ms → wins.
        r.update(actor, make_payload(2, 2500), hlc.clone());
        assert_eq!(r.resolve(&actor).unwrap().iroh_node_id, [2; 32]);

        // Reverse order: lower announced_at_ms must NOT overwrite higher.
        let r2 = ReachabilityResolver::new();
        r2.update(actor, make_payload(2, 2500), hlc.clone());
        r2.update(actor, make_payload(1, 1500), hlc);
        assert_eq!(r2.resolve(&actor).unwrap().iroh_node_id, [2; 32]);
    }

    #[test]
    fn lww_iroh_node_id_breaks_announced_at_ms_tie() {
        // Spec §5.4 tie-break #2: equal HLC + equal announced_at_ms,
        // lex-greater iroh_node_id wins. Final defense — guarantees a
        // deterministic resolution even when the first two columns
        // collide.
        let r = ReachabilityResolver::new();
        let actor = OwnerAddr([0x11; 16]);
        let hlc = make_hlc(1000, 0, "a");
        // Lower node_id ([0x01; 32]) seeded first.
        r.update(actor, make_payload(0x01, 2000), hlc.clone());
        // Higher node_id ([0x02; 32]) arrives — wins on lex comparison.
        r.update(actor, make_payload(0x02, 2000), hlc.clone());
        assert_eq!(r.resolve(&actor).unwrap().iroh_node_id, [0x02; 32]);

        // Reverse order: lower lex must NOT overwrite higher.
        let r2 = ReachabilityResolver::new();
        r2.update(actor, make_payload(0x02, 2000), hlc.clone());
        r2.update(actor, make_payload(0x01, 2000), hlc);
        assert_eq!(r2.resolve(&actor).unwrap().iroh_node_id, [0x02; 32]);
    }
}
