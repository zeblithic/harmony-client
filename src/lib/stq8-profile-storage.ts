// Thin localStorage wrapper for the stq8 voice profile JSON produced by
// WasmPipeline.export_profile().
//
// The profile is opaque to us — a JSON blob with centroids and metadata
// the Rust classifier understands. We just read/write it as a string.
//
// localStorage can throw (storage disabled, quota exceeded, private
// browsing) and is absent in jsdom/SSR-shaped environments; every call
// guards against that and treats failures as "no profile available"
// rather than propagating.

const STORAGE_KEY = 'harmony.stq8.profile';

// `typeof localStorage` is a property read that runs through a host getter,
// so in restricted contexts (private browsing in some browsers, cross-origin
// sandboxed iframes, disabled site data) it can throw SecurityError rather
// than simply returning 'undefined'. Probe inside try/catch so every helper
// below can rely on this returning cleanly.
function hasLocalStorage(): boolean {
  try {
    return typeof window !== 'undefined' && typeof window.localStorage !== 'undefined';
  } catch {
    return false;
  }
}

/** Returns the stored profile JSON, or null if none present / storage unavailable. */
export function loadProfile(): string | null {
  if (!hasLocalStorage()) return null;
  try {
    return localStorage.getItem(STORAGE_KEY);
  } catch {
    return null;
  }
}

/**
 * Persist the profile JSON. Returns true on success; false when storage is
 * unavailable or setItem rejects (quota exceeded, SecurityError, etc.).
 *
 * Callers that surface a "Calibrated ✓" state should check this — an
 * in-memory-calibrated pipeline whose profile wasn't persisted will be lost
 * on reload, and the user deserves to know instead of being surprised next
 * boot.
 */
export function saveProfile(json: string): boolean {
  if (!hasLocalStorage()) return false;
  try {
    localStorage.setItem(STORAGE_KEY, json);
    return true;
  } catch (err) {
    console.warn('[harmony-client] failed to persist stq8 profile:', err);
    return false;
  }
}

/**
 * Forcibly remove the stored profile.
 *
 * Used by App.svelte's boot-time corrupt-profile recovery path — if
 * `importProfile` rejects a stored blob as malformed, we clear it so the
 * next boot doesn't re-hit the same poison. **Not** called from the
 * Recalibrate UX flow: recalibration leaves the old profile in place until
 * `saveProfile` atomically overwrites on successful finalize, so a
 * permission denial or mid-flow bail doesn't strand the user without a
 * calibration.
 */
export function clearProfile(): void {
  if (!hasLocalStorage()) return;
  try {
    localStorage.removeItem(STORAGE_KEY);
  } catch (err) {
    console.warn('[harmony-client] failed to clear stq8 profile:', err);
  }
}
