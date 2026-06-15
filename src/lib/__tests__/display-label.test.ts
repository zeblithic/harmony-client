import { describe, it, expect } from 'vitest';
import { nonEmpty } from '../display-label';

describe('nonEmpty', () => {
  it('returns a present, non-blank string unchanged (no trimming of the value)', () => {
    expect(nonEmpty('Alice')).toBe('Alice');
    // Only blankness gates — a name with surrounding spaces is still a name.
    expect(nonEmpty('  Alice  ')).toBe('  Alice  ');
  });

  it('returns undefined for null / undefined', () => {
    expect(nonEmpty(null)).toBeUndefined();
    expect(nonEmpty(undefined)).toBeUndefined();
  });

  it('returns undefined for empty or whitespace-only strings', () => {
    expect(nonEmpty('')).toBeUndefined();
    expect(nonEmpty('   ')).toBeUndefined();
    expect(nonEmpty('\t\n ')).toBeUndefined();
  });

  it('drives a `??` label ladder past a blank card name to the next source', () => {
    const cardName = '';
    const backendName = 'BackendName';
    const label = nonEmpty(undefined) ?? nonEmpty(cardName) ?? nonEmpty(backendName) ?? 'hexfall';
    expect(label).toBe('BackendName');
  });
});
