import { describe, it, expect, beforeEach } from 'vitest';
import { JitterBuffer } from './jitter-buffer';

describe('JitterBuffer', () => {
  const DEPTH = 4;
  const FRAME_MS = 20;

  let buf: JitterBuffer;

  beforeEach(() => {
    buf = new JitterBuffer(DEPTH, FRAME_MS);
  });

  it('is not ready before buffer fill period', () => {
    // insert some frames but do not advance enough times
    buf.insert(0, new Float32Array(160));
    buf.insert(1, new Float32Array(160));
    // advance fewer than DEPTH times — still in fill period
    buf.advance();
    buf.advance();
    expect(buf.isReady()).toBe(false);
  });

  it('becomes ready after fill period elapses', () => {
    // advance DEPTH times to exhaust fill period
    for (let i = 0; i < DEPTH; i++) {
      buf.advance();
    }
    expect(buf.isReady()).toBe(true);
  });

  it('plays frames in sequence order', () => {
    const pcm0 = new Float32Array([1, 2, 3]);
    const pcm1 = new Float32Array([4, 5, 6]);
    const pcm2 = new Float32Array([7, 8, 9]);
    const pcm3 = new Float32Array([10, 11, 12]);

    buf.insert(0, pcm0);
    buf.insert(1, pcm1);
    buf.insert(2, pcm2);
    buf.insert(3, pcm3);

    // exhaust fill period — returns null during fill
    for (let i = 0; i < DEPTH; i++) {
      buf.advance();
    }

    // Now reads should start at seq 4 (playSeq advanced DEPTH times during fill)
    // Insert frames for the next round
    const pcm4 = new Float32Array([13, 14, 15]);
    const pcm5 = new Float32Array([16, 17, 18]);
    buf.insert(4, pcm4);
    buf.insert(5, pcm5);

    expect(buf.advance()).toEqual(pcm4);
    expect(buf.advance()).toEqual(pcm5);
  });

  it('returns null for missing frames (silence)', () => {
    // No frames inserted; advance through fill period
    for (let i = 0; i < DEPTH; i++) {
      buf.advance();
    }
    // No frame at playSeq — should return null (concealment)
    expect(buf.advance()).toBeNull();
  });

  it('handles out-of-order arrival', () => {
    // Insert frames out of order
    buf.insert(1, new Float32Array([2]));
    buf.insert(0, new Float32Array([1]));
    buf.insert(3, new Float32Array([4]));
    buf.insert(2, new Float32Array([3]));

    // Advance through fill period (seq 0..3 consumed)
    for (let i = 0; i < DEPTH; i++) {
      buf.advance();
    }

    // Frames 4+ are not inserted, so next advance should return null
    expect(buf.advance()).toBeNull();
  });

  it('drops late frames (already played past that sequence)', () => {
    // Advance through fill period — playSeq is now DEPTH (4)
    for (let i = 0; i < DEPTH; i++) {
      buf.advance();
    }

    // Insert a frame at seq=0 which is now in the past
    const stalePcm = new Float32Array([99, 99]);
    buf.insert(0, stalePcm);

    // playSeq is 4; inserting seq=4 with a sentinel value
    const freshPcm = new Float32Array([1, 2]);
    buf.insert(4, freshPcm);

    const result = buf.advance();
    expect(result).toEqual(freshPcm);
    expect(result).not.toEqual(stalePcm);
  });

  it('handles sequence wraparound at u16 boundary', () => {
    // Wind playSeq to just before the u16 wraparound boundary.
    // After the fill period (DEPTH advances), playSeq == DEPTH.
    // We want playSeq == 0xFFFE so frames 0xFFFE, 0xFFFF, 0x0000, 0x0001 wrap around.
    // Total advances needed: 0xFFFE = fill(DEPTH) + extra(0xFFFE - DEPTH)
    buf = new JitterBuffer(DEPTH, FRAME_MS);
    for (let i = 0; i < DEPTH; i++) buf.advance(); // playSeq = DEPTH
    const extra = (0xFFFE - DEPTH + 0x10000) & 0xFFFF;
    for (let i = 0; i < extra; i++) buf.advance(); // playSeq = 0xFFFE

    // Insert four frames that straddle the 0xFFFF → 0x0000 wraparound
    const pcmA = new Float32Array([10]); // seq 0xFFFE
    const pcmB = new Float32Array([20]); // seq 0xFFFF
    const pcmC = new Float32Array([30]); // seq 0x0000
    const pcmD = new Float32Array([40]); // seq 0x0001
    buf.insert(0xFFFE, pcmA);
    buf.insert(0xFFFF, pcmB);
    buf.insert(0x0000, pcmC);
    buf.insert(0x0001, pcmD);

    expect(buf.advance()).toEqual(pcmA);
    expect(buf.advance()).toEqual(pcmB);
    expect(buf.advance()).toEqual(pcmC);
    expect(buf.advance()).toEqual(pcmD);
  });

  it('reset clears all state', () => {
    buf.insert(0, new Float32Array([1, 2, 3]));
    for (let i = 0; i < DEPTH; i++) buf.advance();
    expect(buf.isReady()).toBe(true);

    buf.reset();

    expect(buf.isReady()).toBe(false);
    // After reset, fill period starts again
    for (let i = 0; i < DEPTH - 1; i++) buf.advance();
    expect(buf.isReady()).toBe(false);
    buf.advance();
    expect(buf.isReady()).toBe(true);
    // Slots cleared — no lingering frames
    expect(buf.advance()).toBeNull();
  });
});
