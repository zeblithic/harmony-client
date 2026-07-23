import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { tick } from 'svelte';
import DevicesPanel from '../DevicesPanel.svelte';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));

// ZEB-668 S6: the replace flow mounts PairingInviter inside the panel, whose
// PairingService.init() awaits listen(). Unmocked, jsdom's listen rejects and
// the inviter dies before fetching its snapshot. Resolved per-test in the
// file-level beforeEach (resetAllMocks wipes the factory's return value).
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn(),
}));

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

const mockedListen = listen as unknown as ReturnType<typeof vi.fn>;

import { loadProfile, saveProfile } from '../../profile-service';

vi.mock('../../profile-service', () => ({
  loadProfile: vi.fn(),
  saveProfile: vi.fn(),
}));

import {
  loadDeviceLabel,
  saveDeviceLabel,
  clearDeviceLabel,
  resolveDefaultDeviceLabel,
} from '../../device-label-service';

vi.mock('../../device-label-service', () => ({
  loadDeviceLabel: vi.fn(),
  saveDeviceLabel: vi.fn(),
  clearDeviceLabel: vi.fn(),
  resolveDefaultDeviceLabel: vi.fn(),
}));

import { fetchCommunitiesCount } from '../../owner-meta';

// ZEB-650: mocked as a unit so the count fetch never consumes this file's
// ordered invoke stubs. resetAllMocks leaves it returning undefined for the
// pre-existing tests — the component treats that as "omit the fact".
vi.mock('../../owner-meta', () => ({
  fetchCommunitiesCount: vi.fn(),
}));

const mockedCommunitiesCount = fetchCommunitiesCount as unknown as ReturnType<typeof vi.fn>;

const mockedInvoke = invoke as unknown as ReturnType<typeof vi.fn>;

// File-level beforeEach: runs before EVERY test in this file, including those
// in nested describe blocks. resetAllMocks (not clearAllMocks) wipes
// mockReturnValue/mockResolvedValue implementations so stubs from one suite
// don't leak into the next (e.g., loadProfile.mockReturnValue from rename
// suites bleeding into populated-state suites via applyLocalProfileOverlay).
beforeEach(() => {
  vi.resetAllMocks();
  // resetAllMocks leaves listen returning undefined, and the panel calls
  // `.then` on its result — restore a resolved no-op unlistener.
  mockedListen.mockResolvedValue(() => {});
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

  it('shows a self-sovereign badge but no rotation/danger chrome beyond Remove', async () => {
    // Honesty ledger §0.5, amended by ZEB-668 S2: `revoke_device` now exists,
    // so Remove affordances render (gated tests below). Rotation and
    // delete-identity still have no backing IPC and must NOT be invented.
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
    expect(screen.getByText(/self-sovereign/i)).toBeTruthy();
    expect(screen.queryByText(/rotate keys/i)).toBeNull();
    expect(screen.queryByText(/delete this identity/i)).toBeNull();
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
    // Exact match: the ZEB-668 "Remove this device" button also contains the
    // phrase, so the marker is asserted by its full normalized text.
    expect(screen.getByText('this device')).toBeInTheDocument();
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

  it('saving the rename persists the device label, not the owner profile', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'a4f1', ownerDisplayName: 'zeblith',
      devices: [{
        deviceId: 'aa11bb22', displayName: 'KRILE', isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000, fingerprint: 'aa11·bb22',
      }],
      canBackUp: true,
    });
    (loadDeviceLabel as ReturnType<typeof vi.fn>).mockReturnValue('KRILE');

    render(DevicesPanel);
    const renameBtn = await screen.findByRole('button', { name: /rename/i });
    await fireEvent.click(renameBtn);
    const input = screen.getByRole('textbox', { name: /device name/i });
    await fireEvent.input(input, { target: { value: 'KRILE-prime' } });
    const saveBtn = screen.getByRole('button', { name: /save/i });
    await fireEvent.click(saveBtn);
    // ZEB-336: rename writes the per-device LABEL, never the owner profile.
    expect(saveDeviceLabel).toHaveBeenCalledWith('KRILE-prime');
    expect(saveProfile).not.toHaveBeenCalled();
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

describe('DevicesPanel — fleet-epoch rotate affordance (ZEB-677 S5)', () => {
  const staleState = (extra: Record<string, unknown>) => ({
    ownerId: 'a4f1c8239b7dd809abcdef0123456789',
    ownerDisplayName: 'me',
    devices: [{
      deviceId: 'aa11bb22cc33dd44ee55ff6677889900',
      displayName: 'this', isThisDevice: true,
      trustDecision: { kind: 'full', reason: null },
      enrolledAt: 1_700_000_000, fingerprint: 'aa11·bb22',
    }],
    fleetEpoch: 2,
    fleetEpochStale: true,
    ...extra,
  });

  it('seed-holder sees the direct rotate button', async () => {
    mockedInvoke.mockResolvedValueOnce(staleState({ canBackUp: true }));
    render(DevicesPanel);
    expect(await screen.findByTestId('rotate-fleet-keys')).toBeInTheDocument();
    expect(screen.queryByTestId('rotate-fleet-keys-quorum')).toBeNull();
  });

  it('master-less quorum-capable fleet sees the co-sign rotate button', async () => {
    mockedInvoke.mockResolvedValueOnce(
      staleState({ canBackUp: false, selfIsMaster: false, canArmEnrollment: true }),
    );
    render(DevicesPanel);
    const btn = await screen.findByTestId('rotate-fleet-keys-quorum');
    expect(btn).toBeInTheDocument();
    expect(screen.queryByTestId('rotate-fleet-keys')).toBeNull();

    mockedInvoke.mockResolvedValue(null); // the request + refresh calls
    await fireEvent.click(btn);
    await waitFor(() =>
      expect(mockedInvoke).toHaveBeenCalledWith('request_quorum_epoch_bump'),
    );
  });

  it('non-seed device without a quorum sees only the master-device note', async () => {
    mockedInvoke.mockResolvedValueOnce(
      staleState({ canBackUp: false, selfIsMaster: false, canArmEnrollment: false }),
    );
    render(DevicesPanel);
    expect(
      await screen.findByText(/rotate them from the device that holds your master key/i),
    ).toBeInTheDocument();
    expect(screen.queryByTestId('rotate-fleet-keys')).toBeNull();
    expect(screen.queryByTestId('rotate-fleet-keys-quorum')).toBeNull();
  });
});

describe('DevicesPanel — owner name and device label are separated (ZEB-336)', () => {
  it('shows the owner name in the header and the device label in the row', async () => {
    // Backend returns placeholders for both; the local stores override them
    // INDEPENDENTLY — the owner name and device label are distinct values.
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'a4f1c8239b7dd809abcdef0123456789',
      ownerDisplayName: 'backend-placeholder',
      devices: [{
        deviceId: 'aa11bb22cc33dd44ee55ff6677889900',
        displayName: 'this device', // backend placeholder
        isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000,
        fingerprint: 'aa11·bb22',
      }],
      canBackUp: true,
    });
    (loadProfile as ReturnType<typeof vi.fn>).mockReturnValue({ address: 'addr', displayName: 'zeblith' });
    (loadDeviceLabel as ReturnType<typeof vi.fn>).mockReturnValue('KRILE');

    render(DevicesPanel);
    await screen.findByText('zeblith');           // owner header ← profile
    expect(screen.getByText('KRILE')).toBeInTheDocument();   // device row ← label store
    expect(screen.queryByText('backend-placeholder')).not.toBeInTheDocument();
  });

  it('renaming the device does not change the owner display name', async () => {
    // Regression guard for the conflation: pre-split this rename rewrote
    // profile.displayName (the owner name).
    const deviceRow = {
      deviceId: 'aa11bb22', displayName: 'this device', isThisDevice: true,
      trustDecision: { kind: 'full', reason: null },
      enrolledAt: 1_700_000_000, fingerprint: 'aa11·bb22',
      deviceVkHex: 'aa'.repeat(32),
    };
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'a4f1', ownerDisplayName: 'backend',
      devices: [deviceRow],
      canBackUp: true,
    });
    // ZEB-668 S4: rename now flows through set_device_petname + a refresh;
    // the refreshed view carries the fleet petname.
    mockedInvoke.mockResolvedValueOnce(undefined); // set_device_petname
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'a4f1', ownerDisplayName: 'backend',
      devices: [{ ...deviceRow, petName: 'KRILE-prime' }],
      canBackUp: true,
    });
    (loadProfile as ReturnType<typeof vi.fn>).mockReturnValue({ address: 'addr', displayName: 'zeblith' });
    (loadDeviceLabel as ReturnType<typeof vi.fn>).mockReturnValue('KRILE');

    render(DevicesPanel);
    const renameBtn = await screen.findByRole('button', { name: /rename/i });
    await fireEvent.click(renameBtn);
    const input = screen.getByRole('textbox', { name: /device name/i });
    await fireEvent.input(input, { target: { value: 'KRILE-prime' } });
    await fireEvent.click(screen.getByRole('button', { name: /save/i }));
    await screen.findByText('KRILE-prime'); // device row updated (post-refresh)
    expect(screen.getByText('zeblith')).toBeInTheDocument();     // owner header unchanged
  });

  it('defaults the device label to the OS hostname when none is stored, without persisting it', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'a4f1', ownerDisplayName: 'zeblith',
      devices: [{
        deviceId: 'aa11bb22', displayName: 'this device', isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000, fingerprint: 'aa11·bb22',
      }],
      canBackUp: true,
    });
    (loadDeviceLabel as ReturnType<typeof vi.fn>).mockReturnValue(null);
    (resolveDefaultDeviceLabel as ReturnType<typeof vi.fn>).mockResolvedValue('HOSTBOX');

    render(DevicesPanel);
    await screen.findByText('HOSTBOX'); // resolved hostname shown as the label
    // The auto-default is DISPLAY-only and resolved fresh each launch — it must
    // NOT be persisted, so a transient hostname failure can't lock in a
    // fallback. Only a user rename persists. (CodeRabbit, PR #180.)
    expect(saveDeviceLabel).not.toHaveBeenCalled();
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
    // ZEB-196: cancelling with a live (un-exported) token now fires a
    // fire-and-forget revoke_owner_recovery_token before the reopen re-issues.
    mockedInvoke.mockResolvedValueOnce(undefined);
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

describe('DevicesPanel — ZEB-196 revoke unconsumed token on cancel', () => {
  const populatedOwner = {
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
  };

  it('revokes the server-side token when the backup modal is cancelled without exporting', async () => {
    // An issued-but-unconsumed token must be dropped server-side on cancel so
    // it can't linger for its 5-minute TTL and LRU-evict a legitimate live
    // token. closeBackup fires revoke_owner_recovery_token(token).
    mockedInvoke.mockResolvedValueOnce(populatedOwner); // mount refresh
    mockedInvoke.mockResolvedValueOnce({ recoveryToken: 'live-tok' }); // openBackup issue
    mockedInvoke.mockResolvedValueOnce(undefined); // revoke_owner_recovery_token

    render(DevicesPanel);
    const backupBtn = await screen.findByRole('button', { name: /back up owner identity/i });
    await fireEvent.click(backupBtn);
    // Modal open with a live token; cancel WITHOUT exporting.
    const cancelBtn = await screen.findByRole('button', { name: /cancel/i });
    await fireEvent.click(cancelBtn);

    const revokeCalls = mockedInvoke.mock.calls.filter(
      (c) => c[0] === 'revoke_owner_recovery_token',
    );
    expect(revokeCalls.length).toBe(1);
    expect(revokeCalls[0][1]).toEqual({ recoveryToken: 'live-tok' });
  });

  it('does NOT revoke on the happy export path (token already consumed)', async () => {
    // After a successful export, commitBackup nulls the token in its finally
    // block, so closeBackup sees null and issues no wasted revoke.
    mockedInvoke.mockResolvedValueOnce(populatedOwner); // mount refresh
    mockedInvoke.mockResolvedValueOnce({ recoveryToken: 'exp-tok' }); // openBackup issue
    mockedInvoke.mockResolvedValueOnce('path-token-uuid'); // request_export_save_path
    mockedInvoke.mockResolvedValueOnce({ identityHash: 'h', byteLen: 0, path: '/tmp/x.bin' }); // export

    render(DevicesPanel);
    const backupBtn = await screen.findByRole('button', { name: /back up owner identity/i });
    await fireEvent.click(backupBtn);
    const passInput = await screen.findByLabelText('Passphrase');
    const confirmInput = screen.getByLabelText('Confirm passphrase');
    await fireEvent.input(passInput, { target: { value: 'a-strong-passphrase' } });
    await fireEvent.input(confirmInput, { target: { value: 'a-strong-passphrase' } });
    await fireEvent.click(screen.getByRole('button', { name: /save backup/i }));
    // Wait for the export to complete (saved-path confirmation renders "Done").
    const doneBtn = await screen.findByRole('button', { name: /done/i });
    await fireEvent.click(doneBtn);

    const revokeCalls = mockedInvoke.mock.calls.filter(
      (c) => c[0] === 'revoke_owner_recovery_token',
    );
    expect(revokeCalls.length).toBe(0);
  });

  it('revokes a token that resolves AFTER the modal was cancelled (in-flight guard)', async () => {
    // ZEB-196 (Qodo): if the user cancels while issueRecoveryToken() is still
    // pending, closeBackup can't revoke a token it doesn't hold yet. The
    // generation guard must revoke the token when it finally arrives, and must
    // not populate recoveryToken while the modal is closed.
    mockedInvoke.mockResolvedValueOnce(populatedOwner); // mount refresh
    // openBackup's issue: a promise we resolve only AFTER cancel.
    let resolveIssue!: (v: { recoveryToken: string }) => void;
    const issuePromise = new Promise<{ recoveryToken: string }>((r) => {
      resolveIssue = r;
    });
    mockedInvoke.mockReturnValueOnce(issuePromise); // issue_owner_recovery_token (pending)
    mockedInvoke.mockResolvedValueOnce(undefined); // revoke_owner_recovery_token

    render(DevicesPanel);
    const backupBtn = await screen.findByRole('button', { name: /back up owner identity/i });
    await fireEvent.click(backupBtn); // openBackup → issue starts, stays pending
    // Cancel BEFORE the token resolves — closeBackup bumps the generation.
    const cancelBtn = await screen.findByRole('button', { name: /cancel/i });
    await fireEvent.click(cancelBtn);

    // The token now arrives: it must be revoked (stale generation), not stored.
    resolveIssue({ recoveryToken: 'late-tok' });
    await waitFor(() => {
      const revokeCalls = mockedInvoke.mock.calls.filter(
        (c) => c[0] === 'revoke_owner_recovery_token',
      );
      expect(revokeCalls.length).toBe(1);
      expect(revokeCalls[0][1]).toEqual({ recoveryToken: 'late-tok' });
    });
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
    // ZEB-203: wording matches the canonical copy in IdentityPanel + backend
    // ("Recovery passphrase must be at least N characters."), not a divergent
    // DevicesPanel-only string.
    expect(
      screen.getByText(/Recovery passphrase must be at least 12 characters/i),
    ).toBeInTheDocument();

    // Second attempt: long enough but mismatched. The length-error string MUST
    // be gone; only "do not match" should render.
    await fireEvent.input(passInput, { target: { value: 'twelve-char-passphrase' } });
    await fireEvent.input(confirmInput, { target: { value: 'different-passphrase' } });
    await fireEvent.click(saveBtn);
    expect(
      screen.queryByText(/Recovery passphrase must be at least 12 characters/i),
    ).not.toBeInTheDocument();
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

describe('DevicesPanel — butler pin toggle ID form (ZEB-418 P2 round-2)', () => {
  it('passes deviceVkHex (the SP1 verify-key form), never deviceId, to set_butler_pin', async () => {
    // Round-2 Greptile P1: the backend's enrolled-set check only accepts the
    // 64-hex VERIFY-KEY id (deviceVkHex). The pre-fix handler sent deviceId
    // (the 32-hex identity hash) and was rejected for every device.
    const identityHashHex = 'aa11bb22cc33dd44ee55ff6677889900'; // 32-hex
    const vkHex = 'cc'.repeat(32); // 64-hex SP1 form
    const view = {
      ownerId: 'a4f1c8239b7dd809abcdef0123456789',
      ownerDisplayName: 'zeblith',
      devices: [{
        deviceId: identityHashHex,
        deviceVkHex: vkHex,
        displayName: 'KRILE',
        isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000,
        fingerprint: 'aa11·bb22',
        butlerPinned: false,
      }],
      canBackUp: true,
    };
    mockedInvoke
      .mockResolvedValueOnce(view)       // get_owner_state on mount
      .mockResolvedValueOnce(undefined)  // set_butler_pin
      .mockResolvedValueOnce(view);      // get_owner_state refresh after toggle

    render(DevicesPanel);
    await screen.findByText('KRILE');
    const toggle = screen.getByRole('checkbox', { name: /set KRILE as always-on butler/i });
    await fireEvent.click(toggle);

    expect(mockedInvoke).toHaveBeenCalledWith('set_butler_pin', { deviceId: vkHex });
    expect(mockedInvoke).not.toHaveBeenCalledWith('set_butler_pin', {
      deviceId: identityHashHex,
    });
  });
});

describe('DevicesPanel meta row + backup stamps (ZEB-650 slice 1)', () => {
  const metaView = {
    ownerId: 'a4f1c8239b7dd809abcdef0123456789',
    ownerDisplayName: 'zeblith',
    devices: [
      {
        deviceId: 'aa11bb22cc33dd44ee55ff6677889900',
        displayName: 'this device',
        isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000,
        fingerprint: 'aa11·bb22',
      },
      {
        deviceId: 'bb22cc33dd44ee55ff667788990011aa',
        displayName: 'other device',
        isThisDevice: false,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_600_000_000,
        fingerprint: 'bb22·cc33',
      },
    ],
    canBackUp: true,
  };
  const backedUpKey = `harmony.onboarding.recoveryArtifactBackedUp:owner-${metaView.ownerId}`;
  const backedUpAtKey = `harmony.onboarding.recoveryBackedUpAt:owner-${metaView.ownerId}`;

  beforeEach(() => {
    localStorage.clear();
    sessionStorage.clear();
  });

  it('renders keytype, earliest enrollment date, and communities count', async () => {
    mockedInvoke.mockResolvedValueOnce(metaView); // get_owner_state
    mockedCommunitiesCount.mockResolvedValue(4);
    render(DevicesPanel);
    const keytype = await screen.findByTestId('devices-meta-keytype');
    expect(keytype.textContent).toBe('ed25519');
    // MIN of the two enrolledAt values (Unix seconds → ms).
    expect(screen.getByTestId('devices-meta-enrolled').textContent).toContain(
      new Date(1_600_000_000 * 1000).toLocaleDateString(),
    );
    const communities = await screen.findByTestId('devices-meta-communities');
    expect(communities.textContent).toContain('4');
    expect(communities.textContent).toContain('communities');
  });

  it('uses singular copy for exactly one community', async () => {
    mockedInvoke.mockResolvedValueOnce(metaView);
    mockedCommunitiesCount.mockResolvedValue(1);
    render(DevicesPanel);
    const communities = await screen.findByTestId('devices-meta-communities');
    expect(communities.textContent).toContain('1 community');
    expect(communities.textContent).not.toContain('communities');
  });

  it('omits the communities fact when the count is unavailable', async () => {
    mockedInvoke.mockResolvedValueOnce(metaView);
    mockedCommunitiesCount.mockResolvedValue(null);
    render(DevicesPanel);
    await screen.findByTestId('devices-meta-keytype');
    expect(screen.queryByTestId('devices-meta-communities')).toBeNull();
  });

  it('shows last-backed-up only when a stamp exists', async () => {
    const stamp = Date.UTC(2026, 0, 15);
    localStorage.setItem(backedUpAtKey, String(stamp));
    mockedInvoke.mockResolvedValueOnce(metaView);
    render(DevicesPanel);
    const line = await screen.findByTestId('devices-last-backed-up');
    expect(line.textContent).toContain(new Date(stamp).toLocaleDateString());
  });

  it('omits last-backed-up for a legacy owner without a stamp', async () => {
    mockedInvoke.mockResolvedValueOnce(metaView);
    render(DevicesPanel);
    await screen.findByTestId('devices-meta-keytype');
    expect(screen.queryByTestId('devices-last-backed-up')).toBeNull();
  });

  it('commitBackup marks the owner backed up (gap fix)', async () => {
    mockedInvoke.mockResolvedValueOnce(metaView); // get_owner_state (mount)
    render(DevicesPanel);
    const backupBtn = await screen.findByRole('button', { name: /back up owner identity/i });
    mockedInvoke.mockResolvedValueOnce('token-1'); // issue_owner_recovery_token
    await fireEvent.click(backupBtn);
    const pass = await screen.findByLabelText('Passphrase');
    await fireEvent.input(pass, { target: { value: 'a'.repeat(12) } });
    await fireEvent.input(screen.getByLabelText('Confirm passphrase'), {
      target: { value: 'a'.repeat(12) },
    });
    mockedInvoke.mockResolvedValueOnce('path-token'); // request_export_save_path
    mockedInvoke.mockResolvedValueOnce({ identityHash: 'h', byteLen: 1, path: '/tmp/x.bin' }); // export
    await fireEvent.click(screen.getByRole('button', { name: /save backup/i }));
    await screen.findByText(/recovery file written to/i);
    expect(localStorage.getItem(backedUpKey)).toBe('true');
    expect(localStorage.getItem(backedUpAtKey)).not.toBeNull();
    // The last-backed-up line appears immediately (state updated at the
    // mutation point, not just on next mount).
    expect(screen.getByTestId('devices-last-backed-up')).toBeTruthy();
  });
});

describe('DevicesPanel meta facts are keyed to the owner (Qodo PR #436)', () => {
  beforeEach(() => {
    localStorage.clear();
    sessionStorage.clear();
  });

  it('populates the meta row after an in-panel mint (owner appears post-mount)', async () => {
    mockedInvoke.mockResolvedValueOnce(null); // get_owner_state → empty state
    mockedCommunitiesCount.mockResolvedValue(2);
    render(DevicesPanel);
    const bindBtn = await screen.findByRole('button', { name: /bind this device/i });
    await fireEvent.click(bindBtn);
    mockedInvoke.mockResolvedValueOnce({
      state: {
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
      },
      recoveryToken: 'tok-1',
    }); // mint_owner_identity
    const confirmBtn = await screen.findByRole('button', { name: /^create owner identity/i });
    await fireEvent.click(confirmBtn);
    await screen.findByText(/my devices/i);
    // The $effect keys the meta facts to the NEW ownerId — a run-once onMount
    // would leave this row unpopulated (or, worse, stale on an owner swap).
    const keytype = await screen.findByTestId('devices-meta-keytype');
    expect(keytype.textContent).toBe('ed25519');
    const communities = await screen.findByTestId('devices-meta-communities');
    expect(communities.textContent).toContain('2');
  });
});

describe('DevicesPanel owner phrase reveal (ZEB-650 slice 2)', () => {
  const phraseView = {
    ownerId: 'a4f1c8239b7dd809abcdef0123456789',
    ownerDisplayName: 'zeblith',
    devices: [
      {
        deviceId: 'aa11bb22cc33dd44ee55ff6677889900',
        displayName: 'this device',
        isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000,
        fingerprint: 'aa11·bb22',
      },
    ],
    canBackUp: true,
  };
  const WORDS = [
    'abandon', 'ability', 'able', 'about', 'above', 'absent',
    'absorb', 'abstract', 'absurd', 'abuse', 'access', 'accident',
    'account', 'accuse', 'achieve', 'acid', 'acoustic', 'acquire',
    'across', 'act', 'action', 'actor', 'actress', 'actual',
  ];

  beforeEach(() => {
    localStorage.clear();
    sessionStorage.clear();
  });

  it('renders the view-phrase button beside back-up, enabled when canBackUp', async () => {
    mockedInvoke.mockResolvedValueOnce(phraseView); // get_owner_state
    const { findByTestId } = render(DevicesPanel);
    const btn = await findByTestId('devices-view-phrase');
    expect((btn as HTMLButtonElement).disabled).toBe(false);
  });

  it('disables the view-phrase button when the seed is wiped (canBackUp false)', async () => {
    mockedInvoke.mockResolvedValueOnce({ ...phraseView, canBackUp: false });
    const { findByTestId } = render(DevicesPanel);
    const btn = await findByTestId('devices-view-phrase');
    expect((btn as HTMLButtonElement).disabled).toBe(true);
  });

  it('opens the phrase modal, reveals via ordered stub, and checkbox updates last-backed-up', async () => {
    mockedInvoke
      .mockResolvedValueOnce(phraseView) // get_owner_state on mount
      .mockResolvedValueOnce({ words: WORDS, ownerId: phraseView.ownerId }); // export_owner_mnemonic_words
    const { findByTestId, getByTestId, queryByTestId } = render(DevicesPanel);
    await fireEvent.click(await findByTestId('devices-view-phrase'));
    // Modal open, still collapsed — the export stub is not yet consumed.
    expect(getByTestId('phrase-reveal-open')).toBeTruthy();
    await fireEvent.click(getByTestId('phrase-reveal-open'));
    await fireEvent.click(getByTestId('phrase-reveal-confirm'));
    await Promise.resolve();
    await Promise.resolve();
    expect(getByTestId('phrase-grid').querySelectorAll('li').length).toBe(24);
    await fireEvent.click(getByTestId('phrase-reveal-unblur'));
    // Before the checkbox: no last-backed-up line for this owner.
    expect(queryByTestId('devices-last-backed-up')).toBeNull();
    await fireEvent.click(getByTestId('phrase-written-down'));
    await tick();
    // markRecoveryBackedUp dispatched the flags-changed event → the panel's
    // listener refreshed lastBackedUpMs without a remount.
    expect(await findByTestId('devices-last-backed-up')).toBeTruthy();
  });
});

// ── ZEB-668 S2: device revoke UI ─────────────────────────────────────────────

describe('DevicesPanel — device revoke (ZEB-668 S2)', () => {
  const seedHolderView = (overrides: Record<string, unknown> = {}) => ({
    ownerId: 'a4f1c8239b7dd809abcdef0123456789',
    ownerDisplayName: 'zeblith',
    devices: [
      {
        deviceId: 'aa11bb22cc33dd44ee55ff6677889900',
        displayName: 'KRILE',
        isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000,
        fingerprint: 'aa11·bb22',
        butlerPinned: false,
        deviceVkHex: 'aa'.repeat(32),
        revoked: false,
        revokedAt: null,
        revokedReason: null,
      },
      {
        deviceId: 'bb22cc33dd44ee55ff66778899001122',
        displayName: 'Device bb22cc33',
        isThisDevice: false,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_100_000,
        fingerprint: 'bb22·cc33',
        butlerPinned: false,
        deviceVkHex: 'bb'.repeat(32),
        revoked: false,
        revokedAt: null,
        revokedReason: null,
      },
    ],
    canBackUp: true,
    ...overrides,
  });

  it('seed-holder sees Remove… on sibling rows and Remove this device on self', async () => {
    mockedInvoke.mockResolvedValueOnce(seedHolderView());
    render(DevicesPanel);
    await screen.findByText('KRILE');
    expect(screen.getByRole('button', { name: /remove this device/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^remove…$/i })).toBeInTheDocument();
  });

  it('non-seed device hides sibling Remove… but keeps self-remove (honesty rule)', async () => {
    mockedInvoke.mockResolvedValueOnce(seedHolderView({ canBackUp: false }));
    render(DevicesPanel);
    await screen.findByText('KRILE');
    expect(screen.getByRole('button', { name: /remove this device/i })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /^remove…$/i })).toBeNull();
  });

  it('sibling remove flow: dialog → type name → confirm → revoke_device + refresh', async () => {
    mockedInvoke.mockResolvedValueOnce(seedHolderView()); // mount refresh
    mockedInvoke.mockResolvedValueOnce(undefined); // revoke_device
    mockedInvoke.mockResolvedValueOnce(seedHolderView()); // post-revoke refresh
    render(DevicesPanel);
    await screen.findByText('KRILE');
    await fireEvent.click(screen.getByRole('button', { name: /^remove…$/i }));
    // Dialog opens with the typed-confirm input.
    const input = await screen.findByRole('textbox', { name: /type to confirm/i });
    await fireEvent.input(input, { target: { value: 'Device bb22cc33' } });
    await fireEvent.click(screen.getByRole('button', { name: /^remove device$/i }));
    await tick();
    expect(mockedInvoke).toHaveBeenCalledWith('revoke_device', {
      deviceVkHex: 'bb'.repeat(32),
      reason: 'decommissioned',
    });
    // Post-success refresh re-fetched the roster.
    const calls = mockedInvoke.mock.calls.map((c: unknown[]) => c[0]);
    expect(calls.filter((n: unknown) => n === 'get_owner_state').length).toBe(2);
  });

  it('backend prefix errors surface as friendly copy in the dialog', async () => {
    mockedInvoke.mockResolvedValueOnce(seedHolderView());
    mockedInvoke.mockRejectedValueOnce(new Error('lastDevice: refusing'));
    render(DevicesPanel);
    await screen.findByText('KRILE');
    await fireEvent.click(screen.getByRole('button', { name: /remove this device/i }));
    const input = await screen.findByRole('textbox', { name: /type to confirm/i });
    await fireEvent.input(input, { target: { value: 'KRILE' } });
    await fireEvent.click(screen.getByRole('button', { name: /^remove device$/i }));
    expect(await screen.findByText(/only active device/i)).toBeInTheDocument();
  });

  it('revoked devices leave the active list and render in the collapsed Removed section', async () => {
    const view = seedHolderView();
    (view.devices as Record<string, unknown>[])[1] = {
      ...(view.devices as Record<string, unknown>[])[1],
      revoked: true,
      revokedAt: 1_700_200_000,
      revokedReason: 'lost',
    };
    mockedInvoke.mockResolvedValueOnce(view);
    render(DevicesPanel);
    await screen.findByText('KRILE');
    // Active count excludes the revoked device.
    expect(screen.getByText(/my devices \(1\)/i)).toBeInTheDocument();
    // Collapsed by default: the row is not visible until expanded.
    expect(screen.queryByText(/^lost$/i)).toBeNull();
    const toggle = screen.getByRole('button', { name: /removed devices \(1\)/i });
    expect(toggle).toHaveAttribute('aria-expanded', 'false');
    await fireEvent.click(toggle);
    expect(screen.getByText('Device bb22cc33')).toBeInTheDocument();
    expect(screen.getByText(/^lost$/i)).toBeInTheDocument();
    // No butler checkbox on removed rows (only the active row's remains).
    expect(screen.getAllByRole('checkbox').length).toBe(1);
  });
});

// ── ZEB-668 S4: petnames + last-seen presence line ────────────────────────────

describe('DevicesPanel — petnames + last-seen (ZEB-668 S4)', () => {
  const s4View = (overrides: Record<string, unknown> = {}) => ({
    ownerId: 'a4f1c8239b7dd809abcdef0123456789',
    ownerDisplayName: 'zeblith',
    devices: [
      {
        deviceId: 'aa11bb22cc33dd44ee55ff6677889900',
        displayName: 'this device',
        isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000,
        fingerprint: 'aa11·bb22',
        butlerPinned: false,
        deviceVkHex: 'aa'.repeat(32),
        revoked: false,
        revokedAt: null,
        revokedReason: null,
        petName: null as string | null,
        lastSeenMs: null as number | null,
        connectedNow: false,
      },
      {
        deviceId: 'bb22cc33dd44ee55ff66778899001122',
        displayName: 'Device bb22cc33',
        isThisDevice: false,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_100_000,
        fingerprint: 'bb22·cc33',
        butlerPinned: false,
        deviceVkHex: 'bb'.repeat(32),
        revoked: false,
        revokedAt: null,
        revokedReason: null,
        petName: null as string | null,
        lastSeenMs: null as number | null,
        connectedNow: false,
      },
    ],
    canBackUp: true,
    ...overrides,
  });

  const withSibling = (sibling: Record<string, unknown>) => {
    const view = s4View();
    view.devices[1] = { ...view.devices[1], ...sibling };
    return view;
  };

  it('petName wins the label ladder on a sibling row', async () => {
    mockedInvoke.mockResolvedValueOnce(withSibling({ petName: 'Ildwyn' }));
    render(DevicesPanel);
    await screen.findByText('Ildwyn');
    expect(screen.queryByText('Device bb22cc33')).toBeNull();
  });

  it('connectedNow renders the online badge on a sibling row', async () => {
    mockedInvoke.mockResolvedValueOnce(withSibling({ connectedNow: true }));
    render(DevicesPanel);
    await screen.findByText('Device bb22cc33');
    expect(screen.getByText(/online/i)).toBeInTheDocument();
  });

  it('lastSeenMs renders heartbeat-tolerant relative time when not connected', async () => {
    mockedInvoke.mockResolvedValueOnce(
      withSibling({ lastSeenMs: Date.now() - 2 * 3_600_000 }),
    );
    render(DevicesPanel);
    await screen.findByText('Device bb22cc33');
    expect(screen.getByText(/last seen ~2h ago/i)).toBeInTheDocument();
  });

  it('null lastSeenMs renders neither presence line (honest absence)', async () => {
    mockedInvoke.mockResolvedValueOnce(s4View());
    render(DevicesPanel);
    await screen.findByText('Device bb22cc33');
    expect(screen.queryByText(/online/i)).toBeNull();
    expect(screen.queryByText(/last seen/i)).toBeNull();
  });

  it('sibling rename saves through set_device_petname with the vk-hex id', async () => {
    mockedInvoke.mockResolvedValueOnce(s4View()); // mount refresh
    mockedInvoke.mockResolvedValueOnce(undefined); // set_device_petname
    mockedInvoke.mockResolvedValueOnce(s4View()); // post-save refresh
    render(DevicesPanel);
    await screen.findByText('Device bb22cc33');
    // Two rename buttons now (self + sibling); take the sibling's (last).
    const renameBtns = screen.getAllByRole('button', { name: /rename/i });
    await fireEvent.click(renameBtns[renameBtns.length - 1]);
    const input = screen.getByRole('textbox', { name: /device name/i });
    await fireEvent.input(input, { target: { value: 'Ildwyn' } });
    await fireEvent.click(screen.getByRole('button', { name: /save/i }));
    await tick();
    expect(mockedInvoke).toHaveBeenCalledWith('set_device_petname', {
      deviceVkHex: 'bb'.repeat(32),
      petname: 'Ildwyn',
    });
    // Sibling rename must NOT touch the local this-device label.
    expect(saveDeviceLabel).not.toHaveBeenCalled();
  });

  it('self rename keeps localStorage label in step as the offline fallback', async () => {
    mockedInvoke.mockResolvedValueOnce(s4View()); // mount refresh
    mockedInvoke.mockResolvedValueOnce(undefined); // set_device_petname
    mockedInvoke.mockResolvedValueOnce(s4View()); // post-save refresh
    render(DevicesPanel);
    await screen.findByText('Device bb22cc33');
    const renameBtns = screen.getAllByRole('button', { name: /rename/i });
    await fireEvent.click(renameBtns[0]);
    const input = screen.getByRole('textbox', { name: /device name/i });
    await fireEvent.input(input, { target: { value: 'KRILE-prime' } });
    await fireEvent.click(screen.getByRole('button', { name: /save/i }));
    await tick();
    expect(mockedInvoke).toHaveBeenCalledWith('set_device_petname', {
      deviceVkHex: 'aa'.repeat(32),
      petname: 'KRILE-prime',
    });
    expect(saveDeviceLabel).toHaveBeenCalledWith('KRILE-prime');
  });

  it('one-shot migration seeds the fleet petname from a user-set local label', async () => {
    (loadDeviceLabel as ReturnType<typeof vi.fn>).mockReturnValue('KRILE');
    mockedInvoke.mockResolvedValueOnce(s4View()); // mount refresh (self has no petName)
    mockedInvoke.mockResolvedValueOnce(undefined); // migration set_device_petname
    mockedInvoke.mockResolvedValueOnce(withSibling({})); // post-migration refresh
    render(DevicesPanel);
    await screen.findByText('Device bb22cc33');
    await tick();
    expect(mockedInvoke).toHaveBeenCalledWith('set_device_petname', {
      deviceVkHex: 'aa'.repeat(32),
      petname: 'KRILE',
    });
  });

  it('no migration when the self row already has a petname', async () => {
    (loadDeviceLabel as ReturnType<typeof vi.fn>).mockReturnValue('KRILE');
    const view = s4View();
    view.devices[0] = { ...view.devices[0], petName: 'Koya' };
    mockedInvoke.mockResolvedValueOnce(view);
    render(DevicesPanel);
    await screen.findByText('Koya');
    await tick();
    const petnameCalls = mockedInvoke.mock.calls.filter(
      (c: unknown[]) => c[0] === 'set_device_petname',
    );
    expect(petnameCalls.length).toBe(0);
  });

  // ── PR #454 round 1 ──────────────────────────────────────────────────────

  it('a fleet-cleared petname suppresses the private local label (no stale resurface)', async () => {
    // Greptile P1 (round 2): sibling clears THIS device's petname → backend
    // sends petName: "". The ladder must show the backend placeholder, not
    // keep rendering the stale localStorage label.
    (loadDeviceLabel as ReturnType<typeof vi.fn>).mockReturnValue('KRILE');
    const view = s4View();
    view.devices[0] = { ...view.devices[0], petName: '' };
    mockedInvoke.mockResolvedValueOnce(view);
    render(DevicesPanel);
    await screen.findByText('Device bb22cc33');
    expect(screen.queryByText('KRILE')).toBeNull();
    expect(screen.getByText('this device', { selector: '.device-name' })).toBeInTheDocument();
  });

  it('no migration for an explicitly CLEARED petname (Some("") ≠ never named)', async () => {
    (loadDeviceLabel as ReturnType<typeof vi.fn>).mockReturnValue('KRILE');
    const view = s4View();
    view.devices[0] = { ...view.devices[0], petName: '' };
    mockedInvoke.mockResolvedValueOnce(view);
    render(DevicesPanel);
    await screen.findByText('Device bb22cc33');
    await tick();
    const petnameCalls = mockedInvoke.mock.calls.filter(
      (c: unknown[]) => c[0] === 'set_device_petname',
    );
    expect(petnameCalls.length).toBe(0);
  });

  it('empty save clears a sibling petname through the IPC', async () => {
    mockedInvoke.mockResolvedValueOnce(withSibling({ petName: 'Ildwyn' })); // mount
    mockedInvoke.mockResolvedValueOnce(undefined); // set_device_petname (clear)
    mockedInvoke.mockResolvedValueOnce(withSibling({ petName: '' })); // refresh
    render(DevicesPanel);
    await screen.findByText('Ildwyn');
    const renameBtns = screen.getAllByRole('button', { name: /rename/i });
    await fireEvent.click(renameBtns[renameBtns.length - 1]);
    const input = screen.getByRole('textbox', { name: /device name/i });
    await fireEvent.input(input, { target: { value: '' } });
    await fireEvent.click(screen.getByRole('button', { name: /save/i }));
    await tick();
    expect(mockedInvoke).toHaveBeenCalledWith('set_device_petname', {
      deviceVkHex: 'bb'.repeat(32),
      petname: '',
    });
    // Sibling clear never touches this device's local label store.
    expect(clearDeviceLabel).not.toHaveBeenCalled();
    // Cleared name falls back to the backend display name.
    await screen.findByText('Device bb22cc33');
  });

  it('clearing the self petname also removes the local fallback label', async () => {
    mockedInvoke.mockResolvedValueOnce(s4View()); // mount (self petName null, no label → no migration)
    mockedInvoke.mockResolvedValueOnce(undefined); // set_device_petname (clear)
    mockedInvoke.mockResolvedValueOnce(s4View()); // refresh
    render(DevicesPanel);
    await screen.findByText('Device bb22cc33');
    const renameBtns = screen.getAllByRole('button', { name: /rename/i });
    await fireEvent.click(renameBtns[0]); // self row
    const input = screen.getByRole('textbox', { name: /device name/i });
    await fireEvent.input(input, { target: { value: '   ' } });
    await fireEvent.click(screen.getByRole('button', { name: /save/i }));
    await tick();
    expect(mockedInvoke).toHaveBeenCalledWith('set_device_petname', {
      deviceVkHex: 'aa'.repeat(32),
      petname: '',
    });
    expect(clearDeviceLabel).toHaveBeenCalled();
    expect(saveDeviceLabel).not.toHaveBeenCalled();
  });

  it('over-length rename shows an inline error and never invokes the IPC', async () => {
    mockedInvoke.mockResolvedValueOnce(s4View());
    render(DevicesPanel);
    await screen.findByText('Device bb22cc33');
    const renameBtns = screen.getAllByRole('button', { name: /rename/i });
    await fireEvent.click(renameBtns[renameBtns.length - 1]);
    const input = screen.getByRole('textbox', { name: /device name/i });
    await fireEvent.input(input, { target: { value: 'x'.repeat(65) } });
    await fireEvent.click(screen.getByRole('button', { name: /save/i }));
    await tick();
    expect(screen.getByRole('alert')).toHaveTextContent(/too long/i);
    const petnameCalls = mockedInvoke.mock.calls.filter(
      (c: unknown[]) => c[0] === 'set_device_petname',
    );
    expect(petnameCalls.length).toBe(0);
    // Cancel clears the error.
    await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('counts code points, not UTF-16 units (64 emoji are accepted)', async () => {
    mockedInvoke.mockResolvedValueOnce(s4View()); // mount
    mockedInvoke.mockResolvedValueOnce(undefined); // set_device_petname
    mockedInvoke.mockResolvedValueOnce(s4View()); // refresh
    render(DevicesPanel);
    await screen.findByText('Device bb22cc33');
    const renameBtns = screen.getAllByRole('button', { name: /rename/i });
    await fireEvent.click(renameBtns[renameBtns.length - 1]);
    const input = screen.getByRole('textbox', { name: /device name/i });
    // 64 surrogate-pair emoji = 128 UTF-16 units but exactly the 64-char cap.
    await fireEvent.input(input, { target: { value: '😀'.repeat(64) } });
    await fireEvent.click(screen.getByRole('button', { name: /save/i }));
    await tick();
    expect(mockedInvoke).toHaveBeenCalledWith('set_device_petname', {
      deviceVkHex: 'bb'.repeat(32),
      petname: '😀'.repeat(64),
    });
  });
});

// ── ZEB-668 S5: fleet-key staleness banner + rotate action ────────────────────

describe('DevicesPanel — fleet epoch staleness (ZEB-668 S5)', () => {
  const s5View = (overrides: Record<string, unknown> = {}) => ({
    ownerId: 'a4f1c8239b7dd809abcdef0123456789',
    ownerDisplayName: 'zeblith',
    devices: [
      {
        deviceId: 'aa11bb22cc33dd44ee55ff6677889900',
        displayName: 'this device',
        isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000,
        fingerprint: 'aa11·bb22',
        butlerPinned: false,
        deviceVkHex: 'aa'.repeat(32),
        revoked: false,
        revokedAt: null,
        revokedReason: null,
        petName: null as string | null,
        lastSeenMs: null as number | null,
        connectedNow: false,
      },
    ],
    canBackUp: true,
    fleetEpoch: 0,
    fleetEpochStale: false,
    ...overrides,
  });

  it('renders nothing when the fleet keys are fresh', async () => {
    mockedInvoke.mockResolvedValueOnce(s5View());
    render(DevicesPanel);
    await screen.findByText(/my devices/i);
    expect(screen.queryByTestId('fleet-epoch-banner')).not.toBeInTheDocument();
  });

  it('stale + seed-holder: banner with a Rotate button that bumps and refreshes', async () => {
    mockedInvoke.mockResolvedValueOnce(s5View({ fleetEpochStale: true }));
    render(DevicesPanel);
    await screen.findByTestId('fleet-epoch-banner');
    const btn = screen.getByTestId('rotate-fleet-keys');
    mockedInvoke.mockResolvedValueOnce(1); // bump_fleet_epoch → new epoch
    mockedInvoke.mockResolvedValueOnce(s5View({ fleetEpoch: 1, fleetEpochStale: false }));
    await fireEvent.click(btn);
    await waitFor(() =>
      expect(screen.queryByTestId('fleet-epoch-banner')).not.toBeInTheDocument()
    );
    expect(mockedInvoke).toHaveBeenCalledWith('bump_fleet_epoch');
  });

  it('stale + cert-only device: passive note, no button', async () => {
    mockedInvoke.mockResolvedValueOnce(
      s5View({ fleetEpochStale: true, canBackUp: false })
    );
    render(DevicesPanel);
    await screen.findByTestId('fleet-epoch-banner');
    expect(
      screen.getByText(/rotate them from the device that holds your master key/i)
    ).toBeInTheDocument();
    expect(screen.queryByTestId('rotate-fleet-keys')).not.toBeInTheDocument();
  });

  it('bump rejection surfaces as an alert and re-click is possible', async () => {
    mockedInvoke.mockResolvedValueOnce(s5View({ fleetEpochStale: true }));
    render(DevicesPanel);
    await screen.findByTestId('fleet-epoch-banner');
    mockedInvoke.mockRejectedValueOnce('notMaster: nope');
    await fireEvent.click(screen.getByTestId('rotate-fleet-keys'));
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toMatch(/notMaster/);
    // Banner still present; button re-enabled for retry.
    expect(screen.getByTestId('rotate-fleet-keys')).not.toBeDisabled();
  });
});


describe('DevicesPanel — replace device (ZEB-668 S6)', () => {
  const SUCCESSOR_ID = 'cc'.repeat(16);
  const SUCCESSOR_VK = 'dd'.repeat(32);

  const baseDevice = {
    trustDecision: { kind: 'full', reason: null },
    enrolledAt: 1_700_000_000,
    fingerprint: 'xx11·yy22',
    butlerPinned: false,
    revoked: false,
    revokedAt: null,
    revokedReason: null,
  };

  const s6View = (siblingPetName: string | null = 'Living room') => ({
    ownerId: 'a4f1c8239b7dd809abcdef0123456789',
    ownerDisplayName: 'zeblith',
    devices: [
      {
        ...baseDevice,
        deviceId: 'aa11bb22cc33dd44ee55ff6677889900',
        displayName: 'KRILE',
        isThisDevice: true,
        deviceVkHex: 'aa'.repeat(32),
        petName: null,
      },
      {
        ...baseDevice,
        deviceId: 'bb22cc33dd44ee55ff66778899001122',
        displayName: 'Device bb22cc33',
        isThisDevice: false,
        deviceVkHex: 'bb'.repeat(32),
        petName: siblingPetName,
      },
    ],
    canBackUp: true,
  });

  const successorRow = {
    ...baseDevice,
    deviceId: SUCCESSOR_ID,
    displayName: 'Device cccccccc',
    isThisDevice: false,
    deviceVkHex: SUCCESSOR_VK,
    petName: null,
  };

  it('seed-holder sees Replace… on the sibling row only', async () => {
    mockedInvoke.mockResolvedValueOnce(s6View());
    render(DevicesPanel);
    await screen.findByText('KRILE');
    // Exactly one — the sibling row; the self row never offers Replace.
    expect(screen.getAllByRole('button', { name: /^replace…$/i })).toHaveLength(1);
  });

  it('non-seed device hides Replace… (honesty rule)', async () => {
    mockedInvoke.mockResolvedValueOnce({ ...s6View(), canBackUp: false });
    render(DevicesPanel);
    await screen.findByText('KRILE');
    expect(screen.queryByRole('button', { name: /^replace…$/i })).toBeNull();
  });

  it('replace flow: decommissioned revoke → inviter pairing → petname carried to successor', async () => {
    const before = s6View();
    const afterRevoke = s6View();
    afterRevoke.devices[1].revoked = true;
    const withSuccessor = s6View();
    withSuccessor.devices[1].revoked = true;
    withSuccessor.devices.push({ ...successorRow });

    let ownerView: unknown = before;
    let pairingFetches = 0;
    mockedInvoke.mockImplementation(async (cmd: unknown) => {
      if (cmd === 'get_owner_state') return ownerView;
      if (cmd === 'revoke_device') {
        ownerView = afterRevoke;
        return undefined;
      }
      if (cmd === 'get_pairing_state') {
        pairingFetches += 1;
        // The SECOND fetch is PairingInviter's fold-on-complete — the
        // backend folds the persisted enrollment into the resident doc
        // there, which is exactly when the successor becomes visible.
        if (pairingFetches >= 2) ownerView = withSuccessor;
        return { kind: 'complete', deviceIdHex: SUCCESSOR_ID };
      }
      return undefined;
    });

    render(DevicesPanel);
    await screen.findByText('KRILE');
    // Sibling displays its petname (S4 overlay ladder).
    await fireEvent.click(screen.getByRole('button', { name: /^replace…$/i }));
    const input = await screen.findByRole('textbox', { name: /type to confirm/i });
    await fireEvent.input(input, { target: { value: 'Living room' } });
    await fireEvent.click(screen.getByRole('button', { name: /remove & continue/i }));

    await waitFor(() =>
      expect(mockedInvoke).toHaveBeenCalledWith('revoke_device', {
        deviceVkHex: 'bb'.repeat(32),
        reason: 'decommissioned',
      }),
    );
    await waitFor(() =>
      expect(mockedInvoke).toHaveBeenCalledWith('start_inviter_pairing', {
        displayName: 'zeblith',
      }),
    );
    // Completion: old petname lands on the SUCCESSOR's vk (never deviceId).
    await waitFor(() =>
      expect(mockedInvoke).toHaveBeenCalledWith('set_device_petname', {
        deviceVkHex: SUCCESSOR_VK,
        petname: 'Living room',
      }),
    );
  });

  it('refresh failure after a successful revoke still opens pairing (round 1)', async () => {
    // The revoke is irreversible — gating pairing on the refresh would
    // strand the user and invite a duplicate revoke on retry.
    let ownerView: unknown = s6View();
    let revoked = false;
    mockedInvoke.mockImplementation(async (cmd: unknown) => {
      if (cmd === 'get_owner_state') {
        if (revoked) throw new Error('transient: node busy');
        return ownerView;
      }
      if (cmd === 'revoke_device') {
        revoked = true;
        return undefined;
      }
      if (cmd === 'get_pairing_state') {
        return {
          kind: 'discovering',
          role: 'inviter',
          ephemeralPubkeyHex: '',
          sessionId: '00000000-0000-0000-0000-000000000010',
        };
      }
      return undefined;
    });

    render(DevicesPanel);
    await screen.findByText('KRILE');
    await fireEvent.click(screen.getByRole('button', { name: /^replace…$/i }));
    const input = await screen.findByRole('textbox', { name: /type to confirm/i });
    await fireEvent.input(input, { target: { value: 'Living room' } });
    await fireEvent.click(screen.getByRole('button', { name: /remove & continue/i }));

    // Pairing launches despite the failing refresh.
    await screen.findByText(/looking for nearby devices/i);
    await waitFor(() =>
      expect(mockedInvoke).toHaveBeenCalledWith('start_inviter_pairing', {
        displayName: 'zeblith',
      }),
    );
  });

  it('successor invisible on the first refresh → carried on the bounded second attempt (round 1)', async () => {
    const before = s6View();
    const afterRevoke = s6View();
    afterRevoke.devices[1].revoked = true;
    const withSuccessor = s6View();
    withSuccessor.devices[1].revoked = true;
    withSuccessor.devices.push({ ...successorRow });

    let ownerView: unknown = before;
    let completed = false;
    let postCompleteFetches = 0;
    mockedInvoke.mockImplementation(async (cmd: unknown) => {
      if (cmd === 'get_owner_state') {
        if (completed) {
          postCompleteFetches += 1;
          // First post-complete refresh is stale; the second sees the row.
          if (postCompleteFetches >= 2) ownerView = withSuccessor;
        }
        return ownerView;
      }
      if (cmd === 'revoke_device') {
        ownerView = afterRevoke;
        return undefined;
      }
      if (cmd === 'get_pairing_state') {
        completed = true;
        return { kind: 'complete', deviceIdHex: SUCCESSOR_ID };
      }
      return undefined;
    });

    render(DevicesPanel);
    await screen.findByText('KRILE');
    await fireEvent.click(screen.getByRole('button', { name: /^replace…$/i }));
    const input = await screen.findByRole('textbox', { name: /type to confirm/i });
    await fireEvent.input(input, { target: { value: 'Living room' } });
    await fireEvent.click(screen.getByRole('button', { name: /remove & continue/i }));

    await waitFor(() =>
      expect(mockedInvoke).toHaveBeenCalledWith('set_device_petname', {
        deviceVkHex: SUCCESSOR_VK,
        petname: 'Living room',
      }),
    );
    // And no stale carry alert.
    expect(screen.queryByText(/couldn't carry the name/i)).toBeNull();
  });

  it('never-named old device → no petname write for the successor', async () => {
    const before = s6View(null);
    const withSuccessor = s6View(null);
    withSuccessor.devices[1].revoked = true;
    withSuccessor.devices.push({ ...successorRow });

    let ownerView: unknown = before;
    mockedInvoke.mockImplementation(async (cmd: unknown) => {
      if (cmd === 'get_owner_state') return ownerView;
      if (cmd === 'revoke_device') {
        ownerView = withSuccessor;
        return undefined;
      }
      if (cmd === 'get_pairing_state') {
        return { kind: 'complete', deviceIdHex: SUCCESSOR_ID };
      }
      return undefined;
    });

    render(DevicesPanel);
    await screen.findByText('KRILE');
    await fireEvent.click(screen.getByRole('button', { name: /^replace…$/i }));
    const input = await screen.findByRole('textbox', { name: /type to confirm/i });
    await fireEvent.input(input, { target: { value: 'Device bb22cc33' } });
    await fireEvent.click(screen.getByRole('button', { name: /remove & continue/i }));

    // Pairing completes (successor visible in the refreshed view)…
    await waitFor(() =>
      expect(mockedInvoke).toHaveBeenCalledWith('get_pairing_state'),
    );
    await waitFor(() => expect(screen.getByText('Device cccccccc')).toBeInTheDocument());
    // …but there is no name to carry.
    expect(mockedInvoke).not.toHaveBeenCalledWith(
      'set_device_petname',
      expect.anything(),
    );
  });

  it('abandoned pairing clears the pending carry — a later add-device cannot inherit it', async () => {
    let ownerView: unknown = s6View();
    let pairingSnapshot: unknown = {
      kind: 'discovering',
      role: 'inviter',
      ephemeralPubkeyHex: '',
      sessionId: '00000000-0000-0000-0000-000000000009',
    };
    mockedInvoke.mockImplementation(async (cmd: unknown) => {
      if (cmd === 'get_owner_state') return ownerView;
      if (cmd === 'get_pairing_state') return pairingSnapshot;
      if (cmd === 'revoke_device') {
        const v = s6View();
        v.devices[1].revoked = true;
        ownerView = v;
        return undefined;
      }
      return undefined;
    });

    render(DevicesPanel);
    await screen.findByText('KRILE');
    await fireEvent.click(screen.getByRole('button', { name: /^replace…$/i }));
    const input = await screen.findByRole('textbox', { name: /type to confirm/i });
    await fireEvent.input(input, { target: { value: 'Living room' } });
    await fireEvent.click(screen.getByRole('button', { name: /remove & continue/i }));

    // The inviter opens in discovering state; abandon it. Wait for the
    // pairing modal's copy first — finding Cancel any earlier can grab the
    // replace dialog's (about-to-unmount) Cancel instead.
    await screen.findByText(/looking for nearby devices/i);
    const cancelBtn = await screen.findByRole('button', { name: /^cancel$/i });
    await fireEvent.click(cancelBtn);
    await waitFor(() => expect(mockedInvoke).toHaveBeenCalledWith('cancel_pairing'));

    // A LATER unrelated pairing that completes must not inherit the carry.
    pairingSnapshot = { kind: 'complete', deviceIdHex: SUCCESSOR_ID };
    await fireEvent.click(screen.getByRole('button', { name: /add another device/i }));
    await waitFor(() =>
      expect(
        mockedInvoke.mock.calls.filter((c) => c[0] === 'get_pairing_state').length,
      ).toBeGreaterThanOrEqual(2),
    );
    expect(mockedInvoke).not.toHaveBeenCalledWith(
      'set_device_petname',
      expect.anything(),
    );
  });
});

// ── ZEB-677 S3: quorum co-sign ceremony surfaces ──────────────────────────────

describe('DevicesPanel — quorum co-sign ceremony (ZEB-677 S3)', () => {
  const device = (idByte: string, name: string, self = false) => ({
    deviceId: idByte.repeat(16),
    displayName: name,
    isThisDevice: self,
    trustDecision: { kind: 'full', reason: null },
    enrolledAt: 1_700_000_000,
    fingerprint: `${idByte}${idByte}·xx`,
    butlerPinned: false,
    deviceVkHex: idByte.repeat(32),
    revoked: false,
    revokedAt: null,
    revokedReason: null,
    petName: null,
    lastSeenMs: null,
    connectedNow: false,
    quorumRemovable: !self,
  });

  /** Master-less 3-device fleet: self KRILE, siblings Bramble + Copse. */
  const masterlessView = (overrides: Record<string, unknown> = {}) => ({
    ownerId: 'a4f1c8239b7dd809abcdef0123456789',
    ownerDisplayName: 'zeblith',
    devices: [
      device('aa', 'KRILE', true),
      { ...device('bb', 'Bramble'), petName: 'Bramble' },
      { ...device('cc', 'Copse'), petName: 'Copse' },
    ],
    canBackUp: false,
    selfIsMaster: false,
    fleetEpoch: 0,
    fleetEpochStale: false,
    quorumRequests: [],
    quorumArmedUntilMs: null,
    ...overrides,
  });

  const cosignRequest = (overrides: Record<string, unknown> = {}) => ({
    requestId: '0102'.repeat(8),
    kind: 'revocation',
    targetDeviceId: 'cc'.repeat(16),
    initiatorDeviceId: 'bb'.repeat(16),
    reason: 'lost',
    expiresAtMs: 1_900_000_000_000,
    initiatedByMe: false,
    signedByMe: false,
    declinedByMe: false,
    declined: false,
    cosignerSigned: false,
    canCosign: true,
    ...overrides,
  });

  it('renders the co-sign banner with joined names; Approve calls cosign_quorum_request', async () => {
    mockedInvoke.mockResolvedValueOnce(
      masterlessView({ quorumRequests: [cosignRequest()] }),
    );
    mockedInvoke.mockResolvedValueOnce(undefined); // cosign_quorum_request
    mockedInvoke.mockResolvedValueOnce(masterlessView()); // post-approve refresh
    render(DevicesPanel);
    const banner = await screen.findByTestId('quorum-cosign-banner');
    expect(banner.textContent).toContain('Bramble');
    expect(banner.textContent).toContain('Copse');
    expect(banner.textContent).toContain('lost');
    await fireEvent.click(screen.getByTestId('quorum-approve'));
    await tick();
    expect(mockedInvoke).toHaveBeenCalledWith('cosign_quorum_request', {
      requestId: '0102'.repeat(8),
    });
  });

  it('Decline calls decline_quorum_request', async () => {
    mockedInvoke.mockResolvedValueOnce(
      masterlessView({ quorumRequests: [cosignRequest()] }),
    );
    mockedInvoke.mockResolvedValueOnce(undefined); // decline_quorum_request
    mockedInvoke.mockResolvedValueOnce(masterlessView());
    render(DevicesPanel);
    await screen.findByTestId('quorum-cosign-banner');
    await fireEvent.click(screen.getByTestId('quorum-decline'));
    await tick();
    expect(mockedInvoke).toHaveBeenCalledWith('decline_quorum_request', {
      requestId: '0102'.repeat(8),
    });
  });

  it('no banner when canCosign is false (signed / declined / not addressed)', async () => {
    mockedInvoke.mockResolvedValueOnce(
      masterlessView({
        quorumRequests: [cosignRequest({ canCosign: false, signedByMe: true })],
      }),
    );
    render(DevicesPanel);
    await screen.findByText('KRILE');
    expect(screen.queryByTestId('quorum-cosign-banner')).toBeNull();
  });

  it('initiator sees the pending note (waiting → declined variants)', async () => {
    mockedInvoke.mockResolvedValueOnce(
      masterlessView({
        quorumRequests: [
          cosignRequest({ initiatedByMe: true, canCosign: false }),
        ],
      }),
    );
    render(DevicesPanel);
    const note = await screen.findByTestId('quorum-pending-note');
    expect(note.textContent).toMatch(/waiting for another device to co-sign/i);
    expect(note.textContent).toContain('Copse');
  });

  it('initiator pending note renders the declined state', async () => {
    mockedInvoke.mockResolvedValueOnce(
      masterlessView({
        quorumRequests: [
          cosignRequest({ initiatedByMe: true, canCosign: false, declined: true }),
        ],
      }),
    );
    render(DevicesPanel);
    const note = await screen.findByTestId('quorum-pending-note');
    expect(note.textContent).toMatch(/was declined/i);
  });

  it('master-less sibling rows show Remove… when quorumRemovable', async () => {
    mockedInvoke.mockResolvedValueOnce(masterlessView());
    render(DevicesPanel);
    await screen.findByText('KRILE');
    expect(screen.getAllByRole('button', { name: /^remove…$/i }).length).toBe(2);
    // Replace stays master-only.
    expect(screen.queryByRole('button', { name: /^replace…$/i })).toBeNull();
  });

  it('master-less sibling rows hide Remove… when not quorumRemovable', async () => {
    const view = masterlessView();
    (view.devices as Record<string, unknown>[]).forEach((d) => {
      d.quorumRemovable = false;
    });
    mockedInvoke.mockResolvedValueOnce(view);
    render(DevicesPanel);
    await screen.findByText('KRILE');
    expect(screen.queryByRole('button', { name: /^remove…$/i })).toBeNull();
  });

  it('quorum remove confirm calls request_quorum_revocation, not revoke_device, and shows the co-sign copy', async () => {
    mockedInvoke.mockResolvedValueOnce(masterlessView());
    mockedInvoke.mockResolvedValueOnce('req-id-1'); // request_quorum_revocation
    mockedInvoke.mockResolvedValueOnce(
      masterlessView({
        quorumRequests: [cosignRequest({ initiatedByMe: true, canCosign: false })],
      }),
    );
    render(DevicesPanel);
    await screen.findByText('KRILE');
    // Bramble's row (first sibling Remove…).
    await fireEvent.click(screen.getAllByRole('button', { name: /^remove…$/i })[0]);
    expect(await screen.findByTestId('remove-quorum-copy')).toBeInTheDocument();
    const input = await screen.findByRole('textbox', { name: /type to confirm/i });
    await fireEvent.input(input, { target: { value: 'Bramble' } });
    await fireEvent.click(screen.getByRole('button', { name: /^request removal$/i }));
    await tick();
    expect(mockedInvoke).toHaveBeenCalledWith('request_quorum_revocation', {
      deviceVkHex: 'bb'.repeat(32),
      reason: 'decommissioned',
    });
    const calls = mockedInvoke.mock.calls.map((c: unknown[]) => c[0]);
    expect(calls).not.toContain('revoke_device');
  });

  it('tolerates a stale backend view without quorum fields', async () => {
    const view = masterlessView();
    delete (view as Record<string, unknown>).quorumRequests;
    delete (view as Record<string, unknown>).quorumArmedUntilMs;
    (view.devices as Record<string, unknown>[]).forEach((d) => {
      delete d.quorumRemovable;
    });
    mockedInvoke.mockResolvedValueOnce(view);
    render(DevicesPanel);
    await screen.findByText('KRILE');
    expect(screen.queryByTestId('quorum-cosign-banner')).toBeNull();
    expect(screen.queryByRole('button', { name: /^remove…$/i })).toBeNull();
  });
});

// ── ZEB-677 S4: quorum-enrollment arm surface + live countdown ────────────────

describe('DevicesPanel — quorum-enrollment arm surface (ZEB-677 S4)', () => {
  const device = (idByte: string, name: string, self = false) => ({
    deviceId: idByte.repeat(16),
    displayName: name,
    isThisDevice: self,
    trustDecision: { kind: 'full', reason: null },
    enrolledAt: 1_700_000_000,
    fingerprint: `${idByte}${idByte}·xx`,
    butlerPinned: false,
    deviceVkHex: idByte.repeat(32),
    revoked: false,
    revokedAt: null,
    revokedReason: null,
    petName: null,
    lastSeenMs: null,
    connectedNow: false,
    quorumRemovable: !self,
  });

  /** Master-less fleet with the arm affordance backend-enabled. */
  const armView = (overrides: Record<string, unknown> = {}) => ({
    ownerId: 'a4f1c8239b7dd809abcdef0123456789',
    ownerDisplayName: 'zeblith',
    devices: [device('aa', 'KRILE', true), { ...device('bb', 'Bramble'), petName: 'Bramble' }],
    canBackUp: false,
    selfIsMaster: false,
    fleetEpoch: 0,
    fleetEpochStale: false,
    quorumRequests: [],
    quorumArmedUntilMs: null,
    canArmEnrollment: true,
    ...overrides,
  });

  const HONESTY_COPY =
    /for the next 15 minutes this device will approve one new device enrollment started from your other devices/i;

  it('renders the arm button + honesty copy when canArmEnrollment and not armed', async () => {
    mockedInvoke.mockResolvedValueOnce(armView());
    render(DevicesPanel);
    expect(await screen.findByTestId('quorum-arm-button')).toBeInTheDocument();
    expect(screen.getByText(HONESTY_COPY)).toBeInTheDocument();
    // No countdown/cancel while un-armed.
    expect(screen.queryByTestId('quorum-arm-countdown')).toBeNull();
    expect(screen.queryByTestId('quorum-arm-cancel')).toBeNull();
  });

  it('treats an expired quorumArmedUntilMs as not armed (shows the arm button)', async () => {
    mockedInvoke.mockResolvedValueOnce(armView({ quorumArmedUntilMs: Date.now() - 60_000 }));
    render(DevicesPanel);
    expect(await screen.findByTestId('quorum-arm-button')).toBeInTheDocument();
    expect(screen.queryByTestId('quorum-arm-countdown')).toBeNull();
  });

  it('hides the arm surface entirely when canArmEnrollment is false', async () => {
    mockedInvoke.mockResolvedValueOnce(armView({ canArmEnrollment: false }));
    render(DevicesPanel);
    await screen.findByText('KRILE');
    expect(screen.queryByTestId('quorum-arm-button')).toBeNull();
    expect(screen.queryByText(HONESTY_COPY)).toBeNull();
  });

  it('hides the arm surface when canArmEnrollment is absent (stale backend)', async () => {
    const view = armView();
    delete (view as Record<string, unknown>).canArmEnrollment;
    mockedInvoke.mockResolvedValueOnce(view);
    render(DevicesPanel);
    await screen.findByText('KRILE');
    expect(screen.queryByTestId('quorum-arm-button')).toBeNull();
  });

  it('renders a live countdown + Cancel when armed; Cancel invokes disarm_quorum_enrollment', async () => {
    const armedUntil = Date.now() + 15 * 60 * 1000; // 15 min out
    mockedInvoke.mockResolvedValueOnce(armView({ quorumArmedUntilMs: armedUntil }));
    mockedInvoke.mockResolvedValueOnce(undefined); // disarm_quorum_enrollment
    mockedInvoke.mockResolvedValueOnce(armView()); // post-disarm refresh (un-armed)
    render(DevicesPanel);
    const countdown = await screen.findByTestId('quorum-arm-countdown');
    // mm:ss derived from the backend deadline, not fabricated client-side.
    expect(countdown.textContent).toMatch(/^\d{2}:\d{2}$/);
    // The arm button must not co-render while armed.
    expect(screen.queryByTestId('quorum-arm-button')).toBeNull();
    await fireEvent.click(screen.getByTestId('quorum-arm-cancel'));
    await tick();
    expect(mockedInvoke).toHaveBeenCalledWith('disarm_quorum_enrollment');
  });

  it('Approve adding a device invokes arm_quorum_enrollment', async () => {
    mockedInvoke.mockResolvedValueOnce(armView());
    mockedInvoke.mockResolvedValueOnce(Date.now() + 15 * 60 * 1000); // arm_quorum_enrollment
    mockedInvoke.mockResolvedValueOnce(
      armView({ quorumArmedUntilMs: Date.now() + 15 * 60 * 1000 }),
    ); // post-arm refresh
    render(DevicesPanel);
    await fireEvent.click(await screen.findByTestId('quorum-arm-button'));
    await tick();
    expect(mockedInvoke).toHaveBeenCalledWith('arm_quorum_enrollment');
  });
});
