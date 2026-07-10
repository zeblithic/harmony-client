import { describe, it, expect, beforeEach } from 'vitest';
import {
  markBackupSkipped,
  markRecoveryBackedUp,
  markBannerDismissed,
  isBackupSkipped,
  isRecoveryBackedUp,
  isBannerDismissed,
  isBackupReminderVisible,
  backupSkippedAtMs,
  recoveryBackedUpAtMs,
  daysSinceBackupSkipped,
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

describe('backup timestamps (ZEB-650 slice 1)', () => {
  beforeEach(() => {
    localStorage.clear();
    sessionStorage.clear();
  });

  it('markBackupSkipped stamps an owner-scoped skippedAt', () => {
    const before = Date.now();
    markBackupSkipped(A);
    const at = backupSkippedAtMs(A);
    expect(at).not.toBeNull();
    expect(at!).toBeGreaterThanOrEqual(before);
    expect(backupSkippedAtMs(B)).toBeNull();
  });

  it('markRecoveryBackedUp stamps an owner-scoped backedUpAt', () => {
    markRecoveryBackedUp(A);
    expect(recoveryBackedUpAtMs(A)).not.toBeNull();
    expect(recoveryBackedUpAtMs(B)).toBeNull();
  });

  it('legacy boolean-only flags read as null timestamps', () => {
    // Pre-timestamp writers only set the boolean key.
    localStorage.setItem(`harmony.onboarding.backupSkipped:owner-${A}`, 'true');
    expect(isBackupSkipped(A)).toBe(true);
    expect(backupSkippedAtMs(A)).toBeNull();
    expect(daysSinceBackupSkipped(A)).toBeNull();
  });

  it('corrupt stamp value reads as null', () => {
    localStorage.setItem(`harmony.onboarding.backupSkippedAt:owner-${A}`, 'garbage');
    expect(backupSkippedAtMs(A)).toBeNull();
  });

  it('daysSinceBackupSkipped floors whole days from injected now', () => {
    markBackupSkipped(A);
    const at = backupSkippedAtMs(A)!;
    expect(daysSinceBackupSkipped(A, at)).toBe(0);
    expect(daysSinceBackupSkipped(A, at + 86_399_000)).toBe(0);
    expect(daysSinceBackupSkipped(A, at + 86_400_000)).toBe(1);
    expect(daysSinceBackupSkipped(A, at + 7 * 86_400_000 + 5)).toBe(7);
  });

  it('clock skew (stamp in the future) clamps to 0, never negative', () => {
    markBackupSkipped(A);
    const at = backupSkippedAtMs(A)!;
    expect(daysSinceBackupSkipped(A, at - 86_400_000)).toBe(0);
  });

  it('re-backing-up updates the backedUpAt stamp', () => {
    localStorage.setItem(`harmony.onboarding.recoveryBackedUpAt:owner-${A}`, '5');
    markRecoveryBackedUp(A);
    expect(recoveryBackedUpAtMs(A)!).toBeGreaterThan(5);
  });
});

describe('readStamp strictness (Qodo/CodeRabbit PR #436)', () => {
  beforeEach(() => {
    localStorage.clear();
  });

  const atKey = `harmony.onboarding.backupSkippedAt:owner-${A}`;

  it.each([['empty', ''], ['whitespace', '   '], ['zero', '0'], ['float', '1.5'], ['negative', '-5'], ['exponent', '1e10']])(
    'rejects %s stamp values as null',
    (_label, value) => {
      localStorage.setItem(atKey, value);
      expect(backupSkippedAtMs(A)).toBeNull();
      expect(daysSinceBackupSkipped(A)).toBeNull();
    },
  );
});
