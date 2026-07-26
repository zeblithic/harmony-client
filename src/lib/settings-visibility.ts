import type { AppMode } from './types';

/**
 * ZEB-767 — the left-rail Settings gear.
 *
 * Settings renders only inside the messages layout: `Layout.svelte` gates it on
 * `isMessages && showSettings && !!settingsPanel`. The gear, however, is
 * rendered in *every* mode, and used to toggle `showSettings` unconditionally.
 *
 * So `showSettings` alone never meant "Settings is on screen", and driving the
 * gear's `class:active` / `aria-pressed` from it produced two distinct faults in
 * the five non-messages modes:
 *
 *  - a dead control — clicking it did nothing visible;
 *  - an accessibility lie — it reported `aria-pressed="true"` with no panel,
 *    telling a screen-reader user Settings had opened when it had not. That is
 *    the worse half: a control that *announces success* is harder to recover
 *    from than one that is merely inert.
 *
 * These two functions are the honest predicate and the corresponding action.
 * They live in a module rather than inline in `App.svelte` so the invariant
 * "`aria-pressed` never disagrees with what is on screen" can be pinned by
 * tests — it is a correctness claim, not a styling preference.
 *
 * `handleExportRequested` already performed this same mode-switch by hand
 * before opening Settings → Account, which was the standing evidence that
 * callers should not have to know where Settings lives.
 */

/** The mode Settings is rendered inside. */
export const SETTINGS_MODE: AppMode = 'messages';

/**
 * Whether Settings is actually on screen — the value the gear's active and
 * `aria-pressed` states must be derived from.
 */
export function isSettingsVisible(showSettings: boolean, appMode: AppMode): boolean {
  return showSettings && appMode === SETTINGS_MODE;
}

/**
 * Next state for a gear press. Opening routes to the mode Settings lives in;
 * closing only applies when it is genuinely open, so a press in a non-messages
 * mode always *opens* (matching what the user sees) rather than silently
 * clearing a `showSettings` flag they were never shown.
 */
export function toggleSettingsState(
  showSettings: boolean,
  appMode: AppMode,
): { showSettings: boolean; appMode: AppMode } {
  if (isSettingsVisible(showSettings, appMode)) {
    return { showSettings: false, appMode };
  }
  return { showSettings: true, appMode: SETTINGS_MODE };
}
