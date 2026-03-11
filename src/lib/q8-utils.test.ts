// src/lib/q8-utils.test.ts
import { describe, it, expect } from 'vitest';
import {
  BOX_CONSONANTS,
  BOX_VOWELS,
  nibbleToConsonant,
  nibbleToVowel,
  byteToBoxCell,
  consonantIndex,
} from './q8-utils';

describe('q8-utils', () => {
  it('BOX_CONSONANTS matches Rust constants', () => {
    expect(BOX_CONSONANTS).toEqual(['A', '>', '<', 'V']);
  });

  it('BOX_VOWELS matches Rust constants', () => {
    expect(BOX_VOWELS).toEqual(['O', '=', 'X', 'I']);
  });

  it('nibbleToConsonant maps all 16 nibbles', () => {
    expect(nibbleToConsonant(0)).toBe('A');
    expect(nibbleToConsonant(4)).toBe('>');
    expect(nibbleToConsonant(8)).toBe('<');
    expect(nibbleToConsonant(12)).toBe('V');
    expect(nibbleToConsonant(7)).toBe('>');
  });

  it('nibbleToVowel maps all 16 nibbles', () => {
    expect(nibbleToVowel(0)).toBe('O');
    expect(nibbleToVowel(1)).toBe('=');
    expect(nibbleToVowel(2)).toBe('X');
    expect(nibbleToVowel(3)).toBe('I');
    expect(nibbleToVowel(5)).toBe('=');
  });

  it('byteToBoxCell returns 2x2 character grid for 0x00', () => {
    const cell = byteToBoxCell(0x00);
    expect(cell).toEqual({
      topLeft: 'A', topRight: 'A',
      bottomLeft: 'O', bottomRight: 'O',
    });
  });

  it('byteToBoxCell for 0xFF', () => {
    const cell = byteToBoxCell(0xff);
    expect(cell).toEqual({
      topLeft: 'V', topRight: 'V',
      bottomLeft: 'I', bottomRight: 'I',
    });
  });

  it('byteToBoxCell for 0x59', () => {
    const cell = byteToBoxCell(0x59);
    expect(cell).toEqual({
      topLeft: '>', topRight: '<',
      bottomLeft: '=', bottomRight: '=',
    });
  });

  it('consonantIndex extracts consonant from nibble', () => {
    expect(consonantIndex(0)).toBe(0);
    expect(consonantIndex(4)).toBe(1);
    expect(consonantIndex(8)).toBe(2);
    expect(consonantIndex(15)).toBe(3);
  });
});
