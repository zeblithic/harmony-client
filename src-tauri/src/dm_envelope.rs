//! ZEB-216 Sub-B Phase 1 + Phase 3b: DM wire envelope types + signed
//! discriminant codec.
//!
//! See `docs/specs/2026-05-02-zeb-216-sub-b-dm-transport-design.md`
//! §"Wire format", §"Plaintext envelope (Phase 1, recap from ZEB-219)",
//! and §"Application-signature binding rule".
//!
//! Phase 3b wire layout per Reticulum unicast packet:
//! `[u8 discriminant][CBOR(signed_body)][bstr(64) signature]`
//!
//! The discriminant is routing-only (excluded from the signed bytes),
//! the signature lives outside the CBOR map (avoids a chicken-and-egg
//! computing it inside), and `signed_bytes` is captured on decode so the
//! receive handler can call `dm_signing::verify_dm_packet_signature`
//! without re-encoding.
//!
//! All wire types use two-character serde renames so each struct's keys
//! are the same encoded length at a single nesting level — the same-length-
//! keys precondition documented on `crate::owner_state_crypto::canonical_cbor_encode`.

use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize, Serializer};

use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::{
    deserialize_vec_from_bstr, serialize_vec_as_bstr, ContentId, DeviceIdentityHash, DmContentKey,
    Hlc, OwnerAddr, SpaceId, SpaceKind, MAX_DEVICES_PER_OWNER,
};

/// Plaintext envelope encrypted into the CAS storage_blob. Bound by AAD
/// to the Space's dedupe_key; decrypt enforces (sender, sent_at) authenticity.
/// See ZEB-216 §"Plaintext envelope" / ZEB-219 §"Wire format".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagePayload {
    /// Message body bytes. Encoded as CBOR bstr (major type 2): one
    /// header byte plus the raw bytes. The default `Vec<u8>` derive
    /// would emit a CBOR array of u8 (major type 4) — two bytes per
    /// byte once values exceed 0x17 — roughly doubling ciphertext
    /// overhead. Lock the byte-efficient form in pre-Phase-2 so Phase 2's
    /// receive path doesn't get to silently regress it.
    #[serde(
        rename = "bd",
        serialize_with = "serialize_vec_as_bstr",
        deserialize_with = "deserialize_vec_from_bstr"
    )]
    pub body: Vec<u8>,
    #[serde(rename = "mt")]
    pub mime_type: String,
    #[serde(rename = "se")]
    pub sender: OwnerAddr,
    #[serde(rename = "sa")]
    pub sent_at: Hlc,
}

/// Reticulum-unicast packet announcing a new DM Space and distributing
/// the per-Space content_key. Receiver MUST run the bootstrap sanity
/// gates (ZEB-216 §"Link-origin binding rule") before applying.
///
/// Phase 3b: this is the CBOR-encoded body that the appended Ed25519
/// signature covers (the wire packet is `[disc][CBOR(this)][sig]`).
/// The `Signed` suffix makes that explicit and disambiguates from any
/// pre-Phase-3b callers that might still expect the old struct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmInviteSigned {
    #[serde(rename = "si")]
    pub space_id: SpaceId,
    #[serde(rename = "kn")]
    pub kind: SpaceKind,
    /// Members sorted ascending lex (matches Space::members invariant).
    /// Cannot be used to identify the inviter — see `inviter` field.
    #[serde(rename = "me")]
    pub members: Vec<OwnerAddr>,
    /// Explicit inviter OwnerAddr. Receiver MUST verify
    /// `inviter ∈ members` and `signing_device_hash ∈ sender_devices`
    /// before prompting the user.
    #[serde(rename = "iv")]
    pub inviter: OwnerAddr,
    #[serde(rename = "ck")]
    pub content_key: DmContentKey,
    #[serde(rename = "sd")]
    pub sender_devices: Vec<DeviceIdentityHash>,
    #[serde(rename = "ca")]
    pub created_at: Hlc,
    /// The DeviceIdentityHash of the device that signed this packet.
    /// MUST be in `sender_devices`. Inside the signed body so an attacker
    /// can't swap which device claims authorship without invalidating the
    /// signature.
    #[serde(rename = "dh")]
    pub signing_device_hash: DeviceIdentityHash,
    /// Inviter's full device-Identity public bytes (X25519_pub(32) ||
    /// Ed25519_pub(32), the canonical
    /// `harmony_identity::Identity::to_public_bytes()` layout). Bootstrap-
    /// only on DmInvite — receiver doesn't yet have an OwnerDeviceCache
    /// entry for the inviter, so the inviter ships its pubs inline so
    /// the receiver can verify the signature + reproduce the device hash
    /// (`signing_device_hash = SHA256(X25519 || Ed25519)[:16]` per Task
    /// 3's `dm_signing::derive_device_hash_from_identity_pub`
    /// equivalence test).
    ///
    /// Wire format: bstr(64). Custom `serialize_with`/`deserialize_with`
    /// because serde's blanket `Serialize`/`Deserialize` impl on `[T; N]`
    /// only covers small N and would emit array-of-u8 (~2x overhead) anyway —
    /// mirrors Task 4's `serialize_device_identity_pubs` pattern in
    /// owner_state_types.rs.
    #[serde(
        rename = "sp",
        serialize_with = "serialize_identity_pub_as_bstr",
        deserialize_with = "deserialize_identity_pub_from_bstr"
    )]
    pub inviter_identity_pub: [u8; 64],
}

/// Reticulum-unicast packet notifying recipients that a new encrypted
/// message blob exists at `message_cid` in CAS. `sender_owner_addr` is
/// diagnostic only — receiver MUST resolve the actual sender via
/// link-origin binding (ZEB-216 §"Link-origin binding rule") and
/// signature verification (Path B per ZEB-216 spec §"Application-
/// signature binding rule").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmCidNotifySigned {
    #[serde(rename = "si")]
    pub space_id: SpaceId,
    #[serde(rename = "mc")]
    pub message_cid: ContentId,
    #[serde(rename = "so")]
    pub sender_owner_addr: OwnerAddr,
    #[serde(rename = "sd")]
    pub sender_devices: Vec<DeviceIdentityHash>,
    /// The DeviceIdentityHash of the device that signed this packet.
    /// MUST be in `sender_devices`. No inline `inviter_identity_pub`
    /// here — post-bootstrap, the receiver looks up the pub via
    /// OwnerDeviceCache (which Task 4's device_identity_pubs parallel
    /// vec populates).
    #[serde(rename = "dh")]
    pub signing_device_hash: DeviceIdentityHash,
}

/// Reticulum-unicast packet acknowledging receipt of a DmCidNotify.
/// `ack_from_owner_addr` is diagnostic only — receiver MUST resolve via
/// link-origin binding + signature verification AND verify the resolved
/// owner is in `OutboxEntry.recipient_owners`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmAckSigned {
    #[serde(rename = "si")]
    pub space_id: SpaceId,
    #[serde(rename = "mc")]
    pub message_cid: ContentId,
    #[serde(rename = "ao")]
    pub ack_from_owner_addr: OwnerAddr,
    #[serde(rename = "ad")]
    pub ack_from_devices: Vec<DeviceIdentityHash>,
    /// The DeviceIdentityHash of the device that signed this packet.
    /// MUST be in `ack_from_devices`. Same rationale as
    /// DmCidNotifySigned: post-bootstrap pub lookup goes through
    /// OwnerDeviceCache.
    #[serde(rename = "dh")]
    pub signing_device_hash: DeviceIdentityHash,
}

/// Discriminated union of Reticulum DM packets. Wire layout per packet:
/// `[u8 discriminant][CBOR(signed_body)][bstr(64) signature]` with
/// discriminants 0x01=Invite, 0x02=CidNotify, 0x03=Ack.
///
/// Each variant carries:
/// - `signed`: the typed body the signature covers.
/// - `signature`: the 64-byte Ed25519 signature appended after the body.
/// - `signed_bytes`: the canonical CBOR encoding of `signed` captured by
///   `decode_packet` on the receive path so the receive handler can call
///   `dm_signing::verify_dm_packet_signature` without re-encoding (the
///   captured bytes are exactly what the signature covers, so even if
///   the encoder were to drift this is the bit-exact verification input).
///   On the send path `encode_packet` re-encodes from `signed` rather
///   than trusting this field — see `encode_packet`'s invariant-guard
///   doc comment for rationale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DmPacket {
    Invite {
        signed: DmInviteSigned,
        signature: [u8; 64],
        signed_bytes: Vec<u8>,
    },
    CidNotify {
        signed: DmCidNotifySigned,
        signature: [u8; 64],
        signed_bytes: Vec<u8>,
    },
    Ack {
        signed: DmAckSigned,
        signature: [u8; 64],
        signed_bytes: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    #[error("CBOR encode failed: {0}")]
    Cbor(String),
    /// Re-encoding `signed` to canonical CBOR failed inside `encode_packet`.
    /// The build_signed_* helpers already round-tripped this value through
    /// the same encoder, so this should be unreachable in practice — surface
    /// as a clear distinct variant so a regression here doesn't mask as a
    /// generic Cbor encode failure.
    #[error("re-encode signed body failed: {0}")]
    ReSerialize(String),
}

/// Errors produced by [`decode_packet`].
///
/// `Cbor(String)` stringifies the underlying `ciborium::de::Error` because
/// that error type does not implement `Clone + Eq`, which `DecodeError`
/// derives for use in tests and telemetry. Phase 3b's receive handler
/// can't currently distinguish truncated packets from type-mismatch errors —
/// if that distinction becomes load-bearing, widen this enum into specific
/// variants (e.g., `Truncated`, `TypeMismatch`) at that time.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
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

/// Encode a fully-built DmPacket to the wire layout
/// `[disc][signed_bytes][signature]`.
///
/// **Invariant guard.** Re-encode the body from `signed` (rather than
/// trusting the cached `signed_bytes` field) before concatenating with
/// the signature. This eliminates the inconsistent-state class where a
/// caller could hold `signed = A` but `signed_bytes` for some prior
/// `B` — a re-encode here means whatever ships on the wire matches
/// `signed` by construction. The cost is one extra CBOR encoding per
/// send, which is cheap (signed bodies are ≤ ~200 bytes serialized).
///
/// `signature` is still written verbatim — recomputing it here would
/// require the signing key, and the build_signed_* helpers already
/// signed `signed_bytes` (which equals the freshly-re-encoded bytes by
/// canonical-CBOR determinism, asserted in the build_signed_* tests).
pub fn encode_packet(packet: &DmPacket) -> Result<Vec<u8>, EncodeError> {
    let (disc, signed_bytes, signature): (u8, Vec<u8>, &[u8; 64]) = match packet {
        DmPacket::Invite {
            signed, signature, ..
        } => (
            0x01,
            crate::owner_state_crypto::canonical_cbor_encode(signed)
                .map_err(|e| EncodeError::ReSerialize(format!("re-encode signed body: {e}")))?,
            signature,
        ),
        DmPacket::CidNotify {
            signed, signature, ..
        } => (
            0x02,
            crate::owner_state_crypto::canonical_cbor_encode(signed)
                .map_err(|e| EncodeError::ReSerialize(format!("re-encode signed body: {e}")))?,
            signature,
        ),
        DmPacket::Ack {
            signed, signature, ..
        } => (
            0x03,
            crate::owner_state_crypto::canonical_cbor_encode(signed)
                .map_err(|e| EncodeError::ReSerialize(format!("re-encode signed body: {e}")))?,
            signature,
        ),
    };
    let mut out = Vec::with_capacity(1 + signed_bytes.len() + 64);
    out.push(disc);
    out.extend_from_slice(&signed_bytes);
    out.extend_from_slice(signature);
    Ok(out)
}

/// Build + sign a complete DmInvite packet ready for `encode_packet`.
/// Encodes `signed` to canonical CBOR, signs the resulting bytes via
/// `dm_signing::sign_dm_packet`, and bundles into the
/// `DmPacket::Invite` variant.
pub fn build_signed_invite(
    signed: DmInviteSigned,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<DmPacket, EncodeError> {
    let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed)
        .map_err(|e| EncodeError::Cbor(e.to_string()))?;
    let signature = crate::dm_signing::sign_dm_packet(&signed_bytes, signing_key);
    Ok(DmPacket::Invite {
        signed,
        signature,
        signed_bytes,
    })
}

/// Build + sign a complete DmCidNotify packet ready for `encode_packet`.
/// See `build_signed_invite` for rationale.
pub fn build_signed_cidnotify(
    signed: DmCidNotifySigned,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<DmPacket, EncodeError> {
    let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed)
        .map_err(|e| EncodeError::Cbor(e.to_string()))?;
    let signature = crate::dm_signing::sign_dm_packet(&signed_bytes, signing_key);
    Ok(DmPacket::CidNotify {
        signed,
        signature,
        signed_bytes,
    })
}

/// Build + sign a complete DmAck packet ready for `encode_packet`.
/// See `build_signed_invite` for rationale.
pub fn build_signed_ack(
    signed: DmAckSigned,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<DmPacket, EncodeError> {
    let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed)
        .map_err(|e| EncodeError::Cbor(e.to_string()))?;
    let signature = crate::dm_signing::sign_dm_packet(&signed_bytes, signing_key);
    Ok(DmPacket::Ack {
        signed,
        signature,
        signed_bytes,
    })
}

pub fn decode_packet(bytes: &[u8]) -> Result<DmPacket, DecodeError> {
    let (disc, rest) = bytes.split_first().ok_or(DecodeError::Empty)?;
    // Need at least 1 byte of body + 64 bytes of signature.
    if rest.len() < 64 + 1 {
        return Err(DecodeError::TooShortForSignature);
    }
    let split_at = rest.len() - 64;
    let (body_bytes, signature_bytes) = rest.split_at(split_at);
    let signature: [u8; 64] = signature_bytes
        .try_into()
        .expect("just split at len-64; signature_bytes is exactly 64 bytes");
    let signed_bytes = body_bytes.to_vec();
    let packet = match disc {
        0x01 => {
            let signed: DmInviteSigned = decode_body(body_bytes)?;
            // Phase 1 invariant: invites only flow over the DM transport
            // (Reticulum-unicast Dm/GroupDm). A non-DM kind on the wire is
            // either a malicious cross-protocol confusion attempt or a
            // sender bug; reject at the boundary, not in downstream code.
            if !matches!(signed.kind, SpaceKind::Dm | SpaceKind::GroupDm) {
                return Err(DecodeError::Invalid("DmInvite.kind must be Dm or GroupDm"));
            }
            if signed.sender_devices.len() > MAX_DEVICES_PER_OWNER {
                return Err(DecodeError::Invalid(
                    "DmInvite.sender_devices exceeds MAX_DEVICES_PER_OWNER",
                ));
            }
            // Mirror Space::validate_invariants for Dm/GroupDm member-set
            // shape (see owner_state_types.rs §"Distinct-member check"):
            // members must be strictly-ascending sorted (catches both
            // unsorted ordering AND duplicates in one predicate), the
            // member count must match the kind's range, and the inviter
            // must itself be a member. A peer violating any of these is
            // either malicious or buggy — reject at the wire boundary so
            // downstream code never sees a malformed DmInvite.
            if !signed.members.windows(2).all(|w| w[0] < w[1]) {
                return Err(DecodeError::Invalid(
                    "DmInvite.members must be strictly-ascending sorted (catches unsorted and duplicate)",
                ));
            }
            match signed.kind {
                SpaceKind::Dm => {
                    if signed.members.len() != 2 {
                        return Err(DecodeError::Invalid(
                            "DmInvite kind=Dm requires exactly 2 members",
                        ));
                    }
                }
                SpaceKind::GroupDm => {
                    if !(3..=16).contains(&signed.members.len()) {
                        return Err(DecodeError::Invalid(
                            "DmInvite kind=GroupDm requires 3..=16 members",
                        ));
                    }
                }
                _ => unreachable!("kind already restricted to Dm or GroupDm above"),
            }
            if !signed.members.contains(&signed.inviter) {
                return Err(DecodeError::Invalid(
                    "DmInvite.inviter must be a member of DmInvite.members",
                ));
            }
            // Phase 3b invariant: the device that signed the packet MUST
            // be in sender_devices. Catches structurally-inconsistent
            // packets before the receive handler even reaches the
            // (more expensive) signature-verification step.
            if !signed.sender_devices.contains(&signed.signing_device_hash) {
                return Err(DecodeError::Invalid(
                    "DmInvite.signing_device_hash must be in sender_devices",
                ));
            }
            DmPacket::Invite {
                signed,
                signature,
                signed_bytes,
            }
        }
        0x02 => {
            let signed: DmCidNotifySigned = decode_body(body_bytes)?;
            if signed.sender_devices.len() > MAX_DEVICES_PER_OWNER {
                return Err(DecodeError::Invalid(
                    "DmCidNotify.sender_devices exceeds MAX_DEVICES_PER_OWNER",
                ));
            }
            if !signed.sender_devices.contains(&signed.signing_device_hash) {
                return Err(DecodeError::Invalid(
                    "DmCidNotify.signing_device_hash must be in sender_devices",
                ));
            }
            DmPacket::CidNotify {
                signed,
                signature,
                signed_bytes,
            }
        }
        0x03 => {
            let signed: DmAckSigned = decode_body(body_bytes)?;
            if signed.ack_from_devices.len() > MAX_DEVICES_PER_OWNER {
                return Err(DecodeError::Invalid(
                    "DmAck.ack_from_devices exceeds MAX_DEVICES_PER_OWNER",
                ));
            }
            if !signed
                .ack_from_devices
                .contains(&signed.signing_device_hash)
            {
                return Err(DecodeError::Invalid(
                    "DmAck.signing_device_hash must be in ack_from_devices",
                ));
            }
            DmPacket::Ack {
                signed,
                signature,
                signed_bytes,
            }
        }
        other => return Err(DecodeError::UnknownDiscriminant(*other)),
    };
    Ok(packet)
}

/// Decode a CBOR body, rejecting any trailing bytes after the first valid
/// value. Mirrors `canonical_cbor_decode` in owner_state_crypto: without
/// this check an attacker can append arbitrary bytes to a valid packet
/// body, defeating any downstream code that fingerprints the encoded form
/// and weakening wire-format malleability resistance.
///
/// (In Phase 3b's appended-signature layout the signature already
/// occupies the last 64 bytes after `decode_packet`'s split, so the
/// trailing-bytes check on the body specifically catches CBOR-side
/// malleability — extra CBOR bytes that the body decoder ignored.)
fn decode_body<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, DecodeError> {
    let mut cursor = std::io::Cursor::new(bytes);
    let value: T =
        ciborium::from_reader(&mut cursor).map_err(|e| DecodeError::Cbor(e.to_string()))?;
    let consumed = cursor.position();
    if consumed as usize != bytes.len() {
        return Err(DecodeError::TrailingBytes {
            consumed,
            total: bytes.len() as u64,
        });
    }
    Ok(value)
}

/// Helper: serialize `[u8; 64]` as CBOR bstr (major type 2). Mirrors
/// the bstr-everywhere convention in owner_state_types and Task 4's
/// `serialize_device_identity_pubs`. Necessary because serde's blanket
/// `[T; N]: Serialize` only covers small N, so the derive-generated
/// serialization for the outer struct can't see `[u8; 64]: Serialize`
/// without an explicit `serialize_with`.
fn serialize_identity_pub_as_bstr<S>(b: &[u8; 64], s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    s.serialize_bytes(b)
}

/// Helper: deserialize CBOR bstr(64) into `[u8; 64]`. Pair with
/// `serialize_identity_pub_as_bstr`. Length is enforced strictly: a
/// bstr of any length other than 64 is rejected.
fn deserialize_identity_pub_from_bstr<'de, D>(d: D) -> Result<[u8; 64], D::Error>
where
    D: Deserializer<'de>,
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
                    "inviter_identity_pub must be 64 bytes, got {}",
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

// CanonicalPayload registrations.
// - MessagePayload: encoded via canonical_cbor_encode (required for AAD
//   stability in CAS blobs — see dm_crypto Task 6).
// - DmInviteSigned/DmCidNotifySigned/DmAckSigned: Phase 3b signs the
//   canonical-CBOR encoding of the body so verifiers reproduce the same
//   bytes. Registering on CanonicalPayload makes that the only encoding
//   path the build_signed_* helpers + signature-verification path use,
//   and satisfies the Phase 2 enforcement gate in owner_state_crypto.rs.
impl CanonicalPayloadSealed for MessagePayload {}
impl CanonicalPayload for MessagePayload {}
impl CanonicalPayloadSealed for DmInviteSigned {}
impl CanonicalPayload for DmInviteSigned {}
impl CanonicalPayloadSealed for DmCidNotifySigned {}
impl CanonicalPayload for DmCidNotifySigned {}
impl CanonicalPayloadSealed for DmAckSigned {}
impl CanonicalPayload for DmAckSigned {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::{
        ContentId, DeviceIdentityHash, DmContentKey, Hlc, OwnerAddr, SpaceId, SpaceKind,
    };

    fn hlc(ms: u64) -> Hlc {
        Hlc {
            wall_ms: ms,
            logical: 0,
            device_id: "d".into(),
        }
    }

    /// Test fixture: deterministic identity from a seed. Mirrors the
    /// pattern in dm_signing.rs::tests::make_test_identity. Returns the
    /// PrivateIdentity (for signing), the 64-byte identity pub (for
    /// inviter_identity_pub fields), and the device hash (for
    /// signing_device_hash + sender_devices fields).
    fn make_test_identity(
        seed_byte: u8,
    ) -> (
        harmony_identity::PrivateIdentity,
        [u8; 64],
        DeviceIdentityHash,
    ) {
        let private = harmony_identity::PrivateIdentity::from_seed(&[seed_byte; 32]);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let device_hash = DeviceIdentityHash(public.address_hash);
        (private, identity_pub, device_hash)
    }

    /// Sign + bundle a DmInviteSigned via the same path build_signed_invite
    /// would, but using PrivateIdentity::sign (since PrivateIdentity does
    /// not expose its inner SigningKey). Equivalence with sign_dm_packet
    /// is pinned by dm_signing's `sign_dm_packet_matches_private_identity_sign`
    /// test, so this gives us a real round-trip without test-only plumbing.
    fn sign_invite_with_identity(
        signed: DmInviteSigned,
        private: &harmony_identity::PrivateIdentity,
    ) -> DmPacket {
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private.sign(&signed_bytes);
        DmPacket::Invite {
            signed,
            signature,
            signed_bytes,
        }
    }

    fn sign_cidnotify_with_identity(
        signed: DmCidNotifySigned,
        private: &harmony_identity::PrivateIdentity,
    ) -> DmPacket {
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private.sign(&signed_bytes);
        DmPacket::CidNotify {
            signed,
            signature,
            signed_bytes,
        }
    }

    fn sign_ack_with_identity(
        signed: DmAckSigned,
        private: &harmony_identity::PrivateIdentity,
    ) -> DmPacket {
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private.sign(&signed_bytes);
        DmPacket::Ack {
            signed,
            signature,
            signed_bytes,
        }
    }

    #[test]
    fn message_payload_round_trip_canonical_cbor() {
        let m = MessagePayload {
            body: b"hello bob".to_vec(),
            mime_type: "text/plain".into(),
            sender: OwnerAddr([1; 16]),
            sent_at: hlc(1),
        };
        let bytes = crate::owner_state_crypto::canonical_cbor_encode(&m).unwrap();
        let recovered: MessagePayload =
            crate::owner_state_crypto::canonical_cbor_decode(&bytes).unwrap();
        assert_eq!(m, recovered);
    }

    #[test]
    fn message_payload_body_encodes_as_cbor_bstr_not_array() {
        // Golden-byte pin: MessagePayload.body MUST encode as CBOR bstr
        // (major type 2) — one header byte plus the raw bytes — NOT as
        // CBOR array of u8 (major type 4) where each byte > 0x17 takes
        // two bytes. The body field will dominate ciphertext size in
        // production; locking the byte-efficient form here means a
        // future refactor can't silently double on-wire overhead.
        let m = MessagePayload {
            body: vec![0xde, 0xad, 0xbe, 0xef],
            mime_type: "application/octet-stream".into(),
            sender: OwnerAddr([1; 16]),
            sent_at: hlc(1),
        };
        let bytes = crate::owner_state_crypto::canonical_cbor_encode(&m).unwrap();

        // bstr(4) header is 0x44 (major type 2 << 5 | length 4) followed
        // by the raw 4 bytes. This sequence MUST appear in the encoded
        // output.
        let bstr_form: [u8; 5] = [0x44, 0xde, 0xad, 0xbe, 0xef];
        assert!(
            bytes.windows(bstr_form.len()).any(|w| w == bstr_form),
            "expected CBOR bstr(4) form [0x44, 0xde, 0xad, 0xbe, 0xef] in encoded bytes; got {:02x?}",
            bytes
        );

        // array(4) of u8 form: 0x84 (major type 4 << 5 | length 4)
        // followed by four uint(0x18 N) tagged bytes (since each byte
        // > 0x17 needs the one-byte-uint header). This sequence MUST
        // NOT appear — that would mean the deserialize hook was lost
        // and we regressed to the old, ~2x-overhead encoding.
        let array_form: [u8; 9] = [0x84, 0x18, 0xde, 0x18, 0xad, 0x18, 0xbe, 0x18, 0xef];
        assert!(
            !bytes.windows(array_form.len()).any(|w| w == array_form),
            "encoded bytes contain forbidden array-of-u8 form for body: {:02x?}",
            bytes
        );

        // Round-trip: the decode hook must accept the bstr form and
        // produce the original Vec<u8>.
        let recovered: MessagePayload =
            crate::owner_state_crypto::canonical_cbor_decode(&bytes).unwrap();
        assert_eq!(recovered.body, vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(recovered, m);
    }

    /// Build a sample DmInviteSigned whose `signing_device_hash` is the
    /// hash derived from the test identity's public bytes. Tests that
    /// don't need a real signature can substitute a placeholder
    /// device_hash (matching `sender_devices[0]`) when the goal is to
    /// exercise a different invariant — see `sample_invite_with_hash`.
    fn sample_invite_with_identity(
        identity_pub: [u8; 64],
        device_hash: DeviceIdentityHash,
    ) -> DmInviteSigned {
        DmInviteSigned {
            space_id: SpaceId([1; 16]),
            kind: SpaceKind::Dm,
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            inviter: OwnerAddr([1; 16]),
            content_key: DmContentKey::new([0xaa; 32]),
            sender_devices: vec![device_hash],
            created_at: hlc(1),
            signing_device_hash: device_hash,
            inviter_identity_pub: identity_pub,
        }
    }

    /// For tests that don't care about real signature verification (they
    /// exercise other invariants like sender_devices oversized, member
    /// sort order, etc.). Uses placeholder signing_device_hash =
    /// sender_devices[0] so the new decode-time invariant
    /// (`signing_device_hash ∈ sender_devices`) doesn't preempt the
    /// test's intended check. inviter_identity_pub is a placeholder
    /// (all 0x42) — encoders/decoders don't validate the bytes against
    /// the device hash; that pairing is dm_signing's job.
    fn sample_invite_with_hash(device_hash: DeviceIdentityHash) -> DmInviteSigned {
        DmInviteSigned {
            space_id: SpaceId([1; 16]),
            kind: SpaceKind::Dm,
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            inviter: OwnerAddr([1; 16]),
            content_key: DmContentKey::new([0xaa; 32]),
            sender_devices: vec![device_hash],
            created_at: hlc(1),
            signing_device_hash: device_hash,
            inviter_identity_pub: [0x42; 64],
        }
    }

    fn sample_cidnotify_with_hash(device_hash: DeviceIdentityHash) -> DmCidNotifySigned {
        DmCidNotifySigned {
            space_id: SpaceId([1; 16]),
            message_cid: ContentId::from_bytes([0xee; 32]),
            sender_owner_addr: OwnerAddr([1; 16]),
            sender_devices: vec![device_hash],
            signing_device_hash: device_hash,
        }
    }

    fn sample_ack_with_hash(device_hash: DeviceIdentityHash) -> DmAckSigned {
        DmAckSigned {
            space_id: SpaceId([1; 16]),
            message_cid: ContentId::from_bytes([0xee; 32]),
            ack_from_owner_addr: OwnerAddr([2; 16]),
            ack_from_devices: vec![device_hash],
            signing_device_hash: device_hash,
        }
    }

    #[test]
    fn dm_packet_invite_round_trip() {
        let (private, identity_pub, device_hash) = make_test_identity(0x42);
        let signed = sample_invite_with_identity(identity_pub, device_hash);
        let p = sign_invite_with_identity(signed, &private);
        let encoded = encode_packet(&p).unwrap();
        assert_eq!(encoded[0], 0x01);
        let decoded = decode_packet(&encoded).unwrap();
        assert_eq!(p, decoded);
    }

    #[test]
    fn dm_packet_cidnotify_round_trip() {
        let (private, _, device_hash) = make_test_identity(0x42);
        let signed = sample_cidnotify_with_hash(device_hash);
        let p = sign_cidnotify_with_identity(signed, &private);
        let encoded = encode_packet(&p).unwrap();
        assert_eq!(encoded[0], 0x02);
        let decoded = decode_packet(&encoded).unwrap();
        assert_eq!(p, decoded);
    }

    #[test]
    fn dm_packet_ack_round_trip() {
        let (private, _, device_hash) = make_test_identity(0x42);
        let signed = sample_ack_with_hash(device_hash);
        let p = sign_ack_with_identity(signed, &private);
        let encoded = encode_packet(&p).unwrap();
        assert_eq!(encoded[0], 0x03);
        let decoded = decode_packet(&encoded).unwrap();
        assert_eq!(p, decoded);
    }

    #[test]
    fn dm_packet_unknown_discriminant_rejects() {
        // disc 0xff + 65 bytes of body+sig (1 byte body + 64 byte sig)
        // so we get past the TooShortForSignature check and hit the
        // discriminant check.
        let mut bytes = vec![0xff, 0xa0];
        bytes.extend_from_slice(&[0u8; 64]);
        let err = decode_packet(&bytes).unwrap_err();
        assert!(matches!(err, DecodeError::UnknownDiscriminant(0xff)));
    }

    #[test]
    fn dm_packet_empty_bytes_rejects() {
        let err = decode_packet(&[]).unwrap_err();
        assert!(matches!(err, DecodeError::Empty));
    }

    #[test]
    fn dm_packet_decode_too_short_for_signature_rejects() {
        // After the discriminant byte we need at least 1 byte of body + 64
        // bytes of signature = 65 bytes total post-discriminant.
        let bytes = vec![0x02, 0xa0]; // disc=0x02, body = empty CBOR map (1 byte), no signature
        let err = decode_packet(&bytes).unwrap_err();
        assert!(matches!(err, DecodeError::TooShortForSignature));
    }

    #[test]
    fn dm_packet_trailing_bytes_after_body_reject() {
        // Build a valid invite, then inject one extra byte between the
        // CBOR body and the appended signature. decode_body must reject
        // — matches canonical_cbor_decode and avoids wire-format
        // malleability. (Phase 3b's split-at-len-64 puts the extra byte
        // into the body slice, so decode_body sees it as a trailing
        // byte after the CBOR map.)
        let (private, identity_pub, device_hash) = make_test_identity(0x42);
        let signed = sample_invite_with_identity(identity_pub, device_hash);
        let p = sign_invite_with_identity(signed, &private);
        let encoded = encode_packet(&p).unwrap();
        // Insert the extra byte just before the appended 64-byte signature.
        let mut tampered = Vec::with_capacity(encoded.len() + 1);
        tampered.extend_from_slice(&encoded[..encoded.len() - 64]);
        tampered.push(0x00);
        tampered.extend_from_slice(&encoded[encoded.len() - 64..]);
        let err = decode_packet(&tampered).unwrap_err();
        assert!(
            matches!(err, DecodeError::TrailingBytes { .. }),
            "expected TrailingBytes, got {:?}",
            err
        );

        // Sanity: without the trailing byte, the same packet decodes fine.
        let decoded = decode_packet(&encoded).unwrap();
        assert_eq!(p, decoded);
    }

    #[test]
    fn dm_packet_invite_non_dm_kind_rejected() {
        // A peer sending an invite with kind=PublicChannel is either
        // malicious (cross-protocol confusion) or buggy. Reject at the
        // wire boundary instead of letting consumers see a mis-typed
        // Space proposal.
        let (private, identity_pub, device_hash) = make_test_identity(0x42);
        let mut signed = sample_invite_with_identity(identity_pub, device_hash);
        signed.kind = SpaceKind::PublicChannel;
        let p = sign_invite_with_identity(signed, &private);
        let bytes = encode_packet(&p).unwrap();
        let err = decode_packet(&bytes).unwrap_err();
        match err {
            DecodeError::Invalid(msg) => {
                assert!(
                    msg.contains("kind") && msg.contains("DmInvite"),
                    "expected message mentioning DmInvite.kind, got: {msg}"
                );
            }
            other => panic!("expected DecodeError::Invalid, got {:?}", other),
        }
    }

    #[test]
    fn dm_packet_invite_oversized_sender_devices_rejected() {
        // Use placeholder identity; this test exercises the oversized-
        // sender_devices check (decode-time invariant), not signature
        // verification. Set signing_device_hash = sender_devices[0] so
        // the new "signing_device_hash ∈ sender_devices" check doesn't
        // preempt the size check.
        let device_hash = DeviceIdentityHash([0; 16]);
        let mut signed = sample_invite_with_hash(device_hash);
        signed.sender_devices = (0..(MAX_DEVICES_PER_OWNER as u8 + 1))
            .map(|i| DeviceIdentityHash([i; 16]))
            .collect();
        signed.signing_device_hash = signed.sender_devices[0];
        // Encode without going through build_signed_invite (which would
        // reject if we tried to sign a malformed body).
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = [0u8; 64];
        let p = DmPacket::Invite {
            signed,
            signature,
            signed_bytes,
        };
        let bytes = encode_packet(&p).unwrap();
        let err = decode_packet(&bytes).unwrap_err();
        assert!(
            matches!(err, DecodeError::Invalid(msg) if msg.contains("sender_devices")),
            "expected DecodeError::Invalid with sender_devices, got {:?}",
            err
        );
    }

    #[test]
    fn dm_packet_invite_unsorted_members_rejected() {
        // Build a valid invite, then rotate members into descending
        // order. Encoder still emits, but decoder must reject — the
        // sorted-ascending invariant is part of the wire contract
        // (matches Space::validate_invariants for Dm/GroupDm).
        let (private, identity_pub, device_hash) = make_test_identity(0x42);
        let mut signed = sample_invite_with_identity(identity_pub, device_hash);
        signed.members = vec![OwnerAddr([2; 16]), OwnerAddr([1; 16])];
        // Inviter must still be in members for this to isolate the sort
        // check (otherwise the inviter-check would fire first).
        signed.inviter = OwnerAddr([2; 16]);
        let p = sign_invite_with_identity(signed, &private);
        let bytes = encode_packet(&p).unwrap();
        let err = decode_packet(&bytes).unwrap_err();
        assert!(
            matches!(err, DecodeError::Invalid(msg) if msg.contains("members") && msg.contains("ascending")),
            "expected DecodeError::Invalid mentioning members + ascending, got {:?}",
            err
        );
    }

    #[test]
    fn dm_packet_invite_duplicate_members_rejected() {
        // Strictly-ascending sort also catches adjacent duplicates —
        // a single predicate covers both unsorted and duplicate
        // members. Use a duplicated member to exercise that path.
        let (private, identity_pub, device_hash) = make_test_identity(0x42);
        let mut signed = sample_invite_with_identity(identity_pub, device_hash);
        signed.members = vec![OwnerAddr([1; 16]), OwnerAddr([1; 16])];
        signed.inviter = OwnerAddr([1; 16]);
        let p = sign_invite_with_identity(signed, &private);
        let bytes = encode_packet(&p).unwrap();
        let err = decode_packet(&bytes).unwrap_err();
        assert!(
            matches!(err, DecodeError::Invalid(msg) if msg.contains("members") && msg.contains("ascending")),
            "expected DecodeError::Invalid mentioning members + ascending, got {:?}",
            err
        );
    }

    #[test]
    fn dm_packet_invite_wrong_member_count_rejected() {
        // Dm requires exactly 2 members — sending 3 must reject.
        let (private, identity_pub, device_hash) = make_test_identity(0x42);
        let mut signed = sample_invite_with_identity(identity_pub, device_hash);
        signed.kind = SpaceKind::Dm;
        signed.members = vec![OwnerAddr([1; 16]), OwnerAddr([2; 16]), OwnerAddr([3; 16])];
        signed.inviter = OwnerAddr([1; 16]);
        let p = sign_invite_with_identity(signed, &private);
        let bytes = encode_packet(&p).unwrap();
        let err = decode_packet(&bytes).unwrap_err();
        assert!(
            matches!(err, DecodeError::Invalid(msg) if msg.contains("Dm") && msg.contains("2")),
            "expected DecodeError::Invalid for Dm wrong-count, got {:?}",
            err
        );

        // GroupDm requires 3..=16 — sending 2 must also reject.
        let mut signed = sample_invite_with_identity(identity_pub, device_hash);
        signed.kind = SpaceKind::GroupDm;
        signed.members = vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])];
        signed.inviter = OwnerAddr([1; 16]);
        let p = sign_invite_with_identity(signed, &private);
        let bytes = encode_packet(&p).unwrap();
        let err = decode_packet(&bytes).unwrap_err();
        assert!(
            matches!(err, DecodeError::Invalid(msg) if msg.contains("GroupDm") && msg.contains("3..=16")),
            "expected DecodeError::Invalid for GroupDm wrong-count, got {:?}",
            err
        );
    }

    #[test]
    fn dm_packet_invite_inviter_not_in_members_rejected() {
        // Inviter ∉ members is a contract violation: receiver MUST
        // verify inviter ∈ members before prompting the user (per the
        // doc comment on DmInvite.inviter). Reject at the wire boundary.
        let (private, identity_pub, device_hash) = make_test_identity(0x42);
        let mut signed = sample_invite_with_identity(identity_pub, device_hash);
        signed.members = vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])];
        signed.inviter = OwnerAddr([3; 16]); // not in members
        let p = sign_invite_with_identity(signed, &private);
        let bytes = encode_packet(&p).unwrap();
        let err = decode_packet(&bytes).unwrap_err();
        assert!(
            matches!(err, DecodeError::Invalid(msg) if msg.contains("inviter")),
            "expected DecodeError::Invalid mentioning inviter, got {:?}",
            err
        );
    }

    #[test]
    fn dm_packet_cidnotify_oversized_sender_devices_rejected() {
        let device_hash = DeviceIdentityHash([0; 16]);
        let mut signed = sample_cidnotify_with_hash(device_hash);
        signed.sender_devices = (0..(MAX_DEVICES_PER_OWNER as u8 + 1))
            .map(|i| DeviceIdentityHash([i; 16]))
            .collect();
        signed.signing_device_hash = signed.sender_devices[0];
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let p = DmPacket::CidNotify {
            signed,
            signature: [0u8; 64],
            signed_bytes,
        };
        let bytes = encode_packet(&p).unwrap();
        let err = decode_packet(&bytes).unwrap_err();
        assert!(
            matches!(err, DecodeError::Invalid(msg) if msg.contains("sender_devices")),
            "expected DecodeError::Invalid with sender_devices, got {:?}",
            err
        );
    }

    #[test]
    fn dm_packet_ack_oversized_ack_from_devices_rejected() {
        let device_hash = DeviceIdentityHash([0; 16]);
        let mut signed = sample_ack_with_hash(device_hash);
        signed.ack_from_devices = (0..(MAX_DEVICES_PER_OWNER as u8 + 1))
            .map(|i| DeviceIdentityHash([i; 16]))
            .collect();
        signed.signing_device_hash = signed.ack_from_devices[0];
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let p = DmPacket::Ack {
            signed,
            signature: [0u8; 64],
            signed_bytes,
        };
        let bytes = encode_packet(&p).unwrap();
        let err = decode_packet(&bytes).unwrap_err();
        assert!(
            matches!(err, DecodeError::Invalid(msg) if msg.contains("ack_from_devices")),
            "expected DecodeError::Invalid with ack_from_devices, got {:?}",
            err
        );
    }

    // ---- Phase 3b new tests ----

    #[test]
    fn dm_packet_invite_round_trip_with_signature() {
        // Full Phase 3b round-trip: build_signed_-equivalent flow,
        // verify the on-wire shape, decode, then run the signature back
        // through dm_signing::verify_dm_packet_signature to confirm the
        // captured signed_bytes are exactly what was signed.
        let (private, identity_pub, device_hash) = make_test_identity(0x42);
        let signed = sample_invite_with_identity(identity_pub, device_hash);
        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private.sign(&signed_bytes);
        let packet = DmPacket::Invite {
            signed: signed.clone(),
            signature,
            signed_bytes: signed_bytes.clone(),
        };

        let wire = encode_packet(&packet).unwrap();
        assert_eq!(wire[0], 0x01);
        assert_eq!(wire.len(), 1 + signed_bytes.len() + 64);

        let decoded = decode_packet(&wire).unwrap();
        match decoded {
            DmPacket::Invite {
                signed: d_signed,
                signature: d_sig,
                signed_bytes: d_bytes,
            } => {
                assert_eq!(d_signed, signed);
                assert_eq!(d_sig, signature);
                assert_eq!(d_bytes, signed_bytes);
                // Verify signature round-trips through decode.
                assert!(crate::dm_signing::verify_dm_packet_signature(
                    &d_bytes,
                    &d_sig,
                    &identity_pub,
                    device_hash,
                )
                .is_ok());
            }
            other => panic!("expected Invite, got {:?}", other),
        }
    }

    #[test]
    fn dm_packet_decode_signing_device_hash_not_in_sender_devices_rejects() {
        // Signed-body invariant: signing_device_hash MUST be in
        // sender_devices. A packet that violates this is structurally
        // inconsistent — reject at decode time, before signature
        // verification is even attempted.
        let (private, identity_pub, device_hash) = make_test_identity(0x42);

        let signed = DmInviteSigned {
            space_id: SpaceId([1; 16]),
            kind: SpaceKind::Dm,
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            inviter: OwnerAddr([1; 16]),
            content_key: DmContentKey::new([0xaa; 32]),
            // sender_devices does NOT include device_hash:
            sender_devices: vec![DeviceIdentityHash([0xab; 16])],
            created_at: hlc(1),
            // claims a device NOT in sender_devices:
            signing_device_hash: device_hash,
            inviter_identity_pub: identity_pub,
        };

        let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let signature = private.sign(&signed_bytes);
        let packet = DmPacket::Invite {
            signed,
            signature,
            signed_bytes,
        };
        let wire = encode_packet(&packet).unwrap();

        let err = decode_packet(&wire).unwrap_err();
        assert!(
            matches!(err, DecodeError::Invalid(msg) if msg.contains("signing_device_hash") && msg.contains("sender_devices")),
            "expected Invalid error mentioning signing_device_hash and sender_devices, got {:?}",
            err
        );
    }

    #[test]
    fn encode_packet_re_encodes_from_signed_not_signed_bytes() {
        // Invariant guard: `encode_packet` must re-encode the body from
        // `signed` and IGNORE the cached `signed_bytes` field. If a caller
        // ever ends up with `signed = A` but `signed_bytes` corresponds
        // to some other body B (e.g., from in-place mutation after a
        // `build_signed_*` helper), the wire output must still match
        // `signed = A`. This test simulates that inconsistent state by
        // building a packet via `build_signed_cidnotify` and then
        // overwriting `signed_bytes` with garbage, then verifying the
        // wire output decodes back to the ORIGINAL `signed`.
        let (private, _, device_hash) = make_test_identity(0x77);
        // Mirror the signing-key extraction from build_signed_invite_produces_verifiable_packet.
        use hkdf::Hkdf;
        use sha2::Sha256;
        let hk = Hkdf::<Sha256>::new(None, &[0x77u8; 32]);
        let mut ed_arr = [0u8; 32];
        hk.expand(b"harmony-identity-ed25519-v1", &mut ed_arr)
            .expect("HKDF length 32 within SHA-256 limit");
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_arr);

        let original_signed = sample_cidnotify_with_hash(device_hash);
        let packet = build_signed_cidnotify(original_signed.clone(), &signing_key).unwrap();

        // Corrupt the cached signed_bytes — this is the inconsistent
        // state we're guarding against. Use bytes that would never
        // CBOR-decode to a valid DmCidNotifySigned (a leading 0xff
        // is not a legal CBOR major type).
        let corrupted_packet = match packet {
            DmPacket::CidNotify {
                signed, signature, ..
            } => DmPacket::CidNotify {
                signed,
                signature,
                signed_bytes: vec![0xff, 0xff, 0xff, 0xff, 0xff],
            },
            other => panic!("expected CidNotify, got {:?}", other),
        };

        // Encode the corrupted packet — this MUST re-encode from `signed`
        // and produce a wire-decodable result, NOT propagate the corrupted
        // bytes.
        let wire = encode_packet(&corrupted_packet).unwrap();
        assert_eq!(wire[0], 0x02);

        // Decode and confirm we recovered the ORIGINAL signed body, not
        // the corrupted bytes (which would fail to decode).
        let decoded = decode_packet(&wire).unwrap();
        match decoded {
            DmPacket::CidNotify { signed, .. } => {
                let _ = private; // silence unused warning if path changes
                assert_eq!(
                    signed, original_signed,
                    "decoded signed must match the ORIGINAL signed, proving encode_packet \
                     re-encoded from `signed` and ignored the corrupted signed_bytes"
                );
            }
            other => panic!("expected CidNotify after decode, got {:?}", other),
        }
    }

    #[test]
    fn build_signed_invite_produces_verifiable_packet() {
        // Pin that the build_signed_invite helper produces a packet
        // whose appended signature verifies under the same key.
        // Without this pin, a future refactor that broke the
        // canonical-CBOR-encode step inside build_signed_invite
        // (e.g., switched to plain ciborium encoding without updating
        // dm_signing.verify_dm_packet_signature) could go undetected.
        let (private, identity_pub, device_hash) = make_test_identity(0x99);
        // Mirror the SigningKey from PrivateIdentity's HKDF derivation
        // (see dm_signing::tests::sign_dm_packet_matches_private_identity_sign
        // for the rationale and pinned bit-equivalence).
        use hkdf::Hkdf;
        use sha2::Sha256;
        let hk = Hkdf::<Sha256>::new(None, &[0x99u8; 32]);
        let mut ed_arr = [0u8; 32];
        hk.expand(b"harmony-identity-ed25519-v1", &mut ed_arr)
            .expect("HKDF length 32 within SHA-256 limit");
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_arr);

        let signed = sample_invite_with_identity(identity_pub, device_hash);
        let packet = build_signed_invite(signed, &signing_key).unwrap();

        let wire = encode_packet(&packet).unwrap();
        let decoded = decode_packet(&wire).unwrap();
        match decoded {
            DmPacket::Invite {
                signed: _,
                signature,
                signed_bytes,
            } => {
                assert!(crate::dm_signing::verify_dm_packet_signature(
                    &signed_bytes,
                    &signature,
                    &identity_pub,
                    device_hash,
                )
                .is_ok());
                // Also confirm the PrivateIdentity-based shortcut
                // produces the same signature (defensive — the real
                // bit-equivalence pin is in dm_signing).
                let sig_via_identity = private.sign(&signed_bytes);
                assert_eq!(signature, sig_via_identity);
            }
            other => panic!("expected Invite, got {:?}", other),
        }
    }
}
