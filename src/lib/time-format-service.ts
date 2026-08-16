// ZEB-944 — owner-scoped time & date format preferences.
//
// Follow-up to ZEB-943: `time-format.ts` exposes a swappable `TimeFormatPrefs`
// seam ({ hour12?, locale?, dateOrder? }) at its format functions. This module owns the
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

/**
 * Resolve the user-facing two-axis model into the `time-format.ts` seam's
 * `TimeFormatPrefs`. The axes are orthogonal by construction: the clock maps to
 * `hour12` and the date order maps to the explicit, locale-independent
 * `dateOrder` field — neither touches `locale`, so choosing a date order can
 * never affect the clock. Both axes on `system` resolve to `{}` — byte-identical
 * to the ZEB-943 default (follow the runtime locale for everything).
 */
export function resolveTimeFormatPrefs(settings: TimeFormatSettings): TimeFormatPrefs {
  const prefs: TimeFormatPrefs = {};
  if (settings.clock === '12h') prefs.hour12 = true;
  else if (settings.clock === '24h') prefs.hour12 = false;
  if (settings.dateOrder !== 'system') prefs.dateOrder = settings.dateOrder;
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
