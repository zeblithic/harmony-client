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
