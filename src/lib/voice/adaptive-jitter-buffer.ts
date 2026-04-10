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

/**
 * EWMA smoothing factor for jitter estimation.
 * Smaller divisor = faster convergence (more reactive to recent jitter).
 * RFC 3550 uses 16; we use 4 for faster adaptation to bursts.
 */
const EWMA_DIVISOR = 4;

export class AdaptiveJitterBuffer {
  private readonly minDepth: number;
  private readonly maxDepth: number;
  private readonly frameMs: number;
  private readonly now: () => number;

  /**
   * Sparse frame store keyed by u16 sequence number.
   * Using a Map avoids ring-buffer slot collision when frames arrive
   * out of order with gaps (e.g., seq 0 and seq 2 both buffered while
   * seq 1 is missing).
   */
  private frames: Map<number, Float32Array>;
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
    this.frames = new Map();
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
      // EWMA jitter: jitter += (deviation - jitter) / EWMA_DIVISOR
      this.jitter += (deviation - this.jitter) / EWMA_DIVISOR;
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

    // Late frame (seq is behind playSeq in modular sense) — drop
    if (dist >= 0x8000) return;

    // Too far ahead — drop to bound memory usage
    if (dist >= this.maxDepth * 2) return;

    this.frames.set(seq, pcm);
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

    const seq = this.playSeq;
    const frame = this.frames.get(seq) ?? null;
    this.frames.delete(seq);
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

    return frame;
  }

  /** Whether the fill period has elapsed and playback has begun. */
  isReady(): boolean {
    return this.seeded && this.fillCount >= this.depth;
  }

  /** Clear all state — fill period resets, depth returns to minimum. */
  reset(): void {
    this.depth = this.minDepth;
    this.frames = new Map();
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
      this.depth = target;
    }
  }

  /** Shrink buffer by one slot if jitter allows. */
  private tryShrink(): void {
    const target = Math.max(
      this.minDepth,
      Math.min(this.maxDepth, Math.ceil((this.jitter * 3) / this.frameMs)),
    );

    if (target < this.depth) {
      // Shrink by 1 at a time to be conservative
      this.depth = this.depth - 1;
      this.cleanRun = 0; // Reset counter after shrink
    }
  }
}
