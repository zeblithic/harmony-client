//! ZEB-216 Sub-B Phase 1: DM wire envelope types + discriminant codec.
//!
//! See `docs/specs/2026-05-02-zeb-216-sub-b-dm-transport-design.md`
//! §"Wire format" and §"Plaintext envelope (Phase 1, recap from ZEB-219)".
//!
//! All wire types use two-character serde renames so each struct's keys
//! are the same encoded length at a single nesting level — the same-length-
//! keys precondition documented on `crate::owner_state_crypto::canonical_cbor_encode`.

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::{
    ContentId, DeviceIdentityHash, DmContentKey, Hlc, OwnerAddr, SpaceId, SpaceKind,
    MAX_DEVICES_PER_OWNER,
};

/// Plaintext envelope encrypted into the CAS storage_blob. Bound by AAD
/// to the Space's dedupe_key; decrypt enforces (sender, sent_at) authenticity.
/// See ZEB-216 §"Plaintext envelope" / ZEB-219 §"Wire format".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagePayload {
    #[serde(rename = "bd")]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmInvite {
    #[serde(rename = "si")]
    pub space_id: SpaceId,
    #[serde(rename = "kn")]
    pub kind: SpaceKind,
    /// Members sorted ascending lex (matches Space::members invariant).
    /// Cannot be used to identify the inviter — see `inviter` field.
    #[serde(rename = "me")]
    pub members: Vec<OwnerAddr>,
    /// Explicit inviter OwnerAddr. Receiver MUST verify
    /// `inviter ∈ members` and `from_identity_hash ∈ sender_devices`
    /// before prompting the user.
    #[serde(rename = "iv")]
    pub inviter: OwnerAddr,
    #[serde(rename = "ck")]
    pub content_key: DmContentKey,
    #[serde(rename = "sd")]
    pub sender_devices: Vec<DeviceIdentityHash>,
    #[serde(rename = "ca")]
    pub created_at: Hlc,
}

/// Reticulum-unicast packet notifying recipients that a new encrypted
/// message blob exists at `message_cid` in CAS. `sender_owner_addr` is
/// diagnostic only — receiver MUST resolve the actual sender via
/// link-origin binding (ZEB-216 §"Link-origin binding rule").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmCidNotify {
    #[serde(rename = "si")]
    pub space_id: SpaceId,
    #[serde(rename = "mc")]
    pub message_cid: ContentId,
    #[serde(rename = "so")]
    pub sender_owner_addr: OwnerAddr,
    #[serde(rename = "sd")]
    pub sender_devices: Vec<DeviceIdentityHash>,
}

/// Reticulum-unicast packet acknowledging receipt of a DmCidNotify.
/// `ack_from_owner_addr` is diagnostic only — receiver MUST resolve via
/// link-origin binding AND verify the resolved owner is in
/// `OutboxEntry.recipient_owners`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmAck {
    #[serde(rename = "si")]
    pub space_id: SpaceId,
    #[serde(rename = "mc")]
    pub message_cid: ContentId,
    #[serde(rename = "ao")]
    pub ack_from_owner_addr: OwnerAddr,
    #[serde(rename = "ad")]
    pub ack_from_devices: Vec<DeviceIdentityHash>,
}

/// Discriminated union of Reticulum DM packets. Wire layout:
/// `[u8 discriminant][CBOR-encoded body]` with discriminants
/// 0x01=Invite, 0x02=CidNotify, 0x03=Ack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DmPacket {
    Invite(DmInvite),
    CidNotify(DmCidNotify),
    Ack(DmAck),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    #[error("CBOR encode failed: {0}")]
    Cbor(String),
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
    #[error("unknown discriminant byte 0x{0:02x}")]
    UnknownDiscriminant(u8),
    #[error("CBOR decode failed: {0}")]
    Cbor(String),
    #[error("trailing bytes after CBOR body: consumed {consumed} of {total}")]
    TrailingBytes { consumed: u64, total: u64 },
    #[error("payload invariant violated: {0}")]
    Invalid(&'static str),
}

pub fn encode_packet(packet: &DmPacket) -> Result<Vec<u8>, EncodeError> {
    let (disc, body): (u8, Vec<u8>) = match packet {
        DmPacket::Invite(p) => (0x01, encode_body(p)?),
        DmPacket::CidNotify(p) => (0x02, encode_body(p)?),
        DmPacket::Ack(p) => (0x03, encode_body(p)?),
    };
    let mut out = Vec::with_capacity(1 + body.len());
    out.push(disc);
    out.extend_from_slice(&body);
    Ok(out)
}

pub fn decode_packet(bytes: &[u8]) -> Result<DmPacket, DecodeError> {
    let (disc, body) = bytes.split_first().ok_or(DecodeError::Empty)?;
    let packet = match disc {
        0x01 => {
            let invite: DmInvite = decode_body(body)?;
            // Phase 1 invariant: invites only flow over the DM transport
            // (Reticulum-unicast Dm/GroupDm). A non-DM kind on the wire is
            // either a malicious cross-protocol confusion attempt or a
            // sender bug; reject at the boundary, not in downstream code.
            if !matches!(invite.kind, SpaceKind::Dm | SpaceKind::GroupDm) {
                return Err(DecodeError::Invalid("DmInvite.kind must be Dm or GroupDm"));
            }
            if invite.sender_devices.len() > MAX_DEVICES_PER_OWNER {
                return Err(DecodeError::Invalid(
                    "DmInvite.sender_devices exceeds MAX_DEVICES_PER_OWNER",
                ));
            }
            DmPacket::Invite(invite)
        }
        0x02 => {
            let pkt: DmCidNotify = decode_body(body)?;
            if pkt.sender_devices.len() > MAX_DEVICES_PER_OWNER {
                return Err(DecodeError::Invalid(
                    "DmCidNotify.sender_devices exceeds MAX_DEVICES_PER_OWNER",
                ));
            }
            DmPacket::CidNotify(pkt)
        }
        0x03 => {
            let pkt: DmAck = decode_body(body)?;
            if pkt.ack_from_devices.len() > MAX_DEVICES_PER_OWNER {
                return Err(DecodeError::Invalid(
                    "DmAck.ack_from_devices exceeds MAX_DEVICES_PER_OWNER",
                ));
            }
            DmPacket::Ack(pkt)
        }
        other => return Err(DecodeError::UnknownDiscriminant(*other)),
    };
    Ok(packet)
}

// Plain ciborium (not canonical_cbor_encode): Reticulum packets are not
// AAD-bound, so canonical byte-stability isn't required for correctness.
// Reticulum link-layer ECDH already provides per-packet integrity.
fn encode_body<T: Serialize>(value: &T) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::new();
    ciborium::into_writer(value, &mut out).map_err(|e| EncodeError::Cbor(e.to_string()))?;
    Ok(out)
}

/// Decode a CBOR body, rejecting any trailing bytes after the first valid
/// value. Mirrors `canonical_cbor_decode` in owner_state_crypto: without
/// this check an attacker can append arbitrary bytes to a valid packet
/// body, defeating any downstream code that fingerprints the encoded form
/// and weakening wire-format malleability resistance.
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

// CanonicalPayload registrations.
// - MessagePayload: encoded via canonical_cbor_encode (required for AAD
//   stability in CAS blobs — see dm_crypto Task 6).
// - DmInvite/DmCidNotify/DmAck: Reticulum wire bodies use plain
//   ciborium (see encode_body). Registered here so the Phase 2
//   enforcement gate in owner_state_crypto.rs passes, and so future
//   callers may use canonical_cbor_encode if they have reason to.
impl CanonicalPayloadSealed for MessagePayload {}
impl CanonicalPayload for MessagePayload {}
impl CanonicalPayloadSealed for DmInvite {}
impl CanonicalPayload for DmInvite {}
impl CanonicalPayloadSealed for DmCidNotify {}
impl CanonicalPayload for DmCidNotify {}
impl CanonicalPayloadSealed for DmAck {}
impl CanonicalPayload for DmAck {}

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

    fn sample_invite() -> DmInvite {
        DmInvite {
            space_id: SpaceId([1; 16]),
            kind: SpaceKind::Dm,
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            inviter: OwnerAddr([1; 16]),
            content_key: DmContentKey::new([0xaa; 32]),
            sender_devices: vec![DeviceIdentityHash([7; 16])],
            created_at: hlc(1),
        }
    }

    fn sample_cidnotify() -> DmCidNotify {
        DmCidNotify {
            space_id: SpaceId([1; 16]),
            message_cid: ContentId::from_bytes([0xee; 32]),
            sender_owner_addr: OwnerAddr([1; 16]),
            sender_devices: vec![DeviceIdentityHash([7; 16])],
        }
    }

    fn sample_ack() -> DmAck {
        DmAck {
            space_id: SpaceId([1; 16]),
            message_cid: ContentId::from_bytes([0xee; 32]),
            ack_from_owner_addr: OwnerAddr([2; 16]),
            ack_from_devices: vec![DeviceIdentityHash([8; 16])],
        }
    }

    #[test]
    fn dm_packet_invite_round_trip() {
        let p = DmPacket::Invite(sample_invite());
        let encoded = encode_packet(&p).unwrap();
        assert_eq!(encoded[0], 0x01);
        let decoded = decode_packet(&encoded).unwrap();
        assert_eq!(p, decoded);
    }

    #[test]
    fn dm_packet_cidnotify_round_trip() {
        let p = DmPacket::CidNotify(sample_cidnotify());
        let encoded = encode_packet(&p).unwrap();
        assert_eq!(encoded[0], 0x02);
        let decoded = decode_packet(&encoded).unwrap();
        assert_eq!(p, decoded);
    }

    #[test]
    fn dm_packet_ack_round_trip() {
        let p = DmPacket::Ack(sample_ack());
        let encoded = encode_packet(&p).unwrap();
        assert_eq!(encoded[0], 0x03);
        let decoded = decode_packet(&encoded).unwrap();
        assert_eq!(p, decoded);
    }

    #[test]
    fn dm_packet_unknown_discriminant_rejects() {
        let bytes = vec![0xff, 0xa0]; // garbage discriminant + empty CBOR map
        let err = decode_packet(&bytes).unwrap_err();
        assert!(matches!(err, DecodeError::UnknownDiscriminant(0xff)));
    }

    #[test]
    fn dm_packet_empty_bytes_rejects() {
        let err = decode_packet(&[]).unwrap_err();
        assert!(matches!(err, DecodeError::Empty));
    }

    #[test]
    fn dm_packet_trailing_bytes_after_body_reject() {
        // Encode a valid invite, then append one extra byte to the body
        // portion (after the discriminant). decode_body must reject —
        // matches canonical_cbor_decode and avoids wire-format malleability.
        let p = DmPacket::Invite(sample_invite());
        let mut encoded = encode_packet(&p).unwrap();
        encoded.push(0x00);
        let err = decode_packet(&encoded).unwrap_err();
        assert!(
            matches!(err, DecodeError::TrailingBytes { .. }),
            "expected TrailingBytes, got {:?}",
            err
        );

        // Sanity: without the trailing byte, the same packet decodes fine.
        let clean = encode_packet(&p).unwrap();
        let decoded = decode_packet(&clean).unwrap();
        assert_eq!(p, decoded);
    }

    #[test]
    fn dm_packet_invite_non_dm_kind_rejected() {
        // A peer sending an invite with kind=PublicChannel is either
        // malicious (cross-protocol confusion) or buggy. Reject at the
        // wire boundary instead of letting consumers see a mis-typed
        // Space proposal.
        let mut invite = sample_invite();
        invite.kind = SpaceKind::PublicChannel;
        let bytes = encode_packet(&DmPacket::Invite(invite)).unwrap();
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
        let mut invite = sample_invite();
        invite.sender_devices = (0..(MAX_DEVICES_PER_OWNER as u8 + 1))
            .map(|i| DeviceIdentityHash([i; 16]))
            .collect();
        let bytes = encode_packet(&DmPacket::Invite(invite)).unwrap();
        let err = decode_packet(&bytes).unwrap_err();
        assert!(
            matches!(err, DecodeError::Invalid(msg) if msg.contains("sender_devices")),
            "expected DecodeError::Invalid with sender_devices, got {:?}",
            err
        );
    }

    #[test]
    fn dm_packet_cidnotify_oversized_sender_devices_rejected() {
        let mut pkt = sample_cidnotify();
        pkt.sender_devices = (0..(MAX_DEVICES_PER_OWNER as u8 + 1))
            .map(|i| DeviceIdentityHash([i; 16]))
            .collect();
        let bytes = encode_packet(&DmPacket::CidNotify(pkt)).unwrap();
        let err = decode_packet(&bytes).unwrap_err();
        assert!(
            matches!(err, DecodeError::Invalid(msg) if msg.contains("sender_devices")),
            "expected DecodeError::Invalid with sender_devices, got {:?}",
            err
        );
    }

    #[test]
    fn dm_packet_ack_oversized_ack_from_devices_rejected() {
        let mut pkt = sample_ack();
        pkt.ack_from_devices = (0..(MAX_DEVICES_PER_OWNER as u8 + 1))
            .map(|i| DeviceIdentityHash([i; 16]))
            .collect();
        let bytes = encode_packet(&DmPacket::Ack(pkt)).unwrap();
        let err = decode_packet(&bytes).unwrap_err();
        assert!(
            matches!(err, DecodeError::Invalid(msg) if msg.contains("ack_from_devices")),
            "expected DecodeError::Invalid with ack_from_devices, got {:?}",
            err
        );
    }
}
