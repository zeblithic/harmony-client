import { render, fireEvent, screen, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import BackupStalenessWarning from '../BackupStalenessWarning.svelte';

vi.mock('../../backup-service', () => ({
  BACKUP_DISMISS_KEY: 'harmony.backupStaleness.dismissUntilMs',
  getBackupStaleness: vi.fn(),
  dismissForDays: vi.fn(),
}));

import { getBackupStaleness, dismissForDays } from '../../backup-service';

const OWNER = 'aaaa0000aaaa0000aaaa0000aaaa0000';

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
    render(BackupStalenessWarning, { props: { ownerId: OWNER } });
    expect(await screen.findByText(/Your backup is 23 days old/i)).toBeTruthy();
    // ZEB-589: the staleness check is scoped to this owner.
    expect(getBackupStaleness).toHaveBeenCalledWith(OWNER);
  });

  it('does NOT render when isStale is false', async () => {
    // M4 (CodeAnt): fixed-timer waits race the Svelte microtask queue.
    // waitFor retries until the assertion holds (or times out), making
    // the test deterministic regardless of microtask scheduling.
    (getBackupStaleness as ReturnType<typeof vi.fn>).mockResolvedValue({
      isStale: false,
      daysSince: 0,
    });
    const { container } = render(BackupStalenessWarning, { props: { ownerId: OWNER } });
    await waitFor(() => {
      expect(getBackupStaleness).toHaveBeenCalled();
    });
    expect(container.querySelector('[data-testid="backup-staleness-banner"]')).toBeNull();
  });

  it('does NOT query staleness before the owner identity resolves (null)', async () => {
    render(BackupStalenessWarning, { props: { ownerId: null } });
    await waitFor(() => {
      // give the effect a chance to (not) run
      expect(true).toBe(true);
    });
    expect(getBackupStaleness).not.toHaveBeenCalled();
    expect(screen.queryByTestId('backup-staleness-banner')).toBeNull();
  });

  it('hides the banner after Dismiss for 7 days clicked', async () => {
    // M5 (CodeAnt): the dismiss click sets isStale=false synchronously,
    // but Svelte renders on the microtask queue — the assertion that
    // the banner is gone may run BEFORE the DOM update flushes. waitFor
    // retries until the {#if isStale} block has actually re-evaluated.
    (getBackupStaleness as ReturnType<typeof vi.fn>).mockResolvedValue({
      isStale: true,
      daysSince: 30,
    });
    render(BackupStalenessWarning, { props: { ownerId: OWNER } });
    const btn = await screen.findByRole('button', { name: /dismiss/i });
    await fireEvent.click(btn);
    // ZEB-589: the snooze is recorded for this owner only.
    expect(dismissForDays).toHaveBeenCalledWith(7, OWNER);
    // After dismiss the banner should disappear in-place.
    await waitFor(() => {
      expect(screen.queryByText(/Your backup is/i)).toBeNull();
    });
  });

  it('calls onExportRequested when Export new backup clicked', async () => {
    (getBackupStaleness as ReturnType<typeof vi.fn>).mockResolvedValue({
      isStale: true,
      daysSince: 30,
    });
    const onExportRequested = vi.fn();
    render(BackupStalenessWarning, { props: { ownerId: OWNER, onExportRequested } });
    const btn = await screen.findByRole('button', { name: /export new backup/i });
    await fireEvent.click(btn);
    expect(onExportRequested).toHaveBeenCalledTimes(1);
  });
});
