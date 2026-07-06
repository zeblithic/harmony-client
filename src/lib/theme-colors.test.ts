import { beforeEach, describe, expect, it } from 'vitest';
import { COMMONS_FALLBACK, tokenColor, _clearTokenColorCacheForTest } from './theme-colors';
import { THEME_APPLIED_EVENT } from './theme-service';

beforeEach(() => {
  _clearTokenColorCacheForTest();
});

describe('tokenColor', () => {
  it('resolves via the fallback table in jsdom (getComputedStyle yields empty)', () => {
    expect(tokenColor('--accent')).toBe(COMMONS_FALLBACK['--accent']);
    expect(tokenColor('--danger')).toBe('#b1402f');
  });

  it('unknown token resolves to black rather than empty string', () => {
    expect(tokenColor('--no-such-token')).toBe('#000000');
  });

  it('caches and invalidates on THEME_APPLIED_EVENT', () => {
    document.documentElement.style.setProperty('--accent', '#123456');
    // jsdom's getComputedStyle DOES surface inline custom properties.
    document.dispatchEvent(new CustomEvent(THEME_APPLIED_EVENT, { detail: 'light' }));
    expect(tokenColor('--accent')).toBe('#123456');
    document.documentElement.style.removeProperty('--accent');
    expect(tokenColor('--accent')).toBe('#123456'); // still cached
    document.dispatchEvent(new CustomEvent(THEME_APPLIED_EVENT, { detail: 'dark' }));
    expect(tokenColor('--accent')).toBe(COMMONS_FALLBACK['--accent']); // cache dropped
  });
});
