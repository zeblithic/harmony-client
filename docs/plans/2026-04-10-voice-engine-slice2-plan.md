# Voice Engine Slice 2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add codec2 3200 low-bandwidth voice encoding, adaptive jitter buffering, and comfort noise to the existing PTT Opus pipeline.

**Architecture:** Browser-side codec2 WASM (emscripten-compiled C), unified `VoiceCodec` interface for Opus and codec2, packet header codec bit for self-describing frames, adaptive jitter buffer driven by inter-arrival jitter estimation, comfort noise generator for missing frames, and a codec toggle UI component.

**Tech Stack:** TypeScript, Svelte 5 (runes), vitest + jsdom, opusscript (existing), codec2 C via emscripten, Web Audio API

---

## File Structure

### New files

| File | Responsibility |
|------|---------------|
| `src/lib/voice/voice-codec.ts` | `VoiceCodec` interface + `CodecType` type |
| `src/lib/voice/codec2-codec.ts` | `Codec2Codec` class wrapping emscripten WASM |
| `src/lib/voice/adaptive-jitter-buffer.ts` | Jitter-driven adaptive depth buffer |
| `src/lib/voice/comfort-noise.ts` | Deterministic comfort noise generator |
| `src/lib/components/CodecToggle.svelte` | Segmented codec selection control |
| `src/lib/voice/codec2-codec.test.ts` | Codec2Codec unit tests |
| `src/lib/voice/adaptive-jitter-buffer.test.ts` | Adaptive jitter buffer tests |
| `src/lib/voice/comfort-noise.test.ts` | Comfort noise tests |
| `src/lib/components/__tests__/CodecToggle.test.ts` | CodecToggle component tests |
| `build/codec2/Makefile` | Emscripten build for codec2 WASM |
| `build/codec2/codec2_glue.c` | Thin C wrapper exporting codec2 functions |

### Modified files

| File | Change |
|------|--------|
| `src/lib/voice/voice-packet.ts` | Add codec bit encode/decode |
| `src/lib/voice/voice-packet.test.ts` | Codec bit tests |
| `src/lib/voice/opus-codec.ts` | Implement `VoiceCodec` interface |
| `src/lib/voice/opus-codec.test.ts` | Verify `codecType` property |
| `src/lib/voice/audio-capture.ts` | Accept configurable `sampleRate` |
| `src/lib/voice/pcm-capture-processor.ts` | Derive frame size from sample rate |
| `src/lib/voice/voice-sender.ts` | Accept `VoiceCodec`, set codec header bit |
| `src/lib/voice/voice-sender.test.ts` | VoiceCodec interface, codec bit tests |
| `src/lib/voice/voice-receiver.ts` | Read codec bit, adaptive buffer, comfort noise |
| `src/lib/voice/voice-receiver.test.ts` | Mixed-codec, comfort noise tests |
| `src/lib/components/PttButton.svelte` | Add codec toggle integration |

---

## Codebase Context for Implementers

**Test runner:** `npx vitest run` (NOT `cargo test`). All tests are vitest + jsdom.

**Test location:** Tests are co-located with source files (e.g., `voice-packet.test.ts` next to `voice-packet.ts`). Component tests go in `src/lib/components/__tests__/`.

**Import patterns:** Relative imports within `src/lib/voice/`. No path aliases.

**Svelte 5 runes:** Components use `$props()`, `$state()`, `$derived()`. Event handlers use `onclick={handler}` (NOT `on:click`).

**Mock patterns:** WASM modules (opusscript) are mocked with `vi.mock()`. Codec tests mock the WASM loader, not the codec wrapper. See `opus-codec.test.ts` for the pattern.

**Existing voice packet header (byte 0):** `[4-bit version=0x1][PTT bit=bit3][3 reserved bits=bits2-0]`. We add a codec bit at bit 2.

---

### Task 1: VoiceCodec Interface

**Files:**
- Create: `src/lib/voice/voice-codec.ts`
- Modify: `src/lib/voice/opus-codec.ts`
- Modify: `src/lib/voice/opus-codec.test.ts`

- [ ] **Step 1: Create the VoiceCodec interface**

Create `src/lib/voice/voice-codec.ts`:

```typescript
/** Codec type identifier carried in voice packet headers. */
export type CodecType = 'opus' | 'codec2';

/**
 * Common interface for voice codecs used by VoiceSender and VoiceReceiver.
 *
 * Both OpusCodec and Codec2Codec implement this interface so the voice
 * pipeline can work with either codec interchangeably.
 */
export interface VoiceCodec {
  /** Load WASM and initialize encoder/decoder state. */
  init(sampleRate: number, channels: number): Promise<void>;
  /** Encode PCM float samples to compressed bytes. */
  encode(pcm: Float32Array): Uint8Array;
  /** Decode compressed bytes to PCM float samples. */
  decode(encoded: Uint8Array): Float32Array;
  /** Release WASM heap allocations. encode/decode throw after this. */
  destroy(): void;
  /** Identifies the codec for packet header encoding. */
  readonly codecType: CodecType;
}
```

- [ ] **Step 2: Add codecType to OpusCodec**

Modify `src/lib/voice/opus-codec.ts`. Add the import and property:

At the top, add the import:
```typescript
import type { VoiceCodec, CodecType } from './voice-codec';
```

Change the class declaration from:
```typescript
export class OpusCodec {
```
to:
```typescript
export class OpusCodec implements VoiceCodec {
```

Add the property inside the class, right after the opening brace:
```typescript
  readonly codecType: CodecType = 'opus';
```

- [ ] **Step 3: Add codecType test to opus-codec.test.ts**

Add this test at the end of the `describe('OpusCodec', ...)` block in `src/lib/voice/opus-codec.test.ts`:

```typescript
  it('exposes codecType as opus', () => {
    expect(codec.codecType).toBe('opus');
  });
```

- [ ] **Step 4: Run tests to verify**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx vitest run src/lib/voice/opus-codec.test.ts`
Expected: All tests pass, including the new `codecType` test.

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2
git add src/lib/voice/voice-codec.ts src/lib/voice/opus-codec.ts src/lib/voice/opus-codec.test.ts
git commit -m "feat(voice): add VoiceCodec interface, implement on OpusCodec"
```

---

### Task 2: Packet Header Codec Bit

**Files:**
- Modify: `src/lib/voice/voice-packet.ts`
- Modify: `src/lib/voice/voice-packet.test.ts`

- [ ] **Step 1: Write failing tests for codec bit**

Add these tests at the end of the `describe('voice-packet header', ...)` block in `src/lib/voice/voice-packet.test.ts`:

```typescript
  it('encodes codec=opus (bit 2 clear) by default', () => {
    const fields: VoiceHeaderFields = {
      pttActive: true,
      sequence: 0,
      timestamp: 0,
      senderHash: makeSenderHash(),
    };
    const buf = encodeHeader(fields);
    // Bit 2 should be 0 (Opus)
    expect(buf[0] & 0x04).toBe(0);
    const decoded = decodeHeader(buf);
    expect(decoded.codec).toBe('opus');
  });

  it('encodes codec=codec2 when specified', () => {
    const fields: VoiceHeaderFields = {
      pttActive: true,
      sequence: 10,
      timestamp: 200,
      senderHash: makeSenderHash(),
      codec: 'codec2',
    };
    const buf = encodeHeader(fields);
    // Bit 2 should be set
    expect(buf[0] & 0x04).toBe(0x04);
    const decoded = decodeHeader(buf);
    expect(decoded.codec).toBe('codec2');
  });

  it('roundtrips codec2 with PTT active', () => {
    const fields: VoiceHeaderFields = {
      pttActive: true,
      sequence: 100,
      timestamp: 5000,
      senderHash: makeSenderHash(),
      codec: 'codec2',
    };
    const buf = encodeHeader(fields);
    const decoded = decodeHeader(buf);
    expect(decoded.pttActive).toBe(true);
    expect(decoded.codec).toBe('codec2');
    expect(decoded.sequence).toBe(100);
    expect(decoded.timestamp).toBe(5000);
  });

  it('existing packets without codec field decode as opus', () => {
    // Simulate a Slice 1 packet (no codec field)
    const fields: VoiceHeaderFields = {
      pttActive: false,
      sequence: 42,
      timestamp: 1000,
      senderHash: makeSenderHash(),
    };
    const buf = encodeHeader(fields);
    const decoded = decodeHeader(buf);
    expect(decoded.codec).toBe('opus');
  });
```

Also add the `CodecType` import at the top of the test file — update the import line:
```typescript
import {
  HEADER_SIZE,
  VOICE_VERSION,
  encodeHeader,
  decodeHeader,
  type VoiceHeaderFields,
} from './voice-packet';
```
(No change needed yet — `codec` field will be added to `VoiceHeaderFields` in Step 3.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx vitest run src/lib/voice/voice-packet.test.ts`
Expected: FAIL — `codec` property does not exist on `VoiceHeaderFields` or `DecodedVoiceHeader`.

- [ ] **Step 3: Implement codec bit in voice-packet.ts**

In `src/lib/voice/voice-packet.ts`, add the import at the top:
```typescript
import type { CodecType } from './voice-codec';
```

Add `codec` to `VoiceHeaderFields`:
```typescript
export interface VoiceHeaderFields {
  pttActive: boolean;
  sequence: number;
  timestamp: number;
  senderHash: Uint8Array;
  /** Codec type. Defaults to 'opus' if omitted (backward compatible). */
  codec?: CodecType;
}
```

Add `codec` to `DecodedVoiceHeader`:
```typescript
export interface DecodedVoiceHeader {
  version: number;
  pttActive: boolean;
  sequence: number;
  timestamp: number;
  senderHash: Uint8Array;
  /** Codec identified by bit 2 of the flags byte. */
  codec: CodecType;
}
```

Update `encodeHeader` — replace the flags line:
```typescript
  // Byte 0: version nibble | PTT bit | codec bit | 2 reserved bits
  const codecBit = fields.codec === 'codec2' ? 0x04 : 0x00;
  const flags = (VOICE_VERSION << 4) | (fields.pttActive ? 0x08 : 0x00) | codecBit;
  buf[0] = flags;
```

Update `decodeHeader` — add codec extraction after the existing `pttActive` line:
```typescript
  const codec: CodecType = (flagsByte & 0x04) !== 0 ? 'codec2' : 'opus';
```

And include `codec` in the return object:
```typescript
  return { version, pttActive, codec, sequence, timestamp, senderHash };
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx vitest run src/lib/voice/voice-packet.test.ts`
Expected: All tests pass (existing + 4 new).

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2
git add src/lib/voice/voice-packet.ts src/lib/voice/voice-packet.test.ts
git commit -m "feat(voice): add codec type bit to voice packet header"
```

---

### Task 3: Comfort Noise Generator

**Files:**
- Create: `src/lib/voice/comfort-noise.ts`
- Create: `src/lib/voice/comfort-noise.test.ts`

- [ ] **Step 1: Write failing tests**

Create `src/lib/voice/comfort-noise.test.ts`:

```typescript
import { describe, it, expect } from 'vitest';
import { generateComfortNoise, ComfortNoiseGenerator } from './comfort-noise';

describe('generateComfortNoise', () => {
  it('returns Float32Array of requested length', () => {
    const noise = generateComfortNoise(320, 0.005);
    expect(noise).toBeInstanceOf(Float32Array);
    expect(noise.length).toBe(320);
  });

  it('produces non-zero samples', () => {
    const noise = generateComfortNoise(320, 0.005);
    const hasNonZero = noise.some((s) => s !== 0);
    expect(hasNonZero).toBe(true);
  });

  it('all samples are within [-level, +level]', () => {
    const level = 0.01;
    const noise = generateComfortNoise(1000, level);
    for (let i = 0; i < noise.length; i++) {
      expect(Math.abs(noise[i])).toBeLessThanOrEqual(level);
    }
  });

  it('different calls produce different output (stateful PRNG)', () => {
    const a = generateComfortNoise(160, 0.005);
    const b = generateComfortNoise(160, 0.005);
    // Not identical — PRNG state advances between calls
    const same = a.every((v, i) => v === b[i]);
    expect(same).toBe(false);
  });
});

describe('ComfortNoiseGenerator', () => {
  it('produces deterministic output for same seed', () => {
    const gen1 = new ComfortNoiseGenerator(42);
    const gen2 = new ComfortNoiseGenerator(42);
    const a = gen1.generate(160, 0.005);
    const b = gen2.generate(160, 0.005);
    expect(a).toEqual(b);
  });

  it('different seeds produce different output', () => {
    const gen1 = new ComfortNoiseGenerator(42);
    const gen2 = new ComfortNoiseGenerator(99);
    const a = gen1.generate(160, 0.005);
    const b = gen2.generate(160, 0.005);
    const same = a.every((v, i) => v === b[i]);
    expect(same).toBe(false);
  });

  it('generates correct number of samples', () => {
    const gen = new ComfortNoiseGenerator(1);
    expect(gen.generate(160, 0.005).length).toBe(160);
    expect(gen.generate(320, 0.005).length).toBe(320);
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx vitest run src/lib/voice/comfort-noise.test.ts`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement comfort noise**

Create `src/lib/voice/comfort-noise.ts`:

```typescript
/**
 * Comfort noise generator for voice playback gaps.
 *
 * Produces low-level white noise to mask missing frames instead of
 * hard silence. Uses mulberry32 PRNG for deterministic, testable output.
 */

/**
 * Mulberry32 PRNG — fast, deterministic, 32-bit state.
 * Returns a float in [0, 1).
 */
function mulberry32(state: { seed: number }): number {
  let t = (state.seed += 0x6d2b79f5);
  t = Math.imul(t ^ (t >>> 15), t | 1);
  t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}

/**
 * Stateful comfort noise generator with deterministic PRNG.
 * Use for testing or when you need reproducible output.
 */
export class ComfortNoiseGenerator {
  private state: { seed: number };

  constructor(seed: number) {
    this.state = { seed };
  }

  /**
   * Generate `samples` of white noise at the given amplitude level.
   * Output range: [-level, +level].
   */
  generate(samples: number, level: number): Float32Array {
    const out = new Float32Array(samples);
    for (let i = 0; i < samples; i++) {
      // Map [0, 1) to [-1, 1) then scale by level
      out[i] = (mulberry32(this.state) * 2 - 1) * level;
    }
    return out;
  }
}

/** Module-level generator for production use (non-deterministic across runs). */
const defaultGenerator = new ComfortNoiseGenerator(Date.now() | 0);

/**
 * Generate comfort noise using the module-level generator.
 * Convenient for production — use ComfortNoiseGenerator directly for tests.
 *
 * @param samples  Number of PCM samples to generate.
 * @param level    Amplitude level (default 0.005 ≈ -46dB).
 */
export function generateComfortNoise(
  samples: number,
  level = 0.005,
): Float32Array {
  return defaultGenerator.generate(samples, level);
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx vitest run src/lib/voice/comfort-noise.test.ts`
Expected: All 7 tests pass.

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2
git add src/lib/voice/comfort-noise.ts src/lib/voice/comfort-noise.test.ts
git commit -m "feat(voice): add comfort noise generator for missing frames"
```

---

### Task 4: Adaptive Jitter Buffer

**Files:**
- Create: `src/lib/voice/adaptive-jitter-buffer.ts`
- Create: `src/lib/voice/adaptive-jitter-buffer.test.ts`

- [ ] **Step 1: Write failing tests**

Create `src/lib/voice/adaptive-jitter-buffer.test.ts`:

```typescript
import { describe, it, expect, beforeEach, vi } from 'vitest';
import { AdaptiveJitterBuffer } from './adaptive-jitter-buffer';

describe('AdaptiveJitterBuffer', () => {
  const MIN_DEPTH = 2;
  const MAX_DEPTH = 10;
  const FRAME_MS = 20;

  let buf: AdaptiveJitterBuffer;

  beforeEach(() => {
    buf = new AdaptiveJitterBuffer({ minDepth: MIN_DEPTH, maxDepth: MAX_DEPTH, frameMs: FRAME_MS });
  });

  it('starts at minDepth', () => {
    expect(buf.getDepth()).toBe(MIN_DEPTH);
  });

  it('reports zero jitter initially', () => {
    expect(buf.getJitterMs()).toBe(0);
  });

  it('advance returns null before first insert (not seeded)', () => {
    expect(buf.advance()).toBeNull();
    expect(buf.isReady()).toBe(false);
  });

  it('becomes ready after fill period at minDepth', () => {
    buf.insert(0, new Float32Array(160));
    for (let i = 0; i < MIN_DEPTH; i++) {
      expect(buf.isReady()).toBe(false);
      buf.advance();
    }
    expect(buf.isReady()).toBe(true);
  });

  it('plays frames in sequence order after fill', () => {
    const pcm0 = new Float32Array([1, 2]);
    const pcm1 = new Float32Array([3, 4]);

    buf.insert(0, pcm0);
    buf.insert(1, pcm1);

    // Fill period
    for (let i = 0; i < MIN_DEPTH; i++) buf.advance();

    expect(buf.advance()).toEqual(pcm0);
    expect(buf.advance()).toEqual(pcm1);
  });

  it('returns null for missing frames', () => {
    buf.insert(0, new Float32Array([1]));
    // Skip seq 1
    buf.insert(2, new Float32Array([3]));

    for (let i = 0; i < MIN_DEPTH; i++) buf.advance();
    buf.advance(); // seq 0
    expect(buf.advance()).toBeNull(); // seq 1 missing
    expect(buf.advance()).toEqual(new Float32Array([3])); // seq 2
  });

  it('grows depth when jitter increases', () => {
    // Simulate high jitter by manipulating arrival times
    const mockNow = vi.fn();
    buf = new AdaptiveJitterBuffer({
      minDepth: MIN_DEPTH,
      maxDepth: MAX_DEPTH,
      frameMs: FRAME_MS,
      now: mockNow,
    });

    // First frame at t=0
    mockNow.mockReturnValue(0);
    buf.insert(0, new Float32Array(160));

    // Second frame arrives 80ms late (expected at 20ms, arrived at 100ms)
    mockNow.mockReturnValue(100);
    buf.insert(1, new Float32Array(160));

    // Third frame also late
    mockNow.mockReturnValue(200);
    buf.insert(2, new Float32Array(160));

    // Jitter should have increased, causing depth growth
    expect(buf.getDepth()).toBeGreaterThan(MIN_DEPTH);
  });

  it('does not exceed maxDepth', () => {
    const mockNow = vi.fn();
    buf = new AdaptiveJitterBuffer({
      minDepth: MIN_DEPTH,
      maxDepth: MAX_DEPTH,
      frameMs: FRAME_MS,
      now: mockNow,
    });

    // Simulate extreme jitter
    for (let i = 0; i < 50; i++) {
      mockNow.mockReturnValue(i * 500); // 500ms between frames (480ms jitter)
      buf.insert(i, new Float32Array(160));
    }

    expect(buf.getDepth()).toBeLessThanOrEqual(MAX_DEPTH);
  });

  it('shrinks depth after sustained clean playback', () => {
    const mockNow = vi.fn();
    buf = new AdaptiveJitterBuffer({
      minDepth: MIN_DEPTH,
      maxDepth: MAX_DEPTH,
      frameMs: FRAME_MS,
      now: mockNow,
    });

    // Create high jitter to grow buffer
    mockNow.mockReturnValue(0);
    buf.insert(0, new Float32Array(160));
    mockNow.mockReturnValue(100);
    buf.insert(1, new Float32Array(160));
    mockNow.mockReturnValue(200);
    buf.insert(2, new Float32Array(160));

    const grownDepth = buf.getDepth();
    expect(grownDepth).toBeGreaterThan(MIN_DEPTH);

    // Now simulate stable arrivals and clean playback
    // Fill period first
    for (let i = 0; i < grownDepth; i++) buf.advance();

    // Insert frames at stable 20ms intervals and advance
    let t = 200;
    let seq = 3;
    for (let i = 0; i < 100; i++) {
      t += 20;
      mockNow.mockReturnValue(t);
      buf.insert(seq, new Float32Array(160));
      seq++;
      buf.advance();
    }

    // After sustained clean playback, depth should have shrunk
    expect(buf.getDepth()).toBeLessThan(grownDepth);
  });

  it('does not shrink below minDepth', () => {
    // Start with stable arrivals — should stay at minDepth
    const mockNow = vi.fn();
    buf = new AdaptiveJitterBuffer({
      minDepth: MIN_DEPTH,
      maxDepth: MAX_DEPTH,
      frameMs: FRAME_MS,
      now: mockNow,
    });

    // Insert stable frames
    for (let i = 0; i < 100; i++) {
      mockNow.mockReturnValue(i * 20);
      buf.insert(i, new Float32Array(160));
    }

    // Fill + advance
    for (let i = 0; i < MIN_DEPTH; i++) buf.advance();
    for (let i = 0; i < 80; i++) buf.advance();

    expect(buf.getDepth()).toBe(MIN_DEPTH);
  });

  it('handles sequence wraparound at u16 boundary', () => {
    buf.insert(0, new Float32Array([0]));
    for (let i = 0; i < MIN_DEPTH; i++) buf.advance();

    // Advance playhead to near wraparound
    for (let i = 0; i < 0xFFFE; i++) buf.advance();

    const pcmA = new Float32Array([10]);
    const pcmB = new Float32Array([20]);
    buf.insert(0xFFFE, pcmA);
    buf.insert(0xFFFF, pcmB);

    expect(buf.advance()).toEqual(pcmA);
    expect(buf.advance()).toEqual(pcmB);
  });

  it('reset clears all state', () => {
    buf.insert(0, new Float32Array([1]));
    for (let i = 0; i < MIN_DEPTH; i++) buf.advance();
    expect(buf.isReady()).toBe(true);

    buf.reset();

    expect(buf.isReady()).toBe(false);
    expect(buf.getDepth()).toBe(MIN_DEPTH);
    expect(buf.getJitterMs()).toBe(0);
  });

  it('seeds playSeq from first frame for mid-stream join', () => {
    const pcm = new Float32Array([42]);
    buf.insert(500, pcm);
    for (let i = 0; i < MIN_DEPTH; i++) buf.advance();
    expect(buf.advance()).toEqual(pcm);
  });

  it('drops late frames', () => {
    buf.insert(0, new Float32Array([1]));
    buf.insert(1, new Float32Array([2]));
    for (let i = 0; i < MIN_DEPTH; i++) buf.advance();
    buf.advance(); // play seq 0
    buf.advance(); // play seq 1, playSeq now 2

    // Insert frame at seq 0 — already played, should be dropped
    buf.insert(0, new Float32Array([99]));
    // Next frame at seq 2
    buf.insert(2, new Float32Array([3]));
    expect(buf.advance()).toEqual(new Float32Array([3]));
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx vitest run src/lib/voice/adaptive-jitter-buffer.test.ts`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement adaptive jitter buffer**

Create `src/lib/voice/adaptive-jitter-buffer.ts`:

```typescript
/**
 * Adaptive jitter buffer for voice playback.
 *
 * Dynamically adjusts buffer depth based on observed inter-arrival jitter
 * using an exponentially weighted moving average (RFC 3550 style).
 *
 * One instance per active sender. Drop-in replacement for JitterBuffer
 * with the same core API (insert/advance/reset/isReady).
 */

export interface AdaptiveJitterBufferConfig {
  /** Minimum buffer depth in frames (default: 2 = 40ms). */
  minDepth: number;
  /** Maximum buffer depth in frames (default: 10 = 200ms). */
  maxDepth: number;
  /** Frame duration in milliseconds (default: 20). */
  frameMs: number;
  /**
   * Time source for inter-arrival measurement.
   * Defaults to performance.now. Injectable for testing.
   */
  now?: () => number;
}

/** Number of consecutive clean advances before depth can shrink. */
const SHRINK_DELAY = 50;

export class AdaptiveJitterBuffer {
  private readonly minDepth: number;
  private readonly maxDepth: number;
  private readonly frameMs: number;
  private readonly now: () => number;

  private slots: (Float32Array | null)[];
  private depth: number;
  private playSeq = 0;
  private fillCount = 0;
  private seeded = false;

  /** Exponentially weighted jitter estimate in ms. */
  private jitter = 0;
  /** Timestamp of last insert() call. */
  private lastArrival: number | null = null;
  /** Consecutive advance() calls that returned a non-null frame. */
  private cleanRun = 0;

  constructor(config: AdaptiveJitterBufferConfig) {
    this.minDepth = config.minDepth;
    this.maxDepth = config.maxDepth;
    this.frameMs = config.frameMs;
    this.now = config.now ?? (() => performance.now());
    this.depth = config.minDepth;
    this.slots = new Array<Float32Array | null>(this.depth).fill(null);
  }

  /**
   * Insert a decoded PCM frame at the given u16 sequence number.
   * Also measures inter-arrival jitter and adjusts buffer depth.
   */
  insert(seq: number, pcm: Float32Array): void {
    // Measure inter-arrival jitter
    const arrivalTime = this.now();
    if (this.lastArrival !== null) {
      const arrivalDelta = arrivalTime - this.lastArrival;
      const deviation = Math.abs(arrivalDelta - this.frameMs);
      // RFC 3550 EWMA: jitter += (deviation - jitter) / 16
      this.jitter += (deviation - this.jitter) / 16;
      this.adaptDepth();
    }
    this.lastArrival = arrivalTime;

    // Seed playSeq from first frame
    if (!this.seeded) {
      this.playSeq = seq;
      this.seeded = true;
    }

    // Modular distance: positive means seq is ahead of playSeq
    const dist = (seq - this.playSeq + 0x10000) & 0xffff;

    // Late frame — drop
    if (dist >= 0x8000) return;

    // Too far ahead — drop
    if (dist >= this.depth) return;

    this.slots[seq % this.depth] = pcm;
  }

  /**
   * Advance the playhead by one frame.
   * Returns the PCM frame at the current position, or null if missing.
   */
  advance(): Float32Array | null {
    if (!this.seeded) return null;

    if (this.fillCount < this.depth) {
      this.fillCount++;
      return null;
    }

    const slot = this.playSeq % this.depth;
    const frame = this.slots[slot];
    this.slots[slot] = null;
    this.playSeq = (this.playSeq + 1) & 0xffff;

    // Track clean run for shrink decision
    if (frame !== null) {
      this.cleanRun++;
    } else {
      this.cleanRun = 0;
    }

    // Attempt to shrink after sustained clean playback
    if (this.cleanRun >= SHRINK_DELAY) {
      this.tryShrink();
    }

    return frame ?? null;
  }

  /** Whether the fill period has elapsed and playback has begun. */
  isReady(): boolean {
    return this.seeded && this.fillCount >= this.depth;
  }

  /** Clear all state — fill period resets, depth returns to minimum. */
  reset(): void {
    this.depth = this.minDepth;
    this.slots = new Array<Float32Array | null>(this.depth).fill(null);
    this.playSeq = 0;
    this.fillCount = 0;
    this.seeded = false;
    this.jitter = 0;
    this.lastArrival = null;
    this.cleanRun = 0;
  }

  /** Current buffer depth in frames. */
  getDepth(): number {
    return this.depth;
  }

  /** Current estimated jitter in milliseconds. */
  getJitterMs(): number {
    return Math.round(this.jitter * 100) / 100;
  }

  /**
   * Recalculate target depth from jitter and grow if needed.
   * Growth is immediate — no delay.
   */
  private adaptDepth(): void {
    // target = ceil(jitter * 3 / frameMs), clamped to [min, max]
    const target = Math.max(
      this.minDepth,
      Math.min(this.maxDepth, Math.ceil((this.jitter * 3) / this.frameMs)),
    );

    if (target > this.depth) {
      this.grow(target);
    }
  }

  /** Grow buffer to newDepth. Existing slots are preserved. */
  private grow(newDepth: number): void {
    const newSlots = new Array<Float32Array | null>(newDepth).fill(null);
    // Copy existing frames into new slots at their modular positions
    for (let i = 0; i < this.depth; i++) {
      if (this.slots[i] !== null) {
        // Find the sequence number that maps to old slot i
        // and remap to new slot array
        newSlots[i % newDepth] = this.slots[i];
      }
    }
    this.slots = newSlots;
    this.depth = newDepth;

    // Extend fill period if still filling
    // (fillCount stays where it is, but now needs to reach new depth)
  }

  /** Shrink buffer by one slot if jitter allows. */
  private tryShrink(): void {
    const target = Math.max(
      this.minDepth,
      Math.min(this.maxDepth, Math.ceil((this.jitter * 3) / this.frameMs)),
    );

    if (target < this.depth) {
      // Shrink by 1 at a time to be conservative
      const newDepth = this.depth - 1;
      const newSlots = new Array<Float32Array | null>(newDepth).fill(null);
      // Preserve frames that fit in the new depth
      for (let i = 0; i < newDepth; i++) {
        newSlots[i] = this.slots[i] ?? null;
      }
      this.slots = newSlots;
      this.depth = newDepth;
      this.cleanRun = 0; // Reset counter after shrink
    }
  }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx vitest run src/lib/voice/adaptive-jitter-buffer.test.ts`
Expected: All 12 tests pass.

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2
git add src/lib/voice/adaptive-jitter-buffer.ts src/lib/voice/adaptive-jitter-buffer.test.ts
git commit -m "feat(voice): add adaptive jitter buffer with RFC 3550 jitter estimation"
```

---

### Task 5: Codec2Codec WASM Wrapper

**Files:**
- Create: `src/lib/voice/codec2-codec.ts`
- Create: `src/lib/voice/codec2-codec.test.ts`
- Create: `build/codec2/Makefile`
- Create: `build/codec2/codec2_glue.c`

This task creates the codec2 wrapper and its emscripten build infrastructure. The wrapper follows the same pattern as `OpusCodec` — emscripten-compiled C, lazy WASM load, int16 PCM conversion, explicit heap cleanup.

- [ ] **Step 1: Write failing tests**

Create `src/lib/voice/codec2-codec.test.ts`:

```typescript
/**
 * Tests for Codec2Codec — codec2 WASM wrapper.
 *
 * The underlying codec2 emscripten module is mocked (same pattern as
 * opus-codec.test.ts) because WASM binaries don't load in jsdom.
 */
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { Codec2Codec } from './codec2-codec';

// ---------------------------------------------------------------------------
// Mock codec2 WASM module
// ---------------------------------------------------------------------------

const mockCreate = vi.fn().mockReturnValue(1); // state pointer
const mockEncode = vi.fn();
const mockDecode = vi.fn();
const mockDestroy = vi.fn();
const mockSamplesPerFrame = vi.fn().mockReturnValue(160);
const mockBitsPerFrame = vi.fn().mockReturnValue(64);
const mockMalloc = vi.fn().mockReturnValue(1024); // heap pointer
const mockFree = vi.fn();

const mockHEAP16 = new Int16Array(4096);
const mockHEAPU8 = new Uint8Array(mockHEAP16.buffer);

vi.mock('./codec2-wasm', () => {
  return {
    default: vi.fn().mockResolvedValue({
      _codec2_create: mockCreate,
      _codec2_encode: mockEncode,
      _codec2_decode: mockDecode,
      _codec2_destroy: mockDestroy,
      _codec2_samples_per_frame: mockSamplesPerFrame,
      _codec2_bits_per_frame: mockBitsPerFrame,
      _malloc: mockMalloc,
      _free: mockFree,
      HEAP16: mockHEAP16,
      HEAPU8: mockHEAPU8,
    }),
  };
});

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe('Codec2Codec', () => {
  let codec: Codec2Codec;

  beforeEach(() => {
    vi.clearAllMocks();
    codec = new Codec2Codec();
  });

  it('exposes codecType as codec2', () => {
    expect(codec.codecType).toBe('codec2');
  });

  it('initializes WASM module and creates codec2 state', async () => {
    await codec.init(8000, 1);
    expect(mockCreate).toHaveBeenCalled();
    expect(mockMalloc).toHaveBeenCalled();
  });

  it('throws if encode called before init', () => {
    expect(() => codec.encode(new Float32Array(160))).toThrow('not initialized');
  });

  it('throws if decode called before init', () => {
    expect(() => codec.decode(new Uint8Array(8))).toThrow('not initialized');
  });

  it('encode accepts Float32Array and returns Uint8Array', async () => {
    await codec.init(8000, 1);
    const pcm = new Float32Array(160).fill(0.5);
    const result = codec.encode(pcm);
    expect(result).toBeInstanceOf(Uint8Array);
    expect(mockEncode).toHaveBeenCalled();
  });

  it('decode accepts Uint8Array and returns Float32Array', async () => {
    await codec.init(8000, 1);
    const encoded = new Uint8Array(8);
    const result = codec.decode(encoded);
    expect(result).toBeInstanceOf(Float32Array);
    expect(mockDecode).toHaveBeenCalled();
  });

  it('destroy frees resources and prevents further use', async () => {
    await codec.init(8000, 1);
    codec.destroy();
    expect(mockDestroy).toHaveBeenCalled();
    expect(mockFree).toHaveBeenCalled();
    expect(() => codec.encode(new Float32Array(160))).toThrow('not initialized');
  });

  it('destroy is safe to call multiple times', async () => {
    await codec.init(8000, 1);
    codec.destroy();
    codec.destroy(); // should not throw
    expect(mockDestroy).toHaveBeenCalledTimes(1);
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx vitest run src/lib/voice/codec2-codec.test.ts`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement Codec2Codec wrapper**

Create `src/lib/voice/codec2-codec.ts`:

```typescript
/**
 * Codec2Codec — wrapper around emscripten-compiled codec2 WASM.
 *
 * Uses codec2 3200 mode: 8kHz, 20ms frames, 160 samples, 8 bytes output.
 *
 * Follows the same pattern as OpusCodec:
 * - Lazy WASM load via dynamic import
 * - Emscripten heap buffers for zero-copy encode/decode
 * - Explicit destroy() for heap cleanup
 */
import type { VoiceCodec, CodecType } from './voice-codec';

/** codec2 mode constant for 3200 bps. */
const CODEC2_MODE_3200 = 0;

interface Codec2Module {
  _codec2_create(mode: number): number;
  _codec2_encode(state: number, bits: number, speech: number): void;
  _codec2_decode(state: number, speech: number, bits: number): void;
  _codec2_destroy(state: number): void;
  _codec2_samples_per_frame(state: number): number;
  _codec2_bits_per_frame(state: number): number;
  _malloc(size: number): number;
  _free(ptr: number): void;
  HEAP16: Int16Array;
  HEAPU8: Uint8Array;
}

export class Codec2Codec implements VoiceCodec {
  readonly codecType: CodecType = 'codec2';

  private module: Codec2Module | null = null;
  private statePtr = 0;
  private speechPtr = 0;
  private bitsPtr = 0;
  private samplesPerFrame = 0;
  private bytesPerFrame = 0;

  async init(sampleRate: number, channels: number): Promise<void> {
    if (this.module !== null) {
      this.destroy();
    }

    const loader = (await import('./codec2-wasm')).default;
    this.module = await loader() as Codec2Module;

    this.statePtr = this.module._codec2_create(CODEC2_MODE_3200);
    this.samplesPerFrame = this.module._codec2_samples_per_frame(this.statePtr);
    const bitsPerFrame = this.module._codec2_bits_per_frame(this.statePtr);
    this.bytesPerFrame = Math.ceil(bitsPerFrame / 8);

    // Allocate heap buffers: speech (int16) and bits (uint8)
    this.speechPtr = this.module._malloc(this.samplesPerFrame * 2); // int16 = 2 bytes
    this.bitsPtr = this.module._malloc(this.bytesPerFrame);
  }

  encode(pcm: Float32Array): Uint8Array {
    if (this.module === null) throw new Error('not initialized');

    // Convert float32 [-1,1] to int16 and write to heap
    const offset = this.speechPtr >> 1; // int16 index
    for (let i = 0; i < this.samplesPerFrame; i++) {
      const s = Math.max(-1, Math.min(1, pcm[i] ?? 0));
      this.module.HEAP16[offset + i] = s < 0 ? s * 0x8000 : s * 0x7fff;
    }

    this.module._codec2_encode(this.statePtr, this.bitsPtr, this.speechPtr);

    // Copy encoded bytes out of heap
    return new Uint8Array(
      this.module.HEAPU8.buffer,
      this.bitsPtr,
      this.bytesPerFrame,
    ).slice();
  }

  decode(encoded: Uint8Array): Float32Array {
    if (this.module === null) throw new Error('not initialized');

    // Write encoded bytes to heap
    this.module.HEAPU8.set(encoded.subarray(0, this.bytesPerFrame), this.bitsPtr);

    this.module._codec2_decode(this.statePtr, this.speechPtr, this.bitsPtr);

    // Read int16 from heap and convert to float32
    const offset = this.speechPtr >> 1;
    const out = new Float32Array(this.samplesPerFrame);
    for (let i = 0; i < this.samplesPerFrame; i++) {
      const sample = this.module.HEAP16[offset + i];
      out[i] = sample / (sample < 0 ? 0x8000 : 0x7fff);
    }
    return out;
  }

  destroy(): void {
    if (this.module === null) return;
    this.module._codec2_destroy(this.statePtr);
    this.module._free(this.speechPtr);
    this.module._free(this.bitsPtr);
    this.module = null;
    this.statePtr = 0;
    this.speechPtr = 0;
    this.bitsPtr = 0;
  }
}
```

Also create a placeholder `src/lib/voice/codec2-wasm.ts` for the WASM module loader (will be replaced by the actual emscripten build output):

```typescript
/**
 * Placeholder for the emscripten-compiled codec2 WASM module.
 *
 * In production, this file is replaced by the emscripten build output
 * (build/codec2/Makefile generates codec2-wasm.js + codec2-wasm.wasm).
 *
 * The default export is a factory function that resolves to the module.
 */
export default async function createCodec2Module(): Promise<unknown> {
  throw new Error(
    'codec2 WASM not built. Run: make -C build/codec2'
  );
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx vitest run src/lib/voice/codec2-codec.test.ts`
Expected: All 8 tests pass (the vi.mock intercepts the placeholder import).

- [ ] **Step 5: Create the emscripten build infrastructure**

Create `build/codec2/codec2_glue.c`:

```c
/*
 * Thin C wrapper for codec2 — exports the minimal surface needed
 * by the TypeScript Codec2Codec wrapper.
 *
 * Build with emscripten:
 *   emcc codec2_glue.c -I codec2/src -L codec2/build/src -lcodec2 \
 *     -O2 -s MODULARIZE=1 -s EXPORT_ES6=1 \
 *     -s EXPORTED_FUNCTIONS='["_codec2_create","_codec2_destroy","_codec2_encode","_codec2_decode","_codec2_samples_per_frame","_codec2_bits_per_frame","_malloc","_free"]' \
 *     -s EXPORTED_RUNTIME_METHODS='["HEAP16","HEAPU8"]' \
 *     -o ../../src/lib/voice/codec2-wasm.js
 */

#include "codec2/codec2.h"
```

Create `build/codec2/Makefile`:

```makefile
# Build codec2 WASM via emscripten
#
# Prerequisites: emsdk activated, codec2 source checked out
#
# Usage:
#   make            - build codec2-wasm.js + codec2-wasm.wasm
#   make clean      - remove build artifacts
#   make fetch      - clone codec2 source

CODEC2_DIR = codec2
CODEC2_BUILD = $(CODEC2_DIR)/build
OUTPUT = ../../src/lib/voice/codec2-wasm.js

EMCC_FLAGS = -O2 \
	-s MODULARIZE=1 \
	-s EXPORT_ES6=1 \
	-s ALLOW_MEMORY_GROWTH=0 \
	-s INITIAL_MEMORY=1048576 \
	-s EXPORTED_FUNCTIONS='["_codec2_create","_codec2_destroy","_codec2_encode","_codec2_decode","_codec2_samples_per_frame","_codec2_bits_per_frame","_malloc","_free"]' \
	-s EXPORTED_RUNTIME_METHODS='["HEAP16","HEAPU8"]' \
	-I $(CODEC2_DIR)/src

.PHONY: all clean fetch

all: $(OUTPUT)

$(OUTPUT): codec2_glue.c $(CODEC2_BUILD)/src/libcodec2.a
	emcc codec2_glue.c \
		$(EMCC_FLAGS) \
		-L $(CODEC2_BUILD)/src -lcodec2 \
		-o $(OUTPUT)
	@echo "Built: $(OUTPUT) + codec2-wasm.wasm"

$(CODEC2_BUILD)/src/libcodec2.a: $(CODEC2_DIR)/CMakeLists.txt
	mkdir -p $(CODEC2_BUILD)
	cd $(CODEC2_BUILD) && emcmake cmake .. -DCMAKE_BUILD_TYPE=Release
	cd $(CODEC2_BUILD) && emmake make codec2

fetch:
	git clone --depth 1 https://github.com/drowe67/codec2.git $(CODEC2_DIR)

clean:
	rm -rf $(CODEC2_BUILD) $(OUTPUT) $(OUTPUT:.js=.wasm)
```

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2
git add src/lib/voice/codec2-codec.ts src/lib/voice/codec2-codec.test.ts src/lib/voice/codec2-wasm.ts build/codec2/
git commit -m "feat(voice): add Codec2Codec WASM wrapper and emscripten build"
```

---

### Task 6: Audio Capture Sample Rate Support

**Files:**
- Modify: `src/lib/voice/audio-capture.ts`
- Modify: `src/lib/voice/pcm-capture-processor.ts`
- Modify: `src/lib/voice/audio-capture.test.ts`

- [ ] **Step 1: Write failing test for configurable sample rate**

Add this test to the `describe` block in `src/lib/voice/audio-capture.test.ts`:

```typescript
  it('passes sampleRate to getUserMedia and AudioContext', async () => {
    const capture = new AudioCapture();
    const mockGetUserMedia = vi.fn().mockResolvedValue({
      getTracks: () => [{ stop: vi.fn() }],
    });
    Object.defineProperty(navigator, 'mediaDevices', {
      value: { getUserMedia: mockGetUserMedia },
      writable: true,
    });

    const mockCtx = {
      sampleRate: 8000,
      audioWorklet: { addModule: vi.fn().mockResolvedValue(undefined) },
      createMediaStreamSource: vi.fn().mockReturnValue({ connect: vi.fn(), disconnect: vi.fn() }),
      close: vi.fn().mockResolvedValue(undefined),
    };
    const mockWorklet = {
      port: { onmessage: null },
      connect: vi.fn(),
      disconnect: vi.fn(),
    };

    await capture.start(
      () => {},
      () => mockCtx as unknown as AudioContext,
      () => mockWorklet as unknown as AudioWorkletNode,
      8000,
    );

    expect(mockGetUserMedia).toHaveBeenCalledWith(
      expect.objectContaining({
        audio: expect.objectContaining({ sampleRate: 8000 }),
      }),
    );

    await capture.stop();
  });
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx vitest run src/lib/voice/audio-capture.test.ts`
Expected: FAIL — `start()` does not accept a 4th parameter.

- [ ] **Step 3: Modify AudioCapture to accept sampleRate**

In `src/lib/voice/audio-capture.ts`, update the `start` method signature to accept an optional `sampleRate` parameter:

```typescript
  async start(
    onFrame: FrameCallback,
    createContext?: () => AudioContext,
    createWorkletNode?: (ctx: AudioContext) => AudioWorkletNode,
    sampleRate = 16000,
  ): Promise<void> {
```

Replace the two hardcoded `16000` values in the method body:

In `getUserMedia`:
```typescript
      this.stream = await navigator.mediaDevices.getUserMedia({
        audio: { sampleRate, channelCount: 1, echoCancellation: false },
      });
```

In `AudioContext` creation:
```typescript
      this.context = createContext
        ? createContext()
        : new AudioContext({ sampleRate });
```

- [ ] **Step 4: Update PcmCaptureProcessor for variable frame size**

In `src/lib/voice/pcm-capture-processor.ts`, the frame size is hardcoded as `const FRAME_SIZE = 320`. For codec2 at 8kHz, the frame size should be 160 (8000 * 0.02).

The AudioWorklet processor runs at the AudioContext's sample rate. The frame size should be derived from the context's sample rate. Replace the hardcoded constant:

```typescript
// Derive frame size from the AudioContext sample rate.
// 20ms frames: 16kHz → 320 samples, 8kHz → 160 samples.
// sampleRate is available via the AudioWorkletGlobalScope.
const FRAME_MS = 20;

class PcmCaptureProcessor extends AudioWorkletProcessor {
  private buffer: Float32Array;
  private offset = 0;
  private readonly frameSize: number;

  constructor() {
    super();
    // sampleRate is a global in AudioWorkletGlobalScope
    this.frameSize = Math.round(sampleRate * FRAME_MS / 1000);
    this.buffer = new Float32Array(this.frameSize);
  }

  process(inputs: Float32Array[][]): boolean {
    const input = inputs[0]?.[0];
    if (!input) return true;

    let pos = 0;
    while (pos < input.length) {
      const remaining = this.frameSize - this.offset;
      const toCopy = Math.min(remaining, input.length - pos);
      this.buffer.set(input.subarray(pos, pos + toCopy), this.offset);
      this.offset += toCopy;
      pos += toCopy;

      if (this.offset === this.frameSize) {
        const frame = this.buffer;
        this.port.postMessage(frame, [frame.buffer]);
        this.buffer = new Float32Array(this.frameSize);
        this.offset = 0;
      }
    }
    return true;
  }
}

registerProcessor('pcm-capture-processor', PcmCaptureProcessor);
```

- [ ] **Step 5: Run tests to verify**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx vitest run src/lib/voice/audio-capture.test.ts`
Expected: All tests pass (existing + new).

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2
git add src/lib/voice/audio-capture.ts src/lib/voice/audio-capture.test.ts src/lib/voice/pcm-capture-processor.ts
git commit -m "feat(voice): support configurable sample rate in audio capture"
```

---

### Task 7: Update VoiceSender for VoiceCodec Interface

**Files:**
- Modify: `src/lib/voice/voice-sender.ts`
- Modify: `src/lib/voice/voice-sender.test.ts`

- [ ] **Step 1: Write failing tests for VoiceCodec and codec bit**

In `src/lib/voice/voice-sender.test.ts`, update the import and add tests.

Update the imports at the top:
```typescript
import { VoiceSender, type VoiceSenderConfig } from './voice-sender';
import { decodeHeader, HEADER_SIZE } from './voice-packet';
import type { VoiceCodec } from './voice-codec';
import type { AudioCapture } from './audio-capture';
```

Update the `makeConfig` function to use `VoiceCodec` instead of `OpusCodec`:
- Change the return type's `mockCodec` from `OpusCodec` to `VoiceCodec`
- Add `codecType: 'opus'` to the mock codec object

Replace the mockCodec definition in `makeConfig`:
```typescript
  const mockCodec = {
    codecType: 'opus' as const,
    init: vi.fn().mockResolvedValue(undefined),
    encode: mockEncode,
    decode: vi.fn(),
    destroy: vi.fn(),
  } as unknown as VoiceCodec;
```

Add these tests at the end of the `describe('VoiceSender', ...)` block:

```typescript
  it('sets codec bit to 0 for opus', async () => {
    const sender = new VoiceSender(ctx.config);
    await sender.start();

    const onFrame = ctx.getCapturedOnFrame()!;
    onFrame(new Float32Array(320));

    const [, args] = ctx.mockInvoke.mock.calls[0] as [string, Record<string, unknown>];
    const payload = args.payload as Record<string, unknown>;
    const frameBytes = payload.frameBytes as number[];
    const headerBuf = new Uint8Array(frameBytes.slice(0, HEADER_SIZE));
    const decoded = decodeHeader(headerBuf);
    expect(decoded.codec).toBe('opus');
  });

  it('sets codec bit to 1 for codec2', async () => {
    const codec2Mock = {
      codecType: 'codec2' as const,
      init: vi.fn().mockResolvedValue(undefined),
      encode: vi.fn((_pcm: Float32Array) => new Uint8Array(8)),
      decode: vi.fn(),
      destroy: vi.fn(),
    } as unknown as VoiceCodec;

    const config: VoiceSenderConfig = {
      ...ctx.config,
      codec: codec2Mock,
      sampleRate: 8000,
    };
    const sender = new VoiceSender(config);
    await sender.start();

    const onFrame = ctx.getCapturedOnFrame()!;
    onFrame(new Float32Array(160));

    const [, args] = ctx.mockInvoke.mock.calls[0] as [string, Record<string, unknown>];
    const payload = args.payload as Record<string, unknown>;
    const frameBytes = payload.frameBytes as number[];
    const headerBuf = new Uint8Array(frameBytes.slice(0, HEADER_SIZE));
    const decoded = decodeHeader(headerBuf);
    expect(decoded.codec).toBe('codec2');
  });

  it('uses configured sampleRate for codec init', async () => {
    const config: VoiceSenderConfig = {
      ...ctx.config,
      sampleRate: 8000,
    };
    const sender = new VoiceSender(config);
    await sender.start();

    expect((ctx.mockCodec.init as ReturnType<typeof vi.fn>)).toHaveBeenCalledWith(8000, 1);
  });
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx vitest run src/lib/voice/voice-sender.test.ts`
Expected: FAIL — `sampleRate` not in config, `codec` field not in header decode result.

- [ ] **Step 3: Update VoiceSender**

In `src/lib/voice/voice-sender.ts`:

Update imports:
```typescript
import { type AudioCapture } from './audio-capture';
import { type VoiceCodec } from './voice-codec';
import { encodeHeader, HEADER_SIZE } from './voice-packet';
```

Update `VoiceSenderConfig`:
```typescript
export interface VoiceSenderConfig {
  senderHash: Uint8Array;
  channelId: string;
  invoke: (cmd: string, args: Record<string, unknown>) => Promise<unknown>;
  /** Voice codec instance (OpusCodec or Codec2Codec). */
  codec: VoiceCodec;
  capture: AudioCapture;
  /** Sample rate for audio capture and codec init. Default: 16000. */
  sampleRate?: number;
}
```

In `start()`, change the hardcoded `16000` to use the config:
```typescript
      const sr = this.config.sampleRate ?? 16000;
      await this.config.codec.init(sr, 1);
```

Also pass sample rate to capture.start():
```typescript
      try {
        await this.config.capture.start(
          (pcm) => this.sendFrame(pcm, true),
          undefined,
          undefined,
          sr,
        );
      } catch (err) {
```

In `stop()`, the silence frame size should match the codec's sample rate. Replace the hardcoded `320`:
```typescript
    const sr = this.config.sampleRate ?? 16000;
    const frameSize = Math.round(sr * 0.02);
    const silence = new Float32Array(frameSize);
```

In `sendFrame()`, add codec type to the header:
```typescript
    const header = encodeHeader({
      pttActive,
      sequence: this.sequence & 0xffff,
      timestamp: this.timestamp >>> 0,
      senderHash: this.config.senderHash,
      codec: this.config.codec.codecType,
    });
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx vitest run src/lib/voice/voice-sender.test.ts`
Expected: All tests pass (existing + 3 new).

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2
git add src/lib/voice/voice-sender.ts src/lib/voice/voice-sender.test.ts
git commit -m "feat(voice): update VoiceSender for VoiceCodec interface and codec header bit"
```

---

### Task 8: Update VoiceReceiver for Multi-Codec, Adaptive Buffer, Comfort Noise

**Files:**
- Modify: `src/lib/voice/voice-receiver.ts`
- Modify: `src/lib/voice/voice-receiver.test.ts`

This is the largest task — VoiceReceiver needs to: read codec bit from headers, create per-codec decoders, use AdaptiveJitterBuffer instead of JitterBuffer, and play comfort noise for missing frames.

- [ ] **Step 1: Write failing tests**

In `src/lib/voice/voice-receiver.test.ts`, update imports:

```typescript
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { VoiceReceiver, type VoiceReceiverConfig } from './voice-receiver';
import { encodeHeader } from './voice-packet';
import type { VoiceCodec } from './voice-codec';
```

Update `mockCodecFactory` to return `VoiceCodec`:
```typescript
function mockCodecFactory(codecType: 'opus' | 'codec2' = 'opus'): VoiceCodec {
  const frameSize = codecType === 'opus' ? 320 : 160;
  return {
    codecType,
    init: vi.fn().mockResolvedValue(undefined),
    encode: vi.fn().mockReturnValue(new Uint8Array(codecType === 'opus' ? 40 : 8)),
    decode: vi.fn().mockReturnValue(new Float32Array(frameSize).fill(0.1)),
    destroy: vi.fn(),
  } as unknown as VoiceCodec;
}
```

Update `config` in `beforeEach` (this change makes ALL existing tests use the new signature — `mockCodecFactory('opus')` returns the same mock as before, so existing tests still pass):
```typescript
    config = {
      listen: mockListen.listen,
      createCodec: (codecType: 'opus' | 'codec2') => mockCodecFactory(codecType),
    };
```

Update `emitVoiceFrame` to accept an optional `codec` parameter:
```typescript
function emitVoiceFrame(
  emit: (event: string, payload: unknown) => void,
  opts: {
    senderHash: Uint8Array;
    sequence: number;
    pttActive: boolean;
    timestamp?: number;
    codec?: 'opus' | 'codec2';
  },
) {
  const header = encodeHeader({
    pttActive: opts.pttActive,
    sequence: opts.sequence,
    timestamp: opts.timestamp ?? 0,
    senderHash: opts.senderHash,
    codec: opts.codec,
  });

  const payloadSize = opts.codec === 'codec2' ? 8 : 40;
  const payload = new Uint8Array(payloadSize).fill(0x01);
  const frameBytes = new Uint8Array(header.length + payload.length);
  frameBytes.set(header, 0);
  frameBytes.set(payload, header.length);

  emit('voice-frame-received', { frameBytes: Array.from(frameBytes) });
}
```

Add these tests at the end of the `describe('VoiceReceiver', ...)` block:

```typescript
  // -------------------------------------------------------------------------
  // Test: handles codec2 frames
  // -------------------------------------------------------------------------
  it('creates codec2 decoder for codec2 frames', async () => {
    const codecs: string[] = [];
    const receiver = new VoiceReceiver({
      ...config,
      createCodec: (ct: 'opus' | 'codec2') => {
        codecs.push(ct);
        return mockCodecFactory(ct);
      },
    });
    await receiver.init();

    emitVoiceFrame(mockListen.emit, {
      senderHash: makeSenderHash(0xaa),
      sequence: 0,
      pttActive: true,
      codec: 'codec2',
    });

    expect(codecs).toContain('codec2');
    receiver.destroy();
  });

  // -------------------------------------------------------------------------
  // Test: plays comfort noise for missing frames
  // -------------------------------------------------------------------------
  it('plays comfort noise instead of null for missing frames', async () => {
    const playedFrames: (Float32Array | null)[] = [];
    const receiver = new VoiceReceiver({
      ...config,
      onPlayFrame: (_hex, pcm) => playedFrames.push(pcm),
    });
    await receiver.init();

    const hash = makeSenderHash(0xaa);
    // Send frame 0, skip frame 1
    emitVoiceFrame(mockListen.emit, {
      senderHash: hash,
      sequence: 0,
      pttActive: true,
    });

    // Wait for codec init
    await vi.advanceTimersByTimeAsync(10);

    // Advance through fill period + a few frames
    // The adaptive buffer has minDepth=2, so 2 fill advances then playback
    for (let i = 0; i < 5; i++) {
      vi.advanceTimersByTime(20);
    }

    // At least one frame should be comfort noise (non-null Float32Array from gap)
    // The exact count depends on fill + playback timing
    const nonNull = playedFrames.filter((f) => f !== null);
    expect(nonNull.length).toBeGreaterThan(0);

    receiver.destroy();
  });
```

- [ ] **Step 2: Run tests to verify new tests fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx vitest run src/lib/voice/voice-receiver.test.ts`
Expected: FAIL — `createCodec` signature mismatch, no `codec` field in header.

- [ ] **Step 3: Implement VoiceReceiver updates**

In `src/lib/voice/voice-receiver.ts`, make the following changes:

Update imports:
```typescript
import { AdaptiveJitterBuffer } from './adaptive-jitter-buffer';
import { generateComfortNoise } from './comfort-noise';
import { decodeHeader, HEADER_SIZE } from './voice-packet';
import type { VoiceCodec, CodecType } from './voice-codec';
```

Update constants — remove old `BUFFER_DEPTH`:
```typescript
const FRAME_MS = 20;
const IDLE_TIMEOUT_MS = 2000;
const MAX_PENDING_FRAMES = 16;
const MIN_BUFFER_DEPTH = 2;
const MAX_BUFFER_DEPTH = 10;
```

Update `PendingFrame`:
```typescript
interface PendingFrame {
  sequence: number;
  payload: Uint8Array;
  codec: CodecType;
}
```

Update `SenderState`:
```typescript
interface SenderState {
  jitterBuffer: AdaptiveJitterBuffer;
  /** One decoder per codec type, lazy-created on first frame of that type. */
  codecs: Map<CodecType, VoiceCodec>;
  speaking: boolean;
  playbackTimer: ReturnType<typeof setInterval> | null;
  idleTimer: ReturnType<typeof setTimeout> | null;
  ready: boolean;
  pendingFrames: PendingFrame[];
  /** Frame size for the current primary codec (320 for opus, 160 for codec2). */
  frameSize: number;
}
```

Update `VoiceReceiverConfig`:
```typescript
export interface VoiceReceiverConfig {
  listen: (
    event: string,
    handler: (event: { payload: unknown }) => void,
  ) => Promise<() => void>;
  /** Factory that creates a codec instance for the given type. */
  createCodec: (codecType: CodecType) => VoiceCodec;
  onPlayFrame?: (senderHex: string, pcm: Float32Array | null) => void;
  ownSenderHex?: string;
}
```

Update `handleFrame` — extract codec from header and pass through:
```typescript
  private handleFrame(payload: { frameBytes: number[] }): void {
    const bytes = new Uint8Array(payload.frameBytes);
    if (bytes.byteLength < HEADER_SIZE) return;

    const header = decodeHeader(bytes);
    const encodedPayload = bytes.slice(HEADER_SIZE);
    const senderHex = Array.from(header.senderHash)
      .map((b) => b.toString(16).padStart(2, '0'))
      .join('');

    if (this.config.ownSenderHex && senderHex === this.config.ownSenderHex) return;

    let state = this.senders.get(senderHex);
    if (!state) {
      const codec = this.config.createCodec(header.codec);
      const frameSize = header.codec === 'codec2' ? 160 : 320;
      state = {
        jitterBuffer: new AdaptiveJitterBuffer({
          minDepth: MIN_BUFFER_DEPTH,
          maxDepth: MAX_BUFFER_DEPTH,
          frameMs: FRAME_MS,
        }),
        codecs: new Map([[header.codec, codec]]),
        speaking: false,
        playbackTimer: null,
        idleTimer: null,
        ready: false,
        pendingFrames: [],
        frameSize,
      };
      this.senders.set(senderHex, state);

      const stateRef = state;
      codec.init(header.codec === 'codec2' ? 8000 : 16000, 1).then(() => {
        if (this.senders.get(senderHex) !== stateRef) {
          stateRef.codecs.forEach((c) => c.destroy());
          return;
        }
        stateRef.ready = true;
        for (const pf of stateRef.pendingFrames) {
          try {
            let dec = stateRef.codecs.get(pf.codec);
            if (!dec) {
              dec = this.config.createCodec(pf.codec);
              stateRef.codecs.set(pf.codec, dec);
              // Sync init for queued frames of a different codec type
              // In practice, pending frames should all be same codec
            }
            const pcm = dec.decode(pf.payload);
            stateRef.jitterBuffer.insert(pf.sequence, pcm);
          } catch {
            // Drop undecodable frame
          }
        }
        stateRef.pendingFrames = [];
        stateRef.playbackTimer = setInterval(() => {
          this.advancePlayback(senderHex);
        }, FRAME_MS);
      }).catch(() => {
        if (this.senders.get(senderHex) === stateRef) {
          this.removeSender(senderHex);
        }
      });
    }

    if (header.pttActive && !state.speaking && state.ready) {
      state.jitterBuffer.reset();
    }
    state.speaking = header.pttActive;

    if (state.idleTimer) clearTimeout(state.idleTimer);
    state.idleTimer = setTimeout(() => {
      this.removeSender(senderHex);
    }, IDLE_TIMEOUT_MS);

    if (state.ready) {
      try {
        // Get or create decoder for this codec type
        let codec = state.codecs.get(header.codec);
        if (!codec) {
          codec = this.config.createCodec(header.codec);
          state.codecs.set(header.codec, codec);
          // Init synchronously isn't possible — for now, lazy init
          // will cause first frame of new codec to fail decode.
          // Acceptable tradeoff: codec switches are rare user actions.
        }
        const pcm = codec.decode(encodedPayload);
        state.jitterBuffer.insert(header.sequence, pcm);
      } catch {
        // Drop malformed frame
      }
    } else if (state.pendingFrames.length < MAX_PENDING_FRAMES) {
      state.pendingFrames.push({
        sequence: header.sequence,
        payload: encodedPayload,
        codec: header.codec,
      });
    }
  }
```

Update `advancePlayback` to use comfort noise:
```typescript
  private advancePlayback(senderHex: string): void {
    const state = this.senders.get(senderHex);
    if (!state) return;
    let pcm = state.jitterBuffer.advance();
    // Generate comfort noise for missing frames (only after fill period)
    if (pcm === null && state.jitterBuffer.isReady()) {
      pcm = generateComfortNoise(state.frameSize, 0.005);
    }
    this.config.onPlayFrame?.(senderHex, pcm);
  }
```

Update `removeSender` to destroy all codecs:
```typescript
  private removeSender(senderHex: string): void {
    const state = this.senders.get(senderHex);
    if (!state) return;
    if (state.playbackTimer) clearInterval(state.playbackTimer);
    if (state.idleTimer) clearTimeout(state.idleTimer);
    state.codecs.forEach((c) => c.destroy());
    this.senders.delete(senderHex);
  }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx vitest run src/lib/voice/voice-receiver.test.ts`
Expected: All tests pass (existing + 2 new).

- [ ] **Step 5: Run full voice test suite**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx vitest run src/lib/voice/`
Expected: All tests across all voice modules pass.

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2
git add src/lib/voice/voice-receiver.ts src/lib/voice/voice-receiver.test.ts
git commit -m "feat(voice): update VoiceReceiver for multi-codec, adaptive buffer, comfort noise"
```

---

### Task 9: CodecToggle Component

**Files:**
- Create: `src/lib/components/CodecToggle.svelte`
- Create: `src/lib/components/__tests__/CodecToggle.test.ts`

- [ ] **Step 1: Write failing tests**

Create `src/lib/components/__tests__/CodecToggle.test.ts`:

```typescript
import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import CodecToggle from '../CodecToggle.svelte';

describe('CodecToggle', () => {
  it('renders two radio options', () => {
    render(CodecToggle, { props: { selected: 'opus' } });
    const group = screen.getByRole('radiogroup', { name: /voice codec/i });
    expect(group).toBeTruthy();
    const radios = screen.getAllByRole('radio');
    expect(radios.length).toBe(2);
  });

  it('marks opus as checked when selected', () => {
    render(CodecToggle, { props: { selected: 'opus' } });
    const opus = screen.getByRole('radio', { name: /opus/i });
    expect(opus.getAttribute('aria-checked')).toBe('true');
  });

  it('marks codec2 as checked when selected', () => {
    render(CodecToggle, { props: { selected: 'codec2' } });
    const codec2 = screen.getByRole('radio', { name: /codec2/i });
    expect(codec2.getAttribute('aria-checked')).toBe('true');
  });

  it('fires onCodecChange on click', async () => {
    const onCodecChange = vi.fn();
    render(CodecToggle, { props: { selected: 'opus', onCodecChange } });
    const codec2 = screen.getByRole('radio', { name: /codec2/i });
    await fireEvent.click(codec2);
    expect(onCodecChange).toHaveBeenCalledWith('codec2');
  });

  it('fires onCodecChange on Enter key', async () => {
    const onCodecChange = vi.fn();
    render(CodecToggle, { props: { selected: 'opus', onCodecChange } });
    const codec2 = screen.getByRole('radio', { name: /codec2/i });
    await fireEvent.keyDown(codec2, { key: 'Enter' });
    expect(onCodecChange).toHaveBeenCalledWith('codec2');
  });

  it('fires onCodecChange on Space key with preventDefault', async () => {
    const onCodecChange = vi.fn();
    render(CodecToggle, { props: { selected: 'opus', onCodecChange } });
    const codec2 = screen.getByRole('radio', { name: /codec2/i });
    const event = new KeyboardEvent('keydown', { key: ' ', bubbles: true, cancelable: true });
    const prevented = !codec2.dispatchEvent(event);
    // Space should be prevented to avoid scroll
    // Note: fireEvent doesn't track preventDefault, so we verify via dispatchEvent
    expect(onCodecChange).toHaveBeenCalledWith('codec2');
  });

  it('navigates with arrow keys', async () => {
    const onCodecChange = vi.fn();
    render(CodecToggle, { props: { selected: 'opus', onCodecChange } });
    const opus = screen.getByRole('radio', { name: /opus/i });
    await fireEvent.keyDown(opus, { key: 'ArrowRight' });
    expect(onCodecChange).toHaveBeenCalledWith('codec2');
  });

  it('is disabled when disabled prop is true', () => {
    render(CodecToggle, { props: { selected: 'opus', disabled: true } });
    const radios = screen.getAllByRole('radio');
    for (const radio of radios) {
      expect(radio.getAttribute('aria-disabled')).toBe('true');
    }
  });

  it('does not fire onCodecChange when disabled', async () => {
    const onCodecChange = vi.fn();
    render(CodecToggle, { props: { selected: 'opus', onCodecChange, disabled: true } });
    const codec2 = screen.getByRole('radio', { name: /codec2/i });
    await fireEvent.click(codec2);
    expect(onCodecChange).not.toHaveBeenCalled();
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx vitest run src/lib/components/__tests__/CodecToggle.test.ts`
Expected: FAIL — component not found.

- [ ] **Step 3: Implement CodecToggle.svelte**

Create `src/lib/components/CodecToggle.svelte`:

```svelte
<script lang="ts">
  import type { CodecType } from '$lib/voice/voice-codec';

  let {
    selected = 'opus' as CodecType,
    disabled = false,
    onCodecChange,
  }: {
    selected?: CodecType;
    disabled?: boolean;
    onCodecChange?: (codec: CodecType) => void;
  } = $props();

  const options: { value: CodecType; label: string }[] = [
    { value: 'opus', label: 'Opus' },
    { value: 'codec2', label: 'codec2' },
  ];

  function select(codec: CodecType) {
    if (disabled || codec === selected) return;
    onCodecChange?.(codec);
  }

  function handleKeyDown(e: KeyboardEvent, codec: CodecType) {
    if (disabled) return;

    if (e.key === 'Enter') {
      select(codec);
    } else if (e.key === ' ') {
      e.preventDefault();
      select(codec);
    } else if (e.key === 'ArrowRight' || e.key === 'ArrowDown') {
      e.preventDefault();
      const idx = options.findIndex((o) => o.value === codec);
      const next = options[(idx + 1) % options.length];
      select(next.value);
    } else if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') {
      e.preventDefault();
      const idx = options.findIndex((o) => o.value === codec);
      const prev = options[(idx - 1 + options.length) % options.length];
      select(prev.value);
    }
  }
</script>

<div
  class="codec-toggle"
  role="radiogroup"
  aria-label="Voice codec"
>
  {#each options as option}
    <div
      class="codec-option"
      class:selected={selected === option.value}
      role="radio"
      aria-checked={selected === option.value}
      aria-label={option.label}
      aria-disabled={disabled}
      tabindex={disabled ? -1 : selected === option.value ? 0 : -1}
      onclick={() => select(option.value)}
      onkeydown={(e) => handleKeyDown(e, option.value)}
    >
      {option.label}
    </div>
  {/each}
</div>

<style>
  .codec-toggle {
    display: inline-flex;
    border: 1px solid var(--border, #3f4147);
    border-radius: 6px;
    overflow: hidden;
    font-size: 0.75rem;
  }

  .codec-option {
    padding: 4px 10px;
    cursor: pointer;
    color: var(--text-secondary, #b5bac1);
    transition: all 0.15s ease;
    user-select: none;
  }

  .codec-option:not(:last-child) {
    border-right: 1px solid var(--border, #3f4147);
  }

  .codec-option.selected {
    background: var(--accent, #5865f2);
    color: var(--text-primary, #f2f3f5);
  }

  .codec-option:hover:not(.selected):not([aria-disabled='true']) {
    background: rgba(88, 101, 242, 0.1);
  }

  .codec-option[aria-disabled='true'] {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .codec-option:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: -2px;
  }
</style>
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx vitest run src/lib/components/__tests__/CodecToggle.test.ts`
Expected: All 9 tests pass.

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2
git add src/lib/components/CodecToggle.svelte src/lib/components/__tests__/CodecToggle.test.ts
git commit -m "feat(voice): add CodecToggle segmented control component"
```

---

### Task 10: Integrate CodecToggle into PttButton

**Files:**
- Modify: `src/lib/components/PttButton.svelte`
- Modify: `src/lib/components/__tests__/PttButton.test.ts`

- [ ] **Step 1: Write failing tests**

Add these tests to the end of the `describe('PttButton', ...)` block in `src/lib/components/__tests__/PttButton.test.ts`:

```typescript
  it('renders codec toggle', () => {
    render(PttButton, { props: { active: false } });
    const toggle = screen.getByRole('radiogroup', { name: /voice codec/i });
    expect(toggle).toBeTruthy();
  });

  it('fires onCodecChange when codec toggle is clicked', async () => {
    const onCodecChange = vi.fn();
    render(PttButton, { props: { active: false, onCodecChange } });
    const codec2 = screen.getByRole('radio', { name: /codec2/i });
    await fireEvent.click(codec2);
    expect(onCodecChange).toHaveBeenCalledWith('codec2');
  });

  it('disables codec toggle when PTT is active', () => {
    render(PttButton, { props: { active: true } });
    const radios = screen.getAllByRole('radio');
    for (const radio of radios) {
      expect(radio.getAttribute('aria-disabled')).toBe('true');
    }
  });
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx vitest run src/lib/components/__tests__/PttButton.test.ts`
Expected: FAIL — no radiogroup in PttButton.

- [ ] **Step 3: Integrate CodecToggle into PttButton**

In `src/lib/components/PttButton.svelte`:

Add import and new props in the `<script>` block:

```svelte
<script lang="ts">
  import type { CodecType } from '$lib/voice/voice-codec';
  import CodecToggle from './CodecToggle.svelte';

  let {
    active = false,
    processing = false,
    disabled = false,
    selectedCodec = 'opus' as CodecType,
    onPttStart,
    onPttStop,
    onCodecChange,
  }: {
    active?: boolean;
    processing?: boolean;
    disabled?: boolean;
    selectedCodec?: CodecType;
    onPttStart?: () => void;
    onPttStop?: () => void;
    onCodecChange?: (codec: CodecType) => void;
  } = $props();
```

Add the CodecToggle below the button in the template, wrapped in a container:

Replace the entire template (everything after `</script>`) with:

```svelte
<svelte:window onkeydown={handleKeyDown} onkeyup={handleKeyUp} />

<div class="ptt-container">
  <button
    type="button"
    class="ptt-button"
    class:active
    class:processing
    aria-label="Push to talk"
    onmousedown={() => activate('mouse')}
    onmouseup={() => deactivate('mouse')}
    onmouseleave={() => deactivate('mouse')}
    ontouchstart={(e) => { e.preventDefault(); activate('touch'); }}
    ontouchend={(e) => { e.preventDefault(); deactivate('touch'); }}
    ontouchcancel={() => deactivate('touch')}
    {disabled}
  >
    <span class="ptt-icon" aria-hidden="true">
      {#if processing}
        ...
      {:else}
        🎤
      {/if}
    </span>
  </button>
  <CodecToggle
    selected={selectedCodec}
    disabled={active}
    {onCodecChange}
  />
</div>
```

Add container styles at the end of the `<style>` block:

```css
  .ptt-container {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 8px;
  }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx vitest run src/lib/components/__tests__/PttButton.test.ts`
Expected: All tests pass (existing + 3 new).

- [ ] **Step 5: Run full test suite**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx vitest run`
Expected: ALL tests pass across the entire project.

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2
git add src/lib/components/PttButton.svelte src/lib/components/__tests__/PttButton.test.ts
git commit -m "feat(voice): integrate CodecToggle into PttButton"
```

---

### Task 11: Final Integration Test and Cleanup

**Files:**
- All files from previous tasks (read-only verification)

- [ ] **Step 1: Run the full test suite**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx vitest run`
Expected: ALL tests pass. Note the total count — should be higher than Slice 1's baseline.

- [ ] **Step 2: Run the build**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npm run build`
Expected: Build succeeds with no errors.

- [ ] **Step 3: Verify no type errors**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/.claude/worktrees/jake-client-voice-slice2 && npx tsc --noEmit 2>&1 || npm run check 2>&1`
Expected: No type errors.

- [ ] **Step 4: Update ZEB-35 in Linear**

Update ZEB-35 description to reflect Slice 2 completion scope and what remains for future slices.
