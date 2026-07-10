import { describe, it, expect, vi, afterEach } from 'vitest';
import { probeVideoDuration } from './video-metadata';

/** Minimal fake <video> — jsdom never fires media events, so drive them by hand. */
function fakeVideoElement() {
  const el = {
    preload: '',
    src: '',
    onloadedmetadata: null as (() => void) | null,
    onerror: null as (() => void) | null,
    duration: Number.NaN,
    removeAttribute: vi.fn(),
    load: vi.fn(),
  };
  return el;
}

afterEach(() => vi.restoreAllMocks());

describe('probeVideoDuration (ZEB-612 S2)', () => {
  it('resolves the duration once metadata loads', async () => {
    const el = fakeVideoElement();
    vi.spyOn(document, 'createElement').mockReturnValue(el as unknown as HTMLElement);
    const p = probeVideoDuration('blob:fake');
    expect(el.src).toBe('blob:fake');
    expect(el.preload).toBe('metadata');
    el.duration = 5.8;
    el.onloadedmetadata!();
    await expect(p).resolves.toBe(5.8);
    expect(el.removeAttribute).toHaveBeenCalledWith('src');
  });

  it('rejects when the element errors (undecodable container)', async () => {
    const el = fakeVideoElement();
    vi.spyOn(document, 'createElement').mockReturnValue(el as unknown as HTMLElement);
    const p = probeVideoDuration('blob:bad');
    el.onerror!();
    await expect(p).rejects.toThrow('could not read video metadata');
  });

  it('rejects a non-finite duration (WebM/MediaRecorder missing-duration case)', async () => {
    // Infinity/NaN is "no metadata", not a measurement — resolving it would
    // let the publish gate render "Infinitys" and NaN-compare past the ≤6s
    // check. Rejection routes callers into their documented fail-open path.
    const el = fakeVideoElement();
    vi.spyOn(document, 'createElement').mockReturnValue(el as unknown as HTMLElement);
    const p = probeVideoDuration('blob:webm-stream');
    el.duration = Number.POSITIVE_INFINITY;
    el.onloadedmetadata!();
    await expect(p).rejects.toThrow('no finite duration');
  });

  it('rejects when metadata never arrives (timeout guard — callers must not hang)', async () => {
    vi.useFakeTimers();
    try {
      const el = fakeVideoElement();
      vi.spyOn(document, 'createElement').mockReturnValue(el as unknown as HTMLElement);
      const p = probeVideoDuration('blob:stalled');
      // Attach the rejection expectation BEFORE advancing time so the
      // rejection is handled the moment it fires.
      const assertion = expect(p).rejects.toThrow('timed out reading video metadata');
      vi.advanceTimersByTime(5000);
      await assertion;
      expect(el.removeAttribute).toHaveBeenCalledWith('src');
    } finally {
      vi.useRealTimers();
    }
  });
});
