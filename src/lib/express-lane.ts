// src/lib/express-lane.ts
import type { ByteResult, ExpressMode } from './flashcard-types';
import { consonantIndex, vowelIndex } from './q8-utils';

/**
 * Check if a single nibble passes express matching for the given mode.
 *
 * A nibble passes if its consonant matches (consonant/both mode)
 * or its vowel matches (vowel/both mode).
 */
function nibbleExpressMatch(
  expected: number,
  heard: number,
  mode: ExpressMode,
): boolean {
  if (mode === 'consonant' || mode === 'both') {
    if (consonantIndex(expected) === consonantIndex(heard)) return true;
  }
  if (mode === 'vowel' || mode === 'both') {
    if (vowelIndex(expected) === vowelIndex(heard)) return true;
  }
  return false;
}

/**
 * Express lane per-byte evaluation.
 *
 * For each byte (pair of nibbles), compare expected vs heard:
 * - Both nibbles match exactly → green (8 bits)
 * - Both nibbles pass express matching → yellow (4 bits)
 * - Any nibble fails → red (0 bits)
 */
export function evaluateBytes(
  expectedBytes: number[],
  heardNibbles: number[],
  expressMode: ExpressMode,
): { results: ByteResult[]; hasRed: boolean } {
  const results: ByteResult[] = [];
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
    } else if (
      expressMode !== 'off' &&
      nibbleExpressMatch(expHigh, heardHigh, expressMode) &&
      nibbleExpressMatch(expLow, heardLow, expressMode)
    ) {
      results.push('yellow');
    } else {
      results.push('red');
      hasRed = true;
    }
  }

  return { results, hasRed };
}
