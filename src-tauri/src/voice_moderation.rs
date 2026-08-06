//! ZEB-358 community voice moderation: power-gated server-mute + remove-from-
//! voice. A device-#2-signed, ChannelKey-sealed directive rides a dedicated
//! Zenoh control topic (never the CRDT); honest clients enforce it receiver-
//! side (drop the target's audio + hide/flag them). Mute and kick are the same
//! time-boxed directive. Mirrors `voice_presence.rs`.

use crate::community_membership::ChannelId;
use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use std::collections::BTreeMap;

/// Liveness TTL: a directive stays effective this long after last receipt.
pub const ENFORCE_TTL_MS: u64 = 12_000;
/// Issuer re-publishes each active directive this often (< ENFORCE_TTL_MS).
pub const RE_ASSERT_INTERVAL_MS: u64 = 4_000;
/// Default moderator-chosen duration (5 min), enforced issuer-side.
pub const DEFAULT_MODERATION_MS: u64 = 300_000;
/// ZEB-612: issuer-side re-assert window for an unclaimed invite-to-speak
/// (~2 min; the invite's default `duration_ms`, analog of
/// `DEFAULT_MODERATION_MS`). Receive-side liveness stays `ENFORCE_TTL_MS`
/// refreshed by the 4 s re-asserts, so an invite dies ≤12 s after the issuer
/// stops re-asserting (window expiry or issuer departure).
pub const INVITE_TTL_MS: u64 = 120_000;
/// Minimum power to moderate (reuses the existing `kick` threshold).
pub const MOD_POWER: u8 = 50;

/// What a directive asserts about the target owner. `serde_repr` encodes each
/// variant as its bare u8 discriminant (mirrors `ChannelKind`) and rejects
/// unknown discriminants on decode — a pre-ZEB-612 client drops an
/// `InviteToSpeak` directive at decode (contained; invites are cosmetic for
/// it anyway).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum ModAction {
    Mute = 0,
    Unmute = 1,
    Kick = 2,
    Unkick = 3,
    /// ZEB-612 Town Hall: a mod invites `target_owner` to speak. Benign (no
    /// enforcement AGAINST the target): surfaces on the overlay as
    /// `invitedOwners`; the TARGET's own client lowers its hand on accept or
    /// dismiss — the invite never mutates another owner's beacon. No revoke
    /// variant: an unclaimed invite lapses via the TTL discipline.
    InviteToSpeak = 4,
}

/// Enforcement class: each class is an independent last-writer-wins slot per
/// target inside [`ActiveModeration`], so an invite can never clobber a
/// mute/kick (and vice versa). Also keys the issuer's re-assert map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ModClass {
    Mute,
    Kick,
    Invite,
}

impl ModAction {
    /// The enforcement class this action belongs to.
    pub fn class(self) -> ModClass {
        match self {
            ModAction::Mute | ModAction::Unmute => ModClass::Mute,
            ModAction::Kick | ModAction::Unkick => ModClass::Kick,
            ModAction::InviteToSpeak => ModClass::Invite,
        }
    }
    /// True for the "positive" directives that turn enforcement ON.
    pub fn enforces(self) -> bool {
        matches!(
            self,
            ModAction::Mute | ModAction::Kick | ModAction::InviteToSpeak
        )
    }
}

/// Unsigned directive. Canonical CBOR, 2-char keys (same-length invariant).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceModerationDirective {
    #[serde(
        rename = "ao",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub actor_owner: [u8; 16],
    #[serde(
        rename = "ad",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub actor_device: [u8; 32],
    #[serde(
        rename = "to",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub target_owner: [u8; 16],
    #[serde(rename = "ac")]
    pub action: ModAction,
    #[serde(rename = "ih")]
    pub issued_hlc: Hlc,
    #[serde(rename = "sq")]
    pub seq: u64,
}

/// Directive + detached device-#2 signature over `canonical_cbor_encode(directive)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedVoiceModerationDirective {
    #[serde(rename = "dr")]
    pub directive: VoiceModerationDirective,
    #[serde(
        rename = "sg",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],
}

impl crate::owner_state_crypto::sealed::CanonicalPayloadSealed for ModAction {}
impl crate::owner_state_crypto::CanonicalPayload for ModAction {}
impl crate::owner_state_crypto::sealed::CanonicalPayloadSealed for VoiceModerationDirective {}
impl crate::owner_state_crypto::CanonicalPayload for VoiceModerationDirective {}
impl crate::owner_state_crypto::sealed::CanonicalPayloadSealed for SignedVoiceModerationDirective {}
impl crate::owner_state_crypto::CanonicalPayload for SignedVoiceModerationDirective {}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModError {
    /// Canonical-CBOR encoding of the directive failed (a serialization bug).
    #[error("directive CBOR encode failed")]
    Encode,
    /// AEAD seal under the channel key failed — a crypto/runtime fault, kept
    /// distinct from `Encode` so a seal failure can be diagnosed separately.
    #[error("directive seal failed")]
    Seal,
    #[error("directive signature invalid")]
    BadSig,
    #[error("signer is not an enrolled, joined member")]
    NotMember,
    #[error("signer lacks moderation power over target")]
    NotAuthorized,
    /// `session.put` failed — a transport/runtime fault, distinct from an
    /// encode/seal fault, so callers can diagnose network failures separately.
    #[error("directive transport publish failed")]
    Publish,
}

use crate::community_channel_log::ChannelKey;
use crate::owner_state_crypto::canonical_cbor_encode;
use crate::voice_crypto::{decrypt_voice_packet, encrypt_voice_packet, VOICE_MODERATION_AAD};

/// Sign a directive with the device-#2 ed25519 key. The signature covers the
/// canonical CBOR of the unsigned directive (the `sig` field is excluded by
/// construction). Mirrors `voice_presence::sign_presence_beacon`.
pub fn sign_directive(
    directive: VoiceModerationDirective,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<SignedVoiceModerationDirective, ModError> {
    use ed25519_dalek::Signer;
    let bytes = canonical_cbor_encode(&directive).map_err(|_| ModError::Encode)?;
    let sig = signing_key.sign(&bytes).to_bytes();
    Ok(SignedVoiceModerationDirective { directive, sig })
}

/// Verify the detached signature against the verifying key embedded in
/// `directive.actor_device`. This proves the holder of that device key signed
/// it; authority (device ∈ actor's enrolled keys + power) is checked separately
/// in `verify_directive_authority` (Task 3).
pub fn verify_directive_sig(signed: &SignedVoiceModerationDirective) -> Result<(), ModError> {
    let bytes = canonical_cbor_encode(&signed.directive).map_err(|_| ModError::Encode)?;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&signed.directive.actor_device)
        .map_err(|_| ModError::BadSig)?;
    let sig = ed25519_dalek::Signature::from_bytes(&signed.sig);
    vk.verify_strict(&bytes, &sig).map_err(|_| ModError::BadSig)
}

/// Seal a signed directive under the channel key for transport. Output framing
/// matches the voice media/presence packet (`[12B nonce][ct+tag]`), distinct
/// AAD (`VOICE_MODERATION_AAD ‖ community ‖ channel`).
pub fn seal_directive(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    signed: &SignedVoiceModerationDirective,
) -> Result<Vec<u8>, ModError> {
    let plain = canonical_cbor_encode(signed).map_err(|_| ModError::Encode)?;
    encrypt_voice_packet(key, community, channel, VOICE_MODERATION_AAD, &plain)
        .map_err(|_| ModError::Seal)
}

/// Open + decode a sealed directive. Returns `None` on any failure (drop) —
/// wrong key, wrong (community, channel) scope, tampered ciphertext, or bad CBOR.
pub fn open_directive(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    packet: &[u8],
) -> Option<SignedVoiceModerationDirective> {
    let plain = decrypt_voice_packet(key, community, channel, VOICE_MODERATION_AAD, packet).ok()?;
    ciborium::from_reader(plain.as_slice()).ok()
}

/// The middle of the 5-step receive pipeline (step 1 = open under ChannelKey,
/// done by the caller; step 5 = `ActiveModeration::apply`). This function runs,
/// in order: the device-#2 **signature** check; the **membership** check —
/// `actor_device ∈ actor_owner.enrolled_device_keys` AND `actor_owner` is Joined,
/// via `device_is_enrolled`; and the **power** gate — `power(actor) ≥ MOD_POWER`
/// AND, for the punitive actions only, `power(actor) > power(target)` (the `>`
/// rejects equal-power peers and makes self-moderation impossible; the benign
/// `InviteToSpeak` skips that clause — ZEB-612).
pub fn verify_directive_authority(
    materialized: &crate::community_membership::MaterializedMembership,
    signed: &SignedVoiceModerationDirective,
) -> Result<(), ModError> {
    verify_directive_sig(signed)?;
    let d = &signed.directive;
    if !crate::voice_presence::device_is_enrolled(
        materialized,
        &OwnerAddr(d.actor_owner),
        &d.actor_device,
    ) {
        return Err(ModError::NotMember);
    }
    let actor_power = materialized
        .power_levels
        .get(&OwnerAddr(d.actor_owner))
        .copied()
        .unwrap_or(0);
    let target_power = materialized
        .power_levels
        .get(&OwnerAddr(d.target_owner))
        .copied()
        .unwrap_or(0);
    if actor_power < MOD_POWER {
        return Err(ModError::NotAuthorized);
    }
    // The `actor > target` clause is punitive-action logic (blocks equal-power
    // retaliation and self-moderation). The benign invite-to-speak skips it: a
    // mod may invite an equal- or higher-power member to speak (spec §5).
    if d.action != ModAction::InviteToSpeak && actor_power <= target_power {
        return Err(ModError::NotAuthorized);
    }
    Ok(())
}

/// Async wrapper resolving materialized membership off the registry (mirrors
/// `voice_presence::beacon_signer_is_member`), then applying the authority
/// check. Returns Ok(()) only if the directive's signer is authorized.
pub async fn directive_signer_is_authorized(
    registry: &std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
    community: &SpaceId,
    signed: &SignedVoiceModerationDirective,
) -> Result<(), ModError> {
    let Some(engine) = registry.engine_arc(community).await else {
        return Err(ModError::NotMember);
    };
    let admin = engine.admin_addr();
    let state = engine.state();
    let materialized = {
        let guard = state.lock().await;
        guard.materialized(admin)
    };
    verify_directive_authority(&materialized, signed)
}

#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn seal_directive_with_nonce(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    signed: &SignedVoiceModerationDirective,
    nonce: [u8; 12],
) -> Result<Vec<u8>, ModError> {
    let plain = canonical_cbor_encode(signed).map_err(|_| ModError::Encode)?;
    crate::voice_crypto::encrypt_voice_packet_with_nonce(
        key,
        community,
        channel,
        VOICE_MODERATION_AAD,
        &plain,
        nonce,
    )
    .map_err(|_| ModError::Seal)
}

#[derive(Debug, Clone)]
struct ClassState {
    latest_hlc: Hlc,
    latest_seq: u64,
    enforced: bool,
    enforce_until_ms: u64,
}

#[derive(Debug, Clone, Default)]
struct TargetState {
    mute: Option<ClassState>,
    kick: Option<ClassState>,
    /// ZEB-612: invite-to-speak (single positive action, lapses via TTL).
    invite: Option<ClassState>,
}

impl TargetState {
    fn slot_mut(&mut self, class: ModClass) -> &mut Option<ClassState> {
        match class {
            ModClass::Mute => &mut self.mute,
            ModClass::Kick => &mut self.kick,
            ModClass::Invite => &mut self.invite,
        }
    }
    fn slot(&self, class: ModClass) -> &Option<ClassState> {
        match class {
            ModClass::Mute => &self.mute,
            ModClass::Kick => &self.kick,
            ModClass::Invite => &self.invite,
        }
    }
    fn slots_mut(&mut self) -> [&mut Option<ClassState>; 3] {
        [&mut self.mute, &mut self.kick, &mut self.invite]
    }
    fn is_empty(&self) -> bool {
        self.mute.is_none() && self.kick.is_none() && self.invite.is_none()
    }
}

/// The currently-enforced targets of one `(community, channel)`, one owner
/// list per enforcement class. Returned by [`ActiveModeration::snapshot`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModerationSnapshot {
    pub muted: Vec<[u8; 16]>,
    pub kicked: Vec<[u8; 16]>,
    /// ZEB-612: owners holding an unexpired invite-to-speak.
    pub invited: Vec<[u8; 16]>,
}

/// In-memory enforcement state: which owners are currently muted/kicked/
/// invited-to-speak per (community, channel). Never persisted. Three
/// independent classes per target (mute-class {Mute,Unmute}, kick-class
/// {Kick,Unkick}, invite-class {InviteToSpeak}), each last-writer-wins
/// by `(issued_hlc, seq)`. Mirrors the freshness discipline of `VoicePresenceMap`.
#[derive(Debug, Default)]
pub struct ActiveModeration {
    inner: BTreeMap<(SpaceId, ChannelId), BTreeMap<[u8; 16], TargetState>>,
}

/// True iff `(a_hlc, a_seq)` is strictly newer than `(b_hlc, b_seq)` — HLC first
/// (via `is_strictly_newer_than`), seq as the tiebreak when the HLCs are equal.
fn strictly_newer(a_hlc: &Hlc, a_seq: u64, b_hlc: &Hlc, b_seq: u64) -> bool {
    if a_hlc.is_strictly_newer_than(b_hlc) {
        true
    } else if b_hlc.is_strictly_newer_than(a_hlc) {
        false
    } else {
        a_seq > b_seq
    }
}

impl ActiveModeration {
    /// Apply a (verified) directive. Returns true iff the *effective* state
    /// (enforced or not) changed, so the caller re-emits the roster. Stale
    /// directives are ignored; an exact re-assert refreshes the TTL only.
    pub fn apply(
        &mut self,
        c: &SpaceId,
        ch: &ChannelId,
        d: &VoiceModerationDirective,
        now_ms: u64,
        ttl_ms: u64,
    ) -> bool {
        let target = self
            .inner
            .entry((*c, *ch))
            .or_default()
            .entry(d.target_owner)
            .or_default();
        let slot = target.slot_mut(d.action.class());
        match slot.as_ref() {
            Some(s) => {
                let is_same = !d.issued_hlc.is_strictly_newer_than(&s.latest_hlc)
                    && !s.latest_hlc.is_strictly_newer_than(&d.issued_hlc)
                    && d.seq == s.latest_seq;
                if is_same {
                    // Re-assert of the current directive: refresh TTL, no flip.
                    if s.enforced {
                        let until = now_ms.saturating_add(ttl_ms);
                        slot.as_mut().expect("slot is Some").enforce_until_ms = until;
                    }
                    false
                } else if strictly_newer(&d.issued_hlc, d.seq, &s.latest_hlc, s.latest_seq) {
                    let was = s.enforced;
                    let enforced = d.action.enforces();
                    *slot = Some(ClassState {
                        latest_hlc: d.issued_hlc.clone(),
                        latest_seq: d.seq,
                        enforced,
                        enforce_until_ms: now_ms.saturating_add(ttl_ms),
                    });
                    was != enforced
                } else {
                    false // stale → ignore (tombstone retains latest order)
                }
            }
            None => {
                let enforced = d.action.enforces();
                *slot = Some(ClassState {
                    latest_hlc: d.issued_hlc.clone(),
                    latest_seq: d.seq,
                    enforced,
                    enforce_until_ms: now_ms.saturating_add(ttl_ms),
                });
                enforced // previously absent (== not enforced) → changed iff now enforced
            }
        }
    }

    /// ZEB-853 C2 ingest gate: reject a directive stamped implausibly far in the
    /// receiver's *future* before it can touch the LWW slot, then [`apply`]. This
    /// is the anti-freeze boundary — a same-slot `strictly_newer` LWW means a
    /// far-future `issued_hlc.wall_ms` wins forever and permanently freezes the
    /// `(community, channel, target, class)` slot (FAIL-OPEN). Mirrors the
    /// ZEB-846/847 forward-skew bounds in `community_membership.rs`.
    ///
    /// `now_wall_ms` is the receiver's own *wall-epoch* clock
    /// ([`crate::clock_trust::receiver_now_ms`]), used ONLY for the skew reject.
    /// `None` (unreadable / pre-epoch local clock) ⇒ apply-all (skip the gate): a
    /// bad LOCAL clock must never drop an honest directive. `now_ms` / `ttl_ms`
    /// are the *monotonic* liveness clock + TTL threaded straight into `apply` —
    /// never compared against the wall stamp.
    ///
    /// A directive whose `issued_hlc.wall_ms` exceeds
    /// [`crate::clock_trust::MAX_FORWARD_SKEW_MS`] (control tier, 5 min) beyond
    /// `now_wall_ms` is dropped: not applied, slot not advanced, returning
    /// `false` (apply's "no flip"). The gate runs BEFORE the `strictly_newer`
    /// comparison so poison never advances the slot.
    ///
    /// [`apply`]: ActiveModeration::apply
    pub fn apply_gated(
        &mut self,
        c: &SpaceId,
        ch: &ChannelId,
        d: &VoiceModerationDirective,
        now_ms: u64,
        ttl_ms: u64,
        now_wall_ms: Option<u64>,
    ) -> bool {
        if crate::clock_trust::wall_exceeds_forward_skew_logged(
            d.issued_hlc.wall_ms,
            now_wall_ms,
            "voice_moderation.directive.issued_hlc",
        ) {
            return false;
        }
        self.apply(c, ch, d, now_ms, ttl_ms)
    }

    fn is(
        &self,
        c: &SpaceId,
        ch: &ChannelId,
        owner: &[u8; 16],
        now_ms: u64,
        class: ModClass,
    ) -> bool {
        self.inner
            .get(&(*c, *ch))
            .and_then(|m| m.get(owner))
            .and_then(|t| t.slot(class).as_ref())
            .is_some_and(|s| s.enforced && now_ms < s.enforce_until_ms)
    }

    /// True iff `owner` is currently server-muted in `(c, ch)`.
    pub fn is_muted(&self, c: &SpaceId, ch: &ChannelId, owner: &[u8; 16], now_ms: u64) -> bool {
        self.is(c, ch, owner, now_ms, ModClass::Mute)
    }

    /// True iff `owner` is currently kicked from voice in `(c, ch)`.
    pub fn is_kicked(&self, c: &SpaceId, ch: &ChannelId, owner: &[u8; 16], now_ms: u64) -> bool {
        self.is(c, ch, owner, now_ms, ModClass::Kick)
    }

    /// ZEB-612: true iff `owner` holds an unexpired invite-to-speak in `(c, ch)`.
    pub fn is_invited(&self, c: &SpaceId, ch: &ChannelId, owner: &[u8; 16], now_ms: u64) -> bool {
        self.is(c, ch, owner, now_ms, ModClass::Invite)
    }

    /// Lapse any enforced class whose TTL passed; return the (community, channel)
    /// pairs whose effective state changed so the caller re-emits the roster.
    ///
    /// After lapsing, PRUNE long-expired class slots so rows don't accumulate
    /// forever: a `!enforced` slot is removed once `now_ms > enforce_until_ms +
    /// ENFORCE_TTL_MS` — a ≥2×TTL grace, so the LWW tombstone still blocks a
    /// reordered stale re-apply within the enforcement window (the
    /// `newer_unmute_clears_and_blocks_stale_mute` invariant). Then empty
    /// `TargetState`s (both classes None) and empty `(community, channel)` maps
    /// are dropped. Pruning never changes the *effective* state (a pruned slot was
    /// already un-enforced), so it does not affect the returned `changed` set.
    pub fn sweep(&mut self, now_ms: u64) -> Vec<(SpaceId, ChannelId)> {
        let mut changed = Vec::new();
        for (scope, targets) in self.inner.iter_mut() {
            let mut any = false;
            for t in targets.values_mut() {
                for s in t.slots_mut().into_iter().flatten() {
                    if s.enforced && now_ms >= s.enforce_until_ms {
                        s.enforced = false;
                        any = true;
                    }
                }
            }
            if any {
                changed.push(*scope);
            }
            // Prune long-expired (≥2×TTL) un-enforced slots, then drop emptied
            // target rows. The grace keeps the LWW tombstone alive long enough to
            // reject a network-reordered stale re-apply.
            for t in targets.values_mut() {
                for slot in t.slots_mut() {
                    let prune = slot.as_ref().is_some_and(|s| {
                        !s.enforced && now_ms > s.enforce_until_ms.saturating_add(ENFORCE_TTL_MS)
                    });
                    if prune {
                        *slot = None;
                    }
                }
            }
            targets.retain(|_, t| !t.is_empty());
        }
        // Reclaim channel sub-maps emptied by pruning so they don't accumulate as
        // ghost entries every future sweep re-scans.
        self.inner.retain(|_, targets| !targets.is_empty());
        changed
    }

    /// True iff any AUDIO-RELEVANT target (mute/kick class) in any channel is
    /// currently enforced & unexpired. The event loop caches this in an
    /// `AtomicBool` so the audio hot path can skip the per-frame
    /// presence+moderation `Mutex` locks when no moderation is active (the
    /// common case) — Qodo perf.
    pub fn any_enforced(&self, now_ms: u64) -> bool {
        self.inner.values().any(|targets| {
            targets.values().any(|t| {
                // ZEB-612: the invite class is deliberately EXCLUDED — this
                // flag gates the audio hot path's per-frame moderation checks,
                // and invites never participate in the is_muted/is_kicked drop
                // decisions. Counting them would force per-frame map locks
                // while a benign invite is pending (Qodo perf, PR #442).
                [&t.mute, &t.kick]
                    .into_iter()
                    .flatten()
                    .any(|s| s.enforced && now_ms < s.enforce_until_ms)
            })
        })
    }

    /// Drop all moderation state for a channel (called on Leave).
    pub fn remove_channel(&mut self, c: &SpaceId, ch: &ChannelId) {
        self.inner.remove(&(*c, *ch));
    }

    /// Enumerate currently-enforced targets in (c, ch), one list per class.
    pub fn snapshot(&self, c: &SpaceId, ch: &ChannelId, now_ms: u64) -> ModerationSnapshot {
        let mut snap = ModerationSnapshot::default();
        if let Some(targets) = self.inner.get(&(*c, *ch)) {
            for (owner, t) in targets {
                let live = |slot: &Option<ClassState>| {
                    slot.as_ref()
                        .is_some_and(|s| s.enforced && now_ms < s.enforce_until_ms)
                };
                if live(&t.mute) {
                    snap.muted.push(*owner);
                }
                if live(&t.kick) {
                    snap.kicked.push(*owner);
                }
                if live(&t.invite) {
                    snap.invited.push(*owner);
                }
            }
        }
        snap
    }

    /// Number of target rows currently retained for `(c, ch)` (enforced or not).
    /// Test-only: lets the prune test assert the internal map shrank, since the
    /// public surface (`snapshot`/`is_*`) can't distinguish "un-enforced row
    /// retained" from "row pruned".
    #[cfg(test)]
    fn target_count(&self, c: &SpaceId, ch: &ChannelId) -> usize {
        self.inner.get(&(*c, *ch)).map_or(0, |m| m.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_channel_log::derive_channel_key;
    use crate::owner_state_types::EpochKey;

    fn key() -> ChannelKey {
        derive_channel_key(
            &EpochKey::new([9u8; 32]),
            &SpaceId([1u8; 16]),
            &ChannelId([2u8; 16]),
        )
    }
    fn sk() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[3u8; 32])
    }
    const C: SpaceId = SpaceId([1u8; 16]);
    const CH: ChannelId = ChannelId([2u8; 16]);

    fn directive(action: ModAction, vk: [u8; 32]) -> VoiceModerationDirective {
        VoiceModerationDirective {
            actor_owner: [0xAA; 16],
            actor_device: vk,
            target_owner: [0xBB; 16],
            action,
            issued_hlc: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "x".into(),
            },
            seq: 1,
        }
    }

    /// A directive with a chosen `issued_hlc.wall_ms`, `seq`, and action (default
    /// actor/target/device). Drives the ZEB-853 C2 forward-skew-gate tests.
    fn directive_fixture(wall_ms: u64, seq: u64, action: ModAction) -> VoiceModerationDirective {
        let mut d = directive(action, [0u8; 32]);
        d.issued_hlc = Hlc {
            wall_ms,
            logical: 0,
            device_id: "x".into(),
        };
        d.seq = seq;
        d
    }

    /// Mirrors the real ingest call path (reject-then-apply): `now` is the
    /// receiver's *wall-epoch* clock feeding the skew gate (`Some(now)`); it also
    /// doubles as the monotonic TTL clock here since flip semantics are
    /// TTL-independent.
    fn apply_with_skew_gate(
        am: &mut ActiveModeration,
        d: &VoiceModerationDirective,
        now: u64,
    ) -> bool {
        am.apply_gated(&C, &CH, d, now, ENFORCE_TTL_MS, Some(now))
    }

    /// ZEB-853 C2: a directive stamped past the control-tier forward-skew ceiling
    /// is REJECTED at ingest — it must neither apply nor advance (freeze) the
    /// slot. Without the gate a far-future `issued_hlc.wall_ms` wins the
    /// `strictly_newer` LWW forever, and a subsequent honest in-window directive
    /// is stale-ignored (the FAIL-OPEN freeze this fix bounds).
    #[test]
    fn future_dated_directive_rejected_does_not_freeze_slot() {
        let now = 1_700_000_000_000u64;
        let mut active = ActiveModeration::default();
        // 1) attacker directive stamped one ms past the 5-min control ceiling.
        let attacker = directive_fixture(
            now + crate::clock_trust::MAX_FORWARD_SKEW_MS + 1,
            1,
            ModAction::Mute,
        );
        let flipped = apply_with_skew_gate(&mut active, &attacker, now);
        assert!(
            !flipped,
            "future-dated directive must be rejected, not applied"
        );
        // 2) an honest later directive (in-window, smaller wall) still applies —
        //    proof the slot was never frozen by the rejected attacker stamp.
        let honest = directive_fixture(now, 2, ModAction::Mute);
        let flipped2 = apply_with_skew_gate(&mut active, &honest, now);
        assert!(
            flipped2,
            "honest in-window directive must apply (slot was not frozen)"
        );
        assert!(
            active.is_muted(&C, &CH, &[0xBB; 16], now),
            "enforcement is live after the honest apply"
        );
    }

    /// ZEB-853 C2: the control-tier ceiling is inclusive — a stamp EXACTLY at
    /// `now + MAX_FORWARD_SKEW_MS` is accepted (matches `reject_future`).
    #[test]
    fn directive_at_exact_skew_ceiling_is_accepted() {
        let now = 1_700_000_000_000u64;
        let mut active = ActiveModeration::default();
        let d = directive_fixture(
            now + crate::clock_trust::MAX_FORWARD_SKEW_MS,
            1,
            ModAction::Mute,
        );
        assert!(
            apply_with_skew_gate(&mut active, &d, now),
            "inclusive ceiling: exactly-at-tolerance applies"
        );
    }

    /// ZEB-853 C2: `receiver_now_ms() == None` (unreadable local clock) ⇒
    /// apply-all — a bad LOCAL clock must never drop an honest directive, even a
    /// far-future one. Pins the fail-open half of the gate.
    #[test]
    fn none_receiver_clock_applies_all() {
        let now_mono = 1_000u64;
        let mut active = ActiveModeration::default();
        let d = directive_fixture(u64::MAX, 1, ModAction::Mute);
        assert!(
            active.apply_gated(&C, &CH, &d, now_mono, ENFORCE_TTL_MS, None),
            "None receiver clock must apply-all (fail-open), not drop"
        );
    }

    #[test]
    fn mod_action_discriminant_and_roundtrip() {
        for a in [
            ModAction::Mute,
            ModAction::Unmute,
            ModAction::Kick,
            ModAction::Unkick,
            ModAction::InviteToSpeak,
        ] {
            let b = canonical_cbor_encode(&a).unwrap();
            let back: ModAction = ciborium::from_reader(b.as_slice()).unwrap();
            assert_eq!(a, back);
        }
        assert_eq!(
            canonical_cbor_encode(&ModAction::Kick).unwrap(),
            canonical_cbor_encode(&2u8).unwrap()
        );
        assert_eq!(
            canonical_cbor_encode(&ModAction::InviteToSpeak).unwrap(),
            canonical_cbor_encode(&4u8).unwrap()
        );
    }

    #[test]
    fn mod_action_classes_route_independently() {
        assert_eq!(ModAction::Mute.class(), ModClass::Mute);
        assert_eq!(ModAction::Unmute.class(), ModClass::Mute);
        assert_eq!(ModAction::Kick.class(), ModClass::Kick);
        assert_eq!(ModAction::Unkick.class(), ModClass::Kick);
        assert_eq!(ModAction::InviteToSpeak.class(), ModClass::Invite);
        assert!(ModAction::InviteToSpeak.enforces());
    }

    #[test]
    fn sign_then_verify_ok_and_tamper_fails() {
        let signing = sk();
        let vk = signing.verifying_key().to_bytes();
        let signed = sign_directive(directive(ModAction::Mute, vk), &signing).unwrap();
        assert!(verify_directive_sig(&signed).is_ok());
        let mut bad = signed.clone();
        bad.directive.action = ModAction::Unmute;
        assert_eq!(verify_directive_sig(&bad), Err(ModError::BadSig));
    }

    #[test]
    fn seal_open_roundtrip_and_wrong_channel_drops() {
        let signing = sk();
        let vk = signing.verifying_key().to_bytes();
        let signed = sign_directive(directive(ModAction::Kick, vk), &signing).unwrap();
        let sealed = seal_directive(&key(), &C, &CH, &signed).unwrap();
        let opened = open_directive(&key(), &C, &CH, &sealed).unwrap();
        assert_eq!(opened, signed);
        let other_ch = ChannelId([0xEE; 16]);
        assert!(open_directive(&key(), &C, &other_ch, &sealed).is_none());
    }

    fn applied(
        am: &mut ActiveModeration,
        action: ModAction,
        hlc_wall: u64,
        seq: u64,
        now: u64,
    ) -> bool {
        let mut d = directive(action, [0u8; 32]);
        d.issued_hlc = Hlc {
            wall_ms: hlc_wall,
            logical: 0,
            device_id: "x".into(),
        };
        d.seq = seq;
        am.apply(&C, &CH, &d, now, ENFORCE_TTL_MS)
    }

    #[test]
    fn mute_then_expire() {
        let mut am = ActiveModeration::default();
        assert!(applied(&mut am, ModAction::Mute, 100, 1, 1_000));
        assert!(am.is_muted(&C, &CH, &[0xBB; 16], 1_000));
        assert!(am.is_muted(&C, &CH, &[0xBB; 16], 1_000 + ENFORCE_TTL_MS - 1));
        assert!(!am.is_muted(&C, &CH, &[0xBB; 16], 1_000 + ENFORCE_TTL_MS));
    }

    #[test]
    fn reassert_refreshes_ttl() {
        let mut am = ActiveModeration::default();
        applied(&mut am, ModAction::Mute, 100, 1, 1_000);
        applied(&mut am, ModAction::Mute, 100, 1, 10_000);
        assert!(am.is_muted(&C, &CH, &[0xBB; 16], 10_000 + ENFORCE_TTL_MS - 1));
    }

    #[test]
    fn newer_unmute_clears_and_blocks_stale_mute() {
        let mut am = ActiveModeration::default();
        applied(&mut am, ModAction::Mute, 100, 1, 1_000);
        assert!(applied(&mut am, ModAction::Unmute, 200, 2, 1_100));
        assert!(!am.is_muted(&C, &CH, &[0xBB; 16], 1_100));
        assert!(!applied(&mut am, ModAction::Mute, 100, 1, 1_200)); // stale, ignored
        assert!(!am.is_muted(&C, &CH, &[0xBB; 16], 1_200));
    }

    #[test]
    fn mute_and_kick_are_independent_classes() {
        let mut am = ActiveModeration::default();
        applied(&mut am, ModAction::Mute, 100, 1, 1_000);
        applied(&mut am, ModAction::Kick, 100, 1, 1_000);
        assert!(am.is_muted(&C, &CH, &[0xBB; 16], 1_000));
        assert!(am.is_kicked(&C, &CH, &[0xBB; 16], 1_000));
        applied(&mut am, ModAction::Unmute, 200, 2, 1_100);
        assert!(!am.is_muted(&C, &CH, &[0xBB; 16], 1_100));
        assert!(am.is_kicked(&C, &CH, &[0xBB; 16], 1_100));
    }

    /// ZEB-612: the invite class is independent of mute/kick — an invite on a
    /// kicked target neither clears the kick nor is cleared by an unkick.
    #[test]
    fn invite_and_kick_are_independent_classes() {
        let mut am = ActiveModeration::default();
        applied(&mut am, ModAction::Kick, 100, 1, 1_000);
        assert!(applied(&mut am, ModAction::InviteToSpeak, 100, 1, 1_000));
        assert!(am.is_kicked(&C, &CH, &[0xBB; 16], 1_000));
        assert!(am.is_invited(&C, &CH, &[0xBB; 16], 1_000));
        applied(&mut am, ModAction::Unkick, 200, 2, 1_100);
        assert!(!am.is_kicked(&C, &CH, &[0xBB; 16], 1_100));
        assert!(am.is_invited(&C, &CH, &[0xBB; 16], 1_100));
    }

    /// ZEB-612 (Qodo perf, PR #442): a pending invite must NOT flip the
    /// audio-hot-path flag — invites never gate audio, so `any_enforced`
    /// counts only the mute/kick classes.
    #[test]
    fn invite_alone_does_not_enable_hot_path_checks() {
        let mut am = ActiveModeration::default();
        assert!(applied(&mut am, ModAction::InviteToSpeak, 100, 1, 1_000));
        assert!(am.is_invited(&C, &CH, &[0xBB; 16], 1_000));
        assert!(
            !am.any_enforced(1_000),
            "invite-only state must not force per-frame moderation locks"
        );
        // A mute alongside it flips the flag as before.
        applied(&mut am, ModAction::Mute, 100, 1, 1_000);
        assert!(am.any_enforced(1_000));
    }

    /// ZEB-612: an invite lapses via the sweep like any enforced class (the
    /// receive-side TTL; the ~2 min window is issuer-side re-assertion).
    #[test]
    fn invite_expires_via_sweep() {
        let mut am = ActiveModeration::default();
        assert!(applied(&mut am, ModAction::InviteToSpeak, 100, 1, 1_000));
        assert!(am.is_invited(&C, &CH, &[0xBB; 16], 1_000 + ENFORCE_TTL_MS - 1));
        let lapsed = am.sweep(1_000 + ENFORCE_TTL_MS);
        assert_eq!(lapsed, vec![(C, CH)]);
        assert!(!am.is_invited(&C, &CH, &[0xBB; 16], 1_000 + ENFORCE_TTL_MS));
    }

    #[test]
    fn sweep_reports_lapsed_targets() {
        let mut am = ActiveModeration::default();
        applied(&mut am, ModAction::Mute, 100, 1, 1_000);
        let lapsed = am.sweep(1_000 + ENFORCE_TTL_MS);
        assert_eq!(lapsed, vec![(C, CH)]);
        assert!(!am.is_muted(&C, &CH, &[0xBB; 16], 1_000 + ENFORCE_TTL_MS));
    }

    #[test]
    fn sweep_prunes_long_expired_rows() {
        let mut am = ActiveModeration::default();
        // Mute at t=1_000 → enforce_until_ms = 1_000 + ENFORCE_TTL_MS.
        assert!(applied(&mut am, ModAction::Mute, 100, 1, 1_000));
        assert_eq!(am.target_count(&C, &CH), 1);
        // First sweep AT the TTL edge lapses it (enforced→false) but RETAINS the
        // row as a tombstone within the grace window.
        let lapsed = am.sweep(1_000 + ENFORCE_TTL_MS);
        assert_eq!(lapsed, vec![(C, CH)]);
        assert_eq!(
            am.target_count(&C, &CH),
            1,
            "lapsed-but-recent row retained as a stale-re-apply tombstone"
        );
        // A sweep well past 2×TTL (the grace) prunes the row entirely and reclaims
        // the now-empty channel sub-map.
        let lapsed2 = am.sweep(1_000 + 3 * ENFORCE_TTL_MS);
        assert!(lapsed2.is_empty(), "no effective change on the prune sweep");
        assert_eq!(
            am.target_count(&C, &CH),
            0,
            "long-expired row pruned so rows don't accumulate forever"
        );
    }

    #[test]
    fn snapshot_lists_enforced_targets() {
        let mut am = ActiveModeration::default();
        // Mute owner [0xBB;16] (the default target the `directive` helper uses).
        applied(&mut am, ModAction::Mute, 100, 1, 1_000);
        // Kick a SECOND owner [0xDD;16] — build the directive directly so the
        // target differs from the helper's hardcoded [0xBB;16].
        let mut kd = directive(ModAction::Kick, [0u8; 32]);
        kd.target_owner = [0xDD; 16];
        kd.issued_hlc = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "x".into(),
        };
        kd.seq = 1;
        assert!(am.apply(&C, &CH, &kd, 1_000, ENFORCE_TTL_MS));

        // ZEB-612: invite a THIRD owner so the snapshot exercises all classes.
        let mut inv = directive(ModAction::InviteToSpeak, [0u8; 32]);
        inv.target_owner = [0xEE; 16];
        inv.issued_hlc = Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "x".into(),
        };
        inv.seq = 1;
        assert!(am.apply(&C, &CH, &inv, 1_000, ENFORCE_TTL_MS));

        let snap = am.snapshot(&C, &CH, 1_000);
        assert_eq!(snap.muted, vec![[0xBB; 16]]);
        assert_eq!(snap.kicked, vec![[0xDD; 16]]);
        assert_eq!(snap.invited, vec![[0xEE; 16]]);

        // After both TTLs lapse (sweep marks them un-enforced), snapshot is empty.
        am.sweep(1_000 + ENFORCE_TTL_MS);
        let snap = am.snapshot(&C, &CH, 1_000 + ENFORCE_TTL_MS);
        assert!(snap.muted.is_empty());
        assert!(snap.kicked.is_empty());
        assert!(snap.invited.is_empty());
    }

    use crate::community_membership::{MaterializedMembership, MemberState, MemberStatus};
    use std::collections::BTreeSet;

    fn joined_member(device: [u8; 32]) -> MemberState {
        let mut keys = BTreeSet::new();
        keys.insert(device);
        MemberState {
            status: MemberStatus::Joined,
            joined_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: "x".into(),
            },
            left_at: None,
            enrolled_device_keys: keys,
            revoked_device_keys: BTreeSet::new(),
        }
    }

    fn membership(
        actor_power: u8,
        target_power: u8,
        actor_dev: [u8; 32],
    ) -> MaterializedMembership {
        let mut m = MaterializedMembership::default();
        m.members
            .insert(OwnerAddr([0xAA; 16]), joined_member(actor_dev));
        m.members
            .insert(OwnerAddr([0xBB; 16]), joined_member([0xCC; 32]));
        m.power_levels.insert(OwnerAddr([0xAA; 16]), actor_power);
        m.power_levels.insert(OwnerAddr([0xBB; 16]), target_power);
        m
    }

    #[test]
    fn authority_accepts_powerful_member() {
        let signing = sk();
        let vk = signing.verifying_key().to_bytes();
        let signed = sign_directive(directive(ModAction::Mute, vk), &signing).unwrap();
        let mm = membership(60, 0, vk);
        assert!(verify_directive_authority(&mm, &signed).is_ok());
    }

    #[test]
    fn authority_rejects_non_member() {
        let signing = sk();
        let vk = signing.verifying_key().to_bytes();
        let signed = sign_directive(directive(ModAction::Mute, vk), &signing).unwrap();
        let mm = MaterializedMembership::default();
        assert_eq!(
            verify_directive_authority(&mm, &signed),
            Err(ModError::NotMember)
        );
    }

    #[test]
    fn authority_rejects_insufficient_power() {
        let signing = sk();
        let vk = signing.verifying_key().to_bytes();
        let signed = sign_directive(directive(ModAction::Mute, vk), &signing).unwrap();
        let mm = membership(49, 0, vk);
        assert_eq!(
            verify_directive_authority(&mm, &signed),
            Err(ModError::NotAuthorized)
        );
    }

    #[test]
    fn authority_rejects_power_not_greater_than_target() {
        let signing = sk();
        let vk = signing.verifying_key().to_bytes();
        let signed = sign_directive(directive(ModAction::Kick, vk), &signing).unwrap();
        let mm = membership(60, 60, vk);
        assert_eq!(
            verify_directive_authority(&mm, &signed),
            Err(ModError::NotAuthorized)
        );
    }

    /// ZEB-612: the benign invite skips the `actor > target` clause — a mod
    /// may invite an equal- or higher-power member to speak. The punitive
    /// contrast (Mute with the same powers) stays rejected.
    #[test]
    fn authority_allows_invite_to_equal_or_higher_power_target() {
        let signing = sk();
        let vk = signing.verifying_key().to_bytes();
        let invite = sign_directive(directive(ModAction::InviteToSpeak, vk), &signing).unwrap();
        // Equal power (60 vs 60) → invite OK, mute rejected.
        let mm = membership(60, 60, vk);
        assert!(verify_directive_authority(&mm, &invite).is_ok());
        let mute = sign_directive(directive(ModAction::Mute, vk), &signing).unwrap();
        assert_eq!(
            verify_directive_authority(&mm, &mute),
            Err(ModError::NotAuthorized)
        );
        // Higher-power target (50 vs 90) → invite still OK.
        let mm = membership(50, 90, vk);
        assert!(verify_directive_authority(&mm, &invite).is_ok());
    }

    /// ZEB-612: the invite still requires mod-tier power.
    #[test]
    fn authority_rejects_invite_below_mod_power() {
        let signing = sk();
        let vk = signing.verifying_key().to_bytes();
        let invite = sign_directive(directive(ModAction::InviteToSpeak, vk), &signing).unwrap();
        let mm = membership(49, 0, vk);
        assert_eq!(
            verify_directive_authority(&mm, &invite),
            Err(ModError::NotAuthorized)
        );
    }

    #[test]
    fn authority_rejects_device_not_enrolled_for_actor() {
        let signing = sk();
        let vk = signing.verifying_key().to_bytes();
        let signed = sign_directive(directive(ModAction::Mute, vk), &signing).unwrap();
        let mm = membership(60, 0, [0x11; 32]); // actor enrolled with a DIFFERENT device
        assert_eq!(
            verify_directive_authority(&mm, &signed),
            Err(ModError::NotMember)
        );
    }

    #[test]
    fn authority_rejects_left_member() {
        // A member who has Left the community must not moderate, even though
        // their device key may still sit in `enrolled_device_keys` and their
        // stale `power_levels` entry persists (power is not cleaned up on Leave).
        let signing = sk();
        let vk = signing.verifying_key().to_bytes();
        let signed = sign_directive(directive(ModAction::Mute, vk), &signing).unwrap();
        let mut mm = membership(60, 0, vk);
        mm.members.get_mut(&OwnerAddr([0xAA; 16])).unwrap().status = MemberStatus::Left;
        assert_eq!(
            verify_directive_authority(&mm, &signed),
            Err(ModError::NotMember)
        );
    }
}
