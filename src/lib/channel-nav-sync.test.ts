import { describe, it, expect, vi } from 'vitest';
import { ChannelNavSyncService, type ChannelNavSyncDeps } from './channel-nav-sync';
import type { ChannelInfo } from './community-service';

const HLC = { wallMs: 0, logical: 0, deviceId: 'd' };
const ch = (id: string, name: string, deleted = false): ChannelInfo => ({
  channelId: id,
  name,
  writePower: 0,
  kind: 'text',
  createdAt: HLC,
  ...(deleted ? { deletedAt: HLC } : {}),
});

function harness(
  channelsByCommunity: Record<string, ChannelInfo[] | Error>,
  communityIds: string[],
) {
  const setCalls: Array<[string, ChannelInfo[]]> = [];
  const deps: ChannelNavSyncDeps = {
    listChannels: vi.fn(async (id: string) => {
      const v = channelsByCommunity[id];
      if (v instanceof Error) throw v;
      return v ?? [];
    }),
    setChannels: (id, channels) => setCalls.push([id, channels]),
    listCommunityIds: () => communityIds,
  };
  return { svc: new ChannelNavSyncService(deps), setCalls, deps };
}

describe('ChannelNavSyncService (ZEB-663)', () => {
  it('start() eager-populates every joined community', async () => {
    const { svc, setCalls } = harness(
      { c1: [ch('a', 'general')], c2: [ch('b', 'lobby')] },
      ['c1', 'c2'],
    );
    await svc.start();
    expect(setCalls.map(([id]) => id).sort()).toEqual(['c1', 'c2']);
  });

  it('resync filters deletedAt before setChannels', async () => {
    const { svc, setCalls } = harness(
      { c1: [ch('a', 'general'), ch('b', 'gone', true)] },
      ['c1'],
    );
    await svc.resync('c1');
    expect(setCalls).toHaveLength(1);
    expect(setCalls[0][1].map((c) => c.channelId)).toEqual(['a']); // 'b' filtered
  });

  it('a listChannels rejection is swallowed and does not block other communities', async () => {
    const { svc, setCalls } = harness(
      { c1: new Error('boom'), c2: [ch('b', 'lobby')] },
      ['c1', 'c2'],
    );
    await svc.start(); // must not reject
    expect(setCalls.map(([id]) => id)).toEqual(['c2']); // c1 skipped, c2 populated
  });

  it('resync never rejects even if listChannels throws', async () => {
    const { svc, setCalls } = harness({ c1: new Error('boom') }, ['c1']);
    await expect(svc.resync('c1')).resolves.toBeUndefined();
    expect(setCalls).toHaveLength(0);
  });
});
