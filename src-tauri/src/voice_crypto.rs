//! ZEB-350 Voice V2: raw-byte AEAD seam for voice packets and presence
//! beacons. Thin wrappers over ChaCha20-Poly1305 keyed by the channel
//! `ChannelKey` (the same key that seals channel text — voice inherits the
//! existing E2E channel encryption). A distinct per-domain, per-scope AAD
//! prevents cross-domain replay (a text packet replayed as voice) and
//! cross-channel replay (channel X's packet opened under channel Y).

use crate::community_channel_log::ChannelKey;
use crate::community_membership::ChannelId;
use crate::owner_state_types::SpaceId;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};

/// Domain separator for sealed voice media packets.
pub const VOICE_PACKET_AAD: &[u8] = b"harmony-voice-pkt-v1";
/// Domain separator for sealed presence beacons.
pub const VOICE_PRESENCE_AAD: &[u8] = b"harmony-voice-presence-v1";
/// ZEB-537 community-presence beacon AEAD domain. Distinct from
/// `VOICE_PRESENCE_AAD` so a community-presence packet can never be opened in
/// (or confused with) a voice-presence or channel-log context.
pub const COMMUNITY_PRESENCE_AAD: &[u8] = b"harmony-community-presence-v1";
/// Domain separator for sealed DM-call voice media packets.
pub const VOICE_DM_PACKET_AAD: &[u8] = b"harmony-voice-dm-pkt-v1";
/// Domain tag for sealed voice-moderation directives (ZEB-358). Distinct from
/// presence/media so a moderation packet can never be opened under another
/// domain's AAD.
pub const VOICE_MODERATION_AAD: &[u8] = b"harmony-voice-moderation-v1";

/// ZEB-362 v2 AAD domain for community voice MEDIA packets. Bumped from
/// `VOICE_PACKET_AAD` (v1) so a stray unsigned v1 frame cleanly fails to open
/// rather than mis-parsing. v2 packets also carry a detached sender signature.
pub const VOICE_PACKET_AAD_V2: &[u8] = b"harmony-voice-pkt-v2";
/// Domain separator for the per-frame sender-signature transcript. Distinct
/// from every AAD domain so a media signature can never be confused with any
/// other signed/sealed voice artifact.
pub const VOICE_PACKET_SIG_DOMAIN_V2: &[u8] = b"harmony-voice-pkt-sig-v2";
/// Detached Ed25519 signature length appended to a v2 community voice packet.
pub const SIG_LEN: usize = 64;
/// Minimum v2 packet length: nonce(12) + tag(16) + sig(64), empty plaintext.
pub const MIN_PACKET_LEN_V2: usize = NONCE_LEN + TAG_LEN + SIG_LEN;

/// Upper bound on an inbound voice/presence Zenoh payload before any
/// allocation. A sealed 20 ms voice frame (23 B header + ≤510 B Opus +
/// 28 B nonce/tag) and a sealed presence beacon both fit well under this;
/// the cap bounds DoS allocation from a peer flooding the joined topic.
pub const MAX_VOICE_PACKET_BYTES: usize = 4096;

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const MIN_PACKET_LEN: usize = NONCE_LEN + TAG_LEN; // empty plaintext still carries a tag

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VoiceCryptoError {
    #[error("voice packet too short ({0} bytes)")]
    TooShort(usize),
    #[error("voice AEAD seal failed")]
    SealFailed,
    #[error("voice AEAD open failed (wrong key / wrong scope / tampered)")]
    OpenFailed,
    #[error("voice packet signature verification failed")]
    SigFailed,
}

/// AAD = domain ‖ community_id (16B) ‖ channel_id (16B). Binds every sealed
/// packet to its domain and (community, channel) scope.
fn scope_aad(domain: &[u8], community: &SpaceId, channel: &ChannelId) -> Vec<u8> {
    let mut aad = Vec::with_capacity(domain.len() + 32);
    aad.extend_from_slice(domain);
    aad.extend_from_slice(&community.0);
    aad.extend_from_slice(&channel.0);
    aad
}

/// v2 AAD = domain ‖ community_id(16) ‖ channel_id(16) ‖ sender_device_vk(32).
/// Binds a v2 media packet to its (community, channel) scope AND the claimed
/// sender device (defense-in-depth; the detached signature is what actually
/// authenticates the sender, since the shared key alone can't).
fn scope_aad_v2(
    domain: &[u8],
    community: &SpaceId,
    channel: &ChannelId,
    device_vk: &[u8; 32],
) -> Vec<u8> {
    let mut aad = Vec::with_capacity(domain.len() + 16 + 16 + 32);
    aad.extend_from_slice(domain);
    aad.extend_from_slice(&community.0);
    aad.extend_from_slice(&channel.0);
    aad.extend_from_slice(device_vk);
    aad
}

/// Transcript the sender's device key signs, over the CIPHERTEXT
/// (encrypt-then-sign): sig_domain ‖ community(16) ‖ channel(16) ‖ nonce(12) ‖
/// ciphertext+tag. Signing the ciphertext lets a receiver reject a forgery
/// before spending an AEAD-open and binds the exact transmitted bytes.
fn voice_sig_transcript(
    community: &SpaceId,
    channel: &ChannelId,
    nonce: &[u8; NONCE_LEN],
    ct: &[u8],
) -> Vec<u8> {
    let mut t =
        Vec::with_capacity(VOICE_PACKET_SIG_DOMAIN_V2.len() + 16 + 16 + NONCE_LEN + ct.len());
    t.extend_from_slice(VOICE_PACKET_SIG_DOMAIN_V2);
    t.extend_from_slice(&community.0);
    t.extend_from_slice(&channel.0);
    t.extend_from_slice(nonce);
    t.extend_from_slice(ct);
    t
}

/// AAD = domain ‖ call_id (16B). Binds every sealed DM-call packet to its
/// domain and `call_id`, so a packet from one call can't open in another.
fn scope_aad_dm(domain: &[u8], call_id: &[u8; 16]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(domain.len() + 16);
    aad.extend_from_slice(domain);
    aad.extend_from_slice(&call_id[..]);
    aad
}

/// Seal `plaintext` under `key` for `(community, channel)` with a random nonce.
/// Output: `[12B nonce][ChaCha20-Poly1305 ciphertext+tag]`.
pub fn encrypt_voice_packet(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    domain: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, VoiceCryptoError> {
    use chacha20poly1305::aead::OsRng;
    use chacha20poly1305::AeadCore;
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let nonce_bytes: [u8; NONCE_LEN] = nonce.into();
    seal_inner(key, community, channel, domain, plaintext, nonce_bytes)
}

fn seal_inner(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    domain: &[u8],
    plaintext: &[u8],
    nonce_bytes: [u8; NONCE_LEN],
) -> Result<Vec<u8>, VoiceCryptoError> {
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let aad = scope_aad(domain, community, channel);
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| VoiceCryptoError::SealFailed)?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open a packet sealed by [`encrypt_voice_packet`]. Any failure (wrong key,
/// wrong scope, wrong domain, tamper, truncation) returns an error — callers
/// drop silently.
pub fn decrypt_voice_packet(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    domain: &[u8],
    packet: &[u8],
) -> Result<Vec<u8>, VoiceCryptoError> {
    if packet.len() < MIN_PACKET_LEN {
        return Err(VoiceCryptoError::TooShort(packet.len()));
    }
    let (nonce_bytes, ct) = packet.split_at(NONCE_LEN);
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let aad = scope_aad(domain, community, channel);
    cipher
        .decrypt(
            Nonce::from_slice(nonce_bytes),
            Payload { msg: ct, aad: &aad },
        )
        .map_err(|_| VoiceCryptoError::OpenFailed)
}

/// ZEB-362: seal `plaintext` for `(community, channel)` and append a detached
/// Ed25519 signature by `device_sk` over the ciphertext. A receiver verifies
/// the signature against the device VK it trusts from the presence roster,
/// binding the frame to its true sender. Output:
/// `[12B nonce][ChaCha20-Poly1305 ct+tag][64B Ed25519 sig]`.
pub fn seal_and_sign_voice_packet(
    key: &ChannelKey,
    device_sk: &ed25519_dalek::SigningKey,
    community: &SpaceId,
    channel: &ChannelId,
    plaintext: &[u8],
) -> Result<Vec<u8>, VoiceCryptoError> {
    use chacha20poly1305::aead::OsRng;
    use chacha20poly1305::AeadCore;
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let nonce_bytes: [u8; NONCE_LEN] = nonce.into();
    seal_and_sign_inner(key, device_sk, community, channel, plaintext, nonce_bytes)
}

fn seal_and_sign_inner(
    key: &ChannelKey,
    device_sk: &ed25519_dalek::SigningKey,
    community: &SpaceId,
    channel: &ChannelId,
    plaintext: &[u8],
    nonce_bytes: [u8; NONCE_LEN],
) -> Result<Vec<u8>, VoiceCryptoError> {
    use ed25519_dalek::Signer;
    let device_vk = device_sk.verifying_key().to_bytes();
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let aad = scope_aad_v2(VOICE_PACKET_AAD_V2, community, channel, &device_vk);
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| VoiceCryptoError::SealFailed)?;
    let transcript = voice_sig_transcript(community, channel, &nonce_bytes, &ct);
    let sig = device_sk.sign(&transcript).to_bytes();
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len() + SIG_LEN);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    out.extend_from_slice(&sig);
    Ok(out)
}

/// ZEB-362: verify the detached sender signature on a v2 community voice packet
/// against `device_vk` (the claimed sender from the topic suffix, confirmed
/// present in the verified roster by the caller). Public-key only — no channel
/// key, no decryption. `Ok(())` iff the holder of `device_vk`'s private key
/// produced this exact ciphertext.
pub fn verify_voice_frame_sig(
    device_vk: &[u8; 32],
    community: &SpaceId,
    channel: &ChannelId,
    packet: &[u8],
) -> Result<(), VoiceCryptoError> {
    if packet.len() < MIN_PACKET_LEN_V2 {
        return Err(VoiceCryptoError::TooShort(packet.len()));
    }
    let sig_start = packet.len() - SIG_LEN;
    let (nonce_and_ct, sig_bytes) = packet.split_at(sig_start);
    let (nonce_bytes, ct) = nonce_and_ct.split_at(NONCE_LEN);
    let vk = ed25519_dalek::VerifyingKey::from_bytes(device_vk)
        .map_err(|_| VoiceCryptoError::SigFailed)?;
    let sig_arr: [u8; SIG_LEN] = sig_bytes
        .try_into()
        .map_err(|_| VoiceCryptoError::SigFailed)?;
    let sig = ed25519_dalek::Signature::from_bytes(&sig_arr);
    let nonce_arr: [u8; NONCE_LEN] = nonce_bytes
        .try_into()
        .map_err(|_| VoiceCryptoError::SigFailed)?;
    let transcript = voice_sig_transcript(community, channel, &nonce_arr, ct);
    vk.verify_strict(&transcript, &sig)
        .map_err(|_| VoiceCryptoError::SigFailed)
}

/// Crate-private: AEAD-opens WITHOUT verifying the sender signature. Callers
/// MUST verify first (the event loop's verify→moderation→open split) or use
/// `verify_and_open_voice_packet`. Exposing this publicly would be a security
/// footgun.
pub(crate) fn open_voice_packet_unchecked(
    key: &ChannelKey,
    device_vk: &[u8; 32],
    community: &SpaceId,
    channel: &ChannelId,
    packet: &[u8],
) -> Result<Vec<u8>, VoiceCryptoError> {
    if packet.len() < MIN_PACKET_LEN_V2 {
        return Err(VoiceCryptoError::TooShort(packet.len()));
    }
    let sig_start = packet.len() - SIG_LEN;
    let nonce_and_ct = &packet[..sig_start];
    let (nonce_bytes, ct) = nonce_and_ct.split_at(NONCE_LEN);
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let aad = scope_aad_v2(VOICE_PACKET_AAD_V2, community, channel, device_vk);
    cipher
        .decrypt(
            Nonce::from_slice(nonce_bytes),
            Payload { msg: ct, aad: &aad },
        )
        .map_err(|_| VoiceCryptoError::OpenFailed)
}

/// ZEB-362: the SAFE public entry point — verify the detached sender signature
/// against `device_vk`, then AEAD-open. Fail-closed by default: a frame whose
/// signature does not match `device_vk` (e.g. forged by another `ChannelKey`
/// holder) is rejected before decryption. Prefer this over the crate-private
/// `open_voice_packet_unchecked`, which exists only so the event loop can run
/// its verify → moderation-drop → open split (dropping a moderated sender's
/// frame before spending the AEAD-open).
pub fn verify_and_open_voice_packet(
    key: &ChannelKey,
    device_vk: &[u8; 32],
    community: &SpaceId,
    channel: &ChannelId,
    packet: &[u8],
) -> Result<Vec<u8>, VoiceCryptoError> {
    verify_voice_frame_sig(device_vk, community, channel, packet)?;
    open_voice_packet_unchecked(key, device_vk, community, channel, packet)
}

/// Seal `plaintext` under `key` for a DM call identified by `call_id`, with
/// a random nonce. Output: `[12B nonce][ChaCha20-Poly1305 ciphertext+tag]`.
pub fn encrypt_dm_voice_packet(
    key: &ChannelKey,
    call_id: &[u8; 16],
    domain: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, VoiceCryptoError> {
    use chacha20poly1305::aead::OsRng;
    use chacha20poly1305::AeadCore;
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let nonce_bytes: [u8; NONCE_LEN] = nonce.into();
    seal_inner_dm(key, call_id, domain, plaintext, nonce_bytes)
}

fn seal_inner_dm(
    key: &ChannelKey,
    call_id: &[u8; 16],
    domain: &[u8],
    plaintext: &[u8],
    nonce_bytes: [u8; NONCE_LEN],
) -> Result<Vec<u8>, VoiceCryptoError> {
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let aad = scope_aad_dm(domain, call_id);
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| VoiceCryptoError::SealFailed)?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open a DM-call packet sealed by [`encrypt_dm_voice_packet`]. Any failure
/// (wrong key, wrong call_id, wrong domain, tamper, truncation) returns an
/// error — callers drop silently.
pub fn decrypt_dm_voice_packet(
    key: &ChannelKey,
    call_id: &[u8; 16],
    domain: &[u8],
    packet: &[u8],
) -> Result<Vec<u8>, VoiceCryptoError> {
    if packet.len() < MIN_PACKET_LEN {
        return Err(VoiceCryptoError::TooShort(packet.len()));
    }
    let (nonce_bytes, ct) = packet.split_at(NONCE_LEN);
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let aad = scope_aad_dm(domain, call_id);
    cipher
        .decrypt(
            Nonce::from_slice(nonce_bytes),
            Payload { msg: ct, aad: &aad },
        )
        .map_err(|_| VoiceCryptoError::OpenFailed)
}

/// Deterministic-nonce DM variant for wire-format fixtures. NEVER call from
/// production — a fixed nonce with a reused key is catastrophic nonce reuse.
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn encrypt_dm_voice_packet_with_nonce(
    key: &ChannelKey,
    call_id: &[u8; 16],
    domain: &[u8],
    plaintext: &[u8],
    nonce: [u8; NONCE_LEN],
) -> Result<Vec<u8>, VoiceCryptoError> {
    seal_inner_dm(key, call_id, domain, plaintext, nonce)
}

/// AAD domain tag for group-DM presence beacons (ZEB-360). Distinct from
/// `VOICE_DM_PACKET_AAD` / `VOICE_PRESENCE_AAD` so a beacon can never be
/// confused for media or community-presence even under the same key.
pub const VOICE_GROUPDM_PRESENCE_AAD: &[u8] = b"harmony-voice-groupdm-presence-v1";

/// AAD = domain ‖ space_id (16B). Binds every sealed group-DM presence beacon
/// to its domain and `space_id` (presence is group-scoped, not call-scoped).
fn scope_aad_groupdm(domain: &[u8], space_id: &[u8; 16]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(domain.len() + 16);
    aad.extend_from_slice(domain);
    aad.extend_from_slice(&space_id[..]);
    aad
}

/// Seal a group-DM presence payload under `key`, scoped to `space_id`, with a
/// random nonce. Mirrors [`encrypt_dm_voice_packet`] but binds the 16-byte
/// `space_id` instead of a `call_id` (presence is group-scoped, not
/// call-scoped). Output: `[12B nonce][ChaCha20-Poly1305 ciphertext+tag]`.
pub fn encrypt_groupdm_presence_packet(
    key: &ChannelKey,
    space_id: &[u8; 16],
    domain: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, VoiceCryptoError> {
    use chacha20poly1305::aead::OsRng;
    use chacha20poly1305::AeadCore;
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let nonce_bytes: [u8; NONCE_LEN] = nonce.into();
    seal_inner_groupdm(key, space_id, domain, plaintext, nonce_bytes)
}

fn seal_inner_groupdm(
    key: &ChannelKey,
    space_id: &[u8; 16],
    domain: &[u8],
    plaintext: &[u8],
    nonce_bytes: [u8; NONCE_LEN],
) -> Result<Vec<u8>, VoiceCryptoError> {
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let aad = scope_aad_groupdm(domain, space_id);
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| VoiceCryptoError::SealFailed)?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open a group-DM presence packet sealed by [`encrypt_groupdm_presence_packet`].
/// Any failure (wrong key, wrong space_id, wrong domain, tamper, truncation)
/// returns an error — callers drop silently.
pub fn decrypt_groupdm_presence_packet(
    key: &ChannelKey,
    space_id: &[u8; 16],
    domain: &[u8],
    packet: &[u8],
) -> Result<Vec<u8>, VoiceCryptoError> {
    if packet.len() < MIN_PACKET_LEN {
        return Err(VoiceCryptoError::TooShort(packet.len()));
    }
    let (nonce_bytes, ct) = packet.split_at(NONCE_LEN);
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let aad = scope_aad_groupdm(domain, space_id);
    cipher
        .decrypt(
            Nonce::from_slice(nonce_bytes),
            Payload { msg: ct, aad: &aad },
        )
        .map_err(|_| VoiceCryptoError::OpenFailed)
}

/// Deterministic-nonce group-DM presence variant for wire-format fixtures.
/// NEVER call from production — a fixed nonce with a reused key is catastrophic
/// nonce reuse.
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn encrypt_groupdm_presence_packet_with_nonce(
    key: &ChannelKey,
    space_id: &[u8; 16],
    domain: &[u8],
    plaintext: &[u8],
    nonce: [u8; NONCE_LEN],
) -> Result<Vec<u8>, VoiceCryptoError> {
    seal_inner_groupdm(key, space_id, domain, plaintext, nonce)
}

/// Deterministic-nonce variant for wire-format fixtures. NEVER call from
/// production — a fixed nonce with a reused key is catastrophic nonce reuse.
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn encrypt_voice_packet_with_nonce(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    domain: &[u8],
    plaintext: &[u8],
    nonce: [u8; NONCE_LEN],
) -> Result<Vec<u8>, VoiceCryptoError> {
    seal_inner(key, community, channel, domain, plaintext, nonce)
}

/// Deterministic-nonce v2 variant for wire-format fixtures. NEVER call from
/// production — a fixed nonce with a reused key is catastrophic nonce reuse.
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn seal_and_sign_voice_packet_with_nonce(
    key: &ChannelKey,
    device_sk: &ed25519_dalek::SigningKey,
    community: &SpaceId,
    channel: &ChannelId,
    plaintext: &[u8],
    nonce: [u8; NONCE_LEN],
) -> Result<Vec<u8>, VoiceCryptoError> {
    seal_and_sign_inner(key, device_sk, community, channel, plaintext, nonce)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_channel_log::derive_channel_key;
    use crate::owner_state_types::EpochKey;

    fn key() -> ChannelKey {
        derive_channel_key(
            &EpochKey::new([0x11; 32]),
            &SpaceId([0xc0; 16]),
            &ChannelId([0xc1; 16]),
        )
    }
    const C: SpaceId = SpaceId([0xc0; 16]);
    const CH: ChannelId = ChannelId([0xc1; 16]);

    #[test]
    fn round_trip_voice_packet() {
        let k = key();
        let plain = b"opus-frame-bytes-1234567890".to_vec();
        let sealed = encrypt_voice_packet(&k, &C, &CH, VOICE_PACKET_AAD, &plain).unwrap();
        assert_ne!(sealed, plain);
        assert!(sealed.len() >= MIN_PACKET_LEN);
        let opened = decrypt_voice_packet(&k, &C, &CH, VOICE_PACKET_AAD, &sealed).unwrap();
        assert_eq!(opened, plain);
    }

    #[test]
    fn wrong_key_drops() {
        let sealed = encrypt_voice_packet(&key(), &C, &CH, VOICE_PACKET_AAD, b"x").unwrap();
        let other = derive_channel_key(&EpochKey::new([0x22; 32]), &C, &CH);
        assert_eq!(
            decrypt_voice_packet(&other, &C, &CH, VOICE_PACKET_AAD, &sealed),
            Err(VoiceCryptoError::OpenFailed)
        );
    }

    #[test]
    fn wrong_scope_drops() {
        let k = key();
        let sealed = encrypt_voice_packet(&k, &C, &CH, VOICE_PACKET_AAD, b"x").unwrap();
        // same key, different channel id in the AAD → must not open
        let other_ch = ChannelId([0xc2; 16]);
        assert_eq!(
            decrypt_voice_packet(&k, &C, &other_ch, VOICE_PACKET_AAD, &sealed),
            Err(VoiceCryptoError::OpenFailed)
        );
    }

    #[test]
    fn wrong_domain_drops() {
        let k = key();
        let sealed = encrypt_voice_packet(&k, &C, &CH, VOICE_PACKET_AAD, b"x").unwrap();
        // a media packet must not open as a presence beacon
        assert_eq!(
            decrypt_voice_packet(&k, &C, &CH, VOICE_PRESENCE_AAD, &sealed),
            Err(VoiceCryptoError::OpenFailed)
        );
    }

    #[test]
    fn truncated_drops() {
        assert_eq!(
            decrypt_voice_packet(&key(), &C, &CH, VOICE_PACKET_AAD, b"short"),
            Err(VoiceCryptoError::TooShort(5))
        );
    }

    #[test]
    fn deterministic_nonce_variant_is_stable() {
        let k = key();
        let a = encrypt_voice_packet_with_nonce(&k, &C, &CH, VOICE_PACKET_AAD, b"hello", [7u8; 12])
            .unwrap();
        let b = encrypt_voice_packet_with_nonce(&k, &C, &CH, VOICE_PACKET_AAD, b"hello", [7u8; 12])
            .unwrap();
        assert_eq!(a, b);
        assert_eq!(&a[..NONCE_LEN], &[7u8; 12]);
    }

    fn dev_sk(b: u8) -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[b; 32])
    }

    #[test]
    fn v2_round_trip_seal_verify_open() {
        let k = key();
        let sk = dev_sk(1);
        let vk = sk.verifying_key().to_bytes();
        let plain = b"opus-frame-bytes-1234567890".to_vec();
        let sealed = seal_and_sign_voice_packet(&k, &sk, &C, &CH, &plain).unwrap();
        assert!(sealed.len() >= MIN_PACKET_LEN_V2);
        verify_voice_frame_sig(&vk, &C, &CH, &sealed).unwrap();
        assert_eq!(
            open_voice_packet_unchecked(&k, &vk, &C, &CH, &sealed).unwrap(),
            plain
        );
    }

    #[test]
    fn v2_tampered_ciphertext_fails_open() {
        let k = key();
        let sk = dev_sk(1);
        let vk = sk.verifying_key().to_bytes();
        let mut sealed = seal_and_sign_voice_packet(&k, &sk, &C, &CH, b"hello").unwrap();
        // Flip a ciphertext byte (after the 12B nonce). The sig is over the
        // ciphertext, so verify fails first; assert open fails too on its own.
        sealed[NONCE_LEN + 1] ^= 0xff;
        assert_eq!(
            verify_voice_frame_sig(&vk, &C, &CH, &sealed),
            Err(VoiceCryptoError::SigFailed)
        );
        // Re-seal clean, then tamper only what open sees by re-signing is not
        // possible without the key; instead prove open rejects a flipped tag.
        let mut s2 = seal_and_sign_voice_packet(&k, &sk, &C, &CH, b"hello").unwrap();
        let tag_byte = s2.len() - SIG_LEN - 1; // last ciphertext/tag byte
        s2[tag_byte] ^= 0xff;
        assert_eq!(
            open_voice_packet_unchecked(&k, &vk, &C, &CH, &s2),
            Err(VoiceCryptoError::OpenFailed)
        );
    }

    #[test]
    fn v2_tampered_signature_fails_verify() {
        let k = key();
        let sk = dev_sk(1);
        let vk = sk.verifying_key().to_bytes();
        let mut sealed = seal_and_sign_voice_packet(&k, &sk, &C, &CH, b"hello").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff;
        assert_eq!(
            verify_voice_frame_sig(&vk, &C, &CH, &sealed),
            Err(VoiceCryptoError::SigFailed)
        );
    }

    #[test]
    fn v2_wrong_device_vk_fails_verify() {
        let k = key();
        let sk = dev_sk(1);
        let other_vk = dev_sk(2).verifying_key().to_bytes();
        let sealed = seal_and_sign_voice_packet(&k, &sk, &C, &CH, b"hello").unwrap();
        assert_eq!(
            verify_voice_frame_sig(&other_vk, &C, &CH, &sealed),
            Err(VoiceCryptoError::SigFailed)
        );
    }

    #[test]
    fn v2_cross_channel_fails_verify_and_open() {
        let k = key();
        let sk = dev_sk(1);
        let vk = sk.verifying_key().to_bytes();
        let other_ch = ChannelId([0xc2; 16]);
        let sealed = seal_and_sign_voice_packet(&k, &sk, &C, &CH, b"hello").unwrap();
        // transcript includes the channel id → verifying for another channel fails
        assert_eq!(
            verify_voice_frame_sig(&vk, &C, &other_ch, &sealed),
            Err(VoiceCryptoError::SigFailed)
        );
        // AAD also includes the channel id → opening for another channel fails
        assert_eq!(
            open_voice_packet_unchecked(&k, &vk, &C, &other_ch, &sealed),
            Err(VoiceCryptoError::OpenFailed)
        );
    }

    #[test]
    fn v2_rejects_v1_shaped_unsigned_frame() {
        // A v1 frame is [nonce][ct] with no 64B sig. Most are shorter than the
        // v2 minimum; even a long one fails the signature check.
        let k = key();
        let v1 = encrypt_voice_packet(&k, &C, &CH, VOICE_PACKET_AAD, b"hello").unwrap();
        let vk = dev_sk(1).verifying_key().to_bytes();
        assert!(verify_voice_frame_sig(&vk, &C, &CH, &v1).is_err());
    }

    #[test]
    fn v2_too_short_is_rejected() {
        let vk = dev_sk(1).verifying_key().to_bytes();
        assert_eq!(
            verify_voice_frame_sig(&vk, &C, &CH, b"short"),
            Err(VoiceCryptoError::TooShort(5))
        );
    }

    #[test]
    fn v2_deterministic_variant_is_stable() {
        let k = key();
        let sk = dev_sk(7);
        let a = seal_and_sign_voice_packet_with_nonce(&k, &sk, &C, &CH, b"hi", [0u8; 12]).unwrap();
        let b = seal_and_sign_voice_packet_with_nonce(&k, &sk, &C, &CH, b"hi", [0u8; 12]).unwrap();
        assert_eq!(a, b);
        assert_eq!(&a[..NONCE_LEN], &[0u8; 12]);
    }

    #[test]
    fn v2_verify_and_open_rejects_forged_then_opens_honest() {
        let k = key();
        let sk = dev_sk(1);
        let vk = sk.verifying_key().to_bytes();
        let sealed = seal_and_sign_voice_packet(&k, &sk, &C, &CH, b"hello").unwrap();
        // Honest frame opens.
        assert_eq!(
            verify_and_open_voice_packet(&k, &vk, &C, &CH, &sealed).unwrap(),
            b"hello"
        );
        // Wrong device VK → rejected BEFORE decryption (fail-closed).
        let other_vk = dev_sk(2).verifying_key().to_bytes();
        assert_eq!(
            verify_and_open_voice_packet(&k, &other_vk, &C, &CH, &sealed),
            Err(VoiceCryptoError::SigFailed)
        );
    }
}

#[cfg(test)]
mod dm_tests {
    use super::*;
    use crate::community_channel_log::derive_dm_voice_key;
    use crate::owner_state_types::DmContentKey;

    fn key(call: &[u8; 16]) -> ChannelKey {
        derive_dm_voice_key(&DmContentKey::new([9u8; 32]), call)
    }

    #[test]
    fn dm_round_trip() {
        let call = [3u8; 16];
        let k = key(&call);
        let frame = (0u8..30).collect::<Vec<_>>();
        let sealed = encrypt_dm_voice_packet(&k, &call, VOICE_DM_PACKET_AAD, &frame).unwrap();
        let opened = decrypt_dm_voice_packet(&k, &call, VOICE_DM_PACKET_AAD, &sealed).unwrap();
        assert_eq!(opened, frame);
    }

    #[test]
    fn dm_wrong_call_id_drops() {
        let k = key(&[3u8; 16]);
        let frame = (0u8..30).collect::<Vec<_>>();
        let sealed = encrypt_dm_voice_packet(&k, &[3u8; 16], VOICE_DM_PACKET_AAD, &frame).unwrap();
        assert!(decrypt_dm_voice_packet(&k, &[4u8; 16], VOICE_DM_PACKET_AAD, &sealed).is_err());
    }

    #[test]
    fn dm_wrong_domain_drops() {
        let call = [3u8; 16];
        let k = key(&call);
        let frame = (0u8..30).collect::<Vec<_>>();
        let sealed = encrypt_dm_voice_packet(&k, &call, VOICE_DM_PACKET_AAD, &frame).unwrap();
        assert!(decrypt_dm_voice_packet(&k, &call, VOICE_PACKET_AAD, &sealed).is_err());
    }
}

#[cfg(test)]
mod groupdm_presence_tests {
    use super::*;
    use crate::community_channel_log::derive_groupdm_presence_key;
    use crate::owner_state_types::DmContentKey;

    // `ChannelKey`'s field is private to `community_channel_log`, so construct
    // the key via the real call-independent presence-key derivation.
    fn key() -> ChannelKey {
        derive_groupdm_presence_key(&DmContentKey::new([0x33; 32]))
    }

    #[test]
    fn groupdm_presence_packet_round_trips_and_binds_space() {
        let k = key();
        let sp = [0x44u8; 16];
        let pt = b"beacon-bytes".to_vec();
        let sealed =
            encrypt_groupdm_presence_packet(&k, &sp, VOICE_GROUPDM_PRESENCE_AAD, &pt).unwrap();
        assert_eq!(
            decrypt_groupdm_presence_packet(&k, &sp, VOICE_GROUPDM_PRESENCE_AAD, &sealed).unwrap(),
            pt
        );
        // Wrong space_id in AAD must fail.
        assert!(decrypt_groupdm_presence_packet(
            &k,
            &[0x45; 16],
            VOICE_GROUPDM_PRESENCE_AAD,
            &sealed
        )
        .is_err());
        // Wrong domain must fail.
        assert!(decrypt_groupdm_presence_packet(&k, &sp, VOICE_DM_PACKET_AAD, &sealed).is_err());
    }
}
