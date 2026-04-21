// src/lib/q8-flat.ts

/** Q8-FLAT consonant characters (indexed 0-3). Matches stq8_core::q8::FLAT_CONSONANT_CHARS. */
export const FLAT_CONSONANTS = ["'", 'J', 'K', 'V'] as const;

/** Q8-FLAT vowel characters (indexed 0-3). Matches stq8_core::q8::FLAT_VOWEL_CHARS. */
export const FLAT_VOWELS = ['O', 'U', 'E', 'I'] as const;

/** Format a single nibble as a 2-char Q8-FLAT syllable (consonant + vowel). */
export function formatFlatSyllable(nibble: number): string {
  return FLAT_CONSONANTS[(nibble >> 2) & 3] + FLAT_VOWELS[nibble & 3];
}

/** Format a byte (high + low nibble) as a 4-char Q8-FLAT word. */
export function formatFlatByte(byte: number): string {
  return formatFlatSyllable((byte >> 4) & 0xf) + formatFlatSyllable(byte & 0xf);
}

/** Format a sequence of bytes as Q8-FLAT, words space-separated. */
export function formatFlatBytes(bytes: number[]): string {
  return bytes.map(formatFlatByte).join(' ');
}

/**
 * Index of the first nibble where `heard` differs from `expected`.
 *
 * Returns -1 if the two sequences agree on every byte and `heard` is
 * no longer than `expected`. If `heard` runs past `expected`, the
 * first extra nibble is reported as the diff position so the mismatch
 * display's caret points at the first overflow character.
 */
export function findFirstNibbleDiff(
  expectedBytes: number[],
  heardNibbles: number[],
): number {
  for (let i = 0; i < expectedBytes.length; i++) {
    const exp = expectedBytes[i];
    const expHigh = (exp >> 4) & 0xf;
    const expLow = exp & 0xf;
    if (heardNibbles[i * 2] !== expHigh) return i * 2;
    if (heardNibbles[i * 2 + 1] !== expLow) return i * 2 + 1;
  }
  if (heardNibbles.length > expectedBytes.length * 2) {
    return expectedBytes.length * 2;
  }
  return -1;
}

/**
 * Format loose nibbles as Q8-FLAT aligned to the byte grid.
 *
 * Nibbles are paired left-to-right; an unpaired trailing nibble is
 * padded with `??` so the column layout matches a full-byte row.
 * Useful for the mismatch "Heard:" line when the classifier returned
 * a partial or overlong sequence.
 */
export function formatFlatNibbles(nibbles: number[]): string {
  const words: string[] = [];
  for (let i = 0; i < nibbles.length; i += 2) {
    const high = formatFlatSyllable(nibbles[i]);
    const low = i + 1 < nibbles.length ? formatFlatSyllable(nibbles[i + 1]) : '??';
    words.push(high + low);
  }
  return words.join(' ');
}
