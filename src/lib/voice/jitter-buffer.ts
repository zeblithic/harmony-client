/**
 * Fixed-delay jitter buffer for voice playback.
 *
 * Sits between "received decoded PCM frame" and "schedule for playback."
 * One instance per active sender. Smooths out network jitter by holding
 * `depth` frames before beginning playback.
 *
 * Sequence numbers are u16 (0–65535) and wrap around.
 */
export class JitterBuffer {
  private readonly depth: number;
  private readonly frameMs: number;
  private slots: (Float32Array | null)[];
  private playSeq: number;
  private fillCount: number;
  /** Whether playSeq has been seeded from the first received frame. */
  private seeded: boolean;

  /**
   * @param depth   Number of slots (and fill-period duration in frames).
   * @param frameMs Duration of each frame in milliseconds (informational).
   */
  constructor(depth: number, frameMs: number) {
    this.depth = depth;
    this.frameMs = frameMs;
    this.slots = new Array<Float32Array | null>(depth).fill(null);
    this.playSeq = 0;
    this.fillCount = 0;
    this.seeded = false;
  }

  /**
   * Insert a decoded PCM frame at the given u16 sequence number.
   *
   * Late frames (seq is behind playSeq in u16 modular sense) are dropped.
   * Frames too far ahead (>= depth slots away) are also dropped to prevent
   * overwriting a slot that hasn't been played yet.
   */
  insert(seq: number, pcm: Float32Array): void {
    // Seed playSeq from the first received frame so mid-stream joins work.
    if (!this.seeded) {
      this.playSeq = seq;
      this.seeded = true;
    }

    // Modular distance: positive means seq is ahead of playSeq
    const dist = (seq - this.playSeq + 0x10000) & 0xffff;

    // dist >= 0x8000 means seq is behind playSeq (late frame) — drop
    if (dist >= 0x8000) return;

    // Too far ahead — would overwrite unplayed slots — drop
    if (dist >= this.depth) return;

    this.slots[seq % this.depth] = pcm;
  }

  /**
   * Advance the playhead by one frame.
   *
   * During the fill period (first `depth` calls), always returns null.
   * After fill, returns the PCM frame at the current playSeq slot (or null
   * if missing — caller should apply concealment / play silence), then
   * increments playSeq with u16 wraparound.
   */
  advance(): Float32Array | null {
    // Don't progress fill until we have a stream anchor.
    if (!this.seeded) return null;

    if (this.fillCount < this.depth) {
      // Still in fill period — let frames accumulate without consuming them.
      this.fillCount++;
      return null;
    }

    const slot = this.playSeq % this.depth;
    const frame = this.slots[slot];
    this.slots[slot] = null;
    this.playSeq = (this.playSeq + 1) & 0xffff;
    return frame ?? null;
  }

  /** Whether the fill period has elapsed and playback has begun. */
  isReady(): boolean {
    return this.seeded && this.fillCount >= this.depth;
  }

  /** Clear all state — fill period resets to the beginning. */
  reset(): void {
    this.slots = new Array<Float32Array | null>(this.depth).fill(null);
    this.playSeq = 0;
    this.fillCount = 0;
    this.seeded = false;
  }
}
