//! ZEB-290 Phase 1: shared voting infrastructure (types + lifecycle + envelope).
//!
//! See spec `docs/specs/2026-05-16-zeb-289-voting-polling-design.md` §2 + §3.
//!
//! This module owns wire-stable types used by all voting tiers
//! (`voting_approval.rs`, future `voting_conviction.rs`, `voting_sortition.rs`).

use serde::{Deserialize, Serialize};

// `crate::owner_state_types::{Hlc, OwnerAddr, SpaceId}` will be wired in
// in later ZEB-290 Phase 1 tasks (envelope + tier-1 ballot structs).
// Kept out of imports for now so clippy `-D warnings` stays green.

/// Globally-unique identifier for a poll, derived from
/// `H(community_id || poll_create_event_hash)`.
///
/// 32 bytes (SHA-256 output). Newtype wrapper keeps type-safety —
/// callers cannot accidentally pass a raw `[u8; 32]` like a `ChannelId`
/// or `EventId` of the same length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PollId(pub [u8; 32]);

/// The three voting tiers. Wire-encoded as u8 (`tr` field of envelope).
/// See spec §1 + §3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Tier {
    Approval = 1,
    Conviction = 2,
    Sortition = 3,
}

/// Per-poll eligibility predicate. Spec §1 Goal 5 + §7.
///
/// `min_power`: required member power level (verified against
/// community_membership at the poll's eligibility-snapshot HLC).
///
/// `min_vouching_depth`: optional Sybil filter; voter must be vouched
/// for by at least this many other members. None = no vouching gate.
///
/// `sortition_size`: Tier 3 only; ignored for Tier 1/2. Reserved here
/// so the type is wire-stable across all tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Eligibility {
    #[serde(rename = "mp")]
    pub min_power: u8,
    #[serde(rename = "mv", skip_serializing_if = "Option::is_none", default)]
    pub min_vouching_depth: Option<u8>,
    #[serde(rename = "sz", skip_serializing_if = "Option::is_none", default)]
    pub sortition_size: Option<u16>,
}

/// Poll lifecycle state. Spec §2 (poll lifecycle diagram).
///
/// Transitions: Draft → Open → Closed → Finalized → Archived.
/// (Draft is implementation-only — never on the wire; PollCreate
/// events publish directly into Open.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lifecycle {
    Draft,
    Open,
    Closed,
    Finalized,
    Archived,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_id_round_trip() {
        let pid = PollId([0x42; 32]);
        let mut encoded = Vec::new();
        ciborium::into_writer(&pid, &mut encoded).expect("encode");
        let decoded: PollId = ciborium::from_reader(&encoded[..]).expect("decode");
        assert_eq!(pid, decoded);
    }

    #[test]
    fn tier_is_u8_repr() {
        assert_eq!(Tier::Approval as u8, 1);
        assert_eq!(Tier::Conviction as u8, 2);
        assert_eq!(Tier::Sortition as u8, 3);
    }

    #[test]
    fn eligibility_minimal_omits_optional_fields() {
        let e = Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        };
        let mut encoded = Vec::new();
        ciborium::into_writer(&e, &mut encoded).expect("encode");
        // Should be a 1-field map: just "mp" → 0.
        let value: ciborium::Value = ciborium::from_reader(&encoded[..]).expect("decode as value");
        let map = value.as_map().expect("map");
        assert_eq!(map.len(), 1, "optional None fields must be skipped");
        assert!(map
            .iter()
            .any(|(k, _): &(ciborium::Value, ciborium::Value)| k.as_text() == Some("mp")));
    }

    #[test]
    fn lifecycle_round_trip() {
        for state in &[
            Lifecycle::Draft,
            Lifecycle::Open,
            Lifecycle::Closed,
            Lifecycle::Finalized,
            Lifecycle::Archived,
        ] {
            let mut encoded = Vec::new();
            ciborium::into_writer(state, &mut encoded).expect("encode");
            let decoded: Lifecycle = ciborium::from_reader(&encoded[..]).expect("decode");
            assert_eq!(*state, decoded);
        }
    }
}
