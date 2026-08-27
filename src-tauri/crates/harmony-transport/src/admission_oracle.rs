//! Node-id admission classifier for R4 bounded-degree dialing (ZEB-928).
//!
//! Read on the hot path at the supervisor's dial-dispatch point. Peer mode — or an
//! unwired oracle — admits everything, so behavior is byte-for-byte identical to pre-R4.
//! Router mode admits a node_id iff a device key bound to it is in the admitted set; an
//! unknown node_id fails open.
//!
//! Three writers, one reader:
//! - the event-loop **controller** replaces the admitted *device-key* set on membership deltas
//!   ([`AdmissionOracle::publish_admitted`], fed by [`compute_admitted`]);
//! - the **resolver** binds `(owner, node_id) -> device_key` as records are verified/ingested
//!   ([`AdmissionOracle::bind`] / [`AdmissionOracle::unbind_owner`]);
//! - the **supervisor** reads [`AdmissionOracle::admit`] at the dial-dispatch point.
//!
//! Bindings are keyed by owner as well as node_id: a node_id can be asserted under more than
//! one owner (delegate/butler devices, a node hosting multiple identities), and a departing
//! owner must not evict a co-resident owner's still-live binding for a shared node_id.
//!
//! See `docs/superpowers/specs/2026-08-13-zeb928-r4-wiring-design.md`.

use crate::community_topology::community_neighbors;
use std::collections::{BTreeSet, HashMap};
use std::sync::RwLock;

/// The *current* enrolled device verify key each owner asserts for a node_id, with the
/// freshness stamp of the record that asserted it. Keyed by owner so
/// (a) a departing owner never evicts a co-resident owner's binding for a shared node_id, and
/// (b) a superseded reachability record REPLACES that owner's prior key rather than accumulating
/// stale keys that could keep the node admitted after its verified key changed (Greptile P1).
/// The stamp (ZEB-995) orders competing binds for the same `(owner, node_id)`: book ingest is
/// per-community LWW, so without it, community iteration/delivery order — not record freshness —
/// decided which community's key won the slot.
type OwnerDeviceKeys = HashMap<[u8; 16], ([u8; 32], u64)>;

/// Shared admission classifier. Cheap to read; the hot path takes read locks only.
pub struct AdmissionOracle {
    /// Router-mode gate, fixed at construction. Disabled → [`AdmissionOracle::admit`] is always
    /// true (no filtering, no behavior change from pre-R4).
    enabled: bool,
    /// Admitted enrolled device keys (the realized ring-neighbor union). Controller writes.
    admitted: RwLock<BTreeSet<[u8; 32]>>,
    /// Reverse bridge `iroh_node_id -> {owner -> current enrolled device key}`. Admission is
    /// "some owner's current bound key is in the admitted set"; a rebind replaces that owner's
    /// prior key (no stale keys), and eviction is scoped to one owner. Resolver writes.
    node_to_owned_devices: RwLock<HashMap<[u8; 32], OwnerDeviceKeys>>,
}

impl AdmissionOracle {
    /// `enabled` = router mode. A disabled oracle admits everything.
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            admitted: RwLock::new(BTreeSet::new()),
            node_to_owned_devices: RwLock::new(HashMap::new()),
        }
    }

    /// Whether the filter is active (router mode). The controller skips all polling when false.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Hot path. `true` = allow/keep this dial target. Peer mode → always true. Router mode →
    /// admit iff a device key bound to `node_id` (under any owner) is in the admitted set;
    /// **fail-open** on an unknown node_id (no binding: infrastructure or non-community peers
    /// stay dialable).
    pub fn admit(&self, node_id: &[u8; 32]) -> bool {
        if !self.enabled {
            return true;
        }
        let map = self
            .node_to_owned_devices
            .read()
            .expect("node_to_owned_devices poisoned");
        let owners = match map.get(node_id) {
            Some(d) => d,
            None => return true, // fail-open on unknown identity
        };
        let admitted = self.admitted.read().expect("admitted poisoned");
        owners.values().any(|(dk, _)| admitted.contains(dk))
    }

    /// Controller: replace the admitted device-key set (called on a membership delta).
    pub fn publish_admitted(&self, keys: BTreeSet<[u8; 32]>) {
        *self.admitted.write().expect("admitted poisoned") = keys;
    }

    /// Resolver: record that `node_id`'s *current* verified key under `owner` is `device_key`,
    /// asserted by a record stamped `stamp_ms` (the record's clamped HLC wall time — callers
    /// clamp to their site's skew tolerance so a future-dated record can't squat the slot).
    /// Replaces any prior key this owner asserted for the node_id, so a superseded record can't
    /// leave a stale key admitted (Greptile P1, PR #674).
    pub fn bind(&self, owner: [u8; 16], node_id: [u8; 32], device_key: [u8; 32], stamp_ms: u64) {
        let mut map = self
            .node_to_owned_devices
            .write()
            .expect("node_to_owned_devices poisoned");
        let owners = map.entry(node_id).or_default();
        // ZEB-995: per-(owner, node_id) LWW on the record stamp. Book ingest is per-community
        // LWW, so bind order across communities tracks iteration/delivery order, not global
        // freshness — a strictly older record must not clobber the current key. Equal stamps
        // still replace, preserving the #674 same-stamp refresh semantics exactly.
        match owners.get(&owner) {
            Some(&(_, existing_ms)) if existing_ms > stamp_ms => {}
            _ => {
                owners.insert(owner, (device_key, stamp_ms));
            }
        }
    }

    /// Resolver `remove_owner`: forget only `owner`'s binding for these node_ids. A node_id still
    /// bound under another owner survives; a node_id whose owner-map becomes empty is dropped.
    pub fn unbind_owner(&self, owner: [u8; 16], node_ids: &[[u8; 32]]) {
        let mut map = self
            .node_to_owned_devices
            .write()
            .expect("node_to_owned_devices poisoned");
        for n in node_ids {
            if let Some(owners) = map.get_mut(n) {
                owners.remove(&owner);
                if owners.is_empty() {
                    map.remove(n);
                }
            }
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
    fn own(b: u8) -> [u8; 16] {
        [b; 16]
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
        o.bind(own(1), nid(1), dk(1), 1_000);
        o.bind(own(2), nid(2), dk(2), 1_000);
        assert!(o.admit(&nid(1)), "bound to admitted device key");
        assert!(!o.admit(&nid(2)), "bound to non-admitted device key");
    }

    #[test]
    fn rebind_supersedes_prior_key_for_owner() {
        // A new record for the same (owner, node_id) with a different enrolled key must REPLACE
        // the prior key — the superseded key can't keep the node admitted (Greptile P1, #674).
        let o = AdmissionOracle::new(true);
        o.bind(own(1), nid(1), dk(4), 1_000); // old key, once a neighbor
        o.bind(own(1), nid(1), dk(5), 1_000); // rotation: dk(5) is now the current key
        o.publish_admitted(BTreeSet::from([dk(4)]));
        assert!(
            !o.admit(&nid(1)),
            "superseded key dk(4) must NOT keep the node admitted"
        );
        o.publish_admitted(BTreeSet::from([dk(5)]));
        assert!(o.admit(&nid(1)), "current key dk(5) admits");
    }

    // ---- ZEB-995: stamp-LWW bind (cross-community bind-overwrite) ----

    #[test]
    fn bind_stale_stamp_does_not_overwrite_fresh() {
        // Cross-community replay: community B's fresh record bound first, then community A's
        // stale-but-TTL-fresh row for the same (owner, node_id) arrives. Iteration/delivery
        // order must not decide the winner — the fresher record's key must survive.
        let o = AdmissionOracle::new(true);
        o.bind(own(1), nid(1), dk(2), 200); // fresh (rotated-identity key)
        o.bind(own(1), nid(1), dk(1), 100); // stale (pre-recovery key, other community's book)
        o.publish_admitted(BTreeSet::from([dk(2)]));
        assert!(o.admit(&nid(1)), "fresh binding survives a stale re-bind");
        o.publish_admitted(BTreeSet::from([dk(1)]));
        assert!(!o.admit(&nid(1)), "stale key did not install");
    }

    #[test]
    fn bind_fresh_stamp_replaces_stale() {
        let o = AdmissionOracle::new(true);
        o.bind(own(1), nid(1), dk(1), 100);
        o.bind(own(1), nid(1), dk(2), 200);
        o.publish_admitted(BTreeSet::from([dk(1)]));
        assert!(!o.admit(&nid(1)), "superseded key no longer admits");
        o.publish_admitted(BTreeSet::from([dk(2)]));
        assert!(o.admit(&nid(1)), "fresh key admits");
    }

    #[test]
    fn bind_equal_stamp_replaces() {
        // At equal stamps the later write still replaces — the #674 replace-semantics are
        // preserved exactly for same-stamp refreshes (never accumulate, never resurrect).
        let o = AdmissionOracle::new(true);
        o.bind(own(1), nid(1), dk(1), 100);
        o.bind(own(1), nid(1), dk(2), 100);
        o.publish_admitted(BTreeSet::from([dk(2)]));
        assert!(
            o.admit(&nid(1)),
            "equal-stamp bind replaces (#674 semantics)"
        );
    }

    #[test]
    fn publish_admitted_transitions_membership() {
        let o = AdmissionOracle::new(true);
        o.bind(own(2), nid(2), dk(2), 1_000);
        assert!(!o.admit(&nid(2)));
        o.publish_admitted(BTreeSet::from([dk(2)]));
        assert!(o.admit(&nid(2)), "admitted after republish");
    }

    #[test]
    fn unbind_owner_drops_only_that_owners_binding() {
        let o = AdmissionOracle::new(true);
        o.publish_admitted(BTreeSet::from([dk(2)]));
        o.bind(own(2), nid(2), dk(2), 1_000);
        assert!(o.admit(&nid(2)));
        o.unbind_owner(own(2), &[nid(2)]);
        assert!(o.admit(&nid(2)), "binding gone -> unknown -> fail open");
    }

    #[test]
    fn unbind_owner_preserves_co_resident_owner_on_shared_node_id() {
        // The SAME node_id is asserted under two owners; owner A's departure must not
        // evict owner B's live, admitted binding (CR-2 / CodeRabbit PR #674).
        let o = AdmissionOracle::new(true);
        let shared = nid(7);
        o.bind(own(0xA), shared, dk(1), 1_000); // owner A, NOT admitted
        o.bind(own(0xB), shared, dk(2), 1_000); // owner B, admitted
        o.publish_admitted(BTreeSet::from([dk(2)]));
        assert!(o.admit(&shared), "admitted via owner B");
        o.unbind_owner(own(0xA), &[shared]);
        assert!(
            o.admit(&shared),
            "owner B's admitted binding survives owner A's departure"
        );
        o.unbind_owner(own(0xB), &[shared]);
        assert!(o.admit(&shared), "now fully unbound -> fail open (unknown)");
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
    fn compute_admitted_at_n200_bounded_degree_is_14() {
        // The ticket's headline (ZEB-929 claim C): at N=200 the bounded degree is ~2·log₂N = 14
        // (100 is not a power of two, so no antipode collapse). Pins the number as a regression
        // guard against a topology-engine drift.
        let devices = synth(200);
        let self_vk = *devices.iter().next().unwrap();
        let salt = b"zeb929".to_vec();
        let out = compute_admitted(&[(devices.clone(), salt.clone())], &self_vk);
        // Structural: equals the engine directly (never a naively-applied formula).
        assert_eq!(out, community_neighbors(&devices, &self_vk, &salt));
        assert!(!out.contains(&self_vk));
        assert_eq!(
            out.len(),
            14,
            "bounded degree at N=200 is 2·(⌊log₂100⌋+1) = 14"
        );
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
        assert_eq!(
            out, devices,
            "full active set admitted while self un-materialized"
        );
    }
}
