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

const OWNER = 'aaaa0000aaaa0000aaaa0000aaaa0000';
const OTHER = 'bbbb1111bbbb1111bbbb1111bbbb1111';
const skippedKey = (id: string) => `harmony.onboarding.backupSkipped:owner-${id}`;
const backedUpKey = (id: string) => `harmony.onboarding.recoveryArtifactBackedUp:owner-${id}`;
const dismissedKey = (id: string) => `harmony.onboarding.backupBannerDismissed:owner-${id}`;

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  requestExportSavePathMock.mockReset();
  exportRecoveryFileMock.mockReset();
  issueRecoveryTokenMock.mockReset();
});

describe('BackupReminderBanner visibility (owner-scoped, ZEB-587)', () => {
  it('mounts when this owner skipped and has no backup flag', () => {
    localStorage.setItem(skippedKey(OWNER), 'true');
    const { queryByTestId } = render(BackupReminderBanner, { props: { ownerId: OWNER } });
    expect(queryByTestId('backup-reminder-banner')).toBeTruthy();
  });

  it('does not mount when this owner backed up', () => {
    localStorage.setItem(skippedKey(OWNER), 'true');
    localStorage.setItem(backedUpKey(OWNER), 'true');
    const { queryByTestId } = render(BackupReminderBanner, { props: { ownerId: OWNER } });
    expect(queryByTestId('backup-reminder-banner')).toBeNull();
  });

  it('does not mount when backup was never skipped', () => {
    const { queryByTestId } = render(BackupReminderBanner, { props: { ownerId: OWNER } });
    expect(queryByTestId('backup-reminder-banner')).toBeNull();
  });

  it('does not mount before the owner identity resolves (null)', () => {
    localStorage.setItem(skippedKey(OWNER), 'true');
    const { queryByTestId } = render(BackupReminderBanner, { props: { ownerId: null } });
    expect(queryByTestId('backup-reminder-banner')).toBeNull();
  });

  // ── ZEB-587 regression: the data-loss case the smoke test surfaced ──
  it('reminds an un-backed-up identity even when another identity backed up', () => {
    localStorage.setItem(backedUpKey(OTHER), 'true'); // a DIFFERENT identity backed up
    localStorage.setItem(skippedKey(OWNER), 'true'); // this identity skipped, never backed up
    const { queryByTestId } = render(BackupReminderBanner, { props: { ownerId: OWNER } });
    expect(queryByTestId('backup-reminder-banner')).toBeTruthy();
  });

  it('dismiss hides for session and writes the owner-scoped session key', async () => {
    localStorage.setItem(skippedKey(OWNER), 'true');
    const { queryByTestId, getByTestId } = render(BackupReminderBanner, {
      props: { ownerId: OWNER },
    });
    await fireEvent.click(getByTestId('backup-reminder-dismiss'));
    expect(queryByTestId('backup-reminder-banner')).toBeNull();
    expect(sessionStorage.getItem(dismissedKey(OWNER))).toBe('true');
  });

  it('does not mount when dismissed this session', () => {
    localStorage.setItem(skippedKey(OWNER), 'true');
    sessionStorage.setItem(dismissedKey(OWNER), 'true');
    const { queryByTestId } = render(BackupReminderBanner, { props: { ownerId: OWNER } });
    expect(queryByTestId('backup-reminder-banner')).toBeNull();
  });

  it('a per-session dismiss does not carry across an owner change', async () => {
    localStorage.setItem(skippedKey(OWNER), 'true');
    localStorage.setItem(skippedKey(OTHER), 'true');
    const { queryByTestId, getByTestId, rerender } = render(BackupReminderBanner, {
      props: { ownerId: OWNER },
    });
    await fireEvent.click(getByTestId('backup-reminder-dismiss'));
    expect(queryByTestId('backup-reminder-banner')).toBeNull();
    // The owner prop resolves to a different identity while still mounted: the
    // first owner's dismiss must not suppress the new owner's reminder.
    await rerender({ ownerId: OTHER });
    await vi.waitFor(() => expect(queryByTestId('backup-reminder-banner')).toBeTruthy());
  });

  // After the clay restyle + tokenization (the amber #4a3a1a literal is gone),
  // the banner must still render with its primary action + dismiss control.
  it('renders the clay backup banner with its action + dismiss', () => {
    localStorage.setItem(skippedKey(OWNER), 'true');
    const { getByTestId } = render(BackupReminderBanner, { props: { ownerId: OWNER } });
    expect(getByTestId('backup-reminder-banner')).toBeTruthy();
    expect(getByTestId('backup-reminder-backup-now')).toBeTruthy();
    expect(getByTestId('backup-reminder-dismiss')).toBeTruthy();
  });

  it('back up now runs export flow and marks owner-scoped backed-up on success', async () => {
    localStorage.setItem(skippedKey(OWNER), 'true');
    issueRecoveryTokenMock.mockResolvedValue('tok');
    requestExportSavePathMock.mockResolvedValue('path-token');
    exportRecoveryFileMock.mockResolvedValue({ identityHash: 'h', byteLen: 1, path: '/x' });
    const { queryByTestId, getByTestId } = render(BackupReminderBanner, {
      props: { ownerId: OWNER },
    });
    await fireEvent.click(getByTestId('backup-reminder-backup-now'));
    // passphrase prompt appears inline; fill + submit
    await fireEvent.input(getByTestId('backup-reminder-passphrase'), {
      target: { value: 'longenoughpass' },
    });
    await fireEvent.click(getByTestId('backup-reminder-save'));
    await Promise.resolve();
    await Promise.resolve();
    expect(localStorage.getItem(backedUpKey(OWNER))).toBe('true');
    expect(queryByTestId('backup-reminder-banner')).toBeNull();
  });
});
