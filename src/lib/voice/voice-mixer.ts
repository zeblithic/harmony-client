// src/lib/voice/voice-mixer.ts
//
// N-stream output mixer. Collects per-sender decoded PCM frames (driven by
// VoiceReceiver.onPlayFrame), applies per-sender gain (0 = sender muted) and a
// master gain (0 = deafen), soft-clips the sum, and feeds the playback worklet.
//
// The class accepts optional factory functions for AudioContext and
// AudioWorkletNode so that tests can inject mocks without touching jsdom's
// (non-existent) Web Audio implementation — mirroring audio-capture.ts.

import { loadWorkletModule } from './load-worklet-module';
// `?raw` inlines the plain-JS worklet source as a string; loadWorkletModule
// loads it from a same-origin blob: URL (ZEB-575).
import playbackProcessorSource from './pcm-playback-processor.js?raw';

/** Below this magnitude the limiter is transparent (pure identity). */
const SOFT_CLIP_KNEE = 0.8;

/**
 * Output AudioContext sample rate. MUST match the capture/codec pipeline:
 * audio-capture.ts opens its context at 16 kHz and the Opus path emits 20 ms /
 * 320-sample frames. A bare `new AudioContext()` picks the hardware rate
 * (commonly 48 kHz); the playback worklet would then drain ~960 samples per
 * 20 ms while only 320 are produced — a 3:1 underrun heard as rhythmic
 * dropouts. Pinning to 16 kHz keeps producer and consumer rates equal.
 */
const PLAYBACK_SAMPLE_RATE = 16000;

/**
 * Soft-clip limiter for summed audio.
 *
 * Transparent (identity) for |x| <= SOFT_CLIP_KNEE, so a single speaker or a
 * quiet mix passes through untouched. Above the knee it smoothly compresses
 * only the overshoot via tanh, asymptotically bounded to ±1 — graceful
 * limiting instead of a hard clamp's audible crackle when several speakers
 * overlap. The mapping is C¹-continuous at the knee (tanh'(0) = 1 matches the
 * identity slope), so there is no kink, and the output never reaches ±1 for a
 * finite input.
 */
export function softClip(x: number): number {
  const mag = Math.abs(x);
  if (mag <= SOFT_CLIP_KNEE) return x;
  const sign = x < 0 ? -1 : 1;
  const over = mag - SOFT_CLIP_KNEE; // amount past the knee
  const range = 1 - SOFT_CLIP_KNEE; // headroom up to the ±1 ceiling
  return sign * (SOFT_CLIP_KNEE + range * Math.tanh(over / range));
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
  /**
   * ZEB-359: preferred playback device provider, read at init. Absent /
   * returning null ⇒ system default. Only honored where the platform exposes
   * `AudioContext.setSinkId` (Chromium/WebView2; WKWebView does not).
   */
  outputDeviceId?: () => string | null;
}

/** `setSinkId` is a Chromium extension not present in every TS dom lib. */
type SinkCapableContext = AudioContext & {
  setSinkId?: (id: string) => Promise<void>;
};

/**
 * Mixes per-sender PCM frames into one playback stream.
 *
 * Producers call pushFrame(senderHex, pcm) (driven by VoiceReceiver.onPlayFrame).
 * drain() sums the latest frame per sender, applies per-sender + master gain,
 * soft-clips, and posts the result to the playback worklet. drain() is called
 * on a 20ms cadence by the session controller.
 */
export class VoiceMixer {
  private config: VoiceMixerConfig;
  private ctx: AudioContext | null = null;
  private node: AudioWorkletNode | null = null;
  private pending = new Map<string, Float32Array>();
  private senderGain = new Map<string, number>();
  private masterGain = 1;

  constructor(config: VoiceMixerConfig = {}) {
    this.config = config;
  }

  async init(): Promise<void> {
    const ctx = this.config.createContext
      ? this.config.createContext()
      : new AudioContext({ sampleRate: PLAYBACK_SAMPLE_RATE });
    this.ctx = ctx;
    // Mirror audio-capture.ts's same-origin worklet load mechanism exactly (ZEB-575).
    if (!this.config.createWorkletNode) {
      await loadWorkletModule(ctx, playbackProcessorSource);
    }
    const node = this.config.createWorkletNode
      ? this.config.createWorkletNode(ctx)
      : new AudioWorkletNode(ctx, 'pcm-playback-processor');
    node.connect(ctx.destination);
    this.node = node;
    if (ctx.state === 'suspended') await ctx.resume();
    // ZEB-359: route to the preferred output where the platform allows it. A
    // fresh context already plays to the system default, so null needs no call.
    const preferredOut = this.config.outputDeviceId?.() ?? null;
    if (preferredOut) await this.applySink(preferredOut);
  }

  /**
   * ZEB-359: live output-device switch. `null` resets to the system default
   * (empty sinkId per spec). No-op before init or where `setSinkId` is
   * unsupported; a rejection (device unplugged mid-call) falls back to the
   * current sink rather than failing playback.
   */
  async setOutputDevice(id: string | null): Promise<void> {
    await this.applySink(id ?? '');
  }

  private async applySink(id: string): Promise<void> {
    const ctx = this.ctx as SinkCapableContext | null;
    if (!ctx?.setSinkId) return;
    try {
      await ctx.setSinkId(id);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      console.warn('voice output setSinkId failed; using current sink:', msg);
    }
  }

  pushFrame(senderHex: string, pcm: Float32Array | null): void {
    if (pcm) {
      // Copy: the source may be a view into the jitter buffer's internal storage,
      // which can be overwritten before the next drain() reads it.
      this.pending.set(senderHex, new Float32Array(pcm));
    }
  }

  setSenderGain(senderHex: string, gain: number): void {
    this.senderGain.set(senderHex, gain);
  }

  setDeafened(deaf: boolean): void {
    this.masterGain = deaf ? 0 : 1;
  }

  drain(): void {
    if (!this.node) return;
    // Output length = the longest pending frame, recomputed every drain. With
    // mixed frame sizes (codec2 160 vs opus 320) a single global length taken
    // from whichever frame arrived last would truncate the others; taking the
    // max and letting mixFrames zero-pad the shorter streams is lossless.
    // Computed over `pending` (not the post-gain `frames`) so a deafened drain
    // (all gains 0 ⇒ no frames) still emits a correctly-sized silence frame.
    let frameLen = 0;
    for (const pcm of this.pending.values()) {
      if (pcm.length > frameLen) frameLen = pcm.length;
    }
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
    const mixed = mixFrames(frames, frameLen);
    this.node.port.postMessage(mixed, [mixed.buffer]);
    this.pending.clear();
  }

  async destroy(): Promise<void> {
    this.node?.disconnect();
    this.node = null;
    if (this.ctx) {
      await this.ctx.close();
      this.ctx = null;
    }
    this.pending.clear();
    this.senderGain.clear();
  }
}
