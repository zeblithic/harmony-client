# Voice Engine Slice 1: PTT Opus Pipeline

## Goal

End-to-end push-to-talk voice over Zenoh pub/sub. Browser captures audio,
encodes Opus, sends via Tauri IPC to Rust, which publishes to Zenoh. Inbound
voice arrives via Zenoh subscription, relayed to the frontend for decode and
playback through a fixed jitter buffer.

## Scope

**In scope (Slice 1):**
- Audio capture via AudioWorklet (16kHz mono)
- Opus encoding/decoding in browser (WASM)
- Push-to-talk only (no full-duplex)
- Zenoh pub/sub transport with per-sender topics
- Fixed 80ms jitter buffer
- Speaking activity indicators
- 20ms frame size, 16kbps Opus bitrate

**Out of scope (Slice 2 / future):**
- codec2 low-bandwidth fallback
- Full-duplex with echo cancellation
- Adaptive jitter buffer
- Comfort noise / packet loss concealment
- Settings UI (device picker, volume controls)
- Per-channel voice encryption
- Rawlink jitter bypass for voice topics

## Architecture

### Data Flow

```
[Outbound]
PttButton press -> AudioService.start()
  -> AudioWorklet captures 20ms PCM frames @ 16kHz mono
  -> opus-wasm encodes -> ~40-byte Opus frame
  -> Tauri IPC invoke("send_voice_frame", { channel_id, opus_bytes })
  -> voice_tx channel -> event_loop.rs
  -> Zenoh publish to harmony/voice/{channel_id}/{sender_address}

[Inbound]
Zenoh subscription on harmony/voice/{channel_id}/*
  -> event_loop.rs receives sample, extracts sender from topic
  -> Tauri emit("voice-frame-received", { sender, seq, ts, opus_bytes })
  -> VoiceReceiver service receives event
  -> opus-wasm decodes -> PCM
  -> Fixed 80ms jitter buffer (4 frames, sequence-indexed ring buffer)
  -> AudioContext schedules playback via AudioBufferSourceNode
  -> Browser mixes multiple senders at destination node
```

### Key Constraint

Voice frames bypass the runtime tick loop entirely. They go directly from
Tauri IPC to Zenoh publish (outbound) and Zenoh subscription to Tauri emit
(inbound). This keeps voice off the 250ms tick cycle and in its own hot path.

### Encoding Location

All encoding and decoding happens in the browser via opus-wasm. The Rust/Tauri
layer is a dumb relay between frontend IPC and Zenoh pub/sub. This keeps the
audio path in the browser's well-optimized real-time pipeline and sends only
small encoded frames (~63 bytes) over Tauri IPC.

## Transport

### Topic Structure

Per-sender topics under a channel namespace:

```
harmony/voice/{channel_id}/{sender_address_hex}
```

- Publishers write to their own sender topic
- Subscribers use wildcard: `harmony/voice/{channel_id}/*`
- Muting a sender = unsubscribing from their specific topic
- One wildcard subscription per channel gets all voice in that channel

### Channel Lifecycle

- `join_voice_channel(channel_id)` — Rust subscribes to `harmony/voice/{channel_id}/*`
- `leave_voice_channel(channel_id)` — Rust unsubscribes
- Dynamic subscription management via `HashMap<String, zenoh::Subscriber>` in event_loop

## Packet Format

```
Byte 0:       Version/flags
              [4 bits] version = 0x1
              [1 bit]  PTT active (1 = speaking, 0 = tail/end-of-transmission)
              [3 bits] reserved (zero)

Bytes 1-2:    Sequence number (u16 big-endian, wraps at 65535)

Bytes 3-6:    Timestamp (u32 big-endian, milliseconds since stream start)

Bytes 7-22:   Sender address hash (16 bytes, same as identity address_hash)

Bytes 23+:    Opus frame payload (~40 bytes at 16kbps/20ms)
```

Total per packet: ~63 bytes. Well under Reticulum's 500-byte MTU.

The PTT-active flag distinguishes active speech from end-of-transmission. On
PTT release, 2-3 tail frames are sent with PTT=0 to give the decoder a clean
end-of-stream signal and clear the receiver's speaking indicator.

Sequence number and timestamp are independent: sequence counts frames (gap
detection in jitter buffer), timestamp tracks wall-clock offset (playback
scheduling). Both set by the sender at encode time.

## Frontend Components

All new files live under `src/lib/voice/`.

### New Files

**`audio-capture.ts`** — Manages AudioContext, getUserMedia (16kHz mono), and
an AudioWorkletProcessor that extracts 20ms PCM frames (320 samples Float32).
Exposes `start()` / `stop()` controlled by PttButton. Fires
`onFrame(pcm: Float32Array)` callback per frame.

**`pcm-capture-processor.ts`** — AudioWorkletProcessor script. Runs in the
audio thread, accumulates samples into 320-sample frames, posts them to the
main thread via MessagePort.

**`opus-codec.ts`** — Wraps opus-wasm. Exposes `encode(pcm: Float32Array): Uint8Array`
and `decode(opus: Uint8Array): Float32Array`. Stateful — Opus encoder/decoder
maintain internal state across frames for compression.

**`voice-sender.ts`** — Orchestrates outbound: takes PCM frames from
audio-capture, encodes via opus-codec, builds the 23-byte header (version,
sequence, timestamp, sender hash), invokes `send_voice_frame` Tauri command.
Manages sequence counter and stream timestamp.

**`voice-receiver.ts`** — Orchestrates inbound: listens for
`voice-frame-received` Tauri events, strips header, decodes Opus, manages
per-sender jitter buffers, schedules decoded PCM into per-sender
AudioBufferSourceNode chains. Inserts silence on sequence gaps. Tracks
per-sender speaking state for UI indicators.

**`jitter-buffer.ts`** — Fixed-delay ring buffer. 4 slots (80ms). Keyed by
sequence number. Play cursor advances every 20ms. Late frames dropped. Missing
frames produce silence.

### Deleted Files

- `src/lib/audio-service.ts` — replaced by `voice/audio-capture.ts`
- `src/lib/audio-service.test.ts` — tests move to voice/ modules

### Unchanged Files

- **PttButton.svelte** — No changes needed. Its `onPttStart`/`onPttStop`
  callbacks already provide the right interface.

## Rust Backend (Tauri Side)

### New File

**`src-tauri/src/voice.rs`** — Voice channel module:
- `VoiceOutbound` struct: `{ channel_id: String, frame: Vec<u8> }` (frame is
  the full header + Opus payload, assembled by the frontend)
- `voice_tx` / `voice_rx` bounded mpsc channel (capacity 100 frames = 2s buffer)
- Subscription management types

### Modified Files

**`src-tauri/src/lib.rs`:**
- Add `voice_tx: mpsc::Sender<VoiceOutbound>` to NodeState
- New Tauri command: `send_voice_frame(channel_id, frame_bytes)` — pushes to voice_tx
- New Tauri command: `join_voice_channel(channel_id)` — tells event loop to subscribe
- New Tauri command: `leave_voice_channel(channel_id)` — unsubscribes

**`src-tauri/src/event_loop.rs`:**
- Add `voice_rx.recv()` arm to the select loop: publishes frame to
  `harmony/voice/{channel_id}/{sender_address}` via Zenoh
- Voice subscription handler: on sample received, emit `voice-frame-received`
  to frontend with raw payload
- Dynamic subscription map: `HashMap<String, zenoh::Subscriber>` keyed by channel_id

## Jitter Buffer

One jitter buffer instance per active sender, managed by voice-receiver.

**Parameters:**
- Buffer depth: 4 slots (80ms at 20ms frame intervals)
- Play interval: 20ms (scheduled via AudioContext.currentTime)

**Mechanics:**
- Incoming frames insert at `sequence_number % 4`
- Play cursor advances every 20ms, reading the next expected sequence
- Slot contains expected frame: decode and schedule for playback
- Slot empty: insert 20ms silence
- Frame arrives after its slot was played: drop (late arrival)
- On stream start: wait 80ms (fill buffer) before beginning playback

**Lifecycle:**
- Created on first frame from a new sender
- Destroyed 2 seconds after last frame from that sender
- Reused if sender resumes within the 2-second window

## Testing Strategy

### Unit Tests

**jitter-buffer.test.ts** — In-order frames, out-of-order arrival, gaps
produce silence, late frames dropped, buffer fill before first playback,
stream timeout and cleanup, sequence wraparound at u16 boundary.

**voice-sender.test.ts** — Header construction: version/flags encoding,
sequence incrementing, timestamp advancing 20ms per frame, PTT flag
set/cleared, tail frames on stop. Mock Tauri invoke.

**voice-receiver.test.ts** — Event parsing, header extraction, per-sender
jitter buffer creation/reuse/cleanup, speaking indicator state transitions.
Mock Tauri event listener and AudioContext.

**opus-codec.test.ts** — Encode/decode roundtrip: verify output is plausible
length and non-silent. Requires opus-wasm loadable in vitest (WASM loader shim
may be needed).

**audio-capture.test.ts** — Lifecycle: start requests getUserMedia, creates
AudioWorklet, stop tears down. Mock navigator.mediaDevices.

### Integration Test (Manual)

1. Two harmony-client instances on the same LAN
2. Client A holds PTT, speaks
3. Client B sees speaking indicator, hears audio
4. Release PTT, indicator clears within ~200ms
5. Verify no audio after release

## Known Limitations

1. **Rawlink jitter interference** — Rawlink bridge nodes
   (`harmony-rawlink/src/bridge.rs:213`) add 100-500ms transmission jitter.
   Voice frames transiting through rawlink bridges will experience added
   latency. Requires voice-aware topic matching to bypass JitterHold for
   `harmony/voice/**`. Separate issue from this slice.

2. **No encryption** — Voice frames ride unencrypted Zenoh pub/sub. Per-channel
   encryption is a broader Zenoh story, not voice-specific.

3. **PTT only** — Full-duplex requires echo cancellation (AEC). Slice 2.

4. **Fixed jitter buffer** — 80ms fixed depth works for local mesh. Multi-hop
   paths with variable latency need adaptive buffering. Slice 2.

5. **No codec2 fallback** — Opus at 16kbps is the only codec. Low-bandwidth
   codec2 fallback for congested paths is Slice 2.

6. **No comfort noise** — Missing frames produce hard silence. Opus PLC
   (packet loss concealment) and comfort noise generation are Slice 2.

7. **No settings UI** — Uses system default mic/speakers. Device picker and
   volume controls are a follow-up.
