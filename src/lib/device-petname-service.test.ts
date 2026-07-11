import { describe, expect, it, vi, beforeEach } from 'vitest';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));

import { invoke } from '@tauri-apps/api/core';
import { setDevicePetname, MAX_DEVICE_PETNAME_CHARS } from './device-petname-service';

const mockedInvoke = invoke as unknown as ReturnType<typeof vi.fn>;

beforeEach(() => {
  vi.resetAllMocks();
});

describe('device-petname-service', () => {
  it('invokes set_device_petname with camelCase args', async () => {
    mockedInvoke.mockResolvedValueOnce(undefined);
    await setDevicePetname('ab'.repeat(32), 'KRILE');
    expect(mockedInvoke).toHaveBeenCalledWith('set_device_petname', {
      deviceVkHex: 'ab'.repeat(32),
      petname: 'KRILE',
    });
  });

  it('clears with an empty string', async () => {
    mockedInvoke.mockResolvedValueOnce(undefined);
    await setDevicePetname('ab'.repeat(32), '');
    expect(mockedInvoke).toHaveBeenCalledWith('set_device_petname', {
      deviceVkHex: 'ab'.repeat(32),
      petname: '',
    });
  });

  it('exports the backend length cap', () => {
    expect(MAX_DEVICE_PETNAME_CHARS).toBe(64);
  });
});
