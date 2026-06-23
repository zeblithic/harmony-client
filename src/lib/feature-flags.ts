/**
 * ZEB-544 — alpha surface gating: which top-level nav modes appear in the rail.
 *
 * The client ships seven `AppMode`s, but the alpha presents a focused
 * Communities-first surface, so several are hidden from the nav rail by default.
 * This module is the single source of truth for that gating, mirroring the thin,
 * try/catch-guarded localStorage pattern of `media-panel-prefs`. Defaults are
 * code-defined (so a fresh install is correctly scoped); a device-local override
 * lets a developer re-enable any mode without a rebuild.
 *
 * Gating is nav-rail visibility only — it does NOT remove any feature's code or
 * backend, and it does NOT touch contextual entry points (e.g. the Network health
 * view stays reachable from the connectivity-diagnostic affordance even when its
 * rail button is hidden).
 */
import type { AppMode } from './types';

/** localStorage key holding a JSON `Partial<Record<AppMode, boolean>>` override. */
const OVERRIDE_KEY = 'harmony-feature-flags';

/**
 * Alpha defaults — the focused Communities-first surface.
 * - `messages` (Communities / Friends / DMs / Notes), `vines`, `files` — shown.
 * - `spellbook` (broken in shipped builds: needs an absent sibling WASM module),
 *   `network` (dev/diagnostics rail), `mint` (deferred → FinanceStudio), and
 *   `mail` (secondary to Communities) — hidden.
 *
 * `messages` is the core home and is always enabled regardless of overrides.
 */
const NAV_MODE_DEFAULTS: Record<AppMode, boolean> = {
  messages: true,
  vines: true,
  files: true,
  spellbook: false,
  network: false,
  mint: false,
  mail: false,
};

/** The home mode can never be gated off — there must always be a way back. */
const ALWAYS_ENABLED: ReadonlySet<AppMode> = new Set<AppMode>(['messages']);

/**
 * Read the device-local override map. Returns an empty object for an unset,
 * malformed, or unavailable value — a tampered or corrupt entry can never hide
 * the whole rail; it simply falls back to the code defaults. Only known modes
 * with boolean values are accepted.
 */
function loadOverrides(): Partial<Record<AppMode, boolean>> {
  try {
    const raw = localStorage.getItem(OVERRIDE_KEY);
    if (raw === null) return {};
    const parsed: unknown = JSON.parse(raw);
    if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) return {};
    const out: Partial<Record<AppMode, boolean>> = {};
    for (const [mode, value] of Object.entries(parsed as Record<string, unknown>)) {
      if (mode in NAV_MODE_DEFAULTS && typeof value === 'boolean') {
        out[mode as AppMode] = value;
      }
    }
    return out;
  } catch {
    return {};
  }
}

/**
 * Whether a top-level mode should appear in the nav rail. `messages` is always
 * enabled; every other mode uses its code default unless a valid device-local
 * override flips it.
 */
export function isNavModeEnabled(mode: AppMode): boolean {
  if (ALWAYS_ENABLED.has(mode)) return true;
  return loadOverrides()[mode] ?? NAV_MODE_DEFAULTS[mode];
}

/**
 * Persist a device-local override for a mode (re-enable or force-hide), merged
 * into any existing overrides. No-op if localStorage is unavailable. Intended
 * for dev use / a future settings toggle; there is no UI for it yet.
 */
export function setNavModeOverride(mode: AppMode, enabled: boolean): void {
  try {
    const current = loadOverrides();
    current[mode] = enabled;
    localStorage.setItem(OVERRIDE_KEY, JSON.stringify(current));
  } catch {
    // localStorage unavailable (SSR / private-mode quota) — non-fatal.
  }
}
