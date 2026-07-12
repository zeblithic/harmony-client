import { describe, expect, it, vi, beforeEach } from 'vitest';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));

import { invoke } from '@tauri-apps/api/core';
import { bumpFleetEpoch } from './fleet-epoch-service';

const mockedInvoke = invoke as unknown as ReturnType<typeof vi.fn>;

beforeEach(() => {
  vi.resetAllMocks();
});

describe('fleet-epoch-service', () => {
  it('invokes bump_fleet_epoch and resolves to the new epoch', async () => {
    mockedInvoke.mockResolvedValueOnce(2);
    await expect(bumpFleetEpoch()).resolves.toBe(2);
    expect(mockedInvoke).toHaveBeenCalledWith('bump_fleet_epoch');
  });

  it('propagates backend rejections (e.g. notMaster)', async () => {
    mockedInvoke.mockRejectedValueOnce('notMaster: this device does not hold the master key');
    await expect(bumpFleetEpoch()).rejects.toMatch(/notMaster/);
  });
});
