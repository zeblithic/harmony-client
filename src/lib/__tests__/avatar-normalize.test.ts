import { describe, it, expect } from 'vitest';
import {
  validateAvatarInput,
  assertDecodedDimsOk,
  AVATAR_MAX_INPUT_BYTES,
  AVATAR_MAX_DECODED_DIM,
} from '../avatar-normalize';

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
