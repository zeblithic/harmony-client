//! ZEB-510 step 2: same-owner fleet-peer dial seeds observed during SAS
//! pairing. A one-shot local store (NOT a synced CRDT) that lets a device dial
//! a freshly-paired sibling before fleet-net has ever converged. Fed into the
//! ReachabilityResolver at boot as a `FleetSibling` entry; superseded by the
//! sibling's real FleetNetDoc row (same node_id) once fleet-net converges.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetPeerSeedDoc {
    /// Keyed by the peer's iroh node_id (hex). Both sides learn the peer's
    /// node_id directly from the received SAS `Confirm`; the resolver key is
    /// `(self_owner, iroh_node_id)` regardless, so a seed and the eventual real
    /// FleetNetDoc row converge on the same resolver slot.
    #[serde(rename = "sd")]
    pub seeds: BTreeMap<String, FleetPeerSeedRow>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetPeerSeedRow {
    #[serde(rename = "ep")]
    pub iroh_node_id: [u8; 32],
    #[serde(rename = "hr")]
    pub home_relay: String,
    /// Pairing-time wall-clock ms; the resolver entry's announce time.
    #[serde(rename = "oa")]
    pub observed_at_ms: u64,
}

/// Project a seed row into a dial-target reachability payload for the
/// ReachabilityResolver. Verification-exempt (zero signature): the endpoint's
/// integrity comes from the SAS-authenticated channel it was observed on.
/// Mirrors `crate::fleet_net::sibling_reachability_payload`.
pub fn seed_reachability_payload(
    row: &FleetPeerSeedRow,
) -> crate::reachability_record::ReachabilityAnnouncePayload {
    crate::reachability_record::ReachabilityAnnouncePayload {
        iroh_node_id: row.iroh_node_id,
        home_relay_url: row.home_relay.clone(),
        direct_addresses: Vec::new(),
        announced_at_ms: row.observed_at_ms,
        identity_signature: [0u8; 64],
        butler_set: Vec::new(),
        bs_at: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_payload_maps_fields_and_is_unsigned() {
        let row = FleetPeerSeedRow {
            iroh_node_id: [0xB2; 32],
            home_relay: "https://relay.example/".into(),
            observed_at_ms: 4242,
        };
        let p = seed_reachability_payload(&row);
        assert_eq!(p.iroh_node_id, [0xB2; 32]);
        assert_eq!(p.home_relay_url, "https://relay.example/");
        assert_eq!(p.announced_at_ms, 4242);
        assert!(p.direct_addresses.is_empty());
        assert_eq!(p.identity_signature, [0u8; 64]);
        assert!(p.butler_set.is_empty());
        assert_eq!(p.bs_at, 0);
    }
}
