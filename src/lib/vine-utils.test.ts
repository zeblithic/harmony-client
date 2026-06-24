import { describe, it, expect } from 'vitest';
import { resolveOriginalCreator, vineCreatorLabel, vineOriginalCreatorLabel } from './vine-utils';
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

describe('vineCreatorLabel', () => {
  it('returns the name verbatim when present', () => {
    expect(vineCreatorLabel('Alice', '685e4ba7deadbeef')).toBe('Alice');
  });

  it('falls back to truncated owner-hex when the name is empty (ZEB-561)', () => {
    // A reshare/publish via the headless RPC with creatorName omitted carries
    // "" — the viewer must never render a blank resharer.
    expect(vineCreatorLabel('', '685e4ba7deadbeef')).toBe('685e4ba7');
  });

  it('falls back when the name is whitespace-only', () => {
    expect(vineCreatorLabel('   ', '685e4ba7deadbeef')).toBe('685e4ba7');
  });

  it('falls back when the name is null or undefined', () => {
    expect(vineCreatorLabel(null, '685e4ba7deadbeef')).toBe('685e4ba7');
    expect(vineCreatorLabel(undefined, '685e4ba7deadbeef')).toBe('685e4ba7');
  });

  it('trims surrounding whitespace from a real name', () => {
    expect(vineCreatorLabel('  Bob  ', 'addr')).toBe('Bob');
  });
});

describe('vineOriginalCreatorLabel', () => {
  it('uses the true-origin name when a reshare carries both originalCreator fields', () => {
    const v = vine({
      reshareOf: 'vine-orig',
      creatorAddress: 'addr-resharer',
      creatorName: 'Resharer',
      originalCreatorAddress: 'addr-alice',
      originalCreatorName: 'Alice',
    });
    expect(vineOriginalCreatorLabel(v)).toBe('Alice');
  });

  it('falls back to the source creator NAME (not hex) when originalCreatorName is missing (Qodo #337 regression)', () => {
    // A legacy/partial reshare payload with only a creatorName: must show that
    // name, NOT a truncated address — the regression Qodo flagged.
    const v = vine({
      reshareOf: 'vine-orig',
      creatorAddress: '685e4ba7deadbeef',
      creatorName: 'Carol',
      originalCreatorAddress: 'addr-alice',
      // originalCreatorName intentionally unset (partial payload)
    });
    expect(vineOriginalCreatorLabel(v)).toBe('Carol');
  });

  it('uses the creator name for a non-reshare vine', () => {
    const v = vine({ creatorAddress: 'addr-dan', creatorName: 'Dan' });
    expect(vineOriginalCreatorLabel(v)).toBe('Dan');
  });

  it('falls back to truncated hex only when both resolved name and creator name are blank', () => {
    const v = vine({
      reshareOf: 'vine-orig',
      creatorAddress: '685e4ba7deadbeef',
      creatorName: '',
      // no originalCreator* → resolver returns the (blank) source creator pair
    });
    expect(vineOriginalCreatorLabel(v)).toBe('685e4ba7');
  });
});
