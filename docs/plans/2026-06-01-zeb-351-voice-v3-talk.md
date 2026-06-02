# Voice V3 — Talk (Session Controller, VAD/Mute/PTT, N-Stream Mix, VoiceChannelView) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the existing (already-N-stream) browser voice engine into community voice channels so members can **talk and hear each other** — adding voice-activity detection (VAD/DTX), an output mixer, a one-session-at-a-time session controller with mute/PTT/deafen, a dynamic mute→beacon path in Rust, and a full `VoiceChannelView` hybrid grid↔list UI.

**Architecture:** The engine (`src/lib/voice/`) already provides `VoiceSender` (capture→encode→`send_voice_frame` IPC) and an **N-stream** `VoiceReceiver` (`onPlayFrame(senderHex, pcm)` per sender) — neither has a production caller yet; V3 is their first driver. V3 adds three new browser modules — `vad.ts` (RMS energy + hangover gate), `voice-mixer.ts` + `pcm-playback-processor.ts` (sum per-sender PCM → soft-clip → playback AudioWorklet), and `voice-session.ts` (a Svelte-store session controller that owns the sender — gated by VAD/mute/PTT — the receiver, the mixer, the presence-driven roster, and member-card resolution). `VoiceSender` gains an optional `frameGate` so the controller decides send/PTT per frame. On the Rust side, the presence publisher's hardcoded `muted: true` becomes a shared `Arc<AtomicBool>` driven by a new `set_voice_muted` IPC + `VoiceChannelRequest::SetMuted` (which also emits an immediate beacon). `VoiceChannelView.svelte` is rewritten from the V1 scaffold into the real UI: join flow (mic permission once, **start muted**, one-tap unmute), header, control bar (Mute / PTT / Deafen / Leave), and a hybrid layout (avatar-tile stage grid ≤ ~12 participants, auto-collapsing to a compact roster list beyond).

**Tech Stack:** Svelte 5 (`$props`/`$state`/`$derived`/`$effect`), TypeScript, Web Audio `AudioWorklet`, `vitest` + `@testing-library/svelte`; Rust (Tauri backend), `tokio`, `Arc<AtomicBool>`, existing `ChaCha20-Poly1305` channel AEAD + `ed25519-dalek` beacon signing, `cargo-nextest`.

---

## Background context (read before starting)

Verified against the current tree on `main` at `9537c27` (the ZEB-350 V2 merge). **Line numbers drift as you edit — re-grep before trusting an offset.** Cargo commands run from `src-tauri/`; frontend commands from the repo root.

### The voice engine — what already exists (DO NOT rebuild)

`src/lib/voice/` (all tested):

- **`voice-codec.ts`** — `interface VoiceCodec { init(sampleRate, channels): Promise<void>; encode(pcm: Float32Array): Uint8Array; decode(encoded: Uint8Array): Float32Array; destroy(): void; readonly codecType: CodecType }`; `type CodecType = 'opus' | 'codec2'`.
- **`opus-codec.ts`** — `OpusCodec implements VoiceCodec` (16 kHz mono, 20 ms = 320 samples/frame).
- **`codec2-codec.ts`** — `Codec2Codec` (8 kHz mono, 160 samples/frame). V3 uses **Opus**.
- **`voice-packet.ts`** — `HEADER_SIZE = 23`; `encodeHeader(fields)` / `decodeHeader(buf)`; header carries `pttActive: boolean`, `sequence: u16`, `timestamp: u32`, `senderHash: Uint8Array(16)`, `codec`.
- **`audio-capture.ts`** — `class AudioCapture { start(onFrame: (pcm: Float32Array) => void, createContext?, createWorkletNode?, sampleRate = 16000): Promise<void>; stop(): Promise<void>; isActive(): boolean }`. `start()` calls `getUserMedia({ audio: { sampleRate, channelCount: 1, echoCancellation: false } })` (this is the mic-permission prompt) and registers the `pcm-capture-processor` worklet. The `createContext`/`createWorkletNode` injection params are how tests mock Web Audio.
- **`pcm-capture-processor.ts`** — capture-side `AudioWorkletProcessor`; accumulates 20 ms frames (`Math.round(sampleRate * 20 / 1000)` samples) and `postMessage`s them.
- **`voice-sender.ts`** — `class VoiceSender { constructor(config: VoiceSenderConfig); start(): Promise<void>; stop(): Promise<void> }`. Config: `{ senderHash: Uint8Array(16); communityId: string; channelId: string; invoke; codec: VoiceCodec; capture: AudioCapture; sampleRate?: number }`. **`start()` currently hardcodes `capture.start((pcm) => this.sendFrame(pcm, true), …, sr)` — every frame is sent with `pttActive=true`. `sendFrame(pcm, pttActive)` is `private`.** `stop()` flushes 3 tail frames with `pttActive=false`. **No production caller exists** (grep: only referenced in a `voice-codec.ts` doc comment).
- **`voice-receiver.ts`** — `class VoiceReceiver { constructor(config: VoiceReceiverConfig); init(): Promise<void>; getActiveSenders(): string[]; isSpeaking(senderHex: string): boolean; destroy(): void }`. Config: `{ listen; createCodec: (CodecType) => VoiceCodec; onPlayFrame?: (senderHex: string, pcm: Float32Array | null) => void; ownSenderHex?: string }`. **Already fully N-stream** (`senders: Map<string, SenderState>`, per-sender jitter buffer + per-codec decoder + 20 ms `playbackTimer`, idle-timeout eviction at 2 s, comfort-noise gap-fill while speaking, `ownSenderHex` self-echo filter). `onPlayFrame` fires once per sender per 20 ms with that sender's decoded PCM (or `null` when no frame and not speaking). **No production caller exists.**
- **`adaptive-jitter-buffer.ts`**, **`comfort-noise.ts`** — internal to the receiver; `generateComfortNoise(samples, level = 0.005)` is exported.

**Gap summary (what V3 creates):** no VAD module; no output mixer or playback worklet (only `onPlayFrame` per-sender PCM exists — nothing sums it to speakers); no `voice-session` controller/store; `VoiceSender` has no send gate; the Rust presence publisher hardcodes `muted: true` with no update path; `VoiceChannelView` is a placeholder scaffold.

### V2 comms wiring (the seam V3 plugs into)

- **IPCs** in `src-tauri/src/lib.rs` (re-grep for offsets):
  - `send_voice_frame(payload: voice::SendVoiceFramePayload, state)` — `SendVoiceFramePayload { #[serde(rename_all="camelCase")] community_id: String, channel_id: String, frame_bytes: Vec<u8> }`.
  - `join_voice_channel(community_id: String, channel_id: String, state)` — resolves `VoiceJoinCaps` and sends `VoiceChannelRequest::Join`.
  - `leave_voice_channel(community_id: String, channel_id: String, state)` — sends `VoiceChannelRequest::Leave`.
  - Registered in `tauri::generate_handler!` (re-grep `send_voice_frame,`). **V3 adds `set_voice_muted` to this list.**
- **`src-tauri/src/voice.rs`** — `SendVoiceFramePayload`, `VoiceOutbound`, `VoiceJoinCaps { channel_key, signing_key, self_owner, self_device, joined_hlc }`, and `enum VoiceChannelRequest { Join { community_id: SpaceId, channel_id: ChannelId, caps: VoiceJoinCaps }, Leave { community_id, channel_id } }`. **V3 adds a `SetMuted` variant + a `SetVoiceMutedPayload`.**
- **`src-tauri/src/voice_presence.rs`** — `VoicePresenceBeacon { owner: [u8;16], device: [u8;32], muted: bool, joined_hlc: Hlc, seq: u64, left: bool }`; `sign_presence_beacon`, `seal_presence_beacon`; `spawn_voice_presence_publisher(session, topic, channel_key, community, channel, signing_key, self_owner, self_device, joined_hlc, interval, closing: Arc<AtomicBool>) -> JoinHandle<()>` — its loop builds the beacon with **`muted: true` hardcoded** (re-grep `muted: true`). `RosterEntry { owner, device, muted, joined_hlc, seq }` is what `voice-presence-changed` carries.
- **Events** emitted from `src-tauri/src/event_loop.rs` voice arm (re-grep `voice-frame-received` / `voice-presence-changed`):
  - `voice-frame-received` → `{ frameBytes: number[] }` (Rust opens the sealed packet; payload is the plaintext 23-byte-header + encoded frame).
  - `voice-presence-changed` → `{ community: string (hex), channel: string (hex), roster: RosterEntry[] }` where each entry is `{ owner: hex, device, muted, joinedHlc, seq }` (confirm the exact JS-facing field casing by reading the emit site — match it exactly in the controller).
  - The `Join` arm (re-grep `VoiceChannelRequest::Join`) spawns the media subscriber, presence subscriber, and `spawn_voice_presence_publisher`. **V3 stores the publisher's new mute `Arc<AtomicBool>` keyed by `(community, channel)` so `SetMuted` can flip it.**

### Frontend conventions to match

- **Tauri adapter** (`src/lib/zenoh-service.ts`): `interface TauriAdapter { invoke(cmd, args?): Promise<unknown>; listen(event, handler: (e: { payload: unknown }) => void): Promise<() => void> }`. New code takes a `TauriAdapter` (or the `invoke`/`listen` fns) by injection so tests pass mocks.
- **IPC param casing:** Rust `snake_case` ↔ JS `camelCase` (Tauri auto-converts). `send_voice_frame` is wrapped as `{ payload: { communityId, channelId, frameBytes } }`.
- **Member cards** (`src/lib/member-card-service.ts`): `class MemberCardService { seedSelf(ownerIdHex, card); subscribeVisible(ownerIdHex[]): Promise<void>; unsubscribeAll(): Promise<void>; resolve(ownerIdHex): ResolvedCard | undefined; onUpdate?: () => void }`; `ResolvedCard { displayName; statusText; avatarUrl?; profilePageRoot? }`. Owner keys are **lowercase 32-hex**. V3 resolves roster `owner` (16 bytes → 32 hex) to tiles.
- **Store pattern** (`src/lib/stores/toast.ts`): `writable()` + a `Readable`-typed export object with methods. The session store follows this shape.
- **Tests:** `vitest` + `@testing-library/svelte` (`render`, `screen`, `fireEvent`), `vi.fn()`/`vi.useFakeTimers()`. Web-Audio mocking via the capture factory-injection pattern (see `src/lib/voice/audio-capture.test.ts`); Tauri-event mocking via a `makeMockListen()` map (see `src/lib/voice/voice-receiver.test.ts`). Svelte component tests live in `src/lib/components/__tests__/`.

### Branch + gates

- **Branch:** `git checkout -b zeb-351-voice-v3-talk` off `main` `9537c27` (NEVER a worktree).
- **Gates (run from `src-tauri/` unless noted):**
  - `cargo fmt --all -- --check`
  - `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  - `cargo nextest run --locked --all-targets --features test-fixtures`
  - frontend (repo root): `npx tsc --noEmit` and `npx vitest run`
  - MSRV: `cargo check --locked --all-targets --features test-fixtures` (declared MSRV toolchain)
- 6 iroh/zenoh transport tests fail only in the local loopback sandbox; they pass on CI — non-blocking.

### File structure (created / modified)

| File | Responsibility |
|---|---|
| `src/lib/voice/vad.ts` *(new)* | RMS energy + hangover voice-activity gate (DTX decision). Pure, no Web Audio. |
| `src/lib/voice/vad.test.ts` *(new)* | VAD unit tests (logical frames). |
| `src/lib/voice/voice-sender.ts` *(modify)* | Add optional `frameGate(pcm) => { send, ptt }`; consult per frame. |
| `src/lib/voice/pcm-playback-processor.ts` *(new)* | Playback `AudioWorkletProcessor`: ring-buffers mixed 20 ms frames → render quanta. |
| `src/lib/voice/voice-mixer.ts` *(new)* | Owns playback AudioContext+worklet; sums per-sender PCM with per-sender gain + master gain (deafen) + soft-clip; `pushFrame(senderHex, pcm)`. |
| `src/lib/voice/voice-mixer.test.ts` *(new)* | Mixer sum/soft-clip/gain unit tests (injected Web-Audio mocks). |
| `src/lib/voice-session.ts` *(new)* | Session controller + Svelte store: one session at a time; mute/PTT/deafen; roster from presence; member cards; speaking indicators. |
| `src/lib/voice-session.test.ts` *(new)* | Controller state-machine + gate-override + event-routing tests. |
| `src/lib/components/VoiceChannelView.svelte` *(rewrite)* | Real UI: join flow, control bar, hybrid grid↔list, tiles. |
| `src/lib/components/__tests__/VoiceChannelView.test.ts` *(rewrite)* | Join-muted, control rendering, grid↔list threshold. |
| `src/lib/components/CommunityView.svelte` *(modify)* | Pass `communityId`/`channelId` + session controller into `VoiceChannelView`; teardown on switch/unmount. |
| `src-tauri/src/voice.rs` *(modify)* | `VoiceChannelRequest::SetMuted { community, channel, muted }` + `SetVoiceMutedPayload`. |
| `src-tauri/src/voice_presence.rs` *(modify)* | Publisher reads `Arc<AtomicBool>` mute flag instead of `muted: true`. |
| `src-tauri/src/event_loop.rs` *(modify)* | Store per-channel mute flag; handle `SetMuted` (flip flag + emit immediate beacon). |
| `src-tauri/src/lib.rs` *(modify)* | `set_voice_muted` IPC + register in handler list. |
| `src-tauri/tests/voice_presence_mute_integration.rs` *(new)* | Mute-toggle changes the published beacon's `muted` (two-engine / logical). |

---

## Task 0: Pre-flight baseline

**Files:** none (verification only)

- [ ] **Step 1: Create the branch off latest main**

```bash
cd <repo-root>
git checkout main && git pull --ff-only
git checkout -b zeb-351-voice-v3-talk
git rev-parse HEAD   # expect 9537c27…
```

- [ ] **Step 2: Confirm the engine baseline is green**

```bash
npx vitest run src/lib/voice    # all engine unit tests pass
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures -E 'test(voice_presence)' -E 'test(voice_crypto)'
```

Expected: PASS. Record the voice test count as the regression floor.

- [ ] **Step 3: Re-grep the moving offsets** (record actual line numbers for later tasks)

```bash
cd <repo-root>/src-tauri
grep -n "send_voice_frame\|join_voice_channel\|leave_voice_channel" src/lib.rs
grep -n "muted: true" src/voice_presence.rs
grep -n "VoiceChannelRequest::Join\|spawn_voice_presence_publisher\|voice-presence-changed\|voice-frame-received" src/event_loop.rs
grep -n "enum VoiceChannelRequest" src/voice.rs
```

---

## Task 1: VAD module (`vad.ts`)

**Files:**
- Create: `src/lib/voice/vad.ts`
- Test: `src/lib/voice/vad.test.ts`

The VAD decides, per 20 ms PCM frame, whether the local user is "speaking" for **open-mic/DTX**. Energy = RMS of the frame. Above `threshold` → speaking; once speaking, stay speaking for `hangoverMs` after energy drops (so word-tails aren't clipped). Pure and synchronous — no Web Audio, no timers (the caller feeds frames at 20 ms cadence, so hangover is counted in frames).

- [ ] **Step 1: Write the failing test**

```ts
// src/lib/voice/vad.test.ts
import { describe, it, expect } from 'vitest';
import { VoiceActivityDetector } from './vad';

/** Build a 320-sample (20ms @16k) frame of constant amplitude. */
function frame(amp: number, n = 320): Float32Array {
  const f = new Float32Array(n);
  f.fill(amp);
  return f;
}

describe('VoiceActivityDetector', () => {
  it('reports silence below threshold', () => {
    const vad = new VoiceActivityDetector({ threshold: 0.02, hangoverMs: 200, frameMs: 20 });
    expect(vad.process(frame(0.0))).toBe(false);
    expect(vad.process(frame(0.005))).toBe(false);
  });

  it('reports speaking at/above threshold', () => {
    const vad = new VoiceActivityDetector({ threshold: 0.02, hangoverMs: 200, frameMs: 20 });
    expect(vad.process(frame(0.1))).toBe(true);
  });

  it('holds speaking through hangover then drops', () => {
    const vad = new VoiceActivityDetector({ threshold: 0.02, hangoverMs: 200, frameMs: 20 });
    expect(vad.process(frame(0.1))).toBe(true);      // loud → speaking
    // 200ms hangover / 20ms = 10 silent frames still report speaking
    for (let i = 0; i < 10; i++) {
      expect(vad.process(frame(0.0))).toBe(true);
    }
    // 11th silent frame: hangover expired
    expect(vad.process(frame(0.0))).toBe(false);
  });

  it('reset() clears state', () => {
    const vad = new VoiceActivityDetector({ threshold: 0.02, hangoverMs: 200, frameMs: 20 });
    vad.process(frame(0.1));
    vad.reset();
    expect(vad.process(frame(0.0))).toBe(false);
  });
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `npx vitest run src/lib/voice/vad.test.ts` — Expected: FAIL (`Cannot find module './vad'`).

- [ ] **Step 3: Implement**

```ts
// src/lib/voice/vad.ts

export interface VadConfig {
  /** RMS energy above which a frame counts as speech. Default 0.02. */
  threshold?: number;
  /** Keep "speaking" this long after energy drops, to avoid word-tail clipping. Default 200ms. */
  hangoverMs?: number;
  /** Frame cadence in ms (used to convert hangoverMs → frame count). Default 20. */
  frameMs?: number;
}

/**
 * Energy-threshold voice-activity detector with hangover.
 *
 * `process(pcm)` is called once per captured frame (≈20ms). Returns true while
 * the user is considered to be speaking. Once triggered, stays true for
 * `hangoverMs` after the energy falls below threshold. Stateful but pure
 * (no timers / Web Audio) — hangover is counted in frames.
 */
export class VoiceActivityDetector {
  private readonly threshold: number;
  private readonly hangoverFrames: number;
  private hangoverRemaining = 0;

  constructor(config: VadConfig = {}) {
    this.threshold = config.threshold ?? 0.02;
    const hangoverMs = config.hangoverMs ?? 200;
    const frameMs = config.frameMs ?? 20;
    this.hangoverFrames = Math.max(0, Math.round(hangoverMs / frameMs));
  }

  /** Root-mean-square energy of a PCM frame. */
  private static rms(pcm: Float32Array): number {
    if (pcm.length === 0) return 0;
    let sum = 0;
    for (let i = 0; i < pcm.length; i++) sum += pcm[i] * pcm[i];
    return Math.sqrt(sum / pcm.length);
  }

  process(pcm: Float32Array): boolean {
    if (VoiceActivityDetector.rms(pcm) >= this.threshold) {
      this.hangoverRemaining = this.hangoverFrames;
      return true;
    }
    if (this.hangoverRemaining > 0) {
      this.hangoverRemaining--;
      return true;
    }
    return false;
  }

  reset(): void {
    this.hangoverRemaining = 0;
  }
}
```

- [ ] **Step 4: Run to verify pass** — `npx vitest run src/lib/voice/vad.test.ts` → PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib/voice/vad.ts src/lib/voice/vad.test.ts
git commit -m "feat(zeb-351): energy+hangover voice-activity detector (VAD/DTX)"
```

---

## Task 2: `VoiceSender` frame gate

**Files:**
- Modify: `src/lib/voice/voice-sender.ts`
- Test: `src/lib/voice/voice-sender.test.ts` (extend existing)

Add an optional `frameGate` the sender consults per captured frame. Default (absent) preserves today's behavior (`{ send: true, ptt: true }`) so existing tests pass. The session controller supplies a gate that encodes mute/PTT/VAD.

- [ ] **Step 1: Write the failing test** (append to `src/lib/voice/voice-sender.test.ts`)

```ts
it('skips frames the gate rejects and uses the gate ptt flag', async () => {
  const invoke = vi.fn().mockResolvedValue(undefined);
  let onFrame!: (pcm: Float32Array) => void;
  const capture = {
    start: vi.fn(async (cb: (pcm: Float32Array) => void) => { onFrame = cb; }),
    stop: vi.fn(async () => {}),
    isActive: () => true,
  };
  const codec = {
    init: vi.fn(async () => {}), encode: () => new Uint8Array([1, 2, 3]),
    decode: () => new Float32Array(0), destroy: vi.fn(), codecType: 'opus' as const,
  };
  let allow = false;
  const sender = new VoiceSender({
    senderHash: new Uint8Array(16), communityId: 'c', channelId: 'ch',
    invoke, codec, capture: capture as never,
    frameGate: () => ({ send: allow, ptt: allow }),
  });
  await sender.start();
  onFrame(new Float32Array(320));          // gate rejects
  expect(invoke).not.toHaveBeenCalled();
  allow = true;
  onFrame(new Float32Array(320));          // gate accepts
  expect(invoke).toHaveBeenCalledTimes(1);
  const arg = invoke.mock.calls[0][1] as { payload: { frameBytes: number[] } };
  // ptt bit lives in header byte 0 bit 3 (0x08)
  expect((arg.payload.frameBytes[0] & 0x08) !== 0).toBe(true);
});
```

- [ ] **Step 2: Run to verify it fails** — `npx vitest run src/lib/voice/voice-sender.test.ts` → FAIL (`frameGate` ignored; invoke called for rejected frame).

- [ ] **Step 3: Implement** — add to `VoiceSenderConfig`:

```ts
  /**
   * Optional per-frame gate. Called with each captured PCM frame; returns
   * whether to transmit it and the PTT bit to stamp. Absent ⇒ always
   * { send: true, ptt: true } (legacy behavior). The session controller
   * supplies a gate encoding mute / PTT / VAD (DTX).
   */
  frameGate?: (pcm: Float32Array) => { send: boolean; ptt: boolean };
```

Change the capture callback in `start()`:

```ts
      await this.config.capture.start(
        (pcm) => {
          const decision = this.config.frameGate
            ? this.config.frameGate(pcm)
            : { send: true, ptt: true };
          if (decision.send) this.sendFrame(pcm, decision.ptt);
        },
        undefined,
        undefined,
        sr,
      );
```

(Leave `stop()`'s tail frames unchanged — they must always flush regardless of gate.)

- [ ] **Step 4: Run to verify pass** — `npx vitest run src/lib/voice/voice-sender.test.ts` → PASS (new + all existing).

- [ ] **Step 5: Commit**

```bash
git add src/lib/voice/voice-sender.ts src/lib/voice/voice-sender.test.ts
git commit -m "feat(zeb-351): per-frame send gate on VoiceSender (mute/PTT/VAD seam)"
```

---

## Task 3: Output mixer + playback worklet

**Files:**
- Create: `src/lib/voice/pcm-playback-processor.ts`
- Create: `src/lib/voice/voice-mixer.ts`
- Test: `src/lib/voice/voice-mixer.test.ts`

The receiver emits per-sender PCM via `onPlayFrame(senderHex, pcm)`. The mixer sums concurrent senders into one output stream, applies per-sender gain (0 = that sender muted) and a master gain (0 = **deafen**), soft-clips, and feeds a playback `AudioWorkletNode`. The worklet ring-buffers whole 20 ms frames and drains them across the 128-sample render quantum.

- [ ] **Step 1: Playback worklet** (no unit test — covered via mixer integration; mirrors `pcm-capture-processor.ts`)

```ts
// src/lib/voice/pcm-playback-processor.ts
/**
 * Playback worklet: receives mixed Float32 frames (20ms) on its port and
 * plays them out via a ring buffer, decoupling the 20ms producer cadence
 * from the 128-sample render quantum. Underrun → silence (no glitches).
 */
class PcmPlaybackProcessor extends AudioWorkletProcessor {
  private ring: Float32Array;
  private writeIdx = 0;
  private readIdx = 0;
  private available = 0;

  constructor() {
    super();
    // ~1s ring at 48k (worklet runs at the context rate; size generously).
    this.ring = new Float32Array(48000);
    this.port.onmessage = (e: MessageEvent) => {
      const frame = e.data as Float32Array;
      for (let i = 0; i < frame.length; i++) {
        if (this.available >= this.ring.length) break; // drop on overflow
        this.ring[this.writeIdx] = frame[i];
        this.writeIdx = (this.writeIdx + 1) % this.ring.length;
        this.available++;
      }
    };
  }

  process(_inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
    const out = outputs[0][0];
    for (let i = 0; i < out.length; i++) {
      if (this.available > 0) {
        out[i] = this.ring[this.readIdx];
        this.readIdx = (this.readIdx + 1) % this.ring.length;
        this.available--;
      } else {
        out[i] = 0;
      }
    }
    return true;
  }
}

registerProcessor('pcm-playback-processor', PcmPlaybackProcessor);
```

- [ ] **Step 2: Write the failing mixer test**

```ts
// src/lib/voice/voice-mixer.test.ts
import { describe, it, expect, vi } from 'vitest';
import { VoiceMixer, softClip, mixFrames } from './voice-mixer';

describe('mixFrames (pure)', () => {
  it('sums equal-length frames sample-wise', () => {
    const a = new Float32Array([0.1, 0.2, -0.1]);
    const b = new Float32Array([0.2, 0.2, 0.1]);
    const out = mixFrames([a, b], 3);
    expect(Array.from(out)).toEqual([
      softClip(0.3), softClip(0.4), softClip(0.0),
    ]);
  });
  it('returns silence for no inputs', () => {
    expect(Array.from(mixFrames([], 4))).toEqual([0, 0, 0, 0]);
  });
});

describe('softClip', () => {
  it('is identity in the linear region', () => {
    expect(softClip(0.5)).toBeCloseTo(0.5, 5);
  });
  it('compresses beyond ±1 without exceeding ±1', () => {
    expect(Math.abs(softClip(3))).toBeLessThanOrEqual(1);
    expect(Math.abs(softClip(-3))).toBeLessThanOrEqual(1);
  });
});

describe('VoiceMixer', () => {
  function mockCtx() {
    const node = { port: { postMessage: vi.fn() }, connect: vi.fn(), disconnect: vi.fn() };
    const ctx = {
      audioWorklet: { addModule: vi.fn().mockResolvedValue(undefined) },
      destination: {},
      close: vi.fn().mockResolvedValue(undefined),
      sampleRate: 48000,
      state: 'running',
      resume: vi.fn().mockResolvedValue(undefined),
    };
    return { ctx, node };
  }

  it('pushes a mixed frame to the worklet once per drain tick', async () => {
    const { ctx, node } = mockCtx();
    const mixer = new VoiceMixer({
      createContext: () => ctx as unknown as AudioContext,
      createWorkletNode: () => node as unknown as AudioWorkletNode,
    });
    await mixer.init();
    mixer.pushFrame('aa', new Float32Array([0.1, 0.1]));
    mixer.pushFrame('bb', new Float32Array([0.2, 0.2]));
    mixer.drain();                        // sum + emit
    expect(node.port.postMessage).toHaveBeenCalledTimes(1);
    const sent = node.port.postMessage.mock.calls[0][0] as Float32Array;
    expect(Array.from(sent)).toEqual([softClip(0.3), softClip(0.3)]);
  });

  it('deafen (master gain 0) emits silence', async () => {
    const { ctx, node } = mockCtx();
    const mixer = new VoiceMixer({
      createContext: () => ctx as unknown as AudioContext,
      createWorkletNode: () => node as unknown as AudioWorkletNode,
    });
    await mixer.init();
    mixer.setDeafened(true);
    mixer.pushFrame('aa', new Float32Array([0.5, 0.5]));
    mixer.drain();
    const sent = node.port.postMessage.mock.calls[0][0] as Float32Array;
    expect(Array.from(sent)).toEqual([0, 0]);
  });
});
```

- [ ] **Step 3: Run to verify it fails** — `npx vitest run src/lib/voice/voice-mixer.test.ts` → FAIL (no module).

- [ ] **Step 4: Implement the mixer**

```ts
// src/lib/voice/voice-mixer.ts

/** tanh-style soft clip: identity near 0, asymptotes to ±1. */
export function softClip(x: number): number {
  return Math.tanh(x);
}

/** Sum N equal-length frames sample-wise with soft-clip. Missing → silence. */
export function mixFrames(frames: Float32Array[], frameLen: number): Float32Array {
  const out = new Float32Array(frameLen);
  if (frames.length === 0) return out;
  for (let i = 0; i < frameLen; i++) {
    let acc = 0;
    for (const f of frames) acc += i < f.length ? f[i] : 0;
    out[i] = softClip(acc);
  }
  return out;
}

export interface VoiceMixerConfig {
  createContext?: () => AudioContext;
  createWorkletNode?: (ctx: AudioContext) => AudioWorkletNode;
}

/**
 * Mixes per-sender PCM frames into one playback stream.
 *
 * Producers call pushFrame(senderHex, pcm) (driven by VoiceReceiver.onPlayFrame).
 * drain() sums the latest frame per sender, applies per-sender + master gain,
 * soft-clips, and posts the result to the playback worklet. drain() is called
 * on a 20ms cadence by the session controller (or internally — see init()).
 */
export class VoiceMixer {
  private config: VoiceMixerConfig;
  private ctx: AudioContext | null = null;
  private node: AudioWorkletNode | null = null;
  private pending = new Map<string, Float32Array>();
  private senderGain = new Map<string, number>();
  private masterGain = 1;
  private frameLen = 320; // 16k source frames; resampling handled by context if needed

  constructor(config: VoiceMixerConfig = {}) {
    this.config = config;
  }

  async init(): Promise<void> {
    const ctx = this.config.createContext
      ? this.config.createContext()
      : new AudioContext();
    this.ctx = ctx;
    await ctx.audioWorklet.addModule(
      // Vite resolves the worklet URL at build time.
      new URL('./pcm-playback-processor.ts', import.meta.url),
    );
    const node = this.config.createWorkletNode
      ? this.config.createWorkletNode(ctx)
      : new AudioWorkletNode(ctx, 'pcm-playback-processor');
    node.connect(ctx.destination);
    this.node = node;
    if (ctx.state === 'suspended') await ctx.resume();
  }

  /** Latest frame wins per sender within a drain window. */
  pushFrame(senderHex: string, pcm: Float32Array | null): void {
    if (pcm) {
      this.pending.set(senderHex, pcm);
      this.frameLen = pcm.length;
    }
  }

  setSenderGain(senderHex: string, gain: number): void {
    this.senderGain.set(senderHex, gain);
  }

  setDeafened(deaf: boolean): void {
    this.masterGain = deaf ? 0 : 1;
  }

  /** Sum the pending per-sender frames, emit one mixed frame, clear pending. */
  drain(): void {
    if (!this.node) return;
    const frames: Float32Array[] = [];
    for (const [hex, pcm] of this.pending) {
      const g = this.senderGain.get(hex) ?? 1;
      const eff = g * this.masterGain;
      if (eff === 1) frames.push(pcm);
      else if (eff !== 0) {
        const scaled = new Float32Array(pcm.length);
        for (let i = 0; i < pcm.length; i++) scaled[i] = pcm[i] * eff;
        frames.push(scaled);
      }
    }
    const mixed = mixFrames(frames, this.frameLen);
    this.node.port.postMessage(mixed, [mixed.buffer]);
    this.pending.clear();
  }

  async destroy(): Promise<void> {
    this.node?.disconnect();
    this.node = null;
    if (this.ctx) { await this.ctx.close(); this.ctx = null; }
    this.pending.clear();
    this.senderGain.clear();
  }
}
```

> Note: the mixer posts one mixed frame per `drain()`. The session controller (Task 4) calls `drain()` on a 20 ms interval. `pushFrame` keeps the latest frame per sender per window — the receiver already paces at 20 ms per sender, so at most one frame/sender/window in steady state.

- [ ] **Step 5: Run to verify pass** — `npx vitest run src/lib/voice/voice-mixer.test.ts` → PASS.

- [ ] **Step 6: Commit**

```bash
git add src/lib/voice/pcm-playback-processor.ts src/lib/voice/voice-mixer.ts src/lib/voice/voice-mixer.test.ts
git commit -m "feat(zeb-351): N-stream output mixer + playback worklet (gain/deafen/soft-clip)"
```

---

## Task 4: Session controller core (`voice-session.ts`) — state machine + gate

**Files:**
- Create: `src/lib/voice-session.ts`
- Test: `src/lib/voice-session.test.ts`

The controller owns one active session. It composes VAD + mute + PTT into the sender's `frameGate`, exposes a Svelte-readable store of session state, and (Task 5) routes presence/frames. This task builds the lifecycle + gate logic with everything injectable; audio/IPC are mocked.

Gate truth table the controller implements (per frame):
- **muted** → `{ send: false }` (DTX silence; also Deafen forces muted).
- **PTT mode** → `{ send: pttHeld, ptt: true }` (VAD ignored).
- **open-mic** → `{ send: vad.process(pcm), ptt: vad.process(pcm) }` (single VAD call/ frame — cache it).

State: `'idle' | 'joining' | 'connected' | 'leaving'`. One session at a time: `join()` while not idle throws.

- [ ] **Step 1: Write the failing test**

```ts
// src/lib/voice-session.test.ts
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { get } from 'svelte/store';
import { VoiceSession } from './voice-session';

function deps() {
  const invoke = vi.fn().mockResolvedValue(undefined);
  const listeners = new Map<string, ((e: { payload: unknown }) => void)[]>();
  const listen = vi.fn(async (ev: string, h: (e: { payload: unknown }) => void) => {
    (listeners.get(ev) ?? listeners.set(ev, []).get(ev)!).push(h);
    return () => {};
  });
  const emit = (ev: string, payload: unknown) =>
    (listeners.get(ev) ?? []).forEach((h) => h({ payload }));
  // Capture the frameGate the controller hands to its sender.
  let capturedGate: ((pcm: Float32Array) => { send: boolean; ptt: boolean }) | undefined;
  const sender = {
    start: vi.fn(async () => {}), stop: vi.fn(async () => {}),
    __setGate: (g: never) => { capturedGate = g; },
  };
  const receiver = { init: vi.fn(async () => {}), destroy: vi.fn(), getActiveSenders: () => [], isSpeaking: () => false };
  const mixer = { init: vi.fn(async () => {}), pushFrame: vi.fn(), drain: vi.fn(), setDeafened: vi.fn(), destroy: vi.fn(async () => {}) };
  return {
    invoke, listen, emit,
    getGate: () => capturedGate,
    factories: {
      makeSender: (gate: never) => { sender.__setGate(gate); return sender as never; },
      makeReceiver: () => receiver as never,
      makeMixer: () => mixer as never,
    },
    sender, receiver, mixer,
  };
}

describe('VoiceSession lifecycle + gate', () => {
  let d: ReturnType<typeof deps>;
  beforeEach(() => { d = deps(); });

  function newSession() {
    return new VoiceSession({
      invoke: d.invoke, listen: d.listen,
      selfOwnerHex: 'aa'.repeat(16), selfDeviceHex: 'bb'.repeat(16),
      senderHash: new Uint8Array(16),
      ...d.factories,
    });
  }

  it('joins muted and is connected', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    expect(get(s.state).phase).toBe('connected');
    expect(get(s.state).muted).toBe(true);
    expect(d.invoke).toHaveBeenCalledWith('join_voice_channel', { communityId: 'comm', channelId: 'chan' });
  });

  it('rejects a second join while active', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    await expect(s.join('comm', 'chan2')).rejects.toThrow(/already/i);
  });

  it('muted gate never sends', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    const gate = d.getGate()!;
    expect(gate(new Float32Array(320)).send).toBe(false);
  });

  it('open-mic gate follows VAD energy', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    await s.setMuted(false);
    const gate = d.getGate()!;
    const loud = new Float32Array(320).fill(0.2) as unknown as Float32Array;
    const quiet = new Float32Array(320);
    expect(gate(loud).send).toBe(true);
    // hangover then silence
    for (let i = 0; i < 11; i++) gate(quiet);
    expect(gate(quiet).send).toBe(false);
  });

  it('PTT mode ignores VAD and follows hold', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    await s.setMuted(false);
    s.setPttMode(true);
    const gate = d.getGate()!;
    const quiet = new Float32Array(320);
    expect(gate(quiet).send).toBe(false);  // not held
    s.setPttHeld(true);
    expect(gate(quiet).send).toBe(true);   // held, VAD ignored
  });

  it('setMuted invokes set_voice_muted', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    await s.setMuted(false);
    expect(d.invoke).toHaveBeenCalledWith('set_voice_muted',
      { communityId: 'comm', channelId: 'chan', muted: false });
  });

  it('leave returns to idle and tears down', async () => {
    const s = newSession();
    await s.join('comm', 'chan');
    await s.leave();
    expect(get(s.state).phase).toBe('idle');
    expect(d.invoke).toHaveBeenCalledWith('leave_voice_channel', { communityId: 'comm', channelId: 'chan' });
    expect(d.mixer.destroy).toHaveBeenCalled();
    expect(d.receiver.destroy).toHaveBeenCalled();
  });
});
```

- [ ] **Step 2: Run to verify it fails** — `npx vitest run src/lib/voice-session.test.ts` → FAIL (no module).

- [ ] **Step 3: Implement** (lifecycle + gate; presence/frames wired in Task 5)

```ts
// src/lib/voice-session.ts
import { writable, type Readable } from 'svelte/store';
import { VoiceActivityDetector } from './voice/vad';
import { VoiceSender } from './voice/voice-sender';
import { VoiceReceiver } from './voice/voice-receiver';
import { VoiceMixer } from './voice/voice-mixer';
import { AudioCapture } from './voice/audio-capture';
import { OpusCodec } from './voice/opus-codec';
import type { CodecType } from './voice/voice-codec';

export type SessionPhase = 'idle' | 'joining' | 'connected' | 'leaving';

export interface RosterMember {
  ownerHex: string;     // 32 hex
  deviceHex: string;
  muted: boolean;
  speaking: boolean;    // derived (Task 5)
}

export interface VoiceSessionState {
  phase: SessionPhase;
  community: string | null;
  channel: string | null;
  muted: boolean;
  deafened: boolean;
  pttMode: boolean;
  roster: RosterMember[];
}

type Invoke = (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;
type Listen = (ev: string, h: (e: { payload: unknown }) => void) => Promise<() => void>;
type FrameGate = (pcm: Float32Array) => { send: boolean; ptt: boolean };

export interface VoiceSessionDeps {
  invoke: Invoke;
  listen: Listen;
  selfOwnerHex: string;
  selfDeviceHex: string;
  senderHash: Uint8Array;          // 16 bytes
  vadThreshold?: number;
  // Factories (injected in tests; real defaults below).
  makeSender?: (gate: FrameGate) => Pick<VoiceSender, 'start' | 'stop'>;
  makeReceiver?: () => Pick<VoiceReceiver, 'init' | 'destroy' | 'getActiveSenders' | 'isSpeaking'>;
  makeMixer?: () => Pick<VoiceMixer, 'init' | 'pushFrame' | 'drain' | 'setDeafened' | 'destroy'>;
}

const INITIAL: VoiceSessionState = {
  phase: 'idle', community: null, channel: null,
  muted: true, deafened: false, pttMode: false, roster: [],
};

export class VoiceSession {
  readonly state: Readable<VoiceSessionState>;
  private store = writable<VoiceSessionState>({ ...INITIAL });
  private deps: VoiceSessionDeps;

  private vad: VoiceActivityDetector;
  private sender: Pick<VoiceSender, 'start' | 'stop'> | null = null;
  private receiver: Pick<VoiceReceiver, 'init' | 'destroy' | 'getActiveSenders' | 'isSpeaking'> | null = null;
  private mixer: Pick<VoiceMixer, 'init' | 'pushFrame' | 'drain' | 'setDeafened' | 'destroy'> | null = null;

  private muted = true;
  private deafened = false;
  private pttMode = false;
  private pttHeld = false;
  private community: string | null = null;
  private channel: string | null = null;
  private unlisteners: (() => void)[] = [];
  private drainTimer: ReturnType<typeof setInterval> | null = null;

  constructor(deps: VoiceSessionDeps) {
    this.deps = deps;
    this.state = this.store;
    this.vad = new VoiceActivityDetector({ threshold: deps.vadThreshold ?? 0.02 });
  }

  private patch(p: Partial<VoiceSessionState>): void {
    this.store.update((s) => ({ ...s, ...p }));
  }

  /** The per-frame send decision (mute / PTT / VAD). */
  private gate: FrameGate = (pcm) => {
    if (this.muted || this.deafened) return { send: false, ptt: false };
    if (this.pttMode) return { send: this.pttHeld, ptt: true };
    const speaking = this.vad.process(pcm);
    return { send: speaking, ptt: speaking };
  };

  async join(community: string, channel: string): Promise<void> {
    let phase: SessionPhase = 'idle';
    this.store.update((s) => { phase = s.phase; return s; });
    if (phase !== 'idle') throw new Error('A voice session is already active');

    this.community = community;
    this.channel = channel;
    this.muted = true; this.deafened = false; this.pttMode = false; this.pttHeld = false;
    this.vad.reset();
    this.patch({ phase: 'joining', community, channel, muted: true, deafened: false, pttMode: false, roster: [] });

    // Backend join (spawns subscribers + presence publisher, starts muted).
    await this.deps.invoke('join_voice_channel', { communityId: community, channelId: channel });

    // Build engine pieces.
    this.mixer = this.deps.makeMixer ? this.deps.makeMixer() : new VoiceMixer();
    await this.mixer.init();

    this.receiver = this.deps.makeReceiver
      ? this.deps.makeReceiver()
      : new VoiceReceiver({
          listen: this.deps.listen,
          createCodec: (t: CodecType) => (t === 'opus' ? new OpusCodec() : new OpusCodec()),
          onPlayFrame: (hex, pcm) => this.mixer?.pushFrame(hex, pcm),
          ownSenderHex: this.deps.selfDeviceHex.slice(0, 32),
        });
    await this.receiver.init();

    this.sender = this.deps.makeSender
      ? this.deps.makeSender(this.gate)
      : new VoiceSender({
          senderHash: this.deps.senderHash, communityId: community, channelId: channel,
          invoke: this.deps.invoke, codec: new OpusCodec(), capture: new AudioCapture(),
          frameGate: this.gate,
        });
    await this.sender.start();   // capture starts; muted gate ⇒ nothing transmits

    // 20ms mixer drain.
    this.drainTimer = setInterval(() => this.mixer?.drain(), 20);

    await this.subscribePresence();   // Task 5

    this.patch({ phase: 'connected' });
  }

  async setMuted(muted: boolean): Promise<void> {
    this.muted = muted;
    if (muted) this.vad.reset();
    this.patch({ muted });
    if (this.community && this.channel) {
      await this.deps.invoke('set_voice_muted',
        { communityId: this.community, channelId: this.channel, muted });
    }
  }

  setPttMode(on: boolean): void { this.pttMode = on; this.patch({ pttMode: on }); }
  setPttHeld(held: boolean): void { this.pttHeld = held; }

  async setDeafened(deaf: boolean): Promise<void> {
    this.deafened = deaf;
    this.mixer?.setDeafened(deaf);
    this.patch({ deafened: deaf });
    if (deaf && !this.muted) await this.setMuted(true);   // deafen implies self-mute
  }

  async leave(): Promise<void> {
    this.patch({ phase: 'leaving' });
    if (this.drainTimer) { clearInterval(this.drainTimer); this.drainTimer = null; }
    for (const u of this.unlisteners) u();
    this.unlisteners = [];
    await this.sender?.stop().catch(() => {});
    this.receiver?.destroy();
    await this.mixer?.destroy().catch(() => {});
    this.sender = null; this.receiver = null; this.mixer = null;
    const community = this.community, channel = this.channel;
    this.community = null; this.channel = null;
    if (community && channel) {
      await this.deps.invoke('leave_voice_channel', { communityId: community, channelId: channel }).catch(() => {});
    }
    this.store.set({ ...INITIAL });
  }

  // Placeholder; implemented in Task 5.
  protected async subscribePresence(): Promise<void> {}
}
```

- [ ] **Step 4: Run to verify pass** — `npx vitest run src/lib/voice-session.test.ts` → PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib/voice-session.ts src/lib/voice-session.test.ts
git commit -m "feat(zeb-351): voice-session controller — lifecycle + mute/PTT/VAD gate + store"
```

---

## Task 5: Session controller — presence roster + speaking + member cards

**Files:**
- Modify: `src/lib/voice-session.ts`
- Modify: `src/lib/voice-session.test.ts`

Wire `voice-presence-changed` → roster store, derive `speaking` per member from `receiver.isSpeaking(deviceHex)` (self from local gate), and resolve display names/avatars via an injected `MemberCardService`-like resolver. **First read the exact `voice-presence-changed` emit site** in `event_loop.rs` to confirm JS field names (`community`/`channel`/`roster`, and each entry's `owner`/`device`/`muted` casing) — match them exactly.

- [ ] **Step 1: Write the failing test** (append)

```ts
it('updates roster from voice-presence-changed for the active channel', async () => {
  const s = newSession();
  await s.join('comm', 'chan');
  d.emit('voice-presence-changed', {
    community: 'comm', channel: 'chan',
    roster: [
      { owner: 'cc'.repeat(16), device: 'dd'.repeat(16), muted: false },
      { owner: 'ee'.repeat(16), device: 'ff'.repeat(16), muted: true },
    ],
  });
  const roster = get(s.state).roster;
  expect(roster.map((m) => m.ownerHex)).toEqual(['cc'.repeat(16), 'ee'.repeat(16)]);
  expect(roster[1].muted).toBe(true);
});

it('ignores presence for a different channel', async () => {
  const s = newSession();
  await s.join('comm', 'chan');
  d.emit('voice-presence-changed', { community: 'comm', channel: 'other', roster: [
    { owner: 'cc'.repeat(16), device: 'dd'.repeat(16), muted: false },
  ] });
  expect(get(s.state).roster).toHaveLength(0);
});
```

- [ ] **Step 2: Run to verify it fails** — `npx vitest run src/lib/voice-session.test.ts` → FAIL.

- [ ] **Step 3: Implement `subscribePresence` + roster derivation**

Replace the `subscribePresence` placeholder and add a resolver dep. Add to `VoiceSessionDeps`:

```ts
  /** Resolve an owner hex → { displayName, avatarUrl } for tiles (optional). */
  resolveCard?: (ownerHex: string) => { displayName?: string; avatarUrl?: string } | undefined;
  /** Subscribe/refresh member cards for visible roster owners (optional). */
  onRosterOwners?: (ownerHexes: string[]) => void;
```

```ts
  private lastRoster: { ownerHex: string; deviceHex: string; muted: boolean }[] = [];

  protected async subscribePresence(): Promise<void> {
    const un = await this.deps.listen('voice-presence-changed', (e) => {
      const p = e.payload as { community: string; channel: string;
        roster: { owner: string; device: string; muted: boolean }[] };
      if (p.community !== this.community || p.channel !== this.channel) return;
      this.lastRoster = p.roster.map((r) => ({
        ownerHex: r.owner, deviceHex: r.device, muted: r.muted,
      }));
      this.deps.onRosterOwners?.(this.lastRoster.map((r) => r.ownerHex));
      this.refreshRoster();
    });
    this.unlisteners.push(un);
  }

  /** Recompute roster view (speaking + card resolution). Call on presence + each drain. */
  private refreshRoster(): void {
    const roster: RosterMember[] = this.lastRoster.map((r) => {
      const isSelf = r.deviceHex.slice(0, 32) === this.deps.selfDeviceHex.slice(0, 32);
      const speaking = isSelf
        ? (!this.muted && !this.deafened && this.lastSelfSpeaking)
        : (this.receiver?.isSpeaking(r.deviceHex.slice(0, 32)) ?? false);
      const card = this.deps.resolveCard?.(r.ownerHex);
      return {
        ownerHex: r.ownerHex, deviceHex: r.deviceHex, muted: r.muted, speaking,
        ...(card?.displayName ? { displayName: card.displayName } : {}),
        ...(card?.avatarUrl ? { avatarUrl: card.avatarUrl } : {}),
      } as RosterMember;
    });
    this.patch({ roster });
  }

  private lastSelfSpeaking = false;
```

Extend `RosterMember` with optional `displayName?: string; avatarUrl?: string`. In `gate`, record self-speaking and trigger a light roster refresh when it flips:

```ts
  private gate: FrameGate = (pcm) => {
    if (this.muted || this.deafened) { this.setSelfSpeaking(false); return { send: false, ptt: false }; }
    if (this.pttMode) { this.setSelfSpeaking(this.pttHeld); return { send: this.pttHeld, ptt: true }; }
    const speaking = this.vad.process(pcm);
    this.setSelfSpeaking(speaking);
    return { send: speaking, ptt: speaking };
  };

  private setSelfSpeaking(v: boolean): void {
    if (v !== this.lastSelfSpeaking) { this.lastSelfSpeaking = v; this.refreshRoster(); }
  }
```

Also call `this.refreshRoster()` once at the end of the mixer-drain tick so remote `speaking` (from `receiver.isSpeaking`) stays live:

```ts
    this.drainTimer = setInterval(() => { this.mixer?.drain(); this.refreshRoster(); }, 20);
```

(Refresh is cheap — array map over ≤64 entries; `patch` only re-renders Svelte when values change. If profiling shows churn, throttle to e.g. every 5th tick — note in code.)

- [ ] **Step 4: Run to verify pass** — `npx vitest run src/lib/voice-session.test.ts` → PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib/voice-session.ts src/lib/voice-session.test.ts
git commit -m "feat(zeb-351): session roster from presence + speaking indicators + card resolution"
```

---

## Task 6: Rust — dynamic mute (publisher reads atomic) + `SetMuted` request

**Files:**
- Modify: `src-tauri/src/voice.rs`
- Modify: `src-tauri/src/voice_presence.rs`
- Test: `src-tauri/src/voice_presence.rs` (unit) — beacon reflects the atomic

- [ ] **Step 1: `voice.rs` — add the request variant + payload**

```rust
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetVoiceMutedPayload {
    pub community_id: String,
    pub channel_id: String,
    pub muted: bool,
}

// In enum VoiceChannelRequest, add:
    SetMuted {
        community_id: SpaceId,
        channel_id: ChannelId,
        muted: bool,
    },
```

- [ ] **Step 2: `voice_presence.rs` — publisher reads a shared mute flag**

Change `spawn_voice_presence_publisher` to take `muted: Arc<AtomicBool>` (insert the param after `joined_hlc`; update the call site in Task 7) and read it each tick:

```rust
    joined_hlc: Hlc,
    muted: Arc<AtomicBool>,
    interval: std::time::Duration,
    closing: Arc<AtomicBool>,
) -> JoinHandle<()> {
    // …
            let beacon = VoicePresenceBeacon {
                owner: self_owner.0,
                device: self_device,
                muted: muted.load(Ordering::SeqCst),   // was: muted: true
                joined_hlc: joined_hlc.clone(),
                seq,
                left: false,
            };
```

- [ ] **Step 3: Write a failing unit test** (in `voice_presence.rs` `mod tests`) — a helper that builds one beacon from a flag, asserting it tracks the atomic. Since the publisher loops on Zenoh, extract the beacon-construction into a tiny pure helper to test:

```rust
/// Build the heartbeat beacon for the current mute state (pure; unit-tested).
pub(crate) fn build_heartbeat_beacon(
    self_owner: OwnerAddr, self_device: [u8; 32], joined_hlc: &Hlc, seq: u64, muted: bool,
) -> VoicePresenceBeacon {
    VoicePresenceBeacon {
        owner: self_owner.0, device: self_device, muted,
        joined_hlc: joined_hlc.clone(), seq, left: false,
    }
}
```

```rust
#[test]
fn heartbeat_beacon_tracks_mute_flag() {
    let hlc = Hlc { wall_ms: 1, logical: 0, device_id: [0u8; 32] };
    let owner = OwnerAddr([7u8; 16]);
    assert!(build_heartbeat_beacon(owner, [1u8; 32], &hlc, 0, true).muted);
    assert!(!build_heartbeat_beacon(owner, [1u8; 32], &hlc, 1, false).muted);
}
```

Use `build_heartbeat_beacon` inside the publisher loop so the path is shared.

- [ ] **Step 4: Run to verify fail → implement → pass**

```bash
cd src-tauri
cargo nextest run --locked --workspace --all-targets --features test-fixtures -E 'test(voice_presence)'
```

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/voice.rs src-tauri/src/voice_presence.rs
git commit -m "feat(zeb-351): dynamic presence mute — publisher reads Arc<AtomicBool> + SetMuted request"
```

---

## Task 7: Rust — event-loop mute wiring + immediate beacon

**Files:**
- Modify: `src-tauri/src/event_loop.rs`
- Test: `src-tauri/tests/voice_presence_mute_integration.rs` (new)

- [ ] **Step 1: Store the mute flag per active channel.** In the voice state near `voice_keys` (re-grep), add a parallel map:

```rust
// (community, channel) → mute flag shared with that channel's presence publisher.
let mut voice_mute_flags: std::collections::HashMap<(SpaceId, ChannelId), Arc<AtomicBool>> =
    std::collections::HashMap::new();
```

In the `VoiceChannelRequest::Join` arm, create the flag (start **muted = true**), pass a clone into `spawn_voice_presence_publisher`, and store it:

```rust
let mute_flag = Arc::new(AtomicBool::new(true));
voice_mute_flags.insert((community_id, channel_id), mute_flag.clone());
// … spawn_voice_presence_publisher(…, joined_hlc, mute_flag, interval, closing) …
```

Remove the flag in the `Leave` arm.

- [ ] **Step 2: Handle `SetMuted`** (new match arm):

```rust
crate::voice::VoiceChannelRequest::SetMuted { community_id, channel_id, muted } => {
    if let Some(flag) = voice_mute_flags.get(&(community_id, channel_id)) {
        flag.store(muted, Ordering::SeqCst);
        // Immediate beacon so the roster updates without waiting for the 4s heartbeat.
        // Reuse the publisher's seal path (factor a one-shot publish helper in voice_presence.rs
        // that builds → signs → seals → put once); call it here best-effort.
        // (If the helper isn't worth it, the next ≤4s heartbeat carries the new state.)
    }
}
```

> Decision: implement the one-shot immediate beacon via a small `publish_presence_once(...)` helper in `voice_presence.rs` (build_heartbeat_beacon → sign → seal → `session.put`). It needs the session, topic, channel_key, signing_key, ids, joined_hlc, a `seq` (use a per-channel counter or `u64::MAX - 1` sentinel kept < tombstone's `u64::MAX`), and the new mute value. If threading the seq cleanly is awkward, ship the flag-flip alone (heartbeat ≤4 s) and file a follow-up — the heartbeat path is correct, just slower. Pick the immediate-beacon path if it fits without contorting the loop.

- [ ] **Step 3: Integration test** mirroring `voice_presence_two_engine_integration.rs`: engine A joins (muted=true), B subscribes presence; assert the first beacon B sees has `muted == true`; A's loop processes a `SetMuted{ muted:false }`; assert a subsequent beacon shows `muted == false`. Use logical/injected time consistent with the existing two-engine harness.

```bash
cd src-tauri
cargo nextest run --locked --all-targets --features test-fixtures -E 'test(voice_presence_mute)'
```

- [ ] **Step 4: Gate (Rust)**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --all-targets --features test-fixtures -E 'test(voice)'
```

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/event_loop.rs src-tauri/tests/voice_presence_mute_integration.rs
git commit -m "feat(zeb-351): event-loop mute flag per channel + SetMuted handling + integration test"
```

---

## Task 8: Rust — `set_voice_muted` IPC

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Add the command** (model on `leave_voice_channel`; re-grep for it):

```rust
#[tauri::command]
async fn set_voice_muted(
    payload: voice::SetVoiceMutedPayload,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let community_id = parse_space_id(&payload.community_id)?;   // reuse the same parser join uses
    let channel_id = parse_channel_id(&payload.channel_id)?;
    // NodeState is behind a std::sync::Mutex (sync lock(), NOT tokio .await).
    let tx = { state.lock().map_err(|e| format!("lock: {e}"))?.voice_channel_tx.clone() };
    tx.send(voice::VoiceChannelRequest::SetMuted { community_id, channel_id, muted: payload.muted })
        .await
        .map_err(|e| format!("voice channel tx: {e}"))?;
    Ok(())
}
```

(Match the exact id-parsing + tx-access pattern used by `join_voice_channel` in the current file — re-read it.)

- [ ] **Step 2: Register** in `tauri::generate_handler!` — add `set_voice_muted,` next to `leave_voice_channel,`.

- [ ] **Step 3: Gate**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo check --locked --all-targets --features test-fixtures
```

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-351): set_voice_muted IPC command + handler registration"
```

---

## Task 9: `VoiceChannelView` — join flow + control bar

**Files:**
- Rewrite: `src/lib/components/VoiceChannelView.svelte`
- Rewrite: `src/lib/components/__tests__/VoiceChannelView.test.ts`

Drive the view from a `VoiceSession`. Props: `{ session: VoiceSession; channelName: string; communityId: string; channelId: string }`. Subscribe to `session.state`. Render: header `🔊 {channelName} · {roster.length} here`; a **Join** button when `phase === 'idle'`; once connected, the control bar (Mute toggle, PTT mode toggle, Deafen toggle, Leave). **Join calls `session.join()` which connects muted**; show a prominent **Unmute** affordance.

- [ ] **Step 1: Write failing tests** (rewrite the file)

```ts
// src/lib/components/__tests__/VoiceChannelView.test.ts
import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import { writable } from 'svelte/store';
import VoiceChannelView from '../VoiceChannelView.svelte';

function fakeSession(state: object) {
  return {
    state: writable({ phase: 'idle', community: null, channel: null, muted: true,
      deafened: false, pttMode: false, roster: [], ...state }),
    join: vi.fn(async () => {}), leave: vi.fn(async () => {}),
    setMuted: vi.fn(async () => {}), setDeafened: vi.fn(async () => {}),
    setPttMode: vi.fn(), setPttHeld: vi.fn(),
  };
}

const base = { channelName: 'General', communityId: 'c', channelId: 'ch' };

it('renders header with participant count', () => {
  const session = fakeSession({ phase: 'connected', roster: [
    { ownerHex: 'a', deviceHex: 'a', muted: false, speaking: false },
    { ownerHex: 'b', deviceHex: 'b', muted: true, speaking: false },
  ] });
  render(VoiceChannelView, { props: { session: session as never, ...base } });
  expect(screen.getByText(/General/)).toBeInTheDocument();
  expect(screen.getByText(/2 here/)).toBeInTheDocument();
});

it('Join triggers session.join (connects muted)', async () => {
  const session = fakeSession({ phase: 'idle' });
  render(VoiceChannelView, { props: { session: session as never, ...base } });
  await fireEvent.click(screen.getByRole('button', { name: /join/i }));
  expect(session.join).toHaveBeenCalledWith('c', 'ch');
});

it('shows an unmute control when connected & muted', () => {
  const session = fakeSession({ phase: 'connected', muted: true });
  render(VoiceChannelView, { props: { session: session as never, ...base } });
  const btn = screen.getByRole('button', { name: /unmute|muted/i });
  expect(btn).toBeInTheDocument();
});

it('toggles mute via session.setMuted', async () => {
  const session = fakeSession({ phase: 'connected', muted: true });
  render(VoiceChannelView, { props: { session: session as never, ...base } });
  await fireEvent.click(screen.getByRole('button', { name: /unmute|muted/i }));
  expect(session.setMuted).toHaveBeenCalledWith(false);
});

it('Leave triggers session.leave', async () => {
  const session = fakeSession({ phase: 'connected' });
  render(VoiceChannelView, { props: { session: session as never, ...base } });
  await fireEvent.click(screen.getByRole('button', { name: /leave/i }));
  expect(session.leave).toHaveBeenCalled();
});
```

- [ ] **Step 2: Run to verify fail** — `npx vitest run src/lib/components/__tests__/VoiceChannelView.test.ts` → FAIL.

- [ ] **Step 3: Implement** (Svelte 5 runes; control bar + join flow; tiles come in Task 10). Use the `$props`/`$derived`/store-auto-subscription patterns from existing components (read a sibling like `ChannelMessageFeed.svelte` for house style — error toasts via `toastStore`, button classes).

```svelte
<script lang="ts">
  import type { VoiceSession } from '../../voice-session';
  let { session, channelName, communityId, channelId }:
    { session: VoiceSession; channelName: string; communityId: string; channelId: string } = $props();

  const state = session.state;            // Readable<VoiceSessionState>; use $state in markup
  let joining = $state(false);
  let error = $state<string | null>(null);

  async function onJoin() {
    joining = true; error = null;
    try { await session.join(communityId, channelId); }
    catch (e) { error = e instanceof Error ? e.message : String(e); }
    finally { joining = false; }
  }
  const toggleMute = () => session.setMuted(!$state.muted);
  const toggleDeafen = () => session.setDeafened(!$state.deafened);
  const togglePtt = () => session.setPttMode(!$state.pttMode);
  const onLeave = () => session.leave();
</script>

<div class="voice-view">
  <header class="voice-header">🔊 {channelName} · {$state.roster.length} here</header>

  {#if error}<div class="voice-error" role="alert">{error}</div>{/if}

  {#if $state.phase === 'idle'}
    <div class="voice-join-pane">
      <button class="btn-primary" onclick={onJoin} disabled={joining}>
        {joining ? 'Joining…' : 'Join Voice'}
      </button>
      <p class="hint">You'll join muted — unmute when you're ready.</p>
    </div>
  {:else}
    <!-- roster grid/list slot — Task 10 -->
    <div class="voice-stage" data-testid="voice-stage"></div>

    <div class="voice-controls">
      <button class:active={!$state.muted}
              aria-pressed={!$state.muted}
              onclick={toggleMute}
              aria-label={$state.muted ? 'Unmute' : 'Mute'}>
        {$state.muted ? '🔇 Muted' : '🎙 Live'}
      </button>
      <button class:active={$state.pttMode} aria-pressed={$state.pttMode}
              onclick={togglePtt} aria-label="Push to talk mode">PTT</button>
      <button class:active={$state.deafened} aria-pressed={$state.deafened}
              onclick={toggleDeafen} aria-label="Deafen">
        {$state.deafened ? '🔕 Deafened' : '🔈 Deafen'}
      </button>
      <button class="btn-danger" onclick={onLeave} aria-label="Leave voice">Leave</button>
    </div>
  {/if}
</div>
```

> `$state` here is the auto-subscribed store value (Svelte's `$store` syntax). If the project lints against shadowing the `$state` rune name, alias the import: `const vs = session.state;` and use `$vs`. Read a sibling component to confirm the convention and match it.

- [ ] **Step 4: Run to verify pass** — `npx vitest run src/lib/components/__tests__/VoiceChannelView.test.ts` → PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/VoiceChannelView.svelte src/lib/components/__tests__/VoiceChannelView.test.ts
git commit -m "feat(zeb-351): VoiceChannelView join flow + mute/PTT/deafen/leave control bar"
```

---

## Task 10: `VoiceChannelView` — hybrid grid↔list tiles

**Files:**
- Modify: `src/lib/components/VoiceChannelView.svelte`
- Modify: `src/lib/components/__tests__/VoiceChannelView.test.ts`

Render the roster: a **stage grid** of avatar tiles (avatar, name, speaking ring, mute glyph) when `roster.length <= GRID_MAX` (12), auto-collapsing to a **compact list** beyond. Speaking ring = `member.speaking`; mute glyph = `member.muted`. Avatar/name from `member.avatarUrl`/`member.displayName` (fallbacks: a default avatar + a shortened owner hex).

- [ ] **Step 1: Write failing tests** (append)

```ts
import { writable as w } from 'svelte/store';
function roster(n: number) {
  return Array.from({ length: n }, (_, i) => ({
    ownerHex: String(i).padStart(2, '0').repeat(16), deviceHex: String(i).repeat(16),
    muted: false, speaking: i === 0, displayName: `User${i}`,
  }));
}

it('renders a grid at/below 12 participants', () => {
  const session = fakeSession({ phase: 'connected', roster: roster(12) });
  render(VoiceChannelView, { props: { session: session as never, ...base } });
  expect(screen.getByTestId('voice-grid')).toBeInTheDocument();
  expect(screen.queryByTestId('voice-list')).not.toBeInTheDocument();
  expect(screen.getAllByTestId('voice-tile')).toHaveLength(12);
});

it('collapses to a compact list past 12 participants', () => {
  const session = fakeSession({ phase: 'connected', roster: roster(13) });
  render(VoiceChannelView, { props: { session: session as never, ...base } });
  expect(screen.getByTestId('voice-list')).toBeInTheDocument();
  expect(screen.queryByTestId('voice-grid')).not.toBeInTheDocument();
  expect(screen.getAllByTestId('voice-list-row')).toHaveLength(13);
});

it('shows a speaking ring for speaking members', () => {
  const session = fakeSession({ phase: 'connected', roster: roster(2) });
  render(VoiceChannelView, { props: { session: session as never, ...base } });
  const tiles = screen.getAllByTestId('voice-tile');
  expect(tiles[0].className).toMatch(/speaking/);
  expect(tiles[1].className).not.toMatch(/speaking/);
});
```

- [ ] **Step 2: Run to verify fail** → implement the stage region:

```svelte
<script lang="ts">
  const GRID_MAX = 12;
  // … existing …
  function label(m: { displayName?: string; ownerHex: string }) {
    return m.displayName ?? `${m.ownerHex.slice(0, 6)}…`;
  }
</script>

<!-- replace the empty .voice-stage with: -->
{#if $state.roster.length <= GRID_MAX}
  <div class="voice-grid" data-testid="voice-grid">
    {#each $state.roster as m (m.deviceHex)}
      <div class="voice-tile" class:speaking={m.speaking} data-testid="voice-tile">
        {#if m.avatarUrl}<img class="avatar" src={m.avatarUrl} alt="" />
        {:else}<div class="avatar avatar-fallback" aria-hidden="true"></div>{/if}
        <span class="name">{label(m)}</span>
        {#if m.muted}<span class="mute-glyph" aria-label="muted">🔇</span>{/if}
      </div>
    {/each}
  </div>
{:else}
  <ul class="voice-list" data-testid="voice-list">
    {#each $state.roster as m (m.deviceHex)}
      <li class="voice-list-row" class:speaking={m.speaking} data-testid="voice-list-row">
        <span class="dot" class:on={m.speaking}></span>
        <span class="name">{label(m)}</span>
        {#if m.muted}<span class="mute-glyph" aria-label="muted">🔇</span>{/if}
      </li>
    {/each}
  </ul>
{/if}
```

Add CSS for `.voice-grid` (responsive tile grid), `.voice-tile.speaking` (accent ring via `box-shadow`/`outline`), `.voice-list`, fallback avatar — match the palette/vars used in sibling components.

- [ ] **Step 3: Run to verify pass** — `npx vitest run src/lib/components/__tests__/VoiceChannelView.test.ts` → PASS.

- [ ] **Step 4: Commit**

```bash
git add src/lib/components/VoiceChannelView.svelte src/lib/components/__tests__/VoiceChannelView.test.ts
git commit -m "feat(zeb-351): hybrid grid↔list voice roster tiles with speaking ring + mute glyph"
```

---

## Task 11: `CommunityView` wiring + session lifecycle

**Files:**
- Modify: `src/lib/components/CommunityView.svelte`
- Test: extend its test if one exists (else a focused render test)

Construct/obtain one `VoiceSession` (e.g. a module-level singleton in `voice-session.ts`, `getVoiceSession(adapter, selfIds)`, since only one session is allowed app-wide) and pass it + `communityId`/`channelId` into `VoiceChannelView`. Ensure switching away from a voice channel (or unmounting) calls `session.leave()` if the active session matches.

- [ ] **Step 1: Read the current routing** (re-grep `VoiceChannelView` in `CommunityView.svelte`) — V1 renders `<VoiceChannelView channelName={activeChannel.name} />`.

- [ ] **Step 2: Provide a session singleton** in `voice-session.ts`:

```ts
let _singleton: VoiceSession | null = null;
export function getVoiceSession(deps: VoiceSessionDeps): VoiceSession {
  if (!_singleton) _singleton = new VoiceSession(deps);
  return _singleton;
}
```

(Wire `deps` from the app's existing Tauri adapter + the self owner/device hex + sender hash. Read how `App.svelte`/`CommunityView` already obtain self identity + the adapter, and reuse that source — do not re-derive.)

- [ ] **Step 3: Update routing**:

```svelte
{#if activeChannel.kind === 'voice'}
  <VoiceChannelView
    session={voiceSession}
    channelName={activeChannel.name}
    communityId={community.id}
    channelId={activeChannel.channelId} />
{:else}
  <ChannelMessageFeed … />
{/if}
```

Add an `$effect` (or the existing channel-change hook) that, when the active channel changes away from a voice channel the session is connected to, calls `voiceSession.leave()`. Confirm `community.id`/`activeChannel.channelId` are the hex strings the IPCs expect (the same values V1/V2 already pass to `join_voice_channel`).

- [ ] **Step 4: Gate (frontend)**

```bash
npx tsc --noEmit
npx vitest run
```

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/CommunityView.svelte src/lib/voice-session.ts
git commit -m "feat(zeb-351): wire VoiceChannelView to a singleton VoiceSession + leave on channel switch"
```

---

## Task 12: Final gate sweep + manual checklist + PR

**Files:**
- Modify: append a voice-talk smoke section to the manual checklist (ZEB-224 doc, if tracked in-repo; else note in the PR body)

- [ ] **Step 1: Full local gate**

```bash
cd <repo-root>
npx tsc --noEmit && npx vitest run
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --all-targets --features test-fixtures
cargo check --locked --all-targets --features test-fixtures   # MSRV proxy
```

Expected: all green (modulo the 6 known iroh/zenoh loopback orphan flakes).

- [ ] **Step 2: Append the V3 manual smoke** (two real peers): create a Voice channel; both Join (land muted); each unmutes and confirms the other is heard; speaking rings light for the active speaker; mute glyph appears on the muted peer; PTT mode sends only while held; Deafen silences inbound and self-mutes; Leave clears the roster within ~12 s on the other peer.

- [ ] **Step 3: Push + open PR**

```bash
git push -u origin zeb-351-voice-v3-talk
gh pr create --title "ZEB-351 Voice V3: talk (session controller, VAD/mute/PTT, N-stream mix, VoiceChannelView)" --body "$(cat <<'EOF'
## Summary
Wires the existing browser voice engine into community voice channels — members can talk and hear each other.

- VAD (energy + 200ms hangover) → DTX send gate on `VoiceSender` (new `frameGate`).
- Output mixer + playback worklet sum the already-N-stream `VoiceReceiver`'s per-sender PCM (per-sender gain, master gain = deafen, soft-clip).
- `voice-session.ts` controller/store: one session at a time; mute/PTT/deafen; roster from `voice-presence-changed`; member-card resolution; speaking indicators.
- Dynamic mute: presence publisher reads `Arc<AtomicBool>`; new `set_voice_muted` IPC + `VoiceChannelRequest::SetMuted` (+ immediate beacon).
- `VoiceChannelView` rewrite: join-muted flow, control bar (Mute/PTT/Deafen/Leave), hybrid grid↔list roster tiles (speaking ring + mute glyph).

Spec: `docs/specs/2026-05-31-voice-comms-design.md` §V3. Plan: `docs/plans/2026-06-01-zeb-351-voice-v3-talk.md`. Parent ZEB-348; builds on ZEB-350 (V2). Next: ZEB-352 (DM calls) reuses this controller.

## Test plan
- [ ] `cargo fmt --check` / `clippy -D warnings` / `nextest` / MSRV `cargo check`
- [ ] `npx tsc --noEmit` / `npx vitest run`
- [ ] VAD gating, mute/PTT overrides, N-stream mix, session state machine (unit)
- [ ] VoiceChannelView grid↔list threshold + join-muted (component)
- [ ] Two-peer manual smoke (talk/hear, speaking ring, mute glyph, PTT, deafen, leave)
EOF
)"
```

- [ ] **Step 4: Hand off to the autonomous bot-review loop** (CodeRabbit / Cursor Bugbot / CodeAnt / Qodo; never trigger Greptile). Bundle fixes per round; gate freshness/audio-core edits locally before pushing; do NOT merge (Jake's gate); Pushover at ready-to-merge.

---

## Self-review checklist (controller, before dispatching Task 1)

- **Spec coverage:** session controller (T4–5), VAD (T1), mute/PTT (T2/T4), N-stream mix (T3 — receiver already N-stream), VoiceChannelView hybrid + join-muted (T9–10), speaking indicators (T5/T10), Deafen (T4/T9), one-session-at-a-time (T4). ✔
- **Type consistency:** `frameGate(pcm) => { send, ptt }` (T2) matches `VoiceSession.gate` (T4). `onPlayFrame(senderHex, pcm)` → `mixer.pushFrame` (T3/T4). `voice-presence-changed` field casing — VERIFY at the emit site before T5/T7. `VoiceChannelRequest::SetMuted` shape identical across voice.rs (T6), event_loop (T7), lib.rs (T8). ✔
- **Ambiguity flagged inline:** immediate-beacon-vs-heartbeat (T7), `$state` rune-name shadowing (T9), self-identity/adapter source (T11) — each says "read the existing site and match."
