//! Community membership CRDT primitives — ZEB-217 Sub-C Phase 1.
//!
//! Per-community signed-event CRDT replicated via the encrypted Zenoh
//! state-root topic (Phase 2). Phase 1 ships only the types,
//! materialization rules, and verification logic — no IPC, no
//! networking, no UI.
//!
//! See `docs/specs/2026-05-05-zeb-217-sub-c-communities-design.md`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::OwnerAddr;

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
    /// `ChannelCreate` event references a `channel_id` that already
    /// exists in `prior_state.channels` (with or without `deleted_at`
    /// set). Duplicate channel-ids violate create semantics — the IPC
    /// layer's first call returns InsertOutcome::Inserted for the first
    /// event and (without this gate) silently no-ops the second in
    /// materialize, leaving the caller thinking creation succeeded.
    /// Reject at verify time so the IPC surfaces a clean "channel
    /// already exists" error.
    ChannelIdAlreadyExists,

    /// `ChannelDelete` event references a `channel_id` that has no
    /// entry in `prior_state.channels`. Channels are tombstoned (not
    /// removed), so a missing entry means the channel was never
    /// created. Surfaces a clean "no such channel" error at IPC level.
    /// Note: `ChannelModify` on unknown channel is intentionally NOT
    /// rejected (DAG-sync may reorder Modify before Create — materialize
    /// safely no-ops).
    ChannelNotFound,

    /// `ChannelDelete` event references a channel whose `deleted_at` is
    /// already set. Closes the TOCTOU between `delete_channel` IPC's
    /// preflight check and the engine insert; surfaces a clean "already
    /// deleted" error.
    ChannelAlreadyDeleted,

    /// `ChannelModify` event has both `name: None` and `write_power: None`
    /// — a no-op the IPC layer would normally reject before signing, but
    /// remote peers can still submit signed no-ops. Reject at verify
    /// time to avoid bloating the CRDT log with effectless events.
    ChannelModifyNoOp,

    /// `ChannelCreate` or `ChannelModify` carries a `name` that is
    /// empty/whitespace-only or exceeds 32 chars (per spec §12.3).
    /// Receive-side enforcement so a malicious peer can't replicate
    /// invalid names that would break the UI.
    ChannelNameInvalid,

    EncodeError(String),
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
                    "invite target is currently Banned (admin must unban first; \
                     not yet implemented in v1)"
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
            VerifyError::ChannelIdAlreadyExists => {
                write!(f, "ChannelCreate channel_id already exists in materialized state")
            }
            VerifyError::ChannelNotFound => write!(f, "ChannelDelete channel_id not in materialized state"),
            VerifyError::ChannelAlreadyDeleted => write!(f, "ChannelDelete on already-tombstoned channel"),
            VerifyError::ChannelModifyNoOp => {
                write!(f, "ChannelModify with both name and write_power None (no-op rejected at verify)")
            }
            VerifyError::ChannelNameInvalid => write!(
                f,
                "channel name is empty or exceeds 32 chars (spec §12.3 limit)"
            ),
            VerifyError::EncodeError(s) => write!(f, "canonical encode failed: {s}"),
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
    pub channels: BTreeMap<ChannelId, ChannelInfo>,
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

    for event in sorted {
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
                }
            }
            MembershipEventKind::Leave => {
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
                    }
                }
                // If actor never joined, Leave is silently no-op.
                // verify_event tolerates this case (no rejection) so
                // the materialization path stays simple — the
                // alternative (insert-with-Left) would corrupt state
                // from a malformed event.
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
            }
            MembershipEventKind::SetPower { target, level } => {
                m.power_levels.insert(*target, *level);
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
        | MembershipEventKind::SetPower { .. } => {
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
        MembershipEventKind::Kick { target, .. } => {
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
        }
        MembershipEventKind::SetPower { level, .. } => {
            if actor_power < POWER_THRESHOLDS.set_power {
                return Err(VerifyError::ActorPowerInsufficient);
            }
            if *level > POWER_THRESHOLDS.max {
                return Err(VerifyError::PowerLevelOutOfRange);
            }
        }
        MembershipEventKind::ChannelCreate {
            channel_id,
            name,
            write_power,
        } => {
            if actor_power < POWER_THRESHOLDS.kick {
                return Err(VerifyError::ChannelAdminInsufficientPower);
            }
            // Reject duplicate channel_id (CodeAnt #1).
            if prior_state.channels.contains_key(channel_id) {
                return Err(VerifyError::ChannelIdAlreadyExists);
            }
            // Validate name length (1-32 chars per spec §12.3).
            if name.trim().is_empty() || name.chars().count() > 32 {
                return Err(VerifyError::ChannelNameInvalid);
            }
            // Validate write_power range.
            if *write_power > POWER_THRESHOLDS.max {
                return Err(VerifyError::PowerLevelOutOfRange);
            }
        }
        MembershipEventKind::ChannelModify {
            channel_id: _,
            name,
            write_power,
        } => {
            if actor_power < POWER_THRESHOLDS.kick {
                return Err(VerifyError::ChannelAdminInsufficientPower);
            }
            // Reject all-None ChannelModify (no-op signal, malformed).
            if name.is_none() && write_power.is_none() {
                return Err(VerifyError::ChannelModifyNoOp);
            }
            // Validate name length when Some.
            if let Some(n) = name {
                if n.trim().is_empty() || n.chars().count() > 32 {
                    return Err(VerifyError::ChannelNameInvalid);
                }
            }
            // Validate write_power range when Some.
            if let Some(wp) = write_power {
                if *wp > POWER_THRESHOLDS.max {
                    return Err(VerifyError::PowerLevelOutOfRange);
                }
            }
            // Note: ChannelModify on unknown channel_id is intentionally
            // ALLOWED — DAG-sync may deliver Modify before Create;
            // materialize safely no-ops on unknown.
        }
        MembershipEventKind::ChannelDelete { channel_id } => {
            if actor_power < POWER_THRESHOLDS.kick {
                return Err(VerifyError::ChannelAdminInsufficientPower);
            }
            // Reject delete on unknown channel (clean error).
            let channel = prior_state
                .channels
                .get(channel_id)
                .ok_or(VerifyError::ChannelNotFound)?;
            // Reject delete on already-tombstoned (closes TOCTOU between
            // delete_channel IPC preflight and the engine insert).
            if channel.deleted_at.is_some() {
                return Err(VerifyError::ChannelAlreadyDeleted);
            }
        }
    }

    Ok(())
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
