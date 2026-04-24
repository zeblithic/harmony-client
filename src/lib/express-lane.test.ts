// src/lib/express-lane.test.ts
import { describe, it, expect } from 'vitest';
import { evaluateBytes, failingNibbleInRedByte } from './express-lane';

describe('evaluateBytes', () => {
  // --- Core matching ---

  it('all green when exact match (express off)', () => {
    const { results, hasRed } = evaluateBytes([0x00], [0, 0], 'off');
    expect(results).toEqual(['green']);
    expect(hasRed).toBe(false);
  });

  it('red when mismatch (express off)', () => {
    const { results, hasRed } = evaluateBytes([0x00], [1, 0], 'off');
    expect(results).toEqual(['red']);
    expect(hasRed).toBe(true);
  });

  // --- Consonant mode ---

  it('yellow when consonant matches but vowel differs (consonant mode)', () => {
    // nibble 0: consonant=0, vowel=0
    // nibble 1: consonant=0, vowel=1 (same consonant, different vowel)
    const { results, hasRed } = evaluateBytes([0x00], [1, 1], 'consonant');
    expect(results).toEqual(['yellow']);
    expect(hasRed).toBe(false);
  });

  it('red when consonant differs (consonant mode)', () => {
    // nibble 0 consonant index 0, nibble 4 consonant index 1
    const { results, hasRed } = evaluateBytes([0x00], [4, 0], 'consonant');
    expect(results).toEqual(['red']);
    expect(hasRed).toBe(true);
  });

  it('red when vowel matches but consonant differs (consonant mode)', () => {
    // Expected: nibble 0 = consonant 0, vowel 0
    // Heard: nibble 4 = consonant 1, vowel 0 (vowel matches, consonant doesn't)
    const { results, hasRed } = evaluateBytes([0x00], [4, 4], 'consonant');
    expect(results).toEqual(['red']);
    expect(hasRed).toBe(true);
  });

  // --- Vowel mode ---

  it('yellow when vowel matches but consonant differs (vowel mode)', () => {
    // Expected: nibble 0 = consonant 0, vowel 0
    // Heard: nibble 4 = consonant 1, vowel 0 (same vowel, different consonant)
    const { results, hasRed } = evaluateBytes([0x00], [4, 4], 'vowel');
    expect(results).toEqual(['yellow']);
    expect(hasRed).toBe(false);
  });

  it('red when vowel differs (vowel mode)', () => {
    // Expected: nibble 0 = consonant 0, vowel 0
    // Heard: nibble 1 = consonant 0, vowel 1 (consonant matches but vowel differs)
    const { results, hasRed } = evaluateBytes([0x00], [1, 1], 'vowel');
    expect(results).toEqual(['red']);
    expect(hasRed).toBe(true);
  });

  // --- Both mode ---

  it('yellow when consonant matches in both mode', () => {
    // Same consonant, different vowel → yellow (consonant arm of "both")
    const { results } = evaluateBytes([0x00], [1, 1], 'both');
    expect(results).toEqual(['yellow']);
  });

  it('yellow when vowel matches in both mode', () => {
    // Same vowel, different consonant → yellow (vowel arm of "both")
    const { results } = evaluateBytes([0x00], [4, 4], 'both');
    expect(results).toEqual(['yellow']);
  });

  it('red when neither consonant nor vowel matches (both mode)', () => {
    // Expected: nibble 0 = consonant 0, vowel 0
    // Heard: nibble 5 = consonant 1, vowel 1 (both differ)
    const { results, hasRed } = evaluateBytes([0x00], [5, 5], 'both');
    expect(results).toEqual(['red']);
    expect(hasRed).toBe(true);
  });

  // --- Mixed and edge cases ---

  it('mixed results across multiple bytes (consonant mode)', () => {
    // byte 0x00 (nibbles 0,0) heard [0,0] = green
    // byte 0xFF (nibbles 15,15) heard [13,13] → consonant 3==3 → yellow
    const { results, hasRed } = evaluateBytes(
      [0x00, 0xff],
      [0, 0, 13, 13],
      'consonant',
    );
    expect(results).toEqual(['green', 'yellow']);
    expect(hasRed).toBe(false);
  });

  it('consonant-only mismatch is red without express', () => {
    const { results, hasRed } = evaluateBytes([0x00], [1, 1], 'off');
    expect(results).toEqual(['red']);
    expect(hasRed).toBe(true);
  });

  it('missing nibbles produce red, not false match', () => {
    const { results, hasRed } = evaluateBytes([0xff], [], 'consonant');
    expect(results).toEqual(['red']);
    expect(hasRed).toBe(true);
  });

  it('partially missing nibbles produce red', () => {
    const { results, hasRed } = evaluateBytes([0x00], [0], 'consonant');
    expect(results).toEqual(['red']);
    expect(hasRed).toBe(true);
  });

  it('one nibble can match consonant while other matches vowel (both mode)', () => {
    // Expected byte 0x00: high=0 (c=0,v=0), low=0 (c=0,v=0)
    // Heard: high=1 (c=0,v=1) → consonant matches
    //        low=4 (c=1,v=0) → vowel matches
    // In "both" mode, each nibble passes independently (one via consonant, one via vowel)
    const { results } = evaluateBytes([0x00], [1, 4], 'both');
    expect(results).toEqual(['yellow']);
  });
});

describe('failingNibbleInRedByte', () => {
  it('returns 0 when the high nibble differs strictly in express-off mode', () => {
    // Byte 0x92, heard [0xa, 0x2]: high differs, low exact. Off mode → high fails.
    expect(failingNibbleInRedByte(0x92, 0xa, 0x2, 'off')).toBe(0);
  });

  it('returns 1 when only the low nibble differs strictly in express-off mode', () => {
    expect(failingNibbleInRedByte(0x92, 0x9, 0x3, 'off')).toBe(1);
  });

  it('points at the low nibble when the high nibble express-matches but the low does not (vowel mode)', () => {
    // Expected byte 0x12: high=1 (cons=0 ', vowel=1 U), low=2 (cons=0 ', vowel=2 E).
    // Heard high=0x5 (cons=1 J, vowel=1 U): strict diff, vowel matches → express-accepted.
    // Heard low=0x3 (cons=0 ', vowel=3 I): strict diff, vowel differs → true failure.
    // Caret should point at the low nibble (idx 1 within the byte), not the high.
    expect(failingNibbleInRedByte(0x12, 0x5, 0x3, 'vowel')).toBe(1);
  });

  it('points at the high nibble when only the high truly fails in consonant mode', () => {
    // Expected byte 0x48: high=4 (cons=1 J, vowel=0 O), low=8 (cons=2 K, vowel=0 O).
    // Heard high=0x80 (cons=2 K, vowel=0 O): cons differs (1 vs 2), vowel matches.
    // consonant mode → high fails (consonant differs, no match via consonant rule).
    // Heard low=0x8 (exact).
    expect(failingNibbleInRedByte(0x48, 0x8, 0x8, 'consonant')).toBe(0);
  });
});
