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

  it('falls back to creator PAIR when originalCreator fields are only partially set (address only)', () => {
    // FIX 4 (PR #120 round 1): a malformed wire payload could land
    // with only one of the two fields populated. Per-field fallback
    // would produce a mismatched pair (e.g. addr-alice + name "Bob").
    // The atomic "both or neither" rule treats partial as "absent"
    // and falls back to the source vine's creator pair instead.
    const v = vine({
      creatorAddress: 'addr-bob',
      creatorName: 'Bob',
      reshareOf: 'orig',
      originalCreatorAddress: 'addr-alice',
      // originalCreatorName missing
    });
    expect(resolveOriginalCreator(v)).toEqual({
      originalCreatorAddress: 'addr-bob',
      originalCreatorName: 'Bob',
    });
  });

  it('falls back to creator PAIR when originalCreator fields are only partially set (name only)', () => {
    // Symmetric to the address-only case: the same atomic rule applies.
    const v = vine({
      creatorAddress: 'addr-bob',
      creatorName: 'Bob',
      reshareOf: 'orig',
      // originalCreatorAddress missing
      originalCreatorName: 'Alice',
    });
    expect(resolveOriginalCreator(v)).toEqual({
      originalCreatorAddress: 'addr-bob',
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
