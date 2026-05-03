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

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Helper: serialize byte array as CBOR bstr, not as array.
fn serialize_bytes_as_bstr<const N: usize, S>(b: &[u8; N], s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    s.serialize_bytes(b)
}

/// Helper: serialize a `Vec<u8>` as CBOR bstr (major type 2). Used by
/// variable-length opaque-bytes wrapper types like `ReticulumDest` so
/// they don't accidentally encode as a CBOR array of u8 (major type 4).
fn serialize_vec_as_bstr<S>(b: &[u8], s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    s.serialize_bytes(b)
}

/// Helper: deserialize a CBOR bstr into a `Vec<u8>`. Pair with
/// `serialize_vec_as_bstr`.
fn deserialize_vec_from_bstr<'de, D>(d: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Visitor;
    use std::fmt;

    struct VecBytesVisitor;

    impl<'de> Visitor<'de> for VecBytesVisitor {
        type Value = Vec<u8>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            write!(formatter, "a CBOR byte string (major type 2)")
        }

        fn visit_bytes<E>(self, value: &[u8]) -> Result<Vec<u8>, E>
        where
            E: serde::de::Error,
        {
            Ok(value.to_vec())
        }

        fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Vec<u8>, E>
        where
            E: serde::de::Error,
        {
            Ok(v)
        }
    }

    d.deserialize_bytes(VecBytesVisitor)
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

/// State-root publish payload (encrypted plaintext on the
/// `harmony/owner/{addr_hex}/state-root-v1` Zenoh topic).
///
/// Wire format: canonical CBOR map with two single-letter-length
/// keys to satisfy `canonical_cbor_encode`'s same-length-keys
/// precondition. See spec §"State-root payload format".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootPublishPayload {
    #[serde(rename = "rc")]
    pub root_cid: ContentId,
    #[serde(rename = "at")]
    pub at: Hlc,
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

/// 32-byte structured content identifier (4-byte header + 28-byte hash).
/// Re-exported from harmony-content. Stored as `bstr(32)` on the wire
/// (after the harmony-content companion PR fixed `Serialize for ContentId`
/// to emit bstr instead of array-of-u8).
///
/// Phase 3b switches from a local `ContentId([u8; 32])` newtype (raw
/// BLAKE3 hash) to harmony-content's structured CID (header[4] +
/// SHA-256-MSB-truncated hash[28]). Wire shape unchanged: 32-byte bstr.
/// Meaning of those 32 bytes changes — see Phase 3b spec §"Wire format".
pub use harmony_content::cid::ContentId;

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

/// Maximum number of historical content keys retained per Space.
/// See ZEB-219 §"Cap policy" and ZEB-216 §"Dedupe-merge cap rule".
pub const MAX_PRIOR_CONTENT_KEYS: usize = 16;

/// Maximum number of device identities retained per OwnerAddr in
/// OwnerDeviceCache. Bounds the cache's memory footprint AND the
/// Reticulum-MTU cost of any piggybacked sender_devices lists.
/// See ZEB-216 §"OwnerDeviceCache".
pub const MAX_DEVICES_PER_OWNER: usize = 32;

/// 32-byte symmetric content key for DM/group-DM ChaCha20-Poly1305
/// encryption. Wire format: bstr(32). In-memory: zeroized on drop
/// (custom Drop via ZeroizeOnDrop derive). Debug redacts the bytes
/// to avoid accidental leakage to logs.
///
/// See ZEB-216 §"Wire-format newtypes (Phase 1)".
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, zeroize::ZeroizeOnDrop)]
pub struct DmContentKey(
    #[serde(
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    [u8; 32],
);

impl DmContentKey {
    pub fn new(key: [u8; 32]) -> Self {
        Self(key)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Generate a fresh random key from OS entropy. Used when creating a
    /// new DM/group-DM Space.
    pub fn random() -> Self {
        use rand::RngCore;
        use zeroize::Zeroizing;
        let mut k = Zeroizing::new([0u8; 32]);
        rand::rngs::OsRng.fill_bytes(k.as_mut());
        Self(*k)
    }
}

impl std::fmt::Debug for DmContentKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DmContentKey(<32 bytes redacted>)")
    }
}

/// 16-byte Reticulum device identity hash. Wire format: bstr(16).
/// See ZEB-216 §"OwnerDeviceCache".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DeviceIdentityHash(
    #[serde(
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub [u8; 16],
);

/// Per-OwnerAddr cache of known bound-device identity hashes. Replicated
/// across the user's bound devices via Flow A (owner-state CRDT sync).
/// Each entry maintained via LWW on `learned_at` HLC.
///
/// See ZEB-216 §"OwnerDeviceCache (Phase 1)".
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerDeviceCache {
    #[serde(rename = "d")]
    pub devices: BTreeMap<OwnerAddr, OwnerDeviceEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerDeviceEntry {
    /// Sorted ascending lex, deduped, capped at MAX_DEVICES_PER_OWNER.
    /// Sorted invariant means binary_search works for lookup
    /// (used by resolve_link_origin_owner in Phase 3b).
    ///
    /// `deserialize_with` re-normalizes (sort + dedup + truncate) on every
    /// load so persisted-state files and remote replicas can't hand us a
    /// `Vec` that violates the invariant — a corrupted on-disk snapshot
    /// or a malicious peer's `OwnerState` blob can otherwise break the
    /// binary_search precondition Phase 3b's link-origin resolver depends on.
    ///
    /// SECURITY NOTE: truncation keeps lex-smallest entries; an attacker
    /// who controls injected DeviceIdentityHash values could grind low-byte
    /// prefixes to displace legitimate devices. Acceptable in Phase 1 since
    /// updates must win the LWW HLC check (i.e., the owner's own device must
    /// publish the update). See ZEB-219 for the analogous prior_content_keys
    /// concern.
    #[serde(rename = "v", deserialize_with = "deserialize_device_identities")]
    pub devices: Vec<DeviceIdentityHash>,
    /// HLC of when this entry was learned. LWW key for merge.
    #[serde(rename = "l")]
    pub learned_at: Hlc,
}

/// Deserialize a `Vec<DeviceIdentityHash>` and re-establish the
/// `OwnerDeviceEntry::devices` invariant (sorted + deduped + truncated to
/// `MAX_DEVICES_PER_OWNER`). This runs at every load — persisted-state files
/// and remote `OwnerState` snapshots are equally untrusted with respect to
/// the in-memory invariant.
fn deserialize_device_identities<'de, D>(d: D) -> Result<Vec<DeviceIdentityHash>, D::Error>
where
    D: Deserializer<'de>,
{
    let mut devices = Vec::<DeviceIdentityHash>::deserialize(d)?;
    devices.sort();
    devices.dedup();
    devices.truncate(MAX_DEVICES_PER_OWNER);
    Ok(devices)
}

impl OwnerDeviceCache {
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }
}

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
pub struct ReticulumDest(
    #[serde(
        serialize_with = "serialize_vec_as_bstr",
        deserialize_with = "deserialize_vec_from_bstr"
    )]
    pub Vec<u8>,
);

/// Transport binding. Internally tagged so the wire format is one CBOR
/// map per binding (not nested). Discriminant key `tg` (2 chars to match
/// the inner field key length per `canonical_cbor_encode`'s same-length-
/// keys precondition); variant codes `z` / `r` (1 char — values, not keys,
/// so not subject to that rule); inner field names `tp` / `pa`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tg")]
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

// ZEB-220 sealed trait impls — every wire type in this module must
// implement `CanonicalPayload` so it can pass through
// `canonical_cbor_encode` / `canonical_cbor_decode`. Adding a new
// type to this module is incomplete until its impl is added here.

use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};

macro_rules! impl_canonical {
    ($($t:ty),* $(,)?) => {
        $(
            impl CanonicalPayloadSealed for $t {}
            impl CanonicalPayload for $t {}
        )*
    };
}

impl_canonical!(
    Hlc,
    SpaceId,
    OwnerAddr,
    ContentId,
    OutboxEntryId,
    DmContentKey,
    DeviceIdentityHash,
    OwnerDeviceCache,
    OwnerDeviceEntry,
    SpaceKind,
    NotificationPref,
    ReticulumDest,
    TransportBinding,
    Space,
    DedupeKey,
    DeliveryStatus,
    OutboxEntry,
    InboxKey,
    InboxEntry,
    ReadMarker,
    RootPublishPayload,
);

// OwnerState lives in owner_state_crdt to keep CRDT semantics together;
// its CanonicalPayload impl is registered here alongside all Phase 2 wire types.
impl crate::owner_state_crypto::sealed::CanonicalPayloadSealed
    for crate::owner_state_crdt::OwnerState
{
}
impl crate::owner_state_crypto::CanonicalPayload for crate::owner_state_crdt::OwnerState {}

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
        let c = ContentId::from_bytes([0u8; 32]);
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
        let c = ContentId::from_bytes([0xef; 32]);
        let mut bytes = Vec::new();
        into_writer(&c, &mut bytes).unwrap();
        let recovered: ContentId = from_reader(&bytes[..]).unwrap();
        assert_eq!(c, recovered);
        assert_eq!(bytes.len(), 34);
        assert_eq!(bytes[0], 0x58);
        assert_eq!(bytes[1], 0x20);
    }

    #[test]
    fn dm_content_key_serializes_as_bstr_32() {
        use ciborium::into_writer;
        let k = DmContentKey::new([0u8; 32]);
        let mut bytes = Vec::new();
        into_writer(&k, &mut bytes).unwrap();
        // bstr(32): 0x58 0x20 || <32 bytes> = 34 bytes total.
        assert_eq!(bytes.len(), 34);
        assert_eq!(bytes[0], 0x58);
        assert_eq!(bytes[1], 0x20);
    }

    #[test]
    fn dm_content_key_round_trip() {
        use ciborium::{from_reader, into_writer};
        let k = DmContentKey::new([0xab; 32]);
        let mut bytes = Vec::new();
        into_writer(&k, &mut bytes).unwrap();
        let recovered: DmContentKey = from_reader(&bytes[..]).unwrap();
        assert_eq!(k.as_bytes(), recovered.as_bytes());
    }

    #[test]
    fn dm_content_key_debug_redacts_bytes() {
        let k = DmContentKey::new([0xab; 32]);
        let s = format!("{:?}", k);
        // No raw byte values, no hex, no decimal — must be a fixed redacted form.
        assert!(!s.contains("0xab"));
        assert!(!s.contains("171")); // 0xab as decimal
        assert!(s.contains("redacted") || s.contains("REDACTED") || s.contains("***"));
    }

    #[test]
    fn dm_content_key_zeroized_on_drop() {
        // Use ZeroizeOnDrop's invariant: dropping the wrapper zeros the
        // underlying [u8; 32]. We can't easily observe the freed memory,
        // but we can verify the trait is implemented by constraining a
        // generic function.
        fn assert_zeroize_on_drop<T: zeroize::ZeroizeOnDrop>() {}
        assert_zeroize_on_drop::<DmContentKey>();
    }

    #[test]
    fn device_identity_hash_serializes_as_bstr_16() {
        use ciborium::into_writer;
        let d = DeviceIdentityHash([0u8; 16]);
        let mut bytes = Vec::new();
        into_writer(&d, &mut bytes).unwrap();
        // bstr(16): 0x50 || <16 bytes> = 17 bytes total.
        assert_eq!(bytes.len(), 17);
        assert_eq!(bytes[0], 0x50);
    }

    #[test]
    fn device_identity_hash_round_trip() {
        use ciborium::{from_reader, into_writer};
        let d = DeviceIdentityHash([0xcd; 16]);
        let mut bytes = Vec::new();
        into_writer(&d, &mut bytes).unwrap();
        let recovered: DeviceIdentityHash = from_reader(&bytes[..]).unwrap();
        assert_eq!(d, recovered);
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

    /// Regression for PR #73 round 2 review: ReticulumDest must encode
    /// as CBOR bstr (major type 2), not as a CBOR array of u8 (major
    /// type 4). Pin the wire bytes so any future serde-attribute change
    /// that breaks bstr emission fails loudly.
    #[test]
    fn reticulum_dest_emits_cbor_bstr() {
        let d = ReticulumDest(vec![0xde, 0xad, 0xbe, 0xef]);
        let mut bytes = Vec::new();
        into_writer(&d, &mut bytes).unwrap();
        // CBOR bstr len=4: 0x44 (major type 2, length 4) + the 4 bytes.
        // CBOR array len=4 would be 0x84 + four u8-encoded values
        // (each itself another byte or two), so this byte pattern would
        // never match an array encoding.
        assert_eq!(bytes, vec![0x44, 0xde, 0xad, 0xbe, 0xef]);
        let recovered: ReticulumDest = from_reader(&bytes[..]).unwrap();
        assert_eq!(d, recovered);
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

/// The unified Space CRDT entry — see ZEB-206 spec §"Space — unified
/// entry in owner-state CRDT".
///
/// Wire-format note: every field is renamed to a 2-char code so all 14
/// keys at this nesting level have identical encoded length (CBOR
/// text(2) = 3 bytes per key). Mixing 1-char and 2-char renames here
/// would re-introduce the same-length-keys violation Hlc had before
/// PR #72 round 3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Space {
    #[serde(rename = "id")]
    pub id: SpaceId,
    #[serde(rename = "kn")]
    pub kind: SpaceKind,
    #[serde(rename = "pa")]
    pub parent: Option<SpaceId>,
    #[serde(rename = "ci")]
    pub community_id: Option<SpaceId>,
    #[serde(rename = "nm")]
    pub name: String,
    #[serde(rename = "tr")]
    pub transport: Option<TransportBinding>,
    #[serde(rename = "me")]
    pub members: Vec<OwnerAddr>,
    #[serde(rename = "cn")]
    pub custom_name: Option<String>,
    #[serde(rename = "np")]
    pub notification_pref: Option<NotificationPref>,
    #[serde(rename = "la")]
    pub left_at: Option<Hlc>,
    #[serde(rename = "ca")]
    pub created_at: Hlc,
    #[serde(rename = "ua")]
    pub updated_at: Hlc,

    /// Per-DM-Space symmetric content key (ChaCha20-Poly1305).
    /// MUST be Some for kind ∈ {dm, group-dm}; MUST be None otherwise.
    /// (Enforcement via validate_invariants lands in Task 3.)
    /// Wire format: bstr(32) inside the Space CBOR map under key "ck".
    /// In-memory: zeroized on drop via DmContentKey's ZeroizeOnDrop impl.
    /// See ZEB-216 §"Space struct additions (Phase 1)".
    #[serde(rename = "ck", skip_serializing_if = "Option::is_none", default)]
    pub content_key: Option<DmContentKey>,

    /// Historical content keys retained from past dedupe-collision merges.
    /// Used as fallback decryption for messages encrypted under a now-
    /// superseded key. Bounded by MAX_PRIOR_CONTENT_KEYS = 16.
    /// (Validation lands in Task 3; cap-rule merge in Task 7.)
    /// MUST NOT contain the current `content_key`.
    /// MUST be empty for non-DM kinds.
    /// Wire format: array of bstr(32) under key "pk".
    #[serde(rename = "pk", skip_serializing_if = "Vec::is_empty", default)]
    pub prior_content_keys: Vec<DmContentKey>,
}

/// Per-kind dedupe key — what the CRDT uses to identify "same Space"
/// across two devices' independent writes. See ZEB-206 spec
/// §"Dedupe key per Space kind".
///
/// Also used as the AAD seed for DM encryption (see `dm_crypto::compute_aad`):
/// canonical CBOR of the dedupe key is stable across cross-SpaceId collapses.
/// Adjacently tagged: tag key `"tg"` (2 chars), content key `"vl"` (2 chars
/// to match the same-length-keys precondition at this nesting level);
/// variant codes `"n"/"i"/"t"/"s"` (1-char values, not keys).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "tg", content = "vl")]
pub enum DedupeKey {
    /// Folders never dedupe — same name on different devices = different folders.
    #[serde(rename = "n")]
    None,
    /// Community / channel / group-dm: by Space.id.
    #[serde(rename = "i")]
    Id(SpaceId),
    /// public-channel: by Zenoh topic string.
    #[serde(rename = "t")]
    Topic(String),
    /// dm: by sorted members (immutable 2-member set).
    #[serde(rename = "s")]
    SortedMembers(Vec<OwnerAddr>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvariantError(pub String);

impl Space {
    /// Validate the kind-specific shape invariants. Run on every write
    /// (locally produced) and after merge (incoming). See ZEB-206 spec
    /// §"Invariants".
    pub fn validate_invariants(&self) -> Result<(), InvariantError> {
        match self.kind {
            SpaceKind::Folder => {
                if self.transport.is_some() {
                    return Err(InvariantError("folder must have transport=None".into()));
                }
                if !self.members.is_empty() {
                    return Err(InvariantError("folder must have members=[]".into()));
                }
            }
            SpaceKind::Channel => {
                if self.community_id.is_none() {
                    return Err(InvariantError("channel must have community_id".into()));
                }
                match &self.transport {
                    Some(TransportBinding::Zenoh { .. }) => {}
                    _ => return Err(InvariantError("channel must have zenoh transport".into())),
                }
            }
            SpaceKind::PublicChannel => {
                if self.community_id.is_some() {
                    return Err(InvariantError(
                        "public-channel must have community_id=None".into(),
                    ));
                }
                match &self.transport {
                    Some(TransportBinding::Zenoh { .. }) => {}
                    _ => {
                        return Err(InvariantError(
                            "public-channel must have zenoh transport".into(),
                        ))
                    }
                }
            }
            SpaceKind::Dm => {
                // Distinct-member check: dedupe_key() collapses members
                // via SortedMembers, so [alice, alice] would otherwise
                // pass len==2 here yet hash to a 1-member dedupe key.
                // Sorted-ascending check: independently-created DM
                // Spaces with members in different orders would produce
                // different canonical CBOR bytes (and thus different
                // cipher_cids) until CRDT dedup converges. Forcing
                // sorted-ascending makes the wire format deterministic
                // by construction.
                let unique: BTreeSet<&OwnerAddr> = self.members.iter().collect();
                if self.members.len() != 2 || unique.len() != 2 {
                    return Err(InvariantError(format!(
                        "dm must have exactly 2 distinct members, got {} ({} unique)",
                        self.members.len(),
                        unique.len()
                    )));
                }
                if !self.members.windows(2).all(|w| w[0] < w[1]) {
                    return Err(InvariantError(
                        "dm members must be sorted ascending (canonical CBOR determinism)".into(),
                    ));
                }
                match &self.transport {
                    Some(TransportBinding::Reticulum { .. }) => {}
                    _ => return Err(InvariantError("dm must have reticulum transport".into())),
                }
            }
            SpaceKind::GroupDm => {
                let unique: BTreeSet<&OwnerAddr> = self.members.iter().collect();
                if !(3..=16).contains(&self.members.len()) || unique.len() != self.members.len() {
                    return Err(InvariantError(format!(
                        "group-dm must have 3..=16 distinct members, got {} ({} unique)",
                        self.members.len(),
                        unique.len()
                    )));
                }
                if !self.members.windows(2).all(|w| w[0] < w[1]) {
                    return Err(InvariantError(
                        "group-dm members must be sorted ascending (canonical CBOR determinism)"
                            .into(),
                    ));
                }
                match &self.transport {
                    Some(TransportBinding::Reticulum { .. }) => {}
                    _ => {
                        return Err(InvariantError(
                            "group-dm must have reticulum transport".into(),
                        ))
                    }
                }
            }
            SpaceKind::Community => {
                // community must have a corresponding CommunityMembership CRDT
                // — that lives in Sub-C scope, not validated here.
            }
        }

        // Content-key invariants per ZEB-216 §"Validate invariants extension".
        match self.kind {
            SpaceKind::Dm | SpaceKind::GroupDm => {
                if self.content_key.is_none() {
                    return Err(InvariantError(format!(
                        "{:?} kind requires content_key",
                        self.kind
                    )));
                }
            }
            _ => {
                if self.content_key.is_some() {
                    return Err(InvariantError(format!(
                        "{:?} kind must not have content_key",
                        self.kind
                    )));
                }
                if !self.prior_content_keys.is_empty() {
                    return Err(InvariantError(format!(
                        "{:?} kind must not have prior_content_keys",
                        self.kind
                    )));
                }
            }
        }

        if self.prior_content_keys.len() > MAX_PRIOR_CONTENT_KEYS {
            return Err(InvariantError(format!(
                "prior_content_keys.len()={} exceeds MAX_PRIOR_CONTENT_KEYS={}",
                self.prior_content_keys.len(),
                MAX_PRIOR_CONTENT_KEYS
            )));
        }

        if let Some(ck) = &self.content_key {
            if self
                .prior_content_keys
                .iter()
                .any(|p| p.as_bytes() == ck.as_bytes())
            {
                return Err(InvariantError(
                    "content_key must not appear in prior_content_keys".into(),
                ));
            }
        }

        Ok(())
    }

    /// Extract the dedupe key for this Space — see ZEB-206 spec
    /// §"Dedupe key per Space kind".
    pub fn dedupe_key(&self) -> DedupeKey {
        match self.kind {
            SpaceKind::Folder => DedupeKey::None,
            SpaceKind::Community | SpaceKind::Channel | SpaceKind::GroupDm => {
                DedupeKey::Id(self.id)
            }
            SpaceKind::PublicChannel => match &self.transport {
                Some(TransportBinding::Zenoh { topic }) => DedupeKey::Topic(topic.clone()),
                _ => DedupeKey::Id(self.id), // invariant violation; should be caught by validate_invariants
            },
            SpaceKind::Dm => {
                let mut sorted = self.members.clone();
                sorted.sort();
                DedupeKey::SortedMembers(sorted)
            }
        }
    }
}

use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryStatus {
    #[serde(rename = "p")]
    Pending,
    #[serde(rename = "r")]
    Partial,
    #[serde(rename = "c")]
    Complete,
    #[serde(rename = "x")]
    Expired,
}

/// OutboxEntry — persistent sent-DM log entry.
///
/// Wire-format note: 7 fields, all 2-char renames, same-length-keys
/// satisfied. `delivered_to` is `BTreeSet` so canonical CBOR encoding
/// is deterministic (BTreeSet serializes in `K::Ord` order which is
/// bytewise for `OwnerAddr` since it's a `bstr(16)`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxEntry {
    #[serde(rename = "id")]
    pub id: OutboxEntryId,
    #[serde(rename = "sp")]
    pub space_id: SpaceId,
    #[serde(rename = "rc")]
    pub recipient_owners: Vec<OwnerAddr>,
    #[serde(rename = "mc")]
    pub message_cid: ContentId,
    #[serde(rename = "ca")]
    pub created_at: Hlc,
    #[serde(rename = "dl")]
    pub delivered_to: BTreeSet<OwnerAddr>,
    #[serde(rename = "ds")]
    pub delivery_status: DeliveryStatus,
}

impl OutboxEntry {
    /// Compute the delivery_status that *should* hold given the current
    /// `delivered_to` set + recipient list + the optional 30-day-expired
    /// flag from the caller (Phase 3 owns the wall-clock; Phase 2 just
    /// reflects the state).
    ///
    /// Rules (ZEB-206 spec):
    /// - `Pending` → no acks yet (`delivered_to` empty)
    /// - `Partial` → some recipients acked, others outstanding
    /// - `Complete` → all `recipient_owners` are in `delivered_to`
    /// - `Expired` → `is_expired` flag set AND at least one recipient
    ///   not in `delivered_to`
    pub fn compute_status(&self, is_expired: bool) -> DeliveryStatus {
        let recipients_in_set: BTreeSet<&OwnerAddr> = self.recipient_owners.iter().collect();
        let all_acked = recipients_in_set
            .iter()
            .all(|r| self.delivered_to.contains(*r));
        if all_acked {
            DeliveryStatus::Complete
        } else if is_expired {
            DeliveryStatus::Expired
        } else if self.delivered_to.is_empty() {
            DeliveryStatus::Pending
        } else {
            DeliveryStatus::Partial
        }
    }
}

/// InboxEntry composite lookup key. `(space_id, message_cid)` is the
/// upsert key for inbox writes — see ZEB-206 spec §"Idempotency".
///
/// `PartialOrd`/`Ord` are implemented manually because
/// `harmony_content::cid::ContentId` does not derive those traits.
/// Ordering is bytewise: first by `space_id.0`, then by
/// `message_cid.to_bytes()` on the 32-byte wire representation.
///
/// Note: this is NOT the same order as Phase 3a's ContentId([u8; 32]),
/// which compared 32 raw BLAKE3 bytes. The byte layout changed (BLAKE3
/// → SHA-256-MSB-truncated, plus the structured header), so the order
/// of the same logical InboxKey changed too. Acceptable because Phase
/// 3a's stub-only sync left no persisted inbox entries — see Phase 3b
/// spec §"Wire format" for the v1-stub-only rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InboxKey {
    #[serde(rename = "sp")]
    pub space_id: SpaceId,
    #[serde(rename = "mc")]
    pub message_cid: ContentId,
}

impl PartialOrd for InboxKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for InboxKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.space_id.cmp(&other.space_id).then_with(|| {
            self.message_cid
                .to_bytes()
                .cmp(&other.message_cid.to_bytes())
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxEntry {
    #[serde(rename = "sp")]
    pub space_id: SpaceId,
    #[serde(rename = "mc")]
    pub message_cid: ContentId,
    #[serde(rename = "fr")]
    pub from: OwnerAddr,
    #[serde(rename = "ra")]
    pub received_at: Hlc,
}

impl InboxEntry {
    pub fn key(&self) -> InboxKey {
        InboxKey {
            space_id: self.space_id,
            message_cid: self.message_cid,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadMarker {
    #[serde(rename = "sp")]
    pub space_id: SpaceId,
    #[serde(rename = "lr")]
    pub last_read_at: Hlc,
}

#[cfg(test)]
mod inbox_tests {
    use super::*;

    #[test]
    fn key_extracts_composite() {
        let e = InboxEntry {
            space_id: SpaceId([1u8; 16]),
            message_cid: ContentId::from_bytes([2u8; 32]),
            from: OwnerAddr([3u8; 16]),
            received_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "d".into(),
            },
        };
        assert_eq!(
            e.key(),
            InboxKey {
                space_id: SpaceId([1u8; 16]),
                message_cid: ContentId::from_bytes([2u8; 32])
            }
        );
    }

    #[test]
    fn inbox_entry_round_trip() {
        let e = InboxEntry {
            space_id: SpaceId([7u8; 16]),
            message_cid: ContentId::from_bytes([8u8; 32]),
            from: OwnerAddr([9u8; 16]),
            received_at: Hlc {
                wall_ms: 50,
                logical: 1,
                device_id: "alice".into(),
            },
        };
        let mut bytes = Vec::new();
        ciborium::into_writer(&e, &mut bytes).unwrap();
        let recovered: InboxEntry = ciborium::from_reader(&bytes[..]).unwrap();
        assert_eq!(e, recovered);
    }
}

#[cfg(test)]
mod marker_tests {
    use super::*;

    #[test]
    fn read_marker_round_trip() {
        let m = ReadMarker {
            space_id: SpaceId([5u8; 16]),
            last_read_at: Hlc {
                wall_ms: 999,
                logical: 7,
                device_id: "d".into(),
            },
        };
        let mut bytes = Vec::new();
        ciborium::into_writer(&m, &mut bytes).unwrap();
        let recovered: ReadMarker = ciborium::from_reader(&bytes[..]).unwrap();
        assert_eq!(m, recovered);
    }

    #[test]
    fn root_publish_payload_round_trip() {
        let p = RootPublishPayload {
            root_cid: ContentId::from_bytes([0xAA; 32]),
            at: Hlc {
                wall_ms: 12345,
                logical: 7,
                device_id: "alice".into(),
            },
        };
        let mut bytes = Vec::new();
        ciborium::into_writer(&p, &mut bytes).unwrap();
        let recovered: RootPublishPayload = ciborium::from_reader(&bytes[..]).unwrap();
        assert_eq!(p, recovered);
    }
}

#[cfg(test)]
mod outbox_tests {
    use super::*;

    fn hlc(w: u64) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: "test".into(),
        }
    }

    fn entry(recipients: Vec<u8>, delivered: Vec<u8>) -> OutboxEntry {
        OutboxEntry {
            id: OutboxEntryId([1u8; 16]),
            space_id: SpaceId([2u8; 16]),
            recipient_owners: recipients.into_iter().map(|i| OwnerAddr([i; 16])).collect(),
            message_cid: ContentId::from_bytes([3u8; 32]),
            created_at: hlc(100),
            delivered_to: delivered.into_iter().map(|i| OwnerAddr([i; 16])).collect(),
            delivery_status: DeliveryStatus::Pending,
        }
    }

    #[test]
    fn status_pending_when_no_acks() {
        let e = entry(vec![1, 2, 3], vec![]);
        assert_eq!(e.compute_status(false), DeliveryStatus::Pending);
    }

    #[test]
    fn status_partial_when_some_acked() {
        let e = entry(vec![1, 2, 3], vec![1]);
        assert_eq!(e.compute_status(false), DeliveryStatus::Partial);
    }

    #[test]
    fn status_complete_when_all_acked() {
        let e = entry(vec![1, 2, 3], vec![1, 2, 3]);
        assert_eq!(e.compute_status(false), DeliveryStatus::Complete);
    }

    #[test]
    fn status_expired_only_when_partial_and_flag_set() {
        let e = entry(vec![1, 2, 3], vec![1]);
        assert_eq!(e.compute_status(true), DeliveryStatus::Expired);
        // Complete trumps expired.
        let done = entry(vec![1, 2, 3], vec![1, 2, 3]);
        assert_eq!(done.compute_status(true), DeliveryStatus::Complete);
    }

    #[test]
    fn outbox_entry_round_trip() {
        let e = entry(vec![1, 2, 3], vec![1]);
        let mut bytes = Vec::new();
        ciborium::into_writer(&e, &mut bytes).unwrap();
        let recovered: OutboxEntry = ciborium::from_reader(&bytes[..]).unwrap();
        assert_eq!(e, recovered);
    }
}

#[cfg(test)]
mod space_tests {
    use super::*;

    fn hlc(w: u64) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: "test".into(),
        }
    }

    fn folder() -> Space {
        Space {
            id: SpaceId([1u8; 16]),
            kind: SpaceKind::Folder,
            parent: None,
            community_id: None,
            name: "Work".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(1),
            updated_at: hlc(1),
            content_key: None,
            prior_content_keys: vec![],
        }
    }

    #[test]
    fn folder_invariants_pass() {
        assert_eq!(folder().validate_invariants(), Ok(()));
    }

    #[test]
    fn folder_with_transport_rejects() {
        let mut f = folder();
        f.transport = Some(TransportBinding::Zenoh { topic: "x".into() });
        assert!(f.validate_invariants().is_err());
    }

    #[test]
    fn folder_with_members_rejects() {
        let mut f = folder();
        f.members = vec![OwnerAddr([0u8; 16])];
        assert!(f.validate_invariants().is_err());
    }

    #[test]
    fn dm_must_have_exactly_two_members() {
        let mk_dm = |n_members: usize| Space {
            id: SpaceId([2u8; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "DM".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: (0..n_members).map(|i| OwnerAddr([i as u8; 16])).collect(),
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(1),
            updated_at: hlc(1),
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: vec![],
        };
        assert!(mk_dm(0).validate_invariants().is_err());
        assert!(mk_dm(1).validate_invariants().is_err());
        assert!(mk_dm(2).validate_invariants().is_ok());
        assert!(mk_dm(3).validate_invariants().is_err());
    }

    #[test]
    fn group_dm_caps_at_16() {
        let mk = |n: usize| Space {
            id: SpaceId([3u8; 16]),
            kind: SpaceKind::GroupDm,
            parent: None,
            community_id: None,
            name: "Group".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: (0..n).map(|i| OwnerAddr([i as u8; 16])).collect(),
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(1),
            updated_at: hlc(1),
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: vec![],
        };
        assert!(mk(2).validate_invariants().is_err());
        assert!(mk(3).validate_invariants().is_ok());
        assert!(mk(16).validate_invariants().is_ok());
        assert!(mk(17).validate_invariants().is_err());
    }

    /// Regression for PR #73 round 2 review: a DM with two identical
    /// members ([alice, alice]) passes the len==2 check today but
    /// dedupe_key collapses it to one member. Reject up front.
    #[test]
    fn dm_rejects_duplicate_members() {
        let mut d = Space {
            id: SpaceId([2u8; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "DM".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: vec![OwnerAddr([1u8; 16]), OwnerAddr([1u8; 16])],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(1),
            updated_at: hlc(1),
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: vec![],
        };
        assert!(d.validate_invariants().is_err());
        // Distinct members still pass.
        d.members = vec![OwnerAddr([1u8; 16]), OwnerAddr([2u8; 16])];
        assert!(d.validate_invariants().is_ok());
    }

    /// Regression for PR #73 Greptile P2: independently-created DM
    /// Spaces with members in different orders would produce different
    /// canonical CBOR bytes. Reject unsorted at construction time so
    /// wire format is deterministic by construction.
    #[test]
    fn dm_rejects_unsorted_members() {
        let mut d = Space {
            id: SpaceId([2u8; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "DM".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            // Reverse order — bob > alice but listed bob-first.
            members: vec![OwnerAddr([2u8; 16]), OwnerAddr([1u8; 16])],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(1),
            updated_at: hlc(1),
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: vec![],
        };
        assert!(d.validate_invariants().is_err());
        // Sorted ascending passes.
        d.members = vec![OwnerAddr([1u8; 16]), OwnerAddr([2u8; 16])];
        assert!(d.validate_invariants().is_ok());
    }

    #[test]
    fn group_dm_rejects_unsorted_members() {
        let mut g = Space {
            id: SpaceId([3u8; 16]),
            kind: SpaceKind::GroupDm,
            parent: None,
            community_id: None,
            name: "Group".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: vec![
                OwnerAddr([3u8; 16]),
                OwnerAddr([1u8; 16]),
                OwnerAddr([2u8; 16]),
            ],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(1),
            updated_at: hlc(1),
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: vec![],
        };
        assert!(g.validate_invariants().is_err());
        g.members = vec![
            OwnerAddr([1u8; 16]),
            OwnerAddr([2u8; 16]),
            OwnerAddr([3u8; 16]),
        ];
        assert!(g.validate_invariants().is_ok());
    }

    #[test]
    fn group_dm_rejects_duplicate_members() {
        let g = Space {
            id: SpaceId([3u8; 16]),
            kind: SpaceKind::GroupDm,
            parent: None,
            community_id: None,
            name: "Group".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            // 4 entries but only 3 distinct — len passes 3..=16 but
            // dedupe_key would collapse to 3 sorted members.
            members: vec![
                OwnerAddr([1u8; 16]),
                OwnerAddr([2u8; 16]),
                OwnerAddr([3u8; 16]),
                OwnerAddr([1u8; 16]),
            ],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(1),
            updated_at: hlc(1),
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: vec![],
        };
        assert!(g.validate_invariants().is_err());
    }

    #[test]
    fn channel_must_have_community_id_and_zenoh_transport() {
        let mk_channel =
            |community_id: Option<SpaceId>, transport: Option<TransportBinding>| Space {
                id: SpaceId([4u8; 16]),
                kind: SpaceKind::Channel,
                parent: None,
                community_id,
                name: "general".into(),
                transport,
                members: vec![],
                custom_name: None,
                notification_pref: None,
                left_at: None,
                created_at: hlc(1),
                updated_at: hlc(1),
                content_key: None,
                prior_content_keys: vec![],
            };
        // Missing community_id → reject.
        assert!(
            mk_channel(None, Some(TransportBinding::Zenoh { topic: "t".into() }))
                .validate_invariants()
                .is_err()
        );
        // Wrong transport → reject.
        assert!(mk_channel(
            Some(SpaceId([5u8; 16])),
            Some(TransportBinding::Reticulum {
                participants: vec![]
            })
        )
        .validate_invariants()
        .is_err());
        // Both correct → pass.
        assert!(mk_channel(
            Some(SpaceId([5u8; 16])),
            Some(TransportBinding::Zenoh { topic: "t".into() })
        )
        .validate_invariants()
        .is_ok());
    }

    #[test]
    fn dedupe_key_folder_is_none() {
        assert_eq!(folder().dedupe_key(), DedupeKey::None);
    }

    #[test]
    fn dedupe_key_dm_sorts_members() {
        let mk_dm = |m: Vec<OwnerAddr>| Space {
            id: SpaceId([6u8; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "DM".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: m,
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(1),
            updated_at: hlc(1),
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: vec![],
        };
        let a = OwnerAddr([1u8; 16]);
        let b = OwnerAddr([2u8; 16]);
        // Both orderings produce the same dedupe key.
        assert_eq!(
            mk_dm(vec![a, b]).dedupe_key(),
            mk_dm(vec![b, a]).dedupe_key()
        );
    }

    #[test]
    fn dedupe_key_public_channel_is_topic() {
        let pc = Space {
            id: SpaceId([7u8; 16]),
            kind: SpaceKind::PublicChannel,
            parent: None,
            community_id: None,
            name: "rust".into(),
            transport: Some(TransportBinding::Zenoh {
                topic: "harmony/public/rust".into(),
            }),
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(1),
            updated_at: hlc(1),
            content_key: None,
            prior_content_keys: vec![],
        };
        assert_eq!(
            pc.dedupe_key(),
            DedupeKey::Topic("harmony/public/rust".into())
        );
    }

    #[test]
    fn space_round_trip_preserves_all_fields() {
        let mut s = folder();
        s.parent = Some(SpaceId([99u8; 16]));
        s.custom_name = Some("My Work".into());
        s.notification_pref = Some(NotificationPref::Mentions);
        let mut bytes = Vec::new();
        ciborium::into_writer(&s, &mut bytes).unwrap();
        let recovered: Space = ciborium::from_reader(&bytes[..]).unwrap();
        assert_eq!(s, recovered);
    }

    #[test]
    fn dm_must_have_content_key() {
        let mut d = Space {
            id: SpaceId([1; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "x".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            updated_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            content_key: None, // ← invariant violation
            prior_content_keys: vec![],
        };
        assert!(d.validate_invariants().is_err());
        d.content_key = Some(DmContentKey::new([0xaa; 32]));
        assert!(d.validate_invariants().is_ok());
    }

    #[test]
    fn group_dm_must_have_content_key() {
        let mut d = Space {
            id: SpaceId([1; 16]),
            kind: SpaceKind::GroupDm,
            parent: None,
            community_id: None,
            name: "x".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: (0u8..3).map(|i| OwnerAddr([i; 16])).collect(),
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            updated_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            content_key: None,
            prior_content_keys: vec![],
        };
        assert!(d.validate_invariants().is_err());
        d.content_key = Some(DmContentKey::new([0xaa; 32]));
        assert!(d.validate_invariants().is_ok());
    }

    #[test]
    fn folder_rejects_content_key() {
        let f = Space {
            id: SpaceId([1; 16]),
            kind: SpaceKind::Folder,
            parent: None,
            community_id: None,
            name: "Work".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            updated_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            content_key: Some(DmContentKey::new([0xaa; 32])), // ← invariant violation
            prior_content_keys: vec![],
        };
        assert!(f.validate_invariants().is_err());
    }

    #[test]
    fn folder_rejects_prior_content_keys() {
        let f = Space {
            id: SpaceId([1; 16]),
            kind: SpaceKind::Folder,
            parent: None,
            community_id: None,
            name: "Work".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            updated_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            content_key: None,
            prior_content_keys: vec![DmContentKey::new([0xbb; 32])], // ← invariant violation
        };
        assert!(f.validate_invariants().is_err());
    }

    #[test]
    fn dm_content_key_in_prior_list_rejects() {
        let dup = DmContentKey::new([0xaa; 32]);
        let d = Space {
            id: SpaceId([1; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "x".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            updated_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            content_key: Some(dup.clone()),
            prior_content_keys: vec![dup], // ← same as content_key — violation
        };
        assert!(d.validate_invariants().is_err());
    }

    #[test]
    fn dm_prior_content_keys_cap_exceeded_rejects() {
        let d = Space {
            id: SpaceId([1; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "x".into(),
            transport: Some(TransportBinding::Reticulum {
                participants: vec![],
            }),
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            updated_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: (0u8..(MAX_PRIOR_CONTENT_KEYS as u8 + 1))
                .map(|i| DmContentKey::new([i; 32]))
                .collect(),
        };
        assert!(d.validate_invariants().is_err());
    }

    #[test]
    fn space_dm_with_content_key_round_trip() {
        use ciborium::{from_reader, into_writer};
        let s = Space {
            id: SpaceId([1u8; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "alice-bob".to_string(),
            transport: None,
            members: vec![OwnerAddr([1u8; 16]), OwnerAddr([2u8; 16])],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "dev".into(),
            },
            updated_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "dev".into(),
            },
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: vec![DmContentKey::new([0xbb; 32])],
        };
        let mut bytes = Vec::new();
        into_writer(&s, &mut bytes).unwrap();
        let recovered: Space = from_reader(&bytes[..]).unwrap();
        assert_eq!(s, recovered);
    }

    #[test]
    fn space_folder_omits_content_key_keys_in_cbor() {
        use ciborium::into_writer;
        let s = Space {
            id: SpaceId([1u8; 16]),
            kind: SpaceKind::Folder,
            parent: None,
            community_id: None,
            name: "Work".to_string(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "dev".into(),
            },
            updated_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "dev".into(),
            },
            content_key: None,
            prior_content_keys: vec![],
        };
        let mut bytes = Vec::new();
        into_writer(&s, &mut bytes).unwrap();
        // Folder serialization MUST NOT contain the "ck" or "pk" map keys —
        // the skip_serializing_if attributes elide them. Crude check: the
        // text strings "ck" and "pk" should not appear in the encoded bytes.
        let needle_ck = b"ck";
        let needle_pk = b"pk";
        assert!(
            !bytes.windows(2).any(|w| w == needle_ck),
            "Folder serialization unexpectedly contains 'ck' key"
        );
        assert!(
            !bytes.windows(2).any(|w| w == needle_pk),
            "Folder serialization unexpectedly contains 'pk' key"
        );
    }
}

#[cfg(test)]
mod owner_device_entry_deserialize_tests {
    use super::*;

    /// Parallel struct that bypasses `OwnerDeviceEntry`'s `deserialize_with`
    /// hook so the test can plant a malformed `devices` Vec on the wire and
    /// assert the real type re-normalizes on load.
    #[derive(Serialize)]
    struct RawOwnerDeviceEntry {
        #[serde(rename = "v")]
        v: Vec<DeviceIdentityHash>,
        #[serde(rename = "l")]
        l: Hlc,
    }

    fn hlc(ms: u64) -> Hlc {
        Hlc {
            wall_ms: ms,
            logical: 0,
            device_id: "d".into(),
        }
    }

    #[test]
    fn deserialize_normalizes_unsorted_duplicated_oversized_devices() {
        // Build a payload with all three pathologies at once:
        //   (a) duplicates    — [42, 42, 42, ...]
        //   (b) unsorted      — values descending below a smaller hash
        //   (c) oversized     — 100 entries > MAX_DEVICES_PER_OWNER (32)
        // After normalization the result must be sorted, deduped, and capped
        // at 32 — anything else breaks binary_search in
        // resolve_link_origin_owner (Phase 3b).
        let mut malformed: Vec<DeviceIdentityHash> = Vec::with_capacity(100);
        // 50 identical entries of hash 0xff...
        for _ in 0..50 {
            malformed.push(DeviceIdentityHash([0xff; 16]));
        }
        // Then 50 distinct descending hashes 0..50 (so total length = 100,
        // and the hashes are NOT in sorted order).
        for i in (0..50u8).rev() {
            malformed.push(DeviceIdentityHash([i; 16]));
        }

        let raw = RawOwnerDeviceEntry {
            v: malformed,
            l: hlc(7),
        };

        let mut bytes = Vec::new();
        ciborium::into_writer(&raw, &mut bytes).expect("encode raw");

        let entry: OwnerDeviceEntry = ciborium::from_reader(&bytes[..]).expect("decode entry");

        // Capped at MAX_DEVICES_PER_OWNER.
        assert_eq!(entry.devices.len(), MAX_DEVICES_PER_OWNER);
        // Sorted ascending — required for binary_search.
        assert!(entry.devices.windows(2).all(|w| w[0] <= w[1]));
        // Deduped — no consecutive equal entries (and given sorted, no dups
        // anywhere).
        assert!(entry.devices.windows(2).all(|w| w[0] != w[1]));
        // Lex-smallest survives truncation: the smallest hashes are
        // [0; 16], [1; 16], ..., so position 0 must be [0; 16].
        assert_eq!(entry.devices[0], DeviceIdentityHash([0; 16]));
        // learned_at preserved from the wire.
        assert_eq!(entry.learned_at, hlc(7));
    }
}
