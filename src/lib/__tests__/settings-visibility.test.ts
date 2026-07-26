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

describe('isSettingsVisible — ZEB-767', () => {
  it('is true only in the mode Settings actually renders in', () => {
    expect(isSettingsVisible(true, SETTINGS_MODE)).toBe(true);
  });

  it.each(NON_SETTINGS_MODES)(
    'is false in %s mode even when showSettings is set',
    (mode) => {
      // The exact state the old gear reported as pressed: the flag is true,
      // and Layout renders nothing.
      expect(isSettingsVisible(true, mode)).toBe(false);
    },
  );

  it('is false when the flag is clear', () => {
    expect(isSettingsVisible(false, SETTINGS_MODE)).toBe(false);
  });
});

describe('toggleSettingsState — ZEB-767', () => {
  it('opens and stays put when already in the settings mode', () => {
    expect(toggleSettingsState(false, SETTINGS_MODE)).toEqual({
      showSettings: true,
      appMode: SETTINGS_MODE,
    });
  });

  it('closes only when Settings is genuinely visible', () => {
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
});

describe('the invariant the fix exists to hold — ZEB-767', () => {
  const ALL_MODES: AppMode[] = [SETTINGS_MODE, ...NON_SETTINGS_MODES];

  it('never reports pressed without Settings being visible, from any state', () => {
    for (const mode of ALL_MODES) {
      for (const flag of [false, true]) {
        const next = toggleSettingsState(flag, mode);
        // `aria-pressed` binds to isSettingsVisible, so post-press the two must
        // agree by construction — there is no reachable state where the gear
        // announces an open panel that Layout did not render.
        const pressed = isSettingsVisible(next.showSettings, next.appMode);
        const rendered = next.showSettings && next.appMode === SETTINGS_MODE;
        expect(pressed).toBe(rendered);
      }
    }
  });
});
