//! ZEB-418 SP2 Phase 1: butler deposit wire module — frame + ack + sealed
//! payload types for the `harmony/butler-deposit/v1` ALPN, plus u32-LE
//! length-prefixed framing helpers.
//!
//! A sender that cannot reach a recipient's device directly deposits the
//! sealed DM with an online sibling device of the recipient (the "butler"),
//! which persists it into the `dm-inbox-v1` fleet dataset and acks. See
//! `docs/specs/2026-06-09-zeb-418-sp2-butler-design.md` §4.
//!
//! Wire conventions (mirrors `dm_envelope`):
//! * Two-character serde renames so each struct's keys are the same encoded
//!   length at a single nesting level — the same-length-keys precondition
//!   documented on `crate::owner_state_crypto::canonical_cbor_encode`.
//! * Canonical CBOR via `canonical_cbor_encode` / `canonical_cbor_decode`
//!   (trailing bytes rejected on decode).
//! * The sealed envelope is `dm_signing`'s shipped sealed-ECDH construction
//!   byte-for-byte (`32-byte ephemeral X25519 ‖ 12-byte nonce ‖ ct+tag`),
//!   domain-separated with the new HKDF info string
//!   [`BUTLER_DEPOSIT_SEAL_INFO`] via `seal_to_owner_with_info` /
//!   `open_from_owner_with_info`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ed25519_dalek::Signer;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::owner_state_crypto::{
    canonical_cbor_decode, canonical_cbor_encode, sealed::CanonicalPayloadSealed, CanonicalPayload,
};
use crate::owner_state_types::{ContentId, OutboxEntryId, OwnerAddr, SpaceId};
use crate::reachability_record::{ButlerSetEntry, ReachabilityAnnouncePayload};

/// Domain-separation prefix for the deposit frame signature. The sender's
/// cert-bound device ed25519 signs `domain ‖ recipient_owner ‖ sealed_blob`
/// (see [`deposit_sig_payload`] — the single shared construction for signer
/// and verifier).
pub const BUTLER_DEPOSIT_SIG_DOMAIN: &[u8] = b"harmony-zeb-418-deposit-sig-v1";

/// HKDF info string for the deposit sealed envelope. Distinct from the
/// ZEB-249 epoch-key-seal info so a ciphertext sealed for one context can
/// never be opened in the other (key-derivation domain separation).
pub const BUTLER_DEPOSIT_SEAL_INFO: &[u8] = b"harmony-zeb-418-butler-deposit-v1";

/// Cap on a single length-prefixed frame body (either direction). Any u32
/// length prefix exceeding this is rejected BEFORE the body is read or
/// allocated.
pub const DEPOSIT_MAX_FRAME_BYTES: usize = 256 * 1024;

/// ZEB-483: per-deposit cap on the piggybacked DmInvite packet bytes. A DM
/// invite is a few hundred bytes (member set + 32-byte content key + 64-byte
/// identity pub + 64-byte sig); 4 KiB is a generous ceiling that still bars a
/// malicious sender from inflating butler/relay storage via the invite field.
pub const MAX_DEPOSIT_INVITE_BYTES: usize = 4096;

/// ZEB-505: the `DepositAck.message_cid` echoed for a standalone invite-only
/// deposit (no message). A fixed non-cid marker the client and acceptor agree
/// on — it can't collide with a real `message_cid` (always 32 bytes), so it
/// binds the ack without inventing a fake content id.
pub const INVITE_ONLY_DEPOSIT_MARKER: &[u8] = b"zeb505-invite-only";

/// ZEB-691: ack marker returned for a device-revocation deposit (no message
/// CID). Mirrors `INVITE_ONLY_DEPOSIT_MARKER`; the sender's `IrohButlerDepositClient`
/// binds the ack to this value for a `revocation_push` request.
pub const REVOCATION_DEPOSIT_MARKER: &[u8] = b"zeb691-revocation";

/// ZEB-674 (C4): ack marker returned for a standalone file-share grant deposit
/// (no message CID). Mirrors `INVITE_ONLY_DEPOSIT_MARKER` / `REVOCATION_DEPOSIT_MARKER`;
/// the sender binds the ack to this value for a `grant_push` request.
pub const GRANT_DEPOSIT_MARKER: &[u8] = b"zeb674-file-grant";

/// ZEB-674 (C4): per-deposit cap on the piggybacked `grant_push` bytes. A grant
/// carries one sealed `FileGrantInner` (~200 B) per grantee device; 64 KiB is a
/// generous ceiling (hundreds of devices) that still bars a malicious sender
/// from inflating butler/relay storage via the grant field, well under the
/// [`DEPOSIT_MAX_FRAME_BYTES`] whole-frame cap.
pub const MAX_DEPOSIT_GRANT_BYTES: usize = 64 * 1024;

/// ZEB-730: ack marker returned for a standalone file-grant *revoke* deposit
/// (no message CID). Mirrors `GRANT_DEPOSIT_MARKER`; the sender binds the ack to
/// this value for a `grant_revoke` request.
pub const GRANT_REVOKE_DEPOSIT_MARKER: &[u8] = b"zeb730-grant-revoke";

/// ZEB-730: per-deposit cap on the piggybacked `grant_revoke` bytes. A revoke
/// carries a single 32-byte root ContentId as canonical CBOR (~35 B); 256 is
/// generous headroom that still bars a malicious sender from inflating
/// butler/relay storage via the field. Fail-closed on overflow.
pub const MAX_DEPOSIT_GRANT_REVOKE_BYTES: usize = 256;

/// Maximum number of [`crate::reachability_record::ButlerSetEntry`]s carried
/// in the pkarr routing blob's butler set (spec §3: ordered priority list,
/// max 2 in v1). Readers truncate anything longer (defence against oversized
/// adversarial ads); the builder never produces more.
pub const BUTLER_SET_MAX_ENTRIES: usize = 2;

/// Freshness window for the routing blob's `bs_at` stamp (spec §3, ~15 min).
/// Senders treat a butler set whose `bs_at` is older than this as *no
/// butler set* and fall through to the existing retry chain — a stale ad
/// can never make delivery worse than today.
pub const BUTLER_SET_FRESHNESS_MS: u64 = 15 * 60 * 1_000;

/// ZEB-418 P2 (D16): periodic routing-record re-publish cadence — half the
/// freshness window so `bs_at` never lapses while the device is up.
pub const BUTLER_SET_REFRESH_MS: u64 = BUTLER_SET_FRESHNESS_MS / 2;

/// ZEB-418 P2 (D16): debounce for fleet-change-triggered re-publishes.
pub const FLEET_CHANGE_REPUBLISH_DEBOUNCE_MS: u64 = 60_000;

/// Per-sender inbox quota (spec §4 "per-contact quota"): bounds the LIVE
/// entries a single sender may hold in the `dm-inbox-v1` doc — deposited
/// and not yet GC'd, including entries partially ingested or awaiting the
/// one-sweep coverage-GC deferral. This is a butler STORAGE quota, not a
/// workflow-state count. Enforced atomically INSIDE the deposit ctx's
/// `persist_entry` critical section (PR #221 round 1); a deposit whose
/// inbox key is already stored is exempt, so a redelivery after a lost ack
/// still re-acks at a full inbox.
pub const INBOX_PER_SENDER_CAP: usize = 64;

/// Global inbox quota (spec §4 "global inbox cap"): bounds the total LIVE
/// entries in the `dm-inbox-v1` doc regardless of sender — the same
/// storage-quota semantics, enforcement point, and duplicate-key exemption
/// as [`INBOX_PER_SENDER_CAP`].
pub const INBOX_GLOBAL_CAP: usize = 1024;

/// ZEB-423: inbound Zenoh size cap for `dm-inbox-v1` full-doc fleet-sync frames,
/// the missing parity to the gate PR #222 added to the sibling P2 datasets
/// (`dm-outhold-v1` / `fleet-net-v1`) on the shared
/// `spawn_dataset_sync_zenoh_adapter` helper. Every inbound sample on this
/// peer-fed topic was previously copied into an owned `Vec<u8>` with no size
/// gate — attacker-driven allocation before the FleetSync engine could reject
/// the frame.
///
/// Sized to MATCH the sibling [`crate::dm_outhold::DM_OUTHOLD_DATASET_MAX_BYTES`]
/// (16 MiB = 64 deposit frames), deliberately NOT the dm-inbox quota's
/// theoretical max. The dataset replicates the WHOLE
/// [`crate::dm_inbox_crdt::DmInboxDoc`] per frame, and [`INBOX_GLOBAL_CAP`]
/// (1024) would put that max near 256 MiB — but the whole *point* of this gate
/// is to bound attacker-driven allocation, and a 256 MiB cap over the 64-deep
/// inbound channel is itself a multi-GB OOM vector (Qodo, PR #370). A cap that
/// large would not bound the DoS it exists to prevent. Whole-doc fleet-sync also
/// can't practically carry a frame that big, so an inbox exceeding 16 MiB is the
/// pre-existing whole-doc-replication scaling limit (fail-safe: the oversize
/// frame is dropped before allocation and the fleet re-publishes) — shared with
/// dm-outhold and best addressed by delta/chunked replication, not by raising
/// this gate into OOM range.
pub const DM_INBOX_DATASET_MAX_BYTES: usize = 64 * DEPOSIT_MAX_FRAME_BYTES;

// Deliberate parity with the sibling dm-outhold dataset (same DM-blob payloads,
// same whole-doc-replication memory profile). Pin the equality so any future
// divergence is a conscious decision rather than silent drift.
const _: () =
    assert!(DM_INBOX_DATASET_MAX_BYTES == crate::dm_outhold::DM_OUTHOLD_DATASET_MAX_BYTES);

/// GC TTL for un-ingested dm-inbox deposits (spec §5): the retention window
/// for a deposit absent full-fleet coverage. Since ZEB-851, enforcement is
/// on the RECIPIENT's own local first-observation clock, not
/// `deposited_at.wall_ms` (a backdated or peer-controlled stamp can no
/// longer pre-expire a live deposit) — see the `ingest_pending` GC pass in
/// `dm_inbox_ingest.rs`. 30 days, deliberately equal to
/// `dm_outbox::EXPIRATION_MS` (ZEB-227 alignment): once the sender's own
/// outbox has given up retrying, a parked deposit no longer has a delivery
/// path worth pinning fleet storage for.
pub const INBOX_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1_000;

/// ZEB-925: local expiry-tombstone retention — how long a TTL-expired key's
/// tombstone suppresses resurrection-by-merge in `DmInboxDoc::merge_from`
/// (2×TTL bounds tombstone age; after it, a still-merging sibling can reopen
/// at most one fresh TTL window per retention period).
pub const INBOX_TOMBSTONE_RETENTION_MS: u64 = 2 * INBOX_TTL_MS;

/// ZEB-925: hard bound on the tombstone map (4× the live-entry cap gives
/// headroom for churn; oldest-first eviction beyond it).
pub const INBOX_TOMBSTONE_CAP: usize = 4 * INBOX_GLOBAL_CAP;

// Eviction must never make the tombstone set smaller than what a full inbox
// can expire in one sweep.
const _: () = assert!(INBOX_TOMBSTONE_CAP >= INBOX_GLOBAL_CAP);

/// ZEB-422 (P2): number of full backoff windows a (entry, recipient) pair may
/// sit sent-but-never-acked before each FURTHER Ok-send window also attempts
/// the butler-deposit rung. Windows accumulate via the Ok-arm's AttemptState
/// bump in `drain_phase_c`; an ack clears the pair entirely.
pub const DEPOSIT_NOACK_WINDOWS: u32 = 2;

/// The sender → butler deposit request body (canonical CBOR, sent inside a
/// length-prefixed frame). Spec §4: the butler verifies the enrollment cert
/// chain, the `sig`, and admission (friend graph) BEFORE any decryption of
/// `sealed_blob`; the inbox quotas are enforced atomically inside the
/// persist step (PR #221 round 1 — duplicate keys exempt).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DepositFrame {
    /// Recipient OwnerAddr bytes — whose inbox this deposit is for.
    #[serde(rename = "ro")]
    pub recipient_owner: [u8; 16],
    /// Sender OwnerAddr bytes — admission-checked against the butler's
    /// friend graph.
    #[serde(rename = "so")]
    pub sender_owner: [u8; 16],
    /// Canonical CBOR of the sender device's `EnrollmentCert`, verified
    /// against the sender owner's master key.
    #[serde(rename = "ec", with = "serde_bytes")]
    pub sender_enrollment_cert: Vec<u8>,
    /// 64-byte device ed25519 signature over
    /// `BUTLER_DEPOSIT_SIG_DOMAIN ‖ recipient_owner ‖ sealed_blob`
    /// ([`deposit_sig_payload`]).
    #[serde(rename = "sg", with = "serde_bytes")]
    pub sig: Vec<u8>,
    /// [`DepositPayload`] canonical CBOR, sealed to the butler device's
    /// birational X25519 key with [`BUTLER_DEPOSIT_SEAL_INFO`].
    #[serde(rename = "sb", with = "serde_bytes")]
    pub sealed_blob: Vec<u8>,
    /// ZEB-677: canonical CBOR of `Vec<EnrollmentCert>` — the Master-issued
    /// signer certs backing a Quorum-issued `sender_enrollment_cert`. Empty
    /// when the cert is Master-issued (key omitted on the wire; old butlers
    /// ignore it and keep rejecting quorum certs).
    #[serde(
        rename = "sc",
        default,
        skip_serializing_if = "Vec::is_empty",
        with = "serde_bytes"
    )]
    pub signer_certs_cbor: Vec<u8>,
}

/// The butler → sender acknowledgement, returned only AFTER the deposit is
/// persisted into `dm-inbox-v1` (spec §4 step 7: persist-then-ack).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DepositAck {
    #[serde(rename = "sp")]
    pub space_id: [u8; 16],
    #[serde(rename = "mc", with = "serde_bytes")]
    pub message_cid: Vec<u8>,
}

/// Plaintext inside `sealed_blob` (sealed to birational(vk) of the butler
/// device): the full signed CidNotify packet plus the CAS storage blob —
/// exactly what the recipient's normal DM receive path consumes on ingestion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DepositPayload {
    /// Signed CidNotify packet bytes (discriminant+body+sig). `None` for a
    /// ZEB-505 standalone durable DM *invite* deposit (no message) — then
    /// `invite_packet` is the sole payload and `storage_blob` is empty.
    /// Symmetric to `invite_packet`/`iv`; backward-compatible since legacy
    /// payloads always carry `cn` (decoding to `Some`).
    #[serde(
        rename = "cn",
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_bytes"
    )]
    pub cidnotify_packet: Option<Vec<u8>>,
    /// The CAS storage blob ([ver][nonce][ct][tag]). Empty for an invite-only
    /// deposit (`cidnotify_packet` is `None`).
    #[serde(rename = "pl", with = "serde_bytes")]
    pub storage_blob: Vec<u8>,
    /// ZEB-483: optional signed DmInvite packet bytes (a `DmPacket::Invite`),
    /// piggybacked so an offline-at-create recipient bootstraps the DM Space
    /// from the deposit rung. Opaque to the butler/relay (sealed end-to-end);
    /// applied + verified on recover. `None` for non-DM deposits and legacy
    /// senders.
    #[serde(
        rename = "iv",
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_bytes"
    )]
    pub invite_packet: Option<Vec<u8>>,
    /// ZEB-691: signed `RevocationPush` frame bytes for the recipient's inbox
    /// sweeper to apply via `handle_revocation_push`. `None` for message /
    /// invite / legacy deposits.
    #[serde(
        rename = "rp",
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_bytes"
    )]
    pub revocation_push: Option<Vec<u8>>,
    /// ZEB-674 C3: canonical CBOR of `Vec<serde_bytes Vec<u8>>` — the
    /// per-device sealed [`crate::file_sharing::FileGrantInner`] blobs
    /// (`crate::file_sharing::seal_grant_for_devices`), piggybacked so a
    /// file-share grant rides the offline-capable butler-deposit rung. Opaque
    /// to the butler/relay (each inner blob is sealed end-to-end to a
    /// specific grantee device's X25519 key); decoded and fanned out to
    /// `OwnerState.file_grants` on the recipient's ingest path
    /// (`ingest_grant_push`, Task 4). `None` for non-grant / legacy deposits.
    #[serde(
        rename = "gp",
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_bytes"
    )]
    pub grant_push: Option<Vec<u8>>,
    /// ZEB-730: owner→grantee file-grant revoke. Canonical CBOR of the revoked
    /// root ContentId ([u8;32]). The whole `DepositPayload` is frame-sealed to
    /// the recipient, so — unlike `grant_push` (which per-device-seals a DEK
    /// secret) — this carries the CID in the clear inside the seal: there is no
    /// secret here the grantee doesn't already hold, hence no inner seal. Sender
    /// is the butler-verified `frame.sender_owner`, never a payload claim. `None`
    /// for message / invite / revocation / grant / legacy deposits.
    #[serde(
        rename = "gr",
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_bytes"
    )]
    pub grant_revoke: Option<Vec<u8>>,
}

// Manual CanonicalPayload registration (the `impl_canonical!` macro in
// owner_state_types.rs is module-private; mirror `dm_inbox_crdt`).
impl CanonicalPayloadSealed for DepositFrame {}
impl CanonicalPayload for DepositFrame {}
impl CanonicalPayloadSealed for DepositAck {}
impl CanonicalPayload for DepositAck {}
impl CanonicalPayloadSealed for DepositPayload {}
impl CanonicalPayload for DepositPayload {}

/// Errors from the deposit wire layer (codec + framing).
#[derive(Debug, thiserror::Error)]
pub enum DepositWireError {
    /// Length prefix of zero or exceeding [`DEPOSIT_MAX_FRAME_BYTES`].
    /// Raised BEFORE any body byte is read or allocated.
    #[error("deposit frame length {len} out of bounds (min 1, max {max})")]
    FrameOutOfBounds { len: usize, max: usize },
    #[error("deposit frame I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("deposit CBOR encode: {0}")]
    Encode(String),
    #[error("deposit CBOR decode: {0}")]
    Decode(String),
}

impl From<crate::iroh_framing::FramingError> for DepositWireError {
    fn from(e: crate::iroh_framing::FramingError) -> Self {
        match e {
            crate::iroh_framing::FramingError::OutOfBounds(f) => {
                DepositWireError::FrameOutOfBounds {
                    len: f.len,
                    max: f.max,
                }
            }
            crate::iroh_framing::FramingError::Io(io) => DepositWireError::Io(io),
        }
    }
}

/// The single shared signature construction for deposit frames:
/// `BUTLER_DEPOSIT_SIG_DOMAIN ‖ recipient_owner ‖ sealed_blob`. Both the
/// sender (signer) and the butler (verifier) MUST build the signed bytes
/// through this function — never inline the concatenation.
pub fn deposit_sig_payload(recipient_owner: &[u8; 16], sealed_blob: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        BUTLER_DEPOSIT_SIG_DOMAIN.len() + recipient_owner.len() + sealed_blob.len(),
    );
    out.extend_from_slice(BUTLER_DEPOSIT_SIG_DOMAIN);
    out.extend_from_slice(recipient_owner);
    out.extend_from_slice(sealed_blob);
    out
}

/// Encode a [`DepositFrame`] to canonical CBOR (no length prefix — the
/// caller frames it with [`write_length_prefixed`]).
pub fn encode_deposit_frame(frame: &DepositFrame) -> Result<Vec<u8>, DepositWireError> {
    canonical_cbor_encode(frame).map_err(|e| DepositWireError::Encode(format!("{e}")))
}

/// Decode a [`DepositFrame`] from canonical CBOR (trailing bytes rejected).
pub fn decode_deposit_frame(bytes: &[u8]) -> Result<DepositFrame, DepositWireError> {
    canonical_cbor_decode(bytes).map_err(|e| DepositWireError::Decode(format!("{e}")))
}

/// Encode a [`DepositAck`] to canonical CBOR (no length prefix).
pub fn encode_deposit_ack(ack: &DepositAck) -> Result<Vec<u8>, DepositWireError> {
    canonical_cbor_encode(ack).map_err(|e| DepositWireError::Encode(format!("{e}")))
}

/// Decode a [`DepositAck`] from canonical CBOR (trailing bytes rejected).
pub fn decode_deposit_ack(bytes: &[u8]) -> Result<DepositAck, DepositWireError> {
    canonical_cbor_decode(bytes).map_err(|e| DepositWireError::Decode(format!("{e}")))
}

/// Encode a [`DepositPayload`] to canonical CBOR (the plaintext that gets
/// sealed into `DepositFrame::sealed_blob`).
pub fn encode_deposit_payload(payload: &DepositPayload) -> Result<Vec<u8>, DepositWireError> {
    canonical_cbor_encode(payload).map_err(|e| DepositWireError::Encode(format!("{e}")))
}

/// Decode a [`DepositPayload`] from canonical CBOR (trailing bytes rejected).
pub fn decode_deposit_payload(bytes: &[u8]) -> Result<DepositPayload, DepositWireError> {
    canonical_cbor_decode(bytes).map_err(|e| DepositWireError::Decode(format!("{e}")))
}

/// ZEB-730: encode a revoked root ContentId as the canonical CBOR of a 32-byte
/// string — the wire value carried in [`DepositPayload::grant_revoke`]. A
/// fixed-size 32-byte input can never fail to encode.
pub fn encode_grant_revoke(cid: [u8; 32]) -> Vec<u8> {
    let mut buf = Vec::new();
    ciborium::into_writer(&serde_bytes::Bytes::new(&cid), &mut buf)
        .expect("encode_grant_revoke: fixed-size input never fails");
    buf
}

/// ZEB-730: decode a [`DepositPayload::grant_revoke`] wire value back to the
/// revoked root ContentId. Rejects any non-32-byte payload (fail-closed).
pub fn decode_grant_revoke(bytes: &[u8]) -> Result<[u8; 32], String> {
    let buf: serde_bytes::ByteBuf =
        ciborium::from_reader(bytes).map_err(|e| format!("grant_revoke decode: {e}"))?;
    let arr: [u8; 32] = buf
        .as_ref()
        .try_into()
        .map_err(|_| "grant_revoke: not 32 bytes".to_string())?;
    Ok(arr)
}

/// Write `body` as a `[u32 LE length-prefix][body]` frame. Rejects bodies
/// exceeding [`DEPOSIT_MAX_FRAME_BYTES`] (or empty) before writing anything.
pub async fn write_length_prefixed<W: AsyncWrite + Unpin>(
    writer: &mut W,
    body: &[u8],
) -> Result<(), DepositWireError> {
    write_length_prefixed_with_max(writer, body, DEPOSIT_MAX_FRAME_BYTES).await
}

/// Like [`write_length_prefixed`] but with a caller-supplied frame cap. Used by
/// the relay pull RESPONSE path, whose frame batches many held blobs and so
/// needs the larger [`crate::community_relay::RELAY_PULL_MAX_FRAME_BYTES`] cap
/// rather than the per-blob [`DEPOSIT_MAX_FRAME_BYTES`]. Rejects bodies
/// exceeding `max` (or empty) before writing anything.
pub async fn write_length_prefixed_with_max<W: AsyncWrite + Unpin>(
    writer: &mut W,
    body: &[u8],
    max: usize,
) -> Result<(), DepositWireError> {
    crate::iroh_framing::write_len_prefixed(
        writer,
        body,
        max,
        crate::iroh_framing::Endian::Le,
        false,
    )
    .await
    .map_err(DepositWireError::from)
}

/// Read a `[u32 LE length-prefix][body]` frame. A prefix of zero or
/// exceeding [`DEPOSIT_MAX_FRAME_BYTES`] is rejected BEFORE the body is
/// read or allocated — an attacker-supplied prefix never drives an
/// allocation past the cap.
pub async fn read_length_prefixed<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Vec<u8>, DepositWireError> {
    read_length_prefixed_with_max(reader, DEPOSIT_MAX_FRAME_BYTES).await
}

/// Like [`read_length_prefixed`] but with a caller-supplied frame cap. Used by
/// the relay pull RESPONSE path (see
/// [`crate::community_relay::RELAY_PULL_MAX_FRAME_BYTES`]). A prefix of zero or
/// exceeding `max` is rejected BEFORE the body is read or allocated — an
/// attacker-supplied prefix never drives an allocation past `max`.
pub async fn read_length_prefixed_with_max<R: AsyncRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> Result<Vec<u8>, DepositWireError> {
    crate::iroh_framing::read_len_prefixed(reader, max, crate::iroh_framing::Endian::Le, false)
        .await
        .map_err(DepositWireError::from)
}

// =====================================================================
// Sender-side deposit rung (ZEB-418 P1 Task 8)
// =====================================================================

/// One deposit work unit handed from `DmOutbox`'s drain to the
/// [`ButlerDepositClient`]: everything the rung needs that only the outbox
/// knows. The CidNotify packet bytes are built by the outbox with the SAME
/// construction the direct path uses (`dm_envelope::build_signed_cidnotify`
/// signed by the Reticulum device key), so the butler's inner verification
/// sees exactly what a direct arrival would carry.
#[derive(Debug, Clone, PartialEq)]
pub struct ButlerDepositRequest {
    pub entry_id: OutboxEntryId,
    pub recipient_owner: OwnerAddr,
    pub space_id: SpaceId,
    /// ZEB-505: `None` for a standalone durable invite deposit (no message).
    pub message_cid: Option<ContentId>,
    /// Signed CidNotify packet bytes (discriminant+body+sig). ZEB-505: `None`
    /// for a standalone durable invite deposit — then `invite_packet` is the
    /// sole payload.
    pub cidnotify_packet: Option<Vec<u8>>,
    /// ZEB-483: signed DmInvite packet bytes for the recipient to bootstrap the
    /// DM Space from the deposit rung; `None` for non-DM Spaces. Copied verbatim
    /// into the sealed `DepositPayload` by each deposit client. ZEB-505: MUST be
    /// `Some` when `cidnotify_packet` is `None` (an invite-only deposit).
    pub invite_packet: Option<Vec<u8>>,
    /// ZEB-691: signed `RevocationPush` frame bytes. When `Some`, this is a
    /// revocation deposit — `cidnotify_packet`/`invite_packet`/`message_cid` are
    /// all `None` and the ack binds to `REVOCATION_DEPOSIT_MARKER`.
    pub revocation_push: Option<Vec<u8>>,
    /// ZEB-674 (C3): opaque `grant_push` wire bytes (canonical CBOR of the
    /// per-device sealed `FileGrantInner` blobs — `file_sharing::encode_grant_push`).
    /// When `Some`, this is a pure file-share grant deposit —
    /// `message_cid`/`cidnotify_packet`/`invite_packet`/`revocation_push` are all
    /// `None`, `space_id` is zero, and the ack binds to `GRANT_DEPOSIT_MARKER`.
    pub grant_push: Option<Vec<u8>>,
    /// ZEB-730: opaque `grant_revoke` wire bytes (canonical CBOR of the revoked
    /// root ContentId — `butler_deposit::encode_grant_revoke`). When `Some`, this
    /// is a pure file-grant revoke deposit —
    /// `message_cid`/`cidnotify_packet`/`invite_packet`/`revocation_push`/`grant_push`
    /// are all `None`, `space_id` is zero, and the ack binds to
    /// `GRANT_REVOKE_DEPOSIT_MARKER`.
    pub grant_revoke: Option<Vec<u8>>,
    /// Wall-clock now (ms) at candidacy time — drives the `bs_at` freshness
    /// check against the resolved routing blob.
    pub now_ms: u64,
}

/// Outcome of one deposit-rung attempt. Spec §6 is normative: NOTHING here
/// may make delivery worse than today — every non-[`Self::Acked`] outcome
/// leaves the outbox entry exactly as the transient direct failure that
/// triggered the rung left it (same shared `AttemptState` backoff; no extra
/// bump, no clear, no drop).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepositRungOutcome {
    /// A butler accepted the deposit and acked; the ack's space/CID were
    /// verified against the request. The caller marks the recipient
    /// delivered via the existing idempotent `mark_ack_delivered`.
    Acked,
    /// The recipient advertises no fresh butler set (missing/stale `bs_at`,
    /// or no routing record at all) — the rung is skipped silently and the
    /// existing retry chain proceeds unchanged (spec §6 failure table).
    SkippedNoFreshButlerSet,
    /// Fresh butler entries existed but every one failed (dial/IO timeout,
    /// bad ack, seal/build error, blob unavailable). Treated exactly like
    /// the skip by the caller; the carried message is debug-telemetry only.
    Failed(String),
}

/// Sender-side deposit seam injected into `DmOutbox` (mirrors how
/// `DmTransport` keeps the wire out of the outbox tests): production is
/// [`IrohButlerDepositClient`]; outbox unit tests install a mock.
#[async_trait]
pub trait ButlerDepositClient: Send + Sync {
    async fn deposit(&self, req: &ButlerDepositRequest) -> DepositRungOutcome;
}

/// Sender-side last-resort rung (D40). Mirrors [`ButlerDepositClient`]: given
/// the same outbox candidate the butler rung uses, seal the DepositPayload to
/// R's advertised butler-set device(s) and deposit to a relay in a shared
/// community. Returns true iff at least one relay acked. Never touches
/// AttemptState (the caller treats an acked candidate as delivered-pending-pull).
///
/// ZEB-548 Stage 2: moved here from `community_relay` (beside its twin
/// [`ButlerDepositClient`]) so the spine's `dm_outbox` no longer depends up on
/// the community tier; re-exported from `community_relay` for byte-stable call
/// sites.
#[async_trait]
pub trait CommunityRelayDepositClient: Send + Sync {
    async fn deposit(&self, req: &ButlerDepositRequest) -> bool;
}

/// Build a [`DepositFrame`] for ONE butler-set entry — the exact
/// construction `iroh_butler_acceptor::handle_deposit_core` verifies:
///
/// * `sealed_blob` = [`DepositPayload`] canonical CBOR sealed to
///   `birational(vk)` of the butler device
///   (`dm_signing::ed25519_pub_to_x25519` of the entry's
///   `device_ed25519_verify`) under [`BUTLER_DEPOSIT_SEAL_INFO`];
/// * `sig` = the sender's CERT-BOUND enrolled device ed25519 (key #2,
///   ZEB-339 — `DmOutbox::new` pins it to
///   `enrollment_cert.device_pubkeys.classical.ed25519_verify`, the key the
///   acceptor extracts from the cert and verifies against) over
///   [`deposit_sig_payload`];
/// * `sender_enrollment_cert` = the canonical CBOR the acceptor
///   strict-decodes (`harmony_owner::cbor::to_canonical` of the
///   `state.enrollments` cert).
///
/// `pub` (not `pub(crate)`) so `tests/butler_deposit_integration.rs` — which
/// compiles against the public API — exercises the EXACT sender construction
/// rather than a hand-rolled twin (ZEB-418 P1 Task 9).
pub fn build_deposit_frame(
    recipient_owner: &[u8; 16],
    sender_owner: &[u8; 16],
    sender_enrollment_cert: &[u8],
    device_signing_key: &ed25519_dalek::SigningKey,
    butler_device_ed25519_verify: &[u8; 32],
    payload: &DepositPayload,
) -> Result<DepositFrame, String> {
    let payload_bytes =
        encode_deposit_payload(payload).map_err(|e| format!("encode deposit payload: {e}"))?;
    let seal_pub = crate::dm_signing::ed25519_pub_to_x25519(butler_device_ed25519_verify)
        .map_err(|e| format!("butler vk -> x25519: {e:?}"))?;
    let sealed_blob = crate::dm_signing::seal_to_owner_with_info(
        &seal_pub,
        &payload_bytes,
        BUTLER_DEPOSIT_SEAL_INFO,
    )
    .map_err(|e| format!("seal deposit payload: {e:?}"))?;
    let sig = device_signing_key
        .sign(&deposit_sig_payload(recipient_owner, &sealed_blob))
        .to_bytes()
        .to_vec();
    Ok(DepositFrame {
        recipient_owner: *recipient_owner,
        sender_owner: *sender_owner,
        sender_enrollment_cert: sender_enrollment_cert.to_vec(),
        sig,
        sealed_blob,
        // ZEB-677: bundle threading for quorum-certed senders lands with the
        // ceremony slices (S4); every self-cert today is Master-issued.
        signer_certs_cbor: Vec::new(),
    })
}

/// Pick the butler set to dial from the recipient's resolved routing blobs,
/// carrying each blob's reachability source for the per-source freshness policy:
/// durable-CRDT entries are window-exempt
/// ([`crate::reachability_record::durable_butler_set`]) because their vk
/// seal-targets stay valid even when the recipient's primary device is
/// long-offline; pkarr-live entries keep the 15-min window
/// ([`crate::reachability_record::fresh_butler_set`]). Each of the owner's
/// devices publishes a whole-fleet set (spec §3 — "both writers describe the
/// same fleet"), so among the entries that yield a non-empty set we take the one
/// with the newest `bs_at` rather than merging across blobs. The max-2 cap is
/// enforced per blob by the underlying readers; an empty return means "no
/// resolvable butler set" (rung skipped silently).
///
/// Task 4 wires both production deposit rungs to this selector:
/// [`IrohButlerDepositClient::deposit`] (same module) and
/// [`crate::community_relay_prod::ReachabilityButlerSetResolve::resolve_targets`].
pub fn freshest_butler_set_by_source(
    tagged: &[(
        ReachabilityAnnouncePayload,
        crate::reachability_resolver::ReachabilitySource,
    )],
    now_ms: u64,
) -> Vec<ButlerSetEntry> {
    use crate::reachability_resolver::ReachabilitySource;
    tagged
        .iter()
        .map(|(b, src)| {
            let set = match src {
                ReachabilitySource::DurableCrdt => {
                    crate::reachability_record::durable_butler_set(b)
                }
                ReachabilitySource::PkarrLive => {
                    crate::reachability_record::fresh_butler_set(b, now_ms)
                }
                // ZEB-510: fleet-sibling records are dial-only (empty
                // butler_set by construction) and never shape the butler
                // view — see ResolverSlots::durable_preferred.
                ReachabilitySource::FleetSibling => Vec::new(),
            };
            (b.bs_at, set)
        })
        .filter(|(_, set)| !set.is_empty())
        .max_by_key(|(bs_at, _)| *bs_at)
        .map(|(_, set)| set)
        .unwrap_or_default()
}

/// Production [`ButlerDepositClient`] (ZEB-418 P1 Task 8): resolve the
/// RECIPIENT OWNER's routing record (in-memory CRDT cache first, pkarr
/// fallback on miss, via `ReachabilityResolver::resolve_async_with_source`),
/// read the freshest advertised butler set via [`freshest_butler_set_by_source`]
/// (durable-CRDT entries window-exempt, pkarr-live windowed — Task 4), and try
/// each entry in priority order —
/// dial `harmony/butler-deposit/v1`, write the length-prefixed sealed
/// frame, read the length-prefixed ack, verify the ack matches the deposit.
/// First ack wins; every entry failing = rung failure (the caller's retry
/// chain is untouched either way — spec §6).
pub struct IrohButlerDepositClient {
    pub endpoint: Arc<crate::iroh_endpoint::IrohEndpoint>,
    pub resolver: crate::reachability_resolver::ReachabilityResolver,
    /// CAS holding the message storage blob (written by `send_dm` step 5).
    pub cas: Arc<dyn crate::content_store::ContentStore>,
    /// This (sender) owner's address bytes — `DepositFrame.sender_owner`.
    pub sender_owner: [u8; 16],
    /// Canonical CBOR of this device's own Master `EnrollmentCert` (what
    /// the acceptor strict-decodes; see [`build_deposit_frame`]).
    pub enrollment_cert_bytes: Vec<u8>,
    /// The cert-bound enrolled device signing key (#2, ZEB-339) — signs
    /// `DepositFrame.sig`.
    pub device_signing_key: Arc<ed25519_dalek::SigningKey>,
    /// Per-entry deadline bounding the WHOLE dial+frame+ack exchange for
    /// one butler entry (production uses the acceptor's
    /// `DEFAULT_BUTLER_IO_DEADLINE_MS`).
    pub io_deadline: Duration,
}

impl IrohButlerDepositClient {
    /// One butler entry: dial → frame → ack, bounded by `io_deadline`.
    async fn deposit_to_entry(
        &self,
        entry: &ButlerSetEntry,
        frame: &DepositFrame,
        expect_space: &[u8; 16],
        expect_cid: &[u8],
    ) -> Result<(), String> {
        let exchange = async {
            let ep_id = iroh::EndpointId::from_bytes(&entry.iroh_endpoint_id)
                .map_err(|e| format!("butler endpoint id: {e}"))?;
            let mut addr = iroh::EndpointAddr::new(ep_id);
            if !entry.home_relay.is_empty() {
                match entry.home_relay.parse::<iroh::RelayUrl>() {
                    Ok(url) => addr = addr.with_relay_url(url),
                    Err(e) => tracing::trace!(
                        relay = %entry.home_relay,
                        "ZEB-418: skip malformed butler home_relay: {e}"
                    ),
                }
            }
            let conn = self
                .endpoint
                .inner()
                .connect(addr, crate::iroh_endpoint::alpn::HARMONY_BUTLER_DEPOSIT_V1)
                .await
                .map_err(|e| format!("connect: {e}"))?;
            let (mut send, mut recv) = conn.open_bi().await.map_err(|e| format!("open_bi: {e}"))?;
            let frame_bytes =
                encode_deposit_frame(frame).map_err(|e| format!("encode frame: {e}"))?;
            write_length_prefixed(&mut send, &frame_bytes)
                .await
                .map_err(|e| format!("write frame: {e}"))?;
            send.finish().map_err(|e| format!("finish: {e}"))?;
            let ack_bytes = read_length_prefixed(&mut recv)
                .await
                .map_err(|e| format!("read ack: {e}"))?;
            let ack = decode_deposit_ack(&ack_bytes).map_err(|e| format!("decode ack: {e}"))?;
            // A reject closes the stream without detail (read fails above);
            // a present-but-mismatched ack is a butler bug or a confused
            // exchange — never mark delivered off it.
            if ack.space_id != *expect_space || ack.message_cid != expect_cid {
                return Err("ack space/cid mismatch".to_string());
            }
            // Dialer-driven close: the butler waits on `conn.closed()` after
            // writing the ack (acceptor shell), so close from this side.
            conn.close(0u32.into(), b"");
            Ok(())
        };
        tokio::time::timeout(self.io_deadline, exchange)
            .await
            .map_err(|_| "deposit IO timeout".to_string())?
    }
}

/// Pure request→(ack marker, sealed payload) mapping for a single deposit,
/// factored out of [`IrohButlerDepositClient::deposit`] so the
/// marker-select + payload-shape invariant is unit-testable without any
/// network / CAS I/O. `storage_blob` is the CAS blob for a message deposit
/// (empty for every non-message deposit); the caller fetches it — the only
/// async step — before calling. The ack marker is the message CID bytes for a
/// message deposit, else the fixed per-variant marker for whichever single
/// sub-payload the request carries. A valid request sets exactly one
/// sub-payload `Some`, so the `None` arms are mutually exclusive; the ordering
/// mirrors the historical precedence (message → revocation → grant → grant
/// revoke → invite).
fn build_deposit_payload_and_marker(
    req: &ButlerDepositRequest,
    storage_blob: Vec<u8>,
) -> (Vec<u8>, DepositPayload) {
    // ZEB-691: a revocation deposit has no message CID and its own ack marker;
    // ZEB-674: a pure grant deposit likewise; ZEB-730: a pure grant-revoke
    // deposit likewise; an invite-only deposit keeps the ZEB-505 marker.
    let expect_cid: Vec<u8> = match req.message_cid {
        Some(message_cid) => message_cid.to_bytes().to_vec(),
        None if req.revocation_push.is_some() => REVOCATION_DEPOSIT_MARKER.to_vec(),
        None if req.grant_push.is_some() => GRANT_DEPOSIT_MARKER.to_vec(),
        None if req.grant_revoke.is_some() => GRANT_REVOKE_DEPOSIT_MARKER.to_vec(),
        None => INVITE_ONLY_DEPOSIT_MARKER.to_vec(),
    };
    let payload = DepositPayload {
        cidnotify_packet: req.cidnotify_packet.clone(),
        storage_blob,
        invite_packet: req.invite_packet.clone(),
        revocation_push: req.revocation_push.clone(),
        // ZEB-674 Task 6 (C3): carry the opaque per-device sealed grant through
        // to the acceptor's grant-only arm (which acks with
        // `GRANT_DEPOSIT_MARKER` and persists under `grant_key`). The butler
        // cannot open the seals (`butler_cannot_open_grant_push`).
        grant_push: req.grant_push.clone(),
        // ZEB-730: carry the revoked-CID CBOR through to the acceptor's
        // grant-revoke arm (acks with `GRANT_REVOKE_DEPOSIT_MARKER`). Sealed by
        // the frame; unlike `grant_push` it carries no per-device secret.
        grant_revoke: req.grant_revoke.clone(),
    };
    (expect_cid, payload)
}

#[async_trait]
impl ButlerDepositClient for IrohButlerDepositClient {
    async fn deposit(&self, req: &ButlerDepositRequest) -> DepositRungOutcome {
        // 1. Resolve the recipient OWNER's routing blobs and read the
        // freshest advertised butler set. Missing/stale → rung silently
        // skipped (spec §6: a stale ad can never make delivery worse).
        let tagged = self
            .resolver
            .resolve_async_with_source(&req.recipient_owner)
            .await;
        let entries = freshest_butler_set_by_source(&tagged, req.now_ms);
        if entries.is_empty() {
            return DepositRungOutcome::SkippedNoFreshButlerSet;
        }

        // 2. Build the deposit payload. A message deposit fetches the CAS
        // storage blob (the butler re-derives message_cid via for_book) and
        // binds the ack to that cid. A ZEB-505 invite-only deposit (no
        // message_cid) carries the invite alone — no blob to fetch — and binds
        // the ack to a fixed invite marker instead. (`push_deposit_candidate`
        // guarantees `invite_packet` is Some when `message_cid` is None.)
        // Fetched only after a fresh butler set is confirmed — no CAS work for a
        // skipped rung.
        // A message deposit fetches its CAS storage blob (the only async step);
        // every non-message deposit carries an empty blob. The marker-select +
        // payload build is then a pure mapping — see
        // `build_deposit_payload_and_marker` (unit-tested without I/O).
        let storage_blob: Vec<u8> = match req.message_cid {
            Some(message_cid) => match self.cas.get(&message_cid).await {
                Ok(Some(blob)) => blob,
                Ok(None) => {
                    return DepositRungOutcome::Failed("storage blob missing from CAS".to_string())
                }
                Err(e) => return DepositRungOutcome::Failed(format!("CAS get: {e}")),
            },
            None => Vec::new(),
        };
        let (expect_cid, payload) = build_deposit_payload_and_marker(req, storage_blob);

        // 3. Priority order, first ack wins. The frame is rebuilt per
        // entry: the sealed blob targets THAT entry's device key, and the
        // sig binds to the sealed bytes.
        let mut last_err = "no butler entries attempted".to_string();
        for entry in &entries {
            let frame = match build_deposit_frame(
                &req.recipient_owner.0,
                &self.sender_owner,
                &self.enrollment_cert_bytes,
                &self.device_signing_key,
                &entry.device_ed25519_verify,
                &payload,
            ) {
                Ok(f) => f,
                Err(e) => {
                    last_err = e;
                    continue;
                }
            };
            match self
                .deposit_to_entry(entry, &frame, &req.space_id.0, &expect_cid)
                .await
            {
                Ok(()) => return DepositRungOutcome::Acked,
                Err(e) => {
                    tracing::debug!(
                        butler_device = %hex::encode(entry.device_id),
                        error = %e,
                        "ZEB-418: butler deposit entry failed; trying next"
                    );
                    last_err = e;
                }
            }
        }
        DepositRungOutcome::Failed(last_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dm_signing::{open_from_owner, open_from_owner_with_info, seal_to_owner_with_info};

    /// Deterministic test X25519 keypair (mirrors `dm_signing`'s
    /// `epoch_seal_tests::make_x25519_keypair`). Returns (priv, pub).
    fn make_x25519_keypair(seed_byte: u8) -> ([u8; 32], [u8; 32]) {
        use hkdf::Hkdf;
        use sha2::Sha256;
        use x25519_dalek::{PublicKey, StaticSecret};

        let seed = [seed_byte; 32];
        let hk = Hkdf::<Sha256>::new(None, &seed);
        let mut scalar = [0u8; 32];
        hk.expand(b"harmony-zeb-418-test-x25519-scalar", &mut scalar)
            .expect("HKDF 32 bytes always works");

        let secret = StaticSecret::from(scalar);
        let public = PublicKey::from(&secret);
        (scalar, *public.as_bytes())
    }

    /// A fully deterministic DepositFrame for the byte-pin fixture: fixed
    /// repeated-byte addresses, a fixed fake cert blob, and a fixed
    /// `sig: [0x09; 64]` (pins BYTE LAYOUT only, not a real signature —
    /// mirrors the wire_format_zeb375 fixture discipline).
    fn fixture_frame() -> DepositFrame {
        DepositFrame {
            signer_certs_cbor: Vec::new(),
            recipient_owner: [0x11; 16],
            sender_owner: [0x22; 16],
            sender_enrollment_cert: vec![0xE1, 0xE2, 0xE3, 0xE4],
            sig: vec![0x09; 64],
            sealed_blob: vec![0x5B, 0x5C, 0x5D],
        }
    }

    #[test]
    fn deposit_frame_cbor_round_trips() {
        let frame = fixture_frame();
        let bytes = encode_deposit_frame(&frame).expect("encode");
        let back = decode_deposit_frame(&bytes).expect("decode");
        assert_eq!(back, frame);

        // Canonical-decode rejects trailing bytes (fingerprint safety).
        let mut padded = bytes.clone();
        padded.push(0x00);
        assert!(matches!(
            decode_deposit_frame(&padded),
            Err(DepositWireError::Decode(_))
        ));
    }

    /// Byte-pin the DepositFrame wire encoding. A failure here is a
    /// wire-protocol break (field rename, key-length change, reorder,
    /// encoding-shape change) — review carefully before updating the pinned
    /// hex (cross-version compat: deployed senders/butlers must agree).
    ///
    /// DO NOT REGENERATE casually: the hex below was captured from the
    /// FIRST VERIFIED RUN of this test (run once with "FILL_AFTER", paste
    /// the actual hex from the panic message). Any change to it after that
    /// first pin means the wire format changed and old peers break.
    /// (Regenerated ONCE pre-ship, 2026-06-09, when the Vec<u8> payload
    /// fields moved to serde_bytes/bstr packing — nothing had shipped yet.)
    #[test]
    fn deposit_frame_wire_fixture_pinned() {
        const EXPECTED_DEPOSIT_FRAME_HEX: &str = "a562726f901111111111111111111111111111111162736f90182218221822182218221822182218221822182218221822182218221822182262656344e1e2e3e4627367584009090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909090909627362435b5c5d";
        let bytes = encode_deposit_frame(&fixture_frame()).expect("encode");
        let actual = hex::encode(&bytes);
        assert_eq!(
            actual, EXPECTED_DEPOSIT_FRAME_HEX,
            "DepositFrame wire encoding drifted from the pinned fixture.\nactual hex: {actual}"
        );
    }

    #[test]
    fn ack_round_trips() {
        let ack = DepositAck {
            space_id: [0x33; 16],
            message_cid: vec![0xCD; 32],
        };
        let bytes = encode_deposit_ack(&ack).expect("encode");
        let back = decode_deposit_ack(&bytes).expect("decode");
        assert_eq!(back, ack);
    }

    #[test]
    fn deposit_payload_round_trips() {
        let payload = DepositPayload {
            cidnotify_packet: Some(vec![0x01, 0x02, 0x03]),
            storage_blob: vec![0x04, 0x05, 0x06, 0x07],
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
            grant_revoke: None,
        };
        let bytes = encode_deposit_payload(&payload).expect("encode");
        let back = decode_deposit_payload(&bytes).expect("decode");
        assert_eq!(back, payload);
    }

    /// ZEB-730: a `DepositPayload` carrying ONLY `grant_revoke` (all other
    /// sub-payloads `None`, `storage_blob` empty) round-trips byte-identically,
    /// and the carried CBOR decodes back to the revoked root ContentId.
    #[test]
    fn deposit_payload_round_trips_grant_revoke() {
        let cid = [7u8; 32];
        let payload = DepositPayload {
            cidnotify_packet: None,
            storage_blob: Vec::new(),
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
            grant_revoke: Some(encode_grant_revoke(cid)),
        };
        let bytes = encode_deposit_payload(&payload).expect("encode");
        let decoded: DepositPayload = decode_deposit_payload(&bytes).expect("decode");
        assert_eq!(decoded, payload);
        assert_eq!(
            decode_grant_revoke(decoded.grant_revoke.as_deref().unwrap()).unwrap(),
            cid
        );

        // A legacy payload (no `gr` key) decodes grant_revoke to None, and the
        // `gr` key is omitted entirely when grant_revoke is None.
        let legacy = DepositPayload {
            cidnotify_packet: Some(vec![9]),
            storage_blob: vec![1],
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
            grant_revoke: None,
        };
        let lbytes = encode_deposit_payload(&legacy).expect("encode legacy");
        assert_eq!(decode_deposit_payload(&lbytes).unwrap().grant_revoke, None);
        assert!(
            !lbytes.windows(2).any(|w| w == b"gr"),
            "gr key omitted when grant_revoke is None"
        );
    }

    /// ZEB-730: `encode_grant_revoke`/`decode_grant_revoke` round-trip a
    /// [u8;32] ContentId through canonical CBOR.
    #[test]
    fn encode_grant_revoke_round_trips() {
        let cid = [0xABu8; 32];
        assert_eq!(decode_grant_revoke(&encode_grant_revoke(cid)).unwrap(), cid);
    }

    /// ZEB-730: `decode_grant_revoke` fails closed on non-32-byte / non-CBOR
    /// garbage rather than silently truncating or padding.
    #[test]
    fn decode_grant_revoke_rejects_garbage() {
        assert!(decode_grant_revoke(&[0x00, 0x01, 0x02]).is_err());
        // A 31-byte CBOR byte string is well-formed CBOR but the wrong length.
        let short = encode_grant_revoke_len(31);
        assert!(decode_grant_revoke(&short).is_err());
    }

    /// Local helper: canonical CBOR of an `n`-byte all-zero byte string, to
    /// exercise `decode_grant_revoke`'s length check with well-formed CBOR.
    fn encode_grant_revoke_len(n: usize) -> Vec<u8> {
        let mut buf = Vec::new();
        ciborium::into_writer(&serde_bytes::Bytes::new(&vec![0u8; n]), &mut buf)
            .expect("encode fixed byte string");
        buf
    }

    /// A `ButlerDepositRequest` with every sub-payload `None` — the base for the
    /// sender-side marker-select tests, which set exactly one field `Some`.
    fn empty_deposit_request() -> ButlerDepositRequest {
        ButlerDepositRequest {
            entry_id: OutboxEntryId([0u8; 16]),
            recipient_owner: OwnerAddr([0xAA; 16]),
            space_id: SpaceId([0u8; 16]),
            message_cid: None,
            cidnotify_packet: None,
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
            grant_revoke: None,
            now_ms: 0,
        }
    }

    /// ZEB-730 (Task 3): a `grant_revoke`-only request maps, through the pure
    /// sender-side seam, to a `DepositPayload` carrying ONLY `grant_revoke`
    /// (empty `storage_blob`, every other sub-payload `None`), acked with
    /// `GRANT_REVOKE_DEPOSIT_MARKER`. Exercised directly — no network / CAS I/O.
    #[test]
    fn build_deposit_payload_and_marker_selects_grant_revoke() {
        let cid = [5u8; 32];
        let req = ButlerDepositRequest {
            grant_revoke: Some(encode_grant_revoke(cid)),
            ..empty_deposit_request()
        };
        let (marker, payload) = build_deposit_payload_and_marker(&req, Vec::new());
        assert_eq!(marker, GRANT_REVOKE_DEPOSIT_MARKER.to_vec());
        assert_eq!(
            decode_grant_revoke(payload.grant_revoke.as_deref().unwrap()).unwrap(),
            cid
        );
        assert!(payload.storage_blob.is_empty());
        assert!(payload.cidnotify_packet.is_none());
        assert!(payload.invite_packet.is_none());
        assert!(payload.revocation_push.is_none());
        assert!(payload.grant_push.is_none());
    }

    /// ZEB-730 (Task 3): the extracted seam preserves every pre-existing
    /// variant's marker-select + payload shape byte-for-byte — the guard that
    /// the testability refactor is behavior-preserving for message / revocation
    /// / grant / invite deposits.
    #[test]
    fn build_deposit_payload_and_marker_preserves_existing_variants() {
        // Message deposit: ack marker == message CID bytes; the fetched blob is
        // carried through verbatim; grant_revoke stays absent.
        let mcid = ContentId::from_bytes([9u8; 32]);
        let msg = ButlerDepositRequest {
            message_cid: Some(mcid),
            cidnotify_packet: Some(vec![1, 2, 3]),
            ..empty_deposit_request()
        };
        let (marker, payload) = build_deposit_payload_and_marker(&msg, vec![0xAB, 0xCD]);
        assert_eq!(marker, mcid.to_bytes().to_vec());
        assert_eq!(payload.storage_blob, vec![0xAB, 0xCD]);
        assert_eq!(payload.cidnotify_packet, Some(vec![1, 2, 3]));
        assert!(payload.grant_revoke.is_none());

        // Revocation deposit.
        let rev = ButlerDepositRequest {
            revocation_push: Some(vec![7]),
            ..empty_deposit_request()
        };
        let (marker, payload) = build_deposit_payload_and_marker(&rev, Vec::new());
        assert_eq!(marker, REVOCATION_DEPOSIT_MARKER.to_vec());
        assert_eq!(payload.revocation_push, Some(vec![7]));
        assert!(payload.storage_blob.is_empty());
        assert!(payload.grant_revoke.is_none());

        // Pure grant deposit.
        let gp = ButlerDepositRequest {
            grant_push: Some(vec![8]),
            ..empty_deposit_request()
        };
        let (marker, payload) = build_deposit_payload_and_marker(&gp, Vec::new());
        assert_eq!(marker, GRANT_DEPOSIT_MARKER.to_vec());
        assert_eq!(payload.grant_push, Some(vec![8]));
        assert!(payload.grant_revoke.is_none());

        // Invite-only deposit (no message, no other sub-payload).
        let inv = ButlerDepositRequest {
            invite_packet: Some(vec![4]),
            ..empty_deposit_request()
        };
        let (marker, payload) = build_deposit_payload_and_marker(&inv, Vec::new());
        assert_eq!(marker, INVITE_ONLY_DEPOSIT_MARKER.to_vec());
        assert_eq!(payload.invite_packet, Some(vec![4]));
        assert!(payload.grant_revoke.is_none());
    }

    #[test]
    fn deposit_payload_round_trips_revocation_push_and_decodes_legacy_as_none() {
        let with_rev = DepositPayload {
            cidnotify_packet: None,
            storage_blob: Vec::new(),
            invite_packet: None,
            revocation_push: Some(vec![0x05, 1, 2, 3]),
            grant_push: None,
            grant_revoke: None,
        };
        let bytes = encode_deposit_payload(&with_rev).unwrap();
        assert_eq!(decode_deposit_payload(&bytes).unwrap(), with_rev);

        // Legacy payload (no `rp`) decodes revocation_push to None.
        let legacy = DepositPayload {
            cidnotify_packet: Some(vec![9]),
            storage_blob: vec![1],
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
            grant_revoke: None,
        };
        let lbytes = encode_deposit_payload(&legacy).unwrap();
        assert_eq!(
            decode_deposit_payload(&lbytes).unwrap().revocation_push,
            None
        );
    }

    #[test]
    fn deposit_payload_round_trips_invite_packet_and_decodes_legacy_as_none() {
        // (a) Some(invite) round-trips.
        let with_invite = DepositPayload {
            cidnotify_packet: Some(vec![1, 2, 3]),
            storage_blob: vec![4, 5, 6],
            invite_packet: Some(vec![7, 8, 9]),
            revocation_push: None,
            grant_push: None,
            grant_revoke: None,
        };
        let bytes = encode_deposit_payload(&with_invite).expect("encode");
        assert_eq!(decode_deposit_payload(&bytes).expect("decode"), with_invite);

        // (b) None round-trips and (skip_serializing_if) omits the key.
        let without = DepositPayload {
            cidnotify_packet: Some(vec![1]),
            storage_blob: vec![2],
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
            grant_revoke: None,
        };
        let bytes_none = encode_deposit_payload(&without).expect("encode none");
        assert_eq!(
            decode_deposit_payload(&bytes_none).expect("decode none"),
            without
        );

        // (c) A LEGACY payload (encoded by a struct WITHOUT invite_packet) decodes
        //     to invite_packet: None — proving forward-compat for old senders.
        #[derive(serde::Serialize)]
        struct LegacyDepositPayload {
            #[serde(rename = "cn", with = "serde_bytes")]
            cidnotify_packet: Vec<u8>,
            #[serde(rename = "pl", with = "serde_bytes")]
            storage_blob: Vec<u8>,
        }
        let legacy = LegacyDepositPayload {
            cidnotify_packet: vec![1],
            storage_blob: vec![2],
        };
        // Encode via ciborium directly (byte-identical to
        // `canonical_cbor_encode`, which this local test struct can't satisfy —
        // the `CanonicalPayload` sealed trait is module-private).
        let mut legacy_bytes = Vec::new();
        ciborium::into_writer(&legacy, &mut legacy_bytes).expect("legacy encode");
        let decoded = decode_deposit_payload(&legacy_bytes).expect("legacy decode");
        assert_eq!(decoded.invite_packet, None);
        assert_eq!(decoded.cidnotify_packet, Some(vec![1]));
    }

    #[test]
    fn deposit_payload_invite_only_cidnotify_none_round_trips() {
        // ZEB-505: a standalone invite-only deposit carries the bootstrap
        // invite ALONE — no CidNotify, no storage blob. `cidnotify_packet: None`
        // must survive the wire round-trip: `skip_serializing_if` omits the `cn`
        // key on encode, and an absent `cn` decodes back to None via `default`.
        let invite_only = DepositPayload {
            cidnotify_packet: None,
            storage_blob: Vec::new(),
            invite_packet: Some(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            revocation_push: None,
            grant_push: None,
            grant_revoke: None,
        };
        let bytes = encode_deposit_payload(&invite_only).expect("encode invite-only");
        assert_eq!(
            decode_deposit_payload(&bytes).expect("decode invite-only"),
            invite_only
        );
    }

    /// ZEB-674 Task 3 (C3): `grant_push` carries the opaque canonical-CBOR
    /// `Vec<serde_bytes Vec<u8>>` of per-device sealed file-grant blobs
    /// (Task 2's `seal_grant_for_devices`) so a grant can ride the
    /// offline-capable butler-deposit rung. (a) `Some(bytes)` round-trips
    /// intact; (b) `None` omits the `gp` key entirely — a legacy/old
    /// decoder that has never heard of `grant_push` decodes the SAME bytes
    /// unaffected (backward compatibility).
    #[test]
    fn deposit_payload_grant_push_roundtrip_and_back_compat() {
        let with_grant = DepositPayload {
            cidnotify_packet: None,
            storage_blob: Vec::new(),
            invite_packet: None,
            revocation_push: None,
            grant_push: Some(vec![0xAA, 0xBB, 0xCC, 0xDD]),
            grant_revoke: None,
        };
        let bytes = encode_deposit_payload(&with_grant).expect("encode grant_push");
        assert_eq!(
            decode_deposit_payload(&bytes).expect("decode grant_push"),
            with_grant
        );

        // `None` omits the `gp` key (skip_serializing_if) — old decoders /
        // old-shaped payloads are unaffected by this new field.
        let without_grant = DepositPayload {
            cidnotify_packet: Some(vec![1]),
            storage_blob: vec![2],
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
            grant_revoke: None,
        };
        let bytes_none = encode_deposit_payload(&without_grant).expect("encode none");
        assert_eq!(
            decode_deposit_payload(&bytes_none).expect("decode none"),
            without_grant
        );
        assert!(
            !bytes_none.windows(2).any(|w| w == b"gp"),
            "gp key omitted when grant_push is None"
        );
    }

    // ZEB-548 Stage 2: `butler_cannot_open_grant_push` (the ZEB-674 Task-3
    // butler-cannot-open seam test) relocated to
    // `tests/dm/grant_ingest_seam_integration.rs` — it needs the
    // orchestration-tier `file_sharing` sealing, which this spine module's
    // tests may no longer name.

    /// The sealed envelope round-trips under the butler info string, and is
    /// domain-separated from ZEB-249: the same ciphertext MUST NOT open
    /// under the epoch-key-seal info (different HKDF info → different AEAD
    /// key → tag mismatch).
    #[test]
    fn sealed_envelope_round_trips_with_butler_info_string() {
        let (priv_bytes, pub_bytes) = make_x25519_keypair(0x01);
        let plaintext = b"butler deposit payload plaintext";

        let sealed = seal_to_owner_with_info(&pub_bytes, plaintext, BUTLER_DEPOSIT_SEAL_INFO)
            .expect("seal must succeed");
        // Same 32 ‖ 12 ‖ ct+tag envelope layout as ZEB-249.
        assert_eq!(sealed.len(), 32 + 12 + plaintext.len() + 16);

        let opened = open_from_owner_with_info(&priv_bytes, &sealed, BUTLER_DEPOSIT_SEAL_INFO)
            .expect("open with butler info must succeed");
        assert_eq!(opened, plaintext);

        // Cross-domain open MUST fail: ZEB-249's open_from_owner uses the
        // epoch-key-seal info string.
        let err = open_from_owner(&priv_bytes, &sealed)
            .expect_err("butler-sealed envelope must not open under the ZEB-249 info");
        assert!(
            matches!(err, crate::dm_signing::DmSignError::DecryptionFailed),
            "expected DecryptionFailed, got {err:?}"
        );
    }

    /// The signature payload is domain-separated: a different domain prefix
    /// produces different signed bytes, and a signature over the real
    /// payload does not verify against the alternate-domain payload.
    #[test]
    fn sig_payload_is_domain_separated() {
        use ed25519_dalek::{Signer, SigningKey, Verifier};

        let ro = [0x11u8; 16];
        let sb = vec![0xAB; 24];
        let payload = deposit_sig_payload(&ro, &sb);

        // Pin the construction: domain ‖ ro ‖ sealed_blob, in that order.
        assert!(payload.starts_with(BUTLER_DEPOSIT_SIG_DOMAIN));
        assert_eq!(
            &payload[BUTLER_DEPOSIT_SIG_DOMAIN.len()..BUTLER_DEPOSIT_SIG_DOMAIN.len() + 16],
            &ro
        );
        assert_eq!(&payload[BUTLER_DEPOSIT_SIG_DOMAIN.len() + 16..], &sb[..]);

        // Same-length alternate domain → different payload bytes.
        let alt_domain = b"harmony-zeb-999-deposit-sig-v1";
        assert_eq!(alt_domain.len(), BUTLER_DEPOSIT_SIG_DOMAIN.len());
        let mut alt_payload = Vec::new();
        alt_payload.extend_from_slice(alt_domain);
        alt_payload.extend_from_slice(&ro);
        alt_payload.extend_from_slice(&sb);
        assert_ne!(payload, alt_payload);

        // A signature over the domain-separated payload does not verify
        // against the alternate-domain bytes.
        let signing = SigningKey::from_bytes(&[0x42; 32]);
        let sig = signing.sign(&payload);
        assert!(signing.verifying_key().verify(&payload, &sig).is_ok());
        assert!(signing.verifying_key().verify(&alt_payload, &sig).is_err());
    }

    #[tokio::test]
    async fn frame_write_read_round_trips() {
        let body = encode_deposit_frame(&fixture_frame()).expect("encode");
        let mut wire: Vec<u8> = Vec::new();
        write_length_prefixed(&mut wire, &body)
            .await
            .expect("write frame");
        assert_eq!(wire.len(), 4 + body.len());
        assert_eq!(&wire[..4], &(body.len() as u32).to_le_bytes());

        let mut reader = wire.as_slice();
        let read_back = read_length_prefixed(&mut reader).await.expect("read frame");
        assert_eq!(read_back, body);
        assert!(
            reader.is_empty(),
            "frame read must consume exactly the frame"
        );
    }

    /// An oversize length prefix is rejected BEFORE the body is read or
    /// allocated. The reader is given ONLY the 4 prefix bytes — if the
    /// implementation attempted to read the body first it would hit
    /// UnexpectedEof (`Io`), not `FrameOutOfBounds`.
    #[tokio::test]
    async fn frame_read_rejects_oversize_before_body() {
        let oversize = (DEPOSIT_MAX_FRAME_BYTES as u32) + 1;
        let prefix_only = oversize.to_le_bytes().to_vec();
        let mut reader = prefix_only.as_slice();
        let err = read_length_prefixed(&mut reader)
            .await
            .expect_err("oversize prefix must be rejected");
        assert!(
            matches!(
                err,
                DepositWireError::FrameOutOfBounds { len, max }
                    if len == oversize as usize && max == DEPOSIT_MAX_FRAME_BYTES
            ),
            "expected FrameOutOfBounds (rejected before body read), got {err:?}"
        );

        // Zero-length prefix is likewise rejected before any body handling.
        let zero_prefix = 0u32.to_le_bytes().to_vec();
        let mut reader = zero_prefix.as_slice();
        let err = read_length_prefixed(&mut reader)
            .await
            .expect_err("zero-length prefix must be rejected");
        assert!(matches!(
            err,
            DepositWireError::FrameOutOfBounds { len: 0, .. }
        ));

        // Exactly-at-cap is accepted (boundary pin) — body present.
        let mut at_cap_wire = (DEPOSIT_MAX_FRAME_BYTES as u32).to_le_bytes().to_vec();
        at_cap_wire.extend(std::iter::repeat_n(0xAA, DEPOSIT_MAX_FRAME_BYTES));
        let mut reader = at_cap_wire.as_slice();
        let body = read_length_prefixed(&mut reader)
            .await
            .expect("at-cap frame must be accepted");
        assert_eq!(body.len(), DEPOSIT_MAX_FRAME_BYTES);
    }

    /// Task 3 (Decision 3): durable-CRDT butler-sets are window-exempt — their
    /// vk seal-targets stay valid even when the recipient's primary device is
    /// long-offline — while pkarr-live butler-sets keep the freshness window.
    #[test]
    fn durable_source_butler_set_survives_past_freshness_window() {
        use crate::reachability_resolver::ReachabilitySource;
        let now = 100 * BUTLER_SET_FRESHNESS_MS; // far past the window
        let stale = ReachabilityAnnouncePayload {
            iroh_node_id: [1; 32],
            home_relay_url: "r".into(),
            direct_addresses: vec![],
            announced_at_ms: 1,
            identity_signature: [0; 64],
            butler_set: vec![crate::reachability_record::ButlerSetEntry {
                device_id: [9; 16],
                iroh_endpoint_id: [8; 32],
                device_ed25519_verify: [7; 32],
                home_relay: "r".into(),
                pinned: false,
            }],
            bs_at: 1, // ancient stamp
        };
        // DurableCrdt: window-exempt -> returns the set.
        let durable =
            freshest_butler_set_by_source(&[(stale.clone(), ReachabilitySource::DurableCrdt)], now);
        assert_eq!(
            durable.len(),
            1,
            "durable butler-set must survive the window"
        );
        // PkarrLive: windowed -> filtered to empty.
        let live = freshest_butler_set_by_source(&[(stale, ReachabilitySource::PkarrLive)], now);
        assert!(
            live.is_empty(),
            "pkarr butler-set past the window must be empty"
        );
    }

    /// Task 4: the deposit path resolves a recipient whose butler-set lives in
    /// the durable community CRDT even when its `bs_at` is far past the pkarr
    /// freshness window. Pinned at the resolver seam the deposit client uses:
    /// `resolve_async_with_source` (cache hit after `update`, so no live pkarr
    /// fallback) → `freshest_butler_set_by_source` returns the durable set.
    #[tokio::test]
    async fn butler_deposit_resolves_durable_butler_set_past_window() {
        use crate::owner_state_types::Hlc;
        use crate::reachability_resolver::{ReachabilityResolver, ReachabilitySource};
        let resolver = ReachabilityResolver::new();
        let addr = OwnerAddr([0x5; 16]);
        let payload = ReachabilityAnnouncePayload {
            iroh_node_id: [0xA; 32],
            home_relay_url: "https://derp.example/".into(),
            direct_addresses: vec![],
            announced_at_ms: 1,
            identity_signature: [0; 64],
            butler_set: vec![ButlerSetEntry {
                device_id: [9; 16],
                iroh_endpoint_id: [8; 32],
                device_ed25519_verify: [7; 32],
                home_relay: "https://derp.example/".into(),
                pinned: false,
            }],
            bs_at: 1, // ancient stamp — far before `now`
        };
        // `update` (not `update_with_source`) tags it `DurableCrdt`.
        resolver.update(
            addr,
            payload,
            Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "x".into(),
            },
        );
        let now = 100 * BUTLER_SET_FRESHNESS_MS; // far past the window
        let tagged = resolver.resolve_async_with_source(&addr).await;
        assert!(
            matches!(tagged[0].1, ReachabilitySource::DurableCrdt),
            "cache-hit entry keeps its DurableCrdt source"
        );
        let set = freshest_butler_set_by_source(&tagged, now);
        assert_eq!(set.len(), 1, "durable butler-set resolves past the window");
    }

    /// Task 4 (reviewer-flagged gap): the mixed durable+pkarr selection path.
    /// Both sources are non-empty, but the durable one is past-window and the
    /// pkarr one is fresh. `freshest_butler_set_by_source` must (a) return a
    /// non-empty set and (b) select by newest `bs_at` AMONG the entries that
    /// survive their own source's freshness policy — so the durable past-window
    /// set, despite a smaller `bs_at`, is the only surviving candidate when the
    /// pkarr entry's `bs_at` is *higher but still windowed-in*, and vice versa.
    #[test]
    fn freshest_butler_set_by_source_mixed_picks_newest_surviving() {
        use crate::reachability_resolver::ReachabilitySource;
        let now = 100 * BUTLER_SET_FRESHNESS_MS;
        let blob = |bs_at: u64, seed: u8| ReachabilityAnnouncePayload {
            iroh_node_id: [seed; 32],
            home_relay_url: "r".into(),
            direct_addresses: vec![],
            announced_at_ms: bs_at,
            identity_signature: [0; 64],
            butler_set: vec![ButlerSetEntry {
                device_id: [seed; 16],
                iroh_endpoint_id: [seed; 32],
                device_ed25519_verify: [seed; 32],
                home_relay: "r".into(),
                pinned: false,
            }],
            bs_at,
        };
        // Durable entry: ancient stamp, window-exempt → survives.
        let durable = blob(1, 0xDD);
        // Pkarr entry: fresh (within window of `now`) → survives.
        let fresh_pkarr = blob(now - 1_000, 0xEE);

        // Both survive; the fresh pkarr entry has the strictly newer `bs_at`,
        // so it wins the `max_by_key(bs_at)` selection.
        let picked = freshest_butler_set_by_source(
            &[
                (durable.clone(), ReachabilitySource::DurableCrdt),
                (fresh_pkarr, ReachabilitySource::PkarrLive),
            ],
            now,
        );
        assert_eq!(picked.len(), 1, "mixed-source must yield a non-empty set");
        assert_eq!(
            picked[0].device_id, [0xEE; 16],
            "newer surviving bs_at wins (the fresh pkarr entry)"
        );

        // Now make the pkarr entry past-window (filtered to empty for its
        // source) while the durable one stays exempt: the durable set wins
        // even though its `bs_at` is the smaller of the two.
        let stale_pkarr = blob(2, 0xEE); // higher bs_at than durable, but past window
        let picked = freshest_butler_set_by_source(
            &[
                (durable, ReachabilitySource::DurableCrdt),
                (stale_pkarr, ReachabilitySource::PkarrLive),
            ],
            now,
        );
        assert_eq!(
            picked.len(),
            1,
            "durable survives when the higher-bs_at pkarr entry is past-window"
        );
        assert_eq!(
            picked[0].device_id, [0xDD; 16],
            "durable set is the only surviving candidate"
        );
    }

    #[tokio::test]
    async fn frame_write_rejects_oversize() {
        let body = vec![0u8; DEPOSIT_MAX_FRAME_BYTES + 1];
        let mut wire: Vec<u8> = Vec::new();
        let err = write_length_prefixed(&mut wire, &body)
            .await
            .expect_err("oversize body must be rejected");
        assert!(matches!(err, DepositWireError::FrameOutOfBounds { .. }));
        assert!(
            wire.is_empty(),
            "nothing may be written for a rejected frame"
        );
    }

    /// The `_with_max` helpers honour a caller-supplied cap distinct from
    /// `DEPOSIT_MAX_FRAME_BYTES`: a body BETWEEN the default and the larger cap
    /// is rejected by the default helper but round-trips under the larger cap,
    /// and an over-the-larger-cap body is still rejected (before any write /
    /// before the body read).
    #[tokio::test]
    async fn with_max_helpers_honour_a_larger_cap() {
        let larger = DEPOSIT_MAX_FRAME_BYTES * 4;
        // A body bigger than the default cap but within the larger cap.
        let body = vec![0x5Au8; DEPOSIT_MAX_FRAME_BYTES + 1];

        // Default helper rejects it (regression guard for the existing callers).
        let mut wire: Vec<u8> = Vec::new();
        assert!(matches!(
            write_length_prefixed(&mut wire, &body).await,
            Err(DepositWireError::FrameOutOfBounds { .. })
        ));
        assert!(wire.is_empty());

        // Under the larger cap it round-trips intact.
        let mut wire: Vec<u8> = Vec::new();
        write_length_prefixed_with_max(&mut wire, &body, larger)
            .await
            .expect("body within larger cap must write");
        let mut reader = wire.as_slice();
        let got = read_length_prefixed_with_max(&mut reader, larger)
            .await
            .expect("body within larger cap must read back");
        assert_eq!(got, body, "round-trips byte-identical under the larger cap");

        // A prefix exceeding the larger cap is rejected before the body read.
        let oversize = (larger as u32) + 1;
        let prefix_bytes = oversize.to_le_bytes();
        let mut prefix_only = prefix_bytes.as_slice();
        let err = read_length_prefixed_with_max(&mut prefix_only, larger)
            .await
            .expect_err("over-larger-cap prefix must be rejected before body read");
        assert!(matches!(
            err,
            DepositWireError::FrameOutOfBounds { len, max }
                if len == oversize as usize && max == larger
        ));
    }
}
