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

  it('drops a stale resync when a newer resync for the same community superseded it', async () => {
    // Two overlapping resyncs for c1; the FIRST (stale) resolves LAST. Without
    // the last-write-wins guard the stale snapshot would clobber the fresh one.
    const resolvers: Array<(v: ChannelInfo[]) => void> = [];
    const setCalls: Array<[string, ChannelInfo[]]> = [];
    let call = 0;
    const deps: ChannelNavSyncDeps = {
      listChannels: () =>
        new Promise<ChannelInfo[]>((resolve) => {
          resolvers[call++] = resolve;
        }),
      setChannels: (id, channels) => setCalls.push([id, channels]),
      listCommunityIds: () => ['c1'],
    };
    const svc = new ChannelNavSyncService(deps);
    const p1 = svc.resync('c1'); // issue 1 — stale snapshot
    const p2 = svc.resync('c1'); // issue 2 — fresh snapshot
    resolvers[1]([ch('fresh', 'fresh')]); // newer completes first
    resolvers[0]([ch('stale', 'stale')]); // older completes last
    await Promise.all([p1, p2]);
    expect(setCalls).toHaveLength(1);
    expect(setCalls[0][1].map((c) => c.channelId)).toEqual(['fresh']);
  });

  it('applies resyncs independently per community (no cross-community suppression)', async () => {
    const { svc, setCalls } = harness(
      { c1: [ch('a', 'general')], c2: [ch('b', 'lobby')] },
      ['c1', 'c2'],
    );
    await Promise.all([svc.resync('c1'), svc.resync('c2')]);
    expect(setCalls.map(([id]) => id).sort()).toEqual(['c1', 'c2']);
  });
});
