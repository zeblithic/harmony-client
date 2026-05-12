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
