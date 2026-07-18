import { describe, it, expect } from 'vitest';
import { CommunityService, type ChannelInfo } from './community-service';
import type { TauriAdapter } from './zenoh-service';

const HLC = { wallMs: 0, logical: 0, deviceId: 'd' };
const CHANNELS: ChannelInfo[] = [
  { channelId: 'ch1', name: 'general', writePower: 0, kind: 'text', createdAt: HLC },
  { channelId: 'ch2', name: 'random', writePower: 0, kind: 'text', createdAt: HLC },
];

function mockAdapter(channels: ChannelInfo[]): TauriAdapter {
  return {
    invoke: async (cmd) => (cmd === 'list_channels' ? channels : undefined),
    listen: async () => () => {},
  };
}

describe('CommunityService.getCachedChannelName (ZEB-662)', () => {
  it('returns undefined before the community has been fetched', () => {
    const svc = new CommunityService();
    expect(svc.getCachedChannelName('c1', 'ch1')).toBeUndefined();
  });

  it('resolves a channel name from the session cache after listChannels', async () => {
    const svc = new CommunityService();
    await svc.connectAdapter(mockAdapter(CHANNELS));
    await svc.listChannels('c1');
    expect(svc.getCachedChannelName('c1', 'ch1')).toBe('general');
    expect(svc.getCachedChannelName('c1', 'ch2')).toBe('random');
  });

  it('returns undefined for an unknown channel or a different community', async () => {
    const svc = new CommunityService();
    await svc.connectAdapter(mockAdapter(CHANNELS));
    await svc.listChannels('c1');
    expect(svc.getCachedChannelName('c1', 'nope')).toBeUndefined();
    expect(svc.getCachedChannelName('other-community', 'ch1')).toBeUndefined();
  });
});

describe('CommunityService.listLeftCommunities (ZEB-435)', () => {
  it('invokes list_left_communities and returns the DTO rows verbatim', async () => {
    const rows = [
      { spaceId: 'aa'.repeat(16), name: 'Old Crew', leftAtMs: 1_700_000_000_000 },
      { spaceId: 'bb'.repeat(16), name: 'Test Community', leftAtMs: 1_600_000_000_000 },
    ];
    const calls: Array<{ cmd: string; args: unknown }> = [];
    const adapter: TauriAdapter = {
      invoke: async (cmd, args) => {
        calls.push({ cmd, args });
        return cmd === 'list_left_communities' ? rows : undefined;
      },
      listen: async () => () => {},
    };
    const svc = new CommunityService();
    await svc.connectAdapter(adapter);
    const got = await svc.listLeftCommunities();
    expect(got).toEqual(rows);
    expect(calls.some((c) => c.cmd === 'list_left_communities')).toBe(true);
  });
});
