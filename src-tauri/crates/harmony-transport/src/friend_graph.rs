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
//! all single-char keys ("m","n","s","v","r","l"); `FriendGraph` uses "f".
//!
//! ## Identity model (corrected 2026-06-03, spec §3)
//!
//! Friends are keyed on the friend's master **`owner_id`** (16 bytes) =
//! `PubKeyBundle::identity_hash()` — the SAME principal used by `self_owner`,
//! community membership, profile cards, and enrollment certs. An earlier draft
//! wrongly keyed friends on the Reticulum/DM combined-pub `Identity::address_hash`
//! (`SHA256(X25519‖Ed25519)[:16]`), a different identity used only for DM
//! transport routing. `FriendEntry` therefore stores the friend's master
//! Ed25519 verify key, and the map key MUST equal
//! `PubKeyBundle::classical_only(master_ed25519).identity_hash()` (v1; no PQ).

use crate::owner_state_types::{
    deserialize_bytes_from_bstr, serialize_bytes_as_bstr, Hlc, OwnerAddr,
};
use harmony_owner::pubkey_bundle::PubKeyBundle;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

/// Upper bound on `FriendEntry.display` length, enforced at deserialize time
/// (see [`deserialize_capped_display`]). A malformed/oversized wire entry is
/// REJECTED rather than truncated — matching this codebase's strict-deserialize
/// culture (cf. `OwnerDeviceEntry`). The serialized shape of valid values is
/// unchanged. 256 bytes is generous for a human display name.
pub const MAX_FRIEND_DISPLAY_LEN: usize = 256;

/// Derive the friend's 16-byte master `OwnerAddr` (their `owner_id`) from their
/// 32-byte master Ed25519 verify key.
///
/// Single source of truth: delegates to
/// `PubKeyBundle::classical_only(master_ed25519).identity_hash()` — exactly the
/// `owner_id` derivation `self_owner`, community membership, and enrollment
/// certs use (`harmony-owner/pubkey_bundle.rs`; cf. the `classical_only(..)
/// .identity_hash()` device-id derivation at `lib.rs:2651`). v1 has no
/// post-quantum key, so a Master `EnrollmentCert`'s `owner_id` equals this for
/// the same master key — the friend-graph key invariant holds by construction.
/// Never re-derive the hash formula here; if harmony changes its scheme we
/// follow automatically rather than silently diverge.
///
/// Infallible: any 32-byte value is a valid input to `identity_hash` (the
/// classical bundle hashes the raw bytes; no point-decoding is performed).
///
/// `pub` so the ZEB-370 wire-format pinning fixtures (`tests/`) can derive the
/// map key (the friend's `owner_id`) for a `FriendEntry` from a fixed master
/// Ed25519 key — exactly the binding `apply_friend_update` enforces.
pub fn owner_id_from_master_ed25519(master_ed25519: &[u8; 32]) -> OwnerAddr {
    OwnerAddr(PubKeyBundle::classical_only(*master_ed25519).identity_hash())
}

/// Strict deserializer for `FriendEntry.display`: rejects (does NOT truncate)
/// any string longer than [`MAX_FRIEND_DISPLAY_LEN`]. The field stays
/// `Option<String>`; a present value over the cap is a hard decode error so a
/// malformed/oversized wire entry never enters the CRDT. `None`/absent is
/// passed through unchanged, preserving the `skip_serializing_if` wire shape.
///
/// `pub(crate)` so the friend-handshake wire types (`FriendLinkRequest` /
/// `FriendLinkAccepted` in `iroh_friend_acceptor`) can enforce the SAME cap at
/// network ingress — an authenticated peer must not be able to push an
/// oversized `display` through the handshake into a `FriendEntry` that then
/// fails to deserialize on the owner's other devices during owner-state sync.
pub fn deserialize_capped_display<'de, D>(d: D) -> Result<Option<String>, D::Error>
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

/// One friend, keyed in `FriendGraph.friends` by the friend's master
/// `OwnerAddr` (their `owner_id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendEntry {
    /// Friend's 32-byte master Ed25519 verify key. The map key (their
    /// `owner_id`) MUST equal
    /// `owner_id_from_master_ed25519(&master_ed25519)` (v1; no PQ) — enforced
    /// in `apply_friend_update`. This anchors the friend to the SAME principal
    /// as their community/profile identity. Stored as a CBOR bstr(32).
    #[serde(
        rename = "m",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub master_ed25519: [u8; 32],
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
    /// ZEB-371: the per-friendship rendezvous secret, KeyTree-sealed
    /// (`owner_state_crypto::encrypt_friend_secret`). `None` for legacy Phase-1
    /// entries and for `Pending`/`Revoked` entries. Opaque bytes; never stored
    /// or logged in the clear. Stored as a CBOR bstr (major type 2) per spec §3.3.
    #[serde(
        rename = "k",
        with = "serde_bytes",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub sealed_secret: Option<Vec<u8>>,
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

/// Test helper: a minimal ACTIVE [`FriendEntry`]. ZEB-236's tier fork
/// auto-accepts DM invites only from active friends, so receive-path tests that
/// exercise the auto-accept/bootstrap branch must seed a friendship. The
/// `master_ed25519` is a placeholder — the tier check reads only `status`, and
/// these tests insert directly into `friends` (bypassing `apply_friend_update`'s
/// key↔master gate, since their `OwnerAddr`s are not master-derived).
#[cfg(any(test, feature = "test-fixtures"))]
pub fn active_friend_entry_for_test(learned_wall_ms: u64) -> FriendEntry {
    FriendEntry {
        master_ed25519: [0u8; 32],
        display: None,
        status: FriendStatus::Active,
        established_via: FriendOrigin::Token,
        referrable: false,
        learned_at: crate::owner_state_types::Hlc {
            wall_ms: learned_wall_ms,
            logical: 0,
            device_id: "friend-fixture".into(),
        },
        sealed_secret: None,
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
            master_ed25519: [0x11; 32],
            display: Some("alice".into()),
            status: FriendStatus::Active,
            established_via: FriendOrigin::Token,
            referrable: false,
            learned_at: hlc(7),
            sealed_secret: None,
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
    fn owner_id_from_master_ed25519_matches_master_cert_owner_id() {
        // The helper MUST agree with the master `owner_id` derivation used by
        // enrollment certs and `self_owner` — it's the binding
        // `apply_friend_update` checks `master_ed25519` against. For a v1
        // Master cert (no PQ), `cert.owner_id` is exactly
        // `PubKeyBundle::classical_only(master_ed25519).identity_hash()`, so a
        // friend learned from such a cert satisfies the key invariant by
        // construction.
        let owner = crate::enrollment_verify::quorum_fixtures::mint_test_owner(0x5a);
        let master_ed25519 =
            if let harmony_owner::certs::EnrollmentIssuer::Master { master_pubkey } =
                &owner.cert.issuer
            {
                master_pubkey.classical.ed25519_verify
            } else {
                panic!("mint_test_owner produces a Master cert");
            };
        let derived = owner_id_from_master_ed25519(&master_ed25519);
        assert_eq!(derived, OwnerAddr(owner.cert.owner_id));
        assert_eq!(derived, owner.owner);
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

    #[test]
    fn friend_entry_sealed_secret_round_trips() {
        let mut e = sample_entry();
        e.sealed_secret = Some(vec![0xCD; 12 + 32 + 16]); // fixed opaque blob
        let bytes = canonical_cbor_encode(&e).expect("encode");
        let back: FriendEntry = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(e, back);
    }

    #[test]
    fn friend_entry_absent_sealed_secret_decodes_as_none() {
        let e = sample_entry(); // sample_entry sets sealed_secret: None
        let bytes = canonical_cbor_encode(&e).expect("encode");
        let back: FriendEntry = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(back.sealed_secret, None);
    }
}
