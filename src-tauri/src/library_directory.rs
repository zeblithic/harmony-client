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

    // === Sub-D Phase 3 (ZEB-280) wrapping signature fields ===
    //
    // Wire-compatible with Phase 1: `skip_serializing_if = "Option::is_none"`
    // omits the keys from canonical CBOR when None, so a Phase 1 entry's
    // bytes are byte-identical regardless of whether it's emitted by a
    // Phase 1 or Phase 3 client.
    //
    // 2-char field keys preserve `canonical_cbor_encode`'s same-length-keys
    // precondition (mirrors all other Sub-A/B/C wire types).
    //
    // See spec §4.1.
    /// 64-byte identity bundle (X25519_pub || Ed25519_pub) of the
    /// broadcasting library. None for unwrapped (Phase 1-style) entries.
    #[serde(
        rename = "li",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::owner_state_types::serialize_optional_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_optional_bytes_from_bstr"
    )]
    pub library_identity_pub: Option<[u8; 64]>,

    /// Ed25519 wrapping signature from the broadcasting library over
    /// the canonical CBOR encoding of all fields with `library_signature`
    /// zeroed (analogous to Phase 1's `community_signature` pattern).
    /// None for unwrapped entries.
    #[serde(
        rename = "ls",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::owner_state_types::serialize_optional_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_optional_bytes_from_bstr"
    )]
    pub library_signature: Option<[u8; 64]>,
}

impl CanonicalPayloadSealed for LibraryDirectoryEntry {}
impl CanonicalPayload for LibraryDirectoryEntry {}

/// Sub-D Phase 3 (ZEB-280) — outcome of `verify_entry`. Captures the
/// admin-sig-verified entry's wrapping-signature state, which feeds the
/// aggregation's broadcasting-library tracking and the frontend
/// "unattested" badge. Spec §4.2.
///
/// `Copy` is derived defensively (R1, CodeRabbit): `process_sample` matches
/// on `status` once and then passes it to `on_entry`, which compiles today
/// via match-ergonomics but would break on future refactors that move the
/// match. The variant only holds `OwnerAddr` (already `Copy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttestationStatus {
    /// Phase 1-style entry: no wrapping sig present (both
    /// `library_signature` and `library_identity_pub` are `None`).
    /// Implicit trust from subscription topic — entries arriving from
    /// library X's topic are treated as if X attested to them.
    Unwrapped,
    /// Phase 3: wrapping sig present and verified. `OwnerAddr` is the
    /// broadcasting library's derived address (from
    /// `library_identity_pub` via `Identity::from_public_bytes`).
    Attested(OwnerAddr),
    /// Phase 3: wrapping sig present but invalid. Entry is still
    /// surfaced — the community admin's signature is the trust anchor
    /// for content. `OwnerAddr` is the broadcasting library's CLAIMED
    /// address (the derived addr from `library_identity_pub`; we keep
    /// it for aggregation tracking even though we couldn't verify the
    /// claim).
    Unattested(OwnerAddr),
}

/// Sub-D Phase 2 auto-discovery announce record. Spec §4.1.
///
/// Published by libraries to `harmony/discovery/library/announce` to
/// advertise their existence. Each device subscribes the topic once at
/// startup; valid announces populate the in-memory `Announces` map and
/// surface in the `LibraryDirectoryBrowser` "Discovered libraries"
/// section.
///
/// Signing model: the library signs its own announce with the Ed25519
/// half of its 64-byte identity bundle. The OwnerAddr derives from the
/// identity bundle (via `Identity::from_public_bytes`), so no separate
/// `library_addr` field is on the wire — it cannot disagree with the
/// signed identity.
///
/// 2-char field keys (`ai`, `nm`, `ds`, `la`, `ls`) satisfy
/// `canonical_cbor_encode`'s same-length-keys precondition (mirrors
/// `LibraryDirectoryEntry` and all other Sub-A/B/C wire types).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryAnnounce {
    /// 64-byte identity bundle (X25519_pub(32) || Ed25519_pub(32)).
    /// The OwnerAddr derives from this; the Ed25519 half verifies
    /// `library_signature`.
    #[serde(
        rename = "ai",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub library_identity_pub: [u8; 64],

    #[serde(rename = "nm")]
    pub name: String,

    #[serde(rename = "ds")]
    pub description: String,

    #[serde(rename = "la")]
    pub listed_at: Hlc,

    /// Ed25519 sig over canonical CBOR with `ls` zeroed.
    #[serde(
        rename = "ls",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub library_signature: [u8; 64],
}

impl CanonicalPayloadSealed for LibraryAnnounce {}
impl CanonicalPayload for LibraryAnnounce {}

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
    /// The decoded `invite_url` payload's `community_id` doesn't equal
    /// the entry's signed `community_id`. A malicious publisher could
    /// otherwise sign an entry advertising community A while embedding
    /// an invite that joins community B (phishing-style mismatch).
    #[error("invite payload community_id mismatch: entry={entry:?}, payload={payload:?}")]
    PayloadCommunityIdMismatch { entry: SpaceId, payload: SpaceId },
    /// The decoded `invite_url` payload's admin identity doesn't match
    /// the entry's signed `community_admin_identity_pub`. Defends against
    /// an entry signed by admin X embedding an invite carrying admin Y's
    /// epoch state — the joiner would bootstrap under Y's authority while
    /// the directory UI displays X.
    #[error(
        "invite payload admin identity mismatch: entry_addr={entry_addr:?}, payload_addr={payload_addr:?}"
    )]
    PayloadAdminIdentityMismatch {
        entry_addr: OwnerAddr,
        payload_addr: OwnerAddr,
    },
    /// Sub-D Phase 3: exactly one of `library_signature` and
    /// `library_identity_pub` is `Some`. Cannot verify a wrapping sig
    /// without both fields; this is a malformed wire state and must
    /// be rejected (not silently treated as Unwrapped, which would
    /// mask a publisher bug). Spec §5.
    #[error("library_signature and library_identity_pub must both be Some or both be None")]
    LibrarySignatureFieldsInconsistent,

    /// Sub-D Phase 3: `library_identity_pub` bytes failed
    /// `Identity::from_public_bytes` validation. Spec §5.
    #[error("malformed library identity_pub: {0}")]
    InvalidLibraryIdentityPub(String),
}

pub const MAX_NAME_LEN: usize = 200;
pub const MAX_DESCRIPTION_LEN: usize = 2000;
pub const MAX_TOPICS_PER_ENTRY: usize = 16;
pub const MAX_TOPIC_LEN: usize = 64;
pub const MAX_ENTRIES_PER_LIBRARY: usize = 10_000;

/// Cap on the in-memory `Announces` map. Smaller than
/// `MAX_ENTRIES_PER_LIBRARY` because this is a global count of known
/// libraries, not per-library entries. Spec §4.2 / §10.
pub const MAX_DISCOVERED_LIBRARIES: usize = 1_000;

/// Hard cap on the wire size of a single `LibraryAnnounce` payload
/// before CBOR decode. The caller (the Zenoh announce subscriber)
/// MUST drop payloads larger than this without allocating them into
/// owned `Vec<u8>` buffers downstream of `to_bytes()`. Bound rationale:
/// `name` (≤ 200) + `description` (≤ 2000) + `library_identity_pub`
/// (64) + `library_signature` (64) + `Hlc` (≈ 20) + CBOR framing
/// overhead ≈ 2.4 KB worst-case canonical payload. 4 KB is ~1.6×
/// headroom — generous enough to survive minor schema additions while
/// still bounding adversarial allocations on the global
/// `harmony/discovery/library/announce` topic.
pub const MAX_ANNOUNCE_WIRE_BYTES: usize = 4_096;

/// Verification error categories for `LibraryAnnounce`. Mirrors
/// `EntryVerifyError` but simpler — no invite URL, no
/// community/admin payload binding. Each variant surfaces as a
/// warn-level log; the announce is silently dropped from the
/// caller's perspective.
#[derive(Debug, thiserror::Error)]
pub enum AnnounceVerifyError {
    #[error("CBOR decode failed: {0}")]
    DecodeFailed(String),
    #[error("malformed library identity_pub: {0}")]
    InvalidIdentityPub(String),
    #[error("canonical CBOR encode failed: {0}")]
    Encode(#[from] crate::owner_state_crypto::CryptoError),
    #[error("Ed25519 signature verification failed")]
    SignatureInvalid,
    #[error("name exceeds {MAX_NAME_LEN} bytes")]
    NameTooLong,
    #[error("description exceeds {MAX_DESCRIPTION_LEN} bytes")]
    DescriptionTooLong,
    #[error("listed_at is too far in the future (beyond display skew tolerance)")]
    ListedAtTooFarInFuture,
}

/// Verify a `LibraryDirectoryEntry` end-to-end and return the
/// wrapping-signature attestation outcome. Spec §5.
///
/// **Phase 1 invariants (unchanged):**
/// 1. Anti-spam bounds (name/description/topic lengths)
/// 2. Parse `community_admin_identity_pub` via
///    `harmony_identity::Identity::from_public_bytes`
/// 3. Verify the Ed25519 admin signature over canonical-CBOR-encoded
///    fields with `community_signature` zeroed (so verify == sign
///    exactly). The Optional `library_identity_pub` / `library_signature`
///    are also zeroed (via `None` + `skip_serializing_if`), so admin
///    sig bytes are portable across libraries.
/// 4. Parse `invite_url` and reject if `is_invite_only == true`
/// 5. Invite payload binding (community_id + admin_addr)
///
/// **Phase 3 addition:** if `library_signature` and
/// `library_identity_pub` are both `Some`, verify the wrapping sig
/// over canonical-CBOR-encoded fields with only `library_signature`
/// zeroed (keep `library_identity_pub` + `community_signature`
/// populated, so the wrapping sig commits to the admin-signed bundle).
///
/// **Returns:**
/// - `Ok(AttestationStatus::Unwrapped)` — Phase 1-style entry (both
///   Optional fields None)
/// - `Ok(AttestationStatus::Attested(library_addr))` — wrapping sig
///   verified, library_addr is derived from `library_identity_pub`
/// - `Ok(AttestationStatus::Unattested(library_addr))` — wrapping sig
///   present but invalid; entry NOT dropped (admin sig was valid).
///   library_addr is the CLAIMED broadcasting library.
/// - `Err(LibrarySignatureFieldsInconsistent)` — exactly one of
///   library_signature / library_identity_pub is Some (malformed).
/// - `Err(InvalidLibraryIdentityPub)` — library_identity_pub bytes
///   failed `Identity::from_public_bytes`.
/// - Other `Err(...)` — admin sig path failed; entry should be dropped.
pub fn verify_entry(entry: &LibraryDirectoryEntry) -> Result<AttestationStatus, EntryVerifyError> {
    // (1) Bounds — unchanged from Phase 1.
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

    // (3) Verify admin sig over canonical CBOR with cs zeroed and
    //     li/ls forced to None. The Phase 1 invariant of "sig field
    //     zeroed during sign/verify" extends to "Optional fields
    //     forced absent" — skip_serializing_if omits them from CBOR.
    //     This makes the admin sig portable across libraries (the
    //     wrapping library can attach li+ls without invalidating the
    //     admin sig).
    let mut for_sig = entry.clone();
    for_sig.community_signature = [0u8; 64];
    for_sig.library_identity_pub = None;
    for_sig.library_signature = None;
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

    // (5) Invite-payload consistency with the signed directory entry.
    //
    // A malicious directory publisher could sign a `LibraryDirectoryEntry`
    // advertising community A (name/description/topics presented in the
    // UI come from the signed entry), but embed an `invite_url` whose
    // decoded payload points at community B. The browser UI would show
    // A's metadata, the user clicks "Join", and the backend's
    // `redeem_invite` uses ONLY the `invite_url` — so the user joins B.
    // This is a phishing-class attack. Bind the payload to the signed
    // entry on (a) community_id and (b) admin identity.
    if payload.community_id != entry.community_id {
        return Err(EntryVerifyError::PayloadCommunityIdMismatch {
            entry: entry.community_id,
            payload: payload.community_id,
        });
    }
    // The entry's `community_admin_identity_pub` is the 64-byte identity
    // bundle (X25519_pub || Ed25519_pub) that signed the entry. Its
    // derived 16-byte `address_hash` is the OwnerAddr. The invite payload
    // carries `admin_addr: OwnerAddr` (always present; for invite-only
    // payloads the full identity_pub also rides along in
    // `admin_identity_pub`). For open-community URLs in the directory,
    // only `admin_addr` is guaranteed-present — compare on that axis.
    let entry_admin_addr = OwnerAddr(identity.address_hash);
    if payload.admin_addr != entry_admin_addr {
        return Err(EntryVerifyError::PayloadAdminIdentityMismatch {
            entry_addr: entry_admin_addr,
            payload_addr: payload.admin_addr,
        });
    }

    // (6) Sub-D Phase 3 — wrapping signature check.
    match (&entry.library_signature, &entry.library_identity_pub) {
        (None, None) => Ok(AttestationStatus::Unwrapped),
        (Some(_), None) | (None, Some(_)) => {
            Err(EntryVerifyError::LibrarySignatureFieldsInconsistent)
        }
        (Some(lib_sig), Some(lib_pub)) => {
            let lib_identity = harmony_identity::Identity::from_public_bytes(lib_pub)
                .map_err(|e| EntryVerifyError::InvalidLibraryIdentityPub(format!("{e:?}")))?;
            let lib_addr = OwnerAddr(lib_identity.address_hash);

            // Reconstruct sign-time bytes: zero `library_signature`
            // only (keep `library_identity_pub` + `community_signature`
            // populated so the wrapping sig commits to the admin sig).
            let mut for_sig = entry.clone();
            for_sig.library_signature = None;
            let signed_bytes = canonical_cbor_encode(&for_sig)?;
            let sig = Signature::from_bytes(lib_sig);

            match lib_identity
                .verifying_key
                .verify_strict(&signed_bytes, &sig)
            {
                Ok(()) => Ok(AttestationStatus::Attested(lib_addr)),
                Err(_) => Ok(AttestationStatus::Unattested(lib_addr)),
            }
        }
    }
}

/// Verify a `LibraryAnnounce` end-to-end:
/// 1. Anti-spam bounds (name/description lengths)
/// 2. Parse `library_identity_pub` via
///    `harmony_identity::Identity::from_public_bytes` (validates both
///    halves of the X25519||Ed25519 bundle)
/// 3. Verify the Ed25519 signature over canonical-CBOR-encoded fields
///    with `library_signature` zeroed (so verify == sign exactly)
/// 4. Forward-skew bound on `announce.listed_at.wall_ms` (ZEB-852 C7):
///    `listed_at` is inside the signed CBOR (authenticated) but
///    self-attested by the broadcasting library. A future-dated stamp
///    both wins the per-community LWW (`is_strictly_newer_than`) — pinning
///    the top of discovery — AND is never the min in cap-eviction, so it
///    is immune to eviction and evicts honest libraries instead. Reject
///    when it exceeds `now + DISPLAY_SKEW_TOLERANCE_MS`. This is pure
///    discovery ranking + cap eviction (a DISPLAY-tier concern), not a
///    control/routing decision.
///
/// `now_ms` is the receiver's own trusted wall clock
/// (`clock_trust::receiver_now_ms()`); production call sites pass that.
/// `None` ⇒ apply-all: an unreadable local clock never rejects an honest
/// announce (fail-open — never substitute `0`).
///
/// Returns the derived `OwnerAddr` (library_addr) on success — callers
/// need this to insert into the Announces map.
pub fn verify_announce(
    announce: &LibraryAnnounce,
    now_ms: Option<u64>,
) -> Result<OwnerAddr, AnnounceVerifyError> {
    // (1) Bounds
    if announce.name.len() > MAX_NAME_LEN {
        return Err(AnnounceVerifyError::NameTooLong);
    }
    if announce.description.len() > MAX_DESCRIPTION_LEN {
        return Err(AnnounceVerifyError::DescriptionTooLong);
    }

    // (2) Parse identity_pub — also rejects malformed point bytes.
    let identity = harmony_identity::Identity::from_public_bytes(&announce.library_identity_pub)
        .map_err(|e| AnnounceVerifyError::InvalidIdentityPub(format!("{e:?}")))?;

    // (3) Verify sig over canonical CBOR with signature field zeroed.
    let mut for_sig = announce.clone();
    for_sig.library_signature = [0u8; 64];
    let signed_bytes = canonical_cbor_encode(&for_sig)?;
    let sig = Signature::from_bytes(&announce.library_signature);
    identity
        .verifying_key
        .verify_strict(&signed_bytes, &sig)
        .map_err(|_| AnnounceVerifyError::SignatureInvalid)?;

    // (4) Forward-skew bound on the self-attested `listed_at` (DISPLAY tier).
    if let Some(now) = now_ms {
        if crate::clock_trust::reject_future_logged(
            announce.listed_at.wall_ms,
            now,
            crate::clock_trust::DISPLAY_SKEW_TOLERANCE_MS,
            "library_directory.announce.listed_at",
        ) {
            return Err(AnnounceVerifyError::ListedAtTooFarInFuture);
        }
    }
    // None ⇒ apply-all (unreadable local clock never rejects an honest announce).

    Ok(OwnerAddr(identity.address_hash))
}

use std::collections::{BTreeMap, BTreeSet};

/// One entry per community_id, deduped across libraries. Spec §4.3.
#[derive(Debug, Clone)]
pub struct AggregatedEntry {
    /// Latest (highest-HLC) entry observed for this community.
    pub entry: LibraryDirectoryEntry,

    /// Sub-D Phase 3 (ZEB-280): libraries whose broadcast of this
    /// community we trust. Populated by:
    /// - `AttestationStatus::Attested(lib_addr)` → insert(lib_addr)
    /// - `AttestationStatus::Unwrapped` → insert(entry.listed_by)
    ///   (Phase 1 backward compat — implicit trust from subscription
    ///   topic)
    ///
    /// Replaces the Phase 1 `listed_by: BTreeSet<OwnerAddr>` field
    /// semantics. Eviction triggers when this set empties.
    pub attested_by: BTreeSet<OwnerAddr>,

    /// Sub-D Phase 3 (ZEB-280): libraries we received this entry from
    /// whose wrapping sig failed to verify. Drives the "unattested"
    /// UI badge (entry shown but flagged):
    ///   unattested = !unattested_by.is_empty()
    ///
    /// Tracks the broadcasting library's CLAIMED address (derived from
    /// the signed `library_identity_pub` — we know who claimed to
    /// broadcast even when their sig was bad).
    pub unattested_by: BTreeSet<OwnerAddr>,
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
            .filter(|e| e.attested_by.contains(library) || e.unattested_by.contains(library))
            .cloned()
            .collect()
    }

    pub fn entry_count_for_library(&self, library: &OwnerAddr) -> usize {
        self.per_library_count.get(library).copied().unwrap_or(0)
    }

    /// Process a verified entry. Caller MUST have run `verify_entry`
    /// first — this method does NOT re-verify the signature.
    ///
    /// `status` is the AttestationStatus returned by `verify_entry`,
    /// which determines whether the broadcasting library is the
    /// admin-signed `entry.listed_by` (Unwrapped) or the wrapping-sig
    /// derived `OwnerAddr` (Attested / Unattested). The
    /// broadcasting library drives:
    ///   - per-library cap accounting (Phase 1 used entry.listed_by;
    ///     Phase 3 uses the broadcasting library, which can differ
    ///     when library A republishes library B's entry verbatim)
    ///   - eviction by `drop_library` (must sweep both attested_by
    ///     and unattested_by)
    ///
    /// ## ZEB-280 R1 unified policy (CodeRabbit + Qodo)
    ///
    /// **Invariant:** `attested_by` and `unattested_by` are DISJOINT per
    /// community — a library is in at most one set per community.
    ///
    /// **`per_library_count[X]`** = count of communities where X is in
    /// `attested_by`. **Unattested-only contributions do NOT count toward
    /// the cap.** This shuts down the Qodo DoS finding: a network
    /// adversary publishing bad-sig entries on library X's open Zenoh
    /// topic with `library_identity_pub = X` could otherwise pump X's
    /// count via the unattested path until cap eviction targeted X's
    /// LEGITIMATE attested contributions.
    ///
    /// **Insertion rules:**
    /// - `Attested(X)` / `Unwrapped` (X = entry.listed_by):
    ///   - X in `attested_by`: no-op (idempotent)
    ///   - X in `unattested_by`: recovery — move X to `attested_by`,
    ///     bump `per_library_count[X]`
    ///   - X in neither: insert X into `attested_by`, bump count
    /// - `Unattested(X)`:
    ///   - X in `attested_by`: no-op (do NOT downgrade trusted attestation)
    ///   - X in `unattested_by`: no-op (idempotent)
    ///   - X in neither + community exists: insert into `unattested_by`,
    ///     do NOT bump `per_library_count`
    ///   - community does NOT exist: DROP entirely (return Idempotent).
    ///     Prevents fake-community memory DoS from a network adversary
    ///     on the library's open topic.
    ///
    /// **Cap check** fires only for Attested/Unwrapped status and only
    /// when X is NOT already attesting this community. Unattested status
    /// never triggers cap eviction.
    pub fn on_entry(
        &mut self,
        entry: LibraryDirectoryEntry,
        status: AttestationStatus,
    ) -> ProcessResult {
        let community_id = entry.community_id;
        // Sub-D Phase 3: the broadcasting library identity comes from
        // AttestationStatus. For Phase 1-shaped entries (Unwrapped),
        // it falls back to the admin-signed `listed_by` (which Phase 1
        // attribution-checking already constrained to equal the topic
        // owner).
        let broadcasting_lib = match status {
            AttestationStatus::Attested(addr) | AttestationStatus::Unattested(addr) => addr,
            AttestationStatus::Unwrapped => entry.listed_by,
        };
        let is_attesting_status = matches!(
            status,
            AttestationStatus::Attested(_) | AttestationStatus::Unwrapped
        );

        // ZEB-280 R1 (Qodo): drop Unattested entries for communities
        // that have no prior aggregation. Prevents a network adversary
        // on library X's open Zenoh topic from publishing bad-sig
        // entries with `library_identity_pub = X` to fabricate fake
        // communities in our aggregation map.
        if !is_attesting_status && !self.by_community.contains_key(&community_id) {
            return ProcessResult {
                outcome: OnEntryOutcome::Idempotent,
                evicted: None,
            };
        }

        // Cap check BEFORE insert. ZEB-280 R1: only triggers for
        // attesting status (Attested/Unwrapped). Unattested entries do
        // NOT count toward `per_library_count` and therefore cannot
        // cause cap eviction (defeats Qodo's DoS via spoofed
        // library_identity_pub on an open topic).
        //
        // "is_new_contribution_for_library" under the new disjoint-sets
        // invariant: X is a new attesting contributor iff X is NOT
        // already in `attested_by` (membership in `unattested_by` is a
        // recovery candidate, not "already attesting").
        let mut evicted: Option<SpaceId> = None;
        if is_attesting_status {
            let library_at_cap =
                self.entry_count_for_library(&broadcasting_lib) >= MAX_ENTRIES_PER_LIBRARY;
            let already_attesting = self
                .by_community
                .get(&community_id)
                .map(|agg| agg.attested_by.contains(&broadcasting_lib))
                .unwrap_or(false);
            if library_at_cap && !already_attesting {
                if let Some(oldest_id) = self.find_oldest_for_library(&broadcasting_lib) {
                    self.evict_library_contribution(&broadcasting_lib, oldest_id);
                    evicted = Some(oldest_id);
                } else {
                    tracing::warn!(
                        target: "library_directory",
                        library = ?broadcasting_lib,
                        per_library_count = self.entry_count_for_library(&broadcasting_lib),
                        max = MAX_ENTRIES_PER_LIBRARY,
                        "per_library_count says at-cap but find_oldest_for_library returned None — counter invariant violated",
                    );
                    debug_assert!(
                        false,
                        "per_library_count invariant violated: library {:?} count={} but no community lists it",
                        broadcasting_lib,
                        self.entry_count_for_library(&broadcasting_lib)
                    );
                }
            }
        }

        let outcome = match self.by_community.get_mut(&community_id) {
            None => {
                // Brand-new community. Unattested-status branches were
                // already filtered above (the `!is_attesting_status &&
                // !contains_key` early-return), so we only land here for
                // Attested or Unwrapped status — the disjoint-sets
                // invariant gives `unattested_by = ∅`.
                debug_assert!(
                    is_attesting_status,
                    "ZEB-280 R1: Unattested for new community should have early-returned"
                );
                let inserted_lib = match status {
                    AttestationStatus::Attested(lib_addr) => lib_addr,
                    AttestationStatus::Unwrapped => entry.listed_by,
                    // Unreachable: filtered by early-return above.
                    AttestationStatus::Unattested(addr) => addr,
                };
                let mut attested_by = BTreeSet::new();
                attested_by.insert(inserted_lib);
                self.by_community.insert(
                    community_id,
                    AggregatedEntry {
                        entry,
                        attested_by,
                        unattested_by: BTreeSet::new(),
                    },
                );
                *self.per_library_count.entry(broadcasting_lib).or_insert(0) += 1;
                OnEntryOutcome::Inserted(community_id)
            }
            Some(existing) => {
                let incoming_newer = entry
                    .listed_at
                    .is_strictly_newer_than(&existing.entry.listed_at);
                // ZEB-280 R1: disjoint-sets policy. Track whether this
                // insert grew the *attested* set (which is what drives
                // both AccretedListedBy and per_library_count).
                //
                // Recovery semantics: when an Attested/Unwrapped entry
                // arrives for a library already in `unattested_by`,
                // move the library from unattested_by → attested_by
                // (preserves disjoint invariant; bumps the cap counter
                // exactly once, treating the move as a new attestation).
                let was_new_attestation = match status {
                    AttestationStatus::Attested(lib_addr) => {
                        let was_new = existing.attested_by.insert(lib_addr);
                        if was_new {
                            // Recovery: previously-tampered wrap is now
                            // valid. Remove the stale unattested entry.
                            existing.unattested_by.remove(&lib_addr);
                        }
                        was_new
                    }
                    AttestationStatus::Unwrapped => {
                        let was_new = existing.attested_by.insert(entry.listed_by);
                        if was_new {
                            existing.unattested_by.remove(&entry.listed_by);
                        }
                        was_new
                    }
                    AttestationStatus::Unattested(lib_addr) => {
                        // Don't downgrade a trusted attestation: if X
                        // is already in attested_by, ignore the bad-sig
                        // broadcast entirely. Otherwise, insert into
                        // unattested_by (idempotent if already present).
                        // Unattested NEVER bumps per_library_count.
                        if !existing.attested_by.contains(&lib_addr) {
                            let _ = existing.unattested_by.insert(lib_addr);
                        }
                        false
                    }
                };
                if was_new_attestation {
                    *self.per_library_count.entry(broadcasting_lib).or_insert(0) += 1;
                }
                if incoming_newer {
                    existing.entry = entry;
                    OnEntryOutcome::Replaced(community_id)
                } else if was_new_attestation {
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
        // Two-pass to satisfy the borrow checker (R2 F2 patterns
        // unchanged from Phase 1 — just generalized to both sets).
        //
        // ZEB-280 R1: counter rollback uses only `attested_by`
        // membership (deduped). Under the disjoint-sets invariant a
        // library appears in at most one of attested_by/unattested_by
        // per community, but per_library_count tracks ONLY attested
        // contributions (unattested entries don't count toward the
        // cap). Decrementing for unattested-set membership would
        // double-decrement the count for libraries that recovered
        // from unattested → attested.
        let mut to_evict: Vec<(SpaceId, BTreeSet<OwnerAddr>)> = Vec::new();
        for (community_id, agg) in self.by_community.iter_mut() {
            let source_was_this_library = &agg.entry.listed_by == library;
            let _ = agg.attested_by.remove(library);
            let _ = agg.unattested_by.remove(library);
            let both_sets_empty = agg.attested_by.is_empty() && agg.unattested_by.is_empty();
            if source_was_this_library || both_sets_empty {
                // Capture remaining attested_by ONLY for counter rollback.
                to_evict.push((*community_id, agg.attested_by.clone()));
            }
        }
        let mut evicted_ids = Vec::with_capacity(to_evict.len());
        for (id, remaining_attested) in to_evict {
            self.by_community.remove(&id);
            for other in remaining_attested {
                if &other != library {
                    if let Some(c) = self.per_library_count.get_mut(&other) {
                        if *c > 0 {
                            *c -= 1;
                        }
                    }
                }
            }
            evicted_ids.push(id);
        }
        self.per_library_count.remove(library);
        evicted_ids
    }

    /// ZEB-280 R1: filter only by `attested_by` membership. Cap eviction
    /// operates exclusively on attested contributions — unattested
    /// entries never counted toward `per_library_count` in the first
    /// place, so they cannot be "the oldest contribution" by cap
    /// semantics.
    fn find_oldest_for_library(&self, library: &OwnerAddr) -> Option<SpaceId> {
        self.by_community
            .iter()
            .filter(|(_, agg)| agg.attested_by.contains(library))
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
    ///
    /// R2 F2 (Medium, correctness): when the F3 source-matches rule
    /// triggers eviction of a shared community, OTHER libraries that
    /// were also in `listed_by` previously had their `per_library_count`
    /// stale-incremented for this community. Decrement those counts
    /// alongside the eviction to keep the counters consistent.
    fn evict_library_contribution(&mut self, library: &OwnerAddr, community_id: SpaceId) {
        // Capture state in a scoped borrow, then apply mutations after.
        //
        // ZEB-280 R1: per_library_count tracks ONLY attested
        // contributions. Decrement when removing the library from
        // `attested_by`; removal from `unattested_by` is counter-neutral.
        // Same dedupe rule for OTHER libraries on surviving eviction:
        // roll back counts only via attested_by membership.
        let mut surviving_attested: Option<BTreeSet<OwnerAddr>> = None;
        if let Some(agg) = self.by_community.get_mut(&community_id) {
            let removed_from_attested = agg.attested_by.remove(library);
            let _ = agg.unattested_by.remove(library);
            if removed_from_attested {
                if let Some(c) = self.per_library_count.get_mut(library) {
                    if *c > 0 {
                        *c -= 1;
                    }
                }
            }
            // Phase 3 generalization of the Phase 1 "source matches"
            // rule: if the stored entry was sourced from this
            // library, evict the community entirely. The entry's
            // signed `listed_by` is the closest proxy for "who
            // sourced the stored metadata". Also evict if both sets
            // empty after the removal.
            let source_was_this_library = &agg.entry.listed_by == library;
            let both_sets_empty = agg.attested_by.is_empty() && agg.unattested_by.is_empty();
            if source_was_this_library || both_sets_empty {
                surviving_attested = Some(agg.attested_by.clone());
            }
        }
        if let Some(remaining_attested) = surviving_attested {
            self.by_community.remove(&community_id);
            // R2 F2: roll back per_library_count for OTHER libraries
            // whose attested contributions are also being dropped by
            // this eviction. Unattested contributions don't carry a
            // counter, so they need no rollback.
            for other in remaining_attested {
                if &other != library {
                    if let Some(c) = self.per_library_count.get_mut(&other) {
                        if *c > 0 {
                            *c -= 1;
                        }
                    }
                }
            }
        }
    }
}

/// In-memory discovered-libraries map populated by
/// `process_announce`. NOT persisted — rebuilt on startup from the
/// announce-topic subscription. Spec §4.2.
#[derive(Debug, Default)]
pub struct Announces {
    by_addr: BTreeMap<OwnerAddr, LibraryAnnounce>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnounceOutcome {
    /// New library address — first time seen.
    Inserted(OwnerAddr),
    /// Existing library, replaced by newer-HLC announce.
    Updated(OwnerAddr),
    /// Existing library, incoming has older/equal HLC.
    Idempotent,
}

/// Result of `process_announce`. The outer outcome and any
/// orthogonal cap-eviction are independent — both fields can be
/// populated when an at-cap insert evicts an unrelated library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnounceProcessResult {
    pub outcome: AnnounceOutcome,
    /// `Some(addr)` if the cap was hit and `addr` was evicted to make
    /// room for the incoming announce.
    pub evicted: Option<OwnerAddr>,
}

impl Announces {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.by_addr.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_addr.is_empty()
    }

    /// Snapshot for IPC return. Sorted by `listed_at` descending (newest
    /// first) so the UI surfaces fresh announces at the top. Ties on
    /// `listed_at` fall back to `OwnerAddr` byte order ascending for
    /// deterministic test output.
    pub fn snapshot(&self) -> Vec<LibraryAnnounce> {
        // Collect (addr, announce) pairs so we can stable-tie-break on
        // OwnerAddr when listed_at compares equal across entries.
        let mut pairs: Vec<(OwnerAddr, LibraryAnnounce)> =
            self.by_addr.iter().map(|(a, e)| (*a, e.clone())).collect();
        pairs.sort_by(|(addr_a, a), (addr_b, b)| {
            // Newer first: `b` strictly newer than `a` => b comes first.
            if b.listed_at.is_strictly_newer_than(&a.listed_at) {
                std::cmp::Ordering::Greater
            } else if a.listed_at.is_strictly_newer_than(&b.listed_at) {
                std::cmp::Ordering::Less
            } else {
                // Equal listed_at => stable tie-break by addr ascending.
                addr_a.cmp(addr_b)
            }
        });
        pairs.into_iter().map(|(_, e)| e).collect()
    }

    /// Process a verified announce. Caller MUST have run
    /// `verify_announce` first (which returns the derived `library_addr`).
    /// This method does NOT re-verify.
    pub fn on_announce(
        &mut self,
        library_addr: OwnerAddr,
        announce: LibraryAnnounce,
    ) -> AnnounceProcessResult {
        // Dedupe: latest-listed_at-wins (strict; equal HLC is idempotent).
        if let Some(existing) = self.by_addr.get(&library_addr) {
            if !announce
                .listed_at
                .is_strictly_newer_than(&existing.listed_at)
            {
                return AnnounceProcessResult {
                    outcome: AnnounceOutcome::Idempotent,
                    evicted: None,
                };
            }
            // Strictly newer — replace.
            self.by_addr.insert(library_addr, announce);
            return AnnounceProcessResult {
                outcome: AnnounceOutcome::Updated(library_addr),
                evicted: None,
            };
        }

        // Brand-new library — apply cap.
        let mut evicted: Option<OwnerAddr> = None;
        if self.by_addr.len() >= MAX_DISCOVERED_LIBRARIES {
            // Evict oldest-by-listed_at. Stable tie-break by addr byte
            // order ascending so eviction is deterministic across runs.
            if let Some(oldest_addr) = self
                .by_addr
                .iter()
                .min_by(|(addr_a, a), (addr_b, b)| {
                    if a.listed_at.is_strictly_newer_than(&b.listed_at) {
                        std::cmp::Ordering::Greater
                    } else if b.listed_at.is_strictly_newer_than(&a.listed_at) {
                        std::cmp::Ordering::Less
                    } else {
                        // Equal listed_at — evict by addr ascending.
                        addr_a.cmp(addr_b)
                    }
                })
                .map(|(addr, _)| *addr)
            {
                self.by_addr.remove(&oldest_addr);
                evicted = Some(oldest_addr);
            }
        }
        self.by_addr.insert(library_addr, announce);
        AnnounceProcessResult {
            outcome: AnnounceOutcome::Inserted(library_addr),
            evicted,
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
    /// Sub-D Phase 2: discovered-libraries map populated by the
    /// announce-topic subscriber. Spec §4.2.
    pub announces: Mutex<Announces>,
    pub request_tx: mpsc::UnboundedSender<LibraryDirectoryRequest>,
}

impl LibraryDirectory {
    /// Construct alongside the matching `request_rx` consumed by the
    /// event loop.
    pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<LibraryDirectoryRequest>) {
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let dir = Arc::new(Self {
            aggregation: Mutex::new(Aggregation::new()),
            announces: Mutex::new(Announces::new()),
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
        // Phase 1 attribution check: signed `listed_by` is the topic
        // owner's address. Phase 3 generalizes this — for wrapped
        // entries, the broadcasting library identity (from the
        // wrapping sig's library_identity_pub) is what must match the
        // topic owner. Library A republishing library B's entry has
        // listed_by=B but library_identity_pub=A. We require
        // library_identity_pub's derived addr == library_addr (the
        // topic owner). Unwrapped entries fall through to the Phase 1
        // listed_by == library_addr semantics.
        let status = verify_entry(&entry).map_err(ProcessSampleError::Verify)?;
        let broadcasting_lib = match status {
            AttestationStatus::Attested(addr) | AttestationStatus::Unattested(addr) => addr,
            AttestationStatus::Unwrapped => entry.listed_by,
        };
        if broadcasting_lib != library_addr {
            return Err(ProcessSampleError::AttributionMismatch {
                expected: library_addr,
                actual: broadcasting_lib,
            });
        }
        let mut agg = self.aggregation.lock().await;
        Ok(agg.on_entry(entry, status))
    }

    /// Sub-D Phase 2: ingest one announce-topic sample. Decodes, verifies,
    /// then inserts/updates the announces map. Returns the outcome so the
    /// caller can emit `library-directory-updated` on non-`Idempotent`
    /// changes (or on orthogonal cap-eviction).
    pub async fn process_announce(
        &self,
        bytes: Vec<u8>,
    ) -> Result<AnnounceProcessResult, AnnounceVerifyError> {
        let announce: LibraryAnnounce = ciborium::from_reader(&bytes[..])
            .map_err(|e| AnnounceVerifyError::DecodeFailed(format!("{e}")))?;
        let library_addr = verify_announce(&announce, crate::clock_trust::receiver_now_ms())?;
        let mut announces = self.announces.lock().await;
        Ok(announces.on_announce(library_addr, announce))
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

/// Frontend-facing DTO returned by `list_discovered_libraries` (Sub-D
/// Phase 2, ZEB-279). Surfaces libraries the user has *discovered* via
/// the `harmony/discovery/library/announce` topic but has NOT yet
/// added to their trust set. Filtered at the IPC layer against
/// non-tombstoned entries in `OwnerState.libraries`, so a library
/// migrates from the "Discovered" panel to "Your libraries" on the
/// next refetch after `add_library`.
///
/// `listed_at` is the raw `wall_ms` rendered as a base-10 string. The
/// frontend formats for display; callers MUST NOT use this for HLC
/// ordering decisions (HLC ordering happens inside `Announces`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredLibraryInfo {
    /// Hex-encoded 16-byte library OwnerAddr (32 hex chars).
    pub library_addr: String,
    pub name: String,
    pub description: String,
    /// `listed_at.wall_ms` as base-10 string for display only.
    pub listed_at: String,
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
    /// Sub-D Phase 3 (ZEB-280): count of libraries with valid attestation
    /// for this entry (i.e., `attested_by.len()`). Includes Phase 1
    /// unwrapped contributions (which fall back to entry.listed_by).
    pub listed_by_count: usize,
    /// Sub-D Phase 3 (ZEB-280): true if at least one broadcasting
    /// library's wrapping sig failed to verify (`!unattested_by.is_empty()`).
    /// Drives the inline "unattested" badge in the frontend browser.
    pub unattested: bool,
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
            listed_by_count: agg.attested_by.len(),
            unattested: !agg.unattested_by.is_empty(),
            listed_at: agg.entry.listed_at.clone(),
        }
    }
}

/// ZEB-252 Sub-D Phase 6: find the open-community `invite_url` for a
/// given hex-encoded `community_id` in a directory snapshot.
///
/// Returns the entry's `invite_url` on success. Errors:
/// - No matching entry: `"This community is no longer listed by any of your libraries"`
///   (the user-facing race-window message from spec §4.3).
/// - Matching entry but its invite URL decodes to `is_invite_only == true`:
///   `"Invite-only community cannot be joined directly from the directory"`
///   (belt-and-suspenders per spec §4.4 — Phase 1's `verify_entry` already
///   rejects invite-only URLs at receive, so this branch is unreachable in
///   practice; the re-check defends against future Phase 1 regressions).
/// - Malformed `community_id_hex`: bubbles a "invalid hex" / "wrong length" message.
///
/// Pure function. Caller supplies the snapshot (typically the result of
/// `LibraryDirectory::snapshot_all().await`).
pub fn find_open_community_invite_url_in_snapshot(
    snapshot: &[AggregatedEntry],
    community_id_hex: &str,
) -> Result<String, String> {
    // 1. Parse community_id_hex into a SpaceId for the comparison key.
    let id_bytes: [u8; 16] = hex::decode(community_id_hex)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let want = crate::owner_state_types::SpaceId(id_bytes);

    // 2. Find the matching aggregated entry.
    let agg = snapshot
        .iter()
        .find(|a| a.entry.community_id == want)
        .ok_or_else(|| "This community is no longer listed by any of your libraries".to_string())?;

    // 3. Defensive re-checks (spec §4.4 + CodeRabbit PR #113):
    //    decode the invite URL and verify both
    //    (a) `is_invite_only == false` and
    //    (b) the payload's `community_id` matches the entry's `community_id`.
    //    Phase 1's `verify_entry` (lines 359-395) already enforces BOTH
    //    bindings at receive time, so a malformed entry should be unreachable
    //    here — but the re-checks defend against future regressions in
    //    `verify_entry`. The community_id bind is the load-bearing
    //    anti-phishing check: without it, a malicious entry advertising
    //    community A's metadata but carrying community B's invite_url could
    //    silently redirect the join (the UI shows A, the user clicks Join,
    //    we'd redeem into B).
    let payload = crate::community_invite::decode_invite_url(&agg.entry.invite_url)
        .map_err(|e| format!("directory entry's invite URL failed to decode: {e:?}"))?;
    if payload.is_invite_only {
        return Err("Invite-only community cannot be joined directly from the directory".into());
    }
    if payload.community_id != agg.entry.community_id {
        return Err(
            "Directory entry is malformed: invite URL's community_id does not match the entry's"
                .into(),
        );
    }

    Ok(agg.entry.invite_url.clone())
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
    ///
    /// R2 F1: `verify_entry` now binds the payload's `community_id` and
    /// `admin_addr` to the signed entry, so callers must pass the
    /// matching values. The legacy zero-arg call sites historically used
    /// `SpaceId([0; 16])` + `OwnerAddr([0; 16])` for both; the
    /// `build_open_invite_url_default()` shim preserves that for callers
    /// where the binding isn't load-bearing.
    fn build_open_invite_url_for(community_id: SpaceId, admin_addr: OwnerAddr) -> String {
        let payload = CommunityInvitePayload {
            inviter_signer_certs: Vec::new(),
            community_id,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: vec![0u8; 32], // open communities use 32-byte sealed_epoch_key
                sealed_epoch_keys: Vec::new(),
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr,
            community_name: "test".to_string(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
            admin_bootstrap: None,
            admin_identity_pub: None,
            forked_from: None,
            pre_fork_snapshot: None,
            inviter_enrollment: None,
            untargeted_decrypt_key: None,
        };
        encode_invite_url(&payload).expect("encode open invite url")
    }

    /// Build an open-community invite URL whose payload's `community_id`
    /// and `admin_addr` match what `build_signed_entry(community_id,
    /// admin_seed, ...)` produces. Derives the admin_addr from the same
    /// seed via the identity_pub the entry will be signed under, so
    /// `verify_entry`'s payload-consistency checks (R2 F1) pass.
    fn build_matching_open_invite_url(community_id: SpaceId, admin_seed: [u8; 32]) -> String {
        let (_signing_key, identity_pub) = build_test_identity_pub(admin_seed);
        let admin_addr = OwnerAddr(
            harmony_identity::Identity::from_public_bytes(&identity_pub)
                .expect("identity from pub")
                .address_hash,
        );
        build_open_invite_url_for(community_id, admin_addr)
    }

    /// Convenience for `build_signed_entry(SpaceId([1; 16]), [7; 32], ...)`
    /// — the most common fixture combination in this module's tests.
    /// Pre-binds the invite payload's `community_id` and `admin_addr` to
    /// the values the entry will be signed under, so the R2 F1 payload-
    /// consistency check passes.
    fn build_open_invite_url() -> String {
        build_matching_open_invite_url(SpaceId([1; 16]), [7; 32])
    }

    /// Build an invite-only invite URL — same machinery but with
    /// `is_invite_only == true` so the discipline check rejects it.
    fn build_invite_only_url() -> String {
        use crate::community_invite::InviteToken;
        use crate::community_membership::{MembershipEventKind, SignedMembershipEvent};
        let admin_addr = OwnerAddr([0u8; 16]);
        let community_id = SpaceId([0u8; 16]);
        let admin_bootstrap = SignedMembershipEvent {
            signer_certs: Vec::new(),
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
            // ZEB-339: bootstrap-Join must embed the admin's EnrollmentCert.
            enrollment: Some(crate::community_membership::mint_test_owner(0xC3).cert),
        };
        let payload = CommunityInvitePayload {
            inviter_signer_certs: Vec::new(),
            community_id,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: vec![0u8; 92], // invite-only: 92-byte sealed_epoch_key
                sealed_epoch_keys: Vec::new(),
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
            forked_from: None,
            pre_fork_snapshot: None,
            // ZEB-339: invite-only payloads must carry the inviter's
            // EnrollmentCert. The directory rejects invite-only entries at
            // verify time regardless of cert content, so any valid cert suffices.
            inviter_enrollment: Some(crate::community_membership::mint_test_owner(0xA1).cert),
            // ZEB-367: an untargeted invite-only payload (invitee_hint None) must
            // carry the URL decrypt key or encode_invite_url rejects it
            // (UntargetedKeyMissing). Content is irrelevant here — these directory
            // tests reject invite-only entries at verify time regardless.
            untargeted_decrypt_key: Some([0u8; 32]),
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
            library_identity_pub: None,
            library_signature: None,
        };
        // Sign over canonical CBOR of all fields with community_signature
        // zeroed — verify_entry zeroes the same field before recomputing.
        let bytes = canonical_cbor_encode(&entry).expect("encode for sign");
        let sig = signing_key.sign(&bytes);
        entry.community_signature = sig.to_bytes();
        entry
    }

    /// ZEB-280 Phase 3: variant of `build_signed_entry` that lets the
    /// caller bind a specific `community_id` and `admin_seed` while
    /// auto-deriving an invite URL whose payload binds to the same
    /// community_id and the admin_addr derived from the seed (so
    /// `verify_entry`'s R2 F1 payload-consistency check passes). Returns
    /// a Phase 1-style entry (`library_identity_pub = None`,
    /// `library_signature = None`) ready to be wrapped via `wrap_entry`.
    fn build_signed_open_entry_for(
        community_id: SpaceId,
        admin_seed: [u8; 32],
    ) -> LibraryDirectoryEntry {
        build_signed_open_entry_for_library(community_id, admin_seed, OwnerAddr([0xAA; 16]))
    }

    /// ZEB-280 Phase 3: variant of `build_signed_open_entry_for` that
    /// also binds an explicit `listed_by` OwnerAddr — used by aggregation
    /// tests that need a Phase 1-shaped entry whose admin-signed
    /// `listed_by` field is independent of the broadcasting library
    /// passed via `AttestationStatus`.
    fn build_signed_open_entry_for_library(
        community_id: SpaceId,
        admin_seed: [u8; 32],
        listed_by: OwnerAddr,
    ) -> LibraryDirectoryEntry {
        let invite_url = build_matching_open_invite_url(community_id, admin_seed);
        build_signed_entry(
            community_id,
            admin_seed,
            listed_by,
            Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "test".to_string(),
            },
            invite_url,
        )
    }

    /// ZEB-280 Phase 3: build a library signer + 64-byte identity bundle.
    /// Uses a distinct X25519 prefix (`[0x22; 32]`) from
    /// `build_test_identity_pub`'s admin prefix (`[0x11; 32]`) so admin
    /// and library identities derived from the same Ed25519 seed produce
    /// distinct `OwnerAddr`s.
    fn build_test_library_identity(seed: [u8; 32]) -> (SigningKey, [u8; 64]) {
        let signing = SigningKey::from_bytes(&seed);
        let verifying = signing.verifying_key().to_bytes();
        let mut bundle = [0u8; 64];
        bundle[..32].copy_from_slice(&[0x22; 32]);
        bundle[32..].copy_from_slice(&verifying);
        (signing, bundle)
    }

    /// ZEB-280 Phase 3: take an admin-signed entry, sign it with
    /// `library_signing_key` over canonical CBOR with `library_signature`
    /// zeroed (mirror of the verifier's reconstruction).
    fn wrap_entry(
        mut entry: LibraryDirectoryEntry,
        library_signing_key: &SigningKey,
        library_identity_bundle: [u8; 64],
    ) -> LibraryDirectoryEntry {
        entry.library_identity_pub = Some(library_identity_bundle);
        entry.library_signature = None;
        let signed_bytes = canonical_cbor_encode(&entry).expect("encode for lib sign");
        entry.library_signature = Some(library_signing_key.sign(&signed_bytes).to_bytes());
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
        assert!(
            matches!(verify_entry(&entry), Ok(AttestationStatus::Unwrapped)),
            "signed entry must verify as Unwrapped"
        );
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
            library_identity_pub: None,
            library_signature: None,
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

    /// R2 F1 (Critical, security): an entry advertising community A but
    /// embedding an `invite_url` whose decoded payload points at
    /// community B must be rejected. Otherwise a malicious directory
    /// publisher could sign an entry presenting A's name/description in
    /// the UI while the Join action redeems B (phishing).
    #[test]
    fn invite_url_pointing_at_different_community_rejected() {
        let community_a = SpaceId([0xAA; 16]);
        let community_b = SpaceId([0xBB; 16]);
        // Invite payload points at community_b, but entry will be signed
        // advertising community_a. Admin addr matches the entry's seed so
        // the admin-identity check would otherwise pass — only the
        // community_id mismatch should trigger.
        let invite_url_for_b = build_matching_open_invite_url(community_b, [7; 32]);
        let entry = build_signed_entry(
            community_a,
            [7; 32],
            OwnerAddr([2; 16]),
            Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "d".into(),
            },
            invite_url_for_b,
        );
        let result = verify_entry(&entry);
        assert!(
            matches!(
                result,
                Err(EntryVerifyError::PayloadCommunityIdMismatch { .. })
            ),
            "expected PayloadCommunityIdMismatch, got {:?}",
            result
        );
    }

    /// R2 F1 (Critical, security): an entry whose `community_admin_identity_pub`
    /// is signed by admin X but embeds an `invite_url` whose payload's
    /// `admin_addr` derives from a DIFFERENT admin Y must be rejected.
    /// Otherwise a malicious publisher could present X's reputation in
    /// the UI while the Join action redeems Y's epoch state.
    #[test]
    fn invite_url_pointing_at_different_admin_rejected() {
        let community = SpaceId([1; 16]);
        // Build an open invite URL whose admin_addr binds to seed=[9; 32]
        // (admin Y). The entry will be signed with seed=[7; 32] (admin X)
        // for the SAME community_id, so only the admin-identity check
        // should fire.
        let invite_url_admin_y = build_matching_open_invite_url(community, [9; 32]);
        let entry = build_signed_entry(
            community,
            [7; 32],
            OwnerAddr([2; 16]),
            Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "d".into(),
            },
            invite_url_admin_y,
        );
        let result = verify_entry(&entry);
        assert!(
            matches!(
                result,
                Err(EntryVerifyError::PayloadAdminIdentityMismatch { .. })
            ),
            "expected PayloadAdminIdentityMismatch, got {:?}",
            result
        );
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
            agg.on_entry(e1, AttestationStatus::Unwrapped).outcome,
            OnEntryOutcome::Inserted(community)
        );
        assert_eq!(
            agg.on_entry(e2.clone(), AttestationStatus::Unwrapped)
                .outcome,
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
            agg.on_entry(e_from_a, AttestationStatus::Unwrapped).outcome,
            OnEntryOutcome::Inserted(community)
        );
        assert_eq!(
            agg.on_entry(e_from_b, AttestationStatus::Unwrapped).outcome,
            OnEntryOutcome::AccretedListedBy(community)
        );

        let snap = agg.snapshot_all();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].attested_by.len(), 2);
        assert!(snap[0].attested_by.contains(&library_a));
        assert!(snap[0].attested_by.contains(&library_b));
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

        agg.on_entry(
            build_signed_entry(solo, [7; 32], library_a, h_old.clone(), invite_url.clone()),
            AttestationStatus::Unwrapped,
        );
        agg.on_entry(
            build_signed_entry(
                shared,
                [7; 32],
                library_a,
                h_old.clone(),
                invite_url.clone(),
            ),
            AttestationStatus::Unwrapped,
        );
        // library_b publishes at a NEWER HLC so its entry becomes the
        // stored source — required for the F3-rule shared-retention
        // path to apply.
        agg.on_entry(
            build_signed_entry(
                shared,
                [7; 32],
                library_b,
                h_new.clone(),
                invite_url.clone(),
            ),
            AttestationStatus::Unwrapped,
        );

        let evicted = agg.drop_library(&library_a);
        assert_eq!(evicted, vec![solo]);
        let snap = agg.snapshot_all();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].entry.community_id, shared);
        assert_eq!(snap[0].attested_by, [library_b].into_iter().collect());
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
        agg.on_entry(entry_a, AttestationStatus::Unwrapped);

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
        agg.on_entry(entry_b, AttestationStatus::Unwrapped);

        // Sanity: pre-drop state has community C listed by both, stored
        // entry sourced from B.
        let snap = agg.snapshot_all();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].entry.name, "from-B");
        assert_eq!(snap[0].attested_by.len(), 2);

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

    /// R2 F2 regression: when `drop_library` evicts a community via the
    /// F3 source-matches rule (R1), OTHER libraries that were in
    /// `listed_by` previously had their `per_library_count` incremented
    /// for that community via the accretion path. Those counts must be
    /// decremented when the community is dropped, or the counter drifts
    /// upward → premature cap enforcement.
    ///
    /// Scenario: library A publishes community C at HLC 100; library B
    /// publishes for the SAME C at HLC 200 (newer-HLC wins → stored
    /// entry sourced from A is overwritten, listed_by={A, B}, B's count
    /// rises to 1 via accretion). Drop A — F3-rule eviction fires
    /// because stored entry's listed_by == A. Without R2 F2, B's count
    /// stays at 1 while no community is actually listed by B.
    #[test]
    fn drop_library_decrements_per_library_count_for_other_libraries_when_evicting() {
        let mut agg = Aggregation::new();
        let library_a = OwnerAddr([0xAA; 16]);
        let library_b = OwnerAddr([0xBB; 16]);
        let community = SpaceId([1; 16]);
        let invite_url = build_open_invite_url();

        // Library A publishes community at HLC 100 — becomes the stored
        // source.
        agg.on_entry(
            build_signed_entry(
                community,
                [7; 32],
                library_a,
                Hlc {
                    wall_ms: 100,
                    logical: 0,
                    device_id: "d".into(),
                },
                invite_url.clone(),
            ),
            AttestationStatus::Unwrapped,
        );
        // Library B publishes SAME community at HLC 50 (older) — does
        // NOT replace stored entry but DOES accrete to listed_by and
        // increments B's per_library_count.
        agg.on_entry(
            build_signed_entry(
                community,
                [7; 32],
                library_b,
                Hlc {
                    wall_ms: 50,
                    logical: 0,
                    device_id: "d".into(),
                },
                invite_url.clone(),
            ),
            AttestationStatus::Unwrapped,
        );

        assert_eq!(
            agg.entry_count_for_library(&library_a),
            1,
            "A contributed via Inserted path"
        );
        assert_eq!(
            agg.entry_count_for_library(&library_b),
            1,
            "B contributed via AccretedListedBy path"
        );

        // Drop A → F3-rule eviction fires (stored entry's listed_by ==
        // A). Community is FULLY evicted. B's count must be decremented
        // because B no longer contributes to ANY community.
        let evicted = agg.drop_library(&library_a);
        assert_eq!(evicted, vec![community]);
        assert_eq!(
            agg.entry_count_for_library(&library_a),
            0,
            "A's count cleared by drop_library"
        );
        assert_eq!(
            agg.entry_count_for_library(&library_b),
            0,
            "R2 F2: B's count must be decremented when source-matches eviction removes the only community B was listing"
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
            let result = agg.on_entry(entry, AttestationStatus::Unwrapped);
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

        agg.on_entry(
            build_signed_entry(community, [7; 32], library, h_new, invite_url.clone()),
            AttestationStatus::Unwrapped,
        );
        let result = agg.on_entry(
            build_signed_entry(community, [7; 32], library, h_old, invite_url.clone()),
            AttestationStatus::Unwrapped,
        );
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

    /// ZEB-280 Phase 3: an entry with `library_identity_pub` and
    /// `library_signature` both populated (static `[0u8; 64]` sentinel
    /// bytes here — Task 2 adds real-signer verifier tests) round-trips
    /// through canonical CBOR and the bstr serde helpers correctly.
    #[test]
    fn phase3_wrapped_entry_roundtrips_via_canonical_cbor() {
        let entry = LibraryDirectoryEntry {
            community_id: SpaceId([0x11; 16]),
            community_admin_identity_pub: [0x11; 64],
            name: "Phase 3 test".to_string(),
            description: "Round-trip test for wrapped entry".to_string(),
            topics: vec!["test".to_string()],
            invite_url: "harmony://invite/?p=AAAA".to_string(),
            listed_by: OwnerAddr([0xAA; 16]),
            listed_at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "test".to_string(),
            },
            community_signature: [0u8; 64],
            library_identity_pub: Some([0xBB; 64]),
            library_signature: Some([0xCC; 64]),
        };
        let bytes = canonical_cbor_encode(&entry).expect("encode");
        let decoded: LibraryDirectoryEntry = ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(entry, decoded, "wrapped entry round-trips");
    }

    /// ZEB-280 Phase 3: a Phase 1-style entry (both Optional fields
    /// `None`) round-trips and the decoded fields are still `None`
    /// (the `#[serde(default)]` attribute lets ciborium decode missing
    /// fields as `None`).
    #[test]
    fn phase1_unwrapped_entry_roundtrips_with_optional_fields_absent() {
        let entry = LibraryDirectoryEntry {
            community_id: SpaceId([0x22; 16]),
            community_admin_identity_pub: [0x33; 64],
            name: "Phase 1 test".to_string(),
            description: "Backward compat check".to_string(),
            topics: vec![],
            invite_url: "harmony://invite/?p=AAAA".to_string(),
            listed_by: OwnerAddr([0x44; 16]),
            listed_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: String::new(),
            },
            community_signature: [0x55; 64],
            library_identity_pub: None,
            library_signature: None,
        };
        let bytes = canonical_cbor_encode(&entry).expect("encode");
        let decoded: LibraryDirectoryEntry = ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(entry, decoded, "Phase 1-shaped entry round-trips");
        assert_eq!(decoded.library_identity_pub, None);
        assert_eq!(decoded.library_signature, None);
    }

    /// ZEB-280 Phase 3: Phase 1-style entry (Optional fields both None)
    /// verifies as `AttestationStatus::Unwrapped`.
    #[test]
    fn verify_entry_phase1_unwrapped_returns_unwrapped() {
        let community_id = SpaceId([0x11; 16]);
        let admin_seed = [7u8; 32];
        let entry = build_signed_open_entry_for(community_id, admin_seed);
        // (build_signed_open_entry_for returns an entry with li=None, ls=None)

        match verify_entry(&entry) {
            Ok(AttestationStatus::Unwrapped) => {}
            other => panic!("expected Unwrapped, got {other:?}"),
        }
    }

    /// ZEB-280 Phase 3: a wrapped entry with a valid library signature
    /// verifies as `AttestationStatus::Attested(library_addr)`.
    #[test]
    fn verify_entry_phase3_wrapped_valid_returns_attested() {
        let community_id = SpaceId([0x22; 16]);
        let admin_seed = [8u8; 32];
        let admin_entry = build_signed_open_entry_for(community_id, admin_seed);

        let (lib_signing, lib_bundle) = build_test_library_identity([9u8; 32]);
        let wrapped = wrap_entry(admin_entry, &lib_signing, lib_bundle);

        let expected_lib_addr = {
            let id = harmony_identity::Identity::from_public_bytes(&lib_bundle)
                .expect("library identity");
            OwnerAddr(id.address_hash)
        };

        match verify_entry(&wrapped) {
            Ok(AttestationStatus::Attested(addr)) => {
                assert_eq!(addr, expected_lib_addr);
            }
            other => panic!("expected Attested, got {other:?}"),
        }
    }

    /// ZEB-280 Phase 3: a wrapped entry with a TAMPERED library
    /// signature returns `Ok(AttestationStatus::Unattested(library_addr))`.
    /// The entry is NOT dropped — admin sig still valid.
    #[test]
    fn verify_entry_phase3_tampered_wrapping_sig_returns_unattested() {
        let community_id = SpaceId([0x33; 16]);
        let admin_seed = [10u8; 32];
        let admin_entry = build_signed_open_entry_for(community_id, admin_seed);

        let (lib_signing, lib_bundle) = build_test_library_identity([11u8; 32]);
        let mut wrapped = wrap_entry(admin_entry, &lib_signing, lib_bundle);

        // Tamper the library signature.
        let mut bad_sig = wrapped.library_signature.expect("wrapping sig present");
        bad_sig[0] ^= 0xFF;
        wrapped.library_signature = Some(bad_sig);

        let expected_lib_addr = {
            let id = harmony_identity::Identity::from_public_bytes(&lib_bundle)
                .expect("library identity");
            OwnerAddr(id.address_hash)
        };

        match verify_entry(&wrapped) {
            Ok(AttestationStatus::Unattested(addr)) => {
                assert_eq!(
                    addr, expected_lib_addr,
                    "Unattested still carries the CLAIMED library addr"
                );
            }
            other => panic!("expected Unattested, got {other:?}"),
        }
    }

    /// ZEB-280 Phase 3: if the entry's payload is tampered (e.g., name
    /// field changed), the ADMIN sig fails FIRST, and the entry is
    /// dropped via `Err(SignatureInvalid)`. The wrapping sig is not
    /// even reached — admin sig is the gatekeeper.
    #[test]
    fn verify_entry_phase3_tampered_payload_invalidates_both_sigs() {
        let community_id = SpaceId([0x44; 16]);
        let admin_seed = [12u8; 32];
        let admin_entry = build_signed_open_entry_for(community_id, admin_seed);
        let (lib_signing, lib_bundle) = build_test_library_identity([13u8; 32]);
        let mut wrapped = wrap_entry(admin_entry, &lib_signing, lib_bundle);

        // Tamper the payload (name) AFTER both sigs were applied.
        wrapped.name = "TAMPERED".to_string();

        match verify_entry(&wrapped) {
            Err(EntryVerifyError::SignatureInvalid) => {}
            other => panic!("expected admin SignatureInvalid, got {other:?}"),
        }
    }

    /// ZEB-280 Phase 3: an entry with `library_signature = Some` but
    /// `library_identity_pub = None` returns
    /// `Err(LibrarySignatureFieldsInconsistent)`.
    #[test]
    fn verify_entry_inconsistent_library_fields_rejected_lib_sig_only() {
        let community_id = SpaceId([0x55; 16]);
        let admin_seed = [14u8; 32];
        let mut entry = build_signed_open_entry_for(community_id, admin_seed);
        entry.library_signature = Some([0xAA; 64]);
        entry.library_identity_pub = None;

        // Admin sig is over (cs=0, li=None, ls=None) — but the entry
        // now has li=None, ls=Some. The admin sig verifier reconstructs
        // by setting cs=0, li=None, ls=None — so admin sig still
        // verifies (unchanged). Then we hit the inconsistency check.
        match verify_entry(&entry) {
            Err(EntryVerifyError::LibrarySignatureFieldsInconsistent) => {}
            other => panic!("expected LibrarySignatureFieldsInconsistent, got {other:?}"),
        }
    }

    /// ZEB-280 Phase 3: an entry with `library_identity_pub = Some` but
    /// `library_signature = None` returns
    /// `Err(LibrarySignatureFieldsInconsistent)`.
    #[test]
    fn verify_entry_inconsistent_library_fields_rejected_lib_pub_only() {
        let community_id = SpaceId([0x66; 16]);
        let admin_seed = [15u8; 32];
        let mut entry = build_signed_open_entry_for(community_id, admin_seed);
        entry.library_identity_pub = Some([0xBB; 64]);
        entry.library_signature = None;

        match verify_entry(&entry) {
            Err(EntryVerifyError::LibrarySignatureFieldsInconsistent) => {}
            other => panic!("expected LibrarySignatureFieldsInconsistent, got {other:?}"),
        }
    }

    /// ZEB-280 Phase 3: an entry with a malformed `library_identity_pub`
    /// (bytes that fail `Identity::from_public_bytes`) returns
    /// `Err(InvalidLibraryIdentityPub)`.
    #[test]
    fn verify_entry_malformed_library_identity_pub_rejected() {
        let community_id = SpaceId([0x77; 16]);
        let admin_seed = [16u8; 32];
        let admin_entry = build_signed_open_entry_for(community_id, admin_seed);
        let (lib_signing, lib_bundle) = build_test_library_identity([17u8; 32]);

        // Wrap with the GOOD bundle (so the wrapping sig is valid for
        // this bundle), then SWAP IN a malformed bundle. The wrapping
        // sig won't verify against the malformed pub, but we never
        // get that far — the Identity::from_public_bytes check fires
        // first.
        let mut wrapped = wrap_entry(admin_entry, &lib_signing, lib_bundle);
        // Ed25519 half `[0x7F; 32]` doesn't decompress under
        // ed25519-dalek 2.x / curve25519-dalek 4.x — same fixture used by
        // `malformed_identity_pub_rejected` for the admin-side check.
        let mut malformed_bundle = [0u8; 64];
        malformed_bundle[32..].copy_from_slice(&[0x7F; 32]);
        wrapped.library_identity_pub = Some(malformed_bundle);

        match verify_entry(&wrapped) {
            Err(EntryVerifyError::InvalidLibraryIdentityPub(_)) => {}
            other => panic!("expected InvalidLibraryIdentityPub, got {other:?}"),
        }
    }

    /// ZEB-280 Phase 3: an `AttestationStatus::Unwrapped` entry
    /// falls back to `entry.listed_by` when inserting into the
    /// `attested_by` set.
    #[test]
    fn aggregation_on_entry_unwrapped_inserts_into_attested_by_via_listed_by_fallback() {
        let mut agg = Aggregation::new();
        let community_id = SpaceId([0x11; 16]);
        let library = OwnerAddr([0xAA; 16]);
        let entry = build_signed_open_entry_for_library(community_id, [7u8; 32], library);
        let _ = agg.on_entry(entry, AttestationStatus::Unwrapped);

        let snap = agg.snapshot_all();
        assert_eq!(snap.len(), 1);
        assert!(
            snap[0].attested_by.contains(&library),
            "Unwrapped path should insert listed_by into attested_by"
        );
        assert!(
            snap[0].unattested_by.is_empty(),
            "no unattested contributions"
        );
    }

    /// ZEB-280 Phase 3: an `AttestationStatus::Attested(lib_addr)`
    /// entry inserts `lib_addr` (NOT `entry.listed_by`) into
    /// `attested_by`.
    #[test]
    fn aggregation_on_entry_attested_inserts_into_attested_by_via_lib_addr() {
        let mut agg = Aggregation::new();
        let community_id = SpaceId([0x22; 16]);
        let listed_by = OwnerAddr([0xAA; 16]);
        let lib_addr = OwnerAddr([0xBB; 16]);
        // Federation case: admin signed listed_by=A but broadcaster is B.
        let entry = build_signed_open_entry_for_library(community_id, [8u8; 32], listed_by);
        let _ = agg.on_entry(entry, AttestationStatus::Attested(lib_addr));

        let snap = agg.snapshot_all();
        assert_eq!(snap.len(), 1);
        assert!(
            snap[0].attested_by.contains(&lib_addr),
            "Attested(lib_addr) inserts the broadcasting lib, NOT listed_by"
        );
        assert!(
            !snap[0].attested_by.contains(&listed_by),
            "listed_by is NOT inserted when status is Attested"
        );
    }

    /// ZEB-280 Phase 3 (R1, CodeRabbit + Qodo): an
    /// `AttestationStatus::Unattested(lib_addr)` entry inserts
    /// `lib_addr` into `unattested_by` ONLY when the community has a
    /// prior attestation. The DTO surfaces `unattested = true`. The
    /// disjoint-sets invariant (attested_by ∩ unattested_by = ∅ per
    /// community) means an unattested broadcast from library X for a
    /// community where X is the only attester is a no-op.
    #[test]
    fn aggregation_on_entry_unattested_inserts_into_unattested_by_when_community_already_attested()
    {
        let mut agg = Aggregation::new();
        let community_id = SpaceId([0x33; 16]);
        let attested_lib = OwnerAddr([0xAA; 16]);
        let unattested_lib = OwnerAddr([0xCC; 16]);

        // Step 1: legitimate attested broadcast creates the community.
        let entry = build_signed_open_entry_for_library(community_id, [9u8; 32], attested_lib);
        let _ = agg.on_entry(entry.clone(), AttestationStatus::Attested(attested_lib));

        // Step 2: unattested broadcast from a DIFFERENT library on
        // existing community.
        let _ = agg.on_entry(entry, AttestationStatus::Unattested(unattested_lib));

        let snap = agg.snapshot_all();
        assert_eq!(snap.len(), 1);
        assert!(snap[0].attested_by.contains(&attested_lib));
        assert!(snap[0].unattested_by.contains(&unattested_lib));
        assert!(
            !snap[0].attested_by.contains(&unattested_lib),
            "library never appears in BOTH sets — disjoint invariant"
        );

        let dto = DirectoryEntryDTO::from_aggregated(&snap[0]);
        assert!(dto.unattested);
    }

    /// ZEB-280 Phase 3 R1 (Qodo finding — DoS prevention): Unattested
    /// entries for communities that have no prior attestation are
    /// DROPPED entirely. Prevents a network adversary on a library's
    /// open Zenoh topic from creating fake-community memory pressure
    /// via bad-sig broadcasts with `library_identity_pub` spoofed.
    #[test]
    fn aggregation_on_entry_unattested_dropped_when_no_prior_attestation() {
        let mut agg = Aggregation::new();
        let community_id = SpaceId([0x33; 16]);
        let lib_addr = OwnerAddr([0xCC; 16]);
        let entry =
            build_signed_open_entry_for_library(community_id, [9u8; 32], OwnerAddr([0xAA; 16]));

        let result = agg.on_entry(entry, AttestationStatus::Unattested(lib_addr));

        assert!(matches!(result.outcome, OnEntryOutcome::Idempotent));
        assert!(
            agg.snapshot_all().is_empty(),
            "Unattested entry for new community dropped — no aggregation created"
        );
        assert_eq!(
            agg.entry_count_for_library(&lib_addr),
            0,
            "no per-library count bump for dropped Unattested"
        );
    }

    /// ZEB-280 Phase 3 R1 (Qodo finding — cap-bypass prevention):
    /// `per_library_count` tracks attested+unwrapped contributions
    /// only. Unattested broadcasts (potentially from a network
    /// adversary on the library's topic) cannot pump the count and
    /// cause cap eviction of the library's legitimate attestations.
    #[test]
    fn aggregation_unattested_does_not_count_toward_per_library_cap() {
        let mut agg = Aggregation::new();
        let attested_lib = OwnerAddr([0xAA; 16]);
        let unattested_lib = OwnerAddr([0xCC; 16]);

        // Create an attested community.
        let entry = build_signed_open_entry_for_library(SpaceId([1; 16]), [9u8; 32], attested_lib);
        let _ = agg.on_entry(entry, AttestationStatus::Attested(attested_lib));

        // Have unattested_lib unattested-broadcast many times for this
        // community. Under R1 policy: each is a no-op for cap (community
        // exists, after first insertion unattested_lib is in
        // unattested_by; per_library_count NEVER counts unattested).
        for _ in 0..100 {
            let entry =
                build_signed_open_entry_for_library(SpaceId([1; 16]), [9u8; 32], attested_lib);
            let _ = agg.on_entry(entry, AttestationStatus::Unattested(unattested_lib));
        }

        assert_eq!(
            agg.entry_count_for_library(&unattested_lib),
            0,
            "Unattested broadcasts don't count toward cap"
        );
        assert_eq!(
            agg.entry_count_for_library(&attested_lib),
            1,
            "Attested broadcast counts once"
        );
    }

    /// ZEB-280 Phase 3: drop_library sweeps BOTH attested_by and
    /// unattested_by sets — the per_library_count decrements for the
    /// dropped library, and OTHER libraries' counts roll back when
    /// the source-matches eviction rule fires.
    #[test]
    fn aggregation_drop_library_sweeps_both_attestation_sets() {
        let mut agg = Aggregation::new();
        let community_id = SpaceId([0x44; 16]);
        let library_a = OwnerAddr([0xAA; 16]);
        let library_b = OwnerAddr([0xBB; 16]);

        // Library A attests via Unwrapped (listed_by=A); Library B
        // also broadcasts the same community but with a TAMPERED
        // wrapping sig, so they land in unattested_by.
        let entry_a = build_signed_open_entry_for_library(community_id, [10u8; 32], library_a);
        let _ = agg.on_entry(entry_a, AttestationStatus::Unwrapped);

        let entry_b = build_signed_open_entry_for_library(community_id, [10u8; 32], library_a);
        let _ = agg.on_entry(entry_b, AttestationStatus::Unattested(library_b));

        let snap_before = agg.snapshot_all();
        assert_eq!(snap_before.len(), 1);
        assert!(snap_before[0].attested_by.contains(&library_a));
        assert!(snap_before[0].unattested_by.contains(&library_b));

        // Drop library_b — should sweep it from unattested_by, NOT
        // evict the community (library_a still attests).
        let evicted = agg.drop_library(&library_b);
        assert!(
            evicted.is_empty(),
            "library_b drop should not evict community (library_a still attests)"
        );
        let snap_after_b = agg.snapshot_all();
        assert_eq!(snap_after_b.len(), 1);
        assert!(snap_after_b[0].attested_by.contains(&library_a));
        assert!(
            !snap_after_b[0].unattested_by.contains(&library_b),
            "library_b swept from unattested_by"
        );

        // Drop library_a — should evict (last remaining contributor).
        let evicted = agg.drop_library(&library_a);
        assert_eq!(
            evicted,
            vec![community_id],
            "library_a drop should evict the community"
        );
        let snap_after_a = agg.snapshot_all();
        assert!(
            snap_after_a.is_empty(),
            "community evicted after both drops"
        );
    }

    // ── ZEB-252 Sub-D Phase 6 — find_open_community_invite_url_in_snapshot tests ──

    #[test]
    fn find_open_community_invite_url_returns_err_when_missing() {
        use crate::library_directory::find_open_community_invite_url_in_snapshot;

        let snapshot: Vec<AggregatedEntry> = Vec::new();
        let result =
            find_open_community_invite_url_in_snapshot(&snapshot, "00".repeat(16).as_str());
        let err = result.expect_err("empty snapshot should not match");
        assert!(
            err.contains("no longer listed"),
            "expected friendly missing-entry message, got: {err}"
        );
    }

    #[test]
    fn find_open_community_invite_url_returns_err_when_entry_is_invite_only() {
        use crate::community_invite::InviteToken;
        use crate::community_membership::{MembershipEventKind, SignedMembershipEvent};
        use crate::library_directory::find_open_community_invite_url_in_snapshot;
        use harmony_identity::PrivateIdentity;
        use std::collections::BTreeSet;

        let admin_identity = PrivateIdentity::from_seed(&[0xaa; 32]);
        let admin_addr = OwnerAddr(admin_identity.identity.address_hash);
        let community_id = SpaceId([0xf1; 16]);

        let admin_bootstrap = SignedMembershipEvent {
            signer_certs: Vec::new(),
            id: [0u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin_addr,
            at: Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "test-dev".into(),
            },
            sig: [0u8; 64],
            countersig: None,
            // ZEB-339: bootstrap-Join must embed the admin's EnrollmentCert.
            enrollment: Some(crate::community_membership::mint_test_owner(0xC4).cert),
        };

        let payload = CommunityInvitePayload {
            inviter_signer_certs: Vec::new(),
            community_id,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: vec![0u8; 92],
                sealed_epoch_keys: Vec::new(),
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr,
            community_name: "Inviteonly".into(),
            is_invite_only: true,
            expires_at: None,
            invite_token: Some(InviteToken {
                inviter: admin_addr,
                invitee_hint: None,
                minted_at: Hlc {
                    wall_ms: 1_000,
                    logical: 0,
                    device_id: "test-dev".into(),
                },
                expires_at: None,
                sig: [0u8; 64],
            }),
            admin_bootstrap: Some(admin_bootstrap),
            admin_identity_pub: Some(admin_identity.identity.to_public_bytes()),
            forked_from: None,
            pre_fork_snapshot: None,
            // ZEB-339: invite-only payloads must carry the inviter's
            // EnrollmentCert; rejected at receive regardless of cert content.
            inviter_enrollment: Some(crate::community_membership::mint_test_owner(0xA2).cert),
            // ZEB-367: untargeted invite-only payload requires the URL decrypt key
            // for encode_invite_url to accept it (content irrelevant for this test).
            untargeted_decrypt_key: Some([0u8; 32]),
        };
        let invite_url = encode_invite_url(&payload).expect("encode invite-only url");

        let entry = LibraryDirectoryEntry {
            community_id,
            community_admin_identity_pub: admin_identity.identity.to_public_bytes(),
            name: "Inviteonly".into(),
            description: String::new(),
            topics: Vec::new(),
            invite_url,
            listed_by: OwnerAddr([0xcc; 16]),
            listed_at: Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "test-dev".into(),
            },
            library_identity_pub: None,
            library_signature: None,
            community_signature: [0u8; 64],
        };
        let agg = AggregatedEntry {
            entry,
            attested_by: BTreeSet::new(),
            unattested_by: BTreeSet::new(),
        };
        let snapshot = vec![agg];

        let result =
            find_open_community_invite_url_in_snapshot(&snapshot, &hex::encode(community_id.0));
        let err = result.expect_err("invite-only entry must be rejected");
        assert!(
            err.to_lowercase().contains("invite-only"),
            "expected invite-only message, got: {err}"
        );
    }

    #[test]
    fn find_open_community_invite_url_returns_ok_for_open_entry() {
        use crate::library_directory::find_open_community_invite_url_in_snapshot;
        use harmony_identity::PrivateIdentity;
        use std::collections::BTreeSet;

        let admin_identity = PrivateIdentity::from_seed(&[0xaa; 32]);
        let admin_addr = OwnerAddr(admin_identity.identity.address_hash);
        let community_id = SpaceId([0xf2; 16]);

        let payload = CommunityInvitePayload {
            inviter_signer_certs: Vec::new(),
            community_id,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: vec![0x42u8; 32],
                sealed_epoch_keys: Vec::new(),
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr,
            community_name: "OpenCom".into(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
            admin_bootstrap: None,
            admin_identity_pub: None,
            forked_from: None,
            pre_fork_snapshot: None,
            inviter_enrollment: None,
            untargeted_decrypt_key: None,
        };
        let invite_url = encode_invite_url(&payload).expect("encode open url");

        let entry = LibraryDirectoryEntry {
            community_id,
            community_admin_identity_pub: admin_identity.identity.to_public_bytes(),
            name: "OpenCom".into(),
            description: String::new(),
            topics: Vec::new(),
            invite_url: invite_url.clone(),
            listed_by: OwnerAddr([0xcc; 16]),
            listed_at: Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "test-dev".into(),
            },
            library_identity_pub: None,
            library_signature: None,
            community_signature: [0u8; 64],
        };
        let agg = AggregatedEntry {
            entry,
            attested_by: BTreeSet::new(),
            unattested_by: BTreeSet::new(),
        };
        let snapshot = vec![agg];

        let returned =
            find_open_community_invite_url_in_snapshot(&snapshot, &hex::encode(community_id.0))
                .expect("open entry must return Ok");
        assert_eq!(
            returned, invite_url,
            "helper must return the entry's invite_url verbatim"
        );
    }

    /// Anti-phishing defense: an entry that advertises community A's id
    /// but whose `invite_url` decodes to community B's payload must be
    /// rejected. Phase 1's `verify_entry` (lines 376-381) already enforces
    /// this binding at receive time; the helper re-checks defensively so
    /// a future regression in `verify_entry` cannot silently redirect the
    /// join intent through this code path.
    #[test]
    fn find_open_community_invite_url_returns_err_when_community_id_mismatch() {
        use crate::library_directory::find_open_community_invite_url_in_snapshot;
        use harmony_identity::PrivateIdentity;
        use std::collections::BTreeSet;

        let admin_identity = PrivateIdentity::from_seed(&[0xaa; 32]);
        let admin_addr = OwnerAddr(admin_identity.identity.address_hash);
        let entry_community_id = SpaceId([0xf1; 16]);
        let payload_community_id = SpaceId([0xf2; 16]);

        // Build the invite URL pointing at community B (0xf2).
        let payload = CommunityInvitePayload {
            inviter_signer_certs: Vec::new(),
            community_id: payload_community_id,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: vec![0x42u8; 32],
                sealed_epoch_keys: Vec::new(),
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr,
            community_name: "MaliciousB".into(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
            admin_bootstrap: None,
            admin_identity_pub: None,
            forked_from: None,
            pre_fork_snapshot: None,
            inviter_enrollment: None,
            untargeted_decrypt_key: None,
        };
        let invite_url = encode_invite_url(&payload).expect("encode mismatched url");

        // But the entry claims community A (0xf1) — phishing-class entry.
        let entry = LibraryDirectoryEntry {
            community_id: entry_community_id,
            community_admin_identity_pub: admin_identity.identity.to_public_bytes(),
            name: "DisplayedAsA".into(),
            description: String::new(),
            topics: Vec::new(),
            invite_url,
            listed_by: OwnerAddr([0xcc; 16]),
            listed_at: Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "test-dev".into(),
            },
            library_identity_pub: None,
            library_signature: None,
            community_signature: [0u8; 64],
        };
        let agg = AggregatedEntry {
            entry,
            attested_by: BTreeSet::new(),
            unattested_by: BTreeSet::new(),
        };
        let snapshot = vec![agg];

        // Caller asks to join community A; helper must reject because the
        // entry's URL would actually redeem into community B.
        let result = find_open_community_invite_url_in_snapshot(
            &snapshot,
            &hex::encode(entry_community_id.0),
        );
        let err = result.expect_err("mismatched community_id must be rejected");
        assert!(
            err.to_lowercase().contains("community_id") || err.to_lowercase().contains("malformed"),
            "expected community_id mismatch / malformed message, got: {err}"
        );
    }
}

#[cfg(test)]
mod announce_verify_tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    /// A plausible present-day wall used as the receiver's `now` in the
    /// forward-skew tests (~2023-11-14).
    const NOW_MS: u64 = 1_700_000_000_000;
    const ONE_YEAR_MS: u64 = 365 * 24 * 60 * 60 * 1000;

    fn unsigned_announce_with_identity(identity_pub: [u8; 64]) -> LibraryAnnounce {
        LibraryAnnounce {
            library_identity_pub: identity_pub,
            name: "Test".to_string(),
            description: "Test desc".to_string(),
            listed_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".to_string(),
            },
            library_signature: [0u8; 64],
        }
    }

    /// Build a validly-signed `LibraryAnnounce` whose `listed_at.wall_ms`
    /// is `wall_ms`, so the forward-skew bound (which runs AFTER the sig
    /// check) is what these tests actually exercise.
    fn signed_announce_at(wall_ms: u64) -> LibraryAnnounce {
        let signing_key = SigningKey::from_bytes(&[9u8; 32]);
        let ed_verifying = signing_key.verifying_key().to_bytes();
        let mut identity_pub = [0u8; 64];
        identity_pub[..32].copy_from_slice(&[0x11; 32]);
        identity_pub[32..].copy_from_slice(&ed_verifying);

        let mut announce = LibraryAnnounce {
            library_identity_pub: identity_pub,
            name: "Test".to_string(),
            description: "Test desc".to_string(),
            listed_at: Hlc {
                wall_ms,
                logical: 0,
                device_id: "d".to_string(),
            },
            library_signature: [0u8; 64],
        };
        let signed_bytes = canonical_cbor_encode(&announce).expect("encode for sign");
        announce.library_signature = signing_key.sign(&signed_bytes).to_bytes();
        announce
    }

    #[test]
    fn rejects_invalid_identity_pub() {
        // Ed25519 half `[0x7F; 32]` doesn't decompress under ed25519-dalek
        // 2.x / curve25519-dalek 4.x — same fixture used by
        // `malformed_identity_pub_rejected` in the entry tests.
        let mut bad_identity_pub = [0u8; 64];
        bad_identity_pub[32..].copy_from_slice(&[0x7F; 32]);
        let announce = unsigned_announce_with_identity(bad_identity_pub);
        let err = verify_announce(&announce, None).unwrap_err();
        assert!(matches!(err, AnnounceVerifyError::InvalidIdentityPub(_)));
    }

    #[test]
    fn rejects_name_too_long() {
        // Bounds checks come BEFORE identity parse in `verify_announce`,
        // so we can use any identity_pub bytes here — name-too-long
        // fires before the (otherwise-invalid) identity is ever parsed.
        let mut announce = unsigned_announce_with_identity([0x7F; 64]);
        announce.name = "x".repeat(MAX_NAME_LEN + 1);
        let err = verify_announce(&announce, None).unwrap_err();
        assert!(matches!(err, AnnounceVerifyError::NameTooLong));
    }

    // ZEB-852 C7: forward-skew bound on the self-attested `listed_at`.

    #[test]
    fn verify_announce_rejects_future_listed_at() {
        // A stamp a full year past the receiver's `now` is well beyond the
        // 30-min DISPLAY tolerance → rejected.
        let announce = signed_announce_at(NOW_MS + ONE_YEAR_MS);
        let err = verify_announce(&announce, Some(NOW_MS)).unwrap_err();
        assert!(
            matches!(err, AnnounceVerifyError::ListedAtTooFarInFuture),
            "future listed_at must be rejected, got {err:?}"
        );
    }

    #[test]
    fn verify_announce_accepts_in_range() {
        // A present stamp verifies.
        let present = signed_announce_at(NOW_MS);
        verify_announce(&present, Some(NOW_MS)).expect("present listed_at verifies");

        // An OLDER in-range stamp verifies too — the forward bound never
        // over-rejects the past (staleness is a separate, opposite concern).
        let older = signed_announce_at(NOW_MS - ONE_YEAR_MS);
        verify_announce(&older, Some(NOW_MS)).expect("older in-range listed_at verifies");
    }

    #[test]
    fn verify_announce_none_now_is_apply_all() {
        // Fail-open: an unreadable local clock (None) must never reject an
        // honest announce — even a far-future one verifies (apply-all pin).
        let announce = signed_announce_at(NOW_MS + ONE_YEAR_MS);
        verify_announce(&announce, None).expect("None ⇒ apply-all: future announce still verifies");
    }
}

#[cfg(test)]
mod announce_tests {
    use super::*;

    fn test_announce(name: &str, wall_ms: u64) -> LibraryAnnounce {
        LibraryAnnounce {
            // Not verified at this layer — `on_announce` is purely the
            // map-mutation surface and assumes its caller has already run
            // `verify_announce`.
            library_identity_pub: [0u8; 64],
            name: name.to_string(),
            description: String::new(),
            listed_at: Hlc {
                wall_ms,
                logical: 0,
                device_id: "d".to_string(),
            },
            library_signature: [0u8; 64],
        }
    }

    fn addr(b: u8) -> OwnerAddr {
        OwnerAddr([b; 16])
    }

    #[test]
    fn on_announce_inserts_new_library() {
        let mut announces = Announces::new();
        let result = announces.on_announce(addr(1), test_announce("LibA", 100));
        assert_eq!(result.outcome, AnnounceOutcome::Inserted(addr(1)));
        assert_eq!(result.evicted, None);
        assert_eq!(announces.len(), 1);
    }

    #[test]
    fn on_announce_dedupes_latest_listed_at_wins() {
        let mut announces = Announces::new();
        announces.on_announce(addr(1), test_announce("Old", 100));
        let result = announces.on_announce(addr(1), test_announce("New", 200));
        assert_eq!(result.outcome, AnnounceOutcome::Updated(addr(1)));
        assert_eq!(result.evicted, None);
        let snap = announces.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].name, "New");
    }

    #[test]
    fn on_announce_older_is_idempotent() {
        let mut announces = Announces::new();
        announces.on_announce(addr(1), test_announce("New", 200));
        let result = announces.on_announce(addr(1), test_announce("Older", 100));
        assert_eq!(result.outcome, AnnounceOutcome::Idempotent);
        assert_eq!(result.evicted, None);
        let snap = announces.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].name, "New");
    }

    #[test]
    fn on_announce_cap_eviction_drops_oldest_listed_at() {
        let mut announces = Announces::new();
        // Fill to cap with distinct addrs (distinct first byte +
        // last-two bytes encoding the index to disambiguate >255 entries)
        // and ascending listed_at values.
        for i in 0..MAX_DISCOVERED_LIBRARIES {
            let mut a = [0u8; 16];
            a[0] = (i & 0xFF) as u8;
            a[1] = ((i >> 8) & 0xFF) as u8;
            announces.on_announce(
                OwnerAddr(a),
                test_announce(&format!("Lib{i}"), 100 + i as u64),
            );
        }
        assert_eq!(announces.len(), MAX_DISCOVERED_LIBRARIES);

        // Insert one more — should evict the oldest (i=0, listed_at=100,
        // addr = all-zero bytes).
        let new_addr = OwnerAddr([0xFF; 16]);
        let result = announces.on_announce(new_addr, test_announce("New", 9_999));
        assert_eq!(result.outcome, AnnounceOutcome::Inserted(new_addr));
        let evicted_addr = result.evicted.expect("must have evicted oldest");
        assert_eq!(evicted_addr, OwnerAddr([0u8; 16]));
        assert_eq!(announces.len(), MAX_DISCOVERED_LIBRARIES);
    }

    #[test]
    fn snapshot_sorted_newest_first() {
        let mut announces = Announces::new();
        announces.on_announce(addr(1), test_announce("Old", 100));
        announces.on_announce(addr(2), test_announce("Mid", 200));
        announces.on_announce(addr(3), test_announce("New", 300));
        let snap = announces.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].name, "New");
        assert_eq!(snap[1].name, "Mid");
        assert_eq!(snap[2].name, "Old");
    }
}
