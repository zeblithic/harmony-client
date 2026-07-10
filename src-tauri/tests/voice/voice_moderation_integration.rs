//! ZEB-358: deterministic integration of the voice-moderation directive
//! pipeline across parties (moderator issues → observer receives + enforces).
//! No real transport — exercises seal/open + membership authority + LWW + TTL.
#![cfg(feature = "test-fixtures")]

use ed25519_dalek::SigningKey;
use harmony_app::community_channel_log::{derive_channel_key, ChannelKey};
use harmony_app::community_membership::{
    ChannelId, MaterializedMembership, MemberState, MemberStatus,
};
use harmony_app::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};
use harmony_app::voice_moderation::{
    open_directive, seal_directive, sign_directive, verify_directive_authority, ActiveModeration,
    ModAction, VoiceModerationDirective, ENFORCE_TTL_MS,
};
use std::collections::BTreeSet;

const MOD: [u8; 16] = [0xAA; 16]; // moderator owner, power 60
const TARGET: [u8; 16] = [0xBB; 16]; // target owner, power 0
const PEER: [u8; 16] = [0xEE; 16]; // co-equal admin, power 60

fn key() -> ChannelKey {
    derive_channel_key(
        &EpochKey::new([9u8; 32]),
        &SpaceId([1u8; 16]),
        &ChannelId([2u8; 16]),
    )
}
const C: SpaceId = SpaceId([1u8; 16]);
const CH: ChannelId = ChannelId([2u8; 16]);

fn joined(device: [u8; 32]) -> MemberState {
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

/// Membership with mod(60, mod_vk), target(0), peer(60, peer_vk).
fn membership(mod_vk: [u8; 32], peer_vk: [u8; 32]) -> MaterializedMembership {
    let mut m = MaterializedMembership::default();
    m.members.insert(OwnerAddr(MOD), joined(mod_vk));
    m.members.insert(OwnerAddr(TARGET), joined([0xCC; 32]));
    m.members.insert(OwnerAddr(PEER), joined(peer_vk));
    m.power_levels.insert(OwnerAddr(MOD), 60);
    m.power_levels.insert(OwnerAddr(TARGET), 0);
    m.power_levels.insert(OwnerAddr(PEER), 60);
    m
}

fn directive(
    actor: [u8; 16],
    actor_vk: [u8; 32],
    target: [u8; 16],
    action: ModAction,
    wall: u64,
) -> VoiceModerationDirective {
    VoiceModerationDirective {
        actor_owner: actor,
        actor_device: actor_vk,
        target_owner: target,
        action,
        issued_hlc: Hlc {
            wall_ms: wall,
            logical: 0,
            device_id: "m".into(),
        },
        seq: 0,
    }
}

/// Simulate a receiver: open the sealed packet, authorize against membership,
/// and apply. Returns Ok(state_changed) if authorized+opened, Err(()) if dropped.
fn receive(
    am: &mut ActiveModeration,
    mm: &MaterializedMembership,
    sealed: &[u8],
    now: u64,
) -> Result<bool, ()> {
    let signed = open_directive(&key(), &C, &CH, sealed).ok_or(())?;
    verify_directive_authority(mm, &signed).map_err(|_| ())?;
    Ok(am.apply(&C, &CH, &signed.directive, now, ENFORCE_TTL_MS))
}

#[test]
fn moderator_mute_then_observer_drops_target() {
    let mod_sk = SigningKey::from_bytes(&[3u8; 32]);
    let mod_vk = mod_sk.verifying_key().to_bytes();
    let mm = membership(mod_vk, [0x55; 32]);
    let signed = sign_directive(
        directive(MOD, mod_vk, TARGET, ModAction::Mute, 100),
        &mod_sk,
    )
    .unwrap();
    let sealed = seal_directive(&key(), &C, &CH, &signed).unwrap();

    let mut am = ActiveModeration::default();
    assert_eq!(receive(&mut am, &mm, &sealed, 1_000), Ok(true));
    assert!(am.is_muted(&C, &CH, &TARGET, 1_000));
    assert!(!am.is_kicked(&C, &CH, &TARGET, 1_000));
    let snap = am.snapshot(&C, &CH, 1_000);
    assert_eq!(snap.muted, vec![TARGET]);
    assert!(snap.kicked.is_empty());
    assert!(snap.invited.is_empty());
}

#[test]
fn moderator_kick_enforced_then_unkick_clears() {
    let mod_sk = SigningKey::from_bytes(&[3u8; 32]);
    let mod_vk = mod_sk.verifying_key().to_bytes();
    let mm = membership(mod_vk, [0x55; 32]);
    let mut am = ActiveModeration::default();

    let kick = seal_directive(
        &key(),
        &C,
        &CH,
        &sign_directive(
            directive(MOD, mod_vk, TARGET, ModAction::Kick, 100),
            &mod_sk,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(receive(&mut am, &mm, &kick, 1_000), Ok(true));
    assert!(am.is_kicked(&C, &CH, &TARGET, 1_000));

    // Unkick with a strictly-newer HLC clears it.
    let unkick = seal_directive(
        &key(),
        &C,
        &CH,
        &sign_directive(
            directive(MOD, mod_vk, TARGET, ModAction::Unkick, 200),
            &mod_sk,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(receive(&mut am, &mm, &unkick, 1_100), Ok(true));
    assert!(!am.is_kicked(&C, &CH, &TARGET, 1_100));
}

#[test]
fn non_moderator_directive_is_rejected() {
    // The target (power 0) tries to mute the moderator — open succeeds (they hold
    // the channel key) but authority fails, so nothing is applied.
    let target_sk = SigningKey::from_bytes(&[4u8; 32]);
    let target_vk = target_sk.verifying_key().to_bytes();
    // Put the target's real signing vk into membership so the sig+enrollment pass
    // and ONLY the power gate rejects.
    let mut mm = membership([0x77; 32], [0x55; 32]);
    mm.members.insert(OwnerAddr(TARGET), joined(target_vk));
    let signed = sign_directive(
        directive(TARGET, target_vk, MOD, ModAction::Mute, 100),
        &target_sk,
    )
    .unwrap();
    let sealed = seal_directive(&key(), &C, &CH, &signed).unwrap();

    let mut am = ActiveModeration::default();
    assert_eq!(receive(&mut am, &mm, &sealed, 1_000), Err(()));
    assert!(!am.is_muted(&C, &CH, &MOD, 1_000));
}

#[test]
fn power_not_greater_than_target_is_rejected() {
    // moderator (60) tries to mute a co-equal admin PEER (60) → rejected.
    let mod_sk = SigningKey::from_bytes(&[3u8; 32]);
    let mod_vk = mod_sk.verifying_key().to_bytes();
    let mm = membership(mod_vk, [0x55; 32]);
    let signed =
        sign_directive(directive(MOD, mod_vk, PEER, ModAction::Mute, 100), &mod_sk).unwrap();
    let sealed = seal_directive(&key(), &C, &CH, &signed).unwrap();

    let mut am = ActiveModeration::default();
    assert_eq!(receive(&mut am, &mm, &sealed, 1_000), Err(()));
    assert!(!am.is_muted(&C, &CH, &PEER, 1_000));
}

#[test]
fn directive_sealed_for_another_channel_is_dropped() {
    let mod_sk = SigningKey::from_bytes(&[3u8; 32]);
    let mod_vk = mod_sk.verifying_key().to_bytes();
    let signed = sign_directive(
        directive(MOD, mod_vk, TARGET, ModAction::Mute, 100),
        &mod_sk,
    )
    .unwrap();
    let sealed = seal_directive(&key(), &C, &CH, &signed).unwrap();
    // Open under a DIFFERENT channel id → AAD mismatch → None (drop).
    let other_ch = ChannelId([0x99; 16]);
    assert!(open_directive(&key(), &C, &other_ch, &sealed).is_none());
}

#[test]
fn mute_lapses_after_ttl() {
    let mod_sk = SigningKey::from_bytes(&[3u8; 32]);
    let mod_vk = mod_sk.verifying_key().to_bytes();
    let mm = membership(mod_vk, [0x55; 32]);
    let signed = sign_directive(
        directive(MOD, mod_vk, TARGET, ModAction::Mute, 100),
        &mod_sk,
    )
    .unwrap();
    let sealed = seal_directive(&key(), &C, &CH, &signed).unwrap();

    let mut am = ActiveModeration::default();
    receive(&mut am, &mm, &sealed, 1_000).unwrap();
    assert!(am.is_muted(&C, &CH, &TARGET, 1_000));
    // Past TTL with no re-assert → sweep lapses it.
    let lapsed = am.sweep(1_000 + ENFORCE_TTL_MS);
    assert_eq!(lapsed, vec![(C, CH)]);
    assert!(!am.is_muted(&C, &CH, &TARGET, 1_000 + ENFORCE_TTL_MS));
}
