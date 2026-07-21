//! ZEB-458 Phase A Task 1: community relay wire module.
//!
//! A "community sealed relay" for Harmony's butler store-and-forward: the
//! sender seals the SAME [`crate::butler_deposit::DepositPayload`] to the
//! recipient's butler-set DEVICE key (P1 crypto), a co-community-membership-
//! gated relay holds it OPAQUE (it cannot open it), and the recipient's device
//! pulls + opens + ingests.
//!
//! Wire conventions mirror [`crate::butler_deposit`]:
//! * Two-character serde renames so each struct's keys are the same encoded
//!   length at a single nesting level.
//! * Canonical CBOR via `canonical_cbor_encode` / `canonical_cbor_decode`
//!   (trailing bytes rejected on decode).
//! * The sealed envelope is `dm_signing`'s shipped sealed-ECDH construction
//!   byte-for-byte, domain-separated with [`COMMUNITY_RELAY_SEAL_INFO`].

use ed25519_dalek::Signer;
use serde::{Deserialize, Serialize};

use crate::butler_deposit::{encode_deposit_payload, DepositPayload, DepositWireError};
use crate::community_membership::{MaterializedMembership, MemberStatus};
use crate::owner_state_crypto::{
    canonical_cbor_decode, canonical_cbor_encode, sealed::CanonicalPayloadSealed, CanonicalPayload,
};
use crate::owner_state_types::{OwnerAddr, SpaceId};

// =====================================================================
// Constants
// =====================================================================

/// ALPN for the deposit direction: sender → relay deposits a sealed blob.
pub const COMMUNITY_RELAY_DEPOSIT_ALPN: &[u8] = b"harmony/community-relay-deposit/v1";

/// ALPN for the pull direction: recipient → relay pulls held blobs.
pub const COMMUNITY_RELAY_PULL_ALPN: &[u8] = b"harmony/community-relay-pull/v1";

/// Cap on a single pull-RESPONSE frame body ([`RelayPullResponse`], a
/// `Vec<RelayHeldBlob>`). This is DISTINCT from
/// [`crate::butler_deposit::DEPOSIT_MAX_FRAME_BYTES`] (256 KiB), which bounds
/// one *per-blob* deposit frame: a pull response batches many held blobs into a
/// single frame, so it needs a larger cap. 16 MiB is generous — it must be
/// ≥ one maximum sealed blob (≤ 256 KiB) plus its CBOR envelope so that at
/// least one held blob always fits in a response (the query handler size-batches
/// to this cap, but a single oversize entry must never wedge the drain). The
/// query / ack frames stay on the small 256 KiB cap; only the response frame
/// uses this.
pub const RELAY_PULL_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// HKDF info string for the relay sealed envelope. Distinct from both the
/// butler-deposit and epoch-key-seal info strings (key-derivation domain
/// separation ensures a ciphertext sealed for one context can never be
/// opened in another).
pub const COMMUNITY_RELAY_SEAL_INFO: &[u8] = b"harmony-zeb-458-community-relay-v1";

/// Domain-separation prefix for the deposit frame signature. The sender's
/// cert-bound device ed25519 signs
/// `domain ‖ recipient_owner ‖ community_id ‖ sealed_blob`
/// (see [`relay_deposit_sig_payload`]).
pub const COMMUNITY_RELAY_DEPOSIT_SIG_DOMAIN: &[u8] = b"harmony-zeb-458-community-relay-deposit-v1";

/// Domain-separation prefix for the pull query signature. The requester's
/// cert-bound device ed25519 signs
/// `domain ‖ recipient_owner ‖ community_id`
/// (see [`relay_pull_sig_payload`]).
pub const COMMUNITY_RELAY_PULL_SIG_DOMAIN: &[u8] = b"harmony-zeb-458-community-relay-pull-v1";

/// Domain-separation prefix for the pull ack signature. The requester's
/// cert-bound device ed25519 signs
/// `domain ‖ recipient_owner ‖ community_id ‖ (content_ids SORTED + concatenated)`
/// (see [`relay_pull_ack_sig_payload`]).
///
/// The distinct domain ensures an ack cannot be replayed as a query and vice versa.
pub const COMMUNITY_RELAY_PULL_ACK_SIG_DOMAIN: &[u8] =
    b"harmony-zeb-458-community-relay-pull-ack-v1";

/// Per-sender cap on relay-held blobs (mirrors [`crate::butler_deposit::INBOX_PER_SENDER_CAP`]).
pub const RELAY_HOLD_PER_SENDER_CAP: usize = 64;

/// Global cap on relay-held blobs (mirrors [`crate::butler_deposit::INBOX_GLOBAL_CAP`]).
pub const RELAY_HOLD_GLOBAL_CAP: usize = 1024;

/// Per-blob byte ceiling on an accepted `sealed_blob`, enforced at admission
/// (before any crypto) so the count caps ([`RELAY_HOLD_PER_SENDER_CAP`] /
/// [`RELAY_HOLD_GLOBAL_CAP`]) also bound the relay's byte footprint, not just
/// the entry count. Matches the documented deposit-frame ceiling
/// [`crate::butler_deposit::DEPOSIT_MAX_FRAME_BYTES`] (256 KiB): the sealed blob
/// is one deposit frame's payload, so the Phase B transport frame cap already
/// bounds it at or below this — the admission check makes the invariant
/// explicit and independent of the transport. With the per-sender cap this
/// bounds a single sender's hold footprint to
/// `RELAY_HOLD_PER_SENDER_CAP * RELAY_MAX_SEALED_BLOB_BYTES` (mirroring
/// [`crate::dm_outhold::DM_OUTHOLD_DATASET_MAX_BYTES`]).
pub const RELAY_MAX_SEALED_BLOB_BYTES: usize = crate::butler_deposit::DEPOSIT_MAX_FRAME_BYTES;

/// ZEB-458 P4 Phase B: generous per-entry CBOR-metadata overhead allowance,
/// added on top of [`RELAY_MAX_SEALED_BLOB_BYTES`] when sizing the whole-doc
/// dataset cap below. A replicated `RelayHoldDoc` is not just the blob bytes —
/// each held entry serializes as a CBOR map keyed by `"{recipient_hex}:
/// {content_id_hex}"` (≈ 81 bytes of key text) plus the entry's metadata:
/// `recipient_owner` + `sender_owner` (OwnerAddr, 16 bytes each), `community_id`
/// (SpaceId, 16 bytes), `held_at` (an HLC: wall_ms + logical + device_id
/// string), a 64-hex `held_by` relay-device string, the `content_id`, and a
/// `pulled_by` set — all wrapped in CBOR map/key/length headers. 512 bytes is
/// deliberately over-budget (the measured per-entry overhead is well under this
/// — see the unit test) so the dataset cap never under-counts the doc and the
/// zenoh dataset adapter cannot silently DROP a full-capacity sample.
pub const RELAY_HOLD_ENTRY_OVERHEAD_BYTES: usize = 512;

/// ZEB-458 P4 Phase B: inbound size cap for one `relay-hold-v1` fleet-sync
/// sample (a whole CBOR-encoded `RelayHoldDoc`), enforced in the zenoh bridge
/// before the owned copy. The doc holds at most [`RELAY_HOLD_GLOBAL_CAP`]
/// entries, each bounded by [`RELAY_MAX_SEALED_BLOB_BYTES`] for the blob plus
/// [`RELAY_HOLD_ENTRY_OVERHEAD_BYTES`] for the per-entry CBOR map key + metadata
/// (the replicated sample is the WHOLE doc, not just the raw blobs — a
/// blob-bytes-only product under-counts the doc and, at full capacity, the
/// zenoh dataset adapter would DROP the over-cap sample so sibling relays never
/// converge). A fixed 64 KiB envelope margin covers the doc-level CBOR framing
/// (outer map header, version tag, etc.) independent of entry count.
pub const RELAY_HOLD_DATASET_MAX_BYTES: usize = RELAY_HOLD_GLOBAL_CAP
    * (RELAY_MAX_SEALED_BLOB_BYTES + RELAY_HOLD_ENTRY_OVERHEAD_BYTES)
    + 64 * 1024;

/// ZEB-458 P4 Phase B: inbound size cap for one `relay-optin-v1` fleet-sync
/// sample (a whole CBOR-encoded `RelayOptInDoc`). The doc carries only a
/// boolean + HLC stamp per community, so a generous fixed ceiling (mirroring
/// [`crate::fleet_net::FLEET_NET_DATASET_MAX_BYTES`]) bounds peer-driven
/// allocation without constraining a realistic opt-in set.
pub const RELAY_OPTIN_DATASET_MAX_BYTES: usize = 256 * 1024;

/// TTL for un-pulled relay blobs in milliseconds (30 days, matching
/// [`crate::butler_deposit::INBOX_TTL_MS`]).
pub const RELAY_HOLD_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1_000;

/// Sweep interval (ms) for the relay-hold GC task — the ONLY storage-reclaim
/// path (TTL-expired + fully-covered entries). 10 minutes: frequent enough to
/// bound storage shortly after coverage propagates, infrequent enough to be
/// negligible overhead. Wired at start_node (T11b).
pub const RELAY_HOLD_GC_INTERVAL_MS: u64 = 10 * 60 * 1_000;

// =====================================================================
// Wire types — Deposit direction (sender → relay)
// =====================================================================

/// Sender → relay deposit request (canonical CBOR). The relay verifies the
/// enrollment cert chain, the `sig`, and co-membership BEFORE accepting the
/// blob; it holds the blob OPAQUE (cannot open it — the sealed_blob is sealed
/// to the recipient's device key, not the relay's).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayDepositFrame {
    /// Recipient OwnerAddr bytes. The relay holds for this owner; which of the
    /// recipient's devices the blob is sealed to is identified implicitly by the
    /// `ContentId(sealed_blob)` (each seal is unique), so no device label is
    /// carried on the wire — only the sealed-to device can open + ack a given
    /// content id.
    #[serde(rename = "ro")]
    pub recipient_owner: [u8; 16],
    /// Sender OwnerAddr bytes — co-membership-checked against the community.
    #[serde(rename = "so")]
    pub sender_owner: [u8; 16],
    /// Community SpaceId — gating predicate: both sender and recipient must be
    /// [`MemberStatus::Joined`] in this community.
    #[serde(rename = "ci")]
    pub community_id: SpaceId,
    /// Canonical CBOR of the sender device's `EnrollmentCert`, verified
    /// against the sender owner's master key.
    #[serde(rename = "ec", with = "serde_bytes")]
    pub sender_enrollment_cert: Vec<u8>,
    /// 64-byte device ed25519 signature over [`relay_deposit_sig_payload`].
    #[serde(rename = "sg", with = "serde_bytes")]
    pub sig: Vec<u8>,
    /// [`DepositPayload`] canonical CBOR, sealed to the recipient device's
    /// birational X25519 key with [`COMMUNITY_RELAY_SEAL_INFO`].
    #[serde(rename = "sb", with = "serde_bytes")]
    pub sealed_blob: Vec<u8>,
    /// ZEB-677: canonical CBOR of `Vec<EnrollmentCert>` — the Master-issued
    /// signer certs backing a Quorum-issued `sender_enrollment_cert`. Empty
    /// when the cert is Master-issued (key omitted on the wire).
    #[serde(
        rename = "sc",
        default,
        skip_serializing_if = "Vec::is_empty",
        with = "serde_bytes"
    )]
    pub signer_certs_cbor: Vec<u8>,
}

/// Relay → sender acknowledgement, returned after the blob is persisted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayDepositAck {
    /// 32-byte content id computed by the relay over the sealed_blob.
    #[serde(rename = "ci")]
    pub content_id: [u8; 32],
}

// =====================================================================
// Wire types — Pull direction (recipient → relay)
// =====================================================================

/// Recipient → relay pull query (canonical CBOR). The relay verifies
/// co-membership and the sig before yielding held blobs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayPullQuery {
    /// Recipient OwnerAddr bytes (who is pulling their held blobs).
    #[serde(rename = "ro")]
    pub recipient_owner: [u8; 16],
    /// Community SpaceId. This is the membership-LIVENESS gate: the relay
    /// requires the requester to be a `Joined` member of this community before
    /// serving anything. It is NOT a per-blob response filter — the response is
    /// recipient-scoped (all of R's held blobs, regardless of which community
    /// each was deposited under); see [`RelayPullResponse`] and the acceptor's
    /// `held_for`.
    #[serde(rename = "ci")]
    pub community_id: SpaceId,
    /// Canonical CBOR of the requester device's `EnrollmentCert`.
    #[serde(rename = "ec", with = "serde_bytes")]
    pub requester_enrollment_cert: Vec<u8>,
    /// 64-byte device ed25519 signature over [`relay_pull_sig_payload`].
    #[serde(rename = "sg", with = "serde_bytes")]
    pub sig: Vec<u8>,
    /// ZEB-677: canonical CBOR of `Vec<EnrollmentCert>` — the Master-issued
    /// signer certs backing a Quorum-issued `requester_enrollment_cert`.
    /// Empty when the cert is Master-issued (key omitted on the wire).
    #[serde(
        rename = "sc",
        default,
        skip_serializing_if = "Vec::is_empty",
        with = "serde_bytes"
    )]
    pub signer_certs_cbor: Vec<u8>,
}

/// One held blob entry returned in a [`RelayPullResponse`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayHeldBlob {
    /// Sender OwnerAddr bytes.
    #[serde(rename = "so")]
    pub sender_owner: [u8; 16],
    /// The sealed blob (opaque to the relay; recipient opens with their device
    /// key under [`COMMUNITY_RELAY_SEAL_INFO`]).
    #[serde(rename = "sb", with = "serde_bytes")]
    pub sealed_blob: Vec<u8>,
}

/// Relay → recipient pull response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayPullResponse {
    /// All held blobs for the authenticated recipient — recipient-scoped, NOT
    /// filtered by the query's `community_id` (that is only the membership-
    /// liveness gate). The recipient opens the copies sealed to one of its own
    /// device keys and ignores any it cannot open.
    #[serde(rename = "en")]
    pub entries: Vec<RelayHeldBlob>,
}

/// Recipient → relay acknowledgement after successfully ingesting pulled blobs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayPullAck {
    /// Content IDs of successfully ingested blobs (relay may GC these).
    #[serde(rename = "ci")]
    pub content_ids: Vec<[u8; 32]>,
}

/// Self-contained pull-ack wire frame (recipient → relay, second message of
/// the pull bi-stream). Unlike [`RelayPullAck`] — which carries only the bare
/// content-id list and relies on the surrounding pull session for the
/// requester's identity, cert, and signature — this frame bundles everything
/// the relay's [`crate::iroh_community_relay_acceptor::handle_relay_pull_ack`]
/// needs to authenticate the ack on its own: the recipient owner, community,
/// the requester's `EnrollmentCert`, the acked content ids, and the ack
/// signature over [`relay_pull_ack_sig_payload`]. The Phase B pull transport
/// writes this frame on the SAME stream after receiving the pull response, so
/// the relay can mark the listed content ids pulled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayPullAckFrame {
    /// Recipient OwnerAddr bytes (who is acking their pulled blobs).
    #[serde(rename = "ro")]
    pub recipient_owner: [u8; 16],
    /// Community SpaceId — the membership-liveness gate (same role as on the
    /// pull query).
    #[serde(rename = "ci")]
    pub community_id: SpaceId,
    /// Canonical CBOR of the requester device's `EnrollmentCert` (re-supplied
    /// so the ack is self-authenticating on a fresh stream read).
    #[serde(rename = "ec", with = "serde_bytes")]
    pub requester_enrollment_cert: Vec<u8>,
    /// Content IDs of successfully ingested blobs (relay may GC these).
    #[serde(rename = "cd")]
    pub content_ids: Vec<[u8; 32]>,
    /// 64-byte device ed25519 signature over [`relay_pull_ack_sig_payload`].
    #[serde(rename = "sg", with = "serde_bytes")]
    pub sig: Vec<u8>,
    /// ZEB-677: canonical CBOR of `Vec<EnrollmentCert>` — the Master-issued
    /// signer certs backing a Quorum-issued `requester_enrollment_cert`.
    /// Empty when the cert is Master-issued (key omitted on the wire).
    #[serde(
        rename = "sc",
        default,
        skip_serializing_if = "Vec::is_empty",
        with = "serde_bytes"
    )]
    pub signer_certs_cbor: Vec<u8>,
}

// Manual CanonicalPayload registrations (mirrors butler_deposit.rs).
impl CanonicalPayloadSealed for RelayDepositFrame {}
impl CanonicalPayload for RelayDepositFrame {}
impl CanonicalPayloadSealed for RelayDepositAck {}
impl CanonicalPayload for RelayDepositAck {}
impl CanonicalPayloadSealed for RelayPullQuery {}
impl CanonicalPayload for RelayPullQuery {}
impl CanonicalPayloadSealed for RelayHeldBlob {}
impl CanonicalPayload for RelayHeldBlob {}
impl CanonicalPayloadSealed for RelayPullResponse {}
impl CanonicalPayload for RelayPullResponse {}
impl CanonicalPayloadSealed for RelayPullAck {}
impl CanonicalPayload for RelayPullAck {}
impl CanonicalPayloadSealed for RelayPullAckFrame {}
impl CanonicalPayload for RelayPullAckFrame {}

// =====================================================================
// Encode / decode helpers (one pair per wire type, trailing bytes rejected)
// =====================================================================

/// Encode a [`RelayDepositFrame`] to canonical CBOR.
pub fn encode_relay_deposit_frame(frame: &RelayDepositFrame) -> Result<Vec<u8>, DepositWireError> {
    canonical_cbor_encode(frame).map_err(|e| DepositWireError::Encode(format!("{e}")))
}

/// Decode a [`RelayDepositFrame`] from canonical CBOR (trailing bytes rejected).
pub fn decode_relay_deposit_frame(bytes: &[u8]) -> Result<RelayDepositFrame, DepositWireError> {
    canonical_cbor_decode(bytes).map_err(|e| DepositWireError::Decode(format!("{e}")))
}

/// Encode a [`RelayDepositAck`] to canonical CBOR.
pub fn encode_relay_deposit_ack(ack: &RelayDepositAck) -> Result<Vec<u8>, DepositWireError> {
    canonical_cbor_encode(ack).map_err(|e| DepositWireError::Encode(format!("{e}")))
}

/// Decode a [`RelayDepositAck`] from canonical CBOR (trailing bytes rejected).
pub fn decode_relay_deposit_ack(bytes: &[u8]) -> Result<RelayDepositAck, DepositWireError> {
    canonical_cbor_decode(bytes).map_err(|e| DepositWireError::Decode(format!("{e}")))
}

/// Encode a [`RelayPullQuery`] to canonical CBOR.
pub fn encode_relay_pull_query(query: &RelayPullQuery) -> Result<Vec<u8>, DepositWireError> {
    canonical_cbor_encode(query).map_err(|e| DepositWireError::Encode(format!("{e}")))
}

/// Decode a [`RelayPullQuery`] from canonical CBOR (trailing bytes rejected).
pub fn decode_relay_pull_query(bytes: &[u8]) -> Result<RelayPullQuery, DepositWireError> {
    canonical_cbor_decode(bytes).map_err(|e| DepositWireError::Decode(format!("{e}")))
}

/// Encode a [`RelayPullResponse`] to canonical CBOR.
pub fn encode_relay_pull_response(resp: &RelayPullResponse) -> Result<Vec<u8>, DepositWireError> {
    canonical_cbor_encode(resp).map_err(|e| DepositWireError::Encode(format!("{e}")))
}

/// Decode a [`RelayPullResponse`] from canonical CBOR (trailing bytes rejected).
pub fn decode_relay_pull_response(bytes: &[u8]) -> Result<RelayPullResponse, DepositWireError> {
    canonical_cbor_decode(bytes).map_err(|e| DepositWireError::Decode(format!("{e}")))
}

/// Encode a [`RelayPullAck`] to canonical CBOR.
pub fn encode_relay_pull_ack(ack: &RelayPullAck) -> Result<Vec<u8>, DepositWireError> {
    canonical_cbor_encode(ack).map_err(|e| DepositWireError::Encode(format!("{e}")))
}

/// Decode a [`RelayPullAck`] from canonical CBOR (trailing bytes rejected).
pub fn decode_relay_pull_ack(bytes: &[u8]) -> Result<RelayPullAck, DepositWireError> {
    canonical_cbor_decode(bytes).map_err(|e| DepositWireError::Decode(format!("{e}")))
}

/// Encode a [`RelayPullAckFrame`] to canonical CBOR.
pub fn encode_relay_pull_ack_frame(frame: &RelayPullAckFrame) -> Result<Vec<u8>, DepositWireError> {
    canonical_cbor_encode(frame).map_err(|e| DepositWireError::Encode(format!("{e}")))
}

/// Decode a [`RelayPullAckFrame`] from canonical CBOR (trailing bytes rejected).
pub fn decode_relay_pull_ack_frame(bytes: &[u8]) -> Result<RelayPullAckFrame, DepositWireError> {
    canonical_cbor_decode(bytes).map_err(|e| DepositWireError::Decode(format!("{e}")))
}

// =====================================================================
// Signature payload constructors (single shared construction for signer + verifier)
// =====================================================================

/// Signed bytes for a deposit frame:
/// `COMMUNITY_RELAY_DEPOSIT_SIG_DOMAIN ‖ recipient_owner ‖ community_id.0 ‖ sealed_blob`.
///
/// Both the sender (signer) and the relay (verifier) MUST build the signed
/// bytes through this function — never inline the concatenation.
pub fn relay_deposit_sig_payload(
    recipient_owner: &[u8; 16],
    community_id: &SpaceId,
    sealed_blob: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        COMMUNITY_RELAY_DEPOSIT_SIG_DOMAIN.len()
            + recipient_owner.len()
            + community_id.0.len()
            + sealed_blob.len(),
    );
    out.extend_from_slice(COMMUNITY_RELAY_DEPOSIT_SIG_DOMAIN);
    out.extend_from_slice(recipient_owner);
    out.extend_from_slice(&community_id.0);
    out.extend_from_slice(sealed_blob);
    out
}

/// Signed bytes for a pull query:
/// `COMMUNITY_RELAY_PULL_SIG_DOMAIN ‖ recipient_owner ‖ community_id.0`.
///
/// Both the requester (signer) and the relay (verifier) MUST build the signed
/// bytes through this function.
pub fn relay_pull_sig_payload(recipient_owner: &[u8; 16], community_id: &SpaceId) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        COMMUNITY_RELAY_PULL_SIG_DOMAIN.len() + recipient_owner.len() + community_id.0.len(),
    );
    out.extend_from_slice(COMMUNITY_RELAY_PULL_SIG_DOMAIN);
    out.extend_from_slice(recipient_owner);
    out.extend_from_slice(&community_id.0);
    out
}

/// Signed bytes for a pull ack:
/// `COMMUNITY_RELAY_PULL_ACK_SIG_DOMAIN ‖ recipient_owner ‖ community_id.0 ‖ (content_ids SORTED + concatenated)`.
///
/// content_ids MUST be sorted ascending before concatenation so the binding is
/// order-independent: the relay and the recipient may enumerate content IDs in
/// different orders, and the canonical form must be identical for both sides.
///
/// Both the requester (signer) and the relay (verifier) MUST build the signed
/// bytes through this function — never inline the concatenation.
pub fn relay_pull_ack_sig_payload(
    recipient_owner: &[u8; 16],
    community_id: &SpaceId,
    content_ids: &[[u8; 32]],
) -> Vec<u8> {
    let mut sorted = content_ids.to_vec();
    sorted.sort_unstable();
    let mut out = Vec::with_capacity(
        COMMUNITY_RELAY_PULL_ACK_SIG_DOMAIN.len()
            + recipient_owner.len()
            + community_id.0.len()
            + sorted.len() * 32,
    );
    out.extend_from_slice(COMMUNITY_RELAY_PULL_ACK_SIG_DOMAIN);
    out.extend_from_slice(recipient_owner);
    out.extend_from_slice(&community_id.0);
    for cid in &sorted {
        out.extend_from_slice(cid);
    }
    out
}

// =====================================================================
// Build helpers
// =====================================================================

/// Build a [`RelayDepositFrame`] for one relay — the exact construction the
/// relay's acceptor verifies:
///
/// * `sealed_blob` = [`DepositPayload`] canonical CBOR sealed to
///   `birational(vk)` of the RECIPIENT device
///   (`dm_signing::ed25519_pub_to_x25519` of `recipient_device_ed25519_verify`)
///   under [`COMMUNITY_RELAY_SEAL_INFO`];
/// * `sig` = the sender's cert-bound enrolled device ed25519 over
///   [`relay_deposit_sig_payload`].
///
/// `recipient_device_ed25519_verify` is the SEAL TARGET only (one of R's
/// advertised butler-set device keys); it is not echoed on the wire.
///
/// Reuses [`encode_deposit_payload`] from `butler_deposit` — the sealed
/// plaintext is the same `DepositPayload` shape the butler P1 path uses,
/// so recipient ingestion is wire-identical.
pub fn build_relay_deposit_frame(
    recipient_owner: [u8; 16],
    recipient_device_ed25519_verify: &[u8; 32],
    sender_owner: [u8; 16],
    community_id: SpaceId,
    sender_cert_bytes: Vec<u8>,
    sender_device_key: &ed25519_dalek::SigningKey,
    payload: &DepositPayload,
) -> Result<RelayDepositFrame, String> {
    let payload_bytes =
        encode_deposit_payload(payload).map_err(|e| format!("encode deposit payload: {e}"))?;
    let seal_pub = crate::dm_signing::ed25519_pub_to_x25519(recipient_device_ed25519_verify)
        .map_err(|e| format!("recipient vk -> x25519: {e:?}"))?;
    let sealed_blob = crate::dm_signing::seal_to_owner_with_info(
        &seal_pub,
        &payload_bytes,
        COMMUNITY_RELAY_SEAL_INFO,
    )
    .map_err(|e| format!("seal relay deposit payload: {e:?}"))?;
    let sig = sender_device_key
        .sign(&relay_deposit_sig_payload(
            &recipient_owner,
            &community_id,
            &sealed_blob,
        ))
        .to_bytes()
        .to_vec();
    Ok(RelayDepositFrame {
        signer_certs_cbor: Vec::new(),
        recipient_owner,
        sender_owner,
        community_id,
        sender_enrollment_cert: sender_cert_bytes,
        sig,
        sealed_blob,
    })
}

// =====================================================================
// Co-membership predicate
// =====================================================================

/// Returns `true` iff both `a` and `b` are present in `membership.members`
/// with [`MemberStatus::Joined`].
///
/// Used by the relay acceptor as the admission gate: a deposit or pull is
/// allowed only when both the sender/requester and the recipient share a
/// `Joined` membership in the gating community.
pub fn both_joined_members(
    membership: &MaterializedMembership,
    a: &OwnerAddr,
    b: &OwnerAddr,
) -> bool {
    let joined = |addr: &OwnerAddr| {
        membership
            .members
            .get(addr)
            .map(|m| m.status == MemberStatus::Joined)
            .unwrap_or(false)
    };
    joined(a) && joined(b)
}

// =====================================================================
// D40 sender-side helpers
// =====================================================================

use crate::owner_state_types::SpaceKind;

/// Communities where BOTH `self_owner` and `recipient` are Joined members
/// (D40 gate). Pure: `kind_of` and `is_joined` abstract the community-state
/// registry so this is unit-testable. The caller passes the sender's own
/// space-id list (`OwnerState.spaces` keys).
pub fn find_shared_communities(
    space_ids: &[SpaceId],
    self_owner: &OwnerAddr,
    recipient: &OwnerAddr,
    kind_of: impl Fn(&SpaceId) -> SpaceKind,
    is_joined: impl Fn(&SpaceId, &OwnerAddr) -> bool,
) -> Vec<SpaceId> {
    space_ids
        .iter()
        .filter(|s| kind_of(s) == SpaceKind::Community)
        .filter(|s| is_joined(s, self_owner) && is_joined(s, recipient))
        .copied()
        .collect()
}

/// Sender-side last-resort rung (D40). Mirrors the butler deposit client: given
/// the same outbox candidate the butler rung uses, seal the DepositPayload to
/// R's advertised butler-set device(s) and deposit to a relay in a shared
/// community. Returns true iff at least one relay acked. Never touches
/// AttemptState (the caller treats an acked candidate as delivered-pending-pull).
#[async_trait::async_trait]
pub trait CommunityRelayDepositClient: Send + Sync {
    async fn deposit(&self, req: &crate::butler_deposit::ButlerDepositRequest) -> bool;
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::butler_deposit::decode_deposit_payload;
    use crate::community_membership::{mint_test_owner, MemberState, MemberStatus, TestOwner};
    use crate::dm_signing::{ed25519_priv_to_x25519, open_from_owner_with_info};
    use crate::owner_state_types::SpaceId;
    use ed25519_dalek::Verifier;

    // ----------------------------------------------------------------
    // Fixtures
    // ----------------------------------------------------------------

    fn fixture_space_id() -> SpaceId {
        SpaceId([0xCC; 16])
    }

    fn fixture_deposit_frame() -> RelayDepositFrame {
        RelayDepositFrame {
            signer_certs_cbor: Vec::new(),
            recipient_owner: [0x11; 16],
            sender_owner: [0x22; 16],
            community_id: fixture_space_id(),
            sender_enrollment_cert: vec![0xE1, 0xE2, 0xE3, 0xE4],
            sig: vec![0x09; 64],
            sealed_blob: vec![0x5B, 0x5C, 0x5D],
        }
    }

    fn fixture_pull_query() -> RelayPullQuery {
        RelayPullQuery {
            signer_certs_cbor: Vec::new(),
            recipient_owner: [0x11; 16],
            community_id: fixture_space_id(),
            requester_enrollment_cert: vec![0xA1, 0xA2],
            sig: vec![0x07; 64],
        }
    }

    fn fixture_pull_response() -> RelayPullResponse {
        RelayPullResponse {
            entries: vec![
                RelayHeldBlob {
                    sender_owner: [0x22; 16],
                    sealed_blob: vec![0x01, 0x02, 0x03],
                },
                RelayHeldBlob {
                    sender_owner: [0x33; 16],
                    sealed_blob: vec![0x04, 0x05],
                },
            ],
        }
    }

    fn fixture_pull_ack() -> RelayPullAck {
        RelayPullAck {
            content_ids: vec![[0xAA; 32], [0xBB; 32]],
        }
    }

    fn fixture_pull_ack_frame() -> RelayPullAckFrame {
        RelayPullAckFrame {
            signer_certs_cbor: Vec::new(),
            recipient_owner: [0x11; 16],
            community_id: fixture_space_id(),
            requester_enrollment_cert: vec![0xA1, 0xA2, 0xA3],
            content_ids: vec![[0xAA; 32], [0xBB; 32]],
            sig: vec![0x07; 64],
        }
    }

    fn fixture_payload() -> DepositPayload {
        DepositPayload {
            cidnotify_packet: Some(vec![0x01, 0x02, 0x03, 0x04]),
            storage_blob: vec![0x05, 0x06, 0x07, 0x08, 0x09],
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
        }
    }

    // Build a Hlc for MemberState — minimal valid value.
    fn dummy_hlc() -> crate::owner_state_types::Hlc {
        crate::owner_state_types::Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "test".to_string(),
        }
    }

    fn joined_member_state(owner: &TestOwner) -> MemberState {
        let mut keys = std::collections::BTreeSet::new();
        keys.insert(owner.cert.device_pubkeys.classical.ed25519_verify);
        MemberState {
            status: MemberStatus::Joined,
            joined_at: dummy_hlc(),
            left_at: None,
            enrolled_device_keys: keys,
            revoked_device_keys: std::collections::BTreeSet::new(),
        }
    }

    fn left_member_state(owner: &TestOwner) -> MemberState {
        let mut ms = joined_member_state(owner);
        ms.status = MemberStatus::Left;
        ms.left_at = Some(dummy_hlc());
        ms
    }

    // ----------------------------------------------------------------
    // Round-trip encode/decode
    // ----------------------------------------------------------------

    #[test]
    fn relay_deposit_frame_round_trips() {
        let frame = fixture_deposit_frame();
        let bytes = encode_relay_deposit_frame(&frame).expect("encode");
        let back = decode_relay_deposit_frame(&bytes).expect("decode");
        assert_eq!(back, frame);
    }

    #[test]
    fn relay_deposit_frame_rejects_trailing_bytes() {
        let frame = fixture_deposit_frame();
        let mut bytes = encode_relay_deposit_frame(&frame).expect("encode");
        bytes.push(0x00);
        assert!(
            matches!(
                decode_relay_deposit_frame(&bytes),
                Err(DepositWireError::Decode(_))
            ),
            "trailing byte must cause Decode error"
        );
    }

    #[test]
    fn relay_pull_query_round_trips() {
        let q = fixture_pull_query();
        let bytes = encode_relay_pull_query(&q).expect("encode");
        let back = decode_relay_pull_query(&bytes).expect("decode");
        assert_eq!(back, q);
    }

    #[test]
    fn relay_pull_query_rejects_trailing_bytes() {
        let q = fixture_pull_query();
        let mut bytes = encode_relay_pull_query(&q).expect("encode");
        bytes.push(0xFF);
        assert!(matches!(
            decode_relay_pull_query(&bytes),
            Err(DepositWireError::Decode(_))
        ));
    }

    #[test]
    fn relay_pull_response_round_trips() {
        let resp = fixture_pull_response();
        let bytes = encode_relay_pull_response(&resp).expect("encode");
        let back = decode_relay_pull_response(&bytes).expect("decode");
        assert_eq!(back, resp);
    }

    #[test]
    fn relay_pull_response_rejects_trailing_bytes() {
        let resp = fixture_pull_response();
        let mut bytes = encode_relay_pull_response(&resp).expect("encode");
        bytes.push(0x01);
        assert!(matches!(
            decode_relay_pull_response(&bytes),
            Err(DepositWireError::Decode(_))
        ));
    }

    #[test]
    fn relay_pull_ack_round_trips() {
        let ack = fixture_pull_ack();
        let bytes = encode_relay_pull_ack(&ack).expect("encode");
        let back = decode_relay_pull_ack(&bytes).expect("decode");
        assert_eq!(back, ack);
    }

    #[test]
    fn relay_pull_ack_rejects_trailing_bytes() {
        let ack = fixture_pull_ack();
        let mut bytes = encode_relay_pull_ack(&ack).expect("encode");
        bytes.push(0x42);
        assert!(matches!(
            decode_relay_pull_ack(&bytes),
            Err(DepositWireError::Decode(_))
        ));
    }

    #[test]
    fn relay_pull_ack_frame_round_trips() {
        let frame = fixture_pull_ack_frame();
        let bytes = encode_relay_pull_ack_frame(&frame).expect("encode");
        let back = decode_relay_pull_ack_frame(&bytes).expect("decode");
        assert_eq!(back, frame);
    }

    #[test]
    fn relay_pull_ack_frame_rejects_trailing_bytes() {
        let frame = fixture_pull_ack_frame();
        let mut bytes = encode_relay_pull_ack_frame(&frame).expect("encode");
        bytes.push(0x13);
        assert!(matches!(
            decode_relay_pull_ack_frame(&bytes),
            Err(DepositWireError::Decode(_))
        ));
    }

    // ----------------------------------------------------------------
    // build_relay_deposit_frame: sealed_blob opens with the right device key
    // ----------------------------------------------------------------

    #[test]
    fn build_relay_deposit_frame_sealed_blob_opens_with_recipient_device_key() {
        let recipient = mint_test_owner(0x01);
        let sender = mint_test_owner(0x02);
        let community_id = fixture_space_id();
        let payload = fixture_payload();

        let frame = build_relay_deposit_frame(
            recipient.owner.0,
            &recipient.cert.device_pubkeys.classical.ed25519_verify,
            sender.owner.0,
            community_id,
            vec![0xDE, 0xAD],
            &sender.device_key,
            &payload,
        )
        .expect("build frame");

        // recipient's device x25519 priv opens the sealed blob.
        let x_priv = ed25519_priv_to_x25519(&recipient.device_key);
        let plaintext =
            open_from_owner_with_info(&x_priv, &frame.sealed_blob, COMMUNITY_RELAY_SEAL_INFO)
                .expect("recipient device key must open sealed blob");

        let recovered = decode_deposit_payload(&plaintext).expect("decode payload");
        assert_eq!(recovered, payload, "round-tripped payload must match");
    }

    #[test]
    fn build_relay_deposit_frame_different_device_key_cannot_open() {
        let recipient = mint_test_owner(0x01);
        let other = mint_test_owner(0x03); // different device key
        let sender = mint_test_owner(0x02);
        let community_id = fixture_space_id();
        let payload = fixture_payload();

        let frame = build_relay_deposit_frame(
            recipient.owner.0,
            &recipient.cert.device_pubkeys.classical.ed25519_verify,
            sender.owner.0,
            community_id,
            vec![0xDE, 0xAD],
            &sender.device_key,
            &payload,
        )
        .expect("build frame");

        let other_x_priv = ed25519_priv_to_x25519(&other.device_key);
        let result =
            open_from_owner_with_info(&other_x_priv, &frame.sealed_blob, COMMUNITY_RELAY_SEAL_INFO);
        assert!(
            result.is_err(),
            "a different device's x25519 priv must NOT open the sealed blob"
        );
    }

    // ----------------------------------------------------------------
    // build_relay_deposit_frame: sig verifies under sender device vk
    // ----------------------------------------------------------------

    #[test]
    fn build_relay_deposit_frame_sig_verifies_under_sender_device_vk() {
        let recipient = mint_test_owner(0x01);
        let sender = mint_test_owner(0x02);
        let community_id = fixture_space_id();
        let payload = fixture_payload();

        let frame = build_relay_deposit_frame(
            recipient.owner.0,
            &recipient.cert.device_pubkeys.classical.ed25519_verify,
            sender.owner.0,
            community_id,
            vec![0xCE, 0xAB],
            &sender.device_key,
            &payload,
        )
        .expect("build frame");

        let sig_bytes: [u8; 64] = frame.sig.clone().try_into().expect("64-byte sig");
        let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
        let sender_vk = sender.device_key.verifying_key();
        let signed_bytes = relay_deposit_sig_payload(
            &frame.recipient_owner,
            &frame.community_id,
            &frame.sealed_blob,
        );
        assert!(
            sender_vk.verify(&signed_bytes, &sig).is_ok(),
            "sender device verifying key must verify the deposit frame sig"
        );
    }

    // ----------------------------------------------------------------
    // Sig payload domain separation
    // ----------------------------------------------------------------

    #[test]
    fn relay_deposit_sig_payload_domain_separated() {
        let ro = [0x11u8; 16];
        let cid = SpaceId([0xCC; 16]);
        let sb = vec![0xAB; 24];
        let payload = relay_deposit_sig_payload(&ro, &cid, &sb);

        assert!(payload.starts_with(COMMUNITY_RELAY_DEPOSIT_SIG_DOMAIN));
        let off = COMMUNITY_RELAY_DEPOSIT_SIG_DOMAIN.len();
        assert_eq!(&payload[off..off + 16], &ro);
        assert_eq!(&payload[off + 16..off + 32], &cid.0);
        assert_eq!(&payload[off + 32..], sb.as_slice());
    }

    #[test]
    fn relay_pull_sig_payload_domain_separated() {
        let ro = [0x11u8; 16];
        let cid = SpaceId([0xCC; 16]);
        let payload = relay_pull_sig_payload(&ro, &cid);

        assert!(payload.starts_with(COMMUNITY_RELAY_PULL_SIG_DOMAIN));
        let off = COMMUNITY_RELAY_PULL_SIG_DOMAIN.len();
        assert_eq!(&payload[off..off + 16], &ro);
        assert_eq!(&payload[off + 16..], &cid.0);
    }

    // ----------------------------------------------------------------
    // both_joined_members predicate
    // ----------------------------------------------------------------

    fn make_membership(
        entries: impl IntoIterator<Item = (OwnerAddr, MemberState)>,
    ) -> MaterializedMembership {
        let mut m = MaterializedMembership::default();
        for (addr, state) in entries {
            m.members.insert(addr, state);
        }
        m
    }

    #[test]
    fn both_joined_members_true_when_both_joined() {
        let a = mint_test_owner(0x10);
        let b = mint_test_owner(0x20);
        let membership = make_membership([
            (a.owner, joined_member_state(&a)),
            (b.owner, joined_member_state(&b)),
        ]);
        assert!(both_joined_members(&membership, &a.owner, &b.owner));
    }

    #[test]
    fn both_joined_members_false_when_one_left() {
        let a = mint_test_owner(0x10);
        let b = mint_test_owner(0x20);
        let membership = make_membership([
            (a.owner, joined_member_state(&a)),
            (b.owner, left_member_state(&b)),
        ]);
        assert!(!both_joined_members(&membership, &a.owner, &b.owner));
    }

    #[test]
    fn both_joined_members_false_when_one_absent() {
        let a = mint_test_owner(0x10);
        let b = mint_test_owner(0x20);
        let membership = make_membership([(a.owner, joined_member_state(&a))]);
        assert!(!both_joined_members(&membership, &a.owner, &b.owner));
    }

    #[test]
    fn both_joined_members_false_when_one_banned() {
        let a = mint_test_owner(0x10);
        let b = mint_test_owner(0x20);
        let mut banned_state = joined_member_state(&b);
        banned_state.status = MemberStatus::Banned;
        let membership =
            make_membership([(a.owner, joined_member_state(&a)), (b.owner, banned_state)]);
        assert!(!both_joined_members(&membership, &a.owner, &b.owner));
    }

    #[test]
    fn both_joined_members_false_when_one_invited() {
        let a = mint_test_owner(0x10);
        let b = mint_test_owner(0x20);
        let mut invited_state = joined_member_state(&b);
        invited_state.status = MemberStatus::Invited;
        let membership =
            make_membership([(a.owner, joined_member_state(&a)), (b.owner, invited_state)]);
        assert!(!both_joined_members(&membership, &a.owner, &b.owner));
    }

    #[test]
    fn both_joined_members_false_when_one_pending_join() {
        // A member whose join was minted but not yet countersigned
        // (`JoinCountersign` pending, per ZEB-254) is `PendingJoin`, NOT
        // `Joined` — they must not pass the relay co-membership gate.
        let a = mint_test_owner(0x10);
        let b = mint_test_owner(0x20);
        let mut pending_state = joined_member_state(&b);
        pending_state.status = MemberStatus::PendingJoin;
        let membership =
            make_membership([(a.owner, joined_member_state(&a)), (b.owner, pending_state)]);
        assert!(!both_joined_members(&membership, &a.owner, &b.owner));
    }

    #[test]
    fn both_joined_members_false_when_both_absent() {
        let a = mint_test_owner(0x10);
        let b = mint_test_owner(0x20);
        let membership = make_membership([]);
        assert!(!both_joined_members(&membership, &a.owner, &b.owner));
    }

    #[test]
    fn find_shared_communities_includes_only_mutually_joined_community_spaces() {
        use crate::owner_state_types::{OwnerAddr, SpaceId, SpaceKind};
        let self_o = OwnerAddr([0x01; 16]);
        let r = OwnerAddr([0x02; 16]);
        let c_both = SpaceId([0xC1; 16]); // both joined -> included
        let c_self_only = SpaceId([0xC2; 16]); // only self joined -> excluded
        let dm = SpaceId([0xD0; 16]); // not a community -> excluded

        let kinds = |s: &SpaceId| -> SpaceKind {
            if *s == dm {
                SpaceKind::Dm
            } else {
                SpaceKind::Community
            }
        };
        let joined = |s: &SpaceId, who: &OwnerAddr| -> bool {
            if *s == c_both {
                true
            } else if *s == c_self_only {
                *who == self_o
            } else {
                false
            }
        };
        let space_ids = vec![c_both, c_self_only, dm];
        let got = find_shared_communities(&space_ids, &self_o, &r, kinds, joined);
        assert_eq!(got, vec![c_both]);
    }
}
