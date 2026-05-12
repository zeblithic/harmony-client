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
}

/// Result of `Aggregation::on_entry` — the outcome plus, independently,
/// any community evicted by per-library cap enforcement during the same
/// call. The two dimensions are orthogonal: cap-eviction can co-occur
/// with `Inserted`, `Replaced`, or `AccretedListedBy` (e.g., when a
/// library already at cap re-publishes a community whose entry the
/// library hasn't previously contributed to — the new arrival accretes
/// to an existing community while the cap forces eviction of the
/// library's oldest *other* contribution).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessResult {
    pub outcome: OnEntryOutcome,
    /// If `Some(community_id)`, the per-library cap was hit and this
    /// community was the library's oldest contribution, dropped to make
    /// room. Independent of `outcome`'s discriminant — callers should
    /// process `evicted` for emit/cleanup even when `outcome` is
    /// `Replaced` or `AccretedListedBy`.
    pub evicted: Option<SpaceId>,
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
    ///
    /// Returns a `ProcessResult` carrying both the outcome discriminant
    /// (Inserted / Replaced / AccretedListedBy / Idempotent) and any
    /// orthogonal cap-eviction. Callers must consult both fields: an
    /// eviction can co-occur with `Replaced` or `AccretedListedBy`,
    /// not just `Inserted`.
    pub fn on_entry(&mut self, entry: LibraryDirectoryEntry) -> ProcessResult {
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

        let mut evicted: Option<SpaceId> = None;
        if library_at_cap && is_new_contribution_for_library {
            if let Some(oldest_id) = self.find_oldest_for_library(&library) {
                self.evict_library_contribution(&library, oldest_id);
                evicted = Some(oldest_id);
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

        ProcessResult { outcome, evicted }
    }

    /// Remove all contributions from `library`. Walks the entire
    /// aggregation map (O(N over total entries from this library);
    /// the per-library count is bounded by MAX_ENTRIES_PER_LIBRARY).
    /// Spec §5.3.
    ///
    /// Phase 1 correctness trade-off: if the stored `entry.listed_by`
    /// matches the dropped library, the community is evicted entirely
    /// even if OTHER libraries also listed it. Rationale: we keep
    /// exactly one `LibraryDirectoryEntry` per community (the latest-
    /// HLC), and that entry's metadata (name / description / topics /
    /// invite_url) was sourced from the removed library — surfacing
    /// stale curated metadata under a "trusted library" frame would be
    /// worse than re-discovery on the next subscribe.
    ///
    /// Net behavior:
    /// - Solo listing from this library → evict (unchanged).
    /// - Shared listing where stored entry came from THIS library →
    ///   evict (new in R1).
    /// - Shared listing where stored entry came from another library
    ///   → reduce `listed_by` set only (unchanged).
    pub fn drop_library(&mut self, library: &OwnerAddr) -> Vec<SpaceId> {
        let mut evicted = Vec::new();
        self.by_community.retain(|community_id, agg| {
            let source_was_this_library = &agg.entry.listed_by == library;
            let _ = agg.listed_by.remove(library);
            if source_was_this_library || agg.listed_by.is_empty() {
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

    /// Same correctness rule as `drop_library`: if the stored entry was
    /// sourced from `library`, evict the community entirely rather than
    /// retain its stale metadata under another library's `listed_by`.
    fn evict_library_contribution(&mut self, library: &OwnerAddr, community_id: SpaceId) {
        if let Some(agg) = self.by_community.get_mut(&community_id) {
            if agg.listed_by.remove(library) {
                if let Some(c) = self.per_library_count.get_mut(library) {
                    if *c > 0 {
                        *c -= 1;
                    }
                }
                let source_was_this_library = &agg.entry.listed_by == library;
                if source_was_this_library || agg.listed_by.is_empty() {
                    self.by_community.remove(&community_id);
                }
            }
        }
    }
}

use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Request from IPC handlers (or startup walk) to the event loop:
/// declare or drop a Zenoh subscriber for one library's directory topic.
#[derive(Debug, Clone)]
pub enum LibraryDirectoryRequest {
    Subscribe(OwnerAddr),
    Unsubscribe(OwnerAddr),
}

/// Shared state: aggregation map + the request sender. Held inside
/// `NodeState` (Arc<Mutex<...>>).
///
/// `request_tx` is `UnboundedSender` — Subscribe/Unsubscribe traffic is
/// tiny (single OwnerAddr) and infrequent (user action + startup walk).
/// Unbounded avoids the F1 deadlock where the startup walk in
/// `start_node` could block forever on >capacity sends BEFORE the
/// event-loop consumer task is spawned.
pub struct LibraryDirectory {
    pub aggregation: Mutex<Aggregation>,
    pub request_tx: mpsc::UnboundedSender<LibraryDirectoryRequest>,
}

impl LibraryDirectory {
    /// Construct alongside the matching `request_rx` consumed by the
    /// event loop.
    pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<LibraryDirectoryRequest>) {
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let dir = Arc::new(Self {
            aggregation: Mutex::new(Aggregation::new()),
            request_tx,
        });
        (dir, request_rx)
    }

    /// Decode + verify + aggregate one received sample. Returns the
    /// `ProcessResult` (outcome + optional cap-eviction) for the caller
    /// (event-loop task) to emit `library-directory-updated` from.
    ///
    /// `library_addr` is the subscribed library's `OwnerAddr` — the
    /// topic owner the sample arrived on, captured by the per-library
    /// subscriber task and passed in here. Used to canonicalize entry
    /// attribution: if the decoded entry's `listed_by` field disagrees
    /// with the topic owner, we reject the entry as
    /// `AttributionMismatch`. This prevents a subscribed library from
    /// publishing entries attributing themselves to OTHER library
    /// addresses (which would bypass per-library caps and prevent
    /// `remove_library` from evicting them).
    pub async fn process_sample(
        &self,
        library_addr: OwnerAddr,
        bytes: Vec<u8>,
    ) -> Result<ProcessResult, ProcessSampleError> {
        let entry: LibraryDirectoryEntry =
            ciborium::de::from_reader(&bytes[..]).map_err(ProcessSampleError::Decode)?;
        if entry.listed_by != library_addr {
            return Err(ProcessSampleError::AttributionMismatch {
                expected: library_addr,
                actual: entry.listed_by,
            });
        }
        verify_entry(&entry).map_err(ProcessSampleError::Verify)?;
        let mut agg = self.aggregation.lock().await;
        Ok(agg.on_entry(entry))
    }

    pub async fn drop_library(&self, library: &OwnerAddr) -> Vec<SpaceId> {
        let mut agg = self.aggregation.lock().await;
        agg.drop_library(library)
    }

    pub async fn snapshot_all(&self) -> Vec<AggregatedEntry> {
        self.aggregation.lock().await.snapshot_all()
    }

    pub async fn snapshot_filtered_by_library(&self, library: &OwnerAddr) -> Vec<AggregatedEntry> {
        self.aggregation
            .lock()
            .await
            .snapshot_filtered_by_library(library)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessSampleError {
    #[error("CBOR decode failed: {0}")]
    Decode(ciborium::de::Error<std::io::Error>),
    #[error("verify failed: {0}")]
    Verify(#[from] EntryVerifyError),
    /// The decoded entry's `listed_by` doesn't match the subscribed
    /// library topic the sample arrived on. Rejected to prevent
    /// cross-library attribution spoofing — see `process_sample`.
    #[error("attribution mismatch: topic={expected:?}, entry={actual:?}")]
    AttributionMismatch {
        expected: OwnerAddr,
        actual: OwnerAddr,
    },
}

/// Frontend-facing DTO: minimal library info for chip rendering.
#[derive(Debug, Clone, Serialize)]
pub struct LibraryInfo {
    /// Hex-encoded OwnerAddr (32 chars).
    pub address: String,
    pub added_at: Hlc,
    /// Count of entries currently aggregated from this library.
    pub entry_count: usize,
}

/// Frontend-facing DTO: one community in the browse list. Strips
/// `community_admin_identity_pub` and `community_signature` (verified
/// at receive); exposes the derived `community_addr` for display.
#[derive(Debug, Clone, Serialize)]
pub struct DirectoryEntryDTO {
    /// Hex-encoded SpaceId (32 chars).
    pub community_id: String,
    /// Hex-encoded OwnerAddr (32 chars) derived from
    /// `community_admin_identity_pub` via `address_hash`.
    pub community_addr: String,
    pub name: String,
    pub description: String,
    pub topics: Vec<String>,
    pub invite_url: String,
    pub listed_by_count: usize,
    pub listed_at: Hlc,
}

impl DirectoryEntryDTO {
    pub fn from_aggregated(agg: &AggregatedEntry) -> Self {
        let addr_bytes =
            harmony_identity::Identity::from_public_bytes(&agg.entry.community_admin_identity_pub)
                .map(|id| id.address_hash)
                .unwrap_or_default();
        Self {
            community_id: hex::encode(agg.entry.community_id.0),
            community_addr: hex::encode(addr_bytes),
            name: agg.entry.name.clone(),
            description: agg.entry.description.clone(),
            topics: agg.entry.topics.clone(),
            invite_url: agg.entry.invite_url.clone(),
            listed_by_count: agg.listed_by.len(),
            listed_at: agg.entry.listed_at.clone(),
        }
    }
}

/// Parse a 32-hex-char address into `OwnerAddr`. Validation entry point
/// used by `add_library` / `remove_library` IPCs.
pub fn parse_owner_addr_hex(s: &str) -> Result<OwnerAddr, String> {
    let bytes = hex::decode(s).map_err(|e| format!("invalid hex: {e}"))?;
    if bytes.len() != 16 {
        return Err(format!(
            "expected 16-byte OwnerAddr (32 hex chars), got {} bytes",
            bytes.len()
        ));
    }
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes);
    Ok(OwnerAddr(out))
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

        assert_eq!(
            agg.on_entry(e1).outcome,
            OnEntryOutcome::Inserted(community)
        );
        assert_eq!(
            agg.on_entry(e2.clone()).outcome,
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

        assert_eq!(
            agg.on_entry(e_from_a).outcome,
            OnEntryOutcome::Inserted(community)
        );
        assert_eq!(
            agg.on_entry(e_from_b).outcome,
            OnEntryOutcome::AccretedListedBy(community)
        );

        let snap = agg.snapshot_all();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].listed_by.len(), 2);
        assert!(snap[0].listed_by.contains(&library_a));
        assert!(snap[0].listed_by.contains(&library_b));
    }

    /// `drop_library` evicts solo listings AND retains shared listings
    /// whose STORED entry was sourced from another library.
    ///
    /// To exercise the "shared listing retained" path under the F3
    /// rule, library_b must be the stored source (highest-HLC). We
    /// publish library_b's entry at a newer HLC so the stored entry's
    /// `listed_by == library_b`; dropping library_a then reduces only
    /// the `listed_by` set without evicting the community.
    #[test]
    fn drop_library_evicts_solo_listings() {
        let mut agg = Aggregation::new();
        let library_a = OwnerAddr([0xAA; 16]);
        let library_b = OwnerAddr([0xBB; 16]);
        let solo = SpaceId([1; 16]);
        let shared = SpaceId([2; 16]);
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
            solo,
            [7; 32],
            library_a,
            h_old.clone(),
            invite_url.clone(),
        ));
        agg.on_entry(build_signed_entry(
            shared,
            [7; 32],
            library_a,
            h_old.clone(),
            invite_url.clone(),
        ));
        // library_b publishes at a NEWER HLC so its entry becomes the
        // stored source — required for the F3-rule shared-retention
        // path to apply.
        agg.on_entry(build_signed_entry(
            shared,
            [7; 32],
            library_b,
            h_new.clone(),
            invite_url.clone(),
        ));

        let evicted = agg.drop_library(&library_a);
        assert_eq!(evicted, vec![solo]);
        let snap = agg.snapshot_all();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].entry.community_id, shared);
        assert_eq!(snap[0].listed_by, [library_b].into_iter().collect());
    }

    /// F3 regression: `drop_library` must evict the community when the
    /// stored entry's `listed_by` field matches the dropped library,
    /// even when OTHER libraries also list it — otherwise we'd retain
    /// stale curated metadata under a "trusted library" frame.
    ///
    /// Scenario: library A publishes for community C at HLC 100
    /// (name="from-A"); library B publishes for the SAME community at
    /// HLC 200 (name="from-B") — newer-HLC wins so stored entry is
    /// from B. Drop library B → community C must be FULLY evicted
    /// (not just `listed_by` reduced to {A}), because the stored entry
    /// metadata came from B.
    #[test]
    fn drop_library_evicts_entries_sourced_from_dropped_library() {
        let mut agg = Aggregation::new();
        let library_a = OwnerAddr([0xAA; 16]);
        let library_b = OwnerAddr([0xBB; 16]);
        let community = SpaceId([1; 16]);
        let invite_url = build_open_invite_url();

        // Library A publishes first at HLC 100 with name="from-A".
        let mut entry_a = build_signed_entry(
            community,
            [7; 32],
            library_a,
            Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "d".into(),
            },
            invite_url.clone(),
        );
        entry_a.name = "from-A".into();
        // Re-sign because we changed name.
        let mut for_sig = entry_a.clone();
        for_sig.community_signature = [0u8; 64];
        let (sk, _) = build_test_identity_pub([7; 32]);
        entry_a.community_signature = sk
            .sign(&canonical_cbor_encode(&for_sig).unwrap())
            .to_bytes();
        agg.on_entry(entry_a);

        // Library B publishes for the SAME community at HLC 200 with
        // name="from-B" — newer-HLC wins, stored entry becomes B's.
        let mut entry_b = build_signed_entry(
            community,
            [7; 32],
            library_b,
            Hlc {
                wall_ms: 200,
                logical: 0,
                device_id: "d".into(),
            },
            invite_url.clone(),
        );
        entry_b.name = "from-B".into();
        let mut for_sig_b = entry_b.clone();
        for_sig_b.community_signature = [0u8; 64];
        entry_b.community_signature = sk
            .sign(&canonical_cbor_encode(&for_sig_b).unwrap())
            .to_bytes();
        agg.on_entry(entry_b);

        // Sanity: pre-drop state has community C listed by both, stored
        // entry sourced from B.
        let snap = agg.snapshot_all();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].entry.name, "from-B");
        assert_eq!(snap[0].listed_by.len(), 2);

        // Drop library B → community must be FULLY evicted, not just
        // reduced to listed_by={A}, because stored entry came from B.
        let evicted = agg.drop_library(&library_b);
        assert_eq!(
            evicted,
            vec![community],
            "community must be evicted when stored entry's listed_by == dropped library"
        );
        assert!(
            agg.snapshot_all().is_empty(),
            "no stale metadata retained from dropped library"
        );
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
            let result = agg.on_entry(entry);
            if i < MAX_ENTRIES_PER_LIBRARY as u32 {
                assert!(matches!(result.outcome, OnEntryOutcome::Inserted(_)));
                assert!(result.evicted.is_none(), "no eviction under cap");
            } else {
                // The overflow insert evicts the oldest (i=0) AND
                // inserts the new arrival.
                let mut oldest_cid = [0u8; 16];
                oldest_cid[..4].copy_from_slice(&0u32.to_be_bytes());
                assert!(
                    matches!(result.outcome, OnEntryOutcome::Inserted(_)),
                    "overflow path inserts the new community: got {:?}",
                    result.outcome
                );
                assert_eq!(
                    result.evicted,
                    Some(SpaceId(oldest_cid)),
                    "overflow must surface eviction of i=0"
                );
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
        let result = agg.on_entry(build_signed_entry(
            community,
            [7; 32],
            library,
            h_old,
            invite_url.clone(),
        ));
        assert_eq!(result.outcome, OnEntryOutcome::Idempotent);
        assert!(result.evicted.is_none());
    }

    #[test]
    fn parse_owner_addr_hex_round_trips() {
        let good = "aa".repeat(16);
        let addr = parse_owner_addr_hex(&good).expect("valid 32-hex-char");
        assert_eq!(addr, OwnerAddr([0xAA; 16]));
    }

    #[test]
    fn parse_owner_addr_hex_rejects_short_input() {
        let too_short = "aa".repeat(15);
        let err = parse_owner_addr_hex(&too_short).unwrap_err();
        assert!(err.contains("expected 16-byte"), "got: {err}");
    }

    #[test]
    fn parse_owner_addr_hex_rejects_non_hex() {
        let non_hex = "zz".repeat(16);
        let err = parse_owner_addr_hex(&non_hex).unwrap_err();
        assert!(err.contains("invalid hex"), "got: {err}");
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
