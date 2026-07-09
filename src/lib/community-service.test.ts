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
