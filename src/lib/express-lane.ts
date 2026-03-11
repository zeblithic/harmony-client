// src/lib/express-lane.ts
import type { ByteResult } from './flashcard-types';
import { consonantIndex } from './q8-utils';

/**
 * Express lane per-byte evaluation.
 *
 * For each byte (pair of nibbles), compare expected vs heard:
 * - Both nibbles match exactly → green (8 bits)
 * - Consonants match, vowels differ, express ON → yellow (4 bits)
 * - Consonant doesn't match → red (0 bits)
 */
export function evaluateBytes(
  expectedBytes: number[],
  heardNibbles: number[],
  express: boolean,
): { results: ByteResult[]; creditedBits: number; hasRed: boolean } {
  const results: ByteResult[] = [];
  let creditedBits = 0;
  let hasRed = false;

  for (let i = 0; i < expectedBytes.length; i++) {
    const byte = expectedBytes[i];
    const expHigh = (byte >> 4) & 0x0f;
    const expLow = byte & 0x0f;
    const heardHigh = heardNibbles[i * 2];
    const heardLow = heardNibbles[i * 2 + 1];

    // Missing nibbles → red (fail the row)
    if (heardHigh === undefined || heardLow === undefined) {
      results.push('red');
      hasRed = true;
      continue;
    }

    if (expHigh === heardHigh && expLow === heardLow) {
      results.push('green');
      creditedBits += 8;
    } else if (
      express &&
      consonantIndex(expHigh) === consonantIndex(heardHigh) &&
      consonantIndex(expLow) === consonantIndex(heardLow)
    ) {
      results.push('yellow');
      creditedBits += 4;
    } else {
      results.push('red');
      hasRed = true;
    }
  }

  return { results, creditedBits, hasRed };
}
