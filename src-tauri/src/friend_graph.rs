//! ZEB-370 Phase 1: Friend Graph owner-state sub-CRDT + token/policy types.
//!
//! Mirrors `OwnerDeviceCache` (`owner_state_types.rs`): a `BTreeMap` keyed by
//! `OwnerAddr`, LWW-merged per entry on `learned_at`. Friend links live in EACH
//! owner's own owner-state (replicated across that owner's devices via the
//! existing owner-state Zenoh sync) — there is no shared friend CRDT.
//!
//! Wire format: canonical CBOR. Every map key at a single nesting level must
//! encode to the same byte length (see the precondition on
//! `crate::owner_state_crypto::canonical_cbor_encode`). `FriendEntry` uses
//! all single-char keys ("p","n","s","v","r","l"); `FriendGraph` uses "f".

use crate::owner_state_types::{
    deserialize_bytes_from_bstr, serialize_bytes_as_bstr, Hlc, OwnerAddr,
};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

/// Upper bound on `FriendEntry.display` length, enforced at deserialize time
/// (see [`deserialize_capped_display`]). A malformed/oversized wire entry is
/// REJECTED rather than truncated — matching this codebase's strict-deserialize
/// culture (cf. `OwnerDeviceEntry`). The serialized shape of valid values is
/// unchanged. 256 bytes is generous for a human display name.
pub(crate) const MAX_FRIEND_DISPLAY_LEN: usize = 256;

/// Derive the friend's 16-byte `OwnerAddr` from their 64-byte combined
/// identity public-bytes value (`X25519_pub(32) || Ed25519_pub(32)`, the
/// canonical `harmony_identity::Identity::to_public_bytes()` layout).
///
/// Single source of truth: delegates to
/// `harmony_identity::Identity::from_public_bytes(pub_).address_hash`, the
/// same primitive behind `dm_signing::derive_device_hash_from_identity_pub`
/// (which produces a `DeviceIdentityHash` from the identical bytes). Never
/// re-derive the hash formula here — if harmony changes its scheme, we follow
/// automatically rather than silently diverge.
///
/// Returns `None` if the bytes are malformed (invalid X25519 or Ed25519 point
/// encoding). `apply_friend_update` treats `None` as an invariant failure.
pub(crate) fn owner_addr_from_identity_pub(pub_: &[u8; 64]) -> Option<OwnerAddr> {
    let identity = harmony_identity::Identity::from_public_bytes(pub_).ok()?;
    Some(OwnerAddr(identity.address_hash))
}

/// Strict deserializer for `FriendEntry.display`: rejects (does NOT truncate)
/// any string longer than [`MAX_FRIEND_DISPLAY_LEN`]. The field stays
/// `Option<String>`; a present value over the cap is a hard decode error so a
/// malformed/oversized wire entry never enters the CRDT. `None`/absent is
/// passed through unchanged, preserving the `skip_serializing_if` wire shape.
fn deserialize_capped_display<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(d)?;
    if let Some(s) = &opt {
        if s.len() > MAX_FRIEND_DISPLAY_LEN {
            return Err(serde::de::Error::custom(format!(
                "FriendEntry.display is {} bytes, exceeds MAX_FRIEND_DISPLAY_LEN ({})",
                s.len(),
                MAX_FRIEND_DISPLAY_LEN
            )));
        }
    }
    Ok(opt)
}

/// Lifecycle of a friend link. `Revoked` is an LWW tombstone (kept, not
/// deleted, so an unfriend on one device cannot be silently resurrected by a
/// stale `Active` from another device unless its `learned_at` is strictly
/// newer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FriendStatus {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "revoked")]
    Revoked,
}

/// How the link was formed (provenance, for UX + audit). Phase 1 only ever
/// produces `Token`; `MutualKey`/`Introduction` are reserved for later phases
/// so the data model needs no migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FriendOrigin {
    #[serde(rename = "mutual_key")]
    MutualKey,
    #[serde(rename = "token")]
    Token,
    #[serde(rename = "introduction")]
    Introduction,
}

/// Per-user policy governing whether OTHERS may reach you via a friend's
/// introduction. Stored now (so no later CRDT migration); ENFORCED in Phase 2.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerIntroPolicy {
    #[serde(rename = "open")]
    Open,
    #[default]
    #[serde(rename = "fof")]
    FriendsOfFriends,
    #[serde(rename = "ask")]
    AskMe,
    #[serde(rename = "closed")]
    Closed,
}

/// One friend, keyed in `FriendGraph.friends` by the friend's `OwnerAddr`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendEntry {
    /// Friend's 64-byte owner identity: `X25519_pub(32) || Ed25519_pub(32)`.
    /// Stored as a CBOR bstr(64).
    #[serde(
        rename = "p",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub friend_owner_pub: [u8; 64],
    /// Human label (their advertised display name at link time; refreshable).
    /// Capped at [`MAX_FRIEND_DISPLAY_LEN`] at deserialize time (oversized →
    /// hard decode error, not truncation). `serialize_with`/`skip_serializing_if`
    /// behavior is unchanged for valid values.
    #[serde(
        rename = "n",
        skip_serializing_if = "Option::is_none",
        default,
        deserialize_with = "deserialize_capped_display"
    )]
    pub display: Option<String>,
    /// Lifecycle. `Pending` = request sent/received, not yet mutual.
    #[serde(rename = "s")]
    pub status: FriendStatus,
    /// How this link was formed (provenance, for UX + audit).
    #[serde(rename = "v")]
    pub established_via: FriendOrigin,
    /// Whether THIS friend may be surfaced in our referral catalog to others
    /// (sharer-side opt-in for the Phase 2 awareness layer; default false).
    #[serde(rename = "r", default)]
    pub referrable: bool,
    /// LWW key.
    #[serde(rename = "l")]
    pub learned_at: Hlc,
}

/// Owner-state sub-CRDT. Replicated across the user's own devices via the
/// existing owner-state Zenoh topic; LWW-merged per entry on `learned_at`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendGraph {
    #[serde(rename = "f")]
    pub friends: BTreeMap<OwnerAddr, FriendEntry>,
}

impl FriendGraph {
    pub fn is_empty(&self) -> bool {
        self.friends.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
    use crate::owner_state_types::{Hlc, OwnerAddr};

    fn hlc(w: u64) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: "d".into(),
        }
    }

    fn sample_entry() -> FriendEntry {
        FriendEntry {
            friend_owner_pub: [0x11; 64],
            display: Some("alice".into()),
            status: FriendStatus::Active,
            established_via: FriendOrigin::Token,
            referrable: false,
            learned_at: hlc(7),
        }
    }

    #[test]
    fn friend_entry_round_trips() {
        let e = sample_entry();
        let bytes = canonical_cbor_encode(&e).expect("encode");
        let back: FriendEntry = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(e, back);
    }

    #[test]
    fn friend_graph_round_trips_with_entry() {
        let mut g = FriendGraph::default();
        g.friends.insert(OwnerAddr([0x22; 16]), sample_entry());
        let bytes = canonical_cbor_encode(&g).expect("encode");
        let back: FriendGraph = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(g, back);
    }

    #[test]
    fn default_policy_is_friends_of_friends() {
        assert_eq!(
            PeerIntroPolicy::default(),
            PeerIntroPolicy::FriendsOfFriends
        );
    }

    #[test]
    fn owner_addr_from_identity_pub_matches_harmony_address_hash() {
        // The helper MUST agree with harmony_identity's own address_hash for
        // the same identity — it's the binding `apply_friend_update` checks
        // friend_owner_pub against. Mirrors dm_signing's equivalent pin.
        let private = harmony_identity::PrivateIdentity::from_seed(&[0x5a; 32]);
        let public = private.public_identity();
        let pub_bytes = public.to_public_bytes();
        let derived = owner_addr_from_identity_pub(&pub_bytes).expect("valid pub derives");
        assert_eq!(derived, OwnerAddr(public.address_hash));
        // Note: we deliberately do NOT assert that `[0u8; 64]` returns `None`.
        // Per the documented quirk in `dm_signing` (see the dropped
        // "malformed_rejects" note there), the harmony-pinned
        // `Identity::from_public_bytes(&[0u8; 64])` actually SUCCEEDS, so the
        // `None` branch is unreachable with trivially-constructed inputs. The
        // load-bearing property — that the helper agrees with harmony's own
        // `address_hash` — is what `apply_friend_update` relies on, and that
        // is what we pin above.
    }

    #[test]
    fn friend_entry_rejects_oversized_display() {
        // An entry with a display string just over the cap must FAIL to decode
        // (rejected, not truncated). We encode it directly (serialize has no
        // cap) and assert the strict deserializer rejects it.
        let oversized = "x".repeat(MAX_FRIEND_DISPLAY_LEN + 1);
        let mut e = sample_entry();
        e.display = Some(oversized);
        let bytes = canonical_cbor_encode(&e).expect("encode (serialize is uncapped)");
        let decoded = canonical_cbor_decode::<FriendEntry>(&bytes);
        assert!(
            decoded.is_err(),
            "oversized display must be rejected at deserialize"
        );

        // A display at exactly the cap is still accepted.
        let mut at_cap = sample_entry();
        at_cap.display = Some("y".repeat(MAX_FRIEND_DISPLAY_LEN));
        let bytes = canonical_cbor_encode(&at_cap).expect("encode");
        let back: FriendEntry = canonical_cbor_decode(&bytes).expect("at-cap display decodes");
        assert_eq!(at_cap, back);
    }
}
