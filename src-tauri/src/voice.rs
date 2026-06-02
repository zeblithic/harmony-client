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
use serde::Deserialize;
use std::sync::Arc;

/// An outbound voice frame from the frontend, ready to publish to Zenoh.
#[derive(Debug)]
pub struct VoiceOutbound {
    pub community_id: SpaceId,
    pub channel_id: ChannelId,
    pub frame: Vec<u8>,
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
