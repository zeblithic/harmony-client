// ZEB-943 — date-aware message timestamp formatting.
import { describe, it, expect } from 'vitest';
import {
  formatMessageTimestamp,
  formatFullTimestamp,
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
});
