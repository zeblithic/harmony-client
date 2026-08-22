// ZEB-943 — date-aware message timestamp formatting.
import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import {
  formatMessageTimestamp,
  formatFullTimestamp,
  formatClockTime,
  formatDateOnly,
  formatMailRecency,
  formatLastSeen,
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

// ZEB-952 — the mail recency label buckets by LOCAL CALENDAR DAY, not by elapsed
// 24h windows. The old `floor((now - ms) / 86400000)` math misfiled anything
// whose *elapsed* time disagreed with the calendar-day count (late-yesterday
// viewed early-today; a Sunday-to-Sunday span that stays under 7×24h).
describe('formatMailRecency', () => {
  const weekdayOf = (ms: number) => new Date(ms).toLocaleDateString('en-US', { weekday: 'short' });
  const monthDayOf = (ms: number) =>
    new Date(ms).toLocaleDateString('en-US', { month: 'short', day: 'numeric' });

  it('renders time-of-day for a same-local-day message, whatever the hours elapsed', () => {
    const now = at(2026, 7, 16, 23, 55);
    const ms = at(2026, 7, 16, 0, 5); // ~23h50m earlier, but the SAME local day
    expect(formatMailRecency(ms, now, prefs)).toBe(timeOf(ms));
  });

  it('honors the clock preference on the same-day branch', () => {
    const now = at(2026, 7, 16, 22, 14);
    const ms = at(2026, 7, 16, 20, 11);
    expect(formatMailRecency(ms, now, { locale: 'en-US', hour12: false })).toContain('20:11');
  });

  // THE regression: late-yesterday viewed early-today. Only ~11h elapsed, so the
  // old floor(elapsed/24h) put it in the "today" bucket (a bare clock time that
  // looks like it arrived today). The calendar-day rule shows the weekday.
  it('shows the weekday (not a bare time) for late-yesterday viewed early today', () => {
    const now = at(2026, 7, 16, 10, 0); // today 10:00
    const ms = at(2026, 7, 15, 23, 0); // yesterday 23:00 — 11h elapsed
    const out = formatMailRecency(ms, now, prefs);
    expect(out).toBe(weekdayOf(ms));
    expect(out).not.toBe(timeOf(ms));
  });

  it('shows the weekday up to 6 calendar days back', () => {
    const now = at(2026, 7, 16, 10, 0);
    const ms = at(2026, 7, 10, 8, 0); // 6 calendar days earlier
    expect(formatMailRecency(ms, now, prefs)).toBe(weekdayOf(ms));
  });

  it('falls back to month/day at 7 calendar days and beyond', () => {
    const now = at(2026, 7, 16, 10, 0);
    const ms = at(2026, 7, 9, 8, 0); // 7 calendar days earlier
    expect(formatMailRecency(ms, now, prefs)).toBe(monthDayOf(ms));
  });

  it('uses month/day when a <7×24h span still crosses into the 7th calendar day', () => {
    // now Aug 16 09:00, ms Aug 9 23:00 → 6d10h elapsed (old floor=6 → weekday,
    // and Aug 9 & Aug 16 are the SAME weekday, so the label would collide with
    // today). Calendar-day count is 7 → month/day.
    const now = at(2026, 7, 16, 9, 0);
    const ms = at(2026, 7, 9, 23, 0);
    expect(formatMailRecency(ms, now, prefs)).toBe(monthDayOf(ms));
  });
});

// A DST-observing zone makes a local calendar day 23h (spring-forward) or 25h
// (fall-back), so the underlying day diff must count LOCAL midnights and ROUND
// — a `floor` over raw elapsed ms misfiles the 23h day as 0 calendar days and
// would mislabel a 1-day-old message as month/day. Node re-reads `process.env.TZ`
// on every Date op, so pinning it here exercises the transition regardless of
// the CI runner's own zone. US 2026: spring-forward Sun 2026-03-08 (23h day),
// fall-back Sun 2026-11-01 (25h day).
describe('formatMailRecency — DST boundaries (round, not floor)', () => {
  const origTZ = process.env.TZ;
  beforeAll(() => {
    process.env.TZ = 'America/New_York';
  });
  afterAll(() => {
    if (origTZ === undefined) delete process.env.TZ;
    else process.env.TZ = origTZ;
  });

  const weekdayOf = (ms: number) => new Date(ms).toLocaleDateString('en-US', { weekday: 'short' });

  it('labels a previous-day message as the weekday across spring-forward (23h day)', () => {
    const ms = at(2026, 2, 8, 9, 0); // Sun 2026-03-08 — the 23h local day
    const now = at(2026, 2, 9, 10, 0); // Mon 2026-03-09
    // round(23h/24h) = 1 calendar day → weekday; a floor day-diff → 0 → month/day.
    expect(formatMailRecency(ms, now, prefs)).toBe(weekdayOf(ms));
  });

  it('labels a previous-day message as the weekday across fall-back (25h day)', () => {
    const ms = at(2026, 10, 1, 9, 0); // Sun 2026-11-01 — the 25h local day
    const now = at(2026, 10, 2, 10, 0); // Mon 2026-11-02
    expect(formatMailRecency(ms, now, prefs)).toBe(weekdayOf(ms));
  });
});

// ZEB-972 — shared heartbeat-tolerant "last seen" recency label, lifted from
// DevicesPanel so presence tooltips can reuse it. The `justNowUnderMin` floor
// is the honesty knob: DevicesPanel keeps the default 10 (fleet re-stamp
// cadence ~7.5 min), presence passes 1 (10 s beacons — a 2-minute-stale peer
// must not read "just now").
describe('formatLastSeen', () => {
  const now = at(2026, 7, 21, 12, 0);

  it('renders "just now" under the default 10-minute floor', () => {
    expect(formatLastSeen(now - 30_000, now, prefs)).toBe('just now');
    expect(formatLastSeen(now - 9 * 60_000, now, prefs)).toBe('just now');
  });

  it('approximate minute bucket past the floor, with ~ prefix', () => {
    expect(formatLastSeen(now - 15 * 60_000, now, prefs)).toBe('~15m ago');
  });

  it('a lowered floor exposes minute resolution (presence dots)', () => {
    expect(formatLastSeen(now - 2 * 60_000, now, prefs, { justNowUnderMin: 1 })).toBe('~2m ago');
    expect(formatLastSeen(now - 30_000, now, prefs, { justNowUnderMin: 1 })).toBe('just now');
  });

  it('hour and day buckets', () => {
    expect(formatLastSeen(now - 90 * 60_000, now, prefs)).toBe('~1h ago');
    expect(formatLastSeen(now - 26 * 3_600_000, now, prefs)).toBe('1d ago');
  });

  it('falls back to the numeric date at 30+ days', () => {
    const ms = now - 40 * 86_400_000;
    expect(formatLastSeen(ms, now, prefs)).toBe(formatDateOnly(ms, prefs));
  });
});
