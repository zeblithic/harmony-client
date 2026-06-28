import { describe, it, expect, beforeEach } from 'vitest';
import {
  markBackupSkipped,
  markRecoveryBackedUp,
  markBannerDismissed,
  isBackupSkipped,
  isRecoveryBackedUp,
  isBannerDismissed,
  isBackupReminderVisible,
} from './onboarding-backup-flags';

const A = 'aaaa0000aaaa0000aaaa0000aaaa0000';
const B = 'bbbb1111bbbb1111bbbb1111bbbb1111';

describe('onboarding-backup-flags (owner-scoped)', () => {
  beforeEach(() => {
    localStorage.clear();
    sessionStorage.clear();
  });

  it('skipped flag is owner-scoped', () => {
    markBackupSkipped(A);
    expect(isBackupSkipped(A)).toBe(true);
    expect(isBackupSkipped(B)).toBe(false);
  });

  it('backed-up flag is owner-scoped', () => {
    markRecoveryBackedUp(A);
    expect(isRecoveryBackedUp(A)).toBe(true);
    expect(isRecoveryBackedUp(B)).toBe(false);
  });

  it('dismissed flag is owner-scoped and lives in sessionStorage (not localStorage)', () => {
    markBannerDismissed(A);
    expect(isBannerDismissed(A)).toBe(true);
    expect(isBannerDismissed(B)).toBe(false);
    expect(localStorage.length).toBe(0);
  });

  it('visible when this owner skipped and has not backed up or dismissed', () => {
    markBackupSkipped(A);
    expect(isBackupReminderVisible(A)).toBe(true);
  });

  it('hidden when backup was never skipped', () => {
    expect(isBackupReminderVisible(A)).toBe(false);
  });

  it('hidden when this owner backed up', () => {
    markBackupSkipped(A);
    markRecoveryBackedUp(A);
    expect(isBackupReminderVisible(A)).toBe(false);
  });

  it('hidden when dismissed this session', () => {
    markBackupSkipped(A);
    markBannerDismissed(A);
    expect(isBackupReminderVisible(A)).toBe(false);
  });

  it('hidden when the owner identity has not resolved yet (null)', () => {
    expect(isBackupReminderVisible(null)).toBe(false);
  });

  // ── ZEB-587 regression: the data-loss case the smoke test surfaced ──
  it('an un-backed-up identity that skipped STILL sees the reminder even after another identity backed up (ZEB-587)', () => {
    markRecoveryBackedUp(A); // identity A backed up on this machine
    markBackupSkipped(B); // identity B skipped, never backed up
    expect(isBackupReminderVisible(B)).toBe(true); // B must still be reminded
    expect(isBackupReminderVisible(A)).toBe(false); // A is covered
  });

  it('ignores legacy owner-agnostic keys (clean break, no migration)', () => {
    localStorage.setItem('harmony.onboarding.recoveryArtifactBackedUp', 'true');
    localStorage.setItem('harmony.onboarding.backupSkipped', 'true');
    expect(isRecoveryBackedUp(A)).toBe(false);
    expect(isBackupSkipped(A)).toBe(false);
    expect(isBackupReminderVisible(A)).toBe(false);
  });
});
