import { describe, it, expect } from 'vitest';
import { resolveOriginalCreator } from './vine-utils';
import type { VineVideo } from './types';

function vine(overrides: Partial<VineVideo> = {}): VineVideo {
  return {
    id: 'vine-x',
    creatorAddress: 'addr-creator',
    creatorName: 'Creator',
    createdAt: 1700000000,
    videoCid: 'cid-x',
    viewed: false,
    ...overrides,
  };
}

describe('resolveOriginalCreator', () => {
  it('uses creator fields when the vine is an original (no reshareOf)', () => {
    const v = vine({
      creatorAddress: 'addr-alice',
      creatorName: 'Alice',
    });
    expect(resolveOriginalCreator(v)).toEqual({
      originalCreatorAddress: 'addr-alice',
      originalCreatorName: 'Alice',
    });
  });

  it('propagates existing originalCreator fields when the vine is itself a reshare (transitive)', () => {
    // Carol's reshare of Bob's reshare of Alice's vine: the
    // originalCreator fields point at Alice (the true origin) even
    // though `creatorAddress` is Bob. We must propagate Alice, not
    // re-credit Bob.
    const v = vine({
      creatorAddress: 'addr-bob',
      creatorName: 'Bob',
      reshareOf: 'vine-alice-orig',
      originalCreatorAddress: 'addr-alice',
      originalCreatorName: 'Alice',
    });
    expect(resolveOriginalCreator(v)).toEqual({
      originalCreatorAddress: 'addr-alice',
      originalCreatorName: 'Alice',
    });
  });

  it('falls back per-field when originalCreator fields are partially set', () => {
    // Defensive: a malformed wire payload could land with only one of
    // the two fields populated. The helper falls back per-field rather
    // than treating "partial" as "absent" — matches the `??` semantics
    // documented in vine-utils.ts.
    const v = vine({
      creatorAddress: 'addr-bob',
      creatorName: 'Bob',
      reshareOf: 'orig',
      originalCreatorAddress: 'addr-alice',
      // originalCreatorName missing
    });
    expect(resolveOriginalCreator(v)).toEqual({
      originalCreatorAddress: 'addr-alice',
      originalCreatorName: 'Bob',
    });
  });

  it('returns creator fields verbatim when no original-fields are set', () => {
    // An original vine never has originalCreator* — the helper must
    // return the source vine's creator unchanged so the downstream
    // reshare credits this creator as the origin.
    const v = vine({
      creatorAddress: 'addr-dan',
      creatorName: 'Dan',
    });
    const resolved = resolveOriginalCreator(v);
    expect(resolved.originalCreatorAddress).toBe(v.creatorAddress);
    expect(resolved.originalCreatorName).toBe(v.creatorName);
  });
});
