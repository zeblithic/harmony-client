import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));

import { invoke } from '@tauri-apps/api/core';
import { getBackupStaleness, BACKUP_DISMISS_KEY, readDismissUntilMs } from '../backup-service';

describe('backup-service', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    localStorage.clear();
  });

  it('passes dismissUntilMs from localStorage to the IPC', async () => {
    const future = Date.now() + 86_400_000;
    localStorage.setItem(BACKUP_DISMISS_KEY, String(future));
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue({ isStale: false, daysSince: 0 });
    await getBackupStaleness();
    expect(invoke).toHaveBeenCalledWith('get_backup_staleness', {
      dismissUntilMs: future,
    });
  });

  it('normalizes errors via instanceof Error', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockRejectedValue(new Error('boom'));
    await expect(getBackupStaleness()).rejects.toMatchObject({ message: 'boom' });

    (invoke as ReturnType<typeof vi.fn>).mockRejectedValue('plain string rejection');
    await expect(getBackupStaleness()).rejects.toMatchObject({
      message: 'plain string rejection',
    });
  });

  // Round-1 bot finding M6 (CodeAnt): readDismissUntilMs must return
  // `undefined` for every flavor of corrupted localStorage content.
  // Without these tests, a future refactor could silently widen the
  // accepted set (e.g. by dropping the Number.isFinite check) and the
  // IPC would receive NaN / Infinity / negative timestamps.
  describe('readDismissUntilMs', () => {
    it('returns undefined for missing key', () => {
      expect(readDismissUntilMs()).toBeUndefined();
    });

    it('returns undefined for empty string', () => {
      localStorage.setItem(BACKUP_DISMISS_KEY, '');
      expect(readDismissUntilMs()).toBeUndefined();
    });

    it('returns undefined for non-numeric garbage', () => {
      localStorage.setItem(BACKUP_DISMISS_KEY, 'abc');
      expect(readDismissUntilMs()).toBeUndefined();
    });

    it('returns undefined for "Infinity"', () => {
      localStorage.setItem(BACKUP_DISMISS_KEY, 'Infinity');
      expect(readDismissUntilMs()).toBeUndefined();
    });

    it('returns undefined for negative numbers', () => {
      localStorage.setItem(BACKUP_DISMISS_KEY, '-1');
      expect(readDismissUntilMs()).toBeUndefined();
      localStorage.setItem(BACKUP_DISMISS_KEY, '-1700000000000');
      expect(readDismissUntilMs()).toBeUndefined();
    });

    it('returns undefined for zero', () => {
      localStorage.setItem(BACKUP_DISMISS_KEY, '0');
      expect(readDismissUntilMs()).toBeUndefined();
    });

    it('returns the parsed value for valid positive numbers', () => {
      const future = Date.now() + 86_400_000;
      localStorage.setItem(BACKUP_DISMISS_KEY, String(future));
      expect(readDismissUntilMs()).toBe(future);
    });
  });
});
