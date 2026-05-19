//! ZEB-301 Phase 4a-foundation: wire types for the D-FROST committee event log.
//!
//! Parallels `community_voting_core::SignedVotingEvent`, but the envelope
//! tag is `'d'` (DFrost) instead of `'p'` (Poll) and the kind discriminator
//! covers the five committee-ceremony events (`dr`/`dk`/`ts`/`vb`/`rf`).
//!
//! Same-length-keys invariant: every top-level CBOR map in this module
//! uses 2-character keys so canonical-CBOR encoding stays byte-stable
//! across language reimplementations (see `Hlc` rationale in
//! `owner_state_types`).

use crate::community_membership::RecipientCiphertext;
use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Domain separator used when deriving the VRF seed for a poll.
const VRF_SEED_DS: &[u8] = b"dfrost-vrf-v1";

/// Discriminator for the 5 D-FROST committee event kinds. Wire-encoded
/// as a 2-char string in the envelope's `kd` field.
///
/// * `DkgRound` (`dr`) — DKG round 1 commitments OR round 2 encrypted shares.
/// * `DkgComplete` (`dk`) — finalisation announcement (joint VK + per-member shares).
/// * `ThresholdSign` (`ts`) — per-member contribution to a threshold-signing ceremony.
/// * `VrfBeacon` (`vb`) — aggregated Schnorr signature + derived VRF output.
/// * `ProactiveRefresh` (`rf`) — coordinator-mediated proactive share refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DfrostEventKind {
    #[serde(rename = "dr")]
    DkgRound,
    #[serde(rename = "dk")]
    DkgComplete,
    #[serde(rename = "ts")]
    ThresholdSign,
    #[serde(rename = "vb")]
    VrfBeacon,
    #[serde(rename = "rf")]
    ProactiveRefresh,
}

/// Wire envelope for every D-FROST committee event. Structurally
/// isomorphic to `SignedVotingEvent` but with `tag='d'` and a different
/// kind enum. All 8 keys are 2 characters to satisfy the same-length-keys
/// invariant.
///
/// `tr` (committee_tier) is `0` — it's a placeholder kept for byte-shape
/// parity with `SignedVotingEvent` rather than a real `Tier`. We don't
/// use a real `Tier` here because committee events do not belong to any
/// voting tier; reusing the `Tier` enum would conflate dimensions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedCommitteeEvent {
    #[serde(rename = "tg")]
    pub tag: char,
    #[serde(rename = "vr")]
    pub version: u8,
    #[serde(rename = "tr")]
    pub committee_tier: u8,
    #[serde(rename = "kd")]
    pub kind: DfrostEventKind,
    #[serde(rename = "hc")]
    pub hlc: Hlc,
    #[serde(rename = "ac")]
    pub actor: OwnerAddr,
    /// Opaque CBOR-encoded payload (kind-specific). `serde_bytes` is
    /// load-bearing: without it ciborium encodes `Vec<u8>` as a CBOR
    /// array-of-u8 (major type 4) doubling on-wire size.
    #[serde(rename = "pd", with = "serde_bytes")]
    pub payload: Vec<u8>,
    /// Ed25519 signature over `signing_bytes()`. 64 bytes.
    #[serde(rename = "sg", with = "serde_bytes")]
    pub sig: Vec<u8>,
}

impl SignedCommitteeEvent {
    /// Canonical CBOR bytes the Ed25519 signature covers — all fields
    /// EXCEPT `sg`. Wire-encoded as a 7-key 2-char-key map.
    pub fn signing_bytes(&self) -> Result<Vec<u8>, ciborium::ser::Error<std::io::Error>> {
        #[derive(Serialize)]
        struct SigInput<'a> {
            #[serde(rename = "tg")]
            tag: char,
            #[serde(rename = "vr")]
            version: u8,
            #[serde(rename = "tr")]
            committee_tier: u8,
            #[serde(rename = "kd")]
            kind: DfrostEventKind,
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
            committee_tier: self.committee_tier,
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

/// Payload for `DfrostEventKind::DkgRound` (`dr`). Carries either the
/// public round-1 package (broadcast) or the per-recipient sealed
/// round-2 packages (encrypted DM-style).
///
/// `rn=1` populates `round1_package` and leaves `recipient_ciphertexts`
/// = `None`; `rn=2` does the inverse. The two halves are skipped via
/// `skip_serializing_if` so the on-wire map has exactly 3 keys in both
/// rounds (`ci`/`rn`/`pk` or `ci`/`rn`/`rc`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DkgRoundPayload {
    #[serde(rename = "ci", with = "serde_bytes")]
    pub ceremony_id: [u8; 32],
    #[serde(rename = "rn")]
    pub round_num: u8,
    /// `rn=1` only: ciborium-encoded `dkg::round1::Package` bytes.
    #[serde(
        rename = "pk",
        with = "serde_bytes",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub round1_package: Option<Vec<u8>>,
    /// `rn=2` only: one sealed (X25519) round-2 package per recipient.
    #[serde(rename = "rc", skip_serializing_if = "Option::is_none", default)]
    pub recipient_ciphertexts: Option<Vec<RecipientCiphertext>>,
}

/// Per-member VerifyingShare entry inside `DkgCompletePayload.vs`.
/// 2-key map (`id`/`vk`) to satisfy the same-length-keys invariant at
/// this nesting depth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberVerifyingShare {
    #[serde(rename = "id")]
    pub member: OwnerAddr,
    #[serde(rename = "vk", with = "serde_bytes")]
    pub verifying_share: [u8; 32],
}

/// Payload for `DfrostEventKind::DkgComplete` (`dk`). Announces the
/// finalised joint VerifyingKey plus per-member VerifyingShares. The
/// same payload is reused for proactive-refresh completion (the `vk`
/// MUST equal the existing committee's joint key — enforced by
/// `DfrostLog::apply_dkg_complete`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DkgCompletePayload {
    #[serde(rename = "ci", with = "serde_bytes")]
    pub ceremony_id: [u8; 32],
    #[serde(rename = "vk", with = "serde_bytes")]
    pub joint_verifying_key: [u8; 32],
    #[serde(rename = "vs")]
    pub verifying_shares: Vec<MemberVerifyingShare>,
    #[serde(rename = "ep")]
    pub epoch: u64,
    #[serde(rename = "mb")]
    pub members: Vec<OwnerAddr>,
    #[serde(rename = "th")]
    pub threshold: u16,
    #[serde(rename = "mx")]
    pub max_signers: u16,
}

/// Payload for `DfrostEventKind::ThresholdSign` (`ts`). One per
/// committee member per signing ceremony. Bundles round-1 commitments
/// (`cm`) and round-2 signature share (`sh`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThresholdSignPayload {
    #[serde(rename = "ci", with = "serde_bytes")]
    pub ceremony_id: [u8; 32],
    #[serde(rename = "ms", with = "serde_bytes")]
    pub message_hash: [u8; 32],
    #[serde(rename = "cm", with = "serde_bytes")]
    pub commitment_bytes: Vec<u8>,
    #[serde(rename = "sh", with = "serde_bytes")]
    pub share_bytes: Vec<u8>,
}

/// Payload for `DfrostEventKind::VrfBeacon` (`vb`). Aggregated Schnorr
/// signature plus derived VRF output (`SHA-256(b"dfrost-vrf-v1" || R)`).
///
/// Note: the `sg` key here is the FROST Schnorr signature, NOT the
/// outer envelope's Ed25519 actor signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VrfBeaconPayload {
    #[serde(rename = "ci", with = "serde_bytes")]
    pub ceremony_id: [u8; 32],
    #[serde(rename = "ms", with = "serde_bytes")]
    pub message_hash: [u8; 32],
    /// 64-byte Schnorr signature `R(32) || s(32)`.
    #[serde(rename = "sg", with = "serde_bytes")]
    pub signature: Vec<u8>,
    #[serde(rename = "vf", with = "serde_bytes")]
    pub vrf_output: [u8; 32],
}

/// Payload for `DfrostEventKind::ProactiveRefresh` (`rf`). Carries
/// either coordinator-distributed sealed `SecretShare`s (`rn=1`) or the
/// round-2 refresh DKG package (`rn=2`, fully-distributed mode).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshRoundPayload {
    #[serde(rename = "ci", with = "serde_bytes")]
    pub ceremony_id: [u8; 32],
    #[serde(rename = "rn")]
    pub round_num: u8,
    #[serde(rename = "rc", skip_serializing_if = "Option::is_none", default)]
    pub recipient_ciphertexts: Option<Vec<RecipientCiphertext>>,
    #[serde(
        rename = "pk",
        with = "serde_bytes",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub round2_package: Option<Vec<u8>>,
}

/// Deterministic ceremony-ID derivation:
/// `SHA-256(community_id_bytes || wall_ms_le8 || tag_bytes)`.
///
/// `tag` is a short domain separator (e.g. `b"dkg-v1"` for DKG,
/// `b"rf-v1"` for refresh). Two nodes that independently observe the
/// same trigger derive the same `ceremony_id`.
pub fn derive_ceremony_id(community_id: &SpaceId, wall_ms: u64, tag: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(community_id.0);
    hasher.update(wall_ms.to_le_bytes());
    hasher.update(tag);
    let mut out = [0u8; 32];
    out.copy_from_slice(&hasher.finalize());
    out
}

/// VRF-seed derivation for threshold signing:
/// `SHA-256(b"dfrost-vrf-v1" || poll_event_hash || epoch_le8)`.
///
/// Including the epoch prevents a beacon produced under one committee
/// epoch from being replayed against a refreshed committee that shares
/// the same joint VerifyingKey.
pub fn derive_vrf_seed(poll_event_hash: &[u8; 32], epoch: u64) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(VRF_SEED_DS);
    hasher.update(poll_event_hash);
    hasher.update(epoch.to_le_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&hasher.finalize());
    out
}

/// VRF-output derivation from the aggregated Schnorr nonce point R:
/// `SHA-256(b"dfrost-vrf-v1" || R_compressed)`.
///
/// `r_compressed` is the first 32 bytes of `aggregated_signature.serialize()`
/// — the compressed Ristretto point representation of the nonce R from
/// the FROST signing protocol. Hashing R (not the full signature) makes
/// the output uniformly distributed even when adversarial signers
/// influence `s`.
pub fn derive_vrf_output(r_compressed: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(VRF_SEED_DS);
    hasher.update(r_compressed);
    let mut out = [0u8; 32];
    out.copy_from_slice(&hasher.finalize());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dfrost_kind_codes_are_2_char_strings() {
        for (kind, expected) in [
            (DfrostEventKind::DkgRound, "dr"),
            (DfrostEventKind::DkgComplete, "dk"),
            (DfrostEventKind::ThresholdSign, "ts"),
            (DfrostEventKind::VrfBeacon, "vb"),
            (DfrostEventKind::ProactiveRefresh, "rf"),
        ] {
            let mut buf = Vec::new();
            ciborium::into_writer(&kind, &mut buf).unwrap();
            let val: ciborium::Value = ciborium::from_reader(&buf[..]).unwrap();
            let s = val.as_text().expect("kind encodes as text");
            assert_eq!(s, expected);
            assert_eq!(s.len(), 2);
        }
    }

    #[test]
    fn signed_committee_event_envelope_has_8_two_char_keys() {
        let ev = SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::DkgRound,
            hlc: crate::owner_state_types::Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: crate::owner_state_types::OwnerAddr([0xaa; 16]),
            payload: vec![0x42],
            sig: vec![0u8; 64],
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&ev, &mut buf).unwrap();
        let val: ciborium::Value = ciborium::from_reader(&buf[..]).unwrap();
        let map = val.as_map().expect("map");
        assert_eq!(map.len(), 8);
        for (k, _) in map.iter() {
            let s = k.as_text().expect("key is text");
            assert_eq!(s.len(), 2, "key {s:?} violates 2-char invariant");
        }
    }

    #[test]
    fn signing_bytes_exclude_sg_field() {
        let mut ev = SignedCommitteeEvent {
            tag: 'd',
            version: 1,
            committee_tier: 0,
            kind: DfrostEventKind::DkgComplete,
            hlc: crate::owner_state_types::Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "t".into(),
            },
            actor: crate::owner_state_types::OwnerAddr([0xaa; 16]),
            payload: vec![0xde, 0xad],
            sig: vec![0u8; 64],
        };
        let sb1 = ev.signing_bytes().unwrap();
        ev.sig = vec![0xff; 64];
        let sb2 = ev.signing_bytes().unwrap();
        assert_eq!(sb1, sb2, "signing_bytes must not depend on sig field");
        let val: ciborium::Value = ciborium::from_reader(&sb1[..]).unwrap();
        let map = val.as_map().unwrap();
        assert_eq!(
            map.len(),
            7,
            "signing_bytes must have 7 fields (excludes sg)"
        );
        assert!(!map
            .iter()
            .any(|(k, _): &(ciborium::Value, ciborium::Value)| k.as_text() == Some("sg")));
    }

    #[test]
    fn derive_vrf_seed_is_deterministic() {
        let hash = [0x11u8; 32];
        let epoch = 3u64;
        let s1 = derive_vrf_seed(&hash, epoch);
        let s2 = derive_vrf_seed(&hash, epoch);
        assert_eq!(s1, s2);
        let s3 = derive_vrf_seed(&hash, epoch + 1);
        assert_ne!(s1, s3);
    }
}
