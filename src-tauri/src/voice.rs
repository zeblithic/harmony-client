//! Voice channel relay types.
//!
//! The Rust side is a dumb relay — all audio encoding/decoding happens
//! in the browser. This module defines the IPC types and channel request
//! enum for voice traffic between Tauri commands and the event loop.

use serde::Deserialize;

/// An outbound voice frame from the frontend, ready to publish to Zenoh.
#[derive(Debug)]
pub struct VoiceOutbound {
    pub channel_id: String,
    pub frame: Vec<u8>,
}

/// Voice channel lifecycle requests.
#[derive(Debug)]
pub enum VoiceChannelRequest {
    Join { channel_id: String },
    Leave { channel_id: String },
}

/// Payload for the send_voice_frame Tauri command.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendVoiceFramePayload {
    pub channel_id: String,
    pub frame_bytes: Vec<u8>,
}
