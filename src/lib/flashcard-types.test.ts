// src/lib/flashcard-types.test.ts
import { describe, it, expect } from 'vitest';
import type { AppMode } from './types';
import { LEVELS, LEVEL_NAMES, initialSessionStats } from './flashcard-types';

describe('flashcard-types', () => {
  it('AppMode includes spellbook', () => {
    const mode: AppMode = 'spellbook';
    expect(mode).toBe('spellbook');
  });

  it('LEVELS has 5 entries matching design spec', () => {
    expect(LEVELS).toHaveLength(5);
    expect(LEVELS).toEqual([0, 1, 2, 3, 4]);
  });

  it('LEVEL_NAMES maps all levels', () => {
    expect(LEVEL_NAMES[0]).toBe('Novice');
    expect(LEVEL_NAMES[1]).toBe('Apprentice');
    expect(LEVEL_NAMES[2]).toBe('Journeyman');
    expect(LEVEL_NAMES[3]).toBe('Expert');
    expect(LEVEL_NAMES[4]).toBe('Master');
  });

  it('initialSessionStats returns zeroed stats', () => {
    const stats = initialSessionStats();
    expect(stats.cardsCompleted).toBe(0);
    expect(stats.perfectCards).toBe(0);
    expect(stats.expressCards).toBe(0);
    expect(stats.bestTimeMs).toBeNull();
    expect(stats.totalTimeMs).toBe(0);
    expect(stats.previousTimeMs).toBeNull();
    expect(stats.combo).toBe(0);
    expect(stats.totalCreditedBits).toBe(0);
  });
});
