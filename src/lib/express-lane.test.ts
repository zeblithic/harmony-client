// src/lib/express-lane.test.ts
import { describe, it, expect } from 'vitest';
import { evaluateBytes } from './express-lane';

describe('evaluateBytes', () => {
  it('all green when exact match (express off)', () => {
    const { results, creditedBits, hasRed } = evaluateBytes([0x00], [0, 0], false);
    expect(results).toEqual(['green']);
    expect(creditedBits).toBe(8);
    expect(hasRed).toBe(false);
  });

  it('red when mismatch (express off)', () => {
    const { results, creditedBits, hasRed } = evaluateBytes([0x00], [1, 0], false);
    expect(results).toEqual(['red']);
    expect(creditedBits).toBe(0);
    expect(hasRed).toBe(true);
  });

  it('yellow when consonant matches but vowel differs (express on)', () => {
    // nibble 0: consonant=0, vowel=0
    // nibble 1: consonant=0, vowel=1 (same consonant!)
    const { results, creditedBits, hasRed } = evaluateBytes([0x00], [1, 1], true);
    expect(results).toEqual(['yellow']);
    expect(creditedBits).toBe(4);
    expect(hasRed).toBe(false);
  });

  it('red when consonant differs even with express on', () => {
    // nibble 0 consonant index 0, nibble 4 consonant index 1
    const { results, creditedBits, hasRed } = evaluateBytes([0x00], [4, 0], true);
    expect(results).toEqual(['red']);
    expect(creditedBits).toBe(0);
    expect(hasRed).toBe(true);
  });

  it('mixed results across multiple bytes', () => {
    // byte 0x00 (nibbles 0,0) heard [0,0] = green
    // byte 0xFF (nibbles 15,15) heard [13,13] → consonant 3==3 → yellow
    const { results, creditedBits, hasRed } = evaluateBytes(
      [0x00, 0xff],
      [0, 0, 13, 13],
      true,
    );
    expect(results).toEqual(['green', 'yellow']);
    expect(creditedBits).toBe(12);
    expect(hasRed).toBe(false);
  });

  it('vowel-only mismatch is red without express', () => {
    const { results, hasRed } = evaluateBytes([0x00], [1, 1], false);
    expect(results).toEqual(['red']);
    expect(hasRed).toBe(true);
  });

  it('missing nibbles produce red, not false consonant match', () => {
    // heardNibbles shorter than expected — should be red, not yellow
    // (previously -1 fallback matched consonant index 3 for nibbles 12-15)
    const { results, hasRed } = evaluateBytes([0xff], [], true);
    expect(results).toEqual(['red']);
    expect(hasRed).toBe(true);
  });

  it('partially missing nibbles produce red', () => {
    // Only one nibble heard for a byte that needs two
    const { results, hasRed } = evaluateBytes([0x00], [0], true);
    expect(results).toEqual(['red']);
    expect(hasRed).toBe(true);
  });
});
