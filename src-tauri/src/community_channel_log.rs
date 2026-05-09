//! ZEB-248 Phase 2: per-channel data plane.
//!
//! Ships:
//! - `SignedChannelEvent` (Post variant; v3-reserved variants commented).
//! - `ChannelKey` + `derive_channel_key` (HKDF-SHA256 over MembershipKey).
//! - `encrypt_channel_packet` / `decrypt_channel_packet` (ChaCha20-Poly1305 with
//!   12-byte random nonce + static AAD).
//! - `ChannelLogReplayTracker` (per-(channel, author, device) HLC monotonicity).
//! - `verify_channel_event` (§7 chain steps 3-7 against a pre-decrypted event).
//! - `ChannelLog` + `ChannelLogManifest` + `SegmentDescriptor` + segmented
//!   persistence (manifest + tail + sealed segments).
//!
//! Out of scope (Phase 3): `ChannelLogEngine`, Zenoh transport, debounced flush
//! task, IPC surface, frontend.
//!
//! Parent spec: docs/specs/2026-05-09-zeb-248-channels-within-communities-design.md
//! (commit 5145484), sections §5.2, §6, §7, §8, §13.1.

use crate::community_membership::ChannelId;
use crate::owner_state_types::MembershipKey;
use crate::owner_state_types::SpaceId;
use hkdf::Hkdf;
use sha2::Sha256;

/// Symmetric key for one channel's wire encryption. Derived
/// deterministically from `(MembershipKey, community_id, channel_id)`
/// via HKDF-SHA256, so any Joined member can derive every channel's
/// key without out-of-band coordination. v3 will use this seam to
/// add private channels (distribute the ChannelKey to a subset of
/// members) without a wire-format break.
#[derive(Clone, zeroize::ZeroizeOnDrop)]
pub struct ChannelKey(#[cfg_attr(not(test), allow(dead_code))] [u8; 32]);

impl ChannelKey {
    /// Borrow the raw 32 bytes for AEAD initialization. Not `pub` —
    /// callers go through `encrypt_channel_packet` / `decrypt_channel_packet`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for ChannelKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ChannelKey(<32 bytes redacted>)")
    }
}

/// HKDF-SHA256 derivation of a per-channel symmetric key.
///
/// - IKM: `MembershipKey` raw bytes (32 B).
/// - Salt: `community_id` raw bytes (16 B). Community-scoped so the same
///   channel-id collision across two communities yields different keys.
/// - Info: `b"channel:" || channel_id` (8 + 16 = 24 B). Channel-scoped so
///   distinct channels in the same community yield different keys.
/// - Output: 32 B → ChannelKey.
///
/// Per spec §6.
pub fn derive_channel_key(
    mk: &MembershipKey,
    community_id: &SpaceId,
    channel_id: &ChannelId,
) -> ChannelKey {
    let salt = community_id.0;
    let mut info = Vec::with_capacity(8 + 16);
    info.extend_from_slice(b"channel:");
    info.extend_from_slice(&channel_id.0[..]);
    let mut out = zeroize::Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(Some(&salt), mk.as_bytes())
        .expand(&info, out.as_mut())
        .expect("32 ≤ 8160");
    ChannelKey(*out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_mk() -> MembershipKey {
        MembershipKey::new([0xaa; 32])
    }

    fn fixture_community(id: u8) -> SpaceId {
        SpaceId([id; 16])
    }

    fn fixture_channel(id: u8) -> ChannelId {
        ChannelId([id; 16])
    }

    #[test]
    fn derive_channel_key_is_deterministic() {
        let mk = fixture_mk();
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let k1 = derive_channel_key(&mk, &cid, &chid);
        let k2 = derive_channel_key(&mk, &cid, &chid);
        assert_eq!(k1.as_bytes(), k2.as_bytes());
    }

    #[test]
    fn derive_channel_key_distinct_by_channel_id() {
        let mk = fixture_mk();
        let cid = fixture_community(0xc0);
        let k_a = derive_channel_key(&mk, &cid, &fixture_channel(0x01));
        let k_b = derive_channel_key(&mk, &cid, &fixture_channel(0x02));
        assert_ne!(
            k_a.as_bytes(),
            k_b.as_bytes(),
            "different channel_id under same community must yield distinct keys"
        );
    }

    #[test]
    fn derive_channel_key_distinct_by_community_id() {
        let mk = fixture_mk();
        let chid = fixture_channel(0x01);
        let k_a = derive_channel_key(&mk, &fixture_community(0xc0), &chid);
        let k_b = derive_channel_key(&mk, &fixture_community(0xc1), &chid);
        assert_ne!(
            k_a.as_bytes(),
            k_b.as_bytes(),
            "same channel_id under different communities must yield distinct keys"
        );
    }

    #[test]
    fn derive_channel_key_distinct_by_membership_key() {
        let cid = fixture_community(0xc0);
        let chid = fixture_channel(0x01);
        let k_a = derive_channel_key(&MembershipKey::new([0xaa; 32]), &cid, &chid);
        let k_b = derive_channel_key(&MembershipKey::new([0xbb; 32]), &cid, &chid);
        assert_ne!(
            k_a.as_bytes(),
            k_b.as_bytes(),
            "different membership keys must yield distinct channel keys"
        );
    }

    #[test]
    fn channel_key_zeroize_on_drop() {
        // Use ZeroizeOnDrop's invariant: dropping the wrapper zeros the
        // underlying [u8; 32]. We can't easily observe the freed memory,
        // but we can verify the trait is implemented by constraining a
        // generic function.
        fn assert_zeroize_on_drop<T: zeroize::ZeroizeOnDrop>() {}
        assert_zeroize_on_drop::<ChannelKey>();
    }
}
