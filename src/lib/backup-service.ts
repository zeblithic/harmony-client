import { invoke } from '@tauri-apps/api/core';

/**
 * localStorage key for the "Dismiss for 7 days" timestamp (unix ms).
 * Reading/writing this key keeps dismiss state purely frontend-side.
 */
export const BACKUP_DISMISS_KEY = 'harmony.backupStaleness.dismissUntilMs';

export interface BackupStaleness {
  isStale: boolean;
  daysSince: number;
}

/**
 * Read the localStorage dismiss-until-ms key. Returns `undefined` for any
 * non-positive-finite-number content (missing, empty string, "abc",
 * "Infinity", negative, zero). Round-1 bot finding M6 (CodeAnt).
 *
 * `Number(raw)` returns `NaN` for non-numeric strings and `Infinity` for
 * "Infinity"; `Number.isFinite` rejects both. We also reject zero and
 * negative values — a dismiss timestamp is an absolute wall-ms in the
 * future, so a value <= 0 cannot be valid (treating it as `undefined`
 * keeps the IPC dismissUntilMs payload clean).
 */
export function readDismissUntilMs(): number | undefined {
  const raw = localStorage.getItem(BACKUP_DISMISS_KEY);
  if (raw === null || raw === '') return undefined;
  const n = Number(raw);
  if (!Number.isFinite(n)) return undefined;
  if (n <= 0) return undefined;
  return n;
}

export async function getBackupStaleness(): Promise<BackupStaleness> {
  try {
    return await invoke<BackupStaleness>('get_backup_staleness', {
      dismissUntilMs: readDismissUntilMs(),
    });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(msg);
  }
}

export function dismissForDays(days: number): void {
  const until = Date.now() + days * 86_400_000;
  localStorage.setItem(BACKUP_DISMISS_KEY, String(until));
}

export function clearDismiss(): void {
  localStorage.removeItem(BACKUP_DISMISS_KEY);
}
