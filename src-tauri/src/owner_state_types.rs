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
pub(crate) fn serialize_bytes_as_bstr<const N: usize, S>(
    b: &[u8; N],
    s: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    s.serialize_bytes(b)
}

/// Helper: serialize a `Vec<u8>` as CBOR bstr (major type 2). Used by
/// variable-length opaque-bytes wrapper types so they don't accidentally
/// encode as a CBOR array of u8 (major type 4).
///
/// Crate-public so DM wire types (`dm_envelope::MessagePayload.body`)
/// and future Phase 2/3b modules can reuse the same byte-efficient
/// encoding without redefining the helper. The bstr form is one CBOR
/// header byte plus the raw bytes, vs. array-of-u8's two bytes per
/// byte once values exceed 0x17 — load-bearing for ciphertext-bearing
/// fields where overhead dominates packet size.
pub(crate) fn serialize_vec_as_bstr<S>(b: &[u8], s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    s.serialize_bytes(b)
}

/// Helper: deserialize a CBOR bstr into a `Vec<u8>`. Pair with
/// `serialize_vec_as_bstr`.
///
/// Crate-public alongside its serialize partner so DM wire types
/// (`dm_envelope::MessagePayload.body`) and future Phase 2/3b modules
/// can reuse the same bstr decoding contract.
pub(crate) fn deserialize_vec_from_bstr<'de, D>(d: D) -> Result<Vec<u8>, D::Error>
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

/// Helper: serialize a `Vec<Vec<u8>>` as a CBOR array of byte-strings
/// (one bstr per inner `Vec<u8>`). Pairs with
/// `deserialize_vec_of_vec_from_bstr`.
///
/// ZEB-369: targeted invite-only invites carry one X25519-sealed epoch-key
/// envelope per invitee device in `InviteEpochSnapshot.sealed_epoch_keys`.
/// Encoding each envelope as a bstr (major type 2) inside the outer array is
/// far more compact than the default `Vec<u8>` Serialize (which would emit an
/// array-of-u8, two bytes per byte once values exceed 0x17). Mirrors the
/// single-`Vec<u8>` `serialize_vec_as_bstr` one nesting level up.
pub(crate) fn serialize_vec_of_vec_as_bstr<S>(v: &[Vec<u8>], s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    use serde::ser::SerializeSeq;

    // Wrap each inner vec in a tiny newtype that overrides Serialize to emit a
    // bstr (not an array-of-u8). `SerializeSeq::serialize_element` requires
    // `T: Serialize`, so we can't pass the bare `&[u8]`.
    struct Bstr<'a>(&'a [u8]);
    impl serde::Serialize for Bstr<'_> {
        fn serialize<S2: Serializer>(&self, s: S2) -> Result<S2::Ok, S2::Error> {
            s.serialize_bytes(self.0)
        }
    }

    let mut seq = s.serialize_seq(Some(v.len()))?;
    for inner in v {
        seq.serialize_element(&Bstr(inner))?;
    }
    seq.end()
}

/// Helper: deserialize a CBOR array of byte-strings into a `Vec<Vec<u8>>`.
/// Pairs with `serialize_vec_of_vec_as_bstr`. Each element must be a CBOR
/// bstr (major type 2); the inner `deserialize_vec_from_bstr` enforces that.
pub(crate) fn deserialize_vec_of_vec_from_bstr<'de, D>(d: D) -> Result<Vec<Vec<u8>>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{SeqAccess, Visitor};
    use std::fmt;

    /// One element: a CBOR bstr → `Vec<u8>`, routed through the existing
    /// single-vec bstr visitor so the bstr-only contract is identical.
    struct InnerVec(Vec<u8>);
    impl<'de> Deserialize<'de> for InnerVec {
        fn deserialize<D2: Deserializer<'de>>(d: D2) -> Result<Self, D2::Error> {
            deserialize_vec_from_bstr(d).map(InnerVec)
        }
    }

    struct OuterVisitor;
    impl<'de> Visitor<'de> for OuterVisitor {
        type Value = Vec<Vec<u8>>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "a CBOR array of byte strings (major type 2)")
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            // CodeAnt (PR #286): `size_hint()` is attacker-influenced for
            // untrusted CBOR, so clamp the upfront allocation. The only consumer
            // (ZEB-369 `sealed_epoch_keys`) is validated to at most
            // MAX_ENROLLED_DEVICE_KEYS (32) entries at the invite-URL boundary;
            // the Vec still grows naturally if a valid caller exceeds the hint —
            // this only bounds the eagerly-reserved capacity.
            const VEC_OF_VEC_PREALLOC_CAP: usize = 32;
            let mut out: Vec<Vec<u8>> =
                Vec::with_capacity(seq.size_hint().unwrap_or(0).min(VEC_OF_VEC_PREALLOC_CAP));
            while let Some(InnerVec(v)) = seq.next_element::<InnerVec>()? {
                out.push(v);
            }
            Ok(out)
        }
    }

    d.deserialize_seq(OuterVisitor)
}

/// Helper: deserialize CBOR bstr into byte array.
pub(crate) fn deserialize_bytes_from_bstr<'de, const N: usize, D>(d: D) -> Result<[u8; N], D::Error>
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

/// Helper: serialize `Option<[u8; N]>` as CBOR bstr (major type 2)
/// when `Some`. Pair with `deserialize_optional_bytes_from_bstr` and
/// `#[serde(skip_serializing_if = "Option::is_none")]` on the field so
/// `None` cases omit the key entirely from canonical CBOR (preserving
/// wire-format byte-identity with earlier schema versions that didn't
/// have the field).
///
/// ZEB-280 (Sub-D Phase 3) adds Optional `library_identity_pub` and
/// `library_signature` fields to `LibraryDirectoryEntry`. Phase 1
/// entries (no wrapping sig) must serialize to byte-identical CBOR
/// when the new fields are `None` — see spec §4.1.
pub(crate) fn serialize_optional_bytes_as_bstr<const N: usize, S>(
    b: &Option<[u8; N]>,
    s: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    // `skip_serializing_if = "Option::is_none"` on the field guarantees
    // serializer is only called for `Some(...)` — but be defensive in
    // case a future caller forgets the attribute.
    match b {
        Some(arr) => s.serialize_bytes(arr),
        None => s.serialize_none(),
    }
}

/// Helper: deserialize CBOR bstr into `Option<[u8; N]>`. Returns
/// `Some(arr)` on a bstr, `None` on CBOR null OR absent field (the
/// absent-field case is handled by `#[serde(default)]` on the field).
/// Pair with `serialize_optional_bytes_as_bstr`.
pub(crate) fn deserialize_optional_bytes_from_bstr<'de, const N: usize, D>(
    d: D,
) -> Result<Option<[u8; N]>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Visitor;
    use std::fmt;

    struct OptBytesVisitor<const N: usize>;

    impl<'de, const N: usize> Visitor<'de> for OptBytesVisitor<N> {
        type Value = Option<[u8; N]>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            write!(formatter, "an optional byte string of length {}", N)
        }

        fn visit_none<E>(self) -> Result<Option<[u8; N]>, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Option<[u8; N]>, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_some<D2>(self, d: D2) -> Result<Option<[u8; N]>, D2::Error>
        where
            D2: Deserializer<'de>,
        {
            let arr: [u8; N] = crate::owner_state_types::deserialize_bytes_from_bstr(d)?;
            Ok(Some(arr))
        }

        fn visit_bytes<E>(self, value: &[u8]) -> Result<Option<[u8; N]>, E>
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
            Ok(Some(arr))
        }

        fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Option<[u8; N]>, E>
        where
            E: serde::de::Error,
        {
            self.visit_bytes(&v)
        }
    }

    d.deserialize_option(OptBytesVisitor::<N>)
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

/// Canonical exact byte-length of an ML-DSA-65 public key (the
/// `pq_dsa_pubkey` carried by a [`DeviceTunnelContact`]). Defined ONCE here
/// and referenced by every tunnel-contact validation site (ZEB-473). The
/// upstream `harmony_tunnel`/`harmony_identity` value (1952) is a private
/// `const` not re-exported across the public surface, so we pin it locally.
pub const ML_DSA_65_PUBKEY_LEN: usize = 1952;

/// Canonical exact byte-length of an ML-KEM-768 public (encapsulation) key
/// (the `pq_kem_pubkey` carried by a [`DeviceTunnelContact`]). See
/// [`ML_DSA_65_PUBKEY_LEN`].
pub const ML_KEM_768_PUBKEY_LEN: usize = 1184;

/// Maximum accepted byte-length of a tunnel contact's `home_relay_url`. A
/// relay URL longer than this is treated as malformed/abusive and rejected,
/// keeping replicated owner-state payloads bounded.
pub const MAX_TUNNEL_RELAY_URL_LEN: usize = 2048;

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

/// 32-byte symmetric key for community membership-topic encryption
/// (ChaCha20-Poly1305) at a specific epoch. Wire format: bstr(32).
/// In-memory: zeroized on drop. Debug redacts bytes to avoid log
/// leakage.
///
/// Per-epoch — rotates on every Kick/Leave via the `EpochRotation`
/// CRDT event. The current key for new outbound events lives in
/// `Space.current_epoch_key`; historical keys (for decrypting old
/// events) live in `Space.old_epoch_keys`.
///
/// Mirrors DmContentKey precisely — same shape, different purpose.
/// Distributed via per-recipient X25519-sealed ciphertexts on every
/// rotation; the initial key ships in `CommunityInvitePayload.epoch_snapshot`.
///
/// See ZEB-249 spec §"Data model — EpochKey".
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, zeroize::ZeroizeOnDrop)]
pub struct EpochKey(
    #[serde(
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    [u8; 32],
);

impl EpochKey {
    pub fn new(key: [u8; 32]) -> Self {
        Self(key)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Expose this key as a borrowed `chacha20poly1305::Key` for AEAD
    /// helpers. Borrows the underlying 32 bytes — the cipher must not
    /// outlive `self`. Used by `community_state_sync` for both random-
    /// nonce wire encryption and deterministic-nonce blob encryption
    /// at per-community granularity (no KeyTree derivation).
    pub fn as_chacha_key(&self) -> &chacha20poly1305::Key {
        chacha20poly1305::Key::from_slice(&self.0)
    }

    /// Generate a fresh random key from OS entropy. Used when
    /// creating a new community.
    pub fn random() -> Self {
        use rand::RngCore;
        use zeroize::Zeroizing;
        let mut k = Zeroizing::new([0u8; 32]);
        rand::rngs::OsRng.fill_bytes(k.as_mut());
        Self(*k)
    }
}

impl std::fmt::Debug for EpochKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EpochKey(<32 bytes redacted>)")
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

/// Per-device tunnel reachability + post-quantum public keys for a
/// friend's bound device (ZEB-473 / DM-over-iroh Move 1a). Parallel-indexed
/// onto `OwnerDeviceEntry.device_tunnel_contacts` — element i is the contact
/// for `OwnerDeviceEntry.devices[i]` when present, or `None` when the device
/// is known-by-hash but its reachability/PQ keys haven't been propagated yet.
///
/// This is a routing hint, NOT an identity authority: the per-device pubs in
/// `device_identity_pubs` remain the signature-verification source of truth.
/// Unlike `device_identity_pubs`, contacts legitimately change over time (a
/// peer's iroh node id, relay, or rotated PQ keys), so the CRDT merge rule is
/// last-writer-wins by the entry's `learned_at` HLC — never an InvariantFail.
///
/// Populated on friend handshake (ZEB-473): `peer_handshake_contact` derives a
/// contact from the signed reachability + PQ keys a peer advertises, and the
/// dialer-side handshake apply persists it parallel to the device. A device
/// known-by-hash but not yet handshaked still carries `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceTunnelContact {
    /// The peer device's iroh `EndpointId` (32-byte ed25519 public key the
    /// QUIC endpoint is keyed on) — the dial target for the PQ tunnel.
    #[serde(
        rename = "n",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub iroh_node_id: [u8; 32],

    /// The peer's home relay URL, if advertised (iroh relay hole-punch
    /// fallback). `None` when the peer didn't advertise one (direct-only).
    #[serde(rename = "r")]
    pub home_relay_url: Option<String>,

    /// ML-DSA-65 signature public key (post-quantum tunnel handshake auth).
    #[serde(
        rename = "d",
        serialize_with = "serialize_vec_as_bstr",
        deserialize_with = "deserialize_vec_from_bstr"
    )]
    pub pq_dsa_pubkey: Vec<u8>,

    /// ML-KEM-768 key-encapsulation public key (post-quantum tunnel KEX).
    #[serde(
        rename = "k",
        serialize_with = "serialize_vec_as_bstr",
        deserialize_with = "deserialize_vec_from_bstr"
    )]
    pub pq_kem_pubkey: Vec<u8>,
}

impl DeviceTunnelContact {
    /// Structural validity gate (ZEB-473): a contact is only dialable — and
    /// only worth persisting/replicating — when both PQ public keys are exactly
    /// their canonical FIPS sizes ([`ML_DSA_65_PUBKEY_LEN`] /
    /// [`ML_KEM_768_PUBKEY_LEN`]) and any advertised relay URL is within
    /// [`MAX_TUNNEL_RELAY_URL_LEN`]. Defined ONCE here so the handshake-derive,
    /// CRDT-apply, and deserialize gates can't drift. Does NOT validate the
    /// `iroh_node_id` non-zero / dial-target rule — that's `peer_handshake_contact`'s
    /// concern (a zero node id is a legitimately-absent contact, not a malformed one).
    pub fn has_valid_key_sizes(&self) -> bool {
        self.pq_dsa_pubkey.len() == ML_DSA_65_PUBKEY_LEN
            && self.pq_kem_pubkey.len() == ML_KEM_768_PUBKEY_LEN
            && self
                .home_relay_url
                .as_ref()
                .is_none_or(|u| u.len() <= MAX_TUNNEL_RELAY_URL_LEN)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OwnerDeviceEntry {
    /// Sorted ascending lex, deduped, capped at MAX_DEVICES_PER_OWNER.
    /// Sorted invariant means binary_search works for lookup
    /// (used by resolve_link_origin_owner in Phase 3b).
    ///
    /// The struct-level manual `Deserialize` impl re-normalizes
    /// (sort + merge-by-hash + truncate) on every load JOINTLY with
    /// `device_identity_pubs` so persisted-state files and remote
    /// replicas can't hand us a `Vec` pair that violates either the
    /// per-vec invariant or the parallel-vec correspondence — a
    /// corrupted on-disk snapshot or a malicious peer's `OwnerState`
    /// blob can otherwise break the binary_search precondition Phase
    /// 3b's link-origin resolver depends on, OR (the parallel-vec
    /// flavor) re-pair re-sorted devices with stale-indexed pubs.
    ///
    /// SECURITY NOTE: truncation keeps lex-smallest entries; an attacker
    /// who controls injected DeviceIdentityHash values could grind low-byte
    /// prefixes to displace legitimate devices. Acceptable in Phase 1 since
    /// updates must win the LWW HLC check (i.e., the owner's own device must
    /// publish the update). See ZEB-219 for the analogous prior_content_keys
    /// concern.
    #[serde(rename = "v", serialize_with = "serialize_devices_passthrough")]
    pub devices: Vec<DeviceIdentityHash>,

    /// Per-device combined identity public-bytes (`X25519_pub(32) ||
    /// Ed25519_pub(32)`, the canonical
    /// `harmony_identity::Identity::to_public_bytes()` layout). Parallel
    /// to `devices` — element i is the identity_pub for `devices[i]`
    /// when present, or `None` when this device is known-by-hash but its
    /// pub hasn't been propagated yet (the bootstrap-incompleteness case
    /// from Path B per ZEB-216 spec §"Public-key storage on
    /// OwnerDeviceCache"). Caller treats `None` as `UnknownSigningKey` —
    /// signature verification cannot proceed without the cached pub.
    ///
    /// 64 bytes (not 32): `signing_device_hash = SHA256(X25519 ||
    /// Ed25519)[:16]` per `harmony_identity::Identity::address_hash`.
    /// Storing only the Ed25519 half would yield an Ed25519-only hash
    /// that diverges from `DeviceIdentityHash` values stored in
    /// `devices`, silently breaking every cache lookup in
    /// `resolve_signed_origin_owner`.
    ///
    /// Pre-Phase-3b snapshots wrote no `p` field at all; the manual
    /// `Deserialize` impl on this struct treats a missing or empty `p`
    /// as `vec![None; devices.len()]` so the parallel-vec invariant
    /// (`pubs.len() == devices.len()`) holds in-memory regardless of
    /// wire shape. The receive path then drops signature-verification
    /// packets from those devices as `UnknownSigningKey` until the next
    /// invite-equivalent flow repopulates the pubs.
    #[serde(rename = "p", serialize_with = "serialize_device_identity_pubs")]
    pub device_identity_pubs: Vec<Option<[u8; 64]>>,

    /// HLC of when this entry was learned. LWW key for merge.
    #[serde(rename = "l")]
    pub learned_at: Hlc,

    /// Per-device tunnel reachability + PQ keys (ZEB-473). Parallel to
    /// `devices` — element i is the `DeviceTunnelContact` for `devices[i]`
    /// when present, or `None` when the device is known-by-hash but its
    /// reachability/PQ keys haven't been propagated yet. Held parallel
    /// through the manual `Deserialize` impl JOINTLY with `device_identity_pubs`.
    ///
    /// `#[serde(default)]`: pre-ZEB-473 snapshots wrote no `t` field; the
    /// manual `Deserialize` impl treats a missing or empty `t` as
    /// `vec![None; devices.len()]` so the parallel-vec invariant holds
    /// in-memory regardless of wire shape. Merge rule is last-writer-wins
    /// by `learned_at` (a routing hint, not an identity authority — see
    /// `DeviceTunnelContact`); a `None` never overwrites a `Some`.
    #[serde(default, rename = "t")]
    pub device_tunnel_contacts: Vec<Option<DeviceTunnelContact>>,
}

/// Pass-through Serialize for `OwnerDeviceEntry::devices`. Required only
/// so the per-field `serialize_with` signatures stay symmetric with
/// `device_identity_pubs`'s custom `serialize_device_identity_pubs`; the
/// derived behavior is what we want for the devices vec.
fn serialize_devices_passthrough<S>(v: &[DeviceIdentityHash], s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    use serde::ser::SerializeSeq;
    let mut seq = s.serialize_seq(Some(v.len()))?;
    for d in v {
        seq.serialize_element(d)?;
    }
    seq.end()
}

// Manual Deserialize for OwnerDeviceEntry. The previous setup —
// `#[derive(Deserialize)]` plus per-field `deserialize_with` for
// `devices` and `device_identity_pubs` — was a correctness hazard: the
// two fields were normalized INDEPENDENTLY, so a non-canonical on-disk
// snapshot (devices unsorted, pubs in wire order) would deserialize
// into re-sorted devices paired with stale-indexed pubs, silently
// breaking the parallel-vec correspondence that
// `resolve_signed_origin_owner` and signature verification rely on.
//
// This impl reads BOTH vecs raw (cap-rejected at the per-element level
// for OOM safety), pads `pubs` to `devices.len()` (handles old snapshots
// where pubs == [] but devices is non-empty), zips them, sorts BY HASH,
// walks-and-merges duplicates with the same merge rule as
// `apply_owner_device_update` (prefer Some over None; reject conflicting
// Somes via D::Error::custom), then splits back into parallel vecs.
impl<'de> Deserialize<'de> for OwnerDeviceEntry {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::{Error, MapAccess, Visitor};
        use std::fmt;

        #[derive(serde::Deserialize)]
        #[serde(field_identifier, rename_all = "lowercase")]
        enum Field {
            #[serde(rename = "v")]
            V,
            #[serde(rename = "p")]
            P,
            #[serde(rename = "l")]
            L,
            #[serde(rename = "t")]
            T,
            #[serde(other)]
            Other,
        }

        struct EntryVisitor;

        impl<'de> Visitor<'de> for EntryVisitor {
            type Value = OwnerDeviceEntry;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an OwnerDeviceEntry CBOR map with keys v/p/l")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut devices: Option<Vec<DeviceIdentityHash>> = None;
                let mut pubs: Option<Vec<Option<[u8; 64]>>> = None;
                let mut learned_at: Option<Hlc> = None;
                let mut contacts: Option<Vec<Option<DeviceTunnelContact>>> = None;

                while let Some(field) = map.next_key::<Field>()? {
                    match field {
                        Field::V => {
                            if devices.is_some() {
                                return Err(A::Error::duplicate_field("v"));
                            }
                            // Use the raw cap-aware reader: it bounds
                            // per-element memory but does NOT sort/dedup
                            // (the join below does that with pubs).
                            devices = Some(map.next_value_seed(RawDevicesSeed)?);
                        }
                        Field::P => {
                            if pubs.is_some() {
                                return Err(A::Error::duplicate_field("p"));
                            }
                            pubs = Some(map.next_value_seed(RawPubsSeed)?);
                        }
                        Field::L => {
                            if learned_at.is_some() {
                                return Err(A::Error::duplicate_field("l"));
                            }
                            learned_at = Some(map.next_value()?);
                        }
                        Field::T => {
                            if contacts.is_some() {
                                return Err(A::Error::duplicate_field("t"));
                            }
                            // Cap-bounded raw reader (no sort/dedup — the
                            // join below carries contacts parallel through
                            // sort/merge jointly with devices+pubs).
                            contacts = Some(map.next_value_seed(RawTunnelContactsSeed)?);
                        }
                        Field::Other => {
                            // Unknown field — drain its value and ignore.
                            // Matches `#[derive(Deserialize)]`'s default
                            // forgiving-of-unknown-fields posture.
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                    }
                }

                let devices = devices.ok_or_else(|| A::Error::missing_field("v"))?;
                let mut pubs = pubs.unwrap_or_default();
                let learned_at = learned_at.ok_or_else(|| A::Error::missing_field("l"))?;
                // ZEB-473: `t` is `#[serde(default)]`; pre-ZEB-473 snapshots
                // wrote no `t` field at all, so a missing/short vec is the
                // common case (→ pad to None below).
                let mut contacts = contacts.unwrap_or_default();

                // Pre-Phase-3b snapshots wrote no `p` field at all (or wrote
                // `p == []`); pad to devices.len() so the parallel-vec
                // invariant holds before the join below. If `p.len() >
                // devices.len()` (malformed peer), truncate — devices is
                // the source of truth for length. Same parity rule for the
                // ZEB-473 `t` (device_tunnel_contacts) vec.
                if pubs.len() < devices.len() {
                    pubs.resize(devices.len(), None);
                } else if pubs.len() > devices.len() {
                    pubs.truncate(devices.len());
                }
                if contacts.len() < devices.len() {
                    contacts.resize(devices.len(), None);
                } else if contacts.len() > devices.len() {
                    contacts.truncate(devices.len());
                }

                // Cap-reject BEFORE zip — cheap, mirrors the per-element
                // streaming check inside the raw readers (which is the
                // OOM-safety path; this catches the rarer case where both
                // raw vecs landed within their per-vec cap individually
                // but the joined collection still wants to exceed the
                // shared cap).
                if devices.len() > MAX_DEVICES_PER_OWNER {
                    return Err(A::Error::custom(format!(
                        "OwnerDeviceEntry.devices length {} exceeds \
                         MAX_DEVICES_PER_OWNER ({})",
                        devices.len(),
                        MAX_DEVICES_PER_OWNER
                    )));
                }

                // Zip + sort BY HASH (preserves parallel-vec correspondence)
                // + merge duplicate-hash entries, carrying the ZEB-473
                // tunnel contact as a third parallel element.
                //
                // identity-pub merge (unchanged):
                //   both None → None
                //   exactly one Some → keep the Some
                //   both Some equal → keep one
                //   both Some different → reject (real invariant fail).
                //
                // tunnel-contact merge (ZEB-473): contacts are routing
                // hints that legitimately change, so there is NO invariant
                // fail — a `None` never overwrites a `Some`, and two
                // differing `Some`s collapse to the LAST one seen (within a
                // single entry there is no per-element HLC; the CRDT-apply
                // path applies the true LWW-by-`learned_at` rule).
                let mut zipped: Vec<(
                    DeviceIdentityHash,
                    Option<[u8; 64]>,
                    Option<DeviceTunnelContact>,
                )> = devices
                    .into_iter()
                    .zip(pubs)
                    .zip(contacts)
                    .map(|((d, p), t)| (d, p, t))
                    .collect();
                zipped.sort_by_key(|(d, _, _)| *d);

                let mut merged: Vec<(
                    DeviceIdentityHash,
                    Option<[u8; 64]>,
                    Option<DeviceTunnelContact>,
                )> = Vec::with_capacity(zipped.len());
                for (d, p, t) in zipped {
                    match merged.last_mut() {
                        Some((prev_d, prev_p, prev_t)) if *prev_d == d => {
                            match (*prev_p, p) {
                                (None, None) => {}
                                (None, Some(_)) => *prev_p = p,
                                (Some(_), None) => {}
                                (Some(a), Some(b)) if a == b => {}
                                (Some(_), Some(_)) => {
                                    return Err(A::Error::custom(format!(
                                        "OwnerDeviceEntry has conflicting identity pubs \
                                         for device {:?}",
                                        d
                                    )));
                                }
                            }
                            // Contact: None never overwrites Some; otherwise
                            // last-Some-wins. No reject (LWW, not authority).
                            if t.is_some() {
                                *prev_t = t;
                            }
                        }
                        _ => merged.push((d, p, t)),
                    }
                }
                merged.truncate(MAX_DEVICES_PER_OWNER);

                // Defense-in-depth: every cached `Some(identity_pub)` MUST
                // derive (via SHA256(pub)[:16] —
                // `derive_device_hash_from_identity_pub`) to its paired
                // `DeviceIdentityHash`. A poisoned snapshot or malicious
                // peer's `OwnerState` blob could otherwise pair a hash
                // with a non-matching pub, silently breaking every
                // signature verify in `resolve_signed_origin_owner`.
                // Reject at deserialize time so the bad state never enters
                // the in-memory cache. Mirrors the parallel check in
                // `apply_owner_device_update`.
                for (d, p, _t) in merged.iter() {
                    if let Some(pub_bytes) = p {
                        match crate::dm_signing::derive_device_hash_from_identity_pub(pub_bytes) {
                            Some(derived) if derived == *d => {}
                            Some(derived) => {
                                return Err(A::Error::custom(format!(
                                    "OwnerDeviceEntry has identity pub for device {:?} \
                                     that derives to a different device hash {:?}",
                                    d, derived
                                )));
                            }
                            None => {
                                return Err(A::Error::custom(format!(
                                    "OwnerDeviceEntry has structurally-invalid identity pub \
                                     for device {:?}",
                                    d
                                )));
                            }
                        }
                    }
                }

                let mut sanitized_devices = Vec::with_capacity(merged.len());
                let mut sanitized_pubs = Vec::with_capacity(merged.len());
                let mut sanitized_contacts = Vec::with_capacity(merged.len());
                for (d, p, t) in merged {
                    sanitized_devices.push(d);
                    sanitized_pubs.push(p);
                    sanitized_contacts.push(t);
                }

                Ok(OwnerDeviceEntry {
                    devices: sanitized_devices,
                    device_identity_pubs: sanitized_pubs,
                    learned_at,
                    device_tunnel_contacts: sanitized_contacts,
                })
            }
        }

        // DeserializeSeed wrappers that route to the existing raw
        // (cap-aware, OOM-safe) sequence visitors without re-introducing
        // the per-vec sort/dedup that the join above now owns.
        struct RawDevicesSeed;
        impl<'de> serde::de::DeserializeSeed<'de> for RawDevicesSeed {
            type Value = Vec<DeviceIdentityHash>;
            fn deserialize<De: Deserializer<'de>>(self, d: De) -> Result<Self::Value, De::Error> {
                deserialize_raw_device_identities(d)
            }
        }
        struct RawPubsSeed;
        impl<'de> serde::de::DeserializeSeed<'de> for RawPubsSeed {
            type Value = Vec<Option<[u8; 64]>>;
            fn deserialize<De: Deserializer<'de>>(self, d: De) -> Result<Self::Value, De::Error> {
                deserialize_device_identity_pubs(d)
            }
        }
        struct RawTunnelContactsSeed;
        impl<'de> serde::de::DeserializeSeed<'de> for RawTunnelContactsSeed {
            type Value = Vec<Option<DeviceTunnelContact>>;
            fn deserialize<De: Deserializer<'de>>(self, d: De) -> Result<Self::Value, De::Error> {
                deserialize_raw_tunnel_contacts(d)
            }
        }

        d.deserialize_map(EntryVisitor)
    }
}

/// Deserialize a `Vec<DeviceIdentityHash>` for `OwnerDeviceEntry::devices`
/// with cap-rejection, but WITHOUT sort/dedup — the struct-level
/// `Deserialize` impl on `OwnerDeviceEntry` owns sort/dedup because it
/// must be performed jointly with the parallel `device_identity_pubs`
/// vec to preserve the parallel-vec correspondence (sorting either
/// independently leaves them misaligned).
///
/// Streams items via a `Visitor` rather than calling `Vec::deserialize`
/// to bound peak memory: a peer (or corrupted file) declaring
/// `array(2^32-1)` of 16-byte hashes would otherwise force a multi-GB
/// allocation BEFORE any cap could take effect. The visitor:
///   1. Rejects up-front via `seq.size_hint()` when the deserializer
///      can tell us a definite-length array exceeds the cap (cheap path).
///   2. Pre-allocates `min(cap, size_hint)` so a small honest payload
///      doesn't grow the vec needlessly.
///   3. Returns `Err` immediately on the (cap+1)-th element, refusing to
///      consume the rest of the stream.
///
/// Reject vs. truncate: `apply_owner_device_update` always emits a vec
/// already capped at `MAX_DEVICES_PER_OWNER`, so any wire input above
/// the cap is malformed by definition. Reject is semantically correct
/// AND surfaces buggy peers that "silently truncate" would mask
/// (e.g., a peer emitting >cap entries is dropping device entries we
/// might need for DM delivery — better to fail loudly).
fn deserialize_raw_device_identities<'de, D>(d: D) -> Result<Vec<DeviceIdentityHash>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{Error, SeqAccess, Visitor};
    use std::fmt;

    struct CapVisitor;

    impl<'de> Visitor<'de> for CapVisitor {
        type Value = Vec<DeviceIdentityHash>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(
                f,
                "an array of at most {} DeviceIdentityHash entries",
                MAX_DEVICES_PER_OWNER
            )
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            // Cheap upfront rejection if the deserializer can tell us
            // the length (definite-length CBOR arrays carry it).
            if let Some(n) = seq.size_hint() {
                if n > MAX_DEVICES_PER_OWNER {
                    return Err(A::Error::custom(format!(
                        "DeviceIdentityHash array length {} exceeds MAX_DEVICES_PER_OWNER ({})",
                        n, MAX_DEVICES_PER_OWNER
                    )));
                }
            }
            let initial_cap = seq
                .size_hint()
                .unwrap_or(MAX_DEVICES_PER_OWNER)
                .min(MAX_DEVICES_PER_OWNER);
            let mut out: Vec<DeviceIdentityHash> = Vec::with_capacity(initial_cap);
            while let Some(item) = seq.next_element::<DeviceIdentityHash>()? {
                if out.len() >= MAX_DEVICES_PER_OWNER {
                    return Err(A::Error::custom(format!(
                        "DeviceIdentityHash array exceeds MAX_DEVICES_PER_OWNER ({}); \
                         legitimate peers always send canonical (capped) form",
                        MAX_DEVICES_PER_OWNER
                    )));
                }
                out.push(item);
            }
            // No sort/dedup here — the struct-level Deserialize impl on
            // OwnerDeviceEntry sorts+dedups jointly with the parallel
            // `device_identity_pubs` vec. Sorting independently here
            // would leave the two vecs misaligned (the original bug).
            Ok(out)
        }
    }

    d.deserialize_seq(CapVisitor)
}

/// Deserialize a `Vec<Option<DeviceTunnelContact>>` for
/// `OwnerDeviceEntry::device_tunnel_contacts` (ZEB-473) with cap-rejection
/// but WITHOUT sort/dedup — the struct-level `Deserialize` impl on
/// `OwnerDeviceEntry` owns sort/merge because it must be performed jointly
/// with `devices` and `device_identity_pubs` to preserve the parallel-vec
/// correspondence (sorting any vec independently leaves them misaligned).
///
/// Mirrors `deserialize_raw_device_identities`'s cap behavior: rejects
/// up-front via `size_hint`, pre-allocates `min(cap, hint)`, refuses the
/// (cap+1)-th element. Each element is a standard `Option<DeviceTunnelContact>`
/// (CBOR null → `None`, a map → `Some`), decoded via the type's derived
/// `Deserialize`.
fn deserialize_raw_tunnel_contacts<'de, D>(
    d: D,
) -> Result<Vec<Option<DeviceTunnelContact>>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{Error, SeqAccess, Visitor};
    use std::fmt;

    struct CapVisitor;

    impl<'de> Visitor<'de> for CapVisitor {
        type Value = Vec<Option<DeviceTunnelContact>>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(
                f,
                "an array of at most {} optional DeviceTunnelContact entries",
                MAX_DEVICES_PER_OWNER
            )
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            if let Some(n) = seq.size_hint() {
                if n > MAX_DEVICES_PER_OWNER {
                    return Err(A::Error::custom(format!(
                        "device_tunnel_contacts array length {} exceeds MAX_DEVICES_PER_OWNER ({})",
                        n, MAX_DEVICES_PER_OWNER
                    )));
                }
            }
            let initial_cap = seq
                .size_hint()
                .unwrap_or(MAX_DEVICES_PER_OWNER)
                .min(MAX_DEVICES_PER_OWNER);
            let mut out: Vec<Option<DeviceTunnelContact>> = Vec::with_capacity(initial_cap);
            while let Some(item) = seq.next_element::<Option<DeviceTunnelContact>>()? {
                if out.len() >= MAX_DEVICES_PER_OWNER {
                    return Err(A::Error::custom(format!(
                        "device_tunnel_contacts array exceeds MAX_DEVICES_PER_OWNER ({}); \
                         legitimate peers always send canonical (capped) form",
                        MAX_DEVICES_PER_OWNER
                    )));
                }
                // CR11 (ZEB-473): the element count is capped above, but each
                // `DeviceTunnelContact` carries unbounded `pq_dsa_pubkey` /
                // `pq_kem_pubkey` byte strings. A malicious owner-state blob
                // could otherwise force huge per-key allocations. The PQ keys
                // have FIXED canonical sizes, so anything larger is malformed —
                // reject so it can never poison downstream tunnel code. (Short
                // keys are tolerated here; the apply-time gate enforces exact
                // sizes — deser just bounds allocation.)
                if let Some(ref c) = item {
                    if c.pq_dsa_pubkey.len() > ML_DSA_65_PUBKEY_LEN
                        || c.pq_kem_pubkey.len() > ML_KEM_768_PUBKEY_LEN
                    {
                        return Err(A::Error::custom(
                            "DeviceTunnelContact PQ key material exceeds its canonical size cap",
                        ));
                    }
                }
                out.push(item);
            }
            Ok(out)
        }
    }

    d.deserialize_seq(CapVisitor)
}

/// Serialize `Vec<Option<[u8; 64]>>` for
/// `OwnerDeviceEntry::device_identity_pubs`. Each element is encoded as
/// either CBOR null (for `None` — known-by-hash, pub not yet cached) or
/// a CBOR bstr(64) (for `Some(pub_bytes)`). The 64-byte bstr form is
/// significantly more compact than the default `[u8; 64]` Serialize impl
/// (which emits a 64-element CBOR array, two bytes per element after
/// 0x17) and matches the bstr-everywhere convention used elsewhere in
/// this module (see `serialize_bytes_as_bstr`).
///
/// We need an explicit `serialize_with` here because serde's blanket
/// `Serialize` impl on `[T; N]` only covers `N <= 32` (or per-version
/// const-generic limits), so the derive-generated serialization for the
/// outer struct can't see `[u8; 64]: Serialize`. Custom helper sidesteps
/// the derive entirely by walking the vec and emitting each element via
/// `serialize_bytes`.
fn serialize_device_identity_pubs<S>(v: &[Option<[u8; 64]>], s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    use serde::ser::SerializeSeq;

    let mut seq = s.serialize_seq(Some(v.len()))?;
    for opt in v {
        // Wrap in a tiny newtype that overrides Serialize to emit
        // bstr(64) for Some / null for None. SerializeSeq::serialize_element
        // requires `T: Serialize`, so we can't pass the bare slice.
        struct BstrOpt<'a>(&'a Option<[u8; 64]>);
        impl serde::Serialize for BstrOpt<'_> {
            fn serialize<S2: Serializer>(&self, s: S2) -> Result<S2::Ok, S2::Error> {
                match self.0 {
                    Some(bytes) => s.serialize_bytes(bytes),
                    None => s.serialize_none(),
                }
            }
        }
        seq.serialize_element(&BstrOpt(opt))?;
    }
    seq.end()
}

/// Deserialize `Vec<Option<[u8; 64]>>` for
/// `OwnerDeviceEntry::device_identity_pubs`. Mirrors
/// `deserialize_raw_device_identities`'s cap behavior: rejects up-front via
/// `size_hint` when possible, pre-allocates `min(cap, hint)`, refuses
/// the (cap+1)-th element. Does NOT sort or dedup — order is meaningful
/// (parallel-indexed to `OwnerDeviceEntry.devices`, so reordering would
/// silently break the device→pub correspondence that
/// `dm_signing::verify_dm_packet_signature` relies on).
///
/// Each element is decoded as either CBOR null (→ `None`) or CBOR
/// bstr(64) (→ `Some([u8; 64])`). Length is enforced strictly: a bstr
/// of any length other than 64 is rejected.
fn deserialize_device_identity_pubs<'de, D>(d: D) -> Result<Vec<Option<[u8; 64]>>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{Error, SeqAccess, Visitor};
    use std::fmt;

    /// One element: CBOR null (→ None) or bstr(64) (→ Some).
    struct OptPubVisitor;

    impl<'de> Visitor<'de> for OptPubVisitor {
        type Value = Option<[u8; 64]>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "CBOR null or a 64-byte CBOR byte string")
        }

        fn visit_none<E: Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_unit<E: Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
            // serde's deserialize_option fans out to visit_some when the
            // wire value is non-null; delegate to a fresh bytes-visitor.
            d.deserialize_bytes(BytesVisitor).map(Some)
        }
        // ciborium drives Option-shaped deserialization through the
        // outer Visitor when the value is bytes (not via Option's
        // visit_some path). Accept bstr directly here.
        fn visit_bytes<E: Error>(self, value: &[u8]) -> Result<Self::Value, E> {
            BytesVisitor.visit_bytes(value).map(Some)
        }
        fn visit_byte_buf<E: Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
            BytesVisitor.visit_byte_buf(v).map(Some)
        }
    }

    struct BytesVisitor;
    impl<'de> Visitor<'de> for BytesVisitor {
        type Value = [u8; 64];

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "a 64-byte CBOR byte string")
        }

        fn visit_bytes<E: Error>(self, value: &[u8]) -> Result<[u8; 64], E> {
            if value.len() != 64 {
                return Err(E::custom(format!(
                    "device identity pub must be 64 bytes, got {}",
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

    /// Wrapper newtype so we can dispatch the custom OptPubVisitor per
    /// element without going through serde's default Option deserialize
    /// (which would call deserialize_bytes on a non-existent
    /// `[u8; 64]: Deserialize` for serde versions where that fails).
    struct OptPub(Option<[u8; 64]>);
    impl<'de> Deserialize<'de> for OptPub {
        fn deserialize<D2: Deserializer<'de>>(d: D2) -> Result<Self, D2::Error> {
            d.deserialize_any(OptPubVisitor).map(OptPub)
        }
    }

    struct CapVisitor;

    impl<'de> Visitor<'de> for CapVisitor {
        type Value = Vec<Option<[u8; 64]>>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(
                f,
                "an array of at most {} optional 64-byte identity pubs",
                MAX_DEVICES_PER_OWNER
            )
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            if let Some(n) = seq.size_hint() {
                if n > MAX_DEVICES_PER_OWNER {
                    return Err(A::Error::custom(format!(
                        "device_identity_pubs array length {} exceeds MAX_DEVICES_PER_OWNER ({})",
                        n, MAX_DEVICES_PER_OWNER
                    )));
                }
            }
            let initial_cap = seq
                .size_hint()
                .unwrap_or(MAX_DEVICES_PER_OWNER)
                .min(MAX_DEVICES_PER_OWNER);
            let mut out: Vec<Option<[u8; 64]>> = Vec::with_capacity(initial_cap);
            while let Some(item) = seq.next_element::<OptPub>()? {
                if out.len() >= MAX_DEVICES_PER_OWNER {
                    return Err(A::Error::custom(format!(
                        "device_identity_pubs array exceeds MAX_DEVICES_PER_OWNER ({}); \
                         legitimate peers always send canonical (capped) form",
                        MAX_DEVICES_PER_OWNER
                    )));
                }
                out.push(item.0);
            }
            Ok(out)
        }
    }

    d.deserialize_seq(CapVisitor)
}

/// Deserialize a `Vec<DmContentKey>` and re-establish the
/// `Space::prior_content_keys` canonical-form invariant (sorted ascending
/// by raw bytes + deduped + truncated to `MAX_PRIOR_CONTENT_KEYS`). Mirrors
/// the rationale on `deserialize_raw_device_identities`: `validate_invariants`
/// runs on apply paths only, NOT on initial load, so without this hook a
/// corrupted on-disk file with non-canonical priors would sit in
/// `state.spaces` unchecked, get re-serialized on the next `save_crdt`,
/// and produce a different `root_cid` than correctly-converged peers
/// (silent convergence break — replicas with semantically-equal but
/// differently-ordered priors disagree on `canonical_cbor_encode` bytes).
///
/// Same OOM-safe streaming pattern as `deserialize_raw_device_identities`:
/// a peer/file declaring `array(2^32-1)` of 32-byte keys would otherwise
/// force a multi-GB allocation before truncation runs. Reject (rather
/// than truncate) above the cap — `merge_prior_content_keys` always
/// produces ≤ cap entries, so anything more on the wire is malformed.
fn deserialize_prior_content_keys<'de, D>(d: D) -> Result<Vec<DmContentKey>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{Error, SeqAccess, Visitor};
    use std::fmt;

    struct CapVisitor;

    impl<'de> Visitor<'de> for CapVisitor {
        type Value = Vec<DmContentKey>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(
                f,
                "an array of at most {} DmContentKey entries",
                MAX_PRIOR_CONTENT_KEYS
            )
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            if let Some(n) = seq.size_hint() {
                if n > MAX_PRIOR_CONTENT_KEYS {
                    return Err(A::Error::custom(format!(
                        "DmContentKey array length {} exceeds MAX_PRIOR_CONTENT_KEYS ({})",
                        n, MAX_PRIOR_CONTENT_KEYS
                    )));
                }
            }
            let initial_cap = seq
                .size_hint()
                .unwrap_or(MAX_PRIOR_CONTENT_KEYS)
                .min(MAX_PRIOR_CONTENT_KEYS);
            let mut out: Vec<DmContentKey> = Vec::with_capacity(initial_cap);
            while let Some(item) = seq.next_element::<DmContentKey>()? {
                if out.len() >= MAX_PRIOR_CONTENT_KEYS {
                    return Err(A::Error::custom(format!(
                        "DmContentKey array exceeds MAX_PRIOR_CONTENT_KEYS ({}); \
                         legitimate peers always send canonical (capped) form",
                        MAX_PRIOR_CONTENT_KEYS
                    )));
                }
                out.push(item);
            }
            // Re-establish canonical form. DmContentKey doesn't impl Ord
            // directly; sort/dedup by raw bytes (matches the round-7
            // helper convention).
            out.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.dedup_by(|a, b| a.as_bytes() == b.as_bytes());
            out.truncate(MAX_PRIOR_CONTENT_KEYS);
            Ok(out)
        }
    }

    d.deserialize_seq(CapVisitor)
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

/// Transport binding. Internally tagged so the wire format is one CBOR
/// map per binding (not nested). Discriminant key `tg` (2 chars to match
/// the inner field key length per `canonical_cbor_encode`'s same-length-
/// keys precondition); variant codes `z` (1 char — values, not keys,
/// so not subject to that rule); inner field name `tp`.
///
/// ZEB-474: The `Reticulum { participants }` variant and the `ReticulumDest`
/// newtype have been removed (flag-day-for-alpha CBOR wire-format change).
/// DM/GroupDm Spaces now carry `transport: None` (deposit-only; no live
/// point-to-point binding until Move 1a / ZEB-473 adds an iroh binding).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tg")]
pub enum TransportBinding {
    #[serde(rename = "z")]
    Zenoh {
        #[serde(rename = "tp")] // "topic"
        topic: String,
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
    EpochKey, // ← NEW (Phase 1: ZEB-217)
    DeviceIdentityHash,
    OwnerDeviceCache,
    OwnerDeviceEntry,
    SpaceKind,
    NotificationPref,
    TransportBinding,
    Space,
    DedupeKey,
    DeliveryStatus,
    OutboxEntry,
    InboxKey,
    InboxEntry,
    ReadMarker,
    LibraryEntry,
    RootPublishPayload,
    crate::friend_graph::FriendGraph, // ZEB-370 Phase 1: friend-graph sub-CRDT
    crate::friend_graph::FriendEntry,
    crate::friend_token::FriendTokenPayload, // ZEB-370 Phase 1: friend-token URL payload
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

    #[test]
    fn deserialize_rejects_oversized_tunnel_contact_pq_key() {
        // CR11 (ZEB-473): a malicious owner-state blob carrying a
        // `DeviceTunnelContact` with a PQ key LARGER than its canonical size
        // must be rejected at deserialize time so it can't force a huge
        // allocation / poison downstream tunnel code. A correctly-sized contact
        // round-trips; an oversized one fails.
        use ciborium::{from_reader, into_writer};

        let device = DeviceIdentityHash([0x11; 16]);
        let ok_contact = DeviceTunnelContact {
            iroh_node_id: [0x22; 32],
            home_relay_url: None,
            pq_dsa_pubkey: vec![0x33; ML_DSA_65_PUBKEY_LEN],
            pq_kem_pubkey: vec![0x44; ML_KEM_768_PUBKEY_LEN],
        };
        let good = OwnerDeviceEntry {
            devices: vec![device],
            device_identity_pubs: vec![None],
            learned_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            device_tunnel_contacts: vec![Some(ok_contact.clone())],
        };
        let mut good_bytes = Vec::new();
        into_writer(&good, &mut good_bytes).unwrap();
        let recovered: OwnerDeviceEntry = from_reader(&good_bytes[..]).unwrap();
        assert_eq!(recovered.device_tunnel_contacts, vec![Some(ok_contact)]);

        // Now an oversized ML-DSA key (one byte past the canonical size).
        let bad = OwnerDeviceEntry {
            devices: vec![device],
            device_identity_pubs: vec![None],
            learned_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            device_tunnel_contacts: vec![Some(DeviceTunnelContact {
                iroh_node_id: [0x22; 32],
                home_relay_url: None,
                pq_dsa_pubkey: vec![0x33; ML_DSA_65_PUBKEY_LEN + 1],
                pq_kem_pubkey: vec![0x44; ML_KEM_768_PUBKEY_LEN],
            })],
        };
        let mut bad_bytes = Vec::new();
        into_writer(&bad, &mut bad_bytes).unwrap();
        let err = from_reader::<OwnerDeviceEntry, _>(&bad_bytes[..]);
        assert!(
            err.is_err(),
            "an oversized PQ key must fail deserialization, got Ok"
        );
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

    // ZEB-474: transport_binding_reticulum_round_trip and
    // reticulum_dest_emits_cbor_bstr tests deleted — the
    // TransportBinding::Reticulum variant and ReticulumDest type were
    // removed (flag-day-for-alpha CBOR wire-format change).

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
/// Wire-format note: every field is renamed to a 2-char code so all 17
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
    /// MUST NOT contain the current `content_key`.
    /// MUST be empty for non-DM kinds.
    /// Wire format: array of bstr(32) under key "pk".
    ///
    /// `deserialize_with` re-normalizes (sort + dedup + truncate) on every
    /// load so persisted-state files and remote replicas can't hand us a
    /// `Vec` that violates the canonical-form invariant — `validate_
    /// invariants` runs only on apply, not on initial load, so without
    /// this hook a corrupted file's malformed priors would round-trip
    /// through `save_crdt` and break root_cid convergence with peers.
    #[serde(
        rename = "pk",
        skip_serializing_if = "Vec::is_empty",
        default,
        deserialize_with = "deserialize_prior_content_keys"
    )]
    pub prior_content_keys: Vec<DmContentKey>,

    /// Current epoch counter for this community. 0 at community creation;
    /// increments on every successful EpochRotation. MUST be Some for
    /// kind == Community; MUST be None otherwise. Wire: u64 under "ce".
    /// See ZEB-249 spec §3.2.
    #[serde(rename = "ce", skip_serializing_if = "Option::is_none", default)]
    pub current_epoch: Option<u64>,

    /// Active EpochKey for new outbound events at `current_epoch`.
    /// MUST be Some for kind == Community; MUST be None otherwise.
    /// Wire: bstr(32) under "ek". Zeroized on drop.
    /// See ZEB-249 spec §3.2.
    #[serde(rename = "ek", skip_serializing_if = "Option::is_none", default)]
    pub current_epoch_key: Option<EpochKey>,

    /// Historical EpochKeys for decrypting old events. Keyed by the
    /// epoch counter at which the key was current. MUST be empty for
    /// kind != Community. Wire: map<u64, bstr(32)> under "ok".
    /// See ZEB-249 spec §3.2 + §10.5 (storage growth bounds).
    #[serde(rename = "ok", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub old_epoch_keys: BTreeMap<u64, EpochKey>,

    /// Initial admin (creator) — receives power 100 implicitly via the
    /// bootstrap rule (see ZEB-217 spec §"Materialization rules /
    /// Bootstrap"). MUST be Some for kind == Community; MUST be None
    /// otherwise. Wire: bstr(16) under "ad".
    #[serde(rename = "ad", skip_serializing_if = "Option::is_none", default)]
    pub admin_addr: Option<OwnerAddr>,

    /// Policy flag — false = open (peers publish join events directly),
    /// true = invite-only (join requires counter-sig from member with
    /// power ≥ POWER_THRESHOLDS.invite). MUST be Some for kind ==
    /// Community; MUST be None otherwise. Wire: bool under "io".
    #[serde(rename = "io", skip_serializing_if = "Option::is_none", default)]
    pub is_invite_only: Option<bool>,

    /// Sub-D Phase 4 (ZEB-281): opt-in flag for including this Space's
    /// `Space.id` (the community's identifier) in the owner's
    /// ProfileMembershipBroadcast. Community Spaces have
    /// `community_id = None` (the field is a back-pointer that lives on
    /// child Channel Spaces); the shared identifier IS this Space's own
    /// `id`. Default `false` (no communities shared until user explicitly
    /// opts in). Replicated across the owner's bound devices via the
    /// existing owner-state CRDT sync — opting in on one device shows
    /// on all of them.
    ///
    /// Only meaningful for `kind == Community`. Setting `true` on
    /// non-community Spaces is rejected by `validate_invariants`.
    ///
    /// `skip_serializing_if = "core::ops::Not::not"` (skip when false)
    /// keeps the default-false case byte-identical to pre-Phase-4
    /// owner-state wire bytes. Verified by existing wire-format pinning
    /// fixtures (Task 4 will add a dedicated regression test).
    #[serde(rename = "sp", default, skip_serializing_if = "core::ops::Not::not")]
    pub shared_in_profile: bool,

    /// ZEB-254: set when the joiner has minted a PendingJoin for this
    /// community but no JoinCountersign has yet landed locally. None
    /// means the joiner is fully Joined (or this Space is non-Community,
    /// or pre-ZEB-254 Space). Transitions:
    ///   None → Some(hlc): set at redeem-invite commit when the 5s
    ///     fast-path timeout fires without a counter-sign.
    ///   Some(hlc) → None: cleared by the community engine's post-Inserted
    ///     hook when self's PendingJoin receives a JoinCountersign.
    ///
    /// CRDT merge: existing LWW-by-updated_at handles None ↔ Some
    /// transitions (Space.updated_at advances on each transition).
    #[serde(rename = "pj", skip_serializing_if = "Option::is_none", default)]
    pub pending_join_at: Option<Hlc>,
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
        // Universal: community-only fields MUST be None unless kind == Community.
        // Checked before the per-kind match so every non-community kind gets
        // the same enforcement without per-arm duplication.
        if self.kind != SpaceKind::Community {
            if self.pending_join_at.is_some() {
                return Err(InvariantError(format!(
                    "{:?} must have pending_join_at=None (only Community carries it)",
                    self.kind
                )));
            }
            if self.current_epoch.is_some() {
                return Err(InvariantError(format!(
                    "{:?} must have current_epoch=None (only Community carries epoch state)",
                    self.kind
                )));
            }
            if self.current_epoch_key.is_some() {
                return Err(InvariantError(format!(
                    "{:?} must have current_epoch_key=None (only Community carries epoch state)",
                    self.kind
                )));
            }
            if !self.old_epoch_keys.is_empty() {
                return Err(InvariantError(format!(
                    "{:?} must have old_epoch_keys=empty (only Community carries epoch state)",
                    self.kind
                )));
            }
            if self.admin_addr.is_some() {
                return Err(InvariantError(format!(
                    "{:?} must have admin_addr=None (only Community carries it)",
                    self.kind
                )));
            }
            if self.is_invite_only.is_some() {
                return Err(InvariantError(format!(
                    "{:?} must have is_invite_only=None (only Community carries it)",
                    self.kind
                )));
            }
            // Sub-D Phase 4 (ZEB-281): shared_in_profile is only
            // meaningful for communities. Reject malformed peers attempting
            // to set it on DMs/group-DMs/profiles/folders/etc.
            if self.shared_in_profile {
                return Err(InvariantError(format!(
                    "{:?} must have shared_in_profile=false (only Community carries it)",
                    self.kind
                )));
            }
        }

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
                // ZEB-474: deposit-only DMs carry no live point-to-point
                // transport binding (the Reticulum carrier was removed).
                // Move 1a (ZEB-473) may reintroduce an iroh binding here.
                if self.transport.is_some() {
                    return Err(InvariantError("dm must have transport=None".into()));
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
                // ZEB-474: deposit-only GroupDMs carry no live point-to-point
                // transport binding (the Reticulum carrier was removed).
                // Move 1a (ZEB-473) may reintroduce an iroh binding here.
                if self.transport.is_some() {
                    return Err(InvariantError("group-dm must have transport=None".into()));
                }
            }
            SpaceKind::Community => {
                if self.current_epoch.is_none() {
                    return Err(InvariantError(
                        "community must have current_epoch (epoch counter, 0 at creation)".into(),
                    ));
                }
                if self.current_epoch_key.is_none() {
                    return Err(InvariantError(
                        "community must have current_epoch_key (active symmetric key for the membership topic at current_epoch)"
                            .into(),
                    ));
                }
                // old_epoch_keys may be empty (epoch 0 has no history); no None check.
                if self.admin_addr.is_none() {
                    return Err(InvariantError(
                        "community must have admin_addr (creator who holds power 100 via the bootstrap rule)"
                            .into(),
                    ));
                }
                if self.is_invite_only.is_none() {
                    return Err(InvariantError(
                        "community must have is_invite_only (open vs invite-only policy flag)"
                            .into(),
                    ));
                }
                if !self.members.is_empty() {
                    return Err(InvariantError(
                        "community must have members=[] in owner-state Space \
                         (real membership is in CommunityState CRDT)"
                            .into(),
                    ));
                }
                if self.transport.is_some() {
                    return Err(InvariantError("community must have transport=None".into()));
                }
                if self.community_id.is_some() {
                    return Err(InvariantError(
                        "community must have community_id=None \
                         (community Space IS the community)"
                            .into(),
                    ));
                }
                if self.content_key.is_some() {
                    return Err(InvariantError(
                        "community must have content_key=None \
                         (current_epoch_key is the community's symmetric key)"
                            .into(),
                    ));
                }
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
            SpaceKind::Community => {
                // content_key invariant for Community is enforced in the Community
                // arm of the kind-invariants match above (with its own rationale
                // message — "current_epoch_key is the community's symmetric key").
                // Excluding the content_key check here prevents a duplicated
                // error firing with a less-informative message if the
                // kind-invariants match is ever refactored to fall through.
                //
                // prior_content_keys must still be empty (Community has no
                // historical content-key chain — historical epoch keys live in
                // old_epoch_keys, not prior_content_keys). Enforce it here because
                // the catch-all _ arm below would also reject content_key=Some,
                // which Community legitimately disallows via its own message above
                // — so falling through would surface a worse error.
                if !self.prior_content_keys.is_empty() {
                    return Err(InvariantError(
                        "community must have prior_content_keys=[] \
                         (historical epoch keys live in old_epoch_keys, not prior_content_keys)"
                            .into(),
                    ));
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

        // Canonical-form invariant: prior_content_keys must be
        // strictly-ascending sorted (catches both unsorted ordering
        // and adjacent duplicates in one predicate).
        // `merge_prior_content_keys` always emits canonical
        // (sorted+deduped) output, so a non-canonical value here
        // represents malformed wire data, corrupted on-disk state,
        // or a bug elsewhere. Space serializes via canonical_cbor_encode
        // into the encrypted root blob — two replicas with semantically-
        // equal but differently-ordered prior_content_keys would
        // produce different canonical bytes (and thus different
        // root_cids), breaking convergence. Enforce strictly so this
        // invariant remains load-bearing.
        if !self
            .prior_content_keys
            .windows(2)
            .all(|w| w[0].as_bytes() < w[1].as_bytes())
        {
            return Err(InvariantError(
                "prior_content_keys must be sorted ascending lex (catches unsorted and duplicated)"
                    .into(),
            ));
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
///
/// ZEB-505: `message_cid` is `Option<ContentId>`. `Some(cid)` = a normal
/// sent-message entry; `None` = a standalone durable DM *invite* with no
/// following message (minted by `add_space` so the bootstrap invite gets the
/// same retry + deposit durability a message rides on, instead of the old
/// best-effort fire-and-forget). Wire-compatible by construction: ciborium
/// encodes `Some(cid)` transparently as the bare `cid`, so existing persisted
/// entries stay byte-identical; `#[serde(default)]` lets an absent `mc` decode
/// to `None`; a `None` entry encodes `mc: null`.
///
/// Forward-compat caveat (Greptile): a *pre-ZEB-505* paired device decodes
/// `message_cid` as a MANDATORY `ContentId` and so cannot decode an invite-only
/// entry (`mc: null`). Because a fleet root publish is decoded as one atomic
/// `OwnerState` blob, such a device drops the WHOLE publish that carries an
/// invite-only entry until it upgrades. This is a transient multi-device
/// upgrade-window degradation ONLY: it never affects the originating device's
/// own durability or the recipient's invite delivery (that device deposits
/// regardless), and it self-heals once both devices run ZEB-505. Filtering
/// invite-only entries out of the published root would avoid it, at the cost of
/// fleet-failover redundancy for the invite — a tradeoff deferred while the
/// originating device's durability is sufficient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxEntry {
    #[serde(rename = "id")]
    pub id: OutboxEntryId,
    #[serde(rename = "sp")]
    pub space_id: SpaceId,
    #[serde(rename = "rc")]
    pub recipient_owners: Vec<OwnerAddr>,
    #[serde(rename = "mc", default)]
    pub message_cid: Option<ContentId>,
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

/// A persistent record that a DM message exists in this Space's history
/// (sender OR recipient).
///
/// Originally InboxEntry meant "a message received from someone else";
/// Phase 4 widens the semantics so that `dm_outbox::send_dm` writes a
/// self-InboxEntry on every send, alongside the OutboxEntry. The motive:
/// OutboxEntry is delivery-state-tracking (Pending/Partial/Complete/Expired)
/// and Complete entries can be GC'd, but InboxEntry is the durable
/// scrollback record. Without a self-InboxEntry, self-sent messages would
/// vanish from the Space's history once the OutboxEntry is collected.
///
/// `from` distinguishes sender vs. receiver: `from == self_owner` for
/// self-sent messages, `from == sender_owner` for received messages.
///
/// Cross-device convergence: a paired device receiving the same
/// DmCidNotify writes its own InboxEntry on receipt with the same
/// `(space_id, message_cid)` key, so the table converges across the
/// originating + paired-receiving devices without special-casing.
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

/// A received DM message bundle — Phase 4 IPC payload carrier.
///
/// The receive path (`dm_inbox_ingest::ingest_dm_packet`, relay recover)
/// decrypts the message, then emits this struct directly as the
/// `dm-received` IPC event payload (via `dm_received_event_payload`) with
/// body + mime_type + sent_at fields the frontend needs to render the
/// message.
///
/// This widens the previous `Vec<InboxEntry>` carrier so the decrypted
/// body doesn't have to be re-fetched + re-decrypted on the IPC emit
/// path. The fields are not persisted — only InboxEntry persists; body
/// lives in CAS keyed by message_cid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedMessage {
    pub inbox_entry: InboxEntry,
    pub body: Vec<u8>,
    pub mime_type: String,
    pub sent_at: Hlc,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadMarker {
    #[serde(rename = "sp")]
    pub space_id: SpaceId,
    #[serde(rename = "lr")]
    pub last_read_at: Hlc,
}

/// User's per-library trust record. Lives in owner-state CRDT; syncs
/// across bound devices via existing Flow A. Spec §4.2.
///
/// LWW semantics for add/remove:
/// - Effective state at any HLC = `removed_at.is_none() || added_at >
///   removed_at`.
/// - Re-add at HLC > removed_at re-enables; the higher-HLC operation
///   wins.
/// - Tombstones (Some(removed_at)) are NEVER GC'd — needed for cross-
///   device convergence on add-on-A / remove-on-B at later HLC.
///
/// 2-char field keys (codebase convention; satisfies
/// `canonical_cbor_encode`'s same-length-keys precondition).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryEntry {
    /// Library OwnerAddr (also the BTreeMap key in OwnerState).
    #[serde(rename = "ad")]
    pub address: OwnerAddr,

    /// HLC when this device added the library.
    #[serde(rename = "at")]
    pub added_at: Hlc,

    /// HLC of the most-recent remove operation; None if never removed.
    /// Compared against `added_at` to determine effective state.
    #[serde(rename = "rm", skip_serializing_if = "Option::is_none")]
    pub removed_at: Option<Hlc>,
}

impl LibraryEntry {
    /// True if the user currently has this library in their trust set.
    /// Implements the LWW rule: present unless a remove with higher HLC
    /// is recorded.
    pub fn is_effective(&self) -> bool {
        match &self.removed_at {
            None => true,
            Some(rm) => self.added_at.is_strictly_newer_than(rm),
        }
    }
}

/// ZEB-674 Task 2 (C2): one owner-local record that the owner shared read
/// access to an encrypted file with a specific grantee — a row in the
/// owner's "Shared with" list. Stored in `OwnerState.file_grants` keyed by
/// the file's root ContentId, and replicated across the owner's own devices
/// via Flow A.
///
/// The sealed key is intentionally NOT stored here: sealing to the grantee's
/// devices happens at share time from the DEK (see `file_sharing`), so this
/// record only names WHO was granted, WHEN, and (if applicable) when it was
/// revoked.
///
/// This is one element of an LWW-element-set (ZEB-725): the grant is ACTIVE iff
/// `granted_at > revoked_at`. A re-share bumps `granted_at` forward (reactivate);
/// a revoke bumps `revoked_at` forward (deactivate). Both timestamps merge by
/// `max`, so a revoke CONVERGES across the owner's devices — a stale sibling can
/// no longer resurrect a revoked grant (the pre-ZEB-725 "drop the record"
/// approach let a union re-add it). Crypto access is a separate matter: an
/// already-delivered DEK cannot be withdrawn without rotation.
///
/// 2-char field keys (codebase convention; satisfies `canonical_cbor_encode`'s
/// same-length-keys precondition — mirrors `ReadMarker` / `LibraryEntry`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantEntry {
    /// The grantee's master `OwnerAddr` (their `owner_id`).
    #[serde(rename = "go")]
    pub grantee_owner: OwnerAddr,
    /// Wall-clock milliseconds when this grant was last (re-)recorded.
    #[serde(rename = "ga")]
    pub granted_at: u64,
    /// Wall-clock milliseconds of the latest revoke of this grantee for this
    /// file, or `0` if never revoked. The grant is ACTIVE iff
    /// `granted_at > revoked_at`. Absent on the wire when `0` (so a
    /// never-revoked grant encodes exactly as it did pre-ZEB-725, and
    /// pre-tombstone snapshots load with `revoked_at = 0`).
    #[serde(rename = "gv", default, skip_serializing_if = "is_zero_u64")]
    pub revoked_at: u64,
}

/// serde `skip_serializing_if` predicate: drop a `u64` field from the wire when
/// it is zero (keeps never-revoked `GrantEntry`s byte-identical to pre-tombstone
/// encoding and satisfies the equal-length-key canonical-encode precondition
/// whether or not `gv` is present).
fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

/// ZEB-674 Task 4 (C4): one grant the local owner RECEIVED — an encrypted file
/// another owner shared with one of this owner's devices. Stored in
/// `OwnerState.received_file_grants` keyed by the shared file's root ContentId
/// bytes, and replicated across the owner's own devices via Flow A. Because the
/// DEK is RE-SEALED under the grantee's own shared `KeyTree` at ingest (see
/// `file_sharing::ingest_grant_push`), ANY of the owner's bound devices can
/// render "shared with me" AND open the file — exactly like `file_deks` — not
/// only the device the deposit was originally sealed to.
///
/// `sealed_dek` is the KeyTree-sealed DEK
/// (`file_sharing::seal_dek_at_rest(keytree, dek)`), stored so
/// `open_received_file` can unseal it lazily on demand rather than caching the
/// raw DEK at rest. Confidentiality rests on the grantee's shared KeyTree — the
/// DEK never lands unsealed in `OwnerState`.
///
/// 2-char field keys (codebase convention; satisfies `canonical_cbor_encode`'s
/// same-length-keys precondition — mirrors `GrantEntry` / `FileGrantInner`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceivedFileGrant {
    /// The granting owner's master `OwnerAddr` (who shared the file).
    #[serde(rename = "gr")]
    pub granter_owner: OwnerAddr,
    /// The shared file's encrypted root ContentId, canonical 32-byte form.
    #[serde(rename = "ci")]
    pub cid: [u8; 32],
    /// Display file name (for the grantee's received-files UI).
    #[serde(rename = "nm")]
    pub file_name: String,
    /// Stored (CAS) byte length of the file's content. For v3 streaming-encrypted
    /// content this is the chunked-AEAD ciphertext length: a 9-byte header plus,
    /// per 64 KiB frame, a 16-byte tag (see `file_stream_crypto::v3_ciphertext_len`),
    /// so it exceeds the plaintext length by the header + per-frame tag overhead.
    #[serde(rename = "sz")]
    pub file_size: u64,
    /// MIME type string.
    #[serde(rename = "mt")]
    pub mime: String,
    /// The KeyTree-sealed DEK (opaque; opens with the grantee's shared KeyTree
    /// on any bound device). NEVER the raw DEK — always the sealed envelope.
    #[serde(rename = "sk")]
    pub sealed_dek: Vec<u8>,
    /// Wall-clock milliseconds when this grant was ingested.
    #[serde(rename = "ra")]
    pub received_at: u64,
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
            message_cid: Some(ContentId::from_bytes([3u8; 32])),
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

    #[test]
    fn outbox_entry_invite_only_message_cid_none_round_trips() {
        // ZEB-505: a durable invite-only entry has no message. `message_cid:
        // None` must survive the persisted-CRDT round-trip and come back None.
        let mut e = entry(vec![1, 2, 3], vec![]);
        e.message_cid = None;
        let mut bytes = Vec::new();
        ciborium::into_writer(&e, &mut bytes).unwrap();
        let recovered: OutboxEntry = ciborium::from_reader(&bytes[..]).unwrap();
        assert_eq!(e, recovered);
        assert_eq!(recovered.message_cid, None);
    }

    #[test]
    fn outbox_entry_legacy_bare_message_cid_decodes_as_some() {
        // ZEB-505 migration: a pre-change OutboxEntry encoded `mc` as a BARE
        // mandatory ContentId. Under the new `Option<ContentId>` with
        // `#[serde(default)]`, those already-persisted bytes must still load —
        // the bare value decoding to `Some(cid)` — so existing on-disk owner
        // outbox state survives the format change (ciborium encodes `Some(x)`
        // transparently as bare `x`, so new MESSAGE entries are also
        // byte-identical to the legacy encoding).
        #[derive(serde::Serialize)]
        struct LegacyOutboxEntry {
            #[serde(rename = "id")]
            id: OutboxEntryId,
            #[serde(rename = "sp")]
            space_id: SpaceId,
            #[serde(rename = "rc")]
            recipient_owners: Vec<OwnerAddr>,
            #[serde(rename = "mc")]
            message_cid: ContentId,
            #[serde(rename = "ca")]
            created_at: Hlc,
            #[serde(rename = "dl")]
            delivered_to: BTreeSet<OwnerAddr>,
            #[serde(rename = "ds")]
            delivery_status: DeliveryStatus,
        }
        let cid = ContentId::from_bytes([7u8; 32]);
        let legacy = LegacyOutboxEntry {
            id: OutboxEntryId([1u8; 16]),
            space_id: SpaceId([2u8; 16]),
            recipient_owners: vec![OwnerAddr([3u8; 16])],
            message_cid: cid,
            created_at: hlc(100),
            delivered_to: BTreeSet::new(),
            delivery_status: DeliveryStatus::Pending,
        };
        let mut bytes = Vec::new();
        ciborium::into_writer(&legacy, &mut bytes).unwrap();
        let recovered: OutboxEntry = ciborium::from_reader(&bytes[..]).unwrap();
        assert_eq!(recovered.message_cid, Some(cid));
        assert_eq!(recovered.space_id, SpaceId([2u8; 16]));
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
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
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

    /// F2 (ZEB-254 R2): non-Community Spaces must not carry pending_join_at.
    /// A malformed peer attempting to inject invalid state should be rejected
    /// by validate_invariants before it can persist or replicate.
    #[test]
    fn space_non_community_with_pending_join_at_violates_invariant() {
        let mut f = folder();
        f.pending_join_at = Some(hlc(42));
        let result = f.validate_invariants();
        assert!(
            result.is_err(),
            "Folder with pending_join_at=Some must fail validate_invariants; got Ok"
        );
        let msg = result.unwrap_err().0;
        assert!(
            msg.contains("pending_join_at"),
            "error message must mention pending_join_at; got: {msg}"
        );
    }

    #[test]
    fn dm_must_have_exactly_two_members() {
        let mk_dm = |n_members: usize| Space {
            id: SpaceId([2u8; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "DM".into(),
            transport: None,
            members: (0..n_members).map(|i| OwnerAddr([i as u8; 16])).collect(),
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(1),
            updated_at: hlc(1),
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
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
            transport: None,
            members: (0..n).map(|i| OwnerAddr([i as u8; 16])).collect(),
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(1),
            updated_at: hlc(1),
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
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
            transport: None,
            members: vec![OwnerAddr([1u8; 16]), OwnerAddr([1u8; 16])],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(1),
            updated_at: hlc(1),
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
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
            transport: None,
            // Reverse order — bob > alice but listed bob-first.
            members: vec![OwnerAddr([2u8; 16]), OwnerAddr([1u8; 16])],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(1),
            updated_at: hlc(1),
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
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
            transport: None,
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
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
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
            transport: None,
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
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
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
                current_epoch: None,
                current_epoch_key: None,
                old_epoch_keys: ::std::collections::BTreeMap::new(),
                admin_addr: None,
                is_invite_only: None,
                shared_in_profile: false,
                pending_join_at: None,
            };
        // Missing community_id → reject.
        assert!(
            mk_channel(None, Some(TransportBinding::Zenoh { topic: "t".into() }))
                .validate_invariants()
                .is_err()
        );
        // Wrong transport (None instead of Zenoh) → reject.
        // ZEB-474: was formerly tested with Reticulum variant (now removed);
        // None is the other non-Zenoh value and Channel requires Zenoh.
        assert!(mk_channel(Some(SpaceId([5u8; 16])), None)
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
            transport: None,
            members: m,
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: hlc(1),
            updated_at: hlc(1),
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
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
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
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
            transport: None,
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
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
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
            transport: None,
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
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
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
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
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
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
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
            transport: None,
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
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
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
            transport: None,
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
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
        };
        assert!(d.validate_invariants().is_err());
    }

    /// Builds a valid DM Space with the supplied prior_content_keys
    /// vec. Used by the canonical-form tests below to isolate the
    /// new sorted-strict invariant from unrelated DM-shape rules.
    fn dm_with_priors(priors: Vec<DmContentKey>) -> Space {
        Space {
            id: SpaceId([1; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "x".into(),
            transport: None,
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
            // Pick a content_key disjoint from the prior fixtures
            // below so the existing "content_key not in priors" check
            // never fires by accident.
            content_key: Some(DmContentKey::new([0xff; 32])),
            prior_content_keys: priors,
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
        }
    }

    #[test]
    fn validate_invariants_rejects_unsorted_prior_content_keys() {
        // Descending order violates the canonical-form invariant:
        // merge_prior_content_keys always emits sorted output, so a
        // non-canonical value here means malformed wire data,
        // corrupted on-disk state, or a bug elsewhere. Two replicas
        // with semantically-equal but differently-ordered priors
        // would produce different canonical CBOR bytes (and thus
        // different root_cids), breaking convergence.
        let d = dm_with_priors(vec![
            DmContentKey::new([0x22; 32]),
            DmContentKey::new([0x11; 32]),
        ]);
        let err = d.validate_invariants().unwrap_err();
        assert!(
            err.0.contains("prior_content_keys") && err.0.contains("ascending"),
            "expected prior_content_keys + ascending in error, got: {}",
            err.0
        );
    }

    #[test]
    fn validate_invariants_rejects_duplicate_prior_content_keys() {
        // Strict `<` collapses both unsorted and adjacent-duplicate
        // checks into one predicate. A duplicated key must reject
        // for the same convergence reason as unsorted.
        let dup = DmContentKey::new([0x11; 32]);
        let d = dm_with_priors(vec![dup.clone(), dup]);
        let err = d.validate_invariants().unwrap_err();
        assert!(
            err.0.contains("prior_content_keys") && err.0.contains("ascending"),
            "expected prior_content_keys + ascending in error, got: {}",
            err.0
        );
    }

    #[test]
    fn validate_invariants_accepts_sorted_deduped_prior_content_keys() {
        // Smoke check: the canonical form (strictly-ascending,
        // deduped) must still pass. Single-element and multi-element
        // sorted vectors both validate.
        let one = dm_with_priors(vec![DmContentKey::new([0x11; 32])]);
        assert!(one.validate_invariants().is_ok());

        let many = dm_with_priors(vec![
            DmContentKey::new([0x11; 32]),
            DmContentKey::new([0x22; 32]),
            DmContentKey::new([0x33; 32]),
        ]);
        assert!(many.validate_invariants().is_ok());
    }

    #[test]
    fn deserialize_rejects_oversized_prior_content_keys() {
        use ciborium::{from_reader, into_writer};
        // Build a Space with MAX_PRIOR_CONTENT_KEYS+1 distinct prior keys.
        // `merge_prior_content_keys` always emits ≤ cap entries, so any
        // wire input above the cap is malformed by definition. The
        // streaming visitor MUST reject (rather than silently truncate)
        // so a buggy peer dropping prior keys we'd need for fallback
        // decryption surfaces loudly instead of being hidden by a quiet
        // truncate.
        let oversized: Vec<DmContentKey> = (0..(MAX_PRIOR_CONTENT_KEYS as u8 + 1))
            .map(|i| DmContentKey::new([i; 32]))
            .collect();
        // dm_with_priors uses content_key = [0xff; 32]; oversized uses
        // bytes 0..=cap which never reaches 0xff, so content_key never
        // appears in priors.
        let space = dm_with_priors(oversized);

        let mut bytes = Vec::new();
        into_writer(&space, &mut bytes).expect("encode space with oversized priors");

        let err = from_reader::<Space, _>(&bytes[..]).expect_err("decode must reject oversized");
        let msg = err.to_string();
        assert!(
            msg.contains("MAX_PRIOR_CONTENT_KEYS") || msg.contains("DmContentKey"),
            "expected error mentioning cap/type, got: {msg}"
        );
    }

    #[test]
    fn deserialize_normalizes_unsorted_duplicated_prior_content_keys() {
        use ciborium::{from_reader, into_writer};
        // Build a Space with within-cap pathologies (unsorted +
        // duplicates) — total entries ≤ MAX_PRIOR_CONTENT_KEYS so the
        // cap-rejection path doesn't fire. After CBOR round-trip the
        // deserialize_with hook MUST restore canonical form
        // (strictly-ascending bytes, no duplicates). Without this hook
        // a corrupted on-disk file would round-trip through save_crdt
        // unchanged and break root_cid convergence with peers.
        //
        // Layout: 4 copies of [0xee; 32] (dups) + 10 distinct descending
        // keys = 14 entries total (within the 16-key cap).
        let mut malformed: Vec<DmContentKey> = Vec::with_capacity(14);
        for _ in 0..4 {
            malformed.push(DmContentKey::new([0xee; 32]));
        }
        for i in (0..10u8).rev() {
            malformed.push(DmContentKey::new([i; 32]));
        }
        assert!(
            malformed.len() <= MAX_PRIOR_CONTENT_KEYS,
            "test fixture must stay within cap to isolate the normalize path"
        );
        // dm_with_priors uses content_key = [0xff; 32]; malformed uses
        // bytes 0..10 plus 0xee, so content_key never appears in priors.
        let space = dm_with_priors(malformed);

        let mut bytes = Vec::new();
        into_writer(&space, &mut bytes).expect("encode space with malformed priors");

        let decoded: Space = from_reader(&bytes[..]).expect("decode space");

        // After dedup we have 10 distinct + 1 [0xee] = 11 keys.
        assert_eq!(
            decoded.prior_content_keys.len(),
            11,
            "expected 11 unique keys after dedup"
        );
        // Strictly ascending by raw bytes — required for canonical-form
        // convergence and for validate_invariants to accept on next apply.
        assert!(
            decoded
                .prior_content_keys
                .windows(2)
                .all(|w| w[0].as_bytes() < w[1].as_bytes()),
            "expected strictly-ascending priors after normalization"
        );
        // Position 0 is lex-smallest: the [0; 32] key.
        assert_eq!(
            decoded.prior_content_keys[0].as_bytes(),
            &[0u8; 32],
            "expected lex-smallest key (all zeros) at position 0"
        );
        // Round-tripped Space must still pass validate_invariants —
        // proves the normalized form satisfies every downstream check.
        decoded
            .validate_invariants()
            .expect("normalized priors must pass validate_invariants");
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
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
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
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
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

    #[test]
    fn community_space_round_trips_with_new_fields() {
        use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};

        let admin = OwnerAddr([1u8; 16]);
        let community_id = SpaceId([2u8; 16]);
        let key = EpochKey::new([3u8; 32]);

        let space = Space {
            id: community_id,
            kind: SpaceKind::Community,
            parent: None,
            community_id: None, // community Space IS the community
            name: "harmony-design".to_string(),
            transport: None,
            members: vec![], // membership lives in CommunityState CRDT
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "d".into(),
            },
            updated_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "d".into(),
            },
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: Some(0),
            current_epoch_key: Some(key.clone()),
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: Some(admin),
            is_invite_only: Some(true),
            shared_in_profile: false,
            pending_join_at: None,
        };

        let encoded = canonical_cbor_encode(&space).expect("encode");
        let decoded: Space = canonical_cbor_decode(&encoded).expect("decode");

        assert_eq!(decoded.kind, SpaceKind::Community);
        // Compare raw bytes (not EpochKey directly) so the failure
        // message shows actual hex on mismatch — EpochKey's Debug
        // impl redacts the bytes, which would render assert_eq! useless
        // on failure.
        assert_eq!(
            decoded.current_epoch_key.as_ref().map(|k| *k.as_bytes()),
            Some(*key.as_bytes())
        );
        assert_eq!(decoded.admin_addr, Some(admin));
        assert_eq!(decoded.is_invite_only, Some(true));

        // Activated by Task 3 once SpaceKind::Community gets enforced
        // invariants. Currently a no-op; the call here means Task 3 can
        // land without needing to revisit this test, AND any future
        // regression in round-trip that breaks invariant fields will be
        // caught here.
        decoded
            .validate_invariants()
            .expect("community Space must pass validate_invariants after round-trip");
    }

    #[test]
    fn community_space_validates_when_all_required_fields_present() {
        let s = Space {
            id: SpaceId([1u8; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None,
            name: "ok".into(),
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
            prior_content_keys: vec![],
            current_epoch: Some(0),
            current_epoch_key: Some(EpochKey::new([0u8; 32])),
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: Some(OwnerAddr([2u8; 16])),
            is_invite_only: Some(false),
            shared_in_profile: false,
            pending_join_at: None,
        };
        assert!(s.validate_invariants().is_ok());
    }

    #[test]
    fn community_space_rejects_missing_current_epoch_key() {
        let s = Space {
            id: SpaceId([1u8; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None,
            name: "x".into(),
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
            prior_content_keys: vec![],
            current_epoch: Some(0),
            current_epoch_key: None, // ← invariant violation
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: Some(OwnerAddr([2u8; 16])),
            is_invite_only: Some(false),
            shared_in_profile: false,
            pending_join_at: None,
        };
        let err = s.validate_invariants().expect_err("must reject");
        assert!(err.0.contains("current_epoch_key"));
    }

    #[test]
    fn community_space_rejects_missing_admin_addr() {
        let s = Space {
            id: SpaceId([1u8; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None,
            name: "x".into(),
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
            prior_content_keys: vec![],
            current_epoch: Some(0),
            current_epoch_key: Some(EpochKey::new([0u8; 32])),
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: None, // ← invariant violation
            is_invite_only: Some(false),
            shared_in_profile: false,
            pending_join_at: None,
        };
        let err = s.validate_invariants().expect_err("must reject");
        assert!(err.0.contains("admin_addr"));
    }

    #[test]
    fn community_space_rejects_missing_is_invite_only() {
        let s = Space {
            id: SpaceId([1u8; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None,
            name: "x".into(),
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
            prior_content_keys: vec![],
            current_epoch: Some(0),
            current_epoch_key: Some(EpochKey::new([0u8; 32])),
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: Some(OwnerAddr([2u8; 16])),
            is_invite_only: None, // ← invariant violation
            shared_in_profile: false,
            pending_join_at: None,
        };
        let err = s.validate_invariants().expect_err("must reject");
        assert!(err.0.contains("is_invite_only"));
    }

    #[test]
    fn community_space_rejects_community_id_present() {
        let s = Space {
            id: SpaceId([1u8; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: Some(SpaceId([99u8; 16])), // ← invariant violation
            name: "x".into(),
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
            prior_content_keys: vec![],
            current_epoch: Some(0),
            current_epoch_key: Some(EpochKey::new([0u8; 32])),
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: Some(OwnerAddr([2u8; 16])),
            is_invite_only: Some(false),
            shared_in_profile: false,
            pending_join_at: None,
        };
        let err = s.validate_invariants().expect_err("must reject");
        assert!(err.0.contains("community_id=None"));
    }

    #[test]
    fn community_space_rejects_content_key_present() {
        let s = Space {
            id: SpaceId([1u8; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None,
            name: "x".into(),
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
            content_key: Some(DmContentKey::new([7u8; 32])), // ← invariant violation
            prior_content_keys: vec![],
            current_epoch: Some(0),
            current_epoch_key: Some(EpochKey::new([0u8; 32])),
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: Some(OwnerAddr([2u8; 16])),
            is_invite_only: Some(false),
            shared_in_profile: false,
            pending_join_at: None,
        };
        let err = s.validate_invariants().expect_err("must reject");
        assert!(err.0.contains("content_key=None"));
    }

    #[test]
    fn community_space_rejects_non_empty_members() {
        let s = Space {
            id: SpaceId([1u8; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None,
            name: "x".into(),
            transport: None,
            members: vec![OwnerAddr([99u8; 16])], // ← invariant violation
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
            current_epoch: Some(0),
            current_epoch_key: Some(EpochKey::new([0u8; 32])),
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: Some(OwnerAddr([2u8; 16])),
            is_invite_only: Some(false),
            shared_in_profile: false,
            pending_join_at: None,
        };
        let err = s.validate_invariants().expect_err("must reject");
        assert!(
            err.0.contains("members=[]"),
            "expected error mentioning empty members invariant; got: {}",
            err.0
        );
    }

    #[test]
    fn community_space_rejects_non_empty_prior_content_keys() {
        // ZEB-216 §"Validate invariants extension" requires non-DM kinds
        // (including Community) to have prior_content_keys=[]. The
        // community arm of the content-key match doesn't fall through
        // to the catch-all rule because the catch-all also rejects
        // content_key=Some, which Community has its own rule for —
        // so the prior_content_keys check must be present in the
        // Community arm too.
        let s = Space {
            id: SpaceId([1u8; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None,
            name: "x".into(),
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
            prior_content_keys: vec![DmContentKey::new([5u8; 32])], // ← invariant violation
            current_epoch: Some(0),
            current_epoch_key: Some(EpochKey::new([0u8; 32])),
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: Some(OwnerAddr([2u8; 16])),
            is_invite_only: Some(false),
            shared_in_profile: false,
            pending_join_at: None,
        };
        let err = s.validate_invariants().expect_err("must reject");
        assert!(
            err.0.contains("prior_content_keys"),
            "expected error mentioning prior_content_keys invariant; got: {}",
            err.0
        );
    }

    #[test]
    fn community_space_rejects_transport_present() {
        let s = Space {
            id: SpaceId([1u8; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None,
            name: "x".into(),
            transport: Some(TransportBinding::Zenoh {
                topic: "wat".into(),
            }),
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
            prior_content_keys: vec![],
            current_epoch: Some(0),
            current_epoch_key: Some(EpochKey::new([0u8; 32])),
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: Some(OwnerAddr([2u8; 16])),
            is_invite_only: Some(false),
            shared_in_profile: false,
            pending_join_at: None,
        };
        let err = s.validate_invariants().expect_err("must reject");
        assert!(err.0.contains("transport=None"));
    }

    #[test]
    fn dm_space_rejects_epoch_key_present() {
        let s = Space {
            id: SpaceId([1u8; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "dm".into(),
            transport: None,
            members: vec![OwnerAddr([1u8; 16]), OwnerAddr([2u8; 16])],
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
            content_key: Some(DmContentKey::new([5u8; 32])),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: Some(EpochKey::new([7u8; 32])), // ← wrong kind
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
        };
        let err = s.validate_invariants().expect_err("must reject");
        assert!(
            err.0.contains("current_epoch_key"),
            "expected error about non-community current_epoch_key; got: {}",
            err.0
        );
    }

    /// Sub-D Phase 4 (ZEB-281): a Community Space with the opt-in flag
    /// set MUST validate. Counterpart to
    /// `non_community_with_shared_in_profile_true_rejected` below.
    #[test]
    fn community_space_with_shared_in_profile_true_accepted() {
        let s = Space {
            id: SpaceId([1u8; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None,
            name: "opted-in community".into(),
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
            prior_content_keys: vec![],
            current_epoch: Some(0),
            current_epoch_key: Some(EpochKey::new([0u8; 32])),
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: Some(OwnerAddr([2u8; 16])),
            is_invite_only: Some(false),
            shared_in_profile: true,
            pending_join_at: None,
        };
        assert_eq!(s.validate_invariants(), Ok(()));
    }

    /// Sub-D Phase 4 (ZEB-281): a non-Community Space with the opt-in
    /// flag set MUST be rejected. Defends against a malformed peer or
    /// future-self bug that tries to advertise a DM / channel / folder
    /// in the public profile broadcast.
    #[test]
    fn non_community_with_shared_in_profile_true_rejected() {
        let s = Space {
            id: SpaceId([2u8; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "DM".into(),
            transport: None,
            members: vec![OwnerAddr([1u8; 16]), OwnerAddr([2u8; 16])],
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
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: true, // ← invariant violation
            pending_join_at: None,
        };
        let err = s
            .validate_invariants()
            .expect_err("non-community must reject shared_in_profile=true");
        assert!(
            err.0.contains("shared_in_profile=false"),
            "expected error mentioning shared_in_profile invariant; got: {}",
            err.0
        );
    }

    #[test]
    fn non_community_space_skips_membership_fields_in_wire() {
        use crate::owner_state_crypto::canonical_cbor_encode;

        let dm = Space {
            id: SpaceId([1u8; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "dm".to_string(),
            transport: None,
            members: vec![OwnerAddr([2u8; 16]), OwnerAddr([3u8; 16])],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "d".into(),
            },
            updated_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "d".into(),
            },
            content_key: Some(DmContentKey::new([5u8; 32])),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: ::std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
        };

        let bytes = canonical_cbor_encode(&dm).expect("encode");
        // skip_serializing_if guarantees these CBOR map keys DON'T appear
        // in the encoded blob for non-community Spaces (defense against
        // wire-bloat regression). Each 2-char key encodes as CBOR text(2):
        // 0x62 (major type 3, length 2) followed by the two ASCII bytes.
        // We check the full 3-byte sequence to avoid false positives from
        // data values that incidentally contain the same 2-char byte pair
        // (e.g. device_id "d" = 0x61 0x64 would spuriously match "ad").
        let needles: &[[u8; 3]] = &[
            [0x62, b'c', b'e'], // CBOR text(2) "ce"
            [0x62, b'e', b'k'], // CBOR text(2) "ek"
            [0x62, b'o', b'k'], // CBOR text(2) "ok"
            [0x62, b'a', b'd'], // CBOR text(2) "ad"
            [0x62, b'i', b'o'], // CBOR text(2) "io"
        ];
        for needle in needles {
            let found = bytes.windows(3).any(|w| w == needle);
            assert!(
                !found,
                "non-community Space wire blob contained CBOR key {:?} — \
                 skip_serializing_if regression",
                std::str::from_utf8(&needle[1..]).unwrap()
            );
        }
    }

    #[test]
    fn space_with_pending_join_at_round_trip() {
        // R3 (M1): fixture must be internally consistent. Community Spaces
        // require `current_epoch_key: Some(...)` per validate_invariants —
        // the prior fixture set `current_epoch: Some(0)` with `current_epoch_key:
        // None`, which would fail invariant validation in any context that
        // performs it. The round-trip test only exercises serde, but a valid
        // fixture documents the wire shape correctly.
        let admin = OwnerAddr([1u8; 16]);
        let space = Space {
            id: SpaceId([7u8; 16]),
            kind: SpaceKind::Community,
            parent: None,
            community_id: None,
            name: "test community".into(),
            transport: None,
            members: vec![],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "admin".into(),
            },
            updated_at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "admin".into(),
            },
            content_key: None,
            prior_content_keys: vec![],
            current_epoch: Some(0),
            current_epoch_key: Some(EpochKey([0x42u8; 32])),
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: Some(admin),
            is_invite_only: Some(true),
            shared_in_profile: false,
            pending_join_at: Some(Hlc {
                wall_ms: 1_700_000_000_500,
                logical: 0,
                device_id: "joiner".into(),
            }),
        };
        let encoded = crate::owner_state_crypto::canonical_cbor_encode(&space).expect("encode");
        let decoded: Space = ciborium::from_reader(&mut encoded.as_slice()).expect("decode");
        assert_eq!(space, decoded);
    }

    #[test]
    fn space_without_pending_join_at_omits_field() {
        // Pre-ZEB-254 Space (pending_join_at = None) must encode WITHOUT
        // the "pj" key — skip_serializing_if guarantees wire compat.
        let admin = OwnerAddr([1u8; 16]);
        let space = Space {
            id: SpaceId([7u8; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "dm".into(),
            transport: None,
            members: vec![OwnerAddr([1u8; 16]), OwnerAddr([2u8; 16])],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "admin".into(),
            },
            updated_at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "admin".into(),
            },
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: vec![],
            current_epoch: None,
            current_epoch_key: None,
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: None,
            is_invite_only: None,
            shared_in_profile: false,
            pending_join_at: None,
        };
        let _ = admin; // suppress unused warning — admin is for test symmetry
        let encoded = crate::owner_state_crypto::canonical_cbor_encode(&space).expect("encode");
        // The "pj" key (3-byte CBOR text(2) prefix: 0x62 'p' 'j') must NOT appear.
        assert!(
            !encoded.windows(3).any(|w| w == [0x62, b'p', b'j']),
            "Space with pending_join_at=None must omit the pj key from canonical CBOR"
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

    /// Build a real (DeviceIdentityHash, [u8; 64]) pair from a seed where
    /// the hash IS `derive_device_hash_from_identity_pub(&pub)`. Required
    /// for any test that exercises a path now gated by the
    /// pub-derives-to-hash invariant added in this commit — placeholder
    /// pubs (e.g., `[0x42u8; 64]`) would now be rejected as
    /// `D::Error::custom(...)` and mask the test's intent.
    fn matching_device_pair(seed_byte: u8) -> (DeviceIdentityHash, [u8; 64]) {
        let private = harmony_identity::PrivateIdentity::from_seed(&[seed_byte; 32]);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let device_hash = DeviceIdentityHash(public.address_hash);
        (device_hash, identity_pub)
    }

    #[test]
    fn deserialize_rejects_oversized_devices() {
        // A wire input with > MAX_DEVICES_PER_OWNER (32) device hashes
        // is malformed by definition: `apply_owner_device_update` always
        // emits ≤ cap entries. The streaming visitor MUST reject (rather
        // than silently truncate) so a buggy peer dropping device entries
        // we'd need for DM delivery surfaces loudly. This is also the
        // OOM-safety property: an attacker declaring `array(2^32-1)` of
        // 16-byte hashes would otherwise force a multi-GB allocation
        // before any cap took effect.
        let oversized: Vec<DeviceIdentityHash> = (0..(MAX_DEVICES_PER_OWNER as u8 + 1))
            .map(|i| DeviceIdentityHash([i; 16]))
            .collect();
        let raw = RawOwnerDeviceEntry {
            v: oversized,
            l: hlc(7),
        };

        let mut bytes = Vec::new();
        ciborium::into_writer(&raw, &mut bytes).expect("encode raw");

        let err = ciborium::from_reader::<OwnerDeviceEntry, _>(&bytes[..])
            .expect_err("decode must reject oversized");
        let msg = err.to_string();
        assert!(
            msg.contains("MAX_DEVICES_PER_OWNER") || msg.contains("DeviceIdentityHash"),
            "expected error mentioning cap/type, got: {msg}"
        );
    }

    #[test]
    fn deserialize_normalizes_unsorted_duplicated_devices() {
        // Build a payload with within-cap pathologies (duplicates +
        // unsorted) — total entries ≤ MAX_DEVICES_PER_OWNER so the
        // cap-rejection path doesn't fire. After normalization the
        // result must be sorted and deduped — anything else breaks
        // binary_search in resolve_link_origin_owner (Phase 3b).
        //
        // Layout: 5 copies of [0xff; 16] + 25 distinct descending
        // hashes = 30 entries total (within the 32-device cap).
        let mut malformed: Vec<DeviceIdentityHash> = Vec::with_capacity(30);
        for _ in 0..5 {
            malformed.push(DeviceIdentityHash([0xff; 16]));
        }
        for i in (0..25u8).rev() {
            malformed.push(DeviceIdentityHash([i; 16]));
        }
        assert!(
            malformed.len() <= MAX_DEVICES_PER_OWNER,
            "test fixture must stay within cap to isolate the normalize path"
        );

        let raw = RawOwnerDeviceEntry {
            v: malformed,
            l: hlc(7),
        };

        let mut bytes = Vec::new();
        ciborium::into_writer(&raw, &mut bytes).expect("encode raw");

        let entry: OwnerDeviceEntry = ciborium::from_reader(&bytes[..]).expect("decode entry");

        // After dedup: 25 distinct (0..25) + 1 [0xff; 16] = 26 hashes.
        assert_eq!(entry.devices.len(), 26);
        // Sorted ascending — required for binary_search.
        assert!(entry.devices.windows(2).all(|w| w[0] <= w[1]));
        // Deduped — no consecutive equal entries (and given sorted, no dups
        // anywhere).
        assert!(entry.devices.windows(2).all(|w| w[0] != w[1]));
        // Lex-smallest survives normalization: the smallest hash is
        // [0; 16], so position 0 must be [0; 16].
        assert_eq!(entry.devices[0], DeviceIdentityHash([0; 16]));
        // learned_at preserved from the wire.
        assert_eq!(entry.learned_at, hlc(7));
    }

    #[test]
    fn owner_device_entry_serialize_includes_identity_pubs() {
        // Use a real matching (hash, pub) pair so the new
        // pub-derives-to-hash invariant in struct-level Deserialize
        // doesn't reject the round-trip.
        let (h1, p1) = matching_device_pair(0xa1);
        let entry = OwnerDeviceEntry {
            devices: vec![h1],
            device_identity_pubs: vec![Some(p1)],
            learned_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            device_tunnel_contacts: vec![None],
        };
        let bytes = crate::owner_state_crypto::canonical_cbor_encode(&entry).unwrap();
        let recovered: OwnerDeviceEntry =
            crate::owner_state_crypto::canonical_cbor_decode(&bytes).unwrap();
        assert_eq!(entry, recovered);
    }

    #[test]
    fn owner_device_entry_serialize_handles_unknown_pubs() {
        // Mid-bootstrap state: device hash is known but pub isn't yet
        // cached. The Option::None entries must round-trip cleanly.
        // The Some(pub) entry uses a real derive-matching pair so the
        // new pub-derives-to-hash invariant doesn't fire.
        let (h1, p1) = matching_device_pair(0xa1);
        let (h2, _) = matching_device_pair(0xa2);
        // Pre-sort so the post-deserialize sorted order is unambiguous
        // (the deserialize impl re-sorts; the serialize side does not).
        let (devices, pubs) = if h1 < h2 {
            (vec![h1, h2], vec![Some(p1), None])
        } else {
            (vec![h2, h1], vec![None, Some(p1)])
        };
        let entry = OwnerDeviceEntry {
            devices: devices.clone(),
            device_identity_pubs: pubs.clone(),
            learned_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            device_tunnel_contacts: vec![None; devices.len()],
        };
        let bytes = crate::owner_state_crypto::canonical_cbor_encode(&entry).unwrap();
        let recovered: OwnerDeviceEntry =
            crate::owner_state_crypto::canonical_cbor_decode(&bytes).unwrap();
        assert_eq!(entry, recovered);
        // The Some(pub) entry survives round-trip; the None entry stays None.
        assert!(recovered.device_identity_pubs.contains(&Some(p1)));
        assert!(recovered.device_identity_pubs.contains(&None));
    }

    #[test]
    fn owner_device_entry_loads_pre_phase3b_snapshot_pads_pubs_to_devices_len() {
        // Phase 1/2 snapshots stored only `devices` and `learned_at`.
        // Phase 3b adds `device_identity_pubs` — old snapshots load and
        // the struct-level Deserialize impl pads pubs to
        // `vec![None; devices.len()]` so the parallel-vec invariant
        // (`pubs.len() == devices.len()`) holds in-memory regardless of
        // wire shape. Path B's signature verification then drops packets
        // from those devices as UnknownSigningKey (handler-side
        // semantic; this test only verifies the deserializer-side
        // contract).
        //
        // Construction: build the OLD shape via a stripped-down struct
        // that mirrors Phase 1/2's OwnerDeviceEntry, encode it, then
        // decode under the NEW OwnerDeviceEntry definition. This is
        // more robust than pinning hand-crafted CBOR bytes (which would
        // lock in serde's current encoding choices and become brittle).
        #[derive(serde::Serialize)]
        struct OldOwnerDeviceEntry {
            #[serde(rename = "v")]
            devices: Vec<DeviceIdentityHash>,
            #[serde(rename = "l")]
            learned_at: Hlc,
        }
        let old = OldOwnerDeviceEntry {
            devices: vec![
                DeviceIdentityHash([0xa1; 16]),
                DeviceIdentityHash([0xa2; 16]),
            ],
            learned_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
        };
        // OldOwnerDeviceEntry is local to this test (cannot impl the
        // sealed CanonicalPayload trait), so encode via ciborium
        // directly. Decode goes through the sealed canonical decoder
        // since OwnerDeviceEntry IS a registered canonical type.
        let mut bytes = Vec::new();
        ciborium::into_writer(&old, &mut bytes).expect("encode old shape");
        let recovered: OwnerDeviceEntry =
            crate::owner_state_crypto::canonical_cbor_decode(&bytes).unwrap();
        assert_eq!(
            recovered.devices,
            vec![
                DeviceIdentityHash([0xa1; 16]),
                DeviceIdentityHash([0xa2; 16]),
            ]
        );
        assert_eq!(
            recovered.device_identity_pubs,
            vec![None, None],
            "old snapshot must load with pubs padded to devices.len() (graceful upgrade)"
        );
        assert_eq!(recovered.learned_at.wall_ms, 1);
    }

    /// Variant of the parallel `RawOwnerDeviceEntry` that includes the
    /// pubs field, so we can plant deliberately-malformed (non-canonical)
    /// {devices, pubs} pairs on the wire and assert the new struct-level
    /// `Deserialize` impl re-pairs them through a JOINT sort (the
    /// pre-fix bug: independent per-field sort would shuffle pubs out
    /// of alignment).
    #[derive(Serialize)]
    struct RawOwnerDeviceEntryWithPubs {
        #[serde(rename = "v")]
        v: Vec<DeviceIdentityHash>,
        #[serde(rename = "p", serialize_with = "serialize_device_identity_pubs")]
        p: Vec<Option<[u8; 64]>>,
        #[serde(rename = "l")]
        l: Hlc,
    }

    #[test]
    fn owner_device_entry_deserialize_normalizes_unsorted_devices_and_pubs_together() {
        // The pre-fix bug: `#[serde(deserialize_with = ...)]` on each
        // field independently meant `devices` got sorted but `pubs`
        // stayed in wire order. A non-canonical snapshot then loaded
        // with re-ordered devices paired with the WRONG pubs.
        //
        // Wire shape (deliberately non-canonical):
        //   devices = [d_high, d_low]  (descending — would be sorted to [d_low, d_high])
        //   pubs    = [P_high, P_low]  (parallel to wire devices)
        //
        // Under the fix, devices and pubs are zipped, sorted by hash,
        // then re-split — pubs follow devices through the sort and the
        // result is [d_low, d_high] paired with [P_low, P_high].
        //
        // Each (hash, pub) is a real derive-matching pair so the
        // pub-derives-to-hash invariant doesn't fire. We then
        // disambiguate which is "low" vs "high" by lex order on the
        // derived hashes.
        let (h_a, p_a) = matching_device_pair(0xa1);
        let (h_b, p_b) = matching_device_pair(0xa2);
        let (low_h, low_p, high_h, high_p) = if h_a < h_b {
            (h_a, p_a, h_b, p_b)
        } else {
            (h_b, p_b, h_a, p_a)
        };

        let raw = RawOwnerDeviceEntryWithPubs {
            v: vec![high_h, low_h], // descending on the wire
            p: vec![Some(high_p), Some(low_p)],
            l: hlc(7),
        };
        let mut bytes = Vec::new();
        ciborium::into_writer(&raw, &mut bytes).expect("encode raw");

        let entry: OwnerDeviceEntry = ciborium::from_reader(&bytes[..]).expect("decode entry");

        assert_eq!(
            entry.devices,
            vec![low_h, high_h],
            "devices sorted ascending"
        );
        assert_eq!(
            entry.device_identity_pubs,
            vec![Some(low_p), Some(high_p)],
            "pubs MUST follow devices through the sort \
             (parallel-vec correspondence preserved)"
        );
    }

    #[test]
    fn owner_device_entry_deserialize_old_snapshot_pads_pubs() {
        // Old snapshot (pre-Phase-3b): pubs field absent entirely. The
        // struct-level Deserialize impl pads to `vec![None;
        // devices.len()]` so the parallel-vec invariant holds regardless
        // of wire shape.
        #[derive(Serialize)]
        struct OldEntry {
            #[serde(rename = "v")]
            v: Vec<DeviceIdentityHash>,
            #[serde(rename = "l")]
            l: Hlc,
        }
        let old = OldEntry {
            v: vec![
                DeviceIdentityHash([0x01; 16]),
                DeviceIdentityHash([0x02; 16]),
                DeviceIdentityHash([0x03; 16]),
            ],
            l: hlc(11),
        };
        let mut bytes = Vec::new();
        ciborium::into_writer(&old, &mut bytes).expect("encode old");

        let entry: OwnerDeviceEntry = ciborium::from_reader(&bytes[..]).expect("decode entry");

        assert_eq!(entry.devices.len(), 3);
        assert_eq!(
            entry.device_identity_pubs,
            vec![None, None, None],
            "missing/empty pubs MUST be padded to devices.len() with None"
        );
    }

    #[test]
    fn owner_device_entry_deserialize_rejects_conflicting_pubs() {
        // Two entries with the SAME device hash but DIFFERENT identity
        // pubs is a real invariant violation (a peer claimed two
        // different identity pubs for the same DeviceIdentityHash —
        // either malicious or a bug in their bootstrap path). The
        // struct-level Deserialize impl rejects via D::Error::custom.
        let d = DeviceIdentityHash([0x11; 16]);
        let p_a = [0xaa; 64];
        let p_b = [0xbb; 64];

        let raw = RawOwnerDeviceEntryWithPubs {
            v: vec![d, d],
            p: vec![Some(p_a), Some(p_b)],
            l: hlc(7),
        };
        let mut bytes = Vec::new();
        ciborium::into_writer(&raw, &mut bytes).expect("encode raw");

        let err = ciborium::from_reader::<OwnerDeviceEntry, _>(&bytes[..])
            .expect_err("decode must reject conflicting pubs for same device");
        let msg = err.to_string();
        assert!(
            msg.contains("conflicting identity pubs"),
            "expected error mentioning conflicting identity pubs, got: {msg}"
        );
    }

    #[test]
    fn owner_device_entry_deserialize_merges_some_over_none_on_duplicate_hash() {
        // [d, d] with pubs = [None, Some(P)] must dedup to [d] with
        // pub = Some(P) — the merge rule prefers Some over None
        // regardless of order. The pre-fix `dedup_by_key` would have
        // kept the FIRST entry and dropped the Some.
        // Real (hash, pub) pair so the new pub-derives-to-hash invariant
        // doesn't fire.
        let (d, p) = matching_device_pair(0xa1);

        let raw = RawOwnerDeviceEntryWithPubs {
            v: vec![d, d],
            p: vec![None, Some(p)],
            l: hlc(7),
        };
        let mut bytes = Vec::new();
        ciborium::into_writer(&raw, &mut bytes).expect("encode raw");

        let entry: OwnerDeviceEntry = ciborium::from_reader(&bytes[..]).expect("decode entry");
        assert_eq!(entry.devices, vec![d]);
        assert_eq!(
            entry.device_identity_pubs,
            vec![Some(p)],
            "merge MUST prefer Some over None"
        );
    }

    #[test]
    fn owner_device_entry_deserialize_rejects_pub_with_mismatched_hash() {
        // Defense-in-depth (mirror of `apply_owner_device_update`'s
        // pub-derives-to-hash check): a snapshot or remote-replica blob
        // that pairs a `Some(identity_pub)` with a `DeviceIdentityHash`
        // that the pub does NOT derive to is malformed/poisoned.
        // Loading it would silently break every later signature verify
        // in `resolve_signed_origin_owner` — reject at deserialize time.
        //
        // Use a STRUCTURALLY-VALID pub (derived from a real
        // PrivateIdentity) paired with a hash that does NOT derive from
        // it — isolates the derives-to-different-hash branch.
        let (real_hash, real_pub) = matching_device_pair(0xa1);
        let mismatched_hash = DeviceIdentityHash([0x42; 16]);
        assert_ne!(
            real_hash, mismatched_hash,
            "test fixture must keep mismatched_hash distinct from the derived hash"
        );

        let raw = RawOwnerDeviceEntryWithPubs {
            v: vec![mismatched_hash],
            p: vec![Some(real_pub)],
            l: hlc(7),
        };
        let mut bytes = Vec::new();
        ciborium::into_writer(&raw, &mut bytes).expect("encode raw");

        let err = ciborium::from_reader::<OwnerDeviceEntry, _>(&bytes[..])
            .expect_err("decode must reject pub that does not derive to its paired hash");
        let msg = err.to_string();
        assert!(
            msg.contains("identity pub") && msg.contains("device hash"),
            "expected error mentioning identity pub deriving to a different device hash, got: {msg}"
        );
    }
}

#[cfg(test)]
mod epoch_key_tests {
    use super::*;

    #[test]
    fn epoch_key_round_trips_through_canonical_cbor() {
        use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};

        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = i as u8;
        }
        let key = EpochKey::new(bytes);

        let encoded = canonical_cbor_encode(&key).expect("encode");
        let decoded: EpochKey = canonical_cbor_decode(&encoded).expect("decode");

        assert_eq!(key.as_bytes(), decoded.as_bytes());
    }

    #[test]
    fn epoch_key_serializes_as_bstr_32() {
        use ciborium::into_writer;
        let k = EpochKey::new([0u8; 32]);
        let mut bytes = Vec::new();
        into_writer(&k, &mut bytes).unwrap();
        // bstr(32): 0x58 0x20 || <32 bytes> = 34 bytes total.
        assert_eq!(bytes.len(), 34);
        assert_eq!(bytes[0], 0x58);
        assert_eq!(bytes[1], 0x20);
    }

    #[test]
    fn epoch_key_debug_is_redacted() {
        let k = EpochKey::new([0xab; 32]);
        let s = format!("{:?}", k);
        // No raw byte values, no hex, no decimal — must be a fixed redacted form.
        assert!(!s.contains("0xab"));
        assert!(!s.contains("171")); // 0xab as decimal
        assert!(s.contains("redacted") || s.contains("REDACTED") || s.contains("***"));
    }

    #[test]
    fn epoch_key_zeroized_on_drop() {
        // Use ZeroizeOnDrop's invariant: dropping the wrapper zeros the
        // underlying [u8; 32]. We can't easily observe the freed memory,
        // but we can verify the trait is implemented by constraining a
        // generic function.
        fn assert_zeroize_on_drop<T: zeroize::ZeroizeOnDrop>() {}
        assert_zeroize_on_drop::<EpochKey>();
    }

    #[test]
    fn epoch_key_random_produces_distinct_values() {
        let a = EpochKey::random();
        let b = EpochKey::random();
        assert_ne!(a.as_bytes(), b.as_bytes(), "OsRng entropy was identical?!");
    }
}
