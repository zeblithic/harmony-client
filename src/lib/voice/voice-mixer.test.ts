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

  it('mixes streams of different lengths without truncating the longer one', async () => {
    // Regression (CodeAnt): a single global frameLen from the last-pushed frame
    // truncated other streams. With codec2 (160) and opus (320) frames active,
    // the output must be the LONGEST length, zero-padding the shorter stream.
    const { ctx, node } = mockCtx();
    const mixer = new VoiceMixer({
      createContext: () => ctx as unknown as AudioContext,
      createWorkletNode: () => node as unknown as AudioWorkletNode,
    });
    await mixer.init();
    mixer.pushFrame('short', new Float32Array([0.1, 0.1]));
    mixer.pushFrame('long', new Float32Array([0.2, 0.2, 0.2, 0.2]));
    mixer.drain();
    const sent = node.port.postMessage.mock.calls[0][0] as Float32Array;
    expect(sent.length).toBe(4); // longest stream preserved, not truncated to 2
    expect(sent[0]).toBeCloseTo(0.3, 5); // overlap: 0.1 + 0.2
    expect(sent[1]).toBeCloseTo(0.3, 5);
    expect(sent[2]).toBeCloseTo(0.2, 5); // only the long stream past index 1
    expect(sent[3]).toBeCloseTo(0.2, 5);
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

  // ZEB-359: output device routing (AudioContext.setSinkId where supported).
  describe('output device routing', () => {
    function mockSinkCtx() {
      const base = mockCtx();
      const setSinkId = vi.fn().mockResolvedValue(undefined);
      return {
        node: base.node,
        setSinkId,
        ctx: { ...base.ctx, setSinkId },
      };
    }

    it('init applies the preferred output via setSinkId when supported', async () => {
      const { ctx, node, setSinkId } = mockSinkCtx();
      const mixer = new VoiceMixer({
        createContext: () => ctx as unknown as AudioContext,
        createWorkletNode: () => node as unknown as AudioWorkletNode,
        outputDeviceId: () => 'spk-1',
      });
      await mixer.init();
      expect(setSinkId).toHaveBeenCalledWith('spk-1');
    });

    it('init skips setSinkId for the system default (null pref)', async () => {
      const { ctx, node, setSinkId } = mockSinkCtx();
      const mixer = new VoiceMixer({
        createContext: () => ctx as unknown as AudioContext,
        createWorkletNode: () => node as unknown as AudioWorkletNode,
        outputDeviceId: () => null,
      });
      await mixer.init();
      expect(setSinkId).not.toHaveBeenCalled();
    });

    it('init tolerates a platform without setSinkId (WKWebView)', async () => {
      const { ctx, node } = mockCtx();
      const mixer = new VoiceMixer({
        createContext: () => ctx as unknown as AudioContext,
        createWorkletNode: () => node as unknown as AudioWorkletNode,
        outputDeviceId: () => 'spk-1',
      });
      await expect(mixer.init()).resolves.toBeUndefined();
    });

    it('a rejecting setSinkId is non-fatal (falls back to default output)', async () => {
      const { ctx, node, setSinkId } = mockSinkCtx();
      setSinkId.mockRejectedValue(new Error('sink gone'));
      const mixer = new VoiceMixer({
        createContext: () => ctx as unknown as AudioContext,
        createWorkletNode: () => node as unknown as AudioWorkletNode,
        outputDeviceId: () => 'spk-gone',
      });
      await expect(mixer.init()).resolves.toBeUndefined();
    });

    it('setOutputDevice routes the live context; null resets to default', async () => {
      const { ctx, node, setSinkId } = mockSinkCtx();
      const mixer = new VoiceMixer({
        createContext: () => ctx as unknown as AudioContext,
        createWorkletNode: () => node as unknown as AudioWorkletNode,
      });
      await mixer.init();
      await mixer.setOutputDevice('spk-2');
      expect(setSinkId).toHaveBeenCalledWith('spk-2');
      await mixer.setOutputDevice(null);
      expect(setSinkId).toHaveBeenCalledWith('');
    });

    it('setOutputDevice before init is a no-op', async () => {
      const mixer = new VoiceMixer();
      await expect(mixer.setOutputDevice('spk-2')).resolves.toBeUndefined();
    });
  });
});
