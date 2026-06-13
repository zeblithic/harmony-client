//! ZEB-458 P4 Phase B: per-community relay opt-in, fleet-replicated so every
//! online device of a volunteering owner advertises + serves (D43). LWW per
//! community by HLC, mirroring FleetNetDoc / RelayHoldDoc.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::fleet_sync::MergeOutcome;
use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::{Hlc, SpaceId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayOptInState {
    #[serde(rename = "oi")]
    pub opted_in: bool,
    #[serde(rename = "st")]
    pub stamp: Hlc,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayOptInDoc {
    #[serde(rename = "co")]
    pub communities: BTreeMap<SpaceId, RelayOptInState>,
}

impl CanonicalPayloadSealed for RelayOptInState {}
impl CanonicalPayload for RelayOptInState {}
impl CanonicalPayloadSealed for RelayOptInDoc {}
impl CanonicalPayload for RelayOptInDoc {}

impl RelayOptInDoc {
    /// LWW set: a strictly-newer (or first) stamp wins; older/equal ignored.
    pub fn set(&mut self, community_id: SpaceId, opted_in: bool, stamp: Hlc) {
        match self.communities.get(&community_id) {
            Some(s) if !stamp.is_strictly_newer_than(&s.stamp) => {}
            _ => {
                self.communities
                    .insert(community_id, RelayOptInState { opted_in, stamp });
            }
        }
    }

    pub fn is_opted_in(&self, community_id: &SpaceId) -> bool {
        self.communities
            .get(community_id)
            .map(|s| s.opted_in)
            .unwrap_or(false)
    }

    pub fn opted_in_communities(&self) -> Vec<SpaceId> {
        self.communities
            .iter()
            .filter(|(_, s)| s.opted_in)
            .map(|(c, _)| *c)
            .collect()
    }

    /// LWW merge by per-community stamp. `changed` true if any entry advanced.
    pub fn merge_from(&mut self, remote: RelayOptInDoc) -> MergeOutcome {
        let mut changed = false;
        for (c, rs) in remote.communities {
            match self.communities.get(&c) {
                Some(ls) if !rs.stamp.is_strictly_newer_than(&ls.stamp) => {}
                _ => {
                    self.communities.insert(c, rs);
                    changed = true;
                }
            }
        }
        MergeOutcome { changed }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::{Hlc, SpaceId};

    fn hlc(ms: u64, dev: &str) -> Hlc {
        Hlc {
            wall_ms: ms,
            logical: 0,
            device_id: dev.into(),
        }
    }

    #[test]
    fn set_then_query_round_trips() {
        let mut d = RelayOptInDoc::default();
        let c = SpaceId([0xCC; 16]);
        assert!(!d.is_opted_in(&c));
        d.set(c, true, hlc(100, "a"));
        assert!(d.is_opted_in(&c));
        assert_eq!(d.opted_in_communities(), vec![c]);
    }

    #[test]
    fn later_stamp_wins_lww() {
        let mut d = RelayOptInDoc::default();
        let c = SpaceId([0xCC; 16]);
        d.set(c, true, hlc(100, "a"));
        d.set(c, false, hlc(50, "a")); // older — ignored
        assert!(d.is_opted_in(&c));
        d.set(c, false, hlc(200, "a")); // newer — wins
        assert!(!d.is_opted_in(&c));
    }

    #[test]
    fn merge_from_is_lww_and_reports_change() {
        let mut local = RelayOptInDoc::default();
        let c = SpaceId([0xCC; 16]);
        local.set(c, true, hlc(100, "a"));
        let mut remote = RelayOptInDoc::default();
        remote.set(c, false, hlc(200, "b")); // newer opt-out from a sibling
        let out = local.merge_from(remote);
        assert!(out.changed);
        assert!(!local.is_opted_in(&c));
    }

    #[test]
    fn merge_from_no_change_when_local_newer() {
        let mut local = RelayOptInDoc::default();
        let c = SpaceId([0xCC; 16]);
        local.set(c, true, hlc(300, "a"));
        let mut remote = RelayOptInDoc::default();
        remote.set(c, false, hlc(100, "b"));
        let out = local.merge_from(remote);
        assert!(!out.changed);
        assert!(local.is_opted_in(&c));
    }

    #[test]
    fn doc_round_trips_canonical_cbor() {
        // Confirm the doc serializes/deserializes (it is FleetSync-persisted).
        let mut d = RelayOptInDoc::default();
        d.set(SpaceId([0xCC; 16]), true, hlc(100, "a"));
        let bytes = crate::owner_state_crypto::canonical_cbor_encode(&d).expect("encode");
        let back: RelayOptInDoc =
            crate::owner_state_crypto::canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(back, d);
    }
}
