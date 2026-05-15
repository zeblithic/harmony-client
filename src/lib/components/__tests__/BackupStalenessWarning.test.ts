import { render, fireEvent, screen } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import BackupStalenessWarning from '../BackupStalenessWarning.svelte';

vi.mock('../../backup-service', () => ({
  BACKUP_DISMISS_KEY: 'harmony.backupStaleness.dismissUntilMs',
  getBackupStaleness: vi.fn(),
  dismissForDays: vi.fn(),
}));

import { getBackupStaleness, dismissForDays } from '../../backup-service';

describe('BackupStalenessWarning', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    localStorage.clear();
  });

  it('renders the banner when isStale is true', async () => {
    (getBackupStaleness as ReturnType<typeof vi.fn>).mockResolvedValue({
      isStale: true,
      daysSince: 23,
    });
    render(BackupStalenessWarning, { props: {} });
    expect(await screen.findByText(/Your backup is 23 days old/i)).toBeTruthy();
  });

  it('does NOT render when isStale is false', async () => {
    (getBackupStaleness as ReturnType<typeof vi.fn>).mockResolvedValue({
      isStale: false,
      daysSince: 0,
    });
    const { container } = render(BackupStalenessWarning, { props: {} });
    // Wait for the await to flush.
    await new Promise((r) => setTimeout(r, 0));
    expect(container.querySelector('[data-testid="backup-staleness-banner"]')).toBeNull();
  });

  it('hides the banner after Dismiss for 7 days clicked', async () => {
    (getBackupStaleness as ReturnType<typeof vi.fn>).mockResolvedValue({
      isStale: true,
      daysSince: 30,
    });
    render(BackupStalenessWarning, { props: {} });
    const btn = await screen.findByRole('button', { name: /dismiss/i });
    await fireEvent.click(btn);
    expect(dismissForDays).toHaveBeenCalledWith(7);
    // After dismiss the banner should disappear in-place.
    expect(screen.queryByText(/Your backup is/i)).toBeNull();
  });

  it('calls onExportRequested when Export new backup clicked', async () => {
    (getBackupStaleness as ReturnType<typeof vi.fn>).mockResolvedValue({
      isStale: true,
      daysSince: 30,
    });
    const onExportRequested = vi.fn();
    render(BackupStalenessWarning, { props: { onExportRequested } });
    const btn = await screen.findByRole('button', { name: /export new backup/i });
    await fireEvent.click(btn);
    expect(onExportRequested).toHaveBeenCalledTimes(1);
  });
});
