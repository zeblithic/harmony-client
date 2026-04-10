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

  it('plays frames in sequence order (preserved through fill)', () => {
    const pcm0 = new Float32Array([1, 2, 3]);
    const pcm1 = new Float32Array([4, 5, 6]);
    const pcm2 = new Float32Array([7, 8, 9]);
    const pcm3 = new Float32Array([10, 11, 12]);

    buf.insert(0, pcm0);
    buf.insert(1, pcm1);
    buf.insert(2, pcm2);
    buf.insert(3, pcm3);

    // Fill period returns null — frames are NOT consumed
    for (let i = 0; i < DEPTH; i++) {
      expect(buf.advance()).toBeNull();
    }

    // After fill, frames are preserved and played in order
    expect(buf.advance()).toEqual(pcm0);
    expect(buf.advance()).toEqual(pcm1);
    expect(buf.advance()).toEqual(pcm2);
    expect(buf.advance()).toEqual(pcm3);
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
    // Insert frames 0–3 out of order during the fill period
    const pcm0 = new Float32Array([1]);
    const pcm1 = new Float32Array([2]);
    const pcm2 = new Float32Array([3]);
    const pcm3 = new Float32Array([4]);

    buf.insert(2, pcm2);
    buf.insert(0, pcm0);
    buf.insert(3, pcm3);
    buf.insert(1, pcm1);

    // Fill period
    for (let i = 0; i < DEPTH; i++) buf.advance();

    // Playback returns frames in ascending sequence order
    expect(buf.advance()).toEqual(pcm0); // seq 0
    expect(buf.advance()).toEqual(pcm1); // seq 1
    expect(buf.advance()).toEqual(pcm2); // seq 2
    expect(buf.advance()).toEqual(pcm3); // seq 3
  });

  it('drops late frames (already played past that sequence)', () => {
    // Insert and play through frames 0–3
    buf.insert(0, new Float32Array([1]));
    buf.insert(1, new Float32Array([2]));
    buf.insert(2, new Float32Array([3]));
    buf.insert(3, new Float32Array([4]));

    for (let i = 0; i < DEPTH; i++) buf.advance(); // fill period
    for (let i = 0; i < DEPTH; i++) buf.advance(); // play 0–3, playSeq now 4

    // Insert a frame at seq=0 which is now in the past
    const stalePcm = new Float32Array([99, 99]);
    buf.insert(0, stalePcm);

    // Insert a fresh frame at the current playhead
    const freshPcm = new Float32Array([5, 6]);
    buf.insert(4, freshPcm);

    const result = buf.advance();
    expect(result).toEqual(freshPcm);
    expect(result).not.toEqual(stalePcm);
  });

  it('handles sequence wraparound at u16 boundary', () => {
    // Wind playSeq to just before the u16 wraparound boundary.
    // Fill period: DEPTH advances, playSeq stays at 0.
    // Post-fill: each advance increments playSeq by 1.
    // Target: playSeq == 0xFFFE → need DEPTH (fill) + 0xFFFE (post-fill) advances.
    buf = new JitterBuffer(DEPTH, FRAME_MS);
    for (let i = 0; i < DEPTH; i++) buf.advance(); // fill, playSeq stays 0
    for (let i = 0; i < 0xFFFE; i++) buf.advance(); // post-fill, playSeq → 0xFFFE

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
