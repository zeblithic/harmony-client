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

  it('createCommunity invokes the IPC with the boolean is_invite_only argument', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('aabbccdd');
    const id = await service.createCommunity('Test', 'invite-only');
    expect(adapter.invoke).toHaveBeenCalledWith('create_community', { name: 'Test', isInviteOnly: true });
    expect(id).toBe('aabbccdd');
  });

  it('createCommunity translates kind=open to isInviteOnly=false', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('aabbccdd');
    await service.createCommunity('Open', 'open');
    expect(adapter.invoke).toHaveBeenCalledWith('create_community', { name: 'Open', isInviteOnly: false });
  });

  it('setPowerLevel sends a clamped integer level (not newPower)', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.setPowerLevel('aabb', 'cc11', 75);
    expect(adapter.invoke).toHaveBeenCalledWith('set_power_level', {
      communityId: 'aabb',
      targetAddr: 'cc11',
      level: 75,
    });
  });

  it('listCommunityMembers maps backend MemberInfoDto.addr → CommunityMember.address', async () => {
    await service.connectAdapter(adapter);
    const dtoRoster = [
      {
        addr: 'a3f8c1d2',
        displayName: 'Alice',
        status: 'joined',
        power: 100,
        joinedAt: { wallMs: 1700000000, logical: 0, deviceId: 'dev1' },
      },
    ];
    (adapter.invoke as any).mockResolvedValue(dtoRoster);
    const r = await service.listCommunityMembers('aabbccdd');
    expect(r).toEqual([
      { address: 'a3f8c1d2', displayName: 'Alice', status: 'joined', power: 100, joinedAt: 1700000000 },
    ]);
  });

  it('redeemInvite returns the DTO and learns the community kind', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue({
      communityId: 'eeff0011',
      communityName: 'Real Name',
      isInviteOnly: true,
    });
    const dto = await service.redeemInvite('harmony://invite/v1?ci=...');
    expect(adapter.invoke).toHaveBeenCalledWith('redeem_invite', { url: 'harmony://invite/v1?ci=...' });
    expect(dto).toEqual({
      communityId: 'eeff0011',
      communityName: 'Real Name',
      isInviteOnly: true,
    });
    // ZEB-265: redeem now records kind so getKind() doesn't return 'unknown'
    // for redeemed communities.
    expect(service.getKind('eeff0011')).toBe('invite-only');
  });

  it('redeemInvite records open kind when isInviteOnly is false', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue({
      communityId: '00112233',
      communityName: 'Open Community',
      isInviteOnly: false,
    });
    await service.redeemInvite('harmony://invite/v1?ci=...');
    expect(service.getKind('00112233')).toBe('open');
  });

  it('listCommunityMembers caches per-community result', async () => {
    await service.connectAdapter(adapter);
    const dtoRoster = [
      {
        addr: 'a3f8c1d2',
        displayName: 'Alice',
        status: 'joined',
        power: 100,
        joinedAt: { wallMs: 1700000000, logical: 0, deviceId: 'dev1' },
      },
    ];
    (adapter.invoke as any).mockResolvedValue(dtoRoster);

    const r1 = await service.listCommunityMembers('aabbccdd');
    const r2 = await service.listCommunityMembers('aabbccdd');

    expect(r1[0].address).toBe('a3f8c1d2');
    expect(r2).toEqual(r1);
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

  it('getKind records the chosen kind for created communities and returns unknown otherwise', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('aabbccdd');
    await service.createCommunity('Test', 'invite-only');
    expect(service.getKind('aabbccdd')).toBe('invite-only');
    expect(service.getKind('11223344')).toBe('unknown');
  });

  it('community-state-sync-degraded sets degraded flag', async () => {
    await service.connectAdapter(adapter);
    expect(service.isDegraded('aabbccdd')).toBe(false);

    const handler = adapter.listeners.get('community-state-sync-degraded')!;
    handler({ payload: { communityId: 'aabbccdd', degraded: true } });

    expect(service.isDegraded('aabbccdd')).toBe(true);
  });
});
