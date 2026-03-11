// src/lib/q8-utils.ts

/** Q8-BOX consonant characters (indexed 0-3). Matches stq8_core::q8::BOX_CONSONANT_CHARS. */
export const BOX_CONSONANTS = ['A', '>', '<', 'V'] as const;

/** Q8-BOX vowel characters (indexed 0-3). Matches stq8_core::q8::BOX_VOWEL_CHARS. */
export const BOX_VOWELS = ['O', '=', 'X', 'I'] as const;

/** Extract consonant index (0-3) from a Q8 nibble (0-15). */
export function consonantIndex(nibble: number): number {
  return (nibble >> 2) & 0x03;
}

/** Extract vowel index (0-3) from a Q8 nibble (0-15). */
export function vowelIndex(nibble: number): number {
  return nibble & 0x03;
}

/** Get the BOX consonant character for a nibble. */
export function nibbleToConsonant(nibble: number): string {
  return BOX_CONSONANTS[consonantIndex(nibble)];
}

/** Get the BOX vowel character for a nibble. */
export function nibbleToVowel(nibble: number): string {
  return BOX_VOWELS[vowelIndex(nibble)];
}

/** A 2x2 character cell representing one byte in Q8-BOX format. */
export interface BoxCell {
  topLeft: string;
  topRight: string;
  bottomLeft: string;
  bottomRight: string;
}

/** Convert a byte (0-255) to its Q8-BOX 2x2 character cell. */
export function byteToBoxCell(byte: number): BoxCell {
  const high = (byte >> 4) & 0x0f;
  const low = byte & 0x0f;
  return {
    topLeft: nibbleToConsonant(high),
    topRight: nibbleToConsonant(low),
    bottomLeft: nibbleToVowel(high),
    bottomRight: nibbleToVowel(low),
  };
}
