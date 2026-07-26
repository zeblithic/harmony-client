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
 *
 * Mirrors `Layout.svelte`'s render condition, which is the only authority:
 *
 *   rightColumnVisible = isMessages && !collapsed && (wantSettings || …)
 *   showingSettings    = rightColumnVisible && wantSettings
 *
 * The `collapsed` term is load-bearing and was missed in the first cut of this
 * fix (Qodo, PR #553). Below the responsive breakpoint the whole right column
 * is hidden, so Settings does not render *even in messages mode with the flag
 * set* — and a predicate without `collapsed` reproduced the exact
 * `aria-pressed="true"` with no panel that this module exists to eliminate,
 * just on the width axis instead of the mode axis.
 *
 * `showSettings` remains the user's *intent* and deliberately survives a
 * collapse, so Settings appears when the window widens again (the behaviour
 * `handleExportRequested` already depends on). This function is the *actual*.
 * Never bind ARIA to the intent.
 */
export function isSettingsVisible(
  showSettings: boolean,
  appMode: AppMode,
  collapsed: boolean,
): boolean {
  return showSettings && appMode === SETTINGS_MODE && !collapsed;
}

/**
 * Next state for a gear press. Opening routes to the mode Settings lives in;
 * closing only applies when it is genuinely open, so a press in a non-messages
 * mode always *opens* (matching what the user sees) rather than silently
 * clearing a `showSettings` flag they were never shown.
 *
 * Deliberately keyed on mode alone, NOT on `isSettingsVisible`: while collapsed
 * nothing renders at any flag value, so consulting visibility would make the
 * gear unable to ever clear the intent. A press while collapsed therefore
 * toggles the intent, which is the most useful thing available — the panel
 * appears or not on the next widen.
 *
 * The caller must apply an `appMode` change through the app's canonical mode
 * transition rather than assigning it directly; see `toggleSettings` in
 * App.svelte.
 */
export function toggleSettingsState(
  showSettings: boolean,
  appMode: AppMode,
): { showSettings: boolean; appMode: AppMode } {
  if (showSettings && appMode === SETTINGS_MODE) {
    return { showSettings: false, appMode };
  }
  return { showSettings: true, appMode: SETTINGS_MODE };
}
