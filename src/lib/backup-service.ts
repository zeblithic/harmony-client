import { invoke } from '@tauri-apps/api/core';

/**
 * Base localStorage key for the "Dismiss for 7 days" timestamp (unix ms).
 * The persisted key is ALWAYS owner-scoped — `<base>:owner-<ownerId>` — so a
 * machine that hosts more than one identity never shares one owner's staleness
 * snooze with another (ZEB-589; same class as ZEB-586/587). WebView localStorage
 * is bundle-scoped and NOT isolated by `HARMONY_PROFILE`, so a fixed key would be
 * shared across every identity on the box. Reading/writing keeps dismiss state
 * purely frontend-side.
 */
export const BACKUP_DISMISS_KEY = 'harmony.backupStaleness.dismissUntilMs';

function dismissKey(ownerId: string): string {
  return `${BACKUP_DISMISS_KEY}:owner-${ownerId}`;
}

export interface BackupStaleness {
  isStale: boolean;
  daysSince: number;
}

/**
 * Read `ownerId`'s dismiss-until-ms key. Returns `undefined` for any
 * non-positive-finite-number content (missing, empty string, "abc",
 * "Infinity", negative, zero) — and whenever `ownerId` is absent (the
 * pre-identity window), since there is no owner to scope to and reading a
 * shared key would be the ZEB-589 leak. Round-1 bot finding M6 (CodeAnt).
 *
 * `Number(raw)` returns `NaN` for non-numeric strings and `Infinity` for
 * "Infinity"; `Number.isFinite` rejects both. We also reject zero and
 * negative values — a dismiss timestamp is an absolute wall-ms in the
 * future, so a value <= 0 cannot be valid (treating it as `undefined`
 * keeps the IPC dismissUntilMs payload clean).
 */
export function readDismissUntilMs(ownerId?: string): number | undefined {
  if (!ownerId) return undefined;
  const raw = localStorage.getItem(dismissKey(ownerId));
  if (raw === null || raw === '') return undefined;
  const n = Number(raw);
  if (!Number.isFinite(n)) return undefined;
  if (n <= 0) return undefined;
  return n;
}

export async function getBackupStaleness(ownerId?: string): Promise<BackupStaleness> {
  try {
    return await invoke<BackupStaleness>('get_backup_staleness', {
      dismissUntilMs: readDismissUntilMs(ownerId),
    });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(msg);
  }
}

/** Snooze the staleness warning for `ownerId` for `days`. No-op without an
 *  owner — there is no identity to scope the snooze to. */
export function dismissForDays(days: number, ownerId?: string): void {
  if (!ownerId) return;
  const until = Date.now() + days * 86_400_000;
  localStorage.setItem(dismissKey(ownerId), String(until));
}

/** Clear `ownerId`'s staleness snooze. No-op without an owner. */
export function clearDismiss(ownerId?: string): void {
  if (!ownerId) return;
  localStorage.removeItem(dismissKey(ownerId));
}
