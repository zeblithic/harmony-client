import { describe, expect, it } from 'vitest';
import type { AppMode } from '../types';
import {
  SETTINGS_MODE,
  isSettingsVisible,
  toggleSettingsState,
} from '../settings-visibility';

const NON_SETTINGS_MODES: AppMode[] = [
  'vines',
  'files',
  'spellbook',
  'mail',
  'mint',
  'network',
];

const EXPANDED = false;
const COLLAPSED = true;

describe('isSettingsVisible — ZEB-767', () => {
  it('is true only in the mode Settings renders in, on an expanded layout', () => {
    expect(isSettingsVisible(true, SETTINGS_MODE, EXPANDED)).toBe(true);
  });

  it.each(NON_SETTINGS_MODES)(
    'is false in %s mode even when showSettings is set',
    (mode) => {
      // The exact state the old gear reported as pressed: flag true, nothing rendered.
      expect(isSettingsVisible(true, mode, EXPANDED)).toBe(false);
    },
  );

  it('is false when the flag is clear', () => {
    expect(isSettingsVisible(false, SETTINGS_MODE, EXPANDED)).toBe(false);
  });

  // The width axis. Missed by the first cut of this fix, which checked mode
  // only — below the breakpoint Layout hides the entire right column, so the
  // predicate reported visible while nothing rendered. Same lie, other axis.
  it('is false on a collapsed layout even in the settings mode with the flag set', () => {
    expect(isSettingsVisible(true, SETTINGS_MODE, COLLAPSED)).toBe(false);
  });

  it.each(NON_SETTINGS_MODES)('is false collapsed in %s mode', (mode) => {
    expect(isSettingsVisible(true, mode, COLLAPSED)).toBe(false);
  });
});

describe('toggleSettingsState — ZEB-767', () => {
  it('opens and stays put when already in the settings mode', () => {
    expect(toggleSettingsState(false, SETTINGS_MODE)).toEqual({
      showSettings: true,
      appMode: SETTINGS_MODE,
    });
  });

  it('closes when Settings is genuinely open', () => {
    expect(toggleSettingsState(true, SETTINGS_MODE)).toEqual({
      showSettings: false,
      appMode: SETTINGS_MODE,
    });
  });

  it.each(NON_SETTINGS_MODES)('routes to the settings mode from %s', (mode) => {
    expect(toggleSettingsState(false, mode)).toEqual({
      showSettings: true,
      appMode: SETTINGS_MODE,
    });
  });

  it.each(NON_SETTINGS_MODES)(
    'opens rather than silently clearing a stale flag in %s',
    (mode) => {
      // Reachable via the pre-fix bug (a press in a non-messages mode left
      // showSettings true) and via handleExportRequested racing a mode change.
      // The user sees no panel, so a press must open — treating this as "close"
      // would make the first press appear to do nothing.
      expect(toggleSettingsState(true, mode)).toEqual({
        showSettings: true,
        appMode: SETTINGS_MODE,
      });
    },
  );

  it('still toggles the intent while collapsed, so the gear can clear it', () => {
    // Keyed on mode, not on isSettingsVisible: while collapsed nothing renders
    // at any flag value, so a visibility-keyed toggle could never turn the
    // intent back off and the flag would latch on.
    expect(toggleSettingsState(true, SETTINGS_MODE).showSettings).toBe(false);
    expect(toggleSettingsState(false, SETTINGS_MODE).showSettings).toBe(true);
  });
});

/*
 * The invariant "aria-pressed never disagrees with what rendered" is NOT
 * asserted here. It was, and the assertion restated this module's own predicate
 * as the definition of "rendered" — so when the predicate was wrong (missing
 * `collapsed`) the test agreed with it and passed. A guard that re-derives the
 * thing it is checking certifies the author's model, not the behaviour.
 *
 * It now lives in Layout.test.ts, where the oracle is the rendered DOM.
 */
