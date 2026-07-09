import { describe, it, expect } from 'vitest';
import { compareHlc, hlcNewer } from './hlc';

const h = (wallMs: number, logical = 0, deviceId = 'a') => ({ wallMs, logical, deviceId });

describe('compareHlc', () => {
  it('orders by wallMs first', () => {
    expect(compareHlc(h(1), h(2))).toBeLessThan(0);
    expect(compareHlc(h(2), h(1))).toBeGreaterThan(0);
  });

  it('breaks wallMs ties by logical', () => {
    expect(compareHlc(h(1, 0), h(1, 1))).toBeLessThan(0);
    expect(compareHlc(h(1, 2), h(1, 1))).toBeGreaterThan(0);
  });

  it('breaks (wallMs, logical) ties by deviceId lexical', () => {
    expect(compareHlc(h(1, 1, 'a'), h(1, 1, 'b'))).toBeLessThan(0);
    expect(compareHlc(h(1, 1, 'b'), h(1, 1, 'a'))).toBeGreaterThan(0);
  });

  it('returns 0 for identical HLCs', () => {
    expect(compareHlc(h(1, 1, 'a'), h(1, 1, 'a'))).toBe(0);
  });
});

describe('hlcNewer', () => {
  it('is strict: true only when a > b', () => {
    expect(hlcNewer(h(2), h(1))).toBe(true);
    expect(hlcNewer(h(1), h(1))).toBe(false);
    expect(hlcNewer(h(1), h(2))).toBe(false);
  });
});
