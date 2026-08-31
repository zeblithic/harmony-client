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

/// SHA-256 hash of a `DraftCandidate` event's signing bytes, used by
/// `DraftApprovalPayload` to reference the candidate being approved.
pub type CandidateEventHash = [u8; 32];

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

// ---------- Tier 3 payload structs (ZEB-309 Phase 4a-main) ----------

/// Payload for `kd=ss` SortitionSelection: announces the selected mini-public
/// (primary members + backup pool) for a Tier 3 poll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortitionSelectionPayload {
    #[serde(rename = "pi")]
    pub poll_id: PollId,
    #[serde(rename = "pr")]
    pub primary: Vec<OwnerAddr>,
    #[serde(rename = "bk")]
    pub backup: Vec<OwnerAddr>,
}

/// Payload for `kd=ds` DeliberationStatement: a mini-public member's text
/// contribution during the deliberation stage (≤280 chars).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliberationStatementPayload {
    #[serde(rename = "pi")]
    pub poll_id: PollId,
    #[serde(rename = "tx")]
    pub text: String,
}

/// Vote type for `kd=dv` DeliberationVote events. Wire encoding is a single
/// u8 (0=agree, 1=disagree, 2=pass) inside the payload; this enum is the
/// type-safe Rust representation used throughout the engine + IPC layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum BridgingVoteCode {
    Agree = 0,
    Disagree = 1,
    Pass = 2,
}

impl BridgingVoteCode {
    pub fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Agree),
            1 => Some(Self::Disagree),
            2 => Some(Self::Pass),
            _ => None,
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::Agree => "agree",
            Self::Disagree => "disagree",
            Self::Pass => "pass",
        }
    }

    pub fn from_wire_str(s: &str) -> Option<Self> {
        match s {
            "agree" => Some(Self::Agree),
            "disagree" => Some(Self::Disagree),
            "pass" => Some(Self::Pass),
            _ => None,
        }
    }
}

/// Payload for `kd=dv` DeliberationVote: a mini-public member's vote
/// (agree/disagree/pass) on another member's DeliberationStatement.
/// `statement_event_hash` is the SHA-256 of the signing bytes of the
/// referenced `kd=ds` event (32 bytes). `vote` is `BridgingVoteCode::as_u8`.
///
/// All field keys are 2 chars per spec §3 same-length-keys invariant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliberationVotePayload {
    #[serde(rename = "pi")]
    pub poll_id: PollId,
    #[serde(
        rename = "sh",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub statement_event_hash: [u8; 32],
    // Wire-tolerant by spec §2.3: vt is stored as u8 (not a validated enum)
    // so malformed peer events deserialize successfully and are silently
    // dropped at apply time via `BridgingVoteCode::from_u8(...).is_none()`.
    // Adding a validating deserializer here would convert silent drops into
    // `ApplyError::PayloadDecode`, conflicting with the CRDT-tolerant drop
    // semantics the apply layer relies on.
    #[serde(rename = "vt")]
    pub vote: u8,
}

/// Payload for `kd=md` MiniPublicDecline: a selected member declining
/// participation. Optional reason code ≤2 chars; omitted when `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MiniPublicDeclinePayload {
    #[serde(rename = "pi")]
    pub poll_id: PollId,
    #[serde(rename = "rs", skip_serializing_if = "Option::is_none", default)]
    pub reason: Option<String>,
}

/// Payload for `kd=dc` DraftCandidate: a mini-public member submitting a
/// draft proposal text (≤512 chars). Implicit self-approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftCandidatePayload {
    #[serde(rename = "pi")]
    pub poll_id: PollId,
    #[serde(rename = "tx")]
    pub text: String,
}

/// Payload for `kd=da` DraftApproval: a mini-public member approving a
/// peer's draft, referenced by the signing-bytes hash of the DraftCandidate
/// event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftApprovalPayload {
    #[serde(rename = "pi")]
    pub poll_id: PollId,
    #[serde(
        rename = "ch",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub candidate_event_hash: CandidateEventHash,
}

/// Payload for `kd=sf` SortitionFailed: the proposer declares sortition
/// failed (all backup slots exhausted).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortitionFailedPayload {
    #[serde(rename = "pi")]
    pub poll_id: PollId,
}

/// Payload for `kd=rb` RatificationBallot. Overloaded for both privacy
/// modes per ZEB-295 spec §2.1. Mode is determined at apply time from
/// the poll's `privacy_mode` field, NOT from the payload itself.
/// Same-length-keys invariant: all top-level CBOR keys are 2 chars.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RatificationBallotPayload {
    #[serde(rename = "pi")]
    pub poll_id: PollId,
    /// `"pu"`-mode: raw scores 0..=5 per candidate. None in `"se"` mode.
    #[serde(
        rename = "sc",
        default,
        skip_serializing_if = "Option::is_none",
        with = "scores_opt_serde"
    )]
    pub scores: Option<Vec<u8>>,
    /// `"se"`-mode: one ElGamal ciphertext per candidate; len == n.
    #[serde(rename = "cs", default, skip_serializing_if = "Option::is_none")]
    pub ciphertexts_scores: Option<Vec<EncCiphertext>>,
    /// `"se"`-mode: one ElGamal ciphertext per unordered candidate pair
    /// (smaller-index-wins canonical orientation); len == n*(n-1)/2.
    #[serde(rename = "in", default, skip_serializing_if = "Option::is_none")]
    pub ciphertexts_indicators: Option<Vec<EncCiphertext>>,
    /// `"se"`-mode: per-ballot NIZK bundle (range proofs + consistency proofs).
    #[serde(rename = "pf", default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<BallotNIZKProof>,
}

/// Tiny module so the `with = "..."` attribute on `scores` keeps the
/// `serde_bytes` Vec<u8> encoding for Some(...) but elides None entirely.
mod scores_opt_serde {
    use serde::{Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(b) => serde_bytes::serialize(b.as_slice(), s),
            None => unreachable!("skip_serializing_if elides None before reaching here"),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        let b: Vec<u8> = serde_bytes::deserialize(d)?;
        Ok(Some(b))
    }
}

/// ElGamal ciphertext in Ristretto255. Compressed-point encoding per spec §2.3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncCiphertext {
    #[serde(rename = "c1", with = "serde_bytes_32")]
    pub c1: [u8; 32],
    #[serde(rename = "c2", with = "serde_bytes_32")]
    pub c2: [u8; 32],
}

/// Per-ballot NIZK bundle. Concatenated sigma-protocol bytes per spec §4.7.
/// Sizes are deterministic in n (number of candidates).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BallotNIZKProof {
    /// `n` range proofs over {0..5}, 384 B each; total len = 384*n.
    #[serde(rename = "rp", with = "serde_bytes")]
    pub range_proofs: Vec<u8>,
    /// `C(n,2)` consistency proofs, 768 B each; total len = 768*C(n,2).
    #[serde(rename = "ip", with = "serde_bytes")]
    pub consistency_proofs: Vec<u8>,
}

/// Single committee member's per-aggregate decryption share + DLEQ proof.
/// Spec §2.2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TallyShareEntry {
    /// Partial decryption share `d_i = c1_agg * x_i` (compressed Ristretto).
    #[serde(rename = "sh", with = "serde_bytes_32")]
    pub share: [u8; 32],
    /// Chaum-Pedersen DLEQ proof bytes — `(challenge: [u8;32], response: [u8;32])`.
    #[serde(rename = "dp", with = "serde_bytes_64")]
    pub dleq_proof: [u8; 64],
}

/// Payload for `kd=ts` TallyShare. Spec §2.2.
/// Same-length-keys invariant: pi/ce/ts are all 2 chars.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TallySharePayload {
    #[serde(rename = "pi")]
    pub poll_id: PollId,
    /// CHURP rotation generation. Shares from different epochs cannot be mixed.
    ///
    /// `u64` to match `DfrostLogEngine::current_epoch` (the DKG-side source of
    /// truth) and `Tier3PollMeta::community_epoch`. The earlier `u32` shape
    /// was a cross-module type-contract drift (CodeAnt PR #155 critical):
    /// if the CHURP rotation counter ever exceeds `u32::MAX` (4 billion
    /// rotations) the tally-share envelope's epoch silently truncates and
    /// no longer matches the dfrost log's epoch, so the apply path drops
    /// every kd=ts. Even below that horizon, the encoded-as-u32 wire byte
    /// length differs from the encoded-as-u64 length, so a future widening
    /// would break compatibility with already-shipped envelopes — fixing
    /// this NOW, before Phase 6 ships, is the only race-free option.
    #[serde(rename = "ce")]
    pub committee_epoch: u64,
    /// `n + C(n,2)` entries: candidate score-sum entries first, then indicator-sum
    /// entries in unordered-pair lexicographic order. Vec (not fixed array) because
    /// `n` is per-poll and only known at apply time.
    #[serde(rename = "ts")]
    pub entries: Vec<TallyShareEntry>,
}

// Fixed-length byte-array helpers used by EncCiphertext / TallyShareEntry.
mod serde_bytes_32 {
    use serde::{Deserializer, Serializer};
    pub fn serialize<S: Serializer>(b: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        serde_bytes::serialize(b.as_slice(), s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let v: Vec<u8> = serde_bytes::deserialize(d)?;
        v.as_slice()
            .try_into()
            .map_err(|_| serde::de::Error::custom("expected 32 bytes"))
    }
}

mod serde_bytes_64 {
    use serde::{Deserializer, Serializer};
    pub fn serialize<S: Serializer>(b: &[u8; 64], s: S) -> Result<S::Ok, S::Error> {
        serde_bytes::serialize(b.as_slice(), s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 64], D::Error> {
        let v: Vec<u8> = serde_bytes::deserialize(d)?;
        v.as_slice()
            .try_into()
            .map_err(|_| serde::de::Error::custom("expected 64 bytes"))
    }
}

/// Payload for `kd=cl` PollClose events (Tier 3). Wire format is a CBOR
/// map with a single 2-char same-length key `pi` → 32-byte `poll_id`.
/// Per spec §3 same-length-keys invariant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollClosePayload {
    #[serde(rename = "pi")]
    pub poll_id: PollId,
}

/// Payload for `kd=cr` PollCreate when `tier == Tier::Sortition`.
/// Contains the full Tier 3 poll configuration.
///
/// `privacy_mode` values: `"pu"` (public, Phase 4a-main only);
/// `"se"` and `"rf"` are reserved for Phase 6/7 and must not be
/// produced or accepted in Phase 4a-main apply logic.
///
/// `retry_of` is `Some(prev_poll_id)` when this poll is a retry of a
/// failed sortition attempt; omitted on first-try polls
/// (`skip_serializing_if = "Option::is_none"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tier3PollConfigPayload {
    #[serde(rename = "pt")]
    pub proposal_text: String,
    #[serde(rename = "ss")]
    pub sortition_size: u16,
    #[serde(rename = "dw")]
    pub deliberation_window_seconds: u32,
    #[serde(rename = "fw")]
    pub drafting_window_seconds: u32,
    #[serde(rename = "rw")]
    pub ratification_window_seconds: u32,
    #[serde(rename = "pm")]
    pub privacy_mode: String,
    #[serde(rename = "im")]
    pub incentive_mode: String,
    #[serde(rename = "el")]
    pub eligibility: Eligibility,
    #[serde(rename = "ro", skip_serializing_if = "Option::is_none", default)]
    pub retry_of: Option<PollId>,
    /// ZEB-1031 Task 7: `Some(old_poll_id)` when this poll is a relaunch
    /// of a poll voided by a committee reset (spec §7). Omitted on
    /// ordinary polls — optional-key evolution, legacy byte-identical
    /// (mirrors `retry_of`'s addition). Distinct from `retry_of`: a
    /// retry follows a *failed sortition*; a predecessor follows a
    /// *committee reset* voiding an otherwise-live poll.
    #[serde(rename = "pv", skip_serializing_if = "Option::is_none", default)]
    pub predecessor: Option<PollId>,
    /// ZEB-1031 Task 7 review C1: the D-FROST committee epoch active at
    /// mint time, embedded in this SIGNED payload so every reader — this
    /// node's own future replay AND every peer that ingests the event via
    /// `process_inbound`/backfill apply — derives the SAME
    /// `Tier3PollMeta.community_epoch`. Before this field existed, only
    /// the author's own local-mint path patched `community_epoch` after
    /// the fact (`VotingLog::set_tier3_poll_epoch`, called from
    /// `VotingLogEngine::publish_event`'s pre-apply epoch read, which
    /// never runs for peer-ingested creates by design) — every other
    /// device materialized `community_epoch = 0` forever, making the
    /// reset-voiding sweep (spec §7) treat every such poll as pre-reset
    /// regardless of when it was actually created.
    ///
    /// `None` means a pre-Task-7 poll, from before this field existed.
    /// Every materialization path treats an absent `ce` as epoch `0` —
    /// correct by construction: a legacy poll necessarily predates every
    /// reset this scheme can represent, so "voidable by any reset" is the
    /// right disposition, not a fallback to paper over.
    ///
    /// Trust model: this payload is creator-signed, but `ce` is NOT
    /// independently verified against real D-FROST state (unlike a reset
    /// marker's `old_epoch`/`old_vk`, which ARE cryptographically pinned
    /// via `dfrost_reset_digest` + membership evidence — see
    /// `community_dfrost_types::ResetMarkerPayload`). A dishonest `ce`
    /// only mis-dispositions the LIAR'S OWN poll: too low risks an
    /// unwarranted void by a reset that hasn't actually superseded it
    /// (self-inflicted, fixable by relaunch); too high means the
    /// beacon-seed derivation (`derive_beacon_seed(poll_create_event_hash,
    /// community_epoch)`) never matches a real VRF beacon at that epoch,
    /// silently stalling the poll forever (also self-inflicted). Neither
    /// gives an attacker any lever over another member's poll.
    #[serde(rename = "ce", skip_serializing_if = "Option::is_none", default)]
    pub ce: Option<u64>,
}

#[cfg(test)]
mod tier3_payload_tests {
    use super::*;

    #[test]
    fn sortition_selection_round_trip() {
        let payload = SortitionSelectionPayload {
            poll_id: PollId([0x01; 32]),
            primary: vec![OwnerAddr([0xaa; 16]), OwnerAddr([0xbb; 16])],
            backup: vec![OwnerAddr([0xcc; 16])],
        };
        let mut encoded = Vec::new();
        ciborium::into_writer(&payload, &mut encoded).expect("encode");
        let decoded: SortitionSelectionPayload =
            ciborium::from_reader(&encoded[..]).expect("decode");
        assert_eq!(payload, decoded);
    }

    #[test]
    fn deliberation_statement_round_trip() {
        let payload = DeliberationStatementPayload {
            poll_id: PollId([0x02; 32]),
            text: "We should consider option A carefully.".into(),
        };
        let mut encoded = Vec::new();
        ciborium::into_writer(&payload, &mut encoded).expect("encode");
        let decoded: DeliberationStatementPayload =
            ciborium::from_reader(&encoded[..]).expect("decode");
        assert_eq!(payload, decoded);
    }

    #[test]
    fn deliberation_vote_payload_round_trip() {
        let payload = DeliberationVotePayload {
            poll_id: PollId([0xAB; 32]),
            statement_event_hash: [0xCD; 32],
            vote: BridgingVoteCode::Agree.as_u8(),
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&payload, &mut buf).expect("encode");
        let decoded: DeliberationVotePayload = ciborium::de::from_reader(&buf[..]).expect("decode");
        assert_eq!(decoded, payload);
    }

    #[test]
    fn deliberation_vote_payload_all_three_vote_codes_round_trip() {
        for code in [
            BridgingVoteCode::Agree,
            BridgingVoteCode::Disagree,
            BridgingVoteCode::Pass,
        ] {
            let payload = DeliberationVotePayload {
                poll_id: PollId([1; 32]),
                statement_event_hash: [2; 32],
                vote: code.as_u8(),
            };
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&payload, &mut buf).expect("encode");
            let decoded: DeliberationVotePayload =
                ciborium::de::from_reader(&buf[..]).expect("decode");
            assert_eq!(decoded.vote, code.as_u8());
            assert_eq!(BridgingVoteCode::from_u8(decoded.vote), Some(code));
        }
    }

    #[test]
    fn bridging_vote_code_from_u8_rejects_out_of_range() {
        assert_eq!(BridgingVoteCode::from_u8(3), None);
        assert_eq!(BridgingVoteCode::from_u8(255), None);
    }

    #[test]
    fn bridging_vote_code_wire_str_round_trip() {
        for code in [
            BridgingVoteCode::Agree,
            BridgingVoteCode::Disagree,
            BridgingVoteCode::Pass,
        ] {
            assert_eq!(
                BridgingVoteCode::from_wire_str(code.as_wire_str()),
                Some(code)
            );
        }
        assert_eq!(BridgingVoteCode::from_wire_str("foo"), None);
    }

    #[test]
    fn mini_public_decline_with_reason_round_trip() {
        let payload = MiniPublicDeclinePayload {
            poll_id: PollId([0x03; 32]),
            reason: Some("b1".into()),
        };
        let mut encoded = Vec::new();
        ciborium::into_writer(&payload, &mut encoded).expect("encode");
        let decoded: MiniPublicDeclinePayload =
            ciborium::from_reader(&encoded[..]).expect("decode");
        assert_eq!(payload, decoded);
    }

    #[test]
    fn mini_public_decline_no_reason_omits_rs_field() {
        let payload = MiniPublicDeclinePayload {
            poll_id: PollId([0x03; 32]),
            reason: None,
        };
        let mut encoded = Vec::new();
        ciborium::into_writer(&payload, &mut encoded).expect("encode");
        let value: ciborium::Value = ciborium::from_reader(&encoded[..]).expect("decode as value");
        let map = value.as_map().expect("map");
        assert!(
            !map.iter()
                .any(|(k, _): &(ciborium::Value, ciborium::Value)| k.as_text() == Some("rs")),
            "rs field must be omitted when reason is None"
        );
        let decoded: MiniPublicDeclinePayload =
            ciborium::from_reader(&encoded[..]).expect("round-trip decode");
        assert_eq!(payload, decoded);
    }

    #[test]
    fn draft_candidate_round_trip() {
        let payload = DraftCandidatePayload {
            poll_id: PollId([0x04; 32]),
            text: "This is a draft proposal.".into(),
        };
        let mut encoded = Vec::new();
        ciborium::into_writer(&payload, &mut encoded).expect("encode");
        let decoded: DraftCandidatePayload = ciborium::from_reader(&encoded[..]).expect("decode");
        assert_eq!(payload, decoded);
    }

    #[test]
    fn draft_approval_round_trip() {
        let payload = DraftApprovalPayload {
            poll_id: PollId([0x05; 32]),
            candidate_event_hash: [0xde; 32],
        };
        let mut encoded = Vec::new();
        ciborium::into_writer(&payload, &mut encoded).expect("encode");
        let decoded: DraftApprovalPayload = ciborium::from_reader(&encoded[..]).expect("decode");
        assert_eq!(payload, decoded);
    }

    #[test]
    fn sortition_failed_round_trip() {
        let payload = SortitionFailedPayload {
            poll_id: PollId([0x06; 32]),
        };
        let mut encoded = Vec::new();
        ciborium::into_writer(&payload, &mut encoded).expect("encode");
        let decoded: SortitionFailedPayload = ciborium::from_reader(&encoded[..]).expect("decode");
        assert_eq!(payload, decoded);
    }

    #[test]
    fn ratification_ballot_round_trip() {
        let payload = RatificationBallotPayload {
            poll_id: PollId([0x07; 32]),
            scores: Some(vec![5, 3, 1, 0, 4]),
            ciphertexts_scores: None,
            ciphertexts_indicators: None,
            proof: None,
        };
        let mut encoded = Vec::new();
        ciborium::into_writer(&payload, &mut encoded).expect("encode");
        let decoded: RatificationBallotPayload =
            ciborium::from_reader(&encoded[..]).expect("decode");
        assert_eq!(payload, decoded);
    }

    #[test]
    fn tier3_poll_config_round_trip_no_retry() {
        let payload = Tier3PollConfigPayload {
            proposal_text: "Shall we adopt the new governance structure?".into(),
            sortition_size: 15,
            deliberation_window_seconds: 604_800,
            drafting_window_seconds: 259_200,
            ratification_window_seconds: 259_200,
            privacy_mode: "pu".into(),
            incentive_mode: "a".into(),
            eligibility: Eligibility {
                min_power: 1,
                min_vouching_depth: None,
                sortition_size: None,
            },
            retry_of: None,
            predecessor: None,
            ce: None,
        };
        let mut encoded = Vec::new();
        ciborium::into_writer(&payload, &mut encoded).expect("encode");
        // Verify `ro` is omitted when retry_of is None.
        let value: ciborium::Value = ciborium::from_reader(&encoded[..]).expect("decode as value");
        let map = value.as_map().expect("map");
        assert!(
            !map.iter()
                .any(|(k, _): &(ciborium::Value, ciborium::Value)| k.as_text() == Some("ro")),
            "ro field must be omitted when retry_of is None"
        );
        let decoded: Tier3PollConfigPayload =
            ciborium::from_reader(&encoded[..]).expect("round-trip decode");
        assert_eq!(payload, decoded);
    }

    #[test]
    fn tier3_poll_config_round_trip_with_retry() {
        let prev_poll = PollId([0xf0; 32]);
        let payload = Tier3PollConfigPayload {
            proposal_text: "Retry: shall we adopt the new governance structure?".into(),
            sortition_size: 15,
            deliberation_window_seconds: 604_800,
            drafting_window_seconds: 259_200,
            ratification_window_seconds: 259_200,
            privacy_mode: "pu".into(),
            incentive_mode: "b".into(),
            eligibility: Eligibility {
                min_power: 1,
                min_vouching_depth: None,
                sortition_size: None,
            },
            retry_of: Some(prev_poll),
            predecessor: None,
            ce: None,
        };
        let mut encoded = Vec::new();
        ciborium::into_writer(&payload, &mut encoded).expect("encode");
        // Verify `ro` is present when retry_of is Some.
        let value: ciborium::Value = ciborium::from_reader(&encoded[..]).expect("decode as value");
        let map = value.as_map().expect("map");
        assert!(
            map.iter()
                .any(|(k, _): &(ciborium::Value, ciborium::Value)| k.as_text() == Some("ro")),
            "ro field must be present when retry_of is Some"
        );
        let decoded: Tier3PollConfigPayload =
            ciborium::from_reader(&encoded[..]).expect("round-trip decode");
        assert_eq!(payload, decoded);
    }

    // ── ZEB-295 Phase 6 (Tier 3c ballot-secret) wire-format tests ───────────

    #[test]
    fn ratification_ballot_payload_pu_mode_round_trips() {
        let payload = RatificationBallotPayload {
            poll_id: PollId([0x11; 32]),
            scores: Some(vec![5, 3, 1, 0, 4]),
            ciphertexts_scores: None,
            ciphertexts_indicators: None,
            proof: None,
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&payload, &mut buf).expect("encode");
        let decoded: RatificationBallotPayload = ciborium::from_reader(&buf[..]).expect("decode");
        assert_eq!(payload, decoded);
    }

    #[test]
    fn ratification_ballot_payload_se_mode_round_trips() {
        let payload = RatificationBallotPayload {
            poll_id: PollId([0x22; 32]),
            scores: None,
            ciphertexts_scores: Some(vec![
                EncCiphertext {
                    c1: [0xAA; 32],
                    c2: [0xBB; 32]
                };
                3
            ]),
            ciphertexts_indicators: Some(vec![
                EncCiphertext {
                    c1: [0xCC; 32],
                    c2: [0xDD; 32]
                };
                3
            ]),
            proof: Some(BallotNIZKProof {
                range_proofs: vec![0xEE; crate::community_voting_tier3_nizk::Range5Proof::SIZE * 3],
                consistency_proofs: vec![
                    0xFF;
                    crate::community_voting_tier3_nizk::ConsistencyProof::SIZE
                        * 3
                ],
            }),
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&payload, &mut buf).expect("encode");
        let decoded: RatificationBallotPayload = ciborium::from_reader(&buf[..]).expect("decode");
        assert_eq!(payload, decoded);
    }

    #[test]
    fn ratification_ballot_payload_pu_mode_omits_se_keys() {
        // skip_serializing_if on Option-fields must elide them from the wire.
        let payload = RatificationBallotPayload {
            poll_id: PollId([0; 32]),
            scores: Some(vec![5]),
            ciphertexts_scores: None,
            ciphertexts_indicators: None,
            proof: None,
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&payload, &mut buf).expect("encode");
        let value: ciborium::Value = ciborium::from_reader(&buf[..]).expect("decode value");
        let map = value.as_map().expect("map");
        assert_eq!(map.len(), 2, "pu-mode payload must have exactly {{pi, sc}}");
    }

    #[test]
    fn ratification_ballot_payload_se_mode_omits_sc_key() {
        let payload = RatificationBallotPayload {
            poll_id: PollId([0; 32]),
            scores: None,
            ciphertexts_scores: Some(vec![EncCiphertext {
                c1: [0; 32],
                c2: [0; 32],
            }]),
            ciphertexts_indicators: Some(vec![EncCiphertext {
                c1: [0; 32],
                c2: [0; 32],
            }]),
            proof: Some(BallotNIZKProof {
                range_proofs: vec![0; 384],
                consistency_proofs: vec![0; 768],
            }),
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&payload, &mut buf).expect("encode");
        let value: ciborium::Value = ciborium::from_reader(&buf[..]).expect("decode value");
        let map = value.as_map().expect("map");
        assert_eq!(
            map.len(),
            4,
            "se-mode payload must have exactly {{pi, cs, in, pf}}"
        );
        let keys: std::collections::BTreeSet<&str> = map
            .iter()
            .map(|(k, _): &(ciborium::Value, ciborium::Value)| k.as_text().expect("text key"))
            .collect();
        assert_eq!(
            keys,
            std::collections::BTreeSet::from(["pi", "cs", "in", "pf"])
        );
    }

    #[test]
    fn tally_share_payload_round_trips() {
        let payload = TallySharePayload {
            poll_id: PollId([0x33; 32]),
            committee_epoch: 7,
            entries: vec![
                TallyShareEntry {
                    share: [0xA1; 32],
                    dleq_proof: [0xB2; 64],
                },
                TallyShareEntry {
                    share: [0xC3; 32],
                    dleq_proof: [0xD4; 64],
                },
            ],
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&payload, &mut buf).expect("encode");
        let decoded: TallySharePayload = ciborium::from_reader(&buf[..]).expect("decode");
        assert_eq!(payload, decoded);
    }

    #[test]
    fn tally_share_payload_top_keys_are_two_char() {
        let payload = TallySharePayload {
            poll_id: PollId([0; 32]),
            committee_epoch: 0,
            entries: vec![],
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&payload, &mut buf).expect("encode");
        let value: ciborium::Value = ciborium::from_reader(&buf[..]).expect("decode value");
        for (k, _) in value.as_map().expect("map").iter() {
            let s = k.as_text().expect("text key");
            assert_eq!(
                s.len(),
                2,
                "TallySharePayload key {s:?} violates 2-char invariant"
            );
        }
    }
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
    // Tier 3 (Sortition) kinds — added Phase 4a-main (ZEB-309).
    #[serde(rename = "ss")]
    SortitionSelection,
    #[serde(rename = "ds")]
    DeliberationStatement,
    /// kd=dv DeliberationVote — mini-public member agrees/disagrees/passes
    /// on another member's DeliberationStatement.
    #[serde(rename = "dv")]
    DeliberationVote,
    #[serde(rename = "md")]
    MiniPublicDecline,
    #[serde(rename = "dc")]
    DraftCandidate,
    #[serde(rename = "da")]
    DraftApproval,
    #[serde(rename = "sf")]
    SortitionFailed,
    #[serde(rename = "rb")]
    RatificationBallot,
    // Tier 3c (ballot-secret) kind — added Phase 6 (ZEB-295). Committee
    // member's partial decryption share + DLEQ proof; one per aggregate
    // ciphertext after the ratification window closes.
    #[serde(rename = "ts")]
    TallyShare,
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
            // Tier 3 (Sortition) variants added Phase 4a-main (ZEB-309).
            // Cluster 7 fix (CodeRabbit major, R1 bot review): cover all 7
            // new variants so wire-string renames surface immediately.
            PollEventKindCode::SortitionSelection,
            PollEventKindCode::DeliberationStatement,
            PollEventKindCode::DeliberationVote,
            PollEventKindCode::MiniPublicDecline,
            PollEventKindCode::DraftCandidate,
            PollEventKindCode::DraftApproval,
            PollEventKindCode::SortitionFailed,
            PollEventKindCode::RatificationBallot,
            // Phase 6 (ZEB-295): ballot-secret tally share.
            PollEventKindCode::TallyShare,
        ] {
            let mut encoded = Vec::new();
            ciborium::into_writer(kind, &mut encoded).expect("encode");
            let decoded: PollEventKindCode = ciborium::from_reader(&encoded[..]).expect("decode");
            assert_eq!(*kind, decoded);
        }
    }

    /// Pin Tier 3 wire codes so a future enum rename can't silently change them.
    /// Cluster 7 fix (CodeRabbit major, R1 bot review): matches the existing
    /// Tier 2 wire-string pin test pattern.
    #[test]
    fn tier3_kind_codes_have_expected_wire_strings() {
        let cases = [
            (PollEventKindCode::SortitionSelection, "ss"),
            (PollEventKindCode::DeliberationStatement, "ds"),
            (PollEventKindCode::DeliberationVote, "dv"),
            (PollEventKindCode::MiniPublicDecline, "md"),
            (PollEventKindCode::DraftCandidate, "dc"),
            (PollEventKindCode::DraftApproval, "da"),
            (PollEventKindCode::SortitionFailed, "sf"),
            (PollEventKindCode::RatificationBallot, "rb"),
            // Phase 6 (ZEB-295): ballot-secret tally share.
            (PollEventKindCode::TallyShare, "ts"),
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

/// Resolves `OwnerAddr` (16-byte truncated hash) to the full 64-byte
/// composite identity (X25519 || Ed25519) needed for signature verification
/// on inbound voting events. Production impl reads from `harmony_identity`
/// state; tests use a fixed `HashMap`-backed resolver. Mirrors
/// `ChannelIdentityResolver` exactly so the production
/// `OwnerDeviceCacheResolver` adapter can be reused without conversion.
///
/// `verify_voting_event` re-derives `address_hash` from these bytes and
/// rejects if it doesn't match `event.actor.0` — defends against resolver
/// bugs that could attribute valid signatures to wrong owners.
#[async_trait::async_trait]
pub trait VotingIdentityResolver: Send + Sync {
    /// Look up the 64-byte composite identity (X25519 || Ed25519) for
    /// `owner`. Returns `None` if the owner is not known to this node.
    /// Mirrors `ChannelIdentityResolver` exactly so the production
    /// `OwnerDeviceCacheResolver` adapter can be reused without
    /// conversion.
    ///
    /// `verify_voting_event` re-derives `address_hash` from these bytes
    /// and rejects if it doesn't match `event.actor.0` — defends against
    /// resolver bugs that could attribute valid signatures to wrong
    /// owners.
    async fn resolve(&self, owner: &OwnerAddr) -> Option<[u8; 64]>;
}

/// Why a voting event failed verification.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VotingVerifyError {
    #[error("actor not in membership snapshot")]
    ActorNotInMembership,
    #[error("identity not resolvable for actor (resolver returned None)")]
    IdentityNotResolvable,
    #[error("identity-pubkey-to-actor binding mismatch (resolver returned key for wrong owner)")]
    ActorAddressMismatch,
    #[error("invalid Ed25519 signature")]
    InvalidSignature,
    #[error("malformed event (signing_bytes encode failed)")]
    MalformedEvent,
    #[error("signature length is not 64 bytes")]
    BadSignatureLength,
}

/// Verify an inbound voting event:
///   1. Actor is in the membership snapshot (V6 per spec §8).
///   2. Resolver returns 64-byte composite identity (X25519 || Ed25519) for actor.
///   3. Defense-in-depth: re-derive canonical address_hash from the resolved
///      bytes and compare to `event.actor.0` — catches resolver bugs, cache
///      lookup poisoning, or malicious peer substitution. (Voting still binds
///      identity via the resolver; the channel-log layer dropped resolver-based
///      binding in ZEB-399 and now authenticates posts against each author's
///      materialized enrolled device keys instead.)
///   4. The Ed25519 signature on the envelope's `signing_bytes()` is verified
///      with `verify_strict` (RFC 8032 strict subset) against the binding-checked
///      verifying key. (channel-log also verifies with `verify_strict`, but
///      against membership-enrolled keys rather than a resolver-bound key.)
///
/// Eligibility is NOT checked here — apply layer handles that with the
/// same snapshot via `check_eligibility`.
pub async fn verify_voting_event(
    event: &SignedVotingEvent,
    snapshot: &MembershipSnapshot,
    resolver: &dyn VotingIdentityResolver,
) -> Result<(), VotingVerifyError> {
    // V6: actor must be in the membership snapshot.
    if !snapshot.members.contains_key(&event.actor) {
        return Err(VotingVerifyError::ActorNotInMembership);
    }

    // Resolve actor → 64-byte composite identity.
    let identity_bytes = resolver
        .resolve(&event.actor)
        .await
        .ok_or(VotingVerifyError::IdentityNotResolvable)?;

    // Defense-in-depth: re-derive the canonical address_hash from the
    // resolver's returned bytes and compare to the claimed actor. Catches
    // resolver bugs, cache lookup poisoning, or malicious peer
    // substitution — even a valid Ed25519 signature is rejected if the
    // identity doesn't bind to the claimed owner. (Channel-log dropped this
    // resolver binding in ZEB-399; it now verifies against membership-enrolled
    // device keys.)
    let identity = harmony_identity::Identity::from_public_bytes(&identity_bytes)
        .map_err(|_| VotingVerifyError::ActorAddressMismatch)?;
    if identity.address_hash != event.actor.0 {
        return Err(VotingVerifyError::ActorAddressMismatch);
    }

    // Reconstruct signing bytes.
    let sb = event
        .signing_bytes()
        .map_err(|_| VotingVerifyError::MalformedEvent)?;

    // Sig length check.
    let sig_bytes: [u8; 64] = event
        .sig
        .clone()
        .try_into()
        .map_err(|_| VotingVerifyError::BadSignatureLength)?;
    let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);

    // Verify signature against the binding-checked Ed25519 verifying
    // key. Use verify_strict (RFC 8032 strict subset) to match
    // community_channel_log's verify_channel_event.
    identity
        .verifying_key
        .verify_strict(&sb, &sig)
        .map_err(|_| VotingVerifyError::InvalidSignature)
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

/// Build a fully-signed `kd=cr` PollCreate event for Tier 3 (Sortition).
///
/// Used by the `voting_create_tier3_proposal` IPC and any test fixture that
/// needs to mint a Tier 3 PollCreate event. Tier is hardcoded to
/// `Tier::Sortition` (parity with `build_signed_poll_create_tier1`).
pub fn build_signed_poll_create_tier3(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    config: &Tier3PollConfigPayload,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let mut payload = Vec::new();
    ciborium::ser::into_writer(config, &mut payload).map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
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

/// Build a fully-signed `kd=ds` DeliberationStatement event.
pub fn build_signed_deliberation_statement(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    poll_id: PollId,
    text: String,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let payload_struct = DeliberationStatementPayload { poll_id, text };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&payload_struct, &mut payload)
        .map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::DeliberationStatement,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
}

/// Build a fully-signed `kd=dv` DeliberationVote event.
pub fn build_signed_deliberation_vote(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    poll_id: PollId,
    statement_event_hash: [u8; 32],
    vote: BridgingVoteCode,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let payload_struct = DeliberationVotePayload {
        poll_id,
        statement_event_hash,
        vote: vote.as_u8(),
    };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&payload_struct, &mut payload)
        .map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::DeliberationVote,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
}

/// Build a fully-signed `kd=md` MiniPublicDecline event.
pub fn build_signed_mini_public_decline(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    poll_id: PollId,
    reason: Option<String>,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let payload_struct = MiniPublicDeclinePayload { poll_id, reason };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&payload_struct, &mut payload)
        .map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::MiniPublicDecline,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
}

/// Build a fully-signed `kd=dc` DraftCandidate event.
pub fn build_signed_draft_candidate(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    poll_id: PollId,
    text: String,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let payload_struct = DraftCandidatePayload { poll_id, text };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&payload_struct, &mut payload)
        .map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::DraftCandidate,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
}

/// Build a fully-signed `kd=da` DraftApproval event.
pub fn build_signed_draft_approval(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    poll_id: PollId,
    candidate_event_hash: CandidateEventHash,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let payload_struct = DraftApprovalPayload {
        poll_id,
        candidate_event_hash,
    };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&payload_struct, &mut payload)
        .map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::DraftApproval,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
}

/// Build a fully-signed `kd=rb` RatificationBallot event in pu-mode
/// (raw scores 0..=5). Delegates to `build_signed_ratification_ballot_payload`
/// with the pu-mode payload pre-built so existing callers don't need to know
/// about the new se-mode variant.
pub fn build_signed_ratification_ballot(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    poll_id: PollId,
    scores: Vec<u8>,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    let payload_struct = RatificationBallotPayload {
        poll_id,
        scores: Some(scores),
        ciphertexts_scores: None,
        ciphertexts_indicators: None,
        proof: None,
    };
    build_signed_ratification_ballot_payload(keypair, actor, payload_struct, hlc)
}

/// ZEB-295 Phase 6 Task 9: build a fully-signed `kd=rb` RatificationBallot
/// event from a pre-built `RatificationBallotPayload`. Mirrors the pattern
/// used by `build_signed_tally_share` — the se-mode IPC path constructs the
/// payload (encrypt + NIZK) and then hands it to this helper for envelope
/// assembly + signing.
pub fn build_signed_ratification_ballot_payload(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    payload_struct: RatificationBallotPayload,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&payload_struct, &mut payload)
        .map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::RatificationBallot,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
}

/// Build a fully-signed `kd=ss` SortitionSelection event (Tier 3).
///
/// Exposed so integration tests can create properly-signed kd=ss events
/// for the cross-engine bridge path, enabling end-to-end verification
/// of inbound apply through real signatures. Engine-auto-emitted kd=ss
/// in PR 1 is dormant (Tier 3 IPCs still apply directly to VotingLog);
/// PR 2 will route IPCs through `engine.publish_event` so engine-auto
/// kd=ss mints with the local signing key.
pub fn build_signed_sortition_selection(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    poll_id: PollId,
    primary: Vec<OwnerAddr>,
    backup: Vec<OwnerAddr>,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let payload_struct = SortitionSelectionPayload {
        poll_id,
        primary,
        backup,
    };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&payload_struct, &mut payload)
        .map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::SortitionSelection,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
}

pub fn build_signed_sortition_failed(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    poll_id: PollId,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let payload_struct = SortitionFailedPayload { poll_id };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&payload_struct, &mut payload)
        .map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::SortitionFailed,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
}

/// Build a fully-signed `kd=cl` PollClose event (Tier 3).
pub fn build_signed_poll_close_tier3(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    poll_id: PollId,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let payload_struct = PollClosePayload { poll_id };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&payload_struct, &mut payload)
        .map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::PollClose,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
}

/// Build a fully-signed `kd=rs` PollResult event (Tier 3).
pub fn build_signed_poll_result_tier3(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    poll_id: PollId,
    result: crate::community_voting_star::StarResult,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let payload_struct = crate::community_voting_tier3::Tier3PollResultPayload { poll_id, result };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&payload_struct, &mut payload)
        .map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::PollResult,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
}

/// Build a fully-signed `kd=ts` TallyShare event (Tier 3, ZEB-295 Phase 6).
///
/// Used by the engine-auto `maybe_emit_tally_share` hook on committee
/// members after ratification close. The payload bundles `n + C(n,2)`
/// partial-decryption shares + DLEQ proofs against a single CHURP epoch.
pub fn build_signed_tally_share(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    payload_struct: TallySharePayload,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&payload_struct, &mut payload)
        .map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::TallyShare,
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
    use crate::community_voting_star::{CandidateRef, StarResult};
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    fn verify_sig(keypair: &SigningKey, ev: &SignedVotingEvent) {
        let sb = ev.signing_bytes().expect("signing bytes");
        let sig_bytes: [u8; 64] = ev.sig.clone().try_into().expect("sig len");
        let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
        use ed25519_dalek::Verifier;
        keypair.verifying_key().verify(&sb, &sig).expect("verify");
    }

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
        verify_sig(&keypair, &ev);
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
        verify_sig(&keypair, &ev);
    }

    #[test]
    fn signed_tier3_poll_create_round_trip() {
        let keypair = SigningKey::generate(&mut OsRng);
        let actor = OwnerAddr([0x33; 16]);
        let cfg = Tier3PollConfigPayload {
            proposal_text: "test proposal".into(),
            sortition_size: 20,
            deliberation_window_seconds: 600,
            drafting_window_seconds: 600,
            ratification_window_seconds: 600,
            privacy_mode: "pu".into(),
            incentive_mode: "dp".into(),
            eligibility: Eligibility {
                min_power: 0,
                min_vouching_depth: None,
                sortition_size: Some(20),
            },
            retry_of: None,
            predecessor: None,
            ce: None,
        };
        let hlc = Hlc {
            wall_ms: 1,
            logical: 0,
            device_id: "d".into(),
        };
        let ev = build_signed_poll_create_tier3(&keypair, actor, &cfg, hlc).expect("build");
        assert_eq!(ev.kind, PollEventKindCode::PollCreate);
        assert_eq!(ev.tier, Tier::Sortition);
        verify_sig(&keypair, &ev);
    }

    #[test]
    fn signed_deliberation_statement_round_trip() {
        let keypair = SigningKey::generate(&mut OsRng);
        let actor = OwnerAddr([0x44; 16]);
        let pid = PollId([0x55; 32]);
        let hlc = Hlc {
            wall_ms: 2,
            logical: 0,
            device_id: "d".into(),
        };
        let ev = build_signed_deliberation_statement(&keypair, actor, pid, "hello".into(), hlc)
            .expect("build");
        assert_eq!(ev.kind, PollEventKindCode::DeliberationStatement);
        verify_sig(&keypair, &ev);
    }

    #[test]
    fn signed_deliberation_vote_round_trip() {
        let keypair = SigningKey::generate(&mut OsRng);
        let actor = OwnerAddr([0x55; 16]);
        let pid = PollId([0x66; 32]);
        let seh: [u8; 32] = [0x77; 32];
        let hlc = Hlc {
            wall_ms: 3,
            logical: 0,
            device_id: "d".into(),
        };
        let ev =
            build_signed_deliberation_vote(&keypair, actor, pid, seh, BridgingVoteCode::Agree, hlc)
                .expect("build");
        assert_eq!(ev.kind, PollEventKindCode::DeliberationVote);
        verify_sig(&keypair, &ev);
        let decoded: DeliberationVotePayload =
            ciborium::de::from_reader(&ev.payload[..]).expect("decode payload");
        assert_eq!(decoded.poll_id, pid);
        assert_eq!(decoded.statement_event_hash, seh);
        assert_eq!(
            BridgingVoteCode::from_u8(decoded.vote),
            Some(BridgingVoteCode::Agree)
        );
    }

    #[test]
    fn signed_mini_public_decline_round_trip() {
        let keypair = SigningKey::generate(&mut OsRng);
        let actor = OwnerAddr([0x66; 16]);
        let pid = PollId([0x77; 32]);
        let hlc = Hlc {
            wall_ms: 3,
            logical: 0,
            device_id: "d".into(),
        };
        let ev = build_signed_mini_public_decline(&keypair, actor, pid, Some("u1".into()), hlc)
            .expect("build");
        assert_eq!(ev.kind, PollEventKindCode::MiniPublicDecline);
        verify_sig(&keypair, &ev);
    }

    #[test]
    fn signed_draft_candidate_round_trip() {
        let keypair = SigningKey::generate(&mut OsRng);
        let actor = OwnerAddr([0x88; 16]);
        let pid = PollId([0x99; 32]);
        let hlc = Hlc {
            wall_ms: 4,
            logical: 0,
            device_id: "d".into(),
        };
        let ev =
            build_signed_draft_candidate(&keypair, actor, pid, "draft".into(), hlc).expect("build");
        assert_eq!(ev.kind, PollEventKindCode::DraftCandidate);
        verify_sig(&keypair, &ev);
    }

    #[test]
    fn signed_draft_approval_round_trip() {
        let keypair = SigningKey::generate(&mut OsRng);
        let actor = OwnerAddr([0xaa; 16]);
        let pid = PollId([0xbb; 32]);
        let ceh: CandidateEventHash = [0xcc; 32];
        let hlc = Hlc {
            wall_ms: 5,
            logical: 0,
            device_id: "d".into(),
        };
        let ev = build_signed_draft_approval(&keypair, actor, pid, ceh, hlc).expect("build");
        assert_eq!(ev.kind, PollEventKindCode::DraftApproval);
        verify_sig(&keypair, &ev);
    }

    #[test]
    fn signed_ratification_ballot_round_trip() {
        let keypair = SigningKey::generate(&mut OsRng);
        let actor = OwnerAddr([0xdd; 16]);
        let pid = PollId([0xee; 32]);
        let hlc = Hlc {
            wall_ms: 6,
            logical: 0,
            device_id: "d".into(),
        };
        let ev = build_signed_ratification_ballot(&keypair, actor, pid, vec![5, 3, 1], hlc)
            .expect("build");
        assert_eq!(ev.kind, PollEventKindCode::RatificationBallot);
        verify_sig(&keypair, &ev);
    }

    #[test]
    fn signed_sortition_failed_round_trip() {
        let keypair = SigningKey::generate(&mut OsRng);
        let actor = OwnerAddr([0xff; 16]);
        let pid = PollId([0x11; 32]);
        let hlc = Hlc {
            wall_ms: 7,
            logical: 0,
            device_id: "d".into(),
        };
        let ev = build_signed_sortition_failed(&keypair, actor, pid, hlc).expect("build");
        assert_eq!(ev.kind, PollEventKindCode::SortitionFailed);
        verify_sig(&keypair, &ev);
    }

    #[test]
    fn signed_poll_close_tier3_round_trip() {
        let keypair = SigningKey::generate(&mut OsRng);
        let actor = OwnerAddr([0x22; 16]);
        let pid = PollId([0x33; 32]);
        let hlc = Hlc {
            wall_ms: 8,
            logical: 0,
            device_id: "d".into(),
        };
        let ev = build_signed_poll_close_tier3(&keypair, actor, pid, hlc).expect("build");
        assert_eq!(ev.kind, PollEventKindCode::PollClose);
        verify_sig(&keypair, &ev);
    }

    #[test]
    fn signed_poll_result_tier3_round_trip() {
        let keypair = SigningKey::generate(&mut OsRng);
        let actor = OwnerAddr([0x44; 16]);
        let pid = PollId([0x55; 32]);
        let hlc = Hlc {
            wall_ms: 9,
            logical: 0,
            device_id: "d".into(),
        };
        let winner = CandidateRef {
            event_hash: [0xaa; 32],
            approval_count: 0,
        };
        let result = StarResult {
            winner: winner.clone(),
            finalists: vec![winner],
            total_scores: vec![0],
            runoff_votes: vec![0],
        };
        let ev = build_signed_poll_result_tier3(&keypair, actor, pid, result, hlc).expect("build");
        assert_eq!(ev.kind, PollEventKindCode::PollResult);
        verify_sig(&keypair, &ev);
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

#[cfg(test)]
mod voting_verify_tests {
    use super::*;
    use crate::community_membership::ChannelId;
    use crate::community_voting_approval::Tier1PollConfig;
    use crate::owner_state_types::{Hlc, OwnerAddr};
    use ed25519_dalek::SigningKey;
    use std::collections::HashMap;
    use std::sync::Arc;

    struct FixedVotingIdentityResolver {
        map: HashMap<OwnerAddr, [u8; 64]>,
    }

    #[async_trait::async_trait]
    impl VotingIdentityResolver for FixedVotingIdentityResolver {
        async fn resolve(&self, owner: &OwnerAddr) -> Option<[u8; 64]> {
            self.map.get(owner).copied()
        }
    }

    /// Build a `(SigningKey, OwnerAddr, [u8; 64])` triple from a single-byte
    /// seed. The returned `owner`'s `address_hash` is derived from the public
    /// key bytes — the same binding enforced by `verify_voting_event`'s
    /// defense-in-depth check. Mirrors `fixture_identity` in
    /// `community_channel_log.rs`.
    fn fixture_identity(seed: u8) -> (SigningKey, OwnerAddr, [u8; 64]) {
        let priv_id = harmony_identity::PrivateIdentity::from_seed(&[seed; 32]);
        let owner = OwnerAddr(priv_id.identity.address_hash);
        let pub_64 = priv_id.identity.to_public_bytes();
        let private_bytes = priv_id.to_private_bytes();
        let mut ed_secret = [0u8; 32];
        ed_secret.copy_from_slice(&private_bytes[32..64]);
        let signing = ed25519_dalek::SigningKey::from_bytes(&ed_secret);
        (signing, owner, pub_64)
    }

    fn snapshot_of(addrs: &[OwnerAddr]) -> MembershipSnapshot {
        let mut members = HashMap::new();
        for a in addrs {
            members.insert(
                *a,
                MemberAttrs {
                    power: 1,
                    vouching_depth: 1,
                },
            );
        }
        MembershipSnapshot { members }
    }

    fn sample_tier1_event(keypair: &SigningKey, actor: OwnerAddr) -> SignedVotingEvent {
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
            wall_ms: 1,
            logical: 0,
            device_id: "a".into(),
        };
        build_signed_poll_create_tier1(keypair, actor, &cfg, hlc).expect("build")
    }

    #[tokio::test]
    async fn verify_voting_event_accepts_valid_event() {
        let (signing, owner_a, pub_a) = fixture_identity(0xaa);
        let ev = sample_tier1_event(&signing, owner_a);

        let snapshot = snapshot_of(&[owner_a]);
        let resolver = Arc::new(FixedVotingIdentityResolver {
            map: HashMap::from([(owner_a, pub_a)]),
        });

        assert!(verify_voting_event(&ev, &snapshot, resolver.as_ref())
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn verify_voting_event_rejects_actor_not_in_membership() {
        let (signing, owner_bb, pub_bb) = fixture_identity(0xbb);
        let ev = sample_tier1_event(&signing, owner_bb);

        // Snapshot only contains a different actor.
        let (_, other_owner, _) = fixture_identity(0xcc);
        let snapshot = snapshot_of(&[other_owner]);
        let resolver = Arc::new(FixedVotingIdentityResolver {
            map: HashMap::from([(owner_bb, pub_bb)]),
        });

        assert!(matches!(
            verify_voting_event(&ev, &snapshot, resolver.as_ref()).await,
            Err(VotingVerifyError::ActorNotInMembership)
        ));
    }

    #[tokio::test]
    async fn verify_voting_event_rejects_forged_signature() {
        // forger signs with their key but claims real's owner address.
        let (forger_signing, _forger_owner, _forger_pub) = fixture_identity(0xdd);
        let (_, real_owner, real_pub) = fixture_identity(0xee);

        // Event is signed by forger but actor = real_owner.
        let ev = sample_tier1_event(&forger_signing, real_owner);
        let snapshot = snapshot_of(&[real_owner]);
        // Resolver returns real's pub_64 for real_owner — so binding passes
        // but Ed25519 sig (from forger) won't verify against real's verifying key.
        let resolver = Arc::new(FixedVotingIdentityResolver {
            map: HashMap::from([(real_owner, real_pub)]),
        });

        assert!(matches!(
            verify_voting_event(&ev, &snapshot, resolver.as_ref()).await,
            Err(VotingVerifyError::InvalidSignature)
        ));
    }

    #[tokio::test]
    async fn verify_voting_event_rejects_no_resolver_entry() {
        let (signing, owner_ee, _) = fixture_identity(0xee);
        let ev = sample_tier1_event(&signing, owner_ee);

        let snapshot = snapshot_of(&[owner_ee]);
        let resolver = Arc::new(FixedVotingIdentityResolver {
            map: HashMap::new(),
        });

        assert!(matches!(
            verify_voting_event(&ev, &snapshot, resolver.as_ref()).await,
            Err(VotingVerifyError::IdentityNotResolvable)
        ));
    }

    #[tokio::test]
    async fn verify_voting_event_rejects_resolver_address_mismatch() {
        // Resolver maps actor A → pub_64 of identity B. Defense-in-depth
        // catches this even though Ed25519 sig from A would otherwise
        // verify against A's key — the address_hash re-derivation from
        // B's bytes won't match A's owner address.
        let (signing_a, owner_a, _pub_a) = fixture_identity(0x11);
        let (_signing_b, _owner_b, pub_b_64) = fixture_identity(0x22);
        let ev = sample_tier1_event(&signing_a, owner_a);

        let snapshot = snapshot_of(&[owner_a]);
        let resolver = Arc::new(FixedVotingIdentityResolver {
            map: HashMap::from([(owner_a, pub_b_64)]), // WRONG bytes for owner_a
        });

        assert!(matches!(
            verify_voting_event(&ev, &snapshot, resolver.as_ref()).await,
            Err(VotingVerifyError::ActorAddressMismatch)
        ));
    }
}
