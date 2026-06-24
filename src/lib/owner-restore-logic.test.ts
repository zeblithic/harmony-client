import { describe, it, expect } from 'vitest';
import {
  classifyRestore,
  restoreForceFlag,
  ownerIdPrefix,
  parseMnemonicWords,
  countMnemonicWords,
  friendlyPreviewError,
} from './owner-restore-logic';

describe('classifyRestore', () => {
  it('is fresh when there is no current owner (null)', () => {
    expect(classifyRestore(null, 'aabbccdd0011')).toEqual({ kind: 'fresh' });
  });

  it('is fresh when the current owner-id is empty', () => {
    expect(classifyRestore('', 'aabbccdd0011')).toEqual({ kind: 'fresh' });
  });

  it('is readopt-same when the phrase matches the current owner (case-insensitive)', () => {
    expect(classifyRestore('AABBCCDD0011', 'aabbccdd0011')).toEqual({ kind: 'readopt-same' });
    expect(classifyRestore('aabbccdd0011', 'aabbccdd0011')).toEqual({ kind: 'readopt-same' });
  });

  it('is different-owner when the phrase derives a different owner-id', () => {
    expect(classifyRestore('aabbccdd0011', 'ffffffff9999')).toEqual({ kind: 'different-owner' });
  });
});

describe('restoreForceFlag', () => {
  it('forces only the same-owner re-adoption (overwrites an existing owner)', () => {
    expect(restoreForceFlag({ kind: 'readopt-same' })).toBe(true);
  });

  it('does not force a fresh install (nothing to overwrite)', () => {
    expect(restoreForceFlag({ kind: 'fresh' })).toBe(false);
  });

  it('does not force a different-owner (never reaches restore anyway)', () => {
    expect(restoreForceFlag({ kind: 'different-owner' })).toBe(false);
  });
});

describe('ownerIdPrefix', () => {
  it('returns the first 8 chars', () => {
    expect(ownerIdPrefix('aabbccdd0011223344')).toBe('aabbccdd');
  });
});

describe('parseMnemonicWords / countMnemonicWords', () => {
  it('splits on arbitrary whitespace and drops empties', () => {
    const phrase = '  alpha   bravo\tcharlie\ndelta  ';
    expect(parseMnemonicWords(phrase)).toEqual(['alpha', 'bravo', 'charlie', 'delta']);
    expect(countMnemonicWords(phrase)).toBe(4);
  });

  it('counts exactly 24 words for a full phrase', () => {
    const words = Array.from({ length: 24 }, (_, i) => `word${i}`).join('  ');
    expect(countMnemonicWords(words)).toBe(24);
  });

  it('counts 0 for blank input', () => {
    expect(countMnemonicWords('   \n\t ')).toBe(0);
  });
});

describe('friendlyPreviewError', () => {
  it('maps a checksum failure to transcription copy', () => {
    expect(friendlyPreviewError('bad checksum at word 13')).toMatch(/valid recovery phrase/i);
  });

  it('maps a wordlist failure to recognized-word copy', () => {
    expect(friendlyPreviewError('word not in wordlist')).toMatch(/recognized recovery word/i);
  });

  it('falls back to the raw message for anything else', () => {
    expect(friendlyPreviewError('disk on fire')).toBe('Could not parse recovery phrase: disk on fire');
  });
});
