//! ZEB-290 Phase 1: shared voting infrastructure (types + lifecycle + envelope).
//!
//! See spec `docs/specs/2026-05-16-zeb-289-voting-polling-design.md` §2 + §3.
//!
//! This module owns wire-stable types used by all voting tiers
//! (`voting_approval.rs`, future `voting_conviction.rs`, `voting_sortition.rs`).

use crate::community_membership::ChannelId;
use crate::owner_state_types::{
    deserialize_bytes_from_bstr, serialize_bytes_as_bstr, Hlc, OwnerAddr, SpaceId,
};
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Globally-unique identifier for a poll, derived from
/// `H(community_id || poll_create_event_hash)`.
///
/// 32 bytes (SHA-256 output). Newtype wrapper keeps type-safety —
/// callers cannot accidentally pass a raw `[u8; 32]` like a `ChannelId`
/// or `EventId` of the same length.
///
/// Uses `serialize_bytes_as_bstr` for CBOR consistency with the other
/// ID newtypes (`SpaceId`, `OwnerAddr`, `ChannelId`). Note: this fixes
/// the CBOR wire encoding but not the Tauri IPC JSON boundary — JSON
/// has no byte-string type and still emits an integer array. Frontend
/// must currently treat `poll_id` over IPC as `number[]`; a format-
/// aware (is_human_readable) serializer for hex-string IPC encoding
/// is tracked as a Phase 1.5 IPC-boundary concern shared with the
/// other ID types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PollId(
    #[serde(
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub [u8; 32],
);

/// The three voting tiers. Wire-encoded as u8 (`tr` field of envelope).
/// See spec §1 + §3.
///
/// `serde_repr` is load-bearing here: without it, the standard
/// `#[derive(Serialize)]` would encode variants by NAME ("Approval"),
/// not by the u8 discriminant the spec mandates. `#[repr(u8)]` alone
/// only affects Rust memory layout, not serde — that mismatch was
/// caught by CodeRabbit on PR #130.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize_repr, Deserialize_repr)]
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
/// Tier 1 transitions: Draft → Open → Closed → Finalized → Archived.
/// Tier 2 transitions: Draft → Open ⇄ ThresholdReached → Finalized → Archived.
/// (`ThresholdReached` is a Tier-2-only transient state where conviction
/// has crossed the threshold but the 24h contestability window has not
/// elapsed; late Unsignal events can drop conviction back below threshold,
/// reverting to `Open`.)
///
/// Draft is implementation-only — never on the wire; PollCreate events
/// publish directly into Open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lifecycle {
    Draft,
    Open,
    ThresholdReached,
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
            Lifecycle::ThresholdReached,
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
    #[serde(rename = "sg")]
    Signal,
    #[serde(rename = "dg")]
    Delegate,
    #[serde(rename = "ud")]
    Undelegate,
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
    /// Opaque tier+kind-specific CBOR-encoded payload. `serde_bytes`
    /// is load-bearing here: without it ciborium encodes `Vec<u8>` as
    /// a CBOR array-of-u8 (major type 4) instead of a byte string
    /// (major type 2), roughly doubling the on-wire size and producing
    /// a format peers from other languages wouldn't expect for binary.
    #[serde(rename = "pd", with = "serde_bytes")]
    pub payload: Vec<u8>,
    /// Ed25519 signature; see `payload` for serde_bytes rationale.
    #[serde(rename = "sg", with = "serde_bytes")]
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
            #[serde(rename = "pd", with = "serde_bytes")]
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
            PollEventKindCode::Signal,
            PollEventKindCode::Delegate,
            PollEventKindCode::Undelegate,
        ] {
            let mut encoded = Vec::new();
            ciborium::into_writer(kind, &mut encoded).expect("encode");
            let decoded: PollEventKindCode = ciborium::from_reader(&encoded[..]).expect("decode");
            assert_eq!(*kind, decoded);
        }
    }

    /// Pin the Tier 2 wire codes (`sg`/`dg`/`ud`) so a future enum
    /// rename can't silently change them.
    #[test]
    fn tier2_kind_codes_have_expected_wire_strings() {
        let cases = [
            (PollEventKindCode::Signal, "sg"),
            (PollEventKindCode::Delegate, "dg"),
            (PollEventKindCode::Undelegate, "ud"),
        ];
        for (kind, expected) in cases {
            let mut encoded = Vec::new();
            ciborium::into_writer(&kind, &mut encoded).expect("encode");
            let value: ciborium::Value =
                ciborium::from_reader(&encoded[..]).expect("decode as value");
            assert_eq!(
                value.as_text(),
                Some(expected),
                "wire code for {kind:?} must be {expected:?}"
            );
        }
    }
}

/// Materialized metadata for a single poll. Returned by
/// `voting_get_poll` / `voting_list_active_polls` IPCs.
///
/// `created_at`, `opens_at`, `closes_at` are HLC timestamps;
/// `extends_at` is the most recent PollExtend event's HLC (or None
/// if no extend has occurred).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollMeta {
    pub poll_id: PollId,
    pub community_id: SpaceId,
    pub creator: OwnerAddr,
    pub tier: Tier,
    pub eligibility: Eligibility,
    pub lifecycle: Lifecycle,
    pub created_at: Hlc,
    pub opens_at: Hlc,
    pub closes_at: Hlc,
    pub extends_at: Option<Hlc>,
    /// Channel where the poll was created (Tier 1 only; Tier 2/3 may
    /// not be channel-scoped). For Tier 1 chat-native polls this is
    /// the channel where the poll-message card appears.
    pub channel_id: Option<ChannelId>,
    /// Wall-clock ms (UNIX_EPOCH-relative) when the poll transitioned
    /// to `Lifecycle::Finalized`. Set by the tick for Tier 2 (which has
    /// no terminal event), unset for Tier 1 (which uses the PollResult
    /// event's HLC instead). `archive_finalized_polls` consults this for
    /// Tier 2 ageing — without it, Tier 2 finalized polls would never
    /// archive because the sweep only knew about PollResult HLCs.
    /// Defaults to `None` for backwards compatibility with pre-Tier-2
    /// PollState records (`#[serde(default)]`).
    #[serde(default)]
    pub finalized_at_ms: Option<u64>,
}

/// Deterministically derive a PollId from the community + the
/// PollCreate event's signing-bytes hash.
///
/// `PollId = SHA-256(community_id_bytes || create_event_signing_bytes)`.
///
/// Two nodes that independently observe the same PollCreate event
/// derive the same PollId. Re-derivable at any time; never stored
/// inside the event itself (would be circular).
pub fn derive_poll_id(community_id: &SpaceId, create_signing_bytes: &[u8]) -> PollId {
    let mut hasher = Sha256::new();
    hasher.update(community_id.0);
    hasher.update(create_signing_bytes);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    PollId(out)
}

#[cfg(test)]
mod poll_meta_tests {
    use super::*;

    #[test]
    fn derive_poll_id_is_deterministic() {
        let cid = SpaceId([0x11; 16]);
        let sb = vec![1, 2, 3, 4, 5];
        let pid1 = derive_poll_id(&cid, &sb);
        let pid2 = derive_poll_id(&cid, &sb);
        assert_eq!(pid1, pid2);
    }

    #[test]
    fn derive_poll_id_differs_by_community() {
        let sb = vec![1, 2, 3];
        let pid_a = derive_poll_id(&SpaceId([0x11; 16]), &sb);
        let pid_b = derive_poll_id(&SpaceId([0x22; 16]), &sb);
        assert_ne!(pid_a, pid_b);
    }

    #[test]
    fn derive_poll_id_differs_by_event_bytes() {
        let cid = SpaceId([0x33; 16]);
        let pid_a = derive_poll_id(&cid, &[1, 2, 3]);
        let pid_b = derive_poll_id(&cid, &[1, 2, 4]);
        assert_ne!(pid_a, pid_b);
    }

    #[test]
    fn poll_meta_round_trip() {
        let meta = PollMeta {
            poll_id: PollId([0xab; 32]),
            community_id: SpaceId([0x11; 16]),
            creator: OwnerAddr([0xcc; 16]),
            tier: Tier::Approval,
            eligibility: Eligibility {
                min_power: 1,
                min_vouching_depth: None,
                sortition_size: None,
            },
            lifecycle: Lifecycle::Open,
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "a".into(),
            },
            opens_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "a".into(),
            },
            closes_at: Hlc {
                wall_ms: 3700,
                logical: 0,
                device_id: "a".into(),
            },
            extends_at: None,
            channel_id: Some(ChannelId([0xdd; 16])),
            finalized_at_ms: None,
        };
        let mut encoded = Vec::new();
        ciborium::into_writer(&meta, &mut encoded).expect("encode");
        let decoded: PollMeta = ciborium::from_reader(&encoded[..]).expect("decode");
        assert_eq!(meta, decoded);
    }
}

/// Snapshot of community membership at a specific HLC, used by the
/// eligibility verifier. Built by querying `community_membership`
/// materialized state at the desired HLC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MembershipSnapshot {
    pub members: HashMap<OwnerAddr, MemberAttrs>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemberAttrs {
    pub power: u8,
    pub vouching_depth: u8,
}

/// Why an eligibility check failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EligibilityFailure {
    NotMember,
    InsufficientPower { required: u8, actual: u8 },
    InsufficientVouchingDepth { required: u8, actual: u8 },
}

/// Verify that `signer` meets `eligibility` against `snapshot`.
/// Returns Ok(()) if eligible; Err(reason) otherwise.
pub fn check_eligibility(
    snapshot: &MembershipSnapshot,
    signer: &OwnerAddr,
    eligibility: &Eligibility,
) -> Result<(), EligibilityFailure> {
    let attrs = snapshot
        .members
        .get(signer)
        .ok_or(EligibilityFailure::NotMember)?;
    if attrs.power < eligibility.min_power {
        return Err(EligibilityFailure::InsufficientPower {
            required: eligibility.min_power,
            actual: attrs.power,
        });
    }
    if let Some(req_depth) = eligibility.min_vouching_depth {
        if attrs.vouching_depth < req_depth {
            return Err(EligibilityFailure::InsufficientVouchingDepth {
                required: req_depth,
                actual: attrs.vouching_depth,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod eligibility_tests {
    use super::*;

    fn snapshot_with(addr: OwnerAddr, power: u8, vouching_depth: u8) -> MembershipSnapshot {
        let mut members = HashMap::new();
        members.insert(
            addr,
            MemberAttrs {
                power,
                vouching_depth,
            },
        );
        MembershipSnapshot { members }
    }

    #[test]
    fn non_member_rejected() {
        let snap = snapshot_with(OwnerAddr([0x11; 16]), 100, 5);
        let elig = Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: None,
        };
        assert_eq!(
            check_eligibility(&snap, &OwnerAddr([0x22; 16]), &elig),
            Err(EligibilityFailure::NotMember)
        );
    }

    #[test]
    fn member_with_sufficient_power_accepted() {
        let addr = OwnerAddr([0x11; 16]);
        let snap = snapshot_with(addr, 50, 0);
        let elig = Eligibility {
            min_power: 50,
            min_vouching_depth: None,
            sortition_size: None,
        };
        assert_eq!(check_eligibility(&snap, &addr, &elig), Ok(()));
    }

    #[test]
    fn member_with_insufficient_power_rejected() {
        let addr = OwnerAddr([0x11; 16]);
        let snap = snapshot_with(addr, 10, 0);
        let elig = Eligibility {
            min_power: 50,
            min_vouching_depth: None,
            sortition_size: None,
        };
        assert_eq!(
            check_eligibility(&snap, &addr, &elig),
            Err(EligibilityFailure::InsufficientPower {
                required: 50,
                actual: 10
            })
        );
    }

    #[test]
    fn vouching_depth_gate_enforced() {
        let addr = OwnerAddr([0x11; 16]);
        let snap = snapshot_with(addr, 1, 1);
        let elig = Eligibility {
            min_power: 1,
            min_vouching_depth: Some(3),
            sortition_size: None,
        };
        assert_eq!(
            check_eligibility(&snap, &addr, &elig),
            Err(EligibilityFailure::InsufficientVouchingDepth {
                required: 3,
                actual: 1
            })
        );
    }

    #[test]
    fn vouching_depth_gate_satisfied() {
        let addr = OwnerAddr([0x11; 16]);
        let snap = snapshot_with(addr, 1, 5);
        let elig = Eligibility {
            min_power: 1,
            min_vouching_depth: Some(3),
            sortition_size: None,
        };
        assert_eq!(check_eligibility(&snap, &addr, &elig), Ok(()));
    }

    #[test]
    fn power_checked_before_vouching_depth() {
        let addr = OwnerAddr([0x11; 16]);
        let snap = snapshot_with(addr, 1, 1);
        let elig = Eligibility {
            min_power: 50,
            min_vouching_depth: Some(10),
            sortition_size: None,
        };
        assert!(matches!(
            check_eligibility(&snap, &addr, &elig),
            Err(EligibilityFailure::InsufficientPower { .. })
        ));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleError {
    IllegalTransition {
        from: Lifecycle,
        attempted: PollEventKindCode,
    },
}

/// Given current lifecycle + incoming event kind + tier, return new lifecycle
/// or an error. Per spec §2 + verify rules L1, B2, R1.
///
/// The `tier` parameter gates Tier-specific transitions: only `Tier::Conviction`
/// polls may enter `Lifecycle::ThresholdReached`. Tier 1 (Approval) polls are
/// rejected at any ThresholdReached touchpoint.
///
/// For Tier 2, `Signal` is treated as a *toggle* between `Open` and
/// `ThresholdReached`: the caller is responsible for invoking
/// `next_lifecycle` with a `Signal` kind only when the conviction
/// computation indicates the threshold has actually been crossed (going up)
/// or dropped (going back down — typically a late-arriving CRDT Unsignal).
/// Normal-case Signal events that don't cross or drop the threshold are
/// not lifecycle-driving; the caller must skip the state machine in that
/// case (the Signal still applies to the delegation/signaling state).
///
/// `Delegate` and `Undelegate` never change lifecycle; they affect the
/// delegation graph, not the poll lifecycle. They're accepted in any
/// Open-equivalent state for Tier 2 only.
///
/// Tier 2 finalization (`ThresholdReached → Finalized`) is driven by a
/// `PollResult` event emitted after the 24h contestability window has
/// elapsed uncontested. There is no `ThresholdReached → Closed` path;
/// `PollClose` from `ThresholdReached` is rejected (per spec, the close
/// path is reserved for window-expiry on Tier 2 polls that never crossed
/// threshold, i.e. `Open → Closed → Finalized`).
pub fn next_lifecycle(
    current: Lifecycle,
    kind: PollEventKindCode,
    tier: Tier,
) -> Result<Lifecycle, LifecycleError> {
    use Lifecycle::*;
    use PollEventKindCode::*;
    let illegal = || LifecycleError::IllegalTransition {
        from: current,
        attempted: kind,
    };
    match (current, kind, tier) {
        // Tier-agnostic transitions (apply to both Tier 1 + Tier 2).
        (Draft, PollCreate, _) => Ok(Open),
        // BallotCast / PollOpen / PollExtend are Tier 1 (Approval) only —
        // Tier 2 uses Signal for vote events and has no explicit
        // open/extend lifecycle (those are derived from continuous
        // conviction state). Apply-time decode would also fail on the
        // wrong tier, but pinning the lifecycle gate here is a
        // defense-in-depth layer that catches malformed/forged events
        // before they reach apply (Cursor R4 Low Sev).
        (Open, BallotCast, Tier::Approval)
        | (Open, PollExtend, Tier::Approval)
        | (Open, PollOpen, Tier::Approval) => Ok(Open),
        (Open, PollClose, _) => Ok(Closed),
        (Closed, PollResult, _) => Ok(Finalized),

        // Tier 2: Signal toggles between Open and ThresholdReached.
        // Caller invokes only on threshold-cross or threshold-drop —
        // see fn-level doc.
        (Open, Signal, Tier::Conviction) => Ok(ThresholdReached),
        (ThresholdReached, Signal, Tier::Conviction) => Ok(Open),

        // Tier 2: Delegate / Undelegate do not move lifecycle.
        (Open, Delegate, Tier::Conviction)
        | (Open, Undelegate, Tier::Conviction)
        | (ThresholdReached, Delegate, Tier::Conviction)
        | (ThresholdReached, Undelegate, Tier::Conviction) => Ok(current),

        // Tier 2: 24h-uncontested finalization from ThresholdReached.
        (ThresholdReached, PollResult, Tier::Conviction) => Ok(Finalized),

        _ => Err(illegal()),
    }
}

/// Errors from signed-event construction helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildError {
    EncodePayload,
    SigningBytes,
}

/// Build a fully-signed PollCreate event for Tier 1, ready to broadcast.
/// Used by `voting_create_tier1_poll` IPC.
///
/// The tier is hardcoded to `Tier::Approval` so a caller cannot accidentally
/// emit a cross-tier-invalid event (Conviction/Sortition payload shapes
/// differ; pairing a Tier1PollConfig with a non-Approval tier on the wire
/// would produce events peers can't materialize).
pub fn build_signed_poll_create_tier1(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    config: &crate::community_voting_approval::Tier1PollConfig,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let mut payload = Vec::new();
    ciborium::ser::into_writer(config, &mut payload).map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Approval,
        kind: PollEventKindCode::PollCreate,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
}

/// Build a fully-signed BallotCast event for Tier 1.
pub fn build_signed_ballot_tier1(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    poll_id: PollId,
    approved_indices: Vec<u8>,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let ballot = crate::community_voting_approval::Tier1Ballot {
        poll_id,
        approved_indices,
    };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&ballot, &mut payload).map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Approval,
        kind: PollEventKindCode::BallotCast,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
}

/// Frontend-friendly subset of `PollState` for IPC return values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollStateExport {
    pub meta: PollMeta,
    pub tally: TallyExport,
    /// The calling node's own latest ballot (if any) — used by the UI
    /// to render "your current vote" state without a second IPC call.
    pub your_ballot: Option<Vec<u8>>,
    /// Tier-1 option labels (display strings). Empty for non-Tier-1
    /// polls or peer-received polls without a cached `Tier1PollConfig`.
    /// Populated from `state.tier1_cfg.options` so the UI can render
    /// labels alongside the tally without a second IPC.
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TallyExport {
    pub counts: Vec<u32>,
    pub ballot_count: u32,
}

#[cfg(test)]
mod build_tests {
    use super::*;
    use crate::community_membership::ChannelId;
    use crate::community_voting_approval::Tier1PollConfig;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    #[test]
    fn signed_poll_create_round_trip() {
        let mut csprng = OsRng;
        let keypair = SigningKey::generate(&mut csprng);
        let actor = OwnerAddr([0xaa; 16]);
        let cfg = Tier1PollConfig {
            options: vec!["a".into(), "b".into()],
            window_seconds: 600,
            quorum: None,
            threshold_percent: None,
            multi_winner: None,
            eligibility: Eligibility {
                min_power: 0,
                min_vouching_depth: None,
                sortition_size: None,
            },
            channel_id: ChannelId([0; 16]),
        };
        let hlc = Hlc {
            wall_ms: 1000,
            logical: 0,
            device_id: "a".into(),
        };
        let ev = build_signed_poll_create_tier1(&keypair, actor, &cfg, hlc).expect("build");
        assert_eq!(ev.kind, PollEventKindCode::PollCreate);
        assert_eq!(ev.actor, actor);
        let sb = ev.signing_bytes().expect("signing bytes");
        let sig_bytes: [u8; 64] = ev.sig.clone().try_into().expect("sig len");
        let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
        use ed25519_dalek::Verifier;
        keypair.verifying_key().verify(&sb, &sig).expect("verify");
    }

    #[test]
    fn signed_ballot_round_trip() {
        let mut csprng = OsRng;
        let keypair = SigningKey::generate(&mut csprng);
        let actor = OwnerAddr([0xbb; 16]);
        let pid = PollId([0xcc; 32]);
        let hlc = Hlc {
            wall_ms: 2000,
            logical: 0,
            device_id: "b".into(),
        };
        let ev = build_signed_ballot_tier1(&keypair, actor, pid, vec![0, 2], hlc).expect("build");
        assert_eq!(ev.kind, PollEventKindCode::BallotCast);
        let sb = ev.signing_bytes().expect("signing bytes");
        let sig_bytes: [u8; 64] = ev.sig.clone().try_into().expect("sig len");
        let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
        use ed25519_dalek::Verifier;
        keypair.verifying_key().verify(&sb, &sig).expect("verify");
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn draft_to_open_via_create() {
        assert_eq!(
            next_lifecycle(
                Lifecycle::Draft,
                PollEventKindCode::PollCreate,
                Tier::Approval
            ),
            Ok(Lifecycle::Open)
        );
    }

    #[test]
    fn open_accepts_ballot_cast() {
        assert_eq!(
            next_lifecycle(
                Lifecycle::Open,
                PollEventKindCode::BallotCast,
                Tier::Approval
            ),
            Ok(Lifecycle::Open)
        );
    }

    #[test]
    fn open_to_closed_via_close() {
        assert_eq!(
            next_lifecycle(
                Lifecycle::Open,
                PollEventKindCode::PollClose,
                Tier::Approval
            ),
            Ok(Lifecycle::Closed)
        );
    }

    #[test]
    fn closed_to_finalized_via_result() {
        assert_eq!(
            next_lifecycle(
                Lifecycle::Closed,
                PollEventKindCode::PollResult,
                Tier::Approval
            ),
            Ok(Lifecycle::Finalized)
        );
    }

    #[test]
    fn closed_rejects_ballot_cast() {
        assert!(matches!(
            next_lifecycle(
                Lifecycle::Closed,
                PollEventKindCode::BallotCast,
                Tier::Approval
            ),
            Err(LifecycleError::IllegalTransition { .. })
        ));
    }

    #[test]
    fn finalized_rejects_everything() {
        for kind in &[
            PollEventKindCode::BallotCast,
            PollEventKindCode::PollClose,
            PollEventKindCode::PollResult,
        ] {
            assert!(matches!(
                next_lifecycle(Lifecycle::Finalized, *kind, Tier::Approval),
                Err(LifecycleError::IllegalTransition { .. })
            ));
        }
    }

    #[test]
    fn archived_rejects_everything() {
        for kind in &[
            PollEventKindCode::BallotCast,
            PollEventKindCode::PollClose,
            PollEventKindCode::PollResult,
        ] {
            assert!(matches!(
                next_lifecycle(Lifecycle::Archived, *kind, Tier::Approval),
                Err(LifecycleError::IllegalTransition { .. })
            ));
        }
    }

    // ---------- Tier 2 (Conviction) transitions ----------

    #[test]
    fn tier2_open_to_threshold_reached_via_signal() {
        assert_eq!(
            next_lifecycle(Lifecycle::Open, PollEventKindCode::Signal, Tier::Conviction),
            Ok(Lifecycle::ThresholdReached)
        );
    }

    #[test]
    fn tier2_threshold_reached_to_open_via_signal() {
        // Late-arriving Unsignal modeled as a Signal event landing while
        // the poll is in ThresholdReached and conviction has dropped.
        assert_eq!(
            next_lifecycle(
                Lifecycle::ThresholdReached,
                PollEventKindCode::Signal,
                Tier::Conviction
            ),
            Ok(Lifecycle::Open)
        );
    }

    #[test]
    fn tier2_threshold_reached_to_finalized_via_result() {
        // 24h-uncontested finalization.
        assert_eq!(
            next_lifecycle(
                Lifecycle::ThresholdReached,
                PollEventKindCode::PollResult,
                Tier::Conviction
            ),
            Ok(Lifecycle::Finalized)
        );
    }

    #[test]
    fn tier2_delegate_undelegate_preserve_lifecycle() {
        for current in &[Lifecycle::Open, Lifecycle::ThresholdReached] {
            for kind in &[PollEventKindCode::Delegate, PollEventKindCode::Undelegate] {
                assert_eq!(
                    next_lifecycle(*current, *kind, Tier::Conviction),
                    Ok(*current),
                    "tier2 {kind:?} in {current:?} must preserve state"
                );
            }
        }
    }

    // ---------- Tier 2 rejection cases ----------

    #[test]
    fn tier2_draft_to_threshold_reached_rejected() {
        // Draft can only reach ThresholdReached by going Open first.
        assert!(matches!(
            next_lifecycle(
                Lifecycle::Draft,
                PollEventKindCode::Signal,
                Tier::Conviction
            ),
            Err(LifecycleError::IllegalTransition { .. })
        ));
    }

    #[test]
    fn tier2_threshold_reached_to_closed_rejected() {
        // No close path from ThresholdReached — must go via Open revert
        // or via 24h-uncontested PollResult.
        assert!(matches!(
            next_lifecycle(
                Lifecycle::ThresholdReached,
                PollEventKindCode::PollClose,
                Tier::Conviction
            ),
            Err(LifecycleError::IllegalTransition { .. })
        ));
    }

    #[test]
    fn tier2_archived_rejects_signal() {
        // Archived → anything is rejected, including Tier 2 events.
        for kind in &[
            PollEventKindCode::Signal,
            PollEventKindCode::Delegate,
            PollEventKindCode::Undelegate,
        ] {
            assert!(matches!(
                next_lifecycle(Lifecycle::Archived, *kind, Tier::Conviction),
                Err(LifecycleError::IllegalTransition { .. })
            ));
        }
    }

    #[test]
    fn tier2_finalized_rejects_signal() {
        for kind in &[
            PollEventKindCode::Signal,
            PollEventKindCode::Delegate,
            PollEventKindCode::Undelegate,
        ] {
            assert!(matches!(
                next_lifecycle(Lifecycle::Finalized, *kind, Tier::Conviction),
                Err(LifecycleError::IllegalTransition { .. })
            ));
        }
    }

    // ---------- Tier 1 (Approval) cannot enter ThresholdReached ----------

    #[test]
    fn tier1_signal_rejected_in_any_state() {
        // Tier 1 (Approval) polls have no Signal/Delegate/Undelegate
        // semantics — these are Tier 2 only.
        for current in &[
            Lifecycle::Draft,
            Lifecycle::Open,
            Lifecycle::ThresholdReached,
            Lifecycle::Closed,
            Lifecycle::Finalized,
            Lifecycle::Archived,
        ] {
            for kind in &[
                PollEventKindCode::Signal,
                PollEventKindCode::Delegate,
                PollEventKindCode::Undelegate,
            ] {
                assert!(
                    matches!(
                        next_lifecycle(*current, *kind, Tier::Approval),
                        Err(LifecycleError::IllegalTransition { .. })
                    ),
                    "tier1 must reject {kind:?} in {current:?}"
                );
            }
        }
    }

    #[test]
    fn tier1_cannot_enter_threshold_reached() {
        // Even from Open, Tier 1 cannot transition to ThresholdReached.
        // (Signal kind for Tier::Approval is rejected outright.)
        let result = next_lifecycle(Lifecycle::Open, PollEventKindCode::Signal, Tier::Approval);
        assert!(matches!(
            result,
            Err(LifecycleError::IllegalTransition { .. })
        ));
    }

    #[test]
    fn tier3_signal_rejected() {
        // Sortition (Tier 3) does not use Signal/Delegate/Undelegate.
        for kind in &[
            PollEventKindCode::Signal,
            PollEventKindCode::Delegate,
            PollEventKindCode::Undelegate,
        ] {
            assert!(matches!(
                next_lifecycle(Lifecycle::Open, *kind, Tier::Sortition),
                Err(LifecycleError::IllegalTransition { .. })
            ));
        }
    }

    #[test]
    fn tier2_rejects_tier1_only_kinds() {
        // Defense-in-depth (Cursor R4 Low Sev): BallotCast / PollOpen /
        // PollExtend are Tier 1 — Approval semantics — and the
        // lifecycle gate must reject them outright for Tier 2 so
        // forged/malformed events can't slip past apply-time decode.
        for kind in &[
            PollEventKindCode::BallotCast,
            PollEventKindCode::PollOpen,
            PollEventKindCode::PollExtend,
        ] {
            assert!(
                matches!(
                    next_lifecycle(Lifecycle::Open, *kind, Tier::Conviction),
                    Err(LifecycleError::IllegalTransition { .. })
                ),
                "Tier 2 must reject {kind:?} at the lifecycle gate"
            );
        }
        // Sanity: same kinds are still legal for Tier 1.
        for kind in &[
            PollEventKindCode::BallotCast,
            PollEventKindCode::PollOpen,
            PollEventKindCode::PollExtend,
        ] {
            assert_eq!(
                next_lifecycle(Lifecycle::Open, *kind, Tier::Approval),
                Ok(Lifecycle::Open)
            );
        }
    }
}
