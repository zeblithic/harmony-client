//! Community invite payload types — ZEB-217 Sub-C Phase 1.
//!
//! Phase 1 ships ONLY the type definitions + canonical CBOR. Encoding
//! to a `harmony://invite/...` URL (base64url + URL prefix) lives in
//! Phase 3 alongside the `generate_invite` IPC. Reticulum send/receive
//! for invite-only counter-sig flow lives in Phase 4.
//!
//! See `docs/specs/2026-05-05-zeb-217-sub-c-communities-design.md`
//! §"Invite system".

use serde::{Deserialize, Serialize};

use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::{
    deserialize_bytes_from_bstr, serialize_bytes_as_bstr, Hlc, OwnerAddr, SpaceId,
};

/// ZEB-249: A snapshot of the community at invite issuance, bound to
/// the invitee via X25519-sealed EpochKey. Carried inside
/// `CommunityInvitePayload.epoch_snapshot`.
///
/// `state_snapshot` is a UI bootstrap hint — CRDT replay post-redemption
/// is the source of truth (spec §5.2 + §10.3).
///
/// For open-community invites (no specific invitee), `sealed_epoch_key`
/// carries the raw 32-byte EpochKey unencrypted (the key is "public"
/// for open communities — anyone with the link can join and receive it).
/// For invite-only flows (Phase 4+), it carries the X25519-sealed key
/// (92 bytes: 32 ephemeral_pub + 12 nonce + 32 ct + 16 tag).
///
/// Spec §5.1 + §7.3. Field keys (ep, sk, ss) are 2-char.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteEpochSnapshot {
    #[serde(rename = "ep")]
    pub epoch: u64,

    /// EpochKey delivery bytes. For open communities: 32 raw bytes.
    /// For invite-only (Phase 4+): 92-byte X25519-sealed envelope
    /// (32 ephemeral_pub + 12 nonce + 32 ct + 16 tag).
    #[serde(
        rename = "sk",
        serialize_with = "crate::owner_state_types::serialize_vec_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_vec_from_bstr"
    )]
    pub sealed_epoch_key: Vec<u8>,

    /// ZEB-369: targeted invite-only invites seal the epoch key to EACH of the
    /// invitee's enrolled device-#2 X25519 keys — one 92-byte envelope per
    /// device — so the invitee can redeem on any bound device. Empty for open +
    /// untargeted invites (those carry the single `sealed_epoch_key`);
    /// `skip_serializing_if` keeps their encoded wire byte-identical to
    /// pre-ZEB-369 snapshots, and `default` lets those old snapshots decode with
    /// this field empty. When non-empty, redemption tries each envelope with
    /// `ed25519_priv_to_x25519(device_sk)` until one opens, and
    /// `sealed_epoch_key` is left empty.
    #[serde(
        rename = "se",
        default,
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "crate::owner_state_types::serialize_vec_of_vec_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_vec_of_vec_from_bstr"
    )]
    pub sealed_epoch_keys: Vec<Vec<u8>>,

    #[serde(rename = "ss")]
    pub state_snapshot: MaterializedCommunityState,
}

/// Materialized state snapshot for UI bootstrap on join. Spec §5.1.
/// Field keys (mb, ch, pl) are 2-char.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializedCommunityState {
    #[serde(rename = "mb")]
    pub members: std::collections::BTreeMap<
        crate::owner_state_types::OwnerAddr,
        crate::community_membership::MemberState,
    >,

    #[serde(
        rename = "ch",
        default,
        skip_serializing_if = "std::collections::BTreeMap::is_empty"
    )]
    pub channels: std::collections::BTreeMap<
        crate::community_membership::ChannelId,
        crate::community_membership::ChannelInfo,
    >,

    #[serde(rename = "pl")]
    pub power_levels: std::collections::BTreeMap<crate::owner_state_types::OwnerAddr, u8>,
}

impl CanonicalPayloadSealed for InviteEpochSnapshot {}
impl CanonicalPayload for InviteEpochSnapshot {}
impl CanonicalPayloadSealed for MaterializedCommunityState {}
impl CanonicalPayload for MaterializedCommunityState {}

/// The full payload an invite link carries. Encoded as canonical CBOR
/// (~120-180 bytes), then base64url-encoded into the URL form
/// `harmony://invite/{base64url}` (encoding helpers land in Phase 3).
///
/// Wire format: 7-key map. Field codes are 2 chars to satisfy the
/// same-length-keys CBOR invariant at this nesting level. Optional
/// fields use skip_serializing_if so non-applicable variants
/// (e.g., open communities have invite_token=None) don't bloat the
/// encoded URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityInvitePayload {
    #[serde(rename = "ci")]
    pub community_id: SpaceId,

    /// ZEB-249: replaces v1's flat `membership_key: EpochKey` field.
    /// Carries the epoch number, invitee-bound EpochKey delivery bytes,
    /// and frozen materialized state for UI bootstrap.
    #[serde(rename = "es")]
    pub epoch_snapshot: InviteEpochSnapshot,

    #[serde(rename = "ad")]
    pub admin_addr: OwnerAddr,

    #[serde(rename = "nm")]
    pub community_name: String,

    #[serde(rename = "io")]
    pub is_invite_only: bool,

    #[serde(rename = "ex", skip_serializing_if = "Option::is_none", default)]
    pub expires_at: Option<Hlc>,

    /// Required for invite-only redemption (carries the inviter's
    /// pre-signed authorization). Optional for open communities (could
    /// still be present as an authenticity hint, but not required).
    #[serde(rename = "tk", skip_serializing_if = "Option::is_none", default)]
    pub invite_token: Option<InviteToken>,

    /// Admin's signed self-Join (their bootstrap event). Required for
    /// invite-only payloads (ZEB-260): without this the joiner's empty
    /// CRDT cannot admit the admin's eventual publish-back, because the
    /// receive-side membership-at-HLC gate evaluates publisher status
    /// against the joiner's local prefix (which has no admin entry).
    /// Joiner's `redeem_invite_inner` verifies this against
    /// `admin_identity_pub` and inserts via
    /// `engine.insert_local_event_with_pubs` before sending the unicast
    /// packet — the publish-back is generated strictly later, so this
    /// closes the race by construction. Open communities ignore this
    /// field; encoding stays byte-identical via skip_serializing_if.
    #[serde(rename = "ab", skip_serializing_if = "Option::is_none", default)]
    pub admin_bootstrap: Option<crate::community_membership::SignedMembershipEvent>,

    /// Admin's 64-byte identity_pub (X25519_pub(32) || Ed25519_pub(32),
    /// matching `harmony_identity::Identity::to_public_bytes()`). Required
    /// (present) for invite-only payloads (ZEB-260). ZEB-339: this is the
    /// admin's RETICULUM transport pub; it is NO LONGER used to verify
    /// `admin_bootstrap` (that now goes through the admin's EnrollmentCert
    /// carried on the bootstrap event — see `verify_admin_bootstrap`), and
    /// the old `address_hash(admin_identity_pub) == admin_addr` binding does
    /// not hold under the owner/device split (admin_addr is the owner_id /
    /// master hash). Still threaded into `insert_local_event_with_pubs`,
    /// which ignores it post-ZEB-339.
    #[serde(
        rename = "ap",
        skip_serializing_if = "Option::is_none",
        default,
        serialize_with = "serialize_admin_identity_pub_as_bstr",
        deserialize_with = "deserialize_admin_identity_pub_from_bstr"
    )]
    pub admin_identity_pub: Option<[u8; 64]>,

    /// ZEB-285: SpaceId of the community this one was forked from.
    /// Mirrors CommunityState.forked_from; carried in the invite so
    /// joiners can mirror it into their local CommunityState during
    /// redeem_invite_inner. None for non-fork invites. Byte-compatible
    /// with pre-ZEB-285 invites when None.
    #[serde(rename = "ff", skip_serializing_if = "Option::is_none", default)]
    pub forked_from: Option<SpaceId>,

    /// ZEB-285: frozen snapshot of the forker's pre-fork view of the
    /// ORIGINAL community. Present only on fork-invites (None for normal
    /// community invites). Bounded by snapshot policy (§4.2). Joiner
    /// stores the snapshot in the fork's data dir keyed by the original
    /// SpaceId for dual-keyset verification of pre-fork events.
    /// Byte-compatible with pre-ZEB-285 invites when None.
    #[serde(rename = "fs", skip_serializing_if = "Option::is_none", default)]
    pub pre_fork_snapshot: Option<PreForkSnapshot>,

    /// ZEB-339: the inviter's Master EnrollmentCert, so a joiner who has not
    /// yet synced the community log can verify the inviter's owner->device
    /// binding (and thus the InviteToken signature) at first contact.
    #[serde(rename = "ec", skip_serializing_if = "Option::is_none", default)]
    pub inviter_enrollment: Option<harmony_owner::certs::EnrollmentCert>,

    /// ZEB-367 untargeted invite-only only: the ephemeral X25519 private key the
    /// redeemer uses to open `epoch_snapshot.sealed_epoch_key`. Rides ONLY in the
    /// URL — never in the case-A pkarr record (which publishes routing keyed by
    /// token.sig) and OUTSIDE the token-sig preimage. `None` for targeted + open
    /// invites. Guarded so it can appear only on an untargeted invite-only payload.
    #[serde(
        rename = "ud",
        skip_serializing_if = "Option::is_none",
        default,
        serialize_with = "crate::owner_state_types::serialize_optional_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_optional_bytes_from_bstr"
    )]
    pub untargeted_decrypt_key: Option<[u8; 32]>,

    /// ZEB-677: Master-issued signer certs backing a Quorum-issued
    /// `inviter_enrollment`. Empty for Master-issued certs (key omitted on
    /// the wire; old redeemers ignore it and keep rejecting quorum-certed
    /// inviters). The admin-bootstrap and join-event bundles ride inside
    /// their own `SignedMembershipEvent.signer_certs` instead.
    #[serde(rename = "eb", default, skip_serializing_if = "Vec::is_empty")]
    pub inviter_signer_certs: Vec<harmony_owner::certs::EnrollmentCert>,
}

/// The inviter's pre-signed authorization, embedded in the invite link
/// for invite-only communities. The redeemer presents this via
/// Reticulum to any community member with `power ≥ invite_threshold`,
/// who counter-signs the resulting Join event (Phase 4).
///
/// `sig` covers the canonical-CBOR encoding of `(inviter, invitee_hint,
/// minted_at, expires_at_in_outer_payload)` — bound to the outer
/// CommunityInvitePayload's expires_at so a token can't be replayed
/// past its outer expiry. (Sig construction lives in Phase 3 with
/// `generate_invite`.)
///
/// Wire format: up to 5-key map (`xa` and `ih` skipped when `None`).
/// Field codes 2 chars per the same-length-keys rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteToken {
    #[serde(rename = "iv")]
    pub inviter: OwnerAddr,

    /// `None` = open redemption (anyone with the link can use this
    /// token). `Some(addr)` = bound to that owner addr; the joiner's
    /// signed Join.actor MUST equal this hint or verification rejects.
    #[serde(rename = "ih", skip_serializing_if = "Option::is_none", default)]
    pub invitee_hint: Option<OwnerAddr>,

    #[serde(rename = "mt")]
    pub minted_at: Hlc,

    /// Wall-clock ms past which the receiver MUST reject this token.
    /// `None` = no expiry (open-ended). Bound into the InviteToken
    /// signature via `canonical_invite_token_bytes` so the inviter's
    /// signature commits to the expiry value — an attacker can't strip
    /// `expires_at` post-mint to extend the redemption window.
    /// (Spec §verify-step-h.) Per the spec, `verify_packet_pure` rejects
    /// when `signed.created_at.wall_ms >= expires_at`.
    #[serde(rename = "xa", skip_serializing_if = "Option::is_none", default)]
    pub expires_at: Option<u64>,

    #[serde(
        rename = "sg",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],
}

impl CanonicalPayloadSealed for CommunityInvitePayload {}
impl CanonicalPayload for CommunityInvitePayload {}
impl CanonicalPayloadSealed for InviteToken {}
impl CanonicalPayload for InviteToken {}

use crate::community_membership::SignedMembershipEvent;
use crate::owner_state_types::DeviceIdentityHash;

/// ZEB-262 Phase 4: Reticulum unicast packet body sent from joiner →
/// counter-signer. Mirrors `dm_envelope::DmInviteSigned`'s Path B app-
/// sig binding shape: the signing_device_hash is INSIDE the signed body
/// so an attacker can't swap which device claims authorship without
/// invalidating the signature, and joiner_identity_pub rides along
/// inline because the receiver doesn't yet have an OwnerDeviceCache
/// entry for the joiner (bootstrap-only).
///
/// Wire format: 6-key map. Field codes are 2 chars to satisfy the
/// same-length-keys CBOR invariant at this nesting level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityInviteSigned {
    /// The community being joined.
    #[serde(rename = "ci")]
    pub community_id: SpaceId,

    /// The joiner's signed Join event WITHOUT countersig. Counter-sig
    /// is applied by the receiver (after verification) via
    /// `community_membership::attach_countersig_with_identity`.
    #[serde(rename = "je")]
    pub join_event: SignedMembershipEvent,

    /// The InviteToken from the URL payload — proves the inviter
    /// authorized this redemption.
    #[serde(rename = "it")]
    pub invite_token: InviteToken,

    /// Joiner's full 64-byte identity public bytes
    /// (`X25519_pub(32) || Ed25519_pub(32)` per
    /// `harmony_identity::Identity::to_public_bytes()`). Bootstrap-only
    /// — receiver doesn't yet have an OwnerDeviceCache entry for the
    /// joiner. Mirrors DmInviteSigned.inviter_identity_pub. Wire form:
    /// CBOR bstr(64).
    #[serde(
        rename = "ip",
        serialize_with = "serialize_identity_pub_as_bstr",
        deserialize_with = "deserialize_identity_pub_from_bstr"
    )]
    pub joiner_identity_pub: [u8; 64],

    /// Joiner's DeviceIdentityHash. Receiver verifies that
    /// SHA256(joiner_identity_pub)[..16] == signing_device_hash.0
    /// (defense-in-depth against a buggy sender pairing pubs with the
    /// wrong device claim). Mirrors DmInvite's signing_device_hash.
    #[serde(rename = "dh")]
    pub signing_device_hash: DeviceIdentityHash,

    /// Wall-clock at packet creation. Used for staleness checks against
    /// `invite_token` (carried via outer `InviteToken.minted_at` and
    /// the outer `CommunityInvitePayload.expires_at`). Also used for
    /// clock-skew rejection (created_at.wall_ms > now + 60s).
    #[serde(rename = "ca")]
    pub created_at: Hlc,
}

impl CanonicalPayloadSealed for CommunityInviteSigned {}
impl CanonicalPayload for CommunityInviteSigned {}

/// Tokenless open-community join request — a sibling of
/// [`CommunityInviteSigned`] on the same `HARMONY_HANDSHAKE_V1` ALPN.
/// Carries the joiner's self-signed Join (with its enrollment cert inside
/// `join_event.enrollment`) plus the link-capability proof
/// (`epoch_auth` + `nonce`) in place of an invite token.
///
/// Wire format: 7-key map. Field codes are 2 chars to satisfy the
/// same-length-keys CBOR invariant at this nesting level, exactly like
/// [`CommunityInviteSigned`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenJoinRequest {
    /// The community being joined.
    #[serde(rename = "ci")]
    pub community_id: SpaceId,

    /// The joiner's signed Join event WITHOUT countersig. Open-community
    /// joins are admitted via `bootstrap_admit_open_publisher` (no
    /// counter-sig); the embedded `join_event.enrollment` carries the
    /// joiner's master-signed EnrollmentCert.
    #[serde(rename = "je")]
    pub join_event: SignedMembershipEvent,

    /// Joiner's full 64-byte identity public bytes
    /// (`X25519_pub(32) || Ed25519_pub(32)`). Bootstrap-only — the beacon
    /// doesn't yet have an OwnerDeviceCache entry for the joiner. Wire
    /// form: CBOR bstr(64).
    #[serde(
        rename = "ip",
        serialize_with = "serialize_identity_pub_as_bstr",
        deserialize_with = "deserialize_identity_pub_from_bstr"
    )]
    pub joiner_identity_pub: [u8; 64],

    /// Joiner's DeviceIdentityHash.
    #[serde(rename = "dh")]
    pub signing_device_hash: DeviceIdentityHash,

    /// Link-capability MAC proving the joiner holds the community
    /// `epoch_key` (minted via `open_join_auth::mint_epoch_auth`). The
    /// beacon recomputes and rejects on mismatch. Wire form: CBOR
    /// bstr(32).
    #[serde(
        rename = "ea",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub epoch_auth: [u8; 32],

    /// Per-request nonce bound into `epoch_auth`; the beacon's replay
    /// cache rejects repeats. Wire form: CBOR bstr(16).
    #[serde(
        rename = "no",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub nonce: [u8; 16],

    /// Wall-clock at request creation. Bound into `epoch_auth` and used
    /// by the beacon for freshness/clock-skew rejection.
    #[serde(rename = "ca")]
    pub created_at: Hlc,
}

impl CanonicalPayloadSealed for OpenJoinRequest {}
impl CanonicalPayload for OpenJoinRequest {}

/// Beacon → joiner response to an [`OpenJoinRequest`]. Either admits the
/// joiner and ships the current membership snapshot, or rejects with a
/// human-readable reason. Shared by the beacon-side admit path (Task 6)
/// and the joiner-side dial path (Task 10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpenJoinResponse {
    /// Admitted: the membership snapshot the joiner should apply to
    /// bootstrap-sync the community.
    Admitted {
        member_events: Vec<SignedMembershipEvent>,
    },
    /// Rejected with a reason tag (mirrors the beacon-side
    /// `OpenJoinReject` discriminant name).
    Rejected { reason: String },
}

// =====================================================================
// ZEB-285 Phase 1 — PreForkSnapshot + BoundedChannelLogSnapshot
//
// Frozen history bundle carried in fork-invites. See
// docs/specs/2026-05-14-zeb-285-phase1-community-forking-design.md §3.4.
// =====================================================================

/// ZEB-285: serialize BTreeMap<OwnerAddr, [u8; 64]> as a CBOR map
/// where keys are bstr(16) (OwnerAddr) and values are bstr(64).
/// Called via serde's `serialize_with` on `PreForkSnapshot.identity_pubs`.
fn serialize_identity_pubs_map<S: serde::Serializer>(
    map: &std::collections::BTreeMap<OwnerAddr, [u8; 64]>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;

    /// Local newtype that serializes a byte slice as CBOR bstr without
    /// requiring the `serde_bytes` dep. Used by `serialize_identity_pubs_map`.
    struct BstrBytes<'a>(&'a [u8]);
    impl serde::Serialize for BstrBytes<'_> {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_bytes(self.0)
        }
    }

    let mut m = serializer.serialize_map(Some(map.len()))?;
    for (addr, pub_bytes) in map {
        m.serialize_entry(&BstrBytes(&addr.0), &BstrBytes(pub_bytes))?;
    }
    m.end()
}

/// ZEB-285: deserialize CBOR map of bstr(16) → bstr(64) into
/// `BTreeMap<OwnerAddr, [u8; 64]>`. Paired with `serialize_identity_pubs_map`.
fn deserialize_identity_pubs_map<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<std::collections::BTreeMap<OwnerAddr, [u8; 64]>, D::Error> {
    use serde::de::{MapAccess, Visitor};

    /// DeserializeSeed that reads a bstr of exactly N bytes into `[u8; N]`.
    struct BytesSeed<const N: usize>;
    impl<'de, const N: usize> serde::de::DeserializeSeed<'de> for BytesSeed<N> {
        type Value = [u8; N];
        fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<[u8; N], D::Error> {
            struct Vis<const N: usize>;
            impl<'de, const N: usize> Visitor<'de> for Vis<N> {
                type Value = [u8; N];
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    write!(f, "a {N}-byte CBOR byte string")
                }
                fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<[u8; N], E> {
                    v.try_into()
                        .map_err(|_| E::custom(format!("expected {N}-byte bstr, got {}", v.len())))
                }
                fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<[u8; N], E> {
                    self.visit_bytes(&v)
                }
            }
            d.deserialize_bytes(Vis::<N>)
        }
    }

    struct MapVisitor;
    impl<'de> Visitor<'de> for MapVisitor {
        type Value = std::collections::BTreeMap<OwnerAddr, [u8; 64]>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(f, "a CBOR map of bstr(16) -> bstr(64)")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
            let mut result = std::collections::BTreeMap::new();
            while let Some(key_bytes) = access.next_key_seed(BytesSeed::<16>)? {
                let val: [u8; 64] = access.next_value_seed(BytesSeed::<64>)?;
                result.insert(OwnerAddr(key_bytes), val);
            }
            Ok(result)
        }
    }
    deserializer.deserialize_map(MapVisitor)
}

/// ZEB-285: bounded snapshot of an original community's channel-log
/// state at fork time. Wire format: 1-key CBOR map keyed by ChannelId.
/// Per-channel value is a Vec<SignedChannelEvent> bounded by the
/// snapshot policy (§4.2 of spec):
/// - most-recent N=500 messages per channel by HLC descending
/// - total capped at M=5000 messages across all channels with
///   proportional trim
///
/// **Phase 1 NOTE**: Channel events stored here are NOT signature-verified
/// at redemption time. They are rendered with a muted pre-fork treatment
/// under the trust assumption that the forker bundled honest history.
/// Phase 2 will add per-message signature verification via a
/// `verify_snapshot_channel_event` function (sibling to
/// `verify_snapshot_event` in `community_membership.rs`). See spec §4.4.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BoundedChannelLogSnapshot {
    /// Per-channel signed log events, frozen at fork time. Empty for
    /// channels with no posts (or omitted entirely if no channels
    /// have any posts). BTreeMap (not HashMap) for canonical-CBOR
    /// deterministic ordering.
    #[serde(rename = "pc")]
    pub per_channel: std::collections::BTreeMap<
        crate::community_membership::ChannelId,
        Vec<crate::community_channel_log::SignedChannelEvent>,
    >,
}

impl CanonicalPayloadSealed for BoundedChannelLogSnapshot {}
impl CanonicalPayload for BoundedChannelLogSnapshot {}

/// ZEB-287 Phase 2: one entry in a fork's ancestor chain. Frozen at the
/// time it was added to a fork's lineage; ancestor renames after this
/// do not propagate to descendants. Bundled into
/// `PreForkSnapshot.parent_lineage` and persisted in
/// `CommunityState.parent_lineage`.
///
/// Same-length-keys invariant: CBOR keys at this nesting level are all
/// 2-char (`si`, `nm`, `at`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentLineageEntry {
    /// SpaceId of this ancestor community.
    #[serde(rename = "si")]
    pub space_id: SpaceId,

    /// Display name of this ancestor at the time it was frozen.
    #[serde(rename = "nm")]
    pub name: String,

    /// wall_ms component of the Fork event that created THIS ancestor
    /// from its predecessor in the chain. `None` for the root (top of
    /// the chain — never forked, has no predecessor).
    #[serde(rename = "at", skip_serializing_if = "Option::is_none", default)]
    pub forked_at_wall_ms: Option<u64>,

    /// ZEB-649: the stated reason THIS ancestor was forked from its
    /// predecessor (parallel to `forked_at_wall_ms`). `None` for the root
    /// and for ancestors forked before ZEB-649. Byte-compatible (omitted
    /// when None).
    #[serde(rename = "rs", skip_serializing_if = "Option::is_none", default)]
    pub reason: Option<String>,
}

impl CanonicalPayloadSealed for ParentLineageEntry {}
impl CanonicalPayload for ParentLineageEntry {}

/// ZEB-285: frozen snapshot of an original community's history,
/// bundled into fork-invites so fork-invitees can see pre-fork
/// context. Self-contained for verification: `identity_pubs` carries
/// the owner-pubkeys needed to verify every signer in
/// `membership_events` and `channel_log`, so joiners do NOT need to
/// query profile-broadcast to verify the snapshot.
///
/// Wire format: 6-key CBOR map (7th `pl` key added in ZEB-287 Phase 2,
/// skipped when empty). Field codes 2-char per same-length-keys at this
/// nesting level. See spec §3.4 (Phase 1) and §3.2 (Phase 2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreForkSnapshot {
    /// The original community's SpaceId. Signed pre-fork events
    /// reference this SpaceId in their bodies; the dual-keyset
    /// verifier dispatches by this value.
    #[serde(rename = "oi")]
    pub original_community_id: SpaceId,

    /// Display name of the original community at fork time. Used for
    /// the fork's Lineage UI ("Forked from {name}").
    #[serde(rename = "on")]
    pub original_community_name: String,

    /// Membership-CRDT events from the original, signed against the
    /// original's keyset. Replayed at display time against
    /// `identity_pubs` for verification; not inserted into the fork's
    /// own CommunityState event log.
    #[serde(rename = "ev")]
    pub membership_events: Vec<crate::community_membership::SignedMembershipEvent>,

    /// Bounded channel-log snapshot per §4.2 policy.
    #[serde(rename = "cl")]
    pub channel_log: BoundedChannelLogSnapshot,

    /// Map from every OwnerAddr that signs any event in this snapshot
    /// to their 64-byte identity public bytes (X25519_pub(32) ||
    /// Ed25519_pub(32) matching Identity::to_public_bytes()).
    /// Required because fork members are NOT necessarily members of
    /// the original community, so OwnerDeviceCache won't have signers
    /// cached. Bundled inline so verification needs no external lookup.
    #[serde(
        rename = "ip",
        serialize_with = "serialize_identity_pubs_map",
        deserialize_with = "deserialize_identity_pubs_map"
    )]
    pub identity_pubs: std::collections::BTreeMap<OwnerAddr, [u8; 64]>,

    /// Forker's local HLC at fork time. Informational — used to
    /// render the "Fork point" divider in the fork's unified timeline.
    /// NOT used for any verification or ordering decision.
    #[serde(rename = "ts")]
    pub forked_at: Hlc,

    /// ZEB-287 Phase 2: ordered ancestor chain (root → immediate parent)
    /// frozen at fork-time. For a Phase 2 fork-invite built via
    /// `community_invite::build_parent_lineage`, the tail entry is the
    /// fork's immediate parent — the forker community itself — which is
    /// ALSO encoded via `original_community_id` / `original_community_name`.
    /// This duplication is intentional: it lets the redeemer mirror the
    /// chain into `CommunityState.parent_lineage` verbatim while keeping
    /// `original_community_id` as the canonical immediate-parent pointer
    /// for the Phase 1 `forked_from` path.
    ///
    /// Length capped at 16 entries at fork-build time (see
    /// `community_invite::apply_lineage_cap`). Phase 1 fork-invites
    /// encode without this field; decoded as empty Vec via `default`.
    #[serde(rename = "pl", skip_serializing_if = "Vec::is_empty", default)]
    pub parent_lineage: Vec<ParentLineageEntry>,

    /// ZEB-649: the forker's stated reason for creating the fork this
    /// snapshot belongs to. Mirrored into the joiner's
    /// `CommunityState.fork_reason` at redeem-time so fork members see
    /// the "why" in their lineage/divider UI. `None` for snapshots built
    /// before ZEB-649. Byte-compatible (omitted when None).
    #[serde(rename = "fr", skip_serializing_if = "Option::is_none", default)]
    pub fork_reason: Option<String>,
}

impl CanonicalPayloadSealed for PreForkSnapshot {}
impl CanonicalPayload for PreForkSnapshot {}

/// ZEB-287 Phase 2: spec §3.4 maximum depth for a fork's parent_lineage.
/// Applied at fork-build time (community_fork.rs) AND at redeem time
/// (lib.rs::redeem_invite_inner) to defend against future-protocol-revision
/// or malicious payloads that exceed the cap.
pub const MAX_LINEAGE_DEPTH: usize = 16;

/// ZEB-287 Phase 2: enforce the 16-deep cap on a parent_lineage vector by
/// dropping the OLDEST (root-side) entries until length ≤ MAX_LINEAGE_DEPTH.
/// Used by `build_parent_lineage` and by the redeem-path payload guard in
/// `lib.rs::redeem_invite_inner` (R1-2).
pub fn apply_lineage_cap(lineage: &mut Vec<ParentLineageEntry>) {
    if lineage.len() > MAX_LINEAGE_DEPTH {
        let overflow = lineage.len() - MAX_LINEAGE_DEPTH;
        lineage.drain(0..overflow);
    }
}

/// ZEB-287 Phase 2: build a new fork's parent_lineage by extending the
/// forker's existing chain with the forker's own community as the new
/// immediate-parent-above-the-immediate-parent. Mirrors spec §3.4.
///
/// Inputs:
/// - `forker_lineage`: the forker community's existing `parent_lineage`
///   (slice; cloned internally).
/// - `forker_id` / `forker_name`: the forker community's identity at
///   fork-time. Frozen into the new entry.
/// - `forker_forked_at_wall_ms`: the forker community's own
///   `forked_at_wall_ms` (Some when the forker is itself a fork; None
///   when the forker is a top-level / root community).
///
/// The new entry (`forker_id`, `forker_name`, `forker_forked_at_wall_ms`)
/// is appended; then the 16-deep cap is applied (drops oldest if needed).
///
/// Production callers: `community_fork.rs::fork_community` (Task 4 build
/// site). Tests in `community_fork_integration.rs` + `community_invite_unit.rs`
/// also use this helper so a regression in production logic surfaces there.
pub fn build_parent_lineage(
    forker_lineage: &[ParentLineageEntry],
    forker_id: SpaceId,
    forker_name: &str,
    forker_forked_at_wall_ms: Option<u64>,
    forker_fork_reason: Option<String>,
) -> Vec<ParentLineageEntry> {
    let mut chain: Vec<ParentLineageEntry> = forker_lineage.to_vec();
    chain.push(ParentLineageEntry {
        space_id: forker_id,
        name: forker_name.to_string(),
        forked_at_wall_ms: forker_forked_at_wall_ms,
        // ZEB-649: why the FORKER itself was forked from its own parent —
        // its own `fork_reason` — so reasons accumulate down the chain.
        // (The NEW fork's reason lives on the fork's state/snapshot, not
        // in this chain, until the fork is itself forked.)
        reason: forker_fork_reason,
    });
    apply_lineage_cap(&mut chain);
    chain
}

/// ZEB-262 Phase 4: Path B app-sig wrapper around CommunityInviteSigned.
/// Wire layout: `[u8 disc=0x10][CBOR(signed)][64 raw signature bytes]`.
/// The signature is 64 raw bytes appended after the CBOR body — same
/// pattern as `DmPacket` (NOT a CBOR bstr; encode appends via
/// `extend_from_slice`, decode splits via `split_at(len - 64)`).
///
/// Discriminant 0x10 is reserved for community packets per the spec
/// §"Wire format" (DM packets occupy 0x01-0x03; 0x10-0x1F reserved for
/// community packets; 0x20+ reserved for Sub-D directory packets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommunityInvitePacket {
    Invite {
        signed: CommunityInviteSigned,
        signature: [u8; 64],
        /// Captured at decode for re-verify. The signature covers
        /// `signed_bytes` exactly as transmitted, so signature
        /// verification operates on bit-exact bytes regardless of
        /// encoder drift. On send, encode_packet re-encodes from
        /// `signed`, asserts byte-equality with `signed_bytes`, and
        /// emits `signed_bytes` verbatim.
        signed_bytes: Vec<u8>,
    },
    /// Tokenless open-community join (discriminant 0x11). Sibling of
    /// `Invite` on the same ALPN; `signature` is the joiner's Ed25519 sig
    /// over canonical CBOR of `req`, captured as `signed_bytes` for
    /// bit-exact re-verify.
    OpenJoin {
        req: OpenJoinRequest,
        signature: [u8; 64],
        signed_bytes: Vec<u8>,
    },
}

/// Helper: serialize `[u8; 64]` as CBOR bstr (major type 2). Mirrors
/// dm_envelope::serialize_identity_pub_as_bstr — necessary because
/// serde's blanket `[T; N]: Serialize` only covers small N.
fn serialize_identity_pub_as_bstr<S>(b: &[u8; 64], s: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    s.serialize_bytes(b)
}

/// Helper: deserialize CBOR bstr(64) into `[u8; 64]`. Length is
/// enforced strictly; bstr of any length other than 64 is rejected.
/// Mirrors dm_envelope::deserialize_identity_pub_from_bstr.
fn deserialize_identity_pub_from_bstr<'de, D>(d: D) -> Result<[u8; 64], D::Error>
where
    D: serde::Deserializer<'de>,
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
                    "joiner_identity_pub must be 64 bytes, got {}",
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

/// Serialize `Option<[u8; 64]>` as a CBOR bstr (Some) or absent (None,
/// via `skip_serializing_if`). Mirrors the existing
/// `serialize_identity_pub_as_bstr` shape; wraps it for the optional
/// case used by `CommunityInvitePayload::admin_identity_pub`.
fn serialize_admin_identity_pub_as_bstr<S>(
    val: &Option<[u8; 64]>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    // skip_serializing_if = "Option::is_none" on the field guards None;
    // serde calls this only with Some.
    serializer.serialize_bytes(
        val.as_ref()
            .expect("skip_serializing_if guards None — unreachable"),
    )
}

/// Deserialize `Option<[u8; 64]>` from CBOR. The field is wrapped in
/// `Option` because invite-only payloads always set it but open-community
/// payloads omit it entirely; serde routes the absent-key case to
/// `default` (None) and the present-bstr case here. Uses
/// `deserialize_option` so CBOR null is handled gracefully (mirrors
/// `owner_state_types::OptPubVisitor`).
fn deserialize_admin_identity_pub_from_bstr<'de, D>(
    deserializer: D,
) -> Result<Option<[u8; 64]>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Visitor;
    use std::fmt;

    struct OptBytesVisitor;
    impl<'de> Visitor<'de> for OptBytesVisitor {
        type Value = Option<[u8; 64]>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "a 64-byte CBOR byte string or null")
        }

        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_bytes(self)
        }

        fn visit_bytes<E: serde::de::Error>(self, value: &[u8]) -> Result<Option<[u8; 64]>, E> {
            if value.len() != 64 {
                return Err(E::custom(format!(
                    "admin_identity_pub must be 64 bytes, got {}",
                    value.len()
                )));
            }
            let mut out = [0u8; 64];
            out.copy_from_slice(value);
            Ok(Some(out))
        }

        fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Option<[u8; 64]>, E> {
            self.visit_bytes(&v)
        }
    }

    deserializer.deserialize_option(OptBytesVisitor)
}

use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use base64::Engine;

const URL_PREFIX: &str = "harmony://invite/";

/// Errors decoding a `harmony://invite/...` URL into a
/// `CommunityInvitePayload`. Distinct variants per failure class so the
/// IPC layer can surface a precise diagnostic to the frontend (and a
/// future telemetry dashboard can tally each independently).
#[derive(thiserror::Error, Debug)]
pub enum InviteUrlError {
    #[error("invite URL scheme must be `harmony://invite/`, got `{0}`")]
    WrongScheme(String),
    #[error("base64url decode failed: {0}")]
    Base64(String),
    #[error("CBOR decode failed: {0}")]
    Cbor(String),
    /// Defends the CBOR decoder against unbounded input: a hostile
    /// paste of a multi-MB body would otherwise burn allocator + decode
    /// time before failing. A real invite is ~120-240 bytes base64; the
    /// cap is generous enough to absorb future field growth (§10.3
    /// materialized-state snapshot) without becoming a DoS vector.
    /// Measured in base64 characters of the body
    /// (post-`harmony://invite/` strip), NOT decoded bytes — 85 333
    /// base64 chars decode to exactly 64 KiB raw.
    #[error("invite payload exceeds 85 333 base64-char limit (got {0} chars)")]
    TooLarge(usize),
    /// Caller passed an invite-only payload missing the admin bootstrap
    /// fields (`admin_bootstrap` and/or `admin_identity_pub`). The reader
    /// would reject the resulting URL via `verify_admin_bootstrap`; we
    /// catch this at the writer to surface a clearer error and avoid
    /// shipping un-redeemable URLs. ZEB-260.
    #[error("invite-only payload missing admin_bootstrap or admin_identity_pub")]
    InviteOnlyMissingBootstrap,
    /// Caller populated `admin_bootstrap` or `admin_identity_pub` on an
    /// open-community payload. These fields are scoped to invite-only
    /// flows; encoding them on an open-community URL would leak admin's
    /// signed bootstrap event over a URL that doesn't need it. ZEB-260.
    #[error("open-community payload must not carry admin_bootstrap / admin_identity_pub")]
    OpenCommunityHasBootstrap,
    /// Caller passed an invite-only payload missing `invite_token`. The
    /// reader-side `redeem_invite_inner` would tear down the spawned
    /// engine and return `"invite-only payload missing invite_token"`;
    /// catching this at the writer prevents the un-redeemable URL from
    /// leaving the mint site. ZEB-260 PR #90 round-3 (CodeRabbit).
    #[error("invite-only payload missing invite_token")]
    InviteOnlyMissingToken,
    /// ZEB-339: invite-only payload missing `inviter_enrollment`. The
    /// joiner needs the inviter's Master EnrollmentCert to verify the
    /// inviter's owner->device binding (and thus the InviteToken signature)
    /// before it has synced the community log. Enforced at both encode and
    /// decode so an un-verifiable invite-only URL never leaves the mint site.
    #[error("invite-only payload missing inviter_enrollment")]
    InviteOnlyMissingInviterEnrollment,
    /// ZEB-339: invite-only payload whose `admin_bootstrap` is present but
    /// is missing its embedded EnrollmentCert (`admin_bootstrap.enrollment`).
    /// `verify_admin_bootstrap` resolves the bootstrap-Join's signer via
    /// `enrolled_key_from_cert`, which reads `admin_bootstrap.enrollment`;
    /// without it the URL encodes and decodes cleanly but fails late at
    /// redeem with `BootstrapSignatureInvalid` (a confusing signature error).
    /// Enforced at both encode and decode so an un-redeemable invite-only URL
    /// never leaves the mint site and is rejected clearly on receipt.
    #[error("invite-only payload's admin_bootstrap is missing its embedded enrollment cert")]
    InviteOnlyBootstrapMissingEnrollment,
    /// `epoch_snapshot.sealed_epoch_key` has the wrong byte length for
    /// the declared mode. Open communities must carry 32 raw bytes;
    /// invite-only flows must carry the 92-byte X25519-sealed envelope
    /// (32 ephemeral_pub + 12 nonce + 32 ct + 16 tag). Enforced at
    /// both encode and decode time so a badly-formed payload is caught
    /// as early as possible. ZEB-249 PR #106 R5 (CodeRabbit Major).
    #[error(
        "sealed_epoch_key length invalid for {mode} community: \
         expected {expected} bytes, got {got}"
    )]
    InvalidSealedEpochKeyLen {
        mode: &'static str,
        expected: usize,
        got: usize,
    },
    /// ZEB-367: `untargeted_decrypt_key` was set on a payload that is not an
    /// untargeted invite-only invite (i.e. open, or invite-only but targeted
    /// via `invite_token.invitee_hint == Some(_)`). This key seals the epoch
    /// key open to anyone holding the URL; allowing it on a targeted or open
    /// payload would leak the epoch key. Enforced at both encode and decode.
    #[error("untargeted_decrypt_key is only valid on an untargeted invite-only payload")]
    UntargetedKeyNotAllowed,
    /// ZEB-367: an untargeted invite-only payload (invite-only, `invite_token`
    /// present, `invitee_hint == None`) is MISSING its `untargeted_decrypt_key`.
    /// Such a URL encodes/decodes cleanly, but at redeem `mint_redemption` treats
    /// the absent key as the targeted path and derives the redeemer's own device-#2
    /// key — which never matches the ephemeral key the epoch was sealed to, so
    /// decryption fails late with a confusing "epoch key decryption failed". Caught
    /// at both encode and decode so the un-redeemable URL never leaves the mint site
    /// and is rejected clearly on receipt. Mirrors `UntargetedKeyNotAllowed`.
    #[error("untargeted invite-only payload is missing its untargeted_decrypt_key")]
    UntargetedKeyMissing,
    /// ZEB-369: a targeted invite-only payload (invite-only,
    /// `invite_token.invitee_hint == Some(_)`) carries its sealed epoch key in
    /// `epoch_snapshot.sealed_epoch_keys` (one X25519-sealed 92-byte envelope
    /// per invitee device) with `sealed_epoch_key` left empty. This error fires
    /// when that shape is violated: the per-device list is empty, holds more
    /// than `MAX_ENROLLED_DEVICE_KEYS` envelopes, holds an envelope that is not
    /// exactly 92 bytes, or `sealed_epoch_key` is non-empty alongside a
    /// populated list. Enforced at both encode and decode.
    #[error(
        "targeted invite-only payload has a malformed sealed_epoch_keys shape \
         (empty list, more than MAX_ENROLLED_DEVICE_KEYS envelopes, an envelope \
         that is not exactly 92 bytes, or a non-empty sealed_epoch_key set \
         alongside it)"
    )]
    InvalidSealedEpochKeysShape,
}

/// Hard cap on the base64url body length (post-prefix-strip, in base64
/// chars) we'll hand to the base64 + CBOR decoders.
///
/// v2 budget: `InviteEpochSnapshot` embeds `MaterializedCommunityState`
/// which grows linearly with community size. At ~500 members the CBOR
/// payload is roughly 40-50 KB, base64-encoded to ~60-70 KB. 85 333
/// base64 chars decodes to exactly 64 KiB raw — a comfortable ceiling
/// for moderate communities while still bounding the work done on
/// untrusted input. Open-community v1 payloads (~180 bytes, ~240 base64
/// chars) are well within this limit.
///
/// Greptile P2 on PR #87 round 2 flagged that the prior name "BYTES"
/// misled. See `InviteUrlError::TooLarge`.
///
/// Phase 1 cap targets ~2 MiB decoded payload (≈ 2_800_000 base64url chars).
/// Sized to fit realistic 5000-message snapshots inline. Phase 2 will
/// add content-addressed snapshot delivery via Zenoh BLOB transfer so
/// large snapshots ride out-of-band and this cap can return to a
/// stricter URL-friendly value.
pub const MAX_INVITE_BODY_B64_CHARS: usize = 2_800_000; // ≈ 2 MiB decoded (Phase 1)

/// Exact byte length of a single X25519-sealed epoch-key envelope
/// (32 ephemeral x25519 pubkey + 12 nonce + 32 ciphertext + 16 AEAD tag,
/// as produced by `seal_to_owner`). Each ZEB-369 per-device envelope must be
/// EXACTLY this length — `validate_sealed_epoch_key_shape` rejects anything
/// else so an untrusted invite URL can't carry an oversized ciphertext that
/// `open_from_owner` would then AEAD-decrypt in full.
const SEALED_ENVELOPE_LEN: usize = 92;

/// CR Minor (PR #106 R6): shared helper for the sealed-epoch-key shape
/// contract enforced at both the encode and decode boundary. Centralises the
/// mode labels and expected sizes so they can't drift independently.
///
/// Three valid shapes (ZEB-369 added the third):
/// - **Open community**: `sealed_epoch_key` is 32 raw bytes (EpochKey material,
///   no envelope overhead); `sealed_epoch_keys` empty.
/// - **Untargeted invite-only**: `sealed_epoch_key` is one 92-byte envelope;
///   `sealed_epoch_keys` empty.
/// - **Targeted invite-only** (`invite_token.invitee_hint == Some(_)`):
///   `sealed_epoch_key` empty; `sealed_epoch_keys` carries one fixed 92-byte
///   envelope per invitee device, at most MAX_ENROLLED_DEVICE_KEYS of them.
fn validate_sealed_epoch_key_shape(payload: &CommunityInvitePayload) -> Result<(), InviteUrlError> {
    let sealed_key_len = payload.epoch_snapshot.sealed_epoch_key.len();
    let sealed_keys = &payload.epoch_snapshot.sealed_epoch_keys;
    let is_targeted_invite_only = payload.is_invite_only
        && payload
            .invite_token
            .as_ref()
            .is_some_and(|t| t.invitee_hint.is_some());

    if is_targeted_invite_only {
        // Targeted: the single blob must be empty and the per-device list must
        // be non-empty, bounded to MAX_ENROLLED_DEVICE_KEYS entries, with each
        // envelope EXACTLY the fixed sealed-envelope length. An untrusted invite
        // URL must not be able to force oversized or unbounded AEAD decrypt work
        // during redemption (Qodo Bugs 1+2 — `open_from_owner` decrypts the whole
        // ciphertext slice, and one envelope is tried per device).
        if !sealed_key_len_is_empty_targeted(sealed_key_len)
            || sealed_keys.is_empty()
            || sealed_keys.len() > crate::community_membership::MAX_ENROLLED_DEVICE_KEYS
            || sealed_keys.iter().any(|e| e.len() != SEALED_ENVELOPE_LEN)
        {
            return Err(InviteUrlError::InvalidSealedEpochKeysShape);
        }
        return Ok(());
    }

    // Open + untargeted invite-only: the per-device list must be empty (it is
    // ONLY for the targeted shape) and the single blob carries the key.
    if !sealed_keys.is_empty() {
        return Err(InviteUrlError::InvalidSealedEpochKeysShape);
    }
    let (mode, expected): (&'static str, usize) = if payload.is_invite_only {
        ("invite-only", SEALED_ENVELOPE_LEN)
    } else {
        ("open", 32)
    };
    if sealed_key_len != expected {
        return Err(InviteUrlError::InvalidSealedEpochKeyLen {
            mode,
            expected,
            got: sealed_key_len,
        });
    }
    Ok(())
}

/// Targeted invites leave `sealed_epoch_key` empty (the per-device envelopes
/// live in `sealed_epoch_keys`). Tiny named predicate so the shape check above
/// reads cleanly.
#[inline]
fn sealed_key_len_is_empty_targeted(len: usize) -> bool {
    len == 0
}

/// ZEB-367: the presence of `untargeted_decrypt_key` and the payload's invite
/// shape must agree EXACTLY. The key may ride ONLY on an untargeted invite-only
/// payload (invite-only, `invite_token` present, `invitee_hint == None`):
///
/// - On an untargeted invite-only payload the key is REQUIRED — without it
///   `mint_redemption` falls back to the device-#2 (targeted) decrypt path and
///   fails late with "epoch key decryption failed" (`UntargetedKeyMissing`).
/// - On any other shape (open, or invite-only-but-targeted) the key is FORBIDDEN
///   — it seals the epoch key open to any URL holder and would leak it on a
///   payload whose confidentiality model assumes otherwise (`UntargetedKeyNotAllowed`).
///
/// Enforced identically at encode and decode so a structurally-inconsistent
/// invite-only URL never leaves the mint site and is rejected clearly on receipt.
fn validate_untargeted_decrypt_key_shape(
    payload: &CommunityInvitePayload,
) -> Result<(), InviteUrlError> {
    let is_untargeted_invite_only = payload.is_invite_only
        && payload
            .invite_token
            .as_ref()
            .is_some_and(|t| t.invitee_hint.is_none());
    match (
        is_untargeted_invite_only,
        payload.untargeted_decrypt_key.is_some(),
    ) {
        // untargeted invite-only WITH key, or any other shape WITHOUT key
        (true, true) | (false, false) => Ok(()),
        // key present on a targeted / open payload — would leak the epoch key
        (false, true) => Err(InviteUrlError::UntargetedKeyNotAllowed),
        // untargeted invite-only missing its URL-carried decrypt key
        (true, false) => Err(InviteUrlError::UntargetedKeyMissing),
    }
}

/// Canonical-CBOR-encode the payload, then base64url-no-pad the result,
/// and prefix `harmony://invite/`. The output is copy-paste-safe across
/// chat / email / messaging clients that munge `+`, `/`, or `=`.
pub fn encode_invite_url(payload: &CommunityInvitePayload) -> Result<String, InviteUrlError> {
    if payload.is_invite_only && payload.invite_token.is_none() {
        return Err(InviteUrlError::InviteOnlyMissingToken);
    }
    if payload.is_invite_only
        && (payload.admin_bootstrap.is_none() || payload.admin_identity_pub.is_none())
    {
        return Err(InviteUrlError::InviteOnlyMissingBootstrap);
    }
    // ZEB-339: invite-only payloads must carry the inviter's EnrollmentCert
    // so the joiner can verify the inviter's owner->device binding.
    if payload.is_invite_only && payload.inviter_enrollment.is_none() {
        return Err(InviteUrlError::InviteOnlyMissingInviterEnrollment);
    }
    // ZEB-339: the admin bootstrap-Join must embed the admin's EnrollmentCert,
    // because verify_admin_bootstrap resolves the signer via enrolled_key_from_cert
    // (which reads admin_bootstrap.enrollment). Without it the URL encodes and
    // decodes cleanly but dies at redeem with BootstrapSignatureInvalid — enforce
    // here so the un-redeemable URL never leaves the mint site.
    if payload.is_invite_only
        && payload
            .admin_bootstrap
            .as_ref()
            .is_some_and(|b| b.enrollment.is_none())
    {
        return Err(InviteUrlError::InviteOnlyBootstrapMissingEnrollment);
    }
    if !payload.is_invite_only
        && (payload.admin_bootstrap.is_some() || payload.admin_identity_pub.is_some())
    {
        return Err(InviteUrlError::OpenCommunityHasBootstrap);
    }
    // ZEB-249 PR #106 R5: enforce sealed-epoch-key shape contract BEFORE CBOR
    // encoding so a badly-formed payload is caught at the mint site and never
    // produces a URL that decode_invite_url would reject. CR Minor (PR #106 R6):
    // delegated to shared helper. ZEB-369: shape-aware (open / untargeted /
    // targeted invite-only).
    validate_sealed_epoch_key_shape(payload)?;
    // ZEB-367: confidentiality + redeemability invariant — the untargeted decrypt
    // key and the invite shape must agree exactly (key required on untargeted
    // invite-only, forbidden elsewhere). See validate_untargeted_decrypt_key_shape.
    validate_untargeted_decrypt_key_shape(payload)?;
    let cbor = canonical_cbor_encode(payload).map_err(|e| InviteUrlError::Cbor(e.to_string()))?;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&cbor);
    // Encode-time size check: fail fast rather than producing an invite URL
    // that decode_invite_url would immediately reject with TooLarge.
    if b64.len() > MAX_INVITE_BODY_B64_CHARS {
        return Err(InviteUrlError::TooLarge(b64.len()));
    }
    Ok(format!("{URL_PREFIX}{b64}"))
}

/// Strip the `harmony://invite/` prefix, base64url-decode, then
/// canonical-CBOR-decode into a `CommunityInvitePayload`.
///
/// Trims surrounding whitespace before scheme inspection — paste flows
/// (chat / email / messenger clients) routinely add leading or trailing
/// whitespace, and `harmony://invite/...\n` would otherwise fail with
/// `WrongScheme` for the trailing newline alone.
///
/// Caps the post-prefix body length at `MAX_INVITE_BODY_B64_CHARS`
/// (measured in base64 characters, not decoded bytes) to bound the work
/// the base64 + CBOR decoders do on untrusted input.
pub fn decode_invite_url(url: &str) -> Result<CommunityInvitePayload, InviteUrlError> {
    let url = url.trim();
    let body = url
        .strip_prefix(URL_PREFIX)
        .ok_or_else(|| InviteUrlError::WrongScheme(url.chars().take(URL_PREFIX.len()).collect()))?;
    if body.len() > MAX_INVITE_BODY_B64_CHARS {
        return Err(InviteUrlError::TooLarge(body.len()));
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body)
        .map_err(|e| InviteUrlError::Base64(e.to_string()))?;
    let payload = canonical_cbor_decode::<CommunityInvitePayload>(&bytes)
        .map_err(|e| InviteUrlError::Cbor(e.to_string()))?;
    // ZEB-249 PR #106 R5: enforce sealed-epoch-key shape contract AFTER decoding
    // so a tampered or malformed URL is rejected with a clear error rather than
    // silently producing a structurally valid but semantically broken payload.
    // CR Minor (PR #106 R6): delegated to shared helper. ZEB-369: shape-aware
    // (open / untargeted / targeted invite-only).
    validate_sealed_epoch_key_shape(&payload)?;
    // ZEB-339: invite-only payloads must carry the inviter's EnrollmentCert
    // (mirrors the admin_bootstrap / admin_identity_pub presence requirement).
    if payload.is_invite_only && payload.inviter_enrollment.is_none() {
        return Err(InviteUrlError::InviteOnlyMissingInviterEnrollment);
    }
    // ZEB-339: the admin bootstrap-Join must embed the admin's EnrollmentCert.
    // The joiner needs it to verify the admin's owner->device binding at redeem:
    // verify_admin_bootstrap resolves the signer via enrolled_key_from_cert
    // (which reads admin_bootstrap.enrollment). Without it the URL decodes
    // cleanly but dies at redeem with BootstrapSignatureInvalid — reject here
    // so a certless invite-only URL is rejected clearly on receipt.
    if payload.is_invite_only
        && payload
            .admin_bootstrap
            .as_ref()
            .is_some_and(|b| b.enrollment.is_none())
    {
        return Err(InviteUrlError::InviteOnlyBootstrapMissingEnrollment);
    }
    // ZEB-285 INVARIANT: forked_from and pre_fork_snapshot must both be
    // Some or both be None. An invite with one set but not the other is
    // malformed — reject it so joiners never enter a half-fork state.
    // Additionally, when both are Some, the snapshot's original_community_id
    // must match forked_from. (Fix: PR #122 round-2 bot review — CodeRabbit
    // Major.)
    match (&payload.forked_from, &payload.pre_fork_snapshot) {
        (Some(_), None) | (None, Some(_)) => {
            return Err(InviteUrlError::Cbor(
                "malformed fork-invite: forked_from and pre_fork_snapshot must both be present \
                 or both absent"
                    .to_string(),
            ));
        }
        (Some(ff), Some(snap)) if *ff != snap.original_community_id => {
            return Err(InviteUrlError::Cbor(
                "malformed fork-invite: forked_from does not match \
                 pre_fork_snapshot.original_community_id"
                    .to_string(),
            ));
        }
        _ => {} // (None, None) or matching (Some, Some) — valid
    }
    // ZEB-367: confidentiality + redeemability invariant — mirrors the encode-side
    // guard. A tampered/hostile URL that smuggles the untargeted key onto a
    // targeted/open payload (key leak), or an untargeted invite-only URL that omits
    // the key (un-redeemable), is rejected on receipt. See the shared helper.
    validate_untargeted_decrypt_key_shape(&payload)?;
    Ok(payload)
}

// =====================================================================
// ZEB-262 Phase 4 — packet codec + envelope-sig verify
//
// Mirrors `dm_envelope::encode_packet` / `decode_packet` /
// `build_signed_invite` exactly (see src-tauri/src/dm_envelope.rs:262-492).
// Wire layout: `[u8 disc=0x10][CBOR(signed)][64 raw signature bytes]`.
// =====================================================================

/// Errors produced by [`encode_packet`] / [`build_signed_invite_packet`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommunityInviteEncodeError {
    #[error("CBOR encode failed: {0}")]
    Cbor(String),
    /// Re-encoding `signed` to canonical CBOR failed inside encode_packet.
    /// build_signed_invite_packet already round-tripped this value through
    /// the same encoder, so this should be unreachable in practice — surface
    /// as a clear distinct variant so a regression here doesn't mask as a
    /// generic Cbor encode failure.
    #[error("re-encode signed body failed: {0}")]
    ReSerialize(String),
    /// encode_packet re-encoded `signed` and the result diverged from the
    /// cached `signed_bytes` field — the only way this fires is post-build
    /// mutation of the `signed` field. Mirrors
    /// `dm_envelope::EncodeError::SignedMutated`.
    #[error("signed body mutated post-build: {0}")]
    SignedMutated(String),
}

/// Errors produced by [`decode_packet`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommunityInviteDecodeError {
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

/// ZEB-262 Phase 4 receive-side rejection variants. Each maps to a
/// `community-state-sync-degraded` reason tag for the frontend banner.
///
/// Membership-state-dependent variants (`CommunityUnknown`,
/// `SelfNotJoined`, `SelfPowerInsufficient`) are defined here but
/// raised by `handle_unicast` in Task 9 — they require engine state
/// that isn't in scope for `verify_packet_pure`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommunityInviteVerifyError {
    /// Path B envelope sig didn't validate.
    #[error("envelope sig invalid")]
    EnvelopeSigInvalid,
    /// signing_device_hash != SHA256(joiner_identity_pub)[..16]. Caught
    /// at decode time but surfaced through this error type when the
    /// caller wants the unified reason tag.
    #[error("device hash mismatch")]
    DeviceHashMismatch,
    /// Inner Join event sig failed.
    #[error("Join event sig invalid")]
    JoinSigInvalid,
    /// InviteToken sig failed.
    #[error("InviteToken sig invalid")]
    InviteTokenSigInvalid,
    /// InviteToken.inviter != self_owner. v1 only counter-signs invites
    /// we issued. ZEB-251 broadens this to any joined member with
    /// power ≥ invite_threshold.
    #[error("invite signer mismatch: token says {signer:?}, we are {self_owner:?}")]
    InviteSignerMismatch {
        signer: crate::owner_state_types::OwnerAddr,
        self_owner: crate::owner_state_types::OwnerAddr,
    },
    /// community_id disagreement across envelope, Join, and token.
    #[error("community_id mismatch across envelope/Join/token")]
    CommunityIdMismatch,
    /// created_at >= invite_token expires_at, OR created_at > now + 60s.
    #[error("invite expired or clock-skew rejected")]
    Expired,
    /// `join_event.at.wall_ms` — the Join's OWN wall, distinct from the
    /// envelope's `created_at` above — is more than
    /// `clock_trust::MAX_FORWARD_SKEW_MS` ahead of the receiver's wall clock.
    /// This is the timestamp that actually lands in the persisted membership
    /// log, so it gets its own bound (ZEB-846 Task 7 — closes the gap left by
    /// Task 3's zenoh-merge-only `community_membership::verify_event`
    /// forward-skew reject).
    #[error("Join event forward-skew rejected")]
    JoinEventFutureSkew,
    /// invite_token.invitee_hint set and != join_event.actor.
    #[error("invitee_hint mismatch")]
    InviteeHintMismatch,
    /// No engine for this community — packet was misrouted. Receiver
    /// surface; not raised by `verify_packet_pure` (engine state isn't
    /// in scope there).
    #[error("community unknown: {community_id:?}")]
    CommunityUnknown {
        community_id: crate::owner_state_types::SpaceId,
    },
    /// Self isn't currently a Joined member. Receiver surface; engine-
    /// coupled.
    #[error("self not joined in community")]
    SelfNotJoined,
    /// Self power < invite_threshold (= 0 in v1, structural no-op).
    #[error("self power insufficient: {self_power} < {threshold}")]
    SelfPowerInsufficient { self_power: u8, threshold: u8 },
    /// `community_membership::attach_countersig_with_identity` failed
    /// (canonical-CBOR encoder error). Vanishingly rare in practice;
    /// distinct from JoinSigInvalid so degraded telemetry can
    /// distinguish a malformed inner Join from a counter-sign encoder
    /// regression on the receiver side.
    #[error("counter-sign attach failed")]
    CounterSignAttachFailed,
    /// Engine-side CRDT verify rejected the counter-signed Join
    /// (`InsertOutcome::Rejected`). Distinct from JoinSigInvalid: the
    /// inner Join sig already validated in step 5 of `verify_packet_pure`,
    /// but the engine's own VerifyContext (admin / invite-only /
    /// expected_community_id) saw something unexpected.
    #[error("engine rejected counter-signed Join")]
    EngineRejected,
    /// `insert_local_event_with_pubs` returned a `LocalInsertError`
    /// (resolver missing, wrong community on the inner event, etc.).
    /// Surfaced separately so the degraded reason tag points at the
    /// engine's local-insert pipeline rather than at sig classes.
    #[error("engine local-insert error")]
    EngineLocalError,
    /// inviter_enrollment cert failed verification (bad signature, expired,
    /// or untrusted issuer — Master and ZEB-677 quorum certs both route
    /// through the ZEB-680 `enrollment_verify` chokepoint). ZEB-497.
    #[error("inviter enrollment cert invalid")]
    InviterEnrollmentCertInvalid,
    /// inviter_enrollment cert binds a different owner than invite_token.inviter.
    /// ZEB-497.
    #[error("inviter enrollment owner mismatch")]
    InviterEnrollmentOwnerMismatch,
}

impl CommunityInviteVerifyError {
    /// Reason tag for the `community-state-sync-degraded` Tauri event.
    pub fn reason_tag(&self) -> &'static str {
        match self {
            Self::EnvelopeSigInvalid => "community_invite_envelope_sig_invalid",
            Self::DeviceHashMismatch => "community_invite_device_hash_mismatch",
            Self::JoinSigInvalid => "community_invite_join_sig_invalid",
            Self::InviteTokenSigInvalid => "community_invite_token_sig_invalid",
            Self::InviteSignerMismatch { .. } => "community_invite_signer_mismatch",
            Self::CommunityIdMismatch => "community_invite_id_mismatch",
            Self::Expired => "community_invite_expired",
            Self::JoinEventFutureSkew => "community_invite_join_event_future_skew",
            Self::InviteeHintMismatch => "community_invitee_hint_mismatch",
            Self::CommunityUnknown { .. } => "community_invite_unknown",
            Self::SelfNotJoined => "community_invite_self_not_joined",
            Self::SelfPowerInsufficient { .. } => "community_invite_self_power_insufficient",
            Self::CounterSignAttachFailed => "community_invite_counter_sign_attach_failed",
            Self::EngineRejected => "community_invite_engine_rejected",
            Self::EngineLocalError => "community_invite_engine_local_error",
            Self::InviterEnrollmentCertInvalid => {
                "community_invite_inviter_enrollment_cert_invalid"
            }
            Self::InviterEnrollmentOwnerMismatch => {
                "community_invite_inviter_enrollment_owner_mismatch"
            }
        }
    }
}

/// Errors from `verify_admin_bootstrap` — the six-step binding chain
/// the joiner runs against the invite payload's `admin_bootstrap` +
/// `admin_identity_pub` fields before inserting the bootstrap into the
/// engine. ZEB-260: closing the cold-cache gap that prevents the new
/// joiner's empty CRDT from admitting the admin's first publish-back.
///
/// Each variant maps to a stable IPC error string via Display (NOT
/// Debug), matching the pattern established in PR #89 for IPC error
/// surface stability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedeemBootstrapVerifyError {
    /// Invite-only payload missing `admin_bootstrap` and/or
    /// `admin_identity_pub`. Fires for old PR #89 invite URLs (which
    /// never carried these fields). Stable IPC string:
    /// "redeem_invite: invite-only payload missing admin bootstrap".
    BootstrapMissing,

    /// `admin_bootstrap.actor` does not equal `payload.admin_addr`.
    BootstrapActorMismatch,

    /// `admin_bootstrap.community_id` does not equal
    /// `payload.community_id`.
    BootstrapCommunityMismatch,

    /// Ed25519 signature verification of `admin_bootstrap` failed under
    /// `admin_identity_pub`.
    BootstrapSignatureInvalid,

    /// `admin_bootstrap.kind` is not `Join`, or `countersig` is `Some`.
    /// Admin's bootstrap is always a self-Join with no countersig.
    BootstrapKindInvalid,
}

impl std::fmt::Display for RedeemBootstrapVerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BootstrapMissing => write!(
                f,
                "redeem_invite: invite-only payload missing admin bootstrap"
            ),
            Self::BootstrapActorMismatch => write!(
                f,
                "redeem_invite: admin_bootstrap.actor != admin_addr"
            ),
            Self::BootstrapCommunityMismatch => write!(
                f,
                "redeem_invite: admin_bootstrap.community_id != payload.community_id"
            ),
            Self::BootstrapSignatureInvalid => write!(
                f,
                "redeem_invite: admin_bootstrap signature verify failed"
            ),
            Self::BootstrapKindInvalid => write!(
                f,
                "redeem_invite: admin_bootstrap is not a self-Join (countersig present or wrong kind)"
            ),
        }
    }
}

impl std::error::Error for RedeemBootstrapVerifyError {}

impl RedeemBootstrapVerifyError {
    /// Short telemetry tag for the existing `record_redeem_outcome`-
    /// style logging path. Kept stable across builds — frontend-side
    /// metrics dashboards key off these strings. Mirrors the
    /// `CommunityInviteVerifyError::reason_tag` shape.
    pub fn reason_tag(&self) -> &'static str {
        match self {
            Self::BootstrapMissing => "bootstrap_missing",
            Self::BootstrapActorMismatch => "bootstrap_actor_mismatch",
            Self::BootstrapCommunityMismatch => "bootstrap_community_mismatch",
            Self::BootstrapSignatureInvalid => "bootstrap_signature_invalid",
            Self::BootstrapKindInvalid => "bootstrap_kind_invalid",
        }
    }
}

/// Run the binding chain that admits the admin's signed bootstrap event
/// into the joiner's engine (ZEB-260, updated ZEB-339). Pure / sync.
///
/// Returns `Ok((&admin_bootstrap, &admin_identity_pub))` on success so
/// the caller can pass them to `engine.insert_local_event_with_pubs`.
/// Returns `Err(variant)` on the first failure.
///
/// The chain (each step's failure → distinct error variant):
///   1. Required fields present (`admin_bootstrap` + `admin_identity_pub`
///      both `Some`). [BootstrapMissing]
///   2. (Removed ZEB-339) — the old flat `address_hash == admin_addr`
///      check is incompatible with the owner/device split. The actor
///      binding is enforced by step 3 instead.
///   3. `admin_bootstrap.actor == payload.admin_addr`.
///      [BootstrapActorMismatch]
///   4. `admin_bootstrap.community_id == payload.community_id`.
///      [BootstrapCommunityMismatch]
///   5. ZEB-339: cert-based verification — `enrolled_key_from_cert` extracts
///      the admin's enrolled device key from the carried `EnrollmentCert`
///      (cert.owner_id == admin_bootstrap.actor, which step 3 binds to
///      admin_addr), then `verify_membership_signer` checks the signature
///      under that key. [BootstrapSignatureInvalid]
///   6. Sanity: `admin_bootstrap.kind == Join` and `countersig is None`.
///      [BootstrapKindInvalid]
///
/// Caller (`redeem_invite_inner` invite-only branch, Task 4) calls this
/// AFTER `spawn_engine` and BEFORE the unicast send. On `Ok`, the caller
/// proceeds to `engine.insert_local_event_with_pubs(admin_bootstrap,
/// admin_identity_pub, None)`. On `Err`, the caller tears down the
/// engine via `shutdown_engine_and_cleanup_persistence` and surfaces
/// the error string.
pub fn verify_admin_bootstrap(
    payload: &CommunityInvitePayload,
) -> Result<
    (
        &crate::community_membership::SignedMembershipEvent,
        &[u8; 64],
    ),
    RedeemBootstrapVerifyError,
> {
    // 1. Required fields.
    let admin_bootstrap = payload
        .admin_bootstrap
        .as_ref()
        .ok_or(RedeemBootstrapVerifyError::BootstrapMissing)?;
    let admin_identity_pub = payload
        .admin_identity_pub
        .as_ref()
        .ok_or(RedeemBootstrapVerifyError::BootstrapMissing)?;

    // Step 2 removed (ZEB-339): the old flat `address_hash == admin_addr`
    // check is incompatible with the owner/device split introduced by
    // ZEB-339. Under ZEB-339, admin_addr is an owner/master hash (not
    // the address_hash of any runtime key). The actor↔admin_addr binding
    // is still enforced by step 3 below; the cryptographic owner↔device
    // binding is enforced by the cert in step 5.

    // 3. bootstrap.actor ↔ admin_addr binding.
    if admin_bootstrap.actor != payload.admin_addr {
        return Err(RedeemBootstrapVerifyError::BootstrapActorMismatch);
    }

    // 4. bootstrap.community_id ↔ payload.community_id binding.
    if admin_bootstrap.community_id != payload.community_id {
        return Err(RedeemBootstrapVerifyError::BootstrapCommunityMismatch);
    }

    // 5. ZEB-339: cert-based verification. The admin's bootstrap Join is
    // device-#2-signed and carries the admin's EnrollmentCert.
    // enrolled_key_from_cert extracts the device key from the cert and
    // verifies cert.owner_id == admin_bootstrap.actor (which step 3 bound
    // to admin_addr). verify_membership_signer then checks the signature
    // under that enrolled device key — same model as verify_packet_pure's
    // inner join_event check.
    let signer = crate::community_membership::enrolled_key_from_cert(admin_bootstrap)
        .map_err(|_| RedeemBootstrapVerifyError::BootstrapSignatureInvalid)?;
    crate::community_membership::verify_membership_signer(admin_bootstrap, &signer)
        .map_err(|_| RedeemBootstrapVerifyError::BootstrapSignatureInvalid)?;

    // 6. Sanity: self-Join with no countersig.
    if !matches!(
        admin_bootstrap.kind,
        crate::community_membership::MembershipEventKind::Join
    ) || admin_bootstrap.countersig.is_some()
    {
        return Err(RedeemBootstrapVerifyError::BootstrapKindInvalid);
    }

    Ok((admin_bootstrap, admin_identity_pub))
}

/// Encode a [`CommunityInvitePacket`] to wire bytes.
///
/// **Mutation guard.** Re-encodes `signed` and asserts byte-equality
/// with the cached `signed_bytes` (which was the source for `signature`
/// at build time); mismatch returns `SignedMutated`. The only way this
/// fires is post-build mutation of `signed`; no in-crate code path does
/// this, but the guard catches future regressions cheaply with a memcmp.
///
/// On success the function emits the cached `signed_bytes` verbatim
/// (NOT the freshly re-encoded bytes), preserving byte-exactness on
/// decode→encode round trips. Mirrors [`crate::dm_envelope::encode_packet`].
pub fn encode_packet(
    packet: &CommunityInvitePacket,
) -> Result<Vec<u8>, CommunityInviteEncodeError> {
    match packet {
        CommunityInvitePacket::Invite {
            signed,
            signature,
            signed_bytes,
        } => {
            let re_encoded = canonical_cbor_encode(signed)
                .map_err(|e| CommunityInviteEncodeError::ReSerialize(format!("re-encode: {e}")))?;
            if re_encoded != *signed_bytes {
                return Err(CommunityInviteEncodeError::SignedMutated(
                    "CommunityInvitePacket::Invite: signed mutated post-build (re-encode \
                     mismatches cached signed_bytes; signature would not cover wire body)"
                        .into(),
                ));
            }
            let mut out = Vec::with_capacity(1 + signed_bytes.len() + 64);
            out.push(0x10);
            out.extend_from_slice(signed_bytes);
            out.extend_from_slice(signature);
            Ok(out)
        }
        CommunityInvitePacket::OpenJoin {
            req,
            signature,
            signed_bytes,
        } => {
            let re_encoded = canonical_cbor_encode(req)
                .map_err(|e| CommunityInviteEncodeError::ReSerialize(format!("re-encode: {e}")))?;
            if re_encoded != *signed_bytes {
                return Err(CommunityInviteEncodeError::SignedMutated(
                    "CommunityInvitePacket::OpenJoin: req mutated post-build (re-encode \
                     mismatches cached signed_bytes; signature would not cover wire body)"
                        .into(),
                ));
            }
            let mut out = Vec::with_capacity(1 + signed_bytes.len() + 64);
            out.push(0x11);
            out.extend_from_slice(signed_bytes);
            out.extend_from_slice(signature);
            Ok(out)
        }
    }
}

/// Decode wire bytes into a [`CommunityInvitePacket`]. Captures
/// `signed_bytes` exactly as transmitted so envelope-sig verify
/// operates on bit-exact bytes regardless of encoder drift.
///
/// Rejects: unknown discriminants, trailing bytes after the CBOR body,
/// non-canonical encodings (decode → canonical-re-encode mismatch),
/// and `signing_device_hash` not equal to `SHA256(joiner_identity_pub)[..16]`
/// (defense-in-depth before the receive handler runs the Ed25519 verify).
pub fn decode_packet(bytes: &[u8]) -> Result<CommunityInvitePacket, CommunityInviteDecodeError> {
    let (disc, rest) = bytes
        .split_first()
        .ok_or(CommunityInviteDecodeError::Empty)?;
    if rest.len() < 64 + 1 {
        return Err(CommunityInviteDecodeError::TooShortForSignature);
    }
    let split_at = rest.len() - 64;
    let (body_bytes, signature_bytes) = rest.split_at(split_at);
    let signature: [u8; 64] = signature_bytes
        .try_into()
        .expect("just split at len-64; signature_bytes is exactly 64 bytes");
    let signed_bytes = body_bytes.to_vec();
    match disc {
        0x10 => {
            let mut cursor = std::io::Cursor::new(body_bytes);
            let signed: CommunityInviteSigned = ciborium::from_reader(&mut cursor)
                .map_err(|e| CommunityInviteDecodeError::Cbor(e.to_string()))?;
            let consumed = cursor.position();
            if consumed as usize != body_bytes.len() {
                return Err(CommunityInviteDecodeError::TrailingBytes {
                    consumed,
                    total: body_bytes.len() as u64,
                });
            }
            // Canonical-encoding round-trip check: re-encode and reject
            // if the re-encoded bytes differ from body_bytes. Catches
            // reordered map keys, indefinite-length encodings, oversized
            // length prefixes — anything where decode → canonical-re-
            // encode is not byte-identical. Mirrors
            // dm_envelope::ensure_canonical_body.
            let canonical = canonical_cbor_encode(&signed)
                .map_err(|e| CommunityInviteDecodeError::Cbor(e.to_string()))?;
            if canonical != body_bytes {
                return Err(CommunityInviteDecodeError::Invalid(
                    "CommunityInvitePacket body must use canonical CBOR",
                ));
            }
            // Structural check: signing_device_hash must match
            // SHA256(joiner_identity_pub)[..16]. Not a sig check (no
            // crypto here); cheap defense-in-depth before the sig
            // verifier runs in handle_unicast.
            let derived = device_hash_from_identity_pub(&signed.joiner_identity_pub);
            if derived != signed.signing_device_hash.0 {
                return Err(CommunityInviteDecodeError::Invalid(
                    "CommunityInviteSigned.signing_device_hash must equal \
                     SHA256(joiner_identity_pub)[..16]",
                ));
            }
            Ok(CommunityInvitePacket::Invite {
                signed,
                signature,
                signed_bytes,
            })
        }
        0x11 => {
            let mut cursor = std::io::Cursor::new(body_bytes);
            let req: OpenJoinRequest = ciborium::from_reader(&mut cursor)
                .map_err(|e| CommunityInviteDecodeError::Cbor(e.to_string()))?;
            let consumed = cursor.position();
            if consumed as usize != body_bytes.len() {
                return Err(CommunityInviteDecodeError::TrailingBytes {
                    consumed,
                    total: body_bytes.len() as u64,
                });
            }
            // Canonical-encoding round-trip check: reject reordered map
            // keys / indefinite-length / oversized-prefix encodings, same
            // as the 0x10 arm. The joiner identity ↔ signing_device_hash
            // binding + capability/freshness checks are beacon-side
            // (open_join_admit), not part of pure framing.
            let canonical = canonical_cbor_encode(&req)
                .map_err(|e| CommunityInviteDecodeError::Cbor(e.to_string()))?;
            if canonical != body_bytes {
                return Err(CommunityInviteDecodeError::Invalid(
                    "CommunityInvitePacket body must use canonical CBOR",
                ));
            }
            Ok(CommunityInvitePacket::OpenJoin {
                req,
                signature,
                signed_bytes,
            })
        }
        other => Err(CommunityInviteDecodeError::UnknownDiscriminant(*other)),
    }
}

/// Compute the 16-byte device-address hash `SHA256(identity_pub)[..16]` over
/// the 64-byte combined pub (`X25519(32) ‖ Ed25519(32)`). Mirrors how
/// `DmInvite` Path B derives `signing_device_hash`; the receiver checks this
/// binding before running the (more expensive) Ed25519 verify.
///
/// Delegates to `harmony_crypto::hash::truncated_hash` — the same primitive
/// behind `harmony_identity::Identity::address_hash` — so the wire commitment
/// and the identity-layer device hash cannot drift (ZEB-716). Deliberately
/// INFALLIBLE: this is a pure-hash commitment over untrusted wire bytes,
/// enforced by the invite/open-join decode defenses BEFORE any point
/// validation or signature check, so a non-canonical Ed25519 half must still
/// hash (and then fail verification downstream). Identity contexts that must
/// REJECT invalid points use the fallible twin,
/// `crate::dm_signing::derive_device_hash_from_identity_pub`. Distinct notion
/// from `harmony_owner::PubKeyBundle::identity_hash()`, which hashes SIGNING
/// material only (encryption-key rotation preserves it) — the two must never
/// be converged (ZEB-571 item 14 adjudication).
pub fn device_hash_from_identity_pub(identity_pub: &[u8; 64]) -> [u8; 16] {
    harmony_crypto::hash::truncated_hash(identity_pub)
}

/// Build a complete [`CommunityInvitePacket`] ready for [`encode_packet`].
/// Encodes `signed` to canonical CBOR, signs the resulting bytes via
/// `signing_key`, bundles into the `Invite` variant. Mirrors
/// [`crate::dm_envelope::build_signed_invite`].
pub fn build_signed_invite_packet(
    signed: CommunityInviteSigned,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<CommunityInvitePacket, CommunityInviteEncodeError> {
    use ed25519_dalek::Signer;
    let signed_bytes = canonical_cbor_encode(&signed)
        .map_err(|e| CommunityInviteEncodeError::Cbor(e.to_string()))?;
    let signature = signing_key.sign(&signed_bytes).to_bytes();
    Ok(CommunityInvitePacket::Invite {
        signed,
        signature,
        signed_bytes,
    })
}

/// Build a complete open-join [`CommunityInvitePacket`] ready for
/// [`encode_packet`]. Encodes `req` to canonical CBOR, signs the resulting
/// bytes via `signing_key`, bundles into the `OpenJoin` variant. Mirrors
/// [`build_signed_invite_packet`], swapping the invite token for the
/// `epoch_auth`/`nonce` capability already inside `req`.
pub fn build_signed_open_join_packet(
    req: OpenJoinRequest,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<CommunityInvitePacket, CommunityInviteEncodeError> {
    use ed25519_dalek::Signer;
    let signed_bytes =
        canonical_cbor_encode(&req).map_err(|e| CommunityInviteEncodeError::Cbor(e.to_string()))?;
    let signature = signing_key.sign(&signed_bytes).to_bytes();
    Ok(CommunityInvitePacket::OpenJoin {
        req,
        signature,
        signed_bytes,
    })
}

/// Pure verify helper: takes a [`CommunityInviteSigned`], the local self
/// owner addr, a wall-clock function, and the counter-signer's enrolled
/// device #2 ed25519 verify key for the InviteToken sig check. Returns the
/// joiner's signed Join event on success — caller is then responsible for
/// the engine-coupled checks (community known, self joined, self power
/// sufficient) before counter-signing.
///
/// Order of checks chosen so cheaper / more diagnostic rejections fire
/// before expensive crypto:
///   1. community_id agreement (cheap struct compare)
///   2. invitee_hint match (cheap if hint is None)
///   3. expiry / clock-skew (cheap arithmetic, 60s tolerance)
///   4. InviteToken signer == self (cheap struct compare)
///   5. Inner Join event sig (1× Ed25519 verify_strict via
///      `community_membership::verify_signature`)
///   6. InviteToken sig (1× Ed25519 verify_strict against the canonical
///      token payload, verified against `self_device_ed25519`)
///
/// ZEB-846 Task 7 adds one more bound, checked immediately after step 3:
/// `join_event.at.wall_ms` — the Join's OWN wall — must also be within
/// `clock_trust::MAX_FORWARD_SKEW_MS` of the receiver's wall clock. This is
/// separate from `created_at` above (which only bounds the request envelope)
/// because the Join event is what actually lands in the persisted log.
///
/// `self_device_ed25519` must be the counter-signer's enrolled device #2
/// ed25519 verifying key (32 bytes). This aligns step 6 with
/// `verify_event`'s P5 gate (`verify_invite_token_sig_with_enrolled`),
/// which resolves the same key from materialized membership.
///
/// Membership-state-dependent checks (`SelfNotJoined`, `CommunityUnknown`,
/// `SelfPowerInsufficient`) are NOT raised here — they require engine
/// state and ship in Task 9's `handle_unicast`.
pub fn verify_packet_pure<F>(
    signed: &CommunityInviteSigned,
    self_owner: crate::owner_state_types::OwnerAddr,
    now_fn: F,
    self_device_ed25519: &[u8; 32],
) -> Result<crate::community_membership::SignedMembershipEvent, CommunityInviteVerifyError>
where
    F: FnOnce() -> u64,
{
    // 1. community_id agreement across envelope + Join.
    if signed.community_id != signed.join_event.community_id {
        return Err(CommunityInviteVerifyError::CommunityIdMismatch);
    }
    // (InviteToken doesn't carry community_id directly in v1 — the
    // outer URL payload does. Skip a token vs envelope comparison
    // here; the receive-side engine resolution catches misroutes.)

    // 2. invitee_hint match.
    if let Some(hint) = signed.invite_token.invitee_hint {
        if signed.join_event.actor != hint {
            return Err(CommunityInviteVerifyError::InviteeHintMismatch);
        }
    }

    // 3. Expiry / clock-skew. Three arms:
    //    (a) clock-skew: created_at can't be more than 60s in the
    //        receiver's future (defense against a malicious mint that
    //        backdates `now` to dodge expiry).
    //    (b) expires_at vs created_at (if the inviter set one):
    //        created_at must be strictly before expires_at. The
    //        inviter's signature binds `xa` via
    //        `canonical_invite_token_bytes`, so an attacker cannot
    //        strip the field to extend the window — the InviteToken
    //        sig check in step 6 would fail.
    //    (c) expires_at vs now: a packet whose created_at predated
    //        expires_at can still be replayed AFTER expires_at. Reject
    //        when the receiver's wall clock is at-or-past the token's
    //        expiry. (Greptile / CodeRabbit P1: replay window without
    //        this check is unbounded.)
    let now = now_fn();
    if signed.created_at.wall_ms > now.saturating_add(60_000) {
        return Err(CommunityInviteVerifyError::Expired);
    }
    if let Some(exp) = signed.invite_token.expires_at {
        if signed.created_at.wall_ms >= exp {
            return Err(CommunityInviteVerifyError::Expired);
        }
        if now >= exp {
            return Err(CommunityInviteVerifyError::Expired);
        }
    }

    // 3b. ZEB-846 (Task 7): bound the inner join_event's OWN wall — separate
    //     from `signed.created_at` above, which only bounds the request
    //     envelope. This is the Join event `verify_admin_bootstrap`/the
    //     engine's counter-sign path actually inserts into the persisted
    //     membership log, so an attacker who mints a fresh envelope around a
    //     far-future-walled Join must be rejected here too, not just at the
    //     zenoh-merge path's `verify_event` (Task 3).
    if crate::clock_trust::reject_future_logged(
        signed.join_event.at.wall_ms,
        now,
        crate::clock_trust::MAX_FORWARD_SKEW_MS,
        "community_invite.join_event.at",
    ) {
        return Err(CommunityInviteVerifyError::JoinEventFutureSkew);
    }

    // 4. InviteToken signer == self.
    if signed.invite_token.inviter != self_owner {
        return Err(CommunityInviteVerifyError::InviteSignerMismatch {
            signer: signed.invite_token.inviter,
            self_owner,
        });
    }

    // 5. Inner Join event MEMBERSHIP sig (ZEB-339).
    //    The inner join_event is a community-membership event: it is signed by
    //    the joiner's enrolled device key (#2) and carries the joiner's Master
    //    EnrollmentCert (attached at the redeem mint, Task 7). So its MEMBERSHIP
    //    signature is verified via the cert (owner->device binding), NOT via the
    //    Reticulum `joiner_identity_pub`. (The `joiner_identity_pub` /
    //    `signing_device_hash` defense + the envelope sig stay transport-bound
    //    and are checked elsewhere — out of ZEB-339 scope.)
    let signer = crate::community_membership::enrolled_key_from_cert(&signed.join_event)
        .map_err(|_| CommunityInviteVerifyError::JoinSigInvalid)?;
    crate::community_membership::verify_membership_signer(&signed.join_event, &signer)
        .map_err(|_| CommunityInviteVerifyError::JoinSigInvalid)?;

    // 6. InviteToken sig — verify against the counter-signer's enrolled
    //    device #2 key (ZEB-339 consistency fix). Step 4 already binds
    //    `signed.invite_token.inviter == self_owner`, so this is exactly
    //    the key that `verify_event`'s P5 gate (`verify_invite_token_sig_with_enrolled`)
    //    resolves from materialized membership. Both paths now agree: only a
    //    device-#2-signed token satisfies the invite-only redemption chain.
    verify_invite_token_sig_device_key(&signed.invite_token, self_device_ed25519)
        .map_err(|_| CommunityInviteVerifyError::InviteTokenSigInvalid)?;

    Ok(signed.join_event.clone())
}

/// Canonical-CBOR-encode the InviteToken payload (excluding the sig).
/// Both the IPC mint path (Phase 4 `generate_invite` for invite-only —
/// not yet shipped) and the verify path encode through this so signature
/// bytes cover bit-exact bytes.
///
/// Wire format: a 2- to 4-key map with field codes `iv`, `ih`, `mt`,
/// `xa` (mirrors `InviteToken`'s renames; same-length-keys CBOR
/// invariant). `ih` is omitted when `invitee_hint = None`; `xa` is
/// omitted when `expires_at = None`. The InviteToken sig commits to
/// these bytes — the inviter cannot strip `xa` post-sign without
/// invalidating the signature, so the receiver's expiry enforcement
/// in `verify_packet_pure` is bound to the inviter's authorization.
///
/// Public so the test harness can call it; mint path (Phase 4 IPC) will
/// also call this when invite-only `generate_invite` ships, ensuring
/// mint and verify never drift.
pub fn canonical_invite_token_bytes(
    token: &InviteToken,
) -> Result<Vec<u8>, ciborium::ser::Error<std::io::Error>> {
    #[derive(serde::Serialize)]
    struct InviteTokenPayload<'a> {
        #[serde(rename = "iv")]
        inviter: &'a crate::owner_state_types::OwnerAddr,
        #[serde(rename = "ih", skip_serializing_if = "Option::is_none")]
        invitee_hint: Option<&'a crate::owner_state_types::OwnerAddr>,
        #[serde(rename = "mt")]
        minted_at: &'a crate::owner_state_types::Hlc,
        #[serde(rename = "xa", skip_serializing_if = "Option::is_none")]
        expires_at: Option<u64>,
    }
    let payload = InviteTokenPayload {
        inviter: &token.inviter,
        invitee_hint: token.invitee_hint.as_ref(),
        minted_at: &token.minted_at,
        expires_at: token.expires_at,
    };
    let mut out = Vec::new();
    ciborium::into_writer(&payload, &mut out)?;
    Ok(out)
}

/// Verify the Path B envelope signature over the captured `signed_bytes`.
/// Pure crypto check — no membership or expiry semantics. Returns
/// [`CommunityInviteVerifyError::EnvelopeSigInvalid`] on any failure
/// (including malformed `identity_pub`). Used by `handle_unicast`
/// (Task 9) and exercised by the
/// `community_invite_packet_envelope_sig_rejected_on_tampered_body` test.
pub fn verify_envelope_sig(
    signed_bytes: &[u8],
    signature: &[u8; 64],
    identity_pub: &[u8; 64],
) -> Result<(), CommunityInviteVerifyError> {
    use ed25519_dalek::Signature;
    let identity = harmony_identity::Identity::from_public_bytes(identity_pub)
        .map_err(|_| CommunityInviteVerifyError::EnvelopeSigInvalid)?;
    let sig = Signature::from_bytes(signature);
    identity
        .verifying_key
        .verify_strict(signed_bytes, &sig)
        .map_err(|_| CommunityInviteVerifyError::EnvelopeSigInvalid)
}

/// ZEB-254: pure helper for verifying an InviteToken's signature against
/// a known admin identity_pub. Extracted for use by `verify_event` on
/// `PendingJoin` events, where the admin's identity_pub is available in
/// `VerifyContext` but we don't have a `PrivateIdentity` to call
/// `verify_packet_pure` with.
///
/// Verifies that `token.sig` covers the canonical token bytes (as produced
/// by `canonical_invite_token_bytes`) and was produced by the Ed25519 key
/// embedded in `admin_identity_pub[32..]`.
///
/// Returns `Err(CommunityInviteVerifyError::InviteTokenSigInvalid)` on any
/// failure (malformed pub, bad signature).
pub fn verify_invite_token_signature(
    token: &InviteToken,
    admin_identity_pub: &[u8; 64],
) -> Result<(), CommunityInviteVerifyError> {
    let token_canonical = canonical_invite_token_bytes(token)
        .map_err(|_| CommunityInviteVerifyError::InviteTokenSigInvalid)?;
    use ed25519_dalek::Signature;
    let identity = harmony_identity::Identity::from_public_bytes(admin_identity_pub)
        .map_err(|_| CommunityInviteVerifyError::InviteTokenSigInvalid)?;
    let sig = Signature::from_bytes(&token.sig);
    identity
        .verifying_key
        .verify_strict(&token_canonical, &sig)
        .map_err(|_| CommunityInviteVerifyError::InviteTokenSigInvalid)
}

/// ZEB-339: verify an InviteToken's signature against a 32-byte ed25519
/// device verifying key directly. Mirrors the inner loop of
/// `community_membership::verify_invite_token_sig_with_enrolled` but takes
/// a single concrete key rather than iterating over a set. Used by
/// `verify_packet_pure` step 6, where the counter-signer IS the sole
/// owner of the device key (resolved from `community_signing_key` in
/// `handle_unicast`).
///
/// Returns `Err(CommunityInviteVerifyError::InviteTokenSigInvalid)` on any
/// failure (malformed key bytes, bad signature).
pub fn verify_invite_token_sig_device_key(
    token: &InviteToken,
    device_ed25519: &[u8; 32],
) -> Result<(), CommunityInviteVerifyError> {
    let token_canonical = canonical_invite_token_bytes(token)
        .map_err(|_| CommunityInviteVerifyError::InviteTokenSigInvalid)?;
    use ed25519_dalek::Signature;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(device_ed25519)
        .map_err(|_| CommunityInviteVerifyError::InviteTokenSigInvalid)?;
    let sig = Signature::from_bytes(&token.sig);
    vk.verify_strict(&token_canonical, &sig)
        .map_err(|_| CommunityInviteVerifyError::InviteTokenSigInvalid)
}

/// Verify the inviter's `inviter_enrollment` cert on an invite-only invite:
/// recover the inviter's enrolled device key from the cert, bind it to
/// `invite_token.inviter`, and verify the token signature against it. No-op for
/// open communities. Mirrors `iroh_friend_acceptor::verify_enrolled_device`
/// with the community error type (ZEB-497).
pub fn verify_inviter_enrollment(
    payload: &CommunityInvitePayload,
    now_secs: u64,
) -> Result<(), CommunityInviteVerifyError> {
    if !payload.is_invite_only {
        return Ok(());
    }
    let cert = payload
        .inviter_enrollment
        .as_ref()
        .ok_or(CommunityInviteVerifyError::InviterEnrollmentCertInvalid)?;
    let token = payload
        .invite_token
        .as_ref()
        .ok_or(CommunityInviteVerifyError::InviteTokenSigInvalid)?;
    // Recover the inviter's enrolled device key via the ZEB-677 chokepoint:
    // Master certs verify self-contained; Quorum certs verify against the
    // payload's inviter_signer_certs bundle (depth-1). No-bundle quorum
    // certs still fail closed.
    let verified = crate::enrollment_verify::verify_enrollment_any_issuer(
        cert,
        &payload.inviter_signer_certs,
        Some(&token.inviter.0),
        now_secs,
    )
    .map_err(|e| match e {
        crate::enrollment_verify::EnrollmentVerifyError::OwnerMismatch => {
            CommunityInviteVerifyError::InviterEnrollmentOwnerMismatch
        }
        _ => CommunityInviteVerifyError::InviterEnrollmentCertInvalid,
    })?;
    verify_invite_token_sig_device_key(token, &verified.device_ed25519)
}

// =====================================================================
// ZEB-262 Phase 4 Task 9 — receive-side dispatch
// =====================================================================

/// Tiny trait so `handle_unicast` can take either a real
/// `tauri::AppHandle` or a test stub (`None::<&()>`). Production impl on
/// `tauri::AppHandle` lives in `lib.rs` (small adapter that calls
/// `app.emit("community-state-sync-degraded", …)`). Tests typically
/// pass `None`.
pub trait AppHandleEmit {
    /// Emit a `community-state-sync-degraded` Tauri event with the
    /// community id (lowercase hex) and reason tag.
    fn emit_degraded(&self, community_id_hex: &str, reason_tag: &'static str);
}

/// Unit-type impl: tests can pass `None::<&()>` and the trait method is
/// never called in the None path. Provided here so the bound resolves
/// without forcing tests to define their own stub.
impl AppHandleEmit for () {
    fn emit_degraded(&self, _: &str, _: &'static str) {}
}

fn emit_degraded<H: AppHandleEmit>(
    app: Option<&H>,
    community_id: &crate::owner_state_types::SpaceId,
    reason_tag: &'static str,
) {
    if let Some(app) = app {
        app.emit_degraded(&hex::encode(community_id.0), reason_tag);
    } else {
        tracing::warn!(
            community_id = %hex::encode(community_id.0),
            reason = reason_tag,
            "community_invite verify failed (no app handle); not emitting Tauri event"
        );
    }
}

/// ZEB-262 Phase 4 Task 9: receive-side handler for Reticulum unicast
/// packets with discriminant 0x10. Runs the verify chain per spec
/// §"Receive path", attaches the counter-sig via
/// [`crate::community_membership::attach_countersig_with_identity`],
/// inserts the counter-signed Join via `engine.insert_local_event`. The
/// engine's post-Inserted hook (Task 7) fires the joiner-side
/// `pending_redemptions[event_id]` oneshot.
///
/// On any verify failure, emits `community-state-sync-degraded` (when
/// `app` is `Some`) and returns `Err`. No retry — Reticulum retransmit
/// will redrive from the sender if needed.
///
/// `crdt_state` is plumbed through but unused in v1: the receive-side
/// only mutates the per-community CRDT (inside the engine), not the
/// owner-state Space. The arg is kept for future expansion (e.g.,
/// resolving the inviter's devices for ack-back routing in ZEB-251).
pub async fn handle_unicast<H: AppHandleEmit>(
    community_registry: &std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
    dm_outbox: &std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>,
    _crdt_state: &std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    packet_bytes: Vec<u8>,
    app: Option<&H>,
) -> Result<(), CommunityInviteVerifyError> {
    // 1. decode_packet — peels the 0x10 discriminant + 64-byte trailer,
    //    canonical-CBOR-checks the inner body, enforces the
    //    SHA256(joiner_identity_pub)[..16] == signing_device_hash bind.
    let packet = match decode_packet(&packet_bytes) {
        Ok(p) => p,
        Err(e) => {
            // Decode failure: caller can't identify a community_id, so
            // there's no community to flag in a degraded event. Drop +
            // warn. Returning a generic envelope-sig variant lets
            // handle_unicast keep a uniform error type without forcing
            // CommunityInviteVerifyError to absorb decode variants.
            tracing::warn!(error = %e, "community_invite decode_packet failed; dropping");
            return Err(CommunityInviteVerifyError::EnvelopeSigInvalid);
        }
    };
    let CommunityInvitePacket::Invite {
        signed,
        signature,
        signed_bytes,
    } = packet
    else {
        // Open-join packets (0x11) are dispatched by the open-join admit
        // path, not this invite-redeem helper; reject as not-an-invite.
        tracing::warn!("community_invite handle_unicast received a non-invite packet; dropping");
        return Err(CommunityInviteVerifyError::EnvelopeSigInvalid);
    };

    // 2. Snapshot self_owner + private_identity + community_signing_key (#2)
    //    from dm_outbox under its lock; drop the guard before any further
    //    `.await`. ZEB-339: `verify_packet_pure` step 6 now verifies the
    //    InviteToken sig against the enrolled device key (#2), consistent
    //    with `verify_event`'s P5 gate. `self_private_identity` is kept for
    //    the `countersigner_pub` Reticulum transport field further below.
    let (self_owner, self_private_identity, community_signing_key) = {
        let outbox_g = dm_outbox.lock().await;
        (
            outbox_g.self_owner,
            std::sync::Arc::clone(&outbox_g.private_identity),
            std::sync::Arc::clone(&outbox_g.community_signing_key),
        )
    };

    // 3a. Path B envelope sig over signed_bytes (joiner's signature
    //     over the canonical-CBOR body).
    if let Err(e) = verify_envelope_sig(&signed_bytes, &signature, &signed.joiner_identity_pub) {
        emit_degraded(app, &signed.community_id, e.reason_tag());
        return Err(e);
    }
    // 3b. Pure verify chain (community_id agreement, invitee_hint,
    //     expiry/clock-skew, InviteToken signer == self, Join sig,
    //     InviteToken sig). Step 6 verifies the InviteToken sig against the
    //     counter-signer's enrolled device key (#2), consistent with
    //     `verify_event`'s P5 gate.
    let self_device_vk = community_signing_key.verifying_key().to_bytes();
    let join_event = match verify_packet_pure(
        &signed,
        self_owner,
        || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64
        },
        &self_device_vk,
    ) {
        Ok(e) => e,
        Err(e) => {
            emit_degraded(app, &signed.community_id, e.reason_tag());
            return Err(e);
        }
    };

    // 4. Resolve engine + state for community_id.
    let engine_arc = match community_registry.engine_arc(&signed.community_id).await {
        Some(e) => e,
        None => {
            let e = CommunityInviteVerifyError::CommunityUnknown {
                community_id: signed.community_id,
            };
            emit_degraded(app, &signed.community_id, e.reason_tag());
            return Err(e);
        }
    };
    let state_arc = match community_registry.state_for(&signed.community_id).await {
        Some(s) => s,
        None => {
            let e = CommunityInviteVerifyError::CommunityUnknown {
                community_id: signed.community_id,
            };
            emit_degraded(app, &signed.community_id, e.reason_tag());
            return Err(e);
        }
    };

    // 5. Self-eligibility: must be Joined; power ≥ invite_threshold
    //    (= 0 in v1 — structural no-op + stable hook for ZEB-251).
    let (self_status, self_power) = {
        let s = state_arc.lock().await;
        let events: Vec<_> = s.events().cloned().collect();
        drop(s);
        // R4-6: pass wall_now_ms so an idle-community PendingJoin is
        // surfaced as expired (status == None) rather than as
        // PendingJoin/Joined, preserving alignment with verify-time
        // expiry semantics on this display path.
        let wall_now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let mat = crate::community_membership::materialize_with_now(
            &events,
            engine_arc.admin_addr(),
            Some(wall_now_ms),
        );
        let st = mat.members.get(&self_owner).map(|m| m.status);
        let pw = mat.power_levels.get(&self_owner).copied().unwrap_or(0);
        (st, pw)
    };
    if self_status != Some(crate::community_membership::MemberStatus::Joined) {
        let e = CommunityInviteVerifyError::SelfNotJoined;
        emit_degraded(app, &signed.community_id, e.reason_tag());
        return Err(e);
    }
    let invite_threshold: u8 = 0;
    if self_power < invite_threshold {
        let e = CommunityInviteVerifyError::SelfPowerInsufficient {
            self_power,
            threshold: invite_threshold,
        };
        emit_degraded(app, &signed.community_id, e.reason_tag());
        return Err(e);
    }

    // ZEB-254: Two-event flow for invite-only counter-sign.
    //
    // New ZEB-254 path: joiner's PendingJoin event enters the engine
    // via insert_local_event_with_pubs. The post-Inserted hook (Task 10)
    // detects PendingJoin + self-has-power and emits a JoinCountersign
    // automatically.
    //
    // LEGACY pre-ZEB-254 path: joiners on stale clients still send
    // SignedMembershipEvent { kind: Join, countersig: None }. We
    // continue to attach_countersig + insert the counter-signed Join
    // so those joiners can still join.
    let is_pending_join_shape = matches!(
        &join_event.kind,
        crate::community_membership::MembershipEventKind::PendingJoin { .. }
    );

    if is_pending_join_shape {
        // 6+7 (ZEB-254 new shape). Insert PendingJoin AS-IS — no
        // countersig append here. The joiner's `joiner_identity_pub`
        // was already verified in `verify_packet_pure` step 5 (Path B
        // app-sig binding). The production `OwnerDeviceCacheResolver`
        // won't have the joiner yet (bootstrap-by-design), so we bypass
        // it via `insert_local_event_with_pubs` which accepts explicit
        // pubs.
        //
        // The post-Inserted hook (Task 10) detects PendingJoin +
        // self-has-power and auto-emits JoinCountersign.
        // ZEB-339: PendingJoin no longer carries an inline joiner_identity_pub
        // (the joiner's enrolled device key is proven by the carried
        // EnrollmentCert and verified inside verify_event). The envelope's
        // joiner_identity_pub — already verified via the Path B app-sig binding
        // in verify_packet_pure step 5 — is passed to the resolver-bypass
        // insert path. The former F6 event↔envelope pub cross-check is
        // subsumed by the cert→actor binding inside verify_event. (Task 8 will
        // rework this redemption path onto the cert model end-to-end.)
        let joiner_identity_pub = signed.joiner_identity_pub;
        match engine_arc
            .insert_local_event_with_pubs(join_event, joiner_identity_pub, None)
            .await
        {
            Ok(crate::community_state_crdt::InsertOutcome::Inserted) => {
                // ZEB-874: the single-use invite is NO LONGER burned here. The
                // burn moved to the acceptor, gated behind a successful
                // countersign-response write, so a post-insert delivery failure
                // leaves the invite live for retry. See
                // iroh_invite_acceptor::handle_invite_handshake_inbound.
                Ok(())
            }
            Ok(crate::community_state_crdt::InsertOutcome::AlreadyKnown) => Ok(()),
            Ok(crate::community_state_crdt::InsertOutcome::Rejected(verr)) => {
                tracing::warn!(error = ?verr, "ZEB-254 handle_unicast: PendingJoin rejected by engine");
                let e = CommunityInviteVerifyError::EngineRejected;
                emit_degraded(app, &signed.community_id, e.reason_tag());
                Err(e)
            }
            Err(local_err) => {
                tracing::warn!(error = %local_err, "ZEB-254 handle_unicast: insert PendingJoin failed");
                let e = CommunityInviteVerifyError::EngineLocalError;
                emit_degraded(app, &signed.community_id, e.reason_tag());
                Err(e)
            }
        }
    } else {
        // 6. LEGACY path: Attach countersig with our enrolled device key (#2).
        //    ZEB-339: the counter-sig MUST be produced with device #2, since
        //    `verify_countersig` resolves the counter-signer's key from their
        //    materialized `enrolled_device_keys` (populated from cert-bearing
        //    Joins). Signing with the Reticulum identity would no longer verify.
        let counter_signed = match crate::community_membership::attach_countersig_with_device_key(
            &join_event,
            self_owner,
            community_signing_key.as_ref(),
        ) {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(error = %err, "attach_countersig_with_device_key failed");
                let e = CommunityInviteVerifyError::CounterSignAttachFailed;
                emit_degraded(app, &signed.community_id, e.reason_tag());
                return Err(e);
            }
        };

        // 7. LEGACY path: Insert via engine using `insert_local_event_with_pubs` — the
        //    joiner's `joiner_identity_pub` was already verified in
        //    `verify_packet_pure` step 5 (Path B app-sig binding), and the
        //    receiver's own identity_pub is known locally. The production
        //    `OwnerDeviceCacheResolver` won't have the joiner yet (this IS
        //    the bootstrap that would populate the cache), so we MUST
        //    bypass it. Skipping the resolver here is the load-bearing fix
        //    for the bootstrap-by-design case: a counter-signed Join lands
        //    LOCALLY here regardless of whether the resolver knows the
        //    joiner; the publish-back path then carries the full
        //    counter-signed event to peers, who do their own membership-
        //    state verify against their resolver caches as those caches
        //    populate.
        //
        //    The engine's post-Inserted hook
        //    (`notify_pending_redemption_in_map`) fires
        //    `pending_redemptions[event_id]` for the joiner side — this
        //    wakes the redeemer's `redeem_invite_inner` oneshot wait once
        //    the counter-signed Join propagates back via Phase 2's
        //    state-root publish.
        let countersigner_pub = self_private_identity.identity.to_public_bytes();
        match engine_arc
            .insert_local_event_with_pubs(
                counter_signed,
                signed.joiner_identity_pub,
                Some(countersigner_pub),
            )
            .await
        {
            Ok(crate::community_state_crdt::InsertOutcome::Inserted) => {
                // ZEB-874: the single-use invite is NO LONGER burned here. The
                // burn moved to the acceptor, gated behind a successful
                // countersign-response write, so a post-insert delivery failure
                // leaves the invite live for retry. See
                // iroh_invite_acceptor::handle_invite_handshake_inbound.
                Ok(())
            }
            Ok(crate::community_state_crdt::InsertOutcome::AlreadyKnown) => {
                // Idempotent retransmit (Reticulum can deliver duplicates).
                // Treat as success — we've already counter-signed this id.
                Ok(())
            }
            Ok(crate::community_state_crdt::InsertOutcome::Rejected(verr)) => {
                tracing::warn!(error = ?verr, "counter-signed Join rejected by engine");
                let e = CommunityInviteVerifyError::EngineRejected;
                emit_degraded(app, &signed.community_id, e.reason_tag());
                Err(e)
            }
            Err(local_err) => {
                tracing::warn!(error = %local_err, "engine.insert_local_event_with_pubs errored");
                let e = CommunityInviteVerifyError::EngineLocalError;
                emit_degraded(app, &signed.community_id, e.reason_tag());
                Err(e)
            }
        }
    }
}

// =====================================================================
// Unit tests — ZEB-249 PR #106 R5
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_crypto::canonical_cbor_encode;
    use crate::owner_state_types::{OwnerAddr, SpaceId};

    /// ZEB-716 byte-equivalence pin: the infallible wire commitment equals the
    /// fallible identity-layer device hash (`harmony_identity::Identity::
    /// address_hash`) for a well-formed combined pub. Guards the delegation to
    /// `harmony_crypto::hash::truncated_hash` — if either side ever changes
    /// its formula, this breaks loudly instead of letting the decode defense
    /// and the identity layer drift apart.
    #[test]
    fn device_hash_matches_identity_address_hash_for_valid_keys() {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[0x5Au8; 32]);
        let x_pub = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from([0x5Bu8; 32]));
        let mut pub64 = [0u8; 64];
        pub64[..32].copy_from_slice(x_pub.as_bytes());
        pub64[32..].copy_from_slice(sk.verifying_key().as_bytes());

        let wire = device_hash_from_identity_pub(&pub64);
        let identity = crate::dm_signing::derive_device_hash_from_identity_pub(&pub64)
            .expect("honestly-built combined pub must parse");
        assert_eq!(
            wire, identity.0,
            "wire commitment and identity-layer device-address hash must be byte-identical"
        );
    }

    /// ZEB-716 fallibility-divergence pin: for a NON-canonical Ed25519 half
    /// the identity layer rejects (`None`) while the wire commitment still
    /// hashes — the decode defenses run it over arbitrary untrusted bytes
    /// before any point validation, so infallibility is load-bearing wire
    /// semantics, not sloppiness.
    #[test]
    fn device_hash_stays_infallible_for_invalid_ed25519_point() {
        // Search for a combined pub whose Ed25519 half the IDENTITY layer
        // itself rejects, using the production predicate as the search
        // criterion (mirrors `dm_tunnel_contact.rs`) so the fixture can never
        // drift from the layer under test. A magic constant is NOT reliably
        // invalid — curve25519-dalek REDUCES a non-canonical y mod p instead
        // of rejecting it — and roughly half of all encodings are off-curve,
        // so a deterministic scan finds one almost immediately.
        let pub64 = (0u32..20_000)
            .map(|i| {
                let mut cand = [0u8; 64];
                cand[..32].copy_from_slice(&[0x5Bu8; 32]);
                cand[32] = (i & 0xFF) as u8;
                cand[33] = (i >> 8) as u8;
                cand
            })
            .find(|cand| crate::dm_signing::derive_device_hash_from_identity_pub(cand).is_none())
            .expect("a 20k-candidate space must contain an off-curve Ed25519 encoding");

        // Redundant with the search predicate, but states the contract's
        // rejecting half explicitly next to the hashing half below.
        assert!(
            crate::dm_signing::derive_device_hash_from_identity_pub(&pub64).is_none(),
            "identity layer must reject the off-curve Ed25519 point"
        );
        assert_eq!(
            device_hash_from_identity_pub(&pub64),
            harmony_crypto::hash::truncated_hash(&pub64),
            "wire commitment must still hash arbitrary bytes (decode-defense contract)"
        );
    }

    /// Minimal open-community invite payload with correct 32-byte
    /// `sealed_epoch_key`. Use as a baseline; mutate the field under test.
    fn make_open_payload_correct() -> CommunityInvitePayload {
        CommunityInvitePayload {
            inviter_signer_certs: Vec::new(),
            community_id: SpaceId([0u8; 16]),
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: vec![0u8; 32], // correct: 32 bytes for open
                sealed_epoch_keys: Vec::new(),
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr: OwnerAddr([0u8; 16]),
            community_name: "test".to_string(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
            admin_bootstrap: None,
            admin_identity_pub: None,
            forked_from: None,
            pre_fork_snapshot: None,
            inviter_enrollment: None,
            untargeted_decrypt_key: None,
        }
    }

    /// Minimal invite-only invite payload with correct 92-byte
    /// `sealed_epoch_key`. Use as a baseline; mutate the field under test.
    ///
    /// All signatures are stubs (all-zeros): `encode_invite_url` only
    /// validates field *presence*, not cryptographic correctness.
    fn make_invite_only_payload_correct() -> CommunityInvitePayload {
        use crate::community_membership::{MembershipEventKind, SignedMembershipEvent};
        use crate::owner_state_types::Hlc;

        let admin_addr = OwnerAddr([0u8; 16]);
        let community_id = SpaceId([0u8; 16]);

        let admin_bootstrap = SignedMembershipEvent {
            signer_certs: Vec::new(),
            id: [0u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin_addr,
            at: Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "test".to_string(),
            },
            sig: [0u8; 64],
            countersig: None,
            // ZEB-339: bootstrap-Join must embed the admin's EnrollmentCert.
            enrollment: Some(crate::community_membership::mint_test_owner(0x6E).cert),
        };

        CommunityInvitePayload {
            inviter_signer_certs: Vec::new(),
            community_id,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: vec![0u8; 92], // correct: 92 bytes for invite-only
                sealed_epoch_keys: Vec::new(),
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr,
            community_name: "test-invite-only".to_string(),
            is_invite_only: true,
            expires_at: None,
            invite_token: Some(InviteToken {
                inviter: admin_addr,
                invitee_hint: None,
                minted_at: Hlc {
                    wall_ms: 1_000,
                    logical: 0,
                    device_id: "test".to_string(),
                },
                expires_at: None,
                sig: [0u8; 64],
            }),
            admin_bootstrap: Some(admin_bootstrap),
            admin_identity_pub: Some([0u8; 64]),
            forked_from: None,
            pre_fork_snapshot: None,
            // ZEB-339: invite-only payloads must carry the inviter's cert.
            inviter_enrollment: Some(crate::community_membership::mint_test_owner(0x7E).cert),
            // ZEB-367: this helper builds an UNtargeted invite-only payload
            // (invitee_hint == None), which now REQUIRES the URL-carried
            // ephemeral decrypt key to be redeemable. Tests that exercise the
            // targeted path flip invitee_hint to Some and set/clear this field.
            untargeted_decrypt_key: Some([0x99; 32]),
        }
    }

    // ── encode_invite_url ────────────────────────────────────────────

    #[test]
    fn encode_invite_url_accepts_correct_open_key_len() {
        let payload = make_open_payload_correct();
        assert!(encode_invite_url(&payload).is_ok());
    }

    #[test]
    fn encode_invite_url_rejects_wrong_sealed_key_length() {
        let mut payload = make_open_payload_correct();
        payload.epoch_snapshot.sealed_epoch_key = vec![0u8; 50]; // wrong for open
        let err = encode_invite_url(&payload).unwrap_err();
        assert!(
            matches!(
                err,
                InviteUrlError::InvalidSealedEpochKeyLen {
                    mode: "open",
                    expected: 32,
                    got: 50,
                }
            ),
            "unexpected err: {err}"
        );
    }

    // ── ZEB-367 untargeted_decrypt_key guards ────────────────────────

    #[test]
    fn untargeted_key_rejected_on_open_payload() {
        let mut p = make_open_payload_correct();
        p.untargeted_decrypt_key = Some([1u8; 32]);
        assert!(matches!(
            encode_invite_url(&p),
            Err(InviteUrlError::UntargetedKeyNotAllowed)
        ));
    }

    /// Convert the untargeted invite-only baseline into a structurally-VALID
    /// targeted shape (ZEB-369): `invitee_hint = Some`, the single 92-byte
    /// envelope moved into `sealed_epoch_keys`, `sealed_epoch_key` emptied. Used
    /// by the untargeted-key-on-targeted tests so the `sealed_epoch_keys` shape
    /// check passes and the `untargeted_decrypt_key`-shape check is the one that
    /// fires. encode_invite_url does NOT verify the token sig, so mutating
    /// invitee_hint without re-signing is fine here.
    fn make_targeted_invite_only_payload() -> CommunityInvitePayload {
        let mut p = make_invite_only_payload_correct();
        if let Some(t) = p.invite_token.as_mut() {
            t.invitee_hint = Some(OwnerAddr([5u8; 16]));
        }
        // Targeted shape: per-device envelopes, empty single blob.
        let env = std::mem::take(&mut p.epoch_snapshot.sealed_epoch_key);
        p.epoch_snapshot.sealed_epoch_keys = vec![env];
        // Targeted invites carry no URL-borne decrypt key.
        p.untargeted_decrypt_key = None;
        p
    }

    #[test]
    fn untargeted_key_rejected_on_targeted_invite_only() {
        // A valid targeted invite-only payload + a smuggled untargeted_decrypt_key
        // → the key is illegal on a targeted payload (it would leak the epoch key
        // to any URL holder), so encode rejects with UntargetedKeyNotAllowed.
        let mut p = make_targeted_invite_only_payload();
        p.untargeted_decrypt_key = Some([1u8; 32]);
        assert!(matches!(
            encode_invite_url(&p),
            Err(InviteUrlError::UntargetedKeyNotAllowed)
        ));
    }

    #[test]
    fn untargeted_key_round_trips_on_untargeted_invite_only() {
        // make_invite_only_payload_correct() is already untargeted
        // (invite_token Some, invitee_hint None) and otherwise valid.
        let mut p = make_invite_only_payload_correct();
        p.untargeted_decrypt_key = Some([7u8; 32]);
        let url = encode_invite_url(&p).expect("untargeted invite-only encodes");
        let back = decode_invite_url(&url).expect("decodes");
        assert_eq!(back.untargeted_decrypt_key, Some([7u8; 32]));
    }

    /// The decode-side guard is the REAL confidentiality defense: an attacker
    /// crafts URL bytes directly and never calls the guarded `encode_invite_url`.
    /// CBOR-encode a payload that smuggles the untargeted decrypt key onto a
    /// TARGETED invite-only payload (invitee_hint Some) — bypassing the encode
    /// gate — and confirm `decode_invite_url` rejects it before the key can leak
    /// the epoch secret. Without this test the decode-side guard is unverified
    /// (the round-trip test above only exercises decode's accept path).
    #[test]
    fn decode_rejects_smuggled_untargeted_key_on_targeted_payload() {
        let mut p = make_targeted_invite_only_payload(); // valid targeted shape
        p.untargeted_decrypt_key = Some([1u8; 32]); // smuggled secret
                                                    // Bypass the encode-side guard by CBOR-encoding directly.
        let cbor = canonical_cbor_encode(&p).expect("cbor encode");
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&cbor);
        let url = format!("harmony://invite/{b64}");
        assert!(matches!(
            decode_invite_url(&url),
            Err(InviteUrlError::UntargetedKeyNotAllowed)
        ));
    }

    // ── ZEB-369: targeted invite-only sealed-key shape ───────────────────

    /// A structurally-valid targeted invite-only payload (empty
    /// `sealed_epoch_key`, one ≥92-byte envelope in `sealed_epoch_keys`,
    /// `invitee_hint = Some`, no URL key) encodes and round-trips through the
    /// URL with the per-device envelopes preserved.
    #[test]
    fn targeted_invite_only_payload_round_trips_via_url() {
        let p = make_targeted_invite_only_payload();
        assert!(p.epoch_snapshot.sealed_epoch_key.is_empty());
        assert_eq!(p.epoch_snapshot.sealed_epoch_keys.len(), 1);
        let url = encode_invite_url(&p).expect("targeted invite-only encodes");
        let back = decode_invite_url(&url).expect("targeted invite-only decodes");
        assert!(back.epoch_snapshot.sealed_epoch_key.is_empty());
        assert_eq!(
            back.epoch_snapshot.sealed_epoch_keys,
            p.epoch_snapshot.sealed_epoch_keys
        );
        assert_eq!(back.untargeted_decrypt_key, None);
    }

    /// A targeted invite-only payload whose `sealed_epoch_keys` is empty (no
    /// per-device envelope) is rejected by the shape gate at encode time.
    #[test]
    fn targeted_invite_only_rejects_empty_sealed_epoch_keys() {
        let mut p = make_targeted_invite_only_payload();
        p.epoch_snapshot.sealed_epoch_keys.clear();
        assert!(matches!(
            encode_invite_url(&p),
            Err(InviteUrlError::InvalidSealedEpochKeysShape)
        ));
    }

    /// A targeted invite-only payload with a too-short envelope (< 92 bytes) is
    /// rejected by the shape gate.
    #[test]
    fn targeted_invite_only_rejects_short_envelope() {
        let mut p = make_targeted_invite_only_payload();
        p.epoch_snapshot.sealed_epoch_keys = vec![vec![0u8; 50]]; // too short
        assert!(matches!(
            encode_invite_url(&p),
            Err(InviteUrlError::InvalidSealedEpochKeysShape)
        ));
    }

    /// Qodo Bug 1 (PR #286): a targeted payload with an OVERSIZED envelope
    /// (> 92 bytes) is rejected. `open_from_owner` decrypts the whole ciphertext
    /// slice, so accepting an over-long envelope at the URL boundary would let a
    /// crafted invite force unbounded AEAD work during redemption.
    #[test]
    fn targeted_invite_only_rejects_oversized_envelope() {
        let mut p = make_targeted_invite_only_payload();
        p.epoch_snapshot.sealed_epoch_keys = vec![vec![0u8; 4096]]; // oversized
        assert!(matches!(
            encode_invite_url(&p),
            Err(InviteUrlError::InvalidSealedEpochKeysShape)
        ));
    }

    /// Qodo Bug 2 (PR #286): a targeted payload carrying more than
    /// MAX_ENROLLED_DEVICE_KEYS envelopes is rejected at the URL boundary — one
    /// envelope is tried per device at redeem, so an unbounded list is O(n)
    /// decrypt amplification on untrusted input.
    #[test]
    fn targeted_invite_only_rejects_too_many_envelopes() {
        let mut p = make_targeted_invite_only_payload();
        let too_many = crate::community_membership::MAX_ENROLLED_DEVICE_KEYS + 1;
        p.epoch_snapshot.sealed_epoch_keys = vec![vec![0u8; 92]; too_many];
        assert!(matches!(
            encode_invite_url(&p),
            Err(InviteUrlError::InvalidSealedEpochKeysShape)
        ));
    }

    /// The exact-count boundary: exactly MAX_ENROLLED_DEVICE_KEYS well-formed
    /// envelopes is still accepted (the cap is inclusive).
    #[test]
    fn targeted_invite_only_accepts_max_envelopes() {
        let mut p = make_targeted_invite_only_payload();
        let at_cap = crate::community_membership::MAX_ENROLLED_DEVICE_KEYS;
        p.epoch_snapshot.sealed_epoch_keys = vec![vec![0u8; 92]; at_cap];
        assert!(encode_invite_url(&p).is_ok());
    }

    /// `sealed_epoch_keys` set on an OPEN payload (non-targeted) is rejected —
    /// the per-device list is only valid on a targeted invite-only payload.
    #[test]
    fn sealed_epoch_keys_rejected_on_open_payload() {
        let mut p = make_open_payload_correct();
        p.epoch_snapshot.sealed_epoch_keys = vec![vec![0u8; 92]];
        assert!(matches!(
            encode_invite_url(&p),
            Err(InviteUrlError::InvalidSealedEpochKeysShape)
        ));
    }

    /// An untargeted invite-only payload (invitee_hint None) that OMITS the
    /// untargeted_decrypt_key must be rejected at the mint site: without the key
    /// `mint_redemption` would derive the redeemer's device-#2 key (targeted path)
    /// and fail to decrypt the ephemeral-sealed epoch key. Catching it at encode
    /// keeps the un-redeemable URL from ever leaving.
    #[test]
    fn encode_rejects_untargeted_invite_only_missing_key() {
        let mut p = make_invite_only_payload_correct(); // untargeted, key Some
        p.untargeted_decrypt_key = None; // drop the required key
        assert!(matches!(
            encode_invite_url(&p),
            Err(InviteUrlError::UntargetedKeyMissing)
        ));
    }

    /// Decode-side mirror: a hostile/buggy URL whose CBOR carries an untargeted
    /// invite-only payload with no untargeted_decrypt_key is rejected on receipt
    /// (CBOR-encoded directly to bypass the encode-side guard), so the redeemer
    /// never spins up an engine for an invite it can't decrypt.
    #[test]
    fn decode_rejects_untargeted_invite_only_missing_key() {
        let mut p = make_invite_only_payload_correct(); // untargeted, key Some
        p.untargeted_decrypt_key = None; // drop the required key
        let cbor = canonical_cbor_encode(&p).expect("cbor encode");
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&cbor);
        let url = format!("harmony://invite/{b64}");
        assert!(matches!(
            decode_invite_url(&url),
            Err(InviteUrlError::UntargetedKeyMissing)
        ));
    }

    // ── decode_invite_url ────────────────────────────────────────────

    /// Encode a payload with correct key length, then manually replace
    /// the sealed_epoch_key bytes in the decoded struct and re-encode
    /// with the wrong length to produce a URL that passes base64+CBOR
    /// but fails the decode-side length check.
    #[test]
    fn decode_invite_url_rejects_wrong_sealed_key_length() {
        // Build a payload with a wrong sealed_epoch_key (50 bytes) and
        // CBOR-encode it directly (bypassing encode_invite_url's gate)
        // so we get a URL that the CBOR decoder will accept but the
        // length check will reject.
        let mut payload = make_open_payload_correct();
        payload.epoch_snapshot.sealed_epoch_key = vec![0u8; 50]; // wrong length
        let cbor = canonical_cbor_encode(&payload).expect("cbor encode");
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&cbor);
        let url = format!("harmony://invite/{b64}");

        let err = decode_invite_url(&url).unwrap_err();
        assert!(
            matches!(
                err,
                InviteUrlError::InvalidSealedEpochKeyLen {
                    mode: "open",
                    expected: 32,
                    got: 50,
                }
            ),
            "unexpected err: {err}"
        );
    }

    // ── invite-only sealed_epoch_key length contract (CR Minor PR #106 R6) ─

    /// encode_invite_url accepts an invite-only payload with the correct
    /// 92-byte sealed_epoch_key.
    #[test]
    fn encode_invite_url_accepts_correct_invite_only_key_len() {
        let payload = make_invite_only_payload_correct();
        assert!(
            encode_invite_url(&payload).is_ok(),
            "invite-only payload with 92-byte sealed_epoch_key must encode successfully"
        );
    }

    /// encode_invite_url rejects an invite-only payload whose sealed_epoch_key
    /// is not exactly 92 bytes.
    #[test]
    fn encode_invite_url_rejects_wrong_sealed_key_length_for_invite_only() {
        let mut payload = make_invite_only_payload_correct();
        payload.epoch_snapshot.sealed_epoch_key = vec![0u8; 32]; // wrong for invite-only
        let err = encode_invite_url(&payload).unwrap_err();
        assert!(
            matches!(
                err,
                InviteUrlError::InvalidSealedEpochKeyLen {
                    mode: "invite-only",
                    expected: 92,
                    got: 32,
                }
            ),
            "unexpected err: {err}"
        );
    }

    /// decode_invite_url rejects an invite-only URL whose CBOR-decoded
    /// sealed_epoch_key is not exactly 92 bytes.
    #[test]
    fn decode_invite_url_rejects_wrong_sealed_key_length_for_invite_only() {
        // Build an invite-only payload with wrong sealed_epoch_key length and
        // CBOR-encode directly (bypassing encode_invite_url's gate) so we
        // produce a URL the CBOR decoder accepts but the length check rejects.
        let mut payload = make_invite_only_payload_correct();
        payload.epoch_snapshot.sealed_epoch_key = vec![0u8; 32]; // wrong for invite-only
        let cbor = canonical_cbor_encode(&payload).expect("cbor encode");
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&cbor);
        let url = format!("harmony://invite/{b64}");

        let err = decode_invite_url(&url).unwrap_err();
        assert!(
            matches!(
                err,
                InviteUrlError::InvalidSealedEpochKeyLen {
                    mode: "invite-only",
                    expected: 92,
                    got: 32,
                }
            ),
            "unexpected err: {err}"
        );
    }

    // ── ZEB-339: admin_bootstrap embedded EnrollmentCert gate ──────────

    /// encode_invite_url rejects an invite-only payload whose admin_bootstrap
    /// is present but carries no embedded EnrollmentCert. Without it the URL
    /// would encode/decode cleanly then die late at redeem with
    /// BootstrapSignatureInvalid (verify_admin_bootstrap resolves the signer
    /// via enrolled_key_from_cert, which reads admin_bootstrap.enrollment).
    #[test]
    fn encode_invite_url_rejects_invite_only_bootstrap_missing_enrollment() {
        let mut payload = make_invite_only_payload_correct();
        payload.admin_bootstrap.as_mut().unwrap().enrollment = None;
        let err = encode_invite_url(&payload).unwrap_err();
        assert!(
            matches!(err, InviteUrlError::InviteOnlyBootstrapMissingEnrollment),
            "unexpected err: {err}"
        );
    }

    /// decode_invite_url rejects an invite-only URL whose admin_bootstrap is
    /// present but carries no embedded EnrollmentCert. CBOR-encode directly
    /// (bypassing encode_invite_url's gate) so the bytes are well-formed but
    /// the decode-time embedded-cert check rejects them. make_invite_only_
    /// payload_correct already supplies inviter_enrollment=Some and a 92-byte
    /// sealed_epoch_key, so no earlier decode gate fires first.
    #[test]
    fn decode_invite_url_rejects_invite_only_bootstrap_missing_enrollment() {
        let mut payload = make_invite_only_payload_correct();
        payload.admin_bootstrap.as_mut().unwrap().enrollment = None;
        let cbor = canonical_cbor_encode(&payload).expect("cbor encode");
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&cbor);
        let url = format!("harmony://invite/{b64}");

        let err = decode_invite_url(&url).unwrap_err();
        assert!(
            matches!(err, InviteUrlError::InviteOnlyBootstrapMissingEnrollment),
            "unexpected err: {err}"
        );
    }

    // ── ZEB-285 Phase 1: PreForkSnapshot + BoundedChannelLogSnapshot ────

    #[test]
    fn pre_fork_snapshot_canonical_cbor_roundtrip_and_keys() {
        use crate::community_membership::{MembershipEventKind, SignedMembershipEvent};
        use crate::owner_state_types::Hlc;
        use std::collections::BTreeMap;

        let original_id = SpaceId([0xa0; 16]);
        let admin = OwnerAddr([0xaa; 16]);

        // Construct a stub signed Join event (sig is all-zeros, no crypto needed
        // for the roundtrip test — only structure matters).
        let admin_join = SignedMembershipEvent {
            signer_certs: Vec::new(),
            id: [0x01; 16],
            community_id: original_id,
            kind: MembershipEventKind::Join,
            actor: admin,
            at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "t".to_string(),
            },
            // Stub event with zeroed sig — this test exercises CBOR roundtrip only,
            // not crypto verification.
            sig: [0u8; 64],
            countersig: None,
            enrollment: None,
        };

        let mut identity_pubs: BTreeMap<OwnerAddr, [u8; 64]> = BTreeMap::new();
        identity_pubs.insert(admin, [0xbb; 64]);

        let snapshot = PreForkSnapshot {
            original_community_id: original_id,
            original_community_name: "Test Community".to_string(),
            membership_events: vec![admin_join],
            channel_log: BoundedChannelLogSnapshot::default(),
            identity_pubs,
            forked_at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "t".to_string(),
            },
            parent_lineage: Vec::new(),
            fork_reason: None,
        };

        let bytes = canonical_cbor_encode(&snapshot).expect("encode");
        let decoded: PreForkSnapshot = ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(snapshot, decoded);

        // Verify top-level field keys: oi, on, ev, cl, ip, ts (all 2-char).
        let value: ciborium::Value = ciborium::de::from_reader(&bytes[..]).expect("re-decode");
        let map = value.as_map().expect("outer is map");
        for expected in &["oi", "on", "ev", "cl", "ip", "ts"] {
            assert!(
                map.iter()
                    .any(|(k, _): &(ciborium::Value, ciborium::Value)| {
                        k.as_text() == Some(*expected)
                    }),
                "expected key {} in snapshot encoding",
                expected
            );
        }
    }

    // ── ZEB-285 Phase 1 Task 4: CommunityInvitePayload fork-lineage fields ─

    #[test]
    fn invite_payload_without_pre_fork_snapshot_byte_compat() {
        // ZEB-285: a CommunityInvitePayload with both forked_from = None
        // AND pre_fork_snapshot = None must encode byte-identical to
        // pre-ZEB-285 wire form (no "ff" or "fs" keys emitted).
        let cid = SpaceId([0xc0; 16]);
        let admin = OwnerAddr([0xaa; 16]);
        let payload = CommunityInvitePayload {
            inviter_signer_certs: Vec::new(),
            community_id: cid,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: vec![0u8; 32],
                sealed_epoch_keys: Vec::new(),
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr: admin,
            community_name: "test".to_string(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
            admin_bootstrap: None,
            admin_identity_pub: None,
            forked_from: None,
            pre_fork_snapshot: None,
            inviter_enrollment: None,
            untargeted_decrypt_key: None,
        };

        let bytes = canonical_cbor_encode(&payload).expect("encode");
        let value: ciborium::Value =
            ciborium::de::from_reader(&bytes[..]).expect("decode as value");
        let map = value.as_map().expect("outer is map");

        assert!(
            !map.iter()
                .any(|(k, _): &(ciborium::Value, ciborium::Value)| { k.as_text() == Some("ff") }),
            "forked_from=None should be omitted"
        );
        assert!(
            !map.iter()
                .any(|(k, _): &(ciborium::Value, ciborium::Value)| { k.as_text() == Some("fs") }),
            "pre_fork_snapshot=None should be omitted"
        );
    }

    #[test]
    fn invite_payload_with_pre_fork_snapshot_roundtrip() {
        use crate::community_membership::{MembershipEventKind, SignedMembershipEvent};
        use crate::owner_state_types::Hlc;
        use std::collections::BTreeMap;

        let cid = SpaceId([0xc0; 16]);
        let forked_from_id = SpaceId([0xa0; 16]);
        let snapshot_original_id = SpaceId([0xa1; 16]);
        let admin = OwnerAddr([0xaa; 16]);

        let admin_join = SignedMembershipEvent {
            signer_certs: Vec::new(),
            id: [0x01; 16],
            community_id: snapshot_original_id,
            kind: MembershipEventKind::Join,
            actor: admin,
            at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "t".to_string(),
            },
            sig: [0u8; 64],
            countersig: None,
            enrollment: None,
        };

        let mut identity_pubs: BTreeMap<OwnerAddr, [u8; 64]> = BTreeMap::new();
        identity_pubs.insert(admin, [0xbb; 64]);

        let snapshot = PreForkSnapshot {
            original_community_id: snapshot_original_id,
            original_community_name: "Original".to_string(),
            membership_events: vec![admin_join],
            channel_log: BoundedChannelLogSnapshot::default(),
            identity_pubs,
            forked_at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "t".to_string(),
            },
            parent_lineage: Vec::new(),
            fork_reason: None,
        };

        let payload = CommunityInvitePayload {
            inviter_signer_certs: Vec::new(),
            community_id: cid,
            epoch_snapshot: InviteEpochSnapshot {
                epoch: 0,
                sealed_epoch_key: vec![0u8; 32],
                sealed_epoch_keys: Vec::new(),
                state_snapshot: MaterializedCommunityState::default(),
            },
            admin_addr: admin,
            community_name: "fork".to_string(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
            admin_bootstrap: None,
            admin_identity_pub: None,
            forked_from: Some(forked_from_id),
            pre_fork_snapshot: Some(snapshot.clone()),
            inviter_enrollment: None,
            untargeted_decrypt_key: None,
        };

        let bytes = canonical_cbor_encode(&payload).expect("encode");
        let decoded: CommunityInvitePayload =
            ciborium::de::from_reader(&bytes[..]).expect("decode");

        assert_eq!(decoded.forked_from, Some(forked_from_id));
        assert_eq!(
            decoded
                .pre_fork_snapshot
                .as_ref()
                .unwrap()
                .original_community_id,
            snapshot_original_id
        );
        assert_eq!(decoded.pre_fork_snapshot, Some(snapshot));
    }

    // ── ZEB-339: inviter_enrollment field ────────────────────────────

    #[test]
    fn invite_payload_inviter_enrollment_roundtrip() {
        // ZEB-339: a payload carrying the inviter's EnrollmentCert encodes
        // and decodes losslessly (the "ec" key round-trips).
        let cert = crate::community_membership::mint_test_owner(0x5C).cert;
        let mut payload = make_open_payload_correct();
        payload.inviter_enrollment = Some(cert.clone());

        let bytes = canonical_cbor_encode(&payload).expect("encode");
        let decoded: CommunityInvitePayload =
            ciborium::de::from_reader(&bytes[..]).expect("decode");

        assert_eq!(decoded.inviter_enrollment, Some(cert));
    }

    #[test]
    fn invite_payload_without_inviter_enrollment_decodes_to_none() {
        // ZEB-339 back-compat: a payload WITHOUT the "ec" key (pre-ZEB-339
        // wire form) decodes with inviter_enrollment = None rather than
        // failing, and the key is omitted on encode when None.
        let payload = make_open_payload_correct();
        assert_eq!(payload.inviter_enrollment, None);

        let bytes = canonical_cbor_encode(&payload).expect("encode");
        // "ec" must be absent from the wire form when None.
        let value: ciborium::Value =
            ciborium::de::from_reader(&bytes[..]).expect("decode as value");
        let map = value.as_map().expect("outer is map");
        assert!(
            !map.iter()
                .any(|(k, _): &(ciborium::Value, ciborium::Value)| { k.as_text() == Some("ec") }),
            "inviter_enrollment=None should be omitted (no `ec` key)"
        );

        // And a payload encoded without "ec" decodes back to None.
        let decoded: CommunityInvitePayload =
            ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(decoded.inviter_enrollment, None);
    }

    // ── ZEB-369: InviteEpochSnapshot.sealed_epoch_keys wire format ────────

    /// A snapshot carrying two sealed envelopes (the targeted-invite shape)
    /// survives a canonical-CBOR encode/decode round-trip with both envelopes
    /// preserved in order, and the encoded form carries the `se` key.
    #[test]
    fn snapshot_with_two_sealed_envelopes_round_trips() {
        let env_a = vec![0xAAu8; 92];
        let env_b = vec![0xBBu8; 92];
        let snap = InviteEpochSnapshot {
            epoch: 7,
            // Targeted invites leave the single blob empty.
            sealed_epoch_key: Vec::new(),
            sealed_epoch_keys: vec![env_a.clone(), env_b.clone()],
            state_snapshot: MaterializedCommunityState::default(),
        };
        let bytes = canonical_cbor_encode(&snap).expect("encode");

        // The `se` key MUST be present on a targeted snapshot.
        let value: ciborium::Value =
            ciborium::de::from_reader(&bytes[..]).expect("decode as value");
        let map = value.as_map().expect("outer is map");
        let se = map
            .iter()
            .find(|(k, _): &&(ciborium::Value, ciborium::Value)| k.as_text() == Some("se"))
            .map(|(_, v)| v)
            .expect("targeted snapshot encodes the `se` key");
        let arr = se.as_array().expect("`se` is a CBOR array");
        assert_eq!(arr.len(), 2, "two envelopes in the array");
        // Each element is a CBOR byte-string (major type 2), not an array-of-u8.
        assert!(
            arr.iter().all(|v| v.as_bytes().is_some()),
            "each `se` envelope must encode as a bstr"
        );

        let back: InviteEpochSnapshot =
            crate::owner_state_crypto::canonical_cbor_decode(&bytes).expect("decode snapshot");
        assert_eq!(back.epoch, 7);
        assert_eq!(back.sealed_epoch_key, Vec::<u8>::new());
        assert_eq!(back.sealed_epoch_keys, vec![env_a, env_b]);
    }

    /// Back-compat: an OLD-format snapshot CBOR (only `ep`/`sk`/`ss`, no `se`
    /// key — exactly what a pre-ZEB-369 build emitted) decodes with
    /// `sealed_epoch_keys` defaulting to empty (proves `#[serde(default)]`).
    #[test]
    fn old_format_snapshot_decodes_with_empty_sealed_epoch_keys() {
        // Hand-build the pre-ZEB-369 wire form: a 3-key CBOR map
        // { "ep": 0, "sk": <32-byte bstr>, "ss": <state map> }.
        // Encode a state-snapshot sub-map the same way the derive would so the
        // `ss` value is structurally valid.
        let old_value = ciborium::Value::Map(vec![
            (
                ciborium::Value::Text("ep".into()),
                ciborium::Value::Integer(0u8.into()),
            ),
            (
                ciborium::Value::Text("sk".into()),
                ciborium::Value::Bytes(vec![0u8; 32]),
            ),
            (
                ciborium::Value::Text("ss".into()),
                // MaterializedCommunityState::default() → { "mb": {}, "pl": {} }
                // ("ch" is skip_serializing_if-empty). Build it via the type's
                // own encoder to stay faithful.
                {
                    let state_bytes = canonical_cbor_encode(&MaterializedCommunityState::default())
                        .expect("encode state");
                    ciborium::de::from_reader(&state_bytes[..]).expect("state value")
                },
            ),
        ]);
        let mut old_bytes = Vec::new();
        ciborium::ser::into_writer(&old_value, &mut old_bytes).expect("encode old form");
        // Sanity: the old form carries no `se` key.
        assert!(
            !old_bytes.windows(2).any(|w| w == b"se"),
            "old form must not carry the `se` key"
        );

        let decoded: InviteEpochSnapshot =
            ciborium::de::from_reader(&old_bytes[..]).expect("old-format snapshot decodes");
        assert_eq!(decoded.epoch, 0);
        assert_eq!(decoded.sealed_epoch_key, vec![0u8; 32]);
        assert!(
            decoded.sealed_epoch_keys.is_empty(),
            "missing `se` key must default to an empty vec"
        );
    }

    /// An untargeted/open snapshot (empty `sealed_epoch_keys`) encodes
    /// BYTE-IDENTICALLY to the pre-ZEB-369 wire form: `skip_serializing_if`
    /// omits the `se` key entirely. We pin the expected bytes against an
    /// independently-built old-format CBOR map (the exact bytes a pre-change
    /// build produced) so a regression that started emitting an empty `se`
    /// array (or otherwise perturbed the encoding) is caught.
    #[test]
    fn untargeted_snapshot_encodes_byte_identical_to_pre_change_form() {
        let snap = InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: vec![0u8; 32],
            sealed_epoch_keys: Vec::new(),
            state_snapshot: MaterializedCommunityState::default(),
        };
        let new_bytes = canonical_cbor_encode(&snap).expect("encode");

        // The captured pre-change encoding: the 3-key canonical map the derive
        // emitted before the `se` field existed. canonical_cbor_encode sorts
        // map keys, so reconstruct via the same encoder for the `ss` sub-map
        // and assemble the outer map in canonical (length-then-lex) key order.
        let state_value: ciborium::Value = {
            let b = canonical_cbor_encode(&MaterializedCommunityState::default())
                .expect("encode state");
            ciborium::de::from_reader(&b[..]).expect("state value")
        };
        let expected_value = ciborium::Value::Map(vec![
            (
                ciborium::Value::Text("ep".into()),
                ciborium::Value::Integer(0u8.into()),
            ),
            (
                ciborium::Value::Text("sk".into()),
                ciborium::Value::Bytes(vec![0u8; 32]),
            ),
            (ciborium::Value::Text("ss".into()), state_value),
        ]);
        // `canonical_cbor_encode` is just `ciborium::into_writer` under the
        // hood (it preserves serde field-declaration order — no key sorting),
        // so encoding this hand-built Value mirrors exactly what the derive
        // emits for the field order ep, sk, (se skipped), ss.
        let mut expected_bytes = Vec::new();
        ciborium::ser::into_writer(&expected_value, &mut expected_bytes)
            .expect("encode expected form");

        assert_eq!(
            new_bytes, expected_bytes,
            "untargeted snapshot must encode byte-identical to the pre-ZEB-369 form"
        );
        // Belt-and-suspenders: no `se` key on the wire.
        assert!(
            !new_bytes.windows(2).any(|w| w == b"se"),
            "empty sealed_epoch_keys must omit the `se` key"
        );
    }
}

#[cfg(test)]
mod open_join_packet_tests {
    use super::*;
    use crate::community_membership::{MembershipEventKind, SignedMembershipEvent};

    fn joiner_signing_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[11u8; 32])
    }

    /// Minimal self-signed Join for the joiner. Mirrors the inline
    /// `SignedMembershipEvent` literal used by the invite-only payload
    /// helpers above (no dedicated `sample_join_event` helper exists in
    /// this file); the enrollment cert comes from `mint_test_owner`.
    fn sample_join_event() -> SignedMembershipEvent {
        SignedMembershipEvent {
            signer_certs: Vec::new(),
            id: [0u8; 16],
            community_id: SpaceId([1u8; 16]),
            kind: MembershipEventKind::Join,
            actor: OwnerAddr([0u8; 16]),
            at: Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "j".to_string(),
            },
            sig: [0u8; 64],
            countersig: None,
            enrollment: Some(crate::community_membership::mint_test_owner(0x4A).cert),
        }
    }

    fn sample_request() -> OpenJoinRequest {
        OpenJoinRequest {
            community_id: SpaceId([1u8; 16]),
            join_event: sample_join_event(),
            joiner_identity_pub: [4u8; 64],
            signing_device_hash: DeviceIdentityHash([7u8; 16]),
            epoch_auth: [9u8; 32],
            nonce: [2u8; 16],
            created_at: Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "j".to_string(),
            },
        }
    }

    #[test]
    fn open_join_packet_round_trips_and_verifies() {
        let sk = joiner_signing_key();
        let req = sample_request();
        let packet = build_signed_open_join_packet(req.clone(), &sk).expect("build");
        let wire = encode_packet(&packet).expect("encode");
        // First byte is the open-join discriminant.
        assert_eq!(wire[0], 0x11, "open-join discriminant");
        let decoded = decode_packet(&wire).expect("decode");
        match decoded {
            CommunityInvitePacket::OpenJoin { req: got, .. } => {
                assert_eq!(got.community_id, req.community_id);
                assert_eq!(got.joiner_identity_pub, req.joiner_identity_pub);
                assert_eq!(got.epoch_auth, req.epoch_auth);
                assert_eq!(got.nonce, req.nonce);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn invite_and_open_join_discriminants_are_distinct() {
        let sk = joiner_signing_key();
        let wire =
            encode_packet(&build_signed_open_join_packet(sample_request(), &sk).unwrap()).unwrap();
        assert_eq!(wire[0], 0x11);
        assert_ne!(wire[0], 0x10);
    }
}
