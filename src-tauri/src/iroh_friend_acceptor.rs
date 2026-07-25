//! ZEB-370 Phase 1 (Tasks 7-8): the `harmony/friend/v1` friend-link control
//! protocol — wire types, length-prefixed codec, point-to-point enrolled-device
//! authentication, and the inbound handshake acceptor.
//!
//! ## Identity & auth model (spec §3)
//!
//! A friend link is authenticated by the requester's **device-#2 Ed25519
//! signature** plus their **`EnrollmentCert`** (the ZEB-339 model), applied
//! point-to-point (no `SignedMembershipEvent` wrapper). Verification routes
//! through the [`crate::enrollment_verify`] chokepoint (ZEB-677): Master certs
//! verify self-contained; Quorum certs verify against the Master-issued
//! signer-cert bundle the peer presents (`signer_certs` wire field, depth-1
//! chain carriage). The cert's `owner_id` is bound to the claimed owner, and
//! the enrolled device key is what the handshake signature is verified
//! against. This is [`verify_enrolled_device`].
//!
//! Friends are keyed on the master `owner_id`; a friend's `master_ed25519`
//! anchor comes from the cert's `Master` issuer — or, for Quorum-issued
//! certs, from the verified signer certs (which all carry the same master).
//!
//! ## Wire protocol
//!
//! Both directions use `[u32 LE length-prefix][canonical-ish CBOR body]` over an
//! iroh bi-stream on the `harmony/friend/v1` ALPN, mirroring
//! `iroh_invite_acceptor`'s framing. Bodies are encoded with `ciborium`
//! (`into_writer`/`from_reader`); decode bounds the body at
//! [`FRIEND_MAX_PACKET_LEN`].
//!
//! Requester → acceptor: [`FriendLinkRequest`].
//! Acceptor → requester: [`FriendLinkAccepted`].

use crate::owner_state_types::{
    deserialize_bytes_from_bstr, serialize_bytes_as_bstr, DeviceIdentityHash, OwnerAddr,
};
use harmony_owner::certs::{EnrollmentCert, RevocationCert};
use serde::{Deserialize, Serialize};

/// Serde for `Option<[u8; 64]>` as an optional CBOR bstr (None → CBOR null /
/// absent via skip_serializing_if; Some → bstr(64)). Lets `token_sig` be absent
/// for Path A (no token) while keeping the Some-encoding byte-identical to a
/// bare bstr(64).
mod opt_bstr64 {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &Option<[u8; 64]>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(b) => s.serialize_some(serde_bytes::Bytes::new(b)),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<[u8; 64]>, D::Error> {
        let opt: Option<serde_bytes::ByteBuf> = Option::deserialize(d)?;
        match opt {
            Some(b) => {
                let arr: [u8; 64] = b
                    .as_ref()
                    .try_into()
                    .map_err(|_| serde::de::Error::custom("token_sig must be 64 bytes"))?;
                Ok(Some(arr))
            }
            None => Ok(None),
        }
    }
}

/// Serde for `Vec<Option<[u8; 64]>>` as a CBOR array whose elements are each
/// either a bstr(64) (`Some`) or CBOR null (`None`). Mirrors
/// `owner_state_types::{serialize,deserialize}_device_identity_pubs` (the
/// `OwnerDeviceEntry.device_identity_pubs` parallel-vec encoding) so the
/// ZEB-461 device bundle a friend ships over the handshake encodes identically
/// to how that same parallel vec is stored on the receiver. Order is meaningful
/// (parallel-indexed to `sender_devices`), so decode never sorts or dedups; it
/// only bounds length at `MAX_DEVICES_PER_OWNER` against a hostile peer.
mod vec_opt_bstr64 {
    use crate::owner_state_types::MAX_DEVICES_PER_OWNER;
    use serde::de::{Error as DeError, SeqAccess, Visitor};
    use serde::ser::SerializeSeq;
    use serde::{Deserialize, Deserializer, Serializer};
    use std::fmt;

    pub fn serialize<S: Serializer>(v: &[Option<[u8; 64]>], s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(v.len()))?;
        // serde's blanket `[T; N]: Serialize` only covers small N, so wrap each
        // element in a newtype that emits bstr(64) / null explicitly.
        struct BstrOpt<'a>(&'a Option<[u8; 64]>);
        impl serde::Serialize for BstrOpt<'_> {
            fn serialize<S2: Serializer>(&self, s: S2) -> Result<S2::Ok, S2::Error> {
                match self.0 {
                    Some(bytes) => s.serialize_bytes(bytes),
                    None => s.serialize_none(),
                }
            }
        }
        for opt in v {
            seq.serialize_element(&BstrOpt(opt))?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Option<[u8; 64]>>, D::Error> {
        /// One element: CBOR null (→ None) or bstr(64) (→ Some).
        struct OptPubVisitor;
        impl<'de> Visitor<'de> for OptPubVisitor {
            type Value = Option<[u8; 64]>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "CBOR null or a 64-byte CBOR byte string")
            }
            fn visit_none<E: DeError>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_unit<E: DeError>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
                d.deserialize_bytes(BytesVisitor).map(Some)
            }
            fn visit_bytes<E: DeError>(self, value: &[u8]) -> Result<Self::Value, E> {
                BytesVisitor.visit_bytes(value).map(Some)
            }
            fn visit_byte_buf<E: DeError>(self, v: Vec<u8>) -> Result<Self::Value, E> {
                BytesVisitor.visit_byte_buf(v).map(Some)
            }
        }

        struct BytesVisitor;
        impl<'de> Visitor<'de> for BytesVisitor {
            type Value = [u8; 64];
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "a 64-byte CBOR byte string")
            }
            fn visit_bytes<E: DeError>(self, value: &[u8]) -> Result<[u8; 64], E> {
                let arr: [u8; 64] = value.try_into().map_err(|_| {
                    E::custom(format!(
                        "device identity pub must be 64 bytes, got {}",
                        value.len()
                    ))
                })?;
                Ok(arr)
            }
            fn visit_byte_buf<E: DeError>(self, v: Vec<u8>) -> Result<[u8; 64], E> {
                self.visit_bytes(&v)
            }
        }

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
                    "an array of at most {MAX_DEVICES_PER_OWNER} optional 64-byte identity pubs"
                )
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                if let Some(n) = seq.size_hint() {
                    if n > MAX_DEVICES_PER_OWNER {
                        return Err(A::Error::custom(format!(
                            "device_identity_pubs array length {n} exceeds MAX_DEVICES_PER_OWNER ({MAX_DEVICES_PER_OWNER})"
                        )));
                    }
                }
                let cap = seq
                    .size_hint()
                    .unwrap_or(MAX_DEVICES_PER_OWNER)
                    .min(MAX_DEVICES_PER_OWNER);
                let mut out: Vec<Option<[u8; 64]>> = Vec::with_capacity(cap);
                while let Some(item) = seq.next_element::<OptPub>()? {
                    if out.len() >= MAX_DEVICES_PER_OWNER {
                        return Err(A::Error::custom(format!(
                            "device_identity_pubs array exceeds MAX_DEVICES_PER_OWNER ({MAX_DEVICES_PER_OWNER})"
                        )));
                    }
                    out.push(item.0);
                }
                Ok(out)
            }
        }
        d.deserialize_seq(CapVisitor)
    }
}

/// Serde for `sender_devices: Vec<DeviceIdentityHash>` that REJECTS a sequence
/// longer than `MAX_DEVICES_PER_OWNER` on decode — defense-in-depth against a
/// hostile peer padding the bundle (mirrors the cap on the parallel
/// `device_identity_pubs` via [`vec_opt_bstr64`]). The whole packet is already
/// bounded by `FRIEND_MAX_PACKET_LEN` and `apply_owner_device_update` truncates
/// at the cache, but capping here keeps the two parallel vecs symmetric and
/// rejects the oversized packet before the digest is computed over it. Serialize
/// is a plain pass-through (byte-identical to the derived `Vec` impl, so the
/// pinned wire fixtures are unaffected).
mod vec_devhash_capped {
    use crate::owner_state_types::{DeviceIdentityHash, MAX_DEVICES_PER_OWNER};
    use serde::de::{Error as DeError, SeqAccess, Visitor};
    use serde::ser::SerializeSeq;
    use serde::{Deserializer, Serializer};
    use std::fmt;

    pub fn serialize<S: Serializer>(v: &[DeviceIdentityHash], s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(v.len()))?;
        for d in v {
            seq.serialize_element(d)?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Vec<DeviceIdentityHash>, D::Error> {
        struct CapVisitor;
        impl<'de> Visitor<'de> for CapVisitor {
            type Value = Vec<DeviceIdentityHash>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(
                    f,
                    "a sequence of at most {MAX_DEVICES_PER_OWNER} device hashes"
                )
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut out: Vec<DeviceIdentityHash> =
                    Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX_DEVICES_PER_OWNER));
                while let Some(elem) = seq.next_element::<DeviceIdentityHash>()? {
                    if out.len() >= MAX_DEVICES_PER_OWNER {
                        return Err(A::Error::custom(format!(
                            "sender_devices exceeds MAX_DEVICES_PER_OWNER ({MAX_DEVICES_PER_OWNER})"
                        )));
                    }
                    out.push(elem);
                }
                Ok(out)
            }
        }
        d.deserialize_seq(CapVisitor)
    }
}

/// Serde for `revocations: Vec<RevocationAttestation>` (ZEB-680) that REJECTS a
/// sequence longer than [`MAX_CARRIED_REVOCATIONS`] on decode — the established
/// hostile-peer convention (same shape as [`vec_devhash_capped`] /
/// `vec_opt_bstr64`: over-cap is a HARD decode error that rejects the frame,
/// never truncation). Serialize is a plain pass-through (byte-identical to the
/// derived `Vec` impl), so the send side never emits more than the sender chose
/// to carry.
mod vec_revocation_capped {
    use super::{RevocationAttestation, MAX_CARRIED_REVOCATIONS};
    use serde::de::{Error as DeError, SeqAccess, Visitor};
    use serde::ser::SerializeSeq;
    use serde::{Deserializer, Serializer};
    use std::fmt;

    pub fn serialize<S: Serializer>(v: &[RevocationAttestation], s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(v.len()))?;
        for a in v {
            seq.serialize_element(a)?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Vec<RevocationAttestation>, D::Error> {
        struct CapVisitor;
        impl<'de> Visitor<'de> for CapVisitor {
            type Value = Vec<RevocationAttestation>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(
                    f,
                    "an array of at most {MAX_CARRIED_REVOCATIONS} revocation attestations"
                )
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                if let Some(n) = seq.size_hint() {
                    if n > MAX_CARRIED_REVOCATIONS {
                        return Err(A::Error::custom(format!(
                            "revocations array length {n} exceeds MAX_CARRIED_REVOCATIONS ({MAX_CARRIED_REVOCATIONS})"
                        )));
                    }
                }
                let cap = seq.size_hint().unwrap_or(0).min(MAX_CARRIED_REVOCATIONS);
                let mut out: Vec<RevocationAttestation> = Vec::with_capacity(cap);
                while let Some(elem) = seq.next_element::<RevocationAttestation>()? {
                    if out.len() >= MAX_CARRIED_REVOCATIONS {
                        return Err(A::Error::custom(format!(
                            "revocations array exceeds MAX_CARRIED_REVOCATIONS ({MAX_CARRIED_REVOCATIONS})"
                        )));
                    }
                    out.push(elem);
                }
                Ok(out)
            }
        }
        d.deserialize_seq(CapVisitor)
    }
}

/// Maximum bytes the acceptor reads per friend-handshake packet. The wire shape
/// is `[u32 LE length-prefix][body]`; any prefix exceeding this is rejected to
/// defend against memory-exhaustion by an adversarial dialer. 256 KiB matches
/// `iroh_invite_acceptor::HANDSHAKE_MAX_PACKET_LEN` and is far larger than any
/// legitimate request (an `EnrollmentCert` + two `[u8;64]` sigs fit in single-
/// digit KB).
pub const FRIEND_MAX_PACKET_LEN: usize = 256 * 1024;

/// ZEB-680: hard cap on the number of [`RevocationAttestation`]s carried on a
/// single friend-link frame. Exceeding it on decode is a HARD error that rejects
/// the frame (the hostile-peer convention shared with `MAX_DEVICES_PER_OWNER`),
/// never truncation. Send side sends at most this many (smallest-N by byte order
/// for determinism, matching the ZEB-692 per-owner store cap).
pub const MAX_CARRIED_REVOCATIONS: usize = 32;

/// ZEB-680: a self-authenticating device-revocation attestation carried on the
/// friend-link frames — a Master-issued [`RevocationCert`] paired with the
/// retired device's [`EnrollmentCert`]. Each pair is independently verifiable
/// (`dm_outbox::verify_revocation_push` binds `revocation.owner_id` to the link
/// peer and `revocation.target` to the enrollment's `device_id`), so it carries
/// no outer signature. `enrollment` is `Box`ed to keep the element small (the
/// cert is large and a frame can carry up to [`MAX_CARRIED_REVOCATIONS`] of
/// them); `Box<T>` serializes byte-identically to `T`.
///
/// Single-char serde keys match this module's canonical-CBOR convention:
/// `revocation` "r", `enrollment` "e". (`dm_envelope::RevocationPushBody` is the
/// same cert pair but module-private with two-char keys, so it is not reusable
/// across the module boundary.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevocationAttestation {
    /// Master-issued revocation cert for one of the sender's own devices.
    #[serde(rename = "r")]
    pub revocation: RevocationCert,
    /// The retired device's enrollment cert (binds `revocation.target` to the
    /// device's identity so the pair verifies without an outer signature).
    #[serde(rename = "e")]
    pub enrollment: Box<EnrollmentCert>,
}

/// ZEB-680 §2 (send side): build the own-fleet revocation attestations to carry
/// on an outbound friend-link frame, from the owner's live trust snapshot.
///
/// Mirrors `owner_commands::push_revocation_to_friends`'s pairing: for each
/// `RevocationCert` in the trust `OwnerState`, pair it with the retired device's
/// `EnrollmentCert` (`enrollments.get(&rc.target)`), skipping — with the same
/// warn as the push path — any revocation with no enrollment on record.
///
/// Only **Master-issued** revocations are carried. The receive-side
/// `dm_outbox::verify_revocation_push` accepts Master-issued certs only, so
/// carrying a `SelfDevice`/`Quorum`-issued revocation would make an honest
/// receiver reject the *entire* handshake (fail-closed) — the worst outcome for
/// a legitimate link. Excluding them here keeps the carry purely additive.
///
/// The result is capped at [`MAX_CARRIED_REVOCATIONS`], keeping the
/// most-recently-issued N (`revocation.issued_at` descending, ZEB-701): recent
/// revocations are the security-relevant subset for a brand-new friend who has
/// no other propagation channel yet, whereas the previous smallest-N-by-target
/// order permanently starved every revocation past the cap out of this
/// channel. Ties break on `revocation.target` byte order so the selection is
/// deterministic and the emitted bytes are stable. (The ZEB-692 persisted
/// store keeps its own smallest-N-by-byte-order cap — the wire carry
/// deliberately diverges from the store's eviction order.)
pub fn build_revocation_attestations(
    trust: &harmony_owner::state::OwnerState,
) -> Vec<RevocationAttestation> {
    use harmony_owner::certs::RevocationIssuer;
    let mut atts: Vec<RevocationAttestation> = trust
        .revocations
        .iter()
        .filter(|rc| matches!(rc.issuer, RevocationIssuer::Master { .. }))
        .filter_map(|rc| match trust.enrollments.get(&rc.target) {
            Some(enrollment) => Some(RevocationAttestation {
                revocation: rc.clone(),
                enrollment: Box::new(enrollment.clone()),
            }),
            None => {
                tracing::warn!(
                    target = %hex::encode(rc.target),
                    "ZEB-680: no enrollment for revoked device; skipping carried attestation"
                );
                None
            }
        })
        .collect();
    // ZEB-701: deterministic most-recent-N selection (`RevocationSet::iter`
    // yields HashMap order): sort by issued_at DESCENDING with the target
    // byte order as tie-break, then truncate to the cap.
    atts.sort_unstable_by(|a, b| {
        b.revocation
            .issued_at
            .cmp(&a.revocation.issued_at)
            .then_with(|| a.revocation.target.cmp(&b.revocation.target))
    });
    atts.truncate(MAX_CARRIED_REVOCATIONS);
    atts
}

/// A friend-link request: "I am owner `from_addr`; here is my proof (cert +
/// device-#2 signature) and the friend-token signature I am redeeming; please
/// add me and reply with your own proof."
///
/// `sig` is the requester's device-#2 Ed25519 signature over
/// [`friend_request_sig_preimage`]`(from_addr, token_sig)`. `token_sig` binds
/// the request to a specific minted friend token (the ZEB-367 `InviteToken.sig`
/// the inviter published Case-A), so an acceptor can `unregister_friend_token`
/// the consumed one-shot.
//
// ZEB-371: this struct (and `FriendLinkAccepted`) gets EXPLICIT single-char
// `#[serde(rename)]` on EVERY field. The struct went from field-name keys to
// single-char keys to keep every map key equal-length (one byte) at this level,
// which is what the codebase's canonical-CBOR convention wants and avoids any
// length-driven key reordering surprises now that an optional field
// (`token_sig`) can be absent. Renames: from_addr "a", display "n",
// token_sig "t", eph_x25519_pub "e", enrollment "c", sig "s".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendLinkRequest {
    /// The requester's master `OwnerAddr` (their `owner_id`). MUST equal
    /// `enrollment.owner_id` (checked by `verify_enrolled_device`).
    #[serde(rename = "a")]
    pub from_addr: OwnerAddr,
    /// The requester's advertised display name (UX hint). `None` when unset.
    ///
    /// Capped at `MAX_FRIEND_DISPLAY_LEN` at the WIRE boundary (oversized →
    /// hard decode error, not truncation) via the same strict deserializer
    /// `FriendEntry.display` uses. Without this cap an authenticated peer could
    /// push an oversized `display` through the handshake into a `FriendEntry`,
    /// which would then fail to deserialize on the owner's other devices during
    /// owner-state sync.
    #[serde(
        rename = "n",
        default,
        deserialize_with = "crate::friend_graph::deserialize_capped_display"
    )]
    pub display: Option<String>,
    /// The friend-token signature being redeemed (the inviter's published
    /// `InviteToken.sig`). Bound into the request preimage; lets the acceptor
    /// unregister the consumed Case-A one-shot. Stored as a CBOR bstr(64).
    ///
    /// `Option` (ZEB-371): absent for the future Path-A (no-token) friend flow.
    /// The `Some(_)` encoding is byte-identical to the old bare bstr(64) so only
    /// the new `eph_x25519_pub` field changes the request hex.
    #[serde(
        rename = "t",
        with = "opt_bstr64",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub token_sig: Option<[u8; 64]>,
    /// ZEB-371: requester's ephemeral X25519 public for the rendezvous-secret
    /// ECDH. Stored as a CBOR bstr(32).
    #[serde(
        rename = "e",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub eph_x25519_pub: [u8; 32],
    /// The requester's Master `EnrollmentCert` (their owner→device-#2 binding).
    #[serde(rename = "c")]
    pub enrollment: EnrollmentCert,
    /// Requester's device-#2 Ed25519 signature over
    /// `friend_request_sig_preimage(from_addr, token_sig, eph_x25519_pub)`.
    /// Stored as a CBOR bstr(64).
    #[serde(
        rename = "s",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],
    /// ZEB-461: the requester's device bundle (their `OwnerDeviceEntry.devices`),
    /// so the acceptor can seed an `OwnerDeviceCache` entry for the new friend's
    /// owner without a separate fetch. Bound into `sig` via `devices_digest`
    /// (an attacker can't swap the requester's claimed device list). Encodes as
    /// `DeviceIdentityHash` bstr(16) elements. `#[serde(default)]` for back-compat
    /// with pre-ZEB-461 peers (absent → empty).
    #[serde(rename = "d", default, with = "vec_devhash_capped")]
    pub sender_devices: Vec<DeviceIdentityHash>,
    /// ZEB-461: full identity pubs parallel-indexed to `sender_devices` (`Some` =
    /// pub known, `None` = known-by-hash only). Also bound into `sig` via
    /// `devices_digest`. Encoded like `OwnerDeviceEntry.device_identity_pubs`.
    #[serde(rename = "p", default, with = "vec_opt_bstr64")]
    pub device_identity_pubs: Vec<Option<[u8; 64]>>,
    /// ZEB-461: the requester's iroh `NodeId` (the cross-WAN DM tunnel dial
    /// target). ZEB-473 §6.3: SIGNED — bound into the `contact_digest` preimage so
    /// an active MITM can't redirect the tunnel. `#[serde(default)]` on the wire
    /// (back-compat); only the digest covers it. Stored as a CBOR bstr(32).
    #[serde(
        rename = "i",
        default,
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub iroh_node_id: [u8; 32],
    /// ZEB-461: the requester's home-relay URL. ZEB-473 §6.3: SIGNED (in
    /// `contact_digest`). `None` when no relay is configured.
    #[serde(rename = "r", default)]
    pub home_relay_url: Option<String>,
    /// ZEB-461: the requester's post-quantum DSA public key for the tunnel.
    /// ZEB-473 §6.3: SIGNED (in `contact_digest`) so a PQ downgrade is detectable.
    /// Encoded as a CBOR bstr.
    #[serde(rename = "q", default, with = "serde_bytes")]
    pub pq_dsa_pubkey: Vec<u8>,
    /// ZEB-461: the requester's post-quantum KEM public key for the tunnel.
    /// ZEB-473 §6.3: SIGNED (in `contact_digest`), same rationale as
    /// `pq_dsa_pubkey`. Encoded as a CBOR bstr.
    #[serde(rename = "k", default, with = "serde_bytes")]
    pub pq_kem_pubkey: Vec<u8>,
    /// ZEB-677: Master-issued signer certs backing a Quorum-issued
    /// `enrollment`. Empty for Master-issued certs (the key is omitted on
    /// the wire; old peers ignore it and keep rejecting quorum certs).
    /// Not signature-bound — each cert is independently self-authenticating.
    #[serde(rename = "b", default, skip_serializing_if = "Vec::is_empty")]
    pub signer_certs: Vec<EnrollmentCert>,
    /// ZEB-680: the sender's own fleet's device revocations (Master-issued
    /// `RevocationCert` + the retired device's `EnrollmentCert`), so a NEW friend
    /// learns of past revocations at link time. Not signature-bound — each pair
    /// is independently self-authenticating (ZEB-677 `signer_certs` precedent).
    /// Absent/empty for pre-ZEB-680 peers (the key is omitted on the wire).
    /// Decode is capped at [`MAX_CARRIED_REVOCATIONS`]; over-cap is a hard decode
    /// error that rejects the frame (never truncation).
    #[serde(
        rename = "v",
        default,
        skip_serializing_if = "Vec::is_empty",
        with = "vec_revocation_capped"
    )]
    pub revocations: Vec<RevocationAttestation>,
}

/// The acceptor's reply: "accepted; here is my own proof so you can add me back
/// (the mutual link)."
///
/// `sig` is the acceptor's device-#2 Ed25519 signature over
/// [`friend_accept_sig_preimage`]`(from_addr, token_sig)`, where `token_sig` is
/// the same token signature from the originating request (binding the accept to
/// the request it answers).
//
// ZEB-371: single-char renames on every field, consistent with
// `FriendLinkRequest`: from_addr "a", display "n", eph_x25519_pub "e",
// enrollment "c", sig "s".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendLinkAccepted {
    /// The accepter's master `OwnerAddr` (their `owner_id`). MUST equal
    /// `enrollment.owner_id`.
    #[serde(rename = "a")]
    pub from_addr: OwnerAddr,
    /// The accepter's advertised display name (UX hint). `None` when unset.
    ///
    /// Capped at `MAX_FRIEND_DISPLAY_LEN` at the WIRE boundary, same as
    /// `FriendLinkRequest.display` — matters for the future Task-10 redeem path
    /// that turns an accept into a local `FriendEntry`.
    #[serde(
        rename = "n",
        default,
        deserialize_with = "crate::friend_graph::deserialize_capped_display"
    )]
    pub display: Option<String>,
    /// ZEB-371: accepter's ephemeral X25519 public for the rendezvous-secret
    /// ECDH. Stored as a CBOR bstr(32).
    #[serde(
        rename = "e",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub eph_x25519_pub: [u8; 32],
    /// The accepter's Master `EnrollmentCert`.
    #[serde(rename = "c")]
    pub enrollment: EnrollmentCert,
    /// Accepter's device-#2 Ed25519 signature over
    /// `friend_accept_sig_preimage(from_addr, token_sig, eph_x25519_pub,
    /// devices_digest)`. Stored as a CBOR bstr(64).
    #[serde(
        rename = "s",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],
    /// ZEB-461: the accepter's device bundle (mirrors `FriendLinkRequest`); lets
    /// the requester seed an `OwnerDeviceCache` entry for the new friend. Bound
    /// into `sig` via `devices_digest`.
    #[serde(rename = "d", default, with = "vec_devhash_capped")]
    pub sender_devices: Vec<DeviceIdentityHash>,
    /// ZEB-461: identity pubs parallel-indexed to `sender_devices`. Bound into
    /// `sig` via `devices_digest`.
    #[serde(rename = "p", default, with = "vec_opt_bstr64")]
    pub device_identity_pubs: Vec<Option<[u8; 64]>>,
    /// ZEB-461: the accepter's iroh `NodeId`. ZEB-473 §6.3: SIGNED (in
    /// `contact_digest`). bstr(32).
    #[serde(
        rename = "i",
        default,
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub iroh_node_id: [u8; 32],
    /// ZEB-461: the accepter's home-relay URL. ZEB-473 §6.3: SIGNED.
    #[serde(rename = "r", default)]
    pub home_relay_url: Option<String>,
    /// ZEB-461: the accepter's post-quantum DSA public key. ZEB-473 §6.3: SIGNED.
    /// bstr.
    #[serde(rename = "q", default, with = "serde_bytes")]
    pub pq_dsa_pubkey: Vec<u8>,
    /// ZEB-461: the accepter's post-quantum KEM public key. ZEB-473 §6.3: SIGNED.
    /// bstr.
    #[serde(rename = "k", default, with = "serde_bytes")]
    pub pq_kem_pubkey: Vec<u8>,
    /// ZEB-677: Master-issued signer certs backing a Quorum-issued
    /// `enrollment`. Empty for Master-issued certs; see
    /// `FriendLinkRequest.signer_certs`.
    #[serde(rename = "b", default, skip_serializing_if = "Vec::is_empty")]
    pub signer_certs: Vec<EnrollmentCert>,
    /// ZEB-680: the accepter's own fleet's device revocations; see
    /// `FriendLinkRequest.revocations`. Attached so the requester (a possibly-new
    /// friend) learns of the accepter's past revocations at link time. Capped at
    /// [`MAX_CARRIED_REVOCATIONS`] on decode; over-cap rejects the frame.
    #[serde(
        rename = "v",
        default,
        skip_serializing_if = "Vec::is_empty",
        with = "vec_revocation_capped"
    )]
    pub revocations: Vec<RevocationAttestation>,
}

/// The acceptor's reply on the `harmony/friend/v1` ALPN. ZEB-371 Task 12:
/// wraps the existing [`FriendLinkAccepted`] so the acceptor can also signal
/// `Pending` for the Path-A (no-token) flow where a NEW owner's request is
/// recorded and awaits the user's accept — in which case NO friend is written
/// and NO `FriendLinkAccepted` proof is produced.
///
/// Length-prefix framing happens at the call site; the codec
/// ([`encode_friend_response`]/[`decode_friend_response`]) is plain ciborium
/// bounded by [`FRIEND_MAX_PACKET_LEN`] with strict trailing-byte rejection,
/// mirroring [`encode_friend_accepted`]/[`decode_friend_accepted`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FriendLinkResponse {
    /// Link complete; both sides derive the secret.
    ///
    /// `FriendLinkAccepted` is large (it embeds an `EnrollmentCert`), so it is
    /// `Box`ed to keep the enum small (clippy `large_enum_variant`). `Box<T>`
    /// serializes byte-identically to `T` via serde — the wire format is the
    /// externally-tagged `{"ok": <accepted>}` map regardless.
    #[serde(rename = "ok")]
    Accepted(Box<FriendLinkAccepted>),
    /// Request recorded; awaiting the user's accept (Path A, new owner).
    #[serde(rename = "pending")]
    Pending,
}

/// ZEB-473: SHA-256 digest of the canonical CBOR encoding of the friend-contact
/// bundle — the device list, its parallel identity pubs, AND the peer's iroh
/// reachability + post-quantum keys. Bound into the friend request/accept
/// signatures so an active MITM cannot have ANY of these swapped in flight
/// (decision §6.3: the reachability + PQ keys are SIGNED, not unsigned routing
/// hints, so a downgrade of the PQ tunnel is detectable).
///
/// Field order is fixed and deterministic:
/// `(devices, pubs, iroh_node_id, home_relay_url, pq_dsa_pubkey, pq_kem_pubkey)`.
/// `device_identity_pubs` is encoded via the same bstr(64)/null shape used on the
/// wire (see [`vec_opt_bstr64`]) so the digest is stable regardless of serde
/// array limits; the byte arrays / vecs are wrapped via `serde_bytes` so they
/// encode as CBOR bstrs (not N-element arrays). The whole tuple is digested in
/// ONE place so every build + verify site is byte-for-byte identical.
///
/// ZEB-461 history: this was `friend_devices_digest` over only `(devices, pubs)`;
/// ZEB-473 extended the preimage to cover the four reachability/PQ fields, which
/// were previously unsigned routing hints.
#[allow(clippy::too_many_arguments)]
pub fn contact_digest(
    devices: &[DeviceIdentityHash],
    pubs: &[Option<[u8; 64]>],
    iroh_node_id: &[u8; 32],
    home_relay_url: Option<&str>,
    pq_dsa_pubkey: &[u8],
    pq_kem_pubkey: &[u8],
) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    // A tiny wrapper so the pubs vec encodes via vec_opt_bstr64 (bstr(64)/null)
    // and the byte fields encode as CBOR bstrs — keeping the digest input
    // identical to the wire bytes. Field order is fixed (see doc).
    #[derive(Serialize)]
    struct Bundle<'a> {
        devices: &'a [DeviceIdentityHash],
        #[serde(with = "vec_opt_bstr64")]
        pubs: &'a [Option<[u8; 64]>],
        iroh_node_id: &'a serde_bytes::Bytes,
        home_relay_url: Option<&'a str>,
        pq_dsa_pubkey: &'a serde_bytes::Bytes,
        pq_kem_pubkey: &'a serde_bytes::Bytes,
    }
    let mut buf = Vec::new();
    ciborium::into_writer(
        &Bundle {
            devices,
            pubs,
            iroh_node_id: serde_bytes::Bytes::new(iroh_node_id),
            home_relay_url,
            pq_dsa_pubkey: serde_bytes::Bytes::new(pq_dsa_pubkey),
            pq_kem_pubkey: serde_bytes::Bytes::new(pq_kem_pubkey),
        },
        &mut buf,
    )
    .expect("friend contact bundle always encodes");
    let mut hasher = Sha256::new();
    hasher.update(&buf);
    hasher.finalize().into()
}

/// ZEB-461: this node's own reachability + identity material to advertise in an
/// outbound friend handshake (request OR accept). `None` at a build site means
/// "ship the empty bundle" (tests / pre-identity); `Some` fills the real values.
///
/// The device bundle is DERIVED from `identity_pub_64` via
/// [`crate::dm_tunnel_contact::self_device_bundle`]. ZEB-473 §6.3: the
/// reachability (`iroh_node_id`, `home_relay_url`) and PQ keys (`pq_dsa_pubkey`,
/// `pq_kem_pubkey`) are now SIGNED alongside the device bundle — all six fields
/// are folded into [`contact_digest`], which the handshake signature binds — so an
/// active MITM cannot silently downgrade the PQ tunnel by rewriting them.
#[derive(Clone)]
pub struct SelfHandshakeReachability {
    pub identity_pub_64: [u8; 64],
    pub iroh_node_id: [u8; 32],
    pub home_relay_url: Option<String>,
    pub pq_dsa_pubkey: Vec<u8>,
    pub pq_kem_pubkey: Vec<u8>,
}

/// ZEB-621 (ZEB-521 completion): the IMMUTABLE self-handshake material the friend
/// acceptor advertises in every signed accept — this node's identity pub, iroh
/// node id, and PQ keys. Captured once at `start_node` and constant for the
/// process lifetime.
///
/// This is [`SelfHandshakeReachability`] MINUS `home_relay_url`, and that omission
/// is the whole point of ZEB-621: the relay is VOLATILE (it flaps whenever the
/// network changes) and iroh's `Endpoint::home_relay()` often hasn't resolved when
/// `start_node` runs. Storing a boot-time relay froze it for the process lifetime,
/// so a node could advertise a stale (or `None`) relay to *every* friend forever,
/// leaving each peer's `DeviceTunnelContact` relay-less (the ZEB-504 capture). So
/// the acceptor no longer stores a relay at all — it reads one FRESH from the live
/// endpoint via [`HomeRelayRefresh`] at each accept-sign time.
///
/// (The DIALER path still uses the full `SelfHandshakeReachability`, which it
/// rebuilds fresh per dial — already ZEB-521-correct, so it keeps the relay field.)
#[derive(Clone)]
pub struct SelfHandshakeStatics {
    pub identity_pub_64: [u8; 64],
    pub iroh_node_id: [u8; 32],
    pub pq_dsa_pubkey: Vec<u8>,
    pub pq_kem_pubkey: Vec<u8>,
}

/// ZEB-521: a cheap, synchronous read of this node's CURRENT iroh home-relay URL
/// (unresolved/empty → `None`; the empty-string filter lives in the closure wired
/// in `lib.rs`). Wired from the iroh endpoint in production; it is the SOLE source
/// of the `home_relay_url` the friend acceptor advertises at accept-sign time (the
/// acceptor holds no frozen snapshot — ZEB-621). `None` in tests / when iroh
/// didn't bind → the accept advertises no relay rather than a stale one.
pub type HomeRelayRefresh = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// Canonical preimage bytes the requester's device-#2 key signs for a
/// [`FriendLinkRequest`]. A small CBOR-encoded tuple `("hfr1", from_addr,
/// token_sig, eph, devices_digest)` — the `"hfr1"` domain tag makes a
/// friend-request signature unmistakable for any other Ed25519 signature this
/// device produces. ZEB-371: the signature binds the requester's ephemeral
/// X25519 public key (so an attacker can't swap the rendezvous key) and the
/// optional token. ZEB-461: it also binds `devices_digest` (the requester's
/// device bundle) so the bundle can't be tampered. ZEB-473 §6.3: `devices_digest`
/// is now the six-field [`contact_digest`], so the requester's iroh reachability +
/// PQ keys are signed too (MITM-evident).
pub fn friend_request_sig_preimage(
    from_addr: OwnerAddr,
    token_sig: Option<&[u8; 64]>,
    eph_x25519_pub: &[u8; 32],
    devices_digest: &[u8; 32],
) -> Vec<u8> {
    sig_preimage("hfr1", from_addr, token_sig, eph_x25519_pub, devices_digest)
}

/// Canonical preimage bytes the accepter's device-#2 key signs for a
/// [`FriendLinkAccepted`]. Domain-separated from the request preimage by the
/// `"hfa1"` tag so a request signature can never be replayed as an accept.
/// ZEB-371: binds the accepter's ephemeral X25519 public key + optional token.
/// ZEB-461: also binds `devices_digest` (the accepter's device bundle).
pub fn friend_accept_sig_preimage(
    from_addr: OwnerAddr,
    token_sig: Option<&[u8; 64]>,
    eph_x25519_pub: &[u8; 32],
    devices_digest: &[u8; 32],
) -> Vec<u8> {
    sig_preimage("hfa1", from_addr, token_sig, eph_x25519_pub, devices_digest)
}

/// Shared preimage builder. The byte arrays are wrapped via `serde_bytes` so
/// they encode as CBOR bstrs (not N-element arrays), keeping the preimage
/// compact and stable. `token_sig` is `Option` (absent for the future Path-A
/// no-token flow); `None` vs `Some` produces a distinct preimage.
/// `devices_digest` (ZEB-461) binds the sender's device bundle.
fn sig_preimage(
    domain: &'static str,
    from_addr: OwnerAddr,
    token_sig: Option<&[u8; 64]>,
    eph_x25519_pub: &[u8; 32],
    devices_digest: &[u8; 32],
) -> Vec<u8> {
    #[derive(Serialize)]
    struct Preimage<'a> {
        domain: &'a str,
        from_addr: OwnerAddr,
        token_sig: Option<&'a serde_bytes::Bytes>,
        eph: &'a serde_bytes::Bytes,
        devices_digest: &'a serde_bytes::Bytes,
    }
    let mut out = Vec::new();
    // Infallible for this fixed-shape value; an encode error would be a logic
    // bug, so surface it loudly rather than silently signing empty bytes.
    ciborium::into_writer(
        &Preimage {
            domain,
            from_addr,
            token_sig: token_sig.map(|t| serde_bytes::Bytes::new(t)),
            eph: serde_bytes::Bytes::new(eph_x25519_pub),
            devices_digest: serde_bytes::Bytes::new(devices_digest),
        },
        &mut out,
    )
    .expect("friend sig preimage always encodes");
    out
}

/// Errors raised while encoding/decoding or authenticating a friend handshake.
#[derive(Debug, thiserror::Error)]
pub enum FriendHandshakeError {
    #[error("CBOR encode failed: {0}")]
    Encode(String),
    #[error("CBOR decode failed: {0}")]
    Decode(String),
    /// Trailing bytes remained after the first CBOR item. `ciborium::from_reader`
    /// stops at the first item and ignores the rest; we reject the remainder so
    /// the friend decoders match the codebase's strict `canonical_cbor_decode`
    /// (no smuggling extra bytes inside an otherwise-valid packet).
    #[error("trailing bytes after CBOR: consumed={consumed} len={len}")]
    TrailingBytes { consumed: usize, len: usize },
    /// The body exceeds [`FRIEND_MAX_PACKET_LEN`]. Bounds work on hostile input.
    #[error("friend packet exceeds size limit: len={len} max={max}")]
    TooLarge { len: usize, max: usize },
    /// Cert verification failed at the ZEB-680 `enrollment_verify`
    /// chokepoint: bad signature or an untrusted issuer (Master and ZEB-677
    /// quorum certs both route through the same chokepoint).
    #[error("enrollment cert invalid (verify failed or untrusted issuer)")]
    EnrollmentCertInvalid,
    /// `cert.owner_id` does not equal the claimed owner address.
    #[error("enrollment owner mismatch: cert binds a different owner_id")]
    EnrollmentOwnerMismatch,
    /// The handshake signature did not verify against the enrolled device key.
    #[error("handshake signature invalid")]
    SignatureInvalid,
    /// `cert.verify()` failed specifically because the cert's `expires_at` is in
    /// the past (ZEB-378). Distinct from `EnrollmentCertInvalid` for telemetry.
    #[error("enrollment cert expired")]
    EnrollmentCertExpired,
    /// Applying the resulting `FriendEntry` to the CRDT was rejected (e.g. a
    /// stale HLC or a key↔master-key invariant failure).
    #[error("friend-graph apply rejected: {0}")]
    ApplyRejected(String),
    /// ZEB-680 §1: the cert verified against the enrollment chokepoint, but the
    /// enrolled device-#2 key is revoked for the claimed owner per the local
    /// `RevokedDeviceProjection` (community tombstones, `RevocationPush`, or the
    /// link-time carry). Consulted AFTER the chokepoint succeeds.
    #[error("enrolled device is revoked for the claimed owner")]
    DeviceRevoked,
    /// ZEB-680 §2 (Task 5): a carried `RevocationAttestation` on a friend-link
    /// frame failed its self-authenticating trust-bind check
    /// (`verify_revocation_push`). Present-but-invalid attestations fail the
    /// handshake closed. Unused until Task 5 wires the handshake carry receive
    /// path; added now so the enum changes once (thiserror does not warn on
    /// unused variants).
    #[error("carried revocation attestation invalid: {0}")]
    RevocationAttestationInvalid(String),
}

/// Encode a [`FriendLinkRequest`] to CBOR bytes (no length prefix). The caller
/// frames it with a `u32 LE` length prefix on the wire.
pub fn encode_friend_request(req: &FriendLinkRequest) -> Result<Vec<u8>, FriendHandshakeError> {
    let mut out = Vec::new();
    ciborium::into_writer(req, &mut out)
        .map_err(|e| FriendHandshakeError::Encode(e.to_string()))?;
    Ok(out)
}

/// Decode a [`FriendLinkRequest`] from CBOR bytes, bounding the input at
/// [`FRIEND_MAX_PACKET_LEN`] before decoding.
pub fn decode_friend_request(bytes: &[u8]) -> Result<FriendLinkRequest, FriendHandshakeError> {
    if bytes.len() > FRIEND_MAX_PACKET_LEN {
        return Err(FriendHandshakeError::TooLarge {
            len: bytes.len(),
            max: FRIEND_MAX_PACKET_LEN,
        });
    }
    decode_strict(bytes)
}

/// Encode a [`FriendLinkAccepted`] to CBOR bytes (no length prefix).
pub fn encode_friend_accepted(acc: &FriendLinkAccepted) -> Result<Vec<u8>, FriendHandshakeError> {
    let mut out = Vec::new();
    ciborium::into_writer(acc, &mut out)
        .map_err(|e| FriendHandshakeError::Encode(e.to_string()))?;
    Ok(out)
}

/// Decode a [`FriendLinkAccepted`] from CBOR bytes, bounding the input at
/// [`FRIEND_MAX_PACKET_LEN`] before decoding.
pub fn decode_friend_accepted(bytes: &[u8]) -> Result<FriendLinkAccepted, FriendHandshakeError> {
    if bytes.len() > FRIEND_MAX_PACKET_LEN {
        return Err(FriendHandshakeError::TooLarge {
            len: bytes.len(),
            max: FRIEND_MAX_PACKET_LEN,
        });
    }
    decode_strict(bytes)
}

/// Encode a [`FriendLinkResponse`] to CBOR bytes (no length prefix). The caller
/// frames it with a `u32 LE` length prefix on the wire.
pub fn encode_friend_response(resp: &FriendLinkResponse) -> Result<Vec<u8>, FriendHandshakeError> {
    let mut out = Vec::new();
    ciborium::into_writer(resp, &mut out)
        .map_err(|e| FriendHandshakeError::Encode(e.to_string()))?;
    Ok(out)
}

/// Decode a [`FriendLinkResponse`] from CBOR bytes, bounding the input at
/// [`FRIEND_MAX_PACKET_LEN`] before decoding (strict trailing-byte rejection).
pub fn decode_friend_response(bytes: &[u8]) -> Result<FriendLinkResponse, FriendHandshakeError> {
    if bytes.len() > FRIEND_MAX_PACKET_LEN {
        return Err(FriendHandshakeError::TooLarge {
            len: bytes.len(),
            max: FRIEND_MAX_PACKET_LEN,
        });
    }
    decode_strict(bytes)
}

/// Decode a single CBOR item from `bytes` and reject any trailing bytes.
/// `ciborium::from_reader` reads the first item and silently ignores the rest;
/// decoding via a cursor lets us assert the whole buffer was consumed, matching
/// the codebase's strict `canonical_cbor_decode` (no extra bytes smuggled inside
/// an otherwise-valid friend packet).
fn decode_strict<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, FriendHandshakeError> {
    let mut cursor = std::io::Cursor::new(bytes);
    let val = ciborium::from_reader(&mut cursor)
        .map_err(|e| FriendHandshakeError::Decode(e.to_string()))?;
    let consumed = cursor.position() as usize;
    if consumed != bytes.len() {
        return Err(FriendHandshakeError::TrailingBytes {
            consumed,
            len: bytes.len(),
        });
    }
    Ok(val)
}

/// Point-to-point enrolled-device authentication: the friend-handshake face
/// of the `enrollment_verify` chokepoint (ZEB-677 §2), applied without the
/// `SignedMembershipEvent` wrapper.
///
/// Verifies `cert` (Master self-contained; Quorum against the presented
/// `signer_certs` bundle, depth-1), binds `cert.owner_id == claimed_owner.0`,
/// and returns both the enrolled device-#2 Ed25519 verify key (the handshake
/// signature check) and the owner's master anchor key (the
/// `FriendEntry.master_ed25519` friend-graph anchor — recovered from the
/// signer certs when the cert itself is Quorum-issued).
pub fn verify_enrolled_device(
    cert: &EnrollmentCert,
    signer_certs: &[EnrollmentCert],
    claimed_owner: OwnerAddr,
    // ZEB-680 §1: consulted AFTER the pure chokepoint succeeds, against the
    // VERIFIED device-#2 key + `claimed_owner`. An empty projection revokes
    // nothing (back-compat / test default); a real handle enforces every
    // revocation this node has learned (community tombstones, RevocationPush,
    // link-time carry). Sync (std RwLock) — no async added to the verifier.
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
    now_secs: u64,
) -> Result<crate::enrollment_verify::VerifiedEnrollment, FriendHandshakeError> {
    let verified = crate::enrollment_verify::verify_enrollment_any_issuer(
        cert,
        signer_certs,
        Some(&claimed_owner.0),
        now_secs,
    )
    .map_err(|e| match e {
        crate::enrollment_verify::EnrollmentVerifyError::Expired => {
            FriendHandshakeError::EnrollmentCertExpired
        }
        crate::enrollment_verify::EnrollmentVerifyError::OwnerMismatch => {
            FriendHandshakeError::EnrollmentOwnerMismatch
        }
        crate::enrollment_verify::EnrollmentVerifyError::Invalid(_) => {
            FriendHandshakeError::EnrollmentCertInvalid
        }
    })?;
    // ZEB-680 §1: the chokepoint proves the cert; this proves the device is
    // still trusted. The quorum-cert path (ZEB-677) flows through the same
    // chokepoint, so the recovered `device_ed25519` inherits enforcement
    // regardless of issuer.
    if revoked.is_revoked(&claimed_owner, &verified.device_ed25519) {
        return Err(FriendHandshakeError::DeviceRevoked);
    }
    Ok(verified)
}

/// ZEB-680 §2 (Task 5, receive phase 1): fail-closed verification of the
/// revocation attestations a peer carried on a friend-link frame. For each pair,
/// [`crate::dm_outbox::verify_revocation_push`] enforces the trust-bind — both
/// the revocation's and the paired enrollment's `owner_id` must equal
/// `peer_owner` (a peer may only attest ITS OWN devices, never relay a third
/// party's), the revocation is Master-issued, and the enrollment's `device_id`
/// binds `revocation.target`. A single present-but-invalid attestation REJECTS
/// the whole handshake (spec §2 fail-closed, mapped to
/// [`FriendHandshakeError::RevocationAttestationInvalid`]); an empty/absent slice
/// is the back-compat no-op (`Ok(())`).
///
/// Pure — no writes. Applying the verified pairs to the owner-state store + live
/// projection is receive phase 2 (Task 6), gated on established friendship.
/// (`att.enrollment` is `Box<EnrollmentCert>`; `&att.enrollment` deref-coerces to
/// the `&EnrollmentCert` the verifier expects.)
pub fn verify_carried_revocations(
    peer_owner: OwnerAddr,
    attestations: &[RevocationAttestation],
) -> Result<(), FriendHandshakeError> {
    for att in attestations {
        crate::dm_outbox::verify_revocation_push(peer_owner, &att.revocation, &att.enrollment)
            .map_err(|e| FriendHandshakeError::RevocationAttestationInvalid(e.to_string()))?;
    }
    Ok(())
}

/// ZEB-680 §2 (Task 6): receive PHASE 2 — apply a peer's carried own-fleet
/// revocations to the DM revoked-device store + live projection, at
/// friendship-**establishment** time (never before consent/auth). Shared by the
/// acceptor ([`process_friend_request`]) and both dialer drivers
/// (`link_over_connection`, `connectivity_link_friend_iroh_inner`) so all three
/// apply identically.
///
/// Each pair is (re-)verified + trust-bound to `peer_owner` INSIDE
/// [`crate::dm_outbox::handle_revocation_push`] (a peer may only revoke ITS OWN
/// devices), so a mis-bound pair is skipped, never applied — the apply is safe
/// even if some future caller reached it without the fail-closed phase-1
/// [`verify_carried_revocations`] gate. Callers MUST still run that phase-1 gate
/// BEFORE writing the friendship so a present-but-invalid attestation rejects the
/// whole handshake with nothing written (spec §2); by the time we get here every
/// pair is known-valid, so no per-pair error fires and the apply is all-or-none.
///
/// Returns whether ANY pair was a genuine NEW insert. The revoked-device store
/// lives in the SAME `OwnerState` CRDT as the friend graph, so the friendship
/// write on this SAME establishment path already arms the owner-state
/// `notify_dirty` that persists + replicates these (ZEB-248/#473) — the flag is
/// returned only so the dispatch can log the learn-at-link event and the
/// direct-call unit tests can assert idempotency; it is NOT a separate
/// notification trigger.
pub fn apply_carried_revocations(
    state: &mut OwnerState,
    peer_owner: OwnerAddr,
    attestations: &[RevocationAttestation],
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
) -> bool {
    let mut inserted_any = false;
    for att in attestations {
        match crate::dm_outbox::handle_revocation_push(
            state,
            peer_owner,
            &att.revocation,
            &att.enrollment,
            revoked,
        ) {
            Ok(true) => inserted_any = true,
            Ok(false) => {}
            // Unreachable after the phase-1 verify_carried_revocations gate: a
            // pair that verified there cannot fail handle_revocation_push's
            // identical trust-bind here. Warn (never apply, never unwind an
            // already-established friendship) rather than silently swallow.
            Err(e) => tracing::warn!(
                error = ?e,
                owner = %hex::encode(peer_owner.0),
                "ZEB-680: carried revocation rejected at establishment-apply (phase-1 verify should have caught it)"
            ),
        }
    }
    inserted_any
}

// =====================================================================
// ZEB-371 Task 12 — Path A consent decision tree (spec §7.1)
// =====================================================================

/// Outcome of the inbound consent decision (spec §7.1). Pure; no I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentDecision {
    /// Token path: caller runs the existing one-shot token gate.
    TokenPath,
    /// Accept now: write an Active/MutualKey friend + reply Accepted.
    AcceptInline,
    /// ZEB-376: an introduction the user initiated — accept inline AND stamp
    /// `established_via: Introduction` (distinct from `AcceptInline`'s MutualKey).
    AcceptInlineIntroduced,
    /// No token, unknown owner, no prior approval: record + reply Pending.
    Pending,
}

/// Decide how to consent to an inbound friend request. `known` = requester is
/// already an Active|Pending friend (or, later, a community co-member).
/// `auto_accept_known` = the per-user toggle. `prior_accept` = the user already
/// tapped Accept for this requester in a previous dial.
///
/// Authentication (cert + sig verify) ALWAYS runs BEFORE this decision —
/// `known`/`auto_accept_known` only gate whether to PROMPT, never whether to
/// authenticate. An unknown owner with `auto_accept_known` ON is still NOT
/// auto-accepted: only KNOWN owners auto-accept; an unknown owner falls through
/// to `Pending` (record + prompt the user).
pub fn decide_consent(
    token_sig: Option<&[u8; 64]>,
    known: bool,
    auto_accept_known: bool,
    prior_accept: bool,
) -> ConsentDecision {
    if token_sig.is_some() {
        return ConsentDecision::TokenPath;
    }
    if prior_accept || (known && auto_accept_known) {
        return ConsentDecision::AcceptInline;
    }
    ConsentDecision::Pending
}

/// Resolve consent for an inbound request, consuming a one-shot user approval
/// ATOMICALLY. [`decide_consent`] is the pure static policy (token / known +
/// auto-accept); the prior-approval branch is layered here because it MUST be a
/// single mutex op. Among concurrent handshakes from the same approved
/// requester, EXACTLY ONE consumes the approval (`take_approved`) and gets
/// [`ConsentDecision::AcceptInline`]; the rest get [`ConsentDecision::Pending`]
/// and retry later. This closes the `is_approved` → `process_friend_request` →
/// `clear` TOCTOU that otherwise let concurrent dials each derive a *different*
/// friendship secret, leaving the two sides with mismatched Case-D rendezvous
/// keys. `prior_accept` is passed `false` to `decide_consent` precisely so the
/// approval is resolved HERE, atomically — never via a stale read.
///
/// Consuming the approval BEFORE the accept is safe: authentication already ran
/// unconditionally upstream, and a (rare, post-auth) accept failure only costs
/// the user a re-tap — the requester re-dials and lands back at `Pending`.
fn resolve_consent_consuming_approval(
    pending: Option<&crate::friend_requests::PendingFriendRequests>,
    pending_outbound: Option<&crate::friend_requests::PendingOutboundIntroductions>,
    token_sig: Option<&[u8; 64]>,
    known: bool,
    auto_accept_known: bool,
    from: &OwnerAddr,
    now_ms: u64,
) -> ConsentDecision {
    let decision = decide_consent(token_sig, known, auto_accept_known, false);
    if matches!(decision, ConsentDecision::Pending) {
        // ZEB-376: an introduction the user PRE-AUTHORIZED (via an outbound
        // IntroduceRequest) auto-accepts + stamps Introduction. Checked BEFORE
        // the regular one-shot approval so an introduction lands as such.
        if pending_outbound
            .map(|p| p.take(from, now_ms))
            .unwrap_or(false)
        {
            return ConsentDecision::AcceptInlineIntroduced;
        }
        if pending.map(|p| p.take_approved(from)).unwrap_or(false) {
            return ConsentDecision::AcceptInline;
        }
    }
    decision
}

// =====================================================================
// Task 8 — ALPN acceptor
// =====================================================================

use crate::friend_graph::{FriendEntry, FriendOrigin, FriendStatus};
use crate::owner_state_crdt::{ApplyOutcome, OwnerState};
use crate::owner_state_types::Hlc;
use async_trait::async_trait;
use ed25519_dalek::{Signature, Signer, VerifyingKey};
use iroh::endpoint::Connection;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex as TokioMutex;

/// Tiny emit trait so the acceptor can signal the UI a friend was added
/// without depending on `tauri` directly (mirrors
/// `community_invite::AppHandleEmit`). Production impl on `tauri::AppHandle`
/// lives in `lib.rs`; the unit-type impl lets tests pass `None::<Arc<()>>`.
pub trait FriendEventEmit: Send + Sync + 'static {
    /// Emit a `friend-list-changed` Tauri event (no payload — the frontend
    /// re-fetches the friend list on receipt).
    fn emit_friend_list_changed(&self);
    /// ZEB-371 Task 12: emit a `friend-request-received` Tauri event (no
    /// payload — the frontend re-fetches the pending-request list on receipt)
    /// when a Path-A request from a new owner is RECORDED (not yet accepted).
    fn emit_friend_request_received(&self);
}

impl FriendEventEmit for () {
    fn emit_friend_list_changed(&self) {}
    fn emit_friend_request_received(&self) {}
}

/// Default per-await IO deadline for the inbound friend handshake. Mirrors
/// `iroh_invite_acceptor::DEFAULT_ACCEPTOR_IO_DEADLINE_MS`.
pub const DEFAULT_FRIEND_IO_DEADLINE_MS: u64 = 30_000;

/// Wall-clock now in epoch-milliseconds — the same one-syscall pattern used
/// throughout `lib.rs` (`generate_invite`/HLC reservation) and `next_hlc`.
/// Saturates to `0` if the clock is before the epoch.
pub(crate) fn wall_now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Wall-clock now in epoch-SECONDS. `EnrollmentCert` timestamps (`issued_at` /
/// `expires_at`) are Unix seconds, so cert-expiry checks must pass seconds — NOT
/// the millisecond [`wall_now_ms`]. (ZEB-378)
pub(crate) fn wall_now_secs() -> u64 {
    wall_now_ms() / 1000
}

/// Tunable timeouts for the friend handshake handler. Tests construct this
/// directly with sub-second values; production uses [`Self::default`] (or an
/// env override at the call site).
#[derive(Debug, Clone, Copy)]
pub struct FriendAcceptorConfig {
    /// Per-await IO timeout bounding `accept_bi`, both `read_exact`s, both
    /// `write_all`s, and `conn.closed()`.
    pub io_deadline: Duration,
}

impl Default for FriendAcceptorConfig {
    fn default() -> Self {
        Self {
            io_deadline: Duration::from_millis(DEFAULT_FRIEND_IO_DEADLINE_MS),
        }
    }
}

/// PURE authentication of an inbound [`FriendLinkRequest`] (no CRDT write, no
/// I/O). ZEB-371 Task 12: split out of [`process_friend_request`] so the consent
/// decision can authenticate a request it may decide NOT to accept (the Path-A
/// `Pending` branch records a request but writes no friend — yet must still
/// reject a request that fails auth). Verifies (1) the requester's cert →
/// enrolled device-#2 key and (2) the handshake signature over the request
/// preimage against that key. `process_friend_request` re-runs the same two
/// checks (cheap; keeps it self-contained), so this is belt-and-suspenders on
/// the accept paths and the sole gate on the Pending path.
pub fn authenticate_friend_request(
    req: &FriendLinkRequest,
    // ZEB-680 §1: threaded to the inner `verify_enrolled_device` so a request
    // from a revoked device is rejected at auth time.
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
    now_secs: u64,
) -> Result<(), FriendHandshakeError> {
    let verified = verify_enrolled_device(
        &req.enrollment,
        &req.signer_certs,
        req.from_addr,
        revoked,
        now_secs,
    )?;
    let vk = VerifyingKey::from_bytes(&verified.device_ed25519)
        .map_err(|_| FriendHandshakeError::SignatureInvalid)?;
    let devices_digest = contact_digest(
        &req.sender_devices,
        &req.device_identity_pubs,
        &req.iroh_node_id,
        req.home_relay_url.as_deref(),
        &req.pq_dsa_pubkey,
        &req.pq_kem_pubkey,
    );
    let preimage = friend_request_sig_preimage(
        req.from_addr,
        req.token_sig.as_ref(),
        &req.eph_x25519_pub,
        &devices_digest,
    );
    vk.verify_strict(&preimage, &Signature::from_bytes(&req.sig))
        .map_err(|_| FriendHandshakeError::SignatureInvalid)?;
    // ZEB-680 §2 (Task 5): fail-closed phase-1 verify of the requester's carried
    // own-fleet revocation attestations. A present-but-invalid attestation
    // rejects the handshake; an empty/absent list is the back-compat no-op.
    verify_carried_revocations(req.from_addr, &req.revocations)?;
    Ok(())
}

/// PURE, testable core of the friend handshake. Authenticates `req`, writes the
/// resulting `FriendEntry` into `state`, and returns a signed
/// `FriendLinkAccepted` for the requester plus a `bool` reporting whether the
/// requester's carried own-fleet revocations (ZEB-680 §2 phase 2) produced a
/// genuine NEW insert into the DM revoked-device store. No I/O.
///
/// Steps (spec §5.2 accept side):
/// 1. `verify_enrolled_device(&req.enrollment, &req.signer_certs,
///    req.from_addr)` → device key + master anchor,
/// 2. verify `req.sig` over the request preimage against that device key,
/// 3. take the requester's `master_ed25519` anchor from step 1,
/// 4. build `FriendEntry { master_ed25519, display, Active, Token, referrable:
///    false, learned_at }`,
/// 5. `state.apply_friend_update(req.from_addr, entry)` — must be
///    `Inserted`/`Merged` (a `Rejected` is a hard error),
/// 6. build + device-#2-sign a `FriendLinkAccepted` from `self_owner` /
///    `self_enrollment`, signing `friend_accept_sig_preimage(self_owner,
///    req.token_sig)`.
#[allow(clippy::too_many_arguments)]
pub fn process_friend_request(
    state: &mut OwnerState,
    learned_at: Hlc,
    req: &FriendLinkRequest,
    self_owner: OwnerAddr,
    self_display: Option<String>,
    self_enrollment: &EnrollmentCert,
    self_device2: &ed25519_dalek::SigningKey,
    keytree: &crate::owner_state_crypto::KeyTree,
    // ZEB-680 §1: threaded to the step-1 `verify_enrolled_device` re-check so a
    // revoked requester is rejected before any CRDT write.
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
    now_secs: u64,
    self_statics: Option<&SelfHandshakeStatics>,
    // ZEB-621: this node's CURRENT home relay, read fresh from the live endpoint by
    // the dispatch site at accept-sign time (cheap `Option<String>`). This is the
    // SOLE relay source — the acceptor holds no frozen snapshot to fall back to
    // (ZEB-521 completion). `None` when iroh hasn't resolved a relay yet (or no
    // refresh is wired): the signed accept advertises no relay rather than a stale
    // one. Ignored when `self_statics` is `None` (an empty bundle has no relay).
    home_relay_url: Option<String>,
    // ZEB-376: when `Some`, overrides the derived `established_via` — the
    // introduction path passes `Some(FriendOrigin::Introduction)` so an accepted
    // introduction is stamped as such rather than the no-token `MutualKey`
    // default. `None` preserves the token/mutual-key derivation below.
    origin_override: Option<crate::friend_graph::FriendOrigin>,
    // ZEB-680 §2: this node's own-fleet revocation attestations to carry on the
    // signed accept, built FRESH from the live trust snapshot by the dispatch
    // caller (`build_revocation_attestations`). Placed verbatim into the Accepted
    // literal; `Vec::new()` (tests / pre-identity) carries nothing. Not folded
    // into the accept signature — each pair is independently self-authenticating
    // (ZEB-677 `signer_certs` precedent).
    self_revocations: Vec<RevocationAttestation>,
) -> Result<(FriendLinkAccepted, bool), FriendHandshakeError> {
    // 1. Authenticate the requester's cert → enrolled device-#2 key (and the
    // master anchor, recovered from the signer bundle for quorum certs).
    let verified = verify_enrolled_device(
        &req.enrollment,
        &req.signer_certs,
        req.from_addr,
        revoked,
        now_secs,
    )?;

    // 2. Verify the request signature over the canonical preimage (binds the
    // requester's ephemeral X25519 key + optional token + ZEB-461 device-bundle
    // digest computed from the bundle the request carries).
    let vk = VerifyingKey::from_bytes(&verified.device_ed25519)
        .map_err(|_| FriendHandshakeError::SignatureInvalid)?;
    let req_devices_digest = contact_digest(
        &req.sender_devices,
        &req.device_identity_pubs,
        &req.iroh_node_id,
        req.home_relay_url.as_deref(),
        &req.pq_dsa_pubkey,
        &req.pq_kem_pubkey,
    );
    let preimage = friend_request_sig_preimage(
        req.from_addr,
        req.token_sig.as_ref(),
        &req.eph_x25519_pub,
        &req_devices_digest,
    );
    vk.verify_strict(&preimage, &Signature::from_bytes(&req.sig))
        .map_err(|_| FriendHandshakeError::SignatureInvalid)?;

    // ZEB-680 §2 (Task 6 / T5-review BINDING REQ 1): process_friend_request does
    // NOT call authenticate_friend_request — it inlines cert+sig verify only — so
    // it CANNOT lean on serve()'s phase-1 precheck across the function boundary.
    // Re-run the fail-closed carried-revocation verify HERE, before ANY write, so
    // a present-but-invalid attestation rejects the whole handshake with nothing
    // written (friend or revocation) even when a caller drives this fn directly.
    // Cheap bounded ed25519; empty/absent list is the back-compat no-op. The
    // phase-2 apply below re-verifies per-pair inside handle_revocation_push.
    verify_carried_revocations(req.from_addr, &req.revocations)?;

    // ZEB-371: generate this side's ephemeral X25519 keypair for the rendezvous
    // secret.
    let (self_eph_sk, self_eph_pub) = crate::friend_rendezvous::generate_ephemeral();

    // 3. The requester's master key (their friend-graph anchor) came from the
    // chokepoint verification in step 1.
    let master_ed25519 = verified.master_ed25519;

    // ZEB-371 Task 7: derive the shared friendship secret via ECDH (this side's
    // ephemeral secret + the requester's ephemeral public), then KeyTree-seal it
    // under this node's owner keys, AAD-bound to the requester's owner_id. The
    // requester derives the SAME 32-byte plaintext from their secret + this
    // side's `self_eph_pub` (returned in the accept). Derive BEFORE building the
    // entry so the sealed blob can be stored on it.
    let secret = crate::friend_rendezvous::derive_friendship_secret(
        self_eph_sk,
        &req.eph_x25519_pub,
        self_owner,
        req.from_addr,
    );
    let sealed =
        crate::owner_state_crypto::encrypt_friend_secret(keytree, &req.from_addr.0, &secret)
            .map_err(|_| FriendHandshakeError::ApplyRejected("friend-secret seal failed".into()))?;

    // ORDERING (ZEB-680 review — CodeRabbit + Greptile): every FALLIBLE
    // owner-state update runs BEFORE the friendship + revocation commits, and the
    // friendship write and the revocation apply are ADJACENT (nothing fallible
    // between them). This makes the pair effectively atomic: a device-cache
    // rejection aborts here, having committed NOTHING that would leave an active
    // friend WITHOUT its carried revocations (Greptile) — and because the commits
    // are followed only by infallible work + a pre-write notify (below), a
    // committed pair is always flushed, never applied-but-unpersisted (CodeRabbit).
    //
    // ZEB-580 S1: cache the requester's cert-attested #2 DM identity (the
    // combined pub keyed by the #2 DM hash), derived from the EnrollmentCert
    // already verified in step 1 above (`verify_enrolled_device`). This is what
    // the DM signature-verification path consumes, so the CERT is authoritative —
    // NOT the self-asserted wire #3 bundle (`req.sender_devices` /
    // `req.device_identity_pubs`), which is now discarded for DM signing. Degrade
    // to the legacy #3 bundle ONLY when the cert carries no usable X25519
    // (synthetic / pre-ZEB-372 zeroed cert), then to empty if that bundle is
    // empty too. `learned_at` is this node's local HLC (the value also stamped on
    // the friend entry below) — never the peer's claimed time (anti-forgery rule).
    // An empty result skips the write so a peer that advertised nothing never
    // LWW-clobbers a previously-known-good cache entry. A device-cache Rejected is
    // a security-invariant signal (a peer advertising two different pubs for one
    // device hash, a mismatched hash, or invalid key sizes), so it still aborts
    // the whole handshake — but now BEFORE any friendship/revocation commit.
    //
    // ZEB-473 Task 5: the requester's iroh reachability + PQ keys ride along as a
    // SINGLE tunnel contact parallel to the (sole) cached device, so the DM
    // tunnel can later dial them; `apply_owner_device_update` re-aligns it to the
    // sorted device list. `None` (don't fabricate a contact) when the peer
    // advertised no reachability — same skip-on-empty rationale as the bundle.
    let device_tunnel_contacts = vec![crate::dm_tunnel_contact::peer_handshake_contact(
        req.iroh_node_id,
        req.home_relay_url.clone(),
        req.pq_dsa_pubkey.clone(),
        req.pq_kem_pubkey.clone(),
    )];
    let (devices, pubs): (Vec<DeviceIdentityHash>, Vec<Option<[u8; 64]>>) =
        match crate::dm_signing::device2_signing_hash(&req.enrollment) {
            Some(h2) => (
                vec![h2],
                vec![Some(crate::dm_signing::device2_combined_pub(
                    &req.enrollment,
                ))],
            ),
            None if !req.sender_devices.is_empty() => {
                (req.sender_devices.clone(), req.device_identity_pubs.clone())
            }
            None => (Vec::new(), Vec::new()),
        };
    if !devices.is_empty() {
        if let crate::owner_state_crdt::ApplyOutcome::Rejected(reason) = state
            .apply_owner_device_update(
                req.from_addr,
                devices,
                pubs,
                device_tunnel_contacts,
                learned_at.clone(),
            )
        {
            return Err(FriendHandshakeError::ApplyRejected(format!(
                "device cache: {reason:?}"
            )));
        }
    }

    // 4-5. Apply the new friend entry to the CRDT. apply_friend_update re-checks
    // the key↔master-key invariant; a Rejected is a hard error here.
    // established_via is Token when the requester supplied a token_sig (the
    // normal token-invite path); MutualKey for the future Path-A (no-token)
    // reuse path where no token is present.
    let entry = FriendEntry {
        master_ed25519,
        display: req.display.clone(),
        status: FriendStatus::Active,
        established_via: origin_override.unwrap_or_else(|| {
            if req.token_sig.is_some() {
                FriendOrigin::Token
            } else {
                FriendOrigin::MutualKey
            }
        }),
        referrable: false,
        learned_at,
        sealed_secret: Some(sealed),
    };
    match state.apply_friend_update(req.from_addr, entry) {
        ApplyOutcome::Inserted | ApplyOutcome::Merged { .. } => {}
        ApplyOutcome::Rejected(reason) => {
            return Err(FriendHandshakeError::ApplyRejected(format!("{reason:?}")));
        }
    }

    // ZEB-680 §2 (Task 6): PHASE 2 — the friendship is established, so apply the
    // requester's carried own-fleet revocations to the DM revoked-device store +
    // live projection. This is ADJACENT to the friendship write above with no
    // fallible step between (all fallible updates ran earlier), so an active
    // friend can never be left without its carried revocations. The phase-1 verify
    // already proved every pair valid + owner-bound (`req.from_addr`), so no
    // partial apply is possible; `apply_carried_revocations` is infallible.
    // `revocations_inserted` rides back to the dispatch, which arms the owner-state
    // publish (same CRDT) BEFORE the accept-write.
    let revocations_inserted =
        apply_carried_revocations(state, req.from_addr, &req.revocations, revoked);

    // 6. Build + sign the mutual accept reply. The accept sig binds to the same
    // token_sig as the request it answers (domain-separated from the request),
    // this side's ephemeral X25519 public, and (ZEB-461/473) this side's
    // contact digest. ZEB-461 Task 6: when `self_statics` is `Some`, fill the
    // REAL self device bundle + reachability + PQ keys; when `None` (tests /
    // pre-identity) ship the EMPTY bundle exactly as before. ZEB-473 §6.3: the
    // reachability + PQ keys are now SIGNED (folded into the digest), so they must
    // be computed BEFORE the digest. Either way the digest is computed from the
    // SAME six fields placed on the wire, so the signature stays consistent with
    // what a peer re-digests on receipt.
    let (accept_devices, accept_device_pubs): (Vec<DeviceIdentityHash>, Vec<Option<[u8; 64]>>) =
        match self_statics {
            Some(s) => crate::dm_tunnel_contact::self_device_bundle(s.identity_pub_64),
            None => (vec![], vec![]),
        };
    // Reachability + PQ keys (signed via the digest below): real values when
    // `Some`, empty/zero/None when not. ZEB-621: `iroh_node_id` + PQ keys come from
    // the immutable statics; `home_relay_url` is the FRESH endpoint read passed in
    // (the acceptor stores no relay snapshot). No bundle → no relay regardless of
    // the fresh read.
    let (iroh_node_id, home_relay_url, pq_dsa_pubkey, pq_kem_pubkey) = match self_statics {
        Some(s) => (
            s.iroh_node_id,
            home_relay_url,
            s.pq_dsa_pubkey.clone(),
            s.pq_kem_pubkey.clone(),
        ),
        None => ([0u8; 32], None, vec![], vec![]),
    };
    let accept_devices_digest = contact_digest(
        &accept_devices,
        &accept_device_pubs,
        &iroh_node_id,
        home_relay_url.as_deref(),
        &pq_dsa_pubkey,
        &pq_kem_pubkey,
    );
    let accept_preimage = friend_accept_sig_preimage(
        self_owner,
        req.token_sig.as_ref(),
        &self_eph_pub,
        &accept_devices_digest,
    );
    let sig = self_device2.sign(&accept_preimage).to_bytes();
    Ok((
        FriendLinkAccepted {
            from_addr: self_owner,
            display: self_display,
            eph_x25519_pub: self_eph_pub,
            enrollment: self_enrollment.clone(),
            // ZEB-677: bundle threading for a quorum-certed self lands with the
            // ceremony slices (S4); every self-cert today is Master-issued.
            signer_certs: Vec::new(),
            sig,
            sender_devices: accept_devices,
            device_identity_pubs: accept_device_pubs,
            iroh_node_id,
            home_relay_url,
            pq_dsa_pubkey,
            pq_kem_pubkey,
            revocations: self_revocations,
        },
        revocations_inserted,
    ))
}

/// Inbound dispatcher for the `harmony/friend/v1` ALPN. Holds the handles the
/// pure core needs plus the IO plumbing. Generic over the `FriendEventEmit`
/// impl so tests can stub with `()`.
///
/// Structural template: `iroh_invite_acceptor::IrohInviteHandshakeAcceptor`.
pub struct IrohFriendHandshakeAcceptor<H>
where
    H: FriendEventEmit,
{
    crdt_state: Arc<TokioMutex<OwnerState>>,
    /// Shared HLC tracker (`device_id → last Hlc`), bumped per accepted request
    /// to stamp `FriendEntry.learned_at`. Same map the profile broadcaster uses.
    hlc_tracker: Arc<TokioMutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>>,
    device_id: String,
    self_owner: OwnerAddr,
    self_display: Option<String>,
    self_enrollment: EnrollmentCert,
    device2_signing_key: Arc<ed25519_dalek::SigningKey>,
    /// ZEB-371: this node's owner KeyTree (derived from the master seed). Used to
    /// KeyTree-seal the per-friend rendezvous secret derived in
    /// `process_friend_request` before it is written into the `FriendEntry`.
    keytree: Arc<crate::owner_state_crypto::KeyTree>,
    /// `Some(app)` emits `friend-list-changed`; `None` warn-logs only (tests).
    app: Option<Arc<H>>,
    /// `Some` unregisters the consumed Case-A friend-token one-shot on success.
    pkarr_invite_publisher: Option<Arc<crate::pkarr_invite_publisher::PkarrInvitePublisher>>,
    /// `Some` arms the OWNER-state debounced publish-root + persist after a
    /// successful inbound friend write, so the new friend reaches the user's
    /// other devices and survives a clean shutdown. `None` in tests (and when
    /// owner-state sync isn't running) — the field is optional so the friend
    /// write still succeeds locally without it. NOT the community sync engine.
    owner_sync_engine: Option<Arc<crate::owner_state_sync::SyncEngine>>,
    /// ZEB-371: `Some` reconciles this node's Case-D pkarr slots with the friend
    /// graph after a successful inbound accept, so a just-added friend's
    /// reachability slot is published immediately (not on the next reachability
    /// tick). `None` in tests and when pkarr isn't running — the friend write
    /// still succeeds locally without it.
    friend_publisher: Option<Arc<crate::pkarr_friend_publisher::PkarrFriendPublisher>>,
    /// ZEB-371 Task 12: process-local pending inbound friend-request store. The
    /// Path-A branch records a NEW owner's request here (reply `Pending`) and
    /// consumes a prior approval here (`take_approved`). `None` in tests and
    /// when the Path-A flow isn't wired — an absent store collapses the consent
    /// tree to "token path or unknown→Pending-with-no-record".
    pending_requests: Option<Arc<crate::friend_requests::PendingFriendRequests>>,
    /// ZEB-376: process-local pre-authorizations for introductions the user
    /// initiated. `Some` lets an inbound introduction-driven request from a
    /// pre-authorized target auto-accept inline as `established_via:
    /// Introduction` (one-shot + TTL-bounded, see `PendingOutboundIntroductions`).
    /// `None` in tests / when the flow isn't wired — introductions then fall
    /// through to the normal Pending prompt.
    pending_outbound: Option<Arc<crate::friend_requests::PendingOutboundIntroductions>>,
    /// ZEB-371 Task 12: the per-user "auto-accept known requesters" toggle
    /// (spec §7.1; Jake's "Both" choice, default ON). Only gates whether to
    /// PROMPT a KNOWN requester — never relaxes authentication, and never
    /// auto-accepts an UNKNOWN owner.
    auto_accept_known: bool,
    /// ZEB-461 Task 6: this node's own IMMUTABLE device bundle + PQ keys to
    /// advertise in the accept it signs. `None` (the default; tests) ships the
    /// empty bundle. Production wires the real values via `with_self_statics`.
    /// ZEB-621: the volatile `home_relay_url` is deliberately NOT stored here — it
    /// is read fresh per accept via `self_home_relay_refresh`.
    self_statics: Option<SelfHandshakeStatics>,
    /// ZEB-521/621: optional live read of this node's CURRENT iroh home-relay URL.
    /// This is the SOLE source of the `home_relay_url` the accept advertises — the
    /// acceptor holds no frozen snapshot to fall back to. Wired from the iroh
    /// endpoint so the relay isn't permanently `None`/stale when iroh's relay
    /// round-trip resolves (or flaps) after `start_node`. `None` in tests / when
    /// iroh didn't bind — the accept advertises no relay.
    self_home_relay_refresh: Option<HomeRelayRefresh>,
    /// ZEB-680 §2: live handle to this node's owner trust doc, read FRESH each
    /// handshake to build the own-fleet revocation attestations carried on the
    /// signed accept (`current_fresh_revocations` →
    /// `build_revocation_attestations`). A live handle — NOT a frozen boot
    /// snapshot — so a device revoked after `start_node` is still carried
    /// (mirrors the ZEB-621 `self_home_relay_refresh` fresh-read discipline).
    /// `None` in tests / when the owner isn't loaded → the accept carries none.
    self_trust_doc: Option<Arc<TokioMutex<harmony_owner::state::OwnerState>>>,
    /// ZEB-680 §1: the live by-owner revoked-device projection, consulted by the
    /// inbound `verify_enrolled_device` (via `authenticate_friend_request` +
    /// `process_friend_request`). A plain (non-`Option`) field — every path,
    /// tests included, always holds one; `with_config` seeds the EMPTY
    /// projection (revokes nothing) and production overrides it with the real
    /// `NodeState` handle via [`Self::with_revoked`]. Following the
    /// `intro_rate_limiter` precedent. Clone shares the inner `Arc<RwLock<..>>`,
    /// so this sees the live feed's writes.
    revoked: crate::revoked_device_projection::RevokedDeviceProjection,
    /// ZEB-700: two-tier rate limiter for this ALPN (the ZEB-694 pattern
    /// extended to friend/v1) — Tier 1 pre-auth connection shield + Tier 2
    /// post-auth per-owner window, budgets disjoint from the PEX acceptor's
    /// `intro_rate_limiter`. `with_config` seeds the production caps; tests
    /// override tiny/zero caps via [`Self::with_rate_limiter`].
    rate_limiter: Arc<crate::friend_intro::FriendRateLimiter>,
    config: FriendAcceptorConfig,
}

impl<H> IrohFriendHandshakeAcceptor<H>
where
    H: FriendEventEmit,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        crdt_state: Arc<TokioMutex<OwnerState>>,
        hlc_tracker: Arc<TokioMutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>>,
        device_id: String,
        self_owner: OwnerAddr,
        self_display: Option<String>,
        self_enrollment: EnrollmentCert,
        device2_signing_key: Arc<ed25519_dalek::SigningKey>,
        keytree: Arc<crate::owner_state_crypto::KeyTree>,
        app: Option<Arc<H>>,
        pkarr_invite_publisher: Option<Arc<crate::pkarr_invite_publisher::PkarrInvitePublisher>>,
    ) -> Self {
        Self::with_config(
            crdt_state,
            hlc_tracker,
            device_id,
            self_owner,
            self_display,
            self_enrollment,
            device2_signing_key,
            keytree,
            app,
            pkarr_invite_publisher,
            FriendAcceptorConfig::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_config(
        crdt_state: Arc<TokioMutex<OwnerState>>,
        hlc_tracker: Arc<TokioMutex<harmony_crdt_sync::ReplayTracker<String, Hlc>>>,
        device_id: String,
        self_owner: OwnerAddr,
        self_display: Option<String>,
        self_enrollment: EnrollmentCert,
        device2_signing_key: Arc<ed25519_dalek::SigningKey>,
        keytree: Arc<crate::owner_state_crypto::KeyTree>,
        app: Option<Arc<H>>,
        pkarr_invite_publisher: Option<Arc<crate::pkarr_invite_publisher::PkarrInvitePublisher>>,
        config: FriendAcceptorConfig,
    ) -> Self {
        Self {
            crdt_state,
            hlc_tracker,
            device_id,
            self_owner,
            self_display,
            self_enrollment,
            device2_signing_key,
            keytree,
            app,
            pkarr_invite_publisher,
            owner_sync_engine: None,
            friend_publisher: None,
            pending_requests: None,
            // ZEB-376: default to no introduction pre-auth; production wires it.
            pending_outbound: None,
            // ZEB-371 spec §7.1 default: auto-accept KNOWN requesters is ON.
            auto_accept_known: true,
            // ZEB-461: default to the empty self bundle; production fills it.
            self_statics: None,
            // ZEB-521: default to no live refresh; production wires it from the
            // iroh endpoint so the advertised home relay is read fresh per accept.
            self_home_relay_refresh: None,
            // ZEB-680 §2: default to no trust doc → the accept carries no
            // revocations; production wires the live handle via
            // `with_self_trust_doc` so they are built fresh per handshake.
            self_trust_doc: None,
            // ZEB-680 §1: default to the EMPTY projection (revokes nothing) —
            // production overrides via `with_revoked` with the real NodeState
            // handle. Tests keep the empty default.
            revoked: crate::revoked_device_projection::RevokedDeviceProjection::new(),
            // ZEB-700: production caps by default; tests shrink them via
            // `with_rate_limiter`. Honest single-handshake peers never hit them.
            rate_limiter: Arc::new(crate::friend_intro::FriendRateLimiter::new()),
            config,
        }
    }

    /// ZEB-700: override the friend-handshake rate limiter (tests use
    /// tiny/zero caps to force deterministic sheds; production keeps the
    /// `with_config` default).
    pub fn with_rate_limiter(
        mut self,
        rate_limiter: Arc<crate::friend_intro::FriendRateLimiter>,
    ) -> Self {
        self.rate_limiter = rate_limiter;
        self
    }

    /// ZEB-680 §1: wire in the live `RevokedDeviceProjection` so the inbound
    /// verifiers reject a revoked device. Fluent setter (default: the EMPTY
    /// projection from `with_config`) so existing call sites — including tests —
    /// keep compiling. PRODUCTION MUST call this with the real `NodeState`
    /// handle; a fresh `new()` here would silently disable enforcement.
    pub fn with_revoked(
        mut self,
        revoked: crate::revoked_device_projection::RevokedDeviceProjection,
    ) -> Self {
        self.revoked = revoked;
        self
    }

    /// Wire in the OWNER-state `SyncEngine` so a successful inbound friend write
    /// arms a debounced publish + persist. Fluent setter (rather than a 10th
    /// constructor arg) so existing call sites — including tests that build via
    /// `new`/`with_config` — keep compiling without an explicit `None`.
    pub fn with_owner_sync_engine(
        mut self,
        engine: Option<Arc<crate::owner_state_sync::SyncEngine>>,
    ) -> Self {
        self.owner_sync_engine = engine;
        self
    }

    /// ZEB-371: wire in this node's Case-D friend publisher so a successful
    /// inbound accept immediately reconciles the published friend slots (the
    /// just-accepted friend starts publishing without waiting for the next
    /// reachability tick). Fluent setter (default `None`) so existing call
    /// sites — including tests — keep compiling without an explicit `None`.
    pub fn with_friend_publisher(
        mut self,
        friend_publisher: Option<Arc<crate::pkarr_friend_publisher::PkarrFriendPublisher>>,
    ) -> Self {
        self.friend_publisher = friend_publisher;
        self
    }

    /// ZEB-371 Task 12: wire in the process-local pending inbound friend-request
    /// store (Path A). Fluent setter (default `None`) so existing call sites —
    /// including tests — keep compiling without an explicit `None`.
    pub fn with_pending_requests(
        mut self,
        pending: Option<Arc<crate::friend_requests::PendingFriendRequests>>,
    ) -> Self {
        self.pending_requests = pending;
        self
    }

    /// ZEB-376: wire in the process-local outbound-introduction pre-auth store so
    /// an inbound introduction-driven request from a pre-authorized target
    /// auto-accepts inline (stamped `established_via: Introduction`). Fluent
    /// setter (default `None`) so existing call sites — including tests — keep
    /// compiling without an explicit `None`.
    pub fn with_pending_outbound(
        mut self,
        pending_outbound: Option<Arc<crate::friend_requests::PendingOutboundIntroductions>>,
    ) -> Self {
        self.pending_outbound = pending_outbound;
        self
    }

    /// ZEB-371 Task 12: set the per-user "auto-accept known requesters" toggle
    /// (spec §7.1). Default is ON (set in `with_config`); production reads the
    /// persisted setting and passes it here.
    pub fn with_auto_accept_known(mut self, auto_accept_known: bool) -> Self {
        self.auto_accept_known = auto_accept_known;
        self
    }

    /// ZEB-461/621: advertise this node's IMMUTABLE device bundle + PQ keys in the
    /// outbound accept it signs. `None` (the default) ships the empty bundle. The
    /// volatile relay is NOT part of the statics — it is read fresh per accept via
    /// [`Self::with_self_home_relay_refresh`]. Fluent setter (default `None`) so
    /// existing call sites — including tests — keep compiling without an explicit
    /// `None`.
    pub fn with_self_statics(mut self, statics: Option<SelfHandshakeStatics>) -> Self {
        self.self_statics = statics;
        self
    }

    /// ZEB-521/621: wire a live read of this node's current iroh home-relay URL —
    /// the SOLE source of the `home_relay_url` the accept advertises (the acceptor
    /// stores no relay snapshot). Fluent setter (default `None`, used by tests) so
    /// existing call sites keep compiling.
    pub fn with_self_home_relay_refresh(mut self, refresh: Option<HomeRelayRefresh>) -> Self {
        self.self_home_relay_refresh = refresh;
        self
    }

    /// ZEB-680 §2: wire the live owner trust doc so each signed accept carries
    /// this node's own-fleet revocation attestations, built FRESH per handshake
    /// (a device revoked after `start_node` is still carried — no boot snapshot).
    /// Fluent setter (default `None`, used by tests) so existing call sites keep
    /// compiling; `None` carries no revocations.
    pub fn with_self_trust_doc(
        mut self,
        trust_doc: Option<Arc<TokioMutex<harmony_owner::state::OwnerState>>>,
    ) -> Self {
        self.self_trust_doc = trust_doc;
        self
    }

    /// ZEB-521/621: the live endpoint's CURRENT home relay (or `None` when no
    /// refresh closure is wired — tests/iroh-unbound — or the relay still hasn't
    /// resolved). Passed into [`process_friend_request`] as the SOLE `home_relay_url`
    /// source at accept-sign time. Cheap: a single closure call returning an
    /// `Option<String>`, no bundle clone.
    fn current_fresh_home_relay(&self) -> Option<String> {
        self.self_home_relay_refresh.as_ref().and_then(|f| f())
    }

    /// ZEB-680 §2: build this node's own-fleet revocation attestations FRESH from
    /// the live trust doc, for the accept about to be signed. Locks the trust doc
    /// (cloned under the guard is avoided — the builder borrows it) and drops the
    /// guard before returning, so it is never held across the subsequent
    /// `crdt_state` lock at the call site. `None` trust doc (tests / owner not
    /// loaded) → carries nothing.
    async fn current_fresh_revocations(&self) -> Vec<RevocationAttestation> {
        match &self.self_trust_doc {
            Some(doc) => build_revocation_attestations(&*doc.lock().await),
            None => Vec::new(),
        }
    }

    /// Reconcile the published Case-D friend slots with the current friend graph
    /// after an inbound accept. No-op when no friend publisher is wired in
    /// (tests / pkarr disabled). Snapshots the friend map under the CRDT lock and
    /// drops the guard BEFORE the network `.await`s in `sync_case_d_handles`, so
    /// the owner-state lock is never held across pkarr IO.
    async fn reconcile_case_d_slots(&self) {
        let Some(friend_pub) = self.friend_publisher.as_ref() else {
            return;
        };
        let friends = {
            let state = self.crdt_state.lock().await;
            state.friend_graph.friends.clone()
        };
        crate::pkarr_friend_publisher::sync_case_d_handles(friend_pub, &friends, &self.keytree)
            .await;
    }

    /// Arm the owner-state debounced publish + persist after a friend write.
    /// No-op when no engine is wired in. Split out so it's directly observable
    /// in a unit test (a real `SyncEngine` sharing this acceptor's `crdt_state`
    /// publishes after this fires).
    fn notify_owner_state_dirty(&self) {
        if let Some(engine) = self.owner_sync_engine.as_ref() {
            engine.notify_dirty();
        }
    }

    /// Signal the UI a friend was added (success side-effect of an accept). No
    /// app handle (tests) → debug-log only.
    fn emit_friend_added(&self, req: &FriendLinkRequest) {
        match self.app.as_ref() {
            Some(app) => app.emit_friend_list_changed(),
            None => tracing::debug!(
                from_addr = %hex::encode(req.from_addr.0),
                "friend added (no app handle); not emitting friend-list-changed"
            ),
        }
    }

    /// Write a length-prefixed friend-handshake response (`[u32 LE len][body]`),
    /// bounding the body at `FRIEND_MAX_PACKET_LEN` and timing out each await.
    /// Shared by the Accepted and Pending reply paths.
    async fn write_friend_response(
        &self,
        send: &mut iroh::endpoint::SendStream,
        resp: &[u8],
    ) -> Result<(), FriendAcceptError> {
        let resp_prefix = crate::iroh_framing::encode_len_prefix(
            resp.len(),
            FRIEND_MAX_PACKET_LEN,
            crate::iroh_framing::Endian::Le,
            false,
        )
        .map_err(|e| FriendAcceptError::ResponseTooLarge {
            len: e.len,
            max: e.max,
        })?;
        tokio::time::timeout(self.config.io_deadline, send.write_all(&resp_prefix))
            .await
            .map_err(|_| FriendAcceptError::IoTimeout {
                step: "write length-prefix",
            })?
            .map_err(|e| FriendAcceptError::WritePrefix(e.to_string()))?;
        tokio::time::timeout(self.config.io_deadline, send.write_all(resp))
            .await
            .map_err(|_| FriendAcceptError::IoTimeout { step: "write body" })?
            .map_err(|e| FriendAcceptError::WriteBody(e.to_string()))?;
        // `send.finish()` is sync — no timeout needed.
        send.finish()
            .map_err(|e| FriendAcceptError::Finish(e.to_string()))?;
        Ok(())
    }

    /// Bump-and-return a fresh HLC stamped with this device's id. Mirrors
    /// `profile_broadcast::OwnerStateBroadcastSource::next_hlc`.
    async fn next_hlc(&self) -> Hlc {
        let now_ms = wall_now_ms();
        let mut tracker = self.hlc_tracker.lock().await;
        // Delegates to the core tick kernel like every other minting path
        // (ZEB-759); this was an open-coded copy of the same rule.
        let tick = harmony_crdt_sync::HlcTick::next(
            tracker
                .accepted_from(&self.device_id)
                .map(harmony_crdt_sync::HlcTick::from),
            now_ms,
        );
        let next = Hlc {
            wall_ms: tick.wall_ms,
            logical: tick.logical,
            device_id: self.device_id.clone(),
        };
        tracker.observe_local(next.clone());
        next
    }

    /// ZEB-370 consent gate (ATOMIC check-and-consume). Returns `Ok(())` iff
    /// `token_sig` corresponds to a friend token THIS node minted + published and
    /// currently live (unexpired) — i.e. `self.pkarr_invite_publisher` is
    /// `Some(p)` AND `p.try_consume_friend_token(token_sig, now_ms)` wins the
    /// one-shot. Otherwise returns `Err(TokenNotLive { .. })` so the caller FAILS
    /// CLOSED: no friend is added and no accept is sent.
    ///
    /// On `Ok(())` the token is ALREADY CONSUMED from the publisher's live map
    /// (atomically with the liveness check, so two concurrent handshakes redeeming
    /// the same `token_sig` cannot both pass — closes the TOCTOU race). Having won
    /// the consume, the gate also stops the Case-A `friend:{hex}` DHT republish via
    /// `unregister_friend_token` (whose map-remove is now an idempotent no-op and
    /// whose real job here is to halt the republish loop). The connection is
    /// already established by this point, so stopping the publish now is safe even
    /// if the later `process_friend_request` were to fail.
    ///
    /// Without this gate, any peer reaching `harmony/friend/v1` with a self-signed
    /// request and an arbitrary `token_sig` would be auto-friended, bypassing the
    /// `harmony://friend/` token gate entirely. Split out as an `async fn` (not
    /// requiring a `Connection`) so it is directly unit-testable.
    async fn token_gate_open(&self, token_sig: &[u8; 64]) -> Result<(), FriendAcceptError> {
        let Some(publisher) = self.pkarr_invite_publisher.as_ref() else {
            // No publisher wired in → we cannot prove this node minted the token.
            // Fail closed.
            return Err(FriendAcceptError::TokenNotLive {
                reason: "no friend-token publisher to verify against",
            });
        };
        let now_ms = wall_now_ms();
        if publisher.try_consume_friend_token(token_sig, now_ms).await {
            // Won the one-shot: the token is now removed from the live map. Stop
            // the Case-A DHT republish too (the map-remove inside is a no-op now;
            // the republish-stop is the point). Doing this at the gate — rather
            // than after process_friend_request — guarantees consume + DHT-stop
            // happen exactly once, atomically w.r.t. concurrent redeems.
            publisher.unregister_friend_token(token_sig).await;
            Ok(())
        } else {
            Err(FriendAcceptError::TokenNotLive {
                reason: "token_sig is not a live minted friend token (unregistered or expired)",
            })
        }
    }

    /// Inbound bi-stream handler: read the length-prefixed `FriendLinkRequest`,
    /// AUTHENTICATE it (always), run the spec §7.1 consent decision tree, then
    /// side-effect + write the length-prefixed `FriendLinkResponse`:
    ///   * `TokenPath` → token gate + accept (reply `Accepted`),
    ///   * `AcceptInline` → accept with no token gate (reply `Accepted`),
    ///   * `Pending` → record the request + reply `Pending` (write NO friend).
    ///
    /// Returns WHICH benign outcome was delivered ([`FriendInboundOutcome`])
    /// so the dispatcher's completion log reports the real result (ZEB-700
    /// review: an `Ok` here is not necessarily a delivered accept).
    async fn handle_friend_handshake_inbound(
        &self,
        conn: &Connection,
    ) -> Result<FriendInboundOutcome, FriendAcceptError> {
        let (mut send, mut recv) = tokio::time::timeout(self.config.io_deadline, conn.accept_bi())
            .await
            .map_err(|_| FriendAcceptError::IoTimeout { step: "accept_bi" })?
            .map_err(|e| FriendAcceptError::AcceptBi(e.to_string()))?;

        // ZEB-700 Tier 1 (pre-auth connection shield): shed a flooding endpoint
        // BEFORE any stream read — so a malformed or slow/trickled frame can't
        // consume framing I/O without consuming the per-connection budget
        // (CodeRabbit R1) — and before decode + all crypto (cert chain +
        // handshake sig + up to 32 carried revocation attestations), keyed on
        // the connecting endpoint's authenticated iroh id — un-spoofable.
        // (Earlier than the PEX precedent's post-decode placement, which only
        // exists to select an arm; friend/v1 has one arm, and the benign reply
        // doesn't depend on the request.) On a shed we LOG ("no silent
        // truncation") and write the SAME benign `Pending` reply the Path-A
        // outcome writes — network-indistinguishable (no oracle) and it leaks
        // nothing about consent state — with ZERO state effect: nothing is
        // recorded, no token consumed, no friend written. An honest peer that
        // somehow trips the cap self-heals by re-dialing after the window (a
        // live token stays live and redeemable).
        // ZEB-711: the limiter timeline is the limiter's own monotonic
        // clock, never wall time — a wall step would distort the window
        // (forward jump: flood gets a fresh budget; backward jump: honest
        // shed peer stays shed).
        let limiter_now_ms = self.rate_limiter.monotonic_now_ms();
        if let Err(reason) = self
            .rate_limiter
            .admit_connection(*conn.remote_id().as_bytes(), limiter_now_ms)
        {
            tracing::warn!(
                reason,
                "ZEB-700: friend handshake shed by connection shield"
            );
            let resp = encode_friend_response(&FriendLinkResponse::Pending)
                .map_err(FriendAcceptError::Handshake)?;
            self.write_friend_response(&mut send, &resp).await?;
            return Ok(FriendInboundOutcome::Shed);
        }

        // Read [u32 LE length-prefix][body].
        let mut len_buf = [0u8; 4];
        tokio::time::timeout(self.config.io_deadline, recv.read_exact(&mut len_buf))
            .await
            .map_err(|_| FriendAcceptError::IoTimeout {
                step: "read length-prefix",
            })?
            .map_err(|e| FriendAcceptError::ReadPrefix(e.to_string()))?;
        let len = crate::iroh_framing::decode_len_prefix(
            len_buf,
            FRIEND_MAX_PACKET_LEN,
            crate::iroh_framing::Endian::Le,
            false,
        )
        .map_err(|e| FriendAcceptError::PrefixOutOfBounds {
            len: e.len,
            max: e.max,
        })?;
        let mut body = vec![0u8; len];
        tokio::time::timeout(self.config.io_deadline, recv.read_exact(&mut body))
            .await
            .map_err(|_| FriendAcceptError::IoTimeout { step: "read body" })?
            .map_err(|e| FriendAcceptError::ReadBody(e.to_string()))?;

        let req = decode_friend_request(&body).map_err(FriendAcceptError::Handshake)?;

        // ZEB-371 Task 12 (spec §7.1): AUTHENTICATE ALWAYS, then decide consent.
        // Authentication (cert chain + handshake-sig verify) runs UNCONDITIONALLY
        // here — the consent flags below only gate whether to PROMPT, never
        // whether to authenticate. A request that fails auth is rejected before
        // any consent branch (no friend written, no record kept, no accept sent).
        //
        // ZEB-378: sample the expiry clock ONCE per inbound handshake and thread it
        // through both the pre-consent auth and the post-token-gate accept. Using one
        // instant means a cert can't pass auth here yet fail the later accept (which
        // would burn the one-shot token), and both checks agree on a single time.
        let now_secs = wall_now_secs();
        authenticate_friend_request(&req, &self.revoked, now_secs)
            .map_err(FriendAcceptError::Handshake)?;

        // ZEB-700 Tier 2 (post-auth per-owner quota): `req.from_addr` is now
        // AUTHENTICATED, so the window keys on a real owner. Deliberately
        // AFTER auth — an unauthenticated request is still refused above, so a
        // shed never masks an auth failure — and BEFORE any lock/consent work.
        // Same benign `Pending` shed as Tier 1 (zero state effect); the legit
        // re-dial flows (`Pending` → approve → re-dial, `Pending` → token →
        // re-dial) stay far under the cap — see `FriendRateLimiter` for why
        // this tier has no dedupe. Reuses Tier 1's `now` sample so the two
        // tiers can't straddle a window boundary within one handshake.
        if let Err(reason) = self.rate_limiter.admit_owner(req.from_addr, limiter_now_ms) {
            tracing::warn!(
                reason,
                key = %hex::encode(req.from_addr.0),
                "ZEB-700: friend handshake shed by per-owner quota"
            );
            let resp = encode_friend_response(&FriendLinkResponse::Pending)
                .map_err(FriendAcceptError::Handshake)?;
            self.write_friend_response(&mut send, &resp).await?;
            return Ok(FriendInboundOutcome::Shed);
        }

        // Compute `known` under the CRDT lock: is the requester already an
        // Active|Pending friend? Snapshot the boolean and DROP the guard before
        // any network await (never hold the owner-state lock across IO).
        // TODO Phase 2: community co-member also counts as `known`.
        let known = {
            let state = self.crdt_state.lock().await;
            state
                .friend_graph
                .friends
                .get(&req.from_addr)
                .map(|e| matches!(e.status, FriendStatus::Active | FriendStatus::Pending))
                .unwrap_or(false)
        };
        // Resolve consent, consuming any one-shot approval ATOMICALLY so
        // concurrent handshakes from the same approved requester cannot all
        // inline-accept and derive mismatched Case-D secrets (see
        // `resolve_consent_consuming_approval`).
        let (accepted, revocations_inserted) = match resolve_consent_consuming_approval(
            self.pending_requests.as_deref(),
            self.pending_outbound.as_deref(),
            req.token_sig.as_ref(),
            known,
            self.auto_accept_known,
            &req.from_addr,
            wall_now_ms(),
        ) {
            ConsentDecision::TokenPath => {
                // ZEB-370 token gate (FAIL CLOSED): require `req.token_sig` is a
                // friend token THIS node actually minted + published and still
                // live. A self-signed request carrying an arbitrary `token_sig`
                // we never minted is rejected here. `decide_consent` only returns
                // TokenPath when `token_sig` is Some, so the unwrap-via-expect is
                // unreachable in practice; we re-extract it defensively.
                let token_sig = req.token_sig.ok_or(FriendAcceptError::TokenNotLive {
                    reason: "token path reached without a token_sig (logic bug)",
                })?;
                self.token_gate_open(&token_sig).await?;
                let learned_at = self.next_hlc().await;
                // ZEB-521/621: read the live endpoint's current home relay — the
                // SOLE relay source (the acceptor holds no boot snapshot) — so the
                // signed accept advertises a resolvable relay even though iroh's
                // relay round-trip often hasn't resolved at start_node time.
                let fresh_home_relay = self.current_fresh_home_relay();
                // ZEB-680 §2: build our own-fleet revocations fresh from the live
                // trust doc, before the crdt_state lock (never both at once).
                let self_revocations = self.current_fresh_revocations().await;
                let (accepted, revocations_inserted) = {
                    let mut state = self.crdt_state.lock().await;
                    process_friend_request(
                        &mut state,
                        learned_at,
                        &req,
                        self.self_owner,
                        self.self_display.clone(),
                        &self.self_enrollment,
                        &self.device2_signing_key,
                        &self.keytree,
                        &self.revoked,
                        now_secs,
                        self.self_statics.as_ref(),
                        fresh_home_relay,
                        None,
                        self_revocations,
                    )
                    .map_err(FriendAcceptError::Handshake)?
                };
                // Drop any stale pending-inbox entry: this requester may have
                // received `Pending` before obtaining (and now redeeming) the
                // token, which would otherwise leave a ghost request in the UI.
                if let Some(pending) = self.pending_requests.as_ref() {
                    pending.clear_completed(&req.from_addr);
                }
                self.emit_friend_added(&req);
                (accepted, revocations_inserted)
            }
            ConsentDecision::AcceptInline => {
                // Path A, known-or-pre-approved: accept inline with NO token gate.
                // `process_friend_request` resolves `established_via` to MutualKey
                // because `req.token_sig` is None on this path.
                let learned_at = self.next_hlc().await;
                // ZEB-521/621: read the live endpoint's current home relay — the
                // SOLE relay source (the acceptor holds no boot snapshot) — so the
                // signed accept advertises a resolvable relay even though iroh's
                // relay round-trip often hasn't resolved at start_node time.
                let fresh_home_relay = self.current_fresh_home_relay();
                // ZEB-680 §2: build our own-fleet revocations fresh from the live
                // trust doc, before the crdt_state lock (never both at once).
                let self_revocations = self.current_fresh_revocations().await;
                let (accepted, revocations_inserted) = {
                    let mut state = self.crdt_state.lock().await;
                    process_friend_request(
                        &mut state,
                        learned_at,
                        &req,
                        self.self_owner,
                        self.self_display.clone(),
                        &self.self_enrollment,
                        &self.device2_signing_key,
                        &self.keytree,
                        &self.revoked,
                        now_secs,
                        self.self_statics.as_ref(),
                        fresh_home_relay,
                        None,
                        self_revocations,
                    )
                    .map_err(FriendAcceptError::Handshake)?
                };
                // Link completed: consume the one-shot approval AND drop any
                // stale pending-inbox entry. The requester may have first
                // received `Pending` (recorded in the inbox) before becoming
                // known/approved — clear it so the UI shows no ghost request.
                if let Some(pending) = self.pending_requests.as_ref() {
                    pending.clear_completed(&req.from_addr);
                }
                self.emit_friend_added(&req);
                (accepted, revocations_inserted)
            }
            ConsentDecision::AcceptInlineIntroduced => {
                // ZEB-376: an introduction the user pre-authorized. Accept inline
                // with NO token gate (same as AcceptInline) but stamp
                // `established_via: Introduction` via the origin override.
                let learned_at = self.next_hlc().await;
                let fresh_home_relay = self.current_fresh_home_relay();
                // ZEB-680 §2: build our own-fleet revocations fresh from the live
                // trust doc, before the crdt_state lock (never both at once).
                let self_revocations = self.current_fresh_revocations().await;
                let (accepted, revocations_inserted) = {
                    let mut state = self.crdt_state.lock().await;
                    process_friend_request(
                        &mut state,
                        learned_at,
                        &req,
                        self.self_owner,
                        self.self_display.clone(),
                        &self.self_enrollment,
                        &self.device2_signing_key,
                        &self.keytree,
                        &self.revoked,
                        now_secs,
                        self.self_statics.as_ref(),
                        fresh_home_relay,
                        Some(crate::friend_graph::FriendOrigin::Introduction),
                        self_revocations,
                    )
                    .map_err(FriendAcceptError::Handshake)?
                };
                // Link completed: drop any stale pending-inbox entry (the target
                // may have first received `Pending` before the introduction dial).
                if let Some(pending) = self.pending_requests.as_ref() {
                    pending.clear_completed(&req.from_addr);
                }
                self.emit_friend_added(&req);
                (accepted, revocations_inserted)
            }
            ConsentDecision::Pending => {
                // Path A, NEW owner: record the request + reply Pending. WRITE NO
                // FRIEND. The user's accept (next task's IPC) marks it approved so
                // the requester's NEXT dial is accepted inline.
                let now_ms = wall_now_ms();
                if let Some(pending) = self.pending_requests.as_ref() {
                    pending.record_inbound(req.from_addr, req.display.clone(), now_ms);
                }
                match self.app.as_ref() {
                    Some(app) => app.emit_friend_request_received(),
                    None => tracing::debug!(
                        from_addr = %hex::encode(req.from_addr.0),
                        "friend request recorded (no app handle); not emitting friend-request-received"
                    ),
                }
                // Reply Pending and return: no token cleanup, no owner-state
                // dirty, no Case-D reconcile (no friend was written).
                let resp = encode_friend_response(&FriendLinkResponse::Pending)
                    .map_err(FriendAcceptError::Handshake)?;
                self.write_friend_response(&mut send, &resp).await?;
                return Ok(FriendInboundOutcome::Pending);
            }
        };

        // Arm the OWNER-state debounced publish-root + persist BEFORE the
        // peer-facing accept-write. `process_friend_request` above wrote a new
        // Active/Token FriendEntry (and MAY have folded the requester's carried
        // own-fleet revocations into the SAME OwnerState CRDT). Notifying here —
        // ahead of the fallible `write_friend_response` below — guarantees those
        // local CRDT mutations reach the user's other devices and survive a clean
        // shutdown even if the accept-write IO fails: a write failure must not
        // strand an applied-but-unpersisted mutation (review). The peer simply
        // re-handshakes on a failed accept and the idempotent re-apply converges.
        // No-op when no engine is wired in (tests). The Pending arm returned
        // earlier and never reaches here, so this only fires on an actual accept.
        // The `revocations_inserted` flag drives only the learn-at-link log.
        if revocations_inserted {
            tracing::info!(
                from_addr = %hex::encode(req.from_addr.0),
                "ZEB-680: accepted friend link carried a new device revocation; folded into the owner-state publish"
            );
        }
        self.notify_owner_state_dirty();

        // Write [u32 LE length-prefix][FriendLinkResponse::Accepted CBOR].
        let resp = encode_friend_response(&FriendLinkResponse::Accepted(Box::new(accepted)))
            .map_err(FriendAcceptError::Handshake)?;
        self.write_friend_response(&mut send, &resp).await?;

        // The Case-A one-shot was consumed + its DHT republish stopped atomically
        // at the gate (see `token_gate_open`), so there is no token cleanup to do
        // here. Consuming at the gate (rather than after a successful accept-write)
        // is what closes the concurrent-redeem TOCTOU: two handshakes racing the
        // same `token_sig` cannot both pass. The connection is already established
        // by the time we consume, so stopping the publish early is safe even if a
        // later step fails — a re-redeem would find the token already consumed.

        // ZEB-371: publish the just-accepted friend's Case-D reachability slot
        // immediately (rather than waiting for the next reachability tick) so the
        // new friend can resolve us right away. No-op when pkarr isn't running.
        // Snapshots the friend graph under the lock, drops it, THEN does pkarr IO.
        self.reconcile_case_d_slots().await;
        Ok(FriendInboundOutcome::Accepted)
    }
}

#[async_trait]
impl<H> crate::iroh_invite_acceptor::IrohHandshakeDispatcher for IrohFriendHandshakeAcceptor<H>
where
    H: FriendEventEmit,
{
    async fn handle_connection(&self, conn: Connection) {
        match self.handle_friend_handshake_inbound(&conn).await {
            // ZEB-700 review (Qodo R1): log the ACTUAL outcome — `Ok` covers
            // delivered accepts, consent-`Pending` replies, AND rate-limit
            // sheds, so an unconditional "accept delivered" here would
            // misreport the latter two (the Pending mislabel predated ZEB-700).
            Ok(outcome) => tracing::info!(
                remote_id = ?conn.remote_id(),
                outcome = ?outcome,
                "ZEB-370: friend handshake completed"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                remote_id = ?conn.remote_id(),
                "ZEB-370: friend handshake failed"
            ),
        }
        // Wait for the dialer to drive the close so the response bytes flush
        // before `conn` drops (same race-avoidance as iroh_invite_acceptor).
        let _ = tokio::time::timeout(self.config.io_deadline, conn.closed()).await;
    }
}

/// ZEB-700 review (Qodo R1): which benign outcome the inbound handler actually
/// delivered, so the dispatcher's completion log reports the real result
/// instead of claiming "accept delivered" for every `Ok` (the `Pending`
/// mislabel predated ZEB-700; the shed paths widened it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FriendInboundOutcome {
    /// A `FriendLinkResponse::Accepted` was written — a friend was added.
    Accepted,
    /// The consent tree recorded the request and wrote `Pending`.
    Pending,
    /// A rate-limit tier shed the handshake with the benign `Pending` reply
    /// (zero state effect); the tier already `warn!`-logged the shed reason.
    Shed,
}

/// Errors that can short-circuit the inbound friend handshake. The crypto/codec
/// failures are wrapped from [`FriendHandshakeError`]; the rest are IO framing.
#[derive(Debug, thiserror::Error)]
pub enum FriendAcceptError {
    #[error("accept_bi failed: {0}")]
    AcceptBi(String),
    #[error("read length-prefix: {0}")]
    ReadPrefix(String),
    #[error("length-prefix out of bounds: len={len} max={max}")]
    PrefixOutOfBounds { len: usize, max: usize },
    #[error("read body: {0}")]
    ReadBody(String),
    #[error("handshake: {0}")]
    Handshake(#[source] FriendHandshakeError),
    /// ZEB-370 consent gate (FAIL CLOSED): the request's `token_sig` is not a
    /// live friend token this node minted + published — either no
    /// `pkarr_invite_publisher` is wired in, or the token was never registered,
    /// or it has expired. The friend is NOT added and no accept is sent.
    #[error("friend token not live (consent gate): {reason}")]
    TokenNotLive { reason: &'static str },
    #[error("response too large: len={len} max={max}")]
    ResponseTooLarge { len: usize, max: usize },
    #[error("write length-prefix: {0}")]
    WritePrefix(String),
    #[error("write body: {0}")]
    WriteBody(String),
    #[error("send.finish: {0}")]
    Finish(String),
    #[error("IO timeout in {step}")]
    IoTimeout { step: &'static str },
}

// =====================================================================
// Task 9 — ALPN dispatch multiplexer
// =====================================================================

/// Which inner dispatcher an inbound connection's negotiated ALPN routes to.
/// Factored out as a pure value so the routing decision is unit-testable
/// without constructing a live `iroh::endpoint::Connection`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FriendDispatchTarget {
    /// `harmony/friend/v1` → the friend-link acceptor.
    Friend,
    /// `harmony/handshake/v1` (and any other non-friend handshake ALPN the
    /// accept loop forwards) → the invite acceptor. The accept loop only ever
    /// hands the multiplexer connections whose ALPN it already matched to one
    /// of the two handshake ALPNs, so the invite acceptor is the correct
    /// default for "not the friend ALPN".
    Invite,
    /// ZEB-375 `harmony/friend-pex/v1` → the friend-PEX referral-catalog
    /// acceptor (`iroh_pex_acceptor::IrohFriendPexAcceptor`).
    Pex,
}

/// Pure ALPN → target decision. `HARMONY_FRIEND_PEX_V1` routes to the friend-PEX
/// acceptor; `HARMONY_FRIEND_V1` routes to the friend acceptor; everything else
/// (the accept loop only forwards `HARMONY_HANDSHAKE_V1` besides the two friend
/// ALPNs) routes to the invite acceptor. The PEX ALPN is matched FIRST.
pub fn route_handshake_alpn(alpn: &[u8]) -> FriendDispatchTarget {
    if alpn == crate::iroh_endpoint::alpn::HARMONY_FRIEND_PEX_V1 {
        FriendDispatchTarget::Pex
    } else if alpn == crate::iroh_endpoint::alpn::HARMONY_FRIEND_V1 {
        FriendDispatchTarget::Friend
    } else {
        FriendDispatchTarget::Invite
    }
}

/// Multiplexing [`IrohHandshakeDispatcher`] that fans an inbound connection to
/// the friend or invite acceptor based on its negotiated ALPN
/// ([`iroh::endpoint::Connection::alpn`]). Installed as the SINGLE dispatcher on
/// the iroh link manager (the accept loop forwards both `HARMONY_HANDSHAKE_V1`
/// and `HARMONY_FRIEND_V1` to it); the multiplexer then re-reads `conn.alpn()`
/// and delegates.
pub struct MultiplexHandshakeDispatcher {
    /// Receives `harmony/handshake/v1` connections (community-invite redemption).
    invite: Arc<dyn crate::iroh_invite_acceptor::IrohHandshakeDispatcher>,
    /// Receives `harmony/friend/v1` connections (friend-link handshake).
    friend: Arc<dyn crate::iroh_invite_acceptor::IrohHandshakeDispatcher>,
    /// ZEB-375: receives `harmony/friend-pex/v1` connections (referral catalog).
    pex: Arc<dyn crate::iroh_invite_acceptor::IrohHandshakeDispatcher>,
}

impl MultiplexHandshakeDispatcher {
    /// Build a multiplexer over the invite + friend + friend-PEX acceptors.
    pub fn new(
        invite: Arc<dyn crate::iroh_invite_acceptor::IrohHandshakeDispatcher>,
        friend: Arc<dyn crate::iroh_invite_acceptor::IrohHandshakeDispatcher>,
        pex: Arc<dyn crate::iroh_invite_acceptor::IrohHandshakeDispatcher>,
    ) -> Self {
        Self {
            invite,
            friend,
            pex,
        }
    }
}

impl MultiplexHandshakeDispatcher {
    /// Select (by reference) the inner dispatcher an ALPN routes to, without
    /// consuming a `Connection`. The thin `handle_connection` impl forwards the
    /// owned `Connection` to whichever this returns; splitting the selection out
    /// lets unit tests assert routing with stub dispatchers (a live
    /// `iroh::endpoint::Connection` can't be constructed in-process).
    fn select_for_alpn(
        &self,
        alpn: &[u8],
    ) -> &Arc<dyn crate::iroh_invite_acceptor::IrohHandshakeDispatcher> {
        match route_handshake_alpn(alpn) {
            FriendDispatchTarget::Friend => &self.friend,
            FriendDispatchTarget::Invite => &self.invite,
            FriendDispatchTarget::Pex => &self.pex,
        }
    }
}

#[async_trait]
impl crate::iroh_invite_acceptor::IrohHandshakeDispatcher for MultiplexHandshakeDispatcher {
    async fn handle_connection(&self, conn: Connection) {
        // Re-read the negotiated ALPN the accept loop already matched and
        // delegate the owned connection to the selected acceptor.
        self.select_for_alpn(conn.alpn())
            .handle_connection(conn)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_membership::mint_test_owner;
    use ed25519_dalek::Signer;

    /// ZEB-680: an empty revoked-device projection for the many verifier call
    /// sites here that don't exercise revocation (it revokes nothing).
    fn no_revocations() -> crate::revoked_device_projection::RevokedDeviceProjection {
        crate::revoked_device_projection::RevokedDeviceProjection::new()
    }

    /// Build a signed, well-formed `FriendLinkRequest` from a test owner.
    /// Returns the request, the requester's enrolled device verify-key, and the
    /// (deterministic-per-run) ephemeral X25519 public key it committed to.
    fn signed_request(
        owner_seed: u8,
        token_sig: [u8; 64],
    ) -> (FriendLinkRequest, [u8; 32], [u8; 32]) {
        let owner = mint_test_owner(owner_seed);
        let device_key = owner.cert.device_pubkeys.classical.ed25519_verify;
        let (_eph_sk, eph_pub) = crate::friend_rendezvous::generate_ephemeral();
        // ZEB-461: sign over the (empty) device-bundle digest so the request
        // authenticates. Tests that need a populated bundle re-sign explicitly.
        let devices: Vec<DeviceIdentityHash> = vec![];
        let device_pubs: Vec<Option<[u8; 64]>> = vec![];
        let devices_digest = contact_digest(&devices, &device_pubs, &[0u8; 32], None, &[], &[]);
        let preimage =
            friend_request_sig_preimage(owner.owner, Some(&token_sig), &eph_pub, &devices_digest);
        let sig = owner.device_key.sign(&preimage).to_bytes();
        let req = FriendLinkRequest {
            from_addr: owner.owner,
            display: Some("alice".into()),
            token_sig: Some(token_sig),
            eph_x25519_pub: eph_pub,
            enrollment: owner.cert,
            signer_certs: Vec::new(),
            sig,
            sender_devices: devices,
            device_identity_pubs: device_pubs,
            iroh_node_id: [0u8; 32],
            home_relay_url: None,
            pq_dsa_pubkey: vec![],
            pq_kem_pubkey: vec![],
            revocations: Vec::new(),
        };
        (req, device_key, eph_pub)
    }

    #[test]
    fn friend_request_round_trips() {
        let (req, _, _) = signed_request(0x21, [9u8; 64]);
        let bytes = encode_friend_request(&req).expect("encode");
        let back = decode_friend_request(&bytes).expect("decode");
        assert_eq!(req, back);
    }

    /// ZEB-461: a request with ALL new bundle/reachability/PQ fields populated
    /// (non-empty) must round-trip byte-identically through the codec.
    #[test]
    fn friend_request_roundtrips_with_device_bundle() {
        let (mut req, _, _) = signed_request(0x21, [9u8; 64]);
        req.sender_devices = vec![
            DeviceIdentityHash([0xaa; 16]),
            DeviceIdentityHash([0xbb; 16]),
        ];
        req.device_identity_pubs = vec![Some([0xcc; 64]), None];
        req.iroh_node_id = [0xde; 32];
        req.home_relay_url = Some("https://relay.example/".into());
        req.pq_dsa_pubkey = vec![1, 2, 3, 4, 5];
        req.pq_kem_pubkey = vec![9, 8, 7];
        let bytes = encode_friend_request(&req).expect("encode");
        let back = decode_friend_request(&bytes).expect("decode");
        assert_eq!(req, back);
    }

    /// ZEB-461 (Qodo): an oversized `sender_devices` (> MAX_DEVICES_PER_OWNER) is
    /// REJECTED on decode — defense-in-depth against a hostile peer padding the
    /// bundle. Serialize is pass-through, so we can build the oversized wire; the
    /// cap fires on the receive side.
    #[test]
    fn friend_request_rejects_oversized_sender_devices() {
        use crate::owner_state_types::MAX_DEVICES_PER_OWNER;
        let (mut req, _, _) = signed_request(0x21, [9u8; 64]);
        req.sender_devices = (0..=MAX_DEVICES_PER_OWNER as u8)
            .map(|i| DeviceIdentityHash([i; 16]))
            .collect();
        assert!(req.sender_devices.len() > MAX_DEVICES_PER_OWNER);
        let bytes = encode_friend_request(&req).expect("encode (serialize is pass-through)");
        let err = decode_friend_request(&bytes);
        assert!(
            matches!(err, Err(FriendHandshakeError::Decode(_))),
            "oversized sender_devices must be rejected on decode, got: {err:?}"
        );
    }

    /// ZEB-461: an accept with ALL new fields populated must round-trip.
    #[test]
    fn friend_accepted_roundtrips_with_device_bundle() {
        let owner = mint_test_owner(0x22);
        let acc = FriendLinkAccepted {
            from_addr: owner.owner,
            display: None,
            eph_x25519_pub: [0x77; 32],
            enrollment: owner.cert,
            signer_certs: Vec::new(),
            sig: [4u8; 64],
            sender_devices: vec![DeviceIdentityHash([0x11; 16])],
            device_identity_pubs: vec![Some([0x22; 64])],
            iroh_node_id: [0x33; 32],
            home_relay_url: Some("https://relay.example/accept".into()),
            pq_dsa_pubkey: vec![42; 8],
            pq_kem_pubkey: vec![7; 4],
            revocations: Vec::new(),
        };
        let bytes = encode_friend_accepted(&acc).expect("encode");
        let back = decode_friend_accepted(&bytes).expect("decode");
        assert_eq!(acc, back);
    }

    #[test]
    fn friend_accepted_round_trips() {
        let owner = mint_test_owner(0x22);
        let acc = FriendLinkAccepted {
            from_addr: owner.owner,
            display: None,
            eph_x25519_pub: [0x77; 32],
            enrollment: owner.cert,
            signer_certs: Vec::new(),
            sig: [4u8; 64],
            sender_devices: vec![],
            device_identity_pubs: vec![],
            iroh_node_id: [0u8; 32],
            home_relay_url: None,
            pq_dsa_pubkey: vec![],
            pq_kem_pubkey: vec![],
            revocations: Vec::new(),
        };
        let bytes = encode_friend_accepted(&acc).expect("encode");
        let back = decode_friend_accepted(&bytes).expect("decode");
        assert_eq!(acc, back);
    }

    #[test]
    fn decode_rejects_oversized_request() {
        let huge = vec![0u8; FRIEND_MAX_PACKET_LEN + 1];
        match decode_friend_request(&huge) {
            Err(FriendHandshakeError::TooLarge { .. }) => {}
            other => panic!("expected TooLarge, got {other:?}"),
        }
        match decode_friend_accepted(&huge) {
            Err(FriendHandshakeError::TooLarge { .. }) => {}
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn decode_rejects_oversized_display_request() {
        // FIX 1: an authenticated peer must not be able to push a display longer
        // than MAX_FRIEND_DISPLAY_LEN through the handshake. The cap is enforced
        // at decode (wire ingress), mirroring FriendEntry.display.
        use crate::friend_graph::MAX_FRIEND_DISPLAY_LEN;
        let (mut req, _, _) = signed_request(0x40, [3u8; 64]);

        // 257-byte display → must FAIL to decode.
        req.display = Some("x".repeat(MAX_FRIEND_DISPLAY_LEN + 1));
        let bytes = encode_friend_request(&req).expect("encode (serialize is uncapped)");
        let err = decode_friend_request(&bytes).expect_err("oversized display rejected");
        assert!(
            matches!(err, FriendHandshakeError::Decode(_)),
            "expected Decode error, got {err:?}"
        );

        // 256-byte display (exactly at the cap) → still decodes.
        req.display = Some("y".repeat(MAX_FRIEND_DISPLAY_LEN));
        let bytes = encode_friend_request(&req).expect("encode");
        let back = decode_friend_request(&bytes).expect("at-cap display decodes");
        assert_eq!(req, back);
    }

    #[test]
    fn decode_rejects_oversized_display_accepted() {
        // FIX 1, accept side (matters for the Task-10 redeem path that turns an
        // accept into a local FriendEntry).
        use crate::friend_graph::MAX_FRIEND_DISPLAY_LEN;
        let owner = mint_test_owner(0x41);
        let mut acc = FriendLinkAccepted {
            from_addr: owner.owner,
            display: Some("x".repeat(MAX_FRIEND_DISPLAY_LEN + 1)),
            eph_x25519_pub: [0x77; 32],
            enrollment: owner.cert,
            signer_certs: Vec::new(),
            sig: [4u8; 64],
            sender_devices: vec![],
            device_identity_pubs: vec![],
            iroh_node_id: [0u8; 32],
            home_relay_url: None,
            pq_dsa_pubkey: vec![],
            pq_kem_pubkey: vec![],
            revocations: Vec::new(),
        };

        // 257-byte display → must FAIL to decode.
        let bytes = encode_friend_accepted(&acc).expect("encode (serialize is uncapped)");
        let err = decode_friend_accepted(&bytes).expect_err("oversized display rejected");
        assert!(
            matches!(err, FriendHandshakeError::Decode(_)),
            "expected Decode error, got {err:?}"
        );

        // 256-byte display (exactly at the cap) → still decodes.
        acc.display = Some("y".repeat(MAX_FRIEND_DISPLAY_LEN));
        let bytes = encode_friend_accepted(&acc).expect("encode");
        let back = decode_friend_accepted(&bytes).expect("at-cap display decodes");
        assert_eq!(acc, back);
    }

    #[test]
    fn decode_rejects_trailing_bytes_request() {
        // FIX 2: a valid request packet with extra trailing bytes appended (still
        // within FRIEND_MAX_PACKET_LEN) must be rejected; the clean packet still
        // round-trips.
        let (req, _, _) = signed_request(0x42, [5u8; 64]);
        let bytes = encode_friend_request(&req).expect("encode");

        // Clean packet round-trips.
        let back = decode_friend_request(&bytes).expect("clean packet decodes");
        assert_eq!(req, back);

        // Append trailing garbage → must be rejected as TrailingBytes.
        let mut trailing = bytes.clone();
        trailing.extend_from_slice(&[0xff, 0x00, 0x42]);
        assert!(trailing.len() <= FRIEND_MAX_PACKET_LEN);
        let err = decode_friend_request(&trailing).expect_err("trailing bytes rejected");
        assert!(
            matches!(err, FriendHandshakeError::TrailingBytes { .. }),
            "expected TrailingBytes, got {err:?}"
        );
    }

    #[test]
    fn decode_rejects_trailing_bytes_accepted() {
        // FIX 2, accept side.
        let owner = mint_test_owner(0x43);
        let acc = FriendLinkAccepted {
            from_addr: owner.owner,
            display: None,
            eph_x25519_pub: [0x77; 32],
            enrollment: owner.cert,
            signer_certs: Vec::new(),
            sig: [6u8; 64],
            sender_devices: vec![],
            device_identity_pubs: vec![],
            iroh_node_id: [0u8; 32],
            home_relay_url: None,
            pq_dsa_pubkey: vec![],
            pq_kem_pubkey: vec![],
            revocations: Vec::new(),
        };
        let bytes = encode_friend_accepted(&acc).expect("encode");

        // Clean packet round-trips.
        let back = decode_friend_accepted(&bytes).expect("clean packet decodes");
        assert_eq!(acc, back);

        // Append trailing garbage → must be rejected as TrailingBytes.
        let mut trailing = bytes.clone();
        trailing.extend_from_slice(&[0x01, 0x02]);
        assert!(trailing.len() <= FRIEND_MAX_PACKET_LEN);
        let err = decode_friend_accepted(&trailing).expect_err("trailing bytes rejected");
        assert!(
            matches!(err, FriendHandshakeError::TrailingBytes { .. }),
            "expected TrailingBytes, got {err:?}"
        );
    }

    // ---- ZEB-680: `revocations` carry field (RevocationAttestation) ----

    /// ZEB-680: mint one self-consistent `RevocationAttestation` — a Master-issued
    /// `RevocationCert` for a device paired with that device's `EnrollmentCert`,
    /// using the same recipe as the DM RevocationPush tests (`dm_inbox_ingest.rs`).
    /// `seed` varies both the master and device keys so a caller can build a list
    /// of distinct pairs.
    fn revocation_attestation(seed: u8) -> RevocationAttestation {
        revocation_attestation_at(seed, 1_700_000_000)
    }

    /// ZEB-701: like [`revocation_attestation`] but with a caller-chosen
    /// `issued_at` on the revocation cert, for recency-selection tests.
    fn revocation_attestation_at(seed: u8, issued_at: u64) -> RevocationAttestation {
        use harmony_owner::certs::{EnrollmentCert, RevocationCert, RevocationReason};
        use harmony_owner::pubkey_bundle::PubKeyBundle;
        let master_sk = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        let master_bundle = PubKeyBundle::classical_only(master_sk.verifying_key().to_bytes());
        let device_sk = ed25519_dalek::SigningKey::from_bytes(&[seed ^ 0x5a; 32]);
        let device_bundle = PubKeyBundle::classical_only(device_sk.verifying_key().to_bytes());
        let device_id = device_bundle.identity_hash();
        let enrollment = EnrollmentCert::sign_master(
            &master_sk,
            master_bundle.clone(),
            device_id,
            device_bundle,
            1_700_000_000,
            None,
        )
        .expect("mint enrollment");
        let revocation = RevocationCert::sign_master(
            &master_sk,
            master_bundle,
            device_id,
            issued_at,
            RevocationReason::Compromised,
        )
        .expect("mint revocation");
        RevocationAttestation {
            revocation,
            enrollment: Box::new(enrollment),
        }
    }

    /// ZEB-680: a minimal well-formed `FriendLinkAccepted` for pure codec tests
    /// (the `sig` need not verify for a round-trip through the CBOR codec).
    fn accepted_base(seed: u8) -> FriendLinkAccepted {
        let owner = mint_test_owner(seed);
        FriendLinkAccepted {
            from_addr: owner.owner,
            display: None,
            eph_x25519_pub: [0x77; 32],
            enrollment: owner.cert,
            signer_certs: Vec::new(),
            sig: [4u8; 64],
            sender_devices: vec![],
            device_identity_pubs: vec![],
            iroh_node_id: [0u8; 32],
            home_relay_url: None,
            pq_dsa_pubkey: vec![],
            pq_kem_pubkey: vec![],
            revocations: Vec::new(),
        }
    }

    #[test]
    fn revocations_field_round_trips() {
        // A request carrying one valid attestation round-trips through the codec.
        let (mut req, _, _) = signed_request(0x21, [9u8; 64]);
        req.revocations = vec![revocation_attestation(0x60)];
        let bytes = encode_friend_request(&req).expect("encode");
        let back = decode_friend_request(&bytes).expect("decode");
        assert_eq!(req, back);
        assert_eq!(back.revocations.len(), 1);
    }

    #[test]
    fn revocations_absent_decodes_empty() {
        // Back-compat proof: an empty list emits NO "v" key (skip_serializing_if),
        // so the frame is byte-identical to a pre-ZEB-680 request; decode → empty.
        let (req, _, _) = signed_request(0x21, [9u8; 64]);
        assert!(req.revocations.is_empty());
        let bytes = encode_friend_request(&req).expect("encode");
        let value: ciborium::value::Value =
            ciborium::de::from_reader(&bytes[..]).expect("decode as generic CBOR");
        let map = value.as_map().expect("request is a CBOR map");
        assert!(
            !map.iter().any(|(k, _)| k.as_text() == Some("v")),
            "empty revocations must omit the \"v\" key"
        );
        let back = decode_friend_request(&bytes).expect("decode");
        assert!(back.revocations.is_empty());
    }

    #[test]
    fn revocations_over_cap_is_decode_error() {
        // 33 attestations (> MAX_CARRIED_REVOCATIONS) → HARD decode error, never
        // truncation. Serialize is pass-through, so the oversized wire builds; the
        // cap fires on the receive side (mirrors `sender_devices`).
        let (mut req, _, _) = signed_request(0x21, [9u8; 64]);
        req.revocations = (0..=MAX_CARRIED_REVOCATIONS as u8)
            .map(revocation_attestation)
            .collect();
        assert!(req.revocations.len() > MAX_CARRIED_REVOCATIONS);
        let bytes = encode_friend_request(&req).expect("encode (serialize is uncapped)");
        let err = decode_friend_request(&bytes);
        assert!(
            matches!(err, Err(FriendHandshakeError::Decode(_))),
            "over-cap revocations must be rejected on decode, got: {err:?}"
        );
    }

    #[test]
    fn revocations_field_round_trips_accepted() {
        let mut acc = accepted_base(0x22);
        acc.revocations = vec![revocation_attestation(0x61)];
        let bytes = encode_friend_accepted(&acc).expect("encode");
        let back = decode_friend_accepted(&bytes).expect("decode");
        assert_eq!(acc, back);
        assert_eq!(back.revocations.len(), 1);
    }

    #[test]
    fn revocations_absent_decodes_empty_accepted() {
        let acc = accepted_base(0x22);
        assert!(acc.revocations.is_empty());
        let bytes = encode_friend_accepted(&acc).expect("encode");
        let value: ciborium::value::Value =
            ciborium::de::from_reader(&bytes[..]).expect("decode as generic CBOR");
        let map = value.as_map().expect("accepted is a CBOR map");
        assert!(
            !map.iter().any(|(k, _)| k.as_text() == Some("v")),
            "empty revocations must omit the \"v\" key"
        );
        let back = decode_friend_accepted(&bytes).expect("decode");
        assert!(back.revocations.is_empty());
    }

    #[test]
    fn revocations_over_cap_is_decode_error_accepted() {
        let mut acc = accepted_base(0x22);
        acc.revocations = (0..=MAX_CARRIED_REVOCATIONS as u8)
            .map(revocation_attestation)
            .collect();
        assert!(acc.revocations.len() > MAX_CARRIED_REVOCATIONS);
        let bytes = encode_friend_accepted(&acc).expect("encode (serialize is uncapped)");
        let err = decode_friend_accepted(&bytes);
        assert!(
            matches!(err, Err(FriendHandshakeError::Decode(_))),
            "over-cap revocations must be rejected on decode, got: {err:?}"
        );
    }

    // ---- ZEB-680 T4: `build_revocation_attestations` (send-side builder) ----

    /// ZEB-680 T4: the builder pairs each Master-issued revocation with its
    /// device enrollment, skips revocations with no enrollment on record, and
    /// caps the result at `MAX_CARRIED_REVOCATIONS`. All attestations here
    /// share one `issued_at` (the helper's fixed timestamp), so Case 2 pins
    /// the ZEB-701 tie-break: at equal recency, keep the smallest-N by
    /// `revocation.target` byte order (deterministic selection + stable wire).
    /// The recency ordering itself is pinned by
    /// `builder_carries_most_recent_over_cap` below.
    #[test]
    fn builder_pairs_and_caps() {
        use harmony_owner::crdt::RevocationSet;

        // Case 1: three Master-issued revocations, one with no enrollment on
        // record → exactly two paired attestations (the unpaired one skipped).
        let a = revocation_attestation(0x01);
        let b = revocation_attestation(0x02);
        let c = revocation_attestation(0x03);
        let mut trust = harmony_owner::state::OwnerState {
            revocations: RevocationSet::from(vec![
                a.revocation.clone(),
                b.revocation.clone(),
                c.revocation.clone(),
            ]),
            ..Default::default()
        };
        trust
            .enrollments
            .insert(a.revocation.target, (*a.enrollment).clone());
        trust
            .enrollments
            .insert(b.revocation.target, (*b.enrollment).clone());
        // c's enrollment is deliberately absent.

        let out = build_revocation_attestations(&trust);
        assert_eq!(out.len(), 2, "the unpaired revocation (c) must be skipped");
        let carried: std::collections::HashSet<[u8; 16]> =
            out.iter().map(|att| att.revocation.target).collect();
        assert!(carried.contains(&a.revocation.target));
        assert!(carried.contains(&b.revocation.target));
        assert!(
            !carried.contains(&c.revocation.target),
            "c has no enrollment on record → skipped"
        );

        // Case 2: 33 Master-issued revocations, all with enrollments and all
        // sharing one `issued_at` → capped at 32 via the ZEB-701 tie-break:
        // equal recency keeps the smallest-N by target byte order, ascending.
        let atts: Vec<RevocationAttestation> = (0u8..=MAX_CARRIED_REVOCATIONS as u8)
            .map(revocation_attestation)
            .collect();
        assert_eq!(atts.len(), MAX_CARRIED_REVOCATIONS + 1);
        let mut trust = harmony_owner::state::OwnerState {
            revocations: RevocationSet::from(
                atts.iter()
                    .map(|att| att.revocation.clone())
                    .collect::<Vec<_>>(),
            ),
            ..Default::default()
        };
        for att in &atts {
            trust
                .enrollments
                .insert(att.revocation.target, (*att.enrollment).clone());
        }

        let out = build_revocation_attestations(&trust);
        assert_eq!(
            out.len(),
            MAX_CARRIED_REVOCATIONS,
            "over-cap set truncated to the cap"
        );
        // Expected: the 32 smallest targets, ascending (targets derive from a
        // device-key hash, so we compute the ordering from the actual set).
        let mut all_targets: Vec<[u8; 16]> = atts.iter().map(|att| att.revocation.target).collect();
        all_targets.sort_unstable();
        let expected: Vec<[u8; 16]> = all_targets[..MAX_CARRIED_REVOCATIONS].to_vec();
        let got: Vec<[u8; 16]> = out.iter().map(|att| att.revocation.target).collect();
        assert_eq!(got, expected, "keep smallest-N by target, sorted ascending");
        assert!(
            !got.contains(all_targets.last().unwrap()),
            "the single largest target is dropped"
        );
    }

    /// ZEB-701: over the cap, the builder carries the MOST-RECENTLY-issued 32
    /// revocations (`issued_at` descending), not the smallest-32 by target
    /// hash — recent revocations are the security-relevant subset for a brand
    /// new friend who has no other propagation channel yet. Determinism at
    /// equal `issued_at` comes from the target tie-break (pinned by
    /// `builder_pairs_and_caps` Case 2).
    #[test]
    fn builder_carries_most_recent_over_cap() {
        use harmony_owner::crdt::RevocationSet;

        // 33 attestations with strictly increasing issued_at: seed i is issued
        // at BASE + i, so seed 0 is the single OLDEST revocation.
        const BASE: u64 = 1_700_000_000;
        let atts: Vec<RevocationAttestation> = (0u8..=MAX_CARRIED_REVOCATIONS as u8)
            .map(|i| revocation_attestation_at(i, BASE + u64::from(i)))
            .collect();
        assert_eq!(atts.len(), MAX_CARRIED_REVOCATIONS + 1);
        let mut trust = harmony_owner::state::OwnerState {
            revocations: RevocationSet::from(
                atts.iter()
                    .map(|att| att.revocation.clone())
                    .collect::<Vec<_>>(),
            ),
            ..Default::default()
        };
        for att in &atts {
            trust
                .enrollments
                .insert(att.revocation.target, (*att.enrollment).clone());
        }

        let out = build_revocation_attestations(&trust);
        assert_eq!(out.len(), MAX_CARRIED_REVOCATIONS);
        // The oldest revocation (seed 0, issued_at == BASE) is the one dropped.
        assert!(
            !out.iter()
                .any(|att| att.revocation.target == atts[0].revocation.target),
            "the single oldest revocation must be the one past the cap"
        );
        // Carried set = seeds 1..=32, ordered most-recent-first (issued_at
        // descending: BASE+32, BASE+31, …, BASE+1).
        let got: Vec<u64> = out.iter().map(|att| att.revocation.issued_at).collect();
        let expected: Vec<u64> = (1..=MAX_CARRIED_REVOCATIONS as u64)
            .rev()
            .map(|i| BASE + i)
            .collect();
        assert_eq!(got, expected, "carried most-recent-first");
    }

    /// ZEB-680 T4: a SelfDevice-issued revocation — even WITH its enrollment on
    /// record — is excluded (only Master-issued are carried; the receive-side
    /// `verify_revocation_push` accepts Master only, so a SelfDevice attestation
    /// would fail-close an honest handshake).
    #[test]
    fn builder_skips_non_master_issued() {
        use harmony_owner::certs::{EnrollmentCert, RevocationCert, RevocationReason};
        use harmony_owner::crdt::RevocationSet;
        use harmony_owner::pubkey_bundle::PubKeyBundle;

        let seed = 0x40u8;
        let master_sk = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        let master_bundle = PubKeyBundle::classical_only(master_sk.verifying_key().to_bytes());
        let owner_id = master_bundle.identity_hash();
        let device_sk = ed25519_dalek::SigningKey::from_bytes(&[seed ^ 0x5a; 32]);
        let device_bundle = PubKeyBundle::classical_only(device_sk.verifying_key().to_bytes());
        let device_id = device_bundle.identity_hash();
        let self_enrollment = EnrollmentCert::sign_master(
            &master_sk,
            master_bundle.clone(),
            device_id,
            device_bundle,
            1_700_000_000,
            None,
        )
        .expect("mint enrollment");
        let self_rev = RevocationCert::sign_self(
            &device_sk,
            owner_id,
            device_id,
            1_700_000_000,
            RevocationReason::Compromised,
        )
        .expect("mint self-issued revocation");

        // A Master-issued attestation that MUST be carried, to prove the filter
        // excludes by issuer (not by dropping everything).
        let master = revocation_attestation(0x41);

        let mut trust = harmony_owner::state::OwnerState {
            revocations: RevocationSet::from(vec![self_rev.clone(), master.revocation.clone()]),
            ..Default::default()
        };
        trust.enrollments.insert(device_id, self_enrollment);
        trust
            .enrollments
            .insert(master.revocation.target, (*master.enrollment).clone());

        let out = build_revocation_attestations(&trust);
        assert_eq!(out.len(), 1, "the SelfDevice-issued revocation is excluded");
        assert_eq!(out[0].revocation.target, master.revocation.target);
        assert!(matches!(
            out[0].revocation.issuer,
            harmony_owner::certs::RevocationIssuer::Master { .. }
        ));
    }

    #[test]
    fn verify_enrolled_device_accepts_valid_cert() {
        let owner = mint_test_owner(0x31);
        let revoked = crate::revoked_device_projection::RevokedDeviceProjection::new();
        let verified =
            verify_enrolled_device(&owner.cert, &[], owner.owner, &revoked, 0).expect("valid");
        assert_eq!(
            verified.device_ed25519,
            owner.cert.device_pubkeys.classical.ed25519_verify
        );
    }

    /// ZEB-680 §1: a cert that PASSES the enrollment chokepoint but whose
    /// enrolled device key is present in the projection for the claimed owner is
    /// rejected with `DeviceRevoked` (the consult runs AFTER chokepoint success).
    #[test]
    fn verify_enrolled_device_rejects_revoked_device() {
        let owner = mint_test_owner(0x36);
        let device_ed = owner.cert.device_pubkeys.classical.ed25519_verify;
        let revoked = crate::revoked_device_projection::RevokedDeviceProjection::new();
        let keys: std::collections::BTreeSet<[u8; 32]> = std::iter::once(device_ed).collect();
        revoked.union_from_members(std::iter::once((owner.owner, &keys)));
        let err = verify_enrolled_device(&owner.cert, &[], owner.owner, &revoked, 0).unwrap_err();
        assert!(
            matches!(err, FriendHandshakeError::DeviceRevoked),
            "expected DeviceRevoked, got {err:?}"
        );
    }

    /// ZEB-680 §1: the same cert with an EMPTY projection verifies unchanged —
    /// the consult only rejects keys it actually knows about.
    #[test]
    fn verify_enrolled_device_passes_with_empty_projection() {
        let owner = mint_test_owner(0x37);
        let revoked = crate::revoked_device_projection::RevokedDeviceProjection::new();
        let verified = verify_enrolled_device(&owner.cert, &[], owner.owner, &revoked, 0)
            .expect("empty projection revokes nothing");
        assert_eq!(
            verified.device_ed25519,
            owner.cert.device_pubkeys.classical.ed25519_verify
        );
    }

    #[test]
    fn verify_enrolled_device_rejects_wrong_owner() {
        let owner = mint_test_owner(0x32);
        let other = mint_test_owner(0x33);
        // Cert is owner's, but we claim it belongs to `other` → owner mismatch.
        match verify_enrolled_device(&owner.cert, &[], other.owner, &no_revocations(), 0) {
            Err(FriendHandshakeError::EnrollmentOwnerMismatch) => {}
            other => panic!("expected EnrollmentOwnerMismatch, got {other:?}"),
        }
    }

    #[test]
    fn verify_enrolled_device_rejects_tampered_cert() {
        let owner = mint_test_owner(0x34);
        let mut cert = owner.cert.clone();
        // Structurally tamper: flip issued_at so the master signature no longer
        // covers the payload → cert.verify() fails.
        cert.issued_at ^= 0xFFFF;
        match verify_enrolled_device(&cert, &[], owner.owner, &no_revocations(), 0) {
            Err(FriendHandshakeError::EnrollmentCertInvalid) => {}
            other => panic!("expected EnrollmentCertInvalid, got {other:?}"),
        }
    }

    #[test]
    fn verify_enrolled_device_rejects_quorum_without_bundle() {
        use harmony_owner::certs::EnrollmentIssuer;
        let owner = mint_test_owner(0x35);
        let mut cert = owner.cert.clone();
        // Swap a Quorum issuer in. A quorum cert presented WITHOUT its
        // signer-cert bundle cannot have its part signatures verified — the
        // no-bundle case must stay closed (ZEB-677: this replaces the old
        // blanket non-Master rejection).
        cert.issuer = EnrollmentIssuer::Quorum {
            signers: vec![[1u8; 16], [2u8; 16]],
            signatures: vec![vec![0u8; 64], vec![0u8; 64]],
        };
        cert.signature = Vec::new();
        match verify_enrolled_device(&cert, &[], owner.owner, &no_revocations(), 0) {
            Err(FriendHandshakeError::EnrollmentCertInvalid) => {}
            other => panic!("expected EnrollmentCertInvalid, got {other:?}"),
        }
    }

    #[test]
    fn verify_enrolled_device_accepts_quorum_with_bundle() {
        use crate::enrollment_verify::quorum_fixtures::{mint_quorum_world, WORLD_NOW};
        // ZEB-677: a Quorum-issued cert presented WITH its Master-issued
        // signer certs verifies, and the master anchor is recovered from the
        // bundle (the quorum cert itself carries no master pubkey).
        let world = mint_quorum_world(0x80);
        let verified = verify_enrolled_device(
            &world.c_quorum_cert,
            &world.bundle,
            crate::owner_state_types::OwnerAddr(world.owner_id),
            &no_revocations(),
            WORLD_NOW,
        )
        .expect("quorum cert with bundle verifies");
        assert_eq!(
            verified.device_ed25519,
            world.c_quorum_cert.device_pubkeys.classical.ed25519_verify
        );
        assert_eq!(verified.master_ed25519, world.master_ed25519);
    }

    /// ZEB-677: the `signer_certs` wire field is additive — an empty bundle
    /// omits the key entirely (old-decoder byte-compat), and a populated
    /// bundle round-trips.
    #[test]
    fn signer_certs_field_roundtrips_and_defaults_empty() {
        use crate::enrollment_verify::quorum_fixtures::mint_quorum_world;
        let token_sig = [7u8; 64];
        let (req, _dk, _eph) = signed_request(0x38, token_sig);
        assert!(req.signer_certs.is_empty());
        let bytes = encode_friend_request(&req).expect("encode");
        let back = decode_friend_request(&bytes).expect("decode");
        assert_eq!(req, back, "empty bundle round-trips (key omitted)");

        let world = mint_quorum_world(0x84);
        let mut with_bundle = req.clone();
        with_bundle.signer_certs = world.bundle.clone();
        let bytes = encode_friend_request(&with_bundle).expect("encode with bundle");
        let back = decode_friend_request(&bytes).expect("decode with bundle");
        assert_eq!(back.signer_certs, world.bundle, "bundle round-trips");
    }

    #[test]
    fn verify_enrolled_device_rejects_expired_cert() {
        use harmony_owner::certs::EnrollmentCert;
        use harmony_owner::pubkey_bundle::{ClassicalKeys, PubKeyBundle};

        // Build a cert with expires_at = Some(2_000), issued_at = 1_000.
        let seed = 0x39u8;
        let master_sk = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        let master_bundle = PubKeyBundle {
            classical: ClassicalKeys {
                ed25519_verify: master_sk.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        let owner_id = master_bundle.identity_hash();
        let device_sk = ed25519_dalek::SigningKey::from_bytes(&[seed ^ 0xFF; 32]);
        let device_bundle = PubKeyBundle {
            classical: ClassicalKeys {
                ed25519_verify: device_sk.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        let device_id = device_bundle.identity_hash();
        let cert = EnrollmentCert::sign_master(
            &master_sk,
            master_bundle,
            device_id,
            device_bundle,
            1_000,
            Some(2_000),
        )
        .expect("sign_master");
        let owner = crate::owner_state_types::OwnerAddr(owner_id);

        // Expired: now_ms = 2_001 > expires_at = 2_000 → EnrollmentCertExpired.
        assert!(
            matches!(
                verify_enrolled_device(&cert, &[], owner, &no_revocations(), 2_001),
                Err(FriendHandshakeError::EnrollmentCertExpired)
            ),
            "a cert past its expires_at must be rejected with EnrollmentCertExpired"
        );
        // At-boundary (now_ms = 2_000, expires_at = 2_000): verify uses >
        // so 2_000 is NOT expired — must succeed.
        assert!(
            verify_enrolled_device(&cert, &[], owner, &no_revocations(), 2_000).is_ok(),
            "a cert at exactly expires_at is NOT expired (> not >=)"
        );
        // Before expiry (now_ms = 1_500): must succeed.
        assert!(
            verify_enrolled_device(&cert, &[], owner, &no_revocations(), 1_500).is_ok(),
            "a cert before expires_at must be accepted"
        );
    }

    #[test]
    fn request_signature_verifies_against_enrolled_key_and_tamper_fails() {
        use ed25519_dalek::{Signature, VerifyingKey};
        let token_sig = [7u8; 64];
        let (req, device_key, _eph_pub) = signed_request(0x36, token_sig);

        // The enrolled device key resolved from the cert must verify the sig
        // over the request preimage.
        let resolved = verify_enrolled_device(
            &req.enrollment,
            &req.signer_certs,
            req.from_addr,
            &no_revocations(),
            0,
        )
        .expect("valid cert")
        .device_ed25519;
        assert_eq!(resolved, device_key);
        let vk = VerifyingKey::from_bytes(&resolved).expect("vk");
        let devices_digest = contact_digest(
            &req.sender_devices,
            &req.device_identity_pubs,
            &req.iroh_node_id,
            req.home_relay_url.as_deref(),
            &req.pq_dsa_pubkey,
            &req.pq_kem_pubkey,
        );
        let preimage = friend_request_sig_preimage(
            req.from_addr,
            req.token_sig.as_ref(),
            &req.eph_x25519_pub,
            &devices_digest,
        );
        vk.verify_strict(&preimage, &Signature::from_bytes(&req.sig))
            .expect("untampered sig verifies");

        // A tampered sig (or a preimage over a different token_sig) must fail.
        let bad_preimage = friend_request_sig_preimage(
            req.from_addr,
            Some(&[0u8; 64]),
            &req.eph_x25519_pub,
            &devices_digest,
        );
        assert!(vk
            .verify_strict(&bad_preimage, &Signature::from_bytes(&req.sig))
            .is_err());
    }

    #[test]
    fn request_preimage_binds_ephemeral_key() {
        let eph = [0x42u8; 32];
        let dig = [0u8; 32];
        let p1 = friend_request_sig_preimage(OwnerAddr([1; 16]), Some(&[9u8; 64]), &eph, &dig);
        let mut eph2 = eph;
        eph2[0] ^= 1;
        let p2 = friend_request_sig_preimage(OwnerAddr([1; 16]), Some(&[9u8; 64]), &eph2, &dig);
        assert_ne!(
            p1, p2,
            "a different ephemeral key must change the signed preimage"
        );
        let p3 = friend_request_sig_preimage(OwnerAddr([1; 16]), None, &eph, &dig);
        assert_ne!(p1, p3, "None vs Some(token) must differ");
        // ZEB-461: a different device-bundle digest must change the preimage.
        let dig2 = [1u8; 32];
        let p4 = friend_request_sig_preimage(OwnerAddr([1; 16]), Some(&[9u8; 64]), &eph, &dig2);
        assert_ne!(
            p1, p4,
            "a different device-bundle digest must change the signed preimage"
        );
    }

    /// ZEB-461/473: the contact digest must change when ANY of its six inputs
    /// change — the device list, the parallel identity pubs, OR (ZEB-473) the iroh
    /// node id / home relay / PQ DSA pub / PQ KEM pub — so a peer can't have its
    /// bundle OR its reachability/PQ keys swapped without invalidating the sig.
    #[test]
    fn contact_digest_binds_all_fields() {
        let dev = [DeviceIdentityHash([1; 16])];
        let base = contact_digest(&dev, &[None], &[0u8; 32], None, &[], &[]);
        // Device list + identity pubs (ZEB-461).
        let d_empty = contact_digest(&[], &[], &[0u8; 32], None, &[], &[]);
        let d_dev2 = contact_digest(
            &[DeviceIdentityHash([2; 16])],
            &[None],
            &[0u8; 32],
            None,
            &[],
            &[],
        );
        let d_pub = contact_digest(&dev, &[Some([9; 64])], &[0u8; 32], None, &[], &[]);
        // Reachability + PQ keys (ZEB-473 — newly signed).
        let d_node = contact_digest(&dev, &[None], &[7u8; 32], None, &[], &[]);
        let d_relay = contact_digest(&dev, &[None], &[0u8; 32], Some("https://r/"), &[], &[]);
        let d_dsa = contact_digest(&dev, &[None], &[0u8; 32], None, &[1, 2, 3], &[]);
        let d_kem = contact_digest(&dev, &[None], &[0u8; 32], None, &[], &[4, 5, 6]);
        assert_ne!(base, d_empty, "empty vs non-empty bundle must differ");
        assert_ne!(
            base, d_dev2,
            "a different device hash must change the digest"
        );
        assert_ne!(
            base, d_pub,
            "a different identity pub must change the digest"
        );
        assert_ne!(
            base, d_node,
            "a different iroh node id must change the digest"
        );
        assert_ne!(
            base, d_relay,
            "a different home relay must change the digest"
        );
        assert_ne!(base, d_dsa, "a different PQ DSA pub must change the digest");
        assert_ne!(base, d_kem, "a different PQ KEM pub must change the digest");
    }

    /// ZEB-473 §6.3 (the headline security guarantee): an active MITM that
    /// rewrites ANY of the four reachability/PQ fields on a signed
    /// `FriendLinkRequest` must break verification — the fields are SIGNED into the
    /// `contact_digest` preimage now, not unsigned routing hints, so a silent PQ
    /// downgrade is detectable. The untampered message verifies; each single-field
    /// tamper fails.
    #[test]
    fn signed_request_reachability_fields_are_tamper_evident() {
        let owner = mint_test_owner(0x73);
        let (_eph_sk, eph_pub) = crate::friend_rendezvous::generate_ephemeral();
        let id = harmony_identity::PrivateIdentity::from_seed(&[0x73; 32]);
        let pub64 = id.public_identity().to_public_bytes();
        let (devices, pubs) = crate::dm_tunnel_contact::self_device_bundle(pub64);
        // Build the SIGNED request over the full six-field digest, with all four
        // reachability/PQ fields populated.
        let iroh_node_id = [0x11; 32];
        let home_relay_url = Some("https://relay.example/".to_string());
        let pq_dsa_pubkey = vec![0xaa; 32];
        let pq_kem_pubkey = vec![0xbb; 16];
        let digest = contact_digest(
            &devices,
            &pubs,
            &iroh_node_id,
            home_relay_url.as_deref(),
            &pq_dsa_pubkey,
            &pq_kem_pubkey,
        );
        let preimage = friend_request_sig_preimage(owner.owner, None, &eph_pub, &digest);
        let sig = owner.device_key.sign(&preimage).to_bytes();
        let req = FriendLinkRequest {
            from_addr: owner.owner,
            display: None,
            token_sig: None,
            eph_x25519_pub: eph_pub,
            enrollment: owner.cert,
            signer_certs: Vec::new(),
            sig,
            sender_devices: devices,
            device_identity_pubs: pubs,
            iroh_node_id,
            home_relay_url,
            pq_dsa_pubkey,
            pq_kem_pubkey,
            revocations: Vec::new(),
        };
        // Untampered: BOTH verify paths accept.
        authenticate_friend_request(&req, &no_revocations(), 0)
            .expect("untampered request authenticates");

        // Tamper iroh_node_id.
        let mut t = req.clone();
        t.iroh_node_id[0] ^= 0xFF;
        assert!(
            matches!(
                authenticate_friend_request(&t, &no_revocations(), 0),
                Err(FriendHandshakeError::SignatureInvalid)
            ),
            "tampering iroh_node_id must fail verification"
        );
        // Tamper home_relay_url.
        let mut t = req.clone();
        t.home_relay_url = Some("https://evil.example/".to_string());
        assert!(
            matches!(
                authenticate_friend_request(&t, &no_revocations(), 0),
                Err(FriendHandshakeError::SignatureInvalid)
            ),
            "tampering home_relay_url must fail verification"
        );
        // Tamper pq_dsa_pubkey (the PQ-downgrade vector §6.3).
        let mut t = req.clone();
        t.pq_dsa_pubkey[0] ^= 0xFF;
        assert!(
            matches!(
                authenticate_friend_request(&t, &no_revocations(), 0),
                Err(FriendHandshakeError::SignatureInvalid)
            ),
            "tampering pq_dsa_pubkey must fail verification"
        );
        // Tamper pq_kem_pubkey.
        let mut t = req.clone();
        t.pq_kem_pubkey[0] ^= 0xFF;
        assert!(
            matches!(
                authenticate_friend_request(&t, &no_revocations(), 0),
                Err(FriendHandshakeError::SignatureInvalid)
            ),
            "tampering pq_kem_pubkey must fail verification"
        );
    }

    /// ZEB-473 §6.3, accept side: the same tamper-evidence on a signed
    /// `FriendLinkAccepted`. A `process_friend_request`-produced accept carries
    /// real signed reachability; verifying it via the accept preimage must fail if
    /// any reachability/PQ field is rewritten in flight.
    #[test]
    fn signed_accept_reachability_fields_are_tamper_evident() {
        use ed25519_dalek::{Signature, VerifyingKey};
        // Self-side material that ends up SIGNED into the accept.
        let me = mint_test_owner(0x74);
        let id = harmony_identity::PrivateIdentity::from_seed(&[0x74; 32]);
        let pub64 = id.public_identity().to_public_bytes();
        let statics = SelfHandshakeStatics {
            identity_pub_64: pub64,
            iroh_node_id: [0x22; 32],
            pq_dsa_pubkey: vec![0xcc; 32],
            pq_kem_pubkey: vec![0xdd; 16],
        };
        let req = signed_request_no_token(0x75);
        let kt = crate::owner_state_crypto::KeyTree::derive(&[9u8; 32]).expect("kt");
        let mut state = OwnerState::default();
        let (accepted, _revocations_inserted) = process_friend_request(
            &mut state,
            test_hlc(1),
            &req,
            me.owner,
            None,
            &me.cert,
            &me.device_key,
            &kt,
            &no_revocations(),
            0,
            Some(&statics),
            // ZEB-621: the relay is supplied fresh at accept-sign time (the SOLE
            // source), and — like the statics — is folded into the signed digest.
            Some("https://relay.example/accept".to_string()),
            None,
            vec![],
        )
        .expect("processed");
        // The accept must actually carry the signed reachability (not empty).
        assert_eq!(accepted.iroh_node_id, [0x22; 32]);
        assert!(!accepted.pq_dsa_pubkey.is_empty());

        // Closure re-runs the accept-verify path (same six-field digest the dialer
        // computes) over a possibly-tampered accept.
        let verify = |acc: &FriendLinkAccepted| -> bool {
            let device_key = verify_enrolled_device(
                &acc.enrollment,
                &acc.signer_certs,
                acc.from_addr,
                &no_revocations(),
                0,
            )
            .expect("self cert verifies")
            .device_ed25519;
            let vk = VerifyingKey::from_bytes(&device_key).expect("vk");
            let digest = contact_digest(
                &acc.sender_devices,
                &acc.device_identity_pubs,
                &acc.iroh_node_id,
                acc.home_relay_url.as_deref(),
                &acc.pq_dsa_pubkey,
                &acc.pq_kem_pubkey,
            );
            let preimage =
                friend_accept_sig_preimage(acc.from_addr, None, &acc.eph_x25519_pub, &digest);
            vk.verify_strict(&preimage, &Signature::from_bytes(&acc.sig))
                .is_ok()
        };
        assert!(verify(&accepted), "untampered accept verifies");

        let mut t = accepted.clone();
        t.iroh_node_id[0] ^= 0xFF;
        assert!(!verify(&t), "tampering accept iroh_node_id must fail");
        let mut t = accepted.clone();
        t.home_relay_url = Some("https://evil.example/".to_string());
        assert!(!verify(&t), "tampering accept home_relay_url must fail");
        let mut t = accepted.clone();
        t.pq_dsa_pubkey[0] ^= 0xFF;
        assert!(!verify(&t), "tampering accept pq_dsa_pubkey must fail");
        let mut t = accepted.clone();
        t.pq_kem_pubkey[0] ^= 0xFF;
        assert!(!verify(&t), "tampering accept pq_kem_pubkey must fail");
    }

    #[test]
    fn verified_master_anchor_matches_owner_id() {
        let owner = mint_test_owner(0x37);
        let master = verify_enrolled_device(&owner.cert, &[], owner.owner, &no_revocations(), 0)
            .expect("master cert")
            .master_ed25519;
        // The friend-graph key invariant: owner_id derived from this master key
        // equals the cert's owner_id. (Pinned for the butler/relay CoMember
        // admission checks, which re-derive owner ids from the anchor.)
        assert_eq!(
            crate::friend_graph::owner_id_from_master_ed25519(&master),
            owner.owner
        );
    }

    fn test_hlc(w: u64) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: "self".into(),
        }
    }

    #[test]
    fn process_friend_request_adds_active_token_friend_and_returns_verifiable_accept() {
        use crate::owner_state_crypto::KeyTree;
        use ed25519_dalek::{Signature, VerifyingKey};
        let me = mint_test_owner(0x60); // the acceptor (self)
        let token_sig = [0x5a; 64];
        let (req, _requester_device, _req_eph) = signed_request(0x61, token_sig);

        let kt = KeyTree::derive(&[9u8; 32]).expect("kt");
        let mut state = OwnerState::default();
        let (accepted, _revocations_inserted) = process_friend_request(
            &mut state,
            test_hlc(1_000),
            &req,
            me.owner,
            Some("me".into()),
            &me.cert,
            &me.device_key,
            &kt,
            &no_revocations(),
            0,
            None,
            None,
            None,
            vec![],
        )
        .expect("valid request processed");

        // The requester is now an Active/Token friend keyed on req.from_addr,
        // anchored to the requester's master key.
        let entry = state
            .friend_graph
            .friends
            .get(&req.from_addr)
            .expect("friend inserted");
        assert_eq!(entry.status, FriendStatus::Active);
        assert_eq!(entry.established_via, FriendOrigin::Token);
        assert!(!entry.referrable);
        assert_eq!(entry.display.as_deref(), Some("alice"));
        assert_eq!(
            crate::friend_graph::owner_id_from_master_ed25519(&entry.master_ed25519),
            req.from_addr
        );

        // The returned accept is from self and signed by self's device-#2 key
        // over the accept preimage (same token_sig, accept domain tag).
        assert_eq!(accepted.from_addr, me.owner);
        assert_eq!(accepted.display.as_deref(), Some("me"));
        let self_device_key = verify_enrolled_device(
            &accepted.enrollment,
            &accepted.signer_certs,
            accepted.from_addr,
            &no_revocations(),
            0,
        )
        .expect("self cert verifies")
        .device_ed25519;
        let vk = VerifyingKey::from_bytes(&self_device_key).expect("vk");
        // The accept binds the accepter's own (randomly-generated) ephemeral key
        // + (ZEB-461) its device-bundle digest, both read back off the accept.
        let accept_digest = contact_digest(
            &accepted.sender_devices,
            &accepted.device_identity_pubs,
            &accepted.iroh_node_id,
            accepted.home_relay_url.as_deref(),
            &accepted.pq_dsa_pubkey,
            &accepted.pq_kem_pubkey,
        );
        let accept_preimage = friend_accept_sig_preimage(
            accepted.from_addr,
            Some(&token_sig),
            &accepted.eph_x25519_pub,
            &accept_digest,
        );
        vk.verify_strict(&accept_preimage, &Signature::from_bytes(&accepted.sig))
            .expect("accept sig verifies against self enrolled device key");
    }

    #[test]
    fn process_friend_request_derives_secret_matching_requester() {
        use crate::friend_rendezvous::{derive_friendship_secret, generate_ephemeral};
        use crate::owner_state_crypto::{decrypt_friend_secret, KeyTree};

        let me = mint_test_owner(0x60); // acceptor (self)
        let requester = mint_test_owner(0x61);
        let token_sig = [0x5a; 64];
        let (req_eph_sk, req_eph_pub) = generate_ephemeral();
        let req_devices_digest = contact_digest(&[], &[], &[0u8; 32], None, &[], &[]);
        let preimage = friend_request_sig_preimage(
            requester.owner,
            Some(&token_sig),
            &req_eph_pub,
            &req_devices_digest,
        );
        let sig = requester.device_key.sign(&preimage).to_bytes();
        let req = FriendLinkRequest {
            from_addr: requester.owner,
            display: None,
            token_sig: Some(token_sig),
            eph_x25519_pub: req_eph_pub,
            enrollment: requester.cert,
            signer_certs: Vec::new(),
            sig,
            sender_devices: vec![],
            device_identity_pubs: vec![],
            iroh_node_id: [0u8; 32],
            home_relay_url: None,
            pq_dsa_pubkey: vec![],
            pq_kem_pubkey: vec![],
            revocations: Vec::new(),
        };

        let kt = KeyTree::derive(&[9u8; 32]).expect("kt");
        let mut state = OwnerState::default();
        let (accepted, _revocations_inserted) = process_friend_request(
            &mut state,
            test_hlc(1000),
            &req,
            me.owner,
            Some("me".into()),
            &me.cert,
            &me.device_key,
            &kt,
            &no_revocations(),
            0,
            None,
            None,
            None,
            vec![],
        )
        .expect("processed");

        // The requester derives the secret from the accept's ephemeral; it must
        // equal what the acceptor sealed into the FriendEntry.
        let requester_secret = derive_friendship_secret(
            req_eph_sk,
            &accepted.eph_x25519_pub,
            requester.owner,
            me.owner,
        );
        let entry = state
            .friend_graph
            .friends
            .get(&requester.owner)
            .expect("friend");
        let sealed = entry.sealed_secret.as_ref().expect("secret stored");
        let opened = decrypt_friend_secret(&kt, &requester.owner.0, sealed).expect("open");
        assert_eq!(opened.as_ref(), requester_secret.as_ref());
    }

    #[test]
    fn process_friend_request_rejects_bad_signature_and_writes_nothing() {
        let me = mint_test_owner(0x62);
        let (mut req, _, _) = signed_request(0x63, [0x11; 64]);
        // Corrupt the request signature.
        req.sig[0] ^= 0xFF;

        let kt = crate::owner_state_crypto::KeyTree::derive(&[9u8; 32]).expect("kt");
        let mut state = OwnerState::default();
        let err = process_friend_request(
            &mut state,
            test_hlc(1),
            &req,
            me.owner,
            None,
            &me.cert,
            &me.device_key,
            &kt,
            &no_revocations(),
            0,
            None,
            None,
            None,
            vec![],
        )
        .expect_err("bad sig rejected");
        assert!(matches!(err, FriendHandshakeError::SignatureInvalid));
        assert!(
            state.friend_graph.friends.is_empty(),
            "a rejected request must not write a friend entry"
        );
    }

    #[test]
    fn process_friend_request_rejects_wrong_owner_cert_and_writes_nothing() {
        let me = mint_test_owner(0x64);
        // Build a request whose from_addr does NOT match its embedded cert's
        // owner_id (cert/owner mismatch) — verify_enrolled_device must reject.
        let requester = mint_test_owner(0x65);
        let imposter = mint_test_owner(0x66);
        let token_sig = [0x22; 64];
        let (_eph_sk, eph_pub) = crate::friend_rendezvous::generate_ephemeral();
        // Sign with the imposter's owner addr in the preimage so the request is
        // internally consistent except for the cert↔from_addr binding.
        let devices_digest = contact_digest(&[], &[], &[0u8; 32], None, &[], &[]);
        let preimage = friend_request_sig_preimage(
            imposter.owner,
            Some(&token_sig),
            &eph_pub,
            &devices_digest,
        );
        let sig = imposter.device_key.sign(&preimage).to_bytes();
        let req = FriendLinkRequest {
            from_addr: imposter.owner, // claims to be imposter…
            display: None,
            token_sig: Some(token_sig),
            eph_x25519_pub: eph_pub,
            enrollment: requester.cert, // …but presents requester's cert
            signer_certs: Vec::new(),
            sig,
            sender_devices: vec![],
            device_identity_pubs: vec![],
            iroh_node_id: [0u8; 32],
            home_relay_url: None,
            pq_dsa_pubkey: vec![],
            pq_kem_pubkey: vec![],
            revocations: Vec::new(),
        };

        let kt = crate::owner_state_crypto::KeyTree::derive(&[9u8; 32]).expect("kt");
        let mut state = OwnerState::default();
        let err = process_friend_request(
            &mut state,
            test_hlc(1),
            &req,
            me.owner,
            None,
            &me.cert,
            &me.device_key,
            &kt,
            &no_revocations(),
            0,
            None,
            None,
            None,
            vec![],
        )
        .expect_err("owner-mismatched cert rejected");
        assert!(matches!(err, FriendHandshakeError::EnrollmentOwnerMismatch));
        assert!(
            state.friend_graph.friends.is_empty(),
            "a rejected request must not write a friend entry"
        );
    }

    // ── ZEB-370 consent gate: token_gate_open ────────────────────────────

    use crate::pkarr_invite_publisher::PkarrInvitePublisher;
    use harmony_pkarr::testing::MockPkarrRelay;
    use harmony_pkarr::{PkarrPublisher, RelayClient, RelayPool};

    /// Build a friend acceptor wired to `publisher` (may be `None`). All the
    /// crypto/CRDT deps are stub-defaultable since the gate tests never reach
    /// `process_friend_request`.
    fn acceptor_with_publisher(
        publisher: Option<Arc<PkarrInvitePublisher>>,
    ) -> IrohFriendHandshakeAcceptor<()> {
        let me = mint_test_owner(0x70);
        IrohFriendHandshakeAcceptor::<()>::new(
            Arc::new(TokioMutex::new(OwnerState::default())),
            Arc::new(TokioMutex::new(harmony_crdt_sync::ReplayTracker::new(
                "gate-test-dev".to_string(),
            ))),
            "gate-test-dev".to_string(),
            me.owner,
            Some("me".to_string()),
            me.cert,
            Arc::new(ed25519_dalek::SigningKey::from_bytes(
                &me.device_key.to_bytes(),
            )),
            Arc::new(crate::owner_state_crypto::KeyTree::derive(&[7u8; 32]).expect("kt")),
            None,
            publisher,
        )
    }

    // ── ZEB-521/621: accept-time home-relay is FRESH-READ only ───────────

    fn sample_statics() -> SelfHandshakeStatics {
        SelfHandshakeStatics {
            identity_pub_64: [0x21; 64],
            iroh_node_id: [0x22; 32],
            pq_dsa_pubkey: vec![0xaa; 4],
            pq_kem_pubkey: vec![0xbb; 4],
        }
    }

    /// ZEB-621: the acceptor's fresh-relay seam reads the live closure (or `None`
    /// when no closure is wired — the test/iroh-unbound default). This cheap
    /// `Option<String>` is what gets threaded into `process_friend_request`.
    #[test]
    fn acceptor_current_fresh_home_relay_reads_live_closure() {
        let wired =
            acceptor_with_publisher(None).with_self_home_relay_refresh(Some(Arc::new(|| {
                Some("https://relay.live/".to_string())
            })));
        assert_eq!(
            wired.current_fresh_home_relay().as_deref(),
            Some("https://relay.live/"),
        );
        let bare = acceptor_with_publisher(None);
        assert!(bare.current_fresh_home_relay().is_none());
    }

    /// ZEB-621 (the regression guard): the acceptor holds NO frozen relay — the
    /// `home_relay_url` passed at accept-sign time (read fresh from the live
    /// endpoint) is the SOLE source. A fresh read of `Some("https://relay.live/")`
    /// must be advertised verbatim in the signed accept, alongside the immutable
    /// identity/node/PQ statics. This is the exact condition that left peers with
    /// relay-less `DeviceTunnelContact`s in the ZEB-504 capture (a frozen `None`).
    #[test]
    fn process_friend_request_advertises_fresh_relay() {
        let me = mint_test_owner(0x78);
        let req = signed_request_no_token(0x79);
        let kt = crate::owner_state_crypto::KeyTree::derive(&[9u8; 32]).expect("kt");
        let mut state = OwnerState::default();
        let (accepted, _revocations_inserted) = process_friend_request(
            &mut state,
            test_hlc(1),
            &req,
            me.owner,
            None,
            &me.cert,
            &me.device_key,
            &kt,
            &no_revocations(),
            0,
            Some(&sample_statics()),
            Some("https://relay.live/".to_string()),
            None,
            vec![],
        )
        .expect("processed");
        assert_eq!(
            accepted.home_relay_url.as_deref(),
            Some("https://relay.live/"),
            "the signed accept must carry the fresh live relay",
        );
        // The immutable statics still round-trip into the signed accept. (The
        // fabricated `identity_pub_64` isn't a parseable identity key, so the
        // device bundle degrades to empty — the identity→device round-trip is
        // covered by `signed_accept_reachability_fields_are_tamper_evident`.)
        assert_eq!(accepted.iroh_node_id, [0x22; 32]);
        assert_eq!(accepted.pq_dsa_pubkey, vec![0xaa; 4]);
        assert_eq!(accepted.pq_kem_pubkey, vec![0xbb; 4]);
    }

    /// ZEB-621: with no fresh relay resolved (the iroh-unbound / not-yet-resolved
    /// case), the signed accept carries `home_relay_url = None` — there is NO stale
    /// snapshot to fall back to. The statics still round-trip.
    #[test]
    fn process_friend_request_advertises_no_relay_when_fresh_unresolved() {
        let me = mint_test_owner(0x7a);
        let req = signed_request_no_token(0x7b);
        let kt = crate::owner_state_crypto::KeyTree::derive(&[9u8; 32]).expect("kt");
        let mut state = OwnerState::default();
        let (accepted, _revocations_inserted) = process_friend_request(
            &mut state,
            test_hlc(1),
            &req,
            me.owner,
            None,
            &me.cert,
            &me.device_key,
            &kt,
            &no_revocations(),
            0,
            Some(&sample_statics()),
            None,
            None,
            vec![],
        )
        .expect("processed");
        assert!(
            accepted.home_relay_url.is_none(),
            "no fresh relay → accept carries None (no frozen-snapshot fallback)",
        );
        // The immutable statics still round-trip even with an unresolved relay.
        assert_eq!(accepted.iroh_node_id, [0x22; 32]);
        assert_eq!(accepted.pq_dsa_pubkey, vec![0xaa; 4]);
        assert_eq!(accepted.pq_kem_pubkey, vec![0xbb; 4]);
    }

    /// A real `PkarrInvitePublisher` backed by a mock relay (no records actually
    /// resolved — we only exercise the in-memory live-token map).
    async fn test_publisher() -> Arc<PkarrInvitePublisher> {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;
        let relay = MockPkarrRelay::start().await;
        // Keep the relay alive for the lifetime of the publisher by leaking it —
        // these unit tests are short-lived and never touch the network path.
        Box::leak(Box::new(relay));
        let pool = RelayPool::new(vec!["http://127.0.0.1:1/".to_string()]);
        let client = Arc::new(RelayClient::new(pool));
        let publisher = Arc::new(PkarrPublisher::new(client));
        let _ph = Arc::clone(&publisher).spawn();
        let sk = SigningKey::generate(&mut OsRng);
        let mut id_pub = [0u8; 64];
        id_pub[32..].copy_from_slice(&sk.verifying_key().to_bytes());
        Arc::new(PkarrInvitePublisher::new(
            publisher,
            sk,
            id_pub,
            Arc::new(|| b"routing".to_vec()),
        ))
    }

    #[tokio::test]
    async fn token_gate_rejects_when_no_publisher() {
        // No publisher wired in → cannot prove the token was minted here → REJECT
        // (fail-closed). This is the consent-bypass closure: a peer with a
        // structurally-valid request but no proof of a minted token is refused.
        let acceptor = acceptor_with_publisher(None);
        let err = acceptor
            .token_gate_open(&[0x11; 64])
            .await
            .expect_err("no publisher must fail closed");
        assert!(matches!(err, FriendAcceptError::TokenNotLive { .. }));
    }

    #[tokio::test]
    async fn token_gate_rejects_never_registered_token() {
        // FIX 1 (consent bypass): a structurally-valid request whose token_sig
        // was NEVER registered (this node never minted it) is rejected — the gate
        // returns early, so process_friend_request is never reached, no friend is
        // written, and no accept is sent.
        let publisher = test_publisher().await;
        let acceptor = acceptor_with_publisher(Some(Arc::clone(&publisher)));
        let unknown = [0x22; 64];
        // Sanity: the publisher agrees this token is not active.
        assert!(!publisher.is_friend_token_active(&unknown, 1_000).await);
        let err = acceptor
            .token_gate_open(&unknown)
            .await
            .expect_err("never-registered token_sig must be rejected");
        assert!(matches!(err, FriendAcceptError::TokenNotLive { .. }));
    }

    #[tokio::test]
    async fn token_gate_accepts_live_registered_token() {
        let publisher = test_publisher().await;
        let token_sig = [0x33; 64];
        publisher.register_friend_token(&token_sig, None).await;
        let acceptor = acceptor_with_publisher(Some(Arc::clone(&publisher)));
        acceptor
            .token_gate_open(&token_sig)
            .await
            .expect("a live, registered, non-expiring token must pass the gate");
    }

    #[tokio::test]
    async fn token_gate_consumes_on_pass_so_replay_is_rejected() {
        // FIX (ZEB-370 cursor review — TOCTOU): the gate now CONSUMES the token on
        // a successful pass (atomic check-and-consume). A second `token_gate_open`
        // for the same token_sig must therefore fail closed — enforcing the
        // one-shot at the gate, before two concurrent handshakes could both be
        // admitted.
        let publisher = test_publisher().await;
        let token_sig = [0x55; 64];
        publisher.register_friend_token(&token_sig, None).await;
        let acceptor = acceptor_with_publisher(Some(Arc::clone(&publisher)));

        // First pass consumes.
        acceptor
            .token_gate_open(&token_sig)
            .await
            .expect("the first gate pass for a live token must succeed (and consume)");

        // The token is now gone from the live map.
        assert!(
            !publisher.is_friend_token_active(&token_sig, 1_000).await,
            "passing the gate must consume the token from the live map"
        );

        // Second pass for the same token is rejected (one-shot enforced at gate).
        let err = acceptor
            .token_gate_open(&token_sig)
            .await
            .expect_err("a second gate pass for a consumed token must fail closed");
        assert!(matches!(err, FriendAcceptError::TokenNotLive { .. }));
    }

    #[tokio::test]
    async fn token_gate_rejects_expired_registered_token() {
        // FIX 1/2 (expiry): a token registered with an expiry already in the past
        // must be rejected by the gate even though it IS in the live map.
        let publisher = test_publisher().await;
        let token_sig = [0x44; 64];
        // expires_at = 1 ms after epoch → expired against any realistic wall clock.
        publisher.register_friend_token(&token_sig, Some(1)).await;
        // Confirm the underlying check sees it as expired at a current-ish clock.
        let now_ms = wall_now_ms();
        assert!(now_ms > 1, "wall clock should be well past epoch+1ms");
        assert!(!publisher.is_friend_token_active(&token_sig, now_ms).await);
        let acceptor = acceptor_with_publisher(Some(Arc::clone(&publisher)));
        let err = acceptor
            .token_gate_open(&token_sig)
            .await
            .expect_err("expired token must be rejected by the gate");
        assert!(matches!(err, FriendAcceptError::TokenNotLive { .. }));
    }

    // ── ZEB-370 (cursor review): owner-state sync trigger on friend write ──

    /// A successful inbound friend write must arm the owner-state SyncEngine so
    /// the new friend reaches the user's other devices AND persists on clean
    /// shutdown. Here we wire a REAL `SyncEngine` (short debounce) sharing the
    /// acceptor's `crdt_state`, invoke the same `notify_owner_state_dirty()` the
    /// inbound handler calls on success, and assert a publish lands — proving the
    /// acceptor's `owner_sync_engine` field is invoked end-to-end. Mirrors
    /// owner_state_sync.rs's `single_notify_dirty_fires_one_publish`.
    #[tokio::test]
    async fn friend_write_arms_owner_state_sync_engine() {
        use crate::content_store::InMemoryStub;
        use crate::owner_state_crypto::KeyTree;
        use crate::owner_state_sync::{PersistPaths, SyncEngine};
        use std::time::Duration;
        use tokio::sync::mpsc;

        let shared_state = Arc::new(TokioMutex::new(OwnerState::default()));
        let (pub_tx, mut pub_rx) = mpsc::channel(16);
        let (_sub_tx, sub_rx) = mpsc::channel(16);
        let dir = tempfile::tempdir().unwrap();
        let engine = Arc::new(SyncEngine::new(
            crate::owner_state_crypto::FleetKeySet::new(Arc::new(
                KeyTree::derive(&[7u8; 32]).expect("kt"),
            )),
            "acceptor-test-dev".into(),
            Arc::clone(&shared_state),
            Arc::new(TokioMutex::new(harmony_crdt_sync::ReplayTracker::new(
                "acceptor-test-dev".into(),
            ))),
            Arc::new(InMemoryStub::default()),
            pub_tx,
            sub_rx,
            PersistPaths {
                crdt: dir.path().join("crdt.cbor"),
                replay: dir.path().join("replay.cbor"),
            },
            50, // short debounce for the test
        ));

        let me = mint_test_owner(0x71);
        let acceptor = IrohFriendHandshakeAcceptor::<()>::new(
            Arc::clone(&shared_state),
            Arc::new(TokioMutex::new(harmony_crdt_sync::ReplayTracker::new(
                "acceptor-test-dev".into(),
            ))),
            "acceptor-test-dev".to_string(),
            me.owner,
            None,
            me.cert,
            Arc::new(ed25519_dalek::SigningKey::from_bytes(
                &me.device_key.to_bytes(),
            )),
            Arc::new(KeyTree::derive(&[7u8; 32]).expect("kt")),
            None,
            None,
        )
        .with_owner_sync_engine(Some(Arc::clone(&engine)));

        // The success-path call the inbound handler makes after a friend write.
        acceptor.notify_owner_state_dirty();

        let bytes = tokio::time::timeout(Duration::from_millis(500), pub_rx.recv())
            .await
            .expect("a friend write must arm the owner-state publish within the debounce")
            .expect("publish channel not closed");
        assert!(!bytes.is_empty(), "publish payload should be non-empty");
        let _ = engine.shutdown().await;
    }

    /// Without an engine wired in, `notify_owner_state_dirty()` is a safe no-op
    /// (the friend write still succeeds locally). Guards the `Option` contract.
    #[tokio::test]
    async fn friend_write_without_engine_is_noop() {
        let acceptor = acceptor_with_publisher(None);
        // Must not panic; nothing to assert beyond "does not blow up".
        acceptor.notify_owner_state_dirty();
    }

    // ── Task 9: ALPN dispatch multiplexer ────────────────────────────────

    use crate::iroh_endpoint::alpn;
    use crate::iroh_invite_acceptor::IrohHandshakeDispatcher;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Stub dispatcher that records whether it was handed a connection. We can't
    /// construct a real `iroh::endpoint::Connection` in-process, so the routing
    /// tests assert via `select_for_alpn` (pointer identity) rather than driving
    /// `handle_connection`; this stub is here to satisfy the `Arc<dyn …>` bound
    /// the multiplexer holds and to prove the trait object is constructible.
    struct RecordingDispatcher {
        called: AtomicBool,
    }

    #[async_trait]
    impl IrohHandshakeDispatcher for RecordingDispatcher {
        async fn handle_connection(&self, _conn: Connection) {
            self.called.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn route_handshake_alpn_maps_friend_and_invite() {
        assert_eq!(
            route_handshake_alpn(alpn::HARMONY_FRIEND_V1),
            FriendDispatchTarget::Friend,
            "harmony/friend/v1 must route to the friend acceptor"
        );
        assert_eq!(
            route_handshake_alpn(alpn::HARMONY_HANDSHAKE_V1),
            FriendDispatchTarget::Invite,
            "harmony/handshake/v1 must route to the invite acceptor"
        );
        // Defensive: any other ALPN the accept loop might forward defaults to
        // the invite acceptor (the loop only ever forwards the two handshake
        // ALPNs, so this is the safe catch-all).
        assert_eq!(
            route_handshake_alpn(alpn::HARMONY_ZENOH_V1),
            FriendDispatchTarget::Invite
        );
    }

    #[test]
    fn route_pex_alpn_targets_pex() {
        use crate::iroh_endpoint::alpn;
        assert_eq!(
            route_handshake_alpn(alpn::HARMONY_FRIEND_PEX_V1),
            FriendDispatchTarget::Pex
        );
        assert_eq!(
            route_handshake_alpn(alpn::HARMONY_FRIEND_V1),
            FriendDispatchTarget::Friend
        );
        assert_eq!(
            route_handshake_alpn(alpn::HARMONY_HANDSHAKE_V1),
            FriendDispatchTarget::Invite
        );
    }

    #[test]
    fn multiplexer_selects_friend_stub_for_friend_alpn_and_invite_stub_otherwise() {
        let invite: Arc<dyn IrohHandshakeDispatcher> = Arc::new(RecordingDispatcher {
            called: AtomicBool::new(false),
        });
        let friend: Arc<dyn IrohHandshakeDispatcher> = Arc::new(RecordingDispatcher {
            called: AtomicBool::new(false),
        });
        let pex: Arc<dyn IrohHandshakeDispatcher> = Arc::new(RecordingDispatcher {
            called: AtomicBool::new(false),
        });
        let mux = MultiplexHandshakeDispatcher::new(
            Arc::clone(&invite),
            Arc::clone(&friend),
            Arc::clone(&pex),
        );

        // A connection reporting `harmony/friend/v1` selects the friend stub…
        assert!(
            Arc::ptr_eq(mux.select_for_alpn(alpn::HARMONY_FRIEND_V1), &friend),
            "friend ALPN must select the friend acceptor"
        );
        // …and `harmony/handshake/v1` selects the invite stub.
        assert!(
            Arc::ptr_eq(mux.select_for_alpn(alpn::HARMONY_HANDSHAKE_V1), &invite),
            "handshake ALPN must select the invite acceptor"
        );
        // …and `harmony/friend-pex/v1` selects the PEX stub.
        assert!(
            Arc::ptr_eq(mux.select_for_alpn(alpn::HARMONY_FRIEND_PEX_V1), &pex),
            "friend-pex ALPN must select the PEX acceptor"
        );
    }

    // ── ZEB-371 Task 12: consent decision tree ──────────────────────────

    /// Build a signed no-token (Path A) request from a test owner.
    fn signed_request_no_token(owner_seed: u8) -> FriendLinkRequest {
        let owner = mint_test_owner(owner_seed);
        let (_eph_sk, eph_pub) = crate::friend_rendezvous::generate_ephemeral();
        let devices_digest = contact_digest(&[], &[], &[0u8; 32], None, &[], &[]);
        let preimage = friend_request_sig_preimage(owner.owner, None, &eph_pub, &devices_digest);
        let sig = owner.device_key.sign(&preimage).to_bytes();
        FriendLinkRequest {
            from_addr: owner.owner,
            display: Some("carol".into()),
            token_sig: None,
            eph_x25519_pub: eph_pub,
            enrollment: owner.cert,
            signer_certs: Vec::new(),
            sig,
            sender_devices: vec![],
            device_identity_pubs: vec![],
            iroh_node_id: [0u8; 32],
            home_relay_url: None,
            pq_dsa_pubkey: vec![],
            pq_kem_pubkey: vec![],
            revocations: Vec::new(),
        }
    }

    /// ZEB-461 Task 7: a signed, well-formed request carrying a VALID,
    /// self-consistent device bundle (each `Some(pub)` hashes to its parallel
    /// device hash, so `apply_owner_device_update` accepts it). The bundle is
    /// bound into `devices_digest`, which the request signature covers — so the
    /// digest must be computed over the REAL bundle before signing. Returns the
    /// request and the device hashes it advertises (the cache should learn them).
    fn signed_request_with_devices(owner_seed: u8) -> (FriendLinkRequest, Vec<DeviceIdentityHash>) {
        let owner = mint_test_owner(owner_seed);
        let (_eph_sk, eph_pub) = crate::friend_rendezvous::generate_ephemeral();
        // A self-consistent device bundle (hash derived from a real identity pub).
        let id = harmony_identity::PrivateIdentity::from_seed(&[owner_seed; 32]);
        let pub64 = id.public_identity().to_public_bytes();
        let (devices, pubs) = crate::dm_tunnel_contact::self_device_bundle(pub64);
        let devices_digest = contact_digest(&devices, &pubs, &[0u8; 32], None, &[], &[]);
        let preimage = friend_request_sig_preimage(owner.owner, None, &eph_pub, &devices_digest);
        let sig = owner.device_key.sign(&preimage).to_bytes();
        let req = FriendLinkRequest {
            from_addr: owner.owner,
            display: Some("carol".into()),
            token_sig: None,
            eph_x25519_pub: eph_pub,
            enrollment: owner.cert,
            signer_certs: Vec::new(),
            sig,
            sender_devices: devices.clone(),
            device_identity_pubs: pubs,
            iroh_node_id: [0u8; 32],
            home_relay_url: None,
            pq_dsa_pubkey: vec![],
            pq_kem_pubkey: vec![],
            revocations: Vec::new(),
        };
        (req, devices)
    }

    #[test]
    fn process_friend_request_populates_owner_device_cache() {
        // ZEB-461 Task 7: on accept, the requester's advertised device bundle is
        // learned into the local OwnerDeviceCache (keyed by the requester's
        // OwnerAddr) so the DM outbox can resolve a route to that friend even
        // without a shared community.
        use crate::owner_state_crypto::KeyTree;
        let (req, expected_devices) = signed_request_with_devices(0x90);
        let me = mint_test_owner(0x91);
        let kt = KeyTree::derive(&[7u8; 32]).expect("kt");
        let mut state = OwnerState::default();
        process_friend_request(
            &mut state,
            test_hlc(5),
            &req,
            me.owner,
            None,
            &me.cert,
            &me.device_key,
            &kt,
            &no_revocations(),
            0,
            None,
            None,
            None,
            vec![],
        )
        .expect("processed");
        let entry = state
            .owner_device_cache
            .devices
            .get(&req.from_addr)
            .expect("cache entry for requester");
        assert_eq!(entry.devices, expected_devices);
    }

    #[test]
    fn process_friend_request_empty_bundle_does_not_touch_device_cache() {
        // ZEB-461 Task 7 skip-on-empty guard: an EMPTY advertised bundle (older
        // client, or a None-reachability self path) must NOT create or clobber a
        // cache entry — storing an empty list under a newer HLC would LWW-clobber
        // a previously-known-good entry.
        use crate::owner_state_crypto::KeyTree;
        let req = signed_request_no_token(0x92);
        assert!(
            req.sender_devices.is_empty(),
            "the no-token builder ships an empty device bundle"
        );
        let me = mint_test_owner(0x93);
        let kt = KeyTree::derive(&[8u8; 32]).expect("kt");
        let mut state = OwnerState::default();
        process_friend_request(
            &mut state,
            test_hlc(5),
            &req,
            me.owner,
            None,
            &me.cert,
            &me.device_key,
            &kt,
            &no_revocations(),
            0,
            None,
            None,
            None,
            vec![],
        )
        .expect("processed");
        assert!(
            !state
                .owner_device_cache
                .devices
                .contains_key(&req.from_addr),
            "an empty bundle must not create a device-cache entry"
        );
    }

    /// ZEB-580 S1: a signed friend request from a REAL (`mint_owner`) owner,
    /// whose EnrollmentCert carries a real birational X25519 — so the acceptor
    /// can derive the peer's #2 DM identity from the cert. It also carries a
    /// non-empty #3 wire bundle (a bogus hash + `None` pub) that DIFFERS from the
    /// cert-derived #2 hash, plus correctly-sized reachability so the handshake
    /// yields a dialable tunnel contact. Used to prove the acceptor caches the
    /// CERT-attested #2 identity, NOT the wire #3 bundle.
    fn signed_request_real_owner() -> FriendLinkRequest {
        use crate::owner_state_types::{ML_DSA_65_PUBKEY_LEN, ML_KEM_768_PUBKEY_LEN};
        let minted = harmony_owner::lifecycle::mint_owner(1_700_000_000).expect("mint real owner");
        let cert = minted
            .state
            .enrollments
            .values()
            .next()
            .expect("mint_owner enrolls one device")
            .clone();
        let device_key = minted.device_signing_key;
        let from_addr = OwnerAddr(cert.owner_id);
        let (_eph_sk, eph_pub) = crate::friend_rendezvous::generate_ephemeral();
        // A #3 wire bundle that DIFFERS from the cert-derived #2 hash. A `None`
        // pub is always accepted by `apply_owner_device_update` (no hash check),
        // so pre-fix code caches THIS hash — the fix must cache #2 instead.
        let sender_devices = vec![DeviceIdentityHash([0x77; 16])];
        let device_identity_pubs: Vec<Option<[u8; 64]>> = vec![None];
        let iroh_node_id = [0x11; 32];
        let pq_dsa_pubkey = vec![0u8; ML_DSA_65_PUBKEY_LEN];
        let pq_kem_pubkey = vec![0u8; ML_KEM_768_PUBKEY_LEN];
        let devices_digest = contact_digest(
            &sender_devices,
            &device_identity_pubs,
            &iroh_node_id,
            None,
            &pq_dsa_pubkey,
            &pq_kem_pubkey,
        );
        let preimage = friend_request_sig_preimage(from_addr, None, &eph_pub, &devices_digest);
        let sig = device_key.sign(&preimage).to_bytes();
        FriendLinkRequest {
            from_addr,
            display: Some("dave".into()),
            token_sig: None,
            eph_x25519_pub: eph_pub,
            enrollment: cert,
            signer_certs: Vec::new(),
            sig,
            sender_devices,
            device_identity_pubs,
            iroh_node_id,
            home_relay_url: None,
            pq_dsa_pubkey,
            pq_kem_pubkey,
            revocations: Vec::new(),
        }
    }

    /// ZEB-580 S1: after processing a friend request, the acceptor caches the
    /// requester's #2 DM identity (derived from the verified enrollment cert),
    /// keyed by the #2 DM hash — not the wire #3 bundle it also carries.
    #[test]
    fn process_friend_request_caches_requester_device2_identity() {
        use crate::owner_state_crypto::KeyTree;
        let req = signed_request_real_owner();
        let expect_pub = crate::dm_signing::device2_combined_pub(&req.enrollment);
        let expect_hash = crate::dm_signing::device2_signing_hash(&req.enrollment)
            .expect("real cert yields a #2 hash");
        let me = mint_test_owner(0x94);
        let kt = KeyTree::derive(&[9u8; 32]).expect("kt");
        let mut state = OwnerState::default();
        process_friend_request(
            &mut state,
            test_hlc(5),
            &req,
            me.owner,
            None,
            &me.cert,
            &me.device_key,
            &kt,
            &no_revocations(),
            0,
            None,
            None,
            None,
            vec![],
        )
        .expect("processed");
        let entry = state
            .owner_device_cache
            .devices
            .get(&req.from_addr)
            .expect("cache entry for requester");
        let idx = entry
            .devices
            .iter()
            .position(|d| *d == expect_hash)
            .expect("#2 device cached");
        assert_eq!(entry.device_identity_pubs[idx], Some(expect_pub));
        // The tunnel contact rode along on the cached #2 device.
        assert!(
            entry
                .device_tunnel_contacts
                .get(idx)
                .map(|c| c.is_some())
                .unwrap_or(false),
            "the dialable tunnel contact must persist parallel to the #2 device"
        );
        // The cert-derived #2 identity REPLACED the wire #3 bundle: the bogus
        // wire hash must NOT be cached.
        assert!(
            !entry.devices.contains(&DeviceIdentityHash([0x77; 16])),
            "the wire #3 bundle must not be cached once the cert carries a #2 identity"
        );
    }

    #[test]
    fn authenticate_friend_request_accepts_valid_and_rejects_tampered() {
        let req = signed_request_no_token(0x80);
        authenticate_friend_request(&req, &no_revocations(), 0)
            .expect("a valid no-token request authenticates");

        let mut bad = req.clone();
        bad.sig[0] ^= 0xFF;
        assert!(
            matches!(
                authenticate_friend_request(&bad, &no_revocations(), 0),
                Err(FriendHandshakeError::SignatureInvalid)
            ),
            "a tampered signature must fail authentication"
        );
    }

    /// ZEB-680 §1 (T3 regression pin): `authenticate_friend_request` consults the
    /// revoked-device projection through the inner `verify_enrolled_device`. A
    /// request whose enrolled device-#2 key is revoked for its own owner fails
    /// with `DeviceRevoked`; the SAME request with an empty projection
    /// authenticates. Because the only difference between the two calls is the
    /// projection, the rejection can only come from the revocation consult — so
    /// this pins the per-site enforcement against a later refactor dropping it.
    #[test]
    fn authenticate_friend_request_rejects_revoked_requester() {
        let req = signed_request_no_token(0x81);
        // Empty projection revokes nothing: the request authenticates.
        authenticate_friend_request(&req, &no_revocations(), 0)
            .expect("empty projection revokes nothing");
        // Seed the requester's enrolled device key against its own owner.
        let revoked = crate::revoked_device_projection::RevokedDeviceProjection::new();
        let keys: std::collections::BTreeSet<[u8; 32]> =
            std::iter::once(req.enrollment.device_pubkeys.classical.ed25519_verify).collect();
        revoked.union_from_members(std::iter::once((req.from_addr, &keys)));
        let err = authenticate_friend_request(&req, &revoked, 0).unwrap_err();
        assert!(
            matches!(err, FriendHandshakeError::DeviceRevoked),
            "expected DeviceRevoked, got {err:?}"
        );
    }

    // ---- ZEB-680 T5: receive phase 1 — carried-attestation verification ----

    #[test]
    fn carried_revocations_valid_pass() {
        // An own-fleet Master pair verifies when `peer_owner` is the pair's own
        // master (both the revocation's and the enrollment's `owner_id`).
        let att = revocation_attestation(0x60);
        let peer_owner = OwnerAddr(att.revocation.owner_id);
        verify_carried_revocations(peer_owner, std::slice::from_ref(&att))
            .expect("a valid own-fleet attestation passes");
    }

    #[test]
    fn carried_revocations_empty_ok() {
        // Absence is the back-compat no-op regardless of the claimed owner.
        verify_carried_revocations(OwnerAddr([0xAB; 16]), &[])
            .expect("empty slice is Ok (back-compat no-op)");
    }

    #[test]
    fn carried_revocations_third_party_owner_rejected() {
        // Trust-bind: a valid pair whose owner != the link peer is rejected — a
        // peer may only attest ITS OWN devices, never relay a third party's
        // (mirrors dm_outbox::handle_revocation_push_rejects_third_party_owner).
        let att = revocation_attestation(0x60);
        let wrong = OwnerAddr([0xEE; 16]);
        assert_ne!(wrong, OwnerAddr(att.revocation.owner_id));
        let err = verify_carried_revocations(wrong, std::slice::from_ref(&att)).unwrap_err();
        assert!(
            matches!(err, FriendHandshakeError::RevocationAttestationInvalid(_)),
            "third-party owner must be rejected, got {err:?}"
        );
    }

    #[test]
    fn carried_revocations_target_enrollment_mismatch_rejected() {
        use harmony_owner::certs::{EnrollmentCert, RevocationCert, RevocationReason};
        use harmony_owner::pubkey_bundle::PubKeyBundle;
        // A valid revocation for device A paired with a valid enrollment for a
        // DIFFERENT device B under the SAME master: enrollment.device_id !=
        // revocation.target, so the target↔enrollment binding rejects (mirrors
        // dm_outbox::handle_revocation_push_rejects_target_enrollment_mismatch).
        let master_sk = ed25519_dalek::SigningKey::from_bytes(&[0x71; 32]);
        let master_bundle = PubKeyBundle::classical_only(master_sk.verifying_key().to_bytes());
        let owner = OwnerAddr(master_bundle.identity_hash());
        let dev_a = PubKeyBundle::classical_only(
            ed25519_dalek::SigningKey::from_bytes(&[0x72; 32])
                .verifying_key()
                .to_bytes(),
        );
        let revocation = RevocationCert::sign_master(
            &master_sk,
            master_bundle.clone(),
            dev_a.identity_hash(),
            1_700_000_000,
            RevocationReason::Compromised,
        )
        .expect("mint revocation for device A");
        let dev_b = PubKeyBundle::classical_only(
            ed25519_dalek::SigningKey::from_bytes(&[0x73; 32])
                .verifying_key()
                .to_bytes(),
        );
        let other_enrollment = EnrollmentCert::sign_master(
            &master_sk,
            master_bundle,
            dev_b.identity_hash(),
            dev_b,
            1_700_000_000,
            None,
        )
        .expect("mint enrollment for device B");
        let att = RevocationAttestation {
            revocation,
            enrollment: Box::new(other_enrollment),
        };
        let err = verify_carried_revocations(owner, std::slice::from_ref(&att)).unwrap_err();
        assert!(
            matches!(err, FriendHandshakeError::RevocationAttestationInvalid(_)),
            "target/enrollment mismatch must be rejected, got {err:?}"
        );
    }

    #[test]
    fn authenticate_friend_request_rejects_invalid_attestation() {
        // A fully valid signed request carrying one bogus attestation (a valid
        // pair whose owner != req.from_addr → third-party) fails the whole
        // handshake closed; the SAME request with the attestation removed
        // authenticates. The only difference between the two calls is the carried
        // list, so the rejection can only come from the phase-1 carried-revocation
        // verify — pinning that wire-in against a later refactor dropping it.
        let (mut req, _, _) = signed_request(0x84, [7u8; 64]);
        authenticate_friend_request(&req, &no_revocations(), 0)
            .expect("valid request with no attestations authenticates");
        let bogus = revocation_attestation(0x62);
        assert_ne!(OwnerAddr(bogus.revocation.owner_id), req.from_addr);
        req.revocations = vec![bogus];
        let err = authenticate_friend_request(&req, &no_revocations(), 0).unwrap_err();
        assert!(
            matches!(err, FriendHandshakeError::RevocationAttestationInvalid(_)),
            "a present-but-invalid attestation must fail the handshake closed, got {err:?}"
        );
    }

    // ---- ZEB-680 T6: receive phase 2 — apply carried revocations at establishment ----

    /// ZEB-680 §2 (Task 6): a full `process_friend_request` with one valid
    /// own-fleet attestation lands the revoked device key in BOTH the DM
    /// revoked-device store AND the live projection, and reports the genuine
    /// insert. Phase-2 (apply-at-establishment) counterpart to the T5 phase-1
    /// verify tests.
    #[test]
    fn accepted_handshake_applies_carried_revocations() {
        use crate::owner_state_crypto::KeyTree;
        let me = mint_test_owner(0x60); // acceptor (self)
        let token_sig = [0x5a; 64];
        let (mut req, _, _) = signed_request(0x61, token_sig);
        // Same master seed → the attestation's owner == the requester's owner, so
        // the peer attests ITS OWN device (identity_hash excludes x25519, so the
        // classical_only-vs-explicit-bundle x25519 difference is irrelevant).
        let att = revocation_attestation(0x61);
        assert_eq!(
            OwnerAddr(att.revocation.owner_id),
            req.from_addr,
            "fixture: attestation must be bound to the requester's own owner"
        );
        let revoked_key = att.enrollment.device_pubkeys.classical.ed25519_verify;
        req.revocations = vec![att];

        let kt = KeyTree::derive(&[9u8; 32]).expect("kt");
        let mut state = OwnerState::default();
        let projection = crate::revoked_device_projection::RevokedDeviceProjection::new();
        let (_accepted, inserted) = process_friend_request(
            &mut state,
            test_hlc(1_000),
            &req,
            me.owner,
            Some("me".into()),
            &me.cert,
            &me.device_key,
            &kt,
            &projection,
            0,
            None,
            None,
            None,
            vec![],
        )
        .expect("valid request with an own-fleet attestation is processed");

        assert!(inserted, "a genuine new revoked key must report inserted");
        assert!(
            state
                .revoked_dm_devices
                .get(&req.from_addr)
                .is_some_and(|s| s.contains(&revoked_key)),
            "the revoked device key must land in the DM revoked-device store"
        );
        assert!(
            projection.is_revoked(&req.from_addr, &revoked_key),
            "the revoked device key must feed the live projection"
        );
    }

    /// ZEB-680 §2 (Task 6): the carried-revocation apply is establishment-gated —
    /// an auth-FAILING request (bad signature) carrying a valid attestation is
    /// rejected before any write, so nothing (friend or revocation) is applied.
    #[test]
    fn carried_revocation_apply_is_establishment_gated() {
        use crate::owner_state_crypto::KeyTree;
        let me = mint_test_owner(0x62);
        let (mut req, _, _) = signed_request(0x63, [0x5a; 64]);
        // A valid own-fleet attestation — but the request signature is corrupted,
        // so the handshake fails auth BEFORE the establishment apply is reached.
        req.revocations = vec![revocation_attestation(0x63)];
        req.sig[0] ^= 0xFF;

        let kt = KeyTree::derive(&[9u8; 32]).expect("kt");
        let mut state = OwnerState::default();
        let projection = crate::revoked_device_projection::RevokedDeviceProjection::new();
        let err = process_friend_request(
            &mut state,
            test_hlc(1_000),
            &req,
            me.owner,
            Some("me".into()),
            &me.cert,
            &me.device_key,
            &kt,
            &projection,
            0,
            None,
            None,
            None,
            vec![],
        )
        .expect_err("a bad-signature request must be rejected");

        assert!(
            matches!(err, FriendHandshakeError::SignatureInvalid),
            "a bad-signature request must fail auth, got {err:?}"
        );
        assert!(
            state.revoked_dm_devices.is_empty(),
            "an auth-failed handshake must apply no carried revocations"
        );
        assert!(
            state.friend_graph.friends.is_empty(),
            "an auth-failed handshake must write no friend"
        );
    }

    /// ZEB-680 §2 (Task 6): applying the same carried attestation twice reports a
    /// genuine insert the FIRST time and NO insert the second — the flag the
    /// dispatch/driver uses to skip a redundant owner-state publish for an
    /// all-duplicate re-apply.
    #[test]
    fn duplicate_carried_revocation_reports_no_insert() {
        use crate::owner_state_crypto::KeyTree;
        let me = mint_test_owner(0x64);
        let (mut req, _, _) = signed_request(0x65, [0x5a; 64]);
        req.revocations = vec![revocation_attestation(0x65)];

        let kt = KeyTree::derive(&[9u8; 32]).expect("kt");
        let mut state = OwnerState::default();
        let projection = crate::revoked_device_projection::RevokedDeviceProjection::new();
        let (_, first) = process_friend_request(
            &mut state,
            test_hlc(1_000),
            &req,
            me.owner,
            Some("me".into()),
            &me.cert,
            &me.device_key,
            &kt,
            &projection,
            0,
            None,
            None,
            None,
            vec![],
        )
        .expect("first apply");
        assert!(first, "first apply of a fresh revocation inserts");
        let (_, second) = process_friend_request(
            &mut state,
            test_hlc(2_000),
            &req,
            me.owner,
            Some("me".into()),
            &me.cert,
            &me.device_key,
            &kt,
            &projection,
            0,
            None,
            None,
            None,
            vec![],
        )
        .expect("second apply");
        assert!(
            !second,
            "a duplicate carried revocation must report no insert (dirty gate)"
        );
    }

    /// ZEB-680 §2 (Task 6 / BINDING REQ 1): a valid signed request carrying a
    /// BOGUS attestation (owner != req.from_addr → third-party) passed DIRECTLY to
    /// process_friend_request (bypassing serve()'s phase-1 precheck) is rejected
    /// in-function with the typed error, writing NOTHING. Pins the in-function
    /// re-verify so no refactor/new-caller can apply unverified pairs or establish
    /// a friendship on a bad carried list.
    #[test]
    fn process_friend_request_rejects_invalid_attestation_in_function() {
        use crate::owner_state_crypto::KeyTree;
        let me = mint_test_owner(0x66);
        let (mut req, _, _) = signed_request(0x67, [0x5a; 64]);
        let bogus = revocation_attestation(0xAA); // third-party master
        assert_ne!(OwnerAddr(bogus.revocation.owner_id), req.from_addr);
        req.revocations = vec![bogus];

        let kt = KeyTree::derive(&[9u8; 32]).expect("kt");
        let mut state = OwnerState::default();
        let projection = crate::revoked_device_projection::RevokedDeviceProjection::new();
        let err = process_friend_request(
            &mut state,
            test_hlc(1_000),
            &req,
            me.owner,
            Some("me".into()),
            &me.cert,
            &me.device_key,
            &kt,
            &projection,
            0,
            None,
            None,
            None,
            vec![],
        )
        .expect_err("a bogus carried attestation must reject");

        assert!(
            matches!(err, FriendHandshakeError::RevocationAttestationInvalid(_)),
            "a bogus carried attestation must reject in-function, got {err:?}"
        );
        assert!(
            state.friend_graph.friends.is_empty(),
            "a rejected handshake writes no friend"
        );
        assert!(
            state.revoked_dm_devices.is_empty(),
            "a rejected handshake writes no revocation"
        );
    }

    /// ZEB-680 §2 (Task 6): direct coverage of the shared establishment-apply
    /// helper both dialer drivers call (their apply cannot be unit-driven without
    /// an iroh harness). A valid own-fleet pair stores + feeds the projection and
    /// reports the insert; re-applying reports no insert.
    #[test]
    fn apply_carried_revocations_stores_dedupes_and_feeds_projection() {
        let att = revocation_attestation(0x68);
        let peer_owner = OwnerAddr(att.revocation.owner_id);
        let revoked_key = att.enrollment.device_pubkeys.classical.ed25519_verify;
        let atts = vec![att];

        let mut state = OwnerState::default();
        let projection = crate::revoked_device_projection::RevokedDeviceProjection::new();
        assert!(
            apply_carried_revocations(&mut state, peer_owner, &atts, &projection),
            "first apply reports a genuine insert"
        );
        assert!(
            state
                .revoked_dm_devices
                .get(&peer_owner)
                .is_some_and(|s| s.contains(&revoked_key)),
            "the revoked key must land in the DM store"
        );
        assert!(projection.is_revoked(&peer_owner, &revoked_key));
        assert!(
            !apply_carried_revocations(&mut state, peer_owner, &atts, &projection),
            "an idempotent re-apply reports no insert"
        );
    }

    #[test]
    fn process_no_token_request_yields_mutual_key_friend() {
        // AcceptInline path: a no-token request resolves established_via to
        // MutualKey (since token_sig is None) and writes an Active friend.
        use crate::owner_state_crypto::KeyTree;
        let me = mint_test_owner(0x81);
        let req = signed_request_no_token(0x82);
        let kt = KeyTree::derive(&[3u8; 32]).expect("kt");
        let mut state = OwnerState::default();
        process_friend_request(
            &mut state,
            test_hlc(10),
            &req,
            me.owner,
            None,
            &me.cert,
            &me.device_key,
            &kt,
            &no_revocations(),
            0,
            None,
            None,
            None,
            vec![],
        )
        .expect("no-token request processed");
        let entry = state
            .friend_graph
            .friends
            .get(&req.from_addr)
            .expect("friend inserted");
        assert_eq!(entry.status, FriendStatus::Active);
        assert_eq!(
            entry.established_via,
            FriendOrigin::MutualKey,
            "a no-token (Path A) request must record established_via = MutualKey"
        );
    }

    #[test]
    fn friend_response_round_trips_both_variants() {
        // Codec round-trip for both FriendLinkResponse variants (Box-transparent).
        let owner = mint_test_owner(0x83);
        let acc = FriendLinkAccepted {
            from_addr: owner.owner,
            display: Some("dave".into()),
            eph_x25519_pub: [0x12; 32],
            enrollment: owner.cert,
            signer_certs: Vec::new(),
            sig: [0x34; 64],
            sender_devices: vec![],
            device_identity_pubs: vec![],
            iroh_node_id: [0u8; 32],
            home_relay_url: None,
            pq_dsa_pubkey: vec![],
            pq_kem_pubkey: vec![],
            revocations: Vec::new(),
        };
        for resp in [
            FriendLinkResponse::Accepted(Box::new(acc)),
            FriendLinkResponse::Pending,
        ] {
            let bytes = encode_friend_response(&resp).expect("encode");
            let back = decode_friend_response(&bytes).expect("decode");
            assert_eq!(resp, back);
        }
    }

    #[test]
    fn decide_consent_truth_table() {
        let tok = [0u8; 64];
        // Some(token) → TokenPath, regardless of the other flags.
        for &known in &[false, true] {
            for &auto in &[false, true] {
                for &prior in &[false, true] {
                    assert_eq!(
                        decide_consent(Some(&tok), known, auto, prior),
                        ConsentDecision::TokenPath,
                        "Some(token) must always take the token path \
                         (known={known} auto={auto} prior={prior})"
                    );
                }
            }
        }

        // None + known + auto-on → AcceptInline.
        assert_eq!(
            decide_consent(None, true, true, false),
            ConsentDecision::AcceptInline,
        );
        // None + known + auto-off → Pending.
        assert_eq!(
            decide_consent(None, true, false, false),
            ConsentDecision::Pending,
        );
        // None + unknown + auto-on → Pending (unknown is NEVER auto-accepted).
        assert_eq!(
            decide_consent(None, false, true, false),
            ConsentDecision::Pending,
        );
        // None + unknown + prior_accept → AcceptInline (user already tapped Accept).
        assert_eq!(
            decide_consent(None, false, true, true),
            ConsentDecision::AcceptInline,
        );
        // None + known + auto-off + prior_accept → AcceptInline.
        assert_eq!(
            decide_consent(None, true, false, true),
            ConsentDecision::AcceptInline,
        );
    }

    #[test]
    fn resolve_consent_consumes_approval_once_under_concurrency() {
        use crate::friend_requests::PendingFriendRequests;
        let pending = PendingFriendRequests::new();
        let from = OwnerAddr([0x4d; 16]);
        pending.approve(from); // user tapped Accept once

        // Two handshakes race on the single approval (no token, not known/auto).
        // EXACTLY one wins AcceptInline; the loser falls back to Pending so it
        // does NOT derive a second, mismatched friendship secret.
        let first =
            resolve_consent_consuming_approval(Some(&pending), None, None, false, false, &from, 0);
        let second =
            resolve_consent_consuming_approval(Some(&pending), None, None, false, false, &from, 0);
        assert_eq!(
            first,
            ConsentDecision::AcceptInline,
            "first handshake claims the one-shot approval"
        );
        assert_eq!(
            second,
            ConsentDecision::Pending,
            "approval is one-shot: the racing handshake must NOT also inline-accept"
        );
    }

    #[test]
    fn resolve_consent_token_and_known_paths_preserve_approval() {
        use crate::friend_requests::PendingFriendRequests;
        let tok = [0u8; 64];
        let from = OwnerAddr([0x4e; 16]);

        // Token path authorises via the token gate, never the approval — so it
        // must not consume a pending approval.
        let pending = PendingFriendRequests::new();
        pending.approve(from);
        assert_eq!(
            resolve_consent_consuming_approval(
                Some(&pending),
                None,
                Some(&tok),
                false,
                false,
                &from,
                0
            ),
            ConsentDecision::TokenPath,
        );
        assert!(
            pending.is_approved(&from),
            "token path must leave the approval intact"
        );

        // known + auto-accept → AcceptInline without needing/consuming an approval.
        let empty = PendingFriendRequests::new();
        assert_eq!(
            resolve_consent_consuming_approval(Some(&empty), None, None, true, true, &from, 0),
            ConsentDecision::AcceptInline,
        );
    }

    // ==================================================================
    // ZEB-680 T7 — handshake-level revocation regressions
    // ==================================================================
    //
    // These drive the REAL serve handler (`handle_friend_handshake_inbound`)
    // over a live iroh loopback bi-stream, with the TEST playing the dialer so
    // the exact request frame — including its carried `revocations` — is under
    // test control. This is the integration seam the pure-core T2/T5/T6 unit
    // tests can't reach on their own: wire decode → `authenticate_friend_request`
    // (consulting `self.revoked`) → `process_friend_request` (applying the
    // carried revocations), end to end. NOTE: the acceptor test module had no
    // pre-existing serve-level harness — the only prior end-to-end rig lives in
    // `tests/dm/friend_token_roundtrip_integration.rs`, whose full pkarr/token
    // machinery the dialer controls (so it can't inject a carried attestation).
    // This minimal rig mirrors that harness's hermetic-endpoint pattern but
    // hands the dialer to the test.

    /// Build a hermetic, relay-disabled iroh endpoint on loopback registered
    /// with the friend ALPN. Mirrors the integration harness's
    /// `build_hermetic_endpoint`, minus the zenoh/handshake ALPNs this
    /// direct-dial rig never negotiates.
    async fn t7_loopback_endpoint() -> iroh::endpoint::Endpoint {
        use iroh::endpoint::{presets, Endpoint, RelayMode};
        use iroh::SecretKey;
        Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::generate())
            .alpns(vec![crate::iroh_endpoint::alpn::HARMONY_FRIEND_V1.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .dns_resolver(crate::iroh_endpoint::hermetic_dns_resolver())
            .clear_ip_transports()
            .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("bind_addr loopback")
            .bind()
            .await
            .expect("bind hermetic iroh endpoint")
    }

    /// Build a friend acceptor for the T7 rig: self identity from `self_seed`,
    /// the given owner-state + revoked projection wired in, no pkarr publisher
    /// (the refuse + AcceptInline paths under test never touch the token gate).
    fn t7_build_acceptor(
        self_seed: u8,
        crdt_state: Arc<TokioMutex<OwnerState>>,
        projection: crate::revoked_device_projection::RevokedDeviceProjection,
    ) -> IrohFriendHandshakeAcceptor<()> {
        let me = mint_test_owner(self_seed);
        let hlc_tracker = Arc::new(TokioMutex::new(harmony_crdt_sync::ReplayTracker::new(
            format!("dev-{self_seed:02x}"),
        )));
        let device2 = Arc::new(ed25519_dalek::SigningKey::from_bytes(
            &me.device_key.to_bytes(),
        ));
        let keytree = Arc::new(
            crate::owner_state_crypto::KeyTree::derive(&[self_seed; 32]).expect("keytree derive"),
        );
        IrohFriendHandshakeAcceptor::<()>::new(
            crdt_state,
            hlc_tracker,
            format!("dev-{self_seed:02x}"),
            me.owner,
            Some("me".to_string()),
            me.cert.clone(),
            device2,
            keytree,
            None,
            None,
        )
        .with_revoked(projection)
    }

    /// Drive one full inbound friend handshake against `acceptor` over an iroh
    /// loopback pair. Returns the handler's own `Result` (the authoritative
    /// server-side outcome) plus the decoded response the dialer read back, if
    /// any — a refused handshake resets the stream with no response, surfacing
    /// as `Err` on the dialer's read.
    async fn t7_drive_handshake(
        acceptor: Arc<IrohFriendHandshakeAcceptor<()>>,
        req: FriendLinkRequest,
    ) -> (
        Result<FriendInboundOutcome, FriendAcceptError>,
        Result<FriendLinkResponse, String>,
    ) {
        let server_ep = t7_loopback_endpoint().await;
        let client_ep = t7_loopback_endpoint().await;

        let mut server_addr = iroh::EndpointAddr::new(server_ep.id());
        for sock in server_ep.bound_sockets() {
            server_addr = server_addr.with_ip_addr(sock);
        }

        // Server: accept exactly one connection and run the REAL handler, then
        // hold the connection open (bounded) until the dialer drives the close —
        // mirroring the production `handle_connection`, which waits on
        // `conn.closed()` so response bytes flush before `conn`/`server_ep` drop.
        // `server_ep` is owned by this block, so it — and the connection derived
        // from it — stays alive until the block completes.
        let server_task = tokio::spawn(async move {
            let incoming = server_ep
                .accept()
                .await
                .expect("server: incoming connection");
            let conn = incoming.await.expect("server: accept→connect");
            let result = acceptor.handle_friend_handshake_inbound(&conn).await;
            let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
            result
        });

        // Client (the dialer): connect on the friend ALPN, open a bi-stream, and
        // write the length-prefixed request frame under full test control.
        let conn = client_ep
            .connect(server_addr, crate::iroh_endpoint::alpn::HARMONY_FRIEND_V1)
            .await
            .expect("client: dial");
        let (mut send, mut recv) = conn.open_bi().await.expect("client: open_bi");
        let body = encode_friend_request(&req).expect("client: encode request");
        let prefix = crate::iroh_framing::encode_len_prefix(
            body.len(),
            FRIEND_MAX_PACKET_LEN,
            crate::iroh_framing::Endian::Le,
            false,
        )
        .expect("client: len prefix");
        send.write_all(&prefix).await.expect("client: write prefix");
        send.write_all(&body).await.expect("client: write body");
        send.finish().expect("client: finish");

        // Read the response frame, BOUNDED: a refused handshake writes nothing
        // and (endpoint-drop resets don't reach the dialer promptly) the read
        // would otherwise block, so a timeout is the dialer's "refused" signal →
        // `Err`. A happy-path accept resolves it well inside the bound.
        let resp = match tokio::time::timeout(Duration::from_secs(3), async {
            let mut len_buf = [0u8; 4];
            recv.read_exact(&mut len_buf)
                .await
                .map_err(|e| e.to_string())?;
            let len = crate::iroh_framing::decode_len_prefix(
                len_buf,
                FRIEND_MAX_PACKET_LEN,
                crate::iroh_framing::Endian::Le,
                false,
            )
            .map_err(|e| e.to_string())?;
            let mut buf = vec![0u8; len];
            recv.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
            decode_friend_response(&buf).map_err(|e| e.to_string())
        })
        .await
        {
            Ok(inner) => inner,
            Err(_elapsed) => Err("dialer response read timed out (handshake refused)".to_string()),
        };

        // Close from the dialer so the server's `conn.closed()` completes, then
        // join the handler for its authoritative outcome.
        conn.close(0u32.into(), b"t7-done");
        let result = server_task.await.expect("server task join");
        (result, resp)
    }

    /// ZEB-680 T7(a): the ACCEPTOR's `revoked` projection (seeded via
    /// `with_revoked` at construction) names the requester's enrolled device
    /// key → the full inbound handshake is refused with `DeviceRevoked` (surfaced
    /// as `FriendAcceptError::Handshake(DeviceRevoked)`), no response is written,
    /// and NO friend entry lands in the acceptor's owner-state. Pins the T2
    /// serve-side wire-in of the projection through `authenticate_friend_request`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serve_refuses_revoked_requester() {
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(Duration::from_secs(30), async {
            let req = signed_request_no_token(0x71);
            let revoked_key = req.enrollment.device_pubkeys.classical.ed25519_verify;

            let crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
            let projection = crate::revoked_device_projection::RevokedDeviceProjection::new();
            let keys: std::collections::BTreeSet<[u8; 32]> = std::iter::once(revoked_key).collect();
            projection.union_from_members(std::iter::once((req.from_addr, &keys)));

            let acceptor = Arc::new(t7_build_acceptor(0x70, Arc::clone(&crdt_state), projection));
            let (result, resp) = t7_drive_handshake(acceptor, req).await;

            assert!(
                matches!(
                    result,
                    Err(FriendAcceptError::Handshake(
                        FriendHandshakeError::DeviceRevoked
                    ))
                ),
                "a revoked requester must be refused with DeviceRevoked, got {result:?}"
            );
            assert!(
                resp.is_err(),
                "a refused handshake writes no response frame, got {resp:?}"
            );
            assert!(
                crdt_state.lock().await.friend_graph.friends.is_empty(),
                "a refused handshake must write no friend entry"
            );
        })
        .await
        .expect("serve_refuses_revoked_requester completes within 30s");
    }

    /// ZEB-680 T7(b): a request that carries one valid own-fleet revocation
    /// attestation → the handshake is accepted AND the carried revoked device key
    /// lands in the acceptor's DM revoked-device store and its live projection.
    /// Pins the serve-side thread-through of `req.revocations` into
    /// `process_friend_request`'s apply-at-establishment.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serve_applies_carried_revocations_end_to_end() {
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(Duration::from_secs(30), async {
            // The attestation is bound to the requester's OWN owner (same master
            // seed), so the phase-1 own-fleet trust-bind passes.
            let mut req = signed_request_no_token(0x73);
            let att = revocation_attestation(0x73);
            assert_eq!(
                OwnerAddr(att.revocation.owner_id),
                req.from_addr,
                "fixture: the attestation must attest the requester's own device"
            );
            let revoked_key = att.enrollment.device_pubkeys.classical.ed25519_verify;
            req.revocations = vec![att];
            let requester_owner = req.from_addr;

            let crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
            let projection = crate::revoked_device_projection::RevokedDeviceProjection::new();
            // Pre-approve the requester so the no-token request accepts inline
            // (the accept path is what runs the carried-revocation apply).
            let pending = Arc::new(crate::friend_requests::PendingFriendRequests::new());
            pending.mark_approved(requester_owner);

            let acceptor = Arc::new(
                t7_build_acceptor(0x72, Arc::clone(&crdt_state), projection.clone())
                    .with_pending_requests(Some(pending)),
            );
            let (result, resp) = t7_drive_handshake(acceptor, req).await;

            assert!(
                result.is_ok(),
                "a carried-revocation handshake must be accepted, got {result:?}"
            );
            assert!(
                matches!(resp, Ok(FriendLinkResponse::Accepted(_))),
                "the dialer must read an Accepted response, got {resp:?}"
            );
            let state = crdt_state.lock().await;
            assert!(
                state
                    .revoked_dm_devices
                    .get(&requester_owner)
                    .is_some_and(|s| s.contains(&revoked_key)),
                "the carried revoked device key must land in the DM revoked-device store"
            );
            drop(state);
            assert!(
                projection.is_revoked(&requester_owner, &revoked_key),
                "the acceptor's live projection must report the carried key revoked"
            );
        })
        .await
        .expect("serve_applies_carried_revocations_end_to_end completes within 30s");
    }

    /// ZEB-680 T7(c) — back-compat pin: a pre-ZEB-680 frame carries
    /// `revocations: vec![]`, which T1's codec encodes WITHOUT the "v" key, so
    /// the wire is byte-identical to an old dialer's. The handshake must complete
    /// exactly as before — an Active friend written, `Accepted` returned — and
    /// touch no revocation state.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serve_accepts_pre_zeb680_frame_unchanged() {
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(Duration::from_secs(30), async {
            let req = signed_request_no_token(0x75);
            assert!(
                req.revocations.is_empty(),
                "pre-ZEB-680 shape carries no revocations (codec omits the \"v\" key)"
            );
            let requester_owner = req.from_addr;

            let crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
            let projection = crate::revoked_device_projection::RevokedDeviceProjection::new();
            let pending = Arc::new(crate::friend_requests::PendingFriendRequests::new());
            pending.mark_approved(requester_owner);

            let acceptor = Arc::new(
                t7_build_acceptor(0x74, Arc::clone(&crdt_state), projection)
                    .with_pending_requests(Some(pending)),
            );
            let (result, resp) = t7_drive_handshake(acceptor, req).await;

            assert!(
                result.is_ok(),
                "a pre-ZEB-680 frame must complete the handshake, got {result:?}"
            );
            assert!(
                matches!(resp, Ok(FriendLinkResponse::Accepted(_))),
                "the dialer must read an Accepted response, got {resp:?}"
            );
            let state = crdt_state.lock().await;
            let entry = state
                .friend_graph
                .friends
                .get(&requester_owner)
                .expect("friend entry written");
            assert_eq!(
                entry.status,
                FriendStatus::Active,
                "the requester becomes an Active friend"
            );
            assert!(
                state.revoked_dm_devices.is_empty(),
                "a pre-ZEB-680 frame carries no revocations to apply"
            );
        })
        .await
        .expect("serve_accepts_pre_zeb680_frame_unchanged completes within 30s");
    }

    /// ZEB-700 Tier 1: a connection-tier shed answers the SAME benign `Pending`
    /// a Path-A outcome writes (server-side `Ok`, no error) with ZERO state
    /// effect — nothing recorded in the pending-request store (the observable
    /// difference from a REAL Pending outcome), no friend written. Zero conn
    /// cap forces the shed on the first handshake; the sliding-window
    /// mechanics are pinned by the `FriendRateLimiter` unit tests.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serve_connection_shed_writes_benign_pending_zeb700() {
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(Duration::from_secs(30), async {
            let req = signed_request_no_token(0x77);

            let crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
            let projection = crate::revoked_device_projection::RevokedDeviceProjection::new();
            let pending = Arc::new(crate::friend_requests::PendingFriendRequests::new());

            let acceptor = Arc::new(
                t7_build_acceptor(0x76, Arc::clone(&crdt_state), projection)
                    .with_pending_requests(Some(Arc::clone(&pending)))
                    .with_rate_limiter(Arc::new(
                        crate::friend_intro::FriendRateLimiter::with_caps(0, 100, 3_600_000),
                    )),
            );
            let (result, resp) = t7_drive_handshake(acceptor, req).await;

            assert!(
                matches!(result, Ok(FriendInboundOutcome::Shed)),
                "a shed is a benign outcome reported as Shed (not a mislogged accept), got {result:?}"
            );
            assert!(
                matches!(resp, Ok(FriendLinkResponse::Pending)),
                "the dialer must read the SAME benign Pending reply, got {resp:?}"
            );
            assert!(
                pending.list().is_empty(),
                "a shed records NOTHING (unlike a real Pending outcome)"
            );
            assert!(
                crdt_state.lock().await.friend_graph.friends.is_empty(),
                "a shed writes no friend entry"
            );
        })
        .await
        .expect("serve_connection_shed_writes_benign_pending_zeb700 completes within 30s");
    }

    /// ZEB-700 Tier 2: the per-owner quota sits POST-auth. (a) A pre-approved
    /// requester that would inline-accept is shed to the benign `Pending`
    /// instead — no friend written, nothing recorded. (b) A request with a
    /// tampered handshake sig is still REFUSED (no response frame) even with a
    /// zero owner cap — the shed never masks an auth failure, so unauthenticated
    /// traffic cannot buy the benign ack.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serve_owner_quota_sheds_post_auth_only_zeb700() {
        crate::iroh_endpoint::warm_up_iroh_global_init().await;
        tokio::time::timeout(Duration::from_secs(60), async {
            // (a) authenticated + pre-approved, owner cap 0 → shed to Pending.
            let req = signed_request_no_token(0x79);
            let requester_owner = req.from_addr;
            let crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
            let pending = Arc::new(crate::friend_requests::PendingFriendRequests::new());
            pending.mark_approved(requester_owner);
            let acceptor = Arc::new(
                t7_build_acceptor(
                    0x78,
                    Arc::clone(&crdt_state),
                    crate::revoked_device_projection::RevokedDeviceProjection::new(),
                )
                .with_pending_requests(Some(Arc::clone(&pending)))
                .with_rate_limiter(Arc::new(
                    crate::friend_intro::FriendRateLimiter::with_caps(100, 0, 3_600_000),
                )),
            );
            let (result, resp) = t7_drive_handshake(acceptor, req).await;
            assert!(
                matches!(result, Ok(FriendInboundOutcome::Shed)),
                "owner-quota shed is benign and reported as Shed, got {result:?}"
            );
            assert!(
                matches!(resp, Ok(FriendLinkResponse::Pending)),
                "shed pre-empts the inline accept with the benign Pending, got {resp:?}"
            );
            assert!(
                crdt_state.lock().await.friend_graph.friends.is_empty(),
                "the shed handshake writes no friend"
            );

            // (b) tampered sig, owner cap 0 → refused (no response), not shed.
            let mut bad = signed_request_no_token(0x7B);
            bad.sig[0] ^= 0xFF;
            let crdt_state_b = Arc::new(TokioMutex::new(OwnerState::default()));
            let acceptor_b = Arc::new(
                t7_build_acceptor(
                    0x7A,
                    Arc::clone(&crdt_state_b),
                    crate::revoked_device_projection::RevokedDeviceProjection::new(),
                )
                .with_rate_limiter(Arc::new(
                    crate::friend_intro::FriendRateLimiter::with_caps(100, 0, 3_600_000),
                )),
            );
            let (result_b, resp_b) = t7_drive_handshake(acceptor_b, bad).await;
            assert!(
                matches!(result_b, Err(FriendAcceptError::Handshake(_))),
                "unauthenticated traffic is refused BEFORE the owner quota, got {result_b:?}"
            );
            assert!(
                resp_b.is_err(),
                "a refused handshake writes no response frame, got {resp_b:?}"
            );
        })
        .await
        .expect("serve_owner_quota_sheds_post_auth_only_zeb700 completes within 60s");
    }
}
