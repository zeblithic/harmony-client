// ZEB-943 — date-aware message timestamp formatting.
import { describe, it, expect } from 'vitest';
import {
  formatMessageTimestamp,
  formatFullTimestamp,
  formatClockTime,
  formatDateOnly,
  type TimeFormatPrefs,
} from './time-format';

// Pin the locale so assertions are deterministic regardless of the CI runner's
// default. Build every timestamp with the LOCAL-time Date constructor and
// derive expected time strings from the same Intl call the helper uses, so the
// tests are timezone-robust (and survive ICU's narrow-no-break-space before
// AM/PM).
const prefs: TimeFormatPrefs = { locale: 'en-US' };

/** ms for a given LOCAL wall-clock instant (month is 0-indexed). */
function at(y: number, mo: number, d: number, h: number, mi: number): number {
  return new Date(y, mo, d, h, mi).getTime();
}

function timeOf(ms: number, hour12?: boolean): string {
  return new Date(ms).toLocaleTimeString('en-US', {
    hour: '2-digit',
    minute: '2-digit',
    ...(hour12 === undefined ? {} : { hour12 }),
  });
}

describe('formatMessageTimestamp', () => {
  it('renders bare time (no date) when the message is from the same local day', () => {
    const now = at(2026, 7, 16, 22, 14);
    const ms = at(2026, 7, 16, 8, 11);
    const out = formatMessageTimestamp(ms, now, prefs);
    expect(out).toBe(timeOf(ms));
    expect(out).not.toContain('/');
  });

  it('prepends a compact M/D date for a different day in the same year', () => {
    const now = at(2026, 7, 16, 22, 14);
    const ms = at(2026, 7, 14, 8, 11);
    expect(formatMessageTimestamp(ms, now, prefs)).toBe(`8/14, ${timeOf(ms)}`);
  });

  it('adds a 2-digit year when the message is from a different calendar year', () => {
    const now = at(2026, 7, 16, 22, 14);
    const ms = at(2025, 7, 14, 8, 11);
    expect(formatMessageTimestamp(ms, now, prefs)).toBe(`8/14/25, ${timeOf(ms)}`);
  });

  it('treats the local-midnight boundary as a day change (just-before vs just-after)', () => {
    const now = at(2026, 7, 16, 0, 5);
    const ms = at(2026, 7, 15, 23, 55);
    expect(formatMessageTimestamp(ms, now, prefs)).toContain('8/15');
  });

  it('honors hour12=false for a 24-hour clock (preferences seam)', () => {
    const now = at(2026, 7, 16, 22, 14);
    const ms = at(2026, 7, 16, 20, 11);
    const out = formatMessageTimestamp(ms, now, { locale: 'en-US', hour12: false });
    expect(out).toContain('20:11');
  });
});

describe('formatFullTimestamp', () => {
  it('gives an unambiguous date + time with a 4-digit year', () => {
    const ms = at(2026, 7, 14, 8, 11);
    const out = formatFullTimestamp(ms, prefs);
    expect(out).toContain('8/14/2026');
    expect(out).toContain(timeOf(ms));
  });

  it('renders the explicit date order with a 4-digit year (dateOrder set)', () => {
    const ms = at(2026, 7, 14, 20, 11);
    expect(formatFullTimestamp(ms, { dateOrder: 'mdy' })).toMatch(/^8\/14\/2026, /);
    expect(formatFullTimestamp(ms, { dateOrder: 'dmy' })).toMatch(/^14\/8\/2026, /);
    expect(formatFullTimestamp(ms, { dateOrder: 'ymd' })).toMatch(/^2026-08-14, /);
  });
});

// ZEB-944 — explicit date order is assembled from raw local parts, so its
// output is deterministic across runtimes (no dependency on Intl locale data).
describe('formatMessageTimestamp — explicit dateOrder', () => {
  const now = at(2026, 7, 16, 22, 14); // 2026-08-16

  it('orders a same-year date per the dateOrder pref (year dropped)', () => {
    const ms = at(2026, 7, 14, 8, 11); // 2026-08-14, different day, same year
    expect(formatMessageTimestamp(ms, now, { dateOrder: 'mdy' })).toMatch(/^8\/14, /);
    expect(formatMessageTimestamp(ms, now, { dateOrder: 'dmy' })).toMatch(/^14\/8, /);
    expect(formatMessageTimestamp(ms, now, { dateOrder: 'ymd' })).toMatch(/^08-14, /);
  });

  it('adds a 2-digit year per the dateOrder pref for a different year', () => {
    const ms = at(2025, 7, 14, 8, 11); // 2025-08-14, different calendar year
    expect(formatMessageTimestamp(ms, now, { dateOrder: 'mdy' })).toMatch(/^8\/14\/25, /);
    expect(formatMessageTimestamp(ms, now, { dateOrder: 'dmy' })).toMatch(/^14\/8\/25, /);
    expect(formatMessageTimestamp(ms, now, { dateOrder: 'ymd' })).toMatch(/^25-08-14, /);
  });

  it('still drops the date entirely for a same-day message', () => {
    const ms = at(2026, 7, 16, 20, 11);
    const out = formatMessageTimestamp(ms, now, { dateOrder: 'ymd', locale: 'en-US' });
    expect(out).not.toContain('-');
    expect(out).toBe(timeOf(ms));
  });
});

describe('formatDateOnly', () => {
  it('renders a full numeric date only, honoring the date-order preference', () => {
    const ms = at(2026, 7, 14, 20, 11);
    expect(formatDateOnly(ms, { locale: 'en-US' })).toBe('8/14/2026'); // system → locale
    expect(formatDateOnly(ms, { dateOrder: 'mdy' })).toBe('8/14/2026');
    expect(formatDateOnly(ms, { dateOrder: 'dmy' })).toBe('14/8/2026');
    expect(formatDateOnly(ms, { dateOrder: 'ymd' })).toBe('2026-08-14');
  });

  it('carries no time-of-day', () => {
    const ms = at(2026, 7, 14, 20, 11);
    expect(formatDateOnly(ms, { dateOrder: 'ymd' })).not.toContain(':');
  });
});

describe('formatClockTime', () => {
  it('renders bare time-of-day honoring the clock preference', () => {
    const ms = at(2026, 7, 16, 20, 11);
    expect(formatClockTime(ms, { locale: 'en-US', hour12: false })).toContain('20:11');
    expect(formatClockTime(ms, { locale: 'en-US', hour12: true })).toContain('08:11');
    expect(formatClockTime(ms, prefs)).not.toContain('/'); // never carries a date
  });
});
