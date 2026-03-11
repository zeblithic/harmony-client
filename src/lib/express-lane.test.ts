// src/lib/express-lane.test.ts
import { describe, it, expect } from 'vitest';
import { evaluateBytes } from './express-lane';

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
