import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));

import { invoke } from '@tauri-apps/api/core';
import {
  getBackupStaleness,
  BACKUP_DISMISS_KEY,
  readDismissUntilMs,
  dismissForDays,
} from '../backup-service';

const OWNER = 'aaaa0000aaaa0000aaaa0000aaaa0000';
const OTHER = 'bbbb1111bbbb1111bbbb1111bbbb1111';
const dismissKey = (id: string) => `${BACKUP_DISMISS_KEY}:owner-${id}`;

describe('backup-service (owner-scoped, ZEB-589)', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    localStorage.clear();
  });

  it('passes this owner’s dismissUntilMs from localStorage to the IPC', async () => {
    const future = Date.now() + 86_400_000;
    localStorage.setItem(dismissKey(OWNER), String(future));
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue({ isStale: false, daysSince: 0 });
    await getBackupStaleness(OWNER);
    expect(invoke).toHaveBeenCalledWith('get_backup_staleness', {
      dismissUntilMs: future,
    });
  });

  it('passes dismissUntilMs=undefined when no owner is known yet', async () => {
    // An owner-less call must not read any shared key.
    localStorage.setItem(dismissKey(OWNER), String(Date.now() + 86_400_000));
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue({ isStale: false, daysSince: 0 });
    await getBackupStaleness();
    expect(invoke).toHaveBeenCalledWith('get_backup_staleness', {
      dismissUntilMs: undefined,
    });
  });

  // ── ZEB-589 regression: a dismiss under one identity must not snooze another ──
  it('does not leak one owner’s dismiss into another owner', () => {
    dismissForDays(7, OWNER);
    expect(readDismissUntilMs(OWNER)).toBeGreaterThan(Date.now());
    expect(readDismissUntilMs(OTHER)).toBeUndefined();
  });

  it('dismissForDays without an owner is a no-op (writes no shared key)', () => {
    dismissForDays(7);
    expect(localStorage.length).toBe(0);
  });

  it('normalizes errors via instanceof Error', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockRejectedValue(new Error('boom'));
    await expect(getBackupStaleness(OWNER)).rejects.toMatchObject({ message: 'boom' });

    (invoke as ReturnType<typeof vi.fn>).mockRejectedValue('plain string rejection');
    await expect(getBackupStaleness(OWNER)).rejects.toMatchObject({
      message: 'plain string rejection',
    });
  });

  // Round-1 bot finding M6 (CodeAnt), carried forward owner-scoped:
  // readDismissUntilMs must return `undefined` for every flavor of corrupted
  // localStorage content (and for an absent owner).
  describe('readDismissUntilMs', () => {
    it('returns undefined when no owner is given', () => {
      localStorage.setItem(dismissKey(OWNER), String(Date.now() + 86_400_000));
      expect(readDismissUntilMs()).toBeUndefined();
    });

    it('returns undefined for missing key', () => {
      expect(readDismissUntilMs(OWNER)).toBeUndefined();
    });

    it('returns undefined for empty string', () => {
      localStorage.setItem(dismissKey(OWNER), '');
      expect(readDismissUntilMs(OWNER)).toBeUndefined();
    });

    it('returns undefined for non-numeric garbage', () => {
      localStorage.setItem(dismissKey(OWNER), 'abc');
      expect(readDismissUntilMs(OWNER)).toBeUndefined();
    });

    it('returns undefined for "Infinity"', () => {
      localStorage.setItem(dismissKey(OWNER), 'Infinity');
      expect(readDismissUntilMs(OWNER)).toBeUndefined();
    });

    it('returns undefined for negative numbers', () => {
      localStorage.setItem(dismissKey(OWNER), '-1');
      expect(readDismissUntilMs(OWNER)).toBeUndefined();
      localStorage.setItem(dismissKey(OWNER), '-1700000000000');
      expect(readDismissUntilMs(OWNER)).toBeUndefined();
    });

    it('returns undefined for zero', () => {
      localStorage.setItem(dismissKey(OWNER), '0');
      expect(readDismissUntilMs(OWNER)).toBeUndefined();
    });

    it('returns the parsed value for valid positive numbers', () => {
      const future = Date.now() + 86_400_000;
      localStorage.setItem(dismissKey(OWNER), String(future));
      expect(readDismissUntilMs(OWNER)).toBe(future);
    });
  });
});
