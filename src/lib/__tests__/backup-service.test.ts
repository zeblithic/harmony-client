import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));

import { invoke } from '@tauri-apps/api/core';
import { getBackupStaleness, BACKUP_DISMISS_KEY } from '../backup-service';

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
});
