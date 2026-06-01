import { describe, it, expect, vi } from 'vitest';
import { VoiceMixer, softClip, mixFrames } from './voice-mixer';

/** Mirror of the limiter knee in voice-mixer.ts (kept local to avoid exporting an impl detail). */
const SOFT_CLIP_KNEE_TEST = 0.8;

describe('mixFrames (pure)', () => {
  it('sums in-range frames transparently sample-wise', () => {
    const a = new Float32Array([0.1, 0.2, -0.1]);
    const b = new Float32Array([0.2, 0.2, 0.1]);
    const out = mixFrames([a, b], 3);
    // Sums 0.3 / 0.4 / 0.0 are all below the knee → pass through (float32 precision).
    expect(out[0]).toBeCloseTo(0.3, 5);
    expect(out[1]).toBeCloseTo(0.4, 5);
    expect(out[2]).toBeCloseTo(0.0, 6);
  });
  it('returns silence for no inputs', () => {
    expect(Array.from(mixFrames([], 4))).toEqual([0, 0, 0, 0]);
  });
  it('soft-limits an over-unity sum below ±1 while staying loud', () => {
    const a = new Float32Array([0.9]);
    const b = new Float32Array([0.9]); // sum 1.8 must be limited, not clipped to a flat 1
    const out = mixFrames([a, b], 1);
    expect(out[0]).toBeLessThanOrEqual(1);
    expect(out[0]).toBeGreaterThan(SOFT_CLIP_KNEE_TEST);
  });
});

describe('softClip', () => {
  it('is transparent within the linear region', () => {
    expect(softClip(0)).toBe(0);
    expect(softClip(0.5)).toBeCloseTo(0.5, 6);
    expect(softClip(-0.3)).toBeCloseTo(-0.3, 6);
  });
  it('smoothly compresses past the knee without exceeding ±1', () => {
    expect(softClip(3)).toBeLessThanOrEqual(1);
    expect(softClip(3)).toBeGreaterThan(SOFT_CLIP_KNEE_TEST);
    expect(softClip(-3)).toBeGreaterThanOrEqual(-1);
    expect(softClip(-3)).toBeLessThan(-SOFT_CLIP_KNEE_TEST);
  });
  it('is monotonic across the knee (no kink/fold)', () => {
    expect(softClip(0.7)).toBeLessThan(softClip(0.9));
    expect(softClip(0.9)).toBeLessThan(softClip(2));
    expect(softClip(2)).toBeLessThan(softClip(5));
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
    mixer.drain();
    expect(node.port.postMessage).toHaveBeenCalledTimes(1);
    const sent = node.port.postMessage.mock.calls[0][0] as Float32Array;
    // 0.1 + 0.2 = 0.3 is below the knee → transparent pass-through.
    expect(sent[0]).toBeCloseTo(0.3, 5);
    expect(sent[1]).toBeCloseTo(0.3, 5);
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
