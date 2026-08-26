//! ZEB-216 Sub-B Phase 1 + Phase 3b: DM wire envelope types + signed
//! discriminant codec.
//!
//! See `docs/specs/2026-05-02-zeb-216-sub-b-dm-transport-design.md`
//! §"Wire format", §"Plaintext envelope (Phase 1, recap from ZEB-219)",
//! and §"Application-signature binding rule".
//!
//! Phase 3b wire layout per Reticulum unicast packet:
//! `[u8 discriminant][CBOR(signed_body)][64 raw signature bytes]`
//!
//! The discriminant is routing-only (excluded from the signed bytes).
//! The signature is 64 raw Ed25519 bytes appended to the wire (NOT a
//! CBOR bstr — `encode_packet` uses `extend_from_slice(signature)`,
//! `decode_packet` uses `split_at(len - 64)`, no CBOR header involved).
//! Keeping it outside the CBOR map avoids the chicken-and-egg of
//! computing the signature over a body that contains it, and
//! `signed_bytes` is captured on decode so the receive handler can call
//! `dm_signing::verify_dm_packet_signature` without re-encoding.
//!
//! All wire types use two-character serde renames so each struct's keys
//! are the same encoded length at a single nesting level — the same-length-
//! keys precondition documented on `crate::owner_state_crypto::canonical_cbor_encode`.

use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize, Serializer};

use crate::owner_state_crypto::{
    canonical_cbor_encode, sealed::CanonicalPayloadSealed, CanonicalPayload,
};
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
    /// ZEB-580 S1: the inviter's enrolled-device (#2) EnrollmentCert. When
    /// present, the receiver verifies master→#2 (+ owner_id match + that the
    /// derived #2 DM hash equals `signing_device_hash`) and caches the #2 DM
    /// identity — no prior friend handshake needed. Absent for legacy #3
    /// senders (then the receiver falls back to `inviter_identity_pub`).
    /// Additive: the map key is omitted entirely when None, so pre-ZEB-580
    /// invites are byte-stable and decode with None.
    #[serde(rename = "ie", default, skip_serializing_if = "Option::is_none")]
    pub inviter_enrollment: Option<Box<harmony_owner::certs::EnrollmentCert>>,
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

/// ZEB-214: an opt-in read-receipt watermark. `read_up_to` is the HLC of the
/// newest message the sender has read in `space_id`; the recipient marks its
/// own sent messages at or before it as "seen". Signed with the sender's
/// per-device Ed25519 key (same key as `DmCidNotifySigned`) and verified via
/// the same admission chain. Ephemeral: pushed over the live tunnel only,
/// never deposited; carries no message body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmReadReceiptSigned {
    #[serde(rename = "si")]
    pub space_id: SpaceId,
    #[serde(rename = "so")]
    pub sender_owner_addr: OwnerAddr,
    #[serde(rename = "dh")]
    pub signing_device_hash: DeviceIdentityHash,
    #[serde(rename = "ru")]
    pub read_up_to: Hlc,
    #[serde(rename = "sa")]
    pub sent_at: Hlc,
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

/// Discriminated union of DM packets. The uniform wire layout per packet is
/// `[u8 discriminant][CBOR(signed_body)][64 raw signature bytes]` with
/// discriminants 0x01=Invite, 0x02=CidNotify, 0x03=Ack. The signature
/// tail is 64 raw bytes (NOT a CBOR bstr — encode appends via
/// `extend_from_slice`, decode splits via `split_at(len - 64)`).
///
/// 0x04=CidNotifyWithBlob (ZEB-484) is the ONE exception: it carries a second
/// variable-length field (the inline `storage_blob`) after the signature, so it
/// uses an explicit length-delimited layout
/// `[0x04][u32 BE len(signed_bytes)][signed_bytes][64 sig][storage_blob]` and its
/// OWN encode/decode path (`decode_cidnotify_with_blob`) — the "sig = last 64
/// bytes" split above does NOT apply to it.
///
/// Each variant carries:
/// - `signed`: the typed body the signature covers.
/// - `signature`: the 64-byte Ed25519 signature appended after the body.
/// - `signed_bytes`: the canonical CBOR encoding of `signed` captured by
///   `decode_packet` on the receive path so the receive handler can call
///   `dm_signing::verify_dm_packet_signature` without re-encoding (the
///   captured bytes are exactly what the signature covers, so even if
///   the encoder were to drift this is the bit-exact verification input).
///   On the send path `encode_packet` re-encodes from `signed`, asserts
///   byte-equality with `signed_bytes`, and emits `signed_bytes`
///   verbatim — see `encode_packet`'s mutation-guard doc comment for
///   rationale.
// ZEB-580 S1: `DmInviteSigned` grew an optional inline `EnrollmentCert`
// (`inviter_enrollment`), which widened `Invite` well past `CidNotify`/`Ack`
// and tripped clippy's `large_enum_variant` lint. Fixed by boxing the cert
// (`Option<Box<EnrollmentCert>>`) rather than suppressing the lint —
// `Box<T>`'s `Serialize`/`Deserialize` forward transparently to `T`, so this
// is wire-format-neutral, and `Option<Box<T>>` is pointer-sized so it shrinks
// `DmInviteSigned`/`DmPacket` back down without rippling `Box::new`/deref
// through every `DmPacket::Invite { signed, .. }` match site (only the field
// itself is boxed, not the whole `signed`/`DmInviteSigned` payload).
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
    /// ZEB-484 (Move 1c): a `CidNotify` carrying the encrypted DM `storage_blob`
    /// inline, for live peer-to-peer delivery over the PQ tunnel when the
    /// recipient has no butler. `signed`/`signature`/`signed_bytes` are IDENTICAL
    /// to a bare `CidNotify` (the same Ed25519 signature authenticates them); the
    /// `storage_blob` carries no separate signature because it is bound by
    /// content-addressing — the receiver recomputes
    /// `ContentId::for_book(storage_blob)` and rejects a mismatch. The wire layout
    /// is length-delimited (two variable-length fields), distinct from the
    /// `[disc][body][64-sig]` layout of the other three variants. See
    /// `encode_packet` / `decode_packet`.
    CidNotifyWithBlob {
        signed: DmCidNotifySigned,
        signature: [u8; 64],
        signed_bytes: Vec<u8>,
        storage_blob: Vec<u8>,
    },
    /// ZEB-685 (S3): a friend-scoped device-revocation push. Carries the
    /// revoking owner's master-signed `RevocationCert` + the paired
    /// `EnrollmentCert` (needed to bridge the cert's `target` device_id[16] to
    /// the revoked ed25519[32] the cutoff projection keys on). No outer frame
    /// signature — both certs are master-signed and the sender is authenticated
    /// by the tunnel-peer bind + `revocation.owner == peer owner` trust-bind
    /// (see dm ingest). Wire: `0x05 || cbor(RevocationPushBody)`.
    RevocationPush {
        revocation: Box<harmony_owner::certs::RevocationCert>,
        enrollment: Box<harmony_owner::certs::EnrollmentCert>,
    },
    /// ZEB-214: an opt-in read-receipt watermark control frame. Uses the shared
    /// `[disc 0x06][CBOR body][64-byte sig]` layout, device-Ed25519-signed like
    /// `CidNotify`. Never a chat message — ingest emits `dm-read-receipt` only.
    ReadReceipt {
        signed: DmReadReceiptSigned,
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
    /// `encode_packet` re-encoded `signed` and the result diverged from the
    /// cached `signed_bytes` field — the only way this fires is post-build
    /// mutation of the `signed` field (no in-crate code path does this, but
    /// the guard catches future regressions cheaply: a stale `signature`
    /// would otherwise ship on the wire over a body it doesn't cover).
    #[error("signed body mutated post-build: {0}")]
    SignedMutated(String),
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
/// `[disc][signed_bytes][signature]` (Invite/CidNotify/Ack). The
/// `CidNotifyWithBlob` variant (0x04) instead emits the length-delimited
/// `[0x04][u32 BE len(signed_bytes)][signed_bytes][signature][storage_blob]` via
/// an early return — see the `DmPacket` docs.
///
/// **Mutation guard.** Re-encodes `signed` and asserts byte-equality
/// with the cached `signed_bytes` (which was the source for `signature`
/// at build time). Mismatch → `SignedMutated` (the only way this fires
/// is post-build mutation of the `signed` field; no in-crate code path
/// does this, but the guard catches future regressions cheaply with a
/// memcmp — no Ed25519 verify on the send path).
///
/// On success the function emits the cached `signed_bytes` verbatim
/// (NOT the freshly re-encoded bytes). Since `signed_bytes` was signed
/// during build, the `signature ↔ signed_bytes` invariant is
/// constructed; the byte-equality check above transitively gives
/// `signature ↔ signed`. Side benefit: emitting `signed_bytes`
/// verbatim preserves byte-exactness on decode→encode round trips
/// (relay scenarios), since the on-wire body is preserved bit-for-bit.
pub fn encode_packet(packet: &DmPacket) -> Result<Vec<u8>, EncodeError> {
    // ZEB-484: the blob-carrying variant has two variable-length fields, so it
    // uses an explicit length-delimited layout instead of the shared
    // [disc][signed_bytes][64-sig] layout. Handle + return early.
    if let DmPacket::CidNotifyWithBlob {
        signed,
        signature,
        signed_bytes,
        storage_blob,
    } = packet
    {
        let re_encoded = crate::owner_state_crypto::canonical_cbor_encode(signed)
            .map_err(|e| EncodeError::ReSerialize(format!("re-encode signed body: {e}")))?;
        if re_encoded != *signed_bytes {
            return Err(EncodeError::SignedMutated(
                "DmPacket CidNotifyWithBlob variant: signed mutated post-build (re-encode \
                 mismatches cached signed_bytes; signature would not cover wire body)"
                    .to_string(),
            ));
        }
        let body_len = u32::try_from(signed_bytes.len()).map_err(|_| {
            EncodeError::Cbor("CidNotifyWithBlob signed_bytes length exceeds u32".to_string())
        })?;
        let mut out = Vec::with_capacity(1 + 4 + signed_bytes.len() + 64 + storage_blob.len());
        out.push(0x04);
        out.extend_from_slice(&body_len.to_be_bytes());
        out.extend_from_slice(signed_bytes);
        out.extend_from_slice(signature);
        out.extend_from_slice(storage_blob);
        return Ok(out);
    }
    // ZEB-685 (S3): RevocationPush has no outer frame signature (both certs
    // are master-signed already) and only one variable-length field, so it
    // gets its own early-return rather than participating in the shared
    // [disc][signed_bytes][64-sig] layout below.
    if let DmPacket::RevocationPush {
        revocation,
        enrollment,
    } = packet
    {
        let body = RevocationPushBody {
            revocation: (**revocation).clone(),
            enrollment: (**enrollment).clone(),
        };
        let cbor = crate::owner_state_crypto::canonical_cbor_encode(&body)
            .map_err(|e| EncodeError::ReSerialize(format!("revocation_push body: {e}")))?;
        let mut out = Vec::with_capacity(1 + cbor.len());
        out.push(0x05);
        out.extend_from_slice(&cbor);
        return Ok(out);
    }
    let (disc, signed_bytes, signature): (u8, &Vec<u8>, &[u8; 64]) = match packet {
        DmPacket::Invite {
            signed,
            signature,
            signed_bytes,
        } => {
            let re_encoded = crate::owner_state_crypto::canonical_cbor_encode(signed)
                .map_err(|e| EncodeError::ReSerialize(format!("re-encode signed body: {e}")))?;
            if re_encoded != *signed_bytes {
                return Err(EncodeError::SignedMutated(
                    "DmPacket Invite variant: signed mutated post-build (re-encode mismatches \
                     cached signed_bytes; signature would not cover wire body)"
                        .to_string(),
                ));
            }
            (0x01, signed_bytes, signature)
        }
        DmPacket::CidNotify {
            signed,
            signature,
            signed_bytes,
        } => {
            let re_encoded = crate::owner_state_crypto::canonical_cbor_encode(signed)
                .map_err(|e| EncodeError::ReSerialize(format!("re-encode signed body: {e}")))?;
            if re_encoded != *signed_bytes {
                return Err(EncodeError::SignedMutated(
                    "DmPacket CidNotify variant: signed mutated post-build (re-encode mismatches \
                     cached signed_bytes; signature would not cover wire body)"
                        .to_string(),
                ));
            }
            (0x02, signed_bytes, signature)
        }
        DmPacket::Ack {
            signed,
            signature,
            signed_bytes,
        } => {
            let re_encoded = crate::owner_state_crypto::canonical_cbor_encode(signed)
                .map_err(|e| EncodeError::ReSerialize(format!("re-encode signed body: {e}")))?;
            if re_encoded != *signed_bytes {
                return Err(EncodeError::SignedMutated(
                    "DmPacket Ack variant: signed mutated post-build (re-encode mismatches \
                     cached signed_bytes; signature would not cover wire body)"
                        .to_string(),
                ));
            }
            (0x03, signed_bytes, signature)
        }
        DmPacket::ReadReceipt {
            signed,
            signature,
            signed_bytes,
        } => {
            let re_encoded = crate::owner_state_crypto::canonical_cbor_encode(signed)
                .map_err(|e| EncodeError::ReSerialize(format!("re-encode signed body: {e}")))?;
            if re_encoded != *signed_bytes {
                return Err(EncodeError::SignedMutated(
                    "DmPacket ReadReceipt variant: signed mutated post-build (re-encode \
                     mismatches cached signed_bytes; signature would not cover wire body)"
                        .to_string(),
                ));
            }
            (0x06, signed_bytes, signature)
        }
        DmPacket::CidNotifyWithBlob { .. } => {
            unreachable!("CidNotifyWithBlob is handled by the early return above")
        }
        DmPacket::RevocationPush { .. } => {
            unreachable!("RevocationPush is handled by the early return above")
        }
    };
    let mut out = Vec::with_capacity(1 + signed_bytes.len() + 64);
    out.push(disc);
    out.extend_from_slice(signed_bytes);
    out.extend_from_slice(signature);
    Ok(out)
}

/// ZEB-685 (S3): the CBOR body for `DmPacket::RevocationPush` (everything
/// after the `0x05` discriminant). Two-character serde renames match the
/// same-length-keys canonical-CBOR convention this module uses throughout
/// (see the module doc comment).
#[derive(serde::Serialize, serde::Deserialize)]
struct RevocationPushBody {
    #[serde(rename = "rv")]
    revocation: harmony_owner::certs::RevocationCert,
    #[serde(rename = "en")]
    enrollment: harmony_owner::certs::EnrollmentCert,
}

/// ZEB-685: construct a `RevocationPush` packet (no outer signature). Called by
/// the send-side friend-push hook (`owner_commands::push_revocation_to_friends`).
pub fn build_revocation_push_packet(
    revocation: harmony_owner::certs::RevocationCert,
    enrollment: harmony_owner::certs::EnrollmentCert,
) -> DmPacket {
    DmPacket::RevocationPush {
        revocation: Box::new(revocation),
        enrollment: Box::new(enrollment),
    }
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

/// ZEB-214: build + sign a complete read-receipt packet ready for
/// `encode_packet`. Same shape as `build_signed_cidnotify`.
pub fn build_signed_read_receipt(
    signed: DmReadReceiptSigned,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<DmPacket, EncodeError> {
    let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed)
        .map_err(|e| EncodeError::Cbor(e.to_string()))?;
    let signature = crate::dm_signing::sign_dm_packet(&signed_bytes, signing_key);
    Ok(DmPacket::ReadReceipt {
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

/// ZEB-484: build a `CidNotifyWithBlob` packet — a signed `CidNotify` (signed
/// EXACTLY like `build_signed_cidnotify`) plus the encrypted `storage_blob`
/// carried inline. The blob is NOT signed; it is bound by content-addressing at
/// the receiver (`ContentId::for_book(storage_blob) == signed.message_cid`).
pub fn build_signed_cidnotify_with_blob(
    signed: DmCidNotifySigned,
    signing_key: &ed25519_dalek::SigningKey,
    storage_blob: Vec<u8>,
) -> Result<DmPacket, EncodeError> {
    let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed)
        .map_err(|e| EncodeError::Cbor(e.to_string()))?;
    let signature = crate::dm_signing::sign_dm_packet(&signed_bytes, signing_key);
    Ok(DmPacket::CidNotifyWithBlob {
        signed,
        signature,
        signed_bytes,
        storage_blob,
    })
}

pub fn decode_packet(bytes: &[u8]) -> Result<DmPacket, DecodeError> {
    let (disc, rest) = bytes.split_first().ok_or(DecodeError::Empty)?;
    // ZEB-484: the blob-carrying variant is length-delimited (two variable
    // fields); it does NOT use the "sig = last 64 bytes" split used below.
    if *disc == 0x04 {
        return decode_cidnotify_with_blob(rest);
    }
    // ZEB-685 (S3): RevocationPush (`0x05`) carries no outer frame signature
    // (both certs inside are already master-signed — see the `DmPacket`
    // variant doc comment), so it does not fit the "last 64 bytes are a
    // signature" split below either. Decode the whole remainder as the CBOR
    // body and return early, mirroring the 0x04 special case above.
    if *disc == 0x05 {
        let body: RevocationPushBody = crate::owner_state_crypto::canonical_cbor_decode(rest)
            .map_err(|e| DecodeError::Cbor(format!("revocation_push body: {e}")))?;
        return Ok(DmPacket::RevocationPush {
            revocation: Box::new(body.revocation),
            enrollment: Box::new(body.enrollment),
        });
    }
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
            ensure_canonical_body(&signed, body_bytes)?;
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
            ensure_canonical_body(&signed, body_bytes)?;
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
            ensure_canonical_body(&signed, body_bytes)?;
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
        0x06 => {
            // ZEB-214: a read-receipt watermark. Shared `[disc][body][64-sig]`
            // layout; no `sender_devices` field, so no membership guard here —
            // the ingest admission check resolves + verifies the signer.
            let signed: DmReadReceiptSigned = decode_body(body_bytes)?;
            ensure_canonical_body(&signed, body_bytes)?;
            DmPacket::ReadReceipt {
                signed,
                signature,
                signed_bytes,
            }
        }
        other => return Err(DecodeError::UnknownDiscriminant(*other)),
    };
    Ok(packet)
}

/// ZEB-484: decode the length-delimited `CidNotifyWithBlob` body (everything
/// after the `0x04` discriminant):
/// `[u32 BE len(signed_bytes)][signed_bytes][64 sig][storage_blob]`.
/// Applies the same CidNotify structural invariants as the `0x02` arm and
/// requires a non-empty blob (an empty blob is malformed — the sender would use
/// a bare `CidNotify`).
fn decode_cidnotify_with_blob(rest: &[u8]) -> Result<DmPacket, DecodeError> {
    if rest.len() < 4 {
        return Err(DecodeError::TooShortForSignature);
    }
    let (len_bytes, after_len) = rest.split_at(4);
    let body_len = u32::from_be_bytes(
        len_bytes
            .try_into()
            .expect("split_at(4) yields exactly 4 bytes"),
    ) as usize;
    // Checked add: `body_len` is attacker-controlled (from the wire u32). On a
    // 32-bit target `body_len + 64` could wrap and bypass the guard, panicking in
    // `split_at` below. Parsing untrusted bytes must never panic. (Qodo)
    let need = body_len
        .checked_add(64)
        .ok_or(DecodeError::TooShortForSignature)?;
    if after_len.len() < need {
        return Err(DecodeError::TooShortForSignature);
    }
    let (body_bytes, after_body) = after_len.split_at(body_len);
    let (signature_bytes, storage_blob) = after_body.split_at(64);
    let signature: [u8; 64] = signature_bytes
        .try_into()
        .expect("split_at(64) yields exactly 64 bytes");
    if storage_blob.is_empty() {
        return Err(DecodeError::Invalid(
            "CidNotifyWithBlob.storage_blob must be non-empty",
        ));
    }
    let signed: DmCidNotifySigned = decode_body(body_bytes)?;
    ensure_canonical_body(&signed, body_bytes)?;
    if signed.sender_devices.len() > MAX_DEVICES_PER_OWNER {
        return Err(DecodeError::Invalid(
            "CidNotifyWithBlob.sender_devices exceeds MAX_DEVICES_PER_OWNER",
        ));
    }
    if !signed.sender_devices.contains(&signed.signing_device_hash) {
        return Err(DecodeError::Invalid(
            "CidNotifyWithBlob.signing_device_hash must be in sender_devices",
        ));
    }
    Ok(DmPacket::CidNotifyWithBlob {
        signed,
        signature,
        signed_bytes: body_bytes.to_vec(),
        storage_blob: storage_blob.to_vec(),
    })
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

/// Re-encode `signed` via the same canonical CBOR encoder that
/// `encode_packet` uses, and reject if the result differs from
/// `body_bytes`. This is the round-trip invariant that pairs with
/// `encode_packet`'s re-encode-from-`signed` guard (commit 6efc81f):
/// the send path now always produces canonical bytes, and a future
/// relay/cache-and-replay path would re-encode on send — so any
/// non-canonical body accepted here would break signature verification
/// downstream. Reject non-canonical bodies at the wire boundary.
///
/// Catches: reordered map keys, indefinite-length encodings, oversized
/// integer/length prefixes, anything else where decode → canonical-
/// re-encode is not byte-identical to the original.
fn ensure_canonical_body<T: CanonicalPayload>(
    signed: &T,
    body_bytes: &[u8],
) -> Result<(), DecodeError> {
    let canonical = canonical_cbor_encode(signed).map_err(|e| DecodeError::Cbor(e.to_string()))?;
    if canonical != body_bytes {
        return Err(DecodeError::Invalid(
            "DM packet body must use canonical CBOR",
        ));
    }
    Ok(())
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
impl CanonicalPayloadSealed for DmReadReceiptSigned {}
impl CanonicalPayload for DmReadReceiptSigned {}
// ZEB-685 (S3): RevocationPushBody is encoded/decoded via
// canonical_cbor_encode/canonical_cbor_decode (see encode_packet's 0x05 arm
// and decode_packet's 0x05 early return), so it needs the same registration.
impl CanonicalPayloadSealed for RevocationPushBody {}
impl CanonicalPayload for RevocationPushBody {}

/// Deterministic wire-type fixtures for in-crate tests. ZEB-236 Task 1
/// extracted `minimal_invite_for_space` from the inline builder in
/// `dm_outbox.rs`'s `handle_invite_writes_space_and_cache_with_signing_pub`
/// so downstream stores (`pending_dm_invites`) and later tasks share one
/// self-consistent invite. Plain `#[cfg(test)]`: every consumer is an
/// in-crate unit test (no integration test needs it — see Task 1 brief).
#[cfg(any(test, feature = "test-fixtures"))]
pub mod test_fixtures {
    use super::DmInviteSigned;
    use crate::owner_state_types::{
        DeviceIdentityHash, DmContentKey, Hlc, OwnerAddr, SpaceId, SpaceKind,
    };

    /// A minimal, signature-consistent `DmInviteSigned` seeded by `space`.
    ///
    /// The identity is derived from `PrivateIdentity::from_seed(&[space; 32])`
    /// exactly as the `handle_invite_*` test does, so `signing_device_hash`,
    /// `sender_devices[0]`, and `inviter_identity_pub` all agree (the invite
    /// would verify). `space` seeds `space_id`/`inviter`/`members` so distinct
    /// `space` values key distinct store entries. `inviter ∈ members` and
    /// `signing_device_hash ∈ sender_devices` hold, so a later accept path can
    /// reuse this without re-deriving.
    pub fn minimal_invite_for_space(space: u8) -> DmInviteSigned {
        let private = harmony_identity::PrivateIdentity::from_seed(&[space; 32]);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let device_hash = DeviceIdentityHash(public.address_hash);

        DmInviteSigned {
            space_id: SpaceId([space; 16]),
            kind: SpaceKind::Dm,
            inviter: OwnerAddr([space; 16]),
            members: vec![OwnerAddr([space; 16])],
            content_key: DmContentKey::new([space; 32]),
            sender_devices: vec![device_hash],
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "fixture".into(),
            },
            signing_device_hash: device_hash,
            inviter_identity_pub: identity_pub,
            inviter_enrollment: None,
        }
    }
}

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
            inviter_enrollment: None,
        }
    }

    /// ZEB-580 S1 Task 2: zero-arg sample invite fixture for tests that
    /// don't care about the specific identity/device values, just a
    /// well-formed `DmInviteSigned` with `inviter_enrollment: None` to
    /// override or leave as-is. Wraps `sample_invite_with_identity` with a
    /// fixed seed (0x42, matching `make_test_identity`'s convention
    /// elsewhere in this module).
    fn sample_invite_signed() -> DmInviteSigned {
        let (_, identity_pub, device_hash) = make_test_identity(0x42);
        sample_invite_with_identity(identity_pub, device_hash)
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
            inviter_enrollment: None,
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

    /// ZEB-580 S1: a DmInvite carrying an inviter_enrollment round-trips
    /// byte-exactly and the signature still covers the cert.
    #[test]
    fn dm_invite_with_inviter_enrollment_round_trips() {
        let minted = harmony_owner::lifecycle::mint_owner(1_700_000_000).expect("mint");
        let cert = minted.state.enrollments.values().next().unwrap().clone();
        let sk = minted.device_signing_key;

        let mut signed = sample_invite_signed(); // existing helper in this test mod
        signed.inviter_enrollment = Some(Box::new(cert.clone()));

        let packet = build_signed_invite(signed.clone(), &sk).expect("build");
        let wire = encode_packet(&packet).expect("encode");
        let decoded = decode_packet(&wire).expect("decode");
        match decoded {
            DmPacket::Invite { signed: got, .. } => {
                assert_eq!(got.inviter_enrollment, Some(Box::new(cert)));
                assert_eq!(got, signed);
            }
            _ => panic!("expected Invite"),
        }
    }

    /// Back-compat: a DmInvite WITHOUT the field decodes with None, and the
    /// canonical bytes are identical to a pre-ZEB-580 invite (skip_serializing_if
    /// omits the key entirely when None).
    #[test]
    fn dm_invite_without_inviter_enrollment_is_none_and_byte_stable() {
        let signed = sample_invite_signed(); // inviter_enrollment defaults to None
        let bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let back: DmInviteSigned =
            crate::owner_state_crypto::canonical_cbor_decode(&bytes).unwrap();
        assert_eq!(back.inviter_enrollment, None);
        // The "ie" key must be absent from the map when None.
        assert!(!bytes.windows(2).any(|w| w == b"ie"));
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
            inviter_enrollment: None,
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
    fn encode_packet_returns_signed_mutated_when_signed_field_diverges_from_cached_bytes() {
        // Mutation guard: if a caller mutates the `signed` field after
        // `build_signed_*` (no in-crate path does this, but the guard
        // catches future regressions), the cached `signature` would no
        // longer cover the wire body. `encode_packet` re-encodes
        // `signed`, compares with the cached `signed_bytes`, and rejects
        // on mismatch with `EncodeError::SignedMutated`.
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
        let packet = build_signed_cidnotify(original_signed, &signing_key).unwrap();

        // Mutate the `signed` field — this simulates a post-build mutation.
        // The cached `signed_bytes` (which was the input to `signature`) no
        // longer matches a fresh re-encode of `signed`.
        let mutated_packet = match packet {
            DmPacket::CidNotify {
                mut signed,
                signature,
                signed_bytes,
            } => {
                signed.message_cid = ContentId::from_bytes([0x99; 32]);
                DmPacket::CidNotify {
                    signed,
                    signature,
                    signed_bytes,
                }
            }
            other => panic!("expected CidNotify, got {:?}", other),
        };

        let err = encode_packet(&mutated_packet).unwrap_err();
        match err {
            EncodeError::SignedMutated(msg) => {
                assert!(
                    msg.contains("CidNotify") && msg.contains("signed mutated post-build"),
                    "expected message mentioning CidNotify variant and post-build mutation, \
                     got: {msg}"
                );
            }
            other => panic!("expected EncodeError::SignedMutated, got {:?}", other),
        }
        let _ = private; // unused — kept to match make_test_identity tuple
    }

    #[test]
    fn encode_packet_returns_signed_mutated_when_cached_signed_bytes_diverges() {
        // The dual: corrupt the cached `signed_bytes` directly. The
        // re-encoded `signed` no longer matches, so the same guard fires.
        // Pre-fix this test asserted the corrupted bytes were IGNORED and
        // the re-encode shipped — the new invariant is stricter (the
        // cached bytes ARE what ships, and divergence MUST be surfaced).
        let (private, _, device_hash) = make_test_identity(0x77);
        use hkdf::Hkdf;
        use sha2::Sha256;
        let hk = Hkdf::<Sha256>::new(None, &[0x77u8; 32]);
        let mut ed_arr = [0u8; 32];
        hk.expand(b"harmony-identity-ed25519-v1", &mut ed_arr)
            .expect("HKDF length 32 within SHA-256 limit");
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_arr);

        let original_signed = sample_cidnotify_with_hash(device_hash);
        let packet = build_signed_cidnotify(original_signed, &signing_key).unwrap();

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

        let err = encode_packet(&corrupted_packet).unwrap_err();
        assert!(
            matches!(err, EncodeError::SignedMutated(_)),
            "expected SignedMutated, got {:?}",
            err
        );
        let _ = private;
    }

    #[test]
    fn encode_packet_round_trip_emits_byte_exact_cached_signed_bytes() {
        // Side benefit of emitting `signed_bytes` verbatim: a
        // decode→encode round trip preserves the on-wire body bit-for-
        // bit (load-bearing for relay scenarios where intermediaries
        // forward a verbatim copy). If `encode_packet` shipped the
        // re-encoded bytes, canonical-CBOR determinism gives byte-
        // equality in practice but doesn't *guarantee* it across
        // encoder versions.
        let (private, _, device_hash) = make_test_identity(0x77);
        use hkdf::Hkdf;
        use sha2::Sha256;
        let hk = Hkdf::<Sha256>::new(None, &[0x77u8; 32]);
        let mut ed_arr = [0u8; 32];
        hk.expand(b"harmony-identity-ed25519-v1", &mut ed_arr)
            .expect("HKDF length 32 within SHA-256 limit");
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_arr);

        let signed = sample_cidnotify_with_hash(device_hash);
        let packet = build_signed_cidnotify(signed, &signing_key).unwrap();

        let wire1 = encode_packet(&packet).unwrap();
        let decoded = decode_packet(&wire1).unwrap();
        let wire2 = encode_packet(&decoded).unwrap();
        assert_eq!(
            wire1, wire2,
            "decode→encode must preserve on-wire bytes exactly (byte-for-byte)"
        );
        let _ = private;
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

    #[test]
    fn decode_packet_rejects_non_canonical_cbor_body() {
        // Defense-in-depth pair to encode_packet's re-encode-from-`signed`
        // invariant (commit 6efc81f). decode_packet must reject any body
        // that is not the canonical encoding of the typed `signed` value
        // — otherwise decode → encode is no longer byte-identical, which
        // would break signature verification on any future relay /
        // cache-and-replay path.
        //
        // Strategy: take a canonical CidNotify body, swap the leading
        // definite-length map header (0xa5 — major type 5 << 5 | count 5)
        // for an indefinite-length map header (0xbf) and append a CBOR
        // break byte (0xff). Both encodings decode to the same
        // DmCidNotifySigned via ciborium, but only the definite-length
        // form is canonical. The decoder must therefore reject the
        // indefinite-length variant.
        let device_hash = DeviceIdentityHash([0x42; 16]);
        let signed = sample_cidnotify_with_hash(device_hash);
        let canonical = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        // Sanity: confirm the canonical leading byte is the 5-element
        // definite-length map header. If this ever changes (e.g., the
        // struct gains/loses a field) the test will surface it loudly
        // rather than silently testing the wrong byte.
        assert_eq!(
            canonical[0], 0xa5,
            "expected definite-length map(5) header 0xa5 at start of canonical CidNotify body, got 0x{:02x}",
            canonical[0]
        );

        // Build the non-canonical variant: 0xbf + entries + 0xff.
        let mut non_canonical = Vec::with_capacity(canonical.len() + 1);
        non_canonical.push(0xbf);
        non_canonical.extend_from_slice(&canonical[1..]);
        non_canonical.push(0xff);

        // Sanity: the non-canonical body still ciborium-decodes to the
        // same value (otherwise we'd just be testing decode_body's
        // CBOR-error path, not the canonical-body check).
        let round_trip: DmCidNotifySigned =
            ciborium::from_reader(non_canonical.as_slice()).unwrap();
        assert_eq!(round_trip, signed);

        // Assemble a wire packet: [disc=0x02][non_canonical_body][sig].
        // Signature can be all zeros — the canonical-body check fires
        // before signature verification ever runs.
        let mut wire = Vec::with_capacity(1 + non_canonical.len() + 64);
        wire.push(0x02);
        wire.extend_from_slice(&non_canonical);
        wire.extend_from_slice(&[0u8; 64]);

        let err = decode_packet(&wire).unwrap_err();
        match err {
            DecodeError::Invalid(msg) => {
                assert!(
                    msg.contains("canonical"),
                    "expected error mentioning canonical CBOR, got: {msg}"
                );
            }
            other => panic!(
                "expected DecodeError::Invalid('...canonical...'), got {:?}",
                other
            ),
        }

        // Sanity: the canonical version of the same body is accepted
        // (proves the test is isolating the canonical-body check, not
        // some other invariant).
        let mut canonical_wire = Vec::with_capacity(1 + canonical.len() + 64);
        canonical_wire.push(0x02);
        canonical_wire.extend_from_slice(&canonical);
        canonical_wire.extend_from_slice(&[0u8; 64]);
        // We expect the decode to succeed structurally (signature is
        // not validated by decode_packet — that happens in the receive
        // handler).
        let _decoded = decode_packet(&canonical_wire).expect(
            "canonical-body wire packet must decode (canonical-body check is the only \
             new gate in this test)",
        );
    }

    #[test]
    fn dm_packet_cidnotify_with_blob_round_trip() {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x42u8; 32]);
        let device_hash = DeviceIdentityHash([0x11; 16]);
        let signed = DmCidNotifySigned {
            space_id: SpaceId([0x77; 16]),
            message_cid: ContentId::from_bytes([0xab; 32]),
            sender_owner_addr: OwnerAddr([0xA1; 16]),
            sender_devices: vec![device_hash],
            signing_device_hash: device_hash,
        };
        let storage_blob = vec![0xCDu8; 4096];
        let packet =
            build_signed_cidnotify_with_blob(signed.clone(), &signing_key, storage_blob.clone())
                .expect("build with-blob packet");
        let wire = encode_packet(&packet).expect("encode");
        assert_eq!(wire[0], 0x04, "discriminant byte is 0x04");
        let decoded = decode_packet(&wire).expect("decode");
        match decoded {
            DmPacket::CidNotifyWithBlob {
                signed: d_signed,
                storage_blob: d_blob,
                ..
            } => {
                assert_eq!(d_signed, signed, "signed body round-trips");
                assert_eq!(d_blob, storage_blob, "storage_blob round-trips");
            }
            other => panic!("expected CidNotifyWithBlob, got {other:?}"),
        }
    }

    #[test]
    fn read_receipt_packet_roundtrips_and_signs() {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let signed = DmReadReceiptSigned {
            space_id: SpaceId([1u8; 16]),
            sender_owner_addr: OwnerAddr([2u8; 16]),
            signing_device_hash: DeviceIdentityHash([3u8; 16]),
            read_up_to: hlc(1_700_000_000_000),
            sent_at: hlc(1_700_000_005_000),
        };
        let packet = build_signed_read_receipt(signed.clone(), &sk).expect("build");
        let wire = encode_packet(&packet).expect("encode");
        assert_eq!(wire[0], 0x06, "read-receipt discriminant");
        match decode_packet(&wire).expect("decode") {
            DmPacket::ReadReceipt {
                signed: got,
                signature,
                signed_bytes,
            } => {
                assert_eq!(got, signed, "signed body round-trips");
                // The signature covers exactly the canonical body bytes.
                sk.verifying_key()
                    .verify_strict(
                        &signed_bytes,
                        &ed25519_dalek::Signature::from_bytes(&signature),
                    )
                    .expect("signature must verify over signed_bytes");
            }
            other => panic!("expected ReadReceipt, got {other:?}"),
        }
    }

    #[test]
    fn dm_packet_cidnotify_with_blob_empty_blob_rejected() {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x42u8; 32]);
        let device_hash = DeviceIdentityHash([0x11; 16]);
        let signed = DmCidNotifySigned {
            space_id: SpaceId([0x77; 16]),
            message_cid: ContentId::from_bytes([0xab; 32]),
            sender_owner_addr: OwnerAddr([0xA1; 16]),
            sender_devices: vec![device_hash],
            signing_device_hash: device_hash,
        };
        let packet = build_signed_cidnotify_with_blob(signed, &signing_key, Vec::new()).unwrap();
        let wire = encode_packet(&packet).unwrap();
        let err = decode_packet(&wire).unwrap_err();
        assert!(
            matches!(err, DecodeError::Invalid(m) if m.contains("storage_blob must be non-empty")),
            "an empty inline blob must be rejected, got {err:?}"
        );
    }

    #[test]
    fn dm_packet_cidnotify_with_blob_truncated_rejected() {
        let err = decode_packet(&[0x04, 0x00, 0x00]).unwrap_err();
        assert!(
            matches!(err, DecodeError::TooShortForSignature),
            "got {err:?}"
        );
    }

    #[test]
    fn dm_packet_cidnotify_with_blob_huge_len_rejected_without_panic() {
        // A 0x04 packet claiming body_len = u32::MAX with only a few trailing
        // bytes must be rejected GRACEFULLY — no panic, even where `body_len + 64`
        // would overflow usize on a 32-bit target (Qodo: untrusted parsing must
        // never panic; the decoder uses `checked_add`).
        let mut wire = vec![0x04];
        wire.extend_from_slice(&u32::MAX.to_be_bytes());
        wire.extend_from_slice(&[0u8; 8]);
        let err = decode_packet(&wire).unwrap_err();
        assert!(
            matches!(err, DecodeError::TooShortForSignature),
            "got {err:?}"
        );
    }

    // ── ZEB-685 (S3) Task 1: RevocationPush DM control frame ───────────────

    /// ZEB-685 (S3) test fixture: a master-signed `RevocationCert` paired
    /// with the master-signed `EnrollmentCert` it targets (same owner,
    /// `revocation.target == enrollment.device_id`). Built via the real
    /// `harmony_owner::lifecycle::mint_owner` mint path — mirrors
    /// `dm_invite_with_inviter_enrollment_round_trips` above — so the master
    /// signing key is recoverable from the `RecoveryArtifact` to sign the
    /// revocation with the same owner the enrollment cert is bound to.
    fn sample_revocation_and_enrollment() -> (
        harmony_owner::certs::RevocationCert,
        harmony_owner::certs::EnrollmentCert,
    ) {
        let minted = harmony_owner::lifecycle::mint_owner(1_700_000_000).expect("mint");
        let enrollment = minted.state.enrollments.values().next().unwrap().clone();
        let master_sk = minted.recovery_artifact.master_signing_key();
        let master_bundle = minted.recovery_artifact.master_pubkey_bundle();
        let revocation = harmony_owner::certs::RevocationCert::sign_master(
            &master_sk,
            master_bundle,
            enrollment.device_id,
            1_700_000_100,
            harmony_owner::certs::RevocationReason::Lost,
        )
        .expect("sign_master revocation");
        (revocation, enrollment)
    }

    #[test]
    fn revocation_push_round_trips() {
        let (revocation, enrollment) = sample_revocation_and_enrollment();
        let pkt = build_revocation_push_packet(revocation.clone(), enrollment.clone());
        let wire = encode_packet(&pkt).expect("encode");
        assert_eq!(wire[0], 0x05, "discriminant");
        let back = decode_packet(&wire).expect("decode");
        match back {
            DmPacket::RevocationPush {
                revocation: r,
                enrollment: e,
            } => {
                assert_eq!(*r, revocation);
                assert_eq!(*e, enrollment);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
