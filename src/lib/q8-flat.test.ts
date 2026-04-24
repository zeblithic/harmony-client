// src/lib/q8-flat.test.ts
import { describe, it, expect } from 'vitest';
import {
  FLAT_CONSONANTS,
  FLAT_VOWELS,
  formatFlatSyllable,
  formatFlatByte,
  formatFlatBytes,
  formatFlatNibbles,
  findFirstNibbleDiff,
} from './q8-flat';

describe('q8-flat', () => {
  it('FLAT_CONSONANTS matches Rust constants', () => {
    expect(FLAT_CONSONANTS).toEqual(["'", 'J', 'K', 'V']);
  });

  it('FLAT_VOWELS matches Rust constants', () => {
    expect(FLAT_VOWELS).toEqual(['O', 'U', 'E', 'I']);
  });

  it('formatFlatSyllable maps canonical nibbles', () => {
    expect(formatFlatSyllable(0x0)).toBe("'O");
    expect(formatFlatSyllable(0x3)).toBe("'I");
    expect(formatFlatSyllable(0x4)).toBe('JO');
    expect(formatFlatSyllable(0xa)).toBe('KE');
    expect(formatFlatSyllable(0xf)).toBe('VI');
  });

  it("formatFlatByte renders KU'E for 0x92", () => {
    // Spec example (flashcard-design.md §Q8 Display Formats): 0x92 → KU'E.
    expect(formatFlatByte(0x92)).toBe("KU'E");
  });

  it('formatFlatBytes joins words with single spaces', () => {
    // 0xa8 = KE KO, 0x3f = 'I VI — rendered as "KEKO 'IVI"
    expect(formatFlatBytes([0xa8, 0x3f])).toBe("KEKO 'IVI");
  });

  it('formatFlatNibbles pads unpaired trailing nibble with ??', () => {
    // Single nibble 0x5 = JU, padded to one word "JU??"
    expect(formatFlatNibbles([0x5])).toBe('JU??');
  });

  it('formatFlatNibbles groups pairs into space-separated bytes', () => {
    // [0x0, 0x4, 0xa, 0xf] → "'OJO KEVI"
    expect(formatFlatNibbles([0x0, 0x4, 0xa, 0xf])).toBe("'OJO KEVI");
  });

  it('formatFlatNibbles handles empty input', () => {
    expect(formatFlatNibbles([])).toBe('');
  });

  describe('findFirstNibbleDiff', () => {
    it('returns -1 when sequences agree on every nibble', () => {
      // 0x92 → nibbles [9, 2]; exact match.
      expect(findFirstNibbleDiff([0x92], [0x9, 0x2])).toBe(-1);
    });

    it('returns 0 when the first (high) nibble differs', () => {
      expect(findFirstNibbleDiff([0x92], [0xa, 0x2])).toBe(0);
    });

    it('returns 1 when the low nibble of the first byte differs', () => {
      expect(findFirstNibbleDiff([0x92], [0x9, 0x3])).toBe(1);
    });

    it('returns the nibble index of the first diverging byte', () => {
      // Two bytes expected [0x92, 0xa8] → nibbles [9, 2, a, 8];
      // heard [9, 2, a, 9] differs at index 3 (the low nibble of byte 1).
      expect(findFirstNibbleDiff([0x92, 0xa8], [0x9, 0x2, 0xa, 0x9])).toBe(3);
    });

    it('returns end index when heard is shorter than expected', () => {
      // Expected has 2 nibbles, heard has 1 (first matches). Diff at index 1 (missing).
      expect(findFirstNibbleDiff([0x92], [0x9])).toBe(1);
    });

    it('returns overflow index when heard runs past expected', () => {
      // Expected 1 byte, heard 3 nibbles (matches + extra) → diff at index 2.
      expect(findFirstNibbleDiff([0x92], [0x9, 0x2, 0xa])).toBe(2);
    });
  });
});
