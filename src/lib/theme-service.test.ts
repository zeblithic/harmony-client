import { beforeEach, describe, expect, it, vi } from 'vitest';
import { get } from 'svelte/store';
import {
  THEME_APPLIED_EVENT,
  _resetThemeServiceForTest,
  connectOwnerTheme,
  initThemePrePaint,
  resolveTheme,
  setThemePreference,
  themePreference,
} from './theme-service';

const OWNER = 'aaaa0000aaaa0000';
const OTHER = 'bbbb1111bbbb1111';

beforeEach(() => {
  localStorage.clear();
  _resetThemeServiceForTest();
  delete document.documentElement.dataset.theme;
});

describe('resolveTheme', () => {
  it('passes explicit prefs through and resolves system via matchMedia', () => {
    expect(resolveTheme('light')).toBe('light');
    expect(resolveTheme('dark')).toBe('dark');
    expect(resolveTheme('system')).toBe('light'); // global stub: matches=false
  });
});

describe('pre-paint init', () => {
  it('applies the device hint when present', () => {
    localStorage.setItem('harmony-theme:last-applied', 'dark');
    initThemePrePaint();
    expect(document.documentElement.dataset.theme).toBe('dark');
  });

  it('falls back to the system theme with no hint', () => {
    initThemePrePaint();
    expect(document.documentElement.dataset.theme).toBe('light');
  });

  it('ignores a corrupt hint', () => {
    localStorage.setItem('harmony-theme:last-applied', 'blurple');
    initThemePrePaint();
    expect(document.documentElement.dataset.theme).toBe('light');
  });

  it('installs the system-follow listener (network-window path, no owner ever)', () => {
    let matches = false;
    const listeners = new Set<() => void>();
    vi.stubGlobal('matchMedia', (query: string) => ({
      matches,
      media: query,
      onchange: null,
      addEventListener: (_: string, cb: () => void) => listeners.add(cb),
      removeEventListener: (_: string, cb: () => void) => listeners.delete(cb),
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    }));
    try {
      initThemePrePaint();
      expect(document.documentElement.dataset.theme).toBe('light');
      matches = true;
      listeners.forEach((cb) => cb());
      // No connectOwnerTheme call — the effective preference is 'system',
      // so the OS flip must be followed live (PR #407 Qodo R1 bug 2).
      expect(document.documentElement.dataset.theme).toBe('dark');
    } finally {
      vi.unstubAllGlobals();
    }
  });
});

describe('owner-scoped preference', () => {
  it('persists under the connected owner key only', () => {
    connectOwnerTheme(OWNER);
    setThemePreference('dark');
    expect(localStorage.getItem(`harmony-theme:owner-${OWNER}`)).toBe('dark');
    expect(localStorage.getItem(`harmony-theme:owner-${OTHER}`)).toBeNull();
    expect(document.documentElement.dataset.theme).toBe('dark');
  });

  it('connectOwnerTheme loads that owner’s stored pref; missing → system', () => {
    localStorage.setItem(`harmony-theme:owner-${OWNER}`, 'dark');
    connectOwnerTheme(OWNER);
    expect(document.documentElement.dataset.theme).toBe('dark');
    _resetThemeServiceForTest();
    connectOwnerTheme(OTHER);
    expect(get(themePreference)).toBe('system');
    expect(document.documentElement.dataset.theme).toBe('light');
  });

  it('corrupt stored preference degrades to system', () => {
    localStorage.setItem(`harmony-theme:owner-${OWNER}`, 'blurple');
    connectOwnerTheme(OWNER);
    expect(get(themePreference)).toBe('system');
  });

  it('no connected owner: applies but persists no owner-scoped key', () => {
    setThemePreference('dark');
    expect(document.documentElement.dataset.theme).toBe('dark');
    const ownerKeys = Object.keys(localStorage).filter((k) => k.includes(':owner-'));
    expect(ownerKeys).toEqual([]);
  });

  it('writes the last-applied device hint on every apply', () => {
    connectOwnerTheme(OWNER);
    setThemePreference('dark');
    expect(localStorage.getItem('harmony-theme:last-applied')).toBe('dark');
    setThemePreference('light');
    expect(localStorage.getItem('harmony-theme:last-applied')).toBe('light');
  });

  it('emits THEME_APPLIED_EVENT with the resolved theme', () => {
    const seen: string[] = [];
    const listener = (e: Event) => seen.push((e as CustomEvent).detail as string);
    document.addEventListener(THEME_APPLIED_EVENT, listener);
    connectOwnerTheme(OWNER);
    setThemePreference('dark');
    document.removeEventListener(THEME_APPLIED_EVENT, listener);
    expect(seen).toContain('dark');
  });
});

describe('system follow', () => {
  it('re-applies on matchMedia change only while preference is system', () => {
    let matches = false;
    const listeners = new Set<() => void>();
    vi.stubGlobal('matchMedia', (query: string) => ({
      matches,
      media: query,
      onchange: null,
      addEventListener: (_: string, cb: () => void) => listeners.add(cb),
      removeEventListener: (_: string, cb: () => void) => listeners.delete(cb),
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    }));
    try {
      connectOwnerTheme(OWNER); // pref defaults to 'system'
      expect(document.documentElement.dataset.theme).toBe('light');
      matches = true;
      listeners.forEach((cb) => cb());
      expect(document.documentElement.dataset.theme).toBe('dark');
      setThemePreference('light');
      matches = false;
      listeners.forEach((cb) => cb());
      // Explicit preference wins; the flip back to matches=false must not re-apply.
      expect(document.documentElement.dataset.theme).toBe('light');
    } finally {
      vi.unstubAllGlobals();
    }
  });
});
