import { check, type Update } from "@tauri-apps/plugin-updater";

const DISMISSED_VERSION_KEY = "harmony.updater.dismissed_version";

/**
 * SemVer 2.0–aware compare. Returns >0 if a > b, <0 if a < b, 0 if equal.
 *
 * ZEB-328 PR #160 R3 (Greptile): plain `Intl.Collator` over the full string
 * misorders alpha-vs-stable. Per SemVer 2.0 §11, a stable version (no
 * pre-release suffix) is greater than ANY pre-release of the same release.
 *
 * ZEB-328 PR #160 R4 (CodeRabbit nitpick): per SemVer 2.0 §10, build
 * metadata (anything after `+`) is ignored in precedence. Strip it
 * before splitting on `-`.
 *
 * Strategy: strip build metadata, split on the first `-`, compare the
 * release parts numerically (so `0.1.10 > 0.1.2`), apply §11 rule for
 * pre-release tiebreak.
 */
function semverCompare(a: string, b: string): number {
  // §10: drop build metadata (everything after the first '+').
  const stripBuild = (v: string) => v.split("+", 1)[0];
  const [aRelease, aPre] = stripBuild(a).split(/-(.+)/, 2);
  const [bRelease, bPre] = stripBuild(b).split(/-(.+)/, 2);
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

  // ZEB-328 PR #160 R4 (Cursor MED): "Skip this version" semantics are
  // PER-VERSION (per spec §6.4), not "suppress all ≤ this version". The
  // previous `semverCompare(update.version, dismissed) <= 0` form would,
  // after dismissing a stable `1.0.0`, silently suppress any future
  // pre-release of the same release (e.g., `1.0.0-alpha.11` — which is
  // SemVer-§11-less-than `1.0.0`). Strict equality matches user intent:
  // "I dismissed exactly this version; show me anything else, including
  // new pre-releases I haven't seen."
  //
  // semverCompare is retained (used by the per-version comparison test
  // suite + future range checks if we ever need them).
  const dismissed = localStorage.getItem(DISMISSED_VERSION_KEY);
  if (dismissed && semverCompare(update.version, dismissed) === 0) {
    return null;
  }

  return update;
}

/** Persist a per-version "don't bother me about this version" decision. */
export function dismissVersion(version: string): void {
  localStorage.setItem(DISMISSED_VERSION_KEY, version);
}
