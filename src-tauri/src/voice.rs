//! Voice channel relay types.
//!
//! The Rust side is a dumb relay — all audio encoding/decoding happens
//! in the browser. This module defines the IPC types and channel request
//! enum for voice traffic between Tauri commands and the event loop.
//!
//! ZEB-350 Voice V2: every type is scoped to `(community, channel)` and the
//! join request is *enriched* with the capabilities the event loop needs to
//! seal/open media + presence (the channel `ChannelKey`, the device-#2
//! signing key, and the node's own owner/device identity) — so
//! `event_loop::run`'s signature is left untouched.

use crate::community_channel_log::ChannelKey;
use crate::community_membership::ChannelId;
use crate::owner_state_types::{OwnerAddr, SpaceId};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// An outbound voice frame from the frontend, ready to publish to Zenoh.
#[derive(Debug)]
pub enum VoiceOutbound {
    Channel {
        community_id: SpaceId,
        channel_id: ChannelId,
        frame: Vec<u8>,
    },
    /// ZEB-352: a frame for a 1:1 DM call, sealed under per-call K_voice and
    /// published to harmony/voice/dm/{callId}/{own}.
    Dm { call_id: [u8; 16], frame: Vec<u8> },
}

/// Capabilities resolved at the IPC boundary (which holds `NodeState`) and
/// carried into the event loop on join — so `event_loop::run` needs no new
/// parameters. The signing key + own identity drive the presence publisher;
/// the channel key seals/open both media and beacons.
#[derive(Debug)]
pub struct VoiceJoinCaps {
    pub channel_key: Arc<ChannelKey>,
    pub signing_key: Arc<ed25519_dalek::SigningKey>,
    pub self_owner: OwnerAddr,
    /// 32-byte ed25519 verifying key of this device (device #2).
    pub self_device: [u8; 32],
    /// ZEB-350: the HLC reserved for this join session (own device),
    /// stamped at the IPC boundary via `reserve_next_hlc_for_device`. Carried
    /// as identifying metadata in every presence beacon (beacons order by
    /// `seq`, not HLC) and reused for the `left` tombstone on leave.
    pub joined_hlc: crate::owner_state_types::Hlc,
}

/// Voice channel lifecycle requests.
#[derive(Debug)]
pub enum VoiceChannelRequest {
    Join {
        community_id: SpaceId,
        channel_id: ChannelId,
        caps: VoiceJoinCaps,
    },
    Leave {
        community_id: SpaceId,
        channel_id: ChannelId,
    },
    /// ZEB-351 Voice V3: flip the shared mute flag the presence publisher reads,
    /// driven by the `set_voice_muted` IPC. The event loop flips the channel's
    /// `Arc<AtomicBool>` (and emits an immediate beacon) so the roster updates
    /// without waiting for the next ≤4 s heartbeat.
    SetMuted {
        community_id: SpaceId,
        channel_id: ChannelId,
        muted: bool,
    },
    /// ZEB-352: join a 1:1 DM voice call. No presence — 2-party implicit.
    JoinDmCall {
        call_id: [u8; 16],
        caps: VoiceJoinCaps,
    },
    /// ZEB-352: leave a 1:1 DM voice call.
    LeaveDmCall { call_id: [u8; 16] },
    /// ZEB-352: flip the mute flag for an active DM call.
    SetDmCallMuted { call_id: [u8; 16], muted: bool },
}

/// Payload for the send_voice_frame Tauri command.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendVoiceFramePayload {
    pub community_id: String,
    pub channel_id: String,
    pub frame_bytes: Vec<u8>,
}

/// ZEB-351 Voice V3: payload for the `set_voice_muted` Tauri command. Flips the
/// presence publisher's shared mute flag for an active voice channel.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetVoiceMutedPayload {
    pub community_id: String,
    pub channel_id: String,
    pub muted: bool,
}

/// ZEB-352: payload for the `send_dm_voice_frame` Tauri command.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendDmVoiceFramePayload {
    pub call_id: String,
    pub frame_bytes: Vec<u8>,
}

/// ZEB-352: payload for the `set_dm_call_muted` Tauri command.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetDmCallMutedPayload {
    pub call_id: String,
    pub muted: bool,
}

/// ZEB-351 Voice V3: this node's own voice identity, returned by the
/// `get_self_voice_identity` Tauri command.
///
/// The presence roster keys members by their 32-byte device verifying key
/// (`device_vk_hex`, 64 hex — same value the event loop carries as
/// `VoiceJoinCaps::self_device` and that `RosterEntry.device` serializes via
/// `ser_hex_32`). The voice packet's `sender_hash` is `VK[0..16]`, so the
/// frontend can self-detect its own roster row and correlate the receiver's
/// speaking senders (keyed by `hex(sender_hash)`) back to roster tiles.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelfVoiceIdentity {
    /// 64 hex chars = this device's 32-byte ed25519 verifying key.
    pub device_vk_hex: String,
    /// 16 bytes = `VK[0..16]`, matching the voice packet `sender_hash`.
    pub sender_hash: Vec<u8>,
}

/// Build a [`SelfVoiceIdentity`] from this device's 32-byte verifying key.
///
/// Pure helper so the byte-slicing invariant (`sender_hash == VK[0..16]`) is
/// unit-testable without a started node.
pub fn self_voice_identity_from_vk(vk: [u8; 32]) -> SelfVoiceIdentity {
    SelfVoiceIdentity {
        device_vk_hex: hex::encode(vk),
        sender_hash: vk[..16].to_vec(),
    }
}

#[cfg(test)]
mod self_voice_identity_tests {
    use super::self_voice_identity_from_vk;

    #[test]
    fn self_voice_sender_hash_is_first_16_bytes_of_vk() {
        let vk: [u8; 32] = std::array::from_fn(|i| (i as u8).wrapping_mul(7).wrapping_add(3));
        let id = self_voice_identity_from_vk(vk);

        // device_vk_hex round-trips to the full 32-byte VK.
        let decoded = hex::decode(&id.device_vk_hex).expect("hex");
        assert_eq!(
            decoded.as_slice(),
            &vk[..],
            "device_vk_hex must encode the VK"
        );
        assert_eq!(id.device_vk_hex.len(), 64, "VK is 64 hex chars");

        // sender_hash is exactly the first 16 bytes of that decoded VK.
        assert_eq!(id.sender_hash.len(), 16, "sender_hash is 16 bytes");
        assert_eq!(
            id.sender_hash.as_slice(),
            &decoded[..16],
            "sender_hash must be VK[0..16]"
        );
    }
}
