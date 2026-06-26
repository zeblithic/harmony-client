// src/lib/voice/load-worklet-module.test.ts
//
// ZEB-575: the worklet loader must hand `addModule` a same-origin `blob:` URL
// (built from the worklet source), never a cross-origin module URL. In the
// Tauri webview the document origin (http://tauri.localhost) differs from where
// the worklet asset would be served, and `addModule` rejects a cross-origin
// module URL with a SecurityError.
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { loadWorkletModule } from './load-worklet-module';

describe('loadWorkletModule', () => {
  let createObjectURL: ReturnType<typeof vi.fn>;
  let revokeObjectURL: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    createObjectURL = vi.fn(() => 'blob:http://tauri.localhost/abc-123');
    revokeObjectURL = vi.fn();
    (URL as unknown as { createObjectURL: unknown }).createObjectURL = createObjectURL;
    (URL as unknown as { revokeObjectURL: unknown }).revokeObjectURL = revokeObjectURL;
  });
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it('addModules a same-origin blob: URL built from the worklet source', async () => {
    const addModule = vi.fn().mockResolvedValue(undefined);
    const ctx = { audioWorklet: { addModule } } as unknown as AudioContext;
    const source = 'registerProcessor("x", class extends AudioWorkletProcessor {});';

    await loadWorkletModule(ctx, source);

    // The blob is built from the worklet source as JavaScript.
    expect(createObjectURL).toHaveBeenCalledTimes(1);
    const blob = createObjectURL.mock.calls[0][0] as Blob;
    expect(blob).toBeInstanceOf(Blob);
    expect(blob.type).toBe('text/javascript');
    // addModule receives the same-origin blob: URL, never a raw/cross-origin URL.
    expect(addModule).toHaveBeenCalledTimes(1);
    expect(addModule.mock.calls[0][0]).toMatch(/^blob:/);
    // The blob URL is revoked after use (no leak).
    expect(revokeObjectURL).toHaveBeenCalledWith('blob:http://tauri.localhost/abc-123');
  });

  it('revokes the blob URL even when addModule rejects', async () => {
    const addModule = vi.fn().mockRejectedValue(new Error('boom'));
    const ctx = { audioWorklet: { addModule } } as unknown as AudioContext;

    await expect(loadWorkletModule(ctx, 'x')).rejects.toThrow('boom');
    expect(revokeObjectURL).toHaveBeenCalledWith('blob:http://tauri.localhost/abc-123');
  });
});
