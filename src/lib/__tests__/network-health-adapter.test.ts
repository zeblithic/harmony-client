import { describe, it, expect } from 'vitest';
import { explainNatClass, redactAddr } from '../network-health-adapter';
import type { NatClass } from '../types/network-health';

describe('explainNatClass', () => {
  const cases: NatClass[] = [
    'fullCone',
    'restrictedCone',
    'portRestricted',
    'symmetric',
    'unknown',
  ];

  it.each(cases)('returns non-empty headline + detail for %s', (n) => {
    const { headline, detail } = explainNatClass(n);
    expect(headline).toBeTruthy();
    expect(headline.length).toBeGreaterThan(0);
    expect(detail).toBeTruthy();
    expect(detail.length).toBeGreaterThan(0);
  });
});

describe('redactAddr', () => {
  it('returns full address when full=true', () => {
    const addr = 'a3f9e1c2'.repeat(8);
    expect(redactAddr(addr, true)).toBe(addr);
  });

  it('returns first 8 chars + ellipsis when full=false', () => {
    const addr = 'a3f9e1c2deadbeef';
    expect(redactAddr(addr, false)).toBe('a3f9e1c2…');
  });

  it('returns (unknown) for empty input', () => {
    expect(redactAddr('', false)).toBe('(unknown)');
    expect(redactAddr('', true)).toBe('(unknown)');
  });

  it('returns (unknown) for too-short input', () => {
    expect(redactAddr('abc', false)).toBe('(unknown)');
  });
});
