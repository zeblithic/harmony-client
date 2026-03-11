// src/lib/flashcard-types.ts

/** WASM level index (0-4). Maps to stq8-core flashcard::Level. */
export type FlashcardLevel = 0 | 1 | 2 | 3 | 4;

export const LEVELS: readonly FlashcardLevel[] = [0, 1, 2, 3, 4] as const;

export const LEVEL_NAMES: Readonly<Record<FlashcardLevel, string>> = {
  0: 'Novice',
  1: 'Apprentice',
  2: 'Journeyman',
  3: 'Expert',
  4: 'Master',
};

/** Level metadata returned by WasmPipeline.level_info(). */
export interface LevelInfo {
  total_bytes: number;
  bytes_per_row: number;
  num_rows: number;
  total_bits: number;
}

/** Challenge returned by WasmPipeline.generate_challenge(). */
export interface Challenge {
  level: string;
  data: number[];
  rows: number[][];
}

/** Express lane mode: which phoneme dimension(s) to match on. */
export type ExpressMode = 'off' | 'consonant' | 'vowel' | 'both';

export const EXPRESS_MODES: readonly ExpressMode[] = ['off', 'consonant', 'vowel', 'both'] as const;

export const EXPRESS_MODE_LABELS: Readonly<Record<ExpressMode, string>> = {
  off: 'Off',
  consonant: 'Consonant',
  vowel: 'Vowel',
  both: 'Both',
};

/** Per-byte evaluation result for express lane scoring. */
export type ByteResult = 'pending' | 'green' | 'yellow' | 'red';

/** State of a single row during practice. */
export interface RowState {
  /** Index into Challenge.rows */
  rowIndex: number;
  /** Per-byte results (2 syllables per byte → 1 result per byte). */
  byteResults: ByteResult[];
  /** Whether this row has been completed (all green/yellow). */
  completed: boolean;
}

/** Session statistics (no persistence in v1). */
export interface SessionStats {
  cardsCompleted: number;
  perfectCards: number;
  expressCards: number;
  bestTimeMs: number | null;
  totalTimeMs: number;
  previousTimeMs: number | null;
  combo: number;
  totalCreditedBits: number;
}

export function initialSessionStats(): SessionStats {
  return {
    cardsCompleted: 0,
    perfectCards: 0,
    expressCards: 0,
    bestTimeMs: null,
    totalTimeMs: 0,
    previousTimeMs: null,
    combo: 0,
    totalCreditedBits: 0,
  };
}
