// ZEB-944 — owner-scoped time & date format preferences.
//
// Follow-up to ZEB-943: `time-format.ts` exposes a swappable `TimeFormatPrefs`
// seam ({ hour12?, locale? }) at both format functions. This module owns the
// *user-facing* preference model — two independent axes, a clock (12h/24h) and
// a date order (M/D · D/M · ISO) — persists it per-owner (the ZEB-586 pattern,
// mirroring theme-service.ts so switching identity never leaks a preference),
// and exposes a reactive `timeFormatPrefs` store that resolves the model into
// the seam's `TimeFormatPrefs` for the message surfaces to consume as
// `$timeFormatPrefs` (the same singleton-store idiom they already use for
// `dayClock`).
import { writable, derived, type Readable, type Writable } from 'svelte/store';
import type { TimeFormatPrefs } from './time-format';

/** Clock axis: follow the locale's convention, or force a 12h / 24h clock. */
export type ClockPref = 'system' | '12h' | '24h';
/** Date-order axis: follow the runtime locale, or a curated field ordering. */
export type DateOrderPref = 'system' | 'mdy' | 'dmy' | 'ymd';

export interface TimeFormatSettings {
  clock: ClockPref;
  dateOrder: DateOrderPref;
}

export const DEFAULT_TIME_FORMAT_SETTINGS: TimeFormatSettings = {
  clock: 'system',
  dateOrder: 'system',
};

const PREF_KEY_BASE = 'harmony-time-format';

/** Owner-scoped localStorage key (ZEB-586 pattern; see theme-service.ts). */
export function ownerTimeFormatKey(ownerId: string): string {
  return `${PREF_KEY_BASE}:owner-${ownerId}`;
}

function isClockPref(v: unknown): v is ClockPref {
  return v === 'system' || v === '12h' || v === '24h';
}

function isDateOrderPref(v: unknown): v is DateOrderPref {
  return v === 'system' || v === 'mdy' || v === 'dmy' || v === 'ymd';
}

/** Curated locale whose numeric date rendering yields the requested field
 *  order. `system` → undefined (follow the runtime locale — current behavior). */
function dateOrderLocale(order: DateOrderPref): string | undefined {
  switch (order) {
    case 'mdy':
      return 'en-US'; // 8/14
    case 'dmy':
      return 'en-GB'; // 14/8
    case 'ymd':
      return 'en-CA'; // 2026-08-14
    case 'system':
      return undefined;
  }
}

/** The runtime locale's own 12h/24h convention, resolved explicitly so a chosen
 *  date-order locale can't silently flip the clock (see resolveTimeFormatPrefs). */
function systemHour12(): boolean {
  try {
    return (
      new Intl.DateTimeFormat(undefined, { hour: 'numeric' }).resolvedOptions().hour12 ?? false
    );
  } catch {
    return false;
  }
}

/**
 * Resolve the user-facing two-axis model into the `time-format.ts` seam's
 * `TimeFormatPrefs`.
 *
 * Decoupling guard: the two axes are independent *to the user*, but both the
 * clock convention and the date order flow through Intl locale resolution. If
 * the user leaves the clock on 'system' yet picks a date order whose locale has
 * a different clock default (e.g. en-GB → 24h), that date choice would silently
 * change the clock. So whenever a date-order locale is set AND the clock is
 * 'system', we pin `hour12` to the real system convention. When both axes are
 * 'system' we return {} — byte-identical to today's default behavior.
 *
 * `systemHour12Fn` is injectable for deterministic tests.
 */
export function resolveTimeFormatPrefs(
  settings: TimeFormatSettings,
  systemHour12Fn: () => boolean = systemHour12,
): TimeFormatPrefs {
  const locale = dateOrderLocale(settings.dateOrder);
  let hour12: boolean | undefined =
    settings.clock === '12h' ? true : settings.clock === '24h' ? false : undefined;
  if (hour12 === undefined && locale !== undefined) {
    hour12 = systemHour12Fn();
  }
  const prefs: TimeFormatPrefs = {};
  if (hour12 !== undefined) prefs.hour12 = hour12;
  if (locale !== undefined) prefs.locale = locale;
  return prefs;
}

function defaultStorage(): Storage | null {
  try {
    return typeof localStorage !== 'undefined' ? localStorage : null;
  } catch {
    return null;
  }
}

function parseSettings(raw: string | null): TimeFormatSettings {
  if (!raw) return { ...DEFAULT_TIME_FORMAT_SETTINGS };
  try {
    const obj = JSON.parse(raw) as Record<string, unknown>;
    return {
      clock: isClockPref(obj.clock) ? obj.clock : 'system',
      dateOrder: isDateOrderPref(obj.dateOrder) ? obj.dateOrder : 'system',
    };
  } catch {
    return { ...DEFAULT_TIME_FORMAT_SETTINGS };
  }
}

const settingsWritable: Writable<TimeFormatSettings> = writable({
  ...DEFAULT_TIME_FORMAT_SETTINGS,
});
let connectedOwnerId: string | null = null;

/** The user-facing settings model (for the Settings UI to bind). */
export const timeFormatSettings: Readable<TimeFormatSettings> = settingsWritable;

/** The RESOLVED seam prefs (for message surfaces to pass to time-format.ts).
 *  Reactive: re-emits whenever the settings change, so every surface restyles
 *  live when the user flips a control. */
export const timeFormatPrefs: Readable<TimeFormatPrefs> = derived(settingsWritable, (s) =>
  resolveTimeFormatPrefs(s),
);

/** Owner identity resolved: load that owner's persisted settings (or defaults). */
export function connectOwnerTimeFormat(
  ownerId: string,
  storage: Storage | null = defaultStorage(),
): void {
  connectedOwnerId = ownerId;
  let raw: string | null = null;
  if (storage) {
    try {
      raw = storage.getItem(ownerTimeFormatKey(ownerId));
    } catch {
      raw = null;
    }
  }
  settingsWritable.set(parseSettings(raw));
}

/** Settings entry point. Persists only when an owner is connected (matching the
 *  theme-service no-owner contract), then updates the store. */
export function setTimeFormatSettings(
  next: TimeFormatSettings,
  storage: Storage | null = defaultStorage(),
): void {
  if (connectedOwnerId !== null && storage) {
    try {
      storage.setItem(ownerTimeFormatKey(connectedOwnerId), JSON.stringify(next));
    } catch {
      // Sandboxed WebView / quota failure is non-fatal; the choice still applies.
    }
  }
  settingsWritable.set(next);
}

/** Test-only reset. */
export function _resetTimeFormatServiceForTest(): void {
  connectedOwnerId = null;
  settingsWritable.set({ ...DEFAULT_TIME_FORMAT_SETTINGS });
}
