import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import DevicesPanel from '../DevicesPanel.svelte';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));

vi.mock('@tauri-apps/plugin-dialog', () => ({
  save: vi.fn().mockResolvedValue('/tmp/owner-recovery.bin'),
  open: vi.fn(),
}));

import { invoke } from '@tauri-apps/api/core';
import { save } from '@tauri-apps/plugin-dialog';

import { loadProfile, saveProfile } from '../../profile-service';

vi.mock('../../profile-service', () => ({
  loadProfile: vi.fn(),
  saveProfile: vi.fn(),
}));

const mockedInvoke = invoke as unknown as ReturnType<typeof vi.fn>;

// File-level beforeEach: runs before EVERY test in this file, including those
// in nested describe blocks. resetAllMocks (not clearAllMocks) wipes
// mockReturnValue/mockResolvedValue implementations so stubs from one suite
// don't leak into the next (e.g., loadProfile.mockReturnValue from rename
// suites bleeding into populated-state suites via applyLocalProfileOverlay).
// The save() default is reapplied because resetAllMocks wiped the vi.mock
// factory's mockResolvedValue too.
beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(save).mockResolvedValue('/tmp/owner-recovery.bin');
});

describe('DevicesPanel — empty + bootstrap states', () => {

  it('renders empty state when get_owner_state returns null', async () => {
    mockedInvoke.mockResolvedValueOnce(null);
    render(DevicesPanel);
    // Wait for async refresh on mount.
    await screen.findByRole('button', { name: /bind this device/i });
    expect(screen.queryAllByText(/owner identity/i).length).toBeGreaterThan(0);
  });

  it('opens confirm modal when bind CTA is clicked', async () => {
    mockedInvoke.mockResolvedValueOnce(null);
    render(DevicesPanel);
    const bindBtn = await screen.findByRole('button', { name: /bind this device/i });
    await fireEvent.click(bindBtn);
    expect(screen.getByText(/will create your owner identity/i)).toBeInTheDocument();
  });

  it('calls mint_owner_identity on confirm and transitions to populated state', async () => {
    mockedInvoke.mockResolvedValueOnce(null);
    const mintResult = {
      state: {
        ownerId: 'a4f1c8239b7dd809abcdef0123456789',
        ownerDisplayName: 'this device',
        devices: [{
          deviceId: 'aa11bb22cc33dd44ee55ff6677889900',
          displayName: 'this device',
          isThisDevice: true,
          trustDecision: { kind: 'full', reason: null },
          enrolledAt: 1_700_000_000,
          fingerprint: 'aa11·bb22',
        }],
        canBackUp: true,
      },
      recoveryToken: 'tok-1',
    };
    mockedInvoke.mockResolvedValueOnce(mintResult);
    render(DevicesPanel);
    const bindBtn = await screen.findByRole('button', { name: /bind this device/i });
    await fireEvent.click(bindBtn);
    const confirmBtn = await screen.findByRole('button', { name: /^create owner identity/i });
    await fireEvent.click(confirmBtn);
    await screen.findByText(/my devices/i);
    expect(mockedInvoke).toHaveBeenCalledWith('mint_owner_identity');
  });

  it('cancel modal returns to empty state without invoking mint', async () => {
    mockedInvoke.mockResolvedValueOnce(null);
    render(DevicesPanel);
    const bindBtn = await screen.findByRole('button', { name: /bind this device/i });
    await fireEvent.click(bindBtn);
    const cancel = await screen.findByRole('button', { name: /cancel/i });
    await fireEvent.click(cancel);
    expect(mockedInvoke).toHaveBeenCalledTimes(1); // only the initial refresh
    expect(screen.queryByText(/will create your owner identity/i)).not.toBeInTheDocument();
  });
});

describe('DevicesPanel — populated state', () => {
  it('renders owner header with display name and fingerprint', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'a4f1c8239b7dd809abcdef0123456789',
      ownerDisplayName: 'zeblith',
      devices: [{
        deviceId: 'aa11bb22cc33dd44ee55ff6677889900',
        displayName: 'this device',
        isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000,
        fingerprint: 'aa11·bb22',
      }],
      canBackUp: true,
    });
    render(DevicesPanel);
    await screen.findByText('zeblith');
    expect(screen.getByText(/a4f1·c823/i)).toBeInTheDocument();
    expect(screen.getByText(/back up owner identity/i)).toBeInTheDocument();
  });

  it('renders device row with name, this-device marker, trust badge, fingerprint, enrolled date', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'a4f1c8239b7dd809abcdef0123456789',
      ownerDisplayName: 'zeblith',
      devices: [{
        deviceId: 'aa11bb22cc33dd44ee55ff6677889900',
        displayName: 'KRILE',
        isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000,
        fingerprint: 'aa11·bb22',
      }],
      canBackUp: true,
    });
    render(DevicesPanel);
    await screen.findByText('KRILE');
    expect(screen.getByText(/this device/i)).toBeInTheDocument();
    expect(screen.getByText(/trusted/i)).toBeInTheDocument();
    expect(screen.getByText(/aa11·bb22/i)).toBeInTheDocument();
  });

  it('renders educational footer for adding another device', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'a4f1c8239b7dd809abcdef0123456789',
      ownerDisplayName: 'zeblith',
      devices: [{
        deviceId: 'aa11bb22cc33dd44ee55ff6677889900',
        displayName: 'this',
        isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000,
        fingerprint: 'aa11·bb22',
      }],
      canBackUp: true,
    });
    render(DevicesPanel);
    await screen.findByText(/add another device/i);
    expect(screen.getByText(/pairing UI is coming/i)).toBeInTheDocument();
  });
});

describe('DevicesPanel — rename', () => {
  it('clicking Rename shows inline edit field with current name pre-filled', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'a4f1', ownerDisplayName: 'zeblith',
      devices: [{
        deviceId: 'aa11bb22', displayName: 'KRILE', isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000, fingerprint: 'aa11·bb22',
      }],
      canBackUp: true,
    });
    (loadProfile as ReturnType<typeof vi.fn>).mockReturnValue({ address: 'a', displayName: 'KRILE' });

    render(DevicesPanel);
    const renameBtn = await screen.findByRole('button', { name: /rename/i });
    await fireEvent.click(renameBtn);
    const input = screen.getByRole('textbox', { name: /device name/i });
    expect((input as HTMLInputElement).value).toBe('KRILE');
  });

  it('saving the rename calls profile-service.saveProfile', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'a4f1', ownerDisplayName: 'zeblith',
      devices: [{
        deviceId: 'aa11bb22', displayName: 'KRILE', isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000, fingerprint: 'aa11·bb22',
      }],
      canBackUp: true,
    });
    (loadProfile as ReturnType<typeof vi.fn>).mockReturnValue({ address: 'a', displayName: 'KRILE' });

    render(DevicesPanel);
    const renameBtn = await screen.findByRole('button', { name: /rename/i });
    await fireEvent.click(renameBtn);
    const input = screen.getByRole('textbox', { name: /device name/i });
    await fireEvent.input(input, { target: { value: 'KRILE-prime' } });
    const saveBtn = screen.getByRole('button', { name: /save/i });
    await fireEvent.click(saveBtn);
    expect(saveProfile).toHaveBeenCalledWith(
      expect.objectContaining({ displayName: 'KRILE-prime' })
    );
  });
});

describe('DevicesPanel — backup wiring', () => {
  it('clicking Back up opens the backup modal and issues a token if needed', async () => {
    mockedInvoke
      .mockResolvedValueOnce({ ownerId: 'x', ownerDisplayName: 'me',
        devices: [{ deviceId: 'd', displayName: 'this', isThisDevice: true,
          trustDecision: { kind: 'full', reason: null },
          enrolledAt: 1_700_000_000, fingerprint: 'd·x' }],
        canBackUp: true })
      .mockResolvedValueOnce({ recoveryToken: 'fresh-tok' });

    render(DevicesPanel);
    const btn = await screen.findByRole('button', { name: /back up owner identity/i });
    await fireEvent.click(btn);
    expect(await screen.findByRole('dialog')).toBeInTheDocument();
    expect(invoke).toHaveBeenCalledWith('issue_owner_recovery_token');
  });

  it('passphrase mismatch shows inline error and does not call export', async () => {
    mockedInvoke.mockResolvedValueOnce({ ownerId: 'x', ownerDisplayName: 'me',
      devices: [{ deviceId: 'd', displayName: 'this', isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000, fingerprint: 'd·x' }],
      canBackUp: true });
    mockedInvoke.mockResolvedValueOnce({ recoveryToken: 'tok' });
    render(DevicesPanel);
    const btn = await screen.findByRole('button', { name: /back up owner identity/i });
    await fireEvent.click(btn);
    const passInput = await screen.findByLabelText('Passphrase');
    const confirmInput = screen.getByLabelText('Confirm passphrase');
    await fireEvent.input(passInput, { target: { value: 'first-passphrase' } });
    await fireEvent.input(confirmInput, { target: { value: 'second-passphrase' } });
    const saveBtn = screen.getByRole('button', { name: /save backup/i });
    await fireEvent.click(saveBtn);
    expect(screen.getByText(/do not match/i)).toBeInTheDocument();
    expect(invoke).not.toHaveBeenCalledWith(
      'export_owner_recovery_file_to_path',
      expect.anything(),
    );
  });
});

describe('DevicesPanel — degraded state (canBackUp: false)', () => {
  it('Back-up CTA is disabled when canBackUp is false', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'x', ownerDisplayName: 'me',
      devices: [{ deviceId: 'd', displayName: 'this', isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000, fingerprint: 'd·x' }],
      canBackUp: false,
    });
    render(DevicesPanel);
    const btn = await screen.findByRole('button', { name: /back up owner identity/i });
    expect(btn).toBeDisabled();
    expect(btn).toHaveAttribute('title');
  });
});

describe('DevicesPanel — rename overlay survives refresh', () => {
  it('overlays profile.displayName onto the isThisDevice row after refresh', async () => {
    // Backend returns the placeholder "this device" — but localStorage holds
    // a previously-renamed value. The panel must overlay the local value so
    // the rename survives refresh/restart (Qodo + CodeAnt finding).
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'a4f1c8239b7dd809abcdef0123456789',
      ownerDisplayName: 'this device',  // backend placeholder
      devices: [{
        deviceId: 'aa11bb22cc33dd44ee55ff6677889900',
        displayName: 'this device',  // backend placeholder
        isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000,
        fingerprint: 'aa11·bb22',
      }],
      canBackUp: true,
    });
    (loadProfile as ReturnType<typeof vi.fn>).mockReturnValue({
      address: 'addr',
      displayName: 'KRILE-renamed',
    });

    render(DevicesPanel);
    // Both the owner header AND the device row should show the renamed value.
    const matches = await screen.findAllByText('KRILE-renamed');
    expect(matches.length).toBe(2);
    // The "this device" string still appears as the marker label on the row,
    // confirming the overlay only replaced the displayName, not the marker.
    expect(screen.queryByText('this device')).toBeInTheDocument();
  });
});

describe('DevicesPanel — byte-cap on backup comment', () => {
  it('rejects backup comment that exceeds 256 bytes (multibyte aware)', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'x', ownerDisplayName: 'me',
      devices: [{ deviceId: 'd', displayName: 'this', isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000, fingerprint: 'd·x' }],
      canBackUp: true,
    });
    mockedInvoke.mockResolvedValueOnce({ recoveryToken: 'tok-1' });

    render(DevicesPanel);
    const backupBtn = await screen.findByRole('button', { name: /back up owner identity/i });
    await fireEvent.click(backupBtn);
    const passInput = await screen.findByLabelText('Passphrase');
    const confirmInput = screen.getByLabelText('Confirm passphrase');
    const commentInput = screen.getByLabelText('Comment');
    await fireEvent.input(passInput, { target: { value: 'a-strong-passphrase' } });
    await fireEvent.input(confirmInput, { target: { value: 'a-strong-passphrase' } });
    // 90 emoji × 4 bytes/emoji = 360 bytes, well over the 256-byte cap, even
    // though [...str].length only counts 90 codepoints. The byte-aware check
    // must reject.
    const longEmojiComment = '🌟'.repeat(90);
    await fireEvent.input(commentInput, { target: { value: longEmojiComment } });
    const saveBtn = screen.getByRole('button', { name: /save backup/i });
    await fireEvent.click(saveBtn);
    expect(screen.getByText(/at most 256 bytes/i)).toBeInTheDocument();
    // export must NOT have been called — validation precedes the invoke.
    expect(invoke).not.toHaveBeenCalledWith(
      'export_owner_recovery_file_to_path',
      expect.anything(),
    );
  });
});

describe('DevicesPanel — Save backup disabled when token unavailable', () => {
  it('disables Save backup if issue_owner_recovery_token fails on openBackup', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'x', ownerDisplayName: 'me',
      devices: [{ deviceId: 'd', displayName: 'this', isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000, fingerprint: 'd·x' }],
      canBackUp: true,
    });
    // issue_owner_recovery_token rejects
    mockedInvoke.mockRejectedValueOnce(new Error('keychain locked'));

    render(DevicesPanel);
    const backupBtn = await screen.findByRole('button', { name: /back up owner identity/i });
    await fireEvent.click(backupBtn);
    // Wait for the error to render (proves issue_owner_recovery_token resolved-with-rejection)
    await screen.findByText(/keychain locked/i);
    // The "Save backup" button must be disabled because recoveryToken is null.
    const saveBtn = screen.getByRole('button', { name: /save backup/i });
    expect(saveBtn).toBeDisabled();
  });
});

describe('DevicesPanel — stale token cleared after export failure', () => {
  it('clears recoveryToken in finally so next openBackup issues fresh', async () => {
    // Refresh + populated state
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'x',
      ownerDisplayName: 'me',
      devices: [{
        deviceId: 'd',
        displayName: 'this',
        isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000,
        fingerprint: 'd·x',
      }],
      canBackUp: true,
    });
    // First openBackup → issue token
    mockedInvoke.mockResolvedValueOnce({ recoveryToken: 'tok-1' });
    // exportRecoveryFile FAILS (e.g., disk write error). Backend already
    // consumed the token via take_token before the failure point.
    mockedInvoke.mockRejectedValueOnce(new Error('disk write failed'));
    // Next openBackup → must request a FRESH token (not replay tok-1).
    mockedInvoke.mockResolvedValueOnce({ recoveryToken: 'tok-2' });

    render(DevicesPanel);
    const backupBtn = await screen.findByRole('button', { name: /back up owner identity/i });
    await fireEvent.click(backupBtn);
    // Fill passphrases and trigger commit (will fail).
    const passInput = await screen.findByLabelText('Passphrase');
    const confirmInput = screen.getByLabelText('Confirm passphrase');
    await fireEvent.input(passInput, { target: { value: 'a-strong-passphrase' } });
    await fireEvent.input(confirmInput, { target: { value: 'a-strong-passphrase' } });
    const saveBtn = screen.getByRole('button', { name: /save backup/i });
    await fireEvent.click(saveBtn);
    await screen.findByText(/disk write failed/i);

    // Cancel → reopen backup. Must call issue_owner_recovery_token AGAIN
    // because the previous token is now consumed/invalid server-side.
    const cancelBtn = screen.getByRole('button', { name: /cancel/i });
    await fireEvent.click(cancelBtn);
    await fireEvent.click(backupBtn);

    // Assert two issue_owner_recovery_token calls (one before each open),
    // not one. The single-use semantics require a fresh token per attempt.
    const issueCalls = mockedInvoke.mock.calls.filter(
      (c) => c[0] === 'issue_owner_recovery_token',
    );
    expect(issueCalls.length).toBe(2);
  });
});

describe('DevicesPanel — mint→backup token reuse', () => {
  it('reuses the mint-issued token instead of issuing a fresh one on first openBackup', async () => {
    // Cursor finding: after handleConfirmMint sets recoveryToken, the user
    // immediately clicking "Back up owner identity →" should consume that
    // token, not discard it and issue another one. Two reasons:
    //   1) the second issue_owner_recovery_token call is wasted work and
    //      occupies a second slot in the server-side LRU cache (cap=8);
    //   2) it adds an unnecessary failure mode (issue can fail on locked
    //      keychain) to the happy mint→backup path even when a perfectly
    //      valid token is already in hand.
    // 1st invoke: get_owner_state (initial refresh) — returns null (un-minted).
    mockedInvoke.mockResolvedValueOnce(null);
    // 2nd invoke: mint_owner_identity → returns state + recoveryToken.
    mockedInvoke.mockResolvedValueOnce({
      state: {
        ownerId: 'a4f1c8239b7dd809abcdef0123456789',
        ownerDisplayName: 'this device',
        devices: [{
          deviceId: 'aa11bb22cc33dd44ee55ff6677889900',
          displayName: 'this device',
          isThisDevice: true,
          trustDecision: { kind: 'full', reason: null },
          enrolledAt: 1_700_000_000,
          fingerprint: 'aa11·bb22',
        }],
        canBackUp: true,
      },
      recoveryToken: 'mint-tok',
    });
    // No issue_owner_recovery_token mock set up — the test asserts the call
    // is NOT made. (If the regression returns, the unmocked invoke would
    // resolve undefined and the test would still detect it via the filter.)

    render(DevicesPanel);
    const bindBtn = await screen.findByRole('button', { name: /bind this device/i });
    await fireEvent.click(bindBtn);
    const confirmBtn = await screen.findByRole('button', { name: /^create owner identity/i });
    await fireEvent.click(confirmBtn);
    // Wait for transition to populated state.
    const backupBtn = await screen.findByRole('button', { name: /back up owner identity/i });
    await fireEvent.click(backupBtn);
    // Modal should be open, but no extra issue_owner_recovery_token call —
    // the cached mint-issued token is reused.
    expect(await screen.findByRole('dialog')).toBeInTheDocument();
    const issueCalls = mockedInvoke.mock.calls.filter(
      (c) => c[0] === 'issue_owner_recovery_token',
    );
    expect(issueCalls.length).toBe(0);
  });
});

describe('DevicesPanel — stale backupError cleared between attempts', () => {
  it('clears prior commit error before re-validating on the next click', async () => {
    // Cursor round-5 finding: a backupError from a previous failed commit
    // attempt within the same modal session would render alongside (or
    // instead of) the current attempt's outcome unless cleared. The fix is
    // backupError = null at the top of commitBackup before re-validating.
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'x',
      ownerDisplayName: 'me',
      devices: [{
        deviceId: 'd',
        displayName: 'this',
        isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000,
        fingerprint: 'd·x',
      }],
      canBackUp: true,
    });
    mockedInvoke.mockResolvedValueOnce({ recoveryToken: 'tok-1' });

    render(DevicesPanel);
    const backupBtn = await screen.findByRole('button', { name: /back up owner identity/i });
    await fireEvent.click(backupBtn);
    const passInput = await screen.findByLabelText('Passphrase');
    const confirmInput = screen.getByLabelText('Confirm passphrase');

    // First attempt: too short → renders length error.
    await fireEvent.input(passInput, { target: { value: 'short' } });
    await fireEvent.input(confirmInput, { target: { value: 'short' } });
    const saveBtn = screen.getByRole('button', { name: /save backup/i });
    await fireEvent.click(saveBtn);
    expect(screen.getByText(/at least 12 characters/i)).toBeInTheDocument();

    // Second attempt: long enough but mismatched. The "at least 12 characters"
    // string MUST be gone; only "do not match" should render.
    await fireEvent.input(passInput, { target: { value: 'twelve-char-passphrase' } });
    await fireEvent.input(confirmInput, { target: { value: 'different-passphrase' } });
    await fireEvent.click(saveBtn);
    expect(screen.queryByText(/at least 12 characters/i)).not.toBeInTheDocument();
    expect(screen.getByText(/do not match/i)).toBeInTheDocument();
  });
});
