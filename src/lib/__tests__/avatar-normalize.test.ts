import { describe, it, expect } from 'vitest';
import {
  validateAvatarInput,
  assertDecodedDimsOk,
  assertHeaderDimsOk,
  parseImageHeaderDims,
  AVATAR_MAX_INPUT_BYTES,
  AVATAR_MAX_DECODED_DIM,
} from '../avatar-normalize';

/** Build a minimal PNG (signature + IHDR) declaring the given dimensions. */
function pngWithDims(width: number, height: number): Uint8Array {
  const b = new Uint8Array(24);
  b.set([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a], 0); // signature
  b.set([0x00, 0x00, 0x00, 0x0d], 8); // IHDR chunk length = 13
  b.set([0x49, 0x48, 0x44, 0x52], 12); // "IHDR"
  b.set([(width >>> 24) & 0xff, (width >>> 16) & 0xff, (width >>> 8) & 0xff, width & 0xff], 16);
  b.set([(height >>> 24) & 0xff, (height >>> 16) & 0xff, (height >>> 8) & 0xff, height & 0xff], 20);
  return b;
}

/** Build a minimal JPEG (SOI + SOF0) declaring the given 16-bit dimensions. */
function jpegWithDims(width: number, height: number): Uint8Array {
  return new Uint8Array([
    0xff, 0xd8, // SOI
    0xff, 0xc0, // SOF0
    0x00, 0x11, // segment length = 17
    0x08, // sample precision
    (height >> 8) & 0xff, height & 0xff,
    (width >> 8) & 0xff, width & 0xff,
    0x03, // component count
    0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01, // 3 component specs
  ]);
}

describe('avatar-normalize input guards', () => {
  it('rejects a non-image file', () => {
    const f = new File([new Uint8Array([1, 2, 3])], 'x.txt', { type: 'text/plain' });
    expect(() => validateAvatarInput(f)).toThrow(/image/i);
  });

  it('rejects an oversize file', () => {
    const big = new Uint8Array(AVATAR_MAX_INPUT_BYTES + 1);
    const f = new File([big], 'big.png', { type: 'image/png' });
    expect(() => validateAvatarInput(f)).toThrow(/too large/i);
  });

  it('accepts a small png', () => {
    const f = new File([new Uint8Array([0x89, 0x50])], 'ok.png', { type: 'image/png' });
    expect(() => validateAvatarInput(f)).not.toThrow();
  });
});

describe('avatar-normalize decompression-bomb guard', () => {
  it('rejects an oversize decoded width', () => {
    expect(() => assertDecodedDimsOk(AVATAR_MAX_DECODED_DIM + 1, 64)).toThrow(/too large/i);
  });

  it('rejects an oversize decoded height', () => {
    expect(() => assertDecodedDimsOk(64, AVATAR_MAX_DECODED_DIM + 1)).toThrow(/too large/i);
  });

  it('accepts dimensions at the limit', () => {
    expect(() =>
      assertDecodedDimsOk(AVATAR_MAX_DECODED_DIM, AVATAR_MAX_DECODED_DIM),
    ).not.toThrow();
  });
});

describe('avatar-normalize header-dimension guard (ZEB-408)', () => {
  it('rejects a PNG whose IHDR declares an over-limit dimension before decode', () => {
    expect(() => assertHeaderDimsOk(pngWithDims(100000, 64))).toThrow(/too large/i);
    expect(() => assertHeaderDimsOk(pngWithDims(64, 100000))).toThrow(/too large/i);
  });

  it('rejects a JPEG whose SOF declares an over-limit dimension before decode', () => {
    expect(() => assertHeaderDimsOk(jpegWithDims(60000, 64))).toThrow(/too large/i);
    expect(() => assertHeaderDimsOk(jpegWithDims(64, 60000))).toThrow(/too large/i);
  });

  it('accepts in-bounds PNG and JPEG headers', () => {
    expect(() => assertHeaderDimsOk(pngWithDims(256, 256))).not.toThrow();
    expect(() => assertHeaderDimsOk(jpegWithDims(256, 256))).not.toThrow();
    expect(() =>
      assertHeaderDimsOk(pngWithDims(AVATAR_MAX_DECODED_DIM, AVATAR_MAX_DECODED_DIM)),
    ).not.toThrow();
  });

  it('is a no-op for an unknown / truncated header (defers to the post-decode guard)', () => {
    // GIF signature — not parsed.
    expect(() => assertHeaderDimsOk(new Uint8Array([0x47, 0x49, 0x46, 0x38, 0x39, 0x61]))).not.toThrow();
    // PNG signature but too short to hold an IHDR.
    expect(() => assertHeaderDimsOk(new Uint8Array([0x89, 0x50, 0x4e, 0x47]))).not.toThrow();
    // Empty.
    expect(() => assertHeaderDimsOk(new Uint8Array([]))).not.toThrow();
  });

  it('parses exact PNG / JPEG header dims and returns null for unparseable input', () => {
    expect(parseImageHeaderDims(pngWithDims(640, 480))).toEqual({ width: 640, height: 480 });
    expect(parseImageHeaderDims(jpegWithDims(640, 480))).toEqual({ width: 640, height: 480 });
    expect(parseImageHeaderDims(new Uint8Array([0x47, 0x49, 0x46]))).toBeNull();
  });
});
