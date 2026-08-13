//! Node-id admission classifier for R4 bounded-degree dialing (ZEB-928).
//!
//! Read on the hot path at the supervisor's two dial-arming sites (`kick` and the internal
//! `do_sweep` re-arm). Peer mode — or an unwired oracle — admits everything, so behavior is
//! byte-for-byte identical to pre-R4. Router mode admits a node_id iff a device key bound to
//! it is in the admitted set; an unknown node_id fails open.
//!
//! Three writers, one reader:
//! - the event-loop **controller** replaces the admitted *device-key* set on membership deltas
//!   ([`AdmissionOracle::publish_admitted`], fed by [`compute_admitted`]);
//! - the **resolver** binds `node_id -> device_key` as records are verified/ingested
//!   ([`AdmissionOracle::bind`] / [`AdmissionOracle::unbind_node_ids`]);
//! - the **supervisor** reads [`AdmissionOracle::admit`] at each dial-arming site.
//!
//! See `docs/superpowers/specs/2026-08-13-zeb928-r4-wiring-design.md`.

use crate::community_topology::community_neighbors;
use std::collections::{BTreeSet, HashMap};
use std::sync::RwLock;

/// Shared admission classifier. Cheap to read; the hot path takes read locks only.
pub struct AdmissionOracle {
    /// Router-mode gate, fixed at construction. Disabled → [`AdmissionOracle::admit`] is always
    /// true (no filtering, no behavior change from pre-R4).
    enabled: bool,
    /// Admitted enrolled device keys (the realized ring-neighbor union). Controller writes.
    admitted: RwLock<BTreeSet<[u8; 32]>>,
    /// Reverse bridge `iroh_node_id -> enrolled device key(s)`. A node_id can be asserted by more
    /// than one enrolled key (delegate/butler devices, multi-owner), so the value is a set and
    /// admission is set-intersection. Resolver writes.
    node_to_devices: RwLock<HashMap<[u8; 32], BTreeSet<[u8; 32]>>>,
}

impl AdmissionOracle {
    /// `enabled` = router mode. A disabled oracle admits everything.
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            admitted: RwLock::new(BTreeSet::new()),
            node_to_devices: RwLock::new(HashMap::new()),
        }
    }

    /// Whether the filter is active (router mode). The controller skips all polling when false.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Hot path. `true` = allow/keep this dial target. Peer mode → always true. Router mode →
    /// admit iff a device key bound to `node_id` is in the admitted set; **fail-open** on an
    /// unknown node_id (no binding: infrastructure or non-community peers stay dialable).
    pub fn admit(&self, node_id: &[u8; 32]) -> bool {
        if !self.enabled {
            return true;
        }
        let map = self.node_to_devices.read().expect("node_to_devices poisoned");
        let devices = match map.get(node_id) {
            Some(d) => d,
            None => return true, // fail-open on unknown identity
        };
        let admitted = self.admitted.read().expect("admitted poisoned");
        devices.iter().any(|d| admitted.contains(d))
    }

    /// Controller: replace the admitted device-key set (called on a membership delta).
    pub fn publish_admitted(&self, keys: BTreeSet<[u8; 32]>) {
        *self.admitted.write().expect("admitted poisoned") = keys;
    }

    /// Resolver: record that `node_id` is asserted by (verified) `device_key`.
    pub fn bind(&self, node_id: [u8; 32], device_key: [u8; 32]) {
        self.node_to_devices
            .write()
            .expect("node_to_devices poisoned")
            .entry(node_id)
            .or_default()
            .insert(device_key);
    }

    /// Resolver `remove_owner`: forget these node_ids' bindings entirely.
    pub fn unbind_node_ids(&self, node_ids: &[[u8; 32]]) {
        let mut map = self.node_to_devices.write().expect("node_to_devices poisoned");
        for n in node_ids {
            map.remove(n);
        }
    }
}

/// Union of chosen ring neighbors across the joined communities — the controller's pure core.
/// Each entry is `(active enrolled device keys including self, community salt bytes)`.
///
/// Policy (both approved in the design): when `self_vk` is not on a community's ring — the join
/// window before local enrollment materializes — that community contributes its full active
/// device set, preventing islanding. Below `FULL_MESH_THRESHOLD`, `community_neighbors` already
/// returns all-but-self. `self_vk` is never included in the result.
pub fn compute_admitted(
    communities: &[(BTreeSet<[u8; 32]>, Vec<u8>)],
    self_vk: &[u8; 32],
) -> BTreeSet<[u8; 32]> {
    let mut out = BTreeSet::new();
    for (devices, salt) in communities {
        if devices.contains(self_vk) {
            out.extend(community_neighbors(devices, self_vk, salt));
        } else {
            // self not on ring: admit the whole active set for this community (anti-islanding).
            out.extend(devices.iter().copied());
        }
    }
    out.remove(self_vk);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_topology::FULL_MESH_THRESHOLD;

    fn nid(b: u8) -> [u8; 32] {
        [b; 32]
    }
    fn dk(b: u8) -> [u8; 32] {
        [0x80 | b; 32]
    }
    fn synth(n: usize) -> BTreeSet<[u8; 32]> {
        (0..n)
            .map(|i| harmony_crypto::hash::blake3_hash(&(i as u64).to_be_bytes()))
            .collect()
    }

    // ---- oracle ----

    #[test]
    fn peer_mode_admits_everything() {
        let o = AdmissionOracle::new(false);
        assert!(o.admit(&nid(1)));
        o.publish_admitted(BTreeSet::new());
        assert!(o.admit(&nid(1)));
    }

    #[test]
    fn router_mode_unknown_node_id_fails_open() {
        let o = AdmissionOracle::new(true);
        o.publish_admitted(BTreeSet::from([dk(1)]));
        assert!(o.admit(&nid(9)), "no binding -> fail open");
    }

    #[test]
    fn router_mode_admits_bound_admitted_denies_bound_unadmitted() {
        let o = AdmissionOracle::new(true);
        o.publish_admitted(BTreeSet::from([dk(1)]));
        o.bind(nid(1), dk(1));
        o.bind(nid(2), dk(2));
        assert!(o.admit(&nid(1)), "bound to admitted device key");
        assert!(!o.admit(&nid(2)), "bound to non-admitted device key");
    }

    #[test]
    fn multi_device_node_id_admits_on_intersection() {
        let o = AdmissionOracle::new(true);
        o.publish_admitted(BTreeSet::from([dk(5)]));
        o.bind(nid(1), dk(4));
        o.bind(nid(1), dk(5)); // same node_id, second device key
        assert!(o.admit(&nid(1)), "one bound key is admitted");
    }

    #[test]
    fn publish_admitted_transitions_membership() {
        let o = AdmissionOracle::new(true);
        o.bind(nid(2), dk(2));
        assert!(!o.admit(&nid(2)));
        o.publish_admitted(BTreeSet::from([dk(2)]));
        assert!(o.admit(&nid(2)), "admitted after republish");
    }

    #[test]
    fn unbind_node_ids_drops_bindings() {
        let o = AdmissionOracle::new(true);
        o.publish_admitted(BTreeSet::from([dk(2)]));
        o.bind(nid(2), dk(2));
        assert!(o.admit(&nid(2)));
        o.unbind_node_ids(&[nid(2)]);
        assert!(o.admit(&nid(2)), "binding gone -> unknown -> fail open");
    }

    // ---- controller pure compute ----

    #[test]
    fn compute_admitted_below_threshold_is_full_mesh_minus_self() {
        let mut devices = synth(FULL_MESH_THRESHOLD - 1);
        let self_vk = *devices.iter().next().unwrap();
        devices.insert(self_vk);
        let out = compute_admitted(&[(devices.clone(), b"salt".to_vec())], &self_vk);
        let expected: BTreeSet<[u8; 32]> =
            devices.iter().copied().filter(|d| *d != self_vk).collect();
        assert_eq!(out, expected);
    }

    #[test]
    fn compute_admitted_above_threshold_is_bounded_and_excludes_self() {
        let devices = synth(FULL_MESH_THRESHOLD + 40);
        let self_vk = *devices.iter().next().unwrap();
        let out = compute_admitted(&[(devices.clone(), b"salt".to_vec())], &self_vk);
        assert!(!out.contains(&self_vk));
        assert!(out.len() < devices.len() - 1, "bounded, not full mesh");
        let direct = community_neighbors(&devices, &self_vk, b"salt");
        assert_eq!(out, direct);
    }

    #[test]
    fn compute_admitted_unions_across_communities() {
        let a = synth(FULL_MESH_THRESHOLD + 10);
        let b: BTreeSet<[u8; 32]> = synth(FULL_MESH_THRESHOLD + 10)
            .iter()
            .map(|d| {
                let mut x = *d;
                x[0] ^= 0xFF;
                x
            })
            .collect();
        let self_vk = *a.iter().next().unwrap();
        let mut b2 = b.clone();
        b2.insert(self_vk); // self is a member of both
        let out = compute_admitted(
            &[(a.clone(), b"A".to_vec()), (b2.clone(), b"B".to_vec())],
            &self_vk,
        );
        let na = community_neighbors(&a, &self_vk, b"A");
        let nb = community_neighbors(&b2, &self_vk, b"B");
        let mut expected = na.clone();
        expected.extend(nb);
        assert_eq!(out, expected);
    }

    #[test]
    fn compute_admitted_self_not_on_ring_falls_back_to_full_mesh() {
        let devices = synth(FULL_MESH_THRESHOLD + 5);
        let self_vk = [0xAB; 32]; // deliberately absent
        assert!(!devices.contains(&self_vk));
        let out = compute_admitted(&[(devices.clone(), b"salt".to_vec())], &self_vk);
        assert_eq!(out, devices, "full active set admitted while self un-materialized");
    }
}
