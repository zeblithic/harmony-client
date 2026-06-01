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
