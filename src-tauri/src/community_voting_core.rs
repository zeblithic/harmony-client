//! ZEB-290 Phase 1: shared voting infrastructure (types + lifecycle + envelope).
//!
//! See spec `docs/specs/2026-05-16-zeb-289-voting-polling-design.md` §2 + §3.
//!
//! This module owns wire-stable types used by all voting tiers
//! (`voting_approval.rs`, future `voting_conviction.rs`, `voting_sortition.rs`).

use crate::owner_state_types::{Hlc, OwnerAddr};
use serde::{Deserialize, Serialize};

// `crate::owner_state_types::SpaceId` will be wired in
// in later ZEB-290 Phase 1 tasks (tier-1 ballot structs).
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

/// Discriminator for the kind of voting event (cr/op/xt/cl/bl/rs for
/// Phase 1; sg/dg/ud added in Phase 2; ss/ds/dv/dc/rb/ts added in
/// Phase 4-6). Wire-encoded as a 2-char string in the envelope's `kd`
/// field. Spec §3 event-kind catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PollEventKindCode {
    #[serde(rename = "cr")]
    PollCreate,
    #[serde(rename = "op")]
    PollOpen,
    #[serde(rename = "xt")]
    PollExtend,
    #[serde(rename = "cl")]
    PollClose,
    #[serde(rename = "bl")]
    BallotCast,
    #[serde(rename = "rs")]
    PollResult,
}

/// The wire envelope for every voting event. Spec §3.
///
/// All 8 fields use 2-char keys to satisfy the same-length-keys
/// invariant. The `pd` field is opaque tier+kind-specific CBOR bytes.
/// `sg` (signature) is computed over the canonical CBOR of all fields
/// EXCEPT `sg` itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedVotingEvent {
    #[serde(rename = "tg")]
    pub tag: char,
    #[serde(rename = "vr")]
    pub version: u8,
    #[serde(rename = "tr")]
    pub tier: Tier,
    #[serde(rename = "kd")]
    pub kind: PollEventKindCode,
    #[serde(rename = "hc")]
    pub hlc: Hlc,
    #[serde(rename = "ac")]
    pub actor: OwnerAddr,
    #[serde(rename = "pd")]
    pub payload: Vec<u8>,
    #[serde(rename = "sg")]
    pub sig: Vec<u8>,
}

impl SignedVotingEvent {
    /// Canonical CBOR bytes the signature covers (all fields except sg).
    pub fn signing_bytes(&self) -> Result<Vec<u8>, ciborium::ser::Error<std::io::Error>> {
        #[derive(Serialize)]
        struct SigInput<'a> {
            #[serde(rename = "tg")]
            tag: char,
            #[serde(rename = "vr")]
            version: u8,
            #[serde(rename = "tr")]
            tier: Tier,
            #[serde(rename = "kd")]
            kind: PollEventKindCode,
            #[serde(rename = "hc")]
            hlc: &'a Hlc,
            #[serde(rename = "ac")]
            actor: &'a OwnerAddr,
            #[serde(rename = "pd")]
            payload: &'a [u8],
        }
        let inp = SigInput {
            tag: self.tag,
            version: self.version,
            tier: self.tier,
            kind: self.kind,
            hlc: &self.hlc,
            actor: &self.actor,
            payload: &self.payload,
        };
        let mut out = Vec::new();
        ciborium::ser::into_writer(&inp, &mut out)?;
        Ok(out)
    }
}

#[cfg(test)]
mod envelope_tests {
    use super::*;

    fn make_event(kind: PollEventKindCode) -> SignedVotingEvent {
        SignedVotingEvent {
            tag: 'p',
            version: 1,
            tier: Tier::Approval,
            kind,
            hlc: Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "test".into(),
            },
            actor: OwnerAddr([0xaa; 16]),
            payload: vec![0xde, 0xad],
            sig: vec![0u8; 64],
        }
    }

    #[test]
    fn envelope_round_trips() {
        let ev = make_event(PollEventKindCode::PollCreate);
        let mut encoded = Vec::new();
        ciborium::into_writer(&ev, &mut encoded).expect("encode");
        let decoded: SignedVotingEvent = ciborium::from_reader(&encoded[..]).expect("decode");
        assert_eq!(ev, decoded);
    }

    #[test]
    fn envelope_has_eight_top_level_keys() {
        let ev = make_event(PollEventKindCode::BallotCast);
        let mut encoded = Vec::new();
        ciborium::into_writer(&ev, &mut encoded).expect("encode");
        let value: ciborium::Value = ciborium::from_reader(&encoded[..]).expect("decode as value");
        let map = value.as_map().expect("top-level is a CBOR map");
        assert_eq!(map.len(), 8, "envelope must have exactly 8 fields");
        for expected in &["tg", "vr", "tr", "kd", "hc", "ac", "pd", "sg"] {
            assert!(
                map.iter()
                    .any(|(k, _): &(ciborium::Value, ciborium::Value)| k.as_text()
                        == Some(*expected)),
                "envelope missing key {expected:?}"
            );
        }
    }

    #[test]
    fn envelope_keys_all_two_char() {
        let ev = make_event(PollEventKindCode::PollOpen);
        let mut encoded = Vec::new();
        ciborium::into_writer(&ev, &mut encoded).expect("encode");
        let value: ciborium::Value = ciborium::from_reader(&encoded[..]).expect("decode as value");
        let map = value.as_map().expect("map");
        for (k, _) in map.iter() {
            let s = k.as_text().expect("key is text");
            assert_eq!(s.len(), 2, "envelope key {s:?} violates 2-char invariant");
        }
    }

    #[test]
    fn signing_bytes_exclude_sig() {
        let mut ev = make_event(PollEventKindCode::PollResult);
        let sb1 = ev.signing_bytes().expect("signing bytes");
        ev.sig = vec![0xff; 64];
        let sb2 = ev.signing_bytes().expect("signing bytes");
        assert_eq!(sb1, sb2, "signing_bytes must be independent of sig field");
    }

    #[test]
    fn signing_bytes_have_seven_top_level_keys() {
        let ev = make_event(PollEventKindCode::PollClose);
        let sb = ev.signing_bytes().expect("signing bytes");
        let value: ciborium::Value = ciborium::from_reader(&sb[..]).expect("decode");
        let map = value.as_map().expect("map");
        assert_eq!(map.len(), 7, "signing bytes must exclude sg field");
        assert!(!map
            .iter()
            .any(|(k, _): &(ciborium::Value, ciborium::Value)| k.as_text() == Some("sg")));
    }

    #[test]
    fn kind_code_round_trip() {
        for kind in &[
            PollEventKindCode::PollCreate,
            PollEventKindCode::PollOpen,
            PollEventKindCode::PollExtend,
            PollEventKindCode::PollClose,
            PollEventKindCode::BallotCast,
            PollEventKindCode::PollResult,
        ] {
            let mut encoded = Vec::new();
            ciborium::into_writer(kind, &mut encoded).expect("encode");
            let decoded: PollEventKindCode = ciborium::from_reader(&encoded[..]).expect("decode");
            assert_eq!(*kind, decoded);
        }
    }
}
