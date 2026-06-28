/**
 * ZEB-587 — owner-scoped onboarding backup flags.
 *
 * The "have you backed up your recovery artifact?" reminder is gated on three
 * flags. They were originally fixed, owner-agnostic localStorage keys, but
 * WebView localStorage is bundle-scoped (NOT isolated by `HARMONY_PROFILE`), so
 * once ANY identity on a machine backed up, `recoveryArtifactBackedUp` stayed
 * truthy forever and suppressed the safety reminder for every later identity
 * that skipped — a data-loss footgun on identity recreation (ZEB-173).
 *
 * Fix: scope every flag by `owner_id` (`<base>:owner-<id>`). A fresh identity
 * has no flags, so it gets the correct banner; a backed-up identity stays quiet.
 * `backupSkipped` records a UI choice with no backend equivalent, so localStorage
 * (owner-scoped) is its right home.
 *
 * Clean break (no migration): the legacy owner-agnostic keys are never read, so
 * any stale/contaminated global value is simply ignored. The fail-safe default
 * when an owner has no flags is "no banner" for an established identity and
 * "show banner" for a fresh skip — both correct.
 *
 * `backupBannerDismissed` is session-only (sessionStorage) — a per-session
 * snooze, not a durable "handled".
 */

const SKIPPED = 'harmony.onboarding.backupSkipped';
const BACKED_UP = 'harmony.onboarding.recoveryArtifactBackedUp';
const DISMISSED = 'harmony.onboarding.backupBannerDismissed';

type StoreKind = 'local' | 'session';

function ownerKey(base: string, ownerId: string): string {
  return `${base}:owner-${ownerId}`;
}

// Resolve the storage global INSIDE the try: merely accessing
// `localStorage`/`sessionStorage` can throw (e.g. a sandboxed WebView with
// storage disabled), so passing the global in as an argument would let that
// throw escape before any try/catch here could handle it.
function readFlag(kind: StoreKind, base: string, ownerId: string): boolean {
  try {
    const store = kind === 'local' ? localStorage : sessionStorage;
    return store.getItem(ownerKey(base, ownerId)) === 'true';
  } catch {
    // storage unavailable → safest is to treat the flag as unset
    return false;
  }
}

function writeFlag(kind: StoreKind, base: string, ownerId: string): void {
  try {
    const store = kind === 'local' ? localStorage : sessionStorage;
    store.setItem(ownerKey(base, ownerId), 'true');
  } catch (e) {
    console.debug('[zeb-587] onboarding flag write failed:', e instanceof Error ? e.message : String(e));
  }
}

/** Record that this owner skipped the onboarding backup (durable). */
export function markBackupSkipped(ownerId: string): void {
  writeFlag('local', SKIPPED, ownerId);
}

/** Record that this owner exported a recovery artifact (durable). */
export function markRecoveryBackedUp(ownerId: string): void {
  writeFlag('local', BACKED_UP, ownerId);
}

/** Snooze the reminder banner for this owner for the current session only. */
export function markBannerDismissed(ownerId: string): void {
  writeFlag('session', DISMISSED, ownerId);
}

export function isBackupSkipped(ownerId: string): boolean {
  return readFlag('local', SKIPPED, ownerId);
}

export function isRecoveryBackedUp(ownerId: string): boolean {
  return readFlag('local', BACKED_UP, ownerId);
}

export function isBannerDismissed(ownerId: string): boolean {
  return readFlag('session', DISMISSED, ownerId);
}

/**
 * Whether the backup-reminder banner should be visible for `ownerId`.
 *
 * Returns false when the owner identity has not resolved yet (`null`) — the
 * banner must not show for an unknown owner, and it re-evaluates once the real
 * owner id arrives.
 */
export function isBackupReminderVisible(ownerId: string | null): boolean {
  if (!ownerId) return false;
  return isBackupSkipped(ownerId) && !isRecoveryBackedUp(ownerId) && !isBannerDismissed(ownerId);
}
