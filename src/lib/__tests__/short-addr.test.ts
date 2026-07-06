import { describe, it, expect } from 'vitest';
import { shortAddr, shortId } from '../short-addr';

describe('short-addr', () => {
  it('shortAddr renders first-8…last-4 for long hex', () => {
    expect(shortAddr('ab'.repeat(16))).toBe('abababab…abab');
  });
  it('shortAddr passes short strings through', () => {
    expect(shortAddr('abcd1234')).toBe('abcd1234');
  });
  it('shortId renders first-8… for long hex', () => {
    expect(shortId('cd'.repeat(32))).toBe('cdcdcdcd…');
  });
  it('shortId passes 8-char strings through', () => {
    expect(shortId('deadbeef')).toBe('deadbeef');
  });
});
