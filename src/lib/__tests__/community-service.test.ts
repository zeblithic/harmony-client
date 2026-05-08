import { describe, it, expect, vi, beforeEach } from 'vitest';
import { CommunityService } from '../community-service';
import type { TauriAdapter } from '../zenoh-service';

function makeAdapter(): TauriAdapter & { listeners: Map<string, Function> } {
  const listeners = new Map<string, Function>();
  return {
    listeners,
    invoke: vi.fn(),
    listen: vi.fn(async (event: string, handler: Function) => {
      listeners.set(event, handler);
      return () => listeners.delete(event);
    }),
  } as any;
}

describe('CommunityService', () => {
  let service: CommunityService;
  let adapter: ReturnType<typeof makeAdapter>;

  beforeEach(() => {
    service = new CommunityService();
    adapter = makeAdapter();
  });

  it('connectAdapter installs community-members-changed + community-state-sync-degraded listeners', async () => {
    await service.connectAdapter(adapter);
    expect(adapter.listeners.has('community-members-changed')).toBe(true);
    expect(adapter.listeners.has('community-state-sync-degraded')).toBe(true);
  });

  it('createCommunity calls invoke with snake_case args', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('aabbccdd');
    const id = await service.createCommunity('Test', 'invite-only');
    expect(adapter.invoke).toHaveBeenCalledWith('create_community', expect.objectContaining({ name: 'Test', kind: 'invite-only' }));
    expect(id).toBe('aabbccdd');
  });

  it('redeemInvite calls invoke with the URL string', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('eeff0011');
    const id = await service.redeemInvite('harmony://invite/v1?ci=...');
    expect(adapter.invoke).toHaveBeenCalledWith('redeem_invite', { url: 'harmony://invite/v1?ci=...' });
    expect(id).toBe('eeff0011');
  });

  it('listCommunityMembers caches per-community result', async () => {
    await service.connectAdapter(adapter);
    const fakeRoster = [{ address: 'a3f8c1d2', displayName: 'Alice', power: 100, status: 'joined' }];
    (adapter.invoke as any).mockResolvedValue(fakeRoster);

    const r1 = await service.listCommunityMembers('aabbccdd');
    const r2 = await service.listCommunityMembers('aabbccdd');

    expect(r1).toEqual(fakeRoster);
    expect(r2).toEqual(fakeRoster);
    // Cached: only one IPC call
    expect(adapter.invoke).toHaveBeenCalledTimes(1);
  });

  it('community-members-changed for a community invalidates its cache', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue([]);
    await service.listCommunityMembers('aabbccdd');
    expect(adapter.invoke).toHaveBeenCalledTimes(1);

    // Simulate event
    const handler = adapter.listeners.get('community-members-changed')!;
    handler({ payload: { communityId: 'aabbccdd' } });

    await service.listCommunityMembers('aabbccdd');
    // Re-fetched after event
    expect(adapter.invoke).toHaveBeenCalledTimes(2);
  });

  it('community-state-sync-degraded sets degraded flag', async () => {
    await service.connectAdapter(adapter);
    expect(service.isDegraded('aabbccdd')).toBe(false);

    const handler = adapter.listeners.get('community-state-sync-degraded')!;
    handler({ payload: { communityId: 'aabbccdd', degraded: true } });

    expect(service.isDegraded('aabbccdd')).toBe(true);
  });
});
