// ZEB-943 — shared, date-aware timestamp formatting for message surfaces.
//
// Message feeds previously rendered time-of-day only (`08:11 AM`), so a message
// from days ago was byte-identical to one sent minutes ago. These helpers add
// the date once a message is no longer from "today" (local calendar day), and
// expose a full, unambiguous date+time for hover tooltips (`title`).
//
// Preferences seam (ZEB-943 → ZEB-944): every format decision flows from a
// single `TimeFormatPrefs` object rather than hardcoded Intl options, so the
// user-preferences layer (time-format-service.ts) wires a stored prefs object
// into these calls without touching any call site. `hour12` covers the 12h/24h
// axis. Date ordering has two modes: leave `dateOrder` undefined to follow the
// runtime locale (ZEB-943 default behavior), or set it explicitly.
//
// Why explicit date order (ZEB-944, CodeRabbit): `Intl.DateTimeFormat` numeric
// date order is implementation-defined — ICU/CLDR data differs across runtimes
// and WebView versions (e.g. en-CA has flipped between YYYY-MM-DD and M/D/YYYY),
// so a curated-locale mapping could silently render the wrong order. When
// `dateOrder` is set, the date is assembled from the raw local parts and is
// therefore deterministic across every runtime.

export interface TimeFormatPrefs {
  /** Force a 12h (`true`) or 24h (`false`) clock. `undefined` follows the locale. */
  hour12?: boolean;
  /** BCP-47 locale tag(s). `undefined` follows the runtime default. */
  locale?: string | string[];
  /**
   * Explicit numeric date-field order, assembled deterministically from the
   * local date parts (locale-independent). `undefined` follows the locale.
   *  - `mdy` → `8/14`  · `8/14/25`      · `8/14/2026`
   *  - `dmy` → `14/8`  · `14/8/25`      · `14/8/2026`
   *  - `ymd` → `08-14` · `25-08-14`     · `2026-08-14`
   */
  dateOrder?: 'mdy' | 'dmy' | 'ymd';
}

export const DEFAULT_TIME_FORMAT_PREFS: TimeFormatPrefs = {};

/** How the year is rendered in a date string (or omitted entirely). */
type YearMode = 'none' | '2-digit' | 'numeric';

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

function pad2(n: number): string {
  return String(n).padStart(2, '0');
}

/** Deterministic, locale-INDEPENDENT numeric date in an explicit field order.
 *  Assembled from raw local parts so it never depends on ICU/CLDR locale data
 *  (see the header note on en-CA). */
function orderedDate(date: Date, order: 'mdy' | 'dmy' | 'ymd', year: YearMode): string {
  const m = date.getMonth() + 1;
  const d = date.getDate();
  const yFull = date.getFullYear();
  const y = year === '2-digit' ? pad2(yFull % 100) : String(yFull);
  if (order === 'ymd') {
    const md = `${pad2(m)}-${pad2(d)}`;
    return year === 'none' ? md : `${y}-${md}`;
  }
  const md = order === 'mdy' ? `${m}/${d}` : `${d}/${m}`;
  return year === 'none' ? md : `${md}/${y}`;
}

/** Locale-native numeric date — the `system` (undefined `dateOrder`) path,
 *  unchanged from ZEB-943: field order and separators follow the locale. */
function localeDate(date: Date, prefs: TimeFormatPrefs, year: YearMode): string {
  const opts: Intl.DateTimeFormatOptions = {
    month: 'numeric',
    day: 'numeric',
    ...(year === 'none' ? {} : { year }),
  };
  return date.toLocaleDateString(localeArg(prefs), opts);
}

function dateString(date: Date, prefs: TimeFormatPrefs, year: YearMode): string {
  return prefs.dateOrder === undefined
    ? localeDate(date, prefs, year)
    : orderedDate(date, prefs.dateOrder, year);
}

/**
 * Bare time-of-day (`08:11 AM` / `20:11`) honoring the clock preference. For
 * standalone time labels that never carry a date — e.g. the "Seen HH:MM" read
 * receipt — so they track the 12h/24h choice like the message timestamp does.
 */
export function formatClockTime(
  ms: number,
  prefs: TimeFormatPrefs = DEFAULT_TIME_FORMAT_PREFS,
): string {
  return new Date(ms).toLocaleTimeString(localeArg(prefs), timeOptions(prefs));
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
  const year: YearMode = date.getFullYear() === nowDate.getFullYear() ? 'none' : '2-digit';
  return `${dateString(date, prefs, year)}, ${time}`;
}

/** Full, unambiguous date + time (4-digit year) for a hover tooltip / `title`. */
export function formatFullTimestamp(
  ms: number,
  prefs: TimeFormatPrefs = DEFAULT_TIME_FORMAT_PREFS,
): string {
  const date = new Date(ms);
  if (prefs.dateOrder === undefined) {
    // Locale-native combined date+time — unchanged ZEB-943 behavior.
    return date.toLocaleString(localeArg(prefs), {
      year: 'numeric',
      month: 'numeric',
      day: 'numeric',
      ...timeOptions(prefs),
    });
  }
  const time = date.toLocaleTimeString(localeArg(prefs), timeOptions(prefs));
  return `${orderedDate(date, prefs.dateOrder, 'numeric')}, ${time}`;
}
