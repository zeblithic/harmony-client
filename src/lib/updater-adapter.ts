import { check, type Update } from "@tauri-apps/plugin-updater";

const DISMISSED_VERSION_KEY = "harmony.updater.dismissed_version";

/**
 * SemVer 2.0–aware compare. Returns >0 if a > b, <0 if a < b, 0 if equal.
 *
 * ZEB-328 PR #160 R3 (Greptile): plain `Intl.Collator` over the full string
 * misorders alpha-vs-stable. Per SemVer 2.0 §11, a stable version (no
 * pre-release suffix) is greater than ANY pre-release of the same release.
 * `Intl.Collator` would compare `"1.0.0"` < `"1.0.0-alpha.1"` because the
 * second string is longer — the opposite of correct. Bites us at the
 * alpha → stable transition: a user who dismissed `1.0.0-alpha.5` would
 * be incorrectly suppressed from seeing `1.0.0`.
 *
 * Strategy: split on the first `-`, compare the release parts first with
 * numeric collator (so `0.1.10 > 0.1.2`), then apply the §11 rule for
 * pre-release tiebreak.
 */
function semverCompare(a: string, b: string): number {
  const [aRelease, aPre] = a.split(/-(.+)/, 2);
  const [bRelease, bPre] = b.split(/-(.+)/, 2);
  const collator = new Intl.Collator(undefined, { numeric: true });
  const relCmp = collator.compare(aRelease, bRelease);
  if (relCmp !== 0) return relCmp;
  // Release parts equal — apply SemVer 2.0 §11: pre-release < stable.
  if (!aPre && bPre) return 1;
  if (aPre && !bPre) return -1;
  // Both have pre-release identifiers; numeric collate for alpha.10 > alpha.2.
  return collator.compare(aPre ?? "", bPre ?? "");
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
