/**
 * ZEB-405 (WS-C) — right media-panel layout preference (device-local).
 *
 * The messages-mode media column is collapsed by default (opt-in reveal) and
 * resizable. This module persists that choice — open/closed and chosen width —
 * to localStorage, mirroring the thin, guarded pattern of `device-label-service`
 * and `NetworkApp`'s `network-viz-show-table`. It holds no network state and is
 * a single app-wide preference (not per-community).
 */

const OPEN_KEY = 'harmony-media-panel-open';
const WIDTH_KEY = 'harmony-media-panel-width';

/** Minimum panel width in px — narrow enough to reclaim space, wide enough to read. */
export const MEDIA_PANEL_MIN_WIDTH = 260;
/** Maximum panel width in px — keeps the message column usable on wide screens. */
export const MEDIA_PANEL_MAX_WIDTH = 600;
/** Default width when the panel is first revealed. */
export const MEDIA_PANEL_DEFAULT_WIDTH = 340;

function clampWidth(px: number): number {
  return Math.min(MEDIA_PANEL_MAX_WIDTH, Math.max(MEDIA_PANEL_MIN_WIDTH, Math.round(px)));
}

/** Whether the media panel should start open. Defaults to `false` (collapsed). */
export function loadMediaPanelOpen(): boolean {
  try {
    return localStorage.getItem(OPEN_KEY) === 'true';
  } catch {
    return false;
  }
}

/** Persist the open/closed choice. No-op if localStorage is unavailable. */
export function saveMediaPanelOpen(open: boolean): void {
  try {
    localStorage.setItem(OPEN_KEY, String(open));
  } catch {
    // localStorage unavailable (SSR / private-mode quota) — non-fatal.
  }
}

/**
 * The persisted panel width, clamped to `[MIN, MAX]`. Returns the default for an
 * unset, non-numeric, or unavailable value; clamps on load as well as save so a
 * stale or tampered value can never produce an unusable column.
 */
export function loadMediaPanelWidth(): number {
  try {
    const raw = localStorage.getItem(WIDTH_KEY);
    if (raw === null) return MEDIA_PANEL_DEFAULT_WIDTH;
    const n = Number(raw);
    if (!Number.isFinite(n)) return MEDIA_PANEL_DEFAULT_WIDTH;
    return clampWidth(n);
  } catch {
    return MEDIA_PANEL_DEFAULT_WIDTH;
  }
}

/** Persist the panel width, clamped to `[MIN, MAX]` and rounded to whole px. */
export function saveMediaPanelWidth(px: number): void {
  try {
    localStorage.setItem(WIDTH_KEY, String(clampWidth(px)));
  } catch {
    // localStorage unavailable — non-fatal.
  }
}
