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
// climbs nickname → card and stops, returning `undefined` when neither yields a
// non-blank name. It deliberately omits a hex fallback so each call/voice leaf
// keeps its own established hex format (bars: 6-char+ellipsis; toasts:
// slice(0,8)) rather than being forced onto the identity ladder's slice(0,8).
describe('resolveMemberName', () => {
  const ownerHex = 'ff00ff00ff00ff00';

  it('prefers the friend nickname over the published card name', () => {
    const resolveNickname = (id: string) => (id === ownerHex ? 'Ziggy' : undefined);
    const resolveCard = (id: string) => (id === ownerHex ? { displayName: 'RosterName' } : undefined);
    expect(resolveMemberName(ownerHex, resolveNickname, resolveCard)).toBe('Ziggy');
  });

  it('falls through to the card name when there is no nickname', () => {
    const resolveCard = (id: string) => (id === ownerHex ? { displayName: 'CardName' } : undefined);
    expect(resolveMemberName(ownerHex, undefined, resolveCard)).toBe('CardName');
  });

  it('treats a whitespace-only nickname as absent and falls through to the card', () => {
    const resolveNickname = () => '   ';
    const resolveCard = () => ({ displayName: 'CardName' });
    expect(resolveMemberName(ownerHex, resolveNickname, resolveCard)).toBe('CardName');
  });

  it('returns undefined when nickname and card are both blank/absent (leaf hex applies)', () => {
    const resolveNickname = () => '   ';
    const resolveCard = () => ({ displayName: '' });
    expect(resolveMemberName(ownerHex, resolveNickname, resolveCard)).toBeUndefined();
  });

  it('returns undefined when no resolvers are wired at all', () => {
    expect(resolveMemberName(ownerHex)).toBeUndefined();
  });
});
