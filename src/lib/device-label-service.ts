/**
 * ZEB-336 — per-device label store (owner-private).
 *
 * Distinct from the owner display name (which lives in `profile-service` and
 * is broadcast owner-canonically by `profile_card_broadcast`). A device label
 * names THIS machine ("KRILE") and, in Phase 1, never leaves the device.
 * Mirrors `profile-service`'s localStorage pattern. The persistence interface
 * is deliberately thin so Phase 2 can sync the roster across the owner's own
 * devices without touching callers.
 */

const STORAGE_KEY = 'harmony-device-label';

/** The stored per-device label, or null if none has been set/defaulted yet. */
export function loadDeviceLabel(): string | null {
  try {
    const v = localStorage.getItem(STORAGE_KEY);
    return v && v.trim() ? v : null;
  } catch {
    return null;
  }
}

/** Persist the per-device label. No-op for empty / whitespace-only input. */
export function saveDeviceLabel(label: string): void {
  const trimmed = label.trim();
  if (!trimmed) return;
  try {
    localStorage.setItem(STORAGE_KEY, trimmed);
  } catch {
    // localStorage unavailable (SSR / private-mode quota) — non-fatal.
  }
}

/**
 * A default label derived from the OS hostname, falling back to "This device"
 * when the hostname is unavailable (not in Tauri, plugin error, null result,
 * or `os:allow-hostname` not granted). Read-only: callers decide whether to
 * persist the result.
 */
export async function resolveDefaultDeviceLabel(): Promise<string> {
  try {
    const { hostname } = await import('@tauri-apps/plugin-os');
    const h = await hostname();
    if (h && h.trim()) return h.trim();
  } catch {
    // Not in Tauri, or os:allow-hostname not granted — use the fallback.
  }
  return 'This device';
}
