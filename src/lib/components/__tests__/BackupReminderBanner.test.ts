import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import BackupReminderBanner from '../BackupReminderBanner.svelte';

// vi.hoisted ensures these are available at mock-factory call time
// (vi.mock is hoisted to the top of the file by Vitest, so module-level
// vi.fn() declarations would be undefined when the factory runs).
const { requestExportSavePathMock, exportRecoveryFileMock, issueRecoveryTokenMock } = vi.hoisted(
  () => ({
    requestExportSavePathMock: vi.fn(),
    exportRecoveryFileMock: vi.fn(),
    issueRecoveryTokenMock: vi.fn(),
  }),
);

vi.mock('../../owner-service', () => ({
  OwnerService: class {
    requestExportSavePath = requestExportSavePathMock;
    exportRecoveryFile = exportRecoveryFileMock;
    issueRecoveryToken = issueRecoveryTokenMock;
  },
  extractError: (e: unknown) => (e instanceof Error ? e.message : String(e)),
}));

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  requestExportSavePathMock.mockReset();
  exportRecoveryFileMock.mockReset();
  issueRecoveryTokenMock.mockReset();
});

describe('BackupReminderBanner visibility', () => {
  it('mounts when backupSkipped set and no backup flag', () => {
    localStorage.setItem('harmony.onboarding.backupSkipped', 'true');
    const { queryByTestId } = render(BackupReminderBanner);
    expect(queryByTestId('backup-reminder-banner')).toBeTruthy();
  });

  it('does not mount when backup flag set', () => {
    localStorage.setItem('harmony.onboarding.backupSkipped', 'true');
    localStorage.setItem('harmony.onboarding.recoveryArtifactBackedUp', 'true');
    const { queryByTestId } = render(BackupReminderBanner);
    expect(queryByTestId('backup-reminder-banner')).toBeNull();
  });

  it('does not mount when backup was never skipped', () => {
    const { queryByTestId } = render(BackupReminderBanner);
    expect(queryByTestId('backup-reminder-banner')).toBeNull();
  });

  it('dismiss hides for session', async () => {
    localStorage.setItem('harmony.onboarding.backupSkipped', 'true');
    const { queryByTestId, getByTestId } = render(BackupReminderBanner);
    await fireEvent.click(getByTestId('backup-reminder-dismiss'));
    expect(queryByTestId('backup-reminder-banner')).toBeNull();
    expect(sessionStorage.getItem('harmony.onboarding.backupBannerDismissed')).toBe('true');
  });

  it('does not mount when dismissed this session', () => {
    localStorage.setItem('harmony.onboarding.backupSkipped', 'true');
    sessionStorage.setItem('harmony.onboarding.backupBannerDismissed', 'true');
    const { queryByTestId } = render(BackupReminderBanner);
    expect(queryByTestId('backup-reminder-banner')).toBeNull();
  });

  it('back up now runs export flow and hides on success', async () => {
    localStorage.setItem('harmony.onboarding.backupSkipped', 'true');
    issueRecoveryTokenMock.mockResolvedValue('tok');
    requestExportSavePathMock.mockResolvedValue('path-token');
    exportRecoveryFileMock.mockResolvedValue({ identityHash: 'h', byteLen: 1, path: '/x' });
    const { queryByTestId, getByTestId } = render(BackupReminderBanner);
    await fireEvent.click(getByTestId('backup-reminder-backup-now'));
    // passphrase prompt appears inline; fill + submit
    await fireEvent.input(getByTestId('backup-reminder-passphrase'), {
      target: { value: 'longenoughpass' },
    });
    await fireEvent.click(getByTestId('backup-reminder-save'));
    await Promise.resolve();
    await Promise.resolve();
    expect(localStorage.getItem('harmony.onboarding.recoveryArtifactBackedUp')).toBe('true');
    expect(queryByTestId('backup-reminder-banner')).toBeNull();
  });
});
