import { describe, it, expect, vi, beforeEach } from 'vitest';
import { OwnerService, extractError } from './owner-service';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));

import { invoke } from '@tauri-apps/api/core';

describe('OwnerService', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('refresh() sets state to null on un-minted', async () => {
    (invoke as unknown as ReturnType<typeof vi.fn>).mockResolvedValueOnce(null);
    const svc = new OwnerService();
    let changeCount = 0;
    svc.onChange = () => { changeCount++; };
    await svc.refresh();
    expect(svc.state).toBeNull();
    expect(changeCount).toBe(1);
  });

  it('refresh() stores populated view', async () => {
    const view = {
      ownerId: 'a4f1c8239b7dd809',
      ownerDisplayName: 'zeblith',
      devices: [],
      canBackUp: true,
    };
    (invoke as unknown as ReturnType<typeof vi.fn>).mockResolvedValueOnce(view);
    const svc = new OwnerService();
    await svc.refresh();
    expect(svc.state).toEqual(view);
  });

  it('mint() returns recoveryToken and updates state', async () => {
    const result = {
      state: {
        ownerId: 'newowner', ownerDisplayName: 'this device',
        devices: [], canBackUp: true,
      },
      recoveryToken: '01234567-89ab-cdef-0123-456789abcdef',
    };
    (invoke as unknown as ReturnType<typeof vi.fn>).mockResolvedValueOnce(result);
    const svc = new OwnerService();
    const got = await svc.mint();
    expect(got.recoveryToken).toBe(result.recoveryToken);
    expect(svc.state).toEqual(result.state);
  });

  it('exportRecoveryFile passes args verbatim', async () => {
    (invoke as unknown as ReturnType<typeof vi.fn>).mockResolvedValueOnce({
      identityHash: 'abc', byteLen: 1234, path: '/tmp/r',
    });
    const svc = new OwnerService();
    const got = await svc.exportRecoveryFile('tok', 'path-tok', 'a-strong-passphrase', 'comment');
    expect(invoke).toHaveBeenCalledWith('export_owner_recovery_file_to_path', {
      recoveryToken: 'tok',
      pathToken: 'path-tok',
      passphrase: 'a-strong-passphrase',
      comment: 'comment',
    });
    expect(got.path).toBe('/tmp/r');
    expect(got.identityHash).toBe('abc');
    expect(got.byteLen).toBe(1234);
  });

  it('requestExportSavePath forwards the dialog request shape', async () => {
    (invoke as unknown as ReturnType<typeof vi.fn>).mockResolvedValueOnce('path-token-uuid');
    const svc = new OwnerService();
    const got = await svc.requestExportSavePath({
      title: 'Save backup',
      defaultFilename: 'owner-recovery.bin',
      filterName: 'Recovery file',
      filterExtensions: ['bin'],
    });
    expect(got).toBe('path-token-uuid');
    expect(invoke).toHaveBeenCalledWith('request_export_save_path', {
      request: {
        title: 'Save backup',
        defaultFilename: 'owner-recovery.bin',
        filterName: 'Recovery file',
        filterExtensions: ['bin'],
      },
    });
  });

  it('requestExportSavePath returns null when user cancels', async () => {
    (invoke as unknown as ReturnType<typeof vi.fn>).mockResolvedValueOnce(null);
    const svc = new OwnerService();
    const got = await svc.requestExportSavePath({
      defaultFilename: 'x',
      filterName: 'y',
      filterExtensions: ['z'],
    });
    expect(got).toBeNull();
  });
});

describe('extractError', () => {
  it('returns string from Error object (test-mode rejection)', () => {
    expect(extractError(new Error('boom'))).toBe('boom');
  });
  it('returns string from raw string rejection (production-mode)', () => {
    expect(extractError('just a string')).toBe('just a string');
  });
});

describe('OwnerService.revoke (ZEB-668 S2)', () => {
  it('revoke passes camelCase args verbatim', async () => {
    (invoke as unknown as ReturnType<typeof vi.fn>).mockResolvedValueOnce(undefined);
    const svc = new OwnerService();
    await svc.revoke('ab'.repeat(32), 'lost');
    expect(invoke).toHaveBeenCalledWith('revoke_device', {
      deviceVkHex: 'ab'.repeat(32),
      reason: 'lost',
    });
  });

  it('revoke surfaces backend prefix errors unchanged', async () => {
    (invoke as unknown as ReturnType<typeof vi.fn>).mockRejectedValueOnce(
      new Error('notMaster: this device does not hold the master key'),
    );
    const svc = new OwnerService();
    await expect(svc.revoke('cd'.repeat(32), 'compromised')).rejects.toThrow(/notMaster:/);
  });
});
