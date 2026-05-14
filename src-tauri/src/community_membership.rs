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
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    let mut m = MaterializedMembership::default();

    // Bootstrap: admin holds power 100 implicitly. SetPower events
    // (replayed below) can override.
    m.power_levels.insert(admin_addr, 100);

    // Sort by the canonical total order. We don't assume the input
    // is sorted because DAG-sync delivers events partial-ordered.
    // Cloning the &-refs is fine — the event vec is small (community
    // sizes are bounded; even very active communities have O(thousands)
    // of events at the long tail, not millions).
    let mut sorted: Vec<&SignedMembershipEvent> = events.iter().collect();
    sorted.sort_by(|a, b| event_sort_key(a).cmp(&event_sort_key(b)));

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
                    None | Some(MemberStatus::Invited) | Some(MemberStatus::Left) => true,
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
                    | Some(MemberStatus::Banned) => false,
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
                if let Some(s) = m.members.get_mut(target) {
                    s.status = MemberStatus::Banned;
                    s.left_at = Some(event.at.clone());
                }
                // ZEB-249: track that this kick needs a matching EpochRotation.
                // The self-healing observer synthesizes one if the bundled
                // rotation didn't land (e.g., concurrent-kick contention).
                m.pending_rotation_for.insert(*target);
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
                let triggered_event = sorted[..idx].iter().find(|e| e.id == *triggered_by);
                let join_actor = match triggered_event.map(|e| &e.kind) {
                    Some(MembershipEventKind::Join) => triggered_event.map(|e| e.actor),
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
    materialize(&prefix, admin_addr)
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
    materialize(&prefix, admin_addr)
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
            // Defense-in-depth: bound the reason string at the CRDT layer
            // so a malicious peer can't bypass the UI cap and persist a
            // giant reason on every replica.
            if let Some(r) = reason {
                if r.chars().count() > MAX_MODERATION_REASON_CHARS {
                    return Err(VerifyError::ReasonTooLong);
                }
            }
        }
        MembershipEventKind::SetPower { level, .. } => {
            if actor_power < POWER_THRESHOLDS.set_power {
                return Err(VerifyError::ActorPowerInsufficient);
            }
            if *level > POWER_THRESHOLDS.max {
                return Err(VerifyError::PowerLevelOutOfRange);
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
    verify_signature(event, signer_pub)

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
}
