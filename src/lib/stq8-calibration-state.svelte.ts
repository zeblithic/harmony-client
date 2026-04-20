// Module-level reactive state for the most recent completed calibration
// save attempt. Lives outside CalibrationView so its outcome message
// survives component remount (tab switches, navigation within the
// Spellbook) — otherwise a per-instance derivation that falls back to
// `loadProfile() !== null` on remount collapses the save-failed-fallback
// case to 'saved', misleading the user about what will load on reload.
//
// Resets implicitly on page reload. That's the correct behavior: a reload
// re-instantiates the classifier from the persisted profile (or nothing),
// so the in-memory "last outcome" no longer applies and deriving from disk
// presence is accurate again.

export type SaveOutcome = 'saved' | 'save-failed-fallback' | 'save-failed-no-fallback';

export const calibrationSaveState = $state<{ lastOutcome: SaveOutcome | null }>({
  lastOutcome: null,
});
