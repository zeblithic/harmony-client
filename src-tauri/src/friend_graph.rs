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
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    #[serde(rename = "n", skip_serializing_if = "Option::is_none", default)]
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
}
