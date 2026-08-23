import { describe, expect, it } from 'vitest';
import {
  buildKnownPeersIndex,
  detectCollision,
  EMPTY_KNOWN_PEERS,
  nameSkeleton,
} from './name-collision';

const HEX_A = 'aaaa1111aaaa1111aaaa1111aaaa1111';
const HEX_B = 'bbbb2222bbbb2222bbbb2222bbbb2222';
const HEX_C = 'cccc3333cccc3333cccc3333cccc3333';

describe('nameSkeleton (ZEB-979)', () => {
  it('case-folds', () => {
    expect(nameSkeleton('Jake')).toBe('jake');
    expect(nameSkeleton('JAKE')).toBe('jake');
  });

  it('NFKC-normalizes fullwidth and ligature forms', () => {
    expect(nameSkeleton('Ｊａｋｅ')).toBe('jake');
    expect(nameSkeleton('ﬁnn')).toBe('finn');
  });

  it('maps curated Cyrillic homoglyphs onto Latin', () => {
    // "Jаke" with U+0430 CYRILLIC SMALL LETTER A
    expect(nameSkeleton('Jаke')).toBe('jake');
    // "Вob" with U+0412 CYRILLIC CAPITAL LETTER VE (case-folds to в → b)
    expect(nameSkeleton('Вob')).toBe('bob');
  });

  it('maps curated Greek homoglyphs onto Latin', () => {
    // "Jοhn" with U+03BF GREEK SMALL LETTER OMICRON
    expect(nameSkeleton('Jοhn')).toBe('john');
  });

  it('maps digit lookalikes onto letters', () => {
    expect(nameSkeleton('J0hn')).toBe('john');
    expect(nameSkeleton('A1ice')).toBe('alice');
  });

  it('strips zero-width and bidi control characters', () => {
    expect(nameSkeleton('Ja​ke')).toBe('jake');
    expect(nameSkeleton('Ja‍ke')).toBe('jake');
    expect(nameSkeleton('‮Jake‬')).toBe('jake');
    expect(nameSkeleton('﻿Jake')).toBe('jake');
  });

  it('trims and collapses internal whitespace', () => {
    expect(nameSkeleton('  Jake   E  ')).toBe('jake e');
  });

  it('applies full case folding where toLowerCase falls short (PR #726)', () => {
    // ß survives toLowerCase; SS does not — both must land on 'ss'.
    expect(nameSkeleton('Straße')).toBe('strasse');
    expect(nameSkeleton('STRASSE')).toBe('strasse');
    // Capital sharp s (U+1E9E) lowercases to ß, then expands.
    expect(nameSkeleton('ẞ')).toBe('ss');
    // Greek final sigma folds onto medial sigma.
    expect(nameSkeleton('ς')).toBe(nameSkeleton('σ'));
  });

  it('reduces blank input to the empty skeleton', () => {
    expect(nameSkeleton('')).toBe('');
    expect(nameSkeleton('   ')).toBe('');
    expect(nameSkeleton('​​')).toBe('');
  });
});

describe('buildKnownPeersIndex (ZEB-979)', () => {
  it('indexes entries by skeleton and collects known hexes', () => {
    const index = buildKnownPeersIndex([
      { label: 'Jake', ownerIdHex: HEX_A },
      { label: 'Ravi', ownerIdHex: HEX_B },
    ]);
    expect(index.bySkeleton.get('jake')).toEqual({ label: 'Jake', ownerIdHex: HEX_A });
    expect(index.bySkeleton.get('ravi')).toEqual({ label: 'Ravi', ownerIdHex: HEX_B });
    expect(index.knownHexes.has(HEX_A)).toBe(true);
    expect(index.knownHexes.has(HEX_B)).toBe(true);
  });

  it('keeps the FIRST entry on a skeleton tie (caller passes pools strongest-first)', () => {
    const index = buildKnownPeersIndex([
      { label: 'Jake', ownerIdHex: HEX_A },
      { label: 'jake', ownerIdHex: HEX_B },
    ]);
    expect(index.bySkeleton.get('jake')).toEqual({ label: 'Jake', ownerIdHex: HEX_A });
    // The losing entry's identity is still known.
    expect(index.knownHexes.has(HEX_B)).toBe(true);
  });

  it('skips blank labels but still records the hex as known', () => {
    const index = buildKnownPeersIndex([{ label: '   ', ownerIdHex: HEX_A }]);
    expect(index.bySkeleton.size).toBe(0);
    expect(index.knownHexes.has(HEX_A)).toBe(true);
  });

  it('lowercases hexes and honours extraKnownHexes (self exemption)', () => {
    const index = buildKnownPeersIndex(
      [{ label: 'Jake', ownerIdHex: HEX_A.toUpperCase() }],
      [HEX_C.toUpperCase()],
    );
    expect(index.knownHexes.has(HEX_A)).toBe(true);
    expect(index.knownHexes.has(HEX_C)).toBe(true);
    expect(index.bySkeleton.get('jake')?.ownerIdHex).toBe(HEX_A);
  });
});

describe('detectCollision (ZEB-979)', () => {
  const index = buildKnownPeersIndex(
    [
      { label: 'Jake', ownerIdHex: HEX_A },
      { label: 'Ravi', ownerIdHex: HEX_B },
    ],
    [HEX_C], // self
  );

  it('flags a card name colliding with a known peer under a different hex', () => {
    const hit = detectCollision(
      { label: 'jake', source: 'card' },
      'dddd4444dddd4444dddd4444dddd4444',
      index,
    );
    expect(hit).toEqual({ knownLabel: 'Jake', knownHex: HEX_A });
  });

  it('flags a homoglyph collision (Cyrillic а in a card name)', () => {
    const hit = detectCollision(
      { label: 'Jаke', source: 'card' },
      'dddd4444dddd4444dddd4444dddd4444',
      index,
    );
    expect(hit).toEqual({ knownLabel: 'Jake', knownHex: HEX_A });
  });

  it('fires for wire and roster sources too', () => {
    const stranger = 'dddd4444dddd4444dddd4444dddd4444';
    expect(detectCollision({ label: 'Jake', source: 'wire' }, stranger, index)).toBeDefined();
    expect(detectCollision({ label: 'Jake', source: 'roster' }, stranger, index)).toBeDefined();
  });

  it('never fires for petname, hex, or self sources', () => {
    const stranger = 'dddd4444dddd4444dddd4444dddd4444';
    expect(detectCollision({ label: 'Jake', source: 'petname' }, stranger, index)).toBeUndefined();
    expect(detectCollision({ label: 'Jake', source: 'hex' }, stranger, index)).toBeUndefined();
    expect(detectCollision({ label: 'Jake', source: 'self' }, stranger, index)).toBeUndefined();
  });

  it('exempts the known peer itself (their own name is not a collision)', () => {
    expect(detectCollision({ label: 'Jake', source: 'card' }, HEX_A, index)).toBeUndefined();
  });

  it('exempts a known peer even when their name matches ANOTHER known peer', () => {
    // HEX_B is known (as "Ravi"); if their card said "Jake" we stay quiet —
    // the ticket scopes the warning to identities that are NOT known peers.
    expect(detectCollision({ label: 'Jake', source: 'card' }, HEX_B, index)).toBeUndefined();
  });

  it('exempts self via extraKnownHexes', () => {
    expect(detectCollision({ label: 'Jake', source: 'card' }, HEX_C, index)).toBeUndefined();
  });

  it('is case-insensitive on the rendered hex', () => {
    expect(
      detectCollision({ label: 'Jake', source: 'card' }, HEX_A.toUpperCase(), index),
    ).toBeUndefined();
  });

  it('stays quiet on non-matching and blank labels', () => {
    const stranger = 'dddd4444dddd4444dddd4444dddd4444';
    expect(detectCollision({ label: 'Zoe', source: 'card' }, stranger, index)).toBeUndefined();
    expect(detectCollision({ label: '  ', source: 'card' }, stranger, index)).toBeUndefined();
  });

  it('stays quiet on the empty index', () => {
    expect(
      detectCollision({ label: 'Jake', source: 'card' }, HEX_A, EMPTY_KNOWN_PEERS),
    ).toBeUndefined();
  });
});
