// src/lib/express-lane.ts
import type { ByteResult, ExpressMode } from './flashcard-types';
import { consonantIndex, vowelIndex } from './q8-utils';

/**
 * Check if a single nibble passes express matching for the given mode.
 *
 * A nibble passes if its consonant matches (consonant/both mode)
 * or its vowel matches (vowel/both mode).
 */
export function nibbleExpressMatch(
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
 * Within a red byte, return the index of the nibble that's actually
 * failing: one that differs strictly AND doesn't pass express matching
 * in the current mode. Returns 0 if the high nibble is the failure
 * point, 1 if only the low nibble fails.
 *
 * Needed because a red byte in express mode can have one nibble that
 * strictly differs but express-matches (accepted) and another that
 * actually fails. The mismatch display's caret should point at the
 * failure, not the accepted divergence — pointing at the accepted
 * nibble would be misleading feedback.
 *
 * Caller must have already verified the byte is red (i.e., at least
 * one nibble strictly differs and doesn't express-match). If neither
 * nibble is failing, this returns 1 as a sentinel.
 */
export function failingNibbleInRedByte(
  expectedByte: number,
  heardHigh: number,
  _heardLow: number,
  mode: ExpressMode,
): 0 | 1 {
  const expHigh = (expectedByte >> 4) & 0xf;
  const highStrictDiffers = heardHigh !== expHigh;
  const highFailsExpress =
    mode === 'off' || !nibbleExpressMatch(expHigh, heardHigh, mode);
  if (highStrictDiffers && highFailsExpress) return 0;
  return 1;
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
