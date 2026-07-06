// ZEB-605: follow-system theme with an owner-scoped manual override.
//
// Persistence is two keys:
//  - `harmony-theme:owner-<ownerId>` — the preference (source of truth). Owner-
//    scoped per the ZEB-586 pattern (see profile-service.ts): WebView
//    localStorage is bundle-scoped, not profile-scoped, so a fixed key would
//    leak the preference across identities on the same box.
//  - `harmony-theme:last-applied` — the last RESOLVED theme, deliberately
//    device-scoped: owner state arrives post-first-paint (get_owner_state),
//    so the next launch needs a pre-paint hint to avoid flashing the default.
import { writable, type Readable, type Writable } from 'svelte/store';

export type ThemePreference = 'system' | 'light' | 'dark';
export type ResolvedTheme = 'light' | 'dark';

export const THEME_APPLIED_EVENT = 'harmony:theme-applied';

const PREF_KEY_BASE = 'harmony-theme';
const LAST_APPLIED_KEY = 'harmony-theme:last-applied';
const DARK_QUERY = '(prefers-color-scheme: dark)';

function ownerKey(ownerId: string): string {
  return `${PREF_KEY_BASE}:owner-${ownerId}`;
}

function isPreference(v: unknown): v is ThemePreference {
  return v === 'system' || v === 'light' || v === 'dark';
}

function systemTheme(): ResolvedTheme {
  try {
    return window.matchMedia(DARK_QUERY).matches ? 'dark' : 'light';
  } catch {
    return 'light';
  }
}

export function resolveTheme(pref: ThemePreference): ResolvedTheme {
  return pref === 'system' ? systemTheme() : pref;
}

const preferenceWritable: Writable<ThemePreference> = writable('system');
let connectedOwnerId: string | null = null;
let currentPreference: ThemePreference = 'system';
let mediaCleanup: (() => void) | null = null;

function applyResolved(theme: ResolvedTheme): void {
  document.documentElement.dataset.theme = theme;
  try {
    localStorage.setItem(LAST_APPLIED_KEY, theme);
  } catch {
    // Sandboxed WebView storage failure is non-fatal; the theme still applies.
  }
  document.dispatchEvent(new CustomEvent(THEME_APPLIED_EVENT, { detail: theme }));
}

function applyPreference(pref: ThemePreference): void {
  currentPreference = pref;
  preferenceWritable.set(pref);
  applyResolved(resolveTheme(pref));
}

/** Install the prefers-color-scheme change listener once. The handler
 *  re-applies only while the effective preference is 'system', so explicit
 *  owner preferences always win. Called from BOTH entry paths: pre-paint
 *  (so the hint-only network window live-follows the OS — PR #407 Qodo R1)
 *  and owner connect. */
function ensureSystemFollowListener(): void {
  if (mediaCleanup !== null) {
    return;
  }
  try {
    const mql = window.matchMedia(DARK_QUERY);
    const onChange = () => {
      if (currentPreference === 'system') {
        applyResolved(systemTheme());
      }
    };
    mql.addEventListener('change', onChange);
    mediaCleanup = () => mql.removeEventListener('change', onChange);
  } catch {
    mediaCleanup = null;
  }
}

/** Pre-paint apply. Called from main.ts / network-main.ts BEFORE mount(). */
export function initThemePrePaint(): void {
  let hint: string | null = null;
  try {
    hint = localStorage.getItem(LAST_APPLIED_KEY);
  } catch {
    hint = null;
  }
  applyResolved(hint === 'dark' || hint === 'light' ? hint : systemTheme());
  ensureSystemFollowListener();
}

/** Owner identity resolved: load the owner's preference and start following
 *  the system while (and only while) the preference is 'system'. */
export function connectOwnerTheme(ownerId: string): void {
  connectedOwnerId = ownerId;
  let stored: string | null = null;
  try {
    stored = localStorage.getItem(ownerKey(ownerId));
  } catch {
    stored = null;
  }
  applyPreference(isPreference(stored) ? stored : 'system');
  ensureSystemFollowListener();
}

/** Settings entry point. Persists only when an owner is connected (matching
 *  the loadProfile no-owner contract). */
export function setThemePreference(pref: ThemePreference): void {
  if (connectedOwnerId !== null) {
    try {
      localStorage.setItem(ownerKey(connectedOwnerId), pref);
    } catch {
      // non-fatal
    }
  }
  applyPreference(pref);
}

export const themePreference: Readable<ThemePreference> = preferenceWritable;

/** Test-only reset. */
export function _resetThemeServiceForTest(): void {
  connectedOwnerId = null;
  currentPreference = 'system';
  preferenceWritable.set('system');
  if (mediaCleanup !== null) {
    mediaCleanup();
    mediaCleanup = null;
  }
}
