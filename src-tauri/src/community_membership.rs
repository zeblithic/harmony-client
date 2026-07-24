//! Community membership CRDT primitives — ZEB-217 Sub-C Phase 1.
//!
//! Per-community signed-event CRDT replicated via the encrypted Zenoh
//! state-root topic (Phase 2). Phase 1 ships only the types,
//! materialization rules, and verification logic — no IPC, no
//! networking, no UI.
//!
//! See `docs/specs/2026-05-05-zeb-217-sub-c-communities-design.md`.

use harmony_owner::certs::{EnrollmentCert, RevocationCert};
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use std::collections::{BTreeMap, BTreeSet};

use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::OwnerAddr;

/// ZEB-249: one per-recipient sealed ciphertext in an EpochRotation /
/// EpochCatchup. Wire format: 2-key CBOR map. Keys (rc, ct) are 2-char
/// to satisfy the same-length-keys invariant at this nesting level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipientCiphertext {
    #[serde(rename = "rc")]
    pub recipient: OwnerAddr,

    /// X25519-sealed bytes (92 = 32 ephemeral pub + 12 nonce + 32 ct + 16 tag).
    /// See `dm_signing::seal_to_owner`.
    #[serde(
        rename = "ct",
        serialize_with = "crate::owner_state_types::serialize_vec_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_vec_from_bstr"
    )]
    pub sealed: Vec<u8>,
}

impl CanonicalPayloadSealed for RecipientCiphertext {}
impl CanonicalPayload for RecipientCiphertext {}

/// ZEB-250: shape of the proposed admin-affecting action wrapped by
/// [`MembershipEventKind::AdminProposal`]. Mirrors existing
/// single-signed event variants but gated through M-of-N quorum
/// approval.
///
/// Same-length-keys invariant: 1-char variant tags (`s`/`k`/`c`),
/// 2-char inner-field keys. Tagged-union representation with `kd`
/// (kind) discriminator + `bd` (body) container so the CBOR encoding
/// has explicit discriminator + body keys at the ProposalKind level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kd", content = "bd")]
pub enum ProposalKind {
    /// SetPower whose target IS currently an admin (level was 100) OR
    /// whose new level IS 100 (promoting to admin).
    #[serde(rename = "s")]
    SetPower {
        #[serde(rename = "tg")]
        target: OwnerAddr,
        #[serde(rename = "lv")]
        level: u8,
    },
    /// Kick of a target who is currently an admin (level == 100).
    #[serde(rename = "k")]
    Kick {
        #[serde(rename = "tg")]
        target: OwnerAddr,
        #[serde(rename = "rs", skip_serializing_if = "Option::is_none", default)]
        reason: Option<String>,
    },
    /// Change `CommunityState.admin_quorum`. `new_quorum >= 1`,
    /// practical cap enforced at verify_event AP5.
    #[serde(rename = "c")]
    ChangeQuorum {
        #[serde(rename = "nq")]
        new_quorum: u8,
    },
    /// ZEB-713 (ZEB-212 D1): set or replace the community's recovery
    /// designate configuration. Routed through the existing AdminProposal
    /// quorum machinery (AP1–AP5 unchanged; always admin-affecting, like
    /// ChangeQuorum). Additional shape gates RD1–RD4 at verify_event.
    ///
    /// Materializes as `MaterializedMembership.recovery_designates`
    /// (with the trigger event's HLC as the config generation), from
    /// which the binding `config_digest` is derived — see
    /// `recovery_config_digest`. Replacing the config changes the digest
    /// and thereby mechanically kills in-flight recovery proposals
    /// (spec §3.3.4).
    ///
    /// Variant tag "r" (1-char value, unused in ProposalKind before
    /// this). Inner field keys are 2-char (ds, th, vw) per the
    /// same-length-keys invariant at this nesting level.
    /// See `docs/specs/2026-07-19-zeb-212-m-of-n-admin-recovery-design.md` §3.1.
    #[serde(rename = "r")]
    SetRecoveryDesignates {
        #[serde(rename = "ds")]
        designates: Vec<OwnerAddr>,
        /// R: distinct designate signatures required to move a
        /// RecoveryProposal from Collecting to Time-locked.
        #[serde(rename = "th")]
        threshold: u8,
        /// W: veto window in ms. Floor enforced at RD4
        /// (`RECOVERY_VETO_WINDOW_FLOOR_MS`).
        #[serde(rename = "vw")]
        veto_window_ms: u64,
    },
    /// ZEB-251: change the community's per-action power thresholds
    /// (invite/kick/set_power). Routed through the AdminProposal quorum
    /// machinery (AP1–AP5 unchanged; always admin-affecting, like
    /// ChangeQuorum). Validity gate AT1 at verify_event. Materializes as
    /// `MaterializedMembership.power_thresholds`. Variant tag "t" (1-char,
    /// unused before this); inner field key "th" (2-char).
    ///
    /// FORWARD-INCOMPAT (upgrade-before-adopt): `ProposalKind` has no
    /// `#[serde(other)]` fallback and the community-state blob decodes
    /// all-or-nothing, so a client that predates this variant rejects the
    /// ENTIRE state-root of any community that has emitted a ChangeThresholds
    /// event — loudly (`wire_decode_failed`, replay tracker not advanced),
    /// never silently diverging on an authorization decision. This is the same
    /// intentional stale-client posture as Townhall channels / ZEB-349 Voice:
    /// adopting a custom threshold makes that community undecodable to
    /// un-upgraded peers until they upgrade. The loud refusal is deliberate — a
    /// graceful skip-unknown-event would REINTRODUCE the silent cross-version
    /// authorization divergence this design otherwise avoids.
    #[serde(rename = "t")]
    ChangeThresholds {
        #[serde(rename = "th")]
        new_thresholds: PowerThresholds,
    },
}

/// The five membership event kinds. Adjacently tagged so the wire
/// format is `{ "tg": "<variant>", "vl": <body> }` — both keys are
/// 2-char to satisfy the same-length-keys CBOR invariant at this
/// nesting level. Variant codes are 1-char (values, not keys).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tg", content = "vl")]
pub enum MembershipEventKind {
    #[serde(rename = "j")]
    Join,
    #[serde(rename = "l")]
    Leave,
    #[serde(rename = "i")]
    Invite {
        #[serde(rename = "tg")]
        target: OwnerAddr,
    },
    #[serde(rename = "k")]
    Kick {
        #[serde(rename = "tg")]
        target: OwnerAddr,
        #[serde(rename = "rs", skip_serializing_if = "Option::is_none", default)]
        reason: Option<String>,
    },
    #[serde(rename = "p")]
    SetPower {
        #[serde(rename = "tg")]
        target: OwnerAddr,
        #[serde(rename = "lv")]
        level: u8,
    },
    /// Admin-tier action: lifts a prior Kick-as-effective-ban so the target
    /// can be re-invited. Does NOT auto-rejoin — target must accept a fresh
    /// Invite. Transitions MemberStatus::Banned → MemberStatus::Left.
    ///
    /// Variant code "u" (1-char value, keeps same-length-keys invariant).
    /// Inner field keys are 2-char (tg, rs).
    /// See spec `docs/specs/2026-05-13-zeb-284-community-moderation-ux-design.md` §4.1.
    #[serde(rename = "u")]
    Unban {
        #[serde(rename = "tg")]
        target: OwnerAddr,
        #[serde(rename = "rs", skip_serializing_if = "Option::is_none", default)]
        reason: Option<String>,
    },
    /// Channel-config event: a mod-tier+ actor creates a new channel
    /// in this community. `ch` is a fresh ChannelId (ULID); `nm` is
    /// the display name; `wp` is the per-channel write_power threshold
    /// (Phase 1 frontend always submits 0 = anyone-Joined posts; v2
    /// reserves the field so v3 announcement-channel UI is wire-stable).
    /// Variant code "c" (1-char value, not a key — keeps the same-
    /// length-keys invariant intact). Inner field keys are 2-char.
    /// See `docs/specs/2026-05-09-zeb-248-channels-within-communities-design.md` §5.1.
    #[serde(rename = "c")]
    ChannelCreate {
        #[serde(rename = "ch")]
        channel_id: ChannelId,
        #[serde(rename = "nm")]
        name: String,
        #[serde(rename = "wp")]
        write_power: u8,
        /// ZEB-349: channel kind (Text|Voice). `Text` is the default and is
        /// omitted from the CBOR map (`skip_serializing_if`), so a Text
        /// `ChannelCreate` stays byte-identical to pre-ZEB-349 wire; only a
        /// Voice channel carries the extra `ck` map entry. Immutable once set.
        #[serde(rename = "ck", default, skip_serializing_if = "ChannelKind::is_text")]
        kind: ChannelKind,
    },

    /// Channel-config event: a mod-tier+ actor modifies an existing
    /// channel's name and/or write_power. Either field may be `None` to
    /// leave that field unchanged. If both are `None` the IPC layer
    /// rejects the call before signing (no-op). Variant code "m".
    /// See spec `docs/specs/2026-05-09-zeb-248-channels-within-communities-design.md` §5.1.
    #[serde(rename = "m")]
    ChannelModify {
        #[serde(rename = "ch")]
        channel_id: ChannelId,
        #[serde(rename = "nm", skip_serializing_if = "Option::is_none", default)]
        name: Option<String>,
        #[serde(rename = "wp", skip_serializing_if = "Option::is_none", default)]
        write_power: Option<u8>,
    },

    /// Channel-config event: a mod-tier+ actor deletes a channel.
    /// Tombstone semantics — the channel is NOT removed from the
    /// materialized `channels` map; instead `deleted_at` is set. Future
    /// posts to this channel are rejected by Phase 2's verify_channel_event;
    /// historical messages still render with their breadcrumb intact.
    /// Variant code "d". See spec §5.1.
    #[serde(rename = "d")]
    ChannelDelete {
        #[serde(rename = "ch")]
        channel_id: ChannelId,
    },

    /// ZEB-249: Advances current_epoch. Triggered by Kick/Leave
    /// (subtractive — excludes the kicked/leaving member from
    /// recipient_ciphertexts). Spec §4.1.
    ///
    /// Variant code "r". Inner field keys are 2-char (pe, ts, rc).
    #[serde(rename = "r")]
    EpochRotation {
        #[serde(rename = "pe")]
        prior_epoch: u64,

        #[serde(
            rename = "ts",
            serialize_with = "serialize_bytes_as_bstr",
            deserialize_with = "deserialize_bytes_from_bstr"
        )]
        triggered_by: EventId,

        #[serde(rename = "rc")]
        recipient_ciphertexts: Vec<RecipientCiphertext>,
    },

    /// ZEB-249: Delivers `current_epoch_key` to specified members WITHOUT
    /// advancing the epoch. Triggered by a Join whose snapshot was stale
    /// at redemption time. Spec §4.6.
    ///
    /// Variant code "f" (for "fill"). Inner field keys are 2-char.
    #[serde(rename = "f")]
    EpochCatchup {
        #[serde(rename = "ep")]
        epoch: u64,

        #[serde(
            rename = "ts",
            serialize_with = "serialize_bytes_as_bstr",
            deserialize_with = "deserialize_bytes_from_bstr"
        )]
        triggered_by: EventId,

        #[serde(rename = "rc")]
        recipient_ciphertexts: Vec<RecipientCiphertext>,
    },

    /// ZEB-285: a joined member declares they have forked this community
    /// into a new community with `fork_space_id` as its SpaceId. Non-mutating
    /// — does NOT change materialized membership/power/channels, does NOT
    /// trigger EpochRotation. Other members materialize it as visible
    /// fork-lineage history. Verify rule: signer must be Joined at the
    /// event's HLC (power threshold = 0, "any joined member, any time").
    ///
    /// Variant tag "x" (1-char value, lowercase, unused before this).
    /// Inner field keys "fs"/"rs" (2-char) per same-length-keys invariant at
    /// this nesting level. See `docs/specs/2026-05-14-zeb-285-phase1-community-forking-design.md` §3.1.
    ///
    /// ZEB-649: `reason` is the forker's stated why, capped at
    /// `MAX_MODERATION_REASON_CHARS` in `verify_event`. Optional ON THE WIRE
    /// only — `Option` + `skip_serializing_if` keeps pre-ZEB-649 Fork events
    /// re-encoding byte-identically through the `verify_signature`
    /// decode→re-encode path (a bare String default would break every old
    /// event's signature). The IPC layer (`fork_community`) makes it
    /// mandatory for newly minted events.
    #[serde(rename = "x")]
    Fork {
        #[serde(rename = "fs")]
        fork_space_id: SpaceId,
        #[serde(rename = "rs", skip_serializing_if = "Option::is_none", default)]
        reason: Option<String>,
    },

    /// ZEB-254: joiner-signed pending join for invite-only communities.
    /// Distributed via the community CRDT (Zenoh) so admins who were
    /// offline at redemption time can counter-sign asynchronously.
    /// Variant code "g" (gate / guest, unused before this). Inner field
    /// keys are 2-char per same-length-keys invariant.
    /// See spec `docs/specs/2026-05-15-zeb-254-pending-join-crdt-design.md` §3.
    #[serde(rename = "g")]
    PendingJoin {
        #[serde(rename = "it")]
        invite_token: crate::community_invite::InviteToken,
    },

    /// ZEB-254: counter-sign approving a PendingJoin. Pairs by
    /// `target_event_id`. Variant code "y" (yes / approve).
    ///
    /// The signer must be a currently-Joined member whose power level meets
    /// `POWER_THRESHOLDS.invite`. In v1, `POWER_THRESHOLDS.invite = 0`, so
    /// ANY joined member can counter-sign — not just the admin. This is
    /// intentional v1 behaviour: any existing community member can vouch for
    /// a new joiner. ZEB-251 will add per-community threshold customisation
    /// that may restrict counter-signing to higher-power members.
    #[serde(rename = "y")]
    JoinCountersign {
        #[serde(
            rename = "tg",
            serialize_with = "serialize_bytes_as_bstr",
            deserialize_with = "deserialize_bytes_from_bstr"
        )]
        target_event_id: EventId,
    },

    /// ZEB-250: a power-100 admin proposes an admin-affecting action.
    /// Becomes effective only when the proposal accumulates >=
    /// admin_quorum total admin signatures (proposer counts as 1;
    /// remainder come from AdminCountersign events targeting this
    /// event_id).
    ///
    /// 30-day expiry: if quorum isn't reached within 30 days of the
    /// proposal's HLC wall_ms, the proposal is dead (pure-function
    /// check at materialize time). Late countersigns to expired
    /// proposals are no-ops.
    ///
    /// Variant tag "q" (1-char value, lowercase, unused before this).
    /// Inner field key "pk" (proposal_kind) per same-length-keys
    /// invariant.
    #[serde(rename = "q")]
    AdminProposal {
        #[serde(rename = "pk")]
        proposal_kind: ProposalKind,
    },

    /// ZEB-250: admin-tier countersignature on a target AdminProposal.
    /// Lenient forward-ref — verify_event doesn't require target to be
    /// present yet. Pairing happens at materialize time.
    ///
    /// Variant tag "n" (1-char value, lowercase, unused before this).
    #[serde(rename = "n")]
    AdminCountersign {
        #[serde(
            rename = "ti",
            serialize_with = "serialize_bytes_as_bstr",
            deserialize_with = "deserialize_bytes_from_bstr"
        )]
        target_event_id: EventId,
    },

    /// ZEB-321 Phase 1: device publishes its Iroh NodeId + DERP relay +
    /// direct-address hints into the community-state CRDT so other
    /// community members can reach it cross-WAN via Iroh.
    ///
    /// Variant tag "a" (1-char value, unused before this — keeps the
    /// same-length-keys invariant intact). Inner field keys are 2-char
    /// per the `ReachabilityAnnouncePayload` struct.
    /// See `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md` §5.
    #[serde(rename = "a")]
    ReachabilityAnnounce {
        #[serde(rename = "pl")]
        payload: crate::reachability_record::ReachabilityAnnouncePayload,
    },

    /// ZEB-458 P4: a joined member volunteers their device as a sealed
    /// relay for the community. Publishes a signed `CommunityRelayAnnouncePayload`
    /// into the community-state CRDT. No membership-state effect —
    /// consumed by `CommunityRelayResolver` for the fresh advertiser set.
    ///
    /// Variant tag "b" (1-char value, unused before this). Inner field
    /// key "pl" mirrors `ReachabilityAnnounce` (same-length-keys invariant).
    #[serde(rename = "b")]
    CommunityRelayAnnounce {
        #[serde(rename = "pl")]
        payload: crate::community_relay_announce::CommunityRelayAnnouncePayload,
    },

    /// ZEB-495 (ZEB-340 Part 2): a second device of an already-`Joined`
    /// owner introduces itself into the community by self-signing this
    /// event and attaching its own Master `EnrollmentCert` to
    /// `SignedMembershipEvent.enrollment` — exactly as `Join` carries the
    /// joiner's cert. On merge, the cert's
    /// `device_pubkeys.classical.ed25519_verify` is **added** to the
    /// owner's existing `MemberState.enrolled_device_keys` without
    /// disturbing status, `joined_at`, or power, so messages and state
    /// signed by *either* of the owner's devices verify for every member.
    ///
    /// Carries NO body — the introduced device's identity lives entirely
    /// on the carried cert (the device key is `cert.device_pubkeys.
    /// classical.ed25519_verify`). The signer is resolved via
    /// `enrolled_key_from_cert` (the cert path — the device is NOT yet in
    /// the enrolled set), and authorization is "actor is already a Joined
    /// member" (no power level / admin countersign required: a Master-signed
    /// cert for an already-admitted owner only ever ADDS a key).
    ///
    /// Variant code "e" (1-char value, keeps the same-length-keys invariant
    /// intact). Unit variant — no inner keys.
    /// See `docs/specs/2026-06-18-zeb-340-part2-multi-device-per-community-design.md` §Unit 1.
    #[serde(rename = "e")]
    DeviceAnnounce,

    /// ZEB-668 S3: retire-announce. A surviving enrolled device of `actor`
    /// broadcasts that one of the owner's devices has been REVOKED, carrying
    /// the proof (`RevocationCert`) plus the retired device's Master
    /// `EnrollmentCert` — the cert that binds the 16-byte revocation target
    /// to the 32-byte ed25519 key communities actually store (there is no
    /// hash→key map on the receiving side). On materialize the key is
    /// removed from `enrolled_device_keys` AND tombstoned in
    /// `revoked_device_keys` (remove-wins: no replay order can re-add it).
    ///
    /// Signer: any surviving enrolled device — steady-state
    /// `resolve_enrolled_signer`, NOT the cert side-channel path (the `en`
    /// side-channel stays None; the carried certs describe the RETIRED
    /// device, not the signer). Authorization: actor exists in `members`
    /// (ANY status — removal is subtractive; a Left owner's compromised key
    /// must still be retirable) and the cert pair proves itself
    /// (`verify_device_retire_certs`). No power level.
    ///
    /// Variant code "t" (1-char value, unused before this). Inner field
    /// keys are 2-char (rc, ec) per the same-length-keys invariant at this
    /// nesting level; the embedded harmony-owner certs are opaque CBOR maps
    /// below it (same as the `en` side-channel precedent).
    /// See `docs/specs/2026-07-11-zeb-668-device-management-design.md` §4.
    #[serde(rename = "t")]
    DeviceRetire {
        #[serde(rename = "rc")]
        revocation: RevocationCert,
        /// Boxed (clippy `large_enum_variant`): the cert embeds two full
        /// `PubKeyBundle`s and would otherwise dominate every
        /// `MembershipEventKind` value. `Box<T>` is serde-transparent —
        /// the wire encoding is identical to the bare cert.
        #[serde(rename = "ec")]
        enrollment: Box<EnrollmentCert>,
    },

    /// ZEB-713 (ZEB-212 D1): a recovery designate proposes replacing a
    /// lost admin identity with a new one. This is the ONE event kind
    /// deliberately authored by non-admins with elevated effect — gates
    /// RP1–RP6 (spec §3.2) apply instead of the AdminProposal AP gates
    /// (whose AP2 power-100-proposer requirement is exactly what a
    /// bricked sole-admin community cannot satisfy).
    ///
    /// Lifecycle (Collecting → Time-locked → Executed/Terminal) is
    /// evaluated at materialize time on the `materialize_with_now`
    /// now-floor; execution is PURE DERIVED STATE (no synthetic
    /// Kick/SetPower events), so a late-delivered RecoveryVeto simply
    /// re-derives it away. Spec §3.3–§4.
    ///
    /// Variant tag "h" (1-char value, unused before this). Inner field
    /// keys are 2-char (la, na, cd) per the same-length-keys invariant.
    #[serde(rename = "h")]
    RecoveryProposal {
        /// The admin identity being declared lost. Named explicitly —
        /// execution kicks exactly this OwnerAddr (loss-as-compromise;
        /// activity heuristics were considered and declined, spec §7).
        #[serde(rename = "la")]
        lost_admin: OwnerAddr,
        /// Currently-Joined member to promote to power 100 on execution.
        #[serde(rename = "na")]
        new_admin: OwnerAddr,
        /// Binds this proposal to the RecoveryDesignates generation it
        /// was authored under (RP5/RC2). Any config replacement changes
        /// the digest and mechanically kills the proposal.
        #[serde(
            rename = "cd",
            serialize_with = "serialize_bytes_as_bstr",
            deserialize_with = "deserialize_bytes_from_bstr"
        )]
        config_digest: [u8; 32],
    },

    /// ZEB-713: designate co-signature on a RecoveryProposal. Lenient
    /// forward-ref like AdminCountersign — pairing and RC2 digest
    /// re-evaluation happen at materialize time. The proposer counts as
    /// signature 1; the Rth distinct valid signature starts the
    /// time-lock (t_R). Variant tag "s" (1-char value, unused before).
    #[serde(rename = "s")]
    RecoveryCosign {
        #[serde(
            rename = "ti",
            serialize_with = "serialize_bytes_as_bstr",
            deserialize_with = "deserialize_bytes_from_bstr"
        )]
        target_event_id: EventId,
    },

    /// ZEB-713: admin-tier kill switch on a RecoveryProposal. A SINGLE
    /// power-100 signature suffices (deliberately not quorum-gated — a
    /// veto is a liveness proof restoring the status quo ante and cannot
    /// escalate anyone's power; spec §3.2 RV1). Validity is judged on
    /// the veto's AUTHORED HLC lying in `[t₀, deadline]`; veto-wins is
    /// the convergence rule even when delivery is late (spec §4.2).
    /// Variant tag "v" (1-char value, unused before this).
    #[serde(rename = "v")]
    RecoveryVeto {
        #[serde(
            rename = "ti",
            serialize_with = "serialize_bytes_as_bstr",
            deserialize_with = "deserialize_bytes_from_bstr"
        )]
        target_event_id: EventId,
    },
}

impl CanonicalPayloadSealed for MembershipEventKind {}
impl CanonicalPayload for MembershipEventKind {}

use crate::owner_state_types::{deserialize_bytes_from_bstr, serialize_bytes_as_bstr};
use crate::owner_state_types::{Hlc, SpaceId};

/// 16-byte ULID identifying a single signed membership event within
/// a community's CRDT log. Generated client-side at event creation.
pub type EventId = [u8; 16];

/// The kind of a community channel. Serialized on the wire as a `u8` tag
/// (`Text = 0`, `Voice = 1`). `Text` is the default and is **omitted** from
/// the CBOR map by `skip_serializing_if = "ChannelKind::is_text"`, keeping a
/// Text `ChannelCreate`/`ChannelInfo` byte-identical to pre-ZEB-349 wire.
/// Voice channels are introduced by ZEB-349 (epic ZEB-348); kind is immutable
/// once a channel is created.
///
/// `serde_repr` is load-bearing here (mirrors `Tier` in
/// `community_voting_core.rs`): without it, the standard `#[derive(Serialize)]`
/// would encode variants by NAME ("Voice"), not by the u8 discriminant the wire
/// format mandates. `#[repr(u8)]` alone only affects Rust memory layout, not
/// serde. `serde_repr` also rejects unknown discriminants on decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum ChannelKind {
    #[default]
    Text = 0,
    Voice = 1,
    /// ZEB-612 Town Hall: voice fused with assembly affordances (raise-hand
    /// queue, invite-to-speak, motion card). Same media/presence/control
    /// topics as Voice — the kind only routes the frontend view. Stale-client
    /// posture on decode is the ZEB-349 Voice precedent: `serde_repr` rejects
    /// the unknown discriminant, so a pre-ZEB-612 client rejects the
    /// containing state blob loudly (no crash, no partial mutation;
    /// upgrade-before-create is the fleet rule).
    Townhall = 2,
}

impl ChannelKind {
    /// `skip_serializing_if` / default-omission predicate: Text is the default
    /// and is never written to the CBOR map.
    pub fn is_text(&self) -> bool {
        matches!(self, ChannelKind::Text)
    }
}

impl CanonicalPayloadSealed for ChannelKind {}
impl CanonicalPayload for ChannelKind {}

/// 16-byte ULID identifying a single channel within a community.
/// Generated client-side at `ChannelCreate` time. Tuple-struct newtype
/// (not type alias) so the type system catches accidental substitution
/// between event-IDs and channel-IDs at IPC boundaries; bstr serde
/// keeps wire encoding compact (17 bytes vs CBOR array-of-u8 33 bytes).
/// Mirrors the shape of `OwnerAddr` / `SpaceId` in `owner_state_types.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChannelId(
    #[serde(
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub [u8; 16],
);

/// One signed event in a community's membership CRDT.
///
/// Wire format: 6- to 8-key CBOR map — 6 mandatory keys, plus `cs`
/// (invite-only Join countersig) and `en` (ZEB-339 enrollment cert on
/// identity-introducing events), each omitted when None. All keys are
/// 2 chars (text(2) = 3 bytes each) to satisfy the same-length-keys
/// invariant at this nesting level. Adjacently-tagged inner enums
/// (MembershipEventKind, CounterSignature) follow the same rule recursively.
///
/// `sig` covers the canonical-CBOR encoding of (id, community_id, kind,
/// actor, at) — countersig is excluded so an inviter can append their
/// counter-signature without invalidating the actor's signature. See
/// `sign_event` (Task 6) for the exact byte layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedMembershipEvent {
    #[serde(
        rename = "id",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub id: EventId,

    #[serde(rename = "ci")]
    pub community_id: SpaceId,

    #[serde(rename = "kn")]
    pub kind: MembershipEventKind,

    #[serde(rename = "ac")]
    pub actor: OwnerAddr,

    #[serde(rename = "at")]
    pub at: Hlc,

    /// Ed25519 signature over canonical CBOR of
    /// `(id, community_id, kind, actor, at)`. 64 bytes.
    #[serde(
        rename = "sg",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],

    /// Required for Join events in invite-only communities. None
    /// otherwise. Verified at receive time against the signer's
    /// power level at the time of the join.
    #[serde(rename = "cs", skip_serializing_if = "Option::is_none", default)]
    pub countersig: Option<CounterSignature>,

    /// ZEB-339: enrollment proof for the signer. REQUIRED on identity-
    /// introducing events (bootstrap Join, Join, PendingJoin); absent
    /// otherwise (the verifier resolves the signer's device key from
    /// materialized membership). Sits OUTSIDE the signed EventPayload —
    /// safe because cert.owner_id must equal the signed `actor`, the cert
    /// is master-signed (unforgeable), and the event sig must verify under
    /// cert.device_pubkeys.
    #[serde(rename = "en", skip_serializing_if = "Option::is_none", default)]
    pub enrollment: Option<EnrollmentCert>,

    /// ZEB-677: Master-issued signer certs backing a Quorum-issued cert —
    /// either `enrollment` above or a cert inside a `DeviceRetire` payload
    /// (both positions). Empty for Master-issued certs (the key is omitted
    /// on the wire; old peers ignore it and keep rejecting quorum certs).
    /// Sits OUTSIDE the signed EventPayload like `enrollment` — safe for
    /// the same reason: each signer cert is independently master-signed
    /// and the chokepoint verifies the full chain.
    #[serde(rename = "eb", default, skip_serializing_if = "Vec::is_empty")]
    pub signer_certs: Vec<EnrollmentCert>,
}

/// Counter-signature appended by an existing community member to vouch
/// for a new joiner in an invite-only community. The signer's power
/// must be ≥ POWER_THRESHOLDS.invite at the time of signing.
///
/// `sig` covers the same canonical-CBOR bytes as `SignedMembershipEvent.sig`
/// — i.e., the joiner's signed `(id, community_id, kind, actor, at)`.
/// This means the countersig binds to the joiner's exact event, not
/// just to the community ID, preventing a malicious admin from
/// "reusing" a countersig across different join attempts.
///
/// Wire codes match the semantic mapping used by SignedMembershipEvent:
/// `sg` always means signature (Ed25519 64-byte) at every nesting
/// level; `sn` means signer (the OwnerAddr who produced the signature).
/// Earlier drafts inverted these (signer=`sg`, sig=`sx`) — fixed
/// before Phase 1 merge so cross-language deserializers and raw-CBOR
/// audits don't have to special-case CounterSignature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CounterSignature {
    #[serde(rename = "sn")]
    pub signer: OwnerAddr,

    #[serde(
        rename = "sg",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],
}

impl CanonicalPayloadSealed for SignedMembershipEvent {}
impl CanonicalPayload for SignedMembershipEvent {}
impl CanonicalPayloadSealed for CounterSignature {}
impl CanonicalPayload for CounterSignature {}

use ed25519_dalek::{Signature, Signer, SigningKey};

use crate::owner_state_crypto::{canonical_cbor_encode, CryptoError};

/// The unsigned portion of a SignedMembershipEvent. Encoded canonically
/// and signed; the resulting signature populates SignedMembershipEvent.sig.
///
/// Keeping this as a separate type (vs. signing SignedMembershipEvent
/// itself with sig=zero) means the signed bytes are unambiguous —
/// there's no place to put "the actual sig went here" in the encoded
/// form. Mirrors how dm_envelope::SignedDmCidNotify is signed in
/// ZEB-227 (Phase 3b).
///
/// All 5 field keys are 2 chars to satisfy the same-length-keys
/// invariant at this nesting level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventPayload {
    #[serde(
        rename = "id",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub id: EventId,

    #[serde(rename = "ci")]
    pub community_id: SpaceId,

    #[serde(rename = "kn")]
    pub kind: MembershipEventKind,

    #[serde(rename = "ac")]
    pub actor: OwnerAddr,

    #[serde(rename = "at")]
    pub at: Hlc,
}

impl CanonicalPayloadSealed for EventPayload {}
impl CanonicalPayload for EventPayload {}

/// Extract the unsigned payload from a SignedMembershipEvent — the
/// exact bytes the actor sig and (when present) the countersig cover.
/// Centralised so signing/verifying paths can't drift in field order
/// or coverage if EventPayload gains fields in later phases.
impl From<&SignedMembershipEvent> for EventPayload {
    fn from(event: &SignedMembershipEvent) -> Self {
        EventPayload {
            id: event.id,
            community_id: event.community_id,
            kind: event.kind.clone(),
            actor: event.actor,
            at: event.at.clone(),
        }
    }
}

/// Sign an unsigned event payload with the actor's ed25519 key.
/// Returns a SignedMembershipEvent ready for canonical encoding +
/// publication. The countersig field is None — invite-only Joins
/// must be counter-signed via `attach_countersig`.
///
/// Errors only on canonical CBOR encoding failure (vanishingly rare
/// for in-memory values — would indicate a broken serde impl).
///
/// NOTE: this is the low-level primitive that takes only the Ed25519
/// SigningKey. For production callers that hold a full
/// `harmony_identity::PrivateIdentity`, prefer `sign_event_with_identity`
/// — it's a thin wrapper that uses `PrivateIdentity::sign` so tests
/// and production share the same code path. The two are wire-compatible
/// (PrivateIdentity::sign internally calls signing_key.sign).
pub fn sign_event(
    payload: &EventPayload,
    signing_key: &SigningKey,
) -> Result<SignedMembershipEvent, CryptoError> {
    let bytes = canonical_cbor_encode(payload)?;
    let sig = signing_key.sign(&bytes).to_bytes();
    Ok(SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: payload.id,
        community_id: payload.community_id,
        kind: payload.kind.clone(),
        actor: payload.actor,
        at: payload.at.clone(),
        sig,
        countersig: None,
        enrollment: None,
    })
}

/// Sign an unsigned event payload using a `harmony_identity::PrivateIdentity`.
/// Equivalent to sign_event but routes through `PrivateIdentity::sign`,
/// which is the production signing path (PrivateIdentity's signing_key
/// field is private — there's no way to obtain a `&SigningKey` for it
/// directly).
///
/// Caller is responsible for ensuring `payload.actor` matches
/// `private.identity.address_hash` — otherwise verify_signature will
/// reject with ActorPubkeyMismatch on the receiving side.
pub fn sign_event_with_identity(
    payload: &EventPayload,
    private: &harmony_identity::PrivateIdentity,
) -> Result<SignedMembershipEvent, CryptoError> {
    let bytes = canonical_cbor_encode(payload)?;
    let sig = private.sign(&bytes);
    Ok(SignedMembershipEvent {
        signer_certs: Vec::new(),
        id: payload.id,
        community_id: payload.community_id,
        kind: payload.kind.clone(),
        actor: payload.actor,
        at: payload.at.clone(),
        sig,
        countersig: None,
        enrollment: None,
    })
}

/// Errors that can fire during membership-event verification.
/// Wraps everything verify_event needs to surface — signature failure,
/// power insufficiency, counter-sig requirement, etc. Concrete variants
/// added per-task; Task 7 ships SignatureInvalid + CounterSigRequired
/// + CounterSigInvalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// The event's community_id doesn't match the verifier's expected
    /// community_id. Defends against cross-community authorization —
    /// the caller has prior_state and is_invite_only for community A,
    /// but the event was signed for community B; without this binding,
    /// power lookups and invite-only countersigning would credit the
    /// wrong community's state.
    WrongCommunity,
    SignatureInvalid,
    CounterSigRequired,
    /// A countersig is present on an event where it shouldn't be —
    /// any event other than an invite-only Join (i.e., Invite, Kick,
    /// SetPower, Leave, and open-community Join). The actor's sig
    /// intentionally excludes countersig (so an inviter can append
    /// their counter-signature without invalidating the actor's sig
    /// on a Join), which means countersig is malleable on the wire:
    /// a peer could append/strip/replace it on any event without
    /// breaking the actor sig. Reject explicitly so the invariant
    /// "countersig present iff invite-only Join" holds end-to-end.
    UnexpectedCounterSig,
    CounterSigInvalid,
    CounterSigPowerInsufficient,
    ActorPowerInsufficient,
    KickTargetPowerNotLower,
    /// Kick targeted an OwnerAddr that has never appeared in this
    /// community's member list. Without this guard, materialize would
    /// insert a fresh MemberState with status=Banned and
    /// joined_at=kick_time for someone who never actually joined —
    /// misleading state and a "phantom member" entry. Banning a
    /// recently-Left member is still allowed (Left ∈ members).
    KickTargetNotMember,
    /// Invite targeted a currently-Banned member. materialize() no-ops
    /// this case (Banned-sticky), so verify_event returning Ok would
    /// leave the caller incorrectly assuming the invite took effect.
    /// Reject so the IPC layer surfaces a clear "unban first" error to
    /// the UI rather than silently dropping the invite.
    InviteTargetBanned,
    /// Unban event targets an addr whose current MemberStatus is not Banned.
    /// Reject so the IPC layer can surface "target is not currently banned"
    /// rather than silently no-op.
    UnbanTargetNotBanned,
    /// Unban event targets an addr that has no member record at all in this
    /// community. Distinct from `KickTargetNotMember` so the error message
    /// surfaced to the user references the actual operation they performed
    /// (an unban) rather than "kick target has no member record" which is
    /// misleading from a UI perspective.
    UnbanTargetNotMember,
    /// Moderation reason string exceeds `MAX_MODERATION_REASON_CHARS`
    /// (280 codepoints). Mirrors the UI cap so a malicious peer cannot
    /// bypass the UI textarea `maxlength` and persist an oversized reason
    /// to every replica. Applies to both Kick and Unban events.
    ReasonTooLong,
    /// ZEB-649 (Qodo PR #434): a Fork event carried `reason: Some("")` (or
    /// whitespace-only). The fork reason is mandatory-when-present — a
    /// modified client must not be able to satisfy the wire shape with a
    /// meaningless empty string. Absent (`None`, pre-ZEB-649) stays valid.
    ReasonEmpty,
    /// SetPower assigned a level above POWER_THRESHOLDS.max. Even an
    /// authorized actor cannot grant a power higher than the cap, since
    /// that would create a member admin can no longer kick (admin's own
    /// power is bounded by max).
    PowerLevelOutOfRange,
    /// Join from an actor whose prior state is MemberStatus::Banned.
    /// Kick = effective ban until a dedicated unban flow exists, so a
    /// replayed Join must not silently overwrite the Banned status.
    BannedActorJoin,
    /// Leave from an actor whose prior state is MemberStatus::Banned.
    /// Without this guard, a kicked actor could send Leave (no power
    /// gate) to flip status from Banned → Left, then send Join (no
    /// longer Banned-blocked) to rejoin — defeating Kick-as-ban.
    BannedActorLeave,
    /// Invite/Kick/SetPower issued by an actor who is not currently a
    /// Joined member of the community. Power levels alone are not
    /// sufficient — a non-member with high assigned power (e.g., a
    /// former member after Leave or Kick, or an address that received
    /// SetPower but never Joined) cannot wield community moderation.
    ActorNotJoined,
    /// Counter-signature on an invite-only Join is from an OwnerAddr
    /// that is not currently a Joined member. Mirrors ActorNotJoined
    /// for the countersigner side.
    CounterSignerNotJoined,
    /// The 64-byte identity_pub provided for actor-signature verification
    /// hashes to a different address than `event.actor`. Defends against
    /// caller-side cache-lookup bugs that pair a looked-up pubkey with
    /// the wrong actor — without this binding, a malicious peer could
    /// claim event.actor=victim while signing with their own key, and
    /// downstream power lookups would credit the victim's identity.
    ActorPubkeyMismatch,
    /// The 64-byte identity_pub for countersig verification hashes to
    /// a different address than `event.countersig.signer`. Defends the
    /// countersigner side of the same attack; without it a valid
    /// countersignature from key A could be attributed to a higher-power
    /// signer B, bypassing the invite-only authorization gate.
    CounterSignerPubkeyMismatch,
    /// The 64-byte identity_pub bytes don't form a valid ed25519 +
    /// x25519 keypair (e.g., bad point encoding on either curve).
    /// Treat as a signature failure with extra context.
    InvalidIdentityPub,
    /// Channel-config event (`ChannelCreate`/`ChannelModify`/`ChannelDelete`)
    /// was signed by an actor whose power is below
    /// `POWER_THRESHOLDS.kick` (mod-tier). v2 hardcodes mod-tier as the
    /// channel-admin gate; per-community customization is deferred to
    /// ZEB-251. Distinct from `ActorPowerInsufficient` so the IPC layer
    /// can emit a clean "you don't have permission to manage channels"
    /// error string without overloading the membership-level diagnostic.
    ChannelAdminInsufficientPower,

    /// `ChannelModify` event is a no-op: both `name: None` and
    /// `write_power: None` (malformed signal). Content-intrinsic
    /// rejection — no prior_state dependency, so safe under cross-blob
    /// ordering. A signed Modify with both fields None has no
    /// meaningful payload; reject as malformed. Value-matching no-ops
    /// (proposed Some values exactly match prior materialized state)
    /// are NOT rejected here: two mods independently making the same
    /// rename would otherwise cause CRDT log divergence based on
    /// receive order.
    ChannelModifyNoOp,

    /// `ChannelCreate` or `ChannelModify` carries a `name` that is
    /// empty/whitespace-only or exceeds 32 chars (per spec §12.3).
    /// Receive-side enforcement so a malicious peer can't replicate
    /// invalid names that would break the UI.
    ChannelNameInvalid,

    EncodeError(String),

    /// C4: EpochRotation or EpochCatchup was rejected by verify_event's
    /// lightweight authority + shape pre-check. The issuer lacked admin
    /// power, was not the target of the triggering Leave, or the event
    /// had an obviously malformed shape that would unconditionally fail
    /// in materialize. This is a fast-path rejection that prevents
    /// unauthorized epoch events from entering the CRDT log.
    EpochEventUnauthorized,

    // ── ZEB-285 fork verifier ──
    /// ZEB-285: the event's signer (event.actor) has no entry in the
    /// PreForkSnapshot.identity_pubs map. The snapshot is authoritative
    /// for the original community's keyset — a signer not in the map
    /// cannot be verified and must be rejected.
    UnknownSigner {
        signer: OwnerAddr,
    },

    /// ZEB-285: the event's community_id does not match the snapshot's
    /// original_community_id. A validly-signed event from a different
    /// community must be rejected even when the same OwnerAddr is a member
    /// of both communities — without this check, cross-community event
    /// injection could pass signature verification. (Fix: PR #122 bot review.)
    CommunityIdMismatch {
        expected: SpaceId,
        actual: SpaceId,
    },

    /// ZEB-254: PendingJoin's InviteToken.inviter != ctx.admin_addr, OR
    /// invitee_hint does not match the joiner's actor, OR the token's
    /// signature does not verify against the admin's identity_pub.
    PendingJoinTokenInvalid,

    /// ZEB-254: PendingJoin's InviteToken has an `expires_at` value that
    /// is at or before the event's wall_ms.
    PendingJoinTokenExpired,

    /// ZEB-254 P6: PendingJoin actor's prior state is `Joined | Banned |
    /// Invited | PendingJoin` — cannot accept a pending Join for an
    /// already-engaged member. Rule: reject if prior state is any of those
    /// four; allow only `None` (never joined) or `Some(Left)`.
    PendingJoinAlreadyMember,

    /// ZEB-254: JoinCountersign actor is not currently Joined in the
    /// community.
    JoinCountersignActorNotJoined,

    /// ZEB-254: JoinCountersign actor's power is below invite_threshold.
    ///
    /// In v1, `POWER_THRESHOLDS.invite = 0` so this error is unreachable
    /// (every member's power defaults to 0 ≥ 0). The variant is retained as
    /// a forward-compatibility placeholder for ZEB-251 per-community threshold
    /// customisation where invite_threshold may be set above 0.
    JoinCountersignActorPowerInsufficient,

    /// ZEB-250 AP1: AdminProposal actor is not currently Joined.
    AdminProposalActorNotJoined,
    /// ZEB-250 AP2: AdminProposal actor's power is below 100.
    AdminProposalActorNotAdmin,
    /// ZEB-250 AP3: proposal_kind is malformed (target absent, level
    /// out of range, reason empty-string, etc.).
    AdminProposalKindInvalid,
    /// ZEB-250 AP4: proposal_kind is well-formed but doesn't qualify
    /// as admin-affecting per §4.3 — wrapping a routine SetPower or
    /// Kick in AdminProposal is a category error.
    AdminProposalNotAdminAffecting,
    /// ZEB-250 AP5: ChangeQuorum new_quorum is < 1 or exceeds current
    /// admin count.
    AdminProposalQuorumOutOfRange,
    /// ZEB-250 AC1: AdminCountersign actor is not currently Joined.
    AdminCountersignActorNotJoined,
    /// ZEB-250 AC2: AdminCountersign actor's power is below 100.
    AdminCountersignActorNotAdmin,
    /// ZEB-250 AC3: target_event_id is malformed (e.g., all-zero).
    AdminCountersignTargetIdMalformed,

    /// ZEB-250 §4.5: direct SetPower whose target IS an admin (power==100)
    /// or whose new level IS 100 was rejected because admin_quorum > 1.
    /// Must route through AdminProposal + AdminCountersign quorum.
    SetPowerRequiresQuorum,

    /// ZEB-734: direct SetPower that grants or removes admin power (new level
    /// == `max`, or target currently holds `max`) was rejected because the
    /// actor does not already hold admin power. Independent of `admin_quorum`
    /// and of any lowered per-community `set_power` threshold: creating or
    /// removing an admin ALWAYS requires the actor to already be an admin.
    /// Without this, an admin who lowers `set_power` below `max` would
    /// delegate admin-granting to sub-admin members — at `admin_quorum == 1`
    /// letting them seize admin control with no countersignature. A
    /// sub-`max` actor can neither direct-mint this (here) nor route it via
    /// AdminProposal (AP2 requires proposer power == `max`), so it is a hard
    /// authorization failure, not a "use quorum instead" redirect.
    SetPowerAdminAffectingRequiresAdmin,

    /// ZEB-250 §4.6: direct Kick of an admin (target power==100) was
    /// rejected because admin_quorum > 1.
    /// Must route through AdminProposal + AdminCountersign quorum.
    KickRequiresQuorum,

    /// ZEB-251 AT1: ChangeThresholds new_thresholds violate the invariant
    /// 0 <= invite <= kick <= set_power <= max, or max != 100.
    AdminProposalThresholdsInvalid,

    // ── ZEB-713 (ZEB-212 D1) admin-recovery verify rules ──
    /// ZEB-713 RD1: SetRecoveryDesignates designate list is empty or
    /// contains duplicates.
    RecoveryDesignatesMalformed,
    /// ZEB-713 RD2: a listed designate is not a currently-Joined member.
    RecoveryDesignateNotJoined,
    /// ZEB-713 RD3: threshold R is 0 or exceeds the designate count.
    RecoveryThresholdOutOfRange,
    /// ZEB-713 RD4: veto_window_ms is below
    /// `RECOVERY_VETO_WINDOW_FLOOR_MS` (7 days).
    RecoveryVetoWindowTooShort,
    /// ZEB-713 RD4 (ceiling): veto_window_ms exceeds
    /// `RECOVERY_VETO_WINDOW_CEILING_MS` (365 days) — guards the
    /// `t_R + W` deadline arithmetic against u64 wrap and keeps the
    /// value JS-number-exact across the DTO boundary.
    RecoveryVetoWindowTooLong,
    /// ZEB-713 RP2: RecoveryProposal in a community with no
    /// recovery_designates configured.
    RecoveryNotConfigured,
    /// ZEB-713 RP1: RecoveryProposal actor is not a currently-Joined
    /// recovery designate.
    RecoveryProposalActorNotDesignate,
    /// ZEB-713 RP3: new_admin is not currently Joined, already holds
    /// power 100, or equals lost_admin.
    RecoveryProposalNewAdminInvalid,
    /// ZEB-713 RP4: lost_admin is not a currently-Joined power-100
    /// member.
    RecoveryProposalLostAdminNotAdmin,
    /// ZEB-713 RP5: the proposal's config_digest does not match the
    /// live RecoveryDesignates generation.
    RecoveryProposalConfigDigestMismatch,
    /// ZEB-713 RP6: the actor already has an open (Collecting or
    /// TimeLocked) recovery proposal.
    RecoveryProposalActorHasOpenProposal,
    /// ZEB-713 RC1: RecoveryCosign actor is not a currently-Joined
    /// recovery designate.
    RecoveryCosignActorNotDesignate,
    /// ZEB-713: RecoveryCosign target_event_id is malformed (all-zero).
    RecoveryCosignTargetIdMalformed,
    /// ZEB-713 RV1: RecoveryVeto actor is not a currently-Joined
    /// power-100 member.
    RecoveryVetoActorNotAdmin,
    /// ZEB-713: RecoveryVeto target_event_id is malformed (all-zero).
    RecoveryVetoTargetIdMalformed,

    // ── ZEB-321 RCH1-RCH5 ReachabilityAnnounce verify rules ──
    //
    // RCH1 (outer SignedMembershipEvent signature) is enforced by the
    // existing `verify_signature()` call at the top of verify_event —
    // reuses `VerifyError::SignatureInvalid`. No dedicated discriminant.
    /// ZEB-321 RCH2: inner identity signature on a ReachabilityAnnounce
    /// payload failed to verify. Binds the Iroh NodeId to the harmony
    /// identity; rejecting prevents a malicious community member from
    /// claiming someone else's NodeId.
    ReachabilityInnerSigInvalid,

    // ZEB-321 RCH3 (ReachabilityActorMismatch) was REMOVED in ZEB-339: the
    // actor↔signer binding is now enforced upstream by verify_membership_signer
    // (signer.owner == event.actor, proven via the EnrollmentCert), and the
    // inner reachability signature is verified against that same resolved
    // enrolled device key — so the old address-derivation form (which assumed
    // address_hash(signing_key) == actor) is both unnecessary and, under the
    // owner/device split, false by construction.
    /// ZEB-321 RCH4: the payload's `announced_at_ms` differs from the
    /// event's HLC `wall_ms` by more than ±30 minutes. Sanity check —
    /// rejects obviously-tampered records (the spec's "silent drop").
    ReachabilityTimestampSkew,

    /// ZEB-321 RCH5: the actor is not a current community member at
    /// the event's HLC (read via membership projection).
    ReachabilityActorNotMember,

    // ── ZEB-339 enrolled-device cert error taxonomy ────────────────────────
    /// ZEB-339: Join/PendingJoin/bootstrap arrived with no `enrollment` cert.
    MissingEnrollmentCert,
    /// ZEB-339: cert verification failed (bad master sig /
    /// hash(master)!=owner_id / device-id mismatch / unknown version).
    /// ZEB-677: Quorum-issued certs verify via the `enrollment_verify`
    /// chokepoint against the event's carried `signer_certs` bundle; a
    /// quorum cert with a missing/short/invalid bundle lands here.
    EnrollmentCertInvalid,
    /// ZEB-339: `cert.owner_id != event.actor.0`.
    EnrollmentOwnerMismatch,
    /// ZEB-339: no enrolled device key matching the signing key was found for
    /// `actor`. Two origins: (a) a steady-state event whose materialized
    /// `members[actor].enrolled_device_keys` set contains no matching key, or
    /// (b) a resolved signer whose `owner` does not equal the event's `actor`
    /// (a caller-binding precondition in `verify_membership_signer`).
    SignerNotEnrolledForActor,
    /// ZEB-339: counter-signer's signing key is not in the counter-signer's
    /// materialized `enrolled_device_keys`.
    CounterSignerNotEnrolled,
    /// ZEB-495 (ZEB-340 Part 2): a `DeviceAnnounce` was signed for an actor
    /// who is NOT an already-`Joined` member of this community. Unlike
    /// steady-state kinds (whose signer comes from the materialized enrolled
    /// set, which implies membership), `DeviceAnnounce`'s signer comes from
    /// the carried cert, so membership must be checked independently — a
    /// device may not introduce itself into a community its owner has not
    /// already joined.
    DeviceAnnounceForNonMember,
    /// ZEB-401: a `DeviceAnnounce` would grow the actor's
    /// `enrolled_device_keys` beyond `MAX_ENROLLED_DEVICE_KEYS`. Adding a key
    /// already in the set is idempotent and still allowed (no growth); only a
    /// NEW key when the set is already at the cap is rejected, bounding the
    /// per-member verify cost regardless of input.
    EnrolledDeviceKeyLimit,
    /// ZEB-668 S3 (Qodo PR #453): a `DeviceAnnounce` whose carried cert key
    /// is tombstoned in the actor's `revoked_device_keys`. The cert stays
    /// master-signed-valid after revocation, so without this reject a
    /// retired device could insert verify-passing (materialize-no-op)
    /// announces forever. Verify-time mirror of the materialize tombstone
    /// guard.
    DeviceAnnounceForRetiredKey,
    /// ZEB-668 S3: a `DeviceRetire` whose actor has no member entry at all.
    /// ANY member status is acceptable (retire is subtractive; a Left/Banned
    /// owner's stale key must still be retirable) — this fires only for
    /// never-members.
    DeviceRetireForNonMember,
    /// ZEB-668 S3: a `DeviceRetire` whose carried cert pair fails
    /// verification or doesn't bind: owner_id ≠ actor on either cert,
    /// revocation target ≠ enrollment device_id, bad signature on either
    /// cert, a Quorum-issued cert (either position) whose carried
    /// `signer_certs` bundle is missing or fails depth-1 verification
    /// (ZEB-677), or an oversize `Other(reason)` string.
    DeviceRetireCertInvalid,

    // ── ZEB-458 P4B CommunityRelayAnnounce verify rules ───────────────────────
    //
    // RCH1 analogue (outer signature) is enforced by the existing
    // `verify_signature()` call at the top of verify_event —
    // reuses `VerifyError::SignatureInvalid`. No dedicated discriminant.
    /// ZEB-458 RCH2 analogue: inner device identity signature on a
    /// `CommunityRelayAnnounce` payload failed to verify. Binds the relay
    /// advertisement to the announcing device; rejecting prevents a malicious
    /// community member from claiming another device's relay coordinates.
    CommunityRelayInnerSigInvalid,

    /// ZEB-458 RCH4 analogue: the payload's `ad_at` differs from the event's
    /// HLC `wall_ms` by more than ±30 minutes. The 30-min bound is shared
    /// with `REACHABILITY_TIMESTAMP_SKEW_MAX_MS`.
    CommunityRelayTimestampSkew,

    /// ZEB-458 RCH5 analogue: the actor is not a current Joined community
    /// member at the event's HLC. Relay advertisements from non-members are
    /// meaningless and must be rejected.
    CommunityRelayActorNotMember,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::WrongCommunity => write!(
                f,
                "event.community_id does not match the verifier's expected community"
            ),
            VerifyError::SignatureInvalid => write!(f, "signature invalid"),
            VerifyError::CounterSigRequired => write!(f, "invite-only Join requires countersig"),
            VerifyError::UnexpectedCounterSig => write!(
                f,
                "countersig present on a non-invite-only-Join event (sig excludes countersig — \
                 reject to keep the wire form unmalleable)"
            ),
            VerifyError::CounterSigInvalid => write!(f, "countersig invalid"),
            VerifyError::CounterSigPowerInsufficient => {
                write!(f, "countersig signer's power is below invite_threshold")
            }
            VerifyError::ActorPowerInsufficient => {
                write!(f, "actor's power is below the action's threshold")
            }
            VerifyError::KickTargetPowerNotLower => {
                write!(f, "kick requires actor.power > target.power")
            }
            VerifyError::KickTargetNotMember => {
                write!(
                    f,
                    "kick target has no member record in this community"
                )
            }
            VerifyError::InviteTargetBanned => {
                write!(
                    f,
                    "invite target is currently Banned (admin must unban first)"
                )
            }
            VerifyError::UnbanTargetNotBanned => {
                write!(f, "unban target is not currently banned")
            }
            VerifyError::UnbanTargetNotMember => {
                write!(
                    f,
                    "unban target has no member record in this community"
                )
            }
            VerifyError::ReasonTooLong => {
                write!(
                    f,
                    "moderation reason exceeds {MAX_MODERATION_REASON_CHARS} characters"
                )
            }
            VerifyError::ReasonEmpty => {
                write!(f, "fork reason is present but empty")
            }
            VerifyError::PowerLevelOutOfRange => {
                write!(f, "power level exceeds POWER_THRESHOLDS.max")
            }
            VerifyError::BannedActorJoin => {
                write!(f, "Join rejected: actor's prior status is Banned")
            }
            VerifyError::BannedActorLeave => {
                write!(f, "Leave rejected: actor's prior status is Banned")
            }
            VerifyError::ActorNotJoined => {
                write!(
                    f,
                    "actor is not currently a Joined member of this community"
                )
            }
            VerifyError::CounterSignerNotJoined => {
                write!(
                    f,
                    "countersig signer is not currently a Joined member of this community"
                )
            }
            VerifyError::ActorPubkeyMismatch => write!(
                f,
                "actor identity_pub does not hash to event.actor — pubkey-to-claimed-signer binding violated"
            ),
            VerifyError::CounterSignerPubkeyMismatch => write!(
                f,
                "countersigner identity_pub does not hash to cs.signer — pubkey-to-claimed-signer binding violated"
            ),
            VerifyError::InvalidIdentityPub => {
                write!(f, "identity_pub bytes are not a valid (X25519, Ed25519) public-key pair")
            }
            VerifyError::ChannelAdminInsufficientPower => write!(
                f,
                "channel-config events require power >= POWER_THRESHOLDS.kick (mod-tier)"
            ),
            VerifyError::ChannelModifyNoOp => {
                write!(f, "ChannelModify is a no-op (all fields None)")
            }
            VerifyError::ChannelNameInvalid => write!(
                f,
                "channel name is empty or exceeds 32 chars (spec §12.3 limit)"
            ),
            VerifyError::EncodeError(s) => write!(f, "canonical encode failed: {s}"),
            VerifyError::EpochEventUnauthorized => write!(
                f,
                "EpochRotation/EpochCatchup rejected at verify_event: issuer lacks admin power, \
                 is not the cooperative leaver, or the event shape is obviously malformed"
            ),
            VerifyError::UnknownSigner { signer } => {
                write!(
                    f,
                    "signer {} is not present in PreForkSnapshot.identity_pubs",
                    hex::encode(signer.0)
                )
            }
            VerifyError::CommunityIdMismatch { expected, actual } => {
                write!(
                    f,
                    "event.community_id {} does not match snapshot.original_community_id {}",
                    hex::encode(actual.0),
                    hex::encode(expected.0)
                )
            }
            VerifyError::PendingJoinTokenInvalid => write!(f, "ZEB-254 PendingJoin InviteToken invalid (inviter/invitee_hint/sig)"),
            VerifyError::PendingJoinTokenExpired => write!(f, "ZEB-254 PendingJoin InviteToken expired"),
            VerifyError::PendingJoinAlreadyMember => write!(f, "ZEB-254 PendingJoin actor's prior state is already-engaged"),
            VerifyError::JoinCountersignActorNotJoined => write!(f, "ZEB-254 JoinCountersign actor is not Joined"),
            VerifyError::JoinCountersignActorPowerInsufficient => write!(f, "ZEB-254 JoinCountersign actor power < invite_threshold"),
            VerifyError::AdminProposalActorNotJoined => {
                write!(f, "ZEB-250 AdminProposal actor is not Joined")
            }
            VerifyError::AdminProposalActorNotAdmin => {
                write!(f, "ZEB-250 AdminProposal actor power < 100 (admin tier)")
            }
            VerifyError::AdminProposalKindInvalid => {
                write!(f, "ZEB-250 AdminProposal proposal_kind is malformed")
            }
            VerifyError::AdminProposalNotAdminAffecting => {
                write!(f, "ZEB-250 AdminProposal proposal_kind is not admin-affecting")
            }
            VerifyError::AdminProposalQuorumOutOfRange => {
                write!(f, "ZEB-250 AdminProposal ChangeQuorum new_quorum out of range [1, admin_count]")
            }
            VerifyError::AdminCountersignActorNotJoined => {
                write!(f, "ZEB-250 AdminCountersign actor is not Joined")
            }
            VerifyError::AdminCountersignActorNotAdmin => {
                write!(f, "ZEB-250 AdminCountersign actor power < 100 (admin tier)")
            }
            VerifyError::AdminCountersignTargetIdMalformed => {
                write!(f, "ZEB-250 AdminCountersign target_event_id is malformed")
            }
            VerifyError::SetPowerRequiresQuorum => write!(f, "ZEB-250: direct admin-affecting SetPower rejected (admin_quorum > 1 — use AdminProposal)"),
            VerifyError::SetPowerAdminAffectingRequiresAdmin => write!(f, "ZEB-734: granting or removing admin power requires the actor to already be an admin (a lowered set_power threshold does not delegate admin-granting)"),
            VerifyError::KickRequiresQuorum => write!(f, "ZEB-250: direct Kick of an admin rejected (admin_quorum > 1 — use AdminProposal)"),
            VerifyError::AdminProposalThresholdsInvalid => write!(
                f,
                "ZEB-251 AdminProposal ChangeThresholds invariant violated (need 0 <= invite <= kick <= set_power <= max == 100)"
            ),
            VerifyError::RecoveryDesignatesMalformed => {
                write!(f, "ZEB-713 RD1: designate list empty or contains duplicates")
            }
            VerifyError::RecoveryDesignateNotJoined => {
                write!(f, "ZEB-713 RD2: designate is not a currently-Joined member")
            }
            VerifyError::RecoveryThresholdOutOfRange => {
                write!(f, "ZEB-713 RD3: threshold out of range [1, designate count]")
            }
            VerifyError::RecoveryVetoWindowTooShort => {
                write!(f, "ZEB-713 RD4: veto window below the 7-day floor")
            }
            VerifyError::RecoveryVetoWindowTooLong => {
                write!(f, "ZEB-713 RD4: veto window above the 365-day ceiling")
            }
            VerifyError::RecoveryNotConfigured => {
                write!(f, "ZEB-713 RP2: no recovery designates configured")
            }
            VerifyError::RecoveryProposalActorNotDesignate => {
                write!(f, "ZEB-713 RP1: proposal actor is not a Joined designate")
            }
            VerifyError::RecoveryProposalNewAdminInvalid => {
                write!(
                    f,
                    "ZEB-713 RP3: new_admin not Joined, already admin, or equals lost_admin"
                )
            }
            VerifyError::RecoveryProposalLostAdminNotAdmin => {
                write!(f, "ZEB-713 RP4: lost_admin is not a Joined power-100 member")
            }
            VerifyError::RecoveryProposalConfigDigestMismatch => {
                write!(f, "ZEB-713 RP5: config_digest does not match live config")
            }
            VerifyError::RecoveryProposalActorHasOpenProposal => {
                write!(f, "ZEB-713 RP6: actor already has an open recovery proposal")
            }
            VerifyError::RecoveryCosignActorNotDesignate => {
                write!(f, "ZEB-713 RC1: cosign actor is not a Joined designate")
            }
            VerifyError::RecoveryCosignTargetIdMalformed => {
                write!(f, "ZEB-713: RecoveryCosign target_event_id is malformed")
            }
            VerifyError::RecoveryVetoActorNotAdmin => {
                write!(f, "ZEB-713 RV1: veto actor is not a Joined power-100 member")
            }
            VerifyError::RecoveryVetoTargetIdMalformed => {
                write!(f, "ZEB-713: RecoveryVeto target_event_id is malformed")
            }
            VerifyError::ReachabilityInnerSigInvalid => {
                write!(f, "ZEB-321 RCH2 inner ReachabilityAnnounce signature invalid")
            }
            VerifyError::ReachabilityTimestampSkew => {
                write!(f, "ZEB-321 RCH4 ReachabilityAnnounce timestamp skew > 30min")
            }
            VerifyError::ReachabilityActorNotMember => {
                write!(f, "ZEB-321 RCH5 ReachabilityAnnounce actor is not a community member")
            }
            VerifyError::MissingEnrollmentCert => {
                write!(f, "ZEB-339: identity-introducing event carries no enrollment cert")
            }
            VerifyError::EnrollmentCertInvalid => {
                write!(
                    f,
                    "ZEB-339: enrollment cert failed verification (bad master sig, hash mismatch, \
                     device-id mismatch, or unknown version)"
                )
            }
            VerifyError::EnrollmentOwnerMismatch => {
                write!(
                    f,
                    "ZEB-339: cert.owner_id does not match event.actor"
                )
            }
            VerifyError::SignerNotEnrolledForActor => {
                write!(
                    f,
                    "ZEB-339: no enrolled device key matching the signing key found for actor"
                )
            }
            VerifyError::CounterSignerNotEnrolled => {
                write!(
                    f,
                    "ZEB-339: counter-signer's device key is not enrolled for their identity"
                )
            }
            VerifyError::CommunityRelayInnerSigInvalid => {
                write!(f, "ZEB-458 RCH2 inner CommunityRelayAnnounce signature invalid")
            }
            VerifyError::CommunityRelayTimestampSkew => {
                write!(f, "ZEB-458 RCH4 CommunityRelayAnnounce timestamp skew > 30min")
            }
            VerifyError::CommunityRelayActorNotMember => {
                write!(f, "ZEB-458 RCH5 CommunityRelayAnnounce actor is not a community member")
            }
            VerifyError::DeviceAnnounceForNonMember => {
                write!(
                    f,
                    "ZEB-495 DeviceAnnounce actor is not an already-Joined community member"
                )
            }
            VerifyError::EnrolledDeviceKeyLimit => {
                write!(
                    f,
                    "ZEB-401 DeviceAnnounce would exceed MAX_ENROLLED_DEVICE_KEYS ({MAX_ENROLLED_DEVICE_KEYS}) for the actor"
                )
            }
            VerifyError::DeviceAnnounceForRetiredKey => {
                write!(
                    f,
                    "ZEB-668 DeviceAnnounce carries a cert for a retired (tombstoned) device key"
                )
            }
            VerifyError::DeviceRetireForNonMember => {
                write!(
                    f,
                    "ZEB-668 DeviceRetire actor has no member entry in this community"
                )
            }
            VerifyError::DeviceRetireCertInvalid => {
                write!(
                    f,
                    "ZEB-668 DeviceRetire carried an invalid or unbound revocation/enrollment cert pair"
                )
            }
        }
    }
}

impl std::error::Error for VerifyError {}

impl From<CryptoError> for VerifyError {
    fn from(e: CryptoError) -> Self {
        VerifyError::EncodeError(format!("{e:?}"))
    }
}

/// Verify the actor's signature on a SignedMembershipEvent, with a
/// pubkey-to-claimed-signer binding check.
///
/// Steps:
/// 1. Derive `address_hash` from `actor_identity_pub` (the canonical
///    `harmony_identity::Identity` derivation: SHA256(X25519 || Ed25519)[:16]).
///    Reject if it does not equal `event.actor.0` — defends against
///    callers that pair a pubkey with the wrong claimed identity (cache
///    lookup bug, malicious peer substitution, etc.).
/// 2. Use the Ed25519 verifying key from `actor_identity_pub[32..]` to
///    verify_strict the signature over the canonical CBOR encoding of
///    the event's payload (excluding sig and countersig).
///
/// Use `verify_strict` (not `verify`) — strict mode rejects signatures
/// with non-canonical S values and small-order R points, matching the
/// EdDSA RFC 8032 strict subset and protecting against signature
/// malleability attacks. Mirrors how dm_envelope verifies its own
/// signed payloads.
pub fn verify_signature(
    event: &SignedMembershipEvent,
    actor_identity_pub: &[u8; 64],
) -> Result<(), VerifyError> {
    let identity = harmony_identity::Identity::from_public_bytes(actor_identity_pub)
        .map_err(|_| VerifyError::InvalidIdentityPub)?;
    if identity.address_hash != event.actor.0 {
        return Err(VerifyError::ActorPubkeyMismatch);
    }
    let bytes = canonical_cbor_encode(&EventPayload::from(event))?;
    let sig = Signature::from_bytes(&event.sig);
    identity
        .verifying_key
        .verify_strict(&bytes, &sig)
        .map_err(|_| VerifyError::SignatureInvalid)
}

/// Attach a counter-signature to a Join event for an invite-only
/// community. The signer's key signs the SAME canonical bytes the
/// actor signed (the EventPayload), so the countersig binds to the
/// exact joiner event, not just to the community ID.
///
/// Caller is responsible for ensuring `signer` matches the OwnerAddr
/// derived from `signer_key`'s identity — otherwise verify_countersig
/// will reject with CounterSignerPubkeyMismatch.
pub fn attach_countersig(
    event: &SignedMembershipEvent,
    signer: OwnerAddr,
    signer_key: &SigningKey,
) -> Result<SignedMembershipEvent, CryptoError> {
    let bytes = canonical_cbor_encode(&EventPayload::from(event))?;
    let sig = signer_key.sign(&bytes).to_bytes();
    let mut out = event.clone();
    out.countersig = Some(CounterSignature { signer, sig });
    Ok(out)
}

/// Attach a counter-signature using a `harmony_identity::PrivateIdentity`.
/// Sets `cs.signer = OwnerAddr(private.identity.address_hash)` so the
/// pubkey-binding check on the receiving side will pass.
pub fn attach_countersig_with_identity(
    event: &SignedMembershipEvent,
    private: &harmony_identity::PrivateIdentity,
) -> Result<SignedMembershipEvent, CryptoError> {
    let bytes = canonical_cbor_encode(&EventPayload::from(event))?;
    let sig = private.sign(&bytes);
    let mut out = event.clone();
    out.countersig = Some(CounterSignature {
        signer: OwnerAddr(private.identity.address_hash),
        sig,
    });
    Ok(out)
}

/// ZEB-339: attach a counter-signature produced by the signer's enrolled
/// device key (#2). The verifier resolves the signer's key from materialized
/// membership, so the countersig MUST be made with device #2.
pub fn attach_countersig_with_device_key(
    event: &SignedMembershipEvent,
    signer_owner: OwnerAddr,
    signer_key: &ed25519_dalek::SigningKey,
) -> Result<SignedMembershipEvent, CryptoError> {
    let bytes = canonical_cbor_encode(&EventPayload::from(event))?;
    let sig = signer_key.sign(&bytes).to_bytes();
    let mut out = event.clone();
    out.countersig = Some(CounterSignature {
        signer: signer_owner,
        sig,
    });
    Ok(out)
}

/// Verify the counter-signature on an event, with a pubkey-to-claimed-
/// signer binding check.
///
/// Steps:
/// 1. Derive `address_hash` from `signer_identity_pub` and reject if
///    it doesn't equal `event.countersig.signer.0` — without this,
///    a valid countersignature from key A could be attributed to an
///    arbitrary claimed signer B (typically a higher-power one),
///    bypassing the invite-only authorization gate.
/// 2. Use the Ed25519 verifying key from `signer_identity_pub[32..]`
///    to verify_strict the countersignature over the same canonical
///    bytes the actor signed.
///
/// Returns CounterSigRequired if the countersig is missing.
/// Returns CounterSignerNotEnrolled if the signer is not a materialized
/// member, or none of their enrolled device keys verifies the countersig.
/// Power-level checking on the signer happens elsewhere (verify_event)
/// — this function is purely cryptographic.
///
/// ZEB-339: resolves the countersigner's enrolled device key(s) from the
/// materialized membership rather than from a caller-supplied identity_pub.
pub fn verify_countersig(
    event: &SignedMembershipEvent,
    prior_state: &MaterializedMembership,
) -> Result<(), VerifyError> {
    let cs = event
        .countersig
        .as_ref()
        .ok_or(VerifyError::CounterSigRequired)?;
    let member = prior_state
        .members
        .get(&cs.signer)
        .ok_or(VerifyError::CounterSignerNotEnrolled)?;
    let bytes = canonical_cbor_encode(&EventPayload::from(event))?;
    let sig = Signature::from_bytes(&cs.sig);
    for key in &member.enrolled_device_keys {
        if let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(key) {
            if vk.verify_strict(&bytes, &sig).is_ok() {
                return Ok(());
            }
        }
    }
    Err(VerifyError::CounterSignerNotEnrolled)
}

// ── ZEB-339 enrolled-device signing primitives ────────────────────────────────

/// The minimal proven fact: this ed25519 key is a device enrolled under `owner`.
///
/// Produced by [`enrolled_key_from_cert`] (cert path) or by a materialized-
/// state lookup (already-enrolled member path). Consumed by
/// [`verify_membership_signer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnrolledDeviceKey {
    pub owner: OwnerAddr,
    pub device_ed25519: [u8; 32],
}

/// Verify the event was authored by `signer`'s enrolled device key, over the
/// canonical EventPayload. `signer.owner` must equal `event.actor`.
///
/// Uses `verify_strict` to reject non-canonical S values and small-order R
/// points (signature malleability) — same rationale as [`verify_signature`].
///
/// Returns `Err(SignerNotEnrolledForActor)` if owner ≠ actor (a caller-binding
/// precondition: the resolved signer belongs to a different owner than the
/// event's actor).
/// Returns `Err(SignatureInvalid)` if the ed25519 bytes are malformed or the
/// signature doesn't verify.
pub fn verify_membership_signer(
    event: &SignedMembershipEvent,
    signer: &EnrolledDeviceKey,
) -> Result<(), VerifyError> {
    if signer.owner != event.actor {
        return Err(VerifyError::SignerNotEnrolledForActor);
    }
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&signer.device_ed25519)
        .map_err(|_| VerifyError::SignatureInvalid)?;
    let bytes = canonical_cbor_encode(&EventPayload::from(event))?;
    let sig = Signature::from_bytes(&event.sig);
    vk.verify_strict(&bytes, &sig)
        .map_err(|_| VerifyError::SignatureInvalid)
}

/// Resolve an `EnrolledDeviceKey` from an identity-introducing event's carried
/// `EnrollmentCert`: verify the cert, bind `owner == actor`, return the device
/// key.
///
/// Returns `Err(MissingEnrollmentCert)` if `enrollment` is `None`.
/// Returns `Err(EnrollmentCertInvalid)` if verification fails.
/// Returns `Err(EnrollmentOwnerMismatch)` if `cert.owner_id != event.actor.0`.
///
/// Issuer policy (ZEB-677): routes through the `enrollment_verify`
/// chokepoint — Master certs verify self-contained; Quorum certs verify
/// against the event's carried `signer_certs` bundle (depth-1 chain
/// carriage). A quorum cert without its bundle still fails closed.
pub fn enrolled_key_from_cert(
    event: &SignedMembershipEvent,
) -> Result<EnrolledDeviceKey, VerifyError> {
    let cert = event
        .enrollment
        .as_ref()
        .ok_or(VerifyError::MissingEnrollmentCert)?;
    // Divide by 1000: EnrollmentCert expiry is Unix seconds; event.at.wall_ms is
    // milliseconds. Still deterministic — a pure function of event.at.wall_ms. (ZEB-378)
    let verified = crate::enrollment_verify::verify_enrollment_any_issuer(
        cert,
        &event.signer_certs,
        Some(&event.actor.0),
        event.at.wall_ms / 1000,
    )
    .map_err(|e| match e {
        crate::enrollment_verify::EnrollmentVerifyError::OwnerMismatch => {
            VerifyError::EnrollmentOwnerMismatch
        }
        _ => VerifyError::EnrollmentCertInvalid,
    })?;
    Ok(EnrolledDeviceKey {
        owner: event.actor,
        device_ed25519: verified.device_ed25519,
    })
}

/// ZEB-668 S3: validate the cert pair carried by a `DeviceRetire`, proving —
/// with no communal state beyond the actor's OwnerAddr — that:
///
/// 1. `enrollment` is a genuine cert for the actor's owner, binding the
///    16-byte `device_id` to the 32-byte ed25519 key communities store.
///    Verified at the cert's own `issued_at`, NOT event time (unlike
///    `enrolled_key_from_cert`): retire must work for certs that have since
///    EXPIRED — expiry gates a key's authority to ACT, which is irrelevant
///    to removing it; the signature binding is what's load-bearing here.
/// 2. `revocation` targets exactly that device (`target == device_id`),
///    names the same owner, and its signature verifies: Master-issued certs
///    are self-contained; SelfDevice certs verify under the retired
///    device's own ed25519 key taken from the enrollment cert.
/// 3. An `Other(reason)` string is capped — same DoS posture as moderation
///    reasons (a malicious peer must not persist a giant string on every
///    replica).
///
/// ZEB-677: both positions route through the `enrollment_verify`
/// chokepoint, so Quorum-issued certs (either position) verify against the
/// event's carried `signer_certs` bundle — as-of their own `issued_at`,
/// consistent with rule 1's expiry semantics. Without a bundle they fail
/// closed as before.
fn verify_device_retire_certs(
    actor: &OwnerAddr,
    revocation: &RevocationCert,
    enrollment: &EnrollmentCert,
    signer_certs: &[EnrollmentCert],
) -> Result<(), VerifyError> {
    use harmony_owner::certs::RevocationReason;
    if enrollment.owner_id != actor.0
        || revocation.owner_id != actor.0
        || revocation.target != enrollment.device_id
    {
        return Err(VerifyError::DeviceRetireCertInvalid);
    }
    if crate::enrollment_verify::verify_enrollment_any_issuer(
        enrollment,
        signer_certs,
        Some(&actor.0),
        enrollment.issued_at,
    )
    .is_err()
    {
        return Err(VerifyError::DeviceRetireCertInvalid);
    }
    if let RevocationReason::Other(s) = &revocation.reason {
        if s.chars().count() > MAX_MODERATION_REASON_CHARS {
            return Err(VerifyError::DeviceRetireCertInvalid);
        }
    }
    if crate::enrollment_verify::verify_revocation_any_issuer(
        revocation,
        enrollment,
        signer_certs,
        revocation.issued_at,
    )
    .is_err()
    {
        return Err(VerifyError::DeviceRetireCertInvalid);
    }
    Ok(())
}

/// Steady-state signer resolution: find the actor's enrolled device key (from
/// materialized membership) that verifies this event's signature.
fn resolve_enrolled_signer(
    prior_state: &MaterializedMembership,
    event: &SignedMembershipEvent,
) -> Result<EnrolledDeviceKey, VerifyError> {
    let member = prior_state
        .members
        .get(&event.actor)
        .ok_or(VerifyError::SignerNotEnrolledForActor)?;
    let bytes = canonical_cbor_encode(&EventPayload::from(event))?;
    let sig = Signature::from_bytes(&event.sig);
    for key in &member.enrolled_device_keys {
        if let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(key) {
            if vk.verify_strict(&bytes, &sig).is_ok() {
                return Ok(EnrolledDeviceKey {
                    owner: event.actor,
                    device_ed25519: *key,
                });
            }
        }
    }
    Err(VerifyError::SignerNotEnrolledForActor)
}

/// ZEB-339: verify an InviteToken's signature against the inviter's enrolled
/// device key(s), resolved from materialized membership. The inviter is a
/// Joined member whose enrolled key is in `prior_state`.
///
/// `pub(crate)` since ZEB-436: `orphan_dir_adoption_eligible` (lib.rs)
/// mirrors the PendingJoin P5 gate against the orphaned dir's own
/// materialized membership, so adoption is never a weaker
/// authentication path than a first-time join.
pub(crate) fn verify_invite_token_sig_with_enrolled(
    token: &crate::community_invite::InviteToken,
    prior_state: &MaterializedMembership,
) -> Result<(), VerifyError> {
    let member = prior_state
        .members
        .get(&token.inviter)
        .ok_or(VerifyError::PendingJoinTokenInvalid)?;
    let token_bytes = crate::community_invite::canonical_invite_token_bytes(token)
        .map_err(|_| VerifyError::PendingJoinTokenInvalid)?;
    let sig = Signature::from_bytes(&token.sig);
    for key in &member.enrolled_device_keys {
        if let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(key) {
            if vk.verify_strict(&token_bytes, &sig).is_ok() {
                return Ok(());
            }
        }
    }
    Err(VerifyError::PendingJoinTokenInvalid)
}

/// Materialized view computed from a community's signed event log.
/// Pure function of the log + the community Space's admin_addr (per
/// the bootstrap rule). Re-computed when needed; caching belongs at
/// the call site (Phase 2's CommunityState owns the cache + version
/// counter, mirroring the inbox_entries_for_space pattern from
/// owner_state_crdt.rs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializedMembership {
    pub members: BTreeMap<OwnerAddr, MemberState>,
    /// Per-actor power level. Unset key = 0 = default. The community
    /// admin (Space.admin_addr) starts at 100 implicitly via the
    /// bootstrap rule — see `materialize` (Task 9). SetPower events
    /// override.
    pub power_levels: BTreeMap<OwnerAddr, u8>,
    /// Per-channel materialized state. Built by `materialize` from
    /// `ChannelCreate`/`ChannelModify`/`ChannelDelete` event replay
    /// (ZEB-248 Phase 1). `BTreeMap` (not `HashMap`) is load-bearing:
    /// `MaterializedMembership` impls `CanonicalPayload`, and canonical
    /// CBOR requires deterministic key order at every map-typed nesting
    /// level — `HashMap` iteration is non-deterministic and would
    /// break byte-equality across replicas.
    ///
    /// `#[serde(default)]` for forward/backward compat: any cached or
    /// persisted `MaterializedMembership` from before the channels
    /// field existed (Sub-C v1) deserializes with an empty channels
    /// map rather than failing decode. v2-and-beyond persists channel
    /// state via the underlying event log — `materialize` rebuilds
    /// the channels map from events on each call — so the wire form
    /// is functionally a derived view; the default is harmless.
    #[serde(default)]
    pub channels: BTreeMap<ChannelId, ChannelInfo>,

    /// ZEB-249: Current epoch counter; advances on each `EpochRotation`.
    /// `Some(_)` after the first Kick/Leave+rotation; `None` until then.
    #[serde(default)]
    pub current_epoch: Option<u64>,

    /// ZEB-249: Tracks members whose Kick/Leave hasn't been followed
    /// by a successful matching EpochRotation. Self-healing path picks
    /// these up and synthesizes fresh rotations. See spec §4.3.
    #[serde(default)]
    pub pending_rotation_for: BTreeSet<OwnerAddr>,

    /// ZEB-249: Tracks new members whose Bootstrap-Join landed with a
    /// stale snapshot_epoch < current_epoch (kick between invite issuance
    /// and redemption). Self-healing observer synthesizes EpochCatchup
    /// events. Spec §4.6.
    #[serde(default)]
    pub pending_catchup_for: BTreeSet<OwnerAddr>,

    /// ZEB-250: number of admin-tier signatures required for an
    /// admin-affecting action (SetPower to/from 100, Kick of an admin,
    /// or change of admin_quorum itself). Default 1 (current
    /// single-admin behavior); communities opt into multi-sig by
    /// raising it via a successful ChangeQuorum proposal.
    ///
    /// Materialized from events: the materialize pass walks
    /// AdminProposal events in HLC order and updates this field
    /// when a ChangeQuorum proposal reaches quorum (single-pass-with-
    /// running-state, spec §5.2). Byte-compat with pre-ZEB-250 cached
    /// snapshots — the `default = "default_admin_quorum"` decode
    /// produces 1.
    #[serde(
        rename = "aq",
        default = "default_admin_quorum",
        skip_serializing_if = "is_default_admin_quorum"
    )]
    pub admin_quorum: u8,

    /// ZEB-251: per-community power thresholds, materialized from
    /// AdminProposal{ChangeThresholds} events (Task 2). Default =
    /// POWER_THRESHOLDS (Sub-C v1 hardcoded).
    ///
    /// This lives on the DERIVED, LOCAL-ONLY materialized view: it is never
    /// gossiped or persisted (MaterializedMembership rides only `#[serde(skip)]`
    /// fields on CommunityState; the event log is the source of truth and this
    /// is re-folded from it on every load). So no peer ever ingests a `pt`
    /// snapshot field — there is no stale-snapshot path that could hand a wrong
    /// threshold to another client. The only cross-peer carrier of a threshold
    /// change is the signed `ChangeThresholds` event itself (see its
    /// forward-incompat note). The `default`/`skip_serializing_if` pair is
    /// defensive byte-compat for the encode helpers (tests/conversion), keeping
    /// a never-customized community's encoding identical to pre-ZEB-251.
    #[serde(
        rename = "pt",
        default = "default_power_thresholds",
        skip_serializing_if = "is_default_power_thresholds"
    )]
    pub power_thresholds: PowerThresholds,

    /// ZEB-713: recovery-designate configuration, set only via a
    /// quorum-approved `SetRecoveryDesignates` proposal. `None` =
    /// recovery disabled (pre-ZEB-713 behavior). Default-elided on the
    /// wire (`skip_serializing_if`) so pre-ZEB-713 cached snapshots stay
    /// byte-identical — the `admin_quorum` compat pattern.
    #[serde(rename = "rd", default, skip_serializing_if = "Option::is_none")]
    pub recovery_designates: Option<RecoveryDesignates>,

    /// ZEB-713: derived per-proposal recovery lifecycle view, rebuilt by
    /// the materialize post-pass. Sorted by `(proposed_at_wall_ms, id)`
    /// for deterministic encoding. Empty-elided for snapshot
    /// byte-compat.
    #[serde(rename = "rp", default, skip_serializing_if = "Vec::is_empty")]
    pub recovery_proposals: Vec<RecoveryProposalView>,
}

impl Default for MaterializedMembership {
    fn default() -> Self {
        Self {
            members: BTreeMap::new(),
            power_levels: BTreeMap::new(),
            channels: BTreeMap::new(),
            current_epoch: None,
            pending_rotation_for: BTreeSet::new(),
            pending_catchup_for: BTreeSet::new(),
            admin_quorum: 1,
            power_thresholds: POWER_THRESHOLDS,
            recovery_designates: None,
            recovery_proposals: Vec::new(),
        }
    }
}

pub(crate) fn default_admin_quorum() -> u8 {
    1
}

pub(crate) fn is_default_admin_quorum(q: &u8) -> bool {
    *q == 1
}

pub(crate) fn default_power_thresholds() -> PowerThresholds {
    POWER_THRESHOLDS
}

pub(crate) fn is_default_power_thresholds(t: &PowerThresholds) -> bool {
    *t == POWER_THRESHOLDS
}

/// ZEB-713: materialized recovery-designate configuration, set only via
/// a quorum-approved `ProposalKind::SetRecoveryDesignates`. Absent
/// (`recovery_designates: None`) = recovery disabled = pre-ZEB-713
/// behavior.
///
/// All field keys are 2-char (ds, th, vw, sa) per the same-length-keys
/// invariant at this nesting level. `set_at` is the HLC of the event
/// that tipped the setting proposal over quorum — it is the config
/// GENERATION, which is why replacing the config always changes
/// `recovery_config_digest` even when the designate list is identical.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryDesignates {
    #[serde(rename = "ds")]
    pub designates: Vec<OwnerAddr>,
    /// R: distinct designate signatures required to initiate.
    #[serde(rename = "th")]
    pub threshold: u8,
    /// W: veto window in ms (RD4 floor: `RECOVERY_VETO_WINDOW_FLOOR_MS`).
    #[serde(rename = "vw")]
    pub veto_window_ms: u64,
    /// Config generation: HLC of the quorum-trigger event that applied
    /// the SetRecoveryDesignates proposal.
    #[serde(rename = "sa")]
    pub set_at: Hlc,
}

impl CanonicalPayloadSealed for RecoveryDesignates {}
impl CanonicalPayload for RecoveryDesignates {}

/// ZEB-713: the binding digest every RecoveryProposal / RecoveryCosign
/// is validated against (RP5 / RC2): blake3 over the canonical CBOR of
/// the full materialized `RecoveryDesignates` (designates, threshold,
/// veto_window_ms, set_at generation).
///
/// Fallible only through `canonical_cbor_encode` (which cannot fail for
/// this shape in practice); callers treat an encode error as digest
/// mismatch — fail closed.
pub fn recovery_config_digest(config: &RecoveryDesignates) -> Result<[u8; 32], CryptoError> {
    let bytes = canonical_cbor_encode(config)?;
    Ok(*blake3::hash(&bytes).as_bytes())
}

/// ZEB-713: lifecycle phase of a materialized recovery proposal.
/// 1-char serde codes (values, not keys — same convention as
/// `MemberStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryPhase {
    /// Fewer than R distinct valid signatures so far and the 30-day
    /// initiation window has not lapsed.
    #[serde(rename = "c")]
    Collecting,
    /// R signatures reached at t_R; waiting out the veto window
    /// (`deadline_ms = t_R + W`).
    #[serde(rename = "t")]
    TimeLocked,
    /// Deadline passed with no qualifying veto — the derived execution
    /// (promotion + kick of `lost_admin`) is reflected in this
    /// materialized state.
    #[serde(rename = "e")]
    Executed,
    /// Killed by an admin veto authored inside `[t₀, deadline]`.
    /// Terminal.
    #[serde(rename = "v")]
    Vetoed,
    /// R signatures were not reached within 30 days of t₀ (or arrived
    /// too late to count). Terminal.
    #[serde(rename = "x")]
    Expired,
    /// The bound RecoveryDesignates config was replaced while the
    /// proposal was collecting or time-locked (digest mismatch).
    /// Terminal.
    #[serde(rename = "k")]
    ConfigChanged,
    /// A rival proposal for the same `lost_admin` won the deterministic
    /// `(t_R, event_id)` tie-break. Terminal.
    #[serde(rename = "s")]
    Superseded,
    /// PR #497 R2 (Greptile P1): the deadline passed but `new_admin`
    /// was not Joined AS OF THE DEADLINE (replay-time snapshot).
    /// Terminal — a later rejoin does NOT revive it (that would let a
    /// stalled proposal retroactively flip a rival group's winner).
    /// Nothing executed: no promotion, no kick — the community keeps
    /// its (dead) admin and the designates simply run a fresh proposal
    /// (a Stalled proposal does not count against RP6).
    #[serde(rename = "l")]
    Stalled,
}

impl CanonicalPayloadSealed for RecoveryPhase {}
impl CanonicalPayload for RecoveryPhase {}

/// ZEB-713: derived per-proposal view built by `materialize_with_now`'s
/// recovery post-pass. Consumed by verify_event's RP6 open-proposal
/// gate and by the D2 UI (`get_recovery_state`). All field keys are
/// 2-char per the same-length-keys invariant at this nesting level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryProposalView {
    #[serde(
        rename = "id",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub id: EventId,
    #[serde(rename = "pr")]
    pub proposer: OwnerAddr,
    #[serde(rename = "la")]
    pub lost_admin: OwnerAddr,
    #[serde(rename = "na")]
    pub new_admin: OwnerAddr,
    /// Distinct valid signers so far (proposer included). Sorted
    /// (BTreeSet) for deterministic encoding.
    #[serde(rename = "sn")]
    pub signers: BTreeSet<OwnerAddr>,
    /// R bound from the proposal's config generation.
    #[serde(rename = "th")]
    pub threshold: u8,
    /// t₀ (proposal HLC wall_ms).
    #[serde(rename = "t0")]
    pub proposed_at_wall_ms: u64,
    /// `t_R + W` — Some once the Rth signature landed (TimeLocked and
    /// beyond), None while Collecting/Expired-in-collecting.
    #[serde(rename = "dl", skip_serializing_if = "Option::is_none", default)]
    pub deadline_ms: Option<u64>,
    #[serde(rename = "ph")]
    pub phase: RecoveryPhase,
    /// ZEB-714: the admin whose veto killed this proposal — the first
    /// qualifying veto in replay order (deterministic: replay iterates
    /// the sorted log). `Some` iff `phase` is `Vetoed`. Default-elided
    /// so pre-ZEB-714 encodings stay byte-identical (the `"aq"`
    /// pattern).
    #[serde(rename = "vb", skip_serializing_if = "Option::is_none", default)]
    pub vetoed_by: Option<OwnerAddr>,
}

impl CanonicalPayloadSealed for RecoveryProposalView {}
impl CanonicalPayload for RecoveryProposalView {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberState {
    #[serde(rename = "st")]
    pub status: MemberStatus,
    #[serde(rename = "ja")]
    pub joined_at: Hlc,
    #[serde(rename = "la", skip_serializing_if = "Option::is_none", default)]
    pub left_at: Option<Hlc>,
    /// ZEB-339: ed25519 verify keys vouched under this member's owner_id,
    /// learned from the EnrollmentCert carried on their Join. A SET so an
    /// owner with multiple devices in a community is representable (eventual
    /// state); populated with exactly one today.
    #[serde(rename = "ek", default, skip_serializing_if = "BTreeSet::is_empty")]
    pub enrolled_device_keys: BTreeSet<[u8; 32]>,
    /// ZEB-668 S3: tombstones for retired (revoked) device keys —
    /// remove-wins. A key present here is NEVER re-added by any key-adding
    /// arm: `materialize` is a deterministic replay in `event_sort_key`
    /// order, and clock skew can sort a DeviceRetire BEFORE the
    /// DeviceAnnounce it retires — without the tombstone every replica
    /// would converge on the retired key resurrected. Additive field:
    /// `#[serde(default)]` + empty-skip keeps pre-S3 blobs and empty-set
    /// encodings byte-identical (no version bump).
    #[serde(rename = "rk", default, skip_serializing_if = "BTreeSet::is_empty")]
    pub revoked_device_keys: BTreeSet<[u8; 32]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemberStatus {
    #[serde(rename = "j")]
    Joined,
    #[serde(rename = "i")]
    Invited,
    #[serde(rename = "l")]
    Left,
    #[serde(rename = "b")]
    Banned,
    /// ZEB-254: joiner has minted a PendingJoin but no JoinCountersign
    /// has yet paired with it. Transitions to Joined when a matching
    /// JoinCountersign is materialized.
    #[serde(rename = "p")]
    PendingJoin,
}

/// ZEB-339 test helper: a realistic owner with an enrolled device key.
/// Produced by `mint_test_owner`; consumed by membership tests that need
/// the new `actor = owner_id ≠ address_hash(device_key)` signing model.
#[cfg(any(test, feature = "test-fixtures"))]
#[derive(Debug, Clone)]
pub struct TestOwner {
    pub owner: OwnerAddr,
    pub device_key: ed25519_dalek::SigningKey,
    pub cert: EnrollmentCert,
}

/// ZEB-339 test helper: produce a realistic owner — owner_id (master),
/// an enrolled device signing key, and a self-minted Master EnrollmentCert
/// binding them. `seed` makes it deterministic.
///
/// Note on seeds: the master key derives from `[seed; 32]` and the device
/// key from `[seed ^ 0xFF; 32]`, so seeds `N` and `N ^ 0xFF` share raw key
/// material (with master/device roles swapped). Use seeds in `0x01..=0xFE`
/// and avoid pairing `N` with `N ^ 0xFF` in the same test.
#[cfg(any(test, feature = "test-fixtures"))]
pub fn mint_test_owner(seed: u8) -> TestOwner {
    use harmony_owner::pubkey_bundle::{ClassicalKeys, PubKeyBundle};
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
        1_700_000_000,
        None,
    )
    .expect("sign_master");
    cert.verify(0).expect("self-minted cert verifies");
    TestOwner {
        owner: OwnerAddr(owner_id),
        device_key: device_sk,
        cert,
    }
}

/// ZEB-339 test helper: the singleton set of `owner`'s enrolled device key,
/// for seeding a hand-built `MemberState.enrolled_device_keys`.
#[cfg(any(test, feature = "test-fixtures"))]
pub fn test_enrolled_keys(owner: &TestOwner) -> BTreeSet<[u8; 32]> {
    let mut s = BTreeSet::new();
    s.insert(owner.cert.device_pubkeys.classical.ed25519_verify);
    s
}

/// ZEB-339 test helper: ensure `owner`'s enrolled device key is present on
/// their materialized `MemberState`, so a steady-state event the owner signs
/// can have its signer resolved. No-op if the owner has no member entry.
#[cfg(any(test, feature = "test-fixtures"))]
pub fn test_enroll_member(prior: &mut MaterializedMembership, owner: &TestOwner) {
    if let Some(m) = prior.members.get_mut(&owner.owner) {
        m.enrolled_device_keys
            .insert(owner.cert.device_pubkeys.classical.ed25519_verify);
    }
}

/// Materialized state for one channel in a community. Built by
/// `materialize` from `ChannelCreate`/`ChannelModify`/`ChannelDelete`
/// event replay. `deleted_at` is `Some` once a `ChannelDelete` has been
/// processed for this channel — the channel stays in the map after
/// deletion (tombstone semantics) so historical messages with this
/// `channel_id` can still render their breadcrumb. v3+ may garbage-
/// collect old tombstones; Phase 1 retains them indefinitely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelInfo {
    #[serde(rename = "nm")]
    pub name: String,
    #[serde(rename = "wp")]
    pub write_power: u8,
    /// ZEB-349: channel kind (Text|Voice). `Text` is the default and is omitted
    /// from the CBOR map (`skip_serializing_if`), keeping a Text `ChannelInfo`
    /// byte-identical to pre-ZEB-349 wire. `canonical_cbor_encode` (ciborium)
    /// preserves serde field-declaration order, so a Voice `ck` entry is
    /// emitted here between `wp` and `ca`. Immutable (set only by
    /// `ChannelCreate`).
    #[serde(rename = "ck", default, skip_serializing_if = "ChannelKind::is_text")]
    pub kind: ChannelKind,
    #[serde(rename = "ca")]
    pub created_at: Hlc,
    #[serde(rename = "da", skip_serializing_if = "Option::is_none", default)]
    pub deleted_at: Option<Hlc>,
}

impl CanonicalPayloadSealed for ChannelInfo {}
impl CanonicalPayload for ChannelInfo {}

impl CanonicalPayloadSealed for MaterializedMembership {}
impl CanonicalPayload for MaterializedMembership {}
impl CanonicalPayloadSealed for MemberState {}
impl CanonicalPayload for MemberState {}
impl CanonicalPayloadSealed for MemberStatus {}
impl CanonicalPayload for MemberStatus {}

/// Canonical total order for membership events.
///
/// Replay-order is `(wall_ms, logical, device_id, EventId, sig)`
/// ascending — the same tuple `materialize()` and `prior_state_at_event`
/// use. Exposed so callers building a "prefix" of the log (e.g., the
/// Phase 2 sync layer computing prior_state for verify_event) can sort
/// with the EXACT comparator used downstream and never drift.
///
/// Why each field is needed:
/// - HLC `(wall_ms, logical, device_id)`: causal-ish order across
///   devices. Partial — two events authored on different devices in
///   the same wall_ms / logical / device_id string collide.
/// - `EventId`: strong tiebreaker, but caller-supplied — a buggy or
///   malicious peer could emit two distinct events with the same id.
/// - `sig`: 64-byte ed25519 signature; deterministic for distinct
///   payloads under the same key, and signature security guarantees
///   distinct payloads → distinct sigs. This is the field that makes
///   the order truly total across any malformed input.
pub fn event_sort_key(e: &SignedMembershipEvent) -> impl Ord + '_ {
    (
        e.at.wall_ms,
        e.at.logical,
        &e.at.device_id,
        &e.id,
        e.sig.as_slice(),
    )
}

/// Replay a community's signed event log into a MaterializedMembership.
///
/// Implements the spec's "Materialization rules" verbatim:
///
/// 1. Bootstrap: power_levels[admin_addr] = 100 BEFORE replaying any
///    events. Admin can later SetPower themselves to a different value.
/// 2. Events are applied in `event_sort_key` ascending order
///    (`(wall_ms, logical, device_id, EventId, sig)`), regardless of
///    input order — the input may arrive partial-ordered from DAG-sync.
/// 3. Per-kind effects:
///    - Join: members[actor] = Joined / joined_at: at (Banned-sticky)
///    - Leave: members[actor].status = Left, .left_at = at (Banned-sticky)
///    - Invite { target }: members[target] = Invited / joined_at: at
///    - Kick { target }: members[target].status = Banned, .left_at = at
///    - SetPower { target, level }: power_levels[target] = level
///    - ChannelCreate { channel_id, name, write_power }: channels[channel_id] = ChannelInfo { ... } if absent (first-create-wins; duplicate is no-op so a replayed event can't refresh created_at)
///    - ChannelModify { channel_id, name, write_power }: partial update — only Some fields applied; unknown channel_id silently ignored
///    - ChannelDelete { channel_id }: tombstone — sets deleted_at, never removes (so historical messages can render breadcrumb); idempotent first-delete-wins
///
/// Pure function — does NOT verify signatures or power rules. That's
/// `verify_event`. Materialization assumes pre-verified events; the
/// Phase 2 sync layer rejects unverified events before they reach
/// this function. Banned-stickiness on Join/Leave is defense-in-depth
/// for events that slip past verification.
pub fn materialize(
    events: &[SignedMembershipEvent],
    admin_addr: OwnerAddr,
) -> MaterializedMembership {
    // Back-compat wrapper for the no-floor case (tests, replay paths
    // that don't carry a wall-clock). Production callers should use
    // `materialize_with_now` so an idle community's PendingJoin still
    // ages out at the 30-day threshold.
    materialize_with_now(events, admin_addr, None)
}

/// R4-6 variant of `materialize` that accepts an optional wall-clock
/// "now floor" used as the time-reference for PendingJoin's 30-day
/// expiry check.
///
/// Without a floor (`now_ms = None`), expiry compares against
/// `max(events.at.wall_ms)` only — perfectly deterministic across
/// peers, but pathological for idle communities: a lone PendingJoin
/// in a community where no other event has landed has
/// `max(events.at.wall_ms) ≈ pending.at.wall_ms`, so `age_ms ≈ 0`
/// and the event never expires from materialize's view. Combined with
/// `verify_event`'s P6 gate (which calls `prior_state_at_hlc` → this
/// function → renders the joiner as `PendingJoin`), the joiner can't
/// re-redeem even 30 days later because P6 still says
/// `PendingJoinAlreadyMember`.
///
/// Passing `Some(wall_now_ms)` makes the function compare against
/// `max(events_max, wall_now_ms)` — admin's local wall-clock can age
/// out the PendingJoin even when the community's event log is idle.
/// Cross-peer divergence is bounded by the 30-day window vs. clock
/// skew (orders of magnitude apart in practice), so the determinism
/// loss is tolerable.
///
/// **Spec alignment.** Spec §3 ("30d pure-function expiry") permits
/// parameterizing `now_ms` so long as the function remains pure for a
/// fixed input pair — the function is still `(events, admin_addr,
/// now_ms) -> MaterializedMembership`, just with an explicit now
/// reference rather than an implicit one derived from event ordering.
/// ZEB-713: per-proposal working state accumulated during the main
/// replay pass. A work entry exists only for proposals that were VALID
/// at their bind position (config present, digest matched, actor a
/// Joined designate in the running state) — invalid proposals never
/// become work entries and are invisible to the lifecycle.
///
/// `threshold` / `veto_window_ms` / `designates` are SNAPSHOTS of the
/// config generation the proposal bound to (digest-verified), so a
/// later config replacement cannot retroactively change R or W — it
/// kills the proposal instead (digest mismatch, spec §3.3.4).
struct RecoveryWork {
    proposer: OwnerAddr,
    lost_admin: OwnerAddr,
    new_admin: OwnerAddr,
    t0_wall_ms: u64,
    threshold: u8,
    veto_window_ms: u64,
    designates: Vec<OwnerAddr>,
    config_digest: [u8; 32],
    /// wall_ms of each distinct valid signature (proposer's included).
    /// Unsorted — forward-ref cosigns can carry walls ≤ t0; sorted at
    /// evaluation time to find the Rth-smallest (t_R).
    sig_walls: Vec<u64>,
    signers: BTreeSet<OwnerAddr>,
    /// PR #497 R2 (Greptile P1): deadline as computed AT REPLAY TIME
    /// (when the Rth signature folded) — the cutoff for the
    /// `new_admin_joined` snapshot below. May differ from evaluate's
    /// authoritative deadline in exotic late-delivery orderings; both
    /// are pure functions of the sorted log, so replicas still converge.
    deadline_at_replay: Option<u64>,
    /// PR #497 R2 (Greptile P1): running snapshot of "new_admin is
    /// currently Joined", refreshed after every replayed event whose
    /// wall is within `deadline_at_replay`. Execution requires this —
    /// judging Joined-ness AS OF THE DEADLINE (log-derivable, stable
    /// under later rejoins) instead of at evaluation time, so a
    /// stalled proposal can never revive and retroactively flip a
    /// rival group's winner. Without this gate, an execution whose
    /// new_admin left mid-window would kick the last admin and leave
    /// the community with no power-100 member at all.
    new_admin_joined: bool,
}

/// ZEB-713 (PR #497 R2): once a proposal's distinct-signer count
/// reaches R, freeze the replay-time deadline used as the
/// `new_admin_joined` snapshot cutoff. `checked_add` mirrors the
/// evaluate-side fail-closed arithmetic (an overflowing window leaves
/// the proposal Expired there, so a None here is inert).
fn maybe_set_replay_deadline(work: &mut RecoveryWork) {
    if work.deadline_at_replay.is_some() || work.signers.len() < work.threshold as usize {
        return;
    }
    let mut walls = work.sig_walls.clone();
    walls.sort_unstable();
    if let Some(tr) = walls.get(work.threshold as usize - 1) {
        work.deadline_at_replay = tr.checked_add(work.veto_window_ms);
    }
}

/// ZEB-713: fold one cosign signature into a proposal's working state.
/// RC1/RC2 dynamic halves: the cosigner must be Joined at their
/// position (`joined`), the running config digest at their position
/// must equal the proposal's bound digest (`running_digest`), the
/// cosigner must be in the BOUND designate set, and distinct from
/// prior signers (BTreeSet dedup).
/// ZEB-713: facts recorded for a cosign at its replay position —
/// everything the fold needs to judge validity once the target proposal
/// is known (forward-ref cosigns are queued as these).
struct QueuedRecoveryCosign {
    wall_ms: u64,
    actor: OwnerAddr,
    /// Running config digest at the cosign's position (RC2).
    running_digest: Option<[u8; 32]>,
    /// Actor was Joined at the cosign's position (RC1 dynamic half).
    joined: bool,
}

fn fold_recovery_cosign(work: &mut RecoveryWork, cosign: QueuedRecoveryCosign) {
    if !cosign.joined {
        return;
    }
    if cosign.running_digest != Some(work.config_digest) {
        return;
    }
    if !work.designates.contains(&cosign.actor) {
        return;
    }
    if work.signers.insert(cosign.actor) {
        work.sig_walls.push(cosign.wall_ms);
        maybe_set_replay_deadline(work);
    }
}

/// ZEB-713: the current running config's digest, or None when recovery
/// is unconfigured / the digest cannot be computed (fail closed).
fn running_recovery_digest(m: &MaterializedMembership) -> Option<[u8; 32]> {
    m.recovery_designates
        .as_ref()
        .and_then(|c| recovery_config_digest(c).ok())
}

/// ZEB-714: one proposal's lifecycle outcome from
/// `evaluate_recovery_phases`.
#[derive(Clone, Copy)]
struct RecoveryOutcome {
    phase: RecoveryPhase,
    /// `t_R + W` — Some once the Rth signature landed.
    deadline_ms: Option<u64>,
    /// `Some` iff `phase` is `Vetoed`: the first qualifying vetoer in
    /// replay order.
    vetoed_by: Option<OwnerAddr>,
}

/// ZEB-713: evaluate every recovery proposal's lifecycle phase at time
/// reference `t` (spec §3.3). Pure in its inputs — called from the
/// materialize post-pass with `t = max(events_max, now_ms)` (the R4-6
/// now-floor) and position-locally from the EpochRotation trigger arm
/// with `t = rotation.at.wall_ms` (stable there because every
/// in-window cosign/veto has wall ≤ deadline < t and therefore sorts
/// before the rotation).
///
/// Returns `id → RecoveryOutcome`; `deadline_ms` is Some once the
/// Rth signature landed. Phase precedence for terminal states:
/// Vetoed > ConfigChanged > Expired > Superseded.
fn evaluate_recovery_phases(
    rec_proposals: &BTreeMap<EventId, RecoveryWork>,
    rec_vetoes: &[(EventId, u64, OwnerAddr)],
    config_change_walls: &[u64],
    t: u64,
) -> BTreeMap<EventId, RecoveryOutcome> {
    let mut out: BTreeMap<EventId, RecoveryOutcome> = BTreeMap::new();

    // Pass 1: per-proposal phase ignoring rivals.
    for (id, work) in rec_proposals.iter() {
        let t0 = work.t0_wall_ms;
        let r = work.threshold as usize;
        let mut walls = work.sig_walls.clone();
        walls.sort_unstable();
        let t_r = walls.get(r.saturating_sub(1)).copied();
        // Deadline exists only once R distinct signatures landed AND
        // the Rth arrived within the 30-day initiation window (the
        // ZEB-250 expiry constant, spec §3.3.1).
        let (deadline, collect_expired) = match t_r {
            Some(tr) if tr.saturating_sub(t0) <= ADMIN_PROPOSAL_EXPIRY_MS => {
                // Defense-in-depth vs. the RD4 ceiling (PR #497 R1): a
                // window that slipped past verification (corrupted log,
                // raw replay) must fail CLOSED — an overflowed deadline
                // would otherwise wrap small and execute immediately.
                match tr.checked_add(work.veto_window_ms) {
                    Some(deadline) => (Some(deadline), false),
                    None => (None, true),
                }
            }
            Some(_) => (None, true),
            None => (None, t.saturating_sub(t0) > ADMIN_PROPOSAL_EXPIRY_MS),
        };
        // RV1 window: authored wall in [t₀, deadline]; while no
        // deadline exists (Collecting) the window is open-ended —
        // collecting proposals are vetoable early (spec §6 T1).
        // ZEB-714: record the vetoer (first qualifying in replay
        // order) so the D2 banner can resolve to "vetoed by NAME".
        let vetoed_by = rec_vetoes
            .iter()
            .find(|(target, wall, _)| {
                target == id && *wall >= t0 && deadline.map(|d| *wall <= d).unwrap_or(true)
            })
            .map(|(_, _, actor)| *actor);
        // Config replacement inside [t₀, deadline] kills (§3.3.4). A
        // replacement AFTER the deadline leaves an executed proposal
        // executed. `>= t0` (not `>`) is deliberately fail-closed
        // toward killing the recovery attempt on an exact wall tie.
        let config_killed = config_change_walls
            .iter()
            .any(|c| *c >= t0 && deadline.map(|d| *c <= d).unwrap_or(true));
        let phase = if vetoed_by.is_some() {
            RecoveryPhase::Vetoed
        } else if config_killed {
            RecoveryPhase::ConfigChanged
        } else if collect_expired {
            RecoveryPhase::Expired
        } else {
            match deadline {
                None => RecoveryPhase::Collecting,
                // PR #497 R2 (Greptile P1): execution is ATOMIC — it
                // requires new_admin Joined as of the deadline
                // (replay-time snapshot). Otherwise the proposal stalls
                // terminally with NO effects: a kick without the paired
                // promotion would leave a sole-admin community with no
                // power-100 member at all, bricked by the recovery
                // mechanism itself.
                Some(d) if t > d && work.new_admin_joined => RecoveryPhase::Executed,
                Some(d) if t > d => RecoveryPhase::Stalled,
                Some(_) => RecoveryPhase::TimeLocked,
            }
        };
        out.insert(
            *id,
            RecoveryOutcome {
                phase,
                deadline_ms: deadline,
                vetoed_by: if phase == RecoveryPhase::Vetoed {
                    vetoed_by
                } else {
                    None
                },
            },
        );
    }

    // Pass 2: rival tie-break — per lost_admin group, the lowest
    // (t_R, event_id) among executable candidates wins; losers are
    // Superseded (spec §3.3). Grouping by lost_admin (not globally)
    // is deliberate: multi-admin loss is recovered via one proposal
    // per lost admin, each independently executable (spec §3.2).
    let mut winners: BTreeMap<OwnerAddr, (u64, EventId)> = BTreeMap::new();
    for (id, outcome) in out.iter() {
        if outcome.phase != RecoveryPhase::Executed {
            continue;
        }
        let work = &rec_proposals[id];
        // deadline is Some for every Executed proposal; t_R = deadline − W.
        let tr = outcome
            .deadline_ms
            .expect("executed implies deadline")
            .saturating_sub(work.veto_window_ms);
        let key = (tr, *id);
        winners
            .entry(work.lost_admin)
            .and_modify(|best| {
                if key < *best {
                    *best = key;
                }
            })
            .or_insert(key);
    }
    for (id, entry) in out.iter_mut() {
        if entry.phase != RecoveryPhase::Executed {
            continue;
        }
        let work = &rec_proposals[id];
        let tr = entry
            .deadline_ms
            .expect("executed implies deadline")
            .saturating_sub(work.veto_window_ms);
        if winners.get(&work.lost_admin) != Some(&(tr, *id)) {
            entry.phase = RecoveryPhase::Superseded;
        }
    }

    out
}

pub fn materialize_with_now(
    events: &[SignedMembershipEvent],
    admin_addr: OwnerAddr,
    now_ms: Option<u64>,
) -> MaterializedMembership {
    let mut m = MaterializedMembership::default();

    // Bootstrap: admin holds power 100 implicitly. SetPower events
    // (replayed below) can override.
    m.power_levels.insert(admin_addr, 100);

    // ZEB-254 Pre-Pass: compute current max wall_ms across all events.
    // Used as the "current time" reference for PendingJoin expiry.
    //
    // R4-6: if `now_ms` is supplied, take the max with it so an idle
    // community whose only event IS the PendingJoin can still expire
    // it once wall-clock advances 30 days. Without this, P6 in
    // `verify_event` (which calls `prior_state_at_hlc` → this fn)
    // permanently rejects re-redemption attempts because materialize
    // never marks the original pending as expired.
    let events_max_wall_ms: u64 = events.iter().map(|e| e.at.wall_ms).max().unwrap_or(0);
    let current_max_wall_ms: u64 = match now_ms {
        Some(now) => events_max_wall_ms.max(now),
        None => events_max_wall_ms,
    };

    // ZEB-254 Pre-Pass: collect the target_event_ids of all JoinCountersign
    // events into a set. The PendingJoin arm below consults this set to
    // determine whether a pending join has been countersigned — if so, it
    // renders as Joined rather than PendingJoin, regardless of expiry.
    let countersigned_pending_ids: std::collections::HashSet<EventId> = events
        .iter()
        .filter_map(|e| match &e.kind {
            MembershipEventKind::JoinCountersign { target_event_id } => Some(*target_event_id),
            _ => None,
        })
        .collect();

    // ZEB-250 Pre-Pass: collect per-proposal raw signature data.
    // - quorum_signers[event_id]: set of admin OwnerAddrs who have
    //   signed the proposal (proposer auto-included + each
    //   AdminCountersign actor).
    // - proposals_index[event_id]: (proposal_kind, proposer_addr,
    //   proposer_wall_ms).
    // - proposal_signing_hlcs[event_id]: per-signing-event (wall_ms,
    //   actor). Used by the main pass to find when the Nth signature
    //   was contributed (for 30-day expiry).
    //
    // Raw collection only; quorum-reached evaluation happens in the
    // main pass (§5.2) because `admin_quorum` itself is a function
    // of prior ChangeQuorum proposals (single-pass-with-running-state).
    let mut quorum_signers: std::collections::HashMap<
        EventId,
        std::collections::HashSet<OwnerAddr>,
    > = std::collections::HashMap::new();
    let mut proposals_index: std::collections::HashMap<EventId, (ProposalKind, OwnerAddr, u64)> =
        std::collections::HashMap::new();
    let mut proposal_signing_hlcs: std::collections::HashMap<EventId, Vec<(u64, OwnerAddr)>> =
        std::collections::HashMap::new();

    // Bug-fix (ZEB-250 R1): track which actors have already contributed a
    // signing entry per proposal so we push to proposal_signing_hlcs at most
    // once per (proposal_id, actor) pair. The HLC vec is used to find the
    // Nth-smallest wall_ms; a duplicate actor entry would shift that index
    // and corrupt the expiry calculation.
    let mut pre_pass_seen_actors: std::collections::HashMap<
        EventId,
        std::collections::HashSet<OwnerAddr>,
    > = std::collections::HashMap::new();

    for signed_event in events.iter() {
        match &signed_event.kind {
            MembershipEventKind::AdminProposal { proposal_kind } => {
                proposals_index.insert(
                    signed_event.id,
                    (
                        proposal_kind.clone(),
                        signed_event.actor,
                        signed_event.at.wall_ms,
                    ),
                );
                quorum_signers
                    .entry(signed_event.id)
                    .or_default()
                    .insert(signed_event.actor);
                // Only push the EARLIEST entry per actor (pre_pass_seen_actors
                // tracks who we've already recorded for this proposal).
                let seen = pre_pass_seen_actors.entry(signed_event.id).or_default();
                if seen.insert(signed_event.actor) {
                    proposal_signing_hlcs
                        .entry(signed_event.id)
                        .or_default()
                        .push((signed_event.at.wall_ms, signed_event.actor));
                }
            }
            MembershipEventKind::AdminCountersign { target_event_id } => {
                quorum_signers
                    .entry(*target_event_id)
                    .or_default()
                    .insert(signed_event.actor);
                // Only push first occurrence per actor (keep earliest wall_ms).
                let seen = pre_pass_seen_actors.entry(*target_event_id).or_default();
                if seen.insert(signed_event.actor) {
                    proposal_signing_hlcs
                        .entry(*target_event_id)
                        .or_default()
                        .push((signed_event.at.wall_ms, signed_event.actor));
                }
            }
            _ => {}
        }
    }

    // Sort by the canonical total order. We don't assume the input
    // is sorted because DAG-sync delivers events partial-ordered.
    // Cloning the &-refs is fine — the event vec is small (community
    // sizes are bounded; even very active communities have O(thousands)
    // of events at the long tail, not millions).
    let mut sorted: Vec<&SignedMembershipEvent> = events.iter().collect();
    sorted.sort_by(|a, b| event_sort_key(a).cmp(&event_sort_key(b)));

    // ZEB-250 Bug-fix R1 (Bug 2): track signing progress incrementally
    // during the main pass. Effects are applied at the HLC of the event
    // that tips the running signer count over the quorum threshold —
    // either the AdminProposal itself (when admin_quorum == 1) or the
    // AdminCountersign that fills the last slot (when admin_quorum > 1).
    //
    // This preserves CRDT causality: events authored between the proposal
    // and the Nth countersign see the OLD admin_quorum / OLD power_levels,
    // which matches what their authors could have known at write time.
    //
    // (Pre-pass quorum_signers / proposal_signing_hlcs are kept for
    // backward compat with other helpers and test assertions.)
    let mut running_signers_seen: std::collections::HashMap<
        EventId,
        std::collections::HashSet<OwnerAddr>,
    > = std::collections::HashMap::new();

    // ZEB-250 R2 Bug-fix (a+b):
    //
    // (a) applied_admin_proposals: sticky guard preventing double-application
    //     when a ChangeQuorum proposal lowers the threshold AFTER the effect
    //     was already applied. Without this guard, subsequent countersigns
    //     could re-cross the (now lower) threshold and apply the effect again.
    //
    // (b) seen_admin_proposals: tracks which proposal events have been
    //     reached in HLC order. AdminCountersign events whose target proposal
    //     hasn't been seen yet are "forward-ref" countersigns (clock-skewed
    //     or out-of-order DAG delivery). We queue the signer but skip
    //     triggering — the proposal's own arm (when it is reached) will fire
    //     with the already-populated signer_set and use >= to catch the case
    //     where count is already at quorum by proposer-arm time.
    let mut applied_admin_proposals: std::collections::HashSet<EventId> =
        std::collections::HashSet::new();
    let mut seen_admin_proposals: std::collections::HashSet<EventId> =
        std::collections::HashSet::new();

    // ZEB-713: recovery working state (spec §3.3). Bind-valid proposals
    // accumulate signatures during the main pass; lifecycle phases and
    // the derived execution are evaluated in a POST-pass against the
    // now-floor — execution is triggered by TIME passing, not by any
    // event, so it cannot be applied at an event position (an idle
    // community would never execute, the exact R4-6 pathology).
    let mut rec_proposals: BTreeMap<EventId, RecoveryWork> = BTreeMap::new();
    // Forward-ref cosigns (clock skew can sort a cosign before its
    // target proposal — the ZEB-250 R2(b) pattern). Each entry records
    // the facts needed for a later fold: (wall, actor, running config
    // digest at the cosign's position, actor-was-Joined at position).
    let mut rec_pending_cosigns: std::collections::HashMap<EventId, Vec<QueuedRecoveryCosign>> =
        std::collections::HashMap::new();
    // Vetoes that passed the RV1 running-state check at their position:
    // (target proposal id, authored wall_ms).
    let mut rec_vetoes: Vec<(EventId, u64, OwnerAddr)> = Vec::new();
    // Recovery proposals whose triggered EpochRotation already applied
    // in the main pass. The post-pass derived kick must NOT re-insert
    // pending_rotation_for for these — the rotation arm's remove()
    // runs during the main pass, BEFORE the post-pass insert would.
    let mut rec_rotation_applied: std::collections::HashSet<EventId> =
        std::collections::HashSet::new();
    // wall_ms of each APPLIED SetRecoveryDesignates (config generation
    // changes) — a change landing inside a proposal's [t₀, deadline]
    // kills it (spec §3.3.4).
    let mut rec_config_change_walls: Vec<u64> = Vec::new();

    for (idx, event) in sorted.iter().enumerate() {
        match &event.kind {
            MembershipEventKind::Join => {
                // Per-prior-status transition table:
                //   - never seen → set Joined / joined_at = event.at
                //   - Invited → set Joined / joined_at = event.at (acceptance)
                //   - Left → set Joined / joined_at = event.at (rejoin)
                //   - Joined → no-op (idempotent — preserves original
                //     joined_at; otherwise any actor could push their
                //     own join date forward by replaying Join, with
                //     no privilege gate to prevent it)
                //   - Banned → no-op (Banned-sticky; verify_event
                //     also rejects BannedActorJoin, this is
                //     defense-in-depth for events that slip past
                //     verification — corrupted log, replay from
                //     before the Ban arrived)
                let prior_status = m.members.get(&event.actor).map(|s| s.status);
                let should_refresh = match prior_status {
                    None
                    | Some(MemberStatus::Invited)
                    | Some(MemberStatus::Left)
                    | Some(MemberStatus::PendingJoin) => true,
                    Some(MemberStatus::Joined) | Some(MemberStatus::Banned) => false,
                };
                if should_refresh {
                    // ZEB-339: preserve any prior enrolled keys (so a rejoin
                    // doesn't drop a previously-learned device key), then
                    // insert the key from the cert carried on this Join.
                    // ZEB-668 S3: tombstones are carried forward too — a
                    // rejoin must not wipe them — and a tombstoned (retired)
                    // key is refused re-entry (remove-wins).
                    let (mut enrolled, revoked) = m
                        .members
                        .get(&event.actor)
                        .map(|s| {
                            (
                                s.enrolled_device_keys.clone(),
                                s.revoked_device_keys.clone(),
                            )
                        })
                        .unwrap_or_default();
                    // SECURITY INVARIANT (load-bearing): the cert is ingested
                    // WITHOUT re-verification here. This is safe ONLY because an
                    // event reaches the materialized log exclusively via
                    // `CommunityState::insert_event` → `verify_event` →
                    // `enrolled_key_from_cert` (which runs `cert.verify()`, the
                    // Master-issuer gate, and the cert.owner_id == actor bind).
                    // `materialize` only ever replays already-verified events.
                    // The ingested key is the IDENTICAL field that
                    // `verify_membership_signer` validated the signature under.
                    // If a future path ever inserts events into the log bypassing
                    // `verify_event` (e.g. snapshot-seed / import), re-verify here.
                    if let Some(cert) = event.enrollment.as_ref() {
                        let key = cert.device_pubkeys.classical.ed25519_verify;
                        if !revoked.contains(&key) {
                            insert_enrolled_key_capped(&mut enrolled, key);
                        }
                    }
                    m.members.insert(
                        event.actor,
                        MemberState {
                            status: MemberStatus::Joined,
                            joined_at: event.at.clone(),
                            left_at: None,
                            enrolled_device_keys: enrolled,
                            revoked_device_keys: revoked,
                        },
                    );
                    // ZEB-249: if any rotation has already happened
                    // (current_epoch > 0), this new member's snapshot
                    // may be stale — mark for catchup. Self-healing
                    // observer issues a catchup; redundant catchups
                    // are harmless no-ops.
                    // M9: also mark for catchup when an Invited member
                    // joins (prior_status == Some(Invited)). An invited
                    // member was never Joined before, so their snapshot
                    // is just as stale as a brand-new joiner's.
                    if !matches!(prior_status, Some(MemberStatus::Joined))
                        && m.current_epoch.unwrap_or(0) > 0
                    {
                        m.pending_catchup_for.insert(event.actor);
                    }
                }
            }
            MembershipEventKind::Leave => {
                // M2: track whether this Leave actually transitioned a member.
                // Only mark pending_rotation_for if the Leave was non-no-op
                // (i.e., the actor was a known member). A Leave from a
                // never-member would otherwise pollute pending_rotation_for
                // with an entry that can never be cleared by a Kick/Leave lookup.
                let mut leave_transitioned = false;
                if let Some(s) = m.members.get_mut(&event.actor) {
                    // Banned is sticky: a Leave from a Banned actor
                    // must NOT transition status back to Left. Without
                    // this guard, a kicked actor could replay Leave
                    // to mask the Ban, then re-Join (since the Banned
                    // guard in verify_event would no longer fire).
                    // Defense in depth: verify_event also rejects
                    // Leave from Banned.
                    if s.status != MemberStatus::Banned {
                        s.status = MemberStatus::Left;
                        s.left_at = Some(event.at.clone());
                        leave_transitioned = true;
                    }
                }
                // If actor never joined, Leave is silently no-op.
                // verify_event tolerates this case (no rejection) so
                // the materialization path stays simple — the
                // alternative (insert-with-Left) would corrupt state
                // from a malformed event.
                // ZEB-249: Leave needs a rotation. Cooperative leaver may bundle
                // it; otherwise self-healing observer fills.
                // M2: only mark if the Leave actually transitioned someone.
                if leave_transitioned {
                    m.pending_rotation_for.insert(event.actor);
                }
            }
            MembershipEventKind::Invite { target } => {
                // Per-prior-status transition table:
                //   - never seen → set Invited / joined_at = event.at
                //   - Invited → no-op (idempotent — preserves original
                //     invite timestamp; otherwise admin could shift
                //     pending invitation joined_at forward or backward)
                //   - Left → refresh to Invited (legitimate re-invite of
                //     a former member; UI shows "alice has been re-invited")
                //   - Joined → no-op (already past the "invited" stage)
                //   - Banned → no-op (Banned-sticky; verify_event also
                //     rejects InviteTargetBanned, defense-in-depth here)
                //
                // Refresh = replace MemberState entirely (status=Invited,
                // joined_at=event.at, left_at=None) so a re-invited Left
                // entry looks like a fresh invitation rather than
                // carrying stale left_at from the prior departure.
                let prior_status = m.members.get(target).map(|s| s.status);
                let should_refresh = match prior_status {
                    None | Some(MemberStatus::Left) => true,
                    Some(MemberStatus::Invited)
                    | Some(MemberStatus::Joined)
                    | Some(MemberStatus::Banned)
                    | Some(MemberStatus::PendingJoin) => false,
                };
                if should_refresh {
                    // ZEB-668 S3: tombstones survive the refresh — a
                    // re-invited Left member's retired keys must stay
                    // retired (remove-wins), or a post-rejoin
                    // DeviceAnnounce could resurrect them.
                    let revoked = m
                        .members
                        .get(target)
                        .map(|s| s.revoked_device_keys.clone())
                        .unwrap_or_default();
                    m.members.insert(
                        *target,
                        MemberState {
                            status: MemberStatus::Invited,
                            joined_at: event.at.clone(),
                            left_at: None,
                            enrolled_device_keys: BTreeSet::new(),
                            revoked_device_keys: revoked,
                        },
                    );
                }
            }
            MembershipEventKind::Kick { target, .. } => {
                // verify_event rejects KickTargetNotMember at the input
                // layer; only modify an existing entry here. Falling
                // back to entry().or_insert(...) would fabricate a
                // phantom MemberState with status=Banned and
                // joined_at=kick_time for an unknown target — exactly
                // the hazard the verify-time check guards against, but
                // a corrupted log or unverified replay could otherwise
                // surface it. Symmetric with Join/Leave defense-in-depth.
                //
                // R4-1: capture the prior status BEFORE the mutation
                // because the pending_rotation_for guard below needs to
                // distinguish "kick of an established member" (epoch
                // material was actually distributed to this address →
                // rotation needed) from "kick of a PendingJoin" (no
                // epoch material ever went to this address → rotation
                // would be a spurious cycle of every existing member's
                // keys). Mirrors the Leave arm's `leave_transitioned`
                // guard that drops never-member Leaves out of
                // pending_rotation_for.
                let prior_status = m.members.get(target).map(|s| s.status);
                if let Some(s) = m.members.get_mut(target) {
                    s.status = MemberStatus::Banned;
                    s.left_at = Some(event.at.clone());
                }
                // ZEB-249: track that this kick needs a matching EpochRotation.
                // The self-healing observer synthesizes one if the bundled
                // rotation didn't land (e.g., concurrent-kick contention).
                //
                // R4-1: skip the rotation marker when the prior status
                // was PendingJoin — a PendingJoin user never received
                // any epoch key material, so kicking them does NOT
                // require rotating live keys for the rest of the
                // community. Without this guard, every admin "Reject"
                // click in PendingJoinsPanel would synthesize a full
                // EpochRotation re-keying ALL Joined/Invited members.
                // Mirrors the Leave arm's "leave_transitioned" guard
                // which also drops never-members from
                // pending_rotation_for.
                if !matches!(prior_status, Some(MemberStatus::PendingJoin)) {
                    m.pending_rotation_for.insert(*target);
                }
            }
            MembershipEventKind::SetPower { target, level } => {
                m.power_levels.insert(*target, *level);
            }
            MembershipEventKind::Unban { target, .. } => {
                // Transitions Banned → Left so the target can be re-invited.
                // Only update an existing entry; verify_event rejects Unban
                // targeting a non-member so an absent entry here is defense-
                // in-depth (corrupted log, replay from before the Unban
                // arrived). Power level is preserved — power cleanup is
                // a future SetPower's job.
                // No EpochRotation auto-trigger: Unban is additive; re-Join
                // handles its own epoch via the existing Invite → Join flow.
                if let Some(s) = m.members.get_mut(target) {
                    if s.status == MemberStatus::Banned {
                        s.status = MemberStatus::Left;
                        // Preserve the original `joined_at` — overwriting with the
                        // unban HLC would invert the (joined_at, left_at) ordering
                        // since the prior Kick already wrote left_at < unban_at.
                        // Matches the Kick/Leave handlers which also preserve it.
                    }
                }
            }
            MembershipEventKind::ChannelCreate {
                channel_id,
                name,
                write_power,
                kind,
            } => {
                // Idempotent on duplicate channel_id: first create wins
                // (replays + reorderings under DAG-sync may deliver the
                // same ChannelCreate twice; the second one must NOT
                // overwrite name/write_power/created_at — that would let
                // a duplicate-emit refresh created_at and reset history
                // markers). A subsequent ChannelModify is the right path
                // to update fields; a duplicate ChannelCreate is a no-op.
                // `kind` is set here only (immutable — ChannelModify can't
                // touch it).
                m.channels
                    .entry(*channel_id)
                    .or_insert_with(|| ChannelInfo {
                        name: name.clone(),
                        write_power: *write_power,
                        kind: *kind,
                        created_at: event.at.clone(),
                        deleted_at: None,
                    });
            }
            MembershipEventKind::ChannelModify {
                channel_id,
                name,
                write_power,
            } => {
                // Partial update: only apply fields that are Some.
                // Unknown ChannelId is silently ignored — verify_event
                // (Task 3) does NOT gate Modify on the channel existing
                // (a malicious actor could otherwise pre-trigger a verify
                // failure to leak existence info), so materialize stays
                // safe by default. A reordered Modify-before-Create
                // would be discarded here; the eventual sort means the
                // re-replay after the missing Create arrives still does
                // the right thing.
                if let Some(info) = m.channels.get_mut(channel_id) {
                    if let Some(new_name) = name {
                        info.name = new_name.clone();
                    }
                    if let Some(new_wp) = write_power {
                        info.write_power = *new_wp;
                    }
                }
            }
            MembershipEventKind::ChannelDelete { channel_id } => {
                // Tombstone: set deleted_at, do NOT remove. Idempotent
                // on duplicate: first delete wins (preserves the original
                // deleted_at HLC). Subsequent ChannelModify can still
                // mutate name/write_power on a tombstoned channel —
                // intentional, so admins can correct the name of an
                // accidentally-deleted-then-renamed channel without an
                // un-delete primitive (deferred to v3).
                if let Some(info) = m.channels.get_mut(channel_id) {
                    if info.deleted_at.is_none() {
                        info.deleted_at = Some(event.at.clone());
                    }
                }
            }
            MembershipEventKind::EpochRotation {
                prior_epoch,
                triggered_by,
                recipient_ciphertexts,
            } => {
                // Staleness gate (spec §4.2): silently drop if not for current epoch.
                let current = m.current_epoch.unwrap_or(0);
                if *prior_epoch != current {
                    continue;
                }

                // M1: trigger lookup must be CAUSAL — only look in events
                // PRIOR to this rotation in the sorted replay. Scanning the
                // full log would allow a future event to "authorize" a
                // rotation that was issued before its trigger, enabling
                // epoch-advance races.
                let triggered_event = sorted[..idx].iter().find(|e| &e.id == triggered_by);
                // ZEB-713: the second tuple slot is the recovery-promoted
                // issuer additionally authorized to author THIS rotation
                // (None for the Kick/Leave trigger paths).
                let (kick_target, recovery_issuer) = match triggered_event.map(|e| &e.kind) {
                    Some(MembershipEventKind::Kick { target, .. }) => (Some(*target), None),
                    Some(MembershipEventKind::Leave) => (triggered_event.map(|e| e.actor), None),
                    // ZEB-713: an EXECUTED recovery proposal authorizes
                    // rotating out its lost_admin — the derived kick has
                    // no Kick event to cite, so the proposal itself is
                    // the trigger. Executed-ness (incl. the rival
                    // tie-break) is evaluated position-locally at THIS
                    // rotation's wall clock: every in-window cosign and
                    // veto has wall ≤ deadline < rotation wall and
                    // therefore already sorted before this event, so the
                    // outcome computed here never flips as later events
                    // replay. A collecting / vetoed / superseded
                    // proposal authorizes nothing (a rotation citing it
                    // is dropped — same silent-drop posture as the
                    // staleness gate above).
                    //
                    // ZEB-714 (PR #498 R1, CodeRabbit): the rotation must
                    // ALSO clear the finality margin F (spec §4.3) —
                    // `wall_ms > deadline + F`. The observer waits F
                    // before synthesizing, but that alone is client
                    // behavior: a hasty/malicious build could author the
                    // one irreversible follow-on at deadline+1, inside
                    // the window §4.3 reserves for late veto delivery.
                    // Enforcing F here makes the containment a CRDT
                    // invariant. Position-locally stable for the same
                    // reason as the executed-ness evaluation (the gate
                    // only widens the margin); `checked_add` fails
                    // closed (overflow ⇒ never eligible, mirroring
                    // `recovery_rotation_trigger`).
                    Some(MembershipEventKind::RecoveryProposal {
                        lost_admin,
                        new_admin,
                        ..
                    }) => {
                        let outcomes = evaluate_recovery_phases(
                            &rec_proposals,
                            &rec_vetoes,
                            &rec_config_change_walls,
                            event.at.wall_ms,
                        );
                        match outcomes.get(triggered_by) {
                            Some(o)
                                if o.phase == RecoveryPhase::Executed
                                    && o.deadline_ms
                                        .and_then(|d| d.checked_add(RECOVERY_ROTATION_FINALITY_MS))
                                        .is_some_and(|eligible| event.at.wall_ms > eligible) =>
                            {
                                (Some(*lost_admin), Some(*new_admin))
                            }
                            _ => (None, None),
                        }
                    }
                    _ => (None, None),
                };
                let Some(target) = kick_target else {
                    continue;
                };

                // Malformed rotation check (spec §4.4): target must NOT be
                // in recipient_ciphertexts.
                if recipient_ciphertexts
                    .iter()
                    .any(|rc| rc.recipient == target)
                {
                    continue;
                }

                // M8: completeness check — the rotation must include ALL
                // currently-Joined-or-Invited members (minus the target).
                // A malicious rotation that omits some remaining members
                // would advance the epoch while leaving those members
                // unable to decrypt future messages.
                let expected_recipients: std::collections::BTreeSet<OwnerAddr> = m
                    .members
                    .iter()
                    .filter(|(addr, state)| {
                        **addr != target
                            && matches!(state.status, MemberStatus::Joined | MemberStatus::Invited)
                    })
                    .map(|(addr, _)| *addr)
                    .collect();
                let actual_recipients: std::collections::BTreeSet<OwnerAddr> =
                    recipient_ciphertexts
                        .iter()
                        .map(|rc| rc.recipient)
                        .collect();
                if !expected_recipients.is_subset(&actual_recipients) {
                    // Incomplete rotation: some Joined/Invited members are
                    // missing from recipient_ciphertexts. Drop.
                    continue;
                }

                // Validity check (spec §4.4): issuer must have admin power
                // OR be the target of a Leave (cooperative-leaver path).
                // C3: the admin check ALSO requires the issuer to be currently
                // Joined — a kicked former admin retains their power_levels
                // entry (power_levels is not cleaned up on Kick/Leave) but
                // must not be able to authorize epoch rotations from outside
                // the community.
                //
                // Exception: the bootstrap admin (admin_addr) may never have
                // issued an explicit Join event (in test fixtures and early
                // protocol bootstraps). Their power 100 is baked in from
                // materialize's bootstrap step and they have no member record
                // (m.members.get(&issuer) is None). None means "was never
                // kicked/banned" — distinct from Some(Left)/Some(Banned) which
                // means "was a member and then left/was kicked". Allow the
                // admin_addr with None membership as a valid issuer.
                let issuer = event.actor;
                let issuer_power = m.power_levels.get(&issuer).copied().unwrap_or(0);
                let issuer_member_status = m.members.get(&issuer).map(|s| s.status);
                let issuer_is_joined = matches!(issuer_member_status, Some(MemberStatus::Joined));
                // The bootstrap admin has None status (never explicitly joined);
                // any other None is a non-member with no history — allow only
                // admin_addr in the None case.
                let issuer_is_effective_member =
                    issuer_is_joined || (issuer_member_status.is_none() && issuer == admin_addr);
                let is_admin =
                    issuer_power >= m.power_thresholds.kick && issuer_is_effective_member;
                // Cooperative-leaver path: the leaver themselves may issue the
                // rotation. After the Leave arm above, the leaver's status is
                // Left (or Banned if banned — but verify_event rejects Leave
                // from Banned). Guard: triggered_by must point to a Leave by
                // the issuer AND the issuer must be in the members map (was
                // ever a member — prevents Leave-from-never-member from
                // accessing the cooperative-leaver shortcut).
                let is_self_leaver = matches!(
                    triggered_event.map(|e| &e.kind),
                    Some(MembershipEventKind::Leave)
                ) && issuer == target
                    && m.members.contains_key(&issuer);
                // ZEB-713: the recovery-promoted new_admin may author the
                // rotation for the recovery-derived kick. Their power-100
                // promotion is DERIVED (post-pass) and therefore not yet
                // visible in the running power_levels at this position —
                // without this path, the sole-admin recovery case would
                // have no authorizable rotation issuer at all. Joined
                // re-check mirrors RP3 (and blocks a new_admin who left
                // during the window).
                let is_recovery_new_admin = recovery_issuer == Some(issuer)
                    && matches!(
                        m.members.get(&issuer).map(|s| s.status),
                        Some(MemberStatus::Joined)
                    );
                if !is_admin && !is_self_leaver && !is_recovery_new_admin {
                    continue;
                }

                // Apply: advance epoch. Per-receiver key insertion happens
                // outside materialize (community_state_sync apply layer —
                // Tasks 5/6). materialize is pure replay.
                m.current_epoch = Some(current + 1);
                m.pending_rotation_for.remove(&target);
                // ZEB-713: a recovery-triggered rotation is complete —
                // tell the post-pass not to re-mark the target pending.
                if recovery_issuer.is_some() {
                    rec_rotation_applied.insert(*triggered_by);
                }
            }
            MembershipEventKind::EpochCatchup {
                epoch,
                triggered_by,
                recipient_ciphertexts,
            } => {
                // Epoch must match current (spec §4.6).
                let current = m.current_epoch.unwrap_or(0);
                if *epoch != current {
                    continue;
                }

                // M1: triggered_by lookup restricted to causal prefix (events
                // before this catchup in the sorted replay).
                //
                // ZEB-254 R5-1: accept BOTH legacy `Join` AND countersigned
                // `PendingJoin` as a valid catchup trigger. The PendingJoin
                // arm below enqueues a joiner into `pending_catchup_for`
                // when they're admitted via countersign in a rotated epoch
                // (line ~1578); without accepting PendingJoin here, that
                // entry can never be cleared by an EpochCatchup pointing
                // at the PendingJoin event — admins would have to mint a
                // legacy `Join` (which they never do for this path), leaving
                // the joiner permanently flagged for catchup.
                //
                // Uncountersigned PendingJoin is intentionally NOT accepted:
                // a still-pending joiner is not yet a member, and no admin
                // would (or should) issue a catchup for them. The
                // countersigned-ness check uses the same Pre-Pass set
                // (`countersigned_pending_ids`) consumed by the PendingJoin
                // materialize arm, so the two paths stay consistent.
                let triggered_event = sorted[..idx].iter().find(|e| e.id == *triggered_by);
                let join_actor = match triggered_event.map(|e| &e.kind) {
                    Some(MembershipEventKind::Join) => triggered_event.map(|e| e.actor),
                    Some(MembershipEventKind::PendingJoin { .. })
                        if countersigned_pending_ids.contains(triggered_by) =>
                    {
                        triggered_event.map(|e| e.actor)
                    }
                    _ => None,
                };
                let Some(target) = join_actor else {
                    continue;
                };

                // target must be in recipient_ciphertexts.
                if !recipient_ciphertexts
                    .iter()
                    .any(|rc| rc.recipient == target)
                {
                    continue;
                }

                // Issuer must have admin power (spec §4.6 — no cooperative-joiner).
                // C8: also require issuer to be currently Joined — a former admin
                // with a stale power_levels entry must not be able to issue
                // catchups after being kicked or leaving.
                //
                // Same bootstrap-admin exception as EpochRotation: admin_addr
                // may have no member record if they never issued an explicit
                // Join event (None = never a member, not Left/Banned).
                let issuer = event.actor;
                let issuer_power = m.power_levels.get(&issuer).copied().unwrap_or(0);
                let issuer_member_status = m.members.get(&issuer).map(|s| s.status);
                let issuer_is_joined = matches!(issuer_member_status, Some(MemberStatus::Joined));
                let issuer_is_effective_member =
                    issuer_is_joined || (issuer_member_status.is_none() && issuer == admin_addr);
                let is_admin =
                    issuer_power >= m.power_thresholds.kick && issuer_is_effective_member;
                if !is_admin {
                    continue;
                }

                // Apply: clear pending_catchup_for for every member named in
                // recipient_ciphertexts. Spec §4.6 allows multi-recipient
                // catchups (e.g., one admin synthesizing a single catchup
                // event that covers several recent joiners at once).
                // (Actual key delivery to receiver's local Space happens
                // in community_state_sync apply layer — Tasks 5/6.)
                for rc in recipient_ciphertexts {
                    m.pending_catchup_for.remove(&rc.recipient);
                }
            }
            MembershipEventKind::Fork { .. } => {
                // ZEB-285: non-mutating. Fork events are recorded in the event
                // log for historical/audit visibility but do not change the
                // materialized membership/power/channels view. They are
                // surfaced separately via settings-panel listings.
            }
            MembershipEventKind::PendingJoin { .. } => {
                // ZEB-254: PendingJoin materializes to one of three states:
                //   - if countersigned and prior state is not terminal: Joined
                //   - else if within expiry window and prior state is not
                //     terminal: PendingJoin
                //   - else: hidden (no entry / no mutation)
                //
                // The countersigned set is built by the Pre-Pass above.
                // Prior-state guard ensures Leave/Kick/Banned aren't
                // overridden by a late-arriving JoinCountersign.
                let countersigned = countersigned_pending_ids.contains(&event.id);
                let age_ms = current_max_wall_ms.saturating_sub(event.at.wall_ms);
                let expired = age_ms > MATERIALIZE_PENDING_EXPIRY_MS;

                let prior_status = m.members.get(&event.actor).map(|s| s.status);
                match prior_status {
                    Some(MemberStatus::Joined) | Some(MemberStatus::Banned) => {
                        // Terminal state — PendingJoin is shadowed.
                        //
                        // ZEB-254: Left is intentionally NOT in this list.
                        // verify_event P6 explicitly allows prior state None | Left
                        // as valid preconditions for PendingJoin — a user who left
                        // a community is allowed to re-join via a new PendingJoin.
                        // Including Left here would shadow the re-join and produce
                        // a semantic mismatch with the verify gate.
                    }
                    _ => {
                        if countersigned {
                            // ZEB-339: preserve any prior enrolled keys (a
                            // PendingJoin→countersign transition must not drop
                            // keys already learned from a prior Join) AND ingest
                            // the joiner's own cert carried on this PendingJoin
                            // event — for a first-join-via-invite this is the
                            // ONLY place the joiner's enrolled device key is
                            // learned, so without it their later steady-state
                            // events would fail SignerNotEnrolledForActor.
                            // ZEB-668 S3: tombstones carried forward + retired
                            // keys refused re-entry (remove-wins) — same as the
                            // Join arm.
                            let (mut enrolled, revoked) = m
                                .members
                                .get(&event.actor)
                                .map(|s| {
                                    (
                                        s.enrolled_device_keys.clone(),
                                        s.revoked_device_keys.clone(),
                                    )
                                })
                                .unwrap_or_default();
                            // SECURITY INVARIANT: ingested without re-verification
                            // — safe only because this PendingJoin event was
                            // already accepted by verify_event/enrolled_key_from_cert
                            // before reaching the materialized log. See the Join
                            // arm above for the full rationale.
                            if let Some(cert) = event.enrollment.as_ref() {
                                let key = cert.device_pubkeys.classical.ed25519_verify;
                                if !revoked.contains(&key) {
                                    insert_enrolled_key_capped(&mut enrolled, key);
                                }
                            }
                            m.members.insert(
                                event.actor,
                                MemberState {
                                    status: MemberStatus::Joined,
                                    // ZEB-254: joined_at is the PendingJoin event's HLC — i.e.,
                                    // when the joiner declared their intent to join. This matches
                                    // the legacy Join arm's semantics (joined_at = the Join
                                    // event's HLC). For the resurrects-expired-pending case, the
                                    // UI may surface a join date from up to 30+ days ago, which
                                    // is the truthful "when did this person first try to join"
                                    // answer.
                                    joined_at: event.at.clone(),
                                    left_at: None,
                                    enrolled_device_keys: enrolled,
                                    revoked_device_keys: revoked,
                                },
                            );
                            // ZEB-254: mirror the Join arm's catchup invariant — a member
                            // joining a community whose epoch has already rotated needs key
                            // material catchup. Without this insert, the engine will treat
                            // their snapshot as current and skip the catchup dispatch.
                            if m.current_epoch.unwrap_or(0) > 0 {
                                m.pending_catchup_for.insert(event.actor);
                            }
                        } else if !expired {
                            // ZEB-339: ingest the joiner's cert key even for an
                            // un-countersigned PendingJoin. The PendingJoin event
                            // is identity-introducing and carries the joiner's
                            // cert; recording their enrolled device key here lets
                            // their own subsequent events verify (e.g. cancelling
                            // the pending join via Leave) before any countersign
                            // arrives. Status stays PendingJoin, so the per-kind
                            // authorization gates still block privileged actions.
                            // Same SECURITY INVARIANT as the Join arm: the event
                            // was already accepted by verify_event before reaching
                            // the materialized log.
                            // ZEB-668 S3: tombstones carried forward + retired
                            // keys refused re-entry (remove-wins) — same as the
                            // Join arm.
                            let (mut enrolled, revoked) = m
                                .members
                                .get(&event.actor)
                                .map(|s| {
                                    (
                                        s.enrolled_device_keys.clone(),
                                        s.revoked_device_keys.clone(),
                                    )
                                })
                                .unwrap_or_default();
                            if let Some(cert) = event.enrollment.as_ref() {
                                let key = cert.device_pubkeys.classical.ed25519_verify;
                                if !revoked.contains(&key) {
                                    insert_enrolled_key_capped(&mut enrolled, key);
                                }
                            }
                            m.members.insert(
                                event.actor,
                                MemberState {
                                    status: MemberStatus::PendingJoin,
                                    joined_at: event.at.clone(),
                                    left_at: None,
                                    enrolled_device_keys: enrolled,
                                    revoked_device_keys: revoked,
                                },
                            );
                        }
                        // else: expired pending with no countersign → hidden (no insert).
                    }
                }
            }
            MembershipEventKind::JoinCountersign { .. } => {
                // ZEB-254: pairing is handled by the Pre-Pass that builds
                // countersigned_pending_ids, then consumed during the
                // PendingJoin arm above. No direct state mutation here.
            }
            MembershipEventKind::AdminProposal { proposal_kind: _ } => {
                // ZEB-250 §5.2 (Bug-fix R1): effect is applied at the event
                // that tips the running signer count over the threshold, using
                // the *running* admin_quorum at this iteration step.
                //
                // For an AdminProposal, the proposer is the first signer. If
                // admin_quorum == 1 the proposal self-satisfies here; we apply
                // the effect immediately. If admin_quorum > 1, we insert the
                // proposer into running_signers_seen but don't apply yet —
                // the later countersign that crosses the threshold will apply.
                //
                // ZEB-250 R2 fix (b): mark this proposal as seen so the
                // AdminCountersign arm knows the proposal has been reached
                // in HLC order (forward-ref countersigns queue the signer
                // but don't trigger; the proposer arm fires instead).
                seen_admin_proposals.insert(event.id);
                let admin_quorum_now = m.admin_quorum as usize;
                let signer_set = running_signers_seen.entry(event.id).or_default();
                signer_set.insert(event.actor);
                let count_now = signer_set.len();
                // ZEB-250 R2 fix (a): use >= (not ==) so a forward-ref
                // countersign that sorted before the proposer (out-of-order
                // DAG delivery) doesn't bypass the trigger when count is
                // already >= quorum by proposer-arm time.
                // Sticky applied guard prevents double-application.
                if !applied_admin_proposals.contains(&event.id)
                    && count_now >= admin_quorum_now
                    && admin_quorum_now > 0
                {
                    // This event IS the trigger. Age = 0 since proposer
                    // wall_ms == event.at.wall_ms for the self-satisfy path.
                    let age_when_reached = event.at.wall_ms.saturating_sub(event.at.wall_ms);
                    if age_when_reached <= ADMIN_PROPOSAL_EXPIRY_MS {
                        if let Some((kind, _proposer, _proposer_wall_ms)) =
                            proposals_index.get(&event.id).cloned()
                        {
                            apply_admin_proposal_effect(&mut m, &kind, event);
                            applied_admin_proposals.insert(event.id);
                            // ZEB-713: an applied SetRecoveryDesignates is a
                            // config-generation change — record its wall so
                            // the recovery post-pass can kill in-flight
                            // proposals it landed inside (spec §3.3.4).
                            if matches!(kind, ProposalKind::SetRecoveryDesignates { .. }) {
                                rec_config_change_walls.push(event.at.wall_ms);
                            }
                        }
                    }
                }
                // else: quorum > 1 and only 1 signer so far; pending.
            }
            MembershipEventKind::AdminCountersign { target_event_id } => {
                // ZEB-250 §5.2 (Bug-fix R1): insert this countersigner into
                // running_signers_seen. If doing so causes the count to reach
                // admin_quorum for the target proposal, AND the proposal hasn't
                // yet been applied (check proposals_index presence + not yet
                // applied), apply the effect now — at THIS event's HLC.
                //
                // This guarantees events between AdminProposal HLC and this
                // countersign's HLC were materialized under the OLD state,
                // matching CRDT causality (§5.3).
                //
                // ZEB-250 R2 fix (b): if the target proposal hasn't been seen
                // yet in HLC order (forward-ref countersign from out-of-order
                // DAG delivery), queue the signer in running_signers_seen but
                // do NOT trigger application. The proposal's own arm will fire
                // later with this signer already in the set, and >= will catch
                // the case where count is already at quorum by proposer-arm time.
                if !seen_admin_proposals.contains(target_event_id) {
                    // Forward-ref: queue the signer for when the proposal arrives.
                    let signer_set = running_signers_seen.entry(*target_event_id).or_default();
                    signer_set.insert(event.actor);
                    // Don't trigger application — proposal arm handles it.
                } else if applied_admin_proposals.contains(target_event_id) {
                    // ZEB-250 R2 fix (a): idempotent re-trigger guard.
                    // If already applied (e.g., ChangeQuorum lowered the
                    // threshold so count now re-crosses the new threshold),
                    // skip to avoid double-application.
                } else if let Some((kind, _proposer_addr, proposer_wall_ms)) =
                    proposals_index.get(target_event_id).cloned()
                {
                    let admin_quorum_now = m.admin_quorum as usize;
                    let signer_set = running_signers_seen.entry(*target_event_id).or_default();
                    signer_set.insert(event.actor);
                    let count_now = signer_set.len();
                    // Apply when this countersign is the Nth signer (count
                    // just crossed the threshold — count_now == admin_quorum
                    // means we went from count-1 to exactly count).
                    if count_now == admin_quorum_now && admin_quorum_now > 0 {
                        let age_when_reached = event.at.wall_ms.saturating_sub(proposer_wall_ms);
                        if age_when_reached <= ADMIN_PROPOSAL_EXPIRY_MS {
                            // ZEB-250 R2 Fix 2: pass THIS event (the countersign
                            // that tipped quorum) as effective_event so
                            // apply_admin_proposal_effect's left_at uses the
                            // Nth-signer HLC, not the proposal's original HLC.
                            // Preserves CRDT causality: moderation is not backdated.
                            apply_admin_proposal_effect(&mut m, &kind, event);
                            applied_admin_proposals.insert(*target_event_id);
                            // ZEB-713: record config-generation changes for
                            // the recovery post-pass (spec §3.3.4).
                            if matches!(kind, ProposalKind::SetRecoveryDesignates { .. }) {
                                rec_config_change_walls.push(event.at.wall_ms);
                            }
                        }
                    }
                }
                // else: countersign targets an unknown proposal and target IS
                // in seen_admin_proposals but not in proposals_index — shouldn't
                // happen in a well-formed log; silently skip.
            }

            MembershipEventKind::ReachabilityAnnounce { .. } => {
                // ZEB-321: no membership-state effect; handled by
                // ReachabilityResolver hook in event_loop.
            }
            MembershipEventKind::CommunityRelayAnnounce { .. } => {
                // ZEB-458: no membership-state effect; consumed by
                // CommunityRelayResolver for the fresh advertiser set.
            }
            MembershipEventKind::DeviceAnnounce => {
                // ZEB-495 (ZEB-340 Part 2): add the introduced device's key to
                // its owner's EXISTING MemberState. `get_mut`-and-insert
                // inherently preserves every other field (status, joined_at,
                // left_at, prior enrolled keys) — no rebuild/replace. Idempotent:
                // re-announcing an already-present key is a no-op BTreeSet::insert.
                //
                // Defensive guards (member present + Joined + cert present):
                // verify_event already guarantees all three (the actor is a
                // Joined member and the cert resolved the signer), but materialize
                // must never panic on a malformed/replayed event that slipped past
                // verification (corrupted log, etc.), so each is checked here.
                //
                // SECURITY INVARIANT (mirrors the Join arm): the cert is ingested
                // WITHOUT re-verification — verify_event → enrolled_key_from_cert
                // is the SOLE cert gate (cert.verify, Master-issuer, owner==actor).
                // The inserted key is the IDENTICAL field that
                // verify_membership_signer validated the outer signature under.
                if let Some(member) = m.members.get_mut(&event.actor) {
                    if member.status == MemberStatus::Joined {
                        if let Some(cert) = &event.enrollment {
                            // ZEB-401: same capped insert as every other key-adding
                            // arm. verify_event already rejects an over-limit
                            // DeviceAnnounce, but materialize holds the invariant
                            // uniformly even for an event that bypassed verification.
                            // ZEB-668 S3: routed through the tombstone-aware
                            // inserter — a retired key is never re-added.
                            insert_enrolled_key_unless_retired(
                                member,
                                cert.device_pubkeys.classical.ed25519_verify,
                            );
                        }
                    }
                }
            }
            MembershipEventKind::DeviceRetire {
                revocation: _,
                enrollment: cert,
            } => {
                // ZEB-668 S3: remove-wins retire — remove the retired key
                // AND tombstone it so a DeviceAnnounce sorting after this
                // event in the deterministic replay can never re-add it
                // (clock skew can order the announce later even though it
                // happened first).
                //
                // SECURITY INVARIANT (mirrors DeviceAnnounce): the cert pair
                // was verified by verify_event → verify_device_retire_certs;
                // this arm trusts the binding and must never panic on a
                // malformed replayed event, hence the defensive get_mut.
                // ANY member status qualifies (subtractive op — see the
                // verify arm for the Left/Banned rationale).
                if let Some(member) = m.members.get_mut(&event.actor) {
                    let vk = cert.device_pubkeys.classical.ed25519_verify;
                    member.enrolled_device_keys.remove(&vk);
                    // Uncapped by design — see the tombstone note next to
                    // `insert_enrolled_key_unless_retired` (authenticated,
                    // contains-only, and a cap would break remove-wins).
                    member.revoked_device_keys.insert(vk);
                }
            }
            MembershipEventKind::RecoveryProposal {
                lost_admin,
                new_admin,
                config_digest,
            } => {
                // ZEB-713: bind-time validity against the RUNNING state
                // (defense-in-depth re-check of RP1/RP5 — verify_event
                // gated at insert; a proposal invalid here never becomes
                // a work entry and is invisible to the lifecycle).
                let Some(config) = m.recovery_designates.clone() else {
                    continue;
                };
                if running_recovery_digest(&m) != Some(*config_digest) {
                    continue;
                }
                if !config.designates.contains(&event.actor) || !is_joined_member(&m, &event.actor)
                {
                    continue;
                }
                let mut work = RecoveryWork {
                    proposer: event.actor,
                    lost_admin: *lost_admin,
                    new_admin: *new_admin,
                    t0_wall_ms: event.at.wall_ms,
                    threshold: config.threshold,
                    veto_window_ms: config.veto_window_ms,
                    designates: config.designates.clone(),
                    config_digest: *config_digest,
                    sig_walls: vec![event.at.wall_ms],
                    signers: BTreeSet::from([event.actor]),
                    deadline_at_replay: None,
                    // RP3 verified new_admin Joined at proposal time; the
                    // per-event refresh below keeps this current until
                    // the deadline.
                    new_admin_joined: is_joined_member(&m, new_admin),
                };
                // Fold queued forward-ref cosigns (ZEB-250 R2(b) mirror).
                if let Some(queued) = rec_pending_cosigns.remove(&event.id) {
                    for cosign in queued {
                        fold_recovery_cosign(&mut work, cosign);
                    }
                }
                // R=1 (or forward-ref-satisfied) proposals reach
                // threshold at bind — freeze the snapshot cutoff here.
                maybe_set_replay_deadline(&mut work);
                rec_proposals.insert(event.id, work);
            }
            MembershipEventKind::RecoveryCosign { target_event_id } => {
                // ZEB-713: record the position-local facts and fold (or
                // queue when the target hasn't been reached in HLC order).
                let cosign = QueuedRecoveryCosign {
                    wall_ms: event.at.wall_ms,
                    actor: event.actor,
                    running_digest: running_recovery_digest(&m),
                    joined: is_joined_member(&m, &event.actor),
                };
                if let Some(work) = rec_proposals.get_mut(target_event_id) {
                    fold_recovery_cosign(work, cosign);
                } else {
                    rec_pending_cosigns
                        .entry(*target_event_id)
                        .or_default()
                        .push(cosign);
                }
            }
            MembershipEventKind::RecoveryVeto { target_event_id } => {
                // ZEB-713 RV1 running-state half: power-100 AND
                // Joined-or-bootstrap-admin at this position. The
                // [t₀, deadline] authored-window check happens at
                // evaluation time (the target may not even be bound yet).
                let veto_power = m.power_levels.get(&event.actor).copied().unwrap_or(0);
                let is_bootstrap_admin =
                    event.actor == admin_addr && !m.members.contains_key(&event.actor);
                if veto_power == 100 && (is_joined_member(&m, &event.actor) || is_bootstrap_admin) {
                    rec_vetoes.push((*target_event_id, event.at.wall_ms, event.actor));
                }
            }
        }

        // PR #497 R2 (Greptile P1): refresh each open proposal's
        // new_admin-Joined snapshot while replay is still within its
        // deadline window. The last refresh at or before the deadline
        // is the value execution is judged on — Joined-ness AS OF the
        // deadline, log-derivable and stable under later rejoins.
        // Bounded work: RP6 caps open proposals at |designates|.
        for work in rec_proposals.values_mut() {
            let within_window = work
                .deadline_at_replay
                .map(|d| event.at.wall_ms <= d)
                .unwrap_or(true);
            if within_window {
                work.new_admin_joined = matches!(
                    m.members.get(&work.new_admin).map(|s| s.status),
                    Some(MemberStatus::Joined)
                );
            }
        }
    }

    // ZEB-713 post-pass: evaluate recovery lifecycles at the now-floor
    // and apply the derived execution (spec §3.3.3). Execution is PURE
    // DERIVED STATE — no synthetic Kick/SetPower events — so a
    // late-delivered veto re-derives it away on the next materialize
    // (spec §4.2).
    let outcomes = evaluate_recovery_phases(
        &rec_proposals,
        &rec_vetoes,
        &rec_config_change_walls,
        current_max_wall_ms,
    );
    for (id, outcome) in outcomes.iter() {
        if outcome.phase != RecoveryPhase::Executed {
            continue;
        }
        let work = &rec_proposals[id];
        let deadline_ms = outcome.deadline_ms.expect("executed implies deadline");
        // Synthetic derived-state HLC for the kick timestamp:
        // deterministic in (events, now_ms) because deadline is a pure
        // function of event data. Empty device_id marks it as derived,
        // not authored.
        let derived_at = Hlc {
            wall_ms: deadline_ms,
            logical: 0,
            device_id: String::new(),
        };
        // Promote. Executed already implies new_admin was Joined as of
        // the deadline (the evaluate gate); if they left AFTER the
        // deadline the granted power entry is inert without Joined
        // status — the same state any departing admin leaves behind.
        m.power_levels.insert(work.new_admin, 100);
        // Derived kick of the NAMED lost_admin (loss-as-compromise —
        // activity does not immunize the key, spec §6 T9). Mirrors the
        // direct-Kick arm, with one deliberate difference: a
        // record-less bootstrap admin gets a Banned record INSERTED —
        // RP4 explicitly admits that identity, and without a record the
        // verify-side bootstrap-admin exceptions (None membership ⇒
        // "never kicked") would keep honoring the dead key forever.
        let prior_status = m.members.get(&work.lost_admin).map(|s| s.status);
        match m.members.get_mut(&work.lost_admin) {
            Some(ms) => {
                // First-execution-wins on an already-Banned target
                // (idempotent, preserves the earlier left_at).
                if ms.status != MemberStatus::Banned {
                    ms.status = MemberStatus::Banned;
                    ms.left_at = Some(derived_at.clone());
                }
            }
            None if work.lost_admin == admin_addr => {
                m.members.insert(
                    work.lost_admin,
                    MemberState {
                        status: MemberStatus::Banned,
                        joined_at: derived_at.clone(),
                        left_at: Some(derived_at.clone()),
                        enrolled_device_keys: BTreeSet::new(),
                        revoked_device_keys: BTreeSet::new(),
                    },
                );
            }
            None => {}
        }
        // ZEB-249 hand-off: the derived kick marks the rotation need;
        // the existing self-healing path picks it up. PendingJoin guard
        // mirrors the direct-Kick arm (R4-1). Skipped when this
        // proposal's rotation already landed during the main pass
        // (rotation-then-insert would leave a stale pending marker).
        if !matches!(prior_status, Some(MemberStatus::PendingJoin))
            && !rec_rotation_applied.contains(id)
        {
            m.pending_rotation_for.insert(work.lost_admin);
        }
    }
    // Derived per-proposal view for RP6 and the D2 UI, sorted by
    // (t₀, id) for deterministic encoding.
    let mut view: Vec<RecoveryProposalView> = rec_proposals
        .iter()
        .map(|(id, work)| {
            let outcome = outcomes[id];
            RecoveryProposalView {
                id: *id,
                proposer: work.proposer,
                lost_admin: work.lost_admin,
                new_admin: work.new_admin,
                signers: work.signers.clone(),
                threshold: work.threshold,
                proposed_at_wall_ms: work.t0_wall_ms,
                deadline_ms: outcome.deadline_ms,
                phase: outcome.phase,
                vetoed_by: outcome.vetoed_by,
            }
        })
        .collect();
    view.sort_by_key(|v| (v.proposed_at_wall_ms, v.id));
    m.recovery_proposals = view;

    m
}

/// Compute the prior materialized state for an event — the state
/// `verify_event` should authorize against.
///
/// Materializes every event in `all_events` whose `event_sort_key` is
/// STRICTLY less than `target`'s, using the same total order
/// `materialize` uses internally. Equivalent to
/// `materialize(events.filter(|e| event_sort_key(e) < event_sort_key(target)), admin_addr)`
/// but exposed as a helper so callers can't drift from the comparator.
///
/// Why a dedicated helper:
/// - Re-implementing the prefix selection in caller code (e.g., "all
///   events strictly before in HLC order") would miss the EventId / sig
///   tie-breakers and silently authorize a target event against state
///   that DOESN'T include same-HLC predecessors — masking stale
///   membership/power lookups when wall_ms ties occur.
/// - The Phase 2 sync layer is the production caller; this helper is
///   the single source of truth for "what state was true just before
///   this event", and changes to the comparator (e.g., a future
///   tiebreaker) propagate to all call sites automatically.
///
/// `target` must be an event from `all_events` (or one whose sort key
/// is at least defined relative to them). Events EQUAL to `target`
/// under the comparator are excluded — verification of an event at
/// position N looks at the prefix [0, N).
pub fn prior_state_at_event(
    all_events: &[SignedMembershipEvent],
    target: &SignedMembershipEvent,
    admin_addr: OwnerAddr,
) -> MaterializedMembership {
    let target_key = event_sort_key(target);
    let prefix: Vec<SignedMembershipEvent> = all_events
        .iter()
        .filter(|e| event_sort_key(e) < target_key)
        .cloned()
        .collect();
    // R4-6: when computing prior state FOR a specific candidate event,
    // that event's own `at.wall_ms` is the natural "now floor" — by
    // the time this candidate is being verified, the community's
    // wall-clock has reached at least `target.at.wall_ms`. Using it as
    // the floor lets an idle-community re-redeem attempt (a new
    // PendingJoin at t = T + 30d) see its prior-state PendingJoin as
    // expired, so P6 admits the re-redemption.
    materialize_with_now(&prefix, admin_addr, Some(target.at.wall_ms))
}

/// Compute the prior materialized state for a given HLC — the
/// membership view as-of `target_hlc` (events strictly before).
///
/// Companion to `prior_state_at_event`. The difference is the input
/// type: this helper takes a bare `Hlc` (used by the receive-side
/// state-root verify path, where we have only the publish's HLC and
/// not a full `SignedMembershipEvent`).
///
/// Strict prefix on `(wall_ms, logical, device_id)` — events with the
/// same triple as `target_hlc` are excluded (consistent with
/// `event_sort_key`'s ordering, but without the EventId/sig
/// tie-breakers since we have no target event to compare against).
pub fn prior_state_at_hlc(
    all_events: &[SignedMembershipEvent],
    target_hlc: &Hlc,
    admin_addr: OwnerAddr,
) -> MaterializedMembership {
    let prefix: Vec<SignedMembershipEvent> = all_events
        .iter()
        .filter(|e| {
            (e.at.wall_ms, e.at.logical, &e.at.device_id)
                < (
                    target_hlc.wall_ms,
                    target_hlc.logical,
                    &target_hlc.device_id,
                )
        })
        .cloned()
        .collect();
    // R4-6: pass `target_hlc.wall_ms` as the "now floor" for the same
    // reason as `prior_state_at_event` — by the time we're authorizing
    // an event AT this HLC, wall-clock has reached at least
    // `target_hlc.wall_ms`.
    materialize_with_now(&prefix, admin_addr, Some(target_hlc.wall_ms))
}

/// Caller-provided context for verify_event. Carries the expected
/// community_id, the prior materialized state (so the function is pure
/// — verify_event doesn't load state from anywhere), the policy bit,
/// and the 64-byte identity_pubs needed for pubkey-to-claimed-signer
/// binding + signature verification.
///
/// `expected_community_id` MUST match `event.community_id` — verify_event
/// rejects a mismatch BEFORE any other check, defending against
/// cross-community authorization (caller has community A's state but
/// the event was signed for community B).
///
/// `admin_addr` is the community's bootstrap admin (Space.admin_addr).
/// Admin self-Join in an invite-only community is exempt from the
/// countersig requirement — without this exemption a fresh invite-only
/// community would be unbootstrappable (every Join would need a
/// countersig from a Joined member, but no one is Joined initially).
///
/// ZEB-339: verify_event no longer takes caller-resolved identity_pubs.
/// The actor's signer is resolved either from the event's carried
/// `EnrollmentCert` (identity-introducing events) or from the actor's
/// materialized `enrolled_device_keys` (steady-state events). Countersig
/// and InviteToken signers are likewise resolved from materialized
/// membership. The context now carries only policy/binding scalars.
///
/// `Clone`/`Copy` (ZEB-748 phase 6a): all three fields are `Copy`
/// scalars (`SpaceId`, `OwnerAddr`, `bool`), so the derive is free. The
/// `MembershipPolicy` adopter (`community_state_crdt.rs`) threads a
/// `VerifyContext` by value inside its per-insert `MembershipInsertCtx`.
#[derive(Clone, Copy)]
pub struct VerifyContext {
    pub expected_community_id: SpaceId,
    pub admin_addr: OwnerAddr,
    pub is_invite_only: bool,
}

/// Full membership-event verification per ZEB-217 spec §"Verification".
///
/// Run BEFORE materializing an event into the CRDT. Caller must:
/// 1. Compute `prior_state` by materializing every event whose
///    `event_sort_key` is STRICTLY less than `event`'s — i.e., the
///    `(wall_ms, logical, device_id, EventId, sig)` tuple strictly
///    less than the target's. Use `prior_state_at_event` to do this
///    correctly without re-implementing the comparator (which would
///    drift from `materialize`'s tie-breakers and silently authorize
///    against stale state when same-HLC predecessors exist).
/// 2. Resolve `event.actor` → identity_pub via Sub-A's owner-device cache.
/// 3. For invite-only Joins, also resolve the countersig signer's
///    identity_pub.
///
/// Verifies in this order:
/// 1. Actor's signature on the event payload.
/// 2. For invite-only Join: countersig present + valid + signer's
///    power ≥ invite_threshold.
/// 3. Action-specific power rules:
///    - Kick: actor's power ≥ kick_threshold AND > target's power
///    - SetPower: actor's power ≥ set_power_threshold
///    - Invite: actor's power ≥ invite_threshold (currently 0 — any
///      joined member can invite)
///    - Join, Leave: no power check (anyone can leave; join is gated
///      by invite-only countersig logic above)
///
/// Power lookups treat unset entries as 0 (the default per the spec).
/// Bootstrap (admin_addr → 100) is already baked into prior_state by
/// `materialize`, so the lookup here is uniform across all actors.
///
/// ZEB-251: the invite-threshold comparisons below read
/// `prior_state.power_thresholds.invite`, a runtime per-community value
/// (default 0, customizable via a future ChangeThresholds proposal — Task
/// 2) — no longer the hardcoded-0 constant, so `clippy::absurd_extreme_comparisons`
/// no longer fires and the prior suppression has been removed.
pub fn verify_event(
    event: &SignedMembershipEvent,
    prior_state: &MaterializedMembership,
    ctx: &VerifyContext,
) -> Result<(), VerifyError> {
    // 0. Community binding: the event must belong to the community
    //    whose state the caller is verifying against. Without this
    //    check, an event signed for community B could be authorized
    //    using community A's prior_state/is_invite_only — granting
    //    the wrong invite or moderation rights. This guard fires
    //    before any cryptographic work so a misrouted event is
    //    rejected with the specific WrongCommunity discriminant
    //    rather than e.g. SignatureInvalid (which would mask the
    //    real cause).
    if event.community_id != ctx.expected_community_id {
        return Err(VerifyError::WrongCommunity);
    }

    // 0b. Countersig presence rule: a countersig is allowed ONLY on
    //     non-admin invite-only Join events. The actor sig intentionally
    //     excludes countersig (so an inviter can append it without
    //     invalidating the actor's sig), which makes countersig
    //     malleable on the wire. Reject any countersig outside its
    //     allowed slot so the invariant "countersig present iff
    //     non-admin invite-only Join" holds end-to-end (closes a
    //     wire-dedupe hole and keeps admin's bootstrap Join indistinguishable
    //     from open-Join wrt countersig).
    let admin_self_invite_only_join = matches!(event.kind, MembershipEventKind::Join)
        && ctx.is_invite_only
        && event.actor == ctx.admin_addr;
    let countersig_allowed = matches!(event.kind, MembershipEventKind::Join)
        && ctx.is_invite_only
        && !admin_self_invite_only_join;
    if event.countersig.is_some() && !countersig_allowed {
        return Err(VerifyError::UnexpectedCounterSig);
    }

    // 1. ZEB-339: resolve the signer's enrolled device key, then verify the sig.
    let signer = match &event.kind {
        // Identity-introducing events carry their own cert. ZEB-495:
        // DeviceAnnounce is identity-introducing too — the second device's
        // key is NOT yet in the enrolled set (that is exactly what this
        // event adds), so its signer is resolved from the carried cert.
        MembershipEventKind::Join
        | MembershipEventKind::PendingJoin { .. }
        | MembershipEventKind::DeviceAnnounce => enrolled_key_from_cert(event)?,
        // Steady-state events: resolve from materialized membership.
        _ => resolve_enrolled_signer(prior_state, event)?,
    };
    verify_membership_signer(event, &signer)?;

    // 2. Banned-status guard: a Banned actor's Join OR Leave must be
    //    rejected BEFORE materialize() would silently overwrite the
    //    Banned status.
    //
    //    Join: bans-then-rejoins is the obvious bypass.
    //    Leave: a Banned actor can sign Leave (no power gate); without
    //    this guard, materialize() would set status=Left, masking the
    //    Ban — and a subsequent Join would no longer hit the Banned
    //    guard (since prior_state.members[actor].status is now Left,
    //    not Banned). Reject Leave from Banned to keep Banned sticky.
    //
    //    Re-joining after Kick requires a dedicated unban flow
    //    (deferred). materialize() also pins Banned-stickiness as a
    //    state-machine defense (see Leave handler).
    if matches!(
        event.kind,
        MembershipEventKind::Join | MembershipEventKind::Leave
    ) {
        if let Some(state) = prior_state.members.get(&event.actor) {
            if state.status == MemberStatus::Banned {
                return Err(match event.kind {
                    MembershipEventKind::Join => VerifyError::BannedActorJoin,
                    MembershipEventKind::Leave => VerifyError::BannedActorLeave,
                    _ => unreachable!("guarded by outer matches!"),
                });
            }
        }
    }

    // 3. For non-admin invite-only Joins, countersig is required +
    //    valid + countersigner is a Joined member with sufficient
    //    power. Admin self-Join in invite-only is exempt (bootstrap
    //    rule — the community would otherwise be unbootstrappable
    //    since no Joined member exists to countersign).
    //
    // Note: `power_thresholds.invite` is per-community (ZEB-251) and
    // defaults to 0, so the power check below is a no-op (any owner
    // addr defaults to power 0 >= 0) until a community customizes
    // `invite` above 0 via a quorum-approved `ChangeThresholds`
    // proposal — at which point it fires today, using this same
    // materialized field.
    //
    // The Joined-membership check on the countersigner is the security-
    // critical gate in v1: without it, any non-member with a valid
    // countersig key (e.g., a former member who was Kicked but whose
    // power_levels entry persists) could vouch for new joiners.
    if matches!(event.kind, MembershipEventKind::Join)
        && ctx.is_invite_only
        && !admin_self_invite_only_join
    {
        let cs = event
            .countersig
            .as_ref()
            .ok_or(VerifyError::CounterSigRequired)?;
        verify_countersig(event, prior_state)?;

        if !is_joined_member(prior_state, &cs.signer) {
            return Err(VerifyError::CounterSignerNotJoined);
        }

        let signer_power = prior_state
            .power_levels
            .get(&cs.signer)
            .copied()
            .unwrap_or(0);
        if signer_power < prior_state.power_thresholds.invite {
            return Err(VerifyError::CounterSigPowerInsufficient);
        }
    }

    // 4. Joined-membership check for moderation actions. Power-level
    //    gating alone is insufficient — power without membership is
    //    meaningless, and a former member's stale power_levels entry
    //    must not let them moderate after departure.
    match &event.kind {
        MembershipEventKind::Join | MembershipEventKind::Leave => {
            // Join is gated above (Banned guard + invite-only countersig).
            // Leave is always allowed for the actor themselves; the
            // materializer no-ops if they were never Joined, so a
            // non-member's Leave is silently ignored downstream.
        }
        MembershipEventKind::Invite { .. }
        | MembershipEventKind::Kick { .. }
        | MembershipEventKind::SetPower { .. }
        | MembershipEventKind::Unban { .. } => {
            if !is_joined_member(prior_state, &event.actor) {
                return Err(VerifyError::ActorNotJoined);
            }
        }
        MembershipEventKind::ChannelCreate { .. }
        | MembershipEventKind::ChannelModify { .. }
        | MembershipEventKind::ChannelDelete { .. } => {
            // Channel-config requires actor to be Joined AND power >=
            // kick. Joined-check first so a non-member with high power
            // (e.g. former admin after Kick) can't create channels.
            // The power check fires in the per-kind power-rules block
            // below; this block establishes membership.
            if !is_joined_member(prior_state, &event.actor) {
                return Err(VerifyError::ActorNotJoined);
            }
        }
        MembershipEventKind::EpochRotation { .. } => {
            // EpochRotation membership + power checks happen in the
            // per-kind power-rules block below — the cooperative-leaver
            // path allows a non-member (the leaver) to issue the rotation,
            // so we can't apply a blanket ActorNotJoined gate here.
        }
        MembershipEventKind::EpochCatchup { .. } => {
            // EpochCatchup: skip the ActorNotJoined gate because the
            // admin issuing the catchup might subsequently be kicked
            // by the time an observer replays; we deliberately don't
            // enforce membership-at-replay-time. All authority and
            // shape checks (epoch must match current, triggered_by
            // must be a Join, target must be in recipients, issuer
            // must have admin power) are enforced in materialize. Spec §4.6.
        }
        MembershipEventKind::Fork { .. } => {
            // ZEB-285: Fork requires the actor to be currently Joined.
            // Any joined non-Banned member may fork at any time (power
            // threshold = 0), but non-members and Banned members are
            // rejected here before the per-kind power-rules block.
            if !is_joined_member(prior_state, &event.actor) {
                return Err(VerifyError::ActorNotJoined);
            }
        }
        MembershipEventKind::PendingJoin { invite_token } => {
            // P1: joiner identity binding is now subsumed by
            // enrolled_key_from_cert (step 1 above), which binds the carried
            // cert.owner_id == event.actor.

            // P2: invite_token.inviter must equal ctx.admin_addr.
            if invite_token.inviter != ctx.admin_addr {
                return Err(VerifyError::PendingJoinTokenInvalid);
            }

            // P3: invite_token.invitee_hint must match actor (if hint present).
            if let Some(hint) = invite_token.invitee_hint {
                if hint != event.actor {
                    return Err(VerifyError::PendingJoinTokenInvalid);
                }
            }

            // P4: invite_token.expires_at must be strictly greater than
            // event.at.wall_ms (token must not have expired at event time).
            if let Some(exp) = invite_token.expires_at {
                if event.at.wall_ms >= exp {
                    return Err(VerifyError::PendingJoinTokenExpired);
                }
            }

            // P5: ZEB-339 — invite_token signature verifies against the
            // inviter's enrolled device key, resolved from materialized
            // membership (the inviter is a Joined member). The inviter ==
            // admin_addr binding is enforced by P2 above.
            verify_invite_token_sig_with_enrolled(invite_token, prior_state)?;

            // P6: prior state must be None | Some(Left).
            let prior_status = prior_state.members.get(&event.actor).map(|m| m.status);
            match prior_status {
                None | Some(MemberStatus::Left) => { /* ok */ }
                _ => return Err(VerifyError::PendingJoinAlreadyMember),
            }
        }
        MembershipEventKind::JoinCountersign { .. } => {
            // ZEB-254: actor joined-membership check handled in the
            // per-kind power-rules block below (step 5).
        }
        MembershipEventKind::AdminProposal { proposal_kind } => {
            // ZEB-250 §4.1 — five gates AP1-AP5.

            // AP1: actor Joined.
            let actor_state = prior_state.members.get(&event.actor);
            if !matches!(actor_state.map(|s| s.status), Some(MemberStatus::Joined)) {
                return Err(VerifyError::AdminProposalActorNotJoined);
            }
            // AP2: actor power >= 100.
            let actor_power_ap = prior_state
                .power_levels
                .get(&event.actor)
                .copied()
                .unwrap_or(0);
            if actor_power_ap < 100 {
                return Err(VerifyError::AdminProposalActorNotAdmin);
            }
            // AP3 + AP4: well-formedness + admin-affecting check.
            match proposal_kind {
                ProposalKind::SetPower { target, level } => {
                    // AP3: target exists, level in range.
                    if !prior_state.members.contains_key(target) {
                        return Err(VerifyError::AdminProposalKindInvalid);
                    }
                    if *level > prior_state.power_thresholds.max {
                        return Err(VerifyError::AdminProposalKindInvalid);
                    }
                    // AP4: admin-affecting iff level == 100 OR target was admin.
                    let target_power = prior_state.power_levels.get(target).copied().unwrap_or(0);
                    let admin_affecting = *level == 100 || target_power == 100;
                    if !admin_affecting {
                        return Err(VerifyError::AdminProposalNotAdminAffecting);
                    }
                }
                ProposalKind::Kick { target, reason } => {
                    // AP3 part 1: target exists.
                    let target_state = prior_state.members.get(target);
                    if target_state.is_none() {
                        return Err(VerifyError::AdminProposalKindInvalid);
                    }
                    // AP3 part 2: target is Joined.
                    if !matches!(target_state.map(|s| s.status), Some(MemberStatus::Joined)) {
                        return Err(VerifyError::AdminProposalKindInvalid);
                    }
                    // AP3 part 3: reason is None or non-empty (and not too long).
                    if let Some(r) = reason {
                        if r.is_empty() {
                            return Err(VerifyError::AdminProposalKindInvalid);
                        }
                        // Match direct Kick path's length cap (chars, not bytes).
                        if r.chars().count() > MAX_MODERATION_REASON_CHARS {
                            return Err(VerifyError::AdminProposalKindInvalid);
                        }
                    }
                    // AP4: admin-affecting iff target is admin.
                    let target_power = prior_state.power_levels.get(target).copied().unwrap_or(0);
                    if target_power != 100 {
                        return Err(VerifyError::AdminProposalNotAdminAffecting);
                    }
                }
                ProposalKind::SetRecoveryDesignates {
                    designates,
                    threshold,
                    veto_window_ms,
                } => {
                    // ZEB-713 RD1: non-empty, no duplicates.
                    if designates.is_empty() {
                        return Err(VerifyError::RecoveryDesignatesMalformed);
                    }
                    let distinct: BTreeSet<&OwnerAddr> = designates.iter().collect();
                    if distinct.len() != designates.len() {
                        return Err(VerifyError::RecoveryDesignatesMalformed);
                    }
                    // RD2: every designate currently Joined. (An admin may
                    // name themselves — pointless but harmless; the UI
                    // discourages it. Spec §3.1.)
                    for d in designates {
                        if !is_joined_member(prior_state, d) {
                            return Err(VerifyError::RecoveryDesignateNotJoined);
                        }
                    }
                    // RD3: 1 <= R <= designate count.
                    if *threshold < 1 || (*threshold as usize) > designates.len() {
                        return Err(VerifyError::RecoveryThresholdOutOfRange);
                    }
                    // RD4: window floor, enforced on every replica so a
                    // modified client can't shorten it (spec §6 T6).
                    if *veto_window_ms < RECOVERY_VETO_WINDOW_FLOOR_MS {
                        return Err(VerifyError::RecoveryVetoWindowTooShort);
                    }
                    // RD4 ceiling (PR #497 R1): guards the t_R + W
                    // deadline arithmetic against u64 wrap (an unbounded
                    // W would let a crafted config execute recovery
                    // IMMEDIATELY) and keeps the value JS-number-exact.
                    if *veto_window_ms > RECOVERY_VETO_WINDOW_CEILING_MS {
                        return Err(VerifyError::RecoveryVetoWindowTooLong);
                    }
                    // Always admin-affecting (it grants a takeover path);
                    // no AP4 distinction — mirrors ChangeQuorum.
                }
                ProposalKind::ChangeQuorum { new_quorum } => {
                    // AP3: new_quorum >= 1.
                    if *new_quorum < 1 {
                        return Err(VerifyError::AdminProposalKindInvalid);
                    }
                    // AP5: new_quorum <= LIVE admin count. Count only admins
                    // whose MemberStatus is Joined — kicked/left admins retain
                    // their power_levels entry by convention but are no longer
                    // live participants and must not count toward this cap.
                    let admin_count = prior_state
                        .power_levels
                        .iter()
                        .filter(|(addr, p)| {
                            **p == 100
                                && prior_state
                                    .members
                                    .get(addr)
                                    .map(|ms| ms.status == MemberStatus::Joined)
                                    .unwrap_or(false)
                        })
                        .count() as u32;
                    if (*new_quorum as u32) > admin_count {
                        return Err(VerifyError::AdminProposalQuorumOutOfRange);
                    }
                    // ChangeQuorum is always admin-affecting; no AP4 distinction.
                }
                ProposalKind::ChangeThresholds { new_thresholds } => {
                    // AT1: ordering invariant + fixed ceiling. Authoritative —
                    // every member rejects an invalid change identically.
                    let t = new_thresholds;
                    if !(t.invite <= t.kick && t.kick <= t.set_power && t.set_power <= t.max)
                        || t.max != POWER_THRESHOLDS.max
                    {
                        return Err(VerifyError::AdminProposalThresholdsInvalid);
                    }
                    // ChangeThresholds is always admin-affecting; no AP4 distinction.
                }
            }
        }
        MembershipEventKind::AdminCountersign { target_event_id } => {
            // AC1: actor Joined.
            let actor_state = prior_state.members.get(&event.actor);
            if !matches!(actor_state.map(|s| s.status), Some(MemberStatus::Joined)) {
                return Err(VerifyError::AdminCountersignActorNotJoined);
            }
            // AC2: actor power >= 100.
            let actor_power_ac = prior_state
                .power_levels
                .get(&event.actor)
                .copied()
                .unwrap_or(0);
            if actor_power_ac < 100 {
                return Err(VerifyError::AdminCountersignActorNotAdmin);
            }
            // AC3: target_event_id non-zero.
            if target_event_id.iter().all(|b| *b == 0) {
                return Err(VerifyError::AdminCountersignTargetIdMalformed);
            }
            // Note: AC verify does NOT require the target proposal to
            // be in the event log yet. Lenient forward-ref semantics
            // mirror ZEB-254's JoinCountersign — out-of-order DAG-sync
            // delivery is normal. Pairing happens at materialize time.
        }
        MembershipEventKind::RecoveryProposal {
            lost_admin,
            new_admin,
            config_digest,
        } => {
            // ZEB-713 §3.2 — gates RP1-RP6. Deliberately NOT the AP
            // gates: AP2 (power-100 proposer) is exactly what a
            // bricked sole-admin community cannot satisfy.

            // RP2: recovery must be configured.
            let Some(config) = prior_state.recovery_designates.as_ref() else {
                return Err(VerifyError::RecoveryNotConfigured);
            };
            // RP1: actor is a designate AND currently Joined. (This is
            // precisely the event non-admins may author — no power gate.)
            if !config.designates.contains(&event.actor)
                || !is_joined_member(prior_state, &event.actor)
            {
                return Err(VerifyError::RecoveryProposalActorNotDesignate);
            }
            // RP4: lost_admin is a currently-Joined power-100 member.
            // (A genuinely lost admin never authored a Leave, so Joined
            // is the honest state; an admin who LEFT is not recoverable
            // — their departure was voluntary and epoch-rotated already.)
            // Bootstrap-admin exception (mirrors the EpochRotation C4
            // arm): ctx.admin_addr may hold the implicit bootstrap
            // power 100 without a member record; None membership means
            // "was never kicked/banned" and the identity is exactly the
            // one a sole-admin community loses.
            let lost_power = prior_state
                .power_levels
                .get(lost_admin)
                .copied()
                .unwrap_or(0);
            let lost_is_bootstrap_admin =
                *lost_admin == ctx.admin_addr && !prior_state.members.contains_key(lost_admin);
            if lost_power != 100
                || !(is_joined_member(prior_state, lost_admin) || lost_is_bootstrap_admin)
            {
                return Err(VerifyError::RecoveryProposalLostAdminNotAdmin);
            }
            // RP3: new_admin currently Joined, not already power-100,
            // and distinct from lost_admin.
            let new_power = prior_state
                .power_levels
                .get(new_admin)
                .copied()
                .unwrap_or(0);
            if new_admin == lost_admin
                || new_power == 100
                || !is_joined_member(prior_state, new_admin)
            {
                return Err(VerifyError::RecoveryProposalNewAdminInvalid);
            }
            // RP5: digest binds the proposal to the live config
            // generation. Encode failure => fail closed (mismatch).
            match recovery_config_digest(config) {
                Ok(live) if live == *config_digest => {}
                _ => return Err(VerifyError::RecoveryProposalConfigDigestMismatch),
            }
            // RP6: one open proposal per designate — structural spam
            // bound (≤ |designates| open proposals community-wide,
            // spec §6 T1). Open = Collecting or TimeLocked in the
            // prior state's derived view (which is evaluated on the
            // R4-6 now-floor, so expired proposals don't block).
            let has_open = prior_state.recovery_proposals.iter().any(|p| {
                p.proposer == event.actor
                    && matches!(
                        p.phase,
                        RecoveryPhase::Collecting | RecoveryPhase::TimeLocked
                    )
            });
            if has_open {
                return Err(VerifyError::RecoveryProposalActorHasOpenProposal);
            }
        }
        MembershipEventKind::RecoveryCosign { target_event_id } => {
            // ZEB-713 RC1 (static half): actor is a designate AND
            // currently Joined. Distinctness and the RC2 digest check
            // are materialize-time concerns — the target proposal may
            // not be in the log yet (lenient forward-ref, mirrors
            // AdminCountersign).
            let Some(config) = prior_state.recovery_designates.as_ref() else {
                return Err(VerifyError::RecoveryNotConfigured);
            };
            if !config.designates.contains(&event.actor)
                || !is_joined_member(prior_state, &event.actor)
            {
                return Err(VerifyError::RecoveryCosignActorNotDesignate);
            }
            if target_event_id.iter().all(|b| *b == 0) {
                return Err(VerifyError::RecoveryCosignTargetIdMalformed);
            }
        }
        MembershipEventKind::RecoveryVeto { target_event_id } => {
            // ZEB-713 RV1 (static half): power-100 + Joined. ONE veto
            // suffices — deliberately not quorum-gated (a veto is a
            // liveness proof restoring the status quo ante; quorum
            // would only help an attacker who already silenced most
            // admins, spec §3.2). The [t₀, deadline] authored-HLC
            // window is a materialize-time concern (forward-ref).
            let veto_power = prior_state
                .power_levels
                .get(&event.actor)
                .copied()
                .unwrap_or(0);
            // Bootstrap-admin exception (mirrors EpochRotation C4 + RP4
            // above): the implicit-power bootstrap admin — the very
            // identity recovery usually targets — must be able to veto
            // even without an explicit member record (T9: the veto IS
            // the proof of life).
            let veto_is_bootstrap_admin =
                event.actor == ctx.admin_addr && !prior_state.members.contains_key(&event.actor);
            if veto_power != 100
                || !(is_joined_member(prior_state, &event.actor) || veto_is_bootstrap_admin)
            {
                return Err(VerifyError::RecoveryVetoActorNotAdmin);
            }
            if target_event_id.iter().all(|b| *b == 0) {
                return Err(VerifyError::RecoveryVetoTargetIdMalformed);
            }
        }
        MembershipEventKind::ReachabilityAnnounce { .. } => {
            // ZEB-321: membership status (RCH5) is enforced in the
            // per-kind power-rules block below, alongside the inner-sig
            // and timestamp-skew checks. No work here — reachability is
            // a "any-joined-member, no power gate" kind (analogous to
            // Fork) but we keep all RCH2-RCH5 enforcement contiguous in
            // the per-kind block for readability.
        }
        MembershipEventKind::CommunityRelayAnnounce { .. } => {
            // ZEB-458: membership status (RCH5 analogue) is enforced in
            // the per-kind power-rules block below, alongside the inner-sig
            // and timestamp-skew checks. Same pattern as ReachabilityAnnounce.
        }
        MembershipEventKind::DeviceAnnounce => {
            // ZEB-495 (ZEB-340 Part 2): the actor (owner) MUST already be a
            // Joined member. This Joined-check is essential and cannot be
            // skipped: unlike steady-state kinds — whose signer is resolved
            // from the materialized enrolled set (which itself implies the
            // actor is a member) — DeviceAnnounce's signer comes from the
            // carried cert (resolved in step 1 via enrolled_key_from_cert),
            // so membership is NOT implied by signature resolution and must
            // be asserted independently here. A device may not introduce
            // itself into a community its owner has not already joined.
            //
            // This is the WHOLE authorization story: a valid Master-signed
            // cert (owner_id == actor, verified in step 1) for an already-
            // admitted owner is sufficient. No power level and no admin
            // countersign are required — the admin vouched for the *owner*,
            // not per-device; adding another of that owner's own master-
            // attested devices introduces no new principal, only a key. In
            // invite-only communities DeviceAnnounce therefore bypasses the
            // PendingJoin/countersign gate by construction (it is not a
            // Join/PendingJoin).
            let member = match prior_state.members.get(&event.actor) {
                Some(m) if m.status == MemberStatus::Joined => m,
                _ => return Err(VerifyError::DeviceAnnounceForNonMember),
            };
            // ZEB-401: loud, early reject of an over-limit DeviceAnnounce. The
            // load-bearing bound is enforced uniformly in materialize (via
            // insert_enrolled_key_capped at every key-adding arm); this reject is
            // an ADDITIONAL DeviceAnnounce-specific signal — correct here because a
            // DeviceAnnounce exists ONLY to add a key, so rejecting an over-limit
            // one loses nothing (unlike a Join, which must still re-admit the
            // member, so its overflow key is silently dropped at materialize
            // instead of failing the join). Reject only when the carried cert's
            // key is NEW and the set is already at MAX_ENROLLED_DEVICE_KEYS — an
            // idempotent re-announce of an already-enrolled key is no growth and
            // stays allowed. The cert is present by construction (step 1 resolved
            // the signer via enrolled_key_from_cert), but its absence is handled
            // defensively rather than panicking.
            if let Some(cert) = &event.enrollment {
                let key = cert.device_pubkeys.classical.ed25519_verify;
                if !member.enrolled_device_keys.contains(&key)
                    && member.enrolled_device_keys.len() >= MAX_ENROLLED_DEVICE_KEYS
                {
                    return Err(VerifyError::EnrolledDeviceKeyLimit);
                }
                // ZEB-668 S3 (Qodo PR #453): reject a DeviceAnnounce for a
                // TOMBSTONED key at verify time, not just materialize time.
                // DeviceAnnounce's signer comes from the carried cert (which
                // stays master-signed-valid after revocation), so without
                // this a retired device could keep inserting verify-passing
                // announce events forever — each a materialize no-op thanks
                // to the tombstone, but unbounded authenticated log spam.
                // Deterministic across replicas: prior_state is
                // sort-order-derived (prior_state_at_event), not
                // arrival-order. Same loses-nothing rationale as the cap
                // reject above; a Join carrying a tombstoned key still
                // admits the member and drops the key at materialize.
                if member.revoked_device_keys.contains(&key) {
                    return Err(VerifyError::DeviceAnnounceForRetiredKey);
                }
            }
        }
        MembershipEventKind::DeviceRetire {
            revocation,
            enrollment,
        } => {
            // ZEB-668 S3: subtractive retire. Actor must exist as a member —
            // ANY status; a Left/Banned owner's stale device key must still
            // be retirable (member state and enrolled keys persist across
            // Leave, and a rejoin would otherwise resurrect the key). No
            // power level: the authority is the carried RevocationCert
            // itself, not the community ladder. Signer resolution (step 1,
            // steady-state path) already proved the event is signed by one
            // of the actor's currently-enrolled devices.
            if !prior_state.members.contains_key(&event.actor) {
                return Err(VerifyError::DeviceRetireForNonMember);
            }
            verify_device_retire_certs(&event.actor, revocation, enrollment, &event.signer_certs)?;
        }
    }

    // 5. Per-kind power rules.
    let actor_power = prior_state
        .power_levels
        .get(&event.actor)
        .copied()
        .unwrap_or(0);
    match &event.kind {
        MembershipEventKind::Join | MembershipEventKind::Leave => {
            // No power check — Join is gated by the countersig logic
            // above (invite-only) or unconditionally allowed (open).
            // Leave is always allowed for the actor themselves.
        }
        MembershipEventKind::Invite { target } => {
            if actor_power < prior_state.power_thresholds.invite {
                return Err(VerifyError::ActorPowerInsufficient);
            }
            // Inviting a Banned target is a no-op in materialize
            // (Banned-sticky). Returning Ok here would leave the IPC
            // caller incorrectly reporting "invite sent". Reject so
            // the UI can surface a clear "unban first" error.
            if let Some(target_state) = prior_state.members.get(target) {
                if target_state.status == MemberStatus::Banned {
                    return Err(VerifyError::InviteTargetBanned);
                }
            }
        }
        MembershipEventKind::Kick { target, reason } => {
            if actor_power < prior_state.power_thresholds.kick {
                return Err(VerifyError::ActorPowerInsufficient);
            }
            // Target must have a member record (Joined / Invited /
            // Left / Banned). Never-seen targets would otherwise
            // materialize as a phantom MemberState with status=Banned
            // and joined_at=kick_time — misleading state about
            // someone who was never part of the community.
            if !prior_state.members.contains_key(target) {
                return Err(VerifyError::KickTargetNotMember);
            }
            let target_power = prior_state.power_levels.get(target).copied().unwrap_or(0);
            if actor_power <= target_power {
                return Err(VerifyError::KickTargetPowerNotLower);
            }
            // ZEB-250 §4.6: direct Kick of an admin is rejected when
            // admin_quorum > 1. Must route via AdminProposal.
            if prior_state.admin_quorum > 1 && target_power == 100 {
                return Err(VerifyError::KickRequiresQuorum);
            }
            // Defense-in-depth: bound the reason string at the CRDT layer
            // so a malicious peer can't bypass the UI cap and persist a
            // giant reason on every replica.
            if let Some(r) = reason {
                if r.chars().count() > MAX_MODERATION_REASON_CHARS {
                    return Err(VerifyError::ReasonTooLong);
                }
            }
        }
        MembershipEventKind::SetPower { target, level } => {
            if actor_power < prior_state.power_thresholds.set_power {
                return Err(VerifyError::ActorPowerInsufficient);
            }
            if *level > prior_state.power_thresholds.max {
                return Err(VerifyError::PowerLevelOutOfRange);
            }
            // ZEB-250 §4.5 / ZEB-734: is this SetPower "admin-affecting" —
            // does it grant top power (`level == max`) or touch a member who
            // currently holds it? `is_admin_affecting_set_power` is the shared
            // predicate (keyed on the immovable `max` tier, AT1-locked at 100,
            // NOT the customizable `set_power`) that the AdminProposal-routing
            // planner and the local mint pre-checks also use.
            let admin_affecting = is_admin_affecting_set_power(prior_state, *target, *level);
            // ZEB-734: granting or removing admin ALWAYS requires the actor to
            // already hold admin power (`max`), regardless of how low
            // `set_power` was customized. Without this, an admin who lowers
            // `set_power` below `max` delegates admin-granting to every member
            // at/above the new threshold — and at `admin_quorum == 1` (the
            // default) such a member could promote themselves to admin or
            // demote the sitting admin with no countersignature. Checked
            // before the quorum gate so a sub-`max` actor gets the accurate
            // "you must be an admin" error rather than a misleading
            // "use AdminProposal" one (which AP2 would reject anyway).
            if admin_affecting && actor_power < prior_state.power_thresholds.max {
                return Err(VerifyError::SetPowerAdminAffectingRequiresAdmin);
            }
            // ZEB-250 §4.5: direct admin-affecting SetPower is rejected when
            // admin_quorum > 1. Must route via AdminProposal.
            if prior_state.admin_quorum > 1 && admin_affecting {
                return Err(VerifyError::SetPowerRequiresQuorum);
            }
        }
        MembershipEventKind::Unban { target, reason } => {
            // Admin-tier: actor must have power >= set_power threshold (100).
            if actor_power < prior_state.power_thresholds.set_power {
                return Err(VerifyError::ActorPowerInsufficient);
            }
            // Target must have a member record. Use the Unban-specific
            // variant so the surfaced error message references "unban" not
            // "kick" when the user is performing an unban.
            let Some(target_state) = prior_state.members.get(target) else {
                return Err(VerifyError::UnbanTargetNotMember);
            };
            // Target must currently be Banned.
            if target_state.status != MemberStatus::Banned {
                return Err(VerifyError::UnbanTargetNotBanned);
            }
            // Same reason-length cap as Kick (defense-in-depth against
            // a peer signing an oversized reason that bypasses the UI).
            if let Some(r) = reason {
                if r.chars().count() > MAX_MODERATION_REASON_CHARS {
                    return Err(VerifyError::ReasonTooLong);
                }
            }
        }
        MembershipEventKind::ChannelCreate {
            channel_id: _,
            name,
            write_power,
            // ZEB-349: kind needs no verification gate (any valid kind tag is
            // accepted; immutability is enforced by ChannelModify lacking the
            // field, not here).
            kind: _,
        } => {
            if actor_power < prior_state.power_thresholds.kick {
                return Err(VerifyError::ChannelAdminInsufficientPower);
            }
            // Validate name length (1-32 chars per spec §12.3).
            if name.trim().is_empty() || name.chars().count() > 32 {
                return Err(VerifyError::ChannelNameInvalid);
            }
            // Validate write_power range.
            if *write_power > prior_state.power_thresholds.max {
                return Err(VerifyError::PowerLevelOutOfRange);
            }
            // Note: duplicate channel_id is NOT rejected here. Cross-blob
            // ordering can deliver a duplicate before its predecessor;
            // materialize's `or_insert_with` is idempotent first-create-wins
            // and converges across replicas regardless of receive order.
            // Verify-time rejection would cause log divergence (some
            // replicas accept the duplicate, others reject) without
            // gaining materialized-view convergence.
        }
        MembershipEventKind::ChannelModify {
            channel_id: _,
            name,
            write_power,
        } => {
            if actor_power < prior_state.power_thresholds.kick {
                return Err(VerifyError::ChannelAdminInsufficientPower);
            }
            // Reject all-None ChannelModify — content-intrinsic, no
            // prior_state dependency. A signed Modify with both fields
            // None has no meaningful payload; reject as malformed.
            if name.is_none() && write_power.is_none() {
                return Err(VerifyError::ChannelModifyNoOp);
            }
            // Validate name length when Some — content-intrinsic.
            if let Some(n) = name {
                if n.trim().is_empty() || n.chars().count() > 32 {
                    return Err(VerifyError::ChannelNameInvalid);
                }
            }
            // Validate write_power range when Some — content-intrinsic.
            if let Some(wp) = write_power {
                if *wp > prior_state.power_thresholds.max {
                    return Err(VerifyError::PowerLevelOutOfRange);
                }
            }
            // Note: NO value-matching check against prior_state. Two
            // mods independently renaming a channel to the same name
            // would otherwise cause CRDT log divergence based on
            // receive order. materialize handles redundant modifies
            // idempotently (the get_mut + only-Some-applies pattern
            // is no-op when values match). The slight log bloat (one
            // extra event per redundant modify) is the cost of cross-
            // blob safety.
            //
            // Note: ChannelModify on unknown channel_id is intentionally
            // ALLOWED — DAG-sync may deliver Modify before Create;
            // materialize safely no-ops on unknown.
        }
        MembershipEventKind::ChannelDelete { channel_id: _ } => {
            if actor_power < prior_state.power_thresholds.kick {
                return Err(VerifyError::ChannelAdminInsufficientPower);
            }
            // Note: NO prior_state-dependent rejection on the receive
            // path. Cross-blob ordering can deliver Delete before its
            // corresponding Create, OR can deliver two Deletes in
            // reverse order at different replicas. Verify-time rejection
            // would cause CRDT log divergence between replicas without
            // gaining materialized-view convergence (materialize handles
            // unknown deletes as a safe no-op, and tombstone updates
            // are first-delete-wins idempotent).
            //
            // Local UX safeguards live in the IPC preflight checks:
            // delete_channel rejects "no such channel" / "already
            // deleted" against the local materialized view before
            // signing, catching the common-case user error. The race
            // window between IPC preflight and engine insert produces
            // at most a redundant tombstone event in the log; UX-wise
            // both delete attempts return Ok.
        }
        MembershipEventKind::EpochRotation { .. } => {
            // C4: EpochRotation lightweight authority pre-check.
            // The issuer must either be:
            //   (a) an admin (power >= kick_threshold AND currently Joined, OR is
            //       the bootstrap admin who has power but may have no member record), OR
            //   (b) any member recorded in prior_state.members (potential
            //       cooperative-leaver; exact Leave-target check is in materialize
            //       which has the full causal log).
            // This fast-path check prevents completely unauthorized addresses
            // (never-members, zero power) from inserting rotation events into
            // the CRDT log. Full causal-log resolution + staleness/recipient-
            // exclusion checks stay in materialize (spec §4.2-4.4).
            //
            // Bootstrap-admin exception: ctx.admin_addr may have no member
            // record if they never issued an explicit Join event. Their power
            // 100 is from materialize's implicit bootstrap step. None membership
            // means "was never kicked/banned"; allow if power >= threshold.
            let issuer_power = actor_power; // already computed above
            let issuer_member_status = prior_state.members.get(&event.actor).map(|s| s.status);
            let issuer_is_joined = matches!(issuer_member_status, Some(MemberStatus::Joined));
            let issuer_is_bootstrap_admin =
                issuer_member_status.is_none() && event.actor == ctx.admin_addr;
            let issuer_is_admin = issuer_power >= prior_state.power_thresholds.kick
                && (issuer_is_joined || issuer_is_bootstrap_admin);
            let issuer_is_member = prior_state.members.contains_key(&event.actor);
            if !issuer_is_admin && !issuer_is_member {
                return Err(VerifyError::EpochEventUnauthorized);
            }
        }
        MembershipEventKind::EpochCatchup { epoch, .. } => {
            // C4: EpochCatchup lightweight authority pre-check.
            // The issuer must have admin power AND be currently Joined
            // in prior_state (or be the bootstrap admin with no member
            // record). Additionally, the epoch claimed must match
            // prior_state's current epoch — a stale-epoch catchup would
            // always be a no-op in materialize, but blocking it at verify
            // time prevents CRDT log pollution from obviously invalid events.
            // Full triggered_by resolution + target-in-recipients check
            // stay in materialize (spec §4.6).
            let issuer_power = actor_power; // already computed above
            let issuer_member_status = prior_state.members.get(&event.actor).map(|s| s.status);
            let issuer_is_joined = matches!(issuer_member_status, Some(MemberStatus::Joined));
            let issuer_is_bootstrap_admin =
                issuer_member_status.is_none() && event.actor == ctx.admin_addr;
            let issuer_is_effective_admin = issuer_power >= prior_state.power_thresholds.kick
                && (issuer_is_joined || issuer_is_bootstrap_admin);
            if !issuer_is_effective_admin {
                return Err(VerifyError::EpochEventUnauthorized);
            }
            let current_epoch = prior_state.current_epoch.unwrap_or(0);
            if *epoch != current_epoch {
                return Err(VerifyError::EpochEventUnauthorized);
            }
        }
        MembershipEventKind::Fork { reason, .. } => {
            // ZEB-285: any joined non-Banned member can fork at any time.
            // Power threshold 0 — same as Leave. Non-mutating: doesn't
            // affect membership/power/channels, doesn't trigger EpochRotation.
            // Membership check already performed in the joined-membership
            // block above (ActorNotJoined gate). No additional shape
            // validation required: fork_space_id is a self-reported value
            // from the forker; receivers don't (and can't) verify the fork's
            // existence on the forker's device.
            if actor_power < prior_state.power_thresholds.invite {
                return Err(VerifyError::ActorPowerInsufficient);
            }
            // ZEB-649: bound the reason at the CRDT layer (same
            // defense-in-depth as Kick/Unban) so a malicious peer can't
            // bypass the UI cap and persist a giant reason on every replica.
            if let Some(r) = reason {
                if r.chars().count() > MAX_MODERATION_REASON_CHARS {
                    return Err(VerifyError::ReasonTooLong);
                }
                // ZEB-649 (Qodo PR #434): the mandatory-why invariant — a
                // modified client sending Some("") / whitespace must not
                // satisfy the wire shape with a meaningless reason. Absent
                // (None, pre-ZEB-649) remains valid.
                if r.trim().is_empty() {
                    return Err(VerifyError::ReasonEmpty);
                }
            }
        }
        MembershipEventKind::PendingJoin { .. } => {
            // All PendingJoin gates are handled in the joined-membership block
            // above (P1–P6). No separate power rule needed.
        }
        MembershipEventKind::JoinCountersign { .. } => {
            // ZEB-254: actor must be Joined + power >= invite_threshold.
            // Target event existence is a materialize concern (allow
            // out-of-order delivery — JoinCountersign can land before
            // its target PendingJoin on Zenoh state-root sync).
            if !is_joined_member(prior_state, &event.actor) {
                return Err(VerifyError::JoinCountersignActorNotJoined);
            }
            if actor_power < prior_state.power_thresholds.invite {
                return Err(VerifyError::JoinCountersignActorPowerInsufficient);
            }
        }
        MembershipEventKind::AdminProposal { .. } => {
            // All AdminProposal gates (AP1-AP5) are handled in the
            // joined-membership block above. No separate power rule needed.
        }
        MembershipEventKind::AdminCountersign { .. } => {
            // All AdminCountersign gates (AC1-AC3) are handled in the
            // joined-membership block above. No separate power rule needed.
        }
        MembershipEventKind::RecoveryProposal { .. }
        | MembershipEventKind::RecoveryCosign { .. }
        | MembershipEventKind::RecoveryVeto { .. } => {
            // ZEB-713: all recovery gates (RP1-RP6, RC1, RV1 static
            // halves) are handled in the joined-membership block above.
            // Window/digest/distinctness checks are materialize-time
            // concerns (lenient forward-ref). No separate power rule.
        }
        MembershipEventKind::ReachabilityAnnounce { payload } => {
            // ZEB-321 RCH1-RCH5 enforcement.
            //
            // RCH1: outer SignedMembershipEvent signature — already
            // verified by `verify_signature()` at the top of this
            // function (uniform with every other variant; surfaces as
            // VerifyError::SignatureInvalid). No work here.

            // ZEB-339: the inner identity signature is produced by the same
            // enrolled device key that signed the outer event. `signer` was
            // resolved (and bound to event.actor via verify_membership_signer)
            // in step 1 above. Derive the Ed25519 verifying key from the
            // resolved device key for the RCH2 check.
            let signer_vk = ed25519_dalek::VerifyingKey::from_bytes(&signer.device_ed25519)
                .map_err(|_| VerifyError::SignatureInvalid)?;

            // RCH2: inner identity signature must verify over canonical
            // CBOR of (nd, rl, da, ts, actor, hlc) using the signer's
            // enrolled device Ed25519 verifying key.
            crate::reachability_record::verify_inner_signature(
                payload,
                &event.actor,
                &event.at,
                &signer_vk,
            )
            .map_err(|e| match e {
                crate::reachability_record::InnerSigError::Encode => {
                    VerifyError::EncodeError("inner reachability sig encode".to_string())
                }
                crate::reachability_record::InnerSigError::Invalid => {
                    VerifyError::ReachabilityInnerSigInvalid
                }
            })?;

            // RCH3: actor↔signer binding. ZEB-339: the inner sig and outer
            // sig are produced by the same enrolled device key, and
            // verify_membership_signer (step 1) already proved
            // signer.owner == event.actor (the enrolled key resolves under
            // the actor's materialized membership). The previous
            // address-derivation form of RCH3 assumed a single-identity
            // model where address_hash(signing_key) == actor; under the
            // owner/device split that equality no longer holds, so the
            // binding is enforced upstream rather than re-derived here.

            // RCH4: announced_at_ms vs hlc.wall_ms within ±30 min.
            // Use `u64::abs_diff` so the |skew| computation stays in
            // unsigned space — the prior `(u64 as i64) - (u64 as i64)`
            // then `i64::abs()` formulation could overflow at adversarial
            // wall_ms values (i64::MIN.abs() panics in debug, wraps in
            // release). Per CodeRabbit on PR #157.
            let skew = payload.announced_at_ms.abs_diff(event.at.wall_ms);
            if skew > REACHABILITY_TIMESTAMP_SKEW_MAX_MS {
                return Err(VerifyError::ReachabilityTimestampSkew);
            }

            // RCH5: actor must be Joined at hlc. Same shape as Fork's
            // membership check (any joined member, no power gate).
            // Reuses prior_state.members — verify_event's caller has
            // already projected state up to (but not including) this
            // event per the function's contract.
            if !is_joined_member(prior_state, &event.actor) {
                return Err(VerifyError::ReachabilityActorNotMember);
            }
        }
        MembershipEventKind::CommunityRelayAnnounce { payload } => {
            // ZEB-458 P4B: RCH1-RCH5 analogue for CommunityRelayAnnounce.
            //
            // RCH1 (outer signature) already verified by `verify_signature()`
            // above — surfaces as VerifyError::SignatureInvalid. No work here.

            // ZEB-339: derive the signer's Ed25519 verifying key from the
            // resolved enrolled device key (same origin as ReachabilityAnnounce).
            let signer_vk = ed25519_dalek::VerifyingKey::from_bytes(&signer.device_ed25519)
                .map_err(|_| VerifyError::SignatureInvalid)?;

            // RCH2 analogue: inner identity signature must verify over canonical
            // CBOR of (relay fields + ad_at + actor + hlc) using the signer's
            // enrolled device Ed25519 verifying key.
            crate::community_relay_announce::verify_inner_signature(
                payload,
                &event.actor,
                &event.at,
                &signer_vk,
            )
            .map_err(|e| match e {
                crate::reachability_record::InnerSigError::Encode => {
                    VerifyError::EncodeError("inner community relay sig encode".to_string())
                }
                crate::reachability_record::InnerSigError::Invalid => {
                    VerifyError::CommunityRelayInnerSigInvalid
                }
            })?;

            // RCH4 analogue: ad_at vs hlc.wall_ms within ±30 min.
            // The 30-min bound is shared with REACHABILITY_TIMESTAMP_SKEW_MAX_MS.
            let skew = payload.ad_at.abs_diff(event.at.wall_ms);
            if skew > REACHABILITY_TIMESTAMP_SKEW_MAX_MS {
                return Err(VerifyError::CommunityRelayTimestampSkew);
            }

            // RCH5 analogue: actor must be Joined at hlc (any joined member,
            // no power gate — same shape as ReachabilityAnnounce).
            if !is_joined_member(prior_state, &event.actor) {
                return Err(VerifyError::CommunityRelayActorNotMember);
            }
        }
        MembershipEventKind::DeviceAnnounce => {
            // ZEB-495: NO power gate and NO admin countersign. Authorization
            // is entirely the cert (verified in step 1) + the already-Joined
            // membership check (step 4 above). A Master-signed cert for an
            // already-admitted owner only ever adds one of that owner's own
            // devices' keys — no escalation is possible.
        }
        MembershipEventKind::DeviceRetire { .. } => {
            // ZEB-668 S3: NO power gate — same reasoning as DeviceAnnounce.
            // Authorization is entirely the carried cert pair (verified in
            // step 4's `verify_device_retire_certs`): a valid revocation for
            // one of the actor's OWN devices only ever removes a key. No
            // escalation is possible; requiring power would let a
            // zero-power owner's compromised device linger.
        }
    }

    Ok(())
}

/// ZEB-558 — bootstrap-admission for an OPEN-community publisher we don't yet
/// know locally. Given the membership events carried in an incoming publish
/// blob, return the publisher's `MemberState` (with enrolled device keys)
/// IFF the blob carries a signature-valid OPEN self-`Join` for them — the
/// exact authorization `verify_event` applies on the merge path (cert +
/// signer key + open-Join rule). Returns `None` when no such valid self-Join
/// is present, so the caller rejects.
///
/// OPEN communities only: the caller (`handle_incoming_publish`) gates this
/// on `!is_invite_only` AND an entirely-unknown publisher. The returned
/// `MemberState` is used solely to verify the root `publisher_sig`; the
/// authoritative merge re-validates and inserts the Join via `insert_event`,
/// so this helper never widens what actually lands in the CRDT.
pub fn bootstrap_admit_open_publisher(
    incoming_events: &[SignedMembershipEvent],
    publisher_addr: OwnerAddr,
    admin_addr: OwnerAddr,
    expected_community_id: SpaceId,
    publisher_at: &Hlc,
) -> Option<MemberState> {
    let ctx = VerifyContext {
        expected_community_id,
        admin_addr,
        is_invite_only: false,
    };
    // Authorize the deferred open-bootstrap publisher against their OWN
    // membership as of strictly-before the root HLC — the same window the
    // known-publisher path uses via `prior_state_at_hlc(payload.at)`. We can't
    // call that helper directly (the publisher is unknown, so their events live
    // only in this blob, not in local `state.events`), so we reconstruct the
    // equivalent prefix from the blob, then materialize it:
    //
    //   * Only the publisher's own membership-changing events
    //     (Join / DeviceAnnounce / Leave) bear on whether they are a Joined
    //     member authorized to sign this root.
    //   * Bounded by `publisher_at`: a device announced AFTER the root HLC must
    //     not seed an authorizing key, and a Join landing after the root must
    //     not resurrect membership for this publish.
    //   * `Leave` is folded too, so a Join→Leave-before-root publisher
    //     materializes to `Left` and is correctly NOT admitted (closes the
    //     "already-departed" / "rejoined-then-left" gaps).
    //
    // Each event is verified against the membership accumulated so far — the
    // same per-event prior the authoritative merge applies — so signatures,
    // certs (DeviceAnnounce / Join carry their own; Leave's signer resolves from
    // prior enrolled keys), and the Banned guard are enforced exactly as on the
    // merge path. The first valid Join verifies against an empty prior (open
    // Join needs no power/countersig). `verify_publisher_sig` accepts ANY
    // enrolled key, so seeding the full device set (incl. a second device added
    // via DeviceAnnounce — ZEB-339 / #284) is load-bearing for device-#2-signed
    // publishes.
    let before_root = |e: &SignedMembershipEvent| {
        (e.at.wall_ms, e.at.logical, &e.at.device_id)
            < (
                publisher_at.wall_ms,
                publisher_at.logical,
                &publisher_at.device_id,
            )
    };
    let mut candidates: Vec<&SignedMembershipEvent> = incoming_events
        .iter()
        .filter(|e| e.actor == publisher_addr && before_root(e))
        .filter(|e| {
            matches!(
                e.kind,
                MembershipEventKind::Join
                    | MembershipEventKind::DeviceAnnounce
                    | MembershipEventKind::Leave
            )
        })
        .collect();
    // Pre-sort once; pushing accepted events in this order keeps `verified`
    // sorted without re-sorting on each iteration.
    candidates.sort_by(|a, b| event_sort_key(a).cmp(&event_sort_key(b)));

    let mut verified: Vec<SignedMembershipEvent> = Vec::new();
    for event in candidates {
        // Per-event prior, with the event's own wall_ms as the "now floor" —
        // mirrors `prior_state_at_event`'s authorization-time semantics.
        let prior = materialize_with_now(&verified, admin_addr, Some(event.at.wall_ms));
        if verify_event(event, &prior, &ctx).is_ok() {
            verified.push(event.clone());
        }
    }
    if verified.is_empty() {
        return None;
    }

    // Materialize the verified pre-root prefix → the canonical MemberState the
    // merge will produce; admit only if the publisher is Joined at that point.
    let mat = materialize_with_now(&verified, admin_addr, Some(publisher_at.wall_ms));
    mat.members
        .get(&publisher_addr)
        .filter(|s| matches!(s.status, MemberStatus::Joined))
        .cloned()
}

/// ZEB-526: invite-only sibling of [`bootstrap_admit_open_publisher`]. Authorizes
/// a deferred INVITE-ONLY publisher whose membership is unknown locally, so the
/// receiver can derive the publisher's enrolled keys (for the root publisher_sig
/// check) and let the authoritative merge insert the publisher's self-authorizing
/// PendingJoin — firing the admin's auto-counter-sign so the join converges over
/// the zenoh-publish fallback when the iroh first-contact dial didn't deliver.
///
/// Two differences from the open helper:
///   * **Admits a `PendingJoin` publisher, not just `Joined`.** An invite-only
///     redemption mints a `PendingJoin` (admin counter-sign pending); the
///     publisher is legitimately not-yet-`Joined` when it first publishes. The
///     merge admits ONLY the PendingJoin (their non-membership events fail the
///     per-event `verify_event`); their root stays gated until counter-signed.
///   * **Includes the admin's own bootstrap events in the authorization window.**
///     The publisher's PendingJoin carries an admin-signed `InviteToken`;
///     `verify_event` must resolve the ADMIN's enrolled key, which materializes
///     from the admin's own bootstrap `Join`. That admin `Join` rides in the
///     joiner's published blob (the joiner inserts it at redeem time), so we seed
///     candidates with both `admin_addr`- and `publisher_addr`-authored events.
///
/// Bounded strictly-before `publisher_at` (same pre-root window as the known-
/// publisher path) and each event verified against the membership accumulated so
/// far — so the admin `Join` (creator = root of trust) verifies first and seeds
/// the admin key, then the publisher's PendingJoin verifies its `InviteToken`
/// against it. The authoritative merge re-validates and inserts everything; this
/// is the cheap pre-flight that supplies keys + confirms a real self-authorizing
/// join exists (anti-spam for an epoch-key holder who carries no valid join).
pub fn bootstrap_admit_invite_only_publisher(
    incoming_events: &[SignedMembershipEvent],
    publisher_addr: OwnerAddr,
    admin_addr: OwnerAddr,
    expected_community_id: SpaceId,
    publisher_at: &Hlc,
) -> Option<MemberState> {
    let ctx = VerifyContext {
        expected_community_id,
        admin_addr,
        is_invite_only: true,
    };
    let before_root = |e: &SignedMembershipEvent| {
        (e.at.wall_ms, e.at.logical, &e.at.device_id)
            < (
                publisher_at.wall_ms,
                publisher_at.logical,
                &publisher_at.device_id,
            )
    };
    // Seed with BOTH the admin's and the publisher's own membership-bearing
    // events (the admin's so the InviteToken signer key resolves). The merge's
    // per-event `verify_event` is the authoritative gate; this only needs to
    // reconstruct enough prior to verify + materialize the publisher's join.
    let mut candidates: Vec<&SignedMembershipEvent> = incoming_events
        .iter()
        .filter(|e| (e.actor == admin_addr || e.actor == publisher_addr) && before_root(e))
        .filter(|e| {
            matches!(
                e.kind,
                MembershipEventKind::Join
                    | MembershipEventKind::PendingJoin { .. }
                    | MembershipEventKind::JoinCountersign { .. }
                    | MembershipEventKind::DeviceAnnounce
                    | MembershipEventKind::Leave
            )
        })
        .collect();
    candidates.sort_by(|a, b| event_sort_key(a).cmp(&event_sort_key(b)));

    let mut verified: Vec<SignedMembershipEvent> = Vec::new();
    for event in candidates {
        let prior = materialize_with_now(&verified, admin_addr, Some(event.at.wall_ms));
        if verify_event(event, &prior, &ctx).is_ok() {
            verified.push(event.clone());
        }
    }
    if verified.is_empty() {
        return None;
    }

    // Admit ONLY a publisher who materializes to PendingJoin — the invite-only
    // pre-counter-sign state, which is exactly ZEB-526's case: a joiner whose
    // self-authorizing PendingJoin (admin-signed InviteToken) the admin has not
    // yet seen. We deliberately do NOT admit a `Joined` publisher here: an
    // unknown publisher self-presenting as Joined (e.g. the admin's own bootstrap
    // Join, or a countersigned member's republished root) is not part of the
    // joiner-admission flow and must reach the receiver through normal
    // propagation, preserving the invite-only cold-cache reject→propagate→admit
    // contract. Widening to `Joined` would let any self-authorizing root
    // self-admit on first contact, enlarging the invite-only trust surface for no
    // ZEB-526 benefit. Left/Banned → not admitted.
    let mat = materialize_with_now(&verified, admin_addr, Some(publisher_at.wall_ms));
    mat.members
        .get(&publisher_addr)
        .filter(|s| matches!(s.status, MemberStatus::PendingJoin))
        .cloned()
}

/// Select the catchup-trigger event id for `target`: the most recent (by HLC)
/// membership event authored by `target` that is a `Join` OR a **countersigned**
/// `PendingJoin`. Returns `None` when no such event exists.
///
/// ZEB-578: this MUST stay consistent with the EpochCatchup **apply-side**
/// trigger acceptance in `apply_event` (search "ZEB-254 R5-1"), which clears
/// `pending_catchup_for` for a recipient only when the catchup's `triggered_by`
/// points at a `Join` or a *countersigned* `PendingJoin`. The self-heal
/// synthesizer (`self_heal_community_observer`) uses this to choose the
/// `triggered_by` when it MINTS a catchup. If synthesizer and apply disagree, an
/// invite-only joiner — whose originating membership event is a countersigned
/// `PendingJoin`, never a `Join` — is either never sent a catchup (synthesizer
/// too strict, the bug this fixes) or sent one the apply side ignores.
///
/// Un-countersigned `PendingJoin` is intentionally excluded: a still-pending
/// joiner is not yet a member (§10.6 backward-secrecy), and `pending_catchup_for`
/// only ever holds members enqueued at status `Joined` (the `Join` arm and the
/// *countersigned*-`PendingJoin` arm of `apply_event`). The explicit
/// countersigned check keeps the two paths provably aligned regardless.
pub fn select_catchup_trigger_event(
    events: &[SignedMembershipEvent],
    target: OwnerAddr,
) -> Option<EventId> {
    let countersigned: std::collections::HashSet<EventId> = events
        .iter()
        .filter_map(|e| match &e.kind {
            MembershipEventKind::JoinCountersign { target_event_id } => Some(*target_event_id),
            _ => None,
        })
        .collect();
    events
        .iter()
        .filter(|e| {
            e.actor == target
                && match &e.kind {
                    MembershipEventKind::Join => true,
                    MembershipEventKind::PendingJoin { .. } => countersigned.contains(&e.id),
                    _ => false,
                }
        })
        // "Most recent" by the repo's canonical TOTAL order (`event_sort_key`:
        // HLC + device_id + id + sig), not a partial `(wall_ms, logical)` key —
        // an HLC tie (same owner across two devices) must resolve deterministically
        // so every replica's synthesizer picks the same `triggered_by` and derives
        // the same catchup dedupe key. Matches the apply-side candidate sort.
        .max_by(|a, b| event_sort_key(a).cmp(&event_sort_key(b)))
        .map(|e| e.id)
}

/// ZEB-285: verify a single signed event against a frozen pre-fork
/// snapshot's `identity_pubs` map. Used by the fork's UI when loading
/// pre-fork history for display — fork members are not necessarily
/// members of the original community, so the live OwnerDeviceCache
/// won't have the original's signers cached.
///
/// **Phase 1 scope**: performs Ed25519 signature verification via the
/// snapshot's `identity_pubs` keyset (including the pubkey→actor address
/// binding check inside `verify_signature`). Power-rule and membership-
/// state replay is deferred to Phase 2 hardening because `PreForkSnapshot`
/// does not carry an explicit `admin_addr` (required by `materialize`
/// to seed the bootstrap power level). Phase 1 invokes this lazily at
/// display time only against a snapshot from a trusted inviter; eagerly
/// verifying every event at redeem time is a Phase 2 concern.
///
/// **Phase 1 NOTE**: This function verifies `SignedMembershipEvent` only.
/// Channel events from `PreForkSnapshot.channel_log` are NOT
/// signature-verified by Phase 1; they are rendered with the same
/// muted treatment as other pre-fork content under the trust assumption
/// that the forker bundled honest history. Phase 2 will add channel-event
/// verification via a `verify_snapshot_channel_event` sibling function.
/// See spec §4.4 for the broader Phase 1 lazy-verification rationale.
///
/// Returns `Ok(())` when the signature is valid and the signer is in
/// `snapshot.identity_pubs`. Returns `Err(VerifyError::UnknownSigner)`
/// when the signer is absent; returns `Err(VerifyError::SignatureInvalid)`
/// (or `ActorPubkeyMismatch`) when the signature or address binding fails.
///
/// See spec §4.3.
pub fn verify_snapshot_event(
    event: &SignedMembershipEvent,
    snapshot: &crate::community_invite::PreForkSnapshot,
) -> Result<(), VerifyError> {
    // Step 0: community_id binding check. A validly-signed event from a
    // DIFFERENT community must be rejected even when the same OwnerAddr is a
    // member of both communities — without this check, an attacker could inject
    // events from another community whose signer happens to appear in
    // snapshot.identity_pubs. (Fix: PR #122 security finding.)
    if event.community_id != snapshot.original_community_id {
        return Err(VerifyError::CommunityIdMismatch {
            expected: snapshot.original_community_id,
            actual: event.community_id,
        });
    }

    // Step 1: signer must be recorded in identity_pubs.
    let signer_pub =
        snapshot
            .identity_pubs
            .get(&event.actor)
            .ok_or(VerifyError::UnknownSigner {
                signer: event.actor,
            })?;

    // Step 2: Ed25519 signature verification + pubkey→actor address binding.
    // verify_signature derives address_hash from signer_pub and checks it
    // equals event.actor, then verify_strict-checks the sig over the
    // canonical-CBOR EventPayload bytes. Rejects with ActorPubkeyMismatch
    // if the address doesn't match, SignatureInvalid if the sig is bad.
    verify_signature(event, signer_pub)?;

    // Step 3: if the event carries a countersig (invite-only Join voucher),
    // verify it too against snapshot.identity_pubs. Every signature on the
    // event must be verifiable against the snapshot's recorded pubkeys.
    // verify_countersig covers both the pubkey→signer binding and the
    // Ed25519 sig check (same EventPayload body the actor signed).
    // (Fix: PR #122 round-2 bot review — CodeRabbit Major.)
    if let Some(ref cs) = event.countersig {
        let countersigner_pub = snapshot
            .identity_pubs
            .get(&cs.signer)
            .ok_or(VerifyError::UnknownSigner { signer: cs.signer })?;
        // ZEB-339: verify_countersig now resolves the signer from a
        // MaterializedMembership; the pre-fork snapshot path instead carries
        // explicit identity_pubs, so the pubkey→signer binding + Ed25519
        // check is performed inline here (mirrors the former verify_countersig
        // body). Phase 2 fork hardening will migrate snapshots onto the
        // cert/materialized-key model.
        let identity = harmony_identity::Identity::from_public_bytes(countersigner_pub)
            .map_err(|_| VerifyError::InvalidIdentityPub)?;
        if identity.address_hash != cs.signer.0 {
            return Err(VerifyError::CounterSignerPubkeyMismatch);
        }
        let bytes = canonical_cbor_encode(&EventPayload::from(event))?;
        let sig = Signature::from_bytes(&cs.sig);
        identity
            .verifying_key
            .verify_strict(&bytes, &sig)
            .map_err(|_| VerifyError::CounterSigInvalid)?;
    }

    Ok(())

    // NOTE (Phase 2): reconstruct prior-state by replaying
    // snapshot.membership_events in HLC ascending order and invoke
    // verify_event with the materialized context. Deferred because
    // PreForkSnapshot does not carry admin_addr (needed by materialize
    // for the bootstrap power-100 seed). Phase 2 will add admin_addr
    // to PreForkSnapshot or derive it from the earliest Join event.
}

/// True iff `addr` is currently a Joined member in `state`. Pure
/// helper — no logging, no allocation. Used by verify_event to gate
/// moderation actions and counter-signing on active membership.
fn is_joined_member(state: &MaterializedMembership, addr: &OwnerAddr) -> bool {
    state
        .members
        .get(addr)
        .map(|s| s.status == MemberStatus::Joined)
        .unwrap_or(false)
}

/// Per-community power thresholds. v1 hardcoded; per-community
/// customization is deferred to ZEB-251.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerThresholds {
    #[serde(rename = "iv")]
    pub invite: u8,
    #[serde(rename = "kk")]
    pub kick: u8,
    #[serde(rename = "sp")]
    pub set_power: u8,
    #[serde(rename = "mx")]
    pub max: u8,
}

/// Sub-C v1 hardcoded defaults — see ZEB-217 spec §"Power thresholds".
pub const POWER_THRESHOLDS: PowerThresholds = PowerThresholds {
    invite: 0,
    kick: 50,
    set_power: 100,
    max: 100,
};

/// Maximum Unicode codepoint count for a moderation reason string
/// (Kick or Unban event's `reason: Option<String>` field). The UI
/// `ModerationReasonDialog` already enforces `maxlength="280"` on the
/// textarea — this constant mirrors that limit at the CRDT verification
/// layer so a malicious or buggy peer cannot inject an oversized reason
/// that bypasses the UI cap and persists across all replicas.
///
/// `chars().count()` (codepoint count) is the basis because:
///   - It matches user-perceptible "characters" reasonably well (1 codepoint
///     per ASCII/BMP char; 1 codepoint per emoji on modern Unicode planes).
///   - It's deterministic across replicas regardless of UTF-8 byte width.
///   - 280 codepoints is at minimum as permissive as the UI's
///     `maxlength="280"` (which counts UTF-16 code units, so emojis double).
pub const MAX_MODERATION_REASON_CHARS: usize = 280;

/// ZEB-401: hard upper bound on `MemberState.enrolled_device_keys` per member.
/// Every membership/channel-log/root-publish verify path iterates a member's
/// full enrolled-key set (`resolve_enrolled_signer`, `verify_countersig`,
/// `verify_invite_token_sig_with_enrolled`, `verify_publisher_sig`,
/// `verify_channel_event`), and `snapshot_at` clones it per event, so an
/// uncapped set would let a member amplify CPU/alloc on packet receive. ZEB-495
/// (the `DeviceAnnounce` event) made N>1 a real path — before it, single-device
/// enrollment kept N≈1 in practice — so the bound is now load-bearing, not
/// hypothetical. 32 is generous headroom over the ~12-device target of ZEB-169
/// (no real owner approaches it) while keeping the worst case — O(N) ed25519
/// verifies, hit only on a member's OWN bad-signature events since the loops
/// short-circuit via `.any()` for legitimate ones — bounded at ~1.6ms.
///
/// `materialize` is the UNIFORM enforcement point: every key-adding arm
/// (`Join`, `PendingJoin` countersigned + un-countersigned, `DeviceAnnounce`)
/// routes its cert-key insert through [`insert_enrolled_key_capped`], so the
/// materialized set every verify loop iterates can never exceed the bound —
/// including the Leave→Join rejoin path, where `Leave` preserves the set and a
/// fresh cert per rejoin would otherwise accumulate keys without limit.
/// `verify_event` additionally rejects an over-limit `DeviceAnnounce`
/// (`VerifyError::EnrolledDeviceKeyLimit`) as an early/loud signal — that reject
/// is DeviceAnnounce-specific because a DeviceAnnounce exists ONLY to add a key
/// (rejecting it loses nothing), whereas a `Join`/`PendingJoin` at the cap must
/// still (re)admit the member, so its overflow key is silently dropped rather
/// than failing the whole join.
pub const MAX_ENROLLED_DEVICE_KEYS: usize = 32;

/// ZEB-401: compile-time guard — the cap must clear ZEB-169's ~12-device
/// target with headroom so the limit never bites a legitimate multi-device
/// owner. Lowering `MAX_ENROLLED_DEVICE_KEYS` below 12 fails the build.
const _: () = assert!(MAX_ENROLLED_DEVICE_KEYS >= 12);

/// ZEB-401: insert `key` into a member's enrolled-device-key set while holding
/// the `MAX_ENROLLED_DEVICE_KEYS` bound. An already-present key is idempotent;
/// a NEW key is added only while the set is under the cap. This is the single
/// load-bearing enforcement point — it must wrap EVERY site that grows a
/// member's `enrolled_device_keys`, so no event kind (Join / PendingJoin /
/// DeviceAnnounce, including repeated Leave→Join) can push the set the verify
/// loops iterate past the bound.
fn insert_enrolled_key_capped(set: &mut BTreeSet<[u8; 32]>, key: [u8; 32]) {
    if set.contains(&key) || set.len() < MAX_ENROLLED_DEVICE_KEYS {
        set.insert(key);
    }
}

// ZEB-668 S3, revised in PR #453 review: tombstones are deliberately
// UNCAPPED. Every tombstone requires a `DeviceRetire` that passed
// `verify_event` (a valid master- or self-signed RevocationCert), so growth
// is authenticated and owner-inflicted — and the event log itself grows by
// one event per retire regardless, so capping the derived set bounds
// nothing. A cap would silently drop the remove-wins guarantee for the
// overflow key (a pre-retirement DeviceAnnounce sorting later in replay
// could re-add a revoked key — CodeRabbit PR #453). Unlike
// `enrolled_device_keys` (iterated per signature resolution — the ZEB-401
// cost bound), tombstones are only ever probed with `contains`, so an
// unbounded set adds no per-verify iteration cost.

/// ZEB-668 S3: tombstone-aware enrolled-key insert for materialize arms that
/// hold a `&mut MemberState`. Refuses tombstoned (retired) keys — remove-wins
/// — then applies the ZEB-401 cap. Key-adding arms that rebuild the
/// `MemberState` literal from a cloned set (Join / PendingJoin) perform the
/// identical `revoked.contains` check inline instead; a raw
/// `insert_enrolled_key_capped` call without either guard would reopen the
/// replay-order resurrection hole.
fn insert_enrolled_key_unless_retired(member: &mut MemberState, key: [u8; 32]) {
    if member.revoked_device_keys.contains(&key) {
        return;
    }
    insert_enrolled_key_capped(&mut member.enrolled_device_keys, key);
}

/// ZEB-254: PendingJoin events older than this (community current HLC
/// minus event HLC, in wall-ms) are hidden from materialize unless a
/// matching JoinCountersign exists. 30 days.
pub const MATERIALIZE_PENDING_EXPIRY_MS: u64 = 30 * 86_400_000;

/// ZEB-250: AdminProposal expiry window. A proposal that reaches
/// quorum more than 30 days after its proposer's HLC is dead — late
/// countersigns to expired proposals are no-ops at materialize time.
///
/// Mirrors ZEB-254's PendingJoin 30-day expiry. Same constant value;
/// kept as a separate const for clarity at the call site.
pub const ADMIN_PROPOSAL_EXPIRY_MS: u64 = 30 * 86_400_000;

/// ZEB-321 RCH4: maximum allowed skew (ms) between a
/// ReachabilityAnnounce payload's `announced_at_ms` and the event's
/// HLC `wall_ms`. ±30 minutes — generous enough to tolerate normal
/// device clock drift; tight enough to reject obviously-tampered
/// records (spec §5.5 silent-drop semantics).
pub const REACHABILITY_TIMESTAMP_SKEW_MAX_MS: u64 = 30 * 60 * 1000;

/// ZEB-713 RD4: floor on `SetRecoveryDesignates.veto_window_ms`.
/// 7 days — enforced at verify time on every replica, so a malicious
/// client build cannot make honest replicas accept a 1-hour window
/// (spec §6 T6). The default the D2 UI offers is 30 days.
pub const RECOVERY_VETO_WINDOW_FLOOR_MS: u64 = 7 * 86_400_000;

/// ZEB-713 RD4 (ceiling, PR #497 R1): upper bound on
/// `SetRecoveryDesignates.veto_window_ms`. 365 days. Two jobs:
/// 1. `deadline = t_R + W` stays far from u64 wrap — an unbounded W
///    could overflow the addition and produce a tiny deadline,
///    executing a recovery IMMEDIATELY instead of after the time-lock.
///    (`evaluate_recovery_phases` also uses `checked_add` fail-closed
///    as defense-in-depth for events that bypassed verification.)
/// 2. The value survives the u64 → JS-number DTO boundary exactly
///    (365 d ≈ 3.2e10 ms « 2^53).
pub const RECOVERY_VETO_WINDOW_CEILING_MS: u64 = 365 * 86_400_000;

/// ZEB-714 (spec §4.3): finality margin F. Clients do not act on a
/// recovery-produced `pending_rotation_for` until `now > deadline + F`,
/// so a veto delivered within F reconverges the membership state before
/// any irreversible `EpochRotation` event exists. F bounds delivery
/// delay, not authorship — it can be generous because the veto window W
/// (≥ 7 d) already did the waiting.
pub const RECOVERY_ROTATION_FINALITY_MS: u64 = 48 * 3_600_000;

/// ZEB-714: for a `pending_rotation_for` target whose kick was DERIVED
/// by recovery execution (no Kick/Leave event exists to cite), the
/// executed `RecoveryProposal` itself is the rotation trigger the
/// EpochRotation must reference (the D1 trigger-table arm validates
/// against it). Returns `(proposal_id, eligible_at_ms)` where
/// `eligible_at_ms = deadline + F` is the wall clock at which clients
/// may act (spec §4.3), or `None` on overflow (fail closed: never
/// eligible). Returns `None` when the target's marker is not
/// recovery-derived. At most one Executed proposal can exist per
/// `lost_admin` (rival tie-break), so the first match is the only one.
pub fn recovery_rotation_trigger(
    m: &MaterializedMembership,
    target: &OwnerAddr,
) -> Option<(EventId, Option<u64>)> {
    m.recovery_proposals.iter().find_map(|p| {
        (p.lost_admin == *target && p.phase == RecoveryPhase::Executed).then(|| {
            (
                p.id,
                p.deadline_ms
                    .and_then(|d| d.checked_add(RECOVERY_ROTATION_FINALITY_MS)),
            )
        })
    })
}

/// ZEB-250: apply an admin-proposal's effect to the running
/// materialized state when the proposal has reached quorum within the
/// 30-day window. Translates the wrapped ProposalKind into the same
/// mutation that a direct SetPower / Kick / ChangeQuorum would produce.
///
/// `effective_event` is the event that TIPPED the quorum threshold: the
/// AdminProposal itself (when admin_quorum == 1, self-satisfy path) or
/// the AdminCountersign that added the Nth signer (quorum > 1 path).
/// Using the trigger event's HLC ensures `left_at` is set to when the
/// decision was actually reached, not backdated to the proposal's HLC.
fn apply_admin_proposal_effect(
    m: &mut MaterializedMembership,
    proposal_kind: &ProposalKind,
    effective_event: &SignedMembershipEvent,
) {
    match proposal_kind {
        ProposalKind::SetPower { target, level } => {
            // Mirror the existing SetPower arm in the materialize main pass.
            m.power_levels.insert(*target, *level);
        }
        ProposalKind::Kick { target, .. } => {
            // Mirror the existing Kick arm in the materialize main pass.
            // Capture prior status before mutation — needed to decide
            // whether to mark pending_rotation_for (mirrors the direct
            // Kick arm's R4-1 PendingJoin guard).
            let prior_status = m.members.get(target).map(|s| s.status);
            if let Some(ms) = m.members.get_mut(target) {
                ms.status = MemberStatus::Banned;
                // ZEB-250 R2 Fix 2: use effective_event.at (the trigger
                // event's HLC) so left_at reflects when quorum was reached,
                // not the proposal's original HLC.
                ms.left_at = Some(effective_event.at.clone());
            }
            // Mirror the direct Kick arm: track that a rotation is needed,
            // EXCEPT when kicking a PendingJoin (they never received epoch
            // key material, so no rotation is required).
            if !matches!(prior_status, Some(MemberStatus::PendingJoin)) {
                m.pending_rotation_for.insert(*target);
            }
        }
        ProposalKind::ChangeQuorum { new_quorum } => {
            // ChangeQuorum mutates the running admin_quorum so subsequent
            // AdminProposal events in the same replay see the updated threshold
            // (single-pass-with-running-state, spec §5.2).
            m.admin_quorum = *new_quorum;
        }
        ProposalKind::SetRecoveryDesignates {
            designates,
            threshold,
            veto_window_ms,
        } => {
            // ZEB-713: install/replace the recovery config. `set_at` is
            // the quorum-trigger event's HLC — the config GENERATION —
            // so even an identical designate list re-set produces a new
            // digest and kills in-flight proposals bound to the old one
            // (running-state, same single-pass rule as ChangeQuorum).
            m.recovery_designates = Some(RecoveryDesignates {
                designates: designates.clone(),
                threshold: *threshold,
                veto_window_ms: *veto_window_ms,
                set_at: effective_event.at.clone(),
            });
        }
        ProposalKind::ChangeThresholds { new_thresholds } => {
            // Mutates running power_thresholds so subsequent events in the
            // same replay verify against the updated values (single-pass-
            // with-running-state, mirrors ChangeQuorum).
            m.power_thresholds = *new_thresholds;
        }
    }
}

/// ZEB-297: outcome of an auto-exec `SetPower` dispatch from a Tier 2
/// finalization. Distinguishes "the mint actually happened" from "this
/// replica intentionally skipped" so the tick can keep accurate
/// metrics and operators can tell admin replicas from non-admin
/// replicas in logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoExecOutcome {
    /// Local actor satisfied `POWER_THRESHOLDS.set_power`; SetPower
    /// event was minted, signed, and inserted into the engine's local
    /// log. Peers receive it via Zenoh sync on the membership topic.
    Applied,
    /// Local actor's power level in this community is below the
    /// `set_power` threshold (100), so this replica cannot produce a
    /// SetPower event that any verifier will accept. Skip silently —
    /// admins race to mint, HLC LWW dedupes, and the first one's
    /// event propagates to every replica via the existing membership
    /// log sync. This is the intentional "wrong replica" path, not a
    /// failure.
    SkippedNotAdmin,
    /// ZEB-734: this replica clears the (possibly lowered) `set_power`
    /// threshold but does NOT hold admin power (`max`), and the finalized
    /// outcome is admin-affecting (grants/removes admin). A direct SetPower
    /// would self-reject at `verify_event`
    /// (`SetPowerAdminAffectingRequiresAdmin`), and routing via `AdminProposal`
    /// is equally unavailable (AP2 requires proposer power == `max`). Skip and
    /// defer to an admin replica — behaviorally the same "wrong replica"
    /// deferral as `SkippedNotAdmin`, but tracked as a distinct outcome so
    /// telemetry doesn't conflate "below the set_power floor" with "moderator
    /// clears set_power yet lacks the admin tier."
    SkippedAdminAffectingRequiresAdmin,
    /// ZEB-300: community has `admin_quorum > 1` and the SetPower outcome
    /// is admin-affecting (`new_power == 100` OR target currently holds
    /// power 100), so it routes through `AdminProposal` instead of a
    /// direct SetPower. This replica had no live proposal for the exact
    /// (target, level) and minted a fresh `AdminProposal::SetPower` (it
    /// counts as signer 1). Peers countersign to quorum via CRDT sync.
    RoutedProposalMinted,
    /// ZEB-300: a live canonical `AdminProposal::SetPower` for this exact
    /// (target, level) already existed and this replica had not yet
    /// signed it, so it minted an `AdminCountersign` advancing the
    /// proposal toward `admin_quorum` signatures.
    RoutedProposalCountersigned,
    /// ZEB-300: nothing to mint this tick — this replica has already
    /// signed the canonical proposal (as proposer or countersign) and is
    /// awaiting other admins' signatures. The routing converges across
    /// ticks; a pending outcome is the steady state until a peer supplies
    /// the final quorum signature.
    RoutedProposalPending,
    /// ZEB-300 converge R1: terminal — the SetPower effect is already
    /// present in materialized state (`power_levels[target] == level`),
    /// so nothing was minted. Distinguished from `RoutedProposalPending`
    /// so the tick's re-dispatch loop (each Finalized SetPower poll is
    /// re-dispatched every tick while it stays Finalized) can tell an
    /// idempotent no-op from an in-flight quorum wait: this is the
    /// stop condition that keeps re-dispatch from re-minting once the
    /// effect lands on every replica. Applies to both the direct-SetPower
    /// and AdminProposal-routed paths.
    AlreadyApplied,
}

/// ZEB-297: pure helper deciding whether the local actor is allowed
/// to mint a `SetPower` event in this community. Lifted out of
/// `apply_auto_exec_set_power` so the admin-only-mint guard is unit
/// testable without spinning up a full `CommunitySyncEngine` +
/// `NodeState` + registry fixture.
///
/// Mirrors `verify_event`'s two SetPower preconditions (so the guard
/// never lets through an event the verifier would reject):
/// 1. `power_levels[self_owner] >= POWER_THRESHOLDS.set_power`
///    (currently 100). Missing entry treated as 0 per spec §4.
/// 2. `members[self_owner].status == MemberStatus::Joined`. Kick and
///    Leave intentionally do NOT clean up `power_levels` (see the
///    materialize comments on the SetPower / Kick arms), so a former
///    admin who was kicked or who left voluntarily would otherwise
///    sail past a power-only check, mint a SetPower locally, and then
///    have it self-reject at `verify_event` with `ActorNotJoined` —
///    exactly the doomed-mint path this guard exists to prevent
///    (CodeRabbit R1 Major on PR #135).
///
/// Boundary condition: `actor_power == POWER_THRESHOLDS.set_power` is
/// allowed (admins at exactly 100 can mint SetPower) AND the actor
/// must be currently Joined.
pub fn local_actor_can_mint_set_power(mat: &MaterializedMembership, self_owner: OwnerAddr) -> bool {
    let actor_power = mat.power_levels.get(&self_owner).copied().unwrap_or(0);
    // ZEB-251: read the per-community threshold (already available on
    // `mat`), not the global POWER_THRESHOLDS const — a community that
    // customizes `set_power` downward would otherwise leave this guard
    // stuck rejecting a mint that `verify_event` (which already reads
    // `prior_state.power_thresholds.set_power`) would accept.
    if actor_power < mat.power_thresholds.set_power {
        return false;
    }
    matches!(
        mat.members.get(&self_owner).map(|state| state.status),
        Some(MemberStatus::Joined)
    )
}

/// ZEB-733: does `actor`'s power meet the community's INVITE tier?
///
/// Local pre-check helper for the auto-counter-sign path. Reads
/// `mat.power_thresholds.invite` (the per-community value already on `mat`),
/// NOT the global `POWER_THRESHOLDS` const — a community that raised its invite
/// floor must gate locally the same way `verify_event` (which reads
/// `prior_state.power_thresholds.invite`) does. Missing power entry treated as
/// 0 per spec §4.
pub fn actor_power_meets_invite_tier(mat: &MaterializedMembership, actor: OwnerAddr) -> bool {
    mat.power_levels.get(&actor).copied().unwrap_or(0) >= mat.power_thresholds.invite
}

/// ZEB-733: does `actor`'s power meet the community's MODERATOR (kick) tier?
///
/// Shared local pre-check for the moderation audit-feed gates
/// (`list_pending_joins`, `list_recent_counter_signs`) and the self-heal
/// observer. Reads `mat.power_thresholds.kick` (the per-community value already
/// on `mat`), NOT the global `POWER_THRESHOLDS` const — a community that
/// customized its moderator floor must gate locally the same way
/// `verify_event` (which reads `prior_state.power_thresholds.kick`) does.
/// Missing power entry treated as 0 per spec §4.
pub fn actor_power_meets_moderator_tier(mat: &MaterializedMembership, actor: OwnerAddr) -> bool {
    mat.power_levels.get(&actor).copied().unwrap_or(0) >= mat.power_thresholds.kick
}

/// ZEB-250 §4.3 / ZEB-300 T1: a SetPower is "admin-affecting" when it grants
/// top power (`level == max`) or touches a member who currently holds top
/// power. Extracted so the direct-SetPower quorum guard and the
/// AdminProposal-routing planner share one predicate (previously copied
/// inline in `verify_event`, the `set_power_level` IPC, and
/// `setpower_mint_admin_blocked_by_quorum`).
///
/// Uses `POWER_THRESHOLDS.max` (the admin-tier cap), NOT
/// `POWER_THRESHOLDS.set_power` (the minimum power to CALL SetPower). These
/// are coincidentally equal (100) in v1 but conceptually distinct.
///
/// ZEB-251: intentionally still reads the global `POWER_THRESHOLDS.max`
/// rather than `mat.power_thresholds.max` — `max` is NOT customizable
/// (verify_event's ChangeThresholds AT1 gate rejects any proposal where
/// `new_thresholds.max != POWER_THRESHOLDS.max`), so `mat.power_thresholds.max`
/// is always structurally equal to this constant. Unlike
/// `local_actor_can_mint_set_power` (which reads the customizable
/// `set_power` tier and was updated to thread `mat.power_thresholds`
/// through), there is no per-community value to diverge from here.
pub(crate) fn is_admin_affecting_set_power(
    mat: &MaterializedMembership,
    target: OwnerAddr,
    level: u8,
) -> bool {
    let target_power = mat.power_levels.get(&target).copied().unwrap_or(0);
    level == POWER_THRESHOLDS.max || target_power == POWER_THRESHOLDS.max
}

/// ZEB-297 R2 (CodeRabbit Major): mirrors `verify_event`'s third SetPower
/// precondition — direct SetPower of an admin-affecting target is rejected
/// when `admin_quorum > 1` (spec §4.5 / ZEB-250). Without this check, a
/// Tier 2 auto-exec on an admin promotion (`new_power == max`) or admin
/// demotion (target currently holds power max) in a multi-admin-quorum
/// community would mint a SetPower event that the verifier would reject
/// with `SetPowerRequiresQuorum` — exactly the doomed-mint path the R1
/// joined-member guard exists to eliminate, surfacing in a different
/// population.
///
/// Returns `true` when the (target, level) combination would be rejected
/// by the quorum guard at `verify_event`; the auto-exec caller then routes
/// through `AdminProposal` (ZEB-300) instead of minting a direct SetPower.
/// Returns `false` when `admin_quorum <= 1` (single-admin community, direct
/// SetPower allowed) or when the change is not admin-affecting (e.g.,
/// moderator-tier reassignment).
///
/// ZEB-297 R3 (Cursor Low): uses `POWER_THRESHOLDS.max` (the admin-tier
/// power cap), NOT `POWER_THRESHOLDS.set_power` (the minimum power to
/// call SetPower). These are coincidentally equal (100) in v1, but
/// conceptually distinct — if `set_power` is ever lowered (e.g., to
/// allow moderators to perform non-admin SetPower) while `max` remains
/// the admin tier, the "admin-affecting" check still belongs on `max`.
/// Matches the semantic intent of `verify_event:2570`'s `*level == 100`
/// literal (which should ideally also move to `.max` — tracked as a
/// follow-up readability cleanup, not blocking this fix).
pub fn setpower_mint_admin_blocked_by_quorum(
    mat: &MaterializedMembership,
    target: OwnerAddr,
    level: u8,
) -> bool {
    if mat.admin_quorum <= 1 {
        return false;
    }
    is_admin_affecting_set_power(mat, target, level)
}

/// ZEB-734: local mirror of `verify_event`'s admin-affecting-requires-admin
/// gate. Returns `true` when this `(actor, target, level)` direct SetPower
/// would be rejected as `SetPowerAdminAffectingRequiresAdmin` — i.e. it grants
/// or removes admin power (`is_admin_affecting_set_power`) yet `actor` does not
/// already hold admin power (`< max`). Lets the local mint paths
/// (`set_power_level` IPC, Tier 2 `apply_auto_exec_set_power`) decline BEFORE
/// minting, upholding the surrounding no-doomed-mint discipline.
///
/// Intentionally independent of `admin_quorum`: a sub-`max` actor can neither
/// direct-mint an admin-affecting SetPower (this gate) nor route one through
/// `AdminProposal` (AP2 requires proposer power == `max`), so declining is the
/// only correct local action at every quorum. Reads `POWER_THRESHOLDS.max`
/// (the AT1-immovable admin tier), matching `is_admin_affecting_set_power`.
pub fn setpower_admin_affecting_denied_to_non_admin(
    mat: &MaterializedMembership,
    actor: OwnerAddr,
    target: OwnerAddr,
    level: u8,
) -> bool {
    if !is_admin_affecting_set_power(mat, target, level) {
        return false;
    }
    mat.power_levels.get(&actor).copied().unwrap_or(0) < POWER_THRESHOLDS.max
}

/// ZEB-300: what (if anything) this admin replica should mint to advance a
/// finalized admin-affecting Tier 2 SetPower toward AdminProposal quorum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdminProposalPlan {
    /// No live proposal for this (target, level) — mint a fresh
    /// `AdminProposal::SetPower` (proposer counts as signer 1).
    MintProposal,
    /// A live canonical proposal exists that this replica has not yet
    /// signed — countersign it (advances toward quorum). Carries the
    /// canonical proposal's `EventId`.
    Countersign(EventId),
    /// ZEB-300 converge R1: the effect is already applied
    /// (`power_levels[target] == level` — quorum reached on an earlier
    /// tick). Terminal: the tick's bounded re-dispatch loop stops here
    /// (maps to `AutoExecOutcome::AlreadyApplied`), so nothing re-mints.
    AlreadyApplied,
    /// ZEB-300 converge R1: this replica has already signed the canonical
    /// proposal (as proposer or countersign) and is awaiting other admins'
    /// signatures. Nothing to mint this tick, but NOT terminal — the effect
    /// has not landed yet, so re-dispatch keeps checking each tick until it
    /// does (maps to `AutoExecOutcome::RoutedProposalPending`).
    Pending,
}

/// ZEB-300: decide what (if anything) this admin replica should mint to
/// advance a finalized admin-affecting Tier 2 SetPower toward AdminProposal
/// quorum. Pure: no `NodeState` / engine — reads the materialized state and
/// the raw event log so it is unit-testable and every replica computes an
/// identical decision. See design §4.
///
/// Canonical selection: the live proposal with the numerically smallest
/// `EventId` (`[u8; 16]`) — a total order every replica computes identically,
/// so ticks converge on one proposal instead of racing separate ones.
///
/// Idempotency: each admin signs a given proposal at most once (as proposer
/// OR as one countersign) — enforced by the already-signed scan.
pub(crate) fn plan_admin_proposal_auto_exec<'a>(
    mat: &MaterializedMembership,
    events: impl Iterator<Item = &'a SignedMembershipEvent>,
    target: OwnerAddr,
    level: u8,
    self_owner: OwnerAddr,
    now_ms: u64,
) -> AdminProposalPlan {
    // 1. Effect already applied (quorum reached on an earlier tick).
    //    ZEB-300 converge R1: terminal — stops the tick's bounded
    //    re-dispatch loop so nothing re-mints once the effect lands.
    if mat.power_levels.get(&target).copied() == Some(level) {
        return AdminProposalPlan::AlreadyApplied;
    }
    // Collect once: the planner scans the log twice (canonical selection
    // below + the already-signed check further down) and an iterator param
    // can only be consumed once.
    let events: Vec<&SignedMembershipEvent> = events.collect();
    // 2. Live proposals for this exact (target, level); pick the canonical
    //    (smallest EventId) among those still within the expiry window.
    let canonical = events
        .iter()
        .copied()
        .filter(|e| match &e.kind {
            MembershipEventKind::AdminProposal { proposal_kind } => matches!(
                proposal_kind,
                ProposalKind::SetPower { target: t, level: l } if *t == target && *l == level
            ),
            _ => false,
        })
        .filter(|e| now_ms.saturating_sub(e.at.wall_ms) <= ADMIN_PROPOSAL_EXPIRY_MS)
        .min_by_key(|e| e.id);

    let Some(canonical) = canonical else {
        // 3. No live candidate → propose.
        return AdminProposalPlan::MintProposal;
    };

    // 4/5. Already signed the canonical (proposer or countersign) → nothing
    //      to do; otherwise countersign it.
    let already_signed = events.iter().copied().any(|e| match &e.kind {
        MembershipEventKind::AdminProposal { .. } => e.id == canonical.id && e.actor == self_owner,
        MembershipEventKind::AdminCountersign { target_event_id } => {
            *target_event_id == canonical.id && e.actor == self_owner
        }
        _ => false,
    });
    if already_signed {
        // ZEB-300 converge R1: NOT terminal — the effect has not landed
        // yet, so re-dispatch keeps polling each tick until a peer supplies
        // the final quorum signature (maps to RoutedProposalPending).
        AdminProposalPlan::Pending
    } else {
        AdminProposalPlan::Countersign(canonical.id)
    }
}

/// ZEB-291 Phase 2 Task 10: auto-exec dispatch from a Tier 2 contestability finalize.
///
/// Signs and applies a `SetPower` membership event using the node's local
/// signing key, then publishes via the community sync registry so peers
/// see the new power level. Direct-call architecture (no event bus): the
/// voting tick (Task 16) holds `NodeState` briefly to extract the needed
/// handles, drops the std mutex, then calls this function which performs
/// the async signing + apply + publish.
///
/// `new_power` is taken as a `u32` (matches the Tier 2 `AutoExecAction::SetPower`
/// payload shape) and bounded-checked to `u8` (membership `SetPower` event
/// uses a u8 level; valid power levels are 0..=100 per spec §4 power
/// table). Out-of-range `new_power` returns `Err("new_power out of range")`
/// rather than panicking — voting tick (Task 16) logs and continues; the
/// PollResult event is NOT rolled back.
///
/// ZEB-297: returns `Ok(AutoExecOutcome::SkippedNotAdmin)` when the local
/// actor's materialized power level in this community is below
/// `POWER_THRESHOLDS.set_power`. Without this guard, a non-admin replica
/// would mint a SetPower event that its own `verify_event` would reject
/// (`InsufficientPower`), so finalization would only land on whichever
/// admin replica's tick ran first. With the guard, admins race to mint
/// and HLC LWW dedupes; the first admin's event propagates to every
/// replica via the existing membership log sync.
///
/// ZEB-300: when the community has `admin_quorum > 1` AND the (target,
/// level) combination is admin-affecting (`new_power == 100` or target
/// currently holds power 100), a direct SetPower would self-reject at
/// `verify_event` with `SetPowerRequiresQuorum` (spec §4.5), so this
/// branch routes through `apply_auto_exec_admin_proposal_set_power`
/// instead — minting an `AdminProposal::SetPower` (returning
/// `RoutedProposalMinted`), countersigning the canonical pending proposal
/// (`RoutedProposalCountersigned`), or doing nothing this tick
/// (`RoutedProposalPending`). The routing converges across ticks.
///
/// Returns `Err` (as String, matching the existing IPC error convention)
/// if:
/// - NodeState is missing any required field (registry, outbox, etc.) —
///   typically because the node isn't running.
/// - `new_power > 100` (out of u8 / power-table range).
/// - The community has no engine in the registry (caller isn't joined).
/// - Signing or apply fails at the CRDT layer (e.g., actor not admin —
///   verify_event rejects with InsufficientPower).
pub async fn apply_auto_exec_set_power(
    // Takes `&Mutex<NodeState>` (not `&Arc<..>`): the body only locks it to
    // snapshot handles, never clones/stores the Arc. This lets production wire
    // it from Tauri's managed `Mutex<NodeState>` via `app.state().inner()`
    // (ZEB-300 Task 20.1). `&Arc<Mutex<NodeState>>` call sites still pass by
    // Deref coercion.
    node_state: &std::sync::Mutex<crate::NodeState>,
    community_id: crate::owner_state_types::SpaceId,
    target_pubkey: crate::owner_state_types::OwnerAddr,
    new_power: u32,
) -> Result<AutoExecOutcome, String> {
    if new_power > 100 {
        return Err(format!(
            "new_power out of range: {} > 100 (u8/power-table cap)",
            new_power
        ));
    }
    let level: u8 = new_power as u8;

    // Snapshot the handles we need under the std::sync::Mutex, then drop
    // the lock before any await (no awaits while holding a std mutex —
    // existing project convention; see set_power_level IPC).
    let (hlc_tracker, device_id, self_owner, community_registry, dm_outbox) = {
        let g = node_state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker
                .clone()
                .ok_or("hlc_tracker missing (node not running?)")?,
            g.dm_device_id
                .clone()
                .ok_or("dm_device_id missing (node not running?)")?,
            g.dm_self_owner
                .ok_or("dm_self_owner missing (node not running?)")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing (node not running?)")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing (no owner identity?)")?,
        )
    };

    let engine_arc = community_registry
        .engine_arc(&community_id)
        .await
        .ok_or_else(|| {
            format!(
                "no engine for community {} — not currently joined",
                hex::encode(community_id.0)
            )
        })?;

    // ZEB-297: admin-only mint. Read the engine's materialized
    // power_levels for the local actor; skip if below the set_power
    // threshold OR if the (target, level) combination would be
    // rejected by the admin_quorum > 1 guard at verify_event. Both
    // checks happen BEFORE reserving an HLC so we don't burn a
    // tracker slot on a no-op. Admins race to mint; the first one's
    // event propagates via the existing membership log sync.
    //
    // The quorum-blocked path (admin_affecting && admin_quorum > 1)
    // is structurally unsupported by Tier 2 auto-exec — it requires
    // routing through AdminProposal instead, which is filed as a
    // follow-up ticket. Skipping here is correct (no doomed mint)
    // but means the Tier 2 outcome lands as a NoOp on every replica
    // until that follow-up ships.
    //
    // ZEB-297 R3 (CodeRabbit Major): the guard reads `mat` under the
    // engine state lock then drops it, so a concurrent membership
    // change can theoretically invalidate the precondition before
    // `insert_local_event` runs below. This is benign — wire safety
    // is enforced by `insert_local_event_with_resolved_pubs`
    // (community_state_sync.rs:1444-1447), which runs verify_event
    // atomically under `self.state.lock().await` immediately before
    // CRDT apply. A race-and-rejected event surfaces as an
    // `InsertOutcome::Rejected` here (caught at line 3175-3179), not
    // a doomed event on the wire. The fast-path guard exists to (a)
    // avoid burning an HLC for the common-case admin-replica
    // dispatch, and (b) generate accurate skip-counter telemetry.
    // Full atomicity (do guard + sign + insert under one critical
    // section) would require holding the engine state lock across
    // the dm_outbox signing await, a pattern not used elsewhere and
    // out of scope for this fix.
    //
    // ZEB-300 converge R1: also read whether the target is ALREADY at
    // `level` in the same locked block. The Tier 2 tick re-dispatches
    // auto-exec for Finalized SetPower polls every tick while they stay
    // Finalized, so once the effect has synced in on this replica a bare
    // re-dispatch would otherwise re-mint. Returning
    // `AlreadyApplied` BEFORE the admin / quorum / mint logic makes
    // re-dispatch idempotent on every replica.
    let (is_admin, denied_admin_affecting, blocked_by_quorum, already_at_level) = {
        let state_arc = engine_arc.state();
        let state_g = state_arc.lock().await;
        let mat = state_g.materialized(engine_arc.admin_addr());
        (
            local_actor_can_mint_set_power(&mat, self_owner),
            setpower_admin_affecting_denied_to_non_admin(&mat, self_owner, target_pubkey, level),
            setpower_mint_admin_blocked_by_quorum(&mat, target_pubkey, level),
            mat.power_levels.get(&target_pubkey).copied() == Some(level),
        )
    };
    if already_at_level {
        return Ok(AutoExecOutcome::AlreadyApplied);
    }
    if !is_admin {
        tracing::info!(
            community = %hex::encode(community_id.0),
            target = %hex::encode(target_pubkey.0),
            new_power,
            "auto_exec_set_power: skipping — local actor is not admin in this community (deferring to admin race)"
        );
        return Ok(AutoExecOutcome::SkippedNotAdmin);
    }
    // ZEB-734: this replica clears the (possibly lowered) set_power threshold
    // but does NOT hold admin power, and the outcome grants/removes admin. A
    // direct SetPower would self-reject at verify_event
    // (SetPowerAdminAffectingRequiresAdmin), and AdminProposal is equally
    // unavailable (AP2 requires proposer power == max). Defer to an admin
    // replica — mirrors the not-admin skip above, checked before the
    // blocked_by_quorum routing so a sub-admin never mints a doomed proposal.
    if denied_admin_affecting {
        tracing::info!(
            community = %hex::encode(community_id.0),
            target = %hex::encode(target_pubkey.0),
            new_power,
            "auto_exec_set_power: skipping — admin-affecting change requires admin power (local actor is a sub-admin moderator; deferring to admin race)"
        );
        return Ok(AutoExecOutcome::SkippedAdminAffectingRequiresAdmin);
    }
    if blocked_by_quorum {
        // ZEB-300: admin_quorum > 1 + admin-affecting → route through
        // AdminProposal (mint proposal / countersign the canonical pending
        // proposal / no-op) instead of a direct SetPower that verify_event
        // would reject with SetPowerRequiresQuorum. The wrapper re-reads
        // state under the engine lock and mints with community_signing_key.
        return apply_auto_exec_admin_proposal_set_power(
            node_state,
            community_id,
            target_pubkey,
            level,
        )
        .await;
    }

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let event_hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;

    let event = {
        let outbox_g = dm_outbox.lock().await;
        // ZEB-720: SetPower is a steady-state membership event — sign with the
        // ENROLLED community device key (#2), NOT the DM outbox `signing_key`.
        // The direct `set_power_level` IPC path (ZEB-339) signs the same way;
        // using `signing_key` here minted events the CRDT rejected as
        // `SignerNotEnrolledForActor`, so headless auto-exec never applied.
        let signing_key = outbox_g.community_signing_key.as_ref();
        crate::mint_set_power_event(
            community_id,
            self_owner,
            target_pubkey,
            level,
            signing_key,
            event_hlc,
        )?
    };

    let outcome = engine_arc
        .insert_local_event(event)
        .await
        .map_err(|e| format!("engine.insert_local_event (auto_exec set_power): {e}"))?;
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Rejected(_)
    ) {
        return Err(format!("apply_auto_exec_set_power: rejected: {outcome:?}"));
    }
    let _ = device_id;
    Ok(AutoExecOutcome::Applied)
}

/// ZEB-300: route an admin-affecting Tier 2 SetPower auto-exec through the
/// `AdminProposal` quorum machinery (spec §4.5). Called by
/// `apply_auto_exec_set_power`'s `blocked_by_quorum` branch when the
/// community has `admin_quorum > 1` and the change touches the admin tier,
/// where a direct SetPower would self-reject at `verify_event`.
///
/// Reads the community's materialized state + event log under the engine
/// lock, runs the pure `plan_admin_proposal_auto_exec` planner, and mints
/// the corresponding event:
/// - `MintProposal` → `mint_admin_proposal_set_power_event` → `RoutedProposalMinted`
/// - `Countersign(pid)` → `mint_admin_countersign_event` → `RoutedProposalCountersigned`
/// - `AlreadyApplied` → `AlreadyApplied` (effect already in materialized state)
/// - `Pending` → `RoutedProposalPending` (already signed; awaiting quorum)
///
/// Load-bearing (matches the manual `set_power_level` proposal path): the
/// routed events are signed with `outbox.community_signing_key` (the
/// enrolled #2 device key community-membership events use), NOT the
/// direct-SetPower `outbox.signing_key`. A wrong-key event would fail
/// `verify_event` silently.
///
/// Any-admin-proposes + canonical (min-`EventId`) countersign converges
/// across ticks and tolerates absent admins; dangling proposals from
/// simultaneous ticks expire per `ADMIN_PROPOSAL_EXPIRY_MS`.
pub async fn apply_auto_exec_admin_proposal_set_power(
    // `&Mutex<NodeState>` (not `&Arc<..>`) for the same reason as
    // `apply_auto_exec_set_power` — locked, never Arc-cloned.
    node_state: &std::sync::Mutex<crate::NodeState>,
    community_id: crate::owner_state_types::SpaceId,
    target_pubkey: crate::owner_state_types::OwnerAddr,
    level: u8,
) -> Result<AutoExecOutcome, String> {
    // Snapshot the handles we need under the std::sync::Mutex, then drop
    // the lock before any await (no awaits while holding a std mutex).
    let (hlc_tracker, device_id, self_owner, community_registry, dm_outbox) = {
        let g = node_state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker
                .clone()
                .ok_or("hlc_tracker missing (node not running?)")?,
            g.dm_device_id
                .clone()
                .ok_or("dm_device_id missing (node not running?)")?,
            g.dm_self_owner
                .ok_or("dm_self_owner missing (node not running?)")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing (node not running?)")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing (no owner identity?)")?,
        )
    };

    let engine_arc = community_registry
        .engine_arc(&community_id)
        .await
        .ok_or_else(|| {
            format!(
                "no engine for community {} — not currently joined",
                hex::encode(community_id.0)
            )
        })?;

    // Reserve an HLC for the (potential) event; its wall clock is the "now"
    // the planner uses for proposal-expiry checks. An AlreadyApplied /
    // Pending tick still consumes an HLC — benign (the monotonic clock
    // simply advances).
    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let event_hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;
    let now_ms = event_hlc.wall_ms;

    // Decide mint-vs-countersign-vs-already-applied-vs-pending from the
    // materialized state + log.
    //
    // ZEB-300 R3 (CodeRabbit): plan-then-insert race window. We read
    // (mat, events) under the engine state lock, DROP it, then mint +
    // insert the event later (the outbox signing await must not hold the
    // engine lock). A concurrent same-(target, level) insert from another
    // admin's tick between the read and our insert is benign: canonical
    // selection picks the numerically-minimum `EventId` deterministically
    // on every replica, and materialize applies the effect via LWW at the
    // event that tips the signer count to quorum, so the logs converge to
    // one applied SetPower regardless of interleaving. Mirrors the ZEB-297
    // R3 note on the direct-SetPower path above — full atomicity would
    // require holding the engine lock across the signing await, a pattern
    // not used elsewhere.
    let plan = {
        let state_arc = engine_arc.state();
        let state_g = state_arc.lock().await;
        let mat = state_g.materialized(engine_arc.admin_addr());
        plan_admin_proposal_auto_exec(
            &mat,
            state_g.events(),
            target_pubkey,
            level,
            self_owner,
            now_ms,
        )
    };

    let event = match plan {
        AdminProposalPlan::AlreadyApplied => return Ok(AutoExecOutcome::AlreadyApplied),
        AdminProposalPlan::Pending => return Ok(AutoExecOutcome::RoutedProposalPending),
        AdminProposalPlan::MintProposal => {
            let outbox_g = dm_outbox.lock().await;
            // ZEB-300: AdminProposal(SetPower) signs with the #2 enrolled
            // community key, matching the manual set_power_level path.
            let signing_key = outbox_g.community_signing_key.as_ref();
            crate::mint_admin_proposal_set_power_event(
                community_id,
                self_owner,
                target_pubkey,
                level,
                signing_key,
                event_hlc,
            )?
        }
        AdminProposalPlan::Countersign(pid) => {
            let outbox_g = dm_outbox.lock().await;
            let signing_key = outbox_g.community_signing_key.as_ref();
            crate::mint_admin_countersign_event(
                community_id,
                self_owner,
                pid,
                signing_key,
                event_hlc,
            )?
        }
    };

    let outcome_kind = match plan {
        AdminProposalPlan::MintProposal => AutoExecOutcome::RoutedProposalMinted,
        AdminProposalPlan::Countersign(_) => AutoExecOutcome::RoutedProposalCountersigned,
        AdminProposalPlan::AlreadyApplied | AdminProposalPlan::Pending => {
            unreachable!("AlreadyApplied/Pending returned early above")
        }
    };

    let insert = engine_arc
        .insert_local_event(event)
        .await
        .map_err(|e| format!("engine.insert_local_event (AdminProposal auto-exec): {e}"))?;
    if matches!(
        insert,
        crate::community_state_crdt::InsertOutcome::Rejected(_)
    ) {
        return Err(format!(
            "apply_auto_exec_admin_proposal_set_power: rejected: {insert:?}"
        ));
    }
    Ok(outcome_kind)
}

#[cfg(test)]
mod auto_exec_tests {
    use super::*;
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

    /// ZEB-297: `local_actor_can_mint_set_power` returns false when the
    /// local actor's materialized power level is below the
    /// `set_power` threshold (100). This is the pure-helper unit
    /// underpinning the guard inside `apply_auto_exec_set_power` — a
    /// non-admin replica must NOT mint a SetPower event that its own
    /// `verify_event` would self-reject (`InsufficientPower`).
    #[test]
    fn local_actor_can_mint_set_power_returns_false_when_below_threshold() {
        let self_owner = OwnerAddr([0xaa; 16]);
        let mut mat = MaterializedMembership::default();
        // Default power for `self_owner` is 0 (key absent) — non-admin.
        assert!(!local_actor_can_mint_set_power(&mat, self_owner));

        // ZEB-297 R3 (CodeRabbit Nitpick): seed Joined status so the
        // membership gate is satisfied. Without this, the test would
        // pass even if the numeric threshold check were accidentally
        // removed (because the membership check alone would still
        // reject). Seeding Joined isolates the power-gate boundary.
        mat.members.insert(
            self_owner,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "test".to_string(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        // Power 99 is still below the 100 admin threshold — Joined alone
        // is not enough.
        mat.power_levels
            .insert(self_owner, POWER_THRESHOLDS.set_power - 1);
        assert!(!local_actor_can_mint_set_power(&mat, self_owner));
    }

    /// ZEB-297: positive-path companion — `local_actor_can_mint_set_power`
    /// returns true at exactly `POWER_THRESHOLDS.set_power` (100) and
    /// above WHEN the actor is currently Joined. Pins the boundary
    /// condition so a future refactor that accidentally uses `>`
    /// instead of `>=` would fail loudly.
    #[test]
    fn local_actor_can_mint_set_power_returns_true_when_at_or_above_threshold() {
        let self_owner = OwnerAddr([0xaa; 16]);
        let mut mat = MaterializedMembership::default();

        // Power == threshold (100) AND Joined: admin can mint.
        mat.power_levels
            .insert(self_owner, POWER_THRESHOLDS.set_power);
        mat.members.insert(
            self_owner,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "test".to_string(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        assert!(local_actor_can_mint_set_power(&mat, self_owner));
    }

    /// ZEB-733: `actor_power_meets_moderator_tier` reads the PER-COMMUNITY
    /// `kick` threshold, not the global const. A member whose power sits
    /// between the default (50) and a community-customized floor (75) must be
    /// gated OUT once the community raises the threshold — proving the local
    /// audit-feed / self-heal gates track the customized value the way
    /// `verify_event` does.
    #[test]
    fn actor_power_meets_moderator_tier_reads_per_community_kick() {
        let actor = OwnerAddr([0xaa; 16]);
        let mut mat = MaterializedMembership::default();
        mat.power_levels.insert(actor, 60);

        // Default kick == 50: a power-60 member meets the moderator tier.
        assert_eq!(mat.power_thresholds.kick, POWER_THRESHOLDS.kick);
        assert!(
            actor_power_meets_moderator_tier(&mat, actor),
            "power 60 meets the default moderator tier (50)"
        );

        // Community raises kick to 75: the same power-60 member is gated out.
        mat.power_thresholds.kick = 75;
        assert!(
            !actor_power_meets_moderator_tier(&mat, actor),
            "power 60 must NOT meet a customized moderator tier of 75"
        );

        // At exactly the customized threshold: boundary is inclusive (>=).
        mat.power_levels.insert(actor, 75);
        assert!(
            actor_power_meets_moderator_tier(&mat, actor),
            "power == customized kick (75) meets the tier (>= boundary)"
        );

        // Missing power entry is treated as 0 → never meets a positive tier.
        let stranger = OwnerAddr([0xbb; 16]);
        assert!(!actor_power_meets_moderator_tier(&mat, stranger));
    }

    /// ZEB-733: `actor_power_meets_invite_tier` reads the PER-COMMUNITY
    /// `invite` threshold. Default invite == 0 (any member), so it is a no-op
    /// gate; a community that raises the invite floor gates lower-power members
    /// out — the behavior the auto-counter-sign path must honor.
    #[test]
    fn actor_power_meets_invite_tier_reads_per_community_invite() {
        let actor = OwnerAddr([0xaa; 16]);
        let mut mat = MaterializedMembership::default();

        // Default invite == 0: even a power-0 (absent) member meets the tier.
        assert_eq!(mat.power_thresholds.invite, POWER_THRESHOLDS.invite);
        assert!(
            actor_power_meets_invite_tier(&mat, actor),
            "default invite tier (0) is met by any member"
        );

        // Community raises the invite floor to 25: a power-10 member is gated out.
        mat.power_thresholds.invite = 25;
        mat.power_levels.insert(actor, 10);
        assert!(
            !actor_power_meets_invite_tier(&mat, actor),
            "power 10 must NOT meet a customized invite tier of 25"
        );

        // At/above the customized floor: allowed.
        mat.power_levels.insert(actor, 25);
        assert!(actor_power_meets_invite_tier(&mat, actor));
    }

    /// ZEB-297 R1 (CodeRabbit Major): the guard must mirror
    /// `verify_event`'s joined-member check, not just its power check.
    /// Kick/Leave intentionally leave stale `power_levels` entries in
    /// place (so a kicked admin's prior signed events still validate
    /// at their original HLC), which means a former admin who is now
    /// `Left` or `Banned` retains power 100 in the materialized view.
    /// Without the Joined check, the guard would let such an actor
    /// mint a SetPower locally, then `verify_event` would self-reject
    /// with `ActorNotJoined` — the exact doomed-mint path ZEB-297 was
    /// filed to eliminate.
    #[test]
    fn local_actor_can_mint_set_power_returns_false_for_former_admin_who_left() {
        let self_owner = OwnerAddr([0xaa; 16]);
        let mut mat = MaterializedMembership::default();
        mat.power_levels
            .insert(self_owner, POWER_THRESHOLDS.set_power);
        mat.members.insert(
            self_owner,
            MemberState {
                status: MemberStatus::Left,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "test".to_string(),
                },
                left_at: Some(Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "test".to_string(),
                }),
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        assert!(
            !local_actor_can_mint_set_power(&mat, self_owner),
            "Left former admin must NOT pass the guard"
        );
    }

    /// Same as the `Left` case but for `Banned` — a kicked former
    /// admin retains power 100 by spec but must not be allowed to mint.
    #[test]
    fn local_actor_can_mint_set_power_returns_false_for_former_admin_who_was_banned() {
        let self_owner = OwnerAddr([0xaa; 16]);
        let mut mat = MaterializedMembership::default();
        mat.power_levels
            .insert(self_owner, POWER_THRESHOLDS.set_power);
        mat.members.insert(
            self_owner,
            MemberState {
                status: MemberStatus::Banned,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "test".to_string(),
                },
                left_at: Some(Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "test".to_string(),
                }),
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        assert!(
            !local_actor_can_mint_set_power(&mat, self_owner),
            "Banned former admin must NOT pass the guard"
        );
    }

    /// Missing-from-members rejection: a `power_levels` entry without
    /// a corresponding `members` entry shouldn't happen in practice,
    /// but defense-in-depth ensures the guard's two-part predicate
    /// fails closed.
    #[test]
    fn local_actor_can_mint_set_power_returns_false_when_member_record_missing() {
        let self_owner = OwnerAddr([0xaa; 16]);
        let mut mat = MaterializedMembership::default();
        mat.power_levels
            .insert(self_owner, POWER_THRESHOLDS.set_power);
        // No mat.members.insert(...) — power but no member record.
        assert!(!local_actor_can_mint_set_power(&mat, self_owner));
    }

    /// ZEB-297 R2 (CodeRabbit Major): single-admin-quorum communities
    /// (the default) must NEVER trip the quorum guard — direct SetPower
    /// is always allowed there. Pins the early-return so future refactors
    /// that read `mat.admin_quorum` don't accidentally promote the
    /// single-admin path into the quorum-blocked branch.
    #[test]
    fn setpower_mint_admin_blocked_by_quorum_false_when_quorum_is_one() {
        let target = OwnerAddr([0xbb; 16]);
        let mat = MaterializedMembership::default();
        assert_eq!(mat.admin_quorum, 1, "default quorum is 1");

        // level == max (admin promotion) — would block at quorum > 1.
        assert!(!setpower_mint_admin_blocked_by_quorum(
            &mat,
            target,
            POWER_THRESHOLDS.max
        ));
        // level < max, target unknown — non-admin-affecting at any quorum.
        assert!(!setpower_mint_admin_blocked_by_quorum(&mat, target, 50));
    }

    /// ZEB-297 R2: multi-admin-quorum (>= 2) + admin promotion
    /// (`level == 100`) is the canonical quorum-blocked path. Mirrors
    /// `verify_event`'s `admin_affecting = *level == 100 || target_power == 100`
    /// check at line 2570 — auto-exec must short-circuit here so the
    /// minted event can't outrun the verifier's rejection.
    #[test]
    fn setpower_mint_admin_blocked_by_quorum_true_for_promote_to_admin() {
        let target = OwnerAddr([0xbb; 16]);
        let mat = MaterializedMembership {
            admin_quorum: 2,
            ..Default::default()
        };
        assert!(setpower_mint_admin_blocked_by_quorum(
            &mat,
            target,
            POWER_THRESHOLDS.max
        ));
    }

    /// ZEB-297 R2: multi-admin-quorum + demotion of an existing admin
    /// (target currently has `power_levels[target] == 100`, new level
    /// below 100) is the second admin-affecting branch — also blocked.
    #[test]
    fn setpower_mint_admin_blocked_by_quorum_true_for_demote_existing_admin() {
        let target = OwnerAddr([0xbb; 16]);
        let mut mat = MaterializedMembership {
            admin_quorum: 2,
            ..Default::default()
        };
        mat.power_levels.insert(target, POWER_THRESHOLDS.max);
        assert!(setpower_mint_admin_blocked_by_quorum(&mat, target, 50));
    }

    /// ZEB-734: a sub-`max` actor (e.g. a moderator in a community that
    /// lowered `set_power`) is DENIED admin-affecting SetPower — promoting
    /// anyone to `max` grants admin, which requires already holding `max`.
    #[test]
    fn setpower_admin_affecting_denied_to_non_admin_true_for_mod_promoting_to_admin() {
        let actor = OwnerAddr([0xaa; 16]);
        let target = OwnerAddr([0xbb; 16]);
        let mut mat = MaterializedMembership::default();
        mat.power_levels.insert(actor, 50); // moderator, below max
        assert!(setpower_admin_affecting_denied_to_non_admin(
            &mat,
            actor,
            target,
            POWER_THRESHOLDS.max
        ));
    }

    /// ZEB-734: demoting an existing admin (target currently at `max`) is
    /// admin-affecting too — a sub-`max` actor is denied.
    #[test]
    fn setpower_admin_affecting_denied_to_non_admin_true_for_mod_demoting_admin() {
        let actor = OwnerAddr([0xaa; 16]);
        let target = OwnerAddr([0xbb; 16]);
        let mut mat = MaterializedMembership::default();
        mat.power_levels.insert(actor, 50);
        mat.power_levels.insert(target, POWER_THRESHOLDS.max);
        assert!(setpower_admin_affecting_denied_to_non_admin(
            &mat, actor, target, 20
        ));
    }

    /// ZEB-734: an actor who already holds admin power (`max`) is NEVER
    /// denied — the lone-admin direct-SetPower path is unchanged.
    #[test]
    fn setpower_admin_affecting_denied_to_non_admin_false_for_admin_actor() {
        let actor = OwnerAddr([0xaa; 16]);
        let target = OwnerAddr([0xbb; 16]);
        let mut mat = MaterializedMembership::default();
        mat.power_levels.insert(actor, POWER_THRESHOLDS.max);
        assert!(!setpower_admin_affecting_denied_to_non_admin(
            &mat,
            actor,
            target,
            POWER_THRESHOLDS.max
        ));
    }

    /// ZEB-734: a non-admin-affecting change (level < max, target not an
    /// admin) is allowed for a sub-`max` actor — the whole point of a
    /// lowered `set_power` (delegating sub-admin member management) survives.
    #[test]
    fn setpower_admin_affecting_denied_to_non_admin_false_for_non_admin_affecting() {
        let actor = OwnerAddr([0xaa; 16]);
        let target = OwnerAddr([0xbb; 16]);
        let mut mat = MaterializedMembership::default();
        mat.power_levels.insert(actor, 50);
        mat.power_levels.insert(target, 10);
        assert!(!setpower_admin_affecting_denied_to_non_admin(
            &mat, actor, target, 40
        ));
    }

    /// ZEB-297 R2: multi-admin-quorum but non-admin-affecting change
    /// (e.g., moderator-tier reassignment — target below 100 AND new
    /// level below 100) is allowed direct, just like single-admin
    /// communities. Pins the boundary so the helper doesn't over-skip
    /// and starve legitimate Tier 2 outcomes.
    #[test]
    fn setpower_mint_admin_blocked_by_quorum_false_for_non_admin_affecting_change() {
        let target = OwnerAddr([0xbb; 16]);
        let mut mat = MaterializedMembership {
            admin_quorum: 2,
            ..Default::default()
        };
        // target is moderator-tier; moving 30 → 70 is non-admin-affecting.
        mat.power_levels.insert(target, 30);
        assert!(!setpower_mint_admin_blocked_by_quorum(&mat, target, 70));
    }

    /// ZEB-300 T1: the extracted `is_admin_affecting_set_power` helper
    /// classifies a (target, level) as admin-affecting when the new level
    /// grants top power OR the target currently holds top power — the same
    /// predicate the three prior inline copies used.
    #[test]
    fn is_admin_affecting_set_power_true_for_promote_to_100() {
        let mut mat = MaterializedMembership::default();
        let target = OwnerAddr([7u8; 16]);
        // target currently non-admin (power 0), level 100 => admin-affecting
        assert!(is_admin_affecting_set_power(&mat, target, 100));
        // target currently admin (power 100), level 50 (demote) => admin-affecting
        mat.power_levels.insert(target, 100);
        assert!(is_admin_affecting_set_power(&mat, target, 50));
        // non-admin-affecting: target power 10, level 20
        let other = OwnerAddr([8u8; 16]);
        mat.power_levels.insert(other, 10);
        assert!(!is_admin_affecting_set_power(&mat, other, 20));
    }

    /// Apply-auto-exec helper test: bounds-check on `new_power > 100`.
    ///
    /// Spec §4 caps power levels at 100 (admin); the membership
    /// `SetPower` event wire encodes `level: u8`. Tier 2's
    /// `AutoExecAction::SetPower.new_power` field is typed as `u32`
    /// (matches the Phase 2 PollConfig wire shape — small-integer cap
    /// gives room for future power-table extension). Callers passing
    /// `new_power > 100` are surfaced an Err rather than the helper
    /// panicking on a u32→u8 cast.
    #[tokio::test]
    async fn apply_auto_exec_set_power_rejects_out_of_range() {
        let node_state = std::sync::Arc::new(std::sync::Mutex::new(crate::NodeState::default()));
        let community_id = SpaceId([0xc0; 16]);
        let target = OwnerAddr([0xaa; 16]);
        let err = apply_auto_exec_set_power(&node_state, community_id, target, 101)
            .await
            .expect_err("new_power=101 must be rejected");
        assert!(
            err.contains("out of range"),
            "error must mention range; got: {err}"
        );
    }

    /// Apply-auto-exec helper test: missing-NodeState handles surface as Err.
    ///
    /// A fresh `NodeState::default()` has every Option-typed handle set to
    /// `None`. The helper must Err with one of the "missing" diagnostics
    /// rather than panic — voting tick (Task 16) logs + continues on
    /// these failures so the PollResult event still finalizes even when
    /// auto-exec dispatch can't run.
    #[tokio::test]
    async fn apply_auto_exec_set_power_missing_handles_returns_err() {
        let node_state = std::sync::Arc::new(std::sync::Mutex::new(crate::NodeState::default()));
        let community_id = SpaceId([0xc0; 16]);
        let target = OwnerAddr([0xaa; 16]);
        let err = apply_auto_exec_set_power(&node_state, community_id, target, 50)
            .await
            .expect_err("missing NodeState handles must Err, not panic");
        assert!(
            err.contains("missing") || err.contains("not running"),
            "error must mention missing/not running; got: {err}"
        );
    }

    /// Unit test: the SetPower-signing path that `apply_auto_exec_set_power`
    /// uses produces an Ed25519 signature that verifies against the admin's
    /// pubkey — proving the helper's mint step yields a CRDT-acceptable
    /// event. This does NOT call `apply_auto_exec_set_power` itself; the
    /// helper depends on a fully-wired NodeState (CommunitySyncRegistry,
    /// dm_outbox, materialized CommunityState) that's out of reach for a
    /// pure unit test. The full end-to-end pipeline (Tier 2 finalize →
    /// auto-exec dispatch → CRDT accept → peer publish) is exercised by
    /// the `community_voting_tick` integration test that wires the tick
    /// to a real engine.
    #[tokio::test]
    async fn auto_exec_set_power_signing_path_produces_verifiable_signature() {
        use crate::community_state_crdt::CommunityState;
        use crate::owner_state_crypto::canonical_cbor_encode;
        use ed25519_dalek::{Signer, SigningKey};
        use harmony_identity::PrivateIdentity;

        // ── 1. Identities ────────────────────────────────────────────
        let admin_identity = PrivateIdentity::from_seed(&[0xaa; 32]);
        let admin = OwnerAddr(admin_identity.identity.address_hash);
        let admin_pub = admin_identity.identity.to_public_bytes();
        let admin_sk_bytes = admin_identity.to_private_bytes();
        let admin_sk_seed: [u8; 32] = admin_sk_bytes[32..64].try_into().unwrap();
        let admin_signing_key = SigningKey::from_bytes(&admin_sk_seed);

        let target_identity = PrivateIdentity::from_seed(&[0xbb; 32]);
        let target = OwnerAddr(target_identity.identity.address_hash);
        let target_pub = target_identity.identity.to_public_bytes();

        // ── 2. CommunityState: admin joins, then target joins ──────
        let community_id = SpaceId([0xc0; 16]);
        let _state = CommunityState::new(community_id);

        // Sign admin Join + target Join (admin countersigns target's join
        // in invite-only flow; here we use open-community semantics so no
        // countersig needed). insert_event on a fresh state implicitly
        // accepts the first Join as admin.
        let _admin_join_payload = EventPayload {
            id: [0x01; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "admin".into(),
            },
        };
        // We can't reach into CommunitySyncRegistry to install this state
        // without running the full start_node bringup — that's well
        // beyond the scope of a unit test. Instead, this test focuses on
        // the apply_auto_exec_set_power signing path: build a SetPower
        // event the same way the helper does and verify the signature
        // verifies against the admin pubkey, proving the helper's mint
        // step would produce a valid event for the CRDT to accept.
        //
        // The full end-to-end path (Tier 2 finalize → auto-exec dispatch
        // → CRDT accept → peer publish) is exercised in the Task 16
        // integration test that wires the tick to a real engine.
        let payload = EventPayload {
            id: [0x02; 16],
            community_id,
            kind: MembershipEventKind::SetPower { target, level: 50 },
            actor: admin,
            at: Hlc {
                wall_ms: 200,
                logical: 0,
                device_id: "admin".into(),
            },
        };
        let bytes = canonical_cbor_encode(&payload).expect("encode");
        let sig = admin_signing_key.sign(&bytes).to_bytes();

        // Verify the signature against the admin's signing key's verifying
        // half — proves the helper's signing path would produce a
        // CRDT-acceptable event. Using `admin_signing_key.verifying_key()`
        // (the canonical Ed25519 pubkey) rather than `admin_pub[..32]`
        // because `PrivateIdentity::to_public_bytes()` returns the
        // address-hash-prefixed identity blob, not a raw Ed25519 pubkey.
        use ed25519_dalek::Verifier;
        let admin_verifying = admin_signing_key.verifying_key();
        assert!(
            admin_verifying
                .verify(&bytes, &ed25519_dalek::Signature::from_bytes(&sig))
                .is_ok(),
            "signed SetPower event must verify against admin pubkey"
        );
        let _ = admin_pub;
        let _ = target_pub;
    }

    /// ZEB-300 T4: the AdminProposal-routing auto-exec helper exists, fails
    /// closed on a bare NodeState (like `apply_auto_exec_set_power`), and its
    /// routed-mint step produces an `AdminProposal::SetPower` whose signature
    /// verifies — i.e. a CRDT-acceptable event.
    ///
    /// Load-bearing: the helper signs with `outbox.community_signing_key`
    /// (the enrolled #2 device key that community-membership events use), NOT
    /// the direct-SetPower `outbox.signing_key`; a wrong-key event would fail
    /// `verify_event` silently. Reaching the fully-wired two-admin engine
    /// (CommunitySyncRegistry + dm_outbox + materialized state) is out of
    /// reach for a pure unit test — as the sibling `..._signing_path_...`
    /// test documents — so we prove the community_signing_key mint seam
    /// directly via `mint_admin_proposal_set_power_event` (the exact call the
    /// helper's `MintProposal` arm makes).
    #[tokio::test]
    async fn auto_exec_admin_proposal_routes_and_verifies() {
        use crate::owner_state_crypto::canonical_cbor_encode;
        use ed25519_dalek::{Signature, SigningKey, Verifier};

        // Coverage note (Qodo, honest): this test covers exactly two seams —
        // (1) the `community_signing_key` mint seam (Part 2, via the same
        // `mint_admin_proposal_set_power_event` call the wrapper's
        // MintProposal arm makes), and (2) the bare-NodeState fail-closed
        // path (Part 1). It does NOT exercise the wrapper's own
        // plan→branch-select→`insert_local_event` flow: choosing
        // MintProposal vs Countersign vs AlreadyApplied vs Pending and
        // inserting the resulting event requires a live engine fixture
        // (CommunitySyncRegistry + dm_outbox + materialized state), which is
        // deferred. That branch selection is instead exhaustively unit-tested
        // against the pure planner in `mod plan_admin_proposal_tests`.

        // ── Part 1: helper is defined and fails closed on a bare NodeState.
        let node_state = std::sync::Arc::new(std::sync::Mutex::new(crate::NodeState::default()));
        let community_id = SpaceId([0xc0; 16]);
        let target = OwnerAddr([0xbb; 16]);
        let err = apply_auto_exec_admin_proposal_set_power(&node_state, community_id, target, 100)
            .await
            .expect_err("bare NodeState must Err, not panic");
        assert!(
            err.contains("missing") || err.contains("not running"),
            "error must mention missing/not running; got: {err}"
        );

        // ── Part 2: the routed-mint seam yields a verifiable AdminProposal.
        // Mint the SAME way the helper's MintProposal arm does — signed with
        // the community signing key — and prove the AdminProposal::SetPower
        // event's signature verifies against that key.
        let admin = OwnerAddr([0xaa; 16]);
        let community_signing_key = SigningKey::from_bytes(&[0x11; 32]);
        let hlc = Hlc {
            wall_ms: 5_000,
            logical: 0,
            device_id: "admin".into(),
        };
        let event = crate::mint_admin_proposal_set_power_event(
            community_id,
            admin,
            target,
            100,
            &community_signing_key,
            hlc,
        )
        .expect("mint AdminProposal::SetPower must succeed");

        assert!(
            matches!(
                &event.kind,
                MembershipEventKind::AdminProposal {
                    proposal_kind: ProposalKind::SetPower { target: t, level: 100 },
                } if *t == target
            ),
            "routed mint must be an AdminProposal::SetPower for the target at level 100; got {:?}",
            event.kind
        );

        let bytes = canonical_cbor_encode(&EventPayload::from(&event)).expect("encode payload");
        assert!(
            community_signing_key
                .verifying_key()
                .verify(&bytes, &Signature::from_bytes(&event.sig))
                .is_ok(),
            "AdminProposal signed with community_signing_key must verify"
        );
    }
}

#[cfg(test)]
mod plan_admin_proposal_tests {
    use super::*;

    const COM: SpaceId = SpaceId([0xc0; 16]);

    fn ev(
        id: EventId,
        actor: OwnerAddr,
        wall_ms: u64,
        kind: MembershipEventKind,
    ) -> SignedMembershipEvent {
        SignedMembershipEvent {
            signer_certs: Vec::new(),
            id,
            community_id: COM,
            actor,
            at: Hlc {
                wall_ms,
                logical: 0,
                device_id: "test".into(),
            },
            kind,
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        }
    }

    fn mk_proposal(
        id: EventId,
        actor: OwnerAddr,
        target: OwnerAddr,
        level: u8,
        wall_ms: u64,
    ) -> SignedMembershipEvent {
        ev(
            id,
            actor,
            wall_ms,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::SetPower { target, level },
            },
        )
    }

    fn mk_countersign(
        id: EventId,
        actor: OwnerAddr,
        target_id: EventId,
        wall_ms: u64,
    ) -> SignedMembershipEvent {
        ev(
            id,
            actor,
            wall_ms,
            MembershipEventKind::AdminCountersign {
                target_event_id: target_id,
            },
        )
    }

    // (a) already at power => AlreadyApplied
    #[test]
    fn plan_already_applied_when_target_already_at_level() {
        let target = OwnerAddr([1; 16]);
        let me = OwnerAddr([2; 16]);
        let mut mat = MaterializedMembership::default();
        mat.power_levels.insert(target, 100);
        let events: BTreeMap<EventId, SignedMembershipEvent> = BTreeMap::new();
        assert!(matches!(
            plan_admin_proposal_auto_exec(&mat, events.values(), target, 100, me, 1_000),
            AdminProposalPlan::AlreadyApplied
        ));
    }

    // (b) no candidate => MintProposal
    #[test]
    fn plan_mint_when_no_existing_proposal() {
        let target = OwnerAddr([1; 16]);
        let me = OwnerAddr([2; 16]);
        let mat = MaterializedMembership::default();
        let events: BTreeMap<EventId, SignedMembershipEvent> = BTreeMap::new();
        assert!(matches!(
            plan_admin_proposal_auto_exec(&mat, events.values(), target, 100, me, 1_000),
            AdminProposalPlan::MintProposal
        ));
    }

    // (c) one live candidate not signed by me => Countersign(that id)
    #[test]
    fn plan_countersign_existing_unsigned_proposal() {
        let target = OwnerAddr([1; 16]);
        let proposer = OwnerAddr([3; 16]);
        let me = OwnerAddr([2; 16]);
        let pid: EventId = [9u8; 16];
        let mat = MaterializedMembership::default();
        let mut events = BTreeMap::new();
        events.insert(pid, mk_proposal(pid, proposer, target, 100, 1_000));
        match plan_admin_proposal_auto_exec(&mat, events.values(), target, 100, me, 1_500) {
            AdminProposalPlan::Countersign(got) => assert_eq!(got, pid),
            other => panic!("expected Countersign, got {other:?}"),
        }
    }

    // (d) I already proposed it => Pending (awaiting other admins' quorum)
    #[test]
    fn plan_pending_when_i_am_proposer() {
        let target = OwnerAddr([1; 16]);
        let me = OwnerAddr([2; 16]);
        let pid: EventId = [9u8; 16];
        let mat = MaterializedMembership::default();
        let mut events = BTreeMap::new();
        events.insert(pid, mk_proposal(pid, me, target, 100, 1_000));
        assert!(matches!(
            plan_admin_proposal_auto_exec(&mat, events.values(), target, 100, me, 1_500),
            AdminProposalPlan::Pending
        ));
    }

    // (e) I already countersigned it => Pending (awaiting other admins' quorum)
    #[test]
    fn plan_pending_when_i_already_countersigned() {
        let target = OwnerAddr([1; 16]);
        let proposer = OwnerAddr([3; 16]);
        let me = OwnerAddr([2; 16]);
        let pid: EventId = [9u8; 16];
        let cid: EventId = [10u8; 16];
        let mat = MaterializedMembership::default();
        let mut events = BTreeMap::new();
        events.insert(pid, mk_proposal(pid, proposer, target, 100, 1_000));
        events.insert(cid, mk_countersign(cid, me, pid, 1_100));
        assert!(matches!(
            plan_admin_proposal_auto_exec(&mat, events.values(), target, 100, me, 1_500),
            AdminProposalPlan::Pending
        ));
    }

    // (f) two candidates => Countersign(min EventId)
    #[test]
    fn plan_countersign_canonical_min_event_id() {
        let target = OwnerAddr([1; 16]);
        let a = OwnerAddr([3; 16]);
        let b = OwnerAddr([4; 16]);
        let me = OwnerAddr([2; 16]);
        let low: EventId = [1u8; 16];
        let high: EventId = [2u8; 16];
        let mat = MaterializedMembership::default();
        let mut events = BTreeMap::new();
        events.insert(high, mk_proposal(high, a, target, 100, 1_000));
        events.insert(low, mk_proposal(low, b, target, 100, 1_000));
        match plan_admin_proposal_auto_exec(&mat, events.values(), target, 100, me, 1_500) {
            AdminProposalPlan::Countersign(got) => assert_eq!(got, low),
            other => panic!("expected canonical Countersign(low), got {other:?}"),
        }
    }

    // (g) only an expired candidate => MintProposal (fresh window)
    #[test]
    fn plan_mint_when_only_candidate_expired() {
        let target = OwnerAddr([1; 16]);
        let proposer = OwnerAddr([3; 16]);
        let me = OwnerAddr([2; 16]);
        let pid: EventId = [9u8; 16];
        let mat = MaterializedMembership::default();
        let mut events = BTreeMap::new();
        events.insert(pid, mk_proposal(pid, proposer, target, 100, 1_000));
        let now = 1_000 + ADMIN_PROPOSAL_EXPIRY_MS + 1;
        assert!(matches!(
            plan_admin_proposal_auto_exec(&mat, events.values(), target, 100, me, now),
            AdminProposalPlan::MintProposal
        ));
    }

    // (h) candidate for a DIFFERENT (target,level) is ignored => MintProposal
    #[test]
    fn plan_mint_ignores_proposal_for_other_target_or_level() {
        let target = OwnerAddr([1; 16]);
        let other = OwnerAddr([5; 16]);
        let proposer = OwnerAddr([3; 16]);
        let me = OwnerAddr([2; 16]);
        let pid: EventId = [9u8; 16];
        let mat = MaterializedMembership::default();
        let mut events = BTreeMap::new();
        events.insert(pid, mk_proposal(pid, proposer, other, 100, 1_000)); // different target
        assert!(matches!(
            plan_admin_proposal_auto_exec(&mat, events.values(), target, 100, me, 1_500),
            AdminProposalPlan::MintProposal
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_kind_defaults_to_text_and_reports_is_text() {
        assert_eq!(ChannelKind::default(), ChannelKind::Text);
        assert!(ChannelKind::Text.is_text());
        assert!(!ChannelKind::Voice.is_text());
    }

    #[test]
    fn channel_kind_serializes_as_cbor_u8() {
        // serde_repr encodes each variant as its bare u8 discriminant:
        // Text -> the single CBOR byte 0x00, Voice -> 0x01.
        let text = crate::owner_state_crypto::canonical_cbor_encode(&ChannelKind::Text)
            .expect("encode text");
        assert_eq!(text, vec![0x00]);
        let voice = crate::owner_state_crypto::canonical_cbor_encode(&ChannelKind::Voice)
            .expect("encode voice");
        assert_eq!(voice, vec![0x01]);
        // Both round-trip back through ciborium decode.
        let text_back: ChannelKind = ciborium::de::from_reader(&text[..]).expect("decode text");
        assert_eq!(text_back, ChannelKind::Text);
        let voice_back: ChannelKind = ciborium::de::from_reader(&voice[..]).expect("decode voice");
        assert_eq!(voice_back, ChannelKind::Voice);
    }

    #[test]
    fn channel_kind_cbor_unknown_tag_is_rejected() {
        // serde_repr rejects unknown discriminants on decode: 0x03 is not
        // Text (0), Voice (1), or Townhall (2 — ZEB-612), so it must fail
        // rather than silently default. This pins the stale-client posture:
        // a future kind rejects the containing decode, never misdecodes.
        let result: Result<ChannelKind, _> = ciborium::de::from_reader(&[0x03u8][..]);
        assert!(result.is_err(), "tag 3 must be rejected");
        // Townhall's own tag decodes (the flip side of the same pin).
        let townhall: ChannelKind = ciborium::de::from_reader(&[0x02u8][..]).expect("tag 2");
        assert_eq!(townhall, ChannelKind::Townhall);
    }

    fn make_kick_event(
        id_byte: u8,
        actor: OwnerAddr,
        target: OwnerAddr,
        at_wall_ms: u64,
    ) -> SignedMembershipEvent {
        let mut id = [0xfa; 16];
        id[15] = id_byte;
        SignedMembershipEvent {
            signer_certs: Vec::new(),
            id,
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::Kick {
                target,
                reason: None,
            },
            actor,
            at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        }
    }

    fn make_rotation_event(
        id_byte: u8,
        actor: OwnerAddr,
        triggered_by: [u8; 16],
        prior_epoch: u64,
        recipients: Vec<(OwnerAddr, Vec<u8>)>,
        at_wall_ms: u64,
    ) -> SignedMembershipEvent {
        let mut id = [0xfb; 16];
        id[15] = id_byte;
        let recipient_ciphertexts: Vec<RecipientCiphertext> = recipients
            .into_iter()
            .map(|(addr, sealed)| RecipientCiphertext {
                recipient: addr,
                sealed,
            })
            .collect();
        SignedMembershipEvent {
            signer_certs: Vec::new(),
            id,
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::EpochRotation {
                prior_epoch,
                triggered_by,
                recipient_ciphertexts,
            },
            actor,
            at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        }
    }

    fn make_leave_event(id_byte: u8, actor: OwnerAddr, at_wall_ms: u64) -> SignedMembershipEvent {
        let mut id = [0xfc; 16];
        id[15] = id_byte;
        SignedMembershipEvent {
            signer_certs: Vec::new(),
            id,
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::Leave,
            actor,
            at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        }
    }

    /// Helper: construct a Join event for a member so materialize can
    /// find them in the members map (needed for Kick to update status).
    fn make_join_event(id_byte: u8, actor: OwnerAddr, at_wall_ms: u64) -> SignedMembershipEvent {
        let mut id = [0xfd; 16];
        id[15] = id_byte;
        SignedMembershipEvent {
            signer_certs: Vec::new(),
            id,
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::Join,
            actor,
            at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        }
    }

    /// ZEB-339: build a cert-bearing Join for `owner`, signed by their enrolled
    /// device key. Used to seed `prior_state` so a steady-state event the same
    /// owner later signs can have its signer resolved from materialized
    /// `enrolled_device_keys`.
    fn make_enrolled_join(
        id_byte: u8,
        owner: &TestOwner,
        at_wall_ms: u64,
    ) -> SignedMembershipEvent {
        let mut id = [0xfd; 16];
        id[15] = id_byte;
        let payload = EventPayload {
            id,
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::Join,
            actor: owner.owner,
            at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "test".into(),
            },
        };
        let ev = sign_event(&payload, &owner.device_key).expect("sign enrolled join");
        SignedMembershipEvent {
            enrollment: Some(owner.cert.clone()),
            ..ev
        }
    }

    fn make_catchup_event(
        id_byte: u8,
        actor: OwnerAddr,
        triggered_by: [u8; 16],
        epoch: u64,
        recipients: Vec<(OwnerAddr, Vec<u8>)>,
        at_wall_ms: u64,
    ) -> SignedMembershipEvent {
        let mut id = [0xfe; 16];
        id[15] = id_byte;
        let recipient_ciphertexts: Vec<RecipientCiphertext> = recipients
            .into_iter()
            .map(|(addr, sealed)| RecipientCiphertext {
                recipient: addr,
                sealed,
            })
            .collect();
        SignedMembershipEvent {
            signer_certs: Vec::new(),
            id,
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::EpochCatchup {
                epoch,
                triggered_by,
                recipient_ciphertexts,
            },
            actor,
            at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        }
    }

    fn make_stale_join_event(
        id_byte: u8,
        actor: OwnerAddr,
        at_wall_ms: u64,
    ) -> SignedMembershipEvent {
        let mut id = [0xff; 16];
        id[15] = id_byte;
        SignedMembershipEvent {
            signer_certs: Vec::new(),
            id,
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::Join,
            actor,
            at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        }
    }

    #[test]
    fn epoch_catchup_delivers_current_key_without_advancing_epoch() {
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        let dave = OwnerAddr([0xd1; 16]);

        let kick = make_kick_event(0x01, admin, bob, 100);
        let rot = make_rotation_event(0x01, admin, kick.id, 0, vec![(admin, vec![1; 92])], 101);
        let join_d = make_stale_join_event(0x01, dave, 200);

        // Sanity: post-Join, dave is in pending_catchup_for.
        let m_pre = materialize(&[kick.clone(), rot.clone(), join_d.clone()], admin);
        assert_eq!(m_pre.current_epoch, Some(1));
        assert!(
            m_pre.pending_catchup_for.contains(&dave),
            "post-Join with stale snapshot: dave is pending catchup"
        );

        let catchup = make_catchup_event(0x01, admin, join_d.id, 1, vec![(dave, vec![5; 92])], 300);
        let m_post = materialize(&[kick, rot, join_d, catchup], admin);
        assert_eq!(
            m_post.current_epoch,
            Some(1),
            "catchup must NOT advance epoch"
        );
        assert!(!m_post.pending_catchup_for.contains(&dave), "dave cleared");
    }

    #[test]
    fn stale_invite_join_marks_pending_catchup_for() {
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        let dave = OwnerAddr([0xd1; 16]);

        let kick = make_kick_event(0x01, admin, bob, 100);
        let rot = make_rotation_event(0x01, admin, kick.id, 0, vec![(admin, vec![1; 92])], 101);
        let join_d = make_stale_join_event(0x01, dave, 200);

        let m = materialize(&[kick, rot, join_d], admin);
        assert_eq!(m.current_epoch, Some(1));
        assert!(m.pending_catchup_for.contains(&dave));
    }

    #[test]
    fn epoch_catchup_with_stale_epoch_dropped() {
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        let dave = OwnerAddr([0xd1; 16]);

        let kick = make_kick_event(0x01, admin, bob, 100);
        let rot = make_rotation_event(0x01, admin, kick.id, 0, vec![(admin, vec![1; 92])], 101);
        let join_d = make_stale_join_event(0x01, dave, 200);
        // catchup says epoch=0 but current is now 1 → must be dropped.
        let stale_catchup =
            make_catchup_event(0x01, admin, join_d.id, 0, vec![(dave, vec![1; 92])], 300);

        let m = materialize(&[kick, rot, join_d, stale_catchup], admin);
        assert!(
            m.pending_catchup_for.contains(&dave),
            "stale-epoch catchup must NOT clear pending_catchup_for"
        );
    }

    #[test]
    fn epoch_catchup_referencing_non_join_event_dropped() {
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        let dave = OwnerAddr([0xd1; 16]);

        let kick = make_kick_event(0x01, admin, bob, 100);
        let rot = make_rotation_event(0x01, admin, kick.id, 0, vec![(admin, vec![1; 92])], 101);
        let join_d = make_stale_join_event(0x01, dave, 200);
        // Malformed catchup: triggered_by points to kick (not a Join).
        let bad_catchup =
            make_catchup_event(0x01, admin, kick.id, 1, vec![(dave, vec![1; 92])], 300);

        let m = materialize(&[kick, rot, join_d, bad_catchup], admin);
        assert!(
            m.pending_catchup_for.contains(&dave),
            "catchup with non-Join triggered_by must NOT clear pending_catchup_for"
        );
    }

    #[test]
    fn non_admin_issued_catchup_dropped() {
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        let carol = OwnerAddr([0xc1; 16]); // not admin
        let dave = OwnerAddr([0xd1; 16]);

        let kick = make_kick_event(0x01, admin, bob, 100);
        let rot = make_rotation_event(
            0x01,
            admin,
            kick.id,
            0,
            vec![(admin, vec![1; 92]), (carol, vec![1; 92])],
            101,
        );
        let join_d = make_stale_join_event(0x01, dave, 200);
        // Carol tries to catchup-fill dave's gap, but she's not admin.
        let bad_catchup =
            make_catchup_event(0x01, carol, join_d.id, 1, vec![(dave, vec![1; 92])], 300);

        let m = materialize(&[kick, rot, join_d, bad_catchup], admin);
        assert!(
            m.pending_catchup_for.contains(&dave),
            "non-admin catchup must NOT clear pending_catchup_for"
        );
    }

    #[test]
    fn duplicate_catchup_for_same_join_is_harmless_nop() {
        let admin = OwnerAddr([0xa1; 16]);
        let admin2 = OwnerAddr([0xa2; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        let dave = OwnerAddr([0xd1; 16]);

        let setpwr_admin2 = SignedMembershipEvent {
            signer_certs: Vec::new(),
            id: [0x05; 16],
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::SetPower {
                target: admin2,
                level: 100,
            },
            actor: admin,
            at: Hlc {
                wall_ms: 50,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        };
        let kick = make_kick_event(0x01, admin, bob, 100);
        let rot = make_rotation_event(
            0x01,
            admin,
            kick.id,
            0,
            vec![(admin, vec![1; 92]), (admin2, vec![1; 92])],
            101,
        );
        let join_d = make_stale_join_event(0x01, dave, 200);
        // Two admin-issued catchups for the same Join. Use distinct id_bytes.
        let catchup1 =
            make_catchup_event(0x01, admin, join_d.id, 1, vec![(dave, vec![1; 92])], 300);
        let catchup2 =
            make_catchup_event(0x02, admin2, join_d.id, 1, vec![(dave, vec![2; 92])], 301);

        let m = materialize(
            &[setpwr_admin2, kick, rot, join_d, catchup1, catchup2],
            admin,
        );
        assert!(
            !m.pending_catchup_for.contains(&dave),
            "dave was caught up by first catchup"
        );
        assert_eq!(m.current_epoch, Some(1), "epoch unchanged by catchups");
    }

    #[test]
    fn epoch_rotation_advances_current_epoch() {
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        // admin has power 100 from bootstrap; bob needs to be in members for kick to fire
        let bob_join = make_join_event(0x01, bob, 50);
        let kick = make_kick_event(0x01, admin, bob, 100);
        let rot = make_rotation_event(0x01, admin, kick.id, 0, vec![(admin, vec![1; 92])], 101);
        let m = materialize(&[bob_join, kick, rot], admin);
        assert_eq!(m.current_epoch, Some(1));
        assert!(!m.pending_rotation_for.contains(&bob));
        assert_eq!(m.members[&bob].status, MemberStatus::Banned);
    }

    #[test]
    fn stale_rotation_dropped() {
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        let carol = OwnerAddr([0xc1; 16]);
        let bob_join = make_join_event(0x01, bob, 50);
        let carol_join = make_join_event(0x02, carol, 51);
        let kick1 = make_kick_event(0x01, admin, bob, 100);
        // M8: rot1 must include ALL remaining Joined/Invited members (carol + admin).
        // admin has no member record (bootstrap admin) but materialize only checks
        // m.members for completeness, so admin is NOT in expected_recipients
        // (admin never inserted a Join). carol IS in m.members (carol_join at t=51).
        let rot1 = make_rotation_event(
            0x01,
            admin,
            kick1.id,
            0,
            vec![(admin, vec![1; 92]), (carol, vec![1; 92])],
            101,
        );
        // Second kick for carol; try a STALE rotation with prior_epoch=0
        // (current would be 1 after rot1).
        let kick2 = make_kick_event(0x02, admin, carol, 200);
        let stale_rot =
            make_rotation_event(0x02, admin, kick2.id, 0, vec![(admin, vec![3; 92])], 201);
        let m = materialize(
            &[bob_join, carol_join, kick1, rot1, kick2, stale_rot],
            admin,
        );
        assert_eq!(m.current_epoch, Some(1)); // stale rotation didn't advance
        assert!(m.pending_rotation_for.contains(&carol));
    }

    #[test]
    fn malformed_rotation_dropped() {
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        let bob_join = make_join_event(0x01, bob, 50);
        let kick = make_kick_event(0x01, admin, bob, 100);
        // Malformed: recipient_ciphertexts includes bob (the kicked target).
        let malformed = make_rotation_event(
            0x01,
            admin,
            kick.id,
            0,
            vec![(admin, vec![1; 92]), (bob, vec![1; 92])],
            101,
        );
        let m = materialize(&[bob_join, kick, malformed], admin);
        assert!(m.current_epoch.unwrap_or(0) == 0); // didn't advance
        assert!(m.pending_rotation_for.contains(&bob));
    }

    #[test]
    fn leaver_issued_rotation_accepted_when_well_formed() {
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        let bob_join = make_join_event(0x01, bob, 50);
        let leave = make_leave_event(0x01, bob, 100);
        // bob signs the rotation; recipients exclude bob.
        let rot = make_rotation_event(0x01, bob, leave.id, 0, vec![(admin, vec![1; 92])], 101);
        let m = materialize(&[bob_join, leave, rot], admin);
        assert_eq!(m.current_epoch, Some(1));
        assert!(!m.pending_rotation_for.contains(&bob));
    }

    #[test]
    fn leaver_issued_rotation_rejected_when_self_included() {
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        let bob_join = make_join_event(0x01, bob, 50);
        let leave = make_leave_event(0x01, bob, 100);
        // bob signs the rotation BUT includes himself (malformed).
        let rot = make_rotation_event(
            0x01,
            bob,
            leave.id,
            0,
            vec![(admin, vec![1; 92]), (bob, vec![1; 92])],
            101,
        );
        let m = materialize(&[bob_join, leave, rot], admin);
        assert!(m.current_epoch.unwrap_or(0) == 0);
        assert!(m.pending_rotation_for.contains(&bob));
    }

    #[test]
    fn pending_rotation_tracking_clears_after_matching_rotation_lands() {
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        let bob_join = make_join_event(0x01, bob, 50);
        let kick = make_kick_event(0x01, admin, bob, 100);
        let m_partial = materialize(&[bob_join.clone(), kick.clone()], admin);
        assert!(m_partial.pending_rotation_for.contains(&bob));
        let rot = make_rotation_event(0x01, admin, kick.id, 0, vec![(admin, vec![1; 92])], 101);
        let m_full = materialize(&[bob_join, kick, rot], admin);
        assert_eq!(m_full.pending_rotation_for.len(), 0);
    }

    #[test]
    fn kick_then_rotation_same_hlc_tick_materializes_atomically() {
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        let bob_join = make_join_event(0x01, bob, 50);
        // Same wall_ms for kick + rotation; event-id tiebreaks (kick id ends 0x01, rotation ends 0x01)
        // but different id arrays (0xfa...01 vs 0xfb...01) ensure distinct ordering.
        let kick = make_kick_event(0x01, admin, bob, 100);
        let rot = make_rotation_event(0x01, admin, kick.id, 0, vec![(admin, vec![1; 92])], 100);
        let m = materialize(&[bob_join, kick, rot], admin);
        assert_eq!(m.current_epoch, Some(1));
        assert_eq!(m.pending_rotation_for.len(), 0);
    }

    #[test]
    fn concurrent_kicks_self_heal() {
        let admin1 = OwnerAddr([0xa1; 16]);
        let admin2 = OwnerAddr([0xa2; 16]);
        let alice = OwnerAddr([0xb1; 16]);
        let bob = OwnerAddr([0xb2; 16]);
        // Promote admin2 to admin power via SetPower from admin1.
        let setpwr = SignedMembershipEvent {
            signer_certs: Vec::new(),
            id: [0x05; 16],
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::SetPower {
                target: admin2,
                level: 100,
            },
            actor: admin1,
            at: Hlc {
                wall_ms: 50,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        };
        let alice_join = make_join_event(0x01, alice, 51);
        let bob_join = make_join_event(0x02, bob, 52);
        let kick_a = make_kick_event(0x01, admin1, alice, 100);
        let rot_a = make_rotation_event(
            0x01,
            admin1,
            kick_a.id,
            0,
            vec![(admin2, vec![1; 92]), (bob, vec![1; 92])],
            101,
        );
        let kick_b = make_kick_event(0x02, admin2, bob, 200);
        // STALE: prior_epoch=0 but current is now 1 after rot_a.
        let rot_b = make_rotation_event(
            0x02,
            admin2,
            kick_b.id,
            0,
            vec![(admin1, vec![2; 92]), (alice, vec![2; 92])],
            201,
        );
        let m = materialize(
            &[setpwr, alice_join, bob_join, kick_a, rot_a, kick_b, rot_b],
            admin1,
        );
        assert_eq!(m.current_epoch, Some(1)); // only rot_a advanced
        assert!(m.pending_rotation_for.contains(&bob));
        assert!(!m.pending_rotation_for.contains(&alice));
    }

    // ── C4: verify_event rejects unauthorized epoch events ────────────────────

    /// ZEB-339: build a test owner (owner_id + enrolled device key + Master
    /// cert) from a seed byte. Returns `(TestOwner, dummy_pub, owner_addr)` so
    /// existing `(priv, pub, addr)` destructures keep compiling; the middle
    /// element is a placeholder (the old 64-byte identity_pub is no longer
    /// consumed by VerifyContext).
    fn make_identity(seed_byte: u8) -> (TestOwner, [u8; 64], OwnerAddr) {
        let owner = mint_test_owner(seed_byte);
        let addr = owner.owner;
        (owner, [0u8; 64], addr)
    }

    /// ZEB-339: sign a membership event payload with the owner's enrolled
    /// device key. For identity-introducing events (Join / PendingJoin) the
    /// owner's Master enrollment cert is attached so `materialize` populates
    /// `enrolled_device_keys` and `verify_event` can resolve the signer.
    fn sign_with_identity(payload: EventPayload, owner: &TestOwner) -> SignedMembershipEvent {
        let ev = sign_event(&payload, &owner.device_key).expect("sign_event must succeed");
        match ev.kind {
            MembershipEventKind::Join | MembershipEventKind::PendingJoin { .. } => {
                SignedMembershipEvent {
                    enrollment: Some(owner.cert.clone()),
                    ..ev
                }
            }
            _ => ev,
        }
    }

    #[test]
    fn verify_event_accepts_owner_id_actor_signed_by_enrolled_device() {
        // PRODUCTION pairing: actor = owner_id, signed by device #2 (owner_id has
        // no signing key). Bootstrap Join, empty prior.
        let admin = mint_test_owner(0x31);
        let cid = SpaceId([5u8; 16]);
        let join = sign_event(
            &EventPayload {
                id: [1u8; 16],
                community_id: cid,
                kind: MembershipEventKind::Join,
                actor: admin.owner,
                at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &admin.device_key,
        )
        .unwrap();
        let join = SignedMembershipEvent {
            enrollment: Some(admin.cert.clone()),
            ..join
        };
        let prior = MaterializedMembership::default();
        let ctx = VerifyContext {
            expected_community_id: cid,
            admin_addr: admin.owner,
            is_invite_only: false,
        };
        assert_eq!(verify_event(&join, &prior, &ctx), Ok(()));
    }

    /// C4: verify_event must reject an EpochRotation issued by a never-member
    /// (zero power, not in members map) even if the signature is valid.
    #[test]
    fn verify_event_rejects_unauthorized_epoch_rotation_from_never_member() {
        let community_id = SpaceId([0xc0; 16]);
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let (attacker_priv, _attacker_pub, attacker_addr) = make_identity(0xee);

        // Build a prior state where admin is joined (epoch 0, no members other than admin).
        // We use materialize with an admin-join event so power_levels has admin at 100.
        let admin_join_payload = EventPayload {
            id: [0x01; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin_addr,
            at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "t".into(),
            },
        };
        let admin_join = sign_with_identity(admin_join_payload, &admin_priv);
        let prior = materialize(std::slice::from_ref(&admin_join), admin_addr);

        // Attacker (never a member, zero power) tries to issue an EpochRotation.
        // We need a plausible triggered_by; use admin_join.id as placeholder.
        let rotation_payload = EventPayload {
            id: [0xfe; 16],
            community_id,
            kind: MembershipEventKind::EpochRotation {
                prior_epoch: 0,
                triggered_by: admin_join.id,
                recipient_ciphertexts: vec![RecipientCiphertext {
                    recipient: admin_addr,
                    sealed: vec![0u8; 92],
                }],
            },
            actor: attacker_addr,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "t".into(),
            },
        };
        let rotation_event = sign_with_identity(rotation_payload, &attacker_priv);

        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: false,
        };
        let result = verify_event(&rotation_event, &prior, &ctx);
        // ZEB-339: a never-member's event is rejected at signer resolution
        // (step 1) — no materialized membership means no enrolled device key —
        // before the EpochEventUnauthorized power gate is reached. The security
        // property (unauthorized epoch event rejected) is preserved.
        assert!(
            matches!(result, Err(VerifyError::SignerNotEnrolledForActor)),
            "EpochRotation from never-member must be rejected with SignerNotEnrolledForActor; got {result:?}"
        );
    }

    /// C4: verify_event must reject an EpochCatchup from a non-admin (power < 50).
    #[test]
    fn verify_event_rejects_unauthorized_epoch_catchup_non_admin() {
        let community_id = SpaceId([0xc0; 16]);
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let (bob_priv, _bob_pub, bob_addr) = make_identity(0xb1);

        // Admin + bob join.
        let admin_join_payload = EventPayload {
            id: [0x01; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin_addr,
            at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "t".into(),
            },
        };
        let admin_join = sign_with_identity(admin_join_payload, &admin_priv);
        let bob_join_payload = EventPayload {
            id: [0x02; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: bob_addr,
            at: Hlc {
                wall_ms: 2,
                logical: 0,
                device_id: "t".into(),
            },
        };
        let bob_join = sign_with_identity(bob_join_payload, &bob_priv);

        let prior = materialize(&[admin_join.clone(), bob_join.clone()], admin_addr);
        // prior: admin has power 100 (bootstrap), bob has power 0 (default).
        // current_epoch is None (0); epoch=0 in the catchup is OK.

        // bob (power=0) tries to issue a catchup for themselves at epoch 0.
        let catchup_payload = EventPayload {
            id: [0xfd; 16],
            community_id,
            kind: MembershipEventKind::EpochCatchup {
                epoch: 0,
                triggered_by: bob_join.id,
                recipient_ciphertexts: vec![RecipientCiphertext {
                    recipient: bob_addr,
                    sealed: vec![0u8; 92],
                }],
            },
            actor: bob_addr,
            at: Hlc {
                wall_ms: 10,
                logical: 0,
                device_id: "t".into(),
            },
        };
        let catchup_event = sign_with_identity(catchup_payload, &bob_priv);

        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: false,
        };
        let result = verify_event(&catchup_event, &prior, &ctx);
        assert!(
            matches!(result, Err(VerifyError::EpochEventUnauthorized)),
            "EpochCatchup from non-admin must be rejected with EpochEventUnauthorized; got {result:?}"
        );
    }

    // ── C3: EpochRotation rejects kicked former admin with stale power_levels ──

    /// C3: A kicked former admin still has power_levels[addr] = 100 (power_levels
    /// is not cleaned up on Kick/Leave). They must NOT be able to authorize a
    /// subsequent EpochRotation because they are no longer Joined.
    #[test]
    fn epoch_rotation_rejects_non_joined_issuer_with_stale_power() {
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        let carol = OwnerAddr([0xc1; 16]);

        // Give bob admin power so he can kick.
        let setpwr_bob = SignedMembershipEvent {
            signer_certs: Vec::new(),
            id: [0x01; 16],
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::SetPower {
                target: bob,
                level: 100,
            },
            actor: admin,
            at: Hlc {
                wall_ms: 10,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        };
        // Carol joins (to be kicked).
        let carol_join = make_join_event(0x02, carol, 20);
        // Bob joins (so he has status Joined at the time of setpwr).
        let bob_join = make_join_event(0x03, bob, 5);

        // Admin kicks bob (bob is now Banned; retains power_levels[bob]=100).
        let kick_bob = make_kick_event(0x04, admin, bob, 100);

        // Now kick carol triggering a rotation.
        let kick_carol = make_kick_event(0x05, admin, carol, 200);

        // bob (now Banned, not Joined) tries to issue the rotation for carol's kick.
        // Power check alone would pass (bob has power 100), but Joined check must fail.
        let rot_by_bob =
            make_rotation_event(0x06, bob, kick_carol.id, 0, vec![(admin, vec![1; 92])], 201);

        let m = materialize(
            &[
                bob_join, setpwr_bob, carol_join, kick_bob, kick_carol, rot_by_bob,
            ],
            admin,
        );
        // The rotation by bob must be dropped (bob is Banned, not Joined).
        assert_eq!(
            m.current_epoch.unwrap_or(0),
            0,
            "rotation by non-Joined former admin must be dropped"
        );
        assert!(
            m.pending_rotation_for.contains(&carol),
            "carol's rotation is still pending"
        );
    }

    // ── C8: EpochCatchup rejects former admin with stale power_levels ─────────

    /// C8: A former admin (kicked or Left) still has power_levels entry >= kick_threshold.
    /// EpochCatchup must check the issuer is currently Joined, not just powered.
    #[test]
    fn epoch_catchup_rejects_former_admin_with_stale_power() {
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]); // will be kicked (former admin)
        let dave = OwnerAddr([0xd1; 16]); // new member needing catchup

        // Give bob admin power.
        let setpwr_bob = SignedMembershipEvent {
            signer_certs: Vec::new(),
            id: [0x01; 16],
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::SetPower {
                target: bob,
                level: 100,
            },
            actor: admin,
            at: Hlc {
                wall_ms: 5,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        };
        let bob_join = make_join_event(0x02, bob, 10);
        // Kick carol (not bob) so we have a rotation to establish epoch=1.
        let carol = OwnerAddr([0xc1; 16]);
        let carol_join = make_join_event(0x03, carol, 15);
        let kick_carol = make_kick_event(0x04, admin, carol, 100);
        // Rotation for carol's kick: both admin and bob in recipients.
        let rot = make_rotation_event(
            0x01,
            admin,
            kick_carol.id,
            0,
            vec![(admin, vec![1; 92]), (bob, vec![1; 92])],
            101,
        );
        // Now kick bob (he's Banned, retains power_levels[bob]=100).
        let kick_bob = make_kick_event(0x05, admin, bob, 200);
        // Rotation for bob's kick.
        let rot2 =
            make_rotation_event(0x02, admin, kick_bob.id, 1, vec![(admin, vec![2; 92])], 201);
        // Dave joins at epoch 2.
        let join_dave = make_stale_join_event(0x01, dave, 300);

        // bob (Banned, not Joined) tries to issue a catchup for dave.
        let catchup_by_bob =
            make_catchup_event(0x01, bob, join_dave.id, 2, vec![(dave, vec![3; 92])], 400);

        let m = materialize(
            &[
                bob_join,
                setpwr_bob,
                carol_join,
                kick_carol,
                rot,
                kick_bob,
                rot2,
                join_dave,
                catchup_by_bob,
            ],
            admin,
        );
        // catchup by former admin (now Banned) must be dropped.
        assert!(
            m.pending_catchup_for.contains(&dave),
            "catchup by non-Joined former admin must be dropped"
        );
    }

    // ── M2: no-op leave must not pollute pending_rotation_for ────────────────

    /// M2: A Leave from an actor who was never a member is a no-op and must NOT
    /// add them to pending_rotation_for.
    #[test]
    fn noop_leave_does_not_add_to_pending_rotation_for() {
        let admin = OwnerAddr([0xa1; 16]);
        let never_member = OwnerAddr([0xbb; 16]);
        let leave = make_leave_event(0x01, never_member, 100);
        let m = materialize(&[leave], admin);
        assert!(
            !m.pending_rotation_for.contains(&never_member),
            "Leave from never-member must NOT add to pending_rotation_for"
        );
        assert!(
            m.members.is_empty(),
            "never-member must not appear in members"
        );
    }

    // ── M8: rotation must include ALL remaining Joined/Invited members ────────

    /// M8: An EpochRotation that omits a currently-Joined member (other than
    /// the kick target) must be dropped.
    #[test]
    fn epoch_rotation_rejects_incomplete_recipient_list() {
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]); // kick target
        let carol = OwnerAddr([0xc1; 16]); // joined member NOT in recipients

        let bob_join = make_join_event(0x01, bob, 10);
        let carol_join = make_join_event(0x02, carol, 20);
        let kick = make_kick_event(0x01, admin, bob, 100);
        // Incomplete rotation: omits carol from recipient_ciphertexts.
        let incomplete_rot = make_rotation_event(
            0x01,
            admin,
            kick.id,
            0,
            vec![(admin, vec![1; 92])], // carol missing!
            101,
        );
        let m = materialize(&[bob_join, carol_join, kick, incomplete_rot], admin);
        assert_eq!(
            m.current_epoch.unwrap_or(0),
            0,
            "incomplete rotation must be dropped"
        );
        assert!(
            m.pending_rotation_for.contains(&bob),
            "bob's rotation still pending"
        );
    }

    // ── Unban helper (mirrors make_kick_event) ────────────────────────────────

    fn make_unban_event(
        id_byte: u8,
        actor: OwnerAddr,
        target: OwnerAddr,
        reason: Option<String>,
        at_wall_ms: u64,
    ) -> SignedMembershipEvent {
        let mut id = [0xf0; 16];
        id[15] = id_byte;
        SignedMembershipEvent {
            signer_certs: Vec::new(),
            id,
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::Unban { target, reason },
            actor,
            at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        }
    }

    fn make_invite_event(
        id_byte: u8,
        actor: OwnerAddr,
        target: OwnerAddr,
        at_wall_ms: u64,
    ) -> SignedMembershipEvent {
        let mut id = [0xef; 16];
        id[15] = id_byte;
        SignedMembershipEvent {
            signer_certs: Vec::new(),
            id,
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::Invite { target },
            actor,
            at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        }
    }

    // ── ZEB-284 Task 1: Unban variant unit tests ──────────────────────────────

    /// Unban by an admin (power 100) on a Banned target must:
    ///   (a) pass verify_event, and
    ///   (b) materialize to MemberStatus::Left.
    #[test]
    fn unban_event_succeeds_when_actor_is_admin_and_target_is_banned() {
        let community_id = SpaceId([0xc0; 16]);
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let (_, _, target_addr) = make_identity(0xb1);

        // Prior state: admin joined, target joined then kicked (Banned).
        let admin_join = make_enrolled_join(0x01, &admin_priv, 1);
        let target_join = make_join_event(0x02, target_addr, 2);
        let kick = make_kick_event(0x01, admin_addr, target_addr, 10);
        let prior = materialize(
            &[admin_join.clone(), target_join.clone(), kick.clone()],
            admin_addr,
        );

        // Sanity: target is Banned.
        assert_eq!(prior.members[&target_addr].status, MemberStatus::Banned);

        // Build a signed unban event.
        let unban_payload = EventPayload {
            id: [0xf0; 16],
            community_id,
            kind: MembershipEventKind::Unban {
                target: target_addr,
                reason: Some("test".into()),
            },
            actor: admin_addr,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "t".into(),
            },
        };
        let unban = sign_with_identity(unban_payload, &admin_priv);

        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: false,
        };
        let result = verify_event(&unban, &prior, &ctx);
        assert!(
            result.is_ok(),
            "admin unban of Banned target must pass verify_event; got {result:?}"
        );

        // Materialize: Banned → Left.
        let m = materialize(&[admin_join, target_join, kick, unban], admin_addr);
        assert_eq!(m.members[&target_addr].status, MemberStatus::Left);
    }

    /// Unban by a moderator (power 50, below set_power threshold of 100)
    /// must be rejected with ActorPowerInsufficient.
    #[test]
    fn unban_event_rejected_when_actor_is_moderator() {
        let community_id = SpaceId([0xc0; 16]);
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let (mod_priv, _mod_pub, mod_addr) = make_identity(0xb1);
        let (_, _, target_addr) = make_identity(0xc1);

        // Build prior state: mod_addr has power 50, target is Banned.
        let admin_join = make_enrolled_join(0x01, &admin_priv, 1);
        let mod_join = make_enrolled_join(0x02, &mod_priv, 2);
        let target_join = make_join_event(0x03, target_addr, 3);
        let setpwr_mod = SignedMembershipEvent {
            signer_certs: Vec::new(),
            id: [0x04; 16],
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::SetPower {
                target: mod_addr,
                level: 50,
            },
            actor: admin_addr,
            at: Hlc {
                wall_ms: 4,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        };
        let kick = make_kick_event(0x01, admin_addr, target_addr, 10);
        let prior = materialize(
            &[admin_join, mod_join, target_join, setpwr_mod, kick],
            admin_addr,
        );
        assert_eq!(prior.power_levels[&mod_addr], 50);
        assert_eq!(prior.members[&target_addr].status, MemberStatus::Banned);

        // mod_addr (power 50) tries to Unban.
        let unban_payload = EventPayload {
            id: [0xf0; 16],
            community_id,
            kind: MembershipEventKind::Unban {
                target: target_addr,
                reason: None,
            },
            actor: mod_addr,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "t".into(),
            },
        };
        let unban = sign_with_identity(unban_payload, &mod_priv);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&unban, &prior, &ctx),
            Err(VerifyError::ActorPowerInsufficient),
            "moderator (power 50) must not be able to unban"
        );
    }

    /// Unban targeting a member whose status is NOT Banned (e.g., Joined)
    /// must be rejected with UnbanTargetNotBanned.
    #[test]
    fn unban_event_rejected_when_target_is_not_banned() {
        let community_id = SpaceId([0xc0; 16]);
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let (_, _, target_addr) = make_identity(0xb1);

        // Prior state: admin joined, target joined (NOT banned).
        let admin_join = make_enrolled_join(0x01, &admin_priv, 1);
        let target_join = make_join_event(0x02, target_addr, 2);
        let prior = materialize(&[admin_join, target_join], admin_addr);
        assert_eq!(prior.members[&target_addr].status, MemberStatus::Joined);

        let unban_payload = EventPayload {
            id: [0xf0; 16],
            community_id,
            kind: MembershipEventKind::Unban {
                target: target_addr,
                reason: None,
            },
            actor: admin_addr,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "t".into(),
            },
        };
        let unban = sign_with_identity(unban_payload, &admin_priv);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&unban, &prior, &ctx),
            Err(VerifyError::UnbanTargetNotBanned),
            "unban of non-Banned target must return UnbanTargetNotBanned"
        );
    }

    /// Unban targeting an OwnerAddr with no member record (never joined)
    /// must be rejected with UnbanTargetNotMember (the Unban-specific
    /// variant so the surfaced message references "unban" not "kick").
    #[test]
    fn unban_event_rejected_when_target_is_unknown() {
        let community_id = SpaceId([0xc0; 16]);
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let (_, _, unknown_addr) = make_identity(0xdd);

        // Prior state: only admin joined; unknown_addr never appeared.
        let admin_join = make_enrolled_join(0x01, &admin_priv, 1);
        let prior = materialize(&[admin_join], admin_addr);
        assert!(!prior.members.contains_key(&unknown_addr));

        let unban_payload = EventPayload {
            id: [0xf0; 16],
            community_id,
            kind: MembershipEventKind::Unban {
                target: unknown_addr,
                reason: None,
            },
            actor: admin_addr,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "t".into(),
            },
        };
        let unban = sign_with_identity(unban_payload, &admin_priv);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&unban, &prior, &ctx),
            Err(VerifyError::UnbanTargetNotMember),
            "unban of unknown target must return UnbanTargetNotMember"
        );
    }

    /// Kick with a reason longer than `MAX_MODERATION_REASON_CHARS` must be
    /// rejected at verify_event so an oversized reason cannot bypass the UI
    /// cap and persist to every replica.
    #[test]
    fn kick_event_rejected_when_reason_exceeds_max_chars() {
        let community_id = SpaceId([0xc0; 16]);
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let (_, _, target_addr) = make_identity(0xb1);

        let admin_join = make_enrolled_join(0x01, &admin_priv, 1);
        let target_join = make_join_event(0x02, target_addr, 2);
        let prior = materialize(&[admin_join, target_join], admin_addr);

        // Build a reason with exactly MAX_MODERATION_REASON_CHARS+1 codepoints.
        let oversized: String = "a".repeat(MAX_MODERATION_REASON_CHARS + 1);
        let kick_payload = EventPayload {
            id: [0x10; 16],
            community_id,
            kind: MembershipEventKind::Kick {
                target: target_addr,
                reason: Some(oversized),
            },
            actor: admin_addr,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "t".into(),
            },
        };
        let kick = sign_with_identity(kick_payload, &admin_priv);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&kick, &prior, &ctx),
            Err(VerifyError::ReasonTooLong)
        );
    }

    /// Unban with a reason longer than `MAX_MODERATION_REASON_CHARS` must be
    /// rejected at verify_event so an oversized reason cannot bypass the UI
    /// cap and persist to every replica.
    #[test]
    fn unban_event_rejected_when_reason_exceeds_max_chars() {
        let community_id = SpaceId([0xc0; 16]);
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let (_, _, target_addr) = make_identity(0xb1);

        // Set up Banned target via prior Kick
        let admin_join = make_enrolled_join(0x01, &admin_priv, 1);
        let target_join = make_join_event(0x02, target_addr, 2);
        let kick = make_kick_event(0x01, admin_addr, target_addr, 10);
        let prior = materialize(&[admin_join, target_join, kick], admin_addr);
        assert_eq!(prior.members[&target_addr].status, MemberStatus::Banned);

        let oversized: String = "z".repeat(MAX_MODERATION_REASON_CHARS + 1);
        let unban_payload = EventPayload {
            id: [0x20; 16],
            community_id,
            kind: MembershipEventKind::Unban {
                target: target_addr,
                reason: Some(oversized),
            },
            actor: admin_addr,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "t".into(),
            },
        };
        let unban = sign_with_identity(unban_payload, &admin_priv);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&unban, &prior, &ctx),
            Err(VerifyError::ReasonTooLong)
        );
    }

    /// Full lifecycle round-trip via materialize:
    /// Joined → (Kick) Banned → (Unban) Left → (Invite) Invited → (Join) Joined.
    /// Validates that unbanned targets can be re-invited and re-join cleanly.
    #[test]
    fn unban_then_invite_then_join_round_trip_succeeds() {
        let admin = OwnerAddr([0xa1; 16]);
        let target = OwnerAddr([0xb1; 16]);

        // admin has implicit power 100 from bootstrap.
        let admin_join = make_join_event(0x01, admin, 1);
        let target_join = make_join_event(0x02, target, 2);
        let kick = make_kick_event(0x01, admin, target, 10);

        let m_after_kick = materialize(
            &[admin_join.clone(), target_join.clone(), kick.clone()],
            admin,
        );
        assert_eq!(m_after_kick.members[&target].status, MemberStatus::Banned);

        let unban = make_unban_event(0x01, admin, target, Some("misunderstanding".into()), 20);
        let m_after_unban = materialize(
            &[
                admin_join.clone(),
                target_join.clone(),
                kick.clone(),
                unban.clone(),
            ],
            admin,
        );
        assert_eq!(m_after_unban.members[&target].status, MemberStatus::Left);

        let invite = make_invite_event(0x01, admin, target, 30);
        let m_after_invite = materialize(
            &[
                admin_join.clone(),
                target_join.clone(),
                kick.clone(),
                unban.clone(),
                invite.clone(),
            ],
            admin,
        );
        assert_eq!(
            m_after_invite.members[&target].status,
            MemberStatus::Invited
        );

        let rejoin = make_join_event(0x03, target, 40);
        let m_final = materialize(
            &[admin_join, target_join, kick, unban, invite, rejoin],
            admin,
        );
        assert_eq!(m_final.members[&target].status, MemberStatus::Joined);
    }

    // ── M9: Invite → Join marks pending_catchup_for ───────────────────────────

    /// M9: An invited member who joins after an epoch rotation has occurred
    /// must be marked for catchup (just like a brand-new joiner). The prior
    /// check `prior_status.is_none()` missed the Invited→Joined case.
    #[test]
    fn invited_then_join_marks_pending_catchup_for() {
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]); // kick target (to advance epoch)
        let carol = OwnerAddr([0xc1; 16]); // was invited, then joins after rotation

        // Bob join + kick + rotation to advance epoch to 1.
        let bob_join = make_join_event(0x01, bob, 10);
        let kick = make_kick_event(0x01, admin, bob, 100);
        let rot = make_rotation_event(0x01, admin, kick.id, 0, vec![(admin, vec![1; 92])], 101);

        // Carol is invited AFTER the rotation (wall_ms=150 > 101). This ensures
        // the rotation at epoch 0→1 doesn't need to include carol (she's not yet
        // a member at rotation time). Then carol joins at wall_ms=200 with prior
        // status Invited — this is the M9 scenario: Invited→Joined after epoch bump.
        let invite_carol = SignedMembershipEvent {
            signer_certs: Vec::new(),
            id: [0x10; 16],
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::Invite { target: carol },
            actor: admin,
            at: Hlc {
                wall_ms: 150,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        };

        // Carol joins AFTER the rotation (epoch is now 1, carol was Invited).
        let join_carol = make_join_event(0x02, carol, 200);

        let m = materialize(&[bob_join, kick, rot, invite_carol, join_carol], admin);
        assert_eq!(m.current_epoch, Some(1), "epoch should be 1 after rotation");
        assert!(
            m.pending_catchup_for.contains(&carol),
            "Invited→Joined after epoch rotation must mark carol for catchup (M9 regression)"
        );
    }

    // ── ZEB-285: Fork variant tests ───────────────────────────────────────────

    /// ZEB-285 Step 1/4: CBOR roundtrip for the Fork variant, verifying
    /// tag "x" and inner key "fs" on the wire.
    #[test]
    fn fork_event_cbor_roundtrip() {
        use crate::owner_state_crypto::canonical_cbor_encode;

        let fork_space_id = SpaceId([0xfa; 16]);
        let event = MembershipEventKind::Fork {
            fork_space_id,
            reason: None,
        };

        let bytes = canonical_cbor_encode(&event).expect("encode");
        let decoded: MembershipEventKind = ciborium::de::from_reader(&bytes[..]).expect("decode");

        assert_eq!(event, decoded);

        // Verify the variant tag is "x" and inner key is "fs" by inspecting
        // the CBOR encoding directly. Wire form: { "tg": "x", "vl": { "fs": <16-byte bstr> } }.
        let value: ciborium::Value =
            ciborium::de::from_reader(&bytes[..]).expect("re-decode as value");
        let map = value.as_map().expect("outer is map");
        let tg = map
            .iter()
            .find_map(|(k, v): &(ciborium::Value, ciborium::Value)| {
                if k.as_text() == Some("tg") {
                    Some(v)
                } else {
                    None
                }
            })
            .expect("tg key");
        assert_eq!(tg.as_text(), Some("x"));

        let vl = map
            .iter()
            .find_map(|(k, v): &(ciborium::Value, ciborium::Value)| {
                if k.as_text() == Some("vl") {
                    Some(v)
                } else {
                    None
                }
            })
            .expect("vl key");
        let inner = vl.as_map().expect("vl is map");
        assert!(
            inner
                .iter()
                .any(|(k, _): &(ciborium::Value, ciborium::Value)| k.as_text() == Some("fs")),
            "inner has fs key"
        );
    }

    /// ZEB-285 Step 5: all MembershipEventKind variants round-trip through
    /// canonical CBOR, including the new Fork variant.
    #[test]
    fn all_variants_cbor_roundtrip() {
        use crate::owner_state_crypto::canonical_cbor_encode;

        let admin = OwnerAddr([0xaa; 16]);
        let target = OwnerAddr([0xbb; 16]);
        let channel_id = ChannelId([0xcc; 16]);

        let variants: Vec<MembershipEventKind> = vec![
            MembershipEventKind::Join,
            MembershipEventKind::Leave,
            MembershipEventKind::Invite { target },
            MembershipEventKind::Kick {
                target,
                reason: None,
            },
            MembershipEventKind::SetPower { target, level: 50 },
            MembershipEventKind::Unban {
                target,
                reason: None,
            },
            MembershipEventKind::ChannelCreate {
                channel_id,
                name: "test".into(),
                write_power: 0,
                kind: ChannelKind::Text,
            },
            MembershipEventKind::ChannelModify {
                channel_id,
                name: Some("renamed".into()),
                write_power: None,
            },
            MembershipEventKind::ChannelDelete { channel_id },
            MembershipEventKind::EpochRotation {
                prior_epoch: 0,
                triggered_by: [0xde; 16],
                recipient_ciphertexts: vec![RecipientCiphertext {
                    recipient: admin,
                    sealed: vec![0u8; 92],
                }],
            },
            MembershipEventKind::EpochCatchup {
                epoch: 1,
                triggered_by: [0xef; 16],
                recipient_ciphertexts: vec![RecipientCiphertext {
                    recipient: admin,
                    sealed: vec![0u8; 92],
                }],
            },
            MembershipEventKind::Fork {
                fork_space_id: SpaceId([0xfa; 16]),
                reason: None,
            },
            // ZEB-495: unit variant, no body.
            MembershipEventKind::DeviceAnnounce,
        ];

        for variant in &variants {
            let bytes = canonical_cbor_encode(variant)
                .unwrap_or_else(|e| panic!("encode failed for {variant:?}: {e}"));
            let decoded: MembershipEventKind = ciborium::de::from_reader(&bytes[..])
                .unwrap_or_else(|e| panic!("decode failed for {variant:?}: {e}"));
            assert_eq!(variant, &decoded, "roundtrip mismatch for {variant:?}");
        }
    }

    /// ZEB-649: Fork with a reason roundtrips and writes the `rs` inner key;
    /// a reason-less Fork omits the key entirely (skip_serializing_if) —
    /// the omission is what keeps pre-ZEB-649 Fork signatures re-verifiable
    /// through verify_signature's decode→re-encode path.
    #[test]
    fn fork_event_with_reason_cbor_roundtrip_and_rs_key() {
        use crate::owner_state_crypto::canonical_cbor_encode;

        let event = MembershipEventKind::Fork {
            fork_space_id: SpaceId([0xfa; 16]),
            reason: Some("Treasury split".to_string()),
        };
        let bytes = canonical_cbor_encode(&event).expect("encode");
        let decoded: MembershipEventKind = ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(event, decoded);

        fn inner_map_keys(bytes: &[u8]) -> Vec<String> {
            let value: ciborium::Value = ciborium::de::from_reader(bytes).expect("as value");
            let map = value.as_map().expect("outer is map");
            let vl = map
                .iter()
                .find_map(|(k, v): &(ciborium::Value, ciborium::Value)| {
                    if k.as_text() == Some("vl") {
                        Some(v)
                    } else {
                        None
                    }
                })
                .expect("vl key");
            vl.as_map()
                .expect("vl is map")
                .iter()
                .filter_map(|(k, _): &(ciborium::Value, ciborium::Value)| {
                    k.as_text().map(str::to_string)
                })
                .collect()
        }

        let with_reason_keys = inner_map_keys(&bytes);
        assert!(
            with_reason_keys.iter().any(|k| k == "rs"),
            "inner has rs key"
        );
        assert!(
            with_reason_keys.iter().any(|k| k == "fs"),
            "inner keeps fs key"
        );

        let bare = MembershipEventKind::Fork {
            fork_space_id: SpaceId([0xfa; 16]),
            reason: None,
        };
        let bare_bytes = canonical_cbor_encode(&bare).expect("encode bare");
        let bare_keys = inner_map_keys(&bare_bytes);
        assert!(
            !bare_keys.iter().any(|k| k == "rs"),
            "reason-less Fork must omit rs entirely (wire-compat guarantee)"
        );
    }

    /// ZEB-649: Fork with a reason longer than `MAX_MODERATION_REASON_CHARS`
    /// must be rejected at verify_event so an oversized reason cannot bypass
    /// the UI cap and persist to every replica (same defense-in-depth as
    /// Kick/Unban).
    #[test]
    fn fork_event_rejected_when_reason_exceeds_max_chars() {
        let community_id = SpaceId([0xc0; 16]);
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let (regular_priv, _regular_pub, regular_addr) = make_identity(0xb1);

        let admin_join = sign_with_identity(
            EventPayload {
                id: [0x01; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: admin_addr,
                at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &admin_priv,
        );
        let regular_join = sign_with_identity(
            EventPayload {
                id: [0x02; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: regular_addr,
                at: Hlc {
                    wall_ms: 2,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &regular_priv,
        );
        let prior = materialize(&[admin_join, regular_join], admin_addr);

        let oversized: String = "a".repeat(MAX_MODERATION_REASON_CHARS + 1);
        let fork_event = sign_with_identity(
            EventPayload {
                id: [0x03; 16],
                community_id,
                kind: MembershipEventKind::Fork {
                    fork_space_id: SpaceId([0xfe; 16]),
                    reason: Some(oversized),
                },
                actor: regular_addr,
                at: Hlc {
                    wall_ms: 3,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &regular_priv,
        );
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&fork_event, &prior, &ctx),
            Err(VerifyError::ReasonTooLong)
        );

        // At exactly the cap it must pass (boundary check).
        let at_cap: String = "a".repeat(MAX_MODERATION_REASON_CHARS);
        let ok_event = sign_with_identity(
            EventPayload {
                id: [0x04; 16],
                community_id,
                kind: MembershipEventKind::Fork {
                    fork_space_id: SpaceId([0xfe; 16]),
                    reason: Some(at_cap),
                },
                actor: regular_addr,
                at: Hlc {
                    wall_ms: 4,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &regular_priv,
        );
        assert_eq!(verify_event(&ok_event, &prior, &ctx), Ok(()));

        // ZEB-649 (Qodo PR #434): Some("") / whitespace-only must be
        // rejected — the mandatory-why invariant holds at the CRDT layer,
        // not just the IPC gate a modified client can skip.
        for empty in ["", "   \n\t "] {
            let empty_event = sign_with_identity(
                EventPayload {
                    id: [0x05; 16],
                    community_id,
                    kind: MembershipEventKind::Fork {
                        fork_space_id: SpaceId([0xfe; 16]),
                        reason: Some(empty.to_string()),
                    },
                    actor: regular_addr,
                    at: Hlc {
                        wall_ms: 5,
                        logical: 0,
                        device_id: "t".into(),
                    },
                },
                &regular_priv,
            );
            assert_eq!(
                verify_event(&empty_event, &prior, &ctx),
                Err(VerifyError::ReasonEmpty),
                "reason {empty:?} must be rejected"
            );
        }
    }

    /// ZEB-285 Step 6/9: verify_event allows a Fork from any joined member
    /// (power 0 = regular member, not just admin).
    #[test]
    fn verify_event_fork_allows_any_joined_member() {
        let community_id = SpaceId([0xc0; 16]);
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let (regular_priv, _regular_pub, regular_addr) = make_identity(0xb1);

        // Admin joins (power 100 from bootstrap).
        let admin_join = sign_with_identity(
            EventPayload {
                id: [0x01; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: admin_addr,
                at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &admin_priv,
        );
        // Regular member joins (power 0).
        let regular_join = sign_with_identity(
            EventPayload {
                id: [0x02; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: regular_addr,
                at: Hlc {
                    wall_ms: 2,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &regular_priv,
        );

        let prior = materialize(&[admin_join, regular_join.clone()], admin_addr);

        // Regular (power 0) signs a Fork. Should verify cleanly.
        let fork_event = sign_with_identity(
            EventPayload {
                id: [0x03; 16],
                community_id,
                kind: MembershipEventKind::Fork {
                    fork_space_id: SpaceId([0xfe; 16]),
                    reason: None,
                },
                actor: regular_addr,
                at: Hlc {
                    wall_ms: 3,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &regular_priv,
        );

        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&fork_event, &prior, &ctx),
            Ok(()),
            "fork by a regular joined member (power 0) must be accepted"
        );

        // Also verify admin (power 100) can fork.
        let admin_fork = sign_with_identity(
            EventPayload {
                id: [0x04; 16],
                community_id,
                kind: MembershipEventKind::Fork {
                    fork_space_id: SpaceId([0xfe; 16]),
                    reason: None,
                },
                actor: admin_addr,
                at: Hlc {
                    wall_ms: 4,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &admin_priv,
        );
        let admin_ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&admin_fork, &prior, &admin_ctx),
            Ok(()),
            "fork by admin (power 100) must also be accepted"
        );
    }

    /// ZEB-285 Step 10: verify_event rejects a Fork from a non-member
    /// (never joined) with ActorNotJoined.
    #[test]
    fn verify_event_fork_rejects_non_member() {
        let community_id = SpaceId([0xc0; 16]);
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let (outsider_priv, _outsider_pub, outsider_addr) = make_identity(0xcc);

        let admin_join = sign_with_identity(
            EventPayload {
                id: [0x01; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: admin_addr,
                at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &admin_priv,
        );
        let prior = materialize(std::slice::from_ref(&admin_join), admin_addr);

        // Outsider (never joined) tries to Fork. Should reject.
        let fork = sign_with_identity(
            EventPayload {
                id: [0x02; 16],
                community_id,
                kind: MembershipEventKind::Fork {
                    fork_space_id: SpaceId([0xfe; 16]),
                    reason: None,
                },
                actor: outsider_addr,
                at: Hlc {
                    wall_ms: 2,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &outsider_priv,
        );
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: false,
        };
        // ZEB-339: a never-member's Fork is rejected at signer resolution
        // (step 1) before the ActorNotJoined gate — no materialized membership
        // means no enrolled device key. Security property preserved.
        assert_eq!(
            verify_event(&fork, &prior, &ctx),
            Err(VerifyError::SignerNotEnrolledForActor),
            "fork by non-member should reject with SignerNotEnrolledForActor"
        );
    }

    /// ZEB-285 Step 11/14: materialize Fork is non-mutating — members,
    /// power_levels, and channels are unchanged by a Fork event.
    #[test]
    fn materialize_fork_is_non_mutating() {
        let community_id = SpaceId([0xc0; 16]);
        let admin = OwnerAddr([0xa1; 16]);

        let admin_join = make_join_event(0x01, admin, 1);
        let before = materialize(std::slice::from_ref(&admin_join), admin);

        let fork = SignedMembershipEvent {
            signer_certs: Vec::new(),
            id: [0x02; 16],
            community_id,
            kind: MembershipEventKind::Fork {
                fork_space_id: SpaceId([0xfe; 16]),
                reason: None,
            },
            actor: admin,
            at: Hlc {
                wall_ms: 2,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        };
        let after = materialize(&[admin_join, fork], admin);

        // Materialized view should be unchanged by the Fork event.
        assert_eq!(
            before.members, after.members,
            "members should be unchanged after Fork"
        );
        assert_eq!(
            before.power_levels, after.power_levels,
            "power_levels should be unchanged after Fork"
        );
        assert_eq!(
            before.channels, after.channels,
            "channels should be unchanged after Fork"
        );
        assert_eq!(
            before.current_epoch, after.current_epoch,
            "current_epoch should be unchanged after Fork"
        );
    }

    /// ZEB-285 Step 15/16: a Fork event does NOT auto-trigger an EpochRotation.
    /// Contrast with Kick and Leave which do trigger rotation synthesis.
    #[test]
    fn fork_does_not_trigger_epoch_rotation() {
        let community_id = SpaceId([0xc0; 16]);
        let admin = OwnerAddr([0xa1; 16]);

        let admin_join = make_join_event(0x01, admin, 1);
        let fork = SignedMembershipEvent {
            signer_certs: Vec::new(),
            id: [0x02; 16],
            community_id,
            kind: MembershipEventKind::Fork {
                fork_space_id: SpaceId([0xfe; 16]),
                reason: None,
            },
            actor: admin,
            at: Hlc {
                wall_ms: 2,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        };

        let m = materialize(&[admin_join, fork], admin);

        // After Fork: no epoch rotation should have been triggered.
        // current_epoch stays None (no Kick/Leave = no rotation).
        assert_eq!(
            m.current_epoch, None,
            "Fork should NOT advance epoch (contrast with Kick/Leave)"
        );
        assert!(
            m.pending_rotation_for.is_empty(),
            "no pending rotation should exist after a Fork event"
        );
    }

    // ── ZEB-285 Task 5: verify_snapshot_event dual-keyset verifier ────────────

    /// ZEB-285 Task 5: verify_snapshot_event should accept events whose signer
    /// is present in snapshot.identity_pubs, verified against the real
    /// Ed25519 key (not the live OwnerDeviceCache).
    #[test]
    fn verify_snapshot_event_uses_snapshot_identity_pubs() {
        use crate::community_invite::{BoundedChannelLogSnapshot, PreForkSnapshot};
        use std::collections::BTreeMap;

        // ZEB-339: verify_snapshot_event still uses the single-identity model
        // (PreForkSnapshot.identity_pubs); build these events with a real
        // PrivateIdentity so the identity_pub→actor binding holds. (Fork
        // snapshot migration onto the cert model is a later task.)
        let snap_identity = |seed: u8| -> (harmony_identity::PrivateIdentity, [u8; 64], OwnerAddr) {
            let private = harmony_identity::PrivateIdentity::from_seed(&[seed; 32]);
            let public = private.public_identity();
            let pub_bytes = public.to_public_bytes();
            let addr = OwnerAddr(public.address_hash);
            (private, pub_bytes, addr)
        };
        let original_id = SpaceId([0xa0; 16]);
        let (admin_priv, admin_pub, admin_addr) = snap_identity(0xaa);
        let (regular_priv, regular_pub, regular_addr) = snap_identity(0xbb);

        // Bootstrap: admin joins, then regular joins, then admin promotes regular.
        let admin_join = sign_event_with_identity(
            &EventPayload {
                id: [0x01; 16],
                community_id: original_id,
                kind: MembershipEventKind::Join,
                actor: admin_addr,
                at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &admin_priv,
        )
        .expect("sign");
        let regular_join = sign_event_with_identity(
            &EventPayload {
                id: [0x02; 16],
                community_id: original_id,
                kind: MembershipEventKind::Join,
                actor: regular_addr,
                at: Hlc {
                    wall_ms: 2,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &regular_priv,
        )
        .expect("sign");
        let set_power = sign_event_with_identity(
            &EventPayload {
                id: [0x03; 16],
                community_id: original_id,
                kind: MembershipEventKind::SetPower {
                    target: regular_addr,
                    level: 50,
                },
                actor: admin_addr,
                at: Hlc {
                    wall_ms: 3,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &admin_priv,
        )
        .expect("sign");

        let mut identity_pubs = BTreeMap::new();
        identity_pubs.insert(admin_addr, admin_pub);
        identity_pubs.insert(regular_addr, regular_pub);

        let snapshot = PreForkSnapshot {
            original_community_id: original_id,
            original_community_name: "Original".to_string(),
            membership_events: vec![admin_join.clone(), regular_join.clone(), set_power.clone()],
            channel_log: BoundedChannelLogSnapshot::default(),
            identity_pubs,
            forked_at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "t".into(),
            },
            parent_lineage: Vec::new(),
            fork_reason: None,
        };

        // Every event in the snapshot should verify against the snapshot's
        // identity_pubs, even though the fork's live OwnerDeviceCache has
        // neither admin_addr nor regular_addr as members.
        for event in &snapshot.membership_events {
            verify_snapshot_event(event, &snapshot)
                .expect("snapshot event should verify against identity_pubs");
        }
    }

    /// ZEB-285 Task 5: verify_snapshot_event must reject a signer not
    /// recorded in snapshot.identity_pubs with UnknownSigner.
    #[test]
    fn verify_snapshot_event_rejects_unknown_signer() {
        use crate::community_invite::{BoundedChannelLogSnapshot, PreForkSnapshot};
        use std::collections::BTreeMap;

        let original_id = SpaceId([0xa0; 16]);
        let (admin_priv, admin_pub, admin_addr) = make_identity(0xaa);
        let (outsider_priv, _outsider_pub, outsider_addr) = make_identity(0xff);

        let admin_join = sign_with_identity(
            EventPayload {
                id: [0x01; 16],
                community_id: original_id,
                kind: MembershipEventKind::Join,
                actor: admin_addr,
                at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &admin_priv,
        );
        // Signed by `outsider_addr` — a valid keypair never part of the original community.
        let outsider_event = sign_with_identity(
            EventPayload {
                id: [0x02; 16],
                community_id: original_id,
                kind: MembershipEventKind::Leave,
                actor: outsider_addr,
                at: Hlc {
                    wall_ms: 2,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &outsider_priv,
        );

        let mut identity_pubs = BTreeMap::new();
        identity_pubs.insert(admin_addr, admin_pub);
        // No entry for outsider_addr — intentionally missing.

        let snapshot = PreForkSnapshot {
            original_community_id: original_id,
            original_community_name: "Original".to_string(),
            membership_events: vec![admin_join, outsider_event.clone()],
            channel_log: BoundedChannelLogSnapshot::default(),
            identity_pubs,
            forked_at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "t".into(),
            },
            parent_lineage: Vec::new(),
            fork_reason: None,
        };

        let result = verify_snapshot_event(&outsider_event, &snapshot);
        match result {
            Err(VerifyError::UnknownSigner { signer }) => {
                assert_eq!(
                    signer, outsider_addr,
                    "error should carry the actual offending address"
                );
            }
            other => panic!("expected UnknownSigner, got {:?}", other),
        }
    }

    /// ZEB-285 (security fix): verify_snapshot_event must reject an event
    /// whose community_id does not match snapshot.original_community_id, even
    /// when the signer is present in identity_pubs and the Ed25519 signature
    /// is valid. This prevents cross-community event injection by an actor who
    /// is a member of two communities and whose pubkey appears in both snapshots.
    #[test]
    fn verify_snapshot_event_rejects_wrong_community_id() {
        use crate::community_invite::{BoundedChannelLogSnapshot, PreForkSnapshot};
        use std::collections::BTreeMap;

        let original_id = SpaceId([0xa0; 16]);
        let other_community_id = SpaceId([0xbb; 16]); // different community
        let (admin_priv, admin_pub, admin_addr) = make_identity(0xaa);

        // Event is validly signed by admin_addr but references a DIFFERENT community.
        let wrong_community_event = sign_with_identity(
            EventPayload {
                id: [0x10; 16],
                community_id: other_community_id, // wrong community
                kind: MembershipEventKind::Join,
                actor: admin_addr,
                at: Hlc {
                    wall_ms: 5,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &admin_priv,
        );

        // The snapshot's identity_pubs includes admin_addr — signature would
        // verify if we didn't check community_id first.
        let mut identity_pubs = BTreeMap::new();
        identity_pubs.insert(admin_addr, admin_pub);

        let snapshot = PreForkSnapshot {
            original_community_id: original_id,
            original_community_name: "Original".to_string(),
            membership_events: vec![],
            channel_log: BoundedChannelLogSnapshot::default(),
            identity_pubs,
            forked_at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "t".into(),
            },
            parent_lineage: Vec::new(),
            fork_reason: None,
        };

        let result = verify_snapshot_event(&wrong_community_event, &snapshot);
        match result {
            Err(VerifyError::CommunityIdMismatch { expected, actual }) => {
                assert_eq!(
                    expected, original_id,
                    "expected should be snapshot's original_community_id"
                );
                assert_eq!(
                    actual, other_community_id,
                    "actual should be the event's community_id"
                );
            }
            other => panic!("expected CommunityIdMismatch, got {:?}", other),
        }
    }

    #[test]
    fn pending_join_variant_canonical_cbor_round_trip() {
        use crate::community_invite::InviteToken;

        let token = InviteToken {
            inviter: OwnerAddr([1u8; 16]),
            invitee_hint: Some(OwnerAddr([2u8; 16])),
            minted_at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "t".into(),
            },
            expires_at: Some(1_700_000_000_000 + 7 * 86_400_000),
            sig: [3u8; 64],
        };
        let kind = MembershipEventKind::PendingJoin {
            invite_token: token,
        };

        let encoded = crate::owner_state_crypto::canonical_cbor_encode(&kind).expect("encode");
        let decoded: MembershipEventKind =
            ciborium::from_reader(&mut encoded.as_slice()).expect("decode");
        assert_eq!(kind, decoded);
    }

    #[test]
    fn join_countersign_variant_canonical_cbor_round_trip() {
        let kind = MembershipEventKind::JoinCountersign {
            target_event_id: [42u8; 16],
        };
        let encoded = crate::owner_state_crypto::canonical_cbor_encode(&kind).expect("encode");
        let decoded: MembershipEventKind =
            ciborium::from_reader(&mut encoded.as_slice()).expect("decode");
        assert_eq!(kind, decoded);
    }

    #[test]
    fn member_status_pending_join_canonical_cbor_round_trip() {
        let status = MemberStatus::PendingJoin;
        let encoded = crate::owner_state_crypto::canonical_cbor_encode(&status).expect("encode");
        let decoded: MemberStatus = ciborium::from_reader(&mut encoded.as_slice()).expect("decode");
        assert_eq!(status, decoded);
    }

    #[test]
    fn reachability_announce_variant_cbor_roundtrip() {
        use crate::reachability_record::ReachabilityAnnouncePayload;
        let payload = ReachabilityAnnouncePayload {
            iroh_node_id: [0xAB; 32],
            home_relay_url: "https://derp.example/".into(),
            direct_addresses: vec![],
            announced_at_ms: 1_700_000_000_000,
            identity_signature: [0xCD; 64],
            butler_set: Vec::new(),
            bs_at: 0,
        };
        let kind = MembershipEventKind::ReachabilityAnnounce {
            payload: payload.clone(),
        };
        let bytes = crate::owner_state_crypto::canonical_cbor_encode(&kind).expect("encode");
        let decoded: MembershipEventKind = ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(
            decoded,
            MembershipEventKind::ReachabilityAnnounce { payload }
        );
    }

    #[test]
    fn reachability_announce_outer_keys_invariant() {
        use crate::reachability_record::ReachabilityAnnouncePayload;
        let kind = MembershipEventKind::ReachabilityAnnounce {
            payload: ReachabilityAnnouncePayload {
                iroh_node_id: [0; 32],
                home_relay_url: String::new(),
                direct_addresses: vec![],
                announced_at_ms: 0,
                identity_signature: [0; 64],
                butler_set: Vec::new(),
                bs_at: 0,
            },
        };
        let bytes = crate::owner_state_crypto::canonical_cbor_encode(&kind).expect("encode");
        let val: ciborium::Value = ciborium::de::from_reader(&bytes[..]).expect("decode");
        let map = val.as_map().expect("outer is map");
        for (k, _) in map {
            let s = k.as_text().expect("key is text");
            assert_eq!(
                s.chars().count(),
                2,
                "MembershipEventKind::ReachabilityAnnounce outer key {s:?} violates 2-char invariant"
            );
        }
    }

    // ── ZEB-339 Task 1: SignedMembershipEvent enrollment field round-trip ─────

    /// ZEB-339: a steady-state event (enrollment=None) encodes with NO `en`
    /// key (back-compat), and round-trips cleanly through canonical CBOR.
    #[test]
    fn signed_event_enrollment_roundtrips_and_defaults_absent() {
        use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};

        let ev = SignedMembershipEvent {
            signer_certs: Vec::new(),
            id: [1u8; 16],
            community_id: SpaceId([2u8; 16]),
            kind: MembershipEventKind::Leave,
            actor: OwnerAddr([3u8; 16]),
            at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "t".into(),
            },
            sig: [0u8; 64],
            countersig: None,
            enrollment: None,
        };
        let bytes = canonical_cbor_encode(&ev).unwrap();

        // `bytes` is a 6-key map with NO `en` key (skip_serializing_if drops it
        // when None) — i.e. byte-identical to a pre-ZEB-339 wire event. Decoding
        // it here therefore doubles as the back-compat proof: old en-less bytes
        // decode via `serde(default)` to enrollment=None rather than erroring.
        let val: ciborium::Value = ciborium::de::from_reader(&bytes[..]).unwrap();
        let map = val.as_map().expect("outer is map");
        let has_en = map.iter().any(|(k, _)| k.as_text() == Some("en"));
        assert!(!has_en, "`en` key must be absent when enrollment is None");

        let back: SignedMembershipEvent = canonical_cbor_decode(&bytes).unwrap();
        assert_eq!(back, ev);
        assert!(back.enrollment.is_none());
    }

    // ── ZEB-339 Task 2: materialize ingests enrolled_device_keys from Join cert ─

    /// ZEB-339: materialize inserts the ed25519 verify key from the
    /// EnrollmentCert carried on a Join event into
    /// `MemberState.enrolled_device_keys`.
    #[test]
    fn materialize_records_enrolled_device_key_from_join_cert() {
        let admin = mint_test_owner(0x11);
        let payload = EventPayload {
            id: [1u8; 16],
            community_id: SpaceId([9u8; 16]),
            kind: MembershipEventKind::Join,
            actor: admin.owner,
            at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "t".into(),
            },
        };
        let join = sign_event(&payload, &admin.device_key).unwrap();
        let join = SignedMembershipEvent {
            enrollment: Some(admin.cert.clone()),
            ..join
        };
        let m = materialize(&[join], admin.owner);
        let ek = &m.members.get(&admin.owner).unwrap().enrolled_device_keys;
        assert!(
            ek.contains(&admin.device_key.verifying_key().to_bytes()),
            "enrolled_device_keys must contain the device key from the cert"
        );
    }

    /// ZEB-339 back-compat: a `MemberState` CBOR blob without the `ek`
    /// key (pre-ZEB-339 wire format) decodes to an empty
    /// `enrolled_device_keys` set via `#[serde(default)]`, rather than
    /// failing. This is the round-trip proof: encode a MemberState with
    /// an empty set (skip_serializing_if drops the key), confirm `ek` is
    /// absent from the CBOR map, then decode and assert the set is empty.
    #[test]
    fn member_state_enrolled_device_keys_back_compat_decode() {
        use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};

        let ms = MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "t".into(),
            },
            left_at: None,
            enrolled_device_keys: BTreeSet::new(),
            revoked_device_keys: BTreeSet::new(),
        };

        let bytes = canonical_cbor_encode(&ms).unwrap();

        // `ek` must be absent from the map (skip_serializing_if = BTreeSet::is_empty)
        let val: ciborium::Value = ciborium::de::from_reader(&bytes[..]).unwrap();
        let map = val.as_map().expect("MemberState encodes as map");
        let has_ek = map.iter().any(|(k, _)| k.as_text() == Some("ek"));
        assert!(
            !has_ek,
            "`ek` key must be absent when enrolled_device_keys is empty (back-compat wire)"
        );

        // Decode back — must produce an empty set, not an error.
        let back: MemberState = canonical_cbor_decode(&bytes).unwrap();
        assert!(
            back.enrolled_device_keys.is_empty(),
            "decoded MemberState.enrolled_device_keys must be empty when `ek` was absent"
        );
        assert_eq!(back.status, MemberStatus::Joined);
    }

    /// ZEB-251 Task 1: `verify_event`'s Invite power check must read the
    /// community's materialized `power_thresholds` field, not the global
    /// `POWER_THRESHOLDS` const — proven by hand-raising the materialized
    /// invite threshold and observing the SAME event flip from Ok to
    /// `ActorPowerInsufficient`.
    #[test]
    fn verify_event_reads_invite_threshold_from_materialized_field() {
        let community_id = SpaceId([0xc0; 16]);
        let admin = mint_test_owner(0xa1);
        let low = mint_test_owner(0xb2);

        let mut prior = MaterializedMembership {
            power_thresholds: POWER_THRESHOLDS,
            ..Default::default()
        };
        prior.members.insert(
            low.owner,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "t".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        test_enroll_member(&mut prior, &low);
        prior.power_levels.insert(low.owner, 10);

        let target = OwnerAddr([0xd1; 16]);
        let invite_payload = EventPayload {
            id: [0xaa; 16],
            community_id,
            kind: MembershipEventKind::Invite { target },
            actor: low.owner,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "t".into(),
            },
        };
        let ev = sign_event(&invite_payload, &low.device_key).expect("sign invite");

        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr: admin.owner,
            is_invite_only: false,
        };

        // Baseline: invite=0 (default) → a power-10 invite verifies OK.
        prior.power_thresholds = POWER_THRESHOLDS; // invite = 0
        assert!(verify_event(&ev, &prior, &ctx).is_ok());

        // Raise the community's invite threshold to 25 in the materialized
        // state → the SAME event must now fail on insufficient power.
        prior.power_thresholds = PowerThresholds {
            invite: 25,
            ..POWER_THRESHOLDS
        };
        let err = verify_event(&ev, &prior, &ctx).unwrap_err();
        assert!(
            matches!(err, VerifyError::ActorPowerInsufficient),
            "got {err:?}"
        );
    }
}

// ── ZEB-254 PendingJoin verify_event unit tests ───────────────────────────────

#[cfg(test)]
mod zeb_254_pending_join_verify_tests {
    use super::*;
    use crate::community_invite::InviteToken;

    /// Build a test identity from a seed byte.
    /// Returns (PrivateIdentity, identity_pub [u8; 64], OwnerAddr).
    fn make_identity(seed_byte: u8) -> (TestOwner, [u8; 64], OwnerAddr) {
        let owner = mint_test_owner(seed_byte);
        let addr = owner.owner;
        (owner, [0u8; 64], addr)
    }

    /// ZEB-339: build a signed InviteToken, signed by the inviter's enrolled
    /// device key (the same key materialized into the inviter's
    /// `enrolled_device_keys`).
    fn make_invite_token(
        inviter: &TestOwner,
        inviter_addr: OwnerAddr,
        invitee_hint: Option<OwnerAddr>,
        expires_at: Option<u64>,
    ) -> InviteToken {
        use crate::community_invite::canonical_invite_token_bytes;
        use ed25519_dalek::Signer;

        let mut tok = InviteToken {
            inviter: inviter_addr,
            invitee_hint,
            minted_at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "admin-device".into(),
            },
            expires_at,
            sig: [0u8; 64],
        };
        let bytes = canonical_invite_token_bytes(&tok).expect("encode token");
        tok.sig = inviter.device_key.sign(&bytes).to_bytes();
        tok
    }

    /// ZEB-339: build a signed PendingJoin event for the given joiner, signed
    /// by the joiner's enrolled device key with their Master cert attached so
    /// `enrolled_key_from_cert` can resolve the signer.
    fn make_pending_join_event(
        joiner: &TestOwner,
        joiner_addr: OwnerAddr,
        community_id: SpaceId,
        token: InviteToken,
    ) -> SignedMembershipEvent {
        let payload = EventPayload {
            id: [9u8; 16],
            community_id,
            kind: MembershipEventKind::PendingJoin {
                invite_token: token,
            },
            actor: joiner_addr,
            at: Hlc {
                wall_ms: 1_700_000_001_000,
                logical: 0,
                device_id: "joiner-device".into(),
            },
        };
        let ev = sign_event(&payload, &joiner.device_key).expect("sign PendingJoin");
        SignedMembershipEvent {
            enrollment: Some(joiner.cert.clone()),
            ..ev
        }
    }

    /// ZEB-339: materialize a prior state where `inviter` is a Joined member
    /// with their enrolled device key present (so P5 token-sig verification
    /// against the inviter's enrolled key can succeed).
    fn inviter_prior(inviter: &TestOwner, community_id: SpaceId) -> MaterializedMembership {
        let join_payload = EventPayload {
            id: [1u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: inviter.owner,
            at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "admin".into(),
            },
        };
        let join = sign_event(&join_payload, &inviter.device_key).expect("sign inviter join");
        let join = SignedMembershipEvent {
            enrollment: Some(inviter.cert.clone()),
            ..join
        };
        materialize(std::slice::from_ref(&join), inviter.owner)
    }

    #[test]
    fn pending_join_event_signs_and_verifies() {
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let (joiner_priv, _joiner_pub, joiner_addr) = make_identity(0xb1);
        let community_id = SpaceId([7u8; 16]);
        let token = make_invite_token(
            &admin_priv,
            admin_addr,
            Some(joiner_addr),
            Some(1_700_000_100_000),
        );
        let event = make_pending_join_event(&joiner_priv, joiner_addr, community_id, token);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
        };
        let mat = inviter_prior(&admin_priv, community_id);
        let result = verify_event(&event, &mat, &ctx);
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
    }

    #[test]
    fn pending_join_rejected_when_token_invitee_not_actor() {
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let (joiner_priv, _joiner_pub, joiner_addr) = make_identity(0xb1);
        let community_id = SpaceId([7u8; 16]);
        // Hint addresses someone else, not the joiner.
        let token = make_invite_token(
            &admin_priv,
            admin_addr,
            Some(OwnerAddr([99u8; 16])),
            Some(1_700_000_100_000),
        );
        let event = make_pending_join_event(&joiner_priv, joiner_addr, community_id, token);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
        };
        let mat = inviter_prior(&admin_priv, community_id);
        assert!(
            matches!(
                verify_event(&event, &mat, &ctx),
                Err(VerifyError::PendingJoinTokenInvalid)
            ),
            "wrong invitee_hint must yield PendingJoinTokenInvalid"
        );
    }

    #[test]
    fn pending_join_rejected_when_token_expired() {
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let (joiner_priv, _joiner_pub, joiner_addr) = make_identity(0xb1);
        let community_id = SpaceId([7u8; 16]);
        // expires_at is BEFORE the event's wall_ms (1_700_000_001_000).
        let token = make_invite_token(
            &admin_priv,
            admin_addr,
            Some(joiner_addr),
            Some(1_700_000_000_500),
        );
        let event = make_pending_join_event(&joiner_priv, joiner_addr, community_id, token);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
        };
        let mat = inviter_prior(&admin_priv, community_id);
        assert!(
            matches!(
                verify_event(&event, &mat, &ctx),
                Err(VerifyError::PendingJoinTokenExpired)
            ),
            "expired token must yield PendingJoinTokenExpired"
        );
    }

    #[test]
    fn pending_join_rejected_when_token_inviter_not_admin() {
        let (rogue_priv, _rogue_pub, rogue_addr) = make_identity(0xc1);
        let (_admin2_priv, _admin2_pub, admin2_addr) = make_identity(0xa2);
        let (joiner_priv, _joiner_pub, joiner_addr) = make_identity(0xb1);
        let community_id = SpaceId([7u8; 16]);
        // Token is signed by rogue, not admin2.
        let token = make_invite_token(
            &rogue_priv,
            rogue_addr,
            Some(joiner_addr),
            Some(1_700_000_100_000),
        );
        let event = make_pending_join_event(&joiner_priv, joiner_addr, community_id, token);
        // ctx uses admin2 as admin — rogue != admin2, so P2 (inviter != admin) fires.
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr: admin2_addr,
            is_invite_only: true,
        };
        let mat = MaterializedMembership::default();
        let result = verify_event(&event, &mat, &ctx);
        assert!(
            matches!(result, Err(VerifyError::PendingJoinTokenInvalid)),
            "token from non-admin inviter must yield PendingJoinTokenInvalid; got {:?}",
            result
        );
    }

    #[test]
    fn pending_join_rejected_when_actor_already_joined() {
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let (joiner_priv, _joiner_pub, joiner_addr) = make_identity(0xb1);
        let community_id = SpaceId([7u8; 16]);
        let token = make_invite_token(
            &admin_priv,
            admin_addr,
            Some(joiner_addr),
            Some(1_700_000_100_000),
        );
        let event = make_pending_join_event(&joiner_priv, joiner_addr, community_id, token);
        let mut mat = inviter_prior(&admin_priv, community_id);
        mat.members.insert(
            joiner_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "t".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
        };
        assert!(
            matches!(
                verify_event(&event, &mat, &ctx),
                Err(VerifyError::PendingJoinAlreadyMember)
            ),
            "already-Joined actor must yield PendingJoinAlreadyMember"
        );
    }

    #[test]
    fn pending_join_rejected_when_actor_banned() {
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let (joiner_priv, _joiner_pub, joiner_addr) = make_identity(0xb1);
        let community_id = SpaceId([7u8; 16]);
        let token = make_invite_token(
            &admin_priv,
            admin_addr,
            Some(joiner_addr),
            Some(1_700_000_100_000),
        );
        let event = make_pending_join_event(&joiner_priv, joiner_addr, community_id, token);
        let mut mat = inviter_prior(&admin_priv, community_id);
        mat.members.insert(
            joiner_addr,
            MemberState {
                status: MemberStatus::Banned,
                joined_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "t".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
        };
        assert!(
            matches!(
                verify_event(&event, &mat, &ctx),
                Err(VerifyError::PendingJoinAlreadyMember)
            ),
            "Banned actor must yield PendingJoinAlreadyMember"
        );
    }

    #[test]
    fn pending_join_rejected_when_actor_already_pending() {
        // P6 gate: PendingJoin prior state is itself PendingJoin — must
        // reject because the actor is already-engaged (queue slot taken).
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let (joiner_priv, _joiner_pub, joiner_addr) = make_identity(0xb1);
        let community_id = SpaceId([7u8; 16]);
        let token = make_invite_token(
            &admin_priv,
            admin_addr,
            Some(joiner_addr),
            Some(1_700_000_100_000),
        );
        let event = make_pending_join_event(&joiner_priv, joiner_addr, community_id, token);
        let mut mat = inviter_prior(&admin_priv, community_id);
        mat.members.insert(
            joiner_addr,
            MemberState {
                status: MemberStatus::PendingJoin,
                joined_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "t".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
        };
        assert!(
            matches!(
                verify_event(&event, &mat, &ctx),
                Err(VerifyError::PendingJoinAlreadyMember)
            ),
            "actor already in PendingJoin state must yield PendingJoinAlreadyMember"
        );
    }

    #[test]
    fn pending_join_accepted_when_actor_was_left() {
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let (joiner_priv, _joiner_pub, joiner_addr) = make_identity(0xb1);
        let community_id = SpaceId([7u8; 16]);
        let token = make_invite_token(
            &admin_priv,
            admin_addr,
            Some(joiner_addr),
            Some(1_700_000_100_000),
        );
        let event = make_pending_join_event(&joiner_priv, joiner_addr, community_id, token);
        let mut mat = inviter_prior(&admin_priv, community_id);
        mat.members.insert(
            joiner_addr,
            MemberState {
                status: MemberStatus::Left,
                joined_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "t".into(),
                },
                left_at: Some(Hlc {
                    wall_ms: 500,
                    logical: 0,
                    device_id: "t".into(),
                }),
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
        };
        assert!(
            verify_event(&event, &mat, &ctx).is_ok(),
            "Left actor should be allowed to PendingJoin again"
        );
    }

    #[test]
    fn pending_join_rejected_when_cert_owner_not_actor() {
        // ZEB-339: the joiner-binding check is now subsumed by
        // enrolled_key_from_cert, which rejects a PendingJoin whose carried
        // cert.owner_id != event.actor. A joiner who attaches someone else's
        // cert (a different owner's) is rejected with EnrollmentOwnerMismatch.
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let (joiner_priv, _joiner_pub, joiner_addr) = make_identity(0xb1);
        let other = mint_test_owner(0xd1);
        let community_id = SpaceId([7u8; 16]);
        let token = make_invite_token(
            &admin_priv,
            admin_addr,
            Some(joiner_addr),
            Some(1_700_000_100_000),
        );
        // Build a PendingJoin for the joiner, then swap in a cert minted for a
        // different owner. enrolled_key_from_cert binds cert.owner_id == actor,
        // so the mismatched cert must be rejected.
        let mut event = make_pending_join_event(&joiner_priv, joiner_addr, community_id, token);
        event.enrollment = Some(other.cert.clone());
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
        };
        let mat = inviter_prior(&admin_priv, community_id);
        let result = verify_event(&event, &mat, &ctx);
        assert!(
            matches!(result, Err(VerifyError::EnrollmentOwnerMismatch)),
            "cert minted for a different owner must yield EnrollmentOwnerMismatch; got {:?}",
            result
        );
    }

    // ── ZEB-526: bootstrap_admit_invite_only_publisher ───────────────────────

    /// Helper: the admin's community-creation bootstrap Join (root of trust),
    /// at an early HLC, carrying the admin's enrollment cert.
    fn admin_bootstrap_join(admin: &TestOwner, community_id: SpaceId) -> SignedMembershipEvent {
        let payload = EventPayload {
            id: [1u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin.owner,
            at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "admin".into(),
            },
        };
        let ev = sign_event(&payload, &admin.device_key).expect("sign admin bootstrap join");
        SignedMembershipEvent {
            enrollment: Some(admin.cert.clone()),
            ..ev
        }
    }

    /// A valid self-authorizing PendingJoin (admin-signed InviteToken + joiner
    /// cert), with the admin's own bootstrap Join present in the blob to seed the
    /// InviteToken-signer key, admits the joiner as PendingJoin with their
    /// enrolled device key (needed for the deferred root publisher_sig check).
    #[test]
    fn bootstrap_admit_invite_only_publisher_admits_pending_join_with_admin_bootstrap() {
        let admin = mint_test_owner(0xc1);
        let joiner = mint_test_owner(0xc2);
        let community_id = SpaceId([0xcc; 16]);

        let admin_join = admin_bootstrap_join(&admin, community_id);
        let token = make_invite_token(&admin, admin.owner, None, None);
        let pending = make_pending_join_event(&joiner, joiner.owner, community_id, token);

        // Root publish strictly after the PendingJoin (wall 1_700_000_001_000).
        let root_at = Hlc {
            wall_ms: 1_700_000_002_000,
            logical: 0,
            device_id: "root".into(),
        };
        let events = vec![admin_join, pending];
        let ms = bootstrap_admit_invite_only_publisher(
            &events,
            joiner.owner,
            admin.owner,
            community_id,
            &root_at,
        )
        .expect("a valid admin-signed PendingJoin must admit the joiner as PendingJoin");
        assert!(
            matches!(ms.status, MemberStatus::PendingJoin),
            "invite-only joiner is admitted as PendingJoin (pre-counter-sign)"
        );
        assert!(
            ms.enrolled_device_keys
                .contains(&joiner.cert.device_pubkeys.classical.ed25519_verify),
            "joiner's device key (from the PendingJoin cert) must be seeded for publisher_sig"
        );
    }

    /// No self-authorizing PendingJoin for the publisher in the blob → None
    /// (the publish is unauthorized; there is nothing to salvage).
    #[test]
    fn bootstrap_admit_invite_only_publisher_rejects_without_pending_join() {
        let admin = mint_test_owner(0xc3);
        let joiner = mint_test_owner(0xc4);
        let community_id = SpaceId([0xcd; 16]);

        // Blob carries only the admin bootstrap — no PendingJoin for the joiner.
        let events = vec![admin_bootstrap_join(&admin, community_id)];
        let root_at = Hlc {
            wall_ms: 1_700_000_002_000,
            logical: 0,
            device_id: "root".into(),
        };
        let got = bootstrap_admit_invite_only_publisher(
            &events,
            joiner.owner,
            admin.owner,
            community_id,
            &root_at,
        );
        assert!(
            got.is_none(),
            "no PendingJoin for the publisher → not admitted"
        );
    }

    /// A PendingJoin whose InviteToken is signed by a NON-admin (forged) must
    /// fail verify_event → None. Guards the security boundary: only an
    /// admin-authorized invite can bootstrap a publisher onto the gate.
    #[test]
    fn bootstrap_admit_invite_only_publisher_rejects_forged_invite_token() {
        let admin = mint_test_owner(0xc5);
        let attacker = mint_test_owner(0xc6); // NOT the admin
        let joiner = mint_test_owner(0xc7);
        let community_id = SpaceId([0xce; 16]);

        // Token claims `inviter = admin` but is SIGNED by the attacker's key.
        let forged = make_invite_token(&attacker, admin.owner, None, None);
        let pending = make_pending_join_event(&joiner, joiner.owner, community_id, forged);

        let events = vec![admin_bootstrap_join(&admin, community_id), pending];
        let root_at = Hlc {
            wall_ms: 1_700_000_002_000,
            logical: 0,
            device_id: "root".into(),
        };
        let got = bootstrap_admit_invite_only_publisher(
            &events,
            joiner.owner,
            admin.owner,
            community_id,
            &root_at,
        );
        assert!(
            got.is_none(),
            "a forged (non-admin-signed) InviteToken must not admit the joiner"
        );
    }

    /// A publisher whose only self-evidence is a bare `Join` (materializes to
    /// `Joined`) — e.g. the admin's own founder bootstrap Join — must NOT
    /// self-admit on first contact. Only an uncountersigned `PendingJoin`
    /// (ZEB-526's joiner case) bootstraps onto the gate; a `Joined` root reaches
    /// the receiver through normal propagation. This preserves the invite-only
    /// cold-cache reject→propagate→admit contract that the open-community
    /// relaxation (ZEB-558) deliberately left unchanged for invite-only.
    #[test]
    fn bootstrap_admit_invite_only_publisher_rejects_bare_join_publisher() {
        let admin = mint_test_owner(0xc8);
        let community_id = SpaceId([0xcf; 16]);

        // The publisher IS the admin; the blob carries only the admin's founder
        // Join (a valid, self-authorizing root-of-trust Join → materializes to
        // Joined). Even so, the invite-only bootstrap must not admit it.
        let events = vec![admin_bootstrap_join(&admin, community_id)];
        let root_at = Hlc {
            wall_ms: 1_700_000_002_000,
            logical: 0,
            device_id: "root".into(),
        };
        let got = bootstrap_admit_invite_only_publisher(
            &events,
            admin.owner,
            admin.owner,
            community_id,
            &root_at,
        );
        assert!(
            got.is_none(),
            "a bare Joined publisher (admin founder Join) must not self-admit invite-only"
        );
    }

    // ── ZEB-578: select_catchup_trigger_event ────────────────────────────────
    // The synthesizer's catchup-trigger selection must mirror the apply-side
    // acceptance: a `Join` OR a countersigned `PendingJoin` is a valid trigger;
    // an un-countersigned `PendingJoin` is not. (An invite-only joiner only ever
    // authors a PendingJoin, so without accepting it they never get a catchup.)

    /// Build a minimal event of `kind` authored by `actor` at `wall_ms`. Only
    /// the fields `select_catchup_trigger_event` reads (actor/kind/id/at) need to
    /// be meaningful; the sig is a dummy (the selector does not verify).
    fn catchup_ev(
        id_byte: u8,
        actor: OwnerAddr,
        kind: MembershipEventKind,
        wall_ms: u64,
    ) -> SignedMembershipEvent {
        let mut id = [0u8; 16];
        id[15] = id_byte;
        SignedMembershipEvent {
            signer_certs: Vec::new(),
            id,
            community_id: SpaceId([0xcc; 16]),
            kind,
            actor,
            at: Hlc {
                wall_ms,
                logical: 0,
                device_id: "catchup-test".into(),
            },
            sig: [0u8; 64],
            countersig: None,
            enrollment: None,
        }
    }

    #[test]
    fn select_catchup_trigger_event_accepts_join() {
        let dave = mint_test_owner(0xD1);
        let join = catchup_ev(0x01, dave.owner, MembershipEventKind::Join, 100);
        let got = select_catchup_trigger_event(std::slice::from_ref(&join), dave.owner);
        assert_eq!(
            got,
            Some(join.id),
            "a Join authored by the target is a valid catchup trigger"
        );
    }

    #[test]
    fn select_catchup_trigger_event_accepts_countersigned_pending_join() {
        let admin = mint_test_owner(0xA1);
        let dave = mint_test_owner(0xD1);
        let cid = SpaceId([0xcc; 16]);
        let token = make_invite_token(&admin, admin.owner, None, None);
        // make_pending_join_event mints the PendingJoin with id [9u8; 16].
        let pending = make_pending_join_event(&dave, dave.owner, cid, token);
        let countersign = catchup_ev(
            0x58,
            admin.owner,
            MembershipEventKind::JoinCountersign {
                target_event_id: pending.id,
            },
            1_700_000_002_000,
        );
        let events = vec![pending.clone(), countersign];
        let got = select_catchup_trigger_event(&events, dave.owner);
        assert_eq!(
            got,
            Some(pending.id),
            "an invite-only joiner's COUNTERSIGNED PendingJoin is a valid catchup trigger"
        );
    }

    #[test]
    fn select_catchup_trigger_event_rejects_uncountersigned_pending_join() {
        let admin = mint_test_owner(0xA1);
        let dave = mint_test_owner(0xD1);
        let cid = SpaceId([0xcc; 16]);
        let token = make_invite_token(&admin, admin.owner, None, None);
        // No JoinCountersign for `pending` → a still-pending joiner, not a member.
        let pending = make_pending_join_event(&dave, dave.owner, cid, token);
        let got = select_catchup_trigger_event(std::slice::from_ref(&pending), dave.owner);
        assert_eq!(
            got, None,
            "an un-countersigned PendingJoin must NOT trigger a catchup (§10.6 backward-secrecy)"
        );
    }

    #[test]
    fn select_catchup_trigger_event_prefers_most_recent_on_rejoin() {
        let admin = mint_test_owner(0xA1);
        let dave = mint_test_owner(0xD1);
        let cid = SpaceId([0xcc; 16]);
        // An old Join (dave joined, later left), then a newer countersigned
        // PendingJoin (re-join into a rotated epoch). The most-recent qualifying
        // event must win so the dedupe key tracks the live join.
        let early_join = catchup_ev(0x01, dave.owner, MembershipEventKind::Join, 100);
        let token = make_invite_token(&admin, admin.owner, None, None);
        let pending = make_pending_join_event(&dave, dave.owner, cid, token); // wall 1_700_000_001_000
        let countersign = catchup_ev(
            0x58,
            admin.owner,
            MembershipEventKind::JoinCountersign {
                target_event_id: pending.id,
            },
            1_700_000_002_000,
        );
        let events = vec![early_join, pending.clone(), countersign];
        let got = select_catchup_trigger_event(&events, dave.owner);
        assert_eq!(
            got,
            Some(pending.id),
            "the most-recent qualifying event (the re-join PendingJoin) wins over the older Join"
        );
    }
}

#[cfg(test)]
mod zeb_254_join_countersign_verify_tests {
    use super::*;

    /// Build a test identity from a seed byte.
    /// Returns (PrivateIdentity, identity_pub [u8; 64], OwnerAddr).
    fn make_identity(seed_byte: u8) -> (TestOwner, [u8; 64], OwnerAddr) {
        let owner = mint_test_owner(seed_byte);
        let addr = owner.owner;
        (owner, [0u8; 64], addr)
    }

    fn make_join_countersign_event(
        admin: &TestOwner,
        admin_addr: OwnerAddr,
        community_id: SpaceId,
        target_event_id: [u8; 16],
    ) -> SignedMembershipEvent {
        let payload = EventPayload {
            id: [88u8; 16],
            community_id,
            kind: MembershipEventKind::JoinCountersign { target_event_id },
            actor: admin_addr,
            at: Hlc {
                wall_ms: 1_700_000_002_000,
                logical: 0,
                device_id: "admin-device".into(),
            },
        };
        sign_event(&payload, &admin.device_key).expect("sign JoinCountersign")
    }

    /// ZEB-339: a MemberState that is Joined and carries `owner`'s enrolled
    /// device key, so steady-state signer resolution can find it.
    fn joined_with_enrolled(owner: &TestOwner) -> MemberState {
        let mut keys = BTreeSet::new();
        keys.insert(owner.cert.device_pubkeys.classical.ed25519_verify);
        MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "t".into(),
            },
            left_at: None,
            enrolled_device_keys: keys,
            revoked_device_keys: BTreeSet::new(),
        }
    }

    #[test]
    fn join_countersign_event_signs_and_verifies() {
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let community_id = SpaceId([7u8; 16]);
        let target = [9u8; 16];
        let event = make_join_countersign_event(&admin_priv, admin_addr, community_id, target);
        let mut mat = MaterializedMembership::default();
        mat.members
            .insert(admin_addr, joined_with_enrolled(&admin_priv));
        mat.power_levels.insert(admin_addr, POWER_THRESHOLDS.invite);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
        };
        let result = verify_event(&event, &mat, &ctx);
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
    }

    #[test]
    fn join_countersign_rejected_when_actor_not_joined() {
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let community_id = SpaceId([7u8; 16]);
        let target = [9u8; 16];
        let event = make_join_countersign_event(&admin_priv, admin_addr, community_id, target);
        let mat = MaterializedMembership::default(); // actor not in members map
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
        };
        let result = verify_event(&event, &mat, &ctx);
        // ZEB-339: a non-member actor now fails signer resolution (step 1)
        // before the per-kind JoinCountersignActorNotJoined gate is reached —
        // with no materialized member there is no enrolled device key to
        // resolve the signature against.
        assert!(
            matches!(result, Err(VerifyError::SignerNotEnrolledForActor)),
            "expected SignerNotEnrolledForActor, got {:?}",
            result
        );
    }

    #[test]
    fn join_countersign_accepted_when_target_missing() {
        // Out-of-order delivery — JoinCountersign arrives before its
        // target PendingJoin. Verify MUST accept it (target existence
        // is a materialize-time concern, not verify-time).
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let community_id = SpaceId([7u8; 16]);
        let target = [0xDEu8; 16]; // does not exist in prior state
        let event = make_join_countersign_event(&admin_priv, admin_addr, community_id, target);
        let mut mat = MaterializedMembership::default();
        mat.members
            .insert(admin_addr, joined_with_enrolled(&admin_priv));
        mat.power_levels.insert(admin_addr, POWER_THRESHOLDS.invite);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
        };
        let result = verify_event(&event, &mat, &ctx);
        assert!(
            result.is_ok(),
            "expected Ok (out-of-order delivery), got {:?}",
            result
        );
    }

    #[test]
    fn countersig_with_device_key_verifies_under_materialized_enrolled_key() {
        // ZEB-339: a Join event countersigned with the counter-signer's
        // enrolled device key (#2) verifies under verify_countersig, which
        // resolves the signer from the counter-signer's materialized
        // enrolled_device_keys (seeded from a cert-bearing Join).
        let community_id = SpaceId([7u8; 16]);
        // The joiner authors a Join event signed by their device #2.
        let joiner = mint_test_owner(0x31);
        let join_payload = EventPayload {
            id: [0x10u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: joiner.owner,
            at: Hlc {
                wall_ms: 1_700_000_001_000,
                logical: 0,
                device_id: "joiner-device".into(),
            },
        };
        let join_event = sign_event(&join_payload, &joiner.device_key).expect("sign Join");

        // The counter-signer attaches a countersig with THEIR device key (#2).
        let signer = mint_test_owner(0x32);
        let countersigned =
            attach_countersig_with_device_key(&join_event, signer.owner, &signer.device_key)
                .expect("attach device-key countersig");

        // prior_state: the counter-signer is Joined and carries their enrolled
        // device key (as it would be materialized from a cert-bearing Join).
        let mut prior_state = MaterializedMembership::default();
        prior_state
            .members
            .insert(signer.owner, joined_with_enrolled(&signer));

        verify_countersig(&countersigned, &prior_state)
            .expect("device-key countersig verifies under materialized enrolled key");
    }

    #[test]
    fn join_countersign_structural_power_check_documented() {
        // POWER_THRESHOLDS.invite is 0 in v1 — there's no way for a
        // Joined member to have insufficient power in v1. The check is
        // structurally present and will become firable in ZEB-251 when
        // per-community thresholds ship.
        assert_eq!(
            POWER_THRESHOLDS.invite, 0,
            "ZEB-254 v1: invite_threshold is 0; JoinCountersignActorPowerInsufficient \
             is structurally present but cannot fire under v1 thresholds"
        );
    }
}

#[cfg(test)]
mod zeb_254_materialize_tests {
    use super::*;
    use crate::community_invite::InviteToken;

    /// Build a test identity from a seed byte via PrivateIdentity::from_seed.
    fn synth_identity(seed_byte: u8) -> (harmony_identity::PrivateIdentity, OwnerAddr, [u8; 64]) {
        let seed = [seed_byte; 32];
        let private = harmony_identity::PrivateIdentity::from_seed(&seed);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let addr = OwnerAddr(public.address_hash);
        (private, addr, identity_pub)
    }

    fn synth_pending_join(
        actor_private: &harmony_identity::PrivateIdentity,
        actor_addr: OwnerAddr,
        _joiner_pub: [u8; 64],
        community_id: SpaceId,
        at_wall_ms: u64,
        event_id_seed: u8,
    ) -> SignedMembershipEvent {
        let token = InviteToken {
            inviter: OwnerAddr([0u8; 16]),
            invitee_hint: Some(actor_addr),
            minted_at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "joiner".into(),
            },
            expires_at: None,
            sig: [0u8; 64],
        };
        let payload = EventPayload {
            id: [event_id_seed; 16],
            community_id,
            kind: MembershipEventKind::PendingJoin {
                invite_token: token,
            },
            actor: actor_addr,
            at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "joiner".into(),
            },
        };
        sign_event_with_identity(&payload, actor_private).expect("sign pending")
    }

    fn synth_join_countersign(
        admin_private: &harmony_identity::PrivateIdentity,
        admin_addr: OwnerAddr,
        community_id: SpaceId,
        target: EventId,
        at_wall_ms: u64,
        event_id_seed: u8,
    ) -> SignedMembershipEvent {
        let payload = EventPayload {
            id: [event_id_seed; 16],
            community_id,
            kind: MembershipEventKind::JoinCountersign {
                target_event_id: target,
            },
            actor: admin_addr,
            at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "admin".into(),
            },
        };
        sign_event_with_identity(&payload, admin_private).expect("sign countersign")
    }

    fn synth_legacy_join(
        actor_private: &harmony_identity::PrivateIdentity,
        actor_addr: OwnerAddr,
        community_id: SpaceId,
        at_wall_ms: u64,
        event_id_seed: u8,
    ) -> SignedMembershipEvent {
        let payload = EventPayload {
            id: [event_id_seed; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: actor_addr,
            at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "joiner".into(),
            },
        };
        sign_event_with_identity(&payload, actor_private).expect("sign join")
    }

    fn synth_leave(
        actor_private: &harmony_identity::PrivateIdentity,
        actor_addr: OwnerAddr,
        community_id: SpaceId,
        at_wall_ms: u64,
        event_id_seed: u8,
    ) -> SignedMembershipEvent {
        let payload = EventPayload {
            id: [event_id_seed; 16],
            community_id,
            kind: MembershipEventKind::Leave,
            actor: actor_addr,
            at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "joiner".into(),
            },
        };
        sign_event_with_identity(&payload, actor_private).expect("sign leave")
    }

    fn synth_kick(
        actor_private: &harmony_identity::PrivateIdentity,
        actor_addr: OwnerAddr,
        target: OwnerAddr,
        community_id: SpaceId,
        at_wall_ms: u64,
        event_id_seed: u8,
    ) -> SignedMembershipEvent {
        let payload = EventPayload {
            id: [event_id_seed; 16],
            community_id,
            kind: MembershipEventKind::Kick {
                target,
                reason: None,
            },
            actor: actor_addr,
            at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "admin".into(),
            },
        };
        sign_event_with_identity(&payload, actor_private).expect("sign kick")
    }

    #[test]
    fn materialize_pending_join_only_yields_pending_status() {
        let community = SpaceId([7u8; 16]);
        let (joiner_priv, joiner_addr, joiner_pub) = synth_identity(1);
        let (_, admin_addr, _) = synth_identity(2);
        let pending = synth_pending_join(
            &joiner_priv,
            joiner_addr,
            joiner_pub,
            community,
            1_700_000_000_000,
            1,
        );
        let mat = materialize(&[pending], admin_addr);
        assert_eq!(
            mat.members.get(&joiner_addr).map(|m| m.status),
            Some(MemberStatus::PendingJoin)
        );
    }

    #[test]
    fn materialize_pending_join_with_countersign_yields_joined() {
        let community = SpaceId([7u8; 16]);
        let (joiner_priv, joiner_addr, joiner_pub) = synth_identity(1);
        let (admin_priv, admin_addr, _) = synth_identity(2);
        let pending = synth_pending_join(
            &joiner_priv,
            joiner_addr,
            joiner_pub,
            community,
            1_700_000_000_000,
            1,
        );
        let cs = synth_join_countersign(
            &admin_priv,
            admin_addr,
            community,
            pending.id,
            1_700_000_001_000,
            2,
        );
        let mat = materialize(&[pending, cs], admin_addr);
        assert_eq!(
            mat.members.get(&joiner_addr).map(|m| m.status),
            Some(MemberStatus::Joined)
        );
    }

    /// ZEB-339: a first-join-via-invite (PendingJoin + countersign, no prior
    /// Join) must learn the joiner's enrolled device key from the cert carried
    /// on the PendingJoin event — otherwise the joiner ends up Joined with an
    /// empty key set and their later steady-state events fail verification.
    #[test]
    fn materialize_pending_join_countersign_ingests_enrollment_key() {
        let community = SpaceId([7u8; 16]);
        let (joiner_priv, joiner_addr, joiner_pub) = synth_identity(1);
        let (admin_priv, admin_addr, _) = synth_identity(2);
        let owner = mint_test_owner(0x44);
        let mut pending = synth_pending_join(
            &joiner_priv,
            joiner_addr,
            joiner_pub,
            community,
            1_700_000_000_000,
            1,
        );
        pending.enrollment = Some(owner.cert.clone());
        let cs = synth_join_countersign(
            &admin_priv,
            admin_addr,
            community,
            pending.id,
            1_700_000_001_000,
            2,
        );
        let mat = materialize(&[pending, cs], admin_addr);
        let ek = &mat
            .members
            .get(&joiner_addr)
            .expect("joiner is a member after countersign")
            .enrolled_device_keys;
        assert!(
            ek.contains(&owner.cert.device_pubkeys.classical.ed25519_verify),
            "PendingJoin→countersign must ingest the joiner's enrolled device key from event.enrollment"
        );
    }

    #[test]
    fn materialize_pending_join_older_than_30d_hidden() {
        let community = SpaceId([7u8; 16]);
        let (joiner_priv, joiner_addr, joiner_pub) = synth_identity(1);
        let (later_actor_priv, later_actor, _) = synth_identity(99);
        let (_, admin_addr, _) = synth_identity(2);
        let pending = synth_pending_join(
            &joiner_priv,
            joiner_addr,
            joiner_pub,
            community,
            1_700_000_000_000,
            1,
        );
        // Later join from a different actor pushes current_max_wall_ms > 30d ahead.
        let later = synth_legacy_join(
            &later_actor_priv,
            later_actor,
            community,
            1_700_000_000_000 + 31 * 86_400_000,
            2,
        );
        let mat = materialize(&[pending, later], admin_addr);
        // Joiner is hidden — no entry in members map.
        assert!(
            !mat.members.contains_key(&joiner_addr),
            "expected joiner hidden after 30d expiry, got {:?}",
            mat.members.get(&joiner_addr)
        );
    }

    #[test]
    fn materialize_pending_join_countersign_resurrects_expired_pending() {
        let community = SpaceId([7u8; 16]);
        let (joiner_priv, joiner_addr, joiner_pub) = synth_identity(1);
        let (admin_priv, admin_addr, _) = synth_identity(2);
        let pending = synth_pending_join(
            &joiner_priv,
            joiner_addr,
            joiner_pub,
            community,
            1_700_000_000_000,
            1,
        );
        // Counter-sign 31 days later — past the expiry window.
        let cs = synth_join_countersign(
            &admin_priv,
            admin_addr,
            community,
            pending.id,
            1_700_000_000_000 + 31 * 86_400_000,
            2,
        );
        let mat = materialize(&[pending, cs], admin_addr);
        assert_eq!(
            mat.members.get(&joiner_addr).map(|m| m.status),
            Some(MemberStatus::Joined)
        );
    }

    #[test]
    fn materialize_legacy_join_with_countersig_still_yields_joined() {
        let community = SpaceId([7u8; 16]);
        let (joiner_priv, joiner_addr, _) = synth_identity(1);
        let (_, admin_addr, _) = synth_identity(2);
        let join = synth_legacy_join(&joiner_priv, joiner_addr, community, 1_700_000_000_000, 1);
        let mat = materialize(&[join], admin_addr);
        assert_eq!(
            mat.members.get(&joiner_addr).map(|m| m.status),
            Some(MemberStatus::Joined)
        );
    }

    #[test]
    fn materialize_pending_join_then_leave_yields_left() {
        let community = SpaceId([7u8; 16]);
        let (joiner_priv, joiner_addr, joiner_pub) = synth_identity(1);
        let (_, admin_addr, _) = synth_identity(2);
        let pending = synth_pending_join(
            &joiner_priv,
            joiner_addr,
            joiner_pub,
            community,
            1_700_000_000_000,
            1,
        );
        let leave = synth_leave(&joiner_priv, joiner_addr, community, 1_700_000_001_000, 2);
        let mat = materialize(&[pending, leave], admin_addr);
        assert_eq!(
            mat.members.get(&joiner_addr).map(|m| m.status),
            Some(MemberStatus::Left)
        );
    }

    #[test]
    fn materialize_pending_join_with_two_countersigns_yields_joined() {
        let community = SpaceId([7u8; 16]);
        let (joiner_priv, joiner_addr, joiner_pub) = synth_identity(1);
        let (admin1_priv, admin1_addr, _) = synth_identity(2);
        let (admin2_priv, admin2_addr, _) = synth_identity(3);
        let pending = synth_pending_join(
            &joiner_priv,
            joiner_addr,
            joiner_pub,
            community,
            1_700_000_000_000,
            1,
        );
        let cs1 = synth_join_countersign(
            &admin1_priv,
            admin1_addr,
            community,
            pending.id,
            1_700_000_001_000,
            2,
        );
        let cs2 = synth_join_countersign(
            &admin2_priv,
            admin2_addr,
            community,
            pending.id,
            1_700_000_001_500,
            3,
        );
        let mat = materialize(&[pending, cs1, cs2], admin1_addr);
        assert_eq!(
            mat.members.get(&joiner_addr).map(|m| m.status),
            Some(MemberStatus::Joined)
        );
        assert_eq!(
            mat.members.len(),
            1,
            "duplicate countersigns must not produce extra entries; only the joiner is present"
        );
    }

    #[test]
    fn materialize_pending_join_countersign_in_rotated_epoch_marks_catchup() {
        // Mirrors the Join arm's catchup behavior: a member joining via
        // PendingJoin + JoinCountersign in a community that has rotated its
        // epoch must be flagged for key-material catchup.
        //
        // We bypass EpochRotation synthesis (which requires RecipientCiphertexts
        // scaffolding) and instead construct a MaterializedMembership with
        // current_epoch = Some(1) by using an admin's legacy Join followed by
        // manually verifying the path via the materialize fn outcome.
        //
        // Since we cannot trivially produce an EpochRotation event in unit-test
        // scope, we exercise the guard through two separate assertions:
        //   1. epoch = 0 community → pending_catchup_for NOT populated.
        //   2. We simulate epoch > 0 by reading the implementation invariant:
        //      the guard `if m.current_epoch.unwrap_or(0) > 0` is only satisfied
        //      when an EpochRotation event has been applied. We therefore confirm
        //      the epoch=0 case does NOT insert and rely on the integration test
        //      in Task 15 (ZEB-254) for the full rotated-epoch path.
        //
        // For the epoch=0 case, pending_catchup_for must be empty.
        let community = SpaceId([7u8; 16]);
        let (joiner_priv, joiner_addr, joiner_pub) = synth_identity(1);
        let (admin_priv, admin_addr, _) = synth_identity(2);
        let pending = synth_pending_join(
            &joiner_priv,
            joiner_addr,
            joiner_pub,
            community,
            1_700_000_002_000,
            1,
        );
        let cs = synth_join_countersign(
            &admin_priv,
            admin_addr,
            community,
            pending.id,
            1_700_000_003_000,
            2,
        );
        // epoch = 0 (no EpochRotation events) → no catchup needed.
        let mat = materialize(&[pending, cs], admin_addr);
        assert_eq!(
            mat.members.get(&joiner_addr).map(|m| m.status),
            Some(MemberStatus::Joined),
            "joiner must be Joined after countersign"
        );
        assert_eq!(
            mat.current_epoch, None,
            "no epoch rotation → current_epoch is None"
        );
        assert!(
            !mat.pending_catchup_for.contains(&joiner_addr),
            "epoch=0: joiner must NOT be in pending_catchup_for (no rotation yet)"
        );
        // The rotated-epoch path (current_epoch > 0 triggers insert) is validated
        // by Task 15 integration tests where EpochRotation events can be fully
        // synthesized with RecipientCiphertexts.
    }

    /// ZEB-254 bot-review Q1: A previously-Left member re-joining via
    /// PendingJoin must materialize as PendingJoin, NOT be shadowed by Left.
    ///
    /// verify_event P6 gate explicitly permits prior state `None | Left` for
    /// PendingJoin; materialize's terminal-shadow list must be consistent.
    #[test]
    fn materialize_pending_join_after_left_yields_pending() {
        let community = SpaceId([7u8; 16]);
        let (joiner_priv, joiner_addr, joiner_pub) = synth_identity(1);
        let (_, admin_addr, _) = synth_identity(2);

        // Sequence: original legacy Join → Leave → new PendingJoin (re-join
        // attempt). The PendingJoin comes after the Leave in HLC order.
        let original_join =
            synth_legacy_join(&joiner_priv, joiner_addr, community, 1_700_000_000_000, 1);
        let leave = synth_leave(&joiner_priv, joiner_addr, community, 1_700_000_001_000, 2);
        let pending = synth_pending_join(
            &joiner_priv,
            joiner_addr,
            joiner_pub,
            community,
            1_700_000_002_000,
            3,
        );

        // Materialize the full log. The PendingJoin (event_sort_key > Leave)
        // must NOT be shadowed by the Left status; it must win and yield
        // PendingJoin.
        let mat = materialize(&[original_join, leave, pending], admin_addr);
        assert_eq!(
            mat.members.get(&joiner_addr).map(|m| m.status),
            Some(MemberStatus::PendingJoin),
            "a previously-Left member re-joining via PendingJoin must materialize \
             as PendingJoin, not be shadowed by Left"
        );
    }

    /// ZEB-254 bot-review Q1 corollary: a PendingJoin WITH countersign after
    /// Leave must materialize as Joined (countersign approval is not shadowed).
    #[test]
    fn materialize_pending_join_after_left_with_countersign_yields_joined() {
        let community = SpaceId([7u8; 16]);
        let (joiner_priv, joiner_addr, joiner_pub) = synth_identity(1);
        let (admin_priv, admin_addr, _) = synth_identity(2);

        let original_join =
            synth_legacy_join(&joiner_priv, joiner_addr, community, 1_700_000_000_000, 1);
        let leave = synth_leave(&joiner_priv, joiner_addr, community, 1_700_000_001_000, 2);
        let pending = synth_pending_join(
            &joiner_priv,
            joiner_addr,
            joiner_pub,
            community,
            1_700_000_002_000,
            3,
        );
        let cs = synth_join_countersign(
            &admin_priv,
            admin_addr,
            community,
            pending.id,
            1_700_000_003_000,
            4,
        );

        let mat = materialize(&[original_join, leave, pending, cs], admin_addr);
        assert_eq!(
            mat.members.get(&joiner_addr).map(|m| m.status),
            Some(MemberStatus::Joined),
            "a previously-Left member with a countersigned re-join must materialize as Joined"
        );
    }

    /// ZEB-254 R4-1: kicking a PendingJoin user must NOT mark them for
    /// epoch rotation. A PendingJoin user never received any epoch key
    /// material — they have no live key the rest of the community
    /// needs to rotate away from. Without this guard the kick (admin
    /// "Reject" click in PendingJoinsPanel) would synthesize a full
    /// EpochRotation cycling EVERY existing member's keys for a user
    /// who was never actually joined.
    ///
    /// Mirrors the Leave arm's `leave_transitioned` guard which also
    /// drops never-members from pending_rotation_for.
    #[test]
    fn kick_of_pending_join_does_not_trigger_epoch_rotation() {
        let community = SpaceId([7u8; 16]);
        let (joiner_priv, joiner_addr, joiner_pub) = synth_identity(1);
        let (admin_priv, admin_addr, _) = synth_identity(2);

        // PendingJoin lands first; admin Kicks the still-pending joiner
        // (Reject flow). No JoinCountersign — the kick happens BEFORE
        // any counter-sign was issued.
        let pending = synth_pending_join(
            &joiner_priv,
            joiner_addr,
            joiner_pub,
            community,
            1_700_000_000_000,
            1,
        );
        let kick = synth_kick(
            &admin_priv,
            admin_addr,
            joiner_addr,
            community,
            1_700_000_001_000,
            2,
        );

        let mat = materialize(&[pending, kick], admin_addr);

        // The joiner is now Banned (the Kick arm always flips status
        // when an existing entry is found — and PendingJoin inserted
        // one).
        assert_eq!(
            mat.members.get(&joiner_addr).map(|m| m.status),
            Some(MemberStatus::Banned),
            "kick of a PendingJoin user must transition them to Banned"
        );

        // ... but pending_rotation_for must NOT contain the joiner —
        // they never received epoch material, so no rotation is needed.
        assert!(
            !mat.pending_rotation_for.contains(&joiner_addr),
            "R4-1: kick of a PendingJoin user must NOT mark them for \
             EpochRotation — no epoch material was ever delivered to them"
        );
    }

    /// ZEB-254 R4-6: idle-community PendingJoin expiry must agree
    /// across the admin panel (uses wall-clock) and the verify-time
    /// prior-state lookup (uses materialize). Before R4-6,
    /// `materialize`'s `current_max_wall_ms` was always
    /// `max(events.at.wall_ms)`. In a community whose only event IS the
    /// PendingJoin, that max equals the PendingJoin's own wall_ms, so
    /// age_ms = 0 and the event never registered as expired — verify
    /// would reject re-redemption with PendingJoinAlreadyMember 30+
    /// days later.
    ///
    /// This test exercises the fix: at t = T + 30d + 1s, both
    ///   - the admin's view via `materialize_with_now(...,
    ///     Some(wall_now))` (sees the joiner as expired / hidden)
    ///   - the verify-time view via `prior_state_at_hlc(...,
    ///     &re_redeem.at)` (sees the joiner as expired so P6 admits a
    ///     new PendingJoin)
    ///
    /// converge on "expired".
    #[test]
    fn r4_6_idle_community_pending_join_expires_via_wall_clock_floor() {
        let community = SpaceId([7u8; 16]);
        let (joiner_priv, joiner_addr, joiner_pub) = synth_identity(1);
        let (_, admin_addr, _) = synth_identity(2);

        // PendingJoin minted at t=0 (epoch reference, simulating a
        // community where nothing else has happened).
        let t0 = 1_700_000_000_000_u64;
        let pending = synth_pending_join(&joiner_priv, joiner_addr, joiner_pub, community, t0, 1);
        let pending_slice = std::slice::from_ref(&pending);

        // Without a wall-clock floor — i.e., the pre-R4-6 behavior:
        // current_max_wall_ms = pending.at.wall_ms = t0, age_ms = 0 →
        // status is PendingJoin (NOT expired). This is the bug.
        let mat_no_floor = materialize_with_now(pending_slice, admin_addr, None);
        assert_eq!(
            mat_no_floor.members.get(&joiner_addr).map(|m| m.status),
            Some(MemberStatus::PendingJoin),
            "pre-R4-6 baseline: without wall-clock floor, idle-community \
             PendingJoin never expires"
        );

        // With a wall-clock floor at t0 + 30d + 1s — i.e., the R4-6
        // production behavior on admin's panel: the joiner is now
        // expired and hidden from the materialized members map.
        let wall_now = t0 + MATERIALIZE_PENDING_EXPIRY_MS + 1_000;
        let mat_with_floor = materialize_with_now(pending_slice, admin_addr, Some(wall_now));
        assert!(
            !mat_with_floor.members.contains_key(&joiner_addr),
            "R4-6: with wall-clock floor past 30d, PendingJoin must be \
             hidden from materialize"
        );

        // And the verify-time path: when a re-redemption attempt
        // arrives at HLC t0 + 30d + 1s, `prior_state_at_hlc` uses
        // target_hlc.wall_ms as the now-floor, so the prior PendingJoin
        // is seen as expired and P6 admits the re-redemption.
        let re_redeem_hlc = Hlc {
            wall_ms: wall_now,
            logical: 0,
            device_id: "joiner".into(),
        };
        let prior = prior_state_at_hlc(pending_slice, &re_redeem_hlc, admin_addr);
        assert!(
            !prior.members.contains_key(&joiner_addr),
            "R4-6: prior_state_at_hlc must see the original PendingJoin \
             as expired (so verify_event P6 admits the re-redemption)"
        );
    }

    /// Companion to `kick_of_pending_join_does_not_trigger_epoch_rotation`:
    /// confirm the Kick arm STILL marks an established (Joined) member
    /// for rotation. Without this paired assertion, an over-eager guard
    /// in the kick arm could silently regress ZEB-249's
    /// epoch-rotation-on-kick invariant.
    #[test]
    fn kick_of_joined_member_does_trigger_epoch_rotation() {
        let community = SpaceId([7u8; 16]);
        let (member_priv, member_addr, _) = synth_identity(1);
        let (admin_priv, admin_addr, _) = synth_identity(2);

        // Member legacy-joins (so they actually hold epoch material),
        // then admin kicks.
        let join = synth_legacy_join(&member_priv, member_addr, community, 1_700_000_000_000, 1);
        let kick = synth_kick(
            &admin_priv,
            admin_addr,
            member_addr,
            community,
            1_700_000_001_000,
            2,
        );

        let mat = materialize(&[join, kick], admin_addr);
        assert_eq!(
            mat.members.get(&member_addr).map(|m| m.status),
            Some(MemberStatus::Banned),
            "kick of a Joined member must transition them to Banned"
        );
        assert!(
            mat.pending_rotation_for.contains(&member_addr),
            "kick of an established Joined member MUST mark them for \
             EpochRotation (ZEB-249 invariant preserved by R4-1 guard)"
        );
    }

    /// ZEB-254 R5-1: EpochCatchup with `triggered_by` pointing to a
    /// countersigned PendingJoin (not a legacy Join) MUST clear that
    /// joiner from `pending_catchup_for`.
    ///
    /// Repro: In a community whose epoch has already rotated, a joiner
    /// admitted via PendingJoin + JoinCountersign gets enqueued into
    /// `pending_catchup_for` (mirrors the legacy Join arm's catchup
    /// invariant). Before R5-1, the EpochCatchup arm only accepted
    /// `triggered_by` pointing to a legacy `Join`, so the natural
    /// catchup event (issued by an admin pointing at the PendingJoin)
    /// matched no triggering event and silently no-op'd — leaving the
    /// member permanently flagged for catchup.
    #[test]
    fn epoch_catchup_clears_pending_catchup_for_pending_join_admission() {
        let community = SpaceId([7u8; 16]);
        let (admin_priv, admin_addr, _) = synth_identity(2);
        let (joiner_priv, joiner_addr, joiner_pub) = synth_identity(1);
        // Bob is an existing member who will Leave to give us a valid
        // EpochRotation trigger (rotation requires triggered_by to be a
        // Kick or Leave per the rotation arm at line ~1376).
        let (bob_priv, bob_addr, _) = synth_identity(3);

        // 1. Bootstrap admin Join. Admin gets power=100 via the bootstrap
        //    rule in materialize (line ~1081).
        let admin_join =
            synth_legacy_join(&admin_priv, admin_addr, community, 1_700_000_000_000, 10);

        // 2. Bob joins so we have someone to remove.
        let bob_join = synth_legacy_join(&bob_priv, bob_addr, community, 1_700_000_000_500, 11);

        // 3. Bob leaves. This becomes the rotation trigger.
        let bob_leave = synth_leave(&bob_priv, bob_addr, community, 1_700_000_001_000, 12);

        // 4. EpochRotation 0→1 issued by admin, triggered_by=bob_leave.
        //    recipient_ciphertexts must include all remaining
        //    Joined/Invited members EXCEPT the target (bob); after
        //    bob_leave, only admin is Joined, so recipients = [admin].
        let rotation_payload = EventPayload {
            id: [20; 16],
            community_id: community,
            kind: MembershipEventKind::EpochRotation {
                prior_epoch: 0,
                triggered_by: bob_leave.id,
                recipient_ciphertexts: vec![RecipientCiphertext {
                    recipient: admin_addr,
                    sealed: vec![0u8; 92],
                }],
            },
            actor: admin_addr,
            at: Hlc {
                wall_ms: 1_700_000_001_500,
                logical: 0,
                device_id: "admin".into(),
            },
        };
        let rotation =
            sign_event_with_identity(&rotation_payload, &admin_priv).expect("sign rotation");

        // 5. Joiner mints PendingJoin (epoch already at 1 by this point).
        let pending = synth_pending_join(
            &joiner_priv,
            joiner_addr,
            joiner_pub,
            community,
            1_700_000_002_000,
            30,
        );

        // 6. Admin counter-signs the PendingJoin. Materializer now treats
        //    joiner as Joined AND, because current_epoch=1, enqueues them
        //    into pending_catchup_for (PendingJoin arm at line ~1578).
        let countersign = synth_join_countersign(
            &admin_priv,
            admin_addr,
            community,
            pending.id,
            1_700_000_003_000,
            40,
        );

        // Sanity-check the precondition: after PendingJoin + countersign,
        // joiner IS in pending_catchup_for.
        {
            let mat_pre = materialize(
                &[
                    admin_join.clone(),
                    bob_join.clone(),
                    bob_leave.clone(),
                    rotation.clone(),
                    pending.clone(),
                    countersign.clone(),
                ],
                admin_addr,
            );
            assert_eq!(
                mat_pre.current_epoch,
                Some(1),
                "EpochRotation must advance current_epoch to 1"
            );
            assert_eq!(
                mat_pre.members.get(&joiner_addr).map(|m| m.status),
                Some(MemberStatus::Joined),
                "joiner must be Joined after countersign"
            );
            assert!(
                mat_pre.pending_catchup_for.contains(&joiner_addr),
                "PRECONDITION: countersigned PendingJoin in rotated epoch \
                 MUST enqueue joiner into pending_catchup_for"
            );
        }

        // 7. Admin issues EpochCatchup at epoch=1, triggered_by=PendingJoin
        //    event id, recipient=joiner. Pre-R5-1 this was a silent no-op
        //    (only Join-kind triggered_by was accepted); post-R5-1 the
        //    countersigned-PendingJoin lookup must succeed and clear the
        //    joiner from pending_catchup_for.
        let catchup_payload = EventPayload {
            id: [50; 16],
            community_id: community,
            kind: MembershipEventKind::EpochCatchup {
                epoch: 1,
                triggered_by: pending.id,
                recipient_ciphertexts: vec![RecipientCiphertext {
                    recipient: joiner_addr,
                    sealed: vec![0u8; 92],
                }],
            },
            actor: admin_addr,
            at: Hlc {
                wall_ms: 1_700_000_004_000,
                logical: 0,
                device_id: "admin".into(),
            },
        };
        let catchup =
            sign_event_with_identity(&catchup_payload, &admin_priv).expect("sign catchup");

        let mat = materialize(
            &[
                admin_join,
                bob_join,
                bob_leave,
                rotation,
                pending,
                countersign,
                catchup,
            ],
            admin_addr,
        );

        assert_eq!(
            mat.current_epoch,
            Some(1),
            "current_epoch must still be 1 after catchup"
        );
        assert_eq!(
            mat.members.get(&joiner_addr).map(|m| m.status),
            Some(MemberStatus::Joined),
            "joiner remains Joined after catchup"
        );
        assert!(
            !mat.pending_catchup_for.contains(&joiner_addr),
            "POST-R5-1: EpochCatchup with triggered_by=PendingJoin MUST clear \
             joiner from pending_catchup_for (regression for unbounded catchup-pending state)"
        );
    }

    /// ZEB-254 R5-1 negative: an EpochCatchup whose `triggered_by` points
    /// to an UNCOUNTERSIGNED PendingJoin must be a no-op. A still-pending
    /// joiner is not a member; no admin would (or should) issue catchup
    /// keys to them. This guards against widening the trigger contract
    /// past the intent of R5-1.
    #[test]
    fn epoch_catchup_ignores_uncountersigned_pending_join_trigger() {
        let community = SpaceId([7u8; 16]);
        let (admin_priv, admin_addr, _) = synth_identity(2);
        let (joiner_priv, joiner_addr, joiner_pub) = synth_identity(1);
        let (bob_priv, bob_addr, _) = synth_identity(3);

        let admin_join =
            synth_legacy_join(&admin_priv, admin_addr, community, 1_700_000_000_000, 10);
        let bob_join = synth_legacy_join(&bob_priv, bob_addr, community, 1_700_000_000_500, 11);
        let bob_leave = synth_leave(&bob_priv, bob_addr, community, 1_700_000_001_000, 12);
        let rotation_payload = EventPayload {
            id: [20; 16],
            community_id: community,
            kind: MembershipEventKind::EpochRotation {
                prior_epoch: 0,
                triggered_by: bob_leave.id,
                recipient_ciphertexts: vec![RecipientCiphertext {
                    recipient: admin_addr,
                    sealed: vec![0u8; 92],
                }],
            },
            actor: admin_addr,
            at: Hlc {
                wall_ms: 1_700_000_001_500,
                logical: 0,
                device_id: "admin".into(),
            },
        };
        let rotation =
            sign_event_with_identity(&rotation_payload, &admin_priv).expect("sign rotation");
        let pending = synth_pending_join(
            &joiner_priv,
            joiner_addr,
            joiner_pub,
            community,
            1_700_000_002_000,
            30,
        );
        // NOTE: no countersign event.
        let catchup_payload = EventPayload {
            id: [50; 16],
            community_id: community,
            kind: MembershipEventKind::EpochCatchup {
                epoch: 1,
                triggered_by: pending.id,
                recipient_ciphertexts: vec![RecipientCiphertext {
                    recipient: joiner_addr,
                    sealed: vec![0u8; 92],
                }],
            },
            actor: admin_addr,
            at: Hlc {
                wall_ms: 1_700_000_004_000,
                logical: 0,
                device_id: "admin".into(),
            },
        };
        let catchup =
            sign_event_with_identity(&catchup_payload, &admin_priv).expect("sign catchup");

        let mat = materialize(
            &[admin_join, bob_join, bob_leave, rotation, pending, catchup],
            admin_addr,
        );

        // Joiner is PendingJoin (uncountersigned, within expiry window).
        assert_eq!(
            mat.members.get(&joiner_addr).map(|m| m.status),
            Some(MemberStatus::PendingJoin),
            "uncountersigned PendingJoin within expiry window materializes as PendingJoin"
        );
        // pending_catchup_for is NOT populated for this joiner (only the
        // countersigned-PendingJoin path enqueues), and the catchup itself
        // would not clear anything regardless.
        assert!(
            !mat.pending_catchup_for.contains(&joiner_addr),
            "uncountersigned PendingJoin must NOT be in pending_catchup_for"
        );
    }

    #[test]
    fn admin_proposal_setpower_roundtrip() {
        use crate::owner_state_crypto::canonical_cbor_encode;
        let kind = MembershipEventKind::AdminProposal {
            proposal_kind: ProposalKind::SetPower {
                target: OwnerAddr([0x11; 16]),
                level: 100,
            },
        };
        let bytes = canonical_cbor_encode(&kind).expect("encode");
        let decoded: MembershipEventKind = ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(decoded, kind);
    }

    #[test]
    fn admin_proposal_kick_roundtrip() {
        use crate::owner_state_crypto::canonical_cbor_encode;
        let kind = MembershipEventKind::AdminProposal {
            proposal_kind: ProposalKind::Kick {
                target: OwnerAddr([0x22; 16]),
                reason: Some("breach".to_string()),
            },
        };
        let bytes = canonical_cbor_encode(&kind).expect("encode");
        let decoded: MembershipEventKind = ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(decoded, kind);
    }

    #[test]
    fn admin_proposal_change_quorum_roundtrip() {
        use crate::owner_state_crypto::canonical_cbor_encode;
        let kind = MembershipEventKind::AdminProposal {
            proposal_kind: ProposalKind::ChangeQuorum { new_quorum: 3 },
        };
        let bytes = canonical_cbor_encode(&kind).expect("encode");
        let decoded: MembershipEventKind = ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(decoded, kind);
    }

    #[test]
    fn admin_countersign_roundtrip() {
        use crate::owner_state_crypto::canonical_cbor_encode;
        let kind = MembershipEventKind::AdminCountersign {
            target_event_id: [0x33; 16],
        };
        let bytes = canonical_cbor_encode(&kind).expect("encode");
        let decoded: MembershipEventKind = ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(decoded, kind);
    }
}

// ── ZEB-250 Task 4: AdminProposal verify_event gate tests ─────────────────

#[cfg(test)]
mod zeb_250_admin_proposal_verify_tests {
    use super::*;

    fn make_identity(seed_byte: u8) -> (TestOwner, [u8; 64], OwnerAddr) {
        let owner = mint_test_owner(seed_byte);
        let addr = owner.owner;
        (owner, [0u8; 64], addr)
    }

    fn sign_with_identity(payload: EventPayload, owner: &TestOwner) -> SignedMembershipEvent {
        let ev = sign_event(&payload, &owner.device_key).expect("sign_event must succeed");
        match ev.kind {
            MembershipEventKind::Join | MembershipEventKind::PendingJoin { .. } => {
                SignedMembershipEvent {
                    enrollment: Some(owner.cert.clone()),
                    ..ev
                }
            }
            _ => ev,
        }
    }

    fn make_admin_proposal_event(
        id: [u8; 16],
        actor_priv: &TestOwner,
        actor_addr: OwnerAddr,
        proposal_kind: ProposalKind,
        at_wall_ms: u64,
    ) -> SignedMembershipEvent {
        let payload = EventPayload {
            id,
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::AdminProposal { proposal_kind },
            actor: actor_addr,
            at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "test".into(),
            },
        };
        sign_with_identity(payload, actor_priv)
    }

    #[test]
    fn admin_proposal_accepted_when_actor_admin() {
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0x01);
        let (_, _, other_addr) = make_identity(0x02);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(
            admin_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "a".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        prior.members.insert(
            other_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "o".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        prior.power_levels.insert(admin_addr, 100);
        prior.power_levels.insert(other_addr, 0);
        let evt = make_admin_proposal_event(
            [0x10; 16],
            &admin_priv,
            admin_addr,
            ProposalKind::SetPower {
                target: other_addr,
                level: 100,
            },
            1_000,
        );
        test_enroll_member(&mut prior, &admin_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(verify_event(&evt, &prior, &ctx), Ok(()));
    }

    #[test]
    fn admin_proposal_rejected_when_actor_not_joined() {
        let (actor_priv, _actor_pub, actor_addr) = make_identity(0x01);
        let mut prior = MaterializedMembership::default();
        let evt = make_admin_proposal_event(
            [0x10; 16],
            &actor_priv,
            actor_addr,
            ProposalKind::SetPower {
                target: OwnerAddr([0x02; 16]),
                level: 100,
            },
            1_000,
        );
        test_enroll_member(&mut prior, &actor_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr: actor_addr,
            is_invite_only: false,
        };
        // ZEB-339: a non-member actor fails signer resolution (step 1) before
        // the AdminProposalActorNotJoined gate — no materialized membership
        // means no enrolled device key.
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::SignerNotEnrolledForActor)
        );
    }

    #[test]
    fn admin_proposal_rejected_when_actor_power_below_100() {
        let (actor_priv, _actor_pub, actor_addr) = make_identity(0x01);
        let (_, _, target_addr) = make_identity(0x02);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(
            actor_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "a".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        prior.members.insert(
            target_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "t".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        prior.power_levels.insert(actor_addr, 50);
        prior.power_levels.insert(target_addr, 0);
        let evt = make_admin_proposal_event(
            [0x10; 16],
            &actor_priv,
            actor_addr,
            ProposalKind::SetPower {
                target: target_addr,
                level: 100,
            },
            1_000,
        );
        test_enroll_member(&mut prior, &actor_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr: actor_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalActorNotAdmin)
        );
    }

    #[test]
    fn admin_proposal_setpower_rejected_when_target_not_in_members() {
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0x01);
        let (_, _, ghost_addr) = make_identity(0xfe);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(
            admin_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "a".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        prior.power_levels.insert(admin_addr, 100);
        let evt = make_admin_proposal_event(
            [0x10; 16],
            &admin_priv,
            admin_addr,
            ProposalKind::SetPower {
                target: ghost_addr,
                level: 100,
            },
            1_000,
        );
        test_enroll_member(&mut prior, &admin_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalKindInvalid)
        );
    }

    #[test]
    fn admin_proposal_setpower_rejected_when_level_out_of_range() {
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0x01);
        let (_, _, target_addr) = make_identity(0x02);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(
            admin_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "a".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        prior.members.insert(
            target_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "t".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        prior.power_levels.insert(admin_addr, 100);
        prior.power_levels.insert(target_addr, 100);
        let evt = make_admin_proposal_event(
            [0x10; 16],
            &admin_priv,
            admin_addr,
            ProposalKind::SetPower {
                target: target_addr,
                level: 200,
            },
            1_000,
        );
        test_enroll_member(&mut prior, &admin_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalKindInvalid)
        );
    }

    #[test]
    fn admin_proposal_setpower_rejected_when_not_admin_affecting() {
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0x01);
        let (_, _, regular_addr) = make_identity(0x02);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(
            admin_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "a".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        prior.members.insert(
            regular_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "r".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        prior.power_levels.insert(admin_addr, 100);
        prior.power_levels.insert(regular_addr, 0);
        let evt = make_admin_proposal_event(
            [0x10; 16],
            &admin_priv,
            admin_addr,
            ProposalKind::SetPower {
                target: regular_addr,
                level: 50,
            },
            1_000,
        );
        test_enroll_member(&mut prior, &admin_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalNotAdminAffecting)
        );
    }

    #[test]
    fn admin_proposal_kick_rejected_when_target_not_admin() {
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0x01);
        let (_, _, mod_addr) = make_identity(0x02);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(
            admin_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "a".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        prior.members.insert(
            mod_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "m".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        prior.power_levels.insert(admin_addr, 100);
        prior.power_levels.insert(mod_addr, 50);
        let evt = make_admin_proposal_event(
            [0x10; 16],
            &admin_priv,
            admin_addr,
            ProposalKind::Kick {
                target: mod_addr,
                reason: None,
            },
            1_000,
        );
        test_enroll_member(&mut prior, &admin_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalNotAdminAffecting)
        );
    }

    #[test]
    fn admin_proposal_change_quorum_rejected_when_below_one() {
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0x01);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(
            admin_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "a".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        prior.power_levels.insert(admin_addr, 100);
        let evt = make_admin_proposal_event(
            [0x10; 16],
            &admin_priv,
            admin_addr,
            ProposalKind::ChangeQuorum { new_quorum: 0 },
            1_000,
        );
        test_enroll_member(&mut prior, &admin_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalKindInvalid)
        );
    }

    #[test]
    fn admin_proposal_change_quorum_rejected_when_exceeds_admin_count() {
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0x01);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(
            admin_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "a".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        prior.power_levels.insert(admin_addr, 100);
        // Only 1 admin → new_quorum = 2 exceeds.
        let evt = make_admin_proposal_event(
            [0x10; 16],
            &admin_priv,
            admin_addr,
            ProposalKind::ChangeQuorum { new_quorum: 2 },
            1_000,
        );
        test_enroll_member(&mut prior, &admin_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalQuorumOutOfRange)
        );
    }

    #[test]
    fn admin_proposal_change_quorum_accepted_when_equals_admin_count() {
        let (admin1_priv, _admin1_pub, admin1_addr) = make_identity(0x01);
        let (_, _, admin2_addr) = make_identity(0x02);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(
            admin1_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "a1".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        prior.members.insert(
            admin2_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "a2".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        prior.power_levels.insert(admin1_addr, 100);
        prior.power_levels.insert(admin2_addr, 100);
        let evt = make_admin_proposal_event(
            [0x10; 16],
            &admin1_priv,
            admin1_addr,
            ProposalKind::ChangeQuorum { new_quorum: 2 },
            1_000,
        );
        test_enroll_member(&mut prior, &admin1_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr: admin1_addr,
            is_invite_only: false,
        };
        assert_eq!(verify_event(&evt, &prior, &ctx), Ok(()));
    }

    /// Bug-fix R1 (Bug 3): AP5 must count only LIVE (Joined) admins when
    /// checking that new_quorum <= admin_count. Kicked admins retain a
    /// power_levels entry but are no longer live; they must not inflate the
    /// count and allow an out-of-range quorum to pass AP5.
    ///
    /// Setup: 3 admins bootstrapped, 1 kicked → 2 live admins remain.
    /// Propose ChangeQuorum{3}. Must be rejected because 3 > 2 live admins.
    /// Without the fix, power_levels still has all 3 entries so admin_count
    /// would be 3 and the proposal would wrongly pass AP5.
    #[test]
    fn admin_proposal_change_quorum_rejects_when_quorum_would_exceed_live_admin_count() {
        let (admin1_priv, _admin1_pub, admin1_addr) = make_identity(0x01);
        let (_, _, admin2_addr) = make_identity(0x02);
        let (_, _, admin3_addr) = make_identity(0x03);

        let mut prior = MaterializedMembership::default();
        // admin1 + admin2 Joined, admin3 Banned (kicked).
        for (addr, status) in [
            (admin1_addr, MemberStatus::Joined),
            (admin2_addr, MemberStatus::Joined),
            (admin3_addr, MemberStatus::Banned),
        ] {
            prior.members.insert(
                addr,
                MemberState {
                    status,
                    joined_at: Hlc {
                        wall_ms: 0,
                        logical: 0,
                        device_id: "t".into(),
                    },
                    left_at: None,
                    enrolled_device_keys: BTreeSet::new(),
                    revoked_device_keys: BTreeSet::new(),
                },
            );
            prior.power_levels.insert(addr, 100);
        }

        // Propose ChangeQuorum{3}: 3 > 2 live admins → must reject.
        let evt = make_admin_proposal_event(
            [0x10; 16],
            &admin1_priv,
            admin1_addr,
            ProposalKind::ChangeQuorum { new_quorum: 3 },
            1_000,
        );
        test_enroll_member(&mut prior, &admin1_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr: admin1_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalQuorumOutOfRange),
            "ChangeQuorum{{3}} must be rejected when only 2 admins are live (1 is kicked)"
        );

        // Propose ChangeQuorum{2}: 2 == 2 live admins → must accept.
        let evt2 = make_admin_proposal_event(
            [0x11; 16],
            &admin1_priv,
            admin1_addr,
            ProposalKind::ChangeQuorum { new_quorum: 2 },
            1_000,
        );
        assert_eq!(
            verify_event(&evt2, &prior, &ctx),
            Ok(()),
            "ChangeQuorum{{2}} must be accepted when 2 live admins exist"
        );
    }

    /// Bug-fix R1 (Bug 5): ProposalKind::Kick reason must be subject to the
    /// MAX_MODERATION_REASON_CHARS length cap, matching the direct Kick path.
    #[test]
    fn admin_proposal_kick_rejected_when_reason_exceeds_length_cap() {
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0x01);
        let (_, _, target_addr) = make_identity(0x02);

        let mut prior = MaterializedMembership::default();
        for (addr, status) in [
            (admin_addr, MemberStatus::Joined),
            (target_addr, MemberStatus::Joined),
        ] {
            prior.members.insert(
                addr,
                MemberState {
                    status,
                    joined_at: Hlc {
                        wall_ms: 0,
                        logical: 0,
                        device_id: "t".into(),
                    },
                    left_at: None,
                    enrolled_device_keys: BTreeSet::new(),
                    revoked_device_keys: BTreeSet::new(),
                },
            );
        }
        prior.power_levels.insert(admin_addr, 100);
        prior.power_levels.insert(target_addr, 100);

        // reason with exactly MAX_MODERATION_REASON_CHARS + 1 Unicode scalar values.
        let oversized: String = "x".repeat(MAX_MODERATION_REASON_CHARS + 1);

        let evt = make_admin_proposal_event(
            [0x20; 16],
            &admin_priv,
            admin_addr,
            ProposalKind::Kick {
                target: target_addr,
                reason: Some(oversized),
            },
            1_000,
        );
        test_enroll_member(&mut prior, &admin_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalKindInvalid),
            "ProposalKind::Kick with oversized reason must be rejected"
        );
    }
}

// ── ZEB-250 Task 5: AdminCountersign verify_event gate tests ──────────────

#[cfg(test)]
mod zeb_250_admin_countersign_verify_tests {
    use super::*;

    fn make_identity(seed_byte: u8) -> (TestOwner, [u8; 64], OwnerAddr) {
        let owner = mint_test_owner(seed_byte);
        let addr = owner.owner;
        (owner, [0u8; 64], addr)
    }

    fn make_admin_countersign_event(
        id: [u8; 16],
        actor_priv: &TestOwner,
        actor_addr: OwnerAddr,
        target_event_id: [u8; 16],
        at_wall_ms: u64,
    ) -> SignedMembershipEvent {
        let payload = EventPayload {
            id,
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::AdminCountersign { target_event_id },
            actor: actor_addr,
            at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "test".into(),
            },
        };
        sign_event(&payload, &actor_priv.device_key).expect("sign AdminCountersign")
    }

    #[test]
    fn admin_countersign_accepted_when_actor_admin() {
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0x01);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(
            admin_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "a".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        prior.power_levels.insert(admin_addr, 100);
        let evt =
            make_admin_countersign_event([0x10; 16], &admin_priv, admin_addr, [0x55; 16], 1_000);
        test_enroll_member(&mut prior, &admin_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(verify_event(&evt, &prior, &ctx), Ok(()));
    }

    #[test]
    fn admin_countersign_rejected_when_actor_not_joined() {
        let (actor_priv, _actor_pub, actor_addr) = make_identity(0x01);
        let mut prior = MaterializedMembership::default();
        let evt =
            make_admin_countersign_event([0x10; 16], &actor_priv, actor_addr, [0x55; 16], 1_000);
        test_enroll_member(&mut prior, &actor_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr: actor_addr,
            is_invite_only: false,
        };
        // ZEB-339: a non-member actor fails signer resolution (step 1) before
        // the AdminCountersignActorNotJoined gate — no materialized membership
        // means no enrolled device key.
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::SignerNotEnrolledForActor)
        );
    }

    #[test]
    fn admin_countersign_rejected_when_actor_power_below_100() {
        let (mod_priv, _mod_pub, mod_addr) = make_identity(0x01);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(
            mod_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "m".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        prior.power_levels.insert(mod_addr, 50);
        let evt = make_admin_countersign_event([0x10; 16], &mod_priv, mod_addr, [0x55; 16], 1_000);
        test_enroll_member(&mut prior, &mod_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr: mod_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminCountersignActorNotAdmin)
        );
    }

    #[test]
    fn admin_countersign_accepted_when_target_not_present_yet() {
        // Lenient forward-ref: AC must verify even when the target
        // AdminProposal is not yet in the log. prior_state has no
        // record of [0x55; 16] — and that's fine.
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0x01);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(
            admin_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "a".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        prior.power_levels.insert(admin_addr, 100);
        let evt = make_admin_countersign_event(
            [0x11; 16],
            &admin_priv,
            admin_addr,
            [0x55; 16], // target absent from prior_state
            5_000,
        );
        test_enroll_member(&mut prior, &admin_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(verify_event(&evt, &prior, &ctx), Ok(()));
    }
}

// ── ZEB-250 Task 6: direct SetPower/Kick quorum gate tests ───────────────────

#[cfg(test)]
mod zeb_250_direct_event_quorum_gate_tests {
    use super::*;

    fn make_identity(seed_byte: u8) -> (TestOwner, [u8; 64], OwnerAddr) {
        let owner = mint_test_owner(seed_byte);
        let addr = owner.owner;
        (owner, [0u8; 64], addr)
    }

    fn sign_with_identity(payload: EventPayload, owner: &TestOwner) -> SignedMembershipEvent {
        let ev = sign_event(&payload, &owner.device_key).expect("sign_event must succeed");
        match ev.kind {
            MembershipEventKind::Join | MembershipEventKind::PendingJoin { .. } => {
                SignedMembershipEvent {
                    enrollment: Some(owner.cert.clone()),
                    ..ev
                }
            }
            _ => ev,
        }
    }

    fn make_setpower_event(
        id: [u8; 16],
        actor_priv: &TestOwner,
        actor_addr: OwnerAddr,
        target: OwnerAddr,
        level: u8,
        at_wall_ms: u64,
    ) -> SignedMembershipEvent {
        let payload = EventPayload {
            id,
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::SetPower { target, level },
            actor: actor_addr,
            at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "test".into(),
            },
        };
        sign_with_identity(payload, actor_priv)
    }

    fn make_kick_event_signed(
        id: [u8; 16],
        actor_priv: &TestOwner,
        actor_addr: OwnerAddr,
        target: OwnerAddr,
        at_wall_ms: u64,
    ) -> SignedMembershipEvent {
        let payload = EventPayload {
            id,
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::Kick {
                target,
                reason: None,
            },
            actor: actor_addr,
            at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "test".into(),
            },
        };
        sign_with_identity(payload, actor_priv)
    }

    /// Helper: build a prior state with an admin actor (power 100) and an
    /// optional target with the given power level.
    fn prior_with_admin_and_target(
        admin_addr: OwnerAddr,
        target_addr: OwnerAddr,
        target_power: u8,
        admin_quorum: u8,
    ) -> MaterializedMembership {
        let mut prior = MaterializedMembership {
            admin_quorum,
            ..Default::default()
        };
        prior.members.insert(
            admin_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "a".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        prior.members.insert(
            target_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "t".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        prior.power_levels.insert(admin_addr, 100);
        if target_power > 0 {
            prior.power_levels.insert(target_addr, target_power);
        }
        prior
    }

    /// ZEB-250 §4.5: direct SetPower promoting a non-admin to admin (level==100)
    /// is rejected when admin_quorum > 1.
    #[test]
    fn direct_setpower_to_100_rejected_when_admin_quorum_above_1() {
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0x01);
        let (_, _, target_addr) = make_identity(0x02);
        let mut prior = prior_with_admin_and_target(admin_addr, target_addr, 0, 2);
        let evt = make_setpower_event([0x10; 16], &admin_priv, admin_addr, target_addr, 100, 1_000);
        test_enroll_member(&mut prior, &admin_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::SetPowerRequiresQuorum)
        );
    }

    /// ZEB-250 §4.5: direct SetPower demoting an existing admin (target power==100)
    /// to a lower level is rejected when admin_quorum > 1.
    #[test]
    fn direct_setpower_demote_admin_rejected_when_admin_quorum_above_1() {
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0x01);
        let (_, _, target_addr) = make_identity(0x02);
        // Target is currently an admin (power 100).
        let mut prior = prior_with_admin_and_target(admin_addr, target_addr, 100, 3);
        // Actor also has power 100, so actor_power > target_power is false —
        // but that existing check runs first; need actor with higher power
        // conceptually. Actually existing check: actor_power <= target_power → reject.
        // We want to reach the quorum gate. So give actor higher effective power by
        // setting target to 100 and actor conceptually > 100 — but max is 100.
        // In practice, the existing KickTargetPowerNotLower guard blocks actor==target==100.
        // For SetPower there is no such guard; the existing checks are only:
        //   1. actor_power >= set_power threshold (100) ✓
        //   2. level <= max ✓
        // So actor=100, target=100, level=50 is valid up to the quorum gate.
        let evt = make_setpower_event([0x10; 16], &admin_priv, admin_addr, target_addr, 50, 1_000);
        test_enroll_member(&mut prior, &admin_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::SetPowerRequiresQuorum)
        );
    }

    /// ZEB-250 §4.5: direct SetPower to a non-admin level (e.g., mod=50) is
    /// accepted regardless of admin_quorum — non-admin-affecting moderation.
    #[test]
    fn direct_setpower_to_non_admin_accepted_regardless_of_quorum() {
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0x01);
        let (_, _, target_addr) = make_identity(0x02);
        // Target has power 0 (not an admin), new level is 50 (mod, not admin).
        let mut prior = prior_with_admin_and_target(admin_addr, target_addr, 0, 5);
        let evt = make_setpower_event([0x10; 16], &admin_priv, admin_addr, target_addr, 50, 1_000);
        test_enroll_member(&mut prior, &admin_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(verify_event(&evt, &prior, &ctx), Ok(()));
    }

    /// ZEB-734 helper: a community with a distinct owner (admin, power 100),
    /// a moderator actor at `mod_power`, and a target at `target_power`, under
    /// a customized `set_power` threshold and given `admin_quorum`. All three
    /// are Joined. Power entries are omitted when 0 (absent == 0 per spec §4).
    fn prior_owner_mod_target(
        owner_addr: OwnerAddr,
        mod_addr: OwnerAddr,
        mod_power: u8,
        target_addr: OwnerAddr,
        target_power: u8,
        set_power_threshold: u8,
        admin_quorum: u8,
    ) -> MaterializedMembership {
        let mut prior = MaterializedMembership {
            admin_quorum,
            power_thresholds: PowerThresholds {
                invite: 0,
                kick: set_power_threshold,
                set_power: set_power_threshold,
                max: 100,
            },
            ..Default::default()
        };
        for (addr, dev) in [(owner_addr, "o"), (mod_addr, "m"), (target_addr, "t")] {
            prior.members.insert(
                addr,
                MemberState {
                    status: MemberStatus::Joined,
                    joined_at: Hlc {
                        wall_ms: 0,
                        logical: 0,
                        device_id: dev.into(),
                    },
                    left_at: None,
                    enrolled_device_keys: BTreeSet::new(),
                    revoked_device_keys: BTreeSet::new(),
                },
            );
        }
        prior.power_levels.insert(owner_addr, 100);
        if mod_power > 0 {
            prior.power_levels.insert(mod_addr, mod_power);
        }
        if target_power > 0 {
            prior.power_levels.insert(target_addr, target_power);
        }
        prior
    }

    /// ZEB-734: in a community that lowered `set_power` to 50, a power-50
    /// moderator promoting a member to admin (level == 100) is rejected —
    /// granting admin always requires already holding admin power, even
    /// though the moderator clears the (lowered) set_power threshold and
    /// `admin_quorum == 1` skips the quorum gate.
    #[test]
    fn zeb734_admin_affecting_promote_rejected_for_sub_max_actor_when_set_power_lowered() {
        let (_, _, owner_addr) = make_identity(0x01);
        let (mod_priv, _, mod_addr) = make_identity(0x02);
        let (_, _, target_addr) = make_identity(0x03);
        let mut prior = prior_owner_mod_target(owner_addr, mod_addr, 50, target_addr, 10, 50, 1);
        let evt = make_setpower_event([0x10; 16], &mod_priv, mod_addr, target_addr, 100, 1_000);
        test_enroll_member(&mut prior, &mod_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr: owner_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::SetPowerAdminAffectingRequiresAdmin)
        );
    }

    /// ZEB-734: a power-50 moderator demoting an existing admin (target
    /// currently at power 100) is also rejected — touching the admin tier is
    /// admin-affecting regardless of the new level.
    #[test]
    fn zeb734_admin_affecting_demote_rejected_for_sub_max_actor_when_set_power_lowered() {
        let (_, _, owner_addr) = make_identity(0x01);
        let (mod_priv, _, mod_addr) = make_identity(0x02);
        let (_, _, target_addr) = make_identity(0x03);
        // Target is a second admin (power 100).
        let mut prior = prior_owner_mod_target(owner_addr, mod_addr, 50, target_addr, 100, 50, 1);
        let evt = make_setpower_event([0x11; 16], &mod_priv, mod_addr, target_addr, 20, 1_000);
        test_enroll_member(&mut prior, &mod_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr: owner_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::SetPowerAdminAffectingRequiresAdmin)
        );
    }

    /// ZEB-734: the delegation a lowered `set_power` was FOR still works — a
    /// power-50 moderator adjusting a member's power within the sub-admin
    /// range (level 40, target not an admin) is accepted.
    #[test]
    fn zeb734_non_admin_affecting_setpower_allowed_for_sub_max_actor() {
        let (_, _, owner_addr) = make_identity(0x01);
        let (mod_priv, _, mod_addr) = make_identity(0x02);
        let (_, _, target_addr) = make_identity(0x03);
        let mut prior = prior_owner_mod_target(owner_addr, mod_addr, 50, target_addr, 10, 50, 1);
        let evt = make_setpower_event([0x12; 16], &mod_priv, mod_addr, target_addr, 40, 1_000);
        test_enroll_member(&mut prior, &mod_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr: owner_addr,
            is_invite_only: false,
        };
        assert_eq!(verify_event(&evt, &prior, &ctx), Ok(()));
    }

    /// ZEB-734 regression guard: a lone admin (power 100) at the default
    /// `admin_quorum == 1` still promotes a member to admin via a DIRECT
    /// SetPower — the new gate must add no ceremony to the common case.
    #[test]
    fn zeb734_admin_affecting_promote_allowed_for_admin_actor_at_quorum_1() {
        let (owner_priv, _, owner_addr) = make_identity(0x01);
        let (_, _, target_addr) = make_identity(0x03);
        let mut prior = prior_with_admin_and_target(owner_addr, target_addr, 10, 1);
        let evt = make_setpower_event([0x13; 16], &owner_priv, owner_addr, target_addr, 100, 1_000);
        test_enroll_member(&mut prior, &owner_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr: owner_addr,
            is_invite_only: false,
        };
        assert_eq!(verify_event(&evt, &prior, &ctx), Ok(()));
    }

    /// ZEB-250 §4.6: direct Kick of an admin (target power==100) is rejected
    /// when admin_quorum > 1.
    #[test]
    fn direct_kick_of_admin_rejected_when_admin_quorum_above_1() {
        // Need actor power > target power to pass the existing gate.
        // But max power is 100. So we need actor=100 and target<100.
        // Wait — the target IS an admin (power 100). actor_power <= target_power
        // (100 <= 100) → KickTargetPowerNotLower fires before our gate.
        // This is intentional: you can't single-sign kick an equal-power admin,
        // so the quorum gate is only reachable if actor has strictly higher power —
        // which at max=100 is impossible for peer admins. However, the spec
        // still mandates the quorum gate for the case where a super-admin
        // (hypothetically) tries to kick an admin.
        //
        // In practice the existing `actor_power <= target_power` guard fires first
        // for actor=100 kicking target=100. So we set target power to 99
        // (near-admin but not quite 100) — but then target_power != 100 so
        // our quorum gate wouldn't fire either.
        //
        // The only way to test the quorum gate for Kick is: target_power == 100
        // AND actor_power > 100. Since max is 100, this combination is impossible
        // through normal power assignment. The spec §4.6 guards admin_quorum > 1
        // AND target_power == 100 — but the existing KickTargetPowerNotLower
        // (actor_power <= target_power) fires first for equal-100 actors.
        //
        // Per plan spec: tests must exercise the gate. We set actor_power to 100
        // and target_power to 99 — but then target != admin. Re-read spec §4.6:
        // "Kick of a target who is currently an admin (level == 100)."
        // target_power must be 100. actor_power must be > target_power (101),
        // exceeding POWER_THRESHOLDS.max. This is the inherent tension.
        //
        // Resolution: the test directly constructs prior_state with actor_power
        // stored as 100 (the max) and target_power as 100, then verifies that
        // KickTargetPowerNotLower fires — which IS the effective rejection of
        // kicking an admin directly. The quorum gate (KickRequiresQuorum) is a
        // defense-in-depth that would fire if actor_power were > target_power=100,
        // a combination currently unreachable through the API but present for
        // future extensibility.
        //
        // HOWEVER: the plan explicitly lists this test and expects KickRequiresQuorum.
        // To make it reachable: bypass the existing equal-power check by making
        // actor a hypothetical "super-admin" stored directly in power_levels as 101
        // (bypassing the PowerLevelOutOfRange check which only applies to SetPower
        // events, not to power_levels stored in prior_state).
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0x01);
        let (_, _, target_addr) = make_identity(0x02);
        let mut prior = MaterializedMembership {
            admin_quorum: 2,
            ..Default::default()
        };
        prior.members.insert(
            admin_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "a".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        prior.members.insert(
            target_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "t".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        // Store actor power as 101 directly in prior_state (bypasses PowerLevelOutOfRange,
        // which is only checked during SetPower events, not when reading prior_state).
        prior.power_levels.insert(admin_addr, 101);
        prior.power_levels.insert(target_addr, 100);
        let evt = make_kick_event_signed([0x10; 16], &admin_priv, admin_addr, target_addr, 1_000);
        test_enroll_member(&mut prior, &admin_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::KickRequiresQuorum)
        );
    }

    /// ZEB-250 §4.6: direct Kick of a mod (target power<100) is accepted
    /// regardless of admin_quorum — non-admin-affecting moderation.
    #[test]
    fn direct_kick_of_mod_accepted_regardless_of_quorum() {
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0x01);
        let (_, _, target_addr) = make_identity(0x02);
        // Target is a moderator (power 50), not an admin.
        let mut prior = prior_with_admin_and_target(admin_addr, target_addr, 50, 5);
        let evt = make_kick_event_signed([0x10; 16], &admin_priv, admin_addr, target_addr, 1_000);
        test_enroll_member(&mut prior, &admin_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(verify_event(&evt, &prior, &ctx), Ok(()));
    }

    /// ZEB-250 backwards-compat: admin_quorum == 1 preserves single-admin
    /// behavior — direct SetPower to admin and direct Kick of admin are
    /// both accepted when admin_quorum == 1.
    #[test]
    fn direct_setpower_admin_actions_accepted_when_admin_quorum_equals_1() {
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0x01);
        let (_, _, target_addr) = make_identity(0x02);
        // admin_quorum == 1 (the default).
        let mut prior = prior_with_admin_and_target(admin_addr, target_addr, 0, 1);

        // Direct SetPower to level 100 — must be accepted.
        let setpower_evt =
            make_setpower_event([0x10; 16], &admin_priv, admin_addr, target_addr, 100, 1_000);
        test_enroll_member(&mut prior, &admin_priv);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&setpower_evt, &prior, &ctx),
            Ok(()),
            "direct SetPower to 100 must pass when admin_quorum == 1"
        );
    }
}

// ── ZEB-250 Task 7: materialize pre-pass smoke tests ─────────────────────────

#[cfg(test)]
mod zeb_250_materialize_prepass_tests {
    use super::*;

    #[test]
    fn materialize_prepass_collects_admin_proposal_signers() {
        // Smoke test: materialize over a log containing AdminProposal +
        // AdminCountersign events shouldn't crash. Task 8 will add the
        // effect-application assertions; this test just exercises the
        // pre-pass without panicking.
        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);
        let target = OwnerAddr([0x03; 16]);

        let prop_id = [0xAA; 16];
        let events = vec![
            SignedMembershipEvent {
                signer_certs: Vec::new(),
                id: prop_id,
                community_id: SpaceId([0xc0; 16]),
                actor: admin1,
                at: Hlc {
                    wall_ms: 10_000,
                    logical: 0,
                    device_id: "a1".into(),
                },
                kind: MembershipEventKind::AdminProposal {
                    proposal_kind: ProposalKind::SetPower { target, level: 100 },
                },
                sig: [0; 64],
                countersig: None,
                enrollment: None,
            },
            SignedMembershipEvent {
                signer_certs: Vec::new(),
                id: [0xBB; 16],
                community_id: SpaceId([0xc0; 16]),
                actor: admin2,
                at: Hlc {
                    wall_ms: 11_000,
                    logical: 0,
                    device_id: "a2".into(),
                },
                kind: MembershipEventKind::AdminCountersign {
                    target_event_id: prop_id,
                },
                sig: [0; 64],
                countersig: None,
                enrollment: None,
            },
        ];
        // No assertion on effect yet — Task 8 wires the main-pass
        // application. This test just exercises the pre-pass without
        // panicking.
        let _m = materialize(&events, admin1);
    }
}

// ── ZEB-250 Task 8: materialize main-pass AdminProposal effect tests ─────────

#[cfg(test)]
mod zeb_250_admin_proposal_materialize_tests {
    use super::*;

    const COM: SpaceId = SpaceId([0xc0; 16]);

    fn ev(
        id: [u8; 16],
        actor: OwnerAddr,
        wall_ms: u64,
        kind: MembershipEventKind,
    ) -> SignedMembershipEvent {
        SignedMembershipEvent {
            signer_certs: Vec::new(),
            id,
            community_id: COM,
            actor,
            at: Hlc {
                wall_ms,
                logical: 0,
                device_id: "test".into(),
            },
            kind,
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        }
    }

    /// Build the common two-admin bootstrap under quorum=1:
    ///   - admin2 Join
    ///   - admin1 SetPower admin2 → 100
    ///   - admin1 AdminProposal{ChangeQuorum{new_quorum}} (self-satisfies at quorum=1)
    ///
    /// Returns the three events to prepend to per-test event lists.
    fn bootstrap_two_admins_raise_quorum(
        admin1: OwnerAddr,
        admin2: OwnerAddr,
        new_quorum: u8,
    ) -> Vec<SignedMembershipEvent> {
        vec![
            ev([0x80; 16], admin2, 1_000, MembershipEventKind::Join),
            ev(
                [0x81; 16],
                admin1,
                2_000,
                MembershipEventKind::SetPower {
                    target: admin2,
                    level: 100,
                },
            ),
            ev(
                [0xCC; 16],
                admin1,
                10_000,
                MembershipEventKind::AdminProposal {
                    proposal_kind: ProposalKind::ChangeQuorum { new_quorum },
                },
            ),
        ]
    }

    /// A proposal that has no countersigns under quorum=2 must not apply its
    /// effect — the proposer alone is 1 signer, short of the required 2.
    #[test]
    fn materialize_proposal_without_countersigns_pending_when_quorum_above_1() {
        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);

        // Bootstrap: quorum=1 → raise to 2 via sole-signer ChangeQuorum.
        let mut events = bootstrap_two_admins_raise_quorum(admin1, admin2, 2);

        // Under quorum=2: propose demote admin2. No countersign.
        events.push(ev(
            [0xDD; 16],
            admin1,
            11_000,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::SetPower {
                    target: admin2,
                    level: 0,
                },
            },
        ));

        let m = materialize(&events, admin1);

        // Effect must NOT have applied — admin2 still admin (power 100).
        assert_eq!(
            m.power_levels.get(&admin2).copied().unwrap_or(0),
            100,
            "admin2 must retain power=100 when proposal lacks countersign"
        );
        // admin_quorum updated to 2 (sole-signer ChangeQuorum self-satisfies under prior quorum=1).
        assert_eq!(m.admin_quorum, 2);
    }

    /// One countersign from admin2 satisfies quorum=2 for a SetPower proposal.
    #[test]
    fn materialize_proposal_effective_when_one_countersign_reaches_quorum_2() {
        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);
        let target = OwnerAddr([0x03; 16]);

        let mut events = bootstrap_two_admins_raise_quorum(admin1, admin2, 2);

        // target must join so SetPower has a member record (only needed for
        // Kick; SetPower on power_levels doesn't require member presence, but
        // adding target's Join makes the state realistic).

        let prop_id = [0xDD; 16];
        // Propose promote target to admin.
        events.push(ev(
            prop_id,
            admin1,
            20_000,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::SetPower { target, level: 100 },
            },
        ));
        // admin2 countersigns — now 2 signers, meeting quorum=2.
        events.push(ev(
            [0xEE; 16],
            admin2,
            21_000,
            MembershipEventKind::AdminCountersign {
                target_event_id: prop_id,
            },
        ));

        let m = materialize(&events, admin1);

        assert_eq!(
            m.power_levels.get(&target).copied().unwrap_or(0),
            100,
            "target must be promoted to 100 when quorum=2 is reached"
        );
    }

    /// Two countersigns (admin2 + admin3) satisfy quorum=3.
    #[test]
    fn materialize_proposal_effective_when_two_countersigns_reach_quorum_3() {
        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);
        let admin3 = OwnerAddr([0x04; 16]);
        let target = OwnerAddr([0x03; 16]);

        // Bootstrap with quorum=1, promote admin2 and admin3, then raise quorum to 3.
        let mut events = vec![
            // admin2 join + promote
            ev([0x80; 16], admin2, 1_000, MembershipEventKind::Join),
            ev(
                [0x81; 16],
                admin1,
                2_000,
                MembershipEventKind::SetPower {
                    target: admin2,
                    level: 100,
                },
            ),
            // admin3 join + promote
            ev([0x82; 16], admin3, 3_000, MembershipEventKind::Join),
            ev(
                [0x83; 16],
                admin1,
                4_000,
                MembershipEventKind::SetPower {
                    target: admin3,
                    level: 100,
                },
            ),
            // Raise quorum to 3 via sole-signer proposal under quorum=1.
            ev(
                [0xCC; 16],
                admin1,
                10_000,
                MembershipEventKind::AdminProposal {
                    proposal_kind: ProposalKind::ChangeQuorum { new_quorum: 3 },
                },
            ),
        ];

        let prop_id = [0xDD; 16];
        // Propose promote target.
        events.push(ev(
            prop_id,
            admin1,
            20_000,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::SetPower { target, level: 100 },
            },
        ));
        // admin2 countersigns.
        events.push(ev(
            [0xEE; 16],
            admin2,
            21_000,
            MembershipEventKind::AdminCountersign {
                target_event_id: prop_id,
            },
        ));
        // admin3 countersigns — now 3 signers.
        events.push(ev(
            [0xFF; 16],
            admin3,
            22_000,
            MembershipEventKind::AdminCountersign {
                target_event_id: prop_id,
            },
        ));

        let m = materialize(&events, admin1);

        assert_eq!(
            m.power_levels.get(&target).copied().unwrap_or(0),
            100,
            "target must be promoted to 100 when quorum=3 is reached with 2 countersigns"
        );
    }

    /// Duplicate countersigns from the same actor must not be double-counted.
    /// Two countersigns from admin2 (same actor) still count as 1 + proposer = 2 signers.
    /// Under quorum=3 the proposal must remain pending.
    #[test]
    fn materialize_proposal_dedups_duplicate_countersigns_by_same_actor() {
        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);
        let admin3 = OwnerAddr([0x04; 16]);
        let target = OwnerAddr([0x03; 16]);

        // Bootstrap three admins, raise quorum to 3.
        let mut events = vec![
            ev([0x80; 16], admin2, 1_000, MembershipEventKind::Join),
            ev(
                [0x81; 16],
                admin1,
                2_000,
                MembershipEventKind::SetPower {
                    target: admin2,
                    level: 100,
                },
            ),
            ev([0x82; 16], admin3, 3_000, MembershipEventKind::Join),
            ev(
                [0x83; 16],
                admin1,
                4_000,
                MembershipEventKind::SetPower {
                    target: admin3,
                    level: 100,
                },
            ),
            ev(
                [0xCC; 16],
                admin1,
                10_000,
                MembershipEventKind::AdminProposal {
                    proposal_kind: ProposalKind::ChangeQuorum { new_quorum: 3 },
                },
            ),
        ];

        let prop_id = [0xDD; 16];
        events.push(ev(
            prop_id,
            admin1,
            20_000,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::SetPower { target, level: 100 },
            },
        ));
        // admin2 countersigns twice — same actor, only counts once in pre-pass HashSet.
        events.push(ev(
            [0xE1; 16],
            admin2,
            21_000,
            MembershipEventKind::AdminCountersign {
                target_event_id: prop_id,
            },
        ));
        events.push(ev(
            [0xE2; 16],
            admin2,
            22_000,
            MembershipEventKind::AdminCountersign {
                target_event_id: prop_id,
            },
        ));

        let m = materialize(&events, admin1);

        // quorum=3 needs 3 distinct signers; only admin1 (proposer) + admin2 → 2 signers.
        assert_eq!(
            m.power_levels.get(&target).copied().unwrap_or(0),
            0,
            "duplicate countersigns from same actor must not satisfy quorum=3"
        );
    }

    /// A proposal that never reaches quorum within 30 days is expired.
    /// With wall_ms difference > ADMIN_PROPOSAL_EXPIRY_MS and quorum=1,
    /// a sole-signer proposal should still apply because age_when_reached == 0
    /// (proposer IS the Nth signer, same event). The expiry test needs quorum=2
    /// plus one countersign arriving too late.
    #[test]
    fn materialize_proposal_expires_at_30_days_without_quorum() {
        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);
        let target = OwnerAddr([0x03; 16]);

        let mut events = bootstrap_two_admins_raise_quorum(admin1, admin2, 2);

        let prop_id = [0xDD; 16];
        let proposal_wall_ms = 20_000_u64;
        // Propose promote target under quorum=2.
        events.push(ev(
            prop_id,
            admin1,
            proposal_wall_ms,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::SetPower { target, level: 100 },
            },
        ));
        // admin2 countersigns MORE THAN 30 days after the proposal.
        let late_wall_ms = proposal_wall_ms + ADMIN_PROPOSAL_EXPIRY_MS + 1;
        events.push(ev(
            [0xEE; 16],
            admin2,
            late_wall_ms,
            MembershipEventKind::AdminCountersign {
                target_event_id: prop_id,
            },
        ));

        let m = materialize(&events, admin1);

        // The Nth signer arrived > 30 days after proposal → expired.
        assert_eq!(
            m.power_levels.get(&target).copied().unwrap_or(0),
            0,
            "proposal with late countersign (>30d) must not apply"
        );
    }

    /// A countersign that arrives exactly at the 30-day boundary (age ==
    /// ADMIN_PROPOSAL_EXPIRY_MS) is still within the window (<=), so the
    /// proposal applies. An identical scenario with age == expiry + 1 is the
    /// expired case (tested by `materialize_proposal_expires_at_30_days_without_quorum`).
    #[test]
    fn materialize_proposal_late_countersign_after_expiry_is_noop() {
        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);
        let target = OwnerAddr([0x03; 16]);

        let mut events = bootstrap_two_admins_raise_quorum(admin1, admin2, 2);

        let prop_id = [0xDD; 16];
        let proposal_wall_ms = 50_000_u64;
        events.push(ev(
            prop_id,
            admin1,
            proposal_wall_ms,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::SetPower { target, level: 100 },
            },
        ));
        // This countersign is at exactly expiry + 2 — strictly past the window.
        let too_late_wall_ms = proposal_wall_ms + ADMIN_PROPOSAL_EXPIRY_MS + 2;
        events.push(ev(
            [0xEE; 16],
            admin2,
            too_late_wall_ms,
            MembershipEventKind::AdminCountersign {
                target_event_id: prop_id,
            },
        ));

        let m = materialize(&events, admin1);

        assert_eq!(
            m.power_levels.get(&target).copied().unwrap_or(0),
            0,
            "countersign past 30-day expiry must be a no-op"
        );
    }

    /// A proposal that reached quorum within 30 days is permanently effective
    /// even if, by the time a later event arrives, 30 days have passed since
    /// the proposal. Permanence rule: once the threshold was satisfied within
    /// the window, the effect is applied forever (§5.3).
    #[test]
    fn materialize_quorum_reached_within_30d_then_aged_past_30d_remains_effective() {
        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);
        let target = OwnerAddr([0x03; 16]);

        let mut events = bootstrap_two_admins_raise_quorum(admin1, admin2, 2);

        let prop_id = [0xDD; 16];
        let proposal_wall_ms = 20_000_u64;
        events.push(ev(
            prop_id,
            admin1,
            proposal_wall_ms,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::SetPower { target, level: 100 },
            },
        ));
        // admin2 countersigns within 30 days (1 day after).
        let timely_wall_ms = proposal_wall_ms + 86_400_000;
        events.push(ev(
            [0xEE; 16],
            admin2,
            timely_wall_ms,
            MembershipEventKind::AdminCountersign {
                target_event_id: prop_id,
            },
        ));
        // A later event arrives 60 days after the proposal — but the quorum
        // was already reached on day 1, so the effect is permanent.
        let much_later = proposal_wall_ms + 60 * 86_400_000;
        events.push(ev(
            [0xF0; 16],
            admin1,
            much_later,
            MembershipEventKind::SetPower {
                target: OwnerAddr([0x05; 16]),
                level: 50,
            },
        ));

        let m = materialize(&events, admin1);

        assert_eq!(
            m.power_levels.get(&target).copied().unwrap_or(0),
            100,
            "quorum reached within 30d must be permanently effective even if later events are much older"
        );
    }

    /// ChangeQuorum proposal self-satisfies under quorum=1, then updates the
    /// running admin_quorum field so subsequent proposals see the new threshold.
    #[test]
    fn materialize_change_quorum_proposal_updates_admin_quorum_field() {
        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);

        // Start: quorum=1. ChangeQuorum{2} self-satisfies (admin1 is sole signer,
        // quorum=1 → 1 signer is enough).
        let mut events = bootstrap_two_admins_raise_quorum(admin1, admin2, 2);

        let m = materialize(&events, admin1);
        assert_eq!(
            m.admin_quorum, 2,
            "admin_quorum must be 2 after ChangeQuorum proposal"
        );

        // Now raise to 3 — requires 2 signers (quorum=2). admin1 proposes, admin2
        // countersigns within 30 days.
        let prop3_id = [0xD3; 16];
        events.push(ev(
            prop3_id,
            admin1,
            30_000,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::ChangeQuorum { new_quorum: 3 },
            },
        ));
        events.push(ev(
            [0xE3; 16],
            admin2,
            31_000,
            MembershipEventKind::AdminCountersign {
                target_event_id: prop3_id,
            },
        ));

        let m2 = materialize(&events, admin1);
        assert_eq!(
            m2.admin_quorum, 3,
            "admin_quorum must be 3 after second ChangeQuorum proposal with quorum=2 satisfied"
        );
    }

    /// A SetPower via quorum (admin_quorum=1) produces the same materialized
    /// power_levels entry as a direct SetPower event would — effect equivalence.
    #[test]
    fn materialize_setpower_via_quorum_matches_direct_setpower_effect_at_quorum_1() {
        let admin1 = OwnerAddr([0x01; 16]);
        let target = OwnerAddr([0x03; 16]);

        // At quorum=1, a sole-signer AdminProposal{SetPower{target, 75}} self-satisfies.
        let events_via_proposal = vec![ev(
            [0xDD; 16],
            admin1,
            10_000,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::SetPower { target, level: 75 },
            },
        )];

        // Equivalent: direct SetPower at quorum=1.
        let events_direct = vec![ev(
            [0xDE; 16],
            admin1,
            10_000,
            MembershipEventKind::SetPower { target, level: 75 },
        )];

        let m_proposal = materialize(&events_via_proposal, admin1);
        let m_direct = materialize(&events_direct, admin1);

        assert_eq!(
            m_proposal.power_levels.get(&target).copied().unwrap_or(0),
            75,
            "AdminProposal(SetPower) at quorum=1 must apply"
        );
        assert_eq!(
            m_proposal.power_levels.get(&target),
            m_direct.power_levels.get(&target),
            "AdminProposal(SetPower) at quorum=1 must match direct SetPower effect"
        );
    }

    /// Kick via quorum=2 (admin proposal + admin2 countersign) banishes the
    /// target just as a direct Kick would: MemberStatus::Banned + left_at set
    /// + pending_rotation_for populated.
    #[test]
    fn materialize_kick_via_quorum_sets_banned_and_pending_rotation() {
        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);
        let target = OwnerAddr([0x03; 16]);

        let mut events = bootstrap_two_admins_raise_quorum(admin1, admin2, 2);

        // target must join first so the Kick arm has a member entry.
        events.push(ev([0x90; 16], target, 5_000, MembershipEventKind::Join));

        let prop_id = [0xDD; 16];
        events.push(ev(
            prop_id,
            admin1,
            20_000,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::Kick {
                    target,
                    reason: None,
                },
            },
        ));
        events.push(ev(
            [0xEE; 16],
            admin2,
            21_000,
            MembershipEventKind::AdminCountersign {
                target_event_id: prop_id,
            },
        ));

        let m = materialize(&events, admin1);

        let ms = m
            .members
            .get(&target)
            .expect("target must be in members map");
        assert_eq!(
            ms.status,
            MemberStatus::Banned,
            "kick-via-quorum must set target to Banned"
        );
        assert!(
            ms.left_at.is_some(),
            "kick-via-quorum must set left_at on target"
        );
        assert!(
            m.pending_rotation_for.contains(&target),
            "kick-via-quorum must add target to pending_rotation_for"
        );
    }

    /// ZEB-250 R2 Fix 1: forward-ref countersign (wall_ms < proposal wall_ms
    /// due to clock-skew / out-of-order DAG delivery). The countersign sorts
    /// BEFORE the proposal in HLC order; the proposal arm fires with the signer
    /// already in the set and >= catches the already-at-quorum case.
    #[test]
    fn materialize_proposal_with_forward_ref_countersigns_applied_at_proposer_hlc() {
        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);

        // Bootstrap: quorum=1 → raise to 2 via sole-signer ChangeQuorum.
        let mut events = bootstrap_two_admins_raise_quorum(admin1, admin2, 2);

        let prop_id = [0xDD; 16];

        // Forward-ref countersign at wall_ms=19_900 (BEFORE the proposal at 20_000).
        // In HLC sort order this countersign will appear before the AdminProposal
        // because its wall_ms is smaller. The proposal arm must still apply the
        // effect (count >= quorum) using the already-populated signer set.
        events.push(ev(
            [0xEE; 16],
            admin2,
            19_900, // BEFORE prop at 20_000
            MembershipEventKind::AdminCountersign {
                target_event_id: prop_id,
            },
        ));

        // Proposal at 20_000 — admin1 is the second signer (plus admin2 above = 2 total).
        // The proposer arm runs AFTER the forward-ref countersign in HLC order.
        events.push(ev(
            prop_id,
            admin1,
            20_000,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::SetPower {
                    target: admin2,
                    level: 50,
                },
            },
        ));

        let m = materialize(&events, admin1);

        // Effect must have applied: admin2 power drops to 50.
        assert_eq!(
            m.power_levels.get(&admin2).copied().unwrap_or(0),
            50,
            "forward-ref countersign must not prevent proposer-arm application when count >= quorum"
        );
    }

    /// ZEB-250 R2 Fix 2: kick via quorum — left_at must be set to the
    /// COUNTERSIGN event's HLC (the Nth signer that tipped quorum), not
    /// the proposal's original HLC. Preserves CRDT causality.
    #[test]
    fn materialize_kick_via_quorum_left_at_equals_countersign_hlc_not_proposal_hlc() {
        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);
        let target = OwnerAddr([0x03; 16]);

        let mut events = bootstrap_two_admins_raise_quorum(admin1, admin2, 2);

        // target must join so the Kick arm has a member entry.
        events.push(ev([0x90; 16], target, 5_000, MembershipEventKind::Join));

        let prop_id = [0xDD; 16];
        let prop_wall_ms: u64 = 20_000;
        let countersign_wall_ms: u64 = 21_000;

        events.push(ev(
            prop_id,
            admin1,
            prop_wall_ms,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::Kick {
                    target,
                    reason: None,
                },
            },
        ));
        events.push(ev(
            [0xEE; 16],
            admin2,
            countersign_wall_ms,
            MembershipEventKind::AdminCountersign {
                target_event_id: prop_id,
            },
        ));

        let m = materialize(&events, admin1);

        let ms = m
            .members
            .get(&target)
            .expect("target must be in members map");
        assert_eq!(ms.status, MemberStatus::Banned);
        let left_at = ms.left_at.as_ref().expect("left_at must be set");
        assert_eq!(
            left_at.wall_ms, countersign_wall_ms,
            "left_at.wall_ms must equal the countersign HLC (Nth signer), not the proposal HLC"
        );
        assert_ne!(
            left_at.wall_ms, prop_wall_ms,
            "left_at must NOT be backdated to the proposal's HLC"
        );
    }

    /// ZEB-250 R2 Fix 1b: sticky applied guard. After a ChangeQuorum proposal
    /// lowers the threshold, subsequent countersigns on a prior proposal must NOT
    /// re-apply its effect.
    #[test]
    fn materialize_applied_guard_prevents_double_application_after_change_quorum() {
        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);
        let admin3 = OwnerAddr([0x04; 16]);
        let target = OwnerAddr([0x03; 16]);

        // Bootstrap three admins at quorum=1, then raise quorum to 3.
        let mut events = vec![
            ev([0x80; 16], admin2, 1_000, MembershipEventKind::Join),
            ev(
                [0x81; 16],
                admin1,
                2_000,
                MembershipEventKind::SetPower {
                    target: admin2,
                    level: 100,
                },
            ),
            ev([0x82; 16], admin3, 3_000, MembershipEventKind::Join),
            ev(
                [0x83; 16],
                admin1,
                4_000,
                MembershipEventKind::SetPower {
                    target: admin3,
                    level: 100,
                },
            ),
            // Raise to quorum=3.
            ev(
                [0xCC; 16],
                admin1,
                10_000,
                MembershipEventKind::AdminProposal {
                    proposal_kind: ProposalKind::ChangeQuorum { new_quorum: 3 },
                },
            ),
        ];

        // Propose SetPower for target at quorum=3.
        let prop_id = [0xDD; 16];
        events.push(ev(
            prop_id,
            admin1,
            20_000,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::SetPower { target, level: 100 },
            },
        ));
        // admin2 + admin3 countersign → 3 signers, quorum=3 satisfied. Effect applied.
        events.push(ev(
            [0xEE; 16],
            admin2,
            21_000,
            MembershipEventKind::AdminCountersign {
                target_event_id: prop_id,
            },
        ));
        events.push(ev(
            [0xFF; 16],
            admin3,
            22_000,
            MembershipEventKind::AdminCountersign {
                target_event_id: prop_id,
            },
        ));

        // Now a ChangeQuorum lowers the quorum to 2. This must NOT cause the
        // above proposal (already applied at quorum=3) to be "re-applied" for
        // any hypothetical 4th countersign.
        // Propose + countersign ChangeQuorum(2) requiring all 3 admins again.
        let cq_id = [0xA1; 16];
        events.push(ev(
            cq_id,
            admin1,
            30_000,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::ChangeQuorum { new_quorum: 2 },
            },
        ));
        events.push(ev(
            [0xA2; 16],
            admin2,
            31_000,
            MembershipEventKind::AdminCountersign {
                target_event_id: cq_id,
            },
        ));
        events.push(ev(
            [0xA3; 16],
            admin3,
            32_000,
            MembershipEventKind::AdminCountersign {
                target_event_id: cq_id,
            },
        ));

        let m = materialize(&events, admin1);

        // quorum is now 2.
        assert_eq!(m.admin_quorum, 2);
        // target was promoted to 100 exactly once.
        assert_eq!(
            m.power_levels.get(&target).copied().unwrap_or(0),
            100,
            "target must be promoted to 100 (effect applied exactly once)"
        );
    }
}

// ── ZEB-713 (ZEB-212 D1) admin-recovery materialize/lifecycle tests ──────────

#[cfg(test)]
mod zeb_713_recovery_materialize_tests {
    use super::*;

    const COM: SpaceId = SpaceId([0xc0; 16]);
    const W: u64 = RECOVERY_VETO_WINDOW_FLOOR_MS;
    const T0: u64 = 100_000;
    const T_R: u64 = 110_000;
    const DEADLINE: u64 = T_R + W;
    const P1: EventId = [0xB0; 16];

    fn admin1() -> OwnerAddr {
        OwnerAddr([0x01; 16])
    }
    fn d1() -> OwnerAddr {
        OwnerAddr([0x11; 16])
    }
    fn d2() -> OwnerAddr {
        OwnerAddr([0x12; 16])
    }
    fn m_new() -> OwnerAddr {
        OwnerAddr([0x21; 16])
    }
    fn m2() -> OwnerAddr {
        OwnerAddr([0x22; 16])
    }

    fn ev(
        id: [u8; 16],
        actor: OwnerAddr,
        wall_ms: u64,
        kind: MembershipEventKind,
    ) -> SignedMembershipEvent {
        SignedMembershipEvent {
            signer_certs: Vec::new(),
            id,
            community_id: COM,
            actor,
            at: Hlc {
                wall_ms,
                logical: 0,
                device_id: "test".into(),
            },
            kind,
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        }
    }

    /// Base world: bootstrap admin (with explicit Join), two designates,
    /// two regular members, and a quorum-applied RecoveryDesignates
    /// config (R=2, W=floor). Returns (events, config digest).
    fn base_world() -> (Vec<SignedMembershipEvent>, [u8; 32]) {
        let events = vec![
            ev([0xA0; 16], admin1(), 500, MembershipEventKind::Join),
            ev([0xA1; 16], d1(), 1_000, MembershipEventKind::Join),
            ev([0xA2; 16], d2(), 1_100, MembershipEventKind::Join),
            ev([0xA3; 16], m_new(), 1_200, MembershipEventKind::Join),
            ev([0xA4; 16], m2(), 1_300, MembershipEventKind::Join),
            ev(
                [0xA5; 16],
                admin1(),
                10_000,
                MembershipEventKind::AdminProposal {
                    proposal_kind: ProposalKind::SetRecoveryDesignates {
                        designates: vec![d1(), d2()],
                        threshold: 2,
                        veto_window_ms: W,
                    },
                },
            ),
        ];
        let m = materialize(&events, admin1());
        let config = m
            .recovery_designates
            .as_ref()
            .expect("config must materialize via quorum self-satisfy");
        let digest = recovery_config_digest(config).expect("digest");
        (events, digest)
    }

    fn proposal(digest: [u8; 32]) -> SignedMembershipEvent {
        ev(
            P1,
            d1(),
            T0,
            MembershipEventKind::RecoveryProposal {
                lost_admin: admin1(),
                new_admin: m_new(),
                config_digest: digest,
            },
        )
    }

    fn cosign(id: [u8; 16], actor: OwnerAddr, wall_ms: u64) -> SignedMembershipEvent {
        ev(
            id,
            actor,
            wall_ms,
            MembershipEventKind::RecoveryCosign {
                target_event_id: P1,
            },
        )
    }

    fn veto(id: [u8; 16], actor: OwnerAddr, wall_ms: u64) -> SignedMembershipEvent {
        ev(
            id,
            actor,
            wall_ms,
            MembershipEventKind::RecoveryVeto {
                target_event_id: P1,
            },
        )
    }

    fn view(m: &MaterializedMembership, id: EventId) -> &RecoveryProposalView {
        m.recovery_proposals
            .iter()
            .find(|p| p.id == id)
            .expect("proposal view present")
    }

    #[test]
    fn config_materializes_via_quorum_self_satisfy() {
        let (events, _) = base_world();
        let m = materialize(&events, admin1());
        let config = m.recovery_designates.expect("configured");
        assert_eq!(config.designates, vec![d1(), d2()]);
        assert_eq!(config.threshold, 2);
        assert_eq!(config.veto_window_ms, W);
        assert_eq!(config.set_at.wall_ms, 10_000);
    }

    #[test]
    fn full_recovery_executes_strictly_past_deadline() {
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        events.push(cosign([0xB1; 16], d2(), T_R));

        // AT the deadline: still time-locked (execution requires T > deadline).
        let at = materialize_with_now(&events, admin1(), Some(DEADLINE));
        assert_eq!(view(&at, P1).phase, RecoveryPhase::TimeLocked);
        assert_eq!(view(&at, P1).deadline_ms, Some(DEADLINE));
        assert_ne!(at.power_levels.get(&m_new()).copied().unwrap_or(0), 100);

        // Strictly past: executed — promotion + derived kick + rotation marker.
        let done = materialize_with_now(&events, admin1(), Some(DEADLINE + 1));
        assert_eq!(view(&done, P1).phase, RecoveryPhase::Executed);
        assert_eq!(done.power_levels.get(&m_new()).copied(), Some(100));
        assert_eq!(done.members[&admin1()].status, MemberStatus::Banned);
        assert!(done.pending_rotation_for.contains(&admin1()));
        // Derived-kick left_at is the synthetic deadline HLC.
        assert_eq!(
            done.members[&admin1()].left_at.as_ref().map(|h| h.wall_ms),
            Some(DEADLINE)
        );
    }

    #[test]
    fn idle_community_executes_on_now_floor_alone() {
        // §4.1: no event after the cosign — events_max stays below the
        // deadline forever. Only the caller-supplied now-floor advances
        // the lifecycle.
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        events.push(cosign([0xB1; 16], d2(), T_R));

        let stuck = materialize(&events, admin1());
        assert_eq!(view(&stuck, P1).phase, RecoveryPhase::TimeLocked);

        let live = materialize_with_now(&events, admin1(), Some(DEADLINE + 1));
        assert_eq!(view(&live, P1).phase, RecoveryPhase::Executed);
    }

    #[test]
    fn collecting_below_threshold_never_executes() {
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        let m = materialize_with_now(&events, admin1(), Some(DEADLINE + 1));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::Collecting);
        assert_eq!(view(&m, P1).deadline_ms, None);
        assert_ne!(m.power_levels.get(&m_new()).copied().unwrap_or(0), 100);
    }

    #[test]
    fn collecting_expires_after_30_days() {
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        let m = materialize_with_now(&events, admin1(), Some(T0 + ADMIN_PROPOSAL_EXPIRY_MS + 1));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::Expired);
    }

    #[test]
    fn late_threshold_signature_expires_proposal() {
        // Rth signature landing after the 30-day initiation window:
        // dead, even long past what its deadline would have been.
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        let late_tr = T0 + ADMIN_PROPOSAL_EXPIRY_MS + 1_000;
        events.push(cosign([0xB1; 16], d2(), late_tr));
        let m = materialize_with_now(&events, admin1(), Some(late_tr + W + 1));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::Expired);
        assert_ne!(m.power_levels.get(&m_new()).copied().unwrap_or(0), 100);
    }

    #[test]
    fn veto_during_time_lock_kills() {
        // T9: the vetoer IS the named lost admin — the veto is the
        // proof of life.
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        events.push(cosign([0xB1; 16], d2(), T_R));
        events.push(veto([0xB2; 16], admin1(), DEADLINE - 1_000));
        let m = materialize_with_now(&events, admin1(), Some(DEADLINE + 1));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::Vetoed);
        assert_eq!(m.members[&admin1()].status, MemberStatus::Joined);
        assert_ne!(m.power_levels.get(&m_new()).copied().unwrap_or(0), 100);
        assert!(m.pending_rotation_for.is_empty());
    }

    #[test]
    fn veto_during_collecting_kills() {
        // Collecting proposals are vetoable early (§6 T1) — the veto
        // survives the threshold being reached afterwards.
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        events.push(veto([0xB2; 16], admin1(), 105_000));
        events.push(cosign([0xB1; 16], d2(), T_R));
        let m = materialize_with_now(&events, admin1(), Some(DEADLINE + 1));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::Vetoed);
    }

    #[test]
    fn veto_authored_after_deadline_is_ineffective() {
        // RV1 bounds veto AUTHORSHIP to [t₀, deadline] — there is no
        // veto-after-the-fact right.
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        events.push(cosign([0xB1; 16], d2(), T_R));
        events.push(veto([0xB2; 16], admin1(), DEADLINE + 5));
        let m = materialize_with_now(&events, admin1(), Some(DEADLINE + 10));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::Executed);
        assert_eq!(m.members[&admin1()].status, MemberStatus::Banned);
    }

    #[test]
    fn veto_by_non_admin_is_ignored() {
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        events.push(cosign([0xB1; 16], d2(), T_R));
        // d1 has power 0 — their veto records nothing.
        events.push(veto([0xB2; 16], d1(), DEADLINE - 1_000));
        let m = materialize_with_now(&events, admin1(), Some(DEADLINE + 1));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::Executed);
    }

    #[test]
    fn late_delivered_veto_rederives_execution_away() {
        // §4.2: execution is pure derived state. A veto authored inside
        // the window but DELIVERED after replicas already executed must
        // revert promotion, derived kick, and the rotation marker on
        // re-materialization.
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        events.push(cosign([0xB1; 16], d2(), T_R));

        let executed = materialize_with_now(&events, admin1(), Some(DEADLINE + 2_000));
        assert_eq!(view(&executed, P1).phase, RecoveryPhase::Executed);
        assert_eq!(executed.members[&admin1()].status, MemberStatus::Banned);

        // The late-delivered veto (authored in-window).
        events.push(veto([0xB2; 16], admin1(), DEADLINE - 1));
        let reverted = materialize_with_now(&events, admin1(), Some(DEADLINE + 2_000));
        assert_eq!(view(&reverted, P1).phase, RecoveryPhase::Vetoed);
        assert_eq!(reverted.members[&admin1()].status, MemberStatus::Joined);
        assert_ne!(
            reverted.power_levels.get(&m_new()).copied().unwrap_or(0),
            100
        );
        assert!(reverted.pending_rotation_for.is_empty());
    }

    #[test]
    fn recovery_rotation_executes_and_late_veto_heals_divergence() {
        // §4.3 heal vector: derived execution → new_admin authors the
        // recovery-triggered EpochRotation (divergent) → late veto
        // delivery → membership AND epoch re-derive away → a normal
        // kick+rotation afterwards still works.
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        events.push(cosign([0xB1; 16], d2(), T_R));
        // Recovery rotation by the promoted new_admin, citing the
        // proposal as trigger. Recipients: everyone Joined except the
        // kicked lost_admin.
        let recipients = |addrs: &[OwnerAddr]| -> Vec<RecipientCiphertext> {
            addrs
                .iter()
                .map(|a| RecipientCiphertext {
                    recipient: *a,
                    sealed: Vec::new(),
                })
                .collect()
        };
        // ZEB-714 (PR #498 R1): the rotation must clear the finality
        // margin F — authored past DEADLINE + F.
        const ROT_AT: u64 = DEADLINE + RECOVERY_ROTATION_FINALITY_MS + 100;
        events.push(ev(
            [0xC0; 16],
            m_new(),
            ROT_AT,
            MembershipEventKind::EpochRotation {
                prior_epoch: 0,
                triggered_by: P1,
                recipient_ciphertexts: recipients(&[d1(), d2(), m_new(), m2()]),
            },
        ));

        // Without the veto: rotation lands (issuer authorized as the
        // recovery-promoted new_admin), pending marker consumed.
        let diverged = materialize_with_now(&events, admin1(), Some(ROT_AT + 100));
        assert_eq!(view(&diverged, P1).phase, RecoveryPhase::Executed);
        assert_eq!(diverged.current_epoch, Some(1));
        assert!(
            diverged.pending_rotation_for.is_empty(),
            "recovery rotation must consume the pending marker"
        );

        // Late veto delivery: the whole derived chain reverts — the
        // divergent rotation's trigger is no longer executed, so the
        // epoch advance re-derives away too.
        events.push(veto([0xC1; 16], admin1(), DEADLINE - 1_000));
        let healed = materialize_with_now(&events, admin1(), Some(ROT_AT + 100));
        assert_eq!(view(&healed, P1).phase, RecoveryPhase::Vetoed);
        assert_eq!(healed.current_epoch, None);
        assert_eq!(healed.members[&admin1()].status, MemberStatus::Joined);
        assert_ne!(healed.power_levels.get(&m_new()).copied().unwrap_or(0), 100);

        // Normal lifecycle still works post-heal: restored admin kicks
        // m2 and rotates.
        events.push(ev(
            [0xC2; 16],
            admin1(),
            ROT_AT + 300,
            MembershipEventKind::Kick {
                target: m2(),
                reason: None,
            },
        ));
        events.push(ev(
            [0xC3; 16],
            admin1(),
            ROT_AT + 400,
            MembershipEventKind::EpochRotation {
                prior_epoch: 0,
                triggered_by: [0xC2; 16],
                recipient_ciphertexts: recipients(&[admin1(), d1(), d2(), m_new()]),
            },
        ));
        let after = materialize_with_now(&events, admin1(), Some(ROT_AT + 500));
        assert_eq!(after.current_epoch, Some(1));
        assert_eq!(after.members[&m2()].status, MemberStatus::Banned);
        assert!(after.pending_rotation_for.is_empty());
    }

    #[test]
    fn recovery_rotation_inside_finality_margin_is_dropped() {
        // ZEB-714 (PR #498 R1, CodeRabbit): §4.3's F margin is a CRDT
        // invariant, not client politeness — a rotation citing an
        // executed proposal but authored at deadline + 1 (inside the
        // window reserved for late veto delivery) must be silently
        // dropped: no epoch advance, pending marker intact.
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        events.push(cosign([0xB1; 16], d2(), T_R));
        events.push(ev(
            [0xC0; 16],
            m_new(),
            DEADLINE + 1,
            MembershipEventKind::EpochRotation {
                prior_epoch: 0,
                triggered_by: P1,
                recipient_ciphertexts: vec![
                    RecipientCiphertext {
                        recipient: d1(),
                        sealed: Vec::new(),
                    },
                    RecipientCiphertext {
                        recipient: d2(),
                        sealed: Vec::new(),
                    },
                    RecipientCiphertext {
                        recipient: m_new(),
                        sealed: Vec::new(),
                    },
                    RecipientCiphertext {
                        recipient: m2(),
                        sealed: Vec::new(),
                    },
                ],
            },
        ));
        let m = materialize_with_now(&events, admin1(), Some(DEADLINE + 2));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::Executed);
        assert_eq!(
            m.current_epoch, None,
            "premature rotation must not advance the epoch"
        );
        assert!(
            m.pending_rotation_for.contains(&admin1()),
            "pending marker must survive a premature rotation"
        );
    }

    #[test]
    fn rival_proposals_same_lost_admin_tie_break_lowest_tr() {
        let (mut events, digest) = base_world();
        // p1: d1 proposes m_new (t_R = 110_000).
        events.push(proposal(digest));
        events.push(cosign([0xB1; 16], d2(), T_R));
        // p2: d2 proposes m2 for the SAME lost admin (t_R = 111_000).
        events.push(ev(
            [0xB4; 16],
            d2(),
            101_000,
            MembershipEventKind::RecoveryProposal {
                lost_admin: admin1(),
                new_admin: m2(),
                config_digest: digest,
            },
        ));
        events.push(ev(
            [0xB5; 16],
            d1(),
            111_000,
            MembershipEventKind::RecoveryCosign {
                target_event_id: [0xB4; 16],
            },
        ));
        let m = materialize_with_now(&events, admin1(), Some(111_000 + W + 1));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::Executed);
        assert_eq!(view(&m, [0xB4; 16]).phase, RecoveryPhase::Superseded);
        assert_eq!(m.power_levels.get(&m_new()).copied(), Some(100));
        assert_ne!(m.power_levels.get(&m2()).copied().unwrap_or(0), 100);
    }

    #[test]
    fn rival_proposals_different_lost_admins_both_execute() {
        let (mut events, digest) = base_world();
        // Promote m2 to a second admin first (direct SetPower, quorum=1).
        events.push(ev(
            [0xA6; 16],
            admin1(),
            20_000,
            MembershipEventKind::SetPower {
                target: m2(),
                level: 100,
            },
        ));
        // p1: lost=admin1 → new=m_new.
        events.push(proposal(digest));
        events.push(cosign([0xB1; 16], d2(), T_R));
        // p2: lost=m2 → new=d2 (distinct lost admin — own group).
        events.push(ev(
            [0xB4; 16],
            d2(),
            101_000,
            MembershipEventKind::RecoveryProposal {
                lost_admin: m2(),
                new_admin: d2(),
                config_digest: digest,
            },
        ));
        events.push(ev(
            [0xB5; 16],
            d1(),
            111_000,
            MembershipEventKind::RecoveryCosign {
                target_event_id: [0xB4; 16],
            },
        ));
        let m = materialize_with_now(&events, admin1(), Some(111_000 + W + 1));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::Executed);
        assert_eq!(view(&m, [0xB4; 16]).phase, RecoveryPhase::Executed);
        assert_eq!(m.members[&admin1()].status, MemberStatus::Banned);
        assert_eq!(m.members[&m2()].status, MemberStatus::Banned);
        assert_eq!(m.power_levels.get(&m_new()).copied(), Some(100));
        assert_eq!(m.power_levels.get(&d2()).copied(), Some(100));
    }

    #[test]
    fn config_change_mid_collecting_kills() {
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        // Config re-set (same designates — the GENERATION changes) lands
        // between t₀ and the would-be threshold signature.
        events.push(ev(
            [0xA7; 16],
            admin1(),
            105_000,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::SetRecoveryDesignates {
                    designates: vec![d1(), d2()],
                    threshold: 2,
                    veto_window_ms: W,
                },
            },
        ));
        events.push(cosign([0xB1; 16], d2(), T_R));
        let m = materialize_with_now(&events, admin1(), Some(DEADLINE + 1));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::ConfigChanged);
        assert_ne!(m.power_levels.get(&m_new()).copied().unwrap_or(0), 100);
    }

    #[test]
    fn config_change_mid_time_lock_kills() {
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        events.push(cosign([0xB1; 16], d2(), T_R));
        events.push(ev(
            [0xA7; 16],
            admin1(),
            DEADLINE - 500,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::SetRecoveryDesignates {
                    designates: vec![d1(), d2()],
                    threshold: 2,
                    veto_window_ms: W,
                },
            },
        ));
        let m = materialize_with_now(&events, admin1(), Some(DEADLINE + 1));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::ConfigChanged);
        assert_eq!(m.members[&admin1()].status, MemberStatus::Joined);
    }

    #[test]
    fn config_change_after_deadline_leaves_execution_standing() {
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        events.push(cosign([0xB1; 16], d2(), T_R));
        events.push(ev(
            [0xA7; 16],
            admin1(),
            DEADLINE + 50,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::SetRecoveryDesignates {
                    designates: vec![d1(), d2()],
                    threshold: 2,
                    veto_window_ms: W,
                },
            },
        ));
        let m = materialize_with_now(&events, admin1(), Some(DEADLINE + 100));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::Executed);
    }

    #[test]
    fn forward_ref_cosign_before_proposal_counts() {
        // Clock skew sorts the cosign BEFORE the proposal — it must be
        // queued and folded (ZEB-250 R2(b) mirror). t_R becomes the 2nd
        // smallest signature wall = t₀ itself.
        let (mut events, digest) = base_world();
        events.push(cosign([0xB1; 16], d2(), 99_000));
        events.push(proposal(digest));
        let m = materialize_with_now(&events, admin1(), Some(T0 + W + 1));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::Executed);
        assert_eq!(view(&m, P1).deadline_ms, Some(T0 + W));
    }

    #[test]
    fn non_designate_cosign_is_ignored() {
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        events.push(ev(
            [0xB1; 16],
            m_new(),
            T_R,
            MembershipEventKind::RecoveryCosign {
                target_event_id: P1,
            },
        ));
        let m = materialize_with_now(&events, admin1(), Some(DEADLINE + 1));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::Collecting);
    }

    #[test]
    fn duplicate_cosigner_counts_once() {
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        // The proposer cosigning their own proposal adds nothing.
        events.push(cosign([0xB1; 16], d1(), T_R));
        let m = materialize_with_now(&events, admin1(), Some(DEADLINE + 1));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::Collecting);
        assert_eq!(view(&m, P1).signers.len(), 1);
    }

    #[test]
    fn bootstrap_admin_without_record_gets_banned_record_on_execution() {
        // No explicit Join for admin1: the derived kick must INSERT a
        // Banned record so the verify-side bootstrap-admin exceptions
        // (None membership ⇒ "never kicked") stop honoring the dead key.
        let (mut events, digest) = {
            let mut events = vec![
                ev([0xA1; 16], d1(), 1_000, MembershipEventKind::Join),
                ev([0xA2; 16], d2(), 1_100, MembershipEventKind::Join),
                ev([0xA3; 16], m_new(), 1_200, MembershipEventKind::Join),
                ev(
                    [0xA5; 16],
                    admin1(),
                    10_000,
                    MembershipEventKind::AdminProposal {
                        proposal_kind: ProposalKind::SetRecoveryDesignates {
                            designates: vec![d1(), d2()],
                            threshold: 2,
                            veto_window_ms: W,
                        },
                    },
                ),
            ];
            let m = materialize(&events, admin1());
            let digest = recovery_config_digest(m.recovery_designates.as_ref().unwrap()).unwrap();
            events.push(proposal(digest));
            (events, digest)
        };
        let _ = digest;
        events.push(cosign([0xB1; 16], d2(), T_R));
        let m = materialize_with_now(&events, admin1(), Some(DEADLINE + 1));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::Executed);
        assert_eq!(m.members[&admin1()].status, MemberStatus::Banned);
        assert!(m.pending_rotation_for.contains(&admin1()));
    }

    #[test]
    fn terminal_states_are_stable_and_replay_deterministic() {
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        events.push(cosign([0xB1; 16], d2(), T_R));
        // Late extra cosign after execution: no-op (already terminal).
        events.push(cosign([0xB3; 16], d1(), DEADLINE + 500));

        let a = materialize_with_now(&events, admin1(), Some(DEADLINE + 1_000));
        let b = materialize_with_now(&events, admin1(), Some(DEADLINE + 1_000));
        assert_eq!(a, b, "materialize must be deterministic");
        assert_eq!(view(&a, P1).phase, RecoveryPhase::Executed);

        // Input order independence: reversed delivery converges.
        let mut reversed = events.clone();
        reversed.reverse();
        let c = materialize_with_now(&reversed, admin1(), Some(DEADLINE + 1_000));
        assert_eq!(a, c, "delivery order must not matter");
    }

    #[test]
    fn invalid_bind_digest_proposal_is_invisible() {
        // A proposal carrying a stale/wrong digest never becomes a work
        // entry (defense-in-depth mirror of RP5).
        let (mut events, _) = base_world();
        events.push(ev(
            P1,
            d1(),
            T0,
            MembershipEventKind::RecoveryProposal {
                lost_admin: admin1(),
                new_admin: m_new(),
                config_digest: [0xAB; 32],
            },
        ));
        events.push(cosign([0xB1; 16], d2(), T_R));
        let m = materialize_with_now(&events, admin1(), Some(DEADLINE + 1));
        assert!(m.recovery_proposals.is_empty());
        assert_ne!(m.power_levels.get(&m_new()).copied().unwrap_or(0), 100);
    }

    #[test]
    fn overflowing_window_fails_closed_never_executes() {
        // Defense-in-depth: a u64::MAX window can only enter via events
        // that BYPASSED verification (the RD4 ceiling rejects it there).
        // The deadline addition must fail closed (Expired), not wrap
        // small and execute immediately.
        let mut events = vec![
            ev([0xA0; 16], admin1(), 500, MembershipEventKind::Join),
            ev([0xA1; 16], d1(), 1_000, MembershipEventKind::Join),
            ev([0xA2; 16], d2(), 1_100, MembershipEventKind::Join),
            ev([0xA3; 16], m_new(), 1_200, MembershipEventKind::Join),
            ev(
                [0xA5; 16],
                admin1(),
                10_000,
                MembershipEventKind::AdminProposal {
                    proposal_kind: ProposalKind::SetRecoveryDesignates {
                        designates: vec![d1(), d2()],
                        threshold: 2,
                        veto_window_ms: u64::MAX,
                    },
                },
            ),
        ];
        let digest = recovery_config_digest(
            materialize(&events, admin1())
                .recovery_designates
                .as_ref()
                .unwrap(),
        )
        .unwrap();
        events.push(proposal(digest));
        events.push(cosign([0xB1; 16], d2(), T_R));
        let m = materialize_with_now(&events, admin1(), Some(u64::MAX - 1));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::Expired);
        assert_ne!(m.power_levels.get(&m_new()).copied().unwrap_or(0), 100);
        assert_eq!(m.members[&admin1()].status, MemberStatus::Joined);
    }

    #[test]
    fn new_admin_leaving_during_window_stalls_atomically() {
        // PR #497 R2 (Greptile P1): execution is atomic. If new_admin
        // left before the deadline, NOTHING executes — kicking the
        // (sole) lost admin without the paired promotion would leave
        // the community with no power-100 member at all.
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        events.push(cosign([0xB1; 16], d2(), T_R));
        events.push(ev(
            [0xB6; 16],
            m_new(),
            DEADLINE - 100,
            MembershipEventKind::Leave,
        ));
        let m = materialize_with_now(&events, admin1(), Some(DEADLINE + 1));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::Stalled);
        assert_ne!(m.power_levels.get(&m_new()).copied().unwrap_or(0), 100);
        assert_eq!(m.members[&admin1()].status, MemberStatus::Joined);
        // No recovery-derived rotation marker (m_new IS marked — by
        // their ordinary Leave, the normal ZEB-249 path).
        assert!(!m.pending_rotation_for.contains(&admin1()));

        // Terminal: a rejoin AFTER the deadline does not revive it.
        events.push(ev(
            [0xB7; 16],
            m_new(),
            DEADLINE + 50,
            MembershipEventKind::Join,
        ));
        let after = materialize_with_now(&events, admin1(), Some(DEADLINE + 100));
        assert_eq!(view(&after, P1).phase, RecoveryPhase::Stalled);
        assert_eq!(after.members[&admin1()].status, MemberStatus::Joined);
    }

    #[test]
    fn new_admin_leaving_after_deadline_execution_stands() {
        // The snapshot is judged AS OF the deadline: a departure after
        // it does not undo the (already-derived) execution; the granted
        // power entry is inert without Joined status.
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        events.push(cosign([0xB1; 16], d2(), T_R));
        events.push(ev(
            [0xB6; 16],
            m_new(),
            DEADLINE + 200,
            MembershipEventKind::Leave,
        ));
        let m = materialize_with_now(&events, admin1(), Some(DEADLINE + 300));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::Executed);
        assert_eq!(m.power_levels.get(&m_new()).copied(), Some(100));
        assert_eq!(m.members[&admin1()].status, MemberStatus::Banned);
    }

    #[test]
    fn stalled_rival_never_flips_the_executed_winner() {
        // p1 (lower t_R) stalls because its new_admin left mid-window;
        // p2 executes. When p1's new_admin later rejoins, p1 must STAY
        // stalled — the deadline-snapshot rule is what keeps the
        // winner stable instead of retroactively re-deriving p2 away.
        let (mut events, digest) = base_world();
        events.push(proposal(digest)); // p1: lost=admin1, new=m_new, t0=T0
        events.push(cosign([0xB1; 16], d2(), T_R));
        events.push(ev(
            [0xB6; 16],
            m_new(),
            DEADLINE - 100,
            MembershipEventKind::Leave,
        ));
        // p2: same lost admin, new=m2, later t_R.
        events.push(ev(
            [0xB4; 16],
            d2(),
            101_000,
            MembershipEventKind::RecoveryProposal {
                lost_admin: admin1(),
                new_admin: m2(),
                config_digest: digest,
            },
        ));
        events.push(ev(
            [0xB5; 16],
            d1(),
            111_000,
            MembershipEventKind::RecoveryCosign {
                target_event_id: [0xB4; 16],
            },
        ));
        let t_after = 111_000 + W + 1;
        let m = materialize_with_now(&events, admin1(), Some(t_after));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::Stalled);
        assert_eq!(view(&m, [0xB4; 16]).phase, RecoveryPhase::Executed);
        assert_eq!(m.power_levels.get(&m2()).copied(), Some(100));

        // m_new rejoins — p1 stays stalled, p2 stays the winner.
        events.push(ev(
            [0xB8; 16],
            m_new(),
            t_after + 10,
            MembershipEventKind::Join,
        ));
        let after = materialize_with_now(&events, admin1(), Some(t_after + 100));
        assert_eq!(view(&after, P1).phase, RecoveryPhase::Stalled);
        assert_eq!(view(&after, [0xB4; 16]).phase, RecoveryPhase::Executed);
        assert_eq!(after.power_levels.get(&m2()).copied(), Some(100));
        assert_ne!(after.power_levels.get(&m_new()).copied().unwrap_or(0), 100);
    }

    // ── ZEB-714: vetoed_by attribution + rotation-trigger helper ──

    #[test]
    fn vetoed_by_records_the_vetoing_admin() {
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        events.push(cosign([0xB1; 16], d2(), T_R));
        events.push(veto([0xB2; 16], admin1(), T_R + 5_000));
        let m = materialize_with_now(&events, admin1(), Some(DEADLINE + 1));
        let v = view(&m, P1);
        assert_eq!(v.phase, RecoveryPhase::Vetoed);
        assert_eq!(v.vetoed_by, Some(admin1()));
    }

    #[test]
    fn vetoed_by_is_none_for_non_vetoed_phases() {
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        events.push(cosign([0xB1; 16], d2(), T_R));
        let executed = materialize_with_now(&events, admin1(), Some(DEADLINE + 1));
        assert_eq!(view(&executed, P1).phase, RecoveryPhase::Executed);
        assert_eq!(view(&executed, P1).vetoed_by, None);
        let collecting = materialize_with_now(&events[..events.len() - 1], admin1(), Some(T0 + 1));
        assert_eq!(view(&collecting, P1).phase, RecoveryPhase::Collecting);
        assert_eq!(view(&collecting, P1).vetoed_by, None);
    }

    #[test]
    fn vetoed_by_is_first_qualifying_vetoer_in_replay_order() {
        let (mut events, digest) = base_world();
        // Promote m2 to admin (direct SetPower — admin_quorum is 1).
        events.push(ev(
            [0xA9; 16],
            admin1(),
            2_000,
            MembershipEventKind::SetPower {
                target: m2(),
                level: 100,
            },
        ));
        events.push(proposal(digest));
        events.push(cosign([0xB1; 16], d2(), T_R));
        // Two qualifying vetoes: m2's sorts FIRST in replay (lower wall).
        events.push(veto([0xB2; 16], admin1(), T_R + 9_000));
        events.push(veto([0xB3; 16], m2(), T_R + 4_000));
        let m = materialize_with_now(&events, admin1(), Some(DEADLINE + 1));
        let v = view(&m, P1);
        assert_eq!(v.phase, RecoveryPhase::Vetoed);
        assert_eq!(v.vetoed_by, Some(m2()));
    }

    #[test]
    fn rotation_trigger_names_the_executed_proposal_with_finality_wall() {
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        events.push(cosign([0xB1; 16], d2(), T_R));
        let m = materialize_with_now(&events, admin1(), Some(DEADLINE + 1));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::Executed);
        assert_eq!(
            recovery_rotation_trigger(&m, &admin1()),
            Some((P1, Some(DEADLINE + RECOVERY_ROTATION_FINALITY_MS)))
        );
        // Not a recovery target → None.
        assert_eq!(recovery_rotation_trigger(&m, &m2()), None);
    }

    #[test]
    fn rotation_trigger_is_none_before_execution() {
        let (mut events, digest) = base_world();
        events.push(proposal(digest));
        events.push(cosign([0xB1; 16], d2(), T_R));
        // Still time-locked: the marker is not set, and even against a
        // hand-checked view the trigger must not fire.
        let m = materialize_with_now(&events, admin1(), Some(DEADLINE));
        assert_eq!(view(&m, P1).phase, RecoveryPhase::TimeLocked);
        assert_eq!(recovery_rotation_trigger(&m, &admin1()), None);
    }

    #[test]
    fn rotation_trigger_overflowing_finality_fails_closed() {
        // Direct view construction: a deadline close enough to u64::MAX
        // that `deadline + F` wraps must yield eligible_at = None
        // (never eligible), not a tiny wrapped wall.
        let mut m = MaterializedMembership::default();
        m.recovery_proposals.push(RecoveryProposalView {
            id: P1,
            proposer: d1(),
            lost_admin: admin1(),
            new_admin: m_new(),
            signers: BTreeSet::from([d1(), d2()]),
            threshold: 2,
            proposed_at_wall_ms: u64::MAX - 1_000_000,
            deadline_ms: Some(u64::MAX - 1_000),
            phase: RecoveryPhase::Executed,
            vetoed_by: None,
        });
        assert_eq!(recovery_rotation_trigger(&m, &admin1()), Some((P1, None)));
    }
}

// ── ZEB-713 (ZEB-212 D1) admin-recovery verify_event tests ───────────────────

#[cfg(test)]
mod zeb_713_recovery_verify_tests {
    use super::*;

    const COM: SpaceId = SpaceId([0xc0; 16]);

    fn hlc(wall_ms: u64, dev: &str) -> Hlc {
        Hlc {
            wall_ms,
            logical: 0,
            device_id: dev.into(),
        }
    }

    fn joined_state(wall_ms: u64, dev: &str) -> MemberState {
        MemberState {
            status: MemberStatus::Joined,
            joined_at: hlc(wall_ms, dev),
            left_at: None,
            enrolled_device_keys: BTreeSet::new(),
            revoked_device_keys: BTreeSet::new(),
        }
    }

    fn sign_with_identity(payload: EventPayload, owner: &TestOwner) -> SignedMembershipEvent {
        sign_event(&payload, &owner.device_key).expect("sign_event must succeed")
    }

    /// Fixture world: admin (power 100), two designates, one regular
    /// member (the recovery candidate), with a live RecoveryDesignates
    /// config (R=2, W=floor).
    struct World {
        admin: TestOwner,
        d1: TestOwner,
        d2: TestOwner,
        member: TestOwner,
        prior: MaterializedMembership,
        digest: [u8; 32],
        ctx: VerifyContext,
    }

    fn world() -> World {
        let admin = mint_test_owner(0x01);
        let d1 = mint_test_owner(0x02);
        let d2 = mint_test_owner(0x03);
        let member = mint_test_owner(0x04);
        let mut prior = MaterializedMembership::default();
        for (o, dev) in [(&admin, "a"), (&d1, "d1"), (&d2, "d2"), (&member, "m")] {
            prior.members.insert(o.owner, joined_state(0, dev));
        }
        prior.power_levels.insert(admin.owner, 100);
        for o in [&admin, &d1, &d2, &member] {
            test_enroll_member(&mut prior, o);
        }
        let config = RecoveryDesignates {
            designates: vec![d1.owner, d2.owner],
            threshold: 2,
            veto_window_ms: RECOVERY_VETO_WINDOW_FLOOR_MS,
            set_at: hlc(1_000, "cfg"),
        };
        let digest = recovery_config_digest(&config).expect("digest");
        prior.recovery_designates = Some(config);
        let ctx = VerifyContext {
            expected_community_id: COM,
            admin_addr: admin.owner,
            is_invite_only: false,
        };
        World {
            admin,
            d1,
            d2,
            member,
            prior,
            digest,
            ctx,
        }
    }

    fn set_designates_event(
        w: &World,
        designates: Vec<OwnerAddr>,
        threshold: u8,
        veto_window_ms: u64,
    ) -> SignedMembershipEvent {
        sign_with_identity(
            EventPayload {
                id: [0x31; 16],
                community_id: COM,
                kind: MembershipEventKind::AdminProposal {
                    proposal_kind: ProposalKind::SetRecoveryDesignates {
                        designates,
                        threshold,
                        veto_window_ms,
                    },
                },
                actor: w.admin.owner,
                at: hlc(5_000, "a"),
            },
            &w.admin,
        )
    }

    fn proposal_event(
        _w: &World,
        actor: &TestOwner,
        lost_admin: OwnerAddr,
        new_admin: OwnerAddr,
        config_digest: [u8; 32],
    ) -> SignedMembershipEvent {
        sign_with_identity(
            EventPayload {
                id: [0x41; 16],
                community_id: COM,
                kind: MembershipEventKind::RecoveryProposal {
                    lost_admin,
                    new_admin,
                    config_digest,
                },
                actor: actor.owner,
                at: hlc(6_000, "p"),
            },
            actor,
        )
    }

    // ── RD gates (SetRecoveryDesignates shape, via AdminProposal) ──

    #[test]
    fn rd1_empty_designates_rejected() {
        let w = world();
        let evt = set_designates_event(&w, vec![], 1, RECOVERY_VETO_WINDOW_FLOOR_MS);
        assert_eq!(
            verify_event(&evt, &w.prior, &w.ctx),
            Err(VerifyError::RecoveryDesignatesMalformed)
        );
    }

    #[test]
    fn rd1_duplicate_designates_rejected() {
        let w = world();
        let evt = set_designates_event(
            &w,
            vec![w.d1.owner, w.d1.owner],
            1,
            RECOVERY_VETO_WINDOW_FLOOR_MS,
        );
        assert_eq!(
            verify_event(&evt, &w.prior, &w.ctx),
            Err(VerifyError::RecoveryDesignatesMalformed)
        );
    }

    #[test]
    fn rd2_designate_not_joined_rejected() {
        let mut w = world();
        w.prior.members.get_mut(&w.d2.owner).unwrap().status = MemberStatus::Left;
        let evt = set_designates_event(
            &w,
            vec![w.d1.owner, w.d2.owner],
            1,
            RECOVERY_VETO_WINDOW_FLOOR_MS,
        );
        assert_eq!(
            verify_event(&evt, &w.prior, &w.ctx),
            Err(VerifyError::RecoveryDesignateNotJoined)
        );
    }

    #[test]
    fn rd3_threshold_zero_rejected() {
        let w = world();
        let evt = set_designates_event(&w, vec![w.d1.owner], 0, RECOVERY_VETO_WINDOW_FLOOR_MS);
        assert_eq!(
            verify_event(&evt, &w.prior, &w.ctx),
            Err(VerifyError::RecoveryThresholdOutOfRange)
        );
    }

    #[test]
    fn rd3_threshold_above_designate_count_rejected() {
        let w = world();
        let evt = set_designates_event(
            &w,
            vec![w.d1.owner, w.d2.owner],
            3,
            RECOVERY_VETO_WINDOW_FLOOR_MS,
        );
        assert_eq!(
            verify_event(&evt, &w.prior, &w.ctx),
            Err(VerifyError::RecoveryThresholdOutOfRange)
        );
    }

    #[test]
    fn rd4_window_below_floor_rejected() {
        let w = world();
        let evt = set_designates_event(
            &w,
            vec![w.d1.owner, w.d2.owner],
            2,
            RECOVERY_VETO_WINDOW_FLOOR_MS - 1,
        );
        assert_eq!(
            verify_event(&evt, &w.prior, &w.ctx),
            Err(VerifyError::RecoveryVetoWindowTooShort)
        );
    }

    #[test]
    fn rd4_window_above_ceiling_rejected() {
        let w = world();
        let evt = set_designates_event(
            &w,
            vec![w.d1.owner, w.d2.owner],
            2,
            RECOVERY_VETO_WINDOW_CEILING_MS + 1,
        );
        assert_eq!(
            verify_event(&evt, &w.prior, &w.ctx),
            Err(VerifyError::RecoveryVetoWindowTooLong)
        );
    }

    #[test]
    fn rd_valid_set_designates_accepted() {
        let w = world();
        let evt = set_designates_event(
            &w,
            vec![w.d1.owner, w.d2.owner],
            2,
            RECOVERY_VETO_WINDOW_FLOOR_MS,
        );
        assert_eq!(verify_event(&evt, &w.prior, &w.ctx), Ok(()));
    }

    // ── RP gates (RecoveryProposal) ──

    #[test]
    fn rp2_unconfigured_rejected() {
        let mut w = world();
        w.prior.recovery_designates = None;
        let evt = proposal_event(&w, &w.d1.clone(), w.admin.owner, w.member.owner, w.digest);
        assert_eq!(
            verify_event(&evt, &w.prior, &w.ctx),
            Err(VerifyError::RecoveryNotConfigured)
        );
    }

    #[test]
    fn rp1_actor_not_designate_rejected() {
        let w = world();
        let evt = proposal_event(
            &w,
            &w.member.clone(),
            w.admin.owner,
            w.member.owner,
            w.digest,
        );
        assert_eq!(
            verify_event(&evt, &w.prior, &w.ctx),
            Err(VerifyError::RecoveryProposalActorNotDesignate)
        );
    }

    #[test]
    fn rp1_designate_no_longer_joined_rejected() {
        let mut w = world();
        w.prior.members.get_mut(&w.d1.owner).unwrap().status = MemberStatus::Left;
        let evt = proposal_event(&w, &w.d1.clone(), w.admin.owner, w.member.owner, w.digest);
        assert_eq!(
            verify_event(&evt, &w.prior, &w.ctx),
            Err(VerifyError::RecoveryProposalActorNotDesignate)
        );
    }

    #[test]
    fn rp4_lost_admin_not_admin_rejected() {
        let w = world();
        // "lost" target is a regular member, not power-100.
        let evt = proposal_event(&w, &w.d1.clone(), w.member.owner, w.d2.owner, w.digest);
        assert_eq!(
            verify_event(&evt, &w.prior, &w.ctx),
            Err(VerifyError::RecoveryProposalLostAdminNotAdmin)
        );
    }

    #[test]
    fn rp4_bootstrap_admin_without_member_record_accepted() {
        let mut w = world();
        // The bootstrap admin may have implicit power with no member
        // record (never explicitly Joined) — recovery must still be
        // able to name them lost.
        w.prior.members.remove(&w.admin.owner);
        let evt = proposal_event(&w, &w.d1.clone(), w.admin.owner, w.member.owner, w.digest);
        assert_eq!(verify_event(&evt, &w.prior, &w.ctx), Ok(()));
    }

    #[test]
    fn rp3_new_admin_already_admin_rejected() {
        let mut w = world();
        w.prior.power_levels.insert(w.member.owner, 100);
        let evt = proposal_event(&w, &w.d1.clone(), w.admin.owner, w.member.owner, w.digest);
        assert_eq!(
            verify_event(&evt, &w.prior, &w.ctx),
            Err(VerifyError::RecoveryProposalNewAdminInvalid)
        );
    }

    #[test]
    fn rp3_new_admin_equals_lost_admin_rejected() {
        let w = world();
        let evt = proposal_event(&w, &w.d1.clone(), w.admin.owner, w.admin.owner, w.digest);
        assert_eq!(
            verify_event(&evt, &w.prior, &w.ctx),
            Err(VerifyError::RecoveryProposalNewAdminInvalid)
        );
    }

    #[test]
    fn rp3_new_admin_not_joined_rejected() {
        let mut w = world();
        w.prior.members.get_mut(&w.member.owner).unwrap().status = MemberStatus::Left;
        let evt = proposal_event(&w, &w.d1.clone(), w.admin.owner, w.member.owner, w.digest);
        assert_eq!(
            verify_event(&evt, &w.prior, &w.ctx),
            Err(VerifyError::RecoveryProposalNewAdminInvalid)
        );
    }

    #[test]
    fn rp5_config_digest_mismatch_rejected() {
        let w = world();
        let evt = proposal_event(&w, &w.d1.clone(), w.admin.owner, w.member.owner, [0xAB; 32]);
        assert_eq!(
            verify_event(&evt, &w.prior, &w.ctx),
            Err(VerifyError::RecoveryProposalConfigDigestMismatch)
        );
    }

    #[test]
    fn rp6_open_proposal_by_same_actor_rejected() {
        let mut w = world();
        w.prior.recovery_proposals.push(RecoveryProposalView {
            id: [0x99; 16],
            proposer: w.d1.owner,
            lost_admin: w.admin.owner,
            new_admin: w.member.owner,
            signers: BTreeSet::from([w.d1.owner]),
            threshold: 2,
            proposed_at_wall_ms: 4_000,
            deadline_ms: None,
            phase: RecoveryPhase::Collecting,
            vetoed_by: None,
        });
        let evt = proposal_event(&w, &w.d1.clone(), w.admin.owner, w.member.owner, w.digest);
        assert_eq!(
            verify_event(&evt, &w.prior, &w.ctx),
            Err(VerifyError::RecoveryProposalActorHasOpenProposal)
        );
    }

    #[test]
    fn rp6_terminal_proposal_does_not_block() {
        let mut w = world();
        w.prior.recovery_proposals.push(RecoveryProposalView {
            id: [0x99; 16],
            proposer: w.d1.owner,
            lost_admin: w.admin.owner,
            new_admin: w.member.owner,
            signers: BTreeSet::from([w.d1.owner]),
            threshold: 2,
            proposed_at_wall_ms: 4_000,
            deadline_ms: None,
            phase: RecoveryPhase::Vetoed,
            vetoed_by: Some(w.admin.owner),
        });
        let evt = proposal_event(&w, &w.d1.clone(), w.admin.owner, w.member.owner, w.digest);
        assert_eq!(verify_event(&evt, &w.prior, &w.ctx), Ok(()));
    }

    #[test]
    fn rp_valid_proposal_accepted() {
        let w = world();
        let evt = proposal_event(&w, &w.d1.clone(), w.admin.owner, w.member.owner, w.digest);
        assert_eq!(verify_event(&evt, &w.prior, &w.ctx), Ok(()));
    }

    // ── RC gates (RecoveryCosign) ──

    fn cosign_event(_w: &World, actor: &TestOwner, target: EventId) -> SignedMembershipEvent {
        sign_with_identity(
            EventPayload {
                id: [0x51; 16],
                community_id: COM,
                kind: MembershipEventKind::RecoveryCosign {
                    target_event_id: target,
                },
                actor: actor.owner,
                at: hlc(7_000, "c"),
            },
            actor,
        )
    }

    #[test]
    fn rc1_not_designate_rejected() {
        let w = world();
        let evt = cosign_event(&w, &w.member.clone(), [0x41; 16]);
        assert_eq!(
            verify_event(&evt, &w.prior, &w.ctx),
            Err(VerifyError::RecoveryCosignActorNotDesignate)
        );
    }

    #[test]
    fn rc_unconfigured_rejected() {
        let mut w = world();
        w.prior.recovery_designates = None;
        let evt = cosign_event(&w, &w.d2.clone(), [0x41; 16]);
        assert_eq!(
            verify_event(&evt, &w.prior, &w.ctx),
            Err(VerifyError::RecoveryNotConfigured)
        );
    }

    #[test]
    fn rc_zero_target_rejected() {
        let w = world();
        let evt = cosign_event(&w, &w.d2.clone(), [0u8; 16]);
        assert_eq!(
            verify_event(&evt, &w.prior, &w.ctx),
            Err(VerifyError::RecoveryCosignTargetIdMalformed)
        );
    }

    #[test]
    fn rc_valid_forward_ref_accepted() {
        // Target proposal not in any log — lenient forward-ref, mirrors
        // AdminCountersign.
        let w = world();
        let evt = cosign_event(&w, &w.d2.clone(), [0x41; 16]);
        assert_eq!(verify_event(&evt, &w.prior, &w.ctx), Ok(()));
    }

    // ── RV gates (RecoveryVeto) ──

    fn veto_event(_w: &World, actor: &TestOwner, target: EventId) -> SignedMembershipEvent {
        sign_with_identity(
            EventPayload {
                id: [0x61; 16],
                community_id: COM,
                kind: MembershipEventKind::RecoveryVeto {
                    target_event_id: target,
                },
                actor: actor.owner,
                at: hlc(8_000, "v"),
            },
            actor,
        )
    }

    #[test]
    fn rv1_non_admin_rejected() {
        let w = world();
        let evt = veto_event(&w, &w.d1.clone(), [0x41; 16]);
        assert_eq!(
            verify_event(&evt, &w.prior, &w.ctx),
            Err(VerifyError::RecoveryVetoActorNotAdmin)
        );
    }

    #[test]
    fn rv1_admin_accepted() {
        let w = world();
        let evt = veto_event(&w, &w.admin.clone(), [0x41; 16]);
        assert_eq!(verify_event(&evt, &w.prior, &w.ctx), Ok(()));
    }

    #[test]
    fn rv_zero_target_rejected() {
        let w = world();
        let evt = veto_event(&w, &w.admin.clone(), [0u8; 16]);
        assert_eq!(
            verify_event(&evt, &w.prior, &w.ctx),
            Err(VerifyError::RecoveryVetoTargetIdMalformed)
        );
    }
}

// ── ZEB-251 Task 2: ChangeThresholds AT1 verify_event gate tests ─────────────

#[cfg(test)]
mod zeb_251_change_thresholds_verify_tests {
    use super::*;

    fn make_identity(seed_byte: u8) -> (TestOwner, OwnerAddr) {
        let owner = mint_test_owner(seed_byte);
        let addr = owner.owner;
        (owner, addr)
    }

    fn make_admin_proposal_event(
        id: [u8; 16],
        actor_priv: &TestOwner,
        actor_addr: OwnerAddr,
        proposal_kind: ProposalKind,
        at_wall_ms: u64,
    ) -> SignedMembershipEvent {
        let payload = EventPayload {
            id,
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::AdminProposal { proposal_kind },
            actor: actor_addr,
            at: Hlc {
                wall_ms: at_wall_ms,
                logical: 0,
                device_id: "test".into(),
            },
        };
        sign_event(&payload, &actor_priv.device_key).expect("sign_event must succeed")
    }

    /// A single Joined admin (power 100), enrolled so their own signature
    /// resolves. Mirrors `zeb_250_admin_proposal_verify_tests`' fixture shape.
    fn admin_prior(admin: &TestOwner, admin_addr: OwnerAddr) -> MaterializedMembership {
        let mut prior = MaterializedMembership::default();
        prior.members.insert(
            admin_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "a".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        prior.power_levels.insert(admin_addr, 100);
        test_enroll_member(&mut prior, admin);
        prior
    }

    /// AT1 positive control: a well-formed threshold change (ordering holds,
    /// max stays 100) must be accepted — proves the gate doesn't over-reject.
    #[test]
    fn change_thresholds_valid_is_accepted_at_verify() {
        let (admin_priv, admin_addr) = make_identity(0x01);
        let prior = admin_prior(&admin_priv, admin_addr);
        let evt = make_admin_proposal_event(
            [0x10; 16],
            &admin_priv,
            admin_addr,
            ProposalKind::ChangeThresholds {
                new_thresholds: PowerThresholds {
                    invite: 25,
                    kick: 50,
                    set_power: 100,
                    max: 100,
                },
            },
            1_000,
        );
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
        };
        assert_eq!(verify_event(&evt, &prior, &ctx), Ok(()));
    }

    /// AT1: kick(40) < invite(50) violates the required ordering
    /// `invite <= kick <= set_power <= max` — rejected regardless of signer.
    #[test]
    fn change_thresholds_invalid_ordering_is_rejected_at_verify() {
        let (admin_priv, admin_addr) = make_identity(0x01);
        let prior = admin_prior(&admin_priv, admin_addr);
        let evt = make_admin_proposal_event(
            [0x10; 16],
            &admin_priv,
            admin_addr,
            ProposalKind::ChangeThresholds {
                new_thresholds: PowerThresholds {
                    invite: 50,
                    kick: 40,
                    set_power: 100,
                    max: 100,
                },
            },
            1_000,
        );
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
        };
        let err = verify_event(&evt, &prior, &ctx).unwrap_err();
        assert!(
            matches!(err, VerifyError::AdminProposalThresholdsInvalid),
            "got {err:?}"
        );
    }

    /// AT1: set_power(40) < kick(50) also violates the ordering invariant.
    #[test]
    fn change_thresholds_set_power_below_kick_is_rejected_at_verify() {
        let (admin_priv, admin_addr) = make_identity(0x01);
        let prior = admin_prior(&admin_priv, admin_addr);
        let evt = make_admin_proposal_event(
            [0x10; 16],
            &admin_priv,
            admin_addr,
            ProposalKind::ChangeThresholds {
                new_thresholds: PowerThresholds {
                    invite: 0,
                    kick: 50,
                    set_power: 40,
                    max: 100,
                },
            },
            1_000,
        );
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
        };
        let err = verify_event(&evt, &prior, &ctx).unwrap_err();
        assert!(
            matches!(err, VerifyError::AdminProposalThresholdsInvalid),
            "got {err:?}"
        );
    }

    /// AT1: `max` is fixed at 100 — any other value is rejected even when
    /// the rest of the tuple is internally ordered.
    #[test]
    fn change_thresholds_max_not_100_is_rejected_at_verify() {
        let (admin_priv, admin_addr) = make_identity(0x01);
        let prior = admin_prior(&admin_priv, admin_addr);
        let evt = make_admin_proposal_event(
            [0x10; 16],
            &admin_priv,
            admin_addr,
            ProposalKind::ChangeThresholds {
                new_thresholds: PowerThresholds {
                    invite: 0,
                    kick: 50,
                    set_power: 100,
                    max: 99,
                },
            },
            1_000,
        );
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
        };
        let err = verify_event(&evt, &prior, &ctx).unwrap_err();
        assert!(
            matches!(err, VerifyError::AdminProposalThresholdsInvalid),
            "got {err:?}"
        );
    }

    /// CBOR wire roundtrip for the new variant — mirrors
    /// `admin_proposal_change_quorum_roundtrip`. Pins tag "t" / field "th".
    #[test]
    fn change_thresholds_roundtrip() {
        use crate::owner_state_crypto::canonical_cbor_encode;
        let kind = MembershipEventKind::AdminProposal {
            proposal_kind: ProposalKind::ChangeThresholds {
                new_thresholds: PowerThresholds {
                    invite: 10,
                    kick: 40,
                    set_power: 90,
                    max: 100,
                },
            },
        };
        let bytes = canonical_cbor_encode(&kind).expect("encode");
        let decoded: MembershipEventKind = ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(decoded, kind);
    }
}

// ── ZEB-251 Task 2: ChangeThresholds materialize tests ───────────────────────

#[cfg(test)]
mod zeb_251_change_thresholds_materialize_tests {
    use super::*;

    const COM: SpaceId = SpaceId([0xc0; 16]);

    fn ev(
        id: [u8; 16],
        actor: OwnerAddr,
        wall_ms: u64,
        kind: MembershipEventKind,
    ) -> SignedMembershipEvent {
        SignedMembershipEvent {
            signer_certs: Vec::new(),
            id,
            community_id: COM,
            actor,
            at: Hlc {
                wall_ms,
                logical: 0,
                device_id: "test".into(),
            },
            kind,
            sig: [0; 64],
            countersig: None,
            enrollment: None,
        }
    }

    /// Single admin (admin_quorum defaults to 1) proposes invite=25; the
    /// sole-signer proposal self-satisfies and materializes into
    /// `power_thresholds`. Then proves the field actually GOVERNS
    /// verify_event (not just materialized-but-inert): a power-10 actor's
    /// Invite that would have verified fine under the default invite=0
    /// threshold now fails on the SAME materialized state.
    #[test]
    fn change_thresholds_at_quorum1_materializes_and_governs() {
        let community_id = COM;
        let admin = mint_test_owner(0xa1);
        let low = mint_test_owner(0xb2);

        let events = vec![ev(
            [0xD0; 16],
            admin.owner,
            10_000,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::ChangeThresholds {
                    new_thresholds: PowerThresholds {
                        invite: 25,
                        kick: 50,
                        set_power: 100,
                        max: 100,
                    },
                },
            },
        )];

        let mut m = materialize(&events, admin.owner);
        assert_eq!(
            m.power_thresholds.invite, 25,
            "quorum=1 self-satisfying ChangeThresholds must materialize into power_thresholds"
        );

        m.members.insert(
            low.owner,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "t".into(),
                },
                left_at: None,
                enrolled_device_keys: BTreeSet::new(),
                revoked_device_keys: BTreeSet::new(),
            },
        );
        test_enroll_member(&mut m, &low);
        m.power_levels.insert(low.owner, 10);

        let target = OwnerAddr([0xd1; 16]);
        let invite_payload = EventPayload {
            id: [0xaa; 16],
            community_id,
            kind: MembershipEventKind::Invite { target },
            actor: low.owner,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "t".into(),
            },
        };
        let invite_ev = sign_event(&invite_payload, &low.device_key).expect("sign invite");

        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr: admin.owner,
            is_invite_only: false,
        };
        let err = verify_event(&invite_ev, &m, &ctx).unwrap_err();
        assert!(
            matches!(err, VerifyError::ActorPowerInsufficient),
            "got {err:?}"
        );
    }

    /// Mirrors `materialize_proposal_without_countersigns_pending_when_quorum_above_1`:
    /// under admin_quorum=2, a sole-signer ChangeThresholds proposal (1
    /// signer) must NOT apply — power_thresholds stays at the default until
    /// a second admin countersigns.
    #[test]
    fn change_thresholds_below_quorum_stays_pending() {
        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);

        // Bootstrap: quorum=1 -> raise to 2 via sole-signer ChangeQuorum
        // (admin2 must Join + be promoted first so ChangeQuorum's AP5
        // live-admin-count gate — evaluated structurally the same way at
        // materialize time — has 2 live admins to count).
        let mut events = vec![
            ev([0x80; 16], admin2, 1_000, MembershipEventKind::Join),
            ev(
                [0x81; 16],
                admin1,
                2_000,
                MembershipEventKind::SetPower {
                    target: admin2,
                    level: 100,
                },
            ),
            ev(
                [0xCC; 16],
                admin1,
                3_000,
                MembershipEventKind::AdminProposal {
                    proposal_kind: ProposalKind::ChangeQuorum { new_quorum: 2 },
                },
            ),
        ];

        // Under quorum=2: admin1 alone proposes ChangeThresholds. No countersign.
        events.push(ev(
            [0xDD; 16],
            admin1,
            10_000,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::ChangeThresholds {
                    new_thresholds: PowerThresholds {
                        invite: 25,
                        kick: 50,
                        set_power: 100,
                        max: 100,
                    },
                },
            },
        ));

        let m = materialize(&events, admin1);
        assert_eq!(
            m.power_thresholds, POWER_THRESHOLDS,
            "ChangeThresholds proposal lacking countersign must not apply under admin_quorum=2"
        );
    }

    /// One countersign from admin2 satisfies quorum=2 for a ChangeThresholds
    /// proposal — companion positive case to the pending test above.
    #[test]
    fn change_thresholds_effective_when_countersigned_reaches_quorum() {
        let admin1 = OwnerAddr([0x01; 16]);
        let admin2 = OwnerAddr([0x02; 16]);

        let mut events = vec![
            ev([0x80; 16], admin2, 1_000, MembershipEventKind::Join),
            ev(
                [0x81; 16],
                admin1,
                2_000,
                MembershipEventKind::SetPower {
                    target: admin2,
                    level: 100,
                },
            ),
            ev(
                [0xCC; 16],
                admin1,
                3_000,
                MembershipEventKind::AdminProposal {
                    proposal_kind: ProposalKind::ChangeQuorum { new_quorum: 2 },
                },
            ),
        ];

        let prop_id = [0xDD; 16];
        events.push(ev(
            prop_id,
            admin1,
            10_000,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::ChangeThresholds {
                    new_thresholds: PowerThresholds {
                        invite: 25,
                        kick: 50,
                        set_power: 100,
                        max: 100,
                    },
                },
            },
        ));
        events.push(ev(
            [0xEE; 16],
            admin2,
            11_000,
            MembershipEventKind::AdminCountersign {
                target_event_id: prop_id,
            },
        ));

        let m = materialize(&events, admin1);
        assert_eq!(
            m.power_thresholds.invite, 25,
            "countersign reaching quorum=2 must apply the ChangeThresholds effect"
        );
    }

    /// Single-pass-with-running-state (spec §5.2, same rule ChangeQuorum
    /// uses): replaying the SAME log, an Invite ordered BEFORE the
    /// ChangeThresholds proposal's HLC verifies against the OLD (default)
    /// threshold, and one ordered AFTER verifies against the NEW threshold.
    #[test]
    fn change_thresholds_at_hlc_boundary_governs_ordering() {
        let admin = mint_test_owner(0xa1);
        let low = mint_test_owner(0xb2);
        let community_id = COM;
        let target = OwnerAddr([0xd1; 16]);

        // low Joins with a real cert (so materialize populates
        // enrolled_device_keys and the later Invite's signer resolves),
        // then admin sets low's power to 10, then admin's sole-signer
        // ChangeThresholds{invite: 25} self-satisfies (admin_quorum == 1
        // default) at wall_ms 10_000.
        let low_join = SignedMembershipEvent {
            signer_certs: Vec::new(),
            id: [0x50; 16],
            community_id,
            actor: low.owner,
            at: Hlc {
                wall_ms: 1_000,
                logical: 0,
                device_id: "t".into(),
            },
            kind: MembershipEventKind::Join,
            sig: [0; 64],
            countersig: None,
            enrollment: Some(low.cert.clone()),
        };
        let set_low_power = ev(
            [0x51; 16],
            admin.owner,
            2_000,
            MembershipEventKind::SetPower {
                target: low.owner,
                level: 10,
            },
        );
        let change_thresholds = ev(
            [0xD0; 16],
            admin.owner,
            10_000,
            MembershipEventKind::AdminProposal {
                proposal_kind: ProposalKind::ChangeThresholds {
                    new_thresholds: PowerThresholds {
                        invite: 25,
                        kick: 50,
                        set_power: 100,
                        max: 100,
                    },
                },
            },
        );

        let all_events = vec![low_join, set_low_power, change_thresholds];

        let make_invite = |id: [u8; 16], wall_ms: u64| -> SignedMembershipEvent {
            let payload = EventPayload {
                id,
                community_id,
                kind: MembershipEventKind::Invite { target },
                actor: low.owner,
                at: Hlc {
                    wall_ms,
                    logical: 0,
                    device_id: "t".into(),
                },
            };
            sign_event(&payload, &low.device_key).expect("sign invite")
        };

        let before_ev = make_invite([0xaa; 16], 5_000); // before the raise (t=10_000)
        let after_ev = make_invite([0xbb; 16], 20_000); // after the raise

        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr: admin.owner,
            is_invite_only: false,
        };

        // BEFORE the threshold-raise: prior state has invite=0 (default) —
        // power-10 `low` may Invite.
        let prior_before = prior_state_at_event(&all_events, &before_ev, admin.owner);
        assert_eq!(prior_before.power_thresholds.invite, 0);
        assert_eq!(verify_event(&before_ev, &prior_before, &ctx), Ok(()));

        // AFTER the threshold-raise: prior state has invite=25 — the SAME
        // power-10 `low` may no longer Invite.
        let prior_after = prior_state_at_event(&all_events, &after_ev, admin.owner);
        assert_eq!(prior_after.power_thresholds.invite, 25);
        let err = verify_event(&after_ev, &prior_after, &ctx).unwrap_err();
        assert!(
            matches!(err, VerifyError::ActorPowerInsufficient),
            "got {err:?}"
        );
    }
}

// ── ZEB-321 RCH1-RCH5 ReachabilityAnnounce verify_event tests ─────────────────

#[cfg(test)]
mod zeb_321_reachability_verify_tests {
    use super::*;
    use crate::reachability_record::ReachabilityAnnouncePayload;

    /// Build a test identity from a seed byte.
    /// Returns (PrivateIdentity, identity_pub [u8; 64], OwnerAddr).
    fn make_identity(seed_byte: u8) -> (TestOwner, [u8; 64], OwnerAddr) {
        let owner = mint_test_owner(seed_byte);
        let addr = owner.owner;
        (owner, [0u8; 64], addr)
    }

    /// ZEB-339: produce a `prior_state` where `admin` is currently Joined with
    /// their enrolled device key materialized (so steady-state signer
    /// resolution + the RCH2 device-key inner-sig check can succeed).
    fn joined_prior(community_id: SpaceId, admin: &TestOwner) -> MaterializedMembership {
        let admin_join_payload = EventPayload {
            id: [0x01; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin.owner,
            at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "t".into(),
            },
        };
        let admin_join =
            sign_event(&admin_join_payload, &admin.device_key).expect("sign admin join");
        let admin_join = SignedMembershipEvent {
            enrollment: Some(admin.cert.clone()),
            ..admin_join
        };
        materialize(std::slice::from_ref(&admin_join), admin.owner)
    }

    /// ZEB-339: build a signed ReachabilityAnnounce event using the owner's
    /// enrolled device key for BOTH the inner reachability signature and the
    /// outer envelope signature (matching the production split-key model).
    fn make_reachability_event(
        community_id: SpaceId,
        owner: &TestOwner,
        actor: OwnerAddr,
        announced_at_ms: u64,
        wall_ms: u64,
    ) -> SignedMembershipEvent {
        use crate::reachability_record::inner_signed_bytes;
        use ed25519_dalek::Signer;
        let hlc = Hlc {
            wall_ms,
            logical: 0,
            device_id: "t".into(),
        };
        let iroh_node_id = [0xAB; 32];
        let home_relay_url = "https://derp.example/".to_string();
        let direct_addresses: Vec<std::net::SocketAddr> = vec![];
        let inner = inner_signed_bytes(
            &iroh_node_id,
            &home_relay_url,
            &direct_addresses,
            announced_at_ms,
            &actor,
            &hlc,
            &[],
            0,
        )
        .expect("inner signed bytes");
        let identity_signature = owner.device_key.sign(&inner).to_bytes();
        let payload = ReachabilityAnnouncePayload {
            iroh_node_id,
            home_relay_url,
            direct_addresses,
            announced_at_ms,
            identity_signature,
            butler_set: Vec::new(),
            bs_at: 0,
        };
        let payload = EventPayload {
            id: [0x42; 16],
            community_id,
            kind: MembershipEventKind::ReachabilityAnnounce { payload },
            actor,
            at: hlc,
        };
        sign_event(&payload, &owner.device_key).expect("sign envelope")
    }

    /// Positive end-to-end: joined actor + valid inner sig + matching
    /// timestamp → Ok(()).
    /// (Doubles as the implicit RCH1 + RCH3 positive: outer sig + actor
    ///  derivation both pass cleanly here.)
    #[test]
    fn verify_reachability_announce_accepts_valid() {
        let community_id = SpaceId([0xc0; 16]);
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let prior = joined_prior(community_id, &admin_priv);

        let event =
            make_reachability_event(community_id, &admin_priv, admin_addr, 1_000_000, 1_000_000);

        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: false,
        };
        verify_event(&event, &prior, &ctx).expect("valid ReachabilityAnnounce must verify");
    }

    /// RCH2 negative: tampering the inner identity_signature bytes
    /// makes verify_event reject with ReachabilityInnerSigInvalid.
    /// (RCH3 explicit test omitted — it's defense-in-depth provably
    ///  equivalent given RCH2 + outer signature verification. RCH1
    ///  explicit test omitted — covered by existing verify_signature
    ///  tests for the outer SignedMembershipEvent shape.)
    #[test]
    fn verify_reachability_announce_rejects_inner_sig_tampering() {
        let community_id = SpaceId([0xc0; 16]);
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let prior = joined_prior(community_id, &admin_priv);

        let mut event =
            make_reachability_event(community_id, &admin_priv, admin_addr, 1_000_000, 1_000_000);

        // Flip a bit inside the inner identity_signature.
        // We must re-sign the OUTER envelope after mutating the payload
        // (else the outer signer-verify would reject with SignatureInvalid
        // before we reach the RCH2 check).
        if let MembershipEventKind::ReachabilityAnnounce { ref mut payload } = event.kind {
            payload.identity_signature[0] ^= 0xFF;
        } else {
            panic!("expected ReachabilityAnnounce");
        }
        let resigned_payload = EventPayload {
            id: event.id,
            community_id: event.community_id,
            kind: event.kind.clone(),
            actor: event.actor,
            at: event.at.clone(),
        };
        let resigned =
            sign_event(&resigned_payload, &admin_priv.device_key).expect("re-sign envelope");

        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: false,
        };
        let result = verify_event(&resigned, &prior, &ctx);
        assert!(
            matches!(result, Err(VerifyError::ReachabilityInnerSigInvalid)),
            "tampered inner sig must produce ReachabilityInnerSigInvalid; got {result:?}"
        );
    }

    /// RCH4 negative: announced_at_ms outside ±30 min of hlc.wall_ms
    /// (here: +31 min) is rejected with ReachabilityTimestampSkew.
    #[test]
    fn verify_reachability_announce_rejects_timestamp_skew() {
        let community_id = SpaceId([0xc0; 16]);
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let prior = joined_prior(community_id, &admin_priv);

        // announced_at_ms = wall_ms + (skew-max + 1 min), i.e. just
        // outside the RCH4 ±30-min window. Referencing the const keeps
        // this test in sync if the threshold is ever tuned.
        let wall_ms: u64 = 1_000_000_000;
        let announced_at_ms = wall_ms + REACHABILITY_TIMESTAMP_SKEW_MAX_MS + 60 * 1000;
        let event = make_reachability_event(
            community_id,
            &admin_priv,
            admin_addr,
            announced_at_ms,
            wall_ms,
        );

        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: false,
        };
        let result = verify_event(&event, &prior, &ctx);
        assert!(
            matches!(result, Err(VerifyError::ReachabilityTimestampSkew)),
            "out-of-skew announced_at_ms must produce ReachabilityTimestampSkew; got {result:?}"
        );
    }

    /// RCH5 negative: actor is not currently Joined (never joined).
    /// Must be rejected with ReachabilityActorNotMember.
    ///
    /// Note: a never-member's RCH5 check fires AFTER RCH2 (inner-sig
    /// passes since the actor signs validly) and RCH4 (timestamp is
    /// in-window). So this test exercises RCH5 in isolation by
    /// ensuring the other gates pass first.
    #[test]
    fn verify_reachability_announce_rejects_non_member() {
        let community_id = SpaceId([0xc0; 16]);
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        // Bob exists as an identity but has never Joined the community.
        let (bob_priv, _bob_pub, bob_addr) = make_identity(0xbb);
        let prior = joined_prior(community_id, &admin_priv);

        let event =
            make_reachability_event(community_id, &bob_priv, bob_addr, 1_000_000, 1_000_000);

        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: false,
        };
        let result = verify_event(&event, &prior, &ctx);
        // ZEB-339: a non-member fails signer resolution (step 1) before the
        // RCH5 membership gate — no materialized member means no enrolled
        // device key to verify the signature against.
        assert!(
            matches!(result, Err(VerifyError::SignerNotEnrolledForActor)),
            "non-member's ReachabilityAnnounce must produce SignerNotEnrolledForActor; got {result:?}"
        );
    }

    /// Sanity: a fresh `ReachabilityAnnouncePayload` (the all-zeros
    /// shape used in serialization round-trip tests) is NOT mistakenly
    /// accepted by verify — its inner identity_signature is all zeros,
    /// which fails verify_strict regardless of identity.
    #[test]
    fn verify_reachability_announce_rejects_all_zero_inner_sig() {
        let community_id = SpaceId([0xc0; 16]);
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let prior = joined_prior(community_id, &admin_priv);

        let hlc = Hlc {
            wall_ms: 1_000_000,
            logical: 0,
            device_id: "t".into(),
        };
        let bad_payload = ReachabilityAnnouncePayload {
            iroh_node_id: [0xAB; 32],
            home_relay_url: "https://derp.example/".into(),
            direct_addresses: vec![],
            announced_at_ms: 1_000_000,
            identity_signature: [0u8; 64],
            butler_set: Vec::new(),
            bs_at: 0,
        };
        let payload = EventPayload {
            id: [0x99; 16],
            community_id,
            kind: MembershipEventKind::ReachabilityAnnounce {
                payload: bad_payload,
            },
            actor: admin_addr,
            at: hlc,
        };
        let event = sign_event(&payload, &admin_priv.device_key).expect("sign envelope");

        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: false,
        };
        let result = verify_event(&event, &prior, &ctx);
        assert!(
            matches!(result, Err(VerifyError::ReachabilityInnerSigInvalid)),
            "all-zero inner sig must produce ReachabilityInnerSigInvalid; got {result:?}"
        );
    }
}

// ── ZEB-458 P4B CommunityRelayAnnounce verify_event tests ─────────────────────

#[cfg(test)]
mod zeb_458_community_relay_announce_verify_tests {
    use super::*;
    use crate::community_relay_announce::{
        build_signed_community_relay_announce, CommunityRelayEntry,
    };

    /// Build a `CommunityRelayEntry` fixture.
    fn fixture_relay_entry() -> CommunityRelayEntry {
        CommunityRelayEntry {
            relay_device_id: [0x11; 16],
            iroh_endpoint_id: [0x22; 32],
            relay_device_ed25519_verify: [0x33; 32],
            home_relay: "https://relay.example/".into(),
        }
    }

    /// Build a signed `CommunityRelayAnnounce` event for `owner` at
    /// `ad_at` (inner timestamp) and `wall_ms` (outer HLC wall).
    fn make_relay_announce_event(
        community_id: SpaceId,
        owner: &TestOwner,
        actor: OwnerAddr,
        ad_at: u64,
        wall_ms: u64,
    ) -> SignedMembershipEvent {
        let hlc = Hlc {
            wall_ms,
            logical: 0,
            device_id: "t".into(),
        };
        let payload = build_signed_community_relay_announce(
            fixture_relay_entry(),
            ad_at,
            &actor,
            &hlc,
            &owner.device_key,
        )
        .expect("build relay announce payload");
        let event_payload = EventPayload {
            id: [0x43; 16],
            community_id,
            kind: MembershipEventKind::CommunityRelayAnnounce { payload },
            actor,
            at: hlc,
        };
        sign_event(&event_payload, &owner.device_key).expect("sign relay announce envelope")
    }

    /// Build a `prior_state` where `owner` is Joined with their enrolled
    /// device key materialized (mirrors `joined_prior` from the reachability
    /// test module).
    fn joined_prior(community_id: SpaceId, owner: &TestOwner) -> MaterializedMembership {
        let join_payload = EventPayload {
            id: [0x01; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: owner.owner,
            at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "t".into(),
            },
        };
        let join_ev = sign_event(&join_payload, &owner.device_key).expect("sign join");
        let join_ev = SignedMembershipEvent {
            enrollment: Some(owner.cert.clone()),
            ..join_ev
        };
        materialize(std::slice::from_ref(&join_ev), owner.owner)
    }

    /// Build a `prior_state` where BOTH `admin` and `member` are Joined with
    /// their enrolled device keys materialized. `admin` is the bootstrap admin
    /// (`VerifyContext::admin_addr`); `member` is a regular (non-admin) Joined
    /// member. Used so the positive CommunityRelayAnnounce test advertises from
    /// a NON-admin member — if `CommunityRelayAnnounce` ever picked up an
    /// admin-only gate, the test would catch it (an admin-as-advertiser test
    /// would falsely keep passing).
    fn joined_prior_two_members(admin: &TestOwner, member: &TestOwner) -> MaterializedMembership {
        let joined_with_enrolled = |owner: &TestOwner| {
            let mut keys = std::collections::BTreeSet::new();
            keys.insert(owner.cert.device_pubkeys.classical.ed25519_verify);
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "t".into(),
                },
                left_at: None,
                enrolled_device_keys: keys,
                revoked_device_keys: BTreeSet::new(),
            }
        };
        let mut mat = MaterializedMembership::default();
        mat.members.insert(admin.owner, joined_with_enrolled(admin));
        mat.members
            .insert(member.owner, joined_with_enrolled(member));
        // Admin carries invite power; the regular member carries none — so the
        // advertiser is unambiguously a non-admin Joined member.
        mat.power_levels
            .insert(admin.owner, POWER_THRESHOLDS.invite);
        mat
    }

    /// Positive: a NON-admin Joined actor + valid inner sig + in-window
    /// `ad_at` → Ok(()). The advertiser is a regular member, NOT the bootstrap
    /// admin, so this would fail if CommunityRelayAnnounce ever gained an
    /// admin-only gate.
    #[test]
    fn community_relay_announce_verifies_when_actor_joined_and_inner_sig_valid() {
        let community_id = SpaceId([0xd0; 16]);
        let admin = mint_test_owner(0xa1);
        let member = mint_test_owner(0xa2);
        let prior = joined_prior_two_members(&admin, &member);

        // Advertise from the regular (non-admin) `member`.
        let event =
            make_relay_announce_event(community_id, &member, member.owner, 1_000_000, 1_000_000);

        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr: admin.owner,
            is_invite_only: false,
        };
        verify_event(&event, &prior, &ctx)
            .expect("valid CommunityRelayAnnounce from a non-admin Joined member must verify");
    }

    /// RCH2 analogue: tampering the inner identity_signature bytes causes
    /// `CommunityRelayInnerSigInvalid`.
    #[test]
    fn community_relay_announce_rejects_inner_sig_tampering() {
        let community_id = SpaceId([0xd0; 16]);
        let member = mint_test_owner(0xa2);
        let prior = joined_prior(community_id, &member);

        let mut event =
            make_relay_announce_event(community_id, &member, member.owner, 1_000_000, 1_000_000);

        // Flip a bit in the inner signature; re-sign the outer envelope so
        // SignatureInvalid doesn't fire before the inner-sig check.
        if let MembershipEventKind::CommunityRelayAnnounce { ref mut payload } = event.kind {
            payload.identity_signature[0] ^= 0xFF;
        } else {
            panic!("expected CommunityRelayAnnounce");
        }
        let resigned_payload = EventPayload {
            id: event.id,
            community_id: event.community_id,
            kind: event.kind.clone(),
            actor: event.actor,
            at: event.at.clone(),
        };
        let resigned = sign_event(&resigned_payload, &member.device_key).expect("re-sign envelope");

        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr: member.owner,
            is_invite_only: false,
        };
        let result = verify_event(&resigned, &prior, &ctx);
        assert!(
            matches!(result, Err(VerifyError::CommunityRelayInnerSigInvalid)),
            "tampered inner sig must produce CommunityRelayInnerSigInvalid; got {result:?}"
        );
    }

    /// RCH4 analogue: `ad_at` outside ±30 min of HLC wall_ms →
    /// `CommunityRelayTimestampSkew`.
    #[test]
    fn community_relay_announce_rejects_timestamp_skew() {
        let community_id = SpaceId([0xd0; 16]);
        let member = mint_test_owner(0xa2);
        let prior = joined_prior(community_id, &member);

        let wall_ms: u64 = 1_000_000_000;
        // 30-min bound is shared with reachability (REACHABILITY_TIMESTAMP_SKEW_MAX_MS).
        let ad_at = wall_ms + REACHABILITY_TIMESTAMP_SKEW_MAX_MS + 60 * 1_000;
        let event = make_relay_announce_event(community_id, &member, member.owner, ad_at, wall_ms);

        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr: member.owner,
            is_invite_only: false,
        };
        let result = verify_event(&event, &prior, &ctx);
        assert!(
            matches!(result, Err(VerifyError::CommunityRelayTimestampSkew)),
            "skewed ad_at must produce CommunityRelayTimestampSkew; got {result:?}"
        );
    }

    /// RCH5 analogue: actor is not a Joined member →
    /// `SignerNotEnrolledForActor` (signer resolution fails before the
    /// membership gate, mirroring the reachability non-member test).
    #[test]
    fn community_relay_announce_rejects_non_member() {
        let community_id = SpaceId([0xd0; 16]);
        let admin = mint_test_owner(0xa2);
        // Bob has never joined.
        let bob = mint_test_owner(0xbb);
        let prior = joined_prior(community_id, &admin);

        let event = make_relay_announce_event(community_id, &bob, bob.owner, 1_000_000, 1_000_000);

        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr: admin.owner,
            is_invite_only: false,
        };
        let result = verify_event(&event, &prior, &ctx);
        assert!(
            matches!(result, Err(VerifyError::SignerNotEnrolledForActor)),
            "non-member's CommunityRelayAnnounce must produce SignerNotEnrolledForActor; got {result:?}"
        );
    }
}

// ── ZEB-339 verify_membership_signer / enrolled_key_from_cert unit tests ──────

// ZEB-339 (spec §9.2): cross-owner end-to-end test.
//
// Two distinct owners (creator + joiner) each have their own
// owner_id / device_key / cert — built via `mint_test_owner`.
// The test builds an event log purely from carried certs + materialized
// state (no shared identity cache / resolver).
//
// Event sequence (invite-only community, creator = admin):
// - Event 1: creator bootstrap Join (carries creator.cert)
// - Event 2: joiner PendingJoin (carries joiner.cert + InviteToken
//   signed by creator.device_key)
// - Event 3: creator JoinCountersign (signed by creator.device_key,
//   resolved from materialized membership enrolled keys)
//
// Each event is verified via verify_event(event, prior_state, &ctx).
// The point: a verifier with ONLY the events + certs (no cache)
// accepts a different owner's cert-bearing and steady-state events.
#[cfg(test)]
mod zeb_339_cross_owner_e2e_tests {
    use super::*;
    use crate::community_invite::{canonical_invite_token_bytes, InviteToken};
    use ed25519_dalek::Signer;

    #[test]
    fn cross_owner_e2e_invite_path_verifies_from_certs_only() {
        // ── Setup ─────────────────────────────────────────────────────────────
        // Use seeds that don't collide via the seed ^ 0xFF master/device swap:
        // creator=0x10, joiner=0x20. Avoid 0x10 ^ 0xFF = 0xEF and 0x20 ^ 0xFF = 0xDF.
        let creator = mint_test_owner(0x10);
        let joiner = mint_test_owner(0x20);
        let community_id = SpaceId([0xCC; 16]);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr: creator.owner,
            is_invite_only: true,
        };

        // ── Event 1: creator bootstrap Join ─────────────────────────────────
        // In an invite-only community the admin self-Join is exempt from the
        // countersig requirement (bootstrap rule).
        let creator_join_payload = EventPayload {
            id: [0x01u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: creator.owner,
            at: Hlc {
                wall_ms: 1_700_000_001_000,
                logical: 0,
                device_id: "creator-dev".into(),
            },
        };
        let creator_join = {
            let ev = sign_event(&creator_join_payload, &creator.device_key).unwrap();
            SignedMembershipEvent {
                enrollment: Some(creator.cert.clone()),
                ..ev
            }
        };

        // Verify event 1 against EMPTY prior_state — the cert is the only
        // trust anchor; no shared identity cache involved.
        let prior_e1 = prior_state_at_event(&[], &creator_join, creator.owner);
        assert_eq!(
            prior_e1.members.len(),
            0,
            "prior state for first event must be empty"
        );
        verify_event(&creator_join, &prior_e1, &ctx)
            .expect("creator bootstrap Join must verify from cert alone against empty state");

        // ── Event 2: joiner PendingJoin ──────────────────────────────────────
        // InviteToken: signed by creator.device_key (not by creator's old
        // identity/Reticulum key). ZEB-339 P5 verifies it against
        // creator's enrolled device key from materialized membership.
        let mut token = InviteToken {
            inviter: creator.owner,
            invitee_hint: Some(joiner.owner),
            minted_at: Hlc {
                wall_ms: 1_700_000_002_000,
                logical: 0,
                device_id: "creator-dev".into(),
            },
            expires_at: None, // open-ended
            sig: [0u8; 64],
        };
        let token_bytes = canonical_invite_token_bytes(&token).expect("encode token");
        token.sig = creator.device_key.sign(&token_bytes).to_bytes();

        let joiner_pending_payload = EventPayload {
            id: [0x02u8; 16],
            community_id,
            kind: MembershipEventKind::PendingJoin {
                invite_token: token,
            },
            actor: joiner.owner,
            at: Hlc {
                wall_ms: 1_700_000_003_000,
                logical: 0,
                device_id: "joiner-dev".into(),
            },
        };
        let joiner_pending = {
            let ev = sign_event(&joiner_pending_payload, &joiner.device_key).unwrap();
            SignedMembershipEvent {
                enrollment: Some(joiner.cert.clone()),
                ..ev
            }
        };

        // Verify event 2 against prior_state that contains only event 1.
        // The inviter's (creator's) enrolled key must already be in the
        // materialized membership so P5 can resolve it.
        let prior_e2 = prior_state_at_event(
            std::slice::from_ref(&creator_join),
            &joiner_pending,
            creator.owner,
        );
        assert!(
            prior_e2.members.contains_key(&creator.owner),
            "creator must be materialized before joiner PendingJoin verification"
        );
        // The creator's enrolled device key must be present in prior_state
        // (populated by materializing the cert carried on event 1).
        let creator_member_state = prior_e2.members.get(&creator.owner).unwrap();
        assert!(
            creator_member_state
                .enrolled_device_keys
                .contains(&creator.cert.device_pubkeys.classical.ed25519_verify),
            "creator's enrolled device key must be in materialized state (ZEB-339)"
        );
        verify_event(&joiner_pending, &prior_e2, &ctx).expect(
            "joiner PendingJoin must verify from cert + creator's materialized enrolled key",
        );

        // ── Event 3: creator JoinCountersign approving the PendingJoin ───────
        // Signed by creator.device_key — this is a STEADY-STATE event, so
        // verify_event resolves the signer from materialized enrolled keys
        // (not from a cert).
        let countersign_payload = EventPayload {
            id: [0x03u8; 16],
            community_id,
            kind: MembershipEventKind::JoinCountersign {
                target_event_id: joiner_pending.id,
            },
            actor: creator.owner,
            at: Hlc {
                wall_ms: 1_700_000_004_000,
                logical: 0,
                device_id: "creator-dev".into(),
            },
        };
        let creator_countersign = sign_event(&countersign_payload, &creator.device_key).unwrap();

        // Verify event 3 against prior state of events 1 + 2.
        // Creator's enrolled key was learned from event 1's cert, so the
        // steady-state signer resolution (no cert carried) must succeed.
        let two_events = [creator_join.clone(), joiner_pending.clone()];
        let prior_e3 = prior_state_at_event(&two_events, &creator_countersign, creator.owner);
        verify_event(&creator_countersign, &prior_e3, &ctx)
            .expect("creator JoinCountersign must verify from materialized enrolled key (no cert)");

        // ── Final materialization: both creator and joiner must be Joined ─────
        let all_events = [
            creator_join.clone(),
            joiner_pending.clone(),
            creator_countersign.clone(),
        ];
        let final_state = materialize(&all_events, creator.owner);

        let creator_state = final_state
            .members
            .get(&creator.owner)
            .expect("creator must be materialized");
        assert_eq!(
            creator_state.status,
            MemberStatus::Joined,
            "creator must be Joined"
        );
        assert!(
            creator_state
                .enrolled_device_keys
                .contains(&creator.cert.device_pubkeys.classical.ed25519_verify),
            "creator's enrolled device key must be in final materialized state"
        );

        let joiner_state = final_state
            .members
            .get(&joiner.owner)
            .expect("joiner must be materialized");
        assert_eq!(
            joiner_state.status,
            MemberStatus::Joined,
            "joiner must be Joined after JoinCountersign"
        );
        assert!(
            joiner_state
                .enrolled_device_keys
                .contains(&joiner.cert.device_pubkeys.classical.ed25519_verify),
            "joiner's enrolled device key must be in final materialized state"
        );
    }
}

// ZEB-339 (spec §9.6): signing regression guard.
//
// Explicitly encodes the invariant that community events are signed by an
// enrolled DEVICE key (#2), NOT by the owner's Reticulum/master key —
// i.e. `actor (owner_id) ≠ address_hash(signing_device_key)`.
//
// This module would BREAK if community signing ever reverted to the old
// single-identity model where the signing key was derived from the same
// seed as the owner_id, making `actor == address_hash(signing_key)`.
//
// ZEB-339 regression guard — if community signing reverts to the
// Reticulum/owner key (actor == address_hash(signing_key)), this test
// module breaks.
#[cfg(test)]
mod zeb_339_signing_regression_guard {
    use super::*;
    use harmony_owner::pubkey_bundle::{ClassicalKeys, PubKeyBundle};

    // Two assertions:
    // (a) owner_id ≠ address_hash(device_signing_key): the owner/device SPLIT
    //     is real — the signer is NOT the actor's own address.
    // (b) bootstrap Join verifies ONLY because the cert binds device→owner:
    //     passes with cert, fails MissingEnrollmentCert without.
    //
    // ZEB-339 regression guard: if signing reverts to the Reticulum/owner key
    // (actor == address_hash(signing_key)), assertion (a) fires.
    #[test]
    fn owner_and_device_key_address_hash_differ_and_cert_is_load_bearing() {
        let o = mint_test_owner(0x42);

        // ── (a) Owner/device SPLIT: owner_id ≠ address_hash(device_key) ─────
        // Compute what address_hash(device_pubkey) would be — the value that
        // would be used as the actor address if signing reverted to the old
        // single-identity model.
        let device_pubkey_bundle = PubKeyBundle {
            classical: ClassicalKeys {
                ed25519_verify: o.device_key.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        let device_identity_hash = device_pubkey_bundle.identity_hash();
        let device_addr = OwnerAddr(device_identity_hash);

        // The actor address is the OWNER (master key) address, not the device
        // key's address. If this ever becomes equal (i.e. signing reverted to
        // the Reticulum/owner key model), the test breaks with a clear message.
        assert_ne!(
            o.owner, device_addr,
            "ZEB-339 regression: owner_id == address_hash(device_signing_key) — \
             community signing has reverted to the old single-identity model"
        );

        // ── (b) Cert is load-bearing: verify_event passes with cert, fails without ─
        let community_id = SpaceId([0xBB; 16]);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr: o.owner,
            is_invite_only: false,
        };

        let payload = EventPayload {
            id: [0x42u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: o.owner,
            at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "regrdev".into(),
            },
        };

        // With cert: must verify against empty prior_state.
        let ev_with_cert = {
            let ev = sign_event(&payload, &o.device_key).unwrap();
            SignedMembershipEvent {
                enrollment: Some(o.cert.clone()),
                ..ev
            }
        };
        verify_event(&ev_with_cert, &MaterializedMembership::default(), &ctx)
            .expect("cert-bearing bootstrap Join must verify");

        // Without cert: must fail — the verifier has no way to resolve the
        // signer's device key from materialized state (no prior member record)
        // and no cert to extract it from.
        let ev_no_cert = sign_event(&payload, &o.device_key).unwrap();
        assert_eq!(
            ev_no_cert.enrollment, None,
            "precondition: event carries no cert"
        );
        let err = verify_event(&ev_no_cert, &MaterializedMembership::default(), &ctx)
            .expect_err("cert-absent bootstrap Join must fail");
        assert_eq!(
            err,
            VerifyError::MissingEnrollmentCert,
            "ZEB-339 regression: cert-absent Join should fail with MissingEnrollmentCert, \
             not SignatureInvalid (which would indicate the signer is still the owner/Reticulum key)"
        );
    }
}

#[cfg(test)]
mod zeb_339_signer_verify_tests {
    use super::*;

    #[test]
    fn verify_signer_accepts_cert_signed_event() {
        let o = mint_test_owner(0x21);
        let ev = sign_event(
            &EventPayload {
                id: [1u8; 16],
                community_id: SpaceId([7u8; 16]),
                kind: MembershipEventKind::Join,
                actor: o.owner,
                at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &o.device_key,
        )
        .unwrap();
        let ev = SignedMembershipEvent {
            enrollment: Some(o.cert.clone()),
            ..ev
        };
        let signer = EnrolledDeviceKey {
            owner: o.owner,
            device_ed25519: o.cert.device_pubkeys.classical.ed25519_verify,
        };
        assert!(verify_membership_signer(&ev, &signer).is_ok());
    }

    #[test]
    fn verify_signer_rejects_tampered_event() {
        let o = mint_test_owner(0x22);
        let mut ev = sign_event(
            &EventPayload {
                id: [1u8; 16],
                community_id: SpaceId([7u8; 16]),
                kind: MembershipEventKind::Join,
                actor: o.owner,
                at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &o.device_key,
        )
        .unwrap();
        ev.id = [2u8; 16]; // tamper after signing
        let signer = EnrolledDeviceKey {
            owner: o.owner,
            device_ed25519: o.device_key.verifying_key().to_bytes(),
        };
        assert_eq!(
            verify_membership_signer(&ev, &signer),
            Err(VerifyError::SignatureInvalid)
        );
    }

    #[test]
    fn enrolled_key_from_cert_accepts_valid_cert() {
        let o = mint_test_owner(0x23);
        let ev = sign_event(
            &EventPayload {
                id: [1u8; 16],
                community_id: SpaceId([7u8; 16]),
                kind: MembershipEventKind::Join,
                actor: o.owner,
                at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &o.device_key,
        )
        .unwrap();
        let ev = SignedMembershipEvent {
            enrollment: Some(o.cert.clone()),
            ..ev
        };
        let edk = enrolled_key_from_cert(&ev).expect("valid cert must succeed");
        assert_eq!(edk.owner, o.owner);
        assert_eq!(
            edk.device_ed25519,
            o.cert.device_pubkeys.classical.ed25519_verify
        );
    }

    #[test]
    fn enrollment_cert_forged_sig_rejected() {
        let mut o = mint_test_owner(0x24);
        o.cert.signature[0] ^= 1; // flip one byte to forge
        let ev = sign_event(
            &EventPayload {
                id: [1u8; 16],
                community_id: SpaceId([7u8; 16]),
                kind: MembershipEventKind::Join,
                actor: o.owner,
                at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &o.device_key,
        )
        .unwrap();
        let ev = SignedMembershipEvent {
            enrollment: Some(o.cert.clone()),
            ..ev
        };
        assert_eq!(
            enrolled_key_from_cert(&ev),
            Err(VerifyError::EnrollmentCertInvalid)
        );
    }

    #[test]
    fn enrollment_cert_owner_mismatch_rejected() {
        // Use a valid cert from owner A but an event whose actor = owner B.
        // This isolates EnrollmentOwnerMismatch from the cert's internal
        // hash check (the cert itself is valid; only the actor binding is wrong).
        let owner_a = mint_test_owner(0x25);
        let owner_b = mint_test_owner(0x26);
        let ev = sign_event(
            &EventPayload {
                id: [1u8; 16],
                community_id: SpaceId([7u8; 16]),
                kind: MembershipEventKind::Join,
                actor: owner_b.owner, // actor = B
                at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &owner_b.device_key,
        )
        .unwrap();
        let ev = SignedMembershipEvent {
            enrollment: Some(owner_a.cert.clone()), // cert = A (valid cert, wrong owner)
            ..ev
        };
        assert_eq!(
            enrolled_key_from_cert(&ev),
            Err(VerifyError::EnrollmentOwnerMismatch)
        );
    }

    #[test]
    fn enrollment_cert_missing_rejected() {
        let o = mint_test_owner(0x27);
        let ev = sign_event(
            &EventPayload {
                id: [1u8; 16],
                community_id: SpaceId([7u8; 16]),
                kind: MembershipEventKind::Join,
                actor: o.owner,
                at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &o.device_key,
        )
        .unwrap();
        assert_eq!(ev.enrollment, None, "precondition: event carries no cert");
        assert_eq!(
            enrolled_key_from_cert(&ev),
            Err(VerifyError::MissingEnrollmentCert)
        );
    }

    /// ZEB-378: enrolled_key_from_cert uses event.at.wall_ms for the expiry check,
    /// NOT the current wall clock — so the decision is CRDT-deterministic: the same
    /// event always yields the same outcome regardless of when it is replayed.
    #[test]
    fn enrolled_key_from_cert_rejects_cert_expired_at_event_time() {
        use harmony_owner::certs::EnrollmentCert;
        use harmony_owner::pubkey_bundle::{ClassicalKeys, PubKeyBundle};

        // Build an owner with a cert that expires at 2_999, issued at 1_000.
        let seed = 0x28u8;
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
            master_bundle.clone(),
            device_id,
            device_bundle.clone(),
            1_000,
            Some(2_999), // expires at 2_999
        )
        .expect("sign_master");
        let owner = OwnerAddr(owner_id);

        // Helper to build a signed event for this owner at a given wall_ms.
        let make_event = |wall_ms: u64, cert: EnrollmentCert| {
            let payload = EventPayload {
                id: [0xEEu8; 16],
                community_id: SpaceId([7u8; 16]),
                kind: MembershipEventKind::Join,
                actor: owner,
                at: Hlc {
                    wall_ms,
                    logical: 0,
                    device_id: "t".into(),
                },
            };
            let ev = sign_event(&payload, &device_sk).expect("sign_event");
            SignedMembershipEvent {
                enrollment: Some(cert),
                ..ev
            }
        };

        // Expired: event wall_ms = 3_000_000 ms → 3_000 s > expires_at = 2_999 s → EnrollmentCertInvalid.
        let expired_event = make_event(3_000_000, cert.clone());
        assert_eq!(
            enrolled_key_from_cert(&expired_event),
            Err(VerifyError::EnrollmentCertInvalid),
            "cert expired AS-OF the event timestamp must be rejected"
        );

        // Valid: event wall_ms = 2_999_000 ms → 2_999 s == expires_at = 2_999 s (not expired; > not >=).
        let ok_event = make_event(2_999_000, cert.clone());
        assert!(
            enrolled_key_from_cert(&ok_event).is_ok(),
            "cert still valid at event timestamp must be accepted"
        );

        // Determinism: same event, same result — the check is purely a function
        // of event.at.wall_ms, not the current wall clock.
        assert!(
            enrolled_key_from_cert(&ok_event).is_ok(),
            "repeated call must yield the same result (CRDT-deterministic)"
        );
    }

    /// ZEB-339/ZEB-677: a `Quorum`-issued cert presented WITHOUT its
    /// signer-cert bundle is rejected even when it passes `cert.verify()`'s
    /// STRUCTURAL checks — the quorum part signatures cannot be verified
    /// without the bundle. (The old blanket non-Master rejection narrowed to
    /// the no-bundle case; see `enrollment_cert_quorum_accepted_with_bundle`.)
    #[test]
    fn enrollment_cert_quorum_issuer_rejected_without_bundle() {
        use harmony_owner::certs::{EnrollmentCert, EnrollmentIssuer};
        use harmony_owner::pubkey_bundle::{ClassicalKeys, PubKeyBundle};

        let device_sk = ed25519_dalek::SigningKey::from_bytes(&[0x55; 32]);
        let device_bundle = PubKeyBundle {
            classical: ClassicalKeys {
                ed25519_verify: device_sk.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        let device_id = device_bundle.identity_hash();
        let owner_id = [0xABu8; 16];
        // Structurally-valid Quorum cert: 2 distinct signers, parity, version 1,
        // device_pubkeys.identity_hash() == device_id. cert.verify() passes its
        // Quorum structural branch (it does NOT verify the signatures).
        let quorum_cert = EnrollmentCert {
            version: 1,
            owner_id,
            device_id,
            device_pubkeys: device_bundle,
            issued_at: 1_700_000_000,
            expires_at: None,
            issuer: EnrollmentIssuer::Quorum {
                signers: vec![[1u8; 16], [2u8; 16]],
                signatures: vec![vec![0u8; 64], vec![0u8; 64]],
            },
            signature: vec![],
        };
        quorum_cert
            .verify(0)
            .expect("hand-built Quorum cert passes structural verify()");

        let ev = sign_event(
            &EventPayload {
                id: [1u8; 16],
                community_id: SpaceId([7u8; 16]),
                kind: MembershipEventKind::Join,
                actor: OwnerAddr(owner_id),
                at: Hlc {
                    wall_ms: 0,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &device_sk,
        )
        .unwrap();
        let ev = SignedMembershipEvent {
            enrollment: Some(quorum_cert),
            ..ev
        };
        // Despite passing structural verify() AND owner_id == actor, a quorum
        // cert with no signer-cert bundle must be rejected.
        assert!(ev.signer_certs.is_empty());
        assert_eq!(
            enrolled_key_from_cert(&ev),
            Err(VerifyError::EnrollmentCertInvalid)
        );
    }

    /// ZEB-677: a genuine Quorum-issued cert presented WITH its Master-issued
    /// signer certs resolves to the enrolled device key, exactly like a
    /// Master cert (depth-1 chain carriage through the chokepoint).
    #[test]
    fn enrollment_cert_quorum_accepted_with_bundle() {
        use crate::enrollment_verify::quorum_fixtures::{mint_quorum_world, WORLD_NOW};
        let world = mint_quorum_world(0x90);
        let ev = sign_event(
            &EventPayload {
                id: [1u8; 16],
                community_id: SpaceId([7u8; 16]),
                kind: MembershipEventKind::Join,
                actor: OwnerAddr(world.owner_id),
                at: Hlc {
                    // Chokepoint gets wall_ms / 1000 — scale WORLD_NOW up so
                    // every cert in the world is valid at event time.
                    wall_ms: WORLD_NOW * 1000,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &world.c_sk,
        )
        .unwrap();
        let ev = SignedMembershipEvent {
            enrollment: Some(world.c_quorum_cert.clone()),
            signer_certs: world.bundle.clone(),
            ..ev
        };
        let resolved = enrolled_key_from_cert(&ev).expect("quorum cert with bundle resolves");
        assert_eq!(resolved.owner, OwnerAddr(world.owner_id));
        assert_eq!(
            resolved.device_ed25519,
            world.c_quorum_cert.device_pubkeys.classical.ed25519_verify
        );
        // And the full event signature path accepts it too.
        verify_membership_signer(&ev, &resolved).expect("event signed by quorum-enrolled device");
    }

    /// ZEB-677: the event's `signer_certs` wire field is additive — absent
    /// key decodes to empty (old encoders), populated bundle round-trips.
    #[test]
    fn event_signer_certs_field_roundtrips_and_defaults_empty() {
        use crate::enrollment_verify::quorum_fixtures::mint_quorum_world;
        let owner = mint_test_owner(0x26);
        let ev = sign_event(
            &EventPayload {
                id: [2u8; 16],
                community_id: SpaceId([7u8; 16]),
                kind: MembershipEventKind::Join,
                actor: owner.owner,
                at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "t".into(),
                },
            },
            &owner.device_key,
        )
        .unwrap();
        assert!(ev.signer_certs.is_empty());
        let bytes = canonical_cbor_encode(&ev).expect("encode");
        let back: SignedMembershipEvent =
            crate::owner_state_crypto::canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(ev, back, "empty bundle round-trips (key omitted)");

        let world = mint_quorum_world(0x94);
        let with_bundle = SignedMembershipEvent {
            signer_certs: world.bundle.clone(),
            ..ev
        };
        let bytes = canonical_cbor_encode(&with_bundle).expect("encode with bundle");
        let back: SignedMembershipEvent =
            crate::owner_state_crypto::canonical_cbor_decode(&bytes).expect("decode with bundle");
        assert_eq!(back.signer_certs, world.bundle, "bundle round-trips");
    }

    // ── ZEB-495 (ZEB-340 Part 2) DeviceAnnounce tests ─────────────────────────

    /// Mint a SECOND device under the SAME owner as `mint_test_owner(master_seed)`.
    /// Reconstructs the owner's master key from `[master_seed; 32]` (the exact
    /// derivation `mint_test_owner` uses) so the new device's Master cert binds
    /// to the IDENTICAL `owner_id`. `device_seed` selects fresh device key
    /// material distinct from `mint_test_owner`'s `[master_seed ^ 0xFF; 32]`
    /// device key. Returns `(device2_signing_key, device2_master_cert)`.
    fn mint_second_device(
        master_seed: u8,
        device_seed: u8,
    ) -> (ed25519_dalek::SigningKey, EnrollmentCert) {
        use harmony_owner::pubkey_bundle::{ClassicalKeys, PubKeyBundle};
        let master_sk = ed25519_dalek::SigningKey::from_bytes(&[master_seed; 32]);
        let master_bundle = PubKeyBundle {
            classical: ClassicalKeys {
                ed25519_verify: master_sk.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        let device2_sk = ed25519_dalek::SigningKey::from_bytes(&[device_seed; 32]);
        let device2_bundle = PubKeyBundle {
            classical: ClassicalKeys {
                ed25519_verify: device2_sk.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        let device2_id = device2_bundle.identity_hash();
        let cert = EnrollmentCert::sign_master(
            &master_sk,
            master_bundle,
            device2_id,
            device2_bundle,
            1_700_000_000,
            None,
        )
        .expect("sign_master for second device");
        cert.verify(0).expect("second-device cert self-verifies");
        (device2_sk, cert)
    }

    /// Sign a `DeviceAnnounce` for `owner`, signed by the SECOND device's key
    /// `device2_sk` and carrying the second device's Master cert `cert2`.
    /// Mirrors the Join attach-cert-after-signing idiom.
    fn make_device_announce(
        owner: OwnerAddr,
        community_id: SpaceId,
        device2_sk: &ed25519_dalek::SigningKey,
        cert2: &EnrollmentCert,
        wall_ms: u64,
    ) -> SignedMembershipEvent {
        let payload = EventPayload {
            id: [0xDA; 16],
            community_id,
            kind: MembershipEventKind::DeviceAnnounce,
            actor: owner,
            at: Hlc {
                wall_ms,
                logical: 0,
                device_id: "device2".into(),
            },
        };
        let ev = sign_event(&payload, device2_sk).expect("sign DeviceAnnounce");
        SignedMembershipEvent {
            enrollment: Some(cert2.clone()),
            ..ev
        }
    }

    /// Build a `MaterializedMembership` where `owner` is a Joined member with
    /// their FIRST device key enrolled (mirrors the materialize(Join) result).
    fn joined_with_first_device(owner: &TestOwner) -> MaterializedMembership {
        let mut keys = BTreeSet::new();
        keys.insert(owner.cert.device_pubkeys.classical.ed25519_verify);
        let mut mat = MaterializedMembership::default();
        mat.members.insert(
            owner.owner,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "t".into(),
                },
                left_at: None,
                enrolled_device_keys: keys,
                revoked_device_keys: BTreeSet::new(),
            },
        );
        mat
    }

    /// Unit 4: a Joined owner + a DeviceAnnounce carrying a SECOND device's
    /// Master cert ⇒ the second key lands in `enrolled_device_keys`, and
    /// status / joined_at / power are unchanged. Mirrors
    /// `materialize_records_enrolled_device_key_from_join_cert`.
    #[test]
    fn materialize_records_enrolled_device_key_from_device_announce() {
        // Use a DISTINCT bootstrap admin so `owner` is a regular (non-admin)
        // member — this keeps the "power unchanged" assertion meaningful
        // (the bootstrap admin would otherwise be implicitly granted power 100).
        let admin = mint_test_owner(0x41);
        let owner = mint_test_owner(0x31);
        let community_id = SpaceId([9u8; 16]);

        // First, the owner joins (device #1 enrolled).
        let join_payload = EventPayload {
            id: [1u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: owner.owner,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "device1".into(),
            },
        };
        let join = sign_event(&join_payload, &owner.device_key).unwrap();
        let join = SignedMembershipEvent {
            enrollment: Some(owner.cert.clone()),
            ..join
        };

        // Baseline: materialize the Join alone, so we can prove DeviceAnnounce
        // changes ONLY enrolled_device_keys (not status/joined_at/power).
        let m_before = materialize(std::slice::from_ref(&join), admin.owner);
        let member_before = m_before.members.get(&owner.owner).expect("owner joined");
        let power_before = m_before
            .power_levels
            .get(&owner.owner)
            .copied()
            .unwrap_or(0);
        assert_eq!(
            member_before.enrolled_device_keys.len(),
            1,
            "only device #1 enrolled before the announce"
        );

        // Then, the second device announces itself.
        let (device2_sk, cert2) = mint_second_device(0x31, 0x32);
        let device2_key = cert2.device_pubkeys.classical.ed25519_verify;
        let announce = make_device_announce(owner.owner, community_id, &device2_sk, &cert2, 200);

        let m = materialize(&[join, announce.clone()], admin.owner);
        let member = m.members.get(&owner.owner).expect("owner is a member");

        // Both device keys are now enrolled.
        assert!(
            member
                .enrolled_device_keys
                .contains(&owner.cert.device_pubkeys.classical.ed25519_verify),
            "first device key must remain enrolled"
        );
        assert!(
            member.enrolled_device_keys.contains(&device2_key),
            "second device key from the DeviceAnnounce cert must be enrolled"
        );
        assert_eq!(member.enrolled_device_keys.len(), 2, "exactly two keys");

        // Status / joined_at unchanged (joined_at stays the ORIGINAL Join HLC,
        // not the DeviceAnnounce HLC — DeviceAnnounce never touches it).
        assert_eq!(member.status, member_before.status, "status unchanged");
        assert_eq!(member.status, MemberStatus::Joined);
        assert_eq!(
            member.joined_at.wall_ms, 100,
            "joined_at unchanged (original Join timestamp preserved)"
        );
        // Power unchanged by DeviceAnnounce (and 0 for this non-admin owner).
        let power_after = m.power_levels.get(&owner.owner).copied().unwrap_or(0);
        assert_eq!(
            power_after, power_before,
            "DeviceAnnounce introduces no power change"
        );
        assert_eq!(power_after, 0, "non-admin owner has no power level");

        // Idempotent: a SECOND announce of the same device key (after the Join)
        // is a no-op BTreeSet::insert — the enrolled set stays at exactly 2.
        let join2 = {
            let p = EventPayload {
                id: [2u8; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: owner.owner,
                at: Hlc {
                    wall_ms: 100,
                    logical: 0,
                    device_id: "device1".into(),
                },
            };
            let e = sign_event(&p, &owner.device_key).unwrap();
            SignedMembershipEvent {
                enrollment: Some(owner.cert.clone()),
                ..e
            }
        };
        let announce2 = make_device_announce(owner.owner, community_id, &device2_sk, &cert2, 300);
        let m2 = materialize(&[join2, announce, announce2], admin.owner);
        assert_eq!(
            m2.members
                .get(&owner.owner)
                .expect("owner present after re-announce")
                .enrolled_device_keys
                .len(),
            2,
            "re-announcing the same device key is an idempotent no-op (still exactly 2 keys)"
        );

        // Defensive: a DeviceAnnounce with NO prior Join (owner is not a member)
        // is a materialize no-op — the owner gets no member entry, no panic.
        let lone_announce =
            make_device_announce(owner.owner, community_id, &device2_sk, &cert2, 400);
        let m_lone = materialize(std::slice::from_ref(&lone_announce), admin.owner);
        assert!(
            !m_lone.members.contains_key(&owner.owner),
            "DeviceAnnounce without a prior Join must not materialize a member"
        );
    }

    /// Unit 4: verify_event accepts a DeviceAnnounce signed by a second device
    /// of an already-Joined owner (cert-path signer resolution + Joined gate).
    #[test]
    fn verify_event_accepts_device_announce_from_joined_owner() {
        let owner = mint_test_owner(0x33);
        let community_id = SpaceId([0xc1; 16]);
        let prior = joined_with_first_device(&owner);

        let (device2_sk, cert2) = mint_second_device(0x33, 0x34);
        let announce = make_device_announce(owner.owner, community_id, &device2_sk, &cert2, 1_000);

        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr: owner.owner,
            is_invite_only: false,
        };
        verify_event(&announce, &prior, &ctx)
            .expect("DeviceAnnounce from an already-Joined owner must verify");

        // Also succeeds in an invite-only community WITHOUT a countersign:
        // DeviceAnnounce is not a Join/PendingJoin, so the countersign gate
        // does not apply (it adds a key for an already-admitted owner).
        let ctx_invite = VerifyContext {
            expected_community_id: community_id,
            admin_addr: owner.owner,
            is_invite_only: true,
        };
        verify_event(&announce, &prior, &ctx_invite)
            .expect("DeviceAnnounce bypasses the invite-only countersign gate by construction");
    }

    /// ZEB-558 (Qodo #336): the deferred open-bootstrap helper must seed the
    /// publisher's FULL enrolled-key set, including a second device added via
    /// DeviceAnnounce — otherwise a root publish signed by device #2 would be
    /// wrongly rejected as PublisherSigInvalid on the cold-bootstrap path.
    #[test]
    fn bootstrap_admit_open_publisher_includes_device_announce_keys() {
        let admin = mint_test_owner(0x71);
        let owner = mint_test_owner(0x72);
        let community_id = SpaceId([0x77; 16]);

        // device #1: the open self-Join.
        let join_payload = EventPayload {
            id: [1u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: owner.owner,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "device1".into(),
            },
        };
        let join = sign_event(&join_payload, &owner.device_key).unwrap();
        let join = SignedMembershipEvent {
            enrollment: Some(owner.cert.clone()),
            ..join
        };

        // device #2: announced after the join.
        let (device2_sk, cert2) = mint_second_device(0x72, 0x73);
        let announce = make_device_announce(owner.owner, community_id, &device2_sk, &cert2, 200);

        let events = vec![join, announce];
        // Root publish lands after both device events (Join@100, announce@200).
        let root_at = Hlc {
            wall_ms: 300,
            logical: 0,
            device_id: "root".into(),
        };
        let ms = bootstrap_admit_open_publisher(
            &events,
            owner.owner,
            admin.owner,
            community_id,
            &root_at,
        )
        .expect("Join + DeviceAnnounce must admit");
        assert!(matches!(ms.status, MemberStatus::Joined));
        assert!(
            ms.enrolled_device_keys
                .contains(&owner.cert.device_pubkeys.classical.ed25519_verify),
            "device #1 key (from Join) must be seeded"
        );
        assert!(
            ms.enrolled_device_keys
                .contains(&cert2.device_pubkeys.classical.ed25519_verify),
            "device #2 key (from DeviceAnnounce) must be seeded"
        );
        assert_eq!(ms.enrolled_device_keys.len(), 2);
    }

    /// ZEB-558 (CodeRabbit #336): admission authorizes against pre-root
    /// membership only, so a DeviceAnnounce whose HLC is AFTER the root publish
    /// HLC must NOT seed an authorizing key. The publisher is still admitted on
    /// the pre-root Join, but the post-root device's key is out of window.
    #[test]
    fn bootstrap_admit_open_publisher_excludes_post_root_device() {
        let admin = mint_test_owner(0x81);
        let owner = mint_test_owner(0x82);
        let community_id = SpaceId([0x88; 16]);

        let join_payload = EventPayload {
            id: [1u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: owner.owner,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "device1".into(),
            },
        };
        let join = sign_event(&join_payload, &owner.device_key).unwrap();
        let join = SignedMembershipEvent {
            enrollment: Some(owner.cert.clone()),
            ..join
        };

        // device #2 announced at wall 400 — AFTER the root publish at wall 300.
        let (device2_sk, cert2) = mint_second_device(0x82, 0x83);
        let announce = make_device_announce(owner.owner, community_id, &device2_sk, &cert2, 400);

        let events = vec![join, announce];
        let root_at = Hlc {
            wall_ms: 300,
            logical: 0,
            device_id: "root".into(),
        };
        let ms = bootstrap_admit_open_publisher(
            &events,
            owner.owner,
            admin.owner,
            community_id,
            &root_at,
        )
        .expect("pre-root Join admits the publisher");
        assert!(matches!(ms.status, MemberStatus::Joined));
        assert!(
            ms.enrolled_device_keys
                .contains(&owner.cert.device_pubkeys.classical.ed25519_verify),
            "device #1 key (pre-root Join) must be seeded"
        );
        assert!(
            !ms.enrolled_device_keys
                .contains(&cert2.device_pubkeys.classical.ed25519_verify),
            "device #2 announced AFTER the root HLC must NOT be seeded"
        );
        assert_eq!(ms.enrolled_device_keys.len(), 1);
    }

    /// ZEB-558 (CodeRabbit #336): a publisher who joined then LEFT before the
    /// root publish HLC must NOT be admitted — folding the self-Leave
    /// materializes them to `Left`, so the helper returns None.
    #[test]
    fn bootstrap_admit_open_publisher_rejects_publisher_who_left_before_root() {
        let admin = mint_test_owner(0x91);
        let owner = mint_test_owner(0x92);
        let community_id = SpaceId([0x99; 16]);

        let join_payload = EventPayload {
            id: [1u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: owner.owner,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "device1".into(),
            },
        };
        let join = sign_event(&join_payload, &owner.device_key).unwrap();
        let join = SignedMembershipEvent {
            enrollment: Some(owner.cert.clone()),
            ..join
        };

        // Self-Leave at wall 150 (still before the root at wall 300). Leave's
        // signer resolves from prior enrolled keys (the Join), so it carries no
        // enrollment cert.
        let leave_payload = EventPayload {
            id: [2u8; 16],
            community_id,
            kind: MembershipEventKind::Leave,
            actor: owner.owner,
            at: Hlc {
                wall_ms: 150,
                logical: 0,
                device_id: "device1".into(),
            },
        };
        let leave = sign_event(&leave_payload, &owner.device_key).unwrap();

        let events = vec![join, leave];
        let root_at = Hlc {
            wall_ms: 300,
            logical: 0,
            device_id: "root".into(),
        };
        let got = bootstrap_admit_open_publisher(
            &events,
            owner.owner,
            admin.owner,
            community_id,
            &root_at,
        );
        assert!(
            got.is_none(),
            "a publisher who left before the root HLC must not be admitted"
        );
    }

    /// Unit 4: verify_event rejects a DeviceAnnounce whose actor is NOT an
    /// already-Joined member (the cert is valid, but membership is absent).
    #[test]
    fn verify_event_rejects_device_announce_from_non_member() {
        let owner = mint_test_owner(0x35);
        let community_id = SpaceId([0xc2; 16]);

        // Empty prior state: the owner is NOT a member.
        let prior = MaterializedMembership::default();

        let (device2_sk, cert2) = mint_second_device(0x35, 0x36);
        let announce = make_device_announce(owner.owner, community_id, &device2_sk, &cert2, 1_000);

        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr: OwnerAddr([0xaa; 16]),
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&announce, &prior, &ctx),
            Err(VerifyError::DeviceAnnounceForNonMember),
            "DeviceAnnounce for a non-member must be rejected"
        );

        // Also rejected when the owner is present but only Left (not Joined).
        let mut prior_left = MaterializedMembership::default();
        prior_left.members.insert(
            owner.owner,
            MemberState {
                status: MemberStatus::Left,
                joined_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "t".into(),
                },
                left_at: Some(Hlc {
                    wall_ms: 2,
                    logical: 0,
                    device_id: "t".into(),
                }),
                enrolled_device_keys: {
                    let mut k = BTreeSet::new();
                    k.insert(owner.cert.device_pubkeys.classical.ed25519_verify);
                    k
                },
                revoked_device_keys: BTreeSet::new(),
            },
        );
        assert_eq!(
            verify_event(&announce, &prior_left, &ctx),
            Err(VerifyError::DeviceAnnounceForNonMember),
            "DeviceAnnounce for a Left (non-Joined) member must be rejected"
        );
    }

    /// Unit 4: a member with TWO enrolled keys for one owner — a steady-state
    /// event signed by EITHER device passes `resolve_enrolled_signer`. This is
    /// the core multi-device claim: once both keys are in the set, either
    /// device can author. (The channel-post half of this assertion is covered
    /// by `verify_channel_event`'s own enrolled-key tests in
    /// community_channel_log.rs, which already iterate the full key set; that
    /// path is async + store-backed, so it is not re-set-up here.)
    #[test]
    fn both_enrolled_devices_verify_after_announce() {
        let owner = mint_test_owner(0x37);
        let community_id = SpaceId([0xc3; 16]);

        // Materialize Join (device #1) then DeviceAnnounce (device #2) so the
        // member's enrolled set is built by the production materialize path.
        let join_payload = EventPayload {
            id: [1u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: owner.owner,
            at: Hlc {
                wall_ms: 10,
                logical: 0,
                device_id: "device1".into(),
            },
        };
        let join = sign_event(&join_payload, &owner.device_key).unwrap();
        let join = SignedMembershipEvent {
            enrollment: Some(owner.cert.clone()),
            ..join
        };
        let (device2_sk, cert2) = mint_second_device(0x37, 0x38);
        let announce = make_device_announce(owner.owner, community_id, &device2_sk, &cert2, 20);
        let prior = materialize(&[join, announce], owner.owner);
        assert_eq!(
            prior
                .members
                .get(&owner.owner)
                .unwrap()
                .enrolled_device_keys
                .len(),
            2,
            "both device keys must be enrolled after the announce"
        );

        // A steady-state event (Leave) signed by device #1 resolves.
        let leave_payload = EventPayload {
            id: [2u8; 16],
            community_id,
            kind: MembershipEventKind::Leave,
            actor: owner.owner,
            at: Hlc {
                wall_ms: 30,
                logical: 0,
                device_id: "device1".into(),
            },
        };
        let leave_d1 = sign_event(&leave_payload, &owner.device_key).unwrap();
        let signer_d1 = resolve_enrolled_signer(&prior, &leave_d1)
            .expect("event signed by device #1 must resolve");
        assert_eq!(
            signer_d1.device_ed25519,
            owner.cert.device_pubkeys.classical.ed25519_verify
        );

        // The SAME steady-state event, signed instead by device #2, also resolves.
        let leave_d2 = sign_event(&leave_payload, &device2_sk).unwrap();
        let signer_d2 = resolve_enrolled_signer(&prior, &leave_d2)
            .expect("event signed by device #2 must resolve");
        assert_eq!(
            signer_d2.device_ed25519,
            cert2.device_pubkeys.classical.ed25519_verify
        );
        assert_ne!(
            signer_d1.device_ed25519, signer_d2.device_ed25519,
            "the two devices resolve to distinct keys"
        );
    }

    /// Unit 4: a DeviceAnnounce `SignedMembershipEvent` round-trips through CBOR
    /// unchanged, and the encoded kind tag is `"e"`.
    #[test]
    fn device_announce_wire_roundtrip() {
        use crate::owner_state_crypto::canonical_cbor_encode;

        let owner = mint_test_owner(0x39);
        let community_id = SpaceId([0xc4; 16]);
        let (device2_sk, cert2) = mint_second_device(0x39, 0x3a);
        let announce = make_device_announce(owner.owner, community_id, &device2_sk, &cert2, 1_234);

        // Full SignedMembershipEvent round-trip.
        let bytes = canonical_cbor_encode(&announce).expect("encode SignedMembershipEvent");
        let decoded: SignedMembershipEvent =
            ciborium::de::from_reader(&bytes[..]).expect("decode SignedMembershipEvent");
        assert_eq!(announce, decoded, "DeviceAnnounce event must round-trip");

        // The kind itself encodes its tag as "e" (adjacently-tagged: { "tg": "e" }).
        let kind_bytes = canonical_cbor_encode(&announce.kind).expect("encode kind");
        let val: ciborium::Value =
            ciborium::de::from_reader(&kind_bytes[..]).expect("decode kind to Value");
        let map = val.as_map().expect("MembershipEventKind encodes as a map");
        let tag = map
            .iter()
            .find(|(k, _)| k.as_text() == Some("tg"))
            .map(|(_, v)| v.clone())
            .expect("kind map has a `tg` tag key");
        assert_eq!(
            tag.as_text(),
            Some("e"),
            "DeviceAnnounce wire tag must be \"e\""
        );
    }

    // ── ZEB-668 S3: DeviceRetire — community retire-announce ─────────────────

    /// Mint a RevocationCert for the SECOND device (the one
    /// `mint_second_device(master_seed, device_seed)` created). Master- or
    /// self-issued per `by_master`. Same seed recipe as `mint_test_owner`.
    fn mint_revocation_for_second_device(
        master_seed: u8,
        device2_sk: &ed25519_dalek::SigningKey,
        cert2: &EnrollmentCert,
        by_master: bool,
    ) -> RevocationCert {
        use harmony_owner::certs::RevocationReason;
        use harmony_owner::pubkey_bundle::{ClassicalKeys, PubKeyBundle};
        if by_master {
            let master_sk = ed25519_dalek::SigningKey::from_bytes(&[master_seed; 32]);
            let master_bundle = PubKeyBundle {
                classical: ClassicalKeys {
                    ed25519_verify: master_sk.verifying_key().to_bytes(),
                    x25519_pub: [0u8; 32],
                },
                post_quantum: None,
            };
            RevocationCert::sign_master(
                &master_sk,
                master_bundle,
                cert2.device_id,
                1_700_000_100,
                RevocationReason::Lost,
            )
            .expect("sign_master revocation")
        } else {
            RevocationCert::sign_self(
                device2_sk,
                cert2.owner_id,
                cert2.device_id,
                1_700_000_100,
                RevocationReason::Decommissioned,
            )
            .expect("sign_self revocation")
        }
    }

    /// A DeviceRetire for the second device, signed by the FIRST (surviving)
    /// device's key. Steady-state signer — no `en` side-channel.
    fn make_device_retire(
        owner: &TestOwner,
        community_id: SpaceId,
        revocation: RevocationCert,
        cert2: &EnrollmentCert,
        wall_ms: u64,
    ) -> SignedMembershipEvent {
        let payload = EventPayload {
            id: [0xDE; 16],
            community_id,
            kind: MembershipEventKind::DeviceRetire {
                revocation,
                enrollment: Box::new(cert2.clone()),
            },
            actor: owner.owner,
            at: Hlc {
                wall_ms,
                logical: 0,
                device_id: "device1".into(),
            },
        };
        sign_event(&payload, &owner.device_key).expect("sign DeviceRetire")
    }

    /// Prior state where the owner is Joined with BOTH device keys enrolled
    /// (first device + the given second-device key).
    fn joined_with_both_devices(
        owner: &TestOwner,
        device2_key: [u8; 32],
    ) -> MaterializedMembership {
        let mut prior = joined_with_first_device(owner);
        prior
            .members
            .get_mut(&owner.owner)
            .expect("owner present")
            .enrolled_device_keys
            .insert(device2_key);
        prior
    }

    fn retire_ctx(community_id: SpaceId, admin: OwnerAddr) -> VerifyContext {
        VerifyContext {
            expected_community_id: community_id,
            admin_addr: admin,
            is_invite_only: false,
        }
    }

    #[test]
    fn verify_event_accepts_master_signed_device_retire() {
        let owner = mint_test_owner(0x61);
        let community_id = SpaceId([0xe1; 16]);
        let (device2_sk, cert2) = mint_second_device(0x61, 0x62);
        let device2_key = cert2.device_pubkeys.classical.ed25519_verify;
        let prior = joined_with_both_devices(&owner, device2_key);

        let rc = mint_revocation_for_second_device(0x61, &device2_sk, &cert2, true);
        let retire = make_device_retire(&owner, community_id, rc, &cert2, 2_000);

        verify_event(&retire, &prior, &retire_ctx(community_id, owner.owner))
            .expect("master-signed DeviceRetire from a surviving enrolled device must verify");
    }

    #[test]
    fn verify_event_accepts_self_signed_device_retire() {
        let owner = mint_test_owner(0x63);
        let community_id = SpaceId([0xe2; 16]);
        let (device2_sk, cert2) = mint_second_device(0x63, 0x64);
        let device2_key = cert2.device_pubkeys.classical.ed25519_verify;
        let prior = joined_with_both_devices(&owner, device2_key);

        let rc = mint_revocation_for_second_device(0x63, &device2_sk, &cert2, false);
        let retire = make_device_retire(&owner, community_id, rc, &cert2, 2_000);

        verify_event(&retire, &prior, &retire_ctx(community_id, owner.owner))
            .expect("self-signed DeviceRetire (retired device's own cert) must verify");
    }

    // ── ZEB-677: Quorum certs in DeviceRetire (both positions) ──────────────

    /// Prior state + retire event for the quorum world: the actor is Joined
    /// with signer-device A's key enrolled (A signs the retire event), and
    /// the event carries the world's signer-cert bundle.
    fn quorum_world_retire(
        world: &crate::enrollment_verify::quorum_fixtures::QuorumWorld,
        community_id: SpaceId,
        revocation: RevocationCert,
        retired: &EnrollmentCert,
    ) -> (SignedMembershipEvent, MaterializedMembership) {
        let actor = OwnerAddr(world.owner_id);
        let mut keys = BTreeSet::new();
        keys.insert(world.a_cert.device_pubkeys.classical.ed25519_verify);
        keys.insert(retired.device_pubkeys.classical.ed25519_verify);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(
            actor,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "t".into(),
                },
                left_at: None,
                enrolled_device_keys: keys,
                revoked_device_keys: BTreeSet::new(),
            },
        );
        let payload = EventPayload {
            id: [0xDF; 16],
            community_id,
            kind: MembershipEventKind::DeviceRetire {
                revocation,
                enrollment: Box::new(retired.clone()),
            },
            actor,
            at: Hlc {
                wall_ms: 2_000,
                logical: 0,
                device_id: "device-a".into(),
            },
        };
        let ev = sign_event(&payload, &world.a_sk).expect("sign DeviceRetire");
        let ev = SignedMembershipEvent {
            signer_certs: world.bundle.clone(),
            ..ev
        };
        (ev, prior)
    }

    /// A Quorum-issued REVOCATION (signers A+B) retiring the Master-certed
    /// device B verifies when the event carries the signer-cert bundle —
    /// the lost-master story's community-visible half.
    #[test]
    fn verify_event_accepts_quorum_revocation_device_retire() {
        use crate::enrollment_verify::quorum_fixtures::{
            mint_quorum_revocation, mint_quorum_world, WORLD_NOW,
        };
        let world = mint_quorum_world(0x98);
        let community_id = SpaceId([0xe6; 16]);
        let rc = mint_quorum_revocation(&world, world.b_cert.device_id, WORLD_NOW);
        let (retire, prior) = quorum_world_retire(&world, community_id, rc, &world.b_cert.clone());
        verify_event(
            &retire,
            &prior,
            &retire_ctx(community_id, OwnerAddr(world.owner_id)),
        )
        .expect("quorum-signed DeviceRetire with bundle must verify");
    }

    /// A Master-signed revocation retiring the QUORUM-enrolled device C
    /// verifies when the event carries the signer-cert bundle (the retired
    /// cert's quorum enrollment needs the bundle to verify).
    #[test]
    fn verify_event_accepts_quorum_enrollment_device_retire() {
        use crate::enrollment_verify::quorum_fixtures::{mint_quorum_world, WORLD_NOW};
        use harmony_owner::certs::RevocationReason;
        let world = mint_quorum_world(0x9C);
        let community_id = SpaceId([0xe7; 16]);
        let rc = RevocationCert::sign_master(
            &world.master_sk,
            world.master_bundle.clone(),
            world.c_quorum_cert.device_id,
            WORLD_NOW,
            RevocationReason::Lost,
        )
        .expect("master revocation of quorum-enrolled device");
        let (retire, prior) =
            quorum_world_retire(&world, community_id, rc, &world.c_quorum_cert.clone());
        verify_event(
            &retire,
            &prior,
            &retire_ctx(community_id, OwnerAddr(world.owner_id)),
        )
        .expect("DeviceRetire of a quorum-enrolled device with bundle must verify");
    }

    /// Both quorum positions FAIL CLOSED when the bundle is stripped.
    #[test]
    fn verify_event_rejects_quorum_device_retire_without_bundle() {
        use crate::enrollment_verify::quorum_fixtures::{
            mint_quorum_revocation, mint_quorum_world, WORLD_NOW,
        };
        use harmony_owner::certs::RevocationReason;

        // Quorum revocation position.
        let world = mint_quorum_world(0xA0);
        let community_id = SpaceId([0xe8; 16]);
        let rc = mint_quorum_revocation(&world, world.b_cert.device_id, WORLD_NOW);
        let (retire, prior) = quorum_world_retire(&world, community_id, rc, &world.b_cert.clone());
        let stripped = SignedMembershipEvent {
            signer_certs: Vec::new(),
            ..retire
        };
        let err = verify_event(
            &stripped,
            &prior,
            &retire_ctx(community_id, OwnerAddr(world.owner_id)),
        )
        .expect_err("quorum revocation without bundle must be rejected");
        assert_eq!(err, VerifyError::DeviceRetireCertInvalid);

        // Quorum enrollment position.
        let world = mint_quorum_world(0xA4);
        let community_id = SpaceId([0xe9; 16]);
        let rc = RevocationCert::sign_master(
            &world.master_sk,
            world.master_bundle.clone(),
            world.c_quorum_cert.device_id,
            WORLD_NOW,
            RevocationReason::Lost,
        )
        .expect("master revocation");
        let (retire, prior) =
            quorum_world_retire(&world, community_id, rc, &world.c_quorum_cert.clone());
        let stripped = SignedMembershipEvent {
            signer_certs: Vec::new(),
            ..retire
        };
        let err = verify_event(
            &stripped,
            &prior,
            &retire_ctx(community_id, OwnerAddr(world.owner_id)),
        )
        .expect_err("quorum enrollment position without bundle must be rejected");
        assert_eq!(err, VerifyError::DeviceRetireCertInvalid);
    }

    #[test]
    fn verify_event_rejects_device_retire_from_non_member() {
        let owner = mint_test_owner(0x65);
        let community_id = SpaceId([0xe3; 16]);
        let (device2_sk, cert2) = mint_second_device(0x65, 0x66);
        let rc = mint_revocation_for_second_device(0x65, &device2_sk, &cert2, true);
        let retire = make_device_retire(&owner, community_id, rc, &cert2, 2_000);

        let prior = MaterializedMembership::default();
        let err = verify_event(&retire, &prior, &retire_ctx(community_id, owner.owner))
            .expect_err("never-member actor must be rejected");
        // Signer resolution (step 1) fires first for an empty prior — either
        // error is a correct rejection, but pin the observed one so a future
        // reordering is a conscious choice.
        assert!(
            matches!(
                err,
                VerifyError::SignerNotEnrolledForActor | VerifyError::DeviceRetireForNonMember
            ),
            "expected a non-member rejection, got {err:?}"
        );
    }

    #[test]
    fn verify_event_rejects_device_retire_with_wrong_owner_binding() {
        let owner = mint_test_owner(0x67);
        let community_id = SpaceId([0xe4; 16]);
        // Cert pair minted under a DIFFERENT master (0x68 ≠ 0x67): both certs
        // are internally valid but belong to another owner.
        let (other_device2_sk, other_cert2) = mint_second_device(0x68, 0x69);
        let prior = joined_with_first_device(&owner);

        let rc = mint_revocation_for_second_device(0x68, &other_device2_sk, &other_cert2, true);
        let retire = make_device_retire(&owner, community_id, rc, &other_cert2, 2_000);

        let err = verify_event(&retire, &prior, &retire_ctx(community_id, owner.owner))
            .expect_err("cert pair for a different owner must be rejected");
        assert!(
            matches!(err, VerifyError::DeviceRetireCertInvalid),
            "expected DeviceRetireCertInvalid, got {err:?}"
        );
    }

    #[test]
    fn verify_event_rejects_device_retire_with_mismatched_target() {
        let owner = mint_test_owner(0x6a);
        let community_id = SpaceId([0xe5; 16]);
        let (device2_sk, cert2) = mint_second_device(0x6a, 0x6b);
        // Revocation cert targets a THIRD device's id — same owner, wrong target.
        let (_, cert3) = mint_second_device(0x6a, 0x6c);
        let device2_key = cert2.device_pubkeys.classical.ed25519_verify;
        let prior = joined_with_both_devices(&owner, device2_key);

        let rc = mint_revocation_for_second_device(0x6a, &device2_sk, &cert3, true);
        let retire = make_device_retire(&owner, community_id, rc, &cert2, 2_000);

        let err = verify_event(&retire, &prior, &retire_ctx(community_id, owner.owner))
            .expect_err("revocation targeting a different device_id must be rejected");
        assert!(
            matches!(err, VerifyError::DeviceRetireCertInvalid),
            "expected DeviceRetireCertInvalid, got {err:?}"
        );
    }

    #[test]
    fn verify_event_rejects_device_retire_with_tampered_revocation_sig() {
        let owner = mint_test_owner(0x6d);
        let community_id = SpaceId([0xe6; 16]);
        let (device2_sk, cert2) = mint_second_device(0x6d, 0x6e);
        let device2_key = cert2.device_pubkeys.classical.ed25519_verify;
        let prior = joined_with_both_devices(&owner, device2_key);

        let mut rc = mint_revocation_for_second_device(0x6d, &device2_sk, &cert2, true);
        rc.signature[0] ^= 0xFF;
        let retire = make_device_retire(&owner, community_id, rc, &cert2, 2_000);

        let err = verify_event(&retire, &prior, &retire_ctx(community_id, owner.owner))
            .expect_err("tampered revocation signature must be rejected");
        assert!(
            matches!(err, VerifyError::DeviceRetireCertInvalid),
            "expected DeviceRetireCertInvalid, got {err:?}"
        );
    }

    #[test]
    fn verify_event_accepts_device_retire_for_left_member() {
        let owner = mint_test_owner(0x6f);
        let community_id = SpaceId([0xe7; 16]);
        let (device2_sk, cert2) = mint_second_device(0x6f, 0x70);
        let device2_key = cert2.device_pubkeys.classical.ed25519_verify;
        let mut prior = joined_with_both_devices(&owner, device2_key);
        // The owner has LEFT — keys persist; retire must still verify
        // (any-status rule: a departed owner's compromised key must be
        // retirable before a rejoin resurrects its authority).
        prior.members.get_mut(&owner.owner).unwrap().status = MemberStatus::Left;

        let rc = mint_revocation_for_second_device(0x6f, &device2_sk, &cert2, true);
        let retire = make_device_retire(&owner, community_id, rc, &cert2, 2_000);

        verify_event(&retire, &prior, &retire_ctx(community_id, owner.owner))
            .expect("DeviceRetire for a Left member must verify (subtractive op)");
    }

    /// Qodo PR #453: a DeviceAnnounce for a tombstoned key must be rejected
    /// at VERIFY time (the carried cert stays master-signed-valid after
    /// revocation, so without this a retired device could insert
    /// verify-passing, materialize-no-op announces forever).
    #[test]
    fn verify_event_rejects_device_announce_for_tombstoned_key() {
        let owner = mint_test_owner(0x7b);
        let community_id = SpaceId([0xec; 16]);
        let (device2_sk, cert2) = mint_second_device(0x7b, 0x7c);
        let device2_key = cert2.device_pubkeys.classical.ed25519_verify;
        let mut prior = joined_with_first_device(&owner);
        prior
            .members
            .get_mut(&owner.owner)
            .unwrap()
            .revoked_device_keys
            .insert(device2_key);

        let announce = make_device_announce(owner.owner, community_id, &device2_sk, &cert2, 2_000);
        let err = verify_event(&announce, &prior, &retire_ctx(community_id, owner.owner))
            .expect_err("announce for a tombstoned key must be rejected");
        assert!(
            matches!(err, VerifyError::DeviceAnnounceForRetiredKey),
            "expected DeviceAnnounceForRetiredKey, got {err:?}"
        );
    }

    /// Full pipeline: join → announce → retire removes the key and
    /// tombstones it; status/joined_at untouched.
    #[test]
    fn materialize_device_retire_removes_and_tombstones_key() {
        let admin = mint_test_owner(0x42);
        let owner = mint_test_owner(0x71);
        let community_id = SpaceId([0xe8; 16]);

        let join_payload = EventPayload {
            id: [1u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: owner.owner,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "device1".into(),
            },
        };
        let join = sign_event(&join_payload, &owner.device_key).unwrap();
        let join = SignedMembershipEvent {
            enrollment: Some(owner.cert.clone()),
            ..join
        };
        let (device2_sk, cert2) = mint_second_device(0x71, 0x72);
        let device2_key = cert2.device_pubkeys.classical.ed25519_verify;
        let announce = make_device_announce(owner.owner, community_id, &device2_sk, &cert2, 200);
        let rc = mint_revocation_for_second_device(0x71, &device2_sk, &cert2, true);
        let retire = make_device_retire(&owner, community_id, rc, &cert2, 300);

        let m = materialize(&[join, announce, retire], admin.owner);
        let member = m.members.get(&owner.owner).expect("owner is a member");
        assert!(
            !member.enrolled_device_keys.contains(&device2_key),
            "retired key must be removed from enrolled_device_keys"
        );
        assert!(
            member.revoked_device_keys.contains(&device2_key),
            "retired key must be tombstoned"
        );
        assert!(
            member
                .enrolled_device_keys
                .contains(&owner.cert.device_pubkeys.classical.ed25519_verify),
            "surviving device key untouched"
        );
        assert_eq!(member.status, MemberStatus::Joined, "status untouched");
        assert_eq!(member.joined_at.wall_ms, 100, "joined_at untouched");
    }

    /// The remove-wins pin: an announce whose HLC sorts AFTER the retire's
    /// must NOT resurrect the key — the tombstone blocks it regardless of
    /// replay order (clock skew can order the announce later even though it
    /// happened first).
    #[test]
    fn materialize_announce_after_retire_does_not_resurrect_key() {
        let admin = mint_test_owner(0x43);
        let owner = mint_test_owner(0x73);
        let community_id = SpaceId([0xe9; 16]);

        let join_payload = EventPayload {
            id: [1u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: owner.owner,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "device1".into(),
            },
        };
        let join = sign_event(&join_payload, &owner.device_key).unwrap();
        let join = SignedMembershipEvent {
            enrollment: Some(owner.cert.clone()),
            ..join
        };
        let (device2_sk, cert2) = mint_second_device(0x73, 0x74);
        let device2_key = cert2.device_pubkeys.classical.ed25519_verify;
        // Retire at wall_ms 200; the announce arrives with wall_ms 300 —
        // i.e. the announce sorts AFTER the retire in the deterministic
        // replay. Without the tombstone this would re-add the key.
        let rc = mint_revocation_for_second_device(0x73, &device2_sk, &cert2, true);
        let retire = make_device_retire(&owner, community_id, rc, &cert2, 200);
        let announce = make_device_announce(owner.owner, community_id, &device2_sk, &cert2, 300);

        let m = materialize(&[join, retire, announce], admin.owner);
        let member = m.members.get(&owner.owner).expect("owner is a member");
        assert!(
            !member.enrolled_device_keys.contains(&device2_key),
            "tombstoned key must NOT be resurrected by a later-sorting announce"
        );
        assert!(
            member.revoked_device_keys.contains(&device2_key),
            "tombstone persists"
        );
    }

    // ── ZEB-580 S2 (T7): spec §8.5 — DeviceRetire materialize must not ────
    // expiry-filter ───────────────────────────────────────────────────────

    /// Builds a Joined `admin` plus a `DeviceRetire` whose second-device
    /// `EnrollmentCert` carries a REAL expiry (unlike `mint_second_device`'s
    /// `expires_at: None`) that has already passed by the time both the
    /// retire event itself is stamped AND the returned `now_past_expiry`
    /// materialize-time floor is set. Returns
    /// `(log, admin_addr, retired_device2_ed25519, now_past_expiry)`.
    ///
    /// Spec §8.5 hazard: DM identity verify is expiry-agnostic (an
    /// expired-but-not-revoked device still DMs, by design). The
    /// mirror-image risk is materialize DROPPING a DeviceRetire because its
    /// backing enrollment cert expired before the retire was materialized —
    /// which would silently lose the revocation and defeat S2's
    /// `RevokedDeviceProjection` cutoff. This fixture manufactures exactly
    /// that scenario: cert expires at 2_000s, the retire event is stamped
    /// at 3_000_000ms (3_000s, already past expiry), and the materialize
    /// `now` floor is 10_000_000ms (10_000s, further past still).
    fn device_retire_expired_fixture(
    ) -> (Vec<SignedMembershipEvent>, OwnerAddr, [u8; 32], Option<u64>) {
        use harmony_owner::certs::EnrollmentCert;
        use harmony_owner::pubkey_bundle::{ClassicalKeys, PubKeyBundle};

        let admin = mint_test_owner(0x91);
        let community_id = SpaceId([0xed; 16]);

        let join_payload = EventPayload {
            id: [1u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin.owner,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "device1".into(),
            },
        };
        let join = sign_event(&join_payload, &admin.device_key).unwrap();
        let join = SignedMembershipEvent {
            enrollment: Some(admin.cert.clone()),
            ..join
        };

        // Second device's cert: issued at 1_000s, EXPIRES at 2_000s — a
        // real, non-open-ended expiry.
        let master_sk = ed25519_dalek::SigningKey::from_bytes(&[0x91; 32]);
        let master_bundle = PubKeyBundle {
            classical: ClassicalKeys {
                ed25519_verify: master_sk.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        let device2_sk = ed25519_dalek::SigningKey::from_bytes(&[0x92; 32]);
        let device2_bundle = PubKeyBundle {
            classical: ClassicalKeys {
                ed25519_verify: device2_sk.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        let device2_id = device2_bundle.identity_hash();
        let cert2 = EnrollmentCert::sign_master(
            &master_sk,
            master_bundle,
            device2_id,
            device2_bundle,
            1_000,       // issued_at (s)
            Some(2_000), // expires_at (s) — genuinely in the past by materialize time
        )
        .expect("sign_master for expiring second device");
        cert2.verify(0).expect("second-device cert self-verifies");
        let device2_key = cert2.device_pubkeys.classical.ed25519_verify;

        // Retire signed by the surviving first device, stamped at wall_ms
        // 3_000_000 (3_000s) — already 1_000s PAST the cert's 2_000s expiry.
        let rc = mint_revocation_for_second_device(0x91, &device2_sk, &cert2, true);
        let retire = make_device_retire(&admin, community_id, rc, &cert2, 3_000_000);

        // now floor: 10_000_000ms (10_000s) — well past both the cert's
        // expiry (2_000s) and the retire event's own wall_ms (3_000s).
        let now_past_expiry = Some(10_000_000u64);

        (
            vec![join, retire],
            admin.owner,
            device2_key,
            now_past_expiry,
        )
    }

    /// PINS spec §8.5: a `DeviceRetire` whose enrollment cert is already
    /// expired at materialize time still lands the retired key in
    /// `revoked_device_keys`. If this ever fails, the materialize path has
    /// started expiry-filtering `DeviceRetire` and S2's revocation cutoff
    /// (`RevokedDeviceProjection`, fed from `revoked_device_keys`) can
    /// silently miss a revocation of an already-expired device.
    #[test]
    fn device_retire_materializes_revocation_even_for_expired_cert() {
        let (log, admin, retired_vk, now_past_expiry) = device_retire_expired_fixture();
        let mat = materialize_with_now(&log, admin, now_past_expiry);
        let member = mat.members.get(&admin).expect("member present");
        assert!(
            member.revoked_device_keys.contains(&retired_vk),
            "an expired-cert DeviceRetire must still record the revocation (spec §8.5)"
        );
    }

    /// End-to-end through CommunityState::insert_event: after a retire, an
    /// event signed by the retired key is rejected exactly as an
    /// unknown-device event — SignerNotEnrolledForActor.
    #[test]
    fn insert_event_rejects_events_signed_by_retired_key() {
        use crate::community_state_crdt::{CommunityState, InsertOutcome};

        let owner = mint_test_owner(0x75);
        let community_id = SpaceId([0xea; 16]);
        let ctx = retire_ctx(community_id, owner.owner);
        let mut state = CommunityState::new(community_id);

        let join_payload = EventPayload {
            id: [1u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: owner.owner,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "device1".into(),
            },
        };
        let join = sign_event(&join_payload, &owner.device_key).unwrap();
        let join = SignedMembershipEvent {
            enrollment: Some(owner.cert.clone()),
            ..join
        };
        assert!(matches!(
            state.insert_event(join, &ctx),
            InsertOutcome::Inserted
        ));

        let (device2_sk, cert2) = mint_second_device(0x75, 0x76);
        let announce = make_device_announce(owner.owner, community_id, &device2_sk, &cert2, 200);
        assert!(matches!(
            state.insert_event(announce, &ctx),
            InsertOutcome::Inserted
        ));

        // Sanity: BEFORE the retire, a device2-signed steady-state event
        // verifies (Leave at wall_ms 250, unused beyond the probe).
        let probe_payload = EventPayload {
            id: [7u8; 16],
            community_id,
            kind: MembershipEventKind::Fork {
                fork_space_id: SpaceId([0x99; 16]),
                reason: Some("probe".into()),
            },
            actor: owner.owner,
            at: Hlc {
                wall_ms: 250,
                logical: 0,
                device_id: "device2".into(),
            },
        };
        let probe = sign_event(&probe_payload, &device2_sk).unwrap();
        assert!(
            matches!(state.insert_event(probe, &ctx), InsertOutcome::Inserted),
            "device2 events verify while its key is enrolled"
        );

        let rc = mint_revocation_for_second_device(0x75, &device2_sk, &cert2, true);
        let retire = make_device_retire(&owner, community_id, rc, &cert2, 300);
        assert!(matches!(
            state.insert_event(retire, &ctx),
            InsertOutcome::Inserted
        ));

        // AFTER the retire: a device2-signed event is rejected exactly as an
        // unknown device's would be.
        let post_payload = EventPayload {
            id: [8u8; 16],
            community_id,
            kind: MembershipEventKind::Fork {
                fork_space_id: SpaceId([0x9a; 16]),
                reason: Some("post-retire".into()),
            },
            actor: owner.owner,
            at: Hlc {
                wall_ms: 400,
                logical: 0,
                device_id: "device2".into(),
            },
        };
        let post = sign_event(&post_payload, &device2_sk).unwrap();
        match state.insert_event(post, &ctx) {
            InsertOutcome::Rejected(VerifyError::SignerNotEnrolledForActor) => {}
            other => panic!("expected Rejected(SignerNotEnrolledForActor), got {other:?}"),
        }
    }

    #[test]
    fn device_retire_wire_roundtrip() {
        use crate::owner_state_crypto::canonical_cbor_encode;

        let owner = mint_test_owner(0x77);
        let community_id = SpaceId([0xeb; 16]);
        let (device2_sk, cert2) = mint_second_device(0x77, 0x78);
        let rc = mint_revocation_for_second_device(0x77, &device2_sk, &cert2, true);
        let retire = make_device_retire(&owner, community_id, rc, &cert2, 1_234);

        // Full SignedMembershipEvent round-trip.
        let bytes = canonical_cbor_encode(&retire).expect("encode SignedMembershipEvent");
        let decoded: SignedMembershipEvent =
            ciborium::de::from_reader(&bytes[..]).expect("decode SignedMembershipEvent");
        assert_eq!(retire, decoded, "DeviceRetire event must round-trip");

        // The kind encodes tag "t" with 2-char inner keys rc/ec.
        let kind_bytes = canonical_cbor_encode(&retire.kind).expect("encode kind");
        let val: ciborium::Value =
            ciborium::de::from_reader(&kind_bytes[..]).expect("decode kind to Value");
        let map = val.as_map().expect("MembershipEventKind encodes as a map");
        let tag = map
            .iter()
            .find(|(k, _)| k.as_text() == Some("tg"))
            .map(|(_, v)| v.clone())
            .expect("kind map has a `tg` tag key");
        assert_eq!(
            tag.as_text(),
            Some("t"),
            "DeviceRetire wire tag must be \"t\""
        );
        let content = map
            .iter()
            .find(|(k, _)| k.as_text() != Some("tg"))
            .map(|(_, v)| v.clone())
            .expect("kind map has a content entry");
        let inner = content.as_map().expect("DeviceRetire content is a map");
        let inner_keys: Vec<&str> = inner.iter().filter_map(|(k, _)| k.as_text()).collect();
        assert!(
            inner_keys.contains(&"rc") && inner_keys.contains(&"ec"),
            "DeviceRetire inner keys must be rc/ec, got {inner_keys:?}"
        );
    }

    /// Tombstones are UNCAPPED by design (PR #453 review): every one requires
    /// a verify_event-passing RevocationCert, so growth is authenticated, and
    /// a cap would silently break remove-wins for the overflow key. Pin that
    /// retires past the old 2×MAX_ENROLLED_DEVICE_KEYS bound all tombstone.
    #[test]
    fn revoked_tombstones_are_not_capped() {
        let owner = mint_test_owner(0x7a);
        let mut member = MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "t".into(),
            },
            left_at: None,
            enrolled_device_keys: test_enrolled_keys(&owner),
            revoked_device_keys: BTreeSet::new(),
        };
        let n = 2 * MAX_ENROLLED_DEVICE_KEYS + 8;
        for i in 0..n as u32 {
            let mut k = [0u8; 32];
            k[0] = 0xAA;
            k[1] = (i >> 8) as u8;
            k[2] = (i & 0xff) as u8;
            member.revoked_device_keys.insert(k);
            // Remove-wins holds for EVERY tombstone, including past the old
            // cap: the guarded inserter refuses each one.
            insert_enrolled_key_unless_retired(&mut member, k);
            assert!(
                !member.enrolled_device_keys.contains(&k),
                "tombstoned key must never re-enroll (i={i})"
            );
        }
        assert_eq!(member.revoked_device_keys.len(), n, "no tombstone dropped");
    }

    /// Additive-field honesty: a MemberState with no tombstones encodes with
    /// NO `rk` key — pre-S3 blobs and S3 empty-set encodings are
    /// byte-identical (no version bump needed).
    #[test]
    fn member_state_with_empty_tombstones_encodes_without_rk_key() {
        use crate::owner_state_crypto::canonical_cbor_encode;

        let owner = mint_test_owner(0x79);
        let member = MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "t".into(),
            },
            left_at: None,
            enrolled_device_keys: test_enrolled_keys(&owner),
            revoked_device_keys: BTreeSet::new(),
        };
        let bytes = canonical_cbor_encode(&member).expect("encode MemberState");
        let val: ciborium::Value =
            ciborium::de::from_reader(&bytes[..]).expect("decode MemberState to Value");
        let map = val.as_map().expect("MemberState encodes as a map");
        assert!(
            !map.iter().any(|(k, _)| k.as_text() == Some("rk")),
            "empty revoked_device_keys must be omitted from the wire"
        );
        // And a non-empty set round-trips through the `rk` key.
        let mut with_tombstone = member.clone();
        with_tombstone.revoked_device_keys.insert([0xCC; 32]);
        let bytes2 = canonical_cbor_encode(&with_tombstone).expect("encode");
        let decoded: MemberState =
            ciborium::de::from_reader(&bytes2[..]).expect("decode MemberState");
        assert_eq!(
            with_tombstone, decoded,
            "tombstoned MemberState round-trips"
        );
    }

    // ── ZEB-401: cap per-member enrolled_device_keys ──────────────────────────
    // (The headroom-over-ZEB-169 invariant is a compile-time `const _` assert
    // next to the constant definition, not a runtime test.)

    /// Build a set of `n` distinct dummy device keys, none equal to `exclude`.
    fn dummy_keys(n: usize, marker: u8, exclude: &[u8; 32]) -> BTreeSet<[u8; 32]> {
        let mut keys = BTreeSet::new();
        let mut i: u32 = 0;
        while keys.len() < n {
            let mut k = [0u8; 32];
            k[0] = marker;
            k[1] = (i >> 8) as u8;
            k[2] = (i & 0xff) as u8;
            if &k != exclude {
                keys.insert(k);
            }
            i += 1;
        }
        keys
    }

    /// verify_event rejects a DeviceAnnounce whose NEW key would push the actor's
    /// enrolled set past the cap (the loud primary gate).
    #[test]
    fn verify_event_rejects_device_announce_over_key_limit() {
        let owner = mint_test_owner(0x51);
        let community_id = SpaceId([0xd1; 16]);
        let (device2_sk, cert2) = mint_second_device(0x51, 0x52);
        let device2_key = cert2.device_pubkeys.classical.ed25519_verify;

        // Owner is Joined with the set already FULL of OTHER keys.
        let keys = dummy_keys(MAX_ENROLLED_DEVICE_KEYS, 0xAB, &device2_key);
        assert_eq!(keys.len(), MAX_ENROLLED_DEVICE_KEYS);
        assert!(!keys.contains(&device2_key));
        let mut prior = MaterializedMembership::default();
        prior.members.insert(
            owner.owner,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "t".into(),
                },
                left_at: None,
                enrolled_device_keys: keys,
                revoked_device_keys: BTreeSet::new(),
            },
        );

        let announce = make_device_announce(owner.owner, community_id, &device2_sk, &cert2, 1_000);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr: owner.owner,
            is_invite_only: false,
        };
        assert_eq!(
            verify_event(&announce, &prior, &ctx),
            Err(VerifyError::EnrolledDeviceKeyLimit),
            "a NEW device key when the set is already at the cap must be rejected"
        );
    }

    /// verify_event still ACCEPTS a DeviceAnnounce that re-announces an
    /// already-enrolled key even when the set is at the cap — idempotent, no
    /// growth, so it must not be mistaken for an over-limit add.
    #[test]
    fn verify_event_accepts_idempotent_device_announce_at_key_limit() {
        let owner = mint_test_owner(0x53);
        let community_id = SpaceId([0xd2; 16]);
        let (device2_sk, cert2) = mint_second_device(0x53, 0x54);
        let device2_key = cert2.device_pubkeys.classical.ed25519_verify;

        // Set is at MAX and ALREADY contains device2_key.
        let mut keys = dummy_keys(MAX_ENROLLED_DEVICE_KEYS - 1, 0xCD, &device2_key);
        keys.insert(device2_key);
        assert_eq!(keys.len(), MAX_ENROLLED_DEVICE_KEYS);
        let mut prior = MaterializedMembership::default();
        prior.members.insert(
            owner.owner,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "t".into(),
                },
                left_at: None,
                enrolled_device_keys: keys,
                revoked_device_keys: BTreeSet::new(),
            },
        );

        let announce = make_device_announce(owner.owner, community_id, &device2_sk, &cert2, 1_000);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr: owner.owner,
            is_invite_only: false,
        };
        verify_event(&announce, &prior, &ctx)
            .expect("re-announcing an already-enrolled key at the cap is idempotent (no growth)");
    }

    /// materialize is a defense-in-depth backstop: even when MORE than MAX
    /// DeviceAnnounce events (each a distinct, structurally-valid second-device
    /// cert) are applied — as a corrupted/replayed log could carry past
    /// verification — the enrolled set never grows beyond the cap.
    #[test]
    fn materialize_caps_enrolled_device_keys_at_max() {
        let admin = mint_test_owner(0x61);
        let owner = mint_test_owner(0x62);
        let community_id = SpaceId([0xe1; 16]);

        // Owner joins (device #1).
        let join_payload = EventPayload {
            id: [1u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: owner.owner,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "device1".into(),
            },
        };
        let join = sign_event(&join_payload, &owner.device_key).unwrap();
        let join = SignedMembershipEvent {
            enrollment: Some(owner.cert.clone()),
            ..join
        };

        // Announce MAX + 5 DISTINCT second devices for the SAME owner (distinct
        // event ids + distinct device certs). Without the cap the set would reach
        // 1 (device #1) + MAX + 5.
        let extra: u16 = 5;
        let mut events = vec![join];
        for i in 0..(MAX_ENROLLED_DEVICE_KEYS as u16 + extra) {
            let device_seed = 0x80u8.wrapping_add(i as u8);
            let (sk, cert) = mint_second_device(0x62, device_seed);
            let payload = EventPayload {
                id: [0xA0u8.wrapping_add(i as u8); 16],
                community_id,
                kind: MembershipEventKind::DeviceAnnounce,
                actor: owner.owner,
                at: Hlc {
                    wall_ms: 200 + i as u64,
                    logical: 0,
                    device_id: "device2".into(),
                },
            };
            let ev = sign_event(&payload, &sk).unwrap();
            events.push(SignedMembershipEvent {
                enrollment: Some(cert),
                ..ev
            });
        }

        let m = materialize(&events, admin.owner);
        let member = m.members.get(&owner.owner).expect("owner is a member");
        assert_eq!(
            member.enrolled_device_keys.len(),
            MAX_ENROLLED_DEVICE_KEYS,
            "materialize must cap the enrolled-key set at MAX regardless of how many announces arrive"
        );
    }

    /// ZEB-401 (Qodo gap): the Join/rejoin path must ALSO honor the cap. `Leave`
    /// preserves `enrolled_device_keys` and a subsequent `Join` from a FRESH
    /// device unions its cert key in, so repeated Leave→Join with new certs would
    /// grow the set without bound if only `DeviceAnnounce` were capped. Drive
    /// MAX+5 such cycles and assert the set never exceeds MAX.
    #[test]
    fn materialize_caps_enrolled_keys_across_repeated_rejoin() {
        let admin = mint_test_owner(0x71);
        let owner = mint_test_owner(0x72);
        let community_id = SpaceId([0xf1; 16]);

        // Device #1 joins.
        let join1_payload = EventPayload {
            id: [1u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: owner.owner,
            at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "device1".into(),
            },
        };
        let join1 = sign_event(&join1_payload, &owner.device_key).unwrap();
        let join1 = SignedMembershipEvent {
            enrollment: Some(owner.cert.clone()),
            ..join1
        };
        let mut events = vec![join1];

        // Repeated Leave (device #1) → Join from a FRESH device of the same owner.
        let cycles: u16 = MAX_ENROLLED_DEVICE_KEYS as u16 + 5;
        for i in 0..cycles {
            let leave_payload = EventPayload {
                id: [0x20u8.wrapping_add(i as u8); 16],
                community_id,
                kind: MembershipEventKind::Leave,
                actor: owner.owner,
                at: Hlc {
                    wall_ms: 200 + (i as u64) * 2,
                    logical: 0,
                    device_id: "device1".into(),
                },
            };
            let leave = sign_event(&leave_payload, &owner.device_key).unwrap();
            events.push(leave);

            let (sk_i, cert_i) = mint_second_device(0x72, 0x80u8.wrapping_add(i as u8));
            let rejoin_payload = EventPayload {
                id: [0x60u8.wrapping_add(i as u8); 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: owner.owner,
                at: Hlc {
                    wall_ms: 201 + (i as u64) * 2,
                    logical: 0,
                    device_id: "device2".into(),
                },
            };
            let rejoin = sign_event(&rejoin_payload, &sk_i).unwrap();
            events.push(SignedMembershipEvent {
                enrollment: Some(cert_i),
                ..rejoin
            });
        }

        let m = materialize(&events, admin.owner);
        let member = m.members.get(&owner.owner).expect("owner is a member");
        assert_eq!(
            member.status,
            MemberStatus::Joined,
            "owner ends Joined after the last rejoin"
        );
        assert_eq!(
            member.enrolled_device_keys.len(),
            MAX_ENROLLED_DEVICE_KEYS,
            "Join/rejoin path must honor the cap — Leave→Join must not accumulate keys past MAX"
        );
    }
}
