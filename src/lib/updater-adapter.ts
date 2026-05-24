import { check, type Update } from "@tauri-apps/plugin-updater";

const DISMISSED_VERSION_KEY = "harmony.updater.dismissed_version";

/**
 * SemVer-style numeric compare. Returns >0 if a > b, <0 if a < b, 0 if equal.
 * Uses Intl.Collator with numeric:true so `alpha.10` correctly compares as
 * greater than `alpha.2`. Sufficient for our alpha.N pre-release scheme.
 */
function semverCompare(a: string, b: string): number {
  const collator = new Intl.Collator(undefined, { numeric: true });
  return collator.compare(a, b);
}

/**
 * Check the configured updater endpoint. Returns the Update object when a
 * newer-than-current AND newer-than-dismissed version is available; null
 * otherwise. Never throws — network/parse failures log a warning and return
 * null so app startup is not blocked by updater issues.
 */
export async function checkForUpdate(): Promise<Update | null> {
  let update: Update | null = null;
  try {
    update = await check();
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    console.warn(`[updater] check failed: ${msg}`);
    return null;
  }

  if (!update || !update.available) {
    return null;
  }

  const dismissed = localStorage.getItem(DISMISSED_VERSION_KEY);
  if (dismissed && semverCompare(update.version, dismissed) <= 0) {
    return null;
  }

  return update;
}

/** Persist a per-version "don't bother me about this version" decision. */
export function dismissVersion(version: string): void {
  localStorage.setItem(DISMISSED_VERSION_KEY, version);
}
