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

function hasLocalStorage(): boolean {
  return typeof localStorage !== 'undefined';
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

/** Persist the profile JSON. Silent no-op if storage is unavailable. */
export function saveProfile(json: string): void {
  if (!hasLocalStorage()) return;
  try {
    localStorage.setItem(STORAGE_KEY, json);
  } catch (err) {
    console.warn('[harmony-client] failed to persist stq8 profile:', err);
  }
}

/** Wipe the stored profile (used by "Recalibrate" flow). */
export function clearProfile(): void {
  if (!hasLocalStorage()) return;
  try {
    localStorage.removeItem(STORAGE_KEY);
  } catch (err) {
    console.warn('[harmony-client] failed to clear stq8 profile:', err);
  }
}
