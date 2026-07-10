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
});
