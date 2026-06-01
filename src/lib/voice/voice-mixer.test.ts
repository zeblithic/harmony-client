import { describe, it, expect, vi } from 'vitest';
import { VoiceMixer, softClip, mixFrames } from './voice-mixer';

describe('mixFrames (pure)', () => {
  it('sums equal-length frames sample-wise', () => {
    const a = new Float32Array([0.1, 0.2, -0.1]);
    const b = new Float32Array([0.2, 0.2, 0.1]);
    const out = mixFrames([a, b], 3);
    expect(Array.from(out)).toEqual([softClip(0.3), softClip(0.4), softClip(0.0)]);
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
    mixer.drain();
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
