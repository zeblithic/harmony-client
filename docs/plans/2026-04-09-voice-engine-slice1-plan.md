# Voice Engine Slice 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** End-to-end push-to-talk voice over Zenoh pub/sub — browser captures audio, encodes Opus, relays through Tauri/Rust to Zenoh, receivers decode and play back through a fixed jitter buffer.

**Architecture:** Browser-side audio capture via AudioWorklet + Opus WASM encoding. Rust/Tauri layer is a dumb relay between frontend IPC and Zenoh pub/sub. Per-sender Zenoh topics (`harmony/voice/{channel}/{sender}`). Fixed 80ms jitter buffer in the frontend per active sender.

**Tech Stack:** TypeScript, Svelte 5, Web Audio API (AudioWorklet), opus-wasm (`@aspect-build/opus-wasm` or equivalent), Tauri v2 IPC, Rust (tokio mpsc channels, zenoh pub/sub), vitest + jsdom

**Spec:** `docs/specs/2026-04-09-voice-engine-slice1-design.md`

---

## File Structure

### New Frontend Files (all under `src/lib/voice/`)

| File | Responsibility |
|------|---------------|
| `jitter-buffer.ts` | Fixed-delay ring buffer: 4 slots, sequence-indexed, 20ms play interval |
| `jitter-buffer.test.ts` | Unit tests for jitter buffer mechanics |
| `voice-packet.ts` | Header encode/decode: version, flags, sequence, timestamp, sender hash |
| `voice-packet.test.ts` | Unit tests for packet header roundtrip |
| `opus-codec.ts` | Opus WASM wrapper: `encode(pcm) → opus`, `decode(opus) → pcm` |
| `opus-codec.test.ts` | Encode/decode roundtrip test |
| `audio-capture.ts` | AudioWorklet-based PCM capture: start/stop, 20ms frame callbacks |
| `audio-capture.test.ts` | Lifecycle tests (mocked getUserMedia + AudioWorklet) |
| `pcm-capture-processor.ts` | AudioWorkletProcessor: accumulates 320 samples, posts frames |
| `voice-sender.ts` | Outbound orchestrator: capture → encode → packet → Tauri IPC |
| `voice-sender.test.ts` | Header construction, sequence/timestamp, tail frames |
| `voice-receiver.ts` | Inbound orchestrator: Tauri event → decode → jitter buffer → playback |
| `voice-receiver.test.ts` | Event parsing, per-sender buffers, speaking indicators |

### Deleted Frontend Files

| File | Reason |
|------|--------|
| `src/lib/audio-service.ts` | Replaced by `voice/audio-capture.ts` |
| `src/lib/audio-service.test.ts` | Tests move to `voice/audio-capture.test.ts` |

### New Rust Files

| File | Responsibility |
|------|---------------|
| `src-tauri/src/voice.rs` | `VoiceOutbound` struct, `VoiceChannelRequest` enum for join/leave |

### Modified Rust Files

| File | Changes |
|------|---------|
| `src-tauri/src/lib.rs` | Add `voice_tx` to `NodeState`, add `send_voice_frame` / `join_voice_channel` / `leave_voice_channel` commands, register in `invoke_handler` |
| `src-tauri/src/event_loop.rs` | Add `voice_rx` parameter, `voice_channel_rx` parameter, voice publish arm in select loop, dynamic voice subscription management, `voice-frame-received` emit |

---

## Task 1: Jitter Buffer

The jitter buffer is the most testable component and has zero external dependencies. Start here.

**Files:**
- Create: `src/lib/voice/jitter-buffer.ts`
- Create: `src/lib/voice/jitter-buffer.test.ts`

- [ ] **Step 1: Write failing tests for jitter buffer**

Create `src/lib/voice/jitter-buffer.test.ts`:

```typescript
import { describe, it, expect, beforeEach } from 'vitest';
import { JitterBuffer } from './jitter-buffer';

describe('JitterBuffer', () => {
  const FRAME_MS = 20;
  const DEPTH = 4; // 80ms
  let buffer: JitterBuffer;

  beforeEach(() => {
    buffer = new JitterBuffer(DEPTH, FRAME_MS);
  });

  it('is not ready before buffer fill period', () => {
    buffer.insert(0, new Float32Array(320));
    expect(buffer.isReady()).toBe(false);
  });

  it('becomes ready after fill period elapses', () => {
    buffer.insert(0, new Float32Array(320));
    // Advance play clock past fill period (DEPTH * FRAME_MS = 80ms)
    for (let i = 0; i < DEPTH; i++) {
      buffer.advance();
    }
    expect(buffer.isReady()).toBe(true);
  });

  it('plays frames in sequence order', () => {
    const frame0 = new Float32Array(320).fill(0.1);
    const frame1 = new Float32Array(320).fill(0.2);
    const frame2 = new Float32Array(320).fill(0.3);
    const frame3 = new Float32Array(320).fill(0.4);

    buffer.insert(0, frame0);
    buffer.insert(1, frame1);
    buffer.insert(2, frame2);
    buffer.insert(3, frame3);

    // Fill period
    for (let i = 0; i < DEPTH; i++) buffer.advance();

    const out0 = buffer.advance();
    expect(out0?.[0]).toBeCloseTo(0.1);
    const out1 = buffer.advance();
    expect(out1?.[0]).toBeCloseTo(0.2);
  });

  it('returns null for missing frames (silence)', () => {
    buffer.insert(0, new Float32Array(320).fill(0.5));
    // Skip seq 1
    buffer.insert(2, new Float32Array(320).fill(0.7));

    for (let i = 0; i < DEPTH; i++) buffer.advance();

    const out0 = buffer.advance();
    expect(out0).not.toBeNull();
    const out1 = buffer.advance(); // seq 1 missing
    expect(out1).toBeNull();
    const out2 = buffer.advance();
    expect(out2).not.toBeNull();
  });

  it('handles out-of-order arrival', () => {
    // Insert seq 2 before seq 1
    buffer.insert(2, new Float32Array(320).fill(0.3));
    buffer.insert(0, new Float32Array(320).fill(0.1));
    buffer.insert(1, new Float32Array(320).fill(0.2));
    buffer.insert(3, new Float32Array(320).fill(0.4));

    for (let i = 0; i < DEPTH; i++) buffer.advance();

    const out0 = buffer.advance();
    expect(out0?.[0]).toBeCloseTo(0.1);
    const out1 = buffer.advance();
    expect(out1?.[0]).toBeCloseTo(0.2);
    const out2 = buffer.advance();
    expect(out2?.[0]).toBeCloseTo(0.3);
  });

  it('drops late frames (already played past that sequence)', () => {
    buffer.insert(0, new Float32Array(320).fill(0.1));
    buffer.insert(1, new Float32Array(320).fill(0.2));

    for (let i = 0; i < DEPTH; i++) buffer.advance();

    buffer.advance(); // plays seq 0
    buffer.advance(); // plays seq 1

    // Late frame for seq 0 — should be silently ignored
    buffer.insert(0, new Float32Array(320).fill(0.9));

    // Next advance should be seq 2 (missing → null), not the late seq 0
    const out = buffer.advance();
    expect(out).toBeNull();
  });

  it('handles sequence wraparound at u16 boundary', () => {
    const buf = new JitterBuffer(DEPTH, FRAME_MS);
    // Start near u16 max
    const startSeq = 65534;
    buf.insert(startSeq, new Float32Array(320).fill(0.1));
    buf.insert(startSeq + 1, new Float32Array(320).fill(0.2)); // 65535
    buf.insert((startSeq + 2) & 0xFFFF, new Float32Array(320).fill(0.3)); // 0
    buf.insert((startSeq + 3) & 0xFFFF, new Float32Array(320).fill(0.4)); // 1

    for (let i = 0; i < DEPTH; i++) buf.advance();

    const out0 = buf.advance();
    expect(out0?.[0]).toBeCloseTo(0.1);
    const out1 = buf.advance();
    expect(out1?.[0]).toBeCloseTo(0.2);
    const out2 = buf.advance();
    expect(out2?.[0]).toBeCloseTo(0.3);
    const out3 = buf.advance();
    expect(out3?.[0]).toBeCloseTo(0.4);
  });

  it('reset clears all state', () => {
    buffer.insert(0, new Float32Array(320).fill(0.5));
    for (let i = 0; i < DEPTH; i++) buffer.advance();
    buffer.advance();

    buffer.reset();
    expect(buffer.isReady()).toBe(false);
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine && npx vitest run src/lib/voice/jitter-buffer.test.ts`
Expected: FAIL — module `./jitter-buffer` not found

- [ ] **Step 3: Implement JitterBuffer**

Create `src/lib/voice/jitter-buffer.ts`:

```typescript
/**
 * Fixed-delay jitter buffer for voice frames.
 *
 * Holds `depth` slots. Incoming frames are inserted by sequence number.
 * A play cursor advances every call to advance(), returning the next
 * expected frame or null (silence) if missing. Frames arriving after
 * their play deadline are dropped.
 */
export class JitterBuffer {
  private slots: (Float32Array | null)[];
  private depth: number;
  private frameMs: number;
  /** Next sequence number the play cursor expects. */
  private playSeq = 0;
  /** Whether the initial fill period has elapsed. */
  private ready = false;
  /** Counts advance() calls during fill period. */
  private fillCount = 0;
  /** Whether we've received the first frame (sets playSeq). */
  private started = false;

  constructor(depth: number, frameMs: number) {
    this.depth = depth;
    this.frameMs = frameMs;
    this.slots = new Array(depth).fill(null);
  }

  /** Insert a decoded PCM frame at the given sequence number. */
  insert(seq: number, pcm: Float32Array): void {
    if (!this.started) {
      this.playSeq = seq;
      this.started = true;
    }

    // Check if this frame is late (already played past it).
    // Use modular distance to handle u16 wraparound.
    const dist = (seq - this.playSeq + 0x10000) & 0xFFFF;
    if (dist >= 0x8000) {
      // seq is behind playSeq — late frame, drop it
      return;
    }
    if (dist >= this.depth) {
      // Too far ahead — would overwrite unplayed slots. Drop for now.
      // (In practice this means severe network disruption.)
      return;
    }

    const slot = seq % this.depth;
    this.slots[slot] = pcm;
  }

  /** Whether the buffer has finished its initial fill period. */
  isReady(): boolean {
    return this.ready;
  }

  /**
   * Advance the play cursor by one frame.
   *
   * During the fill period (first `depth` calls), returns null and
   * doesn't consume frames. After fill, returns the frame at the
   * current play position (or null for silence if missing).
   */
  advance(): Float32Array | null {
    if (!this.started) return null;

    if (!this.ready) {
      this.fillCount++;
      if (this.fillCount >= this.depth) {
        this.ready = true;
      }
      return null;
    }

    const slot = this.playSeq % this.depth;
    const frame = this.slots[slot];
    this.slots[slot] = null;
    this.playSeq = (this.playSeq + 1) & 0xFFFF;
    return frame;
  }

  /** Reset to initial state. */
  reset(): void {
    this.slots = new Array(this.depth).fill(null);
    this.playSeq = 0;
    this.ready = false;
    this.fillCount = 0;
    this.started = false;
  }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine && npx vitest run src/lib/voice/jitter-buffer.test.ts`
Expected: All 8 tests PASS

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine
git add src/lib/voice/jitter-buffer.ts src/lib/voice/jitter-buffer.test.ts
git commit -m "feat(voice): add fixed-delay jitter buffer with tests"
```

---

## Task 2: Voice Packet Header

Encode/decode the 23-byte voice packet header. Pure data transformation, no dependencies.

**Files:**
- Create: `src/lib/voice/voice-packet.ts`
- Create: `src/lib/voice/voice-packet.test.ts`

- [ ] **Step 1: Write failing tests for packet header**

Create `src/lib/voice/voice-packet.test.ts`:

```typescript
import { describe, it, expect } from 'vitest';
import {
  encodeHeader,
  decodeHeader,
  HEADER_SIZE,
  VOICE_VERSION,
} from './voice-packet';

describe('voice-packet', () => {
  const senderHash = new Uint8Array(16);
  for (let i = 0; i < 16; i++) senderHash[i] = 0xAA + i;

  it('HEADER_SIZE is 23', () => {
    expect(HEADER_SIZE).toBe(23);
  });

  it('encodes and decodes a header roundtrip', () => {
    const header = encodeHeader({
      pttActive: true,
      sequence: 42,
      timestamp: 1000,
      senderHash,
    });

    expect(header.byteLength).toBe(HEADER_SIZE);

    const decoded = decodeHeader(header);
    expect(decoded.version).toBe(VOICE_VERSION);
    expect(decoded.pttActive).toBe(true);
    expect(decoded.sequence).toBe(42);
    expect(decoded.timestamp).toBe(1000);
    expect(decoded.senderHash).toEqual(senderHash);
  });

  it('encodes PTT inactive flag', () => {
    const header = encodeHeader({
      pttActive: false,
      sequence: 0,
      timestamp: 0,
      senderHash,
    });
    const decoded = decodeHeader(header);
    expect(decoded.pttActive).toBe(false);
  });

  it('encodes max sequence number (65535)', () => {
    const header = encodeHeader({
      pttActive: true,
      sequence: 65535,
      timestamp: 0,
      senderHash,
    });
    const decoded = decodeHeader(header);
    expect(decoded.sequence).toBe(65535);
  });

  it('encodes large timestamp', () => {
    const header = encodeHeader({
      pttActive: true,
      sequence: 0,
      timestamp: 0xFFFFFFFF,
      senderHash,
    });
    const decoded = decodeHeader(header);
    expect(decoded.timestamp).toBe(0xFFFFFFFF);
  });

  it('builds a full packet with opus payload', () => {
    const opusFrame = new Uint8Array([0x01, 0x02, 0x03, 0x04]);
    const header = encodeHeader({
      pttActive: true,
      sequence: 1,
      timestamp: 20,
      senderHash,
    });
    const packet = new Uint8Array(header.byteLength + opusFrame.byteLength);
    packet.set(header, 0);
    packet.set(opusFrame, header.byteLength);

    expect(packet.byteLength).toBe(HEADER_SIZE + 4);

    const decoded = decodeHeader(packet);
    expect(decoded.sequence).toBe(1);

    // Opus payload is everything after the header
    const payload = packet.slice(HEADER_SIZE);
    expect(payload).toEqual(opusFrame);
  });

  it('rejects buffer shorter than HEADER_SIZE', () => {
    expect(() => decodeHeader(new Uint8Array(10))).toThrow();
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine && npx vitest run src/lib/voice/voice-packet.test.ts`
Expected: FAIL — module `./voice-packet` not found

- [ ] **Step 3: Implement voice packet encode/decode**

Create `src/lib/voice/voice-packet.ts`:

```typescript
/**
 * Voice packet header: 23 bytes.
 *
 * Byte 0:      [4 bits version=0x1][1 bit PTT active][3 bits reserved]
 * Bytes 1-2:   Sequence number (u16 big-endian)
 * Bytes 3-6:   Timestamp (u32 big-endian, ms since stream start)
 * Bytes 7-22:  Sender address hash (16 bytes)
 */

export const HEADER_SIZE = 23;
export const VOICE_VERSION = 0x01;

export interface VoiceHeaderFields {
  pttActive: boolean;
  sequence: number;
  timestamp: number;
  senderHash: Uint8Array;
}

export interface DecodedVoiceHeader {
  version: number;
  pttActive: boolean;
  sequence: number;
  timestamp: number;
  senderHash: Uint8Array;
}

export function encodeHeader(fields: VoiceHeaderFields): Uint8Array {
  const buf = new Uint8Array(HEADER_SIZE);
  const view = new DataView(buf.buffer);

  // Byte 0: version (high nibble) | PTT flag (bit 3) | reserved (bits 0-2)
  const flags = (VOICE_VERSION << 4) | (fields.pttActive ? 0x08 : 0x00);
  buf[0] = flags;

  // Bytes 1-2: sequence (u16 BE)
  view.setUint16(1, fields.sequence & 0xFFFF, false);

  // Bytes 3-6: timestamp (u32 BE)
  view.setUint32(3, fields.timestamp >>> 0, false);

  // Bytes 7-22: sender hash
  buf.set(fields.senderHash.subarray(0, 16), 7);

  return buf;
}

export function decodeHeader(buf: Uint8Array): DecodedVoiceHeader {
  if (buf.byteLength < HEADER_SIZE) {
    throw new Error(
      `Voice packet too short: ${buf.byteLength} bytes, need ${HEADER_SIZE}`
    );
  }

  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);

  const flagsByte = buf[0];
  const version = (flagsByte >> 4) & 0x0F;
  const pttActive = (flagsByte & 0x08) !== 0;
  const sequence = view.getUint16(1, false);
  const timestamp = view.getUint32(3, false);
  const senderHash = buf.slice(7, 23);

  return { version, pttActive, sequence, timestamp, senderHash };
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine && npx vitest run src/lib/voice/voice-packet.test.ts`
Expected: All 7 tests PASS

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine
git add src/lib/voice/voice-packet.ts src/lib/voice/voice-packet.test.ts
git commit -m "feat(voice): add voice packet header encode/decode"
```

---

## Task 3: Opus Codec Wrapper

Wrap opus-wasm for encode/decode. This task adds the npm dependency and creates a thin TypeScript wrapper.

**Files:**
- Modify: `package.json` (add opus-wasm dependency)
- Create: `src/lib/voice/opus-codec.ts`
- Create: `src/lib/voice/opus-codec.test.ts`

- [ ] **Step 1: Install opus-wasm dependency**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine
npm install opus-wasm
```

Note: If `opus-wasm` is not available or problematic, try `@aspect-build/opus-wasm` or `libopus-wasm`. The wrapper interface stays the same regardless of which package provides the WASM binary. If no suitable npm package exists, we can vendor a pre-built `opus.wasm` and write a minimal JS loader. The implementer should check npm availability and pick the first working option.

- [ ] **Step 2: Write failing tests for opus codec wrapper**

Create `src/lib/voice/opus-codec.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { OpusCodec } from './opus-codec';

// Mock the opus-wasm module since WASM may not load in jsdom.
// Tests verify our wrapper logic, not the WASM binary itself.
vi.mock('opus-wasm', () => {
  return {
    default: {
      createEncoder: vi.fn(() => ({
        encode: vi.fn((pcm: Float32Array) => {
          // Fake compression: return 40 bytes regardless of input
          return new Uint8Array(40);
        }),
        destroy: vi.fn(),
      })),
      createDecoder: vi.fn(() => ({
        decode: vi.fn((opus: Uint8Array) => {
          // Fake decompression: return 320 samples of silence
          return new Float32Array(320);
        }),
        destroy: vi.fn(),
      })),
    },
  };
});

describe('OpusCodec', () => {
  let codec: OpusCodec;

  beforeEach(async () => {
    codec = new OpusCodec();
    await codec.init(16000, 1);
  });

  it('encodes PCM to Opus bytes', () => {
    const pcm = new Float32Array(320).fill(0.5);
    const opus = codec.encode(pcm);
    expect(opus).toBeInstanceOf(Uint8Array);
    expect(opus.byteLength).toBeGreaterThan(0);
  });

  it('decodes Opus bytes to PCM', () => {
    const opus = new Uint8Array(40);
    const pcm = codec.decode(opus);
    expect(pcm).toBeInstanceOf(Float32Array);
    expect(pcm.length).toBe(320);
  });

  it('throws if encode called before init', () => {
    const uninit = new OpusCodec();
    expect(() => uninit.encode(new Float32Array(320))).toThrow('not initialized');
  });

  it('throws if decode called before init', () => {
    const uninit = new OpusCodec();
    expect(() => uninit.decode(new Uint8Array(40))).toThrow('not initialized');
  });

  it('destroy cleans up resources', () => {
    codec.destroy();
    expect(() => codec.encode(new Float32Array(320))).toThrow('not initialized');
  });
});
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine && npx vitest run src/lib/voice/opus-codec.test.ts`
Expected: FAIL — module `./opus-codec` not found

- [ ] **Step 4: Implement OpusCodec wrapper**

Create `src/lib/voice/opus-codec.ts`:

```typescript
/**
 * Thin wrapper around opus-wasm for voice encoding/decoding.
 *
 * Call init() once with sample rate and channel count before
 * using encode/decode. Call destroy() when done.
 *
 * The exact opus-wasm package may vary — this wrapper isolates
 * the rest of the voice pipeline from the specific WASM binding.
 */

// The import path depends on which opus-wasm package is installed.
// The implementer should adjust this import to match the actual package.
// eslint-disable-next-line @typescript-eslint/no-explicit-any
let opusModule: any = null;

export class OpusCodec {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  private encoder: any = null;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  private decoder: any = null;
  private initialized = false;

  /**
   * Initialize the Opus encoder and decoder.
   *
   * @param sampleRate Sample rate in Hz (16000 for voice)
   * @param channels Number of audio channels (1 for mono)
   */
  async init(sampleRate: number, channels: number): Promise<void> {
    if (!opusModule) {
      // Dynamic import so the WASM binary is only loaded when needed.
      const mod = await import('opus-wasm');
      opusModule = mod.default ?? mod;
    }

    this.encoder = opusModule.createEncoder(sampleRate, channels);
    this.decoder = opusModule.createDecoder(sampleRate, channels);
    this.initialized = true;
  }

  /** Encode 20ms of PCM (320 Float32 samples at 16kHz mono) to Opus. */
  encode(pcm: Float32Array): Uint8Array {
    if (!this.initialized || !this.encoder) {
      throw new Error('OpusCodec not initialized');
    }
    return this.encoder.encode(pcm);
  }

  /** Decode an Opus frame back to PCM (320 Float32 samples at 16kHz mono). */
  decode(opus: Uint8Array): Float32Array {
    if (!this.initialized || !this.decoder) {
      throw new Error('OpusCodec not initialized');
    }
    return this.decoder.decode(opus);
  }

  /** Release encoder and decoder resources. */
  destroy(): void {
    this.encoder?.destroy?.();
    this.decoder?.destroy?.();
    this.encoder = null;
    this.decoder = null;
    this.initialized = false;
  }
}
```

**Important note for the implementer:** The `opus-wasm` import and API (`createEncoder`, `createDecoder`, `.encode()`, `.decode()`, `.destroy()`) are based on common opus-wasm packages. When you install the actual package, check its README and adjust the import path and method names. The test mock defines the expected interface — make the wrapper match it. If the real API differs (e.g., uses `Encoder` class constructor instead of `createEncoder` factory), update both the wrapper and the mock consistently.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine && npx vitest run src/lib/voice/opus-codec.test.ts`
Expected: All 5 tests PASS

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine
git add package.json package-lock.json src/lib/voice/opus-codec.ts src/lib/voice/opus-codec.test.ts
git commit -m "feat(voice): add opus codec WASM wrapper"
```

---

## Task 4: Audio Capture (AudioWorklet)

Replace the stub `audio-service.ts` with a real AudioWorklet-based capture service.

**Files:**
- Create: `src/lib/voice/pcm-capture-processor.ts`
- Create: `src/lib/voice/audio-capture.ts`
- Create: `src/lib/voice/audio-capture.test.ts`
- Delete: `src/lib/audio-service.ts`
- Delete: `src/lib/audio-service.test.ts`

- [ ] **Step 1: Create the AudioWorkletProcessor**

Create `src/lib/voice/pcm-capture-processor.ts`:

```typescript
/**
 * AudioWorkletProcessor that accumulates input samples into
 * fixed-size frames (320 samples = 20ms at 16kHz) and posts
 * them to the main thread via MessagePort.
 *
 * This file runs in the AudioWorklet thread, not the main thread.
 * It must be loaded via audioContext.audioWorklet.addModule().
 */

const FRAME_SIZE = 320; // 20ms at 16kHz

class PcmCaptureProcessor extends AudioWorkletProcessor {
  private buffer: Float32Array = new Float32Array(FRAME_SIZE);
  private offset = 0;

  process(inputs: Float32Array[][]): boolean {
    const input = inputs[0]?.[0]; // mono channel 0
    if (!input) return true;

    let pos = 0;
    while (pos < input.length) {
      const remaining = FRAME_SIZE - this.offset;
      const toCopy = Math.min(remaining, input.length - pos);
      this.buffer.set(input.subarray(pos, pos + toCopy), this.offset);
      this.offset += toCopy;
      pos += toCopy;

      if (this.offset === FRAME_SIZE) {
        // Post a copy — the buffer is reused for the next frame
        this.port.postMessage(this.buffer.slice());
        this.offset = 0;
      }
    }

    return true;
  }
}

registerProcessor('pcm-capture-processor', PcmCaptureProcessor);
```

- [ ] **Step 2: Write failing tests for audio capture**

Create `src/lib/voice/audio-capture.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { AudioCapture } from './audio-capture';

function createMockAudioContext() {
  const workletNode = {
    port: { onmessage: null as ((e: MessageEvent) => void) | null },
    connect: vi.fn(),
    disconnect: vi.fn(),
  };
  const source = { connect: vi.fn(), disconnect: vi.fn() };
  return {
    ctx: {
      createMediaStreamSource: vi.fn().mockReturnValue(source),
      audioWorklet: {
        addModule: vi.fn().mockResolvedValue(undefined),
      },
      close: vi.fn().mockResolvedValue(undefined),
      sampleRate: 16000,
      destination: {},
    },
    workletNode,
    source,
  };
}

function createMockStream() {
  return {
    getTracks: vi.fn().mockReturnValue([{ stop: vi.fn() }]),
  };
}

describe('AudioCapture', () => {
  let mockCtx: ReturnType<typeof createMockAudioContext>;
  let mockStream: ReturnType<typeof createMockStream>;

  beforeEach(() => {
    mockCtx = createMockAudioContext();
    mockStream = createMockStream();
    Object.defineProperty(global.navigator, 'mediaDevices', {
      value: {
        getUserMedia: vi.fn().mockResolvedValue(mockStream),
      },
      writable: true,
      configurable: true,
    });
  });

  it('is not active initially', () => {
    const capture = new AudioCapture();
    expect(capture.isActive()).toBe(false);
  });

  it('start requests microphone at 16kHz mono', async () => {
    const capture = new AudioCapture();
    await capture.start(
      vi.fn(),
      () => mockCtx.ctx as unknown as AudioContext,
      () => mockCtx.workletNode as unknown as AudioWorkletNode,
    );
    expect(navigator.mediaDevices.getUserMedia).toHaveBeenCalledWith({
      audio: { sampleRate: 16000, channelCount: 1, echoCancellation: false },
    });
    expect(capture.isActive()).toBe(true);
  });

  it('stop releases resources', async () => {
    const capture = new AudioCapture();
    await capture.start(
      vi.fn(),
      () => mockCtx.ctx as unknown as AudioContext,
      () => mockCtx.workletNode as unknown as AudioWorkletNode,
    );
    await capture.stop();
    expect(capture.isActive()).toBe(false);
    expect(mockCtx.ctx.close).toHaveBeenCalled();
  });

  it('fires onFrame when worklet posts a message', async () => {
    const onFrame = vi.fn();
    const capture = new AudioCapture();
    await capture.start(
      onFrame,
      () => mockCtx.ctx as unknown as AudioContext,
      () => mockCtx.workletNode as unknown as AudioWorkletNode,
    );

    // Simulate worklet posting a PCM frame
    const frame = new Float32Array(320).fill(0.5);
    mockCtx.workletNode.port.onmessage?.({ data: frame } as MessageEvent);

    expect(onFrame).toHaveBeenCalledWith(frame);
  });

  it('start is idempotent', async () => {
    const capture = new AudioCapture();
    await capture.start(
      vi.fn(),
      () => mockCtx.ctx as unknown as AudioContext,
      () => mockCtx.workletNode as unknown as AudioWorkletNode,
    );
    await capture.start(
      vi.fn(),
      () => mockCtx.ctx as unknown as AudioContext,
      () => mockCtx.workletNode as unknown as AudioWorkletNode,
    );
    expect(navigator.mediaDevices.getUserMedia).toHaveBeenCalledTimes(1);
  });

  it('stop is safe to call when not active', async () => {
    const capture = new AudioCapture();
    await expect(capture.stop()).resolves.toBeUndefined();
  });
});
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine && npx vitest run src/lib/voice/audio-capture.test.ts`
Expected: FAIL — module `./audio-capture` not found

- [ ] **Step 4: Implement AudioCapture**

Create `src/lib/voice/audio-capture.ts`:

```typescript
/**
 * AudioWorklet-based PCM capture at 16kHz mono.
 *
 * Replaces the old audio-service.ts stub. Uses an AudioWorkletProcessor
 * (pcm-capture-processor.ts) to extract 20ms PCM frames (320 samples)
 * and delivers them via the onFrame callback.
 */

export type FrameCallback = (pcm: Float32Array) => void;

export class AudioCapture {
  private stream: MediaStream | null = null;
  private context: AudioContext | null = null;
  private source: MediaStreamAudioSourceNode | null = null;
  private worklet: AudioWorkletNode | null = null;
  private active = false;

  isActive(): boolean {
    return this.active;
  }

  /**
   * Start capturing audio.
   *
   * @param onFrame Called with each 20ms PCM frame (320 Float32 samples)
   * @param createContext Factory for AudioContext (injectable for testing)
   * @param createWorkletNode Factory for AudioWorkletNode (injectable for testing)
   */
  async start(
    onFrame: FrameCallback,
    createContext?: () => AudioContext,
    createWorkletNode?: (ctx: AudioContext) => AudioWorkletNode,
  ): Promise<void> {
    if (this.active) return;

    this.stream = await navigator.mediaDevices.getUserMedia({
      audio: { sampleRate: 16000, channelCount: 1, echoCancellation: false },
    });

    this.context = createContext
      ? createContext()
      : new AudioContext({ sampleRate: 16000 });

    if (!createWorkletNode) {
      await this.context.audioWorklet.addModule(
        new URL('./pcm-capture-processor.ts', import.meta.url).href
      );
    }

    this.source = this.context.createMediaStreamSource(this.stream);

    this.worklet = createWorkletNode
      ? createWorkletNode(this.context)
      : new AudioWorkletNode(this.context, 'pcm-capture-processor');

    this.worklet.port.onmessage = (e: MessageEvent) => {
      onFrame(e.data as Float32Array);
    };

    this.source.connect(this.worklet);
    this.active = true;
  }

  async stop(): Promise<void> {
    if (!this.active) return;

    this.worklet?.disconnect();
    this.source?.disconnect();
    this.stream?.getTracks().forEach(t => t.stop());
    await this.context?.close();

    this.worklet = null;
    this.source = null;
    this.stream = null;
    this.context = null;
    this.active = false;
  }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine && npx vitest run src/lib/voice/audio-capture.test.ts`
Expected: All 6 tests PASS

- [ ] **Step 6: Delete old audio-service stub and its tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine
rm src/lib/audio-service.ts src/lib/audio-service.test.ts
```

Verify no other files import from `audio-service`:

```bash
grep -r 'audio-service' src/ --include='*.ts' --include='*.svelte'
```

If any imports are found, update them. The only known consumer is `FlashcardView.svelte` which uses PttButton for timing (not audio capture), so it should not import audio-service.

- [ ] **Step 7: Run full test suite to verify no regressions**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine && npx vitest run`
Expected: All tests PASS (audio-service tests are gone, new audio-capture tests pass)

- [ ] **Step 8: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine
git add src/lib/voice/pcm-capture-processor.ts src/lib/voice/audio-capture.ts src/lib/voice/audio-capture.test.ts
git add -u  # stages the deletions
git commit -m "feat(voice): replace audio-service stub with AudioWorklet capture"
```

---

## Task 5: Voice Sender (Outbound Orchestrator)

Wires audio capture → opus encode → packet header → Tauri IPC.

**Files:**
- Create: `src/lib/voice/voice-sender.ts`
- Create: `src/lib/voice/voice-sender.test.ts`

- [ ] **Step 1: Write failing tests for voice sender**

Create `src/lib/voice/voice-sender.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { VoiceSender } from './voice-sender';
import { decodeHeader, HEADER_SIZE } from './voice-packet';

// Mock Tauri invoke
const mockInvoke = vi.fn().mockResolvedValue(undefined);

// Mock OpusCodec
const mockEncode = vi.fn((pcm: Float32Array) => new Uint8Array(40));
const mockCodec = {
  init: vi.fn().mockResolvedValue(undefined),
  encode: mockEncode,
  decode: vi.fn(),
  destroy: vi.fn(),
};

// Mock AudioCapture
let capturedOnFrame: ((pcm: Float32Array) => void) | null = null;
const mockCapture = {
  start: vi.fn(async (onFrame: (pcm: Float32Array) => void) => {
    capturedOnFrame = onFrame;
  }),
  stop: vi.fn().mockResolvedValue(undefined),
  isActive: vi.fn(() => true),
};

describe('VoiceSender', () => {
  const senderHash = new Uint8Array(16).fill(0xBB);
  let sender: VoiceSender;

  beforeEach(() => {
    vi.clearAllMocks();
    capturedOnFrame = null;
    sender = new VoiceSender({
      senderHash,
      channelId: 'test-channel',
      invoke: mockInvoke,
      codec: mockCodec as any,
      capture: mockCapture as any,
    });
  });

  it('start initializes codec and capture', async () => {
    await sender.start();
    expect(mockCodec.init).toHaveBeenCalledWith(16000, 1);
    expect(mockCapture.start).toHaveBeenCalled();
  });

  it('sends voice frames via Tauri invoke when capture delivers PCM', async () => {
    await sender.start();
    const pcm = new Float32Array(320).fill(0.5);
    capturedOnFrame!(pcm);

    expect(mockInvoke).toHaveBeenCalledWith('send_voice_frame', {
      channelId: 'test-channel',
      frameBytes: expect.any(Array),
    });

    // Verify the frame bytes contain a valid header + opus payload
    const callArgs = mockInvoke.mock.calls[0][1];
    const frameBytes = new Uint8Array(callArgs.frameBytes);
    expect(frameBytes.byteLength).toBe(HEADER_SIZE + 40);

    const header = decodeHeader(frameBytes);
    expect(header.pttActive).toBe(true);
    expect(header.sequence).toBe(0);
    expect(header.senderHash).toEqual(senderHash);
  });

  it('increments sequence number per frame', async () => {
    await sender.start();
    capturedOnFrame!(new Float32Array(320));
    capturedOnFrame!(new Float32Array(320));
    capturedOnFrame!(new Float32Array(320));

    expect(mockInvoke).toHaveBeenCalledTimes(3);

    const seq0 = decodeHeader(new Uint8Array(mockInvoke.mock.calls[0][1].frameBytes)).sequence;
    const seq1 = decodeHeader(new Uint8Array(mockInvoke.mock.calls[1][1].frameBytes)).sequence;
    const seq2 = decodeHeader(new Uint8Array(mockInvoke.mock.calls[2][1].frameBytes)).sequence;
    expect(seq0).toBe(0);
    expect(seq1).toBe(1);
    expect(seq2).toBe(2);
  });

  it('advances timestamp by 20ms per frame', async () => {
    await sender.start();
    capturedOnFrame!(new Float32Array(320));
    capturedOnFrame!(new Float32Array(320));

    const ts0 = decodeHeader(new Uint8Array(mockInvoke.mock.calls[0][1].frameBytes)).timestamp;
    const ts1 = decodeHeader(new Uint8Array(mockInvoke.mock.calls[1][1].frameBytes)).timestamp;
    expect(ts1 - ts0).toBe(20);
  });

  it('stop sends tail frames with PTT=false', async () => {
    await sender.start();
    capturedOnFrame!(new Float32Array(320));

    await sender.stop();

    // Should have sent the original frame + tail frames
    const tailCalls = mockInvoke.mock.calls.filter(call => {
      const bytes = new Uint8Array(call[1].frameBytes);
      return !decodeHeader(bytes).pttActive;
    });
    expect(tailCalls.length).toBeGreaterThanOrEqual(2);
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine && npx vitest run src/lib/voice/voice-sender.test.ts`
Expected: FAIL — module `./voice-sender` not found

- [ ] **Step 3: Implement VoiceSender**

Create `src/lib/voice/voice-sender.ts`:

```typescript
/**
 * Outbound voice orchestrator.
 *
 * Wires: AudioCapture → OpusCodec.encode → voice packet header → Tauri IPC.
 * Manages sequence counter, stream timestamp, and tail frame emission.
 */

import { type AudioCapture } from './audio-capture';
import { type OpusCodec } from './opus-codec';
import { encodeHeader, HEADER_SIZE } from './voice-packet';

const FRAME_MS = 20;
const TAIL_FRAME_COUNT = 3;

export interface VoiceSenderConfig {
  senderHash: Uint8Array;
  channelId: string;
  invoke: (cmd: string, args: Record<string, unknown>) => Promise<unknown>;
  codec: OpusCodec;
  capture: AudioCapture;
}

export class VoiceSender {
  private config: VoiceSenderConfig;
  private sequence = 0;
  private timestamp = 0;
  private active = false;

  constructor(config: VoiceSenderConfig) {
    this.config = config;
  }

  async start(): Promise<void> {
    if (this.active) return;

    this.sequence = 0;
    this.timestamp = 0;

    await this.config.codec.init(16000, 1);
    await this.config.capture.start((pcm: Float32Array) => {
      this.sendFrame(pcm, true);
    });

    this.active = true;
  }

  async stop(): Promise<void> {
    if (!this.active) return;

    await this.config.capture.stop();

    // Send tail frames with PTT=false so receivers get a clean end signal.
    const silence = new Float32Array(320); // 20ms of silence
    for (let i = 0; i < TAIL_FRAME_COUNT; i++) {
      this.sendFrame(silence, false);
    }

    this.config.codec.destroy();
    this.active = false;
  }

  private sendFrame(pcm: Float32Array, pttActive: boolean): void {
    const opus = this.config.codec.encode(pcm);

    const header = encodeHeader({
      pttActive,
      sequence: this.sequence & 0xFFFF,
      timestamp: this.timestamp >>> 0,
      senderHash: this.config.senderHash,
    });

    const frame = new Uint8Array(HEADER_SIZE + opus.byteLength);
    frame.set(header, 0);
    frame.set(opus, HEADER_SIZE);

    // Tauri invoke expects serializable args — convert Uint8Array to number[]
    this.config.invoke('send_voice_frame', {
      channelId: this.config.channelId,
      frameBytes: Array.from(frame),
    });

    this.sequence = (this.sequence + 1) & 0xFFFF;
    this.timestamp += FRAME_MS;
  }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine && npx vitest run src/lib/voice/voice-sender.test.ts`
Expected: All 5 tests PASS

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine
git add src/lib/voice/voice-sender.ts src/lib/voice/voice-sender.test.ts
git commit -m "feat(voice): add outbound voice sender orchestrator"
```

---

## Task 6: Voice Receiver (Inbound Orchestrator)

Listens for Tauri `voice-frame-received` events, decodes, manages per-sender jitter buffers, schedules playback.

**Files:**
- Create: `src/lib/voice/voice-receiver.ts`
- Create: `src/lib/voice/voice-receiver.test.ts`

- [ ] **Step 1: Write failing tests for voice receiver**

Create `src/lib/voice/voice-receiver.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { VoiceReceiver } from './voice-receiver';
import { encodeHeader, HEADER_SIZE } from './voice-packet';

// Mock OpusCodec
const mockDecode = vi.fn((opus: Uint8Array) => new Float32Array(320).fill(0.1));
const mockCodecFactory = vi.fn(() => ({
  init: vi.fn().mockResolvedValue(undefined),
  encode: vi.fn(),
  decode: mockDecode,
  destroy: vi.fn(),
}));

// Mock Tauri listen
type EventHandler = (event: { payload: unknown }) => void;
let registeredHandlers: Map<string, EventHandler> = new Map();
const mockListen = vi.fn(async (event: string, handler: EventHandler) => {
  registeredHandlers.set(event, handler);
  return () => { registeredHandlers.delete(event); };
});

function emitVoiceFrame(opts: {
  senderHash: Uint8Array;
  sequence: number;
  pttActive: boolean;
}) {
  const opusPayload = new Uint8Array(40);
  const header = encodeHeader({
    pttActive: opts.pttActive,
    sequence: opts.sequence,
    timestamp: opts.sequence * 20,
    senderHash: opts.senderHash,
  });
  const frame = new Uint8Array(HEADER_SIZE + opusPayload.byteLength);
  frame.set(header, 0);
  frame.set(opusPayload, HEADER_SIZE);

  const handler = registeredHandlers.get('voice-frame-received');
  handler?.({ payload: { frameBytes: Array.from(frame) } });
}

describe('VoiceReceiver', () => {
  const senderA = new Uint8Array(16).fill(0xAA);
  const senderB = new Uint8Array(16).fill(0xBB);
  let receiver: VoiceReceiver;

  beforeEach(() => {
    vi.clearAllMocks();
    vi.useFakeTimers();
    registeredHandlers = new Map();
    receiver = new VoiceReceiver({
      listen: mockListen,
      createCodec: mockCodecFactory as any,
    });
  });

  afterEach(() => {
    receiver.destroy();
    vi.useRealTimers();
  });

  it('registers a Tauri event listener on init', async () => {
    await receiver.init();
    expect(mockListen).toHaveBeenCalledWith(
      'voice-frame-received',
      expect.any(Function),
    );
  });

  it('creates a per-sender jitter buffer on first frame', async () => {
    await receiver.init();
    emitVoiceFrame({ senderHash: senderA, sequence: 0, pttActive: true });
    expect(receiver.getActiveSenders()).toEqual([expect.any(String)]);
  });

  it('tracks separate senders independently', async () => {
    await receiver.init();
    emitVoiceFrame({ senderHash: senderA, sequence: 0, pttActive: true });
    emitVoiceFrame({ senderHash: senderB, sequence: 0, pttActive: true });
    expect(receiver.getActiveSenders().length).toBe(2);
  });

  it('reports speaking state for active PTT', async () => {
    await receiver.init();
    emitVoiceFrame({ senderHash: senderA, sequence: 0, pttActive: true });
    const senderHex = Array.from(senderA).map(b => b.toString(16).padStart(2, '0')).join('');
    expect(receiver.isSpeaking(senderHex)).toBe(true);
  });

  it('clears speaking state on PTT=false tail frame', async () => {
    await receiver.init();
    emitVoiceFrame({ senderHash: senderA, sequence: 0, pttActive: true });
    emitVoiceFrame({ senderHash: senderA, sequence: 1, pttActive: false });
    const senderHex = Array.from(senderA).map(b => b.toString(16).padStart(2, '0')).join('');
    expect(receiver.isSpeaking(senderHex)).toBe(false);
  });

  it('cleans up sender after 2s idle timeout', async () => {
    await receiver.init();
    emitVoiceFrame({ senderHash: senderA, sequence: 0, pttActive: true });
    expect(receiver.getActiveSenders().length).toBe(1);

    vi.advanceTimersByTime(3000);
    expect(receiver.getActiveSenders().length).toBe(0);
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine && npx vitest run src/lib/voice/voice-receiver.test.ts`
Expected: FAIL — module `./voice-receiver` not found

- [ ] **Step 3: Implement VoiceReceiver**

Create `src/lib/voice/voice-receiver.ts`:

```typescript
/**
 * Inbound voice orchestrator.
 *
 * Listens for `voice-frame-received` Tauri events, decodes Opus,
 * routes frames to per-sender jitter buffers, and schedules playback.
 *
 * In a real browser, decoded PCM would be scheduled into AudioContext
 * via AudioBufferSourceNodes. In this implementation, the playback
 * scheduling uses setInterval to drive jitter buffer advance() calls,
 * and the actual Web Audio scheduling is handled by a pluggable
 * playFrame callback for testability.
 */

import { JitterBuffer } from './jitter-buffer';
import { decodeHeader, HEADER_SIZE } from './voice-packet';
import type { OpusCodec } from './opus-codec';

const FRAME_MS = 20;
const BUFFER_DEPTH = 4; // 80ms
const IDLE_TIMEOUT_MS = 2000;

interface SenderState {
  jitterBuffer: JitterBuffer;
  codec: OpusCodec;
  speaking: boolean;
  lastFrameTime: number;
  playbackTimer: ReturnType<typeof setInterval> | null;
  idleTimer: ReturnType<typeof setTimeout> | null;
}

export interface VoiceReceiverConfig {
  listen: (event: string, handler: (event: { payload: unknown }) => void) => Promise<() => void>;
  createCodec: () => OpusCodec;
  /** Optional callback for each decoded PCM frame ready for playback. */
  onPlayFrame?: (senderHex: string, pcm: Float32Array | null) => void;
}

export class VoiceReceiver {
  private config: VoiceReceiverConfig;
  private senders: Map<string, SenderState> = new Map();
  private unlisten: (() => void) | null = null;

  constructor(config: VoiceReceiverConfig) {
    this.config = config;
  }

  async init(): Promise<void> {
    this.unlisten = await this.config.listen(
      'voice-frame-received',
      (event) => this.handleFrame(event.payload as { frameBytes: number[] }),
    );
  }

  private handleFrame(payload: { frameBytes: number[] }): void {
    const bytes = new Uint8Array(payload.frameBytes);
    if (bytes.byteLength < HEADER_SIZE) return;

    const header = decodeHeader(bytes);
    const opusPayload = bytes.slice(HEADER_SIZE);
    const senderHex = Array.from(header.senderHash)
      .map(b => b.toString(16).padStart(2, '0'))
      .join('');

    let state = this.senders.get(senderHex);
    if (!state) {
      const codec = this.config.createCodec();
      // Fire-and-forget init — codec mock resolves synchronously in tests.
      // In production, the first few frames may fail to decode until init completes.
      codec.init(16000, 1);

      state = {
        jitterBuffer: new JitterBuffer(BUFFER_DEPTH, FRAME_MS),
        codec,
        speaking: false,
        lastFrameTime: Date.now(),
        playbackTimer: null,
        idleTimer: null,
      };
      this.senders.set(senderHex, state);

      // Start playback timer: advance jitter buffer every FRAME_MS
      state.playbackTimer = setInterval(() => {
        this.advancePlayback(senderHex);
      }, FRAME_MS);
    }

    // Update speaking state
    state.speaking = header.pttActive;
    state.lastFrameTime = Date.now();

    // Reset idle timer
    if (state.idleTimer) clearTimeout(state.idleTimer);
    state.idleTimer = setTimeout(() => {
      this.removeSender(senderHex);
    }, IDLE_TIMEOUT_MS);

    // Decode and insert into jitter buffer
    const pcm = state.codec.decode(opusPayload);
    state.jitterBuffer.insert(header.sequence, pcm);
  }

  private advancePlayback(senderHex: string): void {
    const state = this.senders.get(senderHex);
    if (!state) return;

    const pcm = state.jitterBuffer.advance();
    this.config.onPlayFrame?.(senderHex, pcm);
  }

  private removeSender(senderHex: string): void {
    const state = this.senders.get(senderHex);
    if (!state) return;

    if (state.playbackTimer) clearInterval(state.playbackTimer);
    if (state.idleTimer) clearTimeout(state.idleTimer);
    state.codec.destroy();
    this.senders.delete(senderHex);
  }

  /** Get hex addresses of all active senders. */
  getActiveSenders(): string[] {
    return Array.from(this.senders.keys());
  }

  /** Whether a sender is currently transmitting (PTT active). */
  isSpeaking(senderHex: string): boolean {
    return this.senders.get(senderHex)?.speaking ?? false;
  }

  /** Clean up all resources. */
  destroy(): void {
    if (this.unlisten) {
      this.unlisten();
      this.unlisten = null;
    }
    for (const [key] of this.senders) {
      this.removeSender(key);
    }
  }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine && npx vitest run src/lib/voice/voice-receiver.test.ts`
Expected: All 6 tests PASS

- [ ] **Step 5: Run full frontend test suite**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine && npx vitest run`
Expected: All tests PASS

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine
git add src/lib/voice/voice-receiver.ts src/lib/voice/voice-receiver.test.ts
git commit -m "feat(voice): add inbound voice receiver with per-sender jitter buffers"
```

---

## Task 7: Rust Voice Module

Add the Rust-side voice relay: `voice.rs` types, new channels in `NodeState`, new Tauri commands, event loop integration.

**Files:**
- Create: `src-tauri/src/voice.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/src/event_loop.rs`

- [ ] **Step 1: Create voice.rs with types**

Create `src-tauri/src/voice.rs`:

```rust
//! Voice channel relay types.
//!
//! The Rust side is a dumb relay — all audio encoding/decoding happens
//! in the browser. This module defines the IPC types and channel request
//! enum for voice traffic between Tauri commands and the event loop.

use serde::Deserialize;

/// An outbound voice frame from the frontend, ready to publish to Zenoh.
#[derive(Debug)]
pub struct VoiceOutbound {
    /// Target voice channel (maps to Zenoh topic namespace).
    pub channel_id: String,
    /// Raw frame bytes: 23-byte header + Opus payload, assembled by the frontend.
    pub frame: Vec<u8>,
}

/// Voice channel lifecycle requests from Tauri commands to the event loop.
#[derive(Debug)]
pub enum VoiceChannelRequest {
    /// Subscribe to `harmony/voice/{channel_id}/*` and start emitting
    /// `voice-frame-received` events to the frontend.
    Join { channel_id: String },
    /// Unsubscribe from the voice channel.
    Leave { channel_id: String },
}

/// Payload for the `send_voice_frame` Tauri command.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendVoiceFramePayload {
    pub channel_id: String,
    pub frame_bytes: Vec<u8>,
}
```

- [ ] **Step 2: Modify lib.rs — add voice channels to NodeState and new commands**

In `src-tauri/src/lib.rs`, add:

1. Add `mod voice;` after `mod identity;` (line 13):

```rust
mod voice;
```

2. Add voice fields to `NodeState` (after `follow_tx` field, around line 30):

```rust
    /// Channel for routing voice frames through the event loop's Zenoh session.
    voice_tx: Option<tokio::sync::mpsc::Sender<voice::VoiceOutbound>>,
    /// Channel for voice channel join/leave lifecycle requests.
    voice_channel_tx: Option<tokio::sync::mpsc::Sender<voice::VoiceChannelRequest>>,
```

3. Add voice channel creation in `start_node` (after the `follow_tx` channel creation, around line 235):

```rust
    let (voice_tx, voice_rx) = tokio::sync::mpsc::channel(100);
    let (voice_channel_tx, voice_channel_rx) = tokio::sync::mpsc::channel(16);
```

4. Pass `voice_rx` and `voice_channel_rx` to `event_loop::run()` (add after `follow_rx` parameter):

```rust
                        voice_rx,
                        voice_channel_rx,
```

5. Store voice handles in guard (after `guard.follow_tx = ...`):

```rust
        guard.voice_tx = Some(voice_tx);
        guard.voice_channel_tx = Some(voice_channel_tx);
```

6. Drop voice handles in `stop_inner` (add to the destructuring and drops):

In the destructuring tuple, add `guard.voice_tx.take()` and `guard.voice_channel_tx.take()`.
Drop them alongside the other channel senders.

7. Add the three new Tauri commands:

```rust
/// Send a voice frame to the mesh network via the event loop's Zenoh session.
///
/// The frontend assembles the full frame (23-byte header + Opus payload).
/// This command just relays it to the event loop for Zenoh publication.
#[tauri::command]
async fn send_voice_frame(
    payload: voice::SendVoiceFramePayload,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let voice_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .voice_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };

    voice_tx
        .send(voice::VoiceOutbound {
            channel_id: payload.channel_id,
            frame: payload.frame_bytes,
        })
        .await
        .map_err(|_| "event loop not running".to_string())
}

/// Join a voice channel — subscribe to `harmony/voice/{channel_id}/*`.
#[tauri::command]
async fn join_voice_channel(
    channel_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .voice_channel_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };

    tx.send(voice::VoiceChannelRequest::Join { channel_id })
        .await
        .map_err(|_| "event loop not running".to_string())
}

/// Leave a voice channel — unsubscribe from `harmony/voice/{channel_id}/*`.
#[tauri::command]
async fn leave_voice_channel(
    channel_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .voice_channel_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };

    tx.send(voice::VoiceChannelRequest::Leave { channel_id })
        .await
        .map_err(|_| "event loop not running".to_string())
}
```

8. Register the new commands in `invoke_handler` (add after `ingest_content`):

```rust
            send_voice_frame,
            join_voice_channel,
            leave_voice_channel,
```

- [ ] **Step 3: Modify event_loop.rs — add voice relay and dynamic subscriptions**

In `src-tauri/src/event_loop.rs`:

1. Add new parameters to the `run` function signature (after `follow_rx`):

```rust
    mut voice_rx: mpsc::Receiver<crate::voice::VoiceOutbound>,
    mut voice_channel_rx: mpsc::Receiver<crate::voice::VoiceChannelRequest>,
```

2. Add voice subscription tracking after `direct_peer_zids` (around line 268):

```rust
    // Dynamic voice channel subscriptions — keyed by channel_id.
    // Each entry holds a JoinHandle that will be aborted on Leave.
    let mut voice_subs: std::collections::HashMap<String, tokio::task::JoinHandle<()>> = std::collections::HashMap::new();
```

3. Add voice frame and channel arms to the `tokio::select!` block (before the shutdown arm):

```rust
            // ── Voice frame relay (frontend → Zenoh) ────────────────────
            Some(voice) = voice_rx.recv() => {
                let node_addr = {
                    // Read node_addr from app state for the topic.
                    // The event loop doesn't own NodeState, so we use
                    // the sender address embedded in the voice frame header.
                    // Bytes 7-22 of the frame are the sender hash.
                    if voice.frame.len() >= 23 {
                        hex::encode(&voice.frame[7..23])
                    } else {
                        continue;
                    }
                };
                let key_expr = format!(
                    "harmony/voice/{}/{}",
                    voice.channel_id, node_addr
                );
                let session = session.clone();
                let payload = voice.frame;
                tokio::spawn(async move {
                    if let Err(e) = session.put(&key_expr, payload).await {
                        tracing::warn!(%key_expr, err = %e, "voice publish failed");
                    }
                });
            }

            // ── Voice channel join/leave ─────────────────────────────────
            Some(req) = voice_channel_rx.recv() => {
                match req {
                    crate::voice::VoiceChannelRequest::Join { channel_id } => {
                        let key_expr = format!("harmony/voice/{}/*", channel_id);
                        let app = app.clone();
                        let closing = closing.clone();
                        match session.declare_subscriber(&key_expr).await {
                            Ok(sub) => {
                                let handle = tokio::spawn(async move {
                                    while let Ok(sample) = sub.recv_async().await {
                                        let payload = sample.payload().to_bytes().to_vec();
                                        // Emit raw frame bytes to frontend — it handles
                                        // header parsing and decode.
                                        let _ = app.emit("voice-frame-received", serde_json::json!({
                                            "frameBytes": payload,
                                        }));
                                    }
                                    if !closing.load(std::sync::atomic::Ordering::SeqCst) {
                                        tracing::warn!("voice subscriber closed unexpectedly");
                                    }
                                });
                                voice_subs.insert(channel_id, handle);
                            }
                            Err(e) => {
                                tracing::error!(%key_expr, err = %e, "voice subscribe failed");
                            }
                        }
                    }
                    crate::voice::VoiceChannelRequest::Leave { channel_id } => {
                        if let Some(handle) = voice_subs.remove(&channel_id) {
                            handle.abort();
                        }
                    }
                }
            }
```

4. Clean up voice subscriptions in the shutdown section (after `closing.store(true, ...)`):

```rust
    // Abort voice subscriber tasks.
    for (_, handle) in voice_subs.drain() {
        handle.abort();
    }
```

- [ ] **Step 4: Verify Rust compiles**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine/src-tauri
cargo check 2>&1
```

Expected: compiles with no errors. Warnings about unused variables are OK.

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine
git add src-tauri/src/voice.rs src-tauri/src/lib.rs src-tauri/src/event_loop.rs
git commit -m "feat(voice): add Rust voice relay — IPC channels, Zenoh pub/sub, dynamic subscriptions"
```

---

## Task 8: Full Test Suite Verification

Run all tests (frontend + Rust) to verify nothing is broken.

**Files:** None (verification only)

- [ ] **Step 1: Run frontend tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine
npx vitest run
```

Expected: All tests PASS (including all new voice/ tests)

- [ ] **Step 2: Run Rust tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine/src-tauri
cargo test 2>&1
```

Expected: All existing Rust tests PASS

- [ ] **Step 3: Verify Rust compiles clean**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine/src-tauri
cargo clippy 2>&1
```

Expected: No errors (warnings about unused fields from voice.rs may appear — OK for now, they'll be used by runtime)

- [ ] **Step 4: Commit any fixes from verification**

If any tests failed and were fixed, commit the fixes:

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-engine
git add -A
git commit -m "fix: address test failures from voice integration"
```

If all tests passed with no fixes, skip this step.
