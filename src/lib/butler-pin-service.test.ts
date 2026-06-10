import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));

import { invoke } from '@tauri-apps/api/core';
import { setButlerPin, extractButlerPinError } from './butler-pin-service';

const mockedInvoke = invoke as unknown as ReturnType<typeof vi.fn>;

beforeEach(() => {
  vi.resetAllMocks();
});

describe('butler-pin-service — ZEB-418 P2 D17', () => {
  // ── setButlerPin call shape ──────────────────────────────────────────────

  it('pins a device: invokes set_butler_pin with camelCase deviceId', async () => {
    mockedInvoke.mockResolvedValueOnce(undefined);
    const deviceId = 'aa'.repeat(32); // 64-hex SP1 device ID
    await setButlerPin(deviceId);
    expect(invoke).toHaveBeenCalledWith('set_butler_pin', { deviceId });
  });

  it('clears the pin: invokes set_butler_pin with deviceId: null', async () => {
    mockedInvoke.mockResolvedValueOnce(undefined);
    await setButlerPin(null);
    expect(invoke).toHaveBeenCalledWith('set_butler_pin', { deviceId: null });
  });

  it('resolves successfully on Ok backend response', async () => {
    mockedInvoke.mockResolvedValueOnce(undefined);
    await expect(setButlerPin('aa'.repeat(32))).resolves.toBeUndefined();
  });

  // ── error path ───────────────────────────────────────────────────────────

  it('propagates rejection from the backend', async () => {
    mockedInvoke.mockRejectedValueOnce('set_butler_pin: device is not enrolled');
    await expect(setButlerPin('zz'.repeat(32))).rejects.toBeDefined();
  });

  // ── extractButlerPinError ────────────────────────────────────────────────

  it('extracts message from an Error object (test environment shape)', () => {
    const err = new Error('Error: device not enrolled');
    expect(extractButlerPinError(err)).toBe('Error: device not enrolled');
  });

  it('stringifies a plain string rejection (production shape)', () => {
    expect(extractButlerPinError('device not enrolled')).toBe('device not enrolled');
  });

  it('stringifies an unknown rejection type', () => {
    expect(extractButlerPinError(42)).toBe('42');
  });
});
