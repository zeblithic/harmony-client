import { describe, it, expect } from 'vitest';
import { nonEmpty, resolveMemberName } from '../display-label';

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

// resolveMemberName is the name-ONLY ladder for the call/voice cluster: it
// prefers a non-blank nickname over a non-blank card name, returning `undefined`
// when neither is present so each call/voice leaf keeps its own established hex
// format (bars: 6-char+ellipsis; toasts: slice(0,8)) rather than being forced
// onto the identity ladder's slice(0,8). It takes the two candidate names as
// values (not resolvers) so a caller with the card already in hand pays no
// second lookup.
describe('resolveMemberName', () => {
  it('prefers the friend nickname over the published card name', () => {
    expect(resolveMemberName('Ziggy', 'CardName')).toBe('Ziggy');
  });

  it('falls through to the card name when there is no nickname', () => {
    expect(resolveMemberName(undefined, 'CardName')).toBe('CardName');
  });

  it('treats a whitespace-only nickname as absent and falls through to the card', () => {
    expect(resolveMemberName('   ', 'CardName')).toBe('CardName');
  });

  it('returns undefined when nickname and card are both blank/absent (leaf hex applies)', () => {
    expect(resolveMemberName('   ', '')).toBeUndefined();
  });

  it('returns undefined when neither name is present', () => {
    expect(resolveMemberName(undefined, undefined)).toBeUndefined();
  });
});
