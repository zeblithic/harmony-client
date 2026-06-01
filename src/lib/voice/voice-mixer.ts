// src/lib/voice/voice-mixer.ts
//
// N-stream output mixer. Collects per-sender decoded PCM frames (driven by
// VoiceReceiver.onPlayFrame), applies per-sender gain (0 = sender muted) and a
// master gain (0 = deafen), soft-clips the sum, and feeds the playback worklet.
//
// The class accepts optional factory functions for AudioContext and
// AudioWorkletNode so that tests can inject mocks without touching jsdom's
// (non-existent) Web Audio implementation — mirroring audio-capture.ts.

/**
 * Soft clip: identity inside the [-1, 1] linear region, clamped to ±1 beyond.
 *
 * A function that is the identity on [-1, 1] yet never exceeds ±1 is, by
 * construction, a clamp at the boundary — any smooth knee would either break
 * identity below ±1 or overshoot ±1 above it. We round through float32
 * (Math.fround) so the value matches what mixFrames stores in its
 * Float32Array output, keeping the mix bit-exact.
 */
export function softClip(x: number): number {
  if (x > 1) return 1;
  if (x < -1) return -1;
  return Math.fround(x);
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
 * on a 20ms cadence by the session controller.
 */
export class VoiceMixer {
  private config: VoiceMixerConfig;
  private ctx: AudioContext | null = null;
  private node: AudioWorkletNode | null = null;
  private pending = new Map<string, Float32Array>();
  private senderGain = new Map<string, number>();
  private masterGain = 1;
  private frameLen = 320;

  constructor(config: VoiceMixerConfig = {}) {
    this.config = config;
  }

  async init(): Promise<void> {
    const ctx = this.config.createContext ? this.config.createContext() : new AudioContext();
    this.ctx = ctx;
    // Mirror audio-capture.ts's addModule(worklet URL) mechanism exactly.
    if (!this.config.createWorkletNode) {
      await ctx.audioWorklet.addModule(
        new URL('./pcm-playback-processor.ts', import.meta.url).href,
      );
    }
    const node = this.config.createWorkletNode
      ? this.config.createWorkletNode(ctx)
      : new AudioWorkletNode(ctx, 'pcm-playback-processor');
    node.connect(ctx.destination);
    this.node = node;
    if (ctx.state === 'suspended') await ctx.resume();
  }

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
    if (this.ctx) {
      await this.ctx.close();
      this.ctx = null;
    }
    this.pending.clear();
    this.senderGain.clear();
  }
}
