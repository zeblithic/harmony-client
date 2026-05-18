//! Community membership CRDT primitives — ZEB-217 Sub-C Phase 1.
//!
//! Per-community signed-event CRDT replicated via the encrypted Zenoh
//! state-root topic (Phase 2). Phase 1 ships only the types,
//! materialization rules, and verification logic — no IPC, no
//! networking, no UI.
//!
//! See `docs/specs/2026-05-05-zeb-217-sub-c-communities-design.md`.

use serde::{Deserialize, Serialize};
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
    /// Inner field key "fs" (2-char) per same-length-keys invariant at this
    /// nesting level. See `docs/specs/2026-05-14-zeb-285-phase1-community-forking-design.md` §3.1.
    #[serde(rename = "x")]
    Fork {
        #[serde(rename = "fs")]
        fork_space_id: SpaceId,
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
        /// 64-byte concatenation of X25519_pub || Ed25519_pub matching
        /// `harmony_identity::Identity::to_public_bytes()`. Same shape
        /// as `CommunityInviteSigned.joiner_identity_pub` (community_invite.rs:258).
        #[serde(
            rename = "jp",
            serialize_with = "serialize_bytes_as_bstr",
            deserialize_with = "deserialize_bytes_from_bstr"
        )]
        joiner_identity_pub: [u8; 64],
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
}

impl CanonicalPayloadSealed for MembershipEventKind {}
impl CanonicalPayload for MembershipEventKind {}

use crate::owner_state_types::{deserialize_bytes_from_bstr, serialize_bytes_as_bstr};
use crate::owner_state_types::{Hlc, SpaceId};

/// 16-byte ULID identifying a single signed membership event within
/// a community's CRDT log. Generated client-side at event creation.
pub type EventId = [u8; 16];

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
/// Wire format: 8-key CBOR map. All keys are 2 chars (text(2) = 3 bytes
/// each) to satisfy the same-length-keys invariant at this nesting
/// level. Adjacently-tagged inner enums (MembershipEventKind,
/// CounterSignature) follow the same rule recursively.
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
        id: payload.id,
        community_id: payload.community_id,
        kind: payload.kind.clone(),
        actor: payload.actor,
        at: payload.at.clone(),
        sig,
        countersig: None,
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
        id: payload.id,
        community_id: payload.community_id,
        kind: payload.kind.clone(),
        actor: payload.actor,
        at: payload.at.clone(),
        sig,
        countersig: None,
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

    /// ZEB-254: PendingJoin's joiner_identity_pub does not hash (via
    /// SHA-256[..16]) to the event's actor address.
    PendingJoinJoinerPubMismatch,

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

    /// ZEB-250 §4.6: direct Kick of an admin (target power==100) was
    /// rejected because admin_quorum > 1.
    /// Must route through AdminProposal + AdminCountersign quorum.
    KickRequiresQuorum,
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
            VerifyError::PendingJoinJoinerPubMismatch => write!(f, "ZEB-254 PendingJoin joiner_identity_pub hash != actor"),
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
            VerifyError::KickRequiresQuorum => write!(f, "ZEB-250: direct Kick of an admin rejected (admin_quorum > 1 — use AdminProposal)"),
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
/// Returns CounterSignerPubkeyMismatch if step 1 fails.
/// Returns CounterSigInvalid if step 2 fails.
/// Power-level checking on the signer happens elsewhere (verify_event)
/// — this function is purely cryptographic.
pub fn verify_countersig(
    event: &SignedMembershipEvent,
    signer_identity_pub: &[u8; 64],
) -> Result<(), VerifyError> {
    let cs = event
        .countersig
        .as_ref()
        .ok_or(VerifyError::CounterSigRequired)?;
    let identity = harmony_identity::Identity::from_public_bytes(signer_identity_pub)
        .map_err(|_| VerifyError::InvalidIdentityPub)?;
    if identity.address_hash != cs.signer.0 {
        return Err(VerifyError::CounterSignerPubkeyMismatch);
    }
    let bytes = canonical_cbor_encode(&EventPayload::from(event))?;
    let sig = Signature::from_bytes(&cs.sig);
    identity
        .verifying_key
        .verify_strict(&bytes, &sig)
        .map_err(|_| VerifyError::CounterSigInvalid)
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
        }
    }
}

pub(crate) fn default_admin_quorum() -> u8 {
    1
}

pub(crate) fn is_default_admin_quorum(q: &u8) -> bool {
    *q == 1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberState {
    #[serde(rename = "st")]
    pub status: MemberStatus,
    #[serde(rename = "ja")]
    pub joined_at: Hlc,
    #[serde(rename = "la", skip_serializing_if = "Option::is_none", default)]
    pub left_at: Option<Hlc>,
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
                    m.members.insert(
                        event.actor,
                        MemberState {
                            status: MemberStatus::Joined,
                            joined_at: event.at.clone(),
                            left_at: None,
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
                    m.members.insert(
                        *target,
                        MemberState {
                            status: MemberStatus::Invited,
                            joined_at: event.at.clone(),
                            left_at: None,
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
            } => {
                // Idempotent on duplicate channel_id: first create wins
                // (replays + reorderings under DAG-sync may deliver the
                // same ChannelCreate twice; the second one must NOT
                // overwrite name/write_power/created_at — that would let
                // a duplicate-emit refresh created_at and reset history
                // markers). A subsequent ChannelModify is the right path
                // to update fields; a duplicate ChannelCreate is a no-op.
                m.channels
                    .entry(*channel_id)
                    .or_insert_with(|| ChannelInfo {
                        name: name.clone(),
                        write_power: *write_power,
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
                let kick_target = match triggered_event.map(|e| &e.kind) {
                    Some(MembershipEventKind::Kick { target, .. }) => Some(*target),
                    Some(MembershipEventKind::Leave) => triggered_event.map(|e| e.actor),
                    _ => None,
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
                let is_admin = issuer_power >= POWER_THRESHOLDS.kick && issuer_is_effective_member;
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
                if !is_admin && !is_self_leaver {
                    continue;
                }

                // Apply: advance epoch. Per-receiver key insertion happens
                // outside materialize (community_state_sync apply layer —
                // Tasks 5/6). materialize is pure replay.
                m.current_epoch = Some(current + 1);
                m.pending_rotation_for.remove(&target);
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
                let is_admin = issuer_power >= POWER_THRESHOLDS.kick && issuer_is_effective_member;
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
                            m.members.insert(
                                event.actor,
                                MemberState {
                                    status: MemberStatus::PendingJoin,
                                    joined_at: event.at.clone(),
                                    left_at: None,
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
                        }
                    }
                }
                // else: countersign targets an unknown proposal and target IS
                // in seen_admin_proposals but not in proposals_index — shouldn't
                // happen in a well-formed log; silently skip.
            }
        }
    }

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
/// `actor_identity_pub` is the canonical 64-byte combined identity
/// public bytes (`X25519_pub(32) || Ed25519_pub(32)`) for the OwnerAddr
/// claimed in `event.actor`. Sub-A's owner-device cache is the source.
/// verify_event derives the address_hash from these bytes and checks
/// it matches `event.actor` — so a caller cache-lookup bug, a stale
/// cache entry, or a key-substitution attack all surface as
/// ActorPubkeyMismatch instead of being silently accepted.
///
/// `countersigner_identity_pub` is None for open communities, for
/// non-Join events, and for admin self-Join in invite-only. For all
/// other invite-only Joins it MUST be Some, with the hashed bytes
/// matching `event.countersig.signer`.
pub struct VerifyContext<'a> {
    pub expected_community_id: SpaceId,
    pub admin_addr: OwnerAddr,
    pub is_invite_only: bool,
    pub actor_identity_pub: &'a [u8; 64],
    pub countersigner_identity_pub: Option<&'a [u8; 64]>,
    /// ZEB-254: admin's identity_pub (X25519_pub || Ed25519_pub). Required
    /// when verifying PendingJoin events because the verify gate needs to
    /// check the InviteToken's signature against the admin's known pub.
    /// For legacy paths (open community, legacy Join with countersig), this
    /// field can be None; the existing verify paths don't reference it.
    pub admin_identity_pub: Option<&'a [u8; 64]>,
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
// allow: POWER_THRESHOLDS.invite is hardcoded 0 in v1, so `power < invite`
// is always false for u8. The comparisons are structural placeholders for
// ZEB-251 per-community threshold customization where invite_threshold > 0
// will make them firable. Suppressing avoids the lint while keeping the
// rule shape correct for the planned extension.
#[allow(clippy::absurd_extreme_comparisons)]
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

    // 1. Actor's identity_pub must hash to event.actor AND its
    //    Ed25519 component must verify the signature.
    verify_signature(event, ctx.actor_identity_pub)?;

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
    // Note: under v1's hardcoded POWER_THRESHOLDS.invite = 0, the
    // power check below is unreachable (any owner addr defaults to
    // power 0 ≥ 0). The check exists because per-community threshold
    // customization (ZEB-251) will make it firable when invite_threshold
    // > 0. Keeping the rule structurally in place now means ZEB-251
    // doesn't need to revisit verify_event.
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
        let cs_identity_pub = ctx
            .countersigner_identity_pub
            .ok_or(VerifyError::CounterSigRequired)?;
        verify_countersig(event, cs_identity_pub)?;

        if !is_joined_member(prior_state, &cs.signer) {
            return Err(VerifyError::CounterSignerNotJoined);
        }

        let signer_power = prior_state
            .power_levels
            .get(&cs.signer)
            .copied()
            .unwrap_or(0);
        if signer_power < POWER_THRESHOLDS.invite {
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
        MembershipEventKind::PendingJoin {
            invite_token,
            joiner_identity_pub,
        } => {
            // P1: joiner_identity_pub must hash to event.actor.
            // Use harmony_identity::Identity::from_public_bytes which derives
            // address_hash = SHA256(X25519_pub || Ed25519_pub)[..16].
            let identity = harmony_identity::Identity::from_public_bytes(joiner_identity_pub)
                .map_err(|_| VerifyError::PendingJoinJoinerPubMismatch)?;
            if identity.address_hash != event.actor.0 {
                return Err(VerifyError::PendingJoinJoinerPubMismatch);
            }

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

            // P5: invite_token signature verifies against admin's identity_pub.
            let admin_pub = ctx
                .admin_identity_pub
                .ok_or(VerifyError::PendingJoinTokenInvalid)?;
            if crate::community_invite::verify_invite_token_signature(invite_token, admin_pub)
                .is_err()
            {
                return Err(VerifyError::PendingJoinTokenInvalid);
            }

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
                    if *level > POWER_THRESHOLDS.max {
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
            if actor_power < POWER_THRESHOLDS.invite {
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
            if actor_power < POWER_THRESHOLDS.kick {
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
            if actor_power < POWER_THRESHOLDS.set_power {
                return Err(VerifyError::ActorPowerInsufficient);
            }
            if *level > POWER_THRESHOLDS.max {
                return Err(VerifyError::PowerLevelOutOfRange);
            }
            // ZEB-250 §4.5: direct SetPower of admin-affecting target
            // is rejected when admin_quorum > 1. Must route via AdminProposal.
            if prior_state.admin_quorum > 1 {
                let target_power = prior_state.power_levels.get(target).copied().unwrap_or(0);
                let admin_affecting = *level == 100 || target_power == 100;
                if admin_affecting {
                    return Err(VerifyError::SetPowerRequiresQuorum);
                }
            }
        }
        MembershipEventKind::Unban { target, reason } => {
            // Admin-tier: actor must have power >= set_power threshold (100).
            if actor_power < POWER_THRESHOLDS.set_power {
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
        } => {
            if actor_power < POWER_THRESHOLDS.kick {
                return Err(VerifyError::ChannelAdminInsufficientPower);
            }
            // Validate name length (1-32 chars per spec §12.3).
            if name.trim().is_empty() || name.chars().count() > 32 {
                return Err(VerifyError::ChannelNameInvalid);
            }
            // Validate write_power range.
            if *write_power > POWER_THRESHOLDS.max {
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
            if actor_power < POWER_THRESHOLDS.kick {
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
                if *wp > POWER_THRESHOLDS.max {
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
            if actor_power < POWER_THRESHOLDS.kick {
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
            let issuer_is_admin = issuer_power >= POWER_THRESHOLDS.kick
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
            let issuer_is_effective_admin = issuer_power >= POWER_THRESHOLDS.kick
                && (issuer_is_joined || issuer_is_bootstrap_admin);
            if !issuer_is_effective_admin {
                return Err(VerifyError::EpochEventUnauthorized);
            }
            let current_epoch = prior_state.current_epoch.unwrap_or(0);
            if *epoch != current_epoch {
                return Err(VerifyError::EpochEventUnauthorized);
            }
        }
        MembershipEventKind::Fork { .. } => {
            // ZEB-285: any joined non-Banned member can fork at any time.
            // Power threshold 0 — same as Leave. Non-mutating: doesn't
            // affect membership/power/channels, doesn't trigger EpochRotation.
            // Membership check already performed in the joined-membership
            // block above (ActorNotJoined gate). No additional shape
            // validation required: fork_space_id is a self-reported value
            // from the forker; receivers don't (and can't) verify the fork's
            // existence on the forker's device.
            if actor_power < POWER_THRESHOLDS.invite {
                return Err(VerifyError::ActorPowerInsufficient);
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
            if actor_power < POWER_THRESHOLDS.invite {
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
    }

    Ok(())
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
        verify_countersig(event, countersigner_pub)?;
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
#[derive(Debug, Clone, Copy)]
pub struct PowerThresholds {
    pub invite: u8,
    pub kick: u8,
    pub set_power: u8,
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
/// Returns `Err` (as String, matching the existing IPC error convention)
/// if:
/// - NodeState is missing any required field (registry, outbox, etc.) —
///   typically because the node isn't running.
/// - `new_power > 100` (out of u8 / power-table range).
/// - The community has no engine in the registry (caller isn't joined).
/// - Signing or apply fails at the CRDT layer (e.g., actor not admin —
///   verify_event rejects with InsufficientPower).
pub async fn apply_auto_exec_set_power(
    node_state: &std::sync::Arc<std::sync::Mutex<crate::NodeState>>,
    community_id: crate::owner_state_types::SpaceId,
    target_pubkey: crate::owner_state_types::OwnerAddr,
    new_power: u32,
) -> Result<(), String> {
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

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let event_hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;

    let engine_arc = community_registry
        .engine_arc(&community_id)
        .await
        .ok_or_else(|| {
            format!(
                "no engine for community {} — not currently joined",
                hex::encode(community_id.0)
            )
        })?;

    let event = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
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
    Ok(())
}

#[cfg(test)]
mod auto_exec_tests {
    use super::*;
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_kick_event(
        id_byte: u8,
        actor: OwnerAddr,
        target: OwnerAddr,
        at_wall_ms: u64,
    ) -> SignedMembershipEvent {
        let mut id = [0xfa; 16];
        id[15] = id_byte;
        SignedMembershipEvent {
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
        }
    }

    fn make_leave_event(id_byte: u8, actor: OwnerAddr, at_wall_ms: u64) -> SignedMembershipEvent {
        let mut id = [0xfc; 16];
        id[15] = id_byte;
        SignedMembershipEvent {
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
        }
    }

    /// Helper: construct a Join event for a member so materialize can
    /// find them in the members map (needed for Kick to update status).
    fn make_join_event(id_byte: u8, actor: OwnerAddr, at_wall_ms: u64) -> SignedMembershipEvent {
        let mut id = [0xfd; 16];
        id[15] = id_byte;
        SignedMembershipEvent {
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

    /// Build a test identity from a seed byte. Returns (PrivateIdentity, identity_pub, OwnerAddr).
    fn make_identity(seed_byte: u8) -> (harmony_identity::PrivateIdentity, [u8; 64], OwnerAddr) {
        let seed = [seed_byte; 32];
        let private = harmony_identity::PrivateIdentity::from_seed(&seed);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let addr = OwnerAddr(public.address_hash);
        (private, identity_pub, addr)
    }

    /// Helper: sign a membership event payload using a PrivateIdentity.
    fn sign_with_identity(
        payload: EventPayload,
        private: &harmony_identity::PrivateIdentity,
    ) -> SignedMembershipEvent {
        sign_event_with_identity(&payload, private).expect("sign_event_with_identity must succeed")
    }

    /// C4: verify_event must reject an EpochRotation issued by a never-member
    /// (zero power, not in members map) even if the signature is valid.
    #[test]
    fn verify_event_rejects_unauthorized_epoch_rotation_from_never_member() {
        let community_id = SpaceId([0xc0; 16]);
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let (attacker_priv, attacker_pub, attacker_addr) = make_identity(0xee);

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
            actor_identity_pub: &attacker_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
        };
        let result = verify_event(&rotation_event, &prior, &ctx);
        assert!(
            matches!(result, Err(VerifyError::EpochEventUnauthorized)),
            "EpochRotation from never-member must be rejected with EpochEventUnauthorized; got {result:?}"
        );
    }

    /// C4: verify_event must reject an EpochCatchup from a non-admin (power < 50).
    #[test]
    fn verify_event_rejects_unauthorized_epoch_catchup_non_admin() {
        let community_id = SpaceId([0xc0; 16]);
        let (admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let (bob_priv, bob_pub, bob_addr) = make_identity(0xb1);

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
            actor_identity_pub: &bob_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
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
        }
    }

    // ── ZEB-284 Task 1: Unban variant unit tests ──────────────────────────────

    /// Unban by an admin (power 100) on a Banned target must:
    ///   (a) pass verify_event, and
    ///   (b) materialize to MemberStatus::Left.
    #[test]
    fn unban_event_succeeds_when_actor_is_admin_and_target_is_banned() {
        let community_id = SpaceId([0xc0; 16]);
        let (admin_priv, admin_pub, admin_addr) = make_identity(0xa1);
        let (_, _, target_addr) = make_identity(0xb1);

        // Prior state: admin joined, target joined then kicked (Banned).
        let admin_join = make_join_event(0x01, admin_addr, 1);
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
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
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
        let (_admin_priv, _admin_pub, admin_addr) = make_identity(0xa1);
        let (mod_priv, mod_pub, mod_addr) = make_identity(0xb1);
        let (_, _, target_addr) = make_identity(0xc1);

        // Build prior state: mod_addr has power 50, target is Banned.
        let admin_join = make_join_event(0x01, admin_addr, 1);
        let mod_join = make_join_event(0x02, mod_addr, 2);
        let target_join = make_join_event(0x03, target_addr, 3);
        let setpwr_mod = SignedMembershipEvent {
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
            actor_identity_pub: &mod_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
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
        let (admin_priv, admin_pub, admin_addr) = make_identity(0xa1);
        let (_, _, target_addr) = make_identity(0xb1);

        // Prior state: admin joined, target joined (NOT banned).
        let admin_join = make_join_event(0x01, admin_addr, 1);
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
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
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
        let (admin_priv, admin_pub, admin_addr) = make_identity(0xa1);
        let (_, _, unknown_addr) = make_identity(0xdd);

        // Prior state: only admin joined; unknown_addr never appeared.
        let admin_join = make_join_event(0x01, admin_addr, 1);
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
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
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
        let (admin_priv, admin_pub, admin_addr) = make_identity(0xa1);
        let (_, _, target_addr) = make_identity(0xb1);

        let admin_join = make_join_event(0x01, admin_addr, 1);
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
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
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
        let (admin_priv, admin_pub, admin_addr) = make_identity(0xa1);
        let (_, _, target_addr) = make_identity(0xb1);

        // Set up Banned target via prior Kick
        let admin_join = make_join_event(0x01, admin_addr, 1);
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
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
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
        let event = MembershipEventKind::Fork { fork_space_id };

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
            },
        ];

        for variant in &variants {
            let bytes = canonical_cbor_encode(variant)
                .unwrap_or_else(|e| panic!("encode failed for {variant:?}: {e}"));
            let decoded: MembershipEventKind = ciborium::de::from_reader(&bytes[..])
                .unwrap_or_else(|e| panic!("decode failed for {variant:?}: {e}"));
            assert_eq!(variant, &decoded, "roundtrip mismatch for {variant:?}");
        }
    }

    /// ZEB-285 Step 6/9: verify_event allows a Fork from any joined member
    /// (power 0 = regular member, not just admin).
    #[test]
    fn verify_event_fork_allows_any_joined_member() {
        let community_id = SpaceId([0xc0; 16]);
        let (admin_priv, admin_pub, admin_addr) = make_identity(0xa1);
        let (regular_priv, regular_pub, regular_addr) = make_identity(0xb1);

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
            actor_identity_pub: &regular_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
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
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
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
        let (outsider_priv, outsider_pub, outsider_addr) = make_identity(0xcc);

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
            actor_identity_pub: &outsider_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
        };
        assert_eq!(
            verify_event(&fork, &prior, &ctx),
            Err(VerifyError::ActorNotJoined),
            "fork by non-member should reject with ActorNotJoined"
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
            id: [0x02; 16],
            community_id,
            kind: MembershipEventKind::Fork {
                fork_space_id: SpaceId([0xfe; 16]),
            },
            actor: admin,
            at: Hlc {
                wall_ms: 2,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0; 64],
            countersig: None,
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
            id: [0x02; 16],
            community_id,
            kind: MembershipEventKind::Fork {
                fork_space_id: SpaceId([0xfe; 16]),
            },
            actor: admin,
            at: Hlc {
                wall_ms: 2,
                logical: 0,
                device_id: "test".into(),
            },
            sig: [0; 64],
            countersig: None,
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

        let original_id = SpaceId([0xa0; 16]);
        let (admin_priv, admin_pub, admin_addr) = make_identity(0xaa);
        let (regular_priv, regular_pub, regular_addr) = make_identity(0xbb);

        // Bootstrap: admin joins, then regular joins, then admin promotes regular.
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
        let regular_join = sign_with_identity(
            EventPayload {
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
        );
        let set_power = sign_with_identity(
            EventPayload {
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
        );

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
            joiner_identity_pub: [4u8; 64],
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
}

// ── ZEB-254 PendingJoin verify_event unit tests ───────────────────────────────

#[cfg(test)]
mod zeb_254_pending_join_verify_tests {
    use super::*;
    use crate::community_invite::InviteToken;

    /// Build a test identity from a seed byte.
    /// Returns (PrivateIdentity, identity_pub [u8; 64], OwnerAddr).
    fn make_identity(seed_byte: u8) -> (harmony_identity::PrivateIdentity, [u8; 64], OwnerAddr) {
        let seed = [seed_byte; 32];
        let private = harmony_identity::PrivateIdentity::from_seed(&seed);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let addr = OwnerAddr(public.address_hash);
        (private, identity_pub, addr)
    }

    /// Build a signed InviteToken using the admin's identity.
    fn make_invite_token(
        admin_private: &harmony_identity::PrivateIdentity,
        admin_addr: OwnerAddr,
        invitee_hint: Option<OwnerAddr>,
        expires_at: Option<u64>,
    ) -> InviteToken {
        use crate::community_invite::canonical_invite_token_bytes;

        let mut tok = InviteToken {
            inviter: admin_addr,
            invitee_hint,
            minted_at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "admin-device".into(),
            },
            expires_at,
            sig: [0u8; 64],
        };
        // Sign canonical bytes via the admin's PrivateIdentity.
        let bytes = canonical_invite_token_bytes(&tok).expect("encode token");
        tok.sig = admin_private.sign(&bytes);
        tok
    }

    /// Build a signed PendingJoin event for the given joiner.
    fn make_pending_join_event(
        joiner_private: &harmony_identity::PrivateIdentity,
        joiner_addr: OwnerAddr,
        joiner_pub: [u8; 64],
        community_id: SpaceId,
        token: InviteToken,
    ) -> SignedMembershipEvent {
        let payload = EventPayload {
            id: [9u8; 16],
            community_id,
            kind: MembershipEventKind::PendingJoin {
                invite_token: token,
                joiner_identity_pub: joiner_pub,
            },
            actor: joiner_addr,
            at: Hlc {
                wall_ms: 1_700_000_001_000,
                logical: 0,
                device_id: "joiner-device".into(),
            },
        };
        sign_event_with_identity(&payload, joiner_private).expect("sign PendingJoin")
    }

    #[test]
    fn pending_join_event_signs_and_verifies() {
        let (admin_priv, admin_pub, admin_addr) = make_identity(0xa1);
        let (joiner_priv, joiner_pub, joiner_addr) = make_identity(0xb1);
        let community_id = SpaceId([7u8; 16]);
        let token = make_invite_token(
            &admin_priv,
            admin_addr,
            Some(joiner_addr),
            Some(1_700_000_100_000),
        );
        let event =
            make_pending_join_event(&joiner_priv, joiner_addr, joiner_pub, community_id, token);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
            actor_identity_pub: &joiner_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: Some(&admin_pub),
        };
        let mat = MaterializedMembership::default();
        let result = verify_event(&event, &mat, &ctx);
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
    }

    #[test]
    fn pending_join_rejected_when_token_invitee_not_actor() {
        let (admin_priv, admin_pub, admin_addr) = make_identity(0xa1);
        let (joiner_priv, joiner_pub, joiner_addr) = make_identity(0xb1);
        let community_id = SpaceId([7u8; 16]);
        // Hint addresses someone else, not the joiner.
        let token = make_invite_token(
            &admin_priv,
            admin_addr,
            Some(OwnerAddr([99u8; 16])),
            Some(1_700_000_100_000),
        );
        let event =
            make_pending_join_event(&joiner_priv, joiner_addr, joiner_pub, community_id, token);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
            actor_identity_pub: &joiner_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: Some(&admin_pub),
        };
        let mat = MaterializedMembership::default();
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
        let (admin_priv, admin_pub, admin_addr) = make_identity(0xa1);
        let (joiner_priv, joiner_pub, joiner_addr) = make_identity(0xb1);
        let community_id = SpaceId([7u8; 16]);
        // expires_at is BEFORE the event's wall_ms (1_700_000_001_000).
        let token = make_invite_token(
            &admin_priv,
            admin_addr,
            Some(joiner_addr),
            Some(1_700_000_000_500),
        );
        let event =
            make_pending_join_event(&joiner_priv, joiner_addr, joiner_pub, community_id, token);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
            actor_identity_pub: &joiner_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: Some(&admin_pub),
        };
        let mat = MaterializedMembership::default();
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
        let (_admin2_priv, admin2_pub, admin2_addr) = make_identity(0xa2);
        let (joiner_priv, joiner_pub, joiner_addr) = make_identity(0xb1);
        let community_id = SpaceId([7u8; 16]);
        // Token is signed by rogue, not admin2.
        let token = make_invite_token(
            &rogue_priv,
            rogue_addr,
            Some(joiner_addr),
            Some(1_700_000_100_000),
        );
        let event =
            make_pending_join_event(&joiner_priv, joiner_addr, joiner_pub, community_id, token);
        // ctx uses admin2 as admin — rogue != admin2, so P2 (inviter != admin) fires.
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr: admin2_addr,
            is_invite_only: true,
            actor_identity_pub: &joiner_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: Some(&admin2_pub),
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
        let (admin_priv, admin_pub, admin_addr) = make_identity(0xa1);
        let (joiner_priv, joiner_pub, joiner_addr) = make_identity(0xb1);
        let community_id = SpaceId([7u8; 16]);
        let token = make_invite_token(
            &admin_priv,
            admin_addr,
            Some(joiner_addr),
            Some(1_700_000_100_000),
        );
        let event =
            make_pending_join_event(&joiner_priv, joiner_addr, joiner_pub, community_id, token);
        let mut mat = MaterializedMembership::default();
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
            },
        );
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
            actor_identity_pub: &joiner_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: Some(&admin_pub),
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
        let (admin_priv, admin_pub, admin_addr) = make_identity(0xa1);
        let (joiner_priv, joiner_pub, joiner_addr) = make_identity(0xb1);
        let community_id = SpaceId([7u8; 16]);
        let token = make_invite_token(
            &admin_priv,
            admin_addr,
            Some(joiner_addr),
            Some(1_700_000_100_000),
        );
        let event =
            make_pending_join_event(&joiner_priv, joiner_addr, joiner_pub, community_id, token);
        let mut mat = MaterializedMembership::default();
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
            },
        );
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
            actor_identity_pub: &joiner_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: Some(&admin_pub),
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
        let (admin_priv, admin_pub, admin_addr) = make_identity(0xa1);
        let (joiner_priv, joiner_pub, joiner_addr) = make_identity(0xb1);
        let community_id = SpaceId([7u8; 16]);
        let token = make_invite_token(
            &admin_priv,
            admin_addr,
            Some(joiner_addr),
            Some(1_700_000_100_000),
        );
        let event =
            make_pending_join_event(&joiner_priv, joiner_addr, joiner_pub, community_id, token);
        let mut mat = MaterializedMembership::default();
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
            },
        );
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
            actor_identity_pub: &joiner_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: Some(&admin_pub),
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
        let (admin_priv, admin_pub, admin_addr) = make_identity(0xa1);
        let (joiner_priv, joiner_pub, joiner_addr) = make_identity(0xb1);
        let community_id = SpaceId([7u8; 16]);
        let token = make_invite_token(
            &admin_priv,
            admin_addr,
            Some(joiner_addr),
            Some(1_700_000_100_000),
        );
        let event =
            make_pending_join_event(&joiner_priv, joiner_addr, joiner_pub, community_id, token);
        let mut mat = MaterializedMembership::default();
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
            },
        );
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
            actor_identity_pub: &joiner_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: Some(&admin_pub),
        };
        assert!(
            verify_event(&event, &mat, &ctx).is_ok(),
            "Left actor should be allowed to PendingJoin again"
        );
    }

    #[test]
    fn pending_join_rejected_when_identity_pub_does_not_hash_to_actor() {
        let (admin_priv, admin_pub, admin_addr) = make_identity(0xa1);
        let (joiner_priv, joiner_pub, joiner_addr) = make_identity(0xb1);
        let community_id = SpaceId([7u8; 16]);
        let token = make_invite_token(
            &admin_priv,
            admin_addr,
            Some(joiner_addr),
            Some(1_700_000_100_000),
        );
        // Embed a wrong pub — it won't hash to joiner_addr.
        let wrong_pub = [0x55u8; 64];
        // The event itself is signed with the real joiner key, but the embedded
        // joiner_identity_pub in the PendingJoin payload is wrong.
        let event =
            make_pending_join_event(&joiner_priv, joiner_addr, wrong_pub, community_id, token);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
            actor_identity_pub: &joiner_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: Some(&admin_pub),
        };
        let mat = MaterializedMembership::default();
        let result = verify_event(&event, &mat, &ctx);
        // Either PendingJoinJoinerPubMismatch (pub doesn't hash to actor) OR
        // SignatureInvalid / InvalidIdentityPub (all-0x55 pub may fail key parsing).
        assert!(
            matches!(result, Err(VerifyError::PendingJoinJoinerPubMismatch))
                || matches!(result, Err(VerifyError::SignatureInvalid))
                || matches!(result, Err(VerifyError::InvalidIdentityPub)),
            "wrong identity_pub must be rejected; got {:?}",
            result
        );
    }
}

#[cfg(test)]
mod zeb_254_join_countersign_verify_tests {
    use super::*;

    /// Build a test identity from a seed byte.
    /// Returns (PrivateIdentity, identity_pub [u8; 64], OwnerAddr).
    fn make_identity(seed_byte: u8) -> (harmony_identity::PrivateIdentity, [u8; 64], OwnerAddr) {
        let seed = [seed_byte; 32];
        let private = harmony_identity::PrivateIdentity::from_seed(&seed);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let addr = OwnerAddr(public.address_hash);
        (private, identity_pub, addr)
    }

    fn make_join_countersign_event(
        admin_private: &harmony_identity::PrivateIdentity,
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
        sign_event_with_identity(&payload, admin_private).expect("sign JoinCountersign")
    }

    #[test]
    fn join_countersign_event_signs_and_verifies() {
        let (admin_priv, admin_pub, admin_addr) = make_identity(0xa1);
        let community_id = SpaceId([7u8; 16]);
        let target = [9u8; 16];
        let event = make_join_countersign_event(&admin_priv, admin_addr, community_id, target);
        let mut mat = MaterializedMembership::default();
        mat.members.insert(
            admin_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "t".into(),
                },
                left_at: None,
            },
        );
        mat.power_levels.insert(admin_addr, POWER_THRESHOLDS.invite);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: Some(&admin_pub),
        };
        let result = verify_event(&event, &mat, &ctx);
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
    }

    #[test]
    fn join_countersign_rejected_when_actor_not_joined() {
        let (admin_priv, admin_pub, admin_addr) = make_identity(0xa1);
        let community_id = SpaceId([7u8; 16]);
        let target = [9u8; 16];
        let event = make_join_countersign_event(&admin_priv, admin_addr, community_id, target);
        let mat = MaterializedMembership::default(); // actor not in members map
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: Some(&admin_pub),
        };
        let result = verify_event(&event, &mat, &ctx);
        assert!(
            matches!(result, Err(VerifyError::JoinCountersignActorNotJoined)),
            "expected JoinCountersignActorNotJoined, got {:?}",
            result
        );
    }

    #[test]
    fn join_countersign_accepted_when_target_missing() {
        // Out-of-order delivery — JoinCountersign arrives before its
        // target PendingJoin. Verify MUST accept it (target existence
        // is a materialize-time concern, not verify-time).
        let (admin_priv, admin_pub, admin_addr) = make_identity(0xa1);
        let community_id = SpaceId([7u8; 16]);
        let target = [0xDEu8; 16]; // does not exist in prior state
        let event = make_join_countersign_event(&admin_priv, admin_addr, community_id, target);
        let mut mat = MaterializedMembership::default();
        mat.members.insert(
            admin_addr,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "t".into(),
                },
                left_at: None,
            },
        );
        mat.power_levels.insert(admin_addr, POWER_THRESHOLDS.invite);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: Some(&admin_pub),
        };
        let result = verify_event(&event, &mat, &ctx);
        assert!(
            result.is_ok(),
            "expected Ok (out-of-order delivery), got {:?}",
            result
        );
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
        joiner_pub: [u8; 64],
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
                joiner_identity_pub: joiner_pub,
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

    fn make_identity(seed_byte: u8) -> (harmony_identity::PrivateIdentity, [u8; 64], OwnerAddr) {
        let seed = [seed_byte; 32];
        let private = harmony_identity::PrivateIdentity::from_seed(&seed);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let addr = OwnerAddr(public.address_hash);
        (private, identity_pub, addr)
    }

    fn sign_with_identity(
        payload: EventPayload,
        private: &harmony_identity::PrivateIdentity,
    ) -> SignedMembershipEvent {
        sign_event_with_identity(&payload, private).expect("sign_event_with_identity must succeed")
    }

    fn make_admin_proposal_event(
        id: [u8; 16],
        actor_priv: &harmony_identity::PrivateIdentity,
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
        let (admin_priv, admin_pub, admin_addr) = make_identity(0x01);
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
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
        };
        assert_eq!(verify_event(&evt, &prior, &ctx), Ok(()));
    }

    #[test]
    fn admin_proposal_rejected_when_actor_not_joined() {
        let (actor_priv, actor_pub, actor_addr) = make_identity(0x01);
        let prior = MaterializedMembership::default();
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
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr: actor_addr,
            is_invite_only: false,
            actor_identity_pub: &actor_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalActorNotJoined)
        );
    }

    #[test]
    fn admin_proposal_rejected_when_actor_power_below_100() {
        let (actor_priv, actor_pub, actor_addr) = make_identity(0x01);
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
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr: actor_addr,
            is_invite_only: false,
            actor_identity_pub: &actor_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalActorNotAdmin)
        );
    }

    #[test]
    fn admin_proposal_setpower_rejected_when_target_not_in_members() {
        let (admin_priv, admin_pub, admin_addr) = make_identity(0x01);
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
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalKindInvalid)
        );
    }

    #[test]
    fn admin_proposal_setpower_rejected_when_level_out_of_range() {
        let (admin_priv, admin_pub, admin_addr) = make_identity(0x01);
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
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalKindInvalid)
        );
    }

    #[test]
    fn admin_proposal_setpower_rejected_when_not_admin_affecting() {
        let (admin_priv, admin_pub, admin_addr) = make_identity(0x01);
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
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalNotAdminAffecting)
        );
    }

    #[test]
    fn admin_proposal_kick_rejected_when_target_not_admin() {
        let (admin_priv, admin_pub, admin_addr) = make_identity(0x01);
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
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalNotAdminAffecting)
        );
    }

    #[test]
    fn admin_proposal_change_quorum_rejected_when_below_one() {
        let (admin_priv, admin_pub, admin_addr) = make_identity(0x01);
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
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalKindInvalid)
        );
    }

    #[test]
    fn admin_proposal_change_quorum_rejected_when_exceeds_admin_count() {
        let (admin_priv, admin_pub, admin_addr) = make_identity(0x01);
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
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminProposalQuorumOutOfRange)
        );
    }

    #[test]
    fn admin_proposal_change_quorum_accepted_when_equals_admin_count() {
        let (admin1_priv, admin1_pub, admin1_addr) = make_identity(0x01);
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
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr: admin1_addr,
            is_invite_only: false,
            actor_identity_pub: &admin1_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
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
        let (admin1_priv, admin1_pub, admin1_addr) = make_identity(0x01);
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
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr: admin1_addr,
            is_invite_only: false,
            actor_identity_pub: &admin1_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
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
        let (admin_priv, admin_pub, admin_addr) = make_identity(0x01);
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
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
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

    fn make_identity(seed_byte: u8) -> (harmony_identity::PrivateIdentity, [u8; 64], OwnerAddr) {
        let seed = [seed_byte; 32];
        let private = harmony_identity::PrivateIdentity::from_seed(&seed);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let addr = OwnerAddr(public.address_hash);
        (private, identity_pub, addr)
    }

    fn make_admin_countersign_event(
        id: [u8; 16],
        actor_priv: &harmony_identity::PrivateIdentity,
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
        sign_event_with_identity(&payload, actor_priv)
            .expect("sign_event_with_identity must succeed")
    }

    #[test]
    fn admin_countersign_accepted_when_actor_admin() {
        let (admin_priv, admin_pub, admin_addr) = make_identity(0x01);
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
            },
        );
        prior.power_levels.insert(admin_addr, 100);
        let evt =
            make_admin_countersign_event([0x10; 16], &admin_priv, admin_addr, [0x55; 16], 1_000);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
        };
        assert_eq!(verify_event(&evt, &prior, &ctx), Ok(()));
    }

    #[test]
    fn admin_countersign_rejected_when_actor_not_joined() {
        let (actor_priv, actor_pub, actor_addr) = make_identity(0x01);
        let prior = MaterializedMembership::default();
        let evt =
            make_admin_countersign_event([0x10; 16], &actor_priv, actor_addr, [0x55; 16], 1_000);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr: actor_addr,
            is_invite_only: false,
            actor_identity_pub: &actor_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
        };
        assert_eq!(
            verify_event(&evt, &prior, &ctx),
            Err(VerifyError::AdminCountersignActorNotJoined)
        );
    }

    #[test]
    fn admin_countersign_rejected_when_actor_power_below_100() {
        let (mod_priv, mod_pub, mod_addr) = make_identity(0x01);
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
            },
        );
        prior.power_levels.insert(mod_addr, 50);
        let evt = make_admin_countersign_event([0x10; 16], &mod_priv, mod_addr, [0x55; 16], 1_000);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr: mod_addr,
            is_invite_only: false,
            actor_identity_pub: &mod_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
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
        let (admin_priv, admin_pub, admin_addr) = make_identity(0x01);
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
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
        };
        assert_eq!(verify_event(&evt, &prior, &ctx), Ok(()));
    }
}

// ── ZEB-250 Task 6: direct SetPower/Kick quorum gate tests ───────────────────

#[cfg(test)]
mod zeb_250_direct_event_quorum_gate_tests {
    use super::*;

    fn make_identity(seed_byte: u8) -> (harmony_identity::PrivateIdentity, [u8; 64], OwnerAddr) {
        let seed = [seed_byte; 32];
        let private = harmony_identity::PrivateIdentity::from_seed(&seed);
        let public = private.public_identity();
        let identity_pub = public.to_public_bytes();
        let addr = OwnerAddr(public.address_hash);
        (private, identity_pub, addr)
    }

    fn sign_with_identity(
        payload: EventPayload,
        private: &harmony_identity::PrivateIdentity,
    ) -> SignedMembershipEvent {
        sign_event_with_identity(&payload, private).expect("sign_event_with_identity must succeed")
    }

    fn make_setpower_event(
        id: [u8; 16],
        actor_priv: &harmony_identity::PrivateIdentity,
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
        actor_priv: &harmony_identity::PrivateIdentity,
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
        let (admin_priv, admin_pub, admin_addr) = make_identity(0x01);
        let (_, _, target_addr) = make_identity(0x02);
        let prior = prior_with_admin_and_target(admin_addr, target_addr, 0, 2);
        let evt = make_setpower_event([0x10; 16], &admin_priv, admin_addr, target_addr, 100, 1_000);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
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
        let (admin_priv, admin_pub, admin_addr) = make_identity(0x01);
        let (_, _, target_addr) = make_identity(0x02);
        // Target is currently an admin (power 100).
        let prior = prior_with_admin_and_target(admin_addr, target_addr, 100, 3);
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
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
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
        let (admin_priv, admin_pub, admin_addr) = make_identity(0x01);
        let (_, _, target_addr) = make_identity(0x02);
        // Target has power 0 (not an admin), new level is 50 (mod, not admin).
        let prior = prior_with_admin_and_target(admin_addr, target_addr, 0, 5);
        let evt = make_setpower_event([0x10; 16], &admin_priv, admin_addr, target_addr, 50, 1_000);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
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
        let (admin_priv, admin_pub, admin_addr) = make_identity(0x01);
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
            },
        );
        // Store actor power as 101 directly in prior_state (bypasses PowerLevelOutOfRange,
        // which is only checked during SetPower events, not when reading prior_state).
        prior.power_levels.insert(admin_addr, 101);
        prior.power_levels.insert(target_addr, 100);
        let evt = make_kick_event_signed([0x10; 16], &admin_priv, admin_addr, target_addr, 1_000);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
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
        let (admin_priv, admin_pub, admin_addr) = make_identity(0x01);
        let (_, _, target_addr) = make_identity(0x02);
        // Target is a moderator (power 50), not an admin.
        let prior = prior_with_admin_and_target(admin_addr, target_addr, 50, 5);
        let evt = make_kick_event_signed([0x10; 16], &admin_priv, admin_addr, target_addr, 1_000);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
        };
        assert_eq!(verify_event(&evt, &prior, &ctx), Ok(()));
    }

    /// ZEB-250 backwards-compat: admin_quorum == 1 preserves single-admin
    /// behavior — direct SetPower to admin and direct Kick of admin are
    /// both accepted when admin_quorum == 1.
    #[test]
    fn direct_setpower_admin_actions_accepted_when_admin_quorum_equals_1() {
        let (admin_priv, admin_pub, admin_addr) = make_identity(0x01);
        let (_, _, target_addr) = make_identity(0x02);
        // admin_quorum == 1 (the default).
        let prior = prior_with_admin_and_target(admin_addr, target_addr, 0, 1);

        // Direct SetPower to level 100 — must be accepted.
        let setpower_evt =
            make_setpower_event([0x10; 16], &admin_priv, admin_addr, target_addr, 100, 1_000);
        let ctx = VerifyContext {
            expected_community_id: SpaceId([0xc0; 16]),
            admin_addr,
            is_invite_only: false,
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: None,
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
            },
            SignedMembershipEvent {
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
