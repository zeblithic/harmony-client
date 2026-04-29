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
      identityHash: 'abc', byteLen: 1234,
    });
    const svc = new OwnerService();
    await svc.exportRecoveryFile('tok', '/tmp/r', 'a-strong-passphrase', 'comment');
    expect(invoke).toHaveBeenCalledWith('export_owner_recovery_file_to_path', {
      recoveryToken: 'tok',
      path: '/tmp/r',
      passphrase: 'a-strong-passphrase',
      comment: 'comment',
    });
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
