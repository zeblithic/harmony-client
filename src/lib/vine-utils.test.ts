import { describe, it, expect } from 'vitest';
import {
  resolveOriginalCreator,
  vineCreatorLabel,
  vineOriginalCreatorLabel,
  resolveVineCreatorName,
  resolveVineOriginalCreatorName,
  pickCenterIndex,
  formatVineDuration,
  isOwnOriginalVine,
} from './vine-utils';
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

  it('shows originalCreatorName even when its paired address is absent (display, not propagation)', () => {
    // A name-only attribution display has no address/name mixing risk, so a
    // present originalCreatorName must be shown — NOT dropped to creatorName the
    // way resolveOriginalCreator's both-or-neither propagation rule would.
    const v = vine({
      reshareOf: 'vine-orig',
      creatorAddress: 'a1b2c3d4',
      creatorName: 'Resharer',
      originalCreatorName: 'Original Person',
      // originalCreatorAddress intentionally absent
    });
    expect(vineOriginalCreatorLabel(v)).toBe('Original Person');
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

describe('pickCenterIndex (ZEB-612 S2)', () => {
  it('returns -1 for an empty list', () => {
    expect(pickCenterIndex([], 300)).toBe(-1);
  });

  it('returns 0 for a single card', () => {
    expect(pickCenterIndex([120], 300)).toBe(0);
  });

  it('picks the center nearest the viewport center', () => {
    expect(pickCenterIndex([100, 290, 500], 300)).toBe(1);
  });

  it('breaks ties toward the earlier card (stable under all-zero jsdom rects)', () => {
    expect(pickCenterIndex([250, 350], 300)).toBe(0);
    expect(pickCenterIndex([0, 0, 0], 0)).toBe(0);
  });
});

describe('formatVineDuration (ZEB-612 S2)', () => {
  it('formats sub-minute durations as m:ss', () => {
    expect(formatVineDuration(6)).toBe('0:06');
    expect(formatVineDuration(5.96)).toBe('0:06');
    expect(formatVineDuration(5.4)).toBe('0:05');
  });

  it('formats minute-plus durations', () => {
    expect(formatVineDuration(65)).toBe('1:05');
  });

  it('floors non-finite and negative inputs to 0:00', () => {
    expect(formatVineDuration(Number.NaN)).toBe('0:00');
    expect(formatVineDuration(Number.POSITIVE_INFINITY)).toBe('0:00');
    expect(formatVineDuration(-3)).toBe('0:00');
  });
});

describe('isOwnOriginalVine (ZEB-612 S2 — extracted from VinePlayer)', () => {
  it('true for a self-magic original', () => {
    expect(isOwnOriginalVine(vine({ creatorAddress: 'self' }))).toBe(true);
  });

  it('true for a hex-keyed own original when ownAddress matches (FIX 2, PR #120)', () => {
    expect(isOwnOriginalVine(vine({ creatorAddress: 'aabb' }), 'aabb')).toBe(true);
  });

  it("false for someone else's original", () => {
    expect(isOwnOriginalVine(vine({ creatorAddress: 'ccdd' }), 'aabb')).toBe(false);
  });

  it('false for own RESHARE (reshares of own content are re-resharable)', () => {
    expect(isOwnOriginalVine(vine({ creatorAddress: 'self', reshareOf: 'orig' }))).toBe(false);
  });
});

// ── ZEB-978: ladder-resolved vine author names ─────────────────────────

describe('resolveVineCreatorName (ZEB-978)', () => {
  const ADDR = 'a1b2c3d4e5f60718293a4b5c6d7e8f90';
  const pet = (id: string) => (id === ADDR ? 'Zeb (work)' : undefined);
  const card = (id: string) => (id === ADDR ? { displayName: 'Zebulon' } : undefined);

  it('prefers a local petname over the wire creatorName (spoof defense)', () => {
    const v = vine({ creatorAddress: ADDR, creatorName: 'Fake Friend' });
    expect(resolveVineCreatorName(v, pet, card)).toEqual({ label: 'Zeb (work)', source: 'petname' });
  });

  it('prefers the verified card name when no petname is assigned', () => {
    const v = vine({ creatorAddress: ADDR, creatorName: 'Fake Friend' });
    expect(resolveVineCreatorName(v, undefined, card)).toEqual({ label: 'Zebulon', source: 'card' });
  });

  it('demotes the wire creatorName to the unverified wire rung', () => {
    const v = vine({ creatorAddress: ADDR, creatorName: 'Whoever' });
    expect(resolveVineCreatorName(v)).toEqual({ label: 'Whoever', source: 'wire' });
  });

  it('trims the wire rung (never renders padded names)', () => {
    const v = vine({ creatorAddress: ADDR, creatorName: '  Bob  ' });
    expect(resolveVineCreatorName(v)).toEqual({ label: 'Bob', source: 'wire' });
  });

  it('falls to the hex floor when the wire name is blank (ZEB-561 never-blank)', () => {
    const v = vine({ creatorAddress: ADDR, creatorName: '   ' });
    expect(resolveVineCreatorName(v)).toEqual({ label: ADDR.slice(0, 8), source: 'hex' });
  });

  it('tags an ingest-baked hex prefix as hex, not as an unverified wire name', () => {
    // wireToVine defaults a blank wire name to creatorAddress.slice(0, 8);
    // that string is the hex floor wearing wire clothes — tag it honestly.
    const v = vine({ creatorAddress: ADDR, creatorName: ADDR.slice(0, 8) });
    expect(resolveVineCreatorName(v)).toEqual({ label: ADDR.slice(0, 8), source: 'hex' });
  });

  it("short-circuits the 'self' sentinel to the locally-known label", () => {
    const v = vine({ creatorAddress: 'self', creatorName: 'You' });
    expect(resolveVineCreatorName(v, pet, card)).toEqual({ label: 'You', source: 'self' });
  });

  it('never renders a blank self label (offline publish, empty ownDisplayName)', () => {
    const v = vine({ creatorAddress: 'self', creatorName: '' });
    expect(resolveVineCreatorName(v)).toEqual({ label: 'self', source: 'hex' });
  });
});

describe('resolveVineOriginalCreatorName (ZEB-978)', () => {
  const ORIG = 'feedfacecafebeef0123456789abcdef';
  const RESHARER = '00112233445566778899aabbccddeeff';

  it('ladder-resolves the ORIGINAL creator address (petname beats snapshot name)', () => {
    const v = vine({
      creatorAddress: RESHARER, creatorName: 'Resharer',
      reshareOf: 'vine-o', originalCreatorAddress: ORIG, originalCreatorName: 'Snapshot Name',
    });
    const pet = (id: string) => (id === ORIG ? 'My Friend' : undefined);
    expect(resolveVineOriginalCreatorName(v, pet)).toEqual({ label: 'My Friend', source: 'petname' });
  });

  it('a petname for the RESHARER must never label the original (mis-credit guard)', () => {
    const v = vine({
      creatorAddress: RESHARER, creatorName: 'Resharer',
      reshareOf: 'vine-o', originalCreatorAddress: ORIG, originalCreatorName: 'Snapshot Name',
    });
    const resharerPet = (id: string) => (id === RESHARER ? 'My Buddy' : undefined);
    expect(resolveVineOriginalCreatorName(v, resharerPet)).toEqual({ label: 'Snapshot Name', source: 'wire' });
  });

  it('without an origin ADDRESS the ladder never runs — name-only wire display', () => {
    // vineOriginalCreatorLabel contract (Qodo #337): a present name shows even
    // when its paired address is absent; but with no address there is nothing
    // to verify a petname/card against, so resolvers must not be consulted —
    // in particular not against the RESHARER's address.
    const v = vine({
      creatorAddress: RESHARER, creatorName: 'Resharer',
      reshareOf: 'vine-o', originalCreatorName: 'Orig Person',
    });
    const resharerPet = (id: string) => (id === RESHARER ? 'My Buddy' : undefined);
    expect(resolveVineOriginalCreatorName(v, resharerPet)).toEqual({ label: 'Orig Person', source: 'wire' });
  });

  it('falls back to the source creatorName, then the hex floor (existing chain)', () => {
    const named = vine({ creatorAddress: RESHARER, creatorName: 'Carol', reshareOf: 'vine-o' });
    expect(resolveVineOriginalCreatorName(named)).toEqual({ label: 'Carol', source: 'wire' });
    const bare = vine({ creatorAddress: RESHARER, creatorName: '', reshareOf: 'vine-o' });
    expect(resolveVineOriginalCreatorName(bare)).toEqual({ label: RESHARER.slice(0, 8), source: 'hex' });
  });
});
