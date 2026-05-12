//! Sub-D Phase 1 — library-federated discovery directory (consumer side).
//!
//! Spec: `docs/specs/2026-05-11-zeb-218-sub-d-library-directory-vertical-slice-design.md`
//!
//! This module subscribes to `harmony/discovery/library/{addr}/communities`
//! topics for each library the user has added, verifies community-admin
//! Ed25519 signatures on incoming `LibraryDirectoryEntry` records, and
//! aggregates entries across libraries with dedupe by `community_id`
//! (latest-HLC-wins).
//!
//! Phase 1 deliberately omits: library auto-discovery (Phase 2),
//! federated republication signatures (Phase 3), ProfileMembershipBroadcast
//! (Phase 4), and direct-join IPC bypassing redeem_invite (Phase 6 /
//! ZEB-252 rewrite). See spec §12.

use serde::{Deserialize, Serialize};

use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

/// Per-entry wire format published by libraries. Spec §4.1.
///
/// 2-char field keys satisfy `canonical_cbor_encode`'s same-length-keys
/// precondition (mirrors all other Sub-A/B/C wire types).
///
/// `community_admin_identity_pub` is the 64-byte (X25519_pub(32) ||
/// Ed25519_pub(32)) identity bundle — the Ed25519 half verifies
/// `community_signature`. The X25519 half is unused in Phase 1 but
/// kept for shape consistency with
/// `CommunityInvitePayload::admin_identity_pub`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryDirectoryEntry {
    #[serde(rename = "cd")]
    pub community_id: SpaceId,

    #[serde(
        rename = "ai",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub community_admin_identity_pub: [u8; 64],

    #[serde(rename = "nm")]
    pub name: String,

    #[serde(rename = "ds")]
    pub description: String,

    #[serde(rename = "tp")]
    pub topics: Vec<String>,

    #[serde(rename = "iu")]
    pub invite_url: String,

    #[serde(rename = "lb")]
    pub listed_by: OwnerAddr,

    #[serde(rename = "la")]
    pub listed_at: Hlc,

    #[serde(
        rename = "cs",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub community_signature: [u8; 64],
}

impl CanonicalPayloadSealed for LibraryDirectoryEntry {}
impl CanonicalPayload for LibraryDirectoryEntry {}

use crate::owner_state_crypto::canonical_cbor_encode;
use ed25519_dalek::Signature;

/// Verification error categories. Each surfaces as a warn-level log;
/// the entry is dropped silently from the caller's perspective.
#[derive(Debug, thiserror::Error)]
pub enum EntryVerifyError {
    #[error("malformed admin identity_pub: {0}")]
    InvalidIdentityPub(String),
    #[error("canonical CBOR encode failed: {0}")]
    Encode(#[from] crate::owner_state_crypto::CryptoError),
    #[error("Ed25519 signature verification failed")]
    SignatureInvalid,
    #[error("invite_url is invite-only — directory entries may only carry open-community URLs")]
    InviteOnlyUrl,
    #[error("invite_url failed to parse: {0}")]
    InviteUrlParse(String),
    #[error("name exceeds {MAX_NAME_LEN} bytes")]
    NameTooLong,
    #[error("description exceeds {MAX_DESCRIPTION_LEN} bytes")]
    DescriptionTooLong,
    #[error("topics list exceeds {MAX_TOPICS_PER_ENTRY} entries")]
    TooManyTopics,
    #[error("one or more topics exceeds {MAX_TOPIC_LEN} bytes")]
    TopicTooLong,
}

pub const MAX_NAME_LEN: usize = 200;
pub const MAX_DESCRIPTION_LEN: usize = 2000;
pub const MAX_TOPICS_PER_ENTRY: usize = 16;
pub const MAX_TOPIC_LEN: usize = 64;
pub const MAX_ENTRIES_PER_LIBRARY: usize = 10_000;

/// Verify a `LibraryDirectoryEntry` end-to-end:
/// 1. Anti-spam bounds (name/description/topic lengths)
/// 2. Parse `community_admin_identity_pub` via
///    `harmony_identity::Identity::from_public_bytes` (validates both
///    halves)
/// 3. Verify the Ed25519 signature over canonical-CBOR-encoded fields
///    with `community_signature` zeroed (so verify == sign exactly)
/// 4. Parse `invite_url` and reject if `is_invite_only == true`
pub fn verify_entry(entry: &LibraryDirectoryEntry) -> Result<(), EntryVerifyError> {
    // (1) Bounds
    if entry.name.len() > MAX_NAME_LEN {
        return Err(EntryVerifyError::NameTooLong);
    }
    if entry.description.len() > MAX_DESCRIPTION_LEN {
        return Err(EntryVerifyError::DescriptionTooLong);
    }
    if entry.topics.len() > MAX_TOPICS_PER_ENTRY {
        return Err(EntryVerifyError::TooManyTopics);
    }
    if entry.topics.iter().any(|t| t.len() > MAX_TOPIC_LEN) {
        return Err(EntryVerifyError::TopicTooLong);
    }

    // (2) Parse identity_pub — also rejects malformed point bytes.
    let identity =
        harmony_identity::Identity::from_public_bytes(&entry.community_admin_identity_pub)
            .map_err(|e| EntryVerifyError::InvalidIdentityPub(format!("{e:?}")))?;

    // (3) Verify sig over canonical CBOR with signature field zeroed.
    let mut for_sig = entry.clone();
    for_sig.community_signature = [0u8; 64];
    let signed_bytes = canonical_cbor_encode(&for_sig)?;
    let sig = Signature::from_bytes(&entry.community_signature);
    identity
        .verifying_key
        .verify_strict(&signed_bytes, &sig)
        .map_err(|_| EntryVerifyError::SignatureInvalid)?;

    // (4) Invite-URL discipline — open-community only.
    let payload = crate::community_invite::decode_invite_url(&entry.invite_url)
        .map_err(|e| EntryVerifyError::InviteUrlParse(format!("{e}")))?;
    if payload.is_invite_only {
        return Err(EntryVerifyError::InviteOnlyUrl);
    }

    Ok(())
}

use std::collections::{BTreeMap, BTreeSet};

/// One entry per community_id, deduped across libraries. Spec §4.3.
#[derive(Debug, Clone)]
pub struct AggregatedEntry {
    /// Latest (highest-HLC) entry observed for this community.
    pub entry: LibraryDirectoryEntry,
    /// Set of libraries that have listed this community. Eviction
    /// happens when this set empties (last library un-listed it).
    pub listed_by: BTreeSet<OwnerAddr>,
}

/// In-memory aggregation state. NOT persisted — rebuilt on startup
/// by replaying subscriptions.
#[derive(Debug, Default)]
pub struct Aggregation {
    by_community: BTreeMap<SpaceId, AggregatedEntry>,
    /// Per-library contribution count, to enforce
    /// `MAX_ENTRIES_PER_LIBRARY` (spec §10).
    per_library_count: BTreeMap<OwnerAddr, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnEntryOutcome {
    /// New `community_id` — emit `library-directory-updated`.
    Inserted(SpaceId),
    /// Existing community, replaced by newer-HLC entry.
    Replaced(SpaceId),
    /// Existing community, same/older entry but cross-library listed_by union grew.
    AccretedListedBy(SpaceId),
    /// Drop (older-HLC duplicate from a library that already contributes
    /// the newer entry, or no-op).
    Idempotent,
    /// Cap-eviction triggered: oldest entry from `library` dropped to
    /// make room for the new arrival.
    EvictedThenInserted { evicted: SpaceId, inserted: SpaceId },
}

impl Aggregation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot_all(&self) -> Vec<AggregatedEntry> {
        self.by_community.values().cloned().collect()
    }

    pub fn snapshot_filtered_by_library(&self, library: &OwnerAddr) -> Vec<AggregatedEntry> {
        self.by_community
            .values()
            .filter(|e| e.listed_by.contains(library))
            .cloned()
            .collect()
    }

    pub fn entry_count_for_library(&self, library: &OwnerAddr) -> usize {
        self.per_library_count.get(library).copied().unwrap_or(0)
    }

    /// Process a verified entry. Caller MUST have run `verify_entry`
    /// first — this method does NOT re-verify the signature.
    pub fn on_entry(&mut self, entry: LibraryDirectoryEntry) -> OnEntryOutcome {
        let community_id = entry.community_id;
        let library = entry.listed_by;

        // Cap check BEFORE insert. If this library is already at cap and
        // we're about to add a NEW contribution (not an update), evict
        // the oldest entry from this library first.
        let library_at_cap = self.entry_count_for_library(&library) >= MAX_ENTRIES_PER_LIBRARY;
        let is_new_contribution_for_library = !self
            .by_community
            .get(&community_id)
            .map(|agg| agg.listed_by.contains(&library))
            .unwrap_or(false);

        let mut maybe_evicted: Option<SpaceId> = None;
        if library_at_cap && is_new_contribution_for_library {
            if let Some(oldest_id) = self.find_oldest_for_library(&library) {
                self.evict_library_contribution(&library, oldest_id);
                maybe_evicted = Some(oldest_id);
            }
        }

        let outcome = match self.by_community.get_mut(&community_id) {
            None => {
                // Brand-new community in the aggregation.
                let mut listed_by = BTreeSet::new();
                listed_by.insert(library);
                self.by_community
                    .insert(community_id, AggregatedEntry { entry, listed_by });
                *self.per_library_count.entry(library).or_insert(0) += 1;
                OnEntryOutcome::Inserted(community_id)
            }
            Some(existing) => {
                let incoming_newer = entry
                    .listed_at
                    .is_strictly_newer_than(&existing.entry.listed_at);
                let listed_by_was_new = existing.listed_by.insert(library);
                if listed_by_was_new {
                    *self.per_library_count.entry(library).or_insert(0) += 1;
                }
                if incoming_newer {
                    existing.entry = entry;
                    OnEntryOutcome::Replaced(community_id)
                } else if listed_by_was_new {
                    OnEntryOutcome::AccretedListedBy(community_id)
                } else {
                    OnEntryOutcome::Idempotent
                }
            }
        };

        if let Some(evicted_id) = maybe_evicted {
            // Re-shape outcome to surface the eviction.
            if let OnEntryOutcome::Inserted(new_id) = outcome {
                return OnEntryOutcome::EvictedThenInserted {
                    evicted: evicted_id,
                    inserted: new_id,
                };
            }
        }
        outcome
    }

    /// Remove all contributions from `library`. Walks the entire
    /// aggregation map (O(N over total entries from this library);
    /// the per-library count is bounded by MAX_ENTRIES_PER_LIBRARY).
    /// Spec §5.3.
    pub fn drop_library(&mut self, library: &OwnerAddr) -> Vec<SpaceId> {
        let mut evicted = Vec::new();
        self.by_community.retain(|community_id, agg| {
            if agg.listed_by.remove(library) && agg.listed_by.is_empty() {
                evicted.push(*community_id);
                return false;
            }
            true
        });
        self.per_library_count.remove(library);
        evicted
    }

    fn find_oldest_for_library(&self, library: &OwnerAddr) -> Option<SpaceId> {
        self.by_community
            .iter()
            .filter(|(_, agg)| agg.listed_by.contains(library))
            .min_by(|a, b| {
                // Lexicographic ordering on the HLC tuple. Note we want
                // the OLDEST, so we min on (wall_ms, logical, device_id).
                let ha = (
                    &a.1.entry.listed_at.wall_ms,
                    &a.1.entry.listed_at.logical,
                    a.1.entry.listed_at.device_id.as_str(),
                );
                let hb = (
                    &b.1.entry.listed_at.wall_ms,
                    &b.1.entry.listed_at.logical,
                    b.1.entry.listed_at.device_id.as_str(),
                );
                ha.cmp(&hb)
            })
            .map(|(id, _)| *id)
    }

    fn evict_library_contribution(&mut self, library: &OwnerAddr, community_id: SpaceId) {
        if let Some(agg) = self.by_community.get_mut(&community_id) {
            if agg.listed_by.remove(library) {
                if let Some(c) = self.per_library_count.get_mut(library) {
                    if *c > 0 {
                        *c -= 1;
                    }
                }
                if agg.listed_by.is_empty() {
                    self.by_community.remove(&community_id);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_invite::{
        encode_invite_url, CommunityInvitePayload, InviteEpochSnapshot, MaterializedCommunityState,
    };
    use crate::owner_state_crypto::canonical_cbor_encode;
    use ed25519_dalek::{Signer, SigningKey};

    /// Build a real open-community invite URL via `encode_invite_url`,
    /// so `decode_invite_url` round-trips it back into a parseable
    /// `CommunityInvitePayload` with `is_invite_only == false`.
    fn build_open_invite_url() -> String {
        let payload = CommunityInvitePayload {
            community_id: SpaceId([0u8; 16]),
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: vec![0u8; 32], // open communities use 32-byte sealed_epoch_key
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr: OwnerAddr([0u8; 16]),
            community_name: "test".to_string(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
            admin_bootstrap: None,
            admin_identity_pub: None,
        };
        encode_invite_url(&payload).expect("encode open invite url")
    }

    /// Build an invite-only invite URL — same machinery but with
    /// `is_invite_only == true` so the discipline check rejects it.
    fn build_invite_only_url() -> String {
        use crate::community_invite::InviteToken;
        use crate::community_membership::{MembershipEventKind, SignedMembershipEvent};
        let admin_addr = OwnerAddr([0u8; 16]);
        let community_id = SpaceId([0u8; 16]);
        let admin_bootstrap = SignedMembershipEvent {
            id: [0u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin_addr,
            at: Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "test".to_string(),
            },
            sig: [0u8; 64],
            countersig: None,
        };
        let payload = CommunityInvitePayload {
            community_id,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: vec![0u8; 92], // invite-only: 92-byte sealed_epoch_key
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr,
            community_name: "test-invite-only".to_string(),
            is_invite_only: true,
            expires_at: None,
            invite_token: Some(InviteToken {
                inviter: admin_addr,
                invitee_hint: None,
                minted_at: Hlc {
                    wall_ms: 1_000,
                    logical: 0,
                    device_id: "test".to_string(),
                },
                expires_at: None,
                sig: [0u8; 64],
            }),
            admin_bootstrap: Some(admin_bootstrap),
            admin_identity_pub: Some([0u8; 64]),
        };
        encode_invite_url(&payload).expect("encode invite-only url")
    }

    /// Build a test admin identity_pub from a stable seed.
    fn build_test_identity_pub(ed25519_seed: [u8; 32]) -> (SigningKey, [u8; 64]) {
        let ed_signing = SigningKey::from_bytes(&ed25519_seed);
        let ed_verifying = ed_signing.verifying_key().to_bytes();
        // X25519 half can be any 32 bytes for our purposes — the verifier
        // only consults the Ed25519 half. Use a constant prefix so two
        // different seeds produce distinct identity_pubs.
        let mut identity_pub = [0u8; 64];
        identity_pub[..32].copy_from_slice(&[0x11; 32]);
        identity_pub[32..].copy_from_slice(&ed_verifying);
        (ed_signing, identity_pub)
    }

    fn build_signed_entry(
        community_id: SpaceId,
        admin_seed: [u8; 32],
        listed_by: OwnerAddr,
        listed_at: Hlc,
        invite_url: String,
    ) -> LibraryDirectoryEntry {
        let (signing_key, identity_pub) = build_test_identity_pub(admin_seed);
        let mut entry = LibraryDirectoryEntry {
            community_id,
            community_admin_identity_pub: identity_pub,
            name: "Test Community".to_string(),
            description: "for tests".to_string(),
            topics: vec!["test".to_string()],
            invite_url,
            listed_by,
            listed_at,
            community_signature: [0u8; 64],
        };
        // Sign over canonical CBOR of all fields with community_signature
        // zeroed — verify_entry zeroes the same field before recomputing.
        let bytes = canonical_cbor_encode(&entry).expect("encode for sign");
        let sig = signing_key.sign(&bytes);
        entry.community_signature = sig.to_bytes();
        entry
    }

    #[test]
    fn roundtrip_signed_entry_verifies() {
        let entry = build_signed_entry(
            SpaceId([1; 16]),
            [7; 32],
            OwnerAddr([2; 16]),
            Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "d".into(),
            },
            build_open_invite_url(),
        );
        assert!(verify_entry(&entry).is_ok(), "signed entry must verify");
    }

    #[test]
    fn tampered_payload_rejected() {
        let mut entry = build_signed_entry(
            SpaceId([1; 16]),
            [7; 32],
            OwnerAddr([2; 16]),
            Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "d".into(),
            },
            build_open_invite_url(),
        );
        entry.name = "Tampered".to_string();
        assert!(matches!(
            verify_entry(&entry),
            Err(EntryVerifyError::SignatureInvalid)
        ));
    }

    #[test]
    fn wrong_signing_key_rejected() {
        let mut entry = build_signed_entry(
            SpaceId([1; 16]),
            [7; 32],
            OwnerAddr([2; 16]),
            Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "d".into(),
            },
            build_open_invite_url(),
        );
        // Replace the identity_pub's Ed25519 half with a DIFFERENT key,
        // leaving the sig intact. Verify must reject.
        let (_other_key, other_identity_pub) = build_test_identity_pub([9; 32]);
        entry.community_admin_identity_pub = other_identity_pub;
        assert!(matches!(
            verify_entry(&entry),
            Err(EntryVerifyError::SignatureInvalid)
        ));
    }

    #[test]
    fn malformed_identity_pub_rejected() {
        // Ed25519 half `[0x7F; 32]` doesn't decompress under ed25519-dalek
        // 2.x / curve25519-dalek 4.x — same fixture used by
        // community_invite_unit::rejects_invalid_admin_pubkey.
        let mut bad_identity_pub = [0u8; 64];
        bad_identity_pub[32..].copy_from_slice(&[0x7F; 32]);
        let entry = LibraryDirectoryEntry {
            community_id: SpaceId([0; 16]),
            community_admin_identity_pub: bad_identity_pub,
            name: String::new(),
            description: String::new(),
            topics: vec![],
            invite_url: build_open_invite_url(),
            listed_by: OwnerAddr([0; 16]),
            listed_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: String::new(),
            },
            community_signature: [0u8; 64],
        };
        assert!(matches!(
            verify_entry(&entry),
            Err(EntryVerifyError::InvalidIdentityPub(_))
        ));
    }

    #[test]
    fn name_too_long_rejected() {
        let entry = build_signed_entry(
            SpaceId([1; 16]),
            [7; 32],
            OwnerAddr([2; 16]),
            Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "d".into(),
            },
            build_open_invite_url(),
        );
        let mut bad = entry.clone();
        bad.name = "X".repeat(MAX_NAME_LEN + 1);
        assert!(matches!(
            verify_entry(&bad),
            Err(EntryVerifyError::NameTooLong)
        ));
    }

    /// An invite-only invite URL must be rejected at receive — only
    /// open-community URLs may appear in the directory (spec §4.1, §9).
    #[test]
    fn invite_only_url_rejected() {
        let mut entry = build_signed_entry(
            SpaceId([1; 16]),
            [7; 32],
            OwnerAddr([2; 16]),
            Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "d".into(),
            },
            build_invite_only_url(),
        );
        // re-sign because we changed invite_url
        let mut for_sig = entry.clone();
        for_sig.community_signature = [0u8; 64];
        let bytes = canonical_cbor_encode(&for_sig).expect("encode");
        let (signing_key, _) = build_test_identity_pub([7; 32]);
        entry.community_signature = signing_key.sign(&bytes).to_bytes();
        assert!(matches!(
            verify_entry(&entry),
            Err(EntryVerifyError::InviteOnlyUrl)
        ));
    }

    #[test]
    fn latest_hlc_replaces_entry() {
        let mut agg = Aggregation::new();
        let library = OwnerAddr([0xAA; 16]);
        let community = SpaceId([1; 16]);
        let h1 = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "d".into(),
        };
        let h2 = Hlc {
            wall_ms: 200,
            logical: 0,
            device_id: "d".into(),
        };

        let invite_url = build_open_invite_url();
        let mut e1 =
            build_signed_entry(community, [7; 32], library, h1.clone(), invite_url.clone());
        e1.name = "old".into();
        // re-sign because we changed name
        let mut for_sig = e1.clone();
        for_sig.community_signature = [0u8; 64];
        let (sk, _) = build_test_identity_pub([7; 32]);
        e1.community_signature = sk
            .sign(&canonical_cbor_encode(&for_sig).unwrap())
            .to_bytes();

        let mut e2 = e1.clone();
        e2.listed_at = h2.clone();
        e2.name = "new".into();
        let mut for_sig2 = e2.clone();
        for_sig2.community_signature = [0u8; 64];
        e2.community_signature = sk
            .sign(&canonical_cbor_encode(&for_sig2).unwrap())
            .to_bytes();

        assert_eq!(agg.on_entry(e1), OnEntryOutcome::Inserted(community));
        assert_eq!(
            agg.on_entry(e2.clone()),
            OnEntryOutcome::Replaced(community)
        );
        let snap = agg.snapshot_all();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].entry.name, "new");
    }

    #[test]
    fn listed_by_unions_across_libraries() {
        let mut agg = Aggregation::new();
        let library_a = OwnerAddr([0xAA; 16]);
        let library_b = OwnerAddr([0xBB; 16]);
        let community = SpaceId([1; 16]);
        let h = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "d".into(),
        };
        let invite_url = build_open_invite_url();

        let e_from_a =
            build_signed_entry(community, [7; 32], library_a, h.clone(), invite_url.clone());
        let e_from_b =
            build_signed_entry(community, [7; 32], library_b, h.clone(), invite_url.clone());

        assert_eq!(agg.on_entry(e_from_a), OnEntryOutcome::Inserted(community));
        assert_eq!(
            agg.on_entry(e_from_b),
            OnEntryOutcome::AccretedListedBy(community)
        );

        let snap = agg.snapshot_all();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].listed_by.len(), 2);
        assert!(snap[0].listed_by.contains(&library_a));
        assert!(snap[0].listed_by.contains(&library_b));
    }

    #[test]
    fn drop_library_evicts_solo_listings() {
        let mut agg = Aggregation::new();
        let library_a = OwnerAddr([0xAA; 16]);
        let library_b = OwnerAddr([0xBB; 16]);
        let solo = SpaceId([1; 16]);
        let shared = SpaceId([2; 16]);
        let h = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "d".into(),
        };
        let invite_url = build_open_invite_url();

        agg.on_entry(build_signed_entry(
            solo,
            [7; 32],
            library_a,
            h.clone(),
            invite_url.clone(),
        ));
        agg.on_entry(build_signed_entry(
            shared,
            [7; 32],
            library_a,
            h.clone(),
            invite_url.clone(),
        ));
        agg.on_entry(build_signed_entry(
            shared,
            [7; 32],
            library_b,
            h.clone(),
            invite_url.clone(),
        ));

        let evicted = agg.drop_library(&library_a);
        assert_eq!(evicted, vec![solo]);
        let snap = agg.snapshot_all();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].entry.community_id, shared);
        assert_eq!(snap[0].listed_by, [library_b].into_iter().collect());
    }

    #[test]
    fn per_library_cap_evicts_oldest_on_overflow() {
        let mut agg = Aggregation::new();
        let library = OwnerAddr([0xAA; 16]);
        let invite_url = build_open_invite_url();
        // Insert MAX_ENTRIES_PER_LIBRARY + 1 entries from this library
        // with distinct community_ids and strictly-increasing HLCs.
        for i in 0..(MAX_ENTRIES_PER_LIBRARY as u32 + 1) {
            let mut cid = [0u8; 16];
            cid[..4].copy_from_slice(&i.to_be_bytes());
            let entry = build_signed_entry(
                SpaceId(cid),
                [7; 32],
                library,
                Hlc {
                    wall_ms: 1_000 + i as u64,
                    logical: 0,
                    device_id: "d".into(),
                },
                invite_url.clone(),
            );
            let outcome = agg.on_entry(entry);
            if i < MAX_ENTRIES_PER_LIBRARY as u32 {
                assert!(matches!(outcome, OnEntryOutcome::Inserted(_)));
            } else {
                // The overflow insert evicts the oldest (i=0).
                let mut oldest_cid = [0u8; 16];
                oldest_cid[..4].copy_from_slice(&0u32.to_be_bytes());
                match outcome {
                    OnEntryOutcome::EvictedThenInserted { evicted, .. } => {
                        assert_eq!(evicted, SpaceId(oldest_cid));
                    }
                    other => panic!("expected EvictedThenInserted, got {other:?}"),
                }
            }
        }
        assert_eq!(
            agg.entry_count_for_library(&library),
            MAX_ENTRIES_PER_LIBRARY
        );
    }

    #[test]
    fn older_hlc_from_same_library_is_idempotent() {
        let mut agg = Aggregation::new();
        let library = OwnerAddr([0xAA; 16]);
        let community = SpaceId([1; 16]);
        let h_old = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "d".into(),
        };
        let h_new = Hlc {
            wall_ms: 200,
            logical: 0,
            device_id: "d".into(),
        };
        let invite_url = build_open_invite_url();

        agg.on_entry(build_signed_entry(
            community,
            [7; 32],
            library,
            h_new,
            invite_url.clone(),
        ));
        let outcome = agg.on_entry(build_signed_entry(
            community,
            [7; 32],
            library,
            h_old,
            invite_url.clone(),
        ));
        assert_eq!(outcome, OnEntryOutcome::Idempotent);
    }

    #[test]
    fn too_many_topics_rejected() {
        let entry = build_signed_entry(
            SpaceId([1; 16]),
            [7; 32],
            OwnerAddr([2; 16]),
            Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "d".into(),
            },
            build_open_invite_url(),
        );
        let mut bad = entry.clone();
        bad.topics = (0..(MAX_TOPICS_PER_ENTRY + 1))
            .map(|i| format!("t{i}"))
            .collect();
        assert!(matches!(
            verify_entry(&bad),
            Err(EntryVerifyError::TooManyTopics)
        ));
    }
}
