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
}
