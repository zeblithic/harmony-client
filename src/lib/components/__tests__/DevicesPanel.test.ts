import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import DevicesPanel from '../DevicesPanel.svelte';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));

import { invoke } from '@tauri-apps/api/core';

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
beforeEach(() => {
  vi.resetAllMocks();
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

  it('renders footer for adding another device', async () => {
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
    await screen.findByRole('button', { name: /add another device/i });
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
    // request_export_save_path → returns opaque path token. (User picked a
    // location in the native dialog; backend cached the path under this UUID.)
    mockedInvoke.mockResolvedValueOnce('path-token-uuid');
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

describe('DevicesPanel — closeBackup wipes sensitive state', () => {
  it('clears passphrase fields when modal is closed and on reopen', async () => {
    // Cursor round-6 finding: closeBackup was leaving backupPassphrase /
    // backupPassphraseConfirm / backupComment populated in component state.
    // openBackup wipes them on the NEXT open, but in between sessions they
    // sit in JS heap unnecessarily. The fix wipes in closeBackup too so
    // they don't linger in the panel's lifetime between opens.
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
    mockedInvoke.mockResolvedValueOnce({ recoveryToken: 'tok-2' });

    render(DevicesPanel);
    const backupBtn = await screen.findByRole('button', { name: /back up owner identity/i });
    await fireEvent.click(backupBtn);

    const passInput = await screen.findByLabelText('Passphrase');
    const confirmInput = screen.getByLabelText('Confirm passphrase');
    const commentInput = screen.getByLabelText('Comment');
    await fireEvent.input(passInput, { target: { value: 'my-secret-passphrase' } });
    await fireEvent.input(confirmInput, { target: { value: 'my-secret-passphrase' } });
    await fireEvent.input(commentInput, { target: { value: 'a note' } });

    // Cancel — must clear sensitive fields so they don't sit in component
    // state until the next openBackup. Reopen and verify the inputs are
    // empty (which is the user-observable proxy for "not in state").
    const cancelBtn = screen.getByRole('button', { name: /cancel/i });
    await fireEvent.click(cancelBtn);
    await fireEvent.click(backupBtn);

    const passInput2 = await screen.findByLabelText('Passphrase');
    const confirmInput2 = screen.getByLabelText('Confirm passphrase');
    const commentInput2 = screen.getByLabelText('Comment');
    expect((passInput2 as HTMLInputElement).value).toBe('');
    expect((confirmInput2 as HTMLInputElement).value).toBe('');
    expect((commentInput2 as HTMLInputElement).value).toBe('');
  });
});

describe('DevicesPanel — comment byte-cap validates trimmed value', () => {
  it('accepts a comment whose untrimmed bytes exceed 256 but trimmed bytes fit', async () => {
    // Cursor round-6 finding: the validator was checking byte length of the
    // raw input but sending backupComment.trim() to the backend. So a
    // comment like "abc" + 254 trailing spaces would falsely reject (raw =
    // 257 bytes, trimmed = 3). Fix validates the same string sent to backend.
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
    // request_export_save_path → opaque path token from backend dialog.
    mockedInvoke.mockResolvedValueOnce('path-token-uuid');
    // Successful export → returns ExportInfo. path field is required by the
    // post-Task-6 caller (backupSavedPath = info.path).
    mockedInvoke.mockResolvedValueOnce({
      identityHash: 'hash',
      byteLen: 0,
      path: '/tmp/owner-recovery.bin',
    });

    render(DevicesPanel);
    const backupBtn = await screen.findByRole('button', { name: /back up owner identity/i });
    await fireEvent.click(backupBtn);
    const passInput = await screen.findByLabelText('Passphrase');
    const confirmInput = screen.getByLabelText('Confirm passphrase');
    const commentInput = screen.getByLabelText('Comment');
    await fireEvent.input(passInput, { target: { value: 'a-strong-passphrase' } });
    await fireEvent.input(confirmInput, { target: { value: 'a-strong-passphrase' } });
    // 3 chars + 270 trailing spaces = 273 bytes raw, 3 bytes trimmed. The
    // pre-fix validator rejected on raw bytes; the post-fix validator
    // accepts because the trimmed form fits well under 256.
    const commentWithTrailingSpaces = 'abc' + ' '.repeat(270);
    await fireEvent.input(commentInput, { target: { value: commentWithTrailingSpaces } });
    const saveBtn = screen.getByRole('button', { name: /save backup/i });
    await fireEvent.click(saveBtn);

    // No "at most 256 bytes" error.
    expect(screen.queryByText(/at most 256 bytes/i)).not.toBeInTheDocument();
    // The export call MUST have been made, with the trimmed comment passed
    // (not the untrimmed-with-trailing-spaces version) and with the opaque
    // path token rather than a literal filesystem path.
    const exportCalls = mockedInvoke.mock.calls.filter(
      (c) => c[0] === 'export_owner_recovery_file_to_path',
    );
    expect(exportCalls.length).toBe(1);
    expect(exportCalls[0][1]).toMatchObject({
      comment: 'abc',
      pathToken: 'path-token-uuid',
    });
  });
});

describe('DevicesPanel — Track B v2 pairing CTAs', () => {
  it('empty state renders both Bind and Join CTAs', async () => {
    mockedInvoke.mockResolvedValueOnce(null); // get_owner_state -> null
    render(DevicesPanel);
    expect(await screen.findByRole('button', { name: /bind this device/i })).toBeInTheDocument();
    expect(await screen.findByRole('button', { name: /join existing identity/i })).toBeInTheDocument();
  });

  it('populated state renders an active "Add another device" button', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'a4f1c8239b7dd809abcdef0123456789',
      ownerDisplayName: 'me',
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
    const btn = await screen.findByRole('button', { name: /add another device/i });
    expect(btn).toBeInTheDocument();
    expect(btn).not.toBeDisabled();
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

describe('DevicesPanel — backend save dialog (request_export_save_path)', () => {
  it('invokes request_export_save_path with the recovery-file dialog descriptor', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'x',
      ownerDisplayName: 'me',
      devices: [{
        deviceId: 'd', displayName: 'this', isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000, fingerprint: 'd·x',
      }],
      canBackUp: true,
    });
    mockedInvoke.mockResolvedValueOnce({ recoveryToken: 'tok-1' });
    mockedInvoke.mockResolvedValueOnce('path-token-uuid');
    mockedInvoke.mockResolvedValueOnce({
      identityHash: 'hash',
      byteLen: 4096,
      path: '/tmp/owner-recovery.bin',
    });

    render(DevicesPanel);
    const backupBtn = await screen.findByRole('button', { name: /back up owner identity/i });
    await fireEvent.click(backupBtn);
    const passInput = await screen.findByLabelText('Passphrase');
    const confirmInput = screen.getByLabelText('Confirm passphrase');
    await fireEvent.input(passInput, { target: { value: 'a-strong-passphrase' } });
    await fireEvent.input(confirmInput, { target: { value: 'a-strong-passphrase' } });
    const saveBtn = screen.getByRole('button', { name: /save backup/i });
    await fireEvent.click(saveBtn);

    // The renderer no longer touches @tauri-apps/plugin-dialog directly; it
    // delegates dialog presentation to the backend via request_export_save_path.
    expect(invoke).toHaveBeenCalledWith('request_export_save_path', {
      request: {
        defaultFilename: 'owner-recovery.bin',
        filterName: 'Recovery file',
        filterExtensions: ['bin'],
      },
    });
  });

  it('skips export when request_export_save_path returns null (user cancelled)', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'x',
      ownerDisplayName: 'me',
      devices: [{
        deviceId: 'd', displayName: 'this', isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000, fingerprint: 'd·x',
      }],
      canBackUp: true,
    });
    mockedInvoke.mockResolvedValueOnce({ recoveryToken: 'tok-1' });
    // User cancelled the native save dialog → backend returns null.
    mockedInvoke.mockResolvedValueOnce(null);

    render(DevicesPanel);
    const backupBtn = await screen.findByRole('button', { name: /back up owner identity/i });
    await fireEvent.click(backupBtn);
    const passInput = await screen.findByLabelText('Passphrase');
    const confirmInput = screen.getByLabelText('Confirm passphrase');
    await fireEvent.input(passInput, { target: { value: 'a-strong-passphrase' } });
    await fireEvent.input(confirmInput, { target: { value: 'a-strong-passphrase' } });
    const saveBtn = screen.getByRole('button', { name: /save backup/i });
    await fireEvent.click(saveBtn);

    // Cancellation is a silent no-op: no error, no export call.
    expect(invoke).not.toHaveBeenCalledWith(
      'export_owner_recovery_file_to_path',
      expect.anything(),
    );
  });

  it('passes the opaque pathToken (not a filesystem path) to export_owner_recovery_file_to_path', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'x',
      ownerDisplayName: 'me',
      devices: [{
        deviceId: 'd', displayName: 'this', isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000, fingerprint: 'd·x',
      }],
      canBackUp: true,
    });
    mockedInvoke.mockResolvedValueOnce({ recoveryToken: 'tok-1' });
    mockedInvoke.mockResolvedValueOnce('path-token-uuid');
    mockedInvoke.mockResolvedValueOnce({
      identityHash: 'hash',
      byteLen: 4096,
      path: '/tmp/owner-recovery.bin',
    });

    render(DevicesPanel);
    const backupBtn = await screen.findByRole('button', { name: /back up owner identity/i });
    await fireEvent.click(backupBtn);
    const passInput = await screen.findByLabelText('Passphrase');
    const confirmInput = screen.getByLabelText('Confirm passphrase');
    await fireEvent.input(passInput, { target: { value: 'a-strong-passphrase' } });
    await fireEvent.input(confirmInput, { target: { value: 'a-strong-passphrase' } });
    const saveBtn = screen.getByRole('button', { name: /save backup/i });
    await fireEvent.click(saveBtn);

    expect(invoke).toHaveBeenCalledWith(
      'export_owner_recovery_file_to_path',
      expect.objectContaining({
        recoveryToken: 'tok-1',
        pathToken: 'path-token-uuid',
        passphrase: 'a-strong-passphrase',
      }),
    );
  });

  it('renders the saved-path affordance using ExportInfo.path from the backend', async () => {
    // Closes the loop on the path-token redesign: the renderer no longer
    // chooses or even knows the final filesystem path until the backend
    // returns ExportInfo. The "saved" banner must source from info.path so
    // the UI reflects exactly what was written, not what we asked for.
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'x',
      ownerDisplayName: 'me',
      devices: [{
        deviceId: 'd', displayName: 'this', isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000, fingerprint: 'd·x',
      }],
      canBackUp: true,
    });
    mockedInvoke.mockResolvedValueOnce({ recoveryToken: 'tok-1' });
    mockedInvoke.mockResolvedValueOnce('path-token-uuid');
    mockedInvoke.mockResolvedValueOnce({
      identityHash: 'hash',
      byteLen: 4096,
      // Distinct from the dialog's defaultFilename so a regression that
      // re-introduces the renderer-chosen path would surface here.
      path: '/Users/test/Desktop/owner-recovery.bin',
    });

    render(DevicesPanel);
    const backupBtn = await screen.findByRole('button', { name: /back up owner identity/i });
    await fireEvent.click(backupBtn);
    const passInput = await screen.findByLabelText('Passphrase');
    const confirmInput = screen.getByLabelText('Confirm passphrase');
    await fireEvent.input(passInput, { target: { value: 'a-strong-passphrase' } });
    await fireEvent.input(confirmInput, { target: { value: 'a-strong-passphrase' } });
    const saveBtn = screen.getByRole('button', { name: /save backup/i });
    await fireEvent.click(saveBtn);

    // The success banner must echo the backend-reported path verbatim.
    await screen.findByText('/Users/test/Desktop/owner-recovery.bin');
  });
});

describe('DevicesPanel — mint modal a11y (ZEB-195)', () => {
  it('moves focus into the modal when opened, restores it on close via Escape', async () => {
    mockedInvoke.mockResolvedValueOnce(null);
    render(DevicesPanel);
    const trigger = await screen.findByRole('button', { name: /bind this device/i });
    trigger.focus();
    expect(document.activeElement).toBe(trigger);
    await fireEvent.click(trigger);
    const dialog = await screen.findByRole('dialog');
    expect(dialog.contains(document.activeElement)).toBe(true);
    await fireEvent.keyDown(dialog, { key: 'Escape' });
    expect(screen.queryByRole('dialog')).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });

  it('Escape closes the mint modal', async () => {
    mockedInvoke.mockResolvedValueOnce(null);
    render(DevicesPanel);
    const trigger = await screen.findByRole('button', { name: /bind this device/i });
    await fireEvent.click(trigger);
    const dialog = await screen.findByRole('dialog');
    await fireEvent.keyDown(dialog, { key: 'Escape' });
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('Escape is a no-op while mintInFlight', async () => {
    mockedInvoke.mockResolvedValueOnce(null);
    // mint_owner_identity returns a pending promise so mintInFlight stays true.
    let resolveMint: (value: unknown) => void = () => {};
    const pendingMint = new Promise((resolve) => { resolveMint = resolve; });
    mockedInvoke.mockReturnValueOnce(pendingMint);

    render(DevicesPanel);
    const trigger = await screen.findByRole('button', { name: /bind this device/i });
    await fireEvent.click(trigger);
    const dialog = await screen.findByRole('dialog');
    // Click "Create owner identity" inside the modal — this starts the mint
    // flow and sets mintInFlight = true. Do NOT await: we want the in-flight
    // state to persist across the Escape press.
    const confirmBtn = screen.getByRole('button', { name: /^create owner identity/i });
    fireEvent.click(confirmBtn);
    // Press Escape — must be a no-op because canCancel={!mintInFlight} is false.
    await fireEvent.keyDown(dialog, { key: 'Escape' });
    expect(screen.queryByRole('dialog')).toBeInTheDocument();
    // Resolve the pending promise so the test cleans up gracefully.
    resolveMint({
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
    });
  });
});

describe('DevicesPanel — backup modal a11y (ZEB-195)', () => {
  it('moves focus into the modal when opened, restores it on close via Escape', async () => {
    // Setup mirrors the existing "clicking Back up opens the backup modal and
    // issues a token if needed" test: populated state with canBackUp=true,
    // followed by issue_owner_recovery_token.
    mockedInvoke
      .mockResolvedValueOnce({
        ownerId: 'x',
        ownerDisplayName: 'me',
        devices: [{
          deviceId: 'd', displayName: 'this', isThisDevice: true,
          trustDecision: { kind: 'full', reason: null },
          enrolledAt: 1_700_000_000, fingerprint: 'd·x',
        }],
        canBackUp: true,
      })
      .mockResolvedValueOnce({ recoveryToken: 'tok-1' });

    render(DevicesPanel);
    const trigger = await screen.findByRole('button', { name: /back up owner identity/i });
    trigger.focus();
    expect(document.activeElement).toBe(trigger);
    await fireEvent.click(trigger);
    const dialog = await screen.findByRole('dialog');
    expect(dialog.contains(document.activeElement)).toBe(true);
    await fireEvent.keyDown(dialog, { key: 'Escape' });
    expect(screen.queryByRole('dialog')).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });

  it('Escape closes the backup modal', async () => {
    mockedInvoke
      .mockResolvedValueOnce({
        ownerId: 'x',
        ownerDisplayName: 'me',
        devices: [{
          deviceId: 'd', displayName: 'this', isThisDevice: true,
          trustDecision: { kind: 'full', reason: null },
          enrolledAt: 1_700_000_000, fingerprint: 'd·x',
        }],
        canBackUp: true,
      })
      .mockResolvedValueOnce({ recoveryToken: 'tok-1' });

    render(DevicesPanel);
    const trigger = await screen.findByRole('button', { name: /back up owner identity/i });
    await fireEvent.click(trigger);
    const dialog = await screen.findByRole('dialog');
    await fireEvent.keyDown(dialog, { key: 'Escape' });
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('Escape is a no-op while backupDialogInFlight', async () => {
    // Open backup modal, fill matching ≥12-codepoint passphrases, click Save
    // backup. While the request_export_save_path promise is pending,
    // backupDialogInFlight = true → canCancel = false → Escape must no-op.
    mockedInvoke
      .mockResolvedValueOnce({
        ownerId: 'x',
        ownerDisplayName: 'me',
        devices: [{
          deviceId: 'd', displayName: 'this', isThisDevice: true,
          trustDecision: { kind: 'full', reason: null },
          enrolledAt: 1_700_000_000, fingerprint: 'd·x',
        }],
        canBackUp: true,
      })
      .mockResolvedValueOnce({ recoveryToken: 'tok-1' });

    // request_export_save_path returns a pending promise so
    // backupDialogInFlight stays true while we press Escape.
    let resolvePath: (value: unknown) => void = () => {};
    const pendingPath = new Promise((resolve) => { resolvePath = resolve; });
    mockedInvoke.mockReturnValueOnce(pendingPath);

    render(DevicesPanel);
    const trigger = await screen.findByRole('button', { name: /back up owner identity/i });
    await fireEvent.click(trigger);
    const dialog = await screen.findByRole('dialog');
    const passInput = await screen.findByLabelText('Passphrase');
    const confirmInput = screen.getByLabelText('Confirm passphrase');
    await fireEvent.input(passInput, { target: { value: 'a-strong-passphrase' } });
    await fireEvent.input(confirmInput, { target: { value: 'a-strong-passphrase' } });
    const saveBtn = screen.getByRole('button', { name: /save backup/i });
    // Do NOT await — we want backupDialogInFlight = true while we press Escape.
    fireEvent.click(saveBtn);
    // Press Escape — must be a no-op because canCancel is false while the
    // path-token request is in flight.
    await fireEvent.keyDown(dialog, { key: 'Escape' });
    expect(screen.queryByRole('dialog')).toBeInTheDocument();
    // Resolve the pending dialog promise (returns null = user cancelled) so
    // the test cleans up gracefully.
    resolvePath(null);
  });
});
