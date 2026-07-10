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
// ZEB-650 slice 1: additive timestamps beside the booleans. Legacy owners
// (boolean set, stamp absent) read as null — callers degrade to the
// pre-timestamp copy rather than fabricating a figure.
const SKIPPED_AT = 'harmony.onboarding.backupSkippedAt';
const BACKED_UP_AT = 'harmony.onboarding.recoveryBackedUpAt';

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

function writeStamp(base: string, ownerId: string, nowMs: number): void {
  try {
    localStorage.setItem(ownerKey(base, ownerId), String(nowMs));
  } catch (e) {
    console.debug('[zeb-650] onboarding stamp write failed:', e instanceof Error ? e.message : String(e));
  }
}

function readStamp(base: string, ownerId: string): number | null {
  try {
    const raw = localStorage.getItem(ownerKey(base, ownerId));
    if (raw === null) return null;
    // Strict decimal-integer parse: Number('') / Number('   ') are 0, which
    // would read corrupted storage as the Unix epoch and render absurd
    // day counts / a 1970 backup date (Qodo + CodeRabbit, PR #436). A real
    // stamp is a positive Date.now(), so 0 is also rejected.
    const value = raw.trim();
    if (!/^\d+$/.test(value)) return null;
    const n = Number(value);
    return Number.isSafeInteger(n) && n > 0 ? n : null;
  } catch {
    return null;
  }
}

/**
 * Fired on `window` whenever a marker below writes. localStorage is not
 * reactive, so an already-mounted reader (e.g. BackupReminderBanner while a
 * DevicesPanel backup completes) must be told to re-derive — otherwise it
 * shows stale flag state until remount (CodeRabbit, PR #436).
 */
export const BACKUP_FLAGS_CHANGED_EVENT = 'harmony:onboarding-backup-flags-changed';

function signalFlagsChanged(): void {
  try {
    window.dispatchEvent(new Event(BACKUP_FLAGS_CHANGED_EVENT));
  } catch {
    // Non-DOM environment — persistence still happened; readers re-derive on mount.
  }
}

/** Record that this owner skipped the onboarding backup (durable). */
export function markBackupSkipped(ownerId: string): void {
  writeFlag('local', SKIPPED, ownerId);
  writeStamp(SKIPPED_AT, ownerId, Date.now());
  signalFlagsChanged();
}

/** Record that this owner exported a recovery artifact (durable). */
export function markRecoveryBackedUp(ownerId: string): void {
  writeFlag('local', BACKED_UP, ownerId);
  writeStamp(BACKED_UP_AT, ownerId, Date.now());
  signalFlagsChanged();
}

export function backupSkippedAtMs(ownerId: string): number | null {
  return readStamp(SKIPPED_AT, ownerId);
}

export function recoveryBackedUpAtMs(ownerId: string): number | null {
  return readStamp(BACKED_UP_AT, ownerId);
}

/** Whole days (floored) since this owner skipped backup; null when no stamp
 *  exists (legacy owner or never skipped). Clamped at 0 under clock skew. */
export function daysSinceBackupSkipped(ownerId: string, nowMs: number = Date.now()): number | null {
  const at = backupSkippedAtMs(ownerId);
  if (at === null) return null;
  return Math.max(0, Math.floor((nowMs - at) / 86_400_000));
}

/** Snooze the reminder banner for this owner for the current session only. */
export function markBannerDismissed(ownerId: string): void {
  writeFlag('session', DISMISSED, ownerId);
  signalFlagsChanged();
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
