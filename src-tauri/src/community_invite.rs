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
/// Wire format: 4-key map. Field codes 2 chars per the same-length-
/// keys rule.
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
