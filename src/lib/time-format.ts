// ZEB-943 — shared, date-aware timestamp formatting for message surfaces.
//
// Message feeds previously rendered time-of-day only (`08:11 AM`), so a message
// from days ago was byte-identical to one sent minutes ago. These helpers add
// the date once a message is no longer from "today" (local calendar day), and
// expose a full, unambiguous date+time for hover tooltips (`title`).
//
// Preferences seam (ZEB-943 follow-up): every format decision flows from a
// single `TimeFormatPrefs` object rather than hardcoded Intl options, so a
// future user-preferences ticket (24h clock, explicit locale/day-month order)
// wires a stored prefs object into these two calls without touching any call
// site. Locale already drives day/month ordering for free — an en-GB user sees
// `14/8` today with zero code change.

export interface TimeFormatPrefs {
  /** Force a 12h (`true`) or 24h (`false`) clock. `undefined` follows the locale. */
  hour12?: boolean;
  /** BCP-47 locale tag(s). `undefined` follows the runtime default. */
  locale?: string | string[];
}

export const DEFAULT_TIME_FORMAT_PREFS: TimeFormatPrefs = {};

function localeArg(prefs: TimeFormatPrefs): string | string[] {
  return prefs.locale ?? [];
}

function timeOptions(prefs: TimeFormatPrefs): Intl.DateTimeFormatOptions {
  return {
    hour: '2-digit',
    minute: '2-digit',
    ...(prefs.hour12 === undefined ? {} : { hour12: prefs.hour12 }),
  };
}

function sameLocalDay(a: Date, b: Date): boolean {
  return (
    a.getFullYear() === b.getFullYear() &&
    a.getMonth() === b.getMonth() &&
    a.getDate() === b.getDate()
  );
}

/**
 * Inline label for a message timestamp:
 *  - same local day as `now` → bare time (`08:11 AM`)
 *  - other day, same year    → `8/14, 08:11 AM`
 *  - other calendar year     → `8/14/25, 08:11 AM`
 *
 * `now` is injected (not read from the clock) so callers own the reference
 * instant and unit tests stay deterministic.
 */
export function formatMessageTimestamp(
  ms: number,
  now: number,
  prefs: TimeFormatPrefs = DEFAULT_TIME_FORMAT_PREFS,
): string {
  const date = new Date(ms);
  const nowDate = new Date(now);
  const time = date.toLocaleTimeString(localeArg(prefs), timeOptions(prefs));
  if (sameLocalDay(date, nowDate)) return time;
  const dateOpts: Intl.DateTimeFormatOptions = {
    month: 'numeric',
    day: 'numeric',
    ...(date.getFullYear() === nowDate.getFullYear() ? {} : { year: '2-digit' }),
  };
  const day = date.toLocaleDateString(localeArg(prefs), dateOpts);
  return `${day}, ${time}`;
}

/** Full, unambiguous date + time (4-digit year) for a hover tooltip / `title`. */
export function formatFullTimestamp(
  ms: number,
  prefs: TimeFormatPrefs = DEFAULT_TIME_FORMAT_PREFS,
): string {
  return new Date(ms).toLocaleString(localeArg(prefs), {
    year: 'numeric',
    month: 'numeric',
    day: 'numeric',
    ...timeOptions(prefs),
  });
}
