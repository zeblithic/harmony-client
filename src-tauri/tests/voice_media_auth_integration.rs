//! ZEB-362: per-sender authenticated community voice media. Proves a receiver
//! binds each frame to its true owner device via the detached signature, so a
//! modified client cannot evade a mute/kick (or impersonate another speaker) by
//! lying about the Zenoh topic suffix. Drives the same decision the event_loop
//! subscribe arm runs (verify sig → open → attribution), minus the async
//! presence/moderation map lookups the event loop owns.
#![cfg(feature = "test-fixtures")]

use ed25519_dalek::SigningKey;
use harmony_app::community_channel_log::{derive_channel_key, ChannelKey};
use harmony_app::community_membership::ChannelId;
use harmony_app::owner_state_types::{EpochKey, SpaceId};
use harmony_app::voice_crypto::{
    open_voice_packet, seal_and_sign_voice_packet, verify_voice_frame_sig,
};

const C: SpaceId = SpaceId([0xc0; 16]);
const CH: ChannelId = ChannelId([0xc1; 16]);

fn channel_key() -> ChannelKey {
    derive_channel_key(&EpochKey::new([0x11; 32]), &C, &CH)
}

/// 23-byte voice header (flags|seq|ts|senderHash) carrying `sender_vk`'s 16-byte
/// prefix as the senderHash (bytes 7..23, mirroring the frontend layout) + a
/// short payload.
fn frame_with_header(sender_vk_prefix: &[u8; 32]) -> Vec<u8> {
    let mut f = vec![0u8; 23];
    f[0] = 0x10; // version nibble
    f[7..23].copy_from_slice(&sender_vk_prefix[..16]);
    f.extend_from_slice(b"opus-payload");
    f
}

/// Mirror of the event_loop subscribe decision (verify sig → open →
/// attribution). `claimed_dev` is what the receiver parses from the topic
/// suffix. Returns the opened frame, or a drop reason.
fn receive(
    key: &ChannelKey,
    claimed_dev: &[u8; 32],
    packet: &[u8],
) -> Result<Vec<u8>, &'static str> {
    verify_voice_frame_sig(claimed_dev, &C, &CH, packet).map_err(|_| "sig")?;
    let frame = open_voice_packet(key, claimed_dev, &C, &CH, packet).map_err(|_| "open")?;
    if frame.len() < 23 || frame[7..23] != claimed_dev[..16] {
        return Err("attribution");
    }
    Ok(frame)
}

#[test]
fn honest_frame_is_accepted() {
    let key = channel_key();
    let a = SigningKey::from_bytes(&[1u8; 32]);
    let a_vk = a.verifying_key().to_bytes();
    let frame = frame_with_header(&a_vk);
    let packet = seal_and_sign_voice_packet(&key, &a, &C, &CH, &frame).unwrap();
    assert_eq!(receive(&key, &a_vk, &packet).unwrap(), frame);
}

#[test]
fn spoofed_suffix_without_senders_key_is_dropped() {
    // The muted/kicked attacker B seals with B's OWN key but publishes under
    // A's device suffix to evade a drop on B. Receiver parses A's suffix →
    // verifies against A's VK → B's signature fails → dropped. This IS the
    // muted-owner evasion attempt, now closed.
    let key = channel_key();
    let a_vk = SigningKey::from_bytes(&[1u8; 32])
        .verifying_key()
        .to_bytes();
    let b = SigningKey::from_bytes(&[2u8; 32]);
    let b_vk = b.verifying_key().to_bytes();
    let frame = frame_with_header(&b_vk);
    let packet = seal_and_sign_voice_packet(&key, &b, &C, &CH, &frame).unwrap();
    assert_eq!(receive(&key, &a_vk, &packet), Err("sig"));
}

#[test]
fn attribution_mismatch_is_dropped() {
    // A signs a valid frame but stamps B's senderHash into the cleartext header
    // to mislabel the audio as B. Receiver verifies A's sig + opens, but the
    // header senderHash != A → dropped.
    let key = channel_key();
    let a = SigningKey::from_bytes(&[1u8; 32]);
    let a_vk = a.verifying_key().to_bytes();
    let b_vk = SigningKey::from_bytes(&[2u8; 32])
        .verifying_key()
        .to_bytes();
    let frame = frame_with_header(&b_vk); // header lies: says B
    let packet = seal_and_sign_voice_packet(&key, &a, &C, &CH, &frame).unwrap();
    assert_eq!(receive(&key, &a_vk, &packet), Err("attribution"));
}

#[test]
fn tampered_ciphertext_is_dropped() {
    let key = channel_key();
    let a = SigningKey::from_bytes(&[1u8; 32]);
    let a_vk = a.verifying_key().to_bytes();
    let frame = frame_with_header(&a_vk);
    let mut packet = seal_and_sign_voice_packet(&key, &a, &C, &CH, &frame).unwrap();
    packet[12 + 1] ^= 0xff; // flip a ciphertext byte → sig verify fails
    assert_eq!(receive(&key, &a_vk, &packet), Err("sig"));
}
