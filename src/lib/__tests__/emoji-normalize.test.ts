import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { normalizeEmoji, EMOJI_EDGE } from '../emoji-normalize';
import { AVATAR_MAX_INPUT_BYTES, AVATAR_MAX_DECODED_DIM } from '../avatar-normalize';

/**
 * Build a minimal PNG (signature + IHDR) declaring the given dimensions.
 * Backed by a fresh `ArrayBuffer` (not `ArrayBufferLike`) so it's a valid
 * `BlobPart` for `new File([...])` under the strict DOM lib types.
 */
function pngWithDims(width: number, height: number): Uint8Array<ArrayBuffer> {
  const b = new Uint8Array(new ArrayBuffer(24));
  b.set([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a], 0); // signature
  b.set([0x00, 0x00, 0x00, 0x0d], 8); // IHDR chunk length = 13
  b.set([0x49, 0x48, 0x44, 0x52], 12); // "IHDR"
  b.set([(width >>> 24) & 0xff, (width >>> 16) & 0xff, (width >>> 8) & 0xff, width & 0xff], 16);
  b.set([(height >>> 24) & 0xff, (height >>> 16) & 0xff, (height >>> 8) & 0xff, height & 0xff], 20);
  return b;
}

/** A spy-able mock 2D canvas context. */
interface MockCtx {
  drawImage: ReturnType<typeof vi.fn>;
}

/**
 * Stub `createImageBitmap`, `document.createElement('canvas')`, and
 * `HTMLCanvasElement.toBlob` so `normalizeEmoji` runs headless. Returns handles
 * for asserting the drawImage dest geometry and the resulting canvas dims.
 */
function installCanvasStubs(opts: {
  bitmapWidth: number;
  bitmapHeight: number;
}): {
  ctx: MockCtx;
  canvas: { width: number; height: number };
  toBlobMime: { value: string | undefined };
  bitmapClosed: { value: boolean };
} {
  const bitmapClosed = { value: false };
  vi.stubGlobal(
    'createImageBitmap',
    vi.fn(async () => ({
      width: opts.bitmapWidth,
      height: opts.bitmapHeight,
      close: () => {
        bitmapClosed.value = true;
      },
    })),
  );

  const ctx: MockCtx = { drawImage: vi.fn() };
  const toBlobMime: { value: string | undefined } = { value: undefined };
  // A bare object stands in for the canvas; width/height are plain props.
  const canvas: any = {
    width: 0,
    height: 0,
    getContext: vi.fn(() => ctx),
    toBlob: (cb: (b: Blob | null) => void, mime?: string) => {
      toBlobMime.value = mime;
      // Hand back a tiny PNG-signature blob; arrayBuffer() returns those bytes.
      const png = pngWithDims(8, 8);
      const blob = {
        arrayBuffer: async () => png.buffer.slice(png.byteOffset, png.byteOffset + png.byteLength),
      } as unknown as Blob;
      cb(blob);
    },
  };

  vi.stubGlobal('document', {
    createElement: (tag: string) => {
      if (tag !== 'canvas') throw new Error(`unexpected createElement(${tag})`);
      return canvas;
    },
  });

  return { ctx, canvas, toBlobMime, bitmapClosed };
}

describe('emoji-normalize', () => {
  beforeEach(() => {
    vi.unstubAllGlobals();
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it('exports EMOJI_EDGE = 128', () => {
    expect(EMOJI_EDGE).toBe(128);
  });

  it('rejects a non-image file', async () => {
    const f = new File([new Uint8Array([1, 2, 3])], 'x.txt', { type: 'text/plain' });
    await expect(normalizeEmoji(f)).rejects.toThrow(/image/i);
  });

  it('rejects an oversize input file', async () => {
    const big = new Uint8Array(AVATAR_MAX_INPUT_BYTES + 1);
    const f = new File([big], 'big.png', { type: 'image/png' });
    await expect(normalizeEmoji(f)).rejects.toThrow(/too large/i);
  });

  it('rejects a PNG whose header declares over-limit dimensions before decode', async () => {
    // No canvas stubs installed: the header guard must fire BEFORE createImageBitmap.
    const cib = vi.fn();
    vi.stubGlobal('createImageBitmap', cib);
    const header = pngWithDims(AVATAR_MAX_DECODED_DIM + 1, 64);
    const f = new File([header], 'huge.png', { type: 'image/png' });
    await expect(normalizeEmoji(f)).rejects.toThrow(/too large/i);
    expect(cib).not.toHaveBeenCalled();
  });

  it('rejects an over-limit DECODED bitmap (post-decode guard) for an unparseable header', async () => {
    // GIF signature → header guard is a no-op; the decoded-dim guard must fire.
    installCanvasStubs({
      bitmapWidth: AVATAR_MAX_DECODED_DIM + 1,
      bitmapHeight: 64,
    });
    const gif = new Uint8Array([0x47, 0x49, 0x46, 0x38, 0x39, 0x61]); // "GIF89a"
    const f = new File([gif], 'x.gif', { type: 'image/gif' });
    await expect(normalizeEmoji(f)).rejects.toThrow(/too large/i);
  });

  it('produces a PNG byte array (toBlob called with image/png)', async () => {
    const { toBlobMime } = installCanvasStubs({ bitmapWidth: 64, bitmapHeight: 64 });
    const f = new File([pngWithDims(64, 64)], 'e.png', { type: 'image/png' });
    const out = await normalizeEmoji(f);
    expect(out).toBeInstanceOf(Uint8Array);
    expect(toBlobMime.value).toBe('image/png');
    // The returned bytes carry a PNG signature (our stub blob is a PNG).
    expect(Array.from(out.slice(0, 8))).toEqual([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
  });

  it('contain-fit preserves aspect for a wide input (200x100 → 128x64, no crop)', async () => {
    const { ctx, canvas } = installCanvasStubs({ bitmapWidth: 200, bitmapHeight: 100 });
    const f = new File([pngWithDims(200, 100)], 'wide.png', { type: 'image/png' });
    await normalizeEmoji(f);
    // Contain: scale = min(128/200, 128/100) = 0.64 → 128 x 64, centered.
    expect(canvas.width).toBe(EMOJI_EDGE);
    expect(canvas.height).toBe(EMOJI_EDGE);
    expect(ctx.drawImage).toHaveBeenCalledTimes(1);
    const [, dx, dy, dw, dh] = ctx.drawImage.mock.calls[0];
    expect(dw).toBeCloseTo(128);
    expect(dh).toBeCloseTo(64);
    // Centered on the transparent square: dx=0, dy=(128-64)/2=32.
    expect(dx).toBeCloseTo(0);
    expect(dy).toBeCloseTo(32);
  });

  it('contain-fit preserves aspect for a tall input (100x200 → 64x128, no crop)', async () => {
    const { ctx } = installCanvasStubs({ bitmapWidth: 100, bitmapHeight: 200 });
    const f = new File([pngWithDims(100, 200)], 'tall.png', { type: 'image/png' });
    await normalizeEmoji(f);
    const [, dx, dy, dw, dh] = ctx.drawImage.mock.calls[0];
    expect(dw).toBeCloseTo(64);
    expect(dh).toBeCloseTo(128);
    expect(dx).toBeCloseTo(32);
    expect(dy).toBeCloseTo(0);
  });

  it('does NOT upscale a small square below the edge (64x64 → 64x64, centered)', async () => {
    const { ctx } = installCanvasStubs({ bitmapWidth: 64, bitmapHeight: 64 });
    const f = new File([pngWithDims(64, 64)], 'small.png', { type: 'image/png' });
    await normalizeEmoji(f);
    const [, dx, dy, dw, dh] = ctx.drawImage.mock.calls[0];
    // Contain never enlarges: scale = min(1, 128/64=2) = 1 → 64x64, centered.
    expect(dw).toBeCloseTo(64);
    expect(dh).toBeCloseTo(64);
    expect(dx).toBeCloseTo(32);
    expect(dy).toBeCloseTo(32);
  });

  it('closes the decoded bitmap (no leak)', async () => {
    const { bitmapClosed } = installCanvasStubs({ bitmapWidth: 64, bitmapHeight: 64 });
    const f = new File([pngWithDims(64, 64)], 'e.png', { type: 'image/png' });
    await normalizeEmoji(f);
    expect(bitmapClosed.value).toBe(true);
  });
});
