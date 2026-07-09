import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
import { invoke } from '@tauri-apps/api/core';
import { fetchCommunitiesCount } from './owner-meta';

const mockedInvoke = invoke as unknown as ReturnType<typeof vi.fn>;

describe('fetchCommunitiesCount', () => {
  beforeEach(() => vi.resetAllMocks());

  it('returns the row count', async () => {
    mockedInvoke.mockResolvedValueOnce([{}, {}, {}]);
    expect(await fetchCommunitiesCount()).toBe(3);
    expect(mockedInvoke).toHaveBeenCalledWith('list_owner_communities', {});
  });

  it('returns null on IPC failure', async () => {
    mockedInvoke.mockRejectedValueOnce(new Error('nope'));
    expect(await fetchCommunitiesCount()).toBeNull();
  });

  it('returns null on non-array payloads', async () => {
    mockedInvoke.mockResolvedValueOnce(undefined);
    expect(await fetchCommunitiesCount()).toBeNull();
  });
});
