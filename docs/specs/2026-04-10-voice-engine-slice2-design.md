# Voice Engine Slice 2: codec2 + Adaptive Jitter Buffer

**Goal:** Add codec2 3200 low-bandwidth voice encoding and adaptive jitter buffering with comfort noise to the existing PTT Opus pipeline, making voice usable over constrained mesh network paths.

**Parent issue:** ZEB-35

**Builds on:** Voice Engine Slice 1 (PR #36) — PTT Opus pipeline, fixed 80ms jitter buffer, Zenoh transport.

## Scope

### In scope

1. codec2 3200 mode via emscripten-compiled WASM
2. `VoiceCodec` interface abstracting Opus and codec2
3. Manual codec toggle (segmented control near PttButton)
4. Packet header codec-type bit (backward compatible)
5. Adaptive jitter buffer (jitter-driven, 40–200ms)
6. Comfort noise generation for missing frames
7. 8kHz audio capture support for codec2
8. Unit tests for all new modules

### Out of scope

- Automatic codec switching (needs telemetry signals — future slice)
- Full-duplex / echo cancellation
- Additional codec2 modes (1600, 700C — 40ms frame sizes)
- Reticulum rawlink transport
- Per-channel encryption
- Settings panel / device picker
- Voice activity detection (VAD)
- Opus PLC (packet loss concealment)
- Persistence of codec selection across sessions

## Packet Header Extension

Current byte 0 layout:

```
[4-bit version=0x1][PTT bit][3 reserved bits]
 bits 7–4           bit 3    bits 2–0
```

New byte 0 layout:

```
[4-bit version=0x1][PTT bit][CODEC bit][2 reserved bits]
 bits 7–4           bit 3    bit 2      bits 1–0
```

- `CODEC=0` (0x00): Opus — backward compatible with Slice 1 packets
- `CODEC=1` (0x04): codec2 3200

Each packet is self-describing. Receivers read the codec bit to select the correct decoder. Mixed-codec streams work naturally (different senders can use different codecs in the same channel).

Header size remains 23 bytes. No version bump needed — existing receivers that ignore reserved bits will still decode the header fields correctly (they just won't understand the payload if it's codec2).

## VoiceCodec Interface

Common abstraction for all voice codecs:

```typescript
interface VoiceCodec {
  init(sampleRate: number, channels: number): Promise<void>;
  encode(pcm: Float32Array): Uint8Array;
  decode(encoded: Uint8Array): Float32Array;
  destroy(): void;
  readonly codecType: 'opus' | 'codec2';
}
```

`OpusCodec` already matches this shape — add `codecType: 'opus'` and have it implement the interface. `Codec2Codec` implements the same interface with `codecType: 'codec2'`.

`VoiceSender` and `VoiceReceiver` work against `VoiceCodec` rather than `OpusCodec` directly.

## codec2 3200 WASM Integration

### Build

Compile codec2 C source with emscripten. The codec2 library is compact (~15 core files). Build script in `build/codec2/`:

- Input: codec2 C source (vendored or fetched at build time)
- Output: `codec2.js` + `codec2.wasm` placed in `src/lib/voice/`
- Exported functions: `codec2_create(mode)`, `codec2_encode(state, bits, speech)`, `codec2_decode(state, speech, bits)`, `codec2_destroy(state)`, `codec2_samples_per_frame(state)`, `codec2_bits_per_frame(state)`
- Emscripten flags: `-O2`, `MODULARIZE=1`, `EXPORT_ES6=1`, `ALLOW_MEMORY_GROWTH=0` (fixed heap for predictable performance)

### Codec2Codec wrapper

```typescript
class Codec2Codec implements VoiceCodec {
  readonly codecType = 'codec2';

  async init(sampleRate: number, channels: number): Promise<void>;
  encode(pcm: Float32Array): Uint8Array;   // float32→int16, codec2_encode
  decode(encoded: Uint8Array): Float32Array; // codec2_decode, int16→float32
  destroy(): void;                          // codec2_destroy + free heap buffers
}
```

Heap management follows the opusscript pattern:
- `malloc()` input/output buffers on init
- Reuse buffers across encode/decode calls
- `free()` + `codec2_destroy()` on destroy

### codec2 3200 parameters

| Parameter | Value |
|-----------|-------|
| Mode | 3200 (highest quality codec2 mode) |
| Sample rate | 8,000 Hz |
| Frame duration | 20ms |
| Samples per frame | 160 |
| Bits per frame | 64 |
| Bytes per frame | 8 |
| Bitrate | 3,200 bps |

Compare Opus: 16kHz, 320 samples/frame, ~40 bytes/frame, 16kbps. codec2 is 5x lower bandwidth and 5x smaller payload.

### Sample rate handling

- Opus uses 16kHz capture; codec2 uses 8kHz
- `AudioCapture` accepts `sampleRate` parameter, passes to `getUserMedia` and `AudioContext`
- `PcmCaptureProcessor` derives frame size from sample rate: `sampleRate * 0.02` (320 at 16kHz, 160 at 8kHz)
- On codec switch: sender stops, AudioCapture restarts with new sample rate, new codec initialized
- No resampling — capture at native rate for each codec

## Adaptive Jitter Buffer

Replaces the fixed-depth `JitterBuffer` with `AdaptiveJitterBuffer`.

### Jitter estimation

Track inter-arrival jitter using RFC 3550 exponentially weighted moving average:

```
arrival_delta = now - last_arrival_time
expected_delta = frameMs  // 20ms
deviation = |arrival_delta - expected_delta|
jitter = jitter + (deviation - jitter) / 16
```

Updated on every `insert()` call. `last_arrival_time` recorded via `performance.now()`.

### Depth adaptation

```
target_depth = ceil(jitter_ms * 3 / frameMs)
target_depth = clamp(target_depth, minDepth, maxDepth)
```

| Parameter | Value | Rationale |
|-----------|-------|-----------|
| `minDepth` | 2 (40ms) | Minimum viable buffering |
| `maxDepth` | 10 (200ms) | Upper bound on latency |
| Jitter multiplier | 3x | Standard VoIP heuristic — cover 99.7% of jitter variance |
| Shrink delay | 50 consecutive clean advances (~1 second) | Prevent flapping |

**Grow:** Immediately when `target_depth > current_depth`. Allocate additional slots, no playback disruption.

**Shrink:** Only after `shrinkDelay` consecutive `advance()` calls return non-null frames. Trim excess slots as playhead naturally advances past them.

### Interface

```typescript
class AdaptiveJitterBuffer {
  constructor(config: { minDepth: number; maxDepth: number; frameMs: number });
  insert(seq: number, pcm: Float32Array): void;
  advance(): Float32Array | null;
  isReady(): boolean;
  reset(): void;
  getDepth(): number;      // Current buffer depth in frames
  getJitterMs(): number;   // Current estimated jitter in ms
}
```

Same public API as `JitterBuffer` (insert/advance/reset/isReady) plus diagnostic getters. Drop-in replacement in `VoiceReceiver`.

### Fill period

Initial fill uses `minDepth` (2 frames = 40ms) for fast start. If jitter causes depth to grow during fill, the fill period extends to match the new depth.

## Comfort Noise

When `advance()` returns null after the fill period, generate comfort noise instead of hard silence.

### Generator

```typescript
function generateComfortNoise(samples: number, level: number): Float32Array;
```

- Produces low-level white noise at the given amplitude
- `level` default: 0.005 (~-46dB) — barely audible, masks dead-air gaps
- Uses deterministic PRNG (mulberry32) seeded once at construction with a fixed seed for reproducible test output
- No Web Audio API dependency — pure math, works in any JS environment

### Integration

In `VoiceReceiver`'s playback loop:
- `advance()` returns null AND buffer is past fill period → play `generateComfortNoise(frameSize, 0.005)`
- During fill period → still null (no playback)
- After sender idle timeout (stream ended) → true silence (no comfort noise for ended streams)

### Why not Opus PLC?

Opus has built-in packet loss concealment via `decode(null)`, but opusscript's emscripten build doesn't reliably expose this. Comfort noise is codec-agnostic (works identically for Opus and codec2), simpler, and predictable.

## UI: Codec Toggle

### CodecToggle.svelte

Segmented control with two options: **Opus** and **codec2**.

- Positioned adjacent to PttButton in the same toolbar/row
- Default selection: Opus
- Disabled while PTT is active (cannot switch mid-transmission)
- On selection change: fires `onCodecChange` callback with `'opus'` or `'codec2'`

### Accessibility

- `role="radiogroup"` container with `aria-label="Voice codec"`
- Each option: `role="radio"` with `aria-checked`
- Keyboard: arrow keys navigate, Enter/Space selects (with `preventDefault` on Space to prevent scroll)
- Focus visible indicator
- Disabled state communicated via `aria-disabled`

### State flow

1. User selects codec in toggle
2. Parent component stores selection in local state
3. On next PTT press, `VoiceSender` is initialized with the selected `VoiceCodec` and corresponding capture sample rate
4. No persistence across page reloads (defaults to Opus)

### Jitter diagnostics (stretch goal)

Small text below PTT area: "Buffer: 60ms | Jitter: 12ms"
- Polled from `AdaptiveJitterBuffer.getDepth()` and `getJitterMs()` on 1-second interval
- Informational only — skip if implementation time is tight

## File Structure

### New files

| File | Responsibility |
|------|---------------|
| `src/lib/voice/voice-codec.ts` | `VoiceCodec` interface definition |
| `src/lib/voice/codec2-codec.ts` | Codec2Codec class wrapping emscripten WASM |
| `src/lib/voice/codec2.js` + `codec2.wasm` | Emscripten-compiled codec2 3200 |
| `src/lib/voice/adaptive-jitter-buffer.ts` | Adaptive jitter buffer |
| `src/lib/voice/comfort-noise.ts` | Comfort noise generator |
| `src/lib/components/CodecToggle.svelte` | Codec selection UI |
| `build/codec2/Makefile` | Emscripten build script for codec2 WASM |

### Modified files

| File | Change |
|------|--------|
| `src/lib/voice/voice-packet.ts` | Encode/decode codec bit in byte 0 |
| `src/lib/voice/opus-codec.ts` | Implement `VoiceCodec` interface, add `codecType` |
| `src/lib/voice/voice-sender.ts` | Accept `VoiceCodec`, set codec header bit, configurable sample rate |
| `src/lib/voice/voice-receiver.ts` | Read codec bit, per-codec decoders, adaptive buffer, comfort noise |
| `src/lib/voice/audio-capture.ts` | Accept `sampleRate` parameter |
| `src/lib/voice/pcm-capture-processor.ts` | Derive frame size from sample rate |
| `src/lib/components/PttButton.svelte` | Integrate CodecToggle, pass codec selection to sender |

### New test files

| File | Key scenarios |
|------|--------------|
| `src/lib/voice/__tests__/codec2-codec.test.ts` | Encode/decode roundtrip, destroy cleanup, heap management |
| `src/lib/voice/__tests__/adaptive-jitter-buffer.test.ts` | Depth growth on jitter spike, shrink after stable run, min/max clamp, fill adaptation, wraparound |
| `src/lib/voice/__tests__/comfort-noise.test.ts` | Amplitude range, deterministic output, correct sample count |
| `src/lib/components/__tests__/CodecToggle.test.ts` | Selection state, disabled during PTT, keyboard nav, ARIA roles |

### Modified test files

| File | Changes |
|------|---------|
| `voice-packet.test.ts` | Codec bit encode/decode, backward compatibility with CODEC=0 |
| `voice-sender.test.ts` | VoiceCodec interface usage, codec bit in headers, codec2 frame size |
| `voice-receiver.test.ts` | Mixed-codec streams, per-codec decoders, comfort noise on missing frames |

## Testing Strategy

All tests use vitest + jsdom (same as Slice 1). Mock patterns:

- **codec2 WASM mock:** Mock the emscripten module loader. `Codec2Codec` tests verify the wrapper logic (buffer management, int16 conversion, destroy cleanup) without loading real WASM.
- **Adaptive jitter buffer:** Synthetic jitter patterns — stable (low jitter), spiking (sudden increase), settling (gradual decrease), wraparound (u16 sequence boundary).
- **Comfort noise:** Verify output amplitude is within expected range, sample count matches request, deterministic PRNG produces identical output for same seed.
- **CodecToggle:** @testing-library/svelte for component testing. Verify ARIA attributes, keyboard interaction, disabled state during PTT.
- **Integration (voice-sender/receiver):** Verify codec bit round-trips through header, correct decoder selected per packet, codec switch mid-session produces clean break.

## Known Limitations

1. **No automatic codec switching** — manual only. Automatic requires network telemetry (RTT, packet loss) not yet available.
2. **codec2 3200 only** — lower modes (1600, 700C) use 40ms frames requiring frame-size negotiation.
3. **No codec negotiation protocol** — packets are self-describing but there's no capability advertisement. Receivers that lack codec2 will receive unintelligible payloads from codec2 senders.
4. **Browser AEC not leveraged** — still PTT only, no full-duplex.
5. **Comfort noise is static** — fixed amplitude, no adaptation to room noise level.
6. **No codec selection persistence** — resets to Opus on page reload.
7. **Jitter diagnostics are stretch goal** — may be deferred to future slice.
