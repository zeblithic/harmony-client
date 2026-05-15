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

function readDismissUntilMs(): number | undefined {
  const raw = localStorage.getItem(BACKUP_DISMISS_KEY);
  if (!raw) return undefined;
  const n = Number(raw);
  if (!Number.isFinite(n)) return undefined;
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
