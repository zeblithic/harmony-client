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
/// Minimum power to moderate (reuses the existing `kick` threshold).
pub const MOD_POWER: u8 = 50;

/// What a directive asserts about the target owner. `serde_repr` encodes each
/// variant as its bare u8 discriminant (mirrors `ChannelKind`) and rejects
/// unknown discriminants on decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum ModAction {
    Mute = 0,
    Unmute = 1,
    Kick = 2,
    Unkick = 3,
}

impl ModAction {
    /// True for the mute class {Mute, Unmute}; false for the kick class.
    pub fn is_mute_class(self) -> bool {
        matches!(self, ModAction::Mute | ModAction::Unmute)
    }
    /// True for the "positive" directives that turn enforcement ON.
    pub fn enforces(self) -> bool {
        matches!(self, ModAction::Mute | ModAction::Kick)
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
/// AND `power(actor) > power(target)` (the `>` rejects equal-power peers and makes
/// self-moderation impossible).
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
    if actor_power < MOD_POWER || actor_power <= target_power {
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
}

/// In-memory enforcement state: which owners are currently muted/kicked per
/// (community, channel). Never persisted. Two independent classes per target
/// (mute-class {Mute,Unmute}, kick-class {Kick,Unkick}), each last-writer-wins
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
        let slot = if d.action.is_mute_class() {
            &mut target.mute
        } else {
            &mut target.kick
        };
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

    fn is(&self, c: &SpaceId, ch: &ChannelId, owner: &[u8; 16], now_ms: u64, mute: bool) -> bool {
        self.inner
            .get(&(*c, *ch))
            .and_then(|m| m.get(owner))
            .and_then(|t| {
                if mute {
                    t.mute.as_ref()
                } else {
                    t.kick.as_ref()
                }
            })
            .is_some_and(|s| s.enforced && now_ms < s.enforce_until_ms)
    }

    /// True iff `owner` is currently server-muted in `(c, ch)`.
    pub fn is_muted(&self, c: &SpaceId, ch: &ChannelId, owner: &[u8; 16], now_ms: u64) -> bool {
        self.is(c, ch, owner, now_ms, true)
    }

    /// True iff `owner` is currently kicked from voice in `(c, ch)`.
    pub fn is_kicked(&self, c: &SpaceId, ch: &ChannelId, owner: &[u8; 16], now_ms: u64) -> bool {
        self.is(c, ch, owner, now_ms, false)
    }

    /// Lapse any enforced class whose TTL passed; return the (community, channel)
    /// pairs whose effective state changed so the caller re-emits the roster.
    pub fn sweep(&mut self, now_ms: u64) -> Vec<(SpaceId, ChannelId)> {
        let mut changed = Vec::new();
        for (scope, targets) in self.inner.iter_mut() {
            let mut any = false;
            for t in targets.values_mut() {
                for s in [&mut t.mute, &mut t.kick].into_iter().flatten() {
                    if s.enforced && now_ms >= s.enforce_until_ms {
                        s.enforced = false;
                        any = true;
                    }
                }
            }
            if any {
                changed.push(*scope);
            }
        }
        changed
    }

    /// Drop all moderation state for a channel (called on Leave).
    pub fn remove_channel(&mut self, c: &SpaceId, ch: &ChannelId) {
        self.inner.remove(&(*c, *ch));
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

    #[test]
    fn mod_action_discriminant_and_roundtrip() {
        for a in [
            ModAction::Mute,
            ModAction::Unmute,
            ModAction::Kick,
            ModAction::Unkick,
        ] {
            let b = canonical_cbor_encode(&a).unwrap();
            let back: ModAction = ciborium::from_reader(b.as_slice()).unwrap();
            assert_eq!(a, back);
        }
        assert_eq!(
            canonical_cbor_encode(&ModAction::Kick).unwrap(),
            canonical_cbor_encode(&2u8).unwrap()
        );
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

    #[test]
    fn sweep_reports_lapsed_targets() {
        let mut am = ActiveModeration::default();
        applied(&mut am, ModAction::Mute, 100, 1, 1_000);
        let lapsed = am.sweep(1_000 + ENFORCE_TTL_MS);
        assert_eq!(lapsed, vec![(C, CH)]);
        assert!(!am.is_muted(&C, &CH, &[0xBB; 16], 1_000 + ENFORCE_TTL_MS));
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
