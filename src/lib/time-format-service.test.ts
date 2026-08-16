// ZEB-944 — owner-scoped time & date format preference model + persistence.
import { describe, it, expect, beforeEach } from 'vitest';
import { get } from 'svelte/store';
import { formatMessageTimestamp } from './time-format';
import {
  resolveTimeFormatPrefs,
  connectOwnerTimeFormat,
  setTimeFormatSettings,
  timeFormatSettings,
  timeFormatPrefs,
  ownerTimeFormatKey,
  DEFAULT_TIME_FORMAT_SETTINGS,
  _resetTimeFormatServiceForTest,
  type TimeFormatSettings,
} from './time-format-service';

/** Minimal in-memory Storage for deterministic, DOM-free persistence tests. */
function memStorage(): Storage {
  const map = new Map<string, string>();
  return {
    get length() {
      return map.size;
    },
    clear: () => map.clear(),
    getItem: (k: string) => (map.has(k) ? (map.get(k) as string) : null),
    key: (i: number) => Array.from(map.keys())[i] ?? null,
    removeItem: (k: string) => void map.delete(k),
    setItem: (k: string, v: string) => void map.set(k, String(v)),
  };
}

beforeEach(() => {
  _resetTimeFormatServiceForTest();
});

describe('resolveTimeFormatPrefs', () => {
  it('resolves both-system to empty prefs (byte-identical to current default)', () => {
    expect(resolveTimeFormatPrefs({ clock: 'system', dateOrder: 'system' })).toEqual({});
  });

  it('maps the clock axis to hour12', () => {
    expect(resolveTimeFormatPrefs({ clock: '12h', dateOrder: 'system' })).toEqual({
      hour12: true,
    });
    expect(resolveTimeFormatPrefs({ clock: '24h', dateOrder: 'system' })).toEqual({
      hour12: false,
    });
  });

  it('maps the date-order axis to a curated locale', () => {
    expect(resolveTimeFormatPrefs({ clock: 'system', dateOrder: 'mdy' }).locale).toBe('en-US');
    expect(resolveTimeFormatPrefs({ clock: 'system', dateOrder: 'dmy' }).locale).toBe('en-GB');
    expect(resolveTimeFormatPrefs({ clock: 'system', dateOrder: 'ymd' }).locale).toBe('en-CA');
  });

  it('keeps the two axes independent (24h clock + D/M order)', () => {
    expect(resolveTimeFormatPrefs({ clock: '24h', dateOrder: 'dmy' })).toEqual({
      hour12: false,
      locale: 'en-GB',
    });
  });

  it('pins the clock to the system convention when clock=system but a date locale is chosen', () => {
    // Decoupling guard: without this, en-GB (24h default) would silently flip
    // the clock even though the user left it on "system". Inject a deterministic
    // system convention so the test does not depend on the CI runner's locale.
    expect(
      resolveTimeFormatPrefs({ clock: 'system', dateOrder: 'dmy' }, () => true),
    ).toEqual({ hour12: true, locale: 'en-GB' });
    expect(
      resolveTimeFormatPrefs({ clock: 'system', dateOrder: 'dmy' }, () => false),
    ).toEqual({ hour12: false, locale: 'en-GB' });
  });

  it('flows through the time-format seam to force a 24-hour clock', () => {
    const prefs = resolveTimeFormatPrefs({ clock: '24h', dateOrder: 'mdy' });
    const now = new Date(2026, 7, 16, 22, 14).getTime();
    const ms = new Date(2026, 7, 16, 20, 11).getTime();
    expect(formatMessageTimestamp(ms, now, prefs)).toContain('20:11');
  });
});

describe('persistence (owner-scoped)', () => {
  const owner = 'a'.repeat(32);
  const other = 'b'.repeat(32);
  const custom: TimeFormatSettings = { clock: '24h', dateOrder: 'dmy' };

  it('persists a change under the connected owner key and restores it', () => {
    const store = memStorage();
    connectOwnerTimeFormat(owner, store);
    setTimeFormatSettings(custom, store);
    expect(store.getItem(ownerTimeFormatKey(owner))).toBe(JSON.stringify(custom));

    // A fresh connect for the same owner rehydrates the saved settings.
    _resetTimeFormatServiceForTest();
    connectOwnerTimeFormat(owner, store);
    expect(get(timeFormatSettings)).toEqual(custom);
  });

  it('does NOT persist before an owner is connected', () => {
    const store = memStorage();
    setTimeFormatSettings(custom, store);
    expect(store.length).toBe(0);
    // The in-memory store still updates so the UI reflects the choice.
    expect(get(timeFormatSettings)).toEqual(custom);
  });

  it('isolates settings per owner (no cross-identity leak)', () => {
    const store = memStorage();
    connectOwnerTimeFormat(owner, store);
    setTimeFormatSettings(custom, store);

    _resetTimeFormatServiceForTest();
    connectOwnerTimeFormat(other, store);
    expect(get(timeFormatSettings)).toEqual(DEFAULT_TIME_FORMAT_SETTINGS);
  });

  it('falls back to defaults on corrupt stored JSON', () => {
    const store = memStorage();
    store.setItem(ownerTimeFormatKey(owner), '{not valid json');
    connectOwnerTimeFormat(owner, store);
    expect(get(timeFormatSettings)).toEqual(DEFAULT_TIME_FORMAT_SETTINGS);
  });

  it('sanitizes unknown enum values back to system', () => {
    const store = memStorage();
    store.setItem(ownerTimeFormatKey(owner), JSON.stringify({ clock: 'bogus', dateOrder: 42 }));
    connectOwnerTimeFormat(owner, store);
    expect(get(timeFormatSettings)).toEqual(DEFAULT_TIME_FORMAT_SETTINGS);
  });
});

describe('timeFormatPrefs (resolved, reactive)', () => {
  it('re-emits resolved seam prefs when the settings change', () => {
    expect(get(timeFormatPrefs)).toEqual({});
    const store = memStorage();
    connectOwnerTimeFormat('c'.repeat(32), store);
    setTimeFormatSettings({ clock: '24h', dateOrder: 'system' }, store);
    expect(get(timeFormatPrefs)).toEqual({ hour12: false });
  });
});
