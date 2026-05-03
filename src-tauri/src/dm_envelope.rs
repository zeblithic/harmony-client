//! ZEB-216 Sub-B Phase 1: DM wire envelope types + discriminant codec.
//!
//! See `docs/specs/2026-05-02-zeb-216-sub-b-dm-transport-design.md`
//! §"Wire format" and §"Plaintext envelope (Phase 1, recap from ZEB-219)".

use serde::{Deserialize, Serialize};

use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::{
    ContentId, DeviceIdentityHash, DmContentKey, Hlc, OwnerAddr, SpaceId, SpaceKind,
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

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("packet is empty")]
    Empty,
    #[error("unknown discriminant byte 0x{0:02x}")]
    UnknownDiscriminant(u8),
    #[error("CBOR decode failed: {0}")]
    Cbor(String),
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
    match disc {
        0x01 => Ok(DmPacket::Invite(decode_body(body)?)),
        0x02 => Ok(DmPacket::CidNotify(decode_body(body)?)),
        0x03 => Ok(DmPacket::Ack(decode_body(body)?)),
        other => Err(DecodeError::UnknownDiscriminant(*other)),
    }
}

fn encode_body<T: Serialize>(value: &T) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::new();
    ciborium::into_writer(value, &mut out).map_err(|e| EncodeError::Cbor(e.to_string()))?;
    Ok(out)
}

fn decode_body<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, DecodeError> {
    ciborium::from_reader(bytes).map_err(|e| DecodeError::Cbor(e.to_string()))
}

// CanonicalPayload registrations — these wire types pass through
// `canonical_cbor_encode` from owner_state_crypto.
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
}
