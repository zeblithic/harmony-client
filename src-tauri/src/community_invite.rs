//! Community invite payload types — ZEB-217 Sub-C Phase 1.
//!
//! Phase 1 ships ONLY the type definitions + canonical CBOR. Encoding
//! to a `harmony://invite/...` URL (base64url + URL prefix) lives in
//! Phase 3 alongside the `generate_invite` IPC. Reticulum send/receive
//! for invite-only counter-sig flow lives in Phase 4.
//!
//! See `docs/specs/2026-05-05-zeb-217-sub-c-communities-design.md`
//! §"Invite system".

use serde::{Deserialize, Serialize};

use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::{
    deserialize_bytes_from_bstr, serialize_bytes_as_bstr, Hlc, MembershipKey, OwnerAddr, SpaceId,
};

/// The full payload an invite link carries. Encoded as canonical CBOR
/// (~120-180 bytes), then base64url-encoded into the URL form
/// `harmony://invite/{base64url}` (encoding helpers land in Phase 3).
///
/// Wire format: 7-key map. Field codes are 2 chars to satisfy the
/// same-length-keys CBOR invariant at this nesting level. Optional
/// fields use skip_serializing_if so non-applicable variants
/// (e.g., open communities have invite_token=None) don't bloat the
/// encoded URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityInvitePayload {
    #[serde(rename = "ci")]
    pub community_id: SpaceId,

    #[serde(rename = "mk")]
    pub membership_key: MembershipKey,

    #[serde(rename = "ad")]
    pub admin_addr: OwnerAddr,

    #[serde(rename = "nm")]
    pub community_name: String,

    #[serde(rename = "io")]
    pub is_invite_only: bool,

    #[serde(rename = "ex", skip_serializing_if = "Option::is_none", default)]
    pub expires_at: Option<Hlc>,

    /// Required for invite-only redemption (carries the inviter's
    /// pre-signed authorization). Optional for open communities (could
    /// still be present as an authenticity hint, but not required).
    #[serde(rename = "tk", skip_serializing_if = "Option::is_none", default)]
    pub invite_token: Option<InviteToken>,
}

/// The inviter's pre-signed authorization, embedded in the invite link
/// for invite-only communities. The redeemer presents this via
/// Reticulum to any community member with `power ≥ invite_threshold`,
/// who counter-signs the resulting Join event (Phase 4).
///
/// `sig` covers the canonical-CBOR encoding of `(inviter, invitee_hint,
/// minted_at, expires_at_in_outer_payload)` — bound to the outer
/// CommunityInvitePayload's expires_at so a token can't be replayed
/// past its outer expiry. (Sig construction lives in Phase 3 with
/// `generate_invite`.)
///
/// Wire format: up to 5-key map (`xa` and `ih` skipped when `None`).
/// Field codes 2 chars per the same-length-keys rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteToken {
    #[serde(rename = "iv")]
    pub inviter: OwnerAddr,

    /// `None` = open redemption (anyone with the link can use this
    /// token). `Some(addr)` = bound to that owner addr; the joiner's
    /// signed Join.actor MUST equal this hint or verification rejects.
    #[serde(rename = "ih", skip_serializing_if = "Option::is_none", default)]
    pub invitee_hint: Option<OwnerAddr>,

    #[serde(rename = "mt")]
    pub minted_at: Hlc,

    /// Wall-clock ms past which the receiver MUST reject this token.
    /// `None` = no expiry (open-ended). Bound into the InviteToken
    /// signature via `canonical_invite_token_bytes` so the inviter's
    /// signature commits to the expiry value — an attacker can't strip
    /// `expires_at` post-mint to extend the redemption window.
    /// (Spec §verify-step-h.) Per the spec, `verify_packet_pure` rejects
    /// when `signed.created_at.wall_ms >= expires_at`.
    #[serde(rename = "xa", skip_serializing_if = "Option::is_none", default)]
    pub expires_at: Option<u64>,

    #[serde(
        rename = "sg",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],
}

impl CanonicalPayloadSealed for CommunityInvitePayload {}
impl CanonicalPayload for CommunityInvitePayload {}
impl CanonicalPayloadSealed for InviteToken {}
impl CanonicalPayload for InviteToken {}

use crate::community_membership::SignedMembershipEvent;
use crate::owner_state_types::DeviceIdentityHash;

/// ZEB-262 Phase 4: Reticulum unicast packet body sent from joiner →
/// counter-signer. Mirrors `dm_envelope::DmInviteSigned`'s Path B app-
/// sig binding shape: the signing_device_hash is INSIDE the signed body
/// so an attacker can't swap which device claims authorship without
/// invalidating the signature, and joiner_identity_pub rides along
/// inline because the receiver doesn't yet have an OwnerDeviceCache
/// entry for the joiner (bootstrap-only).
///
/// Wire format: 6-key map. Field codes are 2 chars to satisfy the
/// same-length-keys CBOR invariant at this nesting level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityInviteSigned {
    /// The community being joined.
    #[serde(rename = "ci")]
    pub community_id: SpaceId,

    /// The joiner's signed Join event WITHOUT countersig. Counter-sig
    /// is applied by the receiver (after verification) via
    /// `community_membership::attach_countersig_with_identity`.
    #[serde(rename = "je")]
    pub join_event: SignedMembershipEvent,

    /// The InviteToken from the URL payload — proves the inviter
    /// authorized this redemption.
    #[serde(rename = "it")]
    pub invite_token: InviteToken,

    /// Joiner's full 64-byte identity public bytes
    /// (`X25519_pub(32) || Ed25519_pub(32)` per
    /// `harmony_identity::Identity::to_public_bytes()`). Bootstrap-only
    /// — receiver doesn't yet have an OwnerDeviceCache entry for the
    /// joiner. Mirrors DmInviteSigned.inviter_identity_pub. Wire form:
    /// CBOR bstr(64).
    #[serde(
        rename = "ip",
        serialize_with = "serialize_identity_pub_as_bstr",
        deserialize_with = "deserialize_identity_pub_from_bstr"
    )]
    pub joiner_identity_pub: [u8; 64],

    /// Joiner's DeviceIdentityHash. Receiver verifies that
    /// SHA256(joiner_identity_pub)[..16] == signing_device_hash.0
    /// (defense-in-depth against a buggy sender pairing pubs with the
    /// wrong device claim). Mirrors DmInvite's signing_device_hash.
    #[serde(rename = "dh")]
    pub signing_device_hash: DeviceIdentityHash,

    /// Wall-clock at packet creation. Used for staleness checks against
    /// `invite_token` (carried via outer `InviteToken.minted_at` and
    /// the outer `CommunityInvitePayload.expires_at`). Also used for
    /// clock-skew rejection (created_at.wall_ms > now + 60s).
    #[serde(rename = "ca")]
    pub created_at: Hlc,
}

impl CanonicalPayloadSealed for CommunityInviteSigned {}
impl CanonicalPayload for CommunityInviteSigned {}

/// ZEB-262 Phase 4: Path B app-sig wrapper around CommunityInviteSigned.
/// Wire layout: `[u8 disc=0x10][CBOR(signed)][64 raw signature bytes]`.
/// The signature is 64 raw bytes appended after the CBOR body — same
/// pattern as `DmPacket` (NOT a CBOR bstr; encode appends via
/// `extend_from_slice`, decode splits via `split_at(len - 64)`).
///
/// Discriminant 0x10 is reserved for community packets per the spec
/// §"Wire format" (DM packets occupy 0x01-0x03; 0x10-0x1F reserved for
/// community packets; 0x20+ reserved for Sub-D directory packets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommunityInvitePacket {
    Invite {
        signed: CommunityInviteSigned,
        signature: [u8; 64],
        /// Captured at decode for re-verify. The signature covers
        /// `signed_bytes` exactly as transmitted, so signature
        /// verification operates on bit-exact bytes regardless of
        /// encoder drift. On send, encode_packet re-encodes from
        /// `signed`, asserts byte-equality with `signed_bytes`, and
        /// emits `signed_bytes` verbatim.
        signed_bytes: Vec<u8>,
    },
}

/// Helper: serialize `[u8; 64]` as CBOR bstr (major type 2). Mirrors
/// dm_envelope::serialize_identity_pub_as_bstr — necessary because
/// serde's blanket `[T; N]: Serialize` only covers small N.
fn serialize_identity_pub_as_bstr<S>(b: &[u8; 64], s: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    s.serialize_bytes(b)
}

/// Helper: deserialize CBOR bstr(64) into `[u8; 64]`. Length is
/// enforced strictly; bstr of any length other than 64 is rejected.
/// Mirrors dm_envelope::deserialize_identity_pub_from_bstr.
fn deserialize_identity_pub_from_bstr<'de, D>(d: D) -> Result<[u8; 64], D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{Error, Visitor};
    use std::fmt;

    struct BytesVisitor;
    impl<'de> Visitor<'de> for BytesVisitor {
        type Value = [u8; 64];

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "a 64-byte CBOR byte string")
        }

        fn visit_bytes<E: Error>(self, value: &[u8]) -> Result<[u8; 64], E> {
            if value.len() != 64 {
                return Err(E::custom(format!(
                    "joiner_identity_pub must be 64 bytes, got {}",
                    value.len()
                )));
            }
            let mut out = [0u8; 64];
            out.copy_from_slice(value);
            Ok(out)
        }

        fn visit_byte_buf<E: Error>(self, v: Vec<u8>) -> Result<[u8; 64], E> {
            self.visit_bytes(&v)
        }
    }

    d.deserialize_bytes(BytesVisitor)
}

use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use base64::Engine;

const URL_PREFIX: &str = "harmony://invite/";

/// Errors decoding a `harmony://invite/...` URL into a
/// `CommunityInvitePayload`. Distinct variants per failure class so the
/// IPC layer can surface a precise diagnostic to the frontend (and a
/// future telemetry dashboard can tally each independently).
#[derive(thiserror::Error, Debug)]
pub enum InviteUrlError {
    #[error("invite URL scheme must be `harmony://invite/`, got `{0}`")]
    WrongScheme(String),
    #[error("base64url decode failed: {0}")]
    Base64(String),
    #[error("CBOR decode failed: {0}")]
    Cbor(String),
    /// Defends the CBOR decoder against unbounded input: a hostile
    /// paste of a multi-MB body would otherwise burn allocator + decode
    /// time before failing. A real invite is ~120-180 bytes; the cap is
    /// generous enough to absorb future field growth without becoming
    /// a DoS vector. Measured in base64 characters of the body
    /// (post-`harmony://invite/` strip), NOT decoded bytes — 4096
    /// base64 chars decode to ~3072 raw bytes.
    #[error("invite payload exceeds 4096 base64-char limit (got {0} chars)")]
    TooLarge(usize),
}

/// Hard cap on the base64url body length (post-prefix-strip, in base64
/// chars) we'll hand to the base64 + CBOR decoders. 4096 base64 chars
/// decode to ≈3072 raw bytes — well above the expected ~180-byte
/// payload (community_id + membership_key + admin_addr + name + flags).
/// Greptile P2 on PR #87 round 2 flagged that the prior name "BYTES"
/// misled. See `InviteUrlError::TooLarge`.
const MAX_INVITE_BODY_B64_CHARS: usize = 4096;

/// Canonical-CBOR-encode the payload, then base64url-no-pad the result,
/// and prefix `harmony://invite/`. The output is copy-paste-safe across
/// chat / email / messaging clients that munge `+`, `/`, or `=`.
pub fn encode_invite_url(payload: &CommunityInvitePayload) -> Result<String, InviteUrlError> {
    let cbor = canonical_cbor_encode(payload).map_err(|e| InviteUrlError::Cbor(e.to_string()))?;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&cbor);
    Ok(format!("{URL_PREFIX}{b64}"))
}

/// Strip the `harmony://invite/` prefix, base64url-decode, then
/// canonical-CBOR-decode into a `CommunityInvitePayload`.
///
/// Trims surrounding whitespace before scheme inspection — paste flows
/// (chat / email / messenger clients) routinely add leading or trailing
/// whitespace, and `harmony://invite/...\n` would otherwise fail with
/// `WrongScheme` for the trailing newline alone.
///
/// Caps the post-prefix body length at `MAX_INVITE_BODY_B64_CHARS`
/// (measured in base64 characters, not decoded bytes) to bound the work
/// the base64 + CBOR decoders do on untrusted input.
pub fn decode_invite_url(url: &str) -> Result<CommunityInvitePayload, InviteUrlError> {
    let url = url.trim();
    let body = url
        .strip_prefix(URL_PREFIX)
        .ok_or_else(|| InviteUrlError::WrongScheme(url.chars().take(URL_PREFIX.len()).collect()))?;
    if body.len() > MAX_INVITE_BODY_B64_CHARS {
        return Err(InviteUrlError::TooLarge(body.len()));
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body)
        .map_err(|e| InviteUrlError::Base64(e.to_string()))?;
    canonical_cbor_decode::<CommunityInvitePayload>(&bytes)
        .map_err(|e| InviteUrlError::Cbor(e.to_string()))
}

// =====================================================================
// ZEB-262 Phase 4 — packet codec + envelope-sig verify
//
// Mirrors `dm_envelope::encode_packet` / `decode_packet` /
// `build_signed_invite` exactly (see src-tauri/src/dm_envelope.rs:262-492).
// Wire layout: `[u8 disc=0x10][CBOR(signed)][64 raw signature bytes]`.
// =====================================================================

/// Errors produced by [`encode_packet`] / [`build_signed_invite_packet`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommunityInviteEncodeError {
    #[error("CBOR encode failed: {0}")]
    Cbor(String),
    /// Re-encoding `signed` to canonical CBOR failed inside encode_packet.
    /// build_signed_invite_packet already round-tripped this value through
    /// the same encoder, so this should be unreachable in practice — surface
    /// as a clear distinct variant so a regression here doesn't mask as a
    /// generic Cbor encode failure.
    #[error("re-encode signed body failed: {0}")]
    ReSerialize(String),
    /// encode_packet re-encoded `signed` and the result diverged from the
    /// cached `signed_bytes` field — the only way this fires is post-build
    /// mutation of the `signed` field. Mirrors
    /// `dm_envelope::EncodeError::SignedMutated`.
    #[error("signed body mutated post-build: {0}")]
    SignedMutated(String),
}

/// Errors produced by [`decode_packet`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommunityInviteDecodeError {
    #[error("packet is empty")]
    Empty,
    #[error("packet too short for [disc + body + 64-byte signature] layout")]
    TooShortForSignature,
    #[error("unknown discriminant byte 0x{0:02x}")]
    UnknownDiscriminant(u8),
    #[error("CBOR decode failed: {0}")]
    Cbor(String),
    #[error("trailing bytes after CBOR body: consumed {consumed} of {total}")]
    TrailingBytes { consumed: u64, total: u64 },
    #[error("payload invariant violated: {0}")]
    Invalid(&'static str),
}

/// ZEB-262 Phase 4 receive-side rejection variants. Each maps to a
/// `community-state-sync-degraded` reason tag for the frontend banner.
///
/// Membership-state-dependent variants (`CommunityUnknown`,
/// `SelfNotJoined`, `SelfPowerInsufficient`) are defined here but
/// raised by `handle_unicast` in Task 9 — they require engine state
/// that isn't in scope for `verify_packet_pure`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommunityInviteVerifyError {
    /// Path B envelope sig didn't validate.
    #[error("envelope sig invalid")]
    EnvelopeSigInvalid,
    /// signing_device_hash != SHA256(joiner_identity_pub)[..16]. Caught
    /// at decode time but surfaced through this error type when the
    /// caller wants the unified reason tag.
    #[error("device hash mismatch")]
    DeviceHashMismatch,
    /// Inner Join event sig failed.
    #[error("Join event sig invalid")]
    JoinSigInvalid,
    /// InviteToken sig failed.
    #[error("InviteToken sig invalid")]
    InviteTokenSigInvalid,
    /// InviteToken.inviter != self_owner. v1 only counter-signs invites
    /// we issued. ZEB-251 broadens this to any joined member with
    /// power ≥ invite_threshold.
    #[error("invite signer mismatch: token says {signer:?}, we are {self_owner:?}")]
    InviteSignerMismatch {
        signer: crate::owner_state_types::OwnerAddr,
        self_owner: crate::owner_state_types::OwnerAddr,
    },
    /// community_id disagreement across envelope, Join, and token.
    #[error("community_id mismatch across envelope/Join/token")]
    CommunityIdMismatch,
    /// created_at >= invite_token expires_at, OR created_at > now + 60s.
    #[error("invite expired or clock-skew rejected")]
    Expired,
    /// invite_token.invitee_hint set and != join_event.actor.
    #[error("invitee_hint mismatch")]
    InviteeHintMismatch,
    /// No engine for this community — packet was misrouted. Receiver
    /// surface; not raised by `verify_packet_pure` (engine state isn't
    /// in scope there).
    #[error("community unknown: {community_id:?}")]
    CommunityUnknown {
        community_id: crate::owner_state_types::SpaceId,
    },
    /// Self isn't currently a Joined member. Receiver surface; engine-
    /// coupled.
    #[error("self not joined in community")]
    SelfNotJoined,
    /// Self power < invite_threshold (= 0 in v1, structural no-op).
    #[error("self power insufficient: {self_power} < {threshold}")]
    SelfPowerInsufficient { self_power: u8, threshold: u8 },
    /// `community_membership::attach_countersig_with_identity` failed
    /// (canonical-CBOR encoder error). Vanishingly rare in practice;
    /// distinct from JoinSigInvalid so degraded telemetry can
    /// distinguish a malformed inner Join from a counter-sign encoder
    /// regression on the receiver side.
    #[error("counter-sign attach failed")]
    CounterSignAttachFailed,
    /// Engine-side CRDT verify rejected the counter-signed Join
    /// (`InsertOutcome::Rejected`). Distinct from JoinSigInvalid: the
    /// inner Join sig already validated in step 5 of `verify_packet_pure`,
    /// but the engine's own VerifyContext (admin / invite-only /
    /// expected_community_id) saw something unexpected.
    #[error("engine rejected counter-signed Join")]
    EngineRejected,
    /// `insert_local_event_with_pubs` returned a `LocalInsertError`
    /// (resolver missing, wrong community on the inner event, etc.).
    /// Surfaced separately so the degraded reason tag points at the
    /// engine's local-insert pipeline rather than at sig classes.
    #[error("engine local-insert error")]
    EngineLocalError,
}

impl CommunityInviteVerifyError {
    /// Reason tag for the `community-state-sync-degraded` Tauri event.
    pub fn reason_tag(&self) -> &'static str {
        match self {
            Self::EnvelopeSigInvalid => "community_invite_envelope_sig_invalid",
            Self::DeviceHashMismatch => "community_invite_device_hash_mismatch",
            Self::JoinSigInvalid => "community_invite_join_sig_invalid",
            Self::InviteTokenSigInvalid => "community_invite_token_sig_invalid",
            Self::InviteSignerMismatch { .. } => "community_invite_signer_mismatch",
            Self::CommunityIdMismatch => "community_invite_id_mismatch",
            Self::Expired => "community_invite_expired",
            Self::InviteeHintMismatch => "community_invitee_hint_mismatch",
            Self::CommunityUnknown { .. } => "community_invite_unknown",
            Self::SelfNotJoined => "community_invite_self_not_joined",
            Self::SelfPowerInsufficient { .. } => "community_invite_self_power_insufficient",
            Self::CounterSignAttachFailed => "community_invite_counter_sign_attach_failed",
            Self::EngineRejected => "community_invite_engine_rejected",
            Self::EngineLocalError => "community_invite_engine_local_error",
        }
    }
}

/// Encode a [`CommunityInvitePacket`] to wire bytes.
///
/// **Mutation guard.** Re-encodes `signed` and asserts byte-equality
/// with the cached `signed_bytes` (which was the source for `signature`
/// at build time); mismatch returns `SignedMutated`. The only way this
/// fires is post-build mutation of `signed`; no in-crate code path does
/// this, but the guard catches future regressions cheaply with a memcmp.
///
/// On success the function emits the cached `signed_bytes` verbatim
/// (NOT the freshly re-encoded bytes), preserving byte-exactness on
/// decode→encode round trips. Mirrors [`crate::dm_envelope::encode_packet`].
pub fn encode_packet(
    packet: &CommunityInvitePacket,
) -> Result<Vec<u8>, CommunityInviteEncodeError> {
    match packet {
        CommunityInvitePacket::Invite {
            signed,
            signature,
            signed_bytes,
        } => {
            let re_encoded = canonical_cbor_encode(signed)
                .map_err(|e| CommunityInviteEncodeError::ReSerialize(format!("re-encode: {e}")))?;
            if re_encoded != *signed_bytes {
                return Err(CommunityInviteEncodeError::SignedMutated(
                    "CommunityInvitePacket::Invite: signed mutated post-build (re-encode \
                     mismatches cached signed_bytes; signature would not cover wire body)"
                        .into(),
                ));
            }
            let mut out = Vec::with_capacity(1 + signed_bytes.len() + 64);
            out.push(0x10);
            out.extend_from_slice(signed_bytes);
            out.extend_from_slice(signature);
            Ok(out)
        }
    }
}

/// Decode wire bytes into a [`CommunityInvitePacket`]. Captures
/// `signed_bytes` exactly as transmitted so envelope-sig verify
/// operates on bit-exact bytes regardless of encoder drift.
///
/// Rejects: unknown discriminants, trailing bytes after the CBOR body,
/// non-canonical encodings (decode → canonical-re-encode mismatch),
/// and `signing_device_hash` not equal to `SHA256(joiner_identity_pub)[..16]`
/// (defense-in-depth before the receive handler runs the Ed25519 verify).
pub fn decode_packet(bytes: &[u8]) -> Result<CommunityInvitePacket, CommunityInviteDecodeError> {
    let (disc, rest) = bytes
        .split_first()
        .ok_or(CommunityInviteDecodeError::Empty)?;
    if rest.len() < 64 + 1 {
        return Err(CommunityInviteDecodeError::TooShortForSignature);
    }
    let split_at = rest.len() - 64;
    let (body_bytes, signature_bytes) = rest.split_at(split_at);
    let signature: [u8; 64] = signature_bytes
        .try_into()
        .expect("just split at len-64; signature_bytes is exactly 64 bytes");
    let signed_bytes = body_bytes.to_vec();
    match disc {
        0x10 => {
            let mut cursor = std::io::Cursor::new(body_bytes);
            let signed: CommunityInviteSigned = ciborium::from_reader(&mut cursor)
                .map_err(|e| CommunityInviteDecodeError::Cbor(e.to_string()))?;
            let consumed = cursor.position();
            if consumed as usize != body_bytes.len() {
                return Err(CommunityInviteDecodeError::TrailingBytes {
                    consumed,
                    total: body_bytes.len() as u64,
                });
            }
            // Canonical-encoding round-trip check: re-encode and reject
            // if the re-encoded bytes differ from body_bytes. Catches
            // reordered map keys, indefinite-length encodings, oversized
            // length prefixes — anything where decode → canonical-re-
            // encode is not byte-identical. Mirrors
            // dm_envelope::ensure_canonical_body.
            let canonical = canonical_cbor_encode(&signed)
                .map_err(|e| CommunityInviteDecodeError::Cbor(e.to_string()))?;
            if canonical != body_bytes {
                return Err(CommunityInviteDecodeError::Invalid(
                    "CommunityInvitePacket body must use canonical CBOR",
                ));
            }
            // Structural check: signing_device_hash must match
            // SHA256(joiner_identity_pub)[..16]. Not a sig check (no
            // crypto here); cheap defense-in-depth before the sig
            // verifier runs in handle_unicast.
            let derived = device_hash_from_identity_pub(&signed.joiner_identity_pub);
            if derived != signed.signing_device_hash.0 {
                return Err(CommunityInviteDecodeError::Invalid(
                    "CommunityInviteSigned.signing_device_hash must equal \
                     SHA256(joiner_identity_pub)[..16]",
                ));
            }
            Ok(CommunityInvitePacket::Invite {
                signed,
                signature,
                signed_bytes,
            })
        }
        other => Err(CommunityInviteDecodeError::UnknownDiscriminant(*other)),
    }
}

/// Compute `SHA256(identity_pub)[..16]`. Mirrors how `DmInvite` Path B
/// derives `signing_device_hash`; the receiver checks this binding before
/// running the (more expensive) Ed25519 verify.
pub fn device_hash_from_identity_pub(identity_pub: &[u8; 64]) -> [u8; 16] {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(identity_pub);
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// Build a complete [`CommunityInvitePacket`] ready for [`encode_packet`].
/// Encodes `signed` to canonical CBOR, signs the resulting bytes via
/// `signing_key`, bundles into the `Invite` variant. Mirrors
/// [`crate::dm_envelope::build_signed_invite`].
pub fn build_signed_invite_packet(
    signed: CommunityInviteSigned,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<CommunityInvitePacket, CommunityInviteEncodeError> {
    use ed25519_dalek::Signer;
    let signed_bytes = canonical_cbor_encode(&signed)
        .map_err(|e| CommunityInviteEncodeError::Cbor(e.to_string()))?;
    let signature = signing_key.sign(&signed_bytes).to_bytes();
    Ok(CommunityInvitePacket::Invite {
        signed,
        signature,
        signed_bytes,
    })
}

/// Pure verify helper: takes a [`CommunityInviteSigned`], the local self
/// owner addr, a wall-clock function, and the local `PrivateIdentity` for
/// the InviteToken sig check. Returns the joiner's signed Join event on
/// success — caller is then responsible for the engine-coupled checks
/// (community known, self joined, self power sufficient) before
/// counter-signing.
///
/// Order of checks chosen so cheaper / more diagnostic rejections fire
/// before expensive crypto:
///   1. community_id agreement (cheap struct compare)
///   2. invitee_hint match (cheap if hint is None)
///   3. expiry / clock-skew (cheap arithmetic, 60s tolerance)
///   4. InviteToken signer == self (cheap struct compare)
///   5. Inner Join event sig (1× Ed25519 verify_strict via
///      `community_membership::verify_signature`)
///   6. InviteToken sig (1× Ed25519 verify_strict against the canonical
///      token payload)
///
/// Membership-state-dependent checks (`SelfNotJoined`, `CommunityUnknown`,
/// `SelfPowerInsufficient`) are NOT raised here — they require engine
/// state and ship in Task 9's `handle_unicast`.
pub fn verify_packet_pure<F>(
    signed: &CommunityInviteSigned,
    self_owner: crate::owner_state_types::OwnerAddr,
    now_fn: F,
    self_identity: &harmony_identity::PrivateIdentity,
) -> Result<crate::community_membership::SignedMembershipEvent, CommunityInviteVerifyError>
where
    F: FnOnce() -> u64,
{
    // 1. community_id agreement across envelope + Join.
    if signed.community_id != signed.join_event.community_id {
        return Err(CommunityInviteVerifyError::CommunityIdMismatch);
    }
    // (InviteToken doesn't carry community_id directly in v1 — the
    // outer URL payload does. Skip a token vs envelope comparison
    // here; the receive-side engine resolution catches misroutes.)

    // 2. invitee_hint match.
    if let Some(hint) = signed.invite_token.invitee_hint {
        if signed.join_event.actor != hint {
            return Err(CommunityInviteVerifyError::InviteeHintMismatch);
        }
    }

    // 3. Expiry / clock-skew. Three arms:
    //    (a) clock-skew: created_at can't be more than 60s in the
    //        receiver's future (defense against a malicious mint that
    //        backdates `now` to dodge expiry).
    //    (b) expires_at vs created_at (if the inviter set one):
    //        created_at must be strictly before expires_at. The
    //        inviter's signature binds `xa` via
    //        `canonical_invite_token_bytes`, so an attacker cannot
    //        strip the field to extend the window — the InviteToken
    //        sig check in step 6 would fail.
    //    (c) expires_at vs now: a packet whose created_at predated
    //        expires_at can still be replayed AFTER expires_at. Reject
    //        when the receiver's wall clock is at-or-past the token's
    //        expiry. (Greptile / CodeRabbit P1: replay window without
    //        this check is unbounded.)
    let now = now_fn();
    if signed.created_at.wall_ms > now.saturating_add(60_000) {
        return Err(CommunityInviteVerifyError::Expired);
    }
    if let Some(exp) = signed.invite_token.expires_at {
        if signed.created_at.wall_ms >= exp {
            return Err(CommunityInviteVerifyError::Expired);
        }
        if now >= exp {
            return Err(CommunityInviteVerifyError::Expired);
        }
    }

    // 4. InviteToken signer == self.
    if signed.invite_token.inviter != self_owner {
        return Err(CommunityInviteVerifyError::InviteSignerMismatch {
            signer: signed.invite_token.inviter,
            self_owner,
        });
    }

    // 5. Inner Join event sig.
    crate::community_membership::verify_signature(&signed.join_event, &signed.joiner_identity_pub)
        .map_err(|_| CommunityInviteVerifyError::JoinSigInvalid)?;

    // 6. InviteToken sig.
    let token_canonical = canonical_invite_token_bytes(&signed.invite_token)
        .map_err(|_| CommunityInviteVerifyError::InviteTokenSigInvalid)?;
    use ed25519_dalek::Signature;
    let sig = Signature::from_bytes(&signed.invite_token.sig);
    self_identity
        .identity
        .verifying_key
        .verify_strict(&token_canonical, &sig)
        .map_err(|_| CommunityInviteVerifyError::InviteTokenSigInvalid)?;

    Ok(signed.join_event.clone())
}

/// Canonical-CBOR-encode the InviteToken payload (excluding the sig).
/// Both the IPC mint path (Phase 4 `generate_invite` for invite-only —
/// not yet shipped) and the verify path encode through this so signature
/// bytes cover bit-exact bytes.
///
/// Wire format: a 2- to 4-key map with field codes `iv`, `ih`, `mt`,
/// `xa` (mirrors `InviteToken`'s renames; same-length-keys CBOR
/// invariant). `ih` is omitted when `invitee_hint = None`; `xa` is
/// omitted when `expires_at = None`. The InviteToken sig commits to
/// these bytes — the inviter cannot strip `xa` post-sign without
/// invalidating the signature, so the receiver's expiry enforcement
/// in `verify_packet_pure` is bound to the inviter's authorization.
///
/// Public so the test harness can call it; mint path (Phase 4 IPC) will
/// also call this when invite-only `generate_invite` ships, ensuring
/// mint and verify never drift.
pub fn canonical_invite_token_bytes(
    token: &InviteToken,
) -> Result<Vec<u8>, ciborium::ser::Error<std::io::Error>> {
    #[derive(serde::Serialize)]
    struct InviteTokenPayload<'a> {
        #[serde(rename = "iv")]
        inviter: &'a crate::owner_state_types::OwnerAddr,
        #[serde(rename = "ih", skip_serializing_if = "Option::is_none")]
        invitee_hint: Option<&'a crate::owner_state_types::OwnerAddr>,
        #[serde(rename = "mt")]
        minted_at: &'a crate::owner_state_types::Hlc,
        #[serde(rename = "xa", skip_serializing_if = "Option::is_none")]
        expires_at: Option<u64>,
    }
    let payload = InviteTokenPayload {
        inviter: &token.inviter,
        invitee_hint: token.invitee_hint.as_ref(),
        minted_at: &token.minted_at,
        expires_at: token.expires_at,
    };
    let mut out = Vec::new();
    ciborium::into_writer(&payload, &mut out)?;
    Ok(out)
}

/// Verify the Path B envelope signature over the captured `signed_bytes`.
/// Pure crypto check — no membership or expiry semantics. Returns
/// [`CommunityInviteVerifyError::EnvelopeSigInvalid`] on any failure
/// (including malformed `identity_pub`). Used by `handle_unicast`
/// (Task 9) and exercised by the
/// `community_invite_packet_envelope_sig_rejected_on_tampered_body` test.
pub fn verify_envelope_sig(
    signed_bytes: &[u8],
    signature: &[u8; 64],
    identity_pub: &[u8; 64],
) -> Result<(), CommunityInviteVerifyError> {
    use ed25519_dalek::Signature;
    let identity = harmony_identity::Identity::from_public_bytes(identity_pub)
        .map_err(|_| CommunityInviteVerifyError::EnvelopeSigInvalid)?;
    let sig = Signature::from_bytes(signature);
    identity
        .verifying_key
        .verify_strict(signed_bytes, &sig)
        .map_err(|_| CommunityInviteVerifyError::EnvelopeSigInvalid)
}

// =====================================================================
// ZEB-262 Phase 4 Task 9 — receive-side dispatch
// =====================================================================

/// Tiny trait so `handle_unicast` can take either a real
/// `tauri::AppHandle` or a test stub (`None::<&()>`). Production impl on
/// `tauri::AppHandle` lives in `lib.rs` (small adapter that calls
/// `app.emit("community-state-sync-degraded", …)`). Tests typically
/// pass `None`.
pub trait AppHandleEmit {
    /// Emit a `community-state-sync-degraded` Tauri event with the
    /// community id (lowercase hex) and reason tag.
    fn emit_degraded(&self, community_id_hex: &str, reason_tag: &'static str);
}

/// Unit-type impl: tests can pass `None::<&()>` and the trait method is
/// never called in the None path. Provided here so the bound resolves
/// without forcing tests to define their own stub.
impl AppHandleEmit for () {
    fn emit_degraded(&self, _: &str, _: &'static str) {}
}

fn emit_degraded<H: AppHandleEmit>(
    app: Option<&H>,
    community_id: &crate::owner_state_types::SpaceId,
    reason_tag: &'static str,
) {
    if let Some(app) = app {
        app.emit_degraded(&hex::encode(community_id.0), reason_tag);
    } else {
        tracing::warn!(
            community_id = %hex::encode(community_id.0),
            reason = reason_tag,
            "community_invite verify failed (no app handle); not emitting Tauri event"
        );
    }
}

/// ZEB-262 Phase 4 Task 9: receive-side handler for Reticulum unicast
/// packets with discriminant 0x10. Runs the verify chain per spec
/// §"Receive path", attaches the counter-sig via
/// [`crate::community_membership::attach_countersig_with_identity`],
/// inserts the counter-signed Join via `engine.insert_local_event`. The
/// engine's post-Inserted hook (Task 7) fires the joiner-side
/// `pending_redemptions[event_id]` oneshot.
///
/// On any verify failure, emits `community-state-sync-degraded` (when
/// `app` is `Some`) and returns `Err`. No retry — Reticulum retransmit
/// will redrive from the sender if needed.
///
/// `crdt_state` is plumbed through but unused in v1: the receive-side
/// only mutates the per-community CRDT (inside the engine), not the
/// owner-state Space. The arg is kept for future expansion (e.g.,
/// resolving the inviter's devices for ack-back routing in ZEB-251).
#[allow(clippy::too_many_arguments)] // 5 args — clippy default is 7; kept here for symmetry with future expansion.
pub async fn handle_unicast<H: AppHandleEmit>(
    community_registry: &std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
    dm_outbox: &std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>,
    _crdt_state: &std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    packet_bytes: Vec<u8>,
    app: Option<&H>,
) -> Result<(), CommunityInviteVerifyError> {
    // 1. decode_packet — peels the 0x10 discriminant + 64-byte trailer,
    //    canonical-CBOR-checks the inner body, enforces the
    //    SHA256(joiner_identity_pub)[..16] == signing_device_hash bind.
    let packet = match decode_packet(&packet_bytes) {
        Ok(p) => p,
        Err(e) => {
            // Decode failure: caller can't identify a community_id, so
            // there's no community to flag in a degraded event. Drop +
            // warn. Returning a generic envelope-sig variant lets
            // handle_unicast keep a uniform error type without forcing
            // CommunityInviteVerifyError to absorb decode variants.
            tracing::warn!(error = %e, "community_invite decode_packet failed; dropping");
            return Err(CommunityInviteVerifyError::EnvelopeSigInvalid);
        }
    };
    let CommunityInvitePacket::Invite {
        signed,
        signature,
        signed_bytes,
    } = packet;

    // 2. Snapshot self_owner + private_identity from dm_outbox under
    //    its lock; drop the guard before any further `.await`.
    let (self_owner, self_private_identity) = {
        let outbox_g = dm_outbox.lock().await;
        (
            outbox_g.self_owner,
            std::sync::Arc::clone(&outbox_g.private_identity),
        )
    };

    // 3a. Path B envelope sig over signed_bytes (joiner's signature
    //     over the canonical-CBOR body).
    if let Err(e) = verify_envelope_sig(&signed_bytes, &signature, &signed.joiner_identity_pub) {
        emit_degraded(app, &signed.community_id, e.reason_tag());
        return Err(e);
    }
    // 3b. Pure verify chain (community_id agreement, invitee_hint,
    //     expiry/clock-skew, InviteToken signer == self, Join sig,
    //     InviteToken sig).
    let join_event = match verify_packet_pure(
        &signed,
        self_owner,
        || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64
        },
        self_private_identity.as_ref(),
    ) {
        Ok(e) => e,
        Err(e) => {
            emit_degraded(app, &signed.community_id, e.reason_tag());
            return Err(e);
        }
    };

    // 4. Resolve engine + state for community_id.
    let engine_arc = match community_registry.engine_arc(&signed.community_id).await {
        Some(e) => e,
        None => {
            let e = CommunityInviteVerifyError::CommunityUnknown {
                community_id: signed.community_id,
            };
            emit_degraded(app, &signed.community_id, e.reason_tag());
            return Err(e);
        }
    };
    let state_arc = match community_registry.state_for(&signed.community_id).await {
        Some(s) => s,
        None => {
            let e = CommunityInviteVerifyError::CommunityUnknown {
                community_id: signed.community_id,
            };
            emit_degraded(app, &signed.community_id, e.reason_tag());
            return Err(e);
        }
    };

    // 5. Self-eligibility: must be Joined; power ≥ invite_threshold
    //    (= 0 in v1 — structural no-op + stable hook for ZEB-251).
    let (self_status, self_power) = {
        let s = state_arc.lock().await;
        let events: Vec<_> = s.events.values().cloned().collect();
        drop(s);
        let mat = crate::community_membership::materialize(&events, engine_arc.admin_addr());
        let st = mat.members.get(&self_owner).map(|m| m.status);
        let pw = mat.power_levels.get(&self_owner).copied().unwrap_or(0);
        (st, pw)
    };
    if self_status != Some(crate::community_membership::MemberStatus::Joined) {
        let e = CommunityInviteVerifyError::SelfNotJoined;
        emit_degraded(app, &signed.community_id, e.reason_tag());
        return Err(e);
    }
    let invite_threshold: u8 = 0;
    if self_power < invite_threshold {
        let e = CommunityInviteVerifyError::SelfPowerInsufficient {
            self_power,
            threshold: invite_threshold,
        };
        emit_degraded(app, &signed.community_id, e.reason_tag());
        return Err(e);
    }

    // 6. Attach countersig with our identity.
    let counter_signed = match crate::community_membership::attach_countersig_with_identity(
        &join_event,
        self_private_identity.as_ref(),
    ) {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!(error = %err, "attach_countersig_with_identity failed");
            let e = CommunityInviteVerifyError::CounterSignAttachFailed;
            emit_degraded(app, &signed.community_id, e.reason_tag());
            return Err(e);
        }
    };

    // 7. Insert via engine using `insert_local_event_with_pubs` — the
    //    joiner's `joiner_identity_pub` was already verified in
    //    `verify_packet_pure` step 5 (Path B app-sig binding), and the
    //    receiver's own identity_pub is known locally. The production
    //    `OwnerDeviceCacheResolver` won't have the joiner yet (this IS
    //    the bootstrap that would populate the cache), so we MUST
    //    bypass it. Skipping the resolver here is the load-bearing fix
    //    for the bootstrap-by-design case: a counter-signed Join lands
    //    LOCALLY here regardless of whether the resolver knows the
    //    joiner; the publish-back path then carries the full
    //    counter-signed event to peers, who do their own membership-
    //    state verify against their resolver caches as those caches
    //    populate.
    //
    //    The engine's post-Inserted hook
    //    (`notify_pending_redemption_in_map`) fires
    //    `pending_redemptions[event_id]` for the joiner side — this
    //    wakes the redeemer's `redeem_invite_inner` oneshot wait once
    //    the counter-signed Join propagates back via Phase 2's
    //    state-root publish.
    let countersigner_pub = self_private_identity.identity.to_public_bytes();
    match engine_arc
        .insert_local_event_with_pubs(
            counter_signed,
            signed.joiner_identity_pub,
            Some(countersigner_pub),
        )
        .await
    {
        Ok(crate::community_state_crdt::InsertOutcome::Inserted) => Ok(()),
        Ok(crate::community_state_crdt::InsertOutcome::AlreadyKnown) => {
            // Idempotent retransmit (Reticulum can deliver duplicates).
            // Treat as success — we've already counter-signed this id.
            Ok(())
        }
        Ok(crate::community_state_crdt::InsertOutcome::Rejected(verr)) => {
            tracing::warn!(error = ?verr, "counter-signed Join rejected by engine");
            let e = CommunityInviteVerifyError::EngineRejected;
            emit_degraded(app, &signed.community_id, e.reason_tag());
            Err(e)
        }
        Err(local_err) => {
            tracing::warn!(error = %local_err, "engine.insert_local_event_with_pubs errored");
            let e = CommunityInviteVerifyError::EngineLocalError;
            emit_degraded(app, &signed.community_id, e.reason_tag());
            Err(e)
        }
    }
}
