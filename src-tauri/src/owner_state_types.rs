//! Owner-state CRDT typed CBOR shapes (ZEB-215 Sub-A Phase 2).
//!
//! See specs:
//! - `docs/specs/2026-04-30-zeb-206-nav-tree-design.md` — data model
//! - `docs/specs/2026-04-30-zeb-211-owner-state-encryption-design.md` — canonical CBOR
//!
//! Every type in this module exists on the wire — changes here are
//! wire-format breaking. Field name renames are chosen so all keys at
//! a single nesting level have the same encoded CBOR length, satisfying
//! the precondition documented on `crate::owner_state_crypto::canonical_cbor_encode`.
//!
//! Phase 3 (Zenoh sync) and Phase 4 (IPC) consume these types; this
//! module has no I/O of its own.

#![allow(dead_code)] // Skeleton; tasks 2-9 fill in the public surface.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Helper: serialize byte array as CBOR bstr, not as array.
fn serialize_bytes_as_bstr<const N: usize, S>(b: &[u8; N], s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    s.serialize_bytes(b)
}

/// Helper: deserialize CBOR bstr into byte array.
fn deserialize_bytes_from_bstr<'de, const N: usize, D>(d: D) -> Result<[u8; N], D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Visitor;
    use std::fmt;

    struct BytesVisitor<const N: usize>;

    impl<'de, const N: usize> Visitor<'de> for BytesVisitor<N> {
        type Value = [u8; N];

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            write!(formatter, "a byte array of length {}", N)
        }

        fn visit_bytes<E>(self, value: &[u8]) -> Result<[u8; N], E>
        where
            E: serde::de::Error,
        {
            if value.len() != N {
                return Err(E::custom(format!(
                    "expected {} bytes, got {}",
                    N,
                    value.len()
                )));
            }
            let mut arr = [0u8; N];
            arr.copy_from_slice(value);
            Ok(arr)
        }

        fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<[u8; N], E>
        where
            E: serde::de::Error,
        {
            self.visit_bytes(&v)
        }
    }

    d.deserialize_bytes(BytesVisitor::<N>)
}

/// Hybrid Logical Clock.
///
/// Wire format (locked): CBOR map with single-char field names `w` / `l`
/// / `d` so all three keys encode to the same length (CBOR text(1) =
/// 2 bytes per key). Without this, `wall_ms` (7) / `logical` (7) /
/// `device_id` (9) would mix encoded lengths 8/8/10 and silently
/// violate the canonical-CBOR precondition. See PR #72 round 3 for
/// the rationale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hlc {
    #[serde(rename = "w")]
    pub wall_ms: u64,
    #[serde(rename = "l")]
    pub logical: u32,
    #[serde(rename = "d")]
    pub device_id: String,
}

impl Hlc {
    /// Lexicographic ordering on `(wall_ms, logical, device_id)`. See
    /// ZEB-211 spec §"Definition of strictly newer".
    pub fn is_strictly_newer_than(&self, other: &Hlc) -> bool {
        (self.wall_ms, self.logical, self.device_id.as_str())
            > (other.wall_ms, other.logical, other.device_id.as_str())
    }
}

/// 16-byte ULID-shaped identifier for Spaces. Stored on the wire as
/// `bstr(16)` (17 encoded bytes incl. CBOR length byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SpaceId(
    #[serde(
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub [u8; 16],
);

/// 16-byte truncated owner address (matches harmony-identity's
/// `ADDRESS_HASH_LENGTH = 16`). Stored as `bstr(16)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OwnerAddr(
    #[serde(
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub [u8; 16],
);

/// 32-byte BLAKE3 content identifier (matches harmony-content CID size).
/// Stored as `bstr(32)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContentId(
    #[serde(
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub [u8; 32],
);

/// 16-byte ULID for OutboxEntry IDs. Wire-shape identical to SpaceId
/// but the type distinction prevents accidental swaps at call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OutboxEntryId(
    #[serde(
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub [u8; 16],
);

/// Six SpaceKind variants. Wire format: single-char string per variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpaceKind {
    #[serde(rename = "f")]
    Folder,
    #[serde(rename = "c")]
    Community,
    #[serde(rename = "h")]
    Channel,
    #[serde(rename = "p")]
    PublicChannel,
    #[serde(rename = "d")]
    Dm,
    #[serde(rename = "g")]
    GroupDm,
}

/// Reticulum destination identifier — opaque bytes for Phase 2 (ZEB-16
/// plane B has not finalized the wire shape). Wrapped as a newtype so
/// future protocol changes don't ripple through every caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReticulumDest(pub Vec<u8>);

/// Transport binding. Internally tagged so the wire format is one CBOR
/// map per binding (not nested). Discriminant key `t`, variant codes
/// `z` / `r` (canonical CBOR same-length); inner field names also two
/// chars to match within the inner map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum TransportBinding {
    #[serde(rename = "z")]
    Zenoh {
        #[serde(rename = "tp")] // "topic"
        topic: String,
    },
    #[serde(rename = "r")]
    Reticulum {
        #[serde(rename = "pa")] // "participants"
        participants: Vec<ReticulumDest>,
    },
}

/// Per-Space notification preference (owner-local; not propagated to
/// other members).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationPref {
    #[serde(rename = "a")]
    All,
    #[serde(rename = "m")]
    Mentions,
    #[serde(rename = "u")]
    Muted,
}

#[cfg(test)]
mod hlc_tests {
    use super::*;

    #[test]
    fn hlc_strictly_newer_lexicographic() {
        let a = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "alice".into(),
        };
        let b = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "alice".into(),
        };
        assert!(!a.is_strictly_newer_than(&b));
        assert!(!b.is_strictly_newer_than(&a));

        let later_wall = Hlc {
            wall_ms: 101,
            ..a.clone()
        };
        assert!(later_wall.is_strictly_newer_than(&a));

        let later_logical = Hlc {
            logical: 1,
            ..a.clone()
        };
        assert!(later_logical.is_strictly_newer_than(&a));

        let later_device = Hlc {
            device_id: "bob".into(),
            ..a.clone()
        };
        assert!(later_device.is_strictly_newer_than(&a));
    }
}

#[cfg(test)]
mod newtype_tests {
    use super::*;
    use ciborium::{from_reader, into_writer};

    #[test]
    fn space_id_cbor_is_bstr_16() {
        // 0x50 = bstr major type, 16 bytes of data.
        // Encodes as: 0x50 (1 byte) + 16 bytes of data = 17 bytes total.
        let s = SpaceId([0u8; 16]);
        let mut bytes = Vec::new();
        into_writer(&s, &mut bytes).unwrap();
        assert_eq!(bytes.len(), 17);
        assert_eq!(bytes[0], 0x50);
    }

    #[test]
    fn content_id_cbor_is_bstr_32() {
        // 0x58 = bstr major type with 1-byte length following.
        // Encodes as: 0x58 (1 byte) + 0x20 (1 byte length=32) + 32 bytes = 34 bytes total.
        let c = ContentId([0u8; 32]);
        let mut bytes = Vec::new();
        into_writer(&c, &mut bytes).unwrap();
        assert_eq!(bytes.len(), 34);
        assert_eq!(bytes[0], 0x58); // bstr major type, len=32 (one-byte length follows)
        assert_eq!(bytes[1], 0x20); // length = 32
    }

    #[test]
    fn space_id_round_trip() {
        let s = SpaceId([7u8; 16]);
        let mut bytes = Vec::new();
        into_writer(&s, &mut bytes).unwrap();
        let recovered: SpaceId = from_reader(&bytes[..]).unwrap();
        assert_eq!(s, recovered);
    }

    #[test]
    fn space_id_ord_is_bytewise() {
        let a = SpaceId([0u8; 16]);
        let mut b_bytes = [0u8; 16];
        b_bytes[15] = 1;
        let b = SpaceId(b_bytes);
        assert!(a < b);
    }

    #[test]
    fn owner_addr_round_trip() {
        let a = OwnerAddr([0xab; 16]);
        let mut bytes = Vec::new();
        into_writer(&a, &mut bytes).unwrap();
        let recovered: OwnerAddr = from_reader(&bytes[..]).unwrap();
        assert_eq!(a, recovered);
        // Wire shape is bstr(16): 17 bytes total, first byte 0x50.
        assert_eq!(bytes.len(), 17);
        assert_eq!(bytes[0], 0x50);
    }

    #[test]
    fn outbox_entry_id_round_trip() {
        let oid = OutboxEntryId([0xcd; 16]);
        let mut bytes = Vec::new();
        into_writer(&oid, &mut bytes).unwrap();
        let recovered: OutboxEntryId = from_reader(&bytes[..]).unwrap();
        assert_eq!(oid, recovered);
        assert_eq!(bytes.len(), 17);
        assert_eq!(bytes[0], 0x50);
    }

    #[test]
    fn content_id_round_trip() {
        let c = ContentId([0xef; 32]);
        let mut bytes = Vec::new();
        into_writer(&c, &mut bytes).unwrap();
        let recovered: ContentId = from_reader(&bytes[..]).unwrap();
        assert_eq!(c, recovered);
        assert_eq!(bytes.len(), 34);
        assert_eq!(bytes[0], 0x58);
        assert_eq!(bytes[1], 0x20);
    }
}

#[cfg(test)]
mod enum_tests {
    use super::*;
    use ciborium::{from_reader, into_writer};

    #[test]
    fn space_kind_cbor_is_single_char() {
        // text(1) "f" → 0x61 0x66
        let k = SpaceKind::Folder;
        let mut bytes = Vec::new();
        into_writer(&k, &mut bytes).unwrap();
        assert_eq!(bytes, vec![0x61, b'f']);
    }

    #[test]
    fn space_kind_round_trip_all_variants() {
        for k in [
            SpaceKind::Folder,
            SpaceKind::Community,
            SpaceKind::Channel,
            SpaceKind::PublicChannel,
            SpaceKind::Dm,
            SpaceKind::GroupDm,
        ] {
            let mut bytes = Vec::new();
            into_writer(&k, &mut bytes).unwrap();
            let recovered: SpaceKind = from_reader(&bytes[..]).unwrap();
            assert_eq!(k, recovered);
        }
    }

    #[test]
    fn transport_binding_zenoh_round_trip() {
        let b = TransportBinding::Zenoh {
            topic: "harmony/owner/state".into(),
        };
        let mut bytes = Vec::new();
        into_writer(&b, &mut bytes).unwrap();
        let recovered: TransportBinding = from_reader(&bytes[..]).unwrap();
        assert_eq!(b, recovered);
    }

    #[test]
    fn transport_binding_reticulum_round_trip() {
        let b = TransportBinding::Reticulum {
            participants: vec![ReticulumDest(vec![1, 2, 3])],
        };
        let mut bytes = Vec::new();
        into_writer(&b, &mut bytes).unwrap();
        let recovered: TransportBinding = from_reader(&bytes[..]).unwrap();
        assert_eq!(b, recovered);
    }

    #[test]
    fn notification_pref_round_trip() {
        for p in [
            NotificationPref::All,
            NotificationPref::Mentions,
            NotificationPref::Muted,
        ] {
            let mut bytes = Vec::new();
            into_writer(&p, &mut bytes).unwrap();
            let recovered: NotificationPref = from_reader(&bytes[..]).unwrap();
            assert_eq!(p, recovered);
        }
    }
}
